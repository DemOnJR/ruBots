#!/bin/sh
# Copy the mounted config into place, then hand off to HLDS.
#
# Why this exists rather than bind-mounting the .cfg files directly:
# HLDS's filesystem_stdio cannot stat a single-file bind mount under Docker
# Desktop. The file lists fine but stat() fails, and the engine reports
#
#     _stat on file .../cstrike/server.cfg which appeared to exist failed!!!
#     exec: not enough space for server.cfg
#
# and then runs with NONE of your settings applied -- silently, because it does
# not treat that as fatal. Directory bind mounts are unaffected, so we mount a
# directory and copy out of it at start-up. Config edits then take effect on a
# plain `docker compose restart`, with no image rebuild.
#
# `harness doctor` independently reads the live cvars back over rcon, because a
# config that is present is still not proof of a config that was applied.
set -eu

CSTRIKE=/home/steam/hlds/cstrike

if [ -d /cfg ]; then
    for f in /cfg/*.cfg /cfg/mapcycle.txt; do
        [ -e "$f" ] || continue
        cp -f "$f" "$CSTRIKE/$(basename "$f")"
        echo "entrypoint: applied $(basename "$f")"
    done
fi

mkdir -p "$CSTRIKE/logs"

exec "$@"
