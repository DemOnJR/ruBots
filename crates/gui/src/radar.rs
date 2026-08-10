//! Radar projection: map world coordinates -> screen pixels.
//!
//! The map silhouette is generated from the nav grid itself -- every walkable
//! node origin on the 40-unit lattice is a pixel, so the radar is accurate for
//! ANY map with zero per-map art. The world bounds come from the BSP's world
//! model (model 0), which is what the classic overview transform uses.

use nav::navgrid::NavGrid;

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
        let (mut min_x, mut min_y, mut max_x, mut max_y) =
            (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
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

/// Precompute the radar background as a dense grid of (nx, ny) walkable
/// points, so the GUI draws them once per frame cheaply.
pub struct RadarBackground {
    pub points: Vec<(f32, f32)>,
}

impl RadarBackground {
    pub fn from_grid(grid: &NavGrid) -> Self {
        let proj = Projection::from_grid(grid);
        let mut points = Vec::with_capacity(grid.nodes.len());
        for n in &grid.nodes {
            points.push(proj.norm(n.origin[0], n.origin[1]));
        }
        Self { points }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_maps_world_to_screen_with_y_flipped() {
        let p = Projection { min_x: 0.0, min_y: 0.0, span_x: 1000.0, span_y: 1000.0 };
        // World (0, 0) is bottom-left -> screen (0, H) with north up.
        let (sx, sy) = p.screen(0.0, 0.0, 500.0, 500.0);
        assert!((sx - 0.0).abs() < 1e-3);
        assert!((sy - 500.0).abs() < 1e-3, "y should be flipped, got {sy}");
        // World (1000, 1000) is top-right -> screen (W, 0).
        let (sx, sy) = p.screen(1000.0, 1000.0, 500.0, 500.0);
        assert!((sx - 500.0).abs() < 1e-3);
        assert!((sy - 0.0).abs() < 1e-3);
        // Middle maps to middle.
        let (sx, sy) = p.screen(500.0, 500.0, 500.0, 500.0);
        assert!((sx - 250.0).abs() < 1e-3);
        assert!((sy - 250.0).abs() < 1e-3);
    }

    #[test]
    fn negative_world_coords_project_correctly() {
        // de_dust2 spans roughly -2500..3000.
        let p = Projection { min_x: -2500.0, min_y: -2500.0, span_x: 5500.0, span_y: 5500.0 };
        let (sx, sy) = p.screen(-2500.0, 3000.0, 550.0, 550.0);
        assert!((sx - 0.0).abs() < 1e-3);
        assert!((sy - 0.0).abs() < 1e-3, "north edge should be screen top, got {sy}");
    }
}
