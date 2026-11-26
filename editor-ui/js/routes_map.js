// Several routes' map lines on the map at once, each in its own colour (docs
// section 29). Routes are added by number or name, from a stop's routes or from
// a route's page; the address keeps their ids (#/routes-map?ids=21G,45B), so a
// view can be bookmarked, shared, and come back to.
import { get, enc } from "./api.js";
import { state, pref, setPref } from "./state.js";
import { h, clear, debounce, plural, toast, routeStatusTag } from "./util.js";
import * as map from "./map.js";
import { routesTouched } from "./overlay.js";
import { createdChange } from "./drafts.js";
import { nameHere } from "./trail.js";

// One request each: more than this is a corridor study for another tool.
export const ROUTES_MAP_MAX = 20;
const COLORS = map.TRIP_COLORS;
const SOURCE = { gps: "from GPS", osrm: "through the stops by road", manual: "drawn by hand", upload: "uploaded", imported: "imported" };

const panel = () => document.getElementById("panel");

export const routesMapHref = (ids) => (ids.length ? `#/routes-map?ids=${ids.map(enc).join(",")}` : "#/routes-map");

// The ids in an address's `ids=`, read raw so an id with a comma in it
// (encoded, %2C) stays whole: each once, in order. {ids, more}: more is true
// when there were more than ROUTES_MAP_MAX.
export function parseIds(hash) {
  const query = String(hash || "").split("?")[1] || "";
  const raw = query.split("&").find((p) => p.startsWith("ids=")) || "ids=";
  const out = [];
  for (const part of raw.slice(4).split(",")) {
    let id = part.trim();
    try { id = decodeURIComponent(id); } catch { /* as typed */ }
    if (id && !out.includes(id)) out.push(id);
  }
  return { ids: out.slice(0, ROUTES_MAP_MAX), more: out.length > ROUTES_MAP_MAX };
}

// The route as the person's draft has it when the draft touches it (or made
// it), else live: what the route page shows.
async function loadRoute(id) {
  const draft = state.draft;
  const drafted = !!draft && (routesTouched(id) || !!createdChange("route", id));
  if (drafted) {
    try {
      return { route: await get(`change-sets/${enc(draft.change_set_id)}/preview/routes/${enc(id)}`), drafted: true };
    } catch { /* live, then */ }
  }
  return { route: await get(`feeds/${enc(state.feedId)}/routes/${enc(id)}`), drafted: false };
}

export async function showRoutesMap(hash = location.hash) {
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  nameHere("Routes on the map");
  // [{id, color, dashed, shown, route, drafted, error}], in the order added
  const items = [];
  let focus = null;
  let showStops = pref("routesMapStops", false);
  let fitted = false;

  // the next colour no route here has; past the palette, the colours again dashed
  const nextLook = () => {
    for (let i = 0; i < COLORS.length * 2; i++) {
      const look = { color: COLORS[i % COLORS.length], dashed: i >= COLORS.length };
      if (!items.some((it) => it.color === look.color && it.dashed === look.dashed)) return look;
    }
    return { color: COLORS[items.length % COLORS.length], dashed: true };
  };

  const sync = () => {
    const href = routesMapHref(items.map((it) => it.id));
    if (location.hash !== href) {
      history.replaceState(null, "", href);
      // the router and the trail follow the address
      window.dispatchEvent(new Event("hash:replaced"));
      nameHere("Routes on the map", href);
    }
  };

  const draw = ({ fit = false } = {}) => {
    const list = items.filter((it) => it.shown && it.route).map((it) => ({ key: it.id, route: it.route, color: it.color, dashed: it.dashed }));
    map.showRoutes(list, { focus: focus && list.some((x) => x.key === focus) ? focus : null, stops: showStops, fit });
  };

  // ---- the panel
  const list = h("ul.list.routes-map-list", { "data-routes-map": "" });
  const count = h("p.hint", { "aria-live": "polite" });
  const search = h("input", {
    type: "search", id: "routes-map-search", placeholder: "Route number or name, e.g. 21G", autocomplete: "off",
    "aria-label": "Add a route", "aria-controls": "routes-map-results",
  });
  const results = h("ul.list.routes-map-results", { id: "routes-map-results", hidden: true });
  const stopsBox = h("input", { type: "checkbox", id: "routes-map-stops", checked: showStops });
  stopsBox.addEventListener("change", () => { showStops = stopsBox.checked; setPref("routesMapStops", showStops); draw(); });

  const row = (it) => {
    const r = it.route;
    const swatch = h(it.dashed ? "span.route-swatch.dashed" : "span.route-swatch", { style: `--swatch:${it.color}`, "aria-hidden": "true" });
    const shown = h("input", { type: "checkbox", checked: it.shown, "aria-label": `Show route ${r ? r.short_name || it.id : it.id}` });
    shown.addEventListener("change", () => { it.shown = shown.checked; draw(); });
    const line = !r ? null : r.encoded_polyline
      ? `map line ${SOURCE[r.polyline_source] || "saved"}`
      : "no map line: dashed through its stops";
    const served = r ? (r.rows || []).filter((x) => !["ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"].includes(x.stop_type)).length : 0;
    const li = h("li.list-item.routes-map-item", { "data-route": it.id, "data-focus": String(focus === it.id) },
      h("span.routes-map-pick", shown, swatch),
      h("span.key", r ? r.short_name || it.id : it.id),
      r ? h("a", { href: `#/route/${enc(it.id)}` }, r.long_name || `Route ${it.id}`)
        : h("span", { class: it.error ? "routes-map-error" : "hint" }, it.error || "Loading…"),
      h("span.item-end",
        r ? routeStatusTag(r) : null,
        r ? h("button.btn.quiet.small", { type: "button", title: "Zoom the map to this route", on: { click: () => { focus = it.id; draw(); map.fitRoutes([{ route: r }]); refresh(); } } }, "Zoom") : null,
        h("button.btn.quiet.small", { type: "button", "aria-label": `Remove route ${r ? r.short_name || it.id : it.id}`, on: { click: () => remove(it.id) } }, "Remove")),
      r ? h("span.sub", [`Route id ${it.id}`, plural(served, "stop"), line, it.drafted ? "as in your draft" : null].filter(Boolean).join(" · ")) : null);
    // pointing at a route picks it out on the map
    const on = () => { if (focus !== it.id) { focus = it.id; draw(); markFocus(); } };
    li.addEventListener("mouseenter", on);
    li.addEventListener("focusin", on);
    return li;
  };
  const markFocus = () => list.querySelectorAll("[data-route]").forEach((el) => el.setAttribute("data-focus", String(el.dataset.route === focus)));
  list.addEventListener("mouseleave", () => { if (focus != null) { focus = null; draw(); markFocus(); } });

  const refresh = () => {
    clear(list, items.length ? items.map(row) : h("li.empty", "No routes yet. Add one above."));
    const missing = items.filter((it) => it.error).length;
    count.textContent = items.length
      ? `${plural(items.length, "route")} of at most ${ROUTES_MAP_MAX}${missing ? `; ${missing} could not be loaded` : ""}.`
      : "";
    search.disabled = items.length >= ROUTES_MAP_MAX;
    search.placeholder = search.disabled ? `At most ${ROUTES_MAP_MAX} routes: remove one to add another` : "Route number or name, e.g. 21G";
    markFocus();
  };

  const add = async (id, { fit = true } = {}) => {
    if (!id || items.some((it) => it.id === id)) return;
    if (items.length >= ROUTES_MAP_MAX) { toast(`At most ${ROUTES_MAP_MAX} routes at once.`, "error"); return; }
    const it = { id, ...nextLook(), shown: true, route: null, drafted: false, error: null };
    items.push(it);
    sync();
    refresh();
    try {
      Object.assign(it, await loadRoute(id));
    } catch (e) {
      it.error = e.status === 404 ? `No route ${id} in this feed.` : e.message;
    }
    if (!items.includes(it) || !panel().contains(list)) return;
    refresh();
    // the first routes frame the view; later ones are added to it as it is
    draw({ fit: fit && !fitted });
    if (fit && it.route) fitted = true;
  };
  const remove = (id) => {
    const i = items.findIndex((it) => it.id === id);
    if (i < 0) return;
    items.splice(i, 1);
    if (focus === id) focus = null;
    sync();
    refresh();
    draw();
  };

  // ---- adding a route by number or name
  let found = [], active = -1, seq = 0;
  const closeResults = () => { results.hidden = true; found = []; active = -1; };
  const choose = (r) => { closeResults(); search.value = ""; add(r.route_id); search.focus(); };
  const showResults = () => {
    clear(results, found.length
      ? found.map((r, i) => h("li.list-item.routes-map-result", {
          role: "option", "aria-selected": String(i === active), "data-route": r.route_id,
          on: { mousedown: (ev) => { ev.preventDefault(); choose(r); } },
        },
        h("span.key", r.short_name || r.route_id),
        h("span", r.long_name || `Route ${r.route_id}`),
        items.some((it) => it.id === r.route_id) ? h("span.hint", "on the map") : routeStatusTag(r),
        h("span.sub", `Route id ${r.route_id}, ${plural(r.stop_count || 0, "stop")}${r.has_polyline ? "" : ", no map line"}`)))
      : h("li.empty", `No route matches "${search.value.trim()}".`));
    results.hidden = false;
  };
  const find = debounce(async () => {
    const q = search.value.trim();
    if (q.length < 1) return closeResults();
    const mine = ++seq;
    try {
      const page = await get(`feeds/${enc(state.feedId)}/routes?q=${enc(q)}&limit=12`);
      if (mine !== seq) return;
      found = page.items || [];
      active = found.findIndex((r) => !items.some((it) => it.id === r.route_id));
      showResults();
    } catch (e) {
      if (mine === seq) toast(e.message, "error");
    }
  }, 200);
  search.addEventListener("input", find);
  search.addEventListener("blur", () => setTimeout(closeResults, 150));
  search.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") { closeResults(); return; }
    if (results.hidden || !found.length) return;
    if (ev.key === "ArrowDown") { ev.preventDefault(); active = Math.min(found.length - 1, active + 1); showResults(); }
    else if (ev.key === "ArrowUp") { ev.preventDefault(); active = Math.max(0, active - 1); showResults(); }
    else if (ev.key === "Enter" && found[active]) { ev.preventDefault(); choose(found[active]); }
  });

  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/" }, "Back to the map"),
      h("h1", "Routes on the map"),
      h("p.hint", "Each route's map line in its own colour, with its number where it starts. A route with no map line is drawn dashed through its stops. Point at a route in the list to pick it out on the map. The address keeps the routes, so this view can be bookmarked or shared."),
      h("label.field", { for: "routes-map-search" }, h("span", "Add a route"), search),
      results,
      h("div.btn-row",
        h("button.btn.secondary.small", { type: "button", on: { click: () => map.fitRoutes(items.filter((it) => it.shown && it.route)) } }, "Fit all on the map"),
        h("button.btn.quiet.small", { type: "button", on: { click: () => { items.length = 0; focus = null; sync(); refresh(); draw(); } } }, "Remove all")),
      h("label.check", { for: "routes-map-stops" }, stopsBox, " Show their stops"),
      count),
    h("section.section", list));
  refresh();
  draw();
  const { ids, more } = parseIds(hash);
  if (more) toast(`Showing the first ${ROUTES_MAP_MAX} routes of the address.`);
  if (!ids.length) { search.focus(); return; }
  // in the order given, all at once; framed together once they are all here
  await Promise.all(ids.map((id) => add(id, { fit: false })));
  if (!panel().contains(list)) return;
  fitted = items.some((it) => it.route);
  draw({ fit: fitted });
}
