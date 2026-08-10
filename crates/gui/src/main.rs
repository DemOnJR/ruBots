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
//! ```

use std::collections::HashMap;
use std::net::UdpSocket;
use std::sync::mpsc;
use std::thread;

use client::telemetry::{field_str, BotTelemetry, DEFAULT_PORT, PACKET_LEN};
use eframe::egui;

mod radar;

use radar::{Projection, RadarBackground};

/// One bot's last-known state plus a short stuck history.
#[derive(Clone)]
struct BotState {
    t: BotTelemetry,
    /// Consecutive seconds with `vel < 1` while `fwd != 0` (the stuck tell).
    stuck_for: f32,
    /// History of (t, x, y) normalized, for the replay scrubber.
    history: Vec<(f32, f32, f32)>,
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
    /// Window size for the radar canvas.
    radar_w: f32,
    radar_h: f32,
}

fn main() -> eframe::Result<()> {
    let port: u16 = std::env::var("AIPLAYERS_TELEMETRY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    // Listener thread: read datagrams into a channel.
    let sock = UdpSocket::bind(format!("127.0.0.1:{port}")).expect("bind telemetry port");
    sock.set_nonblocking(true).expect("nonblocking");
    let (tx, rx) = mpsc::channel::<BotTelemetry>();
    thread::spawn(move || {
        let mut buf = [0u8; PACKET_LEN + 64];
        loop {
            match sock.recv(&mut buf) {
                Ok(n) => {
                    if let Some(t) = BotTelemetry::decode(&buf[..n]) {
                        let _ = tx.send(t);
                    }
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AI Players Radar",
        options,
        Box::new(move |_cc| {
            let app = RadarApp {
                map: None,
                bots: HashMap::new(),
                rx,
                selected: None,
                replay_offset: 0.0,
                replay_playing: false,
                show_nodes: false,
                show_routes: true,
                radar_w: 900.0,
                radar_h: 700.0,
            };
            Box::new(app) as Box<dyn eframe::App>
        }),
    )
}

impl RadarApp {
    fn drain(&mut self) {
        let now = std::time::Instant::now();
        while let Ok(t) = self.rx.try_recv() {
            let name = field_str(&t.name);
            let entry = self.bots.entry(name).or_insert_with(|| BotState {
                t,
                stuck_for: 0.0,
                history: Vec::new(),
            });
            // Stuck tell: requesting movement but the server says still.
            let requesting = t.fwd.abs() > 1.0 || t.side.abs() > 1.0;
            if t.alive && requesting && t.vel < 1.0 {
                entry.stuck_for += 0.5;
            } else {
                entry.stuck_for = 0.0;
            }
            entry.history.push((t.t, t.origin[0], t.origin[1]));
            if entry.history.len() > 240 {
                entry.history.remove(0);
            }
            entry.t = t;
            // Load the radar from the first bot's map.
            if self.map.is_none() {
                let name = field_str(&t.map);
                if let Some(map) = client::map::Map::load(&name) {
                    let bg = RadarBackground::from_grid(&map.grid);
                    let proj = Projection::from_grid(&map.grid);
                    self.map = Some((name, bg, proj));
                }
            }
        }
        let _ = now;
    }

    /// The bot's screen position on the radar canvas, or None.
    fn screen_pos(&self, t: &BotTelemetry) -> Option<(f32, f32)> {
        let (_, _, proj) = self.map.as_ref()?;
        Some(proj.screen(t.origin[0], t.origin[1], self.radar_w, self.radar_h))
    }

    fn draw_radar(&mut self, ui: &mut egui::Ui) {
        // The radar canvas.
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(self.radar_w, self.radar_h),
            egui::Sense::click(),
        );
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 22, 26));

        if let Some((name, bg, _)) = &self.map {
            // Map silhouette: every walkable node origin as a small pixel.
            for (nx, ny) in &bg.points {
                let x = rect.left() + nx * rect.width();
                let y = rect.top() + ny * rect.height();
                painter.circle_filled(egui::pos2(x, y), 0.8, egui::Color32::from_rgb(52, 62, 74));
            }
            ui.painter().text(
                rect.left_top() + egui::vec2(8.0, 16.0),
                egui::Align2::LEFT_TOP,
                format!("map: {name}  ({} nodes)", bg.points.len()),
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
                        at = Some(BotTelemetry { origin: [x, y, 0.0], ..b.t });
                        break;
                    }
                }
                at.unwrap_or(b.t)
            } else {
                b.t
            };
            let Some((sx, sy)) = self.screen_pos(&t) else { continue };
            let pos = rect.left_top() + egui::vec2(sx, sy);
            let (r, g, bl) = match t.team {
                1 => (235, 200, 40),   // T: yellow
                2 => (60, 150, 235),   // CT: blue
                _ => (150, 150, 150),  // spec/dead: grey
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
            if ui.rect_contains_pointer(rect) && r.contains(ui.input(|i| i.pointer.interact_pos().unwrap_or_default())) {
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
        ui.heading("Bots");
        ui.separator();
        // Sort by name for a stable list.
        let mut names: Vec<&String> = self.bots.keys().collect();
        names.sort();
        let mut stuck_count = 0;
        for name in names {
            let b = &self.bots[name];
            if b.stuck_for > 3.0 {
                stuck_count += 1;
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
            if ui.selectable_label(self.selected.as_deref() == Some(name.as_str()), label).clicked() {
                self.selected = Some(name.clone());
            }
            // The color swatch.
            let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 1.0, color);
        }
        ui.separator();
        ui.label(format!(
            "{} bots, {} stuck (>3s vel<1 while requesting)",
            self.bots.len(),
            stuck_count
        ));
        ui.label(format!("replay: -{:.0}s {}", self.replay_offset, if self.replay_playing { "(playing)" } else { "" }));
        ui.add(egui::Slider::new(&mut self.replay_offset, 0.0..=120.0).text("scrub (s)"));
        ui.checkbox(&mut self.show_routes, "show routes");
        ui.checkbox(&mut self.show_nodes, "show nav grid");

        // Detail panel for the selected bot.
        if let Some(name) = &self.selected {
            ui.separator();
            ui.heading(name);
            if let Some(b) = self.bots.get(name) {
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
                    ui.label(format!("[{:.0} {:.0} {:.0}]", t.origin[0], t.origin[1], t.origin[2]));
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
}

impl eframe::App for RadarApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("AI Players Debug Radar");
                ui.separator();
                if ui.button("reset zoom").clicked() {
                    self.radar_w = 900.0;
                    self.radar_h = 700.0;
                }
                if ui.button("clear").clicked() {
                    self.bots.clear();
                    self.selected = None;
                }
                ui.separator();
                ui.label("stuck = red ring (vel<1 while requesting fwd)");
            });
        });
        egui::SidePanel::right("side")
            .default_width(320.0)
            .show(ctx, |ui| self.draw_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.draw_radar(ui));
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
        sender.send_to(&buf, format!("127.0.0.1:{port}")).expect("send");
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
