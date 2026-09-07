//! One GoldSrc client session.
//!
//! Port of `internal/client/`. This drives the connection through the
//! handshake and signon, then pumps game state into a [`bot::WorldView`] and
//! the bot's commands back onto the wire.
//!
//! The state names are recovered literals from `State.String`
//! (`internal/client/client.go:30`): `disconnected`, `challenging`,
//! `connecting`, `connected`, `running`.

pub mod clock;
pub mod console;
pub mod demo;
pub mod content;
pub mod control;
pub mod map;
pub mod messages;
pub mod navigate;
pub mod replay;
pub mod role;
pub mod session;
pub mod signon;
pub mod stream;
pub mod svc;
pub mod telemetry;
pub mod usermsg;
pub mod view;
pub mod world;

pub use role::{BotRole, ObjectivePick};

pub use control::{intent_to_usercmd, MoveSender};
pub use messages::{Reader, ServerInfo};
pub use session::{Phase, Session};
pub use signon::{walk as walk_signon, Signon};
pub use stream::{collect_user_messages, walk as walk_stream, Item, UserMsgTable};

use std::fmt;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use proto::connectionless as cl;

/// The server reports "Protocol version 48".
pub const PROTOCOL: i32 = 48;

/// Connection lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    Challenging,
    Connecting,
    Connected,
    Running,
}

impl State {
    /// The literal `State.String` returns.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Disconnected => "disconnected",
            State::Challenging => "challenging",
            State::Connecting => "connecting",
            State::Connected => "connected",
            State::Running => "running",
        }
    }

    /// Has the handshake completed?
    pub fn is_in_game(self) -> bool {
        matches!(self, State::Connected | State::Running)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the bot's packets go, abstracted so the state machine can be driven
/// in tests without a socket.
pub trait Transport {
    fn send(&mut self, data: &[u8]) -> io::Result<()>;
    /// Non-blocking receive; `Ok(None)` means nothing was waiting.
    fn recv(&mut self) -> io::Result<Option<Vec<u8>>>;

    /// Block for up to `d` waiting for a datagram.
    ///
    /// This is what lets the send loop be paced by the clock instead of by a
    /// spin: wait until the next command is actually due, and wake early if the
    /// server says something in the meantime. The default is the non-blocking
    /// `recv` so in-memory test transports need not implement it.
    fn recv_timeout(&mut self, _d: Duration) -> io::Result<Option<Vec<u8>>> {
        self.recv()
    }
}

/// A real UDP transport.
pub struct UdpTransport {
    sock: UdpSocket,
}

impl UdpTransport {
    pub fn connect(server: SocketAddr, bind: Option<SocketAddr>) -> io::Result<Self> {
        let bind = bind.unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());
        let sock = UdpSocket::bind(bind)?;
        sock.connect(server)?;
        // A short default so callers that only ever use the non-blocking
        // `recv()` still make progress. The in-game loop should use
        // `recv_timeout` and let `MoveClock` decide how long to wait -- spinning
        // on a tiny timeout is what produced ~200 packets/s and got every
        // movement command discarded as a speedhack.
        sock.set_read_timeout(Some(POLL_TIMEOUT))?;
        Ok(Self { sock })
    }
}

/// Default socket read timeout for the non-blocking `recv()` path.
const POLL_TIMEOUT: Duration = Duration::from_millis(5);

impl Transport for UdpTransport {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        self.sock.send(data).map(|_| ())
    }

    fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 4096];
        match self.sock.recv(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e) if Self::is_empty_mailbox(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn recv_timeout(&mut self, d: Duration) -> io::Result<Option<Vec<u8>>> {
        // Windows treats a zero SO_RCVTIMEO as "block forever", and Rust
        // rejects Duration::ZERO outright, so clamp. The upper bound keeps a
        // wedged socket from parking the whole session.
        let d = d.clamp(Duration::from_millis(1), Duration::from_millis(250));
        self.sock.set_read_timeout(Some(d))?;
        let out = self.recv();
        self.sock.set_read_timeout(Some(POLL_TIMEOUT))?;
        out
    }
}

impl UdpTransport {
    /// Distinguishes "nothing arrived" from a genuine socket failure.
    ///
    /// Windows adds two cases a plain WouldBlock/TimedOut check misses, and
    /// both showed up in live runs as a spurious `pump error`:
    ///
    /// * **997, `ERROR_IO_PENDING`** — surfaced when the read timeout is very
    ///   short, which the old 5 ms spin hit routinely.
    /// * **10054, `WSAECONNRESET`** — on a *connected* UDP socket Windows
    ///   reports an ICMP port-unreachable from a previous send as an error on
    ///   the next receive. It says nothing about the current datagram.
    fn is_empty_mailbox(e: &io::Error) -> bool {
        matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::ConnectionReset
        ) || matches!(e.raw_os_error(), Some(997) | Some(10054) | Some(10035))
    }
}

/// The longest name the engine keeps.
///
/// `MAX_NAME` is 32 *including* the NUL (`rehlds/engine/server.h:35`) and every
/// copy the server makes is `Q_strncpy(dst, val, MAX_NAME - 1)`
/// (`sv_main.cpp:2076`, `sv_main.cpp:2235`, `sv_main.cpp:5328`), so 31 bytes is
/// all that survives. Two longer names sharing a 31-byte prefix arrive as the
/// *same* name and the second is then renamed `(1)…` by
/// `SV_CheckForDuplicateNames` (`sv_main.cpp:2060-2109`) — which is why the
/// clamp happens here, where the caller can still see it, rather than silently
/// on the server.
pub const MAX_NAME: usize = 31;

/// How the bot identifies itself.
#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub key: Vec<u8>,
    pub rate: u32,
    pub update_rate: u32,
}

impl Default for Identity {
    fn default() -> Self {
        Self {
            name: "ruBot".into(),
            key: auth::DEFAULT_KEY.to_vec(),
            // A real client negotiates a high rate; a low one makes the
            // server's `Netchan_CanPacket` rate limiter (cleartime) stall it
            // for long stretches while reliable data keeps queueing behind.
            // These directly govern how fast the server can DRAIN its reliable
            // buffer to us: it flushes at most one reliable message per update
            // interval (`next_messageinterval = 1.0 / cl_updaterate`,
            // ReHLDS `SV_ExtractFromUserinfo`). A low updaterate throttles that
            // drain, so a burst of reliable data — for example everything CS
            // queues on `jointeam` — overflows `netchan.message` and the client
            // is dropped with `Reliable channel overflowed`. Measured: at
            // `cl_updaterate 10` the server emitted reliables ~100 ms apart and
            // died ~1.8 s into the join burst.
            rate: 100_000,
            update_rate: 100,
        }
    }
}

impl Identity {
    /// `\key\value` info string.
    fn info(pairs: &[(&str, &str)]) -> String {
        pairs.iter().map(|(k, v)| format!("\\{k}\\{v}")).collect()
    }

    /// The name as the server will actually hold it: clamped to [`MAX_NAME`]
    /// on a character boundary.
    pub fn wire_name(&self) -> &str {
        let mut end = self.name.len().min(MAX_NAME);
        while end > 0 && !self.name.is_char_boundary(end) {
            end -= 1;
        }
        &self.name[..end]
    }

    /// `setinfo "name" "<name>"` — the `clc_stringcmd` a real client sends when
    /// a userinfo cvar changes (`rehlds/engine/cvar.cpp:267-268`, verbatim
    /// `setinfo "%s" "%s"`), and the only way to correct a name once the
    /// connect handshake is over.
    ///
    /// **This is not decoration — it is half the fix for ghost names.** The
    /// name in the `connect` userinfo can be *silently replaced* by the name of
    /// whoever last used the client slot, and re-asserting it after the spawn
    /// is what puts it back. Full chain, all verified:
    ///
    /// 1. Our processes never say goodbye, so a finished bot's slot stays
    ///    `connected` on the server for `sv_timeout` (120 s) with its `name`,
    ///    its edict and its `pvPrivateData` intact.
    /// 2. Every bot reaches the server from one address (the Docker gateway),
    ///    and ReHLDS matches a connect onto an existing slot by **base address
    ///    only** once that slot has been quiet for 10 s
    ///    (`sv_main.cpp:2378-2384`) — it never checks `client->connected`. So a
    ///    new bot is treated as the *reconnect* of the dead one.
    /// 3. The reconnect branch (`sv_main.cpp:2397-2412`) calls
    ///    `pfnClientDisconnect` but **not** `SV_DropClient`, so unlike a clean
    ///    drop (`host.cpp:504`, `cl->name[0] = 0`) the ghost's name survives.
    /// 4. `SV_ExtractFromUserinfo` hands the userinfo to the game DLL at
    ///    `sv_main.cpp:5325` and only copies the new name into `cl->name`
    ///    afterwards, at `sv_main.cpp:5328` — so during that call
    ///    `pev->netname` still reads the ghost's name.
    /// 5. ReGameDLL therefore sees a *name change*
    ///    (`regamedll/dlls/client.cpp:3756`), asks
    ///    `CBasePlayer::SetClientUserInfoName`, which refuses because the ghost
    ///    is dead (`regamedll/dlls/player.cpp:225-231`), and puts the old name
    ///    back into the buffer (`client.cpp:3773-3776`).
    /// 6. That reverted name is what `sv_main.cpp:5328` stores, what `status`
    ///    prints, and what `"<name>" entered the game`
    ///    (`multiplay_gamerules.cpp:3407`) reports.
    ///
    /// After our own spawn the edict is ours again (`SV_WriteSpawn` resets it,
    /// `sv_main.cpp:1404-1409`), so this `setinfo` is accepted — immediately if
    /// we are alive, and otherwise deferred by ReGameDLL to our next respawn
    /// (`player.cpp:227-228` stores it, `player.cpp:6087-6097` applies it).
    pub fn setinfo_name_command(&self) -> String {
        format!("setinfo \"name\" \"{}\"", self.wire_name())
    }

    /// The certificate-bearing half of the connect packet.
    pub fn protinfo(&self) -> String {
        let cdkey = auth::cdkey_hash(&self.key);
        Self::info(&[
            ("prot", "3"),
            ("unique", "-1"),
            ("raw", "steam"),
            ("cdkey", &cdkey),
        ])
    }

    /// The player-settings half.
    pub fn userinfo(&self) -> String {
        let rate = self.rate.to_string();
        let up = self.update_rate.to_string();
        // Field-for-field what a real CS 1.6 client sends, taken from a
        // captured `connect` packet. The leading-underscore keys are not
        // decoration:
        //
        // * `_vgui_menus 1` tells the game DLL the client can render VGUI
        //   menus. Without it CS falls back to **text** `ShowMenu` blobs, which
        //   are far bigger than the compact `VGUIMenu` message and are re-sent
        //   whenever the menu is shown — a real source of reliable traffic for
        //   a client sitting in the team menu.
        // * `_cl_autowepswitch`, `_ah` (auto-help) and `_demorecorder` are the
        //   other keys a stock client advertises.
        Self::info(&[
            ("_cl_autowepswitch", "1"),
            ("bottomcolor", "6"),
            ("cl_dlmax", "1024"),
            ("cl_lc", "1"),
            ("cl_lw", "1"),
            ("cl_updaterate", &up),
            ("model", "gordon"),
            // Clamped here rather than left to the server: see [`MAX_NAME`].
            ("name", self.wire_name()),
            ("topcolor", "30"),
            ("_vgui_menus", "1"),
            ("_ah", "1"),
            ("_demorecorder", "1"),
            ("rate", &rate),
        ])
    }

    /// Full `connect` datagram: the formatted command, then the certificate as
    /// trailing binary (see `auth` — it is not inside the info string).
    pub fn connect_packet(&self, challenge: u32) -> Vec<u8> {
        let head = format!(
            "connect {PROTOCOL} {challenge} \"{}\" \"{}\"\n",
            self.protinfo(),
            self.userinfo()
        );
        let mut out = cl::build(head.as_bytes());
        out.extend_from_slice(&auth::build_revemu(&self.key));
        out
    }
}

/// Why a connection ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disconnect {
    /// The server sent `9` with a reason.
    Rejected(String),
    Timeout,
    Closed,
}

impl fmt::Display for Disconnect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(why) => write!(f, "rejected: {why}"),
            Self::Timeout => write!(f, "connect timeout"),
            Self::Closed => write!(f, "connection closed"),
        }
    }
}

/// The connection state machine.
pub struct Client {
    state: State,
    pub(crate) identity: Identity,
    challenge: Option<u32>,
    userid: Option<i32>,
    last_send: Option<Instant>,
    retries: u32,
}

/// How long to wait before re-sending a handshake step (`onRetry`).
pub const RETRY_INTERVAL: Duration = Duration::from_millis(1500);
/// Give up after this many retries.
pub const MAX_RETRIES: u32 = 6;

impl Client {
    /// The `clc_stringcmd` that ends the session cleanly.
    ///
    /// `dropclient` is on the engine's client-command whitelist
    /// (`rehlds/engine/sv_user.cpp:45`, checked by `SV_ValidateClientCommand`)
    /// and reaches `SV_Drop_f` (`sv_main.cpp:8144-8155`), which calls
    /// `SV_DropClient`. That is what actually frees the slot: `cl->name[0] = 0`,
    /// `cl->edict = NULL` and a zeroed `cl->userinfo`
    /// (`rehlds/engine/host.cpp:504-519`).
    ///
    /// Without it the slot stays `connected` for the whole `sv_timeout`, and
    /// the next bot to connect from the same address inherits its name — see
    /// [`Identity::setinfo_name_command`] for the full chain. Sending it is the
    /// half of the fix that prevents the problem; the `setinfo` is the half
    /// that repairs it when a bot was killed and never got to send this.
    pub const DISCONNECT_COMMAND: &'static str = "dropclient";

    pub fn new(identity: Identity) -> Self {
        Self {
            state: State::Disconnected,
            identity,
            challenge: None,
            userid: None,
            last_send: None,
            retries: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn userid(&self) -> Option<i32> {
        self.userid
    }

    pub fn challenge(&self) -> Option<u32> {
        self.challenge
    }

    /// Begin the handshake.
    pub fn start<T: Transport>(&mut self, t: &mut T) -> io::Result<()> {
        self.state = State::Challenging;
        self.retries = 0;
        self.send_getchallenge(t)
    }

    fn send_getchallenge<T: Transport>(&mut self, t: &mut T) -> io::Result<()> {
        t.send(&cl::build_getchallenge())?;
        self.last_send = Some(Instant::now());
        Ok(())
    }

    fn send_connect<T: Transport>(&mut self, t: &mut T, challenge: u32) -> io::Result<()> {
        t.send(&self.identity.connect_packet(challenge))?;
        self.last_send = Some(Instant::now());
        Ok(())
    }

    /// Feed one received datagram into the state machine.
    pub fn handle_datagram(&mut self, data: &[u8]) -> Result<(), Disconnect> {
        let Some(payload) = cl::payload(data) else {
            // A sequenced packet: only meaningful once in game.
            return Ok(());
        };
        match payload[0] {
            cl::S2C_CHALLENGE => {
                if let Some(reply) = cl::parse_challenge(payload) {
                    self.challenge = Some(reply.challenge);
                    if self.state == State::Challenging {
                        self.state = State::Connecting;
                        self.retries = 0;
                    }
                }
                Ok(())
            }
            cl::S2C_CONNECTION => {
                // "B <userid> "<addr>" <n> <build>"
                let text = String::from_utf8_lossy(&payload[1..]);
                self.userid = text
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<i32>().ok());
                self.state = State::Connected;
                self.retries = 0;
                Ok(())
            }
            b'9' => {
                let why = String::from_utf8_lossy(&payload[1..])
                    .trim_end_matches(['\n', '\0'])
                    .to_string();
                self.state = State::Disconnected;
                Err(Disconnect::Rejected(why))
            }
            _ => Ok(()),
        }
    }

    /// Re-send the current handshake step if it has gone unanswered.
    pub fn tick<T: Transport>(&mut self, t: &mut T) -> Result<(), Disconnect> {
        if self.state.is_in_game() || self.state == State::Disconnected {
            return Ok(());
        }
        let due = self
            .last_send
            .map_or(true, |t0| t0.elapsed() >= RETRY_INTERVAL);
        if !due {
            return Ok(());
        }
        if self.retries >= MAX_RETRIES {
            self.state = State::Disconnected;
            return Err(Disconnect::Timeout);
        }
        self.retries += 1;

        let r = match self.state {
            State::Challenging => self.send_getchallenge(t),
            State::Connecting => {
                let c = self.challenge.unwrap_or(0);
                self.send_connect(t, c)
            }
            _ => Ok(()),
        };
        r.map_err(|_| Disconnect::Closed)
    }

    /// Drive the handshake to completion (or failure).
    pub fn run_handshake<T: Transport>(
        &mut self,
        t: &mut T,
        timeout: Duration,
    ) -> Result<(), Disconnect> {
        let deadline = Instant::now() + timeout;
        self.start(t).map_err(|_| Disconnect::Closed)?;

        while Instant::now() < deadline {
            match t.recv() {
                Ok(Some(d)) => self.handle_datagram(&d)?,
                Ok(None) => {}
                Err(_) => return Err(Disconnect::Closed),
            }
            if self.state.is_in_game() {
                return Ok(());
            }
            // Once we have the challenge, send the connect immediately rather
            // than waiting out the retry interval.
            if self.state == State::Connecting && self.retries == 0 {
                let c = self.challenge.unwrap_or(0);
                self.send_connect(t, c).map_err(|_| Disconnect::Closed)?;
                self.retries = 1;
            }
            self.tick(t)?;
        }
        Err(Disconnect::Timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted transport: replays canned replies and records what was sent.
    #[derive(Default)]
    struct Fake {
        sent: Vec<Vec<u8>>,
        inbox: Vec<Vec<u8>>,
    }

    impl Transport for Fake {
        fn send(&mut self, data: &[u8]) -> io::Result<()> {
            self.sent.push(data.to_vec());
            Ok(())
        }
        fn recv(&mut self) -> io::Result<Option<Vec<u8>>> {
            Ok(if self.inbox.is_empty() {
                None
            } else {
                Some(self.inbox.remove(0))
            })
        }
    }

    fn challenge_reply(n: u32) -> Vec<u8> {
        cl::build(format!("A00000000 {n} 3 72057594037927936m 0\n\0").as_bytes())
    }

    fn accept_reply(userid: i32) -> Vec<u8> {
        cl::build(format!("B {userid} \"127.0.0.1:1234\" 0 10211\0").as_bytes())
    }

    #[test]
    fn state_names_match_the_recovered_literals() {
        assert_eq!(State::Disconnected.as_str(), "disconnected");
        assert_eq!(State::Challenging.as_str(), "challenging");
        assert_eq!(State::Connecting.as_str(), "connecting");
        assert_eq!(State::Connected.as_str(), "connected");
        assert_eq!(State::Running.as_str(), "running");
    }

    #[test]
    fn a_fresh_client_is_disconnected() {
        let c = Client::new(Identity::default());
        assert_eq!(c.state(), State::Disconnected);
        assert!(!c.state().is_in_game());
    }

    #[test]
    fn start_sends_getchallenge_and_enters_challenging() {
        let mut c = Client::new(Identity::default());
        let mut t = Fake::default();
        c.start(&mut t).unwrap();
        assert_eq!(c.state(), State::Challenging);
        assert_eq!(t.sent.len(), 1);
        assert_eq!(cl::payload(&t.sent[0]).unwrap(), b"getchallenge steam\n");
    }

    #[test]
    fn a_challenge_reply_advances_to_connecting() {
        let mut c = Client::new(Identity::default());
        let mut t = Fake::default();
        c.start(&mut t).unwrap();
        c.handle_datagram(&challenge_reply(551_254_690)).unwrap();
        assert_eq!(c.state(), State::Connecting);
        assert_eq!(c.challenge(), Some(551_254_690));
    }

    #[test]
    fn an_acceptance_captures_the_userid_and_enters_connected() {
        let mut c = Client::new(Identity::default());
        c.handle_datagram(&accept_reply(7)).unwrap();
        assert_eq!(c.state(), State::Connected);
        assert_eq!(c.userid(), Some(7));
        assert!(c.state().is_in_game());
    }

    #[test]
    fn a_rejection_surfaces_the_server_reason() {
        let mut c = Client::new(Identity::default());
        let packet = cl::build(b"9Invalid hashed CD key.\n\0");
        let err = c.handle_datagram(&packet).unwrap_err();
        assert_eq!(err, Disconnect::Rejected("Invalid hashed CD key.".into()));
        assert_eq!(c.state(), State::Disconnected);
    }

    #[test]
    fn the_full_handshake_runs_against_a_scripted_server() {
        let mut c = Client::new(Identity::default());
        let mut t = Fake {
            sent: Vec::new(),
            inbox: vec![challenge_reply(4242), accept_reply(3)],
        };
        c.run_handshake(&mut t, Duration::from_secs(2)).unwrap();
        assert_eq!(c.state(), State::Connected);
        assert_eq!(c.userid(), Some(3));

        // getchallenge, then connect.
        assert_eq!(t.sent.len(), 2);
        let connect = String::from_utf8_lossy(&t.sent[1]);
        assert!(connect.contains("connect 48 4242"), "got: {connect}");
    }

    #[test]
    fn the_connect_packet_carries_the_certificate_as_trailing_binary() {
        let id = Identity::default();
        let p = id.connect_packet(1234);
        assert_eq!(&p[..4], &cl::HEADER);
        // The last 152 bytes are the certificate.
        let cert = auth::build_revemu(&id.key);
        assert_eq!(&p[p.len() - cert.len()..], &cert[..]);
        // And it sits after the newline that ends the command.
        let head_end = p.len() - cert.len();
        assert_eq!(p[head_end - 1], b'\n');
    }

    #[test]
    fn a_handshake_with_no_replies_times_out() {
        let mut c = Client::new(Identity::default());
        let mut t = Fake::default();
        let err = c
            .run_handshake(&mut t, Duration::from_millis(120))
            .unwrap_err();
        assert_eq!(err, Disconnect::Timeout);
    }

    #[test]
    fn sequenced_packets_are_ignored_during_the_handshake() {
        let mut c = Client::new(Identity::default());
        // Not connectionless: a netchannel packet.
        let seq = [0x01u8, 0, 0, 0, 0, 0, 0, 0, 0xAA];
        assert!(c.handle_datagram(&seq).is_ok());
        assert_eq!(c.state(), State::Disconnected);
    }

    #[test]
    fn userinfo_carries_the_configured_name_and_rate() {
        let id = Identity {
            name: "Bravo".into(),
            rate: 30_000,
            ..Default::default()
        };
        let u = id.userinfo();
        assert!(u.contains("\\name\\Bravo"), "{u}");
        assert!(u.contains("\\rate\\30000"), "{u}");
    }

    /// The headline guarantee: launch N bots with N names and the server is
    /// asked for exactly those N names, once each.
    ///
    /// This is the regression guard for a swarm that connected as `Bot01..Bot04`
    /// and appeared in the server log as `NoCert`, `NoCdKey`, `(1)AIPlayer` and
    /// `Bot04` — three of four wearing the names of *earlier* clients whose
    /// slots they were given. The half of that bug the connect packet can be
    /// blamed for is this one: every bot must put its own name in its own
    /// userinfo. (The other half is repaired after the spawn — see
    /// [`Identity::setinfo_name_command`] — and prevented by
    /// [`Client::DISCONNECT_COMMAND`].)
    #[test]
    fn n_bots_ask_for_n_distinct_names() {
        let names: Vec<String> = (1..=8).map(|i| format!("Bot{i:02}")).collect();
        let mut seen = std::collections::BTreeSet::new();
        for (i, name) in names.iter().enumerate() {
            let id = Identity {
                name: name.clone(),
                key: format!("AIPLAYERBOT{:04}", i + 1).into_bytes(),
                ..Default::default()
            };
            let u = id.userinfo();
            assert!(
                u.contains(&format!("\\name\\{name}")),
                "bot {i} asked for the wrong name: {u}"
            );
            // No other bot's name may appear anywhere in this userinfo, so a
            // stale default cannot hide in it.
            for other in &names {
                if other != name {
                    assert!(!u.contains(&format!("\\name\\{other}")), "{u}");
                }
            }
            assert!(
                seen.insert(id.wire_name().to_string()),
                "duplicate name {name}"
            );
        }
        assert_eq!(seen.len(), names.len());
        // And the default is only ever used when nobody asked for a name --
        // "(1)ruBot" in a server log means a bot fell back to it.
        assert_eq!(Identity::default().name, "ruBot");
    }

    /// `MAX_NAME` is 31 usable bytes (`rehlds/engine/server.h:35`), and the
    /// clamp happens here so two long names cannot silently collapse into one
    /// on the server and come back deduplicated as `(1)…`.
    #[test]
    fn names_are_clamped_to_the_engines_max_name() {
        let id = Identity {
            name: "N".repeat(64),
            ..Default::default()
        };
        assert_eq!(id.wire_name().len(), MAX_NAME);
        assert!(id
            .userinfo()
            .contains(&format!("\\name\\{}", "N".repeat(MAX_NAME))));
        // A short name is untouched.
        let short = Identity {
            name: "Bot01".into(),
            ..Default::default()
        };
        assert_eq!(short.wire_name(), "Bot01");
        // Multi-byte names are cut on a character boundary, never mid-codepoint.
        let wide = Identity {
            name: "ä".repeat(20),
            ..Default::default()
        };
        assert!(wide.wire_name().len() <= MAX_NAME);
        assert!(std::str::from_utf8(wide.wire_name().as_bytes()).is_ok());
    }

    /// The exact wire form the engine writes for a userinfo change:
    /// `setinfo "%s" "%s"` (`rehlds/engine/cvar.cpp:268`). The quotes matter —
    /// `Host_SetInfo_f` requires `Cmd_Argc() == 3` (`host_cmd.cpp:2584`), so an
    /// unquoted name containing a space would be silently ignored.
    #[test]
    fn setinfo_name_command_matches_the_engine_wire_form() {
        let id = Identity {
            name: "Bot01".into(),
            ..Default::default()
        };
        assert_eq!(id.setinfo_name_command(), "setinfo \"name\" \"Bot01\"");
        let spaced = Identity {
            name: "Bot 01".into(),
            ..Default::default()
        };
        assert_eq!(spaced.setinfo_name_command(), "setinfo \"name\" \"Bot 01\"");
        // It re-asserts the clamped name, so it agrees with the connect
        // userinfo rather than fighting it.
        let long = Identity {
            name: "N".repeat(64),
            ..Default::default()
        };
        assert!(long.setinfo_name_command().contains(&"N".repeat(MAX_NAME)));
        assert!(!long
            .setinfo_name_command()
            .contains(&"N".repeat(MAX_NAME + 1)));
    }

    /// `dropclient` is the one disconnect verb the engine accepts from a
    /// client: it is on `clcommands` (`rehlds/engine/sv_user.cpp:45`, gated by
    /// `SV_ValidateClientCommand`) and is the only name registered for
    /// `SV_Drop_f` (`sv_main.cpp:8260`). "drop" and "disconnect" are not.
    #[test]
    fn the_disconnect_command_is_the_one_the_engine_whitelists() {
        assert_eq!(Client::DISCONNECT_COMMAND, "dropclient");
    }
}
