#!/usr/bin/env python3
"""One pass over `captures/swarm/bot*.log` producing the humanize-plan section-1
metrics table as a tab-separated row, so "before" and "after" are two rows and
not two recollections.

    python scripts/metrics.py captures/swarm

Metrics (see docs/humanize-plan.md section 1):

  PILE-1   share of `arrived` samples in the single busiest 64u cell
  PILE-2   mean / max bots inside one 192u ball
  PILE-3   share of all live bots standing in that one box
  CONGA-1  live samples within 100u of where a same-team bot stood <=10s earlier
  CONGA-2  same-team pair-time within 300u, live and moving
  SEP-CT   median same-team pairwise separation, CT side
  SEP-200  pair-time within 200u, CT / T
  COVER-1  distinct 128u cells occupied per live bot per instant
  COVER-2  top-5 128u cells' share of live bot-time
  ROUTE-1  same-team Jaccard of visited 128u cell sets
  ROUTE-2  same-team Jaccard of steered-waypoint sets
  ROUTE-3  distinct nav nodes ever steered at (from `wp` counts)
  ROUTE-4  share of steering ticks absorbed by the top 20 waypoints
  STILL-1  live samples with velocity < 1 u/s
  RUNG-1   samples on rung `arrived`
  SPEED-1  walking samples with `fwd` exactly 250.0
  VIEW-1   median |yaw - bearing to objective| while walking
  VIEW-2   consecutive live samples with an identical integer yaw

The log lines the script reads (from `capture_running`):

  t+   2s origin [  -655   -678   164] vel   237 hp 100 maxspeed  250 alive true
  brain: alive true frozen false fwd 237 side -1 yaw -3 site Some([-600, -680]) wp 55 reroutes 0 stuck false
  obj: rung plant-walk bomb true arming false attack false use false to_goal 3629
  objective: [1122 2487 144]

`team` is `bot_index % 2` (odd = T), per scripts/swarm.sh. `wp` is the count of
waypoints remaining, and `reroutes` the count of route re-plans.
"""

import sys
import re
from collections import defaultdict
from itertools import combinations
from math import atan2
from pathlib import Path

ORIGIN_RE = re.compile(
    r"t\+\s*(\d+)s origin \[\s*(-?\d+)\s+(-?\d+)\s+(-?\d+)\]\s+vel\s+(-?\d+)"
)
BRAIN_RE = re.compile(
    r"brain: alive (\S+) frozen (\S+) fwd (-?\d+) side (-?\d+) yaw (-?\d+)"
    r" site (Some\(\[[^\]]*\]\)|None) wp (\d+) node (-?\d+) reroutes (\d+) stuck (\S+)"
)
OBJ_RE = re.compile(
    r"obj: rung (\S+)\s+bomb (\S+)\s+arming (\S+)\s+attack (\S+)\s+use (\S+)\s+to_goal (-?\d+)"
)
OBJECTIVE_RE = re.compile(r"objective: \[\s*(-?\d+)\s+(-?\d+)\s+(-?\d+)\]")


def cell(origin, size):
    """The 2D cell key for a 128/64/192u grid."""
    return (origin[0] // size, origin[1] // size)


def parse_log(path, samples, objectives):
    """Collect per-bot sample lines into `samples` (shared list) and record
    this bot's objective."""
    team = "T" if int(path.stem[3:]) % 2 == 1 else "CT"
    seen_obj = False
    cur = None
    for line in path.read_text(errors="replace").splitlines():
        m = OBJECTIVE_RE.search(line)
        if m:
            objectives.append((team, int(m.group(1)), int(m.group(2))))
            seen_obj = True
            continue
        m = ORIGIN_RE.search(line)
        if m:
            # The origin is the header; brain/obj arrive on the following
            # lines, so stash it and attach them when they come.
            cur = {
                "team": team,
                "t": int(m.group(1)),
                "origin": (int(m.group(2)), int(m.group(3)), int(m.group(4))),
                "vel": int(m.group(5)),
                "alive": None,
                "fwd": None,
                "side": None,
                "yaw": None,
                "wp": None,
                "node": None,
                "reroutes": None,
                "rung": None,
                "to_goal": None,
                "objective": None,
            }
            continue
        if cur is None:
            continue
        b = BRAIN_RE.search(line)
        if b:
            cur["alive"] = b.group(1) == "true"
            cur["fwd"] = float(b.group(3))
            cur["side"] = float(b.group(4))
            cur["yaw"] = float(b.group(5))
            cur["wp"] = int(b.group(7))
            cur["node"] = int(b.group(8))
            cur["reroutes"] = int(b.group(9))
            continue
        o = OBJ_RE.search(line)
        if o:
            cur["rung"] = o.group(1)
            cur["to_goal"] = float(o.group(6))
            if cur["alive"] is not None:
                samples.append(cur)
            cur = None
    return seen_obj


def main():
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        sys.exit(1)
    root = Path(sys.argv[1])
    logs = sorted(root.glob("bot*.log"))
    if not logs:
        print(f"no bot*.log in {root}", file=sys.stderr)
        sys.exit(1)

    samples = []
    objectives = []
    for log in logs:
        parse_log(log, samples, objectives)

    # Distribute the (team, objective) to each sample of that team.
    obj_by_team = {}
    for team, x, y in objectives:
        obj_by_team.setdefault(team, (x, y))

    live = [s for s in samples if s["alive"]]
    n = len(live)
    if n == 0:
        print("no live samples", file=sys.stderr)
        sys.exit(1)

    # --- PILE-1/2/3 ------------------------------------------------------
    arrived = [s for s in live if s["rung"] == "arrived"]
    pile1 = 0.0
    if arrived:
        cells = defaultdict(int)
        for s in arrived:
            cells[cell(s["origin"], 64)] += 1
        pile1 = max(cells.values()) / len(arrived)

    # PILE-2: mean/max bots inside one 192u ball, sampled per tick.
    by_tick = defaultdict(list)
    for s in live:
        by_tick[(s["t"], s["team"])].append(s)
    pile2_means, pile2_max = [], 0
    for grp in by_tick.values():
        for a, b in combinations(grp, 2):
            d = ((a["origin"][0] - b["origin"][0]) ** 2 + (a["origin"][1] - b["origin"][1]) ** 2) ** 0.5
            if d <= 192:
                pile2_means.append(1.0)
        pile2_max = max(pile2_max, len(grp))
    pile2 = (sum(pile2_means) / max(1, len(by_tick)), pile2_max)

    # PILE-3: share of live bots standing in that one 192u box.
    if arrived:
        cells = defaultdict(int)
        for s in arrived:
            cells[cell(s["origin"], 192)] += 1
        pile3 = max(cells.values()) / len(arrived)
    else:
        pile3 = 0.0

    # --- CONGA-1: within 100u of where a same-team bot stood <=10s earlier ---
    by_team_t = defaultdict(list)
    for s in live:
        by_team_t[(s["team"], s["t"])].append(s)
    conga1 = 0.0
    for s in live:
        found = False
        for dt in range(1, 11):
            for o in by_team_t.get((s["team"], s["t"] - dt), []):
                d = ((s["origin"][0] - o["origin"][0]) ** 2 + (s["origin"][1] - o["origin"][1]) ** 2) ** 0.5
                if d <= 100:
                    found = True
                    break
            if found:
                break
        if found:
            conga1 += 1
    conga1 /= n

    # --- CONGA-2: same-team pair-time within 300u, live and moving ---------
    moving = [s for s in live if s["fwd"] != 0 or s["side"] != 0]
    pair_time, pair_moving = 0.0, 0.0
    for grp in by_tick.values():
        for a, b in combinations(grp, 2):
            d = ((a["origin"][0] - b["origin"][0]) ** 2 + (a["origin"][1] - b["origin"][1]) ** 2) ** 0.5
            if d <= 300:
                pair_time += 1
                if (a["fwd"] != 0 or a["side"] != 0) and (b["fwd"] != 0 or b["side"] != 0):
                    pair_moving += 1
    conga2 = pair_moving / max(1, len(moving))

    # --- SEP-CT / SEP-200 --------------------------------------------------
    sep_ct = []
    sep_t = []
    sep200_ct = sep200_t = 0
    for grp in by_tick.values():
        team = grp[0]["team"]
        for a, b in combinations(grp, 2):
            d = ((a["origin"][0] - b["origin"][0]) ** 2 + (a["origin"][1] - b["origin"][1]) ** 2) ** 0.5
            if team == "CT":
                sep_ct.append(d)
                if d <= 200:
                    sep200_ct += 1
            else:
                sep_t.append(d)
                if d <= 200:
                    sep200_t += 1
    def median(xs):
        xs = sorted(xs)
        return xs[len(xs) // 2] if xs else 0.0

    # --- COVER-1/2 ----------------------------------------------------------
    cover1 = 0.0
    cover2 = 0.0
    for grp in by_tick.values():
        cells = {cell(s["origin"], 128) for s in grp}
        cover1 += len(cells) / max(1, len(grp))
    cover1 /= max(1, len(by_tick))
    cell_time = defaultdict(int)
    for s in live:
        cell_time[cell(s["origin"], 128)] += 1
    top5 = sum(sorted(cell_time.values(), reverse=True)[:5])
    cover2 = top5 / n

    # --- ROUTE-1/2/3/4 ------------------------------------------------------
    # ROUTE-1: Jaccard of visited 128u cells, per team.
    team_cells = {"CT": set(), "T": set()}
    for s in live:
        team_cells[s["team"]].add(cell(s["origin"], 128))
    r1 = len(team_cells["CT"] & team_cells["T"]) / max(1, len(team_cells["CT"] | team_cells["T"]))

    # ROUTE-2: Jaccard of steered-waypoint sets. The log carries the actual
    # node id (`node`), so this is exact: the set of (team, node) values seen.
    node_sets = {"CT": set(), "T": set()}
    for s in live:
        if s["node"] is not None and s["node"] >= 0:
            node_sets[s["team"]].add(s["node"])
    r2 = len(node_sets["CT"] & node_sets["T"]) / max(1, len(node_sets["CT"] | node_sets["T"]))

    # ROUTE-3: distinct nav nodes ever steered at (of 4715 on de_dust2).
    # ROUTE-4: share of steering ticks absorbed by the top 20 nodes.
    node_counts = defaultdict(int)
    for s in live:
        if s["node"] is not None and s["node"] >= 0:
            node_counts[s["node"]] += 1
    r3 = len(node_counts)
    top20 = sum(sorted(node_counts.values(), reverse=True)[:20])
    r4 = top20 / n

    # --- STILL-1 / RUNG-1 / SPEED-1 -----------------------------------------
    still1 = sum(1 for s in live if s["vel"] < 1) / n
    rung1 = sum(1 for s in live if s["rung"] == "arrived") / n
    walking = [s for s in live if s["rung"] in ("goto", "plant-walk", "roam") and s["fwd"] > 0]
    speed1 = sum(1 for s in walking if s["fwd"] == 250.0) / max(1, len(walking))

    # --- VIEW-1/2 ------------------------------------------------------------
    # VIEW-1: |yaw - bearing to objective| while walking.
    view1 = []
    for s in walking:
        obj = obj_by_team.get(s["team"])
        if not obj:
            continue
        dx, dy = obj[0] - s["origin"][0], obj[1] - s["origin"][1]
        bearing = (atan2(dy, dx) * 180 / 3.141592653589793) % 360
        err = (s["yaw"] - bearing + 540) % 360 - 180
        view1.append(abs(err))
    view1 = median(view1) if view1 else 0.0

    # VIEW-2: consecutive live samples with identical integer yaw.
    prev = None
    view2 = 0
    total_pairs = 0
    for s in sorted(live, key=lambda s: (s["t"], s["team"])):
        if prev and prev["t"] == s["t"] - 2 and prev["team"] == s["team"]:
            total_pairs += 1
            if int(prev["yaw"]) == int(s["yaw"]):
                view2 += 1
        prev = s
    view2 = view2 / max(1, total_pairs)

    # --- output --------------------------------------------------------------
    print(f"logs {len(logs)}  live {n}")
    print(f"PILE-1\t{pile1:.3f}")
    print(f"PILE-2\t{pile2[0]:.1f}\t{pile2[1]}")
    print(f"PILE-3\t{pile3:.3f}")
    print(f"CONGA-1\t{conga1:.3f}")
    print(f"CONGA-2\t{conga2:.3f}")
    print(f"SEP-CT\t{median(sep_ct):.0f}\t{median(sep_t):.0f}")
    print(f"SEP-200\t{sep200_ct / max(1, len(sep_ct)):.3f}\t{sep200_t / max(1, len(sep_t)):.3f}")
    print(f"COVER-1\t{cover1:.3f}")
    print(f"COVER-2\t{cover2:.3f}")
    print(f"ROUTE-1\t{r1:.3f}")
    print(f"ROUTE-2\t{r2:.3f}")
    print(f"ROUTE-3\t{r3}")
    print(f"ROUTE-4\t{r4:.3f}")
    print(f"STILL-1\t{still1:.3f}")
    print(f"RUNG-1\t{rung1:.3f}")
    print(f"SPEED-1\t{speed1:.3f}")
    print(f"VIEW-1\t{view1:.1f}")
    print(f"VIEW-2\t{view2:.3f}")


if __name__ == "__main__":
    main()
