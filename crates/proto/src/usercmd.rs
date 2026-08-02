//! `usercmd_t` — the per-tick input the client sends the server.
//!
//! Port of `internal/proto/usercmd.go`. Verified from `BuildClcMove`
//! (`0x1406E72A0`):
//!
//! * the command is built as a `map[string]Value` keyed by the field names
//!   below and delta-encoded against the registry's `usercmd_t` table
//!   (`mapaccess1_faststr` on `"usercmd_t"`, then `WriteDelta`)
//! * a check byte is computed over the encoded payload with
//!   `BlockSequenceCRCByte` (`call 0x1406E3AC0`) and kept separately
//! * the payload is then munged in place with table 1 and the same sequence
//!   (`lea rax, [rip+...]` → `0x1407B00C0`, `call 0x1406E6020`)
//!
//! The field names and their order are read directly from the run of
//! `mapassign_faststr` calls in `BuildClcMove`.

use crate::bitbuf::BitWriter;
use crate::crc::block_sequence_crc_byte;
use crate::delta::{write_delta, FieldDesc, Value};
use crate::munge;
use std::collections::HashMap;

/// The `usercmd_t` fields, in the order `BuildClcMove` assigns them.
pub const USERCMD_FIELDS: [&str; 17] = [
    "lerp_msec",
    "msec",
    "lightlevel",
    "viewangles[0]",
    "viewangles[1]",
    "viewangles[2]",
    "buttons",
    "forwardmove",
    "sidemove",
    "upmove",
    "impulse",
    "weaponselect",
    "impact_index",
    "impact_position[0]",
    "impact_position[1]",
    "impact_position[2]",
    "unused",
];

/// Button bits, as used by the bot layer (`fireButtons`, `wiggleButtons`).
pub mod buttons {
    pub const ATTACK: u32 = 1 << 0;
    pub const JUMP: u32 = 1 << 1;
    pub const DUCK: u32 = 1 << 2;
    pub const FORWARD: u32 = 1 << 3;
    pub const BACK: u32 = 1 << 4;
    pub const USE: u32 = 1 << 5;
    pub const CANCEL: u32 = 1 << 6;
    pub const LEFT: u32 = 1 << 7;
    pub const RIGHT: u32 = 1 << 8;
    pub const MOVELEFT: u32 = 1 << 9;
    pub const MOVERIGHT: u32 = 1 << 10;
    pub const ATTACK2: u32 = 1 << 11;
    pub const RUN: u32 = 1 << 12;
    pub const RELOAD: u32 = 1 << 13;
    pub const ALT1: u32 = 1 << 14;
    pub const SCORE: u32 = 1 << 15;
}

/// One tick of player input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UserCmd {
    pub lerp_msec: i16,
    pub msec: u8,
    pub lightlevel: u8,
    pub viewangles: [f32; 3],
    pub buttons: u16,
    pub forwardmove: f32,
    pub sidemove: f32,
    pub upmove: f32,
    pub impulse: u8,
    pub weaponselect: u8,
    pub impact_index: i32,
    pub impact_position: [f32; 3],
}

impl Default for UserCmd {
    fn default() -> Self {
        Self {
            lerp_msec: 0,
            msec: 0,
            lightlevel: 0,
            viewangles: [0.0; 3],
            buttons: 0,
            forwardmove: 0.0,
            sidemove: 0.0,
            upmove: 0.0,
            impulse: 0,
            weaponselect: 0,
            impact_index: 0,
            impact_position: [0.0; 3],
        }
    }
}

impl UserCmd {
    /// Flatten into the name-keyed map the delta encoder consumes.
    pub fn to_fields(self) -> HashMap<String, Value> {
        let mut m = HashMap::with_capacity(USERCMD_FIELDS.len());
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        put("lerp_msec", Value::Int(i64::from(self.lerp_msec)));
        put("msec", Value::Int(i64::from(self.msec)));
        put("lightlevel", Value::Int(i64::from(self.lightlevel)));
        put("viewangles[0]", Value::Float(self.viewangles[0]));
        put("viewangles[1]", Value::Float(self.viewangles[1]));
        put("viewangles[2]", Value::Float(self.viewangles[2]));
        put("buttons", Value::Int(i64::from(self.buttons)));
        put("forwardmove", Value::Float(self.forwardmove));
        put("sidemove", Value::Float(self.sidemove));
        put("upmove", Value::Float(self.upmove));
        put("impulse", Value::Int(i64::from(self.impulse)));
        put("weaponselect", Value::Int(i64::from(self.weaponselect)));
        put("impact_index", Value::Int(i64::from(self.impact_index)));
        put("impact_position[0]", Value::Float(self.impact_position[0]));
        put("impact_position[1]", Value::Float(self.impact_position[1]));
        put("impact_position[2]", Value::Float(self.impact_position[2]));
        m
    }

    /// True if this command asks to fire.
    pub fn is_attacking(&self) -> bool {
        u32::from(self.buttons) & buttons::ATTACK != 0
    }

    /// Encode this command as a delta against `baseline`, returning the bytes
    /// the engine writes for one `usercmd_t` (byte-aligned, since [`BitWriter`]
    /// pads to a whole byte).
    ///
    /// Only fields that differ from `baseline` are written, exactly as a real
    /// client deltas each command against the previous one (the first against a
    /// zeroed command — see [`build_move_payload`]). `table` is the server's
    /// runtime `usercmd_t` description, so widths and scales always match what
    /// the server will parse back.
    pub fn encode_delta(&self, baseline: &UserCmd, table: &[FieldDesc]) -> Vec<u8> {
        let new = self.to_fields();
        let old = baseline.to_fields();
        let mut changed = HashMap::new();
        for f in table {
            if let Some(nv) = new.get(&f.name) {
                let unchanged = old.get(&f.name).is_some_and(|ov| ov == nv);
                if !unchanged {
                    changed.insert(f.name.clone(), nv.clone());
                }
            }
        }
        let mut w = BitWriter::new();
        write_delta(&mut w, table, &changed);
        w.into_bytes()
    }
}

/// Assemble the clear (unmunged) `clc_move` payload for a run of commands.
///
/// Layout verified from ReHLDS `SV_ParseMove` (see [[aiplayers-rust-port]]):
///
/// ```text
/// u8 packet_loss | u8 numbackup | u8 numcmds | <cmd deltas, oldest first>
/// ```
///
/// Each command is delta-encoded against the previous one; the first is
/// deltaed against a zeroed command. Each delta is independently byte-aligned,
/// which holds automatically here because [`UserCmd::encode_delta`] returns
/// whole bytes. Pass the result to [`build_clc_move`], which adds the opcode,
/// length and sequence checksum and munges it.
///
/// `numbackup` is left at zero: a bot that sends a command every packet needs
/// no backup commands, and backup framing above 12 payload bytes is the one
/// piece not yet reproduced from a real client (see [`build_clc_move`]).
pub fn build_move_payload(packet_loss: u8, cmds: &[UserCmd], table: &[FieldDesc]) -> Vec<u8> {
    let mut out = vec![packet_loss, 0, cmds.len() as u8];
    let mut baseline = UserCmd::default();
    for cmd in cmds {
        out.extend_from_slice(&cmd.encode_delta(&baseline, table));
        baseline = *cmd;
    }
    out
}

/// A `clc_move` body: the check byte plus the munged delta payload.
///
/// The outer framing (opcode, length byte, lost-packet count) is written by
/// the client layer, not here — `BuildClcMove` itself only produces these two
/// pieces, which is why they are returned separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovePayload {
    pub checksum: u8,
    pub body: Vec<u8>,
}

/// `clc_move` opcode.
pub const CLC_MOVE: u8 = 2;

/// Take an already delta-encoded usercmd payload and finish it the way
/// `BuildClcMove` does: check byte over the clear payload, then munge in place.
///
/// Order matters and is verified: CRC first (`0x1406E8126`), munge after
/// (`0x1406E81C0`). Only whole dwords are munged — the tail is left alone, on
/// both ends.
pub fn finish_move_payload(mut payload: Vec<u8>, sequence: i32) -> MovePayload {
    let checksum = block_sequence_crc_byte(&payload, sequence);
    let n = payload.len() - payload.len() % 4;
    munge::munge(&mut payload[..n], &munge::TABLE1, sequence);
    MovePayload { checksum, body: payload }
}

/// Build a complete `clc_move` message from a clear (unmunged) payload.
///
/// Framing verified against 24,500 real `clc_move` packets: the length byte
/// equals `body.len()`, i.e. everything after the three header bytes, on every
/// single one.
///
/// ```text
/// u8 opcode (2) | u8 length | u8 checksum | <length bytes, munged>
/// ```
///
/// **Validated:** for payloads of 10 and 12 bytes — what a bot sending one
/// usercmd per packet produces — this reproduces the real client's checksum on
/// 39 of 39 captured packets. For payloads of 14 bytes and above the match
/// rate falls to chance, so something further is involved once a real client
/// starts batching backup commands; that case is not yet understood and is not
/// needed to drive a bot.
pub fn build_clc_move(payload_clear: &[u8], sequence: i32) -> Vec<u8> {
    let finished = finish_move_payload(payload_clear.to_vec(), sequence);
    let mut out = Vec::with_capacity(3 + finished.body.len());
    out.push(CLC_MOVE);
    out.push(finished.body.len() as u8);
    out.push(finished.checksum);
    out.extend_from_slice(&finished.body);
    out
}

/// Convenience: does a `usercmd_t` table look like what we expect?
pub fn table_covers_usercmd(table: &[FieldDesc]) -> bool {
    USERCMD_FIELDS
        .iter()
        .filter(|n| **n != "unused")
        .all(|n| table.iter().any(|f| f.name == *n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_match_the_binary_order() {
        assert_eq!(USERCMD_FIELDS[0], "lerp_msec");
        assert_eq!(USERCMD_FIELDS[1], "msec");
        assert_eq!(USERCMD_FIELDS[3], "viewangles[0]");
        assert_eq!(USERCMD_FIELDS[6], "buttons");
        assert_eq!(USERCMD_FIELDS[15], "impact_position[2]");
    }

    #[test]
    fn to_fields_emits_every_wire_field() {
        let m = UserCmd::default().to_fields();
        for name in USERCMD_FIELDS.iter().filter(|n| **n != "unused") {
            assert!(m.contains_key(*name), "missing field {name}");
        }
        assert_eq!(m.len(), 16);
    }

    #[test]
    fn view_angles_survive_the_round_trip_into_fields() {
        let cmd = UserCmd {
            viewangles: [12.5, -90.0, 0.0],
            forwardmove: 250.0,
            buttons: buttons::ATTACK as u16 | buttons::FORWARD as u16,
            ..Default::default()
        };
        let m = cmd.to_fields();
        assert_eq!(m["viewangles[0]"], Value::Float(12.5));
        assert_eq!(m["viewangles[1]"], Value::Float(-90.0));
        assert_eq!(m["forwardmove"], Value::Float(250.0));
        assert!(cmd.is_attacking());
    }

    #[test]
    fn checksum_is_taken_before_munging() {
        // If we munged first the check byte would differ; pin the order.
        let payload = vec![0x10u8; 32];
        let seq = 17;
        let expected = block_sequence_crc_byte(&payload, seq);
        let out = finish_move_payload(payload.clone(), seq);
        assert_eq!(out.checksum, expected);
        assert_ne!(out.body, payload, "body must be munged");
    }

    /// A real `clc_move` captured from a genuine CS 1.6 client, sequence 87,
    /// after the netchannel body was unmunged with table 2.
    const LIVE_MOVE: &[u8] = &[
        0x02, 0x0A, 0xCD, 0x4E, 0x1A, 0x52, 0x57, 0x4D, 0x58, 0xA3, 0x37, 0x00, 0x00,
    ];
    /// The same packet's payload with table 1 removed — what the client had
    /// before munging.
    const LIVE_MOVE_CLEAR: &[u8] = &[
        0x00, 0x02, 0x02, 0x19, 0x20, 0xA3, 0x00, 0x00, 0x00, 0x00,
    ];
    const LIVE_MOVE_SEQ: i32 = 87;

    #[test]
    fn build_clc_move_reproduces_a_real_packet_exactly() {
        // Byte-for-byte against a packet a real Counter-Strike client sent:
        // this simultaneously checks the framing, the sequence CRC and the
        // table-1 munge.
        let built = build_clc_move(LIVE_MOVE_CLEAR, LIVE_MOVE_SEQ);
        assert_eq!(built, LIVE_MOVE, "must match the captured packet");
    }

    #[test]
    fn the_length_byte_counts_everything_after_the_header() {
        let built = build_clc_move(LIVE_MOVE_CLEAR, LIVE_MOVE_SEQ);
        assert_eq!(built[0], CLC_MOVE);
        assert_eq!(usize::from(built[1]), built.len() - 3);
    }

    #[test]
    fn the_checksum_is_the_sequence_crc_of_the_clear_payload() {
        let built = build_clc_move(LIVE_MOVE_CLEAR, LIVE_MOVE_SEQ);
        assert_eq!(
            built[2],
            block_sequence_crc_byte(LIVE_MOVE_CLEAR, LIVE_MOVE_SEQ)
        );
        assert_eq!(built[2], 0xCD, "the value the real client sent");
    }

    #[test]
    fn a_different_sequence_changes_both_checksum_and_body() {
        let a = build_clc_move(LIVE_MOVE_CLEAR, LIVE_MOVE_SEQ);
        let b = build_clc_move(LIVE_MOVE_CLEAR, LIVE_MOVE_SEQ + 1);
        assert_ne!(a[2], b[2], "checksum is sequence-keyed");
        assert_ne!(a[3..], b[3..], "munge is sequence-keyed");
    }

    #[test]
    fn munged_body_unmunges_back_to_the_original() {
        let payload: Vec<u8> = (0u8..64).collect();
        let out = finish_move_payload(payload.clone(), 5);
        let mut back = out.body.clone();
        munge::unmunge(&mut back, &munge::TABLE1, 5);
        assert_eq!(back, payload);
    }

    use crate::bitbuf::BitReader;
    use crate::delta::{parse_delta, DT_BYTE, DT_FLOAT, DT_SHORT, DT_SIGNED};

    /// A minimal `usercmd_t`-shaped table with unit scales, enough to prove the
    /// encoder is the exact inverse of the decoder for the fields a bot sends.
    fn move_table() -> Vec<FieldDesc> {
        let f = |name: &str, ty: u32, bits: u32| FieldDesc {
            name: name.into(),
            field_type: ty,
            bits,
            premultiply: 1.0,
            postmultiply: 1.0,
        };
        vec![
            f("lerp_msec", DT_SHORT, 16),
            f("msec", DT_BYTE, 8),
            f("viewangles[0]", DT_FLOAT | DT_SIGNED, 16),
            f("viewangles[1]", DT_FLOAT | DT_SIGNED, 16),
            f("buttons", DT_SHORT, 16),
            f("forwardmove", DT_FLOAT | DT_SIGNED, 16),
            f("sidemove", DT_FLOAT | DT_SIGNED, 16),
            f("upmove", DT_FLOAT | DT_SIGNED, 16),
            f("impulse", DT_BYTE, 8),
            f("weaponselect", DT_BYTE, 8),
        ]
    }

    #[test]
    fn encode_delta_round_trips_through_parse_delta() {
        let table = move_table();
        let cmd = UserCmd {
            msec: 21,
            viewangles: [10.0, -45.0, 0.0],
            buttons: (buttons::ATTACK | buttons::FORWARD) as u16,
            forwardmove: 250.0,
            sidemove: -160.0,
            weaponselect: 4,
            ..Default::default()
        };
        let bytes = cmd.encode_delta(&UserCmd::default(), &table);

        let mut r = BitReader::new(&bytes);
        let got = parse_delta(&mut r, &table);
        // Only the fields differing from a zeroed baseline are present.
        assert_eq!(got["msec"], Value::Int(21));
        assert_eq!(got["viewangles[0]"], Value::Float(10.0));
        assert_eq!(got["viewangles[1]"], Value::Float(-45.0));
        assert_eq!(got["buttons"], Value::Int(i64::from(cmd.buttons)));
        assert_eq!(got["forwardmove"], Value::Float(250.0));
        assert_eq!(got["sidemove"], Value::Float(-160.0));
        assert_eq!(got["weaponselect"], Value::Int(4));
        // Untouched fields are absent from the delta.
        assert!(!got.contains_key("upmove"));
        assert!(!got.contains_key("lerp_msec"));
        assert!(!got.contains_key("impulse"));
    }

    #[test]
    fn a_zeroed_command_encodes_to_the_single_valid_null_byte() {
        // Matches the wire fact that a lone 0x00 is a complete usercmd.
        let table = move_table();
        let bytes = UserCmd::default().encode_delta(&UserCmd::default(), &table);
        assert_eq!(bytes, vec![0x00]);
    }

    #[test]
    fn build_move_payload_frames_loss_backup_and_count() {
        let table = move_table();
        let cmd = UserCmd { msec: 20, forwardmove: 250.0, ..Default::default() };
        let payload = build_move_payload(0, &[cmd], &table);
        assert_eq!(payload[0], 0, "packet loss");
        assert_eq!(payload[1], 0, "numbackup");
        assert_eq!(payload[2], 1, "numcmds");

        // The bytes after the 3-byte header are one decodable usercmd delta.
        let mut r = BitReader::new(&payload[3..]);
        let got = parse_delta(&mut r, &table);
        assert_eq!(got["msec"], Value::Int(20));
        assert_eq!(got["forwardmove"], Value::Float(250.0));
    }

    #[test]
    fn a_full_move_packet_builds_from_a_usercmd() {
        // End to end: UserCmd -> clear payload -> framed, checksummed, munged
        // clc_move. Proves the encoder output is something build_clc_move
        // accepts and that the length byte stays consistent.
        let table = move_table();
        let cmd = UserCmd {
            msec: 20,
            viewangles: [0.0, 90.0, 0.0],
            buttons: buttons::FORWARD as u16,
            forwardmove: 250.0,
            ..Default::default()
        };
        let payload = build_move_payload(0, &[cmd], &table);
        let packet = build_clc_move(&payload, 42);
        assert_eq!(packet[0], CLC_MOVE);
        assert_eq!(usize::from(packet[1]), packet.len() - 3);
        assert_eq!(packet[2], block_sequence_crc_byte(&payload, 42));
    }

    #[test]
    fn successive_commands_delta_against_the_previous_one() {
        // Two identical commands: the second deltas to the empty (0x00) delta
        // because nothing changed from the first.
        let table = move_table();
        let cmd = UserCmd { msec: 20, forwardmove: 250.0, ..Default::default() };
        let payload = build_move_payload(0, &[cmd, cmd], &table);
        assert_eq!(payload[2], 2, "numcmds");
        // Last byte is the second command's empty delta.
        assert_eq!(*payload.last().unwrap(), 0x00);
    }
}
