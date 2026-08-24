#!/usr/bin/env python3
"""Dump the live schema of both GIMS databases into ./relations.

Layout produced
---------------
    relations/
      README.md
      <database>/
        <table>/
          schema.sql          <- generated, do not hand-edit
          0001_something.sql  <- hand-written migrations, never touched

GIMS talks to two Postgres databases (see `database_url` and
`internal_database_url` in the dhall config): the master/replica DB and the
internal DB. This dumps the public schema of each and files every object under
the table it belongs to, at `relations/<db>/<table>/schema.sql`.

Only `schema.sql` is ever written. Anything else already sitting in a table's
directory - contributor migrations like `0001_add_gtfs_id_col.sql` - is left
alone.

One pg_dump per database, not per table: the databases are remote, and a
connection + catalog scan per table made a full sync take minutes.

Requires pg_dump/psql on PATH and network access to both databases. No
third-party Python packages.

Examples
--------
  # Refresh everything from a dhall config.
  ./scripts/sync_relations.py --config dhall-configs/dev/gtfs_in_memory_server_rust.dhall

  # Explicit URLs instead of a config.
  ./scripts/sync_relations.py \
      --db-url postgres://user:pw@host:5432/mtc_master_prod_new \
      --internal-db-url postgres://user:pw@host:5432/mtc_internal

  # See what would change without writing.
  ./scripts/sync_relations.py --config <cfg> --dry-run

  # One database, one table.
  ./scripts/sync_relations.py --config <cfg> --only internal --table route_internal
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit

REPO_ROOT = Path(__file__).resolve().parent.parent

# Session boilerplate pg_dump wraps the output in; noise in a checked-in file.
# The \restrict/\unrestrict pair (pg_dump >= 18) carries a random token, so
# keeping it would make every run produce a spurious diff.
_DROP_PREFIXES = (
    "SET ",
    "SELECT pg_catalog.set_config",
    "\\connect ",
    "\\restrict ",
    "\\unrestrict ",
)

# "-- Name: route_name_idx; Type: INDEX; Schema: public; Owner: -"
_BANNER = re.compile(
    r"^-- Name: (?P<name>.+); Type: (?P<type>[^;]+); Schema: (?P<schema>[^;]+); Owner:"
)

# Types whose banner name is "<table> <object>".
_TABLE_PREFIXED = {
    "DEFAULT",
    "CONSTRAINT",
    "CHECK CONSTRAINT",
    "FK CONSTRAINT",
}
# Types that belong to a table but need the body inspected to say which.
_BODY_SCOPED = {"INDEX", "INDEX ATTACH"}
# Everything a table's directory should contain. Sequences and triggers are
# deliberately out of scope; so are views, functions and extensions.
_TABLE_SCOPED = {"TABLE"} | _TABLE_PREFIXED | _BODY_SCOPED

# Which subdirectory each object type is filed under.
# The two databases are mirrored under fixed directory names, so the tree is
# stable no matter which environment the script is pointed at.
DB_DIRS = {"master": "external_replica", "internal": "internal"}

_INDEX_DIR = "indexes"
_MIGRATION_DIR = "migrations"


def log(msg: str) -> None:
    print(msg, file=sys.stderr)


def die(msg: str) -> "NoReturn":  # noqa: F821
    log(f"error: {msg}")
    sys.exit(1)


# ---------------------------------------------------------------- config


def resolve_config(raw: Path) -> Path:
    """Accept a config path relative to the cwd or to the repo root.

    Lets the script work the same from `scripts/` as from the repo root.
    """
    if raw.exists():
        return raw
    if not raw.is_absolute():
        from_root = REPO_ROOT / raw
        if from_root.exists():
            return from_root
    die(f"{raw}: no such file (tried it relative to the cwd and to {REPO_ROOT})")


def urls_from_dhall(path: Path) -> "tuple[str, str]":
    """Pull database_url / internal_database_url out of a dhall config.

    These files are not always single-assignment (aws_prod_new.dhall carries an
    internal-IP block and a public-IP block), so a key with two different values
    is ambiguous and we refuse to guess.
    """
    text = path.read_text()
    out = []
    for key in ("database_url", "internal_database_url"):
        found = re.findall(
            rf'^\s*{key}\s*=\s*Some\s+"([^"]+)"', text, flags=re.MULTILINE
        )
        distinct = list(dict.fromkeys(found))
        if not distinct:
            if re.search(rf"^\s*{key}\s*=\s*None\b", text, flags=re.MULTILINE):
                die(
                    f"{path}: `{key}` is `None Text` - that config points at no "
                    "database. Use a config that sets it, or pass the url explicitly."
                )
            die(f'{path}: no `{key} = Some "..."` found')
        if len(distinct) > 1:
            log(f"error: {path}: `{key}` is set to {len(distinct)} different values:")
            for value in distinct:
                log(f"  {redact(value)}")
            die("pass --db-url/--internal-db-url explicitly to pick one")
        out.append(distinct[0])
    return out[0], out[1]


def normalise(url: str) -> str:
    """GIMS configs use the `psql://` scheme; libpq only knows postgres[ql]://."""
    if url.startswith("psql://"):
        url = "postgresql://" + url[len("psql://") :]
    if not url.startswith(("postgres://", "postgresql://")):
        die(f"not a postgres url: {redact(url)}")
    # A raw '@' or '/' in an un-encoded password silently reconnects you to the
    # wrong host, so refuse rather than dump the wrong database.
    authority = url.split("://", 1)[1].split("/", 1)[0]
    if authority.count("@") > 1:
        die(
            f"url {redact(url)} has an un-encoded '@' in the password; "
            "percent-encode it (@ -> %40, $ -> %24, ...)"
        )
    return url


def redact(url: str) -> str:
    return re.sub(r"://([^:/@]+):[^@]*@", r"://\1:***@", url)


def db_name(url: str) -> str:
    name = urlsplit(url).path.lstrip("/")
    if not name:
        die(f"url {redact(url)} has no database name")
    return name


# ---------------------------------------------------------------- postgres


def run(cmd: "list[str]", url: str) -> str:
    env = dict(os.environ)
    # Keep pg_dump/psql from pausing on a password prompt in CI.
    env.setdefault("PGCONNECT_TIMEOUT", "15")
    try:
        proc = subprocess.run(
            cmd + [url], capture_output=True, text=True, env=env, check=False
        )
    except FileNotFoundError:
        die(f"{cmd[0]} not found on PATH")
    if proc.returncode != 0:
        die(f"{cmd[0]} failed for {redact(url)}:\n{proc.stderr.strip()}")
    return proc.stdout


def list_tables(url: str) -> "list[str]":
    sql = (
        "SELECT tablename FROM pg_tables "
        "WHERE schemaname = 'public' ORDER BY tablename"
    )
    out = run(["psql", "--no-psqlrc", "-Atc", sql], url)
    return [line for line in out.splitlines() if line.strip()]


def dump_schema(url: str) -> str:
    """One pg_dump for the whole public schema."""
    return run(
        [
            "pg_dump",
            "--schema-only",
            "--schema",
            "public",
            "--no-owner",
            "--no-privileges",
            "--no-tablespaces",
            "--no-comments",
        ],
        url,
    )


def split_blocks(dump: str) -> "list[tuple[str, str, str]]":
    """Cut a pg_dump into (type, name, sql) blocks, one per `-- Name:` banner."""
    blocks = []
    btype = name = None
    body: "list[str]" = []

    def flush() -> None:
        if btype is None:
            return
        sql = "\n".join(body).strip("\n")
        if sql:
            blocks.append((btype, name, sql))

    for line in dump.splitlines():
        m = _BANNER.match(line.strip())
        if m:
            flush()
            btype, name = m.group("type").strip(), m.group("name").strip()
            body = []
            continue
        if btype is None:
            continue  # preamble before the first banner
        stripped = line.strip()
        if stripped.startswith(_DROP_PREFIXES) or stripped.startswith("--"):
            continue
        body.append(line.rstrip())
    flush()
    return blocks


def _unquote(ident: str) -> str:
    return ident[1:-1].replace('""', '"') if ident.startswith('"') else ident


_QUALIFIED = re.compile(r'public\.("(?:[^"]|"")+"|[A-Za-z_][A-Za-z0-9_$]*)')


def attribute(
    blocks: "list[tuple[str, str, str]]", tables: "set[str]"
) -> "tuple[dict[str, list[tuple[str, str, str]]], list[tuple[str, str]]]":
    """File each block under the table it belongs to.

    Returns (table -> ordered sql blocks, unattributed table-scoped blocks).
    Non-table objects (views, functions, extensions) are dropped silently;
    anything table-scoped we could not place is reported so a miss is loud
    rather than a silently missing index.
    """
    def owner(btype: str, name: str, sql: str) -> "str | None":
        if btype == "TABLE":
            return name
        if btype in _TABLE_PREFIXED:
            # "<table> <object>" - the table name is the part in `tables`, which
            # survives table names that themselves contain a space.
            parts = name.split(" ")
            for cut in range(len(parts) - 1, 0, -1):
                candidate = " ".join(parts[:cut])
                if candidate in tables:
                    return candidate
            return parts[0]
        if btype.startswith("INDEX"):
            m = re.search(r"\bON\s+(?:ONLY\s+)?" + _QUALIFIED.pattern, sql)
            if m:
                return _unquote(m.group(1))
        # Anything else: fall back to the first public.<table> the
        # statement touches.
        for m in _QUALIFIED.finditer(sql):
            candidate = _unquote(m.group(1))
            if candidate in tables:
                return candidate
        return None

    by_table: "dict[str, list[str]]" = {}
    orphans: "list[tuple[str, str]]" = []
    for btype, name, sql in blocks:
        if btype not in _TABLE_SCOPED:
            continue  # view, matview, function, extension, ...
        table = owner(btype, name, sql)
        if table is None or table not in tables:
            orphans.append((btype, name))
            continue
        by_table.setdefault(table, []).append((btype, name, sql))
    return by_table, orphans


# ---------------------------------------------------------------- writing


_NUMBERED = re.compile(r"^(?P<num>\d{4})_(?P<slug>.+)\.sql$")
_DRIFT_NOTE = re.compile(
    r"differs from the database|no longer in the database"
)


def slugify(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]+", "_", name).strip("_").lower()


def object_label(btype: str, name: str, table: str) -> str:
    """Filename slug for one object: the object's own name, not the table's."""
    if btype == "DEFAULT":
        # Banner name is "<table> <column>".
        column = name[len(table) :].strip() if name.startswith(table) else name
        return slugify(f"default_{column}")
    if btype.endswith("CONSTRAINT"):
        # Banner name is "<table> <constraint>".
        rest = name[len(table) :].strip() if name.startswith(table) else name
        return slugify(rest)
    return slugify(name)


def write_numbered(
    directory: Path, items: "list[tuple[str, str]]", dry_run: bool
) -> "list[str]":
    """Write one numbered .sql per object, keeping numbers stable across runs.

    `items` is [(slug, sql)]. A slug that already has a file keeps its number
    and its contents: these read as applied migrations, so they are append-only
    and never rewritten. New slugs take the next free numbers. A file whose SQL
    no longer matches the database is reported as drift rather than silently
    overwritten - fix it with a new migration.
    """
    existing: "dict[str, Path]" = {}
    used: "set[int]" = set()
    if directory.exists():
        for path in sorted(directory.iterdir()):
            m = _NUMBERED.match(path.name)
            if m:
                existing[m.group("slug")] = path
                used.add(int(m.group("num")))

    notes: "list[str]" = []
    next_num = max(used) + 1 if used else 1
    for slug, sql in items:
        body = sql.rstrip() + "\n"
        path = existing.get(slug)
        if path is not None:
            if path.read_text() != body:
                notes.append(f"{directory.name}/{path.name}: differs from the database")
            continue
        target = directory / f"{next_num:04d}_{slug}.sql"
        next_num += 1
        notes.append(f"{directory.name}/{target.name}: new")
        if not dry_run:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(body)

    # Numbered files carry no provenance marker, so this cannot tell a file the
    # script wrote from a hand-written migration: anything without a counterpart
    # in the dump is reported, hand-written migrations included.
    live = {s for s, _ in items}
    for slug, path in sorted(existing.items()):
        if slug not in live:
            notes.append(f"{directory.name}/{path.name}: no longer in the database")
    return notes


def write_table(
    root: Path,
    db: str,
    table: str,
    blocks: "list[tuple[str, str, str]]",
    dry_run: bool,
) -> "list[str]":
    """Lay one table out as schema.sql + indexes/ + migrations/."""
    table_dir = root / db / table
    notes: "list[str]" = []

    create = [sql for btype, _, sql in blocks if btype == "TABLE"]
    indexes = [
        (object_label(btype, name, table), sql)
        for btype, name, sql in blocks
        if btype in _BODY_SCOPED
    ]
    # Constraints and the column defaults that pg_dump emits separately: both
    # are plain ALTER TABLE statements that bring the bare table up to spec.
    migrations = [
        (object_label(btype, name, table), sql)
        for btype, name, sql in blocks
        if btype in _TABLE_PREFIXED
    ]

    if create:
        path = table_dir / "schema.sql"
        content = "\n\n".join(create).rstrip() + "\n"
        old = path.read_text() if path.exists() else None
        if old != content:
            notes.append("schema.sql: " + ("new" if old is None else "updated"))
            if not dry_run:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)

    notes += write_numbered(table_dir / _MIGRATION_DIR, migrations, dry_run)
    notes += write_numbered(table_dir / _INDEX_DIR, indexes, dry_run)
    return notes


README = """# relations

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
"""


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Dump both GIMS database schemas into ./relations",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--config", type=Path, help="dhall config to read the URLs from")
    ap.add_argument("--db-url", help="master/replica DB url (overrides --config)")
    ap.add_argument("--internal-db-url", help="internal DB url (overrides --config)")
    ap.add_argument(
        "--out",
        type=Path,
        default=REPO_ROOT / "relations",
        help="output directory (default: <repo>/relations)",
    )
    ap.add_argument(
        "--only",
        choices=("master", "internal"),
        help="sync just one of the two databases",
    )
    ap.add_argument(
        "--table",
        action="append",
        default=[],
        help="restrict to these tables (repeatable)",
    )
    ap.add_argument(
        "--dry-run", action="store_true", help="report what would change, write nothing"
    )
    args = ap.parse_args()

    master, internal = args.db_url, args.internal_db_url
    if not (master and internal):
        if not args.config:
            die("need --config, or both --db-url and --internal-db-url")
        cfg_master, cfg_internal = urls_from_dhall(resolve_config(args.config))
        master = master or cfg_master
        internal = internal or cfg_internal

    targets = [("master", normalise(master)), ("internal", normalise(internal))]
    if args.only:
        targets = [t for t in targets if t[0] == args.only]

    root: Path = args.out
    if not args.dry_run:
        root.mkdir(parents=True, exist_ok=True)
        (root / "README.md").write_text(README)

    total = 0
    drift = 0
    for role, url in targets:
        db = DB_DIRS[role]
        log(f"==> {role} -> {db}/  ({db_name(url)} @ {urlsplit(url).hostname})")
        tables = set(list_tables(url))
        if args.table:
            missing = set(args.table) - tables
            if missing:
                die(f"{db_name(url)}: no such table(s): {', '.join(sorted(missing))}")
        if not tables:
            log("    (no tables in public schema)")
            continue
        log(f"    {len(tables)} table(s), dumping...")

        by_table, orphans = attribute(split_blocks(dump_schema(url)), tables)
        for btype, name in orphans:
            log(f"    warning: could not place {btype} `{name}`; not written")

        wanted = set(args.table) if args.table else tables
        for table in sorted(wanted):
            blocks = by_table.get(table)
            if not blocks:
                log(f"    warning: {table}: nothing dumped, skipped")
                continue
            notes = write_table(root, db, table, blocks, args.dry_run)
            total += 1
            for note in notes:
                log(f"    {table}/{note}")
                if _DRIFT_NOTE.search(note):
                    drift += 1

    log(f"{'would sync' if args.dry_run else 'synced'} {total} table(s) into {root}")
    if drift:
        # Numbered files are never rewritten, so these need a human either way.
        # Exiting non-zero is what makes `--dry-run` usable as a CI drift check.
        log(f"{drift} file(s) out of sync with the database")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
