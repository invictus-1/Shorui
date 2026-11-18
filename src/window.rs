//! Remembers where the window was: its normal size and position, and whether it was maximized.
//!
//! Windows' own window placement is used, because it keeps the normal size and position even
//! while the window is maximized or minimized, so un-maximizing after a restart goes back to
//! the right place.

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromRect},
    UI::WindowsAndMessaging::{
        GetWindowPlacement, SW_HIDE, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED, SetWindowPlacement, WINDOWPLACEMENT,
        WPF_RESTORETOMAXIMIZED,
    },
};

/// The window's normal bounds in screen pixels (left, top, right, bottom) and whether it was
/// maximized.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Placement {
    pub rect: [i32; 4],
    pub maximized: bool,
}

impl Placement {
    pub fn to_setting(self) -> String {
        let [l, t, r, b] = self.rect;
        format!("{l}|{t}|{r}|{b}|{}", self.maximized)
    }

    pub fn from_setting(v: &str) -> Option<Self> {
        let p: Vec<&str> = v.split('|').collect();
        if p.len() != 5 {
            return None;
        }
        let n = |i: usize| p[i].trim().parse::<i32>().ok();
        Some(Self { rect: [n(0)?, n(1)?, n(2)?, n(3)?], maximized: p[4].trim() == "true" })
    }
}

pub fn hwnd(frame: &eframe::Frame) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match frame.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

fn placement(hwnd: isize) -> Option<WINDOWPLACEMENT> {
    let mut wp = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(HWND(hwnd as *mut _), &mut wp) }.ok()?;
    Some(wp)
}

/// Where the window is now. A minimized window reports the state it will return to.
pub fn read(hwnd: isize) -> Option<Placement> {
    let wp = placement(hwnd)?;
    let r = wp.rcNormalPosition;
    let maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32
        || (wp.showCmd == SW_SHOWMINIMIZED.0 as u32 && wp.flags.contains(WPF_RESTORETOMAXIMIZED));
    Some(Placement { rect: [r.left, r.top, r.right, r.bottom], maximized })
}

/// Puts a window that hasn't been shown yet back where it was. A normal window stays hidden
/// (the app shows it once its first frame is drawn); a maximized one is shown maximized.
pub fn restore(hwnd: isize, p: Placement) {
    let Some(mut wp) = placement(hwnd) else { return };
    let [l, t, r, b] = p.rect;
    // Only reuse the position if the title bar lands on a monitor: one may have been unplugged.
    let bar = RECT { left: l, top: t, right: r, bottom: t + 32 };
    let on_screen = !unsafe { MonitorFromRect(&bar, MONITOR_DEFAULTTONULL) }.is_invalid();
    if on_screen && r - l >= 300 && b - t >= 200 {
        wp.rcNormalPosition = RECT { left: l, top: t, right: r, bottom: b };
    }
    wp.showCmd = if p.maximized { SW_SHOWMAXIMIZED.0 as u32 } else { SW_HIDE.0 as u32 };
    let _ = unsafe { SetWindowPlacement(HWND(hwnd as *mut _), &wp) };
}
