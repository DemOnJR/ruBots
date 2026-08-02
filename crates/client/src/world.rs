//! Decoding our own player state out of the running-phase datagram.
//!
//! `svc_clientdata` is the server telling us, authoritatively, where we are:
//! `origin`, `velocity`, `health`, `flags`, `weapons`, `maxspeed`, `punchangle`
//! and the rest of `clientdata_t` (`testserver/rehlds/cstrike/delta.lst:6-66`).
//! It is the cheapest possible proof that our movement commands are being
//! *applied* rather than silently discarded, and it is the first piece of the
//! full entity layer.
//!
//! ## Where it sits in the datagram
//!
//! `SV_SendClientDatagram` (`rehlds/engine/sv_main.cpp:5000-5063`) writes, in
//! order: `svc_time` + float, then `SV_WriteClientdataToMessage`, then the
//! entity block. `SV_WriteClientdataToMessage` (`sv_main.cpp:1262-1378`) may
//! emit up to three byte-aligned messages of its own first:
//!
//! ```text
//! [svc_choke    (42)]                        // no payload
//! [svc_setangle (10)] short pitch, yaw, roll // or
//! [svc_addangle (38)] short delta_yaw
//!  svc_clientdata (15) <bit block>
//! ```
//!
//! ## The bit block
//!
//! ```text
//! 1 bit  has_delta
//! if has_delta { 8 bits delta_sequence }     // from = frames[seq].clientdata
//! clientdata_t delta
//! loop { 1 bit more; if !more break;
//!        6 bits slot; weapon_data_t delta }  // only when userinfo has cl_lw 1
//! ```
//!
//! While we send no `clc_delta`, `delta_sequence` is `-1` server-side, so
//! `has_delta` is 0 and the delta is against a **zeroed** struct — the simplest
//! case, and a complete snapshot every frame.

use proto::bitbuf::BitReader;
use proto::delta::{DeltaRegistry, Value};
use std::collections::HashMap;

use crate::svc;

/// Our own player state for one server frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientData {
    /// Server time this frame carries, from the preceding `svc_time`.
    pub time: f32,
    /// `clientdata_t`, keyed by the field names in `delta.lst`.
    pub fields: HashMap<String, Value>,
    /// `weapon_data_t` per weapon slot. Present only with `cl_lw 1`.
    pub weapons: HashMap<u8, HashMap<String, Value>>,
    /// From a `svc_setangle` in the same datagram: the server forcing our view,
    /// which it does on every spawn (`sv_main.cpp:1459-1462`).
    pub forced_angles: Option<[f32; 3]>,
}

impl ClientData {
    pub fn f32(&self, key: &str) -> Option<f32> {
        self.fields.get(key).and_then(Value::as_f32)
    }

    pub fn origin(&self) -> [f32; 3] {
        [
            self.f32("origin[0]").unwrap_or(0.0),
            self.f32("origin[1]").unwrap_or(0.0),
            self.f32("origin[2]").unwrap_or(0.0),
        ]
    }

    pub fn velocity(&self) -> [f32; 3] {
        [
            self.f32("velocity[0]").unwrap_or(0.0),
            self.f32("velocity[1]").unwrap_or(0.0),
            self.f32("velocity[2]").unwrap_or(0.0),
        ]
    }

    pub fn speed(&self) -> f32 {
        let v = self.velocity();
        (v[0] * v[0] + v[1] * v[1]).sqrt()
    }

    pub fn health(&self) -> f32 {
        self.f32("health").unwrap_or(0.0)
    }

    pub fn maxspeed(&self) -> f32 {
        self.f32("maxspeed").unwrap_or(0.0)
    }

    /// Recoil, which the engine adds to our view angles before firing. See
    /// `PM_CheckParameters` (`pm_shared.cpp:3035-3047`).
    pub fn punchangle(&self) -> [f32; 3] {
        [
            self.f32("punchangle[0]").unwrap_or(0.0),
            self.f32("punchangle[1]").unwrap_or(0.0),
            self.f32("punchangle[2]").unwrap_or(0.0),
        ]
    }

    /// Bitmask of carried weapons, `1 << WeaponIdType`.
    ///
    /// The cheapest reliable "am I actually in the game?" test available before
    /// the user-message layer exists: a player who has completed the join gets
    /// a knife (`mp_t_give_player_knife`), so this is non-zero. While
    /// `JoiningThink` is still cycling the intro camera it is zero.
    pub fn weapons(&self) -> u32 {
        self.fields
            .get("weapons")
            .and_then(Value::as_i64)
            .unwrap_or(0) as u32
    }

    /// Have we finished joining and spawned as a live player?
    ///
    /// `maxspeed` is the discriminator, and it is a sharp one.
    /// `CBasePlayer::ResetMaxSpeed` (`player.cpp:8074-8105`) gives **1.0** to a
    /// player in the freeze period or waiting to join, and 210-250 to one who
    /// is actually playing (240 with no weapon deployed, less with a heavy
    /// one). There is nothing in between.
    ///
    /// Nothing else here works. Health is 100 and `deadflag` is `DEAD_NO` while
    /// the join camera cycles spawn points, so both say "alive". `weapons` is
    /// zero on a fresh spawn too, before anything is deployed. Only `maxspeed`
    /// separates "in the world" from "watching it".
    pub fn in_game(&self) -> bool {
        self.alive() && self.maxspeed() > 1.5
    }

    /// `iuser3` carries the per-frame player flags ReGameDLL packs in
    /// `client.cpp:5108-5121`; bit 2 is `PLAYER_IN_BOMB_ZONE`
    /// (`cdll_dll.h:69-77`).
    pub fn iuser3(&self) -> i64 {
        self.fields
            .get("iuser3")
            .and_then(Value::as_i64)
            .unwrap_or(0)
    }

    pub fn in_bomb_zone(&self) -> bool {
        self.iuser3() & (1 << 2) != 0
    }

    /// `deadflag == DEAD_NO`.
    ///
    /// **Absent means alive.** The delta is written against a zeroed struct,
    /// and `DELTA_MarkSendFields` only marks fields that *differ* from it, so a
    /// `deadflag` of `DEAD_NO` (0) is never transmitted. Defaulting a missing
    /// field to "dead" reports every healthy player as a corpse -- which is
    /// exactly the wrong conclusion to draw while debugging whether the bot
    /// spawned. Every accessor here has the same shape: absent == zero.
    pub fn alive(&self) -> bool {
        self.fields
            .get("deadflag")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            == 0
    }
}

/// Parse one running-phase datagram far enough to recover `svc_clientdata`.
///
/// Returns `None` when the message is not a server datagram (it does not begin
/// with `svc_time`) or when the layout does not hold — never a partial guess.
pub fn parse_datagram(msg: &[u8], registry: &DeltaRegistry) -> Option<ClientData> {
    if msg.first() != Some(&svc::SVC_TIME) || msg.len() < 5 {
        return None;
    }
    let mut out = ClientData {
        time: f32::from_le_bytes(msg[1..5].try_into().ok()?),
        ..Default::default()
    };
    let mut at = 5usize;

    // The optional byte-aligned preamble, in the order the server writes it.
    loop {
        match msg.get(at) {
            Some(&svc::SVC_CHOKE) => at += 1,
            Some(&svc::SVC_SETANGLE) => {
                if msg.len() < at + 7 {
                    return None;
                }
                // MSG_WriteHiresAngle: deg = short * 360 / 65536.
                let a = |o: usize| {
                    f32::from(i16::from_le_bytes([msg[o], msg[o + 1]])) * 360.0 / 65536.0
                };
                out.forced_angles = Some([a(at + 1), a(at + 3), a(at + 5)]);
                at += 7;
            }
            Some(&svc::SVC_ADDANGLE) => at += 3,
            _ => break,
        }
    }

    if msg.get(at) != Some(&svc::SVC_CLIENTDATA) {
        return None;
    }
    at += 1;

    let cd = registry.get("clientdata_t")?;
    let mut r = BitReader::new(&msg[at..]);

    // `has_delta` plus, when set, the frame we are being delta'd against.
    // We never advertise a frame, so this is 0 and `from` is a zeroed struct;
    // if that ever changes we cannot reconstruct the base and must bail rather
    // than return numbers that look plausible and are not.
    if r.read_bits(1) != 0 {
        let _seq = r.read_bits(8);
        return None;
    }
    out.fields = proto::delta::parse_delta(&mut r, cd);

    // The weapon loop exists because our userinfo carries `cl_lw 1`
    // (`sv_main.cpp:1362`). The terminating zero bit is written either way.
    if let Some(wd) = registry.get("weapon_data_t") {
        let mut guard = 0;
        while r.read_bits(1) != 0 {
            let slot = r.read_bits(6) as u8;
            let f = proto::delta::parse_delta(&mut r, wd);
            out.weapons.insert(slot, f);
            guard += 1;
            if guard > 64 || r.overflowed() {
                break;
            }
        }
    }
    if r.overflowed() {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::bitbuf::BitWriter;
    use proto::delta::{write_delta, FieldDesc};

    const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");

    fn registry() -> DeltaRegistry {
        crate::signon::walk(SIGNON).registry
    }

    /// Build a datagram the way `SV_SendClientDatagram` does, so the parser is
    /// tested against the real `clientdata_t` table rather than a toy one.
    fn datagram(
        time: f32,
        fields: &HashMap<String, Value>,
        table: &[FieldDesc],
        preamble: &[u8],
    ) -> Vec<u8> {
        let mut out = vec![svc::SVC_TIME];
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(preamble);
        out.push(svc::SVC_CLIENTDATA);

        let mut w = BitWriter::new();
        w.write_bits(0, 1); // has_delta = 0
        write_delta(&mut w, table, fields);
        w.write_bits(0, 1); // no weapon entries
        out.extend_from_slice(&w.into_bytes());
        out
    }

    fn sample() -> HashMap<String, Value> {
        let mut f = HashMap::new();
        f.insert("origin[0]".into(), Value::Float(1234.5));
        f.insert("origin[1]".into(), Value::Float(-678.25));
        f.insert("origin[2]".into(), Value::Float(36.0));
        f.insert("velocity[0]".into(), Value::Float(250.0));
        f.insert("health".into(), Value::Float(87.0));
        f.insert("maxspeed".into(), Value::Float(250.0));
        f
    }

    #[test]
    fn a_clientdata_datagram_round_trips() {
        let reg = registry();
        let table = reg.get("clientdata_t").expect("clientdata_t").clone();
        let msg = datagram(12.5, &sample(), &table, &[]);

        let cd = parse_datagram(&msg, &reg).expect("parses");
        assert!((cd.time - 12.5).abs() < 1e-3);
        let o = cd.origin();
        assert!((o[0] - 1234.5).abs() < 0.1, "origin[0] = {}", o[0]);
        assert!((o[1] + 678.25).abs() < 0.1, "origin[1] = {}", o[1]);
        assert!((cd.health() - 87.0).abs() < 0.5);
        assert!((cd.maxspeed() - 250.0).abs() < 0.5);
        assert!((cd.speed() - 250.0).abs() < 1.0);
    }

    #[test]
    fn the_optional_preamble_messages_are_skipped() {
        let reg = registry();
        let table = reg.get("clientdata_t").expect("clientdata_t").clone();

        // svc_choke, then svc_setangle with pitch 0 / yaw 90 / roll 0.
        let yaw = ((90.0f32 / 360.0) * 65536.0) as i16;
        let mut pre = vec![svc::SVC_CHOKE, svc::SVC_SETANGLE];
        pre.extend_from_slice(&0i16.to_le_bytes());
        pre.extend_from_slice(&yaw.to_le_bytes());
        pre.extend_from_slice(&0i16.to_le_bytes());

        let msg = datagram(1.0, &sample(), &table, &pre);
        let cd = parse_datagram(&msg, &reg).expect("parses past the preamble");
        let a = cd.forced_angles.expect("setangle captured");
        assert!((a[1] - 90.0).abs() < 0.1, "yaw {} should be ~90", a[1]);
        assert!((cd.origin()[0] - 1234.5).abs() < 0.1);
    }

    #[test]
    fn a_non_datagram_message_is_rejected_rather_than_guessed_at() {
        let reg = registry();
        assert!(parse_datagram(&[], &reg).is_none());
        assert!(parse_datagram(&[svc::SVC_PRINT, b'h', b'i', 0], &reg).is_none());
        // svc_time but no clientdata behind it.
        let mut m = vec![svc::SVC_TIME];
        m.extend_from_slice(&1.0f32.to_le_bytes());
        m.push(svc::SVC_PRINT);
        assert!(parse_datagram(&m, &reg).is_none());
    }

    #[test]
    fn a_delta_compressed_clientdata_is_refused_not_misread() {
        let reg = registry();
        let mut out = vec![svc::SVC_TIME];
        out.extend_from_slice(&1.0f32.to_le_bytes());
        out.push(svc::SVC_CLIENTDATA);
        let mut w = BitWriter::new();
        w.write_bits(1, 1); // has_delta -- we cannot reconstruct the base
        w.write_bits(7, 8);
        out.extend_from_slice(&w.into_bytes());
        assert!(parse_datagram(&out, &reg).is_none());
    }

    #[test]
    fn flag_helpers_read_the_right_bits() {
        let mut cd = ClientData::default();
        cd.fields.insert("iuser3".into(), Value::Int(1 << 2));
        assert!(cd.in_bomb_zone());
        cd.fields.insert("iuser3".into(), Value::Int(1 << 1));
        assert!(!cd.in_bomb_zone());

        cd.fields.insert("deadflag".into(), Value::Int(0));
        assert!(cd.alive());
        cd.fields.insert("deadflag".into(), Value::Int(2));
        assert!(!cd.alive());
    }

    /// Deltas carry only fields that differ from the base, so a live player's
    /// `deadflag` (DEAD_NO == 0) never appears on the wire at all. Reading
    /// "absent" as "dead" made a perfectly healthy bot look like a corpse.
    #[test]
    fn an_absent_deadflag_means_alive_not_dead() {
        let cd = ClientData::default();
        assert!(cd.alive(), "a field the server omitted is zero, not unknown");
    }

    /// Values taken from live traces of both states. The join camera is the
    /// deceptive one: full health, DEAD_NO, and an origin that moves -- by
    /// teleporting between spawn points. Only maxspeed tells them apart.
    #[test]
    fn maxspeed_separates_a_spawned_player_from_the_join_camera() {
        let mut cd = ClientData::default();
        cd.fields.insert("health".into(), Value::Float(100.0));

        cd.fields.insert("maxspeed".into(), Value::Float(1.0));
        assert!(!cd.in_game(), "maxspeed 1 is the join camera / freeze period");

        cd.fields.insert("maxspeed".into(), Value::Float(240.0));
        assert!(cd.in_game(), "maxspeed 240 is a spawned player");

        // Heavy weapons drop it, but never near 1.
        cd.fields.insert("maxspeed".into(), Value::Float(210.0));
        assert!(cd.in_game(), "an AWP carrier is still in the game");

        // Dead outranks everything.
        cd.fields.insert("deadflag".into(), Value::Int(2));
        assert!(!cd.in_game());
    }
}
