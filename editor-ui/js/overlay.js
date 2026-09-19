// What the active draft would do to a stop, station or route, for display only.
// A change added to a draft is not live, but the person who added it wants to see
// it: pages read the entity from the API as it is live, then lay the draft's
// changes over it here, labelled "pending in draft, not live" with the live value
// still in view. Nothing in this module writes anything.
//
// One shape serves every screen: an action is {kind, change_id, lat?, lon?, ...}.
// A coordinate review's `draft_actions` from the server already have that shape
// (move, split, merge), and pendingActions() gives the same for any entity from
// the draft's own changes, so the stop, route and station panels and the
// position-review panel share actionsList(), draftedPoints() and pendingNotice().
import { enc } from "./api.js";
import { state } from "./state.js";
import { h, fmtCoord, fmtMetres, haversine, plural } from "./util.js";

// The draft's changes by entity, rebuilt when the draft object changes: every
// mutation the dashboard makes replaces state.draft (useDraft, refreshDraft), as
// does choosing another draft, so identity is the whole invalidation rule.
let indexedFor;
let index = null;
let stamp = 0;

const push = (m, k, v) => { if (!m.has(k)) m.set(k, []); m.get(k).push(v); };

function memberSpec(after) {
  if (!after) return null;
  if (Array.isArray(after.members)) return after.members.filter((m) => m && m.stop_id).map((m) => ({ stop_id: m.stop_id, platform_code: m.platform_code, labelled: "platform_code" in m }));
  if (Array.isArray(after.member_stop_ids)) return after.member_stop_ids.map((id) => ({ stop_id: id, labelled: false }));
  return null;
}

function build() {
  if (index && state.draft === indexedFor) return index;
  indexedFor = state.draft;
  stamp += 1;
  index = { byKey: new Map(), absorbs: new Map(), stationAbsorbs: new Map(), movesTo: new Map(), joins: new Map(), leaves: new Map(), routes: new Set() };
  for (const c of (state.draft && state.draft.changes) || []) {
    push(index.byKey, `${c.entity}|${c.entity_key}`, c);
    const after = c.after || {};
    if (c.entity === "route" || c.entity === "route_stops") index.routes.add(c.entity_key);
    if (c.entity === "stop" && c.op === "merge" && after.into_stop_id) {
      push(index.absorbs, after.into_stop_id, c);
      // the routes a merge switches to the stop that stays
      ((c.before && c.before.affected) || []).forEach((r) => index.routes.add(r.route_id));
    }
    if (c.entity === "station" && c.op === "merge" && after.into_station_id) {
      push(index.stationAbsorbs, after.into_station_id, c);
      // the platforms the merge re-parents: each one shows it on its own page
      ((c.before && c.before.moving_platforms) || []).forEach((p) => index.movesTo.set(p.stop_id, c));
    }
    if (c.entity === "station" && c.op !== "merge") {
      const was = (c.before && (c.before.member_stop_ids || (c.before.members || []).map((m) => m.stop_id))) || [];
      const now = c.op === "delete" ? [] : memberSpec(after);
      if (now) {
        now.forEach((m) => index.joins.set(m.stop_id, { change: c, member: m, already: was.includes(m.stop_id) }));
        was.filter((id) => !now.some((m) => m.stop_id === id)).forEach((id) => index.leaves.set(id, c));
      }
    }
  }
  return index;
}

// Drop the cache by hand (tests, or a caller that changed the draft in place).
export function invalidate() {
  index = null;
}

// Changes every time the draft does; the map redraws its stops on it.
export function draftStamp() {
  build();
  return stamp;
}

export const draftTitle = () => (state.draft ? state.draft.title : "");

export function changesFor(entity, key) {
  return build().byKey.get(`${entity}|${key}`) || [];
}

// Does the draft change this route: its details, its stop list, or (through a
// merge) the stop ids its rows carry?
export function routesTouched(routeId) {
  return build().routes.has(routeId);
}

// ------------------------------------------------------------------ actions
const STOP_TEXT_FIELDS = ["platform_code", "description", "cluster_id", "regional_name", "hindi_name"];

// Everything the active draft does to one entity, in change order.
export function pendingActions(entity, key) {
  const idx = build();
  const out = [];
  for (const c of changesFor(entity, key)) {
    const a = c.after || {};
    const base = { change_id: c.change_id, review_id: a.position_review_id ?? null };
    if (entity === "stop" || entity === "station") {
      if (c.op === "create") out.push({ ...base, kind: "create", lat: a.lat, lon: a.lon, name: a.name, members: memberSpec(a) });
      else if (c.op === "delete") out.push({ ...base, kind: entity === "station" ? "dissolve" : "delete" });
      else if (c.op === "merge" && entity === "station") out.push({ ...base, kind: "station_merge", into_stop_id: a.into_station_id,
        keep_name: a.keep_name || "into", keep_position: a.keep_position || "into",
        platforms: ((c.before && c.before.moving_platforms) || []).length,
        lat: c.before && c.before.into ? c.before.into.lat : null, lon: c.before && c.before.into ? c.before.into.lon : null,
        into_name: c.before && c.before.into ? c.before.into.name : null });
      else if (c.op === "merge") out.push({ ...base, kind: "merge", into_stop_id: a.into_stop_id, keep_name: a.keep_name || "into", keep_position: a.keep_position || "into",
        lat: c.before && c.before.into ? c.before.into.lat : null, lon: c.before && c.before.into ? c.before.into.lon : null,
        into_name: c.before && c.before.into ? c.before.into.name : null });
      else {
        if (a.lat != null && a.lon != null) out.push({ ...base, kind: "move", lat: a.lat, lon: a.lon });
        if (a.name != null) out.push({ ...base, kind: "rename", name: a.name });
        const fields = STOP_TEXT_FIELDS.filter((k) => k in a);
        if (fields.length) out.push({ ...base, kind: "edit", fields, values: Object.fromEntries(fields.map((k) => [k, a[k]])) });
        if (entity === "station" && memberSpec(a)) out.push({ ...base, kind: "members", members: memberSpec(a) });
      }
    } else if (entity === "route") {
      if (c.op === "create") out.push({ ...base, kind: "create", name: a.short_name });
      else if (c.op === "delete") out.push({ ...base, kind: "delete" });
      else out.push({ ...base, kind: "edit", fields: Object.keys(a), values: a });
    } else if (entity === "route_stops") {
      out.push({ ...base, kind: "stops", rows: (a.rows || []).length });
    }
  }
  if (entity === "station") {
    for (const c of idx.stationAbsorbs.get(key) || []) {
      const a = c.after || {};
      const from = (c.before && c.before.from) || {};
      out.push({ kind: "station_absorb", change_id: c.change_id, from_stop_id: c.entity_key, from_name: from.name || null,
        keep_name: a.keep_name || "into", keep_position: a.keep_position || "into",
        platforms: ((c.before && c.before.moving_platforms) || []).length,
        lat: from.lat ?? null, lon: from.lon ?? null });
    }
  }
  if (entity === "stop") {
    const movedBy = idx.movesTo.get(key);
    if (movedBy) {
      out.push({ kind: "moved_station", change_id: movedBy.change_id, station_id: (movedBy.after || {}).into_station_id,
        from_station_id: movedBy.entity_key });
    }
    for (const c of idx.absorbs.get(key) || []) {
      const a = c.after || {};
      const from = (c.before && c.before.from) || {};
      out.push({ kind: "absorb", change_id: c.change_id, from_stop_id: c.entity_key, from_name: from.name || null,
        keep_name: a.keep_name || "into", keep_position: a.keep_position || "into", lat: from.lat ?? null, lon: from.lon ?? null });
    }
    const join = idx.joins.get(key);
    if (join && !join.already) out.push({ kind: "join", change_id: join.change.change_id, station_id: join.change.entity_key, station_name: (join.change.after || {}).name || null, platform_code: join.member.platform_code });
    else if (join && join.member.labelled) out.push({ kind: "relabel", change_id: join.change.change_id, station_id: join.change.entity_key, platform_code: join.member.platform_code });
    const leave = idx.leaves.get(key);
    if (leave) out.push({ kind: "leave", change_id: leave.change_id, station_id: leave.entity_key, dissolved: leave.op === "delete" });
  }
  return out;
}

// The stop (or station) row as it would be once the draft is committed.
// `changed` names the fields that differ from live; `gone` says why it goes away.
export function applyToStop(stop) {
  const entity = stop.location_type === 1 ? "station" : "stop";
  const actions = pendingActions(entity, stop.stop_id);
  const row = { ...stop };
  const changed = new Set();
  let gone = null;
  const put = (k, v) => { if ((row[k] ?? null) !== (v ?? null)) { row[k] = v; changed.add(k); } };
  for (const a of actions) {
    if (a.kind === "move") { put("lat", a.lat); put("lon", a.lon); }
    else if (a.kind === "rename") put("name", a.name);
    else if (a.kind === "edit") a.fields.forEach((k) => put(k, a.values[k]));
    else if (a.kind === "delete" || a.kind === "dissolve" || a.kind === "merge" || a.kind === "station_merge") gone = a;
    else if (a.kind === "moved_station") put("parent_station", a.station_id);
    else if (a.kind === "absorb" || a.kind === "station_absorb") {
      if (a.keep_name === "from" && a.from_name) put("name", a.from_name);
      if (a.keep_position === "from" && a.lat != null) { put("lat", a.lat); put("lon", a.lon); }
    } else if (a.kind === "join") {
      put("parent_station", a.station_id);
      if (a.platform_code !== undefined) put("platform_code", a.platform_code);
    } else if (a.kind === "relabel") put("platform_code", a.platform_code);
    else if (a.kind === "leave") put("parent_station", null);
  }
  const moved = changed.has("lat") || changed.has("lon");
  // a station change that sends a platform's label back unchanged is not news
  const told = actions.filter((a) => a.kind !== "relabel" || (stop.platform_code ?? null) !== (a.platform_code ?? null));
  return { row, live: stop, actions: told, changed, gone, moved, pending: told.length > 0 };
}

// Route fields (number, name, colour, map line) the draft changes.
export function applyToRoute(route) {
  const actions = pendingActions("route", route.route_id);
  const row = { ...route };
  const changed = new Set();
  let gone = null;
  for (const a of actions) {
    if (a.kind === "delete") gone = a;
    if (a.kind !== "edit") continue;
    for (const k of a.fields) if ((row[k] ?? null) !== (a.values[k] ?? null)) { row[k] = a.values[k]; changed.add(k); }
  }
  return { row, live: route, actions, changed, gone, stops: pendingActions("route_stops", route.route_id), pending: actions.length > 0 };
}

// Stops of a list (the map's area stops, a route's rows) at their drafted
// positions and names: [{stop, live, moved, gone}] for those the draft touches.
export function touchedStops(stops) {
  if (!state.draft || !state.draft.changes.length) return new Map();
  const out = new Map();
  const idx = build();
  for (const s of stops) {
    if (!s || !s.stop_id || s.draft) continue;
    if (!idx.byKey.has(`stop|${s.stop_id}`) && !idx.byKey.has(`station|${s.stop_id}`)
      && !idx.absorbs.has(s.stop_id) && !idx.stationAbsorbs.has(s.stop_id) && !idx.movesTo.has(s.stop_id)) continue;
    const o = applyToStop(s);
    if (o.moved || o.gone || o.changed.has("name")) out.set(s.stop_id, o);
  }
  return out;
}

// Stops whose station the draft changes (the map's station links follow it):
// stop_id -> {parent_station, change}, `parent_station` null for a stop that
// leaves. A station the draft creates has its point in `change.after`.
export function draftedParents(stops) {
  const out = new Map();
  if (!state.draft || !state.draft.changes.length) return out;
  const idx = build();
  if (!idx.joins.size && !idx.leaves.size) return out;
  for (const s of stops) {
    if (!s || !s.stop_id) continue;
    const join = idx.joins.get(s.stop_id);
    const movedBy = idx.movesTo.get(s.stop_id);
    if (join && !join.already) out.set(s.stop_id, { parent_station: join.change.entity_key, change: join.change });
    else if (movedBy) out.set(s.stop_id, { parent_station: (movedBy.after || {}).into_station_id, change: movedBy });
    else if (!join && idx.leaves.has(s.stop_id)) out.set(s.stop_id, { parent_station: null, change: idx.leaves.get(s.stop_id) });
  }
  return out;
}

// ------------------------------------------------------------------ words
const KEY = {
  move: "Move", split: "Split", merge: "Merge", rename: "Rename", edit: "Edit", delete: "Delete", dissolve: "Dissolve",
  create: "New", absorb: "Merge", join: "Station", leave: "Station", relabel: "Platform", members: "Stops", stops: "Stop list",
  station_merge: "Merge", station_absorb: "Merge", moved_station: "Station",
};
const FIELD_WORDS = {
  platform_code: "platform label", description: "description", cluster_id: "cluster", regional_name: "Tamil name", hindi_name: "Hindi name",
  short_name: "route number", long_name: "route name", color: "colour", text_color: "text colour", encoded_polyline: "map line", polyline_source: null,
};
const stopLink = (id, words) => h("a", { href: `#/stop/${enc(id)}` }, words || id);

// One action in words. `ctx.current` is the live point (for "how far"), and
// `ctx.routeLabel(route_id)` names a route.
export function actionLine(a, ctx = {}) {
  const from = ctx.current && a.lat != null ? `, ${fmtMetres(haversine(ctx.current.lat, ctx.current.lon, a.lat, a.lon))} from where it ${ctx.current.live === false ? "was" : "is live"}` : "";
  const label = ctx.routeLabel || ((rid) => rid);
  switch (a.kind) {
    case "move": return h("span", `Moves the stop to ${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}${from}.`);
    case "split": return h("span", `Splits ${plural((a.route_ids || []).length, "route")} (${(a.route_ids || []).map(label).join(", ")}) onto `, stopLink(a.new_stop_id), ".");
    case "merge": return h("span", "Merges this stop into ", stopLink(a.into_stop_id, a.into_name ? `${a.into_name} (${a.into_stop_id})` : a.into_stop_id),
      ". Its routes switch to that stop and this one goes away.");
    case "absorb": return h("span", "Stop ", stopLink(a.from_stop_id, a.from_name ? `${a.from_name} (${a.from_stop_id})` : a.from_stop_id), " is merged into this stop",
      a.keep_name === "from" || a.keep_position === "from" ? `, which takes its ${[a.keep_name === "from" ? "name" : null, a.keep_position === "from" ? "position" : null].filter(Boolean).join(" and ")}` : "", ".");
    case "station_merge": return h("span", "Merges this station into ", stopLink(a.into_stop_id, a.into_name ? `${a.into_name} (${a.into_stop_id})` : a.into_stop_id),
      `. Its ${plural(a.platforms ?? 0, "platform")} move${a.platforms === 1 ? "s" : ""} to that station and this one goes away. No route changes.`);
    case "station_absorb": return h("span", "Station ", stopLink(a.from_stop_id, a.from_name ? `${a.from_name} (${a.from_stop_id})` : a.from_stop_id),
      ` is merged into this station, which takes its ${plural(a.platforms ?? 0, "platform")}`,
      a.keep_name === "from" || a.keep_position === "from" ? ` and its ${[a.keep_name === "from" ? "name" : null, a.keep_position === "from" ? "point" : null].filter(Boolean).join(" and ")}` : "", ".");
    case "moved_station": return h("span", "Moves from station ", stopLink(a.from_station_id), " to ", stopLink(a.station_id),
      ", which absorbs it. Its platform label and its routes do not change.");
    case "rename": return h("span", `Renames it to “${a.name}”.`);
    case "edit": return h("span", `Changes the ${a.fields.map((k) => (k in FIELD_WORDS ? FIELD_WORDS[k] : k)).filter(Boolean).join(", ")}.`);
    case "delete": return h("span", "Deletes it.");
    case "dissolve": return h("span", "Dissolves the station. Its stops stay, ungrouped.");
    case "create": return h("span", "Creates it. It does not exist for passengers yet.");
    case "join": return h("span", "Joins station ", stopLink(a.station_id, a.station_name ? `${a.station_name} (${a.station_id})` : a.station_id),
      a.platform_code ? `, as “${a.platform_code}”` : "", ".");
    case "leave": return h("span", a.dissolved ? "Leaves station " : "Is taken out of station ", stopLink(a.station_id), a.dissolved ? ", which is dissolved." : ".");
    case "relabel": return h("span", a.platform_code ? `Platform label becomes “${a.platform_code}”.` : "Platform label is cleared.");
    case "members": return h("span", `Groups ${plural(a.members.length, "stop")}: ${a.members.map((m) => m.stop_id).join(", ")}.`);
    case "stops": return h("span", `Replaces the stop list (${plural(a.rows, "row")}).`);
    default: return h("span", a.kind);
  }
}

// The actions as a list: what each does, and the detour it gives when known.
export function actionsList(actions, ctx = {}) {
  if (!actions.length) return null;
  return h("ul.list.draft-actions", actions.map((a) => h("li.list-item", { dataset: { kind: a.kind } },
    h("span.key", KEY[a.kind] || a.kind),
    actionLine(a, ctx),
    h("span.hint", a.detour_m_after != null ? `detour ${fmtMetres(Math.max(0, a.detour_m_after))}` : ""))));
}

// A marker for each action that has a place: [{lat, lon, label}].
export function draftedPoints(actions) {
  return actions.filter((a) => a.lat != null && a.lon != null && ["move", "split", "merge", "station_merge", "create"].includes(a.kind)).map((a) => ({
    lat: a.lat, lon: a.lon,
    label: a.kind === "move" ? "Moved here in the draft" : a.kind === "split" ? `New stop ${a.new_stop_id}, in the draft`
      : a.kind === "merge" || a.kind === "station_merge" ? `Merges into ${a.into_stop_id}, in the draft` : "New, in the draft",
  }));
}

// The banner over an entity the draft changes: it is pending, in which draft,
// and what it does. Every page says it with the same words.
export function pendingNotice(actions, ctx = {}) {
  if (!actions.length || !state.draft) return null;
  return h("div.notice.draft.pending", { role: "note" },
    h("p", h("strong", `Pending in draft “${state.draft.title}”, not live.`), " ",
      h("a", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft")),
    ctx.intro ? h("p", ctx.intro) : null,
    actionsList(actions, ctx));
}

// A drafted value beside the live one it replaces.
export function withLive(drafted, live) {
  return [h("span.drafted-value", drafted), " ", h("span.live-value", h("span.live-tag", "live: "), h("s", live))];
}
