//! One module per screen, plus the few things every screen shares.

pub mod console;
pub mod dashboard;
pub mod deploy;
pub mod fleet;
pub mod radar;
pub mod server;
pub mod settings;

use eframe::egui::{self, Color32};

use crate::theme;

/// Split the remaining space into a content pane and a fixed-width inspector.
///
/// Both halves come back as independent `Ui`s so a view can lay out a canvas
/// and a side column without nesting egui panels (which only exist at the top
/// level of a frame).
pub fn two_pane(ui: &mut egui::Ui, right_w: f32) -> (egui::Ui, egui::Ui) {
    let full = ui.available_rect_before_wrap();
    ui.allocate_rect(full, egui::Sense::hover());
    let gap = 14.0;
    let split = (full.right() - right_w - gap).max(full.left() + 240.0);
    let left = egui::Rect::from_min_max(full.min, egui::pos2(split, full.bottom()));
    let right = egui::Rect::from_min_max(egui::pos2(split + gap, full.top()), full.max);
    (
        ui.child_ui(left, egui::Layout::top_down(egui::Align::Min)),
        ui.child_ui(right, egui::Layout::top_down(egui::Align::Min)),
    )
}

/// The team colors, dimmed when the bot is dead — used by every list and the
/// radar, so a colour always means the same thing.
pub fn team_color(team: u8, alive: bool) -> Color32 {
    if !alive {
        return theme::DEAD;
    }
    match team {
        1 => theme::TEAM_T,
        2 => theme::TEAM_CT,
        _ => theme::TEXT_DIM,
    }
}

pub fn team_short(team: u8) -> &'static str {
    match team {
        1 => "T",
        2 => "CT",
        _ => "-",
    }
}
