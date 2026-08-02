//! CRC-32 and the GoldSrc per-packet sequence check byte.
//!
//! Port of `internal/proto/crc.go`. Verified from the binary:
//!
//! * `init.0` (`0x1406E3A40`) builds a standard reflected CRC-32 table with
//!   polynomial `0xEDB88320`, then copies it to a second location that is
//!   indexed **as raw bytes** later.
//! * `BlockSequenceCRCByte` (`0x1406E3AC0`) clamps the length to 60
//!   (`cmp rdi,0x3c / cmovg`), takes `seq % 1020` (`0x3FC`, the multiply-by-
//!   `0x8080808080808081` / `shr 9` division idiom), appends four bytes read
//!   out of the CRC table's byte view at that index, then runs CRC-32 with
//!   init `0xFFFFFFFF` (`mov ebx,0xffffffff`) and a final `not`.

const POLY: u32 = 0xEDB8_8320;

/// Reflected CRC-32 table, generated exactly as `init.0` does.
pub fn table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

fn table_ref() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static T: OnceLock<[u32; 256]> = OnceLock::new();
    T.get_or_init(table)
}

/// The table's little-endian byte view — this is what the sequence key is
/// sliced out of, which is why the modulus is `1024 - 4`.
fn table_bytes() -> &'static [u8; 1024] {
    use std::sync::OnceLock;
    static B: OnceLock<[u8; 1024]> = OnceLock::new();
    B.get_or_init(|| {
        let t = table_ref();
        let mut out = [0u8; 1024];
        for (i, v) in t.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        out
    })
}

/// Standard reflected CRC-32 (init `0xFFFFFFFF`, final complement).
pub fn crc32(data: &[u8]) -> u32 {
    let t = table_ref();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = (crc >> 8) ^ t[usize::from((crc as u8) ^ b)];
    }
    !crc
}

/// Maximum payload the sequence check byte covers.
pub const MAX_BLOCK: usize = 60;

/// Number of distinct sequence keys — the CRC table is 1024 bytes and the key
/// is a 4-byte window, so windows start at `0..=1019`.
pub const SEQ_MOD: usize = 1020;

/// GoldSrc `CRC_Block_Sequence`: check byte over (at most 60 bytes of) `data`
/// salted with a 4-byte key selected by the packet sequence number.
pub fn block_sequence_crc_byte(data: &[u8], seq: i32) -> u8 {
    let len = data.len().min(MAX_BLOCK);
    let idx = (seq.max(0) as usize) % SEQ_MOD;
    let key = &table_bytes()[idx..idx + 4];

    let mut buf = Vec::with_capacity(len + 4);
    buf.extend_from_slice(&data[..len]);
    buf.extend_from_slice(key);
    crc32(&buf) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vectors() {
        // The canonical IEEE CRC-32 check values.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn table_entry_zero_and_one_are_standard() {
        let t = table();
        assert_eq!(t[0], 0x0000_0000);
        assert_eq!(t[1], 0x7707_3096);
        assert_eq!(t[255], 0x2D02_EF8D);
    }

    #[test]
    fn sequence_byte_depends_on_sequence() {
        let payload = [0x11u8; 16];
        let a = block_sequence_crc_byte(&payload, 1);
        let b = block_sequence_crc_byte(&payload, 2);
        assert_ne!(a, b, "the sequence must salt the check byte");
    }

    #[test]
    fn sequence_wraps_at_1020() {
        let payload = [0x22u8; 16];
        assert_eq!(
            block_sequence_crc_byte(&payload, 5),
            block_sequence_crc_byte(&payload, 5 + SEQ_MOD as i32)
        );
    }

    #[test]
    fn payload_is_clamped_to_60_bytes() {
        let mut short = vec![0xEEu8; MAX_BLOCK];
        let a = block_sequence_crc_byte(&short, 3);
        short.extend_from_slice(&[0x99u8; 40]); // beyond the clamp
        let b = block_sequence_crc_byte(&short, 3);
        assert_eq!(a, b, "bytes past 60 must not affect the check byte");
    }
}
