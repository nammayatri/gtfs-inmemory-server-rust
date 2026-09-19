// Merging two stations: choose the other station, compare the two side by side,
// choose which station id stays, see which platforms move, and add the merge to
// a draft. On commit every platform of the station that goes becomes a platform
// of the one that stays - keeping its own label - and the other station is
// removed. Route stop lists never name a station, so no route row moves.
import { get, enc } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, confirmDialog, fmtCoord, fmtMetres, haversine, plural } from "./util.js";
import * as map from "./map.js";
import { addChange, requireDraft, removeChange } from "./drafts.js";
import { stopPicker } from "./picker.js";
import { undoScope } from "./undo.js";
import { nameHere } from "./trail.js";

const panel = () => document.getElementById("panel");
const detail = (id) => get(`feeds/${enc(state.feedId)}/stops/${enc(id)}`);
// The server warns past this; `build_stations` groups same-named stops within a
// 500 m diameter, so two stations that are one place are inside it.
const FAR_METRES = 500;
const CANDIDATE_DEGREES = 0.012;   // about 1.3 km each way

export async function showStationMerge(stationId, withId = null) {
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  setLeaveGuard(null);
  clear(panel(), h("section.section", h("p.empty", "Loading…")));
  const back = h("a.crumb", { href: `#/stop/${enc(stationId)}` }, "Back to the station");
  if (!can("editor")) {
    return clear(panel(), h("section.section", back, h("p.notice", "Merging stations needs the editor role. Ask an admin.")));
  }
  let a, b = null;
  try {
    [a, b] = await Promise.all([detail(stationId), withId ? detail(withId) : null]);
  } catch (e) {
    return clear(panel(), h("section.section", back, h("p.notice.error", e.message)));
  }
  const refuse = (s) => (s.location_type !== 1 ? `${s.name} (${s.stop_id}) is a stop, not a station. To merge two stops, use “Merge with a duplicate”.`
    : s.deleted ? `${s.name} (${s.stop_id}) has been removed already.` : null);
  const problem = refuse(a) || (b && refuse(b))
    || (b && b.stop_id === a.stop_id ? "Choose a different station to merge with." : null)
    || (b && b.parent_station ? `${b.name} (${b.stop_id}) is itself inside station ${b.parent_station}, so it cannot be the station that stays.` : null);
  if (problem) return clear(panel(), h("section.section", back, h("p.notice.error", problem)));
  if (!b) return chooseOther(a);
  compare(a, b);
}

// ------------------------------------------------------------------ step 1: which station
async function chooseOther(a) {
  map.focusStop(a);
  const go = (id) => { location.hash = `#/station-merge/${enc(a.stop_id)}?with=${enc(id)}`; };
  const nearby = h("div", h("p.empty", "Looking for stations close by…"));
  const picker = stopPicker({
    title: "Search by name or station id, or pick it on the map",
    kind: "station",
    near: { lat: a.lat, lon: a.lon, label: "this station" },
    exclude: [a.stop_id],
    excludeReason: "that is the station being merged. Choose the other one.",
    includeDraft: false,
    suggest: true,
    mapMessage: `Click the station that is the same place as ${a.name}.`,
    onPick: (s) => go(s.stop_id),
  });
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/stop/${enc(a.stop_id)}` }, "Back to the station"),
      h("div.title-block",
        h("h1", `Merge ${a.name} into another station`),
        h("p.ids", `Station ${a.stop_id}, ${plural((a.children || []).length, "platform")}`)),
      h("p.hint", "Merge two stations only when they are one place entered twice. One station id stays; every platform of the other becomes a platform of it, keeping its own label, and the other station is removed when the draft is committed. No route changes: a route calls at a platform, never at a station.")),
    h("section.section",
      h("h2", "Stations close by"),
      nearby),
    h("section.section",
      h("h2", "Or find the other station"),
      picker.el));

  const d = CANDIDATE_DEGREES;
  try {
    const page = await get(`feeds/${enc(state.feedId)}/stops?station=true&bbox=${[a.lat - d, a.lon - d, a.lat + d, a.lon + d].map((x) => x.toFixed(6)).join(",")}&limit=300`);
    const found = page.items
      .filter((s) => s.stop_id !== a.stop_id && s.location_type === 1)
      .map((s) => ({ s, d: haversine(a.lat, a.lon, s.lat, s.lon), same: s.name === a.name }))
      .sort((x, y) => y.same - x.same || x.d - y.d)
      .slice(0, 12);
    clear(nearby, found.length
      ? [h("p.hint", `Same names first, then the closest. A station that is really this one is usually within ${fmtMetres(FAR_METRES)}.`),
         h("ul.list", found.map(({ s, d: dist, same }) => h("li.list-item",
           h("span.key", fmtMetres(dist)),
           h("span", h("strong", s.name), " ", same ? h("span.chip", "Same name") : null),
           h("button.btn.secondary.small", { type: "button", "aria-label": `Compare with ${s.name} (${s.stop_id})`, on: { click: () => go(s.stop_id) } }, "Compare"),
           h("span.sub", [s.stop_id, plural(s.platform_count ?? 0, "platform")].join(" · ")))))]
      : h("p.empty", "No other station close by. Search for it below."));
  } catch (e) {
    clear(nearby, h("p.notice.error", e.message));
  }
}

// ------------------------------------------------------------------ step 2: compare and choose
const platforms = (s) => s.children || [];
const platformRoutes = (s) => platforms(s).reduce((n, p) => n + (p.route_count || 0), 0);

function compare(a, b) {
  // the station with more platforms is the one to keep, so fewer rows move
  const suggested = platforms(b).length > platforms(a).length ? b.stop_id : a.stop_id;
  const choice = { keep: suggested, name: "into", position: "into" };
  const byId = { [a.stop_id]: a, [b.stop_id]: b };
  nameHere(`Merge ${a.name}`);
  // which id, name and point stay are choices on the screen until the merge is
  // added to a draft: each is one step to undo
  const history = undoScope("this station merge");
  const choose = (patch, label) => {
    const before = { ...choice }, after = { ...choice, ...patch };
    const put = (c) => {
      Object.assign(choice, c);
      const radio = document.getElementById(`keep-station-${choice.keep}`);
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

  const card = (s) => h("label.radio-card", { for: `keep-station-${s.stop_id}` },
    h("input", { type: "radio", name: "keep-station-id", id: `keep-station-${s.stop_id}`, value: s.stop_id, checked: s.stop_id === choice.keep,
      on: { change: () => { choose({ keep: s.stop_id, name: "into", position: "into" }, `chose to keep ${s.stop_id}`); update({ regroup: true }); } } }),
    h("span.radio-card-body",
      h("span.radio-card-title", `Keep station id ${s.stop_id}`),
      h("span", s.name),
      h("span.hint", `${plural(platforms(s).length, "platform")}, ${plural(platformRoutes(s), "route")} through them`),
      s.stop_id === suggested ? h("span.chip", "Suggested: has more platforms") : null));

  const radios = (box, key, legend, options) => clear(box, h("legend", legend), options.map(([value, label]) => h("label.check", { for: `station-${key}-${value}` },
    h("input", { type: "radio", name: `station-${key}`, id: `station-${key}-${value}`, value, checked: choice[key] === value, on: { change: () => { choose({ [key]: value }, `chose which ${key} to keep`); update(); } } }),
    ` ${label}`)));

  // the name and position choices are rebuilt only when the station that stays
  // changes, so choosing one of them keeps keyboard focus where it is
  function update({ regroup = false } = {}) {
    const into = byId[choice.keep], from = other(choice.keep);
    if (regroup) {
      radios(nameBox, "name", "Keep the name of", [["into", `the station that stays: ${into.name}`], ["from", `the other station: ${from.name}`]]);
      radios(positionBox, "position", "Keep the point of", [
        ["into", `the station that stays: ${fmtCoord(into.lat)}, ${fmtCoord(into.lon)}`],
        ["from", `the other station: ${fmtCoord(from.lat)}, ${fmtCoord(from.lon)}`]]);
    }
    map.showPair(into, from);
    addBtn.textContent = `Add merge to draft: keep ${into.stop_id}`;
    renderImpact(impact, into, from, choice, dist);
  }

  addBtn.addEventListener("click", async () => {
    const into = byId[choice.keep], from = other(choice.keep);
    const moving = platforms(from);
    const keptName = choice.name === "from" ? from.name : into.name;
    const ok = await confirmDialog("Add this station merge to your draft?",
      `${from.name} (${from.stop_id}) is merged into ${into.name} (${into.stop_id}). Station id ${into.stop_id} stays, named ${keptName}, at the point of ${choice.position === "from" ? from.stop_id : into.stop_id}. `
      + `${moving.length ? `${plural(moving.length, "platform")} move${moving.length === 1 ? "s" : ""} to it, keeping ${moving.length === 1 ? "its own label" : "their own labels"}. ` : "It has no platforms, so nothing moves. "}`
      + `Station id ${from.stop_id} is removed when the draft is committed, and no route changes.`,
      { confirm: "Add merge to draft" });
    if (!ok || !(await requireDraft())) return;
    try {
      const res = await addChange({
        entity: "station", op: "merge", entity_key: from.stop_id, base_row_version: from.row_version,
        after: { into_station_id: into.stop_id, into_row_version: into.row_version, keep_name: choice.name, keep_position: choice.position },
      }, { merge: false });
      if (res) { history.clear(); done(res, into, from); }
    } catch (e) {
      toast(e.message, "error");
    }
  });

  const inset = h("div.inset.inset-small");
  const row = (label, fa, fb, { differ = false } = {}) => h("tr", { class: differ ? "differs" : "" }, h("th", { scope: "row" }, label), h("td", fa), h("td", fb));
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/station-merge/${enc(a.stop_id)}` }, "Choose a different station"),
      h("h1", "Merge two stations"),
      h("p.hint", "Check these are one place, then choose which station id stays.")),
    h("section.section",
      h("h2", "Compare"),
      h("div.table-wrap", h("table.compare",
        h("thead", h("tr", h("th", ""), h("th", { scope: "col" }, "This station"), h("th", { scope: "col" }, "The other station"))),
        h("tbody",
          row("Name", a.name, b.name, { differ: namesDiffer }),
          row("Station id", a.stop_id, b.stop_id),
          row("Point", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`, `${fmtCoord(b.lat)}, ${fmtCoord(b.lon)}`, { differ: dist >= 1 }),
          row("Platforms", plural(platforms(a).length, "platform"), plural(platforms(b).length, "platform")),
          row("Routes through them", plural(platformRoutes(a), "route"), plural(platformRoutes(b), "route")),
          row("Description", a.description || "none", b.description || "none", { differ: (a.description || "") !== (b.description || "") })))),
      h("p", h("strong", `Their points are ${fmtMetres(dist)} apart.`), dist > FAR_METRES ? ` That is more than the ${fmtMetres(FAR_METRES)} a station is grouped within; check they are one place.` : ""),
      inset),
    h("section.section",
      h("fieldset.choice-group",
        h("legend", h("span.legend-title", "Which station id should stay?")),
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
    points: [{ lat: a.lat, lon: a.lon, label: a.stop_id }, { lat: b.lat, lon: b.lon, label: b.stop_id, color: "#1f5fbf" },
      ...platforms(a).map((p) => ({ lat: p.lat, lon: p.lon, label: p.stop_id })),
      ...platforms(b).map((p) => ({ lat: p.lat, lon: p.lon, label: p.stop_id, color: "#1f5fbf" }))],
    lines: [{ pts: [[a.lat, a.lon], [b.lat, b.lon]], color: "#14252a", dashed: true, weight: 2 }],
    maxZoom: 19,
  });
  update({ regroup: true });
}

// Which platforms move, with the routes through each: what the reviewer and the
// person adding the merge both need to see before agreeing to it.
function renderImpact(box, into, from, choice, dist) {
  const moving = platforms(from);
  const staying = platforms(into);
  const keptName = choice.name === "from" ? from.name : into.name;
  const clashes = moving.filter((m) => staying.some((s) => (s.platform_code || "") && s.platform_code === m.platform_code));
  clear(box,
    moving.length
      ? h("p", h("strong", `${plural(moving.length, "platform")} (${plural(moving.reduce((n, p) => n + (p.route_count || 0), 0), "route")}) move${moving.length === 1 ? "s" : ""} from ${from.stop_id} to ${into.stop_id}.`),
        ` ${keptName} will have ${plural(moving.length + staying.length, "platform")}.`)
      : h("p", h("strong", `${from.stop_id} has no platforms. The merge only removes it.`)),
    h("p.hint", "No route changes: a route's stop list names platforms, and every platform keeps its own id and its own label."),
    dist > FAR_METRES ? h("p.notice.warning", `The two station points are ${fmtMetres(dist)} apart, more than the ${fmtMetres(FAR_METRES)} a station is grouped within. Check they are one place.`) : null,
    from.name !== into.name ? h("p.notice.warning", `The names differ: ${from.name} and ${into.name}. The station that stays will be called ${keptName}.`) : null,
    clashes.length ? h("p.notice.warning", `${plural(clashes.length, "moving platform")} would share a label with a platform already there (${clashes.map((c) => `“${c.platform_code}”`).join(", ")}). Labels are kept as they are; relabel them afterwards if that reads badly.`) : null,
    moving.length ? h("ul.list.platform-move", moving.slice(0, 60).map((p) => h("li.list-item",
      h("span.key", p.platform_code || "no label"),
      h("span", p.name),
      h("span.hint", plural(p.route_count || 0, "route")),
      h("span.sub", p.stop_id)))) : null,
    moving.length > 60 ? h("p.hint", `and ${moving.length - 60} more platforms.`) : null);
}

// ------------------------------------------------------------------ step 3: in the draft
function done(res, into, from) {
  const ch = res.draft.changes.find((c) => c.change_id === res.changeId) || {};
  const moved = (ch.before && ch.before.moving_platforms) || [];
  const errors = res.problems.filter((p) => p.level === "error");
  const warnings = res.problems.filter((p) => p.level !== "error");
  clear(panel(),
    h("section.section",
      errors.length
        ? h("div.notice.error", { role: "alert" },
            h("p", h("strong", "The merge is in your draft, but the draft cannot be submitted like this")),
            h("ul", errors.map((p) => h("li", p.message))))
        : h("p.notice.ok", { role: "status" }, h("strong", "Station merge added to your draft")),
      h("div.title-block",
        h("h1", `${from.name} (${from.stop_id}) merged into ${into.name} (${into.stop_id})`),
        h("p", `${plural(moved.length, "platform")} move${moved.length === 1 ? "s" : ""} in draft “${res.draft.title}”.`)),
      warnings.length ? h("div.notice.warning", h("p", h("strong", "Please check")), h("ul", warnings.map((p) => h("li", p.message)))) : null,
      moved.length ? h("ul.list", moved.slice(0, 40).map((p) => h("li.list-item",
        h("span.key", p.platform_code || "no label"),
        h("a", { href: `#/stop/${enc(p.stop_id)}` }, p.name || p.stop_id),
        h("span.hint", plural(p.route_count || 0, "route")),
        h("span.sub", p.stop_id)))) : h("p.empty", "It had no platforms, so nothing moved."),
      h("div.btn-row",
        h("a.btn", { href: `#/drafts/${enc(res.draft.change_set_id)}` }, "Open the draft"),
        errors.length ? h("button.btn.danger", { type: "button", on: { click: async () => {
          try {
            await removeChange(res.changeId);
            toast("The station merge was removed from the draft.");
            location.hash = `#/stop/${enc(into.stop_id)}`;
          } catch (e) {
            toast(e.message, "error");
          }
        } } }, "Remove the merge from the draft") : null,
        h("a.btn.secondary", { href: `#/stop/${enc(into.stop_id)}` }, "Back to the station"))));
  map.showPair(into, from);
}
