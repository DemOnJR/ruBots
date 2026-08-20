//! Navigation: waypoint graph, pathfinding, and BSP line-of-sight.
//!
//! Handles waypoint graphs (`.graph`), collision hull navigation grids,
//! A* route generation, and line-of-sight raytracing.

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
