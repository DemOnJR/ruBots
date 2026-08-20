#!/usr/bin/env bash
# Enable Reunion RevEmu auth on a CS 1.6 / ReHLDS server so AI bots can join.
#
# Run ON THE GAME SERVER (as the user that owns the HLDS tree), from the
# cstrike directory or pass HLDS root as $1.
#
#   bash enable-revemu-auth.sh /path/to/hlds
#
# Then restart the server process and re-test bots from Windows:
#   capture_running.exe 85.215.153.249:27015 60 captures/remote/r1.bin
#
set -euo pipefail

ROOT="${1:-.}"
# Accept either hlds root or cstrike path
if [[ -d "$ROOT/cstrike" ]]; then
  CSTRIKE="$ROOT/cstrike"
elif [[ -f "$ROOT/server.cfg" || -d "$ROOT/addons" ]]; then
  CSTRIKE="$ROOT"
else
  echo "usage: $0 /path/to/hlds   (directory containing cstrike/)" >&2
  exit 1
fi

echo "==> cstrike: $CSTRIKE"

PLUGINS="$CSTRIKE/addons/metamod/plugins.ini"
REUNION_SO="$CSTRIKE/addons/reunion/reunion_mm_i386.so"
REUNION_CFG="$CSTRIKE/reunion.cfg"

if [[ ! -f "$PLUGINS" ]]; then
  echo "ERROR: metamod plugins.ini not found at $PLUGINS" >&2
  echo "This script expects Metamod + ReHLDS-style layout." >&2
  exit 1
fi

# 1) Ensure Reunion is listed (uncomment or append)
if grep -qiE 'reunion.*\.so' "$PLUGINS"; then
  # Uncomment any commented reunion line
  sed -i 's/^;[[:space:]]*\(linux[[:space:]]\+addons\/reunion\/reunion_mm_i386\.so\)/\1/I' "$PLUGINS" || true
  sed -i 's/^;[[:space:]]*\(linux[[:space:]]\+addons\/metamod\/reunion_mm_i386\.so\)/\1/I' "$PLUGINS" || true
  echo "Reunion entry present in plugins.ini"
else
  if [[ -f "$REUNION_SO" ]]; then
    echo "linux addons/reunion/reunion_mm_i386.so" >> "$PLUGINS"
    echo "Appended reunion to plugins.ini"
  else
    echo "ERROR: $REUNION_SO missing — install Reunion plugin first." >&2
    echo "  https://github.com/rehlds/reunion/releases" >&2
    exit 1
  fi
fi

# 2) reunion.cfg — force accept RevEmu (bots use this)
if [[ ! -f "$REUNION_CFG" ]]; then
  cat > "$REUNION_CFG" <<'EOF'
### AUTH CONTROL ###
cid_Steam = 1
cid_SteamPending = 5
cid_HLTV = 5
cid_NoSteam47 = 5
cid_NoSteam48 = 5
cid_RevEmu = 1
cid_RevEmu2013 = 1
cid_SC2009 = 1
cid_OldRevEmu = 1
cid_SteamEmu = 1
cid_AVSMP = 1
cid_Setti = 3
cid_SXEI = 1
AuthVersion = 3
SteamIdHashSalt = change-me-to-a-random-hex-string
SC2009_RevCompatMode = 1
IDClientsLimit = 5
LoggingMode = 1
EOF
  echo "Created $REUNION_CFG (set SteamIdHashSalt to a random value)"
else
  # Patch critical lines in place
  set_kv() {
    local key="$1" val="$2" file="$3"
    if grep -qE "^[[:space:]]*${key}[[:space:]]*=" "$file"; then
      sed -i "s/^[[:space:]]*${key}[[:space:]]*=.*/${key} = ${val}/" "$file"
    else
      echo "${key} = ${val}" >> "$file"
    fi
  }
  set_kv cid_RevEmu 1 "$REUNION_CFG"
  set_kv cid_RevEmu2013 1 "$REUNION_CFG"
  set_kv cid_OldRevEmu 1 "$REUNION_CFG"
  set_kv cid_SteamEmu 1 "$REUNION_CFG"
  set_kv AuthVersion 3 "$REUNION_CFG"
  # Allow a few bots sharing one IP (swarm from one PC)
  set_kv IDClientsLimit 8 "$REUNION_CFG"
  echo "Patched $REUNION_CFG for RevEmu accept"
fi

# 3) liblist.gam should load metamod, not vanilla dll
LIBLIST="$CSTRIKE/liblist.gam"
if [[ -f "$LIBLIST" ]]; then
  if grep -qi 'gamedll_linux.*metamod' "$LIBLIST"; then
    echo "liblist.gam already uses metamod"
  else
    echo "WARN: $LIBLIST may not load metamod — check gamedll_linux line"
    grep -i gamedll "$LIBLIST" || true
  fi
fi

echo
echo "Done. Restart HLDS, then from Windows:"
echo "  capture_running.exe <ip>:27015 60 captures/remote/r1.bin"
echo "Expect signon success, not 'STEAM validation rejected'."
echo
echo "Current plugins.ini reunion lines:"
grep -i reunion "$PLUGINS" || true
echo
echo "Current reunion auth lines:"
grep -E 'cid_RevEmu|AuthVersion|IDClientsLimit' "$REUNION_CFG" || true
