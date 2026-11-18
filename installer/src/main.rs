//! Shorui Setup: one exe that installs, updates and removes Shorui for the current user.
//! No administrator rights, no services, nothing outside the install folder, the Start menu
//! shortcut and a handful of per-user registry keys.
//!
//!   shorui-setup.exe                     install (or update) with the window
//!   shorui-setup.exe --silent            install without a window
//!   shorui-setup.exe --uninstall         remove with the window (also: run as uninstall.exe)
//!   shorui-setup.exe --uninstall --silent
//!   --dir <folder>                       install somewhere else
//!   --no-register                        files only: no shortcuts, no registry

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[allow(dead_code)]
#[path = "../../src/theme.rs"]
mod theme;

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, channel},
};

use egui::{Color32, CornerRadius, Id, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use theme::*;

static APP_EXE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shorui.exe.z"));
static PDFIUM_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/pdfium.dll.z"));
const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Shorui";

// ------------------------------------------------------------------ the actual work

#[derive(Clone)]
struct Opts {
    dir: PathBuf,
    register: bool,
    desktop: bool,
}

fn default_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Programs").join("Shorui")
}

fn start_menu_link() -> Option<PathBuf> {
    let appdata = PathBuf::from(std::env::var_os("APPDATA")?);
    Some(appdata.join(r"Microsoft\Windows\Start Menu\Programs\Shorui.lnk"))
}

fn desktop_link() -> Option<PathBuf> {
    let desktop = PathBuf::from(std::env::var_os("USERPROFILE")?).join("Desktop");
    desktop.is_dir().then(|| desktop.join("Shorui.lnk"))
}

fn write_file(path: &Path, packed: &[u8]) -> Result<u64, String> {
    let bytes = miniz_oxide::inflate::decompress_to_vec(packed).map_err(|_| "The installer is damaged.".to_string())?;
    std::fs::write(path, &bytes).map_err(|e| {
        // 32 = sharing violation, 5 = access denied: the app is running.
        if matches!(e.raw_os_error(), Some(32) | Some(5)) {
            "Shorui is open. Close it and try again.".to_string()
        } else {
            format!("Can't write {}. {e}", path.display())
        }
    })?;
    Ok(bytes.len() as u64)
}

fn install(o: &Opts, progress: &dyn Fn(&str, f32)) -> Result<(), String> {
    progress("Creating folder", 0.05);
    std::fs::create_dir_all(&o.dir).map_err(|e| format!("Can't create {}. {e}", o.dir.display()))?;
    let exe = o.dir.join("shorui.exe");
    let uninstaller = o.dir.join("uninstall.exe");

    progress("Copying Shorui", 0.2);
    let mut size = write_file(&exe, APP_EXE)?;
    progress("Copying PDF engine", 0.5);
    size += write_file(&o.dir.join("pdfium.dll"), PDFIUM_DLL)?;

    progress("Adding uninstaller", 0.75);
    if let Ok(me) = std::env::current_exe() {
        let same = std::fs::canonicalize(&me).ok() == std::fs::canonicalize(&uninstaller).ok();
        if !same {
            size += std::fs::copy(&me, &uninstaller).map_err(|e| format!("Can't write the uninstaller. {e}"))?;
        }
    }

    if o.register {
        progress("Adding shortcuts", 0.85);
        let link = |to: Option<PathBuf>| -> Result<(), String> {
            let Some(to) = to else { return Ok(()) };
            let mut l = mslnk::ShellLink::new(&exe).map_err(|e| format!("Can't create the shortcut. {e}"))?;
            l.set_working_dir(Some(o.dir.display().to_string()));
            l.set_name(Some("Shorui PDF reader".to_string()));
            l.create_lnk(&to).map_err(|e| format!("Can't create the shortcut. {e}"))
        };
        link(start_menu_link())?;
        if o.desktop {
            link(desktop_link())?;
        }
        progress("Registering", 0.93);
        register(o, &exe, &uninstaller, size).map_err(|e| format!("Can't register Shorui. {e}"))?;
    }
    progress("Done", 1.0);
    Ok(())
}

fn register(o: &Opts, exe: &Path, uninstaller: &Path, size: u64) -> std::io::Result<()> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let open_cmd = format!("\"{}\" \"%1\"", exe.display());

    // Apps & features entry.
    let (k, _) = hkcu.create_subkey(UNINSTALL_KEY)?;
    k.set_value("DisplayName", &"Shorui")?;
    k.set_value("DisplayVersion", &VERSION)?;
    k.set_value("Publisher", &"Shorui")?;
    k.set_value("InstallLocation", &o.dir.display().to_string())?;
    k.set_value("DisplayIcon", &exe.display().to_string())?;
    k.set_value("UninstallString", &format!("\"{}\" --uninstall", uninstaller.display()))?;
    k.set_value("QuietUninstallString", &format!("\"{}\" --uninstall --silent", uninstaller.display()))?;
    k.set_value("NoModify", &1u32)?;
    k.set_value("NoRepair", &1u32)?;
    k.set_value("EstimatedSize", &((size / 1024) as u32))?;

    // "Open with" for PDFs. This offers Shorui; it does not take over the default.
    let (k, _) = hkcu.create_subkey(r"Software\Classes\Shorui.pdf")?;
    k.set_value("", &"PDF document")?;
    let (k, _) = hkcu.create_subkey(r"Software\Classes\Shorui.pdf\shell\open\command")?;
    k.set_value("", &open_cmd)?;
    let (k, _) = hkcu.create_subkey(r"Software\Classes\.pdf\OpenWithProgids")?;
    k.set_value("Shorui.pdf", &"")?;
    let (k, _) = hkcu.create_subkey(r"Software\Classes\Applications\shorui.exe\shell\open\command")?;
    k.set_value("", &open_cmd)?;
    let (k, _) = hkcu.create_subkey(r"Software\Classes\Applications\shorui.exe\SupportedTypes")?;
    k.set_value(".pdf", &"")?;

    // Lists Shorui on Windows' "Default apps" page, where the user can make it the default.
    let (k, _) = hkcu.create_subkey(r"Software\Shorui\Capabilities")?;
    k.set_value("ApplicationName", &"Shorui")?;
    k.set_value("ApplicationDescription", &"A fast PDF reader and editor.")?;
    let (k, _) = hkcu.create_subkey(r"Software\Shorui\Capabilities\FileAssociations")?;
    k.set_value(".pdf", &"Shorui.pdf")?;
    let (k, _) = hkcu.create_subkey(r"Software\RegisteredApplications")?;
    k.set_value("Shorui", &r"Software\Shorui\Capabilities")?;
    Ok(())
}

fn uninstall(o: &Opts, progress: &dyn Fn(&str, f32)) -> Result<(), String> {
    progress("Removing Shorui", 0.15);
    for name in ["shorui.exe", "pdfium.dll"] {
        let path = o.dir.join(name);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                if matches!(e.raw_os_error(), Some(32) | Some(5)) {
                    "Shorui is open. Close it and try again.".to_string()
                } else {
                    format!("Can't remove {}. {e}", path.display())
                }
            })?;
        }
    }
    if o.register {
        progress("Removing shortcuts", 0.6);
        for link in [start_menu_link(), desktop_link()].into_iter().flatten() {
            let _ = std::fs::remove_file(link);
        }
        progress("Cleaning up", 0.8);
        use winreg::{RegKey, enums::HKEY_CURRENT_USER};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        // Only keys this installer created.
        let _ = hkcu.delete_subkey_all(UNINSTALL_KEY);
        let _ = hkcu.delete_subkey_all(r"Software\Classes\Shorui.pdf");
        let _ = hkcu.delete_subkey_all(r"Software\Classes\Applications\shorui.exe");
        let _ = hkcu.delete_subkey_all(r"Software\Shorui");
        if let Ok(k) = hkcu.open_subkey_with_flags(r"Software\RegisteredApplications", winreg::enums::KEY_SET_VALUE) {
            let _ = k.delete_value("Shorui");
        }
        if let Ok(k) = hkcu.open_subkey_with_flags(r"Software\Classes\.pdf\OpenWithProgids", winreg::enums::KEY_SET_VALUE) {
            let _ = k.delete_value("Shorui.pdf");
        }
    }
    progress("Done", 1.0);
    Ok(())
}

/// A running exe can't delete itself, so a hidden helper removes the uninstaller a moment after
/// this process exits, then the folder if (and only if) nothing else is left in it.
fn remove_self_later(dir: &Path) {
    use std::os::windows::process::CommandExt;
    let uninstaller = dir.join("uninstall.exe");
    if !uninstaller.exists() {
        let _ = std::fs::remove_dir(dir);
        return;
    }
    let script = format!(
        "ping -n 2 127.0.0.1 >nul & del /q \"{}\" & rmdir \"{}\"",
        uninstaller.display(),
        dir.display()
    );
    let _ = std::process::Command::new("cmd")
        .raw_arg(format!("/c {script}"))
        .current_dir(std::env::temp_dir())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .spawn();
}

// ------------------------------------------------------------------ the window

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Install,
    Uninstall,
}

enum Screen {
    Welcome,
    Working,
    Done,
    Failed(String),
}

enum Msg {
    Step(&'static str, f32),
    Finished(Result<(), String>),
}

struct Setup {
    mode: Mode,
    opts: Opts,
    existing: bool,
    launch: bool,
    screen: Screen,
    /// When the window opened, and when the current screen was entered.
    born: Option<f64>,
    entered: f64,
    step: &'static str,
    target: f32,
    shown: f32,
    result: Option<Result<(), String>>,
    rx: Option<Receiver<Msg>>,
    removed: bool,
    shots: Option<(PathBuf, u32)>,
}

const W: f32 = 620.0;
const H: f32 = 392.0;

fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

fn fade(c: Color32, a: f32) -> Color32 {
    let a = a.clamp(0.0, 1.0);
    Color32::from_rgb((c.r() as f32 * a) as u8, (c.g() as f32 * a) as u8, (c.b() as f32 * a) as u8)
}

impl Setup {
    fn begin(&mut self, ctx: &egui::Context, now: f64) {
        let (tx, rx) = channel();
        let (opts, mode, ctx) = (self.opts.clone(), self.mode, ctx.clone());
        std::thread::spawn(move || {
            let progress = |step: &'static str, p: f32| {
                let _ = tx.send(Msg::Step(step, p));
                ctx.request_repaint();
            };
            // The step names are static strings, so adapt the callback signature.
            let cb = |s: &str, p: f32| {
                let s: &'static str = match s {
                    "Creating folder" => "Creating folder",
                    "Copying Shorui" => "Copying Shorui",
                    "Copying PDF engine" => "Copying PDF engine",
                    "Adding uninstaller" => "Adding uninstaller",
                    "Adding shortcuts" => "Adding shortcuts",
                    "Registering" => "Registering",
                    "Removing Shorui" => "Removing Shorui",
                    "Removing shortcuts" => "Removing shortcuts",
                    "Cleaning up" => "Cleaning up",
                    _ => "Finishing",
                };
                progress(s, p);
            };
            let result = match mode {
                Mode::Install => install(&opts, &cb),
                Mode::Uninstall => uninstall(&opts, &cb),
            };
            let _ = tx.send(Msg::Finished(result));
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.result = None;
        self.target = 0.0;
        self.shown = 0.0;
        self.step = "Starting";
        self.screen = Screen::Working;
        self.entered = now;
    }

    /// The right-hand artwork: the design system's sheets and dot grid, animated.
    fn art(&self, p: &egui::Painter, origin: Pos2, now: f64) {
        let since_born = (now - self.born.unwrap_or(now)) as f32;
        let since_screen = (now - self.entered) as f32;
        let working = matches!(self.screen, Screen::Working);
        let done = matches!(self.screen, Screen::Done);
        let removing = self.mode == Mode::Uninstall && (working || done);
        let progress = if done { 1.0 } else if working { self.shown } else { 0.0 };

        // x, y, w, h, colour, entrance delay, bob phase
        let sheets = [
            (28.0, 60.0, 132.0, 184.0, ACCENT, 0.00, 0.0),
            (180.0, 108.0, 112.0, 208.0, ACCENT_DEEP, 0.07, 1.7),
            (170.0, -24.0, 84.0, 108.0, ACCENT_BRIGHT, 0.14, 3.1),
            (52.0, 268.0, 84.0, 84.0, BG_300, 0.21, 4.4),
        ];
        let mut rects = [Rect::NOTHING; 4];
        for (i, &(x, y, w, h, color, delay, phase)) in sheets.iter().enumerate() {
            let enter = ease((since_born - delay) / 0.5);
            let bob = (now as f32 * 1.1 + phase).sin() * 3.0;
            // Uninstalling: sheets leave one after another as the work proceeds.
            let leave = if removing { ease((progress - i as f32 * 0.18) / 0.4) } else { 0.0 };
            let alpha = enter * (1.0 - leave);
            let rect = Rect::from_min_size(origin + vec2(x, y + (1.0 - enter) * 40.0 + bob - leave * 24.0), vec2(w, h));
            rects[i] = rect;
            if alpha > 0.01 {
                p.rect_filled(rect, CornerRadius::same(RADIUS_LG), fade(color, alpha));
            }
            if removing && leave > 0.0 {
                p.rect_stroke(rect, CornerRadius::same(RADIUS_LG), Stroke::new(1.0, fade(LINE_STRONG, leave * 0.7)), StrokeKind::Inside);
            }
        }

        // Dot grid on the deep sheet. Idle: a slow wave. Installing: dots light as files land.
        let grid = rects[1];
        let alpha = ease((since_born - 0.3) / 0.5) * if removing { 1.0 - ease(progress / 0.5) } else { 1.0 };
        let (cols, rows) = (5, 10);
        for r in 0..rows {
            for c in 0..cols {
                let i = r * cols + c;
                let wave = 0.38 + 0.2 * (now as f32 * 2.2 - (r as f32) * 0.55 - c as f32 * 0.3).sin();
                let lit = (working || done) && !removing && (i as f32) < progress * (cols * rows) as f32;
                let b = if lit { 1.0 } else { wave };
                let center = grid.min + vec2(24.0 + c as f32 * 16.0, 32.0 + r as f32 * 16.0);
                p.circle_filled(center, if lit { 2.6 } else { 2.0 }, fade(ACCENT_BRIGHT, b * alpha));
            }
        }

        // Installing: the main sheet fills from the bottom like a page being printed.
        let main = rects[0];
        if working && !removing && progress > 0.0 {
            let fill = Rect::from_min_max(pos2(main.min.x, main.max.y - main.height() * progress), main.max);
            p.with_clip_rect(fill).rect_filled(main, CornerRadius::same(RADIUS_LG), ACCENT_BRIGHT);
        }

        // Done: a check draws itself and a ring opens out.
        if done && self.mode == Mode::Install {
            let c = main.center();
            let ring = ease(since_screen / 0.7);
            if ring < 1.0 {
                p.circle_stroke(c, 34.0 + ring * 70.0, Stroke::new(2.0, fade(ACCENT_BRIGHT, 1.0 - ring)));
            }
            let pts = [c + vec2(-24.0, 2.0), c + vec2(-7.0, 19.0), c + vec2(26.0, -18.0)];
            let (l1, l2) = ((pts[1] - pts[0]).length(), (pts[2] - pts[1]).length());
            let drawn = ease(since_screen / 0.38) * (l1 + l2);
            let stroke = Stroke::new(5.0, ON_ACCENT);
            if drawn > 0.0 {
                let end = pts[0] + (pts[1] - pts[0]) * (drawn / l1).min(1.0);
                p.line_segment([pts[0], end], stroke);
                p.circle_filled(pts[0], 2.5, ON_ACCENT);
                p.circle_filled(end, 2.5, ON_ACCENT);
            }
            if drawn > l1 {
                let end = pts[1] + (pts[2] - pts[1]) * ((drawn - l1) / l2).min(1.0);
                p.line_segment([pts[1], end], stroke);
                p.circle_filled(end, 2.5, ON_ACCENT);
            }
        }
    }

    fn close(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).font(theme::label()).size(13.0).color(ON_ACCENT)).fill(ACCENT)
}

fn plain(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_owned()).font(theme::label()).size(13.0))
}

impl eframe::App for Setup {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let (now, dt) = ctx.input(|i| (i.time, i.stable_dt.min(0.05)));
        if self.born.is_none() {
            self.born = Some(now);
            self.entered = now;
        }
        // Everything here moves, so paint continuously while the window is up.
        ctx.request_repaint();

        // Progress from the worker.
        if let Some(rx) = &self.rx {
            while let Ok(m) = rx.try_recv() {
                match m {
                    Msg::Step(step, p) => {
                        self.step = step;
                        self.target = p;
                    }
                    Msg::Finished(r) => {
                        if r.is_ok() {
                            self.target = 1.0;
                        }
                        self.result = Some(r);
                    }
                }
            }
        }
        if matches!(self.screen, Screen::Working) {
            // The bar glides to the real progress instead of jumping.
            self.shown += (self.target - self.shown) * (dt * 9.0).min(1.0);
            self.shown = (self.shown + dt * 0.35).min(self.target);
            match &self.result {
                Some(Err(e)) => {
                    self.screen = Screen::Failed(e.clone());
                    self.entered = now;
                }
                Some(Ok(())) if self.shown > 0.995 => {
                    self.screen = Screen::Done;
                    self.entered = now;
                    self.removed = self.mode == Mode::Uninstall;
                }
                _ => {}
            }
        }

        let full = Rect::from_min_size(Pos2::ZERO, vec2(W, H));
        let p = ui.painter().clone();
        p.rect_filled(full, 0, BG_000);
        self.art(&p.with_clip_rect(full.shrink(1.0)), pos2(312.0, 0.0), now);
        p.rect_stroke(full, 0, Stroke::new(1.0, LINE), StrokeKind::Inside);

        // Drag the window by its top strip; close with the button.
        let strip = ui.interact(Rect::from_min_size(Pos2::ZERO, vec2(W - 48.0, 44.0)), Id::unique("drag"), Sense::drag());
        if strip.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let close_rect = Rect::from_min_size(pos2(W - 42.0, 10.0), vec2(32.0, 32.0));
        let closing = ui.scope_builder(egui::UiBuilder::new().max_rect(close_rect), |ui| {
            theme::icon_button(ui, Icon::Close, "Close", true, false).clicked()
        });
        if closing.inner && !matches!(self.screen, Screen::Working) {
            self.close(&ctx);
        }

        // Text column. It slides up a few pixels each time the screen changes.
        let t = ease(((now - self.entered) as f32) / 0.3);
        let column = Rect::from_min_max(pos2(36.0, 52.0 + (1.0 - t) * 10.0), pos2(304.0, H - SPACE_6));
        let ink = fade(INK, t);
        let muted = fade(INK_MUTED, t);
        let mut start = false;
        let mut switch_to_uninstall = false;
        let mut finish = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(column), |ui| {
            ui.spacing_mut().item_spacing.y = SPACE_2;
            let heading = |ui: &mut Ui, text: &str, size: f32| {
                ui.label(RichText::new(text).font(theme::display()).size(size).color(ink));
            };
            match &self.screen {
                Screen::Welcome => {
                    if self.mode == Mode::Install {
                        heading(ui, "Shorui", 52.0);
                        ui.label(RichText::new("A PDF reader that opens before you finish clicking.").font(theme::body()).color(muted));
                        ui.add_space(SPACE_3);
                        ui.checkbox(&mut self.opts.desktop, RichText::new("Add a desktop shortcut").font(theme::body()));
                        ui.checkbox(&mut self.launch, RichText::new("Open Shorui when done").font(theme::body()));
                        ui.add_space(SPACE_3);
                        ui.horizontal(|ui| {
                            let label = if self.existing { "Update" } else { "Install" };
                            if ui.add_sized([112.0, 36.0], primary(label)).clicked() {
                                start = true;
                            }
                            if self.existing && ui.add_sized([96.0, 36.0], plain("Uninstall")).clicked() {
                                switch_to_uninstall = true;
                            }
                        });
                        ui.add_space(SPACE_2);
                        let note = format!("Version {VERSION}. Installs for you only, to\n{}", self.opts.dir.display());
                        ui.label(RichText::new(note).font(theme::caption()).color(muted));
                    } else {
                        heading(ui, "Remove Shorui?", 32.0);
                        ui.label(RichText::new("This removes the app, its shortcuts and its \"Open with\" entry. Your PDFs are not touched.").font(theme::body()).color(muted));
                        ui.add_space(SPACE_4);
                        ui.horizontal(|ui| {
                            if ui.add_sized([112.0, 36.0], primary("Uninstall")).clicked() {
                                start = true;
                            }
                            if ui.add_sized([96.0, 36.0], plain("Cancel")).clicked() {
                                finish = true;
                            }
                        });
                    }
                }
                Screen::Working => {
                    heading(ui, if self.mode == Mode::Install { "Installing" } else { "Removing" }, 32.0);
                    ui.label(RichText::new(self.step).font(theme::body()).color(muted));
                    ui.add_space(SPACE_4);
                    let (bar, _) = ui.allocate_exact_size(vec2(240.0, 6.0), Sense::hover());
                    ui.painter().rect_filled(bar, CornerRadius::same(3), LINE);
                    let fill = Rect::from_min_size(bar.min, vec2(bar.width() * self.shown, bar.height()));
                    ui.painter().rect_filled(fill, CornerRadius::same(3), ACCENT);
                    ui.label(RichText::new(format!("{:.0}%", self.shown * 100.0)).font(theme::readout()).color(muted));
                }
                Screen::Done => {
                    if self.mode == Mode::Install {
                        heading(ui, "Installed.", 40.0);
                        ui.label(RichText::new("Shorui is in your Start menu, and in \"Open with\" for PDFs.").font(theme::body()).color(muted));
                    } else {
                        heading(ui, "Removed.", 40.0);
                        ui.label(RichText::new("Shorui is gone from this PC.").font(theme::body()).color(muted));
                    }
                    ui.add_space(SPACE_4);
                    if self.mode == Mode::Install && self.opts.register {
                        if ui.link(RichText::new("Make Shorui the default PDF app").font(theme::body())).clicked() {
                            let _ = std::process::Command::new("explorer.exe")
                                .arg("ms-settings:defaultapps?registeredAppUser=Shorui")
                                .spawn();
                        }
                        ui.add_space(SPACE_2);
                    }
                    ui.horizontal(|ui| {
                        if self.mode == Mode::Install {
                            if ui.add_sized([128.0, 36.0], primary("Open Shorui")).clicked() {
                                self.launch = true;
                                finish = true;
                            }
                            if ui.add_sized([96.0, 36.0], plain("Close")).clicked() {
                                self.launch = false;
                                finish = true;
                            }
                        } else if ui.add_sized([112.0, 36.0], primary("Close")).clicked() {
                            finish = true;
                        }
                    });
                }
                Screen::Failed(e) => {
                    heading(ui, if self.mode == Mode::Install { "Can't install." } else { "Can't remove." }, 32.0);
                    ui.label(RichText::new(e).font(theme::body()).color(DANGER));
                    ui.add_space(SPACE_4);
                    ui.horizontal(|ui| {
                        if ui.add_sized([112.0, 36.0], primary("Try again")).clicked() {
                            start = true;
                        }
                        if ui.add_sized([96.0, 36.0], plain("Close")).clicked() {
                            self.launch = false;
                            finish = true;
                        }
                    });
                }
            }
        });

        if switch_to_uninstall {
            self.mode = Mode::Uninstall;
            self.entered = now;
        }
        if start {
            self.begin(&ctx, now);
        }
        if finish {
            self.close(&ctx);
        }

        // Scripted run for checking the installer: SHORUI_SETUP_SHOT=<folder>.
        if let Some((dir, phase)) = &mut self.shots {
            let shot = |name: &str| {
                let path = dir.join(name);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                    let _ = write_bmp(&path, &img);
                })));
            };
            let age = now - self.entered;
            let prefix = if self.mode == Mode::Install { "setup" } else { "remove" };
            match (*phase, &self.screen) {
                (0, Screen::Welcome) if age > 1.2 => {
                    shot(&format!("{prefix}-1-welcome.bmp"));
                    *phase = 1;
                }
                (1, Screen::Welcome) if age > 1.4 => {
                    *phase = 2;
                    self.launch = false;
                    self.begin(&ctx, now);
                }
                (2, Screen::Working) if self.shown > 0.45 => {
                    shot(&format!("{prefix}-2-working.bmp"));
                    *phase = 3;
                }
                (2 | 3, Screen::Done) if age > 1.0 => {
                    shot(&format!("{prefix}-3-done.bmp"));
                    *phase = 4;
                }
                (4, _) if age > 1.3 => self.close(&ctx),
                (_, Screen::Failed(e)) => {
                    let _ = std::fs::write(dir.join(format!("{prefix}-error.txt")), e);
                    self.close(&ctx);
                }
                _ => {}
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.removed {
            remove_self_later(&self.opts.dir);
        } else if self.launch && matches!(self.screen, Screen::Done) && self.mode == Mode::Install {
            let _ = std::process::Command::new(self.opts.dir.join("shorui.exe")).current_dir(&self.opts.dir).spawn();
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

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a.eq_ignore_ascii_case(flag));
    let named_uninstall = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .is_some_and(|s| s.starts_with("uninstall"));
    let mode = if has("--uninstall") || named_uninstall { Mode::Uninstall } else { Mode::Install };
    let dir = args
        .iter()
        .position(|a| a == "--dir")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .or_else(|| {
            // An uninstaller removes the folder it lives in.
            (mode == Mode::Uninstall && named_uninstall)
                .then(|| std::env::current_exe().ok()?.parent().map(Path::to_path_buf))
                .flatten()
        })
        .unwrap_or_else(default_dir);
    let opts = Opts { dir, register: !has("--no-register"), desktop: false };

    if has("--silent") || has("/S") {
        let result = match mode {
            Mode::Install => install(&opts, &|_, _| {}),
            Mode::Uninstall => uninstall(&opts, &|_, _| {}).map(|_| remove_self_later(&opts.dir)),
        };
        std::process::exit(if result.is_ok() { 0 } else { 1 });
    }

    let existing = opts.dir.join("shorui.exe").exists();
    let shots = std::env::var_os("SHORUI_SETUP_SHOT").map(|d| (PathBuf::from(d), 0));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Shorui Setup")
            .with_inner_size([W, H])
            .with_resizable(false)
            .with_decorations(false)
            .with_icon(egui::IconData { rgba: include_bytes!("../../assets/icon-64.rgba").to_vec(), width: 64, height: 64 }),
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Shorui Setup",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Setup {
                mode,
                opts,
                existing,
                launch: true,
                screen: Screen::Welcome,
                born: None,
                entered: 0.0,
                step: "",
                target: 0.0,
                shown: 0.0,
                result: None,
                rx: None,
                removed: false,
                shots,
            }))
        }),
    )
}
