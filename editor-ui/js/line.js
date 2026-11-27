// A route's map line as points, for editing it by hand (docs section 28):
// encoding it back, finding where a click lands on it, replacing a stretch of
// it, and the stops it passes by. No DOM: tested by dev/line_test.mjs.
//
// A line is [[lat, lon], ...]. A place on it is {seg, t, lat, lon, distM}: on
// the segment from point `seg` to point `seg + 1`, `t` of the way along (0 to
// 1), and `distM` from what was asked.
import { haversine, decodePolyline, SERVED_EXCLUDE } from "./util.js";

// GIMS flags a stop its route's line passes more than this from: "the bus may
// not stop here" (docs section 27, OFF_ROUTE_LINE_M).
export const OFF_LINE_M = 100;

// Google's encoded polyline, precision 5, as gtfs_route.encoded_polyline holds it.
export function encodePolyline(pts) {
  let out = "", lat0 = 0, lon0 = 0;
  const put = (v) => {
    let x = v < 0 ? ~(v << 1) : v << 1;
    while (x >= 0x20) {
      out += String.fromCharCode((0x20 | (x & 0x1f)) + 63);
      x >>= 5;
    }
    out += String.fromCharCode(x + 63);
  };
  for (const [lat, lon] of pts) {
    const a = Math.round(lat * 1e5), b = Math.round(lon * 1e5);
    put(a - lat0);
    put(b - lon0);
    lat0 = a;
    lon0 = b;
  }
  return out;
}

// The points as the encoding keeps them (1e-5 of a degree, about a metre),
// with a point that repeats the one before it dropped.
export function tidy(pts) {
  const out = [];
  for (const [lat, lon] of pts) {
    const p = [Math.round(lat * 1e5) / 1e5, Math.round(lon * 1e5) / 1e5];
    const last = out[out.length - 1];
    if (!last || last[0] !== p[0] || last[1] !== p[1]) out.push(p);
  }
  return out;
}

export function lineLength(pts) {
  let d = 0;
  for (let i = 1; i < pts.length; i++) d += haversine(pts[i - 1][0], pts[i - 1][1], pts[i][0], pts[i][1]);
  return d;
}

// Metres east and north of a point: flat enough for a city.
function planar(lat0, lon0) {
  const kx = 111320 * Math.cos((lat0 * Math.PI) / 180), ky = 110540;
  return {
    xy: ([lat, lon]) => [(lon - lon0) * kx, (lat - lat0) * ky],
    ll: ([x, y]) => [lat0 + y / ky, lon0 + x / kx],
  };
}

// The place on the line nearest (lat, lon); null for a line of fewer than two points.
export function nearestOnLine(pts, lat, lon) {
  if (!pts || pts.length < 2) return null;
  const pl = planar(lat, lon);
  let best = null;
  for (let i = 0; i < pts.length - 1; i++) {
    const [x0, y0] = pl.xy(pts[i]), [x1, y1] = pl.xy(pts[i + 1]);
    const dx = x1 - x0, dy = y1 - y0, l2 = dx * dx + dy * dy;
    const t = l2 ? Math.max(0, Math.min(1, -(x0 * dx + y0 * dy) / l2)) : 0;
    const x = x0 + t * dx, y = y0 + t * dy;
    const d = Math.hypot(x, y);
    if (!best || d < best.distM) {
      const [la, lo] = pl.ll([x, y]);
      best = { seg: i, t, lat: la, lon: lo, distM: d };
    }
  }
  return best;
}

// Which of two places comes first along the line: negative, zero or positive.
export const compareAlong = (a, b) => a.seg - b.seg || a.t - b.t;

// The line with the stretch between places `from` and `to` (either order)
// replaced by `middle`: the line up to `from`, `from`, `middle`, `to`, the rest.
export function spliceStretch(pts, from, to, middle = []) {
  const [a, b] = compareAlong(from, to) <= 0 ? [from, to] : [to, from];
  const head = pts.slice(0, a.seg + 1);
  const tail = pts.slice(b.seg + 1);
  return tidy([...head, [a.lat, a.lon], ...middle, [b.lat, b.lon], ...tail]);
}

// The part of the line between two places, for drawing it.
export function stretchOf(pts, from, to) {
  const [a, b] = compareAlong(from, to) <= 0 ? [from, to] : [to, from];
  return [[a.lat, a.lon], ...pts.slice(a.seg + 1, b.seg + 1), [b.lat, b.lon]];
}

// A line through a route's stops and shaping markers in order, for drawing
// one by hand where there is none: what `polyline:osrm` routes through.
export function throughStops(rows) {
  const pts = [];
  for (const r of rows || []) {
    if (r.stop_type === "ROUTE CORRECTION") {
      if (r.marker_lat != null) pts.push([r.marker_lat, r.marker_lon]);
    } else if (!SERVED_EXCLUDE.has(r.stop_type) && r.lat != null) {
      pts.push([r.lat, r.lon]);
    }
  }
  return tidy(pts);
}

// The stops passengers board on this route that the line passes more than
// `limit` metres from, in the route's order, each once: what GIMS would flag
// (docs section 27). [{index, stop_id, name, lat, lon, distM}]
export function stopsOffLine(pts, rows, limit = OFF_LINE_M) {
  if (!pts || pts.length < 2) return [];
  const out = [];
  const seen = new Set();
  (rows || []).forEach((r, index) => {
    if (SERVED_EXCLUDE.has(r.stop_type) || r.lat == null || seen.has(r.stop_id)) return;
    const at = nearestOnLine(pts, r.lat, r.lon);
    if (at && at.distM > limit) {
      seen.add(r.stop_id);
      out.push({ index, stop_id: r.stop_id, name: r.stop_name || r.stop_id, lat: r.lat, lon: r.lon, distM: at.distM });
    }
  });
  return out;
}

// ------------------------------------------------------------------ the toolkit
// (docs section 31)

// The place `distM` metres along the line from its start, clamped to the line.
export function placeAt(pts, distM) {
  let walked = 0;
  for (let i = 0; i < pts.length - 1; i++) {
    const len = haversine(pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]);
    if (walked + len >= distM || i === pts.length - 2) {
      const t = len ? Math.max(0, Math.min(1, (distM - walked) / len)) : 0;
      return { seg: i, t, lat: pts[i][0] + t * (pts[i + 1][0] - pts[i][0]), lon: pts[i][1] + t * (pts[i + 1][1] - pts[i][1]), distM: 0 };
    }
    walked += len;
  }
  return pts.length ? { seg: 0, t: 0, lat: pts[0][0], lon: pts[0][1], distM: 0 } : null;
}

// How far along the line a place on it is, in metres.
export function distanceAlong(pts, at) {
  let d = 0;
  for (let i = 0; i < at.seg; i++) d += haversine(pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]);
  return d + at.t * haversine(pts[at.seg][0], pts[at.seg][1], pts[at.seg + 1][0], pts[at.seg + 1][1]);
}

// To take the line through a stop it passes by: the places `spanM` along the
// line before and after where it passes nearest, which a road route through
// the stop then replaces the stretch between. {from, to}, or null.
export function viaStopStretch(pts, lat, lon, spanM = 150) {
  const at = nearestOnLine(pts, lat, lon);
  if (!at) return null;
  const d = distanceAlong(pts, at);
  return { from: placeAt(pts, Math.max(0, d - spanM)), to: placeAt(pts, d + spanM) };
}

// The line cut to where it passes its first and last boarded stops: a GPS
// line's run out of the depot and back is not the route.
export function trimToStops(pts, rows) {
  const boarded = (rows || []).filter((r) => !SERVED_EXCLUDE.has(r.stop_type) && r.lat != null);
  if (pts.length < 2 || boarded.length < 2) return pts.map((p) => [p[0], p[1]]);
  const a = nearestOnLine(pts, boarded[0].lat, boarded[0].lon);
  const b = nearestOnLine(pts, boarded[boarded.length - 1].lat, boarded[boarded.length - 1].lon);
  return tidy(stretchOf(pts, a, b));
}

// Fewer points: every point the line can lose and stay within `toleranceM`
// metres of where it was (Douglas-Peucker). The ends always stay.
export function simplify(pts, toleranceM = 5) {
  if (pts.length < 3) return pts.map((p) => [p[0], p[1]]);
  const lat0 = pts[0][0], lon0 = pts[0][1];
  const kx = 111320 * Math.cos((lat0 * Math.PI) / 180), ky = 110540;
  const xy = pts.map(([la, lo]) => [(lo - lon0) * kx, (la - lat0) * ky]);
  const keep = new Uint8Array(pts.length);
  keep[0] = keep[pts.length - 1] = 1;
  const stack = [[0, pts.length - 1]];
  while (stack.length) {
    const [a, b] = stack.pop();
    const [x0, y0] = xy[a], [x1, y1] = xy[b];
    const dx = x1 - x0, dy = y1 - y0, l2 = dx * dx + dy * dy;
    let worst = -1, at = -1;
    for (let i = a + 1; i < b; i++) {
      const t = l2 ? Math.max(0, Math.min(1, ((xy[i][0] - x0) * dx + (xy[i][1] - y0) * dy) / l2)) : 0;
      const d = Math.hypot(xy[i][0] - x0 - t * dx, xy[i][1] - y0 - t * dy);
      if (d > worst) { worst = d; at = i; }
    }
    if (worst > toleranceM) {
      keep[at] = 1;
      stack.push([a, at], [at, b]);
    }
  }
  return pts.filter((_, i) => keep[i]).map((p) => [p[0], p[1]]);
}

// ---- files: a line in, a line out

const inWorld = ([la, lo]) => Number.isFinite(la) && Number.isFinite(lo) && Math.abs(la) <= 90 && Math.abs(lo) <= 180;
export const LINE_FILE_MAX_POINTS = 50000;

// A line from a file's text: GeoJSON (a LineString or MultiLineString, bare or
// in a Feature or FeatureCollection: the longest line), GPX (a track's or a
// route's points), KML (a LineString's coordinates: the longest), an encoded
// polyline, or "lat,lon" lines. {points, format}; throws an Error that says
// what was wrong.
export function parseLineFile(text, name = "") {
  const src = String(text || "").replace(/^﻿/, "").trim();
  if (!src) throw new Error("The file is empty.");
  const ext = (name.split(".").pop() || "").toLowerCase();
  const longest = (lines) => lines.reduce((a, b) => (b.length > a.length ? b : a), []);
  let points = null, format = null;
  if (src.startsWith("{") || src.startsWith("[") || ext === "geojson" || ext === "json") {
    let doc;
    try { doc = JSON.parse(src); } catch { throw new Error("The file is not valid JSON."); }
    const lines = [];
    const walk = (g) => {
      if (!g || typeof g !== "object") return;
      if (g.type === "FeatureCollection") (g.features || []).forEach((f) => walk(f));
      else if (g.type === "Feature") walk(g.geometry);
      else if (g.type === "GeometryCollection") (g.geometries || []).forEach(walk);
      else if (g.type === "LineString") lines.push(g.coordinates || []);
      // a MultiLineString is one line in parts: its parts in order
      else if (g.type === "MultiLineString") lines.push((g.coordinates || []).flat());
    };
    walk(doc);
    if (!lines.length) throw new Error("The GeoJSON has no LineString or MultiLineString.");
    points = longest(lines).map((c) => [Number(c[1]), Number(c[0])]);
    format = "GeoJSON";
  } else if (/<gpx[\s>]/i.test(src)) {
    const attr = (tag, a) => { const m = tag.match(new RegExp(`\\b${a}\\s*=\\s*["']([^"']+)["']`, "i")); return m ? Number(m[1]) : NaN; };
    const take = (el) => [...src.matchAll(new RegExp(`<${el}\\b[^>]*>`, "gi"))].map((m) => [attr(m[0], "lat"), attr(m[0], "lon")]);
    points = take("trkpt");
    if (points.length < 2) points = take("rtept");
    format = "GPX";
  } else if (/<kml[\s>]/i.test(src)) {
    const lines = [...src.matchAll(/<LineString\b[^>]*>[\s\S]*?<coordinates>([\s\S]*?)<\/coordinates>/gi)]
      .map((m) => m[1].trim().split(/\s+/).filter(Boolean).map((t) => { const [lo, la] = t.split(","); return [Number(la), Number(lo)]; }));
    if (!lines.length) throw new Error("The KML has no LineString.");
    points = longest(lines);
    format = "KML";
  } else if (/^[\x3f-\x7e]+$/.test(src)) {
    try { points = decodePolyline(src); } catch { throw new Error("The text is not an encoded polyline."); }
    format = "encoded polyline";
  } else {
    points = src.split(/\r?\n/).map((l) => l.trim()).filter(Boolean).map((l) => l.split(/[\s,;]+/).map(Number));
    format = "lat,lon lines";
  }
  if (points.some((p) => !inWorld(p))) throw new Error(`The ${format} has a point that is not a latitude and longitude.`);
  if (points.length < 2) throw new Error(`The ${format} has fewer than two points.`);
  if (points.length > LINE_FILE_MAX_POINTS) throw new Error(`The ${format} has ${points.length} points; at most ${LINE_FILE_MAX_POINTS}.`);
  return { points: tidy(points), format };
}

const xml = (s) => String(s).replace(/[<>&"']/g, (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&apos;" })[c]);

export function toGeoJSON(pts, properties = {}) {
  return JSON.stringify({ type: "Feature", properties, geometry: { type: "LineString", coordinates: pts.map(([la, lo]) => [lo, la]) } });
}

export function toGPX(pts, name = "") {
  return `<?xml version="1.0" encoding="UTF-8"?>\n<gpx version="1.1" creator="GTFS editor" xmlns="http://www.topografix.com/GPX/1/1">\n<trk><name>${xml(name)}</name><trkseg>\n`
    + pts.map(([la, lo]) => `<trkpt lat="${la}" lon="${lo}"/>`).join("\n") + "\n</trkseg></trk>\n</gpx>\n";
}

export function toKML(pts, name = "") {
  return `<?xml version="1.0" encoding="UTF-8"?>\n<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Placemark><name>${xml(name)}</name>\n<LineString><tessellate>1</tessellate><coordinates>\n`
    + pts.map(([la, lo]) => `${lo},${la}`).join(" ") + "\n</coordinates></LineString></Placemark></Document></kml>\n";
}
