use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Id, Key, Modifiers, Rect, RichText, Sense, Stroke, StrokeKind,
    TextureHandle, Ui, Vec2, pos2, vec2,
};

use crate::{
    neural,
    ocr::Kind,
    render::{DocId, Edit, Gens, Link, Markup, OutlineItem, PageText, Req, Resp, TILE, line_rects},
    settings::{self, OpenZoom, Settings, Update},
    theme::{self, Icon, *},
};

/// Logical pixels per PDF point at 100% zoom (96 dpi).
const PT: f32 = 96.0 / 72.0;
const GAP: f32 = SPACE_4;
const MARGIN: f32 = SPACE_6;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 64.0;
/// Thumbnail width in device pixels. Also the instant placeholder under tiles.
const THUMB_W: u32 = 256;
/// How far beyond the viewport tiles are rendered ahead, in logical pixels.
const PREFETCH: f32 = 500.0;
/// Zoom must rest this long before pages are re-rendered sharp. Until then tiles are scaled.
const SETTLE: f64 = 0.08;
/// While zoom keeps changing, tiles are still refreshed at least this often.
const REFRESH: f64 = 0.18;
/// Tile texture budget per document.
const TILE_BUDGET: usize = 768 << 20;
const SIDE_W: f32 = 172.0;
const SIDE_CELL: f32 = 124.0;
const SIDE_LABEL: f32 = 22.0;
const ZOOM_STEPS: [f32; 15] = [0.1, 0.25, 0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 16.0, 32.0, 64.0];

const UV: Rect = Rect { min: pos2(0.0, 0.0), max: pos2(1.0, 1.0) };

#[derive(Clone, Copy, PartialEq)]
enum Fit {
    None,
    Width,
    Page,
}

struct Tile {
    scale: f32,
    tx: u32,
    ty: u32,
    tex: TextureHandle,
    bytes: usize,
    used: u64,
}

type TileKey = (u32, u32, u32, u32);

/// One search match: its page, one rectangle per line it spans (in page points), and the
/// words around it for the results list.
struct Hit {
    page: u32,
    rects: Vec<[f32; 4]>,
    before: String,
    matched: String,
    after: String,
}

/// What the sidebar shows.
#[derive(Clone, Copy, PartialEq)]
enum SideTab {
    Pages,
    Outline,
    Results,
}

/// Start screen card size.
const CARD_W: f32 = 148.0;
const COVER_H: f32 = 192.0;
const CARD_H: f32 = COVER_H + 54.0;

struct DocView {
    id: DocId,
    path: PathBuf,
    title: String,
    loading: bool,
    error: Option<String>,
    password: Option<String>,

    sizes: Vec<[f32; 2]>,
    /// Sum of page heights before page i, in points. Length n + 1.
    prefix: Vec<f32>,
    /// Sidebar cell tops. Length n + 1.
    side_prefix: Vec<f32>,
    max_w: f32,
    epoch: u64,

    zoom: f32,
    fit: Fit,
    needs_fit: bool,
    scroll: Vec2,
    view: Vec2,
    render_scale: f32,
    scale_set: f64,
    zoom_changed: f64,
    current: u32,
    complete: bool,

    tiles: HashMap<u32, Vec<Tile>>,
    tile_bytes: usize,
    pending: HashSet<TileKey>,
    thumbs: HashMap<u32, TextureHandle>,
    thumb_pending: HashSet<u32>,
    thumb_wanted: Vec<u32>,

    dirty: bool,
    close_after_save: bool,
    selected: BTreeSet<u32>,
    select_anchor: u32,
    page_field: String,
    side_view: (f32, f32),
    side_scroll_to: Option<u32>,
    outline: Vec<OutlineItem>,
    can_undo: bool,
    can_redo: bool,
    /// Page and zoom to return to, from the last session.
    restore: Option<(u32, f32)>,
    /// The pen stroke being drawn: page and points in page points.
    stroke: Option<(u32, Vec<[f32; 2]>)>,

    /// Layout rows: one page each, or two side by side in two-page view.
    two_page: bool,
    row_of: Vec<u32>,
    col: Vec<u8>,
    row_first: Vec<u32>,
    row_count: Vec<u8>,
    /// Sum of row heights before row r, in points. Length rows + 1.
    row_top: Vec<f32>,
    row_w: Vec<f32>,
    max_row_w: f32,

    /// Thumbnails being dragged to a new place, and where they would land.
    drag_pages: Option<Vec<u32>>,
    drop_before: Option<u32>,

    texts: HashMap<u32, Arc<PageText>>,
    text_pending: HashSet<u32>,
    /// Text selection: anchor and focus as (page, character index).
    sel: Option<((u32, u32), (u32, u32))>,
    selecting: bool,
    /// Where the right-click menu was opened: page and position in page points.
    menu_at: Option<(u32, Vec2)>,

    search_open: bool,
    focus_search: bool,
    query: String,
    hits: Vec<Hit>,
    active_hit: Option<usize>,
    search_done: bool,
    search_stamp: u64,
    /// Search options: match case, whole words only.
    case: bool,
    whole: bool,
    /// The page OCR is reading while the search waits for it.
    search_reading: Option<u32>,
}

impl DocView {
    fn new(id: DocId, path: PathBuf) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled".into());
        Self {
            id,
            path,
            title,
            loading: true,
            error: None,
            password: None,
            sizes: Vec::new(),
            prefix: vec![0.0],
            side_prefix: vec![0.0],
            max_w: 1.0,
            epoch: 0,
            zoom: 1.0,
            fit: Fit::None,
            needs_fit: true,
            scroll: Vec2::ZERO,
            view: vec2(800.0, 600.0),
            render_scale: 0.0,
            scale_set: 0.0,
            zoom_changed: 0.0,
            current: 0,
            complete: false,
            tiles: HashMap::new(),
            tile_bytes: 0,
            pending: HashSet::new(),
            thumbs: HashMap::new(),
            thumb_pending: HashSet::new(),
            thumb_wanted: Vec::new(),
            dirty: false,
            close_after_save: false,
            selected: BTreeSet::new(),
            select_anchor: 0,
            page_field: "1".into(),
            side_view: (0.0, 0.0),
            side_scroll_to: None,
            outline: Vec::new(),
            can_undo: false,
            can_redo: false,
            restore: None,
            stroke: None,
            two_page: false,
            row_of: Vec::new(),
            col: Vec::new(),
            row_first: Vec::new(),
            row_count: Vec::new(),
            row_top: vec![0.0],
            row_w: Vec::new(),
            max_row_w: 1.0,
            drag_pages: None,
            drop_before: None,
            texts: HashMap::new(),
            text_pending: HashSet::new(),
            sel: None,
            selecting: false,
            menu_at: None,
            search_open: false,
            focus_search: false,
            query: String::new(),
            hits: Vec::new(),
            active_hit: None,
            search_done: true,
            search_stamp: 0,
            case: false,
            whole: false,
            search_reading: None,
        }
    }

    fn n(&self) -> u32 {
        self.sizes.len() as u32
    }

    fn set_sizes(&mut self, sizes: Vec<[f32; 2]>, epoch: u64) {
        self.prefix = Vec::with_capacity(sizes.len() + 1);
        self.side_prefix = Vec::with_capacity(sizes.len() + 1);
        let (mut acc, mut side, mut max_w) = (0.0f32, SPACE_2, 1.0f32);
        for s in &sizes {
            self.prefix.push(acc);
            self.side_prefix.push(side);
            acc += s[1];
            side += Self::side_h(*s) + SIDE_LABEL + SPACE_2;
            max_w = max_w.max(s[0]);
        }
        self.prefix.push(acc);
        self.side_prefix.push(side);
        self.max_w = max_w;
        self.sizes = sizes;
        self.relayout();
        self.epoch = epoch;
        self.tiles.clear();
        self.tile_bytes = 0;
        self.thumbs.clear();
        self.pending.clear();
        self.thumb_pending.clear();
        self.texts.clear();
        self.text_pending.clear();
        self.sel = None;
        self.hits.clear();
        self.active_hit = None;
        self.complete = false;
        self.current = self.current.min(self.n().saturating_sub(1));
    }

    fn side_h(size: [f32; 2]) -> f32 {
        (SIDE_CELL * size[1] / size[0]).clamp(24.0, 280.0)
    }

    fn page_size(&self, i: u32) -> Vec2 {
        let s = self.sizes[i as usize];
        vec2(s[0], s[1]) * (self.zoom * PT)
    }

    /// Groups pages into rows: singles, or pairs in two-page view.
    fn relayout(&mut self) {
        let n = self.sizes.len();
        self.row_of.clear();
        self.col.clear();
        self.row_first.clear();
        self.row_count.clear();
        self.row_w.clear();
        self.row_top = vec![0.0];
        let (mut i, mut acc, mut max_w) = (0, 0.0f32, 1.0f32);
        while i < n {
            let count = if self.two_page && i + 1 < n { 2 } else { 1 };
            let w: f32 = (0..count).map(|k| self.sizes[i + k][0]).sum();
            let h = (0..count).map(|k| self.sizes[i + k][1]).fold(0.0, f32::max);
            let row = self.row_first.len() as u32;
            self.row_first.push(i as u32);
            self.row_count.push(count as u8);
            for k in 0..count {
                self.row_of.push(row);
                self.col.push(k as u8);
            }
            self.row_w.push(w);
            acc += h;
            self.row_top.push(acc);
            max_w = max_w.max(w);
            i += count;
        }
        self.max_row_w = max_w;
    }

    fn top(&self, i: u32, zoom: f32) -> f32 {
        let row = self.row_of[i as usize] as usize;
        MARGIN + GAP * row as f32 + self.row_top[row] * zoom * PT
    }

    fn total_h(&self) -> f32 {
        let rows = self.row_first.len();
        2.0 * MARGIN + GAP * rows.saturating_sub(1) as f32 + self.row_top[rows] * self.zoom * PT
    }

    fn pair_gap(&self) -> f32 {
        if self.two_page { GAP } else { 0.0 }
    }

    fn content_w(&self, zoom: f32) -> f32 {
        (self.max_row_w * zoom * PT + self.pair_gap() + 2.0 * MARGIN).max(self.view.x)
    }

    /// The left edge of a page in content coordinates. Rows are centred.
    fn page_x(&self, i: u32, zoom: f32) -> f32 {
        let row = self.row_of[i as usize] as usize;
        let gap = if self.row_count[row] == 2 { GAP } else { 0.0 };
        let left = (self.content_w(zoom) - (self.row_w[row] * zoom * PT + gap)) / 2.0;
        if self.col[i as usize] == 1 { left + self.sizes[i as usize - 1][0] * zoom * PT + GAP } else { left }
    }

    /// The first page of the row whose band (row plus the gap below it) contains content y.
    fn page_at(&self, y: f32) -> u32 {
        let rows = self.row_first.len();
        if rows == 0 {
            return 0;
        }
        let top = |r: usize| MARGIN + GAP * r as f32 + self.row_top[r] * self.zoom * PT;
        let (mut lo, mut hi) = (0usize, rows - 1);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if top(mid) <= y {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        self.row_first[lo]
    }

    fn clamp_scroll(&mut self) {
        let max_y = (self.total_h() - self.view.y).max(0.0);
        let max_x = (self.content_w(self.zoom) - self.view.x).max(0.0);
        self.scroll.y = self.scroll.y.clamp(0.0, max_y);
        self.scroll.x = self.scroll.x.clamp(0.0, max_x);
    }

    /// Changes zoom, keeping the document point under `anchor` (relative to the view) in place.
    fn set_zoom(&mut self, zoom: f32, anchor: Vec2, now: f64) {
        let new = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if new == self.zoom || self.n() == 0 {
            return;
        }
        let old = self.zoom;
        let cy = self.scroll.y + anchor.y;
        let i = self.page_at(cy);
        let h_old = self.sizes[i as usize][1] * old * PT;
        let off = cy - self.top(i, old);
        let (frac, rem) = if off < 0.0 {
            (0.0, off)
        } else if off > h_old {
            (1.0, off - h_old)
        } else {
            (off / h_old, 0.0)
        };
        let cx = self.scroll.x + anchor.x - self.content_w(old) / 2.0;

        self.zoom = new;
        let h_new = self.sizes[i as usize][1] * new * PT;
        self.scroll.y = self.top(i, new) + frac * h_new + rem - anchor.y;
        self.scroll.x = self.content_w(new) / 2.0 + cx * new / old - anchor.x;
        self.zoom_changed = now;
        self.clamp_scroll();
    }

    fn fit_zoom(&self, fit: Fit) -> f32 {
        if self.n() == 0 {
            return 1.0;
        }
        let w = (self.view.x - 2.0 * MARGIN - 12.0 - self.pair_gap()).max(50.0);
        match fit {
            Fit::Width | Fit::None => w / (self.max_row_w * PT),
            Fit::Page => {
                let row = self.row_of[self.current as usize] as usize;
                let row_h = self.row_top[row + 1] - self.row_top[row];
                let h = (self.view.y - 2.0 * MARGIN).max(50.0);
                (w / (self.row_w[row] * PT)).min(h / (row_h * PT))
            }
        }
    }

    fn go_to(&mut self, page: u32) {
        if self.n() == 0 {
            return;
        }
        let page = page.min(self.n() - 1);
        self.scroll.y = self.top(page, self.zoom) - GAP;
        self.clamp_scroll();
        self.current = page;
    }

    fn has_tile(&self, page: u32, scale: f32, tx: u32, ty: u32) -> bool {
        self.tiles.get(&page).is_some_and(|l| l.iter().any(|t| t.scale == scale && t.tx == tx && t.ty == ty))
    }

    fn insert_tile(&mut self, page: u32, tile: Tile, frame: u64) {
        self.pending.remove(&(page, tile.scale.to_bits(), tile.tx, tile.ty));
        let list = self.tiles.entry(page).or_default();
        if let Some(pos) = list.iter().position(|t| t.scale == tile.scale && t.tx == tile.tx && t.ty == tile.ty) {
            self.tile_bytes -= list[pos].bytes;
            list.swap_remove(pos);
        }
        self.tile_bytes += tile.bytes;
        list.push(tile);

        if self.tile_bytes > TILE_BUDGET {
            // Drop least-recently-drawn tiles until a quarter of the budget is free again.
            let mut ages: Vec<(u64, usize)> =
                self.tiles.values().flatten().filter(|t| t.used + 1 < frame).map(|t| (t.used, t.bytes)).collect();
            ages.sort_unstable();
            let mut excess = self.tile_bytes.saturating_sub(TILE_BUDGET / 4 * 3);
            let mut cutoff = None;
            for (used, bytes) in ages {
                if excess == 0 {
                    break;
                }
                excess = excess.saturating_sub(bytes);
                cutoff = Some(used);
            }
            if let Some(cutoff) = cutoff {
                let mut freed = 0;
                for list in self.tiles.values_mut() {
                    list.retain(|t| {
                        let keep = t.used > cutoff;
                        if !keep {
                            freed += t.bytes;
                        }
                        keep
                    });
                }
                self.tile_bytes -= freed;
                self.tiles.retain(|_, l| !l.is_empty());
            }
        }
    }

    /// The selection with its ends in document order.
    fn sel_range(&self) -> Option<((u32, u32), (u32, u32))> {
        self.sel.map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
    }

    /// Makes a search hit the active one and scrolls it into view if it is not already.
    fn show_hit(&mut self, idx: usize) {
        let Some(h) = self.hits.get(idx) else { return };
        let (Some(r), page) = (h.rects.first().copied(), h.page) else { return };
        self.active_hit = Some(idx);
        let k = self.zoom * PT;
        let y = self.top(page, self.zoom) + r[1] * k;
        if y < self.scroll.y + 40.0 || y + (r[3] - r[1]) * k > self.scroll.y + self.view.y - 60.0 {
            self.scroll.y = y - self.view.y * 0.35;
        }
        let x = self.page_x(page, self.zoom) + r[0] * k;
        if x < self.scroll.x + 20.0 || x + (r[2] - r[0]) * k > self.scroll.x + self.view.x - 40.0 {
            self.scroll.x = x - self.view.x / 2.0;
        }
        self.clamp_scroll();
    }

    /// Pages an edit applies to: the sidebar selection, or the current page.
    fn targets(&self) -> Vec<u32> {
        if self.selected.is_empty() { vec![self.current] } else { self.selected.iter().copied().collect() }
    }
}

/// Starts (or, for an empty query, cancels) a document search.
fn start_search(d: &mut DocView, req: &Sender<Req>, gens: &Gens) {
    let stamp = gens.search.fetch_add(1, Ordering::Relaxed) + 1;
    d.search_stamp = stamp;
    d.hits.clear();
    d.active_hit = None;
    d.search_reading = None;
    d.search_done = d.query.trim().is_empty();
    if !d.search_done {
        let _ = req.send(Req::Search { id: d.id, query: d.query.clone(), stamp, case: d.case, whole: d.whole });
    }
}

/// The character at a point given in page points. With `snap`, falls back to the nearest
/// character, preferring ones on the same line.
fn char_at(text: &PageText, p: Vec2, snap: bool) -> Option<u32> {
    let mut best: Option<(f32, usize)> = None;
    for (i, b) in text.boxes.iter().enumerate() {
        if b[2] - b[0] <= 0.0 || b[3] - b[1] <= 0.0 {
            continue;
        }
        let dx = (b[0] - p.x).max(p.x - b[2]).max(0.0);
        let dy = (b[1] - p.y).max(p.y - b[3]).max(0.0);
        if dx == 0.0 && dy == 0.0 {
            return Some(i as u32);
        }
        let dist = dx + 4.0 * dy;
        if snap && best.is_none_or(|(d, _)| dist < d) {
            best = Some((dist, i));
        }
    }
    best.map(|(_, i)| i as u32)
}

/// The run of letters and digits around character `i`.
fn word_at(text: &PageText, i: u32) -> (u32, u32) {
    let (mut a, mut b) = (i as usize, i as usize);
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '\'';
    if !text.chars.get(a).copied().is_some_and(is_word) {
        return (i, i);
    }
    while a > 0 && is_word(text.chars[a - 1]) {
        a -= 1;
    }
    while b + 1 < text.chars.len() && is_word(text.chars[b + 1]) {
        b += 1;
    }
    (a as u32, b as u32)
}

/// A small toggle button that shows text instead of an icon, for search options.
fn text_toggle(ui: &mut Ui, text: &str, tip: &str, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 28.0), Sense::click());
    let p = ui.painter();
    if on {
        p.rect_filled(rect, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(RADIUS_MD), BG_300);
    }
    let color = if on || resp.hovered() { INK } else { INK_MUTED };
    p.text(rect.center(), Align2::CENTER_CENTER, text, theme::label(), color);
    if text == "ab" {
        // "Whole word": a bracket under the letters.
        let y = rect.center().y + 7.0;
        p.line(
            vec![pos2(rect.center().x - 8.0, y - 2.5), pos2(rect.center().x - 8.0, y), pos2(rect.center().x + 8.0, y), pos2(rect.center().x + 8.0, y - 2.5)],
            Stroke::new(1.2, color),
        );
    }
    resp.on_hover_text(tip)
}

/// A search result's text: a little before the match, the match picked out, a little after.
fn snippet_job(h: &Hit, width: f32) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    let before: Vec<char> = h.before.chars().collect();
    let keep = before.len().min(28);
    let mut lead: String = before[before.len() - keep..].iter().collect();
    if keep < before.len() {
        lead = format!("\u{2026}{}", lead.trim_start());
    }
    let plain = TextFormat { font_id: theme::caption(), color: INK_MUTED, ..Default::default() };
    let hit = TextFormat { font_id: theme::caption(), color: INK, background: ACCENT_DEEP, ..Default::default() };
    job.append(&lead, 0.0, plain.clone());
    job.append(&h.matched, 0.0, hit);
    job.append(&h.after, 0.0, plain);
    job.wrap = egui::text::TextWrapping { max_width: width, max_rows: 2, break_anywhere: false, overflow_character: Some('\u{2026}') };
    job
}

/// One line of text cut to `width` with an ellipsis.
fn elided(ui: &Ui, text: &str, font: egui::FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: width, max_rows: 1, break_anywhere: true, overflow_character: Some('\u{2026}') };
    ui.painter().layout_job(job)
}

/// "3 min ago", "2 h ago", "5 days ago".
fn ago(when: u64) -> String {
    let secs = settings::now().saturating_sub(when);
    match secs {
        0..60 => "just now".into(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        86_400..172_800 => "yesterday".into(),
        _ if secs < 60 * 86_400 => format!("{} days ago", secs / 86_400),
        _ => format!("{} months ago", secs / (30 * 86_400)),
    }
}

enum Action {
    OpenDialog,
    Open(PathBuf),
    Save,
    SaveAs,
    Close(usize),
    Activate(usize),
    ZoomStep(i32),
    ZoomTo(f32),
    Fit(Fit),
    Rotate(bool),
    Delete,
    GoTo(u32),
    ToggleSidebar,
    SearchOpen,
    SearchStep(i32),
    SearchClose,
    Copy,
    SelectAll,
    Markup(Markup),
    Unhighlight(u32, Vec2),
    Settings,
    Undo,
    Redo,
    Print,
    TogglePen,
    NoteAt(u32, Vec2),
    MovePages(Vec<u32>, u32),
    InsertPdf(u32),
    Extract,
    TwoPage,
    ReopenClosed,
    ToggleNight,
    Present(bool),
    /// Next or previous page (or pair of pages) in presentation mode.
    PresentStep(i32),
    PinRecent(PathBuf),
    ForgetRecent(PathBuf),
    ClearRecent,
    ShowInFolder(PathBuf),
    Side(SideTab),
    ShowHit(usize),
}

enum Confirm {
    CloseTab(DocId),
    Quit,
}

struct Toast {
    text: String,
    until: f64,
    error: bool,
}

/// Scripted run used to measure and screenshot the app: SHORUI_SELFTEST=<output dir>.
struct SelfTest {
    dir: PathBuf,
    phase: u32,
    frames: u32,
    log: String,
    zoom_started: Option<Instant>,
    max_ui: Duration,
    sum_ui: Duration,
    shots: Arc<std::sync::atomic::AtomicU32>,
}

pub struct App {
    req: Sender<Req>,
    resp: Receiver<Resp>,
    gens: Arc<Gens>,
    docs: Vec<DocView>,
    active: usize,
    next_id: DocId,
    sidebar: bool,
    frame: u64,
    toast: Option<Toast>,
    fatal: Option<String>,
    confirm: Option<Confirm>,
    password_prompt: Option<(DocId, String, bool)>,
    quit_confirmed: bool,
    shown_title: String,
    t0: Instant,
    first_frame: Option<Duration>,
    selftest: Option<SelfTest>,
    clipboard: Option<String>,
    last_copied: Option<String>,
    settings: Settings,
    settings_open: bool,
    update: Update,
    update_tx: std::sync::mpsc::Sender<Update>,
    update_rx: std::sync::mpsc::Receiver<Update>,
    /// True while the running check was started by the app itself, not by the user.
    quiet_check: bool,
    about_icon: Option<TextureHandle>,
    pen: bool,
    note_prompt: Option<(u32, Vec2, String)>,
    side_tab: SideTab,
    /// Tabs closed this session, newest last, for Ctrl+Shift+T: file, page, zoom.
    closed: Vec<(PathBuf, u32, f32)>,
    /// First pages of recent files for the start screen, with their page counts.
    covers: HashMap<PathBuf, (Option<TextureHandle>, u32)>,
    cover_pending: HashSet<PathBuf>,
    /// In presentation mode: the fit and zoom to return to.
    present: Option<(Fit, f32)>,
    /// Wheel movement collected in presentation mode, where the wheel turns pages.
    wheel: f32,
    pack_tx: std::sync::mpsc::Sender<neural::Progress>,
    pack_rx: std::sync::mpsc::Receiver<neural::Progress>,
    /// A neural OCR download in progress: the engine, bytes done, bytes in all.
    ocr_download: Option<(Kind, u64, u64)>,
    ocr_error: Option<String>,
}

impl App {
    /// `files` were already sent to the worker as `Req::Open` with ids 1..=n, so PDFium loads
    /// and parses them while the window is still being created.
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        files: Vec<PathBuf>,
        t0: Instant,
        gens: Arc<Gens>,
        req: Sender<Req>,
        resp: Receiver<Resp>,
    ) -> Self {
        let created = t0.elapsed();
        theme::apply(&cc.egui_ctx);
        let themed = t0.elapsed();
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let settings = Settings::load();
        gens.ocr.store(settings.ocr, Ordering::Relaxed);
        let (update_tx, update_rx) = std::sync::mpsc::channel();
        let (pack_tx, pack_rx) = std::sync::mpsc::channel();
        let selftest = std::env::var_os("SHORUI_SELFTEST").map(|d| SelfTest {
            dir: PathBuf::from(d),
            phase: 0,
            frames: 0,
            log: format!(
                "window and GPU context ready at {:.1} ms, fonts and theme took {:.1} ms
",
                created.as_secs_f64() * 1000.0,
                (themed - created).as_secs_f64() * 1000.0
            ),
            zoom_started: None,
            max_ui: Duration::ZERO,
            sum_ui: Duration::ZERO,
            shots: Arc::new(0.into()),
        });
        let mut app = Self {
            req,
            resp,
            gens,
            docs: Vec::new(),
            active: 0,
            next_id: 1,
            sidebar: settings.sidebar,
            frame: 0,
            toast: None,
            fatal: None,
            confirm: None,
            password_prompt: None,
            quit_confirmed: false,
            shown_title: String::new(),
            t0,
            first_frame: None,
            selftest,
            clipboard: None,
            last_copied: None,
            settings,
            settings_open: false,
            update: Update::Idle,
            update_tx,
            update_rx,
            quiet_check: false,
            about_icon: None,
            pen: false,
            note_prompt: None,
            side_tab: SideTab::Pages,
            closed: Vec::new(),
            covers: HashMap::new(),
            cover_pending: HashSet::new(),
            present: None,
            wheel: 0.0,
            pack_tx,
            pack_rx,
            ocr_download: None,
            ocr_error: None,
        };
        for f in files {
            let mut doc = DocView::new(app.next_id, f);
            doc.two_page = app.settings.two_page;
            doc.restore = app.settings.session.iter().find(|s| s.0 == doc.path).map(|s| (s.1, s.2));
            if app.selftest.is_none() && doc.restore.is_none() {
                doc.restore = app.settings.place(&doc.path);
            }
            if app.selftest.is_none() {
                app.settings.remember(&doc.path);
            }
            app.docs.push(doc);
            app.next_id += 1;
        }
        app.active = app.docs.len().saturating_sub(1);
        // Self-test only: install a neural engine through the real download path first.
        if let (Some(_), Some(kind)) =
            (&app.selftest, std::env::var("SHORUI_SELFTEST_INSTALL").ok().and_then(|k| Kind::from_key(&k)))
        {
            if neural::installed(kind) {
                app.use_engine(kind);
            } else {
                app.ocr_download = Some((kind, 0, neural::download_size(kind)));
                neural::install(kind, app.pack_tx.clone(), cc.egui_ctx.clone());
            }
        }
        if app.selftest.is_none() && !app.docs.is_empty() {
            app.settings.save();
        }
        if app.selftest.is_none() && app.settings.auto_update && settings::now() > app.settings.last_check + 86_400 {
            app.settings.last_check = settings::now();
            app.settings.save();
            app.update = Update::Checking;
            app.quiet_check = true;
            settings::check(app.update_tx.clone(), cc.egui_ctx.clone());
        }
        app
    }

    fn doc(&mut self) -> Option<&mut DocView> {
        self.docs.get_mut(self.active)
    }

    fn doc_by_id(&mut self, id: DocId) -> Option<&mut DocView> {
        self.docs.iter_mut().find(|d| d.id == id)
    }

    fn invalidate_requests(&mut self) {
        self.gens.tile.fetch_add(1, Ordering::Relaxed);
        self.gens.thumb.fetch_add(1, Ordering::Relaxed);
        for d in &mut self.docs {
            d.pending.clear();
            d.thumb_pending.clear();
        }
    }

    fn open(&mut self, path: PathBuf) {
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if let Some(i) = self.docs.iter().position(|d| std::fs::canonicalize(&d.path).is_ok_and(|p| p == canon)) {
            self.activate(i);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut doc = DocView::new(id, path.clone());
        doc.two_page = self.settings.two_page;
        if self.selftest.is_none() {
            doc.restore = self.settings.place(&path);
            self.settings.remember(&path);
            self.settings.save();
        }
        self.docs.push(doc);
        let _ = self.req.send(Req::Open { id, path, password: None });
        self.activate(self.docs.len() - 1);
    }

    fn activate(&mut self, i: usize) {
        if i < self.docs.len() && i != self.active {
            self.active = i;
            self.invalidate_requests();
        } else if i < self.docs.len() {
            self.active = i;
        }
    }

    fn close(&mut self, i: usize) {
        if i >= self.docs.len() {
            return;
        }
        let d = self.docs.remove(i);
        let _ = self.req.send(Req::Close { id: d.id });
        if d.error.is_none() && !d.loading && d.n() > 0 {
            self.closed.push((d.path.clone(), d.current, d.zoom));
            if self.selftest.is_none() {
                self.settings.note_place(&d.path, d.current, d.zoom, d.n());
                self.settings.save();
            }
        }
        if self.active > i || self.active >= self.docs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.invalidate_requests();
    }

    /// Night reading on or off. Every page image is drawn again in the new colours.
    fn set_night(&mut self, on: bool) {
        self.settings.night = on;
        if self.selftest.is_none() {
            self.settings.save();
        }
        self.gens.night.store(on, Ordering::Relaxed);
        for d in &mut self.docs {
            d.tiles.clear();
            d.tile_bytes = 0;
            d.thumbs.clear();
            d.complete = false;
        }
        self.covers.clear();
        self.cover_pending.clear();
        self.invalidate_requests();
    }

    /// Presentation mode: full screen, one page (or pair) at a time, fitted and centred.
    fn set_present(&mut self, ctx: &egui::Context, on: bool) {
        let Some(d) = self.docs.get_mut(self.active).filter(|d| !d.loading && d.n() > 0) else { return };
        match (on, self.present) {
            (true, None) => {
                self.present = Some((d.fit, d.zoom));
                d.fit = Fit::Page;
                d.sel = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
            }
            (false, Some((fit, zoom))) => {
                self.present = None;
                let page = d.current;
                d.fit = fit;
                if fit == Fit::None {
                    d.zoom = zoom;
                }
                d.go_to(page);
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
            _ => {}
        }
    }

    /// Switches the OCR engine. Pages are read again by the new one as they are needed.
    fn use_engine(&mut self, kind: Kind) {
        self.settings.ocr_engine = kind;
        let _ = self.req.send(Req::OcrEngine(kind));
        if let Ok(mut s) = self.gens.ocr_status.lock() {
            *s = Default::default();
        }
        for d in &mut self.docs {
            d.texts.clear();
            d.text_pending.clear();
        }
    }

    /// Takes progress from a neural OCR download.
    fn poll_download(&mut self, now: f64) {
        while let Ok(p) = self.pack_rx.try_recv() {
            match p {
                neural::Progress::Bytes { done, total } => {
                    if let Some(d) = &mut self.ocr_download {
                        (d.1, d.2) = (done, total);
                    }
                }
                neural::Progress::Done(kind) => {
                    self.ocr_download = None;
                    self.use_engine(kind);
                    self.settings.save();
                    self.say("Neural OCR is ready.", false, now);
                }
                neural::Progress::Failed(e) => {
                    self.ocr_download = None;
                    self.ocr_error = Some(e);
                }
            }
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool, now: f64) {
        self.toast = Some(Toast { text: text.into(), until: now + if error { 6.0 } else { 2.0 }, error });
    }

    fn drain(&mut self, now: f64) {
        let frame = self.frame;
        while let Ok(r) = self.resp.try_recv() {
            match r {
                Resp::Fatal(e) => self.fatal = Some(e),
                Resp::Opened { id, sizes, outline } => {
                    let opened_ms = self.t0.elapsed().as_secs_f64() * 1000.0;
                    if let Some(t) = &mut self.selftest {
                        t.log += &format!("document opened at {opened_ms:.1} ms, {} pages\n", sizes.len());
                    }
                    if let Some(d) = self.doc_by_id(id) {
                        d.loading = false;
                        d.set_sizes(sizes, 0);
                        d.outline = outline;
                    }
                }
                Resp::OpenFailed { id, error, needs_password } => {
                    if needs_password {
                        let wrong = self.doc_by_id(id).is_some_and(|d| d.password.is_some());
                        self.password_prompt = Some((id, String::new(), wrong));
                    } else if let Some(d) = self.doc_by_id(id) {
                        d.loading = false;
                        d.error = Some(error);
                    }
                }
                Resp::Tile { id, epoch, page, scale, tx, ty, tex, bytes } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.epoch == epoch {
                            d.insert_tile(page, Tile { scale, tx, ty, tex, bytes, used: frame }, frame);
                        }
                    }
                }
                Resp::Thumb { id, epoch, page, tex } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.epoch == epoch {
                            d.thumb_pending.remove(&page);
                            d.thumbs.insert(page, tex);
                        }
                    }
                }
                Resp::Text { id, epoch, page, text } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.epoch == epoch {
                            d.text_pending.remove(&page);
                            d.texts.insert(page, text);
                        }
                    }
                }
                Resp::Hits { id, stamp, page, hits } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.search_stamp == stamp && d.hits.len() < 50_000 {
                            let first = d.hits.len();
                            d.hits.extend(hits.into_iter().map(|f| Hit {
                                page,
                                rects: f.rects,
                                before: f.before,
                                matched: f.matched,
                                after: f.after,
                            }));
                            // Land on the first match at or after the page being read.
                            if d.active_hit.is_none() && page >= d.current {
                                d.show_hit(first);
                            }
                        }
                    }
                }
                Resp::SearchReading { id, stamp, page } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.search_stamp == stamp {
                            d.search_reading = Some(page);
                        }
                    }
                }
                Resp::Cover { path, tex, pages } => {
                    self.cover_pending.remove(&path);
                    if pages > 0 {
                        if let Some(r) = self.settings.recent.iter_mut().find(|r| r.path == path && r.pages == 0) {
                            r.pages = pages;
                        }
                    }
                    self.covers.insert(path, (tex, pages));
                }
                Resp::SearchDone { id, stamp } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.search_stamp == stamp {
                            d.search_done = true;
                            d.search_reading = None;
                            if d.active_hit.is_none() && !d.hits.is_empty() {
                                d.show_hit(0);
                            }
                        }
                    }
                }
                Resp::Copied(text) => {
                    self.last_copied = Some(text.clone());
                    if !text.is_empty() {
                        self.clipboard = Some(text);
                        self.say("Copied.", false, now);
                    }
                }
                Resp::Edited { id, epoch, sizes, outline, pages, note, can_undo, can_redo } => {
                    let Self { docs, req, gens, .. } = self;
                    if let Some(d) = docs.iter_mut().find(|d| d.id == id) {
                        match pages {
                            // Only annotations changed: redraw just those pages.
                            Some(pages) => {
                                d.epoch = epoch;
                                for p in &pages {
                                    if let Some(list) = d.tiles.remove(p) {
                                        d.tile_bytes -= list.iter().map(|t| t.bytes).sum::<usize>();
                                    }
                                    d.thumbs.remove(p);
                                    d.texts.remove(p);
                                    d.text_pending.remove(p);
                                }
                                d.complete = false;
                                d.sel = None;
                            }
                            // The page list changed: lay everything out again.
                            None => {
                                d.set_sizes(sizes, epoch);
                                d.selected.retain(|&p| (p as usize) < d.sizes.len());
                                d.clamp_scroll();
                                if d.search_open && !d.query.trim().is_empty() {
                                    start_search(d, req, gens);
                                }
                            }
                        }
                        d.outline = outline;
                        d.dirty = true;
                        d.can_undo = can_undo;
                        d.can_redo = can_redo;
                    }
                    self.invalidate_requests();
                    if !note.is_empty() {
                        self.say(note, false, now);
                    }
                }
                Resp::Notice { text, error } => self.say(text, error, now),
                Resp::EditFailed { error } => self.say(format!("Can't change this page. {error}"), true, now),
                Resp::Saved { id, path } => {
                    let mut close = None;
                    if let Some(i) = self.docs.iter().position(|d| d.id == id) {
                        let d = &mut self.docs[i];
                        d.dirty = false;
                        d.title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        d.path = path;
                        if d.close_after_save {
                            close = Some(i);
                        }
                    }
                    self.say("Saved.", false, now);
                    if let Some(i) = close {
                        self.close(i);
                    }
                }
                Resp::SaveFailed { id, error } => {
                    if let Some(d) = self.doc_by_id(id) {
                        d.close_after_save = false;
                    }
                    self.say(format!("Can't save. {error}"), true, now);
                }
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let typing = ctx.memory(|m| m.focused().is_some());
        let modal = self.confirm.is_some() || self.password_prompt.is_some() || self.settings_open || self.note_prompt.is_some();
        if modal {
            return;
        }
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        if self.present.is_some() {
            // Presentation mode: keys turn pages; Esc or F5 goes back.
            ctx.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::NONE, Key::F5) {
                    actions.push(Action::Present(false));
                }
                for key in [Key::ArrowRight, Key::ArrowDown, Key::PageDown, Key::Space, Key::Enter, Key::N] {
                    if i.consume_key(Modifiers::NONE, key) {
                        actions.push(Action::PresentStep(1));
                    }
                }
                for key in [Key::ArrowLeft, Key::ArrowUp, Key::PageUp, Key::Backspace, Key::P] {
                    if i.consume_key(Modifiers::NONE, key) {
                        actions.push(Action::PresentStep(-1));
                    }
                }
                if i.consume_key(Modifiers::NONE, Key::Home) {
                    actions.push(Action::GoTo(0));
                }
                if i.consume_key(Modifiers::NONE, Key::End) {
                    actions.push(Action::GoTo(u32::MAX));
                }
            });
            return;
        }
        ctx.input_mut(|i| {
            if i.consume_key(ctrl_shift, Key::T) {
                actions.push(Action::ReopenClosed);
            }
            if i.consume_key(Modifiers::NONE, Key::F5) {
                actions.push(Action::Present(true));
            }
            if i.consume_key(Modifiers::COMMAND, Key::O) {
                actions.push(Action::OpenDialog);
            }
            if i.consume_key(ctrl_shift, Key::S) {
                actions.push(Action::SaveAs);
            }
            if i.consume_key(Modifiers::COMMAND, Key::S) {
                actions.push(Action::Save);
            }
            if i.consume_key(Modifiers::COMMAND, Key::W) {
                actions.push(Action::Close(self.active));
            }
            if i.consume_key(Modifiers::COMMAND, Key::P) {
                actions.push(Action::Print);
            }
            if i.consume_key(Modifiers::COMMAND, Key::Comma) {
                actions.push(Action::Settings);
            }
            if i.consume_key(Modifiers::COMMAND, Key::F) {
                actions.push(Action::SearchOpen);
            }
            if i.consume_key(Modifiers::SHIFT, Key::F3) {
                actions.push(Action::SearchStep(-1));
            }
            if i.consume_key(Modifiers::NONE, Key::F3) {
                actions.push(Action::SearchStep(1));
            }
            if self.docs.get(self.active).is_some_and(|d| d.search_open) && i.consume_key(Modifiers::NONE, Key::Escape) {
                actions.push(Action::SearchClose);
            }
            if i.consume_key(Modifiers::COMMAND, Key::B) {
                actions.push(Action::ToggleSidebar);
            }
            if i.consume_key(ctrl_shift, Key::R) {
                actions.push(Action::Rotate(false));
            }
            if i.consume_key(Modifiers::COMMAND, Key::R) {
                actions.push(Action::Rotate(true));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Plus) || i.consume_key(Modifiers::COMMAND, Key::Equals) {
                actions.push(Action::ZoomStep(1));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Minus) {
                actions.push(Action::ZoomStep(-1));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Num0) {
                actions.push(Action::ZoomTo(1.0));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Num1) {
                actions.push(Action::Fit(Fit::Width));
            }
            if i.consume_key(Modifiers::COMMAND, Key::Num3) {
                actions.push(Action::TwoPage);
            }
            if i.consume_key(Modifiers::COMMAND, Key::Num2) {
                actions.push(Action::Fit(Fit::Page));
            }
            if !self.docs.is_empty() {
                let n = self.docs.len();
                if i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Tab) {
                    actions.push(Action::Activate((self.active + n - 1) % n));
                }
                if i.consume_key(Modifiers::CTRL, Key::Tab) {
                    actions.push(Action::Activate((self.active + 1) % n));
                }
            }
        });
        if typing {
            return;
        }
        let Some(d) = self.docs.get_mut(self.active) else { return };
        if d.loading || d.n() == 0 {
            return;
        }
        let page_step = (d.view.y - 60.0).max(40.0);
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::PageDown) || i.consume_key(Modifiers::NONE, Key::Space) {
                d.scroll.y += page_step;
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) || i.consume_key(Modifiers::SHIFT, Key::Space) {
                d.scroll.y -= page_step;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                d.scroll.y += 60.0;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                d.scroll.y -= 60.0;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                actions.push(Action::GoTo(d.current + 1));
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                actions.push(Action::GoTo(d.current.saturating_sub(1)));
            }
            if i.consume_key(Modifiers::NONE, Key::Home) {
                actions.push(Action::GoTo(0));
            }
            if i.consume_key(Modifiers::NONE, Key::End) {
                actions.push(Action::GoTo(u32::MAX));
            }
            if i.consume_key(Modifiers::COMMAND, Key::A) {
                actions.push(Action::SelectAll);
            }
            if d.sel.is_some() && i.events.iter().any(|e| matches!(e, egui::Event::Copy)) {
                actions.push(Action::Copy);
            }
            if i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z) || i.consume_key(Modifiers::COMMAND, Key::Y) {
                actions.push(Action::Redo);
            }
            if i.consume_key(Modifiers::COMMAND, Key::Z) {
                actions.push(Action::Undo);
            }
            if i.consume_key(Modifiers::NONE, Key::P) {
                actions.push(Action::TogglePen);
            }
            if i.consume_key(Modifiers::NONE, Key::N) {
                actions.push(Action::ToggleNight);
            }
            if d.sel.is_some() && i.consume_key(Modifiers::NONE, Key::U) {
                actions.push(Action::Markup(Markup::Underline));
            }
            if d.sel.is_some() && i.consume_key(Modifiers::NONE, Key::S) {
                actions.push(Action::Markup(Markup::Strikeout));
            }
            if d.sel.is_some() && i.consume_key(Modifiers::NONE, Key::H) {
                actions.push(Action::Markup(Markup::Highlight));
            }
            if d.sel.is_some() && i.consume_key(Modifiers::NONE, Key::Escape) {
                d.sel = None;
            }
            if i.consume_key(Modifiers::NONE, Key::Delete) && !d.selected.is_empty() {
                actions.push(Action::Delete);
            }
        });
        d.clamp_scroll();
    }

    fn apply(&mut self, ctx: &egui::Context, actions: Vec<Action>) {
        let now = ctx.input(|i| i.time);
        for a in actions {
            match a {
                Action::OpenDialog => {
                    if let Some(files) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_files() {
                        for f in files {
                            self.open(f);
                        }
                    }
                }
                Action::Open(p) => self.open(p),
                Action::Save => {
                    if let Some(d) = self.docs.get(self.active) {
                        if !d.loading && d.error.is_none() {
                            let _ = self.req.send(Req::Save { id: d.id, path: d.path.clone() });
                        }
                    }
                }
                Action::SaveAs => {
                    let Some(d) = self.docs.get(self.active) else { continue };
                    if d.loading || d.error.is_some() {
                        continue;
                    }
                    let mut dialog = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(&d.title);
                    if let Some(dir) = d.path.parent() {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(path) = dialog.save_file() {
                        let _ = self.req.send(Req::Save { id: d.id, path });
                    }
                }
                Action::Close(i) => {
                    if let Some(d) = self.docs.get(i) {
                        if d.dirty {
                            self.confirm = Some(Confirm::CloseTab(d.id));
                        } else {
                            self.close(i);
                        }
                    }
                }
                Action::Activate(i) => self.activate(i),
                Action::ToggleSidebar => {
                    self.sidebar = !self.sidebar;
                    self.settings.sidebar = self.sidebar;
                    self.settings.save();
                }
                Action::Settings => self.settings_open = true,
                Action::SearchOpen => {
                    if let Some(d) = self.doc() {
                        d.search_open = true;
                        d.focus_search = true;
                    }
                }
                Action::SearchStep(dir) => {
                    if let Some(d) = self.doc() {
                        let len = d.hits.len() as i64;
                        if len > 0 {
                            let next = match d.active_hit {
                                Some(a) => (a as i64 + dir as i64).rem_euclid(len),
                                None => 0,
                            };
                            d.show_hit(next as usize);
                        }
                    }
                }
                Action::SearchClose => {
                    self.gens.search.fetch_add(1, Ordering::Relaxed);
                    if self.side_tab == SideTab::Results {
                        self.side_tab = SideTab::Pages;
                    }
                    if let Some(d) = self.doc() {
                        d.search_open = false;
                        d.hits.clear();
                        d.active_hit = None;
                        d.search_done = true;
                    }
                }
                Action::Copy => {
                    if let Some(d) = self.docs.get(self.active) {
                        if let Some((from, to)) = d.sel_range() {
                            let _ = self.req.send(Req::Copy { id: d.id, from, to });
                        }
                    }
                }
                Action::Markup(kind) => {
                    if let Some(d) = self.docs.get(self.active) {
                        if let Some((from, to)) = d.sel_range() {
                            let color = match kind {
                                Markup::Highlight => self.settings.highlight_rgb(),
                                Markup::Underline => [124, 58, 237],
                                Markup::Strikeout => [229, 72, 77],
                            };
                            let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Markup { kind, from, to, color } });
                        }
                    }
                }
                Action::Unhighlight(page, at) => {
                    if let Some(d) = self.docs.get(self.active) {
                        let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::RemoveAnnot { page, at: [at.x, at.y] } });
                    }
                }
                Action::Undo => {
                    if let Some(d) = self.docs.get(self.active) {
                        let _ = self.req.send(Req::Undo { id: d.id });
                    }
                }
                Action::Redo => {
                    if let Some(d) = self.docs.get(self.active) {
                        let _ = self.req.send(Req::Redo { id: d.id });
                    }
                }
                Action::Print => {
                    if let Some(d) = self.docs.get(self.active).filter(|d| !d.loading && d.n() > 0) {
                        // The dialog runs on its own thread so the document keeps rendering.
                        let (req, id, pages) = (self.req.clone(), d.id, d.n());
                        std::thread::spawn(move || {
                            if let Some(job) = crate::print::dialog(pages) {
                                let _ = req.send(Req::Print { id, job });
                            }
                        });
                    }
                }
                Action::TogglePen => {
                    self.pen = !self.pen;
                    if let Some(d) = self.doc() {
                        d.sel = None;
                        d.stroke = None;
                    }
                }
                Action::NoteAt(page, at) => self.note_prompt = Some((page, at, String::new())),
                Action::MovePages(pages, before) => {
                    if let Some(d) = self.doc() {
                        d.selected.clear();
                        let (id, edit) = (d.id, Edit::Move { pages, before });
                        let _ = self.req.send(Req::Edit { id, edit });
                    }
                }
                Action::InsertPdf(at) => {
                    let Some(id) = self.docs.get(self.active).map(|d| d.id) else { continue };
                    if let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() {
                        let _ = self.req.send(Req::Edit { id, edit: Edit::Insert { path, at } });
                    }
                }
                Action::Extract => {
                    let Some(d) = self.docs.get(self.active) else { continue };
                    let pages = d.targets();
                    let stem = d.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let mut dialog =
                        rfd::FileDialog::new().add_filter("PDF", &["pdf"]).set_file_name(format!("{stem} pages.pdf"));
                    if let Some(dir) = d.path.parent() {
                        dialog = dialog.set_directory(dir);
                    }
                    if let Some(path) = dialog.save_file() {
                        let _ = self.req.send(Req::Extract { id: d.id, pages, path });
                    }
                }
                Action::TwoPage => {
                    self.settings.two_page = !self.settings.two_page;
                    self.settings.save();
                    let two = self.settings.two_page;
                    for d in &mut self.docs {
                        let page = d.current;
                        d.two_page = two;
                        if !d.loading && d.n() > 0 {
                            d.relayout();
                            d.go_to(page);
                        }
                    }
                }
                Action::ReopenClosed => {
                    if let Some((path, page, zoom)) = self.closed.pop() {
                        let before = self.next_id;
                        self.open(path);
                        if let Some(d) = self.docs.iter_mut().find(|d| d.id == before) {
                            d.restore = Some((page, zoom));
                        }
                    }
                }
                Action::ToggleNight => {
                    let on = !self.settings.night;
                    self.set_night(on);
                }
                Action::Present(on) => self.set_present(ctx, on),
                Action::PresentStep(dir) => {
                    if let Some(d) = self.doc() {
                        let rows = d.row_first.len() as i64;
                        if rows > 0 {
                            let row = d.row_of[d.current as usize] as i64;
                            let next = (row + dir as i64).clamp(0, rows - 1) as usize;
                            d.go_to(d.row_first[next]);
                        }
                    }
                }
                Action::PinRecent(path) => {
                    if let Some(r) = self.settings.recent.iter_mut().find(|r| r.path == path) {
                        r.pinned = !r.pinned;
                    }
                    self.settings.save();
                }
                Action::ForgetRecent(path) => {
                    self.settings.recent.retain(|r| r.path != path);
                    self.settings.save();
                }
                Action::ClearRecent => {
                    self.settings.recent.retain(|r| r.pinned);
                    self.settings.save();
                }
                Action::ShowInFolder(path) => settings::show_in_folder(&path),
                Action::Side(tab) => self.side_tab = tab,
                Action::ShowHit(i) => {
                    if let Some(d) = self.doc() {
                        d.show_hit(i);
                    }
                }
                Action::SelectAll => {
                    if let Some(d) = self.doc() {
                        if d.n() > 0 {
                            d.sel = Some(((0, 0), (d.n() - 1, u32::MAX)));
                        }
                    }
                }
                Action::ZoomStep(dir) => {
                    if let Some(d) = self.doc() {
                        let z = d.zoom;
                        let next = if dir > 0 {
                            ZOOM_STEPS.iter().copied().find(|&s| s > z * 1.001).unwrap_or(MAX_ZOOM)
                        } else {
                            ZOOM_STEPS.iter().rev().copied().find(|&s| s < z * 0.999).unwrap_or(MIN_ZOOM)
                        };
                        d.fit = Fit::None;
                        d.set_zoom(next, d.view / 2.0, now);
                    }
                }
                Action::ZoomTo(z) => {
                    if let Some(d) = self.doc() {
                        d.fit = Fit::None;
                        d.set_zoom(z, d.view / 2.0, now);
                    }
                }
                Action::Fit(f) => {
                    if let Some(d) = self.doc() {
                        d.fit = f;
                        if f == Fit::Page {
                            let page = d.current;
                            d.set_zoom(d.fit_zoom(f), vec2(d.view.x / 2.0, 0.0), now);
                            d.go_to(page);
                        }
                    }
                }
                Action::GoTo(p) => {
                    if let Some(d) = self.doc() {
                        d.go_to(p);
                    }
                }
                Action::Rotate(clockwise) => {
                    if let Some(d) = self.docs.get(self.active) {
                        if !d.loading && d.n() > 0 {
                            let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Rotate { pages: d.targets(), clockwise } });
                        }
                    }
                }
                Action::Delete => {
                    let Some(d) = self.docs.get_mut(self.active) else { continue };
                    if d.loading || d.n() == 0 {
                        continue;
                    }
                    let pages = d.targets();
                    if pages.len() as u32 >= d.n() {
                        self.say("Can't delete every page.", true, now);
                        continue;
                    }
                    let count = pages.len();
                    d.selected.clear();
                    let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Delete { pages } });
                    self.say(if count == 1 { "Deleted 1 page.".into() } else { format!("Deleted {count} pages.") }, false, now);
                }
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = SPACE_1;
            if theme::icon_button(ui, Icon::Sidebar, "Thumbnails  Ctrl+B", true, self.sidebar && !self.docs.is_empty())
                .clicked()
            {
                actions.push(Action::ToggleSidebar);
            }
            let n = self.docs.len().max(1) as f32;
            let avail = ui.available_width() - 80.0;
            let tab_w = ((avail - SPACE_1 * n) / n).clamp(48.0, 220.0);
            for (i, d) in self.docs.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(vec2(tab_w, 30.0), Sense::click());
                let active = i == self.active;
                let hovered = resp.hovered();
                let p = ui.painter();
                let r = CornerRadius::same(RADIUS_MD);
                if active {
                    p.rect_filled(rect, r, ACCENT_DEEP);
                } else if hovered {
                    p.rect_filled(rect, r, BG_300);
                }
                let close_rect = Rect::from_center_size(pos2(rect.max.x - 16.0, rect.center().y), Vec2::splat(20.0));
                let text_rect = Rect::from_min_max(pos2(rect.min.x + SPACE_3, rect.min.y), pos2(close_rect.min.x - 2.0, rect.max.y));
                p.with_clip_rect(text_rect).text(
                    pos2(text_rect.min.x, rect.center().y),
                    Align2::LEFT_CENTER,
                    &d.title,
                    theme::body(),
                    if active || hovered { INK } else { INK_MUTED },
                );
                let close = ui.interact(close_rect, Id::unique(("tab-close", d.id)), Sense::click());
                let p = ui.painter();
                if close.hovered() {
                    p.rect_filled(close_rect, CornerRadius::same(RADIUS_SM), if active { ACCENT } else { LINE });
                    theme::paint_icon(p, close_rect.center(), Icon::Close, INK);
                } else if d.dirty {
                    p.circle_filled(close_rect.center(), 3.5, ACCENT_BRIGHT);
                } else if active || hovered {
                    theme::paint_icon(p, close_rect.center(), Icon::Close, INK_MUTED);
                }
                if close.clicked() || resp.middle_clicked() {
                    actions.push(Action::Close(i));
                } else if resp.clicked() {
                    actions.push(Action::Activate(i));
                }
                resp.on_hover_text(d.path.display().to_string());
            }
            if theme::icon_button(ui, Icon::Plus, "Open  Ctrl+O", true, false).clicked() {
                actions.push(Action::OpenDialog);
            }
            if !self.docs.is_empty() {
                let resp = theme::icon_button(ui, Icon::Clock, "Recent files", true, false);
                egui::Popup::menu(&resp).show(|ui| {
                    ui.set_min_width(260.0);
                    let item = |text: &str| egui::Button::new(RichText::new(text).font(theme::body()));
                    if ui.add_enabled(!self.closed.is_empty(), item("Reopen closed tab    Ctrl+Shift+T")).clicked() {
                        actions.push(Action::ReopenClosed);
                    }
                    ui.separator();
                    let mut any = false;
                    for r in self.settings.recent.iter().filter(|r| r.path.exists()).take(12) {
                        any = true;
                        let name = r.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        let text = if r.pinned { format!("{name}  \u{2605}") } else { name };
                        if ui.add(item(&text)).on_hover_text(r.path.display().to_string()).clicked() {
                            actions.push(Action::Open(r.path.clone()));
                        }
                    }
                    if !any {
                        ui.add_enabled(false, item("No recent files"));
                    }
                });
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let resp = theme::icon_button(ui, Icon::Gear, "Settings  Ctrl+,", true, self.settings_open);
                if matches!(self.update, Update::Available { .. }) {
                    // A new version is waiting.
                    ui.painter().circle_filled(resp.rect.right_top() + vec2(-7.0, 7.0), 3.5, ACCENT_BRIGHT);
                }
                if resp.clicked() {
                    actions.push(Action::Settings);
                }
            });
        });
    }

    fn toolbar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Self { docs, active, req, gens, pen, settings, .. } = self;
        let night = settings.night;
        let doc = docs.get_mut(*active).filter(|d| !d.loading && d.error.is_none() && d.n() > 0);
        let has = doc.is_some();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = SPACE_2;
            let sep = |ui: &mut Ui| {
                let (r, _) = ui.allocate_exact_size(vec2(SPACE_2 + 1.0, 20.0), Sense::hover());
                ui.painter().vline(r.center().x, r.y_range(), Stroke::new(1.0, LINE));
            };
            if theme::icon_button(ui, Icon::Open, "Open  Ctrl+O", true, false).clicked() {
                actions.push(Action::OpenDialog);
            }
            let dirty = doc.as_ref().is_some_and(|d| d.dirty);
            if theme::icon_button(ui, Icon::Save, "Save  Ctrl+S", has && dirty, false).clicked() && dirty {
                actions.push(Action::Save);
            }
            let Some(d) = doc else { return };
            if theme::icon_button(ui, Icon::Undo, "Undo  Ctrl+Z", d.can_undo, false).clicked() && d.can_undo {
                actions.push(Action::Undo);
            }
            if theme::icon_button(ui, Icon::Redo, "Redo  Ctrl+Y", d.can_redo, false).clicked() && d.can_redo {
                actions.push(Action::Redo);
            }
            if theme::icon_button(ui, Icon::Print, "Print  Ctrl+P", true, false).clicked() {
                actions.push(Action::Print);
            }
            sep(ui);

            // Page readout: editable current page and the page count.
            let field = ui.add(
                egui::TextEdit::singleline(&mut d.page_field)
                    .desired_width(44.0)
                    .font(theme::readout())
                    .align(Align2::RIGHT_CENTER),
            );
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Ok(p) = d.page_field.trim().parse::<u32>() {
                    actions.push(Action::GoTo(p.saturating_sub(1)));
                }
            }
            if !field.has_focus() {
                d.page_field = (d.current + 1).to_string();
            }
            ui.label(RichText::new(format!("/ {}", d.n())).font(theme::readout()).color(INK_MUTED));
            sep(ui);

            if theme::icon_button(ui, Icon::Minus, "Zoom out  Ctrl+-", d.zoom > MIN_ZOOM, false).clicked() {
                actions.push(Action::ZoomStep(-1));
            }
            let (zr, zresp) = ui.allocate_exact_size(vec2(60.0, 28.0), Sense::click());
            if zresp.hovered() {
                ui.painter().rect_filled(zr, CornerRadius::same(RADIUS_MD), BG_300);
            }
            let pct = d.zoom * 100.0;
            let text = if pct < 100.0 { format!("{pct:.0}%") } else { format!("{:.0}%", pct.round()) };
            ui.painter().text(zr.center(), Align2::CENTER_CENTER, text, theme::readout(), INK);
            egui::Popup::menu(&zresp).show(|ui| {
                ui.set_min_width(150.0);
                for z in [0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 4.0, 8.0] {
                    if ui.button(RichText::new(format!("{:.0}%", z * 100.0)).font(theme::body())).clicked() {
                        actions.push(Action::ZoomTo(z));
                    }
                }
                ui.separator();
                if ui.button(RichText::new("Fit width").font(theme::body())).clicked() {
                    actions.push(Action::Fit(Fit::Width));
                }
                if ui.button(RichText::new("Fit page").font(theme::body())).clicked() {
                    actions.push(Action::Fit(Fit::Page));
                }
            });
            if theme::icon_button(ui, Icon::Plus, "Zoom in  Ctrl++", d.zoom < MAX_ZOOM, false).clicked() {
                actions.push(Action::ZoomStep(1));
            }
            if theme::icon_button(ui, Icon::FitWidth, "Fit width  Ctrl+1", true, d.fit == Fit::Width).clicked() {
                actions.push(Action::Fit(if d.fit == Fit::Width { Fit::None } else { Fit::Width }));
            }
            if theme::icon_button(ui, Icon::FitPage, "Fit page  Ctrl+2", true, d.fit == Fit::Page).clicked() {
                actions.push(Action::Fit(if d.fit == Fit::Page { Fit::None } else { Fit::Page }));
            }
            if theme::icon_button(ui, Icon::TwoPage, "Two pages side by side  Ctrl+3", true, d.two_page).clicked() {
                actions.push(Action::TwoPage);
            }
            if theme::icon_button(ui, Icon::Moon, "Night reading  N", true, night).clicked() {
                actions.push(Action::ToggleNight);
            }
            if theme::icon_button(ui, Icon::Present, "Present  F5", true, false).clicked() {
                actions.push(Action::Present(true));
            }
            sep(ui);

            let what = match d.selected.len() {
                0 => "this page".to_string(),
                1 => "1 selected page".to_string(),
                k => format!("{k} selected pages"),
            };
            if theme::icon_button(ui, Icon::RotateCcw, &format!("Rotate {what} left  Ctrl+Shift+R"), true, false).clicked() {
                actions.push(Action::Rotate(false));
            }
            if theme::icon_button(ui, Icon::RotateCw, &format!("Rotate {what} right  Ctrl+R"), true, false).clicked() {
                actions.push(Action::Rotate(true));
            }
            if theme::icon_button(ui, Icon::Trash, &format!("Delete {what}"), d.n() > 1, false).clicked() && d.n() > 1 {
                actions.push(Action::Delete);
            }
            let has_sel = d.sel.is_some();
            if theme::icon_button(ui, Icon::Highlighter, "Highlight selected text  H", has_sel, false).clicked() && has_sel {
                actions.push(Action::Markup(Markup::Highlight));
            }
            if theme::icon_button(ui, Icon::Pen, "Draw  P", true, *pen).clicked() {
                actions.push(Action::TogglePen);
            }
            sep(ui);

            if !d.search_open {
                if theme::icon_button(ui, Icon::Search, "Find  Ctrl+F", true, false).clicked() {
                    actions.push(Action::SearchOpen);
                }
                return;
            }
            let field = ui.add(
                egui::TextEdit::singleline(&mut d.query).desired_width(180.0).hint_text("Find").font(theme::body()),
            );
            if d.focus_search {
                d.focus_search = false;
                field.request_focus();
            }
            if field.changed() {
                start_search(d, req, gens);
                if !d.query.trim().is_empty() {
                    actions.push(Action::Side(SideTab::Results));
                }
            }
            if text_toggle(ui, "Aa", "Match case", d.case).clicked() {
                d.case = !d.case;
                start_search(d, req, gens);
            }
            if text_toggle(ui, "ab", "Whole words only", d.whole).clicked() {
                d.whole = !d.whole;
                start_search(d, req, gens);
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                // Enter steps through matches and keeps the field ready for the next Enter.
                actions.push(Action::SearchStep(if ui.input(|i| i.modifiers.shift) { -1 } else { 1 }));
                d.focus_search = true;
            }
            let reading = d.search_reading.filter(|_| !d.search_done).map(|p| format!("reading page {} of {}", p + 1, d.n()));
            let status = if d.query.trim().is_empty() {
                String::new()
            } else if d.hits.is_empty() {
                match (&reading, d.search_done) {
                    (_, true) => "No results".to_string(),
                    (Some(r), false) => format!("Searching, {r}"),
                    (None, false) => "Searching".to_string(),
                }
            } else {
                let at = d.active_hit.map(|a| a + 1).unwrap_or(0);
                let more = if d.search_done { "" } else { "+" };
                match reading {
                    Some(r) => format!("{at} / {}{more}, {r}", d.hits.len()),
                    None => format!("{at} / {}{more}", d.hits.len()),
                }
            };
            ui.label(RichText::new(status).font(theme::readout()).color(INK_MUTED));
            let any = !d.hits.is_empty();
            if theme::icon_button(ui, Icon::Up, "Previous match  Shift+Enter", any, false).clicked() && any {
                actions.push(Action::SearchStep(-1));
            }
            if theme::icon_button(ui, Icon::Down, "Next match  Enter", any, false).clicked() && any {
                actions.push(Action::SearchStep(1));
            }
            if theme::icon_button(ui, Icon::Close, "Close find  Esc", true, false).clicked() {
                actions.push(Action::SearchClose);
            }
        });
    }

    fn sidebar(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let Self { docs, active, side_tab, .. } = self;
        let Some(d) = docs.get_mut(*active) else { return };
        if d.loading || d.n() == 0 {
            return;
        }
        let n = d.n();
        let width = ui.available_width();

        // Pages, plus outline and search results when there are any.
        let mut tabs = vec![(SideTab::Pages, "Pages")];
        if !d.outline.is_empty() {
            tabs.push((SideTab::Outline, "Outline"));
        }
        if d.search_open && !d.query.trim().is_empty() {
            tabs.push((SideTab::Results, "Results"));
        }
        let tab = if tabs.iter().any(|t| t.0 == *side_tab) { *side_tab } else { SideTab::Pages };
        if tabs.len() > 1 {
            ui.add_space(SPACE_2);
            ui.horizontal(|ui| {
                ui.add_space(SPACE_2);
                ui.spacing_mut().item_spacing.x = SPACE_1;
                let w = (width - SPACE_2 * 2.0 - SPACE_1 * (tabs.len() - 1) as f32) / tabs.len() as f32;
                for (value, text) in &tabs {
                    let on = tab == *value;
                    let (rect, resp) = ui.allocate_exact_size(vec2(w, 26.0), Sense::click());
                    if on {
                        ui.painter().rect_filled(rect, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(rect, CornerRadius::same(RADIUS_MD), BG_300);
                    }
                    let color = if on || resp.hovered() { INK } else { INK_MUTED };
                    ui.painter().text(rect.center(), Align2::CENTER_CENTER, *text, theme::label(), color);
                    if resp.clicked() {
                        *side_tab = *value;
                    }
                }
            });
            ui.add_space(SPACE_1);
        }
        if tab == SideTab::Results {
            let status = if d.hits.is_empty() && d.search_done {
                "No results".to_string()
            } else if let Some(p) = d.search_reading.filter(|_| !d.search_done) {
                format!("{} so far, reading page {}", d.hits.len(), p + 1)
            } else if d.search_done {
                if d.hits.len() == 1 { "1 result".to_string() } else { format!("{} results", d.hits.len()) }
            } else {
                format!("{} so far", d.hits.len())
            };
            ui.horizontal(|ui| {
                ui.add_space(SPACE_3);
                ui.label(RichText::new(status).font(theme::caption()).color(INK_MUTED));
            });
            let active_hit = d.active_hit;
            egui::ScrollArea::vertical().id_salt(("results", d.id)).auto_shrink(false).show_rows(
                ui,
                60.0,
                d.hits.len(),
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        let h = &d.hits[i];
                        let (rect, resp) = ui.allocate_exact_size(vec2(width, 60.0), Sense::click());
                        let row = rect.shrink2(vec2(SPACE_1, 2.0));
                        let on = active_hit == Some(i);
                        if on {
                            ui.painter().rect_filled(row, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(row, CornerRadius::same(RADIUS_MD), BG_300);
                        }
                        let inner = row.shrink2(vec2(SPACE_2, SPACE_1));
                        ui.painter().text(
                            inner.min,
                            Align2::LEFT_TOP,
                            format!("Page {}", h.page + 1),
                            theme::caption(),
                            if on { INK } else { INK_MUTED },
                        );
                        let galley = ui.painter().layout_job(snippet_job(h, inner.width()));
                        ui.painter().galley(inner.min + vec2(0.0, 16.0), galley, INK);
                        if resp.clicked() {
                            actions.push(Action::ShowHit(i));
                        }
                    }
                },
            );
            return;
        }
        if tab == SideTab::Outline {
            let current = d.current;
            // The entry being read: the last one that starts at or before the current page.
            let reading = d.outline.iter().rposition(|o| o.page.is_some_and(|p| p <= current));
            egui::ScrollArea::vertical().id_salt(("outline", d.id)).auto_shrink(false).show_rows(
                ui,
                26.0,
                d.outline.len(),
                |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for i in range {
                        let item = &d.outline[i];
                        let (rect, resp) = ui.allocate_exact_size(vec2(width, 26.0), Sense::click());
                        let row = rect.shrink2(vec2(SPACE_1, 1.0));
                        if reading == Some(i) {
                            ui.painter().rect_filled(row, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(row, CornerRadius::same(RADIUS_MD), BG_300);
                        }
                        let x = row.min.x + SPACE_2 + item.depth as f32 * 12.0;
                        let color = if reading == Some(i) || resp.hovered() { INK } else { INK_MUTED };
                        ui.painter().with_clip_rect(row.shrink2(vec2(SPACE_1, 0.0))).text(
                            pos2(x, row.center().y),
                            Align2::LEFT_CENTER,
                            &item.title,
                            theme::body(),
                            color,
                        );
                        if resp.clicked() {
                            if let Some(page) = item.page {
                                actions.push(Action::GoTo(page));
                            }
                        }
                        resp.on_hover_text(&item.title);
                    }
                },
            );
            return;
        }

        let total = d.side_prefix[n as usize];
        let mut area = egui::ScrollArea::vertical().id_salt(("thumbs", d.id)).auto_shrink(false);
        if let Some(p) = d.side_scroll_to.take() {
            let top = d.side_prefix[p as usize];
            let bottom = d.side_prefix[p as usize + 1];
            if top < d.side_view.0 || bottom > d.side_view.1 {
                let h = d.side_view.1 - d.side_view.0;
                area = area.vertical_scroll_offset((top - (h - (bottom - top)) / 2.0).max(0.0));
            }
        }
        let (modifiers, ppp) = (ui.input(|i| i.modifiers), ui.ctx().pixels_per_point());
        let (pointer, down) = ui.input(|i| (i.pointer.latest_pos(), i.pointer.primary_down()));
        area.show_viewport(ui, |ui, vp| {
            ui.set_height(total);
            ui.set_width(width);
            d.side_view = (vp.min.y, vp.max.y);
            let origin = ui.min_rect().min;
            let first = d.side_prefix.partition_point(|&y| y <= vp.min.y).saturating_sub(1) as u32;
            let mut i = first;
            while i < n && d.side_prefix[i as usize] < vp.max.y {
                let h = DocView::side_h(d.sizes[i as usize]);
                let min = origin + vec2((width - SIDE_CELL) / 2.0, d.side_prefix[i as usize]);
                let min = pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp);
                let cell = Rect::from_min_size(min, vec2(SIDE_CELL, h));
                let hit = Rect::from_min_max(cell.min - Vec2::splat(4.0), pos2(cell.max.x + 4.0, cell.max.y + SIDE_LABEL));
                let resp = ui.interact(hit, Id::unique(("thumb", d.id, i)), Sense::click_and_drag());
                let p = ui.painter();
                let selected = d.selected.contains(&i);
                let current = d.current == i;
                let dragged = d.drag_pages.as_ref().is_some_and(|l| l.contains(&i));
                if selected {
                    p.rect_filled(hit, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
                } else if resp.hovered() {
                    p.rect_filled(hit, CornerRadius::same(RADIUS_MD), BG_300);
                }
                match d.thumbs.get(&i) {
                    Some(t) => {
                        let tint = if dragged { Color32::from_gray(110) } else { Color32::WHITE };
                        p.image(t.id(), cell, UV, tint);
                    }
                    None => {
                        p.rect_filled(cell, CornerRadius::same(RADIUS_SM), BG_300);
                    }
                }
                if current || selected {
                    p.rect_stroke(
                        cell,
                        CornerRadius::same(RADIUS_SM),
                        Stroke::new(2.0, ACCENT_BRIGHT),
                        StrokeKind::Outside,
                    );
                }
                p.text(
                    pos2(cell.center().x, cell.max.y + SIDE_LABEL / 2.0 + 1.0),
                    Align2::CENTER_CENTER,
                    (i + 1).to_string(),
                    theme::readout(),
                    if current || selected { INK } else { INK_MUTED },
                );
                if resp.clicked() {
                    if modifiers.command {
                        if !d.selected.remove(&i) {
                            d.selected.insert(i);
                        }
                        d.select_anchor = i;
                    } else if modifiers.shift {
                        let (a, b) = (d.select_anchor.min(i), d.select_anchor.max(i));
                        d.selected = (a..=b).collect();
                    } else {
                        d.selected.clear();
                        d.selected.insert(i);
                        d.select_anchor = i;
                        actions.push(Action::GoTo(i));
                    }
                }
                // Drag a thumbnail (or the whole selection) to move pages.
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    d.drag_pages =
                        Some(if selected { d.selected.iter().copied().collect() } else { vec![i] });
                }
                // Page actions on right-click act on the selection, or on this page.
                if resp.secondary_clicked() && !selected {
                    d.selected.clear();
                    d.selected.insert(i);
                    d.select_anchor = i;
                }
                let count = d.selected.len().max(1);
                egui::Popup::context_menu(&resp).show(|ui| {
                    ui.set_min_width(200.0);
                    let item = |text: &str| egui::Button::new(RichText::new(text).font(theme::body()));
                    if ui.add(item("Rotate left")).clicked() {
                        actions.push(Action::Rotate(false));
                    }
                    if ui.add(item("Rotate right")).clicked() {
                        actions.push(Action::Rotate(true));
                    }
                    if ui.add_enabled(n > 1, item(if count == 1 { "Delete page" } else { "Delete pages" })).clicked() {
                        actions.push(Action::Delete);
                    }
                    ui.separator();
                    let extract = if count == 1 { "Extract page".to_string() } else { format!("Extract {count} pages") };
                    if ui.add(item(&extract)).clicked() {
                        actions.push(Action::Extract);
                    }
                    if ui.add(item("Insert PDF before")).clicked() {
                        actions.push(Action::InsertPdf(i));
                    }
                    if ui.add(item("Insert PDF after")).clicked() {
                        actions.push(Action::InsertPdf(i + 1));
                    }
                });
                d.thumb_wanted.push(i);
                i += 1;
            }

            // While dragging: show where the pages would land, and drop them on release.
            if d.drag_pages.is_some() {
                if let Some(pos) = pointer {
                    let y = pos.y - origin.y;
                    let before = (0..n as usize)
                        .take_while(|&k| (d.side_prefix[k] + d.side_prefix[k + 1]) / 2.0 < y)
                        .count() as u32;
                    d.drop_before = Some(before);
                    let line_y = origin.y + d.side_prefix[before as usize] - SPACE_1;
                    ui.painter().hline(
                        (origin.x + SPACE_3)..=(origin.x + width - SPACE_3),
                        line_y,
                        Stroke::new(2.0, ACCENT_BRIGHT),
                    );
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                }
                if !down {
                    if let (Some(pages), Some(before)) = (d.drag_pages.take(), d.drop_before.take()) {
                        actions.push(Action::MovePages(pages, before));
                    }
                }
            }
        });
    }

    fn empty_state(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        if let Some(e) = &self.fatal {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, e, theme::body(), DANGER);
            return;
        }
        // Pinned files first, then the rest, newest first; only files that still exist.
        let mut recent: Vec<settings::Recent> = self.settings.recent.iter().filter(|r| r.path.exists()).cloned().collect();
        recent.sort_by_key(|r| !r.pinned);
        let gap = SPACE_6;
        let cols = (((rect.width() - 2.0 * SPACE_6 + gap) / (CARD_W + gap)).floor() as usize).clamp(1, 6);
        let rows = if rect.height() > 760.0 { 2 } else { 1 };
        recent.truncate(cols * rows);
        let grid_rows = recent.len().div_ceil(cols.max(1));
        let header_h = 150.0;
        let grid_h = if recent.is_empty() { 0.0 } else { 40.0 + grid_rows as f32 * (CARD_H + gap) - gap };
        let top = rect.min.y + ((rect.height() - header_h - grid_h) / 2.0).max(SPACE_6);
        let cx = rect.center().x;

        let p = ui.painter();
        p.text(pos2(cx, top + 20.0), Align2::CENTER_CENTER, "Drop a PDF anywhere", theme::display(), INK);
        p.text(pos2(cx, top + 54.0), Align2::CENTER_CENTER, "or open one with Ctrl+O", theme::body(), INK_MUTED);
        let button = Rect::from_center_size(pos2(cx, top + 100.0), vec2(96.0, 32.0));
        let resp = ui.interact(button, Id::unique("empty-open"), Sense::click());
        let p = ui.painter();
        p.rect_filled(button, CornerRadius::same(RADIUS_MD), ACCENT);
        if resp.hovered() {
            p.rect_stroke(button, CornerRadius::same(RADIUS_MD), Stroke::new(2.0, ACCENT_BRIGHT), StrokeKind::Outside);
        }
        p.text(button.center(), Align2::CENTER_CENTER, "Open", theme::label(), ON_ACCENT);
        if resp.clicked() {
            actions.push(Action::OpenDialog);
        }
        if recent.is_empty() {
            return;
        }

        // Recent files as cards: first page, name, and where you were.
        let row_w = cols.min(recent.len()) as f32 * (CARD_W + gap) - gap;
        let left = cx - row_w / 2.0;
        let label_y = top + header_h;
        ui.painter().text(pos2(left, label_y), Align2::LEFT_CENTER, "Recent", theme::label(), INK_MUTED);
        let clear = Rect::from_min_size(pos2(left + row_w - 80.0, label_y - 12.0), vec2(80.0, 24.0));
        if recent.iter().any(|r| !r.pinned) {
            let cresp = ui.interact(clear, Id::unique("recent-clear"), Sense::click());
            let color = if cresp.hovered() { INK } else { INK_MUTED };
            ui.painter().text(clear.right_center(), Align2::RIGHT_CENTER, "Clear list", theme::caption(), color);
            if cresp.on_hover_text("Forget recent files that aren't pinned").clicked() {
                actions.push(Action::ClearRecent);
            }
        }
        let ppp = ui.ctx().pixels_per_point();
        for (k, r) in recent.iter().enumerate() {
            let (col, row) = ((k % cols) as f32, (k / cols) as f32);
            let card = Rect::from_min_size(
                pos2(left + col * (CARD_W + gap), label_y + 24.0 + row * (CARD_H + gap)),
                vec2(CARD_W, CARD_H),
            );
            let resp = ui.interact(card.expand(SPACE_2), Id::unique(("recent", &r.path)), Sense::click());
            let hovered = resp.hovered();
            let p = ui.painter();
            if hovered {
                p.rect_filled(card.expand(SPACE_2), CornerRadius::same(RADIUS_LG), BG_300);
            }
            let slot = Rect::from_min_size(card.min, vec2(CARD_W, COVER_H));
            match self.covers.get(&r.path) {
                Some((Some(tex), _)) => {
                    // The page, as large as fits, standing on the slot's bottom edge.
                    let [tw, th] = tex.size();
                    let k = (CARD_W / tw as f32).min(COVER_H / th as f32);
                    let size = vec2(tw as f32, th as f32) * k;
                    let img = Rect::from_min_size(pos2(slot.center().x - size.x / 2.0, slot.max.y - size.y), size);
                    p.image(tex.id(), img, UV, Color32::WHITE);
                    p.rect_stroke(img, 0, Stroke::new(1.0, LINE), StrokeKind::Outside);
                    // How far through the document you got.
                    let pages = r.pages.max(self.covers.get(&r.path).map(|c| c.1).unwrap_or(0));
                    if pages > 1 && r.page > 0 {
                        let frac = (r.page + 1) as f32 / pages as f32;
                        let bar = Rect::from_min_max(pos2(img.min.x, img.max.y - 3.0), img.max);
                        p.rect_filled(bar, 0, Color32::from_black_alpha(140));
                        p.rect_filled(Rect::from_min_max(bar.min, pos2(bar.min.x + bar.width() * frac, bar.max.y)), 0, ACCENT);
                    }
                }
                Some((None, _)) => {
                    // Locked or unreadable: a plain card.
                    p.rect_filled(slot.shrink2(vec2(18.0, 0.0)), CornerRadius::same(RADIUS_SM), BG_300);
                    p.text(slot.center(), Align2::CENTER_CENTER, "PDF", theme::label(), INK_MUTED);
                }
                None => {
                    p.rect_filled(slot.shrink2(vec2(18.0, 0.0)), CornerRadius::same(RADIUS_SM), BG_200);
                    if self.cover_pending.insert(r.path.clone()) {
                        let width = ((CARD_W * ppp).round() as u32).max(96);
                        let _ = self.req.send(Req::Cover { path: r.path.clone(), width });
                    }
                }
            }
            let name = r.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let galley = elided(ui, &name, theme::body(), if hovered { INK } else { INK }, CARD_W);
            ui.painter().galley(pos2(card.min.x, slot.max.y + SPACE_2), galley, INK);
            let pages = r.pages.max(self.covers.get(&r.path).map(|c| c.1).unwrap_or(0));
            let place = if r.page > 0 && pages > 0 {
                format!("Page {} of {}", r.page + 1, pages)
            } else if pages > 0 {
                if pages == 1 { "1 page".to_string() } else { format!("{pages} pages") }
            } else {
                String::new()
            };
            let meta = match (place.is_empty(), r.opened) {
                (true, 0) => String::new(),
                (true, t) => ago(t),
                (false, 0) => place,
                (false, t) => format!("{place} \u{b7} {}", ago(t)),
            };
            let galley = elided(ui, &meta, theme::caption(), INK_MUTED, CARD_W);
            ui.painter().galley(pos2(card.min.x, slot.max.y + SPACE_2 + 22.0), galley, INK_MUTED);

            // Pin button on the cover: always shown for pinned files, on hover for the rest.
            let pin = Rect::from_center_size(pos2(slot.max.x - 16.0, slot.min.y + 16.0), Vec2::splat(26.0));
            let pin_resp = ui.interact(pin, Id::unique(("pin", &r.path)), Sense::click());
            if r.pinned || hovered || pin_resp.hovered() {
                let p = ui.painter();
                p.circle_filled(pin.center(), 13.0, Color32::from_rgba_unmultiplied(11, 10, 15, 220));
                let (icon, color) = if r.pinned { (Icon::StarFilled, ACCENT_BRIGHT) } else { (Icon::Star, if pin_resp.hovered() { INK } else { INK_MUTED }) };
                theme::paint_icon(p, pin.center(), icon, color);
            }
            if pin_resp.on_hover_text(if r.pinned { "Unpin" } else { "Pin to the start screen" }).clicked() {
                actions.push(Action::PinRecent(r.path.clone()));
            } else if resp.clicked() {
                actions.push(Action::Open(r.path.clone()));
            }
            let path = r.path.clone();
            let pinned = r.pinned;
            egui::Popup::context_menu(&resp).show(|ui| {
                ui.set_min_width(200.0);
                let item = |text: &str| egui::Button::new(RichText::new(text).font(theme::body()));
                if ui.add(item("Open")).clicked() {
                    actions.push(Action::Open(path.clone()));
                }
                if ui.add(item(if pinned { "Unpin" } else { "Pin to the start screen" })).clicked() {
                    actions.push(Action::PinRecent(path.clone()));
                }
                if ui.add(item("Show in folder")).clicked() {
                    actions.push(Action::ShowInFolder(path.clone()));
                }
                ui.separator();
                if ui.add(item("Remove from this list")).clicked() {
                    actions.push(Action::ForgetRecent(path.clone()));
                }
            });
            resp.on_hover_text(r.path.display().to_string());
        }
    }

    fn viewer(&mut self, ui: &mut Ui, actions: &mut Vec<Action>) {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        ui.painter_at(rect).rect_filled(rect, 0, BG_000);
        if self.docs.is_empty() {
            self.empty_state(ui, rect, actions);
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let now = ui.input(|i| i.time);
        let frame = self.frame;
        let Self { docs, req, gens, active, settings, pen, present, wheel, .. } = self;
        let pen = *pen;
        let present = present.is_some();
        let paper = if settings.night { Color32::from_rgb(22, 22, 22) } else { PAGE };
        let Some(d) = docs.get_mut(*active) else { return };
        let painter = ui.painter_at(rect);
        if let Some(e) = &d.error {
            painter.text(
                rect.center() - vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                "Can't open this file.",
                theme::title(),
                INK,
            );
            painter.text(rect.center() + vec2(0.0, 16.0), Align2::CENTER_CENTER, e, theme::body(), INK_MUTED);
            return;
        }
        if d.loading || d.n() == 0 {
            return;
        }
        let n = d.n();
        d.view = rect.size();
        if d.needs_fit {
            d.needs_fit = false;
            if let Some((page, zoom)) = d.restore.take() {
                d.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                d.go_to(page);
            } else {
                match settings.open_zoom {
                    OpenZoom::FitWidth => d.zoom = d.fit_zoom(Fit::Width).clamp(MIN_ZOOM, 1.5),
                    OpenZoom::FitPage => d.fit = Fit::Page,
                    OpenZoom::Actual => d.zoom = 1.0,
                }
            }
        }
        if d.fit != Fit::None {
            let z = d.fit_zoom(d.fit);
            if (z - d.zoom).abs() > 1e-4 {
                d.set_zoom(z, vec2(d.view.x / 2.0, 0.0), now);
            }
        }

        // Input: wheel scrolls, Ctrl+wheel and pinch zoom around the pointer, left drag selects
        // text, middle or right drag pans.
        if present {
            // The page (or pair) being shown, centred.
            if resp.contains_pointer() {
                *wheel += ui.input(|i| i.smooth_scroll_delta.y);
                if wheel.abs() > 60.0 {
                    actions.push(Action::PresentStep(if *wheel < 0.0 { 1 } else { -1 }));
                    *wheel = 0.0;
                }
            }
            let row = d.row_of[d.current as usize] as usize;
            let h = (d.row_top[row + 1] - d.row_top[row]) * d.zoom * PT;
            d.scroll.y = d.top(d.current, d.zoom) - (d.view.y - h) / 2.0;
            d.scroll.x = (d.content_w(d.zoom) - d.view.x) / 2.0;
            d.clamp_scroll();
        } else if resp.contains_pointer() {
            let (scroll, zoom_delta, pos) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
            if zoom_delta != 1.0 {
                d.fit = Fit::None;
                let anchor = pos.map(|p| p - rect.min).unwrap_or(d.view / 2.0);
                d.set_zoom(d.zoom * zoom_delta, anchor, now);
            } else if scroll != Vec2::ZERO {
                d.scroll -= scroll;
            }
        }
        let panning =
            resp.dragged_by(egui::PointerButton::Middle) || resp.dragged_by(egui::PointerButton::Secondary);
        if panning {
            d.scroll -= resp.drag_delta();
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
        d.clamp_scroll();

        // Scrollbar.
        let total = d.total_h();
        let bar = (total > d.view.y + 1.0 && !present).then(|| {
            let track = Rect::from_min_max(pos2(rect.max.x - 12.0, rect.min.y), rect.max);
            let sresp = ui.interact(track, Id::unique(("vscroll", d.id)), Sense::click_and_drag());
            let thumb_h = (d.view.y * d.view.y / total).max(36.0);
            let max_scroll = total - d.view.y;
            if let Some(p) = sresp.interact_pointer_pos() {
                let t = ((p.y - rect.min.y - thumb_h / 2.0) / (d.view.y - thumb_h)).clamp(0.0, 1.0);
                d.scroll.y = t * max_scroll;
            }
            let y = rect.min.y + (d.view.y - thumb_h) * (d.scroll.y / max_scroll);
            let wide = sresp.hovered() || sresp.dragged();
            let w = if wide { 8.0 } else { 4.0 };
            Rect::from_min_size(pos2(rect.max.x - w - 2.0, y), vec2(w, thumb_h))
        });

        let current = if present { d.current } else { d.page_at(d.scroll.y + d.view.y * 0.35) };
        if current != d.current {
            d.current = current;
            d.side_scroll_to = Some(current);
        }

        // Settle: re-render at the new scale only once zooming has paused.
        let target = d.zoom * PT * ppp;
        if d.render_scale != target {
            let since = now - d.zoom_changed;
            // During a long continuous zoom, refresh a few times a second anyway so the view
            // never stays soft for the whole gesture.
            if d.render_scale == 0.0 || since >= SETTLE || now - d.scale_set >= REFRESH {
                d.render_scale = target;
                d.scale_set = now;
                gens.tile.fetch_add(1, Ordering::Relaxed);
                d.pending.clear();
            } else {
                ui.ctx().request_repaint_after(Duration::from_secs_f64(SETTLE - since + 0.002));
            }
        }
        let scale = d.render_scale;
        let k_cur = d.zoom * PT;

        let mut wanted: Vec<(TileKey, f32)> = Vec::new();
        let mut vis: Vec<(u32, Rect)> = Vec::new();
        let mut complete = d.render_scale == target;
        let view_center = rect.center();
        let mut i = d.page_at((d.scroll.y - PREFETCH).max(0.0));
        while i < n && d.top(i, d.zoom) < d.scroll.y + d.view.y + PREFETCH {
            let size = d.page_size(i);
            let x = rect.min.x + d.page_x(i, d.zoom) - d.scroll.x;
            let y = rect.min.y + d.top(i, d.zoom) - d.scroll.y;
            let min = pos2((x * ppp).round() / ppp, (y * ppp).round() / ppp);
            let prect = Rect::from_min_size(min, size);
            let on_screen = prect.intersects(rect);
            if on_screen {
                d.thumb_wanted.push(i);
                vis.push((i, prect));
            }

            // Which tiles at the settled scale does this page need?
            let s = d.sizes[i as usize];
            let pw = (s[0] * scale).round().max(1.0);
            let ph = (s[1] * scale).round().max(1.0);
            let needs_tiles = pw > THUMB_W as f32 || !d.thumbs.contains_key(&i);
            let mut page_complete = true;
            let reach = rect.expand2(vec2(0.0, PREFETCH)).intersect(prect);
            if needs_tiles && reach.is_positive() {
                let to_px = scale / k_cur;
                let x0 = ((reach.min.x - prect.min.x) * to_px).max(0.0);
                let y0 = ((reach.min.y - prect.min.y) * to_px).max(0.0);
                let x1 = ((reach.max.x - prect.min.x) * to_px).min(pw - 1.0);
                let y1 = ((reach.max.y - prect.min.y) * to_px).min(ph - 1.0);
                let t = TILE as f32;
                for ty in (y0 / t) as u32..=(y1 / t) as u32 {
                    for tx in (x0 / t) as u32..=(x1 / t) as u32 {
                        if !d.has_tile(i, scale, tx, ty) {
                            let c = prect.min + vec2((tx as f32 + 0.5) * t, (ty as f32 + 0.5) * t) / to_px;
                            wanted.push(((i, scale.to_bits(), tx, ty), (c - view_center).length()));
                            let trect = Rect::from_min_size(
                                prect.min + vec2(tx as f32 * t, ty as f32 * t) / to_px,
                                Vec2::splat(t / to_px),
                            );
                            page_complete = false;
                            if trect.intersects(rect) {
                                complete = false;
                            }
                        }
                    }
                }
            }

            if on_screen {
                painter.rect_filled(prect, 0, paper);
                if let Some(t) = d.thumbs.get(&i) {
                    painter.image(t.id(), prect, UV, Color32::WHITE);
                }
            }
            if let Some(list) = d.tiles.get_mut(&i) {
                if page_complete && d.render_scale == target && list.iter().any(|t| t.scale != scale) {
                    // The sharp tiles cover everything needed: older scales can go.
                    let mut freed = 0;
                    list.retain(|t| {
                        let keep = t.scale == scale;
                        if !keep {
                            freed += t.bytes;
                        }
                        keep
                    });
                    d.tile_bytes -= freed;
                }
                if on_screen {
                    // Coarser tiles first, the settled scale last so it ends up on top.
                    list.sort_by(|a, b| (a.scale == scale, a.scale).partial_cmp(&(b.scale == scale, b.scale)).unwrap());
                    for t in list.iter_mut() {
                        let k = k_cur / t.scale;
                        let [tw, th] = t.tex.size();
                        let trect = Rect::from_min_size(
                            prect.min + vec2((t.tx * TILE) as f32, (t.ty * TILE) as f32) * k,
                            vec2(tw as f32, th as f32) * k,
                        );
                        if trect.intersects(rect) {
                            painter.image(t.tex.id(), trect, UV, Color32::WHITE);
                            t.used = frame;
                        }
                    }
                }
            }
            i += 1;
        }
        d.complete = complete;

        // Text layer: character boxes for the pages on screen, fetched once and kept nearby.
        for &(page, _) in &vis {
            if !d.texts.contains_key(&page) && d.text_pending.insert(page) {
                let _ = req.send(Req::Text { id: d.id, page });
            }
        }
        if d.texts.len() > 160 {
            let (cur, keep) = (d.current, d.sel);
            d.texts.retain(|p, _| p.abs_diff(cur) <= 40 || keep.is_some_and(|(a, b)| *p == a.0 || *p == b.0));
        }
        let locate = |d: &DocView, pos: egui::Pos2, snap: bool| -> Option<(u32, u32)> {
            let off = |r: &Rect| (r.min.y - pos.y).max(pos.y - r.max.y).max(0.0);
            let (page, prect) = vis.iter().copied().min_by(|a, b| off(&a.1).total_cmp(&off(&b.1)))?;
            let text = d.texts.get(&page)?;
            char_at(text, (pos - prect.min) / k_cur, snap).map(|c| (page, c))
        };
        let pointer = resp.interact_pointer_pos().or_else(|| ui.input(|i| i.pointer.hover_pos()));
        // What is under the pointer: page position, and any link or note there.
        let here = pointer.and_then(|pos| {
            vis.iter().copied().find(|(_, r)| r.contains(pos)).map(|(page, r)| (page, (pos - r.min) / k_cur))
        });
        let inside = |r: &[f32; 4], at: Vec2| at.x >= r[0] && at.x <= r[2] && at.y >= r[1] && at.y <= r[3];
        let link = here.and_then(|(page, at)| {
            d.texts.get(&page)?.links.iter().find(|(r, _)| inside(r, at)).map(|(_, l)| l.clone())
        });
        let note = here.and_then(|(page, at)| {
            d.texts.get(&page)?.notes.iter().find(|(r, _)| inside(r, at)).map(|(_, t)| t.clone())
        });
        if resp.hovered() && !panning {
            if pen {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            } else if link.is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            } else if pointer.is_some_and(|p| locate(d, p, false).is_some()) {
                ui.ctx().set_cursor_icon(CursorIcon::Text);
            }
        }
        if resp.clicked() || resp.drag_started() {
            // Clicking the document takes the keyboard back from any text field.
            ui.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
        }
        if present {
            // Click for the next page, right-click for the one before; links still work.
            if resp.secondary_clicked() {
                actions.push(Action::PresentStep(-1));
            } else if resp.clicked() && link.is_none() {
                actions.push(Action::PresentStep(1));
            }
        }
        if resp.clicked() {
            d.sel = None;
            match &link {
                Some(Link::Page(p)) if !pen => d.go_to(*p),
                // Only ordinary web and mail links are opened; anything else is ignored.
                Some(Link::Uri(u)) if !pen => {
                    let lower = u.to_lowercase();
                    if lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:") {
                        crate::settings::open_url(u);
                    }
                }
                _ => {}
            }
        }

        // Right-click menu. The click position is kept so "Remove highlight" knows its target.
        if resp.secondary_clicked() {
            d.menu_at = pointer.and_then(|pos| {
                let (page, prect) = vis.iter().copied().find(|(_, r)| r.contains(pos))?;
                Some((page, (pos - prect.min) / k_cur))
            });
        }
        let over_highlight = d.menu_at.filter(|(page, at)| {
            d.texts.get(page).is_some_and(|t| {
                t.marks.iter().any(|h| at.x >= h[0] && at.x <= h[2] && at.y >= h[1] && at.y <= h[3])
            })
        });
        let has_sel = d.sel.is_some();
        let menu_at = d.menu_at;
        let menu = if present { None } else { Some(&resp) };
        menu.map(|resp| egui::Popup::context_menu(resp).show(|ui| {
            ui.set_min_width(210.0);
            let item = |text: &str| egui::Button::new(RichText::new(text).font(theme::body()));
            if ui.add_enabled(has_sel, item("Copy    Ctrl+C")).clicked() {
                actions.push(Action::Copy);
            }
            ui.separator();
            if ui.add_enabled(has_sel, item("Highlight    H")).clicked() {
                actions.push(Action::Markup(Markup::Highlight));
            }
            if ui.add_enabled(has_sel, item("Underline    U")).clicked() {
                actions.push(Action::Markup(Markup::Underline));
            }
            if ui.add_enabled(has_sel, item("Strike out    S")).clicked() {
                actions.push(Action::Markup(Markup::Strikeout));
            }
            if ui.add_enabled(menu_at.is_some(), item("Add note here")).clicked() {
                if let Some((page, at)) = menu_at {
                    actions.push(Action::NoteAt(page, at));
                }
            }
            ui.separator();
            if ui.add_enabled(over_highlight.is_some(), item("Remove annotation")).clicked() {
                if let Some((page, at)) = over_highlight {
                    actions.push(Action::Unhighlight(page, at));
                }
            }
        }));
        if resp.double_clicked() {
            if let Some((page, c)) = pointer.and_then(|p| locate(d, p, false)) {
                let (a, b) = word_at(&d.texts[&page], c);
                d.sel = Some(((page, a), (page, b)));
            }
        }
        if resp.drag_started_by(egui::PointerButton::Primary) && !present {
            let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
            if pen {
                // Start a stroke on the page under the pointer.
                d.sel = None;
                d.stroke = origin.and_then(|pos| {
                    let (page, r) = vis.iter().copied().find(|(_, r)| r.contains(pos))?;
                    let at = (pos - r.min) / k_cur;
                    Some((page, vec![[at.x, at.y]]))
                });
            } else {
                let hit = origin.and_then(|p| locate(d, p, true));
                d.sel = hit.map(|h| (h, h));
                d.selecting = hit.is_some();
            }
        }
        if let (Some((page, points)), Some(pos), true) = (&mut d.stroke, pointer, resp.dragged_by(egui::PointerButton::Primary)) {
            if let Some((_, r)) = vis.iter().find(|(p, _)| p == page) {
                let at = (pos.clamp(r.min, r.max) - r.min) / k_cur;
                let far = points.last().is_none_or(|l| (l[0] - at.x).abs() + (l[1] - at.y).abs() > 0.75);
                if far {
                    points.push([at.x, at.y]);
                }
            }
        }
        if d.selecting && resp.dragged_by(egui::PointerButton::Primary) {
            if let Some(pos) = pointer {
                if let (Some(hit), Some(sel)) = (locate(d, pos, true), d.sel) {
                    d.sel = Some((sel.0, hit));
                }
                // Dragging past the top or bottom edge scrolls the selection along.
                let edge = 28.0;
                if pos.y < rect.min.y + edge {
                    d.scroll.y -= 20.0;
                    ui.ctx().request_repaint();
                } else if pos.y > rect.max.y - edge {
                    d.scroll.y += 20.0;
                    ui.ctx().request_repaint();
                }
                d.clamp_scroll();
            }
        }
        if resp.drag_stopped() {
            d.selecting = false;
            if let Some((page, points)) = d.stroke.take() {
                if points.len() > 1 {
                    let edit = Edit::Ink { page, points, color: [124, 58, 237], width: 2.0 };
                    let _ = req.send(Req::Edit { id: d.id, edit });
                }
            }
        }
        if let Some((page, points)) = &d.stroke {
            // The stroke in progress, drawn where the saved one will land.
            if let Some((_, r)) = vis.iter().find(|(p, _)| p == page) {
                let line: Vec<egui::Pos2> = points.iter().map(|p| r.min + vec2(p[0], p[1]) * k_cur).collect();
                painter.line(line, Stroke::new((2.0 * k_cur).max(1.0), ACCENT));
            }
        }

        let on_page = |prect: Rect, r: &[f32; 4]| {
            Rect::from_min_max(prect.min + vec2(r[0], r[1]) * k_cur, prect.min + vec2(r[2], r[3]) * k_cur)
        };
        if let Some((lo, hi)) = d.sel_range() {
            let fill = Color32::from_rgba_unmultiplied(124, 58, 237, 84);
            for &(page, prect) in &vis {
                let Some(text) = d.texts.get(&page) else { continue };
                if page < lo.0 || page > hi.0 || text.boxes.is_empty() {
                    continue;
                }
                let a = if page == lo.0 { lo.1 as usize } else { 0 };
                let b = (if page == hi.0 { hi.1 as usize } else { usize::MAX }).min(text.boxes.len() - 1);
                if a > b {
                    continue;
                }
                for r in line_rects(&text.boxes[a..=b]) {
                    painter.rect_filled(on_page(prect, &r), 0, fill);
                }
            }
        }
        if d.search_open && !d.hits.is_empty() {
            for &(page, prect) in &vis {
                let start = d.hits.partition_point(|h| h.page < page);
                for (j, h) in d.hits[start..].iter().enumerate().take_while(|(_, h)| h.page == page) {
                    let active = d.active_hit == Some(start + j);
                    for r in &h.rects {
                        let sr = on_page(prect, r);
                        if active {
                            painter.rect_filled(sr, 2, Color32::from_rgba_unmultiplied(124, 58, 237, 120));
                            painter.rect_stroke(sr, 2, Stroke::new(2.0, ACCENT), StrokeKind::Outside);
                        } else {
                            painter.rect_filled(sr, 2, Color32::from_rgba_unmultiplied(167, 139, 250, 84));
                        }
                    }
                }
            }
        }

        // A note's text, or a link's address, shown beside the pointer.
        let tip = note.or(match &link {
            Some(Link::Uri(u)) => Some(u.clone()),
            Some(Link::Page(p)) => Some(format!("Go to page {}", p + 1)),
            None => None,
        });
        if let (Some(text), Some(pos), true) = (tip, pointer, resp.hovered() && !resp.dragged() && !pen) {
            let tip_painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, Id::unique("page-tip")));
            let galley = tip_painter.layout(text, theme::body(), INK, 320.0);
            let size = galley.size() + vec2(SPACE_3 * 2.0, SPACE_2 * 2.0);
            let mut min = pos + vec2(14.0, 18.0);
            min.x = min.x.min(rect.max.x - size.x - 4.0);
            min.y = min.y.min(rect.max.y - size.y - 4.0);
            let tip_rect = Rect::from_min_size(min, size);
            tip_painter.rect(tip_rect, CornerRadius::same(RADIUS_MD), BG_300, Stroke::new(1.0, LINE_STRONG), StrokeKind::Inside);
            tip_painter.galley(tip_rect.min + vec2(SPACE_3, SPACE_2), galley, INK);
        }

        // Request what is missing, nearest the centre first. Anything queued that is no longer
        // wanted is made stale so the worker never renders pages you have scrolled past.
        let wanted_keys: HashSet<TileKey> = wanted.iter().map(|w| w.0).collect();
        if d.pending.iter().any(|k| !wanted_keys.contains(k)) {
            gens.tile.fetch_add(1, Ordering::Relaxed);
            d.pending.clear();
        }
        let stamp = gens.tile.load(Ordering::Relaxed);
        wanted.sort_by(|a, b| a.1.total_cmp(&b.1));
        for ((page, bits, tx, ty), _) in wanted {
            if d.pending.insert((page, bits, tx, ty)) {
                let _ = req.send(Req::Tile { id: d.id, page, scale: f32::from_bits(bits), tx, ty, stamp });
            }
        }

        if let Some(thumb) = bar {
            painter.rect_filled(thumb, CornerRadius::same(4), LINE_STRONG);
        }
    }

    fn flush_thumbs(&mut self) {
        let Self { docs, req, gens, active, .. } = self;
        let Some(d) = docs.get_mut(*active) else { return };
        let wanted: HashSet<u32> = d.thumb_wanted.drain(..).filter(|p| !d.thumbs.contains_key(p)).collect();
        if d.thumb_pending.iter().any(|p| !wanted.contains(p)) {
            gens.thumb.fetch_add(1, Ordering::Relaxed);
            d.thumb_pending.clear();
        }
        let stamp = gens.thumb.load(Ordering::Relaxed);
        let mut list: Vec<u32> = wanted.into_iter().collect();
        list.sort_unstable();
        for page in list {
            if d.thumb_pending.insert(page) {
                let _ = req.send(Req::Thumb { id: d.id, page, width: THUMB_W, stamp });
            }
        }
    }

    /// Takes results from the update worker.
    fn poll_update(&mut self, ctx: &egui::Context, now: f64) {
        while let Ok(u) = self.update_rx.try_recv() {
            let quiet = std::mem::take(&mut self.quiet_check);
            match &u {
                Update::Available { version, .. } if quiet => {
                    self.say(format!("Shorui {version} is available. Open Settings to update."), false, now + 4.0);
                }
                // A background check that fails or finds nothing stays silent.
                Update::Failed(_) | Update::UpToDate if quiet => {
                    self.update = Update::Idle;
                    continue;
                }
                Update::Installing => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
            self.update = u;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let before = self.settings.clone();
        let icon = self
            .about_icon
            .get_or_insert_with(|| {
                let image = egui::ColorImage::from_rgba_unmultiplied([64, 64], include_bytes!("../assets/icon-64.rgba"));
                ctx.load_texture("icon", image, egui::TextureOptions::LINEAR)
            })
            .clone();
        let mut close = false;
        let section = |ui: &mut Ui, text: &str| {
            ui.add_space(SPACE_3);
            ui.label(RichText::new(text).font(theme::label()).color(INK_MUTED));
        };
        let primary =
            |text: &str| egui::Button::new(RichText::new(text).font(theme::label()).color(ON_ACCENT)).fill(ACCENT);
        let plain = |text: &str| egui::Button::new(RichText::new(text).font(theme::label()));
        let max_h = (ctx.content_rect().height() - 120.0).max(240.0);
        let modal = egui::Modal::new(Id::unique("settings")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = SPACE_2;
            ui.label(RichText::new("Settings").font(theme::title()));
            egui::ScrollArea::vertical().max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
            ui.set_width(452.0);

            section(ui, "When a PDF opens");
            ui.horizontal(|ui| {
                for (value, text) in
                    [(OpenZoom::FitWidth, "Fit width"), (OpenZoom::FitPage, "Fit page"), (OpenZoom::Actual, "100%")]
                {
                    let on = self.settings.open_zoom == value;
                    let button = if on { primary(text) } else { plain(text) };
                    if ui.add(button).clicked() {
                        self.settings.open_zoom = value;
                    }
                }
            });
            ui.checkbox(
                &mut self.settings.restore_session,
                RichText::new("Reopen the last tabs when Shorui starts").font(theme::body()),
            );
            if ui.checkbox(&mut self.settings.sidebar, RichText::new("Show page thumbnails").font(theme::body())).changed() {
                self.sidebar = self.settings.sidebar;
            }

            section(ui, "Scanned pages");
            if ui
                .checkbox(&mut self.settings.ocr, RichText::new("Read scanned pages with OCR").font(theme::body()))
                .on_hover_text("Lets you find, select and highlight text in scans. Runs on this PC.")
                .changed()
            {
                self.gens.ocr.store(self.settings.ocr, Ordering::Relaxed);
                // Ask for page text again so the change shows on the pages already open.
                for d in &mut self.docs {
                    d.texts.clear();
                    d.text_pending.clear();
                }
            }
            let busy = self.ocr_download.is_some();
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(SPACE_2, SPACE_2);
                for (kind, name) in [(Kind::Windows, "Windows"), (Kind::Latin, "Neural: Latin"), (Kind::Cjk, "Neural: Chinese & Japanese")] {
                    let on = self.settings.ocr_engine == kind;
                    let size = neural::download_size(kind);
                    let text = if kind.neural() && !neural::installed(kind) {
                        format!("{name}, {} MB download", size.div_ceil(1 << 20))
                    } else {
                        name.to_string()
                    };
                    let button = if on { primary(&text) } else { plain(&text) };
                    if ui.add_enabled(self.settings.ocr && (!busy || on), button).clicked() && !on {
                        self.ocr_error = None;
                        if neural::installed(kind) || !kind.neural() {
                            self.use_engine(kind);
                        } else {
                            self.ocr_download = Some((kind, 0, size));
                            neural::install(kind, self.pack_tx.clone(), ctx.clone());
                        }
                    }
                }
            });
            if let Some((_, done, total)) = self.ocr_download {
                let mb = |b: u64| b as f32 / (1 << 20) as f32;
                ui.add(
                    egui::ProgressBar::new(if total > 0 { done as f32 / total as f32 } else { 0.0 })
                        .desired_height(6.0)
                        .fill(ACCENT),
                );
                ui.label(RichText::new(format!("Downloading {:.0} of {:.0} MB", mb(done), mb(total))).font(theme::caption()).color(INK_MUTED));
            } else if let Some(e) = &self.ocr_error {
                ui.label(RichText::new(e).font(theme::caption()).color(DANGER));
            } else {
                let status = self.gens.ocr_status.lock().map(|s| s.clone()).unwrap_or_default();
                let (text, color) = if !status.text.is_empty() {
                    (status.text, if status.error { DANGER } else { INK_MUTED })
                } else if self.settings.ocr_engine.neural() {
                    ("Neural OCR starts when a scanned page needs reading.".to_string(), INK_MUTED)
                } else {
                    ("Windows OCR reads in the languages installed in Windows.".to_string(), INK_MUTED)
                };
                ui.label(RichText::new(text).font(theme::caption()).color(color));
            }
            ui.label(
                RichText::new(
                    "The neural engines read poor scans better and place every character exactly. They are a one-time \
                     download from Shorui's GitHub releases, checked against a fixed checksum, and run on this PC, on the \
                     graphics card when it can. Nothing you read is sent anywhere.",
                )
                .font(theme::caption())
                .color(INK_MUTED),
            );
            if !busy && (neural::installed(Kind::Latin) || neural::installed(Kind::Cjk)) {
                if ui.link(RichText::new("Remove the downloaded engines").font(theme::caption())).clicked() {
                    if self.settings.ocr_engine.neural() {
                        self.use_engine(Kind::Windows);
                    }
                    neural::remove();
                }
            }

            section(ui, "Highlight colour");
            ui.horizontal(|ui| {
                for (i, (name, rgb)) in settings::HIGHLIGHTS.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
                    let color = Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
                    ui.painter().circle_filled(rect.center(), 9.0, color);
                    if self.settings.highlight == i {
                        ui.painter().circle_stroke(rect.center(), 12.5, Stroke::new(2.0, ACCENT_BRIGHT));
                    }
                    if resp.on_hover_text(*name).clicked() {
                        self.settings.highlight = i;
                    }
                }
            });

            section(ui, "Default app");
            ui.horizontal(|ui| {
                if ui.add(plain("Make Shorui the default PDF app")).clicked() {
                    settings::open_default_apps();
                }
            });
            ui.label(
                RichText::new("Opens Windows Settings. Set .pdf to Shorui there; Windows doesn't let apps do it for you.")
                    .font(theme::caption())
                    .color(INK_MUTED),
            );

            section(ui, "Updates");
            ui.checkbox(
                &mut self.settings.auto_update,
                RichText::new("Check for updates when Shorui starts").font(theme::body()),
            );
            ui.horizontal(|ui| match self.update.clone() {
                Update::Available { version, url } => {
                    if ui.add(primary(&format!("Install {version}"))).clicked() {
                        self.update = Update::Downloading { version: version.clone() };
                        settings::download_and_run(version, url, self.update_tx.clone(), ctx.clone());
                    }
                    ui.label(RichText::new("A new version is available.").font(theme::body()).color(INK_MUTED));
                }
                Update::Checking => {
                    ui.add_enabled(false, plain("Checking"));
                }
                Update::Downloading { version } => {
                    ui.add_enabled(false, plain("Downloading"));
                    ui.label(RichText::new(format!("Getting {version}. Shorui will close to update.")).font(theme::body()).color(INK_MUTED));
                }
                state => {
                    if ui.add(plain("Check for updates")).clicked() {
                        self.update = Update::Checking;
                        self.quiet_check = false;
                        self.settings.last_check = settings::now();
                        settings::check(self.update_tx.clone(), ctx.clone());
                    }
                    let (text, color) = match state {
                        Update::UpToDate => ("You have the latest version.".to_string(), INK_MUTED),
                        Update::Failed(e) => (e, DANGER),
                        _ => (String::new(), INK_MUTED),
                    };
                    ui.label(RichText::new(text).font(theme::body()).color(color));
                }
            });
            ui.label(
                RichText::new("The check asks GitHub for the latest release. Nothing about you or your files is sent.")
                    .font(theme::caption())
                    .color(INK_MUTED),
            );

            section(ui, "About");
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(&icon).fit_to_exact_size(Vec2::splat(48.0)));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(format!("Shorui {}", settings::VERSION)).font(theme::body()).color(INK));
                    ui.label(
                        RichText::new("A fast PDF reader and editor. MIT licence. Renders with PDFium.")
                            .font(theme::caption())
                            .color(INK_MUTED),
                    );
                    if ui.link(RichText::new("github.com/invictus-1/Shorui").font(theme::caption())).clicked() {
                        settings::open_url(settings::REPO_URL);
                    }
                });
            });

            });
            ui.add_space(SPACE_3);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(primary("Done")).clicked() {
                    close = true;
                }
            });
        });
        if close || modal.should_close() {
            self.settings_open = false;
        }
        if self.settings != before {
            self.settings.save();
        }
    }

    fn modals(&mut self, ctx: &egui::Context) {
        if let Some((page, at, text)) = &mut self.note_prompt {
            let (page, at) = (*page, *at);
            let mut choice = 0;
            let modal = egui::Modal::new(Id::unique("note")).frame(theme::modal_frame()).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.label(RichText::new("Add note").font(theme::title()));
                let field = ui.add(
                    egui::TextEdit::multiline(text).desired_rows(4).desired_width(f32::INFINITY).font(theme::body()),
                );
                field.request_focus();
                ui.add_space(SPACE_3);
                ui.horizontal(|ui| {
                    let add = egui::Button::new(RichText::new("Add").font(theme::label()).color(ON_ACCENT)).fill(ACCENT);
                    if ui.add(add).clicked() {
                        choice = 1;
                    }
                    if ui.add(egui::Button::new(RichText::new("Cancel").font(theme::label()))).clicked() {
                        choice = 2;
                    }
                });
            });
            if modal.should_close() && choice == 0 {
                choice = 2;
            }
            if choice != 0 {
                let (_, _, text) = self.note_prompt.take().unwrap();
                if choice == 1 && !text.trim().is_empty() {
                    if let Some(d) = self.docs.get(self.active) {
                        let edit = Edit::Note { page, at: [at.x, at.y], text: text.trim().to_string() };
                        let _ = self.req.send(Req::Edit { id: d.id, edit });
                    }
                }
            }
        }

        let primary = |text: &str| {
            egui::Button::new(RichText::new(text).font(theme::label()).color(ON_ACCENT)).fill(ACCENT)
        };
        let plain = |text: &str| egui::Button::new(RichText::new(text).font(theme::label()));

        if let Some(confirm) = &self.confirm {
            let (title, body, can_save) = match confirm {
                Confirm::CloseTab(id) => {
                    let name = self.docs.iter().find(|d| d.id == *id).map(|d| d.title.clone()).unwrap_or_default();
                    ("Save changes?".to_string(), format!("{name} has changes that aren't saved."), true)
                }
                Confirm::Quit => {
                    let k = self.docs.iter().filter(|d| d.dirty).count();
                    let body = if k == 1 {
                        "1 document has changes that aren't saved.".to_string()
                    } else {
                        format!("{k} documents have changes that aren't saved.")
                    };
                    ("Quit without saving?".to_string(), body, false)
                }
            };
            let mut choice = 0;
            let modal = egui::Modal::new(Id::unique("confirm")).frame(theme::modal_frame()).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(RichText::new(title).font(theme::title()));
                ui.label(RichText::new(body).font(theme::body()).color(INK_MUTED));
                ui.add_space(SPACE_3);
                ui.horizontal(|ui| {
                    if can_save && ui.add(primary("Save")).clicked() {
                        choice = 1;
                    }
                    if ui.add(plain(if can_save { "Discard" } else { "Quit" })).clicked() {
                        choice = 2;
                    }
                    if ui.add(plain("Cancel")).clicked() {
                        choice = 3;
                    }
                });
            });
            if modal.should_close() {
                choice = 3;
            }
            if choice != 0 {
                match (self.confirm.take().unwrap(), choice) {
                    (Confirm::CloseTab(id), 1) => {
                        if let Some(d) = self.doc_by_id(id) {
                            d.close_after_save = true;
                            let (id, path) = (d.id, d.path.clone());
                            let _ = self.req.send(Req::Save { id, path });
                        }
                    }
                    (Confirm::CloseTab(id), 2) => {
                        if let Some(i) = self.docs.iter().position(|d| d.id == id) {
                            self.close(i);
                        }
                    }
                    (Confirm::Quit, 2) => {
                        self.quit_confirmed = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    _ => {}
                }
            }
        }

        if let Some((id, text, wrong)) = &mut self.password_prompt {
            let id = *id;
            let mut choice = 0;
            let modal = egui::Modal::new(Id::unique("password")).frame(theme::modal_frame()).show(ctx, |ui| {
                ui.set_width(340.0);
                ui.label(RichText::new("Password needed").font(theme::title()));
                let note = if *wrong { "That password didn't work." } else { "This PDF is locked." };
                ui.label(RichText::new(note).font(theme::body()).color(if *wrong { DANGER } else { INK_MUTED }));
                let field = ui.add(egui::TextEdit::singleline(text).password(true).desired_width(f32::INFINITY));
                field.request_focus();
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    choice = 1;
                }
                ui.add_space(SPACE_3);
                ui.horizontal(|ui| {
                    if ui.add(primary("Open")).clicked() {
                        choice = 1;
                    }
                    if ui.add(plain("Cancel")).clicked() {
                        choice = 2;
                    }
                });
            });
            if modal.should_close() && choice == 0 {
                choice = 2;
            }
            if choice != 0 {
                let (_, text, _) = self.password_prompt.take().unwrap();
                if let Some(i) = self.docs.iter().position(|d| d.id == id) {
                    if choice == 1 {
                        let d = &mut self.docs[i];
                        d.password = Some(text.clone());
                        let _ = self.req.send(Req::Open { id, path: d.path.clone(), password: Some(text) });
                    } else {
                        self.close(i);
                    }
                }
            }
        }
    }

    fn toast(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let Some(t) = &self.toast else { return };
        if now >= t.until {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_secs_f64(t.until - now));
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::unique("toast")));
        let galley = painter.layout_no_wrap(t.text.clone(), theme::body(), if t.error { DANGER } else { INK });
        let screen = ctx.content_rect();
        let size = galley.size() + vec2(SPACE_4 * 2.0, SPACE_2 * 2.0);
        let rect = Rect::from_min_size(pos2(screen.center().x - size.x / 2.0, screen.max.y - size.y - SPACE_6), size);
        painter.rect(rect, CornerRadius::same(RADIUS_LG), BG_300, Stroke::new(1.0, LINE_STRONG), StrokeKind::Inside);
        painter.galley(rect.min + vec2(SPACE_4, SPACE_2), galley, INK);
    }

    fn run_selftest(&mut self, ctx: &egui::Context, ui_time: Duration) {
        let Some(t) = &mut self.selftest else { return };
        if std::env::var_os("SHORUI_SELFTEST_HOME").is_some() {
            // The start screen with recent files from the settings in SHORUI_CONFIG_DIR.
            ctx.request_repaint();
            t.frames += 1;
            match t.phase {
                0 if t.frames > 5 && self.cover_pending.is_empty() && !self.covers.is_empty() => {
                    t.log += &format!("start screen: {} recent files, {} covers\n", self.settings.recent.len(), self.covers.len());
                    let (dir, shots) = (t.dir.clone(), t.shots.clone());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                        let _ = write_bmp(&dir.join("home.bmp"), &img);
                        shots.fetch_add(1, Ordering::Relaxed);
                    })));
                    t.phase = 1;
                }
                1 if t.shots.load(Ordering::Relaxed) >= 1 => {
                    let _ = std::fs::write(t.dir.join("log.txt"), &t.log);
                    std::process::exit(0);
                }
                _ => {}
            }
            return;
        }
        let Some(d) = self.docs.get_mut(self.active) else { return };
        if let Some(word) = std::env::var_os("SHORUI_SELFTEST_OCR") {
            ctx.request_repaint();
            match t.phase {
                0 if !d.loading && d.complete && d.texts.contains_key(&0) && self.ocr_download.is_none() => {
                    let x = &d.texts[&0];
                    let sample: String = x.chars.iter().take(70).filter(|c| **c != '\r' && **c != '\n').collect();
                    t.log += &format!(
                        "scanned page 1: text from OCR: {}, {} characters, starts {:?}\n",
                        x.ocr,
                        x.chars.len(),
                        sample
                    );
                    d.search_open = true;
                    d.query = word.to_string_lossy().into_owned();
                    start_search(d, &self.req, &self.gens);
                    d.sel = Some(((0, 0), (0, 40)));
                    t.zoom_started = Some(Instant::now());
                    t.phase = 1;
                }
                1 if d.search_done && d.complete => {
                    let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                    let mut pages: Vec<u32> = d.hits.iter().map(|h| h.page).collect();
                    pages.dedup();
                    t.log += &format!(
                        "search {:?} over {} scanned pages: {} matches on {} pages in {ms:.0} ms\n",
                        d.query,
                        d.n(),
                        d.hits.len(),
                        pages.len()
                    );
                    let status = self.gens.ocr_status.lock().map(|s| s.text.clone()).unwrap_or_default();
                    t.log += &format!("engine: {status}\n");
                    let (dir, shots) = (t.dir.clone(), t.shots.clone());
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                        let _ = write_bmp(&dir.join("ocr.bmp"), &img);
                        shots.fetch_add(1, Ordering::Relaxed);
                    })));
                    t.phase = 2;
                }
                2 if t.shots.load(Ordering::Relaxed) >= 1 => {
                    // Copy each page's text in turn, for scoring against the ground truth.
                    self.last_copied = None;
                    t.frames = 0;
                    let _ = self.req.send(Req::Copy { id: d.id, from: (0, 0), to: (0, u32::MAX) });
                    t.phase = 3;
                }
                3 if self.last_copied.is_some() => {
                    let text = self.last_copied.take().unwrap_or_default();
                    let dump = t.dir.join("ocr-text.txt");
                    let mut all = std::fs::read_to_string(&dump).unwrap_or_default();
                    if t.frames == 0 {
                        all.clear();
                    } else {
                        all.push_str("\n\u{c}\n");
                    }
                    all.push_str(&text);
                    let _ = std::fs::write(&dump, all);
                    t.frames += 1;
                    if t.frames < d.n() {
                        let page = t.frames;
                        let _ = self.req.send(Req::Copy { id: d.id, from: (page, 0), to: (page, u32::MAX) });
                    } else {
                        let _ = std::fs::write(t.dir.join("log.txt"), &t.log);
                        std::process::exit(0);
                    }
                }
                _ => {}
            }
            return;
        }
        ctx.request_repaint();
        let elapsed = self.t0.elapsed().as_secs_f64() * 1000.0;
        let shot = |name: &'static str, t: &SelfTest| {
            let (dir, shots) = (t.dir.clone(), t.shots.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let _ = write_bmp(&dir.join(name), &img);
                shots.fetch_add(1, Ordering::Relaxed);
            })));
        };
        // Mode switches wait until the borrows of the test state and document end.
        let mut todo = 0;
        match t.phase {
            0 if !d.loading && d.complete => {
                t.log += &format!("first page sharp at {elapsed:.1} ms after launch\n");
                shot("open.bmp", t);
                t.phase = 1;
            }
            1 if t.shots.load(Ordering::Relaxed) >= 1 => {
                t.phase = 2;
                t.frames = 0;
                t.zoom_started = Some(Instant::now());
            }
            2 => {
                // Zoom in ~8x over 70 frames, then back out, as fast as frames come.
                let f = if t.frames < 70 { 1.03 } else { 1.0 / 1.03 };
                d.fit = Fit::None;
                let anchor = d.view * vec2(0.4, 0.3);
                d.set_zoom(d.zoom * f, anchor, ctx.input(|i| i.time));
                t.frames += 1;
                t.max_ui = t.max_ui.max(ui_time);
                t.sum_ui += ui_time;
                if t.frames == 70 {
                    shot("zoomed-mid-gesture.bmp", t);
                }
                if t.frames >= 140 {
                    let wall = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                    t.log += &format!(
                        "zoom gesture: 140 frames in {wall:.0} ms ({:.1} fps); time spent building a frame: avg {:.2} ms, worst {:.2} ms\n",
                        140000.0 / wall,
                        t.sum_ui.as_secs_f64() * 1000.0 / 140.0,
                        t.max_ui.as_secs_f64() * 1000.0
                    );
                    t.phase = 3;
                    t.zoom_started = Some(Instant::now());
                    d.set_zoom(4.0, anchor, ctx.input(|i| i.time));
                }
            }
            3 if d.complete => {
                let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                t.log += &format!("jump to 400%: sharp after {ms:.0} ms (includes the {:.0} ms settle delay)\n", SETTLE * 1000.0);
                shot("zoom-400.bmp", t);
                t.phase = 4;
            }
            4 if t.shots.load(Ordering::Relaxed) >= 3 => {
                // Search and selection, with the results list showing.
                self.side_tab = SideTab::Results;
                d.fit = Fit::Page;
                d.go_to(0);
                d.search_open = true;
                d.query = "Kerning  glyph".into();
                start_search(d, &self.req, &self.gens);
                d.sel = Some(((0, 0), (0, 44)));
                t.zoom_started = Some(Instant::now());
                t.phase = 5;
            }
            5 if d.search_done && d.complete && d.texts.contains_key(&0) && d.fit_zoom(Fit::Page) == d.zoom => {
                let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                let mut pages: Vec<u32> = d.hits.iter().map(|h| h.page).collect();
                pages.dedup();
                t.log += &format!(
                    "search \"kerning glyph\": {} matches on {} pages in {ms:.0} ms, active match on page {}\n",
                    d.hits.len(),
                    pages.len(),
                    d.active_hit.map(|a| d.hits[a].page + 1).unwrap_or(0)
                );
                let _ = self.req.send(Req::Copy { id: d.id, from: (0, 0), to: (0, 44) });
                shot("search.bmp", t);
                t.phase = 6;
            }
            6 if t.shots.load(Ordering::Relaxed) >= 4 && self.last_copied.is_some() => {
                t.log += &format!("copied selection: {:?}\n", self.last_copied.as_deref().unwrap_or(""));
                self.side_tab = SideTab::Pages;
                // Highlight a selection that spans the title and three lines, plus one on page 2.
                let yellow = [255, 216, 77];
                let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Markup { kind: Markup::Highlight, from: (0, 0), to: (0, 260), color: yellow } });
                let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Markup { kind: Markup::Highlight, from: (1, 30), to: (1, 120), color: yellow } });
                let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::RemoveAnnot { page: 1, at: [200.0, 90.0] } });
                d.search_open = false;
                d.hits.clear();
                d.fit = Fit::Page;
                d.go_to(0);
                t.zoom_started = Some(Instant::now());
                t.phase = 60;
            }
            60 if d.dirty
                && d.complete
                && d.texts.get(&0).is_some_and(|x| !x.marks.is_empty())
                && d.texts.contains_key(&1) =>
            {
                let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                t.log += &format!(
                    "highlight two pages, remove one, redraw: {ms:.0} ms; page 1 has {} highlight(s), page 2 has {}\n",
                    d.texts[&0].marks.len(),
                    d.texts[&1].marks.len()
                );
                shot("highlight.bmp", t);
                t.phase = 61;
            }
            61 if t.shots.load(Ordering::Relaxed) >= 5 => {
                self.settings_open = true;
                t.frames = 0;
                t.phase = 62;
            }
            62 => {
                t.frames += 1;
                if t.frames == 4 {
                    shot("settings.bmp", t);
                    t.phase = 63;
                }
            }
            63 if t.shots.load(Ordering::Relaxed) >= 6 => {
                self.settings_open = false;
                // Move page 1 to third place and read it back from there.
                self.last_copied = None;
                let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Move { pages: vec![0], before: 3 } });
                let _ = self.req.send(Req::Copy { id: d.id, from: (2, 0), to: (2, 23) });
                t.phase = 70;
            }
            70 if self.last_copied.is_some() && d.can_undo => {
                t.log += &format!(
                    "move page 1 before page 4: the third page now reads {:?}\n",
                    self.last_copied.take().unwrap_or_default()
                );
                let _ = self.req.send(Req::Undo { id: d.id });
                let _ = self.req.send(Req::Copy { id: d.id, from: (0, 0), to: (0, 23) });
                t.phase = 71;
            }
            71 if self.last_copied.is_some() && d.can_redo => {
                t.log += &format!("undo the move: the first page reads {:?}\n", self.last_copied.take().unwrap_or_default());
                let id = d.id;
                let zigzag: Vec<[f32; 2]> = (0..22).map(|k| [60.0 + k as f32 * 11.0, if k % 2 == 0 { 668.0 } else { 684.0 }]).collect();
                for edit in [
                    Edit::Markup { kind: Markup::Underline, from: (0, 340), to: (0, 420), color: [124, 58, 237] },
                    Edit::Markup { kind: Markup::Strikeout, from: (0, 520), to: (0, 590), color: [229, 72, 77] },
                    Edit::Note { page: 0, at: [470.0, 60.0], text: "Check this figure against the source.".into() },
                    Edit::Ink { page: 0, points: zigzag, color: [124, 58, 237], width: 2.0 },
                ] {
                    let _ = self.req.send(Req::Edit { id, edit });
                }
                let extract = t.dir.join("extract.pdf");
                let _ = self.req.send(Req::Extract { id, pages: vec![0, 1, 2], path: extract.clone() });
                let job = crate::print::Job::File {
                    printer: "Microsoft Print to PDF".into(),
                    output: t.dir.join("printed.pdf"),
                    pages: 2,
                };
                let _ = self.req.send(Req::Print { id, job });
                let _ = self.req.send(Req::Edit { id, edit: Edit::Insert { path: extract, at: 1 } });
                d.fit = Fit::Page;
                d.go_to(0);
                t.phase = 72;
            }
            72 if d.n() == 303 && d.complete && d.texts.get(&0).is_some_and(|x| !x.notes.is_empty()) => {
                let x = &d.texts[&0];
                t.log += &format!(
                    "page 1 annotations: {} areas, {} note(s); links on page 1: {}; outline entries: {}; pages after inserting 3: {}\n",
                    x.marks.len(),
                    x.notes.len(),
                    x.links.len(),
                    d.outline.len(),
                    d.n()
                );
                shot("annotations.bmp", t);
                t.phase = 73;
            }
            73 if t.shots.load(Ordering::Relaxed) >= 7 => {
                self.side_tab = SideTab::Outline;
                d.two_page = true;
                d.relayout();
                d.fit = Fit::Width;
                d.go_to(0);
                t.frames = 0;
                t.phase = 74;
            }
            74 => {
                t.frames += 1;
                if t.frames > 6 && d.complete && d.fit_zoom(Fit::Width) == d.zoom {
                    shot("two-page.bmp", t);
                    t.phase = 75;
                }
            }
            75 if t.shots.load(Ordering::Relaxed) >= 8 => {
                self.side_tab = SideTab::Pages;
                d.two_page = false;
                d.relayout();
                d.fit = Fit::Page;
                let _ = self.req.send(Req::Undo { id: d.id });
                t.phase = 76;
            }
            76 if d.n() == 300 => {
                t.log += &format!("undo the insert: back to {} pages\n", d.n());
                // Edit round trip: rotate page 1, delete page 2, save a copy.
                let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Rotate { pages: vec![0], clockwise: true } });
                let _ = self.req.send(Req::Edit { id: d.id, edit: Edit::Delete { pages: vec![1] } });
                let _ = self.req.send(Req::Save { id: d.id, path: t.dir.join("edited.pdf") });
                d.go_to(0);
                t.zoom_started = Some(Instant::now());
                t.phase = 7;
            }
            7 if d.title == "edited.pdf" && d.complete && d.fit_zoom(Fit::Page) == d.zoom => {
                let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                t.log += &format!(
                    "rotate + delete + save + redraw: {ms:.0} ms, now {} pages\n",
                    d.n()
                );
                shot("edited.bmp", t);
                t.phase = 8;
            }
            8 if t.shots.load(Ordering::Relaxed) >= 9 => {
                t.zoom_started = Some(Instant::now());
                t.frames = 0;
                t.phase = 81;
                todo = 1;
            }
            81 => {
                t.frames += 1;
                if t.frames > 4 && d.complete && !d.thumbs.is_empty() {
                    let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                    t.log += &format!("night reading: pages redrawn in {ms:.0} ms\n");
                    shot("night.bmp", t);
                    t.phase = 82;
                }
            }
            82 if t.shots.load(Ordering::Relaxed) >= 10 => {
                todo = 2;
                t.frames = 0;
                t.phase = 83;
            }
            83 => {
                t.frames += 1;
                if t.frames > 8 && d.complete && d.fit_zoom(Fit::Page) == d.zoom {
                    shot("present.bmp", t);
                    t.phase = 84;
                }
            }
            84 if t.shots.load(Ordering::Relaxed) >= 11 => {
                todo = 3;
                t.phase = 85;
            }
            85 => {
                if std::env::var_os("SHORUI_SELFTEST_UPDATE").is_some() {
                    // Exercise the update check against the real GitHub repo.
                    let describe = |u: Update| match u {
                        Update::UpToDate => "up to date".to_string(),
                        Update::Available { version, url } => format!("{version} available at {url}"),
                        Update::Failed(e) => format!("failed: {e}"),
                        _ => "unexpected state".to_string(),
                    };
                    t.log += &format!(
                        "update check as {}: {}
update check as 0.0.1: {}
",
                        settings::VERSION,
                        describe(settings::latest_release(settings::VERSION)),
                        describe(settings::latest_release("0.0.1"))
                    );
                }
                let _ = std::fs::write(t.dir.join("log.txt"), &t.log);
                std::process::exit(0);
            }
            _ => {}
        }
        match todo {
            1 => self.set_night(true),
            2 => {
                self.set_night(false);
                self.set_present(ctx, true);
            }
            3 => self.set_present(ctx, false),
            _ => {}
        }
    }
}

fn write_bmp(path: &Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let row = (w * 3 + 3) / 4 * 4;
    let mut out = Vec::with_capacity(54 + row * h);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + row * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..h).rev() {
        for x in 0..w {
            let c = img.pixels[y * w + x];
            out.extend_from_slice(&[c.b(), c.g(), c.r()]);
        }
        out.resize(out.len() + row - w * 3, 0);
    }
    std::fs::write(path, out)
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.selftest.is_some() {
            return;
        }
        // Remember where each file was left, and the open tabs for next time.
        for d in self.docs.iter().filter(|d| d.error.is_none() && !d.loading && d.n() > 0) {
            self.settings.note_place(&d.path, d.current, d.zoom, d.n());
        }
        self.settings.session = if self.settings.restore_session {
            self.docs.iter().filter(|d| d.error.is_none() && !d.loading).map(|d| (d.path.clone(), d.current, d.zoom)).collect()
        } else {
            Vec::new()
        };
        self.settings.save();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let started = Instant::now();
        let ctx = ui.ctx().clone();
        self.frame += 1;
        let now = ctx.input(|i| i.time);
        if self.first_frame.is_none() {
            let t = self.t0.elapsed();
            self.first_frame = Some(t);
            if let Some(st) = &mut self.selftest {
                st.log += &format!("first frame at {:.1} ms after launch\n", t.as_secs_f64() * 1000.0);
            }
        }
        self.drain(now);
        if let Some(text) = self.clipboard.take() {
            ctx.copy_text(text);
        }

        let mut actions = Vec::new();
        for f in ctx.input(|i| i.raw.dropped_files.clone()) {
            actions.push(Action::Open(f.path().to_path_buf()));
        }
        self.shortcuts(&ctx, &mut actions);

        if ctx.input(|i| i.viewport().close_requested()) && !self.quit_confirmed && self.docs.iter().any(|d| d.dirty) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Quit);
        }

        let bar = |fill: Color32| egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(SPACE_2 as i8, 0));
        if self.present.is_none() {
            egui::Panel::top("tabs").exact_size(40.0).frame(bar(BG_100)).show_separator_line(false).show(ui, |ui| {
                self.tab_bar(ui, &mut actions);
            });
        }
        if !self.docs.is_empty() && self.present.is_none() {
            egui::Panel::top("toolbar")
                .exact_size(44.0)
                .frame(bar(BG_200))
                .show_separator_line(false)
                .show(ui, |ui| self.toolbar(ui, &mut actions));
            if self.sidebar {
                egui::Panel::left("sidebar")
                    .exact_size(SIDE_W)
                    .resizable(false)
                    .frame(egui::Frame::new().fill(BG_200))
                    .show_separator_line(false)
                    .show(ui, |ui| self.sidebar(ui, &mut actions));
            }
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.viewer(ui, &mut actions));

        self.flush_thumbs();
        self.poll_update(&ctx, now);
        self.poll_download(now);
        self.settings_window(&ctx);
        self.modals(&ctx);
        self.toast(&ctx);
        self.apply(&ctx, actions);
        if self.present.is_some() && self.docs.get(self.active).is_none_or(|d| d.loading || d.n() == 0) {
            // The document being presented went away.
            self.present = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }

        let title = match self.docs.get(self.active) {
            Some(d) => format!("{}{} - Shorui", d.title, if d.dirty { " (edited)" } else { "" }),
            None => "Shorui".to_string(),
        };
        if title != self.shown_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.shown_title = title;
        }
        self.run_selftest(&ctx, started.elapsed());
    }
}
