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
    /// The server's reply to `sendres`, captured the moment it arrives.
    ///
    /// It does **not** come inside the signon burst — it is its own message,
    /// sent only after we ask — so `Signon::resources` is always empty and
    /// anything that reads consistency out of the signon reads nothing. This
    /// is the authoritative copy: it carries the resource list *and* the
    /// consistency demands, which share one bit block.
    pub resource_message: Option<proto::resources::ResourceMessage>,
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
    /// Which round we last bought in, so a buy happens once per spawn rather
    /// than every frame we happen to be standing in the zone.
    bought_at_reset: Option<u32>,
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
            resource_message: None,
            content: crate::content::GameContent::discover(),
            clock: crate::clock::MoveClock::new(Instant::now()),
            clientdata: None,
            console: crate::console::ConsoleQueue::new(),
            bought_at_reset: None,
            decoder: None,
            cmd_history: std::collections::VecDeque::new(),
            last_valid_frame: None,
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
        for reply in self.cvar_replies(msg) {
            self.chan.queue_reliable(&reply);
        }
    }

    /// The `clc_cvarvalue` / `clc_cvarvalue2` replies `msg` calls for.
    ///
    /// Pure, so a test can assert on the exact bytes rather than on a queue
    /// depth. `svc_sendcvarvalue` (57) is `string cvar`, answered with
    /// `clc_cvarvalue` (10) `string value`. `svc_sendcvarvalue2` (58) adds a
    /// request id that must come back verbatim, and its reply repeats the cvar
    /// name too (`SV_ParseCvarValue2`, `sv_user.cpp:1804-1815`).
    fn cvar_replies(&self, msg: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < msg.len() {
            match msg[i] {
                crate::svc::SVC_SENDCVARVALUE => {
                    let Some(end) = msg[i + 1..].iter().position(|&b| b == 0) else {
                        break;
                    };
                    let name = String::from_utf8_lossy(&msg[i + 1..i + 1 + end]).into_owned();
                    let mut reply = vec![netchan::clc::CVARVALUE];
                    reply.extend_from_slice(self.cvar_value(&name).as_bytes());
                    reply.push(0);
                    out.push(reply);
                    i += 1 + end + 1;
                }
                crate::svc::SVC_SENDCVARVALUE2 => {
                    if msg.len() < i + 5 {
                        break;
                    }
                    let Some(end) = msg[i + 5..].iter().position(|&b| b == 0) else {
                        break;
                    };
                    let name = String::from_utf8_lossy(&msg[i + 5..i + 5 + end]).into_owned();
                    let mut reply = vec![netchan::clc::CVARVALUE2];
                    reply.extend_from_slice(&msg[i + 1..i + 5]);
                    reply.extend_from_slice(name.as_bytes());
                    reply.push(0);
                    reply.extend_from_slice(self.cvar_value(&name).as_bytes());
                    reply.push(0);
                    out.push(reply);
                    i += 5 + end + 1;
                }
                _ => i += 1,
            }
        }
        out
    }

    /// The five bytes `SV_WriteSpawn` + `SV_WriteVoiceCodec` always end on.
    ///
    /// `svc_signonnum 1`, then `svc_voiceinit` with an empty codec string and a
    /// zero quality byte (`sv_main.cpp:5817-5822`) -- three fixed bytes, so the
    /// reassembled spawn response ends on this exact sequence.
    pub const SPAWN_TAIL: [u8; 5] = [crate::svc::SVC_SIGNONNUM, 1, crate::svc::SVC_VOICEINIT, 0, 0];

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
                    if std::env::var("AIPLAYERS_FRAGTRACE").is_ok() {
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

        // 2) Ask for the signon. This goes through the reliable queue, so it
        //    is retransmitted until the server acknowledges it.
        self.chan.queue_reliable(&Self::stringcmd("new"));

        // 3) Collect fragments, acking as we go.
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
                            return Ok(self.signon.as_ref().unwrap());
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
                let pkt = self.chan.transmit(&[netchan::clc::NOP]);
                t.send(&pkt).map_err(|_| Disconnect::Closed)?;
                last_ack = Instant::now();
            }
        }
        Err(Disconnect::Timeout)
    }

    /// Once running, the `MoveSender` bound to the learned `usercmd_t` table.
    ///
    /// Lazily created from the signon; `None` until [`connect_and_signon`] has
    /// reached [`Phase::Running`].
    pub fn move_sender(&mut self) -> Option<&mut MoveSender> {
        if self.sender.is_none() {
            let table = self
                .signon
                .as_ref()?
                .registry
                .get("usercmd_t")?
                .clone();
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
        Self::new(Identity { name: name.to_string(), ..Identity::default() })
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
            self.pump(t, &[netchan::clc::NOP])?;
        }

        // 2) `sendents`, until traffic proves we are in.
        let before = self.stats.plain;
        let mut last_send = Instant::now() - Duration::from_secs(1);
        while Instant::now() < deadline {
            if last_send.elapsed() >= Duration::from_millis(400) && self.reliables_settled() {
                self.send_command("sendents");
                last_send = Instant::now();
            }
            self.pump(t, &[netchan::clc::NOP])?;
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

    /// Scan an assembled `svc_*` stream for stufftexts we are expected to echo,
    /// and queue the echoes. Returns the commands echoed.
    ///
    /// This is a targeted scan rather than a full stream walk: the stream can
    /// contain user messages whose lengths we do not know, so walking it
    /// blindly would desynchronise. Matching on the opcode byte followed by one
    /// of the known command names is safe because those names do not occur in
    /// binary payload data.
    pub fn echo_stufftexts(&mut self, msg: &[u8]) -> Vec<String> {
        let mut echoed = Vec::new();
        let mut i = 0usize;
        while i < msg.len() {
            if msg[i] != crate::svc::SVC_STUFFTEXT {
                i += 1;
                continue;
            }
            let rest = &msg[i + 1..];
            let Some(end) = rest.iter().position(|&b| b == 0) else {
                break;
            };
            let text = String::from_utf8_lossy(&rest[..end]).to_string();
            let head = text.split_whitespace().next().unwrap_or("");
            if Self::ECHO_COMMANDS.contains(&head) {
                let cmd = text.trim_end_matches(['\n', '\r']).to_string();
                self.send_command(&cmd);
                echoed.push(cmd);
            }
            i += 1 + end + 1;
        }
        echoed
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
        let demands =
            proto::consistency::demands(&msg.resources, &msg.consistency, msg.spawncount);

        let mut answers = Vec::with_capacity(demands.len());
        for d in &demands {
            let answer = match d {
                proto::consistency::Demand::Bounds { mins, maxs, .. } => {
                    proto::consistency::Answer::Bounds(*mins, *maxs)
                }
                proto::consistency::Demand::ExactFile { path, .. } => {
                    // Without the file we cannot answer. Send the demand with a
                    // zero hash rather than dropping the entry: the count must
                    // still match, and a wrong hash is a *specific* server-side
                    // complaint ("Bad file <name>") we can act on, whereas a
                    // short count is the generic "Bad file data".
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
    /// Gap between `sendents` and `jointeam`, from a real client: 0.7 s.
    pub const JOIN_SETTLE: Duration = Duration::from_millis(700);

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
                self.pump(t, &[netchan::clc::NOP])?;
            }
        }
        Ok(())
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
        if !self.clientdata.as_ref().is_some_and(|c| c.in_game()) {
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
        self.settle(t, Self::JOIN_SETTLE)?;
        self.send_command(&format!("jointeam {team}"));
        self.settle(t, Self::JOIN_STEP)?;
        self.send_command(&format!("joinclass {}", Self::CLASS_ANY));

        // Then simply wait. `ResetHUD` is the server confirming the join
        // (`GetIntoGame` -> `Spawn` -> `m_fInitHUD` -> `player.cpp:7577`).
        while Instant::now() < deadline {
            self.pump(t, &[netchan::clc::NOP])?;
            if self.joined() {
                return Ok(true);
            }
        }
        Ok(self.joined())
    }

    /// Pump for `d`, doing nothing else.
    fn settle<T: Transport>(&mut self, t: &mut T, d: Duration) -> io::Result<()> {
        let until = Instant::now() + d;
        while Instant::now() < until {
            self.pump(t, &[netchan::clc::NOP])?;
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
    pub fn pump<T: Transport>(
        &mut self,
        t: &mut T,
        unreliable: &[u8],
    ) -> io::Result<Vec<Vec<u8>>> {
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

    /// Are all queued reliable commands acknowledged?
    pub fn reliables_settled(&self) -> bool {
        !self.chan.reliable_in_flight() && self.chan.queued_count() == 0
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
        self.maybe_buy();
        if let Some(cmd) = self.console.next(Instant::now(), self.reliables_settled()) {
            self.send_command(&cmd);
        }

        let msecs = self.clock.due(Instant::now());
        let body = if msecs.is_empty() {
            vec![netchan::clc::NOP]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A server plugin can query any client cvar, and a client that never
    /// answers leaves the request outstanding -- which plugin anticheats kick
    /// for. Silence works on our test server and fails on real ones.
    #[test]
    fn a_cvar_query_is_answered_on_the_reliable_channel() {
        let mut s = Session::new(Identity { name: "Bot7".into(), ..Default::default() });
        let mut msg = vec![crate::svc::SVC_SENDCVARVALUE];
        msg.extend_from_slice(b"cl_lw ");
        s.answer_cvar_queries(&msg);
        assert_eq!(s.chan.queued_count(), 1, "no reply queued");
    }

    /// The v2 form carries a request id that must come back verbatim, and the
    /// reply repeats the cvar name (SV_ParseCvarValue2, sv_user.cpp:1804-1815).
    #[test]
    fn a_v2_cvar_query_echoes_the_request_id_and_the_name() {
        let s = Session::new(Identity { name: "Bot7".into(), ..Default::default() });
        let mut msg = vec![crate::svc::SVC_SENDCVARVALUE2];
        msg.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());
        msg.extend_from_slice(b"name ");

        let replies = s.cvar_replies(&msg);
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

    use super::*;
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
