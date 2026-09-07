//! Settings: ports, paths and swarm defaults, saved next to the executable.
//!
//! Every path is shown with whether it actually exists, because "nothing
//! happens when I press launch" is almost always a path that is not where the
//! shell assumed it would be — a copied `gui.exe`, a moved workspace, a debug
//! binary while the release profile is selected.

use eframe::egui;

use crate::config::Config;
use crate::{theme, widgets, App};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let (mut left, mut right) = super::two_pane(ui, 360.0);
    fields(app, &mut left);
    paths(app, &mut right);
}

fn fields(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::cell(ui, "workspace", None, |ui| {
                let mut root = app.cfg.root.clone();
                widgets::field(ui, "root", &mut root);
                app.cfg.root = root;
                widgets::kv(
                    ui,
                    "detected",
                    &crate::config::default_root().to_string_lossy(),
                    theme::TEXT_FAINT,
                );
                if widgets::btn(ui, "use detected").clicked() {
                    app.cfg.root = crate::config::default_root().to_string_lossy().into_owned();
                }
            });

            ui.add_space(10.0);
            widgets::cell(ui, "telemetry", None, |ui| {
                widgets::field_num(ui, "radar port", &mut app.cfg.telemetry_port, 1, 65535);
                widgets::field_num(ui, "team port", &mut app.cfg.team_port, 0, 65535);
                widgets::kv(
                    ui,
                    "bound now",
                    &format!(
                        "{} / {}",
                        app.feed.radar_port,
                        app.feed
                            .team_port
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "off".into())
                    ),
                    theme::TEXT_DIM,
                );
                widgets::kv(
                    ui,
                    "note",
                    "ports bind at startup — restart to change",
                    theme::TEXT_FAINT,
                );
                if let Some(err) = app.feed.error.clone() {
                    widgets::kv(ui, "bind error", &err, theme::BAD);
                }
            });

            ui.add_space(10.0);
            widgets::cell(ui, "swarm defaults", None, |ui| {
                let mut addr = app.cfg.addr.clone();
                widgets::field(ui, "address", &mut addr);
                app.cfg.addr = addr;
                widgets::field_num(ui, "bots", &mut app.cfg.bots, 1, 64);
                widgets::field_num(ui, "seconds", &mut app.cfg.secs, 15, 36_000);
                widgets::field_num(ui, "stagger ms", &mut app.cfg.stagger_ms, 0, 30_000);
                let mut name = app.cfg.name_prefix.clone();
                widgets::field(ui, "name prefix", &mut name);
                app.cfg.name_prefix = name;
                let mut key = app.cfg.key_prefix.clone();
                widgets::field(ui, "key prefix", &mut key);
                app.cfg.key_prefix = key;
                let (sample_name, sample_key) = app.cfg.bot_identity(7);
                widgets::kv(
                    ui,
                    "bot 7 will be",
                    &format!("{sample_name} / {sample_key}"),
                    theme::TEXT_FAINT,
                );
                widgets::kv(
                    ui,
                    "id parsing",
                    "the digits in the name are the G0 bot id",
                    theme::TEXT_FAINT,
                );
            });

            ui.add_space(10.0);
            widgets::cell(ui, "config file", None, |ui| {
                widgets::kv(
                    ui,
                    "path",
                    &Config::path().to_string_lossy(),
                    theme::TEXT_DIM,
                );
                ui.horizontal(|ui| {
                    if widgets::btn_primary(ui, "save").clicked() {
                        match app.cfg.save() {
                            Ok(()) => app.say("settings saved", theme::OK),
                            Err(e) => app.say(format!("save failed: {e}"), theme::BAD),
                        }
                    }
                    if widgets::btn(ui, "reload").clicked() {
                        app.cfg = Config::load();
                        app.say("settings reloaded", theme::ACCENT_TEXT);
                    }
                    if widgets::btn(ui, "defaults").clicked() {
                        app.cfg = Config::default();
                        app.say("settings reset (not saved)", theme::WARN);
                    }
                });
                widgets::kv(
                    ui,
                    "autosave",
                    "settings are also written on exit",
                    theme::TEXT_FAINT,
                );
            });
        });
}

fn paths(app: &mut App, ui: &mut egui::Ui) {
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            widgets::cell(ui, "resolved paths", None, |ui| {
                let entries = [
                    ("root", app.cfg.root_path()),
                    ("bot runner", app.cfg.runner_path()),
                    ("testserver", app.cfg.testserver_path()),
                    ("maps", app.cfg.maps_dir()),
                    ("captures", app.cfg.root_path().join("captures")),
                ];
                for (label, path) in entries {
                    let ok = path.exists();
                    widgets::kv(
                        ui,
                        label,
                        if ok { "found" } else { "missing" },
                        if ok { theme::OK } else { theme::BAD },
                    );
                    let w = ui.available_width();
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(w, 15.0), egui::Sense::hover());
                    ui.painter().text(
                        rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        path.to_string_lossy(),
                        theme::mono(theme::F_TINY),
                        theme::TEXT_FAINT,
                    );
                    ui.add_space(2.0);
                }
            });

            ui.add_space(10.0);
            widgets::cell(ui, "environment handed to bots", None, |ui| {
                let pairs = [
                    ("RUB_TELEMETRY_PORT", app.cfg.telemetry_port.to_string()),
                    (
                        "RUB_TEAM_PORT",
                        if app.cfg.team_port > 0 {
                            app.cfg.team_port.to_string()
                        } else {
                            "unset".into()
                        },
                    ),
                    ("RUB_MAP", app.cfg.map.clone()),
                    (
                        "RUB_DIFFICULTY",
                        if app.cfg.difficulty.is_empty() {
                            "unset (seeded)".into()
                        } else {
                            app.cfg.difficulty.clone()
                        },
                    ),
                    (
                        "RUB_MAPS_DIR",
                        app.cfg.maps_dir().to_string_lossy().into_owned(),
                    ),
                ];
                for (k, v) in pairs {
                    widgets::kv(ui, k, &v, theme::TEXT_DIM);
                }
                widgets::rule(ui);
                widgets::kv(
                    ui,
                    "per bot",
                    "RUB_NAME · RUB_KEY · RUB_TEAM",
                    theme::TEXT_FAINT,
                );
            });

            ui.add_space(10.0);
            widgets::cell(ui, "about", None, |ui| {
                widgets::kv(ui, "app", "ruBots Control", theme::TEXT_DIM);
                widgets::kv(ui, "version", env!("CARGO_PKG_VERSION"), theme::TEXT_DIM);
                widgets::kv(ui, "radar packet", "APT1 · 128 bytes", theme::TEXT_FAINT);
                widgets::kv(ui, "team packet", "APT2 · 80 bytes", theme::TEXT_FAINT);
                widgets::kv(ui, "log", "gui.log next to the exe", theme::TEXT_FAINT);
            });
        });
}
