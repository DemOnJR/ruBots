//! End-to-end signon against a real HLDS server.
//!
//! Proves the whole receive pipeline over the wire, not just against the
//! static `signon.bin` fixture: handshake → `clc_stringcmd "new"` → SPLIT
//! reassembly → netchannel unmunge → fragment reassembly → bzip2 → signon
//! walk. Success is the `usercmd_t` (and friends) delta tables being learned
//! from what the live server actually sent.
//!
//! Start the server first:
//!
//! ```text
//! cd testserver && docker compose up -d
//! ```
//!
//! Skips when nothing is listening; set `AIPLAYERS_REQUIRE_SERVER=1` to make a
//! missing server a failure.

use std::net::UdpSocket;
use std::time::Duration;

use client::{Identity, Session};

const SERVER: &str = "127.0.0.1:27015";

fn server_is_up() -> bool {
    let Ok(sock) = UdpSocket::bind("127.0.0.1:0") else {
        return false;
    };
    if sock.set_read_timeout(Some(Duration::from_secs(3))).is_err() {
        return false;
    }
    if sock.connect(SERVER).is_err() {
        return false;
    }
    // A getchallenge is the cheapest liveness probe.
    if sock.send(&proto::connectionless::build_getchallenge()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    sock.recv(&mut buf).is_ok()
}

fn skip_or_panic(what: &str) -> bool {
    if std::env::var("RUB_REQUIRE_SERVER").is_ok()
        || std::env::var("RUBOTS_REQUIRE_SERVER").is_ok()
        || std::env::var("REB_REQUIRE_SERVER").is_ok()
        || std::env::var("REBOTS_REQUIRE_SERVER").is_ok()
        || std::env::var("AIPLAYERS_REQUIRE_SERVER").is_ok()
    {
        panic!("no HLDS server on {SERVER}: {what}");
    }
    eprintln!("SKIP: no HLDS server on {SERVER} ({what})");
    false
}

#[test]
fn the_live_signon_is_reached_and_every_delta_table_learned() {
    if !server_is_up() {
        skip_or_panic("getchallenge");
        return;
    }

    let mut t = client::UdpTransport::connect(SERVER.parse().unwrap(), None)
        .expect("bind/connect udp");
    let mut session = Session::new(Identity { name: "ruBot".into(), ..Default::default() });

    match session.connect_and_signon(&mut t, Duration::from_secs(15)) {
        Ok(signon) => {
            // The seven tables the original references by literal.
            for name in [
                "event_t",
                "weapon_data_t",
                "usercmd_t",
                "custom_entity_state_t",
                "entity_state_player_t",
                "entity_state_t",
                "clientdata_t",
            ] {
                assert!(
                    signon.registry.get(name).is_some(),
                    "live signon missing delta table {name}"
                );
            }
            let map = signon
                .server_info
                .as_ref()
                .map(|si| si.map_name().to_string())
                .unwrap_or_default();
            eprintln!(
                "live signon reached: {} tables, map {}",
                signon.registry.len(),
                map
            );
        }
        Err(e) => panic!("did not reach the signon: {e}"),
    }

    // Having the usercmd_t table, the move sender must be constructable.
    assert!(
        session.move_sender().is_some(),
        "move sender should be available once running"
    );
}
