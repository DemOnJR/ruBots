//! The look: pure black, hairline lattice, zero rounding, one accent.
//!
//! Everything in the shell is drawn from these constants, so the whole app
//! stays one system: no widget invents its own grey, and a palette change is
//! a one-line edit here. The rules are deliberately blunt —
//!
//! * **No rounding anywhere.** Every corner is 90°, including scroll bars,
//!   buttons, checkboxes and the map dots (which are squares).
//! * **One accent** (`ACCENT`) for "the thing you are on / the thing you press".
//!   Team and status colors are signal, not decoration.
//! * **Hairlines, not fills.** Structure is drawn with 1 px strokes on black;
//!   a filled panel is the exception, used to say "this is selected".
//! * **Monospace everywhere**, because the whole app is telemetry.

use eframe::egui::{self, Color32, FontFamily, FontId, Rounding, Stroke, TextStyle};

// ---- palette ------------------------------------------------------------

/// The ground. Pure black, so the panels read as cut out of it.
pub const BG: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
/// One step up: cell interiors.
pub const SURFACE: Color32 = Color32::from_rgb(0x07, 0x07, 0x09);
/// Two steps up: selected rows, header strips.
pub const SURFACE_2: Color32 = Color32::from_rgb(0x0D, 0x0D, 0x11);
/// Hairline: the lattice in the background.
pub const LINE: Color32 = Color32::from_rgb(0x17, 0x17, 0x1D);
/// Hairline that has to be seen: cell borders, table rules.
pub const LINE_BRIGHT: Color32 = Color32::from_rgb(0x28, 0x28, 0x32);

pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xE8, 0xEC);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x86, 0x86, 0x92);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x4C, 0x4C, 0x58);

/// Electric violet. The only "brand" color in the app.
pub const ACCENT: Color32 = Color32::from_rgb(0x4B, 0x2B, 0xFF);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x24, 0x16, 0x7A);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(0xA6, 0x95, 0xFF);

pub const OK: Color32 = Color32::from_rgb(0x38, 0xD0, 0x7A);
pub const WARN: Color32 = Color32::from_rgb(0xE8, 0xA0, 0x20);
pub const BAD: Color32 = Color32::from_rgb(0xFF, 0x3B, 0x3B);

/// Terrorist yellow / Counter-Terrorist blue, as the game reads them.
pub const TEAM_T: Color32 = Color32::from_rgb(0xE8, 0xC8, 0x28);
pub const TEAM_CT: Color32 = Color32::from_rgb(0x3C, 0x96, 0xEB);
pub const DEAD: Color32 = Color32::from_rgb(0x50, 0x50, 0x58);

// ---- metrics ------------------------------------------------------------

pub const RAIL_W: f32 = 208.0;
pub const TOPBAR_H: f32 = 46.0;
pub const STATUS_H: f32 = 24.0;
/// Side of the corner ticks drawn on every cell.
pub const TICK: f32 = 4.0;
/// Spacing of the background lattice, in points.
pub const GRID_STEP: f32 = 96.0;

pub const F_TINY: f32 = 9.5;
pub const F_SMALL: f32 = 11.0;
pub const F_BODY: f32 = 12.5;
pub const F_HEAD: f32 = 15.0;
pub const F_BIG: f32 = 26.0;

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// Letter-spaced uppercase, the way the display type on the reference reads.
///
/// egui has no letter-spacing, so the spacing is literal — which is why this
/// is only ever used on short labels (nav items, cell titles).
pub fn spaced(s: &str) -> String {
    let upper = s.to_uppercase();
    let mut out = String::with_capacity(upper.len() * 2);
    for (i, c) in upper.chars().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

// ---- install ------------------------------------------------------------

/// Apply the whole look to a context. Called once, at startup.
pub fn install(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();

    style.text_styles = [
        (TextStyle::Heading, mono(F_HEAD)),
        (TextStyle::Body, mono(F_BODY)),
        (TextStyle::Monospace, mono(F_BODY)),
        (TextStyle::Button, mono(F_SMALL)),
        (TextStyle::Small, mono(F_TINY)),
    ]
    .into();

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.menu_margin = egui::Margin::same(0.0);
    style.spacing.window_margin = egui::Margin::same(0.0);
    style.spacing.indent = 16.0;
    style.spacing.slider_width = 160.0;
    style.spacing.interact_size = egui::vec2(24.0, 22.0);
    style.spacing.scroll = {
        let mut s = egui::style::ScrollStyle::solid();
        s.bar_width = 8.0;
        s.bar_inner_margin = 2.0;
        s.bar_outer_margin = 0.0;
        s.handle_min_length = 24.0;
        s
    };

    let mut v = egui::Visuals::dark();
    v.dark_mode = true;
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, LINE_BRIGHT);
    v.window_rounding = Rounding::ZERO;
    v.menu_rounding = Rounding::ZERO;
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.faint_bg_color = Color32::from_rgb(0x0A, 0x0A, 0x0D);
    v.extreme_bg_color = Color32::from_rgb(0x03, 0x03, 0x04);
    v.code_bg_color = SURFACE_2;
    v.warn_fg_color = WARN;
    v.error_fg_color = BAD;
    v.hyperlink_color = ACCENT_TEXT;
    v.selection = egui::style::Selection {
        bg_fill: ACCENT_DIM,
        stroke: Stroke::new(1.0, ACCENT_TEXT),
    };

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    w.noninteractive.rounding = Rounding::ZERO;
    w.noninteractive.expansion = 0.0;

    w.inactive.bg_fill = Color32::TRANSPARENT;
    w.inactive.weak_bg_fill = Color32::TRANSPARENT;
    w.inactive.bg_stroke = Stroke::new(1.0, LINE_BRIGHT);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    w.inactive.rounding = Rounding::ZERO;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = SURFACE_2;
    w.hovered.weak_bg_fill = SURFACE_2;
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.rounding = Rounding::ZERO;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = ACCENT_DIM;
    w.active.weak_bg_fill = ACCENT_DIM;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.rounding = Rounding::ZERO;
    w.active.expansion = 0.0;

    w.open.bg_fill = SURFACE_2;
    w.open.weak_bg_fill = SURFACE_2;
    w.open.bg_stroke = Stroke::new(1.0, LINE_BRIGHT);
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.rounding = Rounding::ZERO;
    w.open.expansion = 0.0;

    style.visuals = v;
    ctx.set_style(style);
}

// ---- painting helpers ---------------------------------------------------

/// The background lattice: hairlines on a fixed pitch with a filled node at
/// every crossing. This is the one piece of pure decoration in the app, and
/// it is anchored to the panel rect so it never scrolls with content.
pub fn lattice(painter: &egui::Painter, rect: egui::Rect, step: f32) {
    let stroke = Stroke::new(1.0, LINE);
    let mut x = rect.left() + step;
    while x < rect.right() {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            stroke,
        );
        x += step;
    }
    let mut y = rect.top() + step;
    while y < rect.bottom() {
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            stroke,
        );
        y += step;
    }
    // Nodes at the crossings — the tell of the reference design.
    let mut x = rect.left() + step;
    while x < rect.right() {
        let mut y = rect.top() + step;
        while y < rect.bottom() {
            painter.rect_filled(
                egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(3.0, 3.0)),
                Rounding::ZERO,
                LINE_BRIGHT,
            );
            y += step;
        }
        x += step;
    }
}

/// Four corner ticks just outside a rect — the crop marks on every cell.
pub fn corner_ticks(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let t = TICK;
    for (x, y) in [
        (rect.left(), rect.top()),
        (rect.right(), rect.top()),
        (rect.left(), rect.bottom()),
        (rect.right(), rect.bottom()),
    ] {
        painter.rect_filled(
            egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(t, t)),
            Rounding::ZERO,
            color,
        );
    }
}

/// Faux-bold: egui ships no bold face, so heavy display text is the same
/// glyphs painted four times a third of a pixel apart. Cheap, and it is the
/// only way to get the reference's weight contrast out of one font.
pub fn heavy_text(
    painter: &egui::Painter,
    pos: egui::Pos2,
    align: egui::Align2,
    text: &str,
    size: f32,
    color: Color32,
) -> egui::Rect {
    let font = mono(size);
    let mut rect = egui::Rect::NOTHING;
    for (dx, dy) in [(-0.35, 0.0), (0.35, 0.0), (0.0, -0.35), (0.0, 0.35)] {
        rect = painter.text(
            pos + egui::vec2(dx, dy),
            align,
            text,
            font.clone(),
            color,
        );
    }
    rect
}
