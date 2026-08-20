//! Out-of-band ("connectionless") datagrams — the `\xff\xff\xff\xff` packets
//! that carry the handshake before a netchannel exists.
//!
//! Port of the connectionless half of `internal/client/client.go`. Verified
//! from `onConnectionless` (`0x1406EBAA0`):
//!
//! * the first payload byte is the message type; `0x41` (`A`) is the challenge
//!   reply and `0x42` (`B`) is connection-accepted (`cmp r8b, 0x41` / `0x42`)
//! * the challenge body is split on whitespace (`strings.Fields`), requires at
//!   least two fields (`cmp rbx, 2 / jl`), and the challenge itself is field
//!   **index 1** — the load is `[rax+0x10]`/`[rax+0x18]`, i.e. the second
//!   16-byte string header in the slice
//! * the request string is `getchallenge steam\n`, recovered verbatim from
//!   `Start` and `onRetry`
//!
//! Cross-checked against a live HLDS `1.1.2.7/Stdio` server, which replies
//! `A00000000 551254690 3 72057594037927936m 0\n` — field 1 is the challenge.

/// Every connectionless datagram starts with four `0xFF` bytes.
pub const HEADER: [u8; 4] = [0xFF; 4];

/// Server-to-client challenge reply.
pub const S2C_CHALLENGE: u8 = b'A';
/// Server-to-client "connection accepted".
pub const S2C_CONNECTION: u8 = b'B';

/// The exact request the original sends.
pub const GETCHALLENGE: &str = "getchallenge steam\n";

/// Build a connectionless datagram carrying `body`.
pub fn build(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&HEADER);
    out.extend_from_slice(body);
    out
}

/// Build the `getchallenge steam` request.
pub fn build_getchallenge() -> Vec<u8> {
    build(GETCHALLENGE.as_bytes())
}

/// Strip the connectionless header, returning the payload.
///
/// Returns `None` for datagrams that are not connectionless — those belong to
/// the netchannel instead.
pub fn payload(datagram: &[u8]) -> Option<&[u8]> {
    if datagram.len() > 4 && datagram[..4] == HEADER {
        Some(&datagram[4..])
    } else {
        None
    }
}

/// A parsed `A` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeReply {
    /// Field index 1 — the value echoed back in the `connect` packet.
    pub challenge: u32,
    /// Every whitespace-separated field after the leading `A`.
    pub fields: Vec<String>,
}

/// Parse an `A` challenge reply payload (header already stripped).
pub fn parse_challenge(payload: &[u8]) -> Option<ChallengeReply> {
    let first = *payload.first()?;
    if first != S2C_CHALLENGE {
        return None;
    }
    let body = String::from_utf8_lossy(&payload[1..]);
    let fields: Vec<String> = body.split_whitespace().map(str::to_owned).collect();
    if fields.len() < 2 {
        return None;
    }
    let challenge = fields[1].parse::<u32>().ok()?;
    Some(ChallengeReply { challenge, fields })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a live HLDS 1.1.2.7/Stdio server on 2026-08-01.
    const LIVE_CHALLENGE_REPLY: &[u8] =
        b"\xff\xff\xff\xffA00000000 551254690 3 72057594037927936m 0\n\0";

    #[test]
    fn getchallenge_matches_the_original_bytes() {
        let d = build_getchallenge();
        assert_eq!(&d[..4], &HEADER);
        assert_eq!(&d[4..], b"getchallenge steam\n");
    }

    #[test]
    fn live_reply_parses_to_field_one() {
        let p = payload(LIVE_CHALLENGE_REPLY).expect("connectionless header");
        assert_eq!(p[0], S2C_CHALLENGE);
        let reply = parse_challenge(p).expect("parses");
        assert_eq!(reply.challenge, 551_254_690);
        assert_eq!(reply.fields[0], "00000000");
        assert_eq!(reply.fields[1], "551254690");
    }

    #[test]
    fn non_connectionless_datagrams_are_rejected() {
        assert!(payload(&[0x01, 0x02, 0x03, 0x04, 0x05]).is_none());
        assert!(payload(&HEADER).is_none(), "header with no body is not a payload");
    }

    #[test]
    fn wrong_type_byte_is_not_a_challenge() {
        assert!(parse_challenge(b"B whatever here").is_none());
    }

    #[test]
    fn too_few_fields_is_rejected() {
        // `cmp rbx, 2 / jl` in the original.
        assert!(parse_challenge(b"A00000000").is_none());
    }
}
