//! Saved preferences, and the update check against GitHub releases.
//!
//! Settings live in `%APPDATA%\Shorui\settings.txt` as `key=value` lines. The update check is
//! the only time Shorui touches the network: one request to the public GitHub API, made with
//! the `curl.exe` that ships with Windows, sending nothing but a generic user agent.

use std::{
    os::windows::process::CommandExt,
    path::PathBuf,
    process::Command,
    sync::mpsc::Sender,
    time::{SystemTime, UNIX_EPOCH},
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const REPO: &str = "invictus-1/Shorui";
pub const REPO_URL: &str = "https://github.com/invictus-1/Shorui";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Highlight colours offered in Settings: name and RGB.
pub const HIGHLIGHTS: [(&str, [u8; 3]); 4] =
    [("Yellow", [255, 216, 77]), ("Green", [134, 239, 172]), ("Pink", [249, 168, 212]), ("Purple", [196, 181, 253])];

#[derive(Clone, Copy, PartialEq)]
pub enum OpenZoom {
    FitWidth,
    FitPage,
    Actual,
}

#[derive(Clone, PartialEq)]
pub struct Settings {
    pub open_zoom: OpenZoom,
    pub sidebar: bool,
    pub auto_update: bool,
    pub highlight: usize,
    /// Unix time of the last automatic update check.
    pub last_check: u64,
    pub restore_session: bool,
    pub two_page: bool,
    /// Most recently opened files, newest first.
    pub recent: Vec<PathBuf>,
    /// The tabs open when Shorui last closed: file, page and zoom.
    pub session: Vec<(PathBuf, u32, f32)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { open_zoom: OpenZoom::FitWidth, sidebar: true, auto_update: true,
            highlight: 0,
            last_check: 0,
            restore_session: true,
            two_page: false,
            recent: Vec::new(),
            session: Vec::new(),
        }
    }
}

fn path() -> Option<PathBuf> {
    // SHORUI_CONFIG_DIR keeps settings somewhere else: a portable copy, or a test run.
    if let Some(dir) = std::env::var_os("SHORUI_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("settings.txt"));
    }
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("Shorui").join("settings.txt"))
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            match (key.trim(), value.trim()) {
                ("open_zoom", "fit_width") => s.open_zoom = OpenZoom::FitWidth,
                ("open_zoom", "fit_page") => s.open_zoom = OpenZoom::FitPage,
                ("open_zoom", "actual") => s.open_zoom = OpenZoom::Actual,
                ("sidebar", v) => s.sidebar = v == "true",
                ("auto_update", v) => s.auto_update = v == "true",
                ("highlight", v) => s.highlight = v.parse().unwrap_or(0).min(HIGHLIGHTS.len() - 1),
                ("last_check", v) => s.last_check = v.parse().unwrap_or(0),
                ("restore_session", v) => s.restore_session = v == "true",
                ("two_page", v) => s.two_page = v == "true",
                ("recent", v) if !v.is_empty() => s.recent.push(PathBuf::from(v)),
                ("session", v) => {
                    // file|page|zoom. '|' can't appear in a Windows path.
                    let mut parts = v.rsplitn(3, '|');
                    let zoom = parts.next().and_then(|z| z.parse().ok()).unwrap_or(1.0);
                    let page = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
                    if let Some(path) = parts.next().filter(|p| !p.is_empty()) {
                        s.session.push((PathBuf::from(path), page, zoom));
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let zoom = match self.open_zoom {
            OpenZoom::FitWidth => "fit_width",
            OpenZoom::FitPage => "fit_page",
            OpenZoom::Actual => "actual",
        };
        let text = format!(
            "open_zoom={zoom}\nsidebar={}\nauto_update={}\nhighlight={}\nlast_check={}\n",
            self.sidebar, self.auto_update, self.highlight, self.last_check
        );
        let mut text = text;
        text += &format!("restore_session={}\ntwo_page={}\n", self.restore_session, self.two_page);
        for r in &self.recent {
            text += &format!("recent={}\n", r.display());
        }
        for (path, page, zoom) in &self.session {
            text += &format!("session={}|{page}|{zoom}\n", path.display());
        }
        let _ = std::fs::write(p, text);
    }

    /// Puts a file at the top of the recent list.
    pub fn remember(&mut self, path: &std::path::Path) {
        self.recent.retain(|r| r != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(10);
    }

    pub fn highlight_rgb(&self) -> [u8; 3] {
        HIGHLIGHTS[self.highlight.min(HIGHLIGHTS.len() - 1)].1
    }
}

/// Opens Windows Settings on Shorui's default-apps page. Windows does not allow an app to make
/// itself the default; the user confirms there.
pub fn open_default_apps() {
    let _ = Command::new("explorer.exe").arg("ms-settings:defaultapps?registeredAppUser=Shorui").spawn();
}

pub fn open_url(url: &str) {
    let _ = Command::new("explorer.exe").arg(url).spawn();
}

// ------------------------------------------------------------------ updates

#[derive(Clone, PartialEq)]
pub enum Update {
    Idle,
    Checking,
    UpToDate,
    Available { version: String, url: String },
    Downloading { version: String },
    /// The installer has been started; the app should close so it can replace the files.
    Installing,
    Failed(String),
}

fn curl() -> Command {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    let mut c = Command::new(root.join("System32").join("curl.exe"));
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// The string value of `"key": "value"` at or after `from`.
fn json_str(text: &str, key: &str, from: usize) -> Option<(String, usize)> {
    let at = text[from..].find(&format!("\"{key}\""))? + from;
    let colon = text[at..].find(':')? + at;
    let open = text[colon..].find('"')? + colon + 1;
    let close = text[open..].find('"')? + open;
    Some((text[open..close].to_string(), close))
}

fn parse_version(v: &str) -> Vec<u32> {
    v.trim_start_matches(['v', 'V']).split('.').map(|p| p.trim().parse().unwrap_or(0)).collect()
}

/// Asks GitHub for the newest release and compares it with `current`.
pub fn latest_release(current: &str) -> Update {
    let out = curl()
        .args(["-sS", "-L", "--max-time", "12", "-H", "Accept: application/vnd.github+json"])
        .args(["-A", &format!("Shorui/{VERSION}")])
        .arg(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .output();
    let out = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(_) => return Update::Failed("Can't reach GitHub.".into()),
        Err(_) => return Update::Failed("Can't run curl.exe.".into()),
    };
    let Some((tag, _)) = json_str(&out, "tag_name", 0) else {
        // No release published yet, or an API error body.
        return if out.contains("Not Found") { Update::UpToDate } else { Update::Failed("GitHub gave no answer.".into()) };
    };
    if parse_version(&tag) <= parse_version(current) {
        return Update::UpToDate;
    }
    // The installer is the release asset whose download link ends in .exe.
    let mut from = 0;
    while let Some((url, next)) = json_str(&out, "browser_download_url", from) {
        from = next;
        if url.to_lowercase().ends_with(".exe") && url.starts_with(&format!("{REPO_URL}/releases/download/")) {
            return Update::Available { version: tag.trim_start_matches('v').to_string(), url };
        }
    }
    Update::Failed(format!("Version {tag} has no installer attached."))
}

pub fn check(tx: Sender<Update>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let _ = tx.send(latest_release(VERSION));
        ctx.request_repaint();
    });
}

/// Downloads the installer to the temp folder and starts it.
pub fn download_and_run(version: String, url: String, tx: Sender<Update>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let target = std::env::temp_dir().join(format!("Shorui-Setup-{version}.exe"));
        let ok = curl()
            .args(["-sS", "-L", "--fail", "--max-time", "300", "-A", &format!("Shorui/{VERSION}"), "-o"])
            .arg(&target)
            .arg(&url)
            .status()
            .is_ok_and(|s| s.success());
        let result = if !ok {
            Update::Failed("The download failed.".into())
        } else if Command::new(&target).spawn().is_err() {
            Update::Failed("Can't start the installer.".into())
        } else {
            Update::Installing
        };
        let _ = tx.send(result);
        ctx.request_repaint();
    });
}
