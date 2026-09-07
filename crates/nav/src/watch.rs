//! Which way a defender should be looking.
//!
//! A bot holding a position needs somewhere to point the crosshair, and
//! "sweep an arc" is not it. Nor, it turns out, is "the farthest thing you can
//! see in each direction": that was the first answer here, and on de_dust2 it
//! sent a bot holding a site to watch a back wall 606 units away at a bearing
//! no attacker ever comes from. Measured, at the site centre:
//!
//! ```text
//! visible:      -99 deg 900u   147 deg 649u   -155 deg 606u   -57 deg 249u
//! pop points:  -110 deg 306u   -72 deg 557u
//! ```
//!
//! The four "visible" directions fan out all round the room. The two pop
//! points are the tunnel mouth and the door -- the ways in.
//!
//! ## What a player actually watches
//!
//! Not the corridor, and not the doorway in the abstract: **the place where
//! someone coming down that corridor first appears in your view**. So:
//!
//! 1. Route from the enemy's spawn to this spot, with the nav graph.
//! 2. Walk that route back out from the spot and keep the farthest point still
//!    visible from here. That is where they pop into view.
//! 3. Force a different route (charge the nodes already used) and repeat, so
//!    two entrances are two angles rather than one corridor twice.
//!
//! Step 2 is the one that matters. The point where the route *enters* the site
//! is usually **not visible** from inside it -- measured on both dust2 sites --
//! so watching it means staring at the wall in front of it, which is exactly
//! what was reported.
//!
//! [`sight_lines`] is kept as the fallback for when there is no route to work
//! with (no spawns known, unreachable spot), because a wrong angle still beats
//! no angle: a bot with nothing to watch does not move its head at all.

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

/// Fallback: the farthest visible walkable node in each distinct direction.
///
/// Used when [`watch_points`] has no route to work from. It answers "what can
/// I see" rather than "where will they come from", which is why it is the
/// fallback and not the answer — see the module docs.
pub fn sight_lines(
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
    fn the_fallback_returns_distinct_visible_directions() {
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

        let points = sight_lines(&grid, &world, site, DEFAULT_RADIUS, 4);
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

    /// The point of the whole module: what a defender watches is where the
    /// attacker appears, and it must be *visible from the spot*. The route's
    /// own entry into the site is usually not -- which is the bug this
    /// replaced, a bot staring at the wall in front of the doorway.
    #[test]
    fn every_watch_point_is_visible_from_the_spot_it_defends() {
        let Some((bsp, info, grid)) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let world = World::new(&bsp, &info);
        for site in info.bomb_sites.iter().map(|s| s.centre()) {
            let points = watch_points(&grid, &world, site, &info.t_spawns, DEFAULT_RADIUS, 4);
            assert!(!points.is_empty(), "a bomb site must be watchable somehow");
            let from = eye(site);
            for p in &points {
                assert!(
                    world.clear(Hull::Point, from, *p),
                    "watching a point that cannot be seen from the spot: {p:?}"
                );
                let d = route::dist(*p, site);
                assert!(
                    (MIN_POP..=DEFAULT_RADIUS + 1.0).contains(&d),
                    "watch point at {d} u is not an entrance"
                );
            }
            // Distinct directions, not one corridor sampled twice.
            let bearings: Vec<f32> = points
                .iter()
                .map(|p| (p[1] - site[1]).atan2(p[0] - site[0]).to_degrees())
                .collect();
            for (i, a) in bearings.iter().enumerate() {
                for b in &bearings[i + 1..] {
                    assert!(
                        bearing_gap(*a, *b) >= MIN_SEPARATION_DEG - 0.001,
                        "two watch points are the same direction: {a} and {b}"
                    );
                }
            }
        }
    }

    /// A defender watches ground the attacker walks on, not the scenery.
    ///
    /// This is the reported bug, stated as a property: every watch point must
    /// lie on a route an enemy would actually take to this spot. The old
    /// selection -- farthest visible node per bearing -- fails it, because
    /// "visible" says nothing about "walked through".
    #[test]
    fn every_watch_point_lies_on_a_route_an_attacker_would_take() {
        let Some((bsp, info, grid)) = dust2() else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let world = World::new(&bsp, &info);
        for site in info.bomb_sites.iter().map(|s| s.centre()) {
            let watch = watch_points(&grid, &world, site, &info.t_spawns, DEFAULT_RADIUS, 4);
            assert!(!watch.is_empty());

            // Every node on any of the routes an attacker might take, worked
            // out independently of the code under test.
            let goal = grid.nearest_prefer_z(site).expect("a node at the site");
            let start = info
                .t_spawns
                .iter()
                .find_map(|s| grid.nearest_prefer_z(*s))
                .expect("a node at a T spawn");
            let mut on_route: Vec<Vec3> = Vec::new();
            let mut worn: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
            for _ in 0..ROUTES {
                let penalty = |n: usize| worn.get(&n).map_or(0.0, |&c| c as f32 * ROUTE_WEAR);
                let Some(path) = grid.find_path_avoiding(start, goal, &penalty) else {
                    break;
                };
                for &n in &path {
                    *worn.entry(n).or_insert(0) += 1;
                    on_route.push(grid.origin(n));
                }
            }
            assert!(!on_route.is_empty(), "no route to the site at all");

            for p in &watch {
                // The watch point sits at eye height above a node; compare on
                // the floor plane the route is expressed in.
                let floor = [p[0], p[1], p[2] - EYE];
                let nearest = on_route
                    .iter()
                    .map(|o| route::dist(*o, floor))
                    .fold(f32::MAX, f32::min);
                assert!(
                    nearest <= 1.0,
                    "watching [{:.0} {:.0} {:.0}], which is {nearest:.0} u from                      anywhere an attacker walks",
                    p[0],
                    p[1],
                    p[2]
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
        assert!(sight_lines(&grid, &world, site, DEFAULT_RADIUS, 0).is_empty());
        // Deep inside solid geometry nothing is visible, and that is an empty
        // answer rather than a wrong one.
        let sealed = [site[0], site[1], site[2] - 4096.0];
        assert!(sight_lines(&grid, &world, sealed, DEFAULT_RADIUS, 4).is_empty());
    }
}

/// How many alternative attacker routes to look for.
const ROUTES: usize = 4;

/// What each already-used node costs the next route search.
///
/// Large enough that A* genuinely leaves the corridor rather than clipping a
/// corner of it, which is the difference between two entrances and one
/// entrance twice.
const ROUTE_WEAR: f32 = 4000.0;

/// Closer than this and it is the floor at your feet, not an entrance.
const MIN_POP: f32 = 120.0;

/// Where attackers coming from `enemy_spawns` will first come into view.
///
/// Returns points at eye height, sorted by bearing, no two within
/// [`MIN_SEPARATION_DEG`] of each other. Falls back to [`sight_lines`] when
/// there is no usable route -- an empty answer would leave the caller with
/// nothing to look at at all.
pub fn watch_points(
    grid: &NavGrid,
    world: &World,
    spot: Vec3,
    enemy_spawns: &[Vec3],
    radius: f32,
    max: usize,
) -> Vec<Vec3> {
    if max == 0 {
        return Vec::new();
    }
    let from = eye(spot);
    let mut out: Vec<Vec3> = Vec::with_capacity(max);
    let mut taken: Vec<f32> = Vec::with_capacity(max);

    if let Some(goal) = grid.nearest_prefer_z(spot) {
        // One start is enough: twenty spawn points in one room all route the
        // same way, so route diversity has to come from charging the nodes
        // already used, not from picking a different corner of the spawn.
        let start = enemy_spawns
            .iter()
            .find_map(|s| grid.nearest_prefer_z(*s));
        if let Some(start) = start {
            let mut worn: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
            for _ in 0..ROUTES {
                if out.len() == max {
                    break;
                }
                let penalty = |n: usize| worn.get(&n).map_or(0.0, |&c| c as f32 * ROUTE_WEAR);
                let Some(path) = grid.find_path_avoiding(start, goal, &penalty) else {
                    break;
                };
                for &n in &path {
                    *worn.entry(n).or_insert(0) += 1;
                }
                // Outward from the spot, the last point still in view.
                // Walking outward along their route: skip what is underfoot,
                // then follow the UNBROKEN run of visible nodes and stop where
                // it breaks. The far end of that run is the corner they come
                // round and stay in view. Taking the farthest visible node
                // instead would pick a spot past a wall where they flicker
                // into sight and vanish again -- measured on de_dust2, that
                // put a watch point 121 degrees away from the route.
                let pop = path
                    .iter()
                    .rev()
                    .map(|&n| grid.origin(n))
                    .take_while(|o| route::dist(*o, spot) < radius)
                    .skip_while(|o| route::dist(*o, spot) < MIN_POP)
                    .take_while(|o| world.clear(Hull::Point, from, eye(*o)))
                    .last();
                let Some(pop) = pop else { continue };
                let b = (pop[1] - spot[1]).atan2(pop[0] - spot[0]).to_degrees();
                if taken.iter().any(|&t| bearing_gap(t, b) < MIN_SEPARATION_DEG) {
                    continue;
                }
                taken.push(b);
                out.push(eye(pop));
            }
        }
    }

    if out.is_empty() {
        return sight_lines(grid, world, spot, radius, max);
    }
    out
}
