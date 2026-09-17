// What a reviewer needs beside a stop or a route to decide on a clean-up: lists
// of nearby stops that show a station once instead of each of its platforms, and
// the "Cleanup context" sections (detour, same-named stops, coordinate reviews,
// open drafts, recent history).
import { get, enc, ApiError } from "./api.js";
import { state } from "./state.js";
import { h, clear, fmtDate, fmtMetres, plural, STATUS_LABEL, stopDetailWords } from "./util.js";
import * as map from "./map.js";
import { ACTION_LABEL } from "./admin.js";

// ------------------------------------------------------------------ stations, once
// A stop that is already a platform of a station is not a loose stop to club or
// merge: a list names its station once, says how many of the entries were its
// platforms, and keeps the platforms one click away.
//
// `items` carry `parent_station` (stop rows) and may include the station row
// itself. Returns {entries, stations}: entries are the items in their order, the
// first platform of each station replaced by {station, platforms, item?} and the
// other platforms (and the station's own row) folded into it.
export function foldPlatforms(items, names = new Map()) {
  const byStation = new Map();
  for (const it of items) {
    const sid = it.location_type === 1 ? it.stop_id : it.parent_station;
    if (!sid) continue;
    if (!byStation.has(sid)) byStation.set(sid, { station_id: sid, name: names.get(sid) || null, item: null, platforms: [] });
    const g = byStation.get(sid);
    if (it.location_type === 1) { g.item = it; g.name = it.name; } else g.platforms.push(it);
  }
  const placed = new Set();
  const entries = [];
  for (const it of items) {
    const sid = it.location_type === 1 ? it.stop_id : it.parent_station;
    const g = sid ? byStation.get(sid) : null;
    // a station row with none of its platforms in the list stays an ordinary entry
    if (!g || !g.platforms.length) { entries.push({ item: it }); continue; }
    if (placed.has(sid)) continue;
    placed.add(sid);
    entries.push({ station: g });
  }
  return { entries, stations: [...byStation.values()].filter((g) => g.platforms.length) };
}

// Names of the stations a list's platforms belong to, looked up once per panel:
// `cache` is a Map the caller keeps for as long as its panel lives.
export async function stationNames(items, cache = new Map(), known = []) {
  known.forEach((s) => { if (s && s.stop_id) cache.set(s.stop_id, s.name); });
  items.forEach((it) => { if (it.location_type === 1) cache.set(it.stop_id, it.name); });
  const missing = [...new Set(items.map((it) => it.parent_station).filter((sid) => sid && !cache.has(sid)))];
  await Promise.all(missing.map((sid) => get(`feeds/${enc(state.feedId)}/stops/${enc(sid)}`)
    .then((d) => cache.set(sid, d.name))
    .catch(() => cache.set(sid, null))));
  return cache;
}

const stationLink = (g) => h("a", { href: `#/stop/${enc(g.station_id)}` }, g.name || `station ${g.station_id}`);

// "2 of these are already platforms of Luz." One line per station, linked.
export function platformsNote(stations, total) {
  if (!stations.length) return null;
  return h("div.notice.platforms-note", stations.map((g) => h("p",
    `${g.platforms.length === total ? (total === 1 ? "This one is" : `All ${total} are`) : `${g.platforms.length} of these ${g.platforms.length === 1 ? "is" : "are"}`} already ${g.platforms.length === 1 ? "a platform" : "platforms"} of `,
    stationLink(g), ", so the station is listed once instead. Open it to see or change its stops.")));
}

// The list itself. `row(item, {nested})` draws one stop as an li; a station is
// one li with its platforms folded under "Show its platforms here".
export function foldedList(items, names, row, { key = null } = {}) {
  const { entries, stations } = foldPlatforms(items, names);
  if (!entries.length) return [null, []];
  const list = h("ul.list", entries.map((e) => {
    if (e.item) return row(e.item, { nested: false });
    const g = e.station;
    const nearest = g.platforms.reduce((a, p) => (a == null || (p.distance_m ?? Infinity) < a ? p.distance_m ?? a : a), g.item ? g.item.distance_m : null);
    return h("li.list-item.station-entry",
      h("span.key", key ? key(g, nearest) : nearest != null ? fmtMetres(nearest) : ""),
      h("span", stationLink(g), " ", h("span.chip", "Station")),
      h("span.hint", plural(g.platforms.length, "platform")),
      h("span.sub", `${g.station_id}, ${g.platforms.length === 1 ? "its platform here is" : "its platforms here are"} ${g.platforms.map((p) => p.stop_id).join(", ")}`),
      h("details.platforms-fold",
        h("summary", `Show ${g.platforms.length === 1 ? "that platform" : `those ${g.platforms.length} platforms`}`),
        h("ul.list.nested", g.platforms.map((p) => row(p, { nested: true })))));
  }));
  return [list, stations];
}

// ------------------------------------------------------------------ cleanup context
const REVIEW_WORDS = { pending: "to review", approved: "in a draft", committed: "fixed and live", confirmed: "confirmed as correct" };
const actionLabel = (action) => ACTION_LABEL[action] || (action.charAt(0).toUpperCase() + action.slice(1)).replace(/_/g, " ");

const DETOUR_WORDS = "How much further its routes travel to call here than they would going straight from the stop before to the stop after: the median over the routes that pass through. A few metres is a kerb; hundreds of metres usually means the coordinate belongs to another place.";

function draftsBlock(items) {
  if (!items || !items.length) return null;
  return h("div.context-block",
    h("h3", `Open drafts touching it (${items.length})`),
    h("ul.list", items.map((d) => h("li.list-item",
      h("span", { class: `chip ${d.status}` }, STATUS_LABEL[d.status] || d.status),
      h("a", { href: `#/drafts/${enc(d.change_set_id)}` }, d.title || "untitled"),
      h("span.hint", `${d.entity} ${d.op}`),
      h("span.sub", `change #${d.change_id}`)))));
}

function historyBlock(items) {
  if (!items || !items.length) return null;
  return h("div.context-block",
    h("h3", "Recent history"),
    h("ul.timeline", items.slice(0, 8).map((a) => h("li",
      h("strong", actionLabel(a.action)), ` · ${a.actor_email || "system"} · ${fmtDate(a.at)}`,
      a.change_set_id ? [" · ", h("a", { href: `#/drafts/${enc(a.change_set_id)}` }, "draft")] : null))));
}

// The endpoint is newer than some servers: a 404 (or any failure) hides the
// section rather than showing an error for something optional.
async function load(path, box) {
  try {
    return await get(path);
  } catch (e) {
    if (!(e instanceof ApiError && e.status === 404)) console.warn(`cleanup context: ${e.message}`);
    box.hidden = true;
    box.dataset.context = "unavailable";
    return null;
  }
}

// For the stop panel. Returns the section at once and fills it when the answer
// comes; `names` is the panel's station-name cache.
export function stopContext(stop, names = new Map()) {
  const box = h("section.section.cleanup-context", { hidden: true, "aria-label": "Cleanup context" });
  const here = location.hash;
  load(`feeds/${enc(state.feedId)}/stops/${enc(stop.stop_id)}/context`, box).then(async (c) => {
    if (!c || location.hash !== here || !document.body.contains(box)) return;
    const same = Array.isArray(c.same_name) ? c.same_name : [];
    await stationNames(same, names);
    if (location.hash !== here) return;
    const pr = c.position_reviews || {};
    const reviews = Array.isArray(pr.items) ? pr.items : [];
    const counts = ["pending", "approved", "committed", "confirmed"].filter((k) => pr[k]).map((k) => `${pr[k]} ${REVIEW_WORDS[k]}`);
    const [sameList, stations] = foldedList(same, names, (n) => h("li.list-item", { title: stopDetailWords(n) || null },
      h("span.key", fmtMetres(n.distance_m)),
      h("a", { href: `#/stop/${enc(n.stop_id)}` }, n.name),
      h("span.hint", plural(n.route_count || 0, "route")),
      h("span.sub", [n.stop_id, n.similarity != null && n.similarity < 1 ? `name ${Math.round(n.similarity * 100)}% alike` : "same name"].filter(Boolean).join(", "))));
    map.showFaint(same.filter((n) => n.lat != null));
    box.hidden = false;
    clear(box,
      h("h2", "Cleanup context"),
      h("dl.facts",
        h("dt", "Detour"), h("dd", c.detour_m == null ? "Not measured: no route passes through it (its routes start or end here, or none calls)."
          : `${fmtMetres(Math.max(0, c.detour_m))} over ${plural(c.routes_measured || 0, "route")}`)),
      h("p.hint", DETOUR_WORDS),
      h("div.context-block",
        h("h3", `Coordinate reviews (${reviews.length})`),
        reviews.length ? [
          counts.length ? h("p.hint", counts.join(", ")) : null,
          h("ul.list", reviews.map((r) => h("li.list-item",
            h("span", { class: `chip ${{ pending: "submitted", approved: "approved", confirmed: "discarded", committed: "committed" }[r.status] || ""}` }, REVIEW_WORDS[r.status] || r.status),
            h("a", { href: `#/coordinates/${enc(r.review_id)}` }, `Review #${r.review_id}`),
            h("span.sub", r.reason || ""))))]
          : h("p.empty", "No coordinate review names this stop.")),
      h("div.context-block",
        h("h3", `Same-named stops nearby (${same.length})`),
        same.length ? [h("p.hint", "Shown faintly on the map. A same-named stop a few metres away may be a duplicate to merge; across the road it is the other kerb, for a station."),
          platformsNote(stations, same.length), sameList]
          : h("p.empty", "None.")),
      draftsBlock(c.open_drafts),
      historyBlock(c.audit));
  });
  return box;
}

// For the route panel. `onReviews(Map stop_id -> {review_id, status})` lets the
// ladder mark the rows whose stop is under review.
export function routeContext(route, { onReviews } = {}) {
  const box = h("section.section.cleanup-context", { hidden: true, "aria-label": "Cleanup context" });
  const here = location.hash;
  load(`feeds/${enc(state.feedId)}/routes/${enc(route.route_id)}/context`, box).then((c) => {
    if (!c || location.hash !== here || !document.body.contains(box)) return;
    const flagged = Array.isArray(c.stops_with_reviews) ? c.stops_with_reviews : [];
    const worst = Array.isArray(c.worst_detours) ? c.worst_detours : [];
    const pending = flagged.filter((f) => f.status === "pending");
    if (onReviews) onReviews(new Map(flagged.map((f) => [f.stop_id, f])));
    const nameOf = (id) => (route.rows.find((r) => r.stop_id === id) || {}).stop_name || id;
    box.hidden = false;
    clear(box,
      h("h2", "Cleanup context"),
      h("div.context-block",
        h("h3", `Stops under coordinate review (${flagged.length})`),
        flagged.length ? [
          h("p.hint", pending.length ? `${plural(pending.length, "stop")} on this route ${pending.length === 1 ? "is" : "are"} waiting for a position check, marked in the list below.` : "None is waiting; these were reviewed."),
          h("ul.list", flagged.map((f) => h("li.list-item",
            h("span.key", `stop ${f.sequence}`),
            h("a", { href: `#/coordinates/${enc(f.review_id)}` }, nameOf(f.stop_id)),
            h("span.hint", REVIEW_WORDS[f.status] || f.status),
            h("span.sub", `${f.stop_id}, review #${f.review_id}`))))]
          : h("p.empty", "No stop of this route is in a coordinate review.")),
      h("div.context-block",
        h("h3", "Longest detours"),
        h("p.hint", "The stops this route goes furthest out of its way to reach: previous stop to the stop to the next, against previous straight to next. A long one is worth a look on the map."),
        worst.length ? h("ul.list", worst.map((w) => h("li.list-item",
          h("span.key", fmtMetres(Math.max(0, w.detour_m))),
          h("a", { href: `#/stop/${enc(w.stop_id)}` }, w.name || w.stop_id),
          h("span.hint", `stop ${w.sequence}`),
          h("span.sub", w.stop_id)))) : h("p.empty", "No detour worth naming.")),
      draftsBlock(c.open_drafts),
      historyBlock(c.audit));
  });
  return box;
}
