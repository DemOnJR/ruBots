# reBots (REB)

High-performance Rust-based AI player client and visualizer suite for Counter-Strike 1.6 / GoldSrc engine.

---

## Workspace Structure

The project is structured as a Cargo workspace with several modular crates under `crates/`:

| Crate | Path | Description |
| :--- | :--- | :--- |
| **`proto`** | `crates/proto` | Protocol parsing, delta decompression, munge tables, usercmd codecs |
| **`netchan`** | `crates/netchan` | UDP network channel, fragmentation, sequencing, reliability |
| **`auth`** | `crates/auth` | Steam / RevEmu authentication ticket generation |
| **`bot`** | `crates/bot` | Bot state machine, aiming physics, decision making, tactical roles |
| **`nav`** | `crates/nav` | Navmesh parser, lattice pathfinding, raytracing, and map topology |
| **`client`** | `crates/client` | Session runner, world view state, telemetry bus, CLI examples |
| **`gui`** | `crates/gui` | Live debug radar visualizer built with `egui` / `eframe` |

---

## Prerequisites

- **Rust**: 1.85+ (Edition 2024)
- Installed via [rustup](https://rustup.rs/):
  ```bash
  rustup update stable
  ```

---

## Building

### 1. Build the Entire Workspace (All Crates & Examples)

```powershell
# Debug build
cargo build --workspace --all-targets

# Optimized release build
cargo build --workspace --all-targets --release
```

### 2. Build Specific Components

- **reBots Radar Visualizer (`gui`)**:
  ```powershell
  cargo build -p gui
  # Output: target/debug/gui.exe
  ```

- **reBots Client Runner (`capture_running`)**:
  ```powershell
  cargo build -p client --example capture_running
  # Output: target/debug/examples/capture_running.exe
  ```

- **Core Libraries Only**:
  ```powershell
  cargo build -p proto -p auth -p nav -p bot
  ```

---

## Running

### 1. Launch Live Radar GUI

You can start the visualizer directly or using the detached PowerShell helper script:

- **Via Cargo**:
  ```powershell
  cargo run -p gui
  ```

- **Via Helper Script** (detached background process, recommended on Windows):
  ```powershell
  powershell -File scripts/start-gui.ps1
  # With watchdog auto-restart:
  powershell -File scripts/start-gui.ps1 -Watchdog
  ```

### 2. Launch a Bot Swarm

Deploy a fleet of reBots against a local or remote server:

```powershell
# 1. Build the runner example
cargo build -p client --example capture_running

# 2. Launch bots (e.g., 2 bots for 120s against 127.0.0.1:27015)
powershell -File scripts/swarm.ps1 -N 2 -Secs 120 -Addr "127.0.0.1:27015"
```

### 3. Launch a Single Bot

```powershell
cargo run -p client --example capture_running -- 127.0.0.1:27015 120 captures/test.bin
```

---

## Environment Variables (`REB_` / `AIPLAYERS_`)

All configuration variables support the new `REB_` prefix (with fallback to `AIPLAYERS_`):

| Variable | Description | Default |
| :--- | :--- | :--- |
| `REB_NAME` | Bot player name | `reBot` |
| `REB_KEY` | RevEmu CD key | `REBBOT000000001` |
| `REB_TEAM` | Team assignment (`1` = Terrorist, `2` = Counter-Terrorist) | `1` |
| `REB_TELEMETRY_PORT` | UDP port for sending/receiving GUI radar telemetry | `27016` |
| `REB_TEAM_PORT` | UDP port for G0 team multicast bus | `27017` |
| `REB_MAP` | Map name | `de_dust2` |
| `REB_SEED` | PRNG seed for personality and movement | Hash of CD key |
| `REB_DIFFICULTY` | Bot skill (`easy`, `normal`, `hard`, `unfair`) | Derived from seed |
| `REB_MAPS_DIR` | Directory containing `.bsp` map files | `testserver/cstrike/maps` |
| `REB_CSTRIKE_DIR` | Directory containing game resources for consistency checks | `testserver/cstrike` |

---

## Running Tests

Run all unit and integration tests across all workspace crates:

```powershell
cargo test --workspace
```

---

## License

This project is licensed under the [MIT License](LICENSE).
