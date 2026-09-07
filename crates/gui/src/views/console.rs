//! Console: every child process's output in one stream.
//!
//! Thirty bots write thirty log files under `captures/swarm/`, which is a fine
//! place for them and a terrible place to read them from while a run is going
//! wrong. Here they are one filtered stream, tagged by source, with the same
//! colors the rest of the app uses for the same states.

use eframe::egui::{self, Color32};

use crate::proc::{Kind, Level};
use crate::{theme, widgets, App, LogFilter};

pub fn level_color(level: Level) -> Color32 {
    match level {
        Level::Error => theme::BAD,
        Level::Warn => theme::WARN,
        Level::Meta => theme::ACCENT_TEXT,
        Level::Info => theme::TEXT_DIM,
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let (total, errors) = app
        .sup
        .log
        .lock()
        .map(|l| (l.len(), l.errors))
        .unwrap_or((0, 0));

    widgets::cell(
        ui,
        "console",
        Some((
            &format!("{total} lines · {errors} errors"),
            if errors > 0 { theme::BAD } else { theme::TEXT_FAINT },
        )),
        |ui| {
            ui.horizontal(|ui| {
                for (label, filter) in [
                    ("all", LogFilter::All),
                    ("bots", LogFilter::Bots),
                    ("server", LogFilter::Server),
                    ("build", LogFilter::Build),
                    ("errors", LogFilter::Errors),
                ] {
                    if widgets::btn_toggle(ui, label, app.console.filter == filter).clicked() {
                        app.console.filter = filter;
                    }
                }
                ui.add_space(6.0);
                widgets::toggle(ui, &mut app.console.follow, "follow");
                if widgets::btn(ui, "clear").clicked() {
                    if let Ok(mut log) = app.sup.log.lock() {
                        log.clear();
                    }
                }
            });
            ui.add_space(4.0);
            let mut search = app.console.search.clone();
            widgets::field(ui, "search", &mut search);
            app.console.search = search;
            ui.add_space(6.0);

            let needle = app.console.search.to_ascii_lowercase();
            let filter = app.console.filter;
            let rows: Vec<(f32, String, String, Color32)> = app
                .sup
                .log
                .lock()
                .map(|log| {
                    log.iter()
                        .filter(|l| match filter {
                            LogFilter::All => true,
                            LogFilter::Bots => l.kind == Kind::Bot,
                            LogFilter::Server => l.kind == Kind::Server,
                            LogFilter::Build => l.kind == Kind::Build,
                            LogFilter::Errors => l.level == Level::Error,
                        })
                        .filter(|l| {
                            needle.is_empty()
                                || l.text.to_ascii_lowercase().contains(&needle)
                                || l.src.to_ascii_lowercase().contains(&needle)
                        })
                        .map(|l| {
                            (
                                l.at,
                                l.src.clone(),
                                l.text.clone(),
                                level_color(l.level),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();

            if rows.is_empty() {
                widgets::empty(
                    ui,
                    "nothing to show",
                    "child output appears here as soon as something runs",
                );
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(app.console.follow)
                .show_rows(ui, 15.0, rows.len(), |ui, range| {
                    for i in range {
                        let (at, src, text, color) = &rows[i];
                        let w = ui.available_width();
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(w, 15.0), egui::Sense::hover());
                        let p = ui.painter();
                        p.text(
                            rect.left_center(),
                            egui::Align2::LEFT_CENTER,
                            format!("T+{at:7.1}"),
                            theme::mono(theme::F_TINY),
                            theme::TEXT_FAINT,
                        );
                        p.text(
                            rect.left_center() + egui::vec2(66.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            src,
                            theme::mono(theme::F_TINY),
                            theme::TEXT_DIM,
                        );
                        p.text(
                            rect.left_center() + egui::vec2(152.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            text,
                            theme::mono(theme::F_TINY),
                            *color,
                        );
                    }
                });
        },
    );
}
