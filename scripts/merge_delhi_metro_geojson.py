#!/usr/bin/env python3
"""Merge real DMRC gate/boundary survey data into assets/stop_geojsons.csv.

Source data (a Google My Maps export, "SureMetro"):
  --csv   Location Name, GatesInfo (Lat/Lon/name/address), Added/Updated on
          One row per gate.
  --kml   Same map exported as KML. One Polygon Placemark per station
          (its name matches the CSV's Location Name) tracing the station
          boundary, followed by one Point Placemark per gate.

For every delhi_metro row in stop_geojsons.csv, this script:
  - rebuilds the `gates` column from the CSV's gates for that station
  - rebuilds the `geo_json` column from the KML's boundary polygon for
    that station

Station names are matched to stop_geojsons.csv's stop_name via a normalized
comparison, a hand-maintained alias table (spelling/abbreviation
differences), and a fallback word-set match (handles reordered words, e.g.
"Noida Sector 52" vs "Sector - 52 Noida"). Rows with no match in the CSV/KML
are left untouched and reported.

Default is a dry run (report only). Pass --apply to rewrite the CSV.

Usage (source files are committed in the nandi repo):
    python3 scripts/merge_delhi_metro_geojson.py \
        --csv "<path-to-nandi>/scripts/delhi-metro/data/suremetro_gates.csv" \
        --kml "<path-to-nandi>/scripts/delhi-metro/data/dmrc_stations.kml" \
        [--geojsons assets/stop_geojsons.csv] [--gtfs-id delhi_metro] [--apply]
"""
import argparse
import csv
import io
import json
import re
import sys

# CSV/KML station name -> stop_geojsons.csv stop_name, for stations that
# exist under a different spelling. Shared with nandi's
# scripts/delhi-metro/merge_gate_csv.py, which matches the same CSV against
# GTFS stop names.
ALIASES = {
    "r k ashram marg": "ramakrishna ashram marg",
    "mg road": "m g road",
    "new delhi": "new delhi yellow airport line",
    "lal qila": "lal quila",
    "govindpuri": "govind puri",
    "badkhal mor": "badkal mor",
    "delhi cantonment": "delhi cantt",
    "sir vishweshwaraiah moti bagh": "sir m vishweshwaraiah moti bagh",
    "mayur vihar 1": "mayur vihar i",
    "ip extension": "i p extension",
    "anand vihar": "anand vihar isbt",
    "jaffrabad": "jafrabad",
    "brigadier hoshiyar singh": "brig hoshiar singh",
    "mundka industrial area": "mundka industrial area mia",
    "gtb nagar": "guru teg bahadur nagar",
}
REVERSE_ALIASES = {v: k for k, v in ALIASES.items()}


def norm(name):
    n = name.lower().strip()
    n = re.sub(r"\bmetro\b", "", n)
    n = re.sub(r"\bstation\b", "", n)
    n = re.sub(r"[^a-z0-9]+", " ", n)
    return re.sub(r"\s+", " ", n).strip()


def load_csv_gates(csv_path):
    stations = {}
    with open(csv_path, newline="", encoding="utf-8-sig") as f:
        for row in csv.DictReader(f):
            name = row["Location Name"].strip()
            stations.setdefault(name, []).append(
                {
                    "name": row["GatesInfo (name)"].strip(),
                    "lat": row["GatesInfo (Lat)"].strip(),
                    "lon": row["GatesInfo (Lon)"].strip(),
                }
            )
    return {norm(name): (name, gates) for name, gates in stations.items()}


def load_kml_polygons(kml_path):
    data = open(kml_path, encoding="utf-8").read()
    polygons = {}
    for m in re.finditer(r"<Placemark>(.*?)</Placemark>", data, re.S):
        block = m.group(1)
        name_m = re.search(r"<name>(.*?)</name>", block)
        poly_m = re.search(r"<Polygon>.*?<coordinates>(.*?)</coordinates>", block, re.S)
        if not name_m or not poly_m:
            continue
        name = name_m.group(1).strip()
        coords = []
        for triplet in poly_m.group(1).split():
            lon, lat, *_ = triplet.split(",")
            coords.append([float(lon), float(lat)])
        polygons[norm(name)] = (name, coords)
    return polygons


def build_token_index(by_norm):
    index = {}
    for key in by_norm:
        index.setdefault(frozenset(key.split()), key)
    return index


def resolve(key, by_norm, token_index):
    if key in by_norm:
        return by_norm[key]
    alt = REVERSE_ALIASES.get(key)
    if alt in by_norm:
        return by_norm[alt]
    token_key = token_index.get(frozenset(key.split()))
    if token_key:
        return by_norm[token_key]
    return None


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--csv", required=True, help="Path to the SureMetro gate CSV")
    ap.add_argument("--kml", required=True, help="Path to the DMRC station boundary KML")
    ap.add_argument("--geojsons", default="assets/stop_geojsons.csv", help="Path to stop_geojsons.csv")
    ap.add_argument("--gtfs-id", default="delhi_metro", help="gtfs_id to restrict updates to")
    ap.add_argument("--apply", action="store_true", help="Write changes back into the CSV (default: dry run)")
    args = ap.parse_args()

    gates_by_norm = load_csv_gates(args.csv)
    polygons_by_norm = load_kml_polygons(args.kml)
    gates_token_index = build_token_index(gates_by_norm)
    polygons_token_index = build_token_index(polygons_by_norm)

    with open(args.geojsons, newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        fieldnames = reader.fieldnames
        rows = list(reader)

    updated, gate_unmatched, poly_unmatched = [], [], []
    touched = [False] * len(rows)
    for i, row in enumerate(rows):
        if row["gtfs_id"] != args.gtfs_id:
            continue
        key = norm(row["stop_name"])
        gate_entry = resolve(key, gates_by_norm, gates_token_index)
        poly_entry = resolve(key, polygons_by_norm, polygons_token_index)

        if not gate_entry:
            gate_unmatched.append(row["stop_name"])
        else:
            _, gates = gate_entry
            row["gates"] = json.dumps(
                [
                    {"gateName": g["name"], "stopCode": row["stop_code"], "lat": float(g["lat"]), "lon": float(g["lon"])}
                    for g in gates
                ]
            )

        if not poly_entry:
            poly_unmatched.append(row["stop_name"])
        else:
            _, coords = poly_entry
            row["geo_json"] = json.dumps({"type": "MultiPolygon", "coordinates": [[coords]]}, separators=(",", ":"))

        if gate_entry or poly_entry:
            updated.append(row["stop_name"])
            touched[i] = True

    print(f"Updated (gates and/or geo_json): {len(updated)} stations")
    print(f"\nGate-unmatched ({len(gate_unmatched)}):")
    for name in gate_unmatched:
        print(f"  {name}")
    print(f"\nPolygon-unmatched ({len(poly_unmatched)}):")
    for name in poly_unmatched:
        print(f"  {name}")

    if not args.apply:
        print("\nDry run only, no changes written. Pass --apply to write.")
        return

    # The file's existing rows are CRLF-terminated (carried over from however it was
    # first authored); only the rows this script actually rewrites get LF, so the
    # diff stays scoped to real changes instead of touching all 450 rows' line endings.
    def render_row(fields):
        buf = io.StringIO()
        csv.writer(buf, lineterminator="").writerow(fields)
        return buf.getvalue()

    with open(args.geojsons, "w", newline="", encoding="utf-8") as f:
        f.write(render_row(fieldnames) + "\r\n")
        for row, was_touched in zip(rows, touched):
            f.write(render_row([row[fn] for fn in fieldnames]) + ("\n" if was_touched else "\r\n"))
    print(f"\nWrote {args.geojsons}")


if __name__ == "__main__":
    main()
