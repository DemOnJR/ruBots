//! Local game files, for the consistency demands the wire cannot answer.
//!
//! Most of a server's consistency list can be satisfied without owning a single
//! game file, by echoing back the model bounds it already sent us (see
//! [`proto::consistency`]). The exception is `force_exactfile`, which wants the
//! real MD5 and has no escape hatch. On a stock Counter-Strike server that is a
//! short, map-independent list of sprites, so pointing this at any normal
//! `cstrike` directory is enough.
//!
//! Discovered from `AIPLAYERS_CSTRIKE_DIR`, else `./cstrike`, else
//! `testserver/cstrike` (populated by `harness setup`, which copies the files
//! out of the test-server container). Absent content is not an error here — it
//! becomes a specific, reportable failure at the point a demand needs it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A `cstrike` content directory with a memoised hash cache.
#[derive(Debug, Default)]
pub struct GameContent {
    root: PathBuf,
    cache: HashMap<String, Option<[u8; 16]>>,
}

impl GameContent {
    /// Locate a content directory, if there is one.
    pub fn discover() -> Option<Self> {
        let candidates = std::env::var("AIPLAYERS_CSTRIKE_DIR")
            .ok()
            .map(PathBuf::from)
            .into_iter()
            .chain([
                PathBuf::from("cstrike"),
                PathBuf::from("testserver/cstrike"),
                PathBuf::from("../testserver/cstrike"),
            ]);
        candidates.into_iter().find(|p| p.is_dir()).map(Self::at)
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            cache: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// MD5 of a resource path relative to `cstrike/`, or `None` if absent.
    ///
    /// Takes `&self` so it can be called from the response builder; the cache
    /// is therefore only consulted, not filled, here. Use [`Self::prime`] to
    /// fill it up front when that matters.
    pub fn md5(&self, rel: &str) -> Option<[u8; 16]> {
        if let Some(hit) = self.cache.get(rel) {
            return *hit;
        }
        Self::hash_file(&self.root, rel)
    }

    /// Read and cache the hashes for `paths`.
    pub fn prime<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) {
        for rel in paths {
            if !self.cache.contains_key(rel) {
                let h = Self::hash_file(&self.root, rel);
                self.cache.insert(rel.to_string(), h);
            }
        }
    }

    /// How many primed paths are missing from disk.
    pub fn missing(&self) -> Vec<&str> {
        self.cache
            .iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.as_str())
            .collect()
    }

    fn hash_file(root: &Path, rel: &str) -> Option<[u8; 16]> {
        // Resource names use forward slashes; they are relative to the mod
        // directory and must not escape it.
        if rel.contains("..") {
            return None;
        }
        let path = root.join(rel.replace('\\', "/"));
        let data = std::fs::read(path).ok()?;
        Some(proto::resources::file_hash_md5(&data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_none_not_a_panic() {
        let c = GameContent::at("definitely/not/a/real/directory");
        assert_eq!(c.md5("sprites/smokepuff.spr"), None);
    }

    #[test]
    fn traversal_is_refused() {
        let c = GameContent::at(".");
        assert_eq!(c.md5("../../../etc/passwd"), None);
    }

    #[test]
    fn priming_records_what_is_missing() {
        let mut c = GameContent::at("definitely/not/a/real/directory");
        c.prime(["sprites/a.spr", "sprites/b.spr"]);
        let mut missing = c.missing();
        missing.sort_unstable();
        assert_eq!(missing, vec!["sprites/a.spr", "sprites/b.spr"]);
    }
}
