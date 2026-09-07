//! What a defender would watch from a given spot, and what it *should* watch.
//!
//! Written because bots holding a site were reported looking at walls and
//! crates instead of the ways in. "Where can an enemy come from" has two
//! plausible answers and they are not the same:
//!
//! * **visible** — the farthest walkable node with a clear line of sight in
//!   each direction. Cheap, and what `nav::watch` does today.
//! * **approach** — where the enemy's own route to this spot actually enters
//!   it, taken from paths out of their spawns. Slower, and the real answer.
//!
//! This prints both, side by side, with bearings and distances, so the
//! difference can be read rather than argued about.
//!
//! ```text
//! cargo run -p client --example watch_points -- [map] [x y z]
//! ```
//!
//! With no coordinates it uses each bomb site centre.

use std::env;

use nav::navgrid::World;
use nav::route::{self, NavSource};

fn bearing(from: [f32; 3], to: [f32; 3]) -> f32 {
    (to[1] - from[1]).atan2(to[0] - from[0]).to_degrees()
}

fn compass(deg: f32) -> &'static str {
    let d = (deg + 360.0) % 360.0;
    match (d / 45.0).round() as i32 % 8 {
        0 => "E ",
        1 => "NE",
        2 => "N ",
        3 => "NW",
        4 => "W ",
        5 => "SW",
        6 => "S ",
        _ => "SE",
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let map_name = args.next().unwrap_or_else(|| "de_dust2".to_string());
    let coords: Vec<f32> = args.filter_map(|a| a.parse().ok()).collect();

    let Some(map) = client::map::Map::load(&map_name) else {
        eprintln!("could not load {map_name} (set RUB_MAPS_DIR)");
        return;
    };
    let world = World::new(&map.bsp, &map.info);

    let spots: Vec<(String, [f32; 3])> = if coords.len() >= 3 {
        vec![("given".into(), [coords[0], coords[1], coords[2]])]
    } else {
        map.info
            .bomb_sites
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("site {}", if i == 0 { "A" } else { "B" }), s.centre()))
            .collect()
    };

    println!("map {map_name}: {} nodes", map.grid.len());
    println!(
        "T spawns {}, CT spawns {}",
        map.info.t_spawns.len(),
        map.info.ct_spawns.len()
    );

    for (label, spot) in spots {
        println!("\n=== {label} at [{:.0} {:.0} {:.0}] ===", spot[0], spot[1], spot[2]);

        println!("\n  sight_lines (the old selection, kept as a fallback):");
        let visible = nav::watch::sight_lines(
            &map.grid,
            &world,
            spot,
            nav::watch::DEFAULT_RADIUS,
            4,
        );
        if visible.is_empty() {
            println!("    (none)");
        }
        for p in &visible {
            let b = bearing(spot, *p);
            println!(
                "    {:>7.1} deg {}  {:>5.0} u   at [{:.0} {:.0} {:.0}]",
                b,
                compass(b),
                route::dist(*p, spot),
                p[0],
                p[1],
                p[2]
            );
        }

        // Where an attacker's own route actually arrives from. Path from each
        // enemy spawn to the spot, then read off the point where that path is
        // still `RING` units out: that is the direction they appear from.
        const RING: f32 = 420.0;
        println!("\n  approach (where routes from the T spawns arrive):");
        let Some(goal_node) = map.grid.nearest_prefer_z(spot) else {
            println!("    (no node near the spot)");
            continue;
        };
        let mut seen: Vec<(f32, f32, [f32; 3])> = Vec::new();
        for spawn in &map.info.t_spawns {
            let Some(start) = map.grid.nearest_prefer_z(*spawn) else {
                continue;
            };
            let Some(path) = map.grid.find_path(start, goal_node) else {
                continue;
            };
            // Walk back from the goal to the first node outside the ring.
            let entry = path
                .iter()
                .rev()
                .map(|&n| map.grid.origin(n))
                .find(|o| route::dist(*o, spot) >= RING);
            let Some(entry) = entry else { continue };
            let b = bearing(spot, entry);
            if seen
                .iter()
                .any(|(sb, _, _)| ((b - sb + 540.0) % 360.0 - 180.0).abs() < 25.0)
            {
                continue;
            }
            seen.push((b, route::dist(entry, spot), entry));
        }
        seen.sort_by(|a, b| a.0.total_cmp(&b.0));
        let _ = &seen;
        if seen.is_empty() {
            println!("    (none)");
        }
        for (b, d, p) in &seen {
            let vis = world.clear(
                nav::bsp::Hull::Point,
                [spot[0], spot[1], spot[2] + nav::watch::EYE],
                [p[0], p[1], p[2] + nav::watch::EYE],
            );
            println!(
                "    {:>7.1} deg {}  {:>5.0} u   at [{:.0} {:.0} {:.0}]  {}",
                b,
                compass(*b),
                d,
                p[0],
                p[1],
                p[2],
                if vis { "visible" } else { "BLOCKED from the spot" }
            );
        }
    
        // The answer a player actually uses: walk each attacker route outward
        // from the spot and keep the farthest point still VISIBLE from here.
        // That is the doorway or corner where they come into view -- which is
        // what you point the crosshair at, not the corridor behind it.
        println!("
  watch_points (what a defender is given now):");
        for p in nav::watch::watch_points(
            &map.grid,
            &world,
            spot,
            &map.info.t_spawns,
            nav::watch::DEFAULT_RADIUS,
            4,
        ) {
            let b = bearing(spot, p);
            println!(
                "    {:>7.1} deg {}  {:>5.0} u   at [{:.0} {:.0} {:.0}]",
                b, compass(b), route::dist(p, spot), p[0], p[1], p[2]
            );
        }

        println!("
  pop points (same thing, computed inline as a cross-check):");
        let eye = [spot[0], spot[1], spot[2] + nav::watch::EYE];
        let mut routes: Vec<Vec<usize>> = Vec::new();
        let mut worn: std::collections::HashMap<usize, u32> = std::collections::HashMap::new();
        let start = map
            .info
            .t_spawns
            .first()
            .and_then(|s| map.grid.nearest_prefer_z(*s));
        if let Some(start) = start {
            for _ in 0..4 {
                let penalty = |n: usize| -> f32 {
                    worn.get(&n).map_or(0.0, |&c| c as f32 * 4000.0)
                };
                let Some(path) = map.grid.find_path_avoiding(start, goal_node, &penalty) else {
                    break;
                };
                for &n in &path {
                    *worn.entry(n).or_insert(0) += 1;
                }
                routes.push(path);
            }
        }
        let mut pops: Vec<(f32, f32, [f32; 3])> = Vec::new();
        for path in &routes {
            let pop = path
                .iter()
                .rev()
                .map(|&n| map.grid.origin(n))
                .take_while(|o| route::dist(*o, spot) < nav::watch::DEFAULT_RADIUS)
                .filter(|o| route::dist(*o, spot) >= 120.0)
                .filter(|o| {
                    world.clear(
                        nav::bsp::Hull::Point,
                        eye,
                        [o[0], o[1], o[2] + nav::watch::EYE],
                    )
                })
                .last();
            let Some(pop) = pop else { continue };
            let b = bearing(spot, pop);
            if pops
                .iter()
                .any(|(sb, _, _)| ((b - sb + 540.0) % 360.0 - 180.0).abs() < 25.0)
            {
                continue;
            }
            pops.push((b, route::dist(pop, spot), pop));
        }
        pops.sort_by(|a, b| a.0.total_cmp(&b.0));
        if pops.is_empty() {
            println!("    (none)");
        }
        for (b, d, p) in &pops {
            println!(
                "    {:>7.1} deg {}  {:>5.0} u   at [{:.0} {:.0} {:.0}]",
                b, compass(*b), d, p[0], p[1], p[2]
            );
        }
}
}
