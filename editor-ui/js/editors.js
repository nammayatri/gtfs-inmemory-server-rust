// Edit screens. Each builds a change and hands it to drafts.addChange(); none
// of them writes live data.
import { get, post, enc } from "./api.js";
import { state, setLeaveGuard } from "./state.js";
import {
  h, clear, toast, confirmDialog, modal, debounce, fmtMetres, haversine, STOP_TYPE_LABEL, SERVED_EXCLUDE,
  validateRows, renumberStages, decodePolyline, plural, ID_RE, ID_RULE,
} from "./util.js";
import * as map from "./map.js";
import { addChange, existingChange, createdChange, requireDraft, updateChange } from "./drafts.js";
import { stopPicker } from "./picker.js";
import { showStop, showRoute } from "./explore.js";
import { undoScope } from "./undo.js";
import { foldPlatforms, stationNames, platformsNote } from "./context.js";

const panel = () => document.getElementById("panel");
const round7 = (x) => Math.round(x * 1e7) / 1e7;

// A problem's identity independent of row position, so the same problem before
// and after an edit can be matched (counted, like the server's grading).
function problemKey(p, rows) {
  const r = rows[p.index] || {};
  return `${p.code}|${r.stop_id || r.marker_id || ""}|${r.stage_no ?? ""}|${r.stage_name ?? ""}`;
}

function gradeProblems(problems, rows, liveKeys) {
  const remaining = [...liveKeys];
  return problems.map((p) => {
    const i = remaining.indexOf(problemKey(p, rows));
    if (i < 0) return p;
    remaining.splice(i, 1);
    return { ...p, existing: true };
  });
}

export function problemList(problems) {
  if (!problems || !problems.length) return null;
  const errors = problems.filter((p) => p.level !== "warning");
  const warnings = problems.filter((p) => p.level === "warning");
  return [
    errors.length ? h("div.notice.error", { role: "alert" }, h("p", h("strong", "Fix before submitting")), h("ul", errors.map((p) => h("li", p.message)))) : null,
    warnings.length ? h("div.notice.warning", h("p", h("strong", "Check")), h("ul", warnings.map((p) => h("li", p.message)))) : null,
  ];
}

// Unsaved work in a panel: asks before the page is left, cleared on save/cancel.
// A save that the server accepts with problems is still saved: the change is in
// the draft, so the guard is cleared too, and set again by the next edit.
function guard(what) {
  let dirty = false;
  setLeaveGuard(() => (dirty ? what : null));
  return {
    touch() { dirty = true; },
    done() { dirty = false; setLeaveGuard(null); },
  };
}

// ------------------------------------------------------------------ stop
export async function editStop(stop) {
  if (!(await requireDraft())) return;
  const prior = existingChange("stop", stop.stop_id);
  const start = { name: stop.name, lat: stop.lat, lon: stop.lon, platform_code: stop.platform_code || "", regional_name: stop.regional_name || "", ...(prior ? prior.after : {}) };
  const f = {
    name: h("input", { type: "text", id: "stop-name", value: start.name }),
    lat: h("input", { type: "number", id: "stop-lat", step: "any", value: String(start.lat) }),
    lon: h("input", { type: "number", id: "stop-lon", step: "any", value: String(start.lon) }),
    platform_code: h("input", { type: "text", id: "stop-platform", value: start.platform_code || "", maxlength: "120" }),
    regional_name: h("input", { type: "text", id: "stop-regional", value: start.regional_name || "", lang: "ta" }),
  };
  const unsaved = guard(`Your changes to ${stop.name} are not in the draft yet.`);
  const moved = h("p.hint", { "aria-live": "polite" });
  const problems = h("div");
  // a finished drag of the pin, and each field once left, is one step to undo
  const history = undoScope("this stop");
  const syncFields = history.fields([[f.name, "the name"], [f.lat, "the latitude"], [f.lon, "the longitude"],
    [f.platform_code, "the platform label"], [f.regional_name, "the Tamil name"]]);
  let placed = { lat: f.lat.value, lon: f.lon.value };     // as text, so undoing restores it exactly
  const putPin = (p) => {
    placed = p;
    f.lat.value = p.lat;
    f.lon.value = p.lon;
    syncFields();
    setPin(Number(p.lat), Number(p.lon));
    unsaved.touch();
    showMoved();
  };
  const setPin = map.dragStop(stop, (lat, lon) => {
    f.lat.value = lat.toFixed(7);
    f.lon.value = lon.toFixed(7);
    unsaved.touch();
    showMoved();
  }, (lat, lon) => {
    const before = placed, after = { lat: lat.toFixed(7), lon: lon.toFixed(7) };
    placed = after;
    syncFields();
    history.push({ label: "moved the pin", undo: () => putPin(before), redo: () => putPin(after) });
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
    if (Number.isFinite(lat) && Number.isFinite(lon)) { setPin(lat, lon); placed = { lat: f.lat.value, lon: f.lon.value }; }
    showMoved();
  }, 300);
  f.lat.addEventListener("input", typed);
  f.lon.addEventListener("input", typed);
  if (prior) setPin(Number(start.lat), Number(start.lon));
  showMoved();

  const cancel = () => { unsaved.done(); map.endModes(); showStop(stop.stop_id); };
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
      unsaved.done();
      history.clear();
      if (res.problems.some((p) => p.level === "error")) { clear(problems, problemList(res.problems)); return; }
      map.endModes();
      showStop(stop.stop_id);
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { novalidate: true, on: { submit: save, input: () => unsaved.touch() } },
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
      history.buttons(),
      h("div.field-row",
        h("label.field", { for: "stop-platform" }, h("span", "Platform label (optional)"), f.platform_code),
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
// stop_name and lat/lon are for display; stop_name_override (a route's own
// spelling of the stop) and provider_id are data and go back to the server.
const toRow = (r) => ({
  stop_id: r.stop_id, stop_name: r.stop_name, lat: r.lat, lon: r.lon, parent_station: r.parent_station,
  stop_type: r.stop_type, stage_no: r.stage_no, stage_name: r.stage_name,
  marker_id: r.marker_id, marker_name: r.marker_name, marker_lat: r.marker_lat, marker_lon: r.marker_lon,
  stop_name_override: r.stop_name_override ?? null, provider_id: r.provider_id ?? null,
});

// `created`: the route itself is new in the draft, so there is no live list.
export async function editRouteRows(route, { created = false } = {}) {
  if (!(await requireDraft())) return;
  const prior = existingChange("route_stops", route.route_id);
  let baseHash = route.rows_hash;
  let rows = route.rows.map(toRow);
  // Problems the live route already has are shown as warnings: an edit is only
  // blocked by what it introduces (the server grades the same way).
  const liveRows = created ? [] : route.rows.map(toRow);
  const liveProblems = created ? [] : validateRows(liveRows).map((p) => problemKey(p, liveRows));
  if (prior && !created) {
    try {
      const preview = await get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(route.route_id)}`);
      rows = preview.rows.map(toRow);
    } catch (e) {
      toast(e.message, "error");
    }
  }
  if (prior) baseHash = prior.after.base_rows_hash || baseHash;
  const label = route.short_name || route.route_id;
  const unsaved = guard(`Your changes to the stops of route ${label} are not in the draft yet.`);
  let active = null;            // {kind: "insert", at} or {kind: "change", index}
  let activePicker = null;
  let serverProblems = null;
  const typedName = new WeakSet(); // rows whose stage name was typed ("Other name…")
  const list = h("ol.stops");
  const summary = h("div", { "aria-live": "polite" });
  const fixAllBtn = h("button.btn.secondary.small", { type: "button", hidden: true }, "Set every intermediate stop to its stage");

  // Every row operation is one step to undo: the list before and after it. The
  // steps go when the list is saved into the draft.
  const history = undoScope("the stop list");
  const copyRows = (from) => from.map((r) => { const c = { ...r }; if (typedName.has(r)) typedName.add(c); return c; });
  let snap = copyRows(rows);
  const restore = (to) => {
    rows = copyRows(to);
    snap = to;
    active = null;
    serverProblems = null;
    unsaved.touch();
    redraw();
  };
  const edited = (label = "changed the stop list") => {
    serverProblems = null;
    unsaved.touch();
    const before = snap, after = copyRows(rows);
    snap = after;
    history.push({ label, undo: () => restore(before), redo: () => restore(after) });
  };

  // stage names this route already uses, in route order, for the stage name choice
  const stageNames = () => {
    const seen = new Set();
    return [...rows, ...liveRows].filter((r) => r.stop_type !== "ROUTE CORRECTION").map((r) => (r.stage_name || "").trim())
      .filter((n) => n && !seen.has(n) && seen.add(n));
  };

  const redraw = ({ refit = false, focus = null } = {}) => {
    const all = gradeProblems(validateRows(rows), rows, liveProblems);
    const problems = all.filter((p) => !p.existing);
    const existing = all.filter((p) => p.existing);
    const byRow = new Map();
    all.forEach((p) => { if (!byRow.has(p.index)) byRow.set(p.index, []); byRow.get(p.index).push(p); });
    const fixable = all.filter((p) => p.fix);
    fixAllBtn.hidden = fixable.length < 2;
    const jump = (p) => (p.index < 0 ? p.message : h("a", { href: `#row-${p.index}`, on: { click: (ev) => { ev.preventDefault(); document.getElementById(`row-${p.index}`)?.focus(); } } }, p.message));
    clear(summary,
      problems.length
        ? h("div.notice.error", h("p", h("strong", `${plural(problems.length, "problem")} to fix before submitting`)),
            h("ul", problems.slice(0, 6).map((p) => h("li", jump(p)))),
            problems.length > 6 ? h("p", `and ${problems.length - 6} more, marked in red below.`) : null)
        : existing.length || !rows.length ? null : h("p.notice.ok", "The fare stages are consistent."),
      existing.length
        ? h("div.notice.warning", h("p", h("strong", `${plural(existing.length, "problem")} already in the live route`)),
            h("p", "These do not stop you submitting, but please fix them if you can."),
            h("ul", existing.slice(0, 6).map((p) => h("li", jump(p)))),
            existing.length > 6 ? h("p", `and ${existing.length - 6} more, marked in amber below.`) : null)
        : null,
      problemList(serverProblems));
    if (activePicker) activePicker.close();
    activePicker = null;
    const items = [];
    if (!rows.length) items.push(h("li.empty-route", h("p", "This route has no stops yet. Add the first stop; it starts fare stage 1.")));
    items.push(slot(0));
    rows.forEach((r, i) => {
      items.push(rowEditor(r, i, byRow.get(i) || []));
      if (i < rows.length - 1) items.push(slot(i + 1));
    });
    if (rows.length) items.push(slot(rows.length));
    clear(list, items);
    map.showRoute({ ...route, rows }, { fit: refit && rows.some((r) => r.lat != null) });
    if (activePicker) activePicker.focus();
    else if (focus) document.getElementById(focus)?.focus();
  };

  const stageBefore = (i) => {
    for (let k = i; k >= 0; k--) if (rows[k].stop_type === "NEW STOP") return rows[k];
    return null;
  };

  // A stop near where the new or changed row goes, to rank suggestions by.
  const nearFor = (at, skip = -1) => {
    for (let k = at - 1; k >= 0; k--) if (k !== skip && rows[k].lat != null) return { lat: rows[k].lat, lon: rows[k].lon, label: `stop ${k + 1}` };
    for (let k = at; k < rows.length; k++) if (k !== skip && rows[k].lat != null) return { lat: rows[k].lat, lon: rows[k].lon, label: `stop ${k + 1}` };
    return null;
  };

  function slotLabel(at) {
    if (!rows.length) return ["Add the first stop", "Add the first stop"];
    if (at === 0) return ["Add a stop at the start", "Add a stop before stop 1"];
    if (at === rows.length) return ["Add a stop at the end", `Add a stop after stop ${rows.length}`];
    return ["Add stop here", `Add a stop between stop ${at} and stop ${at + 1}`];
  }

  function slot(at) {
    const [visible, spoken] = slotLabel(at);
    if (active && active.kind === "insert" && active.at === at) {
      const near = nearFor(at);
      // the stops either side are never offered: a stop twice in a row is not allowed
      const neighbours = [rows[at - 1], rows[at]].filter((r) => r && r.stop_id).map((r) => r.stop_id);
      activePicker = stopPicker({
        title: spoken,
        near,
        exclude: neighbours,
        excludeReason: "it is already next to this place in the route, and a bus cannot stop at one stop twice in a row.",
        mapMessage: `Click the stop to add ${at === rows.length ? "at the end" : at === 0 ? "at the start" : `after stop ${at}`}.`,
        onPick: (s) => insertAt(at, s),
        onCancel: () => { active = null; redraw({ focus: `add-${at}` }); },
      });
      return h("li.add-slot.open", activePicker.el);
    }
    const edge = at === 0 || at === rows.length;
    return h("li.add-slot", { class: edge ? "edge" : "" },
      h("button.add-stop", { type: "button", id: `add-${at}`, "aria-label": spoken, on: { click: () => { active = { kind: "insert", at }; redraw(); } } },
        h("span.plus", { "aria-hidden": "true" }, "+"), visible));
  }

  function insertAt(at, stop) {
    const servedBefore = rows.slice(0, at).some((r) => !SERVED_EXCLUDE.has(r.stop_type));
    const st = stageBefore(at - 1);
    const next = rows[at];
    const row = {
      stop_id: stop.stop_id, stop_name: stop.name, lat: stop.lat, lon: stop.lon, parent_station: stop.parent_station || null,
      stop_type: servedBefore ? "INTERMEDIATE STOP" : "NEW STOP",
      stage_no: servedBefore && st ? st.stage_no : next ? next.stage_no ?? 1 : 1,
      stage_name: servedBefore && st ? st.stage_name : stop.name,
      marker_id: null, marker_name: null, marker_lat: null, marker_lon: null,
      stop_name_override: null, provider_id: null, draft: !!stop.draft,
    };
    rows.splice(at, 0, row);
    active = null;
    edited(`added ${stop.name} as stop ${at + 1}`);
    redraw({ focus: `row-${at}` });
    toast(`Added ${stop.name} as stop ${at + 1}${row.stop_type === "NEW STOP" ? ", starting a fare stage" : ""}.`);
  }

  function changeStop(i, stop) {
    const r = rows[i];
    const dropped = r.stop_name_override;
    Object.assign(r, {
      stop_id: stop.stop_id, stop_name: stop.name, lat: stop.lat, lon: stop.lon, parent_station: stop.parent_station || null,
      stop_name_override: null, draft: !!stop.draft,
    });
    active = null;
    edited(`changed stop ${i + 1} to ${stop.name}`);
    redraw({ focus: `row-${i}` });
    toast(`Stop ${i + 1} is now ${stop.name} (${stop.stop_id}).${dropped ? ` The route's own spelling "${dropped}" was for the old stop and is removed.` : ""}`);
  }

  // Changing a stage stop's number or name carries along the rows of its stage
  // that matched it, so the fare rule keeps holding.
  function setStage(i, patch) {
    const r = rows[i];
    const old = { stage_no: r.stage_no, stage_name: r.stage_name };
    Object.assign(r, patch);
    if (r.stop_type === "NEW STOP") {
      for (let k = i + 1; k < rows.length && rows[k].stop_type !== "NEW STOP"; k++) {
        if (String(rows[k].stage_no) === String(old.stage_no) && (rows[k].stage_name || "") === (old.stage_name || "")) {
          rows[k].stage_no = r.stage_no;
          rows[k].stage_name = r.stage_name;
        }
      }
    }
    edited(`changed the fare stage at stop ${i + 1}`);
    redraw();
  }

  function stageNameControl(r, i) {
    const own = (r.stop_name || "").trim();
    const current = r.stage_name || "";
    const typed = typedName.has(r);
    const others = stageNames().filter((n) => n !== own);
    const select = h("select.stage-name-select", {
      id: `stage-name-${i}`, "aria-label": `Stage name at stop ${i + 1}`,
      on: {
        change: (ev) => {
          const v = ev.target.value;
          if (v === "other") {
            typedName.add(r);
            redraw({ focus: `stage-other-${i}` });
            return;
          }
          typedName.delete(r);
          setStage(i, { stage_name: v.slice(5) });
        },
      },
    },
    own ? h("optgroup", { label: "This stop's name" }, h("option", { value: `name:${own}`, selected: !typed && current === own }, own)) : null,
    others.length ? h("optgroup", { label: "Stage names on this route" }, others.map((n) => h("option", { value: `name:${n}`, selected: !typed && current === n }, n))) : null,
    current ? null : h("option", { value: "name:", selected: !typed }, "(no stage name yet)"),
    h("option", { value: "other", selected: typed }, "Other name…"));
    const other = typed
      ? h("input.stage-name-input", {
          type: "text", id: `stage-other-${i}`, value: current, "aria-label": `Other stage name at stop ${i + 1}`,
          placeholder: "Type the stage name", on: { change: (ev) => setStage(i, { stage_name: ev.target.value.trim() }) },
        })
      : null;
    return [select, other];
  }

  function rowEditor(r, i, problems) {
    const isMarker = r.stop_type === "ROUTE CORRECTION";
    const kind = { "NEW STOP": "new", "JUMP STOP": "jump", "ROUTE CORRECTION": "marker" }[r.stop_type] || "";
    let detail;
    if (isMarker) {
      detail = h("span.meta", "Bends the map line. Not a stop passengers can use.");
    } else {
      const type = h("select", { "aria-label": `Type of stop ${i + 1}`, on: { change: (ev) => {
        const t = ev.target.value;
        if (t === "INTERMEDIATE STOP") {
          const st = stageBefore(i - 1);
          Object.assign(rows[i], { stop_type: t }, st ? { stage_no: st.stage_no, stage_name: st.stage_name } : {});
        } else {
          Object.assign(rows[i], { stop_type: t });
        }
        edited(`changed the type of stop ${i + 1}`);
        redraw({ focus: null });
      } } }, ["NEW STOP", "INTERMEDIATE STOP", "JUMP STOP"].map((t) => h("option", { value: t, selected: t === r.stop_type }, STOP_TYPE_LABEL[t])),
      r.stop_type === "HIDDEN STOP" ? h("option", { value: "HIDDEN STOP", selected: true }, STOP_TYPE_LABEL["HIDDEN STOP"]) : null);
      const line = [type];
      if (r.stop_type === "NEW STOP") {
        line.push(
          h("label.inline", { for: `stage-no-${i}` }, "stage"),
          h("input.stage-no-input", { type: "number", min: "0", id: `stage-no-${i}`, value: String(r.stage_no ?? ""), "aria-label": `Stage number at stop ${i + 1}`,
            on: { change: (ev) => setStage(i, { stage_no: ev.target.value === "" ? null : Number(ev.target.value) }) } }),
          ...stageNameControl(r, i));
      } else {
        line.push(h("span.meta", `stage ${r.stage_no ?? "?"}, ${r.stage_name || "no name"}`));
      }
      detail = h("div.row-edit-line", line);
    }
    const errs = problems.map((p) => h(p.existing ? "span.row-warning" : "span.row-error", p.message, p.fix
      ? [" ", h("button.btn.quiet.small", { type: "button", on: { click: () => { Object.assign(rows[i], p.fix); edited(`set stop ${i + 1} to stage ${p.fix.stage_no}`); redraw(); } } }, `Use stage ${p.fix.stage_no}`)]
      : null));
    const rowState = problems.some((p) => !p.existing) ? "has-error" : problems.length ? "has-warning" : "";
    const changing = active && active.kind === "change" && active.index === i;
    let pickerEl = null;
    if (changing) {
      activePicker = stopPicker({
        title: `Change stop ${i + 1} (${r.stop_name || r.stop_id}) to`,
        near: r.lat != null ? { lat: r.lat, lon: r.lon, label: "the current stop" } : nearFor(i, i),
        exclude: [rows[i - 1], r, rows[i + 1]].filter((x) => x && x.stop_id).map((x) => x.stop_id),
        excludeReason: "it is this stop or the one next to it, and a bus cannot stop at one stop twice in a row.",
        mapMessage: `Click the stop to use as stop ${i + 1}.`,
        onPick: (s) => changeStop(i, s),
        onCancel: () => { active = null; redraw({ focus: `change-${i}` }); },
      });
      pickerEl = activePicker.el;
    }
    const meta = [r.stop_id, r.parent_station ? `in station ${r.parent_station}` : null, r.draft ? "new in your draft" : null,
      r.stop_name_override ? "this route's own spelling" : null].filter(Boolean).join(" · ");
    return h("li.row", { class: `${kind} ${rowState}`, id: `row-${i}`, tabindex: "-1" },
      h("span.node", { "aria-hidden": "true" }),
      h("div.row-edit",
        h("div.row-head",
          h("span.seq", String(i + 1)),
          h("span.name", isMarker ? `Map shaping point: ${r.marker_name || r.marker_id}` : `${r.stop_name || r.stop_id}`),
          tools(i)),
        isMarker ? null : h("div.row-sub", h("span.meta", meta),
          h("button.btn.quiet.small", { type: "button", id: `change-${i}`, "aria-expanded": String(changing),
            on: { click: () => { active = changing ? null : { kind: "change", index: i }; redraw({ focus: `change-${i}` }); } } }, "Change stop")),
        detail, errs, pickerEl));
  }

  function move(i, delta) {
    const j = i + delta;
    if (j < 0 || j >= rows.length) return;
    [rows[i], rows[j]] = [rows[j], rows[i]];
    active = null;
    edited(`moved stop ${i + 1} ${delta < 0 ? "up" : "down"}`);
    redraw({ focus: `row-${j}` });
  }

  function tools(i) {
    return h("div.row-tools",
      h("button.icon-btn", { type: "button", title: "Move up", "aria-label": `Move stop ${i + 1} up`, disabled: i === 0, on: { click: () => move(i, -1) } }, "↑"),
      h("button.icon-btn", { type: "button", title: "Move down", "aria-label": `Move stop ${i + 1} down`, disabled: i === rows.length - 1, on: { click: () => move(i, 1) } }, "↓"),
      h("button.icon-btn", { type: "button", title: "Remove from the route", "aria-label": `Remove stop ${i + 1}`, on: { click: () => {
        const [gone] = rows.splice(i, 1);
        active = null;
        edited(`removed ${gone.stop_name || gone.marker_name || "a row"}`);
        redraw({ focus: rows.length ? `row-${Math.min(i, rows.length - 1)}` : "add-0" });
        toast(`Removed ${gone.stop_name || gone.marker_name || "the row"} from the route.`);
      } } }, "×"));
  }

  fixAllBtn.addEventListener("click", () => {
    validateRows(rows).filter((p) => p.fix).forEach((p) => Object.assign(rows[p.index], p.fix));
    edited("set every intermediate stop to its stage");
    redraw();
  });

  async function renumber() {
    const firstStage = rows.find((r) => r.stop_type === "NEW STOP");
    if (!firstStage) { toast("There are no stage stops to number yet.", "error"); return; }
    const stages = rows.filter((r) => r.stop_type === "NEW STOP").length;
    const from = await modal("Renumber stages in order", (close) => {
      const startIn = h("input", { type: "number", id: "renumber-start", min: "0", value: String(Number.parseInt(firstStage.stage_no, 10) || 1) });
      const preview = h("p", { "aria-live": "polite" });
      const show = () => {
        const s = Number.parseInt(startIn.value, 10);
        if (Number.isNaN(s)) { preview.textContent = "Enter the number of the first stage."; return; }
        const { changed } = renumberStages(rows, s);
        preview.textContent = changed
          ? `The ${plural(stages, "stage")} will be numbered ${s} to ${s + stages - 1}. ${plural(changed, "stop")} will change.`
          : "The stages are already numbered in order from that number. Nothing will change.";
      };
      startIn.addEventListener("input", show);
      show();
      return h("form", { style: "display:grid;gap:12px", on: { submit: (ev) => { ev.preventDefault(); const s = Number.parseInt(startIn.value, 10); if (!Number.isNaN(s)) close(s); } } },
        h("p", "Stage stops are numbered one after another in route order. Every other stop then takes the number and name of the stage it is in."),
        h("p.notice.warning", "Fares depend on how many stages apart two stops are. If this route skips a stage number on purpose, check the fare chart before renumbering."),
        h("label.field", { for: "renumber-start" }, h("span", "Number of the first stage"), startIn),
        preview,
        h("div.btn-row", h("button.btn", { type: "submit" }, "Renumber stages"), h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
    });
    if (from === undefined) return;
    const { rows: next, changed } = renumberStages(rows, from);
    if (!changed) { toast("The stages were already numbered in order."); return; }
    rows.forEach((r, i) => Object.assign(r, { stage_no: next[i].stage_no, stage_name: next[i].stage_name }));
    edited("renumbered the stages");
    redraw();
    toast(`Stages renumbered: ${plural(changed, "stop")} changed.`);
  }

  const cancel = () => {
    unsaved.done();
    map.endModes();
    showRoute(route.route_id, { preview: created });
  };
  const save = async () => {
    const marker = (r) => r.stop_type === "ROUTE CORRECTION";
    const payload = rows.map((r) => ({
      stop_id: marker(r) ? null : r.stop_id, stop_type: r.stop_type,
      stage_no: r.stage_no, stage_name: r.stage_name,
      marker_id: marker(r) ? r.marker_id || null : null, marker_name: marker(r) ? r.marker_name || null : null,
      marker_lat: marker(r) ? r.marker_lat ?? null : null, marker_lon: marker(r) ? r.marker_lon ?? null : null,
      stop_name_override: marker(r) ? null : r.stop_name_override ?? null,
      provider_id: r.provider_id ?? null,
    }));
    try {
      const res = await addChange({ entity: "route_stops", op: "replace", entity_key: route.route_id, after: { rows: payload, base_rows_hash: baseHash } });
      if (!res) return;
      unsaved.done();
      history.clear();
      serverProblems = res.problems;
      if (res.problems.some((p) => p.level === "error")) {
        redraw();
        summary.scrollIntoView({ block: "nearest" });
        return;
      }
      map.endModes();
      if (created) location.hash = `#/route/${enc(route.route_id)}?draft=1`;
      else showRoute(route.route_id, { preview: true });
    } catch (e) {
      toast(e.message, "error");
    }
  };

  clear(panel(),
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the route"),
      h("h1", created ? `Stops of the new route ${label}` : `Edit stops of ${label}`),
      h("p", route.long_name || ""),
      created ? h("p.notice.draft", "This route is new in your draft. Add its stops in order: the first stop starts fare stage 1.") : null,
      h("p.hint", "A stage stop starts a fare stage. Every intermediate stop after it carries the same stage number and name, or passengers are charged the wrong fare. Changing a stage stop's number or name updates the stops in its stage."),
      summary,
      h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: renumber } }, "Renumber stages in order"), fixAllBtn),
      history.buttons()),
    h("section.section", h("div.ladder.editing", list)),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "button", on: { click: save } }, prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  );
  redraw({ refit: true });
}

// ------------------------------------------------------------------ route details
export async function editRouteDetails(route, { created = false } = {}) {
  if (!(await requireDraft())) return;
  const createChange = created ? createdChange("route", route.route_id) : null;
  const prior = existingChange("route", route.route_id);
  const base = createChange ? createChange.after : route;
  const start = { short_name: base.short_name || "", long_name: base.long_name || "", color: base.color || "", ...(prior && !createChange ? prior.after : {}) };
  let proposed = prior && prior.after.encoded_polyline ? { encoded_polyline: prior.after.encoded_polyline, polyline_source: prior.after.polyline_source } : null;
  const unsaved = guard(`Your changes to route ${route.short_name || route.route_id} are not in the draft yet.`);
  const f = {
    short_name: h("input", { type: "text", id: "route-short", value: start.short_name }),
    long_name: h("input", { type: "text", id: "route-long", value: start.long_name }),
    color: h("input", { type: "text", id: "route-color", value: start.color || "", placeholder: "#0B6660", maxlength: "7" }),
  };
  const swatch = h("input", { type: "color", value: /^#[0-9a-f]{6}$/i.test(start.color) ? start.color : "#14252a", "aria-label": "Pick a colour" });
  swatch.addEventListener("input", () => { f.color.value = swatch.value.toUpperCase(); unsaved.touch(); });
  f.color.addEventListener("input", () => { if (/^#[0-9a-f]{6}$/i.test(f.color.value)) swatch.value = f.color.value; });
  const lineStatus = h("div");
  const problems = h("div");
  const history = undoScope("this route");
  history.fields([[f.short_name, "the route number"], [f.long_name, "the route name"], [f.color, "the colour"]]);
  // a colour picked from the swatch is one step, like a typed one
  swatch.addEventListener("change", () => f.color.dispatchEvent(new Event("change", { bubbles: true })));
  const setLine = (line, label) => {
    const before = proposed;
    proposed = line;
    history.push({ label, undo: () => { proposed = before; unsaved.touch(); showProposal(); }, redo: () => { proposed = line; unsaved.touch(); showProposal(); } });
    showProposal();
  };

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
      h("button.btn.quiet.small", { type: "button", on: { click: () => setLine(null, "discarded the new map line") } }, "Discard the new map line"));
  };

  const suggest = async () => {
    clear(lineStatus, h("p.hint", "Asking the road router for a line through the stops…"));
    try {
      const res = await post(`feeds/${enc(state.feedId)}/routes/${enc(route.route_id)}/polyline:osrm?change_set=${enc(state.draft.change_set_id)}`);
      unsaved.touch();
      setLine({ encoded_polyline: res.encoded_polyline, polyline_source: res.polyline_source || "osrm" }, "suggested a map line");
    } catch (e) {
      clear(lineStatus, h("p.notice.error", e.message));
    }
  };

  const cancel = () => { unsaved.done(); map.clearRoute("proposal"); showRoute(route.route_id, { preview: created }); };
  const save = async (ev) => {
    ev.preventDefault();
    const color = f.color.value.trim().toUpperCase();
    if (color && !/^#[0-9A-F]{6}$/.test(color)) {
      clear(problems, h("p.notice.error", "Colour must be # followed by six hex digits, for example #0B6660."));
      f.color.setAttribute("aria-invalid", "true");
      return;
    }
    try {
      let res = null;
      if (createChange) {
        // a route new in this draft: its name and colour are part of the create
        const short = f.short_name.value.trim();
        if (!short) { clear(problems, h("p.notice.error", "A route needs a route number.")); return; }
        const after = { ...createChange.after, short_name: short };
        const long = f.long_name.value.trim();
        if (long) after.long_name = long; else delete after.long_name;
        if (color) after.color = color; else delete after.color;
        res = await updateChange(createChange, after);
        if (proposed) res = await addChange({ entity: "route", op: "update", entity_key: route.route_id, after: proposed });
      } else {
        const after = {};
        for (const k of ["short_name", "long_name"]) {
          const v = f[k].value.trim();
          if (v !== (route[k] || "")) after[k] = v;
        }
        if ((color || null) !== (route.color ? route.color.toUpperCase() : null)) after.color = color || null;
        if (proposed) Object.assign(after, proposed);
        if (!Object.keys(after).length) { clear(problems, h("p.notice", "Nothing has changed yet.")); return; }
        res = await addChange({ entity: "route", op: "update", entity_key: route.route_id, after, base_row_version: route.row_version });
      }
      if (!res) return;
      unsaved.done();
      history.clear();
      if (res.problems.some((p) => p.level === "error")) { clear(problems, problemList(res.problems)); return; }
      map.clearRoute("proposal");
      showRoute(route.route_id, { preview: true });
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { novalidate: true, on: { submit: save, input: () => unsaved.touch() } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the route"),
      h("h1", `Edit route ${route.short_name || route.route_id}`),
      created ? h("p.notice.draft", "This route is new in your draft; its number, name and colour are updated there.") : null,
      h("label.field", { for: "route-short" }, h("span", "Route number"), f.short_name),
      h("label.field", { for: "route-long" }, h("span", "Route name"), f.long_name),
      h("label.field", { for: "route-color" }, h("span", "Colour on maps (optional)"), h("div.btn-row", swatch, h("div", { style: "flex:1" }, f.color))),
      history.buttons(),
    ),
    h("section.section",
      h("h2", "Map line"),
      h("p.hint", "The map line is the road path drawn between the stops. The road router can suggest one through this route's stops."),
      lineStatus,
      h("div.btn-row", h("button.btn.secondary", { type: "button", on: { click: suggest } }, "Suggest a map line")),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, prior || createChange ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  ));
  showProposal();
}

// ------------------------------------------------------------------ stations
// Create (station null, from some stops or from none) or edit a station: its
// name, id, point, member stops and each member's platform label.
export async function editStation(station, initialStops = []) {
  if (!(await requireDraft())) return;
  const creating = !station;
  const members = new Map();       // stop_id -> {stop_id, name, lat, lon, platform_code}
  if (station) station.children.forEach((c) => members.set(c.stop_id, { ...c }));
  initialStops.forEach((s) => members.set(s.stop_id, { ...s }));
  const prior = station ? existingChange("station", station.stop_id) : null;
  if (prior && prior.after && Array.isArray(prior.after.members)) {
    prior.after.members.forEach((m) => { if (members.has(m.stop_id)) members.get(m.stop_id).platform_code = m.platform_code; });
  }
  const firstId = () => [...members.keys()][0];
  let lat = station ? station.lat : null, lon = station ? station.lon : null;
  if (prior && prior.after && prior.after.lat != null) { lat = prior.after.lat; lon = prior.after.lon; }
  let idTyped = false, placedByHand = !creating;
  const unsaved = guard(creating ? "The new station is not in the draft yet." : `Your changes to station ${station.name} are not in the draft yet.`);
  const idInput = h("input", { type: "text", id: "station-id", value: creating ? (firstId() ? `stn_${firstId()}` : "") : station.stop_id, disabled: !creating, placeholder: "Filled in from the first stop" });
  idInput.addEventListener("input", () => { idTyped = true; });
  const first = [...members.values()][0];
  const nameInput = h("input", { type: "text", id: "station-name", value: (prior && prior.after && prior.after.name) || (station ? station.name : first ? first.name : "") });
  const latIn = h("input", { type: "number", step: "any", id: "station-lat", value: lat != null ? String(lat) : "" });
  const lonIn = h("input", { type: "number", step: "any", id: "station-lon", value: lon != null ? String(lon) : "" });
  const memberList = h("ul.list.member-list");
  const suggestions = h("div");
  const finder = h("div");
  const problems = h("div");

  // The stops in the station, their labels and the station point: each change to
  // them is one step to undo, the whole of it before and after. Name and id are
  // fields, a step each once left.
  const history = undoScope("the station");
  const syncFields = history.fields([[nameInput, "the station name"], [idInput, "the station id"], [latIn, "the latitude"], [lonIn, "the longitude"]]);
  const names = new Map();         // station names for the suggestions, looked up once
  const take = () => ({ members: [...members.values()].map((m) => ({ ...m })), lat: latIn.value, lon: lonIn.value, id: idInput.value, name: nameInput.value, placedByHand });
  let snap = null;
  const remember = (label) => {
    if (!snap) return;
    const before = snap, after = take();
    snap = after;
    syncFields();
    history.push({ label, undo: () => putBack(before), redo: () => putBack(after) });
  };
  function putBack(st) {
    members.clear();
    st.members.forEach((m) => members.set(m.stop_id, { ...m }));
    latIn.value = st.lat;
    lonIn.value = st.lon;
    idInput.value = st.id;
    nameInput.value = st.name;
    placedByHand = st.placedByHand;
    snap = st;
    syncFields();
    selector.set([...members.keys()]);
    if (st.lat !== "" && st.lon !== "") pin(Number(st.lat), Number(st.lon));
    unsaved.touch();
    suggestedFor = null;
    loadSuggestions();
    drawMembers();
  }

  let setPin = null;
  const pin = (la, lo) => {
    if (setPin) setPin(la, lo);
    else setPin = map.stationPin(la, lo, (a, b) => { latIn.value = a.toFixed(7); lonIn.value = b.toFixed(7); placedByHand = true; unsaved.touch(); },
      () => remember("moved the station point"));
  };
  const selector = map.selectStops([...members.keys()], (s, added) => {
    if (added) members.set(s.stop_id, { ...s }); else members.delete(s.stop_id);
    membersChanged(added ? `added ${s.name} to the station` : `took ${s.name} out of the station`);
  });
  if (lat != null) pin(lat, lon);
  [latIn, lonIn].forEach((el) => el.addEventListener("input", debounce(() => {
    const la = Number(latIn.value), lo = Number(lonIn.value);
    if (latIn.value !== "" && lonIn.value !== "" && Number.isFinite(la) && Number.isFinite(lo)) { placedByHand = true; pin(la, lo); }
  }, 300)));

  function centre() {
    const ms = [...members.values()].filter((m) => m.lat != null);
    if (!ms.length) return;
    const la = ms.reduce((a, m) => a + m.lat, 0) / ms.length, lo = ms.reduce((a, m) => a + m.lon, 0) / ms.length;
    latIn.value = la.toFixed(7); lonIn.value = lo.toFixed(7);
    pin(la, lo);
  }

  function membersChanged(label = "changed the station's stops") {
    unsaved.touch();
    if (creating && !idTyped) idInput.value = firstId() ? `stn_${firstId()}` : "";
    if (creating && !nameInput.value.trim() && members.size) nameInput.value = [...members.values()][0].name;
    if (creating && !placedByHand) centre();
    remember(label);
    loadSuggestions();
    drawMembers();
  }

  function drawMembers() {
    clear(memberList, members.size
      ? [...members.values()].map((m) => h("li.member-row",
          h("div.member-row-head",
            h("span", h("strong", m.name), h("span.meta", ` ${m.stop_id}`)),
            h("button.btn.quiet.small", { type: "button", "aria-label": `Take ${m.name} (${m.stop_id}) out of the station`,
              on: { click: () => { members.delete(m.stop_id); selector.set([...members.keys()]); membersChanged(`took ${m.name} out of the station`); } } }, "Take out")),
          h("label.field", { for: `platform-${m.stop_id}` }, h("span", "Platform label (optional)"),
            h("input", { type: "text", id: `platform-${m.stop_id}`, maxlength: "120", value: m.platform_code || "", placeholder: "For example Towards Guindy",
              on: { input: (ev) => { m.platform_code = ev.target.value; unsaved.touch(); }, change: () => remember(`changed the platform label of ${m.name}`) } }))))
      : h("p.empty", "No stops yet. Click the stops of this place on the map (for example both sides of the road)."));
  }

  // adding a stop without the map: search by name or id
  function openFinder() {
    const any = [...members.values()][0];
    const picker = stopPicker({
      title: "Find a stop to add to the station",
      near: any ? { lat: any.lat, lon: any.lon, label: any.name } : null,
      exclude: [...members.keys()],
      excludeReason: "it is already in the station.",
      includeDraft: false,
      mapMessage: "Click the stop to add to the station.",
      onPick: (s) => {
        members.set(s.stop_id, { ...s });
        selector.set([...members.keys()]);
        clear(finder, finderButton());
        membersChanged(`added ${s.name} to the station`);
        document.getElementById(`platform-${s.stop_id}`)?.focus();
      },
      onCancel: () => { clear(finder, finderButton()); finder.querySelector("button")?.focus(); },
    });
    clear(finder, picker.el);
    picker.focus();
  }
  const finderButton = () => h("button.btn.secondary.small", { type: "button", on: { click: openFinder } }, "Find a stop to add");

  let suggestedFor = null;
  function loadSuggestions() {
    const id = firstId();
    if (!id || id === suggestedFor) { if (!id) clear(suggestions); return; }
    suggestedFor = id;
    get(`feeds/${enc(state.feedId)}/stops/${enc(id)}`).then(async (d) => {
      const open = d.nearby.filter((n) => n.location_type === 0 && !members.has(n.stop_id));
      // a platform of another station cannot join this one: say whose it is, once
      const taken = open.filter((n) => n.parent_station && (!station || n.parent_station !== station.stop_id));
      const near = open.filter((n) => !taken.includes(n));
      await stationNames(taken, names);
      if (suggestedFor !== id) return null;
      const note = platformsNote(foldPlatforms(taken, names).stations, open.length);
      if (!near.length) return clear(suggestions, note);
      return clear(suggestions, h("p.hint", "Stops within 60 m you may want in this station:"),
        h("ul.list", near.map((n) => h("li.list-item",
          h("span.key", fmtMetres(n.distance_m)), h("span", n.name),
          h("button.btn.quiet.small", { type: "button", on: { click: (ev) => { members.set(n.stop_id, { ...n }); selector.set([...members.keys()]); membersChanged(`added ${n.name} to the station`); ev.target.closest("li")?.remove(); } } }, "Add"),
          h("span.sub", n.stop_id)))),
        note);
    }).catch(() => {});
  }

  const leave = () => {
    unsaved.done();
    map.endModes();
    if (station) showStop(station.stop_id);
    else if (initialStops[0]) showStop(initialStops[0].stop_id);
    else location.hash = "#/";
  };
  const save = async (ev) => {
    ev.preventDefault();
    const name = nameInput.value.trim();
    const la = Number(latIn.value), lo = Number(lonIn.value);
    const errs = [];
    if (creating && !ID_RE.test(idInput.value.trim())) errs.push(`The station id is not valid. ${ID_RULE}`);
    if (!name) errs.push("Give the station a name.");
    if (members.size < 2) errs.push("A station groups at least two stops. Click its stops on the map.");
    if (latIn.value === "" || !Number.isFinite(la) || !Number.isFinite(lo)) errs.push("Place the station point.");
    if (errs.length) { clear(problems, h("div.notice.error", { role: "alert" }, h("ul", errs.map((e) => h("li", e))))); return; }
    const after = {
      name, lat: round7(la), lon: round7(lo),
      members: [...members.values()].map((m) => ({ stop_id: m.stop_id, platform_code: (m.platform_code || "").trim() || null })),
    };
    if (creating) after.station_id = idInput.value.trim();
    try {
      const res = await addChange({
        entity: "station", op: creating ? "create" : "update",
        entity_key: creating ? after.station_id : station.stop_id, after,
        base_row_version: station ? station.row_version : undefined,
      }, { merge: !creating });
      if (!res) return;
      unsaved.done();
      history.clear();
      if (res.problems.some((p) => p.level === "error")) { clear(problems, problemList(res.problems)); return; }
      map.endModes();
      if (station) showStop(station.stop_id);
      else location.hash = `#/stop/${enc(firstId())}`;
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { novalidate: true, on: { submit: save, input: () => unsaved.touch() } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: leave } }, "Back"),
      h("h1", creating ? (members.size ? "Club stops into a station" : "New station") : `Edit station ${station.name}`),
      h("p.hint", "A station groups the stops of one place, such as the two sides of a road or the bays of a bus terminus, so passengers can search for the place once."),
      h("label.field", { for: "station-name" }, h("span", "Station name"), nameInput),
      h("label.field", { for: "station-id" }, h("span", "Station id"), idInput),
    ),
    h("section.section",
      h("h2", "Stops in the station"),
      h("p.hint", "Click stops on the map to add or take them out, or find one by name. A platform label tells passengers which way the buses at that stop go."),
      memberList,
      finder,
      suggestions,
      history.buttons()),
    h("section.section",
      h("h2", "Station point"),
      h("div.field-row",
        h("label.field", { for: "station-lat" }, h("span", "Latitude"), latIn),
        h("label.field", { for: "station-lon" }, h("span", "Longitude"), lonIn)),
      h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: () => { placedByHand = false; centre(); unsaved.touch(); remember("placed the station point in the middle"); } } }, "Place in the middle of its stops")),
      h("p.hint", "Or drag the dark square on the map."),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: leave } }, "Cancel"))),
  ));
  drawMembers();
  clear(finder, finderButton());
  loadSuggestions();
  if (creating && lat == null) centre();
  snap = take();
  syncFields();
  nameInput.focus();
}

export async function dissolveStation(station) {
  if (!(await requireDraft())) return;
  const ok = await confirmDialog("Dissolve this station?",
    `${station.name} groups ${plural(station.children.length, "stop")}. Dissolving it keeps every stop and removes only the grouping, once the draft is committed.`,
    { confirm: "Add to draft", danger: true });
  if (!ok) return;
  try {
    await addChange({ entity: "station", op: "delete", entity_key: station.stop_id, after: null, base_row_version: station.row_version }, { merge: false });
    showStop(station.stop_id);
  } catch (e) {
    toast(e.message, "error");
  }
}
