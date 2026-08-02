//! Turning a bot [`Intent`] into the `clc_move` datagrams that go on the wire.
//!
//! This is the outbound half of the in-game session — the seam where an
//! engine-neutral decision becomes a real, munged, sequenced packet. It is the
//! mirror of the signon walk (which learns the `usercmd_t` layout): the same
//! table drives both the decode of what the server sends and the encode of
//! what the bot sends back.
//!
//! Framing follows what a real CS 1.6 client does and what ReHLDS
//! `SV_ParseMove` requires: one `clc_move` per packet, its payload
//! `[packet_loss, numbackup=0, numcmds=1]` then a single `usercmd_t` delta
//! against a zeroed command. The whole thing is checksummed and table-1 munged
//! by [`build_clc_move`], then the netchannel table-2 munges the body.

use bot::Intent;
use proto::delta::FieldDesc;
use proto::usercmd::{buttons, build_clc_move, build_move_payload, UserCmd};

use netchan::NetChannel;

/// Map an [`Intent`] onto a `usercmd_t` for a tick of `msec` milliseconds.
///
/// `viewangles[0]` is pitch and `viewangles[1]` is yaw, matching the engine's
/// field order (see [`bot::Angles`]). `msec` is how much game time this command
/// accounts for — the server integrates movement over it.
pub fn intent_to_usercmd(intent: &Intent, msec: u8) -> UserCmd {
    let mut btn = 0u32;
    if intent.attack {
        btn |= buttons::ATTACK;
    }
    if intent.jump {
        btn |= buttons::JUMP;
    }
    if intent.duck {
        btn |= buttons::DUCK;
    }
    if intent.use_action {
        btn |= buttons::USE;
    }
    if intent.forwardmove > 0.0 {
        btn |= buttons::FORWARD;
    } else if intent.forwardmove < 0.0 {
        btn |= buttons::BACK;
    }

    UserCmd {
        msec,
        // lerp_msec tracks the frame interval; msec is a safe, accepted value.
        lerp_msec: i16::from(msec),
        viewangles: [intent.view.pitch, intent.view.yaw, 0.0],
        buttons: btn as u16,
        forwardmove: intent.forwardmove,
        sidemove: intent.sidemove,
        ..Default::default()
    }
}

/// The outbound in-game driver: owns the netchannel and the learned
/// `usercmd_t` table, and turns each tick's [`Intent`] into a datagram.
#[derive(Debug, Clone)]
pub struct MoveSender {
    pub chan: NetChannel,
    usercmd_table: Vec<FieldDesc>,
}

impl MoveSender {
    /// `usercmd_table` is the `usercmd_t` description recovered from the signon
    /// (`Signon::registry.get("usercmd_t")`).
    pub fn new(chan: NetChannel, usercmd_table: Vec<FieldDesc>) -> Self {
        Self { chan, usercmd_table }
    }

    /// Build (and sequence) the datagram carrying this tick's command.
    ///
    /// The `clc_move` payload is munged with the *outgoing* sequence, which is
    /// the same sequence the netchannel then uses for the body munge — so both
    /// layers agree. Advances the netchannel sequence exactly once.
    pub fn build_move(&mut self, intent: &Intent, msec: u8) -> Vec<u8> {
        let cmd = intent_to_usercmd(intent, msec);
        let payload = build_move_payload(0, &[cmd], &self.usercmd_table);
        // The clc_move must be munged with the sequence the packet will carry.
        let seq = self.chan.outgoing_sequence as i32;
        let move_msg = build_clc_move(&payload, seq);
        self.chan.build(&move_msg, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot::Angles;
    use proto::bitbuf::BitReader;
    use proto::delta::{parse_delta, Value};
    use proto::munge;

    const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");

    fn usercmd_table() -> Vec<FieldDesc> {
        let s = crate::signon::walk(SIGNON);
        s.registry.get("usercmd_t").expect("usercmd_t table").clone()
    }

    #[test]
    fn button_bools_map_onto_the_right_bits() {
        let intent = Intent {
            attack: true,
            use_action: true,
            forwardmove: 250.0,
            ..Intent::default()
        };
        let cmd = intent_to_usercmd(&intent, 20);
        let b = u32::from(cmd.buttons);
        assert!(b & buttons::ATTACK != 0);
        assert!(b & buttons::USE != 0);
        assert!(b & buttons::FORWARD != 0);
        assert!(b & buttons::BACK == 0);
        assert_eq!(cmd.msec, 20);
    }

    #[test]
    fn backpedalling_sets_the_back_bit() {
        let intent = Intent { forwardmove: -250.0, ..Intent::default() };
        let cmd = intent_to_usercmd(&intent, 20);
        let b = u32::from(cmd.buttons);
        assert!(b & buttons::BACK != 0);
        assert!(b & buttons::FORWARD == 0);
    }

    /// End to end against the REAL `usercmd_t` table: an Intent aiming at yaw
    /// 90 must produce a datagram whose body, once both munge layers are
    /// peeled off, decodes back to a `clc_move` carrying that yaw. This proves
    /// the entire outbound stack — mapping, delta encode, CRC, both munges and
    /// the netchannel framing — is self-consistent with the decode path.
    #[test]
    fn a_move_datagram_round_trips_back_to_the_intended_command() {
        let table = usercmd_table();
        let mut sender = MoveSender::new(NetChannel::new(), table.clone());
        let intent = Intent {
            view: Angles { pitch: 0.0, yaw: 90.0 },
            forwardmove: 250.0,
            attack: true,
            ..Intent::default()
        };

        // The sequence this packet will be built with.
        let seq = sender.chan.outgoing_sequence as i32;
        let datagram = sender.build_move(&intent, 20);

        // Peel the netchannel: 8-byte header, body munged with table 2 + seq.
        assert!(datagram.len() > 8);
        let mut body = datagram[8..].to_vec();
        let n = body.len() - body.len() % 4;
        munge::unmunge(&mut body[..n], &munge::TABLE2, seq);

        // First message is the clc_move.
        assert_eq!(body[0], proto::usercmd::CLC_MOVE);
        let mlen = body[1] as usize;
        let mut payload = body[3..3 + mlen].to_vec();
        let pn = payload.len() - payload.len() % 4;
        munge::unmunge(&mut payload[..pn], &munge::TABLE1, seq);

        // [loss, numbackup, numcmds] then one usercmd delta.
        assert_eq!(payload[0], 0, "packet loss");
        assert_eq!(payload[2], 1, "one command");
        let mut r = BitReader::new(&payload[3..]);
        let fields = parse_delta(&mut r, &table);

        // The yaw survived the whole pipeline (allowing for angle quantisation).
        let yaw = fields
            .get("viewangles[1]")
            .and_then(Value::as_f32)
            .expect("yaw present in the delta");
        assert!((yaw - 90.0).abs() < 1.0, "yaw {yaw} should be ~90");
        // And the forward move.
        let fwd = fields
            .get("forwardmove")
            .and_then(Value::as_f32)
            .expect("forwardmove present");
        assert!((fwd - 250.0).abs() < 1.0, "forwardmove {fwd} ~ 250");
    }

    #[test]
    fn each_move_advances_the_sequence_once() {
        let mut sender = MoveSender::new(NetChannel::new(), usercmd_table());
        let s0 = sender.chan.outgoing_sequence;
        sender.build_move(&Intent::default(), 20);
        assert_eq!(sender.chan.outgoing_sequence, s0 + 1);
        sender.build_move(&Intent::default(), 20);
        assert_eq!(sender.chan.outgoing_sequence, s0 + 2);
    }
}
