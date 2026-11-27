// The routes page (docs/gtfs-editor.md section 34): every route of the feed in
// one table - filtered, sorted and paged in the browser, with the open draft
// applied - and the two tools that start from it: a route cloned into a new
// route in the draft, and routes exported as the CSV files the import page
// reads back.
import { get, post, enc, download } from "./api.js";
import { state, can, usesStages } from "./state.js";
import { h, clear, toast, modal, debounce, fmtCount, plural, routeStatusTag, ID_RE, ID_RULE } from "./util.js";
import { requireDraft, useDraft } from "./drafts.js";
import { pager } from "./importer.js";
import { routesMapHref, ROUTES_MAP_MAX } from "./routes_map.js";

const page = () => document.getElementById("page");
// rows on one page of the table; the API is asked 500 at a time, its largest page
const PAGE_ROWS = 100;
const FETCH_ROWS = 500;
const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: "base" });

const LINE_SOURCE = { osrm: "by road", gps: "from GPS", manual: "by hand", imported: "imported" };

// What the page remembers while it is open: the filter, the sort, the page and
// the ticked routes, for one feed.
const view = { feedId: null, q: "", status: "all", sort: "route", dir: 1, page: 1, selected: new Set(), items: [] };

// Sort keys: each a function of a route, compared as text (numbers in it as
// numbers) or as numbers.
const COLUMNS = [
  { key: "route", label: "Route", value: (r) => `${r.short_name || ""}\u0000${r.route_id}` },
  { key: "long_name", label: "Name", value: (r) => r.long_name || "" },
  { key: "active", label: "Status", value: (r) => (r.active === false ? 1 : 0), number: true },
  { key: "agency_id", label: "Agency", value: (r) => r.agency_id || "" },
  { key: "stop_count", label: "Stops", value: (r) => r.stop_count || 0, number: true },
  { key: "pattern_count", label: "Stop orders", value: (r) => r.pattern_count || 0, number: true },
  { key: "trip_count", label: "Trips", value: (r) => r.trip_count || 0, number: true },
  { key: "route_tag", label: "Fare tag", value: (r) => r.route_tag || "" },
  { key: "line", label: "Map line", value: (r) => (r.has_polyline ? r.polyline_source || "?" : "") },
];

// Every route, a page of the API at a time, with the open draft applied.
async function loadAll(progress) {
  const draft = state.draft ? `&change_set=${enc(state.draft.change_set_id)}` : "";
  const items = [];
  let cursor = null;
  do {
    const res = await get(`feeds/${enc(state.feedId)}/routes?limit=${FETCH_ROWS}${draft}${cursor ? `&cursor=${enc(cursor)}` : ""}`);
    items.push(...res.items);
    cursor = res.next_cursor;
    progress(items.length);
  } while (cursor);
  return items;
}

// What the open draft does to a route: makes it, or changes it.
function draftMark(routeId) {
  if (!state.draft) return null;
  const mine = state.draft.changes.filter((c) => c.entity_key === routeId
    && ["route", "route_stops", "route_stages", "pattern", "timing_profile", "route_trips", "route_variant"].includes(c.entity));
  if (!mine.length) return null;
  return mine.some((c) => c.entity === "route" && c.op === "create")
    ? h("span.chip.draft", { title: `New in draft “${state.draft.title}”` }, "new in draft")
    : h("span.chip.draft", { title: `Draft “${state.draft.title}” changes it` }, "changed in draft");
}

function matches(r, q) {
  if (!q) return true;
  return [r.route_id, r.short_name, r.long_name, r.agency_id, r.route_tag].some((v) => (v || "").toLowerCase().includes(q));
}

export async function showRoutesList() {
  if (view.feedId !== state.feedId) Object.assign(view, { feedId: state.feedId, q: "", status: "all", page: 1, selected: new Set(), items: [] });
  const feedId = state.feedId;
  const status = h("p.empty", { role: "status" }, "Loading routes…");
  clear(page(), h("div.page-inner.routes-page", h("h1", "Routes"), status));
  let items;
  try {
    items = await loadAll((n) => { status.textContent = `Loading routes… ${fmtCount(n)} so far`; });
  } catch (e) {
    return clear(page(), h("div.page-inner", h("h1", "Routes"), h("p.notice.error", e.message)));
  }
  // left while it loaded
  if (state.feedId !== feedId || !/^#\/routes\/?(\?|$)/.test(location.hash)) return;
  view.items = items;
  // a route no longer listed cannot stay ticked
  const ids = new Set(items.map((r) => r.route_id));
  view.selected = new Set([...view.selected].filter((id) => ids.has(id)));

  const search = h("input", { type: "search", id: "routes-q", autocomplete: "off", spellcheck: "false", value: view.q,
    placeholder: "Route number, name, id, agency or fare tag" });
  search.addEventListener("input", debounce(() => { view.q = search.value; view.page = 1; drawResults(); }, 200));
  const statusPick = h("select", { id: "routes-status", on: { change: (ev) => { view.status = ev.target.value; view.page = 1; drawResults(); } } },
    [["all", "Active and inactive"], ["active", "Active only"], ["inactive", "Inactive only"]].map(([v, label]) => h("option", { value: v, selected: view.status === v }, label)));
  clear(page(), h("div.page-inner.routes-page",
    h("div.page-head",
      h("div.title-block", h("h1", "Routes"),
        h("p.hint", `${plural(items.length, "route")} in this feed. Open one to see and edit it; tick routes to export them as CSV files, which the import page reads back into this feed or another.`)),
      h("div.btn-row",
        h("button.btn.secondary", { type: "button", id: "routes-export-all", on: { click: () => exportRoutes(null) } }, `Export all ${fmtCount(items.length)}`),
        can("editor") ? h("a.btn.secondary", { href: "#/import?kind=routes", id: "routes-import" }, "Import routes") : null)),
    state.draft ? h("p.notice.draft", "Shown as your draft “", state.draft.title, "” makes them: routes it adds are listed, routes it deletes are not, and its changes show. An export reads them the same way.") : null,
    h("div.filters",
      h("label.field", { for: "routes-q" }, h("span", "Find"), search),
      h("label.field", { for: "routes-status" }, h("span", "Status"), statusPick)),
    h("div#routes-results")));
  drawResults();
}

// The table and what goes with it: drawn again on every filter, sort, page and
// tick, while the filters above stay as they are.
function drawResults() {
  const box = document.getElementById("routes-results");
  if (!box) return;
  const all = view.items;
  const q = view.q.trim().toLowerCase();
  const shown = all.filter((r) => matches(r, q)
    && (view.status === "all" || (view.status === "inactive") === (r.active === false)));
  const col = COLUMNS.find((c) => c.key === view.sort) || COLUMNS[0];
  shown.sort((a, b) => {
    const [x, y] = [col.value(a), col.value(b)];
    const d = col.number ? x - y : collator.compare(x, y);
    return (d || collator.compare(a.route_id, b.route_id)) * view.dir;
  });
  const pages = Math.max(1, Math.ceil(shown.length / PAGE_ROWS));
  view.page = Math.min(view.page, pages);
  const slice = shown.slice((view.page - 1) * PAGE_ROWS, view.page * PAGE_ROWS);
  const editor = can("editor");

  const sortHead = (c) => {
    const on = view.sort === c.key;
    return h("th", { scope: "col", "aria-sort": on ? (view.dir > 0 ? "ascending" : "descending") : null, class: c.number ? "num" : null },
      h("button.sort", { type: "button", "data-sort": c.key, on: { click: () => {
        if (view.sort === c.key) view.dir = -view.dir;
        else Object.assign(view, { sort: c.key, dir: 1 });
        drawResults();
      } } }, c.label, on ? (view.dir > 0 ? " ▲" : " ▼") : ""));
  };
  const pageIds = slice.map((r) => r.route_id);
  const allOnPage = pageIds.length > 0 && pageIds.every((id) => view.selected.has(id));
  const pageBox = h("input", { type: "checkbox", id: "routes-tick-page", "aria-label": "Select the routes on this page", checked: allOnPage,
    on: { change: (ev) => { pageIds.forEach((id) => (ev.target.checked ? view.selected.add(id) : view.selected.delete(id))); drawResults(); } } });
  pageBox.indeterminate = !allOnPage && pageIds.some((id) => view.selected.has(id));

  const row = (r) => h("tr", { "data-route": r.route_id, class: view.selected.has(r.route_id) ? "selected" : null },
    h("td.tick", h("input", { type: "checkbox", "aria-label": `Select route ${r.short_name || r.route_id}`, checked: view.selected.has(r.route_id),
      on: { change: (ev) => { if (ev.target.checked) view.selected.add(r.route_id); else view.selected.delete(r.route_id); drawResults(); } } })),
    h("td", h("a", { href: `#/route/${enc(r.route_id)}` }, h("strong", r.short_name || r.route_id)), " ", draftMark(r.route_id),
      h("span.sub", r.route_id)),
    h("td", r.long_name || ""),
    h("td", routeStatusTag(r, { both: true })),
    h("td", r.agency_id || ""),
    h("td.num", fmtCount(r.stop_count || 0)),
    h("td.num", fmtCount(r.pattern_count || 0)),
    h("td.num", fmtCount(r.trip_count || 0)),
    h("td", r.route_tag || ""),
    h("td", r.has_polyline ? LINE_SOURCE[r.polyline_source] || r.polyline_source || "saved" : h("span.hint", "none")),
    editor ? h("td", h("button.btn.quiet.small", { type: "button", "data-clone": r.route_id, on: { click: () => cloneRoute(r) } }, "Clone")) : null);

  const picked = [...view.selected];
  const matching = shown.map((r) => r.route_id);
  const unpicked = matching.filter((id) => !view.selected.has(id)).length;
  clear(box,
    h("div.routes-count",
      h("p.hint", { role: "status" }, shown.length === all.length ? plural(all.length, "route") : `${fmtCount(shown.length)} of ${plural(all.length, "route")} match`),
      unpicked ? h("button.btn.quiet.small", { type: "button", id: "routes-tick-matching", on: { click: () => { matching.forEach((id) => view.selected.add(id)); drawResults(); } } },
        `Select all ${fmtCount(matching.length)}${q || view.status !== "all" ? " that match" : ""}`) : null),
    shown.length
      ? h("div.table-wrap", h("table.routes-table",
          h("thead", h("tr", h("th.tick", pageBox), COLUMNS.map(sortHead), editor ? h("th", { scope: "col" }, h("span.visually-hidden", "Clone")) : null)),
          h("tbody", slice.map(row))))
      : h("p.empty", all.length ? "No route matches." : "This feed has no routes yet."),
    pages > 1 ? pager(view.page, pages, (p) => { view.page = p; drawResults(); box.scrollIntoView({ block: "start" }); }) : null,
    picked.length ? h("div.batch-bar.routes-bar", { role: "status" },
      h("span", h("strong", `${plural(picked.length, "route")} selected`)),
      h("div.btn-row",
        h("button.btn", { type: "button", id: "routes-export-selected", on: { click: () => exportRoutes(picked) } }, `Export ${plural(picked.length, "route")}`),
        picked.length <= ROUTES_MAP_MAX ? h("a.btn.secondary", { href: routesMapHref(picked) }, "Show them on the map") : null,
        h("button.btn.quiet", { type: "button", on: { click: () => { view.selected.clear(); drawResults(); } } }, "Clear the selection"))) : null);
}

// ------------------------------------------------------------------ export
// Routes (or every route, `ids` null) as a zip of CSV files: routes, their
// stop lists or stages, timings, services and trips, each the file the import
// page takes for that kind. Asks whether the rows add the routes elsewhere or
// update them here.
export async function exportRoutes(ids) {
  const n = ids ? ids.length : view.items.length;
  const action = await modal(`Export ${plural(n, "route")}`, (close) => {
    const choice = (value, title, what, checked) => h("label.radio-card", { for: `export-${value}` },
      h("input", { type: "radio", name: "export-action", id: `export-${value}`, value, checked }),
      h("span.radio-card-body", h("span.radio-card-title", title), h("span.hint", what)));
    const form = h("form", { style: "display:grid;gap:14px", on: { submit: (ev) => {
      ev.preventDefault();
      close(form.querySelector('input[name="export-action"]:checked').value);
    } } },
      h("p", `A zip of CSV files: routes.csv, ${usesStages() ? "route_stages.csv (the stages each route runs through)" : "route_stops.csv (each stop list)"}, timing_profiles.csv, services.csv and route_trips.csv, with a README saying the order to import them in on the import page.`),
      h("div.radio-cards", { role: "radiogroup", "aria-label": "What the rows say" },
        choice("add", "To add them elsewhere", "Every row says add: import into another feed, or here under new route ids.", true),
        choice("update", "To change them here", "Every row says update: edit the files, then import them back into this feed.", false)),
      state.draft ? h("p.hint", `As your draft “${state.draft.title}” makes them.`) : null,
      h("div.btn-row",
        h("button.btn", { type: "submit", id: "export-download" }, "Download"),
        h("button.btn.secondary", { type: "button", on: { click: () => close(null) } }, "Cancel")));
    return form;
  });
  if (!action) return;
  const draft = state.draft ? `?change_set=${enc(state.draft.change_set_id)}` : "";
  try {
    const file = await download("POST", `feeds/${enc(state.feedId)}/routes:export${draft}`, ids ? { route_ids: ids, action } : { action });
    const url = URL.createObjectURL(file.blob);
    const a = h("a", { href: url, download: file.name, style: "display:none" });
    document.body.appendChild(a);
    a.click();
    a.remove();
    // let the download start before the blob goes
    setTimeout(() => URL.revokeObjectURL(url), 10000);
    toast(`Exported ${plural(n, "route")} as ${file.name}.`);
  } catch (e) {
    toast(e.message, "error");
  }
}

// ------------------------------------------------------------------ clone
// A new route made from `route` as the open draft has it: its fields, map
// line, stop lists (or stages), timings and trips, added to the draft as the
// changes that build it. Asks for the new id and names; opens the new route.
export async function cloneRoute(route) {
  if (!can("editor")) return null;
  const draft = await requireDraft("A clone is a new route in a draft. Nothing changes for passengers until someone else approves the draft and it is committed.");
  if (!draft) return null;
  const name = route.short_name || route.route_id;
  const made = await modal(`Clone route ${name}`, (close) => {
    const id = h("input", { type: "text", id: "clone-route-id", autocomplete: "off", spellcheck: "false", maxlength: "64", placeholder: `For example ${route.route_id}-2` });
    const short = h("input", { type: "text", id: "clone-short-name", value: route.short_name || "" });
    const long = h("input", { type: "text", id: "clone-long-name", value: route.long_name || "" });
    const err = h("p.notice.error", { role: "alert", hidden: true });
    const submit = h("button.btn", { type: "submit", id: "clone-submit" }, "Add the clone to the draft");
    const fail = (message, field) => { err.hidden = false; err.textContent = message; if (field) field.focus(); };
    const go = async (ev) => {
      ev.preventDefault();
      const to = id.value.trim();
      if (!ID_RE.test(to)) return fail(`The new route id: ${ID_RULE}`, id);
      if (!short.value.trim()) return fail("Give the new route the number passengers see.", short);
      submit.disabled = true;
      try {
        close(await post(`feeds/${enc(state.feedId)}/routes/${enc(route.route_id)}:clone?change_set=${enc(draft.change_set_id)}`,
          { route_id: to, short_name: short.value.trim(), long_name: long.value.trim() || null }));
      } catch (e) {
        submit.disabled = false;
        fail(e.message, e.code === "route_exists" || e.code === "invalid_change" ? id : null);
      }
    };
    return h("form", { style: "display:grid;gap:12px", on: { submit: go } },
      h("p", `A new route with everything route ${name} has${state.draft ? " as your draft has it" : ""}: its colours, fare tag, map line, ${usesStages() ? "stages" : "stop lists"}, timings and trips (with new trip ids). It goes into draft “${draft.title}”.`),
      h("label.field", { for: "clone-route-id" }, h("span", "New route id"), id, h("span.hint", ID_RULE)),
      h("label.field", { for: "clone-short-name" }, h("span", "Route number passengers see"), short),
      h("label.field", { for: "clone-long-name" }, h("span", "Route name (optional)"), long),
      err,
      h("div.btn-row", submit, h("button.btn.secondary", { type: "button", on: { click: () => close(null) } }, "Cancel")));
  });
  if (!made) return null;
  if (made.change_set && made.change_set.change_set_id) useDraft(made.change_set);
  const c = made.copied || {};
  const parts = [
    c.stages ? plural(c.stages, "stage") : c.stop_lists ? plural(c.stop_lists, "stop list") : null,
    c.profiles ? plural(c.profiles, "timing") : null,
    c.trips ? plural(c.trips, "trip") : null,
    c.map_line ? "its map line" : null,
  ].filter(Boolean);
  const left = made.left_out || {};
  toast(`Route ${made.route_id} is in draft “${draft.title}”${parts.length ? ` with ${parts.join(", ")}` : ""}.${(left.patterns || []).length
    ? ` Left out: ${plural(left.patterns.length, "stop order")} a feed built from stages does not serve, with ${plural(left.trips || 0, "trip")}.` : ""}`);
  location.hash = `#/route/${enc(made.route_id)}`;
  return made;
}
