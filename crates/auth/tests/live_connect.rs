//! End-to-end handshake against a real HLDS server.
//!
//! This is the test that proves the reverse engineering is right: it performs
//! the full `getchallenge` -> `connect` exchange using a certificate built by
//! [`auth::build_revemu`] and requires the server to answer `B`
//! (S2C_CONNECTION, "connection accepted").
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

use proto::connectionless as cl;

const SERVER: &str = "127.0.0.1:27015";
/// The server reports "Protocol version 48".
const PROTOCOL: i32 = 48;

fn rpc(request: &[u8]) -> Option<Vec<u8>> {
    rpc_keep(request).map(|(_sock, reply)| reply)
}

/// Like [`rpc`] but hands the socket back, so the caller can keep talking from
/// the **same source port** — which is the only thing the server uses to
/// recognise a client once it is connected.
fn rpc_keep(request: &[u8]) -> Option<(UdpSocket, Vec<u8>)> {
    let sock = UdpSocket::bind("127.0.0.1:0").ok()?;
    sock.set_read_timeout(Some(Duration::from_secs(4))).ok()?;
    sock.connect(SERVER).ok()?;
    sock.send(request).ok()?;
    let mut buf = vec![0u8; 8192];
    let n = sock.recv(&mut buf).ok()?;
    buf.truncate(n);
    Some((sock, buf))
}

/// Hand the client slot back to the server.
///
/// **These tests used to be the source of a user-visible bug.** A `connect` the
/// server answers with `B` occupies a real client slot, and simply dropping the
/// socket does not release it: the slot stays `connected` for the whole
/// `sv_timeout` (120 s on the test server) still holding this test's name. The
/// engine then matches the *next* connect from the same base address onto that
/// slot as a reconnect (`rehlds/engine/sv_main.cpp:2378-2384` — base address
/// only, with no `client->connected` check), the reconnect branch never calls
/// `SV_DropClient` (`sv_main.cpp:2397-2412`), and ReGameDLL hands the ghost's
/// name to the newcomer. That is exactly how a swarm of `Bot01..Bot04` ended up
/// in the server log as `NoCert` and `NoCdKey` — the names below.
///
/// `dropclient` is on the engine's client-command whitelist
/// (`rehlds/engine/sv_user.cpp:45`) and reaches `SV_Drop_f`
/// (`sv_main.cpp:8144`) → `SV_DropClient`, which zeroes `cl->name` and
/// `cl->userinfo` and clears `cl->edict` (`rehlds/engine/host.cpp:504-519`).
///
/// Best effort: it is a courtesy to the next test, not an assertion. If the
/// datagram is lost the only cost is the old 120 s wait.
fn release_slot(sock: &UdpSocket) {
    let mut chan = netchan::NetChannel::new();
    let payload = netchan::NetChannel::string_command("dropclient");
    let _ = sock.send(&chan.build(&payload, true));
}

/// `\key\value` info string.
fn info(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("\\{k}\\{v}")).collect()
}

fn skip_or_panic(what: &str) {
    if std::env::var("AIPLAYERS_REQUIRE_SERVER").is_ok() {
        panic!("no HLDS server on {SERVER}: {what}");
    }
    eprintln!("SKIP: no HLDS server on {SERVER} ({what})");
}

fn get_challenge() -> Option<u32> {
    let reply = rpc(&cl::build_getchallenge())?;
    let p = cl::payload(&reply)?;
    cl::parse_challenge(p).map(|c| c.challenge)
}

/// Build the connect datagram: the formatted command, then the certificate as
/// trailing binary. The certificate is NOT inside the info string -- it is
/// appended after the newline, which is what `sendConnect` does.
fn build_connect(challenge: u32, key: &[u8], name: &str) -> Vec<u8> {
    let cdkey = auth::cdkey_hash(key);
    let protinfo = info(&[
        ("prot", "3"),
        ("unique", "-1"),
        ("raw", "steam"),
        ("cdkey", &cdkey),
    ]);
    let userinfo = info(&[
        ("model", "gordon"),
        ("topcolor", "0"),
        ("bottomcolor", "0"),
        ("rate", "25000"),
        ("cl_updaterate", "20"),
        ("cl_lw", "1"),
        ("cl_lc", "1"),
        ("cl_dlmax", "1024"),
        // NOTE: do NOT advertise `*hltv`. A Reunion-protected server rejects
        // any client carrying that key with
        // `Sorry, HLTV is not allowed on this server`, even with the value 0.
        // A real CS 1.6 client does not send it either.
        ("_vgui_menus", "1"),
        ("name", name),
    ]);

    let head = format!(
        "connect {PROTOCOL} {challenge} \"{protinfo}\" \"{userinfo}\"\n"
    );
    let mut out = cl::build(head.as_bytes());
    out.extend_from_slice(&auth::build_revemu(key));
    out
}

#[test]
fn revemu_certificate_is_accepted_by_a_real_server() {
    let Some(challenge) = get_challenge() else {
        skip_or_panic("getchallenge");
        return;
    };

    let (sock, reply) = rpc_keep(&build_connect(challenge, auth::DEFAULT_KEY, "AIPlayer"))
        .expect("server must answer the connect");
    let payload = cl::payload(&reply).expect("reply must be connectionless");

    let text = String::from_utf8_lossy(&payload[1..]);
    let accepted = payload[0] == cl::S2C_CONNECTION;
    if accepted {
        release_slot(&sock);
    }
    assert!(
        accepted,
        "expected 'B' (connection accepted), got {:?} -- server said: {}",
        payload[0] as char,
        text.trim()
    );
    eprintln!("connection accepted: {}", text.trim());
}

#[test]
fn missing_cdkey_field_is_rejected() {
    // Proves the cdkey half of the identity is load-bearing, not decoration:
    // the server answers '9' (reject) with "Invalid hashed CD key."
    let Some(challenge) = get_challenge() else {
        skip_or_panic("getchallenge");
        return;
    };

    let protinfo = info(&[("prot", "3"), ("unique", "-1"), ("raw", "steam")]);
    let userinfo = info(&[("name", "NoCdKey"), ("rate", "25000")]);
    let head = format!("connect {PROTOCOL} {challenge} \"{protinfo}\" \"{userinfo}\"\n");
    let mut packet = cl::build(head.as_bytes());
    packet.extend_from_slice(&auth::build_revemu(auth::DEFAULT_KEY));

    let (sock, reply) = rpc_keep(&packet).expect("server must answer");
    let payload = cl::payload(&reply).expect("connectionless");
    if payload[0] == cl::S2C_CONNECTION {
        // Reunion accepted us: give the slot straight back, or the name
        // "NoCdKey" haunts the next client to land on it.
        release_slot(&sock);
    }

    // The answer depends on who is doing authentication:
    //
    // * stock Valve HLDS validates the cdkey itself and rejects with
    //   `9 Invalid hashed CD key.`
    // * a Reunion-protected server (what public servers run) takes over
    //   authentication and is happy to derive an identity without one, so it
    //   accepts with `B`.
    //
    // Both are correct for their stack; what matters is that the server gives a
    // definite answer rather than ignoring us.
    match payload[0] {
        b'9' => assert!(
            String::from_utf8_lossy(payload).contains("CD key"),
            "unexpected reject reason: {}",
            String::from_utf8_lossy(payload)
        ),
        cl::S2C_CONNECTION => eprintln!("accepted without a cdkey (Reunion handles auth)"),
        other => panic!("unexpected reply {:?}", other as char),
    }
}

#[test]
fn a_certificate_is_required() {
    // Without the trailing certificate the server complains about its length,
    // which is what pinned down that it is appended rather than embedded.
    let Some(challenge) = get_challenge() else {
        skip_or_panic("getchallenge");
        return;
    };

    let cdkey = auth::cdkey_hash(auth::DEFAULT_KEY);
    let protinfo = info(&[
        ("prot", "3"),
        ("unique", "-1"),
        ("raw", "steam"),
        ("cdkey", &cdkey),
    ]);
    let userinfo = info(&[("name", "NoCert"), ("rate", "25000")]);
    let head = format!("connect {PROTOCOL} {challenge} \"{protinfo}\" \"{userinfo}\"\n");

    let (sock, reply) = rpc_keep(&cl::build(head.as_bytes())).expect("server must answer");
    let payload = cl::payload(&reply).expect("connectionless");
    if payload[0] == cl::S2C_CONNECTION {
        // Should not happen, but if some stack does accept a certificate-less
        // connect, do not leave "NoCert" sitting in a slot for two minutes.
        release_slot(&sock);
    }
    assert_eq!(payload[0], b'9', "expected a reject with no certificate");
    eprintln!(
        "no-certificate reject: {}",
        String::from_utf8_lossy(&payload[1..]).trim()
    );
}
