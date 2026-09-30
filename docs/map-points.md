# What a “map point” is

Short answer: a **map point is not a stop**. It is a row in `gtfs_route_stop`
that carries a position but no `stop_id`, put there so the route's drawn line
follows the road it really takes. No passenger boards there, it is in no GTFS
file, and it exists in no stop table.

Its `stop_type` is `ROUTE CORRECTION`. The dashboard calls it a “map point”
because “route correction” sounds like something is wrong, and nothing is.

## Why it exists at all

You are right that a stop lives in two places: `gtfs_stop` (the stop itself) and
`gtfs_route_stop` (which routes call at it, in what order). A route's line on the
map is drawn by joining its stops in sequence.

That breaks wherever the road does not run straight between two stops. Route 4549
is the clearest case in chennai_bus — between ICF and VILLIVAKKAM KALPANA the bus
goes round a block, but a line drawn straight between those two stops cuts across
it. So two positions are inserted **between** them, with no stop id: the line now
bends the right way, and the route still calls at the same stops it always did.

```
 seq  stop_type           stop_id     name                       lat/lon
   7  INTERMEDIATE STOP   67f6c83a13  ICF                        13.098563, 80.213796
   8  ROUTE CORRECTION    —           ROUTE CORRECTION           13.103540, 80.203080   <- map point
   9  ROUTE CORRECTION    —           ROUTE CORRECTION - return  13.103650, 80.206640   <- map point
  10  INTERMEDIATE STOP   7e82877a88  VILLIVAKKAM KALPANA        13.103093, 80.207655
```

## How the row differs from a stop row

The difference is enforced by the table, not by convention
(`db/gtfs_editor/*.sql`):

```sql
CHECK ((stop_type = 'ROUTE CORRECTION') = (stop_id IS NULL))
CHECK (stop_type <> 'ROUTE CORRECTION' OR (marker_lat IS NOT NULL AND marker_lon IS NOT NULL))
CHECK (stop_name_override IS NULL OR stop_id IS NOT NULL)
```

So the two kinds of row are exact opposites:

| | a stop row | a map point |
|---|---|---|
| `stop_id` | set, and joins to `gtfs_stop` | **always NULL** |
| position | from `gtfs_stop.lat/lon` | from its own `marker_lat`/`marker_lon` |
| name | from `gtfs_stop.name` | its own `marker_name` |
| identity | a stop id shared by every route calling there | `marker_id`, only meaningful on the row |
| in `gtfs_stop` | yes | **no** |
| in the GTFS zip | as `stops.txt` + `stop_times.txt` | **nowhere** |
| counted as a stop call | yes | no |

`stop_type` has five values in all — `NEW STOP` (starts a fare stage),
`INTERMEDIATE STOP`, `JUMP STOP`, `HIDDEN STOP` and `ROUTE CORRECTION`. The last
three are excluded wherever stops are counted or exported, by exactly this
clause, which appears in the route list, the trip builder, the coordinate-review
queue and the dashboard:

```sql
AND rs.stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP')
```

(`src/editor/service.rs:554`, `src/editor/trips.rs:1539`,
`src/editor/position_reviews.rs:558`, and `SERVED_EXCLUDE` in
`editor-ui/js/util.js:208`.)

## Where they came from

They came in with MTC's route data, not from anything the editor did: 1,403 of
the 2,561 are still named `ROUTE CORRECTION` or `ROUTE CORRECTION - return`,
which is what the source called them. The other 1,158 carry a real place name
(`M.G.R.CENTRAL R.S`, `HIGH COURT`, `CENTRAL DENTAL COLLEGE`) — those are points
someone named while shaping the line.

In chennai_bus today:

| | |
|---|---|
| map-point rows in `gtfs_route_stop` (pattern 1) | **2,561** |
| routes carrying at least one | **1,061** of 5,567 |
| distinct `marker_id` | 69 |
| distinct `marker_name` | 42 |
| named just `ROUTE CORRECTION` / `… - return` | 1,403 |
| held inside a stage (`gtfs_stage_stop`) | 102, across 60 stages |

The row count is far larger than the stage count because a stage holds its map
point once and every route running that stage gets a copy when the stages are
flattened into `gtfs_route_stop`.

## The queries

**1. The five kinds of row, and which carry a stop id**

```sql
SELECT stop_type,
       count(*)              AS rows,
       count(stop_id)        AS with_stop_id,
       count(marker_id)      AS with_marker
  FROM gtfs_route_stop
 WHERE gtfs_id = 'chennai_bus' AND pattern_key = 1
 GROUP BY stop_type
 ORDER BY rows DESC;
```

Gives `INTERMEDIATE STOP` 164,943 · `NEW STOP` 58,073 · `ROUTE CORRECTION` 2,561
· `JUMP STOP` 653 — and the map points are the only ones with `with_stop_id = 0`.

**2. Every map point, with where it is**

```sql
SELECT route_id, sequence, marker_id, marker_name, marker_lat, marker_lon,
       stage_no, stage_name
  FROM gtfs_route_stop
 WHERE gtfs_id = 'chennai_bus' AND pattern_key = 1
   AND stop_type = 'ROUTE CORRECTION'
 ORDER BY route_id, sequence;
```

**3. One route read end to end, stops and map points together**

This is the query that shows what they are *for* — swap in any route id:

```sql
SELECT rs.sequence,
       rs.stop_type,
       coalesce(rs.stop_id, '—')                AS stop_id,
       coalesce(s.name, rs.marker_name)         AS name,
       coalesce(s.lat,  rs.marker_lat)          AS lat,
       coalesce(s.lon,  rs.marker_lon)          AS lon,
       rs.stage_name
  FROM gtfs_route_stop rs
  LEFT JOIN gtfs_stop s
         ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id
 WHERE rs.gtfs_id = 'chennai_bus' AND rs.pattern_key = 1
   AND rs.route_id = '4549'
 ORDER BY rs.sequence;
```

The `LEFT JOIN` is the point: a stop row finds its row in `gtfs_stop`, a map
point finds nothing and falls back to its own `marker_*` columns.

**4. Proof that a map point is in no stop table**

```sql
SELECT count(*) AS marker_ids,
       count(s.stop_id) AS also_a_real_stop
  FROM (SELECT DISTINCT marker_id
          FROM gtfs_route_stop
         WHERE gtfs_id = 'chennai_bus' AND stop_type = 'ROUTE CORRECTION') m
  LEFT JOIN gtfs_stop s
         ON s.gtfs_id = 'chennai_bus' AND s.stop_id = m.marker_id;
```

69 marker ids, of which 4 happen to collide with a real stop id — and even those
four are not *used* as stops by these rows, because `stop_id` is NULL on every
one of them.

**5. The routes most shaped by them**

```sql
SELECT route_id, count(*) AS map_points
  FROM gtfs_route_stop
 WHERE gtfs_id = 'chennai_bus' AND pattern_key = 1
   AND stop_type = 'ROUTE CORRECTION'
 GROUP BY route_id
 ORDER BY map_points DESC, route_id
 LIMIT 20;
```

**6. Map points that a stage carries**

A stage holds its stops once and hands them to every route running it, so a map
point inside a stage is repeated into all of them:

```sql
SELECT ss.stage_id, ss.direction, st.name AS stage_name,
       ss.position, ss.marker_name, ss.marker_lat, ss.marker_lon,
       (SELECT count(DISTINCT rs.route_id)
          FROM gtfs_route_stage rs
         WHERE rs.gtfs_id = ss.gtfs_id AND rs.stage_id = ss.stage_id
           AND rs.direction = ss.direction) AS routes_that_get_it
  FROM gtfs_stage_stop ss
  JOIN gtfs_stage st
    ON st.gtfs_id = ss.gtfs_id AND st.stage_id = ss.stage_id
   AND st.direction = ss.direction
 WHERE ss.gtfs_id = 'chennai_bus' AND ss.stop_type = 'ROUTE CORRECTION'
 ORDER BY routes_that_get_it DESC;
```

**7. What a route really calls at (map points excluded)**

The clause every reader uses, so you can see the difference the exclusion makes:

```sql
SELECT count(*) FILTER (
         WHERE stop_type NOT IN ('ROUTE CORRECTION', 'JUMP STOP', 'HIDDEN STOP')
       ) AS stop_calls,
       count(*) AS rows_in_the_line
  FROM gtfs_route_stop
 WHERE gtfs_id = 'chennai_bus' AND pattern_key = 1 AND route_id = '4549';
```

## What this means when you are reviewing a stage

- A map point in a stage's stop list is **not** a stop to be corrected. Leave it
  unless the line is visibly wrong on the map.
- The pencil is disabled on one, because there is no stop to swap it for. Reorder
  or remove are the only things that make sense.
- In *What each route says this stage is*, a map point used to read as a bare
  “map point”, which said nothing about which bend it was. It now reads
  `map point HIGH COURT` where the point has a name. Rows written before that
  change pick it up on the next `--refresh-evidence` run
  (docs/stage-backfill.md).
- A candidate list that differs from another **only** by a map point is not a
  disagreement about where the bus stops. The stop calls are identical; only the
  drawn line differs.
