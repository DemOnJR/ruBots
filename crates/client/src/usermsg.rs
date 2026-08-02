//! Decoding the game DLL's **user messages** — the layer where Counter-Strike,
//! as opposed to the GoldSrc engine, actually says what is happening.
//!
//! [`crate::stream`] walks the running-phase byte stream and hands out
//! `Item::User { name, payload, .. }`. Everything here takes that `name` and
//! that `payload`.
//!
//! ## Why the name and not the id
//!
//! Ids are **assigned dynamically, in registration order**, by
//! `REG_USER_MSG` in `LinkUserMessages` (`regamedll/dlls/client.cpp:143-231`,
//! plus `VoiceMask`/`ReqState` from `game_shared/voice_gamemgr.cpp:64-65`).
//! A metamod plugin that registers one message ahead of the game DLL shifts
//! every id after it. The **names** are stable; the ids are not. So the whole
//! module keys on `&str`, and [`parse`] returns `None` for any name we have not
//! taught it — the server registers roughly ninety of these and we care about
//! two dozen.
//!
//! ## Encoding primitives
//!
//! From the engine's message writers (`rehlds/engine/pr_cmds.cpp`) and the game
//! DLL's `WRITE_*` macros:
//!
//! | Macro | Wire |
//! |---|---|
//! | `WRITE_BYTE` / `WRITE_CHAR` | 1 byte (char is signed) |
//! | `WRITE_SHORT` | 2 bytes, little-endian, signed |
//! | `WRITE_LONG` | 4 bytes, little-endian, signed |
//! | `WRITE_STRING` | bytes then a NUL |
//! | `WRITE_COORD` | **`i16` little-endian, the value times 8** |
//!
//! `PF_WriteCoord_I` (`rehlds/engine/pr_cmds.cpp:2394-2399`) is literally
//! `MSG_WriteShort(&gMsgBuffer, (int)(flValue * 8.0))`, so decoding is
//! `i16 as f32 / 8.0` — and it must go through `i16`, not `u16`. Half of
//! de_dust2 sits at negative x and y; reading a coord unsigned puts a hostage
//! at +8000 instead of -184 and nothing ever reports an error.
//!
//! ## Bounds safety
//!
//! Every parser is total: a short, truncated or otherwise malformed payload
//! yields `None`. Nothing here can panic on hostile input, which matters
//! because this is the one place where bytes chosen by a remote server become
//! numbers we act on.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// cursor
// ---------------------------------------------------------------------------

/// A bounds-checked cursor over one user-message payload.
///
/// Deliberately separate from [`crate::messages::Reader`]: that one serves the
/// signon parsers and has no notion of a `COORD`, and user messages need the
/// signed 16-bit reads that `WRITE_SHORT` and `WRITE_COORD` produce.
struct Cur<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    /// `WRITE_CHAR` — a *signed* byte.
    fn i8(&mut self) -> Option<i8> {
        self.u8().map(|v| v as i8)
    }

    /// `WRITE_SHORT` — 2 bytes little-endian, signed.
    fn i16(&mut self) -> Option<i16> {
        let b = self.data.get(self.pos..self.pos.checked_add(2)?)?;
        self.pos += 2;
        Some(i16::from_le_bytes([b[0], b[1]]))
    }

    /// `WRITE_LONG` — 4 bytes little-endian, signed.
    fn i32(&mut self) -> Option<i32> {
        let b = self.data.get(self.pos..self.pos.checked_add(4)?)?;
        self.pos += 4;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// `WRITE_COORD` — `i16` little-endian, scaled by 8.
    fn coord(&mut self) -> Option<f32> {
        self.i16().map(|v| f32::from(v) / 8.0)
    }

    fn vec3(&mut self) -> Option<[f32; 3]> {
        Some([self.coord()?, self.coord()?, self.coord()?])
    }

    /// `WRITE_STRING` — NUL-terminated. Missing terminator is a decode failure.
    ///
    /// Decoded lossily: CS strings are whatever bytes the server put there
    /// (map names, player names, localisation tokens) and are not guaranteed
    /// UTF-8.
    fn cstr(&mut self) -> Option<String> {
        let rest = self.data.get(self.pos..)?;
        let n = rest.iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&rest[..n]).into_owned();
        self.pos += n + 1;
        Some(s)
    }
}

/// Encode a float the way `WRITE_COORD` does. Useful to callers building
/// fixtures, and used by this module's own tests.
pub fn encode_coord(v: f32) -> [u8; 2] {
    ((v * 8.0) as i16).to_le_bytes()
}

// ---------------------------------------------------------------------------
// shared enums
// ---------------------------------------------------------------------------

/// Highest client slot the engine will use (`common/const.h:36`,
/// `MAX_CLIENTS 32`). Slots are 1-based; `entindex()` 0 is the worldspawn.
pub const MAX_CLIENTS: usize = 32;

/// Which side a player is on.
///
/// The discriminants match the game's `enum TeamName`
/// (`regamedll/dlls/player.h:202-208`): `UNASSIGNED`, `TERRORIST`, `CT`,
/// `SPECTATOR` — which is the order `ScoreInfo` sends as its team short.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Team {
    #[default]
    Unassigned = 0,
    Terrorist = 1,
    CounterTerrorist = 2,
    Spectator = 3,
}

impl Team {
    /// From the literal `TeamInfo` sends.
    ///
    /// The strings come from `GetTeamName` (`regamedll/dlls/client.h:203-211`,
    /// duplicated at `dlls/multiplay_gamerules.cpp:197-202`), which returns
    /// exactly `"CT"`, `"TERRORIST"`, `"SPECTATOR"` or `"UNASSIGNED"`.
    /// Anything else — a mod with its own team names — is
    /// [`Team::Unassigned`] rather than an error, because a strange team
    /// string is no reason to throw away the client index that came with it.
    pub fn from_name(name: &str) -> Team {
        if name.eq_ignore_ascii_case("CT") {
            Team::CounterTerrorist
        } else if name.eq_ignore_ascii_case("TERRORIST") {
            Team::Terrorist
        } else if name.eq_ignore_ascii_case("SPECTATOR") {
            Team::Spectator
        } else {
            Team::Unassigned
        }
    }

    /// From the numeric `TeamName` the `ScoreInfo` team short carries.
    pub fn from_id(id: i16) -> Team {
        match id {
            1 => Team::Terrorist,
            2 => Team::CounterTerrorist,
            3 => Team::Spectator,
            _ => Team::Unassigned,
        }
    }

    /// The literal the server would send for this team.
    pub fn as_name(self) -> &'static str {
        match self {
            Team::Unassigned => "UNASSIGNED",
            Team::Terrorist => "TERRORIST",
            Team::CounterTerrorist => "CT",
            Team::Spectator => "SPECTATOR",
        }
    }

    /// Is this a side that plays the round (as opposed to watching it)?
    pub fn is_playing(self) -> bool {
        matches!(self, Team::Terrorist | Team::CounterTerrorist)
    }

    /// The other playing side, if there is one.
    pub fn opposite(self) -> Option<Team> {
        match self {
            Team::Terrorist => Some(Team::CounterTerrorist),
            Team::CounterTerrorist => Some(Team::Terrorist),
            _ => None,
        }
    }
}

/// `StatusIcon`'s leading byte (`regamedll/dlls/cdll_dll.h:47-49`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconStatus {
    Hide = 0,
    Show = 1,
    Flash = 2,
}

impl IconStatus {
    fn from_u8(v: u8) -> Option<IconStatus> {
        match v {
            0 => Some(IconStatus::Hide),
            1 => Some(IconStatus::Show),
            2 => Some(IconStatus::Flash),
            _ => None,
        }
    }

    /// Is the icon on the HUD at all? Flashing counts.
    pub fn is_visible(self) -> bool {
        !matches!(self, IconStatus::Hide)
    }
}

/// `TextMsg`'s destination (`regamedll/dlls/cdll_dll.h:51-55`).
///
/// Kept open-ended: mods do invent their own destinations, and an unknown one
/// is not a reason to lose the message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextDest {
    /// `HUD_PRINTNOTIFY` 1
    Notify,
    /// `HUD_PRINTCONSOLE` 2
    Console,
    /// `HUD_PRINTTALK` 3
    Talk,
    /// `HUD_PRINTCENTER` 4
    Center,
    /// `HUD_PRINTRADIO` 5
    Radio,
    Other(u8),
}

impl TextDest {
    fn from_u8(v: u8) -> TextDest {
        match v {
            1 => TextDest::Notify,
            2 => TextDest::Console,
            3 => TextDest::Talk,
            4 => TextDest::Center,
            5 => TextDest::Radio,
            other => TextDest::Other(other),
        }
    }
}

/// Whether a `HostagePos` is the initial placement or a movement update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostageUpdate {
    /// Type 0 — the hostage moved (`dlls/hostage/hostage.cpp:1262-1268`).
    Move,
    /// Type 1 — initial position on spawn (`dlls/player.cpp:7480-7486`).
    Init,
}

impl HostageUpdate {
    fn from_u8(v: u8) -> Option<HostageUpdate> {
        match v {
            0 => Some(HostageUpdate::Move),
            1 => Some(HostageUpdate::Init),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// per-message payloads
// ---------------------------------------------------------------------------

/// `CurWeapon` — the active weapon and its clip.
///
/// Emitted by `CBasePlayerWeapon::UpdateClientData`
/// (`regamedll/dlls/weapons.cpp:1380-1384`); also by the observer path
/// (`dlls/observer.cpp:349-353`), and as an all-zero clear on death
/// (`dlls/player.cpp:1922-1926`).
///
/// The first byte is **not a boolean**, despite reading like one: it is
/// `state`, which is `0` when this is not the active item, `1` when it is, and
/// `WEAPON_IS_ONTARGET` = `0x40` (`dlls/weapons.h:49`) when it is active *and*
/// the crosshair is on an enemy (`weapons.cpp:1356-1364`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurWeapon {
    /// Raw `state` byte.
    pub state: u8,
    /// `WeaponIdType` — `weapon_usp` 18, `weapon_ak47` 28, and so on.
    pub weapon_id: u8,
    /// Rounds in the clip. `255` is the game's `-1`, i.e. a weapon with no
    /// clip (knife, grenades) — see [`CurWeapon::clip_count`].
    pub clip: u8,
}

impl CurWeapon {
    /// Is this the weapon we are holding?
    pub fn is_active(&self) -> bool {
        self.state != 0
    }

    /// Is the crosshair on an enemy? (`WEAPON_IS_ONTARGET`)
    pub fn on_target(&self) -> bool {
        self.state & 0x40 != 0
    }

    /// Clip contents, or `None` for a weapon that has no clip.
    pub fn clip_count(&self) -> Option<u8> {
        if self.clip == 255 {
            None
        } else {
            Some(self.clip)
        }
    }
}

/// `WeaponList` — one entry of the weapon table, sent at `MSG_INIT`
/// (`regamedll/dlls/client.cpp:256-266`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeaponList {
    /// e.g. `weapon_ak47`.
    pub name: String,
    pub ammo1_index: u8,
    pub max_ammo1: u8,
    pub ammo2_index: u8,
    pub max_ammo2: u8,
    pub slot: u8,
    pub position: u8,
    pub weapon_id: u8,
    pub flags: u8,
}

/// `Money` — our account, and whether the HUD should blink it.
///
/// `CBasePlayer::AddAccount` (`regamedll/dlls/player.cpp:3568-3571`, and the
/// non-`REGAMEDLL_ADD` branch at `player.cpp:3586-3589`) writes a `LONG`, so
/// the field is a full signed 32-bit value even though it is clamped to
/// `maxmoney` (16000 by default) before sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Money {
    pub amount: i32,
    /// `bTrackChange` — flash the number on the HUD.
    pub blink: bool,
}

/// `ScoreInfo` — one scoreboard row (`regamedll/dlls/player.cpp:6079-6085`,
/// re-sent for every player on join at `dlls/multiplay_gamerules.cpp:3449-3455`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreInfo {
    pub client: u8,
    pub frags: i16,
    pub deaths: i16,
    /// Always `0` in CS — the slot Half-Life used for the player class.
    pub class: i16,
    pub team: Team,
    /// The raw team short, before [`Team::from_id`].
    pub team_id: i16,
}

/// `TeamInfo` — which side a client is on
/// (`regamedll/dlls/player.cpp:6072-6075`, and the join burst at
/// `dlls/multiplay_gamerules.cpp:3491-3494`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamInfo {
    pub client: u8,
    pub team: Team,
    /// The literal the server sent, kept because a mod's team name is
    /// information even when [`Team::from_name`] flattens it to `Unassigned`.
    pub team_name: String,
}

/// `ScoreAttrib` — the scoreboard's per-player status bits
/// (`regamedll/dlls/player.cpp:5738-5744`,
/// `CBasePlayer::SetScoreboardAttributes`).
///
/// Note the server *lies to enemies on purpose*: `REGAMEDLL_FIXES` strips the
/// bomb and defuse-kit bits before sending them to a player who is not a
/// teammate (`player.cpp:5729-5735`). So `has_bomb` means "a teammate is
/// carrying it", never "any player is carrying it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreAttrib {
    pub client: u8,
    pub flags: u8,
}

impl ScoreAttrib {
    /// `SCORE_STATUS_DEAD` (`regamedll/dlls/cdll_dll.h:64`).
    pub const DEAD: u8 = 1;
    /// `SCORE_STATUS_BOMB` (`cdll_dll.h:65`).
    pub const BOMB: u8 = 2;
    /// `SCORE_STATUS_VIP` (`cdll_dll.h:66`).
    pub const VIP: u8 = 4;
    /// `SCORE_STATUS_DEFKIT` (`cdll_dll.h:67`).
    pub const DEFKIT: u8 = 8;

    pub fn is_dead(&self) -> bool {
        self.flags & Self::DEAD != 0
    }
    pub fn has_bomb(&self) -> bool {
        self.flags & Self::BOMB != 0
    }
    pub fn is_vip(&self) -> bool {
        self.flags & Self::VIP != 0
    }
    pub fn has_defuser(&self) -> bool {
        self.flags & Self::DEFKIT != 0
    }
}

/// `DeathMsg` — somebody died
/// (`regamedll/dlls/multiplay_gamerules.cpp:5390-5419`,
/// `CHalfLifeMultiplay::SendDeathMessage`).
///
/// The four base fields are what stock CS sends. ReGameDLL's `REGAMEDLL_ADD`
/// build appends an optional extension when `iDeathMessageFlags > 0`: a `LONG`
/// of flags, then — conditionally on those flags — the victim's position
/// (`PLAYERDEATH_POSITION` 0x1), the assisting teammate
/// (`PLAYERDEATH_ASSISTANT` 0x2) and the kill's rarity bitsum
/// (`PLAYERDEATH_KILLRARITY` 0x4), per `dlls/gamerules.h:233-242`. The
/// extension parses all-or-nothing: if any part of it is short we keep the
/// base fields and drop the extras rather than failing the whole message.
#[derive(Debug, Clone, PartialEq)]
pub struct DeathMsg {
    /// Killer's client slot, or `0` for a world/entity kill.
    pub killer: u8,
    pub victim: u8,
    pub headshot: bool,
    /// e.g. `ak47`, `grenade`, `world`.
    pub weapon: String,
    /// `iDeathMessageFlags`, when the server is a ReGameDLL `REGAMEDLL_ADD`
    /// build and chose to send the extension.
    pub flags: Option<i32>,
    /// Where the victim fell (`PLAYERDEATH_POSITION`).
    pub position: Option<[f32; 3]>,
    /// The teammate who assisted (`PLAYERDEATH_ASSISTANT`); `0` for none.
    pub assistant: Option<u8>,
    /// `iRarityOfKill` bitsum (`PLAYERDEATH_KILLRARITY`).
    pub rarity: Option<i32>,
}

impl DeathMsg {
    /// `PLAYERDEATH_POSITION` (`regamedll/dlls/gamerules.h:233`).
    pub const F_POSITION: i32 = 0x001;
    /// `PLAYERDEATH_ASSISTANT` (`gamerules.h:237`).
    pub const F_ASSISTANT: i32 = 0x002;
    /// `PLAYERDEATH_KILLRARITY` (`gamerules.h:242`).
    pub const F_KILLRARITY: i32 = 0x004;
}

/// `StatusIcon` — one HUD icon on, off or flashing.
///
/// The colour triple is present only when the icon is being shown: the clear
/// path writes just the status and the sprite name
/// (`BuyZoneIcon_Clear`, `regamedll/dlls/player.cpp:2047-2051`) while the set
/// path appends r/g/b (`BuyZoneIcon_Set`, `player.cpp:2036-2043`). Sprites
/// worth knowing: `"buyzone"` (`player.cpp:2039`), `"rescue"`
/// (`player.cpp:2097`), `"defuser"` (`player.cpp:3825`, `player.cpp:5895`) and
/// `"c4"` (`player.cpp:1935`, `dlls/observer.cpp:372`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusIcon {
    pub status: IconStatus,
    pub sprite: String,
    /// `(r, g, b)`; absent on a hide.
    pub color: Option<(u8, u8, u8)>,
}

/// `BarTime` — the plant/defuse progress bar
/// (`regamedll/dlls/player.cpp:1965-1967`, `CBasePlayer::SetProgressBarTime`).
/// `0` clears it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarTime {
    pub seconds: i16,
}

/// `BarTime2` — a progress bar that starts part-way through
/// (`regamedll/dlls/player.cpp:2007-2010`,
/// `CBasePlayer::SetProgressBarTime2`), used when a defuse resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarTime2 {
    pub duration: i16,
    /// How far in we already are, as a percentage.
    pub elapsed_percent: i16,
}

/// `TextMsg` — a localisation token plus up to four substitutions
/// (`regamedll/dlls/util.cpp:654-665`, `UTIL_ClientPrintAll`, and the
/// single-client form at `util.cpp:670-681`).
///
/// The message is normally a `#`-prefixed token such as `#Bomb_Planted`, which
/// is exactly what makes this the cheapest round-state signal available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMsg {
    pub dest: TextDest,
    pub message: String,
    /// `param1`..`param4`, only those the server actually wrote.
    pub params: Vec<String>,
}

/// `BombDrop` — where the C4 is, and whether it is planted
/// (`regamedll/dlls/multiplay_gamerules.cpp:3534-3539`, and the drop-on-death
/// path at `dlls/player.cpp:8494-8499`).
///
/// One caller is a HUD hack rather than a real position: `CBasePlayer::HideTimer`
/// (`player.cpp:10665-10671`) sends `PLANTED` at coordinates `(0, 0, 0)` purely
/// to make the client hide its round timer. [`GameState`] treats an all-zero
/// planted position as "no position given" for that reason.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BombDrop {
    pub position: [f32; 3],
    /// `BOMB_FLAG_DROPPED` 0 / `BOMB_FLAG_PLANTED` 1
    /// (`regamedll/dlls/weapons.h:856-857`).
    pub flag: u8,
}

impl BombDrop {
    pub fn is_planted(&self) -> bool {
        self.flag == 1
    }
}

/// `HostagePos` — a hostage's position
/// (`regamedll/dlls/hostage/hostage.cpp:1262-1268` for movement,
/// `dlls/player.cpp:7480-7486` for the initial placement).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostagePos {
    pub update: HostageUpdate,
    pub index: u8,
    pub position: [f32; 3],
}

/// `HostageK` — a hostage was killed
/// (`regamedll/dlls/hostage/hostage.cpp:1291-1293`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostageK {
    pub index: u8,
}

/// `ItemStatus` — nightvision and defuse kit
/// (`regamedll/dlls/player.cpp:113-115`, `CBasePlayer::SendItemStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStatus {
    pub flags: u8,
}

impl ItemStatus {
    /// `ITEM_STATUS_NIGHTVISION` (`regamedll/dlls/cdll_dll.h:60`).
    pub const NIGHTVISION: u8 = 1;
    /// `ITEM_STATUS_DEFUSER` (`cdll_dll.h:61`).
    pub const DEFUSER: u8 = 2;

    pub fn has_nightvision(&self) -> bool {
        self.flags & Self::NIGHTVISION != 0
    }
    pub fn has_defuser(&self) -> bool {
        self.flags & Self::DEFUSER != 0
    }
}

/// `Radar` — a teammate's position for the HUD radar
/// (`regamedll/dlls/multiplay_gamerules.cpp:3510-3515`, and the periodic update
/// at `dlls/player.cpp:6736-6741`).
///
/// Only ever sent about teammates, and at COORD precision (1/8 unit), but it is
/// the one place the server volunteers other players' positions without us
/// having to decode the entity delta stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Radar {
    pub client: u8,
    pub position: [f32; 3],
}

/// `ScreenFade` (`regamedll/dlls/util.cpp:565-573`,
/// `UTIL_ScreenFade`). The durations are the engine's 12-bit fixed point
/// (`ScreenFadeStruct`), not seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenFade {
    pub duration: i16,
    pub hold: i16,
    pub flags: i16,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// `Scenario` — the objective icon
/// (`regamedll/dlls/ggrenade.cpp:1394-1400` for the ticking bomb,
/// `dlls/player.cpp:7520-7524` for `hostage<n>`, and the bare `WRITE_BYTE(0)`
/// clear at `ggrenade.cpp:1129-1131`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// `None` when the leading byte was 0 — icon off.
    pub icon: Option<ScenarioIcon>,
}

/// The body of an active [`Scenario`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioIcon {
    /// `bombticking`, `hostage1`..`hostage4`.
    pub sprite: String,
    pub flags: u8,
    /// Blink interval; only the bomb-timer form sends it.
    pub interval: Option<i16>,
    pub offset: Option<i16>,
}

/// `ShowMenu` — the text fallback for a VGUI menu
/// (`regamedll/dlls/client.cpp:392-397`, and `dlls/player.cpp:3753-3758`).
/// A menu longer than the 255-byte payload arrives in pieces with
/// `need_more` set on all but the last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowMenu {
    /// Bitmask of usable slots; `MENU_KEY_1` is bit 0 (`dlls/cdll_dll.h:79-88`).
    pub valid_slots: i16,
    /// Seconds; `-1` means "until dismissed", which is why it is signed.
    pub display_time: i8,
    pub need_more: bool,
    pub text: String,
}

/// `VGUIMenu` — the compact menu form a client gets when its userinfo carries
/// `_vgui_menus 1` (`regamedll/dlls/client.cpp:420-426`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VguiMenu {
    /// `VGUIMenu` enum — 2 is team select, 26/27 the CT/T model menus.
    pub menu_type: u8,
    pub bitmask: i16,
    /// Always `-1` from the game DLL.
    pub display_time: i8,
    /// Always `0` from the game DLL.
    pub need_more: u8,
    /// Always `" "` from the game DLL.
    pub text: String,
}

// ---------------------------------------------------------------------------
// the message enum
// ---------------------------------------------------------------------------

/// One decoded user message.
#[derive(Debug, Clone, PartialEq)]
pub enum UserMessage {
    /// We just (re)spawned — `CBasePlayer::UpdateClientData`
    /// (`regamedll/dlls/player.cpp:7577-7578`). Zero payload, and the single
    /// most useful edge in the whole set: it is the canonical start-of-life
    /// signal, fired once per spawn just before the HUD is repopulated.
    ResetHud,
    /// First HUD setup after connecting (`dlls/player.cpp:7582-7583`). Zero
    /// payload; follows the very first `ResetHUD`.
    InitHud,
    CurWeapon(CurWeapon),
    /// `Health` — our hit points, clamped to a byte
    /// (`dlls/player.cpp:7735-7737`).
    Health(u8),
    /// `Battery` — our armour (`dlls/player.cpp:7747-7749`).
    Battery(i16),
    /// `AmmoX` — reserve ammo for one ammo type
    /// (`dlls/player.cpp:7466-7469`); index is `0..32` (`MAX_AMMO_SLOTS`,
    /// `dlls/cdll_dll.h:33`) and the count is clamped to a byte server-side.
    AmmoX { index: u8, count: u8 },
    /// `AmmoPickup` — how much of what we just picked up
    /// (`dlls/player.cpp:7374-7377`).
    AmmoPickup { index: u8, count: u8 },
    WeaponList(WeaponList),
    Money(Money),
    ScoreInfo(ScoreInfo),
    TeamInfo(TeamInfo),
    ScoreAttrib(ScoreAttrib),
    DeathMsg(DeathMsg),
    /// `RoundTime` — seconds left in the round
    /// (`dlls/player.cpp:3689-3691`, `CBasePlayer::SendRoundTime`... sent on
    /// spawn and whenever the round clock is changed, *not* every tick).
    RoundTime(i16),
    StatusIcon(StatusIcon),
    BarTime(BarTime),
    BarTime2(BarTime2),
    TextMsg(TextMsg),
    BombDrop(BombDrop),
    /// `BombPickup` — somebody took the C4 off the ground
    /// (`dlls/multiplay_gamerules.cpp:1709-1710`). Zero payload: it says the
    /// bomb is no longer lying around, not who has it.
    BombPickup,
    HostagePos(HostagePos),
    HostageK(HostageK),
    ItemStatus(ItemStatus),
    Radar(Radar),
    /// `SetFOV` — field of view, also how a scope announces itself
    /// (`dlls/player.cpp:2224-2226`).
    SetFov(u8),
    ScreenFade(ScreenFade),
    Scenario(Scenario),
    ShowMenu(ShowMenu),
    VguiMenu(VguiMenu),
}

/// Decode one user message by name.
///
/// Returns `None` for a name we do not model — which is most of them — and
/// also for any payload too short or too malformed to decode. Callers that
/// need to distinguish the two should check the name themselves; for the bot
/// the two cases mean the same thing, namely "nothing to act on".
pub fn parse(name: &str, payload: &[u8]) -> Option<UserMessage> {
    let mut c = Cur::new(payload);
    let msg = match name {
        "ResetHUD" => UserMessage::ResetHud,
        "InitHUD" => UserMessage::InitHud,

        "CurWeapon" => UserMessage::CurWeapon(CurWeapon {
            state: c.u8()?,
            weapon_id: c.u8()?,
            clip: c.u8()?,
        }),

        "Health" => UserMessage::Health(c.u8()?),
        "Battery" => UserMessage::Battery(c.i16()?),

        "AmmoX" => UserMessage::AmmoX {
            index: c.u8()?,
            count: c.u8()?,
        },
        "AmmoPickup" => UserMessage::AmmoPickup {
            index: c.u8()?,
            count: c.u8()?,
        },

        "WeaponList" => UserMessage::WeaponList(WeaponList {
            name: c.cstr()?,
            ammo1_index: c.u8()?,
            max_ammo1: c.u8()?,
            ammo2_index: c.u8()?,
            max_ammo2: c.u8()?,
            slot: c.u8()?,
            position: c.u8()?,
            weapon_id: c.u8()?,
            flags: c.u8()?,
        }),

        "Money" => UserMessage::Money(Money {
            amount: c.i32()?,
            blink: c.u8()? != 0,
        }),

        "ScoreInfo" => {
            let client = c.u8()?;
            let frags = c.i16()?;
            let deaths = c.i16()?;
            let class = c.i16()?;
            let team_id = c.i16()?;
            UserMessage::ScoreInfo(ScoreInfo {
                client,
                frags,
                deaths,
                class,
                team: Team::from_id(team_id),
                team_id,
            })
        }

        "TeamInfo" => {
            let client = c.u8()?;
            let team_name = c.cstr()?;
            UserMessage::TeamInfo(TeamInfo {
                client,
                team: Team::from_name(&team_name),
                team_name,
            })
        }

        "ScoreAttrib" => UserMessage::ScoreAttrib(ScoreAttrib {
            client: c.u8()?,
            flags: c.u8()?,
        }),

        "DeathMsg" => {
            let killer = c.u8()?;
            let victim = c.u8()?;
            let headshot = c.u8()? != 0;
            let weapon = c.cstr()?;
            let mut m = DeathMsg {
                killer,
                victim,
                headshot,
                weapon,
                flags: None,
                position: None,
                assistant: None,
                rarity: None,
            };
            // ReGameDLL's optional tail; all-or-nothing so a clipped extension
            // cannot leave us with half a position.
            if c.remaining() >= 4 {
                if let Some(ext) = parse_deathmsg_ext(&mut c) {
                    m.flags = Some(ext.0);
                    m.position = ext.1;
                    m.assistant = ext.2;
                    m.rarity = ext.3;
                }
            }
            UserMessage::DeathMsg(m)
        }

        "RoundTime" => UserMessage::RoundTime(c.i16()?),

        "StatusIcon" => {
            let status = IconStatus::from_u8(c.u8()?)?;
            let sprite = c.cstr()?;
            // Colour is written only on the show/flash path; a hide stops at
            // the sprite name.
            let color = if c.remaining() >= 3 {
                Some((c.u8()?, c.u8()?, c.u8()?))
            } else {
                None
            };
            UserMessage::StatusIcon(StatusIcon {
                status,
                sprite,
                color,
            })
        }

        "BarTime" => UserMessage::BarTime(BarTime { seconds: c.i16()? }),
        "BarTime2" => UserMessage::BarTime2(BarTime2 {
            duration: c.i16()?,
            elapsed_percent: c.i16()?,
        }),

        "TextMsg" => {
            let dest = TextDest::from_u8(c.u8()?);
            let message = c.cstr()?;
            let mut params = Vec::new();
            // Up to four, and only those actually written; a truncated tail
            // costs us a parameter, not the message.
            while params.len() < 4 {
                match c.cstr() {
                    Some(p) => params.push(p),
                    None => break,
                }
            }
            UserMessage::TextMsg(TextMsg {
                dest,
                message,
                params,
            })
        }

        "BombDrop" => UserMessage::BombDrop(BombDrop {
            position: c.vec3()?,
            flag: c.u8()?,
        }),
        "BombPickup" => UserMessage::BombPickup,

        "HostagePos" => UserMessage::HostagePos(HostagePos {
            update: HostageUpdate::from_u8(c.u8()?)?,
            index: c.u8()?,
            position: c.vec3()?,
        }),
        "HostageK" => UserMessage::HostageK(HostageK { index: c.u8()? }),

        "ItemStatus" => UserMessage::ItemStatus(ItemStatus { flags: c.u8()? }),

        "Radar" => UserMessage::Radar(Radar {
            client: c.u8()?,
            position: c.vec3()?,
        }),

        "SetFOV" => UserMessage::SetFov(c.u8()?),

        "ScreenFade" => UserMessage::ScreenFade(ScreenFade {
            duration: c.i16()?,
            hold: c.i16()?,
            flags: c.i16()?,
            r: c.u8()?,
            g: c.u8()?,
            b: c.u8()?,
            a: c.u8()?,
        }),

        "Scenario" => {
            let active = c.u8()?;
            let icon = if active == 0 {
                None
            } else {
                let sprite = c.cstr()?;
                let flags = c.u8()?;
                // The bomb-timer form appends a blink interval and offset;
                // the hostage-count form stops at the flags byte.
                let (interval, offset) = if c.remaining() >= 4 {
                    (Some(c.i16()?), Some(c.i16()?))
                } else {
                    (None, None)
                };
                Some(ScenarioIcon {
                    sprite,
                    flags,
                    interval,
                    offset,
                })
            };
            UserMessage::Scenario(Scenario { icon })
        }

        "ShowMenu" => UserMessage::ShowMenu(ShowMenu {
            valid_slots: c.i16()?,
            display_time: c.i8()?,
            need_more: c.u8()? != 0,
            text: c.cstr()?,
        }),

        "VGUIMenu" => UserMessage::VguiMenu(VguiMenu {
            menu_type: c.u8()?,
            bitmask: c.i16()?,
            display_time: c.i8()?,
            need_more: c.u8()?,
            text: c.cstr()?,
        }),

        _ => return None,
    };
    Some(msg)
}

/// The `REGAMEDLL_ADD` tail of `DeathMsg`; `None` if any declared part is
/// missing, so the caller can discard the extension whole.
#[allow(clippy::type_complexity)]
fn parse_deathmsg_ext(
    c: &mut Cur<'_>,
) -> Option<(i32, Option<[f32; 3]>, Option<u8>, Option<i32>)> {
    let flags = c.i32()?;
    let position = if flags & DeathMsg::F_POSITION != 0 {
        Some(c.vec3()?)
    } else {
        None
    };
    let assistant = if flags & DeathMsg::F_ASSISTANT != 0 {
        Some(c.u8()?)
    } else {
        None
    };
    let rarity = if flags & DeathMsg::F_KILLRARITY != 0 {
        Some(c.i32()?)
    } else {
        None
    };
    Some((flags, position, assistant, rarity))
}

// ---------------------------------------------------------------------------
// accumulated state
// ---------------------------------------------------------------------------

/// What we know about one client slot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayerInfo {
    pub team: Team,
    pub frags: i16,
    pub deaths: i16,
    /// From `ScoreAttrib`'s `SCORE_STATUS_DEAD`.
    pub dead: bool,
    /// From `ScoreAttrib`'s `SCORE_STATUS_BOMB`. Only ever true for teammates
    /// — see [`ScoreAttrib`].
    pub has_bomb: bool,
    pub vip: bool,
    pub has_defuser: bool,
    /// Last position the server volunteered via `Radar` — teammates only.
    pub radar_position: Option<[f32; 3]>,
    /// Has the server said anything at all about this slot?
    pub seen: bool,
}

/// Everything the user-message stream tells us, folded into one struct.
///
/// This is deliberately *only* what user messages carry. Our own position and
/// velocity come from `svc_clientdata` ([`crate::world`]); other players'
/// positions come from the entity delta stream. What lives here is the game
/// layer: who is on which side, our wallet, the round clock, and the bomb.
#[derive(Debug, Clone, PartialEq)]
pub struct GameState {
    /// Index 0 is unused so that a client slot indexes directly.
    players: [PlayerInfo; MAX_CLIENTS + 1],

    /// Our own slot, if the caller has told us (it comes from
    /// `svc_serverinfo`'s `player_index`, not from any user message).
    pub self_index: Option<u8>,
    /// Set once the server has offered the team-selection menu.
    ///
    /// `ShowVGUIMenu(VGUI_Menu_Team)` goes out in the same breath as the
    /// SHOWTEAMSELECT -> PICKINGTEAM transition
    /// (`multiplay_gamerules.cpp:3778-3785`), so it is the client-visible
    /// proof that the server is ready to accept `jointeam`. Answering earlier
    /// is accepted and then silently undone.
    pub saw_team_menu: bool,

    // --- our own state, all from MSG_ONE messages ---
    pub money: i32,
    pub health: u8,
    pub armor: i16,
    /// `ItemStatus` bits — see [`ItemStatus::NIGHTVISION`] / [`ItemStatus::DEFUSER`].
    pub item_flags: u8,
    pub weapon_id: u8,
    /// Rounds in the active weapon's clip; `255` means "no clip".
    pub weapon_clip: u8,
    /// Reserve ammo per ammo index (`MAX_AMMO_SLOTS`, `dlls/cdll_dll.h:33`).
    pub ammo: [u8; 32],
    pub fov: u8,

    // --- round state ---
    /// Seconds left, as of the last `RoundTime`. Not a live clock: the server
    /// only sends this on spawn and on changes.
    pub round_time: i16,
    /// How many `ResetHUD` edges we have seen — one per spawn.
    pub hud_resets: u32,

    // --- bomb / hostages ---
    pub bomb_planted: bool,
    /// Where the C4 is. `None` when unknown, and specifically `None` for the
    /// `HideTimer` hack that plants it at the origin (see [`BombDrop`]).
    pub bomb_position: Option<[f32; 3]>,
    /// Hostage index to last known position.
    pub hostages: HashMap<u8, [f32; 3]>,
    pub hostages_killed: u32,

    // --- HUD icons ---
    pub in_buy_zone: bool,
    pub in_rescue_zone: bool,
    /// The `"c4"` icon: we are carrying the bomb, or standing on it.
    pub c4_icon: bool,
    /// The `"defuser"` icon.
    pub defuser_icon: bool,
    /// The `Scenario` objective sprite, e.g. `bombticking` or `hostage3`.
    pub scenario_sprite: Option<String>,

    /// The most recent `BarTime`: `Some(seconds)` while a plant or defuse is
    /// running, `None` once the server clears it with a zero.
    pub bar_time: Option<i16>,
    /// The most recent `BarTime2`, as `(duration, elapsed_percent)`.
    pub bar_time2: Option<(i16, i16)>,
}

impl Default for GameState {
    fn default() -> Self {
        Self {
            players: std::array::from_fn(|_| PlayerInfo::default()),
            self_index: None,
            saw_team_menu: false,
            money: 0,
            health: 0,
            armor: 0,
            item_flags: 0,
            weapon_id: 0,
            weapon_clip: 0,
            ammo: [0u8; 32],
            fov: 0,
            round_time: 0,
            hud_resets: 0,
            bomb_planted: false,
            bomb_position: None,
            hostages: HashMap::new(),
            hostages_killed: 0,
            in_buy_zone: false,
            in_rescue_zone: false,
            c4_icon: false,
            defuser_icon: false,
            scenario_sprite: None,
            bar_time: None,
            bar_time2: None,
        }
    }
}

impl GameState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `payload` as `name` and fold it in.
    ///
    /// A name we do not model, or a payload we cannot decode, is a no-op.
    pub fn apply(&mut self, name: &str, payload: &[u8]) {
        if let Some(msg) = parse(name, payload) {
            self.apply_msg(&msg);
        }
    }

    /// Fold in an already-decoded message.
    pub fn apply_msg(&mut self, msg: &UserMessage) {
        match msg {
            UserMessage::ResetHud => self.reset_round(),
            UserMessage::InitHud => {}

            UserMessage::CurWeapon(w) => {
                // The all-zero clear on death (`dlls/player.cpp:1922-1926`)
                // arrives as state 0 / id 0, which is not a weapon.
                if w.is_active() {
                    self.weapon_id = w.weapon_id;
                    self.weapon_clip = w.clip;
                }
            }
            UserMessage::Health(hp) => self.health = *hp,
            UserMessage::Battery(a) => self.armor = *a,
            UserMessage::AmmoX { index, count } => {
                if let Some(slot) = self.ammo.get_mut(usize::from(*index)) {
                    *slot = *count;
                }
            }
            UserMessage::AmmoPickup { .. } => {}
            UserMessage::WeaponList(_) => {}

            UserMessage::Money(m) => self.money = m.amount,

            UserMessage::ScoreInfo(s) => {
                if let Some(p) = self.player_mut(s.client) {
                    p.seen = true;
                    p.frags = s.frags;
                    p.deaths = s.deaths;
                    p.team = s.team;
                }
            }
            UserMessage::TeamInfo(t) => {
                if let Some(p) = self.player_mut(t.client) {
                    p.seen = true;
                    p.team = t.team;
                }
            }
            UserMessage::ScoreAttrib(a) => {
                if let Some(p) = self.player_mut(a.client) {
                    p.seen = true;
                    p.dead = a.is_dead();
                    p.has_bomb = a.has_bomb();
                    p.vip = a.is_vip();
                    p.has_defuser = a.has_defuser();
                }
            }
            UserMessage::DeathMsg(d) => {
                if let Some(p) = self.player_mut(d.victim) {
                    p.seen = true;
                    p.dead = true;
                }
            }

            UserMessage::RoundTime(t) => self.round_time = *t,

            UserMessage::StatusIcon(icon) => {
                let on = icon.status.is_visible();
                match icon.sprite.as_str() {
                    "buyzone" => self.in_buy_zone = on,
                    "rescue" => self.in_rescue_zone = on,
                    "c4" => self.c4_icon = on,
                    "defuser" => self.defuser_icon = on,
                    _ => {}
                }
            }

            UserMessage::BarTime(b) => {
                self.bar_time = if b.seconds == 0 {
                    None
                } else {
                    Some(b.seconds)
                };
                if b.seconds == 0 {
                    self.bar_time2 = None;
                }
            }
            UserMessage::BarTime2(b) => {
                self.bar_time2 = Some((b.duration, b.elapsed_percent));
                self.bar_time = if b.duration == 0 {
                    None
                } else {
                    Some(b.duration)
                };
            }

            UserMessage::TextMsg(_) => {}

            UserMessage::BombDrop(b) => {
                self.bomb_planted = b.is_planted();
                // (0,0,0) planted is `CBasePlayer::HideTimer`
                // (`dlls/player.cpp:10665-10671`) hiding the round timer, not a
                // bomb at the world origin.
                self.bomb_position = if b.position == [0.0, 0.0, 0.0] {
                    None
                } else {
                    Some(b.position)
                };
            }
            UserMessage::BombPickup => {
                // Off the ground; nobody has told us where it is any more.
                self.bomb_position = None;
            }

            UserMessage::HostagePos(h) => {
                self.hostages.insert(h.index, h.position);
            }
            UserMessage::HostageK(h) => {
                self.hostages.remove(&h.index);
                self.hostages_killed += 1;
            }

            UserMessage::ItemStatus(s) => self.item_flags = s.flags,
            UserMessage::Radar(r) => {
                if let Some(p) = self.player_mut(r.client) {
                    p.seen = true;
                    p.radar_position = Some(r.position);
                }
            }
            UserMessage::SetFov(f) => self.fov = *f,
            UserMessage::ScreenFade(_) => {}

            UserMessage::Scenario(s) => {
                self.scenario_sprite = s.icon.as_ref().map(|i| i.sprite.clone());
            }
            // The team menu is the server saying "I am ready for jointeam".
            // VGUI_Menu_Team is type 2 (`cdll_dll.h:94-108`). The text
            // fallback is used when the client has `_vgui_menus 0`; ours does
            // not, but accept either rather than depend on it.
            UserMessage::VguiMenu(m) => {
                if m.menu_type == 2 {
                    self.saw_team_menu = true;
                }
            }
            UserMessage::ShowMenu(_) => {
                self.saw_team_menu = true;
            }
        }
    }

    /// Clear the per-round flags on a `ResetHUD`.
    ///
    /// What survives is what the server will *not* resend: teams, scores and
    /// money. What goes is everything the HUD is about to be told again —
    /// which is exactly the set that would otherwise go stale for a whole
    /// round.
    fn reset_round(&mut self) {
        self.hud_resets = self.hud_resets.saturating_add(1);
        for p in self.players.iter_mut() {
            p.dead = false;
            p.has_bomb = false;
            p.has_defuser = false;
            p.radar_position = None;
        }
        self.item_flags = 0;
        self.weapon_id = 0;
        self.weapon_clip = 0;
        self.ammo = [0u8; 32];
        self.round_time = 0;
        self.bomb_planted = false;
        self.bomb_position = None;
        self.hostages.clear();
        self.in_buy_zone = false;
        self.in_rescue_zone = false;
        // Cleared deliberately, matching the real client: ResetHUD wipes the
        // status icons and the server re-sends StatusIcon within half a second
        // once HandleSignals republishes the SIGNAL_BUY latch
        // (`player.cpp:7875-7879`). Callers must therefore treat "not in a buy
        // zone" as provisional for the first moments after a spawn, rather
        // than concluding there is no zone.
        self.c4_icon = false;
        self.defuser_icon = false;
        self.scenario_sprite = None;
        self.bar_time = None;
        self.bar_time2 = None;
    }

    /// One client slot, `1..=32`.
    pub fn player(&self, slot: u8) -> Option<&PlayerInfo> {
        if slot == 0 {
            return None;
        }
        self.players.get(usize::from(slot))
    }

    fn player_mut(&mut self, slot: u8) -> Option<&mut PlayerInfo> {
        if slot == 0 {
            return None;
        }
        self.players.get_mut(usize::from(slot))
    }

    /// Us, if [`GameState::self_index`] has been set.
    pub fn me(&self) -> Option<&PlayerInfo> {
        self.player(self.self_index?)
    }

    /// Our own team, or [`Team::Unassigned`] if we do not know yet.
    pub fn my_team(&self) -> Team {
        self.me().map(|p| p.team).unwrap_or_default()
    }

    /// Every slot the server has said anything about, as `(slot, info)`.
    pub fn known_players(&self) -> impl Iterator<Item = (u8, &PlayerInfo)> {
        self.players
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, p)| p.seen)
            .map(|(i, p)| (i as u8, p))
    }

    /// Known players on one side.
    pub fn team_members(&self, team: Team) -> impl Iterator<Item = (u8, &PlayerInfo)> {
        self.known_players().filter(move |(_, p)| p.team == team)
    }

    /// Do we hold a defuse kit? (`ItemStatus`, which is authoritative — the
    /// `"defuser"` status icon is also set for a kit lying on the ground.)
    pub fn has_defuser(&self) -> bool {
        self.item_flags & ItemStatus::DEFUSER != 0
    }

    /// Do we hold nightvision?
    pub fn has_nightvision(&self) -> bool {
        self.item_flags & ItemStatus::NIGHTVISION != 0
    }

    /// Is a plant or defuse bar running?
    pub fn bar_running(&self) -> bool {
        self.bar_time.is_some()
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a payload from a small script, so the tests read like the
    /// `WRITE_*` calls they mirror.
    #[derive(Default)]
    struct Msg(Vec<u8>);

    impl Msg {
        fn new() -> Self {
            Msg(Vec::new())
        }
        fn byte(mut self, v: u8) -> Self {
            self.0.push(v);
            self
        }
        fn char(mut self, v: i8) -> Self {
            self.0.push(v as u8);
            self
        }
        fn short(mut self, v: i16) -> Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn long(mut self, v: i32) -> Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        fn coord(mut self, v: f32) -> Self {
            self.0.extend_from_slice(&encode_coord(v));
            self
        }
        fn string(mut self, s: &str) -> Self {
            self.0.extend_from_slice(s.as_bytes());
            self.0.push(0);
            self
        }
        fn done(self) -> Vec<u8> {
            self.0
        }
    }

    // --- teams -------------------------------------------------------------

    #[test]
    fn teaminfo_decodes_the_literal_team_names() {
        let cases = [
            ("CT", Team::CounterTerrorist),
            ("TERRORIST", Team::Terrorist),
            ("SPECTATOR", Team::Spectator),
            ("UNASSIGNED", Team::Unassigned),
        ];
        for (literal, want) in cases {
            let p = Msg::new().byte(3).string(literal).done();
            match parse("TeamInfo", &p) {
                Some(UserMessage::TeamInfo(t)) => {
                    assert_eq!(t.client, 3);
                    assert_eq!(t.team, want, "team literal {literal}");
                    assert_eq!(t.team_name, literal);
                }
                other => panic!("{literal}: unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn an_unknown_team_string_is_unassigned_not_a_panic() {
        let p = Msg::new().byte(7).string("ZOMBIES").done();
        match parse("TeamInfo", &p) {
            Some(UserMessage::TeamInfo(t)) => {
                assert_eq!(t.team, Team::Unassigned);
                // The literal survives, because it is information.
                assert_eq!(t.team_name, "ZOMBIES");
                assert_eq!(t.client, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn scoreinfo_team_ids_follow_the_teamname_enum() {
        // enum TeamName { UNASSIGNED, TERRORIST, CT, SPECTATOR } (player.h:202)
        assert_eq!(Team::from_id(0), Team::Unassigned);
        assert_eq!(Team::from_id(1), Team::Terrorist);
        assert_eq!(Team::from_id(2), Team::CounterTerrorist);
        assert_eq!(Team::from_id(3), Team::Spectator);
        assert_eq!(Team::from_id(99), Team::Unassigned);
        assert_eq!(Team::from_id(-1), Team::Unassigned);
    }

    #[test]
    fn team_names_round_trip() {
        for t in [
            Team::Unassigned,
            Team::Terrorist,
            Team::CounterTerrorist,
            Team::Spectator,
        ] {
            assert_eq!(Team::from_name(t.as_name()), t);
        }
        assert_eq!(Team::Terrorist.opposite(), Some(Team::CounterTerrorist));
        assert_eq!(Team::Spectator.opposite(), None);
        assert!(!Team::Spectator.is_playing());
    }

    // --- money -------------------------------------------------------------

    #[test]
    fn money_is_a_signed_32_bit_little_endian_long() {
        let p = Msg::new().long(16000).byte(1).done();
        assert_eq!(p, vec![0x80, 0x3E, 0x00, 0x00, 0x01]);
        match parse("Money", &p) {
            Some(UserMessage::Money(m)) => {
                assert_eq!(m.amount, 16000);
                assert!(m.blink);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn money_handles_zero_and_the_full_long_range() {
        for want in [0i32, 800, 16000, i32::MAX, -1, i32::MIN] {
            let p = Msg::new().long(want).byte(0).done();
            match parse("Money", &p) {
                Some(UserMessage::Money(m)) => {
                    assert_eq!(m.amount, want);
                    assert!(!m.blink);
                }
                other => panic!("{want}: unexpected {other:?}"),
            }
        }
    }

    // --- coords ------------------------------------------------------------

    #[test]
    fn bombdrop_coords_round_trip_through_the_eighth_unit_scale() {
        // Real de_dust2-ish A-site numbers, negatives included.
        let p = Msg::new()
            .coord(1264.0)
            .coord(2586.5)
            .coord(-127.875)
            .byte(1)
            .done();
        assert_eq!(p.len(), 7, "BombDrop is registered as 7 bytes");
        match parse("BombDrop", &p) {
            Some(UserMessage::BombDrop(b)) => {
                assert_eq!(b.position, [1264.0, 2586.5, -127.875]);
                assert!(b.is_planted());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_negative_coord_is_read_signed_not_as_a_huge_positive() {
        // -184.0 encodes as -1472 = 0xFA40; read unsigned that is +7808.0.
        let p = Msg::new()
            .coord(-184.0)
            .coord(-1000.0)
            .coord(-64.0)
            .byte(0)
            .done();
        assert_eq!(&p[..2], &[0x40, 0xFA]);
        match parse("BombDrop", &p) {
            Some(UserMessage::BombDrop(b)) => {
                assert_eq!(b.position, [-184.0, -1000.0, -64.0]);
                assert!(!b.is_planted(), "flag 0 is BOMB_FLAG_DROPPED");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hostagepos_decodes_type_index_and_a_negative_position() {
        let p = Msg::new()
            .byte(0)
            .byte(3)
            .coord(-2048.5)
            .coord(512.25)
            .coord(-95.125)
            .done();
        assert_eq!(p.len(), 8, "HostagePos is registered as 8 bytes");
        match parse("HostagePos", &p) {
            Some(UserMessage::HostagePos(h)) => {
                assert_eq!(h.update, HostageUpdate::Move);
                assert_eq!(h.index, 3);
                assert_eq!(h.position, [-2048.5, 512.25, -95.125]);
            }
            other => panic!("unexpected {other:?}"),
        }

        let init = Msg::new()
            .byte(1)
            .byte(1)
            .coord(0.0)
            .coord(0.0)
            .coord(0.0)
            .done();
        match parse("HostagePos", &init) {
            Some(UserMessage::HostagePos(h)) => assert_eq!(h.update, HostageUpdate::Init),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unknown_hostage_update_type_is_rejected() {
        let p = Msg::new()
            .byte(9)
            .byte(1)
            .coord(0.0)
            .coord(0.0)
            .coord(0.0)
            .done();
        assert_eq!(parse("HostagePos", &p), None);
    }

    #[test]
    fn radar_positions_decode_at_coord_precision() {
        let p = Msg::new()
            .byte(5)
            .coord(-1.125)
            .coord(0.375)
            .coord(-0.25)
            .done();
        assert_eq!(p.len(), 7);
        match parse("Radar", &p) {
            Some(UserMessage::Radar(r)) => {
                assert_eq!(r.client, 5);
                assert_eq!(r.position, [-1.125, 0.375, -0.25]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- status icon -------------------------------------------------------

    #[test]
    fn statusicon_show_carries_the_trailing_colour() {
        // BuyZoneIcon_Set, dlls/player.cpp:2036-2043.
        let p = Msg::new()
            .byte(1)
            .string("buyzone")
            .byte(0)
            .byte(160)
            .byte(0)
            .done();
        match parse("StatusIcon", &p) {
            Some(UserMessage::StatusIcon(s)) => {
                assert_eq!(s.status, IconStatus::Show);
                assert_eq!(s.sprite, "buyzone");
                assert_eq!(s.color, Some((0, 160, 0)));
                assert!(s.status.is_visible());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn statusicon_hide_omits_the_colour() {
        // BuyZoneIcon_Clear, dlls/player.cpp:2047-2051.
        let p = Msg::new().byte(0).string("buyzone").done();
        match parse("StatusIcon", &p) {
            Some(UserMessage::StatusIcon(s)) => {
                assert_eq!(s.status, IconStatus::Hide);
                assert_eq!(s.sprite, "buyzone");
                assert_eq!(s.color, None);
                assert!(!s.status.is_visible());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn statusicon_flash_is_a_visible_status() {
        let p = Msg::new()
            .byte(2)
            .string("c4")
            .byte(255)
            .byte(0)
            .byte(0)
            .done();
        match parse("StatusIcon", &p) {
            Some(UserMessage::StatusIcon(s)) => {
                assert_eq!(s.status, IconStatus::Flash);
                assert!(s.status.is_visible());
                assert_eq!(s.color, Some((255, 0, 0)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unknown_statusicon_status_is_rejected() {
        let p = Msg::new().byte(7).string("buyzone").done();
        assert_eq!(parse("StatusIcon", &p), None);
    }

    // --- score attrib ------------------------------------------------------

    #[test]
    fn scoreattrib_flag_bits_match_cdll_dll_h() {
        assert_eq!(ScoreAttrib::DEAD, 1);
        assert_eq!(ScoreAttrib::BOMB, 2);
        assert_eq!(ScoreAttrib::VIP, 4);
        assert_eq!(ScoreAttrib::DEFKIT, 8);

        let all = ScoreAttrib {
            client: 1,
            flags: 0b1111,
        };
        assert!(all.is_dead() && all.has_bomb() && all.is_vip() && all.has_defuser());

        let none = ScoreAttrib {
            client: 1,
            flags: 0,
        };
        assert!(!none.is_dead() && !none.has_bomb() && !none.is_vip() && !none.has_defuser());

        // A live carrier: alive, holding the bomb.
        let carrier = Msg::new().byte(4).byte(ScoreAttrib::BOMB).done();
        match parse("ScoreAttrib", &carrier) {
            Some(UserMessage::ScoreAttrib(a)) => {
                assert_eq!(a.client, 4);
                assert!(a.has_bomb());
                assert!(!a.is_dead());
                assert!(!a.has_defuser());
            }
            other => panic!("unexpected {other:?}"),
        }

        // A dead CT who bought a kit.
        let dead_kit = Msg::new()
            .byte(9)
            .byte(ScoreAttrib::DEAD | ScoreAttrib::DEFKIT)
            .done();
        match parse("ScoreAttrib", &dead_kit) {
            Some(UserMessage::ScoreAttrib(a)) => {
                assert!(a.is_dead() && a.has_defuser());
                assert!(!a.has_bomb() && !a.is_vip());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- the rest of the fixed-size set ------------------------------------

    #[test]
    fn curweapon_separates_state_from_the_weapon_id() {
        let p = Msg::new().byte(1).byte(28).byte(30).done();
        match parse("CurWeapon", &p) {
            Some(UserMessage::CurWeapon(w)) => {
                assert!(w.is_active());
                assert!(!w.on_target());
                assert_eq!(w.weapon_id, 28);
                assert_eq!(w.clip_count(), Some(30));
            }
            other => panic!("unexpected {other:?}"),
        }

        // WEAPON_IS_ONTARGET, dlls/weapons.h:49.
        let on_target = Msg::new().byte(0x40).byte(28).byte(29).done();
        match parse("CurWeapon", &on_target) {
            Some(UserMessage::CurWeapon(w)) => {
                assert!(w.is_active() && w.on_target());
            }
            other => panic!("unexpected {other:?}"),
        }

        // A knife: m_iClip is -1, which reaches us as 255.
        let knife = Msg::new().byte(1).byte(29).byte(255).done();
        match parse("CurWeapon", &knife) {
            Some(UserMessage::CurWeapon(w)) => assert_eq!(w.clip_count(), None),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn the_simple_scalar_messages_decode() {
        assert_eq!(parse("Health", &[87]), Some(UserMessage::Health(87)));
        assert_eq!(parse("SetFOV", &[90]), Some(UserMessage::SetFov(90)));
        assert_eq!(
            parse("Battery", &Msg::new().short(100).done()),
            Some(UserMessage::Battery(100))
        );
        assert_eq!(
            parse("RoundTime", &Msg::new().short(115).done()),
            Some(UserMessage::RoundTime(115))
        );
        assert_eq!(parse("ResetHUD", &[]), Some(UserMessage::ResetHud));
        assert_eq!(parse("InitHUD", &[]), Some(UserMessage::InitHud));
        assert_eq!(parse("BombPickup", &[]), Some(UserMessage::BombPickup));
        assert_eq!(
            parse("HostageK", &[2]),
            Some(UserMessage::HostageK(HostageK { index: 2 }))
        );
        assert_eq!(
            parse("AmmoX", &[3, 90]),
            Some(UserMessage::AmmoX {
                index: 3,
                count: 90
            })
        );
        assert_eq!(
            parse("AmmoPickup", &[3, 30]),
            Some(UserMessage::AmmoPickup {
                index: 3,
                count: 30
            })
        );
    }

    #[test]
    fn itemstatus_bits_match_cdll_dll_h() {
        assert_eq!(ItemStatus::NIGHTVISION, 1);
        assert_eq!(ItemStatus::DEFUSER, 2);
        match parse("ItemStatus", &[3]) {
            Some(UserMessage::ItemStatus(s)) => {
                assert!(s.has_nightvision() && s.has_defuser());
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse("ItemStatus", &[2]) {
            Some(UserMessage::ItemStatus(s)) => {
                assert!(!s.has_nightvision() && s.has_defuser());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bartime_and_bartime2_decode() {
        assert_eq!(
            parse("BarTime", &Msg::new().short(3).done()),
            Some(UserMessage::BarTime(BarTime { seconds: 3 }))
        );
        assert_eq!(
            parse("BarTime2", &Msg::new().short(10).short(50).done()),
            Some(UserMessage::BarTime2(BarTime2 {
                duration: 10,
                elapsed_percent: 50
            }))
        );
    }

    #[test]
    fn scoreinfo_decodes_all_five_shorts() {
        let p = Msg::new()
            .byte(2)
            .short(14)
            .short(9)
            .short(0)
            .short(2)
            .done();
        assert_eq!(p.len(), 9, "ScoreInfo is registered as 9 bytes");
        match parse("ScoreInfo", &p) {
            Some(UserMessage::ScoreInfo(s)) => {
                assert_eq!(s.client, 2);
                assert_eq!(s.frags, 14);
                assert_eq!(s.deaths, 9);
                assert_eq!(s.class, 0);
                assert_eq!(s.team, Team::CounterTerrorist);
                assert_eq!(s.team_id, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn negative_frags_survive_as_a_signed_short() {
        let p = Msg::new()
            .byte(1)
            .short(-3)
            .short(0)
            .short(0)
            .short(1)
            .done();
        match parse("ScoreInfo", &p) {
            Some(UserMessage::ScoreInfo(s)) => assert_eq!(s.frags, -3),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn screenfade_decodes_ten_bytes() {
        let p = Msg::new()
            .short(4096)
            .short(1024)
            .short(1)
            .byte(0)
            .byte(0)
            .byte(0)
            .byte(255)
            .done();
        assert_eq!(p.len(), 10);
        match parse("ScreenFade", &p) {
            Some(UserMessage::ScreenFade(f)) => {
                assert_eq!(f.duration, 4096);
                assert_eq!(f.hold, 1024);
                assert_eq!(f.flags, 1);
                assert_eq!((f.r, f.g, f.b, f.a), (0, 0, 0, 255));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- variable-length messages ------------------------------------------

    #[test]
    fn weaponlist_decodes_a_full_entry() {
        // dlls/client.cpp:256-266.
        let p = Msg::new()
            .string("weapon_ak47")
            .byte(2)
            .byte(90)
            .byte(255)
            .byte(255)
            .byte(0)
            .byte(1)
            .byte(28)
            .byte(0)
            .done();
        match parse("WeaponList", &p) {
            Some(UserMessage::WeaponList(w)) => {
                assert_eq!(w.name, "weapon_ak47");
                assert_eq!(w.ammo1_index, 2);
                assert_eq!(w.max_ammo1, 90);
                assert_eq!(w.slot, 0);
                assert_eq!(w.position, 1);
                assert_eq!(w.weapon_id, 28);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deathmsg_decodes_the_four_base_fields() {
        let p = Msg::new().byte(3).byte(7).byte(1).string("ak47").done();
        match parse("DeathMsg", &p) {
            Some(UserMessage::DeathMsg(d)) => {
                assert_eq!(d.killer, 3);
                assert_eq!(d.victim, 7);
                assert!(d.headshot);
                assert_eq!(d.weapon, "ak47");
                assert_eq!(d.flags, None);
                assert_eq!(d.position, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn deathmsg_decodes_the_regamedll_extension() {
        // PLAYERDEATH_POSITION | PLAYERDEATH_ASSISTANT | PLAYERDEATH_KILLRARITY
        let flags = DeathMsg::F_POSITION | DeathMsg::F_ASSISTANT | DeathMsg::F_KILLRARITY;
        let p = Msg::new()
            .byte(3)
            .byte(7)
            .byte(1)
            .string("ak47")
            .long(flags)
            .coord(-256.0)
            .coord(128.5)
            .coord(-32.0)
            .byte(4)
            .long(0x11)
            .done();
        match parse("DeathMsg", &p) {
            Some(UserMessage::DeathMsg(d)) => {
                assert_eq!(d.flags, Some(flags));
                assert_eq!(d.position, Some([-256.0, 128.5, -32.0]));
                assert_eq!(d.assistant, Some(4));
                assert_eq!(d.rarity, Some(0x11));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_clipped_deathmsg_extension_keeps_the_base_fields() {
        // Flags claim a position, but the coords are missing.
        let p = Msg::new()
            .byte(3)
            .byte(7)
            .byte(0)
            .string("knife")
            .long(DeathMsg::F_POSITION)
            .done();
        match parse("DeathMsg", &p) {
            Some(UserMessage::DeathMsg(d)) => {
                assert_eq!(d.victim, 7);
                assert_eq!(d.weapon, "knife");
                assert_eq!(d.flags, None, "extension dropped whole");
                assert_eq!(d.position, None);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn textmsg_reads_the_destination_and_every_parameter_written() {
        let p = Msg::new()
            .byte(4)
            .string("#Bomb_Planted")
            .done();
        match parse("TextMsg", &p) {
            Some(UserMessage::TextMsg(t)) => {
                assert_eq!(t.dest, TextDest::Center);
                assert_eq!(t.message, "#Bomb_Planted");
                assert!(t.params.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }

        let with_params = Msg::new()
            .byte(3)
            .string("#Game_join_ct")
            .string("Bravo")
            .done();
        match parse("TextMsg", &with_params) {
            Some(UserMessage::TextMsg(t)) => {
                assert_eq!(t.dest, TextDest::Talk);
                assert_eq!(t.params, vec!["Bravo".to_string()]);
            }
            other => panic!("unexpected {other:?}"),
        }

        // A mod's own destination is kept rather than dropped.
        let odd = Msg::new().byte(200).string("hi").done();
        match parse("TextMsg", &odd) {
            Some(UserMessage::TextMsg(t)) => assert_eq!(t.dest, TextDest::Other(200)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn scenario_decodes_both_the_short_and_the_long_form() {
        // ggrenade.cpp:1394-1400 — the ticking bomb, with interval and offset.
        let ticking = Msg::new()
            .byte(1)
            .string("bombticking")
            .byte(255)
            .short(1000)
            .short(250)
            .done();
        match parse("Scenario", &ticking) {
            Some(UserMessage::Scenario(s)) => {
                let icon = s.icon.expect("active");
                assert_eq!(icon.sprite, "bombticking");
                assert_eq!(icon.flags, 255);
                assert_eq!(icon.interval, Some(1000));
                assert_eq!(icon.offset, Some(250));
            }
            other => panic!("unexpected {other:?}"),
        }

        // player.cpp:7520-7524 — hostage count, no interval.
        let hostages = Msg::new().byte(1).string("hostage4").byte(0).done();
        match parse("Scenario", &hostages) {
            Some(UserMessage::Scenario(s)) => {
                let icon = s.icon.expect("active");
                assert_eq!(icon.sprite, "hostage4");
                assert_eq!(icon.interval, None);
            }
            other => panic!("unexpected {other:?}"),
        }

        // ggrenade.cpp:1129-1131 — off.
        match parse("Scenario", &[0]) {
            Some(UserMessage::Scenario(s)) => assert!(s.icon.is_none()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn showmenu_and_vguimenu_decode() {
        let p = Msg::new()
            .short(0b0000_0111)
            .char(-1)
            .byte(0)
            .string("Choose a team")
            .done();
        match parse("ShowMenu", &p) {
            Some(UserMessage::ShowMenu(m)) => {
                assert_eq!(m.valid_slots, 0b111);
                assert_eq!(m.display_time, -1, "WRITE_CHAR is signed");
                assert!(!m.need_more);
                assert_eq!(m.text, "Choose a team");
            }
            other => panic!("unexpected {other:?}"),
        }

        // dlls/client.cpp:420-426.
        let v = Msg::new().byte(2).short(0).char(-1).byte(0).string(" ").done();
        match parse("VGUIMenu", &v) {
            Some(UserMessage::VguiMenu(m)) => {
                assert_eq!(m.menu_type, 2);
                assert_eq!(m.display_time, -1);
                assert_eq!(m.text, " ");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- bounds safety -----------------------------------------------------

    #[test]
    fn every_fixed_size_message_rejects_a_truncated_payload() {
        // (name, a well-formed payload) — every proper prefix must be refused.
        let full: Vec<(&str, Vec<u8>)> = vec![
            ("CurWeapon", vec![1, 28, 30]),
            ("Health", vec![100]),
            ("Battery", Msg::new().short(100).done()),
            ("AmmoX", vec![2, 90]),
            ("AmmoPickup", vec![2, 30]),
            ("Money", Msg::new().long(800).byte(0).done()),
            (
                "ScoreInfo",
                Msg::new().byte(1).short(0).short(0).short(0).short(1).done(),
            ),
            ("ScoreAttrib", vec![1, 2]),
            ("RoundTime", Msg::new().short(115).done()),
            ("BarTime", Msg::new().short(5).done()),
            ("BarTime2", Msg::new().short(5).short(10).done()),
            (
                "BombDrop",
                Msg::new().coord(1.0).coord(2.0).coord(3.0).byte(1).done(),
            ),
            (
                "HostagePos",
                Msg::new()
                    .byte(1)
                    .byte(1)
                    .coord(1.0)
                    .coord(2.0)
                    .coord(3.0)
                    .done(),
            ),
            ("HostageK", vec![1]),
            ("ItemStatus", vec![3]),
            (
                "Radar",
                Msg::new().byte(1).coord(1.0).coord(2.0).coord(3.0).done(),
            ),
            ("SetFOV", vec![90]),
            (
                "ScreenFade",
                Msg::new()
                    .short(1)
                    .short(1)
                    .short(1)
                    .byte(1)
                    .byte(2)
                    .byte(3)
                    .byte(4)
                    .done(),
            ),
        ];

        for (name, payload) in &full {
            assert!(
                parse(name, payload).is_some(),
                "{name}: the full payload must parse"
            );
            for cut in 0..payload.len() {
                assert_eq!(
                    parse(name, &payload[..cut]),
                    None,
                    "{name}: {cut} of {} bytes must not decode",
                    payload.len()
                );
            }
        }
    }

    #[test]
    fn variable_length_messages_reject_a_missing_terminator_or_a_short_tail() {
        // No NUL at all.
        assert_eq!(parse("TeamInfo", b"\x01CT"), None);
        assert_eq!(parse("StatusIcon", b"\x01buyzone"), None);
        // Terminated but the fixed tail is short.
        let short_list = Msg::new().string("weapon_ak47").byte(2).byte(90).done();
        assert_eq!(parse("WeaponList", &short_list), None);
        // TeamInfo with no client byte at all.
        assert_eq!(parse("TeamInfo", &[]), None);
        assert_eq!(parse("Scenario", &[]), None);
        // Active scenario with nothing after the flag.
        assert_eq!(parse("Scenario", &[1]), None);
        // ShowMenu missing its text.
        assert_eq!(parse("ShowMenu", &Msg::new().short(1).char(0).byte(0).done()), None);
    }

    #[test]
    fn an_empty_payload_never_panics_for_any_name_we_model() {
        for name in [
            "ResetHUD",
            "InitHUD",
            "CurWeapon",
            "Health",
            "Battery",
            "AmmoX",
            "AmmoPickup",
            "WeaponList",
            "Money",
            "ScoreInfo",
            "TeamInfo",
            "ScoreAttrib",
            "DeathMsg",
            "RoundTime",
            "StatusIcon",
            "BarTime",
            "BarTime2",
            "TextMsg",
            "BombDrop",
            "BombPickup",
            "HostagePos",
            "HostageK",
            "ItemStatus",
            "Radar",
            "SetFOV",
            "ScreenFade",
            "Scenario",
            "ShowMenu",
            "VGUIMenu",
        ] {
            let _ = parse(name, &[]);
            let _ = parse(name, &[0xFF; 1]);
            let _ = parse(name, &[0xFF; 64]);
            let _ = parse(name, &[0x00; 64]);
        }
    }

    #[test]
    fn an_unmodelled_name_is_none() {
        assert_eq!(parse("SayText", &[1, 2, 3]), None);
        assert_eq!(parse("VoiceMask", &[0; 16]), None);
        assert_eq!(parse("", &[]), None);
        // Case matters: the registration names are exact.
        assert_eq!(parse("teaminfo", &Msg::new().byte(1).string("CT").done()), None);
    }

    // --- game state --------------------------------------------------------

    #[test]
    fn apply_on_an_unknown_name_is_a_no_op() {
        let mut gs = GameState::new();
        let before = gs.clone();
        gs.apply("SayText", &[1, 2, 3, 4]);
        gs.apply("Geiger", &[7]);
        gs.apply("NotARealMessage", b"whatever");
        // And a known name with a broken payload.
        gs.apply("Money", &[1, 2]);
        assert_eq!(gs, before);
    }

    #[test]
    fn a_realistic_spawn_sequence_produces_the_expected_state() {
        let mut gs = GameState::new();
        gs.self_index = Some(2);

        gs.apply("ResetHUD", &[]);
        gs.apply("Money", &Msg::new().long(800).byte(0).done());
        gs.apply("Health", &[100]);
        gs.apply("TeamInfo", &Msg::new().byte(2).string("CT").done());
        gs.apply("RoundTime", &Msg::new().short(115).done());
        gs.apply(
            "StatusIcon",
            &Msg::new()
                .byte(1)
                .string("buyzone")
                .byte(0)
                .byte(160)
                .byte(0)
                .done(),
        );

        assert_eq!(gs.hud_resets, 1);
        assert_eq!(gs.money, 800);
        assert_eq!(gs.health, 100);
        assert_eq!(gs.round_time, 115);
        assert!(gs.in_buy_zone);
        assert!(!gs.in_rescue_zone);
        assert_eq!(gs.my_team(), Team::CounterTerrorist);
        assert_eq!(gs.player(2).map(|p| p.team), Some(Team::CounterTerrorist));
        assert!(gs.player(2).map(|p| p.seen).unwrap_or(false));
        assert!(!gs.player(3).map(|p| p.seen).unwrap_or(true), "slot 3 unseen");

        // Leaving the buy zone clears it.
        gs.apply("StatusIcon", &Msg::new().byte(0).string("buyzone").done());
        assert!(!gs.in_buy_zone);
    }

    #[test]
    fn resethud_clears_the_per_round_flags_but_keeps_teams_scores_and_money() {
        let mut gs = GameState::new();
        gs.apply("TeamInfo", &Msg::new().byte(4).string("TERRORIST").done());
        gs.apply(
            "ScoreInfo",
            &Msg::new().byte(4).short(11).short(5).short(0).short(1).done(),
        );
        gs.apply("Money", &Msg::new().long(4200).byte(1).done());
        gs.apply(
            "ScoreAttrib",
            &Msg::new()
                .byte(4)
                .byte(ScoreAttrib::DEAD | ScoreAttrib::BOMB)
                .done(),
        );
        gs.apply("ItemStatus", &[ItemStatus::DEFUSER]);
        gs.apply("RoundTime", &Msg::new().short(30).done());
        gs.apply(
            "BombDrop",
            &Msg::new()
                .coord(1264.0)
                .coord(2586.0)
                .coord(-127.0)
                .byte(1)
                .done(),
        );
        gs.apply("BarTime", &Msg::new().short(5).done());
        gs.apply(
            "StatusIcon",
            &Msg::new().byte(1).string("rescue").byte(0).byte(160).byte(0).done(),
        );
        gs.apply(
            "HostagePos",
            &Msg::new()
                .byte(1)
                .byte(1)
                .coord(10.0)
                .coord(20.0)
                .coord(30.0)
                .done(),
        );

        assert!(gs.bomb_planted && gs.bomb_position.is_some());
        assert!(gs.in_rescue_zone);
        assert_eq!(gs.bar_time, Some(5));
        assert!(gs.has_defuser());
        assert_eq!(gs.hostages.len(), 1);
        assert_eq!(gs.player(4).map(|p| p.dead), Some(true));
        assert_eq!(gs.player(4).map(|p| p.has_bomb), Some(true));

        gs.apply("ResetHUD", &[]);

        // Gone.
        assert!(!gs.bomb_planted);
        assert_eq!(gs.bomb_position, None);
        assert!(!gs.in_rescue_zone);
        assert_eq!(gs.bar_time, None);
        assert!(!gs.has_defuser());
        assert!(gs.hostages.is_empty());
        assert_eq!(gs.round_time, 0);
        assert_eq!(gs.player(4).map(|p| p.dead), Some(false));
        assert_eq!(gs.player(4).map(|p| p.has_bomb), Some(false));

        // Kept.
        assert_eq!(gs.money, 4200);
        assert_eq!(gs.player(4).map(|p| p.team), Some(Team::Terrorist));
        assert_eq!(gs.player(4).map(|p| p.frags), Some(11));
        assert_eq!(gs.player(4).map(|p| p.deaths), Some(5));
        assert_eq!(gs.hud_resets, 1);
    }

    #[test]
    fn the_bomb_position_ignores_the_hide_timer_hack() {
        let mut gs = GameState::new();
        // A real plant.
        gs.apply(
            "BombDrop",
            &Msg::new().coord(1264.0).coord(2586.0).coord(-127.0).byte(1).done(),
        );
        assert_eq!(gs.bomb_position, Some([1264.0, 2586.0, -127.0]));

        // CBasePlayer::HideTimer, dlls/player.cpp:10665-10671.
        gs.apply(
            "BombDrop",
            &Msg::new().coord(0.0).coord(0.0).coord(0.0).byte(1).done(),
        );
        assert!(gs.bomb_planted, "still planted");
        assert_eq!(gs.bomb_position, None, "the origin is not a bomb site");
    }

    #[test]
    fn team_membership_can_be_enumerated() {
        let mut gs = GameState::new();
        gs.apply("TeamInfo", &Msg::new().byte(1).string("CT").done());
        gs.apply("TeamInfo", &Msg::new().byte(2).string("TERRORIST").done());
        gs.apply("TeamInfo", &Msg::new().byte(3).string("CT").done());
        gs.apply("TeamInfo", &Msg::new().byte(32).string("SPECTATOR").done());

        let cts: Vec<u8> = gs
            .team_members(Team::CounterTerrorist)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(cts, vec![1, 3]);
        assert_eq!(gs.known_players().count(), 4);
        assert_eq!(gs.player(32).map(|p| p.team), Some(Team::Spectator));
    }

    #[test]
    fn out_of_range_client_slots_are_ignored_rather_than_panicking() {
        let mut gs = GameState::new();
        // Slot 0 is worldspawn, slot 200 is off the end of MAX_CLIENTS.
        gs.apply("TeamInfo", &Msg::new().byte(0).string("CT").done());
        gs.apply("TeamInfo", &Msg::new().byte(200).string("CT").done());
        gs.apply("ScoreAttrib", &[200, 0xFF]);
        gs.apply(
            "Radar",
            &Msg::new().byte(255).coord(1.0).coord(2.0).coord(3.0).done(),
        );
        assert_eq!(gs.known_players().count(), 0);
        assert_eq!(gs.player(0), None);
        assert_eq!(gs.player(200), None);
    }

    #[test]
    fn an_out_of_range_ammo_index_is_ignored() {
        let mut gs = GameState::new();
        gs.apply("AmmoX", &[2, 90]);
        gs.apply("AmmoX", &[31, 30]);
        gs.apply("AmmoX", &[200, 77]); // MAX_AMMO_SLOTS is 32
        assert_eq!(gs.ammo[2], 90);
        assert_eq!(gs.ammo[31], 30);
        assert!(gs.ammo.iter().filter(|&&v| v == 77).count() == 0);
    }

    #[test]
    fn a_deathmsg_marks_the_victim_dead() {
        let mut gs = GameState::new();
        gs.apply("TeamInfo", &Msg::new().byte(5).string("CT").done());
        gs.apply("DeathMsg", &Msg::new().byte(1).byte(5).byte(1).string("awp").done());
        assert_eq!(gs.player(5).map(|p| p.dead), Some(true));
    }

    #[test]
    fn a_zero_bartime_clears_the_progress_bar() {
        let mut gs = GameState::new();
        gs.apply("BarTime", &Msg::new().short(3).done());
        assert!(gs.bar_running());
        gs.apply("BarTime", &Msg::new().short(0).done());
        assert!(!gs.bar_running());
        assert_eq!(gs.bar_time2, None);
    }

    #[test]
    fn radar_records_a_teammates_last_known_position() {
        let mut gs = GameState::new();
        gs.apply("TeamInfo", &Msg::new().byte(6).string("CT").done());
        gs.apply(
            "Radar",
            &Msg::new().byte(6).coord(-544.0).coord(1280.5).coord(-63.875).done(),
        );
        assert_eq!(
            gs.player(6).and_then(|p| p.radar_position),
            Some([-544.0, 1280.5, -63.875])
        );
    }

    #[test]
    fn hostage_updates_accumulate_and_a_kill_removes_one() {
        let mut gs = GameState::new();
        for i in 1..=4u8 {
            gs.apply(
                "HostagePos",
                &Msg::new()
                    .byte(1)
                    .byte(i)
                    .coord(f32::from(i) * 100.0)
                    .coord(0.0)
                    .coord(-64.0)
                    .done(),
            );
        }
        assert_eq!(gs.hostages.len(), 4);
        assert_eq!(gs.hostages[&2], [200.0, 0.0, -64.0]);

        // Moved.
        gs.apply(
            "HostagePos",
            &Msg::new().byte(0).byte(2).coord(250.0).coord(0.0).coord(-64.0).done(),
        );
        assert_eq!(gs.hostages[&2], [250.0, 0.0, -64.0]);

        gs.apply("HostageK", &[2]);
        assert_eq!(gs.hostages.len(), 3);
        assert_eq!(gs.hostages_killed, 1);
    }

    #[test]
    fn the_active_weapon_is_tracked_but_the_death_clear_is_not_mistaken_for_one() {
        let mut gs = GameState::new();
        gs.apply("CurWeapon", &[1, 28, 30]);
        assert_eq!(gs.weapon_id, 28);
        assert_eq!(gs.weapon_clip, 30);

        // A holstered weapon's update is not our active weapon.
        gs.apply("CurWeapon", &[0, 18, 12]);
        assert_eq!(gs.weapon_id, 28, "state 0 is some other item in the bag");

        gs.apply("CurWeapon", &[0x40, 28, 29]);
        assert_eq!(gs.weapon_clip, 29, "on-target still counts as active");
    }

    #[test]
    fn the_scenario_sprite_is_tracked_and_cleared() {
        let mut gs = GameState::new();
        gs.apply(
            "Scenario",
            &Msg::new().byte(1).string("bombticking").byte(255).short(1000).short(0).done(),
        );
        assert_eq!(gs.scenario_sprite.as_deref(), Some("bombticking"));
        gs.apply("Scenario", &[0]);
        assert_eq!(gs.scenario_sprite, None);
    }

    #[test]
    fn encode_coord_is_the_inverse_of_the_decoder() {
        for v in [0.0f32, 0.125, -0.125, 1.0, -1.0, 4095.875, -4096.0, 1264.5] {
            let bytes = encode_coord(v);
            let mut c = Cur::new(&bytes);
            assert_eq!(c.coord(), Some(v), "coord {v}");
        }
    }
}
