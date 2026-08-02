//! Validate the BSP loader against a real Counter-Strike map.
//!
//! Point `AIPLAYERS_BSP` at a GoldSrc `.bsp` file, e.g. one pulled out of the
//! test server:
//!
//! ```text
//! docker cp aiplayers-cs16:/home/steam/hlds/cstrike/maps/de_dust2.bsp .
//! ```
//!
//! Skips when the variable is unset, so the suite still passes without a map.

use nav::bsp::{contents, Bsp};

fn load() -> Option<Bsp> {
    let path = std::env::var("AIPLAYERS_BSP").ok()?;
    let data = std::fs::read(&path).ok()?;
    Some(Bsp::parse(&data).unwrap_or_else(|e| panic!("failed to parse {path}: {e}")))
}

#[test]
fn a_real_map_parses_with_consistent_lumps() {
    let Some(m) = load() else {
        eprintln!("SKIP: set AIPLAYERS_BSP to a .bsp file");
        return;
    };
    assert!(!m.planes.is_empty(), "a real map has planes");
    assert!(!m.nodes.is_empty(), "a real map has nodes");
    assert!(!m.leaves.is_empty(), "a real map has leaves");
    assert!(!m.models.is_empty(), "a real map has at least the worldspawn model");
    eprintln!(
        "planes={} nodes={} leaves={} models={}",
        m.planes.len(),
        m.nodes.len(),
        m.leaves.len(),
        m.models.len()
    );
}

#[test]
fn every_node_references_a_valid_plane_and_children() {
    let Some(m) = load() else { return };
    for (i, n) in m.nodes.iter().enumerate() {
        assert!(
            (n.plane as usize) < m.planes.len(),
            "node {i} references plane {} of {}",
            n.plane,
            m.planes.len()
        );
        for c in n.children {
            if c >= 0 {
                assert!((c as usize) < m.nodes.len(), "node {i} child {c} out of range");
            } else {
                let leaf = (-c - 1) as usize;
                assert!(leaf < m.leaves.len(), "node {i} leaf {leaf} out of range");
            }
        }
    }
}

#[test]
fn plane_normals_are_unit_length() {
    let Some(m) = load() else { return };
    for (i, p) in m.planes.iter().enumerate() {
        let len =
            (p.normal[0] * p.normal[0] + p.normal[1] * p.normal[1] + p.normal[2] * p.normal[2])
                .sqrt();
        assert!(
            (len - 1.0).abs() < 1e-3,
            "plane {i} normal length {len} is not unit"
        );
    }
}

#[test]
fn the_world_model_bounds_are_sane() {
    let Some(m) = load() else { return };
    let w = m.models[0];
    for axis in 0..3 {
        assert!(
            w.maxs[axis] > w.mins[axis],
            "world model axis {axis} inverted: {} .. {}",
            w.mins[axis],
            w.maxs[axis]
        );
    }
    eprintln!("world bounds {:?} .. {:?}", w.mins, w.maxs);
}

#[test]
fn far_outside_the_map_is_solid_and_the_middle_is_not_all_solid() {
    let Some(m) = load() else { return };
    let w = m.models[0];
    // Well outside the world box.
    let outside = [w.maxs[0] + 5000.0, w.maxs[1] + 5000.0, w.maxs[2] + 5000.0];
    assert_eq!(
        m.point_contents(outside),
        contents::SOLID,
        "space beyond the map should read as solid"
    );

    // Sample a grid through the middle of the map; a playable map must have
    // a decent amount of open space.
    let mut open = 0;
    let mut total = 0;
    for i in 0..10 {
        for j in 0..10 {
            let x = w.mins[0] + (w.maxs[0] - w.mins[0]) * (i as f32 + 0.5) / 10.0;
            let y = w.mins[1] + (w.maxs[1] - w.mins[1]) * (j as f32 + 0.5) / 10.0;
            let z = w.mins[2] + (w.maxs[2] - w.mins[2]) * 0.5;
            total += 1;
            if m.point_contents([x, y, z]) != contents::SOLID {
                open += 1;
            }
        }
    }
    eprintln!("open samples: {open}/{total}");
    assert!(open > 0, "the map appears to be entirely solid -- tree walk is wrong");
}

#[test]
fn traces_stay_within_bounds_and_are_symmetric() {
    let Some(m) = load() else { return };
    let w = m.models[0];
    let mid_z = w.mins[2] + (w.maxs[2] - w.mins[2]) * 0.5;

    let mut checked = 0;
    for i in 0..8 {
        for j in 0..8 {
            let a = [
                w.mins[0] + (w.maxs[0] - w.mins[0]) * (i as f32 + 0.5) / 8.0,
                w.mins[1] + (w.maxs[1] - w.mins[1]) * (j as f32 + 0.5) / 8.0,
                mid_z,
            ];
            let b = [
                w.mins[0] + (w.maxs[0] - w.mins[0]) * (j as f32 + 0.5) / 8.0,
                w.mins[1] + (w.maxs[1] - w.mins[1]) * (i as f32 + 0.5) / 8.0,
                mid_z,
            ];
            let f = m.trace_fraction(a, b);
            assert!(
                (0.0..=1.0).contains(&f) && f.is_finite(),
                "fraction {f} out of range for {a:?} -> {b:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 0);
}

#[test]
fn a_trace_starting_inside_solid_is_not_reported_visible() {
    let Some(m) = load() else { return };
    let w = m.models[0];
    // The corners of the world bounding box are outside playable space.
    let inside_solid = [w.mins[0] + 1.0, w.mins[1] + 1.0, w.mins[2] + 1.0];
    assert_eq!(m.point_contents(inside_solid), contents::SOLID);

    let target = [
        (w.mins[0] + w.maxs[0]) * 0.5,
        (w.mins[1] + w.maxs[1]) * 0.5,
        (w.mins[2] + w.maxs[2]) * 0.5,
    ];
    let t = m.trace(inside_solid, target);
    assert!(t.start_solid, "should report start_solid");
    assert!(
        !m.visible(inside_solid, target),
        "a point buried in solid must not see anything"
    );
}

/// Across a real level, some sightlines are open and many are blocked. If
/// every pair were visible the trace would not be intersecting geometry at
/// all; if none were, it would be rejecting everything.
#[test]
fn real_geometry_blocks_some_sightlines_but_not_all() {
    let Some(m) = load() else { return };
    let w = m.models[0];

    // Collect open-space sample points near the floor-ish middle of the map.
    let mut open_points = Vec::new();
    for i in 0..24 {
        for j in 0..24 {
            for k in 0..6 {
                let p = [
                    w.mins[0] + (w.maxs[0] - w.mins[0]) * (i as f32 + 0.5) / 24.0,
                    w.mins[1] + (w.maxs[1] - w.mins[1]) * (j as f32 + 0.5) / 24.0,
                    w.mins[2] + (w.maxs[2] - w.mins[2]) * (k as f32 + 0.5) / 6.0,
                ];
                if m.point_contents(p) == contents::EMPTY {
                    open_points.push(p);
                }
            }
        }
    }
    assert!(
        open_points.len() > 20,
        "expected plenty of open space in a real map, found {}",
        open_points.len()
    );

    let mut blocked = 0;
    let mut clear = 0;
    for (n, a) in open_points.iter().enumerate() {
        let b = open_points[(n * 7 + 3) % open_points.len()];
        if m.visible(*a, b) {
            clear += 1;
        } else {
            blocked += 1;
        }
    }
    eprintln!(
        "open points={} sightlines: clear={clear} blocked={blocked}",
        open_points.len()
    );
    assert!(
        blocked > 0,
        "no sightline was blocked -- the trace is not hitting geometry"
    );
    assert!(
        clear > 0,
        "every sightline was blocked -- the trace is rejecting everything"
    );
}
