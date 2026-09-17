// Merging duplicate stops: choose the duplicate, compare the two side by side,
// choose which stop id stays, see which routes switch, and add the merge to a
// draft. On commit every route row on the stop that goes switches to the one
// that stays, and the other stop is removed.
import { get, enc } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, confirmDialog, fmtCoord, fmtMetres, haversine, nameLikeness, plural } from "./util.js";
import * as map from "./map.js";
import { addChange, requireDraft, removeChange } from "./drafts.js";
import { stopPicker } from "./picker.js";
import { undoScope } from "./undo.js";
import { nameHere } from "./trail.js";

const panel = () => document.getElementById("panel");
const detail = (id) => get(`feeds/${enc(state.feedId)}/stops/${enc(id)}`);
const FAR_METRES = 150;
const CANDIDATE_METRES = 250;

export async function showMerge(stopId, withId = null) {
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  setLeaveGuard(null);
  clear(panel(), h("section.section", h("p.empty", "Loading…")));
  const back = h("a.crumb", { href: `#/stop/${enc(stopId)}` }, "Back to the stop");
  if (!can("editor")) {
    return clear(panel(), h("section.section", back, h("p.notice", "Merging stops needs the editor role. Ask an admin.")));
  }
  let a, b = null;
  try {
    [a, b] = await Promise.all([detail(stopId), withId ? detail(withId) : null]);
  } catch (e) {
    return clear(panel(), h("section.section", back, h("p.notice.error", e.message)));
  }
  const refuse = (s) => (s.location_type === 1 ? `${s.name} is a station. Merge the stops in it instead.` : s.deleted ? `${s.name} (${s.stop_id}) has been removed already.` : null);
  const problem = refuse(a) || (b && refuse(b)) || (b && b.stop_id === a.stop_id ? "Choose a different stop to merge with." : null);
  if (problem) return clear(panel(), h("section.section", back, h("p.notice.error", problem)));
  if (!b) return chooseDuplicate(a);
  compare(a, b);
}

// ------------------------------------------------------------------ step 1: which stop
async function chooseDuplicate(a) {
  map.focusStop(a);
  const go = (id) => { location.hash = `#/merge/${enc(a.stop_id)}?with=${enc(id)}`; };
  const nearby = h("div", h("p.empty", "Looking for stops close by…"));
  const picker = stopPicker({
    title: "Search by name or stop id, or pick it on the map",
    near: { lat: a.lat, lon: a.lon, label: "this stop" },
    exclude: [a.stop_id],
    excludeReason: "that is the stop being merged. Choose its duplicate.",
    includeDraft: false,
    suggest: false,
    mapMessage: `Click the stop that duplicates ${a.name}.`,
    onPick: (s) => go(s.stop_id),
  });
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/stop/${enc(a.stop_id)}` }, "Back to the stop"),
      h("div.title-block",
        h("h1", `Merge ${a.name} with a duplicate`),
        h("p.ids", `Stop ${a.stop_id}, ${plural(new Set(a.routes.map((r) => r.route_id)).size, "route")}`)),
      h("p.hint", "Merge two stops only when they are the same kerb entered twice. One stop id stays; every route that uses the other stop switches to it, and the other stop is removed when the draft is committed. Two sides of a road are two stops: club those into a station instead.")),
    h("section.section",
      h("h2", "Stops close by"),
      nearby),
    h("section.section",
      h("h2", "Or find the duplicate"),
      picker.el));

  const d = 0.0025;
  try {
    const page = await get(`feeds/${enc(state.feedId)}/stops?bbox=${[a.lat - d, a.lon - d, a.lat + d, a.lon + d].map((x) => x.toFixed(6)).join(",")}&limit=300`);
    const found = page.items
      .filter((s) => s.stop_id !== a.stop_id && s.location_type === 0)
      .map((s) => ({ s, d: haversine(a.lat, a.lon, s.lat, s.lon), like: nameLikeness(a.name, s.name) }))
      .filter((x) => x.d <= 60 || (x.like && x.d <= CANDIDATE_METRES))
      .sort((x, y) => y.like - x.like || x.d - y.d)
      .slice(0, 12);
    clear(nearby, found.length
      ? [h("p.hint", "Same and similar names first, then the closest."),
         h("ul.list", found.map(({ s, d: dist, like }) => h("li.list-item",
           h("span.key", fmtMetres(dist)),
           h("span", h("strong", s.name), " ", like === 2 ? h("span.chip", "Same name") : like === 1 ? h("span.chip", "Similar name") : null),
           h("button.btn.secondary.small", { type: "button", "aria-label": `Compare with ${s.name} (${s.stop_id})`, on: { click: () => go(s.stop_id) } }, "Compare"),
           h("span.sub", [s.stop_id, plural(s.route_count, "route"), s.platform_code, s.parent_station ? `in station ${s.parent_station}` : null].filter(Boolean).join(" · ")))))]
      : h("p.empty", `No stop within 60 m, and none with a similar name within ${CANDIDATE_METRES} m. Search for it below.`));
  } catch (e) {
    clear(nearby, h("p.notice.error", e.message));
  }
}

// ------------------------------------------------------------------ step 2: compare and choose
const routeCount = (s) => new Set(s.routes.map((r) => r.route_id)).size;

function byRoute(s) {
  const out = new Map();
  for (const r of s.routes) {
    if (!out.has(r.route_id)) out.set(r.route_id, { route_id: r.route_id, short_name: r.short_name, long_name: r.long_name, sequences: [] });
    out.get(r.route_id).sequences.push(r.sequence);
  }
  return out;
}

const stopsWord = (seqs) => (seqs.length === 1 ? `stop ${seqs[0]}` : `stops ${seqs.slice(0, -1).join(", ")} and ${seqs[seqs.length - 1]}`);
const routeName = (r) => (r.short_name && r.short_name !== r.route_id ? `${r.short_name} (${r.route_id})` : `${r.route_id}`);

function compare(a, b) {
  const suggested = routeCount(b) > routeCount(a) ? b.stop_id : a.stop_id;
  const choice = { keep: suggested, name: "into", position: "into" };
  const byId = { [a.stop_id]: a, [b.stop_id]: b };
  nameHere(`Merge ${a.name}`);
  // which id, name and position stay are choices on the screen until the merge
  // is added to a draft: each is one step to undo
  const history = undoScope("this merge");
  const choose = (patch, label) => {
    const before = { ...choice }, after = { ...choice, ...patch };
    const put = (c) => {
      Object.assign(choice, c);
      const radio = document.getElementById(`keep-${choice.keep}`);
      if (radio) radio.checked = true;
      update({ regroup: true });
    };
    history.push({ label, undo: () => put(before), redo: () => put(after) });
    Object.assign(choice, patch);
  };
  const other = (id) => (id === a.stop_id ? b : a);
  const dist = haversine(a.lat, a.lon, b.lat, b.lon);
  const namesDiffer = a.name !== b.name;

  const nameBox = h("fieldset.choice-group", { hidden: !namesDiffer });
  const positionBox = h("fieldset.choice-group", { hidden: dist < 1 });
  const impact = h("div", { "aria-live": "polite" });
  const addBtn = h("button.btn", { type: "button" }, "Add merge to draft");

  const card = (s) => h("label.radio-card", { for: `keep-${s.stop_id}` },
    h("input", { type: "radio", name: "keep-id", id: `keep-${s.stop_id}`, value: s.stop_id, checked: s.stop_id === choice.keep,
      on: { change: () => { choose({ keep: s.stop_id, name: "into", position: "into" }, `chose to keep ${s.stop_id}`); update({ regroup: true }); } } }),
    h("span.radio-card-body",
      h("span.radio-card-title", `Keep stop id ${s.stop_id}`),
      h("span", s.name),
      h("span.hint", `${plural(routeCount(s), "route")}, ${plural(s.routes.length, "stop")} in route lists`),
      s.stop_id === suggested ? h("span.chip", "Suggested: used by more routes") : null));

  const radios = (box, key, legend, options) => clear(box, h("legend", legend), options.map(([value, label]) => h("label.check", { for: `${key}-${value}` },
    h("input", { type: "radio", name: key, id: `${key}-${value}`, value, checked: choice[key] === value, on: { change: () => { choose({ [key]: value }, `chose which ${key} to keep`); update(); } } }),
    ` ${label}`)));

  // the name and position choices are rebuilt only when the stop that stays
  // changes, so choosing one of them keeps keyboard focus where it is
  function update({ regroup = false } = {}) {
    const into = byId[choice.keep], from = other(choice.keep);
    if (regroup) {
      radios(nameBox, "name", "Keep the name of", [["into", `the stop that stays: ${into.name}`], ["from", `the other stop: ${from.name}`]]);
      radios(positionBox, "position", "Keep the position of", [
        ["into", `the stop that stays: ${fmtCoord(into.lat)}, ${fmtCoord(into.lon)}`],
        ["from", `the other stop: ${fmtCoord(from.lat)}, ${fmtCoord(from.lon)}`]]);
    }
    map.showPair(into, from);
    addBtn.textContent = `Add merge to draft: keep ${into.stop_id}`;
    renderImpact(impact, into, from, choice, dist);
  }

  addBtn.addEventListener("click", async () => {
    const into = byId[choice.keep], from = other(choice.keep);
    const moving = byRoute(from);
    const keptName = choice.name === "from" ? from.name : into.name;
    const ok = await confirmDialog("Add this merge to your draft?",
      `${from.name} (${from.stop_id}) is merged into ${into.name} (${into.stop_id}). Stop id ${into.stop_id} stays, named ${keptName}, at the position of ${choice.position === "from" ? from.stop_id : into.stop_id}. `
      + `${moving.size ? `${plural(moving.size, "route")} switch to it. ` : ""}${from.stop_id} is removed when the draft is committed.`,
      { confirm: "Add merge to draft" });
    if (!ok || !(await requireDraft())) return;
    try {
      const res = await addChange({
        entity: "stop", op: "merge", entity_key: from.stop_id, base_row_version: from.row_version,
        after: { into_stop_id: into.stop_id, into_row_version: into.row_version, keep_name: choice.name, keep_position: choice.position },
      }, { merge: false });
      if (res) { history.clear(); done(res, into, from); }
    } catch (e) {
      toast(e.message, "error");
    }
  });

  const inset = h("div.inset.inset-small");
  const row = (label, fa, fb, { differ = false } = {}) => h("tr", { class: differ ? "differs" : "" }, h("th", { scope: "row" }, label), h("td", fa), h("td", fb));
  const station = (s) => [s.parent_station ? `in station ${s.parent_station}` : null, s.platform_code ? `platform ${s.platform_code}` : null].filter(Boolean).join(", ") || "none";
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/merge/${enc(a.stop_id)}` }, "Choose a different stop"),
      h("h1", "Merge duplicate stops"),
      h("p.hint", "Check these are the same kerb, then choose which stop id stays.")),
    h("section.section",
      h("h2", "Compare"),
      h("div.table-wrap", h("table.compare",
        h("thead", h("tr", h("th", ""), h("th", { scope: "col" }, "This stop"), h("th", { scope: "col" }, "The duplicate"))),
        h("tbody",
          row("Name", a.name, b.name, { differ: namesDiffer }),
          row("Stop id", a.stop_id, b.stop_id),
          row("Position", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`, `${fmtCoord(b.lat)}, ${fmtCoord(b.lon)}`, { differ: dist >= 1 }),
          row("Routes", `${plural(routeCount(a), "route")}, ${plural(a.routes.length, "row")}`, `${plural(routeCount(b), "route")}, ${plural(b.routes.length, "row")}`),
          row("Station, platform", station(a), station(b), { differ: station(a) !== station(b) }),
          row("Tamil name", a.regional_name || "none", b.regional_name || "none", { differ: (a.regional_name || "") !== (b.regional_name || "") }),
          row("Position from", a.position_source || "unknown", b.position_source || "unknown")))),
      h("p", h("strong", `They are ${fmtMetres(dist)} apart.`), dist > FAR_METRES ? " That is far for a duplicate; check it is the same place." : ""),
      inset),
    h("section.section",
      h("fieldset.choice-group",
        h("legend", h("span.legend-title", "Which stop id should stay?")),
        h("div.radio-cards", card(a), card(b))),
      nameBox,
      positionBox,
      history.buttons()),
    h("section.section",
      h("h2", "What changes"),
      impact),
    h("div.sticky-actions",
      h("p.hint", "Nothing changes for passengers until the draft is approved by someone else and committed."),
      h("div.btn-row", addBtn, h("a.btn.secondary", { href: `#/stop/${enc(a.stop_id)}` }, "Cancel"))));
  map.inset(inset, {
    points: [{ lat: a.lat, lon: a.lon, label: a.stop_id }, { lat: b.lat, lon: b.lon, label: b.stop_id, color: "#1f5fbf" }],
    lines: [{ pts: [[a.lat, a.lon], [b.lat, b.lon]], color: "#14252a", dashed: true, weight: 2 }],
    maxZoom: 19,
  });
  update({ regroup: true });
}

function renderImpact(box, into, from, choice, dist) {
  const moving = byRoute(from);
  const staying = byRoute(into);
  const keptName = choice.name === "from" ? from.name : into.name;
  const clashes = [], twice = [];
  for (const r of moving.values()) {
    const mine = staying.get(r.route_id);
    if (!mine) continue;
    const adjacent = r.sequences.some((x) => mine.sequences.some((y) => Math.abs(x - y) === 1));
    (adjacent ? clashes : twice).push({ ...r, all: [...r.sequences, ...mine.sequences].sort((x, y) => x - y) });
  }
  const list = [...moving.values()];
  clear(box,
    list.length
      ? h("p", h("strong", `${plural(list.length, "route")} (${plural(from.routes.length, "stop")} in route lists) will switch from ${from.stop_id} to ${into.stop_id}.`))
      : h("p", h("strong", `No route uses ${from.stop_id}. It is simply removed.`)),
    clashes.map((r) => h("p.notice.error", { role: "alert" },
      `Route ${routeName(r)} calls at both stops one after the other (${stopsWord(r.all)}). After the merge it would stop at ${keptName} twice in a row, which is not allowed. Fix that route's stop list first, or do not merge these stops.`)),
    twice.map((r) => h("p.notice.warning",
      `Route ${routeName(r)} calls at both stops (${stopsWord(r.all)}). After the merge it calls at ${keptName} more than once.`)),
    dist > FAR_METRES ? h("p.notice.warning", `The stops are ${fmtMetres(dist)} apart. Duplicates are usually a few metres apart.`) : null,
    from.parent_station ? h("p.notice.warning", `${from.stop_id} is a platform of station ${from.parent_station}; after the merge it is gone from that station.`) : null,
    list.length ? h("ul.list.route-switch", list.slice(0, 60).map((r) => h("li.list-item",
      h("span.key", r.short_name || r.route_id),
      h("span", r.long_name || `Route ${r.route_id}`),
      h("span.hint", stopsWord(r.sequences))))) : null,
    list.length > 60 ? h("p.hint", `and ${list.length - 60} more routes.`) : null);
}

// ------------------------------------------------------------------ step 3: in the draft
function done(res, into, from) {
  const ch = res.draft.changes.find((c) => c.change_id === res.changeId) || {};
  const affected = (ch.before && ch.before.affected) || [];
  const errors = res.problems.filter((p) => p.level === "error");
  const warnings = res.problems.filter((p) => p.level !== "error");
  const names = new Map(affected.map((r) => [r.route_id, r]));
  // say a back-to-back problem in plain words when the server gives the route and positions
  const plain = (p) => {
    const d = p.details || {};
    if (p.code === "merge_would_repeat_stop" && d.route_id && Array.isArray(d.sequences) && d.sequences.length >= 2) {
      const r = names.get(d.route_id) || { route_id: d.route_id };
      return `Route ${routeName(r)} would stop at the same stop twice in a row, at ${stopsWord(d.sequences.slice(0, 2))}. Fix that route's stop list, or remove this merge.`;
    }
    return p.message;
  };
  clear(panel(),
    h("section.section",
      errors.length
        ? h("div.notice.error", { role: "alert" },
            h("p", h("strong", "The merge is in your draft, but the draft cannot be submitted like this")),
            h("ul", errors.map((p) => h("li", plain(p)))))
        : h("p.notice.ok", { role: "status" }, h("strong", "Merge added to your draft")),
      h("div.title-block",
        h("h1", `${from.name} (${from.stop_id}) merged into ${into.name} (${into.stop_id})`),
        h("p", `${plural(affected.length, "route")} updated in draft "${res.draft.title}".`)),
      warnings.length ? h("div.notice.warning", h("p", h("strong", "Please check")), h("ul", warnings.map((p) => h("li", plain(p))))) : null,
      affected.length ? h("ul.list", affected.slice(0, 40).map((r) => h("li.list-item",
        h("span.key", r.short_name || r.route_id),
        h("a", { href: `#/route/${enc(r.route_id)}?draft=1` }, r.long_name || `Route ${r.route_id}`),
        h("span.hint", stopsWord(r.sequences))))) : null,
      h("div.btn-row",
        h("a.btn", { href: `#/drafts/${enc(res.draft.change_set_id)}` }, "Open the draft"),
        errors.length ? h("button.btn.danger", { type: "button", on: { click: async () => {
          try {
            await removeChange(res.changeId);
            toast("The merge was removed from the draft.");
            location.hash = `#/stop/${enc(into.stop_id)}`;
          } catch (e) {
            toast(e.message, "error");
          }
        } } }, "Remove the merge from the draft") : null,
        h("a.btn.secondary", { href: `#/stop/${enc(into.stop_id)}` }, "Back to the stop"))));
  map.showPair(into, from);
}
