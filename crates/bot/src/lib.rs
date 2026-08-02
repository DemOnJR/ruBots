//! The autonomous player: what it wants to do, and how it aims.
//!
//! Port of `internal/bot/` from `aiplayers-gui.exe`. The AI is deliberately
//! decoupled from the netcode: everything here operates on a plain world
//! snapshot, so behaviour can be tested without a game server attached. The
//! client layer's job is to fill that snapshot in and drain the resulting
//! commands.

pub mod aim;
pub mod combat;
pub mod controller;
pub mod math;
pub mod objective;
pub mod rng;
pub mod task;
pub mod world;

pub use aim::{aim_error, turn_toward};
pub use combat::{engage, select_target, EngageParams, Engagement};
pub use controller::{Controller, Intent};
pub use math::{aim_angles, distance, distance2d, norm_angle, Angles, Vec3};
pub use objective::{Objective, ObjectiveState};
pub use rng::Rng;
pub use task::{Difficulty, Task};
pub use world::{BombState, PlayerView, SelfState, Team, WorldView};
