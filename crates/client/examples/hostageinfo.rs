//! Geometry of a hostage map, offline: where the hostages start, where the
//! rescue zone is, and how far a CT actually has to walk with one in tow.
//!
//! ```text
//! cargo run -p client --example hostageinfo -- cs_italy cs_office
//! ```

use std::env;

fn d(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

fn main() {
    let names: Vec<String> = env::args().skip(1).collect();
    for name in names {
        let Some(m) = client::map::Map::load(&name) else {
            println!("{name}: NOT FOUND");
            continue;
        };
        let i = &m.info;
        println!("== {name} ==");
        for (n, z) in i.rescue_zones.iter().enumerate() {
            let c = z.centre();
            println!("  rescue[{n}] centre [{:.0} {:.0} {:.0}]", c[0], c[1], c[2]);
        }
        for (n, h) in i.hostage_spawns.iter().enumerate() {
            println!("  hostage[{n}]     [{:.0} {:.0} {:.0}]", h[0], h[1], h[2]);
        }
        for (n, s) in i.ct_spawns.iter().take(2).enumerate() {
            println!("  ct_spawn[{n}]    [{:.0} {:.0} {:.0}]", s[0], s[1], s[2]);
        }

        let Some(&ct) = i.ct_spawns.first() else { continue };
        let Some(&h0) = i.hostage_spawns.first() else { continue };
        let zone = i.rescue_zones.first().map(|z| z.centre());

        println!("  straight-line ct_spawn -> hostage0: {:.0}", d(ct, h0));
        if let Some(z) = zone {
            println!("  straight-line hostage0 -> rescue0: {:.0}", d(h0, z));
        }

        // Route lengths, in world units, along the nav graph.
        let route = |a: [f32; 3], b: [f32; 3]| -> Option<(usize, f32)> {
            let (na, nb) = (m.grid.nearest(a)?, m.grid.nearest(b)?);
            let p = m.grid.find_path(na, nb)?;
            let mut len = 0.0;
            let mut prev = a;
            for n in &p {
                let pos = nav::route::NavSource::origin(&m.grid, *n);
                len += d(prev, pos);
                prev = pos;
            }
            Some((p.len(), len))
        };
        match route(ct, h0) {
            Some((n, l)) => println!("  route ct_spawn -> hostage0: {n} waypoints, {l:.0} units"),
            None => println!("  route ct_spawn -> hostage0: NO PATH"),
        }
        if let Some(z) = zone {
            match route(h0, z) {
                Some((n, l)) => println!("  route hostage0 -> rescue0: {n} waypoints, {l:.0} units"),
                None => println!("  route hostage0 -> rescue0: NO PATH"),
            }
        }
    }
}
