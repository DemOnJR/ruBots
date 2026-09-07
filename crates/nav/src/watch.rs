//! Which way a defender should be looking.
//!
//! A bot holding a position needs somewhere to point the crosshair, and the
//! honest answer is not "sweep an arc": it is *the places an enemy can come
//! from that I can see from here*. Both halves matter — a doorway behind a
//! wall is not worth watching, and an open sight line down a corridor is.
//!
//! Both come out of things the project already has. The nav lattice knows
//! where a player can walk; the BSP's own hulls answer what is visible from
//! where. So: take the walkable nodes around the spot, keep the ones with a
//! clear line of sight, group them by bearing so two nodes down the same
//! corridor do not count twice, and keep the far end of each distinct
//! direction. Those are the sight lines.

use crate::bsp::{Hull, Vec3};
use crate::navgrid::{NavGrid, World};
use crate::route::{self, NavSource};

/// Eye height above the player origin (`VEC_VIEW`, `pm_shared.h`).
pub const EYE: f32 = 17.0;

/// How far out to look for approaches, in units.
pub const DEFAULT_RADIUS: f32 = 900.0;

/// Nothing closer than this is a sight line; it is the floor at your feet.
pub const MIN_RANGE: f32 = 160.0;

/// Bearing bucket width, in degrees. A pre-filter only: it keeps the number of
/// visibility traces down by considering one candidate per slice.
const BUCKET_DEG: f32 = 15.0;

/// Two watch directions closer together than this are the same direction.
///
/// Bucketing alone does not give this: two nodes either side of a bucket edge
/// are in different buckets and ten degrees apart, and a defender watching two
/// angles ten degrees apart has wasted one of them.
pub const MIN_SEPARATION_DEG: f32 = 35.0;

/// Shortest signed turn from `a` to `b`, in degrees.
fn bearing_gap(a: f32, b: f32) -> f32 {
    let mut d = (b - a) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d < -180.0 {
        d += 360.0;
    }
    d.abs()
}

fn eye(p: Vec3) -> Vec3 {
    [p[0], p[1], p[2] + EYE]
}

/// The sight lines worth watching from `spot`, nearest bearing first.
///
/// Returns points at eye height, so a caller can aim straight at them. An
/// empty result means the position has no walkable approach in view — a
/// corner facing a wall — and the caller should fall back to whatever it did
/// before rather than pretend.
pub fn watch_points(
    grid: &NavGrid,
    world: &World,
    spot: Vec3,
    radius: f32,
    max: usize,
) -> Vec<Vec3> {
    if max == 0 {
        return Vec::new();
    }
    let from = eye(spot);
    let buckets = (360.0 / BUCKET_DEG).ceil() as usize;
    // Farthest visible node per bearing bucket: (distance, point).
    let mut best: Vec<Option<(f32, Vec3)>> = vec![None; buckets];

    for i in 0..grid.nodes.len() {
        let o = grid.origin(i);
        let d = route::dist(o, spot);
        if !(MIN_RANGE..=radius).contains(&d) {
            continue;
        }
        let bearing = (o[1] - spot[1]).atan2(o[0] - spot[0]).to_degrees();
        let bucket = (((bearing + 360.0) % 360.0) / BUCKET_DEG) as usize % buckets;
        // Only trace when this node would actually win its bucket: the trace
        // is the expensive part and most nodes lose on distance alone.
        if best[bucket].is_some_and(|(bd, _)| bd >= d) {
            continue;
        }
        if world.clear(Hull::Point, from, eye(o)) {
            best[bucket] = Some((d, eye(o)));
        }
    }

    let mut found: Vec<(f32, Vec3)> = best.into_iter().flatten().collect();
    // Farthest first: the long sight line into a site is the one that matters,
    // and a bot watching it also covers everything nearer along the same line.
    found.sort_by(|a, b| b.0.total_cmp(&a.0));

    // Then keep only genuinely different directions.
    let mut out: Vec<Vec3> = Vec::with_capacity(max);
    let mut taken: Vec<f32> = Vec::with_capacity(max);
    for (_, p) in found {
        let bearing = (p[1] - spot[1]).atan2(p[0] - spot[0]).to_degrees();
        if taken
            .iter()
            .any(|&t| bearing_gap(t, bearing) < MIN_SEPARATION_DEG)
        {
            continue;
        }
        taken.push(bearing);
        out.push(p);
        if out.len() == max {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsp::Bsp;
    use crate::entities::MapInfo;
    use crate::navgrid::NavGrid;

    fn dust2() -> Option<(Bsp, MapInfo, NavGrid)> {
        let dir = match std::env::var("AIPLAYERS_MAPS") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("testserver")
                .join("cstrike")
                .join("maps"),
        };
        let bytes = std::fs::read(dir.join("de_dust2.bsp")).ok()?;
        let bsp = Bsp::parse(&bytes).ok()?;
        let info = MapInfo::from_bsp(&bsp).ok()?;
        let checksum = crate::navgrid::checksum(&bytes);
        let grid = NavGrid::generate(&bsp, &info, checksum);
        Some((bsp, info, grid))
    }

    #[test]
    fn a_bomb_site_has_several_distinct_sight_lines_and_all_are_visible() {
        let Some((bsp, info, grid)) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let world = World::new(&bsp, &info);
        let site = info
            .bomb_sites
            .first()
            .map(|s| s.centre())
            .expect("de_dust2 has bomb sites");

        let points = watch_points(&grid, &world, site, DEFAULT_RADIUS, 4);
        assert!(
            points.len() >= 2,
            "a bomb site is approachable from more than one way, got {}",
            points.len()
        );

        let from = eye(site);
        for p in &points {
            assert!(
                world.clear(Hull::Point, from, *p),
                "returned a point that cannot be seen from the spot"
            );
            let d = route::dist(*p, site);
            assert!(
                (MIN_RANGE..=DEFAULT_RADIUS + 1.0).contains(&d),
                "point at {d} is outside the range asked for"
            );
        }

        // Distinct directions, not four nodes down one corridor.
        let mut bearings: Vec<f32> = points
            .iter()
            .map(|p| (p[1] - site[1]).atan2(p[0] - site[0]).to_degrees())
            .collect();
        bearings.sort_by(f32::total_cmp);
        for (i, a) in bearings.iter().enumerate() {
            for b in &bearings[i + 1..] {
                assert!(
                    bearing_gap(*a, *b) >= MIN_SEPARATION_DEG - 0.001,
                    "two watch points are the same direction: {a} and {b}"
                );
            }
        }
    }

    #[test]
    fn asking_for_none_returns_none_and_a_sealed_spot_returns_empty() {
        let Some((bsp, info, grid)) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let world = World::new(&bsp, &info);
        let site = info.bomb_sites.first().map(|s| s.centre()).expect("sites");
        assert!(watch_points(&grid, &world, site, DEFAULT_RADIUS, 0).is_empty());
        // Deep inside solid geometry nothing is visible, and that is an empty
        // answer rather than a wrong one.
        let sealed = [site[0], site[1], site[2] - 4096.0];
        assert!(watch_points(&grid, &world, sealed, DEFAULT_RADIUS, 4).is_empty());
    }
}
