// Browsing: the search box, the home panel, and the stop and route panels.
import { get, enc, ApiError } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, debounce, fmtCoord, fmtMetres, fmtDate, plural, STOP_TYPE_LABEL, STATUS_LABEL, groupStages, toast } from "./util.js";
import * as map from "./map.js";
import { existingChange, createdChange } from "./drafts.js";
import { editStop, editRouteRows, editRouteDetails, editStation, deleteStop, dissolveStation } from "./editors.js";
import { showDraftStop } from "./create.js";

const panel = () => document.getElementById("panel");

// ------------------------------------------------------------------ search
export function initSearch() {
  const input = document.getElementById("search-input");
  const box = document.getElementById("search-results");
  let items = [], active = -1, seq = 0;

  const close = () => { box.hidden = true; input.setAttribute("aria-expanded", "false"); active = -1; };
  const choose = (it) => { close(); input.blur(); location.hash = it.href; };
  const highlight = () => {
    box.querySelectorAll(".search-item").forEach((el, i) => el.setAttribute("aria-selected", String(i === active)));
    const el = box.querySelectorAll(".search-item")[active];
    if (el) el.scrollIntoView({ block: "nearest" });
  };

  const run = debounce(async () => {
    const q = input.value.trim();
    if (q.length < 2 || !state.feedId) return close();
    const mine = ++seq;
    try {
      const [routes, stops] = await Promise.all([
        get(`feeds/${enc(state.feedId)}/routes?q=${enc(q)}&limit=8`),
        get(`feeds/${enc(state.feedId)}/stops?q=${enc(q)}&limit=8`),
      ]);
      if (mine !== seq) return;
      items = [
        ...routes.items.map((r) => ({ group: "Routes", key: r.short_name || r.route_id, text: r.long_name || "", sub: `Route id ${r.route_id}, ${r.stop_count} stops`, href: `#/route/${enc(r.route_id)}` })),
        ...stops.items.map((s) => ({ group: "Stops", key: s.location_type === 1 ? "Station" : "Stop", text: s.name, sub: `${s.stop_id}, ${s.route_count} route${s.route_count === 1 ? "" : "s"}`, href: `#/stop/${enc(s.stop_id)}` })),
      ];
      active = items.length ? 0 : -1;
      let lastGroup = null;
      clear(box, items.length ? items.map((it, i) => {
        const header = it.group !== lastGroup ? h("div.search-group", it.group) : null;
        lastGroup = it.group;
        return [header, h("div.search-item", {
          role: "option", id: `search-opt-${i}`, "aria-selected": String(i === active),
          on: { mousedown: (ev) => { ev.preventDefault(); choose(it); } },
        }, h("span.key", it.key), h("span", it.text), h("span.sub", it.sub))];
      }) : h("p.search-empty", `Nothing matches "${q}". Try a route number like 45B, a stop name, or a stop id.`));
      box.hidden = false;
      input.setAttribute("aria-expanded", "true");
    } catch (e) {
      if (mine === seq) toast(e.message, "error");
    }
  }, 220);

  input.addEventListener("input", run);
  window.addEventListener("hashchange", close);
  input.addEventListener("focus", () => { if (input.value.trim().length >= 2) run(); });
  input.addEventListener("blur", () => setTimeout(close, 120));
  input.addEventListener("keydown", (ev) => {
    if (box.hidden) return;
    if (ev.key === "ArrowDown") { ev.preventDefault(); active = Math.min(items.length - 1, active + 1); highlight(); }
    else if (ev.key === "ArrowUp") { ev.preventDefault(); active = Math.max(0, active - 1); highlight(); }
    else if (ev.key === "Enter" && items[active]) { ev.preventDefault(); choose(items[active]); }
    else if (ev.key === "Escape") close();
  });
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "/" && document.activeElement === document.body) { ev.preventDefault(); input.focus(); }
  });
}

// ------------------------------------------------------------------ home
export async function showHome() {
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  const p = clear(panel(),
    h("section.section",
      h("h1", "Find a stop or route"),
      h("p", "Search above by stop name, stop id or route number, or zoom the map in to see stops and click one."),
      can("editor")
        ? h("p.hint", "To change something, open it and choose Edit. To add a stop, route or station, use New at the top. Edits collect in a draft; nothing changes for passengers until another person approves and commits it.")
        : h("p.hint", "You can look at everything. Ask an admin for the editor role to make changes."),
    ));
  const coordinates = h("section.section", { hidden: true });
  p.appendChild(coordinates);
  get(`feeds/${enc(state.feedId)}/position-reviews/summary`).then((c) => {
    if (!c.pending) return;
    coordinates.hidden = false;
    clear(coordinates, h("h2", "Coordinates to review"),
      h("p", `${plural(c.pending, "stop")} may be in the wrong place and ${c.pending === 1 ? "is" : "are"} waiting for someone to check ${c.pending === 1 ? "it" : "them"}.`),
      h("div.btn-row", h("a.btn.secondary", { href: "#/coordinates" }, "Review coordinates")));
  }).catch(() => {});
  const review = h("section.section", { hidden: true });
  p.appendChild(review);
  get(`feeds/${enc(state.feedId)}/station-proposals/summary`).then((c) => {
    if (!c.pending) return;
    review.hidden = false;
    clear(review, h("h2", "Stations to review"),
      h("p", `${plural(c.pending, "suggested station")} ${c.pending === 1 ? "is" : "are"} waiting for someone to check ${c.pending === 1 ? "it" : "them"}.`),
      h("div.btn-row", h("a.btn.secondary", { href: "#/stations" }, "Review stations")));
  }).catch(() => {});
  const work = h("section.section", h("h2", "Drafts in progress"), h("p.empty", "Loading…"));
  p.appendChild(work);
  try {
    const page = await get(`feeds/${enc(state.feedId)}/change-sets?status=draft,submitted,approved&limit=8`);
    clear(work, h("div.section-head", h("h2", "Drafts in progress"), h("a.crumb", { href: "#/drafts" }, "All drafts")),
      page.items.length
        ? h("ul.list", page.items.map((cs) => h("li.list-item",
            h("span", { class: `chip ${cs.status}` }, STATUS_LABEL[cs.status]),
            h("a", { href: `#/drafts/${enc(cs.change_set_id)}` }, cs.title),
            h("span.hint", String(cs.change_count)),
            h("span.sub", `${cs.created_by_email}, updated ${fmtDate(cs.updated_at)}`))))
        : h("p.empty", "No drafts are open."));
  } catch (e) {
    clear(work, h("p.notice.error", e.message));
  }
}

// ------------------------------------------------------------------ stop
export async function showStop(stopId) {
  map.endModes();
  map.clearRoute();
  clear(panel(), h("section.section", h("p.empty", "Loading stop…")));
  let s;
  try {
    s = await get(`feeds/${enc(state.feedId)}/stops/${enc(stopId)}`);
  } catch (e) {
    const created = e instanceof ApiError && e.status === 404 ? createdChange("stop", stopId) : null;
    if (created) return showDraftStop(created);
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/" }, "Back to search"), h("p.notice.error", e.message)));
  }
  map.focusStop(s);
  const isStation = s.location_type === 1;
  const pending = existingChange(isStation ? "station" : "stop", s.stop_id);
  const merges = state.draft ? state.draft.changes.filter((c) => c.entity === "stop" && c.op === "merge"
    && (c.entity_key === s.stop_id || (c.after && c.after.into_stop_id === s.stop_id))) : [];
  const editor = can("editor") && !s.deleted;

  const actions = editor ? h("div.btn-row",
    isStation
      ? [h("button.btn", { type: "button", on: { click: () => editStation(s) } }, "Edit station"),
         h("button.btn.danger", { type: "button", on: { click: () => dissolveStation(s) } }, "Dissolve station")]
      : [h("button.btn", { type: "button", on: { click: () => editStop(s) } }, "Edit stop"),
         h("a.btn.secondary", { href: `#/merge/${enc(s.stop_id)}` }, "Merge with a duplicate…"),
         s.parent_station ? null : h("button.btn.secondary", { type: "button", on: { click: () => editStation(null, [s]) } }, "Club into a station"),
         s.route_count === 0 ? h("button.btn.danger", { type: "button", on: { click: () => deleteStop(s) } }, "Delete stop") : null],
  ) : null;

  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/" }, "Back to search"),
      h("div.title-block",
        h("h1", s.name),
        h("p.ids", `${isStation ? "Station" : "Stop"} ${s.stop_id}${s.stop_code && s.stop_code !== s.stop_id ? `, code ${s.stop_code}` : ""}`)),
      pending ? h("p.notice.draft", `Your draft "${state.draft.title}" changes this ${isStation ? "station" : "stop"}. The details below are what is live now.`) : null,
      merges.map((c) => h("p.notice.draft", c.entity_key === s.stop_id
        ? `Your draft "${state.draft.title}" merges this stop into ${c.after.into_stop_id}. It goes away when the draft is committed.`
        : `Your draft "${state.draft.title}" merges stop ${c.entity_key} into this stop.`)),
      s.deleted ? h("p.notice", "This stop has been removed from the feed.") : null,
      s.parent ? h("p.notice", "Part of station ", h("a", { href: `#/stop/${enc(s.parent.stop_id)}` }, s.parent.name), ".") : null,
      h("dl.facts",
        h("dt", "Position"), h("dd", `${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`),
        s.position_source ? [h("dt", "Position from"), h("dd", s.position_source)] : null,
        s.platform_code ? [h("dt", "Platform"), h("dd", s.platform_code)] : null,
        s.cluster_id ? [h("dt", "Cluster"), h("dd", s.cluster_id)] : null,
        s.regional_name ? [h("dt", "Tamil name"), h("dd", s.regional_name)] : null,
      ),
      actions,
    ),
    isStation ? h("section.section",
      h("h2", `Stops in this station (${s.children.length})`),
      s.children.length ? h("ul.list", s.children.map((c) => h("li.list-item",
        h("span.key", c.platform_code || ""),
        h("a", { href: `#/stop/${enc(c.stop_id)}` }, c.name),
        h("span.hint", `${c.route_count} route${c.route_count === 1 ? "" : "s"}`),
        h("span.sub", c.stop_id)))) : h("p.empty", "This station has no stops.")) : null,
    !isStation ? h("section.section",
      h("h2", `Routes stopping here (${s.routes.length})`),
      s.routes.length ? h("ul.list", s.routes.map((r) => h("li.list-item",
        h("span.key", r.short_name || r.route_id),
        h("a", { href: `#/route/${enc(r.route_id)}` }, r.long_name || `Route ${r.route_id}`),
        h("span.hint", `stop ${r.sequence}`),
        h("span.sub", `${STOP_TYPE_LABEL[r.stop_type] || r.stop_type} in stage ${r.stage_no}, route id ${r.route_id}`)))) : h("p.empty", "No route uses this stop.")) : null,
    h("section.section",
      h("h2", "Other stops within 60 m"),
      h("p.hint", "Two stops close together are often the two sides of the road: club them into a station. Merge them only if they are the same kerb entered twice."),
      s.nearby.length ? h("ul.list", s.nearby.map((n) => h("li.list-item",
        h("span.key", fmtMetres(n.distance_m)),
        h("a", { href: `#/stop/${enc(n.stop_id)}` }, n.name),
        h("span.item-end",
          h("span.hint", n.location_type === 1 ? "station" : plural(n.route_count, "route")),
          editor && !isStation && n.location_type === 0
            ? h("a.btn.quiet.small", { href: `#/merge/${enc(s.stop_id)}?with=${enc(n.stop_id)}`, "aria-label": `Merge ${s.name} with ${n.name} (${n.stop_id})` }, "Merge…")
            : null),
        h("span.sub", `${n.stop_id}${n.parent_station ? `, in station ${n.parent_station}` : ""}`)))) : h("p.empty", "None."),
    ),
  );
}

// ------------------------------------------------------------------ route
export async function showRoute(routeId, { preview = false } = {}) {
  map.endModes();
  clear(panel(), h("section.section", h("p.empty", "Loading route…")));
  const created = createdChange("route", routeId);
  const draftChange = existingChange("route_stops", routeId) || existingChange("route", routeId) || created;
  const usePreview = !!((preview || created) && state.draft && draftChange);
  let r;
  try {
    r = await get(usePreview
      ? `change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(routeId)}`
      : `feeds/${enc(state.feedId)}/routes/${enc(routeId)}`);
  } catch (e) {
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/" }, "Back to search"), h("p.notice.error", e.message)));
  }
  map.clearFocus();
  map.showRoute(r);

  const served = r.rows.filter((x) => x.stop_type !== "ROUTE CORRECTION");
  // GIMS serves a route only once the nightly GTFS build gives it trips
  const notInFeed = created || !(r.provenance && r.provenance.in_shipped_feed === true);
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/" }, "Back to search"),
      h("div.title-block",
        h("h1", r.short_name || `Route ${r.route_id}`),
        h("p", r.long_name || ""),
        h("p.ids", `Route id ${r.route_id}, ${plural(r.stop_count, "stop")}, ${plural(r.rows.length ? groupStages(served).length : 0, "fare stage")}`)),
      created ? h("div.notice.draft",
        h("p", h("strong", `New route in your draft "${state.draft.title}".`)),
        h("p", "It is saved when the draft is approved by someone else and committed. Passengers see it only after the nightly GTFS build gives it trips from the MTC schedule.")) : null,
      draftChange && !created ? h("div.notice.draft",
        h("p", `Your draft "${state.draft.title}" changes this route.`),
        h("div.btn-row",
          usePreview
            ? h("button.btn.secondary.small", { type: "button", on: { click: () => showRoute(routeId) } }, "Show what is live now")
            : h("button.btn.secondary.small", { type: "button", on: { click: () => showRoute(routeId, { preview: true }) } }, "Show it with my draft"))) : null,
      usePreview && !created ? h("p.notice", "You are looking at this route with your draft applied. It is not live.") : null,
      !created && notInFeed ? h("p.notice", "This route is not in the published GTFS feed yet, so passengers do not see it. It appears in the live apps once the nightly GTFS build gives it trips from the MTC schedule.") : null,
      h("dl.facts",
        h("dt", "Map line"), h("dd", r.encoded_polyline ? `Saved (${r.polyline_source || "source unknown"})` : "None yet. The map joins the stops with straight dashed lines."),
        r.color ? [h("dt", "Colour"), h("dd", h("span", { style: `display:inline-block;width:14px;height:14px;border-radius:3px;vertical-align:-2px;margin-right:6px;background:${r.color}` }), r.color)] : null,
      ),
      can("editor") && (!usePreview || created) ? h("div.btn-row",
        h("button.btn", { type: "button", on: { click: () => editRouteRows(r, { created: !!created }) } }, created && !r.rows.length ? "Build the stop list" : "Edit stop list"),
        h("button.btn.secondary", { type: "button", on: { click: () => editRouteDetails(r, { created: !!created }) } }, "Edit name, colour and map line")) : null,
    ),
    h("section.section", h("h2", "Stops by fare stage"), r.rows.length ? ladder(r.rows) : h("p.empty", "No stops yet.")),
  );
}

// The read-only stage ladder.
export function ladder(rows, { onRow } = {}) {
  const stages = groupStages(rows);
  return h("div.ladder", stages.map((st) => h("div.stage",
    h("div.stage-no", { "aria-label": `Stage ${st.stage_no}` }, h("small", "stage"), String(st.stage_no)),
    h("div.stage-body",
      h("p.stage-name", st.stage_name || ""),
      h("ol.stops", st.rows.map(({ row, index }) => {
        const kind = { "NEW STOP": "new", "JUMP STOP": "jump", "ROUTE CORRECTION": "marker" }[row.stop_type] || "";
        const isMarker = row.stop_type === "ROUTE CORRECTION";
        const li = h("li.row", { class: `${kind} ${isMarker ? "" : "clickable"}`, tabindex: isMarker ? null : "0" },
          h("span.node", { "aria-hidden": "true" }),
          h("span.what",
            h("span.name", isMarker ? `Map shaping point: ${row.marker_name || row.marker_id}` : row.stop_name || row.stop_id),
            h("span.meta", isMarker ? "Bends the map line, not a stop" : `${STOP_TYPE_LABEL[row.stop_type]}, ${row.stop_id}${row.parent_station ? `, station ${row.parent_station}` : ""}`)),
          h("span.seq", String(index + 1)));
        if (!isMarker) {
          const open = () => (onRow ? onRow(row) : (location.hash = `#/stop/${enc(row.stop_id)}`));
          li.addEventListener("click", open);
          li.addEventListener("keydown", (ev) => { if (ev.key === "Enter") open(); });
        }
        return li;
      })),
    ))));
}
