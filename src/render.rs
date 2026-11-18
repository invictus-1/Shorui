//! The PDFium worker. PDFium is single-threaded, so one thread owns the library and every open
//! document. The UI sends requests and gets back ready-to-draw textures; it never waits.

use std::{
    collections::{HashMap, VecDeque},
    os::raw::c_int,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{ColorImage, TextureHandle, TextureOptions};
use pdfium_render::prelude::*;

/// Tile edge in device pixels.
pub const TILE: u32 = 1024;
/// How many parsed pages stay loaded for fast re-rendering.
const PAGE_CACHE: usize = 24;
/// Undo history: how many steps, and how much memory whole-document snapshots may hold.
const UNDO_STEPS: usize = 40;
const UNDO_BYTES: usize = 512 << 20;

// Annotation subtypes, from the PDF specification as PDFium numbers them.
const ANNOT_TEXT: c_int = 1;
const ANNOT_HIGHLIGHT: c_int = 9;
const ANNOT_UNDERLINE: c_int = 10;
const ANNOT_SQUIGGLY: c_int = 11;
const ANNOT_STRIKEOUT: c_int = 12;
const ANNOT_INK: c_int = 15;
const ANNOT_FLAG_PRINT: c_int = 4;

pub type DocId = u64;

/// Generation counters. The UI bumps one to make every older queued request of that kind stale.
#[derive(Default)]
pub struct Gens {
    pub tile: AtomicU64,
    pub thumb: AtomicU64,
    pub search: AtomicU64,
    /// Whether pages without text are run through OCR.
    pub ocr: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Markup {
    Highlight,
    Underline,
    Strikeout,
}

/// A change to a document. Positions are (page, character index); points are in top-left page
/// points, the same space as [`PageText::boxes`].
pub enum Edit {
    Rotate { pages: Vec<u32>, clockwise: bool },
    Delete { pages: Vec<u32> },
    /// Moves pages so they sit, in order, just before the page currently at `before`.
    Move { pages: Vec<u32>, before: u32 },
    /// Inserts every page of another PDF before the page currently at `at`.
    Insert { path: PathBuf, at: u32 },
    Markup { kind: Markup, from: (u32, u32), to: (u32, u32), color: [u8; 3] },
    Note { page: u32, at: [f32; 2], text: String },
    Ink { page: u32, points: Vec<[f32; 2]>, color: [u8; 3], width: f32 },
    /// Removes the topmost annotation under a point.
    RemoveAnnot { page: u32, at: [f32; 2] },
}

pub enum Req {
    Open { id: DocId, path: PathBuf, password: Option<String> },
    Close { id: DocId },
    Tile { id: DocId, page: u32, scale: f32, tx: u32, ty: u32, stamp: u64 },
    Thumb { id: DocId, page: u32, width: u32, stamp: u64 },
    Save { id: DocId, path: PathBuf },
    /// Text, links and annotation areas of one page.
    Text { id: DocId, page: u32 },
    /// Case-insensitive search of the whole document. A newer search stamp cancels this one.
    Search { id: DocId, query: String, stamp: u64 },
    /// The text between two (page, character) positions, inclusive.
    Copy { id: DocId, from: (u32, u32), to: (u32, u32) },
    Edit { id: DocId, edit: Edit },
    Undo { id: DocId },
    Redo { id: DocId },
    /// Writes the given pages to a new PDF.
    Extract { id: DocId, pages: Vec<u32>, path: PathBuf },
    Print { id: DocId, job: crate::print::Job },
}

#[derive(Clone, PartialEq)]
pub enum Link {
    Page(u32),
    Uri(String),
}

/// One outline (bookmark) entry: nesting depth, title and target page.
#[derive(Clone)]
pub struct OutlineItem {
    pub depth: u8,
    pub title: String,
    pub page: Option<u32>,
}

/// The characters of a page in reading order, each with its box in points from the page's
/// top-left corner (after page rotation). Characters without a shape have an empty box.
pub struct PageText {
    pub chars: Vec<char>,
    pub boxes: Vec<[f32; 4]>,
    /// Areas covered by annotations Shorui can remove, in the same space as `boxes`.
    pub marks: Vec<[f32; 4]>,
    /// Sticky notes: where they are and what they say.
    pub notes: Vec<([f32; 4], String)>,
    pub links: Vec<([f32; 4], Link)>,
    /// True when the characters came from OCR of a scanned page.
    pub ocr: bool,
    /// True once OCR has been considered for this page, so it isn't tried again.
    ocr_checked: bool,
    /// PDF page space to top-left points: origin, then the images of the x and y unit vectors.
    m: [f32; 6],
}

impl PageText {
    fn to_view(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.m;
        (m[0] + m[2] * x + m[4] * y, m[1] + m[3] * x + m[5] * y)
    }

    fn to_page(&self, vx: f32, vy: f32) -> (f32, f32) {
        let m = &self.m;
        let det = m[2] * m[5] - m[4] * m[3];
        let (dx, dy) = (vx - m[0], vy - m[1]);
        ((dx * m[5] - m[4] * dy) / det, (m[2] * dy - m[3] * dx) / det)
    }

    /// A PDF-space rectangle [left, bottom, right, top] as [x0, y0, x1, y1] in top-left points.
    fn rect_to_view(&self, r: [f32; 4]) -> [f32; 4] {
        let (x0, y0) = self.to_view(r[0], r[1]);
        let (x1, y1) = self.to_view(r[2], r[3]);
        [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]
    }

    /// A top-left-points rectangle as [left, bottom, right, top] in PDF page space.
    fn rect_to_page(&self, r: &[f32; 4]) -> [f32; 4] {
        let (x0, y0) = self.to_page(r[0], r[1]);
        let (x1, y1) = self.to_page(r[2], r[3]);
        [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]
    }
}

/// Merges character boxes into one rectangle per line. Lines may run across the page or, on
/// rotated pages, down it.
pub fn line_rects(boxes: &[[f32; 4]]) -> Vec<[f32; 4]> {
    let mut out: Vec<[f32; 4]> = Vec::new();
    // Whether the rectangle being built runs down the page; unknown while it is one character.
    let mut vertical: Option<bool> = None;
    for b in boxes {
        let (bw, bh) = (b[2] - b[0], b[3] - b[1]);
        if bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        if let Some(cur) = out.last_mut() {
            let (cw, ch) = (cur[2] - cur[0], cur[3] - cur[1]);
            let y_overlap = cur[3].min(b[3]) - cur[1].max(b[1]);
            let x_overlap = cur[2].min(b[2]) - cur[0].max(b[0]);
            let (min_h, min_w) = (ch.min(bh), cw.min(bw));
            let across = vertical != Some(true) && y_overlap > 0.5 * min_h && b[0] >= cur[0] - min_h;
            let down = !across && vertical != Some(false) && x_overlap > 0.7 * min_w && -y_overlap < 0.6 * min_w;
            if across || down {
                vertical = Some(down);
                cur[0] = cur[0].min(b[0]);
                cur[1] = cur[1].min(b[1]);
                cur[2] = cur[2].max(b[2]);
                cur[3] = cur[3].max(b[3]);
                continue;
            }
        }
        vertical = None;
        out.push(*b);
    }
    out
}

pub enum Resp {
    Fatal(String),
    Opened { id: DocId, sizes: Vec<[f32; 2]>, outline: Vec<OutlineItem> },
    OpenFailed { id: DocId, error: String, needs_password: bool },
    Tile { id: DocId, epoch: u64, page: u32, scale: f32, tx: u32, ty: u32, tex: TextureHandle, bytes: usize },
    Thumb { id: DocId, epoch: u64, page: u32, tex: TextureHandle },
    /// The document changed. `pages` set: only those pages need redrawing (annotations).
    /// Otherwise the page list itself changed and everything is laid out again.
    Edited {
        id: DocId,
        epoch: u64,
        sizes: Vec<[f32; 2]>,
        outline: Vec<OutlineItem>,
        pages: Option<Vec<u32>>,
        note: &'static str,
        can_undo: bool,
        can_redo: bool,
    },
    EditFailed { error: String },
    Saved { id: DocId, path: PathBuf },
    SaveFailed { id: DocId, error: String },
    Text { id: DocId, epoch: u64, page: u32, text: Arc<PageText> },
    Hits { id: DocId, stamp: u64, page: u32, hits: Vec<Vec<[f32; 4]>> },
    SearchDone { id: DocId, stamp: u64 },
    Copied(String),
    /// A one-line result to show the user: extract and print outcomes.
    Notice { text: String, error: bool },
}

pub fn spawn(ctx: egui::Context, gens: Arc<Gens>) -> (Sender<Req>, Receiver<Resp>) {
    let (req_tx, req_rx) = unbounded();
    let (resp_tx, resp_rx) = unbounded();
    std::thread::Builder::new()
        .name("pdfium".into())
        .spawn(move || run(ctx, gens, req_rx, resp_tx))
        .expect("spawn pdfium thread");
    (req_tx, resp_rx)
}

/// An annotation to create, in PDF page space.
#[derive(Clone)]
enum AnnotSpec {
    Markup { page: u32, kind: Markup, rects: Vec<[f32; 4]>, color: [u8; 3] },
    Note { page: u32, at: [f32; 2], text: String },
    Ink { page: u32, points: Vec<[f32; 2]>, color: [u8; 3], width: f32 },
}

impl AnnotSpec {
    fn page(&self) -> u32 {
        match self {
            AnnotSpec::Markup { page, .. } | AnnotSpec::Note { page, .. } | AnnotSpec::Ink { page, .. } => *page,
        }
    }
}

/// One reversible step. Running a step returns the step that undoes it.
enum Step {
    Rotate { pages: Vec<u32>, clockwise: bool },
    /// Single-page moves applied in order: the page at index `.0` ends up at index `.1`.
    Moves(Vec<(u32, u32)>),
    AddAnnots(Vec<AnnotSpec>),
    /// Removes the newest annotation on each spec's page, last spec first.
    PopAnnots(Vec<AnnotSpec>),
    /// Replaces the whole document with these bytes. Used for edits with no cheap inverse.
    Restore(Vec<u8>),
}

impl Step {
    fn bytes(&self) -> usize {
        if let Step::Restore(b) = self { b.len() } else { 0 }
    }
}

/// What a step touched: every page, or just the annotations of some.
enum Changed {
    All,
    Pages(Vec<u32>),
}

struct Doc {
    doc: PdfDocument<'static>,
    epoch: u64,
    password: Option<String>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    /// Recognised text of scanned pages, kept so each page is read once.
    ocr: HashMap<u32, Arc<Recognised>>,
}

/// Characters and their boxes in top-left page points.
type Recognised = (Vec<char>, Vec<[f32; 4]>);

/// Runs OCR on a page with no text of its own.
fn recognise(page: &PdfPage<'static>, engine: &crate::ocr::Engine) -> Option<Recognised> {
    if page.objects().is_empty() {
        return Some((Vec::new(), Vec::new()));
    }
    // About 180 dpi: enough for body text, within the engine's size limit.
    let (wpt, hpt) = (page.width().value.max(1.0), page.height().value.max(1.0));
    let cap = engine.max_dim.min(3200) as f32;
    let scale = 2.5f32.min(cap / wpt.max(hpt));
    let (pw, ph) = ((wpt * scale).round().max(1.0) as i32, (hpt * scale).round().max(1.0) as i32);
    let mut bitmap = PdfBitmap::empty(pw, ph, PdfBitmapFormat::BGRA).ok()?;
    let config = PdfRenderConfig::new()
        .set_target_size(pw, ph)
        .set_format(PdfBitmapFormat::BGRA)
        .set_clear_color(PdfColor::new(255, 255, 255, 255))
        .render_annotations(false);
    page.render_into_bitmap_with_config(&mut bitmap, &config).ok()?;
    let raw = bitmap.as_raw_bytes();
    let stride = raw.len() / ph as usize;
    let bgra: Vec<u8> = if stride == pw as usize * 4 {
        raw
    } else {
        raw.chunks_exact(stride).flat_map(|row| row[..pw as usize * 4].iter().copied()).collect()
    };

    let lines = engine.recognise(&bgra, pw, ph)?;
    let (mut chars, mut boxes) = (Vec::new(), Vec::new());
    for line in lines {
        for (i, word) in line.iter().enumerate() {
            if i > 0 {
                chars.push(' ');
                boxes.push([0.0; 4]);
            }
            // The engine gives one box per word: share it out evenly between the letters.
            let n = word.text.chars().count().max(1) as f32;
            let [x0, y0, x1, y1] = word.rect;
            for (k, c) in word.text.chars().enumerate() {
                let (a, b) = (x0 + (x1 - x0) * k as f32 / n, x0 + (x1 - x0) * (k + 1) as f32 / n);
                chars.push(c);
                boxes.push([a / scale, y0 / scale, b / scale, y1 / scale]);
            }
        }
        chars.extend(['\r', '\n']);
        boxes.extend([[0.0; 4], [0.0; 4]]);
    }
    Some((chars, boxes))
}

struct Worker {
    pdfium: &'static Pdfium,
    docs: HashMap<DocId, Doc>,
    pages: VecDeque<((DocId, u32), PdfPage<'static>)>,
    texts: VecDeque<((DocId, u32), Arc<PageText>)>,
    ctx: egui::Context,
    out: Sender<Resp>,
    gens: Arc<Gens>,
    /// The Windows OCR engine, created the first time a scanned page needs it.
    ocr: Option<crate::ocr::Engine>,
    ocr_tried: bool,
}

struct TileJob {
    id: DocId,
    page: u32,
    scale: f32,
    tx: u32,
    ty: u32,
    stamp: u64,
}

struct ThumbJob {
    id: DocId,
    page: u32,
    width: u32,
    stamp: u64,
}

struct SearchJob {
    id: DocId,
    query: Vec<char>,
    stamp: u64,
    next: u32,
}

/// Lower-cases, drops carriage returns and turns every whitespace run into one space, keeping
/// each output character's index in the input.
fn normalise(chars: impl Iterator<Item = char>) -> Vec<(char, usize)> {
    let mut out: Vec<(char, usize)> = Vec::new();
    for (i, c) in chars.enumerate() {
        if c.is_whitespace() {
            if out.last().is_some_and(|l| l.0 != ' ') {
                out.push((' ', i));
            }
        } else {
            out.push((c.to_lowercase().next().unwrap_or(c), i));
        }
    }
    out
}

/// Every occurrence of `query` (already normalised) on a page, as line rectangles.
fn find(text: &PageText, query: &[char]) -> Vec<Vec<[f32; 4]>> {
    let norm = normalise(text.chars.iter().copied());
    let mut hits = Vec::new();
    let mut i = 0;
    while i + query.len() <= norm.len() {
        if norm[i..i + query.len()].iter().map(|c| c.0).eq(query.iter().copied()) {
            let (a, b) = (norm[i].1, norm[i + query.len() - 1].1);
            let rects = line_rects(&text.boxes[a..=b]);
            if !rects.is_empty() {
                hits.push(rects);
            }
            i += query.len();
        } else {
            i += 1;
        }
    }
    hits
}

/// An existing annotation Shorui knows how to remove: where it is, and its note text if any.
struct AnnotInfo {
    index: c_int,
    /// Areas in PDF page space, [left, bottom, right, top].
    rects: Vec<[f32; 4]>,
    note: Option<String>,
}

/// Lists the markup, note and ink annotations on a page.
fn annots(page: &PdfPage<'static>) -> Vec<AnnotInfo> {
    let b = page.bindings();
    let handle = b.get_handle_from_page(page);
    let mut out = Vec::new();
    unsafe {
        for index in 0..b.FPDFPage_GetAnnotCount(handle) {
            let a = b.FPDFPage_GetAnnot(handle, index);
            if a.is_null() {
                continue;
            }
            let subtype = b.FPDFAnnot_GetSubtype(a);
            let markup = matches!(subtype, ANNOT_HIGHLIGHT | ANNOT_UNDERLINE | ANNOT_SQUIGGLY | ANNOT_STRIKEOUT);
            if markup || subtype == ANNOT_TEXT || subtype == ANNOT_INK {
                let mut rects = Vec::new();
                if markup {
                    for q in 0..b.FPDFAnnot_CountAttachmentPoints(a) {
                        let mut quad = FS_QUADPOINTSF { x1: 0.0, y1: 0.0, x2: 0.0, y2: 0.0, x3: 0.0, y3: 0.0, x4: 0.0, y4: 0.0 };
                        if b.is_true(b.FPDFAnnot_GetAttachmentPoints(a, q, &mut quad)) {
                            let xs = [quad.x1, quad.x2, quad.x3, quad.x4];
                            let ys = [quad.y1, quad.y2, quad.y3, quad.y4];
                            let min = |v: [f32; 4]| v.into_iter().fold(f32::MAX, f32::min);
                            let max = |v: [f32; 4]| v.into_iter().fold(f32::MIN, f32::max);
                            rects.push([min(xs), min(ys), max(xs), max(ys)]);
                        }
                    }
                }
                if rects.is_empty() {
                    let mut r = FS_RECTF { left: 0.0, top: 0.0, right: 0.0, bottom: 0.0 };
                    if b.is_true(b.FPDFAnnot_GetRect(a, &mut r)) {
                        rects.push([r.left.min(r.right), r.top.min(r.bottom), r.left.max(r.right), r.top.max(r.bottom)]);
                    }
                }
                let note = (subtype == ANNOT_TEXT).then(|| {
                    let len = b.FPDFAnnot_GetStringValue(a, "Contents", std::ptr::null_mut(), 0) as usize;
                    let mut buf = vec![0u16; len / 2 + 1];
                    b.FPDFAnnot_GetStringValue(a, "Contents", buf.as_mut_ptr(), (buf.len() * 2) as _);
                    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                    String::from_utf16_lossy(&buf[..end])
                });
                out.push(AnnotInfo { index, rects, note });
            }
            b.FPDFPage_CloseAnnot(a);
        }
    }
    out
}

/// Creates one annotation. PDFium draws its appearance from the colour, quads and ink list.
fn create_annot(page: &PdfPage<'static>, spec: &AnnotSpec) -> Result<(), PdfiumError> {
    let b = page.bindings();
    let handle = b.get_handle_from_page(page);
    let fail = || PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::Unknown);
    let color_type = 0; // FPDFANNOT_COLORTYPE_Color: the annotation's own colour
    unsafe {
        let (subtype, bounds, color) = match spec {
            AnnotSpec::Markup { kind, rects, color, .. } => {
                let mut u = *rects.first().ok_or_else(fail)?;
                for r in rects {
                    u = [u[0].min(r[0]), u[1].min(r[1]), u[2].max(r[2]), u[3].max(r[3])];
                }
                let subtype = match kind {
                    Markup::Highlight => ANNOT_HIGHLIGHT,
                    Markup::Underline => ANNOT_UNDERLINE,
                    Markup::Strikeout => ANNOT_STRIKEOUT,
                };
                (subtype, u, *color)
            }
            AnnotSpec::Note { at, .. } => (ANNOT_TEXT, [at[0], at[1] - 20.0, at[0] + 20.0, at[1]], [255, 216, 77]),
            AnnotSpec::Ink { points, color, width, .. } => {
                let first = *points.first().ok_or_else(fail)?;
                let mut u = [first[0], first[1], first[0], first[1]];
                for p in points {
                    u = [u[0].min(p[0]), u[1].min(p[1]), u[2].max(p[0]), u[3].max(p[1])];
                }
                (ANNOT_INK, [u[0] - width, u[1] - width, u[2] + width, u[3] + width], *color)
            }
        };
        let a = b.FPDFPage_CreateAnnot(handle, subtype);
        if a.is_null() {
            return Err(fail());
        }
        let rect = FS_RECTF { left: bounds[0], bottom: bounds[1], right: bounds[2], top: bounds[3] };
        b.FPDFAnnot_SetRect(a, &rect);
        b.FPDFAnnot_SetColor(a, color_type, color[0] as _, color[1] as _, color[2] as _, 255);
        b.FPDFAnnot_SetFlags(a, ANNOT_FLAG_PRINT);
        match spec {
            AnnotSpec::Markup { rects, .. } => {
                for r in rects {
                    // PDF order: top-left, top-right, bottom-left, bottom-right.
                    let quad =
                        FS_QUADPOINTSF { x1: r[0], y1: r[3], x2: r[2], y2: r[3], x3: r[0], y3: r[1], x4: r[2], y4: r[1] };
                    b.FPDFAnnot_AppendAttachmentPoints(a, &quad);
                }
            }
            AnnotSpec::Note { text, .. } => {
                b.FPDFAnnot_SetStringValue_str(a, "Contents", text);
            }
            AnnotSpec::Ink { points, width, .. } => {
                let pts: Vec<FS_POINTF> = points.iter().map(|p| FS_POINTF { x: p[0], y: p[1] }).collect();
                b.FPDFAnnot_AddInkStroke(a, pts.as_ptr(), pts.len() as _);
                b.FPDFAnnot_SetBorder(a, 0.0, 0.0, *width);
            }
        }
        b.FPDFPage_CloseAnnot(a);
    }
    Ok(())
}

/// Reads a page's characters, links and annotations, mapped into top-left page points.
fn extract(page: &PdfPage<'static>) -> Option<PageText> {
    // Recover the page-to-view transform (rotation, crop offset) from three probe points.
    const S: f32 = 8.0;
    let config = PdfRenderConfig::new()
        .set_target_size((page.width().value * S).round() as i32, (page.height().value * S).round() as i32);
    let probe = |x: f32, y: f32| {
        page.points_to_pixels(PdfPoints::new(x), PdfPoints::new(y), &config).ok().map(|(a, b)| (a as f32 / S, b as f32 / S))
    };
    let (o, px, py) = (probe(0.0, 0.0)?, probe(1000.0, 0.0)?, probe(0.0, 1000.0)?);
    let (ax, ay) = ((px.0 - o.0) / 1000.0, (px.1 - o.1) / 1000.0);
    let (bx, by) = ((py.0 - o.0) / 1000.0, (py.1 - o.1) / 1000.0);
    if ax * by - bx * ay == 0.0 {
        return None;
    }
    let mut out = PageText {
        chars: Vec::new(),
        boxes: Vec::new(),
        marks: Vec::new(),
        notes: Vec::new(),
        links: Vec::new(),
        ocr: false,
        ocr_checked: false,
        m: [o.0, o.1, ax, ay, bx, by],
    };
    let rect = |r: &PdfRect| [r.left().value, r.bottom().value, r.right().value, r.top().value];

    for a in annots(page) {
        for r in &a.rects {
            let v = out.rect_to_view(*r);
            if let Some(text) = &a.note {
                out.notes.push((v, text.clone()));
            }
            out.marks.push(v);
        }
    }
    for link in page.links().iter() {
        let Ok(r) = link.rect() else { continue };
        let action = link.action();
        let uri = action.as_ref().and_then(|a| a.as_uri_action()).and_then(|u| u.uri().ok());
        let target = match uri {
            Some(u) => Some(Link::Uri(u)),
            None => action
                .as_ref()
                .and_then(|a| a.as_local_destination_action())
                .and_then(|d| d.destination().ok())
                .and_then(|d| d.page_index().ok())
                .or_else(|| link.destination().and_then(|d| d.page_index().ok()))
                .map(|p| Link::Page(p as u32)),
        };
        if let Some(target) = target {
            out.links.push((out.rect_to_view(rect(&r)), target));
        }
    }
    if let Ok(text) = page.text() {
        for c in text.chars().iter() {
            out.chars.push(c.unicode_char().unwrap_or('\u{fffd}'));
            out.boxes.push(match c.loose_bounds() {
                Ok(r) => out.rect_to_view(rect(&r)),
                Err(_) => [0.0; 4],
            });
        }
    }
    Some(out)
}

fn sizes_of(doc: &PdfDocument<'static>) -> Vec<[f32; 2]> {
    let pages = doc.pages();
    (0..pages.len())
        .map(|i| match pages.page_size(i) {
            Ok(r) => [r.width().value.max(1.0), r.height().value.max(1.0)],
            Err(_) => [612.0, 792.0],
        })
        .collect()
}

/// Flattens the document's bookmark tree, depth first.
fn outline_of(doc: &PdfDocument<'static>) -> Vec<OutlineItem> {
    fn walk(first: Option<PdfBookmark>, depth: u8, out: &mut Vec<OutlineItem>) {
        let mut node = first;
        while let Some(b) = node {
            if out.len() >= 5000 {
                return;
            }
            let page = b
                .destination()
                .and_then(|d| d.page_index().ok())
                .or_else(|| {
                    b.action()
                        .as_ref()
                        .and_then(|a| a.as_local_destination_action())
                        .and_then(|d| d.destination().ok())
                        .and_then(|d| d.page_index().ok())
                })
                .map(|p| p as u32);
            out.push(OutlineItem { depth, title: b.title().unwrap_or_default(), page });
            if depth < 8 {
                walk(b.first_child(), depth + 1, out);
            }
            node = b.next_sibling();
        }
    }
    let mut out = Vec::new();
    walk(doc.bookmarks().root(), 0, &mut out);
    out
}

fn run(ctx: egui::Context, gens: Arc<Gens>, rx: Receiver<Req>, out: Sender<Resp>) {
    let lib = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("pdfium.dll")))
        .unwrap_or_else(|| PathBuf::from("pdfium.dll"));
    let bindings = match Pdfium::bind_to_library(&lib) {
        Ok(b) => b,
        Err(e) => {
            let _ = out.send(Resp::Fatal(format!("Can't load pdfium.dll. {e}")));
            ctx.request_repaint();
            return;
        }
    };
    // Leaked on purpose: documents borrow the library for the life of the process.
    let pdfium: &'static Pdfium = Box::leak(Box::new(Pdfium::new(bindings)));
    let mut w = Worker {
        pdfium,
        docs: HashMap::new(),
        pages: VecDeque::new(),
        texts: VecDeque::new(),
        ctx,
        out,
        gens: gens.clone(),
        ocr: None,
        ocr_tried: false,
    };

    let mut tiles: VecDeque<TileJob> = VecDeque::new();
    let mut thumbs: VecDeque<ThumbJob> = VecDeque::new();
    let mut texts: VecDeque<(DocId, u32)> = VecDeque::new();
    let mut search: Option<SearchJob> = None;

    loop {
        let first = if tiles.is_empty() && thumbs.is_empty() && texts.is_empty() && search.is_none() {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        } else {
            None
        };
        for m in first.into_iter().chain(rx.try_iter()) {
            match m {
                Req::Tile { id, page, scale, tx, ty, stamp } => {
                    tiles.push_back(TileJob { id, page, scale, tx, ty, stamp })
                }
                Req::Thumb { id, page, width, stamp } => thumbs.push_back(ThumbJob { id, page, width, stamp }),
                Req::Text { id, page } => texts.push_back((id, page)),
                Req::Search { id, query, stamp } => {
                    let query = normalise(query.chars()).into_iter().map(|c| c.0).collect();
                    search = Some(SearchJob { id, query, stamp, next: 0 });
                }
                other => w.control(other),
            }
        }

        if let Some(j) = tiles.pop_front() {
            if j.stamp < gens.tile.load(Ordering::Relaxed) {
                continue;
            }
            w.tile(j);
        } else if let Some(j) = thumbs.pop_front() {
            if j.stamp < gens.thumb.load(Ordering::Relaxed) {
                continue;
            }
            w.thumb(j);
        } else if let Some((id, page)) = texts.pop_front() {
            if let (Some(epoch), Some(text)) = (w.docs.get(&id).map(|d| d.epoch), w.text(id, page)) {
                w.send(Resp::Text { id, epoch, page, text });
            }
        } else if let Some(job) = &mut search {
            // One page per turn, so tiles and selection always go first.
            let pages = w.docs.get(&job.id).map(|d| d.doc.pages().len() as u32).unwrap_or(0);
            if job.stamp < gens.search.load(Ordering::Relaxed) || job.query.is_empty() {
                search = None;
            } else if job.next >= pages {
                w.send(Resp::SearchDone { id: job.id, stamp: job.stamp });
                search = None;
            } else {
                let page = job.next;
                job.next += 1;
                if let Some(text) = w.text(job.id, page) {
                    let hits = find(&text, &job.query);
                    if !hits.is_empty() {
                        w.send(Resp::Hits { id: job.id, stamp: job.stamp, page, hits });
                    }
                }
            }
        }
    }
}

type Outcome = Result<(Step, Changed), PdfiumError>;

impl Worker {
    fn send(&self, r: Resp) {
        let _ = self.out.send(r);
        self.ctx.request_repaint();
    }

    /// A page's text, from a small cache shared by search, selection and copy. A page with no
    /// text of its own is read with OCR, once.
    fn text(&mut self, id: DocId, page: u32) -> Option<Arc<PageText>> {
        let want_ocr = self.gens.ocr.load(Ordering::Relaxed);
        if let Some(pos) = self.texts.iter().position(|(k, _)| *k == (id, page)) {
            let t = &self.texts[pos].1;
            if t.ocr_checked || !want_ocr {
                return Some(t.clone());
            }
            // Cached before OCR was switched on: read the page again.
            self.texts.remove(pos);
        }
        let saved = self.docs.get(&id).and_then(|d| d.ocr.get(&page).cloned());
        if want_ocr && saved.is_none() && !self.ocr_tried {
            self.ocr_tried = true;
            self.ocr = crate::ocr::Engine::new();
        }
        let engine = if want_ocr { self.ocr.clone() } else { None };

        let p = self.page(id, page)?;
        let mut t = extract(p)?;
        let mut fresh = None;
        if want_ocr {
            t.ocr_checked = true;
            if !t.chars.iter().any(|c| !c.is_whitespace()) {
                let found = match saved {
                    Some(r) => Some(r),
                    None => {
                        fresh = engine.and_then(|e| recognise(p, &e)).map(Arc::new);
                        fresh.clone()
                    }
                };
                if let Some(r) = found {
                    t.ocr = !r.0.is_empty();
                    t.chars = r.0.clone();
                    t.boxes = r.1.clone();
                }
            }
        }
        if let (Some(r), Some(d)) = (fresh, self.docs.get_mut(&id)) {
            d.ocr.insert(page, r);
        }
        let text = Arc::new(t);
        self.texts.push_front(((id, page), text.clone()));
        self.texts.truncate(64);
        Some(text)
    }

    fn drop_pages(&mut self, id: DocId) {
        self.pages.retain(|(k, _)| k.0 != id);
        self.texts.retain(|(k, _)| k.0 != id);
    }

    /// Returns the parsed page, loading it if needed and keeping it at the front of the cache.
    fn page(&mut self, id: DocId, page: u32) -> Option<&PdfPage<'static>> {
        if let Some(pos) = self.pages.iter().position(|(k, _)| *k == (id, page)) {
            if pos != 0 {
                let e = self.pages.remove(pos)?;
                self.pages.push_front(e);
            }
        } else {
            let p = self.docs.get(&id)?.doc.pages().get(page as PdfPageIndex).ok()?;
            self.pages.push_front(((id, page), p));
            self.pages.truncate(PAGE_CACHE);
        }
        self.pages.front().map(|(_, p)| p)
    }

    fn control(&mut self, req: Req) {
        match req {
            Req::Open { id, path, password } => {
                match self.pdfium.load_pdf_from_file(&path, password.as_deref()) {
                    Ok(doc) => {
                        let (sizes, outline) = (sizes_of(&doc), outline_of(&doc));
                        self.docs.insert(id, Doc { doc, epoch: 0, password, undo: Vec::new(), redo: Vec::new(), ocr: HashMap::new() });
                        self.send(Resp::Opened { id, sizes, outline });
                    }
                    Err(e) => {
                        let needs_password = matches!(
                            e,
                            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
                        );
                        let error = match &e {
                            PdfiumError::IoError(io) => io.to_string(),
                            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::FormatError) => {
                                "It isn't a PDF, or it is damaged.".to_string()
                            }
                            other => format!("{other:?}"),
                        };
                        self.send(Resp::OpenFailed { id, error, needs_password });
                    }
                }
            }
            Req::Close { id } => {
                self.drop_pages(id);
                self.docs.remove(&id);
            }
            Req::Save { id, path } => {
                let Some(d) = self.docs.get(&id) else { return };
                // Serialise fully first, then swap the file in one step, so a failed save never
                // leaves a half-written PDF and the open document keeps reading its old handle.
                let result = d.doc.save_to_bytes().map_err(|e| format!("{e:?}")).and_then(|bytes| {
                    let mut tmp = path.clone().into_os_string();
                    tmp.push(".shorui-tmp");
                    let tmp = PathBuf::from(tmp);
                    std::fs::write(&tmp, bytes)
                        .and_then(|_| std::fs::rename(&tmp, &path))
                        .map_err(|e| {
                            let _ = std::fs::remove_file(&tmp);
                            e.to_string()
                        })
                });
                match result {
                    Ok(()) => self.send(Resp::Saved { id, path }),
                    Err(error) => self.send(Resp::SaveFailed { id, error }),
                }
            }
            Req::Copy { id, from, to } => {
                let mut out = String::new();
                for page in from.0..=to.0 {
                    let Some(text) = self.text(id, page) else { continue };
                    let start = if page == from.0 { from.1 as usize } else { 0 };
                    let end = if page == to.0 { (to.1 as usize + 1).min(text.chars.len()) } else { text.chars.len() };
                    if start < end {
                        out.extend(text.chars[start..end].iter().filter(|c| **c != '\u{fffe}' && **c != '\u{ffff}'));
                    }
                    if page != to.0 && !out.ends_with('\n') {
                        out.push_str("\r\n");
                    }
                }
                self.send(Resp::Copied(out));
            }
            Req::Edit { id, edit } => self.edit(id, edit),
            Req::Undo { id } => self.history(id, true),
            Req::Redo { id } => self.history(id, false),
            Req::Extract { id, mut pages, path } => {
                pages.sort_unstable();
                pages.dedup();
                let result = (|| -> Result<(), PdfiumError> {
                    let Some(d) = self.docs.get(&id) else { return Ok(()) };
                    let mut new = self.pdfium.create_new_pdf()?;
                    let list = pages.iter().map(|p| (p + 1).to_string()).collect::<Vec<_>>().join(",");
                    new.pages_mut().copy_pages_from_document(&d.doc, &list, 0)?;
                    new.save_to_file(&path)
                })();
                let count = pages.len();
                self.send(match result {
                    Ok(()) => Resp::Notice {
                        text: if count == 1 { "Extracted 1 page.".into() } else { format!("Extracted {count} pages.") },
                        error: false,
                    },
                    Err(e) => Resp::Notice { text: format!("Can't extract. {e:?}"), error: true },
                });
            }
            Req::Print { id, job } => {
                let pages = self.docs.get(&id).map(|d| d.doc.pages().len() as u32).unwrap_or(0);
                let result = crate::print::run(job, pages, |index, dc, w, h| {
                    let Some(page) = self.page(id, index) else { return false };
                    // Fit the page inside the printable area, centred.
                    let (pw, ph) = (page.width().value.max(1.0), page.height().value.max(1.0));
                    let scale = (w as f32 / pw).min(h as f32 / ph);
                    let (sx, sy) = ((pw * scale) as c_int, (ph * scale) as c_int);
                    let b = page.bindings();
                    // 0x01 draws annotations, 0x800 tells PDFium the target is a printer.
                    unsafe {
                        b.FPDF_RenderPage(dc, b.get_handle_from_page(page), (w - sx) / 2, (h - sy) / 2, sx, sy, 0, 0x801)
                    };
                    true
                });
                self.send(match result {
                    Ok(1) => Resp::Notice { text: "Sent 1 page to the printer.".into(), error: false },
                    Ok(count) => Resp::Notice { text: format!("Sent {count} pages to the printer."), error: false },
                    Err(e) => Resp::Notice { text: format!("Can't print. {e}"), error: true },
                });
            }
            Req::Tile { .. } | Req::Thumb { .. } | Req::Text { .. } | Req::Search { .. } => {}
        }
    }

    /// The whole document as bytes, for an undo point.
    fn snapshot(&self, id: DocId) -> Result<Vec<u8>, PdfiumError> {
        self.docs.get(&id).ok_or(PdfiumError::PageIndexOutOfBounds)?.doc.save_to_bytes()
    }

    /// Applies a user edit and records how to undo it.
    fn edit(&mut self, id: DocId, edit: Edit) {
        let pages_len = self.docs.get(&id).map(|d| d.doc.pages().len() as u32).unwrap_or(0);
        let (note, outcome): (&'static str, Outcome) = match edit {
            Edit::Rotate { pages, clockwise } => ("", self.run_step(id, Step::Rotate { pages, clockwise })),
            Edit::Move { mut pages, before } => {
                pages.sort_unstable();
                pages.dedup();
                pages.retain(|p| *p < pages_len);
                // Work out one move per page by simulating the list.
                let mut order: Vec<u32> = (0..pages_len).collect();
                let start = before.min(pages_len) - pages.iter().filter(|p| **p < before).count() as u32;
                let mut moves = Vec::new();
                for (t, page) in pages.iter().enumerate() {
                    let target = start as usize + t;
                    let Some(from) = order.iter().position(|p| p == page) else { continue };
                    if from != target && target < order.len() {
                        let p = order.remove(from);
                        order.insert(target, p);
                        moves.push((from as u32, target as u32));
                    }
                }
                if moves.is_empty() {
                    return;
                }
                ("Moved.", self.run_step(id, Step::Moves(moves)))
            }
            Edit::Markup { kind, from, to, color } => {
                // Line rectangles come from the cached page text, converted to page space.
                let mut specs = Vec::new();
                for page in from.0..=to.0.min(pages_len.saturating_sub(1)) {
                    let Some(text) = self.text(id, page) else { continue };
                    if text.boxes.is_empty() {
                        continue;
                    }
                    let a = if page == from.0 { from.1 as usize } else { 0 };
                    let b = (if page == to.0 { to.1 as usize } else { usize::MAX }).min(text.boxes.len() - 1);
                    if a > b {
                        continue;
                    }
                    let rects: Vec<[f32; 4]> =
                        line_rects(&text.boxes[a..=b]).iter().map(|r| text.rect_to_page(r)).collect();
                    if !rects.is_empty() {
                        specs.push(AnnotSpec::Markup { page, kind, rects, color });
                    }
                }
                if specs.is_empty() {
                    return;
                }
                let note = match kind {
                    Markup::Highlight => "Highlighted.",
                    Markup::Underline => "Underlined.",
                    Markup::Strikeout => "Struck out.",
                };
                (note, self.run_step(id, Step::AddAnnots(specs)))
            }
            Edit::Note { page, at, text } => {
                let Some(t) = self.text(id, page) else { return };
                let (x, y) = t.to_page(at[0], at[1]);
                ("Note added.", self.run_step(id, Step::AddAnnots(vec![AnnotSpec::Note { page, at: [x, y], text }])))
            }
            Edit::Ink { page, points, color, width } => {
                let Some(t) = self.text(id, page) else { return };
                let points: Vec<[f32; 2]> = points.iter().map(|p| t.to_page(p[0], p[1])).map(|(x, y)| [x, y]).collect();
                if points.len() < 2 {
                    return;
                }
                ("", self.run_step(id, Step::AddAnnots(vec![AnnotSpec::Ink { page, points, color, width }])))
            }
            // The remaining edits have no cheap inverse: keep the document as it was.
            Edit::Delete { mut pages } => {
                pages.sort_unstable_by(|a, b| b.cmp(a));
                pages.dedup();
                let snapshot = self.snapshot(id);
                self.drop_pages(id);
                let result = snapshot.and_then(|bytes| {
                    let d = self.docs.get(&id).ok_or(PdfiumError::PageIndexOutOfBounds)?;
                    for &i in &pages {
                        d.doc.pages().get(i as PdfPageIndex)?.delete()?;
                    }
                    Ok((Step::Restore(bytes), Changed::All))
                });
                ("", result)
            }
            Edit::Insert { path, at } => {
                let snapshot = self.snapshot(id);
                self.drop_pages(id);
                let pdfium = self.pdfium;
                let result = snapshot.and_then(|bytes| {
                    let src = pdfium.load_pdf_from_file(&path, None)?;
                    let count = src.pages().len();
                    let d = self.docs.get_mut(&id).ok_or(PdfiumError::PageIndexOutOfBounds)?;
                    d.doc.pages_mut().copy_pages_from_document(&src, &format!("1-{count}"), at.min(pages_len) as PdfPageIndex)?;
                    Ok((Step::Restore(bytes), Changed::All))
                });
                ("Inserted.", result)
            }
            Edit::RemoveAnnot { page, at } => {
                let Some(text) = self.text(id, page) else { return };
                let (x, y) = text.to_page(at[0], at[1]);
                let Some(p) = self.page(id, page) else { return };
                let target = annots(p)
                    .into_iter()
                    .rev()
                    .find(|a| a.rects.iter().any(|r| x >= r[0] - 2.0 && x <= r[2] + 2.0 && y >= r[1] - 2.0 && y <= r[3] + 2.0));
                let Some(target) = target else { return };
                let snapshot = self.snapshot(id);
                self.drop_pages(id);
                let result = snapshot.and_then(|bytes| {
                    let d = self.docs.get(&id).ok_or(PdfiumError::PageIndexOutOfBounds)?;
                    let p = d.doc.pages().get(page as PdfPageIndex)?;
                    let b = p.bindings();
                    unsafe { b.FPDFPage_RemoveAnnot(b.get_handle_from_page(&p), target.index) };
                    Ok((Step::Restore(bytes), Changed::Pages(vec![page])))
                });
                ("Removed.", result)
            }
        };
        match outcome {
            Ok((inverse, changed)) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    d.redo.clear();
                    d.undo.push(inverse);
                    // Keep the history within its step and memory limits, dropping the oldest.
                    while d.undo.len() > UNDO_STEPS || d.undo.iter().map(Step::bytes).sum::<usize>() > UNDO_BYTES {
                        d.undo.remove(0);
                    }
                }
                self.changed(id, changed, note);
            }
            Err(e) => {
                self.send(Resp::EditFailed { error: format!("{e:?}") });
                // Some pages may already have changed, so lay everything out again.
                self.changed(id, Changed::All, "");
            }
        }
    }

    /// Undo or redo: runs the newest step of one stack and files its inverse on the other.
    fn history(&mut self, id: DocId, undo: bool) {
        let Some(d) = self.docs.get_mut(&id) else { return };
        let Some(step) = (if undo { d.undo.pop() } else { d.redo.pop() }) else { return };
        match self.run_step(id, step) {
            Ok((inverse, changed)) => {
                if let Some(d) = self.docs.get_mut(&id) {
                    if undo { d.redo.push(inverse) } else { d.undo.push(inverse) }
                }
                self.changed(id, changed, if undo { "Undone." } else { "Redone." });
            }
            Err(e) => {
                self.send(Resp::EditFailed { error: format!("{e:?}") });
                self.changed(id, Changed::All, "");
            }
        }
    }

    /// Runs one step and returns the step that reverses it.
    fn run_step(&mut self, id: DocId, step: Step) -> Outcome {
        self.drop_pages(id);
        let missing = PdfiumError::PageIndexOutOfBounds;
        match step {
            Step::Rotate { pages, clockwise } => {
                let d = self.docs.get(&id).ok_or(missing)?;
                for &i in &pages {
                    let mut p = d.doc.pages().get(i as PdfPageIndex)?;
                    use PdfPageRenderRotation::*;
                    let next = match (p.rotation()?, clockwise) {
                        (None, true) | (Degrees180, false) => Degrees90,
                        (Degrees90, true) | (Degrees270, false) => Degrees180,
                        (Degrees180, true) | (None, false) => Degrees270,
                        (Degrees270, true) | (Degrees90, false) => None,
                    };
                    p.set_rotation(next);
                }
                Ok((Step::Rotate { pages, clockwise: !clockwise }, Changed::All))
            }
            Step::Moves(moves) => {
                let d = self.docs.get(&id).ok_or(missing)?;
                let b = d.doc.bindings();
                let handle = b.get_handle_from_document(&d.doc);
                for &(from, to) in &moves {
                    let index = [from as c_int];
                    if !b.is_true(unsafe { b.FPDF_MovePages(handle, index.as_ptr(), 1, to as c_int) }) {
                        return Err(PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::Unknown));
                    }
                }
                let back = moves.iter().rev().map(|&(from, to)| (to, from)).collect();
                Ok((Step::Moves(back), Changed::All))
            }
            Step::AddAnnots(specs) => {
                let d = self.docs.get(&id).ok_or(missing)?;
                for spec in &specs {
                    let mut p = d.doc.pages().get(spec.page() as PdfPageIndex)?;
                    // Annotations live outside the page content, so leave the content alone.
                    p.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
                    create_annot(&p, spec)?;
                }
                let pages = specs.iter().map(AnnotSpec::page).collect();
                Ok((Step::PopAnnots(specs), Changed::Pages(pages)))
            }
            Step::PopAnnots(specs) => {
                let d = self.docs.get(&id).ok_or(missing)?;
                for spec in specs.iter().rev() {
                    let mut p = d.doc.pages().get(spec.page() as PdfPageIndex)?;
                    p.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
                    let b = p.bindings();
                    let handle = b.get_handle_from_page(&p);
                    unsafe {
                        let count = b.FPDFPage_GetAnnotCount(handle);
                        if count > 0 {
                            b.FPDFPage_RemoveAnnot(handle, count - 1);
                        }
                    }
                }
                let pages = specs.iter().map(AnnotSpec::page).collect();
                Ok((Step::AddAnnots(specs), Changed::Pages(pages)))
            }
            Step::Restore(bytes) => {
                let current = self.snapshot(id)?;
                let password = self.docs.get(&id).ok_or(missing)?.password.clone();
                let doc = match self.pdfium.load_pdf_from_byte_vec(bytes.clone(), password.as_deref()) {
                    Ok(doc) => doc,
                    // A saved copy may no longer be encrypted.
                    Err(_) => self.pdfium.load_pdf_from_byte_vec(bytes, None)?,
                };
                if let Some(d) = self.docs.get_mut(&id) {
                    d.doc = doc;
                    d.ocr.clear();
                }
                Ok((Step::Restore(current), Changed::All))
            }
        }
    }

    /// Tells the UI what changed and what it can now undo and redo.
    fn changed(&mut self, id: DocId, changed: Changed, note: &'static str) {
        self.drop_pages(id);
        let Some(d) = self.docs.get_mut(&id) else { return };
        d.epoch += 1;
        if matches!(changed, Changed::All) {
            // Pages may have moved, so recognised text no longer lines up with page numbers.
            d.ocr.clear();
        }
        let resp = Resp::Edited {
            id,
            epoch: d.epoch,
            sizes: sizes_of(&d.doc),
            outline: outline_of(&d.doc),
            pages: match changed {
                Changed::All => None,
                Changed::Pages(p) => Some(p),
            },
            note,
            can_undo: !d.undo.is_empty(),
            can_redo: !d.redo.is_empty(),
        };
        self.send(resp);
    }

    fn tile(&mut self, j: TileJob) {
        let Some(epoch) = self.docs.get(&j.id).map(|d| d.epoch) else { return };
        let Some(page) = self.page(j.id, j.page) else { return };
        let pw = (page.width().value * j.scale).round().max(1.0) as i32;
        let ph = (page.height().value * j.scale).round().max(1.0) as i32;
        let (x0, y0) = ((j.tx * TILE) as i32, (j.ty * TILE) as i32);
        if x0 >= pw || y0 >= ph {
            return;
        }
        let (w, h) = ((pw - x0).min(TILE as i32), (ph - y0).min(TILE as i32));
        let Some(image) = render(page, pw, ph, x0, y0, w, h) else { return };
        let bytes = (w * h * 4) as usize;
        let tex = self.ctx.load_texture("tile", image, TextureOptions::LINEAR);
        self.send(Resp::Tile { id: j.id, epoch, page: j.page, scale: j.scale, tx: j.tx, ty: j.ty, tex, bytes });
    }

    fn thumb(&mut self, j: ThumbJob) {
        let Some(epoch) = self.docs.get(&j.id).map(|d| d.epoch) else { return };
        let Some(page) = self.page(j.id, j.page) else { return };
        let scale = j.width as f32 / page.width().value.max(1.0);
        let pw = j.width.max(1) as i32;
        let ph = (page.height().value * scale).round().clamp(1.0, 4096.0) as i32;
        let Some(image) = render(page, pw, ph, 0, 0, pw, ph) else { return };
        let tex = self.ctx.load_texture("thumb", image, TextureOptions::LINEAR);
        self.send(Resp::Thumb { id: j.id, epoch, page: j.page, tex });
    }
}

/// Renders the `w`×`h` window at (`x0`, `y0`) of a page drawn `pw`×`ph` pixels large.
fn render(page: &PdfPage<'static>, pw: i32, ph: i32, x0: i32, y0: i32, w: i32, h: i32) -> Option<ColorImage> {
    let mut bitmap = PdfBitmap::empty(w, h, PdfBitmapFormat::BGRA).ok()?;
    let config = PdfRenderConfig::new()
        .set_target_size(pw, ph)
        .set_origin(-x0, -y0)
        .set_format(PdfBitmapFormat::BGRA)
        .set_reverse_byte_order(true)
        .set_clear_color(PdfColor::new(255, 255, 255, 255))
        .render_annotations(true)
        .render_form_data(true);
    page.render_into_bitmap_with_config(&mut bitmap, &config).ok()?;
    let raw = bitmap.as_raw_bytes();
    let (w, h) = (w as usize, h as usize);
    let stride = raw.len() / h;
    if stride == w * 4 {
        Some(ColorImage::from_rgba_premultiplied([w, h], &raw))
    } else {
        let mut packed = Vec::with_capacity(w * h * 4);
        for row in raw.chunks_exact(stride) {
            packed.extend_from_slice(&row[..w * 4]);
        }
        Some(ColorImage::from_rgba_premultiplied([w, h], &packed))
    }
}
