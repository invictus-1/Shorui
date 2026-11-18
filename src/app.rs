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
    render::{DocId, Gens, PageText, Req, Resp, TILE, line_rects},
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

/// One search match: its page and one rectangle per line it spans, in page points.
struct Hit {
    page: u32,
    rects: Vec<[f32; 4]>,
}

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

    fn top(&self, i: u32, zoom: f32) -> f32 {
        MARGIN + GAP * i as f32 + self.prefix[i as usize] * zoom * PT
    }

    fn total_h(&self) -> f32 {
        let n = self.n();
        2.0 * MARGIN + GAP * n.saturating_sub(1) as f32 + self.prefix[n as usize] * self.zoom * PT
    }

    fn content_w(&self, zoom: f32) -> f32 {
        (self.max_w * zoom * PT + 2.0 * MARGIN).max(self.view.x)
    }

    /// The page whose band (page plus the gap below it) contains content y.
    fn page_at(&self, y: f32) -> u32 {
        let n = self.n();
        if n == 0 {
            return 0;
        }
        let (mut lo, mut hi) = (0u32, n - 1);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            if self.top(mid, self.zoom) <= y {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
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
        let w = (self.view.x - 2.0 * MARGIN - 12.0).max(50.0);
        match fit {
            Fit::Width | Fit::None => w / (self.max_w * PT),
            Fit::Page => {
                let s = self.sizes[self.current as usize];
                let h = (self.view.y - 2.0 * MARGIN).max(50.0);
                (w / (s[0] * PT)).min(h / (s[1] * PT))
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
        let x = (self.content_w(self.zoom) - self.sizes[page as usize][0] * k) / 2.0 + r[0] * k;
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
    d.search_done = d.query.trim().is_empty();
    if !d.search_done {
        let _ = req.send(Req::Search { id: d.id, query: d.query.clone(), stamp });
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
    Highlight,
    Unhighlight(u32, Vec2),
    Settings,
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
        let (update_tx, update_rx) = std::sync::mpsc::channel();
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
        };
        for f in files {
            app.docs.push(DocView::new(app.next_id, f));
            app.next_id += 1;
        }
        app.active = app.docs.len().saturating_sub(1);
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
        self.docs.push(DocView::new(id, path.clone()));
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
        if self.active > i || self.active >= self.docs.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.invalidate_requests();
    }

    fn say(&mut self, text: impl Into<String>, error: bool, now: f64) {
        self.toast = Some(Toast { text: text.into(), until: now + if error { 6.0 } else { 2.0 }, error });
    }

    fn drain(&mut self, now: f64) {
        let frame = self.frame;
        while let Ok(r) = self.resp.try_recv() {
            match r {
                Resp::Fatal(e) => self.fatal = Some(e),
                Resp::Opened { id, sizes } => {
                    let opened_ms = self.t0.elapsed().as_secs_f64() * 1000.0;
                    if let Some(t) = &mut self.selftest {
                        t.log += &format!("document opened at {opened_ms:.1} ms, {} pages\n", sizes.len());
                    }
                    if let Some(d) = self.doc_by_id(id) {
                        d.loading = false;
                        d.set_sizes(sizes, 0);
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
                            d.hits.extend(hits.into_iter().map(|rects| Hit { page, rects }));
                            // Land on the first match at or after the page being read.
                            if d.active_hit.is_none() && page >= d.current {
                                d.show_hit(first);
                            }
                        }
                    }
                }
                Resp::SearchDone { id, stamp } => {
                    if let Some(d) = self.doc_by_id(id) {
                        if d.search_stamp == stamp {
                            d.search_done = true;
                            if d.active_hit.is_none() && !d.hits.is_empty() {
                                d.show_hit(0);
                            }
                        }
                    }
                }
                Resp::Annotated { id, epoch, pages, added } => {
                    if let Some(d) = self.doc_by_id(id) {
                        d.epoch = epoch;
                        for p in &pages {
                            if let Some(list) = d.tiles.remove(p) {
                                d.tile_bytes -= list.iter().map(|t| t.bytes).sum::<usize>();
                            }
                            d.thumbs.remove(p);
                            d.texts.remove(p);
                            d.text_pending.remove(p);
                        }
                        d.dirty = true;
                        d.complete = false;
                        if added {
                            d.sel = None;
                        }
                    }
                    self.invalidate_requests();
                    self.say(if added { "Highlighted." } else { "Removed highlight." }, false, now);
                }
                Resp::Copied(text) => {
                    self.last_copied = Some(text.clone());
                    if !text.is_empty() {
                        self.clipboard = Some(text);
                        self.say("Copied.", false, now);
                    }
                }
                Resp::Edited { id, epoch, sizes } => {
                    {
                        // The pages changed, so an open search runs again on the new document.
                        let Self { docs, req, gens, .. } = self;
                        if let Some(d) = docs.iter_mut().find(|d| d.id == id) {
                            if d.search_open && !d.query.trim().is_empty() {
                                start_search(d, req, gens);
                            }
                        }
                    }
                    if let Some(d) = self.doc_by_id(id) {
                        d.set_sizes(sizes, epoch);
                        d.dirty = true;
                        d.selected.retain(|&p| (p as usize) < d.sizes.len());
                        d.clamp_scroll();
                    }
                    self.invalidate_requests();
                }
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
        let modal = self.confirm.is_some() || self.password_prompt.is_some() || self.settings_open;
        if modal {
            return;
        }
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        ctx.input_mut(|i| {
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
            if d.sel.is_some() && i.consume_key(Modifiers::NONE, Key::H) {
                actions.push(Action::Highlight);
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
                Action::Highlight => {
                    if let Some(d) = self.docs.get(self.active) {
                        if let Some((from, to)) = d.sel_range() {
                            let color = self.settings.highlight_rgb();
                            let _ = self.req.send(Req::Highlight { id: d.id, from, to, color });
                        }
                    }
                }
                Action::Unhighlight(page, at) => {
                    if let Some(d) = self.docs.get(self.active) {
                        let _ = self.req.send(Req::Unhighlight { id: d.id, page, at: [at.x, at.y] });
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
                            let _ = self.req.send(Req::Rotate { id: d.id, pages: d.targets(), clockwise });
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
                    let _ = self.req.send(Req::Delete { id: d.id, pages });
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
        let Self { docs, active, req, gens, .. } = self;
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
                actions.push(Action::Highlight);
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
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                // Enter steps through matches and keeps the field ready for the next Enter.
                actions.push(Action::SearchStep(if ui.input(|i| i.modifiers.shift) { -1 } else { 1 }));
                d.focus_search = true;
            }
            let status = if d.query.trim().is_empty() {
                String::new()
            } else if d.hits.is_empty() {
                if d.search_done { "No results".to_string() } else { "Searching".to_string() }
            } else {
                let at = d.active_hit.map(|a| a + 1).unwrap_or(0);
                format!("{at} / {}{}", d.hits.len(), if d.search_done { "" } else { "+" })
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
        let Some(d) = self.docs.get_mut(self.active) else { return };
        if d.loading || d.n() == 0 {
            return;
        }
        let n = d.n();
        let total = d.side_prefix[n as usize];
        let width = ui.available_width();
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
                let resp = ui.interact(hit, Id::unique(("thumb", d.id, i)), Sense::click());
                let p = ui.painter();
                let selected = d.selected.contains(&i);
                let current = d.current == i;
                if selected {
                    p.rect_filled(hit, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
                } else if resp.hovered() {
                    p.rect_filled(hit, CornerRadius::same(RADIUS_MD), BG_300);
                }
                match d.thumbs.get(&i) {
                    Some(t) => {
                        p.image(t.id(), cell, UV, Color32::WHITE);
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
                d.thumb_wanted.push(i);
                i += 1;
            }
        });
    }

    fn empty_state(&mut self, ui: &mut Ui, rect: Rect, actions: &mut Vec<Action>) {
        let c = rect.center();
        let p = ui.painter();
        if let Some(e) = &self.fatal {
            p.text(c, Align2::CENTER_CENTER, e, theme::body(), DANGER);
            return;
        }
        p.text(c - vec2(0.0, 44.0), Align2::CENTER_CENTER, "Drop a PDF anywhere", theme::display(), INK);
        p.text(c - vec2(0.0, 10.0), Align2::CENTER_CENTER, "or open one with Ctrl+O", theme::body(), INK_MUTED);
        let button = Rect::from_center_size(c + vec2(0.0, 36.0), vec2(96.0, 32.0));
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
        let Self { docs, req, gens, active, settings, .. } = self;
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
            match settings.open_zoom {
                OpenZoom::FitWidth => d.zoom = d.fit_zoom(Fit::Width).clamp(MIN_ZOOM, 1.5),
                OpenZoom::FitPage => d.fit = Fit::Page,
                OpenZoom::Actual => d.zoom = 1.0,
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
        if resp.contains_pointer() {
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
        let bar = (total > d.view.y + 1.0).then(|| {
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

        let current = d.page_at(d.scroll.y + d.view.y * 0.35);
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
        let content_w = d.content_w(d.zoom);
        let mut i = d.page_at((d.scroll.y - PREFETCH).max(0.0));
        while i < n && d.top(i, d.zoom) < d.scroll.y + d.view.y + PREFETCH {
            let size = d.page_size(i);
            let x = rect.min.x + (content_w - size.x) / 2.0 - d.scroll.x;
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
                painter.rect_filled(prect, 0, PAGE);
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
        if resp.hovered() && !panning && pointer.is_some_and(|p| locate(d, p, false).is_some()) {
            ui.ctx().set_cursor_icon(CursorIcon::Text);
        }
        if resp.clicked() || resp.drag_started() {
            // Clicking the document takes the keyboard back from any text field.
            ui.memory_mut(|m| {
                if let Some(f) = m.focused() {
                    m.surrender_focus(f);
                }
            });
        }
        if resp.clicked() {
            d.sel = None;
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
                t.highlights.iter().any(|h| at.x >= h[0] && at.x <= h[2] && at.y >= h[1] && at.y <= h[3])
            })
        });
        let has_sel = d.sel.is_some();
        egui::Popup::context_menu(&resp).show(|ui| {
            ui.set_min_width(190.0);
            let item = |text: &str| egui::Button::new(RichText::new(text).font(theme::body()));
            if ui.add_enabled(has_sel, item("Copy    Ctrl+C")).clicked() {
                actions.push(Action::Copy);
            }
            if ui.add_enabled(has_sel, item("Highlight    H")).clicked() {
                actions.push(Action::Highlight);
            }
            if ui.add_enabled(over_highlight.is_some(), item("Remove highlight")).clicked() {
                if let Some((page, at)) = over_highlight {
                    actions.push(Action::Unhighlight(page, at));
                }
            }
        });
        if resp.double_clicked() {
            if let Some((page, c)) = pointer.and_then(|p| locate(d, p, false)) {
                let (a, b) = word_at(&d.texts[&page], c);
                d.sel = Some(((page, a), (page, b)));
            }
        }
        if resp.drag_started_by(egui::PointerButton::Primary) {
            let origin = ui.input(|i| i.pointer.press_origin()).or(pointer);
            let hit = origin.and_then(|p| locate(d, p, true));
            d.sel = hit.map(|h| (h, h));
            d.selecting = hit.is_some();
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
        let modal = egui::Modal::new(Id::unique("settings")).frame(theme::modal_frame()).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.spacing_mut().item_spacing.y = SPACE_2;
            ui.label(RichText::new("Settings").font(theme::title()));

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
            if ui.checkbox(&mut self.settings.sidebar, RichText::new("Show page thumbnails").font(theme::body())).changed() {
                self.sidebar = self.settings.sidebar;
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
        let Some(d) = self.docs.get_mut(self.active) else { return };
        ctx.request_repaint();
        let elapsed = self.t0.elapsed().as_secs_f64() * 1000.0;
        let shot = |name: &'static str, t: &SelfTest| {
            let (dir, shots) = (t.dir.clone(), t.shots.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let _ = write_bmp(&dir.join(name), &img);
                shots.fetch_add(1, Ordering::Relaxed);
            })));
        };
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
                // Search and selection.
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
                // Highlight a selection that spans the title and three lines, plus one on page 2.
                let _ = self.req.send(Req::Highlight { id: d.id, from: (0, 0), to: (0, 260), color: [255, 216, 77] });
                let _ = self.req.send(Req::Highlight { id: d.id, from: (1, 30), to: (1, 120), color: [255, 216, 77] });
                let _ = self.req.send(Req::Unhighlight { id: d.id, page: 1, at: [200.0, 90.0] });
                d.search_open = false;
                d.hits.clear();
                d.fit = Fit::Page;
                d.go_to(0);
                t.zoom_started = Some(Instant::now());
                t.phase = 60;
            }
            60 if d.dirty
                && d.complete
                && d.texts.get(&0).is_some_and(|x| !x.highlights.is_empty())
                && d.texts.contains_key(&1) =>
            {
                let ms = t.zoom_started.unwrap().elapsed().as_secs_f64() * 1000.0;
                t.log += &format!(
                    "highlight two pages, remove one, redraw: {ms:.0} ms; page 1 has {} highlight(s), page 2 has {}\n",
                    d.texts[&0].highlights.len(),
                    d.texts[&1].highlights.len()
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
                // Edit round trip: rotate page 1, delete page 2, save a copy.
                let _ = self.req.send(Req::Rotate { id: d.id, pages: vec![0], clockwise: true });
                let _ = self.req.send(Req::Delete { id: d.id, pages: vec![1] });
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
            8 if t.shots.load(Ordering::Relaxed) >= 7 => {
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
        egui::Panel::top("tabs").exact_size(40.0).frame(bar(BG_100)).show_separator_line(false).show(ui, |ui| {
            self.tab_bar(ui, &mut actions);
        });
        if !self.docs.is_empty() {
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
        self.settings_window(&ctx);
        self.modals(&ctx);
        self.toast(&ctx);
        self.apply(&ctx, actions);

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
