// Small DOM, formatting, geometry and route-validation helpers shared by screens.

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
export function modal(title, build, { actions = [] } = {}) {
  return new Promise((resolve) => {
    const dlg = h("dialog", { "aria-label": title });
    const close = (value) => { dlg.close(); dlg.remove(); resolve(value); };
    dlg.addEventListener("cancel", (ev) => { ev.preventDefault(); close(undefined); });
    const body = h("div.dialog-body", h("h2", title), build(close));
    const bar = h("div.dialog-actions", actions.map((a) => a(close)));
    dlg.append(body, actions.length ? bar : null);
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

export function debounce(fn, ms) {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
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

// ------------------------------------------------------------------ route rules
export const SERVED_EXCLUDE = new Set(["ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"]);

// The same rules the server applies to a route_stops change (docs section 3),
// run while editing so a mistake shows on the row that caused it. The server
// remains the authority.
export function validateRows(rows) {
  const problems = [];
  const add = (index, code, message, extra = {}) => problems.push({ index, code, message, ...extra });
  let stageNo = null, stageName = null, lastNew = null, prevStop = null, firstChecked = false;
  rows.forEach((r, i) => {
    const n = i + 1;
    const name = r.stop_name || r.marker_name || r.stop_id || "?";
    if (r.stop_type === "ROUTE CORRECTION") {
      if (r.marker_lat == null || r.marker_lon == null) add(i, "marker_without_position", `Row ${n} is a map shaping point with no position.`);
      return;
    }
    if (!r.stop_id) add(i, "unknown_stop", `Stop ${n} has no stop chosen.`);
    if (!firstChecked) {
      firstChecked = true;
      if (r.stop_type !== "NEW STOP") add(i, "first_not_stage_stop", `The route must start at a stage stop. Stop 1 (${name}) is not one.`);
    }
    const no = Number.parseInt(r.stage_no, 10);
    if (Number.isNaN(no)) {
      add(i, "missing_stage", `Stop ${n} (${name}) has no stage number.`);
      return;
    }
    if (r.stop_type === "NEW STOP") {
      if (lastNew !== null && no < lastNew) add(i, "stage_decreases", `Stage numbers go back from ${lastNew} to ${no} at stop ${n} (${name}).`);
      lastNew = no;
      stageNo = no;
      stageName = r.stage_name || "";
    } else if (r.stop_type === "INTERMEDIATE STOP" && stageNo !== null) {
      if (no !== stageNo) {
        add(i, "intermediate_wrong_stage", `Stop ${n} (${name}) is an intermediate stop in stage ${stageNo} but carries stage ${no}.`,
          { fix: { stage_no: stageNo, stage_name: stageName } });
      } else if ((r.stage_name || "") !== stageName) {
        add(i, "intermediate_wrong_stage_name", `Stop ${n} (${name}) is in stage ${stageNo}, named "${stageName}", but carries the name "${r.stage_name || ""}".`,
          { fix: { stage_no: stageNo, stage_name: stageName } });
      }
    }
    if (r.stop_id && prevStop === r.stop_id) add(i, "repeated_stop", `Stop ${n - 1} and stop ${n} are the same stop (${name}).`);
    prevStop = r.stop_id || prevStop;
  });
  return problems;
}

// Group ordered rows into fare stages: a stage starts at a NEW STOP; rows before
// the first one (should not happen) form a stage of their own.
export function groupStages(rows) {
  const stages = [];
  rows.forEach((r, i) => {
    if (r.stop_type === "NEW STOP" || stages.length === 0) {
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
