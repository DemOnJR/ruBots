//! ruBots Control — the shell around the whole workspace.
//!
//! One window that does the things the README asks you to do from four
//! terminals: build the bot runner, bring the docker test server up, deploy a
//! staggered swarm, watch the radar, read every bot's log, and see the
//! server's own player count next to what the bots claim.
//!
//! ```text
//! cargo run -p gui
//! ```
//!
//! Structure:
//! * [`theme`] — the look (black, hairlines, zero rounding, one accent).
//! * [`widgets`] — the square widget set everything is drawn from.
//! * [`state`] — the two UDP telemetry feeds, aggregated per bot.
//! * [`proc`] — child processes: bots, docker, cargo.
//! * [`probe`] — A2S_INFO, the server's own view of the swarm.
//! * [`views`] — one module per screen.
//!
//! The radar itself is unchanged in substance from the original debug window
//! (`radar.rs` still generates the silhouette from the nav grid, so it works
//! on any map with no per-map art); what changed is that it is now one view
//! inside an application rather than the whole application.

#![windows_subsystem = "windows"]

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui::{self, Align2, Color32, Rounding, Sense, Stroke};

mod config;
mod probe;
mod proc;
mod radar;
mod state;
mod theme;
mod views;
mod widgets;

use config::Config;
use proc::{Kind, Level, Supervisor};
use state::{Feed, Fleet};

/// Append a line to `gui.log` next to the executable.
///
/// The window has no console (`windows_subsystem = "windows"`), so a panic
/// before the first frame would otherwise be a silent disappearance.
pub fn log_line(msg: &str) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dashboard,
    Radar,
    Fleet,
    Deploy,
    Server,
    Console,
    Settings,
}

impl View {
    pub const ALL: [View; 7] = [
        View::Dashboard,
        View::Radar,
        View::Fleet,
        View::Deploy,
        View::Server,
        View::Console,
        View::Settings,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Radar => "radar",
            Self::Fleet => "fleet",
            Self::Deploy => "deploy",
            Self::Server => "server",
            Self::Console => "console",
            Self::Settings => "settings",
        }
    }

    /// One line under the title: what this screen is for.
    pub fn blurb(self) -> &'static str {
        match self {
            Self::Dashboard => "swarm, telemetry and process health at a glance",
            Self::Radar => "live positions on the nav lattice, with replay",
            Self::Fleet => "every bot's state, sortable, with the stuck tell",
            Self::Deploy => "launch and stop bots, staggered to survive ReAuthCheck",
            Self::Server => "the docker test server and its own A2S player count",
            Self::Console => "every child process's output in one stream",
            Self::Settings => "ports, paths and swarm defaults",
        }
    }
}

/// Radar canvas state: what is drawn and where the camera is.
pub struct RadarState {
    pub zoom: f32,
    pub pan: egui::Vec2,
    pub show_nodes: bool,
    pub show_hops: bool,
    pub show_heading: bool,
    pub show_trails: bool,
    pub show_labels: bool,
    pub show_low: bool,
    pub show_mid: bool,
    pub show_high: bool,
    pub show_jumps: bool,
    pub show_crouch: bool,
    pub show_ladders: bool,
    pub show_falls: bool,
    pub show_goals: bool,
    /// Draw what the selected bot ought to be watching, from the map.
    pub show_watch: bool,
    pub stuck_only: bool,
    /// Cached sight lines: which bot, where it was, and what came back.
    ///
    /// The answer costs four route searches and a trace per node, so it is
    /// computed once per selection and reused until the bot has moved a
    /// meaningful distance.
    pub watch_cache: Option<(String, [f32; 3], Vec<[f32; 3]>)>,
    /// Seconds behind live; 0 = live.
    pub replay: f32,
    pub playing: bool,
    last_step: Instant,
}

impl Default for RadarState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            show_nodes: true,
            show_hops: true,
            show_heading: true,
            show_trails: true,
            show_labels: true,
            show_low: true,
            show_mid: true,
            show_high: true,
            show_jumps: true,
            show_crouch: true,
            show_ladders: true,
            show_falls: true,
            show_goals: true,
            show_watch: true,
            stuck_only: false,
            watch_cache: None,
            replay: 0.0,
            playing: false,
            last_step: Instant::now(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogFilter {
    All,
    Bots,
    Server,
    Build,
    Errors,
}

pub struct ConsoleState {
    pub filter: LogFilter,
    pub search: String,
    pub follow: bool,
}

impl Default for ConsoleState {
    fn default() -> Self {
        Self {
            filter: LogFilter::All,
            search: String::new(),
            follow: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FleetSort {
    Name,
    State,
    Speed,
    Stuck,
}

/// The A2S probe result, shared with the worker thread that fills it.
type ProbeSlot = std::sync::Arc<std::sync::Mutex<Option<Result<probe::A2sInfo, String>>>>;

pub struct ServerState {
    pub info: ProbeSlot,
    pub probing: bool,
    pub last_probe: Option<Instant>,
    /// Re-probe every 5 s while the server view is open.
    pub auto: bool,
}

pub struct App {
    pub cfg: Config,
    pub fleet: Fleet,
    pub feed: Feed,
    pub sup: Supervisor,
    pub view: View,
    pub selected: Option<String>,
    pub radar: RadarState,
    pub console: ConsoleState,
    pub fleet_sort: FleetSort,
    pub server: ServerState,
    pub started: Instant,
    /// A short message in the status bar: what just happened.
    pub toast: Option<(String, Color32, Instant)>,
    closing: bool,
}

impl App {
    fn new(cfg: Config, feed: Feed) -> Self {
        let sup = Supervisor::new();
        if let Some(err) = &feed.error {
            sup.note(
                "telemetry",
                Level::Error,
                format!("{err} - another radar is probably already running"),
            );
        } else {
            sup.note(
                "telemetry",
                Level::Meta,
                format!("listening for APT1 on udp/{}", feed.radar_port),
            );
        }
        if let Some(port) = feed.team_port {
            sup.note("telemetry", Level::Meta, format!("joined G0 bus on udp/{port}"));
        }
        Self {
            cfg,
            fleet: Fleet::new(),
            feed,
            sup,
            view: View::Dashboard,
            selected: None,
            radar: RadarState::default(),
            console: ConsoleState::default(),
            fleet_sort: FleetSort::Name,
            server: ServerState {
                info: Default::default(),
                probing: false,
                last_probe: None,
                auto: true,
            },
            started: Instant::now(),
            toast: None,
            closing: false,
        }
    }

    pub fn say(&mut self, text: impl Into<String>, color: Color32) {
        self.toast = Some((text.into(), color, Instant::now()));
    }

    /// The environment every bot is launched with, minus its identity.
    fn base_env(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("RUB_TELEMETRY_PORT".into(), self.cfg.telemetry_port.to_string()),
            ("RUB_MAP".into(), self.cfg.map.clone()),
            (
                "RUB_MAPS_DIR".into(),
                self.cfg.maps_dir().to_string_lossy().into_owned(),
            ),
            (
                "RUB_CSTRIKE_DIR".into(),
                self.cfg
                    .testserver_path()
                    .join("cstrike")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ];
        if self.cfg.team_port > 0 {
            env.push(("RUB_TEAM_PORT".into(), self.cfg.team_port.to_string()));
        }
        if !self.cfg.difficulty.is_empty() {
            env.push(("RUB_DIFFICULTY".into(), self.cfg.difficulty.clone()));
        }
        env
    }

    /// Queue a staggered swarm. Teams alternate, exits are spread out.
    pub fn deploy(&mut self) {
        let runner = self.cfg.runner_path();
        if !runner.exists() {
            self.sup.note(
                "deploy",
                Level::Error,
                format!("runner missing: {} - build it first", runner.display()),
            );
            self.say("runner missing - press BUILD RUNNER", theme::BAD);
            return;
        }
        let root = self.cfg.root_path();
        let captures = root.join("captures").join("swarm");
        let _ = std::fs::create_dir_all(&captures);
        let stagger = Duration::from_millis(self.cfg.stagger_ms as u64);
        let base = self.base_env();

        for i in 1..=self.cfg.bots {
            let (name, key) = self.cfg.bot_identity(i);
            let team = if i % 2 == 1 { 1u8 } else { 2u8 };
            // Staggered lifetimes, so the fleet leaves spread out as well: a
            // simultaneous mass disconnect is the documented ban trigger.
            let life = self.cfg.secs + 5 * (i - 1) / 2;
            let mut env = base.clone();
            env.push(("RUB_NAME".into(), name.clone()));
            env.push(("RUB_KEY".into(), key));
            env.push(("RUB_TEAM".into(), team.to_string()));
            let args = vec![
                self.cfg.addr.clone(),
                life.to_string(),
                captures
                    .join(format!("{name}.bin"))
                    .to_string_lossy()
                    .into_owned(),
            ];
            self.sup.queue(
                stagger * (i - 1),
                &name,
                team,
                &runner,
                args,
                env,
                &root,
            );
        }
        self.sup.note(
            "deploy",
            Level::Meta,
            format!(
                "queued {} bots at {} ms apart against {}",
                self.cfg.bots, self.cfg.stagger_ms, self.cfg.addr
            ),
        );
        self.say(format!("deploying {} bots", self.cfg.bots), theme::ACCENT_TEXT);
    }

    pub fn stop_bots(&mut self) {
        let stagger = Duration::from_millis(self.cfg.stagger_ms.min(2000) as u64);
        self.sup.stop_all(Kind::Bot, stagger);
        self.say("stopping bots", theme::WARN);
    }

    pub fn build_runner(&mut self) {
        let root = self.cfg.root_path();
        let mut args = vec![
            "build".to_string(),
            "-p".into(),
            "client".into(),
            "--example".into(),
            "capture_running".into(),
        ];
        if self.cfg.release {
            args.push("--release".into());
        }
        self.sup.spawn(
            Kind::Build,
            "cargo",
            0,
            std::path::Path::new("cargo"),
            &args,
            &[("CARGO_TERM_COLOR".into(), "never".into())],
            &root,
        );
        self.say("building the bot runner", theme::ACCENT_TEXT);
    }

    /// Run `docker compose <args>` in `testserver/`.
    pub fn docker(&mut self, args: &[&str], label: &str) {
        let dir = self.cfg.testserver_path();
        if !dir.exists() {
            self.sup.note(
                "docker",
                Level::Error,
                format!("no testserver directory at {}", dir.display()),
            );
            self.say("testserver/ not found - check the root path", theme::BAD);
            return;
        }
        let mut full = vec!["compose".to_string()];
        full.extend(args.iter().map(|a| a.to_string()));
        self.sup.spawn(
            Kind::Server,
            label,
            0,
            std::path::Path::new("docker"),
            &full,
            &[],
            &dir,
        );
    }

    /// Ask the game server itself who is connected.
    pub fn probe_server(&mut self) {
        if self.server.probing {
            return;
        }
        self.server.probing = true;
        self.server.last_probe = Some(Instant::now());
        let slot = std::sync::Arc::clone(&self.server.info);
        let addr = self.cfg.addr.clone();
        std::thread::spawn(move || {
            let result = probe::query(&addr, Duration::from_millis(1200));
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(result);
            }
        });
    }

    /// True while the last probe thread has not written its answer yet.
    fn poll_probe(&mut self) {
        if !self.server.probing {
            return;
        }
        let done = self
            .server
            .info
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false);
        if done {
            self.server.probing = false;
        } else if self
            .server
            .last_probe
            .is_some_and(|t| t.elapsed() > Duration::from_secs(3))
        {
            // The worker is bounded by its own read timeout; if it is later
            // than that, treat the probe as finished so the button comes back.
            self.server.probing = false;
        }
    }

    fn nav_rail(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        theme::lattice(ui.painter(), full, theme::GRID_STEP);
        ui.add_space(10.0);

        for (i, view) in View::ALL.iter().enumerate() {
            let selected = self.view == *view;
            let w = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), Sense::click());
            if resp.clicked() {
                self.view = *view;
            }
            let p = ui.painter();
            if selected {
                p.rect_filled(rect, Rounding::ZERO, theme::SURFACE_2);
                p.rect_filled(
                    egui::Rect::from_min_size(rect.left_top(), egui::vec2(2.0, rect.height())),
                    Rounding::ZERO,
                    theme::ACCENT,
                );
            } else if resp.hovered() {
                p.rect_filled(rect, Rounding::ZERO, Color32::from_rgb(0x0A, 0x0A, 0x0D));
            }
            p.text(
                rect.left_center() + egui::vec2(14.0, 0.0),
                Align2::LEFT_CENTER,
                format!("{:02}", i + 1),
                theme::mono(theme::F_TINY),
                if selected {
                    theme::ACCENT_TEXT
                } else {
                    theme::TEXT_FAINT
                },
            );
            p.text(
                rect.left_center() + egui::vec2(40.0, 0.0),
                Align2::LEFT_CENTER,
                theme::spaced(view.name()),
                theme::mono(theme::F_SMALL),
                if selected {
                    theme::TEXT
                } else if resp.hovered() {
                    theme::TEXT_DIM
                } else {
                    theme::TEXT_DIM
                },
            );
            // Badges: what needs attention on that screen right now.
            let badge = match view {
                View::Fleet | View::Radar if self.fleet.stuck() > 0 => {
                    Some((format!("{}", self.fleet.stuck()), theme::BAD))
                }
                View::Deploy if self.sup.queued() > 0 => {
                    Some((format!("+{}", self.sup.queued()), theme::WARN))
                }
                View::Deploy if self.sup.running(Kind::Bot) > 0 => {
                    Some((format!("{}", self.sup.running(Kind::Bot)), theme::OK))
                }
                _ => None,
            };
            if let Some((text, color)) = badge {
                p.text(
                    rect.right_center() + egui::vec2(-12.0, 0.0),
                    Align2::RIGHT_CENTER,
                    text,
                    theme::mono(theme::F_TINY),
                    color,
                );
            }
        }

        // Footer: the two things that are always true and always wanted.
        let footer = egui::Rect::from_min_max(
            egui::pos2(full.left(), full.bottom() - 74.0),
            egui::pos2(full.right(), full.bottom()),
        );
        let p = ui.painter();
        p.line_segment(
            [
                egui::pos2(footer.left() + 12.0, footer.top()),
                egui::pos2(footer.right() - 12.0, footer.top()),
            ],
            Stroke::new(1.0, theme::LINE),
        );
        let lines = [
            format!("udp/{}", self.feed.radar_port),
            match self.feed.team_port {
                Some(p) => format!("g0 udp/{p}"),
                None => "g0 off".to_string(),
            },
            format!("up {}", fmt_dur(self.started.elapsed())),
        ];
        for (i, line) in lines.iter().enumerate() {
            p.text(
                egui::pos2(footer.left() + 14.0, footer.top() + 16.0 + i as f32 * 15.0),
                Align2::LEFT_CENTER,
                line,
                theme::mono(theme::F_TINY),
                theme::TEXT_FAINT,
            );
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let p = ui.painter();
        theme::heavy_text(
            p,
            egui::pos2(rect.left() + 18.0, rect.center().y),
            Align2::LEFT_CENTER,
            "RUBOTS",
            17.0,
            theme::TEXT,
        );
        p.text(
            egui::pos2(rect.left() + 104.0, rect.center().y + 1.0),
            Align2::LEFT_CENTER,
            "CONTROL",
            theme::mono(theme::F_TINY),
            theme::ACCENT_TEXT,
        );
        p.line_segment(
            [
                egui::pos2(rect.left() + 176.0, rect.top() + 10.0),
                egui::pos2(rect.left() + 176.0, rect.bottom() - 10.0),
            ],
            Stroke::new(1.0, theme::LINE_BRIGHT),
        );
        p.text(
            egui::pos2(rect.left() + 196.0, rect.center().y - 6.0),
            Align2::LEFT_CENTER,
            theme::spaced(self.view.name()),
            theme::mono(theme::F_SMALL),
            theme::TEXT,
        );
        p.text(
            egui::pos2(rect.left() + 196.0, rect.center().y + 9.0),
            Align2::LEFT_CENTER,
            self.view.blurb(),
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );

        // Right: the three states worth knowing from any screen.
        let bots = self.sup.running(Kind::Bot);
        let chips: [(String, Color32); 3] = [
            (
                if self.fleet.live() {
                    format!("LIVE {:.0}/S", self.fleet.pps)
                } else if self.fleet.last_packet_at.is_some() {
                    format!("QUIET {:.0}S", self.fleet.quiet_for())
                } else {
                    "NO TELEMETRY".into()
                },
                if self.fleet.live() {
                    theme::OK
                } else {
                    theme::TEXT_FAINT
                },
            ),
            (
                format!("{bots} PROC"),
                if bots > 0 { theme::ACCENT_TEXT } else { theme::TEXT_FAINT },
            ),
            (
                format!("{} STUCK", self.fleet.stuck()),
                if self.fleet.stuck() > 0 {
                    theme::BAD
                } else {
                    theme::TEXT_FAINT
                },
            ),
        ];
        let mut x = rect.right() - 16.0;
        for (text, color) in chips.iter().rev() {
            let galley =
                p.layout_no_wrap(text.clone(), theme::mono(theme::F_TINY), *color);
            let w = galley.size().x + 16.0;
            let r = egui::Rect::from_min_size(
                egui::pos2(x - w, rect.center().y - 10.0),
                egui::vec2(w, 20.0),
            );
            p.rect_stroke(r, Rounding::ZERO, Stroke::new(1.0, color.gamma_multiply(0.5)));
            p.galley(r.center() - galley.size() * 0.5, galley, *color);
            x -= w + 8.0;
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let p = ui.painter();
        p.line_segment(
            [rect.left_top(), rect.right_top()],
            Stroke::new(1.0, theme::LINE_BRIGHT),
        );
        let left = match &self.toast {
            Some((text, color, at)) if at.elapsed() < Duration::from_secs(6) => {
                (format!("» {text}"), *color)
            }
            _ => (
                format!(
                    "{} bots · {} alive · T {} / CT {} · {} pkt",
                    self.fleet.bots.len(),
                    self.fleet.alive(),
                    self.fleet.team_count(1),
                    self.fleet.team_count(2),
                    self.fleet.packets
                ),
                theme::TEXT_DIM,
            ),
        };
        p.text(
            egui::pos2(rect.left() + 14.0, rect.center().y),
            Align2::LEFT_CENTER,
            left.0,
            theme::mono(theme::F_TINY),
            left.1,
        );
        let map = self.fleet.map_name().unwrap_or("no map");
        let errors = self.sup.log.lock().map(|l| l.errors).unwrap_or(0);
        p.text(
            egui::pos2(rect.right() - 14.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format!(
                "{map} · {} · {errors} errors · 1-7 switches view",
                self.cfg.addr
            ),
            theme::mono(theme::F_TINY),
            theme::TEXT_FAINT,
        );
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let keys = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
        ];
        for (i, key) in keys.iter().enumerate() {
            if ctx.input(|input| input.key_pressed(*key)) {
                self.view = View::ALL[i];
            }
        }
        // Tab cycles the selection. Clicking a 7-pixel square on a radar is
        // fine with a mouse and impossible from a script, and inspecting one
        // bot after another is the most common thing to want here.
        if ctx.input(|i| i.key_pressed(egui::Key::Tab)) {
            let names = self.fleet.names();
            if !names.is_empty() {
                let next = match &self.selected {
                    Some(cur) => names
                        .iter()
                        .position(|n| n == cur)
                        .map(|i| (i + 1) % names.len())
                        .unwrap_or(0),
                    None => 0,
                };
                self.selected = Some(names[next].clone());
            }
        }
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let maps = self.cfg.maps_dir();
        self.fleet
            .drain(&self.feed, 512, Some(maps.as_path()));
        self.sup.poll();
        self.poll_probe();
        self.shortcuts(ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            self.closing = true;
            self.sup.kill_everything();
            let _ = self.cfg.save();
        }

        egui::TopBottomPanel::top("topbar")
            .exact_height(theme::TOPBAR_H)
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .stroke(Stroke::NONE)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| {
                self.top_bar(ui);
                ui.painter().line_segment(
                    [
                        ui.max_rect().left_bottom(),
                        ui.max_rect().right_bottom(),
                    ],
                    Stroke::new(1.0, theme::LINE_BRIGHT),
                );
            });

        egui::TopBottomPanel::bottom("status")
            .exact_height(theme::STATUS_H)
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| self.status_bar(ui));

        egui::SidePanel::left("rail")
            .exact_width(theme::RAIL_W)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| {
                self.nav_rail(ui);
                ui.painter().line_segment(
                    [ui.max_rect().right_top(), ui.max_rect().right_bottom()],
                    Stroke::new(1.0, theme::LINE_BRIGHT),
                );
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(16.0)),
            )
            .show(ctx, |ui| {
                theme::lattice(
                    ui.painter(),
                    ui.max_rect().expand(16.0),
                    theme::GRID_STEP,
                );
                match self.view {
                    View::Dashboard => views::dashboard::ui(self, ui),
                    View::Radar => views::radar::ui(self, ui),
                    View::Fleet => views::fleet::ui(self, ui),
                    View::Deploy => views::deploy::ui(self, ui),
                    View::Server => views::server::ui(self, ui),
                    View::Console => views::console::ui(self, ui),
                    View::Settings => views::settings::ui(self, ui),
                }
            });

        // 10 Hz is enough for 2 Hz telemetry and keeps an idle window cheap.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn main() -> eframe::Result<()> {
    std::panic::set_hook(Box::new(|info| log_line(&format!("PANIC: {info}"))));
    let cfg = Config::load();
    log_line(&format!(
        "ruBots Control starting - root {} - telemetry {}",
        cfg.root, cfg.telemetry_port
    ));

    let feed = Feed::bind(
        cfg.telemetry_port as u16,
        (cfg.team_port > 0).then_some(cfg.team_port as u16),
    );
    if let Some(err) = &feed.error {
        log_line(&format!("telemetry bind failed: {err}"));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([1040.0, 640.0])
            .with_title("ruBots Control"),
        ..Default::default()
    };
    let result = eframe::run_native(
        "ruBots Control",
        options,
        Box::new(move |cc| {
            theme::install(&cc.egui_ctx);
            Box::new(App::new(cfg, feed)) as Box<dyn eframe::App>
        }),
    );
    log_line("ruBots Control exited");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_has_a_shortcut_and_a_blurb() {
        assert_eq!(View::ALL.len(), 7, "the rail shows 1-7");
        for v in View::ALL {
            assert!(!v.name().is_empty());
            assert!(!v.blurb().is_empty());
        }
    }

    #[test]
    fn durations_read_as_a_human_would_say_them() {
        assert_eq!(fmt_dur(Duration::from_secs(9)), "9s");
        assert_eq!(fmt_dur(Duration::from_secs(75)), "1m15s");
        assert_eq!(fmt_dur(Duration::from_secs(3725)), "1h02m");
    }
}
