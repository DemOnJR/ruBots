//! Walk the records written by `capture_running` and report what they contain.
//!
//! ```text
//! cargo run -p client --example scan_capture -- running.bin
//! ```

use std::env;
use std::fs;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| "running.bin".into());
    let data = fs::read(&path).expect("read capture");

    let mut recs: Vec<&[u8]> = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + n > data.len() {
            break;
        }
        recs.push(&data[i..i + n]);
        i += n;
    }
    println!("{} records", recs.len());

    for (idx, r) in recs.iter().enumerate() {
        // Only the interesting (non-nop-padding) records.
        if r.iter().all(|&b| b == 1) {
            continue;
        }
        let s = client::walk_signon(r);
        println!(
            "record {idx}: {} bytes -> consumed {}, stopped_on {:?}, signon_num {:?}, \
             {} delta tables, {} resources, serverinfo {}",
            r.len(),
            s.stopped_at,
            s.stopped_on.map(|o| format!("{} ({})", o, client::svc::name(o))),
            s.signon_num,
            s.registry.len(),
            s.resources.len(),
            s.server_info.is_some()
        );
        // Show the leading bytes so an unknown opcode can be eyeballed.
        let head: Vec<String> = r.iter().take(24).map(|b| format!("{b:02x}")).collect();
        println!("   head: {}", head.join(" "));
        if let Some(op) = s.stopped_on {
            let at = s.stopped_at;
            let tail: Vec<String> = r[at..r.len().min(at + 24)]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            println!("   at stop ({op}): {}", tail.join(" "));
        }
    }
}
