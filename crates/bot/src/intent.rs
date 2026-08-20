//! What the bot wants to do this tick, in engine-neutral terms.
//!
//! There are two output channels, and they are not interchangeable:
//!
//! * the **movement channel** — [`Intent`]'s angles, move values and button
//!   intentions, which the caller packs into a `usercmd_t` and puts on the
//!   unreliable `clc_move` datagram every tick;
//! * the **command channel** — [`BotCommand`]s, which are console commands and
//!   go on the reliable channel as `clc_stringcmd`.
//!
//! The second one exists because **`usercmd_t.weaponselect` is dead**. Its only
//! occurrences in either reference tree are the struct declaration
//! (`regamedll/common/usercmd.h:48`, `rehlds/common/usercmd.h:34`) and the
//! delta description (`rehlds/engine/delta.cpp:221`) — the field is
//! transmitted and then never read by any logic. Changing weapon is a console
//! command and nothing else.

use crate::math::{Angles, Vec3};
use crate::weapons::{Equipment, WeaponId};

/// A console command for the reliable channel.
///
/// The distinction that bites people: `weapon_ak47` **switches** to the AK
/// (any command beginning `weapon_` is routed to `SelectItem`,
/// `dlls/client.cpp:3560-3563`), while `ak47` **buys** one (the buy-alias table
/// at `dlls/weapontype.cpp:125-170`). Sending `weapon_ak47` in a buy zone buys
/// nothing; sending `ak47` outside one buys nothing either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    /// Switch to a weapon already in the inventory — `SelectItem`.
    Select(WeaponId),
    /// Buy a weapon by its buy alias. Only valid inside a buy zone during the
    /// buy time.
    BuyWeapon(WeaponId),
    /// Buy armour, ammo, a grenade, nvgs or a defuse kit.
    BuyEquipment(Equipment),
    /// Anything else — `jointeam`, `chooseteam`, `radio`, `say`. Kept as an
    /// escape hatch rather than modelling the whole console.
    Raw(String),
}

impl BotCommand {
    /// Render the command exactly as it goes on the wire.
    ///
    /// Returns `None` for a [`BotCommand::Select`] or [`BotCommand::BuyWeapon`]
    /// of something with no entity name / no buy alias (the knife and the C4
    /// cannot be bought). Refusing to emit is the right answer there: the
    /// alternative is a console command the server will not recognise.
    pub fn to_console(&self) -> Option<String> {
        match self {
            BotCommand::Select(id) => {
                crate::weapons::info(*id).map(|w| w.select_command().to_string())
            }
            BotCommand::BuyWeapon(id) => crate::weapons::info(*id)
                .and_then(|w| w.buy_alias)
                .map(str::to_string),
            BotCommand::BuyEquipment(e) => Some(e.alias().to_string()),
            BotCommand::Raw(s) => {
                if s.trim().is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            }
        }
    }
}

/// The bot's decision for one tick.
///
/// The client turns this into a `usercmd_t`: `view` → `viewangles`,
/// `forwardmove`/`sidemove` straight across, the button intentions into the
/// `buttons` bitmask, and drains `commands` onto the reliable channel.
#[derive(Debug, Clone, PartialEq)]
pub struct Intent {
    /// The angles to *send*. Already punch-compensated — see
    /// [`crate::aim::compensate`]. Do not adjust it further.
    pub view: Angles,
    pub forwardmove: f32,
    pub sidemove: f32,
    /// `IN_ATTACK`.
    pub attack: bool,
    /// `IN_ATTACK2` — sniper zoom, the shield, silencer/burst toggles.
    ///
    /// Note it is checked *before* `IN_ATTACK` in
    /// `CBasePlayerWeapon::ItemPostFrame` (`dlls/weapons.cpp:1059` vs `:1072`)
    /// and the branches are exclusive, so setting both loses the shot.
    pub attack2: bool,
    /// `IN_RELOAD`.
    pub reload: bool,
    pub jump: bool,
    pub duck: bool,
    /// Move slowly enough not to make footstep noise.
    ///
    /// **Advisory only — do not map this to a button.** There is no server-side
    /// walk key: `IN_RUN` is read in exactly two places in `pm_shared.cpp`
    /// (`:1087`, `:2557`) and both are gated on ReGameDLL's `fuser3` speed-run
    /// addon, so it does nothing on a stock server. Quiet movement is purely a
    /// matter of velocity — `PM_UpdateStepSound` returns early at
    /// `speed <= 150.0` (`pm_shared/pm_shared.cpp:395`). When this flag is set,
    /// `forwardmove`/`sidemove` have *already* been scaled to achieve that.
    pub walk: bool,
    /// `IN_SCORE` — the scoreboard. Harmless, and it is a real input, so it is
    /// useful as a liveness signal.
    pub score: bool,
    /// Hold `+use` — defusing, and picking things up.
    pub use_action: bool,
    /// Where the bot is trying to move, for tracing and the nav layer. `None`
    /// while holding position.
    pub move_target: Option<Vec3>,
    /// Console commands to put on the reliable channel this tick. Usually
    /// empty; the caller should drain it.
    pub commands: Vec<BotCommand>,
}

impl Default for Intent {
    fn default() -> Self {
        Self {
            view: Angles::default(),
            forwardmove: 0.0,
            sidemove: 0.0,
            attack: false,
            attack2: false,
            reload: false,
            jump: false,
            duck: false,
            walk: false,
            score: false,
            use_action: false,
            move_target: None,
            commands: Vec::new(),
        }
    }
}

impl Intent {
    /// An intent that does nothing but keep looking where it was looking.
    ///
    /// The bot's default answer whenever it is unsure. Standing still is always
    /// safe; guessing is not.
    pub fn hold(view: Angles) -> Self {
        Self { view, ..Self::default() }
    }

    pub fn with_command(mut self, cmd: BotCommand) -> Self {
        self.commands.push(cmd);
        self
    }

    /// Every command rendered for the wire, skipping the un-renderable.
    pub fn console_lines(&self) -> Vec<String> {
        self.commands.iter().filter_map(BotCommand::to_console).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_and_buying_are_different_strings() {
        assert_eq!(
            BotCommand::Select(WeaponId::Ak47).to_console().as_deref(),
            Some("weapon_ak47")
        );
        assert_eq!(
            BotCommand::BuyWeapon(WeaponId::Ak47).to_console().as_deref(),
            Some("ak47")
        );
    }

    #[test]
    fn the_beretta_buys_as_elites() {
        assert_eq!(
            BotCommand::BuyWeapon(WeaponId::Elite).to_console().as_deref(),
            Some("elites")
        );
    }

    #[test]
    fn unbuyable_weapons_emit_nothing_rather_than_a_bad_command() {
        assert_eq!(BotCommand::BuyWeapon(WeaponId::Knife).to_console(), None);
        assert_eq!(BotCommand::BuyWeapon(WeaponId::C4).to_console(), None);
        // ...but they can still be selected.
        assert_eq!(
            BotCommand::Select(WeaponId::C4).to_console().as_deref(),
            Some("weapon_c4")
        );
    }

    #[test]
    fn an_unknown_weapon_emits_nothing() {
        assert_eq!(BotCommand::Select(WeaponId::None).to_console(), None);
        assert_eq!(BotCommand::Raw("   ".into()).to_console(), None);
        assert_eq!(
            BotCommand::Raw("jointeam 1".into()).to_console().as_deref(),
            Some("jointeam 1")
        );
    }

    #[test]
    fn equipment_buys_by_bare_alias() {
        assert_eq!(
            BotCommand::BuyEquipment(Equipment::VestHelm).to_console().as_deref(),
            Some("vesthelm")
        );
        assert_eq!(
            BotCommand::BuyEquipment(Equipment::Defuser).to_console().as_deref(),
            Some("defuser")
        );
    }

    #[test]
    fn a_default_intent_does_nothing_at_all() {
        let i = Intent::default();
        assert!(!i.attack && !i.attack2 && !i.reload && !i.jump);
        assert!(!i.duck && !i.walk && !i.score && !i.use_action);
        assert_eq!(i.forwardmove, 0.0);
        assert_eq!(i.sidemove, 0.0);
        assert!(i.commands.is_empty());
        assert_eq!(i.move_target, None);
    }

    #[test]
    fn hold_keeps_the_view_and_nothing_else() {
        let a = Angles { pitch: 3.0, yaw: -120.0 };
        let i = Intent::hold(a);
        assert_eq!(i.view, a);
        assert_eq!(i, Intent { view: a, ..Intent::default() });
    }

    #[test]
    fn console_lines_drops_the_unrenderable() {
        let i = Intent::default()
            .with_command(BotCommand::BuyWeapon(WeaponId::Ak47))
            .with_command(BotCommand::BuyWeapon(WeaponId::Knife))
            .with_command(BotCommand::BuyEquipment(Equipment::Vest));
        assert_eq!(i.console_lines(), vec!["ak47".to_string(), "vest".to_string()]);
    }
}
