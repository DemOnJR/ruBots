//! The dashboard: the six numbers that say whether a run is healthy.
//!
//! Everything here is derived from state the shell already has — no extra
//! polling — so opening this screen costs nothing while a swarm runs.

use std::collections::HashMap;

use eframe::egui::{self, Color32};

use crate::proc::Kind;
use crate::{theme, widgets, App, View};


pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    tiles(app, ui);
    ui.add_space(12.0);

    // The rest of the window is one row: two stacked cells on the left, the
    // action column on the right. Laid out from rects rather than nested
    // layouts so the lower cells actually fill the height available.
    let rest = ui.available_rect_before_wrap();
    ui.allocate_rect(rest, egui::Sense::hover());
    let right_w = 232.0;
    let gap = 12.0;
    let split = (rest.right() - right_w - gap).max(rest.left() + 320.0);
    let mut left = ui.child_ui(
        egui::Rect::from_min_max(rest.min, egui::pos2(split, rest.bottom())),
        egui::Layout::top_down(egui::Align::Min),
    );
    let mut right = ui.child_ui(
        egui::Rect::from_min_max(egui::pos2(split + gap, rest.top()), rest.max),
        egui::Layout::top_down(egui::Align::Min),
    );
    rungs(app, &mut left);
    left.add_space(12.0);
    recent(app, &mut left);
    actions(app, &mut right);
}

fn tiles(app: &mut App, ui: &mut egui::Ui) {
    let bots = app.fleet.bots.len();
    let alive = app.fleet.alive();
    let stuck = app.fleet.stuck();
    let running = app.sup.running(Kind::Bot);
    let queued = app.sup.queued();
    let live = app.fleet.live();

    ui.columns(3, |c| {
        widgets::cell_h(&mut c[0], "swarm", 78.0, None, |ui| {
            widgets::stat(
                ui,
                &format!("{alive}"),
                &format!("/ {bots} seen"),
                if alive > 0 { theme::TEXT } else { theme::TEXT_FAINT },
            );
            widgets::segments(
                ui,
                if bots == 0 {
                    0.0
                } else {
                    alive as f32 / bots as f32
                },
                theme::ACCENT,
                24,
            );
            ui.add_space(4.0);
            widgets::kv(
                ui,
                "t / ct",
                &format!(
                    "{} / {}",
                    app.fleet.team_count(1),
                    app.fleet.team_count(2)
                ),
                theme::TEXT_DIM,
            );
        });

        widgets::cell_h(&mut c[1], "telemetry", 78.0, None, |ui| {
            widgets::stat(
                ui,
                &format!("{:.0}", app.fleet.pps),
                "pkt/s apt1",
                if live { theme::OK } else { theme::TEXT_FAINT },
            );
            widgets::segments(ui, (app.fleet.pps / 60.0).min(1.0), theme::OK, 24);
            ui.add_space(4.0);
            widgets::kv(
                ui,
                "port / total",
                &format!("{} / {}", app.feed.radar_port, app.fleet.packets),
                theme::TEXT_DIM,
            );
        });

        widgets::cell_h(&mut c[2], "stuck", 78.0, None, |ui| {
            widgets::stat(
                ui,
                &format!("{stuck}"),
                "bots grinding",
                if stuck > 0 { theme::BAD } else { theme::TEXT_FAINT },
            );
            widgets::segments(
                ui,
                if alive == 0 {
                    0.0
                } else {
                    stuck as f32 / alive as f32
                },
                theme::BAD,
                24,
            );
            ui.add_space(4.0);
            let worst = app
                .fleet
                .bots
                .iter()
                .max_by(|a, b| a.1.stuck_for.total_cmp(&b.1.stuck_for))
                .filter(|(_, b)| b.stuck())
                .map(|(n, b)| format!("{n} {:.0}s", b.stuck_for))
                .unwrap_or_else(|| "none".into());
            widgets::kv(ui, "worst", &worst, theme::TEXT_DIM);
        });
    });

    ui.add_space(12.0);

    ui.columns(3, |c| {
        widgets::cell_h(&mut c[0], "processes", 78.0, None, |ui| {
            widgets::stat(
                ui,
                &format!("{running}"),
                if queued > 0 {
                    "running"
                } else {
                    "bot processes"
                },
                if running > 0 { theme::ACCENT_TEXT } else { theme::TEXT_FAINT },
            );
            if queued > 0 {
                widgets::kv(ui, "queued", &format!("+{queued} staggered"), theme::WARN);
            } else {
                widgets::kv(ui, "queued", "none", theme::TEXT_DIM);
            }
            widgets::kv(
                ui,
                "server / build",
                &format!(
                    "{} / {}",
                    app.sup.running(Kind::Server),
                    app.sup.running(Kind::Build)
                ),
                theme::TEXT_DIM,
            );
        });

        widgets::cell_h(&mut c[1], "map", 78.0, None, |ui| {
            let (name, nodes, hops) = match &app.fleet.map {
                Some((n, bg, _)) => (n.clone(), bg.points.len(), bg.hops.len()),
                None => ("—".to_string(), 0, 0),
            };
            widgets::stat(ui, &name, "", theme::TEXT);
            widgets::kv(ui, "lattice", &format!("{nodes} nodes"), theme::TEXT_DIM);
            widgets::kv(ui, "special hops", &format!("{hops}"), theme::TEXT_DIM);
        });

        widgets::cell_h(&mut c[2], "server", 78.0, None, |ui| {
            let info = app.server.info.lock().ok().and_then(|g| g.clone());
            match info {
                Some(Ok(i)) => {
                    widgets::stat(
                        ui,
                        &format!("{}/{}", i.players, i.max_players),
                        "players",
                        theme::OK,
                    );
                    widgets::kv(ui, "name", &i.name, theme::TEXT_DIM);
                    widgets::kv(ui, "map", &i.map, theme::TEXT_DIM);
                }
                Some(Err(e)) => {
                    widgets::stat(ui, "down", "", theme::BAD);
                    widgets::kv(ui, "addr", &app.cfg.addr, theme::TEXT_DIM);
                    widgets::kv(ui, "why", &e, theme::TEXT_FAINT);
                }
                None => {
                    widgets::stat(ui, "?", "not probed", theme::TEXT_FAINT);
                    widgets::kv(ui, "addr", &app.cfg.addr, theme::TEXT_DIM);
                    widgets::kv(ui, "hint", "open the server view", theme::TEXT_FAINT);
                }
            }
        });
    });
}

/// Which decision every live bot is on, as a share of the fleet.
fn rungs(app: &mut App, ui: &mut egui::Ui) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for b in app.fleet.bots.values().filter(|b| b.t.alive) {
        *counts.entry(b.rung()).or_default() += 1;
    }
    widgets::cell(ui, "rung distribution", None, |ui| {
        if counts.is_empty() {
            widgets::empty(ui, "no live bots", "deploy a swarm to see decisions");
            return;
        }
        let total: usize = counts.values().sum();
        let mut rows: Vec<(String, usize)> = counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (rung, n) in rows {
            let color = rung_color(&rung);
            let frac = n as f32 / total as f32;
            widgets::kv(
                ui,
                &rung,
                &format!("{n} of {total}   {:.0}%", 100.0 * frac),
                color,
            );
            widgets::segments(ui, frac, color, 40);
            ui.add_space(6.0);
        }
    });
}

/// The action column: everything the shell can start, in one place.
fn actions(app: &mut App, ui: &mut egui::Ui) {
    let h = ui.available_height();
    widgets::cell_h(ui, "actions", (h - 46.0).max(120.0), None, |ui| {
        if widgets::btn_primary(ui, "deploy swarm").clicked() {
            app.deploy();
        }
        ui.add_space(4.0);
        if widgets::btn(ui, "stop bots").clicked() {
            app.stop_bots();
        }
        if widgets::btn(ui, "build runner").clicked() {
            app.build_runner();
        }
        if widgets::btn(ui, "start server").clicked() {
            app.docker(&["up", "-d"], "compose-up");
            app.say("docker compose up -d", theme::ACCENT_TEXT);
        }
        if widgets::btn(ui, "probe server").clicked() {
            app.probe_server();
        }
        widgets::rule(ui);
        if widgets::btn(ui, "open radar").clicked() {
            app.view = View::Radar;
        }
        if widgets::btn(ui, "open console").clicked() {
            app.view = View::Console;
        }
        if widgets::btn(ui, "open deploy").clicked() {
            app.view = View::Deploy;
        }
    });
}

fn recent(app: &mut App, ui: &mut egui::Ui) {
    let lines: Vec<(f32, String, String, Color32)> = app
        .sup
        .log
        .lock()
        .map(|log| {
            log.iter()
                .rev()
                .take(40)
                .map(|l| {
                    (
                        l.at,
                        l.src.clone(),
                        l.text.clone(),
                        crate::views::console::level_color(l.level),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let h = (ui.available_height() - 46.0).max(90.0);
    widgets::cell_h(ui, "recent", h, Some(("newest last", theme::TEXT_FAINT)), |ui| {
        if lines.is_empty() {
            widgets::empty(ui, "nothing has happened yet", "actions and child output land here");
            return;
        }
        for (at, src, text, color) in lines.into_iter().rev() {
            let w = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 16.0), egui::Sense::hover());
            let p = ui.painter();
            p.text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                format!("T+{at:6.1}"),
                theme::mono(theme::F_TINY),
                theme::TEXT_FAINT,
            );
            p.text(
                rect.left_center() + egui::vec2(64.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &src,
                theme::mono(theme::F_TINY),
                theme::TEXT_DIM,
            );
            p.text(
                rect.left_center() + egui::vec2(148.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &text,
                theme::mono(theme::F_TINY),
                color,
            );
        }
    });
}

/// Rungs get stable colors so the same state looks the same everywhere.
pub fn rung_color(rung: &str) -> Color32 {
    match rung {
        "combat" => theme::BAD,
        "goto" => theme::ACCENT_TEXT,
        "camp" => theme::WARN,
        "plant" | "defuse" => theme::OK,
        "freeze" | "dead" => theme::DEAD,
        _ => theme::TEXT_DIM,
    }
}
