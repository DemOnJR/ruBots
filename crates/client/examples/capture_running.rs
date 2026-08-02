//! Connect to a live server, reach the signon, then record the running-phase
//! message stream to a file for offline analysis.
//!
//! This exists because the entity-update bit formats cannot be read reliably
//! out of the disassembly (the BitReader calls are inlined) — the only sound
//! way to get them is to look at what a real server actually sends.
//!
//! ```text
//! cargo run -p client --example capture_running -- [addr] [seconds] [out]
//! ```
//!
//! Output format: repeated `u32 length` + `length` bytes, each record being one
//! fully-assembled, decompressed `svc_*` message stream.

use std::env;
use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use client::{Identity, Session, Transport};

/// Wraps a transport and records every datagram sent, so our own wire bytes
/// can be diffed against a real client's capture.
struct Logged<T: Transport> {
    inner: T,
    log: File,
}

impl<T: Transport> Transport for Logged<T> {
    fn send(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.log.write_all(&(data.len() as u32).to_le_bytes())?;
        self.log.write_all(data)?;
        self.inner.send(data)
    }
    fn recv(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.recv()
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:27015".into());
    let secs: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let out_path = args.next().unwrap_or_else(|| "running.bin".into());

    let inner = match client::UdpTransport::connect(addr.parse().expect("addr"), None) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        }
    };
    let sent_path = format!("{out_path}.sent");
    let mut t = Logged { inner, log: File::create(&sent_path).expect("sent log") };
    eprintln!("logging our outgoing packets to {sent_path}");

    // Distinct name AND key per bot: Reunion's IDClientsLimit is 1, so two
    // bots sharing a CD key are one identity and the second is refused.
    let name = env::var("AIPLAYERS_NAME").unwrap_or_else(|_| "AIPlayer".into());
    let key = env::var("AIPLAYERS_KEY").unwrap_or_else(|_| "AIPLAYER0000000".into());
    let mut session = Session::new(Identity {
        name: name.clone(),
        key: key.into_bytes(),
        ..Default::default()
    });
    session.record_all = true;
    match session.connect_and_signon(&mut t, Duration::from_secs(15)) {
        Ok(signon) => eprintln!(
            "signon reached: {} delta tables, map {}",
            signon.registry.len(),
            signon
                .server_info
                .as_ref()
                .map(|si| si.map_name().to_string())
                .unwrap_or_default()
        ),
        Err(e) => {
            eprintln!("signon failed: {e}");
            std::process::exit(1);
        }
    }

    // The exact post-signon command sequence a real CS 1.6 client sends,
    // recovered by decoding a genuine client's session. Note there is NO
    // `spawn` and NO `begin` - protocol 48 does not use them.
    // The real client asks for the resource list FIRST (~0.3s after `new`),
    // then answers the allow_* stufftexts, and only reaches `sendents` ~1.4s
    // later after uploading its consistency data.
    // Clean entry sequence, with nothing queued ahead of it: a stuck reliable
    // would block `sendents`, and `sendents` is the one command that makes the
    // server consider us fully connected.
    session.send_command(Session::SENDRES);
    let res_until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < res_until {
        let _ = session.pump(&mut t, &[netchan::clc::NOP]);
    }

    eprintln!("  uploading clc_resourcelist (fragmented)");
    session.upload_resource_list();
    let up_until = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < up_until {
        let _ = session.pump(&mut t, &[netchan::clc::NOP]);
    }

    let spawncount: u32 = env::var("AIPLAYERS_SPAWNCOUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| session.resource_message.as_ref().map(|r| r.spawncount))
        .or_else(|| session.recorded.iter().find_map(|m| Session::spawncount_from(m)))
        .unwrap_or(1);
    match session.resource_message.as_ref() {
        Some(rm) => {
            eprintln!(
                "  resource message: {} resources, spawncount {}, consistency {} ({} demands)",
                rm.resources.len(),
                rm.spawncount,
                if rm.consistency.should_send { "REQUESTED" } else { "not requested" },
                rm.consistency.indices.len(),
            );
            if rm.consistency.should_send {
                let demands =
                    proto::consistency::demands(&rm.resources, &rm.consistency, rm.spawncount);
                let exact = demands
                    .iter()
                    .filter(|d| matches!(d, proto::consistency::Demand::ExactFile { .. }))
                    .count();
                eprintln!(
                    "     {} bounds (answerable from the wire), {exact} exact-file (need local content)",
                    demands.len() - exact,
                );
                for d in demands
                    .iter()
                    .filter(|d| matches!(d, proto::consistency::Demand::ExactFile { .. }))
                    .take(12)
                {
                    eprintln!("       exact-file: {}", d.path());
                }
            }
        }
        None => eprintln!("  !!! no svc_resourcerequest seen -- sendres was not answered"),
    }
    if let Some(crc) = session.world_map_crc() {
        // Cross-check this against the server's own log line:
        //   Started map "<name>" (CRC "<n>")
        eprintln!(
            "  server map CRC: {crc}   (spawn argument: {})",
            session.spawn_crc(spawncount)
        );
    }
    session.start_decoding();
    // Give the bot a brain unless we are capturing raw protocol.
    if env::var("AIPLAYERS_NO_BRAIN").is_err() {
        session.brain = Some(bot::Controller::new(0xA1F0, bot::Difficulty::Normal));
        eprintln!("  bot brain enabled");
    }
    session.load_map(0);
    match session.map.as_ref() {
        Some(m) => eprintln!(
            "  map {} loaded: {} nav nodes, {} bomb sites, {} rescue zones",
            m.name, m.grid.len(), m.info.bomb_sites.len(), m.info.rescue_zones.len()
        ),
        None => eprintln!("  no map loaded -- the bot will not path"),
    }
    eprintln!("  entering game: spawn {spawncount} then sendents ...");
    match session.enter_game(&mut t, spawncount, Duration::from_secs(10)) {
        Ok(true) => eprintln!("  *** SERVER IS STREAMING - we are fully connected ***"),
        Ok(false) => eprintln!("  !!! server never started streaming"),
        Err(e) => eprintln!("  enter_game error: {e}"),
    }

    if env::var("AIPLAYERS_NO_JOIN").is_ok() {
        eprintln!("  BISECT: fully connected, sending nothing but moves");
    } else {
        // Let the entry burst drain before adding the join burst on top of it.
        // The server copies the WHOLE of `netchan.message` into `reliable_buf`
        // in one go and only when the previous reliable was acknowledged, so
        // anything the game queues meanwhile piles up in that one buffer. A
        // real client waits ~1.5 s between `sendents` (1.67 s) and `jointeam`
        // (3.17 s); firing them back to back stacks two bursts and overflows.
        let settle = Instant::now()
            + Duration::from_millis(
                env::var("AIPLAYERS_JOIN_DELAY_MS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(2000),
            );
        while Instant::now() < settle {
            let _ = session.pump(&mut t, &[netchan::clc::NOP]);
        }
        eprintln!("  joining team, retrying until the server actually spawns us");
        let team: u8 = env::var("AIPLAYERS_TEAM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(Session::TEAM_TERRORIST);
        match session.join_and_spawn(&mut t, team, Duration::from_secs(15)) {
            Ok(true) => {
                eprintln!("  *** TEAM ACCEPTED -- joined ***");
                session.refresh_objective(0);
                if let Some(site) = session.site {
                    eprintln!("  objective: [{:.0} {:.0} {:.0}]", site[0], site[1], site[2]);
                }
            }
            Ok(false) => eprintln!("  !!! team was never accepted"),
            Err(e) => eprintln!("  join error: {e}"),
        }
    }

    let mut f = File::create(&out_path).expect("create output");
    let mut records = 0usize;
    let mut bytes = 0usize;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last_keep = Instant::now();

    let mut settled_logged = false;
    let start = Instant::now();
    // Walk a square, turning every 3 s. Movement has to be *observable* to be
    // verifiable, and `svc_clientdata` gives us the server's own opinion of
    // where we ended up -- which is the only opinion that counts.
    let mut first_origin: Option<[f32; 3]> = None;
    let mut max_travel = 0.0f32;
    let mut max_speed = 0.0f32;
    let mut last_state = Instant::now();
    while Instant::now() < deadline {
        // Pump continuously: every pump both consumes what arrived and sends
        // our acknowledgement, which is what keeps the server's reliable
        // buffer draining.
        // Send a real clc_move every tick, exactly as a playing client does.
        let secs = start.elapsed().as_secs_f32();
        let intent = bot::Intent {
            view: bot::Angles {
                pitch: 0.0,
                // A slow constant yaw sweep also satisfies ReGameDLL's
                // anti-idle check, which needs BOTH yaw and pitch to move by
                // >= 0.1 degrees across a 5 s sample (CSPlayer.cpp:530-540).
                yaw: (secs * 24.0) % 360.0,
                },
            forwardmove: 250.0,
            ..Default::default()
        };
        let step = if env::var("AIPLAYERS_NO_MOVES").is_ok() {
            session.pump(&mut t, &[netchan::clc::NOP])
        } else {
            // Real-time paced: `frame` blocks until the next command is due, so
            // the msec we claim tracks the wall clock. Sending a fixed msec from
            // a tight loop is what got every movement command discarded by
            // ReHLDS's speedhack accounting.
            session.frame(&mut t, &intent)
        };
        if let Some(cd) = session.clientdata.as_ref() {
            let o = cd.origin();
            let base = *first_origin.get_or_insert(o);
            let d = ((o[0] - base[0]).powi(2) + (o[1] - base[1]).powi(2)).sqrt();
            max_travel = max_travel.max(d);
            max_speed = max_speed.max(cd.speed());
            if last_state.elapsed() >= Duration::from_secs(2) {
                eprintln!(
                    "  t+{:>4.0}s origin [{:>6.0} {:>6.0} {:>5.0}] vel {:>5.0} hp {:>3.0} \
                     maxspeed {:>4.0} alive {} weapons {}",
                    secs,
                    o[0],
                    o[1],
                    o[2],
                    cd.speed(),
                    cd.health(),
                    cd.maxspeed(),
                    cd.alive(),
                    cd.weapons.len(),
                );
                // The server tags a dead player's chat "(dead)" in the log
                // (`util.cpp` Host_Say). That is a direct, one-bit answer to
                // "is this bot actually alive?" -- unlike maxspeed or
                // ResetHUD, both of which lie.
                if std::env::var("AIPLAYERS_ALIVE_PROBE").is_ok() {
                    session.console.say(format!("probe{}", (secs as u32) / 4));
                }
                let queued = session.console.len();
                if let Some(dec) = session.last_decision {
                    eprintln!(
                        "      brain: alive {} frozen {} fwd {:.0} side {:.0} yaw {:.0} site {:?} wp {} reroutes {}",
                        dec.alive, dec.in_game, dec.forwardmove, dec.sidemove, dec.yaw,
                        dec.site.map(|s| [s[0] as i32, s[1] as i32]),
                        dec.waypoints_left, dec.reroutes,
                    );
                }
                if let Some(d) = session.decoder.as_ref() {
                    eprintln!(
                        "      game: team {:?} money ${} hp {} weapon {} clip {} buyzone {} round {}s resets {} queued {}",
                        d.game.my_team(), d.game.money, d.game.health,
                        d.game.weapon_id, d.game.weapon_clip, d.game.in_buy_zone,
                        d.game.round_time, d.game.hud_resets, queued,
                    );
                    let players = d.players();
                    eprintln!(
                        "      world: {} entities, {} players | ok={} ents={} nocd={} tail={} errs={} stop={:?}",
                        d.entities.len(), players.len(),
                        d.stats.ok, d.stats.with_entities, d.stats.no_clientdata, d.stats.partial,
                        d.stats.entity_errors, d.stats.last_stop,
                    );
                    for p in players.iter().take(4) {
                        eprintln!(
                            "        player #{} {:?} at [{:.0} {:.0} {:.0}] yaw {:.0}{}",
                            p.entity, p.team, p.origin[0], p.origin[1], p.origin[2],
                            p.angles[1], if p.ducking { " (ducking)" } else { "" },
                        );
                    }
                }
                if std::env::var("AIPLAYERS_FIELDS").is_ok() {
                    let mut k: Vec<&str> = cd.fields.keys().map(|s| s.as_str()).collect();
                    k.sort_unstable();
                    eprintln!("      fields({}): {}", k.len(), k.join(" "));
                }
                last_state = Instant::now();
            }
        }
        match step {
            Ok(msgs) => {
                for msg in msgs {
                    f.write_all(&(msg.len() as u32).to_le_bytes()).unwrap();
                    f.write_all(&msg).unwrap();
                    records += 1;
                    bytes += msg.len();
                }
            }
            Err(e) => {
                eprintln!("pump error: {e}");
                break;
            }
        }
        if !settled_logged && session.reliables_settled() {
            eprintln!("all reliable commands acknowledged by the server");
            settled_logged = true;
        }
        // Channel telemetry: is our acknowledgement actually advancing?
        if std::env::var("AIPLAYERS_TRACE").is_ok()
            && last_keep.elapsed() >= Duration::from_millis(500)
        {
            eprintln!(
                "  t+{:>5}ms  in_seq={:<6} out_seq={:<6} in_rel={} out_rel={} \
                 rel_inflight={} queued={} records={} stale={}
            dgram={} split={}/{} frag={}/{} plain={} rejected={} resyncs={} LOST={}",
                start.elapsed().as_millis(),
                session.chan.incoming_sequence,
                session.chan.outgoing_sequence,
                session.chan.incoming_reliable,
                session.chan.outgoing_reliable,
                session.chan.reliable_in_flight(),
                session.chan.queued_count(),
                records,
                session.chan.dropped_stale,
                session.stats.datagrams,
                session.stats.split_completed,
                session.stats.split_seen,
                session.stats.frag_completed,
                session.stats.frag_seen,
                session.stats.plain,
                session.stats.read_rejected,
                session.resyncs(),
                session.chan.lost_packets,
            );
            last_keep = Instant::now();
        }
    }

    // Exact decode of every message, using the registered user-message sizes.
    let table = client::collect_user_messages(&session.recorded);
    eprintln!("  user messages registered: {}", table.len());
    let mut totals: std::collections::BTreeMap<String, usize> = Default::default();
    let mut halted: std::collections::BTreeMap<u8, usize> = Default::default();
    for msg in &session.recorded {
        let w = client::walk_stream(msg, &table);
        for it in &w.items {
            if let client::Item::User { name, payload, .. } = it {
                *totals.entry(name.clone()).or_default() += 1;
                if matches!(name.as_str(), "TeamInfo" | "TextMsg" | "StatusIcon" | "CurWeapon" | "Money") {
                    let txt: String = payload
                        .iter()
                        .map(|&c| if (32..127).contains(&c) { c as char } else { '.' })
                        .collect();
                    eprintln!("     {name}: {txt}");
                }
            }
        }
        if let Some(op) = w.stopped_on {
            *halted.entry(op).or_default() += 1;
        }
    }
    eprintln!("  ALL user messages decoded: {totals:?}");
    eprintln!("  walks halted on opcode: {halted:?}");

    // Report every svc_stufftext the server sent us, at any phase: these are
    // commands a real client echoes straight back.
    let mut stuff = Vec::new();
    for msg in &session.recorded {
        let mut i = 0usize;
        while i < msg.len() {
            if msg[i] == client::svc::SVC_STUFFTEXT {
                if let Some(end) = msg[i + 1..].iter().position(|&b| b == 0) {
                    let text = String::from_utf8_lossy(&msg[i + 1..i + 1 + end]).to_string();
                    let printable = text
                        .chars()
                        .all(|c| c.is_ascii_graphic() || c == ' ' || c == '\n');
                    if !text.is_empty() && printable {
                        stuff.push(text);
                    }
                    i += 1 + end + 1;
                    continue;
                }
            }
            i += 1;
        }
    }
    // What userinfo does the SERVER think we have? It echoes it back in
    // svc_updateuserinfo; cl_updaterate there drives next_messageinterval.
    for msg in &session.recorded {
        if let Some(pos) = msg.windows(9).position(|w| w == b"updaterat") {
            let start = pos.saturating_sub(80);
            let end = (pos + 160).min(msg.len());
            let txt: String = msg[start..end]
                .iter()
                .map(|&c| if (32..127).contains(&c) { c as char } else { '.' })
                .collect();
            eprintln!("  SERVER-SIDE USERINFO: {txt}");
            break;
        }
    }
    eprintln!("recorded {} messages; stufftext candidates:", session.recorded.len());
    for t in stuff.iter().take(30) {
        eprintln!("   STUFFTEXT {t:?}");
    }

    // The verdict. `svc_clientdata` is the server's own account of where we
    // are, so this is not our client marking its own homework.
    match session.clientdata.as_ref() {
        Some(cd) => {
            let o = cd.origin();
            eprintln!(
                "  SERVER-SIDE STATE: origin [{:.0} {:.0} {:.0}] health {:.0} maxspeed {:.0} alive {}",
                o[0], o[1], o[2], cd.health(), cd.maxspeed(), cd.alive()
            );
            eprintln!(
                "  MOVEMENT: travelled {max_travel:.0} units, peak speed {max_speed:.0} u/s"
            );
            if max_travel < 32.0 {
                eprintln!("  !!! the bot did not move -- commands are being discarded or it is dead");
            }
        }
        None => eprintln!("  !!! no svc_clientdata decoded -- not receiving datagrams"),
    }
    eprintln!(
        "wrote {records} message records, {bytes} bytes to {out_path} \
         (reliables settled: {})",
        session.reliables_settled()
    );
}
