// Browsing: the search box, the home panel, and the stop and route panels.
import { get, enc, ApiError } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, debounce, fmtCoord, fmtMetres, fmtDate, plural, STOP_TYPE_LABEL, STATUS_LABEL, groupStages, diffRows, toast, stopDetailWords } from "./util.js";
import * as map from "./map.js";
import { createdChange } from "./drafts.js";
import { editStop, editRouteRows, editRouteDetails, editStation, deleteStop, dissolveStation } from "./editors.js";
import { showDraftStop, showDraftStation } from "./create.js";
import { applyToStop, applyToRoute, pendingNotice, withLive, touchedStops, routesTouched, draftTitle } from "./overlay.js";
import { stationNames, foldedList, platformsNote, stopContext, routeContext } from "./context.js";
import { nameHere, parent as trailParent, startFresh } from "./trail.js";

const panel = () => document.getElementById("panel");

// ------------------------------------------------------------------ search
export function initSearch() {
  const input = document.getElementById("search-input");
  const box = document.getElementById("search-results");
  let items = [], active = -1, seq = 0;

  const close = () => { box.hidden = true; input.setAttribute("aria-expanded", "false"); active = -1; };
  // a search result starts a new trail, as the top bar does
  const choose = (it) => { close(); input.blur(); startFresh(); location.hash = it.href; };
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
// The way back, for a panel opened from nowhere: once the trail has somewhere to
// go back to, its Back control is the one to use.
const backLink = () => (trailParent() ? null : h("a.crumb", { href: "#/" }, "Back to search"));

export async function showStop(stopId) {
  map.endModes();
  map.clearRoute();
  clear(panel(), h("section.section", h("p.empty", "Loading stop…")));
  let live;
  try {
    live = await get(`feeds/${enc(state.feedId)}/stops/${enc(stopId)}`);
  } catch (e) {
    // not live yet, but the draft creates it: show what it will be
    const missing = e instanceof ApiError && e.status === 404;
    const created = missing ? createdChange("stop", stopId) : null;
    if (created) return showDraftStop(created);
    const station = missing ? createdChange("station", stopId) : null;
    if (station) return showDraftStation(station);
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/" }, "Back to search"), h("p.notice.error", e.message)));
  }
  // `s` is the stop as the active draft leaves it; `live` is what passengers have
  const o = applyToStop(live);
  const s = o.row;
  nameHere(s.name);
  map.focusStop(s);
  if (o.moved) map.showDrafted([{ lat: s.lat, lon: s.lon, label: `Pending in draft “${draftTitle()}”` }], { live });
  const isStation = s.location_type === 1;
  const what = isStation ? "station" : "stop";
  const editor = can("editor") && !live.deleted;
  const names = new Map();        // station names, looked up once for this panel
  const shown = (k, fmt = (v) => v) => (o.changed.has(k) ? withLive(fmt(s[k]) || "none", fmt(live[k]) || "none") : fmt(s[k]));

  const actions = editor ? h("div.btn-row",
    isStation
      ? [h("button.btn", { type: "button", on: { click: () => editStation(live) } }, "Edit station"),
         h("button.btn.danger", { type: "button", on: { click: () => dissolveStation(live) } }, "Dissolve station")]
      : [h("button.btn", { type: "button", on: { click: () => editStop(live) } }, "Edit stop"),
         h("a.btn.secondary", { href: `#/merge/${enc(live.stop_id)}` }, "Merge with a duplicate…"),
         live.parent_station ? null : h("button.btn.secondary", { type: "button", on: { click: () => editStation(null, [live]) } }, "Club into a station"),
         live.route_count === 0 ? h("button.btn.danger", { type: "button", on: { click: () => deleteStop(live) } }, "Delete stop") : null],
  ) : null;

  // a station's stops as the draft leaves them: who joins, who leaves
  const draftedMembers = isStation ? (o.actions.filter((a) => a.kind === "members").pop() || {}).members : null;
  const members = !isStation ? [] : !draftedMembers ? s.children : [
    ...draftedMembers.map((m) => {
      const c = s.children.find((x) => x.stop_id === m.stop_id);
      return c ? { ...c, platform_code: m.labelled ? m.platform_code : c.platform_code } : { stop_id: m.stop_id, name: m.stop_id, platform_code: m.platform_code, joins: true };
    }),
    ...s.children.filter((c) => !draftedMembers.some((m) => m.stop_id === c.stop_id)).map((c) => ({ ...c, leaves: true }))];
  const nearbyBox = h("div", h("p.empty", "Loading…"));
  const stationLine = () => {
    if (o.changed.has("parent_station")) {
      return h("p.notice.draft", s.parent_station ? ["Joins station ", h("a", { href: `#/stop/${enc(s.parent_station)}` }, s.parent_station), " in the draft"] : "Leaves its station in the draft",
        live.parent ? [" ", h("span.live-value", h("span.live-tag", "live: "), "part of ", h("a", { href: `#/stop/${enc(live.parent.stop_id)}` }, live.parent.name))] : " (live: in no station).");
    }
    return live.parent ? h("p.notice", "Part of station ", h("a", { href: `#/stop/${enc(live.parent.stop_id)}` }, live.parent.name), ".") : null;
  };

  clear(panel(),
    h("section.section",
      backLink(),
      h("div.title-block",
        h("h1", s.name, o.changed.has("name") ? h("span.live-value", h("span.live-tag", "live: "), h("s", live.name)) : null),
        h("p.ids", `${isStation ? "Station" : "Stop"} ${s.stop_id}${s.stop_code && s.stop_code !== s.stop_id ? `, code ${s.stop_code}` : ""}`),
        // what passengers read beside the name: the platform label and the description
        s.platform_code || o.changed.has("platform_code") ? h("p.platform-line", h("span.label-tag", "Platform label: "), shown("platform_code")) : null,
        s.description || o.changed.has("description") ? h("p.stop-description", shown("description")) : null),
      pendingNotice(o.actions, {
        current: live,
        intro: o.gone
          ? `Your draft “${draftTitle()}” ${o.gone.kind === "merge" ? `merges this stop into ${o.gone.into_stop_id}` : o.gone.kind === "dissolve" ? "dissolves this station" : `deletes this ${what}`}. It is still live until the draft is committed.`
          : `Your draft changes this ${what}. It is shown as it will be once the draft is committed, with what is live now marked “live”.`,
      }),
      live.deleted ? h("p.notice", "This stop has been removed from the feed.") : null,
      stationLine(),
      h("dl.facts",
        h("dt", "Position"), h("dd", o.moved
          ? [withLive(`${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`, `${fmtCoord(live.lat)}, ${fmtCoord(live.lon)}`), h("span.hint", ` moved ${map.apart(live, s)}`)]
          : `${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`),
        s.position_source ? [h("dt", "Position from"), h("dd", s.position_source)] : null,
        s.cluster_id || o.changed.has("cluster_id") ? [h("dt", "Cluster"), h("dd", shown("cluster_id"))] : null,
        s.regional_name || o.changed.has("regional_name") ? [h("dt", "Tamil name"), h("dd", shown("regional_name"))] : null,
      ),
      actions,
    ),
    isStation ? h("section.section",
      h("h2", `Stops in this station (${members.length})`),
      members.length ? h("ul.list", members.map((c) => h("li.list-item", { title: stopDetailWords(c) || null },
        h("span.key", c.platform_code || ""),
        c.leaves ? h("s", h("a", { href: `#/stop/${enc(c.stop_id)}` }, c.name)) : h("a", { href: `#/stop/${enc(c.stop_id)}` }, c.name),
        h("span.hint", c.joins ? h("span.chip.draft", "joins in the draft") : c.leaves ? h("span.chip.draft", "leaves in the draft") : `${c.route_count} route${c.route_count === 1 ? "" : "s"}`),
        h("span.sub", c.stop_id)))) : h("p.empty", "This station has no stops.")) : null,
    !isStation ? h("section.section",
      h("h2", `Routes stopping here (${s.routes.length})`),
      s.routes.length ? h("ul.list", s.routes.map((r) => h("li.list-item",
        h("span.key", r.short_name || r.route_id),
        h("a", { href: `#/route/${enc(r.route_id)}` }, r.long_name || `Route ${r.route_id}`),
        h("span.hint", `stop ${r.sequence}`),
        h("span.sub", `${STOP_TYPE_LABEL[r.stop_type] || r.stop_type} in stage ${r.stage_no}, route id ${r.route_id}`)))) : h("p.empty", "No route uses this stop.")) : null,
    h("section.section.nearby-stops",
      h("h2", "Other stops within 60 m"),
      h("p.hint", "Two stops close together are often the two sides of the road: club them into a station. Merge them only if they are the same kerb entered twice."),
      nearbyBox),
    stopContext(live, names),
  );

  // a stop that is already a platform of a station is not listed beside it again
  const here = location.hash;
  await stationNames(live.nearby, names, [live.parent]);
  if (location.hash !== here || !document.body.contains(nearbyBox)) return;
  const [list, stations] = foldedList(live.nearby, names, (n) => h("li.list-item", { title: stopDetailWords(n) || null },
    h("span.key", fmtMetres(n.distance_m)),
    h("a", { href: `#/stop/${enc(n.stop_id)}` }, n.name),
    h("span.item-end",
      h("span.hint", n.location_type === 1 ? "station" : plural(n.route_count, "route")),
      editor && !isStation && n.location_type === 0
        ? h("a.btn.quiet.small", { href: `#/merge/${enc(live.stop_id)}?with=${enc(n.stop_id)}`, "aria-label": `Merge ${live.name} with ${n.name} (${n.stop_id})` }, "Merge…")
        : null),
    h("span.sub", `${n.stop_id}${n.platform_code ? `, ${n.platform_code}` : ""}`)));
  clear(nearbyBox, live.nearby.length ? [platformsNote(stations, live.nearby.length), list] : h("p.empty", "None."));
}

// ------------------------------------------------------------------ route
// `preview`: true shows the route with the draft applied, false what is live;
// left out, the draft is applied whenever it touches the route.
export async function showRoute(routeId, { preview } = {}) {
  map.endModes();
  clear(panel(), h("section.section", h("p.empty", "Loading route…")));
  const created = createdChange("route", routeId);
  let live = null;
  try {
    if (!created) live = await get(`feeds/${enc(state.feedId)}/routes/${enc(routeId)}`);
  } catch (e) {
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/" }, "Back to search"), h("p.notice.error", e.message)));
  }
  // the draft touches a route through its details, its stop list, a merge that
  // switches its rows to another stop, or a stop of it that moves or is renamed
  const movedStops = live ? touchedStops(live.rows.filter((x) => x.stop_id)) : new Map();
  const touched = !!state.draft && (!!created || routesTouched(routeId) || movedStops.size > 0);
  const usePreview = touched && (created || preview !== false);
  let r = live;
  if (usePreview) {
    try {
      r = await get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(routeId)}`);
    } catch (e) {
      if (!live) return clear(panel(), h("section.section", h("a.crumb", { href: "#/" }, "Back to search"), h("p.notice.error", e.message)));
      toast(e.message, "error");
    }
  }
  const drafted = usePreview && r !== live;
  const o = live ? applyToRoute(live) : { changed: new Set(), actions: [], stops: [], gone: null };
  nameHere(r.short_name || `Route ${r.route_id}`);
  map.clearFocus();
  map.showRoute(r);
  // under the drafted line, where the route runs live
  if (drafted && live && (o.changed.has("encoded_polyline") || o.stops.length || movedStops.size)) {
    map.showRoute(live, { layer: "proposal", fit: false, dashed: true, color: "#536569", weight: 3, markers: false });
  }

  const served = r.rows.filter((x) => x.stop_type !== "ROUTE CORRECTION");
  // GIMS serves a route only once the nightly GTFS build gives it trips
  const notInFeed = created || !(r.provenance && r.provenance.in_shipped_feed === true);
  // what the draft does to the rows, against the live list
  const diff = drafted && live ? diffRows(live.rows, r.rows) : [];
  const rowKind = new Map(diff.filter((d) => d.after && d.kind !== "same").map((d) => [d.toIndex, d.kind]));
  const removed = diff.filter((d) => d.kind === "removed");
  let reviews = new Map();
  const field = (k) => (drafted && o.changed.has(k) ? withLive(r[k] || "none", live[k] || "none") : r[k] || "");
  const ladderBox = h("div");
  const drawLadder = () => clear(ladderBox, r.rows.length ? ladder(r.rows, {
    rowInfo: (row, index) => {
      const kind = rowKind.get(index);
      const review = row.stop_id ? reviews.get(row.stop_id) : null;
      const moved = drafted && row.stop_id && movedStops.get(row.stop_id);
      return {
        cls: kind === "added" ? "pending-added" : kind ? "pending-changed" : "",
        chips: [
          kind ? h("span.chip.draft.row-chip", { added: "added in draft", moved: "moved in draft", changed: "changed in draft" }[kind]) : null,
          moved && moved.moved ? h("span.chip.draft.row-chip", "stop moved in draft") : null,
          review && review.status === "pending" ? h("a.chip.under-review.row-chip", { href: `#/coordinates/${enc(review.review_id)}`, on: { click: (ev) => ev.stopPropagation() } }, "position under review") : null,
        ],
      };
    },
  }) : h("p.empty", "No stops yet."));

  clear(panel(),
    h("section.section",
      backLink(),
      h("div.title-block",
        h("h1", r.short_name || `Route ${r.route_id}`, drafted && o.changed.has("short_name") ? h("span.live-value", h("span.live-tag", "live: "), h("s", live.short_name || "none")) : null),
        h("p", field("long_name")),
        h("p.ids", `Route id ${r.route_id}, ${plural(r.stop_count, "stop")}, ${plural(r.rows.length ? groupStages(served).length : 0, "fare stage")}`)),
      created ? h("div.notice.draft",
        h("p", h("strong", `New route in your draft "${state.draft.title}".`)),
        h("p", "It is saved when the draft is approved by someone else and committed. Passengers see it only after the nightly GTFS build gives it trips from the MTC schedule.")) : null,
      touched && !created ? h("div.notice.draft.pending",
        h("p", h("strong", drafted ? `Pending in draft “${state.draft.title}”, not live.` : `Your draft “${state.draft.title}” changes this route.`), " ",
          h("a", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft")),
        h("p", drafted ? "You are looking at this route with your draft applied. It is not live: rows the draft adds, moves or changes are marked, and what is live is marked “live”." : "This is what is live now, without your draft."),
        drafted ? h("ul", [
          o.gone ? h("li", "The draft deletes this route.") : null,
          o.changed.size ? h("li", `Changes its ${[...o.changed].filter((k) => k !== "polyline_source").map((k) => ({ short_name: "route number", long_name: "name", color: "colour", text_color: "text colour", encoded_polyline: "map line" }[k] || k)).join(", ")}.`) : null,
          o.stops.length ? h("li", `Replaces its stop list: ${plural(diff.filter((d) => d.kind === "added").length, "stop")} added, ${removed.length} removed, ${diff.filter((d) => d.kind === "moved" || d.kind === "changed").length} moved or changed.`) : null,
          !o.stops.length && diff.some((d) => d.kind !== "same") ? h("li", "A merge in the draft switches some of its rows to the stop that stays.") : null,
          movedStops.size ? h("li", `${plural(movedStops.size, "stop")} on it ${movedStops.size === 1 ? "is" : "are"} moved, renamed or removed in the draft.`) : null].filter(Boolean)) : null,
        h("div.btn-row",
          drafted
            ? h("button.btn.secondary.small", { type: "button", on: { click: () => showRoute(routeId, { preview: false }) } }, "Show what is live now")
            : h("button.btn.secondary.small", { type: "button", on: { click: () => showRoute(routeId, { preview: true }) } }, "Show it with my draft"))) : null,
      !created && notInFeed ? h("p.notice", "This route is not in the published GTFS feed yet, so passengers do not see it. It appears in the live apps once the nightly GTFS build gives it trips from the MTC schedule.") : null,
      h("dl.facts",
        h("dt", "Map line"), h("dd", r.encoded_polyline
          ? [drafted && o.changed.has("encoded_polyline") ? h("span.drafted-value", `New in the draft (${r.polyline_source || "source unknown"})`) : `Saved (${r.polyline_source || "source unknown"})`,
             drafted && o.changed.has("encoded_polyline") ? h("span.live-value", " ", h("span.live-tag", "live: "), live.encoded_polyline ? `saved (${live.polyline_source || "source unknown"}), dashed grey on the map` : "none") : null]
          : "None yet. The map joins the stops with straight dashed lines."),
        r.color ? [h("dt", "Colour"), h("dd", h("span", { style: `display:inline-block;width:14px;height:14px;border-radius:3px;vertical-align:-2px;margin-right:6px;background:${r.color}` }), field("color"))] : null,
      ),
      // the editors start from the live route and lay the draft's change over it themselves
      can("editor") && (live || created) ? h("div.btn-row",
        h("button.btn", { type: "button", on: { click: () => editRouteRows(created ? r : live, { created: !!created }) } }, created && !r.rows.length ? "Build the stop list" : "Edit stop list"),
        h("button.btn.secondary", { type: "button", on: { click: () => editRouteDetails(created ? r : live, { created: !!created }) } }, "Edit name, colour and map line")) : null,
    ),
    h("section.section", h("h2", "Stops by fare stage"), ladderBox,
      removed.length ? h("p.pending-removed.notice.draft", `Taken off the route in the draft: ${removed.map((d) => d.before.stop_name || d.before.marker_name || d.before.stop_id).join(", ")}.`) : null),
    live ? routeContext(live, { onReviews: (m) => { reviews = m; drawLadder(); } }) : null,
  );
  drawLadder();
}

// The read-only stage ladder. `rowInfo(row, index)` may give a row a class and
// chips (pending in a draft, position under review).
export function ladder(rows, { onRow, rowInfo } = {}) {
  const stages = groupStages(rows);
  return h("div.ladder", stages.map((st) => h("div.stage",
    h("div.stage-no", { "aria-label": `Stage ${st.stage_no}` }, h("small", "stage"), String(st.stage_no)),
    h("div.stage-body",
      h("p.stage-name", st.stage_name || ""),
      h("ol.stops", st.rows.map(({ row, index }) => {
        const kind = { "NEW STOP": "new", "JUMP STOP": "jump", "ROUTE CORRECTION": "marker" }[row.stop_type] || "";
        const isMarker = row.stop_type === "ROUTE CORRECTION";
        const info = (rowInfo && rowInfo(row, index)) || {};
        const li = h("li.row", { class: `${kind} ${isMarker ? "" : "clickable"} ${info.cls || ""}`, tabindex: isMarker ? null : "0" },
          h("span.node", { "aria-hidden": "true" }),
          h("span.what",
            h("span.name", isMarker ? `Map shaping point: ${row.marker_name || row.marker_id}` : row.stop_name || row.stop_id, info.chips || null),
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
