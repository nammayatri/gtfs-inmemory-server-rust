// The draft being edited: picking or starting one, the chip in the top bar, and
// adding changes to it. Every edit screen goes through addChange().
import { get, post, put, del, enc, ApiError } from "./api.js";
import { state, set, setPref, pref, can } from "./state.js";
import { h, modal, toast, fmtDate, plural, STATUS_LABEL } from "./util.js";

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
// way; a create, delete or merge is its own change.
export function existingChange(entity, key) {
  if (!state.draft) return null;
  return state.draft.changes.find((c) => c.entity === entity && c.entity_key === key && (c.op === "update" || c.op === "replace")) || null;
}

// What the draft creates. Stops and routes created in a draft count as existing
// for its later changes, so the editors offer them alongside live data.
export function createdChange(entity, key) {
  if (!state.draft) return null;
  return state.draft.changes.find((c) => c.entity === entity && c.op === "create" && c.entity_key === key) || null;
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
// server's validation entries for this change.
export async function addChange(change, { merge = true } = {}) {
  const draft = await requireDraft();
  if (!draft) return null;
  const prior = merge ? existingChange(change.entity, change.entity_key) : null;
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
  toast(errors ? `Added to "${cs.title}" with ${errors} problem${errors === 1 ? "" : "s"} to fix before submitting.`
    : `Added to draft "${cs.title}".`, errors ? "error" : "");
  return { draft: cs, problems, changeId: mine };
}
