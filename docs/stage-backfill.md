# Backfilling stages from the stop lists we already have

Turning today's routes into routes built from stages (docs/gtfs-editor.md
section 19) needs almost no new data. `gtfs_route_stop` already holds every stop
of every route in order, with the fare stage number and stage name on each row.
The one thing it does not hold is which way along a corridor a route runs, and
that comes from MTC's replica.

`scripts/backfill_stages.py` does it, and is a dry run unless told otherwise.

All numbers below are `chennai_bus` as it stands in `mtc_internal_master`,
measured on 2026-09-29: 101,772 rows, 5,567 routes, 58,790 fare stages.

## The model, whole

**One stage per fare-stage name and direction.** SAIDAPET on the up routes is
one stage, SAIDAPET on the down routes another. 3,306 stages come out of 58,790
fare stages.

**Its stops are the list the most routes give it.** `gtfs_route_stop` was filled
route by route, so the routes do not always agree: one lists 10 stops for a
stage, the next 9, a third only the fare boundary. There is no way to tell from
the data alone which is right, so the backfill takes what most routes say - and
treats it as the guess it is.

**Every name the routes disagree about is raised for review**, carrying every
list they give. A person settles it in the dashboard (section 19.1), builds the
stage's real stop list there, and that goes into a draft. Committing the draft
is what updates the stop mapping for every route using the stage.

### Which list to take

| `--pick` | stop calls | change | stage stops |
|---|---|---|---|
| **`commonest`** (default) | 88,422 | **−13,350** | 5,166 |
| `longest` | 232,862 | **+131,090** | 7,755 |

`longest` is the rule as first stated - the stage gets the most stops anyone
gives it. It more than doubles the feed, because most routes record only the fare
boundary and one route's full list is then handed to all of them: 5,542 routes
would claim stops they do not call at until a person had been through 2,000
reviews. `commonest` is the list most routes actually have, so it is much the
closer starting point. Neither is right until a person has looked.

Holding the stops back until review was considered and does not work: only **14
of 5,567 routes** have no disputed stage, so nothing would be built from stages
at all until the whole queue was done.

## What is read

| table | what is taken from it |
|---|---|
| `gtfs_route_stop` | every row of every live route in `sequence` order: `stop_id`, `stop_type`, `stage_no`, `stage_name`, `stop_name_override`, the marker columns, and `provider_id`/`provenance` for the rewrite |
| `gtfs_stop` | names only, to tell a duplicate stop record from a different place |
| the replica's `bus_route`, `bus_route_point`, `bus_stop` | each route's `route_direction` (UP / DOWN), and the order and names of its fare stages as a cross-check |

### The replica side

`bus_route_point` holds **one row per fare stage**, not one per stop: 60,180
points over 6,172 routes. Its `fare_stage` column is `'Y'` on every row, so there
is nothing to filter on. `bus_stop.bus_stop_name` is the stage's name.

The bridge to our own rows is the route and the stage's name:

```sql
SELECT * FROM gtfs_route_stop
 WHERE route_id = <the replica's route_id>
   AND stage_name = <the bus_stop_name of the stage>
```

which holds because the editor's `route_id` *is* the replica's `route_id`.
Checked on the routes they share: **5,487 of 5,567 (98.6%) have the same stages
in the same order**. Of the rest, 4,306 routes differ only in how a name is
written - the replica's are fuller (`KELAMBAKKAM` / `KELAMBAKKAM B.T`,
`GURUNANAK COLLEGE` / `GURU NANAK COLLEGE`, 191 such names) - and only 76 have a
different set or order of stages. The backfill keeps **our** spelling; adopting
MTC's is a separate change.

The replica's `bus_stop_id`s cannot be used for the stops themselves: they are
MTC integers with no bridge to the editor's stop ids. Only the direction and the
cross-check come from there.

`--no-replica` leaves every direction NULL, which merges the two directions of a
corridor into one stage. It is for trying a run out, not for a real backfill.

### Why `commonest` and not `longest`

Both raise **the same 2,170 reviews**: a name is raised when the routes disagree
about it, whichever list is then picked. So `longest` buys no review effort, it
only changes how wrong the feed is meanwhile.

| | fare stages exactly right | stops invented | stops lost | total wrong |
|---|---|---|---|---|
| `commonest` | 25,990 of 58,790 (44%) | 40,126 | 53,476 | 93,602 |
| `longest` | 10,336 (18%) | 182,559 | 51,469 | 234,028 |

*Invented* is a stop a route now claims to call at and does not - a passenger
waiting for a bus that will not stop. *Lost* is one it does call at that the feed
no longer lists. Nothing is made up either way: every stop in a stage comes from
some route's own rows. The question is only which routes it is then applied to.

M.G.R.KOYAMBEDU is used by 277 routes, which give it nine different lists: 116
say it is the one boundary stop, 28 say it runs thirteen stops out to Vandalur
Zoo. `longest` hands all 277 the Vandalur list - 3,120 stops added to routes that
never went there, from that one stage. `commonest` hands them the boundary: 132.

## Where a fare stage begins

A fare stage is what `stage_no` and `stage_name` say it is, so the cut follows
those: a run ends where either changes. Cutting at every `NEW STOP` instead,
which is what `ensure_normal_list` does, is **not** the same thing - a fare stage
can begin at a `JUMP STOP` (302 do) or a `ROUTE CORRECTION` (137), and cutting
only at `NEW STOP` swallows those into the stage before. The run counts every
such stage so they can be looked at.

## What is written

| table | one row per |
|---|---|
| `gtfs_stage` | stage: its stops, its name, its direction, and why it is under review |
| `gtfs_stage_stop` | stop of a stage, in `position` order |
| `gtfs_route_stage` | fare stage of a route: the route, its position, the stage, the route's `stage_no` |
| `gtfs_stage_review` | stage name the routes disagree about, with every list they give |
| `gtfs_route_stop` | rewritten for the routes whose stages say something else |
| `gtfs_audit_log` | one row, `stages_backfilled`, with the counts |

### Why `gtfs_route_stop` has to be rewritten too

A stage edit refuses a route whose live stop list is not what its stages flatten
to - `route_out_of_sync` in `src/editor/stages.rs`. A backfill that left the two
disagreeing would not leave a working feed: it would leave every stage
uneditable. So the routes it changes are rewritten in the same transaction, the
way the editor itself would - the route keeps its usual `provider_id` and each
row keeps its `provenance` only where the same stop stays at the same sequence,
which is what `write_pattern_rows` does.

Routes carrying per-row GTFS fields a stage cannot hold - `pickup_type`,
`timepoint`, `stop_headsign` and the rest - cannot be rewritten from their
stages. The run refuses if it finds any. `chennai_bus` has none.

## What it refuses to run over

- a feed that already has stages, unless `--reset`
- a route running a temporary route: a backfill cannot tell which list is normal
- an open change set holding a `route_stops`, `stage`, `route_stages` or
  `route_variant` change (`--force` overrides)

A dry run reports these and carries on; only `--write` refuses.

## Names are matched exactly

Two stage names are the same name when the strings are the same, once the spaces
around them are taken off. Nothing else is folded: `CONCORD` and `concord` are
two names, and so are `CIT NAGAR` and `CIT  NAGAR`. A stage is whatever MTC's
data says it is, and it is not this script's place to decide that two spellings
mean one place — where they really do, a person merges them in the dashboard and
that decision is recorded. Folding here would make the same decision silently,
for every name at once, with nothing left to review.

**The name is not what divides fare stages.** The cut follows `stage_no` alone.
30 routes spell one fare stage's name two ways part way through it (route 1012
writes `pallavaram` then `PALLAVARAM` inside stage 11); cutting on the name would
split those into two stages, which is not what MTC says, and it changes the
route's stage count so it stops lining up with the replica — 24 routes lost every
MTC id that way. It is safe to ignore the name here because a stage number never
carries two genuinely different names: the only four places where one does are
all `m.m.d.a.colony rdjn` against `m.m.d.a.colony rd.jn`, a missing full stop.

### Rerunning is reading its own output

A run rewrites `gtfs_route_stop`, so a second run over the same database reads
back what the first one wrote: every route now agrees about every stage, nothing
is raised, and the numbers look wonderful and mean nothing. `--reset` does not
help — it drops the stages and leaves the rewritten rows. The script checks
`gtfs_audit_log` and refuses. The only honest rerun is over the feed as it was.

That audit row cannot be deleted — `gtfs_audit_log` is append-only, enforced by a
trigger — so a restored feed is run again with **`--rerun`**, which says the rows
have been put back from a copy taken beforehand. Putting them back means
restoring `gtfs_route_stop` and clearing what the run writes:

```sh
# the feed as it was, from a copy taken before the first run
docker exec gims-pg18 psql -U postgres -d mtc_internal_before_stages \
  -c "\copy (SELECT * FROM gtfs_route_stop WHERE gtfs_id='chennai_bus' AND pattern_key=1) TO '/tmp/rs_before.csv' WITH CSV"

docker exec -i gims-pg18 psql -U postgres -d mtc_internal_master -v ON_ERROR_STOP=1 <<'SQL'
BEGIN;
DELETE FROM gtfs_route_stop WHERE gtfs_id = 'chennai_bus' AND pattern_key = 1;
\copy gtfs_route_stop FROM '/tmp/rs_before.csv' WITH CSV
DELETE FROM gtfs_route_stage       WHERE gtfs_id = 'chennai_bus';
DELETE FROM gtfs_stage_stop        WHERE gtfs_id = 'chennai_bus';
DELETE FROM gtfs_stage             WHERE gtfs_id = 'chennai_bus';
DELETE FROM gtfs_stage_review      WHERE gtfs_id = 'chennai_bus';
DELETE FROM gtfs_route_stage_issue WHERE gtfs_id = 'chennai_bus';
COMMIT;
SQL
```

`--reset` is not enough on its own: it drops the stages and leaves the rewritten
`gtfs_route_stop` rows, which are the ones that must come back.

## Putting something back into the evidence

The lists the routes gave a stage exist nowhere after a run but in the review
rows: the feed itself has been flattened. So when something is found to be
missing from that evidence — it once held only the first twelve routes of a
list, which was not enough to move a list onto a stage of its own — it is not a
reason to redo the backfill. `--refresh-evidence URL` reads the feed as it was
from `URL`, chooses the stages again exactly as the run did, and writes the new
evidence over the old in `--db`:

```sh
python3 scripts/backfill_stages.py \
  --db postgres://.../mtc_internal_master \
  --refresh-evidence postgres://.../mtc_internal_before_stages \
  --replica-csv ~/Downloads/query_result.csv \
  --pick longest --gtfs-id chennai_bus --write
```

Nothing else is touched: not a stage, not a route's stage links, not a review's
status, note or draft. Because the stages are chosen from the same input in the
same way, `reason` and `impact` come out identical, which is also the check — if
the rows worked out do not line up one for one with the rows stored, the source
is not what the run read and nothing is written.

## How it is checked

The backfill is correct when the stages say exactly what the feed says. For every
route:

```
flatten(its gtfs_route_stage -> gtfs_stage -> gtfs_stage_stop)
    ==  its gtfs_route_stop rows
```

row by row - the same comparison as `flatten_route` / `same_rows` in
`src/editor/stages.rs`, which is what a stage edit uses. Run against
`mtc_internal_master`: **88,422 rows flattened, 0 rows differing either way**,
no row left without a `provider_id`.

`--self-test` checks the choosing itself: one stage per name, the commonest list
winning, `--pick longest` taking the longest, a tie going to the longer list,
every route of the name getting that one list, each review reason, the candidate
lists put in front of the reviewer, and a stage beginning at a `JUMP STOP` being
cut where it should be.

## After it runs

### The queue is worked by weight

2,037 names are raised, which nobody works through by hand. They do not matter
equally: each carries `impact`, the number of stop calls the guess gets wrong
over all the routes using it, and **776 of the 2,037 change 20 calls or more and
hold 91% of the whole difference**. The dashboard opens on those. The rest are
one click away and can be left as they are with little cost.

## After it runs

Every route is a route **built from stages**: its stops are edited only through
them, `route_stops/replace` answers `route_has_stages`, and one reviewed stage
edit changes every route that uses it. 2,037 stage names wait for a person in
*Stages to review* (section 19.1), 776 of them worth the time.
