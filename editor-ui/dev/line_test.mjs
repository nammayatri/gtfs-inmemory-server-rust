// Unit tests for js/line.js, with node's built-in test runner (no dependencies):
//
//   node --test dev/line_test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { decodePolyline, haversine } from "../js/util.js";
import {
  encodePolyline, tidy, lineLength, nearestOnLine, compareAlong, spliceStretch, stretchOf, throughStops, stopsOffLine, OFF_LINE_M,
} from "../js/line.js";

// a straight road east along 13.0 N, a point every ~108 m (0.001 degree)
const east = (n, lat = 13.0, lon0 = 80.2) => Array.from({ length: n }, (_, i) => [lat, +(lon0 + i * 0.001).toFixed(5)]);
const near = (a, b, m = 1.5) => haversine(a[0], a[1], b[0], b[1]) <= m;

test("encoding is Google's, and decodes to the same points", () => {
  // the example in Google's documentation
  assert.equal(encodePolyline([[38.5, -120.2], [40.7, -120.95], [43.252, -126.453]]), "_p~iF~ps|U_ulLnnqC_mqNvxq`@");
  const pts = [[13.08268, 80.27072], [13.0827, 80.27], [12.99001, 80.21789], [-33.86, 151.2]];
  assert.deepEqual(decodePolyline(encodePolyline(pts)), pts);
  assert.equal(encodePolyline([]), "");
});

test("tidy rounds to the encoding and drops repeated points", () => {
  assert.deepEqual(tidy([[13.000001, 80.2], [13.000004, 80.200002], [13.00001, 80.2]]), [[13, 80.2], [13.00001, 80.2]]);
});

test("the nearest place on the line: the segment, how far along, how far off", () => {
  const line = east(5);
  const at = nearestOnLine(line, 13.0005, 80.2015); // ~55 m north of halfway along segment 1
  assert.equal(at.seg, 1);
  assert.ok(Math.abs(at.t - 0.5) < 0.01, `t ${at.t}`);
  assert.ok(Math.abs(at.distM - 55.3) < 1, `distM ${at.distM}`);
  assert.ok(near([at.lat, at.lon], [13.0, 80.2015]));
  // beyond the end: the end point itself
  const past = nearestOnLine(line, 13.0, 80.21);
  assert.deepEqual([past.seg, past.t], [3, 1]);
  assert.equal(nearestOnLine([[13, 80]], 13, 80), null);
  assert.ok(compareAlong({ seg: 1, t: 0.9 }, { seg: 2, t: 0.1 }) < 0);
  assert.ok(compareAlong({ seg: 2, t: 0.5 }, { seg: 2, t: 0.1 }) > 0);
});

test("a stretch replaced: the points between are gone, the rest kept, either order", () => {
  const line = east(10);
  const from = nearestOnLine(line, 13.0, 80.2025); // halfway along segment 2
  const to = nearestOnLine(line, 13.0, 80.2065);   // halfway along segment 6
  const detour = [[13.001, 80.203], [13.001, 80.206]];
  const out = spliceStretch(line, from, to, detour);
  assert.deepEqual(out.slice(0, 3), line.slice(0, 3), "up to the start kept");
  assert.ok(near(out[3], [13.0, 80.2025]), "the start on the line");
  assert.deepEqual(out.slice(4, 6), detour);
  assert.ok(near(out[6], [13.0, 80.2065]), "the end on the line");
  assert.deepEqual(out.slice(7), line.slice(7), "after the end kept");
  assert.equal(out.length, 10);
  assert.deepEqual(spliceStretch(line, to, from, detour), out, "picked end first: the same");
  // nothing between: straight from the start to the end
  const cut = spliceStretch(line, from, to);
  assert.equal(cut.length, 3 + 2 + 3);
  // starting on a point itself repeats nothing
  const onPoint = spliceStretch(line, nearestOnLine(line, 13.0, 80.202), to, []);
  assert.equal(new Set(onPoint.map(String)).size, onPoint.length);
  assert.ok(Math.abs(lineLength(cut) - lineLength(line)) < 2, "a straight road cut straight is as long");
});

test("the stretch between two places, for drawing", () => {
  const line = east(6);
  const s = stretchOf(line, nearestOnLine(line, 13.0, 80.2035), nearestOnLine(line, 13.0, 80.2005));
  assert.equal(s.length, 2 + 3);
  assert.ok(near(s[0], [13.0, 80.2005]) && near(s[4], [13.0, 80.2035]));
});

test("through the stops: boarded stops and shaping markers in order, not jump or hidden stops", () => {
  const rows = [
    { stop_type: "NEW STOP", stop_id: "A", lat: 13.0, lon: 80.2 },
    { stop_type: "ROUTE CORRECTION", marker_lat: 13.001, marker_lon: 80.201 },
    { stop_type: "JUMP STOP", stop_id: "J", lat: 13.5, lon: 80.5 },
    { stop_type: "HIDDEN STOP", stop_id: "H", lat: 13.6, lon: 80.6 },
    { stop_type: "INTERMEDIATE STOP", stop_id: "B", lat: 13.002, lon: 80.202 },
    { stop_type: "INTERMEDIATE STOP", stop_id: "X", lat: null, lon: null },
  ];
  assert.deepEqual(throughStops(rows), [[13.0, 80.2], [13.001, 80.201], [13.002, 80.202]]);
});

test("stops the line passes more than 100 m from, in order, each once", () => {
  const line = east(10);
  const rows = [
    { stop_type: "NEW STOP", stop_id: "ON", stop_name: "ON THE ROAD", lat: 13.0003, lon: 80.201 },   // ~33 m
    { stop_type: "INTERMEDIATE STOP", stop_id: "SR", stop_name: "SERVICE ROAD", lat: 13.0012, lon: 80.204 }, // ~133 m
    { stop_type: "JUMP STOP", stop_id: "J", lat: 13.01, lon: 80.205 },
    { stop_type: "ROUTE CORRECTION", marker_lat: 13.01, marker_lon: 80.205 },
    { stop_type: "INTERMEDIATE STOP", stop_id: "SR", stop_name: "SERVICE ROAD", lat: 13.0012, lon: 80.204 },
    { stop_type: "INTERMEDIATE STOP", stop_id: "EDGE", lat: 13.0009, lon: 80.206 },                     // ~99.5 m
  ];
  const off = stopsOffLine(line, rows);
  assert.deepEqual(off.map((s) => [s.index, s.stop_id, s.name]), [[1, "SR", "SERVICE ROAD"]]);
  assert.ok(Math.abs(off[0].distM - 132.7) < 1.5, `distM ${off[0].distM}`);
  assert.equal(OFF_LINE_M, 100);
  assert.deepEqual(stopsOffLine([[13, 80.2]], rows), [], "no line, nothing flagged");
  // a detour through the service road takes the flag away
  const fixed = spliceStretch(line, nearestOnLine(line, 13.0, 80.2035), nearestOnLine(line, 13.0, 80.2045), [[13.0012, 80.204]]);
  assert.deepEqual(stopsOffLine(fixed, rows), []);
});
