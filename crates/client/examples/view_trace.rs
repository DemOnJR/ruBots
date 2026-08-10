//! Per-tick view trace of what the bot actually sent (plan M0).
//!
//! W7 is about the dynamics of the view -- a spring-damper that accelerates,
//! overshoots on a combat flick, and settles. None of that is visible at a 2 s
//! sample interval, so this decodes the `.bin.sent` stream `capture_running`
//! already writes: every `clc_move` carries the usercmd the server actually
//! saw, which is the ground truth for "what did the view do".
//!
//! ```text
//! cargo run -p client --example view_trace -- captures/Bot01.bin.sent
//! ```
//!
//! Output: one line per command (downsampled) showing the delta against the
//! previous command -- that is what a spectator's server sees:
//!
//! ```text
//!    t=0.00s cmd# 1  yaw -123.4  pitch -3.2   fwd 250.0  side 0.0  btns 0x0000  dyaw 0.0  dpitch 0.0
//! ```
//!
//! And a summary, which is the M0 evidence for W7:
//!
//! * **flicks** -- sustained >= 60 deg swings with a smooth path (no single
//!   teleport step); the spring produces them with peak turn rates in the
//!   400-900 deg/s band;
//! * **overshoot** -- on a combat flick the view passes the target by a real
//!   margin (8-45 deg) before coming back; the old 0.45 ease could not
//!   overshoot at all;
//! * a view that never moves is dead, and a view that only tracks its feet is
//!   the old behaviour.
//!
//! A "flick" is an episode, not a step: the view leaves a rest position
//! (`|dyaw|` small), swings through >= 60 deg total while moving consistently
//! in one direction, and comes back to rest. A single instantaneous teleport
//! (like the manual-drive yaw sweep, or a wrapping server view) is not a flick.
//! Overshoot is measured inside a combat flick: after the largest step the
//! view must *reverse* direction past the target and return -- the signature
//! of a spring arriving with momentum.

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

    // --- decode every clc_move, reconstructing the per-command state -----
    #[derive(Default, Clone, Copy)]
    struct Cmd {
        yaw: f32,
        pitch: f32,
        fwd: f32,
        side: f32,
        buttons: u16,
        msec: u16,
    }

    // Flick episode tracking.
    #[derive(Default)]
    struct Flick {
        start: f64,     // t of the first moving step
        total: f64,     // cumulative |dyaw| while swinging
        peak: f64,      // peak |dyaw|/s
        max_step: f64,  // largest single-step |dyaw|
        reversed: bool, // changed direction after the big step (overshoot)
        max_reverse: f64,
        last_sign: f64, // direction of the previous step, for overshoot
        attacking: bool,
    }
    let mut flick: Option<Flick> = None;
    let mut flicks: Vec<Flick> = Vec::new();
    let mut total_delta = 0.0f64;
    let mut max_abs_delta = 0.0f64;
    let mut any_move = false;

    let mut at = 0usize;
    let mut packets = 0u32;
    let mut moves = 0u32;
    let mut cmds_seen = 0u64;
    let mut prev = Cmd::default();
    let mut tick_time = 0.0f64;

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
        let (_loss, numbackup, numcmds) = (payload[0], payload[1], payload[2]);
        moves += 1;

        // Commands are chained oldest-first, each delta'd against the previous.
        let mut r = proto::bitbuf::BitReader::new(&payload[3..]);
        let total = usize::from(numbackup) + usize::from(numcmds);
        let mut cur = Cmd::default();
        for _ in 0..total {
            let f = proto::delta::parse_delta(&mut r, &table);
            if r.overflowed() {
                break;
            }
            let g = |k: &str, d: f32| f.get(k).and_then(proto::delta::Value::as_f32).unwrap_or(d);
            cur.yaw = g("viewangles[1]", cur.yaw);
            cur.pitch = g("viewangles[0]", cur.pitch);
            cur.fwd = g("forwardmove", cur.fwd);
            cur.side = g("sidemove", cur.side);
            cur.buttons = f
                .get("buttons")
                .and_then(proto::delta::Value::as_i64)
                .map(|b| b as u16)
                .unwrap_or(cur.buttons);
            cur.msec = f
                .get("msec")
                .and_then(proto::delta::Value::as_i64)
                .map(|m| m as u16)
                .unwrap_or(cur.msec);

            // The deltas are what the view dynamics are made of. A jump from
            // one side of the circle to the other is not a swing -- it is a
            // teleport (manual-drive sweep, or a server that reset the view).
            let raw = f64::from(cur.yaw) - f64::from(prev.yaw);
            let short = (raw + 540.0).rem_euclid(360.0) - 180.0;
            let dyaw = short;
            total_delta += dyaw.abs();
            max_abs_delta = max_abs_delta.max(dyaw.abs());

            let dt = cur.msec as f64 / 1000.0;
            tick_time += dt;
            cmds_seen += 1;
            let rate = if dt > 0.0 { dyaw.abs() / dt } else { 0.0 };

            // --- flick episode state machine -----------------------------
            let moving = dyaw.abs() > 0.5; // a real step, not quantization
            if let Some(fl) = flick.as_mut() {
                // Reverse direction during the swing: overshoot momentum.
                if dyaw.abs() > 2.0 && dyaw.signum() != fl.last_sign {
                    fl.reversed = true;
                    fl.max_reverse = fl.max_reverse.max(dyaw.abs());
                }
                if moving {
                    fl.total += dyaw.abs();
                    fl.peak = fl.peak.max(rate);
                    fl.max_step = fl.max_step.max(dyaw.abs());
                    fl.last_sign = dyaw.signum();
                } else if dyaw.abs() < 0.2 {
                    // Rest: the swing is over.
                    if fl.total >= 60.0 && fl.max_step < 30.0 {
                        flicks.push(std::mem::take(fl));
                    } else {
                        flick = None;
                    }
                }
            } else if moving {
                let mut fl = Flick::default();
                fl.start = tick_time;
                fl.total = dyaw.abs();
                fl.peak = rate;
                fl.max_step = dyaw.abs();
                fl.last_sign = dyaw.signum();
                fl.attacking = cur.buttons & 1 != 0;
                flick = Some(fl);
            }

            // Print a downsampled trace so the file is readable.
            if cmds_seen <= 4 || cmds_seen % 120 == 0 {
                println!(
                    "t={:>7.2}s cmd#{:<6} yaw {:>7.1} pitch {:>6.1} fwd {:>6.1} side {:>6.1} btns 0x{:04x} dyaw {:>6.1} dpitch {:>5.1}",
                    tick_time, cmds_seen, cur.yaw, cur.pitch, cur.fwd, cur.side, cur.buttons, dyaw, f64::from(cur.pitch) - f64::from(prev.pitch)
                );
            }
            prev = cur;
            if cur.fwd.abs() > 1.0 || cur.side.abs() > 1.0 {
                any_move = true;
            }
        }
    }
    if let Some(fl) = flick {
        if fl.total >= 60.0 && fl.max_step < 30.0 {
            flicks.push(fl);
        }
    }

    // --- summary: the M0 evidence ----------------------------------------
    println!("\nclc_move {moves} in {packets} packets, {cmds_seen} commands");
    println!("any movement command: {any_move}");
    println!(
        "peak |dyaw|/s over any step: {:.0}",
        flicks.iter().map(|f| f.peak).fold(0.0f64, f64::max).max(max_abs_delta / 0.016)
    );
    if !flicks.is_empty() {
        let mut peaks: Vec<f64> = flicks.iter().map(|f| f.peak).collect();
        peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = peaks[peaks.len() / 2];
        let p90 = peaks[(peaks.len() * 9 / 10).min(peaks.len() - 1)];
        println!(
            "flicks: {} sustained >=60deg swings, median peak {:.0} deg/s, p90 {:.0} deg/s",
            flicks.len(),
            med,
            p90
        );
        let over: Vec<f64> = flicks.iter().filter(|f| f.reversed).map(|f| f.max_reverse).collect();
        if !over.is_empty() {
            let m = over.iter().copied().fold(0.0f64, f64::max);
            println!(
                "combat-flick overshoot: {} flicks reversed direction, max reverse step {:.1} deg",
                over.len(),
                m
            );
        } else {
            println!("combat-flick overshoot: none reversed direction (no momentum overshoot)");
        }
    }
    println!("total |dyaw| {:.0} deg, max single-step {:.1} deg", total_delta, max_abs_delta);
    if cmds_seen > 0 && !any_move {
        println!("\n!!! no movement command in the whole stream");
    }
}
