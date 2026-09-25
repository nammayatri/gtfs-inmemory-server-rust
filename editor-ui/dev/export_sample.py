#!/usr/bin/env python3
"""Export the editor tables from a LOCAL Postgres into dev/sample.json.gz.

The mock server reads that file, so it can then run with the standard library
only. Read-only: one SELECT per table in a read-only transaction. Never point
this at master or prod.

    python dev/export_sample.py                       # 127.0.0.1:55432, mtc_internal_master
    python dev/export_sample.py --dsn "host=127.0.0.1 port=55432 dbname=mtc_internal_master user=postgres"
"""
import argparse
import gzip
import json
from pathlib import Path

import psycopg2
import psycopg2.extras

HERE = Path(__file__).resolve().parent
DEFAULT_DSN = "host=127.0.0.1 port=55432 dbname=mtc_internal_master user=postgres"

QUERIES = {
    "feeds": "SELECT gtfs_id, display_name, version, data_source, agency_name, released_version FROM gtfs_feed",
    "stops": """SELECT gtfs_id, stop_id, stop_code, name, lat, lon, location_type, parent_station,
                       platform_code, description, cluster_id, regional_name, hindi_name, position_source,
                       row_version, deleted
                FROM gtfs_stop""",
    "routes": """SELECT gtfs_id, route_id, short_name, long_name, route_type, agency_id, color, encoded_polyline,
                        polyline_source, provenance, row_version, deleted
                 FROM gtfs_route""",
    "route_stops": """SELECT gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name,
                             marker_id, marker_lat, marker_lon, marker_name, stop_name_override, provider_id
                      FROM gtfs_route_stop WHERE pattern_key = 1
                      ORDER BY gtfs_id, route_id, sequence""",
    # station suggestions waiting for review (docs section 6)
    "station_proposals": """SELECT proposal_id, gtfs_id, batch, station_id, name, lat, lon, members, spread_m,
                                   status, review_note
                            FROM gtfs_station_proposal ORDER BY proposal_id""",
    # suspected wrong coordinates waiting for review (docs section 8); a database
    # without the table (before 0007) exports none, and the mock makes a few up
    "position_reviews": """SELECT review_id, gtfs_id, batch, stop_id, original_stop_id, stop_name, reason, lat, lon,
                                  raw_lat, raw_lon, suggested_lat, suggested_lon, suggested_source, evidence,
                                  status, review_note
                           FROM gtfs_position_review ORDER BY review_id""",
}
# tables that a local database may not have yet
OPTIONAL = {"position_reviews": "gtfs_position_review"}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dsn", default=DEFAULT_DSN)
    ap.add_argument("--out", default=str(HERE / "sample.json.gz"))
    args = ap.parse_args()
    if "127.0.0.1" not in args.dsn and "localhost" not in args.dsn:
        raise SystemExit("refusing: export_sample.py only reads a local database")
    conn = psycopg2.connect(args.dsn, options="-c default_transaction_read_only=on")
    out = {}
    with conn, conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
        for key, sql in QUERIES.items():
            if key in OPTIONAL:
                cur.execute("SELECT to_regclass(%s) IS NOT NULL AS present", (OPTIONAL[key],))
                if not cur.fetchone()["present"]:
                    out[key] = []
                    print(f"{key}: no {OPTIONAL[key]} table")
                    continue
            cur.execute(sql)
            out[key] = [dict(r) for r in cur.fetchall()]
            print(f"{key}: {len(out[key])}")
    conn.close()
    with gzip.open(args.out, "wt", encoding="utf-8") as fh:
        json.dump(out, fh, separators=(",", ":"))
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
