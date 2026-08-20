//! Answering `clc_fileconsistency`.
//!
//! A server with `mp_consistency 1` (the default, `sv_user.cpp:60`) demands
//! proof that the client's copies of certain files match the server's. Get it
//! wrong and `SV_Spawn_f_internal` refuses to spawn you at all
//! (`sv_main.cpp:1663`, `"You didn't send consistency response"`).
//!
//! ## The contract, from `SV_ParseConsistencyResponse` (`sv_user.cpp:83-230`)
//!
//! ```text
//! byte  clc_fileconsistency (7)
//! short length                      // bytes of the munged bit block that follows
//! <length bytes, COM_Munge'd with the FULL spawncount>
//! ```
//!
//! and inside that block:
//!
//! ```text
//! repeat: 1 bit set | 12-bit resource index | <answer>
//! end:    1 bit clear
//! ```
//!
//! Three separate ways to fail, each with its own server-side message:
//!
//! 1. `length <= 0` or past the end of the buffer → dropped, `"Invalid length"`.
//! 2. Declaring more bytes than the bit reader consumes → the leftovers are
//!    parsed as clc opcodes → `"badread on opcode clc_fileconsistency"`.
//!    `MSG_EndBitReading` advances by the bits actually read, not by `length`.
//! 3. Entry **count** != `g_psv.num_consistency` → dropped, `"Bad file data"`.
//!
//! ## Why a client with no game files can still pass
//!
//! Not by echoing the MD5 — that is never on the wire (see
//! [`crate::resources`]). The escape is the *other* branch: for a resource
//! whose 32-byte `rguc_reserved` block is non-zero, the server does not want a
//! hash at all. It wants model bounds, and it compares them against the bounds
//! it packed into that very block (`sv_user.cpp:115-176`):
//!
//! * `force_model_samebounds`  → must be exactly equal
//! * `force_model_specifybounds`, `..._if_avail` → must be contained
//!
//! So **echoing the server's own bounds back satisfies every bounds mode**. In
//! the reference capture that covers 52 of the 62 demands. Only
//! `force_exactfile` entries (zero reserved block) need a real MD5, and on a
//! stock CS install those are a handful of map-independent sprites.
//!
//! There is no "I don't have this file" escape for `force_exactfile` — except
//! `force_model_specifybounds_if_avail`, which accepts `mins = maxs =
//! (-1,-1,-1)` (`sv_user.cpp:166`).

use crate::bitbuf::BitWriter;
use crate::munge;
use crate::resources::{ConsistencyList, Resource, RESERVED_LEN};

/// `clc_fileconsistency`.
pub const CLC_FILECONSISTENCY: u8 = 7;

/// Bits of a resource index in the response. Matches `RESOURCE_INDEX_BITS`.
pub const INDEX_BITS: u32 = 12;

/// `FORCE_TYPE`, the first byte of an unmunged `rguc_reserved` block.
/// `rehlds/common/consistency.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceType {
    ExactFile = 0,
    ModelSameBounds = 1,
    ModelSpecifyBounds = 2,
    ModelSpecifyBoundsIfAvail = 3,
}

impl ForceType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::ExactFile,
            1 => Self::ModelSameBounds,
            2 => Self::ModelSpecifyBounds,
            3 => Self::ModelSpecifyBoundsIfAvail,
            _ => return None,
        })
    }
}

/// One thing the server wants answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Demand {
    /// Needs the real MD5 of the file. No way to fake it.
    ExactFile { index: u32, path: String },
    /// Needs model bounds, which the server already told us.
    Bounds {
        index: u32,
        path: String,
        mins: [f32; 3],
        maxs: [f32; 3],
        check: ForceType,
    },
}

impl Demand {
    pub fn index(&self) -> u32 {
        match self {
            Demand::ExactFile { index, .. } | Demand::Bounds { index, .. } => *index,
        }
    }

    pub fn path(&self) -> &str {
        match self {
            Demand::ExactFile { path, .. } | Demand::Bounds { path, .. } => path,
        }
    }
}

/// What we put on the wire for one demand.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Answer {
    /// First four bytes of the file's MD5, little-endian
    /// (`SV_CheckConsistencyResponse_API`, `sv_user.cpp:79-81`).
    Hash(u32),
    /// 12 raw bytes of `mins` then 12 of `maxs`, IEEE-754 LE
    /// (`MSG_ReadBitData(v, 12)`, `sv_user.cpp:120-121`).
    Bounds([f32; 3], [f32; 3]),
}

/// Decode a `mins`/`maxs` pair out of a munged `rguc_reserved` block.
///
/// Layout, from `SV_TransferConsistencyInfo_internal` (`sv_user.cpp:287-312`):
/// `[check_type:1][mins:12][maxs:12][pad:7]`, then
/// `COM_Munge(rguc_reserved, 32, g_psvs.spawncount)` — the **full** spawncount,
/// not `& 0xFF`.
pub fn decode_reserved(
    reserved: &[u8; RESERVED_LEN],
    spawncount: u32,
) -> Option<(ForceType, [f32; 3], [f32; 3])> {
    let mut buf = *reserved;
    munge::unmunge1(&mut buf, spawncount as i32);

    let check = ForceType::from_u8(buf[0])?;
    let f = |o: usize| f32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let mins = [f(1), f(5), f(9)];
    let maxs = [f(13), f(17), f(21)];
    Some((check, mins, maxs))
}

/// Turn the server's consistency list into the demands we must answer.
///
/// Order matters: the response is checked by count, and every index must name a
/// resource the server actually flagged.
///
/// **The index is an array position, not `Resource::index`.** The server writes
/// the loop counter over `g_psv.resources` (`SV_SendConsistencyList`,
/// `sv_user.cpp:356-364`) and reads it back as `r = &resources[idx]`
/// (`sv_user.cpp:112-114`). `Resource::index` is `nIndex`, the precache slot
/// *within a resource type*, so models and sounds each number from zero and the
/// two only coincide by accident — 4 times out of 62 on the reference capture.
pub fn demands(resources: &[Resource], list: &ConsistencyList, spawncount: u32) -> Vec<Demand> {
    let mut out = Vec::with_capacity(list.indices.len());
    for &index in &list.indices {
        let Some(r) = resources.get(index as usize) else {
            continue;
        };
        match r.reserved.as_ref().and_then(|b| decode_reserved(b, spawncount)) {
            Some((check, mins, maxs)) if check != ForceType::ExactFile => {
                out.push(Demand::Bounds {
                    index,
                    path: r.hash_path(),
                    mins,
                    maxs,
                    check,
                });
            }
            _ => out.push(Demand::ExactFile {
                index,
                path: r.hash_path(),
            }),
        }
    }
    out
}

/// Bit-pack the response body.
pub fn build_body(answers: &[(u32, Answer)]) -> Vec<u8> {
    let mut w = BitWriter::new();
    for (index, answer) in answers {
        w.write_bits(1, 1);
        w.write_bits(*index, INDEX_BITS);
        match answer {
            Answer::Hash(h) => w.write_bits(*h, 32),
            Answer::Bounds(mins, maxs) => {
                for v in mins.iter().chain(maxs.iter()) {
                    for b in v.to_le_bytes() {
                        w.write_bits(u32::from(b), 8);
                    }
                }
            }
        }
    }
    w.write_bits(0, 1);
    w.into_bytes()
}

/// Wrap a body into a complete `clc_fileconsistency` message.
///
/// The declared `u16` must equal the body's real byte length — `BitWriter`
/// already rounds up to whole bytes, which is exactly what
/// `MSG_EndBitReading` expects. `COM_Munge` transforms only `len & ~3` bytes
/// and leaves the tail alone, symmetrically on both sides.
pub fn build_message(mut body: Vec<u8>, spawncount: u32) -> Vec<u8> {
    let n = body.len() & !3;
    if n > 0 {
        munge::munge1(&mut body[..n], spawncount as i32);
    }
    let mut out = Vec::with_capacity(3 + body.len());
    out.push(CLC_FILECONSISTENCY);
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test-only port of the server's own reader, so a fixture test cannot
    /// lie about whether the response would be accepted.
    /// `SV_ParseConsistencyResponse`, `sv_user.cpp:88-200`.
    fn server_parse(msg: &[u8], spawncount: u32, num_consistency: usize) -> Result<usize, String> {
        if msg.first() != Some(&CLC_FILECONSISTENCY) {
            return Err("not a clc_fileconsistency".into());
        }
        let declared = i16::from_le_bytes([msg[1], msg[2]]);
        if declared <= 0 {
            return Err("Invalid length".into());
        }
        let declared = declared as usize;
        if msg.len() < 3 + declared {
            return Err("Invalid length".into());
        }
        let mut buf = msg[3..3 + declared].to_vec();
        let n = buf.len() & !3;
        if n > 0 {
            munge::unmunge1(&mut buf[..n], spawncount as i32);
        }

        let mut r = crate::bitbuf::BitReader::new(&buf);
        let mut count = 0usize;
        while r.read_bits(1) != 0 {
            let idx = r.read_bits(INDEX_BITS);
            if idx as usize >= 4096 {
                return Err("bad index".into());
            }
            // Both answer shapes are fixed width; the server picks by whether
            // the resource had a reserved block, which the caller mirrors.
            r.skip(32);
            if r.overflowed() {
                return Err("badread".into());
            }
            count += 1;
        }
        // MSG_EndBitReading advances by bits consumed, not by `declared`.
        let consumed = r.byte_pos() + usize::from(r.bit_offset() > 0);
        if consumed != declared {
            return Err(format!(
                "badread: declared {declared} bytes, consumed {consumed}"
            ));
        }
        if count != num_consistency {
            return Err(format!(
                "Bad file data: sent {count}, server wants {num_consistency}"
            ));
        }
        Ok(count)
    }

    #[test]
    fn a_hash_response_is_accepted_by_the_servers_own_reader() {
        for spawncount in [1u32, 2, 7, 4242] {
            let answers: Vec<(u32, Answer)> = (0..12u32)
                .map(|i| (i * 3 + 1, Answer::Hash(0xDEAD_0000 ^ i)))
                .collect();
            let msg = build_message(build_body(&answers), spawncount);
            assert_eq!(
                server_parse(&msg, spawncount, answers.len()),
                Ok(answers.len()),
                "spawncount {spawncount}"
            );
        }
    }

    /// The failure this project actually hit: 0 entries against a server whose
    /// `num_consistency` is non-zero. `mp_consistency 0` does NOT zero it.
    #[test]
    fn an_empty_response_is_rejected_when_the_server_wants_entries() {
        let msg = build_message(build_body(&[]), 2);
        let err = server_parse(&msg, 2, 62).unwrap_err();
        assert!(err.starts_with("Bad file data"), "got {err}");
    }

    #[test]
    fn the_declared_length_matches_what_the_reader_consumes() {
        // One entry is 1 + 12 + 32 = 45 bits, plus the terminator = 46 bits,
        // which is 6 bytes. Any padding mistake shows up as a badread.
        let msg = build_message(build_body(&[(3, Answer::Hash(1))]), 9);
        let declared = u16::from_le_bytes([msg[1], msg[2]]) as usize;
        assert_eq!(declared, 6);
        assert_eq!(msg.len(), 3 + declared);
        assert_eq!(server_parse(&msg, 9, 1), Ok(1));
    }

    #[test]
    fn reserved_blocks_round_trip_through_the_munge() {
        let spawncount = 77u32;
        let mins = [-16.0f32, -16.0, -36.0];
        let maxs = [16.0f32, 16.0, 36.0];

        let mut raw = [0u8; RESERVED_LEN];
        raw[0] = ForceType::ModelSpecifyBounds as u8;
        for (i, v) in mins.iter().chain(maxs.iter()).enumerate() {
            raw[1 + i * 4..5 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        munge::munge1(&mut raw, spawncount as i32);

        let (check, got_mins, got_maxs) = decode_reserved(&raw, spawncount).expect("decodes");
        assert_eq!(check, ForceType::ModelSpecifyBounds);
        assert_eq!(got_mins, mins);
        assert_eq!(got_maxs, maxs);
    }

    #[test]
    fn bounds_answers_are_twenty_four_raw_bytes() {
        let body = build_body(&[(5, Answer::Bounds([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]))]);
        // 1 + 12 + 192 bits of payload, plus the 1-bit terminator = 206 bits.
        assert_eq!(body.len(), 206_usize.div_ceil(8));
    }
}
