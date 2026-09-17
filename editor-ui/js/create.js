// Creating things from the New menu: a stop placed on the map, and a route
// whose stop list is then built with the stop pickers. Also the panel for a stop
// that so far exists only in the draft. (New station is the station editor.)
import { get, enc, ApiError } from "./api.js";
import { state, setLeaveGuard } from "./state.js";
import { h, clear, toast, confirmDialog, debounce, fmtCoord, fmtMetres, haversine, ID_RE, ID_RULE, PLATFORM_PLACEHOLDER, PLATFORM_HELP, descriptionField } from "./util.js";
import * as map from "./map.js";
import { addChange, requireDraft, createdChange, createdStops, updateChange, removeChange } from "./drafts.js";
import { editRouteRows, problemList } from "./editors.js";
import { pendingNotice, pendingActions, draftedPoints } from "./overlay.js";
import { nameHere } from "./trail.js";
import { undoScope } from "./undo.js";

const panel = () => document.getElementById("panel");
const round7 = (x) => Math.round(x * 1e7) / 1e7;
const DRAFT_REASON = "New stops and routes are added to a draft. Nothing changes for passengers until someone else approves the draft and it is committed.";

async function emptyListHash() {
  const bytes = await crypto.subtle.digest("SHA-256", new TextEncoder().encode("[]"));
  return [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

// ------------------------------------------------------------------ new stop
// `change`: an existing stop/create change in the draft, to edit instead.
export async function newStop(change = null) {
  if (!(await requireDraft(DRAFT_REASON))) {
    location.hash = "#/";
    return;
  }
  map.clearRoute();
  map.clearFocus();
  let editing = change;
  const start = editing ? editing.after : {};
  let lat = start.lat ?? null, lon = start.lon ?? null;
  let dirty = false;
  setLeaveGuard(() => (!dirty ? null : editing ? "Your changes to the new stop are not in the draft yet." : "The new stop is not in the draft yet."));
  const touch = () => { dirty = true; };

  const description = descriptionField("new-stop-description", start.description);
  const f = {
    name: h("input", { type: "text", id: "new-stop-name", value: start.name || "", autocomplete: "off" }),
    platform: h("input", { type: "text", id: "new-stop-platform", value: start.platform_code || "", maxlength: "120", placeholder: PLATFORM_PLACEHOLDER }),
    description: description.input,
    id: h("input", { type: "text", id: "new-stop-id", value: editing ? editing.entity_key : "", disabled: !!editing, autocomplete: "off", spellcheck: "false", placeholder: "Leave empty to have one made" }),
    lat: h("input", { type: "number", id: "new-stop-lat", step: "any", value: lat != null ? String(lat) : "" }),
    lon: h("input", { type: "number", id: "new-stop-lon", step: "any", value: lon != null ? String(lon) : "" }),
  };
  const where = h("div", { "aria-live": "polite" });
  const problems = h("div");
  // placing and dragging the pin, and each field once left, can be undone until
  // the stop is in the draft
  const history = undoScope("the new stop");
  const syncFields = history.fields([[f.name, "the name"], [f.platform, "the platform label"], [f.description, "the description"], [f.id, "the stop id"], [f.lat, "the latitude"], [f.lon, "the longitude"]]);
  let placed = lat != null ? { lat, lon } : null;   // where the pin was last put down

  let nearSeq = 0;
  const checkNearby = debounce(async () => {
    if (lat == null) return;
    const mine = ++nearSeq;
    const d = 0.0005;
    let live = [];
    try {
      live = (await get(`feeds/${enc(state.feedId)}/stops?bbox=${[lat - d, lon - d, lat + d, lon + d].map((x) => x.toFixed(6)).join(",")}&limit=50`)).items;
    } catch { /* the check is a courtesy */ }
    if (mine !== nearSeq) return;
    const near = [...createdStops(), ...live]
      .filter((s) => s.location_type !== 1 && (!editing || s.stop_id !== editing.entity_key))
      .map((s) => ({ s, d: haversine(lat, lon, s.lat, s.lon) }))
      .filter((x) => x.d <= 40)
      .sort((a, b) => a.d - b.d)
      .slice(0, 4);
    showWhere(near);
  }, 300);

  function showWhere(near = []) {
    if (lat == null) {
      clear(where, h("p.notice", h("strong", "Not placed yet. "), "Click the map where the bus stops (the kerb)."));
      return;
    }
    clear(where,
      h("p.hint", "Drag the amber pin on the map to adjust, or type the coordinates."),
      near.length ? h("div.notice.warning",
        h("p", h("strong", "Is this a stop that already exists?")),
        h("ul", near.map(({ s, d }) => h("li", `${s.name} (${s.stop_id}) is ${fmtMetres(d)} away${s.draft ? ", new in your draft" : ""}.`))),
        h("p", "If it is the same kerb, edit that stop instead of adding a new one.")) : null);
  }

  const setPin = map.placePoint((la, lo) => {
    lat = la;
    lon = lo;
    f.lat.value = la.toFixed(7);
    f.lon.value = lo.toFixed(7);
    touch();
    showWhere();
    checkNearby();
  }, {
    at: lat != null ? { lat, lon } : null,
    // one step per click or finished drag
    onDone: (la, lo) => {
      const before = placed, after = { lat: la, lon: lo };
      placed = after;
      syncFields();
      history.push({ label: before ? "moved the pin" : "placed the pin", undo: () => putPin(before), redo: () => putPin(after) });
    },
  });
  function putPin(p) {
    placed = p;
    lat = p ? p.lat : null;
    lon = p ? p.lon : null;
    f.lat.value = p ? p.lat.toFixed(7) : "";
    f.lon.value = p ? p.lon.toFixed(7) : "";
    syncFields();
    setPin(p ? p.lat : null, p ? p.lon : null);
    showWhere();
    if (p) checkNearby();
  }
  const typed = debounce(() => {
    const la = Number(f.lat.value), lo = Number(f.lon.value);
    if (f.lat.value === "" || f.lon.value === "" || !Number.isFinite(la) || !Number.isFinite(lo)) return;
    lat = la;
    lon = lo;
    placed = { lat: la, lon: lo };
    setPin(la, lo);
    showWhere();
    checkNearby();
  }, 400);
  f.lat.addEventListener("input", typed);
  f.lon.addEventListener("input", typed);

  const cancel = () => {
    dirty = false;
    setLeaveGuard(null);
    map.endModes();
    if (editing) location.hash = `#/stop/${enc(editing.entity_key)}`;
    else location.hash = "#/";
  };

  const save = async (ev) => {
    ev.preventDefault();
    const errs = [];
    const name = f.name.value.trim();
    const id = f.id.value.trim();
    if (lat == null) errs.push("Place the stop on the map first.");
    if (!name) errs.push("Give the stop the name passengers see.");
    if (!editing && id && !ID_RE.test(id)) errs.push(`The stop id is not valid. ${ID_RULE}`);
    if (!editing && id && !errs.length) {
      if (createdChange("stop", id)) errs.push(`Your draft already creates a stop with id ${id}.`);
      else {
        try {
          const taken = await get(`feeds/${enc(state.feedId)}/stops/${enc(id)}`);
          errs.push(`Stop id ${id} is already used by ${taken.name}. Choose another id, or leave it empty.`);
        } catch (e) {
          if (!(e instanceof ApiError && e.status === 404)) errs.push(e.message);
        }
      }
    }
    if (errs.length) {
      clear(problems, h("div.notice.error", { role: "alert" }, h("ul", errs.map((x) => h("li", x)))));
      return;
    }
    const after = { name, lat: round7(lat), lon: round7(lon) };
    const platform = f.platform.value.trim();
    if (platform) after.platform_code = platform;
    const described = f.description.value.trim();
    if (described) after.description = described;
    try {
      let res;
      if (editing) {
        res = await updateChange(editing, { ...editing.after, ...after, platform_code: platform || null, description: described || null, stop_id: editing.entity_key });
      } else {
        if (id) after.stop_id = id;
        res = await addChange({ entity: "stop", op: "create", entity_key: id, after }, { merge: false });
      }
      if (!res) return;
      dirty = false;
      history.clear();
      const ch = res.draft.changes.find((c) => c.change_id === res.changeId);
      if (res.problems.some((p) => p.level === "error")) {
        // it is in the draft now; further saves update that change
        editing = ch;
        f.id.value = ch.entity_key;
        f.id.disabled = true;
        clear(problems, problemList(res.problems));
        return;
      }
      setLeaveGuard(null);
      map.endModes();
      added(ch, !!change);
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { novalidate: true, on: { submit: save, input: touch } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, editing ? "Back to the stop" : "Back to the map"),
      h("h1", editing ? `Edit new stop ${start.name}` : "New stop"),
      h("p.hint", `It goes into draft "${state.draft.title}". Nothing changes for passengers until someone else approves the draft and it is committed.`),
      h("h2", "1. Place it"),
      where,
      history.buttons(),
      h("div.field-row",
        h("label.field", { for: "new-stop-lat" }, h("span", "Latitude"), f.lat),
        h("label.field", { for: "new-stop-lon" }, h("span", "Longitude"), f.lon)),
      h("h2", "2. Name it"),
      h("label.field", { for: "new-stop-name" }, h("span", "Name passengers see"), f.name),
      h("label.field", { for: "new-stop-platform" }, h("span", "Platform label (optional)"), f.platform,
        h("span.hint", PLATFORM_HELP)),
      description.el,
      h("label.field", { for: "new-stop-id" }, h("span", "Stop id (optional)"), f.id,
        h("span.hint", editing ? "The id cannot change once the stop is in the draft." : `Leave empty and the editor makes one, like ed_1a2b3c4d5e. ${ID_RULE}`)),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, editing ? "Update in draft" : "Add stop to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  ));
  showWhere();
  if (lat != null) checkNearby();
  f.name.focus();
}

function added(ch, updated) {
  const a = ch.after;
  clear(panel(),
    h("section.section",
      h("p.notice.ok", { role: "status" }, h("strong", updated ? "New stop updated in your draft" : "New stop added to your draft")),
      h("div.title-block",
        h("h1", a.name),
        h("p.ids", `Stop id ${ch.entity_key}${!a.platform_code ? "" : `, ${a.platform_code}`}`),
        a.description ? h("p.stop-description", a.description) : null),
      h("dl.facts", h("dt", "Position"), h("dd", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)),
      h("p.hint", "The stop is created when the draft is committed. For buses to call at it, add it to a route: open the route, choose Edit stop list, then Add stop. Stops new in your draft are offered there, and shown in amber on the map."),
      h("div.btn-row",
        h("button.btn", { type: "button", on: { click: () => newStop() } }, "Add another stop"),
        h("a.btn.secondary", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft"),
        h("a.btn.quiet", { href: "#/" }, "Done"))));
  map.focusStop(a);
}

// A stop that exists only in the draft: what it will be, and ways to change it.
export function showDraftStop(ch) {
  map.endModes();
  map.clearRoute();
  const a = ch.after;
  nameHere(a.name);
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/" }, "Back to search"),
      h("div.title-block",
        h("h1", a.name),
        h("p.ids", `Stop ${ch.entity_key}`),
        a.description ? h("p.stop-description", a.description) : null),
      pendingNotice(pendingActions("stop", ch.entity_key), { intro: `This stop is new in your draft "${state.draft.title}". It does not exist for passengers until the draft is committed.` }),
      h("dl.facts",
        h("dt", "Position"), h("dd", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`),
        a.platform_code ? [h("dt", "Platform"), h("dd", a.platform_code)] : null),
      h("div.btn-row",
        h("button.btn", { type: "button", on: { click: () => newStop(ch) } }, "Edit"),
        h("button.btn.danger", { type: "button", on: { click: async () => {
          if (!(await confirmDialog("Remove this new stop from the draft?", `${a.name} (${ch.entity_key}) will not be created. A route stop list in the draft that uses it will show a problem.`, { confirm: "Remove from draft", danger: true }))) return;
          try {
            await removeChange(ch.change_id);
            toast(`${a.name} removed from the draft.`);
            location.hash = "#/";
          } catch (e) {
            toast(e.message, "error");
          }
        } } }, "Remove from draft"))));
  map.focusStop(a);
  map.showDrafted(draftedPoints(pendingActions("stop", ch.entity_key)));
}

// A station that exists only in the draft: its point and the stops it groups.
export function showDraftStation(ch) {
  map.endModes();
  map.clearRoute();
  const a = ch.after;
  const members = Array.isArray(a.members) ? a.members : (a.member_stop_ids || []).map((id) => ({ stop_id: id }));
  const rows = h("ul.list", members.map((m) => h("li.list-item", { dataset: { stop: m.stop_id } },
    h("span.key", m.platform_code || ""),
    h("a", { href: `#/stop/${enc(m.stop_id)}` }, m.stop_id),
    h("span.hint", ""),
    h("span.sub", m.stop_id))));
  nameHere(a.name);
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/" }, "Back to search"),
      h("div.title-block",
        h("h1", a.name),
        h("p.ids", `Station ${ch.entity_key}`),
        a.description ? h("p.stop-description", a.description) : null),
      pendingNotice(pendingActions("station", ch.entity_key), { intro: `This station is new in your draft "${state.draft.title}". Passengers do not see it, and its stops are not grouped, until the draft is committed.` }),
      h("dl.facts", h("dt", "Station point"), h("dd", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)),
      h("div.btn-row",
        h("a.btn.secondary", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft"))),
    h("section.section",
      h("h2", `Stops it will group (${members.length})`),
      rows));
  map.focusStop({ ...a, stop_id: ch.entity_key, location_type: 1 });
  map.showDrafted(draftedPoints(pendingActions("station", ch.entity_key)));
  // the members' names, where they are live
  members.forEach((m) => get(`feeds/${enc(state.feedId)}/stops/${enc(m.stop_id)}`).then((d) => {
    const li = rows.querySelector(`[data-stop="${CSS.escape(m.stop_id)}"]`);
    if (!li) return;
    li.querySelector("a").textContent = d.name;
    li.querySelector(".hint").textContent = `${d.route_count} route${d.route_count === 1 ? "" : "s"}`;
  }).catch(() => {}));
}

// ------------------------------------------------------------------ new route
export async function newRoute() {
  if (!(await requireDraft(DRAFT_REASON))) {
    location.hash = "#/";
    return;
  }
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  let dirty = false;
  setLeaveGuard(() => (dirty ? "The new route is not in the draft yet." : null));
  const f = {
    id: h("input", { type: "text", id: "new-route-id", autocomplete: "off", spellcheck: "false", "aria-describedby": "new-route-id-status" }),
    short: h("input", { type: "text", id: "new-route-short", autocomplete: "off", placeholder: "For example 570X" }),
    long: h("input", { type: "text", id: "new-route-long", autocomplete: "off", placeholder: "For example Kelambakkam - CMBT" }),
    color: h("input", { type: "text", id: "new-route-color", maxlength: "7", placeholder: "#0B6660" }),
  };
  const swatch = h("input", { type: "color", value: "#14252a", "aria-label": "Pick a colour" });
  swatch.addEventListener("input", () => { f.color.value = swatch.value.toUpperCase(); dirty = true; });
  f.color.addEventListener("input", () => { if (/^#[0-9a-f]{6}$/i.test(f.color.value)) swatch.value = f.color.value; });
  const idStatus = h("p.hint", { id: "new-route-id-status", "aria-live": "polite" }, "Letters, digits, _ - or . Not used by any other route.");
  const problems = h("div");
  let idFree = null;

  let idSeq = 0;
  const checkId = debounce(async () => {
    const id = f.id.value.trim();
    const mine = ++idSeq;
    idFree = null;
    if (!id) { idStatus.className = "hint"; idStatus.textContent = "Letters, digits, _ - or . Not used by any other route."; return; }
    if (!ID_RE.test(id)) { idStatus.className = "field-error"; idStatus.textContent = `Not a valid route id. ${ID_RULE}`; idFree = false; return; }
    if (createdChange("route", id)) { idStatus.className = "field-error"; idStatus.textContent = `Your draft already creates a route with id ${id}.`; idFree = false; return; }
    try {
      const r = await get(`feeds/${enc(state.feedId)}/routes/${enc(id)}`);
      if (mine !== idSeq) return;
      idFree = false;
      idStatus.className = "field-error";
      idStatus.textContent = `Route id ${id} is already used by route ${r.short_name || id}${r.long_name ? ` (${r.long_name})` : ""}.`;
    } catch (e) {
      if (mine !== idSeq) return;
      if (e instanceof ApiError && e.status === 404) {
        idFree = true;
        idStatus.className = "hint ok";
        idStatus.textContent = `${id} is free to use.`;
      }
    }
  }, 350);
  f.id.addEventListener("input", checkId);

  const cancel = () => { dirty = false; setLeaveGuard(null); location.hash = "#/"; };
  const save = async (ev) => {
    ev.preventDefault();
    const id = f.id.value.trim(), short = f.short.value.trim(), long = f.long.value.trim();
    const color = f.color.value.trim().toUpperCase();
    const errs = [];
    if (!ID_RE.test(id)) errs.push(`Give the route an id. ${ID_RULE}`);
    else if (idFree === false) errs.push(idStatus.textContent);
    if (!short) errs.push("Give the route the number passengers see, for example 570X.");
    if (color && !/^#[0-9A-F]{6}$/.test(color)) errs.push("Colour must be # followed by six hex digits, for example #0B6660.");
    if (errs.length) {
      clear(problems, h("div.notice.error", { role: "alert" }, h("ul", errs.map((x) => h("li", x)))));
      return;
    }
    const after = { route_id: id, short_name: short };
    if (long) after.long_name = long;
    if (color) after.color = color;
    try {
      const res = await addChange({ entity: "route", op: "create", entity_key: id, after }, { merge: false });
      if (!res) return;
      dirty = false;
      if (res.problems.some((p) => p.level === "error")) {
        clear(problems, problemList(res.problems), h("p.hint", "The route is in your draft with these problems. Remove it from the draft page, or fix them there."));
        return;
      }
      setLeaveGuard(null);
      let route;
      try {
        route = await get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(id)}`);
      } catch {
        route = { route_id: id, short_name: short, long_name: long || null, color: color || null, rows: [], rows_hash: await emptyListHash() };
      }
      editRouteRows(route, { created: true });
    } catch (e) {
      clear(problems, h("p.notice.error", e.message));
    }
  };

  clear(panel(), h("form", { novalidate: true, on: { submit: save, input: () => { dirty = true; } } },
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the map"),
      h("h1", "New route"),
      h("div.notice",
        h("p", h("strong", "When will passengers see it? ")),
        h("p", "The route is saved to the database when its draft is committed. It shows in the passenger apps and the live APIs only after the nightly GTFS build gives it trips from the MTC schedule. Until then it has no buses.")),
      h("label.field", { for: "new-route-id" }, h("span", "Route id"), f.id, idStatus),
      h("label.field", { for: "new-route-short" }, h("span", "Route number passengers see"), f.short),
      h("label.field", { for: "new-route-long" }, h("span", "Route name (optional)"), f.long),
      h("label.field", { for: "new-route-color" }, h("span", "Colour on maps (optional)"), h("div.btn-row", swatch, h("div", { style: "flex:1" }, f.color))),
      h("p.hint", "Next you build its stop list: add the stops in order and mark the fare stages."),
      problems),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "submit" }, "Add route, then add its stops"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  ));
  f.id.focus();
}
