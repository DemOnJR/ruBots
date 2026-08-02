//! GoldSrc protocol-48 entity updates.
//!
//! This is the layer that turns a bit stream into "who is standing where":
//! `svc_packetentities` (40), `svc_deltapacketentities` (41) and
//! `svc_spawnbaseline` (22).
//!
//! Everything here is transcribed from two sources, both read directly:
//!
//! * the **writer** — `SV_WriteDeltaHeader` (`rehlds/engine/sv_main.cpp:4411-4475`),
//!   `SV_CreatePacketEntities_internal` (`:4547-4725`) and `SV_CreateBaseline`
//!   (`:5824-5918`). The writer is the authority: whatever it emitted is what
//!   is on the wire.
//! * the **reference decoder** — `World::ParseDeltaHeader`
//!   (`rehlds/HLTV/Core/src/World.cpp:1065-1131`), the delta and full
//!   packet-entities readers (`:1477-1635` and `:1637-1708`) and
//!   `World::ParseBaseline` (`:2003-2071`).
//!
//! The caller owns the byte-aligned framing (the opcode byte, `short
//! num_entities`, and for a delta packet the `byte delta_sequence`); the
//! functions here start at the first bit of the packed block and stop having
//! consumed the 16-bit terminator exactly, leaving the reader on the last bit
//! of the block. GoldSrc pads the block to a byte boundary
//! (`MSG_EndBitWriting`), so the caller realigns with
//! [`BitReader::align`](crate::bitbuf::BitReader::align) afterwards.
//!
//! # The three things that are easy to get wrong
//!
//! 1. **An unchanged entity emits nothing at all — not even a header.**
//!    `_DELTA_WriteDelta` wraps the header callback *and* the payload in
//!    `if (sendfields || force)` (`rehlds/engine/delta.cpp:838-853`), and the
//!    `newnum == oldnum` branch is the only caller that passes `force =
//!    FALSE` (`sv_main.cpp:4611`). So a delta packet is a *sparse* list, and
//!    the decoder reconstructs the rest by copying forward from the previous
//!    frame: the catch-up loop before each header (`World.cpp:1516-1535`) and
//!    the tail loop after the terminator (`World.cpp:1620-1632`). Skip either
//!    and every entity that merely stopped moving vanishes from the bot's view.
//!
//! 2. **`baseline_offset` indexes the *new* frame by position, not by entity
//!    number**: `baseline = &entity[newindex - offset]` (`World.cpp:1586` and
//!    `:1680`), matching `SV_FindBestBaseline` which returns `index -
//!    bestfound` over `to->entities[]` (`sv_main.cpp:4491-4533`). That is why
//!    the output here is an ordered [`Vec`] and the offset is applied to
//!    `out.len()`, never to an entity number.
//!
//! 3. **Two header bits are conditional.** The instanced-baseline bit exists
//!    only when the map has instanced baselines at all (`if
//!    (g_psv.instance_baselines->number)`, `sv_main.cpp:4450`; client side
//!    `if (m_MaxInstanced_BaseLine)`, `World.cpp:1111`), and the
//!    `baseline_offset` bit only when `full && !newbl`
//!    (`sv_main.cpp:4462`, `World.cpp:1121`). Read a bit that was never
//!    written and every following header is shifted by one — which decodes as
//!    plausible garbage, not as an error. ReGameDLL-CS creates zero instanced
//!    baselines, so on a stock CS server that bit is absent; this module still
//!    gates on the count parsed from `svc_spawnbaseline`
//!    ([`PacketCtx::instanced`]) rather than assuming either way.
//!
//!    That gate is only as good as the count feeding it, which is why
//!    [`parse_spawn_baseline`] validates rather than trusts. A live 4-bot run
//!    lost 7263 consecutive entity frames because a user-message burst was
//!    mistaken for a `svc_spawnbaseline` and decoded as 63 instanced
//!    baselines: no error anywhere, just one extra bit in every header from
//!    then on. The story is in that function's docs.

use crate::bitbuf::{BitReader, BitWriter};
use crate::delta::{parse_delta, DeltaRegistry, DeltaTable, Value};
use std::collections::HashMap;
use std::fmt;

/// Bits an entity number occupies in the absolute form of a header.
/// `MAX_EDICT_BITS` — `rehlds/common/const.h:26`.
pub const MAX_EDICT_BITS: u32 = 11;
/// `MAX_EDICTS` — `rehlds/common/const.h:28`, `1 << MAX_EDICT_BITS`.
pub const MAX_EDICTS: u16 = 1 << MAX_EDICT_BITS;
/// Bits of the "+delta" entity-number form.
/// `DELTA_OFFSET_BITS` — `rehlds/HLTV/common/net_internal.h:101`.
pub const DELTA_OFFSET_BITS: u32 = 6;
/// Bits of an instanced-baseline index and of `baseline_offset`.
/// `MAX_BASELINE_BITS` — `rehlds/HLTV/Core/src/World.h:42`.
pub const MAX_BASELINE_BITS: u32 = 6;
/// `MAX_PACKET_ENTITIES` — `rehlds/common/qlimits.h:42`.
pub const MAX_PACKET_ENTITIES: usize = 256;
/// `MAX_INSTANCED_BASELINES` — `rehlds/HLTV/Core/src/World.h:217`.
pub const MAX_INSTANCED_BASELINES: usize = 64;
/// Sentinel entity number used by the merge walk on both sides for "there is
/// no entity here" — `rehlds/engine/delta_packet.h:34`.
pub const ENTITY_SENTINEL: u32 = 9999;

/// `ENTITY_NORMAL` — `rehlds/common/entity_state.h:26`.
pub const ENTITY_NORMAL: u8 = 1;
/// `ENTITY_BEAM` — `rehlds/common/entity_state.h:27`. A beam is encoded with
/// the `custom_entity_state_t` table.
pub const ENTITY_BEAM: u8 = 2;

/// Delta table names, as they arrive in `svc_deltadescription`.
pub const TABLE_ENTITY: &str = "entity_state_t";
pub const TABLE_PLAYER: &str = "entity_state_player_t";
pub const TABLE_CUSTOM: &str = "custom_entity_state_t";

/// The 16-bit value that ends the baseline list in `svc_spawnbaseline`
/// (`sv_main.cpp:5912`; the reader peeks for it at `World.cpp:2027`).
const BASELINE_SENTINEL: u32 = 0xFFFF;

// ---------------------------------------------------------------------------
// Entity header
// ---------------------------------------------------------------------------

/// One decoded entity header, the variable-width preamble in front of every
/// entity's delta payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeltaHeader {
    /// Entity number, 0..=2047 on a well-formed stream.
    pub number: u16,
    /// The entity left the server's PVS/world. Only ever set in a delta
    /// packet: in a full packet the leading bit means "+1 shortcut" instead
    /// (`sv_main.cpp:4416-4430`), so `remove` is structurally impossible there.
    pub remove: bool,
    /// Use the `custom_entity_state_t` table (the entity is an `ENTITY_BEAM`).
    pub custom: bool,
    /// Index into the instanced-baseline array, when the server chose one.
    /// `None` both when the bit said "no" and when the bit was never written
    /// because the map has no instanced baselines.
    pub new_baseline: Option<u8>,
    /// Positional back-reference into the **new** frame: the baseline is
    /// `out[out.len() - baseline_offset]`. Zero means "no offset". Only ever
    /// non-zero in a full packet.
    pub baseline_offset: u8,
}

/// Decode one entity header.
///
/// Transcribed bit for bit from `SV_WriteDeltaHeader`
/// (`sv_main.cpp:4411-4475`) and cross-checked against `World::ParseDeltaHeader`
/// (`World.cpp:1065-1131`):
///
/// ```text
/// delta = num - numbase;
/// if (full) bits(delta == 1, 1);           // "+1 shortcut"
/// else      bits(remove, 1);               // removal
/// if (!full || delta != 1) {
///     if (delta <= 0 || delta > 63) { bits(1,1); bits(num, 11); }
///     else                          { bits(0,1); bits(delta, 6); }
/// }
/// numbase = num;
/// if (!remove) {
///     bits(custom, 1);
///     if (instanced_baselines) { newbl ? (bits(1,1), bits(idx,6)) : bits(0,1); }
///     if (full && !newbl)      { offset ? (bits(1,1), bits(off,6)) : bits(0,1); }
/// }
/// ```
///
/// `numbase` is threaded across the whole packet and is updated here, exactly
/// as the writer updates its own copy — the two must stay in lockstep or the
/// 6-bit form decodes to the wrong entity.
///
/// `instanced_baselines` is the count the map actually has (see trap 3 in the
/// module docs). Pass `ctx.instanced.len()`, never a constant.
///
/// Errors are not reported here, mirroring the C: a truncated stream sets the
/// reader's sticky overflow flag, which the packet parsers check.
pub fn parse_delta_header(
    r: &mut BitReader,
    numbase: &mut i32,
    full: bool,
    instanced_baselines: usize,
) -> DeltaHeader {
    let mut h = DeltaHeader::default();

    // In a full packet the leading bit is the "+1 shortcut"; in a delta packet
    // it is the removal flag. The two cases never overlap: a full packet has
    // no removals and a delta packet has no shortcut.
    let is_plus_one = if full {
        r.read_bit() != 0
    } else {
        h.remove = r.read_bit() != 0;
        false
    };

    let num: i32 = if is_plus_one {
        *numbase + 1
    } else if r.read_bit() != 0 {
        r.read_bits(MAX_EDICT_BITS) as i32
    } else {
        *numbase + r.read_bits(DELTA_OFFSET_BITS) as i32
    };

    *numbase = num;
    // The 6-bit form is additive and unbounded, so a hostile stream could walk
    // `numbase` past 65535. Saturate rather than wrap; the packet parsers
    // reject anything >= MAX_EDICTS anyway (`World.cpp:1660`).
    h.number = num.clamp(0, u16::MAX as i32) as u16;

    if !h.remove {
        h.custom = r.read_bit() != 0;

        // Conditional bit #1: written only when the map has instanced
        // baselines (`sv_main.cpp:4450` / `World.cpp:1111`).
        if instanced_baselines != 0 && r.read_bit() != 0 {
            h.new_baseline = Some(r.read_bits(MAX_BASELINE_BITS) as u8);
        }

        // Conditional bit #2: written only in a full packet, and only when the
        // server did not already point at an instanced baseline
        // (`sv_main.cpp:4462` / `World.cpp:1121`).
        if full && h.new_baseline.is_none() && r.read_bit() != 0 {
            h.baseline_offset = r.read_bits(MAX_BASELINE_BITS) as u8;
        }
    }

    h
}

/// Encode one entity header — the exact inverse of [`parse_delta_header`], and
/// a direct transcription of `SV_WriteDeltaHeader`.
///
/// Useful for a replay/proxy, and it is what the tests use to build inputs.
/// `h.remove` is ignored when `full`, because the writer never emits a removal
/// in a full packet (it spends that bit on the "+1 shortcut").
pub fn write_delta_header(
    w: &mut BitWriter,
    h: &DeltaHeader,
    numbase: &mut i32,
    full: bool,
    instanced_baselines: usize,
) {
    let num = i32::from(h.number);
    let delta = num - *numbase;
    let remove = !full && h.remove;

    if full {
        w.write_bits(u32::from(delta == 1), 1);
    } else {
        w.write_bits(u32::from(remove), 1);
    }

    if !full || delta != 1 {
        if delta <= 0 || delta > 63 {
            w.write_bits(1, 1);
            // `MSG_WriteBits` clamps to (1<<n)-1 (`common.cpp:398-400`).
            w.write_bits((num as u32).min((1 << MAX_EDICT_BITS) - 1), MAX_EDICT_BITS);
        } else {
            w.write_bits(0, 1);
            w.write_bits(delta as u32, DELTA_OFFSET_BITS);
        }
    }

    *numbase = num;

    if !remove {
        w.write_bits(u32::from(h.custom), 1);
        if instanced_baselines != 0 {
            match h.new_baseline {
                Some(idx) => {
                    w.write_bits(1, 1);
                    w.write_bits(u32::from(idx), MAX_BASELINE_BITS);
                }
                None => w.write_bits(0, 1),
            }
        }
        if full && h.new_baseline.is_none() {
            if h.baseline_offset != 0 {
                w.write_bits(1, 1);
                w.write_bits(u32::from(h.baseline_offset), MAX_BASELINE_BITS);
            } else {
                w.write_bits(0, 1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Table selection
// ---------------------------------------------------------------------------

/// Which of the three entity delta tables encodes a given entity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityTable {
    Entity,
    Player,
    Custom,
}

impl EntityTable {
    /// The name the server registered the table under in
    /// `svc_deltadescription`.
    pub fn name(self) -> &'static str {
        match self {
            EntityTable::Entity => TABLE_ENTITY,
            EntityTable::Player => TABLE_PLAYER,
            EntityTable::Custom => TABLE_CUSTOM,
        }
    }
}

/// `SV_IsPlayerIndex` — `index >= 1 && index <= maxclients`
/// (`rehlds/engine/sv_main.cpp:410`). Entity 0 is the world, never a player.
pub fn is_player_index(number: u16, maxclients: u8) -> bool {
    number >= 1 && number <= u16::from(maxclients)
}

/// Table selection, `sv_main.cpp:4611` / `:4669`:
/// `custom ? g_pcustomentitydelta : (SV_IsPlayerIndex(num) ? g_pplayerdelta : g_pentitydelta)`.
pub fn table_for(number: u16, custom: bool, maxclients: u8) -> EntityTable {
    if custom {
        EntityTable::Custom
    } else if is_player_index(number, maxclients) {
        EntityTable::Player
    } else {
        EntityTable::Entity
    }
}

// ---------------------------------------------------------------------------
// Entity state
// ---------------------------------------------------------------------------

/// One entity's state in a frame.
///
/// `fields` is the *accumulated* state, not just the fields that arrived in
/// this packet: the engine's `DELTA_ParseDelta` copies every unmarked field
/// from the `from` struct into the `to` struct (`delta.cpp:894-916`), so a
/// delta is an overlay on its base, and that is how it is applied here.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct EntityState {
    pub number: u16,
    /// `ENTITY_NORMAL` or `ENTITY_BEAM`; not a delta field, it is carried by
    /// the `custom` header bit (`World.cpp:1549`, `:1597`, `:1687`).
    pub entity_type: u8,
    pub fields: HashMap<String, Value>,
}

impl EntityState {
    /// `f32` value of a field, or `0.0` — absent means "never set", which in
    /// the engine's zeroed `entity_state_t` is exactly zero.
    pub fn f32(&self, k: &str) -> f32 {
        self.fields.get(k).and_then(Value::as_f32).unwrap_or(0.0)
    }

    /// `i64` value of a field, or `0`.
    pub fn i64(&self, k: &str) -> i64 {
        self.fields.get(k).and_then(Value::as_i64).unwrap_or(0)
    }

    /// `origin[0..2]` — where the entity is. The field names are literally
    /// `origin[0]`, `origin[1]`, `origin[2]`
    /// (`testserver/rehlds/cstrike/delta.lst:72,75,76`).
    pub fn origin(&self) -> [f32; 3] {
        [
            self.f32("origin[0]"),
            self.f32("origin[1]"),
            self.f32("origin[2]"),
        ]
    }

    /// `angles[0..2]` — where the entity is looking.
    pub fn angles(&self) -> [f32; 3] {
        [
            self.f32("angles[0]"),
            self.f32("angles[1]"),
            self.f32("angles[2]"),
        ]
    }

    /// True when this entity is encoded with the beam/custom table.
    pub fn is_beam(&self) -> bool {
        self.entity_type & ENTITY_BEAM != 0
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Everything a packet-entities block needs that is not in the block itself.
///
/// All of it is session state the caller accumulated earlier: the delta tables
/// from `svc_deltadescription`, and both baseline arrays from
/// `svc_spawnbaseline`.
pub struct PacketCtx<'a> {
    pub registry: &'a DeltaRegistry,
    /// Per-entity-number baselines, keyed by entity number
    /// (`m_BaseLines[num]`, `World.cpp:1594`).
    pub baselines: &'a HashMap<u16, EntityState>,
    /// Instanced baselines, addressed by position
    /// (`m_Instanced_BaseLines[newblindex]`, `World.cpp:1586`). Its length is
    /// also what gates the instanced-baseline header bit.
    pub instanced: &'a [EntityState],
    pub maxclients: u8,
}

impl<'a> PacketCtx<'a> {
    fn table(&self, number: u16, custom: bool) -> Result<&'a DeltaTable, EntityError> {
        let which = table_for(number, custom, self.maxclients);
        self.registry
            .get(which.name())
            .ok_or(EntityError::BadTable(which.name()))
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a block could not be decoded.
///
/// Every one of these is a case where the C would have carried on with
/// whatever bits it happened to find. A wrong entity stream is worse than no
/// entity stream — the bot would aim at ghosts — so they are returned instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityError {
    /// The reader ran past the end of the block.
    Overflow,
    /// The 16 zero bits that end a packet-entities block were not there.
    MissingTerminator,
    /// A delta table the stream needs was never registered.
    BadTable(&'static str),
    /// More than `MAX_PACKET_ENTITIES` entities in one frame
    /// (`World.cpp:1521`, `:1574`, `:1622`).
    TooManyEntities,
    /// The `short num_entities` in the byte-aligned header disagrees with what
    /// the bit block actually contained (`World.cpp:1702`).
    CountMismatch { expected: usize, got: usize },
    /// An entity number outside `0..MAX_EDICTS` (`World.cpp:1660`).
    BadEntityNumber(u16),
    /// `baseline_offset` pointed before the start of the new frame. In C this
    /// is an out-of-bounds read of `entity[newindex - offset]`.
    BadBaselineOffset { index: usize, offset: u8 },
    /// An instanced-baseline index with no such baseline.
    BadInstancedBaseline { index: u8, count: usize },
    /// A `svc_spawnbaseline` whose entity numbers are not strictly ascending.
    /// `SV_CreateBaseline` emits them straight out of its `entnum` loop counter
    /// (`sv_main.cpp:5891-5896`), so on a real one they always are.
    BaselineOutOfOrder { previous: u16, got: u16 },
    /// A baseline carrying an `entityType` the writer cannot produce.
    /// `SV_CreateBaseline` assigns exactly `ENTITY_BEAM` or `ENTITY_NORMAL`
    /// (`sv_main.cpp:5848-5851`) and sends the low two bits
    /// (`sv_main.cpp:5897`), so only 1 and 2 ever reach the wire.
    BadEntityType(u8),
    /// More baselines than there are entity slots (`MAX_EDICTS`).
    TooManyBaselines,
}

impl fmt::Display for EntityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntityError::Overflow => write!(f, "entity stream ran past the end of the block"),
            EntityError::MissingTerminator => write!(f, "missing 16-bit end tag"),
            EntityError::BadTable(t) => write!(f, "delta table {t} was never registered"),
            EntityError::TooManyEntities => {
                write!(f, "more than {MAX_PACKET_ENTITIES} entities in a frame")
            }
            EntityError::CountMismatch { expected, got } => {
                write!(f, "header said {expected} entities, block had {got}")
            }
            EntityError::BadEntityNumber(n) => write!(f, "entity number {n} >= {MAX_EDICTS}"),
            EntityError::BadBaselineOffset { index, offset } => {
                write!(f, "baseline offset {offset} underflows new-frame index {index}")
            }
            EntityError::BadInstancedBaseline { index, count } => {
                write!(f, "instanced baseline {index} of {count}")
            }
            EntityError::BaselineOutOfOrder { previous, got } => write!(
                f,
                "baseline entity {got} follows {previous}; the writer only counts up"
            ),
            EntityError::BadEntityType(t) => {
                write!(f, "baseline entityType {t} is neither NORMAL nor BEAM")
            }
            EntityError::TooManyBaselines => {
                write!(f, "more than {MAX_EDICTS} baselines")
            }
        }
    }
}

impl std::error::Error for EntityError {}

// ---------------------------------------------------------------------------
// Shared bits of the two packet-entities parsers
// ---------------------------------------------------------------------------

/// Resolve the base state a new entity's delta is applied to.
///
/// Three-way choice, `World.cpp:1583-1595` (delta) and `:1674-1685` (full):
/// instanced baseline, else positional offset into the new frame, else the
/// per-number baseline. `out` is the frame being built, so `out.len()` is the
/// `newindex` the offset is measured back from — **trap 2**.
fn baseline_fields(
    h: &DeltaHeader,
    ctx: &PacketCtx,
    out: &[EntityState],
) -> Result<HashMap<String, Value>, EntityError> {
    if let Some(idx) = h.new_baseline {
        return ctx
            .instanced
            .get(usize::from(idx))
            .map(|e| e.fields.clone())
            .ok_or(EntityError::BadInstancedBaseline {
                index: idx,
                count: ctx.instanced.len(),
            });
    }
    if h.baseline_offset != 0 {
        let newindex = out.len();
        let pos = newindex
            .checked_sub(usize::from(h.baseline_offset))
            .ok_or(EntityError::BadBaselineOffset {
                index: newindex,
                offset: h.baseline_offset,
            })?;
        return Ok(out[pos].fields.clone());
    }
    // A number with no baseline is not an error: `m_BaseLines[num]` is a
    // zeroed struct until `svc_spawnbaseline` fills it in.
    Ok(ctx
        .baselines
        .get(&h.number)
        .map(|e| e.fields.clone())
        .unwrap_or_default())
}

/// Apply a parsed delta on top of a base state. Fields not in the delta keep
/// the base's value (`DELTA_ParseDelta`, `delta.cpp:894-916`).
fn apply(mut base: HashMap<String, Value>, delta: HashMap<String, Value>) -> HashMap<String, Value> {
    base.extend(delta);
    base
}

fn push(out: &mut Vec<EntityState>, e: EntityState) -> Result<(), EntityError> {
    if out.len() >= MAX_PACKET_ENTITIES {
        return Err(EntityError::TooManyEntities);
    }
    out.push(e);
    Ok(())
}

fn check_number(h: &DeltaHeader) -> Result<(), EntityError> {
    if h.number >= MAX_EDICTS {
        return Err(EntityError::BadEntityNumber(h.number));
    }
    Ok(())
}

/// Consume the 16 zero bits that end a packet-entities block
/// (`sv_main.cpp:4721`; read back at `World.cpp:1609` / `:1695`).
fn read_terminator(r: &mut BitReader) -> Result<(), EntityError> {
    let tag = r.read_bits(16);
    if r.overflowed() {
        return Err(EntityError::Overflow);
    }
    if tag != 0 {
        return Err(EntityError::MissingTerminator);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// svc_packetentities (40) — full update
// ---------------------------------------------------------------------------

/// Decode the bit block of a `svc_packetentities` (40).
///
/// Mirror of `World::UncompressEntitiesFromStream` (the full overload,
/// `World.cpp:1637-1708`). A full packet has no previous frame: every entity
/// is new, deltas are taken from a baseline, and there are no removals.
pub fn parse_packet_entities_full(
    r: &mut BitReader,
    ctx: &PacketCtx,
) -> Result<Vec<EntityState>, EntityError> {
    let mut out: Vec<EntityState> = Vec::new();
    let mut numbase: i32 = 0;

    loop {
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        if r.peek_bits(16) == 0 {
            break;
        }

        let h = parse_delta_header(r, &mut numbase, true, ctx.instanced.len());
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        check_number(&h)?;

        let base = baseline_fields(&h, ctx, &out)?;
        let table = ctx.table(h.number, h.custom)?;
        let delta = parse_delta(r, table);
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }

        push(
            &mut out,
            EntityState {
                number: h.number,
                entity_type: if h.custom { ENTITY_BEAM } else { ENTITY_NORMAL },
                fields: apply(base, delta),
            },
        )?;
    }

    read_terminator(r)?;
    Ok(out)
}

/// [`parse_packet_entities_full`] plus the `newindex != entnum` check the
/// reference decoder makes against the `short num_entities` in the
/// byte-aligned header (`World.cpp:1702`).
pub fn parse_packet_entities_full_checked(
    r: &mut BitReader,
    ctx: &PacketCtx,
    expected: usize,
) -> Result<Vec<EntityState>, EntityError> {
    let out = parse_packet_entities_full(r, ctx)?;
    if out.len() != expected {
        return Err(EntityError::CountMismatch {
            expected,
            got: out.len(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// svc_deltapacketentities (41) — delta update
// ---------------------------------------------------------------------------

/// Decode the bit block of a `svc_deltapacketentities` (41) against the frame
/// it deltas from.
///
/// Mirror of `World::UncompressEntitiesFromStream` (the delta overload,
/// `World.cpp:1477-1635`). `from` is the acknowledged frame, in ascending
/// entity-number order — the same order the server walks it in
/// (`sv_main.cpp:4594-4718`).
///
/// The two copy-forward loops are **trap 1**: the writer emits nothing at all
/// for an entity whose state did not change, so anything in `from` that the
/// stream does not mention has to be carried over verbatim.
pub fn parse_packet_entities_delta(
    r: &mut BitReader,
    from: &[EntityState],
    ctx: &PacketCtx,
) -> Result<Vec<EntityState>, EntityError> {
    let mut out: Vec<EntityState> = Vec::new();
    let mut numbase: i32 = 0;
    let mut oldindex: usize = 0;

    // `oldnum = (oldindex >= from.len()) ? ENTITY_SENTINEL : from[oldindex].number`
    // (`World.cpp:1514`). The sentinel is what makes "past the end of the old
    // frame" sort after every real entity number.
    let old_num = |i: usize| -> u32 {
        from.get(i)
            .map(|e| u32::from(e.number))
            .unwrap_or(ENTITY_SENTINEL)
    };

    loop {
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        if r.peek_bits(16) == 0 {
            break;
        }

        let h = parse_delta_header(r, &mut numbase, false, ctx.instanced.len());
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        check_number(&h)?;
        let newnum = u32::from(h.number);

        // Catch-up loop (`World.cpp:1516-1535`): every old entity numbered
        // below this header simply did not change, so it was not transmitted.
        while old_num(oldindex) < newnum {
            push(&mut out, from[oldindex].clone())?;
            oldindex += 1;
        }

        let oldnum = old_num(oldindex);

        if newnum == oldnum {
            // The entity is in both frames: delta from its previous state.
            if h.remove {
                oldindex += 1;
                continue;
            }
            let table = ctx.table(h.number, h.custom)?;
            let delta = parse_delta(r, table);
            if r.overflowed() {
                return Err(EntityError::Overflow);
            }
            push(
                &mut out,
                EntityState {
                    number: h.number,
                    entity_type: if h.custom { ENTITY_BEAM } else { ENTITY_NORMAL },
                    fields: apply(from[oldindex].fields.clone(), delta),
                },
            )?;
            oldindex += 1;
            continue;
        }

        // newnum < oldnum: not in the old frame, so delta from a baseline.
        // A removal here has nothing to remove; the reference decoder just
        // skips it (`World.cpp:1569-1572`) and no payload follows.
        if h.remove {
            continue;
        }

        let base = baseline_fields(&h, ctx, &out)?;
        let table = ctx.table(h.number, h.custom)?;
        let delta = parse_delta(r, table);
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        push(
            &mut out,
            EntityState {
                number: h.number,
                entity_type: if h.custom { ENTITY_BEAM } else { ENTITY_NORMAL },
                fields: apply(base, delta),
            },
        )?;
    }

    read_terminator(r)?;

    // Tail of the catch-up (`World.cpp:1620-1632`): everything left in the old
    // frame outlived the last header and was likewise not transmitted.
    while oldindex < from.len() {
        push(&mut out, from[oldindex].clone())?;
        oldindex += 1;
    }

    Ok(out)
}

/// [`parse_packet_entities_delta`] plus the count check against the
/// `short num_entities` in the byte-aligned header.
pub fn parse_packet_entities_delta_checked(
    r: &mut BitReader,
    from: &[EntityState],
    ctx: &PacketCtx,
    expected: usize,
) -> Result<Vec<EntityState>, EntityError> {
    let out = parse_packet_entities_delta(r, from, ctx)?;
    if out.len() != expected {
        return Err(EntityError::CountMismatch {
            expected,
            got: out.len(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// svc_spawnbaseline (22)
// ---------------------------------------------------------------------------

/// The two baseline arrays a `svc_spawnbaseline` carries.
#[derive(Clone, Debug, Default)]
pub struct Baselines {
    /// Per-entity-number baselines (`m_BaseLines`).
    pub by_number: HashMap<u16, EntityState>,
    /// Instanced baselines (`m_Instanced_BaseLines`), addressed by position.
    /// Its length gates the instanced-baseline header bit — feed it straight
    /// into [`PacketCtx::instanced`].
    pub instanced: Vec<EntityState>,
}

/// Decode the bit block of a `svc_spawnbaseline` (22).
///
/// Writer: `SV_CreateBaseline`, `sv_main.cpp:5889-5917`. Reader:
/// `World::ParseBaseline`, `World.cpp:2003-2071`.
///
/// ```text
/// while peek(16) != 0xFFFF { bits(entnum,11); bits(entityType,2); delta }
/// bits(0xFFFF,16)
/// bits(count,6)
/// count x delta                      // ALWAYS entity_state_t
/// ```
///
/// Two details that are not obvious:
///
/// * the instanced baselines use `entity_state_t` unconditionally — never the
///   player table, whatever index they end up standing in for
///   (`sv_main.cpp:5915`, `World.cpp:2062`).
/// * `custom` is *derived* here rather than transmitted, and **the writer and
///   the reference decoder do not agree**. The writer's rule is
///   `custom = ~entityType & ENTITY_NORMAL` (`sv_main.cpp:5898`) — bit0 clear.
///   HLTV tests `(type & ENTITY_BEAM) == ENTITY_BEAM` (`World.cpp:2035`) —
///   bit1 set. They agree on 1 and 2 and disagree on 0 and 3. Since the writer
///   is what chose the table that produced the bits, the writer's rule is the
///   one implemented here.
///
///   In practice only 1 and 2 occur: `SV_CreateBaseline` sets `entityType`
///   from `FL_CUSTOMENTITY` (`sv_main.cpp:5848-5851`) and the one thing that
///   could overwrite it, the game DLL's `pfnCreateBaseline`, does not —
///   ReGameDLL-CS touches `entityType` only in `AddToFullPack`
///   (`regamedll/dlls/client.cpp:4619-4622`), which is the per-frame path, not
///   this one. A mod that left the field at `ENTITY_UNINITIALIZED` (`1<<30`,
///   `common/entity_state.h:28`) would put a plain `0` on the wire, since only
///   the low 2 bits are sent — and that is exactly a value the two rules
///   disagree about, so it is rejected as [`EntityError::BadEntityType`]
///   rather than decoded one of the two possible ways.
///
/// # Why this validates rather than trusts
///
/// Everywhere else in this module the stream has already been identified by an
/// opcode the caller dispatched on. This one has not: the only thing that
/// carries baselines is `svc_spawnbaseline`, whose block is bit-packed, so a
/// caller that cannot walk the byte stream to it has to *guess* — and the one
/// in this workspace guesses by scanning for a bare byte 22
/// (`client/src/world.rs`, formerly `Decoder::absorb_baselines`, now located by the
/// stream walker).
///
/// A guess that lands in the middle of a user-message burst used to be
/// **accepted**: an 894-byte round-restart record from `captures/swarm/Bot02.bin`
/// decoded as nine baselines and *sixty-three instanced baselines*. Nothing
/// then went wrong immediately — but [`PacketCtx::instanced`]'s length gates a
/// header bit (trap 3), so from that moment every `svc_packetentities` header
/// was shifted by one bit and every entity frame failed. In a live 4-bot run
/// that was 7263 consecutive `entity_errors` out of 7602 datagrams, i.e. the
/// bot never saw another player again.
///
/// So the invariants `SV_CreateBaseline` guarantees are checked here, and a
/// stream that breaks one is refused. Each is cheap and each is load-bearing:
///
/// * **entity numbers strictly ascend** — they are the `entnum` loop counter
///   (`sv_main.cpp:5891-5896`). The offending record went `768, 3, 528, 0, …`.
/// * **`entityType` is 1 or 2** (`sv_main.cpp:5848-5851`, sent at `:5897`).
///   The offending record's first entry was type 0.
/// * **no more baselines than there are edicts** (`MAX_EDICTS`), which bounds
///   the loop on a stream that never produces the sentinel.
pub fn parse_spawn_baseline(
    r: &mut BitReader,
    registry: &DeltaRegistry,
    maxclients: u8,
) -> Result<Baselines, EntityError> {
    let mut out = Baselines::default();
    let mut previous: Option<u16> = None;

    loop {
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        if r.peek_bits(16) == BASELINE_SENTINEL {
            break;
        }
        if out.by_number.len() >= usize::from(MAX_EDICTS) {
            return Err(EntityError::TooManyBaselines);
        }

        let number = r.read_bits(MAX_EDICT_BITS) as u16;
        let entity_type = r.read_bits(2) as u8;

        if let Some(prev) = previous {
            if number <= prev {
                return Err(EntityError::BaselineOutOfOrder {
                    previous: prev,
                    got: number,
                });
            }
        }
        previous = Some(number);

        if entity_type != ENTITY_NORMAL && entity_type != ENTITY_BEAM {
            return Err(EntityError::BadEntityType(entity_type));
        }
        let custom = entity_type & ENTITY_NORMAL == 0;

        let which = table_for(number, custom, maxclients);
        let table = registry
            .get(which.name())
            .ok_or(EntityError::BadTable(which.name()))?;
        let fields = parse_delta(r, table);
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }

        out.by_number.insert(
            number,
            EntityState {
                number,
                entity_type,
                fields,
            },
        );
    }

    r.skip(16); // the 0xFFFF sentinel itself

    let count = r.read_bits(MAX_BASELINE_BITS) as usize;
    if r.overflowed() {
        return Err(EntityError::Overflow);
    }
    // `MAX_BASELINE_BITS` is 6 and `NUM_BASELINES` is 64
    // (`rehlds/engine/inst_baseline.h:31`), so the field cannot overrun the
    // array; the assert documents the coupling rather than guarding it.
    debug_assert!(count <= MAX_INSTANCED_BASELINES);

    let table = registry
        .get(TABLE_ENTITY)
        .ok_or(EntityError::BadTable(TABLE_ENTITY))?;
    for _ in 0..count {
        let fields = parse_delta(r, table);
        if r.overflowed() {
            return Err(EntityError::Overflow);
        }
        // Mirrors `AddInstancedBaselineEntity` (`World.cpp:2063`), which
        // memcpy's a state that was memset to zero and never given a number or
        // a type: an instanced baseline is addressed by position only.
        out.instanced.push(EntityState {
            number: 0,
            entity_type: 0,
            fields,
        });
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{
        write_delta, FieldDesc, DT_ANGLE, DT_BYTE, DT_FLOAT, DT_INTEGER, DT_SHORT, DT_SIGNED,
    };

    // -- fixtures ----------------------------------------------------------

    fn fd(name: &str, ty: u32, bits: u32, pre: f32) -> FieldDesc {
        FieldDesc {
            name: name.into(),
            field_type: ty,
            bits,
            premultiply: pre,
            postmultiply: 1.0,
        }
    }

    /// A small `entity_state_t`, field order and widths lifted from the real
    /// `testserver/rehlds/cstrike/delta.lst:68-125`.
    fn entity_table() -> DeltaTable {
        vec![
            fd("frame", DT_FLOAT, 8, 1.0),
            fd("origin[0]", DT_SIGNED | DT_FLOAT, 16, 8.0),
            fd("angles[0]", DT_ANGLE, 16, 1.0),
            fd("angles[1]", DT_ANGLE, 16, 1.0),
            fd("origin[1]", DT_SIGNED | DT_FLOAT, 16, 8.0),
            fd("origin[2]", DT_SIGNED | DT_FLOAT, 16, 8.0),
            fd("modelindex", DT_INTEGER, 10, 1.0),
            fd("solid", DT_SHORT, 3, 1.0),
            fd("effects", DT_INTEGER, 8, 1.0),
        ]
    }

    /// `entity_state_player_t` — deliberately a *different* shape, with a field
    /// (`gaitsequence`) that only players have, so a test can prove which
    /// table was picked.
    fn player_table() -> DeltaTable {
        vec![
            fd("origin[0]", DT_SIGNED | DT_FLOAT, 16, 8.0),
            fd("origin[1]", DT_SIGNED | DT_FLOAT, 16, 8.0),
            fd("origin[2]", DT_SIGNED | DT_FLOAT, 16, 8.0),
            fd("gaitsequence", DT_INTEGER, 8, 1.0),
            fd("health", DT_INTEGER, 10, 1.0),
        ]
    }

    /// `custom_entity_state_t` — beams.
    fn custom_table() -> DeltaTable {
        vec![
            fd("startpos[0]", DT_SIGNED | DT_FLOAT, 13, 1.0),
            fd("endpos[0]", DT_SIGNED | DT_FLOAT, 13, 1.0),
            fd("impacttime", DT_BYTE, 8, 1.0),
        ]
    }

    fn registry() -> DeltaRegistry {
        let mut reg = DeltaRegistry::new();
        reg.register(TABLE_ENTITY, entity_table());
        reg.register(TABLE_PLAYER, player_table());
        reg.register(TABLE_CUSTOM, custom_table());
        reg
    }

    fn fields(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn ent(number: u16, pairs: &[(&str, Value)]) -> EntityState {
        EntityState {
            number,
            entity_type: ENTITY_NORMAL,
            fields: fields(pairs),
        }
    }

    /// Bits consumed so far — the only way to prove a conditional bit was or
    /// was not on the wire.
    fn consumed(r: &BitReader) -> usize {
        r.byte_pos() * 8 + r.bit_offset() as usize
    }

    struct Ctx {
        reg: DeltaRegistry,
        baselines: HashMap<u16, EntityState>,
        instanced: Vec<EntityState>,
        maxclients: u8,
    }

    impl Ctx {
        fn new() -> Self {
            Self {
                reg: registry(),
                baselines: HashMap::new(),
                instanced: Vec::new(),
                maxclients: 32,
            }
        }
        fn ctx(&self) -> PacketCtx<'_> {
            PacketCtx {
                registry: &self.reg,
                baselines: &self.baselines,
                instanced: &self.instanced,
                maxclients: self.maxclients,
            }
        }
    }

    /// Write one entity: header, then its delta payload against `table`.
    fn write_entity(
        w: &mut BitWriter,
        h: &DeltaHeader,
        numbase: &mut i32,
        full: bool,
        instanced: usize,
        table: &DeltaTable,
        payload: &HashMap<String, Value>,
    ) {
        write_delta_header(w, h, numbase, full, instanced);
        if !h.remove {
            write_delta(w, table, payload);
        }
    }

    fn terminate(w: &mut BitWriter) {
        w.write_bits(0, 16);
    }

    fn hdr(number: u16) -> DeltaHeader {
        DeltaHeader {
            number,
            ..Default::default()
        }
    }

    // -- header shapes, against hand-computed bit patterns -----------------
    //
    // These do not go through the encoder-then-decoder loop: the expected
    // bytes were worked out by hand from `SV_WriteDeltaHeader`, so they pin
    // the absolute bit layout rather than merely proving the two halves of
    // this module agree with each other.

    #[test]
    fn full_plus_one_shortcut_is_exactly_three_bits() {
        // num 1, numbase 0, full => delta == 1, so the number is not sent at
        // all. Bits, in order: shortcut 1, custom 0, offset-present 0.
        // LSB-first that is 0b0000_0001.
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut w, &hdr(1), &mut nb, true, 0);
        let bytes = w.into_bytes();
        assert_eq!(bytes, vec![0x01]);
        assert_eq!(nb, 1);

        let mut r = BitReader::new(&bytes);
        let mut nb = 0i32;
        let h = parse_delta_header(&mut r, &mut nb, true, 0);
        assert_eq!(h, hdr(1));
        assert_eq!(nb, 1);
        assert_eq!(consumed(&r), 3, "shortcut + custom + offset-present");
    }

    #[test]
    fn delta_six_bit_form_matches_the_writer_bit_for_bit() {
        // num 5, numbase 0, delta packet: remove 0, absolute-flag 0,
        // delta 5 in 6 bits (101000 LSB-first), custom 0.
        // byte0 = 0,0,1,0,1,0,0,0 = 0x14; byte1 = custom bit only = 0x00.
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut w, &hdr(5), &mut nb, false, 0);
        assert_eq!(w.as_bytes(), &[0x14, 0x00]);

        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let mut nb = 0i32;
        let h = parse_delta_header(&mut r, &mut nb, false, 0);
        assert_eq!(h.number, 5);
        assert!(!h.remove && !h.custom);
        assert_eq!(consumed(&r), 9);
    }

    #[test]
    fn absolute_form_matches_the_writer_bit_for_bit() {
        // num 100 > 63, so: remove 0, absolute-flag 1, 100 in 11 bits, custom 0.
        // 100 = 0b000_0110_0100 -> LSB-first 0,0,1,0,0,1,1,0,0,0,0
        // byte0 = 0,1,0,0,1,0,0,1 = 0x92; byte1 = 1,0,0,0,0,0(custom) = 0x01.
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut w, &hdr(100), &mut nb, false, 0);
        assert_eq!(w.as_bytes(), &[0x92, 0x01]);

        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let mut nb = 0i32;
        let h = parse_delta_header(&mut r, &mut nb, false, 0);
        assert_eq!(h.number, 100);
        assert_eq!(nb, 100);
        assert_eq!(consumed(&r), 14);
    }

    #[test]
    fn a_removal_stops_after_the_number() {
        // numbase 2, num 3 -> delta 1, still the 6-bit form (there is no
        // shortcut in a delta packet). remove 1, absolute-flag 0, 1 in 6 bits,
        // and then nothing: no custom, no baseline bits.
        let mut w = BitWriter::new();
        let mut nb = 2i32;
        let h = DeltaHeader {
            number: 3,
            remove: true,
            ..Default::default()
        };
        write_delta_header(&mut w, &h, &mut nb, false, 0);
        assert_eq!(w.as_bytes(), &[0x05]);

        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let mut nb = 2i32;
        let got = parse_delta_header(&mut r, &mut nb, false, 0);
        assert!(got.remove);
        assert_eq!(got.number, 3);
        assert_eq!(consumed(&r), 8, "remove suppresses the whole trailer");
    }

    #[test]
    fn delta_of_zero_or_negative_forces_the_absolute_form() {
        // delta <= 0 cannot be encoded in the unsigned 6-bit form, so the
        // writer falls back to 11 absolute bits (`sv_main.cpp:4434`).
        for (numbase, number) in [(10i32, 10u16), (10, 4), (0, 0)] {
            let mut w = BitWriter::new();
            let mut nb = numbase;
            write_delta_header(&mut w, &hdr(number), &mut nb, false, 0);
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            let mut nb2 = numbase;
            let h = parse_delta_header(&mut r, &mut nb2, false, 0);
            assert_eq!(h.number, number);
            assert_eq!(nb2, i32::from(number));
            // remove + absolute-flag + 11 + custom == 14 bits
            assert_eq!(consumed(&r), 14, "must be the absolute form");
        }
    }

    #[test]
    fn delta_of_exactly_64_forces_the_absolute_form() {
        // The boundary: 63 fits, 64 does not (`delta > 63`).
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut w, &hdr(63), &mut nb, false, 0);
        let b63 = w.into_bytes();
        let mut r = BitReader::new(&b63);
        let mut nb = 0i32;
        assert_eq!(parse_delta_header(&mut r, &mut nb, false, 0).number, 63);
        assert_eq!(consumed(&r), 9, "6-bit form");

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut w, &hdr(64), &mut nb, false, 0);
        let b64 = w.into_bytes();
        let mut r = BitReader::new(&b64);
        let mut nb = 0i32;
        assert_eq!(parse_delta_header(&mut r, &mut nb, false, 0).number, 64);
        assert_eq!(consumed(&r), 14, "absolute form");
    }

    // -- the full header matrix -------------------------------------------

    #[test]
    fn every_header_shape_round_trips() {
        // Numbers chosen to exercise all four shapes given numbase: +1 (full
        // shortcut), small positive delta (6-bit), > 63 (absolute) and <= 0
        // (absolute).
        let cases: [(i32, u16); 4] = [(0, 1), (10, 40), (10, 900), (900, 5)];
        for full in [false, true] {
            for instanced in [0usize, 7] {
                for custom in [false, true] {
                    for newbl in [None, Some(3u8)] {
                        for offset in [0u8, 5] {
                            for (numbase, number) in cases {
                                // A newbl index can only be sent when the map
                                // has instanced baselines at all.
                                let newbl = if instanced == 0 { None } else { newbl };
                                let want = DeltaHeader {
                                    number,
                                    remove: false,
                                    custom,
                                    new_baseline: newbl,
                                    // The offset field only exists in a full
                                    // packet, and only when newbl was not sent.
                                    baseline_offset: if full && newbl.is_none() {
                                        offset
                                    } else {
                                        0
                                    },
                                };

                                let mut w = BitWriter::new();
                                let mut nb = numbase;
                                write_delta_header(&mut w, &want, &mut nb, full, instanced);
                                assert_eq!(nb, i32::from(number));
                                let bytes = w.into_bytes();

                                let mut r = BitReader::new(&bytes);
                                let mut nb = numbase;
                                let got = parse_delta_header(&mut r, &mut nb, full, instanced);
                                assert_eq!(got, want, "full={full} instanced={instanced}");
                                assert_eq!(nb, i32::from(number));
                                assert!(!r.overflowed());
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn removal_round_trips_and_never_carries_a_trailer() {
        for instanced in [0usize, 7] {
            for (numbase, number) in [(0i32, 1u16), (10, 40), (10, 900), (900, 5)] {
                let want = DeltaHeader {
                    number,
                    remove: true,
                    ..Default::default()
                };
                let mut w = BitWriter::new();
                let mut nb = numbase;
                write_delta_header(&mut w, &want, &mut nb, false, instanced);
                let bytes = w.into_bytes();

                let mut r = BitReader::new(&bytes);
                let mut nb = numbase;
                let got = parse_delta_header(&mut r, &mut nb, false, instanced);
                assert_eq!(got, want);
                // 1 removal bit + the number, and nothing else: the trailer's
                // width does not depend on `instanced` when removing.
                let n = consumed(&r);
                assert!(n == 8 || n == 13, "removal header was {n} bits");
            }
        }
    }

    // -- trap 3: the two conditional bits ---------------------------------

    #[test]
    fn the_instanced_bit_exists_only_when_the_map_has_instanced_baselines() {
        let h = hdr(1);
        let mut a = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut a, &h, &mut nb, false, 0);
        let a = a.into_bytes();

        let mut b = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut b, &h, &mut nb, false, 4);
        let b = b.into_bytes();

        let mut ra = BitReader::new(&a);
        let mut nb = 0i32;
        parse_delta_header(&mut ra, &mut nb, false, 0);
        let mut rb = BitReader::new(&b);
        let mut nb = 0i32;
        parse_delta_header(&mut rb, &mut nb, false, 4);

        assert_eq!(
            consumed(&rb),
            consumed(&ra) + 1,
            "exactly one extra bit when the map has instanced baselines"
        );
    }

    #[test]
    fn getting_the_instanced_gate_wrong_silently_shifts_everything() {
        // This is the failure mode trap 3 warns about: the same bits decode to
        // a different, entirely plausible header. Full packet, entity 1,
        // baseline_offset 5, written by a server with NO instanced baselines.
        let want = DeltaHeader {
            number: 1,
            baseline_offset: 5,
            ..Default::default()
        };
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut w, &want, &mut nb, true, 0);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let mut nb = 0i32;
        assert_eq!(parse_delta_header(&mut r, &mut nb, true, 0), want);

        // Same bits, decoder wrongly believing the map has instanced baselines:
        // the offset-present bit is eaten as the newbl bit and the offset
        // itself as the newbl index. No error, just a wrong answer.
        let mut r = BitReader::new(&bytes);
        let mut nb = 0i32;
        let wrong = parse_delta_header(&mut r, &mut nb, true, 1);
        assert_eq!(wrong.new_baseline, Some(5));
        assert_eq!(wrong.baseline_offset, 0);
        assert_ne!(wrong, want);
    }

    #[test]
    fn the_offset_bit_exists_only_in_a_full_packet() {
        let h = hdr(9);
        let mut d = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut d, &h, &mut nb, false, 0);
        let d = d.into_bytes();

        let mut f = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut f, &h, &mut nb, true, 0);
        let f = f.into_bytes();

        let mut rd = BitReader::new(&d);
        let mut nb = 0i32;
        parse_delta_header(&mut rd, &mut nb, false, 0);
        let mut rf = BitReader::new(&f);
        let mut nb = 0i32;
        parse_delta_header(&mut rf, &mut nb, true, 0);

        // Both spend 1 + (1 + 6) + 1 bits on flag/number/custom; the full one
        // adds the offset-present bit.
        assert_eq!(consumed(&rf), consumed(&rd) + 1);
    }

    #[test]
    fn the_offset_bit_is_suppressed_when_an_instanced_baseline_was_named() {
        // `if (full && !newbl)` — naming an instanced baseline removes the
        // offset field entirely (`sv_main.cpp:4462`).
        let with_newbl = DeltaHeader {
            number: 1,
            new_baseline: Some(2),
            ..Default::default()
        };
        let without = DeltaHeader {
            number: 1,
            ..Default::default()
        };

        let mut a = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut a, &with_newbl, &mut nb, true, 8);
        let a = a.into_bytes();
        let mut ra = BitReader::new(&a);
        let mut nb = 0i32;
        let ha = parse_delta_header(&mut ra, &mut nb, true, 8);
        assert_eq!(ha, with_newbl);
        // shortcut + custom + newbl-present + 6 index = 9
        assert_eq!(consumed(&ra), 9);

        let mut b = BitWriter::new();
        let mut nb = 0i32;
        write_delta_header(&mut b, &without, &mut nb, true, 8);
        let b = b.into_bytes();
        let mut rb = BitReader::new(&b);
        let mut nb = 0i32;
        let hb = parse_delta_header(&mut rb, &mut nb, true, 8);
        assert_eq!(hb, without);
        // shortcut + custom + newbl-present + offset-present = 4
        assert_eq!(consumed(&rb), 4);
    }

    // -- table selection ---------------------------------------------------

    #[test]
    fn table_selection_follows_sv_isplayerindex() {
        assert_eq!(table_for(0, false, 32), EntityTable::Entity, "0 is the world");
        assert_eq!(table_for(1, false, 32), EntityTable::Player);
        assert_eq!(table_for(32, false, 32), EntityTable::Player);
        assert_eq!(table_for(33, false, 32), EntityTable::Entity);
        // custom wins over everything, even for a player slot.
        assert_eq!(table_for(1, true, 32), EntityTable::Custom);
        assert_eq!(table_for(500, true, 32), EntityTable::Custom);
        // maxclients bounds the player range.
        assert_eq!(table_for(5, false, 2), EntityTable::Entity);
        assert!(!is_player_index(0, 32));
        assert!(!is_player_index(1, 0));
    }

    #[test]
    fn a_player_entity_decodes_with_the_player_table() {
        // `gaitsequence` exists only in the player table; if the entity table
        // had been used, the same bits would land in different field names.
        let mut c = Ctx::new();
        c.maxclients = 4;
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(2),
            &mut nb,
            true,
            0,
            &player_table(),
            &fields(&[
                ("origin[0]", Value::Float(64.0)),
                ("gaitsequence", Value::Int(7)),
                ("health", Value::Int(85)),
            ]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full(&mut r, &c.ctx()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].i64("gaitsequence"), 7);
        assert_eq!(out[0].i64("health"), 85);
        assert_eq!(out[0].origin(), [64.0, 0.0, 0.0]);
    }

    #[test]
    fn a_custom_entity_decodes_with_the_beam_table() {
        let c = Ctx::new();
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        let h = DeltaHeader {
            number: 300,
            custom: true,
            ..Default::default()
        };
        write_entity(
            &mut w,
            &h,
            &mut nb,
            true,
            0,
            &custom_table(),
            &fields(&[("startpos[0]", Value::Float(12.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full(&mut r, &c.ctx()).unwrap();
        assert_eq!(out[0].entity_type, ENTITY_BEAM);
        assert!(out[0].is_beam());
        assert_eq!(out[0].f32("startpos[0]"), 12.0);
    }

    // -- full packets ------------------------------------------------------

    #[test]
    fn a_full_packet_deltas_each_entity_from_its_own_baseline() {
        let mut c = Ctx::new();
        // maxclients 4 keeps entity 7 out of the player range, so it is
        // encoded with `entity_state_t` -- get this wrong and the field mask
        // names a different field of a different width.
        c.maxclients = 4;
        c.baselines.insert(
            7,
            ent(
                7,
                &[
                    ("modelindex", Value::Int(31)),
                    ("origin[2]", Value::Float(8.0)),
                ],
            ),
        );

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(7),
            &mut nb,
            true,
            0,
            &entity_table(),
            &fields(&[("origin[0]", Value::Float(128.5))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full_checked(&mut r, &c.ctx(), 1).unwrap();
        assert_eq!(out.len(), 1);
        // origin[0] came from the wire, modelindex/origin[2] from the baseline.
        assert_eq!(out[0].origin(), [128.5, 0.0, 8.0]);
        assert_eq!(out[0].i64("modelindex"), 31);
        assert!(!r.overflowed());
    }

    #[test]
    fn baseline_offset_indexes_the_new_frame_by_position() {
        // Trap 2. Entities 7 and 9, and entity 9 carries offset 1, meaning
        // "my baseline is the entity one slot earlier in THIS frame" -- that
        // is entity 7. Two decoys make a positional/number mix-up visible:
        // baselines[9] (what a naive `baselines[number]` would use) and
        // baselines[8] (what `number - offset` would use).
        let mut c = Ctx::new();
        c.maxclients = 4; // 7/8/9 are world entities, not player slots
        c.baselines
            .insert(9, ent(9, &[("modelindex", Value::Int(111))]));
        c.baselines
            .insert(8, ent(8, &[("modelindex", Value::Int(222))]));
        c.baselines
            .insert(7, ent(7, &[("modelindex", Value::Int(333))]));

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        // Entity 7: takes modelindex 333 from its own baseline.
        write_entity(
            &mut w,
            &hdr(7),
            &mut nb,
            true,
            0,
            &entity_table(),
            &fields(&[("origin[0]", Value::Float(16.0))]),
        );
        // Entity 9 with offset 1 -> baseline is out[1 - 1] = out[0].
        let h9 = DeltaHeader {
            number: 9,
            baseline_offset: 1,
            ..Default::default()
        };
        write_entity(
            &mut w,
            &h9,
            &mut nb,
            true,
            0,
            &entity_table(),
            &fields(&[("origin[1]", Value::Float(32.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full_checked(&mut r, &c.ctx(), 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].number, 9);
        assert_eq!(
            out[1].i64("modelindex"),
            333,
            "offset 1 must resolve to the previous entity in the new frame"
        );
        assert_ne!(out[1].i64("modelindex"), 111, "not baselines[number]");
        assert_ne!(out[1].i64("modelindex"), 222, "not baselines[number - offset]");
        // and it inherited entity 7's origin[0] while overriding origin[1].
        assert_eq!(out[1].origin(), [16.0, 32.0, 0.0]);
    }

    #[test]
    fn a_baseline_offset_that_underflows_the_frame_is_an_error() {
        let mut c = Ctx::new();
        c.maxclients = 4;
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        // First entity in the frame claiming offset 2: `entity[0 - 2]`.
        let h = DeltaHeader {
            number: 5,
            baseline_offset: 2,
            ..Default::default()
        };
        write_entity(&mut w, &h, &mut nb, true, 0, &entity_table(), &HashMap::new());
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_packet_entities_full(&mut r, &c.ctx()),
            Err(EntityError::BadBaselineOffset {
                index: 0,
                offset: 2
            })
        );
    }

    #[test]
    fn an_instanced_baseline_is_resolved_by_index() {
        let mut c = Ctx::new();
        c.instanced = vec![
            ent(0, &[("modelindex", Value::Int(11))]),
            ent(0, &[("modelindex", Value::Int(22))]),
            ent(0, &[("modelindex", Value::Int(33))]),
        ];
        c.baselines
            .insert(40, ent(40, &[("modelindex", Value::Int(99))]));

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        let h = DeltaHeader {
            number: 40,
            new_baseline: Some(2),
            ..Default::default()
        };
        write_entity(
            &mut w,
            &h,
            &mut nb,
            true,
            c.instanced.len(),
            &entity_table(),
            &fields(&[("origin[0]", Value::Float(4.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full(&mut r, &c.ctx()).unwrap();
        assert_eq!(out[0].i64("modelindex"), 33);

        // ... and an index past the end is an error rather than a wild read.
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        let bad = DeltaHeader {
            number: 40,
            new_baseline: Some(9),
            ..Default::default()
        };
        write_entity(
            &mut w,
            &bad,
            &mut nb,
            true,
            c.instanced.len(),
            &entity_table(),
            &HashMap::new(),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_packet_entities_full(&mut r, &c.ctx()),
            Err(EntityError::BadInstancedBaseline { index: 9, count: 3 })
        );
    }

    #[test]
    fn the_terminator_is_consumed_exactly_and_leaves_the_reader_clean() {
        let c = Ctx::new();
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(1),
            &mut nb,
            true,
            0,
            &player_table(),
            &fields(&[("health", Value::Int(100))]),
        );
        // Count the bits of the body before the terminator by re-reading it.
        let body_bits = {
            let bytes = w.as_bytes().to_vec();
            let mut r = BitReader::new(&bytes);
            let mut nb = 0i32;
            let h = parse_delta_header(&mut r, &mut nb, true, 0);
            let _ = parse_delta(&mut r, &player_table());
            assert_eq!(h.number, 1);
            consumed(&r)
        };
        terminate(&mut w);
        // Anything after the block: the caller's next message. It must not be
        // touched, and it must not be mistaken for the terminator.
        w.write_bits(0xFFFF, 16);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full(&mut r, &c.ctx()).unwrap();
        assert_eq!(out.len(), 1);
        assert!(!r.overflowed());
        assert_eq!(
            consumed(&r),
            body_bits + 16,
            "the 16-bit end tag is consumed, and nothing more"
        );
        assert_eq!(r.read_bits(16), 0xFFFF, "the next message is untouched");
    }

    #[test]
    fn an_empty_full_packet_is_just_the_terminator() {
        let c = Ctx::new();
        let mut w = BitWriter::new();
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_full(&mut r, &c.ctx()).unwrap();
        assert!(out.is_empty());
        assert_eq!(consumed(&r), 16);
        assert!(!r.overflowed());
    }

    // -- delta packets: trap 1 --------------------------------------------

    #[test]
    fn unmentioned_entities_are_copied_forward_from_the_previous_frame() {
        // The core of trap 1. `from` holds 1, 2, 3; the packet mentions only
        // entity 2. Entities 1 and 3 were not transmitted at all -- 1 via the
        // catch-up loop before the header, 3 via the tail loop after the
        // terminator -- and both must come out byte-identical.
        let mut c = Ctx::new();
        c.maxclients = 4;
        let from = vec![
            ent(
                1,
                &[("origin[0]", Value::Float(10.0)), ("health", Value::Int(100))],
            ),
            ent(
                2,
                &[("origin[0]", Value::Float(20.0)), ("health", Value::Int(90))],
            ),
            ent(
                3,
                &[("origin[0]", Value::Float(30.0)), ("health", Value::Int(80))],
            ),
        ];

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(2),
            &mut nb,
            false,
            0,
            &player_table(),
            &fields(&[("origin[0]", Value::Float(25.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_delta_checked(&mut r, &from, &c.ctx(), 3).unwrap();

        assert_eq!(out.len(), 3, "all three entities survive");
        assert_eq!(out[0], from[0], "entity 1 is carried over untouched");
        assert_eq!(out[2], from[2], "entity 3 is carried over untouched");
        assert_eq!(out[1].number, 2);
        assert_eq!(out[1].f32("origin[0]"), 25.0, "the moved one moved");
        assert_eq!(
            out[1].i64("health"),
            90,
            "and kept every field the delta did not mention"
        );
        assert!(!r.overflowed());
    }

    #[test]
    fn a_delta_packet_that_mentions_nobody_reproduces_the_frame_exactly() {
        // The steady state of a quiet server: nothing changed, so the block is
        // 16 zero bits and the whole frame is copy-forward.
        let c = Ctx::new();
        let from = vec![
            ent(1, &[("origin[0]", Value::Float(1.0))]),
            ent(9, &[("origin[0]", Value::Float(9.0))]),
        ];
        let mut w = BitWriter::new();
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_delta(&mut r, &from, &c.ctx()).unwrap();
        assert_eq!(out, from);
    }

    #[test]
    fn a_removed_entity_disappears_and_the_rest_still_line_up() {
        let mut c = Ctx::new();
        c.maxclients = 4;
        let from = vec![
            ent(1, &[("origin[0]", Value::Float(10.0))]),
            ent(2, &[("origin[0]", Value::Float(20.0))]),
            ent(3, &[("origin[0]", Value::Float(30.0))]),
        ];

        // This is exactly what the server's merge walk emits when only entity
        // 2 leaves: 1 and 3 are unchanged so they cost nothing, and 2 gets a
        // header-only removal (`sv_main.cpp:4712`).
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        let rm = DeltaHeader {
            number: 2,
            remove: true,
            ..Default::default()
        };
        write_entity(&mut w, &rm, &mut nb, false, 0, &player_table(), &HashMap::new());
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_delta_checked(&mut r, &from, &c.ctx(), 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], from[0]);
        assert_eq!(out[1], from[2], "entity 3 is still entity 3");
        assert!(out.iter().all(|e| e.number != 2));
    }

    #[test]
    fn a_new_entity_in_a_delta_packet_comes_from_its_baseline() {
        let mut c = Ctx::new();
        c.baselines
            .insert(50, ent(50, &[("modelindex", Value::Int(77))]));
        let from = vec![ent(1, &[("origin[0]", Value::Float(1.0))])];

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(50),
            &mut nb,
            false,
            0,
            &entity_table(),
            &fields(&[("origin[2]", Value::Float(-64.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let out = parse_packet_entities_delta(&mut r, &from, &c.ctx()).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], from[0]);
        assert_eq!(out[1].number, 50);
        assert_eq!(out[1].i64("modelindex"), 77);
        assert_eq!(out[1].origin()[2], -64.0);
    }

    #[test]
    fn a_full_reencode_of_a_moving_player_survives_several_frames() {
        // Three frames of the thing this module exists for: a player walking
        // while a prop stands still. Frame 2 mentions only the player, frame 3
        // mentions nobody, and the prop has to still be there at the end.
        let mut c = Ctx::new();
        c.maxclients = 8;
        c.baselines
            .insert(1, ent(1, &[("health", Value::Int(100))]));
        c.baselines
            .insert(60, ent(60, &[("modelindex", Value::Int(12))]));

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(1),
            &mut nb,
            true,
            0,
            &player_table(),
            &fields(&[("origin[0]", Value::Float(0.0))]),
        );
        write_entity(
            &mut w,
            &hdr(60),
            &mut nb,
            true,
            0,
            &entity_table(),
            &fields(&[("origin[0]", Value::Float(512.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let f1 = parse_packet_entities_full_checked(&mut r, &c.ctx(), 2).unwrap();
        assert_eq!(f1[0].i64("health"), 100);

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(1),
            &mut nb,
            false,
            0,
            &player_table(),
            &fields(&[("origin[0]", Value::Float(48.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let f2 = parse_packet_entities_delta_checked(&mut r, &f1, &c.ctx(), 2).unwrap();
        assert_eq!(f2[0].origin()[0], 48.0);
        assert_eq!(f2[0].i64("health"), 100, "health persisted across a delta");
        assert_eq!(f2[1], f1[1], "the prop did not move");

        let mut w = BitWriter::new();
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let f3 = parse_packet_entities_delta(&mut r, &f2, &c.ctx()).unwrap();
        assert_eq!(f3, f2);
    }

    // -- error paths -------------------------------------------------------

    #[test]
    fn a_truncated_stream_is_an_error_not_a_plausible_frame() {
        let mut c = Ctx::new();
        c.maxclients = 4;
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(7),
            &mut nb,
            true,
            0,
            &entity_table(),
            &fields(&[
                ("origin[0]", Value::Float(128.0)),
                ("origin[1]", Value::Float(64.0)),
                ("modelindex", Value::Int(30)),
            ]),
        );
        terminate(&mut w);
        let full = w.into_bytes();

        // Every proper prefix must fail rather than return a frame.
        for cut in 1..full.len() {
            let mut r = BitReader::new(&full[..cut]);
            let got = parse_packet_entities_full(&mut r, &c.ctx());
            assert!(
                got.is_err(),
                "a {cut}-byte prefix of a {}-byte block decoded as {:?}",
                full.len(),
                got
            );
        }
        // ... and the whole thing still works.
        let mut r = BitReader::new(&full);
        assert!(parse_packet_entities_full(&mut r, &c.ctx()).is_ok());
    }

    #[test]
    fn a_truncated_delta_stream_is_an_error_too() {
        let c = Ctx::new();
        let from = vec![ent(1, &[("origin[0]", Value::Float(1.0))])];
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(80),
            &mut nb,
            false,
            0,
            &entity_table(),
            &fields(&[("origin[0]", Value::Float(100.0)), ("effects", Value::Int(3))]),
        );
        terminate(&mut w);
        let full = w.into_bytes();
        for cut in 1..full.len() {
            let mut r = BitReader::new(&full[..cut]);
            assert!(parse_packet_entities_delta(&mut r, &from, &c.ctx()).is_err());
        }
    }

    #[test]
    fn an_empty_block_is_an_error_because_the_end_tag_is_missing() {
        let c = Ctx::new();
        let data: [u8; 0] = [];
        let mut r = BitReader::new(&data);
        assert_eq!(
            parse_packet_entities_full(&mut r, &c.ctx()),
            Err(EntityError::Overflow)
        );
    }

    #[test]
    fn a_missing_delta_table_is_reported() {
        let mut reg = DeltaRegistry::new();
        reg.register(TABLE_ENTITY, entity_table());
        let baselines = HashMap::new();
        let instanced: Vec<EntityState> = Vec::new();
        let ctx = PacketCtx {
            registry: &reg,
            baselines: &baselines,
            instanced: &instanced,
            maxclients: 32,
        };

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(1),
            &mut nb,
            true,
            0,
            &player_table(),
            &fields(&[("health", Value::Int(1))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_packet_entities_full(&mut r, &ctx),
            Err(EntityError::BadTable(TABLE_PLAYER))
        );
    }

    #[test]
    fn the_header_count_is_checked_when_the_caller_supplies_it() {
        let c = Ctx::new();
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(&mut w, &hdr(3), &mut nb, true, 0, &entity_table(), &HashMap::new());
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_packet_entities_full_checked(&mut r, &c.ctx(), 4),
            Err(EntityError::CountMismatch {
                expected: 4,
                got: 1
            })
        );
    }

    #[test]
    fn an_entity_number_past_max_edicts_is_rejected() {
        // Only reachable through the additive 6-bit form: 2040 + 40 = 2080.
        let c = Ctx::new();
        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(&mut w, &hdr(2040), &mut nb, false, 0, &entity_table(), &HashMap::new());
        write_entity(&mut w, &hdr(2080), &mut nb, false, 0, &entity_table(), &HashMap::new());
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_packet_entities_delta(&mut r, &[], &c.ctx()),
            Err(EntityError::BadEntityNumber(2080))
        );
    }

    // -- svc_spawnbaseline -------------------------------------------------

    #[test]
    fn spawn_baseline_round_trips_including_the_instanced_tail() {
        let reg = registry();
        let maxclients = 4u8;

        let mut w = BitWriter::new();
        // world (entity table), a player (player table), a prop, and a beam.
        fn write_one(
            w: &mut BitWriter,
            num: u16,
            ty: u8,
            table: &DeltaTable,
            f: &HashMap<String, Value>,
        ) {
            w.write_bits(u32::from(num), MAX_EDICT_BITS);
            w.write_bits(u32::from(ty), 2);
            write_delta(w, table, f);
        }
        write_one(
            &mut w,
            0,
            ENTITY_NORMAL,
            &entity_table(),
            &fields(&[("modelindex", Value::Int(1))]),
        );
        write_one(
            &mut w,
            2,
            ENTITY_NORMAL,
            &player_table(),
            &fields(&[("health", Value::Int(100)), ("origin[1]", Value::Float(8.0))]),
        );
        write_one(
            &mut w,
            77,
            ENTITY_NORMAL,
            &entity_table(),
            &fields(&[("modelindex", Value::Int(42)), ("solid", Value::Int(4))]),
        );
        write_one(
            &mut w,
            300,
            ENTITY_BEAM,
            &custom_table(),
            &fields(&[("endpos[0]", Value::Float(-3.0))]),
        );
        w.write_bits(BASELINE_SENTINEL, 16);
        w.write_bits(2, MAX_BASELINE_BITS);
        write_delta(&mut w, &entity_table(), &fields(&[("modelindex", Value::Int(5))]));
        write_delta(&mut w, &entity_table(), &fields(&[("modelindex", Value::Int(6))]));
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let b = parse_spawn_baseline(&mut r, &reg, maxclients).unwrap();
        assert!(!r.overflowed());

        assert_eq!(b.by_number.len(), 4);
        assert_eq!(b.by_number[&0].i64("modelindex"), 1);
        assert_eq!(b.by_number[&2].i64("health"), 100, "player table was used");
        assert_eq!(b.by_number[&2].origin(), [0.0, 8.0, 0.0]);
        assert_eq!(b.by_number[&77].i64("solid"), 4);
        assert_eq!(b.by_number[&300].entity_type, ENTITY_BEAM);
        assert_eq!(b.by_number[&300].f32("endpos[0]"), -3.0);

        // The instanced tail always uses entity_state_t, never the player
        // table, whatever entity it later stands in for.
        assert_eq!(b.instanced.len(), 2);
        assert_eq!(b.instanced[0].i64("modelindex"), 5);
        assert_eq!(b.instanced[1].i64("modelindex"), 6);
    }

    #[test]
    fn spawn_baseline_with_no_instanced_baselines_still_reads_the_count() {
        let reg = registry();
        let mut w = BitWriter::new();
        w.write_bits(0, MAX_EDICT_BITS);
        w.write_bits(u32::from(ENTITY_NORMAL), 2);
        write_delta(&mut w, &entity_table(), &fields(&[("modelindex", Value::Int(1))]));
        w.write_bits(BASELINE_SENTINEL, 16);
        w.write_bits(0, MAX_BASELINE_BITS); // ReGameDLL-CS: always zero
        w.write_bits(0xABCD, 16); // whatever follows in the signon
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let b = parse_spawn_baseline(&mut r, &reg, 32).unwrap();
        assert_eq!(b.by_number.len(), 1);
        assert!(b.instanced.is_empty());
        assert_eq!(r.read_bits(16), 0xABCD, "the 6-bit count was consumed once");
    }

    #[test]
    fn spawn_baseline_derives_custom_the_way_the_writer_does() {
        // `custom = ~entityType & ENTITY_NORMAL` (`sv_main.cpp:5898`): bit0
        // clear means the custom table. Type 2 (ENTITY_BEAM) is the only such
        // value the server actually emits, and this is the round trip for it.
        let reg = registry();
        let mut w = BitWriter::new();
        w.write_bits(9, MAX_EDICT_BITS);
        w.write_bits(u32::from(ENTITY_BEAM), 2);
        write_delta(
            &mut w,
            &custom_table(),
            &fields(&[("impacttime", Value::Int(200))]),
        );
        w.write_bits(BASELINE_SENTINEL, 16);
        w.write_bits(0, MAX_BASELINE_BITS);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let b = parse_spawn_baseline(&mut r, &reg, 32).unwrap();
        assert_eq!(b.by_number[&9].i64("impacttime"), 200);
        assert_eq!(b.by_number[&9].entity_type, ENTITY_BEAM);
    }

    #[test]
    fn a_truncated_spawn_baseline_is_an_error() {
        let reg = registry();
        let mut w = BitWriter::new();
        w.write_bits(5, MAX_EDICT_BITS);
        w.write_bits(u32::from(ENTITY_NORMAL), 2);
        write_delta(
            &mut w,
            &entity_table(),
            &fields(&[("modelindex", Value::Int(9)), ("effects", Value::Int(2))]),
        );
        w.write_bits(BASELINE_SENTINEL, 16);
        w.write_bits(1, MAX_BASELINE_BITS);
        write_delta(&mut w, &entity_table(), &fields(&[("modelindex", Value::Int(5))]));
        let full = w.into_bytes();

        for cut in 0..full.len() {
            let mut r = BitReader::new(&full[..cut]);
            assert!(
                parse_spawn_baseline(&mut r, &reg, 4).is_err(),
                "prefix of {cut} bytes decoded"
            );
        }
    }

    // -- svc_spawnbaseline: the guards that stop a mis-identified stream ----
    //
    // These exist because `svc_spawnbaseline` is the one block in this module
    // whose caller cannot always prove it is looking at the right message. See
    // the `parse_spawn_baseline` docs: a running-phase user-message burst that
    // was mistaken for one decoded as nine baselines and 63 *instanced*
    // baselines, and the instanced count gates a header bit -- so every entity
    // frame afterwards was shifted by one bit and refused.

    /// One baseline entry, written the way `SV_CreateBaseline` does.
    fn write_baseline(w: &mut BitWriter, num: u16, ty: u8, table: &DeltaTable) {
        w.write_bits(u32::from(num), MAX_EDICT_BITS);
        w.write_bits(u32::from(ty), 2);
        write_delta(w, table, &fields(&[("modelindex", Value::Int(1))]));
    }

    #[test]
    fn baseline_entity_numbers_must_strictly_ascend() {
        // `SV_CreateBaseline` writes `entnum` straight out of its loop counter
        // (`sv_main.cpp:5891-5896`), so a real block only ever counts up.
        // Numbers stay above `maxclients` so both entries use `entity_state_t`
        // and the writer here matches the table the parser will pick.
        let reg = registry();
        for (a, b) in [(50u16, 30u16), (50, 50)] {
            let mut w = BitWriter::new();
            write_baseline(&mut w, a, ENTITY_NORMAL, &entity_table());
            write_baseline(&mut w, b, ENTITY_NORMAL, &entity_table());
            w.write_bits(BASELINE_SENTINEL, 16);
            w.write_bits(0, MAX_BASELINE_BITS);
            let bytes = w.into_bytes();

            let mut r = BitReader::new(&bytes);
            assert_eq!(
                parse_spawn_baseline(&mut r, &reg, 4).unwrap_err(),
                EntityError::BaselineOutOfOrder {
                    previous: a,
                    got: b
                },
                "{a} then {b} must be refused"
            );
        }

        // ...and the ascending case is still accepted.
        let mut w = BitWriter::new();
        write_baseline(&mut w, 30, ENTITY_NORMAL, &entity_table());
        write_baseline(&mut w, 50, ENTITY_NORMAL, &entity_table());
        w.write_bits(BASELINE_SENTINEL, 16);
        w.write_bits(0, MAX_BASELINE_BITS);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_spawn_baseline(&mut r, &reg, 4).unwrap().by_number.len(),
            2
        );
    }

    #[test]
    fn a_baseline_entity_type_outside_normal_and_beam_is_refused() {
        // The writer assigns exactly one of the two (`sv_main.cpp:5848-5851`)
        // and sends the low two bits (`:5897`). 0 and 3 are also precisely the
        // values the writer's `custom` rule and HLTV's disagree about
        // (`sv_main.cpp:5898` vs `World.cpp:2035`), so guessing one is worse
        // than refusing.
        let reg = registry();
        for ty in [0u8, 3] {
            let mut w = BitWriter::new();
            write_baseline(&mut w, 1, ty, &entity_table());
            w.write_bits(BASELINE_SENTINEL, 16);
            w.write_bits(0, MAX_BASELINE_BITS);
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            assert_eq!(
                parse_spawn_baseline(&mut r, &reg, 4).unwrap_err(),
                EntityError::BadEntityType(ty)
            );
        }
    }

    #[test]
    fn the_baseline_guards_fire_before_the_delta_payload_is_read() {
        // Order matters: both checks are made on the 13 header bits, before
        // `parse_delta` consumes anything. A stream that is not a baseline
        // block must not be walked field by field on the strength of a byte
        // that happened to be 22.
        let reg = registry();
        let mut w = BitWriter::new();
        w.write_bits(9, MAX_EDICT_BITS);
        w.write_bits(0, 2); // a type the writer cannot produce
        // No payload follows at all -- a real entry would have one.
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            parse_spawn_baseline(&mut r, &reg, 4).unwrap_err(),
            EntityError::BadEntityType(0)
        );
        assert!(!r.overflowed(), "refused without reading past the header");
    }

    #[test]
    fn baselines_feed_straight_into_a_packet_context() {
        // The end-to-end shape the client layer will use: parse the baselines
        // out of the signon, then decode a full frame against them.
        let reg = registry();
        let mut w = BitWriter::new();
        w.write_bits(12, MAX_EDICT_BITS);
        w.write_bits(u32::from(ENTITY_NORMAL), 2);
        write_delta(
            &mut w,
            &entity_table(),
            &fields(&[("modelindex", Value::Int(64)), ("solid", Value::Int(2))]),
        );
        w.write_bits(BASELINE_SENTINEL, 16);
        w.write_bits(0, MAX_BASELINE_BITS);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let b = parse_spawn_baseline(&mut r, &reg, 8).unwrap();

        let ctx = PacketCtx {
            registry: &reg,
            baselines: &b.by_number,
            instanced: &b.instanced,
            maxclients: 8,
        };

        let mut w = BitWriter::new();
        let mut nb = 0i32;
        write_entity(
            &mut w,
            &hdr(12),
            &mut nb,
            true,
            b.instanced.len(),
            &entity_table(),
            &fields(&[("origin[0]", Value::Float(256.0))]),
        );
        terminate(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let frame = parse_packet_entities_full_checked(&mut r, &ctx, 1).unwrap();
        assert_eq!(frame[0].origin(), [256.0, 0.0, 0.0]);
        assert_eq!(frame[0].i64("modelindex"), 64, "inherited from the baseline");
        assert_eq!(frame[0].i64("solid"), 2);
    }
}
