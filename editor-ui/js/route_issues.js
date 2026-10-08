// Routes to review (docs/gtfs-editor.md section 19.2). A route here is one our
// feed and MTC's own route definition do not agree about: they list a different
// number of fare stages, so the backfill could not tell which MTC fare stage
// each of ours is and had to name the route's stages after themselves. That is
// where a stage id like `nm_SAIDAPET` comes from, and why the same real fare
// stage can exist twice - once keyed by MTC's stop id, once by its name.
//
// Nothing is missing from either side. Both are complete and they differ,
// usually because MTC has changed the route since our feed was built. So the
// screen puts the two lists side by side, marks where they part company, and
// lets the person either fix the route's stages in a draft or say our side is
// right as it stands.
import { get, post, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtCount, fmtDate, plural } from "./util.js";
import { nameHere } from "./trail.js";
import { editRouteStages } from "./stages.js";

const panel = () => document.getElementById("panel");
const PAGE = 50;
const STATUSES = [
  ["pending", "To review"],
  ["fixed", "Fixed"],
  ["confirmed", "Left alone"],
  ["superseded", "Replaced"],
];
const ISSUES = {
  count_differs: [
    "A different number of fare stages",
    "Our feed and MTC list this route with a different number of fare stages, so none of "
    + "its stages could be given MTC's id. Every stage on it is named after itself.",
  ],
  absent: [
    "Not in MTC's replica",
    "MTC's route table does not carry this route at all, so there was nothing to line it up with.",
  ],
  order_differs: [
    "The same stages in another order",
    "Both lists hold the same stages, but the route runs them in a different order, so which "
    + "stage is which cannot be read off the position.",
  ],
  set_differs: [
    "A different set of stages",
    "The two lists are the same length but hold neither the same stages nor the same order. "
    + "An equal count is not evidence, so nothing here was mapped by position.",
  ],
  ambiguous_names: [
    "The names repeat, so nothing is certain",
    "A stage name is carried more than once on one side or the other, so no stage could be "
    + "identified safely. Nothing was guessed.",
  ],
  missing_internal: [
    "MTC has this route, our feed does not",
    "The replica carries this route and its fare stages, and there is no route of that id in "
    + "our feed at all.",
  ],
  name_differs: [
    "The same stages, spelled differently",
    "The two lists line up stage for stage; some names are written differently. On chennai_bus "
    + "92% of these are one place written two ways (M.G.R.KOYAMBEDU against M.G.R.KOYAMBEDU B.T), "
    + "so these are the least urgent rows in the queue.",
  ],
};
const CONFIRM_NOTES = [
  "Checked with operations: MTC has changed this route and our feed is the one that is out of date.",
  "Our feed is right; MTC's route table is behind and it is not ours to change.",
];

const list = { feedId: null, status: "pending", issue: "", q: "", items: [], cursor: null, counts: null, loaded: false };
let flash = null;

function resetForFeed() {
  if (list.feedId === state.feedId) return;
  Object.assign(list, { feedId: state.feedId, status: "pending", issue: "", q: "", items: [], cursor: null, counts: null, loaded: false });
}

export async function refreshRouteIssueCount() {
  const badge = document.getElementById("route-issues-count");
  if (!badge || !state.feedId) return;
  try {
    list.counts = await get(`feeds/${enc(state.feedId)}/route-issues/summary`);
    const n = list.counts.pending || 0;
    badge.textContent = n ? fmtCount(n) : "";
    badge.hidden = !n;
  } catch {
    badge.hidden = true;
  }
}

const label = (r) => (r.short_name && r.short_name !== r.route_id
  ? `${r.short_name} (${r.route_id})`
  : `Route ${r.route_id}`);

function statusChip(r) {
  if (r.status === "pending") return null;
  const [, word] = STATUSES.find(([k]) => k === r.status) || [null, r.status];
  return h("span.chip", { class: r.status === "fixed" ? "ok" : "" }, word);
}

// ------------------------------------------------------------------ the queue

export async function showRouteIssuesList() {
  resetForFeed();
  nameHere("Routes to review");
  const items = h("div");
  const more = h("div.btn-row");
  const search = h("input", {
    type: "search", id: "route-issue-search", value: list.q,
    placeholder: "Route number", autocomplete: "off", spellcheck: "false",
  });
  const tabs = h("div.filter-chips", { role: "group", "aria-label": "Status" });
  const issues = h("div.filter-chips", { role: "group", "aria-label": "What is wrong" });

  const renderTabs = () => clear(tabs, STATUSES.map(([key, text]) => {
    const n = list.counts ? list.counts[key] : null;
    return h("button.route-chip", {
      type: "button", "aria-pressed": String(list.status === key),
      on: { click: () => { list.status = key; reload(); } },
    }, text, n ? h("span.count", fmtCount(n)) : null);
  }));
  const renderIssues = () => clear(issues, [["", "All"], ...Object.entries(ISSUES).map(([k, [t]]) => [k, t])]
    .map(([key, text]) => {
      const n = key && list.counts ? (list.counts.issues || {})[key] : null;
      if (key && list.status === "pending" && !n) return null;
      return h("button.route-chip", {
        type: "button", "aria-pressed": String(list.issue === key),
        on: { click: () => { list.issue = key; reload(); } },
      }, text, n ? h("span.count", fmtCount(n)) : null);
    }));

  const draw = () => {
    clear(items, list.items.length
      ? list.items.map((r) => h("article.proposal-item",
          h("h3",
            h("a", { href: `#/route-issues/${enc(r.issue_id)}` }, label(r)),
            statusChip(r),
            r.route_deleted ? h("span.chip.warn", "route deleted") : null),
          h("p.hint", (ISSUES[r.issue] || [r.issue])[0],
            " — ",
            `ours ${plural((r.ours || []).length, "fare stage")}, MTC ${(r.theirs || []).length}`),
          r.stages_unkeyed
            ? h("p.hint", h("strong", plural(r.stages_unkeyed, "stage")),
              " on this route could not be given MTC's id")
            : null,
          r.reviewed_by_email
            ? h("p.hint", `${r.status === "fixed" ? "Fixed" : "Left alone"} by ${r.reviewed_by_email}`
              + (r.reviewed_at ? ` on ${fmtDate(r.reviewed_at)}` : ""))
            : null))
      : h("p.hint", list.loaded ? "Nothing here." : "Loading…"));
    clear(more, list.cursor
      ? h("button.btn.secondary", { type: "button", on: { click: () => load(true) } }, "Show more")
      : null);
  };

  const load = async (append = false) => {
    const qs = new URLSearchParams({ status: list.status, limit: String(PAGE) });
    if (list.issue) qs.set("issue", list.issue);
    if (list.q.trim()) qs.set("q", list.q.trim());
    if (append && list.cursor) qs.set("cursor", list.cursor);
    try {
      const page = await get(`feeds/${enc(state.feedId)}/route-issues?${qs}`);
      list.items = append ? [...list.items, ...page.items] : page.items;
      list.cursor = page.next_cursor || null;
      list.loaded = true;
    } catch (e) {
      toast(e.message, "error");
    }
    draw();
  };
  const reload = () => { list.items = []; list.cursor = null; list.loaded = false; renderTabs(); renderIssues(); draw(); load(); };
  search.addEventListener("change", () => { list.q = search.value; reload(); });

  clear(panel(),
    h("section.section",
      h("h1", "Routes to review"),
      flash ? h("p.notice.ok", flash) : null,
      h("p.hint", "These routes do not line up with MTC's own route table: the two list a "
        + "different number of fare stages. Until that is settled, the stages on them are named "
        + "after themselves rather than by MTC's stop id, so a stage like SAIDAPET exists twice "
        + "— once for these routes, once for every other route through it."),
      list.counts && list.counts.stages_unkeyed
        ? h("p.hint", h("strong", plural(list.counts.stages_unkeyed, "stage")),
          " across the routes still open are named this way.")
        : null,
      tabs,
      issues,
      h("label.field", { for: "route-issue-search" }, h("span", "Search"), search)),
    h("section.section", items, more));
  flash = null;
  renderTabs();
  renderIssues();
  refreshRouteIssueCount().then(() => { renderTabs(); renderIssues(); load(); });
}

// ------------------------------------------------------------------ one route
// The two lists beside each other, lined up so the difference is the thing you
// see. Matching runs are aligned; where they part, one side shows a gap.

/// Line the two name lists up the way a diff does: the longest matching runs
/// stay level and what is left over shows as a gap on one side.
function align(ours, theirs) {
  // Matched loosely, because the two sides spell the same place differently far
  // more often than they really differ: THYAGARAYA NAGAR against THYAGARAYA
  // NAGAR B.T, GURUNANAK against GURU NANAK, GOVT. against GOVERNMENT. Lining
  // those up as one stage is what leaves the real difference visible.
  const loose = (n) => (n || "")
    .toUpperCase()
    .replace(/\bGOVT\b/g, "GOVERNMENT")
    .replace(/[^A-Z0-9]+/g, "")
    .replace(/(BT|BS|RS|DEPOT|BUSTERMINUS|TERMINUS|BUSSTAND)$/, "");
  const a = ours.map((x) => loose(x.name));
  const b = theirs.map((x) => loose(x.name));
  // longest common subsequence, then walk it back into pairs
  const n = a.length;
  const m = b.length;
  const len = Array.from({ length: n + 1 }, () => new Int32Array(m + 1));
  for (let i = n - 1; i >= 0; i -= 1) {
    for (let j = m - 1; j >= 0; j -= 1) {
      len[i][j] = a[i] === b[j] ? len[i + 1][j + 1] + 1 : Math.max(len[i + 1][j], len[i][j + 1]);
    }
  }
  const out = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      const spelt = (ours[i].name || "").trim() !== (theirs[j].name || "").trim();
      out.push([ours[i], theirs[j], true, spelt]);
      i += 1;
      j += 1;
    }
    else if (len[i + 1][j] >= len[i][j + 1]) { out.push([ours[i], null, false]); i += 1; }
    else { out.push([null, theirs[j], false]); j += 1; }
  }
  while (i < n) { out.push([ours[i], null, false]); i += 1; }
  while (j < m) { out.push([null, theirs[j], false]); j += 1; }
  return out;
}

export async function showRouteIssue(id) {
  resetForFeed();
  let r;
  try {
    r = await get(`route-issues/${enc(id)}`);
  } catch (e) {
    return clear(panel(), h("section.section",
      h("a.crumb", { href: "#/route-issues" }, "Routes to review"),
      h("p.notice.error", e.message)));
  }
  nameHere(`Route to review: ${label(r)}`);
  const editable = can("editor") && r.status === "pending";
  const [title, why] = ISSUES[r.issue] || [r.issue, ""];
  const rows = align(r.ours || [], r.theirs || []);
  const same = rows.filter(([, , ok]) => ok).length;

  const note = h("textarea", { id: "route-issue-note", rows: "2", placeholder: "What you found" });
  const draftInput = h("input", {
    type: "text", id: "route-issue-draft", placeholder: "Draft id (optional)",
    autocomplete: "off", spellcheck: "false",
    value: state.draft ? state.draft.change_set_id : "",
  });
  const close = async (decision) => {
    const text = note.value.trim();
    if (decision === "confirmed" && !text) {
      note.focus();
      return toast("Say why the route is right as it stands.", "error");
    }
    const set = draftInput.value.trim();
    if (!(await confirmDialog(
      decision === "fixed" ? `Mark ${label(r)} fixed?` : `Leave ${label(r)} as it is?`,
      decision === "fixed"
        ? "The route's stages have been put right in a draft. It comes off the queue; nothing changes for passengers until the draft is approved and committed."
        : "Nothing will be changed on this route. It comes off the queue with your note as the record of why.",
      { confirm: decision === "fixed" ? "Mark it fixed" : "Leave it alone" },
    ))) return;
    try {
      await post(`route-issues/${enc(id)}/close`, {
        decision,
        note: text || null,
        change_set: decision === "fixed" && set ? set : null,
      });
      flash = `${label(r)} is ${decision === "fixed" ? "marked fixed" : "left as it is"}.`;
      list.items = [];
      list.loaded = false;
      refreshRouteIssueCount();
      location.hash = "#/route-issues";
    } catch (e) {
      toast(e.message, "error");
    }
  };
  const reopen = h("button.btn.secondary", { type: "button", hidden: !(can("editor") && ["fixed", "confirmed"].includes(r.status)) },
    "Put it back in the queue");
  reopen.addEventListener("click", async () => {
    if (!(await confirmDialog(`Put ${label(r)} back?`, "It goes back into the queue as if nobody had looked at it.", { confirm: "Put it back" }))) return;
    try {
      await post(`route-issues/${enc(id)}/reopen`, {});
      showRouteIssue(id);
      refreshRouteIssueCount();
    } catch (e) {
      toast(e.message, "error");
    }
  });

  const unkeyed = (r.stages || []).filter((s) => s.name_keyed);
  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/route-issues" }, "Routes to review"),
      h("h1", label(r), statusChip(r)),
      r.long_name ? h("p.hint", r.long_name) : null,
      h("p.hint", h("strong", title), " — ", why),
      h("dl.facts",
        h("dt", "Our fare stages"), h("dd", String((r.ours || []).length)),
        h("dt", "MTC's fare stages"), h("dd", String((r.theirs || []).length)),
        h("dt", "In both, in the same order"), h("dd", String(same)),
        h("dt", "Stages left named after themselves"), h("dd", String(r.stages_unkeyed)),
        h("dt", "Raised by"), h("dd", r.batch),
        r.reviewed_by_email ? [h("dt", "Closed by"), h("dd", `${r.reviewed_by_email}${r.reviewed_at ? ` on ${fmtDate(r.reviewed_at)}` : ""}`)] : null,
        r.review_note ? [h("dt", "Note"), h("dd", r.review_note)] : null,
        r.change_set_id ? [h("dt", "Draft"), h("dd", h("a", { href: `#/drafts/${enc(r.change_set_id)}` }, r.change_set_title || r.change_set_id))] : null)),

    h("section.section",
      h("h2", "The two lists"),
      h("p.hint", "Ours on the left, MTC's on the right. A row on one side only is a fare stage "
        + "that side has and the other does not — that is what has to be settled. Names that are "
        + "only spelt differently are lined up together and marked, since they cost nothing."),
      h("div.two-lists",
        h("div.two-lists-head", h("strong", "Our feed"), h("strong", "MTC")),
        ...rows.map(([a, b, ok, spelt]) => h("div.two-lists-row",
          { class: ok ? (spelt ? "same spelt" : "same") : "differs" },
          h("span", { class: a ? "" : "gap" }, a ? a.name : "—"),
          h("span", { class: b ? "" : "gap" },
            b ? b.name : "—",
            b && b.bus_stop_id ? h("span.ids", b.bus_stop_id) : null))))),

    unkeyed.length ? h("section.section",
      h("h2", "The stages this route left named after themselves"),
      h("p.hint", `${plural(unkeyed.length, "stage")} on this route carries a name-based id `
        + "because the route could not be lined up. One marked “also exists with MTC's id” "
        + "is the same place as a stage the other routes already use — those are the duplicates."),
      h("ul.compact-list", unkeyed.map((s) => h("li",
        h("a", { href: `#/stage/${enc(s.stage_key)}` }, s.name),
        h("span.ids", s.stage_id),
        s.direction ? h("span.chip", s.direction) : null,
        h("span.hint", `${plural(s.stop_count, "stop")}, ${plural(s.route_count, "route")}`),
        s.twin ? h("span.chip.warn", "also exists with MTC's id") : null)))) : null,

    h("section.section",
      h("h2", "Fix it"),
      h("p.hint", "Two ways out, and which one depends on who is behind. If MTC has changed the "
        + "route and we have not, change this route's stages to match — that is the ordinary "
        + "stages editor, and it goes into a draft. If our feed is right and MTC's table is the one "
        + "out of date, leave the route alone and say so."),
      h("div.btn-row",
        h("a.btn", { href: `#/route/${enc(r.route_id)}` }, "Open the route"),
        // the stages editor opens over the route; there is no address for it
        can("editor") ? h("button.btn.secondary", { type: "button", on: { click: async () => {
          try {
            editRouteStages(await get(`feeds/${enc(state.feedId)}/routes/${enc(r.route_id)}`));
          } catch (e) {
            toast(e.message, "error");
          }
        } } }, "Change its stages") : null),
      editable ? h("p.hint", "Come back here when the change is in a draft and mark it fixed.") : null),

    editable || !reopen.hidden ? h("section.section",
      h("h2", "Close it"),
      editable ? h("p.hint", "Mark it fixed once the route's stages are right (name the draft if you like), or leave it alone if our side is right as it stands.") : null,
      editable ? h("label.field", { for: "route-issue-note" }, h("span", "Note"), note) : null,
      editable ? h("div.btn-row", ...CONFIRM_NOTES.map((t) => h("button.route-chip", {
        type: "button", on: { click: () => { note.value = t; note.focus(); } },
      }, t.slice(0, 40) + "…"))) : null,
      editable ? h("label.field", { for: "route-issue-draft" }, h("span", "Draft"), draftInput) : null,
      h("div.btn-row",
        editable ? h("button.btn", { type: "button", on: { click: () => close("fixed") } }, "Mark it fixed") : null,
        editable ? h("button.btn.secondary", { type: "button", on: { click: () => close("confirmed") } }, "Leave it alone") : null,
        reopen)) : null);
}
