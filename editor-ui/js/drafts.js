// The draft being edited: picking or starting one, the chip in the top bar, and
// adding changes to it. Every edit screen goes through addChange().
import { get, post, put, del, enc, ApiError } from "./api.js";
import { state, set, setPref, pref, can } from "./state.js";
import { h, modal, toast, fmtDate, plural, STATUS_LABEL, SERVED_EXCLUDE } from "./util.js";

// Remembered per person and feed: two people sharing a browser must not edit
// into each other's drafts.
const draftKey = () => `draft:${state.me ? state.me.email : ""}:${state.feedId}`;

export async function loadActiveDraft() {
  const id = pref(draftKey(), null);
  if (!id || !can("editor")) return set({ draft: null });
  try {
    const cs = await get(`change-sets/${enc(id)}`);
    set({ draft: cs.status === "draft" && cs.gtfs_id === state.feedId ? cs : null });
  } catch {
    set({ draft: null });
  }
  if (!state.draft) setPref(draftKey(), null);
}

export function useDraft(cs) {
  setPref(draftKey(), cs ? cs.change_set_id : null);
  set({ draft: cs });
}

export async function refreshDraft() {
  if (!state.draft) return null;
  const cs = await get(`change-sets/${enc(state.draft.change_set_id)}`);
  set({ draft: cs.status === "draft" ? cs : null });
  if (cs.status !== "draft") setPref(draftKey(), null);
  return cs;
}

export function renderDraftChip() {
  const chip = document.getElementById("draft-chip");
  if (!can("editor")) {
    chip.hidden = true;
    return;
  }
  chip.hidden = false;
  if (state.draft) {
    const n = state.draft.change_count ?? state.draft.changes.length;
    chip.className = "draft-chip";
    chip.textContent = `Draft: ${state.draft.title} (${plural(n, "change")})`;
    chip.title = "Your edits collect in this draft. Click to switch drafts or review it.";
  } else {
    chip.className = "draft-chip none";
    chip.textContent = "No draft open";
    chip.title = "Start or open a draft to make changes";
  }
}

// Ask which draft to put edits in. Resolves with the chosen change set or null.
export async function chooseDraft({ reason } = {}) {
  let open = [];
  try {
    open = (await get(`feeds/${enc(state.feedId)}/change-sets?status=draft&limit=50`)).items;
  } catch (e) {
    toast(e.message, "error");
  }
  const chosen = await modal("Choose a draft", (close) => {
    const title = h("input", { type: "text", id: "new-draft-title", placeholder: "For example: Alwarpet stops, both kerbs", maxlength: "120" });
    const desc = h("textarea", { id: "new-draft-desc", placeholder: "What is being fixed and why (optional)" });
    const err = h("p.notice.error", { role: "alert", hidden: true });
    const start = async (ev) => {
      ev.preventDefault();
      if (!title.value.trim()) {
        err.hidden = false;
        err.textContent = "Give the draft a title so reviewers know what it is for.";
        title.focus();
        return;
      }
      try {
        close(await post(`feeds/${enc(state.feedId)}/change-sets`, { title: title.value.trim(), description: desc.value.trim() }));
      } catch (e) {
        err.hidden = false;
        err.textContent = e.message;
      }
    };
    return h("div", { style: "display:grid;gap:16px" },
      reason ? h("p", reason) : null,
      open.length ? h("div", { style: "display:grid;gap:6px" },
        h("h3", "Continue a draft"),
        h("ul.list", open.map((cs) => h("li.list-item",
          h("span.chip.draft", STATUS_LABEL[cs.status]),
          h("button.btn.quiet", { type: "button", style: "justify-self:start", on: { click: () => close(cs) } }, cs.title),
          h("span.hint", plural(cs.change_count, "change")),
          h("span.sub", `Started by ${cs.created_by_email}, updated ${fmtDate(cs.updated_at)}`),
        )))) : null,
      h("form", { style: "display:grid;gap:10px", on: { submit: start } },
        h("h3", open.length ? "Or start a new draft" : "Start a new draft"),
        h("label.field", { for: "new-draft-title" }, h("span", "Title"), title),
        h("label.field", { for: "new-draft-desc" }, h("span", "Description"), desc),
        err,
        h("div.btn-row", h("button.btn", { type: "submit" }, "Start draft"),
          state.draft ? h("button.btn.quiet", { type: "button", on: { click: () => close(null) } }, "Stop using a draft") : null),
      ),
    );
  }, {
    actions: [(close) => h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Close")],
  });
  if (chosen === undefined) return state.draft;
  if (chosen === null) {
    useDraft(null);
    return null;
  }
  const full = await get(`change-sets/${enc(chosen.change_set_id)}`);
  useDraft(full);
  return full;
}

// Make sure there is a draft to edit into, asking if needed.
export async function requireDraft(reason) {
  if (state.draft) return state.draft;
  return chooseDraft({ reason: reason || "Edits are collected in a draft. Nothing changes for passengers until someone else approves it and it is committed." });
}

// The change already in the draft for this entity, if any - editing the same
// stop twice updates one change instead of stacking two. Only edits merge this
// way; a create, delete or merge is its own change. A route's stop lists are
// one per stop order (section 16): the dashboard edits pattern 1.
const patternOf = (c) => (c.after && c.after.pattern_key) || 1;
export function existingChange(entity, key, pattern = 1) {
  if (!state.draft) return null;
  return state.draft.changes.find((c) => c.entity === entity && sameKey(c, key) && (c.op === "update" || c.op === "replace")
    && (entity !== "route_stops" || patternOf(c) === pattern)) || null;
}

// A stage is its id and its direction, `143|down`. An edit names it so; a
// create or a split names it by its id and carries the direction in `after`;
// a bare id (from before direction joined the key) is the stage either way.
export const stageKey = (s) => s.stage_key || (s.direction ? `${s.stage_id}|${s.direction}` : s.stage_id);
function changeStageKey(c) {
  if (c.entity_key.includes("|") || (c.op !== "create" && c.op !== "split")) return c.entity_key;
  return `${c.entity_key}|${(c.after && c.after.direction) || ""}`;
}
export function sameStageKey(a, b) {
  if (a.includes("|") && b.includes("|")) return a === b;
  return a.split("|")[0] === b.split("|")[0];
}
const sameKey = (c, key) => (c.entity === "stage" ? sameStageKey(changeStageKey(c), key) : c.entity_key === key);

// What the draft creates. Stops and routes created in a draft count as existing
// for its later changes, so the editors offer them alongside live data. A stage
// is also made by a split, which takes routes off another stage onto it.
export function createdChange(entity, key) {
  if (!state.draft) return null;
  return state.draft.changes.find((c) => c.entity === entity && sameKey(c, key)
    && (c.op === "create" || (entity === "stage" && c.op === "split"))) || null;
}

// The changes of the draft to stage `key`, whichever way the change names it.
export function stageChanges(key) {
  if (!state.draft) return [];
  return state.draft.changes.filter((c) => c.entity === "stage" && sameKey(c, key));
}

// Stages the open draft makes, by a create or a split, in the list shape, for
// the stage search. A later edit of one in the same draft is shown, not the
// stage as it was first made.
export function createdStages() {
  if (!state.draft) return [];
  return state.draft.changes.filter((c) => c.entity === "stage" && (c.op === "create" || c.op === "split") && c.after).map((c) => {
    const key = `${c.entity_key}|${c.after.direction || ""}`;
    const edit = existingChange("stage", key);
    const after = edit ? { ...c.after, ...edit.after } : c.after;
    const rows = after.rows || [];
    return {
      stage_id: c.entity_key, stage_key: key, direction: after.direction || null,
      name: after.name, description: after.description || null, draft: true,
      stop_count: rows.filter((r) => !SERVED_EXCLUDE.has(r.stop_type)).length,
      route_count: c.op === "split" ? (c.after.routes || []).length : 0,
    };
  });
}

export function createdStops() {
  if (!state.draft) return [];
  return state.draft.changes.filter((c) => c.entity === "stop" && c.op === "create" && c.after).map((c) => ({
    stop_id: c.entity_key, stop_code: c.entity_key, name: c.after.name, lat: c.after.lat, lon: c.after.lon,
    platform_code: c.after.platform_code || null, location_type: 0, parent_station: null, route_count: 0,
    draft: true, change_id: c.change_id,
  }));
}

// Replace the `after` of a change already in the draft.
export async function updateChange(change, after) {
  const draft = state.draft;
  const cs = await put(`change-sets/${enc(draft.change_set_id)}/changes/${change.change_id}`, { after });
  useDraft(cs);
  const problems = (cs.validation || []).filter((v) => v.change_id === change.change_id);
  return { draft: cs, problems, changeId: change.change_id };
}

export async function removeChange(changeId) {
  const cs = await del(`change-sets/${enc(state.draft.change_set_id)}/changes/${changeId}`);
  useDraft(cs && cs.change_set_id ? cs : await get(`change-sets/${enc(state.draft.change_set_id)}`));
  return state.draft;
}

// Add (or merge into) a change. Returns {draft, problems} where problems are the
// server's validation entries for this change. `quiet` leaves the toast to the
// caller, for one that adds several changes as one step.
// `sameAs` is for entities whose change is keyed by something inside the
// payload: a route's temporary routes are all changes to the same route, so
// which one a save is about is `after.variant_id`, not the entity key. Without
// it a second save of the same thing adds a second change, and the draft then
// creates a row it has already created.
export async function addChange(change, { merge = true, quiet = false, sameAs = null } = {}) {
  const draft = await requireDraft();
  if (!draft) return null;
  const prior = sameAs
    ? (state.draft.changes || []).find((c) =>
        c.entity === change.entity && c.op === change.op
        && c.entity_key === change.entity_key && sameAs(c)) || null
    : merge ? existingChange(change.entity, change.entity_key, patternOf(change)) : null;
  let cs;
  try {
    if (prior && prior.op === change.op) {
      const after = change.entity === "route_stops" ? change.after : { ...prior.after, ...change.after };
      cs = await put(`change-sets/${enc(draft.change_set_id)}/changes/${prior.change_id}`, { after });
    } else {
      cs = await post(`change-sets/${enc(draft.change_set_id)}/changes`, change);
    }
  } catch (e) {
    if (e instanceof ApiError && e.code === "change_set_not_draft") {
      useDraft(null);
      toast("That draft was submitted or closed. Choose another draft and try again.", "error");
      return null;
    }
    throw e;
  }
  useDraft(cs);
  const mine = prior ? prior.change_id : cs.change_id ?? cs.changes[cs.changes.length - 1].change_id;
  const problems = (cs.validation || []).filter((v) => v.change_id === mine);
  const errors = problems.filter((p) => p.level === "error").length;
  if (!quiet) toast(errors ? `Added to "${cs.title}" with ${errors} problem${errors === 1 ? "" : "s"} to fix before submitting.`
    : `Added to draft "${cs.title}".`, errors ? "error" : "");
  return { draft: cs, problems, changeId: mine };
}

// The draft a review names when it is closed, chosen from the feed's drafts
// rather than typed: still open, waiting for approval, or committed lately,
// newest first, with the one open here marked. "Another draft" takes an id for
// anything older. `value()` is the chosen id, or "" for none.
const OTHER_DRAFT = "other";
export function draftPicker({ id, preselect = null }) {
  const select = h("select", { id });
  const other = h("input", {
    type: "text", id: `${id}-other`, hidden: true, autocomplete: "off", spellcheck: "false",
    placeholder: "Draft id", "aria-label": "Draft id",
  });
  select.addEventListener("change", () => {
    other.hidden = select.value !== OTHER_DRAFT;
    if (!other.hidden) other.focus();
  });
  const groups = [
    ["Open drafts", ["draft"]],
    ["Waiting for approval", ["submitted", "approved"]],
    ["Live", ["committed"]],
  ];
  const fill = (sets) => {
    const want = select.value && select.value !== OTHER_DRAFT ? select.value : preselect;
    // the open draft is always offered, even before the list comes back
    if (state.draft && !sets.some((cs) => cs.change_set_id === state.draft.change_set_id)) sets = [state.draft, ...sets];
    const option = (cs) => h("option", { value: cs.change_set_id, selected: cs.change_set_id === want },
      [
        cs.title || "untitled",
        state.draft && cs.change_set_id === state.draft.change_set_id ? "(open here)" : null,
        "\u2014",
        plural(cs.change_count ?? (cs.changes || []).length, "change"),
        cs.created_by_email ? `by ${cs.created_by_email}` : null,
      ].filter(Boolean).join(" "));
    select.replaceChildren(
      h("option", { value: "", selected: !want }, "No draft"),
      ...groups.map(([label, statuses]) => {
        const of = sets.filter((cs) => statuses.includes(cs.status));
        return of.length ? h("optgroup", { label }, of.map(option)) : null;
      }).filter(Boolean),
      h("option", { value: OTHER_DRAFT }, "Another draft, by its id\u2026"));
    // a preselected draft that is in none of the groups is typed in instead
    if (want && ![...select.options].some((o) => o.value === want)) {
      select.value = OTHER_DRAFT;
      other.value = want;
      other.hidden = false;
    }
  };
  fill([]);
  get(`feeds/${enc(state.feedId)}/change-sets?status=draft,submitted,approved,committed&limit=50`)
    .then((page) => fill(page.items || []))
    .catch(() => {});
  return {
    el: h("span.draft-picker", select, other),
    value: () => (select.value === OTHER_DRAFT ? other.value.trim() : select.value),
    set(v) {
      preselect = v;
      if ([...select.options].some((o) => o.value === v)) {
        select.value = v;
        other.hidden = true;
      } else if (v) {
        select.value = OTHER_DRAFT;
        other.value = v;
        other.hidden = false;
      }
    },
  };
}
