// Small DOM, formatting, geometry and route-validation helpers shared by screens.
import { toCsv } from "./csv.js";

// ------------------------------------------------------------------ DOM
// h("div.class#id", {attrs, on: {click}}, ...children). Children may be
// strings, nodes, arrays, or null/false (skipped). Text is always escaped.
export function h(tag, attrs, ...children) {
  if (attrs === null || typeof attrs !== "object" || attrs instanceof Node || Array.isArray(attrs)) {
    children.unshift(attrs);
    attrs = {};
  }
  const [, name = "div", rest = ""] = tag.match(/^([a-z0-9-]*)(.*)$/i);
  const el = document.createElement(name || "div");
  for (const part of rest.match(/[.#][^.#]+/g) || []) {
    if (part[0] === ".") el.classList.add(part.slice(1));
    else el.id = part.slice(1);
  }
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "on") {
      for (const [ev, fn] of Object.entries(v)) el.addEventListener(ev, fn);
    } else if (k === "class") {
      el.className += (el.className ? " " : "") + v;
    } else if (k === "dataset") {
      Object.assign(el.dataset, v);
    } else if (k in el && typeof v !== "string") {
      el[k] = v;
    } else {
      el.setAttribute(k, v === true ? "" : v);
    }
  }
  append(el, children);
  return el;
}

function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.appendChild(c instanceof Node ? c : document.createTextNode(String(c)));
  }
}

export function clear(el, ...children) {
  el.replaceChildren();
  append(el, children);
  return el;
}

// ------------------------------------------------------------------ stop details
// What the forms say about the two texts a passenger reads at a stop.
export const PLATFORM_PLACEHOLDER = "Towards <next stop>";
export const PLATFORM_HELP = "What passengers see as the platform or direction at this stop, for example Towards Guindy. A stop needs no station to have one.";
export const DESCRIPTION_MAX = 500;

// The description of a stop or station: a textarea with a counter, at most
// DESCRIPTION_MAX characters. Returns {input, el}; `el` is the labelled field.
export function descriptionField(id, value, what = "stop") {
  const input = h("textarea", { id, rows: "3", maxlength: String(DESCRIPTION_MAX), placeholder: what === "station" ? "For example: stops on both sides of the junction, outside the metro entrance" : "For example: outside the post office, opposite the temple tank" });
  input.value = value || "";
  const count = h("span.hint.char-count", { "aria-live": "polite" });
  const show = () => { count.textContent = `${input.value.length} of ${DESCRIPTION_MAX} characters`; };
  input.addEventListener("input", show);
  show();
  return { input, el: h("label.field", { for: id }, h("span", "Description (optional)"), input, count) };
}

// A stop's platform label and description in a few words, for a hover title or
// a tooltip; "" when it has neither.
export function stopDetailWords(s) {
  return [s && s.platform_code, s && s.description].filter(Boolean).join(" · ");
}

// ------------------------------------------------------------------ feedback
export function toast(message, kind = "") {
  const box = document.getElementById("toasts");
  const t = h("div.toast", { class: kind }, message);
  box.appendChild(t);
  setTimeout(() => t.remove(), kind === "error" ? 8000 : 4000);
}

export function errorText(e) {
  return (e && e.message) || "Something went wrong.";
}

// A modal built on <dialog>, which traps focus and closes on Escape natively.
// `build(close)` returns the body; resolves with whatever close() was given.
export function modal(title, build, { actions = [], wide = false } = {}) {
  return new Promise((resolve) => {
    const dlg = h("dialog", { "aria-label": title, class: wide ? "wide" : null });
    const close = (value) => { dlg.close(); dlg.remove(); resolve(value); };
    dlg.addEventListener("cancel", (ev) => { ev.preventDefault(); close(undefined); });
    const body = h("div.dialog-body", h("h2", title), build(close));
    const bar = h("div.dialog-actions", actions.map((a) => a(close)));
    // append() would write a null out as the text "null"
    dlg.append(body);
    if (actions.length) dlg.append(bar);
    document.body.appendChild(dlg);
    dlg.showModal();
    const first = dlg.querySelector("input, textarea, select");
    if (first) first.focus();
  });
}

export function confirmDialog(title, message, { confirm = "Confirm", danger = false } = {}) {
  return modal(title, () => h("p", message), {
    actions: [
      (close) => h("button.btn.secondary", { type: "button", on: { click: () => close(false) } }, "Cancel"),
      (close) => h("button.btn", { type: "button", class: danger ? "danger" : "", on: { click: () => close(true) } }, confirm),
    ],
  });
}

// ------------------------------------------------------------------ formatting
export function fmtDate(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  return d.toLocaleString("en-IN", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
}

export function fmtMetres(m) {
  if (m === null || m === undefined) return "";
  return m >= 1000 ? `${(m / 1000).toFixed(m >= 10000 ? 0 : 1)} km` : `${Math.round(m)} m`;
}

export function fmtCoord(v) {
  return typeof v === "number" ? v.toFixed(6) : "";
}

export function fmtCount(n) {
  return Number(n || 0).toLocaleString("en-IN");
}

// "1 stop", "2 stops"; `many` when the plural is not just an added s.
export function plural(n, one, many = `${one}s`) {
  return `${fmtCount(n)} ${n === 1 ? one : many}`;
}

// Stop and route ids: GIMS splits ids on ':', so the editor allows only these.
export const ID_RE = /^[A-Za-z0-9_.-]{1,64}$/;
export const ID_RULE = "Use up to 64 letters, digits, _ - or . (no spaces).";

// Names compared the way people read them: case, punctuation and spacing aside.
export function normName(name) {
  return String(name || "").toLowerCase().replace(/[^a-z0-9\u0B80-\u0BFF]+/g, " ").trim();
}

// 2 for the same name, 1 for a similar one (one contains the other, or they
// share most words), 0 otherwise.
export function nameLikeness(a, b) {
  const x = normName(a), y = normName(b);
  if (!x || !y) return 0;
  if (x === y) return 2;
  if (x.includes(y) || y.includes(x)) return 1;
  const wa = new Set(x.split(" ").filter((w) => w.length > 2)), wb = new Set(y.split(" ").filter((w) => w.length > 2));
  const shared = [...wa].filter((w) => wb.has(w)).length;
  return shared && shared >= Math.min(wa.size, wb.size) / 2 ? 1 : 0;
}

export function debounce(fn, ms) {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

// Whether a route is listed for passengers (docs section 18.17): an inactive
// route is in no list GIMS gives and only answers when asked for by its id.
// `both` tags an active route too, where the two are shown side by side.
export function routeStatusTag(r, { both = false } = {}) {
  if (r && r.active === false) {
    return h("span.chip.route-inactive", { "data-route-active": "false", title: "Inactive: in no list GIMS gives and not in the published GTFS; it still answers when asked for by its id" }, "Inactive");
  }
  return both ? h("span.chip.route-active", { "data-route-active": "true", title: "Active: listed for passengers" }, "Active") : null;
}

export const STOP_TYPE_LABEL = {
  "NEW STOP": "Stage stop",
  "INTERMEDIATE STOP": "Intermediate stop",
  "JUMP STOP": "Jump stop (not bookable)",
  "ROUTE CORRECTION": "Map shaping point",
  "HIDDEN STOP": "Hidden stop",
};

export const ROLE_LABEL = {
  viewer: "Viewer", editor: "Editor", approver: "Approver", admin: "Admin",
};

export const STATUS_LABEL = {
  draft: "Draft", submitted: "Waiting for review", approved: "Approved",
  rejected: "Rejected", committed: "Live", discarded: "Discarded",
};

// ------------------------------------------------------------------ geometry
export function haversine(aLat, aLon, bLat, bLon) {
  const R = 6371000, toRad = (x) => (x * Math.PI) / 180;
  const dLat = toRad(bLat - aLat), dLon = toRad(bLon - aLon);
  const s = Math.sin(dLat / 2) ** 2 + Math.cos(toRad(aLat)) * Math.cos(toRad(bLat)) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.sqrt(s));
}

export function decodePolyline(str) {
  const pts = [];
  let i = 0, lat = 0, lon = 0;
  while (i < str.length) {
    for (const which of [0, 1]) {
      let shift = 0, result = 0, b;
      do {
        if (i >= str.length) throw new Error("truncated polyline");
        b = str.charCodeAt(i++) - 63;
        result |= (b & 0x1f) << shift;
        shift += 5;
      } while (b >= 0x20);
      const d = result & 1 ? ~(result >> 1) : result >> 1;
      if (which === 0) lat += d; else lon += d;
    }
    pts.push([lat / 1e5, lon / 1e5]);
  }
  return pts;
}

export function lineDeviation(a, b, { step = 10, off = 50 } = {}) {
  if (a.length < 2 || b.length < 2) return null;
  const lat0 = a[0][0], kx = 111320 * Math.cos((lat0 * Math.PI) / 180), ky = 110540;
  const xy = (p) => [(p[1] - a[0][1]) * kx, (p[0] - lat0) * ky];
  const resample = (pts) => {
    const out = [pts[0]];
    let carry = 0;
    for (let i = 1; i < pts.length; i++) {
      const [x0, y0] = xy(pts[i - 1]), [x1, y1] = xy(pts[i]);
      const len = Math.hypot(x1 - x0, y1 - y0);
      let at = step - carry;
      while (at <= len) {
        const t = at / len;
        out.push([pts[i - 1][0] + t * (pts[i][0] - pts[i - 1][0]), pts[i - 1][1] + t * (pts[i][1] - pts[i - 1][1])]);
        at += step;
      }
      carry = len - (at - step);
    }
    if (out[out.length - 1] !== pts[pts.length - 1]) out.push(pts[pts.length - 1]);
    return out;
  };
  const length = (pts) => pts.reduce((s, p, i) => (i ? s + Math.hypot(...[0, 1].map((k) => xy(p)[k] - xy(pts[i - 1])[k])) : 0), 0);
  const toLine = (p, segs) => {
    let best = Infinity;
    for (const [x0, y0, dx, dy, l2] of segs) {
      const t = l2 ? Math.max(0, Math.min(1, ((p[0] - x0) * dx + (p[1] - y0) * dy) / l2)) : 0;
      best = Math.min(best, Math.hypot(p[0] - x0 - t * dx, p[1] - y0 - t * dy));
    }
    return best;
  };
  const segsOf = (pts) => pts.slice(1).map((p, i) => {
    const [x0, y0] = xy(pts[i]), [x1, y1] = xy(p);
    const dx = x1 - x0, dy = y1 - y0;
    return [x0, y0, dx, dy, dx * dx + dy * dy];
  });
  const offM = [0, 0];
  let longest = 0, p95 = 0;
  [[a, b], [b, a]].forEach(([from, to], side) => {
    const segs = segsOf(to), dists = [];
    let run = 0;
    const close = () => {
      longest = Math.max(longest, run * step);
      offM[side] += run * step;
      run = 0;
    };
    for (const p of resample(from)) {
      const d = toLine(xy(p), segs);
      dists.push(d);
      if (d > off) run += 1;
      else close();
    }
    close();
    dists.sort((x, y) => x - y);
    p95 = Math.max(p95, dists[Math.round((dists.length - 1) * 0.95)]);
  });
  const la = length(a), lb = length(b);
  return {
    longestM: longest,
    oldOffM: offM[0],
    newOffM: offM[1],
    p95M: p95,
    lengthRatio: la > 0 ? Math.abs(lb - la) / la : Infinity,
  };
}

export function deviationText(d) {
  const tail = `95% of points within ${fmtMetres(d.p95M)}; length ${Math.round(d.lengthRatio * 100)}% different.`;
  if (!d.oldOffM && !d.newOffM) return `Within 50 m of each other all along; ${tail}`;
  return `More than 50 m apart: ${fmtMetres(d.newOffM)} of the new line, ${fmtMetres(d.oldOffM)} of the old; longest stretch ${fmtMetres(d.longestM)}; ${tail}`;
}

// ------------------------------------------------------------------ route rules
export const SERVED_EXCLUDE = new Set(["ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"]);

// Which rows open a fare stage: every stage stop, and a jump stop with a stage
// of its own (a number other than the stage open before it). A jump stop need
// not have one: carrying the stage it is in, it opens nothing.
export function stageOpeners(rows) {
  let open = null;
  return rows.map((r) => {
    const opens = r.stop_type === "NEW STOP"
      || (r.stop_type === "JUMP STOP" && (!open || String(r.stage_no) !== String(open.stage_no)));
    if (opens) open = r;
    return opens;
  });
}

// The same rules the server applies to a route_stops change (docs section 3),
// run while editing so a mistake shows on the row that caused it. The server
// remains the authority.
export function validateRows(rows) {
  const problems = [];
  const add = (index, code, message, extra = {}) => problems.push({ index, code, message, ...extra });
  let stageNo = null, stageName = null, lastNew = null, prevStop = null, firstChecked = false, served = 0;
  if (!rows.length) {
    add(-1, "route_empty", "The route has no stops yet. Add at least two stops a passenger can board.");
    return problems;
  }
  rows.forEach((r, i) => {
    const n = i + 1;
    const name = r.stop_name || r.marker_name || r.stop_id || "?";
    if (!String(r.stage_name ?? "").trim()) add(i, "stage_name_missing", `Stop ${n} (${name}) has no stage name.`);
    if (r.stop_type === "ROUTE CORRECTION") {
      if (r.marker_lat == null || r.marker_lon == null) add(i, "marker_without_position", `Row ${n} is a map shaping point with no position.`);
      return;
    }
    if (!r.stop_id) add(i, "unknown_stop", `Stop ${n} has no stop chosen.`);
    const isServed = !SERVED_EXCLUDE.has(r.stop_type);
    if (isServed) {
      served++;
      if (!firstChecked) {
        firstChecked = true;
        if (r.stop_type !== "NEW STOP") add(i, "first_not_stage_stop", `The route must start at a stage stop. Stop ${n} (${name}) is not one.`);
      }
    }
    const no = Number.parseInt(r.stage_no, 10);
    if (Number.isNaN(no)) {
      add(i, "missing_stage", `Stop ${n} (${name}) has no stage number.`);
      return;
    }
    if (lastNew !== null && no < lastNew) add(i, "stage_decreases", `Stage numbers go back from ${lastNew} to ${no} at stop ${n} (${name}).`);
    lastNew = lastNew === null ? no : Math.max(lastNew, no);
    if (r.stop_type === "NEW STOP") {
      stageNo = no;
      stageName = r.stage_name || "";
    } else if (r.stop_type === "JUMP STOP") {
      // in the stage before it, or the first stop of a stage of its own
      if (stageNo !== null && no === stageNo && (r.stage_name || "") !== stageName) {
        add(i, "jump_wrong_stage_name", `Stop ${n} (${name}) is a jump stop in stage ${stageNo}, named "${stageName}", but carries the name "${r.stage_name || ""}". Give it that name, or a stage number of its own.`,
          { fix: { stage_no: stageNo, stage_name: stageName } });
      } else {
        stageNo = no;
        stageName = r.stage_name || "";
      }
    } else if (r.stop_type === "INTERMEDIATE STOP" && stageNo !== null) {
      if (no !== stageNo) {
        add(i, "intermediate_wrong_stage", `Stop ${n} (${name}) is an intermediate stop in stage ${stageNo} but carries stage ${no}.`,
          { fix: { stage_no: stageNo, stage_name: stageName } });
      } else if ((r.stage_name || "") !== stageName) {
        add(i, "intermediate_wrong_stage_name", `Stop ${n} (${name}) is in stage ${stageNo}, named "${stageName}", but carries the name "${r.stage_name || ""}".`,
          { fix: { stage_no: stageNo, stage_name: stageName } });
      }
    }
    if (isServed) {
      if (r.stop_id && prevStop === r.stop_id) add(i, "repeated_stop", `This stop is the same as the stop before it (${name}). A bus cannot stop at one stop twice in a row.`);
      prevStop = r.stop_id || prevStop;
    }
  });
  if (served < 2) add(-1, "too_few_stops", "A route needs at least two stops a passenger can board.");
  return problems;
}

// The stage of every row after renumbering: the rows that open a stage (stage
// stops, and jump stops with a stage of their own) count up from `start` in
// route order, and every other row takes the number and name of the stage it
// is in. Returns the new rows and how many rows change.
export function renumberStages(rows, start = 1) {
  const opens = stageOpeners(rows);
  let no = start - 1, stage = null, changed = 0;
  const out = rows.map((r, i) => {
    const next = { ...r };
    if (opens[i]) {
      no += 1;
      next.stage_no = no;
      stage = { stage_no: no, stage_name: r.stage_name };
    } else if (stage) {
      next.stage_no = stage.stage_no;
      next.stage_name = stage.stage_name;
    }
    if (String(next.stage_no) !== String(r.stage_no) || (next.stage_name || "") !== (r.stage_name || "")) changed++;
    return next;
  });
  return { rows: out, changed };
}

// Group ordered rows into fare stages: a stage starts at a NEW STOP or at a
// jump stop with a stage of its own; rows before the first one (should not
// happen) form a stage of their own.
export function groupStages(rows) {
  const stages = [];
  const opens = stageOpeners(rows);
  rows.forEach((r, i) => {
    if (opens[i] || stages.length === 0) {
      stages.push({ stage_no: r.stage_no, stage_name: r.stage_name, rows: [] });
    }
    stages[stages.length - 1].rows.push({ row: r, index: i });
  });
  return stages;
}

// ------------------------------------------------------------------ row diff
const rowKey = (r) => (r.stop_type === "ROUTE CORRECTION" ? `m:${r.marker_id}` : `s:${r.stop_id}`);

// Longest common subsequence over row keys, then classify: rows only in `after`
// are added, only in `before` removed; a key present in both but outside the
// common subsequence moved; matched rows whose type or stage changed changed.
export function diffRows(before, after) {
  const a = before.map(rowKey), b = after.map(rowKey);
  const m = a.length, n = b.length;
  const dp = Array.from({ length: m + 1 }, () => new Uint32Array(n + 1));
  for (let i = m - 1; i >= 0; i--)
    for (let j = n - 1; j >= 0; j--)
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out = [];
  let i = 0, j = 0;
  while (i < m || j < n) {
    if (i < m && j < n && a[i] === b[j]) {
      const x = before[i], y = after[j];
      const changed = x.stop_type !== y.stop_type || String(x.stage_no) !== String(y.stage_no) || (x.stage_name || "") !== (y.stage_name || "");
      out.push({ kind: changed ? "changed" : "same", before: x, after: y, fromIndex: i, toIndex: j });
      i++; j++;
    } else if (j < n && (i >= m || dp[i][j + 1] >= dp[i + 1][j])) {
      out.push({ kind: "added", after: after[j], toIndex: j }); j++;
    } else {
      out.push({ kind: "removed", before: before[i], fromIndex: i }); i++;
    }
  }
  // an added and a removed row with the same key is a move
  const removedByKey = new Map();
  out.forEach((d) => { if (d.kind === "removed") removedByKey.set(rowKey(d.before), d); });
  out.forEach((d) => {
    if (d.kind !== "added") return;
    const r = removedByKey.get(rowKey(d.after));
    if (r) {
      d.kind = "moved";
      d.before = r.before;
      d.fromIndex = r.fromIndex;
      r.kind = "moved-away";
      removedByKey.delete(rowKey(d.after));
    }
  });
  return out.filter((d) => d.kind !== "moved-away");
}

// ------------------------------------------------------------------ location
// The device's position, for placing a stop where the person editing it stands: {lat, lon, accuracy} in metres, or an Error whose message says why not.
// `fresh`: a new fix, not one the browser kept from the last half minute.
export function myLocation({ fresh = false } = {}) {
  if (!("geolocation" in navigator)) return Promise.reject(new Error("This browser cannot tell your location."));
  const ask = (enableHighAccuracy) => new Promise((resolve, reject) => {
    navigator.geolocation.getCurrentPosition(
      (p) => resolve({ lat: p.coords.latitude, lon: p.coords.longitude, accuracy: p.coords.accuracy }),
      reject,
      { enableHighAccuracy, timeout: 15000, maximumAge: fresh ? 0 : 30000 });
  });
  // A laptop has no GPS, and a high-accuracy request can fail where the plain
  // (Wi-Fi) one works: ask again the plain way before giving up.
  return ask(true).catch((e) => (e.code === 1 ? Promise.reject(e) : ask(false))).catch((e) => {
    // the browser's own words help most when the cause is outside the page
    const detail = e.message ? ` (${e.message})` : "";
    throw new Error(e.code === 1 ? "Location is blocked for this site in your browser."
      : e.code === 3 ? `Finding your location took too long${detail}.`
      : `Your browser allows this site, but could not find where you are${detail}. On a Mac, check that the browser itself is allowed in System Settings → Privacy & Security → Location Services, and that Wi-Fi is on.`);
  });
}
// past this the fix is a neighbourhood, not a kerb
export const LOCATION_ROUGH_METRES = 50;

// ------------------------------------------------------------------ download
// Hand the browser a CSV of `records` (the first row is the header) under
// `filename`. The byte order mark makes spreadsheet programs read it as UTF-8,
// so Tamil names survive, as the import templates do.
export function downloadCsv(filename, records) {
  const url = URL.createObjectURL(new Blob(["\ufeff" + toCsv(records)], { type: "text/csv;charset=utf-8" }));
  const a = h("a", { href: url, download: filename, style: "display:none" });
  document.body.appendChild(a);
  a.click();
  a.remove();
  // let the download start before the blob goes
  setTimeout(() => URL.revokeObjectURL(url), 10000);
}
