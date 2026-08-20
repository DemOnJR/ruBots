//! Decoding our own player state out of the running-phase datagram.
//!
//! `svc_clientdata` is the server telling us, authoritatively, where we are:
//! `origin`, `velocity`, `health`, `flags`, `weapons`, `maxspeed`, `punchangle`
//! and the rest of `clientdata_t` (`testserver/rehlds/cstrike/delta.lst:6-66`).
//! It is the cheapest possible proof that our movement commands are being
//! *applied* rather than silently discarded, and it is the first piece of the
//! full entity layer.
//!
//! ## Where it sits in the datagram
//!
//! `SV_SendClientDatagram` (`rehlds/engine/sv_main.cpp:5000-5063`) writes, in
//! order: `svc_time` + float, then `SV_WriteClientdataToMessage`, then the
//! entity block. `SV_WriteClientdataToMessage` (`sv_main.cpp:1262-1378`) may
//! emit up to three byte-aligned messages of its own first:
//!
//! ```text
//! [svc_choke    (42)]                        // no payload
//! [svc_setangle (10)] short pitch, yaw, roll // or
//! [svc_addangle (38)] short delta_yaw
//!  svc_clientdata (15) <bit block>
//! ```
//!
//! ## The bit block
//!
//! ```text
//! 1 bit  has_delta
//! if has_delta { 8 bits delta_sequence }     // from = frames[seq].clientdata
//! clientdata_t delta
//! loop { 1 bit more; if !more break;
//!        6 bits slot; weapon_data_t delta }  // only when userinfo has cl_lw 1
//! ```
//!
//! While we send no `clc_delta`, `delta_sequence` is `-1` server-side, so
//! `has_delta` is 0 and the delta is against a **zeroed** struct — the simplest
//! case, and a complete snapshot every frame.

use proto::bitbuf::BitReader;
use proto::delta::{DeltaRegistry, Value};
use std::collections::HashMap;

use crate::svc;

/// Per-frame player flags ReGameDLL packs into `clientdata_t.iuser3`
/// (`regamedll/dlls/client.cpp:5100-5121`, values from `cdll_dll.h:70-73`).
pub const PLAYER_CAN_SHOOT: i64 = 1 << 0;
pub const PLAYER_FREEZE_TIME_OVER: i64 = 1 << 1;
pub const PLAYER_IN_BOMB_ZONE: i64 = 1 << 2;
pub const PLAYER_HOLDING_SHIELD: i64 = 1 << 3;

/// `FL_ONGROUND`, `rehlds/common/const.h:49`.
pub const FL_ONGROUND: i64 = 1 << 9;

/// Our own player state for one server frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientData {
    /// Server time this frame carries, from the preceding `svc_time`.
    pub time: f32,
    /// `clientdata_t`, keyed by the field names in `delta.lst`.
    pub fields: HashMap<String, Value>,
    /// `weapon_data_t` per weapon slot. Present only with `cl_lw 1`.
    pub weapons: HashMap<u8, HashMap<String, Value>>,
    /// From a `svc_setangle` in the same datagram: the server forcing our view,
    /// which it does on every spawn (`sv_main.cpp:1459-1462`).
    pub forced_angles: Option<[f32; 3]>,
}

impl ClientData {
    pub fn f32(&self, key: &str) -> Option<f32> {
        self.fields.get(key).and_then(Value::as_f32)
    }

    pub fn origin(&self) -> [f32; 3] {
        [
            self.f32("origin[0]").unwrap_or(0.0),
            self.f32("origin[1]").unwrap_or(0.0),
            self.f32("origin[2]").unwrap_or(0.0),
        ]
    }

    pub fn velocity(&self) -> [f32; 3] {
        [
            self.f32("velocity[0]").unwrap_or(0.0),
            self.f32("velocity[1]").unwrap_or(0.0),
            self.f32("velocity[2]").unwrap_or(0.0),
        ]
    }

    pub fn speed(&self) -> f32 {
        let v = self.velocity();
        (v[0] * v[0] + v[1] * v[1]).sqrt()
    }

    pub fn health(&self) -> f32 {
        self.f32("health").unwrap_or(0.0)
    }

    pub fn maxspeed(&self) -> f32 {
        self.f32("maxspeed").unwrap_or(0.0)
    }

    /// Recoil, which the engine adds to our view angles before firing. See
    /// `PM_CheckParameters` (`pm_shared.cpp:3035-3047`).
    pub fn punchangle(&self) -> [f32; 3] {
        [
            self.f32("punchangle[0]").unwrap_or(0.0),
            self.f32("punchangle[1]").unwrap_or(0.0),
            self.f32("punchangle[2]").unwrap_or(0.0),
        ]
    }

    /// Bitmask of carried weapons, `1 << WeaponIdType`.
    ///
    /// The cheapest reliable "am I actually in the game?" test available before
    /// the user-message layer exists: a player who has completed the join gets
    /// a knife (`mp_t_give_player_knife`), so this is non-zero. While
    /// `JoiningThink` is still cycling the intro camera it is zero.
    pub fn weapons(&self) -> u32 {
        self.fields
            .get("weapons")
            .and_then(Value::as_i64)
            .unwrap_or(0) as u32
    }

    /// Have we finished joining and spawned as a live player?
    ///
    /// `maxspeed` is the discriminator, and it is a sharp one.
    /// `CBasePlayer::ResetMaxSpeed` (`player.cpp:8074-8105`) gives **1.0** to a
    /// player in the freeze period or waiting to join, and 210-250 to one who
    /// is actually playing (240 with no weapon deployed, less with a heavy
    /// one). There is nothing in between.
    ///
    /// Nothing else here works. Health is 100 and `deadflag` is `DEAD_NO` while
    /// the join camera cycles spawn points, so both say "alive". `weapons` is
    /// zero on a fresh spawn too, before anything is deployed. Only `maxspeed`
    /// separates "in the world" from "watching it".
    pub fn in_game(&self) -> bool {
        self.alive() && self.maxspeed() > 1.5
    }

    /// `iuser3` carries the per-frame player flags ReGameDLL packs in
    /// `client.cpp:5108-5121`; bit 2 is `PLAYER_IN_BOMB_ZONE`
    /// (`cdll_dll.h:69-77`).
    pub fn iuser3(&self) -> i64 {
        self.fields
            .get("iuser3")
            .and_then(Value::as_i64)
            .unwrap_or(0)
    }

    pub fn in_bomb_zone(&self) -> bool {
        self.iuser3() & PLAYER_IN_BOMB_ZONE != 0
    }

    /// `PLAYER_CAN_SHOOT` — the game DLL's own verdict on whether firing will
    /// do anything, which folds in freeze time, defusing, and holding a shield
    /// (`client.cpp:5100-5110`). Cheaper and more correct than re-deriving it.
    pub fn can_shoot(&self) -> bool {
        self.iuser3() & PLAYER_CAN_SHOOT != 0
    }

    /// `PLAYER_FREEZE_TIME_OVER`.
    ///
    /// **The name is inverted in the source.** It is set *during* the freeze
    /// period (`client.cpp:5104-5106`), not after it, so this reports "frozen".
    pub fn freeze_period(&self) -> bool {
        self.iuser3() & PLAYER_FREEZE_TIME_OVER != 0
    }

    /// `pev->flags`, of which we mostly want `FL_ONGROUND`.
    pub fn flags(&self) -> i64 {
        self.fields.get("flags").and_then(Value::as_i64).unwrap_or(0)
    }

    /// On the ground. Required to plant, and to start a defuse
    /// (`wpn_c4.cpp:114`, `ggrenade.cpp:1256`).
    pub fn on_ground(&self) -> bool {
        self.flags() & FL_ONGROUND != 0
    }

    /// `deadflag == DEAD_NO`.
    ///
    /// **Absent means alive.** The delta is written against a zeroed struct,
    /// and `DELTA_MarkSendFields` only marks fields that *differ* from it, so a
    /// `deadflag` of `DEAD_NO` (0) is never transmitted. Defaulting a missing
    /// field to "dead" reports every healthy player as a corpse -- which is
    /// exactly the wrong conclusion to draw while debugging whether the bot
    /// spawned. Every accessor here has the same shape: absent == zero.
    pub fn alive(&self) -> bool {
        self.fields
            .get("deadflag")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            == 0
    }
}

/// Parse one running-phase datagram far enough to recover `svc_clientdata`.
///
/// Returns `None` when the message is not a server datagram (it does not begin
/// with `svc_time`) or when the layout does not hold — never a partial guess.
pub fn parse_datagram(msg: &[u8], registry: &DeltaRegistry) -> Option<ClientData> {
    if msg.first() != Some(&svc::SVC_TIME) || msg.len() < 5 {
        return None;
    }
    let mut out = ClientData {
        time: f32::from_le_bytes(msg[1..5].try_into().ok()?),
        ..Default::default()
    };
    let mut at = 5usize;

    // The optional byte-aligned preamble, in the order the server writes it.
    loop {
        match msg.get(at) {
            Some(&svc::SVC_CHOKE) => at += 1,
            Some(&svc::SVC_SETANGLE) => {
                if msg.len() < at + 7 {
                    return None;
                }
                // MSG_WriteHiresAngle: deg = short * 360 / 65536.
                let a = |o: usize| {
                    f32::from(i16::from_le_bytes([msg[o], msg[o + 1]])) * 360.0 / 65536.0
                };
                out.forced_angles = Some([a(at + 1), a(at + 3), a(at + 5)]);
                at += 7;
            }
            Some(&svc::SVC_ADDANGLE) => at += 3,
            _ => break,
        }
    }

    if msg.get(at) != Some(&svc::SVC_CLIENTDATA) {
        return None;
    }
    at += 1;

    let cd = registry.get("clientdata_t")?;
    let mut r = BitReader::new(&msg[at..]);

    // `has_delta` plus, when set, the frame we are being delta'd against.
    // We never advertise a frame, so this is 0 and `from` is a zeroed struct;
    // if that ever changes we cannot reconstruct the base and must bail rather
    // than return numbers that look plausible and are not.
    if r.read_bits(1) != 0 {
        let _seq = r.read_bits(8);
        return None;
    }
    out.fields = proto::delta::parse_delta(&mut r, cd);

    // The weapon loop exists because our userinfo carries `cl_lw 1`
    // (`sv_main.cpp:1362`). The terminating zero bit is written either way.
    if let Some(wd) = registry.get("weapon_data_t") {
        let mut guard = 0;
        while r.read_bits(1) != 0 {
            let slot = r.read_bits(6) as u8;
            let f = proto::delta::parse_delta(&mut r, wd);
            out.weapons.insert(slot, f);
            guard += 1;
            if guard > 64 || r.overflowed() {
                break;
            }
        }
    }
    if r.overflowed() {
        return None;
    }
    Some(out)
}

/// `MSG_ReadBitCoord` — `common.cpp:820-869`.
///
/// `[int present][frac present]`, then a sign bit if either is, then a 12-bit
/// integer part and a 3-bit eighths part as declared.
fn read_bit_coord(r: &mut proto::bitbuf::BitReader<'_>) -> f32 {
    let has_int = r.read_bits(1) != 0;
    let has_frac = r.read_bits(1) != 0;
    if !has_int && !has_frac {
        return 0.0;
    }
    let sign = r.read_bits(1) != 0;
    let int = if has_int { r.read_bits(12) } else { 0 };
    let frac = if has_frac { r.read_bits(3) } else { 0 };
    let v = int as f32 + frac as f32 / 8.0;
    if sign {
        -v
    } else {
        v
    }
}

/// `MSG_ReadBitVec3Coord` — three presence bits, then the present coordinates.
fn read_bit_vec3_coord(r: &mut proto::bitbuf::BitReader<'_>) -> [f32; 3] {
    let flags = [r.read_bits(1) != 0, r.read_bits(1) != 0, r.read_bits(1) != 0];
    let mut out = [0.0f32; 3];
    for (i, present) in flags.iter().enumerate() {
        if *present {
            out[i] = read_bit_coord(r);
        }
    }
    out
}

/// Bytes a bit block occupies, given the reader that consumed it.
///
/// GoldSrc bit blocks are byte-framed: `MSG_EndBitReading` advances the byte
/// cursor by `ceil(bits/8)`, minimum one, and the caller resumes aligned
/// (`common.cpp:628-656`). Getting this off by one byte silently reinterprets
/// the rest of the datagram.
fn block_bytes(r: &proto::bitbuf::BitReader<'_>) -> usize {
    (r.byte_pos() + usize::from(r.bit_offset() > 0)).max(1)
}

/// Everything we know about the world, rebuilt from the server's stream.
///
/// This is the piece that turns a connected socket into a player: the entity
/// block is the only place enemy positions exist, and `svc_clientdata` is the
/// only authoritative account of our own.
pub struct Decoder {
    pub registry: proto::delta::DeltaRegistry,
    pub maxclients: u8,
    /// Our own player slot, from `svc_serverinfo`. Our entity number is
    /// `my_slot + 1` (`SV_IsPlayerIndex` counts from 1).
    pub my_slot: u8,
    pub baselines: proto::entity::Baselines,
    /// Last `svc_time`; the timebase for `DT_TIMEWINDOW_*` fields.
    pub time: f32,
    pub clientdata: Option<ClientData>,
    /// The most recent entity frame, ascending by entity number.
    pub entities: Vec<proto::entity::EntityState>,
    pub game: crate::usermsg::GameState,
    pub user_table: crate::stream::UserMsgTable,
    pub stats: DecodeStats,
}

/// Why a datagram did or did not decode. Kept because "the bot cannot see
/// anyone" has several very different causes and they must not be confused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecodeStats {
    /// Datagrams that decoded end to end.
    pub ok: u32,
    /// Datagrams carrying an entity block.
    pub with_entities: u32,
    /// Datagrams beginning with `svc_time` whose clientdata block did not
    /// parse, so nothing after it could be located either.
    pub no_clientdata: u32,
    /// Message tails we could not finish walking, and the opcode that stopped
    /// us. Distinct from `no_clientdata`: this one means we decoded the frame
    /// but may have missed trailing user messages.
    pub partial: u32,
    pub last_stop: Option<u8>,
    /// Entity blocks that failed to parse. Non-zero here means the world model
    /// is wrong, not merely incomplete.
    pub entity_errors: u32,
    /// The most recent entity-decode failure, kept because the count alone
    /// cannot be acted on. Discarding it once cost a live run: 463 errors and
    /// nothing to say which of the eight failure modes it was.
    pub last_entity_error: Option<proto::entity::EntityError>,
    /// `svc_deltapacketentities` seen. We never advertise a frame via
    /// `clc_delta`, so the server should never send one; if it does, our
    /// `last_valid_frame` bookkeeping is lying somewhere.
    pub unexpected_delta_frames: u32,
    /// `svc_spawnbaseline` blocks the walker handed us that decoded. Normally
    /// exactly one per map — see [`Decoder::read_baselines`].
    pub baseline_blocks: u32,
    /// `svc_spawnbaseline` blocks that did not decode. Any non-zero value here
    /// means we reached a byte 22 at a genuine message boundary and it was not
    /// a baseline block, which the server has no way to produce.
    pub baseline_errors: u32,
    pub last_baseline_error: Option<proto::entity::EntityError>,
}

impl Decoder {
    pub fn new(signon: &crate::signon::Signon, user_table: crate::stream::UserMsgTable) -> Self {
        let (maxclients, my_slot) = signon
            .server_info
            .as_ref()
            .map(|si| (si.max_players, si.player_index))
            .unwrap_or((32, 0));
        // `TeamInfo` and friends are keyed by ENTITY index, which for a
        // player is slot + 1 (`SV_IsPlayerIndex` counts from 1). Without this
        // the game state has no idea which of the 32 slots is us, and
        // `my_team()` answers Unassigned forever.
        let mut game = crate::usermsg::GameState::default();
        game.self_index = Some(my_slot.saturating_add(1));
        // Slots above `maxclients` cannot exist, which is what lets the game
        // state reject a team update addressed to one as noise rather than
        // believing it.
        if maxclients >= 1 {
            game.max_clients = maxclients;
        }

        Self {
            registry: signon.registry.clone(),
            maxclients,
            my_slot,
            baselines: proto::entity::Baselines::default(),
            time: 0.0,
            clientdata: None,
            entities: Vec::new(),
            game,
            user_table,
            stats: DecodeStats::default(),
        }
    }

    /// Our own entity number.
    pub fn my_entity(&self) -> u16 {
        u16::from(self.my_slot) + 1
    }

    /// Parse the `svc_spawnbaseline` bit block at `at`, returning the offset
    /// just past it.
    ///
    /// Without baselines every new entity is deltaed against a zeroed state
    /// rather than its baseline, so most fields read as zero and players appear
    /// at the map origin.
    ///
    /// # Where this message can legitimately be, and why it is never scanned for
    ///
    /// `svc_spawnbaseline` is written in exactly one place: `SV_CreateBaseline`
    /// appends it to **`g_psv.signon`**, the server's per-map signon buffer
    /// (`rehlds/engine/sv_main.cpp:5889-5917`), once per map — the only call is
    /// from `SV_ActivateServer` (`sv_main.cpp:6208`), at map load, before any
    /// client has spawned.
    ///
    /// `g_psv.signon` reaches a client in exactly one place too: `SZ_Write(&msg,
    /// g_psv.signon.data, g_psv.signon.cursize)` in `SV_Spawn_f_internal`
    /// (`sv_main.cpp:1671`) — the reply to the client's `spawn` command,
    /// immediately followed by `SV_WriteSpawn` (`:1672`, whose own tail is
    /// `svc_signonnum 1` at `:1472-1473`) and `SV_WriteVoiceCodec` (`:1680`).
    /// That whole buffer is then fragmented and sent (`:1681-1682`).
    ///
    /// Nothing else can produce one:
    ///
    /// * `SV_SendEnts_f` (`rehlds/engine/sv_user.cpp:1935-1974`) — the
    ///   `sendents` command the client sends straight after `spawn`, which is
    ///   what actually starts the entity stream — writes **no baselines at
    ///   all**. It sets `fully_connected = TRUE` and, only under
    ///   `sv_delayed_spray_upload`, at most two `svc_stufftext`s.
    /// * `SV_SendClientDatagram` (`sv_main.cpp:5000-5063`), the running-phase
    ///   writer, never emits opcode 22 either.
    ///
    /// So a byte 22 anywhere in a running-phase datagram is **always** a false
    /// positive, and the message is located the same way every other message is
    /// — by [`crate::stream::walk`] stepping over its predecessors and halting
    /// on it, because it is bit-packed. A byte 22 inside some message's payload
    /// is never at a message boundary and is therefore never offered here at
    /// all. That is a structural guarantee; the validation inside
    /// [`proto::entity::parse_spawn_baseline`] is the backstop, not the gate.
    ///
    /// An empty block is refused rather than installed: `SV_CreateBaseline`'s
    /// loop always emits entity 0 (`entnum == 0` passes `!svent->free &&
    /// g_psvs.maxclients >= entnum`, `sv_main.cpp:5891-5894`; edict 0 is the
    /// world and is never freed), so a baseline block with nothing in it cannot
    /// have come from this server.
    fn read_baselines(&mut self, msg: &[u8], at: usize) -> Option<usize> {
        let body = at + 1;
        let mut r = proto::bitbuf::BitReader::new(msg.get(body..)?);
        match proto::entity::parse_spawn_baseline(&mut r, &self.registry, self.maxclients) {
            Ok(b) if !b.by_number.is_empty() => {
                self.baselines = b;
                self.stats.baseline_blocks += 1;
                Some(body + block_bytes(&r))
            }
            other => {
                self.stats.baseline_errors += 1;
                self.stats.last_baseline_error = other.err();
                None
            }
        }
    }

    /// Take in one fully-assembled `svc_*` stream.
    ///
    /// Single pass, driven by the byte walker: it steps over everything it can
    /// and hands us each message it cannot, which we then parse or skip, and
    /// then it resumes. Nothing is assumed about where in the message anything
    /// sits.
    ///
    /// That last point is the whole reason for this shape. A netchannel packet
    /// is `[reliable messages][unreliable datagram]`, so on any packet carrying
    /// reliable data the datagram does **not** start at byte 0 and
    /// `msg[0] == svc_time` is false. Dispatching on the first byte silently
    /// skipped the entity block of every such packet -- about 7% of them, and
    /// exactly the ones carrying the most interesting reliable traffic.
    ///
    /// **Nothing short-circuits this walk.** `svc_spawnbaseline` used to be
    /// found by scanning the whole datagram for a bare byte 22 *before* the
    /// walk, and a hit made this function `return` — so one payload byte that
    /// happened to be 22 threw away the entire datagram, including every
    /// message in front of it that had not even been looked at yet. Over the
    /// four captures in `captures/swarm/` that scan fired on 14 151 to 24 279
    /// datagrams each. It is now [`Self::read_baselines`], reached only when the
    /// walker halts on a real opcode boundary, and a failure there costs the
    /// tail of one datagram and nothing more.
    pub fn feed(&mut self, msg: &[u8]) {
        let mut at = 0usize;
        let mut saw_entities = false;
        let mut decoded = false;

        loop {
            let w = crate::stream::walk(&msg[at..], &self.user_table);
            for item in &w.items {
                match item {
                    crate::stream::Item::User { name, payload, .. } => {
                        self.game.apply(name, payload);
                    }
                    crate::stream::Item::Engine { id, payload } if *id == svc::SVC_TIME => {
                        if let Ok(b) = <[u8; 4]>::try_from(payload.as_slice()) {
                            self.time = f32::from_le_bytes(b);
                        }
                    }
                    // Who is in each slot. The walker already steps over this
                    // exactly; decoding it is what stops a slot's team
                    // outliving its owner — see
                    // `GameState::apply_user_info`.
                    crate::stream::Item::Engine { id, payload }
                        if *id == svc::SVC_UPDATEUSERINFO =>
                    {
                        if let Some(u) = crate::usermsg::parse_update_user_info(payload) {
                            self.game.apply_user_info(&u);
                        }
                    }
                    _ => {}
                }
            }
            let Some(op) = w.stopped_on else {
                break;
            };
            let stop = at + w.stopped_at;

            let next = match op {
                svc::SVC_CLIENTDATA => {
                    match self.read_clientdata(msg, stop) {
                        Some(end) => {
                            decoded = true;
                            Some(end)
                        }
                        None => None,
                    }
                }
                svc::SVC_PACKETENTITIES => match self.read_entities(msg, stop) {
                    Some(end) => {
                        saw_entities = true;
                        Some(end)
                    }
                    None => None,
                },
                // Only ever legitimate in the reply to `spawn`; see
                // `read_baselines` for why it is located here rather than hunted
                // for. Handled in the dispatch like any other bit-packed
                // message so that the walk resumes into `SV_WriteSpawn`'s
                // `svc_time` / `svc_clientdata` / `svc_signonnum` behind it.
                svc::SVC_SPAWNBASELINE => self.read_baselines(msg, stop),
                svc::SVC_DELTAPACKETENTITIES => {
                    // We never advertise a frame via clc_delta, so the server
                    // has no basis to delta against one. Refuse rather than
                    // decode against a frame we do not have.
                    self.stats.unexpected_delta_frames += 1;
                    None
                }
                _ => self.skip_bit_packed(msg, stop),
            };

            match next {
                Some(n) if n > stop => at = n,
                _ => {
                    self.stats.partial += 1;
                    self.stats.last_stop = Some(op);
                    break;
                }
            }
            if at >= msg.len() {
                break;
            }
        }

        if decoded {
            self.stats.ok += 1;
        }
        if saw_entities {
            self.stats.with_entities += 1;
        }
    }

    /// Parse the `svc_clientdata` bit block at `at`, returning the offset just
    /// past it.
    fn read_clientdata(&mut self, msg: &[u8], at: usize) -> Option<usize> {
        let body = at + 1;
        let cd = self.registry.get("clientdata_t")?;
        let mut r = proto::bitbuf::BitReader::new(msg.get(body..)?);

        // We advertise no frame, so the server cannot be delta-compressing
        // this. If it ever is we cannot reconstruct the base, and returning
        // plausible-looking numbers would be worse than returning none.
        if r.read_bits(1) != 0 {
            self.stats.no_clientdata += 1;
            return None;
        }
        let mut out = ClientData {
            time: self.time,
            fields: proto::delta::parse_delta(&mut r, cd),
            ..Default::default()
        };
        if let Some(wd) = self.registry.get("weapon_data_t") {
            let mut guard = 0;
            while r.read_bits(1) != 0 {
                let slot = r.read_bits(6) as u8;
                let f = proto::delta::parse_delta(&mut r, wd);
                out.weapons.insert(slot, f);
                guard += 1;
                if guard > 64 || r.overflowed() {
                    break;
                }
            }
        }
        if r.overflowed() {
            self.stats.no_clientdata += 1;
            return None;
        }
        self.clientdata = Some(out);
        Some(body + block_bytes(&r))
    }

    /// Parse a full `svc_packetentities` at `at`, returning the offset just
    /// past it.
    fn read_entities(&mut self, msg: &[u8], at: usize) -> Option<usize> {
        if msg.len() < at + 3 {
            return None;
        }
        let count = u16::from_le_bytes([msg[at + 1], msg[at + 2]]);
        let body = at + 3;
        let mut r = proto::bitbuf::BitReader::new(&msg[body..]);
        let ctx = proto::entity::PacketCtx {
            registry: &self.registry,
            baselines: &self.baselines.by_number,
            instanced: &self.baselines.instanced,
            maxclients: self.maxclients,
        };
        match proto::entity::parse_packet_entities_full_checked(&mut r, &ctx, usize::from(count)) {
            Ok(ents) => {
                self.entities = ents;
                Some(body + block_bytes(&r))
            }
            Err(e) => {
                self.stats.entity_errors += 1;
                self.stats.last_entity_error = Some(e);
                None
            }
        }
    }

    /// Step over one bit-packed engine message starting at `at`, returning the
    /// offset just past it.
    fn skip_bit_packed(&self, msg: &[u8], at: usize) -> Option<usize> {
        let id = *msg.get(at)?;
        let body = at + 1;
        let mut r = proto::bitbuf::BitReader::new(msg.get(body..)?);
        match id {
            // `SV_EmitEvents_internal`, sv_main.cpp:4125-4265.
            svc::SVC_EVENT => {
                let count = r.read_bits(5);
                for _ in 0..count {
                    r.skip(10); // event index
                    if r.read_bits(1) != 0 {
                        r.skip(11); // packet (entity) index
                        if r.read_bits(1) != 0 {
                            let t = self.registry.get("event_t")?;
                            proto::delta::parse_delta(&mut r, t);
                        }
                    }
                    if r.read_bits(1) != 0 {
                        r.skip(16); // fire time
                    }
                }
            }
            // A single event, no count prefix.
            svc::SVC_EVENT_RELIABLE => {
                r.skip(10);
                let t = self.registry.get("event_t")?;
                proto::delta::parse_delta(&mut r, t);
                if r.read_bits(1) != 0 {
                    r.skip(16);
                }
            }
            // `SV_EmitPings_internal`, sv_main.cpp:4834-4856.
            svc::SVC_PINGS => {
                let mut guard = 0;
                while r.read_bits(1) != 0 {
                    r.skip(5 + 12 + 7); // slot, ping, loss
                    guard += 1;
                    if guard > 64 || r.overflowed() {
                        return None;
                    }
                }
            }
            // `SV_BuildSoundMsg`, sv_main.cpp:786-799. Worth stepping over
            // properly rather than abandoning the datagram: gunfire is a
            // genuine perception cue we will want later.
            svc::SVC_SOUND => {
                let mask = r.read_bits(9);
                if mask & 0x01 != 0 {
                    r.skip(8); // volume
                }
                if mask & 0x02 != 0 {
                    r.skip(8); // attenuation
                }
                r.skip(3); // channel
                r.skip(11); // entity index
                r.skip(if mask & 0x04 != 0 { 16 } else { 8 }); // sound number
                read_bit_vec3_coord(&mut r);
                if mask & 0x08 != 0 {
                    r.skip(8); // pitch
                }
            }
            _ => return None,
        }
        if r.overflowed() {
            return None;
        }
        Some(body + block_bytes(&r))
    }

    /// Other players, as the bot sees them.
    pub fn players(&self) -> Vec<PlayerView> {
        let me = self.my_entity();
        self.entities
            .iter()
            .filter(|e| {
                e.number >= 1 && e.number <= u16::from(self.maxclients) && e.number != me
            })
            .map(|e| PlayerView {
                entity: e.number,
                origin: e.origin(),
                angles: e.angles(),
                team: self
                    .game
                    .player(e.number as u8)
                    .map(|p| p.team)
                    .unwrap_or_default(),
                // `usehull` is 1 while ducking (`delta.lst:171`), which moves
                // the head down by half a body -- it matters for aim.
                ducking: e.i64("usehull") == 1,
            })
            .collect()
    }
}

/// One other player, projected out of the entity frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerView {
    pub entity: u16,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    pub team: crate::usermsg::Team,
    pub ducking: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::bitbuf::BitWriter;
    use proto::delta::{write_delta, FieldDesc};

    const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");

    fn registry() -> DeltaRegistry {
        crate::signon::walk(SIGNON).registry
    }

    /// Build a datagram the way `SV_SendClientDatagram` does, so the parser is
    /// tested against the real `clientdata_t` table rather than a toy one.
    fn datagram(
        time: f32,
        fields: &HashMap<String, Value>,
        table: &[FieldDesc],
        preamble: &[u8],
    ) -> Vec<u8> {
        let mut out = vec![svc::SVC_TIME];
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(preamble);
        out.push(svc::SVC_CLIENTDATA);

        let mut w = BitWriter::new();
        w.write_bits(0, 1); // has_delta = 0
        write_delta(&mut w, table, fields);
        w.write_bits(0, 1); // no weapon entries
        out.extend_from_slice(&w.into_bytes());
        out
    }

    fn sample() -> HashMap<String, Value> {
        let mut f = HashMap::new();
        f.insert("origin[0]".into(), Value::Float(1234.5));
        f.insert("origin[1]".into(), Value::Float(-678.25));
        f.insert("origin[2]".into(), Value::Float(36.0));
        f.insert("velocity[0]".into(), Value::Float(250.0));
        f.insert("health".into(), Value::Float(87.0));
        f.insert("maxspeed".into(), Value::Float(250.0));
        f
    }

    #[test]
    fn a_clientdata_datagram_round_trips() {
        let reg = registry();
        let table = reg.get("clientdata_t").expect("clientdata_t").clone();
        let msg = datagram(12.5, &sample(), &table, &[]);

        let cd = parse_datagram(&msg, &reg).expect("parses");
        assert!((cd.time - 12.5).abs() < 1e-3);
        let o = cd.origin();
        assert!((o[0] - 1234.5).abs() < 0.1, "origin[0] = {}", o[0]);
        assert!((o[1] + 678.25).abs() < 0.1, "origin[1] = {}", o[1]);
        assert!((cd.health() - 87.0).abs() < 0.5);
        assert!((cd.maxspeed() - 250.0).abs() < 0.5);
        assert!((cd.speed() - 250.0).abs() < 1.0);
    }

    #[test]
    fn the_optional_preamble_messages_are_skipped() {
        let reg = registry();
        let table = reg.get("clientdata_t").expect("clientdata_t").clone();

        // svc_choke, then svc_setangle with pitch 0 / yaw 90 / roll 0.
        let yaw = ((90.0f32 / 360.0) * 65536.0) as i16;
        let mut pre = vec![svc::SVC_CHOKE, svc::SVC_SETANGLE];
        pre.extend_from_slice(&0i16.to_le_bytes());
        pre.extend_from_slice(&yaw.to_le_bytes());
        pre.extend_from_slice(&0i16.to_le_bytes());

        let msg = datagram(1.0, &sample(), &table, &pre);
        let cd = parse_datagram(&msg, &reg).expect("parses past the preamble");
        let a = cd.forced_angles.expect("setangle captured");
        assert!((a[1] - 90.0).abs() < 0.1, "yaw {} should be ~90", a[1]);
        assert!((cd.origin()[0] - 1234.5).abs() < 0.1);
    }

    #[test]
    fn a_non_datagram_message_is_rejected_rather_than_guessed_at() {
        let reg = registry();
        assert!(parse_datagram(&[], &reg).is_none());
        assert!(parse_datagram(&[svc::SVC_PRINT, b'h', b'i', 0], &reg).is_none());
        // svc_time but no clientdata behind it.
        let mut m = vec![svc::SVC_TIME];
        m.extend_from_slice(&1.0f32.to_le_bytes());
        m.push(svc::SVC_PRINT);
        assert!(parse_datagram(&m, &reg).is_none());
    }

    #[test]
    fn a_delta_compressed_clientdata_is_refused_not_misread() {
        let reg = registry();
        let mut out = vec![svc::SVC_TIME];
        out.extend_from_slice(&1.0f32.to_le_bytes());
        out.push(svc::SVC_CLIENTDATA);
        let mut w = BitWriter::new();
        w.write_bits(1, 1); // has_delta -- we cannot reconstruct the base
        w.write_bits(7, 8);
        out.extend_from_slice(&w.into_bytes());
        assert!(parse_datagram(&out, &reg).is_none());
    }

    #[test]
    fn flag_helpers_read_the_right_bits() {
        let mut cd = ClientData::default();
        cd.fields.insert("iuser3".into(), Value::Int(1 << 2));
        assert!(cd.in_bomb_zone());
        cd.fields.insert("iuser3".into(), Value::Int(1 << 1));
        assert!(!cd.in_bomb_zone());

        cd.fields.insert("deadflag".into(), Value::Int(0));
        assert!(cd.alive());
        cd.fields.insert("deadflag".into(), Value::Int(2));
        assert!(!cd.alive());
    }

    /// Deltas carry only fields that differ from the base, so a live player's
    /// `deadflag` (DEAD_NO == 0) never appears on the wire at all. Reading
    /// "absent" as "dead" made a perfectly healthy bot look like a corpse.
    #[test]
    fn an_absent_deadflag_means_alive_not_dead() {
        let cd = ClientData::default();
        assert!(cd.alive(), "a field the server omitted is zero, not unknown");
    }

    /// Values taken from live traces of both states. The join camera is the
    /// deceptive one: full health, DEAD_NO, and an origin that moves -- by
    /// teleporting between spawn points. Only maxspeed tells them apart.
    #[test]
    fn maxspeed_separates_a_spawned_player_from_the_join_camera() {
        let mut cd = ClientData::default();
        cd.fields.insert("health".into(), Value::Float(100.0));

        cd.fields.insert("maxspeed".into(), Value::Float(1.0));
        assert!(!cd.in_game(), "maxspeed 1 is the join camera / freeze period");

        cd.fields.insert("maxspeed".into(), Value::Float(240.0));
        assert!(cd.in_game(), "maxspeed 240 is a spawned player");

        // Heavy weapons drop it, but never near 1.
        cd.fields.insert("maxspeed".into(), Value::Float(210.0));
        assert!(cd.in_game(), "an AWP carrier is still in the game");

        // Dead outranks everything.
        cd.fields.insert("deadflag".into(), Value::Int(2));
        assert!(!cd.in_game());
    }

    // -----------------------------------------------------------------------
    // svc_spawnbaseline is located, never hunted for
    //
    // The regression these pin down: `absorb_baselines` (removed) used to scan the whole
    // datagram for a bare byte 22 before the walk started, and a hit made
    // `feed` return. A payload byte is not a message, so the scan false-
    // positived constantly (14 151 - 24 279 datagrams per capture in
    // `captures/swarm/`), and each hit cost the WHOLE datagram -- including the
    // messages in front of the false 22, which were never even walked.
    // -----------------------------------------------------------------------

    /// The real 549 bytes that were once decoded as nine baselines and 63
    /// instanced baselines: `captures/swarm/Bot02.bin` record 165, from the byte
    /// after the stray `0x16`. See `crates/client/tests/spawnbaseline_guard.rs`.
    const FALSE_POSITIVE: &[u8] = include_bytes!("../tests/fixtures/false_spawnbaseline.bin");

    // Ids are arbitrary (the server assigns them per map via `svc_newusermsg`);
    // the sizes are the ones ReGameDLL registers. `SayText` and `TeamInfo` go in
    // with -1, i.e. length-prefixed; `ScoreInfo` is a fixed 9 bytes and `Money`
    // a fixed 5 (`i32 amount`, `byte blink` -- see `usermsg.rs:745-748`).
    const SAYTEXT: u8 = 76;
    const SCOREINFO: u8 = 85;
    const TEAMINFO: u8 = 86;
    const MONEY: u8 = 88;

    fn user_table() -> crate::stream::UserMsgTable {
        use crate::stream::UserMsgDef;
        let mut t = crate::stream::UserMsgTable::new();
        for (id, name, size) in [
            (SAYTEXT, "SayText", 255u8),
            (TEAMINFO, "TeamInfo", 255),
            (SCOREINFO, "ScoreInfo", 9),
            (MONEY, "Money", 5),
        ] {
            t.insert(id, UserMsgDef { name: name.into(), size });
        }
        t
    }

    fn decoder() -> Decoder {
        Decoder::new(&crate::signon::walk(SIGNON), user_table())
    }

    /// `Money`: `i32 amount`, `byte blink`.
    fn money(amount: i32) -> Vec<u8> {
        let mut m = vec![MONEY];
        m.extend_from_slice(&amount.to_le_bytes());
        m.push(0);
        m
    }

    /// `TeamInfo`: `byte client`, then the team name as a C string.
    fn team_info(client: u8, team: &str) -> Vec<u8> {
        let mut payload = vec![client];
        payload.extend_from_slice(team.as_bytes());
        payload.push(0);
        let mut m = vec![TEAMINFO, payload.len() as u8];
        m.extend_from_slice(&payload);
        m
    }

    /// `ScoreInfo`: `byte client`, then four shorts.
    fn score_info(client: u8, frags: i16) -> Vec<u8> {
        let mut m = vec![SCOREINFO, client];
        for v in [frags, 0i16, 0, 0] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m
    }

    /// One `svc_spawnbaseline` bit block, written the way `SV_CreateBaseline`
    /// writes it (`sv_main.cpp:5891-5915`): `11` bits of entity number, `2` of
    /// entityType, the delta, then the `0xFFFF` sentinel and a 6-bit instanced
    /// count. ReGameDLL creates no instanced baselines, so that count is zero.
    fn baseline_block(
        reg: &DeltaRegistry,
        maxclients: u8,
        ents: &[(u16, HashMap<String, Value>)],
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        for (number, f) in ents {
            w.write_bits(u32::from(*number), 11);
            w.write_bits(u32::from(proto::entity::ENTITY_NORMAL), 2);
            let which = proto::entity::table_for(*number, false, maxclients);
            write_delta(&mut w, reg.get(which.name()).expect("table"), f);
        }
        w.write_bits(0xFFFF, 16);
        w.write_bits(0, 6);
        w.into_bytes()
    }

    /// A byte 22 inside a user-message payload must never reach the baseline
    /// parser, and must not cost the messages behind it.
    ///
    /// `Money 5654` is `16 16 00 00` on the wire — two byte-22s in one
    /// perfectly ordinary message. The old scan stopped on the first of them.
    #[test]
    fn a_stray_byte_22_in_a_payload_is_neither_baselines_nor_a_lost_datagram() {
        let mut msg = money(5654);
        assert!(msg.contains(&svc::SVC_SPAWNBASELINE), "no byte 22 to trip on");
        // ScoreInfo carries a team id of its own, so it goes first: the last
        // writer wins and the assertion below is about TeamInfo.
        msg.extend_from_slice(&score_info(3, 7));
        msg.extend_from_slice(&team_info(3, "CT"));

        let mut d = decoder();
        d.feed(&msg);

        assert!(d.baselines.by_number.is_empty(), "payload byte read as baselines");
        assert_eq!(d.stats.baseline_errors, 0, "the parser was never even offered it");
        assert_eq!(d.game.money, 5654);
        let p = d.game.player(3).expect("slot 3");
        assert_eq!(p.team, crate::usermsg::Team::CounterTerrorist, "TeamInfo behind the 22 was lost");
        assert_eq!(p.frags, 7, "ScoreInfo behind the 22 was lost");
    }

    /// The same thing with the bytes that actually did it, carried the way they
    /// actually arrived: inside user-message payloads.
    ///
    /// `SayText` is registered with -1, so it is length-prefixed and the walker
    /// steps over its payload without looking inside — which is the whole point.
    #[test]
    fn the_real_false_positive_burst_walks_through_intact() {
        // The stray 0x16 and everything the old scan handed to the baseline
        // parser, chunked into payloads (a length-prefixed message tops out at
        // 255 bytes).
        let real: Vec<u8> = std::iter::once(svc::SVC_SPAWNBASELINE)
            .chain(FALSE_POSITIVE.iter().copied())
            .collect();
        let mut msg = Vec::new();
        msg.extend_from_slice(&score_info(1, 0));
        for chunk in real.chunks(200) {
            msg.push(SAYTEXT);
            msg.push(chunk.len() as u8);
            msg.extend_from_slice(chunk);
        }
        // The messages the old code threw away with the rest of the datagram.
        msg.extend_from_slice(&money(3300));
        msg.extend_from_slice(&team_info(4, "TERRORIST"));

        // A scan would fire; the walk must not.
        assert!(
            msg.iter().any(|&b| b == svc::SVC_SPAWNBASELINE),
            "fixture has no byte 22, so this test proves nothing"
        );

        let mut d = decoder();
        d.feed(&msg);

        assert!(d.baselines.by_number.is_empty());
        assert!(d.baselines.instanced.is_empty(), "63 bogus instanced baselines are back");
        assert_eq!(d.stats.baseline_errors, 0);
        assert_eq!(d.stats.partial, 0, "the walk did not reach the end of the datagram");
        assert_eq!(d.stats.last_stop, None);
        assert_eq!(d.game.money, 3300, "the Money behind the burst was lost");
        assert_eq!(
            d.game.player(4).expect("slot 4").team,
            crate::usermsg::Team::Terrorist,
            "the TeamInfo behind the burst was lost"
        );
    }

    /// The genuine article, in the shape `SV_Spawn_f_internal` sends it: the
    /// signon buffer's `svc_spawnbaseline` with `SV_WriteSpawn`'s messages
    /// behind it (`sv_main.cpp:1671-1672`). It must be found by the walker, and
    /// the walk must resume past the bit block rather than end there.
    #[test]
    fn a_real_spawnbaseline_is_located_by_the_walker_and_stepped_over() {
        let signon = crate::signon::walk(SIGNON);
        let maxclients = signon.server_info.as_ref().map(|s| s.max_players).unwrap_or(32);

        let mut world = HashMap::new();
        world.insert("modelindex".to_string(), Value::Int(1));
        let mut player = HashMap::new();
        player.insert("origin[0]".to_string(), Value::Float(256.0));
        player.insert("health".to_string(), Value::Float(100.0));
        let block = baseline_block(
            &signon.registry,
            maxclients,
            &[(0, world), (1, player.clone())],
        );

        let mut msg = score_info(2, 3);
        msg.push(svc::SVC_SPAWNBASELINE);
        msg.extend_from_slice(&block);
        msg.extend_from_slice(&team_info(2, "CT"));

        let mut d = decoder();
        d.feed(&msg);

        assert_eq!(d.stats.baseline_blocks, 1, "the walker never offered the block");
        assert_eq!(d.stats.baseline_errors, 0);
        let mut nums: Vec<u16> = d.baselines.by_number.keys().copied().collect();
        nums.sort();
        assert_eq!(nums, vec![0, 1]);
        assert!(d.baselines.instanced.is_empty());
        assert_eq!(d.baselines.by_number[&1].f32("origin[0]"), 256.0);

        // And the block was stepped over exactly, not merely parsed.
        assert_eq!(d.stats.partial, 0, "walk stopped at {:?}", d.stats.last_stop);
        assert_eq!(
            d.game.player(2).expect("slot 2").team,
            crate::usermsg::Team::CounterTerrorist,
            "the walk did not resume behind the baseline block"
        );
    }

    /// The residual case: a byte 22 that really is at a message boundary but is
    /// not a baseline block. It cannot be stepped over — a bit-packed block of
    /// unknown length has no length — so the tail is lost, exactly as it is for
    /// any other bit-packed failure. What must NOT happen is the old behaviour:
    /// losing the messages in front of it too.
    #[test]
    fn a_boundary_byte_22_that_is_not_baselines_costs_only_the_tail() {
        let mut msg = money(1000);
        msg.extend_from_slice(&team_info(5, "CT"));
        msg.push(svc::SVC_SPAWNBASELINE);
        msg.extend_from_slice(FALSE_POSITIVE);

        let mut d = decoder();
        d.feed(&msg);

        assert!(d.baselines.by_number.is_empty(), "garbage installed as baselines");
        assert_eq!(d.stats.baseline_errors, 1);
        assert!(d.stats.last_baseline_error.is_some());
        assert_eq!(d.stats.partial, 1);
        assert_eq!(d.stats.last_stop, Some(svc::SVC_SPAWNBASELINE));
        // Everything ahead of it survived.
        assert_eq!(d.game.money, 1000);
        assert_eq!(
            d.game.player(5).expect("slot 5").team,
            crate::usermsg::Team::CounterTerrorist
        );
    }
}

// ---------------------------------------------------------------------------
// A note on detecting "has spawned", because two obvious answers are both wrong
// and both cost time.
//
//  * `maxspeed > 1.5` is NOT it. `GetIntoGame` calls `ResetMaxSpeed()` at
//    player.cpp:10718, BEFORE the `if (FPlayerCanRespawn(this)) Spawn()` gate
//    at :10730 -- so maxspeed reaches 240 on merely ENTERING the game.
//  * `ResetHUD` is NOT it either. It fires from `m_fInitHUD`, which `Spawn()`
//    sets (player.cpp:5997) but so do `Precache()` (:6146) and
//    `ForceClientDllUpdate()` (:6694).
//
// The reliable evidence that a player is really in the world is holding a
// weapon: every spawn gives a knife, and `CurWeapon` is emitted when one is
// deployed (weapons.cpp:1380). Absence of any weapon means absence of a spawn,
// whatever maxspeed and ResetHUD claim.
