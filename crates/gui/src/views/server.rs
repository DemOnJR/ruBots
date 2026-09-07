//! Server: the docker test server, and the server's own view of the swarm.
//!
//! The A2S player count is the check that matters. A bot broadcasting
//! telemetry has a brain running; it does not prove the server accepted it.
//! When the radar shows ten bots and A2S shows two players, the problem is
//! authentication or the connect rate limit, not navigation — and that is a
//! completely different afternoon.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::proc::Kind;
use crate::{theme, widgets, App};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    // The probe is cheap (one UDP round trip) and this screen is the only
    // place it runs, so keep it fresh while the screen is open.
    if app.server.auto
        && !app.server.probing
        && app
            .server
            .last_probe
            .is_none_or(|t| t.elapsed() > Duration::from_secs(5))
    {
        app.probe_server();
    }

    let (mut left, mut right) = super::two_pane(ui, 340.0);
    status(app, &mut left);
    controls(app, &mut right);
}

fn status(app: &mut App, ui: &mut egui::Ui) {
    let info = app.server.info.lock().ok().and_then(|g| g.clone());
    let (right_label, right_color) = match &info {
        Some(Ok(_)) => ("responding", theme::OK),
        Some(Err(_)) => ("no reply", theme::BAD),
        None => ("unknown", theme::TEXT_FAINT),
    };

    widgets::cell(
        ui,
        "game server",
        Some((right_label, right_color)),
        |ui| {
            widgets::kv(ui, "address", &app.cfg.addr.clone(), theme::TEXT);
            match &info {
                Some(Ok(i)) => {
                    ui.add_space(6.0);
                    widgets::stat(
                        ui,
                        &format!("{}/{}", i.players, i.max_players),
                        "players the server sees",
                        theme::OK,
                    );
                    widgets::segments(
                        ui,
                        if i.max_players == 0 {
                            0.0
                        } else {
                            i.players as f32 / i.max_players as f32
                        },
                        theme::OK,
                        28,
                    );
                    ui.add_space(6.0);
                    widgets::kv(ui, "hostname", &i.name, theme::TEXT_DIM);
                    widgets::kv(ui, "map", &i.map, theme::TEXT_DIM);
                    widgets::kv(
                        ui,
                        "game",
                        &format!("{} / {}", i.folder, i.game),
                        theme::TEXT_DIM,
                    );
                    widgets::kv(
                        ui,
                        "reply",
                        if i.goldsrc { "goldsrc (m)" } else { "source (I)" },
                        theme::TEXT_FAINT,
                    );

                    // The number that tells you whether a deploy worked.
                    widgets::rule(ui);
                    let claimed = app.fleet.bots.len();
                    let delta = i.players as i32 - claimed as i32;
                    widgets::kv(
                        ui,
                        "telemetry says",
                        &format!("{claimed} bots"),
                        theme::TEXT_DIM,
                    );
                    widgets::kv(
                        ui,
                        "difference",
                        &match delta {
                            0 => "matches".to_string(),
                            d if d < 0 => format!("{} not connected", -d),
                            d => format!("{d} other players"),
                        },
                        if delta < 0 { theme::WARN } else { theme::TEXT_DIM },
                    );
                }
                Some(Err(e)) => {
                    ui.add_space(6.0);
                    widgets::stat(ui, "no reply", "", theme::BAD);
                    widgets::kv(ui, "error", e, theme::TEXT_DIM);
                    widgets::kv(
                        ui,
                        "check",
                        "docker compose up, then the address above",
                        theme::TEXT_FAINT,
                    );
                }
                None => {
                    widgets::empty(ui, "not probed yet", "A2S_INFO runs every 5 s on this screen");
                }
            }
        },
    );

    ui.add_space(12.0);
    let dir = app.cfg.testserver_path();
    let exists = dir.exists();
    let height = (ui.available_height() - 46.0).max(120.0);
    widgets::cell_h(
        ui,
        "compose output",
        height,
        Some((
            if exists { "testserver/" } else { "missing" },
            if exists { theme::TEXT_FAINT } else { theme::BAD },
        )),
        |ui| {
            let lines: Vec<(f32, String, egui::Color32)> = app
                .sup
                .log
                .lock()
                .map(|log| {
                    log.iter()
                        .filter(|l| l.kind == Kind::Server)
                        .rev()
                        .take(200)
                        .map(|l| (l.at, l.text.clone(), super::console::level_color(l.level)))
                        .collect()
                })
                .unwrap_or_default();
            if lines.is_empty() {
                widgets::empty(ui, "no compose output", "press UP, PS or LOGS");
                return;
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for (at, text, color) in lines.into_iter().rev() {
                        let w = ui.available_width();
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(w, 15.0), egui::Sense::hover());
                        let p = ui.painter();
                        p.text(
                            rect.left_center(),
                            egui::Align2::LEFT_CENTER,
                            format!("T+{at:6.1}"),
                            theme::mono(theme::F_TINY),
                            theme::TEXT_FAINT,
                        );
                        p.text(
                            rect.left_center() + egui::vec2(62.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            &text,
                            theme::mono(theme::F_TINY),
                            color,
                        );
                    }
                });
        },
    );
}

fn controls(app: &mut App, ui: &mut egui::Ui) {
    widgets::cell(ui, "docker compose", None, |ui| {
        let dir = app.cfg.testserver_path();
        widgets::kv(
            ui,
            "cwd",
            &dir.file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "-".into()),
            theme::TEXT_DIM,
        );
        widgets::kv(
            ui,
            "found",
            if dir.exists() { "yes" } else { "no" },
            if dir.exists() { theme::OK } else { theme::BAD },
        );
        ui.add_space(6.0);
        if widgets::btn_primary(ui, "up -d").clicked() {
            app.docker(&["up", "-d"], "compose-up");
            app.say("docker compose up -d", theme::ACCENT_TEXT);
        }
        ui.horizontal(|ui| {
            if widgets::btn(ui, "build").clicked() {
                app.docker(&["up", "-d", "--build"], "compose-build");
            }
            if widgets::btn(ui, "down").clicked() {
                app.docker(&["down"], "compose-down");
                app.say("docker compose down", theme::WARN);
            }
        });
        ui.horizontal(|ui| {
            if widgets::btn(ui, "ps").clicked() {
                app.docker(&["ps"], "compose-ps");
            }
            if widgets::btn(ui, "logs").clicked() {
                app.docker(&["logs", "--tail", "120"], "compose-logs");
            }
        });
        widgets::rule(ui);
        widgets::kv(
            ui,
            "note",
            "first run downloads the base image",
            theme::TEXT_FAINT,
        );
    });

    ui.add_space(10.0);
    widgets::cell(ui, "probe", None, |ui| {
        ui.horizontal(|ui| {
            if widgets::btn(ui, "probe now").clicked() {
                app.probe_server();
            }
            widgets::toggle(ui, &mut app.server.auto, "auto 5s");
        });
        let age = app
            .server
            .last_probe
            .map(|t: Instant| format!("{:.0}s ago", t.elapsed().as_secs_f32()))
            .unwrap_or_else(|| "never".into());
        widgets::kv(
            ui,
            "last",
            &if app.server.probing {
                "in flight".to_string()
            } else {
                age
            },
            theme::TEXT_DIM,
        );
        widgets::kv(ui, "query", "A2S_INFO on the game port", theme::TEXT_FAINT);
    });

    ui.add_space(10.0);
    widgets::cell(ui, "swarm", None, |ui| {
        widgets::kv(
            ui,
            "bot processes",
            &format!("{}", app.sup.running(Kind::Bot)),
            theme::TEXT_DIM,
        );
        widgets::kv(
            ui,
            "telemetry",
            &if app.fleet.live() {
                format!("{:.0} pkt/s", app.fleet.pps)
            } else {
                "quiet".into()
            },
            if app.fleet.live() { theme::OK } else { theme::TEXT_FAINT },
        );
        ui.add_space(4.0);
        if widgets::btn(ui, "deploy swarm").clicked() {
            app.deploy();
        }
        if widgets::btn(ui, "stop bots").clicked() {
            app.stop_bots();
        }
    });
}
