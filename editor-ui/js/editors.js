// Edit screens. Each builds a change and hands it to drafts.addChange(); none
// of them writes live data.
import { get, post, enc } from "./api.js";
import { state } from "./state.js";
import {
  h, clear, toast, confirmDialog, debounce, fmtMetres, haversine, STOP_TYPE_LABEL, validateRows, decodePolyline,
} from "./util.js";
import * as map from "./map.js";
import { addChange, existingChange, requireDraft } from "./drafts.js";
import { showStop, showRoute } from "./explore.js";

const panel = () => document.getElementById("panel");

function problemList(problems) {
  if (!problems || !problems.length) return null;
  const errors = problems.filter((p) => p.level !== "warning");
  const warnings = problems.filter((p) => p.level === "warning");
  return [
    errors.length ? h("div.notice.error", { role: "alert" }, h("p", h("strong", "Fix before submitting")), h("ul", errors.map((p) => h("li", p.message)))) : null,
    warnings.length ? h("div.notice.warning", h("p", h("strong", "Check")), h("ul", warnings.map((p) => h("li", p.message)))) : null,
  ];
}

// ------------------------------------------------------------------ stop
export async function editStop(stop) {
  if (!(await requireDraft())) return;
  const prior = existingChange("stop", stop.stop_id);
  const start = { name: stop.name, lat: stop.lat, lon: stop.lon, platform_code: stop.platform_code || "", regional_name: stop.regional_name || "", ...(prior ? prior.after : {}) };
  const f = {
    name: h("input", { type: "text", id: "stop-name", value: start.name }),
    lat: h("input", { type: "number", id: "stop-lat", step: "0.000001", value: String(start.lat) }),
    lon: h("input", { type: "number", id: "stop-lon", step: "0.000001", value: String(start.lon) }),
    platform_code: h("input", { type: "text", id: "stop-platform", value: start.platform_code || "" }),
    regional_name: h("input", { type: "text", id: "stop-regional", value: start.regional_name || "", lang: "ta" }),
  };
  const moved = h("p.hint", { "aria-live": "polite" });
  const problems = h("div");
  const setPin = map.dragStop(stop, (lat, lon) => {
    f.lat.value = lat.toFixed(7);
    f.lon.value = lon.toFixed(7);
    showMoved();
  });
  function showMoved() {
    const lat = Number(f.lat.value), lon = Number(f.lon.value);
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) { moved.textContent = "Enter numbers for both coordinates."; return; }
    const d = haversine(stop.lat, stop.lon, lat, lon);
    moved.textContent = d < 0.5 ? "Not moved." : `Moved ${fmtMetres(d)} from where it is now.`;
    moved.className = d > 500 ? "notice warning" : "hint";
    if (d > 500) moved.textContent += " That is far for a kerb fix; check it is the same place.";
  }
  const typed = debounce(() => {
    const lat = Number(f.lat.value), lon = Number(f.lon.value);
    if (Number.isFinite(lat) && Number.isFinite(lon)) setPin(lat, lon);
    showMoved();
  }, 300);
  f.lat.addEventListener("input", typed);
  f.lon.addEventListener("input", typed);
  if (prior) setPin(Number(start.lat), Number(start.lon));
  showMoved();

  const cancel = () => { map.endModes(); showStop(stop.stop_id); };
  const save = async (ev) => {
    ev.preventDefault();
    const after = {};
    const name = f.name.value.trim();
    if (!name) { clear(problems, h("p.notice.error", "A stop needs a name.")); f.name.focus(); return; }
    if (name !== stop.name) after.name = name;
    const lat = Number(f.lat.value), lon = Number(f.lon.value);
    if (lat !== stop.lat || lon !== stop.lon) { after.lat = lat; after.lon = lon; }
    for (const k of ["platform_code", "regional_name"]) {
      const v = f[k].value.trim() || null;
      if (v !== (stop[k] || null)) after[k] = v;
    }
    if (!Object.keys(after).length) { clear(problems, h("p.notice", "Nothing has changed yet.")); return; }
    try {
      const res = await addChange({ entity: "stop", op: "update", entity_key: stop.stop_id, after, base_row_version: stop.row_version });
      if (!res) return;
      if (res.problems.some((p) => p.level === "error")) { clear(problems, problemList(res.problems)); return; }
      map.endModes();
      showStop(stop.stop_id);
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { on: { submit: save } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the stop"),
      h("h1", `Edit ${stop.name}`),
      h("p.ids", `Stop ${stop.stop_id}. Changes go into draft "${state.draft.title}".`),
      h("label.field", { for: "stop-name" }, h("span", "Name passengers see"), f.name),
      h("div.field-row",
        h("label.field", { for: "stop-lat" }, h("span", "Latitude"), f.lat),
        h("label.field", { for: "stop-lon" }, h("span", "Longitude"), f.lon)),
      moved,
      h("p.hint", "Drag the teal pin on the map to the kerb where the bus stops."),
      h("div.field-row",
        h("label.field", { for: "stop-platform" }, h("span", "Platform or bay (optional)"), f.platform_code),
        h("label.field", { for: "stop-regional" }, h("span", "Tamil name (optional)"), f.regional_name)),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  ));
  f.name.focus();
}

export async function deleteStop(stop) {
  if (!(await requireDraft())) return;
  const ok = await confirmDialog("Delete this stop?", `${stop.name} (${stop.stop_id}) is not used by any route. Deleting it takes it out of the feed once the draft is committed.`, { confirm: "Add deletion to draft", danger: true });
  if (!ok) return;
  try {
    await addChange({ entity: "stop", op: "delete", entity_key: stop.stop_id, after: null, base_row_version: stop.row_version }, { merge: false });
    showStop(stop.stop_id);
  } catch (e) {
    toast(e.message, "error");
  }
}

// ------------------------------------------------------------------ route stop list
const toRow = (r) => ({
  stop_id: r.stop_id, stop_name: r.stop_name, lat: r.lat, lon: r.lon, stop_type: r.stop_type,
  stage_no: r.stage_no, stage_name: r.stage_name, marker_id: r.marker_id, marker_name: r.marker_name,
  marker_lat: r.marker_lat, marker_lon: r.marker_lon,
});

export async function editRouteRows(route) {
  if (!(await requireDraft())) return;
  const prior = existingChange("route_stops", route.route_id);
  let baseHash = route.rows_hash;
  let rows = route.rows.map(toRow);
  if (prior) {
    try {
      const preview = await get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(route.route_id)}`);
      rows = preview.rows.map(toRow);
      baseHash = prior.after.base_rows_hash || baseHash;
    } catch (e) {
      toast(e.message, "error");
    }
  }
  let inserting = null;         // index after which a stop is being inserted
  let serverProblems = null;
  const list = h("ol.stops");
  const summary = h("div", { "aria-live": "polite" });
  const fixAllBtn = h("button.btn.secondary.small", { type: "button", hidden: true }, "Set every intermediate stop to its stage");

  const redraw = ({ refit = false } = {}) => {
    const problems = validateRows(rows);
    const byRow = new Map();
    problems.forEach((p) => { if (!byRow.has(p.index)) byRow.set(p.index, []); byRow.get(p.index).push(p); });
    const fixable = problems.filter((p) => p.fix);
    fixAllBtn.hidden = fixable.length < 2;
    clear(summary,
      problems.length
        ? h("div.notice.error", h("p", h("strong", `${problems.length} problem${problems.length === 1 ? "" : "s"} in this stop list`)),
            h("ul", problems.slice(0, 6).map((p) => h("li", h("a", { href: `#row-${p.index}`, on: { click: (ev) => { ev.preventDefault(); document.getElementById(`row-${p.index}`)?.focus(); } } }, p.message)))),
            problems.length > 6 ? h("p", `and ${problems.length - 6} more, marked in red below.`) : null)
        : h("p.notice.ok", "The fare stages are consistent."),
      problemList(serverProblems));
    clear(list, rows.map((r, i) => rowEditor(r, i, byRow.get(i) || [])));
    map.showRoute({ ...route, rows, encoded_polyline: route.encoded_polyline }, { fit: refit });
  };

  const stageBefore = (i) => {
    for (let k = i; k >= 0; k--) if (rows[k].stop_type === "NEW STOP") return rows[k];
    return null;
  };

  function rowEditor(r, i, problems) {
    const isMarker = r.stop_type === "ROUTE CORRECTION";
    const kind = { "NEW STOP": "new", "JUMP STOP": "jump", "ROUTE CORRECTION": "marker" }[r.stop_type] || "";
    const change = (patch) => { Object.assign(rows[i], patch); serverProblems = null; redraw(); };
    const rowTools = tools(i);
    let detail;
    if (isMarker) {
      detail = h("span.meta", "Bends the map line. Not a stop passengers can use.");
    } else {
      const type = h("select", { "aria-label": `Type of stop ${i + 1}`, on: { change: (ev) => {
        const t = ev.target.value;
        if (t !== "NEW STOP") {
          const st = stageBefore(i - 1);
          change({ stop_type: t, ...(st && t === "INTERMEDIATE STOP" ? { stage_no: st.stage_no, stage_name: st.stage_name } : {}) });
        } else {
          change({ stop_type: t });
        }
      } } }, ["NEW STOP", "INTERMEDIATE STOP", "JUMP STOP"].map((t) => h("option", { value: t, selected: t === r.stop_type }, STOP_TYPE_LABEL[t])));
      const line = [type];
      if (r.stop_type === "NEW STOP") {
        line.push(
          h("input.stage-no-input", { type: "number", min: "1", value: String(r.stage_no ?? ""), "aria-label": `Stage number at stop ${i + 1}`,
            on: { change: (ev) => change({ stage_no: ev.target.value === "" ? null : Number(ev.target.value) }) } }),
          h("input.stage-name-input", { type: "text", value: r.stage_name || "", "aria-label": `Stage name at stop ${i + 1}`,
            on: { change: (ev) => change({ stage_name: ev.target.value }) } }));
      } else {
        line.push(h("span.meta", `stage ${r.stage_no ?? "?"}, ${r.stage_name || "no name"}`));
      }
      detail = h("div.row-edit-line", line);
    }
    const errs = problems.map((p) => h("span.row-error", p.message, p.fix
      ? [" ", h("button.btn.quiet.small", { type: "button", on: { click: () => change({ stage_no: p.fix.stage_no, stage_name: p.fix.stage_name }) } }, `Use stage ${p.fix.stage_no}`)]
      : null));
    const li = h("li.row", { class: `${kind} ${problems.length ? "has-error" : ""}`, id: `row-${i}`, tabindex: "-1" },
      h("span.node", { "aria-hidden": "true" }),
      h("div.row-edit",
        h("div.row-head",
          h("span.seq", String(i + 1)),
          h("span.name", isMarker ? `Map shaping point: ${r.marker_name || r.marker_id}` : `${r.stop_name || r.stop_id}`),
          rowTools),
        isMarker ? null : h("span.meta", r.stop_id),
        detail, errs),
      inserting === i ? insertBox(i) : null);
    return li;
  }

  function move(i, delta) {
    const j = i + delta;
    if (j < 0 || j >= rows.length) return;
    [rows[i], rows[j]] = [rows[j], rows[i]];
    serverProblems = null;
    redraw();
    document.getElementById(`row-${j}`)?.focus();
  }

  function tools(i) {
    return h("div.row-tools",
        h("button.icon-btn", { type: "button", title: "Move up", "aria-label": `Move stop ${i + 1} up`, disabled: i === 0, on: { click: () => move(i, -1) } }, "↑"),
        h("button.icon-btn", { type: "button", title: "Move down", "aria-label": `Move stop ${i + 1} down`, disabled: i === rows.length - 1, on: { click: () => move(i, 1) } }, "↓"),
        h("button.icon-btn", { type: "button", title: "Insert a stop after this one", "aria-label": `Insert a stop after stop ${i + 1}`, on: { click: () => { inserting = inserting === i ? null : i; redraw(); document.getElementById("insert-search")?.focus(); } } }, "+"),
        h("button.icon-btn", { type: "button", title: "Remove from the route", "aria-label": `Remove stop ${i + 1}`, on: { click: () => { rows.splice(i, 1); inserting = null; serverProblems = null; redraw(); } } }, "×"));
  }

  function insertAfter(i, stop) {
    const st = stageBefore(i);
    rows.splice(i + 1, 0, {
      stop_id: stop.stop_id, stop_name: stop.name, lat: stop.lat, lon: stop.lon, stop_type: "INTERMEDIATE STOP",
      stage_no: st ? st.stage_no : 1, stage_name: st ? st.stage_name : "",
    });
    inserting = null;
    serverProblems = null;
    redraw();
    document.getElementById(`row-${i + 1}`)?.focus();
    toast(`Added ${stop.name} as stop ${i + 2}.`);
  }

  function insertBox(i) {
    const results = h("ul.list");
    const input = h("input", { type: "search", id: "insert-search", placeholder: "Stop name or id", "aria-label": "Find a stop to insert" });
    const find = debounce(async () => {
      const q = input.value.trim();
      if (q.length < 2) return clear(results);
      try {
        const page = await get(`feeds/${enc(state.feedId)}/stops?q=${enc(q)}&limit=8`);
        clear(results, page.items.filter((s) => s.location_type === 0).map((s) => h("li.list-item",
          h("button.btn.quiet.small", { type: "button", on: { click: () => insertAfter(i, s) } }, "Insert"),
          h("span", s.name), h("span.hint", `${s.route_count} routes`), h("span.sub", s.stop_id))));
      } catch (e) {
        clear(results, h("p.notice.error", e.message));
      }
    }, 250);
    input.addEventListener("input", find);
    return h("div.insert-box",
      h("p", h("strong", `Insert a stop after stop ${i + 1}`)),
      h("div.btn-row", input,
        h("button.btn.secondary.small", { type: "button", on: { click: () => map.pickStop("Click the stop to insert on the map.", (s) => insertAfter(i, s)) } }, "Pick on the map"),
        h("button.btn.quiet.small", { type: "button", on: { click: () => { inserting = null; redraw(); } } }, "Cancel")),
      results);
  }

  fixAllBtn.addEventListener("click", () => {
    validateRows(rows).filter((p) => p.fix).forEach((p) => Object.assign(rows[p.index], p.fix));
    serverProblems = null;
    redraw();
  });

  const cancel = () => { map.endModes(); showRoute(route.route_id); };
  const save = async () => {
    const payload = rows.map((r) => ({
      stop_id: r.stop_type === "ROUTE CORRECTION" ? null : r.stop_id, stop_name: r.stop_name || null, stop_type: r.stop_type,
      stage_no: r.stage_no, stage_name: r.stage_name,
      marker_id: r.marker_id || null, marker_name: r.marker_name || null, marker_lat: r.marker_lat ?? null, marker_lon: r.marker_lon ?? null,
    }));
    try {
      const res = await addChange({ entity: "route_stops", op: "replace", entity_key: route.route_id, after: { rows: payload, base_rows_hash: baseHash } });
      if (!res) return;
      serverProblems = res.problems;
      if (res.problems.some((p) => p.level === "error")) { redraw(); summary.scrollIntoView({ block: "nearest" }); return; }
      map.endModes();
      showRoute(route.route_id, { preview: true });
    } catch (e) {
      toast(e.message, "error");
    }
  };

  clear(panel(),
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the route"),
      h("h1", `Edit stops of ${route.short_name || route.route_id}`),
      h("p", route.long_name || ""),
      h("p.hint", "A stage stop starts a fare stage. Every intermediate stop after it must carry the same stage number and name, or passengers are charged the wrong fare."),
      summary, fixAllBtn),
    h("section.section", h("div.ladder.editing", list)),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "button", on: { click: save } }, prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  );
  redraw({ refit: true });
}

// ------------------------------------------------------------------ route details
export async function editRouteDetails(route) {
  if (!(await requireDraft())) return;
  const prior = existingChange("route", route.route_id);
  const start = { short_name: route.short_name || "", long_name: route.long_name || "", color: route.color || "", ...(prior ? prior.after : {}) };
  let proposed = prior && prior.after.encoded_polyline ? { encoded_polyline: prior.after.encoded_polyline, polyline_source: prior.after.polyline_source } : null;
  const f = {
    short_name: h("input", { type: "text", id: "route-short", value: start.short_name }),
    long_name: h("input", { type: "text", id: "route-long", value: start.long_name }),
    color: h("input", { type: "text", id: "route-color", value: start.color || "", placeholder: "#0B6660", maxlength: "7" }),
  };
  const swatch = h("input", { type: "color", value: /^#[0-9a-f]{6}$/i.test(start.color) ? start.color : "#14252a", "aria-label": "Pick a colour" });
  swatch.addEventListener("input", () => { f.color.value = swatch.value.toUpperCase(); });
  f.color.addEventListener("input", () => { if (/^#[0-9a-f]{6}$/i.test(f.color.value)) swatch.value = f.color.value; });
  const lineStatus = h("div");
  const problems = h("div");

  const showProposal = () => {
    if (!proposed) {
      map.clearRoute("proposal");
      clear(lineStatus, h("p.hint", route.encoded_polyline ? "This route has a saved map line." : "This route has no map line yet."));
      return;
    }
    map.showRoute({ ...route, encoded_polyline: proposed.encoded_polyline }, { layer: "proposal", dashed: true, color: "#0b6660", fit: true });
    let km = "";
    try {
      const pts = decodePolyline(proposed.encoded_polyline);
      let d = 0;
      for (let i = 1; i < pts.length; i++) d += haversine(pts[i - 1][0], pts[i - 1][1], pts[i][0], pts[i][1]);
      km = fmtMetres(d);
    } catch { /* shown as-is */ }
    clear(lineStatus, h("p.notice.ok", `New map line ready (${km}), shown dashed in teal. It is saved when you add these changes to the draft.`),
      h("button.btn.quiet.small", { type: "button", on: { click: () => { proposed = null; showProposal(); } } }, "Discard the new map line"));
  };

  const suggest = async () => {
    clear(lineStatus, h("p.hint", "Asking the road router for a line through the stops…"));
    try {
      const res = await post(`feeds/${enc(state.feedId)}/routes/${enc(route.route_id)}/polyline:osrm`);
      proposed = { encoded_polyline: res.encoded_polyline, polyline_source: res.polyline_source || "osrm" };
      showProposal();
    } catch (e) {
      clear(lineStatus, h("p.notice.error", e.message));
    }
  };

  const cancel = () => { map.clearRoute("proposal"); showRoute(route.route_id); };
  const save = async (ev) => {
    ev.preventDefault();
    const after = {};
    for (const k of ["short_name", "long_name"]) {
      const v = f[k].value.trim();
      if (v !== (route[k] || "")) after[k] = v;
    }
    const color = f.color.value.trim().toUpperCase();
    if (color && !/^#[0-9A-F]{6}$/.test(color)) {
      clear(problems, h("p.notice.error", "Colour must be # followed by six hex digits, for example #0B6660."));
      f.color.setAttribute("aria-invalid", "true");
      return;
    }
    if ((color || null) !== (route.color ? route.color.toUpperCase() : null)) after.color = color || null;
    if (proposed) Object.assign(after, proposed);
    if (!Object.keys(after).length) { clear(problems, h("p.notice", "Nothing has changed yet.")); return; }
    try {
      const res = await addChange({ entity: "route", op: "update", entity_key: route.route_id, after, base_row_version: route.row_version });
      if (!res) return;
      if (res.problems.some((p) => p.level === "error")) { clear(problems, problemList(res.problems)); return; }
      map.clearRoute("proposal");
      showRoute(route.route_id, { preview: true });
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { on: { submit: save } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the route"),
      h("h1", `Edit route ${route.short_name || route.route_id}`),
      h("label.field", { for: "route-short" }, h("span", "Route number"), f.short_name),
      h("label.field", { for: "route-long" }, h("span", "Route name"), f.long_name),
      h("label.field", { for: "route-color" }, h("span", "Colour on maps (optional)"), h("div.btn-row", swatch, h("div", { style: "flex:1" }, f.color))),
    ),
    h("section.section",
      h("h2", "Map line"),
      h("p.hint", "The map line is the road path drawn between the stops. The road router can suggest one through this route's stops."),
      lineStatus,
      h("div.btn-row", h("button.btn.secondary", { type: "button", on: { click: suggest } }, "Suggest a map line")),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  ));
  showProposal();
}

// ------------------------------------------------------------------ stations
export async function editStation(station, initialStops = []) {
  if (!(await requireDraft())) return;
  const creating = !station;
  const members = new Map();       // stop_id -> {stop_id, name, lat, lon}
  if (station) station.children.forEach((c) => members.set(c.stop_id, c));
  initialStops.forEach((s) => members.set(s.stop_id, s));
  const first = [...members.values()][0];
  const prior = station ? existingChange("station", station.stop_id) : null;
  let lat = station ? station.lat : first.lat, lon = station ? station.lon : first.lon;
  if (prior && prior.after) {
    if (prior.after.lat != null) { lat = prior.after.lat; lon = prior.after.lon; }
  }
  const idInput = h("input", { type: "text", id: "station-id", value: creating ? `stn_${first.stop_id}` : station.stop_id, disabled: !creating });
  const nameInput = h("input", { type: "text", id: "station-name", value: (prior && prior.after && prior.after.name) || (station ? station.name : first.name) });
  const latIn = h("input", { type: "number", step: "0.000001", id: "station-lat", value: String(lat) });
  const lonIn = h("input", { type: "number", step: "0.000001", id: "station-lon", value: String(lon) });
  const memberList = h("ul.list");
  const suggestions = h("div");
  const problems = h("div");

  const setPin = map.stationPin(lat, lon, (la, lo) => { latIn.value = la.toFixed(7); lonIn.value = lo.toFixed(7); });
  const selector = map.selectStops([...members.keys()], (s, added) => {
    if (added) members.set(s.stop_id, s); else members.delete(s.stop_id);
    drawMembers();
  });
  [latIn, lonIn].forEach((el) => el.addEventListener("input", debounce(() => {
    const la = Number(latIn.value), lo = Number(lonIn.value);
    if (Number.isFinite(la) && Number.isFinite(lo)) setPin(la, lo);
  }, 300)));

  function centre() {
    const ms = [...members.values()].filter((m) => m.lat != null);
    if (!ms.length) return;
    const la = ms.reduce((a, m) => a + m.lat, 0) / ms.length, lo = ms.reduce((a, m) => a + m.lon, 0) / ms.length;
    latIn.value = la.toFixed(7); lonIn.value = lo.toFixed(7); setPin(la, lo);
  }

  function drawMembers() {
    clear(memberList, members.size
      ? [...members.values()].map((m) => h("li.list-item",
          h("span.key", m.platform_code || ""),
          h("span", m.name),
          h("button.btn.quiet.small", { type: "button", on: { click: () => { members.delete(m.stop_id); selector.set([...members.keys()]); drawMembers(); } } }, "Take out"),
          h("span.sub", m.stop_id)))
      : h("p.empty", "No stops yet. Click stops on the map to add them."));
  }

  if (first) {
    get(`feeds/${enc(state.feedId)}/stops/${enc(first.stop_id)}`).then((d) => {
      const near = d.nearby.filter((n) => n.location_type === 0 && !members.has(n.stop_id));
      if (!near.length) return;
      clear(suggestions, h("p.hint", "Stops within 60 m you may want in this station:"),
        h("ul.list", near.map((n) => h("li.list-item",
          h("span.key", fmtMetres(n.distance_m)), h("span", n.name),
          h("button.btn.quiet.small", { type: "button", on: { click: (ev) => { members.set(n.stop_id, n); selector.set([...members.keys()]); drawMembers(); ev.target.closest("li").remove(); } } }, "Add"),
          h("span.sub", `${n.stop_id}${n.parent_station ? `, already in station ${n.parent_station}` : ""}`)))));
    }).catch(() => {});
  }

  const cancel = () => { map.endModes(); if (station) showStop(station.stop_id); else if (first) showStop(first.stop_id); };
  const save = async (ev) => {
    ev.preventDefault();
    const name = nameInput.value.trim();
    const la = Number(latIn.value), lo = Number(lonIn.value);
    const errs = [];
    if (creating && !/^[A-Za-z0-9_.:-]+$/.test(idInput.value.trim())) errs.push("The station id may use letters, digits, _ . : and - only.");
    if (!name) errs.push("Give the station a name.");
    if (members.size < 2) errs.push("A station groups at least two stops.");
    if (!Number.isFinite(la) || !Number.isFinite(lo)) errs.push("Enter the station position.");
    if (errs.length) { clear(problems, h("div.notice.error", h("ul", errs.map((e) => h("li", e))))); return; }
    const after = { name, lat: la, lon: lo, member_stop_ids: [...members.keys()] };
    if (creating) after.station_id = idInput.value.trim();
    try {
      const res = await addChange({
        entity: "station", op: creating ? "create" : "update",
        entity_key: creating ? after.station_id : station.stop_id, after,
        base_row_version: station ? station.row_version : undefined,
      }, { merge: !creating });
      if (!res) return;
      if (res.problems.some((p) => p.level === "error")) { clear(problems, problemList(res.problems)); return; }
      map.endModes();
      if (station) showStop(station.stop_id); else showStop(first.stop_id);
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { on: { submit: save } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back"),
      h("h1", creating ? "Club stops into a station" : `Edit station ${station.name}`),
      h("p.hint", "A station groups the stops of one place, such as the two sides of a road or the bays of a bus terminus, so passengers can search for the place once."),
      h("label.field", { for: "station-name" }, h("span", "Station name"), nameInput),
      h("label.field", { for: "station-id" }, h("span", "Station id"), idInput),
    ),
    h("section.section",
      h("h2", "Stops in the station"),
      memberList,
      suggestions),
    h("section.section",
      h("h2", "Station position"),
      h("div.field-row",
        h("label.field", { for: "station-lat" }, h("span", "Latitude"), latIn),
        h("label.field", { for: "station-lon" }, h("span", "Longitude"), lonIn)),
      h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: centre } }, "Place in the middle of its stops")),
      h("p.hint", "Or drag the dark square on the map."),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  ));
  drawMembers();
  if (creating) centre();
}

export async function dissolveStation(station) {
  if (!(await requireDraft())) return;
  const ok = await confirmDialog("Dissolve this station?",
    `${station.name} groups ${station.children.length} stop${station.children.length === 1 ? "" : "s"}. Dissolving it keeps every stop and removes only the grouping, once the draft is committed.`,
    { confirm: "Add to draft", danger: true });
  if (!ok) return;
  try {
    await addChange({ entity: "station", op: "delete", entity_key: station.stop_id, after: null, base_row_version: station.row_version }, { merge: false });
    showStop(station.stop_id);
  } catch (e) {
    toast(e.message, "error");
  }
}
