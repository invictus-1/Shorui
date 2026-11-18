#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod print;
mod render;
mod settings;
mod theme;

use std::{path::PathBuf, time::Instant};

fn main() -> eframe::Result {
    let t0 = Instant::now();
    let mut files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if files.is_empty() && std::env::var_os("SHORUI_SELFTEST").is_none() {
        // Nothing asked for: bring back the tabs from last time.
        let saved = settings::Settings::load();
        if saved.restore_session {
            files = saved.session.into_iter().map(|s| s.0).filter(|p| p.exists()).collect();
        }
    }
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
    let (req, resp) = render::spawn(ctx.clone(), gens.clone());
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
