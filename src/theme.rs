//! Shorui design tokens (see the Shorui Design System) and the egui style built from them.

use std::sync::Arc;

use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Painter, Pos2, Rect, Stroke, TextStyle,
    Vec2, pos2, vec2,
};

pub const BG_000: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
pub const BG_100: Color32 = Color32::from_rgb(0x0b, 0x0a, 0x0f);
pub const BG_200: Color32 = Color32::from_rgb(0x14, 0x12, 0x1b);
pub const BG_300: Color32 = Color32::from_rgb(0x1e, 0x1b, 0x29);
pub const LINE: Color32 = Color32::from_rgb(0x2c, 0x28, 0x39);
pub const LINE_STRONG: Color32 = Color32::from_rgb(0x74, 0x6d, 0x90);
pub const INK: Color32 = Color32::from_rgb(0xf2, 0xef, 0xfa);
pub const INK_MUTED: Color32 = Color32::from_rgb(0xa6, 0x9f, 0xbd);
pub const ACCENT: Color32 = Color32::from_rgb(0x7c, 0x3a, 0xed);
pub const ON_ACCENT: Color32 = Color32::WHITE;
pub const ACCENT_BRIGHT: Color32 = Color32::from_rgb(0xa7, 0x8b, 0xfa);
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x2e, 0x10, 0x65);
pub const PAGE: Color32 = Color32::WHITE;
pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x6b, 0x81);

pub const SPACE_1: f32 = 4.0;
pub const SPACE_2: f32 = 8.0;
pub const SPACE_3: f32 = 12.0;
pub const SPACE_4: f32 = 16.0;
pub const SPACE_6: f32 = 24.0;

pub const RADIUS_SM: u8 = 4;
pub const RADIUS_MD: u8 = 8;
pub const RADIUS_LG: u8 = 12;

const SEMIBOLD: &str = "semibold";

pub fn body() -> FontId {
    FontId::new(14.0, FontFamily::Proportional)
}
pub fn label() -> FontId {
    FontId::new(12.0, FontFamily::Name(SEMIBOLD.into()))
}
pub fn caption() -> FontId {
    FontId::new(11.0, FontFamily::Proportional)
}
pub fn readout() -> FontId {
    FontId::new(12.0, FontFamily::Monospace)
}
pub fn title() -> FontId {
    FontId::new(20.0, FontFamily::Name(SEMIBOLD.into()))
}
pub fn display() -> FontId {
    FontId::new(32.0, FontFamily::Name(SEMIBOLD.into()))
}

/// Installs the system fonts and the dark theme. Nothing is downloaded or bundled.
pub fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR").map(std::path::PathBuf::from).unwrap_or_else(|| "C:\\Windows".into());
    let dir = dir.join("Fonts");
    let mut load = |name: &str, files: &[&str]| -> bool {
        for f in files {
            if let Ok(bytes) = std::fs::read(dir.join(f)) {
                fonts.font_data.insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
                return true;
            }
        }
        false
    };
    let has_sans = load("sans", &["segoeui.ttf"]);
    let has_semi = load(SEMIBOLD, &["seguisb.ttf", "segoeuib.ttf"]);
    let has_mono = load("mono", &["CascadiaMono.ttf", "consola.ttf"]);
    // CJK and symbol fallback so file names in any script render in tabs.
    let has_cjk = load("cjk", &["YuGothR.ttc", "msgothic.ttc", "msyh.ttc"]);

    let mut prop = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut mono = fonts.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    if has_cjk {
        prop.push("cjk".to_owned());
        mono.push("cjk".to_owned());
    }
    let mut semi = prop.clone();
    if has_sans {
        prop.insert(0, "sans".to_owned());
    }
    if has_semi {
        semi.insert(0, SEMIBOLD.to_owned());
    } else if has_sans {
        semi.insert(0, "sans".to_owned());
    }
    if has_mono {
        mono.insert(0, "mono".to_owned());
    }
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Monospace, mono);
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), semi);
    ctx.set_fonts(fonts);

    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, title()),
            (TextStyle::Body, body()),
            (TextStyle::Button, label()),
            (TextStyle::Small, caption()),
            (TextStyle::Monospace, readout()),
        ]
        .into();
        style.spacing.item_spacing = vec2(SPACE_2, SPACE_2);
        style.spacing.button_padding = vec2(SPACE_3, 6.0);
        style.spacing.interact_size = vec2(32.0, 28.0);
        style.spacing.menu_margin = egui::Margin::same(SPACE_1 as i8);
        style.spacing.window_margin = egui::Margin::same(SPACE_6 as i8);
        // Motion: nothing eases. See the design system's Motion rules.
        style.animation_time = 0.0;

        let v = &mut style.visuals;
        v.dark_mode = true;
        v.override_text_color = Some(INK);
        v.panel_fill = BG_100;
        v.window_fill = BG_200;
        v.window_stroke = Stroke::new(1.0, LINE);
        v.window_corner_radius = CornerRadius::same(RADIUS_LG);
        v.menu_corner_radius = CornerRadius::same(RADIUS_LG);
        v.extreme_bg_color = BG_300;
        v.faint_bg_color = BG_200;
        v.hyperlink_color = ACCENT_BRIGHT;
        v.selection.bg_fill = ACCENT_DEEP;
        v.selection.stroke = Stroke::new(1.0, ACCENT_BRIGHT);
        let shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(153) };
        v.window_shadow = shadow;
        v.popup_shadow = shadow;

        let r = CornerRadius::same(RADIUS_MD);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG_200;
        w.noninteractive.weak_bg_fill = BG_200;
        w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
        w.noninteractive.fg_stroke = Stroke::new(1.0, INK_MUTED);
        w.noninteractive.corner_radius = r;
        w.inactive.bg_fill = BG_300;
        w.inactive.weak_bg_fill = BG_300;
        w.inactive.bg_stroke = Stroke::new(1.0, LINE_STRONG);
        w.inactive.fg_stroke = Stroke::new(1.0, INK);
        w.inactive.corner_radius = r;
        w.inactive.expansion = 0.0;
        w.hovered.bg_fill = BG_300;
        w.hovered.weak_bg_fill = Color32::from_rgb(0x2a, 0x26, 0x3a);
        w.hovered.bg_stroke = Stroke::new(1.0, LINE_STRONG);
        w.hovered.fg_stroke = Stroke::new(1.0, INK);
        w.hovered.corner_radius = r;
        w.hovered.expansion = 0.0;
        w.active.bg_fill = ACCENT_DEEP;
        w.active.weak_bg_fill = ACCENT_DEEP;
        w.active.bg_stroke = Stroke::new(1.0, ACCENT_BRIGHT);
        w.active.fg_stroke = Stroke::new(1.0, INK);
        w.active.corner_radius = r;
        w.active.expansion = 0.0;
        w.open = w.hovered;
    });
}

/// The surface for dialogs: raised, hairline edge, large radius, the one shadow.
pub fn modal_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(BG_200)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(CornerRadius::same(RADIUS_LG))
        .inner_margin(egui::Margin::same(SPACE_6 as i8))
        .shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(153) })
}

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Sidebar,
    Open,
    Save,
    Minus,
    Plus,
    Close,
    FitWidth,
    FitPage,
    RotateCw,
    RotateCcw,
    Trash,
    Search,
    Up,
    Down,
    Highlighter,
    Gear,
}

/// Draws a 16px outline icon centred on `c` with a 1.5px stroke.
pub fn paint_icon(p: &Painter, c: Pos2, icon: Icon, color: Color32) {
    let s = Stroke::new(1.5, color);
    let at = |x: f32, y: f32| pos2(c.x + x, c.y + y);
    let seg = |a: (f32, f32), b: (f32, f32)| {
        p.line_segment([at(a.0, a.1), at(b.0, b.1)], s);
    };
    let poly = |pts: &[(f32, f32)]| {
        p.line(pts.iter().map(|&(x, y)| at(x, y)).collect(), s);
    };
    let outline = |min: (f32, f32), max: (f32, f32), r: u8| {
        p.rect_stroke(
            Rect::from_min_max(at(min.0, min.1), at(max.0, max.1)),
            CornerRadius::same(r),
            s,
            egui::StrokeKind::Middle,
        );
    };
    match icon {
        Icon::Sidebar => {
            outline((-7.0, -5.5), (7.0, 5.5), 2);
            seg((-2.5, -5.5), (-2.5, 5.5));
        }
        Icon::Open => {
            poly(&[(-7.0, 5.5), (-7.0, -5.5), (-2.5, -5.5), (-0.5, -3.0), (7.0, -3.0), (7.0, 5.5), (-7.0, 5.5)]);
        }
        Icon::Save => {
            seg((0.0, -6.5), (0.0, 2.5));
            poly(&[(-3.5, -1.0), (0.0, 2.5), (3.5, -1.0)]);
            poly(&[(-6.5, 3.0), (-6.5, 6.0), (6.5, 6.0), (6.5, 3.0)]);
        }
        Icon::Minus => seg((-5.0, 0.0), (5.0, 0.0)),
        Icon::Plus => {
            seg((-5.0, 0.0), (5.0, 0.0));
            seg((0.0, -5.0), (0.0, 5.0));
        }
        Icon::Close => {
            seg((-3.5, -3.5), (3.5, 3.5));
            seg((-3.5, 3.5), (3.5, -3.5));
        }
        Icon::FitWidth => {
            seg((-7.0, -6.0), (-7.0, 6.0));
            seg((7.0, -6.0), (7.0, 6.0));
            seg((-4.0, 0.0), (4.0, 0.0));
            poly(&[(-1.5, -2.5), (-4.0, 0.0), (-1.5, 2.5)]);
            poly(&[(1.5, -2.5), (4.0, 0.0), (1.5, 2.5)]);
        }
        Icon::FitPage => {
            outline((-5.0, -7.0), (5.0, 7.0), 1);
            seg((-2.0, -3.0), (2.0, -3.0));
            seg((-2.0, 0.0), (2.0, 0.0));
            seg((-2.0, 3.0), (0.5, 3.0));
        }
        Icon::RotateCw | Icon::RotateCcw => {
            let flip = if icon == Icon::RotateCw { 1.0 } else { -1.0 };
            // Three-quarter arc ending at the top, with an arrow head.
            let r = 5.5;
            let pts: Vec<Pos2> = (0..=18)
                .map(|i| {
                    let a = std::f32::consts::PI * (0.0 + 1.5 * i as f32 / 18.0);
                    at(flip * r * a.sin() * -1.0, r * a.cos() * -1.0 + 0.5)
                })
                .collect();
            let end = pts[0];
            p.line(pts, s);
            p.line(
                vec![
                    pos2(end.x + flip * 0.5, end.y - 3.5),
                    pos2(end.x + flip * 3.5, end.y),
                    pos2(end.x + flip * 0.5, end.y + 3.5),
                ],
                s,
            );
        }
        Icon::Search => {
            p.circle_stroke(at(-1.5, -1.5), 4.5, s);
            seg((2.0, 2.0), (6.5, 6.5));
        }
        Icon::Up => poly(&[(-4.5, 2.5), (0.0, -2.5), (4.5, 2.5)]),
        Icon::Down => poly(&[(-4.5, -2.5), (0.0, 2.5), (4.5, -2.5)]),
        Icon::Gear => {
            p.circle_stroke(c, 2.4, s);
            p.circle_stroke(c, 5.2, s);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (dx, dy) = (a.cos(), a.sin());
                seg((dx * 5.2, dy * 5.2), (dx * 7.4, dy * 7.4));
            }
        }
        Icon::Highlighter => {
            // A slanted marker with a stroke of ink under it.
            poly(&[(-1.5, 3.0), (-4.0, 0.5), (2.5, -6.0), (5.0, -3.5), (-1.5, 3.0)]);
            poly(&[(-4.0, 0.5), (-5.0, 4.0), (-1.5, 3.0)]);
            seg((-6.5, 6.5), (6.5, 6.5));
        }
        Icon::Trash => {
            seg((-6.5, -4.0), (6.5, -4.0));
            poly(&[(-2.5, -4.0), (-2.5, -6.5), (2.5, -6.5), (2.5, -4.0)]);
            poly(&[(-5.0, -4.0), (-4.0, 6.5), (4.0, 6.5), (5.0, -4.0)]);
            seg((-1.5, -1.0), (-1.5, 3.5));
            seg((1.5, -1.0), (1.5, 3.5));
        }
    }
}

/// A 32px square icon button. Returns the response; hover shows `tip`.
pub fn icon_button(ui: &mut egui::Ui, icon: Icon, tip: &str, enabled: bool, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), egui::Sense::click());
    let hovered = enabled && resp.hovered();
    let p = ui.painter();
    if active {
        p.rect_filled(rect, CornerRadius::same(RADIUS_MD), ACCENT_DEEP);
    } else if hovered {
        p.rect_filled(rect, CornerRadius::same(RADIUS_MD), BG_300);
    }
    if resp.has_focus() {
        p.rect_stroke(rect, CornerRadius::same(RADIUS_MD), Stroke::new(2.0, ACCENT_BRIGHT), egui::StrokeKind::Outside);
    }
    let color = if !enabled {
        LINE_STRONG
    } else if hovered || active {
        INK
    } else {
        INK_MUTED
    };
    paint_icon(p, rect.center(), icon, color);
    let resp = resp.on_hover_text(tip);
    if enabled { resp } else { resp.on_hover_cursor(egui::CursorIcon::Default) }
}
