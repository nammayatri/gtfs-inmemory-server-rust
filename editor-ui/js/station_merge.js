// Merging stations: choose the other stations (one or more), compare them side by
// side, choose which station id stays, see which platforms move, and add the
// merges to a draft - one station/merge change per station that goes, all into
// the one that stays. On commit every platform of a station that goes becomes a
// platform of the one that stays - keeping its own label - and the other
// stations are removed. Route stop lists never name a station, so no route row
// moves.
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
// merged in one go, as for stops
const MAX_GROUP = 10;

// `withIds`: the `with` parameter, one station id or several separated by commas
export async function showStationMerge(stationId, withIds = null) {
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  setLeaveGuard(null);
  clear(panel(), h("section.section", h("p.empty", "Loading…")));
  const back = h("a.crumb", { href: `#/stop/${enc(stationId)}` }, "Back to the station");
  if (!can("editor")) {
    return clear(panel(), h("section.section", back, h("p.notice", "Merging stations needs the editor role. Ask an admin.")));
  }
  const others = [...new Set((withIds || "").split(",").map((s) => s.trim()).filter(Boolean))];
  let a, rest;
  try {
    [a, ...rest] = await Promise.all([detail(stationId), ...others.map(detail)]);
  } catch (e) {
    return clear(panel(), h("section.section", back, h("p.notice.error", e.message)));
  }
  const refuse = (s) => (s.location_type !== 1 ? `${s.name} (${s.stop_id}) is a stop, not a station. To merge stops, use “Merge with a duplicate”.`
    : s.deleted ? `${s.name} (${s.stop_id}) has been removed already.` : null);
  const all = [a, ...rest];
  const problem = all.map(refuse).find(Boolean)
    || (rest.some((s) => s.stop_id === a.stop_id) ? "Choose a different station to merge with." : null)
    || (all.length > MAX_GROUP ? `Merge at most ${MAX_GROUP} stations at a time.` : null)
    // the one that stays cannot be inside another station
    || (rest.length && all.every((s) => s.parent_station) ? "Every one of these stations is itself inside another station, so none of them can be the station that stays." : null);
  if (problem) return clear(panel(), h("section.section", back, h("p.notice.error", problem)));
  if (!rest.length) return chooseOthers(a);
  compare(all);
}

// ------------------------------------------------------------------ step 1: which stations
async function chooseOthers(a) {
  map.focusStop(a);
  // chosen stations, in the order they were chosen
  const chosen = new Map();
  const go = () => { location.hash = `#/station-merge/${enc(a.stop_id)}?with=${[...chosen.keys()].map(enc).join(",")}`; };
  let found = null, nearbyError = null;
  // three parts, each redrawn on its own: the chosen list, the stations close by
  // (which arrive later) and the picker (which closes after each pick)
  const chosenBox = h("div"), nearbyBox = h("div", h("p.empty", "Looking for stations close by…")), pickerBox = h("div");

  const toggle = (s) => {
    if (chosen.has(s.stop_id)) chosen.delete(s.stop_id);
    else if (chosen.size + 2 > MAX_GROUP) return toast(`Merge at most ${MAX_GROUP} stations at a time.`, "error");
    else chosen.set(s.stop_id, s);
    drawChosen();
    drawNearby();
  };
  const platformsWord = (s) => (s.platform_count != null ? plural(s.platform_count, "platform") : s.children ? plural(s.children.length, "platform") : null);

  const drawChosen = () => {
    const n = chosen.size;
    const total = [a, ...chosen.values()].reduce((sum, s) => sum + (s.platform_count ?? (s.children || []).length), 0);
    clear(chosenBox,
      h("h2", n ? `Chosen: ${plural(n, "other station")}` : "Chosen stations"),
      n
        ? [h("ul.list", [...chosen.values()].map((s) => h("li.list-item",
            h("span.key", fmtMetres(haversine(a.lat, a.lon, s.lat, s.lon))),
            h("span", h("strong", s.name)),
            h("button.btn.quiet.small", { type: "button", "aria-label": `Remove ${s.name} (${s.stop_id})`, on: { click: () => toggle(s) } }, "Remove"),
            h("span.sub", [s.stop_id, platformsWord(s)].filter(Boolean).join(" · "))))),
          h("p.hint", `Merged, the station that stays will have ${plural(total, "platform")}.`)]
        : h("p.empty", "None yet. Add them from the stations close by, or find them below."),
      h("div.btn-row",
        h("button.btn", { type: "button", disabled: !n, on: { click: go } },
          n ? `Compare ${plural(n + 1, "station")}` : "Compare")));
  };

  const drawNearby = () => {
    if (nearbyError) return clear(nearbyBox, h("p.notice.error", nearbyError));
    if (!found) return;
    if (!found.length) return clear(nearbyBox, h("p.empty", "No other station close by. Search for it below."));
    clear(nearbyBox, h("p.hint", `Same names first, then the closest. A station that is really this one is usually within ${fmtMetres(FAR_METRES)}.`),
      h("ul.list", found.map(({ s, d: dist, same }) => {
        const on = chosen.has(s.stop_id);
        return h("li.list-item",
          h("span.key", fmtMetres(dist)),
          h("span", h("strong", s.name), " ", same ? h("span.chip", "Same name") : null),
          h(on ? "button.btn.small" : "button.btn.secondary.small", {
            type: "button", "aria-pressed": String(on), "aria-label": `${on ? "Remove" : "Add"} ${s.name} (${s.stop_id})`,
            on: { click: () => toggle(s) },
          }, on ? "Added" : "Add"),
          h("span.sub", [s.stop_id, plural(s.platform_count ?? 0, "platform")].join(" · ")));
      })));
  };

  const drawPicker = () => {
    const picker = stopPicker({
      title: "Search by name or station id, or pick it on the map",
      kind: "station",
      near: { lat: a.lat, lon: a.lon, label: "this station" },
      exclude: [a.stop_id, ...chosen.keys()],
      excludeReason: "that station is already in this merge.",
      includeDraft: false,
      suggest: true,
      mapMessage: `Click a station that is the same place as ${a.name}.`,
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
      h("a.crumb", { href: `#/stop/${enc(a.stop_id)}` }, "Back to the station"),
      h("div.title-block",
        h("h1", `Merge ${a.name} with other stations`),
        h("p.ids", `Station ${a.stop_id}, ${plural((a.children || []).length, "platform")}`)),
      h("p.hint", "Merge stations only when they are one place entered more than once. Choose every one of them; one station id stays, every platform of the others becomes a platform of it, keeping its own label, and the others are removed when the draft is committed. No route changes: a route calls at a platform, never at a station.")),
    h("section.section", chosenBox),
    h("section.section", h("h2", "Stations close by"), nearbyBox),
    h("section.section", h("h2", "Or find another station"), pickerBox));
  drawChosen();
  drawPicker();

  const d = CANDIDATE_DEGREES;
  try {
    const page = await get(`feeds/${enc(state.feedId)}/stops?station=true&bbox=${[a.lat - d, a.lon - d, a.lat + d, a.lon + d].map((x) => x.toFixed(6)).join(",")}&limit=300`);
    found = page.items
      .filter((s) => s.stop_id !== a.stop_id && s.location_type === 1)
      .map((s) => ({ s, d: haversine(a.lat, a.lon, s.lat, s.lon), same: s.name === a.name }))
      .sort((x, y) => y.same - x.same || x.d - y.d)
      .slice(0, 12);
  } catch (e) {
    nearbyError = e.message;
  }
  drawNearby();
}

// ------------------------------------------------------------------ step 2: compare and choose
const platforms = (s) => s.children || [];
const platformRoutes = (s) => platforms(s).reduce((n, p) => n + (p.route_count || 0), 0);
const allSame = (xs) => xs.every((x) => x === xs[0]);

// `stations`: the station the merge started from, then the others
function compare(stations) {
  const [a] = stations;
  const byId = Object.fromEntries(stations.map((s) => [s.stop_id, s]));
  // a station inside another station cannot be the one that stays
  const keepable = stations.filter((s) => !s.parent_station);
  // the station with most platforms is the one to keep, so fewest rows move
  const suggested = keepable.reduce((best, s) => (platforms(s).length > platforms(best).length ? s : best)).stop_id;
  // keep: the station id that stays; name, position: the station whose name / point it keeps
  const choice = { keep: suggested, name: suggested, position: suggested };
  nameHere(`Merge ${a.name}`);
  // which id, name and point stay are choices on the screen until the merges are
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
  const goers = () => stations.filter((s) => s.stop_id !== choice.keep);
  const namesDiffer = !allSame(stations.map((s) => s.name));
  const spread = Math.max(...stations.flatMap((s) => stations.map((t) => haversine(s.lat, s.lon, t.lat, t.lon))));
  const total = stations.reduce((n, s) => n + platforms(s).length, 0);

  const nameBox = h("fieldset.choice-group", { hidden: !namesDiffer });
  const positionBox = h("fieldset.choice-group", { hidden: spread < 1 });
  const impact = h("div", { "aria-live": "polite" });
  const addBtn = h("button.btn", { type: "button" }, "Add merge to draft");

  const card = (s) => h("label.radio-card", { for: `keep-station-${s.stop_id}` },
    h("input", { type: "radio", name: "keep-station-id", id: `keep-station-${s.stop_id}`, value: s.stop_id, checked: s.stop_id === choice.keep,
      disabled: !!s.parent_station,
      on: { change: () => { choose({ keep: s.stop_id, name: s.stop_id, position: s.stop_id }, `chose to keep ${s.stop_id}`); update({ regroup: true }); } } }),
    h("span.radio-card-body",
      h("span.radio-card-title", `Keep station id ${s.stop_id}`),
      h("span", s.name),
      h("span.hint", `${plural(platforms(s).length, "platform")}, ${plural(platformRoutes(s), "route")} through them`),
      s.parent_station ? h("span.hint", `Inside station ${s.parent_station}, so it cannot stay.`) : null,
      s.stop_id === suggested ? h("span.chip", `Suggested: has ${stations.length === 2 ? "more" : "most"} platforms`) : null));

  const radios = (box, key, legend, options) => clear(box, h("legend", legend), options.map(([value, label]) => h("label.check", { for: `station-${key}-${value}` },
    h("input", { type: "radio", name: `station-${key}`, id: `station-${key}-${value}`, value, checked: choice[key] === value, on: { change: () => { choose({ [key]: value }, `chose which ${key} to keep`); update(); } } }),
    ` ${label}`)));
  const whose = (s) => (s.stop_id === choice.keep ? "the station that stays" : s.stop_id);

  // the name and point choices are rebuilt only when the station that stays
  // changes, so choosing one of them keeps keyboard focus where it is
  function update({ regroup = false } = {}) {
    const into = byId[choice.keep], from = goers();
    if (regroup) {
      // one option per distinct name; the station that stays first
      const named = [into, ...from].filter((s, i, all) => all.findIndex((t) => t.name === s.name) === i);
      radios(nameBox, "name", "Keep the name", named.map((s) => [s.stop_id, `${s.name} (${whose(s)})`]));
      radios(positionBox, "position", "Keep the point of", [into, ...from].map((s) => [s.stop_id, `${whose(s)}: ${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`]));
    }
    map.showGroup(into, from);
    addBtn.textContent = from.length === 1 ? `Add merge to draft: keep ${into.stop_id}` : `Add ${from.length} merges to draft: keep ${into.stop_id}`;
    renderImpact(impact, into, from, byId[choice.name].name);
  }

  addBtn.addEventListener("click", async () => {
    const into = byId[choice.keep], from = goers();
    const moving = from.reduce((n, s) => n + platforms(s).length, 0);
    const keptName = byId[choice.name].name;
    const one = from.length === 1;
    const ok = await confirmDialog(one ? "Add this station merge to your draft?" : `Add these ${from.length} station merges to your draft?`,
      `${from.map((s) => `${s.name} (${s.stop_id})`).join(", ")} ${one ? "is" : "are"} merged into ${into.name} (${into.stop_id}). Station id ${into.stop_id} stays, named ${keptName}, at the point of ${choice.position}. `
      + `${moving ? `${plural(moving, "platform")} move${moving === 1 ? "s" : ""} to it, keeping ${moving === 1 ? "its own label" : "their own labels"}, so it will have ${plural(total, "platform")}. ` : "They have no platforms, so nothing moves. "}`
      + `Station id${one ? "" : "s"} ${from.map((s) => s.stop_id).join(", ")} ${one ? "is" : "are"} removed when the draft is committed, and no route changes.`,
      { confirm: one ? "Add merge to draft" : `Add ${from.length} merges to draft` });
    if (!ok || !(await requireDraft())) return;
    // one change per station that goes. The name and the point come from one
    // station at most, so only its merge carries "from"; the others keep what the
    // station that stays has by then. All or nothing: a failure takes back the
    // merges already added.
    const added = [];
    let res = null;
    addBtn.disabled = true;
    try {
      for (const s of from) {
        res = await addChange({
          entity: "station", op: "merge", entity_key: s.stop_id, base_row_version: s.row_version,
          after: {
            into_station_id: into.stop_id, into_row_version: into.row_version,
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
  const row = (label, f, { differ = false } = {}) => h("tr", { class: differ ? "differs" : "" }, h("th", { scope: "row" }, label), stations.map((s) => h("td", f(s))));
  const colors = ["#0b6660", "#1f5fbf", "#7a5000", "#b42318", "#536569"];
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: `#/station-merge/${enc(a.stop_id)}` }, "Choose different stations"),
      h("h1", stations.length === 2 ? "Merge two stations" : `Merge ${stations.length} stations`),
      h("p.hint", "Check these are one place, then choose which station id stays.")),
    h("section.section",
      h("h2", "Compare"),
      h("div.table-wrap", h("table.compare",
        h("thead", h("tr", h("th", ""), stations.map((s, i) => h("th", { scope: "col" }, i === 0 ? "This station" : stations.length === 2 ? "The other station" : `Station ${i + 1}`)))),
        h("tbody",
          row("Name", (s) => s.name, { differ: namesDiffer }),
          row("Station id", (s) => s.stop_id),
          row("Point", (s) => `${fmtCoord(s.lat)}, ${fmtCoord(s.lon)}`, { differ: spread >= 1 }),
          row("Platforms", (s) => plural(platforms(s).length, "platform")),
          row("Routes through them", (s) => plural(platformRoutes(s), "route")),
          row("Description", (s) => s.description || "none", { differ: !allSame(stations.map((s) => s.description || "")) })))),
      h("p", h("strong", stations.length === 2 ? `Their points are ${fmtMetres(spread)} apart.` : `The farthest two points are ${fmtMetres(spread)} apart.`),
        spread > FAR_METRES ? ` That is more than the ${fmtMetres(FAR_METRES)} a station is grouped within; check they are one place.` : ""),
      inset),
    h("section.section",
      h("fieldset.choice-group",
        h("legend", h("span.legend-title", "Which station id should stay?")),
        h("div.radio-cards", stations.map(card))),
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
    points: stations.flatMap((s, i) => [
      { lat: s.lat, lon: s.lon, label: s.stop_id, color: colors[i % colors.length] },
      ...platforms(s).map((p) => ({ lat: p.lat, lon: p.lon, label: p.stop_id, color: colors[i % colors.length] }))]),
    lines: stations.slice(1).map((s) => ({ pts: [[a.lat, a.lon], [s.lat, s.lon]], color: "#14252a", dashed: true, weight: 2 })),
    maxZoom: 19,
  });
  update({ regroup: true });
}

// Which platforms move, with the routes through each: what the reviewer and the
// person adding the merges both need to see before agreeing to them.
function renderImpact(box, into, from, keptName) {
  const moving = from.flatMap((s) => platforms(s).map((p) => ({ ...p, from_station: s.stop_id })));
  const staying = platforms(into);
  // labels shared across all the platforms the station will have
  const seen = new Map();
  for (const p of [...staying, ...moving]) if (p.platform_code) seen.set(p.platform_code, (seen.get(p.platform_code) || 0) + 1);
  const shared = [...seen].filter(([, n]) => n > 1).map(([code]) => code);
  const far = from.map((s) => ({ s, d: haversine(into.lat, into.lon, s.lat, s.lon) })).filter((x) => x.d > FAR_METRES);
  const ids = from.map((s) => s.stop_id).join(", ");
  clear(box,
    moving.length
      ? h("p", h("strong", `${plural(moving.length, "platform")} (${plural(moving.reduce((n, p) => n + (p.route_count || 0), 0), "route")}) move${moving.length === 1 ? "s" : ""} from ${ids} to ${into.stop_id}.`),
        ` ${keptName} will have ${plural(moving.length + staying.length, "platform")}.`)
      : h("p", h("strong", `${ids} ${from.length === 1 ? "has" : "have"} no platforms. The merge only removes ${from.length === 1 ? "it" : "them"}.`)),
    h("p.hint", "No route changes: a route's stop list names platforms, and every platform keeps its own id and its own label."),
    far.map(({ s, d }) => h("p.notice.warning", `${s.stop_id} is ${fmtMetres(d)} from ${into.stop_id}, more than the ${fmtMetres(FAR_METRES)} a station is grouped within. Check they are one place.`)),
    from.some((s) => s.name !== keptName) || into.name !== keptName
      ? h("p.notice.warning", `The names differ: ${[...new Set([into, ...from].map((s) => s.name))].join(", ")}. The station that stays will be called ${keptName}.`) : null,
    shared.length ? h("p.notice.warning", `Some platforms would share a label (${shared.map((c) => `“${c}”`).join(", ")}). Labels are kept as they are; relabel them afterwards if that reads badly.`) : null,
    moving.length ? h("ul.list.platform-move", moving.slice(0, 60).map((p) => h("li.list-item",
      h("span.key", p.platform_code || "no label"),
      h("span", p.name),
      h("span.hint", plural(p.route_count || 0, "route")),
      h("span.sub", `${p.stop_id}, from ${p.from_station}`)))) : null,
    moving.length > 60 ? h("p.hint", `and ${moving.length - 60} more platforms.`) : null);
}

// ------------------------------------------------------------------ step 3: in the draft
function done(draft, changeIds, into, from) {
  const mine = new Set(changeIds);
  const changes = draft.changes.filter((c) => mine.has(c.change_id));
  const moved = changes.flatMap((c) => ((c.before && c.before.moving_platforms) || []).map((p) => ({ ...p, from_station: c.entity_key })));
  const problems = (draft.validation || []).filter((v) => mine.has(v.change_id));
  const errors = problems.filter((p) => p.level === "error");
  const warnings = problems.filter((p) => p.level !== "error");
  const one = from.length === 1;
  const total = platforms(into).length + moved.length;
  toast(errors.length ? `Added to "${draft.title}" with ${plural(errors.length, "problem")} to fix before submitting.` : `Added to draft "${draft.title}".`, errors.length ? "error" : "");
  clear(panel(),
    h("section.section",
      errors.length
        ? h("div.notice.error", { role: "alert" },
            h("p", h("strong", `The ${one ? "merge is" : "merges are"} in your draft, but the draft cannot be submitted like this`)),
            h("ul", errors.map((p) => h("li", p.message))))
        : h("p.notice.ok", { role: "status" }, h("strong", one ? "Station merge added to your draft" : `${from.length} station merges added to your draft`)),
      h("div.title-block",
        h("h1", `${from.map((s) => `${s.name} (${s.stop_id})`).join(", ")} merged into ${into.name} (${into.stop_id})`),
        h("p", `${plural(moved.length, "platform")} move${moved.length === 1 ? "s" : ""} in draft “${draft.title}”; ${into.stop_id} will have ${plural(total, "platform")}.`)),
      warnings.length ? h("div.notice.warning", h("p", h("strong", "Please check")), h("ul", warnings.map((p) => h("li", p.message)))) : null,
      moved.length ? h("ul.list", moved.slice(0, 40).map((p) => h("li.list-item",
        h("span.key", p.platform_code || "no label"),
        h("a", { href: `#/stop/${enc(p.stop_id)}` }, p.name || p.stop_id),
        h("span.hint", plural(p.route_count || 0, "route")),
        h("span.sub", `${p.stop_id}, from ${p.from_station}`)))) : h("p.empty", `${one ? "It" : "They"} had no platforms, so nothing moved.`),
      h("div.btn-row",
        h("a.btn", { href: `#/drafts/${enc(draft.change_set_id)}` }, "Open the draft"),
        errors.length ? h("button.btn.danger", { type: "button", on: { click: async () => {
          try {
            for (const id of [...changeIds].reverse()) await removeChange(id);
            toast(one ? "The station merge was removed from the draft." : "The station merges were removed from the draft.");
            location.hash = `#/stop/${enc(into.stop_id)}`;
          } catch (e) {
            toast(e.message, "error");
          }
        } } }, one ? "Remove the merge from the draft" : "Remove these merges from the draft") : null,
        h("a.btn.secondary", { href: `#/stop/${enc(into.stop_id)}` }, "Back to the station"))));
  map.showGroup(into, from);
}
