# Why some stages are called `nm_SAIDAPET`

An RCA of the 548 stages the backfill gave name-based ids to, written after the
29 September 2026 run of `scripts/backfill_stages.py` over `chennai_bus`.

## Short answer

It is **not a coding bug**. `nm_` is a deliberate fallback that fires when the
backfill cannot tell which of MTC's fare stages one of our fare stages is, and
it fired on 76 routes because **our route data and MTC's replica genuinely
disagree about how many fare stages those routes have**.

The *outcome*, though, is a defect worth fixing: 241 of the 548 carry a name
that an MTC-keyed stage already carries, so the same real fare stage now exists
twice — `159` and `nm_SAIDAPET` are both SAIDAPET, and an edit to one does not
reach the routes on the other.

## How a stage gets its id

Three steps, all in `scripts/backfill_stages.py`:

1. **`cut_runs` (:381)** cuts each route's rows in `gtfs_route_stop` into fare
   stages, cutting wherever `(stage_no, stage_name)` changes. Route 300 becomes
   26 runs.
2. **`pair_with_spine` (:552)** lines those runs up against the fare stages the
   replica lists for the same route, **by position**, and only when the two
   lists are the same length. That hands each run MTC's `bus_stop_id` for its
   head.
3. **`stage_key_of` (:515)** keys the stage on `(bus_stop_id, direction)`. Where
   step 2 handed back nothing, it falls back to the stage's own name, and
   **`stage_id_of` (:534)** writes that as `nm_` + the name slugged.

So `nm_` means exactly one thing: *this fare stage was never matched to a fare
stage in the replica, so there was no MTC id to use.*

## Why those 76 routes were not matched

`pair_with_spine` refuses to pair when the counts differ:

```python
if heads and len(heads) == len(runs):
    return list(heads)
...
return [None] * len(runs)
```

5,487 of the 5,563 routes have equal counts and paired. 76 did not:

| ours − replica | routes |
|---|---|
| 3 fewer stages than the replica | 1 |
| 2 fewer | 53 |
| 1 fewer | 12 |
| 1 more | 10 |

Because the refusal is per route, it is all-or-nothing: a route that fails loses
MTC ids for **every** stage on it, not just the one that does not line up. That
is why all 1,243 stage links on those 76 routes are `nm_`, and why no route is
half and half.

### Worked example — route 300

The replica lists 28 fare stages, our rows carry 26. Diffing the two name lists:

```
-FORESHORE ESTATE B.T        +FORESHORE ESTATE
-MANDAVELI B.T               +MANDAVELI
 ... 22 stages identical ...
-THIRUVALLUR  B.T
-THIRUVALLUR GOVT.MEDICAL COLLEGE
-THIRUVALLUR NEW B.T         +THIRUVALLUR
```

Everything in the middle agrees. The route disagrees only at the tail: MTC has
extended route 300 past THIRUVALLUR into three fare stages where our feed still
has one. The head differences (`FORESHORE ESTATE B.T` vs `FORESHORE ESTATE`) are
spellings, and cost nothing — the pairing never reads names.

So the root cause is **data vintage, not logic**: our `gtfs_route_stop` for these
76 routes predates route changes MTC has since made.

## Why it refuses rather than guesses

If the counts differ and it paired positionally anyway, the shorter list would
slide against the longer one and every stage after the first divergence would be
handed **the wrong MTC id** — silently, and permanently, because the id is the
stage's identity. On route 300 that would be 1 or 3 stages mis-keyed depending on
which end it slid from. Mis-keying is far worse than an unfamiliar id: a wrong
`bus_stop_id` merges two different fare stages into one stage, and every route on
either of them inherits the other's stops.

The fallback is the safe failure. It is also **stable**: rerunning over the same
data produces the same `nm_` ids, because the name is not minted or hashed.

## Blast radius

| | |
|---|---|
| Stages with an `nm_` id | **548** (of 4,041) |
| Distinct names among them | 322 |
| Routes affected | **76** (of 5,563), all-or-nothing |
| Stage links on those routes | 1,243 (of 58,768) |
| Direction split | 40 up, 36 down — none blank |
| `nm_` stages whose name is **also** carried by an MTC-keyed stage, same direction | **241** |
| …ignoring case and spacing | 394 |
| Distinct names that are duplicated this way | 132 |
| Review rows raised for `nm_` stages | 548 — 428 `agreed`, 56 `head_duplicate_stops`, 42 `head_differs`, 22 `stretch_differs` |

Of the 343 (`nm_` × MTC) pairs that share a name and direction, only **70 have
the same stop list** and 139 share a head stop — so most of the duplicates are
not harmless twins; they are the same stage as understood by routes of different
vintages.

The 307 `nm_` stages with no MTC-keyed twin are cosmetic: the id is unfamiliar,
but nothing is duplicated and nothing is wrong.

### What it does *not* affect

- **Stop lists are unaffected by the id.** The stops in every stage came from
  our own `gtfs_route_stop`, never from the replica; the replica only supplied
  identity. A stage with an `nm_` id holds exactly the stops its routes gave it.
- **Nothing is lost.** All 76 routes kept their stages, in order, with their
  names.
- **No route mixes id kinds**, so there is no route whose stages are half
  MTC-keyed and half not.

Separately, and not caused by this: 66 of the 76 routes had their stop lists
changed by the backfill (2,100 stop calls before, 2,469 after; 606 invented, 237
lost). That is `--pick longest` doing what it was asked to do across the whole
feed, not an effect of the `nm_` fallback.

## What would fix it

**Option A — align by name instead of refusing (recommended).** Run the two name
lists through `difflib.SequenceMatcher` and take the positions that match
exactly; leave the rest unpaired. Measured against the current data this keys
**997 of the 1,243 links (80%)**, which would take the 548 `nm_` stages down to
roughly 110, and it cannot mis-key: a position is only paired when both sides
carry the same name. Cost: a re-run of the backfill.

**Option B — let the reviewers settle the routes (now built).** The 76 routes are
raised in `gtfs_route_stage_issue` and worked through at **Routes to review**
(docs/gtfs-editor.md section 19.2), each showing our list against MTC's with the
difference marked and the name-keyed stages it caused. This does not remove the
`nm_` ids by itself - it is how a person decides which side is wrong, which is
the thing no script can do.

**Option B2 — let the reviewers merge the stages.** The 241 duplicated stages already
surface in *Stages to review*, and the merge control on the review page moves one
stage's routes onto another. Correct, but it is 241 manual decisions, and a
reviewer cannot tell from the screen that `nm_SAIDAPET` and `159` are the same
stage.

**Option C — refresh the route data and re-run.** The real cure: our
`gtfs_route_stop` for those 76 routes is out of date against MTC. Bringing them
in line makes the counts match and the fallback never fires. Slowest, and it
needs MTC's current route definitions.

A is the sensible next step, and it does not preclude C.

## Reproducing the numbers

```sh
# the 548, and the 241 duplicates
docker exec gims-pg18 psql -U postgres -d mtc_internal_master -c "
  with nm  as (select * from gtfs_stage where gtfs_id='chennai_bus' and stage_id like 'nm@_%' escape '@'),
       mtc as (select * from gtfs_stage where gtfs_id='chennai_bus' and stage_id not like 'nm@_%' escape '@')
  select count(*) nm_stages,
         count(*) filter (where exists (select 1 from mtc m
                                        where m.name = n.name and m.direction = n.direction)) twins
  from nm n;"

# the 76 routes, all-or-nothing
docker exec gims-pg18 psql -U postgres -d mtc_internal_master -c "
  with per as (select route_id, count(*) n,
                      count(*) filter (where stage_id like 'nm@_%' escape '@') nm
               from gtfs_route_stage where gtfs_id='chennai_bus' group by 1)
  select count(*) filter (where nm = n) all_nm,
         count(*) filter (where nm > 0 and nm < n) part_nm,
         sum(nm) nm_links from per;"
```

The per-route count comparison needs the replica export that the run used
(`route_id, route_direction, route_order, stage_no, stage_name, bus_stop_id`,
exactly as `--print-replica-sql` prints it); `spine_from_rows` in the script
turns it into the list this document compares against.
