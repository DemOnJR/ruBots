//! Navigation: waypoint graph, pathfinding, and (later) BSP line-of-sight.
//!
//! Port of `internal/nav/`. The graph format is YaPB's `.graph`, identified
//! from three constants in `LoadGraph` — see [`graph`].
//!
//! Still to port: `ulzUncompress` (the graph payload is ULZ-compressed) and
//! `bsp.rs` (`Visible`, `TraceFraction`, `GroundHeight`).

pub mod bsp;
pub mod entities;
pub mod graph;
pub mod navgrid;
pub mod route;
pub mod ulz;

pub use bsp::{Bsp, BspError, Hull, Trace};
pub use entities::{Aabb, Entity, EntityError, MapInfo, Scenario};
pub use graph::{flags, Graph, GraphError, Header, Link, Node, Vec3};
pub use navgrid::{Move, NavError, NavGrid, NavNode};
pub use route::NavSource;
pub use ulz::UlzError;
