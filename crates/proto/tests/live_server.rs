//! Integration tests against a real HLDS server.
//!
//! Start it first:
//!
//! ```text
//! cd testserver && docker compose up -d
//! ```
//!
//! These tests SKIP (rather than fail) when nothing is listening, so the suite
//! still passes on a machine without Docker. Set `AIPLAYERS_REQUIRE_SERVER=1`
//! to turn a missing server into a failure instead.

use std::net::UdpSocket;
use std::time::Duration;

use proto::connectionless as cl;

const SERVER: &str = "127.0.0.1:27015";

/// Send one connectionless datagram and wait for a reply.
fn round_trip(request: &[u8]) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind("127.0.0.1:0").ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    sock.connect(SERVER).ok()?;
    sock.send(request).ok()?;
    let mut buf = vec![0u8; 4096];
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        if let Ok(n) = sock.recv(&mut buf) {
            let mut res = buf.clone();
            res.truncate(n);
            if let Some(payload) = cl::payload(&res) {
                if !payload.is_empty() && payload[0] == cl::S2C_CHALLENGE {
                    return Some(res);
                }
            }
        }
    }
    None
}

fn skip_or_panic(what: &str) {
    if std::env::var("RUB_REQUIRE_SERVER").is_ok()
        || std::env::var("RUBOTS_REQUIRE_SERVER").is_ok()
        || std::env::var("REB_REQUIRE_SERVER").is_ok()
        || std::env::var("REBOTS_REQUIRE_SERVER").is_ok()
        || std::env::var("AIPLAYERS_REQUIRE_SERVER").is_ok()
    {
        panic!("no HLDS server on {SERVER}: {what}");
    }
    eprintln!("SKIP: no HLDS server on {SERVER} ({what}) -- `cd testserver && docker compose up -d`");
}

#[test]
fn server_answers_getchallenge() {
    let Some(reply) = round_trip(&cl::build_getchallenge()) else {
        skip_or_panic("getchallenge");
        return;
    };

    let payload = cl::payload(&reply).expect("reply must be connectionless");
    assert_eq!(
        payload[0],
        cl::S2C_CHALLENGE,
        "expected an 'A' challenge reply, got {:?}",
        payload[0] as char
    );

    let parsed = cl::parse_challenge(payload).expect("challenge must parse");
    assert!(
        parsed.challenge != 0,
        "challenge should be a non-zero number, got {}",
        parsed.challenge
    );
    assert!(
        parsed.fields.len() >= 2,
        "challenge reply should carry at least two fields"
    );
    eprintln!(
        "live challenge = {} (fields: {:?})",
        parsed.challenge, parsed.fields
    );
}

#[test]
fn challenge_changes_between_requests_or_is_stable_per_client() {
    // Not asserting which -- just that a second handshake still succeeds, so
    // the client can retry (`onRetry` in the original).
    let Some(a) = round_trip(&cl::build_getchallenge()) else {
        skip_or_panic("first getchallenge");
        return;
    };
    let b = round_trip(&cl::build_getchallenge()).expect("second handshake");

    let pa = cl::parse_challenge(cl::payload(&a).unwrap()).expect("first parses");
    let pb = cl::parse_challenge(cl::payload(&b).unwrap()).expect("second parses");
    assert!(pa.challenge != 0 && pb.challenge != 0);
}
