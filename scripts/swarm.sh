#!/usr/bin/env bash
# Launch N bots against the test server, half on each team.
#
# Runs the already-built example directly rather than through `cargo run`:
# several `cargo` processes share one target directory and serialise on its
# lock, so the second bot would not connect until the first exited.
#
# Each bot needs its OWN key. Reunion is configured with `IDClientsLimit = 1`,
# so two connections presenting the same RevEmu key are one connection --- the
# second kicks the first, which reads as "the bots keep dropping".
#
#   scripts/swarm.sh 10 180        # 10 bots, 5v5, for 180 seconds
#   scripts/swarm.sh 2 120 27015   # a T and a CT, to watch them fight
#
# Logs land in captures/swarm/bot<N>.log; the server's own log is the thing to
# believe about kills and plants.
set -u

N=${1:-2}
SECS=${2:-120}
ADDR=${3:-127.0.0.1:27015}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXE="$ROOT/target/debug/examples/capture_running.exe"
[ -x "$EXE" ] || EXE="$ROOT/target/debug/examples/capture_running"
if [ ! -x "$EXE" ]; then
    echo "build it first:  cargo build -p client --example capture_running" >&2
    exit 1
fi

OUT="$ROOT/captures/swarm"
mkdir -p "$OUT"
rm -f "$OUT"/bot*.log

pids=()
for i in $(seq 1 "$N"); do
    # Alternate teams so the halves fill evenly: 1 = TERRORIST, 2 = CT.
    if [ $((i % 2)) -eq 1 ]; then team=1; else team=2; fi
    # 15 characters, unique per bot, stable across runs so the server sees the
    # same SteamID for "the same" bot each time.
    key=$(printf 'AIPLAYERBOT%04d' "$i")
    name=$(printf 'Bot%02d' "$i")

    AIPLAYERS_NAME="$name" \
    AIPLAYERS_KEY="$key" \
    AIPLAYERS_TEAM="$team" \
        "$EXE" "$ADDR" "$SECS" "$OUT/$name.bin" > "$OUT/bot$i.log" 2>&1 &
    pids+=($!)
    echo "  $name  team $team  key $key  pid ${pids[-1]}"
    # Stagger the joins. Ten simultaneous signons is ten bzip2 blobs in one
    # frame, and it is also not what a filling server looks like.
    sleep 1.5
done

echo "waiting for $N bots (${SECS}s)..."
for p in "${pids[@]}"; do wait "$p"; done
echo "done -- logs in $OUT"
