// Coordinates to review. The cleanup flags stops whose coordinate is probably
// wrong: carrying another stop's point, or stranded off their own routes. A person
// checks each one on the map, with every route's legs through it, then moves the
// stop, splits the routes that belong to another place off onto a new stop, or
// confirms the position is right. A move or a split is a change in a draft; it
// goes live only when someone else approves that draft and it is committed.
import { get, post, enc, ApiError } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, modal, confirmDialog, debounce, fmtCoord, fmtDate, fmtMetres, fmtCount, haversine, plural } from "./util.js";
import * as map from "./map.js";
import { requireDraft, refreshDraft, useDraft } from "./drafts.js";
import { actionsList, draftedPoints, pendingActions } from "./overlay.js";
import { foldedList, platformsNote } from "./context.js";
import { undoScope } from "./undo.js";
import { nameHere } from "./trail.js";

const panel = () => document.getElementById("panel");
const STATUSES = [["pending", "To review"], ["approved", "In a draft"], ["confirmed", "Confirmed"], ["committed", "Live"]];
const STATUS_TEXT = Object.fromEntries(STATUSES);
const PAGE = 50;
const FAR_METRES = 500;          // the server warns about a move this long
const SAME_POINT_METRES = 3;
const DRAFT_REASON = "Moves and splits are added to a draft. Nothing changes for passengers until someone else approves the draft and it is committed.";
// What the clean-up tool made of a review (evidence.auto_fix.action), for the list's filter.
const AUTO_FIX = [["merge", "Merge"], ["move", "Move"], ["choose", "Choose"], ["none", "No fix"]];
const AUTO_FIX_TEXT = { merge: "tool: merge", move: "tool: move", choose: "tool: choose one", none: "tool: no fix" };
const CONFIRM_NOTES = ["Checked on Google Maps: the bus stops here.", "Its routes do pass this point.", "The stops sharing this point are one place."];

// The list survives opening a review and coming back, and gives "Next".
const list = { feedId: null, status: "pending", q: "", area: false, autoFix: "", items: [], cursor: null, counts: null, loaded: false };
let stopWatching = null;
// What the last move, split or confirmation did, shown on the next screen.
let flash = null;

const round7 = (x) => Math.round(x * 1e7) / 1e7;
const mapsUrl = (p) => `https://www.google.com/maps/search/?api=1&query=${round7(p.lat)},${round7(p.lon)}`;
const mapsLink = (p, words = "Google Maps") => h("a", { href: mapsUrl(p), target: "_blank", rel: "noopener noreferrer" }, words);
const routeLabel = (r) => (r && (r.short_name || r.route_id)) || "?";

// ------------------------------------------------------------------ detour
// How far a route goes out of its way to call at a point: previous stop -> point
// -> next stop, against previous -> next. A route that starts or ends at the stop
// has no detour. Over several routes, the median, as the server measures it.
function legDetour(leg, p) {
  if (!leg.prev || !leg.next || !p) return null;
  return haversine(leg.prev.lat, leg.prev.lon, p.lat, p.lon) + haversine(p.lat, p.lon, leg.next.lat, leg.next.lon)
    - haversine(leg.prev.lat, leg.prev.lon, leg.next.lat, leg.next.lon);
}

export function medianDetour(legs, p) {
  const vals = legs.map((l) => legDetour(l, p)).filter((v) => v !== null).sort((a, b) => a - b);
  if (!vals.length) return null;
  const mid = vals.length >> 1;
  return vals.length % 2 ? vals[mid] : (vals[mid - 1] + vals[mid]) / 2;
}

const detourText = (m) => (m === null || m === undefined ? "not measured" : fmtMetres(Math.max(0, m)));

// "13.0123, 80.2345" as Google Maps copies it, or with a space
function parseCoords(value) {
  const m = String(value || "").trim().match(/^(-?\d+(?:\.\d+)?)\s*[,\s]\s*(-?\d+(?:\.\d+)?)$/);
  if (!m) return null;
  const lat = Number(m[1]), lon = Number(m[2]);
  if (Math.abs(lat) > 90 || Math.abs(lon) > 180 || (lat === 0 && lon === 0)) return null;
  return { lat, lon };
}

// ------------------------------------------------------------------ list state
function resetForFeed() {
  if (list.feedId === state.feedId) return;
  Object.assign(list, { feedId: state.feedId, status: "pending", q: "", area: false, autoFix: "", items: [], cursor: null, counts: null, loaded: false });
}

function watchMap(fn) {
  if (stopWatching) stopWatching();
  stopWatching = fn ? map.onMoveEnd(fn) : null;
}

// The router calls this before any screen, so the list stops following the map.
export function leaveCoordinates() {
  watchMap(null);
}

// The pending count in the top bar.
export async function refreshCoordinateCount() {
  const badge = document.getElementById("coordinates-count");
  if (!badge || !state.feedId) return;
  try {
    list.counts = await get(`feeds/${enc(state.feedId)}/position-reviews/summary`);
    badge.textContent = list.counts.pending ? fmtCount(list.counts.pending) : "";
    badge.hidden = !list.counts.pending;
  } catch {
    badge.hidden = true;
  }
}

async function fetchPage({ append = false } = {}) {
  const params = new URLSearchParams({ status: list.status, limit: String(PAGE) });
  if (list.q) params.set("q", list.q);
  if (list.area) params.set("bbox", map.bbox());
  if (list.autoFix) params.set("auto_fix", list.autoFix);
  if (append && list.cursor) params.set("cursor", list.cursor);
  const page = await get(`feeds/${enc(state.feedId)}/position-reviews?${params}`);
  list.items = append ? list.items.concat(page.items) : page.items;
  list.cursor = page.next_cursor;
  list.loaded = true;
}

function statusChip(r) {
  const cls = { pending: "submitted", approved: "approved", confirmed: "discarded", committed: "committed" }[r.status] || "";
  return h("span", { class: `chip ${cls}` }, STATUS_TEXT[r.status] || r.status);
}

function flashNotice() {
  if (!flash) return null;
  const f = flash;
  flash = null;
  return h("div.notice.ok", { role: "status" },
    h("p", h("strong", f.title), f.text ? ` ${f.text}` : ""),
    f.draftId ? h("p", h("a", { href: `#/drafts/${enc(f.draftId)}` }, `Open draft “${f.draftTitle || "untitled"}”`)) : null);
}

function reviewMeta(r) {
  const ev = r.evidence || {};
  const groups = Array.isArray(ev.route_groups) ? ev.route_groups.length : 0;
  return [
    ev.route_rows ? `in ${plural(ev.route_rows, "route list")}` : null,
    ev.mixed_origins && groups > 1 ? `routes from ${groups} original stops` : null,
    Array.isArray(ev.shares_point_with) && ev.shares_point_with.length ? `shares its point with ${plural(ev.shares_point_with.length, "stop")}` : null,
    ev.detour_m ? `detour ${fmtMetres(ev.detour_m)}` : null,
    r.suggested_lat != null ? "has a suggestion" : null,
    ev.auto_fix && AUTO_FIX_TEXT[ev.auto_fix.action] ? AUTO_FIX_TEXT[ev.auto_fix.action] : null,
  ].filter(Boolean).join(" · ");
}

// ------------------------------------------------------------------ the list
export async function showCoordinatesList() {
  resetForFeed();
  setLeaveGuard(null);
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  const search = h("input", { type: "search", id: "review-search", value: list.q, autocomplete: "off", spellcheck: "false", placeholder: "Stop name or stop id" });
  const area = h("input", { type: "checkbox", id: "review-area", checked: list.area });
  const tabs = h("div.tabs.count-tabs", { role: "group", "aria-label": "Show reviews that are" });
  const fixes = h("div.filter-chips", { role: "group", "aria-label": "Show reviews where the tool suggests", hidden: true });
  const items = h("div", { "aria-live": "polite" }, h("p.empty", "Loading…"));
  const more = h("button.btn.secondary", { type: "button", hidden: true }, "Show more");

  const renderTabs = () => clear(tabs, STATUSES.map(([key, label]) => h("button", {
    type: "button", "aria-pressed": String(list.status === key),
    on: { click: () => { if (list.status === key) return; list.status = key; renderTabs(); load(); } },
  }, label, list.counts ? h("span.count", fmtCount(list.counts[key] || 0)) : null)));

  // what the clean-up tool suggested, with how many of each; an older server
  // sends no such counts, and then there is nothing to filter by
  const renderFixes = () => {
    const counts = list.counts && list.counts.auto_fix;
    fixes.hidden = !counts;
    if (!counts) return;
    const chip = (key, label, n) => h("button.route-chip", {
      type: "button", "aria-pressed": String(list.autoFix === key), dataset: { autoFix: key || "any" },
      on: { click: () => { if (list.autoFix === key) return; list.autoFix = key; renderFixes(); load(); } },
    }, label, n != null ? h("span.count", fmtCount(n)) : null);
    clear(fixes, h("span.hint", "The tool suggests:"), chip("", "Anything"), AUTO_FIX.map(([key, label]) => chip(key, label, counts[key] || 0)));
  };

  const renderList = () => {
    more.hidden = !list.cursor;
    map.showReviewPoints(list.items, { onOpen: (r) => { location.hash = `#/coordinates/${r.review_id}`; } });
    if (!list.items.length) {
      const why = list.q ? `No review ${STATUS_TEXT[list.status].toLowerCase()} matches “${list.q}”.`
        : { pending: "Nothing is waiting for review.", approved: "No move or split is waiting in a draft.", confirmed: "No position has been confirmed as correct.", committed: "No fix from a review is live yet." }[list.status];
      return clear(items, h("p.empty", list.area ? `${why} Only the map area is searched; move the map or untick “Only in the map area”.` : why));
    }
    clear(items, h("ul.proposal-list", list.items.map((r) => h("li.proposal-item",
      h("a.proposal-link", { href: `#/coordinates/${r.review_id}` },
        h("span.proposal-name", r.stop_name),
        h("span.proposal-meta", [r.stop_id, reviewMeta(r)].filter(Boolean).join(" · "))),
      statusChip(r),
      r.reason ? h("span.review-reason", r.reason) : null,
      r.status === "approved" && r.change_set_id
        ? h("a.proposal-draft", { href: `#/drafts/${enc(r.change_set_id)}` }, `In draft “${r.change_set_title || "untitled"}”`) : null,
      r.status === "confirmed" ? h("span.proposal-note", r.review_note ? `Confirmed: ${r.review_note}` : "Confirmed as correct.") : null))));
  };

  let seq = 0;
  const load = async ({ append = false } = {}) => {
    const mine = ++seq;
    if (!append) clear(items, h("p.empty", "Loading…"));
    try {
      await fetchPage({ append });
      if (mine !== seq) return;
      renderList();
    } catch (e) {
      if (mine === seq) clear(items, h("p.notice.error", e.message));
    }
  };

  let typing;
  search.addEventListener("input", () => {
    clearTimeout(typing);
    typing = setTimeout(() => { list.q = search.value.trim(); load(); }, 300);
  });
  area.addEventListener("change", () => { list.area = area.checked; load(); });
  more.addEventListener("click", () => load({ append: true }));
  watchMap(() => { if (list.area) load(); });

  clear(panel(),
    h("section.section",
      flashNotice(),
      h("h1", "Coordinates to review"),
      h("p.hint", "Each stop here may be in the wrong place: it carries another stop's coordinate, or its routes go out of their way to reach it. Open one, check it on the map with its routes, then move it, split the routes that belong elsewhere onto a new stop, or confirm it is right. Nothing goes live until the draft is approved by someone else and committed."),
      tabs,
      fixes,
      h("label.field", { for: "review-search" }, h("span", "Search"), search),
      h("label.check", { for: "review-area" }, area, " Only in the map area")),
    h("section.section", items, more));
  renderTabs();
  renderFixes();
  if (list.loaded && list.feedId === state.feedId && !list.area) renderList();
  else load();
  refreshCoordinateCount().then(() => { renderTabs(); renderFixes(); });
}

// ------------------------------------------------------------------ one review
export async function showCoordinateReview(id) {
  resetForFeed();
  setLeaveGuard(null);
  watchMap(null);
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  const here = location.hash;
  clear(panel(), h("section.section", h("p.empty", "Loading the review…")));
  let r;
  try {
    [r] = await Promise.all([get(`position-reviews/${enc(id)}`), list.loaded ? null : fetchPage().catch(() => null)]);
  } catch (e) {
    if (location.hash !== here) return;
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/coordinates" }, "Back to coordinates to review"), h("p.notice.error", e.message)));
  }
  if (location.hash !== here) return;

  const ev = r.evidence || {};
  const stop = r.stop || null;
  const alive = !!(stop && !stop.deleted);
  const current = alive ? { lat: stop.lat, lon: stop.lon } : { lat: r.lat, lon: r.lon };
  const raw = r.raw_lat != null && r.raw_lon != null ? { lat: r.raw_lat, lon: r.raw_lon } : null;
  const suggestion = r.suggested_lat != null && r.suggested_lon != null ? { lat: r.suggested_lat, lon: r.suggested_lon, source: r.suggested_source || "" } : null;
  const legs = Array.isArray(r.routes) ? r.routes : [];
  // the server always grades a problem error or warning now; error blocks a
  // change (stop_missing, stop_deleted, stop_merged_away, stop_is_station),
  // warning does not (moved_since_load)
  const problems = Array.isArray(r.problems) ? r.problems : [];
  const blocking = problems.filter((p) => p.level === "error");
  const warnings = problems.filter((p) => p.level !== "error");
  // the review may still take more actions while `approved`: several splits, then
  // a move, in the same draft (docs section 8.1); only Confirm needs `pending`
  const editable = can("editor") && (r.status === "pending" || r.status === "approved");
  const changeable = editable && alive && !blocking.length;
  const draftActions = Array.isArray(r.draft_actions) ? r.draft_actions : [];
  const hasMove = draftActions.some((a) => a.kind === "move");
  // merged into another stop in this draft: nothing is left here to move or split
  const mergeAction = draftActions.find((a) => a.kind === "merge") || null;
  const candidates = (Array.isArray(ev.same_name_candidates) ? ev.same_name_candidates : []).filter((c) => c && c.stop_id && c.stop_id !== r.stop_id);
  const autoFix = ev.auto_fix && typeof ev.auto_fix === "object" ? ev.auto_fix : null;
  nameHere(`Review: ${stop && stop.name ? stop.name : r.stop_name}`);
  // a route a split already took off this stop, in this draft: locked, and drawn
  // as settled rather than a choice to make
  const splitOff = new Map();
  draftActions.filter((a) => a.kind === "split").forEach((a) => (a.route_ids || []).forEach((rid) => splitOff.set(rid, a.new_stop_id)));

  // routes by id (a route calling twice has two legs), grouped by the original
  // stop they came from: suspect groups first, then the stop's own, then the rest
  const byRoute = new Map();
  legs.forEach((l) => {
    if (!byRoute.has(l.route_id)) byRoute.set(l.route_id, { route_id: l.route_id, short_name: l.short_name, legs: [] });
    byRoute.get(l.route_id).legs.push(l);
  });
  const rawGroups = Array.isArray(ev.route_groups) ? ev.route_groups : [];
  const placed = new Set();
  const groups = rawGroups.map((g, i) => ({ g, i }))
    .sort((a, b) => Number(!!b.g.suspect) - Number(!!a.g.suspect) || a.i - b.i)
    .map(({ g }) => ({
      ...g,
      routes: (Array.isArray(g.route_ids) ? g.route_ids : []).filter((rid) => byRoute.has(rid) && !placed.has(rid) && placed.add(rid)).map((rid) => byRoute.get(rid)),
    }));
  const others = [...byRoute.values()].filter((x) => !placed.has(x.route_id));
  const mixed = !!ev.mixed_origins && rawGroups.length > 1;
  const checkable = byRoute.size - splitOff.size;
  const splittable = changeable && !hasMove && !mergeAction && byRoute.size > 1 && checkable > 0;
  const checked = new Set();
  if (splittable && ev.mixed_origins && rawGroups.some((g) => !g.suspect)) {
    groups.filter((g) => g.suspect).forEach((g) => g.routes.forEach((x) => { if (!splitOff.has(x.route_id)) checked.add(x.route_id); }));
  }

  let pin = null;
  let dirty = false;
  setLeaveGuard(() => (dirty ? `The new position for ${r.stop_name} is not in a draft yet.` : null));
  // The pin and the checked routes are not in a draft until Move or Split: each
  // placement, finished drag and tick is one step to undo. A reload after an
  // action starts again with nothing to undo.
  const history = undoScope("this review");
  let placedPin = null;

  const originPoints = groups.filter((g) => g.suspect && g.raw_lat != null && g.raw_lon != null && g.routes.length)
    .map((g) => ({ lat: g.raw_lat, lon: g.raw_lon, label: `Raw point of ${g.origin_name || g.origin_stop_id}` }));
  const view = map.showPositionReview({
    stopId: r.stop_id, current, loaded: { lat: r.lat, lon: r.lon }, raw, suggestion, origins: originPoints,
    onPlace: changeable ? (la, lo, how) => place(la, lo, how) : null,
  });
  view.setDrafted(draftedPoints(draftActions));

  const root = h("div");
  const statusBox = h("div.review-status");
  const pinBox = h("div", { "aria-live": "polite", style: "display:grid;gap:8px" });
  const routesBox = h("section.section", { "aria-label": "Routes at this stop" });
  const candidatesBox = h("section.section.candidates", { hidden: true });
  const sharingBox = h("section.section", { hidden: true });
  const actionsBox = h("div.sticky-actions");
  const actionProblems = h("div");
  const coordsInput = h("input", { type: "text", id: "review-coords", autocomplete: "off", spellcheck: "false", placeholder: "For example 13.012345, 80.234567" });
  const coordsError = h("p.field-error", { role: "alert", hidden: true });
  const noteInput = h("input", { type: "text", id: "review-note", maxlength: "500", autocomplete: "off", placeholder: "What you checked, for the approver" });
  const nameInput = h("input", { type: "text", id: "split-name", maxlength: "200", autocomplete: "off", placeholder: r.stop_name });
  history.fields([[noteInput, "the note"], [nameInput, "the new stop's name"]]);

  // previous and next in the list this came from
  const at = list.items.findIndex((x) => x.review_id === r.review_id);
  const prev = at > 0 ? list.items[at - 1] : null;
  const next = at >= 0 ? list.items[at + 1] : list.items.find((x) => x.status === "pending" && x.review_id !== r.review_id) || null;
  const goNext = () => {
    const i = list.items.findIndex((x) => x.review_id === r.review_id);
    if (i >= 0 && list.status === "pending") list.items.splice(i, 1);
    const following = list.items[i >= 0 ? i : 0];
    location.hash = following && following.status === "pending" ? `#/coordinates/${following.review_id}` : "#/coordinates";
  };
  // an action leaves the reviewer on the same review, so more can follow (docs
  // section 8.1); reload it from the server and redraw everything
  const reload = () => { dirty = false; setLeaveGuard(null); list.loaded = false; showCoordinateReview(r.review_id); };

  // ---- the server's detour for a point, debounced; an instant local figure
  // shows meanwhile and whenever the server has not been asked this point yet
  const detourCache = new Map();
  const askDetour = debounce(async (key, lat, lon, routeIds) => {
    try {
      const params = new URLSearchParams({ lat: String(round7(lat)), lon: String(round7(lon)) });
      if (routeIds) params.set("route_ids", routeIds.join(","));
      const res = await get(`position-reviews/${enc(r.review_id)}?${params}`);
      detourCache.set(key, res.detour_m_after ?? null);
    } catch {
      detourCache.set(key, undefined);
    }
    if (document.body.contains(root)) { renderPin(); renderRoutes(); }
  }, 300);
  function detourAt(legsSubset, target, routeIds) {
    if (!target) return null;
    const key = `${target.lat.toFixed(6)},${target.lon.toFixed(6)}|${routeIds ? routeIds.join(",") : "all"}`;
    if (detourCache.has(key) && detourCache.get(key) !== undefined) return detourCache.get(key);
    askDetour(key, target.lat, target.lon, routeIds);
    return medianDetour(legsSubset, target);
  }

  // ---- the map
  function legSpecs() {
    const specs = [];
    const splitting = checked.size > 0;
    for (const route of byRoute.values()) {
      const settled = splitOff.get(route.route_id);
      for (const leg of route.legs) {
        const base = { prev: leg.prev, next: leg.next, routes: [routeLabel(route)] };
        if (settled) {
          specs.push({ ...base, via: current, kind: "was" });
          const action = draftActions.find((a) => a.kind === "split" && (a.route_ids || []).includes(route.route_id));
          if (action) specs.push({ ...base, via: { lat: action.lat, lon: action.lon }, kind: "drafted" });
        } else if (mergeAction && mergeAction.lat != null) {
          specs.push({ ...base, via: current, kind: "was" });
          specs.push({ ...base, via: { lat: mergeAction.lat, lon: mergeAction.lon }, kind: "drafted" });
        } else if (hasMove) {
          specs.push({ ...base, via: current, kind: "was" });
          const action = draftActions.find((a) => a.kind === "move");
          specs.push({ ...base, via: { lat: action.lat, lon: action.lon }, kind: "drafted" });
        } else if (pin && (!splitting || checked.has(route.route_id))) {
          specs.push({ ...base, via: current, kind: "was" });
          specs.push({ ...base, via: pin, kind: splitting ? "split" : "move" });
        } else {
          specs.push({ ...base, via: current, kind: "now" });
        }
      }
    }
    return specs;
  }

  const endsOf = (routeIds) => legs.filter((l) => !routeIds || routeIds.has(l.route_id)).flatMap((l) => [l.prev, l.next]).filter(Boolean);

  let frame = 0;
  const redraw = () => {
    if (frame) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      if (!document.body.contains(root)) return;
      view.setLegs(legSpecs());
      renderPin();
      renderRoutes();
      renderActions();
    });
  };

  // undo and redo put the pin back where it was put down before (or take it off)
  function putPin(p) {
    pin = p ? { ...p } : null;
    placedPin = pin;
    dirty = !!pin || checked.size > 0;
    view.setPin(pin);
    redraw();
  }

  function place(lat, lon, how) {
    // a drag reports every move, then once more when it ends: that end is the step
    if (how !== "dragend") {
      pin = { lat, lon };
      dirty = true;
      coordsError.hidden = true;
      if (how !== "drag") view.setPin(pin);
      if (["suggestion", "raw", "origin", "typed", "candidate"].includes(how)) {
        view.fit([pin, ...endsOf(checked.size ? checked : null)], { maxZoom: 18 });
      }
      redraw();
    }
    if (how === "drag") return;
    const before = placedPin, after = { lat, lon };
    if (before && before.lat === after.lat && before.lon === after.lon) return;
    placedPin = after;
    history.push({ label: before ? "moved the pin" : "placed the pin", undo: () => putPin(before), redo: () => putPin(after) });
  }

  // ---- the panel
  function setChecked(ids) {
    checked.clear();
    ids.forEach((rid) => checked.add(rid));
    view.setLegs(legSpecs());
    renderRoutes();
    renderActions();
  }

  function toggle(routeIds, on, focusId) {
    const before = [...checked];
    routeIds.forEach((rid) => { if (!splitOff.has(rid)) (on ? checked.add(rid) : checked.delete(rid)); });
    const after = [...checked];
    history.push({ label: `${on ? "checked" : "unchecked"} ${plural(routeIds.length, "route")}`, undo: () => setChecked(before), redo: () => setChecked(after) });
    view.setLegs(legSpecs());
    renderRoutes();
    renderActions();
    if (focusId) document.getElementById(focusId)?.focus();
  }

  function renderStatus() {
    const who = [r.reviewed_by_email ? ` by ${r.reviewed_by_email}` : "", r.reviewed_at ? ` on ${fmtDate(r.reviewed_at)}` : ""].join("");
    // a fresh node each time: the notice object below evaluates every status's
    // branch eagerly, so a single shared node would be pulled out of the first
    // place it was appended when a later branch reused it
    const draftLink = () => (r.change_set_id ? h("a", { href: `#/drafts/${enc(r.change_set_id)}` }, `“${r.change_set_title || "untitled"}”`) : "(unknown)");
    // the same list every panel uses for what a draft does to an entity (overlay.js)
    const words = { current: { ...current, live: false }, routeLabel: (rid) => routeLabel(byRoute.get(rid) || { route_id: rid }) };
    const drafted = actionsList(draftActions, words);
    // what the ACTIVE draft does to this stop outside this review
    const others = state.draft && state.draft.change_set_id !== r.change_set_id
      ? pendingActions("stop", r.stop_id).filter((a) => a.review_id !== r.review_id) : [];
    const notice = {
      pending: null,
      approved: h("div.notice.draft",
        h("p", "In draft ", draftLink(), who, "."),
        h("p", "It goes live when that draft is approved by someone else and committed. To undo an action, remove its change from the draft.")),
      confirmed: h("div.notice",
        h("p", "Confirmed as correct", who, r.review_note ? `: “${r.review_note}”` : "."),
        can("editor") ? h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: reopen } }, "Reopen")) : null),
      committed: h("p.notice.ok", "Fixed and live", r.change_set_id ? [", committed with draft ", draftLink()] : "", "."),
      superseded: h("p.notice", "A later load of coordinates to review replaced this one."),
    }[r.status];
    const mergedInto = (p) => (stop && stop.provenance && stop.provenance.merged_into) || null;
    clear(statusBox,
      statusChip(r),
      notice,
      drafted,
      blocking.length ? h("div.notice.error", { role: "alert" },
        h("p", h("strong", "This stop cannot be moved or split")),
        h("ul", blocking.map((p) => h("li", p.message,
          p.code === "stop_merged_away" && mergedInto(p) ? [" ", h("a", { href: `#/stop/${enc(mergedInto(p))}` }, `Open stop ${mergedInto(p)}`)] : null))),
        editable ? h("p", "Nothing is left to fix here. Close the review with “Position is correct”, and check the stop that stayed if its position looks wrong.") : null) : null,
      warnings.length ? h("div.notice.warning", h("p", h("strong", "Check first")), h("ul", warnings.map((p) => h("li", p.message)))) : null,
      mixed ? h("div.notice.warning", { role: "note" },
        h("p", h("strong", `This stop serves routes from ${rawGroups.length} original stops — moving it moves all of them. Split the wrong ones off instead.`))) : null,
      editable && others.length
        ? h("div.notice.draft", h("p", `Your draft “${state.draft.title}” already changes this stop, outside this review.`), actionsList(others, { current })) : null);
  }

  // what the clean-up tool made of it: its verdict, in words, with the detour it expects
  function autoFixBanner() {
    if (!autoFix || !autoFix.action) return null;
    const target = autoFix.into_stop_id ? candidates.find((c) => c.stop_id === autoFix.into_stop_id) : null;
    const detour = autoFix.detour_m != null
      ? ` — detour ${detourText(autoFix.detour_m)}${autoFix.detour_m_after != null ? ` → ${detourText(autoFix.detour_m_after)}` : ""}` : "";
    const says = {
      merge: ["The tool suggests merging into ", autoFix.into_stop_id ? h("a", { href: `#/stop/${enc(autoFix.into_stop_id)}` }, target ? `${target.name} (${autoFix.into_stop_id})` : autoFix.into_stop_id) : "a same-named stop", detour, "."],
      move: [`The tool suggests moving it${autoFix.lat != null ? ` to ${fmtCoord(autoFix.lat)}, ${fmtCoord(autoFix.lon)}` : ""}`, detour, "."],
      choose: ["The tool found more than one place that would fit and did not choose", detour, ". Pick one of the same-named stops below, or place the pin."],
      none: ["The tool found no fix", detour, ". Check it by hand."],
    }[autoFix.action];
    if (!says) return null;
    return h("div.notice.auto-fix", { role: "note", dataset: { autoFix: autoFix.action } },
      h("p", h("strong", says)),
      autoFix.reason ? h("p", autoFix.reason) : null,
      h("p.hint", [autoFix.tool ? `From ${autoFix.tool}` : null, autoFix.threshold_m != null ? `it calls a detour under ${fmtMetres(autoFix.threshold_m)} a fit` : null].filter(Boolean).join("; "),
        ". A suggestion only: nothing is changed until you add it to a draft."),
      changeable && !mergeAction ? h("div.btn-row",
        autoFix.action === "move" && autoFix.lat != null
          ? h("button.btn.secondary.small", { type: "button", id: "use-auto-fix", on: { click: () => place(autoFix.lat, autoFix.lon, "suggestion") } }, "Use this position") : null,
        autoFix.action === "merge" && target
          ? h("button.btn.secondary.small", { type: "button", id: "merge-auto-fix", on: { click: () => mergeInto(target) } }, "Merge into this stop…") : null) : null);
  }

  // same-named stops that may be the right place: move this stop onto one, or merge into it
  function renderCandidates() {
    if (!candidates.length) return;
    candidatesBox.hidden = false;
    clear(candidatesBox,
      h("h2", `Same-named stops that may be the right place (${candidates.length})`),
      h("p.hint", "Numbered on the map: teal where this stop's routes would fit, grey where they would not. Use a position to move this stop there, or merge this stop into the other when they are one stop entered twice."),
      h("ol.candidate-list", { style: "list-style:none;margin:0;padding:0" }, candidates.map((c, i) => h("li.candidate", { dataset: { stop: c.stop_id } },
        h("span.picker-no", { class: c.verdict === "fits" ? "fits" : "no_fit", "aria-hidden": "true" }, String(i + 1)),
        h("div", { style: "display:grid;gap:2px;min-width:0" },
          h("span", h("a", { href: `#/stop/${enc(c.stop_id)}` }, h("strong", c.name)), " ",
            c.verdict === "fits" ? h("span.chip.approved", "Routes fit") : h("span.chip.discarded", "Routes do not fit")),
          h("span.meta", [c.stop_id, c.distance_m != null ? `${fmtMetres(c.distance_m)} away` : null, plural(c.route_count || 0, "route"),
            c.name_similarity != null && c.name_similarity < 1 ? `name ${Math.round(c.name_similarity * 100)}% alike` : "same name",
            c.shares_route ? "on one of this stop's routes" : null,
            c.detour_m_after != null ? `detour there ${detourText(c.detour_m_after)}` : null].filter(Boolean).join(" · "))),
        changeable && !mergeAction ? h("div.btn-row",
          h("button.btn.secondary.small", { type: "button", id: `use-candidate-${c.stop_id}`, "aria-label": `Use the position of ${c.name} (${c.stop_id})`, disabled: hasMove, on: { click: () => place(c.lat, c.lon, "candidate") } }, "Use this position"),
          h("button.btn.secondary.small", { type: "button", id: `merge-candidate-${c.stop_id}`, "aria-label": `Merge into ${c.name} (${c.stop_id})`, disabled: draftActions.length > 0, on: { click: () => mergeInto(c) } }, "Merge into this stop…")) : null))),
      changeable && draftActions.length && !mergeAction ? h("p.hint", "This review already has a move or a split in its draft, so it cannot also be merged away. Remove those from the draft first.") : null);
  }

  function renderPin() {
    if (!changeable) return;
    if (!pin) {
      clear(pinBox, h("p.notice", h("strong", "No new position yet. "), "Click the map where the bus stops, or use a point below. Then drag the teal pin to the kerb."));
      return;
    }
    const moved = haversine(current.lat, current.lon, pin.lat, pin.lon);
    const after = detourAt(legs, pin, null);
    clear(pinBox,
      h("p", h("strong", `${fmtCoord(pin.lat)}, ${fmtCoord(pin.lon)}`), h("span.hint", `, ${fmtMetres(moved)} from where it is now. `), mapsLink(pin, "Check in Google Maps")),
      legs.length ? h("p.detour-line", "Moving the stop here: the detour of its routes goes from ",
        h("span.detour-now", detourText(r.detour_m)), " to ", h("span.detour-new", detourText(after)), ".") : null,
      moved > FAR_METRES ? h("p.notice.warning", `That is ${fmtMetres(moved)} from where it is now, far for a kerb fix. Check it is the right place.`) : null);
  }

  function routeRow(route) {
    const inputId = `split-route-${route.route_id}`;
    const isChecked = checked.has(route.route_id);
    const settled = splitOff.get(route.route_id);
    const target = settled ? null : hasMove ? draftActions.find((a) => a.kind === "move")
      : pin && (!checked.size || isChecked) ? pin : null;
    const nowD = medianDetour(route.legs, current);
    const thereD = target ? medianDetour(route.legs, target) : null;
    const name = h("span.split-route-name", h("strong", routeLabel(route)));
    const legLines = route.legs.map((leg) => h("span.leg-line", `${leg.prev ? leg.prev.name : "starts here"} → this stop → ${leg.next ? leg.next.name : "ends here"}`));
    if (settled) {
      return h("li.split-route.locked",
        h("div.split-route-body",
          h("div.split-route-head", name, h("span.chip.split-off", "Split off → ", h("a", { href: `#/stop/${enc(settled)}` }, settled))),
          legLines,
          h("span.hint", nowD === null ? "No detour measured: the route starts or ends here." : `Was detour ${detourText(nowD)}.`)));
    }
    const body = [name,
      legLines,
      h("span.hint", nowD === null ? "No detour measured: the route starts or ends here."
        : [`Detour ${detourText(nowD)}`, thereD !== null ? `, ${detourText(thereD)} at the ${hasMove ? "drafted point" : "pin"}` : "", "."].join("")),
    ];
    return h("li.split-route", { class: isChecked ? "checked" : "" },
      splittable ? h("input", { type: "checkbox", id: inputId, checked: isChecked, on: { change: (e) => toggle([route.route_id], e.target.checked, inputId) } }) : null,
      splittable ? h("label.split-route-body", { for: inputId }, body) : h("div.split-route-body", body));
  }

  function groupBlock(g) {
    if (!g.routes.length) return null;
    const i = groups.indexOf(g);
    const openIds = g.routes.map((x) => x.route_id).filter((rid) => !splitOff.has(rid));
    const all = openIds.length > 0 && openIds.every((x) => checked.has(x)), some = openIds.some((x) => checked.has(x));
    const gid = `split-group-${i}`;
    const box = splittable && openIds.length ? h("input", { type: "checkbox", id: gid, checked: all, on: { change: (e) => toggle(openIds, e.target.checked, gid) } }) : null;
    if (box) box.indeterminate = some && !all;
    const title = [h("strong", g.origin_name || g.origin_stop_id || "Routes"), " ", h("span.meta", g.origin_stop_id || ""), " ",
      g.suspect ? h("span.chip.rejected", "Probably another place") : h("span.chip", "Its own routes")];
    const rawAt = g.raw_lat != null && g.raw_lon != null ? { lat: g.raw_lat, lon: g.raw_lon } : null;
    return h("div.route-group", { class: g.suspect ? "suspect" : "own" },
      h("div.route-group-head", box, box || (splittable && openIds.length) ? h("label.route-group-title", { for: gid }, title) : h("div.route-group-title", title)),
      g.reason ? h("p.hint", g.reason) : null,
      rawAt && openIds.length ? h("p.hint.route-group-raw", `Its raw point ${fmtCoord(rawAt.lat)}, ${fmtCoord(rawAt.lon)} is ${fmtMetres(haversine(current.lat, current.lon, rawAt.lat, rawAt.lon))} from this stop. `,
        mapsLink(rawAt),
        changeable ? [" ", h("button.btn.quiet.small", { type: "button", "aria-label": `Put the pin at the raw point of ${g.origin_name || g.origin_stop_id}`, on: { click: () => place(rawAt.lat, rawAt.lon, "origin") } }, "Put the pin here")] : null) : null,
      h("ul.split-routes", g.routes.map(routeRow)));
  }

  // what splitting the checked routes off at the pin would do
  function splitBox() {
    const all = checked.size > 0 && checked.size === checkable;
    const splitLegs = legs.filter((l) => checked.has(l.route_id));
    const stayLegs = legs.filter((l) => !checked.has(l.route_id));
    const from = new Set(groups.filter((g) => g.suspect && g.routes.some((x) => checked.has(x.route_id))).map((g) => g.origin_stop_id));
    const after = checked.size ? detourAt(splitLegs, pin, [...checked].sort()) : null;
    return h("div.split-summary", { "aria-live": "polite" },
      h("h3", "Split routes off to a new stop"),
      checked.size ? h("p", h("strong", `${plural(checked.size, "route")} checked: ${[...checked].map((rid) => routeLabel(byRoute.get(rid))).join(", ")}.`)) : null,
      checked.size && !all && pin ? h("p", "At the pin their detour goes from ", h("span.detour-now", detourText(medianDetour(splitLegs, current))), " to ",
        h("span.detour-split", detourText(after)),
        `. The ${plural(byRoute.size - splitOff.size - checked.size, "route")} that stay keep a detour of ${detourText(medianDetour(stayLegs, current))}.`) : null,
      from.size > 1 ? h("p.notice.warning", `The checked routes came from ${from.size} different original stops. A split puts them all on one new stop at the pin, so check only the routes of one place.`) : null,
      all ? h("p.hint", "Every route left on the stop is checked. To take them all, move the stop instead.") : null,
      h("label.field", { for: "split-name" }, h("span", "Name of the new stop (optional)"), nameInput));
  }

  function renderRoutes() {
    const focusId = routesBox.contains(document.activeElement) ? document.activeElement.id : null;
    clear(routesBox,
      h("h2", `Routes at this stop (${byRoute.size})`),
      !byRoute.size ? h("p.empty", alive ? "No route calls at this stop now." : "The stop is gone, so no route calls at it.") : null,
      byRoute.size ? h("p.hint", splittable
        ? "Each route with the stops before and after it here. The red lines on the map are the routes through the stop now, the dotted lines the way they would go without it. To take some routes to another place, check them, put the pin there and split them off onto a new stop."
        : "Each route with the stops before and after it here. The red lines on the map are the routes through the stop now, the dotted lines the way they would go without it.") : null,
      groups.map(groupBlock),
      others.length ? h("div.route-group.other",
        groups.some((g) => g.routes.length) ? h("div.route-group-head", h("div.route-group-title", h("strong", "Other routes"))) : null,
        h("ul.split-routes", others.map(routeRow))) : null,
      splittable ? splitBox() : null);
    if (focusId) document.getElementById(focusId)?.focus();
  }

  function renderSharing(found) {
    if (!found.length) return;
    // a stop that has since become a platform of a station is named through it, once
    const where = (d) => (d <= SAME_POINT_METRES ? "same point" : fmtMetres(d));
    const names = new Map(found.filter((s) => s.parent).map((s) => [s.parent.stop_id, s.parent.name]));
    const [listed, stations] = foldedList(found, names, (s) => h("li.list-item",
      h("span.key", s.missing ? "gone" : s.deleted ? "removed" : where(s.distance_m)),
      h("a", { href: `#/stop/${enc(s.stop_id)}` }, s.name || s.stop_id),
      h("span.hint", s.route_count != null ? plural(s.route_count, "route") : ""),
      h("span.sub", s.missing ? `${s.stop_id}, no longer in the feed` : [s.stop_id, s.platform_code].filter(Boolean).join(", "))),
    { key: (g, nearest) => (nearest == null ? "" : where(nearest)) });
    sharingBox.hidden = false;
    clear(sharingBox,
      h("h2", `Stops that shared its point (${found.length})`),
      h("p.hint", "When it was flagged, these stops had exactly the same coordinate. This stop's coordinate may have come from one of them."),
      platformsNote(stations, found.length),
      listed);
  }

  // Move, Split, Position is correct and Next stay in reach at the bottom of the
  // panel; a button that cannot be used yet says why underneath. An action does
  // not leave the review: several can go into the same draft (docs section 8.1).
  function renderActions() {
    if (!editable) return;
    const gone = blocking.length ? "the stop is gone (see above)." : mergeAction ? `the stop is merged into ${mergeAction.into_stop_id} in this draft.` : null;
    const moveWhy = gone || (hasMove ? "the stop was already moved in this draft."
      : !pin ? "set a new position first."
        : haversine(current.lat, current.lon, pin.lat, pin.lon) < 1 ? "the pin is where the stop is now." : null);
    const splitWhy = !splittable && checkable === 0 && byRoute.size > 1 ? "every route has already been split off this stop."
      : !splittable ? null
        : gone || (hasMove ? "the stop was already moved in this draft."
          : checked.size && checked.size === checkable ? "all routes: use Move"
            : !checked.size ? "check the routes to split off." : !pin ? "put the pin where the checked routes stop." : null);
    clear(actionsBox,
      actionProblems,
      h("p.hint", state.draft ? `A move or a split goes into draft “${state.draft.title}”.` : "A move or a split asks which draft to add it to."),
      h("div.btn-row",
        h("button.btn", { type: "button", id: "review-move", disabled: !!moveWhy, "aria-describedby": moveWhy ? "why-move" : null, on: { click: move } }, "Add move to draft"),
        r.status === "pending" ? h("button.btn.secondary", { type: "button", id: "review-confirm", on: { click: confirmPosition } }, "Position is correct…") : null,
        next ? h("a.btn.quiet", { href: `#/coordinates/${next.review_id}`, "aria-label": `Next review: ${next.stop_name}` }, "Next ›") : null),
      splittable || (checkable === 0 && byRoute.size > 1) ? h("div.btn-row",
        h("button.btn", { type: "button", id: "review-split", class: checked.size ? "" : "secondary", disabled: !!splitWhy, "aria-describedby": splitWhy ? "why-split" : null, on: { click: split } },
          "Split checked routes to a new stop here")) : null,
      moveWhy || splitWhy ? h("div.why-lines",
        moveWhy ? h("p.why-not", { id: "why-move" }, h("strong", "Move: "), moveWhy) : null,
        splitWhy ? h("p.why-not", { id: "why-split" }, h("strong", "Split: "), splitWhy) : null) : null);
  }

  // ---- actions
  function failed(e, box) {
    const details = (e instanceof ApiError && e.details) || {};
    const code = e instanceof ApiError ? e.code : "";
    if (code === "change_set_not_draft") {
      useDraft(null);
      renderActions();
      return clear(box, h("p.notice.error", { role: "alert" }, "That draft was submitted or closed. Try again and choose another draft."));
    }
    if (code === "review_in_other_draft") {
      const csId = details.change_set_id, title = details.change_set_title;
      return clear(box, h("div.notice.error", { role: "alert" },
        h("p", e.message),
        csId ? h("p", "In draft ", h("a", { href: `#/drafts/${enc(csId)}` }, `“${title || "untitled"}”`), ".") : null));
    }
    if (code === "draft_conflict") {
      const ids = Array.isArray(details.change_ids) ? details.change_ids : [];
      return clear(box, h("div.notice.error", { role: "alert" },
        h("p", h("strong", "Your draft is in the way")),
        h("p", e.message),
        ids.length ? h("p", `${ids.length === 1 ? "The change" : "The changes"} in the way: ${ids.map((x) => `#${x}`).join(", ")}. Remove ${ids.length === 1 ? "it" : "them"} from the draft, or act in another draft.`) : null,
        state.draft ? h("p", h("a", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, `Open draft “${state.draft.title}”`)) : null));
    }
    if (code === "position_unchanged") {
      return clear(box, h("p.notice.error", { role: "alert" }, "That is where the stop is already. If the position is right, use “Position is correct” instead."));
    }
    if (code === "review_not_pending") {
      return clear(box, h("div.notice.error", { role: "alert" },
        h("p", "Someone else has already reviewed this stop."),
        h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: reload } }, "Show it as it is now"))));
    }
    const found = Array.isArray(details.problems) ? details.problems : [];
    clear(box, h("div.notice.error", { role: "alert" },
      h("p", h("strong", e.message)),
      found.length ? h("ul", found.map((p) => h("li", p.message))) : null));
  }

  async function move() {
    clear(actionProblems);
    if (!pin) return;
    if (mixed && checked.size) {
      const go = await confirmDialog("Move every route?", `${r.stop_name} serves routes from ${rawGroups.length} original stops. Moving it moves all ${plural(byRoute.size, "route")}, not only the checked ones. To move only those, split them off instead.`, { confirm: "Move all routes" });
      if (!go) return;
    }
    const draft = await requireDraft(DRAFT_REASON);
    if (!draft) return;
    const body = { change_set_id: draft.change_set_id, lat: round7(pin.lat), lon: round7(pin.lon) };
    if (noteInput.value.trim()) body.note = noteInput.value.trim();
    try {
      const res = await post(`position-reviews/${enc(r.review_id)}/move`, body);
      await refreshDraft().catch(() => null);
      toast(`Moved ${r.stop_name} ${fmtMetres(haversine(current.lat, current.lon, pin.lat, pin.lon))} in draft “${draft.title}”.${res.detour_m_after != null ? ` Detour now ${detourText(res.detour_m_after)}.` : ""}`);
      reload();
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  async function split() {
    clear(actionProblems);
    if (!pin || !checked.size) return;
    const draft = await requireDraft(DRAFT_REASON);
    if (!draft) return;
    const routeIds = [...checked];
    const body = { change_set_id: draft.change_set_id, route_ids: routeIds, lat: round7(pin.lat), lon: round7(pin.lon) };
    const name = nameInput.value.trim();
    if (name && name !== r.stop_name) body.name = name;
    if (noteInput.value.trim()) body.note = noteInput.value.trim();
    try {
      const res = await post(`position-reviews/${enc(r.review_id)}/split`, body);
      await refreshDraft().catch(() => null);
      const labels = routeIds.map((rid) => routeLabel(byRoute.get(rid)));
      toast(`Split ${labels.join(", ")} off ${r.stop_name} onto new stop ${res.new_stop_id} in draft “${draft.title}”.${res.detour_m_after != null ? ` Their detour is now ${detourText(res.detour_m_after)}.` : ""}`);
      reload();
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  // Merge the reviewed stop into a same-named one: every route moves to that stop
  // and this one goes away, as a stop/merge change in the draft.
  async function mergeInto(c) {
    clear(actionProblems);
    const namesDiffer = (c.name || "") !== (stop ? stop.name : r.stop_name);
    const answer = await modal("Merge into this stop?", (close) => {
      const keep = h("select", { id: "merge-keep-name" },
        h("option", { value: "into" }, `${c.name} (the stop that stays)`),
        h("option", { value: "from" }, `${stop ? stop.name : r.stop_name} (this stop)`));
      return h("form", { style: "display:grid;gap:10px", on: { submit: (e) => { e.preventDefault(); close({ keep_name: keep.value }); } } },
        h("p", `${stop ? stop.name : r.stop_name} (${r.stop_id}) is merged into ${c.name} (${c.stop_id}). Every route that calls here switches to ${c.stop_id}, and ${r.stop_id} is removed when the draft is committed.`),
        c.verdict !== "fits" ? h("p.notice.warning", "The tool does not think this stop's routes fit there. Check the map before merging.") : null,
        namesDiffer ? h("label.field", { for: "merge-keep-name" }, h("span", "Name to keep"), keep) : null,
        h("div.btn-row", h("button.btn", { type: "submit", id: "merge-confirm" }, "Add merge to draft"), h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
    });
    if (!answer) return;
    const draft = await requireDraft(DRAFT_REASON);
    if (!draft) return;
    const body = { change_set_id: draft.change_set_id, into_stop_id: c.stop_id };
    if (namesDiffer) body.keep_name = answer.keep_name;
    if (noteInput.value.trim()) body.note = noteInput.value.trim();
    try {
      const res = await post(`position-reviews/${enc(r.review_id)}/merge`, body);
      await refreshDraft().catch(() => null);
      const warned = Array.isArray(res.warnings) ? res.warnings : [];
      toast(`Merged ${r.stop_name} into ${c.name} (${c.stop_id}) in draft “${draft.title}”.${res.detour_m_after != null ? ` Detour there ${detourText(res.detour_m_after)}.` : ""}`);
      if (warned.length) flash = { title: "Merge added, with things to check:", text: warned.map((w) => w.message).join(" ") };
      reload();
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  async function confirmPosition() {
    clear(actionProblems);
    const note = await modal("Position is correct", (close) => {
      const ta = h("textarea", { id: "confirm-note", placeholder: "What did you check? (optional)" });
      return h("form", { style: "display:grid;gap:10px", on: { submit: (e) => { e.preventDefault(); close(ta.value.trim()); } } },
        h("p", `${r.stop_name} stays where it is, and this review is closed without changing anything.`),
        h("label.field", { for: "confirm-note" }, h("span", "Note (optional)"), ta),
        h("div.btn-row", h("span.hint", "Common notes:"), CONFIRM_NOTES.map((n) => h("button.btn.quiet.small", { type: "button", on: { click: () => { ta.value = n; ta.focus(); } } }, n))),
        h("div.btn-row", h("button.btn", { type: "submit" }, "Confirm position"), h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
    });
    if (note === undefined) return;
    try {
      await post(`position-reviews/${enc(r.review_id)}/confirm`, note ? { note } : {});
      dirty = false;
      setLeaveGuard(null);
      flash = { title: `${r.stop_name} confirmed as correct.`, text: "Nothing was changed." };
      refreshCoordinateCount();
      goNext();
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  async function reopen() {
    try {
      await post(`position-reviews/${enc(r.review_id)}/reopen`);
      toast(`${r.stop_name} is back in the list to review.`);
      refreshCoordinateCount();
      reload();
    } catch (e) {
      toast(e.message, "error");
    }
  }

  const useCoords = () => {
    const p = parseCoords(coordsInput.value);
    if (!p) {
      coordsError.hidden = false;
      coordsError.textContent = "Type latitude and longitude, for example 13.012345, 80.234567.";
      return;
    }
    place(p.lat, p.lon, "typed");
  };
  coordsInput.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); useCoords(); } });

  // ---- facts
  const numbers = [...new Set([...byRoute.values()].map(routeLabel))].sort((a, b) => a.localeCompare(b, "en", { numeric: true }));
  const routeNumbers = numbers.length ? numbers : Array.isArray(ev.route_numbers) ? ev.route_numbers : [];
  const detourNow = r.detour_m;
  const chalo = String(ev.chalo_nearby || "").split("|").map((x) => x.trim()).filter(Boolean);

  clear(panel(), root);
  root.append(
    h("section.section",
      flashNotice(),
      h("div.section-head",
        h("a.crumb", { href: "#/coordinates" }, "Back to coordinates to review"),
        h("div.btn-row.pager-mini",
          prev ? h("a.btn.quiet.small", { href: `#/coordinates/${prev.review_id}`, "aria-label": `Previous review: ${prev.stop_name}` }, "‹ Previous") : null,
          next ? h("a.btn.quiet.small", { href: `#/coordinates/${next.review_id}`, "aria-label": `Next review: ${next.stop_name}` }, "Next ›") : null)),
      h("div.title-block",
        h("h1", stop && stop.name ? stop.name : r.stop_name),
        h("p.ids", [`Stop ${r.stop_id}`, `review #${r.review_id}`, r.original_stop_id && r.original_stop_id !== r.stop_id ? `flagged as ${r.original_stop_id}` : null].filter(Boolean).join(" · "))),
      statusBox,
      h("div.review-why", h("h2", "Why it is flagged"), h("p", r.reason || "No reason was given.")),
      autoFixBanner(),
      h("dl.facts",
        h("dt", "Detour now"), h("dd", detourNow == null
          ? (legs.length ? "Not measured: its routes start or end here." : "No route calls at it.")
          : `${fmtMetres(Math.max(0, detourNow))}, the median over its ${plural(legs.filter((l) => l.prev && l.next).length, "route")}`),
        h("dt", "Routes"), h("dd.route-numbers", routeNumbers.length ? `${routeNumbers.slice(0, 24).join(", ")}${routeNumbers.length > 24 ? ` and ${routeNumbers.length - 24} more` : ""}` : "none"),
        chalo.length ? [h("dt", "Chalo has nearby"), h("dd", chalo.join("; "))] : null,
        h("dt", "Position now"), h("dd", `${fmtCoord(current.lat)}, ${fmtCoord(current.lon)} `, mapsLink(current, "Open in Google Maps")),
        raw ? [h("dt", "MTC raw point"), h("dd", `${fmtCoord(raw.lat)}, ${fmtCoord(raw.lon)}, ${fmtMetres(haversine(current.lat, current.lon, raw.lat, raw.lon))} away `, mapsLink(raw))] : null,
        suggestion ? [h("dt", "Suggestion"), h("dd", `${suggestion.source || "no source given"}: ${fmtCoord(suggestion.lat)}, ${fmtCoord(suggestion.lon)}, ${fmtMetres(haversine(current.lat, current.lon, suggestion.lat, suggestion.lon))} away `, mapsLink(suggestion))] : null,
        h("dt", "Loaded"), h("dd", [r.batch, r.created_at ? fmtDate(r.created_at) : null].filter(Boolean).join(", ") || "unknown")),
      !can("editor") ? h("p.hint", "You can look at these reviews. Ask an admin for the editor role to review them.") : null),
    changeable ? h("section.section",
      h("h2", "New position"),
      pinBox,
      h("div.btn-row",
        suggestion ? h("button.btn.secondary.small", { type: "button", id: "use-suggestion", on: { click: () => place(suggestion.lat, suggestion.lon, "suggestion") } }, "Use suggestion") : null,
        raw ? h("button.btn.secondary.small", { type: "button", id: "use-raw", on: { click: () => place(raw.lat, raw.lon, "raw") } }, "Use raw point") : null),
      h("label.field", { for: "review-coords" }, h("span", "Or type the coordinates"),
        h("div.picker-bar", coordsInput, h("button.btn.secondary.small", { type: "button", on: { click: useCoords } }, "Put the pin there"))),
      coordsError,
      history.buttons(),
      h("label.field", { for: "review-note" }, h("span", "Note (optional)"), noteInput)) : null,
    candidatesBox,
    routesBox,
    sharingBox,
    editable ? actionsBox : null);

  renderStatus();
  renderPin();
  renderCandidates();
  renderRoutes();
  renderActions();
  view.setLegs(legSpecs());
  view.setCandidates(candidates, changeable && !mergeAction && !hasMove ? (c) => place(c.lat, c.lon, "candidate") : null);
  view.fit([current, raw, suggestion, ...originPoints, ...candidates.filter((c) => c.lat != null), ...draftActions.filter((a) => a.lat != null).map((a) => ({ lat: a.lat, lon: a.lon })), ...endsOf(null)].filter(Boolean));

  // the stops that shared its point, where they are now
  const shares = (Array.isArray(ev.shares_point_with) ? ev.shares_point_with : []).filter((s) => s && s.stop_id && s.stop_id !== r.stop_id).slice(0, 12);
  if (shares.length) {
    const found = await Promise.all(shares.map((s) => get(`feeds/${enc(state.feedId)}/stops/${enc(s.stop_id)}`)
      .then((d) => ({ stop_id: s.stop_id, name: d.name || s.name, lat: d.lat, lon: d.lon, deleted: !!d.deleted, route_count: d.route_count,
        location_type: d.location_type, platform_code: d.platform_code, parent_station: d.deleted ? null : d.parent_station, parent: d.parent || null,
        distance_m: haversine(current.lat, current.lon, d.lat, d.lon) }))
      .catch(() => ({ stop_id: s.stop_id, name: s.name, missing: true }))));
    if (!document.body.contains(root)) return;
    view.setSharing(found.filter((s) => !s.missing && !s.deleted));
    renderSharing(found);
  }
}
