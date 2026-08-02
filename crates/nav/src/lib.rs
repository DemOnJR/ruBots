//! Navigation: waypoint graph, pathfinding, and (later) BSP line-of-sight.
//!
//! Port of `internal/nav/`. The graph format is YaPB's `.graph`, identified
//! from three constants in `LoadGraph` — see [`graph`].
//!
//! Still to port: `ulzUncompress` (the graph payload is ULZ-compressed) and
//! `bsp.rs` (`Visible`, `TraceFraction`, `GroundHeight`).

pub mod bsp;
pub mod graph;
pub mod ulz;

pub use bsp::{Bsp, BspError, Trace};
pub use graph::{flags, Graph, GraphError, Header, Link, Node, Vec3};
pub use ulz::UlzError;
