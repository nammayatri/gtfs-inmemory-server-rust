// Temporary routes (docs section 19): a route runs somewhere else for a while.
// A diversion is another list of stages for the same route; the route wears one
// at a time, and its normal list is the one with no id. Everything here goes
// into a draft like any other edit: nothing is live until someone else approves
// it and it is committed.
import { get, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtDate, plural } from "./util.js";
import { addChange, requireDraft } from "./drafts.js";
import { editRouteStages, stageListView } from "./stages.js";

const page = () => document.getElementById("page");
const panel = () => document.getElementById("panel");

// the draft's own changes to this route's lists, so the page shows what it will
// be rather than only what is live
const draftTouchesVariants = () =>
  !!state.draft && (state.draft.changes || []).some((c) => c.entity === "route_variant");

const problems = (list) => (list || []).map((p) =>
  h("li", { class: p.level === "error" ? "err" : "warn" }, p.message));

async function readVariants(routeId) {
  return draftTouchesVariants()
    ? get(`change-sets/${enc(state.draft.change_set_id)}/preview/routes/${enc(routeId)}/variants`)
    : get(`feeds/${enc(state.feedId)}/routes/${enc(routeId)}/variants`);
}

// ------------------------------------------------------------------ on a route
// One section for every list of stages a route has: the dropdown picks which
// one to look at (its normal route, or one of its temporary routes), the stages
// below follow it, and + adds a temporary route.
export function routeListsSection(route, { created = false } = {}) {
  const box = h("div", h("p.empty", "Loading stages…"));
  const section = h("section.section.route-lists", h("h2", "Stages"), box);
  if (created) {
    clear(box, h("p.hint", "This route is new in your draft. Choose its stages once it is committed."));
    return section;
  }
  let chosen = null;              // null is the route's normal list
  // the section opens on whatever the route is running, and after that shows
  // what the person picked: settling it on every draw would snap the dropdown
  // back to the diversion and leave no way to reach the normal route
  let opening = true;
  const draw = async () => {
    let info;
    try {
      info = await readVariants(route.route_id);
    } catch (e) {
      clear(box, h("p.notice.error", e.message));
      return;
    }
    const editor = can("editor");
    const variants = info.variants || [];
    if (chosen && !variants.some((v) => v.variant_id === chosen)) chosen = null;
    if (opening && info.active_variant_id) chosen = info.active_variant_id;
    opening = false;
    const active = variants.find((v) => v.active) || null;
    const picked = chosen ? variants.find((v) => v.variant_id === chosen) : null;
    const stages = picked ? picked.stages || [] : info.normal_stages || [];
    const running = picked ? picked.active : !info.active_variant_id;

    const select = h("select", { id: "route-list", "aria-label": "Which list of stages to show",
      on: { change: (ev) => { chosen = ev.target.value || null; draw(); } } },
      h("option", { value: "", selected: !picked }, `Normal route${info.active_variant_id ? "" : " (running)"}`),
      ...variants.map((v) => h("option", { value: v.variant_id, selected: picked && picked.variant_id === v.variant_id },
        `${v.variant_id}${v.active ? " (running)" : ""}`)));
    const plus = editor
      ? h("button.btn.secondary.add-variant", { type: "button", title: "Add a temporary route",
          "aria-label": "Add a temporary route", on: { click: () => editVariant(route, null) } }, "+")
      : null;

    // what this list is, and what can be done with it
    const actions = [];
    if (editor && picked && !picked.active) {
      actions.push(h("button.btn", { type: "button", on: { click: () => activate(route, picked, info, draw) } }, "Run this one"));
    }
    // the diversion the route is running is where someone looks for the way
    // off it, so the way back is offered here as well as on the normal route
    if (editor && picked && picked.active) {
      actions.push(h("button.btn", { type: "button", on: { click: () => backToNormal(route, info, () => { chosen = null; draw(); }) } }, "Back to normal route"));
    }
    if (editor && picked) {
      actions.push(h("button.btn.secondary", { type: "button", on: { click: () => editVariant(route, picked) } }, "Edit"));
      if (!picked.active) {
        actions.push(h("button.btn.danger", { type: "button", on: { click: () => removeVariant(route, picked, draw) } }, "Delete"));
      }
    }
    if (editor && !picked) {
      if (info.active_variant_id) {
        actions.push(h("button.btn", { type: "button", on: { click: () => backToNormal(route, info, () => { chosen = null; draw(); }) } }, "Back to normal route"));
      }
      actions.push(h("button.btn.secondary", { type: "button", on: { click: () => editRouteStages(route, {}) } },
        info.has_normal_list ? "Change stages" : "Build from stages"));
    }

    clear(box,
      h("div.list-picker",
        h("label", { for: "route-list" }, "Showing"),
        select,
        plus),
      active && !picked
        ? h("p.notice.warning", { role: "status" },
            h("strong", "This route is running "), h("code", active.variant_id),
            " The stops it serves come from that list, not this one.")
        : null,
      picked
        ? h("p.hint", running
            ? "The route is running this temporary route."
            : `A temporary route. It is not running: the route serves ${active ? active.variant_id : "its normal route"}.`)
        : h("p.hint", info.has_normal_list
            ? "The route's normal list of stages. Changing a stage changes every route that uses it."
            : "This route is edited stop by stop today. Adding a temporary route turns its stops into stages first, so it can go back to them; what it serves does not change."),
      stages.length ? stageListView(stages) : h("p.empty", picked ? "No stages yet." : "No stages yet: this route is edited stop by stop."),
      actions.length ? h("div.btn-row", ...actions) : null);
  };
  draw();
  return section;
}

// ------------------------------------------------------------------ the changes
async function activate(route, variant, _info, done) {
  if (!(await requireDraft())) return;
  const ok = await confirmDialog(
    `Run ${variant.variant_id} on route ${route.short_name || route.route_id}?`,
    `Its stops come from ${plural(variant.stage_count, "stage")} instead of the route's own until someone puts it back. `
    + "Nothing changes for passengers until this draft is approved and committed.",
    { confirm: "Add to draft", danger: true });
  if (!ok) return;
  try {
    const res = await addChange({
      entity: "route_variant", op: "activate", entity_key: route.route_id,
      after: { variant_id: variant.variant_id, base_stages_hash: variant.stages_hash || "" },
      // which list the route wears is one thing about the route, so a later
      // choice in the same draft replaces the earlier one
    }, { merge: false, sameAs: () => true });
    if (res) { report(res, `Route runs ${variant.variant_id} once the draft is committed.`); done(); }
  } catch (e) {
    toast(e.message, "error");
  }
}

async function backToNormal(route, info, done) {
  if (!(await requireDraft())) return;
  const ok = await confirmDialog(
    `Put route ${route.short_name || route.route_id} back on its normal route?`,
    "It goes back to the stops of its normal list of stages. The temporary route stays, to use again the next time.",
    { confirm: "Add to draft" });
  if (!ok) return;
  try {
    const res = await addChange({
      entity: "route_variant", op: "activate", entity_key: route.route_id,
      after: { variant_id: null, base_stages_hash: info.normal_stages_hash || "" },
    }, { merge: false, sameAs: () => true });
    if (res) { report(res, "Back on its normal route once the draft is committed."); done(); }
  } catch (e) {
    toast(e.message, "error");
  }
}

async function removeVariant(route, variant, done) {
  if (!(await requireDraft())) return;
  const ok = await confirmDialog(
    `Delete ${variant.variant_id}?`,
    `The route is not running it. Its list of stages goes; the stages themselves stay, since other routes may use them.`,
    { confirm: "Delete in draft", danger: true });
  if (!ok) return;
  try {
    const res = await addChange({
      entity: "route_variant", op: "delete", entity_key: route.route_id,
      after: { variant_id: variant.variant_id },
    }, { merge: false, sameAs: (c) => (c.after || {}).variant_id === variant.variant_id });
    if (res) { report(res, `${variant.variant_id} is deleted once the draft is committed.`); done(); }
  } catch (e) {
    toast(e.message, "error");
  }
}

function report(res, said) {
  const errors = (res.problems || []).filter((p) => p.level === "error");
  if (!errors.length) return toast(said);
  toast(errors[0].message, "error");
}

// ------------------------------------------------------------------ add or edit one
// The stages of a temporary route are chosen on the route's own stages screen,
// so there is one picker to learn. A new one starts from what the route runs
// today: most diversions are the normal route with a few stops swapped.
export function editVariant(route, variant) {
  return editRouteStages(route, {
    variant: variant
      ? { id: variant.variant_id, creating: false, stages: variant.stages || [], hash: variant.stages_hash }
      : { id: "", creating: true, stages: [], hash: "" },
  });
}

// ------------------------------------------------------------------ the watchlist
// Every route running a temporary route, so one does not quietly become the
// route.
export async function showDiversions() {
  clear(page(), h("div.page-inner",
    h("div.page-head", h("div.title-block", h("h1", "Diversions"),
      h("p.hint", "Routes running a temporary route right now. A road reopens long before anyone remembers to put the route back, so this is the list to check."))),
    h("p.empty", "Loading…")));
  let res;
  try {
    res = await get(`feeds/${enc(state.feedId)}/diversions`);
  } catch (e) {
    return clear(page(), h("div.page-inner", h("p.notice.error", e.message)));
  }
  const items = res.items || [];
  clear(page(), h("div.page-inner",
    h("div.page-head", h("div.title-block", h("h1", "Diversions"),
      h("p.hint", `${plural(items.length, "route")} running a temporary route.`))),
    items.length
      ? h("div.table-wrap", h("table",
          h("thead", h("tr", h("th", "Route"), h("th", "Temporary route"), h("th", "Since"))),
          h("tbody", items.map((d) => h("tr.linkrow",
            h("td", h("a", { href: `#/route/${enc(d.route_id)}` }, d.short_name || d.route_id),
              h("span.sub", d.long_name || "")),
            h("td", h("code", d.variant_id)),
            h("td", d.since ? fmtDate(d.since) : ""))))))
      : h("p.empty", "No route is diverted. Every route is running normally.")));
}

// ------------------------------------------------------------------ stops out of use
// Every stop nobody can board at right now (docs section 21). Nothing takes a
// stop back into use on its own, so this is the list to check: a barricade comes
// down long before anyone remembers the flag.
export async function showUnserviceableStops() {
  clear(page(), h("div.page-inner",
    h("div.page-head", h("div.title-block", h("h1", "Stops out of use"),
      h("p.hint", "Stops that stay in the feed and in the app while no bus calls there. Journeys are routed around them until they are back in use."))),
    h("p.empty", "Loading…")));
  let res;
  try {
    res = await get(`feeds/${enc(state.feedId)}/unserviceable-stops`);
  } catch (e) {
    return clear(page(), h("div.page-inner", h("p.notice.error", e.message)));
  }
  const items = res.items || [];
  clear(page(), h("div.page-inner",
    h("div.page-head", h("div.title-block", h("h1", "Stops out of use"),
      h("p.hint", `${plural(items.length, "stop")} out of use. Their times come back exactly as they were.`))),
    items.length
      ? h("div.table-wrap", h("table",
          h("thead", h("tr", h("th", "Stop"), h("th", "Code"), h("th", "Routes that call there"), h("th", "Since"))),
          h("tbody", items.map((s) => h("tr.linkrow",
            h("td", h("a", { href: `#/stop/${enc(s.stop_id)}` }, s.name),
              s.platform_code ? h("span.sub", `Platform ${s.platform_code}`) : null),
            h("td", h("code", s.stop_code || s.stop_id)),
            h("td", plural(s.route_count ?? 0, "route")),
            h("td", s.updated_at ? fmtDate(s.updated_at) : ""))))))
      : h("p.empty", "Every stop is in use.")));
}
