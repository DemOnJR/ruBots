//! What the crosshair really did, decoded from our own outgoing commands.
//!
//! "The view looks wrong" is not a measurement, and the angles the brain
//! *intended* are not the angles that went out: the aim spring, the punch
//! compensation and the anti-idle drift all sit between them. This decodes the
//! `.sent` capture with the same table the server uses and reports the motion
//! of the sent `viewangles`.
//!
//! ```text
//! cargo run -p client --example look_trace -- captures/swarm/ruBot01.bin.sent [--csv]
//! ```
//!
//! The numbers that matter for "does this look like a person":
//!
//! * **reversals/s** — how often the yaw changes direction. A human scanning a
//!   corner reverses a handful of times a minute; a shaking crosshair reverses
//!   several times a second, and that is the tell.
//! * **median |dyaw|** — the typical per-command step. Sub-degree steps that
//!   keep reversing are a tremor, not a look.
//! * **still share** — commands where neither axis moved at all.
//! * **dwell** — the longest run of commands with no yaw motion, in seconds:
//!   a person parks the crosshair on a doorway and leaves it there.

use std::env;

fn main() {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "running.bin.sent".to_string());
    let csv = args.any(|a| a == "--csv");
    let data = std::fs::read(&path).expect("read capture");

    const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");
    let signon = client::walk_signon(SIGNON);
    let table = signon
        .registry
        .get("usercmd_t")
        .expect("usercmd_t table")
        .clone();

    // (msec, pitch, yaw) per command, oldest first.
    let mut trace: Vec<(f32, f32, f32)> = Vec::new();
    // Carried ACROSS datagrams on purpose. A usercmd delta omits any field
    // that did not change, and the value it is omitted against is the previous
    // command the server saw -- not zero, and not the start of this datagram.
    // Resetting per packet invents a jump to zero every time the view holds
    // still, which reads as a huge swing in the trace and is exactly the
    // artefact that would make a steady crosshair look violent.
    let mut cur = (0.0f32, 0.0f32, 0.0f32);
    let mut at = 0usize;
    while at + 4 <= data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes")) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        let pkt = &data[at..at + len];
        at += len;
        if pkt.len() <= 8 {
            continue;
        }
        let seq = u32::from_le_bytes(pkt[0..4].try_into().expect("4 bytes")) & 0x3FFF_FFFF;
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
        let mut payload = body[3..3 + mlen].to_vec();
        let pn = payload.len() - payload.len() % 4;
        proto::munge::unmunge(&mut payload[..pn], &proto::munge::TABLE1, seq as i32);
        if payload.len() < 3 {
            continue;
        }
        let (numbackup, numcmds) = (payload[1], payload[2]);
        let mut r = proto::bitbuf::BitReader::new(&payload[3..]);
        // The chain is backups first, then the new commands. Only the new ones
        // are counted: the backups are re-sends of commands already in the
        // trace, and counting them would both duplicate the motion and make
        // the clock run several times too fast.
        let total = usize::from(numbackup) + usize::from(numcmds);
        let first_new = total.saturating_sub(usize::from(numcmds));
        for i in 0..total {
            let f = proto::delta::parse_delta(&mut r, &table);
            let g = |k: &str, d: f32| f.get(k).and_then(proto::delta::Value::as_f32).unwrap_or(d);
            cur = (
                f.get("msec")
                    .and_then(proto::delta::Value::as_i64)
                    .map(|v| v as f32)
                    .unwrap_or(cur.0),
                g("viewangles[0]", cur.1),
                g("viewangles[1]", cur.2),
            );
            if r.overflowed() {
                break;
            }
            if i >= first_new {
                trace.push(cur);
            }
        }
    }

    if trace.is_empty() {
        println!("no commands decoded from {path}");
        return;
    }

    if csv {
        println!("t,pitch,yaw");
        let mut t = 0.0f32;
        for (msec, pitch, yaw) in &trace {
            t += msec / 1000.0;
            println!("{t:.3},{pitch:.3},{yaw:.3}");
        }
        return;
    }

    let wrap = |d: f32| {
        let mut d = d % 360.0;
        if d > 180.0 {
            d -= 360.0;
        }
        if d < -180.0 {
            d += 360.0;
        }
        d
    };

    let mut seconds = 0.0f32;
    let mut dyaws: Vec<f32> = Vec::new();
    let mut dpitches: Vec<f32> = Vec::new();
    let mut reversals = 0u32;
    let mut still = 0u32;
    let mut sign = 0i32;
    let mut dwell = 0.0f32;
    let mut longest_dwell = 0.0f32;

    for w in trace.windows(2) {
        let (msec, pitch, yaw) = w[1];
        let dt = (msec / 1000.0).clamp(0.0, 0.25);
        seconds += dt;
        let dy = wrap(yaw - w[0].2);
        let dp = pitch - w[0].1;
        dyaws.push(dy.abs());
        dpitches.push(dp.abs());
        if dy.abs() < 0.05 && dp.abs() < 0.05 {
            still += 1;
        }
        // A dwell is a run with no meaningful yaw motion.
        if dy.abs() < 0.15 {
            dwell += dt;
            longest_dwell = longest_dwell.max(dwell);
        } else {
            dwell = 0.0;
        }
        let s = if dy > 0.02 {
            1
        } else if dy < -0.02 {
            -1
        } else {
            0
        };
        if s != 0 {
            if sign != 0 && s != sign {
                reversals += 1;
            }
            sign = s;
        }
    }

    let median = |v: &mut Vec<f32>| {
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    let mean = |v: &[f32]| {
        if v.is_empty() {
            0.0
        } else {
            v.iter().sum::<f32>() / v.len() as f32
        }
    };

    let mean_dyaw = mean(&dyaws);
    let mean_dpitch = mean(&dpitches);
    println!("commands           {}", trace.len());
    println!("duration           {seconds:.1} s");
    println!("reversals/s        {:.2}", reversals as f32 / seconds.max(0.001));
    println!("median |dyaw|      {:.3} deg", median(&mut dyaws));
    println!("mean   |dyaw|      {mean_dyaw:.3} deg");
    println!("median |dpitch|    {:.3} deg", median(&mut dpitches));
    println!("mean   |dpitch|    {mean_dpitch:.3} deg");
    println!(
        "still commands     {:.1}%",
        100.0 * still as f32 / trace.len() as f32
    );
    println!("longest dwell      {longest_dwell:.2} s");
}
