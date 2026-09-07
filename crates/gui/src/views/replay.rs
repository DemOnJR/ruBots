//! Watching a recorded session back, inside the app.
//!
//! A bot records what the server sent it as a real `.dem` (see
//! `client::demo`), and `client::replay` decodes that back into "who was
//! where" with the same code the live bot uses. This is the screen that plays
//! it: pick a recording, scrub the timeline, watch the bodies move over the
//! same lattice the radar draws.
//!
//! One thing to know while reading it: a demo is **one bot's view**. The
//! engine only tells a client about entities in its PVS, so a recording shows
//! what that bot could see, not the whole match. That is a property of the
//! recording, not a bug in the player, and the header says so on screen.

use std::path::PathBuf;

use eframe::egui::{self, Align2, Color32, Rounding, Sense, Stroke};

use crate::{theme, widgets, App};

use super::team_color;

/// Everything this screen remembers between frames.
pub struct ReplayState {
    /// Recordings found under the workspace, newest first.
    pub found: Vec<PathBuf>,
    /// Which one is loaded, and the decoded timeline.
    pub loaded: Option<(PathBuf, client::replay::Replay)>,
    pub error: Option<String>,
    pub time: f32,
    pub playing: bool,
    pub speed: f32,
    /// Set when the list needs rebuilding from disk.
    pub rescan: bool,
    last_step: std::time::Instant,
}

impl Default for ReplayState {
    fn default() -> Self {
        Self {
            found: Vec::new(),
            loaded: None,
            error: None,
            time: 0.0,
            playing: false,
            speed: 1.0,
            rescan: true,
            last_step: std::time::Instant::now(),
        }
    }
}

/// Find `.dem` files under `captures/`, newest first.
fn scan(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.join("captures")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("dem")) {
                out.push(path);
            }
        }
    }
    out.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .map(std::cmp::Reverse)
    });
    out
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    if app.replay.rescan {
        app.replay.rescan = false;
        app.replay.found = scan(&app.cfg.root_path());
    }
    advance_clock(app);

    let (mut canvas_ui, mut side_ui) = super::two_pane(ui, 300.0);
    canvas(app, &mut canvas_ui);
    side(app, &mut side_ui);
}

/// Advance playback in real time, scaled by the speed control.
fn advance_clock(app: &mut App) {
    let dt = app.replay.last_step.elapsed().as_secs_f32();
    app.replay.last_step = std::time::Instant::now();
    if !app.replay.playing {
        return;
    }
    let Some((_, replay)) = app.replay.loaded.as_ref() else {
        return;
    };
    app.replay.time += dt * app.replay.speed;
    if app.replay.time >= replay.duration {
        app.replay.time = replay.duration;
        app.replay.playing = false;
    }
}

fn canvas(app: &mut App, ui: &mut egui::Ui) {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(0x03, 0x03, 0x05));
    theme::lattice(&painter, rect, 48.0);
    painter.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0, theme::LINE_BRIGHT));
    theme::corner_ticks(&painter, rect, theme::LINE_BRIGHT);

    let Some((path, replay)) = app.replay.loaded.as_ref() else {
        painter.text(
            rect.center() - egui::vec2(0.0, 8.0),
            Align2::CENTER_CENTER,
            theme::spaced("no recording loaded"),
            theme::mono(theme::F_SMALL),
            theme::TEXT_DIM,
        );
        painter.text(
            rect.center() + egui::vec2(0.0, 12.0),
            Align2::CENTER_CENTER,
            "pick one on the right, or run a bot with RUB_DEMO=1",
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );
        return;
    };

    // The map comes from the same place the radar gets it. Without one there
    // is still a timeline to draw, just no floor under it.
    let projection = app.fleet.map.as_ref().map(|(_, bg, proj)| (bg, proj));
    let (scale, centre) = match projection {
        Some((bg, proj)) => {
            let fit = ((rect.width() - 40.0) / proj.span_x).min((rect.height() - 40.0) / proj.span_y);
            // Floor first, dimmed: this screen is about the bodies.
            for p in &bg.points {
                let x = proj.min_x + p.nx * proj.span_x;
                let y = proj.min_y + (1.0 - p.ny) * proj.span_y;
                let pos = rect.center()
                    + egui::vec2(
                        (x - (proj.min_x + proj.span_x * 0.5)) * fit,
                        -(y - (proj.min_y + proj.span_y * 0.5)) * fit,
                    );
                if !rect.contains(pos) {
                    continue;
                }
                let (r, g, b) = p.band.color();
                painter.rect_filled(
                    egui::Rect::from_center_size(
                        pos,
                        egui::vec2((nav::navgrid::CELL * fit).max(1.5), (nav::navgrid::CELL * fit).max(1.5)),
                    ),
                    Rounding::ZERO,
                    Color32::from_rgb(r, g, b).gamma_multiply(0.30),
                );
            }
            (
                fit,
                (
                    proj.min_x + proj.span_x * 0.5,
                    proj.min_y + proj.span_y * 0.5,
                ),
            )
        }
        None => (0.08, (0.0, 0.0)),
    };

    let world_to_screen = |x: f32, y: f32| {
        rect.center() + egui::vec2((x - centre.0) * scale, -(y - centre.1) * scale)
    };

    // A short trail behind each body, so direction of travel is readable
    // without waiting for the next frame.
    let now = app.replay.time;
    let trail_from = now - 4.0;
    let mut trails: std::collections::HashMap<u16, Vec<egui::Pos2>> = Default::default();
    for snap in replay
        .snapshots
        .iter()
        .filter(|s| s.time >= trail_from && s.time <= now)
    {
        for a in &snap.actors {
            trails
                .entry(a.entity)
                .or_default()
                .push(world_to_screen(a.origin[0], a.origin[1]));
        }
    }
    for (_, points) in &trails {
        for pair in points.windows(2) {
            painter.line_segment(
                [pair[0], pair[1]],
                Stroke::new(1.0, theme::ACCENT_TEXT.gamma_multiply(0.25)),
            );
        }
    }

    if let Some(snap) = replay.at(now) {
        for a in &snap.actors {
            let pos = world_to_screen(a.origin[0], a.origin[1]);
            let color = team_color(a.team, true);
            let body = egui::Rect::from_center_size(pos, egui::vec2(8.0, 8.0));
            painter.rect_filled(body, Rounding::ZERO, color);
            painter.rect_stroke(body, Rounding::ZERO, Stroke::new(1.0, Color32::BLACK));
            // Facing, from the recorded view angles.
            let yaw = f64::from(a.angles[1]).to_radians();
            let tip = pos + egui::vec2(yaw.cos() as f32, -(yaw.sin() as f32)) * 12.0;
            painter.line_segment([pos, tip], Stroke::new(1.0, color));
            painter.text(
                pos + egui::vec2(0.0, 7.0),
                Align2::CENTER_TOP,
                format!("#{}", a.entity),
                theme::mono(theme::F_TINY),
                theme::TEXT_DIM,
            );
        }
    }

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    painter.text(
        rect.left_top() + egui::vec2(12.0, 14.0),
        Align2::LEFT_CENTER,
        format!(
            "{name}  ·  {}  ·  {:.1}s of {:.1}s  ·  one bot's view (PVS)",
            replay.map, now, replay.duration
        ),
        theme::mono(theme::F_TINY),
        theme::TEXT_DIM,
    );
}

fn side(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            transport(app, ui);
            ui.add_space(10.0);
            library(app, ui);
        });
}

fn transport(app: &mut App, ui: &mut egui::Ui) {
    let duration = app
        .replay
        .loaded
        .as_ref()
        .map_or(0.0, |(_, r)| r.duration.max(0.001));
    widgets::cell(ui, "playback", None, |ui| {
        if app.replay.loaded.is_none() {
            widgets::empty(ui, "nothing loaded", "choose a recording below");
            return;
        }
        let mut t = app.replay.time;
        if widgets::scrub(ui, &mut t, duration).dragged() {
            app.replay.playing = false;
        }
        app.replay.time = t;
        widgets::kv(
            ui,
            "time",
            &format!("{:.1}s / {:.1}s", app.replay.time, duration),
            theme::TEXT_DIM,
        );
        ui.horizontal(|ui| {
            let label = if app.replay.playing { "pause" } else { "play" };
            if widgets::btn(ui, label).clicked() {
                app.replay.playing = !app.replay.playing;
                if app.replay.time >= duration {
                    app.replay.time = 0.0;
                }
            }
            if widgets::btn(ui, "start").clicked() {
                app.replay.time = 0.0;
            }
        });
        ui.horizontal(|ui| {
            for speed in [0.5f32, 1.0, 2.0, 4.0] {
                if widgets::btn_toggle(
                    ui,
                    &format!("{speed}x"),
                    (app.replay.speed - speed).abs() < 0.01,
                )
                .clicked()
                {
                    app.replay.speed = speed;
                }
            }
        });
        if let Some((_, r)) = app.replay.loaded.as_ref() {
            widgets::rule(ui);
            widgets::kv(ui, "map", &r.map, theme::TEXT_DIM);
            widgets::kv(ui, "snapshots", &format!("{}", r.snapshots.len()), theme::TEXT_DIM);
            widgets::kv(
                ui,
                "undecoded",
                &format!("{}", r.undecoded),
                if r.undecoded > 0 { theme::WARN } else { theme::TEXT_FAINT },
            );
        }
    });
}

fn library(app: &mut App, ui: &mut egui::Ui) {
    let count = app.replay.found.len();
    widgets::cell(
        ui,
        "recordings",
        Some((&format!("{count} found"), theme::TEXT_FAINT)),
        |ui| {
            if widgets::btn(ui, "rescan").clicked() {
                app.replay.rescan = true;
            }
            if let Some(err) = app.replay.error.clone() {
                widgets::kv(ui, "error", &err, theme::BAD);
            }
            ui.add_space(4.0);
            if count == 0 {
                widgets::empty(
                    ui,
                    "no .dem files",
                    "run a bot with RUB_DEMO=1 to record one",
                );
                return;
            }
            let mut open: Option<PathBuf> = None;
            for path in &app.replay.found {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let selected = app
                    .replay
                    .loaded
                    .as_ref()
                    .is_some_and(|(p, _)| p == path);
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                let (row, resp) = widgets::table_row(ui, 20.0, selected, false);
                widgets::cell_text(ui, row, 0.0, &name, theme::TEXT, theme::F_SMALL);
                widgets::cell_text(
                    ui,
                    row,
                    row.width() - 70.0,
                    &format!("{:.1} MB", size as f32 / (1024.0 * 1024.0)),
                    theme::TEXT_FAINT,
                    theme::F_TINY,
                );
                if resp.clicked() {
                    open = Some(path.clone());
                }
            }
            if let Some(path) = open {
                match client::replay::Replay::load(&path) {
                    Ok(replay) => {
                        app.replay.time = 0.0;
                        app.replay.playing = true;
                        app.replay.error = None;
                        app.replay.loaded = Some((path, replay));
                    }
                    Err(e) => app.replay.error = Some(e.to_string()),
                }
            }
        },
    );
}
