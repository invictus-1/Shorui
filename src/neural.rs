//! Neural OCR: PP-OCRv5 "mobile" detection and recognition models run by ONNX Runtime.
//!
//! Nothing here is shipped with Shorui. When the user turns the engine on, the runtime and the
//! models are downloaded once from this project's GitHub releases, checked against the SHA-256
//! hashes below, and unpacked into `%LOCALAPPDATA%\Shorui\ocr`. ONNX Runtime is the DirectML
//! build: it runs on the graphics card when there is a DirectX 12 one, and on the processor
//! otherwise. Its telemetry is switched off.
//!
//! Pipeline, per page:
//! 1. Detection finds text lines as a probability map. Each connected region becomes a line box,
//!    turned to follow the line, so slightly skewed scans are read straight.
//! 2. Each line is sampled out along its box at 48 px high and recognised in batches of similar
//!    widths. CTC decoding also says where in the line each character was seen, which gives every
//!    character its own box rather than splitting a word's box evenly.

use std::{
    io::Read,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

use ort::{
    ep,
    environment::Environment,
    logging::LogLevel,
    session::{Session, builder::GraphOptimizationLevel},
    value::Tensor,
};
use sha2::{Digest, Sha256};

use crate::ocr::{Engine, Kind, Lines};

/// Detection input is limited to this many pixels on its longer side.
const DET_MAX: u32 = 2048;
/// Recognition input height, fixed by the model.
const REC_H: usize = 48;
const REC_MAX_W: usize = 3200;
/// Lines the recogniser is less sure of than this are dropped as noise.
const MIN_SCORE: f32 = 0.5;

// ------------------------------------------------------------------ downloads

/// One downloadable zip.
pub struct Pack {
    pub file: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

/// ONNX Runtime (DirectML build), DirectML and the detection model. Needed by every language.
pub const RUNTIME: Pack = Pack {
    file: "shorui-ocr-runtime-1.zip",
    sha256: "e248c6ce0e66e3286d88162e6afceda8045dbaf98e5699ff0119ceb51d9ac59f",
    bytes: 20050561,
};
pub const LATIN: Pack = Pack {
    file: "shorui-ocr-latin-1.zip",
    sha256: "ff4f900ead85b0fe4738fafc2450a29304fd483564d856f1ecfe0f72d6f6721c",
    bytes: 7326903,
};
pub const CJK: Pack = Pack {
    file: "shorui-ocr-cjk-1.zip",
    sha256: "06d5ece3af670c07c8990e80283fe8dba35fbedeea7a46b280f9d4bd6a15f1fd",
    bytes: 14085403,
};
const RELEASE: &str = "ocr-models-1";

pub fn packs(kind: Kind) -> Vec<&'static Pack> {
    match kind {
        Kind::Windows => vec![],
        Kind::Latin => vec![&RUNTIME, &LATIN],
        Kind::Cjk => vec![&RUNTIME, &CJK],
    }
}

pub fn dir() -> PathBuf {
    crate::ocr::data_dir().join("ocr")
}

fn marker(p: &Pack) -> PathBuf {
    dir().join(format!("{}.ok", p.file))
}

fn pack_ok(p: &Pack) -> bool {
    std::fs::read_to_string(marker(p)).is_ok_and(|s| s.trim() == p.sha256)
}

pub fn installed(kind: Kind) -> bool {
    packs(kind).into_iter().all(pack_ok)
}

/// Bytes still to download before `kind` can run.
pub fn download_size(kind: Kind) -> u64 {
    packs(kind).into_iter().filter(|p| !pack_ok(p)).map(|p| p.bytes).sum()
}

pub enum Progress {
    Bytes { done: u64, total: u64 },
    Done(Kind),
    Failed(String),
}

fn system32(exe: &str) -> Command {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    let mut c = Command::new(root.join("System32").join(exe));
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

fn sha256_file(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads, checks and unpacks what `kind` still needs, reporting progress as it goes.
pub fn install(kind: Kind, tx: std::sync::mpsc::Sender<Progress>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let result = install_now(kind, &tx, &ctx);
        let _ = tx.send(match result {
            Ok(()) => Progress::Done(kind),
            Err(e) => Progress::Failed(e),
        });
        ctx.request_repaint();
    });
}

fn install_now(kind: Kind, tx: &std::sync::mpsc::Sender<Progress>, ctx: &egui::Context) -> Result<(), String> {
    let dir = dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Can't create {}. {e}", dir.display()))?;
    let _ = std::fs::remove_file(dir.join("remove"));
    let base = std::env::var("SHORUI_OCR_URL")
        .unwrap_or_else(|_| format!("{}/releases/download/{RELEASE}/", crate::settings::REPO_URL));
    let missing: Vec<&Pack> = packs(kind).into_iter().filter(|p| !pack_ok(p)).collect();
    let total: u64 = missing.iter().map(|p| p.bytes).sum();
    let mut before = 0;
    for p in missing {
        let part = dir.join(format!("{}.part", p.file));
        let _ = std::fs::remove_file(&part);
        let mut child = system32("curl.exe")
            .args(["-sS", "-L", "--fail", "--max-time", "900", "-A", &format!("Shorui/{}", crate::settings::VERSION), "-o"])
            .arg(&part)
            .arg(format!("{base}{}", p.file))
            .spawn()
            .map_err(|_| "Can't run curl.exe.".to_string())?;
        let status = loop {
            if let Ok(Some(s)) = child.try_wait() {
                break s;
            }
            let done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            let _ = tx.send(Progress::Bytes { done: before + done.min(p.bytes), total });
            ctx.request_repaint();
            std::thread::sleep(Duration::from_millis(120));
        };
        if !status.success() {
            let _ = std::fs::remove_file(&part);
            return Err("The download failed. Check the connection and try again.".into());
        }
        if sha256_file(&part).as_deref() != Some(p.sha256) {
            let _ = std::fs::remove_file(&part);
            return Err("The download didn't match its checksum, so it was thrown away.".into());
        }
        let unpack = |flags: &str| system32("tar.exe").arg(flags).arg(&part).arg("-C").arg(&dir).status().is_ok_and(|s| s.success());
        // A runtime already loaded by this process can't be overwritten: keep the copy in use.
        let unpacked = unpack("-xf") || unpack("-xkf");
        let _ = std::fs::remove_file(&part);
        if !unpacked {
            return Err("Can't unpack the download.".into());
        }
        std::fs::write(marker(p), p.sha256).map_err(|e| e.to_string())?;
        before += p.bytes;
    }
    Ok(())
}

/// Forgets the downloaded engine. Files in use (the runtime, once loaded) go at the next start.
pub fn remove() {
    let dir = dir();
    for p in [&RUNTIME, &LATIN, &CJK] {
        let _ = std::fs::remove_file(marker(p));
    }
    let mut left = false;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            left |= std::fs::remove_file(e.path()).is_err();
        }
    }
    if left {
        let _ = std::fs::write(dir.join("remove"), "");
    }
}

/// Finishes a removal started in an earlier run. Call before anything loads the runtime.
pub fn finish_removal() {
    let dir = dir();
    if dir.join("remove").exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ------------------------------------------------------------------ the engine

fn s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// ONNX Runtime allows one environment per process; it is created on first use and kept.
fn environment(dir: &Path) -> Result<Environment, String> {
    static ENV: OnceLock<Result<Environment, String>> = OnceLock::new();
    ENV.get_or_init(|| {
        // Load the DirectML that came with the runtime first, so Windows doesn't hand ONNX
        // Runtime the older copy in System32.
        let dml: Vec<u16> = dir.join("DirectML.dll").as_os_str().encode_wide_z();
        unsafe {
            let _ = windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::PCWSTR(dml.as_ptr()));
        }
        let lib = dir.join("onnxruntime.dll");
        if !lib.exists() {
            return Err("The neural OCR files are missing. Download them again in Settings.".into());
        }
        ort::init_from(&lib)
            .map_err(|e| format!("Can't load ONNX Runtime. {e}"))?
            .with_name("Shorui")
            .with_telemetry(false)
            .build()
            .map_err(s)
    })
    .clone()
}

trait EncodeWideZ {
    fn encode_wide_z(&self) -> Vec<u16>;
}

impl EncodeWideZ for std::ffi::OsStr {
    fn encode_wide_z(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(Some(0)).collect()
    }
}

fn session(env: &Environment, model: &Path, gpu: bool) -> Result<Session, String> {
    let mut b = Session::builder(env)
        .map_err(s)?
        .with_optimization_level(GraphOptimizationLevel::All)
        .map_err(s)?
        .with_log_level(LogLevel::Fatal)
        .map_err(s)?;
    if gpu {
        b = b
            .with_memory_pattern(false)
            .map_err(s)?
            .with_parallel_execution(false)
            .map_err(s)?
            .with_execution_providers([ep::DirectML::default().build().error_on_failure()])
            .map_err(s)?;
    } else {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        b = b.with_intra_threads((cores / 2).clamp(1, 8)).map_err(s)?;
    }
    b.commit_from_file(model).map_err(s)
}

/// A detected text line: centre, direction along the line, and half its length and height.
#[derive(Clone, Copy)]
struct LineBox {
    cx: f32,
    cy: f32,
    ux: f32,
    uy: f32,
    half_len: f32,
    half_h: f32,
}

impl LineBox {
    /// The image position at `t` along the line and `s` across it, both from -1 to 1.
    fn at(&self, t: f32, s: f32) -> (f32, f32) {
        let (a, b) = (t * self.half_len, s * self.half_h);
        (self.cx + self.ux * a - self.uy * b, self.cy + self.uy * a + self.ux * b)
    }

    fn top(&self) -> f32 {
        self.cy - self.half_h
    }
}

pub struct Neural {
    det: Session,
    rec: Session,
    dict: Vec<String>,
    gpu: bool,
    kind: Kind,
    dir: PathBuf,
}

impl Neural {
    pub fn load(dir: &Path, kind: Kind) -> Result<Self, String> {
        if !installed(kind) && std::env::var_os("SHORUI_OCR_UNCHECKED").is_none() {
            return Err("Its files aren't downloaded yet.".into());
        }
        let env = environment(dir)?;
        let rec_name = format!("rec-{}", kind.key());
        let dict_text = std::fs::read_to_string(dir.join(format!("{rec_name}.txt"))).map_err(s)?;
        // Index 0 is the CTC blank; the dictionary follows, then a space.
        let mut dict: Vec<String> = dict_text.split('\n').map(|l| l.trim_end_matches('\r').to_string()).collect();
        dict.push(" ".into());
        let want_gpu = std::env::var_os("SHORUI_OCR_CPU").is_none();
        let det_path = dir.join("det.onnx");
        let rec_path = dir.join(format!("{rec_name}.onnx"));
        if want_gpu {
            if let (Ok(det), Ok(rec)) = (session(&env, &det_path, true), session(&env, &rec_path, true)) {
                return Ok(Self { det, rec, dict, gpu: true, kind, dir: dir.to_path_buf() });
            }
        }
        let det = session(&env, &det_path, false)?;
        let rec = session(&env, &rec_path, false)?;
        Ok(Self { det, rec, dict, gpu: false, kind, dir: dir.to_path_buf() })
    }

    pub fn describe(&self) -> String {
        let lang = match self.kind {
            Kind::Cjk => "Chinese, Japanese and English",
            _ => "Latin-script languages",
        };
        let on = if self.gpu { "on the graphics card" } else { "on the processor" };
        format!("Neural OCR for {lang}, running {on}.")
    }

    /// Switches to the processor after the graphics card failed mid-run.
    fn fall_back(&mut self) -> bool {
        if !self.gpu {
            return false;
        }
        let Ok(env) = environment(&self.dir) else { return false };
        let rec_path = self.dir.join(format!("rec-{}.onnx", self.kind.key()));
        match (session(&env, &self.dir.join("det.onnx"), false), session(&env, &rec_path, false)) {
            (Ok(det), Ok(rec)) => {
                (self.det, self.rec, self.gpu) = (det, rec, false);
                true
            }
            _ => false,
        }
    }

    fn detect(&mut self, img: &Image) -> Result<Vec<LineBox>, String> {
        let (w, h) = (img.w as f32, img.h as f32);
        let k = (DET_MAX as f32 / w.max(h)).min(1.0);
        let nw = (((w * k) / 32.0).round() as usize * 32).max(32);
        let nh = (((h * k) / 32.0).round() as usize * 32).max(32);
        let (sx, sy) = (w / nw as f32, h / nh as f32);
        // Normalised the way the model was trained: BGR order, ImageNet mean and deviation.
        const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
        const STD: [f32; 3] = [0.229, 0.224, 0.225];
        let plane = nw * nh;
        let mut input = vec![0f32; 3 * plane];
        for y in 0..nh {
            let fy = (y as f32 + 0.5) * sy - 0.5;
            for x in 0..nw {
                let px = img.sample((x as f32 + 0.5) * sx - 0.5, fy);
                for c in 0..3 {
                    input[c * plane + y * nw + x] = (px[c] / 255.0 - MEAN[c]) / STD[c];
                }
            }
        }
        let tensor = Tensor::from_array(([1usize, 3, nh, nw], input)).map_err(s)?;
        let outputs = self.det.run(ort::inputs![tensor]).map_err(s)?;
        let (_, prob) = outputs[0].try_extract_tensor::<f32>().map_err(s)?;
        Ok(boxes_from_map(prob, nw, nh, sx, sy))
    }

    fn recognise_lines(&mut self, img: &Image, boxes: &[LineBox]) -> Result<Vec<Vec<(char, [f32; 4])>>, String> {
        // Each line sampled straight out of the page at the model's height.
        let widths: Vec<usize> = boxes
            .iter()
            .map(|b| ((REC_H as f32 * b.half_len / b.half_h).ceil() as usize).clamp(8, REC_MAX_W))
            .collect();
        let mut order: Vec<usize> = (0..boxes.len()).collect();
        order.sort_by_key(|&i| widths[i]);
        let batch_size = if self.gpu { 16 } else { 8 };
        let mut out = vec![Vec::new(); boxes.len()];
        for chunk in order.chunks(batch_size) {
            let max_w = chunk.iter().map(|&i| widths[i]).max().unwrap_or(8).div_ceil(32) * 32;
            let plane = REC_H * max_w;
            // Padding stays 0, which is mid-grey after normalisation, as in training.
            let mut input = vec![0f32; chunk.len() * 3 * plane];
            for (n, &i) in chunk.iter().enumerate() {
                let (b, w) = (&boxes[i], widths[i]);
                let base = n * 3 * plane;
                for y in 0..REC_H {
                    let s = (y as f32 + 0.5) / REC_H as f32 * 2.0 - 1.0;
                    for x in 0..w {
                        let t = (x as f32 + 0.5) / w as f32 * 2.0 - 1.0;
                        let (px, py) = b.at(t, s);
                        let v = img.sample(px - 0.5, py - 0.5);
                        for c in 0..3 {
                            input[base + c * plane + y * max_w + x] = v[c] / 127.5 - 1.0;
                        }
                    }
                }
            }
            let tensor = Tensor::from_array(([chunk.len(), 3, REC_H, max_w], input)).map_err(s)?;
            let outputs = self.rec.run(ort::inputs![tensor]).map_err(s)?;
            let (shape, probs) = outputs[0].try_extract_tensor::<f32>().map_err(s)?;
            let (steps, classes) = (shape[1] as usize, shape[2] as usize);
            for (n, &i) in chunk.iter().enumerate() {
                let seq = &probs[n * steps * classes..(n + 1) * steps * classes];
                out[i] = decode(&self.dict, seq, steps, classes, widths[i], max_w, &boxes[i]);
            }
        }
        Ok(out)
    }

    fn read(&mut self, img: &Image) -> Result<Lines, String> {
        let mut boxes = self.detect(img)?;
        // Reading order: top to bottom, then left to right among lines that share a row.
        boxes.sort_by(|a, b| a.top().total_cmp(&b.top()));
        let mut rows: Vec<Vec<LineBox>> = Vec::new();
        for b in boxes {
            match rows.last_mut() {
                Some(row) if (row[0].cy - b.cy).abs() < 0.5 * row[0].half_h.min(b.half_h) => row.push(b),
                _ => rows.push(vec![b]),
            }
        }
        let mut ordered = Vec::new();
        for mut row in rows {
            row.sort_by(|a, b| (a.cx - a.half_len).total_cmp(&(b.cx - b.half_len)));
            ordered.extend(row);
        }
        let lines = self.recognise_lines(img, &ordered)?;
        Ok(lines.into_iter().filter(|l| !l.is_empty()).collect())
    }
}

/// Greedy CTC decoding. Returns the line's characters with boxes in image pixels, or
/// nothing if the recogniser wasn't confident.
#[allow(clippy::too_many_arguments)]
fn decode(dict: &[String], seq: &[f32], steps: usize, classes: usize, width: usize, max_w: usize, b: &LineBox) -> Vec<(char, [f32; 4])> {
    let step_px = max_w as f32 / steps as f32;
    let used = ((width as f32 / step_px).ceil() as usize).min(steps);
    // Runs of the same class: (class, first step, last step, best probability).
    let mut runs: Vec<(usize, usize, usize, f32)> = Vec::new();
    let mut prev = 0;
    for t in 0..used {
        let row = &seq[t * classes..(t + 1) * classes];
        let (mut best, mut p) = (0, row[0]);
        for (c, &v) in row.iter().enumerate().skip(1) {
            if v > p {
                (best, p) = (c, v);
            }
        }
        if best != 0 && best == prev {
            if let Some(r) = runs.last_mut() {
                r.2 = t;
                r.3 = r.3.max(p);
            }
        } else if best != 0 {
            runs.push((best, t, t, p));
        }
        prev = best;
    }
    runs.retain(|r| r.0 <= dict.len());
    if runs.is_empty() {
        return Vec::new();
    }
    let score = runs.iter().map(|r| r.3).sum::<f32>() / runs.len() as f32;
    if score < MIN_SCORE {
        return Vec::new();
    }
    // Each character owns the stretch of line from halfway after the previous one to
    // halfway before the next one. The recogniser sees a character near its middle, so the
    // first and last reach out by half an average character.
    let half = (runs[runs.len() - 1].2 + 1 - runs[0].1) as f32 / runs.len() as f32 / 2.0;
    let mut out = Vec::new();
    for (k, r) in runs.iter().enumerate() {
        let left = if k == 0 { r.1 as f32 - half } else { (runs[k - 1].2 as f32 + 1.0 + r.1 as f32) / 2.0 };
        let right = if k + 1 == runs.len() { r.2 as f32 + 1.0 + half } else { (r.2 as f32 + 1.0 + runs[k + 1].1 as f32) / 2.0 };
        let to_t = |step: f32| ((step * step_px) / width as f32).clamp(0.0, 1.0) * 2.0 - 1.0;
        let (t0, t1) = (to_t(left), to_t(right));
        let text = &dict[r.0 - 1];
        let n = text.chars().count().max(1) as f32;
        for (j, c) in text.chars().enumerate() {
            let (a, z) = (t0 + (t1 - t0) * j as f32 / n, t0 + (t1 - t0) * (j + 1) as f32 / n);
            let corners = [b.at(a, -1.0), b.at(z, -1.0), b.at(a, 1.0), b.at(z, 1.0)];
            let x0 = corners.iter().map(|p| p.0).fold(f32::MAX, f32::min);
            let x1 = corners.iter().map(|p| p.0).fold(f32::MIN, f32::max);
            let y0 = corners.iter().map(|p| p.1).fold(f32::MAX, f32::min);
            let y1 = corners.iter().map(|p| p.1).fold(f32::MIN, f32::max);
            out.push((c, [x0, y0, x1, y1]));
        }
    }
    out
}


impl Engine for Neural {
    fn recognise(&mut self, bgra: &[u8], width: u32, height: u32) -> Option<Lines> {
        let img = Image { px: bgra, w: width as usize, h: height as usize };
        match self.read(&img) {
            Ok(lines) => Some(lines),
            // A graphics card can disappear (driver update, sleep): try once more on the processor.
            Err(_) if self.fall_back() => self.read(&img).ok(),
            Err(_) => None,
        }
    }
}

/// A BGRA page image.
struct Image<'a> {
    px: &'a [u8],
    w: usize,
    h: usize,
}

impl Image<'_> {
    /// Bilinear sample at pixel coordinates (centres at integers). Outside the page is white.
    fn sample(&self, x: f32, y: f32) -> [f32; 3] {
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let get = |xx: isize, yy: isize| -> [f32; 3] {
            if xx < 0 || yy < 0 || xx >= self.w as isize || yy >= self.h as isize {
                return [255.0; 3];
            }
            let o = (yy as usize * self.w + xx as usize) * 4;
            [self.px[o] as f32, self.px[o + 1] as f32, self.px[o + 2] as f32]
        };
        let (a, b, c, d) = (get(x0, y0), get(x0 + 1, y0), get(x0, y0 + 1), get(x0 + 1, y0 + 1));
        let mut out = [0.0; 3];
        for i in 0..3 {
            let top = a[i] + (b[i] - a[i]) * fx;
            let bottom = c[i] + (d[i] - c[i]) * fx;
            out[i] = top + (bottom - top) * fy;
        }
        out
    }
}

/// Turns the detection probability map into line boxes in image pixels.
fn boxes_from_map(prob: &[f32], w: usize, h: usize, sx: f32, sy: f32) -> Vec<LineBox> {
    const THRESH: f32 = 0.3;
    const BOX_THRESH: f32 = 0.6;
    const UNCLIP: f32 = 1.5;
    let mut seen = vec![false; w * h];
    let mut stack = Vec::new();
    let mut pixels: Vec<(u32, u32)> = Vec::new();
    let mut out = Vec::new();
    for start in 0..w * h {
        if seen[start] || prob[start] <= THRESH {
            continue;
        }
        // Flood fill one region, four-connected.
        pixels.clear();
        stack.push(start);
        seen[start] = true;
        let mut sum = 0.0f32;
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            pixels.push((x as u32, y as u32));
            sum += prob[i];
            let mut push = |j: usize| {
                if !seen[j] && prob[j] > THRESH {
                    seen[j] = true;
                    stack.push(j);
                }
            };
            if x > 0 {
                push(i - 1);
            }
            if x + 1 < w {
                push(i + 1);
            }
            if y > 0 {
                push(i - w);
            }
            if y + 1 < h {
                push(i + w);
            }
        }
        if pixels.len() < 10 || sum / (pixels.len() as f32) < BOX_THRESH || out.len() >= 1000 {
            continue;
        }
        // The region's direction from its second moments, in image pixels.
        let n = pixels.len() as f32;
        let pts = pixels.iter().map(|&(x, y)| ((x as f32 + 0.5) * sx, (y as f32 + 0.5) * sy));
        let (mx, my) = pts.clone().fold((0.0, 0.0), |a, p| (a.0 + p.0 / n, a.1 + p.1 / n));
        let (mut cxx, mut cyy, mut cxy) = (0.0f32, 0.0f32, 0.0f32);
        for (x, y) in pts.clone() {
            let (dx, dy) = (x - mx, y - my);
            cxx += dx * dx;
            cyy += dy * dy;
            cxy += dx * dy;
        }
        let mut angle = 0.5 * (2.0 * cxy).atan2(cxx - cyy);
        let (l1, l2) = {
            let tr = cxx + cyy;
            let det = cxx * cyy - cxy * cxy;
            let d = (tr * tr / 4.0 - det).max(0.0).sqrt();
            (tr / 2.0 + d, (tr / 2.0 - d).max(1e-6))
        };
        // Short or steep regions are read level: their direction isn't reliable.
        if (l1 / l2).sqrt() < 2.0 || angle.abs() > std::f32::consts::FRAC_PI_4 {
            angle = 0.0;
        }
        let (ux, uy) = (angle.cos(), angle.sin());
        let (mut a0, mut a1, mut b0, mut b1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for (x, y) in pts {
            let (dx, dy) = (x - mx, y - my);
            let (a, b) = (dx * ux + dy * uy, -dx * uy + dy * ux);
            a0 = a0.min(a);
            a1 = a1.max(a);
            b0 = b0.min(b);
            b1 = b1.max(b);
        }
        let (len, height) = (a1 - a0 + sx, b1 - b0 + sy);
        if len.min(height) < 3.0 {
            continue;
        }
        // The model marks a shrunken core of each line; grow it back out.
        let d = len * height * UNCLIP / (2.0 * (len + height));
        let (ca, cb) = ((a0 + a1) / 2.0, (b0 + b1) / 2.0);
        out.push(LineBox {
            cx: mx + ux * ca - uy * cb,
            cy: my + uy * ca + ux * cb,
            ux,
            uy,
            half_len: len / 2.0 + d,
            half_h: height / 2.0 + d,
        });
    }
    out
}
