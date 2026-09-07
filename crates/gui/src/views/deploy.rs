//! Deploy: launch and stop bots, and watch the processes that result.
//!
//! The launch parameters are the same ones `scripts/swarm.ps1` takes, and the
//! two rules that script encodes are enforced here rather than documented:
//! launches are staggered, and lifetimes are staggered too, so the fleet does
//! not all connect — or all leave — inside the window that trips ReAuthCheck.

use eframe::egui::{self, Color32};

use crate::proc::Kind;
use crate::{theme, widgets, App};

use super::{team_color, team_short};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let (mut left, mut right) = super::two_pane(ui, 340.0);
    processes(app, &mut left);
    form(app, &mut right);
}

fn form(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let runner = app.cfg.runner_path();
            let ready = runner.exists();
            widgets::cell(
                ui,
                "runner",
                Some((
                    if ready { "built" } else { "missing" },
                    if ready { theme::OK } else { theme::BAD },
                )),
                |ui| {
                    widgets::kv(
                        ui,
                        "profile",
                        if app.cfg.release { "release" } else { "debug" },
                        theme::TEXT_DIM,
                    );
                    let shown = runner
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    widgets::kv(ui, "binary", &shown, theme::TEXT_DIM);
                    if !ready {
                        widgets::kv(ui, "path", &runner.to_string_lossy(), theme::TEXT_FAINT);
                    }
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if widgets::btn(ui, "build runner").clicked() {
                            app.build_runner();
                        }
                        let mut release = app.cfg.release;
                        if widgets::toggle(ui, &mut release, "release").changed() {
                            app.cfg.release = release;
                        }
                    });
                },
            );

            ui.add_space(10.0);
            widgets::cell(ui, "swarm", None, |ui| {
                let mut addr = app.cfg.addr.clone();
                widgets::field(ui, "address", &mut addr);
                app.cfg.addr = addr;
                widgets::field_num(ui, "bots", &mut app.cfg.bots, 1, 64);
                widgets::field_num(ui, "seconds", &mut app.cfg.secs, 15, 36_000);
                widgets::field_num(ui, "stagger ms", &mut app.cfg.stagger_ms, 0, 30_000);
                let mut map = app.cfg.map.clone();
                widgets::field(ui, "map", &mut map);
                app.cfg.map = map;
                let mut difficulty = app.cfg.difficulty.clone();
                widgets::field(ui, "difficulty", &mut difficulty);
                app.cfg.difficulty = difficulty;
                ui.add_space(4.0);
                widgets::kv(
                    ui,
                    "window",
                    &format!(
                        "{:.0}s to launch all {}",
                        app.cfg.bots as f32 * app.cfg.stagger_ms as f32 / 1000.0,
                        app.cfg.bots
                    ),
                    theme::TEXT_FAINT,
                );
                widgets::kv(
                    ui,
                    "teams",
                    &format!(
                        "T {} / CT {}",
                        app.cfg.bots.div_ceil(2),
                        app.cfg.bots / 2
                    ),
                    theme::TEXT_FAINT,
                );
            });

            ui.add_space(10.0);
            widgets::cell(ui, "control", None, |ui| {
                ui.horizontal(|ui| {
                    if widgets::btn_primary(ui, "launch").clicked() {
                        app.deploy();
                    }
                    if widgets::btn(ui, "stop all").clicked() {
                        app.stop_bots();
                    }
                });
                if app.sup.queued() > 0 {
                    ui.add_space(4.0);
                    widgets::kv(
                        ui,
                        "queued",
                        &format!("{} waiting", app.sup.queued()),
                        theme::WARN,
                    );
                    if widgets::btn(ui, "cancel queue").clicked() {
                        let n = app.sup.cancel_queued();
                        app.say(format!("cancelled {n} queued launches"), theme::WARN);
                    }
                }
                widgets::rule(ui);
                widgets::kv(
                    ui,
                    "note",
                    "rapid mass connects trip ReAuthCheck",
                    theme::TEXT_FAINT,
                );
            });
        });
}

fn processes(app: &mut App, ui: &mut egui::Ui) {
    let running = app.sup.running(Kind::Bot);
    widgets::cell(
        ui,
        "processes",
        Some((
            &format!("{running} running · {} queued", app.sup.queued()),
            if running > 0 { theme::OK } else { theme::TEXT_FAINT },
        )),
        |ui| {
            if app.sup.procs.is_empty() {
                widgets::empty(
                    ui,
                    "nothing launched",
                    "press LAUNCH to start a staggered swarm",
                );
                return;
            }
            let cols: [(&str, f32); 6] = [
                ("label", 110.0),
                ("kind", 66.0),
                ("pid", 70.0),
                ("team", 48.0),
                ("uptime", 76.0),
                ("state", 90.0),
            ];
            let xs = widgets::table_head(ui, &cols);
            let mut stop: Option<u32> = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, p) in app.sup.procs.iter().enumerate() {
                        let (row, resp) = widgets::table_row(ui, 20.0, false, i % 2 == 1);
                        let (state, color): (String, Color32) = match p.exit {
                            None => ("running".into(), theme::OK),
                            Some(0) => ("exited 0".into(), theme::TEXT_DIM),
                            Some(code) => (format!("exited {code}"), theme::WARN),
                        };
                        let vals: [(String, Color32); 6] = [
                            (p.label.clone(), theme::TEXT),
                            (p.kind.label().to_string(), theme::TEXT_FAINT),
                            (p.pid.to_string(), theme::TEXT_DIM),
                            (
                                team_short(p.team).to_string(),
                                team_color(p.team, true),
                            ),
                            (
                                format!("{:.0}s", p.uptime().as_secs_f32()),
                                theme::TEXT_DIM,
                            ),
                            (state, color),
                        ];
                        for (col, (text, color)) in vals.iter().enumerate() {
                            widgets::cell_text(
                                ui,
                                row,
                                xs.get(col).copied().unwrap_or(0.0),
                                text,
                                *color,
                                theme::F_SMALL,
                            );
                        }
                        // Click a running row to stop just that one process.
                        if resp.clicked() && p.running() {
                            stop = Some(p.id);
                        }
                        if resp.hovered() && p.running() {
                            widgets::cell_text(
                                ui,
                                row,
                                row.width() - 70.0,
                                "click = stop",
                                theme::ACCENT_TEXT,
                                theme::F_TINY,
                            );
                        }
                    }
                });
            if let Some(id) = stop {
                app.sup.stop(id);
                app.say("stopped one process", theme::WARN);
            }
        },
    );
}
