// Stages (docs/gtfs-editor.md section 18). A route is an ordered list of fare
// stages and a stage an ordered list of stops, shared by every route that runs
// through it. A route built from stages is edited only as its list of stages;
// its stops change only inside a stage, and that change reaches every route
// using it. A route not built from stages yet keeps the stop-by-stop editor.
import { get, enc } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, confirmDialog, debounce, plural, STOP_TYPE_LABEL, SERVED_EXCLUDE } from "./util.js";
import * as map from "./map.js";
import { addChange, existingChange, createdChange, requireDraft } from "./drafts.js";
import { stopPicker } from "./picker.js";
import { idKind } from "./stage_reviews.js";
import { showRoute } from "./explore.js";

const panel = () => document.getElementById("panel");
// sha256 of "[]": the stage list of a route that has none
const EMPTY_HASH = "4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945";
// the stop types a stage row may have; shaping points are kept but not added here
const ROW_TYPES = ["NEW STOP", "INTERMEDIATE STOP", "JUMP STOP", "HIDDEN STOP"];

function problems(list) {
  if (!list || !list.length) return null;
  const errors = list.filter((p) => p.level !== "warning");
  const warnings = list.filter((p) => p.level === "warning");
  return [
    errors.length ? h("div.notice.error", { role: "alert" }, h("p", h("strong", "Fix before submitting")), h("ul", errors.map((p) => h("li", p.message)))) : null,
    warnings.length ? h("div.notice.warning", h("p", h("strong", "Check")), h("ul", warnings.map((p) => h("li", p.message)))) : null,
  ];
}

function unsavedGuard(what) {
  let dirty = false;
  setLeaveGuard(() => (dirty ? what : null));
  return { touch() { dirty = true; }, done() { dirty = false; setLeaveGuard(null); } };
}

// Does the open draft change any stage or any route's stage list? Then reads go
// through the draft's preview, which applies it.
export function draftTouchesStages() {
  return !!state.draft && state.draft.changes.some((c) => c.entity === "stage" || c.entity === "route_stages");
}

// Stages the open draft creates, in the list shape, for the stage search.
function createdStages() {
  if (!state.draft) return [];
  return state.draft.changes.filter((c) => c.entity === "stage" && c.op === "create" && c.after).map((c) => {
    const rows = c.after.rows || [];
    return {
      stage_id: c.entity_key, name: c.after.name, description: c.after.description || null, draft: true,
      stop_count: rows.filter((r) => !SERVED_EXCLUDE.has(r.stop_type)).length, route_count: 0,
    };
  });
}

const directionWord = (d) => (d ? `direction: ${d}` : null);
// Why somebody still has to look at this stage: the routes that share its name
// do not agree about it (section 19.1). Cleared when the review is closed.
const REVIEW_TEXT = {
  head_differs: "routes start it at different stops",
  head_duplicate_stops: "routes start it at duplicate stops",
  stretch_differs: "routes disagree about its stops",
  spelled_differently: "spelled more than one way",
};
const reviewChip = (s) => (s.review
  ? h("span.chip.warn", { title: `${REVIEW_TEXT[s.review] || s.review}. Open “Stages to review” to settle it.` }, "to review")
  : null);
const stageLine = (s) => [
  plural(s.stop_count ?? 0, "stop"),
  directionWord(s.direction),
  s.first_stop && s.first_stop.name ? `${s.first_stop.name}${s.last_stop && s.last_stop.name && s.last_stop.stop_id !== s.first_stop.stop_id ? ` to ${s.last_stop.name}` : ""}` : null,
  `used by ${plural(s.route_count ?? 0, "route")}`,
  // a stage keeps a stop nobody can board at; the list says so (section 21)
  s.out_of_use ? `${plural(s.out_of_use, "stop")} out of use` : null,
  s.review ? REVIEW_TEXT[s.review] || s.review : null,
].filter(Boolean).join(" · ");

const rowName = (r) => (r.stop_type === "ROUTE CORRECTION" ? `Map shaping point: ${r.marker_name || r.marker_id}` : r.stop_name || r.stop_id);

// The stops of a stage as a short ordered list.
function stopList(rows) {
  if (!rows || !rows.length) return h("p.empty", "No stops.");
  return h("ol.stage-stops", rows.map((r) => h("li", { class: r.stop_type === "NEW STOP" ? "head" : "" },
    r.stop_type === "ROUTE CORRECTION" ? rowName(r) : h("a", { href: `#/stop/${enc(r.stop_id)}` }, rowName(r)),
    h("span.hint", ` ${STOP_TYPE_LABEL[r.stop_type] || r.stop_type}${r.stop_id ? `, ${r.stop_id}` : ""}`),
    r.unserviceable ? h("span.chip.out-of-use", { title: "The stop stays here; no bus calls there until it is back in use" }, "out of use") : null,
    r.stop_deleted ? h("span.chip.error", "deleted stop") : null)));
}

// A route drawn from its stages: every row of each stage with the route's number.
function flatten(links) {
  return links.flatMap((l) => (l.rows || []).map((r) => ({ ...r, stage_no: l.stage_no, stage_name: l.name })));
}

// ------------------------------------------------------------------ on the route page

// The "Stages" section of a route page. `onInfo(info)` is told whether the route
// is built from stages, so the page can take away the stop-by-stop editor.
// The stages of one list, as the route page and a temporary route both show
// them: fare stage number, name, who else uses it, and its stops.
export function stageListView(stages) {
  const inDraft = new Map((state.draft ? state.draft.changes : [])
    .filter((c) => c.entity === "stage").map((c) => [c.entity_key, c.op]));
  return h("ol.stage-list", (stages || []).map((s) => h("li.list-item",
    h("span.key", { "aria-label": `Fare stage ${s.stage_no}` }, String(s.stage_no)),
    h("a", { href: `#/stage/${enc(s.stage_key || s.stage_id)}` }, s.name),
    h("span.item-end", inDraft.has(s.stage_id) ? h("span.chip.draft", inDraft.get(s.stage_id) === "create" ? "new in draft" : "changed in draft") : null,
      h("span.hint", s.route_count > 1 ? `shared by ${plural(s.route_count, "route")}` : "only this route")),
    h("span.sub", `${plural(s.stop_count, "stop")}: ${(s.rows || []).filter((r) => r.stop_id).map(rowName).join(", ")}`),
    (s.rows || []).some((r) => r.unserviceable)
      ? h("span.sub.out-of-use-line", `${plural((s.rows || []).filter((r) => r.unserviceable).length, "stop")} out of use: ${(s.rows || []).filter((r) => r.unserviceable).map(rowName).join(", ")}`)
      : null)));
}

export function routeStagesSection(route, { created = false, onInfo } = {}) {
  const box = h("div", h("p.empty", "Loading stages…"));
  const section = h("section.section.route-stages", h("h2", "Stages"), box);
  (async () => {
    let info;
    try {
      info = draftTouchesStages() || created
        ? await get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(route.route_id)}/stages`)
        : await get(`feeds/${enc(state.feedId)}/routes/${enc(route.route_id)}/stages`);
    } catch (e) {
      clear(box, h("p.notice.error", e.message));
      return;
    }
    if (onInfo) onInfo(info);
    const edit = can("editor")
      ? h("div.btn-row", h("button.btn", { type: "button", on: { click: () => editRouteStages(route, { created }) } },
        info.has_stages ? "Change stages" : created ? "Choose stages" : "Build from stages"))
      : null;
    if (!info.has_stages) {
      clear(box,
        h("p.hint", created
          ? "This route is new in your draft and has no stages yet. Choose its fare stages in order; its stops come from them."
          : "This route is not built from stages yet. Until it is, its stops are edited stop by stop. Building it from stages replaces its stop list with the stops of the stages you choose."),
        edit);
      return;
    }
    clear(box,
      h("p.hint", `Built from ${plural(info.stages.length, "stage")}. Its stops change only through its stages: changing a stage changes every route that uses it.`),
      info.in_sync ? null : h("p.notice.warning", "This route's stop list was changed outside its stages. Choosing its stages again (Change stages, then Add to draft) puts it back in line with them."),
      stageListView(info.stages),
      edit);
  })();
  return section;
}

// ------------------------------------------------------------------ a route's stages

// `variant`: the same screen, saving a temporary route instead of the route's
// normal list (docs section 19). `{ id, name, reason, creating, stages, hash }`.
export async function editRouteStages(route, { created = false, variant = null } = {}) {
  if (!(await requireDraft("A route's stages are changed in a draft. Nothing changes for passengers until someone else approves it and it is committed."))) return;
  const prior = variant ? null : existingChange("route_stages", route.route_id);
  let live, current;
  try {
    live = created ? null : await get(`feeds/${enc(state.feedId)}/routes/${enc(route.route_id)}/stages`);
    current = draftTouchesStages() || created
      ? await get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(route.route_id)}/stages`)
      : live;
  } catch (e) {
    toast(e.message, "error");
    return;
  }
  // a temporary route starts from its own stages, or from what the route runs
  // today when it is new: most diversions are the route with a few stops swapped
  if (variant) current = { stages: variant.stages && variant.stages.length ? variant.stages : (current ? current.stages : []) };
  const baseHash = variant ? (variant.hash || EMPTY_HASH)
    : prior ? prior.after.base_stages_hash : live ? live.stages_hash : EMPTY_HASH;
  const replacesRows = !created && live && !live.has_stages && route.rows && route.rows.length > 0;
  let links = current.stages.map((s) => ({ ...s }));
  let serverProblems = null;
  const label = route.short_name || route.route_id;
  const unsaved = unsavedGuard(`Your changes to the stages of route ${label} are not in the draft yet.`);
  const list = h("ol.stage-list.editing");
  const summary = h("div", { "aria-live": "polite" });
  const results = h("ol.picker-results");
  const status = h("p.hint", { "aria-live": "polite" });

  const edited = () => { serverProblems = null; unsaved.touch(); redraw(); };
  const redraw = () => {
    const rows = flatten(links);
    const served = rows.filter((r) => !SERVED_EXCLUDE.has(r.stop_type)).length;
    clear(summary,
      h("p", `${plural(links.length, "stage")}, ${plural(served, "stop")} a passenger can board.`),
      problems(serverProblems));
    clear(list, links.length ? links.map((l, i) => h("li.list-item",
      h("label.visually-hidden", { for: `stage-no-${i}` }, `Fare stage number of ${l.name}`),
      h("input.stage-no-input", { type: "number", min: "0", id: `stage-no-${i}`, value: String(l.stage_no ?? ""),
        on: { change: (ev) => { l.stage_no = ev.target.value === "" ? null : Number(ev.target.value); edited(); } } }),
      h("a", { href: `#/stage/${enc(l.stage_key || l.stage_id)}` }, l.name),
      h("span.item-end",
        h("button.btn.quiet.small", { type: "button", disabled: i === 0, "aria-label": `Move ${l.name} up`, on: { click: () => { [links[i - 1], links[i]] = [links[i], links[i - 1]]; edited(); } } }, "↑"),
        h("button.btn.quiet.small", { type: "button", disabled: i === links.length - 1, "aria-label": `Move ${l.name} down`, on: { click: () => { [links[i + 1], links[i]] = [links[i], links[i + 1]]; edited(); } } }, "↓"),
        h("button.btn.quiet.small", { type: "button", "aria-label": `Take ${l.name} off the route`, on: { click: () => { links.splice(i, 1); edited(); } } }, "Remove")),
      h("span.sub", `${l.stage_id} · ${plural((l.rows || []).filter((r) => !SERVED_EXCLUDE.has(r.stop_type)).length, "stop")}: ${(l.rows || []).filter((r) => r.stop_id).map(rowName).join(", ")}`)))
      : h("li.empty-route", h("p", "No stages yet. Add the first one below; it is fare stage 1.")));
    map.showRoute({ ...route, rows }, { fit: rows.some((r) => r.lat != null) });
  };

  const onRoute = (id) => links.some((l) => l.stage_id === id);
  const addStage = async (s) => {
    if (onRoute(s.stage_id)) {
      toast(`${s.name} is already on this route: a route uses a stage once.`, "error");
      return;
    }
    let detail;
    try {
      detail = s.draft
        ? await get(`change-sets/${enc(state.draft.change_set_id)}/preview/stages/${enc(s.stage_key || s.stage_id)}`)
        : await get(`feeds/${enc(state.feedId)}/stages/${enc(s.stage_key || s.stage_id)}`);
    } catch (e) {
      toast(e.message, "error");
      return;
    }
    const last = links[links.length - 1];
    links.push({ stage_id: detail.stage_id, name: detail.name, rows: detail.rows, route_count: detail.route_count, stage_no: last && last.stage_no != null ? last.stage_no + 1 : 1 });
    toast(`Added ${detail.name} as fare stage ${links[links.length - 1].stage_no}.`);
    edited();
    // the results now mark it as on the route
    showResults(shownItems, shownMessage);
  };

  let shownItems = [], shownMessage = "";
  const showResults = (items, message) => {
    shownItems = items;
    shownMessage = message;
    status.textContent = message;
    clear(results, items.map((s, i) => h("li", h("button.picker-item", {
      type: "button", disabled: onRoute(s.stage_id), title: onRoute(s.stage_id) ? "Already on this route" : null,
      on: { click: () => addStage(s) },
    },
      h("span.picker-no", { "aria-hidden": "true" }, String(i + 1)),
      h("span.picker-main", h("span.picker-name", s.name), s.draft ? h("span.chip.draft", "New") : null,
        onRoute(s.stage_id) ? h("span.chip", "already on this route") : null),
      h("span.picker-meta", `${s.stage_id} · ${s.draft ? `${plural(s.stop_count, "stop")}, new in your draft` : stageLine(s)}${s.description ? ` · ${s.description}` : ""}`)))));
  };
  const input = h("input", { type: "search", id: "stage-search", autocomplete: "off", placeholder: "Stage name or stage id" });
  const search = debounce(async () => {
    const q = input.value.trim();
    if (q.length < 2) return showResults([], "Type at least two letters of the stage name, or find stages through a stop.");
    try {
      const page = await get(`feeds/${enc(state.feedId)}/stages?q=${enc(q)}&limit=20`);
      const drafts = createdStages().filter((s) => s.name.toLowerCase().includes(q.toLowerCase()) || s.stage_id === q);
      const items = [...drafts, ...page.items];
      if (!items.length && !drafts.length) {
        // a feed with no stages at all: say so, rather than that the name is wrong
        const any = await get(`feeds/${enc(state.feedId)}/stages?limit=1`);
        const feed = (state.feeds.find((f) => f.gtfs_id === state.feedId) || {}).display_name || state.feedId;
        if (!any.items.length && !createdStages().length) {
          return showResults([], `Feed “${feed}” has no stages yet. Make one with New stage, or switch feeds in the top bar: stages belong to one feed.`);
        }
      }
      showResults(items, items.length ? `${plural(items.length, "stage")} match “${q}”.` : `No stage matches “${q}”.`);
    } catch (e) {
      status.textContent = e.message;
    }
  }, 250);
  input.addEventListener("input", search);
  let picker = null;
  const pickerBox = h("div");
  const byStop = () => {
    if (picker) picker.close();
    picker = stopPicker({
      title: "Find the stages that call at a stop",
      onPick: async (stop) => {
        picker = null;
        clear(pickerBox);
        try {
          const page = await get(`feeds/${enc(state.feedId)}/stages?stop_id=${enc(stop.stop_id)}&limit=50`);
          showResults(page.items, page.items.length ? `${plural(page.items.length, "stage")} call at ${stop.name}.` : `No stage calls at ${stop.name} yet. Create one with New stage.`);
        } catch (e) {
          status.textContent = e.message;
        }
      },
      onCancel: () => { picker = null; clear(pickerBox); },
    });
    clear(pickerBox, picker.el);
    picker.focus();
  };

  // The route is drawn again directly, as the stop-list editor's Cancel does.
  // Setting the address is not enough: this screen opens without changing it,
  // so the address is already the route's and the router, which only acts on a
  // change, does nothing - Cancel and "Back to the route" both looked dead.
  const cancel = () => {
    unsaved.done();
    map.endModes();
    showRoute(route.route_id, { preview: created });
  };
  const renumber = () => {
    links.forEach((l, i) => { l.stage_no = i + 1; });
    edited();
  };
  // a temporary route is its id and its stages, nothing else
  const vId = h("input", { type: "text", id: "variant-id", maxlength: "64", autocomplete: "off",
    spellcheck: "false", placeholder: "mandaveli_1",
    value: variant && variant.id ? variant.id : "", disabled: !!(variant && !variant.creating) });
  const variantFields = variant
    ? h("section.section",
        h("label.field", { for: "variant-id" }, h("span", "Id"), vId,
          h("span.hint", variant.creating
            ? "What people will call it: mandaveli_1. Letters, digits, _ - or . It cannot change later."
            : "An id never changes.")))
    : null;

  const saveVariant = async () => {
    const errs = [];
    if (variant.creating && !/^[A-Za-z0-9_.-]{1,64}$/.test(vId.value.trim())) {
      errs.push("Give it a short id: letters, digits, _ - or . (no spaces), for example mandaveli_1.");
    }
    if (!links.length) errs.push("Choose at least one stage for it to run through.");
    if (errs.length) {
      serverProblems = errs.map((message) => ({ level: "error", message }));
      redraw();
      summary.scrollIntoView({ block: "nearest" });
      return;
    }
    const id = variant.creating ? vId.value.trim() : variant.id;
    try {
      const res = await addChange({
        entity: "route_variant", op: variant.creating ? "create" : "update", entity_key: route.route_id,
        after: {
          variant_id: id,
          stages: links.map((l) => ({ stage_id: l.stage_id, stage_no: l.stage_no })),
        },
        // a second save of this temporary route replaces its own change
      }, { merge: false, sameAs: (c) => (c.after || {}).variant_id === id });
      if (!res) return;
      unsaved.done();
      serverProblems = res.problems;
      if (res.problems.some((p) => p.level === "error")) {
        redraw();
        summary.scrollIntoView({ block: "nearest" });
        return;
      }
      map.endModes();
      location.hash = `#/route/${enc(route.route_id)}?draft=1`;
    } catch (e) {
      toast(e.message, "error");
    }
  };

  const save = async () => {
    if (variant) return saveVariant();
    if (replacesRows && !(await confirmDialog("Replace this route's stop list?",
      `Route ${label} has ${plural(route.rows.length, "row")} today that were not made from stages. Saving replaces them with the stops of the ${plural(links.length, "stage")} chosen here.`,
      { confirm: "Replace its stop list" }))) return;
    try {
      const res = await addChange({ entity: "route_stages", op: "replace", entity_key: route.route_id,
        after: { stages: links.map((l) => ({ stage_id: l.stage_id, stage_no: l.stage_no })), base_stages_hash: baseHash } });
      if (!res) return;
      unsaved.done();
      serverProblems = res.problems;
      if (res.problems.some((p) => p.level === "error")) {
        redraw();
        summary.scrollIntoView({ block: "nearest" });
        return;
      }
      map.endModes();
      location.hash = `#/route/${enc(route.route_id)}?draft=1`;
    } catch (e) {
      toast(e.message, "error");
    }
  };

  clear(panel(),
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: cancel } }, "Back to the route"),
      h("h1", variant
        ? (variant.creating ? `Add a temporary route to ${label}` : `Edit ${variant.id}`)
        : `Stages of ${label}`),
      h("p", route.long_name || ""),
      h("p.hint", variant
        ? "A temporary route is another list of stages for this route, for as long as the road is closed. It starts from what the route runs today: change the stages it runs through. It is not run until you choose to run it."
        : "Choose the route's fare stages in order. The stops of each stage come from the stage itself: to change a stop, open the stage. The number beside each stage is its fare stage number on this route."),
      replacesRows ? h("p.notice.warning", `This route is not built from stages yet. Saving replaces its ${plural(route.rows.length, "row")} with the stops of the stages chosen here.`) : null,
      summary,
      h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: renumber } }, "Number stages 1, 2, 3…"))),
    variantFields,
    h("section.section", list),
    h("section.section",
      h("h2", "Add a stage"),
      h("label.field", { for: "stage-search" }, h("span", "Search stages"), input),
      h("div.btn-row",
        h("button.btn.secondary.small", { type: "button", on: { click: byStop } }, "Find stages through a stop"),
        h("a.btn.quiet.small", { href: "#/new/stage" }, "New stage")),
      pickerBox, status, results),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "button", on: { click: save } },
        variant ? (variant.creating ? "Add to draft" : "Update in draft") : prior ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: cancel } }, "Cancel"))),
  );
  showResults([], "Type at least two letters of the stage name, or find stages through a stop.");
  redraw();
}

// ------------------------------------------------------------------ one stage

export async function showStage(stageId) {
  map.endModes();
  clear(panel(), h("section.section", h("p.empty", "Loading stage…")));
  const created = createdChange("stage", stageId);
  const drafted = !!state.draft && (!!created || state.draft.changes.some((c) => c.entity === "stage" && c.entity_key === stageId));
  let s;
  try {
    s = drafted
      ? await get(`change-sets/${enc(state.draft.change_set_id)}/preview/stages/${enc(stageId)}`)
      : await get(`feeds/${enc(state.feedId)}/stages/${enc(stageId)}`);
  } catch (e) {
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/stages" }, "All stages"), h("p.notice.error", e.message)));
  }
  map.clearFocus();
  map.showRoute({ route_id: s.stage_id, rows: s.rows });
  const editor = can("editor") && !s.deleted;
  const remove = async () => {
    if (!(await confirmDialog("Delete this stage?", `${s.name} (${s.stage_id}) is used by no route. Deleting it takes it out of the stage list once the draft is committed.`, { confirm: "Delete stage", danger: true }))) return;
    try {
      const res = await addChange({ entity: "stage", op: "delete", entity_key: s.stage_key || s.stage_id, after: null }, { merge: false });
      if (res) showStage(s.stage_key || s.stage_id);
    } catch (e) {
      toast(e.message, "error");
    }
  };
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/stages" }, "All stages"),
      h("div.title-block",
        h("h1", s.name, reviewChip(s), idKind(s.stage_id)),
        s.description ? h("p", s.description) : null,
        h("p.ids", `Stage id ${s.stage_id}, ${plural(s.stop_count, "stop")}${s.direction ? `, direction: ${s.direction}` : ""}, used by ${plural(s.route_count, "route")}`)),
      // a stand-in id is said here, not left for somebody to notice
      s.stage_id.startsWith("nm_") ? h("div.notice.warning",
        h("p", h("strong", "This stage is named after itself, not by MTC's stop id."),
          " The backfill could not line up the routes that run it with MTC's own route, "
          + "so it had no fare-stage id to use. A stage for the same place may also exist "
          + "under MTC's id, carrying every other route."),
        h("p", h("a", { href: "#/route-issues" }, "See the routes this came from"))) : null,
      (s.rows || []).some((r) => r.stop_type === "ROUTE CORRECTION") ? h("div.notice",
        h("p", h("strong", "This stage holds a map point."),
          " A map point is not a stop: it is a position the line is drawn through so it "
          + "follows the road. No passenger boards there and it is in no GTFS file.")) : null,
      s.deleted ? h("p.notice.error", "This stage is deleted.") : null,
      s.review ? h("div.notice",
        h("p", h("strong", "The routes using this name do not agree about it: "),
          `${REVIEW_TEXT[s.review] || s.review}.`),
        h("p", h("a", { href: `#/stage-reviews?q=${encodeURIComponent(s.name)}` },
          "See it beside the other stages of this name"))) : null,
      drafted ? h("div.notice.draft.pending",
        h("p", h("strong", created ? `New in your draft “${state.draft.title}”.` : `Pending in draft “${state.draft.title}”, not live.`), " ",
          h("a", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft")),
        h("p", "You are looking at this stage with your draft applied.")) : null,
      editor ? h("div.btn-row",
        h("button.btn", { type: "button", on: { click: () => editStage(s, { mode: "update" }) } }, "Edit stage"),
        h("button.btn.secondary", { type: "button", on: { click: () => editStage(s, { mode: "copy" }) } }, "Duplicate stage"),
        s.route_count === 0 && !created ? h("button.btn.quiet.danger", { type: "button", on: { click: remove } }, "Delete stage") : null) : null),
    h("section.section", h("h2", "Stops"), stopList(s.rows)),
    h("section.section", h("h2", "Routes using this stage"),
      s.routes.length
        ? h("ul.list", s.routes.map((r) => h("li.list-item",
          h("a", { href: `#/route/${enc(r.route_id)}` }, r.short_name || r.route_id),
          h("span.item-end",
            r.variant_id
              ? h("span.chip.variant", { title: r.running ? "The route is running this temporary route" : "One of the route's temporary routes; it is not running it" },
                  `temporary route ${r.variant_id}${r.running ? ", running" : ""}`)
              : h("span.chip.normal-route", { title: r.running ? "The route is running its normal route" : "The route's normal route; it is running a temporary route instead" },
                  `normal route${r.running ? ", running" : ""}`),
            h("span.hint", `fare stage ${r.stage_no}`)),
          h("span.sub", `${r.long_name || ""} · route id ${r.route_id}`))))
        : h("p.empty", "No route uses this stage yet.")),
  );
}

// Edit a stage (`mode` "update"), start a new one (`stage` null, mode "create"),
// or start one as a copy of `stage` (mode "copy").
export async function editStage(stage, { mode = "update" } = {}) {
  if (!(await requireDraft("Stages are changed in a draft. Nothing changes for passengers until someone else approves it and it is committed."))) return;
  const creating = mode !== "update";
  let rows = ((stage && stage.rows) || []).map((r) => ({ ...r }));
  const name = h("input", { type: "text", id: "stage-name", maxlength: "200", value: stage ? (mode === "copy" ? `${stage.name}` : stage.name) : "" });
  // two stages of one corridor share a name and hold the other direction's
  // stops, so the direction is what tells them apart in a search
  const direction = h("select", { id: "stage-direction" },
    h("option", { value: "", selected: !(stage && stage.direction) }, "Either way"),
    h("option", { value: "up", selected: !!(stage && stage.direction === "up") }, "Up"),
    h("option", { value: "down", selected: !!(stage && stage.direction === "down") }, "Down"));
  const desc = h("textarea", { id: "stage-desc", rows: "2", maxlength: "500", placeholder: "To tell same-named stages apart further, for example: via the flyover" });
  desc.value = (stage && stage.description) || "";
  const routes = (stage && mode === "update" && stage.routes) || [];
  const routeIds = [...new Set(routes.map((r) => r.route_id))];
  const list = h("ol.stage-stops.editing");
  const summary = h("div", { "aria-live": "polite" });
  let serverProblems = null;
  let active = null;
  const unsaved = unsavedGuard(creating ? "The new stage is not in the draft yet." : `Your changes to stage ${stage.name} are not in the draft yet.`);
  [name, desc].forEach((el) => el.addEventListener("input", () => unsaved.touch()));
  direction.addEventListener("change", () => unsaved.touch());

  const edited = () => { serverProblems = null; unsaved.touch(); redraw(); };
  const localProblems = () => {
    const out = [];
    if (!rows.length) out.push({ level: "error", message: "A stage needs at least one stop." });
    const heads = rows.filter((r) => r.stop_type === "NEW STOP").length;
    if (heads > 1) out.push({ level: "error", message: `A stage starts with one stage stop, and this one has ${heads}. Split it into two stages.` });
    if (rows.length && rows[0].stop_type === "INTERMEDIATE STOP") out.push({ level: "warning", message: "The first stop is an intermediate stop: a stage normally starts with its stage stop." });
    return out;
  };
  const slot = (at) => {
    if (active === at) {
      const picker = stopPicker({
        title: at === rows.length ? "Add a stop at the end" : `Add a stop before stop ${at + 1}`,
        near: rows[at - 1] && rows[at - 1].lat != null ? { lat: rows[at - 1].lat, lon: rows[at - 1].lon, label: `stop ${at}` } : null,
        exclude: [rows[at - 1], rows[at]].filter((r) => r && r.stop_id).map((r) => r.stop_id),
        excludeReason: "it is already next to this place in the stage, and a bus cannot stop at one stop twice in a row.",
        onPick: (s) => {
          rows.splice(at, 0, { stop_id: s.stop_id, stop_name: s.name, lat: s.lat, lon: s.lon, stop_type: rows.length ? "INTERMEDIATE STOP" : "NEW STOP",
            marker_id: null, marker_name: null, marker_lat: null, marker_lon: null, stop_name_override: null });
          active = null;
          edited();
        },
        onCancel: () => { active = null; redraw(); },
      });
      setTimeout(() => picker.focus(), 0);
      return h("li.add-slot.open", picker.el);
    }
    return h("li.add-slot", h("button.add-stop", { type: "button", on: { click: () => { active = at; redraw(); } } },
      h("span.plus", { "aria-hidden": "true" }, "+"), at === rows.length ? (rows.length ? "Add a stop at the end" : "Add the first stop") : "Add stop here"));
  };
  const rowEditor = (r, i) => h("li.row", { class: r.stop_type === "NEW STOP" ? "new" : "" },
    h("span.what",
      h("span.name", rowName(r),
        r.unserviceable ? h("span.chip.out-of-use", { title: "No bus calls there until it is back in use" }, "out of use") : null),
      r.stop_type === "ROUTE CORRECTION"
        ? h("span.meta", "Bends the map line, not a stop")
        : h("span.meta",
          h("label.visually-hidden", { for: `stage-row-type-${i}` }, `Type of stop ${i + 1}`),
          h("select", { id: `stage-row-type-${i}`, on: { change: (ev) => { r.stop_type = ev.target.value; edited(); } } },
            ROW_TYPES.map((t) => h("option", { value: t, selected: t === r.stop_type }, STOP_TYPE_LABEL[t] || t))),
          ` ${r.stop_id}`)),
    h("span.item-end",
      h("button.btn.quiet.small", { type: "button", disabled: i === 0, "aria-label": `Move stop ${i + 1} up`, on: { click: () => { [rows[i - 1], rows[i]] = [rows[i], rows[i - 1]]; edited(); } } }, "↑"),
      h("button.btn.quiet.small", { type: "button", disabled: i === rows.length - 1, "aria-label": `Move stop ${i + 1} down`, on: { click: () => { [rows[i + 1], rows[i]] = [rows[i], rows[i + 1]]; edited(); } } }, "↓"),
      h("button.btn.quiet.small", { type: "button", "aria-label": `Remove stop ${i + 1}`, on: { click: () => { rows.splice(i, 1); edited(); } } }, "Remove")));
  const redraw = () => {
    clear(summary, problems([...localProblems(), ...(serverProblems || [])]));
    const items = [slot(0)];
    rows.forEach((r, i) => { items.push(rowEditor(r, i)); items.push(slot(i + 1)); });
    if (!rows.length) items.length = 1;
    clear(list, items);
    map.showRoute({ route_id: "stage", rows }, { fit: rows.some((r) => r.lat != null) });
  };

  const back = (id) => {
    unsaved.done();
    map.endModes();
    const to = id ? `#/stage/${enc(id)}` : "#/stages";
    // the editor opens over the stage's own page, so the hash is often already
    // the one we are going back to, and the router ignores a hashchange that
    // changes nothing (main.js): draw the page we are going back to instead
    if (location.hash === to) {
      if (id) showStage(id);
      else showStagesList();
    } else {
      location.hash = to;
    }
  };
  const save = async () => {
    if (!name.value.trim()) {
      toast("Give the stage a name: it is the stage name passengers' fares are counted by.", "error");
      name.focus();
      return;
    }
    const payload = {
      name: name.value.trim(),
      direction: direction.value || null,
      description: desc.value.trim() || null,
      rows: rows.map((r) => {
        const marker = r.stop_type === "ROUTE CORRECTION";
        return {
          stop_id: marker ? null : r.stop_id, stop_type: r.stop_type,
          marker_id: marker ? r.marker_id || null : null, marker_name: marker ? r.marker_name || null : null,
          marker_lat: marker ? r.marker_lat ?? null : null, marker_lon: marker ? r.marker_lon ?? null : null,
          stop_name_override: marker ? null : r.stop_name_override ?? null,
        };
      }),
    };
    if (!creating && routeIds.length > 1 && !(await confirmDialog("Change every route using this stage?",
      `${stage.name} is used by ${plural(routeIds.length, "route")}. Once the draft is committed, all of them get these stops.`, { confirm: "Change them all" }))) return;
    try {
      const res = creating
        ? await addChange({ entity: "stage", op: "create", entity_key: "", after: payload }, { merge: false })
        : await addChange({ entity: "stage", op: "update", entity_key: stage.stage_key || stage.stage_id, after: payload });
      if (!res) return;
      const id = creating ? res.draft.changes.find((c) => c.change_id === res.changeId).entity_key : stage.stage_id;
      serverProblems = res.problems;
      if (res.problems.some((p) => p.level === "error")) {
        unsaved.done();
        redraw();
        summary.scrollIntoView({ block: "nearest" });
        return;
      }
      back(id);
    } catch (e) {
      toast(e.message, "error");
    }
  };

  clear(panel(),
    h("section.section",
      h("button.btn.quiet.small", { type: "button", style: "justify-self:start", on: { click: () => back(stage && stage.stage_id) } }, stage ? "Back to the stage" : "Back to stages"),
      h("h1", mode === "update" ? `Edit stage ${stage.name}` : mode === "copy" ? `Duplicate ${stage.name}` : "New stage"),
      mode === "copy" ? h("p.notice", "The copy is a new stage. Change it, then put it on the routes that need it with Change stages; every other route keeps the original.") : null,
      routeIds.length ? h("div.notice.warning",
        h("p", h("strong", `Used by ${plural(routeIds.length, "route")}.`), " Saving changes the stops of every one of them:"),
        h("p", routes.map((r) => r.short_name || r.route_id).filter((v, i, a) => a.indexOf(v) === i).join(", ")),
        h("p", "To change only some of them, Duplicate stage instead.")) : null,
      h("div.field-row",
        h("label.field", { for: "stage-name" }, h("span", "Stage name"), name),
        h("label.field", { for: "stage-direction" }, h("span", "Direction"), direction,
          h("span.hint", "Which way along the corridor. Two stages may share a name and run opposite ways."))),
      h("label.field", { for: "stage-desc" }, h("span", "Description (optional)"), desc),
      h("p.hint", "A stage starts with its stage stop; the intermediate stops after it are charged as the same fare stage."),
      summary),
    h("section.section", h("h2", "Stops"), h("div.ladder.editing", list)),
    h("div.sticky-actions", h("div.btn-row",
      h("button.btn", { type: "button", on: { click: save } }, creating ? "Add to draft" : existingChange("stage", stage.stage_id) ? "Update in draft" : "Add to draft"),
      h("button.btn.secondary", { type: "button", on: { click: () => back(stage && stage.stage_id) } }, "Cancel"))),
  );
  redraw();
}

// ------------------------------------------------------------------ the stage list

export async function showStagesList() {
  const page = document.getElementById("page");
  const input = h("input", { type: "search", id: "stages-q", autocomplete: "off", placeholder: "Stage name or stage id" });
  const unused = h("input", { type: "checkbox", id: "stages-unused" });
  // the two directions of a corridor share a name, so this is how to find one
  const direction = h("select", { id: "stages-direction" },
    h("option", { value: "" }, "Either way"),
    h("option", { value: "up" }, "Up"),
    h("option", { value: "down" }, "Down"));
  const list = h("ul.list");
  const more = h("div");
  const status = h("p.hint", { "aria-live": "polite" });
  let cursor = null;
  const item = (s) => h("li.list-item",
    h("a", { href: `#/stage/${enc(s.stage_key || s.stage_id)}` }, s.name),
    h("span.item-end", s.route_count === 0 ? h("span.chip", "unused") : h("span.hint", plural(s.route_count, "route"))),
    h("span.sub", `${s.stage_id} · ${s.draft ? `${plural(s.stop_count, "stop")}, new in your draft` : stageLine(s)}${s.description ? ` · ${s.description}` : ""}`));
  const load = async (append = false) => {
    const q = input.value.trim();
    const params = new URLSearchParams({ limit: "50" });
    if (q) params.set("q", q);
    if (unused.checked) params.set("unused", "true");
    if (direction.value) params.set("direction", direction.value);
    if (append && cursor) params.set("cursor", cursor);
    try {
      const res = await get(`feeds/${enc(state.feedId)}/stages?${params}`);
      cursor = res.next_cursor;
      const drafts = append ? [] : createdStages().filter((s) => !q || s.name.toLowerCase().includes(q.toLowerCase()));
      if (!append) clear(list);
      [...drafts, ...res.items].forEach((s) => list.appendChild(item(s)));
      status.textContent = list.children.length ? "" : q ? `No stage matches “${q}”.` : "No stages yet.";
      clear(more, cursor ? h("button.btn.secondary.small", { type: "button", on: { click: () => load(true) } }, "Load more") : null);
    } catch (e) {
      status.textContent = e.message;
    }
  };
  input.addEventListener("input", debounce(() => load(), 250));
  unused.addEventListener("change", () => load());
  direction.addEventListener("change", () => load());
  clear(page, h("div.page-inner",
    h("h1", "Stages"),
    h("p", "A stage is a run of stops charged as one fare stage. Routes are built from stages, and one stage is shared by every route that runs through it, so a stage is fixed once for all of them."),
    can("editor") ? h("div.btn-row", h("a.btn", { href: "#/new/stage" }, "New stage")) : null,
    h("div.filters",
      h("label.field", { for: "stages-q" }, h("span", "Search"), input),
      h("label.field", { for: "stages-direction" }, h("span", "Direction"), direction),
      h("label.inline", { for: "stages-unused" }, unused, " Only stages no route uses")),
    status, list, more));
  load();
}
