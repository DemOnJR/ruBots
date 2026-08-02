//! ULZ decompression — the codec YaPB graph payloads are stored with.
//!
//! Port of `ulzUncompress` (`0x1406F7DA0`). The token layout was read
//! directly out of the disassembly:
//!
//! | instruction | meaning |
//! |---|---|
//! | `cmp r10, 0x20` / `jl` | token >= 32 means a literal run follows |
//! | `shr r10, 5` / `cmp r10, 7` | run length is `token >> 5`, 7 means "extended" |
//! | `and r10d, 0xf` | match length is the low nibble... |
//! | `lea r13, [r10 + 4]` | ...plus a minimum match of 4 |
//! | `cmp r10, 0xf` / `je` | a low nibble of 15 means "extended" |
//! | `and r12d, 0x10` / `shl r12, 0xc` | bit 4 of the token is distance bit 16 |
//!
//! Extended lengths are a chain of bytes: each `255` adds 255 and continues,
//! the first non-`255` byte terminates and is added.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UlzError {
    /// Input ended mid-token.
    Truncated,
    /// A match referenced data before the start of the output.
    BadDistance { distance: usize, produced: usize },
    /// Output grew past what the header promised.
    Overflow { limit: usize },
}

impl std::fmt::Display for UlzError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(f, "ULZ stream truncated"),
            Self::BadDistance { distance, produced } => {
                write!(f, "ULZ match distance {distance} exceeds {produced} bytes produced")
            }
            // The original logs "ULZ output size mismatch".
            Self::Overflow { limit } => write!(f, "ULZ output size mismatch (limit {limit})"),
        }
    }
}

/// Shortest match the format can encode.
pub const MIN_MATCH: usize = 4;

/// Read an extended length: `0xFF` continues, anything else terminates.
fn extended(input: &[u8], ip: &mut usize, base: usize) -> Result<usize, UlzError> {
    let mut len = base;
    loop {
        let c = *input.get(*ip).ok_or(UlzError::Truncated)?;
        *ip += 1;
        len += usize::from(c);
        if c != 0xFF {
            return Ok(len);
        }
    }
}

/// Decompress `input`, expecting exactly `expected` bytes out.
///
/// `expected` comes from the graph header's `uncompressed` field and bounds
/// the allocation, so a corrupt stream cannot be used to exhaust memory.
pub fn decompress(input: &[u8], expected: usize) -> Result<Vec<u8>, UlzError> {
    let mut out: Vec<u8> = Vec::with_capacity(expected.min(1 << 24));
    let mut ip = 0usize;

    while ip < input.len() {
        let token = usize::from(input[ip]);
        ip += 1;

        // Literal run.
        if token >= 0x20 {
            let mut run = token >> 5;
            if run == 7 {
                run = extended(input, &mut ip, run)?;
            }
            let end = ip.checked_add(run).ok_or(UlzError::Truncated)?;
            let lit = input.get(ip..end).ok_or(UlzError::Truncated)?;
            if out.len() + lit.len() > expected {
                return Err(UlzError::Overflow { limit: expected });
            }
            out.extend_from_slice(lit);
            ip = end;
            if ip >= input.len() {
                break;
            }
        }

        // Match.
        let nibble = token & 0x0F;
        let len = if nibble == 0x0F {
            extended(input, &mut ip, nibble + MIN_MATCH)?
        } else {
            nibble + MIN_MATCH
        };

        let lo = *input.get(ip).ok_or(UlzError::Truncated)?;
        let hi = *input.get(ip + 1).ok_or(UlzError::Truncated)?;
        ip += 2;
        let distance = ((token & 0x10) << 12) | usize::from(lo) | (usize::from(hi) << 8);

        if distance == 0 || distance > out.len() {
            return Err(UlzError::BadDistance { distance, produced: out.len() });
        }
        if out.len() + len > expected {
            return Err(UlzError::Overflow { limit: expected });
        }

        // Byte-at-a-time: LZ77 matches may overlap the output cursor.
        let start = out.len() - distance;
        for i in 0..len {
            let b = out[start + i];
            out.push(b);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal ULZ encoder — literals only. Used to build fixtures; the
    /// decoder is also exercised against hand-written token streams below.
    fn literal_only(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let run = (data.len() - i).min(6); // runs 1..6 fit in the token
            let token = (run << 5) as u8;
            out.push(token);
            out.extend_from_slice(&data[i..i + run]);
            i += run;
            if i < data.len() {
                // A match must follow a literal token unless input ends, so
                // only emit short runs when we are about to stop.
                break;
            }
        }
        out
    }

    #[test]
    fn a_single_short_literal_run_decodes() {
        // token 0x20 = run of 1, no match follows because input ends.
        let stream = [0x20u8, b'A'];
        assert_eq!(decompress(&stream, 16).unwrap(), b"A");
    }

    #[test]
    fn literal_runs_up_to_six_use_the_token_only() {
        let data = b"HELLO!";
        let stream = literal_only(data);
        assert_eq!(stream[0] >> 5, 6);
        assert_eq!(decompress(&stream, 16).unwrap(), data);
    }

    #[test]
    fn an_extended_literal_run_reads_further_bytes() {
        // run nibble 7 means extended; then 0x0A adds 10 -> 17 literals.
        let payload: Vec<u8> = (0u8..17).collect();
        let mut stream = vec![7u8 << 5, 0x0A];
        stream.extend_from_slice(&payload);
        assert_eq!(decompress(&stream, 64).unwrap(), payload);
    }

    #[test]
    fn a_255_chain_accumulates_length() {
        // 7<<5 token, then 0xFF (+255) then 0x02 (+2) -> 7 + 255 + 2 = 264
        let payload: Vec<u8> = (0..264).map(|i| (i % 251) as u8).collect();
        let mut stream = vec![7u8 << 5, 0xFF, 0x02];
        stream.extend_from_slice(&payload);
        assert_eq!(decompress(&stream, 512).unwrap(), payload);
    }

    // NOTE ON FIXTURES: one token carries BOTH the literal run (high three
    // bits) and the match length (low nibble). The two distance bytes follow
    // the literal bytes directly -- there is no second token.

    #[test]
    fn a_match_copies_earlier_output() {
        // token: run 4, match nibble 0 (len 4). Literals "ABCD", distance 4.
        let mut stream = vec![(4u8 << 5) | 0];
        stream.extend_from_slice(b"ABCD");
        stream.extend_from_slice(&[4, 0]);
        assert_eq!(decompress(&stream, 32).unwrap(), b"ABCDABCD");
    }

    #[test]
    fn overlapping_matches_repeat_the_pattern() {
        // token: run 2, match nibble 2 (len 6). "AB" + 6 bytes at distance 2.
        let mut stream = vec![(2u8 << 5) | 2];
        stream.extend_from_slice(b"AB");
        stream.extend_from_slice(&[2, 0]);
        assert_eq!(decompress(&stream, 32).unwrap(), b"ABABABAB");
    }

    #[test]
    fn the_token_bit_four_supplies_distance_bit_sixteen() {
        // Build 70000 bytes so a distance above 65535 is reachable.
        let big: Vec<u8> = (0..70_000).map(|i| (i % 253) as u8).collect();
        // Distance 65540 = 0x10004 -> token bit 0x10 set, low 16 bits 0x0004.
        let distance = 65_540usize;
        assert_eq!((distance >> 16) & 1, 1);

        // One token: extended literal run (7), distance bit 16 set (0x10),
        // match nibble 0 -> len 4.
        // 70000 - 7 = 69993 = 274*255 + 123
        let mut stream = vec![(7u8 << 5) | 0x10];
        for _ in 0..274 {
            stream.push(0xFF);
        }
        stream.push(123);
        stream.extend_from_slice(&big);
        stream.extend_from_slice(&[0x04, 0x00]);

        let out = decompress(&stream, 80_000).unwrap();
        assert_eq!(out.len(), big.len() + 4);
        assert_eq!(&out[big.len()..], &big[big.len() - distance..big.len() - distance + 4]);
    }

    #[test]
    fn a_distance_past_the_start_is_rejected() {
        // One literal, then a match reaching 100 bytes back.
        let mut stream = vec![(1u8 << 5) | 0, b'X'];
        stream.extend_from_slice(&[100, 0]);
        assert!(matches!(
            decompress(&stream, 32),
            Err(UlzError::BadDistance { distance: 100, .. })
        ));
    }

    #[test]
    fn a_zero_distance_is_rejected() {
        let mut stream = vec![(1u8 << 5) | 0, b'X'];
        stream.extend_from_slice(&[0, 0]);
        assert!(matches!(
            decompress(&stream, 32),
            Err(UlzError::BadDistance { distance: 0, .. })
        ));
    }

    #[test]
    fn a_truncated_stream_errors_rather_than_panicking() {
        // Literal run claims 6 bytes but only 2 follow.
        let stream = [6u8 << 5, b'A', b'B'];
        assert_eq!(decompress(&stream, 32), Err(UlzError::Truncated));

        // Match with only one of its two distance bytes present.
        let mut s2 = vec![(4u8 << 5) | 0];
        s2.extend_from_slice(b"ABCD");
        s2.push(0x04);
        assert_eq!(decompress(&s2, 32), Err(UlzError::Truncated));
    }

    #[test]
    fn output_larger_than_promised_is_rejected() {
        let mut stream = vec![6u8 << 5];
        stream.extend_from_slice(b"ABCDEF");
        assert!(matches!(
            decompress(&stream, 3),
            Err(UlzError::Overflow { .. })
        ));
    }

    #[test]
    fn an_empty_stream_yields_nothing() {
        assert_eq!(decompress(&[], 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_malformed_stream_never_panics() {
        // Fuzz-lite: every 3-byte stream must return, not crash.
        for a in 0u16..=255 {
            for b in 0u16..=255 {
                let _ = decompress(&[a as u8, b as u8, 0x41], 64);
            }
        }
    }
}
