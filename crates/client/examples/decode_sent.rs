//! Decode our own outgoing packets and report what movement we really sent.
//!
//! Written because the bot reported `forwardmove 250` at every level we can
//! see from inside — the brain, the intent, the `UserCmd` — and did not move.
//! At that point the only honest thing left is to read the bytes that actually
//! went out, with the same decoder the server would use.
//!
//! ```text
//! cargo run -p client --example decode_sent -- captures/foo.bin.sent [signon.bin]
//! ```
//!
//! Input is the `.sent` log written by `capture_running`: repeated
//! `u32 length` + `length` bytes, one record per datagram.

use std::env;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| "running.bin.sent".into());
    let data = std::fs::read(&path).expect("read capture");

    // The usercmd_t table is per-server; take it from the checked-in signon.
    const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");
    let signon = client::walk_signon(SIGNON);
    let table = signon
        .registry
        .get("usercmd_t")
        .expect("usercmd_t table")
        .clone();

    let mut at = 0usize;
    let mut packets = 0u32;
    let mut moves = 0u32;
    let mut nonzero_forward = 0u32;
    let mut last: Option<(f32, f32, f32, i64)> = None;

    while at + 4 <= data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        let pkt = &data[at..at + len];
        at += len;
        packets += 1;

        if pkt.len() <= 8 {
            continue;
        }
        // Netchannel: 8-byte header, body munged with table 2 on the sequence.
        let seq = u32::from_le_bytes(pkt[0..4].try_into().unwrap()) & 0x3FFF_FFFF;
        let mut body = pkt[8..].to_vec();
        let n = body.len() - body.len() % 4;
        proto::munge::unmunge(&mut body[..n], &proto::munge::TABLE2, seq as i32);

        if body.first() != Some(&proto::usercmd::CLC_MOVE) || body.len() < 4 {
            continue;
        }
        let mlen = body[1] as usize;
        if body.len() < 3 + mlen {
            continue;
        }
        // clc_move payload is munged with TABLE1 on the FULL sequence.
        let mut payload = body[3..3 + mlen].to_vec();
        let pn = payload.len() - payload.len() % 4;
        proto::munge::unmunge(&mut payload[..pn], &proto::munge::TABLE1, seq as i32);
        if payload.len() < 3 {
            continue;
        }
        let (loss, numbackup, numcmds) = (payload[0], payload[1], payload[2]);
        moves += 1;

        // Commands are chained oldest-first, each delta'd against the previous.
        let mut r = proto::bitbuf::BitReader::new(&payload[3..]);
        let total = usize::from(numbackup) + usize::from(numcmds);
        let mut cur = (0.0f32, 0.0f32, 0.0f32, 0i64);
        for _ in 0..total {
            let f = proto::delta::parse_delta(&mut r, &table);
            let g = |k: &str, d: f32| f.get(k).and_then(proto::delta::Value::as_f32).unwrap_or(d);
            cur = (
                g("forwardmove", cur.0),
                g("sidemove", cur.1),
                g("viewangles[1]", cur.2),
                f.get("msec")
                    .and_then(proto::delta::Value::as_i64)
                    .unwrap_or(cur.3),
            );
            if r.overflowed() {
                break;
            }
        }
        if cur.0.abs() > 1.0 {
            nonzero_forward += 1;
        }
        if moves <= 3 || moves % 250 == 0 {
            println!(
                "seq {seq:<6} loss {loss} backup {numbackup} cmds {numcmds} | \
                 forwardmove {:>7.1} sidemove {:>7.1} yaw {:>7.1} msec {}",
                cur.0, cur.1, cur.2, cur.3
            );
        }
        last = Some(cur);
    }

    println!("\npackets {packets}, clc_move {moves}, with forwardmove != 0: {nonzero_forward}");
    if let Some(l) = last {
        println!("last decoded command: forwardmove {:.1} sidemove {:.1} yaw {:.1} msec {}", l.0, l.1, l.2, l.3);
    }
    if moves > 0 && nonzero_forward == 0 {
        println!("\n!!! every clc_move carried forwardmove 0 -- the movement never left the client");
    }
}
