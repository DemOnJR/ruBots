//! A2S_INFO probe: ask the game server what it thinks is going on.
//!
//! The shell needs an answer to "is the server up, and does it see my bots?"
//! that does not depend on the bots themselves. A2S_INFO is the query every
//! GoldSrc/ReHLDS server answers on the game port, and the player count in
//! that reply is the server's own view of the swarm — the honest check that a
//! bot which is broadcasting telemetry is actually *connected*.
//!
//! Both reply shapes are handled: the modern Source reply (`I`) and the old
//! GoldSrc one (`m`), plus the `A` challenge that ReHLDS sends first when
//! `sv_enableoldqueries` is off.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

const HEADER: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
const A2S_INFO: &[u8] = b"\xFF\xFF\xFF\xFFTSource Engine Query\0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A2sInfo {
    pub name: String,
    pub map: String,
    pub folder: String,
    pub game: String,
    pub players: u8,
    pub max_players: u8,
    pub bots: u8,
    /// `true` when the reply was the old GoldSrc `m` form.
    pub goldsrc: bool,
}

/// Query a server, following one challenge if it asks for one.
pub fn query(addr: &str, timeout: Duration) -> Result<A2sInfo, String> {
    let target: SocketAddr = addr
        .to_socket_addrs()
        .map_err(|e| format!("bad address: {e}"))?
        .next()
        .ok_or_else(|| "address resolved to nothing".to_string())?;

    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    sock.set_read_timeout(Some(timeout))
        .map_err(|e| format!("timeout: {e}"))?;

    let mut request = A2S_INFO.to_vec();
    for attempt in 0..2 {
        sock.send_to(&request, target)
            .map_err(|e| format!("send: {e}"))?;
        let mut buf = [0u8; 2048];
        let (n, _) = sock
            .recv_from(&mut buf)
            .map_err(|e| format!("no reply: {e}"))?;
        let body = &buf[..n];
        if body.len() < 5 || body[0..4] != HEADER {
            return Err("reply was not an A2S packet".into());
        }
        match body[4] {
            b'A' if attempt == 0 && body.len() >= 9 => {
                request = A2S_INFO.to_vec();
                request.extend_from_slice(&body[5..9]);
                continue;
            }
            b'I' => return parse_source(&body[5..]),
            b'm' => return parse_goldsrc(&body[5..]),
            other => return Err(format!("unexpected reply type {:?}", other as char)),
        }
    }
    Err("server kept asking for a challenge".into())
}

/// Read a NUL-terminated string, advancing the cursor past the terminator.
fn cstr(b: &[u8], at: &mut usize) -> Result<String, String> {
    let start = *at;
    while *at < b.len() && b[*at] != 0 {
        *at += 1;
    }
    if *at >= b.len() {
        return Err("truncated reply".into());
    }
    let s = String::from_utf8_lossy(&b[start..*at]).into_owned();
    *at += 1;
    Ok(s)
}

fn take(b: &[u8], at: &mut usize) -> Result<u8, String> {
    let v = *b.get(*at).ok_or("truncated reply")?;
    *at += 1;
    Ok(v)
}

/// `I`: protocol, name, map, folder, game, appid(u16), players, max, bots, ...
fn parse_source(b: &[u8]) -> Result<A2sInfo, String> {
    let mut at = 0;
    let _protocol = take(b, &mut at)?;
    let name = cstr(b, &mut at)?;
    let map = cstr(b, &mut at)?;
    let folder = cstr(b, &mut at)?;
    let game = cstr(b, &mut at)?;
    at += 2; // appid
    let players = take(b, &mut at)?;
    let max_players = take(b, &mut at)?;
    let bots = take(b, &mut at)?;
    Ok(A2sInfo {
        name,
        map,
        folder,
        game,
        players,
        max_players,
        bots,
        goldsrc: false,
    })
}

/// `m`: address, name, map, folder, game, players, max, ...
fn parse_goldsrc(b: &[u8]) -> Result<A2sInfo, String> {
    let mut at = 0;
    let _address = cstr(b, &mut at)?;
    let name = cstr(b, &mut at)?;
    let map = cstr(b, &mut at)?;
    let folder = cstr(b, &mut at)?;
    let game = cstr(b, &mut at)?;
    let players = take(b, &mut at)?;
    let max_players = take(b, &mut at)?;
    Ok(A2sInfo {
        name,
        map,
        folder,
        game,
        players,
        max_players,
        bots: 0,
        goldsrc: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_reply_is_parsed() {
        let mut b = vec![48u8]; // protocol
        b.extend_from_slice(b"ruBots test\0de_dust2\0cstrike\0Counter-Strike\0");
        b.extend_from_slice(&10u16.to_le_bytes()); // appid
        b.extend_from_slice(&[12, 32, 0]);
        let info = parse_source(&b).expect("parse");
        assert_eq!(info.name, "ruBots test");
        assert_eq!(info.map, "de_dust2");
        assert_eq!((info.players, info.max_players), (12, 32));
        assert!(!info.goldsrc);
    }

    #[test]
    fn a_goldsrc_reply_is_parsed() {
        let mut b = Vec::new();
        b.extend_from_slice(b"127.0.0.1:27015\0old server\0de_inferno\0cstrike\0CS\0");
        b.extend_from_slice(&[3, 24]);
        let info = parse_goldsrc(&b).expect("parse");
        assert_eq!(info.map, "de_inferno");
        assert_eq!(info.players, 3);
        assert!(info.goldsrc);
    }

    #[test]
    fn a_truncated_reply_is_an_error_not_a_panic() {
        assert!(parse_source(&[48, b'x']).is_err());
        assert!(parse_goldsrc(b"127.0.0.1\0half").is_err());
    }
}
