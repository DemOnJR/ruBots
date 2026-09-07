//! The full in-game session: handshake → signon → running.
//!
//! [`Client`] only carries the connectionless handshake up to `connected`.
//! This is the piece that takes it the rest of the way: it sends
//! `clc_stringcmd "new"`, reassembles and decompresses the server's signon
//! burst, learns every delta table from it, and then pumps the bot's commands
//! back out with a [`MoveSender`].
//!
//! The receive pipeline mirrors the engine's, in order: SPLIT (`-2`) datagram
//! reassembly → netchannel unmunge → normal fragment reassembly (the two
//! `fragbuf` streams) → bzip2 → an `svc_*` stream. Each layer is a verified
//! piece from `netchan`; this only sequences them.
//!
//! What is proven here and what is not: the receive path and the signon walk
//! are checked against a live server (`tests/live_signon.rs`). Team/class join
//! and the steady-state entity pump are the next layers up.

use std::io;
use std::time::{Duration, Instant};

use netchan::{
    classify, maybe_decompress, Datagram, FragmentBuffer, Fragments, NetChannel, SplitReassembler,
    MAX_STREAMS,
};

use crate::control::MoveSender;
use crate::signon::{walk as walk_signon, Signon};
use crate::{Client, Disconnect, Identity, State, Transport};

/// A snapshot of one `think()`, so a stalled bot can be attributed to the
/// layer that stalled it rather than guessed at.
/// A rolling measurement of how the view we actually send moves.
///
/// "The crosshair looks wrong" is not a measurement, and a `.sent` capture
/// cannot say which rung was running when it was recorded. The meter lives
/// here, where both the sent angle and the rung are known. Two numbers:
///
/// * **reversals per second** - direction changes in yaw. A person scanning
///   reverses about once a second; five or six a second is a tremor.
/// * **longest dwell** - the longest run with the yaw effectively parked. A
///   defender holds one angle for seconds at a time.
#[derive(Debug, Default, Clone)]
pub struct ViewStats {
    /// `(time, yaw)` over the last [`ViewStats::WINDOW`] seconds.
    samples: std::collections::VecDeque<(f32, f32)>,
    now: f32,
}

impl ViewStats {
    /// How much history the numbers describe.
    pub const WINDOW: f32 = 5.0;
    /// Yaw movement below this is the view standing still.
    const PARKED: f32 = 0.15;

    pub fn note(&mut self, yaw: f32, dt: f32) {
        self.now += dt.clamp(0.0, 0.25);
        self.samples.push_back((self.now, yaw));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| self.now - t > Self::WINDOW)
        {
            self.samples.pop_front();
        }
    }

    fn span(&self) -> f32 {
        match (self.samples.front(), self.samples.back()) {
            (Some((a, _)), Some((b, _))) => (b - a).max(0.001),
            _ => 0.001,
        }
    }

    /// Yaw direction changes per second over the window.
    pub fn reversals_per_sec(&self) -> f32 {
        let mut sign = 0i32;
        let mut reversals = 0u32;
        let mut prev: Option<f32> = None;
        for (_, yaw) in &self.samples {
            if let Some(p) = prev {
                let d = bot::math::norm_angle(f64::from(yaw - p)) as f32;
                let s = if d > 0.02 {
                    1
                } else if d < -0.02 {
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
            prev = Some(*yaw);
        }
        reversals as f32 / self.span()
    }

    /// Longest stretch, in seconds, with the yaw parked.
    pub fn longest_dwell(&self) -> f32 {
        let mut run = 0.0f32;
        let mut best = 0.0f32;
        let mut prev: Option<(f32, f32)> = None;
        for &(t, yaw) in &self.samples {
            if let Some((pt, pyaw)) = prev {
                let d = bot::math::norm_angle(f64::from(yaw - pyaw)).abs() as f32;
                if d < Self::PARKED {
                    run += t - pt;
                    best = best.max(run);
                } else {
                    run = 0.0;
                }
            }
            prev = Some((t, yaw));
        }
        best
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub alive: bool,
    pub in_game: bool,
    pub site: Option<[f32; 3]>,
    pub forwardmove: f32,
    pub sidemove: f32,
    pub yaw: f32,
    pub waypoints_left: usize,
    pub reroutes: u32,
    pub stuck: bool,
    /// The nav node the bot is currently steering at (plan ROUTE-3/4).
    pub node: Option<usize>,
    /// `IN_ATTACK` this tick — also how a plant in progress shows up.
    pub attack: bool,
    /// `IN_USE` this tick — defusing, and hostages.
    pub use_action: bool,
    /// The bot believes it is carrying the C4.
    pub carrying_bomb: bool,
    /// The bot believes a bomb is planted, and where. Without this there is no
    /// way to tell "the defuse machine is broken" from "nobody ever told this
    /// client there was a bomb" -- and for a client that joins mid-round those
    /// are completely different problems in completely different layers.
    pub bomb_planted: bool,
    pub bomb_known_at: Option<[f32; 3]>,
    /// The plant machine has the button down and the timer running.
    pub arming: bool,
    /// Straight-line distance to the objective, which is the number that
    /// actually says whether the navigation is working.
    pub to_goal: f32,
    /// Which rung of the brain's ladder decided this tick.
    pub rung: &'static str,
    /// Yaw direction changes per second in the view actually sent.
    pub look_reversals: f32,
    /// Longest run, in seconds, with the sent yaw parked.
    pub look_dwell: f32,
    /// Escort phase, as a word. None of the escort's state is on the wire in a
    /// form the bot can read back -- `HostagePos` is a 1 Hz radar blip and
    /// nothing at all says who a hostage is following -- so a hostage round is
    /// opaque without this: "no rescue" has a dozen explanations and no way to
    /// tell them apart from outside the process.
    pub escort: &'static str,
    /// Hostages the bot can see, and how many it believes it recruited.
    pub hostages: usize,
    pub hostages_led: usize,
    /// Distance to the hostage the escort machine is working on.
    pub to_hostage: f32,
    /// Rising `+use` edges the escort has emitted this life.
    pub use_edges: u32,
    /// Tactical role this round (assault/hold/flank/split). Empty when unknown.
    pub role: &'static str,
    /// Cumulative G2 rotation events this round.
    pub rotate_events: u32,
    /// Current G2 rotation target site, if any.
    pub rotate_site: Option<usize>,
}

/// Where a session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Still doing the connectionless handshake.
    Handshake,
    /// Connected; `new` sent, collecting the signon.
    Signon,
    /// Signon parsed; sending moves.
    Running,
}

/// A complete client session over a [`Transport`].
pub struct Session {
    pub client: Client,
    pub chan: NetChannel,
    frag: [FragmentBuffer; MAX_STREAMS],
    split: SplitReassembler,
    pub phase: Phase,
    pub signon: Option<Signon>,
    sender: Option<MoveSender>,
    /// Receive-path diagnostics: how many of each datagram kind arrived and
    /// how many actually completed into a message.
    pub stats: RecvStats,
    resyncs: u32,
    /// When set, every assembled message is kept in `recorded` — including the
    /// signon phase, which `connect_and_signon` otherwise consumes silently.
    pub record_all: bool,
    pub recorded: Vec<Vec<u8>>,
    /// When each entry of `recorded` was assembled, index-parallel with it.
    ///
    /// Pushed in the same breath as the message, because the alternative --
    /// timestamping a recording when something gets round to reading it --
    /// produces a trace that says every message in a burst arrived at the same
    /// instant, which is exactly the twelve-second blind spot a blocking call
    /// like [`enter_game`](Session::enter_game) creates.
    pub recorded_at: Vec<Instant>,
    /// The server's reply to `sendres`, captured the moment it arrives.
    ///
    /// It does **not** come inside the signon burst — it is its own message,
    /// sent only after we ask — so `Signon::resources` is always empty and
    /// anything that reads consistency out of the signon reads nothing. This
    /// is the authoritative copy: it carries the resource list *and* the
    /// consistency demands, which share one bit block.
    pub resource_message: Option<proto::resources::ResourceMessage>,
    /// `spawn` has gone out on this signon.
    ///
    /// Reset by [`reconnect`](Session::reconnect), because a level change puts
    /// the client back before `spawn` on the server's side too
    /// (`SV_InactivateClients` clears `spawned`/`fully_connected`,
    /// `sv_main.cpp:7702-7729`).
    pub spawn_uploaded: bool,
    /// The server has stuffed `reconnect` at us and we have not acted yet.
    pub pending_reconnect: bool,
    /// Local game files, for the exact-file consistency demands that cannot be
    /// answered from the wire alone. `None` when we have no content.
    pub content: Option<crate::content::GameContent>,
    /// Paces the outgoing command stream against the wall clock.
    pub clock: crate::clock::MoveClock,
    /// Our own player state as of the last server datagram: position,
    /// velocity, health, weapons. This is the objective test of whether our
    /// movement commands are being applied.
    pub clientdata: Option<crate::world::ClientData>,
    /// Paced console commands (buy aliases, weapon switches, chat).
    pub console: crate::console::ConsoleQueue,
    /// The bot's brain. `None` means "send neutral commands", which is what a
    /// capture or protocol test wants.
    pub brain: Option<bot::Controller>,
    /// Objective the bot is heading for, supplied by the nav layer.
    ///
    /// For Hold/Flank roles this is an approach-ring point, not the plant disc.
    /// The carrier override in `think` uses [`plant_spot`] instead.
    pub site: Option<[f32; 3]>,
    /// Point inside a bomb-site volume — where a C4 carrier must plant.
    pub plant_spot: Option<[f32; 3]>,
    /// Tactical role for this round (plan Phase A1).
    pub role: Option<crate::role::BotRole>,
    /// Phase G2: last site index we rotated toward (for hysteresis / logs).
    pub rotate_site: Option<usize>,
    /// Explicit current assigned bomb-site index for G0 reports.
    pub assigned_site: Option<usize>,
    /// Same-team tactical belief from the G0 state bus.
    pub team_snapshot: Option<bot::TeamSnapshot>,
    /// Number of distinct G2 repaths this round.
    pub rotate_events: u32,
    /// Cooldown so we do not repath every tick when enemies flicker PVS.
    rotate_cooldown: f32,
    /// The loaded map: collision, entities and the navigation graph.
    pub map: Option<crate::map::Map>,
    follower: crate::navigate::PathFollower,
    /// How many times a jump was refused because nothing could clear what was
    /// in front. A bot with a climbing count is being routed into geometry it
    /// cannot pass, which is a graph problem and not a steering one.
    pub refused_jumps: u32,
    /// Rate limit for the diagnostic line that reports those refusals.
    last_refusal_log: Option<Instant>,
    /// Sight lines, cached against the defend point they were computed from.
    watch_cache: Option<([f32; 3], [Option<[f32; 3]>; bot::controller::MAX_WATCH])>,
    /// How the sent view has been moving, for the look diagnostics.
    view_stats: ViewStats,
    /// Last obstacle probe: where it was taken, when, and what it said.
    ///
    /// The sweep is a few dozen hull traces, so it is cached rather than run
    /// every tick while a bot is scraping: the geometry in front of a body
    /// that has not moved does not change.
    ahead_probe: Option<([f32; 3], f32, Instant, nav::ahead::Ahead)>,
    /// Where we were last frame, and when. `clientdata_t` does NOT carry
    /// velocity -- the server omits it because a predicting client computes
    /// its own -- so real speed has to be measured from successive origins.
    last_origin: Option<([f32; 3], Instant)>,
    last_speed: f32,
    /// What the brain decided last frame, for diagnostics.
    pub last_decision: Option<Decision>,
    /// Latest local report for the G0 state bus.
    pub latest_team_report: Option<bot::TeamReport>,
    team_bot_id: u16,
    /// Seconds since the last freeze period ended (round start grace).
    ///
    /// Bots spawn in a crowd and box each other in for the first moments of a
    /// round. During this grace the natural walker (weave / micro-pause) is
    /// suppressed so the follower's unstick can find a gap cleanly instead of
    /// the weave grinding into a teammate.
    pub post_freeze_grace: f32,
    last_think: Option<Instant>,
    /// Which round we last bought in, so a buy happens once per spawn rather
    /// than every frame we happen to be standing in the zone.
    bought_at_reset: Option<u32>,
    /// Which spawn we last deployed a weapon on.
    deployed_at_reset: Option<u32>,
    /// The world model: baselines, entities, and accumulated game state.
    /// Built once the signon has taught us the delta tables and the user
    /// message table.
    pub decoder: Option<crate::world::Decoder>,
    /// The last few commands we sent, re-sent as `numbackup` so a lost packet
    /// costs no input. A real client always carries two.
    cmd_history: std::collections::VecDeque<proto::usercmd::UserCmd>,
    /// Newest server frame we have fully decoded, and may therefore advertise
    /// in `clc_delta`. `None` until the entity decoder exists — advertising a
    /// frame we never parsed makes the server delta against a world we do not
    /// have.
    pub last_valid_frame: Option<u32>,
    /// Every user message the server has registered with `svc_newusermsg`,
    /// learned by [`trace_message`](Session::trace_message) as the
    /// registrations are walked over. Without it the walker cannot size a user
    /// message and has to stop at the first one.
    pub user_msgs: crate::stream::UserMsgTable,
}

/// Counters for the receive pipeline, so a stall can be attributed to the
/// layer that swallowed the packet rather than guessed at.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecvStats {
    pub datagrams: u32,
    pub split_seen: u32,
    pub split_completed: u32,
    pub frag_seen: u32,
    pub frag_completed: u32,
    pub plain: u32,
    pub read_rejected: u32,
}

/// One assembled `svc_*` stream, decoded into the messages it actually
/// contains.
///
/// Produced by [`Session::trace_message`]. Every message here was **located by
/// parsing** — the walker sized its predecessor and landed on this opcode — so
/// an entry is a message boundary, not a byte that happened to hold that value.
#[derive(Debug, Default, Clone)]
pub struct StreamTrace {
    pub items: Vec<crate::stream::Item>,
    /// How far the walk got.
    pub stopped_at: usize,
    /// The opcode that halted it. `None` means the whole stream was consumed;
    /// anything else means the tail after `stopped_at` was **not looked at**,
    /// which is the honest answer and the one a scan cannot give.
    pub stopped_on: Option<u8>,
}

impl StreamTrace {
    /// The text of every `svc_stufftext`, in order.
    pub fn strings_of(&self, id: u8) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|it| match it {
                crate::stream::Item::Engine { id: i, payload } if *i == id => {
                    Some(String::from_utf8_lossy(payload).into_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// The console commands the server pushed at us, in order.
    pub fn stufftexts(&self) -> Vec<String> {
        self.strings_of(crate::svc::SVC_STUFFTEXT)
    }

    /// Did the walk consume everything?
    pub fn complete(&self) -> bool {
        self.stopped_on.is_none()
    }
}

impl Session {
    pub fn new(identity: Identity) -> Self {
        Self {
            client: Client::new(identity),
            chan: NetChannel::new(),
            frag: Default::default(),
            split: SplitReassembler::new(),
            phase: Phase::Handshake,
            signon: None,
            sender: None,
            stats: RecvStats::default(),
            resyncs: 0,
            record_all: false,
            recorded: Vec::new(),
            recorded_at: Vec::new(),
            resource_message: None,
            spawn_uploaded: false,
            pending_reconnect: false,
            content: crate::content::GameContent::discover(),
            clock: crate::clock::MoveClock::new(Instant::now()),
            clientdata: None,
            console: crate::console::ConsoleQueue::new(),
            brain: None,
            site: None,
            plant_spot: None,
            role: None,
            rotate_site: None,
            assigned_site: None,
            team_snapshot: None,
            rotate_events: 0,
            rotate_cooldown: 0.0,
            map: None,
            refused_jumps: 0,
            watch_cache: None,
            view_stats: ViewStats::default(),
            last_refusal_log: None,
            ahead_probe: None,
            follower: crate::navigate::PathFollower::new(), // re-seeded by set_seed
            last_origin: None,
            last_speed: 0.0,
            last_decision: None,
            latest_team_report: None,
            team_bot_id: 0,
            post_freeze_grace: 0.0,
            last_think: None,
            bought_at_reset: None,
            deployed_at_reset: None,
            decoder: None,
            cmd_history: std::collections::VecDeque::new(),
            last_valid_frame: None,
            user_msgs: crate::stream::UserMsgTable::new(),
        }
    }

    /// The `clc_stringcmd` payload for `cmd`, ready for [`NetChannel::build`].
    fn stringcmd(cmd: &str) -> Vec<u8> {
        NetChannel::string_command(cmd)
    }

    /// Single entry point for every fully-assembled message.
    ///
    /// Anything that must be caught the moment it arrives, rather than searched
    /// for afterwards, belongs here. The resource message is the motivating
    /// case: it is its own message rather than part of the signon burst, and
    /// hunting for it later meant scanning recorded bytes for the raw value 43
    /// — which is ASCII `'+'` and false-matches on payload data constantly.
    fn note_message(&mut self, msg: &[u8]) {
        if self.resource_message.is_none() {
            if let Some(rm) = proto::resources::parse_resource_message(msg) {
                self.resource_message = Some(rm);
            }
        }
        // `SV_WriteSpawn` sets `connecttime = realtime` and `cmdtime = 0`
        // together and then emits `svc_signonnum 1` as the last thing in the
        // same burst (`sv_main.cpp:1471-1479`). That is the exact moment the
        // server's move-time accounting restarts, so ours must too -- otherwise
        // every millisecond we claimed during the signon counts against us.
        if Self::has_spawn_tail(msg) {
            self.clock.reset(Instant::now());
        }
        // Our own authoritative position/health, straight from the server.
        if let Some(reg) = self.signon.as_ref().map(|s| &s.registry) {
            if let Some(cd) = crate::world::parse_datagram(msg, reg) {
                self.clientdata = Some(cd);
            }
        }
        if let Some(d) = self.decoder.as_mut() {
            d.feed(msg);
        }
        self.answer_cvar_queries(msg);
        if self.record_all {
            self.recorded.push(msg.to_vec());
            self.recorded_at.push(Instant::now());
        }
    }

    /// Cvar values we report when a server asks.
    ///
    /// A server (or a Metamod plugin) can query any client cvar with
    /// `svc_sendcvarvalue` / `svc_sendcvarvalue2`, and a client that never
    /// answers leaves the request outstanding. ReHLDS itself does not mind, but
    /// plugin-based anticheats routinely kick on the timeout — so a bot that
    /// stays silent works on our test server and gets thrown off real ones.
    ///
    /// The list is what a real client would have; anything unlisted is answered
    /// with an empty string, which is exactly what the engine reports for a
    /// cvar that does not exist.
    fn cvar_value(&self, name: &str) -> String {
        let id = &self.client.identity;
        match name {
            "sv_version" => "1.1.2.7/Stdio,48,4419".into(),
            "rate" => id.rate.to_string(),
            "cl_updaterate" => id.update_rate.to_string(),
            "cl_cmdrate" => "60".into(),
            "cl_lw" | "cl_lc" => "1".into(),
            "cl_dlmax" => "1024".into(),
            "cl_nopred" => "0".into(),
            "cl_timeout" => "60".into(),
            "m_pitch" => "0.022".into(),
            "gl_texturemode" => "GL_LINEAR_MIPMAP_LINEAR".into(),
            "_cl_autowepswitch" => "1".into(),
            "cl_download_ingame" => "1".into(),
            "hud_fastswitch" => "0".into(),
            "name" => id.name.clone(),
            "model" => "gordon".into(),
            _ => String::new(),
        }
    }

    /// Answer any cvar query in this message.
    ///
    /// `svc_sendcvarvalue` (57) is `string cvar` and is answered with
    /// `clc_cvarvalue` (10) `string value`. `svc_sendcvarvalue2` (58) adds a
    /// request id that must be echoed back, and the reply also repeats the cvar
    /// name (`SV_ParseCvarValue2`, `sv_user.cpp:1804-1815`).
    fn answer_cvar_queries(&mut self, msg: &[u8]) {
        let trace = self.trace_message(msg);
        for reply in self.cvar_replies(&trace) {
            self.chan.queue_reliable(&reply);
        }
    }

    /// The `clc_cvarvalue` / `clc_cvarvalue2` replies a walked message calls for.
    ///
    /// Takes a [`StreamTrace`] and not raw bytes, deliberately. The version
    /// this replaced scanned every byte of every datagram for the values 57 and
    /// 58, so any binary payload containing one manufactured a cvar query --
    /// and each phantom query queued a RELIABLE reply. At 50 packets a second
    /// that is a flood: measured live, the netchannel's reliable queue grew
    /// past 7900 entries and climbed by ~113 every two seconds, `in_flight`
    /// never cleared, and from that moment the client could not send another
    /// console command as long as it lived. The bot stood on the bomb site
    /// holding an AK with `weapon_c4` stuck in a queue that would never drain.
    ///
    /// That is the third time in this codebase that locating a message by
    /// searching for its opcode byte has caused a serious bug -- see
    /// `absorb_baselines` and the stufftext handler. Walk the stream.
    ///
    /// Pure, so a test can assert on the exact bytes rather than a queue depth.
    /// `svc_sendcvarvalue` (57) is `string cvar`, answered with
    /// `clc_cvarvalue` (10) `string value`. `svc_sendcvarvalue2` (58) adds a
    /// request id that must come back verbatim, and its reply repeats the cvar
    /// name too (`SV_ParseCvarValue2`, `sv_user.cpp:1804-1815`).
    fn cvar_replies(&self, trace: &StreamTrace) -> Vec<Vec<u8>> {
        let cstr = |b: &[u8]| {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf8_lossy(&b[..end]).into_owned()
        };
        let mut out = Vec::new();
        for item in &trace.items {
            let crate::stream::Item::Engine { id, payload } = item else {
                continue;
            };
            match *id {
                crate::svc::SVC_SENDCVARVALUE => {
                    let name = cstr(payload);
                    let mut reply = vec![netchan::clc::CVARVALUE];
                    reply.extend_from_slice(self.cvar_value(&name).as_bytes());
                    reply.push(0);
                    out.push(reply);
                }
                crate::svc::SVC_SENDCVARVALUE2 => {
                    if payload.len() < 4 {
                        continue;
                    }
                    let name = cstr(&payload[4..]);
                    let mut reply = vec![netchan::clc::CVARVALUE2];
                    reply.extend_from_slice(&payload[..4]);
                    reply.extend_from_slice(name.as_bytes());
                    reply.push(0);
                    reply.extend_from_slice(self.cvar_value(&name).as_bytes());
                    reply.push(0);
                    out.push(reply);
                }
                _ => {}
            }
        }
        out
    }

    /// The five bytes `SV_WriteSpawn` + `SV_WriteVoiceCodec` always end on.
    ///
    /// `svc_signonnum 1`, then `svc_voiceinit` with an empty codec string and a
    /// zero quality byte (`sv_main.cpp:5817-5822`) -- three fixed bytes, so the
    /// reassembled spawn response ends on this exact sequence.
    pub const SPAWN_TAIL: [u8; 5] = [
        crate::svc::SVC_SIGNONNUM,
        1,
        crate::svc::SVC_VOICEINIT,
        0,
        0,
    ];

    fn has_spawn_tail(msg: &[u8]) -> bool {
        let from = msg.len().saturating_sub(16);
        msg[from..].windows(5).any(|w| w == Self::SPAWN_TAIL)
    }

    /// Feed one raw datagram through the whole receive pipeline, returning any
    /// fully-assembled, decompressed reliable message payloads (an `svc_*`
    /// stream). Ordinary in-game packets come back as a single element; a
    /// fragmented signon comes back once every fragment has arrived.
    pub fn ingest(&mut self, datagram: &[u8]) -> Vec<Vec<u8>> {
        self.stats.datagrams += 1;
        match classify(datagram) {
            Datagram::Runt => vec![],
            Datagram::Connectionless(_) => {
                // Only meaningful before we are in game.
                let _ = self.client.handle_datagram(datagram);
                vec![]
            }
            Datagram::Split(header, body) => {
                self.stats.split_seen += 1;
                // Reassemble the datagram, then run it back through ingest.
                if let Some(full) = self.split.feed(header, body) {
                    self.stats.split_completed += 1;
                    self.ingest(&full)
                } else {
                    vec![]
                }
            }
            Datagram::Sequenced(_, _) => {
                let Some((header, body)) = self.chan.read(datagram) else {
                    self.stats.read_rejected += 1;
                    return vec![];
                };
                if !header.fragment {
                    self.stats.plain += 1;
                    // The whole body is a message stream.
                    return match maybe_decompress(body) {
                        Ok(msg) if !msg.is_empty() => {
                            self.note_message(&msg);
                            vec![msg]
                        }
                        _ => vec![],
                    };
                }
                // Fragmented: a two-stream header, then each present stream's
                // chunk concatenated in order.
                self.stats.frag_seen += 1;
                let Some(frags) = Fragments::parse(&body) else {
                    return vec![];
                };
                let mut cursor = frags.len;
                let mut out = Vec::new();
                for (i, slot) in frags.streams.iter().enumerate() {
                    let Some(info) = slot else { continue };
                    let end = (cursor + info.size as usize).min(body.len());
                    let chunk = &body[cursor..end];
                    cursor = end;
                    if std::env::var_os("RUB_FRAGTRACE").is_some()
                        || std::env::var_os("RUBOTS_FRAGTRACE").is_some()
                        || std::env::var_os("REB_FRAGTRACE").is_some()
                        || std::env::var_os("REBOTS_FRAGTRACE").is_some()
                        || std::env::var_os("AIPLAYERS_FRAGTRACE").is_some()
                    {
                        eprintln!(
                            "    frag s{i} idx={}/{} size={} chunkbytes={} held={}",
                            info.index(),
                            info.total(),
                            info.size,
                            chunk.len(),
                            self.frag[i].received(),
                        );
                    }
                    if let Some(assembled) = self.frag[i].push(*info, chunk) {
                        self.stats.frag_completed += 1;
                        // No acknowledgement bookkeeping here: the reliable bit
                        // is toggled per *packet* in `NetChannel::read`, which
                        // is what the engine does. Acknowledging once per
                        // reassembled message instead was tried and breaks the
                        // signon outright (live test: connect timeout).
                        if let Ok(msg) = maybe_decompress(assembled) {
                            if !msg.is_empty() {
                                self.note_message(&msg);
                                out.push(msg);
                            }
                        }
                    }
                }
                out
            }
        }
    }

    /// Drive handshake → signon over `t`, returning once the signon has been
    /// walked (delta tables learned) or the deadline passes.
    ///
    /// After the handshake the server will not send the signon until it sees a
    /// reliable `new`; we send it, then keep sending small acking packets so
    /// the server keeps streaming fragments.
    pub fn connect_and_signon<T: Transport>(
        &mut self,
        t: &mut T,
        timeout: Duration,
    ) -> Result<&Signon, Disconnect> {
        let deadline = Instant::now() + timeout;

        // 1) Connectionless handshake, reusing Client's proven logic.
        self.client.run_handshake(t, timeout)?;
        if self.client.state() != State::Connected {
            return Err(Disconnect::Timeout);
        }
        self.phase = Phase::Signon;

        // 2) Ask for the signon and collect it.
        self.request_new();
        self.collect_signon(t, deadline)?;
        Ok(self.signon.as_ref().unwrap())
    }

    /// Queue `clc_stringcmd "new"`, the request that makes the server send the
    /// signon burst. Reliable, so it is retransmitted until acknowledged.
    fn request_new(&mut self) {
        self.chan.queue_reliable(&Self::stringcmd("new"));
    }

    /// Collect the signon burst until the `usercmd_t` table has been learned.
    ///
    /// Shared by the first connect and by [`resignon`](Self::resignon), because
    /// the engine treats them identically: `Host_Reconnect_f` does not redo the
    /// handshake, it clears the netchannel and writes `clc_stringcmd "new"`.
    fn collect_signon<T: Transport>(
        &mut self,
        t: &mut T,
        deadline: Instant,
    ) -> Result<(), Disconnect> {
        let mut last_ack = Instant::now();
        while Instant::now() < deadline {
            match t.recv() {
                Ok(Some(d)) => {
                    for msg in self.ingest(&d) {
                        // The server's echo prompts (`allow_shaders` /
                        // `allow_autoaim`) arrive during the signon, not after
                        // it, so they have to be answered here as well as in
                        // `pump` — this loop does not go through `pump`.
                        self.echo_stufftexts(&msg);
                        let walked = walk_signon(&msg);
                        // The usercmd_t table is the signal we have the real
                        // signon and can drive moves.
                        if walked.registry.get("usercmd_t").is_some() {
                            self.signon = Some(walked);
                            self.phase = Phase::Running;
                            return Ok(());
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => return Err(Disconnect::Closed),
            }
            // Drive the channel: this carries any in-flight reliable message
            // plus a nop, and is also how our acknowledgements reach the
            // server so it keeps streaming.
            if last_ack.elapsed() >= Duration::from_millis(50) {
                let body = self.idle_body();
                let pkt = self.chan.transmit(&body);
                t.send(&pkt).map_err(|_| Disconnect::Closed)?;
                last_ack = Instant::now();
            }
        }
        Err(Disconnect::Timeout)
    }

    /// Carry out the engine's `reconnect` console command.
    ///
    /// **This is not a re-handshake.** `Host_Reconnect_f`
    /// (`rehlds/engine/host_cmd.cpp`) is, in full:
    ///
    /// ```text
    /// if (cls.state < ca_connected) return;
    /// cls.signon = 0;  cls.state = ca_connected;
    /// Netchan_Clear(&cls.netchan);  SZ_Clear(&cls.netchan.message);
    /// MSG_WriteChar(clc_stringcmd);  MSG_WriteString("new");
    /// ```
    ///
    /// No `getchallenge`, no `connect`, no new UDP socket, no certificate — the
    /// same netchannel is reset and the signon is re-run on it. That matters
    /// twice over: it is why a real client's `reconnect` can never collide with
    /// its own slot (there is no second `connect` for `SV_ConnectClient` to
    /// match, and Reunion's `IDClientsLimit` is never consulted), and it is why
    /// the sequence numbers must be left alone — the engine does not reset
    /// them, so neither do we.
    ///
    /// The server has done the mirror-image reset before sending us the
    /// command: `SV_ActivateServer` calls `Netchan_Clear(&cl->netchan)` and
    /// then writes the stufftext (`sv_main.cpp:6217-6222`), having already run
    /// `SV_InactivateClients` to clear `active`/`spawned`/`fully_connected` and
    /// each client's customization list (`sv_main.cpp:7702-7729`). Everything
    /// we learned from the old signon — delta tables, user messages, baselines,
    /// our own client data — belongs to a server instance that no longer
    /// exists, so all of it is dropped here.
    pub fn reconnect(&mut self) {
        self.chan.clear();
        self.pending_reconnect = false;
        self.phase = Phase::Signon;
        // `signon` is deliberately KEPT until the new burst replaces it.
        //
        // The engine does the same: `Host_Reconnect_f` clears the netchannel
        // and the signon counter, and nothing in it touches the delta
        // descriptions -- those live in the global list `Delta_ParseDescription`
        // registered them into and simply get re-registered when the new
        // `svc_serverinfo` arrives. Keeping ours matters for one concrete
        // reason: [`idle_body`] needs the `usercmd_t` table to build a
        // `clc_move`, and without it we would fall back to `clc_nop` for the
        // whole re-signon -- three seconds of exactly the silence that gets a
        // client flagged (see `idle_body`). A real client never goes quiet on a
        // level change; it has had the table since the *first* signon.
        self.sender = None;
        self.resource_message = None;
        self.spawn_uploaded = false;
        self.clientdata = None;
        self.decoder = None;
        self.last_valid_frame = None;
        self.user_msgs.clear();
        self.cmd_history.clear();
        self.frag = Default::default();
        self.split = Default::default();
        self.request_new();
    }

    /// [`reconnect`](Self::reconnect), then drive the new signon to completion.
    ///
    /// Returns the map name the server came back with, so a caller can notice
    /// that a level change moved it somewhere its navigation does not cover.
    pub fn resignon<T: Transport>(
        &mut self,
        t: &mut T,
        timeout: Duration,
    ) -> Result<String, Disconnect> {
        self.reconnect();
        let deadline = Instant::now() + timeout;
        self.collect_signon(t, deadline)?;
        Ok(self
            .signon
            .as_ref()
            .and_then(|s| s.server_info.as_ref())
            .map(|si| si.map_name().to_string())
            .unwrap_or_default())
    }

    /// Everything a level change asks of a client, end to end.
    ///
    /// [`resignon`](Self::resignon) puts us back through the signon; the server
    /// then wants the same post-signon exchange it wanted the first time,
    /// because `SV_InactivateClients` cleared `m_bSentNewResponse`, `spawned`
    /// and `fully_connected` (`sv_main.cpp:7702-7729`). Team and class are NOT
    /// re-chosen: ReGameDLL keeps the player's team across a level change, so
    /// `jointeam` here would be a second team change and be refused.
    ///
    /// Returns the map the server is now running.
    pub fn rejoin_after_reconnect<T: Transport>(
        &mut self,
        t: &mut T,
        timeout: Duration,
    ) -> Result<String, Disconnect> {
        let map = self.resignon(t, timeout)?;
        let deadline = Instant::now() + timeout;

        self.send_command(Self::SENDRES);
        while Instant::now() < deadline && self.resource_message.is_none() {
            self.pump_idle(t).map_err(|_| Disconnect::Closed)?;
        }
        self.upload_resource_list();
        let settle = Instant::now() + Duration::from_millis(600);
        while Instant::now() < settle {
            self.pump_idle(t).map_err(|_| Disconnect::Closed)?;
        }

        let spawncount = self
            .resource_message
            .as_ref()
            .map(|r| r.spawncount)
            .or_else(|| self.recorded.iter().find_map(|m| Self::spawncount_from(m)))
            .unwrap_or(1);
        self.start_decoding();
        self.enter_game(t, spawncount, Duration::from_secs(12))
            .map_err(|_| Disconnect::Closed)?;
        Ok(map)
    }

    /// Once running, the `MoveSender` bound to the learned `usercmd_t` table.
    ///
    /// Lazily created from the signon; `None` until [`connect_and_signon`] has
    /// reached [`Phase::Running`].
    pub fn move_sender(&mut self) -> Option<&mut MoveSender> {
        if self.sender.is_none() {
            let table = self.signon.as_ref()?.registry.get("usercmd_t")?.clone();
            // The MoveSender drives the same netchannel we have been using, so
            // hand it a clone seeded at the current sequence; callers that send
            // moves should go through it thereafter.
            self.sender = Some(MoveSender::new(self.chan.clone(), table));
        }
        self.sender.as_mut()
    }
}

impl Session {
    /// Convenience constructor for a default identity with a chosen name.
    pub fn named(name: &str) -> Self {
        Self::new(Identity {
            name: name.to_string(),
            ..Identity::default()
        })
    }

    /// The command sequence a real CS 1.6 client sends after the signon, in
    /// order, recovered by decoding a genuine client's session
    /// (`scratchpad/capture.bin`, 24,723 client packets).
    ///
    /// **There is no `spawn` and no `begin`.** Those are Quake-era protocol;
    /// protocol 48 does not use them, which is why every `spawn` attempt was
    /// answered with `spawn is not valid`. The real list is short:
    ///
    /// `allow_shaders 0` / `allow_autoaim 0` are deliberately **not** here:
    /// they are echoes of server stufftexts and are answered as they arrive by
    /// [`echo_stufftexts`](Self::echo_stufftexts). Sending them unprompted
    /// draws an immediate `svc_disconnect`.
    /// * `sendents` — ask for the entity world.
    /// * `specmode 2` / `unpause \n` — spectator/pause state the client sets.
    /// * `jointeam` / `joinclass` — the actual team and class choice. This is
    ///   what takes the bot off `UNASSIGNED`.
    /// * `VModEnable 1` — voice mod handshake.
    pub const SIGNON_TAIL: [&'static str; 3] = ["sendents", "specmode 2", "unpause \n"];

    /// The command the client sends *first*, ~0.3 s after `new` and before
    /// anything else: it asks the server for the resource list.
    ///
    /// Recovered late because it is easy to miss. The real client's packet body
    /// is exactly `03 73 65 6e 64 72 65 73` — `\x03` then `sendres`, eight
    /// bytes with **no NUL terminator** — so a stringcmd walker that insists on
    /// a NUL silently skips it. That is precisely what hid it from the first
    /// scan of the capture.
    pub const SENDRES: &'static str = "sendres";

    /// Queue the post-signon command tail a real client sends.
    pub fn queue_signon_tail(&mut self) {
        for cmd in Self::SIGNON_TAIL {
            self.send_command(cmd);
        }
    }

    /// Has the server started streaming to us?
    ///
    /// Nothing announces it; the proof is that ordinary server traffic keeps
    /// arriving. `sendents` is what flips the server's `fully_connected` flag
    /// — ReHLDS `SV_SendEnts_f` is the **only** place a real client sets it —
    /// and until it does, `SV_SendClientMessages` never picks the client up:
    ///
    /// ```text
    /// if (cl->active && cl->spawned && cl->fully_connected && ...)
    ///     cl->send_message = TRUE;
    /// ```
    ///
    /// Meanwhile `SV_UpdateToReliableMessages` keeps appending to
    /// `netchan.message` until it trips `SIZEBUF_OVERFLOWED` and drops the
    /// client with `Reliable channel overflowed`. That drop is therefore not a
    /// netchannel fault — it is the symptom of never becoming fully connected.
    pub fn server_is_streaming(&self) -> bool {
        self.phase == Phase::Running && self.stats.plain > 0
    }

    /// Drive `spawn` → `sendents` until the server is actually streaming.
    ///
    /// `sendents` must be executed while the client is already
    /// `active && spawned && connected` (all three set by `spawn`), so it is
    /// re-sent periodically rather than fired once: only one reliable is in
    /// flight at a time, so anything stuck ahead of it would block it forever.
    pub fn enter_game<T: Transport>(
        &mut self,
        t: &mut T,
        spawncount: u32,
        timeout: Duration,
    ) -> io::Result<bool> {
        // 1) `spawn`, on the fragment stream — plain reliable is ignored.
        self.upload_spawn(spawncount);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && self.chan.fragment_upload_active() {
            self.pump_idle(t)?;
        }

        // 2) `sendents`, until traffic proves we are in.
        let before = self.stats.plain;
        let mut last_send = Instant::now() - Duration::from_secs(1);
        while Instant::now() < deadline {
            if last_send.elapsed() >= Duration::from_millis(400) && self.reliables_settled() {
                self.send_command("sendents");
                last_send = Instant::now();
            }
            self.pump_idle(t)?;
            if self.stats.plain > before + 20 {
                return Ok(true);
            }
        }
        Ok(self.stats.plain > before + 20)
    }

    /// Stufftext commands whose whole purpose is to be echoed straight back.
    ///
    /// A `svc_stufftext` means "run this command". For these two the client's
    /// implementation simply reports the value to the server, so echoing the
    /// literal text is exactly right. Observed live: the server sends
    /// `allow_shaders 0\n` and `allow_autoaim 0\n` during the signon.
    ///
    /// Echo them **only when prompted** — sending either unprompted draws an
    /// immediate `svc_disconnect`.
    pub const ECHO_COMMANDS: [&'static str; 2] = ["allow_shaders", "allow_autoaim"];

    /// Walk an assembled `svc_*` stream and answer the stufftexts we are
    /// expected to echo. Returns the commands echoed.
    ///
    /// **Every stufftext here is located by parsing**, via
    /// [`trace_message`](Self::trace_message). The previous version scanned the
    /// stream for the byte 9 and read a NUL-terminated string from wherever it
    /// found one, which reports a "command" for every 9 that happens to sit in
    /// a delta description, an MD5, a resource hash or a user-message payload.
    /// One live run produced thirty such lines of which exactly one was a real
    /// message. This is the same defect that was fixed in
    /// [`crate::world::Decoder::absorb_baselines`]; it was in two places.
    pub fn echo_stufftexts(&mut self, msg: &[u8]) -> Vec<String> {
        let trace = self.trace_message(msg);
        let mut echoed = Vec::new();
        for text in trace.stufftexts() {
            // Every console command the server pushes at us, verbatim. An
            // anticheat's whole interface to a client is stufftext, so this is
            // the only place its demands are visible -- and a command we do not
            // recognise is silently dropped, which is indistinguishable from a
            // server that never asked for anything.
            if std::env::var_os("RUB_TRACE_STUFF").is_some()
                || std::env::var_os("RUBOTS_TRACE_STUFF").is_some()
                || std::env::var_os("REB_TRACE_STUFF").is_some()
                || std::env::var_os("REBOTS_TRACE_STUFF").is_some()
                || std::env::var_os("AIPLAYERS_TRACE_STUFF").is_some()
            {
                eprintln!("  <<stufftext>> {:?}", text.trim());
            }
            let head = text.split_whitespace().next().unwrap_or("");
            if head == "reconnect" {
                // Cannot be acted on here: this runs inside the receive path,
                // which does not own the transport. Latch it and let the frame
                // loop (or an explicit `resignon`) carry it out.
                self.pending_reconnect = true;
                echoed.push("reconnect".to_string());
                continue;
            }
            if Self::ECHO_COMMANDS.contains(&head) {
                let cmd = text.trim_end_matches(['\n', '\r']).to_string();
                self.send_command(&cmd);
                echoed.push(cmd);
            }
        }
        echoed
    }

    /// Decode one fully-assembled `svc_*` stream into the messages it contains.
    ///
    /// This is [`crate::stream::walk`] driven in a loop: the byte walker sizes
    /// everything it can and halts on the messages it cannot size byte-wise,
    /// and [`step_over_packed`](Self::step_over_packed) then measures those by
    /// *parsing* them and hands the walk back its resume offset. Nothing is
    /// ever found by searching for an opcode value, so a `9` inside an MD5 or a
    /// delta description is never mistaken for `svc_stufftext`.
    ///
    /// It is also where the user-message registrations are learned: an
    /// `svc_newusermsg` the walk stepped over is a registration at a real
    /// message boundary, so the table it builds cannot contain a phantom entry
    /// the way a scan for the raw byte 39 can.
    ///
    /// Where it stops matters and is reported rather than papered over. The
    /// truly bit-packed messages (`svc_clientdata`, `svc_packetentities`,
    /// `svc_spawnbaseline`, …) need the delta tables and the baselines to
    /// measure, which is [`crate::world::Decoder`]'s job; this walk halts on
    /// them with `stopped_on` set. That costs nothing for the traffic this
    /// function exists to read: a netchannel packet is
    /// `[reliable messages][unreliable datagram]` and every stufftext the
    /// engine writes goes on the **reliable** side (`sv_main.cpp:1584`,
    /// `sv_upld.cpp:82`, `sv_user.cpp:1961`, `SV_BroadcastCommand`
    /// `sv_main.cpp:5940`), so it is always in front of the bit-packed block.
    pub fn trace_message(&mut self, msg: &[u8]) -> StreamTrace {
        let mut out = StreamTrace::default();
        let mut at = 0usize;
        loop {
            let walk = crate::stream::walk(&msg[at..], &self.user_msgs);
            for item in &walk.items {
                self.learn_user_message(item);
            }
            out.items.extend(walk.items);
            let stop = at + walk.stopped_at;
            let Some(op) = walk.stopped_on else {
                out.stopped_at = stop;
                return out;
            };
            // A registration we just learned from this very stream. The table
            // is fixed for the duration of one `stream::walk` call, so a server
            // that registers a user message and then sends one in the same
            // burst halts the first pass on an id we now know. Resume rather
            // than give up -- `stop > at` guarantees progress, so this cannot
            // spin.
            if stop > at && self.user_msgs.contains_key(&op) {
                at = stop;
                continue;
            }
            match Self::step_over_packed(msg, stop) {
                Some(next) if next > stop && next <= msg.len() => {
                    out.items.push(crate::stream::Item::Engine {
                        id: op,
                        payload: msg[stop + 1..next].to_vec(),
                    });
                    at = next;
                    if at >= msg.len() {
                        out.stopped_at = at;
                        return out;
                    }
                }
                _ => {
                    out.stopped_at = stop;
                    out.stopped_on = Some(op);
                    return out;
                }
            }
        }
    }

    /// Register an `svc_newusermsg` the walk stepped over.
    ///
    /// Payload is `u8 id`, `u8 size`, then a fixed 16-byte name field
    /// (`SV_SendUserReg`, `rehlds/engine/sv_main.cpp:1495-1507`). A size of 255
    /// means the game DLL registered it with `-1`: variable length, with a
    /// leading length byte.
    fn learn_user_message(&mut self, item: &crate::stream::Item) {
        let crate::stream::Item::Engine { id, payload } = item else {
            return;
        };
        if *id != crate::svc::SVC_NEWUSERMSG || payload.len() < 18 {
            return;
        }
        let name: String = payload[2..18]
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as char)
            .collect();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_graphic()) {
            return;
        }
        self.user_msgs.insert(
            payload[0],
            crate::stream::UserMsgDef {
                name,
                size: payload[1],
            },
        );
    }

    /// Measure one bit-packed-but-byte-aligned message at `at`, returning the
    /// offset just past it.
    ///
    /// Only the three the *reliable* signon burst is made of, because those are
    /// what stands between the start of a stream and the stufftexts behind
    /// them. `SV_New_f` (`rehlds/engine/sv_main.cpp:1509-1594`) builds its
    /// reply as `svc_serverinfo` (with the seven `svc_deltadescription`s
    /// inside `SV_SendServerinfo`), then the `svc_newusermsg` registrations,
    /// then `svc_stufftext "fullserverinfo …"` — so a walk that cannot step
    /// over a delta description never reaches a single stufftext in the signon.
    /// `svc_resourcelist` is the same story for the reply to `sendres`.
    ///
    /// Each is measured by parsing it with the same code that consumes it for
    /// real, and every bit region ends byte-aligned, which is what makes the
    /// resume offset exact rather than a guess. A parse that does not produce
    /// what the header promised returns `None`, which halts the walk instead of
    /// resuming it somewhere wrong.
    fn step_over_packed(msg: &[u8], at: usize) -> Option<usize> {
        let id = *msg.get(at)?;
        let mut r = crate::messages::Reader::new(msg);
        r.seek(at + 1);
        if r.pos() != at + 1 {
            return None;
        }
        match id {
            // int protocol, int spawncount, int crc, 16-byte dll md5, three
            // bytes, three strings, then the map cycle and a trailing flag.
            crate::svc::SVC_SERVERINFO => {
                crate::messages::ServerInfo::parse(&mut r)?;
                r.cstr()?;
                r.u8()?;
                Some(r.pos())
            }
            // string name, u16 field count, then the packed field table.
            crate::svc::SVC_DELTADESCRIPTION => {
                r.cstr()?;
                let lo = r.u8()?;
                let hi = r.u8()?;
                let count = usize::from(u16::from_le_bytes([lo, hi]));
                let base = r.pos();
                let mut br = proto::bitbuf::BitReader::new(msg.get(base..)?);
                let fields = proto::delta::parse_description(&mut br, count);
                if fields.len() != count {
                    return None;
                }
                Some(base + br.byte_pos() + usize::from(br.bit_offset() > 0))
            }
            // Entries and the consistency demands share ONE bit block, so both
            // have to be read to know where the block ends.
            crate::svc::SVC_RESOURCELIST => {
                let base = r.pos();
                let mut br = proto::bitbuf::BitReader::new(msg.get(base..)?);
                let (list, _consistency) = proto::resources::parse_resource_list_full(&mut br);
                if list.is_empty() || br.overflowed() {
                    return None;
                }
                Some(base + br.byte_pos() + usize::from(br.bit_offset() > 0))
            }
            _ => None,
        }
    }

    /// `clc_resourcelist` declaring no custom resources: opcode, then `short 0`.
    ///
    /// Replaying a captured real client's list (which declares its spray decal)
    /// is actively dangerous here. `SV_ParseResourceList` drops the client with
    /// `"Too many resources in client resource list"` for `total > 1`
    /// (`sv_upld.cpp:401-407`), and validates every entry hard: it must be
    /// `t_decal`, carry `RES_CUSTOM`, be named exactly `tempdecal.wad`, and
    /// have a non-zero size under 1 GiB (`:427-441`). A bot has no spray to
    /// declare, so it declares nothing — which is precisely what the reference
    /// client does in reply to `svc_resourcerequest`
    /// (`HLTV/Core/src/Server.cpp:921-922`).
    pub const CLC_RESOURCELIST: [u8; 3] = [netchan::clc::RESOURCELIST, 0x00, 0x00];

    /// Answer `svc_resourcerequest`. Must come after `sendres`, before `spawn`.
    ///
    /// Sent as a plain reliable message, like the reference client.
    /// `Netchan_Process` hands whole and reassembled messages to the same
    /// parser, so there is nothing the fragment stream would add.
    pub fn upload_resource_list(&mut self) {
        self.chan.queue_reliable(&Self::CLC_RESOURCELIST);
    }

    /// Build the `clc_fileconsistency` upload that carries `spawn`.
    ///
    /// This is the step that turns a netchannel peer into an actual player.
    /// Recovered by reassembling a real client's 12-fragment upload: the
    /// message is `clc_fileconsistency` followed, **at its very end**, by a
    /// `clc_stringcmd` holding `spawn <spawncount> <n>` — never a standalone
    /// packet. It has to go out through
    /// [`queue_fragmented`](netchan::NetChannel::queue_fragmented).
    ///
    /// `spawncount` is the first `u32` of the `svc_resourcerequest` payload;
    /// ReHLDS `SV_Spawn_f` rejects anything else with `spawn is not valid`.
    ///
    /// `consistency` is the bit-packed body; an empty slice declares no
    /// entries, which is all a server with enforcement disabled needs.
    ///
    /// **Layout:** `clc_fileconsistency` is followed by a **`u16` byte length**
    /// and then exactly that many bytes of payload. Getting this wrong is
    /// loud and specific — HLDS answers
    /// `SV_ParseConsistencyResponse: … sent invalid message length: 768`
    /// (`0x0300`, i.e. it read the next two bytes as the length) and drops the
    /// client with `Invalid ParseConsistency message`.
    ///
    /// Cross-checked against the real client's upload, which opens
    /// `07 6d 05` — length `0x056d` = 1389, so the payload runs from offset 3
    /// to 1392, which is exactly where its trailing `clc_stringcmd` begins.
    pub fn build_spawn_upload(spawncount: u32, crc: i32, consistency: &[u8]) -> Vec<u8> {
        let mut msg = vec![netchan::clc::FILECONSISTENCY];
        // The payload is MUNGED with the spawncount as key: ReHLDS
        // `SV_ParseConsistencyResponse` does
        // `COM_UnMunge(&net_message.data[msg_readcount], value, g_psvs.spawncount)`
        // before it starts reading bits. Sending it in the clear is what
        // produced `sent bad file data` -- the server unmunged our zeroes into
        // garbage, read a nonsense resource index and bailed.
        //
        // Bit layout after unmunging: `while (MSG_ReadBits(1)) { idx =
        // MSG_ReadBits(12); ... }` — so a single **zero bit** means "no
        // entries", the loop never runs, the mismatch counter stays 0, and the
        // server clears `has_force_unmodified`, which is what lets `spawn`
        // through. Four zero bytes give that zero bit and a whole dword for
        // munge to work on (it only transforms `len & ~3`).
        // Size matters: `MSG_EndBitReading` advances the read cursor by the
        // bits the loop actually CONSUMED, not by the declared length. Declare
        // more than the reader eats and the leftover bytes are parsed as
        // opcodes — `badread on opcode clc_fileconsistency`.
        //
        // "No entries" is a single zero bit, which consumes exactly one byte.
        // `COM_Munge` only transforms whole dwords (`len & ~3`), so a one-byte
        // payload passes through untouched and the spawncount key is moot.
        let mut payload = if consistency.is_empty() {
            vec![0u8; 1]
        } else {
            consistency.to_vec()
        };
        let n = payload.len() & !3;
        if n > 0 {
            proto::munge::munge(&mut payload[..n], &proto::munge::TABLE1, spawncount as i32);
        }

        msg.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        msg.extend_from_slice(&payload);

        // A real client appends `spawn` to the tail of this same message.
        //
        // The second argument is the map CRC, and it is NOT optional. See
        // `proto::munge::spawn_crc`: the server unmunges whatever we send and
        // compares it against `worldmapCRC` every 5 seconds
        // (`SV_CheckMapDifferences`, sv_main.cpp:8092-8115). A literal `0`
        // unmunges to garbage -- never to 0 -- so the comparison always failed
        // and the client was always dropped for "Reliable channel overflowed",
        // roughly five seconds after every otherwise-successful spawn.
        msg.push(netchan::clc::STRINGCMD);
        msg.extend_from_slice(format!("spawn {spawncount} {crc}").as_bytes());
        msg.push(0);
        msg
    }

    /// Every resource the server has advertised, from anywhere in the stream.
    ///
    /// The list does **not** arrive inside the signon burst — the server sends
    /// `svc_resourcerequest` + `svc_resourcelist` as their own message after
    /// `sendres`, so `Signon::resources` is empty and the consistency response
    /// would be built from nothing. Scan everything we recorded instead.
    pub fn all_resources(&self) -> Vec<proto::resources::Resource> {
        for msg in &self.recorded {
            if let Some(pos) = msg.iter().position(|&b| b == crate::svc::SVC_RESOURCELIST) {
                let mut r = proto::bitbuf::BitReader::new(&msg[pos + 1..]);
                let list = proto::resources::parse_resource_list(&mut r);
                if !list.is_empty() {
                    return list;
                }
            }
        }
        self.signon
            .as_ref()
            .map(|s| s.resources.clone())
            .unwrap_or_default()
    }

    /// Build the `clc_fileconsistency` body the server's list asks for, or
    /// `None` when it is not asking.
    ///
    /// **`None` means send nothing at all.** It does not mean "send an empty
    /// response". `SV_ParseConsistencyResponse` requires the entry count to
    /// equal `g_psv.num_consistency` (`sv_user.cpp:194`), and that stays at
    /// ~62 even under `mp_consistency 0` — the cvar only stops the server
    /// *asking*, it does not zero the counter. Answering an unasked question
    /// with zero entries is exactly the `"Bad file data"` drop this client
    /// spent a long time hitting.
    ///
    /// Bounds demands are answered by echoing the server's own bounds back;
    /// exact-file demands need a real MD5 from [`crate::content`]. See
    /// [`proto::consistency`] for why that is sound.
    pub fn build_consistency(&self) -> Option<Vec<u8>> {
        let msg = self.resource_message.as_ref()?;
        if !msg.consistency.should_send {
            return None;
        }
        // Always answer every listed index — skipping missing resources shortens
        // the response vs `num_consistency` and yields "Bad file data". Prefer
        // decoding bounds; fall back to exact-file hash (0 if content missing).
        let demands = proto::consistency::demands(&msg.resources, &msg.consistency, msg.spawncount);

        let mut answers = Vec::with_capacity(demands.len());
        for d in &demands {
            let answer = match d {
                proto::consistency::Demand::Bounds {
                    mins, maxs, check, ..
                } => {
                    // force_model_specifybounds_if_avail accepts "unavailable"
                    // as mins=maxs=(-1,-1,-1). Use that when we have no model.
                    if *check == proto::consistency::ForceType::ModelSpecifyBoundsIfAvail {
                        // Still echo server bounds when we can (always have them).
                        proto::consistency::Answer::Bounds(*mins, *maxs)
                    } else {
                        proto::consistency::Answer::Bounds(*mins, *maxs)
                    }
                }
                proto::consistency::Demand::ExactFile { path, .. } => {
                    // First 4 bytes of MD5 as LE u32 (`SV_CheckConsistencyResponse`).
                    let hash = self
                        .content
                        .as_ref()
                        .and_then(|c| c.md5(path))
                        .map(|m| u32::from_le_bytes([m[0], m[1], m[2], m[3]]))
                        .unwrap_or(0);
                    proto::consistency::Answer::Hash(hash)
                }
            };
            answers.push((d.index(), answer));
        }
        // If the resource list omitted an index (shouldn't), pad with empty
        // hashes so the entry count still matches — length must stay valid.
        while answers.len() < msg.consistency.indices.len() {
            let idx = msg.consistency.indices[answers.len()];
            answers.push((idx, proto::consistency::Answer::Hash(0)));
        }
        Some(proto::consistency::build_body(&answers))
    }

    /// Send `spawn` on the fragment stream — the step that makes the server
    /// create a player entity for us.
    ///
    /// **This is the one that works, and it is minimal:** just
    /// `clc_stringcmd "spawn <spawncount> 0"` delivered as a fragmented
    /// message. No `clc_fileconsistency` wrapper is required — a real client
    /// appends `spawn` to the tail of its consistency upload, but what the
    /// server actually insists on is that `spawn` arrive **on the fragment
    /// stream**. Sent as an ordinary reliable message it is silently ignored,
    /// which is why nothing the client asked for ever took effect.
    ///
    /// Verified live: with this, `say` reaches the chat
    /// (`*DEAD* AIPlayer : …` in the server log) — the first time any command
    /// after `new` was ever executed.
    pub fn upload_spawn(&mut self, spawncount: u32) {
        self.spawn_uploaded = true;
        let crc = self.spawn_crc(spawncount);
        let msg = match self.build_consistency() {
            Some(body) => Self::build_spawn_upload(spawncount, crc, &body),
            // The server is not asking. Send `spawn` on its own -- answering
            // anyway is a drop, not a no-op.
            None => Self::build_spawn_only(spawncount, crc),
        };
        self.chan.queue_fragmented(&msg);
    }

    /// `clc_stringcmd "spawn <spawncount> <crc>"`, with no consistency wrapper.
    pub fn build_spawn_only(spawncount: u32, crc: i32) -> Vec<u8> {
        let mut msg = vec![netchan::clc::STRINGCMD];
        msg.extend_from_slice(format!("spawn {spawncount} {crc}").as_bytes());
        msg.push(0);
        msg
    }

    /// The `<crc>` argument for `spawn`, derived from the signon we received.
    ///
    /// Returns 0 only if we somehow have no `svc_serverinfo`, which cannot
    /// happen on a real signon — and 0 is the value that gets us dropped, so a
    /// regression here is loud rather than silent.
    pub fn spawn_crc(&self, spawncount: u32) -> i32 {
        self.signon
            .as_ref()
            .and_then(|s| s.server_info.as_ref())
            .map(|si| proto::munge::spawn_crc(si.map_crc, si.player_index, spawncount))
            .unwrap_or(0)
    }

    /// The server's real map CRC, for cross-checking against its own
    /// `Started map "<name>" (CRC "<n>")` log line.
    pub fn world_map_crc(&self) -> Option<i32> {
        let si = self.signon.as_ref()?.server_info.as_ref()?;
        Some(proto::munge::world_map_crc(si.map_crc, si.player_index))
    }

    /// Spawncount advertised by `svc_resourcerequest`, once seen.
    ///
    /// The message arrives inside a signon stream rather than as its own
    /// record, so it has to be searched for by opcode.
    pub fn spawncount_from(msg: &[u8]) -> Option<u32> {
        if msg.first() == Some(&crate::svc::SVC_RESOURCEREQUEST) && msg.len() >= 5 {
            return Some(u32::from_le_bytes(msg[1..5].try_into().ok()?));
        }
        None
    }

    /// Queue the team/class choice, as a real client does a second or two after
    /// the signon tail. `team` is 1 (terrorist) or 2 (counter-terrorist);
    /// `class` is the model choice, 1-5.
    pub fn queue_join(&mut self, team: u8, class: u8) {
        self.send_command(&format!("jointeam {team}"));
        self.send_command(&format!("joinclass {class}"));
        self.send_command("specmode 2");
        self.send_command("VModEnable 1");
    }

    /// Gap a real client leaves between the join commands.
    ///
    /// Measured from a genuine session: `jointeam` 3.17 s, `joinclass` 3.33 s,
    /// `specmode` 3.43 s, `VModEnable` 3.58 s — roughly 150 ms apart.
    pub const JOIN_STEP: Duration = Duration::from_millis(200);

    /// How long to wait for a spawn before re-sending the join pair. The
    /// server only advances the join state inside its own `PlayerThink`, so
    /// this has to cover several server frames, not just a round trip.
    /// Gap between the team menu arriving and `jointeam`.
    pub const JOIN_SETTLE: Duration = Duration::from_millis(400);

    /// How long to wait for the server to offer the team menu.
    pub const JOIN_MENU_WAIT: Duration = Duration::from_millis(6000);

    pub const JOIN_RETRY: Duration = Duration::from_millis(600);

    /// Walk the join sequence with a real client's spacing, pumping between
    /// each step so the server can drain what the previous one produced.
    ///
    /// **Why the spacing matters:** each of these commands makes the game DLL
    /// queue a burst of reliable user messages, and the engine only empties
    /// `netchan.message` into `reliable_buf` once the previous reliable has
    /// been acknowledged. Firing them back to back stacks the bursts into one
    /// buffer — seen on the wire as a single 1232-byte reply followed
    /// immediately by `Reliable channel overflowed`, where a real client's
    /// reply to the same command was only 92 bytes.
    pub fn join_team<T: Transport>(&mut self, t: &mut T, team: u8, class: u8) -> io::Result<()> {
        for cmd in [
            format!("jointeam {team}"),
            format!("joinclass {class}"),
            "specmode 2".to_string(),
            "VModEnable 1".to_string(),
        ] {
            self.send_command(&cmd);
            let until = Instant::now() + Self::JOIN_STEP;
            while Instant::now() < until {
                self.pump_idle(t)?;
            }
        }
        Ok(())
    }

    /// Run the bot for one frame, draining any console commands it asks for.
    ///
    /// Returns `None` when there is nothing to think with — no brain, or no
    /// decoded world yet — so the caller's own intent stands.
    fn think(&mut self) -> Option<bot::Intent> {
        let now = Instant::now();
        let dt = self
            .last_think
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.25);
        self.last_think = Some(now);

        let d = self.decoder.as_ref()?;
        self.brain.as_ref()?;

        // Round-trip latency: everything in the world view is this stale, and
        // the aim layer needs to know in order to lead a moving target.
        let latency = 0.0;
        let rescue = self
            .map
            .as_ref()
            .map(|m| m.info.rescue_zones.iter().map(|z| z.centre()).collect())
            .unwrap_or_default();
        // Measured, not reported: this server does not transmit our velocity,
        // and every weapon's accuracy is decided by it. Taken before the
        // borrow of `map`, because `measured_speed` needs `&mut self`.
        let origin = d
            .clientdata
            .as_ref()
            .map(|c| c.origin())
            .unwrap_or([0.0; 3]);
        let speed = self.measured_speed(origin, now);

        let d = self.decoder.as_ref()?;
        // LOS through world + brush entities (func_wall/crates). Raw BSP alone
        // wallbangs through dust2 cover.
        let world = if let Some(m) = self.map.as_ref() {
            let sight = crate::view::BrushSight {
                bsp: &m.bsp,
                brushes: &m.info.solid_brushes,
            };
            crate::view::project(
                d,
                rescue,
                Some(&sight as &dyn crate::view::Sight),
                latency,
                speed,
            )
        } else {
            crate::view::project(d, rescue, None, latency, speed)
        };

        // Turn the objective into the NEXT waypoint. Steering straight at a
        // distant goal walks into walls -- on de_dust2 the straight line from
        // a T spawn to bombsite B crosses most of the map.

        // Route to whatever the brain last decided it wanted, falling back to
        // the map objective. Without this the navigation always heads for the
        // bomb site even while the brain is trying to reach something else --
        // a dropped bomb, or a planted one to defuse -- so the bot arrives
        // nowhere in particular and the objective machine looks broken.
        //
        // One tick stale by construction: the brain has not run yet this frame.
        // At 50 Hz that is 20 ms of lag on a destination that moves when
        // somebody dies, which is not worth restructuring the frame for.
        //
        // `nav_goal` outranks the bomb objective because it is the rung that
        // actually answered last tick saying where it wants to go. On a hostage
        // map `objective.target` is permanently `None` -- `ObjectiveState` is
        // the bomb machine and a CT with no planted bomb has nothing to say --
        // so without this the route is pinned to the map's declared objective,
        // a hostage spawn, for the entire walk back to the rescue zone.
        // Where the FEET are going. The brain's own target wins -- a hostage or
        // rescue zone, a dropped bomb to retrieve, a planted one to defuse --
        // falling back to the role-aware map objective. A C4 carrier always
        // uses the plant-spot (inside a bomb zone), even if its role is Hold
        // and `site` is an approach ring (plan A1 carrier override).
        // Phase G2: CT rotate when ≥2 visible enemies pressure one site (or bomb planted).
        self.maybe_ct_rotate(&world, dt);

        let map_goal =
            crate::role::active_goal(world.bomb.carried_by_me, self.site, self.plant_spot);
        let route_goal = self
            .brain
            .as_ref()
            .and_then(|b| b.nav_goal.or(b.objective.target))
            .or(map_goal);
        let site = match (self.map.take(), route_goal) {
            // Only advance stuck/progress while the body can actually move.
            // Freezetime used to run next_waypoint with a pinned origin, so
            // unstick_for and origin-stuck climbed for the whole buy phase and
            // the fleet jumped in unison the moment freeze ended.
            (Some(m), Some(goal)) if world.me.alive && !world.me.freeze_period => {
                let w = self
                    .follower
                    .next_waypoint(&m.grid, world.me.origin, goal, dt);
                self.map = Some(m);
                w
            }
            // Dead or freeze: no (allowed) movement, so no stuck clock.
            // Hold the last target instead of manufacturing replan/jump.
            (Some(m), Some(goal)) => {
                self.map = Some(m);
                self.follower.hold();
                Some(goal)
            }
            (m, _) => {
                self.map = m;
                route_goal
            }
        };
        // The goal and the next waypoint are different questions: arrival is
        // about the bomb site, steering is about the route to it. Passing the
        // waypoint as the goal made the bot declare itself on the plant spot at
        // every waypoint it reached.
        // The MAP objective goes to the brain, never the routing goal. Feeding
        // the routing goal back in is a latch: `ObjectiveState::tick` sets its
        // target from whatever it is given, so once anything moved that target
        // -- a dropped bomb lying near spawn, say -- the goal became the
        // target, which became the goal, for the rest of the round. Live, a
        // terrorist sat in its own spawn buy zone reporting `to_goal 25` and
        // `rung plant`, with the real bomb site 3600 units away, holding the C4
        // and pressing nothing, because the server rightly said `bombzone
        // false`.
        // Where the HEAD is going: one or two nodes beyond the feet's target
        // (`PathFollower::look_target`). `None` when the route is done and the
        // brain should look wherever it would otherwise -- the destination.
        let look = match (self.map.as_ref(), site) {
            (Some(m), Some(_)) => self.follower.look_target(&m.grid, world.me.origin),
            _ => None,
        };
        // Natural walker: the follower's weave and micro-pause (a corridor
        // sidemove oscillation and a brief speed dip). Applied on walking
        // rungs; combat/camp/defuse pass defaults. Suppressed entirely while
        // the follower is struggling -- a weave pushing into the same wall is
        // how a stuck bot stays stuck (measured: 49.6% of goto samples were
        // still with vel < 1, most requesting movement). Also suppressed for
        // the first 2 s after a freeze (round-start crowd: bots box each other
        // in; the weave would grind into a teammate).
        if world.me.freeze_period {
            self.post_freeze_grace = 2.0;
        } else {
            self.post_freeze_grace = (self.post_freeze_grace - dt).max(0.0);
        }
        let struggling = self.follower.is_struggling() || self.post_freeze_grace > 0.0;
        let (weave, speed_scale) = match (self.map.as_ref(), site) {
            (Some(m), Some(_)) if !struggling => {
                self.follower.natural_walk(&m.grid, world.me.origin, dt)
            }
            _ => (0.0, 1.0),
        };
        let nav = bot::controller::Nav {
            // Arrival / plant distance uses the active goal (carrier → plant).
            goal: map_goal,
            waypoint: site,
            look,
            // Plan W6: the follower advanced a node this tick, so the brain
            // re-rolls its per-hop slowdown dice.
            new_waypoint: self.follower.took_advanced(),
            // Plan W5: where to defend after arrival, if the route has one.
            defend_point: self.follower.defend_point(),
            // The sight lines into that defend point. Computed here because
            // this is the only layer holding both the nav lattice and the
            // collision hulls: the brain gets an answer, not a map.
            watch_points: self.watch_points(),
            weave,
            speed_scale,
        };
        let mut intent = self.brain.as_mut()?.think(&world, nav, dt);

        // Blocked by geometry the route does not model: strafe, jump, and
        // swing the view off the wall. Without this a solid player walks
        // straight into a door frame and stays there for the rest of the
        // round -- the route stays perfectly valid the whole time, which is
        // what makes it so confusing to watch.
        //
        // ...but NOT while fighting. The follower measures being stuck as "not
        // getting closer to the waypoint", and the combat rung abandons that
        // waypoint on purpose to chase an enemy -- so entering a firefight
        // guarantees "no progress", which fires the nudge, which JUMPS the bot,
        // which is the worst accuracy state in the game (airborne AK spread is
        // `0.04 + 0.4*acc` against `0.0275*acc` standing, `wpn_ak47.cpp:75-86`).
        // It also swings the view up to 60 degrees off the target. Entirely
        // self-inflicted, and invisible until the bots started shooting at each
        // other.
        // Graph hop kinds → buttons. Without this, Jump/Crouch edges are walked
        // as flat run and the bot pins on A lips / boxes while looking at a wall.
        if let Some(m) = self.map.as_ref() {
            match self.follower.required_move(&m.grid) {
                Some(nav::navgrid::Move::Jump) => intent.jump = true,
                Some(nav::navgrid::Move::Crouch) => intent.duck = true,
                _ => {}
            }
        }

        let fighting = self.brain.as_ref().is_some_and(|b| b.rung == "combat");
        if fighting {
            self.follower.hold();
        } else if let Some(u) = self.follower.unstick() {
            if intent.forwardmove != 0.0 || intent.sidemove != 0.0 {
                // Crowd-unstick (round-start T-spawn pile-up): the follower
                // alternates a fixed side, but in a spawn crowd that side may
                // be a TEAMMATE. If the current unstick side is blocked by a
                // same-team player within 60u, push the other way instead --
                // the goal is to find a gap, not to grind into a friend.
                let me = world.me.origin;
                let dir = u.sidemove.signum();
                let mut side_blocked = false;
                for p in &world.players {
                    if p.team == world.me.team && p.alive {
                        let dx = p.origin[0] - me[0];
                        let dy = p.origin[1] - me[1];
                        let d = (dx * dx + dy * dy).sqrt();
                        if d < 60.0 {
                            // Is the teammate on the side we are pushing?
                            // World-space: the view's RIGHT vector for yaw is
                            // `(sin yaw, -cos yaw)` (same basis `move_axes`
                            // decomposes against). Project the teammate offset
                            // onto it.
                            let yaw = f64::from(intent.view.yaw).to_radians();
                            let (sy, cy) = yaw.sin_cos();
                            let right_dot = dx * sy as f32 - dy * cy as f32;
                            if right_dot * dir > 0.0 {
                                side_blocked = true;
                            }
                        }
                    }
                }
                intent.sidemove = if side_blocked {
                    -u.sidemove
                } else {
                    u.sidemove
                };
                // Never BACK UP while stuck: a human stuck at a door strafes
                // sideways, they do not reverse into their own spawn. The
                // unstick's yaw_bias swings the view, and the brain's travel
                // then reads delta > TURN_STOP_ANGLE and emits negative
                // forwardmove -- measured: `fwd -19 side -62 yaw -166`, a bot
                // backing away from a doorway it was walking into. Zero the
                // forward axis so the push is pure sideways.
                if u.yaw_bias.abs() > 0.0 {
                    intent.forwardmove = 0.0;
                }
                // Know what is being jumped at. The unstick's jump used to
                // be a timer with no idea what was in front of it, so a bot
                // pressed against a 96-unit wall hopped at it until the round
                // ended -- measured on de_dust2: ~50 s inside a 150-unit box
                // with the reroute counter past 30. Measure the lip with the
                // engine's own hulls instead: jump only at a height a player
                // can reach, duck when the height needs it, and when nothing
                // clears it say so, so the route goes round.
                if u.jump {
                    match self.obstacle_ahead(world.me.origin, intent.view.yaw) {
                        Some(a) if a.wants_jump() => {
                            intent.jump = true;
                            intent.duck |= a.wants_duck();
                        }
                        Some(a) if a.impassable() => {
                            self.refused_jumps += 1;
                            let due = self
                                .last_refusal_log
                                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
                            if due {
                                self.last_refusal_log = Some(Instant::now());
                                eprintln!(
                                    "      ahead: {a:?} at yaw {:.0} - nothing clears it, routing round instead of jumping (refused {})",
                                    intent.view.yaw, self.refused_jumps
                                );
                            }
                            self.follower.blocked_ahead();
                        }
                        Some(_) => {}
                        // No map loaded: nothing to ask, so keep the old
                        // behaviour rather than never jumping at all.
                        None => intent.jump = true,
                    }
                }
                intent.view.yaw =
                    bot::math::norm_angle(f64::from(intent.view.yaw + u.yaw_bias)) as f32;
            }
        } else if !struggling && !fighting {
            // Phase B / B1: ORCA-lite sidestep + soft brake when a teammate is
            // ahead (CONGA-1). Disabled when struggling.
            let avoid = teammate_avoid_sidemove(
                world.me.origin,
                intent.view.yaw,
                intent.forwardmove,
                intent.sidemove,
                &world.players,
                world.me.team,
            );
            if avoid.abs() > 1.0 {
                intent.sidemove = (intent.sidemove + avoid).clamp(-250.0, 250.0);
            }
            let scale = teammate_forward_scale(
                world.me.origin,
                intent.view.yaw,
                &world.players,
                world.me.team,
            );
            if scale < 0.99 {
                intent.forwardmove *= scale;
            }
        }
        let to_goal = route_goal
            .map(|g| {
                let (dx, dy) = (g[0] - world.me.origin[0], g[1] - world.me.origin[1]);
                (dx * dx + dy * dy).sqrt()
            })
            .unwrap_or(f32::NAN);
        self.view_stats.note(intent.view.yaw, dt);
        self.last_decision = Some(Decision {
            alive: world.me.alive,
            in_game: world.me.freeze_period,
            site,
            node: self.follower.current_node(),
            forwardmove: intent.forwardmove,
            sidemove: intent.sidemove,
            yaw: intent.view.yaw,
            waypoints_left: self.follower.remaining(),
            reroutes: self.follower.reroutes,
            stuck: self.follower.is_stuck(),
            attack: intent.attack,
            use_action: intent.use_action,
            carrying_bomb: world.bomb.carried_by_me,
            bomb_planted: world.bomb.planted,
            bomb_known_at: world.bomb.origin,
            arming: self.brain.as_ref().is_some_and(|b| b.plant.is_arming()),
            to_goal,
            rung: self.brain.as_ref().map_or("none", |b| b.rung),
            look_reversals: self.view_stats.reversals_per_sec(),
            look_dwell: self.view_stats.longest_dwell(),
            escort: self
                .brain
                .as_ref()
                .map_or("none", |b| b.escort.phase.as_str()),
            hostages: world.hostages.len(),
            hostages_led: self.brain.as_ref().map_or(0, |b| b.escort.led_count()),
            to_hostage: self
                .brain
                .as_ref()
                .and_then(|b| b.escort.target)
                .and_then(|e| world.hostages.iter().find(|h| h.entity == e))
                .map(|h| {
                    let (dx, dy) = (
                        h.origin[0] - world.me.origin[0],
                        h.origin[1] - world.me.origin[1],
                    );
                    (dx * dx + dy * dy).sqrt()
                })
                .unwrap_or(f32::NAN),
            use_edges: self.brain.as_ref().map_or(0, |b| b.escort.edges),
            role: self.role.map_or("none", |r| r.as_str()),
            rotate_events: self.rotate_events,
            rotate_site: self.rotate_site,
        });

        let contact_site = self.map.as_ref().and_then(|map| {
            let enemy = world.visible_enemies().next()?;
            let mut nearest = None;
            let mut distance = f32::MAX;
            for (index, site) in map.info.bomb_sites.iter().enumerate() {
                let centre = site.centre();
                let dx = enemy.origin[0] - centre[0];
                let dy = enemy.origin[1] - centre[1];
                let d = dx * dx + dy * dy;
                if d < distance {
                    distance = d;
                    nearest = Some(index);
                }
            }
            nearest.map(|index| {
                if map.info.bomb_sites[index].centre()[2] > 100.0 {
                    bot::PlantSite::A
                } else {
                    bot::PlantSite::B
                }
            })
        });
        let assigned_site = self.assigned_site.and_then(|index| {
            self.map.as_ref()?.info.bomb_sites.get(index).map(|site| {
                if site.centre()[2] > 100.0 {
                    bot::PlantSite::A
                } else {
                    bot::PlantSite::B
                }
            })
        });
        let role = match self.role {
            Some(crate::role::BotRole::Assault) => 1,
            Some(crate::role::BotRole::Hold) => 2,
            Some(crate::role::BotRole::Flank) => 3,
            Some(crate::role::BotRole::Split) => 4,
            None => 0,
        };
        let rung = match self.brain.as_ref().map(|brain| brain.rung) {
            Some("goto") => 1,
            Some("combat") => 2,
            Some("defuse") => 3,
            Some("plant") => 4,
            Some("camp") => 5,
            _ => 0,
        };
        self.latest_team_report = Some(bot::TeamReport {
            bot_id: self.team_bot_id,
            team: world.me.team,
            alive: world.me.alive,
            origin: world.me.origin,
            assigned_site,
            contact_site,
            contact_at: contact_site.map(|_| world.round_time),
            bomb_carrier: world.bomb.carried_by_me,
            bomb_planted: world.bomb.planted,
            bomb_origin: world.bomb.origin,
            observed_at: world.round_time,
            role,
            rung,
        });

        for cmd in &intent.commands {
            if let Some(text) = cmd.to_console() {
                self.console.push(text);
            }
        }
        Some(intent)
    }

    /// Horizontal speed, measured rather than reported.
    ///
    /// `clientdata_t` has a `velocity` field and the server does not send it:
    /// dumping every field that arrives while running at 240 u/s gives eleven,
    /// and velocity is not among them. That is not a decode failure -- a
    /// client with prediction on computes its own velocity, so the server
    /// saves the bits. Trusting the absent field means reading zero, which
    /// makes a bot sprinting across the map look permanently stuck.
    /// Where a bot holding the current defend point should watch.
    ///
    /// `nav::watch::watch_points` is a lattice sweep plus a visibility trace
    /// per direction: far too much to do every tick, and pointless, because
    /// the answer only changes when the defend point does. Cached against it.
    fn watch_points(&mut self) -> [Option<[f32; 3]>; bot::controller::MAX_WATCH] {
        let empty = [None; bot::controller::MAX_WATCH];
        let Some(spot) = self.follower.defend_point() else {
            return empty;
        };
        if let Some((at, cached)) = self.watch_cache {
            let (dx, dy, dz) = (spot[0] - at[0], spot[1] - at[1], spot[2] - at[2]);
            if (dx * dx + dy * dy + dz * dz).sqrt() < 32.0 {
                return cached;
            }
        }
        let Some(map) = self.map.as_ref() else {
            return empty;
        };
        let world = nav::navgrid::World::new(&map.bsp, &map.info);
        let found = nav::watch::watch_points(
            &map.grid,
            &world,
            spot,
            nav::watch::DEFAULT_RADIUS,
            bot::controller::MAX_WATCH,
        );
        let mut out = empty;
        for (slot, point) in out.iter_mut().zip(found) {
            *slot = Some(point);
        }
        self.watch_cache = Some((spot, out));
        out
    }

    /// What is in front of the bot, measured against the engine's own hulls.
    ///
    /// `None` when no map is loaded, which is the honest answer rather than a
    /// guess: without collision there is nothing to measure, and the caller
    /// falls back to the old behaviour instead of refusing to jump at all.
    ///
    /// Re-probed when the bot has moved half a hull width, turned, or a
    /// quarter of a second has passed; a body wedged in one place gets the
    /// same answer every tick and does not need the traces repeated.
    fn obstacle_ahead(&mut self, origin: [f32; 3], yaw: f32) -> Option<nav::ahead::Ahead> {
        const RE_PROBE_AFTER: Duration = Duration::from_millis(250);
        const RE_PROBE_DIST: f32 = 16.0;
        const RE_PROBE_YAW: f32 = 20.0;

        if let Some((at, was_yaw, when, verdict)) = self.ahead_probe {
            let moved = {
                let (dx, dy, dz) = (origin[0] - at[0], origin[1] - at[1], origin[2] - at[2]);
                (dx * dx + dy * dy + dz * dz).sqrt()
            };
            let turned = bot::math::norm_angle(f64::from(yaw - was_yaw)).abs() as f32;
            if moved < RE_PROBE_DIST && turned < RE_PROBE_YAW && when.elapsed() < RE_PROBE_AFTER {
                return Some(verdict);
            }
        }

        let map = self.map.as_ref()?;
        let world = nav::navgrid::World::new(&map.bsp, &map.info);
        let verdict = nav::ahead::ahead(&world, origin, yaw, nav::ahead::PROBE_REACH);
        self.ahead_probe = Some((origin, yaw, Instant::now(), verdict));
        Some(verdict)
    }

    fn measured_speed(&mut self, origin: [f32; 3], now: Instant) -> f32 {
        let speed = match self.last_origin {
            Some((prev, at)) => {
                let dt = now.saturating_duration_since(at).as_secs_f32();
                if dt < 1e-3 {
                    return self.last_speed;
                }
                let (dx, dy) = (origin[0] - prev[0], origin[1] - prev[1]);
                (dx * dx + dy * dy).sqrt() / dt
            }
            None => 0.0,
        };
        self.last_origin = Some((origin, now));
        // A respawn teleports us; that is not running.
        self.last_speed = if speed > 1000.0 { 0.0 } else { speed };
        self.last_speed
    }

    /// Load the map named in `svc_serverinfo` and pick an objective.
    ///
    /// Non-fatal: with no map the bot still plays, it just does not path.
    pub fn load_map(&mut self, seed: usize) {
        let Some(name) = self
            .signon
            .as_ref()
            .and_then(|s| s.server_info.as_ref())
            .map(|si| si.map_name().to_string())
        else {
            return;
        };
        self.map = crate::map::Map::load(&name);
        self.refresh_objective(seed);
    }

    /// Give the navigation layer this bot's identity.
    ///
    /// Must be called before the first route is planned. Without it every bot
    /// shares seed 0, runs the same search over the same graph, and produces the
    /// same path -- which is the conga line, not a steering problem.
    pub fn set_seed(&mut self, seed: u64) {
        self.follower = crate::navigate::PathFollower::with_seed(seed);
    }

    pub fn set_team_bot_id(&mut self, bot_id: u16) {
        self.team_bot_id = bot_id;
    }

    /// Re-pick where to go, e.g. after switching team or a new round.
    ///
    /// Uses the role-aware picker (plan Phase A1/A3): each bot draws a tactical
    /// role from its seed, a site, and either a plant-volume point or an
    /// approach ring so 15 teammates do not share one corridor endpoint.
    pub fn refresh_objective(&mut self, seed: usize) {
        let is_ct = self
            .decoder
            .as_ref()
            .is_some_and(|d| d.game.my_team() == crate::usermsg::Team::CounterTerrorist);
        match self
            .map
            .as_ref()
            .and_then(|m| crate::role::pick_objective(m, is_ct, seed))
        {
            Some(pick) => {
                self.role = Some(pick.role);
                self.site = Some(pick.destination);
                self.plant_spot = Some(pick.plant_spot);
                self.assigned_site = pick.site_index;
            }
            None => {
                self.role = None;
                self.site = None;
                self.plant_spot = None;
                self.assigned_site = None;
            }
        }
        self.rotate_site = None;
        self.rotate_events = 0;
        self.team_snapshot = None;
        self.rotate_cooldown = 0.0;
        self.follower.reset();
    }

    /// Feed same-team reports into the G0 tactical snapshot.
    pub fn ingest_team_reports(&mut self, reports: &[bot::TeamReport]) {
        let Some(map) = self.map.as_ref() else {
            return;
        };
        let Some(team) = self.decoder.as_ref().map(|d| match d.game.my_team() {
            crate::usermsg::Team::Terrorist => bot::Team::Terrorist,
            crate::usermsg::Team::CounterTerrorist => bot::Team::CounterTerrorist,
            crate::usermsg::Team::Spectator => bot::Team::Spectator,
            crate::usermsg::Team::Unassigned => bot::Team::Unassigned,
        }) else {
            return;
        };
        if !matches!(team, bot::Team::Terrorist | bot::Team::CounterTerrorist) {
            return;
        }
        let Some(a) = map.info.bomb_sites.iter().find(|site| site.centre()[2] > 100.0)
        else {
            return;
        };
        let Some(b) = map.info.bomb_sites.iter().find(|site| site.centre()[2] <= 100.0)
        else {
            return;
        };
        let sites = [a.centre(), b.centre()];
        let snapshot = self
            .team_snapshot
            .get_or_insert_with(|| bot::TeamSnapshot::new(team));
        for report in reports.iter().copied().filter(|report| report.team == team) {
            snapshot.apply(report, &sites);
        }
    }

    /// The current same-team tactical snapshot, if a map/team is known.
    pub fn team_snapshot(&self) -> Option<&bot::TeamSnapshot> {
        self.team_snapshot.as_ref()
    }

    /// Phase G2 — repath CTs toward a threatened site, merging local PVS
    /// enemies with the G0 same-team snapshot (pressure / plant belief).
    fn maybe_ct_rotate(&mut self, world: &bot::world::WorldView, dt: f32) {
        self.rotate_cooldown = (self.rotate_cooldown - dt).max(0.0);
        if self.rotate_cooldown > 0.0 {
            return;
        }
        if world.me.team != bot::world::Team::CounterTerrorist || !world.me.alive {
            return;
        }
        // Freeze / buy: stay on G1 holds.
        if world.me.freeze_period {
            return;
        }
        let seed = self.follower.seed as usize;
        let enemies: Vec<[f32; 3]> = world.visible_enemies().map(|p| p.origin).collect();
        let Some(map) = self.map.as_ref() else {
            return;
        };
        // G0 team-bus belief: a teammate's pressure report or plant sighting
        // rotates this bot even when the enemy is outside its own PVS.
        let team = match self.team_snapshot.as_ref() {
            Some(snapshot) => crate::role::TeamTactics {
                pressure_site: crate::role::plant_site_index(map, snapshot.pressure),
                plant_site: crate::role::plant_site_index(map, snapshot.plant_site),
            },
            None => crate::role::TeamTactics::EMPTY,
        };
        let Some(pick) = crate::role::ct_rotate_pick(
            map,
            seed,
            &enemies,
            world.bomb.planted,
            world.bomb.origin,
            team,
        ) else {
            return;
        };
        if !self.apply_ct_rotation(pick) {
            return;
        }
        self.rotate_cooldown = 4.0;
        self.follower.reset();
    }

    /// Apply a new G2 target once, keeping repeated PVS observations from
    /// inflating the live rotation count or resetting the route.
    fn apply_ct_rotation(&mut self, pick: crate::role::ObjectivePick) -> bool {
        if self.rotate_site == pick.site_index && self.site == Some(pick.destination) {
            return false;
        }
        self.role = Some(pick.role);
        self.site = Some(pick.destination);
        self.plant_spot = Some(pick.plant_spot);
        self.assigned_site = pick.site_index;
        self.rotate_site = pick.site_index;
        self.rotate_events = self.rotate_events.saturating_add(1);
        true
    }

    /// Deploy a weapon after spawning.
    ///
    /// A freshly spawned player HOLDS a knife but has not DEPLOYED one, and
    /// nothing deploys it automatically for a network client. The symptoms are
    /// easy to misread: `maxspeed` reports 240, which `ResetMaxSpeed` gives to
    /// a player with **no active item** (`player.cpp:8074-8105`), and no
    /// `CurWeapon` ever arrives because `CBasePlayerWeapon::UpdateClientData`
    /// only sends one for a weapon that is actually out (`weapons.cpp:1380`).
    /// The bot then looks armed-but-silent and can never shoot.
    ///
    /// A real client does this explicitly: the relay capture shows
    /// `weapon_knife` at +3.180 s, right after its spawn burst. `SelectItem`
    /// is the only path -- `usercmd_t.weaponselect` is never read by
    /// ReGameDLL.
    fn maybe_deploy_weapon(&mut self) {
        let Some(d) = self.decoder.as_ref() else {
            return;
        };
        if d.game.hud_resets == 0 || self.deployed_at_reset == Some(d.game.hud_resets) {
            return;
        }
        // Already holding something: nothing to do.
        if d.game.weapon_id != 0 {
            self.deployed_at_reset = Some(d.game.hud_resets);
            return;
        }
        if !self.clientdata.as_ref().is_some_and(|c| c.alive()) {
            return;
        }
        self.deployed_at_reset = Some(d.game.hud_resets);
        self.console.push("weapon_knife");
    }

    /// Queue a loadout when we are alive, in a buy zone, and have not already
    /// bought for this spawn.
    ///
    /// Gated on the `StatusIcon "buyzone"` message rather than on position,
    /// because `SIGNAL_BUY` is a two-phase latch that `HandleSignals`
    /// republishes only every 0.5 s (`player.cpp:7875-7879`), and
    /// `CanPlayerBuy` reads the *previous* window. Buying off our own idea of
    /// where the zone is therefore fails for up to half a second after
    /// entering it; the icon is the server telling us the latch is actually
    /// set.
    ///
    /// `hud_resets` counts `ResetHUD`, which the server sends on every spawn
    /// (`player.cpp:7577`), so it doubles as a round counter -- one buy per
    /// spawn, not one per frame spent standing in the zone.
    fn maybe_buy(&mut self) {
        let Some(d) = self.decoder.as_ref() else {
            return;
        };
        if !d.game.in_buy_zone || self.bought_at_reset == Some(d.game.hud_resets) {
            return;
        }
        // Alive is the only requirement beyond being in the zone. NOT
        // `in_game()`: that is `maxspeed > 1.5`, and ResetMaxSpeed pins
        // maxspeed to exactly 1.0 for the whole freeze period
        // (`player.cpp:8083-8087`) -- which is precisely when players buy.
        // Gating on it meant the bot could never buy anything at all.
        if !self.clientdata.as_ref().is_some_and(|c| c.alive()) {
            return;
        }
        let is_ct = d.game.my_team() == crate::usermsg::Team::CounterTerrorist;
        let plan = crate::console::buy_plan(d.game.money, is_ct);
        self.bought_at_reset = Some(d.game.hud_resets);
        self.console.extend(plan);
    }

    /// Start decoding the world.
    ///
    /// Deferred rather than done in `new()` because it needs two things only
    /// the signon can teach us, both per-server: the delta tables, and the
    /// id-to-name map for user messages. Call once the signon is in.
    pub fn start_decoding(&mut self) {
        let Some(signon) = self.signon.as_ref() else {
            return;
        };
        let table = crate::stream::collect_user_messages(&self.recorded);
        self.decoder = Some(crate::world::Decoder::new(signon, table));
    }

    /// How many previously-sent commands ride along in each `clc_move`.
    /// Measured from a real client: always exactly two.
    pub const NUM_BACKUP: usize = 2;

    /// Team slots for `jointeam` (`regamedll/dlls/client.h:32-42`).
    pub const TEAM_TERRORIST: u8 = 1;
    pub const TEAM_CT: u8 = 2;
    pub const TEAM_RANDOM: u8 = 5;

    /// Any class slot outside `1..CS_NUM_SKIN` is silently randomised
    /// (`client.cpp:1672-1675`), so this can never be refused for being wrong.
    pub const CLASS_ANY: u8 = 6;

    /// Join a team and keep at it until the server actually spawns us.
    ///
    /// **Why this retries rather than firing once on a timer.** The join is a
    /// three-way handshake against a state machine we cannot observe directly:
    ///
    /// * `jointeam` succeeds and sets `m_iMenu = Menu_ChooseAppearance`
    ///   (`client.cpp:3422`);
    /// * `joinclass` is refused outright unless `m_iMenu` is exactly that
    ///   (`client.cpp:3436-3441`);
    /// * and `HandleMenu_ChooseAppearance` only advances the join state when
    ///   `m_iJoiningState == PICKINGTEAM` (`client.cpp:1775-1793`) — for any
    ///   other value it falls through the `switch` doing nothing but resetting
    ///   the menu.
    ///
    /// That last one is the trap. `SHOWTEAMSELECT -> PICKINGTEAM` happens on
    /// the server's own schedule, inside `PlayerThink`. Fire `joinclass` on a
    /// fixed delay and it can land while the state is still `SHOWTEAMSELECT`:
    /// both commands are *accepted*, the team is really assigned — `TeamInfo`
    /// comes back saying TERRORIST — and yet the player never enters the game.
    /// The symptom is a bot that looks connected and healthy while its origin
    /// teleports between spawn points every 6 seconds with zero velocity and
    /// `maxspeed 1`: that is `JoiningThink`'s intro camera, not a player.
    ///
    /// So: send the pair, watch for a real spawn, and if it does not come,
    /// send it again. Repeating is safe — after `HandleMenu_ChooseAppearance`
    /// runs, `ResetMenu()` leaves `m_iMenu` at `Menu_OFF`, so the next
    /// `jointeam` is accepted rather than refused with `#Command_Not_Available`.
    ///
    /// Returns whether we are in the game.
    pub fn join_and_spawn<T: Transport>(
        &mut self,
        t: &mut T,
        team: u8,
        timeout: Duration,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;

        // Replicate a real client's join exactly, because we have measured it:
        // relaying a genuine CS 1.6 client showed `jointeam 5` at +1.767 s and
        // `joinclass 6` at +1.946 s relative to `new` -- roughly 0.7 s after
        // `sendents`, then 0.18 s apart, and each sent EXACTLY ONCE.
        //
        // Sending them once is not a stylistic choice, it is the only thing
        // that works. A second `jointeam` is refused with
        // `#Only_1_Team_Change` (`client.cpp:2088`), and that refusal resets
        // `m_iMenu` to Menu_ChooseTeam, after which `joinclass` is refused with
        // `#Command_Not_Available`. Retrying therefore destroys a join that had
        // already succeeded. Observed, in this order:
        //
        //     TeamInfo TERRORIST / #Game_join_terrorist   <- worked
        //     #Only_1_Team_Change                          <- the retry
        //     #Command_Not_Available                       <- joinclass now dead
        // WAIT FOR THE TEAM MENU before answering it. This is the whole ball
        // game, and answering early deadlocks the join permanently:
        //
        //  1. `jointeam` is accepted and sets m_iMenu = Menu_ChooseAppearance.
        //  2. If m_iJoiningState is still SHOWTEAMSELECT, PlayerThink then
        //     CLOBBERS m_iMenu back to Menu_ChooseTeam
        //     (`multiplay_gamerules.cpp:3756-3785`).
        //  3. So `joinclass` is refused with #Command_Not_Available.
        //  4. And a second `jointeam` is refused too -- #Only_1_Team_Change
        //     needs `m_bTeamChanged && deadflag != DEAD_NO`, and a
        //     not-yet-spawned client IS DEAD_DEAD (`client.cpp:656`).
        //  5. m_iMenu therefore stays at Menu_ChooseAppearance forever, and
        //     `RoundRespawn` skips `respawn()` for exactly that value
        //     (`player.cpp:4106`) -- so the bot is never spawned again, ever.
        //
        // The observable end state is a noclipping spectator that flies the
        // route at full speed with no weapon and no buy zone, while maxspeed
        // and ResetHUD both insist it spawned.
        //
        // `ShowVGUIMenu(VGUI_Menu_Team)` goes out in the same breath as the
        // SHOWTEAMSELECT -> PICKINGTEAM transition, so its arrival is proof
        // the server is ready to be answered.
        let menu_by = Instant::now() + Self::JOIN_MENU_WAIT;
        while Instant::now() < menu_by && !self.saw_team_menu() {
            self.pump_moving(t)?;
        }
        self.settle(t, Self::JOIN_SETTLE)?;
        self.send_command(&format!("jointeam {team}"));
        self.settle(t, Self::JOIN_STEP)?;
        self.send_command(&format!("joinclass {}", Self::CLASS_ANY));

        // Then keep offering the CLASS until the server has really taken it.
        //
        // This retry is not belt-and-braces, it is the difference between a
        // playing bot and a permanent spectator. `jointeam` sets
        // `m_iMenu = Menu_ChooseAppearance`, and the ONLY thing that clears it
        // is an accepted `joinclass` (via `HandleMenu_ChooseAppearance` ->
        // `ResetMenu`). And `RoundRespawn` reads:
        //
        //     if (m_iMenu != Menu_ChooseAppearance) { respawn(pev); ... }
        //
        // (`player.cpp:4106-4112`.) So a player whose `joinclass` never landed
        // is SKIPPED BY EVERY ROUND RESPAWN, silently, forever. It keeps the
        // state `ClientPutInServer` gave it -- DEAD_DEAD, FL_SPECTATOR,
        // MOVETYPE_NOCLIP (`client.cpp:650-660`) -- which is why such a bot
        // appears to fly around the map at full speed with no collision, no
        // velocity, no weapon and no buy zone, while `maxspeed` and `ResetHUD`
        // both cheerfully report that it spawned.
        //
        // Re-sending `joinclass` is safe; re-sending `jointeam` is NOT (it
        // trips `#Only_1_Team_Change` and puts the menu back). So only the
        // class is repeated.
        let mut next_try = Instant::now() + Self::JOIN_RETRY;
        while Instant::now() < deadline {
            self.pump_moving(t)?;
            if self.spawned() {
                return Ok(true);
            }
            if Instant::now() >= next_try {
                self.send_command(&format!("joinclass {}", Self::CLASS_ANY));
                next_try = Instant::now() + Self::JOIN_RETRY;
            }
        }
        Ok(self.joined())
    }

    /// Pump while sending REAL movement commands.
    ///
    /// This is not a nicety, it is what makes the server think at all. The
    /// engine calls `SV_PlayerRunPreThink` from exactly one place --
    /// **inside `SV_RunCmd`** (`sv_user.cpp:850`) -- and `SV_RunCmd` only runs
    /// when a `clc_move` arrives. So a client that sends only `clc_nop`
    /// keepalives gets no `PreThink`, hence no `CBasePlayer::JoiningThink` and
    /// no `CHalfLifeMultiplay::PlayerThink`, and its join state machine is
    /// frozen wherever it was.
    ///
    /// That froze ours at SHOWTEAMSELECT, so `jointeam` always landed in the
    /// wrong window: PlayerThink then clobbered `m_iMenu`, `joinclass` was
    /// refused forever with #Command_Not_Available, and `RoundRespawn` skipped
    /// the player for the rest of the map (`player.cpp:4106`). A real client
    /// streams `clc_move` from the moment it is connected, which is why it
    /// never sees any of this.
    fn pump_moving<T: Transport>(&mut self, t: &mut T) -> io::Result<Vec<Vec<u8>>> {
        let msecs = self.clock.due(Instant::now());
        if msecs.is_empty() {
            return self.pump_idle(t);
        }
        let body = self.build_move_body(&msecs, &bot::Intent::default());
        self.pump(t, &body)
    }

    /// Pump for `d`, doing nothing else.
    fn settle<T: Transport>(&mut self, t: &mut T, d: Duration) -> io::Result<()> {
        let until = Instant::now() + d;
        while Instant::now() < until {
            self.pump_moving(t)?;
        }
        Ok(())
    }

    /// Has the server shown us the team-selection menu?
    ///
    /// `ShowVGUIMenu(VGUI_Menu_Team)` is emitted in the same breath as the
    /// SHOWTEAMSELECT -> PICKINGTEAM transition, so receiving it is proof the
    /// server is ready to be answered.
    pub fn saw_team_menu(&self) -> bool {
        self.decoder
            .as_ref()
            .is_some_and(|d| d.game.saw_team_menu || d.game.hud_resets > 0)
    }

    /// Are we actually in the world -- not merely "in the game"?
    ///
    /// Holding a weapon is the only signal that does not lie. Every spawn
    /// grants a knife and `CurWeapon` is emitted when one is deployed
    /// (`weapons.cpp:1380`), whereas:
    ///
    /// * `maxspeed > 1.5` is set by `ResetMaxSpeed` in `GetIntoGame`
    ///   (`player.cpp:10718`) BEFORE the `FPlayerCanRespawn` gate at `:10730`,
    ///   so it reports 240 for a client that merely entered; and
    /// * `ResetHUD` fires from `m_fInitHUD`, which `Spawn()` sets (`:5997`)
    ///   but so do `Precache()` (`:6146`) and `ForceClientDllUpdate()`
    ///   (`:6694`).
    ///
    /// Both of those agreed with each other for hours while the bot was a
    /// noclipping corpse.
    pub fn spawned(&self) -> bool {
        self.decoder.as_ref().is_some_and(|d| d.game.weapon_id != 0)
    }

    /// Has the server accepted our team choice?
    ///
    /// `TeamInfo` naming our own entity with a real team is the confirmation,
    /// and it arrives within a few hundred milliseconds of `jointeam`
    /// (`player.cpp:6072-6075`), alongside `#Game_join_terrorist`.
    ///
    /// **Deliberately not `ResetHUD`.** That was the obvious choice and it is
    /// wrong: `GetIntoGame` sets `m_iJoiningState = JOINED` unconditionally but
    /// only calls `Spawn()` -- which is what produces `ResetHUD` -- when
    /// `FPlayerCanRespawn` allows it (`player.cpp:10730-10732`), and that is
    /// false mid-round. So a bot that joins between rounds is fully joined and
    /// receives no `ResetHUD` until the next round begins. Waiting for one made
    /// a working join look like a 15-second failure, and the "15 seconds" was
    /// just however long was left on the round clock.
    ///
    /// Spawning is a separate question -- see [`in_game`](Self::in_game).
    pub fn joined(&self) -> bool {
        self.decoder.as_ref().is_some_and(|d| {
            matches!(
                d.game.my_team(),
                crate::usermsg::Team::Terrorist | crate::usermsg::Team::CounterTerrorist
            )
        })
    }

    /// Has the server spawned us as a live player, rather than parked us in the
    /// joining camera?
    pub fn in_game(&self) -> bool {
        self.clientdata.as_ref().is_some_and(|c| c.in_game())
    }

    /// Queue a `clc_stringcmd` (e.g. `sendents`, `jointeam`).
    ///
    /// Queued, not sent: the channel allows one outstanding reliable message
    /// at a time, so several commands issued together go out in order as each
    /// is acknowledged. Call [`pump`](Self::pump) to drive them.
    pub fn send_command(&mut self, cmd: &str) {
        self.chan.queue_reliable(&Self::stringcmd(cmd));
    }

    /// How many datagrams one [`pump`](Self::pump) will take in before it
    /// insists on sending.
    ///
    /// This bound is load-bearing. The server only clears its outgoing
    /// reliable buffer once our echoed `sequence_ack` catches up (ReHLDS
    /// `Netchan_Process`: `reliable_ack == reliable_sequence &&
    /// sequence_ack >= last_reliable_sequence`). An unbounded receive loop
    /// keeps consuming during a burst and never gets round to sending that
    /// acknowledgement, so the server's buffer grows until it gives up with
    /// `WARNING: reliable overflow`. A real client sends once per frame no
    /// matter how much arrived.
    pub const MAX_DRAIN: usize = 256;

    /// Drive the channel one step: receive what is waiting (up to
    /// [`MAX_DRAIN`](Self::MAX_DRAIN) datagrams), then send a packet carrying
    /// any in-flight reliable message plus `unreliable`.
    ///
    /// Returns the assembled `svc_*` message streams that arrived.
    pub fn pump<T: Transport>(&mut self, t: &mut T, unreliable: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        // Drain the ENTIRE backlog before acknowledging, so the sequence we
        // echo is the newest one we have seen.
        //
        // This is load-bearing. The server only releases its reliable buffer
        // when `sequence_ack >= last_reliable_sequence` — our echoed sequence
        // must have caught up to the sequence the reliable went out on. If we
        // process one packet per outgoing packet we fall steadily behind (seen
        // on the wire: acking 176, 177, 178 while the server was already past
        // 200), the condition never holds, `netchan.message` grows every frame
        // and HLDS drops us with `Reliable channel overflowed`.
        for _ in 0..Self::MAX_DRAIN {
            match t.recv()? {
                Some(d) => {
                    for msg in self.ingest(&d) {
                        // Answer the server's echo prompts as they arrive.
                        self.echo_stufftexts(&msg);
                        out.push(msg);
                    }
                }
                None => break,
            }
        }
        // One packet per drain, carrying the freshest acknowledgement.
        let pkt = self.chan.transmit(unreliable);
        t.send(&pkt)?;
        Ok(out)
    }

    /// The unreliable body a real client puts in a packet it has nothing to
    /// say in — which is **never `clc_nop`** once it is connected.
    ///
    /// Verified from `captures/real_client_relay.bin`: a stock CS 1.6 client
    /// sends `clc_nop` only while it is still collecting the signon burst. From
    /// the moment it answers `svc_resourcerequest` it puts a `clc_move` in
    /// every packet — the first one is `mlen=8 loss=0 backup=2 cmds=0`, i.e.
    /// two backup commands and no new ones, *before* `spawn` has even been
    /// sent (`dec_real.txt:832-833`, `[12.214] C->S`). It keeps doing so
    /// through the spawn, the entity burst and `sendents`.
    ///
    /// So "connected but idle" for a real client means *a move with no new
    /// commands in it*, not silence. This builds exactly that: packet-loss
    /// byte, `numbackup` = whatever history we have, `numcmds` = 0.
    pub fn idle_body(&mut self) -> Vec<u8> {
        let Some(table) = self
            .signon
            .as_ref()
            .and_then(|s| s.registry.get("usercmd_t"))
            .cloned()
        else {
            // Still in the signon: `clc_nop` is what a real client sends here.
            return vec![netchan::clc::NOP];
        };
        let mut cmds: Vec<proto::usercmd::UserCmd> = self.cmd_history.iter().copied().collect();
        while cmds.len() < 2 {
            cmds.insert(0, proto::usercmd::UserCmd::default());
        }
        let numbackup = cmds.len() as u8;
        let payload =
            proto::usercmd::build_move_payload_backup(self.packet_loss(), &cmds, numbackup, &table);
        let seq = self.chan.outgoing_sequence as i32;
        proto::usercmd::build_clc_move(&payload, seq)
    }

    /// [`pump`](Self::pump) carrying [`idle_body`](Self::idle_body).
    pub fn pump_idle<T: Transport>(&mut self, t: &mut T) -> io::Result<Vec<Vec<u8>>> {
        let body = self.idle_body();
        self.pump(t, &body)
    }

    /// Are all queued reliable commands acknowledged?
    pub fn reliables_settled(&self) -> bool {
        !self.chan.reliable_in_flight() && self.chan.queued_count() == 0
    }

    /// Re-assert our name now that the server has put us in the world.
    ///
    /// A bot that lands on a client slot recycled from an earlier bot has its
    /// name replaced by that bot's — the name in the `connect` userinfo does
    /// **not** win. [`Identity::setinfo_name_command`] documents the whole
    /// chain, engine line by engine line. Call this once the spawn has gone
    /// through: by then the edict is ours, so the `setinfo` is accepted (or,
    /// if we happen to be dead, applied by ReGameDLL at the next respawn).
    ///
    /// Idempotent and cheap — a `setinfo` whose value already matches is
    /// discarded by `PF_SetClientKeyValue_I` (`rehlds/engine/pr_cmds.cpp:1660`)
    /// before it reaches the game DLL.
    pub fn reassert_name(&mut self) {
        let cmd = self.client.identity.setinfo_name_command();
        self.send_command(&cmd);
    }

    /// The name this session asked the server for.
    pub fn name(&self) -> &str {
        self.client.identity.wire_name()
    }

    /// Leave the server cleanly instead of just going quiet.
    ///
    /// Queues [`Client::DISCONNECT_COMMAND`] and pumps until the server has
    /// acknowledged it (or `timeout` elapses). Skipping this is what leaves a
    /// ghost holding a client slot for the whole `sv_timeout`, and the ghost is
    /// what the next bot inherits its name from.
    pub fn disconnect<T: Transport>(&mut self, t: &mut T, timeout: Duration) -> io::Result<()> {
        self.send_command(Client::DISCONNECT_COMMAND);
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            // Drive the channel until the reliable carrying `dropclient` has
            // been acknowledged; an unsent queue entry is not a disconnect.
            self.pump_idle(t)?;
            if self.reliables_settled() {
                break;
            }
        }
        Ok(())
    }

    /// Flip the echoed reliable-acknowledgement bit.
    ///
    /// **Kept only as a documented negative result.** The in-game stall looked
    /// exactly like an acknowledgement-parity deadlock, so `pump` used to call
    /// this after 400 ms of silence. Live test: it fired 45 times, flipping the
    /// bit through both values repeatedly, and the server never resumed. That
    /// rules the parity theory out — do not re-litigate it. Nothing calls this.
    pub fn resync_reliable_ack(&mut self) {
        self.chan.incoming_reliable ^= 1;
        self.resyncs += 1;
    }

    /// How many times [`resync_reliable_ack`](Self::resync_reliable_ack) fired.
    pub fn resyncs(&self) -> u32 {
        self.resyncs
    }

    /// One in-game tick: send the bot's command and take in what arrived.
    ///
    /// This is what a real client does every frame, and it is not optional —
    /// a connection carrying only `clc_nop` keepalives is not a playing client.
    /// The `clc_move` is built against the `usercmd_t` table learned from the
    /// signon and is munged with the sequence the packet will carry, so it must
    /// be assembled here, where that sequence is known.
    ///
    /// `msec` is the tick length in milliseconds.
    ///
    /// Prefer [`frame`](Self::frame): passing a fixed `msec` from a loop that
    /// runs faster than real time is what got every movement command silently
    /// discarded (see [`crate::clock`]).
    pub fn tick<T: Transport>(
        &mut self,
        t: &mut T,
        intent: &bot::Intent,
        msec: u8,
    ) -> io::Result<Vec<Vec<u8>>> {
        let body = self.build_move_body(&[msec], intent);
        self.pump(t, &body)
    }

    /// One real-time in-game frame: block until something arrives or the next
    /// command is due, drain everything, then send at most one packet.
    ///
    /// This is the loop a playing client actually runs. Three properties
    /// matter, and all three were previously wrong:
    ///
    /// * **The `msec` we claim tracks the wall clock**, via [`MoveClock`], so
    ///   ReHLDS's speedhack accounting stays at a ratio of ~1.0.
    /// * **We block rather than spin.** The old loop polled a 5 ms socket
    ///   timeout and emitted ~200 packets/s.
    /// * **We drain fully before sending.** The server only releases its
    ///   reliable buffer once our echoed acknowledgement catches up, so one
    ///   packet carrying the freshest ack beats many carrying stale ones.
    pub fn frame<T: Transport>(
        &mut self,
        t: &mut T,
        intent: &bot::Intent,
    ) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut drained = 0usize;
        while let Some(d) = t.recv()? {
            out.extend(self.ingest(&d));
            drained += 1;
            if drained >= Self::MAX_DRAIN {
                break;
            }
        }

        // Wait out the rest of the tick, waking early if the server speaks.
        let now = Instant::now();
        if self.clock.debt_ms(now) < 1 {
            if let Some(d) = t.recv_timeout(self.clock.next_due(now))? {
                out.extend(self.ingest(&d));
            }
        }
        for msg in &out {
            self.echo_stufftexts(msg);
        }

        // One console command per frame at most, and only when the reliable
        // channel is idle -- see `crate::console` for why a burst is fatal.
        self.maybe_deploy_weapon();
        self.maybe_buy();
        if let Some(cmd) = self.console.next(Instant::now(), self.reliables_settled()) {
            self.send_command(&cmd);
        }

        // Let the bot decide, if it has a brain and a world to look at.
        // Falls back to the caller's intent otherwise, which is what the
        // protocol captures want.
        let decided = self.think();
        let intent = decided.as_ref().unwrap_or(intent);

        let msecs = self.clock.due(Instant::now());
        let body = if msecs.is_empty() {
            // NOT `clc_nop`: see `idle_body`. A connected client that has
            // nothing new to say still sends a move.
            self.idle_body()
        } else {
            self.build_move_body(&msecs, intent)
        };
        let pkt = self.chan.transmit(&body);
        t.send(&pkt)?;
        Ok(out)
    }

    /// The `clc_move` (+ `clc_delta`) body for a batch of commands.
    ///
    /// `msecs` is oldest-first, matching both `build_move_payload`'s write order
    /// and the server's reversed read (`sv_user.cpp:1638-1641`).
    fn build_move_body(&mut self, msecs: &[u8], intent: &bot::Intent) -> Vec<u8> {
        let Some(table) = self
            .signon
            .as_ref()
            .and_then(|s| s.registry.get("usercmd_t"))
            .cloned()
        else {
            // No table yet: nothing to move with.
            return vec![netchan::clc::NOP];
        };

        // Backup commands first, then this frame's new ones. A real client
        // sends numbackup=2 on every single move; without them a lost packet
        // makes the server replay `lastcmd` instead of what we actually did.
        let mut cmds: Vec<proto::usercmd::UserCmd> = self.cmd_history.iter().copied().collect();
        let numbackup = cmds.len() as u8;
        for &m in msecs {
            let cmd = crate::control::intent_to_usercmd(intent, m);
            cmds.push(cmd);
            self.cmd_history.push_back(cmd);
            while self.cmd_history.len() > Self::NUM_BACKUP {
                self.cmd_history.pop_front();
            }
        }
        let payload =
            proto::usercmd::build_move_payload_backup(self.packet_loss(), &cmds, numbackup, &table);
        // The move payload is keyed on the sequence this packet will carry.
        let seq = self.chan.outgoing_sequence as i32;
        let mut msg = proto::usercmd::build_clc_move(&payload, seq);

        // `clc_delta <frame>` tells the server which frame we have successfully
        // decoded, so it can delta against it instead of sending a full update.
        //
        // We may only claim a frame we really parsed. Until the entity decoder
        // lands there is no such frame, so we say nothing -- `delta_sequence`
        // is reset to -1 at the top of every `SV_ExecuteClientMessage`
        // (`sv_user.cpp:1866`) and `SV_EmitPacketEntities` then sends a FULL
        // `svc_packetentities` (`sv_main.cpp:4727`), which is the only thing we
        // can currently read. Claiming a frame we never decoded made the server
        // delta against a world we did not have.
        if let Some(seq) = self.last_valid_frame {
            msg.push(netchan::clc::DELTA);
            msg.push((seq & 0xFF) as u8);
        }
        msg
    }

    /// Outgoing packet-loss percentage, bits 0-6 of the `clc_move` loss byte.
    fn packet_loss(&self) -> u8 {
        let lost = self.chan.lost_packets;
        let total = self.chan.incoming_sequence.max(1);
        ((lost.saturating_mul(100) / total).min(100)) as u8
    }
}

/// Phase B ORCA-lite: sidemove push away from nearby same-team players.
///
/// Full ORCA solves a linear program over velocity obstacles; we only need the
/// "don't walk into your teammate's back" half for the dust2 approach stream.
/// For each living same-team player inside `AVOID_RADIUS`, contribute half the
/// separation responsibility as a view-relative sidemove (right = +).
///
/// Returns an additive sidemove in [-180, 180]. Callers clamp the total.
fn teammate_avoid_sidemove(
    me: [f32; 3],
    view_yaw: f32,
    fwd: f32,
    side: f32,
    players: &[bot::PlayerView],
    my_team: bot::Team,
) -> f32 {
    // B1 CONGA push: wider bubble + stronger push (was 120 / 48 / 1.6).
    const AVOID_RADIUS: f32 = 160.0;
    const PERSONAL: f32 = 56.0; // approximate combined body radius
                                // Not moving: nothing to avoid into.
    if fwd.abs() < 1.0 && side.abs() < 1.0 {
        return 0.0;
    }
    let yaw = f64::from(view_yaw).to_radians();
    let (sy, cy) = yaw.sin_cos();
    // View right: (sin yaw, -cos yaw) in GoldSrc xy.
    let (rx, ry) = (sy as f32, -cy as f32);
    // View forward for "teammate ahead" soft brake (applied by caller via
    // magnitude of push when we return; we pack brake into |push| excess —
    // no: return only sidemove; brake handled separately).
    let mut push = 0.0f32;
    for p in players {
        if p.team != my_team || !p.alive {
            continue;
        }
        let dx = p.origin[0] - me[0];
        let dy = p.origin[1] - me[1];
        let d = (dx * dx + dy * dy).sqrt();
        if d < 1.0 || d > AVOID_RADIUS {
            continue;
        }
        // Penetration depth of personal spaces.
        let pen = (PERSONAL - d).max(0.0) + (AVOID_RADIUS - d) * 0.22;
        if pen <= 0.0 {
            continue;
        }
        // Sign: if teammate is on our right, push left (negative sidemove).
        let right_dot = dx * rx + dy * ry;
        let dir = if right_dot >= 0.0 { -1.0 } else { 1.0 };
        // Stronger than pure half-ORCA — live CONGA-1 still ~55–60%.
        push += dir * pen * 2.4;
    }
    let _ = sy;
    let _ = cy;
    push.clamp(-220.0, 220.0)
}

/// Scale forwardmove when a teammate is directly ahead (CONGA brake).
fn teammate_forward_scale(
    me: [f32; 3],
    view_yaw: f32,
    players: &[bot::PlayerView],
    my_team: bot::Team,
) -> f32 {
    const AHEAD_R: f32 = 140.0;
    let yaw = f64::from(view_yaw).to_radians();
    let (sy, cy) = yaw.sin_cos();
    let (fx, fy) = (cy as f32, sy as f32); // GoldSrc forward
    let mut scale = 1.0f32;
    for p in players {
        if p.team != my_team || !p.alive {
            continue;
        }
        let dx = p.origin[0] - me[0];
        let dy = p.origin[1] - me[1];
        let d = (dx * dx + dy * dy).sqrt();
        if d < 1.0 || d > AHEAD_R {
            continue;
        }
        let ahead = (dx * fx + dy * fy) / d;
        if ahead > 0.55 {
            // Closer + more centered ahead → slower.
            let t = (1.0 - d / AHEAD_R) * ahead;
            scale = scale.min(1.0 - 0.55 * t);
        }
    }
    scale.clamp(0.35, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server plugin can query any client cvar, and a client that never
    /// answers leaves the request outstanding -- which plugin anticheats kick
    /// for. Silence works on our test server and fails on real ones.
    #[test]
    fn a_cvar_query_is_answered_on_the_reliable_channel() {
        let mut s = Session::new(Identity {
            name: "Bot7".into(),
            ..Default::default()
        });
        let mut msg = vec![crate::svc::SVC_SENDCVARVALUE];
        msg.extend_from_slice(b"cl_lw ");
        s.answer_cvar_queries(&msg);
        assert_eq!(s.chan.queued_count(), 1, "no reply queued");
    }

    /// A 57 or 58 inside a payload must not manufacture a cvar query.
    ///
    /// This is what actually happened, and it was fatal without ever looking
    /// like an error. Every phantom query queued a RELIABLE reply, so at 50
    /// packets a second the netchannel queue ran away -- measured live at 7900
    /// entries and climbing by ~113 every two seconds, `in_flight` stuck true
    /// forever. From that point the client could never send another console
    /// command: the bot stood on the bomb site holding an AK with `weapon_c4`
    /// at the head of a queue that would never drain again.
    #[test]
    fn a_payload_byte_is_not_mistaken_for_a_cvar_query() {
        let mut s = Session::new(Identity {
            name: "Bot7".into(),
            ..Default::default()
        });

        // svc_print, whose text happens to contain both opcode values.
        let mut msg = vec![crate::svc::SVC_PRINT];
        msg.extend_from_slice(&[
            b'x',
            crate::svc::SVC_SENDCVARVALUE,
            crate::svc::SVC_SENDCVARVALUE2,
            b'y',
            0,
        ]);

        let trace = s.trace_message(&msg);
        assert!(trace.complete(), "the walk must consume the whole message");
        assert!(
            s.cvar_replies(&trace).is_empty(),
            "a byte inside a payload was read as a cvar query"
        );

        // ...and the real thing, in the same message, is still answered.
        msg.push(crate::svc::SVC_SENDCVARVALUE);
        msg.extend_from_slice(b"name ");
        let trace = s.trace_message(&msg);
        assert_eq!(s.cvar_replies(&trace).len(), 1, "the real query was missed");
    }

    /// The v2 form carries a request id that must come back verbatim, and the
    /// reply repeats the cvar name (SV_ParseCvarValue2, sv_user.cpp:1804-1815).
    #[test]
    fn a_v2_cvar_query_echoes_the_request_id_and_the_name() {
        let mut s = Session::new(Identity {
            name: "Bot7".into(),
            ..Default::default()
        });
        let mut msg = vec![crate::svc::SVC_SENDCVARVALUE2];
        msg.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        msg.extend_from_slice(b"name ");

        let trace = s.trace_message(&msg);
        let replies = s.cvar_replies(&trace);
        assert_eq!(replies.len(), 1);
        let out = &replies[0];
        assert_eq!(out[0], netchan::clc::CVARVALUE2);
        assert_eq!(&out[1..5], &0xDEADBEEFu32.to_le_bytes());
        let rest = String::from_utf8_lossy(&out[5..]);
        let mut parts = rest.split(' ');
        assert_eq!(parts.next(), Some("name"));
        assert_eq!(parts.next(), Some("Bot7"), "must report our real name");
    }

    /// An unknown cvar is reported as empty, which is what the engine does for
    /// a cvar that does not exist -- not skipped, or the server waits forever.
    #[test]
    fn an_unknown_cvar_is_answered_with_an_empty_value() {
        let s = Session::new(Identity::default());
        assert_eq!(s.cvar_value("definitely_not_a_cvar"), "");
        assert_eq!(s.cvar_value("cl_lw"), "1");
    }

    /// The name in the connect userinfo does not always survive: a bot given a
    /// client slot recycled from an earlier bot has it replaced by that bot's
    /// name (Identity::setinfo_name_command has the engine-level chain). This
    /// is the repair -- and it has to be the real `clc_stringcmd`, because
    /// nothing else can change a userinfo key after the handshake.
    #[test]
    fn reassert_name_queues_a_setinfo_stringcmd_for_our_own_name() {
        let mut s = Session::new(Identity {
            name: "Bot03".into(),
            ..Default::default()
        });
        assert_eq!(s.chan.queued_count(), 0);
        s.reassert_name();
        assert_eq!(s.chan.queued_count(), 1);

        let payload = NetChannel::string_command("setinfo \"name\" \"Bot03\"");
        assert_eq!(payload[0], netchan::clc::STRINGCMD);
        assert_eq!(
            &payload[1..payload.len() - 1],
            b"setinfo \"name\" \"Bot03\"",
            "must carry OUR name, not the default"
        );
        assert_eq!(*payload.last().unwrap(), 0, "stringcmd is NUL terminated");
        assert_eq!(s.name(), "Bot03");
    }

    /// Leaving without a `dropclient` is what creates the ghost the next bot
    /// inherits its name from: the slot stays `connected` for the whole
    /// sv_timeout with `cl->name` intact, because only `SV_DropClient` clears
    /// it (rehlds/engine/host.cpp:504).
    #[test]
    fn disconnect_puts_the_dropclient_command_on_the_wire() {
        struct Sink {
            sent: Vec<Vec<u8>>,
        }
        impl Transport for Sink {
            fn send(&mut self, data: &[u8]) -> io::Result<()> {
                self.sent.push(data.to_vec());
                Ok(())
            }
            fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
                Ok(None)
            }
        }

        let mut s = Session::new(Identity::default());
        let mut t = Sink { sent: Vec::new() };
        s.disconnect(&mut t, Duration::from_millis(150)).unwrap();

        assert!(
            !t.sent.is_empty(),
            "disconnect must put a packet on the wire"
        );

        // Peel the netchannel off every packet and look for the actual bytes.
        //
        // The previous version of this assertion was
        //     s.chan.reliable_in_flight() || s.chan.queued_count() == 0
        // which passes when NOTHING was queued -- precisely the failure it
        // claimed to catch. An assertion that cannot fail is worse than no
        // assertion, because it reports coverage that does not exist.
        let found = t.sent.iter().any(|pkt| {
            if pkt.len() <= 8 {
                return false;
            }
            let seq = u32::from_le_bytes(pkt[0..4].try_into().unwrap()) & 0x3FFF_FFFF;
            let mut body = pkt[8..].to_vec();
            let n = body.len() - body.len() % 4;
            proto::munge::unmunge(&mut body[..n], &proto::munge::TABLE2, seq as i32);
            body.windows(10).any(|w| w == b"dropclient")
        });
        assert!(found, "no packet on the wire carried `dropclient`");
    }

    use proto::munge;

    /// Build a server→client sequenced packet the way HLDS would: header in the
    /// clear, body munged with table 2 on the packet's sequence.
    fn server_packet(seq: u32, ack: u32, body: &[u8]) -> Vec<u8> {
        let mut munged = body.to_vec();
        let n = munged.len() - munged.len() % 4;
        munge::munge(&mut munged[..n], &munge::TABLE2, seq as i32);
        let mut out = Vec::new();
        out.extend_from_slice(&(seq & 0x3FFF_FFFF).to_le_bytes());
        out.extend_from_slice(&(ack & 0x3FFF_FFFF).to_le_bytes());
        out.extend_from_slice(&munged);
        out
    }

    #[test]
    fn a_fresh_session_is_in_the_handshake_phase() {
        let s = Session::named("Tester");
        assert_eq!(s.phase, Phase::Handshake);
        assert!(s.signon.is_none());
    }

    #[test]
    fn repeated_g2_target_does_not_count_or_repath_again() {
        let mut s = Session::named("Tester");
        let pick = crate::role::ObjectivePick {
            role: crate::role::BotRole::Split,
            destination: [100.0, 200.0, 0.0],
            plant_spot: [120.0, 220.0, 0.0],
            site_index: Some(1),
        };
        assert!(s.apply_ct_rotation(pick));
        assert_eq!(s.rotate_events, 1);
        assert!(!s.apply_ct_rotation(pick));
        assert_eq!(s.rotate_events, 1);
    }

    #[test]
    fn ingest_returns_an_uncompressed_message_stream_verbatim() {
        let mut s = Session::named("Tester");
        // A tiny svc stream (svc_nop, svc_time-ish bytes) — content is opaque
        // to ingest, it just unmunges and hands it back.
        let payload = vec![0x01u8, 0x07, 0xDE, 0xAD, 0xBE, 0xEF];
        let pkt = server_packet(s.chan.incoming_sequence + 1, 0, &payload);
        let out = s.ingest(&pkt);
        assert_eq!(out, vec![payload]);
    }

    #[test]
    fn ingest_ignores_a_runt() {
        let mut s = Session::named("Tester");
        assert!(s.ingest(&[0x00, 0x01]).is_empty());
    }

    #[test]
    fn ingest_reassembles_a_split_datagram_into_its_message() {
        let mut s = Session::named("Tester");
        let payload = vec![0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let full = server_packet(s.chan.incoming_sequence + 1, 0, &payload);

        // Cut the reassembled datagram into two SPLIT fragments.
        let mid = full.len() / 2;
        let split = |index: u8, count: u8, chunk: &[u8]| {
            let mut p = Vec::new();
            p.extend_from_slice(&(-2i32).to_le_bytes());
            p.extend_from_slice(&7i32.to_le_bytes()); // arbitrary split sequence
            p.push((index << 4) | (count & 0x0F));
            p.extend_from_slice(chunk);
            p
        };
        assert!(s.ingest(&split(0, 2, &full[..mid])).is_empty());
        let out = s.ingest(&split(1, 2, &full[mid..]));
        assert_eq!(out, vec![payload]);
    }
}

/// The stufftext handler must find messages by **parsing**, never by looking
/// for the byte 9.
///
/// A scan cannot tell an opcode from a payload byte, and `svc_stufftext` is
/// opcode 9 — a value that occurs constantly inside delta descriptions, MD5
/// hashes, resource hashes and user-message payloads. The old handler read a
/// NUL-terminated string from wherever it found a 9 and reported it as a
/// console command the server had sent us; one live run produced thirty such
/// lines, of which exactly one (`fullserverinfo …`) was a real message.
///
/// These tests pin the property that replaces it: a 9 that is not at a message
/// boundary is never seen, and a real stufftext behind an arbitrary amount of
/// binary data still is.
#[cfg(test)]
mod stufftext_is_parsed_not_scanned {
    use super::*;
    use crate::stream::Item;
    use crate::svc;

    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    /// The naive scan, kept verbatim so the tests can show what it reports on
    /// the very same bytes. Nothing but the tests calls it.
    fn scan_for_stufftexts(msg: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < msg.len() {
            if msg[i] != svc::SVC_STUFFTEXT {
                i += 1;
                continue;
            }
            let rest = &msg[i + 1..];
            let Some(end) = rest.iter().position(|&b| b == 0) else {
                break;
            };
            out.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            i += 1 + end + 1;
        }
        out
    }

    /// `svc_updateuserinfo` ends in a 16-byte CD-key hash — raw binary that the
    /// server has no way to keep 9-free. Put a 9 in it followed by text that
    /// looks exactly like the command we are hunting for.
    fn updateuserinfo_with_a_fake_command() -> Vec<u8> {
        let mut m = vec![svc::SVC_UPDATEUSERINFO, 1];
        m.extend_from_slice(&7u32.to_le_bytes());
        m.extend(cstr("\\name\\Probe\\rate\\100000"));
        let mut hash = [0xAAu8; 16];
        hash[0] = svc::SVC_STUFFTEXT;
        hash[1..12].copy_from_slice(b"reconnect\n\0");
        m.extend_from_slice(&hash);
        m
    }

    #[test]
    fn a_nine_inside_a_binary_payload_is_not_read_as_a_command() {
        let mut msg = updateuserinfo_with_a_fake_command();
        msg.extend_from_slice(&[svc::SVC_STUFFTEXT]);
        msg.extend(cstr("allow_shaders 0\n"));

        // The defect is real on these exact bytes: the scan invents a
        // `reconnect` the server never sent.
        let scanned = scan_for_stufftexts(&msg);
        assert!(
            scanned.contains(&"reconnect\n".to_string()),
            "the scan was supposed to be fooled by this payload: {scanned:?}"
        );
        assert_eq!(scanned.len(), 2);

        // The walker sees one message boundary carrying a stufftext, and it is
        // the real one.
        let mut s = Session::named("Probe");
        let trace = s.trace_message(&msg);
        assert_eq!(
            trace.stopped_on, None,
            "halted at byte {}",
            trace.stopped_at
        );
        assert_eq!(trace.stopped_at, msg.len());
        assert_eq!(trace.stufftexts(), vec!["allow_shaders 0\n".to_string()]);

        // And the echo follows the walk, not the scan.
        assert_eq!(s.echo_stufftexts(&msg), vec!["allow_shaders 0".to_string()]);
    }

    /// The same trap inside a **user message** payload, which is where most of
    /// the phantom hits came from live: `SayText`, `TextMsg` and friends carry
    /// arbitrary bytes and are only sizeable against the registration table.
    #[test]
    fn a_nine_inside_a_user_message_payload_is_not_read_as_a_command() {
        // Register SayText (id 76) as variable-length, exactly as the server
        // does with svc_newusermsg.
        let mut msg = vec![svc::SVC_NEWUSERMSG, 76, 255];
        let mut name = [0u8; 16];
        name[..7].copy_from_slice(b"SayText");
        msg.extend_from_slice(&name);

        // Then send one whose payload starts with a 9 and reads like a command.
        let body: Vec<u8> = {
            let mut b = vec![svc::SVC_STUFFTEXT];
            b.extend(cstr("reconnect\n"));
            b.extend_from_slice(&[0xFF, 0x09, 0x00]);
            b
        };
        msg.push(76);
        msg.push(u8::try_from(body.len()).unwrap());
        msg.extend_from_slice(&body);

        // Finally the real one.
        msg.push(svc::SVC_STUFFTEXT);
        msg.extend(cstr("allow_autoaim 0\n"));

        assert!(scan_for_stufftexts(&msg).contains(&"reconnect\n".to_string()));

        let mut s = Session::named("Probe");
        let trace = s.trace_message(&msg);
        assert_eq!(
            trace.stopped_on, None,
            "halted at byte {}",
            trace.stopped_at
        );
        assert_eq!(trace.stufftexts(), vec!["allow_autoaim 0\n".to_string()]);
        // The registration was learned from a walked message, not a scan.
        assert_eq!(s.user_msgs[&76].name, "SayText");
        assert!(s.user_msgs[&76].is_variable());
        assert_eq!(
            trace
                .items
                .iter()
                .filter(|i| matches!(i, Item::User { .. }))
                .count(),
            1
        );
        assert_eq!(s.echo_stufftexts(&msg), vec!["allow_autoaim 0".to_string()]);
    }

    /// The live signon burst, byte for byte.
    ///
    /// This is the case the walk exists for, and the fixture settles it
    /// empirically: it holds **seven** bytes equal to 9, of which **three** are
    /// real `svc_stufftext` messages and four are payload. The scan cannot tell
    /// them apart; the walk does, and it reaches them at all only because it
    /// steps over `svc_serverinfo` and the seven bit-packed
    /// `svc_deltadescription`s in front of them by parsing.
    ///
    /// `SV_New_f` (`rehlds/engine/sv_main.cpp:1509-1594`) is the reason for
    /// that order: serverinfo (with the delta tables inside
    /// `SV_SendServerinfo`), then the `svc_newusermsg` registrations, then
    /// `svc_stufftext "fullserverinfo …"`.
    #[test]
    fn the_live_signon_separates_three_real_commands_from_four_payload_bytes() {
        const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");
        assert_eq!(
            SIGNON.iter().filter(|&&b| b == svc::SVC_STUFFTEXT).count(),
            7,
            "fixture no longer contains the payload bytes this test is about"
        );

        let mut s = Session::named("Probe");
        let trace = s.trace_message(SIGNON);
        assert_eq!(
            trace.stopped_on.map(svc::name),
            None,
            "halted at byte {} of {}",
            trace.stopped_at,
            SIGNON.len()
        );
        assert_eq!(trace.stopped_at, SIGNON.len());

        let real = trace.stufftexts();
        assert_eq!(
            real,
            vec![
                "fullserverinfo \"\\*gamedir\\cstrike\"\n".to_string(),
                "allow_shaders 0\n".to_string(),
                "allow_autoaim 0\n".to_string(),
            ],
        );

        // The scan on the same bytes: it finds four bogus extras and cannot
        // say which three of its seven answers are the messages.
        let scanned = scan_for_stufftexts(SIGNON);
        assert_eq!(scanned.len(), 7);
        assert_eq!(
            scanned.iter().filter(|t| real.contains(t)).count(),
            3,
            "four of the scan's seven hits are payload bytes: {scanned:?}"
        );

        // Seven delta descriptions were stepped over by parsing them; without
        // that the walk stops at byte 0 on `svc_serverinfo` and finds nothing.
        assert_eq!(
            trace
                .items
                .iter()
                .filter(
                    |i| matches!(i, Item::Engine { id, .. } if *id == svc::SVC_DELTADESCRIPTION)
                )
                .count(),
            7
        );
        // And the registrations were learned, not scanned for.
        assert!(
            s.user_msgs.len() > 20,
            "user message table: {} entries",
            s.user_msgs.len()
        );
    }

    /// A message the walk genuinely cannot size must halt it and say so.
    /// Silence and a wrong resume offset are the two ways this goes bad; a
    /// reported stop is neither.
    #[test]
    fn an_unsizeable_message_halts_the_walk_and_is_reported() {
        let mut msg = vec![svc::SVC_STUFFTEXT];
        msg.extend(cstr("allow_shaders 0\n"));
        msg.push(svc::SVC_PACKETENTITIES);
        msg.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let mut s = Session::named("Probe");
        let trace = s.trace_message(&msg);
        assert_eq!(trace.stopped_on, Some(svc::SVC_PACKETENTITIES));
        assert_eq!(trace.stopped_at, 18, "opcode + 17 bytes of string");
        // Everything in FRONT of it was still decoded -- halting costs the
        // tail, not the message.
        assert_eq!(trace.stufftexts(), vec!["allow_shaders 0\n".to_string()]);
    }
}
