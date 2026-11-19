// A node's versions, and rolling it back (docs section 26). A stop, station,
// route, a route's stop list or stages, and a stage each list the committed
// drafts that changed them, newest first, with what each changed. "Restore this
// version" never changes anything live: it puts one change into a draft - a new
// one, or the draft being edited - that a second person approves and commits
// like any other.
import { get, post, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, fmtDate, modal, plural, toast, errorText } from "./util.js";
import { useDraft } from "./drafts.js";

// Words for a change's fields: a position is one thing to a person.
const FIELD_WORDS = {
  lat: "position", lon: "position", name: "name", short_name: "number", long_name: "name",
  encoded_polyline: "map line", polyline_source: "map line", member_stop_ids: "stops", members: "stops",
  rows: "stops", stages: "stages", description: "description", platform_code: "platform label",
  color: "colour", text_color: "text colour", active: "in use", unserviceable: "out of use",
};
const CONTROL = new Set(["base_rows_hash", "base_stages_hash", "base_trips_hash", "base_hash",
  "into_row_version", "position_review_id", "proposal_id", "pattern_key", "profile_key"]);

function fieldWords(after) {
  const keys = Object.keys(after || {}).filter((k) => !CONTROL.has(k));
  return [...new Set(keys.map((k) => FIELD_WORDS[k] || k.replace(/_/g, " ")))];
}

// What one version did, in a line.
function whatChanged(v) {
  if (!v.direct) return v.why_not || `${v.entity} ${v.op}`;
  switch (v.op) {
    case "create": return "Made";
    case "delete": return "Deleted";
    case "merge": return v.why_not || "Merged";
    case "split": return "Split";
    case "replace": {
      const was = Array.isArray(v.before) ? v.before.length : null;
      const now = v.after && (v.after.rows || v.after.stages);
      const what = v.after && v.after.stages ? "stage" : "stop";
      return was != null && Array.isArray(now)
        ? `Replaced the list: ${plural(was, what)} → ${plural(now.length, what)}`
        : "Replaced the list";
    }
    default: {
      const words = fieldWords(v.after);
      return words.length ? `Changed ${words.join(", ")}` : "Changed";
    }
  }
}

// Ask where the change goes, then make it. Resolves with the API's answer or null.
async function restore(node, target, label) {
  const draft = state.draft && state.draft.gtfs_id === state.feedId ? state.draft : null;
  const choice = await modal(`Restore ${label}`, () => h("div",
    h("p", `This adds one change to a draft that puts ${node.label} back as ${target.words}. Only what differs from the live ${node.noun} is changed.`),
    h("p.hint", "Nothing changes live until someone else approves and commits the draft.")), {
    actions: [
      (close) => h("button.btn.secondary", { type: "button", on: { click: () => close(null) } }, "Cancel"),
      draft ? (close) => h("button.btn.secondary", { type: "button", on: { click: () => close("current") } }, `Add to "${draft.title}"`) : null,
      (close) => h("button.btn", { type: "button", on: { click: () => close("new") } }, "Into a new draft"),
    ].filter(Boolean),
  });
  if (!choice) return null;
  const body = { change_id: target.change_id, side: target.side };
  if (node.pattern_key) body.pattern_key = node.pattern_key;
  if (choice === "current") body.change_set_id = draft.change_set_id;
  try {
    const res = await post(`feeds/${enc(state.feedId)}/history/${enc(node.entity)}/${enc(node.key)}/rollback`, body);
    const cs = await get(`change-sets/${enc(res.change_set_id)}`);
    useDraft(cs);
    const skipped = res.not_restored || [];
    toast(`Added to "${cs.title}": ${fieldWords(Object.fromEntries((res.fields || []).map((f) => [f, 1]))).join(", ") || "the change"}.`
      + (skipped.length ? ` Not restored: ${skipped.map((s) => `${s.field} (${s.reason})`).join("; ")}.` : ""));
    location.hash = `#/drafts/${enc(res.change_set_id)}`;
    return res;
  } catch (e) {
    toast(errorText(e), "error");
    return null;
  }
}

function versionRow(node, v) {
  const editable = can("editor") && v.restorable && !v.is_live;
  return h("li.list-item.version",
    h("span.key", `v${v.feed_version}`),
    h("span",
      h("a", { href: `#/drafts/${enc(v.change_set_id)}` }, v.title || "a draft"),
      v.is_live ? [" ", h("span.chip.committed", "live now")] : null,
      !v.direct ? [" ", h("span.chip", "in passing")] : null),
    h("span.hint", fmtDate(v.committed_at)),
    h("span.sub", `${whatChanged(v)}${v.committed_by ? ` · committed by ${v.committed_by}` : ""}`),
    v.unrecorded_change_after
      ? h("p.notice", "Changed again after this version by something this list does not show: a script, a seed, a migration, or a merge of a stop it calls at.")
      : null,
    editable
      ? h("div.btn-row", h("button.btn.secondary.small", {
        type: "button",
        title: "Put it back as this version left it, through a draft",
        on: { click: () => restore(node, { change_id: v.change_id, side: "after", words: `v${v.feed_version} left it` }, `v${v.feed_version}`) },
      }, "Restore this version"))
      : null);
}

// The versions of one node, filled into `box`.
async function fill(box, node) {
  clear(box, h("p.empty", "Loading versions…"));
  let res;
  try {
    const q = node.pattern_key ? `?pattern_key=${enc(node.pattern_key)}` : "";
    res = await get(`feeds/${enc(state.feedId)}/history/${enc(node.entity)}/${enc(node.key)}${q}`);
  } catch (e) {
    // a server without the endpoint: say nothing rather than an error
    if (e.status === 404 && e.code !== "version_not_found") return clear(box);
    return clear(box, h("p.notice.error", errorText(e)));
  }
  node.label = res.label || node.label;
  const versions = res.versions || [];
  const o = res.original;
  const originalRow = o ? h("li.list-item.version",
    h("span.key", "before"),
    h("span", "As it was before its first recorded edit", o.is_live ? [" ", h("span.chip.committed", "live now")] : null),
    h("span.hint", `until v${o.feed_version}`),
    can("editor") && o.restorable && !o.is_live
      ? h("div.btn-row", h("button.btn.secondary.small", {
        type: "button",
        on: { click: () => restore(node, { change_id: o.change_id, side: "before", words: `it was before v${o.feed_version}` }, "the original") },
      }, "Restore this"))
      : null) : null;
  clear(box,
    !res.exists ? h("p.notice", "It is deleted or merged away: its versions are listed, and a rollback needs it live.") : null,
    versions.length
      ? h("ul.list.versions", versions.map((v) => versionRow(node, v)), originalRow)
      : h("p.empty", "No committed draft has changed it yet."),
    res.truncated ? h("p.hint", `The newest ${versions.length} of ${res.total}.`) : null);
}

// The panel section. `nodes`: [{entity, key, label, noun, title, pattern_key?}];
// more than one (a route: its details, its stop list, its stages) get a picker.
export function historySection(nodes) {
  const list = nodes.filter(Boolean);
  if (!list.length) return null;
  const box = h("div");
  const section = h("section.section.node-history", { "aria-label": "Versions" });
  const picker = list.length > 1
    ? h("select", { id: "history-of", "aria-label": "Versions of", on: { change: (ev) => fill(box, list[Number(ev.target.value)]) } },
      list.map((n, i) => h("option", { value: String(i) }, n.title)))
    : null;
  // loaded when opened: most visits never look
  const details = h("details", {
    on: { toggle: () => { if (details.open && !details.dataset.loaded) { details.dataset.loaded = "1"; fill(box, list[0]); } } },
  }, h("summary", h("h2", { style: "display:inline" }, "Versions")),
  h("p.hint", "Every committed draft that changed it, newest first. Restoring one puts it back through a draft, which someone else approves before it goes live."),
  picker ? h("label.field", { for: "history-of" }, h("span", "Versions of"), picker) : null,
  box);
  section.append(details);
  return section;
}

// The nodes of each panel.
export const stopNodes = (s) => [{
  entity: s.location_type === 1 ? "station" : "stop", key: s.stop_id,
  label: `${s.location_type === 1 ? "station" : "stop"} ${s.name}`, noun: s.location_type === 1 ? "station" : "stop", title: "This stop",
}];

export const routeNodes = (r, { stages = false } = {}) => [
  { entity: "route", key: r.route_id, label: `route ${r.short_name || r.route_id}`, noun: "route", title: "Name, colour and map line" },
  { entity: "route_stops", key: r.route_id, label: `route ${r.short_name || r.route_id}'s stop list`, noun: "stop list", title: "Stop list", pattern_key: 1 },
  stages ? { entity: "route_stages", key: r.route_id, label: `route ${r.short_name || r.route_id}'s stages`, noun: "stage list", title: "Stages" } : null,
].filter(Boolean);

export const stageNodes = (s, key) => [{
  entity: "stage", key, label: `stage ${s.name}`, noun: "stage", title: "This stage",
}];
