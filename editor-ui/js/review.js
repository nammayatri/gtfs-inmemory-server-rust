// Drafts: the list by status, and one draft's page with its diff and the actions
// the signed-in person may take.
import { get, post, del, enc, ApiError } from "./api.js";
import { state, can } from "./state.js";
import {
  h, clear, toast, modal, confirmDialog, fmtDate, fmtCoord, fmtMetres, haversine, decodePolyline,
  STATUS_LABEL, STOP_TYPE_LABEL, diffRows,
} from "./util.js";
import * as map from "./map.js";
import { useDraft, refreshDraft, chooseDraft } from "./drafts.js";

const page = () => document.getElementById("page");
const TABS = [
  ["draft", "Drafts"], ["submitted", "Waiting for review"], ["approved", "Approved"],
  ["committed", "Live"], ["rejected", "Rejected"], ["discarded", "Discarded"],
];

// ------------------------------------------------------------------ list
export async function showDraftList(status = "draft") {
  const body = h("div");
  const tabs = h("div.tabs", { role: "tablist" }, TABS.map(([key, label]) => h("button", {
    type: "button", role: "tab", "aria-selected": String(key === status),
    on: { click: () => { location.hash = `#/drafts?status=${key}`; } },
  }, label)));
  clear(page(), h("div.page-inner",
    h("div.page-head",
      h("div.title-block", h("h1", "Drafts"), h("p.hint", "Every change goes through a draft. Someone other than the person who submits it must approve it before it is committed and goes live.")),
      can("editor") ? h("button.btn", { type: "button", on: { click: async () => {
        const cs = await chooseDraft();
        if (cs) location.hash = `#/drafts/${enc(cs.change_set_id)}`;
      } } }, "Start or open a draft") : null),
    tabs, body));
  clear(body, h("p.empty", "Loading…"));
  try {
    const res = await get(`feeds/${enc(state.feedId)}/change-sets?status=${status}&limit=100`);
    if (!res.items.length) {
      return clear(body, h("p.empty", {
        draft: "No drafts are open. Open a stop or route and choose Edit to start one.",
        submitted: "Nothing is waiting for review.",
        approved: "No approved drafts are waiting to be committed.",
        committed: "Nothing has been committed yet.",
        rejected: "No drafts have been rejected.",
        discarded: "No drafts have been discarded.",
      }[status]));
    }
    clear(body, h("div.table-wrap", h("table",
      h("thead", h("tr", h("th", "Title"), h("th", "Changes"), h("th", "Started by"), h("th", status === "draft" ? "Last edited" : "Last step"), h("th", "Status"))),
      h("tbody", res.items.map((cs) => {
        const tr = h("tr.linkrow",
          h("td", h("a", { href: `#/drafts/${enc(cs.change_set_id)}` }, cs.title)),
          h("td.num", String(cs.change_count)),
          h("td", cs.created_by_email || ""),
          h("td", fmtDate(cs.updated_at)),
          h("td", h("span", { class: `chip ${cs.status}` }, STATUS_LABEL[cs.status])));
        tr.addEventListener("click", (ev) => { if (ev.target.tagName !== "A") location.hash = `#/drafts/${enc(cs.change_set_id)}`; });
        return tr;
      })))));
  } catch (e) {
    clear(body, h("p.notice.error", e.message));
  }
}

// ------------------------------------------------------------------ one draft
export async function showDraft(id, { conflicts = null } = {}) {
  clear(page(), h("div.page-inner", h("p.empty", "Loading draft…")));
  let cs;
  try {
    cs = await get(`change-sets/${enc(id)}`);
  } catch (e) {
    return clear(page(), h("div.page-inner", h("a", { href: "#/drafts" }, "All drafts"), h("p.notice.error", e.message)));
  }
  if (state.draft && state.draft.change_set_id === cs.change_set_id && cs.status !== "draft") useDraft(null);
  const me = state.me;
  const errors = cs.validation.filter((v) => v.level === "error");
  const warnings = cs.validation.filter((v) => v.level === "warning");
  const allConflicts = conflicts || cs.conflicts || [];
  const byChange = new Map();
  cs.validation.forEach((v) => { if (!byChange.has(v.change_id)) byChange.set(v.change_id, []); byChange.get(v.change_id).push(v); });
  const conflictByChange = new Map(allConflicts.map((c) => [c.change_id, c]));

  const isAuthor = me.email === cs.created_by_email;
  const isSubmitter = me.email === cs.submitted_by_email;
  const act = async (action, body, success) => {
    try {
      await post(`change-sets/${enc(cs.change_set_id)}/${action}`, body);
      toast(success);
      if (action === "commit" || action === "discard" || action === "submit") {
        if (state.draft && state.draft.change_set_id === cs.change_set_id) useDraft(null);
      }
      if (action === "reopen" && (isAuthor || isSubmitter)) {
        const full = await get(`change-sets/${enc(cs.change_set_id)}`);
        useDraft(full);
      }
      if (action === "commit") map.refreshStops();
      showDraft(cs.change_set_id);
    } catch (e) {
      if (e instanceof ApiError && e.code === "change_set_conflicts") {
        toast(action === "commit"
          ? "Nothing was committed: some changes were overtaken by another commit."
          : "Some changes were overtaken by another commit.", "error");
        showDraft(cs.change_set_id, { conflicts: e.details.conflicts || [] });
      } else if (e instanceof ApiError && e.code === "validation_failed") {
        toast(e.message, "error");
        showDraft(cs.change_set_id);
      } else {
        toast(e.message, "error");
      }
    }
  };
  const withComment = async (title, label, required, confirm, action, success) => {
    const comment = await modal(title, (close) => {
      const ta = h("textarea", { id: "review-comment" });
      const err = h("p.notice.error", { hidden: true, role: "alert" });
      return h("form", { style: "display:grid;gap:10px", on: { submit: (ev) => {
        ev.preventDefault();
        if (required && !ta.value.trim()) { err.hidden = false; err.textContent = "Write a short reason so the author knows what to fix."; return; }
        close(ta.value.trim());
      } } }, h("label.field", { for: "review-comment" }, h("span", label), ta), err,
        h("div.btn-row", h("button.btn", { type: "submit" }, confirm), h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
    });
    if (comment !== undefined) act(action, { comment }, success);
  };

  // A main action that applies to this status is shown disabled, with the
  // reason, when this person may not take it; a housekeeping action (reopen,
  // discard) they may not take is simply not shown.
  const reasons = [];
  const button = (label, { style = "", reason = null, onClick, hideIfNot = false }) => {
    if (reason && hideIfNot) return null;
    const id = `why-${label.replace(/\W/g, "")}`;
    if (reason) reasons.push(h("li.why-not", { id }, `${label}: ${reason}`));
    return h("button.btn", { type: "button", class: style, disabled: !!reason, "aria-describedby": reason ? id : null, on: { click: onClick } }, label);
  };
  const actions = [];
  const roleReason = (role) => (can(role) ? null : `Needs the ${role} role.`);
  if (cs.status === "draft") {
    actions.push(button("Submit for review", {
      reason: roleReason("editor") || (!cs.changes.length ? "Add a change first." : errors.length ? `Fix the ${errors.length} problem${errors.length === 1 ? "" : "s"} below first.` : null),
      onClick: async () => {
        if (await confirmDialog("Submit this draft for review?", "It can no longer be edited unless it is reopened. Someone other than you must approve it.", { confirm: "Submit for review" })) {
          act("submit", undefined, "Submitted for review.");
        }
      },
    }));
    if (state.draft?.change_set_id !== cs.change_set_id && can("editor")) {
      actions.push(button("Edit in this draft", { style: "secondary", onClick: async () => { useDraft(cs); toast(`Edits now go into "${cs.title}".`); location.hash = "#/"; } }));
    }
  }
  if (cs.status === "submitted") {
    const reviewReason = roleReason("approver") || (isSubmitter ? "You submitted this change, so someone else must review it." : null);
    actions.push(button("Approve", { reason: reviewReason, onClick: () => withComment("Approve this draft", "Comment (optional)", false, "Approve", "approve", "Approved.") }));
    actions.push(button("Reject", { style: "danger", reason: reviewReason, onClick: () => withComment("Reject this draft", "What needs to change", true, "Reject", "reject", "Rejected. The author can reopen it.") }));
  }
  if (cs.status === "approved") {
    actions.push(button("Commit and go live", {
      reason: roleReason("approver") || (isSubmitter ? "You submitted this change, so someone else must commit it." : null),
      onClick: async () => {
        if (await confirmDialog("Commit this draft?", `The ${cs.changes.length} change${cs.changes.length === 1 ? "" : "s"} go live for passengers within a minute. This is recorded in the history.`, { confirm: "Commit and go live" })) {
          act("commit", undefined, "Committed. The changes are live.");
        }
      },
    }));
  }
  if (["submitted", "rejected", "approved"].includes(cs.status)) {
    actions.push(button("Reopen for editing", { style: "secondary", hideIfNot: true, reason: isAuthor || isSubmitter || me.role === "admin" ? null : "Only the person who started or submitted this draft, or an admin, can reopen it.",
      onClick: () => act("reopen", undefined, "Reopened as a draft.") }));
  }
  if (!["committed", "discarded"].includes(cs.status)) {
    actions.push(button("Discard", { style: "danger", hideIfNot: true, reason: isAuthor || me.role === "admin" ? null : "Only the person who started this draft, or an admin, can discard it.",
      onClick: async () => {
        if (await confirmDialog("Discard this draft?", "Its changes are thrown away. The draft stays in the history.", { confirm: "Discard draft", danger: true })) {
          act("discard", undefined, "Draft discarded.");
        }
      } }));
  }

  const timeline = [
    `Started by ${cs.created_by_email} on ${fmtDate(cs.created_at)}, from feed version ${cs.base_version}.`,
    cs.submitted_at ? `Submitted by ${cs.submitted_by_email} on ${fmtDate(cs.submitted_at)}.` : null,
    cs.reviewed_at ? `${cs.status === "rejected" ? "Rejected" : "Approved"} by ${cs.reviewed_by_email} on ${fmtDate(cs.reviewed_at)}${cs.review_comment ? `: "${cs.review_comment}"` : "."}` : null,
    cs.committed_at ? `Committed by ${cs.committed_by_email} on ${fmtDate(cs.committed_at)}. Feed version ${cs.committed_version} is live.` : null,
    cs.status === "discarded" ? "Discarded." : null,
  ].filter(Boolean);

  const canRemove = cs.status === "draft" && can("editor");
  clear(page(), h("div.page-inner",
    h("a", { href: "#/drafts" }, "All drafts"),
    h("div.page-head",
      h("div.title-block",
        h("h1", cs.title),
        cs.description ? h("p", cs.description) : null),
      h("span", { class: `chip ${cs.status}`, style: "font-size:14px;padding:3px 12px" }, STATUS_LABEL[cs.status])),
    h("ul.timeline", timeline.map((t) => h("li", t))),
    actions.filter(Boolean).length ? h("div.actionbar", h("div.btn-row", actions), reasons.length ? h("ul.list", reasons) : null) : null,
    allConflicts.length ? h("div.notice.error", { role: "alert" },
      h("p", h("strong", "These changes were overtaken by another commit")),
      h("p", "Someone committed a change to the same stop or route after this draft was made, so committing would overwrite their work. Reopen the draft, open each item below, redo the edit on top of what is live now, and submit again."),
      h("ul", allConflicts.map((c) => h("li", c.message)))) : null,
    errors.length ? h("div.notice.error", h("p", h("strong", `${errors.length} problem${errors.length === 1 ? "" : "s"} to fix before submitting`)), h("ul", errors.map((v) => h("li", v.message)))) : null,
    warnings.length ? h("div.notice.warning", h("p", h("strong", "Please check")), h("ul", warnings.map((v) => h("li", v.message)))) : null,
    h("h2", `${cs.changes.length} change${cs.changes.length === 1 ? "" : "s"}`),
    cs.changes.length ? cs.changes.map((ch) => changeView(cs, ch, byChange.get(ch.change_id) || [], conflictByChange.get(ch.change_id), canRemove))
      : h("p.empty", "This draft has no changes yet. Open a stop or route and choose Edit."),
  ));
}

// ------------------------------------------------------------------ change views
const ENTITY_LABEL = { stop: "Stop", route: "Route", route_stops: "Route stop list", station: "Station" };
const OP_LABEL = { create: "new", update: "changed", delete: "deleted", replace: "changed" };

function changeView(cs, ch, problems, conflict, canRemove) {
  const title = ch.entity === "route_stops" || ch.entity === "route"
    ? `${ENTITY_LABEL[ch.entity]} ${ch.entity_key}`
    : `${ENTITY_LABEL[ch.entity]} ${(ch.after && ch.after.name) || (ch.before && ch.before.name) || ch.entity_key}`;
  const link = ch.entity.startsWith("route") ? `#/route/${enc(ch.entity_key)}` : `#/stop/${enc(ch.entity_key)}`;
  const remove = async () => {
    if (!(await confirmDialog("Remove this change from the draft?", `${title} goes back to what is live.`, { confirm: "Remove change", danger: true }))) return;
    try {
      await del(`change-sets/${enc(cs.change_set_id)}/changes/${ch.change_id}`);
      await refreshDraft();
      showDraft(cs.change_set_id);
    } catch (e) {
      toast(e.message, "error");
    }
  };
  let body;
  if (ch.entity === "stop") body = stopDiff(ch);
  else if (ch.entity === "route") body = routeDiff(ch);
  else if (ch.entity === "route_stops") body = rowsDiff(ch, cs);
  else body = stationDiff(ch);
  return h("article.change",
    h("div.change-head",
      h("h3", title, " ", h("span.chip", OP_LABEL[ch.op])),
      h("div.btn-row",
        ch.op !== "delete" || ch.entity !== "stop" ? h("a.crumb", { href: link }, "Open") : null,
        canRemove ? h("button.btn.quiet.small", { type: "button", on: { click: remove } }, "Remove from draft") : null)),
    h("div.change-body",
      conflict ? h("p.notice.error", conflict.message) : null,
      problems.length ? h("div.notice", { class: problems.some((p) => p.level === "error") ? "error" : "warning" }, h("ul", problems.map((p) => h("li", p.message)))) : null,
      body));
}

function fieldRows(before, after, fields) {
  return fields.filter(([k]) => after && k in after && String(after[k] ?? "") !== String((before || {})[k] ?? "")).map(([k, label, fmt = (v) => v ?? "(empty)"]) =>
    h("tr", h("th", label), h("td.before", before ? String(fmt(before[k])) : ""), h("td.after", String(fmt(after[k])))));
}

function insetEl(spec) {
  const el = h("div.inset");
  requestAnimationFrame(() => map.inset(el, spec));
  return el;
}

function stopDiff(ch) {
  const b = ch.before, a = ch.after;
  if (ch.op === "delete") return h("p", `${b ? b.name : ch.entity_key} is removed from the feed.`);
  if (ch.op === "create") {
    return h("div.change-grid",
      h("dl.facts", h("dt", "Name"), h("dd", a.name), h("dt", "Stop id"), h("dd", a.stop_id || ch.entity_key), h("dt", "Position"), h("dd", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)),
      insetEl({ points: [{ lat: a.lat, lon: a.lon, kind: "after", label: "new" }] }));
  }
  const movedPos = b && a.lat != null && (a.lat !== b.lat || a.lon !== b.lon);
  const rows = fieldRows(b, a, [
    ["name", "Name"], ["platform_code", "Platform"], ["regional_name", "Tamil name"], ["cluster_id", "Cluster"],
  ]);
  if (movedPos) {
    rows.push(h("tr", h("th", "Position"), h("td.before", `${fmtCoord(b.lat)}, ${fmtCoord(b.lon)}`),
      h("td.after", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)} (moved ${fmtMetres(haversine(b.lat, b.lon, a.lat, a.lon))})`)));
  }
  const table = h("table.diff-table", h("thead", h("tr", h("th", ""), h("th", "Before"), h("th", "After"))), h("tbody", rows));
  if (!movedPos) return table;
  return h("div.change-grid", table, insetEl({
    points: [{ lat: b.lat, lon: b.lon, kind: "before" }, { lat: a.lat, lon: a.lon, kind: "after" }],
    lines: [{ pts: [[b.lat, b.lon], [a.lat, a.lon]], color: "#0b6660", dashed: true, weight: 2 }],
  }));
}

function routeDiff(ch) {
  const b = ch.before || {}, a = ch.after || {};
  const rows = fieldRows(b, a, [["short_name", "Route number"], ["long_name", "Route name"], ["color", "Colour"]]);
  const lineChanged = a.encoded_polyline && a.encoded_polyline !== b.encoded_polyline;
  if (lineChanged) rows.push(h("tr", h("th", "Map line"), h("td.before", b.encoded_polyline ? "saved line" : "none"), h("td.after", `new line (${a.polyline_source || "source unknown"})`)));
  const table = h("table.diff-table", h("thead", h("tr", h("th", ""), h("th", "Before"), h("th", "After"))), h("tbody", rows));
  if (!lineChanged) return table;
  const lines = [];
  try { if (b.encoded_polyline) lines.push({ pts: decodePolyline(b.encoded_polyline), color: "#9fb3b0", weight: 5 }); } catch { /* skip */ }
  try { lines.push({ pts: decodePolyline(a.encoded_polyline), color: "#0b6660", weight: 3 }); } catch { /* skip */ }
  return h("div.change-grid", table, insetEl({ lines }));
}

function rowLabel(r) {
  if (!r) return "";
  if (r.stop_type === "ROUTE CORRECTION") return `Map shaping point ${r.marker_name || r.marker_id || ""}`;
  return `${r.stop_name || r.stop_id} (${r.stop_id})`;
}

function rowsDiff(ch, cs) {
  const before = ch.before || [];
  const names = new Map(before.map((r) => [r.stop_id, r.stop_name]));
  const live = cs.stop_names || {};
  const after = ((ch.after && ch.after.rows) || []).map((r) => ({ ...r, stop_name: r.stop_name || r.stop_name_override || names.get(r.stop_id) || live[r.stop_id] }));
  const diff = diffRows(before, after);
  const counts = { added: 0, removed: 0, moved: 0, changed: 0 };
  diff.forEach((d) => { if (d.kind in counts) counts[d.kind]++; });
  const items = [];
  let run = [];
  const flush = () => {
    if (!run.length) return;
    if (run.length <= 2) run.forEach((d) => items.push(h("li", h("span.mark", ""), h("span", String(d.toIndex + 1)), h("span", rowLabel(d.after)))));
    else {
      const hidden = run;
      const li = h("li.same-run", h("span.mark", ""), h("span", ""), h("span", `${hidden.length} stops unchanged (stops ${hidden[0].toIndex + 1} to ${hidden[hidden.length - 1].toIndex + 1}) `,
        h("button", { type: "button", on: { click: () => li.replaceWith(...hidden.map((d) => h("li", h("span.mark", ""), h("span", String(d.toIndex + 1)), h("span", rowLabel(d.after))))) } }, "show")));
      items.push(li);
    }
    run = [];
  };
  diff.forEach((d) => {
    if (d.kind === "same") { run.push(d); return; }
    flush();
    if (d.kind === "added") items.push(h("li.added", h("span.mark", { "aria-label": "added" }, "+"), h("span", String(d.toIndex + 1)), h("span", `${rowLabel(d.after)}, ${STOP_TYPE_LABEL[d.after.stop_type]}, stage ${d.after.stage_no}`)));
    if (d.kind === "removed") items.push(h("li.removed", h("span.mark", { "aria-label": "removed" }, "−"), h("span", String(d.fromIndex + 1)), h("span", `${rowLabel(d.before)} removed`)));
    if (d.kind === "moved") items.push(h("li.moved", h("span.mark", { "aria-label": "moved" }, "↕"), h("span", String(d.toIndex + 1)), h("span", `${rowLabel(d.after)} moved from stop ${d.fromIndex + 1} to stop ${d.toIndex + 1}`)));
    if (d.kind === "changed") {
      const parts = [];
      if (d.before.stop_type !== d.after.stop_type) parts.push(`${STOP_TYPE_LABEL[d.before.stop_type]} to ${STOP_TYPE_LABEL[d.after.stop_type]}`);
      if (String(d.before.stage_no) !== String(d.after.stage_no)) parts.push(`stage ${d.before.stage_no} to ${d.after.stage_no}`);
      if ((d.before.stage_name || "") !== (d.after.stage_name || "")) parts.push(`stage name "${d.before.stage_name}" to "${d.after.stage_name}"`);
      items.push(h("li.changed", h("span.mark", { "aria-label": "changed" }, "~"), h("span", String(d.toIndex + 1)), h("span", `${rowLabel(d.after)}: ${parts.join(", ")}`)));
    }
  });
  flush();
  const summary = Object.entries(counts).filter(([, n]) => n).map(([k, n]) => `${n} ${k}`).join(", ") || "no differences";
  return h("div", { style: "display:grid;gap:8px" },
    h("p", `${before.length} stops before, ${after.length} after: ${summary}.`),
    h("ol.rowdiff", items));
}

function stationDiff(ch) {
  const b = ch.before, a = ch.after;
  if (ch.op === "delete") {
    return h("p", `The station ${b ? b.name : ch.entity_key} is dissolved. Its ${b && b.member_stop_ids ? b.member_stop_ids.length : ""} stops stay; only the grouping goes.`);
  }
  const was = new Set((b && b.member_stop_ids) || []);
  const now = new Set(a.member_stop_ids || [...was]);
  const added = [...now].filter((x) => !was.has(x)), removed = [...was].filter((x) => !now.has(x));
  const table = h("table.diff-table", h("thead", h("tr", h("th", ""), h("th", "Before"), h("th", "After"))), h("tbody",
    fieldRows(b, a, [["name", "Name"]]),
    b && a.lat != null && (a.lat !== b.lat || a.lon !== b.lon)
      ? h("tr", h("th", "Position"), h("td.before", `${fmtCoord(b.lat)}, ${fmtCoord(b.lon)}`), h("td.after", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)) : null,
    !b ? h("tr", h("th", "Position"), h("td.before", ""), h("td.after", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)) : null));
  return h("div.change-grid",
    h("div", { style: "display:grid;gap:10px" }, table,
      h("ol.rowdiff",
        added.map((x) => h("li.added", h("span.mark", "+"), h("span", ""), h("span", `${x} joins the station`))),
        removed.map((x) => h("li.removed", h("span.mark", "−"), h("span", ""), h("span", `${x} leaves the station`))),
        [...now].filter((x) => was.has(x)).map((x) => h("li", h("span.mark", ""), h("span", ""), h("span", `${x} stays`))))),
    a.lat != null ? insetEl({ points: [...(b && b.lat != null ? [{ lat: b.lat, lon: b.lon, kind: "before" }] : []), { lat: a.lat, lon: a.lon, kind: "after", label: a.name }] }) : null);
}
