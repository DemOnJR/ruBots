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
        let pick = |v: &Vec<nav::entities::Aabb>| -> Option<[f32; 3]> {
            if v.is_empty() {
                None
            } else {
                Some(v[seed % v.len()].centre())
            }
        };
        if is_ct && !self.info.rescue_zones.is_empty() && !self.info.hostage_spawns.is_empty() {
            // On a hostage map the CT objective is the hostages, not the zone
            // -- you have to collect before you can deliver.
            let h = &self.info.hostage_spawns;
            return Some(h[seed % h.len()]);
        }
        pick(&self.info.bomb_sites).or_else(|| pick(&self.info.rescue_zones))
    }
}

/// Where the map files might be.
///
/// `AIPLAYERS_MAPS_DIR` first, so a caller can point at a real Counter-Strike
/// install; then the copies extracted from the test-server container.
fn find_bsp(name: &str) -> Option<PathBuf> {
    let file = format!("{name}.bsp");
    let roots = std::env::var("AIPLAYERS_MAPS_DIR")
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
