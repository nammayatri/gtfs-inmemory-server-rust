# Release: the whole GTFS reference in the editor, and every feed in the tables

PR #219 (`feat/gtfs-full-spec`), and the dashboard's **New feed from a GTFS
zip** that followed it (section 18.14). The design and the reasons are
`docs/gtfs-editor.md` sections 15, 16 and 18; this is what to do, in order, to
ship it to master and then prod, and to move the feeds into the editor's
tables.

## What ships

- **Feed access (section 15).** An admin works on every feed; everyone else only
  on the feeds granted to them, at the role the grant gives.
- **Trips, stop times and calendars in the tables (section 16).** Trips, timing
  profiles, stop orders and services, edited through drafts; GIMS serves a
  feed's trips from the tables once its `trips_source` is `db`.
- **The whole GTFS reference (section 18).** Every file and field of the GTFS
  Schedule reference, GTFS-Flex included, stored, edited through drafts and
  exported; a feed loaded from its zip (the dashboard's **New feed from a GTFS
  zip**, or `gtfs_feed import --seed`), a feed that
  has rows brought up to a zip through drafts (`gtfs_feed draft-import`), the
  feed report (`GET /feeds/{g}/validation`), and the dashboard's Feed data menu
  (Feed, GTFS files, Calendar) and Trips and timing page.
- **The image carries `/app/gtfs_feed`**, the tool the steps below run.

## Read this first

1. **Master and prod each have their own editor database** (master's is
   `mtc_internal_master`). Steps 1 to 6 are done once per environment: all of
   them on master first, then on prod. Nothing done on master's database
   changes what prod serves. Seeding a feed changes nothing the environment
   serves; switching a feed's `data_source` or `trips_source` changes what that
   environment serves within seconds.
2. **Switch a feed to the tables only where this image runs.** The loader
   before it does not know what a feed loaded from its zip needs (a route named
   by its own agency, every stop served, split stop orders, the feed's own stop
   numbering), so it would serve such a feed wrongly.
3. **Feed access narrows who can edit.** `0018` gives every non-admin a grant on
   `chennai_bus` at the role they hold today, and nothing else. After the
   release an admin grants the other feeds on the People page.
4. **Nandi.** GIMS serves an edit within seconds; Nandi (OTP) only gets it through
   its own build. For `chennai_bus`, nandi's release builds from the editor
   tables; with this release it can build the whole zip with `gtfs_feed export`
   once the feed's trips are in the tables - the nandi branch
   `feat/gtfs-exporter-release`, not merged yet. For every other feed, nandi
   still builds from the zip in `nandi/assets`, so **an edit to one of those
   feeds reaches GIMS but not Nandi** until its release is switched the same
   way. Decide per feed whether that is acceptable before people edit it.

## 1. Migrations - the environment's editor database, before its image

Find what the database already has with this read-only migration check; it
prints `t` for each migration that is in and `f` for each that is not:

```sql
SELECT m.migration, m.applied FROM (VALUES
  ('0009_feed_config_change', (SELECT pg_get_constraintdef(oid) LIKE '%feed_config%' FROM pg_constraint WHERE conname = 'gtfs_change_entity_check')),
  ('0010_self_approval',      EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'gtfs_change_set' AND column_name = 'self_approved')),
  ('0011_context_indexes',    to_regclass('gtfs_audit_log_stop_idx') IS NOT NULL),
  ('0012_stop_description',   EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'gtfs_stop' AND column_name = 'description')),
  ('0013_webhooks',           to_regclass('gtfs_webhook') IS NOT NULL),
  ('0016_webhook_settings',   to_regclass('gtfs_webhook_settings') IS NOT NULL),
  ('0017_stop_headsign',      EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'gtfs_route_stop' AND column_name = 'stop_headsign')),
  ('0018_feed_access',        to_regclass('gtfs_editor_feed_access') IS NOT NULL),
  ('0019_trips',              to_regclass('gtfs_trip') IS NOT NULL),
  ('0021_polyline_source_gps',(SELECT pg_get_constraintdef(oid) LIKE '%gps%' FROM pg_constraint WHERE conname = 'gtfs_route_polyline_source_check')),
  ('0022_release_requested',  EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'gtfs_webhook_delivery' AND column_name = 'target')),
  ('0023_gtfs_full_spec',     to_regclass('gtfs_agency') IS NOT NULL)
) AS m(migration, applied);
```

Apply each one that says `f`, in number order, from `db/gtfs_editor/` of the
merged commit:

```bash
psql "$EDITOR_DB" -v ON_ERROR_STOP=1 -f db/gtfs_editor/0017_stop_headsign.sql   # and so on, in order
```

- New in this PR: `0017`, `0018`, `0019`, `0023`. `0022` came with #217.
- Every migration is safe to run twice, but apply only the missing ones: an
  early one such as `0008` backfills settings that may have been changed since.
- `0023` is additive: new columns, 25 new tables, and CHECKs relaxed, never
  tightened. The image running now keeps working on it.
- **`0012`, `0017` and `0023` must be in before the new image starts**: the
  loader selects their columns, and a DB feed that fails to load falls back to
  its preprocessed data.

## 2. Deploy the image

1. **Master.** Its dhall needs `is_master = True` (from #217), so its Release
   to Nandi goes to master Nandi; the editor settings are unchanged. Apply that
   config map before the service release. Check:
   - the pods report ready, and `/routes/chennai_bus` and a few other feeds answer as before;
   - the dashboard opens; the top bar has **Feed data** (Feed, GTFS files, Calendar);
   - `chennai_bus`'s Feed page: "Check the feed" returns a report, and "Download the GTFS zip" gives a zip.

   Then steps 3 to 6 on master.
2. **Prod.** Step 1 on prod's database, then the same image with `is_master`
   `False` (or left out). Check the same, then steps 3 to 6 on prod.

Going back: the previous image runs on the migrated database; do it before any
feed has been switched to the tables (step 6), or switch those back first.

## 3. Grants

On the People page, give the people who will review the moved feeds their role
on each of them (approver for those who commit). Admins need none.

## 4. Load the feeds into the tables - nothing served changes

The zips are the ones in `nandi/assets/` at the commit prod ships (the table
below).

**On the dashboard** (an admin; needs the image with section 18.14): Feed
settings, or the New menu, **New feed from a GTFS zip**. Choose the zip,
**Check without writing**, and when it says the feed gives the zip back with
0 errors, **Make feed {id}**. The new feed then opens on its Feed page. One zip
at a time; the largest here, delhi_metro, took 16 s locally (a debug build).
If the page reports a timeout from the proxy instead, reload before trying
again: the seed may have committed after the proxy gave up, and then the feed
is in the switcher (a second try is refused as `feed_not_empty`, harmlessly).

**Or from the command line**, in a pod or on any machine that reaches the
database (`cargo build --release --bin gtfs_feed`). The URL carries no password;
`gtfs_feed` reads it from `PGPASSWORD`.

```bash
export PGPASSWORD=...                          # the editor database's
DB=postgres://<user>@<host>:<port>/<database>  # no password in the URL
/app/gtfs_feed import --db "$DB" --zip /tmp/chennai.metro.gtfs.zip            # dry run
/app/gtfs_feed import --db "$DB" --zip /tmp/chennai.metro.gtfs.zip --seed     # writes
```

A dry run writes nothing. Seed only when it shows `"errors": 0` and
`"round_trip": {}` - the feed exported again from the tables is the zip,
exactly. The seed then does it again in its own transaction and commits only if
nothing differs; it refuses a feed that already has rows or an open draft.

| zip in `nandi/assets` | gtfs_id | sha256 (first 12) | trips | kept with a warning |
|---|---|---|---:|---|
| `BMRC.gtfs.zip` | `bangalore_metro` | `9a48aaaed43f` | 2,722 | - |
| `bhubaneswar.bus.gtfs.zip` | `bhubaneshwar_bus` | `f455af15068e` | 8,203 | - |
| `chennai.metro.gtfs.zip` | `chennai_metro` | `42dea33927df` | 4,292 | a pathway `is_bidirectional` "1x" (row left out), a time `-1:59:35` (cell left out), 2,775 trips with a repeated `stop_sequence` |
| `chennai.suburban.gtfs.zip` | `chennai_suburban` | `87ad22fc2a8a` | 807 | 53 trips whose times go backwards, 5 with a repeated `stop_sequence`, 4 one-stop trips |
| `delhi.bus.temp.gtfs.zip` | `delhi_bus_nammayatri_application_mock` | `bafd0cd3a5ac` | 24 | - |
| `delhi.metro.gtfs.zip` | `delhi_metro` | `f32d3e9eef8b` | 21,825 | - |
| `kochi.metro.gtfs.zip` | `kochi_metro` | `23ec132d2b50` | 450 | - |
| `kolkata.bus.gtfs.zip` | `kolkata_bus` | `ba1250083195` | 60 | - |
| `kolkata.metro.gtfs.zip` | `kolkata_metro` | `75b8bef0bc72` | 1,652 | 55 stops that are their own parent station, 1 trip whose times go backwards |
| `mumbai_MMMOPL.metro.gtfs.zip` | `mumbai_MMMOPL_metro` | `4c97de5e451d` | 2 | - |
| `mumbai_MMOCL.metro.gtfs.zip` | `mumbai_MMOCL_metro` | `e3feb4946744` | 8 | - |
| `mumbai_MMRCL.metro.gtfs.zip` | `mumbai_MMRCL_metro` | `7d6859dbe926` | 2 | - |
| `mumbai.suburban.gtfs.zip` | `mumbai_suburban` | `60b52c97d984` | 134 | - |
| `sambalpur.bus.gtfs.zip` | `sambalpur_bus` | `71a4b2879116` | 449 | `feed.txt`, not a GTFS file (left out) |
| `amsterdam.gtfs.zip` (nandi's Amsterdam branch) | `amsterdam` | `e935a2501490` | 142,711 | seeded locally; only if it ships |

- These are the zips the local run seeded and checked for parity: every one
  round-tripped with 0 differences. The seed's audit row records the zip's
  sha256 (`SELECT gtfs_id, detail->>'zip_sha256' FROM gtfs_audit_log WHERE action = 'seed'`).
  The same prefix as the table means the same rows as were checked.
- A feed with no `gtfs_feed` row gets one, named after its agency, with
  `data_source` and `trips_source` `preprocessed`: GIMS keeps serving its
  preprocessed data until step 6.
- `chennai_bus` is not seeded: it has rows. It is step 5.
- `/app/gtfs_feed validate --db "$DB" --gtfs-id <g>` lists what a feed breaks of
  the reference. For these feeds that is the warnings in the table, unused stops
  and routes, and feed_info end dates in the past (chennai_metro, kochi_metro,
  mumbai_suburban).

## 5. chennai_bus: its agency, calendars and trips, through drafts

`chennai_bus` has been edited through drafts since it was seeded from the MTC
mapping, so its zip comes in as change sets, reviewed like any other. Its stops,
routes and stop orders are compared, never written.

```bash
/app/gtfs_feed draft-import --db "$DB" --zip /tmp/chennai.bus.gtfs.zip --as <editor email>            # dry run
/app/gtfs_feed draft-import --db "$DB" --zip /tmp/chennai.bus.gtfs.zip --as <editor email> --write    # set 1
```

`--as` is an active editor account: the drafts are theirs, and a second person
approves them.

1. The dry run reports `"step": "records"`, `"errors": 0`, and `stop_orders`:
   `same` (stop orders the editor has as the zip does), `moved` (a route whose
   stop order the editor has since changed; its trips, all on the default
   timing, go onto the editor's stop order) and `routes_left` (kept as they are,
   named in the findings). Locally: 3,811 same, 517 moved, 0 left.
2. `--write` makes set 1, "GTFS import …: records, calendars and stop orders":
   the agency's URL, timezone and language, feed_info, the two services
   (locally 4 changes). Approve and commit it in the dashboard.
3. Run the same `--write` again: it makes the trip sets, at most 5,000 trips each
   (locally 11 sets, 52,606 trips). Approve and commit each.
4. Run it again: `"step": "none"`, nothing left to draft.
5. Check: `/app/gtfs_feed export --db "$DB" --gtfs-id chennai_bus --out /tmp/cb.zip`
   then `/app/gtfs_feed compare --a /tmp/chennai.bus.gtfs.zip --b /tmp/cb.zip`.
   `agency`, `feed_info`, `calendar` and `trips` must not differ; `stop_times`
   only on the routes the report called `moved`.

The trips sit in the tables unused while `trips_source` is `preprocessed`. Each
commit moves the feed's version, so nandi's release (`--if-changed`) builds
chennai_bus again - with its generator, as today.

## 6. Switch the feeds to the tables - one at a time, where the image runs

For each feed, as an admin, with the feed chosen in the top bar, on **Feed
settings** (the admin menu):

1. In the feed's row, **Add to draft: switch to database (live edits)**
   (`data_source = db`). A second person approves and commits the draft. The
   environment's GIMS reloads the feed within `gtfs_version_poll_seconds`.
2. Check the feed: `/routes/{g}`, `/stops/{g}`, a few
   `/route/{g}/{r}` and `/route-stop-mapping/{g}/route/{r}`. Locally, parity
   between the preprocessed load and the tables was identical for all 14 feeds
   except three things the tables serve and the preprocessed build leaves out:
   route `color`, stop `description` (`stop_desc`), and a stop's `platform` /
   `platformCode` (`platform_code`).
3. In the **Trips from** column, **Switch to database (live edits)**
   (`trips_source = db`), in its own draft, committed the same way. Check
   `/example-trip/{g}/{r}` and `/trip/{id}?gtfs_id={g}` for a few trips.

Start with a small feed (`mumbai_MMRCL_metro`, `kochi_metro`), then the metros,
then the buses.

**chennai_bus** is already served from the tables (`data_source = db`); only its
`trips_source` moves, and **only after** step 5 is committed, the nandi branch
`feat/gtfs-exporter-release` is merged and nandi's Jenkins image carries
`gtfs_feed` (as `GTFS_FEED_BIN`). From then on nandi's release builds the whole
chennai_bus zip from the tables, and keeps the calendar window starting on the
build date as the generator does.

## Going back

- **A feed:** the same Feed settings switch the other way, in a draft - it serves
  its preprocessed data again within seconds. Its rows stay in the tables.
- **The image:** switch the moved feeds back first, then roll back. The migrations
  stay; they are additive.
- **A seeded feed not yet switched:** nothing to undo. It serves as before.

## Go / no-go

Master first, then prod, each on its own database:

- [ ] The migration check (step 1) says `t` for every row.
- [ ] Master on the new image with `is_master = True`; prod on it with `False`.
- [ ] Reviewers granted on the feeds they review.
- [ ] Each feed seeded after a check that gave the zip back with 0 errors; its sha256 matches the table.
- [ ] chennai_bus set 1 and every trip set committed; `compare` as step 5.
- [ ] Per feed switched: its checks passed in that environment.
- [ ] For chennai_bus `trips_source`: the nandi branch merged and `gtfs_feed` in nandi's image.
