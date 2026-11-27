// Unit tests for the map line toolkit in js/line.js (docs section 31), with
// node's built-in test runner (no dependencies):
//
//   node --test dev/line_toolkit_test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { decodePolyline, haversine } from "../js/util.js";
import {
  placeAt, distanceAlong, nearestOnLine, viaStopStretch, trimToStops, simplify, spliceStretch, stopsOffLine, lineLength,
  parseLineFile, toGeoJSON, toGPX, toKML, encodePolyline,
} from "../js/line.js";

// a straight road east along 13.0 N, a point every ~108 m (0.001 degree)
const east = (n, lat = 13.0, lon0 = 80.2) => Array.from({ length: n }, (_, i) => [lat, +(lon0 + i * 0.001).toFixed(5)]);
const near = (a, b, m = 1.5) => haversine(a[0], a[1], b[0], b[1]) <= m;
const stop = (id, lat, lon, stop_type = "INTERMEDIATE STOP") => ({ stop_id: id, stop_name: id, lat, lon, stop_type });

test("a place a distance along the line, and back", () => {
  const line = east(5);
  const at = placeAt(line, 250);
  assert.equal(at.seg, 2);
  assert.ok(Math.abs(distanceAlong(line, at) - 250) < 0.5);
  assert.ok(near([at.lat, at.lon], [13.0, 80.20231]));
  // clamped to the ends
  assert.deepEqual([placeAt(line, -5).seg, placeAt(line, -5).t], [0, 0]);
  const end = placeAt(line, 1e6);
  assert.deepEqual([end.seg, end.t], [3, 1]);
});

test("through a stop: the stretch around where the line passes it, routed through it, takes the flag away", () => {
  const line = east(12);
  const sr = stop("SR", 13.0012, 80.205); // ~133 m north of the road
  const s = viaStopStretch(line, sr.lat, sr.lon, 150);
  const mid = distanceAlong(line, nearestOnLine(line, sr.lat, sr.lon));
  assert.ok(Math.abs(distanceAlong(line, s.from) - (mid - 150)) < 0.5);
  assert.ok(Math.abs(distanceAlong(line, s.to) - (mid + 150)) < 0.5);
  assert.equal(stopsOffLine(line, [sr]).length, 1);
  const fixed = spliceStretch(line, s.from, s.to, [[sr.lat, s.from.lon], [sr.lat, sr.lon], [sr.lat, s.to.lon]]);
  assert.equal(stopsOffLine(fixed, [sr]).length, 0);
  // near the start: clamped to the line's start
  const s0 = viaStopStretch(line, 13.0012, 80.2001, 150);
  assert.deepEqual([s0.from.seg, s0.from.t], [0, 0]);
});

test("trim to the first and last boarded stops: the depot runs go", () => {
  const line = east(20);  // 80.200 .. 80.219
  const rows = [
    stop("DEPOT", 13.0, 80.2, "HIDDEN STOP"),
    stop("A", 13.0003, 80.2035, "NEW STOP"),
    stop("B", 13.0, 80.21),
    stop("C", 12.9997, 80.2155),
  ];
  const t = trimToStops(line, rows);
  assert.ok(near(t[0], [13.0, 80.2035]) && near(t[t.length - 1], [13.0, 80.2155]), `${t[0]} .. ${t[t.length - 1]}`);
  assert.ok(Math.abs(lineLength(t) - 12 * 108.4) < 5, `${lineLength(t)}`);
  assert.equal(stopsOffLine(t, rows.slice(1)).length, 0, "every stop still on it");
  // a stop list it cannot cut by: the line as it was
  assert.deepEqual(trimToStops(line, rows.slice(0, 2)), line);
});

test("simplify: a straight run loses its middle points, a bend keeps its corner", () => {
  const straight = east(30);
  assert.deepEqual(simplify(straight, 5), [straight[0], straight[29]]);
  const bend = [...east(10), [13.001, 80.209], [13.002, 80.209], [13.003, 80.209]];
  const s = simplify(bend, 5);
  assert.deepEqual(s, [bend[0], bend[9], bend[12]]);
  // a wobble of ~3 m goes at 5 m and stays at 2 m
  const wobble = east(5).map(([la, lo], i) => [i % 2 ? la + 0.00003 : la, lo]);
  assert.equal(simplify(wobble, 5).length, 2);
  assert.equal(simplify(wobble, 2).length, 5);
});

test("GeoJSON in: a LineString, a Feature, the longest line of a collection, a MultiLineString in order", () => {
  const coords = [[80.2, 13.0], [80.201, 13.0], [80.202, 13.001]];
  const want = [[13.0, 80.2], [13.0, 80.201], [13.001, 80.202]];
  assert.deepEqual(parseLineFile(JSON.stringify({ type: "LineString", coordinates: coords })), { points: want, format: "GeoJSON" });
  assert.deepEqual(parseLineFile(toGeoJSON(want, { route_id: "1" })).points, want);
  const fc = { type: "FeatureCollection", features: [
    { type: "Feature", geometry: { type: "Point", coordinates: [80, 13] } },
    { type: "Feature", geometry: { type: "LineString", coordinates: coords.slice(0, 2) } },
    { type: "Feature", geometry: { type: "LineString", coordinates: coords } },
  ] };
  assert.deepEqual(parseLineFile(JSON.stringify(fc)).points, want);
  const multi = { type: "MultiLineString", coordinates: [coords.slice(0, 2), coords.slice(1)] };
  assert.deepEqual(parseLineFile(JSON.stringify(multi)).points, want, "the shared point once");
  assert.throws(() => parseLineFile(JSON.stringify({ type: "Point", coordinates: [80, 13] })), /no LineString/);
  assert.throws(() => parseLineFile("{nope", "x.geojson"), /not valid JSON/);
});

test("GPX in: a track's points (either attribute order), else a route's", () => {
  const gpx = `<?xml version="1.0"?><gpx version="1.1"><trk><trkseg>
    <trkpt lat="13.0" lon="80.2"><ele>5</ele></trkpt><trkpt lon="80.201" lat="13.0"/><trkpt lat='13.001' lon='80.202'/>
  </trkseg></trk></gpx>`;
  assert.deepEqual(parseLineFile(gpx, "r.gpx"), { points: [[13.0, 80.2], [13.0, 80.201], [13.001, 80.202]], format: "GPX" });
  const rte = `<gpx><rte><rtept lat="13" lon="80.2"/><rtept lat="13" lon="80.3"/></rte></gpx>`;
  assert.deepEqual(parseLineFile(rte).points, [[13, 80.2], [13, 80.3]]);
  const line = east(4);
  assert.deepEqual(parseLineFile(toGPX(line, "21G <up>")).points, line);
  assert.ok(toGPX(line, "21G <up>").includes("<name>21G &lt;up&gt;</name>"));
});

test("KML in: the longest LineString's coordinates, altitude ignored", () => {
  const kml = `<kml xmlns="http://www.opengis.net/kml/2.2"><Document>
    <Placemark><LineString><coordinates>80.2,13.0 80.3,13.0</coordinates></LineString></Placemark>
    <Placemark><LineString><coordinates>
      80.2,13.0,0 80.201,13.0,0
      80.202,13.001,0
    </coordinates></LineString></Placemark></Document></kml>`;
  assert.deepEqual(parseLineFile(kml, "r.kml"), { points: [[13.0, 80.2], [13.0, 80.201], [13.001, 80.202]], format: "KML" });
  const line = east(4);
  assert.deepEqual(parseLineFile(toKML(line, "21G")).points, line);
});

test("an encoded polyline or lat,lon lines; what is not a line is said", () => {
  const line = east(4);
  assert.deepEqual(parseLineFile(encodePolyline(line)), { points: line, format: "encoded polyline" });
  assert.deepEqual(parseLineFile("13.0, 80.2\n13.0,80.201\n\n13.001 80.202\n"), { points: [[13.0, 80.2], [13.0, 80.201], [13.001, 80.202]], format: "lat,lon lines" });
  assert.throws(() => parseLineFile(""), /empty/);
  assert.throws(() => parseLineFile("13.0,80.2"), /fewer than two points/);
  assert.throws(() => parseLineFile("<html><body>hi</body></html>"), /not a latitude and longitude/);
  assert.throws(() => parseLineFile("95,80\n13,80"), /not a latitude and longitude/);
  assert.equal(decodePolyline(encodePolyline(parseLineFile(toGeoJSON(line)).points)).length, 4);
});
