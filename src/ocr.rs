//! Text recognition for scanned pages.
//!
//! Two engines read a rendered page and return lines of characters with their boxes:
//! - the OCR engine built into Windows: offline, nothing to download, languages installed in Windows;
//! - the neural engine in [`crate::neural`]: PP-OCRv5 models through ONNX Runtime, downloaded only
//!   when the user turns it on, on the graphics card when there is one.
//!
//! Recognition runs on its own thread, so a slow page never holds up rendering. The PDFium worker
//! renders the page, hands the pixels over, and gets the result back as [`Req::Ocr`].

use std::{
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use sha2::{Digest, Sha256};
use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
};

use crate::render::{DocId, Req};

/// Lines of recognised characters, each with its box in image pixels: x0, y0, x1, y1.
pub type Lines = Vec<Vec<(char, [f32; 4])>>;

/// Characters and their boxes in top-left page points, ready for the text layer.
pub type Recognised = (Vec<char>, Vec<[f32; 4]>);

pub trait Engine {
    /// Reads a BGRA image.
    fn recognise(&mut self, bgra: &[u8], width: u32, height: u32) -> Option<Lines>;
}

/// Which engine reads scanned pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Windows,
    /// Neural, Latin-script recogniser: English and most European languages.
    Latin,
    /// Neural, Chinese and Japanese recogniser. Also reads English.
    Cjk,
}

impl Kind {
    pub fn key(self) -> &'static str {
        match self {
            Kind::Windows => "windows",
            Kind::Latin => "latin",
            Kind::Cjk => "cjk",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        [Kind::Windows, Kind::Latin, Kind::Cjk].into_iter().find(|k| k.key() == key)
    }

    pub fn neural(self) -> bool {
        self != Kind::Windows
    }
}

// ------------------------------------------------------------------ Windows engine

pub struct Windows {
    engine: OcrEngine,
    /// The largest image side the engine accepts, in pixels.
    max_dim: u32,
}

impl Windows {
    /// The engine for the user's Windows languages, if any of them supports OCR.
    pub fn new() -> Option<Self> {
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().ok()?;
        let max_dim = OcrEngine::MaxImageDimension().unwrap_or(2600);
        Some(Self { engine, max_dim })
    }
}

impl Engine for Windows {
    fn recognise(&mut self, bgra: &[u8], width: u32, height: u32) -> Option<Lines> {
        // Shrink pages larger than the engine accepts; boxes are scaled back afterwards.
        let k = (self.max_dim as f32 / width.max(height) as f32).min(1.0);
        let small;
        let (pixels, w, h) = if k < 1.0 {
            small = downscale(bgra, width, height, k);
            (&small.0[..], small.1, small.2)
        } else {
            (bgra, width, height)
        };
        let writer = DataWriter::new().ok()?;
        writer.WriteBytes(pixels).ok()?;
        let buffer = writer.DetachBuffer().ok()?;
        let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &buffer,
            BitmapPixelFormat::Bgra8,
            w as i32,
            h as i32,
            BitmapAlphaMode::Premultiplied,
        )
        .ok()?;
        let result = self.engine.RecognizeAsync(&bitmap).ok()?.join().ok()?;
        let back = 1.0 / k;
        let mut lines = Vec::new();
        for line in result.Lines().ok()? {
            let mut out = Vec::new();
            for word in line.Words().ok()? {
                let (Ok(text), Ok(r)) = (word.Text(), word.BoundingRect()) else { continue };
                let text = text.to_string();
                if !out.is_empty() {
                    out.push((' ', [0.0; 4]));
                }
                // The engine gives one box per word: share it out evenly between the letters.
                let n = text.chars().count().max(1) as f32;
                let (x0, x1) = (r.X * back, (r.X + r.Width) * back);
                let (y0, y1) = (r.Y * back, (r.Y + r.Height) * back);
                for (i, c) in text.chars().enumerate() {
                    let a = x0 + (x1 - x0) * i as f32 / n;
                    let b = x0 + (x1 - x0) * (i + 1) as f32 / n;
                    out.push((c, [a, y0, b, y1]));
                }
            }
            if !out.is_empty() {
                lines.push(out);
            }
        }
        Some(lines)
    }
}

/// Shrinks a BGRA image by `k` (< 1) with an area average.
fn downscale(bgra: &[u8], w: u32, h: u32, k: f32) -> (Vec<u8>, u32, u32) {
    let (nw, nh) = (((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1));
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let (sy0, sy1) = ((y * h / nh) as usize, (((y + 1) * h).div_ceil(nh)).min(h) as usize);
        for x in 0..nw {
            let (sx0, sx1) = ((x * w / nw) as usize, (((x + 1) * w).div_ceil(nw)).min(w) as usize);
            let mut acc = [0u32; 4];
            for sy in sy0..sy1 {
                let row = &bgra[sy * w as usize * 4..];
                for sx in sx0..sx1 {
                    for c in 0..4 {
                        acc[c] += row[sx * 4 + c] as u32;
                    }
                }
            }
            let n = ((sy1 - sy0) * (sx1 - sx0)).max(1) as u32;
            let o = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    (out, nw, nh)
}

// ------------------------------------------------------------------ the OCR thread

/// A page to read, rendered by the PDFium worker. `scale` is image pixels per page point.
pub struct Job {
    pub id: DocId,
    pub page: u32,
    pub epoch: u64,
    pub bgra: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
}

pub enum Msg {
    Read(Job),
    /// Switch engines. Pages read by the old engine are thrown away by the worker.
    Use(Kind),
}

/// The engine in use and how it is running, for Settings.
#[derive(Clone, Default)]
pub struct Status {
    pub text: String,
    pub error: bool,
}

pub type SharedStatus = Arc<Mutex<Status>>;

pub fn spawn(back: Sender<Req>, status: SharedStatus, ctx: egui::Context) -> Sender<Msg> {
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("ocr".into())
        .spawn(move || run(rx, back, status, ctx))
        .expect("spawn ocr thread");
    tx
}

fn set(status: &SharedStatus, text: impl Into<String>, error: bool) {
    if let Ok(mut s) = status.lock() {
        *s = Status { text: text.into(), error };
    }
}

fn run(rx: Receiver<Msg>, back: Sender<Req>, status: SharedStatus, ctx: egui::Context) {
    let mut kind = Kind::Windows;
    // Created the first time a page needs reading, so starting Shorui costs nothing.
    let mut engine: Option<Box<dyn Engine>> = None;
    let mut tried = false;
    for msg in rx {
        match msg {
            Msg::Use(k) => {
                if k != kind {
                    kind = k;
                    engine = None;
                    tried = false;
                }
            }
            Msg::Read(job) => {
                if !tried {
                    tried = true;
                    engine = load(kind, &status);
                    ctx.request_repaint();
                }
                let lines = engine.as_mut().and_then(|e| e.recognise(&job.bgra, job.width, job.height));
                let result = lines.map(|lines| to_points(lines, job.scale));
                let _ = back.send(Req::Ocr { id: job.id, page: job.page, epoch: job.epoch, kind, result });
            }
        }
    }
}

fn load(kind: Kind, status: &SharedStatus) -> Option<Box<dyn Engine>> {
    if kind.neural() {
        match crate::neural::Neural::load(&crate::neural::dir(), kind) {
            Ok(n) => {
                set(status, n.describe(), false);
                return Some(Box::new(n));
            }
            Err(e) => set(status, format!("Neural OCR didn't start, so Windows OCR is reading instead. {e}"), true),
        }
    } else {
        set(status, "Windows OCR, in the languages installed in Windows.", false);
    }
    match Windows::new() {
        Some(w) => Some(Box::new(w)),
        None => {
            set(status, "No OCR language is installed in Windows.", true);
            None
        }
    }
}

/// Flattens lines into the text layer's characters, in page points, with a line break after each.
fn to_points(lines: Lines, scale: f32) -> Recognised {
    let (mut chars, mut boxes) = (Vec::new(), Vec::new());
    for line in lines {
        for (c, b) in line {
            chars.push(c);
            boxes.push(if b == [0.0; 4] { b } else { [b[0] / scale, b[1] / scale, b[2] / scale, b[3] / scale] });
        }
        chars.extend(['\r', '\n']);
        boxes.extend([[0.0; 4], [0.0; 4]]);
    }
    (chars, boxes)
}

// ------------------------------------------------------------------ disk cache

/// Where downloaded engines and cached text live: `%LOCALAPPDATA%\Shorui`, or SHORUI_CONFIG_DIR.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SHORUI_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Shorui")
}

const CACHE_MAGIC: &[u8; 8] = b"SHOCR01\n";
/// The cache is trimmed, oldest files first, when it grows past this.
const CACHE_LIMIT: u64 = 256 << 20;

/// Identifies a file's contents cheaply: its size plus the first and last mebibyte.
pub fn file_key(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut h = Sha256::new();
    h.update(len.to_le_bytes());
    let mut buf = vec![0u8; 1 << 20];
    let n = f.read(&mut buf).ok()?;
    h.update(&buf[..n]);
    if len > buf.len() as u64 {
        f.seek(SeekFrom::Start(len - buf.len() as u64)).ok()?;
        let n = f.read(&mut buf).ok()?;
        h.update(&buf[..n]);
    }
    Some(h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect())
}

fn cache_path(key: &str, kind: Kind) -> PathBuf {
    data_dir().join("ocr-cache").join(format!("{key}-{}.bin", kind.key()))
}

/// Every page recognised earlier for this file by this engine.
pub fn cache_load(key: &str, kind: Kind) -> Vec<(u32, Recognised)> {
    let mut out = Vec::new();
    let Ok(data) = std::fs::read(cache_path(key, kind)) else { return out };
    if !data.starts_with(CACHE_MAGIC) {
        return out;
    }
    let mut at = CACHE_MAGIC.len();
    let u32_at = |i: usize| data.get(i..i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    while let (Some(page), Some(n)) = (u32_at(at), u32_at(at + 4)) {
        let n = n as usize;
        let end = at + 8 + n * 20;
        if end > data.len() {
            break; // a record cut short by a crash
        }
        let (mut chars, mut boxes) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for k in 0..n {
            let r = at + 8 + k * 20;
            let f = |i: usize| f32::from_le_bytes(data[r + 4 + i * 4..r + 8 + i * 4].try_into().unwrap());
            chars.push(char::from_u32(u32_at(r).unwrap_or(0xfffd)).unwrap_or('\u{fffd}'));
            boxes.push([f(0), f(1), f(2), f(3)]);
        }
        out.push((page, (chars, boxes)));
        at = end;
    }
    out
}

/// Appends one recognised page to the file's cache.
pub fn cache_store(key: &str, kind: Kind, page: u32, r: &Recognised) {
    let path = cache_path(key, kind);
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let new = !path.exists();
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let mut buf = Vec::with_capacity(8 + r.0.len() * 20 + if new { 8 } else { 0 });
    if new {
        buf.extend_from_slice(CACHE_MAGIC);
    }
    buf.extend_from_slice(&page.to_le_bytes());
    buf.extend_from_slice(&(r.0.len() as u32).to_le_bytes());
    for (c, b) in r.0.iter().zip(&r.1) {
        buf.extend_from_slice(&(*c as u32).to_le_bytes());
        for v in b {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    let _ = f.write_all(&buf);
}

/// Deletes the oldest cache files while the cache is larger than its limit.
pub fn cache_trim() {
    let Ok(entries) = std::fs::read_dir(data_dir().join("ocr-cache")) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= CACHE_LIMIT {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total -= len;
        }
    }
}
