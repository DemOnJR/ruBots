//! Radar projection: map world coordinates -> screen pixels.
//!
//! The map silhouette is generated from the nav grid itself -- every walkable
//! node origin on the 40-unit lattice is a pixel, so the radar is accurate for
//! ANY map with zero per-map art. Layers color by **height**, **flags**, and
//! **special hops** (jump / crouch / ladder / fall) so multi-level paths and
//! doors are visible.

use nav::navgrid::{flags, Move, NavGrid};

/// A 2D projection of world coordinates onto a `width x height` canvas.
///
/// CS uses +y as north; the radar flips y so north is up on screen.
pub struct Projection {
    pub min_x: f32,
    pub min_y: f32,
    pub span_x: f32,
    pub span_y: f32,
}

impl Projection {
    pub fn from_grid(grid: &NavGrid) -> Self {
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for n in &grid.nodes {
            let o = n.origin;
            min_x = min_x.min(o[0]);
            min_y = min_y.min(o[1]);
            max_x = max_x.max(o[0]);
            max_y = max_y.max(o[1]);
        }
        if max_x <= min_x {
            max_x = min_x + 1.0;
        }
        if max_y <= min_y {
            max_y = min_y + 1.0;
        }
        Self {
            min_x,
            min_y,
            span_x: max_x - min_x,
            span_y: max_y - min_y,
        }
    }

    /// World (x, y) -> normalized (0..1, 0..1), y flipped (north up).
    pub fn norm(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.min_x) / self.span_x,
            1.0 - (y - self.min_y) / self.span_y,
        )
    }

    /// World -> screen pixels on a `w x h` canvas, with a margin.
    pub fn screen(&self, x: f32, y: f32, w: f32, h: f32) -> (f32, f32) {
        let (nx, ny) = self.norm(x, y);
        (nx * w, ny * h)
    }
}

/// Height band for multi-level dust2-style maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightBand {
    /// Underpasses / tunnels / low ground (z < ~40 at player origin).
    Low,
    /// Mid level.
    Mid,
    /// High platforms (A site on dust2 ≈ 144 origin).
    High,
}

impl HeightBand {
    pub fn from_z(z: f32) -> Self {
        // Player origin is ~36 above floor; A platform floor ~96–108 → origin ~132–144.
        if z < 70.0 {
            Self::Low
        } else if z < 120.0 {
            Self::Mid
        } else {
            Self::High
        }
    }

    /// Default walkable color for this band (RGB).
    pub fn color(self) -> (u8, u8, u8) {
        match self {
            Self::Low => (40, 70, 95),  // deep blue-grey — under / tunnels
            Self::Mid => (52, 62, 74),  // neutral grey
            Self::High => (70, 85, 55), // olive — upper decks
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low/under",
            Self::Mid => "mid",
            Self::High => "high",
        }
    }
}

/// One walkable node for the radar.
#[derive(Clone, Copy)]
pub struct LayerPoint {
    pub nx: f32,
    pub ny: f32,
    pub z: f32,
    pub band: HeightBand,
    pub ladder: bool,
    pub narrow: bool, // crouch / door-scale
    pub goal: bool,
}

/// Special hop edge (jump, crouch, ladder, fall) for overlays.
#[derive(Clone, Copy)]
pub struct HopEdge {
    pub ax: f32,
    pub ay: f32,
    pub bx: f32,
    pub by: f32,
    pub kind: Move,
}

/// Precomputed radar layers from a nav grid.
pub struct RadarBackground {
    pub points: Vec<LayerPoint>,
    /// Jump / crouch / ladder / fall edges (normalized coords).
    pub hops: Vec<HopEdge>,
    pub z_min: f32,
    pub z_max: f32,
}

impl RadarBackground {
    pub fn from_grid(grid: &NavGrid) -> Self {
        let proj = Projection::from_grid(grid);
        let mut points = Vec::with_capacity(grid.nodes.len());
        let mut hops = Vec::new();
        let mut z_min = f32::MAX;
        let mut z_max = f32::MIN;
        for (i, n) in grid.nodes.iter().enumerate() {
            let o = n.origin;
            z_min = z_min.min(o[2]);
            z_max = z_max.max(o[2]);
            let (nx, ny) = proj.norm(o[0], o[1]);
            points.push(LayerPoint {
                nx,
                ny,
                z: o[2],
                band: HeightBand::from_z(o[2]),
                ladder: n.flags & flags::LADDER != 0,
                narrow: n.flags & flags::NARROW != 0 || n.radius < 16.0,
                goal: n.flags & flags::GOAL != 0,
            });
            for link in &n.links {
                if matches!(
                    link.kind,
                    Move::Jump | Move::Crouch | Move::Ladder | Move::Fall
                ) {
                    let to = link.to as usize;
                    if to >= grid.nodes.len() {
                        continue;
                    }
                    let t = grid.nodes[to].origin;
                    let (bx, by) = proj.norm(t[0], t[1]);
                    // Only store one direction for fall/jump to cut clutter.
                    if link.kind == Move::Fall && o[2] < t[2] {
                        continue;
                    }
                    let _ = i;
                    hops.push(HopEdge {
                        ax: nx,
                        ay: ny,
                        bx,
                        by,
                        kind: link.kind,
                    });
                }
            }
        }
        if z_min > z_max {
            z_min = 0.0;
            z_max = 1.0;
        }
        Self {
            points,
            hops,
            z_min,
            z_max,
        }
    }
}

/// RGB for a hop edge kind.
pub fn hop_color(kind: Move) -> (u8, u8, u8) {
    match kind {
        Move::Jump => (255, 180, 40),    // orange — jump lip / crate
        Move::Crouch => (180, 100, 220), // purple — duck / narrow door
        Move::Ladder => (80, 200, 120),  // green — ladder
        Move::Fall => (100, 160, 220),   // light blue — drop / under path
        _ => (120, 120, 120),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_maps_world_to_screen_with_y_flipped() {
        let p = Projection {
            min_x: 0.0,
            min_y: 0.0,
            span_x: 1000.0,
            span_y: 1000.0,
        };
        let (sx, sy) = p.screen(0.0, 0.0, 500.0, 500.0);
        assert!((sx - 0.0).abs() < 1e-3);
        assert!((sy - 500.0).abs() < 1e-3, "y should be flipped, got {sy}");
        let (sx, sy) = p.screen(1000.0, 1000.0, 500.0, 500.0);
        assert!((sx - 500.0).abs() < 1e-3);
        assert!((sy - 0.0).abs() < 1e-3);
        let (sx, sy) = p.screen(500.0, 500.0, 500.0, 500.0);
        assert!((sx - 250.0).abs() < 1e-3);
        assert!((sy - 250.0).abs() < 1e-3);
    }

    #[test]
    fn negative_world_coords_project_correctly() {
        let p = Projection {
            min_x: -2500.0,
            min_y: -2500.0,
            span_x: 5500.0,
            span_y: 5500.0,
        };
        let (sx, sy) = p.screen(-2500.0, 3000.0, 550.0, 550.0);
        assert!((sx - 0.0).abs() < 1e-3);
        assert!(
            (sy - 0.0).abs() < 1e-3,
            "north edge should be screen top, got {sy}"
        );
    }

    #[test]
    fn height_bands_split_dust2_levels() {
        assert_eq!(HeightBand::from_z(40.0), HeightBand::Low);
        assert_eq!(HeightBand::from_z(90.0), HeightBand::Mid);
        assert_eq!(HeightBand::from_z(140.0), HeightBand::High);
    }
}
