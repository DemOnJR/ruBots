# Joining a remote HLDS server

## What works today

Bots authenticate with a **RevEmu** (Steam emulator) certificate
(`crates/auth`). That is accepted by:

- Local Docker stack (`testserver/` + Reunion / ReHLDS plugins)
- Public non-Steam / emulator-friendly servers (Reunion, dproto, old nonsteam)

## Live test: `85.215.153.249:27015` (Nexaplay CS 1.6)

**Status 2026-08-11 (after netchan fix):** one bot joins, spawns, moves, fights.

| Check | Result |
|-------|--------|
| Host reachable (UDP) | **yes** |
| RevEmu / STEAM_2 auth | **yes** (Reunion with `cid_RevEmu=1`) |
| Signon + resource list | **yes** |
| `clc_fileconsistency` + spawn | **yes** (was broken — see below) |
| Team join + streaming | **yes** — `*** SERVER IS STREAMING ***` |
| In-game movement | **yes** — thousands of units on de_dust2 |

Probe capture: `captures/remote/r4` / `r4.log`.

**SSH from this machine:** no working key (publickey denied). Server-side
Reunion changes go through **Nexaplay panel** or your own SSH session.

---

## Trap: `Invalid length` / `badread on opcode clc_fileconsistency`

### What the server log looks like

```
"RemoteBot01<…><STEAM_2:…><>" connected, address "…"
Dropped RemoteBot01 from server
Reason:  Invalid length
SV_ReadClientMessage: badread on RemoteBot01, opcode clc_fileconsistency
```

Auth is already past. Drop happens when the server parses `clc_fileconsistency`
(opcode 7). ReHLDS path: `SV_ParseConsistencyResponse` (`sv_user.cpp`) —

```c
int value = MSG_ReadShort();  // declared body length
if (value <= 0 || !SZ_HasSomethingToRead(&net_message, value)) {
    SV_DropClient(..., "Invalid length");
}
```

So either the length field is ≤0, or **the packet is shorter than the declared
length**. Not “Bad file data” (wrong entry count) and not “Malformed compressed
data” (BZ2 fail).

### What we thought first (wrong)

- Wrong bit packing / munge key on the consistency body  
- Missing BZ2 compression on large fragment uploads  
- Declared `u16` length not matching `BitWriter` size  

Those matter for *other* drop reasons; they were **not** this one. The body we
built was fine (`07 d3 05` → length **1491**, 80 demands, munged correctly).

### What it actually was

`clc_fileconsistency` + trailing `spawn` is sent on the **fragment stream**
(`NetChannel::queue_fragmented`, 128-byte pieces). Only one reliable is in
flight at a time; later fragments wait for ACK.

After the “don’t retransmit reliable every packet” change (to avoid reliable
overflow), `transmit` still **always appended `reliable_buf` to the payload**,
even when `send_reliable` was false. Idle / move packets therefore looked like:

| Packet | Flags | Body |
|--------|--------|------|
| frag 1/12 | reliable + fragment | 10-byte frag header + 128 B chunk |
| **idle (bug)** | **neither** | **raw chunk** starting `07 d3 05…` |
| frag 2/12 | reliable + fragment | header + next 128 B |
| **idle (bug)** | **neither** | **raw next chunk** |

The server treated the idle body as a normal clc stream: opcode 7, length 1491,
but only ~140 bytes left in the datagram → **Invalid length** + badread on
`clc_fileconsistency`. Capture proof: `captures/remote/r3.bin.sent` sequences
~100–104 (frag, then bare payload, frag, then bare payload forever).

Only **2 of 12** fragments were ever promoted; the channel looked “stuck,” but
the kill was the bare opcode on idle packets.

### Fix (do not regress)

`crates/netchan/src/lib.rs` — `NetChannel::transmit`:

- Include `reliable_buf` (and the fragment header) **only if** `send_reliable`
  is true — same as ReHLDS `Netchan_Transmit` (`if (send_reliable) SZ_Write(...)`).
- Idle packets carry **only** the unreliable body (moves / nops).

Regression tests in the same file:

- `idle_packets_do_not_leak_in_flight_reliable_payload`
- `a_fragment_waits_for_its_acknowledgement_before_the_next_one` (asserts idle
  bodies are pure nop)

### How to re-check if it comes back

1. Server log: `Invalid length` + `opcode clc_fileconsistency` after STEAM ok.  
2. Decode `.sent` capture: any **non-fragment** packet whose unmunged body
   starts with `07` while a multi-fragment upload is in progress = leak is back.  
3. `cargo test -p netchan idle_packets_do_not_leak`

## What you need on the server

Pick one:

### A) Emulator auth (recommended for AI bots)

#### Via Nexaplay panel (this host)

1. Open the **Nexaplay CS 1.6** instance.
2. **Add-ons / CSB stack** → install/enable **Reunion** (same as local testserver).
3. Metamod `plugins.ini` must load (uncommented):  
   `linux addons/reunion/reunion_mm_i386.so`
4. Edit `cstrike/reunion.cfg`:

```ini
cid_RevEmu = 1
cid_RevEmu2013 = 1
cid_OldRevEmu = 1
cid_SteamEmu = 1
AuthVersion = 3
IDClientsLimit = 8
```

   If `cid_RevEmu = 5`, bots get exactly `STEAM validation rejected`.
5. **Restart** the game server in the panel.
6. Reply **“restarted”** — join is re-tested from this PC.

#### Via SSH (your session)

```bash
# from HLDS root (has cstrike/)
bash enable-revemu-auth.sh /path/to/hlds   # scripts/enable-revemu-auth.sh
# restart hlds
```

Local working reference:  
`testserver/rehlds/cstrike/reunion.cfg` (`cid_RevEmu = 1`).

After Reunion accepts RevEmu, from the project root:

```powershell
cd D:\Downloads\app\aiplayers-rs
cargo build -p client --example capture_running

# one probe bot
$env:AIPLAYERS_NAME="RemoteBot01"
$env:AIPLAYERS_KEY="AIPLAYERBOT0001"
$env:AIPLAYERS_TEAM="1"
.\target\debug\examples\capture_running.exe 85.215.153.249:27015 120 captures\remote\r1.bin

# or a small swarm
powershell -File scripts\swarm.ps1 -N 4 -Secs 300 -Addr "85.215.153.249:27015"
```

### B) Real Steam tickets

Not implemented. Would need Steam client tickets / Game Coordinator — different
project from RevEmu.

## Traps (same as local)

- Do not mass-disconnect from one IP (ReAuthCheck / MaxDrop style bans)
- Unique `AIPLAYERS_KEY` per bot
- Staged exits: use `swarm.ps1` (already spreads disconnects)
- **`Invalid length` on `clc_fileconsistency`** — almost always the netchan
  idle-packet leak (above), not a bad consistency builder. Fix is in
  `transmit`, not `build_spawn_upload`.

## Map / BSP

Bots need the **same map BSP** under the nav/map search path as the server
(e.g. `de_dust2`). If the remote map is custom, put its `.bsp` where
`Map::load` finds it (same as local).
