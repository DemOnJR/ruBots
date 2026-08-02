//! Does `deadflag` ever reach us, and does it stay?
//!
//! `ClientData::alive()` reads `deadflag` out of the last decoded
//! `svc_clientdata` and treats a missing field as alive. Live, that produced
//! `alive true` in 214 of 214 samples while 68 of them showed `maxspeed 900`
//! and `health 1` — the observer state of a corpse. So either the field never
//! arrives, or it arrives once and is then dropped.
//!
//! This replays a capture and prints, for every record where the clientdata
//! decodes, whether `deadflag` was present and what it said, alongside the two
//! values that betray a dead player independently.
//!
//! ```text
//! cargo run -p client --example deadflag_probe -- captures/swarm/Bot03.bin [signon.bin]
//! ```

use std::env;

fn main() {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "captures/swarm/Bot03.bin".into());
    let data = std::fs::read(&path).expect("read capture");

    // The delta tables are per-server; take them from a signon of the SAME
    // server. The checked-in fixture is stock Valve HLDS and disagrees with
    // ReHLDS about field widths, so a wrong one here invents a wrong answer.
    let signon_path = env::args().nth(2);
    let signon_bytes = match &signon_path {
        Some(p) => std::fs::read(p).expect("read signon"),
        None => include_bytes!("../tests/fixtures/signon.bin").to_vec(),
    };
    let signon = client::walk_signon(&signon_bytes);
    eprintln!(
        "tables from {}: {} registered",
        signon_path.as_deref().unwrap_or("tests/fixtures/signon.bin (STOCK HLDS)"),
        signon.registry.len(),
    );

    let mut at = 0usize;
    let mut records = 0u32;
    let mut decoded = 0u32;
    let mut with_deadflag = 0u32;
    let mut observer_looking = 0u32;
    let mut observer_without_deadflag = 0u32;
    let mut printed = 0u32;

    while at + 4 <= data.len() {
        let len = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        if at + len > data.len() {
            break;
        }
        let msg = &data[at..at + len];
        at += len;
        records += 1;

        let Some(cd) = client::world::parse_datagram(msg, &signon.registry) else {
            continue;
        };
        decoded += 1;

        let flag = cd.fields.get("deadflag").and_then(proto::delta::Value::as_i64);
        if flag.is_some() {
            with_deadflag += 1;
        }
        // Independent of any field we might be misreading: a corpse in observer
        // mode. Nothing alive moves at 900.
        let looks_dead = cd.maxspeed() > 800.0 || cd.health() <= 1.0;
        if looks_dead {
            observer_looking += 1;
            if flag.is_none() {
                observer_without_deadflag += 1;
            }
        }

        if (looks_dead || flag.is_some()) && printed < 24 {
            printed += 1;
            println!(
                "rec {records:>5}  hp {:>4.0}  maxspeed {:>4.0}  weapons {:>2}  \
                 deadflag {:<6}  alive() {}",
                cd.health(),
                cd.maxspeed(),
                cd.weapons.len(),
                match flag {
                    Some(v) => v.to_string(),
                    None => "absent".into(),
                },
                cd.alive(),
            );
        }
    }

    println!("\n{records} records, {decoded} decoded as clientdata");
    println!("  deadflag present in ....... {with_deadflag}");
    println!("  look dead (900 or 1 hp) ... {observer_looking}");
    println!("  ...of those, no deadflag .. {observer_without_deadflag}");
    if observer_looking > 0 && observer_without_deadflag == observer_looking {
        println!(
            "\n!!! deadflag NEVER arrives while the player is plainly dead.\n\
             alive() cannot work from this field alone."
        );
    } else if observer_without_deadflag > 0 {
        println!(
            "\n!!! deadflag arrives sometimes but not on every frame -- so it is\n\
             transmitted on CHANGE only, and reading the last datagram in\n\
             isolation loses it. The field has to persist across frames."
        );
    }
}
