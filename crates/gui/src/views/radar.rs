//! The radar: the swarm on the nav lattice, live or scrubbed back in time.
//!
//! The background is generated from the nav grid itself — every walkable node
//! is a point — so it is accurate on any map with no per-map art. Two things
//! are different from the original debug window: the projection now preserves
//! aspect ratio (the old one normalised each axis independently, which
//! stretched every map to the window), and pan/zoom are a real camera rather
//! than a canvas resize, so zooming keeps the point under the cursor still.

use eframe::egui::{self, Align2, Color32, Rounding, Sense, Stroke};

use crate::state::{role_name, site_name};
use crate::{theme, widgets, App};

use super::{team_color, team_short};
use super::dashboard::rung_color;

/// World units drawn by the scale bar.
const SCALE_UNITS: f32 = 512.0;

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let (mut canvas_ui, mut side_ui) = super::two_pane(ui, 300.0);
    canvas(app, &mut canvas_ui);
    inspector(app, &mut side_ui);
}

/// Screen position of a world point under the current camera.
fn project(
    app: &App,
    rect: egui::Rect,
    scale: f32,
    world_center: (f32, f32),
    x: f32,
    y: f32,
) -> egui::Pos2 {
    rect.center()
        + egui::vec2(
            (x - world_center.0) * scale,
            -(y - world_center.1) * scale,
        )
        + app.radar.pan
}

fn canvas(app: &mut App, ui: &mut egui::Ui) {
    let rect = ui.available_rect_before_wrap();
    let resp = ui.allocate_rect(rect, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(0x03, 0x03, 0x05));
    theme::lattice(&painter, rect, 48.0);
    painter.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0, theme::LINE_BRIGHT));
    theme::corner_ticks(&painter, rect, theme::LINE_BRIGHT);

    let Some((map_name, bg, proj)) = &app.fleet.map else {
        painter.text(
            rect.center() - egui::vec2(0.0, 8.0),
            Align2::CENTER_CENTER,
            theme::spaced("no map loaded"),
            theme::mono(theme::F_SMALL),
            theme::TEXT_DIM,
        );
        painter.text(
            rect.center() + egui::vec2(0.0, 12.0),
            Align2::CENTER_CENTER,
            "the radar loads the map named in the first telemetry packet",
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );
        return;
    };

    // Fit the whole map, then apply the camera. One scale for both axes:
    // a stretched radar makes angles lie, and angles are the point.
    let fit = ((rect.width() - 40.0) / proj.span_x).min((rect.height() - 40.0) / proj.span_y);
    let scale = fit * app.radar.zoom;
    let world_center = (
        proj.min_x + proj.span_x * 0.5,
        proj.min_y + proj.span_y * 0.5,
    );

    // --- camera input ----------------------------------------------------
    if resp.dragged() {
        app.radar.pan += resp.drag_delta();
    }
    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
    if scroll.abs() > 0.0 {
        if let Some(hover) = resp.hover_pos() {
            let old = scale;
            let new_zoom = (app.radar.zoom * (1.0 + scroll * 0.0015)).clamp(0.4, 12.0);
            let new = fit * new_zoom;
            // Keep the world point under the cursor exactly where it is.
            let world = (
                world_center.0 + (hover.x - rect.center().x - app.radar.pan.x) / old,
                world_center.1 - (hover.y - rect.center().y - app.radar.pan.y) / old,
            );
            app.radar.zoom = new_zoom;
            app.radar.pan = hover
                - rect.center()
                - egui::vec2(
                    (world.0 - world_center.0) * new,
                    -(world.1 - world_center.1) * new,
                );
        }
    }
    let scale = fit * app.radar.zoom;

    // --- lattice ---------------------------------------------------------
    if app.radar.show_nodes {
        for p in &bg.points {
            let show = match p.band {
                crate::radar::HeightBand::Low => app.radar.show_low,
                crate::radar::HeightBand::Mid => app.radar.show_mid,
                crate::radar::HeightBand::High => app.radar.show_high,
            };
            if !show {
                continue;
            }
            let x = proj.min_x + p.nx * proj.span_x;
            let y = proj.min_y + (1.0 - p.ny) * proj.span_y;
            let pos = project(app, rect, scale, world_center, x, y);
            if !rect.contains(pos) {
                continue;
            }
            let (r, g, b) = p.band.color();
            // One filled square per lattice cell, at the cell's real size, so
            // the walkable area reads as floor rather than as a dot cloud.
            // Dimmed hard: this is the ground the eye should look past, and
            // the bots are what it should look at.
            let mut color = Color32::from_rgb(r, g, b).gamma_multiply(0.42);
            let mut side = (nav::navgrid::CELL * scale).max(1.5);
            if p.goal && app.radar.show_goals {
                color = theme::BAD.gamma_multiply(0.65);
            } else if p.ladder && app.radar.show_ladders {
                color = theme::OK.gamma_multiply(0.8);
                side = side.min(6.0).max(3.0);
            } else if p.narrow {
                // Door-scale cells are a hint, not a headline: every node that
                // touches a wall is narrow, so painting them loudly outlines
                // the whole map in one colour and drowns the floor.
                color = Color32::from_rgb(0x5A, 0x4A, 0x86).gamma_multiply(0.45);
            }
            painter.rect_filled(
                egui::Rect::from_center_size(pos, egui::vec2(side, side)),
                Rounding::ZERO,
                color,
            );
        }
    }

    // --- special hops ----------------------------------------------------
    if app.radar.show_hops {
        for h in &bg.hops {
            let on = match h.kind {
                nav::navgrid::Move::Jump => app.radar.show_jumps,
                nav::navgrid::Move::Crouch => app.radar.show_crouch,
                nav::navgrid::Move::Ladder => app.radar.show_ladders,
                nav::navgrid::Move::Fall => app.radar.show_falls,
                _ => false,
            };
            if !on {
                continue;
            }
            let a = project(
                app,
                rect,
                scale,
                world_center,
                proj.min_x + h.ax * proj.span_x,
                proj.min_y + (1.0 - h.ay) * proj.span_y,
            );
            let b = project(
                app,
                rect,
                scale,
                world_center,
                proj.min_x + h.bx * proj.span_x,
                proj.min_y + (1.0 - h.by) * proj.span_y,
            );
            if !rect.contains(a) && !rect.contains(b) {
                continue;
            }
            let (r, g, bl) = crate::radar::hop_color(h.kind);
            painter.line_segment(
                [a, b],
                Stroke::new(1.0, Color32::from_rgb(r, g, bl).gamma_multiply(0.8)),
            );
        }
    }

    // --- bots ------------------------------------------------------------
    let now = app.fleet.now();
    let replay_at = now - app.radar.replay;
    let scrubbing = app.radar.replay > 0.5;
    let mut click_target: Option<(f32, String)> = None;
    let pointer = resp.interact_pointer_pos();

    let names = app.fleet.names();
    for name in &names {
        let Some(bot) = app.fleet.bots.get(name) else {
            continue;
        };
        if app.radar.stuck_only && !bot.stuck() {
            continue;
        }
        let (wx, wy) = if scrubbing {
            bot.at(replay_at)
        } else {
            (bot.t.origin[0], bot.t.origin[1])
        };
        let pos = project(app, rect, scale, world_center, wx, wy);
        let selected = app.selected.as_deref() == Some(name.as_str());

        // Trail: where it has been, fading out.
        if app.radar.show_trails && bot.history.len() > 1 {
            let take = 40.min(bot.history.len());
            let start = bot.history.len() - take;
            let mut prev: Option<egui::Pos2> = None;
            for (i, &(_, hx, hy)) in bot.history.iter().skip(start).enumerate() {
                let p = project(app, rect, scale, world_center, hx, hy);
                if let Some(q) = prev {
                    let a = 0.10 + 0.5 * (i as f32 / take as f32);
                    painter.line_segment(
                        [q, p],
                        Stroke::new(1.0, team_color(bot.t.team, true).gamma_multiply(a)),
                    );
                }
                prev = Some(p);
            }
        }

        let color = if bot.stuck() {
            theme::BAD
        } else {
            team_color(bot.t.team, bot.t.alive)
        };
        let side = if bot.t.alive { 7.0 } else { 5.0 };
        let body = egui::Rect::from_center_size(pos, egui::vec2(side, side));
        painter.rect_filled(body, Rounding::ZERO, color);
        painter.rect_stroke(body, Rounding::ZERO, Stroke::new(1.0, Color32::BLACK));

        if bot.stuck() {
            painter.rect_stroke(
                body.expand(4.0),
                Rounding::ZERO,
                Stroke::new(1.0, theme::BAD),
            );
        }
        if selected {
            theme::corner_ticks(&painter, body.expand(7.0), theme::ACCENT_TEXT);
        }
        if app.radar.show_heading && bot.t.alive {
            // CS yaw 0 faces +x; screen y is flipped.
            let rad = (bot.t.yaw as f64).to_radians();
            let tip = pos
                + egui::vec2(rad.cos() as f32, -(rad.sin() as f32)) * (side * 0.5 + 9.0);
            painter.line_segment([pos, tip], Stroke::new(1.0, color.gamma_multiply(0.9)));
        }
        if app.radar.show_labels {
            painter.text(
                pos + egui::vec2(0.0, side * 0.5 + 2.0),
                Align2::CENTER_TOP,
                name,
                theme::mono(theme::F_TINY),
                if selected { theme::TEXT } else { theme::TEXT_DIM },
            );
        }
        if let Some(click) = pointer {
            let d = (click - pos).length();
            if d < 16.0 && click_target.as_ref().is_none_or(|(best, _)| d < *best) {
                click_target = Some((d, name.clone()));
            }
        }
    }
    if resp.clicked() {
        app.selected = click_target.map(|(_, name)| name);
    }

    // --- what the selected bot should be watching ------------------------
    // Drawn from the map, not from telemetry: the packet does not carry it,
    // and the shell has the same nav grid the bot does. Seeing the sight lines
    // next to the heading tick is the whole point -- it answers "is it looking
    // at the way in, or at a wall" without reading a log.
    if app.radar.show_watch {
        if let (Some(name), Some(map)) = (app.selected.clone(), app.fleet.map_data.as_ref()) {
            if let Some(bot) = app.fleet.bots.get(&name) {
                let here = bot.t.origin;
                let stale = app
                    .radar
                    .watch_cache
                    .as_ref()
                    .is_none_or(|(who, at, _)| {
                        who != &name
                            || {
                                let (dx, dy) = (here[0] - at[0], here[1] - at[1]);
                                (dx * dx + dy * dy).sqrt() > 64.0
                            }
                    });
                if stale {
                    let world = nav::navgrid::World::new(&map.bsp, &map.info);
                    // A CT watches the T spawns and the other way round.
                    let spawns: &[[f32; 3]] = if bot.t.team == 2 {
                        &map.info.t_spawns
                    } else {
                        &map.info.ct_spawns
                    };
                    let points = nav::watch::watch_points(
                        &map.grid,
                        &world,
                        here,
                        spawns,
                        nav::watch::DEFAULT_RADIUS,
                        4,
                    );
                    app.radar.watch_cache = Some((name.clone(), here, points));
                }
                if let Some((_, _, points)) = app.radar.watch_cache.as_ref() {
                    let from = project(app, rect, scale, world_center, here[0], here[1]);
                    for p in points {
                        let to = project(app, rect, scale, world_center, p[0], p[1]);
                        painter.line_segment(
                            [from, to],
                            Stroke::new(1.0, theme::ACCENT_TEXT.gamma_multiply(0.45)),
                        );
                        painter.rect_stroke(
                            egui::Rect::from_center_size(to, egui::vec2(7.0, 7.0)),
                            Rounding::ZERO,
                            Stroke::new(1.0, theme::ACCENT_TEXT),
                        );
                    }
                }
            }
        }
    }

    // --- overlay ---------------------------------------------------------
    let head = format!(
        "{map_name}  ·  {} nodes  ·  {} hops  ·  z {:.0}..{:.0}",
        bg.points.len(),
        bg.hops.len(),
        bg.z_min,
        bg.z_max
    );
    painter.text(
        rect.left_top() + egui::vec2(12.0, 14.0),
        Align2::LEFT_CENTER,
        head,
        theme::mono(theme::F_TINY),
        theme::TEXT_DIM,
    );
    if scrubbing {
        painter.text(
            rect.center_top() + egui::vec2(0.0, 14.0),
            Align2::CENTER_CENTER,
            format!("REPLAY  -{:.0}s", app.radar.replay),
            theme::mono(theme::F_SMALL),
            theme::WARN,
        );
    }
    // Scale bar: a radar without one invites guessing at distances.
    let bar_px = SCALE_UNITS * scale;
    if bar_px > 20.0 && bar_px < rect.width() * 0.6 {
        let y = rect.bottom() - 18.0;
        let x0 = rect.left() + 12.0;
        painter.line_segment(
            [egui::pos2(x0, y), egui::pos2(x0 + bar_px, y)],
            Stroke::new(1.0, theme::TEXT_DIM),
        );
        for x in [x0, x0 + bar_px] {
            painter.line_segment(
                [egui::pos2(x, y - 3.0), egui::pos2(x, y + 3.0)],
                Stroke::new(1.0, theme::TEXT_DIM),
            );
        }
        painter.text(
            egui::pos2(x0 + bar_px + 8.0, y),
            Align2::LEFT_CENTER,
            format!("{SCALE_UNITS:.0} u"),
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );
    }
    painter.text(
        rect.right_bottom() + egui::vec2(-12.0, -18.0),
        Align2::RIGHT_CENTER,
        format!("{:.1}x  ·  drag to pan  ·  scroll to zoom", app.radar.zoom),
        theme::mono(theme::F_TINY),
        theme::TEXT_FAINT,
    );
}

fn inspector(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::cell(ui, "camera", None, |ui| {
                ui.horizontal(|ui| {
                    if widgets::btn(ui, "fit").clicked() {
                        app.radar.zoom = 1.0;
                        app.radar.pan = egui::Vec2::ZERO;
                    }
                    if widgets::btn(ui, "+").clicked() {
                        app.radar.zoom = (app.radar.zoom * 1.25).min(12.0);
                    }
                    if widgets::btn(ui, "-").clicked() {
                        app.radar.zoom = (app.radar.zoom / 1.25).max(0.4);
                    }
                });
                widgets::kv(ui, "zoom", &format!("{:.2}x", app.radar.zoom), theme::TEXT_DIM);
                widgets::kv(
                    ui,
                    "pan",
                    &format!("{:.0}, {:.0}", app.radar.pan.x, app.radar.pan.y),
                    theme::TEXT_DIM,
                );
            });

            ui.add_space(10.0);
            widgets::cell(ui, "replay", None, |ui| {
                let mut replay = app.radar.replay;
                if widgets::scrub(ui, &mut replay, 120.0).dragged() {
                    app.radar.playing = false;
                }
                app.radar.replay = replay;
                widgets::kv(
                    ui,
                    "offset",
                    &if app.radar.replay < 0.5 {
                        "live".to_string()
                    } else {
                        format!("-{:.0}s", app.radar.replay)
                    },
                    if app.radar.replay < 0.5 {
                        theme::OK
                    } else {
                        theme::WARN
                    },
                );
                ui.horizontal(|ui| {
                    let label = if app.radar.playing { "pause" } else { "play" };
                    if widgets::btn(ui, label).clicked() {
                        app.radar.playing = !app.radar.playing;
                    }
                    if widgets::btn(ui, "live").clicked() {
                        app.radar.replay = 0.0;
                        app.radar.playing = false;
                    }
                });
                // Playback walks the offset back toward live in real time.
                if app.radar.playing {
                    let dt = app.radar.last_step.elapsed().as_secs_f32();
                    if dt > 0.05 {
                        app.radar.replay = (app.radar.replay - dt).max(0.0);
                        app.radar.last_step = std::time::Instant::now();
                        if app.radar.replay <= 0.0 {
                            app.radar.playing = false;
                        }
                    }
                } else {
                    app.radar.last_step = std::time::Instant::now();
                }
            });

            ui.add_space(10.0);
            widgets::cell(ui, "layers", None, |ui| {
                widgets::toggle(ui, &mut app.radar.show_nodes, "nav lattice");
                widgets::toggle(ui, &mut app.radar.show_hops, "special hops");
                widgets::toggle(ui, &mut app.radar.show_trails, "trails");
                widgets::toggle(ui, &mut app.radar.show_heading, "heading");
                widgets::toggle(ui, &mut app.radar.show_labels, "names");
                widgets::toggle(ui, &mut app.radar.stuck_only, "stuck only");
                widgets::rule(ui);
                widgets::toggle(ui, &mut app.radar.show_low, "low / tunnels");
                widgets::toggle(ui, &mut app.radar.show_mid, "mid");
                widgets::toggle(ui, &mut app.radar.show_high, "high platforms");
                widgets::rule(ui);
                widgets::toggle(ui, &mut app.radar.show_jumps, "jumps");
                widgets::toggle(ui, &mut app.radar.show_crouch, "crouch / doors");
                widgets::toggle(ui, &mut app.radar.show_ladders, "ladders");
                widgets::toggle(ui, &mut app.radar.show_falls, "falls");
                widgets::toggle(ui, &mut app.radar.show_goals, "bomb sites");
                widgets::rule(ui);
                widgets::toggle(ui, &mut app.radar.show_watch, "sight lines (selected)");
            });

            ui.add_space(10.0);
            legend(ui);

            ui.add_space(10.0);
            selection(app, ui);
        });
}

/// What the colors mean. Drawn from the same constants the canvas uses, so a
/// palette change cannot leave the legend lying.
fn legend(ui: &mut egui::Ui) {
    widgets::cell(ui, "legend", None, |ui| {
        let rows: [(&str, egui::Color32); 8] = [
            ("terrorist", theme::TEAM_T),
            ("counter-terrorist", theme::TEAM_CT),
            ("dead", theme::DEAD),
            ("stuck", theme::BAD),
            ("bomb site", theme::BAD),
            ("ladder", theme::OK),
            ("jump hop", egui::Color32::from_rgb(255, 180, 40)),
            ("crouch / door", egui::Color32::from_rgb(180, 100, 220)),
        ];
        for (label, color) in rows {
            ui.horizontal(|ui| {
                widgets::swatch(ui, color, 8.0);
                ui.add_space(6.0);
                widgets::kv(ui, label, "", theme::TEXT_DIM);
            });
        }
    });
}

fn selection(app: &mut App, ui: &mut egui::Ui) {
    let Some(name) = app.selected.clone() else {
        widgets::cell(ui, "selection", None, |ui| {
            widgets::empty(ui, "nothing selected", "click a bot on the radar");
        });
        return;
    };
    let Some(bot) = app.fleet.bots.get(&name).cloned() else {
        app.selected = None;
        return;
    };
    let t = bot.t;
    let state_color = if bot.stuck() {
        theme::BAD
    } else {
        rung_color(&bot.rung())
    };
    widgets::cell(
        ui,
        &name,
        Some((team_short(t.team), team_color(t.team, t.alive))),
        |ui| {
            widgets::kv(ui, "state", &bot.rung(), state_color);
            widgets::kv(
                ui,
                "alive",
                if t.alive { "yes" } else { "no" },
                if t.alive { theme::OK } else { theme::DEAD },
            );
            widgets::kv(
                ui,
                "origin",
                &format!("{:.0} {:.0} {:.0}", t.origin[0], t.origin[1], t.origin[2]),
                theme::TEXT_DIM,
            );
            widgets::kv(ui, "yaw", &format!("{:.1} deg", t.yaw), theme::TEXT_DIM);
            widgets::kv(
                ui,
                "vel / fwd / side",
                &format!("{:.0} / {:.0} / {:.0}", t.vel, t.fwd, t.side),
                theme::TEXT_DIM,
            );
            widgets::kv(
                ui,
                "node / waypoints",
                &format!("{} / {}", t.node, t.waypoints_left),
                theme::TEXT_DIM,
            );
            widgets::kv(ui, "to goal", &format!("{:.0} u", t.to_goal), theme::TEXT_DIM);
            widgets::kv(
                ui,
                "stuck",
                &format!("{:.1}s", bot.stuck_for),
                if bot.stuck() { theme::BAD } else { theme::TEXT_DIM },
            );
            widgets::kv(ui, "packets", &format!("{}", bot.packets), theme::TEXT_FAINT);
            if let Some(team) = bot.team {
                widgets::rule(ui);
                widgets::kv(ui, "role (g0)", role_name(team.role), theme::ACCENT_TEXT);
                widgets::kv(ui, "site", site_name(team.assigned_site), theme::TEXT_DIM);
                widgets::kv(
                    ui,
                    "contact",
                    site_name(team.contact_site),
                    theme::TEXT_DIM,
                );
                widgets::kv(
                    ui,
                    "bomb",
                    if team.bomb_planted {
                        "planted"
                    } else if team.bomb_carrier {
                        "carrying"
                    } else {
                        "-"
                    },
                    if team.bomb_planted { theme::WARN } else { theme::TEXT_DIM },
                );
            }
        },
    );
}
