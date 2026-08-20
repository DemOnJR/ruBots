# ruBots (RUB)

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

## Prerequisites & Installation

### 1. Install Rust (Compiler & Cargo)

- **Windows** (PowerShell):
  ```powershell
  winget install Rustlang.Rustup
  # Restart PowerShell, then ensure you are on the stable channel:
  rustup update stable
  ```
- **macOS / Linux**:
  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup update stable
  ```

### 2. Install Docker Desktop (Local CS 1.6 Server)

- **Windows**:
  1. Install via Windows Package Manager:
     ```powershell
     winget install Docker.DockerDesktop
     ```
     Or download the installer from [Docker Desktop for Windows](https://www.docker.com/products/docker-desktop/).
  2. During installation, ensure the **WSL 2 Backend** option is enabled (recommended).
  3. Start Docker Desktop from the Start menu and wait for the status icon to turn green (*Engine running*).
- **Linux (Ubuntu/Debian)**:
  ```bash
  sudo apt-get update && sudo apt-get install -y docker.io docker-compose-v2
  sudo usermod -aG docker $USER
  newgrp docker
  ```
- **macOS**:
  ```bash
  brew install --cask docker
  ```

---

## 🚀 Step-by-Step Tutorial: From Zero to Running Bots & Radar GUI

Follow this end-to-end tutorial to set up the dedicated test server, compile the workspace, launch the live radar visualizer, and deploy bots.

### Step 1: Install & Start the Local CS 1.6 Dedicated Server

The repository comes with a pre-configured ReHLDS + ReGameDLL + Reunion server container under `testserver/`.

1. Open PowerShell and navigate to the `testserver/` directory:
   ```powershell
   cd testserver
   ```
2. Build and start the container in detached mode:
   ```powershell
   docker compose up -d --build
   ```
   > **Note**: On the first run, SteamCMD automatically downloads the clean GoldSrc base (`app_set_config 90`) and overlays the ReHLDS binary stack. This takes 1–2 minutes.
3. Verify that the server is up and listening on UDP port `27015`:
   ```powershell
   docker compose logs -f
   ```
   *(Press `Ctrl+C` to exit the log view)*
4. Return to the project root folder:
   ```powershell
   cd ..
   ```

### Step 2: Compile ruBots & the GUI Visualizer

Compile the entire workspace (all crates, examples, and tools):

```powershell
# Option A: Debug build (faster compilation for development)
cargo build --workspace --all-targets

# Option B: Optimized release build (maximum tickrate & raycast performance)
cargo build --workspace --all-targets --release
```

Key build outputs:
- **`target/debug/gui.exe`**: Live `egui`-based tactical radar visualizer.
- **`target/debug/examples/capture_running.exe`**: Bot client session runner.

### Step 3: Start the Live Radar GUI Visualizer

Launch the tactical visualizer to monitor real-time bot navigation, raycasting, weapon state, and combat decisions on `de_dust2`:

- **Launch directly via Cargo**:
  ```powershell
  cargo run -p gui
  ```
- **Or launch via the detached Windows helper script** (runs in background with auto-restart watchdog):
  ```powershell
  powershell -File scripts/start-gui.ps1 -Watchdog
  ```
  *(The GUI automatically binds to UDP port `27016` to receive telemetry from active bots).*

### Step 4: Start the App / Launch Bots

Once the server and Radar GUI are running, deploy the bots against the server:

- **Option A — Launch a Bot Swarm (Match Simulation)**:
  Deploy multiple bots split evenly between Terrorists and Counter-Terrorists (e.g. 10 bots for 300 seconds):
  ```powershell
  powershell -File scripts/swarm.ps1 -N 10 -Secs 300 -Addr "127.0.0.1:27015"
  ```
- **Option B — Launch a Single Bot Runner**:
  ```powershell
  cargo run -p client --example capture_running -- 127.0.0.1:27015 120 captures/test.bin
  ```

Look at the **ruBots Radar** window: you will see the bots connect, authenticate via RevEmu tickets, spawn, buy weapons, calculate path lattice routes, and engage each other!

### Step 5: Stopping Services

- **Stop bots**: Press `Ctrl+C` in the terminal or wait for the swarm duration timer to expire.
- **Stop Radar GUI**: Close the GUI window or run:
  ```powershell
  Stop-Process -Name gui -Force
  ```
- **Stop Docker Server**:
  ```powershell
  cd testserver
  docker compose down
  cd ..
  ```

---

## Environment Variables (`RUB_` / `REB_` / `AIPLAYERS_`)

All configuration variables support the new `RUB_` prefix (with fallback to `REB_` and `AIPLAYERS_`):

| Variable | Description | Default |
| :--- | :--- | :--- |
| `RUB_NAME` | Bot player name | `ruBot` |
| `RUB_KEY` | RevEmu CD key | `RUBBOT000000001` |
| `RUB_TEAM` | Team assignment (`1` = Terrorist, `2` = Counter-Terrorist) | `1` |
| `RUB_TELEMETRY_PORT` | UDP port for sending/receiving GUI radar telemetry | `27016` |
| `RUB_TEAM_PORT` | UDP port for G0 team multicast bus | `27017` |
| `RUB_MAP` | Map name | `de_dust2` |
| `RUB_SEED` | PRNG seed for personality and movement | Hash of CD key |
| `RUB_DIFFICULTY` | Bot skill (`easy`, `normal`, `hard`, `unfair`) | Derived from seed |
| `RUB_MAPS_DIR` | Directory containing `.bsp` map files | `testserver/cstrike/maps` |
| `RUB_CSTRIKE_DIR` | Directory containing game resources for consistency checks | `testserver/cstrike` |

---

## Running Tests

### 1. Unit & Offline Tests

Run all unit and offline integration tests across all workspace crates (does not require Docker):

```powershell
cargo test --workspace
```

### 2. Live Server Integration Tests (Requires Docker Desktop)

Integration tests against a real HLDS / ReHLDS server (`crates/proto/tests/live_server.rs`, `crates/auth/tests/live_connect.rs`, `crates/client/tests/live_signon.rs`) require a running local test server.

1. **Start the local Docker testserver**:
   ```powershell
   cd testserver
   docker compose up -d
   cd ..
   ```

2. **Run the integration suite**:
   ```powershell
   cargo test --workspace
   ```

   *(Optional)* Enforce that live server tests fail if the testserver is unreachable:
   ```powershell
   $env:AIPLAYERS_REQUIRE_SERVER="1"
   cargo test --workspace
   ```

3. **Stop the testserver**:
   ```powershell
   cd testserver
   docker compose down
   cd ..
   ```

---

## Verified Server Compatibility Matrix

ruBots has been tested and verified across both local Docker stacks and remote dedicated Linux/Windows servers running the modern ReHLDS stack:

| Target / Server | Engine / Build | GameDLL | Auth Layer | AMX Mod X / Addons | Status |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Local Docker Testserver**<br>`127.0.0.1:27015` | **ReHLDS** `3.14.0.849`<br>(Protocol 48 / Stdio 4419) | **ReGameDLL_CS** `5.26.0.668` | **Reunion** `0.2.0.13`<br>(`cid_RevEmu = 1`) | **Metamod-r** `1.3.0.149`<br>**AMX Mod X** `1.10.0.5474` | Verified (Signon, Swarm, Buy, Combat, Defuse) |
| **Remote Live Server**<br>*(Live Dedicated Host - Non-local)* | **ReHLDS** `3.14.x`<br>(Protocol 48 / Stdio Linux) | **ReGameDLL_CS** | **Reunion**<br>(`cid_RevEmu = 1`) | **AMX Mod X** `1.9` / `1.10`<br>VoiceTranscoder | Verified (Remote Signon, `clc_fileconsistency`, Team Join, Live Navigation) |

> **Note on Auth Requirements**: Because bots connect over standard UDP as emulated clients, the server must support RevEmu / emulator tickets (e.g. via **Reunion** configured with `cid_RevEmu = 1`).

---

## Known Issues & Limitations

* **Planar vs Full 3D Vertical Height Awareness (Obstacle Jump Limits)**:
  * The path navigation system uses a 2D lattice representation with floor-height snapping and hull collision traces. In complex multi-level geometry, bots do not fully compute the vertical clearance trajectory before jumping, which can cause them to attempt jumps against obstacles or ledges that exceed the maximum engine jump height (~45–64 units).
* **PVS Network Visibility Limits**:
  * As genuine network clients, bots receive entity positions only when delivered in the server's PVS (Potential Visible Set) frames. Teammates or enemies blocked behind dense walls or outside PVS are not tracked until updated by the engine.
* **Ledge Drop-Down Traversal**:
  * On steep vertical drops without stairs or ladders, bots may hesitate and trigger unstuck re-evaluations before committing to the fall.
* **Anti-Flood / Fast Reconnect Rate Limits**:
  * Rapid mass connection/disconnection of bots from a single IP address can trigger server-side rate limiters or firewall protection (such as ReAuthCheck / ReChecker threshold rules). Staggered connect delays (`scripts/swarm.ps1`) are recommended.

---

## License

This project is licensed under the [MIT License](LICENSE).
