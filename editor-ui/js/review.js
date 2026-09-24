// Drafts: the list by status, and one draft's page with its diff and the actions
// the signed-in person may take. A draft can hold thousands of changes (a bulk
// import), so its changes are filtered and paged, and maps are made only for
// the changes on screen.
import { get, post, del, enc, ApiError } from "./api.js";
import { state, can } from "./state.js";
import {
  h, clear, toast, modal, confirmDialog, fmtDate, fmtCoord, fmtMetres, fmtCount, plural, haversine, decodePolyline,
  STATUS_LABEL, STOP_TYPE_LABEL, diffRows,
} from "./util.js";
import * as map from "./map.js";
import { useDraft, refreshDraft, chooseDraft } from "./drafts.js";
import { pager } from "./importer.js";
import { DATA_SOURCE_LABEL, OVERRIDE_CHIP_STYLE } from "./admin.js";

const page = () => document.getElementById("page");
const TABS = [
  ["draft", "Drafts"], ["submitted", "Waiting for review"], ["approved", "Approved"],
  ["committed", "Live"], ["rejected", "Rejected"], ["discarded", "Discarded"],
];
const PAGE_SIZE = 50;
const NOTICE_MAX = 8;
// An admin approved a draft they submitted themselves: shown wherever its status is.
const selfApprovedChip = (cs) => (cs.self_approved
  ? h("span.chip.self-approved", { style: OVERRIDE_CHIP_STYLE, title: "Approved by the admin who submitted it. No second person reviewed it." }, "self-approved")
  : null);

// ------------------------------------------------------------------ list
// Tabs where several drafts can be put through in one go, and what that does.
const BATCH = {
  draft: [{ action: "submit", label: "Submit for review", then: null }],
  submitted: [
    { action: "approve", label: "Approve", then: null },
    { action: "approve", label: "Approve and commit", then: "commit" },
  ],
  approved: [{ action: "commit", label: "Commit and go live", then: null }],
};

// What this person may do with one draft here: `ok` to tick it, `override` when
// doing so is the admin's self-approval (they submitted it themselves, so no
// second person reviews it), `why` when they may not. The same rules as the
// draft's own page, so nothing here can do what that page would refuse.
function batchState(cs, action) {
  const me = state.me;
  if (action === "submit") {
    // any editor may submit any draft; what stops one is its own state, and the
    // problems the server finds when it is sent
    if (!can("editor")) return { ok: false, why: "needs the editor role" };
    return cs.change_count > 0
      ? { ok: true, override: false }
      : { ok: false, why: "it has no changes yet" };
  }
  if (!can("approver")) return { ok: false, why: "needs the approver role" };
  if (me.email !== cs.submitted_by_email) return { ok: true, override: false };
  // their own draft
  if (action === "approve") {
    return me.role === "admin"
      ? { ok: true, override: true, why: "you submitted it: approving it yourself skips the second reviewer" }
      : { ok: false, why: "you submitted it, so someone else must review it" };
  }
  // committing one's own draft is only for the admin who self-approved it
  return cs.self_approved && me.role === "admin"
    ? { ok: true, override: true, why: "you submitted and self-approved it" }
    : { ok: false, why: "you submitted it, so someone else must commit it" };
}

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
    // several at once, on the tabs where that means something
    const steps = BATCH[status] || [];
    const mine = new Map(res.items.map((cs) => [cs.change_set_id, batchState(cs, steps[0] && steps[0].action)]));
    // the column shows for anyone who could act on this tab at all, even when no
    // row here is theirs, so a draft that cannot be ticked says why instead of
    // simply missing
    const canBatch = steps.length > 0 && can(steps[0].action === "submit" ? "editor" : "approver");
    const chosen = new Set();
    const bar = h("div.batch-bar", { hidden: true, role: "status" });
    const allBox = h("input", { type: "checkbox", "aria-label": "Select every draft you can act on" });

    const drawBar = () => {
      const n = chosen.size;
      bar.hidden = !n;
      if (!n) { allBox.checked = false; allBox.indeterminate = false; return; }
      const eligible = res.items.filter((cs) => mine.get(cs.change_set_id).ok).length;
      allBox.checked = n === eligible;
      allBox.indeterminate = n > 0 && n < eligible;
      const picked = res.items.filter((cs) => chosen.has(cs.change_set_id));
      const changes = picked.reduce((t, cs) => t + cs.change_count, 0);
      const overrides = picked.filter((cs) => mine.get(cs.change_set_id).override).length;
      clear(bar,
        h("span", h("strong", `${plural(n, "draft")} selected`), ` · ${plural(changes, "change")}`,
          overrides ? h("span.chip.override", { style: OVERRIDE_CHIP_STYLE, title: "You submitted these, so no second person reviews them." },
            overrides === n ? "your own" : `${fmtCount(overrides)} your own`) : null),
        h("div.btn-row", ...steps.map((step) => h("button", {
          type: "button", class: `btn${overrides ? " danger" : ""}`, on: { click: () => runBatch(step) },
        }, `${step.label} ${fmtCount(n)}${overrides ? ` (${fmtCount(overrides)} as admin override)` : ""}`)),
          h("button.btn.quiet", { type: "button", on: { click: () => { chosen.clear(); redrawBoxes(); drawBar(); } } }, "Clear")));
    };
    const redrawBoxes = () => {
      body.querySelectorAll("input[data-set]").forEach((b) => { b.checked = chosen.has(b.dataset.set); });
    };
    const toggle = (id, on) => { if (on) chosen.add(id); else chosen.delete(id); drawBar(); };

    // Each draft goes through on its own, in the order shown: a commit is one
    // transaction per draft, and a later one can still be overtaken by an
    // earlier one, so the run reports each draft rather than stopping.
    const runBatch = async (step) => {
      const picked = res.items.filter((cs) => chosen.has(cs.change_set_id));
      const own = picked.filter((cs) => mine.get(cs.change_set_id).override);
      const others = picked.filter((cs) => !mine.get(cs.change_set_id).override);
      const what = step.then ? "approved and committed"
        : { commit: "committed", approve: "approved", submit: "submitted for review" }[step.action];
      // a mixed selection says which drafts are the person's own, since those
      // skip the second reviewer while the rest do not
      const ok = await confirmDialog(`${step.label} ${plural(picked.length, "draft")}?`,
        `${picked.map((cs) => cs.title).join(", ")}. `
        + (own.length
          ? `${own.length === picked.length ? "All of them were" : `${fmtCount(own.length)} of them (${own.map((cs) => cs.title).join(", ")}) ${own.length === 1 ? "was" : "were"}`} submitted by you: approving ${own.length === 1 ? "it" : "them"} yourself skips the second reviewer, and ${own.length === 1 ? "it is" : "they are"} recorded as self-approved. `
            + (others.length ? `The other ${plural(others.length, "draft")} ${others.length === 1 ? "is" : "are"} reviewed normally. ` : "")
          : "")
        + (step.action === "submit"
          ? "They can no longer be edited unless they are reopened, and someone other than the person who submitted each must approve it. One that cannot be submitted is reported and the rest carry on."
          : step.then || step.action === "commit"
            ? "Each is committed on its own, oldest first, and goes live as it commits. One that fails is reported and the rest carry on."
            : "Each is approved on its own; approving does not make anything live."),
        { confirm: step.label, danger: own.length > 0 });
      if (!ok) return;
      bar.querySelectorAll("button").forEach((b) => { b.disabled = true; });
      const done = [], failed = [];
      for (const [i, cs] of picked.entries()) {
        clear(bar, h("span", `${step.label}: ${fmtCount(i + 1)} of ${fmtCount(picked.length)}…`));
        bar.hidden = false;
        // which step it reached matters: approved-but-not-committed is a state
        // the person has to know about, since the draft is waiting to commit
        let stage = step.action;
        try {
          const isOwn = mine.get(cs.change_set_id).override;
          await post(`change-sets/${enc(cs.change_set_id)}/${step.action}`,
            step.action === "approve" ? { comment: "", ...(isOwn ? { self_approve: true } : {}) } : undefined);
          if (step.then) {
            stage = step.then;
            await post(`change-sets/${enc(cs.change_set_id)}/${step.then}`, undefined);
          }
          done.push(cs);
        } catch (e) {
          const why = e instanceof ApiError && e.code === "change_set_conflicts"
            ? "some of its changes were overtaken by another commit"
            : e instanceof ApiError && e.code === "validation_failed"
              ? "it has problems to fix first"
              : e.message;
          failed.push({ cs, why, stage, approved: stage === "commit" && step.then });
        }
      }
      if (done.length && (step.then || step.action === "commit")) map.refreshStops();
      if (state.draft && done.some((cs) => cs.change_set_id === state.draft.change_set_id)) useDraft(null);
      const selfApproved = done.filter((cs) => mine.get(cs.change_set_id).override).length;
      const note = selfApproved && step.action === "approve" ? ` ${fmtCount(selfApproved)} self-approved.` : "";
      toast(failed.length
        ? `${plural(done.length, "draft")} ${what}; ${plural(failed.length, "draft")} could not be.${note}`
        : `${plural(done.length, "draft")} ${what}.${note}`, failed.length ? "error" : "");
      await showDraftList(status);
      if (failed.length) {
        const after = document.querySelector(".page-inner .tabs");
        after?.insertAdjacentElement("afterend", h("div.notice.error", { role: "alert" },
          h("p", h("strong", `${plural(failed.length, "draft")} did not go through`)),
          h("ul", failed.map(({ cs, why, stage, approved }) => h("li",
            h("a", { href: `#/drafts/${enc(cs.change_set_id)}` }, cs.title),
            approved ? `: approved, but not committed — ${why}. It is waiting under Approved.`
              : `: not ${{ commit: "committed", approve: "approved", submit: "submitted" }[stage]} — ${why}.`))),
          done.length ? h("p", `${plural(done.length, "draft")} went through: ${done.map((cs) => cs.title).join(", ")}.`) : null));
      }
    };

    allBox.addEventListener("change", () => {
      chosen.clear();
      if (allBox.checked) res.items.forEach((cs) => { if (mine.get(cs.change_set_id).ok) chosen.add(cs.change_set_id); });
      redrawBoxes();
      drawBar();
    });

    clear(body,
      canBatch ? h("p.hint", !res.items.some((cs) => mine.get(cs.change_set_id).ok)
        ? status === "draft"
          ? "None of these can be sent for review yet: a draft needs at least one change."
          : "None of these are yours to act on: someone other than the person who submitted a draft reviews it."
        : res.items.some((cs) => mine.get(cs.change_set_id).override)
          ? "Tick the drafts to put through together. The ones marked “your draft” you submitted yourself: as an admin you may put them through, and they are recorded as self-approved."
          : status === "draft"
            ? "Tick the drafts to send for review together, or open one to see its changes."
            : "Tick the drafts to put through together, or open one to see its changes.") : null,
      h("div.table-wrap", h("table",
        h("thead", h("tr",
          canBatch ? h("th.tick", allBox) : null,
          h("th", "Title"), h("th", "Changes"), h("th", "Started by"), h("th", status === "draft" ? "Last edited" : "Last step"), h("th", "Status"))),
        h("tbody", res.items.map((cs) => {
          const state_ = mine.get(cs.change_set_id);
          const box = canBatch
            ? h("input", { type: "checkbox", "data-set": cs.change_set_id, disabled: !state_.ok,
                "aria-label": `Select ${cs.title}${state_.override ? " (your own draft: admin override)" : ""}`, title: state_.why || null,
                on: { click: (ev) => ev.stopPropagation(), change: (ev) => toggle(cs.change_set_id, ev.target.checked) } })
            : null;
          const tr = h("tr.linkrow",
            canBatch ? h("td.tick", { title: state_.why || null }, box) : null,
            h("td", h("a", { href: `#/drafts/${enc(cs.change_set_id)}` }, cs.title)),
            h("td.num", fmtCount(cs.change_count)),
            h("td", cs.created_by_email || ""),
            h("td", fmtDate(cs.updated_at)),
            h("td", h("span", { class: `chip ${cs.status}` }, STATUS_LABEL[cs.status]), " ", selfApprovedChip(cs), " ",
              state_.override && !cs.self_approved
                ? h("span.chip.override", { style: OVERRIDE_CHIP_STYLE, title: state_.why }, "your draft")
                : null));
          tr.addEventListener("click", (ev) => {
            if (ev.target.tagName === "A" || ev.target.tagName === "INPUT") return;
            location.hash = `#/drafts/${enc(cs.change_set_id)}`;
          });
          return tr;
        })))),
      bar);
  } catch (e) {
    clear(body, h("p.notice.error", e.message));
  }
}

// ------------------------------------------------------------------ one draft
const KIND_LABEL = {
  "stop:create": ["new stop", "new stops"], "stop:update": ["stop edit", "stop edits"], "stop:delete": ["stop deleted", "stops deleted"],
  "stop:merge": ["stop merge", "stop merges"], "route:create": ["new route", "new routes"], "route:update": ["route edit", "route edits"],
  "route:delete": ["route deleted", "routes deleted"], "route_stops:replace": ["stop list change", "stop list changes"],
  "station:create": ["new station", "new stations"], "station:update": ["station edit", "station edits"], "station:delete": ["station dissolved", "stations dissolved"],
  "station:merge": ["station merge", "station merges"],
  "feed_config:update": ["feed data source switch", "feed data source switches"],
};
const kindOf = (ch) => `${ch.entity}:${ch.op}`;
const kindText = (key, n) => {
  const [one, many] = KIND_LABEL[key] || [key, key];
  return `${fmtCount(n)} ${n === 1 ? one : many}`;
};

// what the page shows, kept while the same draft is open
const pageState = { id: null, page: 1, filter: "all" };
let openInsets = [];
// observers for insets not yet made (still waiting to scroll into view): drop
// these too, or one can still fire and build a map on an element the very same
// re-render already discarded, which is what left map.js reading a
// _leaflet_pos that was never set
let pendingInsets = [];

function dropInsets() {
  openInsets.forEach((m) => { try { m.remove(); } catch { /* already gone */ } });
  openInsets = [];
  pendingInsets.forEach((io) => io.disconnect());
  pendingInsets = [];
}

export async function showDraft(id, { conflicts = null } = {}) {
  dropInsets();
  if (pageState.id !== id) Object.assign(pageState, { id, page: 1, filter: "all" });
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
    const bid = `why-${label.replace(/\W/g, "")}`;
    if (reason) reasons.push(h("li.why-not", { id: bid }, `${label}: ${reason}`));
    return h("button.btn", { type: "button", class: style, disabled: !!reason, "aria-describedby": reason ? bid : null, on: { click: onClick } }, label);
  };
  const actions = [];
  const roleReason = (role) => (can(role) ? null : `Needs the ${role} role.`);
  const n = cs.changes.length;
  if (cs.status === "draft") {
    actions.push(button("Submit for review", {
      reason: roleReason("editor") || (!n ? "Add a change first." : errors.length ? `Fix the ${plural(errors.length, "problem")} below first.` : null),
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
    // the admin override of maker-checker: never the main action, always confirmed
    if (isSubmitter && me.role === "admin") {
      actions.push(h("button.btn.danger", { type: "button", id: "self-approve", on: { click: async () => {
        if (await confirmDialog("Approve your own draft?", "You submitted this draft. Approving it yourself skips the second reviewer. Continue?", { confirm: "Approve it myself", danger: true })) {
          act("approve", { self_approve: true }, "Approved by you, as an admin override. It is recorded in the history.");
        }
      } } }, "Approve it myself (admin override)"));
    }
  }
  if (cs.status === "approved") {
    actions.push(button("Commit and go live", {
      reason: roleReason("approver") || (isSubmitter && !(cs.self_approved && me.role === "admin") ? "You submitted this change, so someone else must commit it." : null),
      onClick: async () => {
        if (await confirmDialog("Commit this draft?", `The ${plural(n, "change")} go live for passengers within a minute. This is recorded in the history.`, { confirm: "Commit and go live" })) {
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
        const fromProposals = cs.changes.some((c) => c.entity === "station" && c.after && c.after.proposal_id);
        const fromReviews = cs.changes.some((c) => (c.after && c.after.position_review_id) || splitOf(cs, c));
        if (await confirmDialog("Discard this draft?", `Its changes are thrown away. The draft stays in the history.${fromProposals ? " Suggested stations approved into it go back to the review list." : ""}${fromReviews ? " Stops moved or split from coordinates to review go back to that list." : ""}`, { confirm: "Discard draft", danger: true })) {
          act("discard", undefined, "Draft discarded.");
        }
      } }));
  }

  const timeline = [
    `Started by ${cs.created_by_email} on ${fmtDate(cs.created_at)}, from feed version ${cs.base_version}.`,
    cs.submitted_at ? `Submitted by ${cs.submitted_by_email} on ${fmtDate(cs.submitted_at)}.` : null,
    cs.reviewed_at ? `${cs.status === "rejected" ? "Rejected" : "Approved"} by ${cs.reviewed_by_email} on ${fmtDate(cs.reviewed_at)}${cs.review_comment ? `: "${cs.review_comment}"` : "."}${cs.self_approved ? " This is the person who submitted it: an admin override, with no second reviewer." : ""}` : null,
    cs.committed_at ? `Committed by ${cs.committed_by_email} on ${fmtDate(cs.committed_at)}. Feed version ${cs.committed_version} is live.` : null,
    cs.status === "discarded" ? "Discarded." : null,
  ].filter(Boolean);

  const kinds = new Map();
  cs.changes.forEach((c) => kinds.set(kindOf(c), (kinds.get(kindOf(c)) || 0) + 1));
  const withProblems = new Set([...byChange.keys(), ...conflictByChange.keys()]);
  const showProblemChanges = () => { pageState.filter = "problems"; pageState.page = 1; renderChanges(); changesBox.scrollIntoView({ block: "start" }); };
  const capped = (list, cls, heading) => (list.length ? h("div.notice", { class: cls },
    h("p", h("strong", heading)),
    h("ul", list.slice(0, NOTICE_MAX).map((v) => h("li", v.message))),
    list.length > NOTICE_MAX ? h("p", `and ${fmtCount(list.length - NOTICE_MAX)} more. `, h("button.linklike", { type: "button", on: { click: showProblemChanges } }, "Show the changes with problems")) : null) : null);

  const canRemove = cs.status === "draft" && can("editor");
  const changesBox = h("section.changes", { id: "draft-changes", "aria-label": "Changes" });
  const names = draftStopNames(cs);

  function renderChanges() {
    dropInsets();
    const filter = pageState.filter;
    const filtered = cs.changes.filter((c) => filter === "all" || (filter === "problems" ? withProblems.has(c.change_id) : kindOf(c) === filter));
    const pages = Math.max(1, Math.ceil(filtered.length / PAGE_SIZE));
    pageState.page = Math.min(Math.max(1, pageState.page), pages);
    const from = (pageState.page - 1) * PAGE_SIZE;
    const slice = filtered.slice(from, from + PAGE_SIZE);
    const go = (p) => { pageState.page = p; renderChanges(); changesBox.scrollIntoView({ block: "start" }); };
    const select = h("select", { id: "change-filter", on: { change: (ev) => { pageState.filter = ev.target.value; pageState.page = 1; renderChanges(); document.getElementById("change-filter")?.focus(); } } },
      h("option", { value: "all", selected: filter === "all" }, `All changes (${fmtCount(n)})`),
      withProblems.size ? h("option", { value: "problems", selected: filter === "problems" }, `With problems (${fmtCount(withProblems.size)})`) : null,
      [...kinds.entries()].map(([key, count]) => h("option", { value: key, selected: filter === key }, kindText(key, count))));
    clear(changesBox,
      h("div.changes-head",
        h("h2", plural(n, "change")),
        n ? h("label.field.inline-field", { for: "change-filter" }, h("span", "Show"), select) : null),
      n > 1 ? h("p.hint", [...kinds.entries()].map(([key, count]) => kindText(key, count)).join(", ") + ".") : null,
      filtered.length > PAGE_SIZE ? h("p.hint", `Showing ${fmtCount(from + 1)} to ${fmtCount(from + slice.length)} of ${fmtCount(filtered.length)}.`) : null,
      pages > 1 ? pager(pageState.page, pages, go) : null,
      n ? (slice.length ? slice.map((ch) => changeView(cs, ch, byChange.get(ch.change_id) || [], conflictByChange.get(ch.change_id), canRemove, names))
        : h("p.empty", "No change matches."))
        : h("p.empty", "This draft has no changes yet. Open a stop or route and choose Edit, or use New."),
      pages > 1 ? pager(pageState.page, pages, go) : null);
  }

  clear(page(), h("div.page-inner",
    h("a", { href: "#/drafts" }, "All drafts"),
    h("div.page-head",
      h("div.title-block",
        h("h1", cs.title),
        cs.description ? h("p", cs.description) : null),
      h("span", { style: "display:inline-flex;gap:6px;align-items:center" },
        h("span", { class: `chip ${cs.status}`, style: "font-size:14px;padding:3px 12px" }, STATUS_LABEL[cs.status]),
        selfApprovedChip(cs))),
    h("ul.timeline", timeline.map((t) => h("li", t))),
    actions.filter(Boolean).length ? h("div.actionbar", h("div.btn-row", actions), reasons.length ? h("ul.list", reasons) : null) : null,
    allConflicts.length ? h("div.notice.error", { role: "alert" },
      h("p", h("strong", "These changes were overtaken by another commit")),
      h("p", "Someone committed a change to the same stop or route after this draft was made, so committing would overwrite their work. Reopen the draft, open each item below, redo the edit on top of what is live now, and submit again."),
      h("ul", allConflicts.slice(0, NOTICE_MAX).map((c) => h("li", c.message))),
      allConflicts.length > NOTICE_MAX ? h("p", `and ${fmtCount(allConflicts.length - NOTICE_MAX)} more.`) : null) : null,
    capped(errors, "error", `${plural(errors.length, "problem")} to fix before submitting`),
    capped(warnings, "warning", "Please check"),
    changesBox,
  ));
  renderChanges();
}

// Names of stops the draft creates, for stop lists that use them.
function draftStopNames(cs) {
  return new Map(cs.changes.filter((c) => c.entity === "stop" && c.op === "create" && c.after).map((c) => [c.entity_key, c.after.name]));
}

// ------------------------------------------------------------------ change views
const ENTITY_LABEL = { stop: "Stop", route: "Route", route_stops: "Route stop list", station: "Station", feed_config: "Feed data source" };
const OP_LABEL = { create: "new", update: "changed", delete: "deleted", replace: "changed", merge: "merged" };

// A stop the draft creates and puts on route stop lists in place of another stop,
// as splitting routes off in a coordinate review does: {from, routeIds}, or null.
function splitOf(cs, ch) {
  if (ch.entity !== "stop" || ch.op !== "create") return null;
  let from = null;
  const routeIds = new Set();
  for (const c of cs.changes) {
    if (c.entity !== "route_stops" || !c.after || !Array.isArray(c.after.rows) || !Array.isArray(c.before)) continue;
    c.after.rows.forEach((row, i) => {
      const was = c.before[i];
      if (row.stop_id !== ch.entity_key || !was || !was.stop_id || was.stop_id === ch.entity_key) return;
      from = from || { stop_id: was.stop_id, name: was.stop_name };
      routeIds.add(c.entity_key);
    });
  }
  return routeIds.size ? { from, routeIds: [...routeIds] } : null;
}

function changeView(cs, ch, problems, conflict, canRemove, names) {
  const a = ch.after || {}, b = ch.before || {};
  // a move made in a coordinate review reads as the move it is
  const reviewMove = ch.entity === "stop" && ch.op === "update" && a.position_review_id != null && a.lat != null && b.lat != null;
  const split = splitOf(cs, ch);
  // a merge made in a coordinate review (the stop goes into a same-named stop)
  const reviewMerge = ch.entity === "stop" && ch.op === "merge" && a.position_review_id != null;
  const feedConfig = ch.entity === "feed_config";
  let title;
  if (feedConfig) title = `Data source of feed ${ch.entity_key}`;
  else if (ch.entity === "stop" && ch.op === "merge") title = `Stop ${(b.from && b.from.name) || ch.entity_key} merged`;
  else if (ch.entity === "station" && ch.op === "merge") title = `Station ${(b.from && b.from.name) || ch.entity_key} merged`;
  else if (reviewMove) title = `Moved ${b.name || ch.entity_key} ${fmtMetres(haversine(b.lat, b.lon, a.lat, a.lon))}`;
  else if (ch.entity === "route_stops" || ch.entity === "route") title = `${ENTITY_LABEL[ch.entity]} ${ch.entity_key}${ch.entity === "route" && a.short_name && a.short_name !== ch.entity_key ? ` (${a.short_name})` : ""}`;
  else title = `${ENTITY_LABEL[ch.entity]} ${a.name || b.name || ch.entity_key}`;
  const link = feedConfig ? "#/feed-settings" : ch.entity.startsWith("route") ? `#/route/${enc(ch.entity_key)}${cs.status === "draft" ? "?draft=1" : ""}`
    : ch.op === "merge" ? `#/stop/${enc(a.into_station_id || a.into_stop_id || ch.entity_key)}` : `#/stop/${enc(ch.entity_key)}`;
  const fromProposal = ch.entity === "station" && a.proposal_id;
  const remove = async () => {
    const extra = fromProposal ? " The suggested station goes back to the list of stations to review."
      : reviewMove || reviewMerge ? " The coordinate review goes back to the list of coordinates to review."
        : split ? ` It takes the place of ${split.from.stop_id} on ${plural(split.routeIds.length, "route")} in this draft. If it was split off in a coordinate review, those route changes are removed with it and the review goes back to the list; otherwise those routes will show a problem.`
          : ch.op === "create" ? " Anything else in the draft that uses it will show a problem." : "";
    if (!(await confirmDialog("Remove this change from the draft?", `${title} goes back to what is live.${extra}`, { confirm: "Remove change", danger: true }))) return;
    try {
      await del(`change-sets/${enc(cs.change_set_id)}/changes/${ch.change_id}`);
      await refreshDraft();
      showDraft(cs.change_set_id);
    } catch (e) {
      toast(e.message, "error");
    }
  };
  let body;
  if (feedConfig) body = feedConfigDiff(ch, cs);
  else if (reviewMerge) {
    body = h("div", { style: "display:grid;gap:10px" },
      h("p", "From ", h("a", { href: `#/coordinates/${enc(a.position_review_id)}` }, `coordinate review #${a.position_review_id}`),
        `: ${(b.from && b.from.name) || ch.entity_key} (${ch.entity_key}) was probably in the wrong place, and a stop of the same name is where its routes pass.`),
      mergeDiff(ch));
  } else if (ch.entity === "stop" && ch.op === "merge") body = mergeDiff(ch);
  else if (ch.entity === "station" && ch.op === "merge") body = stationMergeDiff(ch);
  else if (reviewMove) {
    body = h("div", { style: "display:grid;gap:10px" },
      h("p", "From ", h("a", { href: `#/coordinates/${enc(a.position_review_id)}` }, `coordinate review #${a.position_review_id}`),
        `: ${b.name || ch.entity_key} (${ch.entity_key}) was probably in the wrong place.`),
      stopDiff(ch));
  } else if (split) {
    body = h("div", { style: "display:grid;gap:10px" },
      h("p", `Takes the place of ${split.from.name || split.from.stop_id} (${split.from.stop_id}) on ${plural(split.routeIds.length, "route")}: ${split.routeIds.join(", ")}. The other routes stay on ${split.from.stop_id}.`),
      stopDiff(ch));
  } else if (ch.entity === "stop") body = stopDiff(ch);
  else if (ch.entity === "route" && ch.op === "create") body = routeCreateDiff(ch, cs);
  else if (ch.entity === "route" && ch.op === "delete") body = h("p", `Route ${b.short_name || ch.entity_key} is deleted: it no longer appears in the feed.`);
  else if (ch.entity === "route") body = routeDiff(ch);
  else if (ch.entity === "route_stops") body = rowsDiff(ch, cs, names);
  else body = stationDiff(ch);
  const showOpen = !(ch.op === "delete" && ch.entity === "stop") && !(ch.op === "create" && ch.entity === "station" && cs.status !== "committed");
  return h("article.change",
    h("div.change-head",
      h("h3", title, " ", h("span.chip", reviewMove || reviewMerge ? "coordinate review" : OP_LABEL[ch.op] || ch.op)),
      h("div.btn-row",
        showOpen ? h("a.crumb", { href: link }, "Open") : null,
        canRemove ? h("button.btn.quiet.small", { type: "button", on: { click: remove } }, "Remove from draft") : null)),
    h("div.change-body",
      conflict ? h("p.notice.error", conflict.message) : null,
      problems.length ? h("div.notice", { class: problems.some((p) => p.level === "error") ? "error" : "warning" }, h("ul", problems.map((p) => h("li", p.message)))) : null,
      body));
}

// Which data GIMS serves the feed from. Nothing moves until the draft is committed.
function feedConfigDiff(ch, cs) {
  const b = ch.before || {}, a = ch.after || {};
  const label = (v) => DATA_SOURCE_LABEL[v] || v || "(unknown)";
  return h("div", { style: "display:grid;gap:10px" },
    h("table.diff-table", h("thead", h("tr", h("th", ""), h("th", "Before"), h("th", "After"))),
      h("tbody", h("tr", h("th", "Served from"), h("td.before", label(b.data_source)), h("td.after", label(a.data_source))))),
    h("p.notice", cs.status === "committed"
      ? "Committed. Every GIMS server picked the new data source up within seconds."
      : "GIMS keeps serving this feed as it does now until this draft is submitted, approved by someone else and committed."));
}

function fieldRows(before, after, fields) {
  return fields.filter(([k]) => after && k in after && String(after[k] ?? "") !== String((before || {})[k] ?? "")).map(([k, label, fmt = (v) => v ?? "(empty)"]) =>
    h("tr", h("th", label), h("td.before", before ? String(fmt(before[k])) : ""), h("td.after", String(fmt(after[k])))));
}

// A map for one change, made only when it scrolls into view and removed when the
// page of changes is replaced.
function insetEl(spec) {
  const el = h("div.inset");
  const make = () => {
    if (el.dataset.made || !document.body.contains(el)) return;
    el.dataset.made = "1";
    openInsets.push(map.inset(el, spec));
  };
  if ("IntersectionObserver" in window) {
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        io.disconnect();
        pendingInsets = pendingInsets.filter((x) => x !== io);
        make();
      }
    }, { rootMargin: "200px" });
    pendingInsets.push(io);
    requestAnimationFrame(() => io.observe(el));
  } else {
    requestAnimationFrame(make);
  }
  return el;
}

function stopDiff(ch) {
  const b = ch.before, a = ch.after;
  if (ch.op === "delete") return h("p", `${b ? b.name : ch.entity_key} is removed from the feed.`);
  if (ch.op === "create") {
    return h("div.change-grid",
      h("dl.facts",
        h("dt", "Name"), h("dd", a.name),
        h("dt", "Stop id"), h("dd", ch.entity_key || a.stop_id || "made when added"),
        h("dt", "Position"), h("dd", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`),
        a.platform_code ? [h("dt", "Platform"), h("dd", a.platform_code)] : null,
        a.description ? [h("dt", "Description"), h("dd", a.description)] : null),
      insetEl({ points: [{ lat: a.lat, lon: a.lon, kind: "after", label: "new" }] }));
  }
  const movedPos = b && a.lat != null && (a.lat !== b.lat || a.lon !== b.lon);
  const rows = fieldRows(b, a, [
    ["name", "Name"], ["platform_code", "Platform"], ["description", "Description"], ["regional_name", "Tamil name"], ["cluster_id", "Cluster"],
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

const routeLabel = (r) => (r.short_name && r.short_name !== r.route_id ? `${r.short_name} (${r.route_id})` : r.route_id);
const stopsWord = (seqs) => (seqs.length === 1 ? `stop ${seqs[0]}` : `stops ${seqs.join(", ")}`);

function mergeDiff(ch) {
  const b = ch.before || {}, a = ch.after || {};
  const from = b.from || { stop_id: ch.entity_key }, into = b.into || { stop_id: a.into_stop_id };
  const affected = b.affected || [];
  const keptName = a.keep_name === "from" ? from.name : into.name;
  const keptAt = a.keep_position === "from" ? from : into;
  const both = from.lat != null && into.lat != null;
  return h("div.change-grid",
    h("div", { style: "display:grid;gap:10px" },
      h("p", h("strong", `${from.name || from.stop_id} (${from.stop_id}) merged into ${into.name || into.stop_id} (${into.stop_id})`), `, ${plural(affected.length, "route")} updated.`),
      h("dl.facts",
        h("dt", "Stays"), h("dd", `${into.stop_id}, named ${keptName || into.stop_id}`),
        keptAt.lat != null ? [h("dt", "At"), h("dd", `${fmtCoord(keptAt.lat)}, ${fmtCoord(keptAt.lon)}, the position of ${keptAt.stop_id}`)] : null,
        both ? [h("dt", "Apart"), h("dd", fmtMetres(haversine(from.lat, from.lon, into.lat, into.lon)))] : null,
        h("dt", "Removed"), h("dd", from.stop_id)),
      affected.length
        ? h("ul.list.route-switch", affected.slice(0, 40).map((r) => h("li.list-item",
            h("span.key", r.short_name || r.route_id),
            h("a", { href: `#/route/${enc(r.route_id)}` }, r.long_name || `Route ${routeLabel(r)}`),
            h("span.hint", stopsWord(r.sequences || [])))))
        : h("p.empty", "No route used the removed stop."),
      affected.length > 40 ? h("p.hint", `and ${fmtCount(affected.length - 40)} more routes.`) : null),
    both ? insetEl({
      points: [{ lat: from.lat, lon: from.lon, color: "#b42318", label: `goes: ${from.stop_id}` }, { lat: into.lat, lon: into.lon, label: `stays: ${into.stop_id}` }],
      lines: [{ pts: [[from.lat, from.lon], [into.lat, into.lon]], color: "#14252a", dashed: true, weight: 2 }],
      maxZoom: 19,
    }) : null);
}

// A station merge: which id stays, which platforms move and where they go. No
// route row moves - a route's stop list names platforms, never a station.
function stationMergeDiff(ch) {
  const b = ch.before || {}, a = ch.after || {};
  const from = b.from || { stop_id: ch.entity_key }, into = b.into || { stop_id: a.into_station_id };
  const moving = b.moving_platforms || [];
  const keptName = a.keep_name === "from" ? from.name : into.name;
  const keptAt = a.keep_position === "from" ? from : into;
  const both = from.lat != null && into.lat != null;
  return h("div.change-grid",
    h("div", { style: "display:grid;gap:10px" },
      h("p", h("strong", `${from.name || from.stop_id} (${from.stop_id}) merged into ${into.name || into.stop_id} (${into.stop_id})`),
        `, ${plural(moving.length, "platform")} moved.`),
      h("dl.facts",
        h("dt", "Stays"), h("dd", `${into.stop_id}, named ${keptName || into.stop_id}`),
        keptAt.lat != null ? [h("dt", "At"), h("dd", `${fmtCoord(keptAt.lat)}, ${fmtCoord(keptAt.lon)}, the point of ${keptAt.stop_id}`)] : null,
        both ? [h("dt", "Apart"), h("dd", fmtMetres(haversine(from.lat, from.lon, into.lat, into.lon)))] : null,
        h("dt", "Removed"), h("dd", from.stop_id),
        h("dt", "Routes"), h("dd", "unchanged: a route calls at a platform, never at a station")),
      moving.length
        ? h("ul.list.platform-move", moving.slice(0, 40).map((p) => h("li.list-item",
            h("span.key", p.platform_code || "no label"),
            h("a", { href: `#/stop/${enc(p.stop_id)}` }, p.name || p.stop_id),
            h("span.hint", plural(p.route_count || 0, "route")),
            h("span.sub", p.stop_id))))
        : h("p.empty", "The station that goes had no platforms; the merge only removes it."),
      moving.length > 40 ? h("p.hint", `and ${fmtCount(moving.length - 40)} more platforms.`) : null),
    both ? insetEl({
      points: [{ lat: from.lat, lon: from.lon, color: "#b42318", label: `goes: ${from.stop_id}` }, { lat: into.lat, lon: into.lon, label: `stays: ${into.stop_id}` }],
      lines: [{ pts: [[from.lat, from.lon], [into.lat, into.lon]], color: "#14252a", dashed: true, weight: 2 }],
      maxZoom: 19,
    }) : null);
}

function routeCreateDiff(ch, cs) {
  const a = ch.after || {};
  const stopsChange = cs.changes.find((c) => c.entity === "route_stops" && c.entity_key === ch.entity_key);
  const count = stopsChange && stopsChange.after && Array.isArray(stopsChange.after.rows) ? stopsChange.after.rows.length : 0;
  return h("div", { style: "display:grid;gap:10px" },
    h("dl.facts",
      h("dt", "Route id"), h("dd", ch.entity_key),
      h("dt", "Route number"), h("dd", a.short_name || ""),
      a.long_name ? [h("dt", "Name"), h("dd", a.long_name)] : null,
      a.color ? [h("dt", "Colour"), h("dd", h("span.swatch", { style: `background:${/^#[0-9a-f]{6}$/i.test(a.color) ? a.color : "transparent"}` }), a.color)] : null,
      h("dt", "Stop list"), h("dd", stopsChange ? `${plural(count, "stop")}, in this draft` : "None in this draft yet.")),
    h("p.notice", cs.status === "committed"
      ? "Saved. Passengers see this route only once the nightly GTFS build gives it trips from the MTC schedule."
      : "When this draft is committed the route is saved, but passengers see it only once the nightly GTFS build gives it trips from the MTC schedule."));
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

function rowsDiff(ch, cs, draftNames) {
  const before = ch.before || [];
  const names = new Map(before.map((r) => [r.stop_id, r.stop_name]));
  const live = cs.stop_names || {};
  const after = ((ch.after && ch.after.rows) || []).map((r) => ({ ...r, stop_name: r.stop_name || r.stop_name_override || names.get(r.stop_id) || live[r.stop_id] || draftNames.get(r.stop_id) }));
  // one stop swapped for another at the same place in the list (a split in a
  // coordinate review, or Change stop) reads as one switch, not an add and a remove
  const diff = [];
  const raw = diffRows(before, after);
  for (let i = 0; i < raw.length; i++) {
    const d = raw[i], e = raw[i + 1];
    const add = d.kind === "added" ? d : e && e.kind === "added" ? e : null;
    const rem = d.kind === "removed" ? d : e && e.kind === "removed" ? e : null;
    if (e && add && rem && add !== rem && add.toIndex === rem.fromIndex && add.after.stop_type !== "ROUTE CORRECTION"
      && add.after.stop_type === rem.before.stop_type && String(add.after.stage_no) === String(rem.before.stage_no)
      && (add.after.stage_name || "") === (rem.before.stage_name || "")) {
      diff.push({ kind: "switched", before: rem.before, after: add.after, fromIndex: rem.fromIndex, toIndex: add.toIndex });
      i++;
    } else {
      diff.push(d);
    }
  }
  const counts = { added: 0, removed: 0, moved: 0, changed: 0, switched: 0 };
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
    if (d.kind === "switched") {
      items.push(h("li.changed", h("span.mark", { "aria-label": "switched" }, "⇄"), h("span", String(d.toIndex + 1)),
        h("span", `${rowLabel(d.before)} switched to ${rowLabel(d.after)}${draftNames.has(d.after.stop_id) ? ", new in this draft" : ""}`)));
    }
    if (d.kind === "changed") {
      const parts = [];
      if (d.before.stop_type !== d.after.stop_type) parts.push(`${STOP_TYPE_LABEL[d.before.stop_type]} to ${STOP_TYPE_LABEL[d.after.stop_type]}`);
      if (String(d.before.stage_no) !== String(d.after.stage_no)) parts.push(`stage ${d.before.stage_no} to ${d.after.stage_no}`);
      if ((d.before.stage_name || "") !== (d.after.stage_name || "")) parts.push(`stage name "${d.before.stage_name}" to "${d.after.stage_name}"`);
      items.push(h("li.changed", h("span.mark", { "aria-label": "changed" }, "~"), h("span", String(d.toIndex + 1)), h("span", `${rowLabel(d.after)}: ${parts.join(", ")}`)));
    }
  });
  flush();
  const summary = Object.entries(counts).filter(([, k]) => k).map(([k, v]) => `${v} ${k}`).join(", ") || "no differences";
  return h("div", { style: "display:grid;gap:8px" },
    h("p", `${plural(before.length, "stop")} before, ${after.length} after: ${summary}.`),
    h("ol.rowdiff", items));
}

function stationDiff(ch) {
  const b = ch.before, a = ch.after;
  if (ch.op === "delete") {
    return h("p", `The station ${b ? b.name : ch.entity_key} is dissolved. Its ${b && b.member_stop_ids ? b.member_stop_ids.length : ""} stops stay; only the grouping goes.`);
  }
  const labels = new Map((Array.isArray(a.members) ? a.members : []).map((m) => [m.stop_id, m.platform_code]));
  const listed = Array.isArray(a.members) ? a.members.map((m) => m.stop_id) : Array.isArray(a.member_stop_ids) ? a.member_stop_ids : null;
  const was = new Set((b && b.member_stop_ids) || []);
  const now = new Set(listed || [...was]);
  const added = [...now].filter((x) => !was.has(x)), removed = [...was].filter((x) => !now.has(x));
  const label = (x) => (labels.get(x) ? `, platform “${labels.get(x)}”` : "");
  const table = h("table.diff-table", h("thead", h("tr", h("th", ""), h("th", "Before"), h("th", "After"))), h("tbody",
    fieldRows(b, a, [["name", "Name"], ["description", "Description"]]),
    b && a.lat != null && (a.lat !== b.lat || a.lon !== b.lon)
      ? h("tr", h("th", "Position"), h("td.before", `${fmtCoord(b.lat)}, ${fmtCoord(b.lon)}`), h("td.after", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)) : null,
    !b ? h("tr", h("th", "Position"), h("td.before", ""), h("td.after", `${fmtCoord(a.lat)}, ${fmtCoord(a.lon)}`)) : null));
  return h("div.change-grid",
    h("div", { style: "display:grid;gap:10px" },
      a.proposal_id ? h("p", "From ", h("a", { href: `#/stations/${enc(a.proposal_id)}` }, `suggested station #${a.proposal_id}`), ".") : null,
      table,
      h("ol.rowdiff",
        added.map((x) => h("li.added", h("span.mark", "+"), h("span", ""), h("span", `${x} joins the station${label(x)}`))),
        removed.map((x) => h("li.removed", h("span.mark", "−"), h("span", ""), h("span", `${x} leaves the station`))),
        [...now].filter((x) => was.has(x)).map((x) => h("li", h("span.mark", ""), h("span", ""), h("span", `${x} stays${label(x)}`))))),
    a.lat != null ? insetEl({ points: [...(b && b.lat != null ? [{ lat: b.lat, lon: b.lon, kind: "before" }] : []), { lat: a.lat, lon: a.lon, kind: "after", label: a.name }] }) : null);
}
