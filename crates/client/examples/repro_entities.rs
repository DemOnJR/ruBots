//! Replay a `capture_running` capture and report every `svc_packetentities`
//! block that fails to parse.
//!
//! ```text
//! cargo run -p client --example repro_entities -- captures/swarm/Bot02.bin
//! ```
//!
//! The walk mirrors `client::world::Decoder::feed` — the same stream walker, the
//! same bit-packed skips, the same `parse_packet_entities_full_checked` — with
//! one deliberate difference: the delta context is built explicitly instead of
//! by the since-removed `Decoder::absorb_baselines`, which scanned for a bare byte 22 and on a
//! running-phase capture (no `svc_spawnbaseline` in it at all) latches onto a
//! false positive. That matters here because the *count* of instanced baselines
//! gates a header bit; ReGameDLL creates none
//! (`regamedll/dlls/client.cpp:5247-5260` is empty either way), so the real
//! server never writes that bit and neither may we.
//!
//! The first failure is dumped in full: the [`proto::entity::EntityError`], a
//! header-by-header trace up to the entity it died on, and the raw bytes. That
//! trace is the whole point — the aggregate `errs=463` from a live run says
//! nothing about which of the eight failure modes fired, or on which entity.
//!
//! A second argument writes the failing block to a file, which is how
//! `crates/proto/tests/fixtures/` gets its inputs.

use std::collections::HashMap;
use std::env;
use std::fs;

use proto::bitbuf::BitReader;
use proto::delta::{parse_delta, DeltaRegistry, Value};
use proto::entity::{
    parse_delta_header, parse_packet_entities_full_checked, table_for, EntityState, PacketCtx,
};

use client::svc;

/// Split the `u32 len` + payload record stream.
fn records(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let n = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        if i + n > data.len() {
            break;
        }
        out.push(&data[i..i + n]);
        i += n;
    }
    out
}

/// `MSG_EndBitReading` — a bit block occupies `ceil(bits/8)` bytes, minimum one.
fn block_bytes(r: &BitReader<'_>) -> usize {
    (r.byte_pos() + usize::from(r.bit_offset() > 0)).max(1)
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Default)]
struct Stats {
    blocks: usize,
    ok: usize,
    errs: usize,
    /// Error variant → how many times, so a single dump is known to be typical.
    kinds: Vec<(String, usize)>,
    /// (record, offset, num_entities) of the first failure.
    first_bad: Option<(usize, usize, usize)>,
    max_entities: usize,
    /// (record, num_entities, outcome) for every block, in order.
    log: Vec<(usize, usize, String)>,
}

impl Stats {
    fn note(&mut self, kind: String) {
        match self.kinds.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => self.kinds.push((kind, 1)),
        }
    }
}

fn skip_clientdata(msg: &[u8], at: usize, registry: &DeltaRegistry) -> Option<usize> {
    let body = at + 1;
    let cd = registry.get("clientdata_t")?;
    let mut r = BitReader::new(msg.get(body..)?);
    if r.read_bits(1) != 0 {
        return None;
    }
    parse_delta(&mut r, cd);
    if let Some(wd) = registry.get("weapon_data_t") {
        let mut guard = 0;
        while r.read_bits(1) != 0 {
            r.skip(6);
            parse_delta(&mut r, wd);
            guard += 1;
            if guard > 64 || r.overflowed() {
                break;
            }
        }
    }
    if r.overflowed() {
        return None;
    }
    Some(body + block_bytes(&r))
}

fn skip_bit_packed(msg: &[u8], at: usize, registry: &DeltaRegistry) -> Option<usize> {
    let id = *msg.get(at)?;
    let body = at + 1;
    let mut r = BitReader::new(msg.get(body..)?);
    match id {
        svc::SVC_EVENT => {
            let count = r.read_bits(5);
            for _ in 0..count {
                r.skip(10);
                if r.read_bits(1) != 0 {
                    r.skip(11);
                    if r.read_bits(1) != 0 {
                        parse_delta(&mut r, registry.get("event_t")?);
                    }
                }
                if r.read_bits(1) != 0 {
                    r.skip(16);
                }
            }
        }
        svc::SVC_EVENT_RELIABLE => {
            r.skip(10);
            parse_delta(&mut r, registry.get("event_t")?);
            if r.read_bits(1) != 0 {
                r.skip(16);
            }
        }
        svc::SVC_PINGS => {
            let mut guard = 0;
            while r.read_bits(1) != 0 {
                r.skip(5 + 12 + 7);
                guard += 1;
                if guard > 64 || r.overflowed() {
                    return None;
                }
            }
        }
        svc::SVC_SOUND => {
            let mask = r.read_bits(9);
            if mask & 0x01 != 0 {
                r.skip(8);
            }
            if mask & 0x02 != 0 {
                r.skip(8);
            }
            r.skip(3);
            r.skip(11);
            r.skip(if mask & 0x04 != 0 { 16 } else { 8 });
            r.read_bit_vec3_coord();
            if mask & 0x08 != 0 {
                r.skip(8);
            }
        }
        _ => return None,
    }
    if r.overflowed() {
        return None;
    }
    Some(body + block_bytes(&r))
}

/// Walk one assembled stream exactly as `Decoder::feed` does, parsing every
/// `svc_packetentities` with the real parser.
fn replay_record(
    rec: usize,
    msg: &[u8],
    ctx: &PacketCtx,
    table: &client::UserMsgTable,
    st: &mut Stats,
) {
    let mut at = 0usize;
    loop {
        let w = client::walk_stream(&msg[at..], table);
        let Some(op) = w.stopped_on else { break };
        let stop = at + w.stopped_at;

        let next = match op {
            svc::SVC_CLIENTDATA => skip_clientdata(msg, stop, ctx.registry),
            svc::SVC_PACKETENTITIES => {
                if msg.len() < stop + 3 {
                    break;
                }
                let count = u16::from_le_bytes([msg[stop + 1], msg[stop + 2]]) as usize;
                let body = stop + 3;
                let mut r = BitReader::new(&msg[body..]);
                st.blocks += 1;
                st.max_entities = st.max_entities.max(count);
                match parse_packet_entities_full_checked(&mut r, ctx, count) {
                    Ok(v) => {
                        st.ok += 1;
                        st.log.push((rec, count, format!("ok ({} ents)", v.len())));
                        Some(body + block_bytes(&r))
                    }
                    Err(e) => {
                        st.errs += 1;
                        st.note(format!("{e:?}"));
                        st.log.push((rec, count, format!("{e:?}")));
                        if st.first_bad.is_none() {
                            st.first_bad = Some((rec, stop, count));
                        }
                        None
                    }
                }
            }
            _ => skip_bit_packed(msg, stop, ctx.registry),
        };
        match next {
            Some(n) if n > stop => at = n,
            _ => break,
        }
        if at >= msg.len() {
            break;
        }
    }
}

/// Header-by-header trace of a full packet-entities block, so the entity the
/// parser dies on is named rather than guessed at.
fn trace_full_block(
    block: &[u8],
    ctx: &PacketCtx,
    expected: usize,
) -> Option<(usize, usize, i32)> {
    let mut r = BitReader::new(block);
    let mut numbase = 0i32;
    let mut n = 0usize;
    let mut prev_numbase = 0i32;

    println!("    trace (header said {expected} entities):");
    loop {
        if r.overflowed() {
            println!("    #{n}: OVERFLOW before the header");
            return None;
        }
        if r.peek_bits(16) == 0 {
            println!(
                "    terminator after {n} entities, at bit {} of {}",
                r.byte_pos() * 8 + r.bit_offset() as usize,
                block.len() * 8
            );
            return None;
        }
        let bit0 = r.byte_pos() * 8 + r.bit_offset() as usize;
        let h = parse_delta_header(&mut r, &mut numbase, true, ctx.instanced.len());
        let which = table_for(h.number, h.custom, ctx.maxclients);
        let Some(tbl) = ctx.registry.get(which.name()) else {
            println!("    #{n}: entity {} wants missing table {}", h.number, which.name());
            return Some((bit0, n, prev_numbase));
        };
        let fields = parse_delta(&mut r, tbl);
        let bit1 = r.byte_pos() * 8 + r.bit_offset() as usize;
        println!(
            "    #{n:<3} bit {bit0:<5} ent {:<5} custom={} newbl={:?} off={:<3} {:?} {:>2} fields, {} bits{}",
            h.number,
            u8::from(h.custom),
            h.new_baseline,
            h.baseline_offset,
            which,
            fields.len(),
            bit1 - bit0,
            if r.overflowed() { "   <-- OVERFLOW" } else { "" }
        );
        let mut names: Vec<&String> = fields.keys().collect();
        names.sort();
        let shown: Vec<String> = names
            .iter()
            .take(16)
            .map(|k| match &fields[*k] {
                Value::Float(f) => format!("{k}={f:.1}"),
                Value::Int(i) => format!("{k}={i}"),
                Value::Str(s) => format!("{k}={s:?}"),
            })
            .collect();
        println!("          {}", shown.join(" "));
        if r.overflowed() {
            return Some((bit0, n, prev_numbase));
        }
        prev_numbase = numbase;
        n += 1;
        if n > 300 {
            println!("    ... giving up after 300 entities");
            return None;
        }
    }
}

/// Brute-force where the rest of the block really starts.
///
/// Given a block that dies part way through, try resuming the entity walk at
/// every bit offset in a window and report which ones parse cleanly to the
/// terminator with the expected number of entities left. The bit distance from
/// where the parser thought it was is exactly how many bits it under- or
/// over-read.
fn search_resume(block: &[u8], ctx: &PacketCtx, from_bit: usize, want_left: usize, numbase0: i32) {
    println!(
        "\n    searching for the true resume point (parser stopped at bit {from_bit}, \
         {want_left} entities left, block is {} bits):",
        block.len() * 8
    );
    let lo = from_bit.saturating_sub(48);
    let hi = (from_bit + 64).min(block.len() * 8);
    for b in lo..hi {
        let mut r = BitReader::new(block);
        r.skip(b as u32);
        let mut numbase = numbase0;
        let mut n = 0usize;
        let mut nums = Vec::new();
        let ok = loop {
            if r.overflowed() {
                break false;
            }
            if r.peek_bits(16) == 0 {
                r.skip(16);
                break n == want_left && !r.overflowed();
            }
            if n > want_left {
                break false;
            }
            let h = parse_delta_header(&mut r, &mut numbase, true, ctx.instanced.len());
            if h.number >= 2048 {
                break false;
            }
            let which = table_for(h.number, h.custom, ctx.maxclients);
            let Some(t) = ctx.registry.get(which.name()) else {
                break false;
            };
            parse_delta(&mut r, t);
            if r.overflowed() {
                break false;
            }
            nums.push((h.number, h.custom, h.baseline_offset));
            n += 1;
        };
        if ok {
            let end = r.byte_pos() * 8 + r.bit_offset() as usize;
            println!(
                "      bit {b:<4} ({:+}) parses {n} entities {nums:?}, ends at bit {end} of {}",
                b as i64 - from_bit as i64,
                block.len() * 8
            );
        }
    }
}

/// `CAPTURE_SIGNON=<addr>` — connect to a live server, walk the signon, and
/// write the raw signon stream to the path given as the first argument.
///
/// This exists because the delta tables come from the server's `delta.lst`, and
/// `crates/client/tests/fixtures/signon.bin` was taken from **stock Valve
/// HLDS**, whose `cstrike/delta.lst` differs from the ReHLDS/ReGameDLL bundle
/// the test server actually runs (`testserver/Dockerfile:57` copies
/// `testserver/rehlds/` over the Valve install). Player `origin[0..2]` is 18
/// bits in one and 24 in the other, so replaying a ReHLDS capture against the
/// stock tables desynchronises on the first remote player — a fault in the
/// harness, not in the parser.
fn capture_signon(addr: &str, out: &str) {
    use std::time::Duration;
    let sock = match client::UdpTransport::connect(addr.parse().expect("addr"), None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect failed: {e}");
            return;
        }
    };
    let mut t = sock;
    let mut session = client::Session::named("SignonProbe");
    session.record_all = true;
    match session.connect_and_signon(&mut t, Duration::from_secs(15)) {
        Ok(s) => eprintln!(
            "signon: {} tables, map {}",
            s.registry.len(),
            s.server_info
                .as_ref()
                .map(|i| i.map_name().to_string())
                .unwrap_or_default()
        ),
        Err(e) => {
            eprintln!("signon failed: {e}");
            return;
        }
    }
    let Some(msg) = session
        .recorded
        .iter()
        .find(|m| client::walk_signon(m).registry.get("usercmd_t").is_some())
    else {
        eprintln!("no signon message recorded");
        return;
    };
    fs::write(out, msg).expect("write signon");
    eprintln!("wrote {} bytes to {out}", msg.len());
}

fn main() {
    if let Ok(addr) = env::var("CAPTURE_SIGNON") {
        let out = env::args().nth(1).unwrap_or_else(|| "signon.bin".into());
        capture_signon(&addr, &out);
        return;
    }
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "captures/swarm/Bot02.bin".into());
    let dump_to = args.next();
    let data = fs::read(&path).expect("read capture");
    let recs = records(&data);
    eprintln!("{}: {} records", path, recs.len());

    // `capture_running` only starts recording once the signon is complete, so a
    // capture normally carries no delta tables. They are per-server, not
    // per-session (they come from `cstrike/delta.lst`), so the checked-in signon
    // from the same test server is an exact stand-in — same 7 tables, same
    // `maxplayers 12` (`testserver/Dockerfile:114`).
    let mut signon = None;
    for r in recs.iter() {
        let s = client::walk_signon(r);
        if s.registry.get("usercmd_t").is_some() {
            signon = Some(s);
            break;
        }
    }
    let signon = signon.unwrap_or_else(|| {
        // SIGNON=<path> supplies the delta tables when the capture has none.
        // They must come from the SAME server build: `tests/fixtures/signon.bin`
        // is from stock Valve HLDS and its `entity_state_player_t.origin[*]` is
        // 18 bits, while the ReHLDS/ReGameDLL bundle the test server runs
        // (`testserver/Dockerfile:57`) says 24. Replaying with the wrong one
        // desynchronises on the first remote player.
        let sibling = std::path::Path::new(&path).with_file_name("signon.bin");
        let chosen = env::var("SIGNON").ok().unwrap_or_else(|| {
            sibling.to_string_lossy().into_owned()
        });
        match fs::read(&chosen) {
            Ok(b) => {
                eprintln!("no signon in this capture; using {chosen}");
                client::walk_signon(&b)
            }
            Err(_) => {
                eprintln!("no signon in this capture, and no {chosen}");
                eprintln!("  falling back to tests/fixtures/signon.bin, which is from STOCK");
                eprintln!("  Valve HLDS -- its player origin[*] is 18 bits, ReHLDS says 24, so a");
                eprintln!("  ReHLDS capture will desync on the first remote player. Regenerate the");
                eprintln!("  right one with:");
                eprintln!("    CAPTURE_SIGNON=127.0.0.1:27015 cargo run -p client \\");
                eprintln!("        --example repro_entities -- {}", sibling.display());
                client::walk_signon(include_bytes!("../tests/fixtures/signon.bin"))
            }
        }
    });
    if signon.registry.get("usercmd_t").is_none() {
        eprintln!("no delta tables anywhere -- cannot decode");
        return;
    }
    let maxclients = signon
        .server_info
        .as_ref()
        .map(|s| s.max_players)
        .unwrap_or(12);
    eprintln!(
        "{} delta tables, maxclients {maxclients}",
        signon.registry.len()
    );

    // PATCH=<table>:<field>:<bits>,... overrides widths in the learned registry,
    // so a hypothesis about the true wire width can be tested over the whole
    // capture instead of one block.
    let mut signon = signon;
    if let Ok(spec) = env::var("PATCH") {
        for one in spec.split(',') {
            let parts: Vec<&str> = one.split(':').collect();
            let [tbl, field, bits] = parts[..] else { continue };
            let bits: u32 = bits.parse().unwrap();
            let mut t = signon.registry.get(tbl).expect("table").clone();
            for f in t.iter_mut() {
                if f.name == field || (field.ends_with('*') && f.name.starts_with(field.trim_end_matches('*'))) {
                    eprintln!("patch {tbl}.{} {} -> {bits} bits", f.name, f.bits);
                    f.bits = bits;
                }
            }
            signon.registry.register(tbl, t);
        }
    }

    if env::var("DUMP_TABLES").is_ok() {
        for name in ["entity_state_t", "entity_state_player_t", "custom_entity_state_t"] {
            let Some(t) = signon.registry.get(name) else { continue };
            println!("{name}: {} fields", t.len());
            for (i, f) in t.iter().enumerate() {
                println!(
                    "  {i:>2} {:<18} type {:#x} base {:>3} signed {} bits {:>2} pre {} post {}",
                    f.name,
                    f.field_type,
                    f.base_type(),
                    u8::from(f.is_signed()),
                    f.bits,
                    f.premultiply,
                    f.postmultiply
                );
            }
        }
        return;
    }

    let owned: Vec<Vec<u8>> = recs.iter().map(|r| r.to_vec()).collect();
    let table = client::collect_user_messages(&owned);

    let baselines: HashMap<u16, EntityState> = HashMap::new();
    let instanced: Vec<EntityState> = Vec::new();
    let ctx = PacketCtx {
        registry: &signon.registry,
        baselines: &baselines,
        instanced: &instanced,
        maxclients,
    };

    // BASELINE_AT=<record> runs `parse_spawn_baseline` at the first byte 22 in
    // that record, which is exactly what the since-removed `Decoder::absorb_baselines` did. On a
    // record that is not a `svc_spawnbaseline` at all it should fail; if it
    // succeeds, the decoder has just replaced its baselines with garbage.
    if let Ok(v) = env::var("BASELINE_AT") {
        // "signon" probes the real `svc_spawnbaseline`, which `SV_CreateBaseline`
        // writes into `g_psv.signon` (`sv_main.cpp:5890`) — i.e. it arrives in
        // the signon burst, not in the running stream.
        let signon_bytes;
        let msg: &[u8] = if v == "signon" {
            signon_bytes = fs::read(env::var("SIGNON").expect("SIGNON")).expect("read");
            &signon_bytes
        } else {
            recs[v.parse::<usize>().expect("record number")]
        };
        let rec = v.clone();
        // In the signon the real position is where the byte walk halts; in a
        // running record there is no real one, so take the first byte 22 —
        // which is exactly the guess the since-removed `Decoder::absorb_baselines` made.
        let p = if v == "signon" {
            let w = client::walk_signon(msg);
            assert_eq!(w.stopped_on, Some(22), "signon does not halt on 22");
            w.stopped_at
        } else {
            msg.iter().position(|&b| b == 22).expect("no byte 22")
        };
        println!("record {rec}: {} bytes, first byte 22 at {p}", msg.len());
        let body = &msg[p + 1..];

        // Re-walk the block in wire order, since `Baselines::by_number` is a
        // HashMap and hides both the order and any duplicates.
        {
            let mut rr = BitReader::new(body);
            let mut seq = Vec::new();
            while !rr.overflowed() && rr.peek_bits(16) != 0xFFFF && seq.len() < 300 {
                let n = rr.read_bits(11) as u16;
                let ty = rr.read_bits(2) as u8;
                let custom = ty & 1 == 0;
                let which = table_for(n, custom, maxclients);
                let Some(t) = signon.registry.get(which.name()) else { break };
                parse_delta(&mut rr, t);
                seq.push((n, ty));
            }
            println!("  wire order: {seq:?}");
        }

        let mut r = BitReader::new(body);
        let out =
            proto::entity::parse_spawn_baseline(&mut r, &signon.registry, maxclients);
        match &out {
            Ok(b) => {
                let mut nums: Vec<u16> = b.by_number.keys().copied().collect();
                nums.sort();
                println!(
                    "  Ok: {} by number {nums:?}, {} instanced; consumed {} bytes of {}",
                    b.by_number.len(),
                    b.instanced.len(),
                    block_bytes(&r),
                    body.len()
                );
            }
            Err(e) => println!("  Err({e:?})"),
        }
        let n = block_bytes(&r).min(body.len());
        println!("\n  bytes from the opcode ({} incl. opcode):", n + 1);
        for (i, chunk) in msg[p..p + 1 + n].chunks(16).enumerate() {
            println!("    {:04x}  {}", i * 16, hex(chunk));
        }
        if let Some(p2) = dump_to {
            // The whole tail, not just what this registry happened to consume:
            // how far the parse runs depends on the delta tables, and the
            // fixture must not.
            fs::write(&p2, body).expect("write");
            println!("\n  wrote {} bytes (after the opcode) to {p2}", body.len());
        }
        return;
    }

    // DECODER=1 drives the real `client::world::Decoder` instead, which is what
    // the live bot runs. The difference between the two was `absorb_baselines`, now removed in favour of the stream walker.
    if env::var("DECODER").is_ok() {
        let mut d = client::world::Decoder::new(&signon, table.clone());
        let mut announced = false;
        for (i, r) in recs.iter().enumerate() {
            let before = (d.stats.entity_errors, d.baselines.instanced.len());
            d.feed(r);
            let after = (d.stats.entity_errors, d.baselines.instanced.len());
            if before.1 != after.1 {
                println!(
                    "record {i}: baselines replaced -- {} by number, {} instanced",
                    d.baselines.by_number.len(),
                    after.1
                );
            }
            if after.0 > before.0 && !announced {
                println!(
                    "record {i}: FIRST entity error {:?} (instanced = {})",
                    d.stats.last_entity_error, after.1
                );
                announced = true;
            }
        }
        println!(
            "\nDecoder: ok={} ents={} nocd={} partial={} errs={} stop={:?} last={:?}",
            d.stats.ok,
            d.stats.with_entities,
            d.stats.no_clientdata,
            d.stats.partial,
            d.stats.entity_errors,
            d.stats.last_stop,
            d.stats.last_entity_error
        );
        println!(
            "final baselines: {} by number, {} instanced",
            d.baselines.by_number.len(),
            d.baselines.instanced.len()
        );
        return;
    }

    let mut st = Stats::default();
    for (i, r) in recs.iter().enumerate() {
        replay_record(i, r, &ctx, &table, &mut st);
    }

    println!(
        "\n{} packetentities blocks: {} ok, {} failed (largest header count {})",
        st.blocks, st.ok, st.errs, st.max_entities
    );
    let mut kinds = st.kinds.clone();
    kinds.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (k, n) in kinds.iter().take(12) {
        println!("   {n:>5}  {k}");
    }
    if env::var("VERBOSE").is_ok() {
        let skip: usize = env::var("SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        println!("\nper-block outcomes (from {skip}):");
        for (rec, count, what) in st.log.iter().skip(skip).take(60) {
            println!("   rec {rec:<5} n={count:<3} {what}");
        }
    }

    // TRACE=<record> traces that record's block instead of the first failure,
    // which is how a working block is put side by side with a broken one.
    let forced: Option<usize> = env::var("TRACE").ok().and_then(|v| v.parse().ok());
    if let Some(want) = forced {
        let msg = recs[want];
        // Re-walk just to locate the block offset.
        let mut at = 0usize;
        let mut off = None;
        loop {
            let w = client::walk_stream(&msg[at..], &table);
            let Some(op) = w.stopped_on else { break };
            let stop = at + w.stopped_at;
            if op == svc::SVC_PACKETENTITIES {
                off = Some(stop);
                break;
            }
            let next = if op == svc::SVC_CLIENTDATA {
                skip_clientdata(msg, stop, ctx.registry)
            } else {
                skip_bit_packed(msg, stop, ctx.registry)
            };
            match next {
                Some(n) if n > stop => at = n,
                _ => break,
            }
        }
        if let Some(off) = off {
            let count = u16::from_le_bytes([msg[off + 1], msg[off + 2]]) as usize;
            println!("\nrecord {want}, offset {off}, num_entities {count}");
            let block = &msg[off + 3..];
            if let Some((bit, n, nb)) = trace_full_block(block, &ctx, count) {
                search_resume(block, &ctx, bit, count - n, nb);
            }
            let show = block.len().min(256);
            println!("\n  block bytes ({} shown of {}):", show, block.len());
            for (i, chunk) in block[..show].chunks(16).enumerate() {
                println!("    {:04x}  {}", i * 16, hex(chunk));
            }
        } else {
            println!("\nrecord {want} has no packetentities");
        }
        return;
    }

    // SURVEY=1 runs the resume search on every failure and reports how many
    // bits the parser was off by, which is what separates "one field is the
    // wrong width" from "the framing is wrong".
    if env::var("SURVEY").is_ok() {
        let mut hist: Vec<(i64, usize)> = Vec::new();
        let mut examined = 0usize;
        for (rec, _r) in recs.iter().enumerate() {
            if examined >= 400 {
                break;
            }
            let msg = recs[rec];
            let mut at = 0usize;
            loop {
                let w = client::walk_stream(&msg[at..], &table);
                let Some(op) = w.stopped_on else { break };
                let stop = at + w.stopped_at;
                if op == svc::SVC_PACKETENTITIES && msg.len() > stop + 3 {
                    let count = u16::from_le_bytes([msg[stop + 1], msg[stop + 2]]) as usize;
                    let block = &msg[stop + 3..];
                    let mut r = BitReader::new(block);
                    if parse_packet_entities_full_checked(&mut r, &ctx, count).is_err() {
                        examined += 1;
                        // Re-walk quietly to find where it broke.
                        let mut rr = BitReader::new(block);
                        let mut nb = 0i32;
                        let mut prev_nb = 0i32;
                        let mut n = 0usize;
                        let (bit, ok_n, base) = loop {
                            let b0 = rr.byte_pos() * 8 + rr.bit_offset() as usize;
                            if rr.overflowed() || rr.peek_bits(16) == 0 {
                                break (b0, n, prev_nb);
                            }
                            let h = parse_delta_header(&mut rr, &mut nb, true, ctx.instanced.len());
                            let which = table_for(h.number, h.custom, ctx.maxclients);
                            let Some(t) = ctx.registry.get(which.name()) else {
                                break (b0, n, prev_nb);
                            };
                            parse_delta(&mut rr, t);
                            if rr.overflowed() || h.number >= 2048 {
                                break (b0, n, prev_nb);
                            }
                            prev_nb = nb;
                            n += 1;
                            if n > 300 {
                                break (b0, n, prev_nb);
                            }
                        };
                        // Find the true resume bit.
                        let want = count.saturating_sub(ok_n);
                        let lo = bit.saturating_sub(64);
                        let hi = (bit + 96).min(block.len() * 8);
                        let mut found = None;
                        for b in lo..hi {
                            let mut r2 = BitReader::new(block);
                            r2.skip(b as u32);
                            let mut nb2 = base;
                            let mut m = 0usize;
                            let good = loop {
                                if r2.overflowed() {
                                    break false;
                                }
                                if r2.peek_bits(16) == 0 {
                                    r2.skip(16);
                                    break m == want && !r2.overflowed();
                                }
                                if m > want {
                                    break false;
                                }
                                let h =
                                    parse_delta_header(&mut r2, &mut nb2, true, ctx.instanced.len());
                                if h.number >= 2048 {
                                    break false;
                                }
                                let which = table_for(h.number, h.custom, ctx.maxclients);
                                let Some(t) = ctx.registry.get(which.name()) else {
                                    break false;
                                };
                                parse_delta(&mut r2, t);
                                if r2.overflowed() {
                                    break false;
                                }
                                m += 1;
                            };
                            if good {
                                found = Some(b as i64 - bit as i64);
                                break;
                            }
                        }
                        let key = found.unwrap_or(i64::MIN);
                        match hist.iter_mut().find(|(k, _)| *k == key) {
                            Some((_, c)) => *c += 1,
                            None => hist.push((key, 1)),
                        }
                        break;
                    }
                }
                let next = if op == svc::SVC_CLIENTDATA {
                    skip_clientdata(msg, stop, ctx.registry)
                } else if op == svc::SVC_PACKETENTITIES {
                    let count = u16::from_le_bytes([msg[stop + 1], msg[stop + 2]]) as usize;
                    let body = stop + 3;
                    let mut r = BitReader::new(&msg[body..]);
                    parse_packet_entities_full_checked(&mut r, &ctx, count)
                        .ok()
                        .map(|_| body + block_bytes(&r))
                } else {
                    skip_bit_packed(msg, stop, ctx.registry)
                };
                match next {
                    Some(n) if n > stop => at = n,
                    _ => break,
                }
            }
        }
        hist.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        println!("\nresume-offset histogram over {examined} failures:");
        for (k, c) in hist.iter().take(20) {
            if *k == i64::MIN {
                println!("   {c:>5}  no resume point found in the window");
            } else {
                println!("   {c:>5}  {k:+} bits");
            }
        }
        return;
    }

    let Some((rec, off, count)) = st.first_bad else {
        println!("\nno packetentities failures");
        return;
    };

    let msg = recs[rec];
    let block = &msg[off + 3..];
    println!("\nfirst failing block: record {rec}, offset {off}, num_entities {count}");
    if let Some((bit, n, nb)) = trace_full_block(block, &ctx, count) {
        search_resume(block, &ctx, bit, count - n, nb);
    }

    let show = block.len().min(256);
    println!("\n  block bytes ({} shown of {}):", show, block.len());
    for (i, chunk) in block[..show].chunks(16).enumerate() {
        println!("    {:04x}  {}", i * 16, hex(chunk));
    }

    if let Some(p) = dump_to {
        fs::write(&p, block).expect("write dump");
        println!("\n  wrote {} bytes to {p}", block.len());
    }
}
