//! Dark, sharp developer theme and 256-color ANSI palette.

use egui::{Color32, CornerRadius, Stroke, Style, Visuals};
use pane_proto::Color;

pub const BG_CANVAS: Color32 = Color32::from_rgb(10, 13, 18);
pub const BG_HUD: Color32 = Color32::from_rgb(14, 18, 26);
pub const BG_PANEL: Color32 = Color32::from_rgb(17, 22, 31);
pub const BG_TERM: Color32 = Color32::from_rgb(11, 14, 20);
pub const BG_ELEVATED: Color32 = Color32::from_rgb(24, 31, 43);
pub const BORDER: Color32 = Color32::from_rgb(36, 46, 63);
pub const BORDER_FOCUS: Color32 = Color32::from_rgb(56, 189, 248);

pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(226, 232, 240);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(122, 138, 160);
pub const ACCENT_CYAN: Color32 = Color32::from_rgb(56, 189, 248);
pub const ACCENT_GREEN: Color32 = Color32::from_rgb(52, 211, 153);
pub const ACCENT_AMBER: Color32 = Color32::from_rgb(251, 191, 36);
pub const ACCENT_CORAL: Color32 = Color32::from_rgb(248, 113, 113);
pub const ACCENT_VIOLET: Color32 = Color32::from_rgb(167, 139, 250);

pub const TERM_DEFAULT_FG: Color32 = Color32::from_rgb(218, 226, 238);
pub const TERM_DEFAULT_BG: Color32 = BG_TERM;

pub fn install(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.dark_mode = true;
    visuals.panel_fill = BG_PANEL;
    visuals.window_fill = BG_PANEL;
    visuals.extreme_bg_color = BG_CANVAS;
    visuals.faint_bg_color = BG_ELEVATED;
    visuals.code_bg_color = BG_TERM;
    visuals.override_text_color = Some(TEXT_PRIMARY);
    visuals.window_corner_radius = CornerRadius::same(4);
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.menu_corner_radius = CornerRadius::same(4);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(56, 189, 248, 52);
    visuals.selection.stroke = Stroke::new(1.0, ACCENT_CYAN);

    visuals.widgets.noninteractive.bg_fill = BG_PANEL;
    visuals.widgets.noninteractive.weak_bg_fill = BG_ELEVATED;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(3);

    visuals.widgets.inactive.bg_fill = BG_ELEVATED;
    visuals.widgets.inactive.weak_bg_fill = BG_ELEVATED;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(3);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(32, 42, 58);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(32, 42, 58);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_CYAN);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(3);

    visuals.widgets.active.bg_fill = Color32::from_rgb(20, 62, 86);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(20, 62, 86);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_CYAN);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.active.corner_radius = CornerRadius::same(3);

    let mut style = Style { visuals, ..Style::default() };
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 3.0);
    ctx.set_global_style(style);
}

/// Resolve a protocol [`Color`] into a concrete `Color32`.
pub fn resolve_color(color: Color, default: Color32, bright_bold: bool) -> Color32 {
    match color {
        Color::Default => default,
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        Color::Idx(i) => {
            let idx = if bright_bold && i < 8 { i + 8 } else { i };
            ansi256(idx)
        }
    }
}

fn ansi256(idx: u8) -> Color32 {
    const BASE16: [(u8, u8, u8); 16] = [
        (30, 36, 48),    // 0 black
        (248, 113, 113), // 1 red
        (74, 222, 128),  // 2 green
        (250, 204, 21),  // 3 yellow
        (96, 165, 250),  // 4 blue
        (192, 132, 252), // 5 magenta
        (56, 189, 248),  // 6 cyan
        (203, 213, 225), // 7 white
        (100, 116, 139), // 8 bright black
        (252, 165, 165), // 9 bright red
        (134, 239, 172), // 10 bright green
        (253, 224, 71),  // 11 bright yellow
        (147, 197, 253), // 12 bright blue
        (216, 180, 254), // 13 bright magenta
        (125, 211, 252), // 14 bright cyan
        (248, 250, 252), // 15 bright white
    ];
    match idx {
        0..=15 => {
            let (r, g, b) = BASE16[idx as usize];
            Color32::from_rgb(r, g, b)
        }
        16..=231 => {
            let n = idx - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let comp = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(comp(r), comp(g), comp(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

pub fn dim_color(c: Color32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 150)
}
