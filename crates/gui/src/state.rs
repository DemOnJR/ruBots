//! Live state: what the telemetry buses say is happening right now.
//!
//! Two independent UDP feeds arrive here and are aggregated by bot:
//!
//! * **APT1** (`BotTelemetry`, port 27016) — one packet per bot every 0.5 s
//!   with position, rung, velocity and the stuck tell. This is what the radar
//!   draws.
//! * **APT2** (`TeamTelemetry`, port 27017) — the G0 team bus the bots use to
//!   share contacts and bomb state. The shell only listens; it never
//!   publishes, so joining the bus cannot change what the swarm does.
//!
//! Nothing here blocks: both feeds are read on their own threads into
//! channels, and the UI drains a bounded number of packets per frame so a
//! backlog from thirty bots can never stall the paint.

use std::collections::{HashMap, VecDeque};
use std::net::UdpSocket;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use client::telemetry::{field_str, BotTelemetry, TeamTelemetry, PACKET_LEN, TEAM_PACKET_LEN};

use crate::radar::{Projection, RadarBackground};

/// Seconds of `vel < 1` while movement is requested before a bot counts stuck.
pub const STUCK_AFTER: f32 = 3.0;
/// A bot silent this long is from a previous swarm; drop it.
const ZOMBIE_AFTER: Duration = Duration::from_secs(30);
/// History kept per bot for the replay scrubber: 240 samples at 0.5 Hz.
const HISTORY: usize = 240;

#[derive(Clone)]
pub struct BotState {
    pub t: BotTelemetry,
    /// Consecutive seconds requesting movement while the server reports still.
    pub stuck_for: f32,
    /// `(t, x, y)` samples for the trail and the scrubber.
    pub history: VecDeque<(f32, f32, f32)>,
    pub first_seen: Instant,
    pub last_seen: Instant,
    pub packets: u64,
    /// Last G0 report from this bot, when the team bus is being listened to.
    pub team: Option<TeamTelemetry>,
}

impl BotState {
    pub fn stuck(&self) -> bool {
        self.stuck_for > STUCK_AFTER
    }

    pub fn rung(&self) -> String {
        field_str(&self.t.rung)
    }

    /// Position at a past moment, for the replay scrubber.
    pub fn at(&self, when: f32) -> (f32, f32) {
        for &(tt, x, y) in self.history.iter().rev() {
            if tt <= when {
                return (x, y);
            }
        }
        (self.t.origin[0], self.t.origin[1])
    }
}

/// Role codes as `Session::latest_team_report` encodes them.
pub fn role_name(code: u8) -> &'static str {
    match code {
        1 => "assault",
        2 => "hold",
        3 => "flank",
        4 => "split",
        _ => "-",
    }
}

/// Rung codes on the team bus (the radar packet carries the name in full).
pub fn rung_name(code: u8) -> &'static str {
    match code {
        1 => "goto",
        2 => "combat",
        3 => "defuse",
        4 => "plant",
        5 => "camp",
        _ => "-",
    }
}

pub fn site_name(code: u8) -> &'static str {
    match code {
        1 => "A",
        2 => "B",
        _ => "-",
    }
}

/// The aggregated view of the swarm.
pub struct Fleet {
    pub bots: HashMap<String, BotState>,
    /// Map name, silhouette and projection, loaded from the first packet.
    pub map: Option<(String, RadarBackground, Projection)>,
    /// The loaded map itself, kept so the shell can answer questions the
    /// telemetry packet does not carry -- what a bot at this spot ought to be
    /// watching, for one. Roughly the size of the .bsp, once.
    pub map_data: Option<client::map::Map>,
    /// A map the loader has already failed on, so it is not retried per packet.
    map_failed: Option<String>,
    pub packets: u64,
    pub team_packets: u64,
    pub last_packet_at: Option<Instant>,
    /// Packets per second over the last second, for the status bar.
    pub pps: f32,
    pps_window: VecDeque<Instant>,
}

impl Fleet {
    pub fn new() -> Self {
        Self {
            bots: HashMap::new(),
            map: None,
            map_data: None,
            map_failed: None,
            packets: 0,
            team_packets: 0,
            last_packet_at: None,
            pps: 0.0,
            pps_window: VecDeque::new(),
        }
    }

    pub fn map_name(&self) -> Option<&str> {
        self.map.as_ref().map(|(n, _, _)| n.as_str())
    }

    /// Seconds since the last radar packet, or a large number if never.
    pub fn quiet_for(&self) -> f32 {
        self.last_packet_at
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(f32::MAX)
    }

    pub fn live(&self) -> bool {
        self.quiet_for() < 2.0
    }

    pub fn alive(&self) -> usize {
        self.bots.values().filter(|b| b.t.alive).count()
    }

    pub fn stuck(&self) -> usize {
        self.bots.values().filter(|b| b.stuck()).count()
    }

    pub fn team_count(&self, team: u8) -> usize {
        self.bots
            .values()
            .filter(|b| b.t.team == team && b.t.alive)
            .count()
    }

    /// The newest bot clock seen, which is what "now" means for replay.
    pub fn now(&self) -> f32 {
        self.bots.values().map(|b| b.t.t).fold(0.0f32, f32::max)
    }

    /// Names, sorted, so every list in the app is in the same order.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.bots.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn clear(&mut self) {
        self.bots.clear();
        self.packets = 0;
        self.team_packets = 0;
    }

    /// Drain both feeds, up to `budget` radar packets, and retire zombies.
    pub fn drain(&mut self, feed: &Feed, budget: u32, maps_dir: Option<&std::path::Path>) {
        let now = Instant::now();
        let mut taken = 0;
        while taken < budget {
            match feed.radar.try_recv() {
                Ok(t) => {
                    taken += 1;
                    self.ingest(t, now, maps_dir);
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if let Some(team) = &feed.team {
            let mut taken = 0;
            while taken < budget {
                match team.try_recv() {
                    Ok(report) => {
                        taken += 1;
                        self.team_packets += 1;
                        self.attach_team(report);
                    }
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
        }

        while self
            .pps_window
            .front()
            .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1))
        {
            self.pps_window.pop_front();
        }
        self.pps = self.pps_window.len() as f32;

        self.bots
            .retain(|_, b| now.duration_since(b.last_seen) < ZOMBIE_AFTER);
    }

    fn ingest(&mut self, t: BotTelemetry, now: Instant, maps_dir: Option<&std::path::Path>) {
        self.packets += 1;
        self.last_packet_at = Some(now);
        self.pps_window.push_back(now);

        let name = field_str(&t.name);
        let entry = self.bots.entry(name).or_insert_with(|| BotState {
            t,
            stuck_for: 0.0,
            history: VecDeque::with_capacity(HISTORY),
            first_seen: now,
            last_seen: now,
            packets: 0,
            team: None,
        });
        // The stuck tell: the brain asks for movement, the server reports none.
        let requesting = t.fwd.abs() > 1.0 || t.side.abs() > 1.0;
        if t.alive && requesting && t.vel < 1.0 {
            entry.stuck_for += 0.5;
        } else {
            entry.stuck_for = 0.0;
        }
        entry.history.push_back((t.t, t.origin[0], t.origin[1]));
        while entry.history.len() > HISTORY {
            entry.history.pop_front();
        }
        entry.t = t;
        entry.last_seen = now;
        entry.packets += 1;

        self.load_map(&field_str(&t.map), maps_dir);
    }

    /// Load the radar background once, from whatever map the bots are on.
    fn load_map(&mut self, name: &str, maps_dir: Option<&std::path::Path>) {
        if self.map.is_some() || name.is_empty() {
            return;
        }
        if self.map_failed.as_deref() == Some(name) {
            return;
        }
        // `client::map::Map::load` reads RUB_MAPS_DIR; point it at the
        // configured tree so the GUI finds maps wherever it was started from.
        if let Some(dir) = maps_dir {
            // SAFETY: called from the UI thread before any reader thread of
            // ours reads the environment; the loader below is the only
            // consumer and it runs synchronously, right here.
            unsafe { std::env::set_var("RUB_MAPS_DIR", dir) };
        }
        match client::map::Map::load(name) {
            Some(map) => {
                let bg = RadarBackground::from_grid(&map.grid);
                let proj = Projection::from_grid(&map.grid);
                self.map = Some((name.to_string(), bg, proj));
                self.map_data = Some(map);
            }
            None => self.map_failed = Some(name.to_string()),
        }
    }

    /// Team-bus reports carry a numeric id, not a name; match on the digits in
    /// the bot's name the way `capture_running` derives the id.
    fn attach_team(&mut self, report: TeamTelemetry) {
        for (name, bot) in self.bots.iter_mut() {
            let digits: String = name.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.parse::<u16>().ok() == Some(report.bot_id) {
                bot.team = Some(report);
                return;
            }
        }
    }
}

/// The receiving end of both feeds.
pub struct Feed {
    pub radar: Receiver<BotTelemetry>,
    pub team: Option<Receiver<TeamTelemetry>>,
    /// What the shell is actually bound to, for the status bar.
    pub radar_port: u16,
    pub team_port: Option<u16>,
    /// Set when the radar port could not be bound — usually a second GUI.
    pub error: Option<String>,
}

impl Feed {
    /// Bind both ports and start the reader threads.
    ///
    /// A failure on the radar port is reported rather than fatal: the shell
    /// still has a server tab, a build button and a console, all of which work
    /// without telemetry.
    pub fn bind(radar_port: u16, team_port: Option<u16>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut error = None;
        match UdpSocket::bind(("0.0.0.0", radar_port)) {
            Ok(sock) => {
                let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
                std::thread::spawn(move || {
                    let mut buf = [0u8; PACKET_LEN + 64];
                    loop {
                        match sock.recv(&mut buf) {
                            Ok(n) => {
                                if let Some(t) = BotTelemetry::decode(&buf[..n]) {
                                    if tx.send(t).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) if would_block(&e) => continue,
                            Err(_) => std::thread::sleep(Duration::from_millis(50)),
                        }
                    }
                });
            }
            Err(e) => error = Some(format!("UDP {radar_port}: {e}")),
        }

        // The team bus is multicast and every bot binds it too, so this needs
        // the same SO_REUSEADDR dance `client::telemetry::TeamBus` does. It is
        // optional: without it the shell simply shows no role/site column.
        let team = team_port.and_then(|port| {
            // SAFETY: `bind` runs once, during startup, before any thread
            // that reads the environment has been spawned.
            unsafe { std::env::set_var("RUB_TEAM_PORT", port.to_string()) };
            let bus = client::telemetry::TeamBus::from_env().ok().flatten()?;
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut reports = Vec::new();
                loop {
                    bus.poll(&mut reports);
                    for report in reports.drain(..) {
                        if tx.send(report).is_err() {
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            Some(rx)
        });

        Self {
            radar: rx,
            team_port: team.is_some().then_some(team_port).flatten(),
            team,
            radar_port,
            error,
        }
    }
}

fn would_block(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Compile-time proof the two packet sizes are what the readers assume.
const _: () = {
    assert!(PACKET_LEN == 128);
    assert!(TEAM_PACKET_LEN == 80);
};

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(name: &str, vel: f32, fwd: f32, t: f32) -> BotTelemetry {
        let mut n = [0u8; 16];
        n[..name.len()].copy_from_slice(name.as_bytes());
        let mut map = [0u8; 32];
        map[..8].copy_from_slice(b"de_dust2");
        let mut rung = [0u8; 16];
        rung[..4].copy_from_slice(b"goto");
        BotTelemetry {
            name: n,
            map,
            origin: [0.0, 0.0, 0.0],
            yaw: 0.0,
            team: 1,
            alive: true,
            rung,
            vel,
            fwd,
            side: 0.0,
            waypoints_left: 3,
            node: 1,
            stuck: false,
            to_goal: 500.0,
            t,
        }
    }

    #[test]
    fn grinding_bots_accumulate_stuck_time_and_moving_ones_reset_it() {
        let mut fleet = Fleet::new();
        let now = Instant::now();
        for i in 0..8 {
            fleet.ingest(packet("ruBot01", 0.0, 250.0, i as f32 * 0.5), now, None);
        }
        assert!(fleet.bots["ruBot01"].stuck(), "8 still samples = 4 s > 3 s");
        fleet.ingest(packet("ruBot01", 220.0, 250.0, 4.0), now, None);
        assert!(!fleet.bots["ruBot01"].stuck(), "moving again clears it");
    }

    #[test]
    fn history_answers_where_a_bot_was() {
        let mut fleet = Fleet::new();
        let now = Instant::now();
        for i in 0..4 {
            let mut p = packet("ruBot02", 200.0, 250.0, i as f32);
            p.origin = [i as f32 * 100.0, 0.0, 0.0];
            fleet.ingest(p, now, None);
        }
        let bot = &fleet.bots["ruBot02"];
        assert_eq!(bot.at(2.0).0, 200.0);
        assert_eq!(bot.at(0.0).0, 0.0);
    }

    #[test]
    fn team_reports_bind_to_the_bot_whose_name_carries_that_id() {
        let mut fleet = Fleet::new();
        let now = Instant::now();
        fleet.ingest(packet("ruBot07", 100.0, 0.0, 0.0), now, None);
        fleet.ingest(packet("ruBot08", 100.0, 0.0, 0.0), now, None);
        fleet.attach_team(TeamTelemetry {
            bot_id: 8,
            team: 2,
            alive: true,
            origin: [1.0, 2.0, 3.0],
            assigned_site: 1,
            contact_site: 0,
            contact_at: -1.0,
            bomb_carrier: false,
            bomb_planted: false,
            bomb_origin: None,
            observed_at: 5.0,
            role: 3,
            rung: 2,
        });
        assert!(fleet.bots["ruBot07"].team.is_none());
        assert_eq!(fleet.bots["ruBot08"].team.map(|t| t.role), Some(3));
    }

    #[test]
    fn code_tables_match_the_session_encoding() {
        assert_eq!(role_name(1), "assault");
        assert_eq!(role_name(4), "split");
        assert_eq!(rung_name(2), "combat");
        assert_eq!(site_name(2), "B");
        assert_eq!(rung_name(9), "-");
    }
}
