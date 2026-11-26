// A route's map line as points, for editing it by hand (docs section 28):
// encoding it back, finding where a click lands on it, replacing a stretch of
// it, and the stops it passes by. No DOM: tested by dev/line_test.mjs.
//
// A line is [[lat, lon], ...]. A place on it is {seg, t, lat, lon, distM}: on
// the segment from point `seg` to point `seg + 1`, `t` of the way along (0 to
// 1), and `distM` from what was asked.
import { haversine, SERVED_EXCLUDE } from "./util.js";

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
