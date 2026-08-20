//! Per-bot tactical roles for bomb maps (plan Phase A1 / A3).
//!
//! The remaining humanization misses on de_dust2 (CONGA-1, ROUTE-3, PILE-2 max)
//! share one cause: 15 teammates share one of two sites and funnel into the same
//! final corridor. Different seeds already pick different points *inside* a site
//! volume, but that only spreads the pile after arrival — the walk there is still
//! a stream.
//!
//! Roles fix the *inputs*:
//! - which site a bot wants
//! - whether its destination is deep in the plant volume or on an approach ring
//!   200–450 u out (so non-carriers do not all occupy the plant disc)
//! - a mid / split destination between the two sites for rotators
//!
//! Assignment is deterministic in the bot seed: no IPC, works across the
//! multi-process swarm. The bomb *carrier* overrides at runtime to the plant
//! spot so a Hold-role bot that picks up the C4 still plants inside a zone.

use nav::entities::Aabb;
use nav::navgrid::NavGrid;
use nav::route::NavSource;

use crate::map::{seed_rng, Map};

/// Tactical job this bot is playing this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BotRole {
    /// Push deep into the assigned site (plant volume interior).
    Assault,
    /// Same site, approach ring at a complementary angle (different corridor exit).
    Flank,
    /// Approach ring only — do not pile into the plant disc.
    Hold,
    /// Between sites / the other site — rotate / lurk / mid pressure.
    Split,
}

impl BotRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assault => "assault",
            Self::Flank => "flank",
            Self::Hold => "hold",
            Self::Split => "split",
        }
    }

    /// Draw a role from the bot seed.
    ///
    /// Distribution (of 10):
    /// - Assault 4  — enough people to take a site
    /// - Hold 3     — spread the final approach
    /// - Flank 2    — different ring angle / corridor
    /// - Split 1    — force the second site / mid presence
    pub fn from_seed(seed: usize) -> Self {
        match seed % 10 {
            0..=3 => Self::Assault,
            4..=6 => Self::Hold,
            7..=8 => Self::Flank,
            _ => Self::Split,
        }
    }
}

/// Ring distances around a site for Hold/Flank destinations (plan A3).
/// Wider ring (was 450) so approach positions fan further and CONGA-1 drops.
pub const RING_MIN: f32 = 200.0;
pub const RING_MAX: f32 = 560.0;

/// Result of picking where this bot should go this round.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectivePick {
    pub role: BotRole,
    /// Where the feet should path to (may be a ring point, not the plant disc).
    pub destination: [f32; 3],
    /// A point guaranteed inside (or at the centre of) a bomb-site volume — the
    /// place a carrier must plant. Equal to `destination` for Assault.
    pub plant_spot: [f32; 3],
    /// Index into `map.info.bomb_sites`, if this is a bomb map.
    pub site_index: Option<usize>,
}

/// Same-team tactical belief merged from the G0 state bus, fed into the G2
/// rotate picker alongside the local PVS signal.
///
/// `None` fields mean "the bus has nothing to say" — the picker falls back to
/// local observation alone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeamTactics {
    /// Site (by index) that most of the team believes is under pressure.
    pub pressure_site: Option<usize>,
    /// Site (by index) where a teammate reports the bomb planted.
    pub plant_site: Option<usize>,
}

impl TeamTactics {
    pub const EMPTY: Self = Self {
        pressure_site: None,
        plant_site: None,
    };
}

/// Pick a role-aware objective for this bot on this map.
pub fn pick_objective(map: &Map, is_ct: bool, seed: usize) -> Option<ObjectivePick> {
    let role = BotRole::from_seed(seed);
    let mut rng = seed_rng(seed);

    // Hostage maps keep the existing CT-goes-to-hostage behaviour; roles still
    // apply a ring around that point so rescuers do not stack on one spawn.
    if is_ct && !map.info.rescue_zones.is_empty() && !map.info.hostage_spawns.is_empty() {
        let h = &map.info.hostage_spawns;
        let plant = h[rng() % h.len()];
        let destination = match role {
            BotRole::Assault => plant,
            BotRole::Hold | BotRole::Flank => ring_point(&map.grid, plant, seed, role),
            BotRole::Split => {
                // Another hostage, or the rescue zone centre.
                if h.len() > 1 {
                    h[rng() % h.len()]
                } else if let Some(z) = map.info.rescue_zones.first() {
                    z.centre()
                } else {
                    plant
                }
            }
        };
        return Some(ObjectivePick {
            role,
            destination,
            plant_spot: plant,
            site_index: None,
        });
    }

    let sites = &map.info.bomb_sites;
    if sites.is_empty() {
        // Deathmatch / no objective entities.
        return map.objective(is_ct, seed).map(|destination| ObjectivePick {
            role,
            destination,
            plant_spot: destination,
            site_index: None,
        });
    }

    // Phase G1: CT default setup on de_dust2 — named A/B/mid anchors so both
    // sites and info lanes are held (not a random site pile).
    if is_ct {
        if let Some(pick) = dust2_ct_setup(map, seed) {
            return Some(pick);
        }
    }

    let n = sites.len();
    // Base site from seed; Split flips to the other (or next) site.
    let base = rng() % n;
    let site_index = match role {
        BotRole::Split if n > 1 => (base + 1 + (rng() % (n - 1))) % n,
        _ => base,
    };
    let zone = &sites[site_index];
    // Prefer named tactical plant spots on known maps (Phase D); fall back to
    // a random standable point inside the bomb-target volume.
    let plant_spot =
        dust2_plant_spot(map, site_index, seed).unwrap_or_else(|| spot_in_zone(zone, &mut rng));

    let destination = match role {
        BotRole::Assault => plant_spot,
        BotRole::Hold => ring_point(&map.grid, zone.centre(), seed, role),
        BotRole::Flank => ring_point(&map.grid, zone.centre(), seed.wrapping_add(0x9E37), role),
        BotRole::Split => {
            // Prefer a point mid-way between the two sites (mid control), else ring.
            if n > 1 {
                let a = sites[0].centre();
                let b = sites[1].centre();
                let mid = [
                    (a[0] + b[0]) * 0.5,
                    (a[1] + b[1]) * 0.5,
                    (a[2] + b[2]) * 0.5,
                ];
                // Nudge mid toward this bot's plant site so "split" is not one
                // shared mid pixel for every Split bot.
                let t = 0.25 + (seed % 50) as f32 * 0.005;
                let nudged = [
                    mid[0] + (plant_spot[0] - mid[0]) * t,
                    mid[1] + (plant_spot[1] - mid[1]) * t,
                    mid[2] + (plant_spot[2] - mid[2]) * t,
                ];
                snap_walkable(&map.grid, nudged).unwrap_or(plant_spot)
            } else {
                ring_point(&map.grid, zone.centre(), seed, role)
            }
        }
    };

    Some(ObjectivePick {
        role,
        destination,
        plant_spot,
        site_index: Some(site_index),
    })
}

/// de_dust2 A/B site indices by height (A platform z≈144, B z≈48).
fn dust2_site_indices(map: &Map) -> Option<(usize, usize)> {
    let sites = &map.info.bomb_sites;
    if sites.len() < 2 {
        return None;
    }
    let mut a_idx = 0usize;
    let mut b_idx = 1usize;
    for (i, z) in sites.iter().enumerate() {
        if z.centre()[2] > 100.0 {
            a_idx = i;
        } else {
            b_idx = i;
        }
    }
    if a_idx == b_idx {
        b_idx = 1 - a_idx;
    }
    Some((a_idx, b_idx))
}

/// The `bomb_sites` index matching a [`bot::PlantSite`] belief.
///
/// The G0 bus speaks in `PlantSite::{A,B}` (derived from site centre height in
/// `ingest_team_reports`), while the G2 picker works in site indices. This maps
/// one to the other so snapshot pressure/plant beliefs can drive rotation.
pub fn plant_site_index(map: &Map, site: bot::PlantSite) -> Option<usize> {
    let (a_idx, b_idx) = dust2_site_indices(map)?;
    match site {
        bot::PlantSite::A => Some(a_idx),
        bot::PlantSite::B => Some(b_idx),
        bot::PlantSite::Unknown => None,
    }
}

/// Phase G1 — CT default setup on de_dust2.
///
/// Ten seed buckets (≈3 A / 3 B / 2 mid / 2 flex) so a 10–15 CT side always
/// covers both sites and info lanes. Destinations are **named lane anchors**
/// (snapped to nav), not the plant disc centre.
///
/// Returns `None` on non-dust2 so the generic path still runs.
fn dust2_ct_setup(map: &Map, seed: usize) -> Option<ObjectivePick> {
    if map.name != "de_dust2" {
        return None;
    }
    let (a_idx, b_idx) = dust2_site_indices(map)?;

    // (role, site_index, world anchor). Coords are stock de_dust2-ish holds;
    // snap_walkable corrects to the nav lattice.
    // Buckets 0..2 A, 3..5 B, 6..7 mid info, 8..9 flex connectors.
    let table: [(BotRole, usize, [f32; 3]); 10] = [
        // --- A site ---
        (BotRole::Hold, a_idx, [1280.0, 2392.0, 96.0]), // A long/open
        (BotRole::Assault, a_idx, [1096.0, 2520.0, 96.0]), // A goose / site
        (BotRole::Flank, a_idx, [840.0, 2080.0, 16.0]), // cat → A
        // --- B site ---
        (BotRole::Hold, b_idx, [-1424.0, 2624.0, 48.0]), // B open
        (BotRole::Assault, b_idx, [-1600.0, 2752.0, 48.0]), // B back
        (BotRole::Flank, b_idx, [-1200.0, 2400.0, 48.0]), // B car / mid-B
        // --- Mid / info lanes ---
        (BotRole::Split, a_idx, [100.0, 2100.0, 96.0]), // CT mid doors
        (BotRole::Split, b_idx, [-280.0, 2000.0, 96.0]), // mid → B
        // --- Flex (connector / rotate-ready) ---
        (BotRole::Split, a_idx, [400.0, 2300.0, 96.0]), // CT to A short
        (BotRole::Split, b_idx, [-500.0, 2300.0, 48.0]), // CT to B
    ];

    let (role, site_index, raw) = table[seed % 10];
    let destination = snap_walkable(&map.grid, raw).unwrap_or(raw);
    let mut rng = seed_rng(seed);
    let plant_spot = dust2_plant_spot(map, site_index, seed)
        .unwrap_or_else(|| spot_in_zone(&map.info.bomb_sites[site_index], &mut rng));

    Some(ObjectivePick {
        role,
        destination,
        plant_spot,
        site_index: Some(site_index),
    })
}

/// Random standable-ish point well inside a bomb-target AABB (existing W2 logic).
fn spot_in_zone(zone: &Aabb, rng: &mut impl FnMut() -> usize) -> [f32; 3] {
    let c = zone.centre();
    let frac = |lo: f32, hi: f32, r: usize| -> f32 {
        let half = (hi - lo) * 0.5 * 0.6;
        let t = (r % 1000) as f32 / 1000.0 * 2.0 - 1.0;
        (lo + hi) * 0.5 + t * half
    };
    [
        frac(zone.mins[0], zone.maxs[0], rng()),
        frac(zone.mins[1], zone.maxs[1], rng()),
        c[2],
    ]
}

/// Point on a horizontal ring around `centre`, snapped to the nearest nav node.
///
/// Angle is a golden-angle hash of the seed so 15 bots on the same site fan out
/// instead of stacking on one bearing. Radius is uniform in [RING_MIN, RING_MAX].
fn ring_point(grid: &NavGrid, centre: [f32; 3], seed: usize, role: BotRole) -> [f32; 3] {
    // Golden angle in radians (~2.399): successive seeds tile the circle.
    let base = (seed as f64 * 2.399_963_229_728_653).rem_euclid(std::f64::consts::TAU);
    // Flank gets a 120° offset so it does not share the Hold fan.
    let offset = match role {
        BotRole::Flank => std::f64::consts::TAU / 3.0,
        _ => 0.0,
    };
    let angle = base + offset;
    let t = ((seed.wrapping_mul(0x9E37) >> 3) % 1000) as f32 / 1000.0;
    let r = RING_MIN + t * (RING_MAX - RING_MIN);
    let (s, c) = angle.sin_cos();
    let raw = [
        centre[0] + r * c as f32,
        centre[1] + r * s as f32,
        centre[2],
    ];
    snap_walkable(grid, raw).unwrap_or(raw)
}

fn snap_walkable(grid: &NavGrid, p: [f32; 3]) -> Option<[f32; 3]> {
    let n = grid.nearest(p)?;
    Some(grid.origin(n))
}

/// Active navigation goal: carrier always plants; everyone else uses the role destination.
pub fn active_goal(
    carrying_bomb: bool,
    destination: Option<[f32; 3]>,
    plant_spot: Option<[f32; 3]>,
) -> Option<[f32; 3]> {
    if carrying_bomb {
        plant_spot.or(destination)
    } else {
        destination
    }
}

/// How close an enemy must be to a site centre to count as "contact" for G2.
pub const CT_ROTATE_CONTACT_R: f32 = 900.0;

/// Phase G2 — should this CT leave its default hold and path to a threatened site?
///
/// Merges **local PVS** (≥2 visible enemies near one site, strictly more
/// pressure there than on the quiet site) with **G0 team-bus belief** from
/// [`TeamTactics`]: a snapshot pressure/plant site counts as two virtual
/// contacts so a teammate's report rotates a bot that cannot see the threat
/// yet. A planted bomb reported on the bus is absolute, like a local one.
///
/// Keeps a **delay** player on the quiet site when this seed is a pure site
/// Hold (buckets 0/3) unless pressure is overwhelming (≥3 contacts).
///
/// Returns a new destination + plant_spot + role label site index when rotating;
/// `None` means keep the G1 default.
pub fn ct_rotate_pick(
    map: &Map,
    seed: usize,
    enemy_origins: &[[f32; 3]],
    bomb_planted: bool,
    bomb_origin: Option<[f32; 3]>,
    team: TeamTactics,
) -> Option<ObjectivePick> {
    if map.info.bomb_sites.len() < 2 {
        return None;
    }
    let sites = &map.info.bomb_sites;
    let n = sites.len();

    // Planted bomb is absolute: retake that site (G5 path still owns defuse;
    // this only reassigns feet for CTs whose G1 hold was the other site).
    let threat = if bomb_planted || team.plant_site.is_some() {
        if let Some(bo) = bomb_origin.or_else(|| {
            team.plant_site.and_then(|i| {
                let c = sites.get(i)?.centre();
                Some([c[0], c[1], c[2]])
            })
        }) {
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (i, z) in sites.iter().enumerate() {
                let c = z.centre();
                let dx = bo[0] - c[0];
                let dy = bo[1] - c[1];
                let d = dx * dx + dy * dy;
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            Some(best)
        } else {
            // Planted belief without an origin: trust the bus site directly.
            team.plant_site.filter(|i| *i < n)
        }
    } else {
        let mut counts = vec![0u32; n];
        // G0 team pressure counts as two virtual contacts on that site.
        if let Some(p) = team.pressure_site {
            if p < n {
                counts[p] += 2;
            }
        }
        for e in enemy_origins {
            let mut best_i = 0usize;
            let mut best_d = f32::MAX;
            for (i, z) in sites.iter().enumerate() {
                let c = z.centre();
                let dx = e[0] - c[0];
                let dy = e[1] - c[1];
                let d = (dx * dx + dy * dy).sqrt();
                if d < best_d {
                    best_d = d;
                    best_i = i;
                }
            }
            if best_d <= CT_ROTATE_CONTACT_R {
                counts[best_i] += 1;
            }
        }
        // Need ≥2 signals on one site and more than the other (anti-fake mid shot).
        let mut threat = None;
        for i in 0..n {
            if counts[i] >= 2 {
                let other = counts
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, c)| *c)
                    .max()
                    .unwrap_or(0);
                if counts[i] > other {
                    threat = Some(i);
                    break;
                }
            }
        }
        threat
    }?;

    // Default G1 assignment for this seed — if already on the threatened site, stay.
    let base = pick_objective(map, true, seed)?;
    if base.site_index == Some(threat) {
        return None;
    }

    // Delay: seed buckets 0 and 3 are pure A/B Hold in dust2_ct_setup — leave
    // them on the quiet site unless ≥3 contacts (or bomb planted, locally or
    // reported by the team bus).
    let delay_bucket = matches!(seed % 10, 0 | 3);
    if delay_bucket && !bomb_planted && team.plant_site.is_none() {
        // Only rotate delay players on heavy contact.
        let mut heavy = 0u32;
        let c = sites[threat].centre();
        if let Some(p) = team.pressure_site {
            if p == threat {
                heavy += 2;
            }
        }
        for e in enemy_origins {
            let dx = e[0] - c[0];
            let dy = e[1] - c[1];
            if (dx * dx + dy * dy).sqrt() <= CT_ROTATE_CONTACT_R {
                heavy += 1;
            }
        }
        if heavy < 3 {
            return None;
        }
    }

    // Retake / rotate anchors per site (dust2-named holds into the site).
    let raw = if map.name == "de_dust2" {
        let (a_idx, b_idx) = dust2_site_indices(map)?;
        if threat == a_idx {
            // Path in via long / cat / CT short — seed picks one.
            match seed % 3 {
                0 => [1280.0, 2392.0, 96.0], // long/open A
                1 => [840.0, 2080.0, 16.0],  // cat
                _ => [400.0, 2300.0, 96.0],  // CT short
            }
        } else if threat == b_idx {
            match seed % 3 {
                0 => [-1424.0, 2624.0, 48.0], // B open
                1 => [-1200.0, 2400.0, 48.0], // B car
                _ => [-500.0, 2300.0, 48.0],  // CT to B
            }
        } else {
            sites[threat].centre()
        }
    } else {
        sites[threat].centre()
    };

    let destination = snap_walkable(&map.grid, raw).unwrap_or(raw);
    let mut rng = seed_rng(seed);
    let plant_spot = dust2_plant_spot(map, threat, seed)
        .unwrap_or_else(|| spot_in_zone(&sites[threat], &mut rng));

    Some(ObjectivePick {
        // Log as Split so metrics show rotate≠static hold; feet go to threat.
        role: BotRole::Split,
        destination,
        plant_spot,
        site_index: Some(threat),
    })
}

/// Hand-picked de_dust2 plant positions (Phase D).
///
/// Indices match `bomb_sites` order from the BSP (B first at z≈48, A at z≈144
/// on the stock map — we match by site centre z rather than hard index).
/// Each entry is (label, origin). Origins sit well inside the plant volume.
fn dust2_plant_spot(map: &Map, site_index: usize, seed: usize) -> Option<[f32; 3]> {
    if map.name != "de_dust2" {
        return None;
    }
    let centre = map.info.bomb_sites.get(site_index)?.centre();
    // A site is the high platform (~144); B is the low one (~48).
    let is_a = centre[2] > 100.0;
    // Named spots: default (deep), open (for retake), default-for-postplant.
    let spots: &[[f32; 3]] = if is_a {
        &[
            [1152.0, 2464.0, 144.0], // A default
            [1280.0, 2392.0, 144.0], // A open / long side
            [1096.0, 2520.0, 144.0], // A toward goose
            [1224.0, 2480.0, 144.0], // A mid-site
        ]
    } else {
        &[
            [-1536.0, 2688.0, 48.0], // B default
            [-1424.0, 2624.0, 48.0], // B open
            [-1600.0, 2752.0, 48.0], // B back
            [-1488.0, 2720.0, 48.0], // B toward tunnels
        ]
    };
    // Personality via seed: assault-heavy seeds take default; careful take back.
    let i = match seed % 10 {
        0..=3 => 0, // default
        4..=5 => 1, // open
        6..=7 => 2,
        _ => 3,
    };
    let raw = spots[i % spots.len()];
    // Snap to walkable so we never aim at a brush interior.
    snap_walkable(&map.grid, raw).or(Some(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn role_distribution_covers_all_four() {
        let mut seen = HashSet::new();
        for s in 0..40 {
            seen.insert(BotRole::from_seed(s));
        }
        assert_eq!(
            seen.len(),
            4,
            "all four roles must appear in a 40-seed window"
        );
    }

    #[test]
    fn dust2_roles_spread_destinations_and_sites() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let mut cells = HashSet::new();
        let mut sites = HashSet::new();
        let mut roles = HashSet::new();
        for seed in 0..30 {
            let pick = pick_objective(&map, false, seed).expect("T objective");
            cells.insert((
                (pick.destination[0] / 128.0).floor() as i32,
                (pick.destination[1] / 128.0).floor() as i32,
            ));
            if let Some(i) = pick.site_index {
                sites.insert(i);
            }
            roles.insert(pick.role);
            // Plant spot must sit inside some bomb site volume (xy).
            let in_zone = map
                .info
                .bomb_sites
                .iter()
                .any(|z| z.contains_xy(pick.plant_spot));
            assert!(
                in_zone,
                "plant_spot {:?} for seed {seed} not in any bomb zone",
                pick.plant_spot
            );
            // Hold/Flank destinations should be farther from site centre than Assault's plant.
            if matches!(pick.role, BotRole::Hold | BotRole::Flank) {
                if let Some(i) = pick.site_index {
                    let c = map.info.bomb_sites[i].centre();
                    let dx = pick.destination[0] - c[0];
                    let dy = pick.destination[1] - c[1];
                    let d = (dx * dx + dy * dy).sqrt();
                    assert!(
                        d >= RING_MIN * 0.5,
                        "role {:?} seed {seed}: ring dest only {d:.0}u from centre (want ~ring)",
                        pick.role
                    );
                }
            }
        }
        assert!(
            cells.len() >= 10,
            "30 T seeds should cover many 128u cells, got {}",
            cells.len()
        );
        assert_eq!(
            sites.len(),
            2,
            "both dust2 sites must be chosen across 30 seeds"
        );
        assert_eq!(roles.len(), 4, "all roles should appear");
    }

    #[test]
    fn carrier_overrides_to_plant_spot() {
        let dest = Some([0.0, 0.0, 0.0]);
        let plant = Some([100.0, 100.0, 0.0]);
        assert_eq!(active_goal(true, dest, plant), plant);
        assert_eq!(active_goal(false, dest, plant), dest);
    }

    #[test]
    fn ring_points_fan_out_angularly() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let centre = map.info.bomb_sites[0].centre();
        let mut angles = Vec::new();
        for seed in 0..12 {
            let p = ring_point(&map.grid, centre, seed, BotRole::Hold);
            let a = (p[1] - centre[1]).atan2(p[0] - centre[0]);
            angles.push((a * 10.0).round() as i32); // ~0.1 rad buckets
        }
        let unique: HashSet<_> = angles.iter().copied().collect();
        assert!(
            unique.len() >= 8,
            "12 hold seeds should fan angles, got {} unique buckets: {:?}",
            unique.len(),
            angles
        );
    }

    #[test]
    fn g2_rotate_fires_on_two_enemies_at_b_not_on_a_hold() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let (a_idx, b_idx) = dust2_site_indices(&map).expect("two sites");
        let b_c = map.info.bomb_sites[b_idx].centre();
        // Two Ts on B.
        let enemies = [
            [b_c[0] + 50.0, b_c[1] + 50.0, b_c[2]],
            [b_c[0] - 40.0, b_c[1] + 20.0, b_c[2]],
        ];
        // Seed 1 = A goose assault in G1 table — should rotate toward B.
        let rot = ct_rotate_pick(&map, 1, &enemies, false, None, TeamTactics::EMPTY)
            .expect("rotate");
        assert_eq!(rot.site_index, Some(b_idx));
        assert_eq!(rot.role, BotRole::Split);
        // Seed already on B (bucket 3 = B hold) → no rotate.
        assert!(ct_rotate_pick(&map, 3, &enemies, false, None, TeamTactics::EMPTY).is_none());
        // Single enemy = not enough (anti-fake).
        assert!(ct_rotate_pick(&map, 1, &enemies[..1], false, None, TeamTactics::EMPTY).is_none());
        // Delay seed 0 with only 2 contacts stays home.
        assert!(ct_rotate_pick(&map, 0, &enemies, false, None, TeamTactics::EMPTY).is_none());
        // Bomb planted at B forces rotate for A seed.
        let plant = ct_rotate_pick(&map, 1, &[], true, Some(b_c), TeamTactics::EMPTY)
            .expect("plant rotate");
        assert_eq!(plant.site_index, Some(b_idx));
        let _ = a_idx;
    }

    #[test]
    fn g2_team_pressure_rotates_a_bot_that_sees_nothing() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let (a_idx, b_idx) = dust2_site_indices(&map).expect("two sites");
        // No local PVS enemies at all — the G0 bus alone must rotate seed 1
        // (an A hold) toward B because the team reports pressure at B.
        let team = TeamTactics {
            pressure_site: Some(b_idx),
            plant_site: None,
        };
        let rot = ct_rotate_pick(&map, 1, &[], false, None, team).expect("bus rotate");
        assert_eq!(rot.site_index, Some(b_idx));
        // Team pressure counts as virtual contacts, so seed 0 (delay bucket)
        // still holds with only two virtual contacts.
        assert!(ct_rotate_pick(&map, 0, &[], false, None, team).is_none());
        // A single local enemy plus team pressure on B is still enough to move.
        let b_c = map.info.bomb_sites[b_idx].centre();
        let one = [[b_c[0] + 60.0, b_c[1] + 10.0, b_c[2]]];
        let rot2 = ct_rotate_pick(&map, 1, &one, false, None, team).expect("bus+local rotate");
        assert_eq!(rot2.site_index, Some(b_idx));
        let _ = a_idx;
    }

    #[test]
    fn g2_team_plant_belief_is_absolute_without_local_origin() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let (a_idx, b_idx) = dust2_site_indices(&map).expect("two sites");
        // Bus says the bomb is planted at A; the bot has no local PVS and no
        // bomb origin of its own — must rotate seed 3 (a B hold) toward A.
        let team = TeamTactics {
            pressure_site: None,
            plant_site: Some(a_idx),
        };
        let rot = ct_rotate_pick(&map, 3, &[], false, None, team).expect("bus plant rotate");
        assert_eq!(rot.site_index, Some(a_idx));
        let _ = b_idx;
    }

    #[test]
    fn plant_site_index_maps_bus_belief_to_site_index() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let (a_idx, b_idx) = dust2_site_indices(&map).expect("two sites");
        assert_eq!(plant_site_index(&map, bot::PlantSite::A), Some(a_idx));
        assert_eq!(plant_site_index(&map, bot::PlantSite::B), Some(b_idx));
        assert_eq!(plant_site_index(&map, bot::PlantSite::Unknown), None);
    }

    #[test]
    fn dust2_ct_setup_covers_both_sites_and_mid() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: de_dust2.bsp not present");
            return;
        };
        let (a_idx, b_idx) = dust2_site_indices(&map).expect("two sites");
        let mut a_n = 0;
        let mut b_n = 0;
        let mut cells = HashSet::new();
        let a_c = map.info.bomb_sites[a_idx].centre();
        let b_c = map.info.bomb_sites[b_idx].centre();
        let mut near_a = 0;
        let mut near_b = 0;
        let mut midish = 0;
        for seed in 0..20 {
            let pick = pick_objective(&map, true, seed).expect("CT objective");
            match pick.site_index {
                Some(i) if i == a_idx => a_n += 1,
                Some(i) if i == b_idx => b_n += 1,
                _ => {}
            }
            cells.insert((
                (pick.destination[0] / 256.0).floor() as i32,
                (pick.destination[1] / 256.0).floor() as i32,
            ));
            let da = {
                let dx = pick.destination[0] - a_c[0];
                let dy = pick.destination[1] - a_c[1];
                (dx * dx + dy * dy).sqrt()
            };
            let db = {
                let dx = pick.destination[0] - b_c[0];
                let dy = pick.destination[1] - b_c[1];
                (dx * dx + dy * dy).sqrt()
            };
            if da < 900.0 {
                near_a += 1;
            } else if db < 900.0 {
                near_b += 1;
            } else {
                midish += 1;
            }
        }
        assert!(
            a_n >= 6 && b_n >= 6,
            "CT seeds must split A/B, got A={a_n} B={b_n}"
        );
        assert!(
            near_a >= 4 && near_b >= 4,
            "destinations must sit near both sites: near_a={near_a} near_b={near_b} mid={midish}"
        );
        assert!(
            midish >= 2 || cells.len() >= 6,
            "expect mid/flex spread or many cells; mid={midish} cells={}",
            cells.len()
        );
        assert!(
            cells.len() >= 5,
            "20 CT seeds should cover several map cells, got {}",
            cells.len()
        );
    }
}
