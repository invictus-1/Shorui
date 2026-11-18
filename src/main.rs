#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod neural;
mod ocr;
mod print;
mod render;
mod settings;
mod theme;
mod window;

use std::{path::PathBuf, time::Instant};

fn main() -> eframe::Result {
    let t0 = Instant::now();
    let mut files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let saved = settings::Settings::load();
    if files.is_empty() && std::env::var_os("SHORUI_SELFTEST").is_none() && saved.restore_session {
        // Nothing asked for: bring back the tabs from last time.
        files = saved.session.iter().map(|s| s.0.clone()).filter(|p| p.exists()).collect();
    }
    // A neural OCR removal started last time finishes before anything can load its files.
    neural::finish_removal();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Shorui")
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([560.0, 360.0])
            .with_drag_and_drop(true)
            .with_icon(egui::IconData { rgba: include_bytes!("../assets/icon-64.rgba").to_vec(), width: 64, height: 64 }),
        ..Default::default()
    };
    // Start PDFium and begin opening the files before the window exists, so the two overlap.
    let ctx = egui::Context::default();
    let gens = std::sync::Arc::new(render::Gens::default());
    gens.ocr.store(saved.ocr, std::sync::atomic::Ordering::Relaxed);
    gens.night.store(saved.night, std::sync::atomic::Ordering::Relaxed);
    let engine = match std::env::var("SHORUI_OCR_ENGINE").ok().and_then(|k| ocr::Kind::from_key(&k)) {
        Some(k) => k,
        None => saved.ocr_engine,
    };
    let (req, resp) = render::spawn(ctx.clone(), gens.clone(), engine);
    for (i, path) in files.iter().enumerate() {
        let _ = req.send(render::Req::Open { id: i as u64 + 1, path: path.clone(), password: None });
    }

    eframe::run_native_ext(
        "Shorui",
        options,
        Some(ctx),
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, files, t0, gens, req, resp)))),
    )
}
