//! The CS 1.6 weapon table, transcribed from ReGameDLL.
//!
//! Everything in this file is a direct transcription with a `file:line`
//! citation, not a guess. The reference tree is `regamedll/` (ReGameDLL_CS);
//! paths below are relative to `regamedll/regamedll/`.
//!
//! ## What actually matters to a bot
//!
//! The single behaviour-defining fact is the **fire class**, and it is not a
//! design choice — it falls straight out of which weapons touch
//! `m_iShotsFired`:
//!
//! * The six pistols do `if (++m_iShotsFired > 1) return;` as the *first* thing
//!   in their fire routine (`dlls/wpn_shared/wpn_usp.cpp:164`,
//!   `wpn_p228.cpp:105`, `wpn_deagle.cpp:106`, `wpn_elite.cpp:108`,
//!   `wpn_fiveseven.cpp:105`, `wpn_glock18.cpp:167`). The counter is only
//!   zeroed in `CBasePlayerWeapon::ItemPostFrame`'s "no fire buttons down"
//!   branch (`dlls/weapons.cpp:1114`, reset at `:1136-1140`). Holding
//!   `IN_ATTACK` therefore fires **exactly one bullet, ever** — the trigger has
//!   to be released between every shot.
//! * Rifles and SMGs increment `m_iShotsFired` without that early return and
//!   feed it into spread: `m_flAccuracy = shots³ / DIVISOR + 0.35`
//!   (`dlls/wpn_shared/wpn_ak47.cpp:97`). Holding works but walks the spread
//!   out, so the bot bursts.
//! * Six weapons **never mention `m_iShotsFired` at all** — verified by
//!   `grep -L m_iShotsFired dlls/wpn_shared/*.cpp`, which returns awp, g3sg1,
//!   m3, scout, sg550, xm1014 (plus c4, knife and the three grenades). They are
//!   purely `m_flNextPrimaryAttack`-gated, so holding is correct and
//!   auto-repeats at the cycle time.
//!
//! ## One correction to the usual story
//!
//! `IsPistol()` has two implementations. The legacy one is the `m_iId` list of
//! six at `dlls/weapons.h:401`. Under `REGAMEDLL_FIXES` — which is how release
//! builds ship — it is a virtual overridden per class, and the set is those six
//! **plus `CFlashbang`** (`dlls/weapons.h:999`, which carries the source's own
//! `// TODO: why the object flashbang is IsPistol?`). It makes no observable
//! difference, because a flashbang never increments `m_iShotsFired`, but the
//! "six weapons" claim is only exactly true of the legacy build.

/// Every weapon id the game defines, with ReGameDLL's numbering.
///
/// Transcribed from `enum WeaponIdType`, `dlls/weapontype.h:31-63`. Note the
/// two glocks: `WEAPON_GLOCK` (2) is a legacy duplicate and `WEAPON_GLOCK18`
/// (17) is what players actually carry — the buy alias `glock` resolves to
/// `WEAPON_GLOCK18` (`dlls/weapontype.cpp:148`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum WeaponId {
    None = 0,
    P228 = 1,
    /// Legacy duplicate of [`WeaponId::Glock18`]; see the type docs.
    Glock = 2,
    Scout = 3,
    HeGrenade = 4,
    Xm1014 = 5,
    C4 = 6,
    Mac10 = 7,
    Aug = 8,
    SmokeGrenade = 9,
    Elite = 10,
    FiveSeven = 11,
    Ump45 = 12,
    Sg550 = 13,
    Galil = 14,
    Famas = 15,
    Usp = 16,
    Glock18 = 17,
    Awp = 18,
    Mp5n = 19,
    M249 = 20,
    M3 = 21,
    M4a1 = 22,
    Tmp = 23,
    G3sg1 = 24,
    Flashbang = 25,
    Deagle = 26,
    Sg552 = 27,
    Ak47 = 28,
    Knife = 29,
    P90 = 30,
    ShieldGun = 99,
}

/// How the trigger has to be worked for a weapon to keep firing.
///
/// Derived from `m_iShotsFired` handling, not chosen — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FireClass {
    /// `IN_ATTACK` must be **released between every shot**. The six pistols.
    SemiPistol,
    /// Hold `IN_ATTACK`, but accuracy degrades with `m_iShotsFired`, so burst.
    FullAuto,
    /// Never touches `m_iShotsFired`; purely `m_flNextPrimaryAttack`-gated, so
    /// holding is correct and auto-repeats. Snipers and the two shotguns.
    TimerOnly,
    /// The knife. Also never touches `m_iShotsFired`, so mechanically identical
    /// to [`FireClass::TimerOnly`]; kept separate because it has no ammo and no
    /// range and the AI must not treat it as a gun.
    Melee,
    /// Grenades: primary *pulls the pin*, release throws. Not a trigger at all.
    Thrown,
    /// The C4: `IN_ATTACK` held for `C4_ARMING_ON_TIME`; see
    /// [`crate::objective::bomb`].
    Bomb,
    /// Not a firing weapon (`WEAPON_NONE`, the shield).
    NotAWeapon,
}

impl WeaponId {
    /// Map a wire weapon id onto the enum.
    ///
    /// `CurWeapon` and `weapon_data_t` both carry the raw `WeaponIdType`
    /// (`dlls/weapons.h`), and this is the only way to know what the bot is
    /// actually holding: `usercmd_t.weaponselect` is never read by ReGameDLL,
    /// and `clientdata_t.weapons` is not transmitted at all -- dumping every
    /// field that arrives while playing shows eleven, and `weapons` is not
    /// among them.
    ///
    /// An unknown id becomes [`WeaponId::None`] rather than a panic: a server
    /// running a mod weapon must make the bot hold fire, not crash it.
    pub fn from_id(id: u8) -> Self {
        match id {
            0 => Self::None,
            1 => Self::P228,
            2 => Self::Glock,
            3 => Self::Scout,
            4 => Self::HeGrenade,
            5 => Self::Xm1014,
            6 => Self::C4,
            7 => Self::Mac10,
            8 => Self::Aug,
            9 => Self::SmokeGrenade,
            10 => Self::Elite,
            11 => Self::FiveSeven,
            12 => Self::Ump45,
            13 => Self::Sg550,
            14 => Self::Galil,
            15 => Self::Famas,
            16 => Self::Usp,
            17 => Self::Glock18,
            18 => Self::Awp,
            19 => Self::Mp5n,
            20 => Self::M249,
            21 => Self::M3,
            22 => Self::M4a1,
            23 => Self::Tmp,
            24 => Self::G3sg1,
            25 => Self::Flashbang,
            26 => Self::Deagle,
            27 => Self::Sg552,
            28 => Self::Ak47,
            29 => Self::Knife,
            30 => Self::P90,
            99 => Self::ShieldGun,
            _ => Self::None,
        }
    }
}

impl FireClass {
    /// True when the bot may leave `IN_ATTACK` held down across ticks.
    ///
    /// False for [`FireClass::SemiPistol`] — holding it fires exactly one
    /// bullet and then silently does nothing (`dlls/wpn_shared/wpn_usp.cpp:164`).
    pub fn may_hold(self) -> bool {
        !matches!(self, FireClass::SemiPistol)
    }

    /// True when spread grows with `m_iShotsFired`, so the bot should burst.
    pub fn degrades_with_shots(self) -> bool {
        matches!(self, FireClass::FullAuto)
    }
}

/// Which inventory slot a weapon occupies — what a `weapon_*` switch competes
/// with, and what a buy replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    Primary,
    Secondary,
    Knife,
    Grenade,
    C4,
    Other,
}

/// Everything the AI needs to know about one weapon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeaponInfo {
    pub id: WeaponId,
    /// The `weapon_*` entity name. Sending it as a console command is a
    /// **switch**: `client.cpp:3560-3563` routes any command starting with
    /// `weapon_` to `SelectItem`. It is *not* a purchase.
    pub entity: &'static str,
    /// The buy alias, when the weapon can be bought. This is the bare word
    /// (`ak47`), from `g_weaponBuyAliasInfo`, `dlls/weapontype.cpp:125-170`.
    /// `None` for the knife, the C4 and the shield.
    pub buy_alias: Option<&'static str>,
    /// `GetMaxSpeed()` while carrying it, from the `*_MAX_SPEED` constants in
    /// `dlls/weapons.h`. Un-zoomed value; the snipers drop further when zoomed.
    pub max_speed: f32,
    /// `*_MAX_CLIP` from `dlls/weapontype.h:190-213`. `0` means the weapon has
    /// no clip (`WEAPON_NOCLIP` in the table at `dlls/weapontype.cpp:250-256`).
    pub clip: i32,
    pub fire_class: FireClass,
    pub slot: Slot,
}

impl WeaponInfo {
    /// The console command that switches to this weapon.
    ///
    /// `usercmd_t.weaponselect` is a dead field — its only occurrences in
    /// either tree are the struct declaration (`common/usercmd.h:48`) and the
    /// delta table (`rehlds/engine/delta.cpp:221`). Nothing ever reads it for
    /// logic, so a console command is the only way to change weapon.
    pub fn select_command(&self) -> &'static str {
        self.entity
    }
}

const fn w(
    id: WeaponId,
    entity: &'static str,
    buy_alias: Option<&'static str>,
    max_speed: f32,
    clip: i32,
    fire_class: FireClass,
    slot: Slot,
) -> WeaponInfo {
    WeaponInfo { id, entity, buy_alias, max_speed, clip, fire_class, slot }
}

use FireClass::*;
use Slot::*;
use WeaponId as W;

/// The full table.
///
/// Entity names come from `g_weaponInfo_default` (`dlls/weapontype.cpp:222-262`),
/// clips from `dlls/weapontype.h:190-213`, speeds from the `*_MAX_SPEED`
/// constants in `dlls/weapons.h`, buy aliases from `g_weaponBuyAliasInfo`
/// (`dlls/weapontype.cpp:125-170`).
pub const WEAPONS: &[WeaponInfo] = &[
    // --- pistols: IN_ATTACK must be released between every shot ---
    w(W::P228, "weapon_p228", Some("p228"), 250.0, 13, SemiPistol, Secondary),
    w(W::Glock18, "weapon_glock18", Some("glock"), 250.0, 20, SemiPistol, Secondary),
    // The legacy duplicate resolves to the same entity under REGAMEDLL_FIXES
    // it gets its own "weapon_glock" (dlls/weapontype.cpp:227).
    w(W::Glock, "weapon_glock", Some("glock"), 250.0, 20, SemiPistol, Secondary),
    w(W::Usp, "weapon_usp", Some("usp"), 250.0, 12, SemiPistol, Secondary),
    w(W::Deagle, "weapon_deagle", Some("deagle"), 250.0, 7, SemiPistol, Secondary),
    // The Beretta's alias is `elites`, not `elite` — `elite` is only a *weapon*
    // alias, not a buy alias (dlls/weapontype.cpp:118 vs :156).
    w(W::Elite, "weapon_elite", Some("elites"), 250.0, 30, SemiPistol, Secondary),
    w(W::FiveSeven, "weapon_fiveseven", Some("fiveseven"), 250.0, 20, SemiPistol, Secondary),
    // --- full auto: hold, but spread grows with m_iShotsFired ---
    w(W::Ak47, "weapon_ak47", Some("ak47"), 221.0, 30, FullAuto, Primary),
    w(W::M4a1, "weapon_m4a1", Some("m4a1"), 230.0, 30, FullAuto, Primary),
    w(W::Sg552, "weapon_sg552", Some("sg552"), 235.0, 30, FullAuto, Primary),
    w(W::Aug, "weapon_aug", Some("aug"), 240.0, 30, FullAuto, Primary),
    w(W::Galil, "weapon_galil", Some("galil"), 240.0, 35, FullAuto, Primary),
    w(W::Famas, "weapon_famas", Some("famas"), 240.0, 25, FullAuto, Primary),
    w(W::M249, "weapon_m249", Some("m249"), 220.0, 100, FullAuto, Primary),
    w(W::P90, "weapon_p90", Some("p90"), 245.0, 50, FullAuto, Primary),
    w(W::Mp5n, "weapon_mp5navy", Some("mp5"), 250.0, 30, FullAuto, Primary),
    w(W::Mac10, "weapon_mac10", Some("mac10"), 250.0, 30, FullAuto, Primary),
    w(W::Tmp, "weapon_tmp", Some("tmp"), 250.0, 30, FullAuto, Primary),
    w(W::Ump45, "weapon_ump45", Some("ump45"), 250.0, 25, FullAuto, Primary),
    // --- timer only: never touches m_iShotsFired, holding auto-repeats ---
    w(W::Awp, "weapon_awp", Some("awp"), 210.0, 10, TimerOnly, Primary),
    w(W::Scout, "weapon_scout", Some("scout"), 260.0, 10, TimerOnly, Primary),
    w(W::G3sg1, "weapon_g3sg1", Some("g3sg1"), 210.0, 20, TimerOnly, Primary),
    w(W::Sg550, "weapon_sg550", Some("sg550"), 210.0, 30, TimerOnly, Primary),
    w(W::M3, "weapon_m3", Some("m3"), 230.0, 8, TimerOnly, Primary),
    w(W::Xm1014, "weapon_xm1014", Some("xm1014"), 240.0, 7, TimerOnly, Primary),
    // --- the rest ---
    w(W::Knife, "weapon_knife", None, 250.0, 0, Melee, Knife),
    w(W::C4, "weapon_c4", None, 250.0, 0, Bomb, Slot::C4),
    w(W::HeGrenade, "weapon_hegrenade", Some("hegren"), 250.0, 0, Thrown, Grenade),
    w(W::Flashbang, "weapon_flashbang", Some("flash"), 250.0, 0, Thrown, Grenade),
    w(W::SmokeGrenade, "weapon_smokegrenade", Some("sgren"), 250.0, 0, Thrown, Grenade),
    w(W::ShieldGun, "weapon_shield", Some("shield"), 250.0, 0, NotAWeapon, Other),
];

/// Look a weapon up. `None` for [`WeaponId::None`] and anything unlisted.
pub fn info(id: WeaponId) -> Option<&'static WeaponInfo> {
    WEAPONS.iter().find(|w| w.id == id)
}

/// The fire class, defaulting to [`FireClass::NotAWeapon`] for the unknown.
///
/// Defaulting *away* from firing is deliberate: a bot that does not know what
/// it is holding should not pull the trigger.
pub fn fire_class(id: WeaponId) -> FireClass {
    info(id).map_or(FireClass::NotAWeapon, |w| w.fire_class)
}

/// Carry speed in units/sec, or the 250 default when unknown.
pub fn max_speed(id: WeaponId) -> f32 {
    info(id).map_or(250.0, |w| w.max_speed)
}

/// Things you can buy that are not weapons.
///
/// The aliases are the `FStrEq(pszCommand, ...)` ladder in
/// `HandleBuyAliasCommands`, `dlls/client.cpp:2503-2590`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Equipment {
    /// Primary ammo (`dlls/client.cpp:2503`).
    PrimaryAmmo,
    /// Secondary ammo (`:2522`).
    SecondaryAmmo,
    /// Kevlar (`:2541`).
    Vest,
    /// Kevlar + helmet (`:2546`).
    VestHelm,
    /// Flashbang (`:2551`).
    Flashbang,
    /// HE grenade (`:2556`).
    HeGrenade,
    /// Smoke grenade (`:2561`).
    SmokeGrenade,
    /// Nightvision (`:2566`).
    Nvgs,
    /// Defuse kit, CT only (`:2571`).
    Defuser,
    /// Tactical shield (`:2584`).
    Shield,
}

impl Equipment {
    pub fn alias(self) -> &'static str {
        match self {
            Equipment::PrimaryAmmo => "primammo",
            Equipment::SecondaryAmmo => "secammo",
            Equipment::Vest => "vest",
            Equipment::VestHelm => "vesthelm",
            Equipment::Flashbang => "flash",
            Equipment::HeGrenade => "hegren",
            Equipment::SmokeGrenade => "sgren",
            Equipment::Nvgs => "nvgs",
            Equipment::Defuser => "defuser",
            Equipment::Shield => "shield",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_pistols_are_the_ones_that_must_release_between_shots() {
        // Exactly the set with `if (++m_iShotsFired > 1) return;`
        // (dlls/wpn_shared/wpn_{usp,p228,deagle,elite,fiveseven,glock18}.cpp).
        // WeaponId::Glock is the legacy duplicate of the same gun.
        let semis: Vec<WeaponId> = WEAPONS
            .iter()
            .filter(|w| w.fire_class == FireClass::SemiPistol)
            .map(|w| w.id)
            .collect();
        for id in [W::Usp, W::Glock18, W::P228, W::Deagle, W::Elite, W::FiveSeven] {
            assert!(semis.contains(&id), "{id:?} must be a SemiPistol");
        }
        assert!(!FireClass::SemiPistol.may_hold(), "holding fires exactly one bullet");
    }

    #[test]
    fn the_six_that_never_touch_shots_fired_are_timer_only() {
        // grep -L m_iShotsFired dlls/wpn_shared/*.cpp
        for id in [W::Awp, W::Scout, W::G3sg1, W::Sg550, W::M3, W::Xm1014] {
            assert_eq!(fire_class(id), FireClass::TimerOnly, "{id:?}");
            assert!(fire_class(id).may_hold());
            assert!(!fire_class(id).degrades_with_shots());
        }
    }

    #[test]
    fn full_autos_degrade_and_therefore_burst() {
        for id in [W::Ak47, W::M4a1, W::Galil, W::Famas, W::Aug, W::Sg552, W::M249, W::P90] {
            assert_eq!(fire_class(id), FireClass::FullAuto, "{id:?}");
            assert!(fire_class(id).degrades_with_shots());
            assert!(fire_class(id).may_hold(), "it may be held, it just should not be");
        }
    }

    #[test]
    fn max_speeds_match_the_header() {
        // dlls/weapons.h: the *_MAX_SPEED constants.
        assert_eq!(max_speed(W::Ak47), 221.0);
        assert_eq!(max_speed(W::M249), 220.0);
        assert_eq!(max_speed(W::Awp), 210.0);
        assert_eq!(max_speed(W::G3sg1), 210.0);
        assert_eq!(max_speed(W::Sg550), 210.0);
        assert_eq!(max_speed(W::M4a1), 230.0);
        assert_eq!(max_speed(W::M3), 230.0);
        assert_eq!(max_speed(W::Sg552), 235.0);
        assert_eq!(max_speed(W::Aug), 240.0);
        assert_eq!(max_speed(W::Galil), 240.0);
        assert_eq!(max_speed(W::Famas), 240.0);
        assert_eq!(max_speed(W::Xm1014), 240.0);
        assert_eq!(max_speed(W::P90), 245.0);
        assert_eq!(max_speed(W::Scout), 260.0, "the scout is the fastest gun in the game");
        for id in [W::Usp, W::Deagle, W::Knife, W::C4, W::Mp5n, W::Mac10, W::Tmp, W::Ump45] {
            assert_eq!(max_speed(id), 250.0, "{id:?}");
        }
    }

    #[test]
    fn clip_sizes_match_weapontype_h() {
        assert_eq!(info(W::Ak47).unwrap().clip, 30);
        assert_eq!(info(W::Deagle).unwrap().clip, 7);
        assert_eq!(info(W::M249).unwrap().clip, 100);
        assert_eq!(info(W::P90).unwrap().clip, 50);
        assert_eq!(info(W::Galil).unwrap().clip, 35);
        assert_eq!(info(W::Xm1014).unwrap().clip, 7);
        assert_eq!(info(W::M3).unwrap().clip, 8);
        assert_eq!(info(W::Usp).unwrap().clip, 12);
        assert_eq!(info(W::P228).unwrap().clip, 13);
        assert_eq!(info(W::Knife).unwrap().clip, 0, "no clip");
        assert_eq!(info(W::C4).unwrap().clip, 0, "WEAPON_NOCLIP");
    }

    #[test]
    fn the_beretta_buy_alias_is_elites_not_elite() {
        // dlls/weapontype.cpp:156 — `elite` is a *weapon* alias (:118), and
        // buying with it silently does nothing.
        assert_eq!(info(W::Elite).unwrap().buy_alias, Some("elites"));
        assert_ne!(info(W::Elite).unwrap().buy_alias, Some("elite"));
    }

    #[test]
    fn selecting_is_the_entity_name_and_buying_is_the_bare_alias() {
        let ak = info(W::Ak47).unwrap();
        assert_eq!(ak.select_command(), "weapon_ak47", "SelectItem, client.cpp:3560");
        assert_eq!(ak.buy_alias, Some("ak47"), "the buy alias has no prefix");
        assert_ne!(ak.buy_alias, Some("weapon_ak47"), "that would be a switch, not a buy");
    }

    #[test]
    fn unbuyable_things_have_no_alias() {
        assert_eq!(info(W::Knife).unwrap().buy_alias, None);
        assert_eq!(info(W::C4).unwrap().buy_alias, None);
    }

    #[test]
    fn the_ids_are_the_regamedll_numbering() {
        // dlls/weapontype.h:31-63
        assert_eq!(W::P228 as u8, 1);
        assert_eq!(W::C4 as u8, 6);
        assert_eq!(W::Usp as u8, 16);
        assert_eq!(W::Glock18 as u8, 17);
        assert_eq!(W::Ak47 as u8, 28);
        assert_eq!(W::Knife as u8, 29);
        assert_eq!(W::P90 as u8, 30);
        assert_eq!(W::ShieldGun as u8, 99);
    }

    #[test]
    fn an_unknown_weapon_never_gets_the_trigger_pulled() {
        assert_eq!(fire_class(W::None), FireClass::NotAWeapon);
        assert!(info(W::None).is_none());
    }

    #[test]
    fn every_entry_is_unique_and_well_formed() {
        let mut ids: Vec<WeaponId> = WEAPONS.iter().map(|w| w.id).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate weapon id in the table");
        for w in WEAPONS {
            assert!(w.entity.starts_with("weapon_"), "{:?} entity {}", w.id, w.entity);
            assert!(w.max_speed > 0.0);
            assert!(w.clip >= 0);
            if let Some(a) = w.buy_alias {
                assert!(!a.starts_with("weapon_"), "{a} is a switch, not a buy alias");
            }
        }
    }

    #[test]
    fn equipment_aliases_match_the_buy_command_ladder() {
        // dlls/client.cpp:2503-2590
        assert_eq!(Equipment::VestHelm.alias(), "vesthelm");
        assert_eq!(Equipment::HeGrenade.alias(), "hegren");
        assert_eq!(Equipment::SmokeGrenade.alias(), "sgren");
        assert_eq!(Equipment::Flashbang.alias(), "flash");
        assert_eq!(Equipment::Defuser.alias(), "defuser");
        assert_eq!(Equipment::PrimaryAmmo.alias(), "primammo");
        assert_eq!(Equipment::SecondaryAmmo.alias(), "secammo");
        assert_eq!(Equipment::Nvgs.alias(), "nvgs");
    }
}
