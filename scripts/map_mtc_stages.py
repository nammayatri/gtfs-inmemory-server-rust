#!/usr/bin/env python3
"""Map MTC's replica stages onto our GTFS routes, and write the stage tables.

`gtfs_route_stop` is **read only**. Nothing here inserts, updates, deletes or
copies into it, and a self-test asserts the generated SQL never names it in a
write. It is read to learn which of our stops belong to each route's fare stage,
and that is all.

What is written: `gtfs_stage`, `gtfs_stage_stop`, `gtfs_route_stage`,
`gtfs_stage_review`, `gtfs_route_stage_issue` and one `gtfs_audit_log` row.

Where a stage comes from
------------------------

MTC's replica is the source of truth for what a route's stages ARE:

    bus_route          route_id, route_direction (UP / DOWN)
    bus_route_point    route_id, route_order, stage_no, bus_stop_id
    bus_stop           bus_stop_id, bus_stop_name

which gives, per route, an ordered list of `(route_order, stage_no, bus_stop_id,
stage_name)`. A mapped stage takes its identity from the replica and nothing
else:

    stage_id  = MTC bus_stop_id
    direction = MTC route_direction
    name      = MTC bus_stop_name

`bus_stop_id` alone is not an identity: the same id is used in both directions
and the stops after the boundary differ, so the stage's key is the id AND the
direction.

Where a stage's stops come from
-------------------------------

The replica has no stop list we can use - its `bus_stop_id`s are MTC integers
with no bridge to the editor's stop ids. The stops are in our own
`gtfs_route_stop`, read as:

    SELECT * FROM gtfs_route_stop
     WHERE route_id = <the replica's route_id> AND stage_name = <the bus_stop_name>

which holds because the editor's `route_id` IS the replica's. So the chain is

    replica route + stage identity  ->  gtfs_route_stop rows  ->  a stop list

and one stage collects a stop list from every route that runs it. Those lists
are *observations*, not agreement: route 101 and 102 may give A B C D where 103
gives A B D.

Reconciling a route
-------------------

An equal number of stages is **never** on its own a reason to map. A route can
list as many fare stages as MTC and still be a different route. So:

  1. **By position**, only when the two lists are the same length AND every
     position carries the same name. Then they are the same list.
  2. Otherwise **by name, where the name is unique on both sides**. A name
     carried once in each list names one stage each way, whatever else differs.
  3. Anything left over is left unmapped and raised for review.

Nothing is matched fuzzily, ever. Names are compared exactly, less the spaces
around them. A route that cannot be mapped keeps stages named after themselves
(`nm_<name>`) so nothing is lost, and the route is raised in
`gtfs_route_stage_issue` with both sequences in full.

Choosing a stage's stops
------------------------

`--pick commonest` (the default) takes the exact list the most routes give;
`--pick longest` takes the longest. Ties break deterministically - the longer
list, then the lowest route id - so a rerun makes the same choice.

**Whichever is chosen, a stage whose routes disagree is always raised** in
`gtfs_stage_review`, carrying every candidate list, the stops and their names,
the routes giving each, which was selected and why the review exists. Choosing
is not deciding; a person decides.

Usage
-----

    # 1. the spine: export this query from Metabase as CSV
    python3 scripts/map_mtc_stages.py --print-replica-sql

    # 2. dry run - writes nothing, reports everything
    python3 scripts/map_mtc_stages.py --db <editor> --replica-csv spine.csv

    # 3. write it
    python3 scripts/map_mtc_stages.py --db <editor> --replica-csv spine.csv \
        --write --reset --actor you@nammayatri.in --batch "first run"

Rerunning is safe and idempotent: the run reads the same rows every time and
writes only the stage tables. `--reset` replaces the generated stages of the
routes this run covers and leaves a skipped route's stages alone.

`--no-replica` takes the stage order and names from `gtfs_route_stop` alone and
leaves every direction NULL. It is for trying a run out where the replica is not
reachable; a real run wants the directions, or the two directions of one corridor
become one stage.

Standard library only, and shells out to `psql` - see scripts/requirements.txt.
"""
from __future__ import annotations

import argparse
import collections
import csv
import datetime
import hashlib
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

# Every kind of route-level discrepancy this run can raise. The table's CHECK
# must allow all of them or the write fails; `refuse_if_busy` says so first.
ISSUE_KINDS = (
    "absent",            # the replica does not carry the route
    "missing_internal",  # the replica carries it and our feed does not
    "count_differs",     # a different number of fare stages
    "name_differs",      # the same stages in the same places, spelled differently
    "order_differs",     # the same stages in another order
    "set_differs",       # neither the same set nor the same order
    "ambiguous_names",   # names repeat, so no stage can be identified safely
)


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


# Whether two names that differ only in case are one name. Off by default: a
# stage is whatever MTC's data says it is, and folding decides for the reviewer
# that two spellings mean one place. --ignore-case turns it on, which on
# chennai_bus is the difference between 5,405 stages and about 3,900, because
# 1,474 of the 2,276 name-keyed stages differ from an MTC-keyed one in case
# alone ('230 k.v.tower' against '230 K.V.TOWER').
FOLD_CASE = False


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
    n = (name or "").strip()
    return n.upper() if FOLD_CASE else n


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
    return ("name", canon(run.name), direction), run.name


def stage_id_of(key):
    """The stage's id: MTC's `bus_stop_id` for the fare-stage head, as it stands.

    It is not unique on its own - 1,746 of MTC's 1,836 fare-stage stops are used
    both ways - which is why direction is the other half of the stage's key
    (gtfs_stage's primary key is gtfs_id, stage_id, direction). Nothing is
    minted: a rerun over the same data writes the same ids.

    A fare stage the replica did not line up with falls back to its name, since
    there is no stop id to use. The name is slugged, and slugging is lossy -
    `SIRUSERI I.T PARK` and `SIRUSERI I.T.PARK` are two names under exact
    matching and slug to one id - so `stage_ids_for` gives the ones that collide
    a digest of the name they really carry. Use that, not this, to name a set of
    stages; this is the plain form and may repeat.
    """
    kind, ident, _direction = key
    if kind == "mtc":
        return ident
    slug = re.sub(r"[^A-Za-z0-9]+", "_", ident).strip("_")[:48] or "unnamed"
    return f"nm_{slug}"


def stage_ids_for(keys):
    """`key -> stage id`, with every collision settled.

    Two different names can slug to one `nm_` id. Where they do, EVERY name
    sharing that slug takes a digest of itself, so which id a name gets does not
    depend on what else happens to be in the feed or on the order they are seen
    in - the same name always lands on the same id, and two names never land on
    the same one.
    """
    plain = {key: stage_id_of(key) for key in keys}
    # a slug is contested when two different NAMES want it; the same name in
    # both directions is not a collision, direction being the rest of the key
    names = collections.defaultdict(set)
    for key, sid in plain.items():
        if key[0] != "mtc":
            names[sid].add(key[1])
    out = {}
    for key, sid in plain.items():
        if key[0] != "mtc" and len(names[sid]) > 1:
            digest = hashlib.sha1(key[1].encode("utf-8")).hexdigest()[:6]
            sid = f"{sid[:41]}_{digest}"
        out[key] = sid
    return out


def reconcile(runs, heads):
    """Which fare stage of the replica each of our runs is, or None, and why.

    Returns `(paired, reason, detail)`. `paired[i]` is the replica head for
    `runs[i]`, or None when nothing could be said about it safely.

    Equal counts are **not** on their own a reason to pair. A route can list the
    same number of fare stages as MTC and still be a different route - stages in
    a different order, or a different set of them entirely - and pairing those by
    position hands each stage the id of whichever MTC stage happens to sit at
    that index. So:

      1. **Position**, only when the two lists are the same length AND every
         position carries the same name. Then the lists are the same list and
         the mapping is not a guess.
      2. Otherwise **by name, where the name is unique on both sides**. A name
         carried once in our list and once in MTC's names one stage each way, so
         that pair is certain whatever else differs. A name carried twice on
         either side names nothing, and is left unmapped.
      3. Anything still unmapped is left unmapped and reported. Nothing is
         matched fuzzily, ever, and no stage is given an MTC id because a count
         came out equal.

    Names are compared with `canon` - the string with the spaces around it
    removed, nothing else folded (see its docstring).
    """
    if not heads:
        return [None] * len(runs), "absent", {"matched": 0}
    ours = [canon(r.name) for r in runs]
    theirs = [canon(n) for n, _ in heads]
    if len(ours) == len(theirs) and ours == theirs:
        return list(heads), None, {"matched": len(runs)}

    # by name, and only where the name says one stage on each side
    mine = collections.Counter(ours)
    yours = collections.Counter(theirs)
    where = {n: i for i, n in enumerate(theirs)}
    paired = [None] * len(runs)
    matched = 0
    for i, name in enumerate(ours):
        if mine[name] == 1 and yours.get(name) == 1:
            paired[i] = heads[where[name]]
            matched += 1
    ambiguous = sorted({n for n in ours if mine[n] > 1 or yours.get(n, 0) > 1})
    # Which KIND of discrepancy, said apart rather than lumped together: a name
    # spelled differently is not the same problem as a route running its stages
    # in another order, and an operations queue that cannot tell them apart is
    # 4,435 rows of spelling hiding 72 real ones.
    if matched == 0 and ambiguous:
        # nothing could be said at all: the names repeat, so no pair is certain.
        # This is said ahead of the count, because a differing count is not the
        # actionable fact when no stage could be identified either way.
        reason = "ambiguous_names"
    elif len(ours) != len(theirs):
        reason = "count_differs"
    elif sorted(ours) == sorted(theirs):
        # the same names, in another order
        reason = "order_differs"
    elif all(where[ours[i]] == i for i, p in enumerate(paired) if p):
        # everything that matched sits at the same index on both sides, so the
        # route is the same route and what differs is how a stage is spelled
        reason = "name_differs"
    else:
        reason = "set_differs"
    return paired, reason, {
        "matched": matched,
        "unmatched_ours": [i for i, p in enumerate(paired) if p is None],
        "ambiguous": ambiguous[:20],
    }


WHY = {
    "agreed": "Every route using this stage gave it the same stops. It is raised so "
              "the mapping can be looked over and confirmed.",
    "head_differs": "The routes begin this stage at stops with different names, so the "
                    "name is being used for more than one place.",
    "head_duplicate_stops": "The routes begin this stage at different stop records that "
                            "all carry one name; merging the stops settles it.",
    "stretch_differs": "The routes agree where the stage begins and disagree about the "
                       "stops between the fare boundaries.",
    "no_stops": "The replica says routes run this fare stage, but our rows give it no "
                "stops at all.",
}


def choose_stages(runs_by_route, spine, pick, stop_names, verdicts=None):
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
    if verdicts is None:
        verdicts = {}
    around = {}
    for route_id, runs in runs_by_route.items():
        for i, run in enumerate(runs):
            around[(route_id, run.position)] = (
                runs[i - 1].name if i else None,
                runs[i + 1].name if i + 1 < len(runs) else None,
            )

    groups = collections.defaultdict(list)
    names_of = {}
    for route_id, runs in sorted(runs_by_route.items(), key=lambda kv: route_sort(kv[0])):
        direction, heads = spine.get(route_id, (None, None))
        paired, reason, detail = reconcile(runs, heads)
        # what the reconciliation decided, kept for route_issues to write out
        verdicts[route_id] = (reason, detail, direction, heads, paired)
        for run, head in zip(runs, paired):
            run.key, shown = stage_key_of(run, head, direction)
            groups[run.key].append(run)
            names_of.setdefault(run.key, shown)

    stages, reviews = [], []
    ids = stage_ids_for(groups)
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

        stage = Stage(ids[key], names_of[key], key[2], stops)
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
            for v, rs in ranked
        ]
        # every route's own observation of this stage, so nothing a route said
        # is lost behind the list it happened to share
        observations = sorted(
            (
                {
                    "route_id": r.route_id,
                    "stage_no": r.stage_no,
                    "name": r.name,
                    "stops": [x[0] for x in r.stops()],
                    "candidate": [i for i, (v, _rs) in enumerate(ranked)
                                  if v == r.stops()][0],
                }
                for r in runs
            ),
            key=lambda o: route_sort(o["route_id"]),
        )
        # `agreed` is not a problem with the stage, so it carries no flag: the
        # chip and the ?review= filter stay for the ones that need settling.
        if reason != "agreed":
            stage.review = reason
        stage.evidence = {
            # what this stage IS, so the review reads without a join
            "stage_id": stage.stage_id,
            "direction": stage.direction,
            "stage_name": stage.name,
            "from_replica": not stage.stage_id.startswith("nm_"),
            # why this row exists at all
            "why": WHY[reason],
            # which list was taken, and under which rule
            "pick": pick,
            "selected": [x[0] for x in stops],
            "routes_disagree": len(ranked) > 1,
            "impact": impact,
            "candidates": candidates,
            "lists": len(ranked),
            "heads": heads_seen,
            "head_names": sorted({canon(stop_names.get(h, "")) for h in heads_seen} - {""}),
            "spellings": sorted({r.name for r in runs}),
            "routes": sorted({r.route_id for r in runs}, key=route_sort)[:20],
            "route_count": len({r.route_id for r in runs}),
            "observations": observations,
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


def route_issues(runs_by_route, verdicts, short_names, spine=None):
    """The routes MTC's own route definition does not line up with.

    One row per route the reconciliation could not settle: the replica does not
    carry it (`absent`), the two list a different number of fare stages
    (`count_differs`), they list the same number in a different order or a
    different set (`order_differs`), or the names repeat on one side so nothing
    can be said safely (`ambiguous_names`); and one row per route the replica
    carries that we do not (`missing_internal`).

    A replica stage with **no internal rows** shows up as an unmatched replica
    stage, and is repeated in `stages_without_stops` under that name: the
    replica says the route runs that fare stage and `gtfs_route_stop` has no
    stops for it. (A stage that maps always has stops, because a stage is built
    out of those rows in the first place.)

    The row carries **both sequences in full**, which stage matched which, and
    which did not, so a person can settle it without going back to the source.
    """
    out = []
    for route_id, runs in sorted(runs_by_route.items(), key=lambda kv: route_sort(kv[0])):
        reason, detail, direction, heads, paired = verdicts.get(
            route_id, (None, {}, None, None, [])
        )
        if not reason:
            continue
        unkeyed = sum(
            1 for r in runs if r.stage is not None and r.stage.stage_id.startswith("nm_")
        )
        matched = [
            {
                "ours_position": i + 1,
                "ours_stage_no": runs[i].stage_no,
                "name": runs[i].name,
                "bus_stop_id": head[1],
                "mtc_name": head[0],
            }
            for i, head in enumerate(paired)
            if head
        ]
        out.append(
            {
                "route_id": route_id,
                "short_name": short_names.get(route_id),
                "issue": reason,
                "direction": direction,
                # our fare stages in order, exactly as they are written
                "ours": [
                    {"position": i + 1, "stage_no": r.stage_no, "name": r.name,
                     "matched": paired[i] is not None}
                    for i, r in enumerate(runs)
                ],
                # MTC's, in order, with the id each one would have given
                "theirs": [
                    {"position": i + 1, "name": n, "bus_stop_id": bid,
                     "matched": any(p and p[1] == bid for p in paired)}
                    for i, (n, bid) in enumerate(heads or [])
                ],
                "matched": matched,
                "unmatched_ours": [
                    {"position": i + 1, "stage_no": runs[i].stage_no, "name": runs[i].name}
                    for i, head in enumerate(paired) if head is None
                ],
                "unmatched_theirs": [
                    {"position": i + 1, "name": n, "bus_stop_id": bid}
                    for i, (n, bid) in enumerate(heads or [])
                    if not any(p and p[1] == bid for p in paired)
                ],
                "ambiguous_names": detail.get("ambiguous", []),
                # replica stages this route's rows give no stops for
                "stages_without_stops": [
                    {"position": i + 1, "name": n, "bus_stop_id": bid}
                    for i, (n, bid) in enumerate(heads or [])
                    if not any(p and p[1] == bid for p in paired)
                ],
                "counts": {"ours": len(runs), "theirs": len(heads or []),
                           "matched": detail.get("matched", 0)},
                "stages_unkeyed": unkeyed,
            }
        )
    # a route the replica carries that our feed does not have at all
    for route_id, (direction, heads) in sorted(
        (spine or {}).items(), key=lambda kv: route_sort(kv[0])
    ):
        if route_id in runs_by_route:
            continue
        out.append(
            {
                "route_id": route_id,
                "short_name": short_names.get(route_id),
                "issue": "missing_internal",
                "direction": direction,
                "ours": [],
                "theirs": [
                    {"position": i + 1, "name": n, "bus_stop_id": bid, "matched": False}
                    for i, (n, bid) in enumerate(heads or [])
                ],
                "matched": [],
                "unmatched_ours": [],
                "unmatched_theirs": [
                    {"position": i + 1, "name": n, "bus_stop_id": bid}
                    for i, (n, bid) in enumerate(heads or [])
                ],
                "ambiguous_names": [],
                "stages_without_stops": [],
                "counts": {"ours": 0, "theirs": len(heads or []), "matched": 0},
                "stages_unkeyed": 0,
            }
        )
    return out


# ----------------------------------------------------------------- writing it


def build_sql(gtfs, stages, runs_by_route, reviews, issues, args, batch):
    """The whole backfill as one script, to be run in one transaction."""
    g = lit(gtfs)
    out = [
        f"-- {PROG}: stages for {gtfs}",
        f"-- {len(stages)} stages, {sum(len(s.stops) for s in stages)} stage stops, "
        f"{sum(len(r) for r in runs_by_route.values())} route stages, "
        f"{len(reviews)} raised for review",
        "",
    ]
    if args.reset:
        # Only what this run replaces. A route left alone (--skip-routes: the
        # premium routes) keeps its stages, because this run writes none for it
        # and a blanket DELETE would leave it with no stages at all. Its stages
        # go only if nothing points at them any more.
        kept = sorted(runs_by_route, key=route_sort)
        mine = ", ".join(lit(r) for r in kept) if kept else "NULL"
        out += [
            "-- the stages of the routes this run writes; a skipped route keeps its own",
            f"DELETE FROM gtfs_route_stage WHERE gtfs_id = {g}"
            f" AND route_id IN ({mine});",
            "DELETE FROM gtfs_stage_stop ss WHERE ss.gtfs_id = " + g
            + " AND NOT EXISTS (SELECT 1 FROM gtfs_route_stage rs"
            " WHERE rs.gtfs_id = ss.gtfs_id AND rs.stage_id = ss.stage_id"
            " AND rs.direction = ss.direction);",
            "DELETE FROM gtfs_stage st WHERE st.gtfs_id = " + g
            + " AND NOT EXISTS (SELECT 1 FROM gtfs_route_stage rs"
            " WHERE rs.gtfs_id = st.gtfs_id AND rs.stage_id = st.stage_id"
            " AND rs.direction = st.direction);",
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


    detail = {
        "stages": len(stages),
        "stage_stops": sum(len(s.stops) for s in stages),
        "route_stages": sum(len(r) for r in runs_by_route.values()),
        "routes": len(runs_by_route),
        "reviews": len(reviews),
        "pick": args.pick,
        "ignore_case": FOLD_CASE,
        "batch": batch,
    }
    out += [
        "INSERT INTO gtfs_audit_log (actor_email, action, gtfs_id, detail)",
        f"VALUES ({lit(args.actor)}, 'stages_mapped', {g}, "
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
    # Rerunning is safe: gtfs_route_stop is read only, so this run reads the
    # same rows the last one did and reaches the same stages. Earlier runs are
    # still worth saying, because --reset is what replaces their stages.
    ran = scalar(
        url,
        "SELECT count(*) FROM gtfs_audit_log"
        f" WHERE gtfs_id = {g} AND action = 'stages_mapped'",
    )
    if int(ran):
        when = scalar(
            url,
            "SELECT max(at)::text FROM gtfs_audit_log"
            f" WHERE gtfs_id = {g} AND action = 'stages_mapped'",
        )
        log(f"  note: {gtfs} has been mapped before ({ran} run(s), last at {when}); "
            "this run reads the same rows and reaches the same stages")
    allowed = {
        r[0]
        for r in query(
            url,
            "SELECT unnest(string_to_array("
            " regexp_replace(pg_get_constraintdef(oid), '.*ARRAY\\[(.*)\\].*', '\\1'),"
            " ', ')) FROM pg_constraint"
            " WHERE conrelid = 'gtfs_route_stage_issue'::regclass AND contype = 'c'"
            "   AND pg_get_constraintdef(oid) LIKE '%issue%'",
        )
    }
    allowed = {a.strip().split("::")[0].strip().strip("'") for a in allowed}
    missing = sorted(set(ISSUE_KINDS) - allowed) if allowed else []
    if missing:
        stop(
            "gtfs_route_stage_issue will not accept "
            + ", ".join(missing)
            + ". The table's CHECK allows only "
            + ", ".join(sorted(allowed))
            + ". Widen it before writing:\n"
            "  ALTER TABLE gtfs_route_stage_issue DROP CONSTRAINT gtfs_route_stage_issue_issue_check;\n"
            "  ALTER TABLE gtfs_route_stage_issue ADD CONSTRAINT gtfs_route_stage_issue_issue_check\n"
            "      CHECK (issue IN (" + ", ".join(f"'{k}'" for k in ISSUE_KINDS) + "));"
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

    def staged(route_id, spec):
        """A route as [(stage_no, stage_name, "abc"), ...]."""
        out, seq = [], 0
        for stage_no, stage_name, names in spec:
            for j, n in enumerate(names):
                seq += 1
                out.append(Row(seq, f"s{n}", "NEW STOP" if j == 0 else "INTERMEDIATE STOP",
                               stage_no, stage_name, None, None, None, None, None, "75", None))
        return out

    def run(pick, *routes, spine=None):
        by_route = {
            rid: cut_runs({rid: rows(rid, *rest)}, collections.defaultdict(list))[rid]
            for rid, *rest in routes
        }
        stages, reviews = choose_stages(by_route, spine or {}, pick, NAMES)
        return by_route, stages, {r["reason"] for r in reviews}

    def settle(spec, heads, direction="up"):
        """Reconcile one route's stages against a replica list of (name, id)."""
        by = cut_runs({"9": staged("9", spec)}, collections.defaultdict(list))
        verdicts = {}
        stages, _ = choose_stages(by, {"9": (direction, heads)}, "commonest", NAMES, verdicts)
        issues = route_issues(by, verdicts, {}, {"9": (direction, heads)})
        return by["9"], stages, (issues[0] if issues else None)

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

    # Every route of the name is put on that one stage, so they all serve its
    # list. The stage holds the stops once; nothing is written per route.
    by, stages, _ = run("commonest", ("1", "ab"), ("2", "ab"), ("3", "abcd"))
    assert len(stages) == 1 and len(stages[0].stops) == 2
    assert all(by[r][0].stage is stages[0] for r in ("1", "2", "3"))

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
    for rid, names, spelt_as in (("1", "abc", "SAIDAPET"), ("2", "abc", "SAIDAPET")):
        by_route[rid] = cut_runs({rid: rows(rid, names, spelt_as)},
                                 collections.defaultdict(list))[rid]
    spine = {"1": ("up", [("SAIDAPET", "912")]), "2": ("up", [("SAIDAPET", "912")])}
    stages, _ = choose_stages(by_route, spine, "commonest", NAMES)
    assert len(stages) == 1, f"one MTC stop, one stage: {len(stages)}"
    assert stages[0].stage_id == "912", stages[0].stage_id
    assert stages[0].direction == "up", stages[0].direction
    assert stages[0].name == "SAIDAPET", stages[0].name

    # A route whose name is spelled differently from MTC's is NOT keyed to it.
    # The count matching is not evidence, and folding the case would be deciding
    # for the reviewer that two spellings are one place.
    by_route = {}
    for rid, names, spelt_as in (("1", "abc", "SAIDAPET"), ("2", "abc", "saidapet")):
        by_route[rid] = cut_runs({rid: rows(rid, names, spelt_as)},
                                 collections.defaultdict(list))[rid]
    spine = {"1": ("up", [("SAIDAPET", "912")]), "2": ("up", [("SAIDAPET", "912")])}
    verdicts = {}
    stages, _ = choose_stages(by_route, spine, "commonest", NAMES, verdicts)
    assert sorted(st.stage_id for st in stages) == ["912", "nm_saidapet"], \
        sorted(st.stage_id for st in stages)
    assert verdicts["2"][0] == "name_differs", verdicts["2"][0]
    assert verdicts["1"][0] is None, "the one that agrees is not raised"

    # One stop id used both ways is two stages: the stops after the boundary differ.
    by_route = {
        "1": cut_runs({"1": rows("1", "abc", "SAIDAPET")}, collections.defaultdict(list))["1"],
        "2": cut_runs({"2": rows("2", "abc", "SAIDAPET")}, collections.defaultdict(list))["2"],
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
        "1": cut_runs({"1": rows("1", "abc", "ADYAR B.T")}, collections.defaultdict(list))["1"],
        "2": cut_runs({"2": rows("2", "xyz", "ADYAR B.T")}, collections.defaultdict(list))["2"],
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

    # --------------------------------------- the spec's thirteen, in its order
    # An equal count is never on its own a reason to map.

    # 1. four stages, identical on both sides -> nothing to review
    _runs, stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "BETA", "cd"), (3, "GAMMA", "ef"), (4, "DELTA", "gh")],
        [("ALPHA", "1"), ("BETA", "2"), ("GAMMA", "3"), ("DELTA", "4")],
    )
    assert issue is None, issue
    assert [st.stage_id for st in stages] == ["1", "2", "3", "4"], \
        [st.stage_id for st in stages]
    assert all(st.direction == "up" for st in stages)

    # 2. the replica has four, we have three -> the route is raised
    _runs, stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "BETA", "cd"), (3, "GAMMA", "ef")],
        [("ALPHA", "1"), ("BETA", "2"), ("GAMMA", "3"), ("DELTA", "4")],
    )
    assert issue["issue"] == "count_differs", issue["issue"]
    assert issue["counts"] == {"ours": 3, "theirs": 4, "matched": 3}, issue["counts"]
    assert [u["name"] for u in issue["unmatched_theirs"]] == ["DELTA"]
    # the replica stage our rows give no stops for is named as such
    assert [u["name"] for u in issue["stages_without_stops"]] == ["DELTA"]

    # 3. four each, but the names differ -> raised, and NEVER mapped by position
    _runs, stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "BETA", "cd"), (3, "GAMMA", "ef"), (4, "ZETA", "gh")],
        [("ALPHA", "1"), ("BETA", "2"), ("GAMMA", "3"), ("DELTA", "4")],
    )
    assert issue["issue"] == "name_differs", issue["issue"]
    assert issue["counts"]["matched"] == 3, issue["counts"]
    # ZETA sits where DELTA sits and is NOT given id 4
    assert "4" not in [st.stage_id for st in stages], [st.stage_id for st in stages]
    assert "nm_ZETA" in [st.stage_id for st in stages], [st.stage_id for st in stages]

    # 4. the same set in a different order -> raised
    _runs, _stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "GAMMA", "cd"), (3, "BETA", "ef")],
        [("ALPHA", "1"), ("BETA", "2"), ("GAMMA", "3")],
    )
    assert issue["issue"] == "order_differs", issue["issue"]
    assert issue["counts"]["matched"] == 3, "each name is still unique on both sides"

    # 5. a replica stage our rows have nothing for -> raised, and said plainly
    _runs, _stages, issue = settle(
        [(1, "ALPHA", "ab")], [("ALPHA", "1"), ("BETA", "2")],
    )
    assert issue["issue"] == "count_differs", issue["issue"]
    assert issue["stages_without_stops"] == [
        {"position": 2, "name": "BETA", "bus_stop_id": "2"}
    ], issue["stages_without_stops"]

    # 6. a stage of ours the replica does not have -> raised
    _runs, _stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "EXTRA", "cd")], [("ALPHA", "1")],
    )
    assert issue["issue"] == "count_differs", issue["issue"]
    assert [u["name"] for u in issue["unmatched_ours"]] == ["EXTRA"], issue["unmatched_ours"]

    # 6b. a route the replica carries that we do not have at all
    by = cut_runs({"9": staged("9", [(1, "ALPHA", "ab")])}, collections.defaultdict(list))
    verdicts = {}
    choose_stages(by, {"9": ("up", [("ALPHA", "1")])}, "commonest", NAMES, verdicts)
    spine = {"9": ("up", [("ALPHA", "1")]), "77": ("down", [("BETA", "2")])}
    kinds = {i["route_id"]: i["issue"] for i in route_issues(by, verdicts, {}, spine)}
    assert kinds == {"77": "missing_internal"}, kinds

    # 7. one MTC stage, two routes, the same stop list -> one shared stage
    by_route = {
        "1": cut_runs({"1": rows("1", "abc", "ALPHA")}, collections.defaultdict(list))["1"],
        "2": cut_runs({"2": rows("2", "abc", "ALPHA")}, collections.defaultdict(list))["2"],
    }
    spine = {"1": ("up", [("ALPHA", "912")]), "2": ("up", [("ALPHA", "912")])}
    stages, reviews = choose_stages(by_route, spine, "commonest", NAMES)
    assert len(stages) == 1 and stages[0].stage_id == "912"
    assert stages[0].runs == 2, stages[0].runs
    assert reviews[0]["reason"] == "agreed", reviews[0]["reason"]
    assert reviews[0]["evidence"]["routes_disagree"] is False

    # 8, 9, 10. one MTC stage, routes that disagree -> one shared stage, a list
    # chosen, and a review carrying EVERY candidate with the routes giving it,
    # under commonest and under longest alike
    # the two rules disagree here, so each is really being tested: two routes
    # give A B, one gives A B C D - commonest takes A B, longest takes A B C D
    for pick, want in (("commonest", ["sa", "sb"]), ("longest", ["sa", "sb", "sc", "sd"])):
        by_route = {
            "101": cut_runs({"101": rows("101", "ab", "ALPHA")}, collections.defaultdict(list))["101"],
            "102": cut_runs({"102": rows("102", "ab", "ALPHA")}, collections.defaultdict(list))["102"],
            "103": cut_runs({"103": rows("103", "abcd", "ALPHA")}, collections.defaultdict(list))["103"],
        }
        spine = {r: ("up", [("ALPHA", "912")]) for r in by_route}
        stages, reviews = choose_stages(by_route, spine, pick, NAMES)
        assert len(stages) == 1, f"{pick}: one shared stage"
        ev = reviews[0]["evidence"]
        assert ev["selected"] == want, f"{pick}: {ev['selected']}"
        assert ev["pick"] == pick and ev["routes_disagree"] is True
        assert reviews[0]["reason"] == "stretch_differs", reviews[0]["reason"]
        assert len(ev["candidates"]) == 2, f"{pick}: every list is kept"
        by_list = {tuple(c["stops"]): c for c in ev["candidates"]}
        assert by_list[("sa", "sb")]["routes"] == ["101", "102"]
        assert by_list[("sa", "sb", "sc", "sd")]["routes"] == ["103"]
        assert sum(1 for c in ev["candidates"] if c["chosen"]) == 1
        assert [c["stop_names"] for c in ev["candidates"]][0][0] == "Stop a"
        # and every route's own observation is kept, not just the lists
        assert {o["route_id"] for o in ev["observations"]} == {"101", "102", "103"}
        assert ev["why"], "a review says why it exists"

    # 11. one bus_stop_id used UP and DOWN is two stages
    by_route = {
        "1": cut_runs({"1": rows("1", "abc", "ALPHA")}, collections.defaultdict(list))["1"],
        "2": cut_runs({"2": rows("2", "abc", "ALPHA")}, collections.defaultdict(list))["2"],
    }
    spine = {"1": ("up", [("ALPHA", "912")]), "2": ("down", [("ALPHA", "912")])}
    stages, _ = choose_stages(by_route, spine, "commonest", NAMES)
    assert [st.stage_id for st in stages] == ["912", "912"]
    assert sorted(st.direction for st in stages) == ["down", "up"]

    # 12. a name carried twice names nothing: reviewed, never guessed
    _runs, stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "BETA", "cd"), (3, "ALPHA", "ef")],
        [("ALPHA", "1"), ("ALPHA", "7"), ("BETA", "2")],
    )
    assert issue["issue"] == "order_differs", issue["issue"]
    assert issue["ambiguous_names"] == ["ALPHA"], issue["ambiguous_names"]
    assert issue["counts"]["matched"] == 1, issue["counts"]
    assert sorted(st.stage_id for st in stages) == ["2", "nm_ALPHA"], \
        sorted(st.stage_id for st in stages)
    # 12b. nothing at all can be said
    _runs, _stages, issue = settle(
        [(1, "ALPHA", "ab"), (2, "ALPHA", "cd")],
        [("ALPHA", "1"), ("ALPHA", "7"), ("ALPHA", "9")],
    )
    assert issue["issue"] == "ambiguous_names", issue["issue"]
    assert issue["counts"]["matched"] == 0, issue["counts"]

    # 12c. Two names that slug to one id get told apart. Exact matching keeps
    # `SIRUSERI I.T PARK` and `SIRUSERI I.T.PARK` as two stages, and slugging
    # would have given both `nm_SIRUSERI_I_T_PARK` - a duplicate primary key.
    by_route = {}
    for rid, spelt_as in (("1", "SIRUSERI I.T PARK"), ("2", "SIRUSERI I.T.PARK"),
                          ("3", "SIRUSERI I.T.PARK")):
        by_route[rid] = cut_runs({rid: rows(rid, "abc", spelt_as)},
                                 collections.defaultdict(list))[rid]
    stages, _ = choose_stages(by_route, {}, "commonest", NAMES)
    got = sorted(st.stage_id for st in stages)
    assert len(got) == 2 and len(set(got)) == 2, got
    assert all(g.startswith("nm_SIRUSERI_I_T_PARK_") for g in got), got
    # the same name always lands on the same id, whatever else is in the feed
    again, _ = choose_stages(by_route, {}, "commonest", NAMES)
    assert sorted(st.stage_id for st in again) == got, "ids are stable"
    # and a name that collides with nothing keeps the plain, readable form
    by_route["4"] = cut_runs({"4": rows("4", "abc", "ADYAR B.T")},
                             collections.defaultdict(list))["4"]
    stages, _ = choose_stages(by_route, {}, "commonest", NAMES)
    assert "nm_ADYAR_B_T" in [st.stage_id for st in stages], \
        [st.stage_id for st in stages]

    # 12d. --ignore-case: two spellings of one name become one stage, and the
    # name that is shown is the one the lowest-numbered route carries.
    global FOLD_CASE
    by_route = {}
    for rid, spelt_as in (("1", "ALANDUR COURT"), ("2", "alandur court")):
        by_route[rid] = cut_runs({rid: rows(rid, "abc", spelt_as)},
                                 collections.defaultdict(list))[rid]
    stages, _ = choose_stages(by_route, {}, "commonest", NAMES)
    assert len(stages) == 2, f"case matters by default: {len(stages)}"
    try:
        FOLD_CASE = True
        stages, _ = choose_stages(by_route, {}, "commonest", NAMES)
        assert len(stages) == 1, f"--ignore-case makes them one: {len(stages)}"
        assert stages[0].stage_id == "nm_ALANDUR_COURT", stages[0].stage_id
        assert stages[0].name == "ALANDUR COURT", stages[0].name
        assert stages[0].runs == 2, stages[0].runs
        # and it maps to MTC's id when only the case stood in the way
        spine = {"1": ("up", [("ALANDUR COURT", "326")]),
                 "2": ("up", [("ALANDUR COURT", "326")])}
        stages, _ = choose_stages(by_route, spine, "commonest", NAMES)
        assert [st.stage_id for st in stages] == ["326"], [st.stage_id for st in stages]
    finally:
        FOLD_CASE = False
    # back off again: the same input is two stages once more
    stages, _ = choose_stages(by_route, {}, "commonest", NAMES)
    assert len(stages) == 2, "the switch is not sticky"

    # 13. the generated SQL never writes gtfs_route_stop
    class Args:
        reset = True
        actor = "self-test"
        pick = "commonest"
    by = cut_runs({"9": staged("9", [(1, "ALPHA", "ab")])}, collections.defaultdict(list))
    verdicts = {}
    stages, reviews = choose_stages(by, {}, "commonest", NAMES, verdicts)
    sql = build_sql("feed", stages, by, reviews, route_issues(by, verdicts, {}),
                    Args(), "self-test")
    for bad in ("INSERT INTO gtfs_route_stop", "UPDATE gtfs_route_stop",
                "DELETE FROM gtfs_route_stop", "COPY gtfs_route_stop",
                "UPSERT into gtfs_route_stop", "gtfs_route_stop"):
        assert bad not in sql, f"gtfs_route_stop is read only; found {bad!r}"
    assert "INSERT INTO gtfs_audit_log" in sql, "the run records itself"
    for table in ("gtfs_stage", "gtfs_stage_stop", "gtfs_route_stage",
                  "gtfs_stage_review", "gtfs_route_stage_issue"):
        assert table in sql, f"{table} is written"
    # and --reset replaces only the stages of the routes this run covers
    assert "DELETE FROM gtfs_route_stage WHERE gtfs_id = 'feed' AND route_id IN ('9');" in sql

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
    p.add_argument("--actor", default="backfill", help="goes in updated_by and the audit row")
    p.add_argument("--batch", help="names this run on the review rows (default: the date and time)")
    p.add_argument("--review-csv", metavar="PATH", help="every name raised for review, one row each")
    p.add_argument("--report", metavar="PATH", help="per-route detail, tab separated")
    p.add_argument(
        "--ignore-case",
        action="store_true",
        help="treat two names that differ only in case as one name. Off by default: "
        "on chennai_bus it maps 1,474 stages that are otherwise named after "
        "themselves, at the cost of deciding that two spellings mean one place",
    )
    p.add_argument("--self-test", action="store_true", help="check the choosing and stop")
    args = p.parse_args()

    global FOLD_CASE
    FOLD_CASE = args.ignore_case
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
    source = db
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

    verdicts = {}
    stages, reviews = choose_stages(runs_by_route, spine, args.pick, stop_names, verdicts)
    short_names = dict(
        query(
            source,
            "SELECT route_id, coalesce(short_name, '') FROM gtfs_route"
            f" WHERE gtfs_id = {lit(gtfs)} AND NOT deleted",
        )
    )
    issues = route_issues(runs_by_route, verdicts, short_names, spine)
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
    print(f"  stop calls read: {sum(len(v) for v in live_rows.values())} "
          "(gtfs_route_stop is read only; nothing here writes to it)")
    print(f"  what the stages say each route runs is served by the view "
          "gtfs_route_stop_from_stages")
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
            fh.write("route_id\tdirection\tstages\trows_read\tmatched\tissue\n")
            for route_id in sorted(runs_by_route, key=route_sort):
                runs = runs_by_route[route_id]
                reason, detail, _d, _h, _p = verdicts.get(route_id, (None, {}, None, None, []))
                fh.write(f"{route_id}\t{spine.get(route_id, (None, None))[0] or ''}\t"
                         f"{len(runs)}\t{len(live_rows[route_id])}\t"
                         f"{detail.get('matched', len(runs) if not reason else 0)}\t"
                         f"{reason or ''}\n")
            fh.write("\n# routes whose stages the spine does not agree about\n")
            for route_id, ours, theirs in disagreements:
                fh.write(f"# {route_id}\n#   ours   {' > '.join(ours)}\n"
                         f"#   spine  {' > '.join(theirs)}\n")
        log(f"per-route detail -> {args.report}")

    if not args.write:
        print()
        print("Dry run: nothing was written. Add --write to write it.")
        return 0

    batch = args.batch or f"backfill {datetime.datetime.now().astimezone().isoformat(timespec='seconds')}"
    sql = build_sql(gtfs, stages, runs_by_route, reviews, issues, args, batch)
    fd, path = tempfile.mkstemp(prefix="backfill_stages_", suffix=".sql")
    os.close(fd)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(sql)
    log(f"sql -> {path} ({len(sql)} bytes)")
    psql(db, ["--single-transaction", "-f", path])
    print()
    print(f"{len(stages)} stages, {sum(len(s.stops) for s in stages)} stage stops, "
          f"{n_runs} route stages, "
          f"{len(reviews)} raised for review.")
    print("Every route is now built from its stages.")
    return 0



if __name__ == "__main__":
    sys.exit(main())
