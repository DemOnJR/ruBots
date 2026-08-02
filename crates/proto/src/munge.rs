//! GoldSrc payload munging.
//!
//! Port of `internal/proto/munge.go`. Both the tables and the per-byte key
//! formula were extracted from the binary, not from published GoldSrc source:
//!
//! * tables live at `0x1407B00C0`, `0x1407B00D0`, `0x1407B00E0` (16 bytes each)
//! * the key is `0xA5 | (j << j) | j | table[(i + j) & 15]`
//!   (`mungeGeneric` @ `0x1406E6020`: `shl rcx, cl` with `rcx == cl == j`,
//!   then `or rdi, rcx` / `or rdi, 0xa5`)
//!
//! **`j << j` is correct — confirmed on the wire.** Commonly cited
//! `COM_Munge` listings use `j << (j + 1)`, which yields a different key for
//! every `j > 0`. Sending `clc_stringcmd "new"` to a live HLDS
//! `1.1.2.7/Stdio` server settles it:
//!
//! | key term | server reply |
//! |---|---|
//! | `j << j` | **1042 bytes** — signon data, i.e. the command was understood |
//! | `j << (j + 1)` | 90 bytes — not parsed |
//! | no munge at all | 16 bytes — bare acknowledgement |
//!
//! So the binary is right and the published formula does not apply to this
//! build. Do not "fix" this to match a reference implementation.

/// `mungify_table` — used by the reliable/fragment path.
pub const TABLE1: [u8; 16] = [
    0x7A, 0x64, 0x05, 0xF1, 0x1B, 0x9B, 0xA0, 0xB5, 0xCA, 0xED, 0x61, 0x0D, 0x4A, 0xDF, 0x8E, 0xC7,
];

/// `mungify_table2`.
pub const TABLE2: [u8; 16] = [
    0x05, 0x61, 0x7A, 0xED, 0x1B, 0xCA, 0x0D, 0x9B, 0x4A, 0xF1, 0x64, 0xC7, 0xB5, 0x8E, 0xDF, 0xA0,
];

/// `mungify_table3`.
pub const TABLE3: [u8; 16] = [
    0x20, 0x07, 0x13, 0x61, 0x03, 0x45, 0x17, 0x72, 0x0A, 0x2D, 0x48, 0x0C, 0x4A, 0x12, 0xA9, 0xB5,
];

#[inline]
fn key(table: &[u8; 16], i: u32, j: u32) -> u8 {
    (0xA5u32 | (j << j) | j | u32::from(table[((i + j) & 15) as usize])) as u8
}

/// Munge `data` in place, four bytes at a time. A trailing partial dword is
/// left untouched (`and r8, ~3` in the original).
pub fn munge(data: &mut [u8], table: &[u8; 16], seq: i32) {
    let words = data.len() / 4;
    let seq = seq as u32;
    for i in 0..words {
        let off = i * 4;
        let mut c = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        c ^= !seq;
        c = c.swap_bytes();
        let mut out = 0u32;
        for j in 0..4u32 {
            let b = (c >> (j * 8)) as u8;
            out |= u32::from(b ^ key(table, i as u32, j)) << (j * 8);
        }
        out ^= seq;
        data[off..off + 4].copy_from_slice(&out.to_le_bytes());
    }
}

/// Exact inverse of [`munge`].
pub fn unmunge(data: &mut [u8], table: &[u8; 16], seq: i32) {
    let words = data.len() / 4;
    let seq = seq as u32;
    for i in 0..words {
        let off = i * 4;
        let mut c = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        c ^= seq;
        let mut mid = 0u32;
        for j in 0..4u32 {
            let b = (c >> (j * 8)) as u8;
            mid |= u32::from(b ^ key(table, i as u32, j)) << (j * 8);
        }
        let out = mid.swap_bytes() ^ !seq;
        data[off..off + 4].copy_from_slice(&out.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        (0u8..=63).collect()
    }

    #[test]
    fn round_trips_for_every_table() {
        for table in [&TABLE1, &TABLE2, &TABLE3] {
            for seq in [0i32, 1, 7, 255, 1019, 65535, -3] {
                let original = sample();
                let mut buf = original.clone();
                munge(&mut buf, table, seq);
                assert_ne!(buf, original, "munge should change the payload");
                unmunge(&mut buf, table, seq);
                assert_eq!(buf, original, "table/seq {seq} failed to round-trip");
            }
        }
    }

    #[test]
    fn trailing_partial_dword_is_untouched() {
        let mut buf = vec![0xAAu8; 7];
        munge(&mut buf, &TABLE1, 1);
        assert_eq!(buf[4], 0xAA);
        assert_eq!(buf[6], 0xAA);
    }

    #[test]
    fn tables_are_permutations_of_each_other() {
        // TABLE1 and TABLE2 hold the same 16 byte values in a different order;
        // a typo in either constant would break this.
        let mut a = TABLE1;
        let mut b = TABLE2;
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b);
    }

    #[test]
    fn key_matches_the_extracted_formula() {
        // Spot values computed by hand from `0xA5 | (j<<j) | j | table[(i+j)&15]`.
        assert_eq!(key(&TABLE1, 0, 0), 0xA5 | 0x7A);
        assert_eq!(key(&TABLE1, 0, 1), (0xA5 | 2 | 1 | 0x64) as u8);
        assert_eq!(key(&TABLE1, 0, 2), (0xA5 | 8 | 2 | 0x05) as u8);
        assert_eq!(key(&TABLE1, 0, 3), (0xA5 | 24 | 3 | 0xF1) as u8);
    }
}
