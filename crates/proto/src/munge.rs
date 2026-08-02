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

// The six named GoldSrc entry points. Verified against ReHLDS
// `engine/common.cpp:2526-2845`: all six are the *same* algorithm and differ
// only in which 16-byte table they index, so they are one-line wrappers here
// rather than six transcriptions. (ReHLDS also ships unrolled REHLDS_FIXES
// variants of Munge2/UnMunge2; those reduce to the generic form -- their
// constant `0xFFFFE7A5` is exactly `0xa5|(j<<j)|j|mungify_table2[j]` for
// j = 0..3.)

/// `COM_Munge` — the reliable/fragment path and the `clc_move` payload.
#[inline]
pub fn munge1(data: &mut [u8], seq: i32) {
    munge(data, &TABLE1, seq)
}

/// `COM_UnMunge`.
#[inline]
pub fn unmunge1(data: &mut [u8], seq: i32) {
    unmunge(data, &TABLE1, seq)
}

/// `COM_Munge2` — produces the `spawn` map-CRC argument.
#[inline]
pub fn munge2(data: &mut [u8], seq: i32) {
    munge(data, &TABLE2, seq)
}

/// `COM_UnMunge2` — what the server applies to our `spawn` CRC argument
/// (`sv_main.cpp:1653`).
#[inline]
pub fn unmunge2(data: &mut [u8], seq: i32) {
    unmunge(data, &TABLE2, seq)
}

/// `COM_Munge3`.
#[inline]
pub fn munge3(data: &mut [u8], seq: i32) {
    munge(data, &TABLE3, seq)
}

/// `COM_UnMunge3` — recovers the real map CRC from `svc_serverinfo`.
#[inline]
pub fn unmunge3(data: &mut [u8], seq: i32) {
    unmunge(data, &TABLE3, seq)
}

/// The `(-1 - n) & 0xFF` key GoldSrc derives from a player number or spawncount.
///
/// Appears verbatim at `rehlds/engine/sv_main.cpp:1113` (serverinfo write) and
/// `:1653` (spawn parse), and in the reference client at
/// `HLTV/Core/src/Server.cpp:781` and `:1144`.
#[inline]
pub fn seq_key(n: u32) -> i32 {
    (-1i32 - n as i32) & 0xFF
}

/// The `<crc>` argument of `spawn <spawncount> <crc>`.
///
/// `map_crc_wire` is the third `long` of `svc_serverinfo` exactly as received.
/// The server wrote `COM_Munge3(worldmapCRC, seq_key(playernum))` there
/// (`sv_main.cpp:1111-1115`), and on the way back in `SV_Spawn_f_internal`
/// applies `COM_UnMunge2(crcValue, 4, seq_key(spawncount))`
/// (`sv_main.cpp:1653`). So we undo its munge and redo the one it expects.
/// Mirror of the reference client, `HLTV/Core/src/Server.cpp:781` and `:1143`.
///
/// **Sending `0` here is not a harmless placeholder.** Unmunging zero can never
/// yield zero, so the server stores a garbage CRC, and `SV_CheckMapDifferences`
/// (`sv_main.cpp:8092-8115`, every 5 s) reacts to the mismatch by setting
/// `SIZEBUF_OVERFLOWED` on the reliable channel — which surfaces as the
/// client being dropped for `"Reliable channel overflowed"`, a message that
/// says nothing whatsoever about map CRCs.
pub fn spawn_crc(map_crc_wire: u32, player_index: u8, spawncount: u32) -> i32 {
    let mut b = map_crc_wire.to_le_bytes();
    unmunge3(&mut b, seq_key(u32::from(player_index)));
    munge2(&mut b, seq_key(spawncount));
    i32::from_le_bytes(b)
}

/// The server's real `worldmapCRC`, recovered from `svc_serverinfo`.
///
/// Only useful as a cross-check: it must equal the value in the server's own
/// `Started map "<name>" (CRC "<n>")` log line (`sv_main.cpp:6242`).
pub fn world_map_crc(map_crc_wire: u32, player_index: u8) -> i32 {
    let mut b = map_crc_wire.to_le_bytes();
    unmunge3(&mut b, seq_key(u32::from(player_index)));
    i32::from_le_bytes(b)
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

    /// Closed loop against the server's own arithmetic.
    ///
    /// Play both sides: munge a known `worldmapCRC` the way `SV_SendServerinfo`
    /// does, run it through `spawn_crc`, then apply the `COM_UnMunge2` that
    /// `SV_Spawn_f_internal` applies — and require the original back. If this
    /// passes, `SV_CheckMapDifferences` cannot fire, without a server running.
    #[test]
    fn the_spawn_crc_survives_a_round_trip_through_the_servers_own_maths() {
        for w in [0u32, 1, 0x2B99_5581, 0xFFFF_FFFF, 0xDEAD_BEEF] {
            for p in [0u8, 1, 11, 31] {
                for s in [0u32, 1, 5, 255, 256, 70_000] {
                    let mut wire = w.to_le_bytes();
                    munge3(&mut wire, seq_key(u32::from(p)));
                    let wire = u32::from_le_bytes(wire);

                    // What the client sends, and what the server does with it.
                    let arg = spawn_crc(wire, p, s);
                    let mut back = arg.to_le_bytes();
                    unmunge2(&mut back, seq_key(s));

                    assert_eq!(
                        u32::from_le_bytes(back),
                        w,
                        "crc {w:#x} playernum {p} spawncount {s} did not round-trip"
                    );
                    assert_eq!(world_map_crc(wire, p) as u32, w, "world_map_crc disagrees");
                }
            }
        }
    }

    /// Regression guard for the bug this all existed to fix.
    ///
    /// `SV_CheckMapDifferences` skips clients whose `crcValue` is zero, so if
    /// unmunging zero gave zero, sending `spawn <n> 0` would have been benign.
    /// It does not: for `len == 4` the unmunge reduces to
    /// `bswap(mSeq ^ 0xFFFFE7A5)` with `mSeq = bswap(!seq) ^ seq`, and byte 1
    /// of `mSeq` is always `0xFF`, never `0xE7`. So the result is never zero
    /// for any key, and the mismatch — and the drop — was guaranteed.
    #[test]
    fn a_zero_spawn_crc_unmunges_to_the_value_that_got_us_dropped() {
        let mut b = 0u32.to_le_bytes();
        unmunge2(&mut b, seq_key(5));
        assert_eq!(u32::from_le_bytes(b), 0xA018_00FA);

        for spawncount in 0u32..512 {
            let mut b = 0u32.to_le_bytes();
            unmunge2(&mut b, seq_key(spawncount));
            assert_ne!(
                u32::from_le_bytes(b),
                0,
                "spawncount {spawncount}: a zero CRC argument would have been harmless"
            );
        }
    }

    /// The named wrappers must be the generic function with the right table.
    #[test]
    fn the_named_variants_match_the_generic_function() {
        for (i, (m, u)) in [
            (munge1 as fn(&mut [u8], i32), unmunge1 as fn(&mut [u8], i32)),
            (munge2, unmunge2),
            (munge3, unmunge3),
        ]
        .into_iter()
        .enumerate()
        {
            let table = [&TABLE1, &TABLE2, &TABLE3][i];
            let original = sample();

            let mut a = original.clone();
            m(&mut a, 77);
            let mut b = original.clone();
            munge(&mut b, table, 77);
            assert_eq!(a, b, "munge variant {} disagrees", i + 1);

            u(&mut a, 77);
            assert_eq!(a, original, "unmunge variant {} did not invert", i + 1);
        }
    }

    #[test]
    fn seq_key_matches_the_engines_expression() {
        // `(-1 - n) & 0xFF`, as written at sv_main.cpp:1113 and :1653.
        assert_eq!(seq_key(0), 0xFF);
        assert_eq!(seq_key(1), 0xFE);
        assert_eq!(seq_key(5), 0xFA);
        assert_eq!(seq_key(255), 0);
        assert_eq!(seq_key(256), 0xFF);
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
