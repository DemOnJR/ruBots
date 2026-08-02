//! Walk a captured signon stream and report what is in it.
//!
//! ```text
//! cargo run -p client --example walk_signon -- path\to\signon.bin
//! ```
//!
//! Useful when the server's behaviour changes (different cvars, different
//! map): re-capture and re-walk rather than guessing what it sent.

fn main() {
    let path = match std::env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: walk_signon <signon.bin>");
            std::process::exit(2);
        }
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(1);
        }
    };

    let s = client::signon::walk(&data);

    println!("signon: {} bytes", data.len());
    println!(
        "parsed to byte {} ({:.1}%){}",
        s.stopped_at,
        100.0 * s.stopped_at as f64 / data.len() as f64,
        match s.stopped_on {
            Some(id) => format!("  halted on {} ({})", client::svc::name(id), id),
            None => "  (complete)".to_string(),
        }
    );

    match &s.server_info {
        Some(si) => println!(
            "server: {:?} map={} protocol={} maxplayers={} slot={}",
            si.hostname,
            si.map_name(),
            si.protocol,
            si.max_players,
            si.player_index
        ),
        None => println!("server: <no svc_serverinfo found>"),
    }

    println!("signon_num: {:?}", s.signon_num);
    if !s.resources.is_empty() {
        println!("resources: {}", s.resources.len());
        let checked = s.resources.iter().filter(|r| r.checksum.is_some()).count();
        println!("   consistency-checked: {checked}");
        for r in s.resources.iter().take(3) {
            println!("   {:<40} size={}", r.name, r.size);
        }
    }

    println!("delta tables: {}", s.registry.len());
    for name in [
        "event_t",
        "weapon_data_t",
        "usercmd_t",
        "custom_entity_state_t",
        "entity_state_player_t",
        "entity_state_t",
        "clientdata_t",
    ] {
        if let Some(t) = s.registry.get(name) {
            println!("   {name:<24} {} fields", t.len());
        }
    }

    // Second argument dumps one table in full — needed to hand-encode a delta.
    if let Some(want) = std::env::args().nth(2) {
        match s.registry.get(&want) {
            Some(t) => {
                println!("\n{want} ({} fields):", t.len());
                for (i, f) in t.iter().enumerate() {
                    println!(
                        "  {i:>2}  {:<22} type={:<12} bits={:<3} signed={} pre={} post={}",
                        f.name,
                        type_name(f.base_type()),
                        f.bits,
                        f.is_signed(),
                        f.premultiply,
                        f.postmultiply
                    );
                }
            }
            None => println!("\nno table named {want}"),
        }
    }
}

fn type_name(t: u32) -> &'static str {
    use proto::delta::*;
    match t {
        DT_BYTE => "byte",
        DT_SHORT => "short",
        DT_FLOAT => "float",
        DT_INTEGER => "integer",
        DT_ANGLE => "angle",
        DT_TIMEWINDOW_8 => "timewin8",
        DT_TIMEWINDOW_BIG => "timewinbig",
        DT_STRING => "string",
        _ => "unknown",
    }
}
