#!/usr/bin/env python3
"""Fixtures for `parity_gtfs_db.py --trips` (docs/gtfs-editor.md section 16.6),
for a LOCAL test database only.

The acceptance test for trips served from the editor's tables is parity: a GIMS
loading a feed with `trips_source = 'db'` must answer every public static API as
a GIMS loading the feed's preprocessed data does. That needs two things this
script makes, both of which a real rollout gets from nandi instead (the importer
of section 16.7 fills the tables through drafts, and the nightly build is the
preprocessed data):

    # a GTFS of a feed built from the tables - stops, routes and each route's
    # stop list - and the trips of a schedule zip, timed the way
    # generate_trips_from_db.py times them (start + 135 s a stop, 15 s dwell).
    # Run it through gtfs_preprocessor.py and the preprocessed data describes
    # exactly the feed the tables hold, which a local copy of a database and a
    # shipped zip of another day do not.
    python scripts/parity_trips_fixture.py gtfs --db "$DSN" --feed chennai_bus \\
        --schedule-zip chennai.bus.gtfs.zip --out chennai_bus.gtfs.zip

    # the trip tables of a feed filled from its preprocessed data: every pattern
    # a stop order (pattern 1 where the route's stop list already is it), every
    # distinct timing a profile unless it is the feed's default, every trip in
    # the order the preprocessed patterns list them, one service. Then the feed's
    # data_source and trips_source are 'db'. SQL on stdout, for psql.
    python scripts/parity_trips_fixture.py sql --db "$DSN" --feed chennai_bus \\
        --preprocessed-dir DIR | psql "$DSN"

This writes the trip tables directly, which nothing but a local test fixture may
do: the editor's tables are changed by committed drafts only. Both subcommands
refuse a database that is not on 127.0.0.1 or localhost.
"""
import argparse
import csv
import io
import json
import sys
import zipfile
from collections import OrderedDict
from pathlib import Path

import psycopg2

SERVED = ("NEW STOP", "INTERMEDIATE STOP")
RUN_S, DWELL_S = 120, 15


def local(dsn):
    if "127.0.0.1" not in dsn and "localhost" not in dsn:
        sys.exit("refusing a database that is not local")
    conn = psycopg2.connect(dsn)
    conn.set_session(readonly=True)
    return conn


def rows(conn, sql, *args):
    with conn.cursor() as cur:
        cur.execute(sql, args)
        return cur.fetchall()


def fare_headsign(stage_no, stop_type):
    if stop_type == "NEW STOP":
        return f"{{'fareStageNumber': '{stage_no}', 'isStageStop': true}}"
    return str(stage_no)


def seconds(text):
    h, m, s = (int(x) for x in text.strip().split(":"))
    return h * 3600 + m * 60 + s


def hms(total):
    return f"{total // 3600:02d}:{total % 3600 // 60:02d}:{total % 60:02d}"


def default_offsets(n, run_s, dwell_s):
    arrival = [i * (run_s + dwell_s) for i in range(n)]
    departure = [a if i == n - 1 else a + dwell_s for i, a in enumerate(arrival)]
    return arrival, departure


# ---------------------------------------------------------------- gtfs

def build_gtfs(args):
    conn = local(args.db)
    g = args.feed
    (agency, headsign_source), = rows(
        conn, "SELECT agency_name, headsign_source FROM gtfs_feed WHERE gtfs_id = %s", g)
    routes = rows(conn, """SELECT route_id, short_name, long_name, route_type, color FROM gtfs_route
                           WHERE gtfs_id = %s AND NOT deleted ORDER BY route_id""", g)
    served = OrderedDict()
    for route_id, stop_id, stop_type, stage_no, own in rows(conn, """
            SELECT rs.route_id, rs.stop_id, rs.stop_type, rs.stage_no, rs.stop_headsign
            FROM gtfs_route_stop rs
            JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted
            WHERE rs.gtfs_id = %s AND rs.pattern_key = 1 AND rs.stop_type = ANY(%s)
            ORDER BY rs.route_id, rs.sequence""", g, list(SERVED)):
        own = (own or "").strip() or None
        headsign = own or (fare_headsign(stage_no, stop_type) if headsign_source == "fare_stage" else None)
        served.setdefault(route_id, []).append((stop_id, headsign))
    referenced = {s for calls in served.values() for s, _ in calls}
    stops = [r for r in rows(conn, """
            SELECT stop_id, stop_code, name, lat, lon, location_type, parent_station, platform_code,
                   cluster_id, description FROM gtfs_stop
            WHERE gtfs_id = %s AND NOT deleted ORDER BY stop_id""", g)
             if r[0] in referenced or r[5] == 1]

    zin = zipfile.ZipFile(args.schedule_zip)
    prefix = next((n[:-len("trips.txt")] for n in zin.namelist() if n.endswith("trips.txt")), "")
    read = lambda name: list(csv.DictReader(io.TextIOWrapper(zin.open(prefix + name), "utf-8-sig")))
    first_arrival = {}
    with zin.open(prefix + "stop_times.txt") as f:
        for r in csv.DictReader(io.TextIOWrapper(f, "utf-8-sig")):
            seq, t = int(r["stop_sequence"]), r["trip_id"]
            if t not in first_arrival or seq < first_arrival[t][0]:
                first_arrival[t] = (seq, seconds(r["arrival_time"]))
    trips = [t for t in read("trips.txt") if t["route_id"] in served and t["trip_id"] in first_arrival]

    files = {}

    def table(name, header, body):
        buf = io.StringIO()
        w = csv.writer(buf, lineterminator="\n")
        w.writerow(header)
        w.writerows(body)
        files[name] = buf.getvalue()

    table("agency.txt", ["agency_id", "agency_name", "agency_url", "agency_timezone"],
          [["A", agency or "", "https://example.invalid", "Asia/Kolkata"]])
    table("feed_info.txt", ["feed_publisher_name", "feed_publisher_url", "feed_lang", "feed_id"],
          [["parity fixture", "https://example.invalid", "en", g]])
    table("routes.txt", ["route_id", "agency_id", "route_short_name", "route_long_name", "route_type", "route_color"],
          [[r[0], "A", r[1] or "", r[2] or "", r[3], (r[4] or "").lstrip("#")] for r in routes])
    table("stops.txt", ["stop_id", "stop_code", "stop_name", "stop_lat", "stop_lon", "location_type",
                        "parent_station", "platform_code", "info_json", "stop_desc"],
          [[s[0], s[1] or s[0], s[2], repr(s[3]), repr(s[4]), s[5], s[6] or "", s[7] or "",
            json.dumps({"clusterId": s[8]}, separators=(",", ":")) if s[8] else "", s[9] or ""]
           for s in stops])
    table("trips.txt", ["route_id", "service_id", "trip_id", "direction_id"],
          [[t["route_id"], t["service_id"], t["trip_id"], t.get("direction_id", "")] for t in trips])
    body = []
    for t in trips:
        calls = served[t["route_id"]]
        start = first_arrival[t["trip_id"]][1]
        arrival, departure = default_offsets(len(calls), RUN_S, DWELL_S)
        for i, (stop_id, headsign) in enumerate(calls):
            body.append([t["trip_id"], hms(start + arrival[i]), hms(start + departure[i]), stop_id, i + 1,
                         headsign or ""])
    table("stop_times.txt", ["trip_id", "arrival_time", "departure_time", "stop_id", "stop_sequence",
                             "stop_headsign"], body)
    files["calendar.txt"] = zin.read(prefix + "calendar.txt").decode("utf-8-sig")
    with zipfile.ZipFile(args.out, "w", zipfile.ZIP_DEFLATED) as z:
        for name, text in files.items():
            z.writestr(name, text)
    print(f"{args.out}: {len(routes)} routes, {len(stops)} stops, {len(trips)} trips, "
          f"{len(body)} stop times", file=sys.stderr)


# ---------------------------------------------------------------- sql

def q(value):
    return "NULL" if value is None else "'" + str(value).replace("'", "''") + "'"


def copy(out, table, columns, body):
    out.write(f"COPY {table} ({', '.join(columns)}) FROM STDIN WITH (FORMAT csv, NULL '\\N');\n")
    w = csv.writer(out, lineterminator="\n")
    for row in body:
        w.writerow(["\\N" if v is None else v for v in row])
    out.write("\\.\n")


def fill_sql(args):
    conn = local(args.db)
    g = args.feed
    pre = Path(args.preprocessed_dir)
    patterns = json.loads((pre / "patterns.json").read_text())[g]
    shard = json.loads((pre / "trip_stoptimes" / f"{g}.json").read_text())
    (run_s, dwell_s), = rows(conn, "SELECT default_run_s, default_dwell_s FROM gtfs_feed WHERE gtfs_id = %s", g)
    live_routes = {r for (r,) in rows(conn, "SELECT route_id FROM gtfs_route WHERE gtfs_id = %s AND NOT deleted", g)}
    stop_ids = {s for (s,) in rows(conn, "SELECT stop_id FROM gtfs_stop WHERE gtfs_id = %s AND NOT deleted", g)}
    first = OrderedDict()
    for route_id, stop_id in rows(conn, """
            SELECT route_id, stop_id FROM gtfs_route_stop
            WHERE gtfs_id = %s AND pattern_key = 1 AND stop_type = ANY(%s) ORDER BY route_id, sequence""",
            g, list(SERVED)):
        first.setdefault(route_id, []).append(stop_id)

    by_route = OrderedDict()
    for p in patterns:
        by_route.setdefault(p["routeId"].split(":", 1)[1], []).append(p)
    pattern_rows, stop_rows, profile_rows, trip_rows = [], [], [], []
    skipped, sort_key, counts = [], 0, {"patterns": 0, "extra": 0, "profiles": 0, "default": 0}
    for route_id, route_patterns in by_route.items():
        if route_id not in live_routes:
            skipped.append(route_id)
            continue
        next_key = 2
        for p in route_patterns:
            ids = [s["id"].split(":", 1)[1] for s in p["stops"]]
            missing = [s for s in ids if s not in stop_ids]
            if missing:
                sys.exit(f"{g} route {route_id}: stops {missing[:3]} are not in gtfs_stop")
            counts["patterns"] += 1
            if ids == first.get(route_id):
                key = 1
            else:
                key, next_key = next_key, next_key + 1
                counts["extra"] += 1
                pattern_rows.append([g, route_id, key])
                for i, s in enumerate(p["stops"]):
                    stop_rows.append([g, route_id, key, i + 1, ids[i], "NEW STOP", 0, "", s.get("headsign")])
            n = len(ids)
            default = default_offsets(n, run_s, dwell_s)
            profiles = {}
            for t in p["trips"]:
                calls = shard[t["id"]]["stops"]
                if len(calls) != n:
                    sys.exit(f"{g} trip {t['id']}: {len(calls)} stop times for a pattern of {n} stops")
                ref = calls[0]["arrivalTime"]
                offsets = ([c["arrivalTime"] - ref for c in calls], [c["departureTime"] - ref for c in calls])
                profile = None
                if (offsets[0], offsets[1]) != default:
                    profile = profiles.get((tuple(offsets[0]), tuple(offsets[1])))
                    if profile is None:
                        profile = len(profiles) + 1
                        profiles[(tuple(offsets[0]), tuple(offsets[1]))] = profile
                        profile_rows.append([g, route_id, key, profile,
                                             "{" + ",".join(map(str, offsets[0])) + "}",
                                             "{" + ",".join(map(str, offsets[1])) + "}", "import"])
                else:
                    counts["default"] += 1
                sort_key += 1
                trip_rows.append([g, t["id"], route_id, key, profile, "ALL", t.get("direction"), ref,
                                  sort_key, "import"])
            counts["profiles"] += len(profiles)

    out = sys.stdout
    out.write("\\set ON_ERROR_STOP on\nBEGIN;\n")
    out.write(f"DELETE FROM gtfs_trip WHERE gtfs_id = {q(g)};\n")
    out.write(f"DELETE FROM gtfs_timing_profile WHERE gtfs_id = {q(g)};\n")
    out.write(f"DELETE FROM gtfs_pattern WHERE gtfs_id = {q(g)} AND pattern_key > 1;\n")
    out.write(f"DELETE FROM gtfs_service WHERE gtfs_id = {q(g)};\n")
    out.write(f"INSERT INTO gtfs_service (gtfs_id, service_id, monday, tuesday, wednesday, thursday, friday, "
              f"saturday, sunday, label) VALUES ({q(g)}, 'ALL', true, true, true, true, true, true, true, "
              f"'parity fixture: every day');\n")
    copy(out, "gtfs_pattern", ["gtfs_id", "route_id", "pattern_key"], pattern_rows)
    copy(out, "gtfs_route_stop", ["gtfs_id", "route_id", "pattern_key", "sequence", "stop_id", "stop_type",
                                  "stage_no", "stage_name", "stop_headsign"], stop_rows)
    copy(out, "gtfs_timing_profile", ["gtfs_id", "route_id", "pattern_key", "profile_key", "arrival_s",
                                      "departure_s", "source"], profile_rows)
    copy(out, "gtfs_trip", ["gtfs_id", "trip_id", "route_id", "pattern_key", "profile_key", "service_id",
                            "direction_id", "ref_s", "sort_key", "source"], trip_rows)
    out.write(f"UPDATE gtfs_feed SET data_source = 'db', trips_source = 'db', version = version + 1 "
              f"WHERE gtfs_id = {q(g)};\n")
    out.write("COMMIT;\n")
    print(f"{g}: {counts['patterns']} patterns ({counts['extra']} past a route's first), "
          f"{len(trip_rows)} trips ({counts['default']} on the default timing), "
          f"{counts['profiles']} profiles; {len(skipped)} routes not in the tables"
          f"{' (' + ', '.join(skipped[:5]) + ')' if skipped else ''}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("gtfs", help="a GTFS zip of a feed from the tables and a schedule zip's trips")
    a.add_argument("--db", required=True)
    a.add_argument("--feed", required=True)
    a.add_argument("--schedule-zip", required=True)
    a.add_argument("--out", required=True)
    b = sub.add_parser("sql", help="SQL that fills a feed's trip tables from its preprocessed data")
    b.add_argument("--db", required=True)
    b.add_argument("--feed", required=True)
    b.add_argument("--preprocessed-dir", required=True)
    args = ap.parse_args()
    (build_gtfs if args.cmd == "gtfs" else fill_sql)(args)


if __name__ == "__main__":
    main()
