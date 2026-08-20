//! Round economy: when to full-buy, force, or save (CS 1.6 style).
//!
//! See `docs/utility-economy-plan.md`. Money is local to this bot until the
//! G0 team bus exists; classes are still the right shape for pro-style buys.

use crate::intent::BotCommand;
use crate::utility::{owned_buy_util, NadeKind};
use crate::weapons::{Equipment, WeaponId};
use crate::world::Team;

/// How this bot should spend this freezetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyClass {
    /// ~$800 start / after full loss — pistols only.
    Pistol,
    /// Save for next full buy. Minimal spend.
    Eco,
    /// Contested buy: armor + something that can kill; almost no util.
    Force,
    /// Rifle + armor + kit + owned util slots.
    Full,
}

/// Classify spend style from pocket money (1.6 default prices).
pub fn classify_buy(money: i32, team: Team) -> BuyClass {
    // Thresholds biased to CS 1.6: AK 2500, M4 3100, vesthelm 1000.
    let full_cut = match team {
        Team::Terrorist => 4000, // AK + vesthelm + util headroom
        _ => 4700,               // M4 + vesthelm + kit
    };
    let force_cut = match team {
        Team::Terrorist => 2000,
        _ => 2500,
    };
    if money <= 800 {
        BuyClass::Pistol
    } else if money < force_cut {
        BuyClass::Eco
    } else if money < full_cut {
        BuyClass::Force
    } else {
        BuyClass::Full
    }
}

/// Build a freeze-time buy list that never overspends.
///
/// `seed` selects which util slots this bot is allowed to buy (see utility
/// ownership) so ten bots do not each buy mid-smoke.
pub fn build_buy_plan(money: i32, team: Team, seed: u64) -> Vec<BotCommand> {
    let class = classify_buy(money, team);
    let mut plan = Vec::new();
    let mut purse = money;
    let is_ct = team == Team::CounterTerrorist;

    match class {
        BuyClass::Pistol => {
            // Kevlar if we can; never rifle; no nades.
            try_buy(
                &mut purse,
                &mut plan,
                650,
                BotCommand::BuyEquipment(Equipment::Vest),
            );
            try_buy(
                &mut purse,
                &mut plan,
                650,
                BotCommand::BuyWeapon(WeaponId::Deagle),
            );
            try_buy(
                &mut purse,
                &mut plan,
                ammo_cost_secondary(),
                BotCommand::BuyEquipment(Equipment::SecondaryAmmo),
            );
        }
        BuyClass::Eco => {
            // Keep most of the bank. Vest first; no rifle, no util.
            try_buy(
                &mut purse,
                &mut plan,
                650,
                BotCommand::BuyEquipment(Equipment::Vest),
            );
            if purse >= 2000 {
                try_buy(
                    &mut purse,
                    &mut plan,
                    650,
                    BotCommand::BuyWeapon(WeaponId::Deagle),
                );
            }
            try_buy(
                &mut purse,
                &mut plan,
                ammo_cost_secondary(),
                BotCommand::BuyEquipment(Equipment::SecondaryAmmo),
            );
        }
        BuyClass::Force => {
            try_buy(
                &mut purse,
                &mut plan,
                1000,
                BotCommand::BuyEquipment(Equipment::VestHelm),
            );
            // Util before rifle so slot owners still get nades on tight force
            // buys (U2c inventory gate starved live util).
            buy_owned_util(
                &mut purse, &mut plan, seed, team, /*include_smoke*/ false,
            );
            let rifle = rifle_for(team);
            if purse >= rifle.0 {
                try_buy(
                    &mut purse,
                    &mut plan,
                    rifle.0,
                    BotCommand::BuyWeapon(rifle.1),
                );
                try_buy(
                    &mut purse,
                    &mut plan,
                    ammo_cost_primary(),
                    BotCommand::BuyEquipment(Equipment::PrimaryAmmo),
                );
            } else {
                try_buy(
                    &mut purse,
                    &mut plan,
                    650,
                    BotCommand::BuyWeapon(WeaponId::Deagle),
                );
                try_buy(
                    &mut purse,
                    &mut plan,
                    ammo_cost_secondary(),
                    BotCommand::BuyEquipment(Equipment::SecondaryAmmo),
                );
            }
            if is_ct {
                try_buy(
                    &mut purse,
                    &mut plan,
                    200,
                    BotCommand::BuyEquipment(Equipment::Defuser),
                );
            }
        }
        BuyClass::Full => {
            try_buy(
                &mut purse,
                &mut plan,
                1000,
                BotCommand::BuyEquipment(Equipment::VestHelm),
            );
            // Owned util first (cheap) so a tight full-buy still arms the slot.
            buy_owned_util(
                &mut purse, &mut plan, seed, team, /*include_smoke*/ true,
            );
            let rifle = rifle_for(team);
            try_buy(
                &mut purse,
                &mut plan,
                rifle.0,
                BotCommand::BuyWeapon(rifle.1),
            );
            try_buy(
                &mut purse,
                &mut plan,
                ammo_cost_primary(),
                BotCommand::BuyEquipment(Equipment::PrimaryAmmo),
            );
            if is_ct {
                try_buy(
                    &mut purse,
                    &mut plan,
                    200,
                    BotCommand::BuyEquipment(Equipment::Defuser),
                );
            }
            try_buy(
                &mut purse,
                &mut plan,
                ammo_cost_secondary(),
                BotCommand::BuyEquipment(Equipment::SecondaryAmmo),
            );
        }
    }

    let _ = purse;
    plan
}

fn try_buy(purse: &mut i32, plan: &mut Vec<BotCommand>, cost: i32, cmd: BotCommand) {
    if *purse >= cost {
        *purse -= cost;
        plan.push(cmd);
    }
}

/// Buy every nade kind this seed owns a slot for (inventory for ThrowMachine).
///
/// **Full-buy flash is shared:** every full/force buyer gets one flashbang once
/// money allows (plan U2d follow-up) — smokes/HE stay slot-owned so mid doors
/// is not ten smokes.
fn buy_owned_util(
    purse: &mut i32,
    plan: &mut Vec<BotCommand>,
    seed: u64,
    team: Team,
    include_smoke: bool,
) {
    if include_smoke && owned_buy_util(seed, team, NadeKind::Smoke) {
        try_buy(
            purse,
            plan,
            300,
            BotCommand::BuyEquipment(Equipment::SmokeGrenade),
        );
    }
    // Full/force: every bot buys one flash (shared pop-flash inventory). Smoke/HE
    // stay slot-owned so we never dump 10 mid smokes. This helper is only
    // called from Force/Full — eco/pistol never reach here.
    try_buy(
        purse,
        plan,
        200,
        BotCommand::BuyEquipment(Equipment::Flashbang),
    );
    if owned_buy_util(seed, team, NadeKind::He) {
        try_buy(
            purse,
            plan,
            300,
            BotCommand::BuyEquipment(Equipment::HeGrenade),
        );
    }
}

fn rifle_for(team: Team) -> (i32, WeaponId) {
    match team {
        Team::Terrorist => (2500, WeaponId::Ak47),
        _ => (3100, WeaponId::M4a1),
    }
}

fn ammo_cost_primary() -> i32 {
    // Alias primammo — server charges by weapon; 0 keeps spend() always true
    // for the command itself; real cost is small. We use 0 so full-buy still
    // queues the alias after a tight rifle buy.
    0
}

fn ammo_cost_secondary() -> i32 {
    0
}

/// Total listed cost of a plan (ammo aliases counted as 0).
pub fn plan_cost(plan: &[BotCommand]) -> i32 {
    plan.iter().map(cmd_cost).sum()
}

fn cmd_cost(cmd: &BotCommand) -> i32 {
    match cmd {
        BotCommand::BuyEquipment(Equipment::VestHelm) => 1000,
        BotCommand::BuyEquipment(Equipment::Vest) => 650,
        BotCommand::BuyEquipment(Equipment::Defuser) => 200,
        BotCommand::BuyEquipment(Equipment::HeGrenade) => 300,
        BotCommand::BuyEquipment(Equipment::Flashbang) => 200,
        BotCommand::BuyEquipment(Equipment::SmokeGrenade) => 300,
        BotCommand::BuyEquipment(Equipment::PrimaryAmmo) => 0,
        BotCommand::BuyEquipment(Equipment::SecondaryAmmo) => 0,
        BotCommand::BuyWeapon(WeaponId::Ak47) => 2500,
        BotCommand::BuyWeapon(WeaponId::M4a1) => 3100,
        BotCommand::BuyWeapon(WeaponId::Deagle) => 650,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eco_never_buys_a_rifle() {
        for money in [900, 1500, 1999] {
            let plan = build_buy_plan(money, Team::Terrorist, 1);
            assert!(
                !plan
                    .iter()
                    .any(|c| matches!(c, BotCommand::BuyWeapon(WeaponId::Ak47 | WeaponId::M4a1))),
                "money {money}: {plan:?}"
            );
            assert!(
                !plan.iter().any(|c| matches!(
                    c,
                    BotCommand::BuyEquipment(
                        Equipment::HeGrenade | Equipment::SmokeGrenade | Equipment::Flashbang
                    )
                )),
                "eco must not stack util: {plan:?}"
            );
        }
    }

    #[test]
    fn full_buy_gets_armor_and_rifle_when_rich() {
        let plan = build_buy_plan(16000, Team::Terrorist, 0);
        assert!(plan.contains(&BotCommand::BuyEquipment(Equipment::VestHelm)));
        assert!(plan.contains(&BotCommand::BuyWeapon(WeaponId::Ak47)));
        assert!(plan_cost(&plan) <= 16000);
    }

    #[test]
    fn plan_never_exceeds_purse() {
        for money in [0, 800, 1000, 2500, 4000, 8000] {
            for team in [Team::Terrorist, Team::CounterTerrorist] {
                for seed in 0..8 {
                    let plan = build_buy_plan(money, team, seed);
                    assert!(
                        plan_cost(&plan) <= money,
                        "money {money} team {team:?} seed {seed}: cost {} plan {plan:?}",
                        plan_cost(&plan)
                    );
                }
            }
        }
    }

    #[test]
    fn not_every_seed_buys_smoke_on_full() {
        let mut smoke_buyers = 0;
        for seed in 0..20 {
            let plan = build_buy_plan(8000, Team::Terrorist, seed);
            if plan.contains(&BotCommand::BuyEquipment(Equipment::SmokeGrenade)) {
                smoke_buyers += 1;
            }
        }
        assert!(
            smoke_buyers >= 1 && smoke_buyers <= 16,
            "ownership should spread smokes, got {smoke_buyers}/20"
        );
    }

    #[test]
    fn classify_thresholds() {
        assert_eq!(classify_buy(800, Team::Terrorist), BuyClass::Pistol);
        assert_eq!(classify_buy(1500, Team::Terrorist), BuyClass::Eco);
        assert_eq!(classify_buy(3000, Team::Terrorist), BuyClass::Force);
        assert_eq!(classify_buy(5000, Team::Terrorist), BuyClass::Full);
    }
}
