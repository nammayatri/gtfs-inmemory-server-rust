# relations

A checked-in mirror of the schema of the two Postgres databases GIMS talks to.

## Layout

```
relations/
  external_replica/                # master/replica DB  (config: database_url)
  internal/                        # internal DB        (config: internal_database_url)
    bus_schedule_internal/
      schema.sql                   # the bare CREATE TABLE
      migrations/
        0001_bus_schedule_internal_pkey.sql
        0002_add_gtfs_id_col.sql   # <- yours go here
      indexes/
        0001_idx_bus_schedule_gtfs_schedulenum.sql
        0002_idx_bus_schedule_schedulenum.sql
```

The directory names are fixed (`external_replica`, `internal`) rather than the
live database names, so the tree is the same whichever environment the script
is pointed at.

## What lives where

- **`schema.sql`** - the `CREATE TABLE` and nothing else. Regenerated on every
  sync, so never edit it by hand.
- **`migrations/`** - one numbered file per `ALTER TABLE`: primary keys, unique
  and check constraints, foreign keys, and the column defaults that Postgres
  stores separately from the table definition. This is also where you add your
  own changes.
- **`indexes/`** - one numbered file per `CREATE INDEX`.

Files under `migrations/` and `indexes/` are append-only. Once a file exists the
script never rewrites it or renumbers it, so a number that has been applied
stays put. If the database drifts from what a file says, the sync reports
`differs from the database` and leaves the file alone - correct it with a new
migration rather than by editing the old one.

## Syncing

```
./scripts/sync_relations.py --config dhall-configs/dev/<your-config>.dhall
./scripts/sync_relations.py --config <cfg> --dry-run     # report only
```

Needs `python3` and `pg_dump`/`psql` on PATH, and no pip packages - see
`scripts/requirements.txt`.

## Adding a change

Add a file to the right directory, numbered from the highest one already there:

```
relations/internal/route_internal/migrations/0002_add_gtfs_id_col.sql
relations/internal/route_internal/indexes/0003_route_internal_gtfs_id_idx.sql
```

- One logical change per file. Forward-only; no down-migrations.
- Four-digit prefix, then a short snake_case description.
- Plain SQL, runnable as-is against that database.
- Once it is applied, re-run the sync so `schema.sql` catches up, and commit
  both together.

## Scope

Base tables in the `public` schema. Sequences, triggers, views, materialised
views and functions are not mirrored - this is a reference for reading and
reviewing the schema, not a script that rebuilds a database from scratch.
