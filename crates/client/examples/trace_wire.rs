//! Log **every** server message this client receives, in order, until the
//! server drops us.
//!
//! ```text
//! cargo run -p client --example trace_wire -- [addr] [seconds]
//! ```
//!
//! Why this exists: the question "what does the server actually send us before
//! it drops the bot" had no answer that could be trusted. The only tracing we
//! had scanned the byte stream for the value 9 (`svc_stufftext`) and printed a
//! string from wherever it found one, so it reported commands that were really
//! fragments of MD5 hashes, delta descriptions and user-message payloads — a
//! run produced thirty lines of which one was real. Everything printed here is
//! located by [`Session::trace_message`], which walks the stream message by
//! message and **stops, loudly**, rather than guessing.
//!
//! Three things it therefore reports honestly and a scan cannot:
//!
//! * `<-` lines are messages at real boundaries. Nothing else is printed.
//! * `!! halted on <opcode>` means the rest of that stream was not decoded —
//!   normal at the bit-packed `svc_clientdata`/`svc_packetentities` block,
//!   which needs the delta tables and baselines that `world::Decoder` owns.
//!   Reliable messages are always in FRONT of it (a netchannel packet is
//!   `[reliable][unreliable datagram]`), so nothing the server *tells* us is
//!   lost there.
//! * `<< connectionless` is printed straight off the socket, which is the only
//!   way to see a refusal: a rejected `connect` is answered with an
//!   out-of-band packet, not with `svc_disconnect`.
//!
//! Environment: `AIPLAYERS_NAME`, `AIPLAYERS_KEY`, `AIPLAYERS_IDLE_MS`
//! (how long a silence counts as "the server stopped talking to us").

use std::env;
use std::io;
use std::time::{Duration, Instant};

use client::session::StreamTrace;
use client::stream::Item;
use client::{svc, Identity, Session, Transport};

/// Wraps the socket so out-of-band traffic is visible. A connect refusal never
/// reaches the session at all — `Client::handle_datagram` consumes it — so a
/// trace built only from assembled `svc_*` streams cannot show the one packet
/// that says why we were turned away.
struct Tap<T: Transport> {
    inner: T,
    start: Instant,
}

impl<T: Transport> Transport for Tap<T> {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.send(data)
    }
    fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        let d = self.inner.recv()?;
        if let Some(d) = &d {
            if d.len() >= 4 && d[..4] == [0xFF, 0xFF, 0xFF, 0xFF] {
                println!(
                    "[{:7.3}] << connectionless {:?}",
                    self.start.elapsed().as_secs_f32(),
                    printable(&d[4..])
                );
            }
        }
        Ok(d)
    }
}

/// Render bytes as text, showing anything unprintable as an escape. Used for
/// every payload, so a payload that *is* text reads as text and one that is not
/// cannot be mistaken for it.
fn printable(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        match c {
            b'\n' => s.push_str("\\n"),
            b'\r' => s.push_str("\\r"),
            0 => s.push_str("\\0"),
            0x20..=0x7E => s.push(c as char),
            _ => s.push_str(&format!("\\x{c:02x}")),
        }
    }
    s
}

fn hex(b: &[u8], max: usize) -> String {
    let mut s: String = b
        .iter()
        .take(max)
        .map(|c| format!("{c:02x} "))
        .collect();
    if b.len() > max {
        s.push_str(&format!("... (+{} bytes)", b.len() - max));
    }
    s.trim_end().to_string()
}

/// Message name, filling the gaps in [`svc::name`] so nothing prints as
/// `svc_unknown` when it is in fact a message the walker sized correctly. The
/// distinction matters: `svc_unknown` should mean "we do not know what this
/// is", and if it is used for messages we *do* handle it stops meaning that.
fn msg_name(id: u8) -> &'static str {
    match id {
        svc::SVC_SOUND => "svc_sound",
        svc::SVC_SETANGLE => "svc_setangle",
        svc::SVC_LIGHTSTYLE => "svc_lightstyle",
        svc::SVC_STOPSOUND => "svc_stopsound",
        svc::SVC_PARTICLE => "svc_particle",
        svc::SVC_DAMAGE => "svc_damage",
        svc::SVC_SPAWNSTATIC => "svc_spawnstatic",
        svc::SVC_SETPAUSE => "svc_setpause",
        svc::SVC_CENTERPRINT => "svc_centerprint",
        svc::SVC_SPAWNSTATICSOUND => "svc_spawnstaticsound",
        svc::SVC_INTERMISSION => "svc_intermission",
        svc::SVC_CDTRACK => "svc_cdtrack",
        svc::SVC_WEAPONANIM => "svc_weaponanim",
        svc::SVC_DECALNAME => "svc_decalname",
        svc::SVC_ROOMTYPE => "svc_roomtype",
        svc::SVC_ADDANGLE => "svc_addangle",
        svc::SVC_CHOKE => "svc_choke",
        svc::SVC_NEWMOVEVARS => "svc_newmovevars",
        svc::SVC_RESOURCEREQUEST => "svc_resourcerequest",
        svc::SVC_CUSTOMIZATION => "svc_customization",
        svc::SVC_CROSSHAIRANGLE => "svc_crosshairangle",
        svc::SVC_SOUNDFADE => "svc_soundfade",
        svc::SVC_FILETXFERFAILED => "svc_filetxferfailed",
        svc::SVC_HLTV => "svc_hltv",
        svc::SVC_DIRECTOR => "svc_director",
        svc::SVC_VOICEINIT => "svc_voiceinit",
        svc::SVC_VOICEDATA => "svc_voicedata",
        svc::SVC_SENDEXTRAINFO => "svc_sendextrainfo",
        svc::SVC_TIMESCALE => "svc_timescale",
        svc::SVC_RESOURCELOCATION => "svc_resourcelocation",
        svc::SVC_VERSION => "svc_version",
        other => svc::name(other),
    }
}

/// The messages whose whole content is a string the operator needs verbatim.
fn is_string_message(id: u8) -> bool {
    matches!(
        id,
        svc::SVC_PRINT
            | svc::SVC_STUFFTEXT
            | svc::SVC_CENTERPRINT
            | svc::SVC_DISCONNECT
            | svc::SVC_FILETXFERFAILED
            | svc::SVC_RESOURCELOCATION
            | svc::SVC_SENDCVARVALUE
    )
}

struct Log {
    start: Instant,
    /// How many of `session.recorded` have been printed.
    cursor: usize,
    /// Serial number of each assembled stream, so the order is unambiguous.
    stream: usize,
    /// Every message name seen, in order of first appearance, with a count.
    seen: Vec<(String, usize)>,
    /// The last few messages, for the post-mortem.
    tail: Vec<String>,
    disconnect_reason: Option<String>,
}

impl Log {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            cursor: 0,
            stream: 0,
            seen: Vec::new(),
            tail: Vec::new(),
            disconnect_reason: None,
        }
    }

    fn note(&mut self, name: &str, detail: &str) {
        match self.seen.iter_mut().find(|(n, _)| n == name) {
            Some((_, n)) => *n += 1,
            None => self.seen.push((name.to_string(), 1)),
        }
        self.tail.push(if detail.is_empty() {
            name.to_string()
        } else {
            format!("{name}  {detail}")
        });
        if self.tail.len() > 24 {
            self.tail.remove(0);
        }
    }

    fn banner(&self, what: &str) {
        println!(
            "\n[{:7.3}] ===== {what} =====",
            self.start.elapsed().as_secs_f32()
        );
    }

    fn out(&self, what: &str) {
        println!("[{:7.3}] ->  {what}", self.start.elapsed().as_secs_f32());
    }

    /// Print every stream recorded since the last call.
    ///
    /// The timestamp is `Session::recorded_at`, i.e. when the message was
    /// **assembled**, not when this ran — the two differ by up to the whole
    /// length of a blocking helper like `enter_game`, and it is precisely the
    /// silence inside those calls that has to be measurable.
    fn drain(&mut self, s: &mut Session) {
        while self.cursor < s.recorded.len() {
            // Cloned because tracing borrows the session mutably (it learns the
            // user-message registrations as it walks).
            let msg = s.recorded[self.cursor].clone();
            let at = s.recorded_at[self.cursor];
            self.cursor += 1;
            self.stream += 1;
            let trace = s.trace_message(&msg);
            self.print_stream(at, &msg, &trace);
        }
    }

    fn print_stream(&mut self, at: Instant, msg: &[u8], trace: &StreamTrace) {
        let stamp = at.duration_since(self.start).as_secs_f32();

        // A packet padded out with nothing but svc_nop is a keepalive and says
        // only "the server is still there". Collapsed to one line so it cannot
        // bury the messages that carry information -- 1008 of 1114 messages in
        // the first run of this example were nops.
        if !trace.items.is_empty()
            && trace.stopped_on.is_none()
            && trace
                .items
                .iter()
                .all(|i| matches!(i, Item::Engine { id, .. } if *id == svc::SVC_NOP))
        {
            self.note("svc_nop", "");
            println!(
                "[{stamp:7.3}] <- #{:<4} keepalive ({} x svc_nop)",
                self.stream,
                trace.items.len()
            );
            return;
        }

        println!(
            "[{stamp:7.3}] <- #{:<4} {} bytes, {} messages",
            self.stream,
            msg.len(),
            trace.items.len()
        );
        // Runs of the same message collapse to `xN`; everything else prints.
        let mut run: Option<(u8, usize)> = None;
        for item in &trace.items {
            if let Item::Engine { id, payload } = item {
                if payload.is_empty() {
                    match &mut run {
                        Some((r, n)) if *r == *id => {
                            *n += 1;
                            self.note(msg_name(*id), "");
                            continue;
                        }
                        _ => {}
                    }
                    self.flush_run(&mut run);
                    run = Some((*id, 1));
                    self.note(msg_name(*id), "");
                    continue;
                }
            }
            self.flush_run(&mut run);
            match item {
                Item::Engine { id, payload } => {
                    let name = msg_name(*id);
                    if is_string_message(*id) {
                        println!("             {name:<22} \"{}\"", printable(payload));
                        self.note(name, &format!("\"{}\"", printable(payload)));
                        if *id == svc::SVC_DISCONNECT {
                            self.disconnect_reason =
                                Some(String::from_utf8_lossy(payload).into_owned());
                        }
                    } else {
                        println!(
                            "             {name:<22} {:>5}B  {}",
                            payload.len(),
                            hex(payload, 20)
                        );
                        self.note(name, "");
                    }
                }
                Item::User { id, name, payload } => {
                    println!(
                        "             {:<22} {:>5}B  {}   |{}|",
                        format!("{name}({id})"),
                        payload.len(),
                        hex(payload, 12),
                        printable(payload)
                    );
                    self.note(&format!("user:{name}"), "");
                }
            }
        }
        self.flush_run(&mut run);
        if let Some(op) = trace.stopped_on {
            println!(
                "             !! halted on {} ({}) at byte {} of {} -- {} bytes not decoded",
                msg_name(op),
                op,
                trace.stopped_at,
                msg.len(),
                msg.len() - trace.stopped_at
            );
        }
    }

    fn flush_run(&self, run: &mut Option<(u8, usize)>) {
        if let Some((id, n)) = run.take() {
            if n == 1 {
                println!("             {:<22}     0B", msg_name(id));
            } else {
                println!("             {:<22}  x{n}", msg_name(id));
            }
        }
    }
}

fn reb_env(key: &str) -> Result<String, env::VarError> {
    env::var(format!("REB_{key}"))
        .or_else(|_| env::var(format!("REBOTS_{key}")))
        .or_else(|_| env::var(format!("AIPLAYERS_{key}")))
}

fn main() {
    let mut args = env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:27015".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(90);
    let idle_ms: u64 = reb_env("IDLE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let mut log = Log::new();
    let inner = match client::UdpTransport::connect(addr.parse().expect("addr"), None) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        }
    };
    let mut t = Tap { inner, start: log.start };

    let name = reb_env("NAME").unwrap_or_else(|_| "Probe".into());
    let key = reb_env("KEY").unwrap_or_else(|_| "REBPROBE00000001".into());
    println!("tracing {addr} as name={name:?} key={key:?}");

    let mut s = Session::new(Identity {
        name: name.clone(),
        key: key.into_bytes(),
        ..Default::default()
    });
    // Keeps every assembled stream, including the ones connect_and_signon
    // consumes internally -- otherwise the whole signon is invisible.
    s.record_all = true;

    log.banner("handshake + `new` (signon burst)");
    // Reduced to owned text before anything else touches the session: the
    // signon is borrowed from it and draining the log needs it mutably.
    let outcome = match s.connect_and_signon(&mut t, Duration::from_secs(15)) {
        Ok(signon) => Ok(format!(
            "map {}, {} delta tables, our slot {} of {}",
            signon
                .server_info
                .as_ref()
                .map(|si| si.map_name().to_string())
                .unwrap_or_default(),
            signon.registry.len(),
            signon.server_info.as_ref().map(|si| si.player_index).unwrap_or(0),
            signon.server_info.as_ref().map(|si| si.max_players).unwrap_or(0),
        )),
        Err(e) => Err(e),
    };
    log.drain(&mut s);
    match outcome {
        Ok(d) => println!("             (signon parsed: {d})"),
        Err(e) => {
            println!("!!! signon failed: {e}");
            summary(&log);
            return;
        }
    }

    // Bisect. `AIPLAYERS_TRACE_STOP=<stage>` stops sending after that stage and
    // then does nothing but acknowledge, so the drop can be attributed:
    //
    //   signon | sendres | resourcelist | spawn      (default: run everything)
    //
    // A drop that lands at the same moment whatever we stop after is a
    // server-side timer; one that only appears once a particular message has
    // gone out is a reaction to that message. Nothing in the packets we send
    // can distinguish those two on its own, which is why this knob exists.
    let stop_after = reb_env("TRACE_STOP").unwrap_or_default();
    if stop_after == "signon" {
        return idle_until_drop(&mut s, &mut t, &mut log, secs);
    }

    log.banner("sendres");
    log.out("clc_stringcmd \"sendres\"");
    s.send_command(Session::SENDRES);
    settle(&mut s, &mut t, &mut log, Duration::from_millis(1200));
    if stop_after == "sendres" {
        return idle_until_drop(&mut s, &mut t, &mut log, secs);
    }

    log.banner("clc_resourcelist");
    log.out("clc_resourcelist (0 resources)");
    s.upload_resource_list();
    settle(&mut s, &mut t, &mut log, Duration::from_millis(1200));
    if stop_after == "resourcelist" {
        return idle_until_drop(&mut s, &mut t, &mut log, secs);
    }

    let spawncount = s
        .resource_message
        .as_ref()
        .map(|r| r.spawncount)
        .or_else(|| s.recorded.iter().find_map(|m| Session::spawncount_from(m)))
        .unwrap_or(1);
    println!("             (spawncount {spawncount})");

    s.start_decoding();
    s.load_map(0);

    // Finer bisect: `spawn` alone, without the `sendents` that `enter_game`
    // chases it with. These are two different messages and blaming the wrong
    // one is a day's work.
    if stop_after == "fileconsistency" {
        log.banner("clc_fileconsistency + \"spawn\" ONLY (no sendents)");
        log.out("clc_fileconsistency + \"spawn\"");
        s.upload_spawn(spawncount);
        return idle_until_drop(&mut s, &mut t, &mut log, secs);
    }

    log.banner("spawn (clc_fileconsistency) then sendents");
    log.out("clc_fileconsistency + \"spawn\", then \"sendents\"");
    match s.enter_game(&mut t, spawncount, Duration::from_secs(12)) {
        Ok(true) => {
            log.drain(&mut s);
            println!("             (server is streaming -- fully connected)");
        }
        Ok(false) => {
            log.drain(&mut s);
            println!("             !!! server never started streaming");
        }
        Err(e) => {
            log.drain(&mut s);
            println!("             !!! enter_game error: {e}");
        }
    }
    log.drain(&mut s);
    if stop_after == "spawn" {
        return idle_until_drop(&mut s, &mut t, &mut log, secs);
    }

    settle(&mut s, &mut t, &mut log, Duration::from_millis(2000));

    log.banner("jointeam / joinclass");
    log.out("jointeam 1 + joinclass");
    match s.join_and_spawn(&mut t, Session::TEAM_TERRORIST, Duration::from_secs(15)) {
        Ok(true) => {
            log.drain(&mut s);
            println!("             (team accepted)");
        }
        Ok(false) => {
            log.drain(&mut s);
            println!("             !!! team never accepted");
        }
        Err(e) => {
            log.drain(&mut s);
            println!("             !!! join error: {e}");
        }
    }

    log.banner("running -- until the server drops us");
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last_traffic = Instant::now();
    let mut last_count = s.recorded.len();
    let intent = bot::Intent::default();
    while Instant::now() < deadline {
        if s.frame(&mut t, &intent).is_err() {
            println!("             !!! socket error");
            break;
        }
        log.drain(&mut s);
        // A level change is the one thing that makes this server stuff a
        // command at us. `Session::echo_stufftexts` latches it; acting on it
        // needs the transport, so it happens here.
        if s.pending_reconnect {
            println!(
                "[{:7.3}] *** stufftext `reconnect` -- re-running the signon",
                log.start.elapsed().as_secs_f32()
            );
            if let Ok(path) = reb_env("DUMP_RECONNECT") {
                if let Some(m) = s
                    .recorded
                    .iter()
                    .find(|m| m.windows(9).any(|w| w == b"reconnect"))
                {
                    std::fs::write(&path, m).expect("dump");
                    println!("             (wrote {} bytes to {path})", m.len());
                }
            }
            match s.rejoin_after_reconnect(&mut t, Duration::from_secs(25)) {
                Ok(map) => println!("             (reconnected, map {map})"),
                Err(e) => {
                    println!("             !!! reconnect failed: {e}");
                    break;
                }
            }
            log.drain(&mut s);
        }
        if s.recorded.len() != last_count {
            last_count = s.recorded.len();
            last_traffic = Instant::now();
        }
        if log.disconnect_reason.is_some() {
            println!(
                "[{:7.3}] *** svc_disconnect received -- stopping",
                log.start.elapsed().as_secs_f32()
            );
            break;
        }
        if last_traffic.elapsed() > Duration::from_millis(idle_ms) {
            println!(
                "[{:7.3}] *** {} ms of silence -- the server has stopped sending",
                log.start.elapsed().as_secs_f32(),
                idle_ms
            );
            break;
        }
    }
    log.drain(&mut s);
    // Leave cleanly: a socket that just goes quiet leaves a `client_t` sitting
    // connected for `sv_timeout` (120 s) and the next bot from this same base
    // address inherits its slot AND its name.
    let _ = s.disconnect(&mut t, Duration::from_secs(2));
    summary(&log);
}

/// Send nothing but acknowledgements and log what the server does about it.
fn idle_until_drop<T: Transport>(s: &mut Session, t: &mut T, log: &mut Log, secs: u64) {
    log.banner("BISECT -- sending nothing but acks from here on");
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline && log.disconnect_reason.is_none() {
        let _ = s.pump_idle(t);
        log.drain(s);
    }
    log.drain(s);
    summary(log);
}

fn settle<T: Transport>(s: &mut Session, t: &mut T, log: &mut Log, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        let _ = s.pump_idle(t);
        log.drain(s);
    }
}

fn summary(log: &Log) {
    println!("\n===== every message type received, first appearance first =====");
    for (name, n) in &log.seen {
        println!("  {n:>5}  {name}");
    }
    println!("\n===== the LAST {} messages before we stopped =====", log.tail.len());
    for (i, line) in log.tail.iter().enumerate() {
        println!("  {:>2}. {line}", i + 1);
    }
    match &log.disconnect_reason {
        Some(r) => println!("\nsvc_disconnect reason: {r:?}"),
        None => println!("\nno svc_disconnect was ever received"),
    }
}
