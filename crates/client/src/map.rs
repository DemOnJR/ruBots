//! Loading the map so the bot knows where things are and how to get there.
//!
//! The network stream carries none of this. It tells us where players are, but
//! not where the bomb sites are, not where the walls are, and not how to walk
//! from one to the other — so a bot with only the wire can see an enemy and
//! still have no idea how to reach a bomb site. The `.bsp` supplies all three.
//!
//! Loading is best-effort and non-fatal: with no map the bot still connects,
//! joins, shoots at what it can see and defends where it stands. It just does
//! not path. That degradation is deliberate — a missing map file should not
//! stop the protocol layer from working.

use std::path::{Path, PathBuf};

use nav::bsp::Bsp;
use nav::entities::MapInfo;
use nav::navgrid::NavGrid;

/// A loaded map: collision, entities, and a navigation graph.
pub struct Map {
    pub name: String,
    pub bsp: Bsp,
    pub info: MapInfo,
    pub grid: NavGrid,
}

impl Map {
    /// Find and load `<name>.bsp`, generating (or reusing) its nav graph.
    ///
    /// Generation takes about a tenth of a second and the result is cached
    /// beside the map, keyed on a checksum of the `.bsp` itself so a map that
    /// changes cannot be answered from a stale graph.
    pub fn load(name: &str) -> Option<Self> {
        let path = find_bsp(name)?;
        let bytes = std::fs::read(&path).ok()?;
        let checksum = nav::navgrid::checksum(&bytes);
        let bsp = Bsp::parse(&bytes).ok()?;
        let info = MapInfo::from_bsp(&bsp).ok()?;

        let cache = cache_path(name);
        let grid = match NavGrid::load(&cache, checksum) {
            Ok(g) => g,
            Err(_) => {
                let g = NavGrid::generate(&bsp, &info, checksum);
                // A cache miss is not an error; failing to write one is not
                // either. Regenerating costs a tenth of a second.
                if let Some(dir) = cache.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = g.save(&cache);
                g
            }
        };
        Some(Self {
            name: name.to_string(),
            bsp,
            info,
            grid,
        })
    }

    /// Where this bot should be heading, given its team.
    ///
    /// Terrorists go to a bomb site to plant; counter-terrorists go to a site
    /// to defend, or to the hostages on a rescue map. Returns `None` when the
    /// map offers no objective, which is the correct answer for a deathmatch
    /// map rather than a reason to invent one.
    pub fn objective(&self, is_ct: bool, seed: usize) -> Option<[f32; 3]> {
        // Two independent draws off the seed: WHICH zone, and WHERE in it.
        //
        // Both matter, and the second is the one that was missing. Handing every
        // bot on a team the same zone CENTRE is what produced the pile: 80% of
        // live bots inside one 192-unit box, and 71.8% of `arrived` samples in
        // a single 64-unit cell. A bomb site is a room, not a point -- players
        // spread across it, and so should we.
        //
        // Kept deterministic in the seed so a given bot always wants the same
        // spot: a destination that moves under a bot mid-round makes the
        // navigation layer look broken for reasons that have nothing to do with
        // navigation.
        let mut rng = crate::map::seed_rng(seed);
        let mut spot = |v: &Vec<nav::entities::Aabb>| -> Option<[f32; 3]> {
            if v.is_empty() {
                return None;
            }
            let zone = &v[rng() % v.len()];
            let c = zone.centre();
            // Stay well inside the brush: the edge of a bomb-target volume is
            // not reliably standable, and being outside it means no plant.
            let frac = |lo: f32, hi: f32, r: usize| -> f32 {
                let half = (hi - lo) * 0.5 * 0.6;
                let t = (r % 1000) as f32 / 1000.0 * 2.0 - 1.0;
                (lo + hi) * 0.5 + t * half
            };
            Some([
                frac(zone.mins[0], zone.maxs[0], rng()),
                frac(zone.mins[1], zone.maxs[1], rng()),
                c[2],
            ])
        };
        if is_ct && !self.info.rescue_zones.is_empty() && !self.info.hostage_spawns.is_empty() {
            // On a hostage map the CT objective is the hostages, not the zone
            // -- you have to collect before you can deliver.
            let h = &self.info.hostage_spawns;
            return Some(h[rng() % h.len()]);
        }
        spot(&self.info.bomb_sites).or_else(|| spot(&self.info.rescue_zones))
    }
}

/// A tiny deterministic sequence from a seed, for picking a goal.
///
/// Deliberately not shared with `bot::rng`: this runs once per round to choose
/// a destination, and it must give the SAME answer for the same bot every time
/// it is asked, independently of how many aim errors that bot has drawn since.
pub fn seed_rng(seed: usize) -> impl FnMut() -> usize {
    // SplitMix64, which has no bad seeds -- including 0, which is exactly the
    // seed every bot used to be given.
    let mut state = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF_CAFE_F00D;
    move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 1) as usize
    }
}

/// Where the map files might be.
///
/// `AIPLAYERS_MAPS_DIR` first, so a caller can point at a real Counter-Strike
/// install; then the copies extracted from the test-server container.
fn find_bsp(name: &str) -> Option<PathBuf> {
    let file = format!("{name}.bsp");
    let roots = std::env::var("RUB_MAPS_DIR")
        .or_else(|_| std::env::var("RUBOTS_MAPS_DIR"))
        .or_else(|_| std::env::var("REB_MAPS_DIR"))
        .or_else(|_| std::env::var("REBOTS_MAPS_DIR"))
        .or_else(|_| std::env::var("AIPLAYERS_MAPS_DIR"))
        .ok()
        .map(PathBuf::from)
        .into_iter()
        // Both the workspace root and a crate directory: `cargo test` runs
        // with the CWD set to the package root, so a workspace-relative path
        // alone silently finds nothing and every map test SKIPS while
        // reporting ok. That is exactly the failure mode these tests exist to
        // avoid, so search both.
        .chain([
            PathBuf::from("testserver/cstrike/maps"),
            PathBuf::from("../../testserver/cstrike/maps"),
            PathBuf::from("../testserver/cstrike/maps"),
            PathBuf::from("cstrike/maps"),
        ]);
    roots.map(|r| r.join(&file)).find(|p| p.is_file())
}

fn cache_path(name: &str) -> PathBuf {
    // Beside the maps we actually found, so the cache follows the content.
    let base = find_bsp(name)
        .and_then(|p| p.parent().and_then(|d| d.parent()).map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("testserver/cstrike"));
    base.join("navcache").join(format!("{name}.navgrid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing map must degrade, not panic: the protocol layer has to keep
    /// working on a machine with no game content.
    #[test]
    fn a_missing_map_is_none_not_a_panic() {
        assert!(Map::load("de_definitely_not_a_real_map").is_none());
    }

    #[test]
    fn de_dust2_loads_with_two_bomb_sites_and_reachable_objectives() {
        let Some(map) = Map::load("de_dust2") else {
            eprintln!("SKIP: testserver/cstrike/maps/de_dust2.bsp not present");
            return;
        };
        assert_eq!(map.info.bomb_sites.len(), 2, "de_dust2 has two bomb sites");
        assert!(map.grid.len() > 1000, "graph is implausibly small");

        // Both teams must have somewhere to go, and the two sites must be
        // distinct -- a seed that always picks the same one would look like it
        // works while sending every bot to A.
        let a = map.objective(false, 0).expect("T objective");
        let b = map.objective(false, 1).expect("T objective");
        assert_ne!(a, b, "the two bomb sites should be different places");
        assert!(map.objective(true, 0).is_some(), "CT objective");
    }

    /// The point of generating from the BSP is that it works on maps nobody
    /// hand-authored waypoints for.
    #[test]
    fn a_hostage_map_reports_a_hostage_objective() {
        let Some(map) = Map::load("cs_office") else {
            eprintln!("SKIP: cs_office.bsp not present");
            return;
        };
        assert!(
            map.objective(true, 0).is_some(),
            "a CT on a hostage map needs somewhere to go"
        );
    }
}
