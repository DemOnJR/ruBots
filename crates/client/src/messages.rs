//! Parsers for the byte-oriented signon messages.
//!
//! Port of `parseServerinfo` and friends from `internal/client/messages.go`.
//!
//! The `svc_serverinfo` layout below was decoded from a live HLDS signon
//! stream and every field cross-checks: protocol 48 (matching the server's
//! reported "Protocol version 48"), `maxplayers` 12 (matching the
//! `+maxplayers 12` the container is launched with), and `gamedir` `cstrike`.

/// A cursor over a byte-oriented message stream.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    pub fn i32(&mut self) -> Option<i32> {
        let b = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(i32::from_le_bytes(b.try_into().ok()?))
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.i32().map(|v| v as u32)
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let b = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(b)
    }

    /// NUL-terminated string.
    pub fn cstr(&mut self) -> Option<String> {
        let start = self.pos;
        let end = self.data[start..].iter().position(|b| *b == 0)? + start;
        self.pos = end + 1;
        Some(String::from_utf8_lossy(&self.data[start..end]).into_owned())
    }
}

/// Contents of `svc_serverinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub protocol: i32,
    pub spawn_count: u32,
    pub map_crc: u32,
    pub client_dll_md5: [u8; 16],
    pub max_players: u8,
    /// Our own player slot.
    pub player_index: u8,
    pub deathmatch: u8,
    pub game_dir: String,
    pub hostname: String,
    /// e.g. `maps/de_aztec.bsp`.
    pub map_path: String,
}

impl ServerInfo {
    /// Parse the body of `svc_serverinfo` (opcode already consumed).
    pub fn parse(r: &mut Reader<'_>) -> Option<Self> {
        let protocol = r.i32()?;
        let spawn_count = r.u32()?;
        let map_crc = r.u32()?;
        let mut md5 = [0u8; 16];
        md5.copy_from_slice(r.bytes(16)?);
        let max_players = r.u8()?;
        let player_index = r.u8()?;
        let deathmatch = r.u8()?;
        let game_dir = r.cstr()?;
        let hostname = r.cstr()?;
        let map_path = r.cstr()?;
        Some(Self {
            protocol,
            spawn_count,
            map_crc,
            client_dll_md5: md5,
            max_players,
            player_index,
            deathmatch,
            game_dir,
            hostname,
            map_path,
        })
    }

    /// Bare map name, e.g. `de_aztec` — what the `.bsp` and `.graph` lookups
    /// key on (`BSPForMap`, `FindGraphFile`).
    pub fn map_name(&self) -> &str {
        self.map_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.map_path)
            .strip_suffix(".bsp")
            .unwrap_or_else(|| self.map_path.rsplit('/').next().unwrap_or(&self.map_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `svc_serverinfo` captured from HLDS 1.1.2.7/Stdio on 2026-08-01,
    /// starting at the opcode byte.
    const LIVE_SERVERINFO: &[u8] = &[
        0x0B, // svc_serverinfo
        0x30, 0x00, 0x00, 0x00, // protocol 48
        0x05, 0x00, 0x00, 0x00, // spawn count 5
        0x81, 0x55, 0x99, 0x2B, // map crc
        // client dll md5 (zeroed on this server)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, //
        0x0C, // max players 12
        0x01, // our slot
        0x01, // deathmatch
        b'c', b's', b't', b'r', b'i', b'k', b'e', 0x00,
        b'C', b'o', b'u', b'n', b't', b'e', b'r', b'-', b'S', b't', b'r', b'i', b'k', b'e', b' ',
        b'1', b'.', b'6', b' ', b'S', b'e', b'r', b'v', b'e', b'r', 0x00,
        b'm', b'a', b'p', b's', b'/', b'd', b'e', b'_', b'a', b'z', b't', b'e', b'c', b'.', b'b',
        b's', b'p', 0x00,
    ];

    fn parse_live() -> ServerInfo {
        let mut r = Reader::new(LIVE_SERVERINFO);
        assert_eq!(r.u8(), Some(crate::svc::SVC_SERVERINFO));
        ServerInfo::parse(&mut r).expect("live serverinfo must parse")
    }

    #[test]
    fn live_serverinfo_fields_match_the_server() {
        let si = parse_live();
        // Cross-checks against what the server independently reports.
        assert_eq!(si.protocol, 48, "server logs 'Protocol version 48'");
        assert_eq!(si.max_players, 12, "container runs +maxplayers 12");
        assert_eq!(si.game_dir, "cstrike");
        assert_eq!(si.hostname, "Counter-Strike 1.6 Server");
        assert_eq!(si.map_path, "maps/de_aztec.bsp");
        assert_eq!(si.spawn_count, 5);
        assert_eq!(si.map_crc, 0x2B99_5581);
    }

    #[test]
    fn the_map_name_strips_path_and_extension() {
        assert_eq!(parse_live().map_name(), "de_aztec");
    }

    #[test]
    fn map_name_handles_a_bare_name() {
        let si = ServerInfo {
            map_path: "de_dust2".into(),
            ..parse_live()
        };
        assert_eq!(si.map_name(), "de_dust2");
    }

    #[test]
    fn a_truncated_serverinfo_returns_none_rather_than_panicking() {
        for cut in 1..LIVE_SERVERINFO.len() {
            let mut r = Reader::new(&LIVE_SERVERINFO[..cut]);
            r.u8();
            // Must not panic; may or may not parse depending on where we cut.
            let _ = ServerInfo::parse(&mut r);
        }
    }

    #[test]
    fn the_reader_refuses_to_run_past_the_end() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.u8(), Some(1));
        assert_eq!(r.i32(), None, "not enough bytes for an i32");
        assert_eq!(r.bytes(10), None);
        assert_eq!(r.remaining(), 2);
    }

    #[test]
    fn cstr_stops_at_the_terminator() {
        let mut r = Reader::new(b"hello\0world\0");
        assert_eq!(r.cstr().as_deref(), Some("hello"));
        assert_eq!(r.cstr().as_deref(), Some("world"));
        assert_eq!(r.cstr(), None, "no terminator left");
    }
}
