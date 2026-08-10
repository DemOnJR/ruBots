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

/// Broadcast magic, so a stray datagram on the port is not misread.
pub const MAGIC: [u8; 4] = *b"APT1";
/// Default port the GUI listens on and the bots broadcast to.
pub const DEFAULT_PORT: u16 = 27016;
/// Total packet size, in bytes.
pub const PACKET_LEN: usize = 128;

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
