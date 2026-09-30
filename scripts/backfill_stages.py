#!/usr/bin/env python3
"""Build every route's stages from the stop lists we already have.

Run once per feed. It writes `gtfs_stage`, `gtfs_stage_stop`, `gtfs_route_stage`
and `gtfs_stage_review` (docs/gtfs-editor.md sections 19 and 19.1), so that every
route becomes a route built from stages.

The model, whole
----------------

**One stage per fare-stage name and direction.** SAIDAPET on the up routes is one
stage; SAIDAPET on the down routes is another.

Its stops are the list **the most routes give it**. `gtfs_route_stop` was filled
route by route, so routes do not always agree about a stage - one lists 10 stops,
the next 9, a third only the fare boundary. Taking what most of them say is the
best guess available, and it is only a guess: every name the routes disagree
about is raised in `gtfs_stage_review` with **all** the lists they give, and a
person settles it in the dashboard, builds the real stop list there, and that
goes into a draft like any other edit.

Taking the LONGEST list instead - `--pick longest` - is the rule as first stated.
On chennai_bus it more than doubles the feed (101,772 stop calls -> 232,862),
because most routes record only the fare boundary and one route's full list is
then handed to all of them. The commonest list moves it by -13%. Neither is
right until a person has looked; the commonest is much the closer starting point.

Where the data comes from
-------------------------

The spine is MTC's replica, which is the only place that says which way along a
corridor a route runs:

    bus_route          route_id, route_direction (UP / DOWN)
    bus_route_point    one row per fare stage, in route_order - not one per stop
    bus_stop           bus_stop_name - the stage's name

A stage's *stops* are not in the replica in a form we can use: its `bus_stop_id`s
are MTC integers with no bridge to the editor's stop ids. They are in our own
`gtfs_route_stop`, joined the way the replica's ids allow - on the route and the
stage's name:

    SELECT * FROM gtfs_route_stop
     WHERE route_id = <the replica's route_id> AND stage_name = <the bus_stop_name>

which holds because the editor's `route_id` IS the replica's `route_id`. Checked
on chennai_bus: 5,487 of 5,567 routes have the same stages in the same order on
both sides.

Usage
-----

    # 1. the spine: export this query from Metabase as CSV
    python3 scripts/backfill_stages.py --print-replica-sql

    # 2. dry run - writes nothing, reports everything
    python3 scripts/backfill_stages.py --db <editor> --replica-csv spine.csv

    # 3. write it
    python3 scripts/backfill_stages.py --db <editor> --replica-csv spine.csv \
        --write --reset --actor you@nammayatri.in --batch "first run"

`--no-replica` takes the stage order and names from `gtfs_route_stop` alone and
leaves every direction NULL. It is for trying a run out where the replica is not
reachable; a real backfill wants the directions, or the two directions of one
corridor become one stage.

Standard library only, and shells out to `psql` - see scripts/requirements.txt.
"""
from __future__ import annotations

import argparse
import collections
import csv
import datetime
import io
import json
import os
import re
import subprocess
import sys
import tempfile

PROG = os.path.basename(__file__)

# The spine, as SQL against MTC's replica. Exported from Metabase as CSV it is
# the same five columns in the same order.
REPLICA_SQL = """
SELECT rp.route_id,
       r.route_direction,
       rp.route_order,
       rp.stage_no,
       s.bus_stop_name AS stage_name,
       s.bus_stop_id
  FROM bus_route r
  JOIN bus_route_point rp ON rp.route_id = r.route_id AND NOT rp.deleted
  JOIN bus_stop s ON s.bus_stop_id = rp.bus_stop_id
 WHERE NOT r.deleted
 ORDER BY rp.route_id, rp.route_order
""".strip()


# What a stage's stops are. Everything else on a `gtfs_route_stop` row belongs
# to the route, not to the stage.
STOP_FIELDS = (
    "stop_id",
    "stop_type",
    "stop_name_override",
    "marker_id",
    "marker_name",
    "marker_lat",
    "marker_lon",
)
# The per-row GTFS fields a stage cannot carry. A route that uses any of them
# cannot be rewritten from its stages, so the run refuses rather than lose them.
UNREPRESENTABLE = (
    "pickup_type",
    "drop_off_type",
    "timepoint",
    "stop_sequence",
    "continuous_pickup",
    "continuous_drop_off",
    "shape_dist_traveled",
    "pickup_booking_rule_id",
    "drop_off_booking_rule_id",
    "stop_headsign",
)
ROUTE_STOP_COLS = (
    "gtfs_id",
    "route_id",
    "pattern_key",
    "sequence",
    "stop_id",
    "stop_type",
    "stage_no",
    "stage_name",
    "stop_name_override",
    "marker_id",
    "marker_name",
    "marker_lat",
    "marker_lon",
    "provider_id",
    "provenance",
    "updated_by",
)

# Why a name is in the review queue, worst first. Same vocabulary as
# `gtfs_stage.review`; see the column's comment in db/gtfs_editor/0025.
# Why a stage is in the queue. The first three are a disagreement somebody has
# to settle; `agreed` is a stage every route gave the same stops, raised so the
# operations team can look over what was mapped and say it is right.
REVIEW_REASONS = ("head_differs", "head_duplicate_stops", "stretch_differs", "agreed")
WHAT_TO_DO = {
    "head_differs": "The routes begin it at stops with different names: the name covers more than one place",
    "head_duplicate_stops": "They begin it at different stop records that carry one name: merge the stops",
    "stretch_differs": "They agree where it begins and differ on the stops after it",
    "agreed": "Every route gives it the same stops; check the mapping is right",
}
# Routes MTC does not run as ordinary services - premium and shuttle - are not
# in the replica's `bus_route` at all, so there is no fare-stage spine for them
# and nothing to build stages from. They are left exactly as they are: no
# stages, no route_stage rows, and their gtfs_route_stop untouched.
DEFAULT_SKIP_FILE = "assets/route_service_tiers.csv"


def read_skips(path, gtfs):
    """route_id -> service tier, for the routes of this feed to leave alone."""
    out = {}
    try:
        fh = open(path, newline="", encoding="utf-8-sig")
    except FileNotFoundError:
        return out
    with fh:
        for row in csv.DictReader(fh):
            if (row.get("gtfs_id") or "").strip() == gtfs:
                out[num(row.get("route_id"))] = (row.get("servicetier") or "").strip()
    return out


# A review that changes fewer stop calls than this is small: still raised, still
# in the queue, but behind the ones that matter. On chennai_bus 776 of 2,034
# reviews are at or above it and they hold 92% of the difference.
WORTH_A_LOOK = 20

# At most this many of a name's lists go into the review's evidence; a name the
# routes give forty lists for does not need forty in front of a person.
MAX_CANDIDATES = 12


def log(msg=""):
    print(msg, file=sys.stderr, flush=True)


def die(msg):
    log(f"{PROG}: {msg}")
    sys.exit(1)


# ------------------------------------------------------------------ postgres


def normalise(url: str) -> str:
    """GIMS configs use the `psql://` scheme; libpq only knows postgres[ql]://."""
    if url.startswith("psql://"):
        url = "postgresql://" + url[len("psql://") :]
    if not url.startswith(("postgres://", "postgresql://")):
        die(f"not a postgres url: {redact(url)}")
    authority = url.split("://", 1)[1].split("/", 1)[0]
    if authority.count("@") > 1:
        die(
            f"url {redact(url)} has an un-encoded '@' in the password; "
            "percent-encode it (@ -> %40, $ -> %24, ...)"
        )
    return url


def redact(url: str) -> str:
    return re.sub(r"://([^:/@]+):[^@]*@", r"://\1:***@", url)


def psql(url: str, args: "list[str]", stdin: "str | None" = None) -> str:
    env = dict(os.environ)
    env.setdefault("PGCONNECT_TIMEOUT", "15")
    cmd = ["psql", "--no-psqlrc", "-v", "ON_ERROR_STOP=1"] + args + [url]
    try:
        proc = subprocess.run(
            cmd, input=stdin, capture_output=True, text=True, env=env, check=False
        )
    except FileNotFoundError:
        die("psql not found on PATH (macOS: brew install libpq)")
    if proc.returncode != 0:
        die(f"psql failed for {redact(url)}:\n{proc.stderr.strip()}")
    return proc.stdout


# A tab, a newline or a backslash inside a stop name would break COPY's text
# format, and '\N' is how it spells NULL, so CSV is used for reading. A real
# stop name holding the two characters '\N' would be read back as NULL; none
# does, and the run reports any it sees.
NULL = "\\N"


def query(url: str, sql: str) -> "list[list]":
    out = psql(url, ["-c", f"COPY ({sql}) TO STDOUT WITH (FORMAT csv, NULL '{NULL}')"])
    rows = []
    for row in csv.reader(io.StringIO(out)):
        rows.append([None if v == NULL else v for v in row])
    return rows


def scalar(url: str, sql: str):
    rows = query(url, sql)
    return rows[0][0] if rows else None


def lit(s: str) -> str:
    """A string as a SQL literal."""
    return "'" + s.replace("'", "''") + "'"


def copy_text(v) -> str:
    """One value in COPY's text format."""
    if v is None:
        return NULL
    s = str(v)
    return (
        s.replace("\\", "\\\\")
        .replace("\t", "\\t")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
    )


# --------------------------------------------------------------------- model
class Row:
    """One `gtfs_route_stop` row."""

    __slots__ = (
        "sequence",
        "stop_id",
        "stop_type",
        "stage_no",
        "stage_name",
        "stop_name_override",
        "marker_id",
        "marker_name",
        "marker_lat",
        "marker_lon",
        "provider_id",
        "provenance",
    )

    def __init__(self, *values):
        for name, v in zip(self.__slots__, values):
            setattr(self, name, v)
        self.sequence = int(self.sequence)
        self.stage_no = int(self.stage_no)

    def stops(self) -> tuple:
        """What the stage keeps."""
        return tuple(getattr(self, f) for f in STOP_FIELDS)

    def whole(self) -> tuple:
        """What `same_rows` compares: everything but the provider id."""
        return (self.stage_no, self.stage_name) + self.stops()


class Run:
    """A stretch of a route's rows that is one fare stage."""

    __slots__ = ("route_id", "position", "stage_no", "name", "rows", "key", "stage")

    def __init__(self, route_id, position, rows):
        self.route_id = route_id
        self.position = position
        self.rows = rows
        self.stage_no = None
        self.name = None
        self.key = None
        self.stage = None

    def settle(self):
        self.stage_no = self.rows[0].stage_no
        # exactly as the first row spells it, less the spaces around it
        self.name = (self.rows[0].stage_name or "").strip()

    def stops(self) -> tuple:
        return tuple(r.stops() for r in self.rows)

    def places(self) -> tuple:
        """The run as a sequence of places, for asking whether one list of
        stops is another with stops missing. A stop is its id; a marker is
        where it is, since a marker has no id of its own to match on."""
        return tuple(
            r.stop_id
            if r.stop_id
            else ("~marker", r.marker_name, r.marker_lat, r.marker_lon)
            for r in self.rows
        )


class Stage:
    __slots__ = ("stage_id", "name", "direction", "stops", "runs", "spellings",
                 "review", "evidence")

    def __init__(self, stage_id, name, direction, stops):
        self.stage_id = stage_id
        self.name = name
        self.direction = direction
        self.stops = stops
        self.runs = 0
        self.spellings = collections.Counter()
        # Why a person has to look at this stage, and what to put in front of them.
        self.review = None
        self.evidence = None


def canon(name: str) -> str:
    """A stage name as it is compared: the string itself, with only the spaces
    around it taken off.

    Nothing else is folded. CONCORD and concord are two names, and so are
    CIT NAGAR and CIT  NAGAR, because a stage is whatever MTC's data says it is
    and it is not this script's place to decide two spellings mean one place.
    Where they really are one, a person merges them in the dashboard and that
    decision is recorded; guessing here would make the same decision silently,
    for every name, with nothing to review.
    """
    return (name or "").strip()


def route_sort(route_id: str):
    return (0, int(route_id)) if route_id.isdigit() else (1, route_id)


# ---------------------------------------------------------------- reading it
def read_internal(url: str, gtfs: str):
    """Every route's rows, in sequence order: route_id -> [Row]."""
    where = f"gtfs_id = {lit(gtfs)} AND pattern_key = 1"
    rows = query(
        url,
        "SELECT route_id, sequence, stop_id, stop_type, stage_no, stage_name,"
        " stop_name_override, marker_id, marker_name, marker_lat, marker_lon,"
        " provider_id, provenance::text"
        f" FROM gtfs_route_stop WHERE {where} ORDER BY route_id, sequence",
    )
    routes: "dict[str, list[Row]]" = collections.defaultdict(list)
    for r in rows:
        routes[r[0]].append(Row(*r[1:]))
    return routes


def cut_runs(routes, problems):
    """Cut each route's rows into its fare stages.

    A fare stage is what `stage_no` says it is, so the cut follows that alone: a
    run ends where the number changes. The name is NOT part of the cut. Names are
    compared exactly everywhere else, and 30 routes spell one fare stage's name
    two ways part way through it (`pallavaram` then `PALLAVARAM`, route 1012
    stage 11). Cutting on the name would split those into two stages, which is
    not what MTC says and, worse, changes the route's stage count so it stops
    lining up with the replica - 24 routes lost every MTC id that way.

    It is safe because a stage number never carries two genuinely different
    names: the only four places where one does are all `m.m.d.a.colony rdjn`
    against `m.m.d.a.colony rd.jn`, a missing full stop.

    Cutting at every NEW STOP instead, which is what `ensure_normal_list` does,
    is not the same thing: a fare stage can begin at a JUMP STOP (route 10's
    stage 8 does), and cutting only at NEW STOP swallows it into the stage
    before. Those are counted below, since that is a route the editor itself
    would cut differently.
    """
    out = {}
    for route_id, rows in routes.items():
        runs, last = [], None
        for r in rows:
            here = r.stage_no
            if here != last:
                runs.append(Run(route_id, len(runs) + 1, []))
                last = here
            runs[-1].rows.append(r)
        for run in runs:
            run.settle()
            if run.rows[0].stop_type != "NEW STOP":
                problems[
                    f"a fare stage starting at a {run.rows[0].stop_type},"
                    " which ensure_normal_list would not cut at"
                ].append(f"route {route_id} stage {run.stage_no} {run.name}")
            spellings = {r.stage_name for r in run.rows}
            if len(spellings) > 1:
                problems["a fare stage whose rows spell its name more than one way"].append(
                    f"route {route_id} stage {run.stage_no}: "
                    + " / ".join(sorted(spellings))
                )
            if NULL in (run.name or ""):
                problems["a stage name holding the two characters \\N"].append(
                    f"route {route_id} {run.name}"
                )
        for name, n in collections.Counter(canon(run.name) for run in runs).items():
            if n > 1:
                problems["the same stage name more than once in one route"].append(
                    f"route {route_id} {name} x{n}"
                )
        out[route_id] = runs
    return out


def read_spine_db(url: str) -> "list[list]":
    return query(url, REPLICA_SQL)


def read_spine_csv(path: str) -> "list[list]":
    with open(path, newline="", encoding="utf-8-sig") as fh:
        rows = list(csv.reader(fh))
    if not rows:
        die(f"{path}: empty")
    head = [c.strip().lower() for c in rows[0]]
    want = ["route_id", "route_direction", "route_order", "stage_no", "stage_name",
            "bus_stop_id"]
    if head[:6] != want:
        die(
            f"{path}: first line must be the header {','.join(want)}, "
            f"got {','.join(head[:6])}\n"
            "export exactly the query printed by --print-replica-sql"
        )
    return [[None if v == "" else v for v in r[:6]] for r in rows[1:]]


def num(v) -> str:
    """A number as the editor spells it. Metabase's CSV export can write 1,234
    for 1234 and 1234.0 for an integer column; a route id that came through
    either way would match no route at all, in silence."""
    s = str(v or "").strip().replace(",", "").replace(" ", "")
    if s.endswith(".0"):
        s = s[:-2]
    return s


def spine_from_rows(rows):
    """route_id -> (direction, [(stage name, MTC's bus_stop_id) in order]).

    MTC's `bus_stop_id` is what identifies a fare stage: no id carries two
    names, while 160 names are carried by more than one id (ADYAR B.T is three
    different stops). Keying on the id rather than the name tells those apart
    the way MTC already does.
    """
    heads = collections.defaultdict(list)
    direction = {}
    for route_id, route_direction, route_order, _stage_no, stage_name, bus_stop_id in rows:
        rid = num(route_id)
        d = (route_direction or "").strip().lower()
        direction[rid] = d if d in ("up", "down") else None
        heads[rid].append((int(num(route_order) or 0), stage_name, num(bus_stop_id)))
    out = {}
    for rid, v in heads.items():
        seq = [(n, i) for _, n, i in sorted(v, key=lambda x: x[0])]
        # The replica can carry two points at one fare boundary - a route that
        # turns there, or an arrival and a departure - and lists the stage head
        # twice running. That is one stage, as our own rows have it. A head that
        # comes back later in the route (M.G.R.CENTRAL > CONCORD >
        # M.G.R.CENTRAL) is a second stage and is kept.
        folded = [x for i, x in enumerate(seq) if i == 0 or x[1] != seq[i - 1][1]]
        out[rid] = (direction[rid], folded)
    return out


# ---------------------------------------------------------------- choosing it


def review_reason(runs, stop_names):
    """Why a person has to look at this stage name, or None.

    The routes sharing a name do not always agree about it, and the kind of
    disagreement says what settles it.
    """
    lists = {r.stops() for r in runs}
    if len(lists) == 1:
        # One stop list, however the routes spell the name: the backfill writes
        # one spelling everywhere, so nothing is left for a person to decide.
        return None
    heads = {r.places()[0] for r in runs}
    if len(heads) > 1:
        called = {canon(stop_names.get(h, "")) for h in heads if isinstance(h, str)}
        # the same name on several stop records is a stop problem, not a stage one
        return "head_duplicate_stops" if len(called) == 1 else "head_differs"
    return "stretch_differs"


def stage_key_of(run, head, direction):
    """What identifies the stage this run belongs to, and what to call it.

    MTC's `bus_stop_id` for the fare-stage head, when the replica gave us one:
    it is MTC's own identity for the stage, no id carries two names, and it
    tells apart the 160 names that are carried by more than one stop. Direction
    rides in the key because 1,746 of the 1,836 ids are used both ways and the
    stops after the boundary differ.

    Where the replica did not line up with our rows, the stage falls back to its
    name, **exactly as written**: no folding of case or spacing, so SAIDAPET and
    saidapet stay two stages.
    """
    if head:
        name, bus_stop_id = head
        return ("mtc", bus_stop_id, direction), name
    return ("name", run.name, direction), run.name


def stage_id_of(key):
    """The stage's id: MTC's `bus_stop_id` for the fare-stage head, as it stands.

    It is not unique on its own - 1,746 of MTC's 1,836 fare-stage stops are used
    both ways - which is why direction is the other half of the stage's key
    (gtfs_stage's primary key is gtfs_id, stage_id, direction). Nothing is
    minted: a rerun over the same data writes the same ids.

    A fare stage the replica did not line up with falls back to its name, since
    there is no stop id to use.
    """
    kind, ident, _direction = key
    if kind == "mtc":
        return ident
    slug = re.sub(r"[^A-Za-z0-9]+", "_", ident).strip("_")[:48] or "unnamed"
    return f"nm_{slug}"


def pair_with_spine(runs, heads, problems, route_id):
    """Which fare stage of the replica each of our runs is, or None.

    Paired by position when the two lists are the same length, which they are
    for 5,487 of the 5,567 routes. Where they are not, our runs keep their own
    names and the replica is not guessed at.
    """
    if heads and len(heads) == len(runs):
        return list(heads)
    if heads:
        problems["a route the replica counts a different number of stages for"].append(
            f"route {route_id}: ours {len(runs)}, replica {len(heads)}"
        )
    return [None] * len(runs)


def choose_stages(runs_by_route, spine, pick, stop_names):
    """One stage per fare stage of the replica (its head stop and the route's
    direction), and the review rows for the ones the routes do not agree about.

    The stage's stops are the list the most routes give it (`--pick commonest`),
    or the one with the most stops (`--pick longest`). Ties go to the longer
    list, then to the lowest route id, so a rerun makes the same choice.
    """
    # Where each fare stage goes next, and where it came from. A stage the
    # routes disagree about usually disagrees because they part company after
    # the boundary, so what each candidate list runs on to is the thing that
    # tells a reviewer which one belongs to which road.
    around = {}
    for route_id, runs in runs_by_route.items():
        for i, run in enumerate(runs):
            around[(route_id, run.position)] = (
                runs[i - 1].name if i else None,
                runs[i + 1].name if i + 1 < len(runs) else None,
            )

    groups = collections.defaultdict(list)
    names_of = {}
    for route_id, runs in runs_by_route.items():
        direction, heads = spine.get(route_id, (None, None))
        paired = pair_with_spine(runs, heads, collections.defaultdict(list), route_id)
        for run, head in zip(runs, paired):
            run.key, shown = stage_key_of(run, head, direction)
            groups[run.key].append(run)
            names_of.setdefault(run.key, shown)

    stages, reviews = [], []
    for key, runs in sorted(groups.items(), key=lambda kv: (str(kv[0][1]), kv[0][2] or "")):
        lists = collections.defaultdict(list)
        for run in runs:
            lists[run.stops()].append(run)
        first = lambda rs: route_sort(min((r.route_id for r in rs), key=route_sort))
        rank = {
            "commonest": lambda kv: (-len(kv[1]), -len(kv[0]), first(kv[1])),
            "longest": lambda kv: (-len(kv[0]), -len(kv[1]), first(kv[1])),
        }[pick]
        ranked = sorted(lists.items(), key=rank)
        stops, winners = ranked[0]

        stage = Stage(stage_id_of(key), names_of[key], key[2], stops)
        stage.runs = len(runs)
        for run in runs:
            run.stage = stage
            stage.spellings[run.name] += 1
        stages.append(stage)

        reason = review_reason(runs, stop_names) or "agreed"
        # How much this guess actually costs: for every route that does not have
        # the chosen list, the stops it gains and the stops it loses. A name 300
        # routes share, where the pick is wrong for all of them, matters; one
        # where two routes differ by a stop does not. The queue is worked in this
        # order, and most of the damage is in a few hundred names.
        impact = 0
        for v, rs2 in lists.items():
            gained = collections.Counter(x[0] for x in stops)
            held = collections.Counter(x[0] for x in v)
            impact += (sum((gained - held).values()) + sum((held - gained).values())) * len(rs2)

        heads_seen = []
        for run in runs:
            place = run.places()[0]
            if isinstance(place, str) and place not in heads_seen:
                heads_seen.append(place)
        def going(rs, which):
            """What the routes giving this list run on to (or came from), most
            of them first."""
            seen = collections.Counter(
                around.get((r.route_id, r.position), (None, None))[which] for r in rs
            )
            return [
                {"name": nm, "routes": n}
                for nm, n in seen.most_common(4)
                if nm
            ]

        candidates = [
            {
                "stops": [x[0] for x in v],
                # A map point has no stop id, so it has no name in `stop_names`;
                # without its own name it reads as a bare "map point" and says
                # nothing about where the route is being shaped.
                "stop_names": [
                    stop_names.get(x[0])
                    or (f"map point {x[4]}" if x[4] else (x[0] or "map point"))
                    for x in v
                ],
                # every route, not a sample: the reviewer can split a list off
                # onto a stage of its own, and that moves exactly these routes
                "routes": sorted({r.route_id for r in rs}, key=route_sort),
                "route_count": len({r.route_id for r in rs}),
                "chosen": v == stops,
                "comes_from": going(rs, 0),
                "goes_to": going(rs, 1),
            }
            for v, rs in ranked[:MAX_CANDIDATES]
        ]
        # `agreed` is not a problem with the stage, so it carries no flag: the
        # chip and the ?review= filter stay for the ones that need settling.
        if reason != "agreed":
            stage.review = reason
        stage.evidence = {
            "impact": impact,
            "candidates": candidates,
            "lists": len(ranked),
            "heads": heads_seen,
            "head_names": sorted({canon(stop_names.get(h, "")) for h in heads_seen} - {""}),
            "spellings": sorted({r.name for r in runs}),
            "routes": sorted({r.route_id for r in runs}, key=route_sort)[:20],
            "route_count": len({r.route_id for r in runs}),
        }
        reviews.append(
            {
                "name": stage.name,
                "name_key": stage.stage_id,
                "direction": key[2],
                "reason": reason,
                "impact": impact,
                "stage_id": stage.stage_id,
                "evidence": stage.evidence,
                "route_count": stage.evidence["route_count"],
                "lists": len(ranked),
            }
        )
    return stages, reviews


def flatten(runs):
    """What a route's stops become, in the shape `same_rows` compares."""
    return [
        (run.stage_no, run.stage.name) + stops
        for run in runs
        for stops in run.stage.stops
    ]


def route_issues(runs_by_route, spine, short_names):
    """The routes MTC's own route definition does not line up with.

    A route is raised only when the two lists are a different length, or the
    replica does not carry it at all. That is exactly the set whose stages could
    not be given an MTC id, and it is what a person has to settle: both sides are
    complete and they disagree, so only somebody who knows the route can say
    which is right.

    Names that differ down an otherwise matching list are NOT raised. They pair
    correctly by position and the difference is almost always spelling -
    GURUNANAK COLLEGE against GURU NANAK COLLEGE, GOVT. against GOVERNMENT - on
    1,594 of chennai_bus's routes. Raising those would bury the 76 that matter.
    """
    out = []
    for route_id, runs in sorted(runs_by_route.items(), key=lambda kv: route_sort(kv[0])):
        direction, heads = spine.get(route_id, (None, None))
        if heads is None:
            issue = "absent"
        elif len(heads) != len(runs):
            issue = "count_differs"
        else:
            continue
        unkeyed = sum(
            1 for r in runs if r.stage is not None and r.stage.stage_id.startswith("nm_")
        )
        out.append(
            {
                "route_id": route_id,
                "short_name": short_names.get(route_id),
                "issue": issue,
                "ours": [{"stage_no": r.stage_no, "name": r.name} for r in runs],
                "theirs": [{"name": n, "bus_stop_id": i} for n, i in (heads or [])],
                "stages_unkeyed": unkeyed,
            }
        )
    return out


# ----------------------------------------------------------------- writing it


def refresh_evidence(url, gtfs, reviews, issues, args):
    """Rewrite the evidence of review rows already written, and nothing else.

    A run rewrites gtfs_route_stop, so the lists the routes gave a stage cannot
    be worked out again from the feed afterwards - they live only in the review
    rows. When something is missing from that evidence (it once held a sample of
    a list's routes rather than all of them), the way back is to read the feed as
    it was, choose the stages again, and write the new evidence over the old.

    Every stage is chosen exactly as the run chose it, so `reason` and `impact`
    come out the same; only `evidence` is written, and a row somebody has closed
    keeps its status, its note and its draft.
    """
    g = lit(gtfs)
    have = {
        (name_key, direction): review_id
        for review_id, name_key, direction in query(
            url,
            "SELECT review_id, name_key, direction FROM gtfs_stage_review"
            f" WHERE gtfs_id = {g}",
        )
    }
    want = {(r["name_key"], r["direction"] or "") for r in reviews}
    missing = want - set(have)
    extra = set(have) - want
    log(f"  {len(have)} review rows here, {len(want)} worked out again")
    if missing or extra:
        die(
            f"the review rows do not line up with this feed: {len(missing)} worked "
            f"out and not stored, {len(extra)} stored and not worked out. The "
            "database read with --refresh-evidence is not the one the run read; "
            "nothing was written."
        )
    rows = [
        (have[(r["name_key"], r["direction"] or "")],
         json.dumps(r["evidence"], sort_keys=True))
        for r in reviews
    ]
    out = [
        f"-- {PROG}: evidence for {len(rows)} review rows of {gtfs}",
        "CREATE TEMP TABLE new_evidence (review_id bigint, evidence jsonb)"
        " ON COMMIT DROP;",
        "COPY new_evidence (review_id, evidence) FROM stdin;",
    ]
    out += ["\t".join(copy_text(v) for v in row) for row in rows]
    out += [
        "\\.",
        "",
        "UPDATE gtfs_stage_review r SET evidence = n.evidence",
        f" FROM new_evidence n WHERE n.review_id = r.review_id AND r.gtfs_id = {g};",
        "",
        "-- The routes MTC does not line up with are worked out from the same",
        "-- reading, so they are written here too: a row nobody has closed is",
        "-- this pass's to replace, a closed one is the record of a decision.",
        f"UPDATE gtfs_route_stage_issue SET status = 'superseded'",
        f" WHERE gtfs_id = {g} AND status = 'pending';",
        "",
    ]
    batch = args.batch or f"evidence {datetime.datetime.now().astimezone().isoformat(timespec='seconds')}"
    out.append(
        "COPY gtfs_route_stage_issue (gtfs_id, batch, route_id, short_name, issue,"
        " ours, theirs, stages_unkeyed) FROM stdin;"
    )
    out += [
        "\t".join(
            copy_text(v)
            for v in (
                gtfs, batch, i["route_id"], i["short_name"], i["issue"],
                json.dumps(i["ours"]), json.dumps(i["theirs"]), i["stages_unkeyed"],
            )
        )
        for i in issues
    ]
    out += ["\\.", ""]
    sql = "\n".join(out)
    if not args.write:
        print()
        print(f"Dry run: {len(rows)} review rows would have their evidence rewritten, "
              f"and {len(issues)} route(s) raised as not lining up with MTC. "
              "Add --write to write it.")
        return 0
    fd, path = tempfile.mkstemp(prefix="refresh_evidence_", suffix=".sql")
    os.close(fd)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(sql)
    log(f"sql -> {path} ({len(sql)} bytes)")
    psql(url, ["--single-transaction", "-f", path])
    print()
    print(f"{len(rows)} review rows now carry the evidence this run worked out, "
          f"and {len(issues)} route(s) are raised as not lining up with MTC. "
          "No stage and no route link was touched.")
    return 0


def build_sql(gtfs, stages, runs_by_route, rewrite, live_rows, reviews, issues, args, batch):
    """The whole backfill as one script, to be run in one transaction."""
    g = lit(gtfs)
    out = [
        f"-- {PROG}: stages for {gtfs}",
        f"-- {len(stages)} stages, {sum(len(s.stops) for s in stages)} stage stops, "
        f"{sum(len(r) for r in runs_by_route.values())} route stages, "
        f"{len(rewrite)} routes rewritten, {len(reviews)} raised for review",
        "",
    ]
    if args.reset:
        out += [
            f"DELETE FROM gtfs_route_stage WHERE gtfs_id = {g};",
            f"DELETE FROM gtfs_stage_stop WHERE gtfs_id = {g};",
            f"DELETE FROM gtfs_stage WHERE gtfs_id = {g};",
            "-- a row nobody has closed is this run's to replace; a closed one is",
            "-- the record of a decision and stays",
            f"UPDATE gtfs_stage_review SET status = 'superseded'",
            f" WHERE gtfs_id = {g} AND status = 'pending';",
            f"UPDATE gtfs_route_stage_issue SET status = 'superseded'",
            f" WHERE gtfs_id = {g} AND status = 'pending';",
            "",
        ]

    def copy(table, cols, rows):
        out.append(f"COPY {table} ({', '.join(cols)}) FROM stdin;")
        for row in rows:
            out.append("\t".join(copy_text(v) for v in row))
        out.extend(["\\.", ""])

    copy(
        "gtfs_stage",
        ("gtfs_id", "stage_id", "direction", "name", "review", "provenance", "updated_by"),
        (
            (
                gtfs,
                st.stage_id,
                st.direction or "",
                st.name,
                st.review,
                json.dumps({"source": "backfill", "batch": batch}, sort_keys=True),
                args.actor,
            )
            for st in stages
        ),
    )
    copy(
        "gtfs_stage_stop",
        ("gtfs_id", "stage_id", "direction", "position") + STOP_FIELDS,
        (
            (gtfs, st.stage_id, st.direction or "", i) + stops
            for st in stages
            for i, stops in enumerate(st.stops, 1)
        ),
    )
    copy(
        "gtfs_route_stage",
        ("gtfs_id", "route_id", "position", "stage_id", "direction", "stage_no",
         "updated_by"),
        (
            (
                gtfs, route_id, run.position, run.stage.stage_id,
                run.stage.direction or "", run.stage_no, args.actor,
            )
            for route_id, runs in sorted(
                runs_by_route.items(), key=lambda kv: route_sort(kv[0])
            )
            for run in runs
        ),
    )
    copy(
        "gtfs_stage_review",
        ("gtfs_id", "batch", "name", "name_key", "direction", "reason", "impact",
         "evidence"),
        (
            (
                gtfs,
                batch,
                r["name"],
                r["name_key"],
                r["direction"] or "",
                r["reason"],
                r["impact"],
                json.dumps(r["evidence"], sort_keys=True),
            )
            for r in reviews
        ),
    )

    copy(
        "gtfs_route_stage_issue",
        ("gtfs_id", "batch", "route_id", "short_name", "issue", "ours", "theirs",
         "stages_unkeyed"),
        (
            (
                gtfs, batch, i["route_id"], i["short_name"], i["issue"],
                json.dumps(i["ours"]), json.dumps(i["theirs"]), i["stages_unkeyed"],
            )
            for i in issues
        ),
    )


    if rewrite:
        ids = ", ".join(lit(r) for r in sorted(rewrite, key=route_sort))
        out += [
            "-- Routes whose stages say something other than what is there now.",
            "-- Left as they are, every one of their stages would be uneditable:",
            "-- a stage edit refuses a route that does not match its stages.",
            f"DELETE FROM gtfs_route_stop WHERE gtfs_id = {g} AND pattern_key = 1"
            f" AND route_id IN ({ids});",
            "",
        ]
        rows = []
        for route_id in sorted(rewrite, key=route_sort):
            live = live_rows[route_id]
            # A rewritten route keeps its usual provider id and, where the same
            # stop or marker stays at the same place, its per-row provenance -
            # the same two rules `write_pattern_rows` follows.
            counts = collections.Counter(r.provider_id for r in live if r.provider_id)
            provider = min(counts.items(), key=lambda kv: (-kv[1], kv[0]), default=(None,))[0]
            was = {r.sequence: r for r in live}
            for i, row in enumerate(flatten(runs_by_route[route_id]), 1):
                stage_no, stage_name = row[0], row[1]
                stop_id, stop_type, override, mid, mname, mlat, mlon = row[2:]
                before = was.get(i)
                provenance = (
                    before.provenance
                    if before and before.stop_id == stop_id and before.marker_id == mid
                    else None
                )
                rows.append((
                    gtfs, route_id, 1, i, stop_id, stop_type, stage_no, stage_name,
                    override, mid, mname, mlat, mlon, provider, provenance, args.actor,
                ))
        copy("gtfs_route_stop", ROUTE_STOP_COLS, rows)

    detail = {
        "stages": len(stages),
        "stage_stops": sum(len(s.stops) for s in stages),
        "route_stages": sum(len(r) for r in runs_by_route.values()),
        "routes": len(runs_by_route),
        "routes_rewritten": len(rewrite),
        "reviews": len(reviews),
        "pick": args.pick,
        "batch": batch,
    }
    out += [
        "INSERT INTO gtfs_audit_log (actor_email, action, gtfs_id, detail)",
        f"VALUES ({lit(args.actor)}, 'stages_backfilled', {g}, "
        + lit(json.dumps(detail, sort_keys=True))
        + "::jsonb);",
        "",
    ]
    return "\n".join(out)


# --------------------------------------------------------------------- checks


def refuse_if_busy(url, gtfs, args, writing):
    """What would stop a write. A dry run reports the same things and carries on:
    it writes nothing, so nothing it finds can do any harm."""
    g = lit(gtfs)

    def stop(msg):
        die(msg) if writing else log(f"note: {msg}")

    if not args.reset:
        n = scalar(url, f"SELECT count(*) FROM gtfs_stage WHERE gtfs_id = {g}")
        if int(n):
            stop(
                f"{gtfs} already has {n} stages. Pass --reset to replace every "
                "stage of this feed, or pick a feed that has none."
            )
    # A run rewrites gtfs_route_stop, so a second run reads its own output: the
    # routes now agree about every stage, nothing is raised for review, and the
    # numbers look wonderful and mean nothing. --reset does not help, because it
    # drops the stages and leaves the rewritten rows. The only honest rerun is
    # from the feed as it was.
    ran = scalar(
        url,
        "SELECT count(*) FROM gtfs_audit_log"
        f" WHERE gtfs_id = {g} AND action = 'stages_backfilled'",
    )
    if int(ran):
        when = scalar(
            url,
            "SELECT max(at)::text FROM gtfs_audit_log"
            f" WHERE gtfs_id = {g} AND action = 'stages_backfilled'",
        )
        if args.rerun:
            log(
                f"  {gtfs} was backfilled before ({ran} run(s), the last at {when}). "
                "--rerun says its rows have been put back, so this is a first run "
                "over MTC's own data again."
            )
        else:
            stop(
                f"{gtfs} was already backfilled ({ran} run(s), the last at {when}), so "
                "its stop lists are this script's own output. Running again reads that "
                "back and raises nothing for review. Restore the feed as it was before "
                "the first run, then pass --rerun.\n"
                "The audit row cannot be deleted - gtfs_audit_log is append-only - so "
                "--rerun is how a restored feed is run again."
            )
    diverted = query(
        url,
        "SELECT route_id FROM gtfs_route"
        f" WHERE gtfs_id = {g} AND active_variant_id IS NOT NULL ORDER BY route_id",
    )
    if diverted:
        stop(
            f"{len(diverted)} route(s) are running a temporary route "
            f"({', '.join(r[0] for r in diverted[:10])}). Put them back on their "
            "normal route first; a backfill cannot tell which list is the normal one."
        )
    open_changes = query(
        url,
        "SELECT cs.change_set_id::text, ch.entity, count(*)"
        " FROM gtfs_change ch"
        " JOIN gtfs_change_set cs ON cs.change_set_id = ch.change_set_id"
        f" WHERE cs.gtfs_id = {g} AND cs.status NOT IN ('committed', 'discarded')"
        "   AND ch.entity IN ('route_stops', 'stage', 'route_stages', 'route_variant')"
        " GROUP BY 1, 2 ORDER BY 1, 2",
    )
    if open_changes and not args.force:
        for cs, entity, n in open_changes:
            log(f"  open change set {cs}: {entity} x{n}")
        stop(
            "a route_stops change cannot be submitted once its route has stages "
            "(route_has_stages). Commit or discard these first, or pass --force."
        )
    bad = query(
        url,
        "SELECT route_id, count(*) FROM gtfs_route_stop"
        f" WHERE gtfs_id = {g} AND ("
        + " OR ".join(f"{c} IS NOT NULL" for c in UNREPRESENTABLE)
        + ") GROUP BY 1 ORDER BY 2 DESC",
    )
    if bad:
        for route_id, n in bad[:10]:
            log(f"  route {route_id}: {n} row(s)")
        stop(
            f"{len(bad)} route(s) carry per-row GTFS fields a stage cannot hold "
            f"({', '.join(UNREPRESENTABLE)}); rewriting them would drop those"
        )


# ----------------------------------------------------------------------- main


def self_test():
    """The choosing is the whole of it, so it is checked before a run that
    cannot be undone."""
    NAMES = {f"s{c}": f"Stop {c}" for c in "abcdefghijXYZ"}
    NAMES["sZ"] = "Stop a"

    def rows(route_id, names, stage_name="A1"):
        return [
            Row(i, f"s{n}", "NEW STOP" if i == 1 else "INTERMEDIATE STOP", 1,
                stage_name, None, None, None, None, None, "75", None)
            for i, n in enumerate(names, 1)
        ]

    def run(pick, *routes, spine=None):
        by_route = {
            rid: cut_runs({rid: rows(rid, *rest)}, collections.defaultdict(list))[rid]
            for rid, *rest in routes
        }
        stages, reviews = choose_stages(by_route, spine or {}, pick, NAMES)
        return by_route, stages, {r["reason"] for r in reviews}

    # One stage per name, whatever the routes say.
    by, stages, _ = run("commonest", ("1", "abc"), ("2", "ab"), ("3", "xyz"))
    assert len(stages) == 1, f"one name, one stage: {len(stages)}"

    # The commonest list wins; the longest does under --pick longest.
    by, stages, _ = run("commonest", ("1", "ab"), ("2", "ab"), ("3", "abcd"))
    assert len(stages[0].stops) == 2, "the list two of the three routes give"
    by, stages, _ = run("longest", ("1", "ab"), ("2", "ab"), ("3", "abcd"))
    assert len(stages[0].stops) == 4, "--pick longest takes the longest"
    # A tie on count goes to the longer list, so nothing is lost for nothing.
    by, stages, _ = run("commonest", ("1", "ab"), ("2", "abcd"))
    assert len(stages[0].stops) == 4, "a tie goes to the longer list"

    # Every route of the name gets that one list.
    by, stages, _ = run("commonest", ("1", "ab"), ("2", "ab"), ("3", "abcd"))
    assert [len(flatten(by[r])) for r in ("1", "2", "3")] == [2, 2, 2]

    # Why it is raised.
    # a stage every route agrees about is still raised, to be looked over
    assert run("commonest", ("1", "abc"), ("2", "abc"))[2] == {"agreed"}
    assert run("commonest", ("1", "abc"), ("2", "abd"))[2] == {"stretch_differs"}
    assert run("commonest", ("1", "abc"), ("2", "xbc"))[2] == {"head_differs"}
    assert run("commonest", ("1", "abc"), ("2", "Zbc"))[2] == {"head_duplicate_stops"}, \
        "one name on two stop records is a stop problem"
    # one stop list, two spellings: the backfill writes one spelling everywhere,
    # so there is nothing for a person to settle
    assert run("commonest", ("1", "abc", "A1"), ("2", "abc", "a1"))[2] == {"agreed"}

    # How wrong the guess is, in stop calls, so the queue can be worked by weight:
    # two routes have "ab", one has "abcd", so the one route gains two stops.
    by_route = {
        rid: cut_runs({rid: rows(rid, names)}, collections.defaultdict(list))[rid]
        for rid, names in (("1", "ab"), ("2", "ab"), ("3", "abcd"))
    }
    _, rev = choose_stages(by_route, {}, "commonest", NAMES)
    assert rev[0]["impact"] == 2, rev[0]["impact"]

    # Every list the routes give is put in front of the person, the chosen one
    # marked, so they can build the real one from them.
    by_route = {
        rid: cut_runs({rid: rows(rid, names)}, collections.defaultdict(list))[rid]
        for rid, names in (("1", "ab"), ("2", "ab"), ("3", "abcd"))
    }
    _, reviews = choose_stages(by_route, {}, "commonest", NAMES)
    cands = reviews[0]["evidence"]["candidates"]
    assert len(cands) == 2, cands
    assert cands[0]["chosen"] and cands[0]["route_count"] == 2
    assert not cands[1]["chosen"] and cands[1]["stop_names"] == [
        "Stop a", "Stop b", "Stop c", "Stop d"]

    # Case-sensitive: the names are taken exactly as the routes write them, so
    # two spellings are two stages. (With a spine the id keys it instead, and
    # the question does not arise.)
    by_route = {}
    for rid, names, spelt_as in (("1", "abc", "SAIDAPET"), ("2", "abc", "saidapet")):
        by_route[rid] = cut_runs({rid: rows(rid, names, spelt_as)},
                                 collections.defaultdict(list))[rid]
    stages, _ = choose_stages(by_route, {}, "commonest", NAMES)
    assert sorted(st.name for st in stages) == ["SAIDAPET", "saidapet"], \
        f"names are not folded, got {sorted(st.name for st in stages)}"
    assert all(st.stage_id.startswith("nm_") for st in stages), \
        "with no spine a stage falls back to its own name"

    # With the replica's stop id, the stage is MTC's fare stage: the id keys it
    # and the id names it, for every route that says the same name MTC does.
    by_route = {}
    for rid, names, spelt_as in (("1", "abc", "SAIDAPET"), ("2", "abc", "saidapet")):
        by_route[rid] = cut_runs({rid: rows(rid, names, spelt_as)},
                                 collections.defaultdict(list))[rid]
    spine = {"1": ("up", [("SAIDAPET", "912")]), "2": ("up", [("SAIDAPET", "912")])}
    stages, _ = choose_stages(by_route, spine, "commonest", NAMES)
    assert len(stages) == 1, f"one MTC stop, one stage: {len(stages)}"
    assert stages[0].stage_id == "912", stages[0].stage_id
    assert stages[0].direction == "up", stages[0].direction
    assert stages[0].name == "SAIDAPET", stages[0].name

    # One stop id used both ways is two stages: the stops after the boundary differ.
    by_route = {
        "1": cut_runs({"1": rows("1", "abc")}, collections.defaultdict(list))["1"],
        "2": cut_runs({"2": rows("2", "abc")}, collections.defaultdict(list))["2"],
    }
    spine = {"1": ("up", [("SAIDAPET", "912")]), "2": ("down", [("SAIDAPET", "912")])}
    stages, _ = choose_stages(by_route, spine, "commonest", NAMES)
    # the same MTC stop both ways: one id, two stages, told apart by direction
    assert [st.stage_id for st in stages] == ["912", "912"], \
        [st.stage_id for st in stages]
    assert sorted(st.direction for st in stages) == ["down", "up"], \
        sorted(st.direction for st in stages)

    # Two stops MTC tells apart stay apart, however they are spelled.
    by_route = {
        "1": cut_runs({"1": rows("1", "abc")}, collections.defaultdict(list))["1"],
        "2": cut_runs({"2": rows("2", "xyz")}, collections.defaultdict(list))["2"],
    }
    spine = {"1": ("up", [("ADYAR B.T", "4")]), "2": ("up", [("ADYAR B.T", "6")])}
    stages, reviews = choose_stages(by_route, spine, "commonest", NAMES)
    assert len(stages) == 2, "MTC already tells these apart"
    assert {r["reason"] for r in reviews} == {"agreed"}, \
        "nothing to settle, but both are there to be looked over"

    # A fare stage is cut where stage_no or the name changes, JUMP STOP or not.
    r = rows("9", "abcd")
    r[2].stage_no, r[2].stage_name, r[2].stop_type = 2, "A2", "JUMP STOP"
    r[3].stage_no, r[3].stage_name = 2, "A2"
    cut = cut_runs({"9": r}, collections.defaultdict(list))["9"]
    assert [len(x.rows) for x in cut] == [2, 2] and [x.name for x in cut] == ["A1", "A2"]

    print("self-test: ok")


def main():
    p = argparse.ArgumentParser(
        prog=PROG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
        description=__doc__,
    )
    p.add_argument("--db", help="the editor database (postgres:// or psql://)")
    p.add_argument("--gtfs-id", default="chennai_bus")
    p.add_argument(
        "--skip-routes",
        metavar="PATH",
        default=DEFAULT_SKIP_FILE,
        help="a CSV of gtfs_id,route_id,servicetier whose routes are left alone "
        f"(default: {DEFAULT_SKIP_FILE}; premium and shuttle services are not in "
        "the replica, so there is no spine to build their stages from)",
    )
    spine = p.add_mutually_exclusive_group()
    spine.add_argument("--replica-db", help="MTC's replica, read live")
    spine.add_argument("--replica-csv", help="the same query exported as CSV")
    spine.add_argument(
        "--no-replica",
        action="store_true",
        help="take the order and names from gtfs_route_stop; every direction NULL",
    )
    p.add_argument("--print-replica-sql", action="store_true", help="print the query to export")
    p.add_argument(
        "--pick",
        choices=("commonest", "longest"),
        default="commonest",
        help="which of the lists the routes give a stage becomes its stops: the "
        "one most routes give (default), or the one with the most stops",
    )
    p.add_argument("--write", action="store_true", help="write it; without this, nothing is written")
    p.add_argument("--reset", action="store_true", help="replace this feed's stages")
    p.add_argument("--force", action="store_true", help="write even with open change sets")
    p.add_argument(
        "--rerun",
        action="store_true",
        help="the feed was backfilled before and its rows have been put back from a "
        "copy taken beforehand; run over them again. Without this a second run would "
        "read back its own output and raise nothing",
    )
    p.add_argument("--actor", default="backfill", help="goes in updated_by and the audit row")
    p.add_argument("--batch", help="names this run on the review rows (default: the date and time)")
    p.add_argument("--review-csv", metavar="PATH", help="every name raised for review, one row each")
    p.add_argument("--report", metavar="PATH", help="per-route detail, tab separated")
    p.add_argument(
        "--refresh-evidence",
        metavar="URL",
        help="read the feed as it was from URL and rewrite only the review evidence"
        " in --db: no stage, no route link and no review status is touched",
    )
    p.add_argument("--self-test", action="store_true", help="check the choosing and stop")
    args = p.parse_args()

    if args.self_test:
        self_test()
        return 0
    if args.print_replica_sql:
        print(REPLICA_SQL)
        return 0
    if not args.db:
        p.error("--db is required")
    db = normalise(args.db)

    gtfs = args.gtfs_id
    if scalar(db, f"SELECT count(*) FROM gtfs_feed WHERE gtfs_id = {lit(gtfs)}") == "0":
        die(f"no feed {gtfs} in {redact(db)}")
    # Refreshing reads the feed as it was and writes only the review evidence
    # back to --db, which holds a finished backfill: the rerun guard is about
    # reading a backfill's own output, and here the reading is from elsewhere.
    source = normalise(args.refresh_evidence) if args.refresh_evidence else db
    if args.refresh_evidence:
        if args.write and args.reset:
            die("--refresh-evidence rewrites evidence only; drop --reset")
        if scalar(source, f"SELECT count(*) FROM gtfs_feed WHERE gtfs_id = {lit(gtfs)}") == "0":
            die(f"no feed {gtfs} in {redact(source)}")
    else:
        refuse_if_busy(db, gtfs, args, writing=args.write)

    log(f"reading {gtfs} from {redact(source)} ...")
    live_rows = read_internal(source, gtfs)
    if not live_rows:
        die(f"{gtfs} has no gtfs_route_stop rows")
    stop_names = dict(
        query(source, f"SELECT stop_id, name FROM gtfs_stop WHERE gtfs_id = {lit(gtfs)}")
    )
    skips = read_skips(args.skip_routes, gtfs)
    left_alone = sorted(set(skips) & set(live_rows), key=route_sort)
    for route_id in left_alone:
        del live_rows[route_id]
    if left_alone:
        by_tier = collections.Counter(skips[r] or "no tier" for r in left_alone)
        log(
            f"  leaving {len(left_alone)} route(s) alone "
            + ", ".join(f"{n} {t}" for t, n in sorted(by_tier.items()))
            + f": {' '.join(left_alone[:8])}"
            + (" ..." if len(left_alone) > 8 else "")
        )

    problems = collections.defaultdict(list)
    runs_by_route = cut_runs(live_rows, problems)
    n_runs = sum(len(r) for r in runs_by_route.values())
    log(f"  {sum(len(v) for v in live_rows.values())} rows, {len(live_rows)} routes, "
        f"{n_runs} fare stages")

    if args.replica_db:
        spine_rows = read_spine_db(normalise(args.replica_db))
    elif args.replica_csv:
        spine_rows = read_spine_csv(args.replica_csv)
    elif args.no_replica:
        spine_rows = []
    else:
        p.error("pass one of --replica-db, --replica-csv or --no-replica")
    spine = spine_from_rows(spine_rows) if spine_rows else {}
    if spine_rows:
        log(f"  spine: {len(spine)} routes, {len(spine_rows)} fare stages")

    stages, reviews = choose_stages(runs_by_route, spine, args.pick, stop_names)
    short_names = dict(
        query(
            source,
            "SELECT route_id, coalesce(short_name, '') FROM gtfs_route"
            f" WHERE gtfs_id = {lit(gtfs)} AND NOT deleted",
        )
    )
    issues = route_issues(runs_by_route, spine, short_names)
    if issues:
        by_kind = collections.Counter(i["issue"] for i in issues)
        log(
            "  routes MTC does not line up with: "
            + ", ".join(f"{n} {k}" for k, n in sorted(by_kind.items()))
            + f" ({sum(i['stages_unkeyed'] for i in issues)} stages left name-keyed)"
        )

    # Does the spine agree with our own rows about a route's stages? Two quite
    # different things look like disagreement and only one matters: a name
    # written differently (KELAMBAKKAM / KELAMBAKKAM B.T) is no threat, a
    # different set or order of stages means the two do not agree about the route.
    agree = renamed = reshaped = 0
    renames = collections.Counter()
    disagreements = []
    for route_id, runs in runs_by_route.items():
        if route_id not in spine:
            continue
        ours = [canon(r.name) for r in runs]
        theirs = [canon(n) for n, _ in spine[route_id][1]]
        if ours == theirs:
            agree += 1
        elif len(ours) == len(theirs):
            renamed += 1
            for a, b in zip(ours, theirs):
                if a != b:
                    renames[(a, b)] += 1
        else:
            reshaped += 1
            disagreements.append((route_id, ours, theirs))
    only_ours = sorted(set(runs_by_route) - set(spine), key=route_sort)
    only_theirs = sorted(set(spine) - set(runs_by_route), key=route_sort)

    changed, spelling_only, before_n, after_n = {}, [], 0, 0
    for route_id, runs in runs_by_route.items():
        before = [r.whole() for r in live_rows[route_id]]
        after = flatten(runs)
        before_n, after_n = before_n + len(before), after_n + len(after)
        if before != after:
            changed[route_id] = True
            if [(r[0],) + r[2:] for r in before] == [(r[0],) + r[2:] for r in after]:
                spelling_only.append(route_id)

    # ------------------------------------------------------------- the report
    directions = collections.Counter(s.direction for s in stages)
    by_reason = collections.Counter(r["reason"] for r in reviews)
    print()
    print(f"stages         {len(stages):>8}   "
          + ", ".join(f"{d or 'no direction'} {n}"
                      for d, n in sorted(directions.items(), key=lambda kv: (kv[0] or "~"))))
    print(f"stage stops    {sum(len(s.stops) for s in stages):>8}")
    print(f"route stages   {n_runs:>8}   over {len(runs_by_route)} routes")
    print()
    if spine:
        print("the spine")
        print(f"  agrees stage for stage on               {agree} routes")
        print(f"  same stages, a name written differently: {renamed} routes"
              + (f" ({len(renames)} such names)" if renames else ""))
        print(f"  a different set or order of stages:      {reshaped} routes"
              + ("  <- worth a look" if reshaped else ""))
        print(f"  our routes it does not carry:            {len(only_ours)} (direction unknown)")
        print(f"  its routes we do not carry:              {len(only_theirs)}")
        for (ours_name, theirs), n in renames.most_common(4):
            print(f"      {ours_name}  ->  {theirs}   ({n} routes)")
        print()
    print(f"what the feed serves   (--pick {args.pick})")
    print(f"  stop calls   {before_n} -> {after_n}  ({after_n - before_n:+})")
    print(f"  routes whose stops change: {len(changed) - len(spelling_only)} of {len(runs_by_route)}")
    print(f"  routes where only a name is spelled differently: {len(spelling_only)}")
    print()
    big = [r for r in reviews if r["impact"] >= WORTH_A_LOOK]
    print(f"for someone to look at   {len(reviews)} of the {len(stages)} stages")
    for reason in REVIEW_REASONS:
        if by_reason[reason]:
            print(f"  {reason:<22}{by_reason[reason]:>6}   {WHAT_TO_DO[reason]}")
    seen = sum(r["impact"] for r in big)
    all_wrong = sum(r["impact"] for r in reviews) or 1
    print(f"  of those, {len(big)} change {WORTH_A_LOOK} or more stop calls and hold "
          f"{round(100 * seen / all_wrong)}% of the difference; the rest are small")
    if problems:
        print()
        print("worth a look")
        for what, items in sorted(problems.items()):
            print(f"  {what}: {len(items)}")
            for item in items[:3]:
                print(f"      {item}")

    if args.review_csv:
        with open(args.review_csv, "w", newline="", encoding="utf-8") as fh:
            w = csv.writer(fh)
            w.writerow(["impact", "name", "direction", "reason", "what_to_do", "lists",
                        "routes", "chosen_stops", "begins_at", "spellings", "stage_id"])
            for r in sorted(reviews, key=lambda r: (-r["impact"], r["name_key"])):
                ev = r["evidence"]
                chosen = next((c for c in ev["candidates"] if c["chosen"]), None)
                w.writerow([
                    r["impact"],
                    r["name"], r["direction"] or "", r["reason"], WHAT_TO_DO[r["reason"]],
                    r["lists"], r["route_count"],
                    " > ".join(chosen["stop_names"]) if chosen else "",
                    " | ".join(ev["head_names"]), " / ".join(ev["spellings"]), r["stage_id"],
                ])
        log(f"{len(reviews)} names to review -> {args.review_csv}")

    if args.report:
        with open(args.report, "w", encoding="utf-8") as fh:
            fh.write("route_id\tdirection\tstages\tcalls_before\tcalls_after\tchanged\n")
            for route_id in sorted(runs_by_route, key=route_sort):
                runs = runs_by_route[route_id]
                fh.write(f"{route_id}\t{spine.get(route_id, (None, None))[0] or ''}\t"
                         f"{len(runs)}\t{len(live_rows[route_id])}\t{len(flatten(runs))}\t"
                         f"{'yes' if route_id in changed else 'no'}\n")
            fh.write("\n# routes whose stages the spine does not agree about\n")
            for route_id, ours, theirs in disagreements:
                fh.write(f"# {route_id}\n#   ours   {' > '.join(ours)}\n"
                         f"#   spine  {' > '.join(theirs)}\n")
        log(f"per-route detail -> {args.report}")

    if args.refresh_evidence:
        return refresh_evidence(db, gtfs, reviews, issues, args)

    if not args.write:
        print()
        print("Dry run: nothing was written. Add --write to write it.")
        return 0

    batch = args.batch or f"backfill {datetime.datetime.now().astimezone().isoformat(timespec='seconds')}"
    sql = build_sql(
        gtfs, stages, runs_by_route, changed, live_rows, reviews, issues, args, batch
    )
    fd, path = tempfile.mkstemp(prefix="backfill_stages_", suffix=".sql")
    os.close(fd)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(sql)
    log(f"sql -> {path} ({len(sql)} bytes)")
    psql(db, ["--single-transaction", "-f", path])
    print()
    print(f"{len(stages)} stages, {sum(len(s.stops) for s in stages)} stage stops, "
          f"{n_runs} route stages, {len(changed)} routes rewritten, "
          f"{len(reviews)} raised for review.")
    print("Every route is now built from its stages.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
