#!/usr/bin/env python3
"""Parity check: a DB-backed feed must serve what the preprocessed feed serves.

Run two GIMS instances over the same preprocessed data, one with the feed in
`gtfs_db_feeds` and one without, then:

    python scripts/parity_gtfs_db.py --a http://127.0.0.1:18001 --b http://127.0.0.1:18002

Every public static API for the feed is requested from both and compared as
normalised JSON (object keys sorted, lists compared as multisets - HashMap
iteration order differs between processes, so list order is not data; every
ordered list GIMS serves carries its own sequence number). The sample is
reproducible: every Nth route and stop in sorted order, plus the ids given with
--focus-stop.

`/version/{gtfs_id}` is reported separately: for a DB feed it deliberately
folds in `gtfs_feed.version`, so it is expected to differ.

Exit status is 0 only when every compared response is identical.
"""
import argparse
import collections
import json
import sys
import urllib.error
import urllib.parse
import urllib.request

ALWARPET = ["de9014549c", "ac4c98fee1", "68e3cdc2e0", "12e10ffb03"]


def fetch(base, path, body=None, timeout=300):
    url = base.rstrip("/") + path
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method="POST" if body is not None else "GET",
                                 headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            status = r.status
    except urllib.error.HTTPError as e:
        raw, status = e.read(), e.code
    try:
        return status, json.loads(raw) if raw else None
    except ValueError:
        return status, raw.decode(errors="replace")


def normalise(v):
    if isinstance(v, dict):
        return {k: normalise(v[k]) for k in sorted(v)}
    if isinstance(v, list):
        items = [normalise(x) for x in v]
        return sorted(items, key=lambda x: json.dumps(x, sort_keys=True))
    if isinstance(v, float) and v.is_integer():
        return v
    return v


def first_difference(a, b, path="$"):
    if type(a) is not type(b):
        return f"{path}: {type(a).__name__} vs {type(b).__name__}"
    if isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                return f"{path}.{k}: only in {'b' if k not in a else 'a'}"
            d = first_difference(a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: length {len(a)} vs {len(b)}"
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_difference(x, y, f"{path}[{i}]")
            if d:
                return d
        return None
    return None if a == b else f"{path}: {json.dumps(a)[:120]} vs {json.dumps(b)[:120]}"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--a", required=True, help="GIMS serving the feed from preprocessed data")
    ap.add_argument("--b", required=True, help="GIMS serving the feed from the DB")
    ap.add_argument("--gtfs-id", default="chennai_bus")
    ap.add_argument("--every", type=int, default=50, help="sample every Nth route and stop")
    ap.add_argument("--focus-stop", action="append", default=None,
                    help="stop codes always sampled (default: the Alwarpet Anjaneyar ids)")
    ap.add_argument("--all-routes", action="store_true", help="compare every route, not a sample")
    ap.add_argument("--cached-data", action="store_true", help="also compare /cached-data for the feed")
    args = ap.parse_args()
    g = args.gtfs_id
    focus = args.focus_stop or ALWARPET

    results = collections.defaultdict(lambda: [0, 0])   # group -> [identical, differing]
    examples = collections.defaultdict(list)

    def compare(group, path, body=None, a_resp=None, b_resp=None):
        sa, ja = a_resp if a_resp is not None else fetch(args.a, path, body)
        sb, jb = b_resp if b_resp is not None else fetch(args.b, path, body)
        na, nb = normalise(ja), normalise(jb)
        same = sa == sb and na == nb
        results[group][0 if same else 1] += 1
        if not same and len(examples[group]) < 3:
            why = f"status {sa} vs {sb}" if sa != sb else first_difference(na, nb)
            examples[group].append(f"{path} -> {why}")
        return ja

    for base in (args.a, args.b):
        status, _ = fetch(base, "/ready")
        if status != 200:
            sys.exit(f"{base} is not ready (HTTP {status})")

    routes = compare("/routes/{g}", f"/routes/{g}")
    stops = compare("/stops/{g}", f"/stops/{g}")
    compare("/stops/{g}?includeClusterId=true", f"/stops/{g}?includeClusterId=true")

    route_ids = sorted(r["id"] for r in (routes or []))
    sample_routes = route_ids if args.all_routes else route_ids[:: args.every]
    stop_codes = sorted({s["stopCode"] for s in (stops or [])})
    sample_stops = sorted(set(stop_codes[:: args.every]) | set(focus))

    q = urllib.parse.quote
    pairs = []
    for rid in sample_routes:
        compare("/route/{g}/{r}", f"/route/{g}/{q(rid)}")
        mapping = compare("/route-stop-mapping/{g}/route/{r}", f"/route-stop-mapping/{g}/route/{q(rid)}")
        compare("/example-trip/{g}/{r}", f"/example-trip/{g}/{q(rid)}")
        if isinstance(mapping, list) and len(mapping) >= 2:
            ordered = sorted(mapping, key=lambda m: m.get("sequenceNum", 0))
            pairs.append((ordered[0]["stopCode"], ordered[-1]["stopCode"]))

    for code in sample_stops:
        compare("/stop/{g}/{c}", f"/stop/{g}/{q(code)}")
        compare("/route-stop-mapping/{g}/stop/{c}", f"/route-stop-mapping/{g}/stop/{q(code)}")
        compare("/cluster/{g}/destinations/{c}", f"/cluster/{g}/destinations/{q(code)}")
        compare("/station-children/{g}/{c}", f"/station-children/{g}/{q(code)}")
        compare("/alternateStops/{g}/{c}", f"/alternateStops/{g}/{q(code)}")

    for a_code, b_code in pairs[:100]:
        compare("/cluster/{g}/routes/{from}/{to}", f"/cluster/{g}/routes/{q(a_code)}/{q(b_code)}")

    compare("POST /getAllRoutesByIds", "/getAllRoutesByIds", {"gtfsId": g, "routeIds": sample_routes})
    compare("POST /getAllStopsByIds", "/getAllStopsByIds", {"gtfsId": g, "stopIds": sample_stops})
    compare("POST /getAllRouteStopMappingsByRouteCodes", "/getAllRouteStopMappingsByRouteCodes",
            {"gtfsId": g, "routeCodes": sample_routes})
    compare("POST /getAllRouteStopMappingsByStopCodes", "/getAllRouteStopMappingsByStopCodes",
            {"gtfsId": g, "stopCodes": sample_stops})
    compare("POST /getRoutesByIds/{g}", f"/getRoutesByIds/{g}", sample_routes)
    compare("/example-trip-map", "/example-trip-map")

    if args.cached_data:
        sa, ca = fetch(args.a, "/cached-data")
        sb, cb = fetch(args.b, "/cached-data")
        for key in ("route_data_by_gtfs", "stops_by_gtfs"):
            pa = (sa, (ca or {}).get(key, {}).get(g))
            pb = (sb, (cb or {}).get(key, {}).get(g))
            compare(f"/cached-data {key}[{g}]", "/cached-data", a_resp=pa, b_resp=pb)

    va, vb = fetch(args.a, f"/version/{g}"), fetch(args.b, f"/version/{g}")

    width = max(len(k) for k in results)
    print(f"{'endpoint':<{width}}  identical  differing")
    total_diff = 0
    for group, (same, diff) in results.items():
        total_diff += diff
        print(f"{group:<{width}}  {same:>9}  {diff:>9}")
        for e in examples[group]:
            print(f"{'':<{width}}    {e}")
    print(f"\nsampled {len(sample_routes)} routes, {len(sample_stops)} stops, {len(pairs[:100])} cluster pairs")
    print(f"/version/{g}: a={va[1]} b={vb[1]} (expected to differ: the DB feed folds in gtfs_feed.version)")
    print("PARITY OK" if total_diff == 0 else f"PARITY FAILED: {total_diff} differing responses")
    return 0 if total_diff == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
