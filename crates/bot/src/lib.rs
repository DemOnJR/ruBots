//! The autonomous player: what it wants to do, how it aims, and when it
//! pulls the trigger.
//!
//! Port of `internal/bot/` from `aiplayers-gui.exe`. The AI is deliberately
//! decoupled from the netcode: everything here operates on a plain world
//! snapshot, so behaviour can be tested without a game server attached. The
//! client layer's job is to fill that snapshot in and drain the resulting
//! commands.
//!
//! ## The one idea worth carrying between modules
//!
//! **Prefer doing nothing to acting on a guess, and prefer an observation to a
//! simulation.** Concretely, that shows up as:
//!
//! * an unknown weapon has fire class `NotAWeapon`, so the trigger stays up
//!   ([`weapons`]);
//! * the trigger is gated on `m_flNextPrimaryAttack` and `m_iShotsFired` as the
//!   *server* reports them, not on a local model of the cycle time ([`fire`]);
//! * a plant is finished when the server says the bomb is planted, not when a
//!   local stopwatch reaches three seconds ([`objective::bomb`]);
//! * an enemy whose view angles are missing is not treated as aiming at us
//!   ([`world`]);
//! * a command that cannot be rendered is not sent ([`intent`]).
//!
//! Behaviour is verified against ReGameDLL_CS and ReHLDS, with `file:line`
//! citations at each claim. Where a number is a design choice rather than a
//! recovered fact, it says so.

pub mod aim;
pub mod combat;
pub mod controller;
pub mod economy;
pub mod fire;
pub mod idle;
pub mod intent;
pub mod math;
pub mod objective;
pub mod rng;
pub mod task;
pub mod team;
pub mod utility;
pub mod weapons;
pub mod world;

pub use aim::{
    aim_error, compensate, predict_punch, turn_toward, SpringGains, ViewMotion, COMBAT_GAINS,
    NAV_GAINS,
};
pub use combat::{engage, select_target, threat_score, EngageParams, Engagement};
pub use controller::Controller;
pub use economy::{build_buy_plan, classify_buy, BuyClass};
pub use fire::{FireAction, FireControl, FireParams, HoldFire, WeaponState};
pub use idle::AntiIdle;
pub use intent::{BotCommand, Intent};
pub use math::{aim_angles, distance, distance2d, forward, norm_angle, Angles, Vec3};
pub use objective::bomb::{DefuseMachine, DefusePhase, PlantMachine, PlantPhase};
pub use objective::hostage::{EscortPhase, HostageEscort};
pub use objective::{Objective, ObjectiveState};
pub use rng::Rng;
pub use task::{Difficulty, Task};
pub use team::{PlantSite, TeamReport, TeamSnapshot};
pub use utility::{holding_nade, my_slots, NadeKind, ThrowMachine, UtilitySlot};
pub use weapons::{Equipment, FireClass, WeaponId, WeaponInfo, WEAPONS};
pub use world::{BombState, HostageView, PlayerView, SelfState, Team, WorldView};
