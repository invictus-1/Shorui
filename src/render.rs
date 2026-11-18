//! The PDFium worker. PDFium is single-threaded, so one thread owns the library and every open
//! document. The UI sends requests and gets back ready-to-draw textures; it never waits.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{ColorImage, TextureHandle, TextureOptions};
use pdfium_render::prelude::*;

/// Tile edge in device pixels.
pub const TILE: u32 = 1024;
/// How many parsed pages stay loaded for fast re-rendering.
const PAGE_CACHE: usize = 24;

pub type DocId = u64;

/// Generation counters. The UI bumps one to make every older queued request of that kind stale.
#[derive(Default)]
pub struct Gens {
    pub tile: AtomicU64,
    pub thumb: AtomicU64,
    pub search: AtomicU64,
}

pub enum Req {
    Open { id: DocId, path: PathBuf, password: Option<String> },
    Close { id: DocId },
    Tile { id: DocId, page: u32, scale: f32, tx: u32, ty: u32, stamp: u64 },
    Thumb { id: DocId, page: u32, width: u32, stamp: u64 },
    Rotate { id: DocId, pages: Vec<u32>, clockwise: bool },
    Delete { id: DocId, pages: Vec<u32> },
    Save { id: DocId, path: PathBuf },
    /// Text and character boxes of one page, for selection.
    Text { id: DocId, page: u32 },
    /// Case-insensitive search of the whole document. A newer search stamp cancels this one.
    Search { id: DocId, query: String, stamp: u64 },
    /// The text between two (page, character) positions, inclusive.
    Copy { id: DocId, from: (u32, u32), to: (u32, u32) },
    /// Adds a highlight annotation over the text between two positions, inclusive.
    Highlight { id: DocId, from: (u32, u32), to: (u32, u32) },
    /// Removes the topmost highlight under a point given in top-left page points.
    Unhighlight { id: DocId, page: u32, at: [f32; 2] },
}

/// The characters of a page in reading order, each with its box in points from the page's
/// top-left corner (after page rotation). Characters without a shape have an empty box.
pub struct PageText {
    pub chars: Vec<char>,
    pub boxes: Vec<[f32; 4]>,
    /// Bounds of the highlight annotations already on the page, in the same space as `boxes`.
    pub highlights: Vec<[f32; 4]>,
    /// PDF page space to top-left points: origin, then the images of the x and y unit vectors.
    m: [f32; 6],
}

impl PageText {
    fn to_view(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.m;
        (m[0] + m[2] * x + m[4] * y, m[1] + m[3] * x + m[5] * y)
    }

    /// A PDF-space rectangle as [x0, y0, x1, y1] in top-left points.
    fn rect_to_view(&self, r: &PdfRect) -> [f32; 4] {
        let (x0, y0) = self.to_view(r.left().value, r.bottom().value);
        let (x1, y1) = self.to_view(r.right().value, r.top().value);
        [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]
    }

    /// A top-left-points rectangle as [left, bottom, right, top] in PDF page space.
    fn rect_to_page(&self, r: &[f32; 4]) -> [f32; 4] {
        let m = &self.m;
        let det = m[2] * m[5] - m[4] * m[3];
        let inv = |vx: f32, vy: f32| {
            let (dx, dy) = (vx - m[0], vy - m[1]);
            ((dx * m[5] - m[4] * dy) / det, (m[2] * dy - m[3] * dx) / det)
        };
        let (x0, y0) = inv(r[0], r[1]);
        let (x1, y1) = inv(r[2], r[3]);
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
    Opened { id: DocId, sizes: Vec<[f32; 2]> },
    OpenFailed { id: DocId, error: String, needs_password: bool },
    Tile { id: DocId, epoch: u64, page: u32, scale: f32, tx: u32, ty: u32, tex: TextureHandle, bytes: usize },
    Thumb { id: DocId, epoch: u64, page: u32, tex: TextureHandle },
    Edited { id: DocId, epoch: u64, sizes: Vec<[f32; 2]> },
    EditFailed { error: String },
    Saved { id: DocId, path: PathBuf },
    SaveFailed { id: DocId, error: String },
    Text { id: DocId, epoch: u64, page: u32, text: Arc<PageText> },
    Hits { id: DocId, stamp: u64, page: u32, hits: Vec<Vec<[f32; 4]>> },
    SearchDone { id: DocId, stamp: u64 },
    Copied(String),
    /// Annotations on these pages changed. Only they need redrawing.
    Annotated { id: DocId, epoch: u64, pages: Vec<u32>, added: bool },
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

struct Doc {
    doc: PdfDocument<'static>,
    epoch: u64,
}

struct Worker {
    pdfium: &'static Pdfium,
    docs: HashMap<DocId, Doc>,
    pages: VecDeque<((DocId, u32), PdfPage<'static>)>,
    texts: VecDeque<((DocId, u32), Arc<PageText>)>,
    ctx: egui::Context,
    out: Sender<Resp>,
}

struct TileJob {
    id: DocId,
    page: u32,
    scale: f32,
    tx: u32,
    ty: u32,
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

/// The area a highlight covers, one rectangle per line, in top-left page points.
fn highlight_rects(a: &mut PdfPageAnnotation, text: &PageText) -> Vec<[f32; 4]> {
    let mut out = Vec::new();
    if let Some(h) = a.as_highlight_annotation_mut() {
        for quad in h.attachment_points_mut().iter() {
            out.push(text.rect_to_view(&quad.to_rect()));
        }
    }
    if out.is_empty() {
        if let Ok(b) = a.bounds() {
            out.push(text.rect_to_view(&b));
        }
    }
    out
}

/// Reads a page's characters and maps their boxes into top-left page points.
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

    let text = page.text().ok()?;
    let chars = text.chars();
    let mut out =
        PageText { chars: Vec::new(), boxes: Vec::new(), highlights: Vec::new(), m: [o.0, o.1, ax, ay, bx, by] };
    for mut a in page.annotations().iter() {
        if a.annotation_type() == PdfPageAnnotationType::Highlight {
            let rects = highlight_rects(&mut a, &out);
            out.highlights.extend(rects);
        }
    }
    for c in chars.iter() {
        out.chars.push(c.unicode_char().unwrap_or('\u{fffd}'));
        out.boxes.push(match c.loose_bounds() {
            Ok(r) => out.rect_to_view(&r),
            Err(_) => [0.0; 4],
        });
    }
    Some(out)
}

struct ThumbJob {
    id: DocId,
    page: u32,
    width: u32,
    stamp: u64,
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
    let mut w = Worker { pdfium, docs: HashMap::new(), pages: VecDeque::new(), texts: VecDeque::new(), ctx, out };

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

fn sizes_of(doc: &PdfDocument<'static>) -> Vec<[f32; 2]> {
    let pages = doc.pages();
    (0..pages.len())
        .map(|i| match pages.page_size(i) {
            Ok(r) => [r.width().value.max(1.0), r.height().value.max(1.0)],
            Err(_) => [612.0, 792.0],
        })
        .collect()
}

impl Worker {
    fn send(&self, r: Resp) {
        let _ = self.out.send(r);
        self.ctx.request_repaint();
    }

    /// A page's text, from a small cache shared by search, selection and copy.
    fn text(&mut self, id: DocId, page: u32) -> Option<Arc<PageText>> {
        if let Some((_, t)) = self.texts.iter().find(|(k, _)| *k == (id, page)) {
            return Some(t.clone());
        }
        let text = Arc::new(extract(self.page(id, page)?)?);
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
                        let sizes = sizes_of(&doc);
                        self.docs.insert(id, Doc { doc, epoch: 0 });
                        self.send(Resp::Opened { id, sizes });
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
            Req::Rotate { id, pages, clockwise } => {
                self.drop_pages(id);
                let result = (|| -> Result<(), PdfiumError> {
                    let Some(d) = self.docs.get(&id) else { return Ok(()) };
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
                    Ok(())
                })();
                self.edited(id, result);
            }
            Req::Delete { id, mut pages } => {
                self.drop_pages(id);
                pages.sort_unstable_by(|a, b| b.cmp(a));
                pages.dedup();
                let result = (|| -> Result<(), PdfiumError> {
                    let Some(d) = self.docs.get(&id) else { return Ok(()) };
                    for &i in &pages {
                        d.doc.pages().get(i as PdfPageIndex)?.delete()?;
                    }
                    Ok(())
                })();
                self.edited(id, result);
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
            Req::Highlight { id, from, to } => {
                // Work out the line rectangles first, while the page text is still cached.
                let mut plan: Vec<(u32, Vec<[f32; 4]>)> = Vec::new();
                for page in from.0..=to.0 {
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
                        plan.push((page, rects));
                    }
                }
                if plan.is_empty() {
                    return;
                }
                self.drop_pages(id);
                let result = (|| -> Result<(), PdfiumError> {
                    let Some(d) = self.docs.get(&id) else { return Ok(()) };
                    for (page, rects) in &plan {
                        let mut p = d.doc.pages().get(*page as PdfPageIndex)?;
                        // Annotations live outside the page content, so leave the content alone.
                        p.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
                        let mut a = p.annotations_mut().create_highlight_annotation()?;
                        let mut u = rects[0];
                        for r in rects {
                            u = [u[0].min(r[0]), u[1].min(r[1]), u[2].max(r[2]), u[3].max(r[3])];
                        }
                        a.set_bounds(PdfRect::new_from_values(u[1], u[0], u[3], u[2]))?;
                        a.set_stroke_color(PdfColor::new(255, 216, 77, 255))?;
                        for r in rects {
                            // PDF order: top-left, top-right, bottom-left, bottom-right.
                            let quad = PdfQuadPoints::new_from_values(r[0], r[3], r[2], r[3], r[0], r[1], r[2], r[1]);
                            a.attachment_points_mut().create_attachment_point_at_end(quad)?;
                        }
                    }
                    Ok(())
                })();
                self.annotated(id, plan.into_iter().map(|p| p.0).collect(), true, result);
            }
            Req::Unhighlight { id, page, at } => {
                let Some(text) = self.text(id, page) else { return };
                self.drop_pages(id);
                let mut removed = false;
                let result = (|| -> Result<(), PdfiumError> {
                    let Some(d) = self.docs.get(&id) else { return Ok(()) };
                    let mut p = d.doc.pages().get(page as PdfPageIndex)?;
                    p.set_content_regeneration_strategy(PdfPageContentRegenerationStrategy::Manual);
                    let annots = p.annotations_mut();
                    for i in annots.as_range().rev() {
                        let mut a = annots.get(i)?;
                        if a.annotation_type() != PdfPageAnnotationType::Highlight {
                            continue;
                        }
                        let rects = highlight_rects(&mut a, &text);
                        if rects.iter().any(|b| at[0] >= b[0] && at[0] <= b[2] && at[1] >= b[1] && at[1] <= b[3]) {
                            annots.delete_annotation(a)?;
                            removed = true;
                            break;
                        }
                    }
                    Ok(())
                })();
                if removed || result.is_err() {
                    self.annotated(id, vec![page], false, result);
                }
            }
            Req::Tile { .. } | Req::Thumb { .. } | Req::Text { .. } | Req::Search { .. } => {}
        }
    }

    fn annotated(&mut self, id: DocId, pages: Vec<u32>, added: bool, result: Result<(), PdfiumError>) {
        let Some(d) = self.docs.get_mut(&id) else { return };
        d.epoch += 1;
        let epoch = d.epoch;
        if let Err(e) = result {
            self.send(Resp::EditFailed { error: format!("{e:?}") });
        }
        self.send(Resp::Annotated { id, epoch, pages, added });
    }

    fn edited(&mut self, id: DocId, result: Result<(), PdfiumError>) {
        let Some(d) = self.docs.get_mut(&id) else { return };
        // Bump the epoch even on failure: some pages may already have changed.
        d.epoch += 1;
        let (epoch, sizes) = (d.epoch, sizes_of(&d.doc));
        if let Err(e) = result {
            self.send(Resp::EditFailed { error: format!("{e:?}") });
        }
        self.send(Resp::Edited { id, epoch, sizes });
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
