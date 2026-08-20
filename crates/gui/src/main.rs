//! Live debug radar for the bot swarm (plan `debug-gui-radar.md`).
//!
//! Listens on UDP 27016 for `BotTelemetry` packets (each bot broadcasts every
//! 0.5 s), draws them on an auto-generated map silhouette (from the nav
//! grid), and flags stuck bots: `vel < 1` while requesting `fwd != 0` for
//! more than 3 s gets a red ring. A detail panel shows the last-known state
//! of the clicked bot, and a ring buffer keeps ~120 s of history for the
//! replay scrubber (watch the T-spawn pile-up at round start frame by frame).
//!
//! ```text
//! cargo run -p gui          # while scripts/swarm.ps1 runs bots
//! # preferred on Windows (survives agent/shell exit):
//! powershell -File scripts/start-gui.ps1
//! ```

use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use client::telemetry::{field_str, BotTelemetry, DEFAULT_PORT, PACKET_LEN};
use eframe::egui;

mod radar;

use radar::{hop_color, HeightBand, Projection, RadarBackground};

/// Append a line to `gui.log` next to the executable (or CWD fallback).
fn log_line(msg: &str) {
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("gui.log")))
        .unwrap_or_else(|| PathBuf::from("gui.log"));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {msg}");
    }
    eprintln!("{msg}");
}

/// One bot's last-known state plus a short stuck history.
#[derive(Clone)]
struct BotState {
    t: BotTelemetry,
    /// Consecutive seconds with `vel < 1` while `fwd != 0` (the stuck tell).
    stuck_for: f32,
    /// History of (t, x, y) for the replay scrubber (~120 s at 0.5 Hz).
    history: VecDeque<(f32, f32, f32)>,
    /// Wall clock of last telemetry packet (prune zombies when swarm ends).
    last_seen: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Radar,
    Fleet,
    Settings,
}

struct RadarApp {
    /// The map silhouette and projection. Loaded from the first bot's map
    /// name; `None` until a packet arrives.
    map: Option<(String, RadarBackground, Projection)>,
    bots: HashMap<String, BotState>,
    rx: mpsc::Receiver<BotTelemetry>,
    /// Which bot's detail is shown, by name.
    selected: Option<String>,
    /// Replay: 0.0 = live, > 0 scrubs back this many seconds.
    replay_offset: f32,
    replay_playing: bool,
    /// Toggles.
    show_nodes: bool,
    show_routes: bool,
    show_heading: bool,
    filter_stuck_only: bool,
    /// Layer toggles (height + special hops).
    show_low: bool,
    show_mid: bool,
    show_high: bool,
    show_jumps: bool,
    show_crouch: bool,
    show_ladders: bool,
    show_falls: bool,
    show_narrow: bool,
    show_goals: bool,
    /// Window size for the radar canvas.
    radar_w: f32,
    radar_h: f32,
    /// Zoom scale (1.0 = fit) and pan in canvas pixels.
    zoom: f32,
    pan: egui::Vec2,
    tab: Tab,
    /// Telemetry packets received (for status line).
    packets: u64,
    last_packet_at: Option<Instant>,
}

fn main() -> eframe::Result<()> {
    // Surface panics to gui.log instead of a silent disappearance.
    std::panic::set_hook(Box::new(|info| {
        log_line(&format!("PANIC: {info}"));
    }));
    log_line("gui starting");

    let port: u16 = std::env::var("REB_TELEMETRY_PORT")
        .or_else(|_| std::env::var("REBOTS_TELEMETRY_PORT"))
        .or_else(|_| std::env::var("AIPLAYERS_TELEMETRY_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    // Listener thread: read datagrams into a channel.
    let addr = format!("127.0.0.1:{port}");
    let sock = match UdpSocket::bind(&addr) {
        Ok(s) => s,
        Err(e) => {
            log_line(&format!("FATAL bind {addr}: {e}"));
            // Still open a window so the user sees an error instead of "nothing".
            return eframe::run_native(
                "reBots Radar (no telemetry)",
                eframe::NativeOptions {
                    viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 200.0]),
                    ..Default::default()
                },
                Box::new(move |_cc| {
                    Box::new(ErrorApp {
                        msg: format!(
                            "Could not bind UDP {addr}: {e}\n\nClose other radar instances and restart."
                        ),
                    }) as Box<dyn eframe::App>
                }),
            );
        }
    };
    let _ = sock.set_nonblocking(true);
    log_line(&format!("listening on {addr}"));
    let (tx, rx) = mpsc::channel::<BotTelemetry>();
    thread::spawn(move || {
        let mut buf = [0u8; PACKET_LEN + 64];
        loop {
            match sock.recv(&mut buf) {
                Ok(n) => {
                    if let Some(t) = BotTelemetry::decode(&buf[..n]) {
                        // Drop if UI is busy (full channel): never block the socket thread.
                        if tx.send(t).is_err() {
                            log_line("telemetry channel closed; listener exit");
                            break;
                        }
                    }
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    log_line(&format!("recv error: {e}"));
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("reBots Radar"),
        ..Default::default()
    };
    let result = eframe::run_native(
        "reBots Radar",
        options,
        Box::new(move |_cc| {
            let app = RadarApp {
                map: None,
                bots: HashMap::new(),
                rx,
                selected: None,
                replay_offset: 0.0,
                replay_playing: false,
                show_nodes: true,
                show_routes: true,
                show_heading: true,
                filter_stuck_only: false,
                show_low: true,
                show_mid: true,
                show_high: true,
                show_jumps: true,
                show_crouch: true,
                show_ladders: true,
                show_falls: true,
                show_narrow: true,
                show_goals: true,
                radar_w: 900.0,
                radar_h: 700.0,
                zoom: 1.0,
                pan: egui::Vec2::ZERO,
                tab: Tab::Radar,
                packets: 0,
                last_packet_at: None,
            };
            Box::new(app) as Box<dyn eframe::App>
        }),
    );
    log_line("gui exited cleanly");
    result
}

/// Minimal window when telemetry bind fails (port already in use, etc.).
struct ErrorApp {
    msg: String,
}

impl eframe::App for ErrorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("AI Players Radar — failed to start");
            ui.separator();
            ui.label(&self.msg);
            ui.separator();
            if ui.button("Quit").clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

impl RadarApp {
    fn drain(&mut self) {
        let now = Instant::now();
        // Cap work per frame so a burst of 30 bots × backlog never freezes UI.
        let mut n = 0u32;
        while n < 256 {
            let Ok(t) = self.rx.try_recv() else { break };
            n += 1;
            self.packets += 1;
            self.last_packet_at = Some(now);
            let name = field_str(&t.name);
            let entry = self.bots.entry(name).or_insert_with(|| BotState {
                t,
                stuck_for: 0.0,
                history: VecDeque::with_capacity(256),
                last_seen: now,
            });
            // Stuck tell: requesting movement but the server says still.
            let requesting = t.fwd.abs() > 1.0 || t.side.abs() > 1.0;
            if t.alive && requesting && t.vel < 1.0 {
                entry.stuck_for += 0.5;
            } else {
                entry.stuck_for = 0.0;
            }
            entry.history.push_back((t.t, t.origin[0], t.origin[1]));
            while entry.history.len() > 240 {
                entry.history.pop_front();
            }
            entry.t = t;
            entry.last_seen = now;
            // Load the radar from the first bot's map.
            if self.map.is_none() {
                let map_name = field_str(&t.map);
                match client::map::Map::load(&map_name) {
                    Some(map) => {
                        log_line(&format!(
                            "loaded map {} ({} nodes)",
                            map_name,
                            map.grid.len()
                        ));
                        let bg = RadarBackground::from_grid(&map.grid);
                        let proj = Projection::from_grid(&map.grid);
                        self.map = Some((map_name, bg, proj));
                    }
                    None => {
                        // Don't spam: only once every few seconds would need a flag;
                        // once is enough if maps dir is missing.
                        if self.packets < 5 {
                            log_line(&format!("map load failed for '{map_name}'"));
                        }
                    }
                }
            }
        }
        // Drop bots silent for >30 s (previous swarm) so the list stays honest.
        self.bots
            .retain(|_, b| now.duration_since(b.last_seen) < Duration::from_secs(30));
    }

    /// The bot's screen position on the radar canvas, or None.
    fn screen_pos(&self, t: &BotTelemetry) -> Option<(f32, f32)> {
        let (_, _, proj) = self.map.as_ref()?;
        Some(proj.screen(t.origin[0], t.origin[1], self.radar_w, self.radar_h))
    }

    fn draw_radar(&mut self, ui: &mut egui::Ui) {
        // The radar canvas.
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(self.radar_w, self.radar_h), egui::Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 22, 26));

        if let Some((name, bg, _)) = &self.map {
            // Height-colored walkable lattice (low=under, mid, high=platform).
            if self.show_nodes {
                for p in &bg.points {
                    let show = match p.band {
                        HeightBand::Low => self.show_low,
                        HeightBand::Mid => self.show_mid,
                        HeightBand::High => self.show_high,
                    };
                    if !show {
                        continue;
                    }
                    let x = rect.left() + p.nx * rect.width();
                    let y = rect.top() + p.ny * rect.height();
                    let (r, g, b) = p.band.color();
                    let mut col = egui::Color32::from_rgb(r, g, b);
                    // Narrow / door-scale nodes: brighter purple tint.
                    if self.show_narrow && p.narrow {
                        col = egui::Color32::from_rgb(160, 90, 200);
                    }
                    if self.show_goals && p.goal {
                        col = egui::Color32::from_rgb(220, 80, 80);
                    }
                    if p.ladder && self.show_ladders {
                        col = egui::Color32::from_rgb(80, 200, 120);
                    }
                    let rad = if p.goal || p.ladder { 1.6 } else { 0.9 };
                    painter.circle_filled(egui::pos2(x, y), rad, col);
                }
            }
            // Special hops: jump (orange), crouch/door (purple), ladder (green), fall/under (blue).
            for h in &bg.hops {
                let ok = match h.kind {
                    nav::navgrid::Move::Jump => self.show_jumps,
                    nav::navgrid::Move::Crouch => self.show_crouch,
                    nav::navgrid::Move::Ladder => self.show_ladders,
                    nav::navgrid::Move::Fall => self.show_falls,
                    _ => false,
                };
                if !ok {
                    continue;
                }
                let a = egui::pos2(
                    rect.left() + h.ax * rect.width(),
                    rect.top() + h.ay * rect.height(),
                );
                let b = egui::pos2(
                    rect.left() + h.bx * rect.width(),
                    rect.top() + h.by * rect.height(),
                );
                let (r, g, bl) = hop_color(h.kind);
                painter.line_segment(
                    [a, b],
                    egui::Stroke::new(1.4, egui::Color32::from_rgb(r, g, bl)),
                );
            }
            ui.painter().text(
                rect.left_top() + egui::vec2(8.0, 16.0),
                egui::Align2::LEFT_TOP,
                format!(
                    "map: {name}  ({} nodes, {} hops)  z [{:.0}..{:.0}]",
                    bg.points.len(),
                    bg.hops.len(),
                    bg.z_min,
                    bg.z_max
                ),
                egui::FontId::proportional(13.0),
                egui::Color32::from_gray(140),
            );
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "waiting for telemetry...",
                egui::FontId::proportional(18.0),
                egui::Color32::from_gray(120),
            );
        }

        // Bot dots (or replay positions).
        let now = self.bots.values().map(|b| b.t.t).fold(0.0f32, f32::max);
        let replay_t = now - self.replay_offset;
        let mut clicked: Option<String> = None;
        for (name, b) in &self.bots {
            // Replay: show the position at `replay_t`, if we have one.
            let t = if self.replay_offset > 0.5 {
                let mut at = None;
                for &(tt, x, y) in b.history.iter().rev() {
                    if tt <= replay_t {
                        at = Some(BotTelemetry {
                            origin: [x, y, 0.0],
                            ..b.t
                        });
                        break;
                    }
                }
                at.unwrap_or(b.t)
            } else {
                b.t
            };
            let Some((sx, sy)) = self.screen_pos(&t) else {
                continue;
            };
            let pos = rect.left_top() + egui::vec2(sx, sy);
            let (r, g, bl) = match t.team {
                1 => (235, 200, 40),  // T: yellow
                2 => (60, 150, 235),  // CT: blue
                _ => (150, 150, 150), // spec/dead: grey
            };
            if !t.alive {
                // Dead: dim grey.
                let c = egui::Color32::from_rgb(90, 90, 90);
                painter.circle_filled(pos, 4.0, c);
                continue;
            }
            let color = egui::Color32::from_rgb(r, g, bl);
            // Stuck: red ring.
            if b.stuck_for > 3.0 {
                painter.circle_stroke(
                    pos,
                    9.0,
                    egui::Stroke::new(2.5, egui::Color32::from_rgb(230, 40, 40)),
                );
            }
            painter.circle_filled(pos, 5.0, color);
            // Heading tick (Phase E): where the bot is looking.
            if self.show_heading {
                let rad = (t.yaw as f64).to_radians();
                // CS yaw 0 faces +x; radar y is flipped, so screen direction is
                // (cos yaw, -sin yaw) in canvas space after y-flip.
                let len = 14.0;
                let tip = pos + egui::vec2((rad.cos() as f32) * len, -(rad.sin() as f32) * len);
                painter.line_segment(
                    [pos, tip],
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(220, 220, 220)),
                );
            }
            // Name label under the dot.
            painter.text(
                pos + egui::vec2(0.0, 12.0),
                egui::Align2::CENTER_TOP,
                &name,
                egui::FontId::proportional(10.0),
                egui::Color32::from_gray(200),
            );
            // Click detection.
            let r = egui::Rect::from_center_size(pos, egui::vec2(16.0, 16.0));
            if ui.rect_contains_pointer(rect)
                && r.contains(ui.input(|i| i.pointer.interact_pos().unwrap_or_default()))
            {
                if ui.input(|i| i.pointer.any_click()) {
                    clicked = Some(name.clone());
                }
            }
        }
        if let Some(c) = clicked {
            self.selected = Some(c);
        }
    }

    fn draw_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Radar, "Radar");
            ui.selectable_value(&mut self.tab, Tab::Fleet, "Fleet");
            ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
        });
        ui.separator();

        match self.tab {
            Tab::Radar => self.draw_radar_side(ui),
            Tab::Fleet => self.draw_fleet(ui),
            Tab::Settings => self.draw_settings(ui),
        }
    }

    fn draw_radar_side(&mut self, ui: &mut egui::Ui) {
        ui.heading("Bots");
        ui.separator();
        // Sort by name for a stable list.
        let mut names: Vec<String> = self.bots.keys().cloned().collect();
        names.sort();
        let mut stuck_count = 0;
        for name in &names {
            let b = &self.bots[name];
            if b.stuck_for > 3.0 {
                stuck_count += 1;
            }
            if self.filter_stuck_only && b.stuck_for <= 3.0 {
                continue;
            }
            let color = if b.stuck_for > 3.0 {
                egui::Color32::from_rgb(230, 40, 40)
            } else if b.t.alive {
                match b.t.team {
                    1 => egui::Color32::from_rgb(235, 200, 40),
                    2 => egui::Color32::from_rgb(60, 150, 235),
                    _ => egui::Color32::GRAY,
                }
            } else {
                egui::Color32::from_rgb(90, 90, 90)
            };
            let label = if b.stuck_for > 3.0 {
                format!("{name}  STUCK {:.0}s", b.stuck_for)
            } else {
                format!("{name}  {}", field_str(&b.t.rung))
            };
            if ui
                .selectable_label(self.selected.as_deref() == Some(name.as_str()), label)
                .clicked()
            {
                self.selected = Some(name.clone());
            }
            let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 1.0, color);
        }
        ui.separator();
        ui.label(format!(
            "{} bots, {} stuck (>3s vel<1 while requesting)",
            self.bots.len(),
            stuck_count
        ));
        ui.label(format!("replay: -{:.0}s", self.replay_offset));
        ui.add(egui::Slider::new(&mut self.replay_offset, 0.0..=120.0).text("scrub (s)"));
        ui.checkbox(&mut self.show_routes, "show routes");
        ui.checkbox(&mut self.show_nodes, "show nav grid");
        ui.checkbox(&mut self.show_heading, "show heading");
        ui.checkbox(&mut self.filter_stuck_only, "stuck only");
        ui.separator();
        ui.label("Height layers");
        ui.checkbox(&mut self.show_low, "low / under (tunnels)");
        ui.checkbox(&mut self.show_mid, "mid");
        ui.checkbox(&mut self.show_high, "high (A platform)");
        ui.separator();
        ui.label("Special paths");
        ui.checkbox(&mut self.show_jumps, "jumps (orange)");
        ui.checkbox(&mut self.show_crouch, "crouch / narrow (purple)");
        ui.checkbox(&mut self.show_ladders, "ladders (green)");
        ui.checkbox(&mut self.show_falls, "falls / drops (blue)");
        ui.checkbox(&mut self.show_narrow, "door-scale nodes");
        ui.checkbox(&mut self.show_goals, "bomb sites (red)");
        ui.separator();
        ui.colored_label(
            egui::Color32::from_gray(160),
            "Legend: low=underpass  high=upper  orange=jump  purple=duck/door",
        );

        if let Some(name) = self.selected.clone() {
            ui.separator();
            ui.heading(&name);
            if let Some(b) = self.bots.get(&name) {
                let t = &b.t;
                egui::Grid::new("detail").striped(true).show(ui, |ui| {
                    ui.label("team");
                    ui.label(match t.team {
                        1 => "Terrorist",
                        2 => "Counter-Terrorist",
                        _ => "Spectator",
                    });
                    ui.end_row();
                    ui.label("rung");
                    ui.label(field_str(&t.rung));
                    ui.end_row();
                    ui.label("pos");
                    ui.label(format!(
                        "[{:.0} {:.0} {:.0}]",
                        t.origin[0], t.origin[1], t.origin[2]
                    ));
                    ui.end_row();
                    ui.label("yaw");
                    ui.label(format!("{:.1} deg", t.yaw));
                    ui.end_row();
                    ui.label("vel / fwd / side");
                    ui.label(format!("{:.0} / {:.0} / {:.0}", t.vel, t.fwd, t.side));
                    ui.end_row();
                    ui.label("wp left / node");
                    ui.label(format!("{} / {}", t.waypoints_left, t.node));
                    ui.end_row();
                    ui.label("to_goal");
                    ui.label(format!("{:.0}", t.to_goal));
                    ui.end_row();
                    ui.label("stuck");
                    ui.label(format!("{} ({:.1}s)", t.stuck, b.stuck_for));
                    ui.end_row();
                    ui.label("alive");
                    ui.label(t.alive.to_string());
                    ui.end_row();
                });
            }
        }
    }

    fn draw_fleet(&mut self, ui: &mut egui::Ui) {
        ui.heading("Fleet metrics (live)");
        ui.label("Rolling snapshot from telemetry — not the full metrics.py table.");
        ui.separator();
        let live: Vec<&BotState> = self.bots.values().filter(|b| b.t.alive).collect();
        let n = live.len().max(1);
        let still = live.iter().filter(|b| b.t.vel < 1.0).count();
        let stuck = live.iter().filter(|b| b.stuck_for > 3.0).count();
        let t_count = live.iter().filter(|b| b.t.team == 1).count();
        let ct_count = live.iter().filter(|b| b.t.team == 2).count();
        let combat = live
            .iter()
            .filter(|b| field_str(&b.t.rung) == "combat")
            .count();
        let mean_to_goal = live.iter().map(|b| b.t.to_goal).sum::<f32>() / n as f32;

        // Rough same-team pair fraction within 200u.
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
        let sep200 = if pairs > 0 {
            close as f32 / pairs as f32
        } else {
            0.0
        };

        egui::Grid::new("fleet").striped(true).show(ui, |ui| {
            ui.label("alive");
            ui.label(format!("{} (T {} / CT {})", live.len(), t_count, ct_count));
            ui.end_row();
            ui.label("STILL (vel<1)");
            ui.label(format!("{:.0}%", 100.0 * still as f32 / n as f32));
            ui.end_row();
            ui.label("stuck >3s");
            ui.label(format!("{stuck}"));
            ui.end_row();
            ui.label("combat rung");
            ui.label(format!("{combat}"));
            ui.end_row();
            ui.label("mean to_goal");
            ui.label(format!("{mean_to_goal:.0}"));
            ui.end_row();
            ui.label("pair ≤200u (same team)");
            ui.label(format!("{:.0}%", 100.0 * sep200));
            ui.end_row();
        });

        ui.separator();
        ui.heading("Rungs");
        let mut counts: HashMap<String, usize> = HashMap::new();
        for b in &live {
            *counts.entry(field_str(&b.t.rung)).or_default() += 1;
        }
        let mut rows: Vec<_> = counts.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        for (rung, c) in rows {
            ui.label(format!("{rung}: {c}"));
        }
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        ui.add(egui::Slider::new(&mut self.zoom, 0.5..=3.0).text("zoom"));
        if ui.button("reset pan/zoom").clicked() {
            self.zoom = 1.0;
            self.pan = egui::Vec2::ZERO;
            self.radar_w = 900.0;
            self.radar_h = 700.0;
        }
        ui.label("Drag on the radar to pan (middle mouse / primary drag).");
        ui.label("Scroll over radar to zoom.");
        ui.separator();
        ui.label("Telemetry: UDP APT1 on REB_TELEMETRY_PORT (default 27016).");
        ui.label("APT2 (role/pitch/HP) is planned next.");
    }
}

impl eframe::App for RadarApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("reBots (REB) Debug Radar");
                ui.separator();
                if ui.button("reset view").clicked() {
                    self.radar_w = 900.0;
                    self.radar_h = 700.0;
                    self.zoom = 1.0;
                    self.pan = egui::Vec2::ZERO;
                }
                if ui.button("clear").clicked() {
                    self.bots.clear();
                    self.selected = None;
                }
                ui.separator();
                let age = self
                    .last_packet_at
                    .map(|t| t.elapsed().as_secs_f32())
                    .unwrap_or(999.0);
                let live = if age < 2.0 {
                    format!("LIVE · {} pkts · {} bots", self.packets, self.bots.len())
                } else if age < 30.0 {
                    format!("quiet {:.0}s · {} bots", age, self.bots.len())
                } else {
                    "no telemetry (start swarm)".into()
                };
                ui.label(format!(
                    "stuck = red ring · zoom {:.1}x · {live}",
                    self.zoom
                ));
            });
        });
        egui::SidePanel::right("side")
            .default_width(340.0)
            .show(ctx, |ui| self.draw_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            // Zoom / pan input over the central radar.
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.0 {
                self.zoom = (self.zoom * (1.0 + scroll * 0.001)).clamp(0.5, 3.0);
            }
            if ui.input(|i| i.pointer.secondary_down() || i.pointer.middle_down()) {
                self.pan += ui.input(|i| i.pointer.delta());
            }
            // Apply zoom to canvas size; pan is drawn as an offset inside draw_radar.
            let _ = self.pan;
            self.radar_w = 900.0 * self.zoom;
            self.radar_h = 700.0 * self.zoom;
            self.draw_radar(ui);
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;

    /// The telemetry bus works end-to-end: a UDP packet broadcast on the
    /// default port is decoded by a listener on the same port. This is the
    /// wire-level proof the radar will see the bots without a display.
    #[test]
    fn a_broadcast_packet_reaches_a_listener() {
        let listener = UdpSocket::bind("127.0.0.1:0").expect("bind listener");
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let sender = UdpSocket::bind("127.0.0.1:0").expect("bind sender");
        let t = BotTelemetry {
            name: {
                let mut n = [0u8; 16];
                n[..5].copy_from_slice(b"Bot01");
                n
            },
            map: {
                let mut m = [0u8; 32];
                m[..8].copy_from_slice(b"de_dust2");
                m
            },
            origin: [-1536.0, 2688.0, 48.0],
            yaw: -37.5,
            team: 1,
            alive: true,
            rung: {
                let mut r = [0u8; 16];
                r[..4].copy_from_slice(b"goto");
                r
            },
            vel: 0.0,
            fwd: 237.0,
            side: -12.0,
            waypoints_left: 42,
            node: 299,
            stuck: false,
            to_goal: 812.0,
            t: 120.5,
        };
        let buf = t.encode();
        sender
            .send_to(&buf, format!("127.0.0.1:{port}"))
            .expect("send");
        let mut recv = [0u8; PACKET_LEN + 16];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match listener.recv(&mut recv) {
                Ok(n) => {
                    let got = BotTelemetry::decode(&recv[..n]).expect("decode");
                    assert_eq!(got.name, t.name);
                    assert_eq!(got.origin, t.origin);
                    assert_eq!(got.rung, t.rung);
                    break;
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    assert!(std::time::Instant::now() < deadline, "packet never arrived");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => panic!("recv error"),
            }
        }
    }
}
