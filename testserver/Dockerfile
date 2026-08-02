# Local Counter-Strike 1.6 dedicated server for protocol testing.
#
# Built from Valve's own SteamCMD (downloaded from the Steam CDN) installing
# HLDS app 90 with the cstrike mod. No third-party installer scripts.
#
# HLDS is a 32-bit binary, hence the i386 multiarch runtime libraries.
FROM debian:bookworm-slim

RUN dpkg --add-architecture i386 \
 && apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      lib32gcc-s1 \
      lib32stdc++6 \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home --shell /bin/bash steam
USER steam
WORKDIR /home/steam

RUN mkdir -p steamcmd hlds \
 && curl -sSfL https://steamcdn-a.akamaihd.net/client/installer/steamcmd_linux.tar.gz \
    | tar -xz -C steamcmd

# `app_update 90` is notoriously flaky and often needs several passes before it
# reports success, so retry rather than failing the build on the first hiccup.
RUN set -eu; \
    for attempt in 1 2 3 4 5 6; do \
      echo "=== HLDS install attempt ${attempt} ==="; \
      ./steamcmd/steamcmd.sh \
          +force_install_dir /home/steam/hlds \
          +login anonymous \
          +app_set_config 90 mod cstrike \
          +app_update 90 validate \
          +quit && break; \
      sleep 5; \
    done; \
    test -x /home/steam/hlds/hlds_linux

# HLDS refuses to boot with "Unable to initialize Steam" unless it can find
# steamclient.so at this exact path, even for a LAN server.
RUN mkdir -p /home/steam/.steam/sdk32 \
 && ln -sf /home/steam/steamcmd/linux32/steamclient.so \
           /home/steam/.steam/sdk32/steamclient.so

# The ReHLDS stack, unzipped over the Valve install exactly as its own README
# instructs (over the directory CONTAINING cstrike/, not cstrike/ itself).
#
# Components, all GPL-3.0 from github.com/rehlds: ReHLDS 3.15.0.896,
# ReGameDLL_CS 5.30.0.814, Metamod-R 1.3.0.149, Reunion 0.2.0.25, ReAPI
# 5.29.0.358, ReChecker 2.7, ReSemiclip 2.4.3, ReVoice, SafeNameChat, and
# AMX Mod X 1.10 with the Counter-Strike module.
#
# This replaces the stock Valve engine (engine_i486.so) and game DLL
# (cstrike/dlls/cs.so). It is what real public servers run, so it is the
# representative target for the bot -- stock Valve HLDS is the outlier.
COPY --chown=steam:steam rehlds/ /home/steam/hlds/

# NOTE: the CSB bundle does NOT ship libsteam_api.so, so the Valve one is left
# in place -- and that one does not export SteamGameServer_Init, which the
# ReHLDS engine needs (`rehlds/CMakeLists.txt` links `steam_api`, and the
# project ships its own at rehlds/lib/linux32/). Without it the server dies at
# startup with `engine_i486.so: undefined symbol: SteamGameServer_Init`.
# The correct library is vendored into rehlds/ alongside the bundle.

# Unzip drops the executable bits; the bundle's README calls this out.
RUN chmod +x /home/steam/hlds/hlds_linux \
 && find /home/steam/hlds -name '*.so' -exec chmod +x {} +

# Baked-in copies so the image boots sanely even without the compose bind
# mounts. docker-compose then mounts the same three files read-only, which is
# what makes an edit take effect without a rebuild.
#
# NOTE the precedence, verified in source: the engine exec's server.cfg exactly
# once at startup (rehlds/engine/sys_dll2.cpp:669), while ReGameDLL's
# InstallGameRules exec's game.cfg on EVERY map load
# (regamedll/dlls/gamerules.cpp:136). game.cfg therefore WINS from the first map
# change onward. Anything that must survive a map change goes in game.cfg.
#
# Copied AFTER the bundle so ours win over the ones it ships.
COPY --chown=steam:steam server.cfg /home/steam/hlds/cstrike/server.cfg
COPY --chown=steam:steam game.cfg /home/steam/hlds/cstrike/game.cfg
COPY --chown=steam:steam mapcycle.txt /home/steam/hlds/cstrike/mapcycle.txt

# Log_Open writes L<MM><DD><NNN>.log here (rehlds/engine/sv_log.cpp:117-168).
# Created up front so the compose bind mount lands on a steam-owned directory.
RUN mkdir -p /home/steam/hlds/cstrike/logs

WORKDIR /home/steam/hlds
ENV LD_LIBRARY_PATH=/home/steam/hlds:/home/steam/hlds/cstrike

EXPOSE 27015/udp 27015/tcp

# sv_lan 1 keeps the server off the master list and lets it accept STEAM_ID_LAN
# identities -- which is what the client under test presents.
# mp_timelimit 0 pins the map: without it the server cycles maps on a timer,
# which makes protocol captures non-reproducible (consistency responses hash
# map-specific resources, so a capture is only valid for the map it was taken
# on).
CMD ["./hlds_linux", \
     "-game", "cstrike", \
     "-port", "27015", \
     "-nomaster", \
     "-insecure", \
     "+ip", "0.0.0.0", \
     "+sv_lan", "1", \
     "+maxplayers", "12", \
     "+mp_timelimit", "0", \
     "+sv_voiceenable", "0", \
     "+mp_autoteambalance", "0", \
     "+mp_limitteams", "0", \
     "+map", "de_dust2"]
