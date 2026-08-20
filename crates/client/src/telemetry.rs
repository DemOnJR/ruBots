//! The debug-radar telemetry packet (plan `debug-gui-radar.md`).
//!
//! Every bot broadcasts a fixed-layout UDP packet every 0.5 s to
//! `127.0.0.1:<port>` (default 27016). The GUI (`crates/gui`) listens and
//! aggregates the last-known state per bot name, so 30 separate OS processes
//! can be shown on one radar without any shared file or IPC library.
//!
//! Fixed layout on purpose: the crates here hand-roll binary formats for a
//! living protocol already, and a fixed struct means no serde dependency and
//! no allocation in the hot path. The packet is versioned with a magic so a
//! GUI and a bot build that disagree fail loudly instead of silently.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

/// Broadcast magic, so a stray datagram on the port is not misread.
pub const MAGIC: [u8; 4] = *b"APT1";
/// Default port the GUI listens on and the bots broadcast to.
pub const DEFAULT_PORT: u16 = 27016;
/// Default port for G0 team-state reports.
pub const TEAM_DEFAULT_PORT: u16 = 27017;
/// Total radar packet size, in bytes.
pub const PACKET_LEN: usize = 128;
/// Team-state packet magic.
pub const TEAM_MAGIC: [u8; 4] = *b"APT2";
/// Total team-state packet size, in bytes.
pub const TEAM_PACKET_LEN: usize = 80;

/// A compact same-team report for the G0 state bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeamTelemetry {
    pub bot_id: u16,
    pub team: u8,
    pub alive: bool,
    pub origin: [f32; 3],
    pub assigned_site: u8,
    pub contact_site: u8,
    pub contact_at: f32,
    pub bomb_carrier: bool,
    pub bomb_planted: bool,
    pub bomb_origin: Option<[f32; 3]>,
    pub observed_at: f32,
    pub role: u8,
    pub rung: u8,
}

impl TeamTelemetry {
    pub fn encode(&self) -> [u8; TEAM_PACKET_LEN] {
        let mut b = [0u8; TEAM_PACKET_LEN];
        b[0..4].copy_from_slice(&TEAM_MAGIC);
        b[4..6].copy_from_slice(&self.bot_id.to_le_bytes());
        b[6] = self.team;
        b[7] = self.alive as u8;
        for (i, value) in self.origin.into_iter().enumerate() {
            let at = 8 + i * 4;
            b[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        b[20] = self.assigned_site;
        b[21] = self.contact_site;
        b[22..26].copy_from_slice(&self.contact_at.to_le_bytes());
        b[26] = self.bomb_carrier as u8;
        b[27] = self.bomb_planted as u8;
        b[28] = self.bomb_origin.is_some() as u8;
        if let Some(origin) = self.bomb_origin {
            for (i, value) in origin.into_iter().enumerate() {
                let at = 29 + i * 4;
                b[at..at + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        b[41..45].copy_from_slice(&self.observed_at.to_le_bytes());
        b[45] = self.role;
        b[46] = self.rung;
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < TEAM_PACKET_LEN || b[0..4] != TEAM_MAGIC {
            return None;
        }
        let origin = [
            f32::from_le_bytes(b[8..12].try_into().ok()?),
            f32::from_le_bytes(b[12..16].try_into().ok()?),
            f32::from_le_bytes(b[16..20].try_into().ok()?),
        ];
        let bomb_origin = if b[28] != 0 {
            Some([
                f32::from_le_bytes(b[29..33].try_into().ok()?),
                f32::from_le_bytes(b[33..37].try_into().ok()?),
                f32::from_le_bytes(b[37..41].try_into().ok()?),
            ])
        } else {
            None
        };
        Some(Self {
            bot_id: u16::from_le_bytes(b[4..6].try_into().ok()?),
            team: b[6],
            alive: b[7] != 0,
            origin,
            assigned_site: b[20],
            contact_site: b[21],
            contact_at: f32::from_le_bytes(b[22..26].try_into().ok()?),
            bomb_carrier: b[26] != 0,
            bomb_planted: b[27] != 0,
            bomb_origin,
            observed_at: f32::from_le_bytes(b[41..45].try_into().ok()?),
            role: b[45],
            rung: b[46],
        })
    }
}

/// Encode an optional site as 0=unknown, 1=A, 2=B.
pub fn site_code(site: Option<bot::PlantSite>) -> u8 {
    match site {
        None | Some(bot::PlantSite::Unknown) => 0,
        Some(bot::PlantSite::A) => 1,
        Some(bot::PlantSite::B) => 2,
    }
}

/// Decode an optional site as 0=unknown, 1=A, 2=B.
pub fn decode_site(code: u8) -> Option<bot::PlantSite> {
    match code {
        1 => Some(bot::PlantSite::A),
        2 => Some(bot::PlantSite::B),
        _ => None,
    }
}

const TEAM_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 1);

/// Optional localhost bus for G0 same-team reports.
pub struct TeamBus {
    socket: UdpSocket,
    destination: SocketAddr,
    last_send: Instant,
}

impl TeamBus {
    /// Bind a reusable localhost receiver and broadcast reports to the same port.
    pub fn from_env() -> io::Result<Option<Self>> {
        let Some(port) = std::env::var("RUB_TEAM_PORT")
            .or_else(|_| std::env::var("RUBOTS_TEAM_PORT"))
            .or_else(|_| std::env::var("REB_TEAM_PORT"))
            .or_else(|_| std::env::var("REBOTS_TEAM_PORT"))
            .or_else(|_| std::env::var("AIPLAYERS_TEAM_PORT"))
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
        else {
            return Ok(None);
        };
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
        socket.set_nonblocking(true)?;
        let socket: UdpSocket = socket.into();
        socket.join_multicast_v4(&TEAM_MULTICAST, &Ipv4Addr::LOCALHOST)?;
        let destination = SocketAddr::from((TEAM_MULTICAST, port));
        Ok(Some(Self {
            socket,
            destination,
            last_send: Instant::now() - Duration::from_secs(1),
        }))
    }

    pub fn publish(&mut self, report: TeamTelemetry) {
        if self.last_send.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_send = Instant::now();
        let _ = self.socket.send_to(&report.encode(), self.destination);
    }

    pub fn poll(&self, reports: &mut Vec<TeamTelemetry>) {
        let mut packet = [0u8; TEAM_PACKET_LEN];
        loop {
            match self.socket.recv_from(&mut packet) {
                Ok((len, _)) => {
                    if let Some(report) = TeamTelemetry::decode(&packet[..len]) {
                        reports.push(report);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
}

/// Everything the radar needs to draw one bot and flag it stuck.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BotTelemetry {
    /// `Bot01` .. `Bot30`, NUL-padded.
    pub name: [u8; 16],
    /// `de_dust2`, NUL-padded.
    pub map: [u8; 32],
    pub origin: [f32; 3],
    pub yaw: f32,
    /// 1 = T, 2 = CT, 0 = spectator.
    pub team: u8,
    pub alive: bool,
    /// `goto`, `camp`, `combat`, `roam`, `freeze`, `dead`, ... NUL-padded.
    pub rung: [u8; 16],
    /// Server-measured horizontal speed, u/s.
    pub vel: f32,
    /// What the brain requested this tick (the stuck tell: `vel < 1` while
    /// `fwd != 0` for a few seconds = grinding).
    pub fwd: f32,
    pub side: f32,
    pub waypoints_left: u16,
    /// Nav node being steered at, or -1.
    pub node: i32,
    pub stuck: bool,
    pub to_goal: f32,
    /// Wall-clock seconds since the bot started (for the replay scrubber).
    pub t: f32,
}

impl BotTelemetry {
    pub fn encode(&self) -> [u8; PACKET_LEN] {
        let mut b = [0u8; PACKET_LEN];
        b[0..4].copy_from_slice(&MAGIC);
        let mut at = 4;
        b[at..at + 16].copy_from_slice(&self.name);
        at += 16;
        b[at..at + 32].copy_from_slice(&self.map);
        at += 32;
        for v in self.origin {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
            at += 4;
        }
        b[at..at + 4].copy_from_slice(&self.yaw.to_le_bytes());
        at += 4;
        b[at] = self.team;
        at += 1;
        b[at] = self.alive as u8;
        at += 1;
        b[at..at + 16].copy_from_slice(&self.rung);
        at += 16;
        b[at..at + 4].copy_from_slice(&self.vel.to_le_bytes());
        at += 4;
        b[at..at + 4].copy_from_slice(&self.fwd.to_le_bytes());
        at += 4;
        b[at..at + 4].copy_from_slice(&self.side.to_le_bytes());
        at += 4;
        b[at..at + 2].copy_from_slice(&self.waypoints_left.to_le_bytes());
        at += 2;
        b[at..at + 4].copy_from_slice(&self.node.to_le_bytes());
        at += 4;
        b[at] = self.stuck as u8;
        at += 1;
        b[at..at + 4].copy_from_slice(&self.to_goal.to_le_bytes());
        at += 4;
        b[at..at + 4].copy_from_slice(&self.t.to_le_bytes());
        at += 4;
        debug_assert!(at <= PACKET_LEN, "telemetry packet overflowed at {at}");
        b
    }

    /// Decode a datagram; `None` when the magic does not match.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < PACKET_LEN || b[0..4] != MAGIC {
            return None;
        }
        let mut at = 4;
        let mut name = [0u8; 16];
        name.copy_from_slice(&b[at..at + 16]);
        at += 16;
        let mut map = [0u8; 32];
        map.copy_from_slice(&b[at..at + 32]);
        at += 32;
        let mut origin = [0.0f32; 3];
        for v in &mut origin {
            *v = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
            at += 4;
        }
        let yaw = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        at += 4;
        let team = b[at];
        at += 1;
        let alive = b[at] != 0;
        at += 1;
        let mut rung = [0u8; 16];
        rung.copy_from_slice(&b[at..at + 16]);
        at += 16;
        let vel = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        at += 4;
        let fwd = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        at += 4;
        let side = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        at += 4;
        let waypoints_left = u16::from_le_bytes(b[at..at + 2].try_into().ok()?);
        at += 2;
        let node = i32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        at += 4;
        let stuck = b[at] != 0;
        at += 1;
        let to_goal = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        at += 4;
        let t = f32::from_le_bytes(b[at..at + 4].try_into().ok()?);
        Some(Self {
            name,
            map,
            origin,
            yaw,
            team,
            alive,
            rung,
            vel,
            fwd,
            side,
            waypoints_left,
            node,
            stuck,
            to_goal,
            t,
        })
    }
}

/// A fixed byte slice into a NUL-padded field, trimmed at the first NUL.
pub fn field_str(f: &[u8]) -> String {
    let end = f.iter().position(|&c| c == 0).unwrap_or(f.len());
    String::from_utf8_lossy(&f[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_packet_round_trips_with_bomb_origin() {
        let p = TeamTelemetry {
            bot_id: 7,
            team: 2,
            alive: true,
            origin: [-1536.0, 2688.0, 48.0],
            assigned_site: 2,
            contact_site: 1,
            contact_at: 14.5,
            bomb_carrier: false,
            bomb_planted: true,
            bomb_origin: Some([1040.0, 980.0, 100.0]),
            observed_at: 22.0,
            role: 3,
            rung: 4,
        };
        assert_eq!(TeamTelemetry::decode(&p.encode()), Some(p));
    }

    #[test]
    fn team_packet_rejects_foreign_datagrams() {
        assert!(TeamTelemetry::decode(&[0u8; TEAM_PACKET_LEN]).is_none());
        assert!(TeamTelemetry::decode(&[0u8; 8]).is_none());
    }

    #[test]
    fn the_packet_round_trips() {
        fn pad(s: &str, n: usize) -> [u8; 16] {
            let mut a = [0u8; 16];
            let b = s.as_bytes();
            a[..b.len().min(n)].copy_from_slice(&b[..b.len().min(n)]);
            a
        }
        let mut map = [0u8; 32];
        map[..8].copy_from_slice(b"de_dust2");
        let p = BotTelemetry {
            name: pad("Bot07", 16),
            map,
            origin: [-1536.0, 2688.0, 48.0],
            yaw: -37.5,
            team: 1,
            alive: true,
            rung: pad("goto", 16),
            vel: 0.0,
            fwd: 237.0,
            side: -12.0,
            waypoints_left: 42,
            node: 299,
            stuck: false,
            to_goal: 812.0,
            t: 120.5,
        };
        let enc = p.encode();
        let dec = BotTelemetry::decode(&enc).expect("decode");
        assert_eq!(dec.name, p.name);
        assert_eq!(dec.map, p.map);
        assert_eq!(dec.origin, p.origin);
        assert_eq!(dec.yaw, p.yaw);
        assert_eq!(dec.team, p.team);
        assert_eq!(dec.alive, p.alive);
        assert_eq!(dec.rung, p.rung);
        assert_eq!(dec.vel, p.vel);
        assert_eq!(dec.fwd, p.fwd);
        assert_eq!(dec.side, p.side);
        assert_eq!(dec.waypoints_left, p.waypoints_left);
        assert_eq!(dec.node, p.node);
        assert_eq!(dec.stuck, p.stuck);
        assert_eq!(dec.to_goal, p.to_goal);
        assert_eq!(dec.t, p.t);
    }

    #[test]
    fn a_foreign_datagram_is_rejected() {
        assert!(BotTelemetry::decode(&[0u8; PACKET_LEN]).is_none());
        assert!(BotTelemetry::decode(&[0u8; 8]).is_none());
    }

    #[test]
    fn field_str_trims_at_the_first_nul() {
        assert_eq!(field_str(b"Bot01\x00\x00\x00"), "Bot01");
        assert_eq!(field_str(b"de_dust2\x00\x00"), "de_dust2");
        assert_eq!(field_str(b"full"), "full");
    }
}
