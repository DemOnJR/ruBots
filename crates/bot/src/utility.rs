//! Coordinated grenade slots: who may buy/throw what, and when.
//!
//! Design: `docs/utility-economy-plan.md`.
//!
//! v1 prevents the two failure modes that look bot-like:
//! 1. everyone buys/throws the same mid smoke at freezetime end;
//! 2. walking around with a nade drawn while doing nothing.
//!
//! Actual throw trajectories land in U2; this module is ownership + triggers.

use crate::weapons::WeaponId;
use crate::world::{Team, WorldView};

/// Grenade kind in the 1.6 kit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NadeKind {
    Smoke,
    Flash,
    He,
}

impl NadeKind {
    pub fn weapon_id(self) -> WeaponId {
        match self {
            Self::Smoke => WeaponId::SmokeGrenade,
            Self::Flash => WeaponId::Flashbang,
            Self::He => WeaponId::HeGrenade,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smoke => "smoke",
            Self::Flash => "flash",
            Self::He => "he",
        }
    }

    /// Bit in the controller's owned-util mask (clientdata `weapons` is not on the wire).
    pub fn owned_bit(self) -> u8 {
        match self {
            Self::Smoke => 1,
            Self::Flash => 2,
            Self::He => 4,
        }
    }
}

/// True if we currently hold this nade or the freeze buy plan granted it.
pub fn has_nade(world: &WorldView, owned_mask: u8, kind: NadeKind) -> bool {
    if world.me.weapon_or_unknown().id == kind.weapon_id() {
        return true;
    }
    owned_mask & kind.owned_bit() != 0
}

/// When a slot is allowed to fire (never freezetime / never t&lt;2s).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UtilityTrigger {
    /// T execute: near this choke, bomb not planted.
    ExecuteChoke { x: f32, y: f32, radius: f32 },
    /// CT default: hold near choke after freezetime, deny mid.
    HoldDeny { x: f32, y: f32, radius: f32 },
    /// After plant (T defend or CT retake).
    PostPlant,
    /// Visible enemy within radius (HE).
    Contact { radius: f32 },
}

/// One coordinated util job on a map.
#[derive(Debug, Clone, Copy)]
pub struct UtilitySlot {
    pub id: &'static str,
    pub kind: NadeKind,
    pub side: Team,
    /// Stand near here to throw.
    pub from: [f32; 3],
    /// Look here (simple point-throw aim).
    pub aim: [f32; 3],
    pub trigger: UtilityTrigger,
    /// Rank bucket: owner is `seed % team_buckets == owner_bucket`.
    pub owner_bucket: u64,
    pub team_buckets: u64,
}

/// Starter dust2 pack.
///
/// U2b: `from`/`aim` aligned with G1 CT anchors + Phase D plant spots that
/// already path on live de_dust2 (see `role.rs` / plant logs). Wide trigger
/// radii so owners arm while *passing* the choke, not only at a pixel.
pub fn dust2_slots() -> &'static [UtilitySlot] {
    // Owner buckets 0..4 so in a 10-bot team ~2 candidates; only exact match buys.
    const SLOTS: &[UtilitySlot] = &[
        UtilitySlot {
            id: "t_mid_doors_smoke",
            kind: NadeKind::Smoke,
            side: Team::Terrorist,
            // Mid approach (live split destinations sat near here).
            from: [100.0, 1400.0, 32.0],
            aim: [100.0, 2100.0, 96.0], // toward CT mid doors
            trigger: UtilityTrigger::ExecuteChoke {
                x: 100.0,
                y: 1200.0,
                radius: 900.0,
            },
            owner_bucket: 0,
            // U2d: 5→3 so ~1/3 of team seeds own each slot (more real util).
            team_buckets: 3,
        },
        UtilitySlot {
            id: "t_long_smoke",
            kind: NadeKind::Smoke,
            side: Team::Terrorist,
            from: [700.0, 400.0, 32.0],
            aim: [1200.0, 2000.0, 96.0], // long → A
            trigger: UtilityTrigger::ExecuteChoke {
                x: 600.0,
                y: 600.0,
                radius: 900.0,
            },
            owner_bucket: 1,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "t_xbox_flash",
            kind: NadeKind::Flash,
            side: Team::Terrorist,
            from: [50.0, 1100.0, 32.0],
            aim: [80.0, 1900.0, 120.0],
            trigger: UtilityTrigger::ExecuteChoke {
                x: 80.0,
                y: 1000.0,
                radius: 800.0,
            },
            owner_bucket: 2,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "t_site_he",
            kind: NadeKind::He,
            side: Team::Terrorist,
            // A default plant (Phase D) — post-plant holds already go here.
            from: [1160.0, 2480.0, 132.0],
            aim: [1280.0, 2392.0, 96.0],
            trigger: UtilityTrigger::PostPlant,
            owner_bucket: 0,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "t_b_site_he",
            kind: NadeKind::He,
            side: Team::Terrorist,
            from: [-1520.0, 2680.0, 36.0],
            aim: [-1424.0, 2624.0, 48.0],
            trigger: UtilityTrigger::PostPlant,
            owner_bucket: 1,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "ct_mid_smoke",
            kind: NadeKind::Smoke,
            side: Team::CounterTerrorist,
            // G1 CT mid doors anchor.
            from: [100.0, 2100.0, 96.0],
            aim: [50.0, 1200.0, 50.0],
            trigger: UtilityTrigger::HoldDeny {
                x: 100.0,
                y: 2000.0,
                radius: 800.0,
            },
            owner_bucket: 0,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "ct_retake_flash",
            kind: NadeKind::Flash,
            side: Team::CounterTerrorist,
            from: [400.0, 2300.0, 96.0], // G1 flex → A
            aim: [1160.0, 2480.0, 132.0],
            trigger: UtilityTrigger::PostPlant,
            owner_bucket: 1,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "ct_plant_he",
            kind: NadeKind::He,
            side: Team::CounterTerrorist,
            from: [1000.0, 2300.0, 96.0],
            aim: [1160.0, 2480.0, 132.0],
            trigger: UtilityTrigger::PostPlant,
            owner_bucket: 2,
            team_buckets: 3,
        },
        UtilitySlot {
            id: "ct_contact_he",
            kind: NadeKind::He,
            side: Team::CounterTerrorist,
            // from unused for contact — throw in place toward enemy.
            from: [100.0, 2100.0, 96.0],
            aim: [100.0, 1400.0, 32.0],
            trigger: UtilityTrigger::Contact { radius: 650.0 },
            owner_bucket: 0,
            team_buckets: 3,
        },
    ];
    SLOTS
}

/// Whether this bot seed owns at least one buyable slot of `kind` on its side.
///
/// Flash is always “owned” for buy purposes on force/full (see `buy_owned_util`);
/// smokes/HE stay hash-bucketed so mid doors is not ten smokes.
pub fn owned_buy_util(seed: u64, team: Team, kind: NadeKind) -> bool {
    if kind == NadeKind::Flash {
        return dust2_slots()
            .iter()
            .any(|s| s.side == team && s.kind == NadeKind::Flash);
    }
    dust2_slots()
        .iter()
        .any(|s| s.side == team && s.kind == kind && owns_slot(seed, s))
}

pub fn owns_slot(seed: u64, slot: &UtilitySlot) -> bool {
    if slot.team_buckets == 0 {
        return false;
    }
    seed % slot.team_buckets == slot.owner_bucket
}

/// Slots this bot is allowed to execute this round.
/// Flash slots are shared (any full-buy bot may throw); smoke/HE stay exclusive.
pub fn my_slots(seed: u64, team: Team) -> Vec<&'static UtilitySlot> {
    dust2_slots()
        .iter()
        .filter(|s| s.side == team && (owns_slot(seed, s) || s.kind == NadeKind::Flash))
        .collect()
}

/// Hard bans before any trigger logic.
pub fn may_consider_util(world: &WorldView, seconds_since_unfreeze: f32) -> bool {
    if !world.me.alive || world.me.freeze_period {
        return false;
    }
    // No util dump at T0 — pros walk out first.
    if seconds_since_unfreeze < 3.0 {
        return false;
    }
    true
}

/// Whether the world matches a slot's trigger (ownership already assumed).
pub fn trigger_ready(world: &WorldView, slot: &UtilitySlot) -> bool {
    let o = world.me.origin;
    match slot.trigger {
        UtilityTrigger::ExecuteChoke { x, y, radius } => {
            if world.bomb.planted || world.me.team != Team::Terrorist {
                return false;
            }
            dist2(o[0], o[1], x, y) <= radius * radius
        }
        UtilityTrigger::HoldDeny { x, y, radius } => {
            if world.bomb.planted || world.me.team != Team::CounterTerrorist {
                return false;
            }
            dist2(o[0], o[1], x, y) <= radius * radius
        }
        UtilityTrigger::PostPlant => world.bomb.planted,
        UtilityTrigger::Contact { radius } => world.players.iter().any(|p| {
            p.alive
                && p.visible
                && p.team.is_enemy_of(world.me.team)
                && dist2(o[0], o[1], p.origin[0], p.origin[1]) <= radius * radius
        }),
    }
}

fn dist2(ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = ax - bx;
    let dy = ay - by;
    dx * dx + dy * dy
}

fn contact_aim(world: &WorldView, slot: &UtilitySlot) -> crate::math::Angles {
    if matches!(slot.trigger, UtilityTrigger::Contact { .. }) {
        if let Some(e) = world
            .players
            .iter()
            .filter(|p| p.alive && p.visible && p.team.is_enemy_of(world.me.team))
            .min_by(|a, b| {
                let da = dist2(
                    world.me.origin[0],
                    world.me.origin[1],
                    a.origin[0],
                    a.origin[1],
                );
                let db = dist2(
                    world.me.origin[0],
                    world.me.origin[1],
                    b.origin[0],
                    b.origin[1],
                );
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            return crate::math::aim_angles(world.me.origin, e.origin);
        }
    }
    crate::math::aim_angles(world.me.origin, slot.aim)
}

/// True if the active weapon is a grenade (should not stay selected idle).
pub fn holding_nade(id: WeaponId) -> bool {
    matches!(
        id,
        WeaponId::HeGrenade | WeaponId::Flashbang | WeaponId::SmokeGrenade
    )
}

/// Preferred gun to switch back to after a throw (or when stuck on a nade).
pub fn preferred_gun(world: &WorldView) -> WeaponId {
    use crate::weapons::WeaponId as W;
    // weapons bitmask is available on me; prefer primary names by team.
    let _ = world.me.weapons;
    match world.me.team {
        Team::Terrorist => W::Ak47,
        Team::CounterTerrorist => W::M4a1,
        _ => W::Deagle,
    }
}

// ---------------------------------------------------------------------------
// U2 — throw state machine
// ---------------------------------------------------------------------------

/// How close to `from` counts as "at the throw spot".
/// U2b: was 120 — too tight for rough lineup coords on the lattice.
pub const THROW_ARRIVE: f32 = 220.0;

/// Give up pathing and throw from here after this many seconds (U2b).
/// Prevents permanent `util-walk` when `from` is off-nav.
pub const APPROACH_TIMEOUT: f32 = 5.0;

/// Seconds of +attack to pull the pin before release (1.6: hold = cook start).
pub const PIN_HOLD: f32 = 0.18;

/// After release, brief settle before holster.
pub const AFTER_THROW: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq)]
enum ThrowPhase {
    Idle,
    /// Walk toward the lineup stand point.
    Approach,
    /// `Select` the nade weapon.
    Select,
    /// Face aim point.
    Aim,
    /// Hold IN_ATTACK (pin).
    Pin,
    /// Release attack (throw) then holster.
    Release,
}

/// One bot's util throw progress for the current round.
#[derive(Debug, Clone)]
pub struct ThrowMachine {
    phase: ThrowPhase,
    /// Index into [`dust2_slots`] for the active job.
    slot_i: Option<usize>,
    phase_t: f32,
    /// Slot ids already used this round (no double mid smoke).
    used: Vec<&'static str>,
    was_frozen: bool,
    /// Seconds since freezetime ended (for UTIL-1 ban).
    pub since_unfreeze: f32,
}

impl Default for ThrowMachine {
    fn default() -> Self {
        Self {
            phase: ThrowPhase::Idle,
            slot_i: None,
            phase_t: 0.0,
            used: Vec::new(),
            was_frozen: false,
            since_unfreeze: 0.0,
        }
    }
}

/// Output of one throw-machine tick when it owns the bot.
#[derive(Debug, Clone)]
pub struct ThrowIntent {
    pub aim: crate::math::Angles,
    pub attack: bool,
    pub select: Option<WeaponId>,
    pub move_to: Option<[f32; 3]>,
    /// Diagnostic rung label.
    pub rung: &'static str,
}

impl ThrowMachine {
    /// True while a throw is in progress — caller must not holster the nade.
    pub fn active(&self) -> bool {
        !matches!(self.phase, ThrowPhase::Idle)
    }

    pub fn last_slot_id(&self) -> Option<&'static str> {
        self.slot_i.and_then(|i| dust2_slots().get(i).map(|s| s.id))
    }

    pub fn slot_was_used(&self, id: &str) -> bool {
        self.used.iter().any(|u| *u == id)
    }

    /// Advance. Returns `Some` when this tick should override normal combat/nav
    /// (except we yield to combat if the caller checks threats first).
    ///
    /// `owned_mask`: nades granted by this round's buy plan (see [`NadeKind::owned_bit`]).
    /// Cleared by the controller when a throw completes.
    pub fn tick(
        &mut self,
        world: &WorldView,
        seed: u64,
        owned_mask: u8,
        dt: f32,
    ) -> Option<ThrowIntent> {
        // Round reset on freeze rising edge.
        if world.me.freeze_period {
            if !self.was_frozen {
                self.reset_round();
            }
            self.was_frozen = true;
            self.since_unfreeze = 0.0;
            return None;
        }
        if self.was_frozen {
            self.was_frozen = false;
            self.since_unfreeze = 0.0;
        }
        self.since_unfreeze += dt;

        if !world.me.alive {
            self.phase = ThrowPhase::Idle;
            self.slot_i = None;
            return None;
        }

        if !self.active() {
            if !may_consider_util(world, self.since_unfreeze) {
                return None;
            }
            // Pick first allowed, unused, triggered slot we actually carry.
            // Flash is shared; smoke/HE stay seed-owned.
            let team = world.me.team;
            let pick = dust2_slots().iter().enumerate().find(|(_i, s)| {
                s.side == team
                    && (owns_slot(seed, s) || s.kind == NadeKind::Flash)
                    && !self.used.contains(&s.id)
                    && has_nade(world, owned_mask, s.kind)
                    && trigger_ready(world, s)
            });
            let Some((i, _slot)) = pick else {
                return None;
            };
            self.slot_i = Some(i);
            self.phase = ThrowPhase::Approach;
            self.phase_t = 0.0;
        }

        let slot = dust2_slots().get(self.slot_i?)?;
        // Abort if trigger no longer makes sense (e.g. plant cancelled).
        if matches!(
            self.phase,
            ThrowPhase::Approach | ThrowPhase::Select | ThrowPhase::Aim
        ) && !trigger_ready(world, slot)
            && !matches!(slot.trigger, UtilityTrigger::PostPlant)
        {
            // Keep post-plant once started; others may drop.
            if !matches!(slot.trigger, UtilityTrigger::PostPlant) {
                self.phase = ThrowPhase::Idle;
                self.slot_i = None;
                return None;
            }
        }

        self.phase_t += dt;
        let aim = contact_aim(world, slot);

        match self.phase {
            ThrowPhase::Idle => None,
            ThrowPhase::Approach => {
                let d = dist2(
                    world.me.origin[0],
                    world.me.origin[1],
                    slot.from[0],
                    slot.from[1],
                )
                .sqrt();
                // Contact HE / timed-out approach: throw from current feet.
                let throw_here = d <= THROW_ARRIVE
                    || self.phase_t >= APPROACH_TIMEOUT
                    || matches!(slot.trigger, UtilityTrigger::Contact { .. });
                if throw_here {
                    self.phase = ThrowPhase::Select;
                    self.phase_t = 0.0;
                    // Contact: aim at the nearest visible enemy if any.
                    let aim = contact_aim(world, slot);
                    Some(ThrowIntent {
                        aim,
                        attack: false,
                        select: Some(slot.kind.weapon_id()),
                        move_to: None,
                        rung: "util-select",
                    })
                } else {
                    Some(ThrowIntent {
                        aim,
                        attack: false,
                        select: None,
                        move_to: Some(slot.from),
                        rung: "util-walk",
                    })
                }
            }
            ThrowPhase::Select => {
                // Give the server a moment to switch; re-issue select.
                if self.phase_t > 0.35 {
                    self.phase = ThrowPhase::Aim;
                    self.phase_t = 0.0;
                }
                Some(ThrowIntent {
                    aim,
                    attack: false,
                    select: Some(slot.kind.weapon_id()),
                    move_to: None,
                    rung: "util-select",
                })
            }
            ThrowPhase::Aim => {
                // Face the aim point for a beat, then pin.
                if self.phase_t > 0.2 {
                    self.phase = ThrowPhase::Pin;
                    self.phase_t = 0.0;
                }
                Some(ThrowIntent {
                    aim,
                    attack: false,
                    select: None,
                    move_to: None,
                    rung: "util-aim",
                })
            }
            ThrowPhase::Pin => {
                if self.phase_t >= PIN_HOLD {
                    self.phase = ThrowPhase::Release;
                    self.phase_t = 0.0;
                }
                Some(ThrowIntent {
                    aim,
                    attack: true, // pin
                    select: None,
                    move_to: None,
                    rung: "util-pin",
                })
            }
            ThrowPhase::Release => {
                if self.phase_t >= AFTER_THROW {
                    // Mark used and holster.
                    if !self.used.contains(&slot.id) {
                        self.used.push(slot.id);
                    }
                    self.phase = ThrowPhase::Idle;
                    self.slot_i = None;
                    self.phase_t = 0.0;
                    Some(ThrowIntent {
                        aim,
                        attack: false,
                        select: Some(preferred_gun(world)),
                        move_to: None,
                        rung: "util-done",
                    })
                } else {
                    // First frames: release attack (throw).
                    Some(ThrowIntent {
                        aim,
                        attack: false,
                        select: None,
                        move_to: None,
                        rung: "util-throw",
                    })
                }
            }
        }
    }

    fn reset_round(&mut self) {
        self.phase = ThrowPhase::Idle;
        self.slot_i = None;
        self.phase_t = 0.0;
        self.used.clear();
    }

    /// Cancel an in-flight throw (e.g. close gunfight) but keep round state.
    pub fn abort_active(&mut self) {
        let t = self.since_unfreeze;
        let used = std::mem::take(&mut self.used);
        *self = Self::default();
        self.since_unfreeze = t;
        self.used = used;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_smoke_not_owned_by_all_t_seeds() {
        let mid = dust2_slots()
            .iter()
            .find(|s| s.id == "t_mid_doors_smoke")
            .unwrap();
        let n = (0..9).filter(|&s| owns_slot(s, mid)).count();
        // team_buckets 3, owner 0 → seeds 0,3,6
        assert_eq!(n, 3, "seeds ≡ 0 (mod 3) in 0..9: {n}");
    }

    #[test]
    fn each_slot_has_sparse_owners() {
        for slot in dust2_slots() {
            let owners = (0..12).filter(|&s| owns_slot(s, slot)).count();
            assert!(
                owners >= 2 && owners <= 6,
                "slot {} owners {owners}/12 (bucket {}/{})",
                slot.id,
                slot.owner_bucket,
                slot.team_buckets
            );
        }
    }

    #[test]
    fn freeze_blocks_util() {
        let mut w = WorldView::default();
        w.me.alive = true;
        w.me.freeze_period = true;
        assert!(!may_consider_util(&w, 10.0));
        w.me.freeze_period = false;
        assert!(!may_consider_util(&w, 1.0));
        assert!(may_consider_util(&w, 5.0));
    }

    #[test]
    fn post_plant_trigger() {
        let slot = dust2_slots().iter().find(|s| s.id == "t_site_he").unwrap();
        let mut w = WorldView::default();
        w.me.team = Team::Terrorist;
        w.me.alive = true;
        assert!(!trigger_ready(&w, slot));
        w.bomb.planted = true;
        assert!(trigger_ready(&w, slot));
    }

    #[test]
    fn throw_machine_pins_then_releases_once_at_spot() {
        // Owner of t_site_he is bucket 3, team_buckets 5 → seed 3.
        let mut m = ThrowMachine::default();
        m.since_unfreeze = 10.0;
        let mut w = WorldView::default();
        w.me.alive = true;
        w.me.team = Team::Terrorist;
        w.me.origin = [1100.0, 2400.0, 100.0]; // on the HE from spot
        w.bomb.planted = true;
        let owned = NadeKind::He.owned_bit();

        // Approach should immediately advance toward select (already on spot).
        let mut saw_pin = false;
        let mut saw_release = false;
        for _ in 0..40 {
            let out = m.tick(&w, 3, owned, 0.05);
            if let Some(o) = out {
                if o.rung == "util-pin" && o.attack {
                    saw_pin = true;
                }
                if o.rung == "util-throw" && !o.attack {
                    saw_release = true;
                }
            }
        }
        assert!(saw_pin, "should pin");
        assert!(saw_release, "should release throw");
        assert!(
            m.used.contains(&"t_site_he"),
            "slot marked used: {:?}",
            m.used
        );
    }

    #[test]
    fn throw_machine_no_util_in_first_seconds() {
        let mut m = ThrowMachine::default();
        let mut w = WorldView::default();
        w.me.alive = true;
        w.me.team = Team::Terrorist;
        w.me.origin = [1100.0, 2400.0, 100.0];
        w.bomb.planted = true;
        m.since_unfreeze = 0.5;
        assert!(m.tick(&w, 3, NadeKind::He.owned_bit(), 0.05).is_none());
    }

    #[test]
    fn freeze_resets_used_slots() {
        let mut m = ThrowMachine::default();
        m.used.push("t_site_he");
        let mut w = WorldView::default();
        w.me.freeze_period = true;
        m.tick(&w, 3, 0, 0.05);
        assert!(m.used.is_empty());
    }

    #[test]
    fn approach_timeout_throws_in_place() {
        // Far from lineup but trigger true — after APPROACH_TIMEOUT must pin.
        let mut m = ThrowMachine::default();
        m.since_unfreeze = 10.0;
        let mut w = WorldView::default();
        w.me.alive = true;
        w.me.team = Team::Terrorist;
        w.me.origin = [-500.0, -500.0, 0.0]; // far from A plant HE spot
        w.bomb.planted = true;
        let mut saw_select = false;
        // 5s / 0.1 = 50 ticks of approach then select
        for _ in 0..80 {
            if let Some(o) = m.tick(&w, 3, NadeKind::He.owned_bit(), 0.1) {
                if o.rung == "util-select" || o.rung == "util-pin" {
                    saw_select = true;
                    break;
                }
            }
        }
        assert!(saw_select, "timeout must force throw-in-place");
    }

    #[test]
    fn no_throw_without_inventory() {
        let mut m = ThrowMachine::default();
        m.since_unfreeze = 10.0;
        let mut w = WorldView::default();
        w.me.alive = true;
        w.me.team = Team::Terrorist;
        w.me.origin = [1160.0, 2480.0, 132.0];
        w.bomb.planted = true;
        // owned_mask 0 and not holding nade → no util.
        assert!(m.tick(&w, 3, 0, 0.05).is_none());
    }
}
