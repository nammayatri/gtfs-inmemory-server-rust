#!/usr/bin/env python3
"""Parity check: a DB-backed feed must serve what the preprocessed feed serves.

Run two GIMS instances over the same preprocessed data, one with the feed in
`gtfs_db_feeds` and one without, then:

    python scripts/parity_gtfs_db.py --a http://127.0.0.1:18001 --b http://127.0.0.1:18002

`--gtfs-id` may be given once per feed (default: chennai_bus), and each feed is
reported on its own - a feed is either identical or it is not, and one feed's
failure says nothing about the next one's.

Every public static API for the feed is requested from both and compared as
normalised JSON (object keys sorted, lists compared as multisets - HashMap
iteration order differs between processes, so list order is not data; every
ordered list GIMS serves carries its own sequence number). The sample is
reproducible: every Nth route and stop in sorted order, plus the ids given with
--focus-stop.

`/version/{gtfs_id}` is reported separately: for a DB feed it deliberately
folds in `gtfs_feed.version`, so it is expected to differ.

`--trips` is the acceptance test of docs/gtfs-editor.md section 16.6, for a
feed whose trips come from the editor's tables (`trips_source = 'db'`) against
the same feed's preprocessed load: every route (`/example-trip` for each),
`/cached-data` for the feed, and `/trip/{id}?gtfs_id=` for at least
`--trip-sample` trips (default 500) spread evenly over the trips the
preprocessed patterns list (`--preprocessed-dir`). A `/trip` answer's `source`
and `lastUpdated` say where and when it was produced - they differ between two
calls to the same GIMS - and are not compared. No public API serves a pattern
whole; `cargo run --example parity_gims -- patterns` compares those.

`--ignore-key K` leaves key K out of every compared response, for a field known
to differ for a reason that is not under test (say so when reporting a result).

Exit status is 0 only when every compared response is identical.
"""
import argparse
import collections
import concurrent.futures
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


IGNORED = set()


def normalise(v):
    if isinstance(v, dict):
        return {k: normalise(v[k]) for k in sorted(v) if k not in IGNORED}
    if isinstance(v, list):
        items = [normalise(x) for x in v]
        return sorted(items, key=lambda x: json.dumps(x, sort_keys=True))
    if isinstance(v, float) and v.is_integer():
        return v
    return v


def trip_ids(preprocessed_dir, g, at_least):
    """At least `at_least` trip ids of feed `g`, spread evenly over the trips its
    preprocessed patterns list, in a reproducible order."""
    with open(f"{preprocessed_dir}/patterns.json") as f:
        patterns = json.load(f).get(g, [])
    ids = sorted({t["id"] for p in patterns for t in p["trips"]})
    every = max(1, len(ids) // max(1, at_least))
    return ids[::every]


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
    ap.add_argument("--gtfs-id", action="append", default=None,
                    help="feed to compare; repeat for several (default: chennai_bus)")
    ap.add_argument("--every", type=int, default=50, help="sample every Nth route and stop")
    ap.add_argument("--focus-stop", action="append", default=None,
                    help="stop codes always sampled on every feed that has them "
                         "(default: the Alwarpet Anjaneyar ids, which only chennai_bus has)")
    ap.add_argument("--all-routes", action="store_true", help="compare every route, not a sample")
    ap.add_argument("--cached-data", action="store_true", help="also compare /cached-data for the feed")
    ap.add_argument("--trips", action="store_true",
                    help="section 16.6: every route, /cached-data and /trip/{id} for sampled trips")
    ap.add_argument("--preprocessed-dir", help="the preprocessed data both GIMS load (--trips samples its trips)")
    ap.add_argument("--trip-sample", type=int, default=500, help="at least this many /trip ids a feed")
    ap.add_argument("--jobs", type=int, default=1,
                    help="/trip requests in flight at once (a preprocessed GIMS reads the whole shard per trip)")
    ap.add_argument("--ignore-key", action="append", default=[],
                    help="leave this key out of every compared response; repeat for several")
    args = ap.parse_args()
    feeds = args.gtfs_id or ["chennai_bus"]
    if args.trips:
        if not args.preprocessed_dir:
            sys.exit("--trips needs --preprocessed-dir to sample trip ids from")
        args.all_routes = True
        args.cached_data = True
    IGNORED.update(args.ignore_key)

    for base in (args.a, args.b):
        status, _ = fetch(base, "/ready")
        if status != 200:
            sys.exit(f"{base} is not ready (HTTP {status})")

    failed = [g for g in feeds if compare_feed(args, g) != 0]
    if len(feeds) > 1:
        print("\n==== all feeds ====")
        for g in feeds:
            print(f"{g:<45} {'FAILED' if g in failed else 'OK'}")
    return 1 if failed else 0


def compare_feed(args, g):
    """Every compared response for one feed. 0 when they are all identical."""
    focus = args.focus_stop or ALWARPET
    print(f"\n==== {g} ====")

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

    routes = compare("/routes/{g}", f"/routes/{g}")
    stops = compare("/stops/{g}", f"/stops/{g}")
    compare("/stops/{g}?includeClusterId=true", f"/stops/{g}?includeClusterId=true")

    route_ids = sorted(r["id"] for r in (routes or []))
    sample_routes = route_ids if args.all_routes else route_ids[:: args.every]
    stop_codes = sorted({s["stopCode"] for s in (stops or [])})
    # A focus code the feed does not have would only compare two 404s.
    sample_stops = sorted(set(stop_codes[:: args.every]) | (set(focus) & set(stop_codes)))

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

    sampled_trips = []
    if args.trips:
        # where and when a /trip answer was produced is not data: it differs
        # between two calls to one GIMS
        IGNORED.update({"source", "lastUpdated"})
        sampled_trips = trip_ids(args.preprocessed_dir, g, args.trip_sample)
        paths = [f"/trip/{q(tid, safe='')}?gtfs_id={q(g)}" for tid in sampled_trips]
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
            answers = list(pool.map(lambda path: (fetch(args.a, path), fetch(args.b, path)), paths))
        for path, (a_resp, b_resp) in zip(paths, answers):
            compare("/trip/{id}?gtfs_id={g}", path, a_resp=a_resp, b_resp=b_resp)
        IGNORED.difference_update({"source", "lastUpdated"} - set(args.ignore_key))

    va, vb = fetch(args.a, f"/version/{g}"), fetch(args.b, f"/version/{g}")

    width = max(len(k) for k in results)
    print(f"{'endpoint':<{width}}  identical  differing")
    total_diff = 0
    for group, (same, diff) in results.items():
        total_diff += diff
        print(f"{group:<{width}}  {same:>9}  {diff:>9}")
        for e in examples[group]:
            print(f"{'':<{width}}    {e}")
    print(f"\nsampled {len(sample_routes)} routes, {len(sample_stops)} stops, {len(pairs[:100])} cluster pairs"
          + (f", {len(sampled_trips)} trips" if args.trips else ""))
    if args.ignore_key:
        print(f"keys left out of every comparison: {', '.join(args.ignore_key)}")
    print(f"/version/{g}: a={va[1]} b={vb[1]} (expected to differ: the DB feed folds in gtfs_feed.version)")
    print(f"{g}: PARITY OK" if total_diff == 0
          else f"{g}: PARITY FAILED: {total_diff} differing responses")
    return 0 if total_diff == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
