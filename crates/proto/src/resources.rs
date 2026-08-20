//! Resource list and consistency checking.
//!
//! Port of `internal/proto/resources.go`. A GoldSrc server sends the client a
//! list of resources (models, sounds, decals, generic files) during signon and
//! may demand MD5 "consistency" proof for some of them; a client that does not
//! answer correctly gets dropped.
//!
//! Verified from the binary:
//!
//! * `FileHashMD5` (`0x1406E72A0`) is a plain `crypto/md5.Sum` over the bytes
//! * the client's replies come from `buildClientResourceList` and
//!   `buildConsistencyResponse` in `internal/client/messages.go`, which
//!   reference the literals `"tempdecal:"` and `"sound/"`
//!
//! ## The `svc_resourcelist` wire format
//!
//! Recovered from a live capture rather than the disassembly (the `BitReader`
//! calls are inlined into `ParseResourceList`, so the widths are not readable
//! statically). Validated by parsing a real 793-entry list end to end:
//!
//! Now corroborated directly against `SV_SendResources_internal`
//! (`rehlds/engine/sv_main.cpp:1225-1259`), which resolves an ambiguity the
//! capture alone could not:
//!
//! ```text
//! count : 12 bits                              RESOURCE_INDEX_BITS
//! repeat count times:
//!     type     :  4 bits
//!     name     :  nul-terminated string, 8 bits per character
//!     index    : 12 bits
//!     size     : 24 bits
//!     flags    :  3 bits                       masked to RES_WASMISSING|RES_FATALIFMISSING
//!     if flags & RES_CUSTOM: 16 bytes MD5      dead branch -- see below
//!     reserved :  1 bit, and if set 32 bytes
//! <consistency list>                           see `ConsistencyList`
//! ```
//!
//! The earlier reading of "4 bits of flags, bit 3 means a 32-byte blob" decodes
//! the same bits, because the engine writes 3 flag bits followed by the
//! reserved-present bit — so a 4-bit read lands that bit at value 8. It is the
//! *meaning* that was wrong, and it mattered: the 32-byte blob is
//! `rguc_reserved`, **not** an MD5. It is a `COM_Munge`'d block holding a
//! `check_type` byte plus model bounds (`sv_user.cpp:287-312`), and answering
//! consistency with its first four bytes as if they were a hash is nonsense.
//!
//! **The MD5 is never on the wire.** The engine writes flags as
//! `bits(ucFlags & (RES_WASMISSING|RES_FATALIFMISSING), 3)` — a `0x03` mask —
//! so `RES_CUSTOM` (`1 << 2`) can never reach the client, and the branch that
//! would carry the 16-byte hash is unreachable. It is mirrored here anyway,
//! because the reference client mirrors it (`HLTV/Core/src/Server.cpp:1109`).
//!
//! The widths are corroborated by the decoded values being real files at real
//! sizes — `models/player.mdl` at 2,329,328 bytes, `player/pl_grate1.wav` at
//! 9,188 — every one of the 793 names readable, and the bit cursor landing
//! exactly on the end of the message once the consistency tail is consumed.
//! An incorrect width desynchronises within one or two entries.

use crate::bitbuf::BitReader;
use md5::{Digest, Md5};

/// Bits holding the resource count. `RESOURCE_INDEX_BITS`, `server.h:86`.
pub const COUNT_BITS: u32 = 12;
/// Bits per field of an entry.
pub const TYPE_BITS: u32 = 4;
pub const INDEX_BITS: u32 = 12;
pub const SIZE_BITS: u32 = 24;
/// Only three, and masked to `RES_WASMISSING|RES_FATALIFMISSING` on the way out
/// (`sv_main.cpp:1240`).
pub const FLAGS_BITS: u32 = 3;

/// `resource_t::ucFlags` bits, `public/rehlds/custom.h:52-60`.
pub const RES_FATALIFMISSING: u8 = 1 << 0;
pub const RES_WASMISSING: u8 = 1 << 1;
/// Never observable on the wire — see the module docs.
pub const RES_CUSTOM: u8 = 1 << 2;

/// Size of `resource_t::rguc_reserved`, the munged bounds block.
pub const RESERVED_LEN: usize = 32;
/// Size of `resource_t::rgucMD5_hash`.
pub const MD5_LEN: usize = 16;

/// Bits of a delta-coded consistency index, and the absolute fallback.
/// `SV_SendConsistencyList`, `sv_user.cpp:352-366`.
pub const CONSISTENCY_DELTA_BITS: u32 = 5;
pub const CONSISTENCY_ABSOLUTE_BITS: u32 = 10;

/// Which resources the server wants a `clc_fileconsistency` answer for.
///
/// Transmitted at the tail of the *same* bit block as the resource list
/// (`SV_SendConsistencyList`, `sv_user.cpp:334-380`), so a parser that stops
/// after the entries leaves the bit cursor short and desynchronises whatever
/// reads next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsistencyList {
    /// The single gate bit. When clear the server is not asking, and a client
    /// that answers anyway is dropped: `SV_ParseConsistencyResponse` requires
    /// `length == g_psv.num_consistency` (`sv_user.cpp:194`), and
    /// `num_consistency` stays non-zero even under `mp_consistency 0`.
    pub should_send: bool,
    /// Resource indices to answer, in the order the server listed them.
    pub indices: Vec<u32>,
}

/// Parse `svc_resourcelist` (opcode already consumed), including the
/// consistency tail that shares its bit block.
pub fn parse_resource_list_full(r: &mut BitReader) -> (Vec<Resource>, ConsistencyList) {
    let count = r.read_bits(COUNT_BITS) as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let res_type = r.read_bits(TYPE_BITS) as u8;
        let name = r.read_string();
        let index = r.read_bits(INDEX_BITS);
        let size = r.read_bits(SIZE_BITS);
        let flags = r.read_bits(FLAGS_BITS) as u8;
        let md5 = if flags & RES_CUSTOM != 0 {
            let mut b = [0u8; MD5_LEN];
            for x in b.iter_mut() {
                *x = r.read_byte();
            }
            Some(b)
        } else {
            None
        };
        let reserved = if r.read_bits(1) != 0 {
            let mut b = [0u8; RESERVED_LEN];
            for x in b.iter_mut() {
                *x = r.read_byte();
            }
            Some(b)
        } else {
            None
        };
        if name.is_empty() || r.overflowed() {
            return (out, ConsistencyList::default());
        }
        out.push(Resource {
            name,
            res_type: ResourceType::from_u8(res_type),
            index,
            size,
            flags,
            md5,
            reserved,
        });
    }

    // `bits(1)` gate, then `while bits(1) { bits(1) ? +5-bit delta : 10-bit
    // absolute }`, terminated by a clear bit. `lastcheck` starts at 0, so the
    // first entry at index 0 encodes as [1][00000].
    let mut consistency = ConsistencyList::default();
    consistency.should_send = r.read_bits(1) != 0;
    if consistency.should_send {
        let mut lastcheck = 0u32;
        while r.read_bits(1) != 0 && !r.overflowed() {
            let i = if r.read_bits(1) != 0 {
                lastcheck + r.read_bits(CONSISTENCY_DELTA_BITS)
            } else {
                r.read_bits(CONSISTENCY_ABSOLUTE_BITS)
            };
            lastcheck = i;
            consistency.indices.push(i);
        }
    }
    (out, consistency)
}

/// Parse `svc_resourcelist`, discarding the consistency tail.
///
/// Only safe when nothing reads after this block. Prefer
/// [`parse_resource_list_full`].
pub fn parse_resource_list(r: &mut BitReader) -> Vec<Resource> {
    parse_resource_list_full(r).0
}

/// The server's whole reply to `sendres`, parsed from the opcode byte.
///
/// The byte prologue and the bit block are one unit and have to be read
/// together (`SV_SendResources_internal`, `sv_main.cpp:1210-1260`):
///
/// ```text
/// byte  svc_resourcerequest (45)
/// long  spawncount
/// long  0
/// [byte svc_resourcelocation (56); string sv_downloadurl]   // only if set
/// byte  svc_resourcelist (43)
/// <one bit block: entries, then the consistency list>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMessage {
    pub spawncount: u32,
    pub download_url: Option<String>,
    pub resources: Vec<Resource>,
    pub consistency: ConsistencyList,
    /// Bytes consumed, so a caller walking a larger stream can resume.
    pub consumed: usize,
}

/// Opcodes appearing in the `sendres` reply.
const SVC_RESOURCEREQUEST: u8 = 45;
const SVC_RESOURCELOCATION: u8 = 56;
const SVC_RESOURCELIST: u8 = 43;

/// Parse a message that begins with `svc_resourcerequest`.
///
/// Returns `None` if it does not, or if it is truncated.
pub fn parse_resource_message(msg: &[u8]) -> Option<ResourceMessage> {
    if msg.first() != Some(&SVC_RESOURCEREQUEST) || msg.len() < 10 {
        return None;
    }
    let spawncount = u32::from_le_bytes(msg[1..5].try_into().ok()?);
    // The second long is a start index and is always 0; the reference client
    // validates it (`HLTV/Core/src/Server.cpp:908-919`).
    if u32::from_le_bytes(msg[5..9].try_into().ok()?) != 0 {
        return None;
    }
    let mut at = 9usize;

    let mut download_url = None;
    if msg.get(at) == Some(&SVC_RESOURCELOCATION) {
        at += 1;
        let end = msg[at..].iter().position(|&b| b == 0)?;
        download_url = Some(String::from_utf8_lossy(&msg[at..at + end]).into_owned());
        at += end + 1;
    }

    if msg.get(at) != Some(&SVC_RESOURCELIST) {
        return None;
    }
    at += 1;

    let mut r = BitReader::new(&msg[at..]);
    let (resources, consistency) = parse_resource_list_full(&mut r);
    if r.overflowed() {
        return None;
    }
    // A bit block occupies ceil(bits/8) bytes and the reader resumes aligned
    // (`MSG_EndBitReading`, common.cpp:628-656).
    let consumed = at + r.byte_pos() + usize::from(r.bit_offset() > 0);

    Some(ResourceMessage {
        spawncount,
        download_url,
        resources,
        consistency,
        consumed,
    })
}

/// GoldSrc resource types, as used in the resource list.
///
/// Names follow the engine's `resourcetype_t`. These are ordinal values, not
/// bit flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResourceType {
    Sound = 0,
    Skin = 1,
    Model = 2,
    Decal = 3,
    Generic = 4,
    EventScript = 5,
    World = 6,
}

impl ResourceType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Sound,
            1 => Self::Skin,
            2 => Self::Model,
            3 => Self::Decal,
            4 => Self::Generic,
            5 => Self::EventScript,
            6 => Self::World,
            _ => return None,
        })
    }
}

/// Prefix the client prepends to sound resources when hashing them.
pub const SOUND_PREFIX: &str = "sound/";
/// Marker for the temporary-decal wad entry.
pub const TEMPDECAL_PREFIX: &str = "tempdecal:";

/// One entry of the server's resource list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub name: String,
    pub res_type: Option<ResourceType>,
    pub index: u32,
    pub size: u32,
    /// Three bits, so only `RES_FATALIFMISSING` and `RES_WASMISSING` survive.
    pub flags: u8,
    /// `rgucMD5_hash`. Unreachable in practice — see the module docs.
    pub md5: Option<[u8; MD5_LEN]>,
    /// `rguc_reserved`, still `COM_Munge`'d with the spawncount. Carries a
    /// `check_type` byte and model bounds for consistency-checked models
    /// (`sv_user.cpp:287-312`). **Not a hash.**
    pub reserved: Option<[u8; RESERVED_LEN]>,
}

impl Resource {
    /// The path the client actually hashes: sounds live under `sound/`.
    pub fn hash_path(&self) -> String {
        if self.res_type == Some(ResourceType::Sound) && !self.name.starts_with(SOUND_PREFIX) {
            format!("{SOUND_PREFIX}{}", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// MD5 of a file's contents â€” the value a consistency response carries.
pub fn file_hash_md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finalize().into()
}

/// Does `data` satisfy the consistency hash the server asked for?
pub fn consistency_matches(data: &[u8], expected: &[u8; 16]) -> bool {
    &file_hash_md5(data) == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(
            file_hash_md5(b""),
            [
                0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
                0x42, 0x7e
            ]
        );
        assert_eq!(
            file_hash_md5(b"abc"),
            [
                0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1,
                0x7f, 0x72
            ]
        );
    }

    #[test]
    fn consistency_compares_exactly() {
        let data = b"some model bytes";
        let h = file_hash_md5(data);
        assert!(consistency_matches(data, &h));
        assert!(!consistency_matches(b"other bytes", &h));
    }

    #[test]
    fn sound_resources_get_the_sound_prefix() {
        let r = Resource {
            name: "weapons/ak47-1.wav".into(),
            res_type: Some(ResourceType::Sound),
            index: 3,
            size: 0,
            flags: 0,
            md5: None,
            reserved: None,
        };
        assert_eq!(r.hash_path(), "sound/weapons/ak47-1.wav");
    }

    #[test]
    fn already_prefixed_sounds_are_not_double_prefixed() {
        let r = Resource {
            name: "sound/ambience/wind.wav".into(),
            res_type: Some(ResourceType::Sound),
            index: 4,
            size: 0,
            flags: 0,
            md5: None,
            reserved: None,
        };
        assert_eq!(r.hash_path(), "sound/ambience/wind.wav");
    }

    #[test]
    fn non_sound_resources_are_untouched() {
        let r = Resource {
            name: "models/player.mdl".into(),
            res_type: Some(ResourceType::Model),
            index: 1,
            size: 0,
            flags: 0,
            md5: None,
            reserved: None,
        };
        assert_eq!(r.hash_path(), "models/player.mdl");
    }

    #[test]
    fn resource_type_round_trips() {
        for v in 0u8..=6 {
            assert_eq!(ResourceType::from_u8(v).map(|t| t as u8), Some(v));
        }
        assert_eq!(ResourceType::from_u8(7), None);
    }
}

