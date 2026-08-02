//! What makes this stack accept us, checked against **captured bytes**.
//!
//! Two fixtures, both recorded off the wire, neither one written by hand:
//!
//! * `real_client_c2s_spawn.bin` — 35 consecutive client→server datagrams from
//!   a genuine CS 1.6 client (`captures/real_client_relay.bin`, relay record
//!   format: `u32 len, u8 direction, u64 microseconds, bytes`), covering the
//!   half-second either side of its `spawn`. This is the evidence for the fix:
//!   a real client stops sending `clc_nop` the moment it answers
//!   `svc_resourcerequest`, and from then on puts a `clc_move` in *every*
//!   packet — including the ones where it has nothing new to say.
//! * `svc_stufftext_reconnect.bin` — the twelve bytes this server really sends
//!   on a level change, captured live from a `changelevel` (`SV_BuildReconnect`,
//!   `rehlds/engine/sv_main.cpp:5955-5959`).
//!
//! Why that matters: ReAuthCheck's method 7 ("Player Validation") drops a client
//! that reaches `spawn` having never sent a `clc_move`. Bisected live on one
//! env var — all-`clc_nop` idles get `svc_disconnect "Error! Is Not Valid Auth
//! (7)."` ~240 ms after the spawn upload, `clc_move` idles stay for minutes.

use std::time::Duration;

use client::{Identity, Session};

const REAL_C2S: &[u8] = include_bytes!("fixtures/real_client_c2s_spawn.bin");
const SIGNON: &[u8] = include_bytes!("fixtures/signon.bin");
const RECONNECT: &[u8] = include_bytes!("fixtures/svc_stufftext_reconnect.bin");

const CLC_NOP: u8 = 1;
const CLC_MOVE: u8 = 2;

/// One datagram out of a relay capture.
struct Record {
    micros: u64,
    data: Vec<u8>,
}

/// Walk the relay record format. Only client→server records are kept, which is
/// all this fixture contains.
fn records(mut raw: &[u8]) -> Vec<Record> {
    let mut out = Vec::new();
    while raw.len() >= 13 {
        let len = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
        let dir = raw[4];
        let micros = u64::from_le_bytes(raw[5..13].try_into().unwrap());
        let body = &raw[13..13 + len];
        if dir == 0 {
            out.push(Record { micros, data: body.to_vec() });
        }
        raw = &raw[13 + len..];
    }
    out
}

/// The cleartext body of a captured client→server netchannel packet.
///
/// `Netchan_Transmit` munges everything after the 8-byte header with table 2
/// keyed on the low byte of the sequence it just wrote
/// (`net_chan.cpp`, mirrored by `netchan::NetChannel::transmit`), so undoing it
/// needs nothing but the header we already have.
fn cleartext(datagram: &[u8]) -> Vec<u8> {
    let seq = u32::from_le_bytes(datagram[0..4].try_into().unwrap());
    let mut body = datagram[8..].to_vec();
    let key = (seq & netchan::MUNGE_SEQUENCE_MASK) as i32;
    proto::munge::unmunge(&mut body, &proto::munge::TABLE2, key);
    body
}

/// Does this packet body end in a `clc_move`?
///
/// `clc_move` is `02 <len> <checksum> <len bytes>` and a real client puts it
/// last, after any reliable payload, with at most three `clc_nop` bytes of
/// dword padding behind it. Anchoring on the end is what makes this
/// unambiguous: an `02` inside a munged payload cannot also happen to have a
/// length byte that lands exactly on the tail.
fn trailing_move_len(body: &[u8]) -> Option<usize> {
    for pad in 0..4usize {
        if body.len() < pad || !body[body.len() - pad..].iter().all(|&b| b == CLC_NOP) {
            continue;
        }
        let end = body.len() - pad;
        for start in 0..end {
            if body[start] != CLC_MOVE {
                continue;
            }
            let len = *body.get(start + 1)? as usize;
            if start + 3 + len == end {
                return Some(len);
            }
        }
    }
    None
}

fn only_nops(body: &[u8]) -> bool {
    !body.is_empty() && body.iter().all(|&b| b == CLC_NOP)
}

fn loaded_session() -> Session {
    let mut s = Session::new(Identity { name: "Test".into(), ..Default::default() });
    s.signon = Some(client::signon::walk(SIGNON));
    assert!(
        s.signon.as_ref().unwrap().registry.get("usercmd_t").is_some(),
        "the signon fixture must carry the usercmd_t delta table"
    );
    s
}

/// The fixture itself: the real client goes from silence to moves at `sendres`.
///
/// This is the observation the whole fix rests on, so it is asserted on the
/// captured bytes rather than described in a comment. 23 keepalive packets
/// carrying nothing but `clc_nop`, then — from the packet that answers
/// `svc_resourcerequest` onwards — never another one.
#[test]
fn a_real_client_stops_sending_clc_nop_the_moment_it_answers_sendres() {
    let recs = records(REAL_C2S);
    assert_eq!(recs.len(), 35, "fixture should hold 35 client->server datagrams");

    let bodies: Vec<Vec<u8>> = recs.iter().map(|r| cleartext(&r.data)).collect();
    let first_move = bodies
        .iter()
        .position(|b| trailing_move_len(b).is_some())
        .expect("the real client must send at least one clc_move");

    assert_eq!(
        first_move, 23,
        "the switch happens at the resource-list upload, not earlier"
    );
    assert!(
        bodies[..first_move].iter().all(|b| only_nops(b)),
        "before it answers sendres the real client sends nothing but clc_nop"
    );
    assert!(
        bodies[first_move..].iter().all(|b| !only_nops(b)),
        "after that point NO packet is nop-only -- this is exactly what our \
         client used to do for the whole handshake"
    );
    // Strongest form of the claim, on the packets where it is unambiguous: a
    // datagram with the reliable bit clear carries nothing but the unreliable
    // block, so if there is a move in it, it starts at byte 0.
    let mut plain = 0usize;
    for (i, body) in bodies.iter().enumerate().skip(first_move) {
        let seq = u32::from_le_bytes(recs[i].data[0..4].try_into().unwrap());
        if seq & netchan::RELIABLE_FLAG != 0 {
            continue;
        }
        plain += 1;
        assert_eq!(body[0], CLC_MOVE, "packet {i} is a bare datagram: it IS a move");
        assert!(trailing_move_len(body).is_some(), "packet {i} framing");
    }
    assert!(plain >= 8, "the fixture must contain plain move-only datagrams");

    // And it is doing this *before* it has spawned: the spawn stringcmd is in
    // the very next fragmented packet, 21 ms later.
    let dt = recs[first_move + 1].micros - recs[first_move].micros;
    assert!(dt < 40_000, "spawn follows the first move within a frame or two");
}

/// The real client's first move carries **no new commands** — it is a
/// keepalive move, not a movement. That is the shape `Session::idle_body`
/// has to produce, and the reason it exists.
#[test]
fn the_real_clients_first_move_is_two_backup_commands_and_no_new_ones() {
    let recs = records(REAL_C2S);
    let bodies: Vec<Vec<u8>> = recs.iter().map(|r| cleartext(&r.data)).collect();
    let (idx, body) = bodies
        .iter()
        .enumerate()
        .find(|(_, b)| trailing_move_len(b).is_some())
        .expect("a move");
    assert_eq!(idx, 23);

    let len = trailing_move_len(body).unwrap();
    let start = body.len() - len; // payload start (after opcode/len/checksum)
    let seq = u32::from_le_bytes(recs[idx].data[0..4].try_into().unwrap()) & netchan::SEQUENCE_MASK;
    let mut payload = body[start..].to_vec();
    let n = payload.len() - payload.len() % 4;
    proto::munge::unmunge(&mut payload[..n], &proto::munge::TABLE1, seq as i32);

    assert_eq!(payload[0], 0, "packet loss");
    assert_eq!(payload[1], 2, "numbackup = 2");
    assert_eq!(payload[2], 0, "numcmds = 0 -- a move with nothing new in it");
}

/// The regression guard, stated as the thing that actually went wrong: once the
/// delta tables are known, the idle payload is **never** a bare `clc_nop`.
///
/// A client that idles on `clc_nop` reaches ReAuthCheck's Player Validation
/// having sent zero commands and is dropped with
/// `Error! Is Not Valid Auth (7).` ~240 ms after `spawn`.
#[test]
fn our_idle_body_is_a_move_not_a_nop_once_the_signon_is_known() {
    let mut s = loaded_session();
    let body = s.idle_body();
    assert_ne!(body, vec![CLC_NOP], "this exact value is the bug");
    assert_eq!(body[0], CLC_MOVE);
    assert_eq!(
        body.len(),
        3 + body[1] as usize,
        "clc_move framing: opcode, length, checksum, then `length` bytes"
    );
}

/// …and it is the same shape as the real client's: `numbackup = 2`,
/// `numcmds = 0`. Decoded with our own unmunge, so the checksum/munge order is
/// exercised too.
#[test]
fn our_idle_move_decodes_to_the_same_header_the_real_client_sends() {
    let mut s = loaded_session();
    let seq = s.chan.outgoing_sequence as i32;
    let body = s.idle_body();
    let mut payload = body[3..].to_vec();
    let n = payload.len() - payload.len() % 4;
    proto::munge::unmunge(&mut payload[..n], &proto::munge::TABLE1, seq);

    assert_eq!(payload[0], 0, "packet loss");
    assert_eq!(payload[1], 2, "numbackup");
    assert_eq!(payload[2], 0, "numcmds");
}

/// Before the signon there is no `usercmd_t` table to encode against, and a
/// real client sends `clc_nop` there too (first 23 packets of the fixture).
#[test]
fn before_the_signon_the_idle_body_is_still_a_nop() {
    let mut s = Session::new(Identity::default());
    assert_eq!(s.idle_body(), vec![CLC_NOP]);
}

/// The twelve bytes the live server sent on `changelevel`, fed to the real
/// stufftext parser. It must be recognised, latched, and **not** echoed back:
/// `reconnect` is a command to execute, not one to report.
#[test]
fn the_servers_reconnect_stufftext_is_recognised_from_the_captured_bytes() {
    assert_eq!(RECONNECT, b"\x09reconnect\n\0", "fixture is svc_stufftext + string");

    let mut s = loaded_session();
    assert!(!s.pending_reconnect);
    let echoed = s.echo_stufftexts(RECONNECT);
    assert!(s.pending_reconnect, "the command must be latched for the frame loop");
    assert_eq!(echoed, vec!["reconnect".to_string()]);
    assert_eq!(
        s.chan.queued_count(),
        0,
        "nothing is sent back -- echoing `reconnect` at the server is not a reply"
    );
}

/// `reconnect` is the engine's, not an invention: netchannel reset, state back
/// to pre-signon, and `clc_stringcmd \"new\"` queued. No second handshake, and
/// **the delta tables survive** — otherwise we would fall back to `clc_nop` for
/// the whole re-signon, which is the very thing that gets a client flagged.
#[test]
fn reconnect_resets_the_session_but_keeps_what_the_engine_keeps() {
    let mut s = loaded_session();
    s.spawn_uploaded = true;
    s.pending_reconnect = true;
    s.last_valid_frame = Some(17);
    s.chan.queue_reliable(b"\x03stale\0");
    s.chan.queue_fragmented(&vec![0u8; 400]);

    let seq_before = s.chan.outgoing_sequence;
    s.reconnect();

    assert!(!s.pending_reconnect);
    assert!(!s.spawn_uploaded);
    assert!(s.last_valid_frame.is_none());
    assert!(s.resource_message.is_none());
    assert!(s.decoder.is_none());
    assert!(!s.chan.fragment_upload_active(), "the old upload is abandoned");
    assert_eq!(
        s.chan.outgoing_sequence, seq_before,
        "Netchan_Clear does not touch the sequence numbers"
    );
    assert!(
        s.signon.is_some(),
        "delta tables outlive a reconnect in the engine, and must here"
    );
    assert_ne!(s.idle_body(), vec![CLC_NOP], "so we keep moving through the re-signon");
    assert_eq!(s.chan.queued_count(), 1, "exactly one thing queued: `new`");
}

/// A stale reliable that the server has already thrown away must not be
/// retransmitted, and the reliable bit has to flip with it — `Netchan_Clear`
/// does `chan->reliable_sequence ^= 1` for precisely this case.
#[test]
fn clearing_the_channel_drops_the_in_flight_reliable_and_flips_the_bit() {
    let mut ch = netchan::NetChannel::new();
    ch.queue_reliable(b"\x03new\0");
    let _ = ch.transmit(&[CLC_NOP]); // promotes it: now in flight
    assert!(ch.reliable_in_flight());
    let bit = ch.outgoing_reliable;

    ch.clear();
    assert!(!ch.reliable_in_flight());
    assert_eq!(ch.queued_count(), 0);
    assert_ne!(ch.outgoing_reliable, bit, "reliable_sequence ^= 1");
}

/// Sanity: a session with no transport still refuses to block forever.
/// (Guards the signature of the reconnect entry points more than the logic.)
#[test]
fn resignon_reports_a_timeout_rather_than_hanging() {
    struct Dead;
    impl client::Transport for Dead {
        fn send(&mut self, _: &[u8]) -> std::io::Result<()> {
            Ok(())
        }
        fn recv(&mut self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }
    let mut s = loaded_session();
    let err = s.resignon(&mut Dead, Duration::from_millis(120)).unwrap_err();
    assert_eq!(err, client::Disconnect::Timeout);
}
