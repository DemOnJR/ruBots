//! The fleet table: every bot the radar bus has heard from, sortable.
//!
//! This is the screen for "which one is broken". The stuck column is the
//! reason it exists: `vel < 1` while the brain is asking for movement is the
//! tell that a bot is grinding into geometry, and sorting by it puts the
//! offenders on top without hunting for red dots on the radar.

use eframe::egui::{self, Color32};

use crate::state::{role_name, rung_name, site_name};
use crate::{theme, widgets, App, FleetSort};

use super::dashboard::rung_color;
use super::{team_color, team_short};

const COLS: [(&str, f32); 10] = [
    ("bot", 92.0),
    ("team", 44.0),
    ("state", 74.0),
    ("vel", 54.0),
    ("fwd", 54.0),
    ("node", 58.0),
    ("wp", 42.0),
    ("to goal", 68.0),
    ("stuck", 58.0),
    ("role", 64.0),
];

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let (mut table_ui, mut side_ui) = super::two_pane(ui, 300.0);
    table(app, &mut table_ui);
    inspector(app, &mut side_ui);
}

fn sorted_names(app: &App) -> Vec<String> {
    let mut names = app.fleet.names();
    match app.fleet_sort {
        FleetSort::Name => {}
        FleetSort::State => names.sort_by_key(|n| {
            app.fleet
                .bots
                .get(n)
                .map(|b| b.rung())
                .unwrap_or_default()
        }),
        FleetSort::Speed => names.sort_by(|a, b| {
            let va = app.fleet.bots.get(a).map(|b| b.t.vel).unwrap_or(0.0);
            let vb = app.fleet.bots.get(b).map(|b| b.t.vel).unwrap_or(0.0);
            vb.total_cmp(&va)
        }),
        FleetSort::Stuck => names.sort_by(|a, b| {
            let sa = app.fleet.bots.get(a).map(|b| b.stuck_for).unwrap_or(0.0);
            let sb = app.fleet.bots.get(b).map(|b| b.stuck_for).unwrap_or(0.0);
            sb.total_cmp(&sa)
        }),
    }
    names
}

fn table(app: &mut App, ui: &mut egui::Ui) {
    let count = app.fleet.bots.len();
    let stuck = app.fleet.stuck();
    widgets::cell(
        ui,
        "fleet",
        Some((
            &format!("{count} bots · {stuck} stuck"),
            if stuck > 0 { theme::BAD } else { theme::TEXT_FAINT },
        )),
        |ui| {
            ui.horizontal(|ui| {
                for (label, mode) in [
                    ("name", FleetSort::Name),
                    ("state", FleetSort::State),
                    ("speed", FleetSort::Speed),
                    ("stuck", FleetSort::Stuck),
                ] {
                    if widgets::btn_toggle(ui, label, app.fleet_sort == mode).clicked() {
                        app.fleet_sort = mode;
                    }
                }
                ui.add_space(8.0);
                widgets::toggle(ui, &mut app.radar.stuck_only, "stuck only");
            });
            ui.add_space(6.0);

            if count == 0 {
                widgets::empty(
                    ui,
                    "no telemetry yet",
                    "bots broadcast APT1 every 0.5 s once they are in game",
                );
                return;
            }

            let xs = widgets::table_head(ui, &COLS);
            let names = sorted_names(app);
            let mut clicked: Option<String> = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, name) in names.iter().enumerate() {
                        let Some(bot) = app.fleet.bots.get(name) else {
                            continue;
                        };
                        if app.radar.stuck_only && !bot.stuck() {
                            continue;
                        }
                        let selected = app.selected.as_deref() == Some(name.as_str());
                        let (row, resp) = widgets::table_row(ui, 20.0, selected, i % 2 == 1);
                        if resp.clicked() {
                            clicked = Some(name.clone());
                        }
                        let t = bot.t;
                        let color = team_color(t.team, t.alive);
                        let vals: [(String, Color32); 10] = [
                            (name.clone(), if t.alive { theme::TEXT } else { theme::DEAD }),
                            (team_short(t.team).to_string(), color),
                            (bot.rung(), rung_color(&bot.rung())),
                            (format!("{:.0}", t.vel), speed_color(t.vel)),
                            (format!("{:.0}", t.fwd), theme::TEXT_DIM),
                            (
                                if t.node < 0 {
                                    "-".into()
                                } else {
                                    t.node.to_string()
                                },
                                theme::TEXT_DIM,
                            ),
                            (t.waypoints_left.to_string(), theme::TEXT_DIM),
                            (format!("{:.0}", t.to_goal), theme::TEXT_DIM),
                            (
                                if bot.stuck() {
                                    format!("{:.0}s", bot.stuck_for)
                                } else {
                                    "-".into()
                                },
                                if bot.stuck() { theme::BAD } else { theme::TEXT_FAINT },
                            ),
                            (
                                bot.team
                                    .map(|r| role_name(r.role).to_string())
                                    .unwrap_or_else(|| "-".into()),
                                theme::TEXT_FAINT,
                            ),
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
                    }
                });
            if let Some(name) = clicked {
                app.selected = Some(name);
            }
        },
    );
}

fn speed_color(vel: f32) -> Color32 {
    if vel < 1.0 {
        theme::BAD
    } else if vel < 120.0 {
        theme::WARN
    } else {
        theme::OK
    }
}

fn inspector(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            summary(app, ui);
            ui.add_space(10.0);
            detail(app, ui);
        });
}

fn summary(app: &mut App, ui: &mut egui::Ui) {
    let live: Vec<_> = app.fleet.bots.values().filter(|b| b.t.alive).collect();
    let n = live.len().max(1);
    let still = live.iter().filter(|b| b.t.vel < 1.0).count();
    let combat = live.iter().filter(|b| b.rung() == "combat").count();
    let mean_goal = live.iter().map(|b| b.t.to_goal).sum::<f32>() / n as f32;

    // Same-team crowding: the fraction of same-team pairs inside 200 units.
    // Bots piling onto one route is the failure this catches.
    let mut pairs = 0usize;
    let mut close = 0usize;
    for i in 0..live.len() {
        for j in i + 1..live.len() {
            if live[i].t.team != live[j].t.team {
                continue;
            }
            pairs += 1;
            let dx = live[i].t.origin[0] - live[j].t.origin[0];
            let dy = live[i].t.origin[1] - live[j].t.origin[1];
            if (dx * dx + dy * dy).sqrt() <= 200.0 {
                close += 1;
            }
        }
    }
    let crowding = if pairs > 0 {
        close as f32 / pairs as f32
    } else {
        0.0
    };

    widgets::cell(ui, "aggregate", None, |ui| {
        widgets::kv(
            ui,
            "alive",
            &format!(
                "{} (T {} / CT {})",
                live.len(),
                app.fleet.team_count(1),
                app.fleet.team_count(2)
            ),
            theme::TEXT,
        );
        widgets::kv(
            ui,
            "still (vel<1)",
            &format!("{:.0}%", 100.0 * still as f32 / n as f32),
            if still * 2 > n { theme::WARN } else { theme::TEXT_DIM },
        );
        widgets::segments(ui, still as f32 / n as f32, theme::WARN, 22);
        ui.add_space(6.0);
        widgets::kv(ui, "in combat", &format!("{combat}"), theme::TEXT_DIM);
        widgets::kv(ui, "mean to goal", &format!("{mean_goal:.0} u"), theme::TEXT_DIM);
        widgets::kv(
            ui,
            "crowding <=200u",
            &format!("{:.0}%", 100.0 * crowding),
            if crowding > 0.4 { theme::WARN } else { theme::TEXT_DIM },
        );
        widgets::segments(ui, crowding, theme::ACCENT, 22);
    });
}

fn detail(app: &mut App, ui: &mut egui::Ui) {
    let Some(name) = app.selected.clone() else {
        widgets::cell(ui, "detail", None, |ui| {
            widgets::empty(ui, "no bot selected", "click a row");
        });
        return;
    };
    let Some(bot) = app.fleet.bots.get(&name).cloned() else {
        app.selected = None;
        return;
    };
    let t = bot.t;

    // Distance covered over the last ten samples (~5 s): the honest movement
    // number, since a bot can report velocity while going nowhere in a circle.
    let recent: Vec<_> = bot.history.iter().rev().take(10).collect();
    let mut travelled = 0.0f32;
    for w in recent.windows(2) {
        let (_, x0, y0) = *w[0];
        let (_, x1, y1) = *w[1];
        travelled += ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
    }

    widgets::cell(
        ui,
        &name,
        Some((team_short(t.team), team_color(t.team, t.alive))),
        |ui| {
            widgets::kv(ui, "state", &bot.rung(), rung_color(&bot.rung()));
            widgets::kv(
                ui,
                "alive",
                if t.alive { "yes" } else { "no" },
                if t.alive { theme::OK } else { theme::DEAD },
            );
            widgets::kv(ui, "velocity", &format!("{:.0} u/s", t.vel), speed_color(t.vel));
            widgets::kv(
                ui,
                "requested",
                &format!("fwd {:.0} · side {:.0}", t.fwd, t.side),
                theme::TEXT_DIM,
            );
            widgets::kv(
                ui,
                "moved (5 s)",
                &format!("{travelled:.0} u"),
                if travelled < 40.0 { theme::WARN } else { theme::TEXT_DIM },
            );
            widgets::kv(
                ui,
                "stuck for",
                &format!("{:.1}s", bot.stuck_for),
                if bot.stuck() { theme::BAD } else { theme::TEXT_DIM },
            );
            widgets::rule(ui);
            widgets::kv(
                ui,
                "origin",
                &format!("{:.0} {:.0} {:.0}", t.origin[0], t.origin[1], t.origin[2]),
                theme::TEXT_DIM,
            );
            widgets::kv(ui, "yaw", &format!("{:.1} deg", t.yaw), theme::TEXT_DIM);
            widgets::kv(ui, "node", &format!("{}", t.node), theme::TEXT_DIM);
            widgets::kv(
                ui,
                "waypoints left",
                &format!("{}", t.waypoints_left),
                theme::TEXT_DIM,
            );
            widgets::kv(ui, "to goal", &format!("{:.0} u", t.to_goal), theme::TEXT_DIM);
            widgets::rule(ui);
            widgets::kv(
                ui,
                "packets",
                &format!("{} · {:.0}s seen", bot.packets, bot.first_seen.elapsed().as_secs_f32()),
                theme::TEXT_FAINT,
            );
            if let Some(report) = bot.team {
                widgets::rule(ui);
                widgets::kv(ui, "g0 role", role_name(report.role), theme::ACCENT_TEXT);
                widgets::kv(ui, "g0 rung", rung_name(report.rung), theme::TEXT_DIM);
                widgets::kv(
                    ui,
                    "site / contact",
                    &format!(
                        "{} / {}",
                        site_name(report.assigned_site),
                        site_name(report.contact_site)
                    ),
                    theme::TEXT_DIM,
                );
                widgets::kv(
                    ui,
                    "bomb",
                    if report.bomb_planted {
                        "planted"
                    } else if report.bomb_carrier {
                        "carrying"
                    } else {
                        "-"
                    },
                    if report.bomb_planted { theme::WARN } else { theme::TEXT_DIM },
                );
            }
        },
    );
}
