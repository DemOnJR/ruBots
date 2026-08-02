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
//! ```text
//! count : 12 bits
//! repeat count times:
//!     type   :  4 bits
//!     name   :  nul-terminated string, 8 bits per character
//!     index  : 12 bits
//!     size   : 24 bits
//!     flags  :  4 bits
//!     if flags & CHECKSUM: 32 further bytes
//! ```
//!
//! The widths are corroborated by the decoded values being real files at real
//! sizes â€” `models/player.mdl` at 2,329,328 bytes, `player/pl_grate1.wav` at
//! 9,188 â€” and by the parse consuming 99.7% of the message with every one of
//! the 793 names readable. An incorrect width desynchronises within one or two
//! entries.

use crate::bitbuf::BitReader;
use md5::{Digest, Md5};

/// Bits holding the resource count.
pub const COUNT_BITS: u32 = 12;
/// Bits per field of an entry.
pub const TYPE_BITS: u32 = 4;
pub const INDEX_BITS: u32 = 12;
pub const SIZE_BITS: u32 = 24;
pub const FLAGS_BITS: u32 = 4;

/// Flag marking an entry that carries [`CHECKSUM_LEN`] trailing bytes.
///
/// 52 of the 793 entries in the reference capture set it, and they are the
/// consistency-checked ones (`models/player.mdl`, the player models, ...).
pub const FLAG_CHECKSUM: u32 = 8;

/// Size of the trailing blob on a checksummed entry.
pub const CHECKSUM_LEN: usize = 32;

/// Parse `svc_resourcelist` (opcode already consumed).
pub fn parse_resource_list(r: &mut BitReader) -> Vec<Resource> {
    let count = r.read_bits(COUNT_BITS) as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let res_type = r.read_bits(TYPE_BITS) as u8;
        let name = r.read_string();
        let index = r.read_bits(INDEX_BITS);
        let size = r.read_bits(SIZE_BITS);
        let flags = r.read_bits(FLAGS_BITS);
        let checksum = if flags & FLAG_CHECKSUM != 0 {
            let mut b = [0u8; CHECKSUM_LEN];
            for x in b.iter_mut() {
                *x = r.read_byte();
            }
            Some(b)
        } else {
            None
        };
        if name.is_empty() || r.overflowed() {
            break;
        }
        out.push(Resource {
            name,
            res_type: ResourceType::from_u8(res_type),
            index,
            size,
            flags: flags as u8,
            checksum,
        });
    }
    out
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
    pub flags: u8,
    /// Present when the server demands a consistency check for this resource.
    pub checksum: Option<[u8; CHECKSUM_LEN]>,
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
            checksum: None,
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
            checksum: None,
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
            checksum: None,
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

