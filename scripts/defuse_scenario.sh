#!/usr/bin/env bash
# Prove a defuse: plant unopposed, then bring counter-terrorists in against a
# live bomb.
#
# Every earlier attempt failed for a scenario reason rather than a code one:
#
#   1 T vs 4 CT   the carrier is killed 15-20 s in, every round, and never plants
#   4 T vs 2 CT   the terrorists wipe the CT side in ~20 s and the round ends by
#                 ELIMINATION before anyone walks to a site
#   1 T, CTs on a fixed sleep
#                 works only when the guessed delay happens to straddle the
#                 plant -- which is the one variable that must not be under test
#
# So the CTs wait for the bomb to actually be down.
#
# The trigger is the PLANTER'S OWN decoded state, not the server log. Polling
# the log is self-defeating: forcing a flush with `rcon log off` ROTATES the
# file, so a plant recorded before a poll lands in a file the next poll no
# longer looks at. That cost two runs -- a plant had happened and the harness
# reported "no plant yet" for four minutes.
#
#   scripts/defuse_scenario.sh [max_wait_seconds]
set -u

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
EXE=target/debug/examples/capture_running.exe
OUT=captures/defuse
MAX_WAIT=${1:-240}

if [ ! -x "$EXE" ]; then
    echo "build it first: cargo build -p client --example capture_running" >&2
    exit 1
fi
mkdir -p "$OUT"
rm -f "$OUT"/*.log "$OUT"/*.bin "$OUT"/*.sent

rcon() { python scripts/rcon.py "$@" >/dev/null 2>&1; }

# A long fuse so the CTs have time to cross the map, and a long round so an
# elimination cannot reset it out from under us.
for c in "mp_c4timer 180" "mp_roundtime 9" "mp_startmoney 16000" "mp_freezetime 2"; do
    rcon "$c"
done
rcon "sv_restartround 1"
sleep 3

REB_NAME=Tplanter AIPLAYERS_NAME=Tplanter REB_KEY=REBDK0000000001 AIPLAYERS_KEY=REBDK0000000001 REB_TEAM=1 AIPLAYERS_TEAM=1 \
    "$EXE" 127.0.0.1:27015 420 "$OUT/Tplanter.bin" > "$OUT/Tplanter.log" 2>&1 &
echo "terrorist away, unopposed; waiting for the bomb"

waited=0
planted=no
while [ "$waited" -lt "$MAX_WAIT" ]; do
    sleep 5
    waited=$((waited + 5))
    if grep -qa "BOMB PLANTED" "$OUT/Tplanter.log" 2>/dev/null; then
        planted=yes
        echo "*** bomb is down after ${waited}s -- sending the CTs ***"
        break
    fi
done

if [ "$planted" = no ]; then
    echo "!!! no plant within ${MAX_WAIT}s; sending the CTs anyway so the run still says something"
fi

for spec in "CTx:REBDK0000000002" "CTy:REBDK0000000003" "CTz:REBDK0000000004"; do
    n=${spec%%:*}
    k=${spec##*:}
    REB_NAME="$n" AIPLAYERS_NAME="$n" REB_KEY="$k" AIPLAYERS_KEY="$k" REB_TEAM=2 AIPLAYERS_TEAM=2 \
        "$EXE" 127.0.0.1:27015 200 "$OUT/$n.bin" > "$OUT/$n.log" 2>&1 &
    sleep 2
done

wait
echo "SEQUENCE DONE (planted=$planted)"
