//! Persisted shell settings — a flat `key=value` file, no serde.
//!
//! The rest of the workspace hand-rolls its formats (see
//! `client::telemetry`), and this file has nine fields; a serialization
//! dependency would cost more build time than it saves code. Unknown keys are
//! kept on load and written back, so a newer build's config survives a
//! downgrade.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Workspace root — where `Cargo.toml`, `target/` and `testserver/` live.
    pub root: String,
    /// `host:port` of the game server the bots connect to.
    pub addr: String,
    /// Radar telemetry port (APT1). Must match what the bots broadcast to.
    pub telemetry_port: u32,
    /// G0 team-bus port (APT2), or 0 to not listen.
    pub team_port: u32,
    /// How many bots a deploy launches.
    pub bots: u32,
    /// Seconds each bot lives before it disconnects itself.
    pub secs: u32,
    /// Delay between launches — a mass connect trips ReAuthCheck.
    pub stagger_ms: u32,
    /// Bot names are this plus a two-digit index (`ruBot07`).
    pub name_prefix: String,
    /// CD keys are this plus a four-digit index (`RUBBOT0007`).
    pub key_prefix: String,
    /// Written into `RUB_MAP`; the radar background follows the bots' map.
    pub map: String,
    /// `easy` | `normal` | `hard` | `unfair`, or empty to let the seed decide.
    pub difficulty: String,
    /// Build the bot runner in release mode.
    pub release: bool,
    /// Unrecognized keys, preserved verbatim.
    extra: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root: default_root().to_string_lossy().into_owned(),
            addr: "127.0.0.1:27015".into(),
            telemetry_port: client::telemetry::DEFAULT_PORT as u32,
            team_port: client::telemetry::TEAM_DEFAULT_PORT as u32,
            bots: 10,
            secs: 300,
            stagger_ms: 1500,
            name_prefix: "ruBot".into(),
            key_prefix: "RUBBOT".into(),
            map: "de_dust2".into(),
            difficulty: String::new(),
            release: false,
            extra: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.root)
    }

    /// Where the bot runner should be, given the current build profile.
    pub fn runner_path(&self) -> PathBuf {
        let profile = if self.release { "release" } else { "debug" };
        self.root_path()
            .join("target")
            .join(profile)
            .join("examples")
            .join(if cfg!(windows) {
                "capture_running.exe"
            } else {
                "capture_running"
            })
    }

    pub fn testserver_path(&self) -> PathBuf {
        self.root_path().join("testserver")
    }

    pub fn maps_dir(&self) -> PathBuf {
        self.testserver_path().join("cstrike").join("maps")
    }

    /// Name and CD key for the nth bot of a deploy (1-based).
    pub fn bot_identity(&self, n: u32) -> (String, String) {
        (
            format!("{}{:02}", self.name_prefix, n),
            format!("{}{:04}", self.key_prefix, n),
        )
    }

    /// Config file location: next to the executable, so a copied build keeps
    /// its own settings and nothing is written into the source tree.
    pub fn path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("rubots-gui.conf")))
            .unwrap_or_else(|| PathBuf::from("rubots-gui.conf"))
    }

    pub fn load() -> Self {
        let mut cfg = Self::default();
        let Ok(text) = fs::read_to_string(Self::path()) else {
            return cfg;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "root" => cfg.root = value.into(),
                "addr" => cfg.addr = value.into(),
                "telemetry_port" => cfg.telemetry_port = parse_or(value, cfg.telemetry_port),
                "team_port" => cfg.team_port = parse_or(value, cfg.team_port),
                "bots" => cfg.bots = parse_or(value, cfg.bots),
                "secs" => cfg.secs = parse_or(value, cfg.secs),
                "stagger_ms" => cfg.stagger_ms = parse_or(value, cfg.stagger_ms),
                "name_prefix" => cfg.name_prefix = value.into(),
                "key_prefix" => cfg.key_prefix = value.into(),
                "map" => cfg.map = value.into(),
                "difficulty" => cfg.difficulty = value.into(),
                "release" => cfg.release = value == "1" || value == "true",
                _ => {
                    cfg.extra.insert(key.into(), value.into());
                }
            }
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let mut out = String::from("# ruBots control centre settings\n");
        out.push_str(&format!("root={}\n", self.root));
        out.push_str(&format!("addr={}\n", self.addr));
        out.push_str(&format!("telemetry_port={}\n", self.telemetry_port));
        out.push_str(&format!("team_port={}\n", self.team_port));
        out.push_str(&format!("bots={}\n", self.bots));
        out.push_str(&format!("secs={}\n", self.secs));
        out.push_str(&format!("stagger_ms={}\n", self.stagger_ms));
        out.push_str(&format!("name_prefix={}\n", self.name_prefix));
        out.push_str(&format!("key_prefix={}\n", self.key_prefix));
        out.push_str(&format!("map={}\n", self.map));
        out.push_str(&format!("difficulty={}\n", self.difficulty));
        out.push_str(&format!("release={}\n", u8::from(self.release)));
        for (k, v) in &self.extra {
            out.push_str(&format!("{k}={v}\n"));
        }
        fs::write(Self::path(), out)
    }
}

fn parse_or(value: &str, fallback: u32) -> u32 {
    value.parse().unwrap_or(fallback)
}

/// Find the workspace root: `RUB_ROOT`, else the first ancestor of the exe (or
/// the working directory) that holds a `Cargo.toml` with a `[workspace]`.
///
/// This matters because the GUI is usually started from `target/debug/gui.exe`
/// by a shortcut, with a working directory that is anybody's guess, and every
/// action it can take — build, deploy, docker — is relative to the root.
pub fn default_root() -> PathBuf {
    if let Ok(root) = std::env::var("RUB_ROOT") {
        return PathBuf::from(root);
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        candidates.push(exe);
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("placeholder"));
    }
    for start in candidates {
        let mut dir: Option<&Path> = start.parent();
        while let Some(d) = dir {
            let manifest = d.join("Cargo.toml");
            if let Ok(text) = fs::read_to_string(&manifest) {
                if text.contains("[workspace]") {
                    return d.to_path_buf();
                }
            }
            dir = d.parent();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_zero_padded_the_way_the_swarm_script_makes_them() {
        let cfg = Config::default();
        assert_eq!(cfg.bot_identity(7), ("ruBot07".into(), "RUBBOT0007".into()));
        assert_eq!(cfg.bot_identity(30), ("ruBot30".into(), "RUBBOT0030".into()));
    }

    #[test]
    fn the_runner_path_follows_the_profile() {
        let mut cfg = Config::default();
        cfg.root = "R".into();
        assert!(cfg
            .runner_path()
            .to_string_lossy()
            .contains("target"));
        assert!(cfg.runner_path().to_string_lossy().contains("debug"));
        cfg.release = true;
        assert!(cfg.runner_path().to_string_lossy().contains("release"));
    }
}
