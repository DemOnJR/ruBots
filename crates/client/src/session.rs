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
    /// When a datagram last arrived, for deadlock detection.
    last_recv: Option<Instant>,
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
            last_recv: None,
            resyncs: 0,
            record_all: false,
            recorded: Vec::new(),
            resource_message: None,
            content: crate::content::GameContent::discover(),
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
        if self.record_all {
            self.recorded.push(msg.to_vec());
        }
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
        let mut got_any = false;
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
                    got_any = true;
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
    pub fn tick<T: Transport>(
        &mut self,
        t: &mut T,
        intent: &bot::Intent,
        msec: u8,
    ) -> io::Result<Vec<Vec<u8>>> {
        let Some(table) = self
            .signon
            .as_ref()
            .and_then(|s| s.registry.get("usercmd_t"))
            .cloned()
        else {
            // No table yet: nothing to move with.
            return self.pump(t, &[netchan::clc::NOP]);
        };

        let cmd = crate::control::intent_to_usercmd(intent, msec);
        let payload = proto::usercmd::build_move_payload(0, &[cmd], &table);
        // The move payload is keyed on the sequence this packet will carry.
        let seq = self.chan.outgoing_sequence as i32;
        let mut msg = proto::usercmd::build_clc_move(&payload, seq);

        // A real client follows every `clc_move` with `clc_delta <frame>` in
        // the *same* packet (verified in the captured client stream: opcode 4
        // plus one byte). That byte tells the server which frame we have
        // acknowledged, which is what lets it retire old frames and delta
        // against ours. Without it the server can never discard anything it
        // has sent us.
        msg.push(netchan::clc::DELTA);
        msg.push((self.chan.incoming_sequence & 0xFF) as u8);

        self.pump(t, &msg)
    }
}

#[cfg(test)]
mod tests {
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
