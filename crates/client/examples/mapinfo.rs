//! What the nav layer makes of a map, without a server.
//!
//! The hostage and bomb objectives both come from the BSP's entity lump, not
//! from anything the server sends -- the `Scenario` user message is gated on
//! Condition Zero and is normally absent. So "can the bots play this map at
//! all" is answerable offline, and worth answering before spending a round
//! finding out.
//!
//! ```text
//! cargo run -p client --example mapinfo -- cs_italy cs_office de_dust2
//! ```

use nav::route::NavSource;
use std::env;

fn main() {
    let names: Vec<String> = env::args().skip(1).collect();
    let names = if names.is_empty() {
        vec!["de_dust2".to_string()]
    } else {
        names
    };

    for name in names {
        match client::map::Map::load(&name) {
            None => println!("{name:<16} NOT FOUND (no .bsp on any search path)"),
            Some(m) => {
                let i = &m.info;
                println!(
                    "{name:<16} {:?}  nodes {:<6} bombsites {}  rescue {}  \
                     T spawns {}  CT spawns {}  hostages {}",
                    i.scenario,
                    m.grid.len(),
                    i.bomb_sites.len(),
                    i.rescue_zones.len(),
                    i.t_spawns.len(),
                    i.ct_spawns.len(),
                    i.hostage_spawns.len(),
                );

                // Can the bot physically REACH the objective it is given?
                // `ARRIVE_RADIUS` is 24 units, so if the nearest walkable node
                // is further than that from the objective's centre, the bot
                // can never arrive and the plant rung never fires -- it just
                // stands as close as the graph allows, forever.
                for (label, goal) in i
                    .bomb_sites
                    .iter()
                    .map(|z| ("bombsite", z.centre()))
                    .chain(i.rescue_zones.iter().map(|z| ("rescue  ", z.centre())))
                {
                    match m.grid.nearest(goal) {
                        Some(n) => {
                            let o = m.grid.origin(n);
                            let d = ((o[0] - goal[0]).powi(2) + (o[1] - goal[1]).powi(2)).sqrt();
                            let verdict = if d < 24.0 { "reachable" } else { "TOO FAR" };
                            println!(
                                "    {label} [{:.0} {:.0} {:.0}] nearest node {:.0} units away  {verdict}",
                                goal[0], goal[1], goal[2], d
                            );
                        }
                        None => println!("    {label}: no node at all"),
                    }
                }

                // A route from a T spawn to the objective is the real question:
                // a graph with the right node count is still useless if the
                // spawn sits in its own disconnected component, which is how
                // three of the cs_ maps actually behave.
                for (label, is_ct) in [("T ", false), ("CT", true)] {
                    let from = if is_ct { i.ct_spawns.first() } else { i.t_spawns.first() };
                    let (Some(&from), Some(goal)) = (from, m.objective(is_ct, 0)) else {
                        println!("    {label} -> objective: no spawn or no objective");
                        continue;
                    };
                    match (m.grid.nearest(from), m.grid.nearest(goal)) {
                        (Some(a), Some(b)) => match m.grid.find_path(a, b) {
                            Some(p) => println!("    {label} -> objective: {} waypoints", p.len()),
                            None => println!(
                                "    {label} -> objective: NO PATH -- spawn is in a \
                                 disconnected component"
                            ),
                        },
                        _ => println!("    {label} -> objective: no node near spawn or goal"),
                    }
                }
            }
        }
    }
}
