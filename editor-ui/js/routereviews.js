// Routes to review. The queue is ordered by USE, not by defect: the busiest
// route is first, because a defect nobody rides costs nothing and the same defect
// on the most-booked route is felt all day. Each entry says what the load found
// wrong with the route; the page re-asks the server what is wrong NOW, so nobody
// is sent after something already fixed.
//
// The fix itself is made with the ordinary route editor, as a change in a draft.
// This page only records which draft it went into, and follows it from there.
import { get, post, enc, ApiError } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, modal, fmtDate, fmtMetres, fmtCount, plural } from "./util.js";
import * as map from "./map.js";
import { requireDraft, refreshDraft } from "./drafts.js";
import { nameHere } from "./trail.js";

const panel = () => document.getElementById("panel");
const STATUSES = [["pending", "To review"], ["approved", "In a draft"], ["committed", "Fixed"], ["confirmed", "Left as it is"], ["rejected", "Not worth fixing"]];
const STATUS_TEXT = Object.fromEntries(STATUSES);
const PAGE = 50;
const DRAFT_REASON = "A fix to a route is added to a draft. Nothing changes for passengers until someone else approves the draft and it is committed.";
// What a load can say is wrong with a route, in the order an operator meets them.
const REASONS = [
  ["too_few_stops", "Too few stops"],
  ["repeated_stop", "A stop listed twice"],
  ["worst_detour", "Goes a long way round"],
  ["stops_under_position_review", "Stops under review"],
  ["short_stop_list", "Short stop list"],
  ["no_polyline", "No shape"],
];
const REASON_TEXT = Object.fromEntries(REASONS);
const CONFIRM_NOTES = ["Walked it: the stop list matches the road.", "The long way round is a real diversion.", "Checked with the depot; nothing to change."];
const REJECT_NOTES = ["Not worth fixing before the next timetable change.", "Not a real problem: the data is right.", "A duplicate of another route's review."];

// The list survives opening a review and coming back, and gives "Next".
const list = { feedId: null, status: "pending", q: "", reason: "", items: [], cursor: null, counts: null, loaded: false };
// What the last action did, shown on the next screen.
let flash = null;

const routeLabel = (r) => r.route_short_name || r.route_id;

function resetForFeed() {
  if (list.feedId === state.feedId) return;
  Object.assign(list, { feedId: state.feedId, status: "pending", q: "", reason: "", items: [], cursor: null, counts: null, loaded: false });
}

// How a measure reads in a sentence. The server never assumes what was counted,
// so neither does the page: an unknown measure is printed as it comes.
export function measureText(measure, value) {
  const n = fmtCount(Math.round(value || 0));
  if (measure === "bookings") return `${n} ${plural(Math.round(value || 0), "booking")}`;
  if (measure === "trips_operated") return `${n} ${plural(Math.round(value || 0), "trip")} operated`;
  if (measure === "riders") return `${n} ${plural(Math.round(value || 0), "rider")}`;
  return `${n} ${measure.replace(/_/g, " ")}`;
}

// The pending count in the top bar.
export async function refreshRouteReviewCount() {
  const badge = document.getElementById("routereviews-count");
  if (!badge || !state.feedId) return;
  try {
    list.counts = await get(`feeds/${enc(state.feedId)}/route-reviews/summary`);
    badge.textContent = list.counts.pending ? fmtCount(list.counts.pending) : "";
    badge.hidden = !list.counts.pending;
  } catch {
    // an older server has no such queue; the link then simply carries no count
    badge.hidden = true;
  }
}

async function fetchPage({ append = false } = {}) {
  const params = new URLSearchParams({ status: list.status, limit: String(PAGE) });
  if (list.q) params.set("q", list.q);
  if (list.reason) params.set("reason", list.reason);
  if (append && list.cursor) params.set("cursor", list.cursor);
  const page = await get(`feeds/${enc(state.feedId)}/route-reviews?${params}`);
  list.items = append ? list.items.concat(page.items) : page.items;
  list.cursor = page.next_cursor;
  list.loaded = true;
}

function statusChip(r) {
  const cls = { pending: "submitted", approved: "approved", committed: "committed", confirmed: "discarded", rejected: "rejected" }[r.status] || "";
  return h("span", { class: `chip ${cls}` }, STATUS_TEXT[r.status] || r.status);
}

function flashNotice() {
  if (!flash) return null;
  const f = flash;
  flash = null;
  return h("div.notice.ok", { role: "status" },
    h("p", h("strong", f.title), f.text ? ` ${f.text}` : ""),
    f.draftId ? h("p", h("a", { href: `#/drafts/${enc(f.draftId)}` }, `Open draft “${f.draftTitle || "untitled"}”`)) : null);
}

const reasonChips = (reasons) => (Array.isArray(reasons) ? reasons : [])
  .filter((x) => x && x.code)
  .map((x) => h("span.chip.small", { dataset: { reason: x.code } }, REASON_TEXT[x.code] || x.code));

// ------------------------------------------------------------------ the list
export async function showRouteReviewsList() {
  resetForFeed();
  setLeaveGuard(null);
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  const search = h("input", { type: "search", id: "route-review-search", value: list.q, autocomplete: "off", spellcheck: "false", placeholder: "Route number or route id" });
  const tabs = h("div.tabs.count-tabs", { role: "group", "aria-label": "Show routes that are" });
  const chips = h("div.filter-chips", { role: "group", "aria-label": "Show routes queued for" });
  const items = h("div", { "aria-live": "polite" }, h("p.empty", "Loading…"));
  const more = h("button.btn.secondary", { type: "button", hidden: true }, "Show more");

  const renderTabs = () => clear(tabs, STATUSES.map(([key, label]) => h("button", {
    type: "button", "aria-pressed": String(list.status === key),
    on: { click: () => { if (list.status === key) return; list.status = key; renderTabs(); load(); } },
  }, label, list.counts ? h("span.count", fmtCount(list.counts[key] || 0)) : null)));

  const renderChips = () => {
    const counts = list.counts && list.counts.reasons;
    chips.hidden = !counts;
    if (!counts) return;
    const chip = (key, label, n) => h("button.route-chip", {
      type: "button", "aria-pressed": String(list.reason === key), dataset: { reason: key || "any" },
      on: { click: () => { if (list.reason === key) return; list.reason = key; renderChips(); load(); } },
    }, label, n != null ? h("span.count", fmtCount(n)) : null);
    clear(chips, h("span.hint", "Queued for:"), chip("", "Anything"), REASONS.map(([key, label]) => chip(key, label, counts[key] || 0)));
  };

  const renderList = () => {
    more.hidden = !list.cursor;
    if (!list.items.length) {
      const why = list.q ? `No route ${(STATUS_TEXT[list.status] || "").toLowerCase()} matches “${list.q}”.`
        : { pending: "Nothing is waiting for review.", approved: "No route fix is waiting in a draft.", committed: "No fix from this queue is live yet.", confirmed: "No route has been left as it is.", rejected: "Nothing has been set aside." }[list.status];
      return clear(items, h("p.empty", why));
    }
    clear(items, h("ol.proposal-list", { style: "list-style:none;margin:0;padding:0" }, list.items.map((r) => h("li.proposal-item",
      h("span.picker-no", { "aria-hidden": "true" }, String(r.queue_rank)),
      h("a.proposal-link", { href: `#/routes-to-review/${r.review_id}` },
        h("span.proposal-name", routeLabel(r)),
        h("span.proposal-meta", [r.route_long_name, r.route_id, measureText(r.measure, r.measure_value)].filter(Boolean).join(" · "))),
      statusChip(r),
      h("span.review-reason", reasonChips(r.reasons)),
      r.status === "approved" && r.change_set_id
        ? h("a.proposal-draft", { href: `#/drafts/${enc(r.change_set_id)}` }, `In draft “${r.change_set_title || "untitled"}”`) : null,
      r.review_note ? h("span.proposal-note", r.review_note) : null))));
  };

  let seq = 0;
  const load = async ({ append = false } = {}) => {
    const mine = ++seq;
    if (!append) clear(items, h("p.empty", "Loading…"));
    try {
      await fetchPage({ append });
      if (mine !== seq) return;
      renderList();
    } catch (e) {
      if (mine === seq) clear(items, h("p.notice.error", e.message));
    }
  };

  let typing;
  search.addEventListener("input", () => {
    clearTimeout(typing);
    typing = setTimeout(() => { list.q = search.value.trim(); load(); }, 300);
  });
  more.addEventListener("click", () => load({ append: true }));

  const window_ = list.counts && list.counts.batch;
  clear(panel(),
    h("section.section",
      flashNotice(),
      h("h1", "Routes to review"),
      h("p.hint", "The routes passengers use most, worst first, with what the last load found wrong with each. Open one, check its stops and shape against the road, then fix it on the route page — the fix goes into a draft and goes live only when someone else approves and commits it."),
      window_ ? h("p.hint", `Ranked by ${measureText(window_.measure, 0).replace(/^[\d,]+ /, "")}${window_.measure_window ? ` over ${window_.measure_window}` : ""}, loaded as “${window_.batch}”.`) : null,
      tabs,
      chips,
      h("label.field", { for: "route-review-search" }, h("span", "Search"), search)),
    h("section.section", items, more));
  renderTabs();
  renderChips();
  if (list.loaded && list.feedId === state.feedId) renderList();
  else load();
  refreshRouteReviewCount().then(() => { renderTabs(); renderChips(); });
}

// ------------------------------------------------------------------ one review
export async function showRouteReview(id) {
  resetForFeed();
  setLeaveGuard(null);
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  const here = location.hash;
  clear(panel(), h("section.section", h("p.empty", "Loading the review…")));
  let r;
  try {
    [r] = await Promise.all([get(`route-reviews/${enc(id)}`), list.loaded ? null : fetchPage().catch(() => null)]);
  } catch (e) {
    if (location.hash !== here) return;
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/routes-to-review" }, "Back to routes to review"), h("p.notice.error", e.message)));
  }
  if (location.hash !== here) return;

  const route = r.route || null;
  const alive = !!(route && !route.deleted);
  const stops = Array.isArray(r.stops) ? r.stops : [];
  const problems = Array.isArray(r.problems) ? r.problems : [];
  const ctx = r.context || {};
  const open = r.status === "pending" || r.status === "approved";
  const editable = can("editor") && open;
  nameHere(`Route review: ${routeLabel(r)}`);

  if (alive) {
    map.showRoute({
      encoded_polyline: route.encoded_polyline,
      color: route.color,
      rows: stops.map((s) => ({ ...s, stop_name: s.name, stop_type: "NEW STOP", stage_no: s.sequence })),
    });
  }

  const root = h("div");
  const actionProblems = h("div");
  const noteInput = h("input", { type: "text", id: "route-review-note", maxlength: "500", autocomplete: "off", value: r.review_note || "", placeholder: "What you found, for whoever picks this up next" });

  // previous and next in the list this came from
  const at = list.items.findIndex((x) => x.review_id === r.review_id);
  const prev = at > 0 ? list.items[at - 1] : null;
  const next = at >= 0 ? list.items[at + 1] : list.items.find((x) => x.status === "pending" && x.review_id !== r.review_id) || null;
  const goNext = () => {
    const i = list.items.findIndex((x) => x.review_id === r.review_id);
    if (i >= 0 && list.status === "pending") list.items.splice(i, 1);
    const following = list.items[i >= 0 ? i : 0];
    location.hash = following && following.status === "pending" ? `#/routes-to-review/${following.review_id}` : "#/routes-to-review";
  };
  const reload = () => { list.loaded = false; showRouteReview(r.review_id); };

  function failed(e, box) {
    const details = (e instanceof ApiError && e.details) || {};
    const code = e instanceof ApiError ? e.code : "";
    if (code === "no_change_for_route") {
      return clear(box, h("div.notice.error", { role: "alert" },
        h("p", h("strong", "That draft does not change this route yet")),
        h("p", "Open the route, make the fix there, and it goes into your draft. Then come back and record it."),
        h("div.btn-row", h("a.btn.secondary.small", { href: `#/route/${enc(r.route_id)}` }, `Open route ${routeLabel(r)}`))));
    }
    if (code === "review_in_other_draft") {
      const csId = details.change_set_id;
      return clear(box, h("div.notice.error", { role: "alert" },
        h("p", e.message),
        csId ? h("p", "In draft ", h("a", { href: `#/drafts/${enc(csId)}` }, "that draft"), ".") : null));
    }
    if (code === "review_not_open") {
      return clear(box, h("div.notice.error", { role: "alert" },
        h("p", "Someone else has already dealt with this route."),
        h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: reload } }, "Show it as it is now"))));
    }
    clear(box, h("p.notice.error", { role: "alert" }, e.message));
  }

  async function recordFix() {
    clear(actionProblems);
    const draft = await requireDraft(DRAFT_REASON);
    if (!draft) return;
    const body = { change_set_id: draft.change_set_id };
    if (noteInput.value.trim()) body.note = noteInput.value.trim();
    try {
      await post(`route-reviews/${enc(r.review_id)}/fix`, body);
      await refreshDraft().catch(() => null);
      toast(`Route ${routeLabel(r)} now follows draft “${draft.title}”.`);
      reload();
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  async function close_(action, title, question, notes, confirmWord) {
    clear(actionProblems);
    const answer = await modal(title, (done) => {
      const ta = h("textarea", { id: `${action}-note`, placeholder: "Why? (optional)" }, noteInput.value.trim());
      return h("form", { style: "display:grid;gap:10px", on: { submit: (e) => { e.preventDefault(); done(ta.value.trim()); } } },
        h("p", question),
        h("label.field", { for: `${action}-note` }, h("span", "Note (optional)"), ta),
        h("div.btn-row", h("span.hint", "Common notes:"), notes.map((n) => h("button.btn.quiet.small", { type: "button", on: { click: () => { ta.value = n; ta.focus(); } } }, n))),
        h("div.btn-row", h("button.btn", { type: "submit", id: `${action}-confirm` }, confirmWord), h("button.btn.secondary", { type: "button", on: { click: () => done(undefined) } }, "Cancel")));
    });
    if (answer === undefined) return;
    try {
      await post(`route-reviews/${enc(r.review_id)}/${action}`, answer ? { note: answer } : {});
      flash = { title: `${routeLabel(r)}: ${title.toLowerCase()}.`, text: "Nothing in the feed was changed." };
      refreshRouteReviewCount();
      goNext();
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  async function reopen() {
    try {
      await post(`route-reviews/${enc(r.review_id)}/reopen`);
      toast(`${routeLabel(r)} is back in the queue.`);
      refreshRouteReviewCount();
      reload();
    } catch (e) {
      toast(e.message, "error");
    }
  }

  async function saveNote() {
    clear(actionProblems);
    try {
      await post(`route-reviews/${enc(r.review_id)}/note`, { note: noteInput.value.trim() });
      toast("Note saved.");
    } catch (e) {
      failed(e, actionProblems);
    }
  }

  // ---- what is wrong, as the server sees it NOW
  const problemList = h("section.section",
    h("h2", `What looks wrong (${problems.length})`),
    problems.length
      ? h("ul.list", problems.map((p) => h("li.list-item",
        h("span.key", p.level === "error" ? h("span.chip.rejected", "Error") : h("span.chip", "Check")),
        h("span", p.message),
        p.stop_id ? h("span.sub", h("a", { href: `#/stop/${enc(p.stop_id)}` }, p.stop_id)) : null)))
      : h("p.notice.ok", alive ? "Nothing is wrong with this route now. If the load queued it for something already fixed, leave it as it is." : "The route is gone from the feed."),
    h("p.hint", "Checked against the route as it stands, not as it stood when the queue was loaded."));

  const loadedReasons = Array.isArray(r.reasons) ? r.reasons : [];
  const contextBox = h("section.section",
    h("h2", "Cleanup context"),
    Array.isArray(ctx.worst_detours) && ctx.worst_detours.length
      ? [h("h3", "The longest ways round"), h("ul.list", ctx.worst_detours.map((d) => h("li.list-item",
        h("span.key", fmtMetres(d.detour_m)),
        h("a", { href: `#/stop/${enc(d.stop_id)}` }, d.name || d.stop_id),
        h("span.sub", `sequence ${d.sequence}`))))] : null,
    Array.isArray(ctx.stops_with_reviews) && ctx.stops_with_reviews.length
      ? [h("h3", `Stops with a coordinate review (${ctx.stops_with_reviews.length})`),
        h("p.hint", "A route can look wrong because one of its stops is in the wrong place. Settle the stop first."),
        h("ul.list", ctx.stops_with_reviews.map((s) => h("li.list-item",
          h("span.key", s.status),
          h("a", { href: `#/coordinates/${s.review_id}` }, s.stop_id),
          h("span.sub", `sequence ${s.sequence}`))))] : null,
    Array.isArray(ctx.open_drafts) && ctx.open_drafts.length
      ? [h("h3", "Open drafts touching this route"),
        h("ul.list", ctx.open_drafts.map((d) => h("li.list-item",
          h("span.key", d.status),
          h("a", { href: `#/drafts/${enc(d.change_set_id)}` }, d.title || "untitled"),
          h("span.sub", `${d.entity}/${d.op}`))))] : null,
    !(ctx.worst_detours || []).length && !(ctx.stops_with_reviews || []).length && !(ctx.open_drafts || []).length
      ? h("p.empty", "Nothing else is known about this route.") : null);

  const statusNotice = {
    pending: null,
    approved: h("div.notice.draft",
      h("p", "A fix is in draft ", r.change_set_id ? h("a", { href: `#/drafts/${enc(r.change_set_id)}` }, `“${r.change_set_title || "untitled"}”`) : "(unknown)",
        r.reviewed_by_email ? ` by ${r.reviewed_by_email}` : "", "."),
      h("p", "It goes live when that draft is approved by someone else and committed. Remove its changes from the draft to put this route back in the queue.")),
    committed: h("p.notice.ok", "Fixed and live", r.change_set_id ? [", committed with draft ", h("a", { href: `#/drafts/${enc(r.change_set_id)}` }, `“${r.change_set_title || "untitled"}”`)] : "", "."),
    confirmed: h("div.notice",
      h("p", "Left as it is", r.reviewed_by_email ? ` by ${r.reviewed_by_email}` : "", r.reviewed_at ? ` on ${fmtDate(r.reviewed_at)}` : "", r.review_note ? `: “${r.review_note}”` : "."),
      can("editor") ? h("div.btn-row", h("button.btn.secondary.small", { type: "button", id: "route-review-reopen", on: { click: reopen } }, "Put it back in the queue")) : null),
    rejected: h("div.notice",
      h("p", "Set aside", r.reviewed_by_email ? ` by ${r.reviewed_by_email}` : "", r.review_note ? `: “${r.review_note}”` : "."),
      can("editor") ? h("div.btn-row", h("button.btn.secondary.small", { type: "button", id: "route-review-reopen", on: { click: reopen } }, "Put it back in the queue")) : null),
    superseded: h("p.notice", "A later load of routes to review replaced this one."),
  }[r.status];

  clear(panel(), root);
  root.append(
    h("section.section",
      flashNotice(),
      h("div.section-head",
        h("a.crumb", { href: "#/routes-to-review" }, "Back to routes to review"),
        h("div.btn-row.pager-mini",
          prev ? h("a.btn.quiet.small", { href: `#/routes-to-review/${prev.review_id}`, "aria-label": `Previous route: ${routeLabel(prev)}` }, "‹ Previous") : null,
          next ? h("a.btn.quiet.small", { href: `#/routes-to-review/${next.review_id}`, "aria-label": `Next route: ${routeLabel(next)}` }, "Next ›") : null)),
      h("div.title-block",
        h("h1", routeLabel(r)),
        h("p.ids", [`Route ${r.route_id}`, r.route_long_name, `review #${r.review_id}`].filter(Boolean).join(" · "))),
      h("div.review-status", statusChip(r), statusNotice),
      h("dl.facts",
        h("dt", "Why it is here"), h("dd", h("strong", `#${r.queue_rank} by use`), ` — ${measureText(r.measure, r.measure_value)}${r.measure_window ? ` over ${r.measure_window}` : ""}.`),
        h("dt", "Queued for"), h("dd.route-numbers", loadedReasons.length ? reasonChips(loadedReasons) : "nothing in particular"),
        h("dt", "Stops"), h("dd", alive ? `${plural(r.stop_count || 0, "served stop")}` : "the route is gone from the feed"),
        h("dt", "Shape"), h("dd", r.has_polyline ? "the route has one" : "none: it cannot be drawn on a map"),
        h("dt", "Loaded"), h("dd", [r.batch, r.created_at ? fmtDate(r.created_at) : null].filter(Boolean).join(", ") || "unknown")),
      alive ? h("div.btn-row", h("a.btn", { href: `#/route/${enc(r.route_id)}`, id: "open-route" }, `Open route ${routeLabel(r)} to fix it`)) : null,
      !can("editor") ? h("p.hint", "You can look at this queue. Ask an admin for the editor role to work through it.") : null),
    problemList,
    alive ? h("section.section",
      h("h2", `Stop list (${stops.length})`),
      stops.length
        ? h("ol.list", stops.map((s) => h("li.list-item",
          h("span.key", String(s.sequence)),
          h("a", { href: `#/stop/${enc(s.stop_id)}` }, s.name || s.stop_id),
          h("span.sub", s.stop_id))))
        : h("p.empty", "No stop is on this route.")) : null,
    contextBox,
    editable ? h("section.section.sticky-actions",
      actionProblems,
      h("label.field", { for: "route-review-note" }, h("span", "Note"), noteInput),
      h("p.hint", state.draft ? `Recording a fix ties this route to draft “${state.draft.title}”.` : "Recording a fix asks which draft the fix is in."),
      h("div.btn-row",
        h("button.btn", { type: "button", id: "route-review-fix", on: { click: recordFix } }, "Record the fix in my draft"),
        h("button.btn.secondary", { type: "button", id: "route-review-save-note", on: { click: saveNote } }, "Save note"),
        next ? h("a.btn.quiet", { href: `#/routes-to-review/${next.review_id}` }, "Next ›") : null),
      r.status === "pending" ? h("div.btn-row",
        h("button.btn.secondary", { type: "button", id: "route-review-confirm", on: { click: () => close_("confirm", "Route is right as it is", `${routeLabel(r)} stays as it is, and this review is closed without changing anything.`, CONFIRM_NOTES, "Leave it as it is") } }, "Route is right…"),
        h("button.btn.secondary", { type: "button", id: "route-review-reject", on: { click: () => close_("reject", "Not worth fixing", `${routeLabel(r)} is set aside. It can be put back in the queue later.`, REJECT_NOTES, "Set it aside") } }, "Not worth fixing…")) : null) : null);
}
