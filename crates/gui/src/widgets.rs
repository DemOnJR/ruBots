//! The cubic widget set.
//!
//! egui's stock widgets are rounded and soft; these are the square, hairline
//! versions the shell is built from. They are deliberately small and
//! painter-driven — every one of them is "allocate a rect, stroke a box, draw
//! mono text in it" — because that is the whole design language.

use eframe::egui::{self, Align2, Color32, Rounding, Sense, Stroke};

use crate::theme;

/// `// T I T L E` strip: the header of a cell, and a standalone section rule.
pub fn header_strip(ui: &mut egui::Ui, title: &str, right: Option<(&str, Color32)>) {
    let h = 24.0;
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, Rounding::ZERO, theme::SURFACE_2);
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::LINE_BRIGHT),
    );
    p.text(
        rect.left_center() + egui::vec2(10.0, 0.0),
        Align2::LEFT_CENTER,
        format!("// {}", theme::spaced(title)),
        theme::mono(theme::F_TINY),
        theme::TEXT_DIM,
    );
    if let Some((text, color)) = right {
        p.text(
            rect.right_center() + egui::vec2(-10.0, 0.0),
            Align2::RIGHT_CENTER,
            text,
            theme::mono(theme::F_TINY),
            color,
        );
    }
}

/// A bordered box with a header and crop marks: the only container in the app.
pub fn cell<R>(
    ui: &mut egui::Ui,
    title: &str,
    right: Option<(&str, Color32)>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let out = egui::Frame::none()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::LINE_BRIGHT))
        .inner_margin(egui::Margin::same(0.0))
        .show(ui, |ui| {
            header_strip(ui, title, right);
            egui::Frame::none()
                .inner_margin(egui::Margin::symmetric(12.0, 10.0))
                .show(ui, add)
                .inner
        });
    theme::corner_ticks(ui.painter(), out.response.rect, theme::LINE_BRIGHT);
    out.inner
}

/// A cell with a fixed height, for grid rows that must line up.
pub fn cell_h<R>(
    ui: &mut egui::Ui,
    title: &str,
    height: f32,
    right: Option<(&str, Color32)>,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    cell(ui, title, right, |ui| {
        ui.set_min_height(height);
        add(ui)
    })
}

fn button_impl(ui: &mut egui::Ui, label: &str, primary: bool, enabled: bool) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        theme::spaced(label),
        theme::mono(theme::F_SMALL),
        Color32::WHITE,
    );
    let size = galley.size() + egui::vec2(24.0, 14.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hovered = enabled && resp.hovered();
    let down = enabled && resp.is_pointer_button_down_on();

    let (fill, stroke, fg) = if !enabled {
        (Color32::TRANSPARENT, theme::LINE, theme::TEXT_FAINT)
    } else if primary {
        let f = if down {
            theme::ACCENT_DIM
        } else if hovered {
            Color32::from_rgb(0x5E, 0x40, 0xFF)
        } else {
            theme::ACCENT
        };
        (f, theme::ACCENT, theme::TEXT)
    } else if down {
        (theme::ACCENT_DIM, theme::ACCENT, theme::TEXT)
    } else if hovered {
        (theme::SURFACE_2, theme::ACCENT, theme::TEXT)
    } else {
        (Color32::TRANSPARENT, theme::LINE_BRIGHT, theme::TEXT_DIM)
    };

    let p = ui.painter();
    p.rect_filled(rect, Rounding::ZERO, fill);
    p.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0, stroke));
    p.galley(
        rect.center() - galley.size() * 0.5,
        galley,
        fg,
    );
    if hovered {
        theme::corner_ticks(p, rect, theme::ACCENT);
    }
    resp
}

/// Outlined square button.
pub fn btn(ui: &mut egui::Ui, label: &str) -> egui::Response {
    button_impl(ui, label, false, true)
}

/// Filled accent button — one per view at most.
pub fn btn_primary(ui: &mut egui::Ui, label: &str) -> egui::Response {
    button_impl(ui, label, true, true)
}

/// A button that shows whether its mode is the active one.
///
/// Used for the sort and filter rows: an active mode is the accent-filled
/// button, so "which one am I on" is answered by the same visual rule that
/// says "this is the primary action" everywhere else.
pub fn btn_toggle(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    button_impl(ui, label, active, true)
}

/// Square checkbox with a filled accent core.
pub fn toggle(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), theme::mono(theme::F_SMALL), theme::TEXT);
    let box_side = 12.0;
    let size = egui::vec2(box_side + 8.0 + galley.size().x, galley.size().y.max(box_side) + 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let p = ui.painter();
    let b = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - box_side * 0.5),
        egui::vec2(box_side, box_side),
    );
    let stroke = if resp.hovered() {
        theme::ACCENT
    } else if *on {
        theme::ACCENT
    } else {
        theme::LINE_BRIGHT
    };
    p.rect_stroke(b, Rounding::ZERO, Stroke::new(1.0, stroke));
    if *on {
        p.rect_filled(b.shrink(3.0), Rounding::ZERO, theme::ACCENT);
    }
    p.galley(
        egui::pos2(b.right() + 8.0, rect.center().y - galley.size().y * 0.5),
        galley,
        if *on { theme::TEXT } else { theme::TEXT_DIM },
    );
    resp
}

/// A small square swatch — team color, state light, legend key.
pub fn swatch(ui: &mut egui::Ui, color: Color32, side: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_center_size(rect.center(), egui::vec2(side, side)),
        Rounding::ZERO,
        color,
    );
}

/// The big number in a dashboard tile.
pub fn stat(ui: &mut egui::Ui, value: &str, unit: &str, color: Color32) {
    let h = theme::F_BIG + 6.0;
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), Sense::hover());
    let p = ui.painter();
    let end = theme::heavy_text(
        p,
        rect.left_center() + egui::vec2(0.0, 1.0),
        Align2::LEFT_CENTER,
        value,
        theme::F_BIG,
        color,
    );
    if !unit.is_empty() {
        p.text(
            egui::pos2(end.right() + 6.0, rect.center().y + 5.0),
            Align2::LEFT_BOTTOM,
            unit,
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );
    }
}

/// A meter drawn as discrete cubes rather than a bar, because the whole app is.
pub fn segments(ui: &mut egui::Ui, frac: f32, color: Color32, count: usize) {
    let w = ui.available_width();
    let h = 8.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), Sense::hover());
    let gap = 2.0;
    let seg_w = ((rect.width() - gap * (count as f32 - 1.0)) / count as f32).max(1.0);
    let lit = (frac.clamp(0.0, 1.0) * count as f32).round() as usize;
    let p = ui.painter();
    for i in 0..count {
        let x = rect.left() + i as f32 * (seg_w + gap);
        let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(seg_w, h));
        if i < lit {
            p.rect_filled(r, Rounding::ZERO, color);
        } else {
            p.rect_filled(r, Rounding::ZERO, theme::SURFACE_2);
            p.rect_stroke(r, Rounding::ZERO, Stroke::new(1.0, theme::LINE));
        }
    }
}

/// One hairline across the available width.
pub fn rule(ui: &mut egui::Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 7.0), Sense::hover());
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), rect.center().y),
            egui::pos2(rect.right(), rect.center().y),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

/// `key ................ value`, the app's only two-column layout.
pub fn kv(ui: &mut egui::Ui, key: &str, value: &str, color: Color32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 17.0), Sense::hover());
    let p = ui.painter();
    p.text(
        rect.left_center(),
        Align2::LEFT_CENTER,
        key,
        theme::mono(theme::F_SMALL),
        theme::TEXT_FAINT,
    );
    p.text(
        rect.right_center(),
        Align2::RIGHT_CENTER,
        value,
        theme::mono(theme::F_SMALL),
        color,
    );
}

/// A labelled single-line text field, sized to the available width.
pub fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 22.0), Sense::hover());
        ui.painter().text(
            rect.left_center(),
            Align2::LEFT_CENTER,
            label,
            theme::mono(theme::F_SMALL),
            theme::TEXT_FAINT,
        );
        let w = ui.available_width().max(80.0);
        ui.add_sized(
            [w, 22.0],
            egui::TextEdit::singleline(value)
                .font(theme::mono(theme::F_SMALL))
                .text_color(theme::TEXT)
                .margin(egui::vec2(8.0, 4.0)),
        );
    });
}

/// The same, for an integer. Non-numeric input is simply not kept.
pub fn field_num(ui: &mut egui::Ui, label: &str, value: &mut u32, min: u32, max: u32) {
    let mut text = value.to_string();
    field(ui, label, &mut text);
    if let Ok(n) = text.trim().parse::<u32>() {
        *value = n.clamp(min, max);
    } else if text.trim().is_empty() {
        *value = min;
    }
}

/// A row of column headings, returning each column's offset **from the row's
/// left edge** — the same space `cell_text` works in, so a header and its
/// column cannot drift apart.
pub fn table_head(ui: &mut egui::Ui, cols: &[(&str, f32)]) -> Vec<f32> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 20.0), Sense::hover());
    let p = ui.painter();
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, theme::LINE_BRIGHT),
    );
    let mut offsets = Vec::with_capacity(cols.len());
    let mut x = 0.0;
    for (name, cw) in cols {
        offsets.push(x);
        p.text(
            egui::pos2(rect.left() + x, rect.center().y),
            Align2::LEFT_CENTER,
            name.to_uppercase(),
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );
        x += cw;
    }
    offsets
}

/// A scrubber: ticks, a lit run up to the handle, and a square handle.
///
/// egui's slider is a rounded track with a round grab; this is the same
/// control drawn in the app's own vocabulary, and it reads left-to-right as
/// "live ... further back in time".
pub fn scrub(ui: &mut egui::Ui, value: &mut f32, max: f32) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 22.0), Sense::click_and_drag());
    if resp.dragged() || resp.clicked() {
        if let Some(at) = resp.interact_pointer_pos() {
            let f = ((at.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
            *value = f * max;
        }
    }
    let frac = (*value / max.max(0.001)).clamp(0.0, 1.0);
    let y = rect.center().y;
    let p = ui.painter();
    // Ticks first, so the lit run draws over them.
    for i in 0..=12 {
        let x = rect.left() + rect.width() * i as f32 / 12.0;
        let tall = i % 3 == 0;
        p.line_segment(
            [
                egui::pos2(x, y - if tall { 7.0 } else { 4.0 }),
                egui::pos2(x, y + if tall { 7.0 } else { 4.0 }),
            ],
            Stroke::new(1.0, if tall { theme::LINE_BRIGHT } else { theme::LINE }),
        );
    }
    p.line_segment(
        [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
        Stroke::new(1.0, theme::LINE_BRIGHT),
    );
    let handle_x = rect.left() + rect.width() * frac;
    if frac > 0.0 {
        p.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(handle_x, y)],
            Stroke::new(1.0, theme::ACCENT),
        );
    }
    let handle = egui::Rect::from_center_size(egui::pos2(handle_x, y), egui::vec2(7.0, 16.0));
    p.rect_filled(handle, Rounding::ZERO, if resp.hovered() { theme::ACCENT_TEXT } else { theme::ACCENT });
    resp
}

/// One selectable table row. Cells are painted by the caller via the returned
/// painter rect, so every table can format its own columns.
pub fn table_row(
    ui: &mut egui::Ui,
    height: f32,
    selected: bool,
    zebra: bool,
) -> (egui::Rect, egui::Response) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, height), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, Rounding::ZERO, theme::ACCENT_DIM);
        p.rect_filled(
            egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height())),
            Rounding::ZERO,
            theme::ACCENT,
        );
    } else if resp.hovered() {
        p.rect_filled(rect, Rounding::ZERO, theme::SURFACE_2);
    } else if zebra {
        p.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(0x0A, 0x0A, 0x0D));
    }
    (rect, resp)
}

/// Text inside a table row, at a column offset.
pub fn cell_text(
    ui: &egui::Ui,
    row: egui::Rect,
    x: f32,
    text: &str,
    color: Color32,
    size: f32,
) {
    ui.painter().text(
        egui::pos2(row.left() + x, row.center().y),
        Align2::LEFT_CENTER,
        text,
        theme::mono(size),
        color,
    );
}

/// An empty-state notice: what is missing and what to press.
pub fn empty(ui: &mut egui::Ui, title: &str, hint: &str) {
    ui.add_space(18.0);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 54.0), Sense::hover());
    let p = ui.painter();
    p.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0, theme::LINE));
    theme::corner_ticks(p, rect, theme::LINE_BRIGHT);
    p.text(
        rect.center() - egui::vec2(0.0, 9.0),
        Align2::CENTER_CENTER,
        theme::spaced(title),
        theme::mono(theme::F_SMALL),
        theme::TEXT_DIM,
    );
    p.text(
        rect.center() + egui::vec2(0.0, 10.0),
        Align2::CENTER_CENTER,
        hint,
        theme::mono(theme::F_TINY),
        theme::TEXT_FAINT,
    );
}
