# ruBots — how to run the download

Two binaries, because one is not much use without the other:

| file | what it is |
| --- | --- |
| `rubots-control.exe` | the control centre: radar, fleet, deploy, server, console, replay |
| `rubots-bot.exe` | one bot. The control centre launches copies of this; you can also run it directly |

Nothing to install. Put them both in the same folder and run
`rubots-control.exe`.

## You also need a server, and the game files

The bots connect over UDP as ordinary clients, so there has to be something to
connect to, and the server must accept emulator tickets — **Reunion** with
`cid_RevEmu = 1`. The repository ships a ready-made one under `testserver/`:

```
cd testserver
docker compose up -d
```

The radar draws its map from the map's own `.bsp`, and a bot checks file
consistency against the game's resources, so point both at a `cstrike` folder:

* **In the app**: SETTINGS (key 7) → set `root` to a checkout that has
  `testserver/cstrike`, or edit `rubots-gui.conf` next to the exe.
* **On the command line**: `RUB_MAPS_DIR` and `RUB_CSTRIKE_DIR`.

Without them everything still runs — the radar simply says "no map loaded".

## Five minutes from download to bots on a radar

1. Start the server (above), or point the app at one you already run:
   SERVER (key 5) shows whether it answers, using the game's own A2S query.
2. DEPLOY (key 4): set how many bots and press LAUNCH. They are started a few
   seconds apart on purpose — a mass connect from one IP trips ReAuthCheck and
   gets the address banned for an hour.
3. RADAR (key 2): the swarm on the map, with a replay scrubber over the last
   two minutes. **Tab** cycles the selected bot and draws the sight lines it
   is watching.
4. CONSOLE (key 6): every bot's output in one stream.

## Environment variables

Every one takes the `RUB_` prefix (`REB_` and `AIPLAYERS_` still work).

| variable | what it does | default |
| --- | --- | --- |
| `RUB_NAME` | bot name | `ruBot` |
| `RUB_KEY` | RevEmu CD key | `RUBBOT000000001` |
| `RUB_TEAM` | 1 = T, 2 = CT | `1` |
| `RUB_MAP` | map name | `de_dust2` |
| `RUB_MAPS_DIR` | where the `.bsp` files are | `testserver/cstrike/maps` |
| `RUB_CSTRIKE_DIR` | game resources, for consistency checks | `testserver/cstrike` |
| `RUB_TELEMETRY_PORT` | radar telemetry | `27016` |
| `RUB_TEAM_PORT` | team bus | `27017` |
| `RUB_DIFFICULTY` | `easy` / `normal` / `hard` / `unfair` | from the seed |

Running one bot by hand:

```
set RUB_NAME=ruBot01
set RUB_KEY=RUBBOT0001
set RUB_TEAM=2
rubots-bot.exe 127.0.0.1:27015 300 captures\ruBot01.bin
```

The arguments are the server address, how many seconds to stay, and where to
write the capture.

## If something does not work

* **Bots time out at signon.** Usually the address ban above: stop everything,
  wait, and restart the server (`docker compose restart`) to clear it. Give
  launches more space with the stagger on the DEPLOY screen.
* **The radar says "no map loaded".** `RUB_MAPS_DIR` is not pointing at a
  folder with the map's `.bsp` in it.
* **`rubots-control.exe` opens and closes.** It writes `gui.log` beside
  itself; the reason will be in there. The usual one is another copy already
  holding the telemetry port.
