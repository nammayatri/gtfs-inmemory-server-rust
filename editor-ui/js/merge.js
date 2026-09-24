// Merging duplicate stops: choose the duplicates (one or more), compare them side
// by side, choose which stop id stays, see which routes switch, and add the
// merges to a draft - one stop/merge change per stop that goes, all into the one
// that stays. On commit every route row on a stop that goes switches to the one
// that stays, and the other stops are removed.
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
// merged in one go; more than this is a cleanup for the bulk tools
const MAX_GROUP = 10;

// `withIds`: the `with` parameter, one stop id or several separated by commas
export async function showMerge(stopId, withIds = null) {
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  setLeaveGuard(null);
  clear(panel(), h("section.section", h("p.empty", "Loading…")));
  const back = h("a.crumb", { href: `#/stop/${enc(stopId)}` }, "Back to the stop");
  if (!can("editor")) {
    return clear(panel(), h("section.section", back, h("p.notice", "Merging stops needs the editor role. Ask an admin.")));
  }
  const others = [...new Set((withIds || "").split(",").map((s) => s.trim()).filter(Boolean))];
  let a, rest;
  try {
    [a, ...rest] = await Promise.all([detail(stopId), ...others.map(detail)]);
  } catch (e) {
    return clear(panel(), h("section.section", back, h("p.notice.error", e.message)));
  }
  const refuse = (s) => (s.location_type === 1 ? `${s.name} is a station. Merge the stops in it instead.` : s.deleted ? `${s.name} (${s.stop_id}) has been removed already.` : null);
  const problem = [a, ...rest].map(refuse).find(Boolean)
    || (rest.some((s) => s.stop_id === a.stop_id) ? "Choose a different stop to merge with." : null)
    || (rest.length + 1 > MAX_GROUP ? `Merge at most ${MAX_GROUP} stops at a time.` : null);
  if (problem) return clear(panel(), h("section.section", back, h("p.notice.error", problem)));
  if (!rest.length) return chooseDuplicates(a);
  compare([a, ...rest]);
}

// ------------------------------------------------------------------ step 1: which stops
async function chooseDuplicates(a) {
  map.focusStop(a);
  // chosen duplicates, in the order they were chosen
  const chosen = new Map();
  const go = () => { location.hash = `#/merge/${enc(a.stop_id)}?with=${[...chosen.keys()].map(enc).join(",")}`; };
  let found = null, nearbyError = null;

  // three parts, each redrawn on its own: the chosen list, the stops close by
  // (which arrive later) and the picker (which closes after each pick, so a
  // fresh one follows it)
  const chosenBox = h("div"), nearbyBox = h("div", h("p.empty", "Looking for stops close by…")), pickerBox = h("div");

  const toggle = (s) => {
    if (chosen.has(s.stop_id)) chosen.delete(s.stop_id);
    else if (chosen.size + 2 > MAX_GROUP) return toast(`Merge at most ${MAX_GROUP} stops at a time.`, "error");
    else chosen.set(s.stop_id, s);
    drawChosen();
    drawNearby();
  };

  const drawChosen = () => {
    const n = chosen.size;
    clear(chosenBox,
      h("h2", n ? `Chosen: ${plural(n, "duplicate")}` : "Chosen duplicates"),
      n
        ? h("ul.list", [...chosen.values()].map((s) => h("li.list-item",
            h("span.key", fmtMetres(haversine(a.lat, a.lon, s.lat, s.lon))),
            h("span", h("strong", s.name)),
            h("button.btn.quiet.small", { type: "button", "aria-label": `Remove ${s.name} (${s.stop_id})`, on: { click: () => toggle(s) } }, "Remove"),
            h("span.sub", [s.stop_id, s.route_count != null ? plural(s.route_count, "route") : null].filter(Boolean).join(" · ")))))
        : h("p.empty", "None yet. Add them from the stops close by, or find them below."),
      h("div.btn-row",
        h("button.btn", { type: "button", disabled: !n, on: { click: go } },
          n ? `Compare ${plural(n + 1, "stop")}` : "Compare")));
  };

  const drawNearby = () => {
    if (nearbyError) return clear(nearbyBox, h("p.notice.error", nearbyError));
    if (!found) return;
    if (!found.length) return clear(nearbyBox, h("p.empty", `No stop within 60 m, and none with a similar name within ${CANDIDATE_METRES} m. Search for it below.`));
    clear(nearbyBox, h("p.hint", "Same and similar names first, then the closest."),
      h("ul.list", found.map(({ s, d: dist, like }) => {
        const on = chosen.has(s.stop_id);
        return h("li.list-item",
          h("span.key", fmtMetres(dist)),
          h("span", h("strong", s.name), " ", like === 2 ? h("span.chip", "Same name") : like === 1 ? h("span.chip", "Similar name") : null),
          h(on ? "button.btn.small" : "button.btn.secondary.small", {
            type: "button", "aria-pressed": String(on), "aria-label": `${on ? "Remove" : "Add"} ${s.name} (${s.stop_id})`,
            on: { click: () => toggle(s) },
          }, on ? "Added" : "Add"),
          h("span.sub", [s.stop_id, plural(s.route_count, "route"), s.platform_code, s.parent_station ? `in station ${s.parent_station}` : null].filter(Boolean).join(" · ")));
      })));
  };

  const drawPicker = () => {
    const picker = stopPicker({
      title: "Search by name or stop id, or pick it on the map",
      near: { lat: a.lat, lon: a.lon, label: "this stop" },
      exclude: [a.stop_id, ...chosen.keys()],
      excludeReason: "that stop is already in this merge.",
      includeDraft: false,
      suggest: false,
      mapMessage: `Click a stop that duplicates ${a.name}.`,
      onPick: (s) => {
        if (chosen.has(s.stop_id)) toast(`${s.name} (${s.stop_id}) is already chosen.`);
        else toggle(s);
        drawPicker();
      },
    });
    clear(pickerBox, picker.el);
  };

  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/stop/${enc(a.stop_id)}` }, "Back to the stop"),
      h("div.title-block",
        h("h1", `Merge ${a.name} with duplicates`),
        h("p.ids", `Stop ${a.stop_id}, ${plural(new Set(a.routes.map((r) => r.route_id)).size, "route")}`)),
      h("p.hint", "Merge stops only when they are the same kerb entered more than once. Choose every duplicate; one stop id stays, every route that uses the others switches to it, and the others are removed when the draft is committed. Two sides of a road are two stops: club those into a station instead.")),
    h("section.section", chosenBox),
    h("section.section", h("h2", "Stops close by"), nearbyBox),
    h("section.section", h("h2", "Or find a duplicate"), pickerBox));
  drawChosen();
  drawPicker();

  const d = 0.0025;
  try {
    const page = await get(`feeds/${enc(state.feedId)}/stops?bbox=${[a.lat - d, a.lon - d, a.lat + d, a.lon + d].map((x) => x.toFixed(6)).join(",")}&limit=300`);
    found = page.items
      .filter((s) => s.stop_id !== a.stop_id && s.location_type === 0)
      .map((s) => ({ s, d: haversine(a.lat, a.lon, s.lat, s.lon), like: nameLikeness(a.name, s.name) }))
      .filter((x) => x.d <= 60 || (x.like && x.d <= CANDIDATE_METRES))
      .sort((x, y) => y.like - x.like || x.d - y.d)
      .slice(0, 12);
  } catch (e) {
    nearbyError = e.message;
  }
  drawNearby();
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
const station = (s) => [s.parent_station ? `in station ${s.parent_station}` : null, s.platform_code ? `platform ${s.platform_code}` : null].filter(Boolean).join(", ") || "none";
const allSame = (xs) => xs.every((x) => x === xs[0]);

// `stops`: the stop the merge started from, then its duplicates
function compare(stops) {
  const [a] = stops;
  const byId = Object.fromEntries(stops.map((s) => [s.stop_id, s]));
  const suggested = stops.reduce((best, s) => (routeCount(s) > routeCount(best) ? s : best)).stop_id;
  // keep: the stop id that stays; name, position: the stop whose name / point it keeps
  const choice = { keep: suggested, name: suggested, position: suggested };
  nameHere(`Merge ${a.name}`);
  // which id, name and position stay are choices on the screen until the merges
  // are added to a draft: each is one step to undo
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
  const goers = () => stops.filter((s) => s.stop_id !== choice.keep);
  const namesDiffer = !allSame(stops.map((s) => s.name));
  const spread = Math.max(...stops.flatMap((s) => stops.map((t) => haversine(s.lat, s.lon, t.lat, t.lon))));

  const nameBox = h("fieldset.choice-group", { hidden: !namesDiffer });
  const positionBox = h("fieldset.choice-group", { hidden: spread < 1 });
  const impact = h("div", { "aria-live": "polite" });
  const addBtn = h("button.btn", { type: "button" }, "Add merge to draft");

  const card = (s) => h("label.radio-card", { for: `keep-${s.stop_id}` },
    h("input", { type: "radio", name: "keep-id", id: `keep-${s.stop_id}`, value: s.stop_id, checked: s.stop_id === choice.keep,
      on: { change: () => { choose({ keep: s.stop_id, name: s.stop_id, position: s.stop_id }, `chose to keep ${s.stop_id}`); update({ regroup: true }); } } }),
    h("span.radio-card-body",
      h("span.radio-card-title", `Keep stop id ${s.stop_id}`),
      h("span", s.name),
      h("span.hint", `${plural(routeCount(s), "route")}, ${plural(s.routes.length, "stop")} in route lists`),
      s.stop_id === suggested ? h("span.chip", `Suggested: used by ${stops.length === 2 ? "more" : "most"} routes`) : null));

  const radios = (box, key, legend, options) => clear(box, h("legend", legend), options.map(([value, label]) => h("label.check", { for: `${key}-${value}` },
    h("input", { type: "radio", name: key, id: `${key}-${value}`, value, checked: choice[key] === value, on: { change: () => { choose({ [key]: value }, `chose which ${key} to keep`); update(); } } }),
    ` ${label}`)));
  const whose = (s) => (s.stop_id === choice.keep ? "the stop that stays" : s.stop_id);

  // the name and position choices are rebuilt only when the stop that stays
  // changes, so choosing one of them keeps keyboard focus where it is
  function update({ regroup = false } = {}) {
    const into = byId[choice.keep], from = goers();
    if (regroup) {
      // one option per distinct name; the stop that stays first
      const named = [into, ...from].filter((s, i, all) => all.findIndex((t) => t.name === s.name) === i);
      radios(nameBox, "name", "Keep the name", named.map((s) => [s.stop_id, `${s.name} (${whose(s)})`]));
      radios(positionBox, "position", "Keep the position of", [into, ...from].map((s) => [s.stop_id, `${whose(s)}: ${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`]));
    }
    map.showGroup(into, from);
    addBtn.textContent = from.length === 1 ? `Add merge to draft: keep ${into.stop_id}` : `Add ${from.length} merges to draft: keep ${into.stop_id}`;
    renderImpact(impact, into, from, byId[choice.name].name);
  }

  addBtn.addEventListener("click", async () => {
    const into = byId[choice.keep], from = goers();
    const moving = new Set(from.flatMap((s) => s.routes.map((r) => r.route_id)));
    const keptName = byId[choice.name].name;
    const ok = await confirmDialog(from.length === 1 ? "Add this merge to your draft?" : `Add these ${from.length} merges to your draft?`,
      `${from.map((s) => `${s.name} (${s.stop_id})`).join(", ")} ${from.length === 1 ? "is" : "are"} merged into ${into.name} (${into.stop_id}). Stop id ${into.stop_id} stays, named ${keptName}, at the position of ${choice.position}. `
      + `${moving.size ? `${plural(moving.size, "route")} switch to it. ` : ""}${from.map((s) => s.stop_id).join(", ")} ${from.length === 1 ? "is" : "are"} removed when the draft is committed.`,
      { confirm: from.length === 1 ? "Add merge to draft" : `Add ${from.length} merges to draft` });
    if (!ok || !(await requireDraft())) return;
    // one change per stop that goes. The name and the position come from one
    // stop at most, so only its merge carries "from"; the others keep what the
    // stop that stays has by then. All or nothing: a failure takes back the
    // merges already added.
    const added = [];
    let res = null;
    addBtn.disabled = true;
    try {
      for (const s of from) {
        res = await addChange({
          entity: "stop", op: "merge", entity_key: s.stop_id, base_row_version: s.row_version,
          after: {
            into_stop_id: into.stop_id, into_row_version: into.row_version,
            keep_name: choice.name === s.stop_id ? "from" : "into",
            keep_position: choice.position === s.stop_id ? "from" : "into",
          },
        }, { merge: false, quiet: true });
        if (!res) break;
        added.push(res.changeId);
      }
    } catch (e) {
      toast(e.message, "error");
      res = null;
    }
    if (!res || added.length !== from.length) {
      for (const id of added.reverse()) await removeChange(id).catch(() => {});
      addBtn.disabled = false;
      if (added.length) toast("Nothing was added: the merges already added were taken back out of the draft.", "error");
      return;
    }
    history.clear();
    done(res.draft, added, into, from);
  });

  const inset = h("div.inset.inset-small");
  const row = (label, f, { differ = false } = {}) => h("tr", { class: differ ? "differs" : "" }, h("th", { scope: "row" }, label), stops.map((s) => h("td", f(s))));
  const differs = (f) => !allSame(stops.map(f));
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/merge/${enc(a.stop_id)}` }, "Choose different stops"),
      h("h1", "Merge duplicate stops"),
      h("p.hint", `Check these ${plural(stops.length, "stop")} are the same kerb, then choose which stop id stays.`)),
    h("section.section",
      h("h2", "Compare"),
      h("div.table-wrap", h("table.compare",
        h("thead", h("tr", h("th", ""), stops.map((s, i) => h("th", { scope: "col" }, i === 0 ? "This stop" : stops.length === 2 ? "The duplicate" : `Duplicate ${i}`)))),
        h("tbody",
          row("Name", (s) => s.name, { differ: namesDiffer }),
          row("Stop id", (s) => s.stop_id),
          row("Position", (s) => `${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`, { differ: spread >= 1 }),
          row("Routes", (s) => `${plural(routeCount(s), "route")}, ${plural(s.routes.length, "row")}`),
          row("Station, platform", station, { differ: differs(station) }),
          row("Tamil name", (s) => s.regional_name || "none", { differ: differs((s) => s.regional_name || "") }),
          row("Position from", (s) => s.position_source || "unknown")))),
      h("p", h("strong", stops.length === 2 ? `They are ${fmtMetres(spread)} apart.` : `The farthest two are ${fmtMetres(spread)} apart.`),
        spread > FAR_METRES ? " That is far for a duplicate; check it is the same place." : ""),
      inset),
    h("section.section",
      h("fieldset.choice-group",
        h("legend", h("span.legend-title", "Which stop id should stay?")),
        h("div.radio-cards", stops.map(card))),
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
    points: stops.map((s, i) => ({ lat: s.lat, lon: s.lon, label: s.stop_id, color: i === 0 ? undefined : "#1f5fbf" })),
    lines: stops.slice(1).map((s) => ({ pts: [[a.lat, a.lon], [s.lat, s.lon]], color: "#14252a", dashed: true, weight: 2 })),
    maxZoom: 19,
  });
  update({ regroup: true });
}

function renderImpact(box, into, from, keptName) {
  // every row of every stop in the merge, by route: after the merge they are all
  // the stop that stays, so two of them back to back is a stop called twice in a row
  const group = [into, ...from];
  const rowsByRoute = new Map();
  for (const s of group) {
    for (const r of byRoute(s).values()) {
      if (!rowsByRoute.has(r.route_id)) rowsByRoute.set(r.route_id, { ...r, sequences: [], moving: [] });
      const acc = rowsByRoute.get(r.route_id);
      acc.sequences.push(...r.sequences);
      if (s !== into) acc.moving.push(...r.sequences);
    }
  }
  const clashes = [], twice = [];
  for (const r of rowsByRoute.values()) {
    if (!r.moving.length || r.sequences.length < 2) continue;
    const all = [...r.sequences].sort((x, y) => x - y);
    const adjacent = all.some((x, i) => i > 0 && x - all[i - 1] === 1);
    (adjacent ? clashes : twice).push({ ...r, all });
  }
  const switching = [...rowsByRoute.values()].filter((r) => r.moving.length);
  const movingRows = from.reduce((n, s) => n + s.routes.length, 0);
  const goingIds = from.map((s) => s.stop_id).join(", ");
  const far = from.map((s) => ({ s, d: haversine(into.lat, into.lon, s.lat, s.lon) })).filter((x) => x.d > FAR_METRES);
  clear(box,
    switching.length
      ? h("p", h("strong", `${plural(switching.length, "route")} (${plural(movingRows, "stop")} in route lists) will switch from ${goingIds} to ${into.stop_id}.`))
      : h("p", h("strong", `No route uses ${goingIds}. ${from.length === 1 ? "It is" : "They are"} simply removed.`)),
    clashes.map((r) => h("p.notice.error", { role: "alert" },
      `Route ${routeName(r)} calls at two of these stops one after the other (${stopsWord(r.all)}). After the merge it would stop at ${keptName} twice in a row, which is not allowed. Fix that route's stop list first, or leave one of these stops out of the merge.`)),
    twice.map((r) => h("p.notice.warning",
      `Route ${routeName(r)} calls at more than one of these stops (${stopsWord(r.all)}). After the merge it calls at ${keptName} more than once.`)),
    far.map(({ s, d }) => h("p.notice.warning", `${s.stop_id} is ${fmtMetres(d)} from ${into.stop_id}. Duplicates are usually a few metres apart.`)),
    from.filter((s) => s.parent_station).map((s) => h("p.notice.warning", `${s.stop_id} is a platform of station ${s.parent_station}; after the merge it is gone from that station.`)),
    switching.length ? h("ul.list.route-switch", switching.slice(0, 60).map((r) => h("li.list-item",
      h("span.key", r.short_name || r.route_id),
      h("span", r.long_name || `Route ${r.route_id}`),
      h("span.hint", stopsWord([...r.moving].sort((x, y) => x - y)))))) : null,
    switching.length > 60 ? h("p.hint", `and ${switching.length - 60} more routes.`) : null);
}

// ------------------------------------------------------------------ step 3: in the draft
function done(draft, changeIds, into, from) {
  const mine = new Set(changeIds);
  const changes = draft.changes.filter((c) => mine.has(c.change_id));
  const problems = (draft.validation || []).filter((v) => mine.has(v.change_id));
  // routes that switched, across the merges, with every position that moved
  const affected = new Map();
  for (const c of changes) {
    for (const r of (c.before && c.before.affected) || []) {
      if (!affected.has(r.route_id)) affected.set(r.route_id, { ...r, sequences: [] });
      affected.get(r.route_id).sequences.push(...r.sequences);
    }
  }
  for (const r of affected.values()) r.sequences.sort((x, y) => x - y);
  const errors = problems.filter((p) => p.level === "error");
  const warnings = problems.filter((p) => p.level !== "error");
  // say a back-to-back problem in plain words when the server gives the route and positions
  const plain = (p) => {
    const d = p.details || {};
    if (p.code === "merge_would_repeat_stop" && d.route_id && Array.isArray(d.sequences) && d.sequences.length >= 2) {
      const r = affected.get(d.route_id) || { route_id: d.route_id };
      return `Route ${routeName(r)} would stop at the same stop twice in a row, at ${stopsWord(d.sequences.slice(0, 2))}. Fix that route's stop list, or remove these merges.`;
    }
    return p.message;
  };
  const one = from.length === 1;
  toast(errors.length ? `Added to "${draft.title}" with ${plural(errors.length, "problem")} to fix before submitting.` : `Added to draft "${draft.title}".`, errors.length ? "error" : "");
  clear(panel(),
    h("section.section",
      errors.length
        ? h("div.notice.error", { role: "alert" },
            h("p", h("strong", `The ${one ? "merge is" : "merges are"} in your draft, but the draft cannot be submitted like this`)),
            h("ul", errors.map((p) => h("li", plain(p)))))
        : h("p.notice.ok", { role: "status" }, h("strong", one ? "Merge added to your draft" : `${from.length} merges added to your draft`)),
      h("div.title-block",
        h("h1", `${from.map((s) => `${s.name} (${s.stop_id})`).join(", ")} merged into ${into.name} (${into.stop_id})`),
        h("p", `${plural(affected.size, "route")} updated in draft "${draft.title}".`)),
      warnings.length ? h("div.notice.warning", h("p", h("strong", "Please check")), h("ul", warnings.map((p) => h("li", plain(p))))) : null,
      affected.size ? h("ul.list", [...affected.values()].slice(0, 40).map((r) => h("li.list-item",
        h("span.key", r.short_name || r.route_id),
        h("a", { href: `#/route/${enc(r.route_id)}?draft=1` }, r.long_name || `Route ${r.route_id}`),
        h("span.hint", stopsWord(r.sequences))))) : null,
      h("div.btn-row",
        h("a.btn", { href: `#/drafts/${enc(draft.change_set_id)}` }, "Open the draft"),
        errors.length ? h("button.btn.danger", { type: "button", on: { click: async () => {
          try {
            for (const id of [...changeIds].reverse()) await removeChange(id);
            toast(one ? "The merge was removed from the draft." : "The merges were removed from the draft.");
            location.hash = `#/stop/${enc(into.stop_id)}`;
          } catch (e) {
            toast(e.message, "error");
          }
        } } }, one ? "Remove the merge from the draft" : "Remove these merges from the draft") : null,
        h("a.btn.secondary", { href: `#/stop/${enc(into.stop_id)}` }, "Back to the stop"))));
  map.showGroup(into, from);
}
