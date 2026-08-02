Counter-Strike 1.6 server — built with the CSB Server Creator
https://counter-strike-boost.com/addons/server-builder

HOW TO INSTALL
--------------
1. Download Counter-Strike 1.6 dedicated server files from Valve (SteamCMD,
   app 90). You end up with a folder that CONTAINS a `cstrike` directory.
2. Unzip this archive over THAT folder — the one holding cstrike/, not
   cstrike/ itself. Say yes to overwriting.
3. Start the server:  ./hlds_run -game cstrike +map de_dust2 +maxplayers 32
4. Check the console for "Metamod version" — that means the stack loaded.

If you get "Permission denied" on startup, your unzip tool dropped the
executable bits. Restore them with:
   chmod +x hlds_linux hltv && find . -name '*.so' -exec chmod +x {} +

CONFIGURATION: CSB optimized. server.cfg, amxx.cfg and the map rotation are
tuned by counter-strike-boost.com. Set hostname and rcon_password in
cstrike/server.cfg before going public.

COMPONENTS
----------
  ReHLDS                 3.15.0.896         https://github.com/rehlds/ReHLDS
                         license: GPL-3.0
  ReGameDLL_CS           5.30.0.814         https://github.com/rehlds/ReGameDLL_CS
                         license: GPL-3.0
  Metamod-R              1.3.0.149          https://github.com/rehlds/Metamod-R
                         license: GPL-3.0
  AMX Mod X 1.10         1.10.0-git5479     https://www.amxmodx.org
                         license: GPL-3.0
  AMX Mod X 1.10 — Counter-Strike module 1.10.0-git5479     https://www.amxmodx.org
                         license: GPL-3.0
  Reunion                0.2.0.25           https://github.com/rehlds/reunion
                         license: GPL-3.0
  ReAPI                  5.29.0.358         https://github.com/rehlds/ReAPI
                         license: GPL-3.0
  ReChecker              2.7                https://github.com/rehlds/rechecker
                         license: GPL-3.0
  ReSemiclip             2.4.3              https://github.com/rehlds/ReSemiclip
                         license: GPL-3.0
  SafeNameAndChat        1.2Beta3           https://github.com/WPMGPRoSToTeMa/SafeNameAndChat
                         license: Open source
  Revoice                0.1.0.34           https://github.com/rehlds/Revoice
                         license: GPL-3.0

Each component is redistributed under its own license, unmodified, from its
official release channel. Follow the links above for sources and terms.
