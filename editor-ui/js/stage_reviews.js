// Stages to review. The stage backfill flags a stage name whose routes do not
// agree about what it means: they begin it at different stops, or they disagree
// about the stops between the fare boundaries. One row here is one NAME, however
// many stages carry it, because that is one decision to make.
//
// A person opens it, sees each stage of the name side by side on the map with the
// routes that run it, then either fixes it - renaming the stages apart, merging
// duplicate stops, correcting a stage's stops, all through the editing that
// already exists - and closes the row naming the draft, or confirms that the
// routes really do differ and says why. Closing takes the flag off the stages.
import { get, post, enc } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, confirmDialog, fmtCount, fmtDate, plural } from "./util.js";
import * as map from "./map.js";
import { addChange, removeChange, requireDraft, draftPicker, stageChanges, sameStageKey, stageKey } from "./drafts.js";
import { stagesLike, draftMerges, finalStage, draftTouchesStage, stageBrief, undoChange, alsoClose, fixedByNotice } from "./stage_like.js";
import { stopPicker } from "./picker.js";
import { nameHere } from "./trail.js";

const panel = () => document.getElementById("panel");
const PAGE = 50;
const STATUSES = [
  ["pending", "To review"],
  ["fixed", "Fixed"],
  ["confirmed", "Left alone"],
  ["superseded", "Superseded"],
];
const STATUS_TEXT = Object.fromEntries(STATUSES);
// Why a name is here, and what the person is being asked to decide. Worst first,
// the same order the server lists them in.
const AGREED = "agreed";
// the two sides of the work: names somebody has to settle, and names that mapped
// cleanly and only want looking over
const SIDES = [
  ["settle", "To settle", "The routes disagree about these, so somebody has to decide what the stage is."],
  ["verify", "Mapped cleanly", "Every route gave these the same stops. Look them over and say they are right."],
];
const REASONS = [
  ["head_differs", "Different first stop", "The routes begin this stage at stops with different names, so the name is being used for more than one place. Give the stages names that tell them apart."],
  ["head_duplicate_stops", "Duplicate stops", "The routes begin this stage at different stop records that all carry one name. Merge the stops and the stages come right on their own."],
  ["stretch_differs", "Different stops after it", "The routes agree where the stage begins and disagree about the stops between the boundaries. Decide which stops it really has."],
];
const REASON_TEXT = Object.fromEntries(REASONS.map(([k, label]) => [k, label]));
const REASON_WHY = Object.fromEntries(REASONS.map(([k, , why]) => [k, why]));
const CONFIRM_NOTES = [
  "Checked on the map: the routes take different roads, so these really are separate stages.",
  "Two places that share a name; the stops are right as they are.",
];
const COLOURS = ["#1f6feb", "#c2410c", "#15803d", "#7e22ce", "#b91c1c", "#0e7490"];

// The list survives opening a review and coming back, and gives "Next".
// `big` opens the queue on the reviews carrying most of the difference; the
// small ones are still there, behind the toggle.
const list = { feedId: null, side: "settle", status: "pending", reason: "", q: "", big: true, lost: false, items: [], cursor: null, counts: null, loaded: false };
let flash = null;

function resetForFeed() {
  if (list.feedId === state.feedId) return;
  Object.assign(list, { feedId: state.feedId, side: "settle", status: "pending", reason: "", q: "", big: true, lost: false, items: [], cursor: null, counts: null, loaded: false });
}

export function leaveStageReviews() {
  for (let i = 0; i < COLOURS.length; i += 1) map.clearRoute(`stage-review-${i}`);
}

export async function refreshStageReviewCount() {
  const badge = document.getElementById("stage-reviews-count");
  if (!badge || !state.feedId) return;
  try {
    list.counts = await get(`feeds/${enc(state.feedId)}/stage-reviews/summary`);
    const n = ((list.counts.settle || {}).pending) || 0;
    badge.textContent = n ? fmtCount(n) : "";
    badge.hidden = !n;
  } catch {
    badge.hidden = true;
  }
}

async function fetchPage({ append = false } = {}) {
  // one row per STAGE ID, its two directions inside. Names are not grouped: MTC
  // gives 160 names to more than one stop, and those are different places.
  // The side goes to the server as itself: "Anything" on the settle side is
  // every reason but mapped cleanly, which a stage that mapped cleanly is not.
  const params = new URLSearchParams({ status: list.status, limit: String(PAGE), group: "stage", side: list.side });
  if (list.q) params.set("q", list.q);
  if (list.side === "settle" && list.reason) params.set("reason", list.reason);
  const worth = list.counts && list.counts.worth_a_look;
  if (list.side === "settle" && list.big && list.status === "pending" && worth) {
    params.set("min_impact", String(worth.threshold));
  }
  // marked fixed with a draft that was then discarded or rejected
  if (list.status === "fixed" && list.lost) params.set("lost", "true");
  if (append && list.cursor) params.set("cursor", list.cursor);
  const page = await get(`feeds/${enc(state.feedId)}/stage-reviews?${params}`);
  list.items = append ? list.items.concat(page.items) : page.items;
  list.cursor = page.next_cursor;
  list.loaded = true;
}

function statusChip(r) {
  const cls = { pending: "submitted", fixed: "committed", confirmed: "discarded", superseded: "discarded" }[r.status] || "";
  return h("span", { class: `chip ${cls}` }, STATUS_TEXT[r.status] || r.status);
}

const directionWord = (d) => (d ? `direction: ${d}` : "either direction");

function reviewMeta(r) {
  const ev = r.evidence || {};
  return [
    r.impact ? `${plural(r.impact, "stop call")} wrong` : null,
    directionWord(r.direction),
    ev.lists ? plural(ev.lists, "stage") : null,
    Array.isArray(ev.routes) && ev.routes.length ? `${plural(ev.routes.length, "route")} disagree` : null,
    Array.isArray(ev.head_names) && ev.head_names.length > 1 ? `starts at ${ev.head_names.slice(0, 2).join(" or ")}` : null,
    Array.isArray(ev.stops) && ev.stops.length ? `${plural(ev.stops.length, "stop")} off the longest list` : null,
    Array.isArray(ev.spellings) && ev.spellings.length ? ev.spellings.join(" / ") : null,
  ].filter(Boolean).join(" · ");
}

function flashNotice() {
  if (!flash) return null;
  const f = flash;
  flash = null;
  return h("div.notice.ok", { role: "status" },
    h("p", h("strong", f.title), f.text ? ` ${f.text}` : ""),
    f.draftId ? h("p", h("a", { href: `#/drafts/${enc(f.draftId)}` }, `Open draft “${f.draftTitle || "untitled"}”`)) : null);
}

// ------------------------------------------------------------------ the list
export async function showStageReviewsList({ q = null } = {}) {
  resetForFeed();
  // a stage's page links here by its name
  if (q !== null && q !== list.q) Object.assign(list, { q, items: [], cursor: null, loaded: false });
  setLeaveGuard(null);
  leaveStageReviews();
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  nameHere("Stages to review");

  const search = h("input", { type: "search", id: "stage-review-search", value: list.q, autocomplete: "off", spellcheck: "false", placeholder: "Stage name or stage id" });
  const tabs = h("div.tabs.count-tabs", { role: "group", "aria-label": "Show reviews that are" });
  const reasons = h("div.filter-chips", { role: "group", "aria-label": "Show reviews whose reason is" });
  const scope = h("div.filter-chips", { role: "group", "aria-label": "How much of the queue to show" });
  const sides = h("div.tabs.count-tabs", { role: "group", "aria-label": "Which side of the work" });
  const sideHint = h("p.hint");
  const lostNotice = h("div.notice.error", { hidden: true, role: "status" });
  const items = h("div", { "aria-live": "polite" }, h("p.empty", "Loading…"));
  const more = h("button.btn.secondary", { type: "button", hidden: true }, "Show more");

  // every count here is a count of NAMES, because that is what the list shows.
  // A name is counted under a status when any of its stages is in it, so a
  // half-finished name is in two of them and they do not sum to the total.
  const sideCounts = () => (list.counts && list.counts[list.side]) || {};
  const renderTabs = () => clear(tabs, STATUSES.map(([key, label]) => h("button", {
    type: "button", "aria-pressed": String(list.status === key),
    on: { click: () => { if (list.status === key) return; list.status = key; list.lost = false; renderTabs(); renderScope(); load(); } },
  }, label, list.counts ? h("span.count", fmtCount(sideCounts()[key] || 0)) : null)));

  const renderReasons = () => {
    const counts = list.counts && list.counts.reason;
    const chip = (key, label, n) => h("button.route-chip", {
      type: "button", "aria-pressed": String(list.reason === key), dataset: { reason: key || "any" },
      on: { click: () => { if (list.reason === key) return; list.reason = key; renderReasons(); load(); } },
    }, label, n != null ? h("span.count", fmtCount(n)) : null);
    clear(reasons, h("span.hint", "Because:"), chip("", "Anything"),
      REASONS.map(([key, label]) => chip(key, label, counts ? counts[key] || 0 : null)));
  };

  const renderSide = () => {
    clear(sides, SIDES.map(([key, label]) => h("button", {
      type: "button", "aria-pressed": String(list.side === key),
      on: { click: () => { if (list.side === key) return; list.side = key; renderSide(); renderTabs(); renderScope(); renderReasons(); load(); } },
    }, label, h("span.count",
      fmtCount((((list.counts || {})[key]) || {})[list.status] || 0)))));
    const why = SIDES.find(([k]) => k === list.side);
    sideHint.textContent = why ? why[2] : "";
    reasons.hidden = list.side !== "settle";
  };

  const renderScope = () => {
    const w = sideCounts().worth_a_look;
    const lost = sideCounts().fixed_lost || 0;
    // A fix whose draft was thrown away never went live: on the Fixed tab
    // those are a filter of their own, and the queue says how many there are.
    if (list.status === "fixed") {
      scope.hidden = !lost && !list.lost;
      const chip = (on, label, n) => h("button.route-chip", {
        type: "button", "aria-pressed": String(list.lost === on),
        on: { click: () => { if (list.lost === on) return; list.lost = on; renderScope(); load(); } },
      }, label, n != null ? h("span.count", fmtCount(n)) : null);
      clear(scope, chip(false, "All fixed", sideCounts().fixed || 0), chip(true, "Draft thrown away", lost));
      return;
    }
    lostNotice.hidden = !(lost && list.status === "pending");
    if (!lostNotice.hidden) {
      clear(lostNotice, h("p", h("strong", `${plural(lost, "stage")} marked fixed with a draft that was then discarded or rejected.`),
        " Those fixes never went live. ",
        h("button.btn.quiet.small", { type: "button", on: { click: () => {
          list.status = "fixed"; list.lost = true; renderTabs(); renderScope(); load();
        } } }, "See them")));
    }
    scope.hidden = !w || list.status !== "pending" || list.side !== "settle";
    if (!w) return;
    const chip = (big, label, n) => h("button.route-chip", {
      type: "button", "aria-pressed": String(list.big === big),
      on: { click: () => { if (list.big === big) return; list.big = big; renderScope(); load(); } },
    }, label, h("span.count", fmtCount(n)));
    clear(scope,
      chip(true, `Worth a look \u2014 ${w.share_of_difference}% of the difference`, w.names),
      chip(false, "Everything, small ones too", w.names + w.small));
  };

  const renderList = () => {
    more.hidden = !list.cursor;
    if (!list.items.length) {
      const why = list.q
        ? `No stage ${list.status === "pending" ? "still to review" : "here"} matches \u201c${list.q}\u201d.`
        : list.side === "verify"
          ? "Nothing left to look over."
          : "Nothing is waiting to be settled.";
      return clear(items, h("p.empty", why));
    }
    clear(items, h("ul.proposal-list", list.items.map((r) => {
      const ways = (r.directions || []).length
        ? `${(r.directions || []).join(" and ")}`
        : "either way";
      const reasons = (r.reasons || []).filter((x) => x !== AGREED)
        .map((x) => REASON_TEXT[x] || x);
      // on the queue: the other way of the stage already closed, and a draft
      // that changed the stage and went live after it was raised
      const open = list.status === "pending";
      const closedWays = open ? (r.closed_ways || []).map((w) => `${w.direction || "either way"} ${w.status === "fixed" ? "fixed" : "left alone"}`) : [];
      const changed = open && r.changed ? r.changed : null;
      return h("li.proposal-item",
        h("a.proposal-link", { href: `#/stage-reviews/${r.review_id}` },
          h("span.proposal-name", r.name || r.stage_id,
            h("span.muted", ` \u00b7 stage ${r.stage_id}`)),
          h("span.proposal-meta", [
            r.impact ? `${plural(r.impact, "stop call")} wrong` : null,
            ways,
            r.done ? `${r.done} of ${r.parts} done` : null,
            closedWays.length ? `${closedWays.join(", ")} already` : null,
          ].filter(Boolean).join(" \u00b7 "))),
        r.left_to_do
          ? h("span.chip.submitted", r.left_to_do === r.parts
              ? "To review"
              : `${r.left_to_do} left`)
          : h("span.chip.committed", "Done"),
        r.lost ? h("span.chip.warn", { title: "Marked fixed with a draft that was discarded or rejected: the fix never went live." }, "draft thrown away") : null,
        r.left_to_do && draftTouchesStage(r.stage_id)
          ? h("span.chip.draft", { title: "Your open draft already changes this stage. Open it to finish and mark it fixed." }, "in your draft") : null,
        changed
          ? h("span.chip.warn", { title: `Draft \u201c${changed.title}\u201d changed this stage after it was raised, and went live on ${fmtDate(changed.committed_at)}. If that fixed it, open it and mark it fixed.` },
            "changed since") : null,
        reasons.length ? h("span.review-reason", reasons.join(" \u00b7 ")) : null);
    })));
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

  clear(panel(),
    h("section.section",
      flashNotice(),
      h("h1", "Stages to review"),
      h("p.hint", "One row per stage id, its up and down inside \u2014 switch between "
        + "them without coming back here. A name can belong to more than one stage id, "
        + "so it is more than one row: GOVT ESTATE METRO R.S is stages 143, 148, 170 "
        + "and 1834, and whether any of them are the same place is what merging is for."),
      sides,
      sideHint,
      // Said once, at the top of the queue, rather than left for somebody to
      // work out from a prefix: everything here that is a stand-in or a guess.
      h("details.filled-in",
        h("summary", "What the backfill filled in, and what is only a stand-in"),
        h("ul.compact-list",
          h("li", h("strong", "The stops in each stage are a guess."),
            " Where the routes gave a stage different lists, one was chosen and written to "
            + "them all. Every row below says how many stop calls that guess gets wrong; "
            + "that is what you are settling."),
          h("li", h("strong", "A stage id starting nm_ is not MTC's."),
            " The backfill could not line those routes up with MTC's own route, so the "
            + "stage was named after itself. The same place may also exist under MTC's id. ",
            h("a", { href: "#/route-issues" }, "Routes to review")),
          h("li", h("strong", "A stage id starting stg_, or a stop id starting ed_, was made here"),
            " in the editor, not taken from MTC's data."),
          h("li", h("strong", "A \u201cmap point\u201d is not a stop."),
            " It is a position the line is drawn through so it follows the road. No passenger "
            + "boards there and it is in no GTFS file, so a list that differs only by one is "
            + "not a disagreement about where the bus stops."))),
      lostNotice,
      tabs,
      scope,
      reasons,
      h("label.field", { for: "stage-review-search" }, h("span", "Search"), search)),
    h("section.section", items, more));
  renderTabs();
  renderReasons();
  renderScope();
  renderSide();
  refreshStageReviewCount().then(() => {
    renderTabs(); renderReasons(); renderScope(); renderSide(); load();
  });
}

/// A name that tells this stage from the others of its name. The routes here
/// disagree about where the stage begins or where it goes, so what tells them
/// apart is exactly that: the stop it starts at, or the one it ends at.
function suggestName(stage, siblings) {
  const served = (s) => (s.rows || []).filter((r) => r.stop_type !== "ROUTE CORRECTION");
  const mine = served(stage);
  if (!mine.length) return stage.name;
  const base = stage.name.trim();
  const same = (a, b) => (a || "").trim().toUpperCase() === (b || "").trim().toUpperCase();
  const head = mine[0].stop_name || mine[0].stop_id;
  // the first stop, when it is not just the stage's own name again and no
  // sibling starts there too
  if (!same(head, base) && !siblings.some((o) => o !== stage && same(served(o)[0]?.stop_name, head))) {
    return `${base} (from ${head})`;
  }
  const tail = mine[mine.length - 1].stop_name || mine[mine.length - 1].stop_id;
  if (!same(tail, base) && !siblings.some((o) => o !== stage && same(served(o).at(-1)?.stop_name, tail))) {
    return `${base} (to ${tail})`;
  }
  return mine.length > 1 ? `${base} (via ${mine[1].stop_name || mine[1].stop_id})` : base;
}

// ------------------------------------------------------------------ one review
// The whole of the review: the stage's stops, as a list the person builds. It
// starts as the list the backfill guessed - the one most routes give it - with
// every other list the routes give offered beside it, and a search for any stop
// in the feed. Saving puts a stage/update in the draft; committing that is what
// updates the stop mapping for every route using the stage.

/// Ids the editor or the backfill made up, said plainly wherever one is shown.
/// A reviewer should never have to know a prefix to tell a real id from a
/// stand-in: `nm_` is a stage the backfill could not match to MTC's own fare
/// stage, `stg_` one somebody made here, and anything else is MTC's.
export function idKind(id) {
  if (typeof id !== "string") return null;
  if (id.startsWith("nm_")) {
    return h("a.chip.warn", {
      href: "#/route-issues",
      title: "The backfill could not line this route up with MTC's own route, so "
        + "the stage was named after itself instead of being given MTC's stop id. "
        + "The same fare stage may also exist under MTC's id. Open Routes to review.",
    }, "name-based id");
  }
  if (id.startsWith("stg_")) return h("span.chip", { title: "Made in the editor, not one of MTC's fare-stage stops." }, "made here");
  if (id.startsWith("ed_")) return h("span.chip", { title: "A stop created in the editor, not one from MTC's data." }, "made here");
  return null;
}

const stopLabel = (r) => (r.stop_type === "ROUTE CORRECTION"
  ? `map point ${r.marker_name || r.marker_id || ""}`.trim()
  : r.stop_name || r.stop_id);

function neighbours(label, list) {
  if (!list || !list.length) return null;
  return h("p.hint.neighbours", h("span.muted", `${label} `),
    ...list.map((n, i) => [
      i ? ", " : "",
      h("strong", n.name),
      h("span.muted", ` \u00d7${n.routes}`),
    ]));
}

// `label(id)` names a route by its number and id, `moved` is what the draft
// has already done with the list's routes: {to, count} or null.
function candidateCard(c, onUse, inList, split, moved, label) {
  const movedTo = moved && moved.count >= (c.routes || []).length ? moved.to : null;
  const names = c.stop_names || [];
  const same = c.stops.length === inList.length
    && c.stops.every((id, i) => id === inList[i]);
  const known = (c.routes || []).length;
  // the review carries at most a few of a list's routes; a new stage can only
  // be handed the ones it names
  const allKnown = known > 0 && known >= (c.route_count || 0);
  return h("li.stop-choice", { class: same ? "stop-choice is-current" : "stop-choice" },
    h("div.stop-choice-head",
      h("strong", plural(c.stops.length, "stop")),
      h("span.hint", plural(c.route_count, "route")),
      same ? h("span.chip", "the list above") : null,
      c.chosen && !same ? h("span.chip", "what the backfill chose") : null,
      movedTo ? h("span.chip.draft", `moved to ${movedTo} in your draft`) : null,
      moved && !movedTo ? h("span.chip.draft", `${moved.count} of its routes moved to ${moved.to} in your draft`) : null),
    h("ol.stop-choice-stops", names.map((name, i) => h("li",
      { class: inList.includes(c.stops[i]) ? "" : "is-new" }, name))),
    // where these routes came from and where they go next: a stage the routes
    // disagree about usually disagrees because they take different roads after
    // the boundary, so this is what says which list belongs to which road
    neighbours("comes from", c.comes_from),
    neighbours("then goes to", c.goes_to),
    h("p.hint", `${(c.routes || []).length === 1 && c.route_count <= 1 ? "Route" : "Routes"} `
      + `${(c.routes || []).slice(0, 6).map(label).join(", ")}`
      + (c.route_count > 6 ? ` and ${c.route_count - 6} more` : "")),
    onUse ? h("div.btn-row",
      h("button.btn.secondary.small", {
        type: "button", disabled: same,
        on: { click: () => onUse(c) },
      }, same ? "Already the list above" : "Use this list"),
      // The other way out of a disagreement: these routes are not wrong, they
      // are a different stage. Give them one of their own and leave the rest
      // of the routes on this one.
      split && !split.open && !movedTo ? h("button.btn.quiet.small", {
        type: "button", disabled: !allKnown,
        title: allKnown ? null
          : `The review names only ${known} of this list's ${c.route_count} routes, so they cannot all be moved.`,
        on: { click: () => split.start(c) },
      }, "Create a new stage") : null) : null,
    split && split.open ? split.form(c, allKnown) : null);
}

// A stage's stops being edited: reordered by dragging the handle (or with the
// arrow keys while the handle has focus), a stop changed for another (✎), taken
// out (✕, and Undo puts it back) or added. The review's stage and a new stage
// being made from one of the lists both use this, so the two edit alike.
// `onChange` runs after every redraw; `beforePick` before the stop search opens,
// so a page with two of these can close the other one's search.
function stopListEditor({ rows: start, editable, idPrefix, layer, color, empty, what, onChange, beforePick }) {
  let rows = (start || []).map((x) => ({ ...x }));
  // A removed stop is kept in the list, struck through, until the save: the
  // reviewer can put it back. Only these reach the draft.
  const kept = () => rows.filter((x) => !x.removed);
  const retype = () => {
    let first = true;
    for (const x of rows) {
      if (x.removed || x.stop_type === "ROUTE CORRECTION") continue;
      x.stop_type = first ? "NEW STOP" : (x.stop_type === "NEW STOP" ? "INTERMEDIATE STOP" : x.stop_type);
      first = false;
    }
  };
  const listEl = h("ol.stage-stops.stage-build");
  const pickerSlot = h("div");

  // Reordering: dragged by the handle, or moved with the arrow keys while the
  // handle has focus. `dropAt` is only a highlight, so it is painted straight
  // onto the list - a redraw mid-drag would throw away the row being dragged.
  let dragFrom = null;
  let dropAt = null;
  let focusGrip = null;
  const paintDrop = () => {
    for (const li of listEl.children) {
      li.classList.toggle("drop-here", dropAt !== null && li.dataset.i === String(dropAt));
    }
  };
  const moveTo = (from, to) => {
    dragFrom = null;
    dropAt = null;
    if (from === null || to === from || to < 0 || to >= rows.length) return paintDrop();
    const [row] = rows.splice(from, 1);
    rows.splice(to, 0, row);
    retype();
    focusGrip = to;
    closePicker();
    draw();
  };

  // stopPicker builds the search and hands it back to be placed; it does not
  // put itself anywhere, so it is mounted under the list while it is open.
  // Adding and changing share it: the only difference is where the stop lands.
  let picker = null;
  const closePicker = () => {
    if (picker) picker.close();
    picker = null;
    clear(pickerSlot);
    addBtn.hidden = false;
  };
  const openPicker = (opts) => {
    closePicker();
    if (beforePick) beforePick(api);
    addBtn.hidden = true;
    picker = stopPicker({ ...opts, onCancel: closePicker });
    clear(pickerSlot, picker.el);
    picker.focus();
    pickerSlot.scrollIntoView({ block: "nearest" });
  };
  const addStop = () => {
    if (picker) return closePicker();
    openPicker({
      title: `Add a stop to ${what}`,
      exclude: kept().map((x) => x.stop_id).filter(Boolean),
      excludeReason: `it is already in ${what}.`,
      mapMessage: `Click the stop to add it to the end of ${what}.`,
      onPick: (stop) => {
        rows.push({
          stop_id: stop.stop_id,
          stop_type: kept().length ? "INTERMEDIATE STOP" : "NEW STOP",
          stop_name: stop.name,
          lat: stop.lat,
          lon: stop.lon,
        });
        closePicker();
        draw();
        toast(`Added ${stop.name} as stop ${kept().length} of ${what}.`);
      },
    });
  };
  // The pencil: the stage still calls somewhere at this point in its order, at
  // a different stop. The row keeps its place and its kind; only the stop moves.
  const changeStop = (i) => {
    const was = rows[i];
    openPicker({
      title: `Change stop ${i + 1}, ${stopLabel(was)}, for another`,
      near: was.lat != null && was.lon != null ? { lat: was.lat, lon: was.lon, label: stopLabel(was) } : null,
      exclude: rows.map((x, n) => (n === i || x.removed ? null : x.stop_id)).filter(Boolean),
      excludeReason: `it is already in ${what}.`,
      mapMessage: "Click the stop this one becomes.",
      onPick: (stop) => {
        rows[i] = {
          ...was,
          stop_id: stop.stop_id,
          stop_name: stop.name,
          lat: stop.lat,
          lon: stop.lon,
          stop_name_override: null,
        };
        closePicker();
        focusGrip = i;
        draw();
        toast(`Stop ${i + 1} is now ${stop.name}.`);
      },
    });
  };
  const addBtn = h("button.btn.secondary", { type: "button", on: { click: addStop } }, "Add a stop");

  const draw = () => {
    map.clearRoute(layer);
    const live = kept();
    if (live.some((x) => x.lat != null)) {
      map.showRoute({ route_id: layer, rows: live }, { layer, fit: false, color, weight: 4 });
      map.fitPoints(live.filter((x) => x.lat != null));
    }
    clear(listEl, rows.length
      ? rows.map((row, i) => h("li.stop-row", {
          draggable: editable && !row.removed,
          class: row.removed ? "is-removed" : "",
          dataset: { i: String(i) },
          // hovering a row shows you where it is, which is the question the
          // list cannot answer on its own
          on: {
            mouseenter: () => { if (row.lat != null) map.focusStop(row, { zoom: 16 }); },
            mouseleave: () => map.clearFocus(),
            ...(editable && !row.removed ? {
            dragstart: (ev) => {
              dragFrom = i;
              ev.dataTransfer.effectAllowed = "move";
              // Firefox starts no drag without a payload; the index rides along
              ev.dataTransfer.setData("text/plain", String(i));
            },
            dragover: (ev) => {
              if (dragFrom === null || dragFrom === i) return;
              ev.preventDefault();
              ev.dataTransfer.dropEffect = "move";
              if (dropAt !== i) { dropAt = i; paintDrop(); }
            },
            dragleave: () => { if (dropAt === i) { dropAt = null; paintDrop(); } },
            drop: (ev) => { ev.preventDefault(); moveTo(dragFrom, i); },
            dragend: () => { dragFrom = null; dropAt = null; paintDrop(); },
            } : {}),
          },
        },
          // the handle is a button so the list can be reordered from the
          // keyboard as well: the arrow keys move the row it belongs to
          editable ? h("button.grip", {
            type: "button", id: `grip-${idPrefix}-${i}`,
            "aria-label": `${stopLabel(row)}, stop ${i + 1} of ${rows.length}. Drag it, or move it with the arrow keys.`,
            on: { keydown: (ev) => {
              const by = ev.key === "ArrowUp" ? -1 : ev.key === "ArrowDown" ? 1 : 0;
              if (!by) return;
              ev.preventDefault();
              moveTo(i, i + by);
            } },
          }, h("span", { "aria-hidden": "true" }, "⋮⋮")) : null,
          h("span.stop-row-name", { title: stopLabel(row) }, stopLabel(row)),
          row.stop_id ? h("span.ids", row.stop_id) : null,
          row.stop_type === "ROUTE CORRECTION"
            ? h("span.chip", { title: "A point the line is drawn through so it follows the road. No passenger boards here, and it is in no GTFS file." }, "map point, not a stop")
            : null,
          row.removed ? h("span.chip.warn", "removed when you save") : null,
          !row.removed && kept()[0] === row ? h("span.chip", "first stop") : null,
          row.unserviceable ? h("span.chip.warn", "out of use") : null,
          editable && row.removed ? h("span.btn-row",
            h("button.route-chip", {
              type: "button", title: "Put this stop back",
              "aria-label": `Put ${stopLabel(row)} back`,
              on: { click: () => { delete row.removed; retype(); closePicker(); draw(); } },
            }, "Undo")) : null,
          editable && !row.removed ? h("span.btn-row",
            h("button.route-chip", {
              type: "button", title: "Change this stop for another",
              "aria-label": `Change ${stopLabel(row)} for another stop`,
              disabled: row.stop_type === "ROUTE CORRECTION",
              on: { click: () => changeStop(i) },
            }, "✎"),
            h("button.route-chip", {
              type: "button", title: "Remove this stop",
              "aria-label": `Remove ${stopLabel(row)}`,
              // struck through rather than taken out: nothing is lost until
              // the save, and Undo puts it back where it was
              on: { click: () => { row.removed = true; retype(); closePicker(); draw(); } },
            }, "✕")) : null))
      : h("li", h("span.hint", empty)));
    if (focusGrip !== null) {
      document.getElementById(`grip-${idPrefix}-${focusGrip}`)?.focus();
      focusGrip = null;
    }
    if (onChange) onChange();
  };

  const api = {
    listEl, pickerSlot, addBtn, kept, draw, closePicker,
    setRows(next) {
      rows = next.map((x) => ({ ...x }));
      closePicker();
      draw();
    },
    destroy() {
      closePicker();
      map.clearRoute(layer);
    },
  };
  return api;
}

// What the open draft already does to stage `key`: edits it, takes routes off
// it onto a new stage, merges other stages into it, or replaces it with
// another stage altogether.
function draftFor(key) {
  const none = { edits: [], splits: [], merges: [], replacedBy: null, any: false };
  if (!state.draft || !key) return none;
  const all = state.draft.changes.filter((c) => c.entity === "stage" && c.after);
  const { into, replacedBy } = draftMerges(key);
  const out = {
    edits: stageChanges(key).filter((c) => c.op === "update"),
    splits: all.filter((c) => c.op === "split" && sameStageKey(c.after.from_stage_id || "", key)),
    merges: into,
    replacedBy,
  };
  out.any = out.edits.length + out.splits.length + out.merges.length > 0 || !!replacedBy;
  return out;
}

// The key a create or split names the stage it makes by.
const madeKey = (c) => `${c.entity_key}|${(c.after && c.after.direction) || ""}`;

// The changes of the draft that build on stage `key` being there: edits of
// it, merges into or out of it, and route stage lists naming it. Taking out
// the change that makes it takes these with it, or they fail.
function buildsOn(key) {
  const names = (c) => ((c.after && c.after.stages) || []).some((l) => sameStageKey(String(l.stage_id || ""), key));
  return state.draft.changes.filter((c) => (c.entity === "stage" && c.op !== "create" && c.op !== "split"
      && (sameStageKey(c.entity_key, key) || sameStageKey((c.after && c.after.into_stage_id) || "", key)))
    || ((c.entity === "route_stages" || c.entity === "route_variant") && names(c)));
}

// Take a split back out of the draft, and everything that builds on the stage
// it made: the routes go back onto this stage.
async function undoSplit(c) {
  const after = buildsOn(madeKey(c));
  const name = c.after.name || c.entity_key;
  if (!(await confirmDialog(`Take ${name} out of the draft?`,
    `${name} is not made, and ${plural((c.after.routes || []).length, "route")} stay on this stage.`
      + (after.length ? ` ${plural(after.length, "other change")} of the draft that ${after.length === 1 ? "uses" : "use"} ${name} come${after.length === 1 ? "s" : ""} out with it.` : ""),
    { confirm: "Take it out" }))) return false;
  try {
    for (const d of after.reverse()) await removeChange(d.change_id);
    await removeChange(c.change_id);
    toast(`${name} is out of the draft.`, "ok");
    return true;
  } catch (e) {
    toast(e.message, "error");
    return false;
  }
}

// `keep`: the stops and name being edited, carried over when the page is drawn
// again after a split or a merge went into the draft, so they are not lost.
export async function showStageReview(id, { keep = null } = {}) {
  resetForFeed();
  setLeaveGuard(null);
  map.endModes();
  map.clearRoute();
  leaveStageReviews();

  let r;
  try {
    r = await get(`stage-reviews/${enc(id)}`);
  } catch (e) {
    return clear(panel(), h("section.section",
      h("a.crumb", { href: "#/stage-reviews" }, "Stages to review"),
      h("p.notice.error", e.message)));
  }
  nameHere(`Review: ${r.name}`);
  let stage = (r.stages || [])[0];
  // A draft that already changes this stage is what the reviewer is working
  // in: show the stage with it applied, or Save and the split and merge
  // buttons offer again what the draft already does, and a second Save starts
  // from the live stops and drops the first one's edits.
  const key = stage && stageKey(stage);
  const drafted = draftFor(key);
  let previewFailed = null;
  // A stage the draft replaces is shown as it is live, and not edited: the
  // draft deletes it.
  // Replaced by B, and B in turn by C, ends on C.
  const replacement = drafted.replacedBy ? await stageBrief(finalStage(key)) : null;
  const via = drafted.replacedBy && !sameStageKey(finalStage(key), drafted.replacedBy.after.into_stage_id)
    ? drafted.replacedBy.after.into_stage_id.split("|")[0] : null;
  if (drafted.any && !replacement) {
    try {
      const mine = await get(`change-sets/${enc(state.draft.change_set_id)}/preview/stages/${enc(key)}`);
      stage = { ...stage, name: mine.name, description: mine.description, rows: mine.rows, routes: mine.routes, route_count: mine.route_count };
    } catch (e) {
      previewFailed = e.message;
    }
  }
  // routes the draft has already taken off this stage, and the stage each went to
  const movedTo = new Map();
  for (const c of drafted.splits) for (const rt of c.after.routes || []) movedTo.set(rt, c.after.name || c.entity_key);
  const ev = r.evidence || {};
  const editable = can("editor") && r.status === "pending" && !!stage && !replacement;
  // a route by its number as well as its id: the evidence holds only ids
  const routeNames = r.route_names || {};
  const label = (rt) => (routeNames[rt] && routeNames[rt] !== rt ? `${routeNames[rt]} (${rt})` : rt);
  const labels = (ids) => `${ids.length === 1 ? "route" : "routes"} ${ids.map(label).join(", ")}`;
  // a review whose stages have all gone (merged or deleted) has nothing to
  // edit, and is still closed: it said something about stages no longer there
  const canClose = can("editor") && r.status === "pending";

  // ---- moving between the stages of this name without going back to the list.
  // MOOLAKADAI is up and down, and several of MTC's stops besides; a person
  // settling it wants to step through them and see which are already done.
  const family = r.family || [];
  const familyEl = (() => {
    if (family.length < 2) return null;
    const label = (f) => [
      f.direction || "either way",
      f.reason === AGREED ? "mapped cleanly" : REASON_TEXT[f.reason] || f.reason,
      f.impact ? `${plural(f.impact, "stop call")} wrong` : null,
    ].filter(Boolean).join(" \u00b7 ");
    const done = (f) => ({ fixed: "fixed", confirmed: "left alone" })[f.status];
    const select = h("select", { id: "stage-review-switch" },
      family.map((f) => h("option", {
        value: String(f.review_id),
        selected: String(f.review_id) === String(id) ? "selected" : null,
      }, `${label(f)}${done(f) ? ` \u2014 ${done(f)}` : ""}`)));
    select.addEventListener("change", () => {
      if (select.value && select.value !== String(id)) {
        location.hash = `#/stage-reviews/${enc(select.value)}`;
      }
    });
    const left = family.filter((f) => f.status === "pending").length;
    return h("div.notice",
      h("p", h("strong", `${r.name} (stage ${ev.stage_id || (stage && stage.stage_id) || ""}) runs ${plural(family.length, "way")}.`), " ",
        left ? `${left} still to look at.` : "Both have been looked at."),
      h("label.field", { for: "stage-review-switch" },
        h("span", "Which one you are on"), select),
      h("p.hint", family.map((f, i) => [
        i ? " \u00b7 " : "",
        h(String(f.review_id) === String(id) ? "strong" : "span",
          `${f.direction || "either way"}${f.stage_id ? ` (${f.stage_id})` : ""}`),
        done(f) ? h("span.muted", ` ${done(f)}`) : null,
      ])));
  })();

  // What is unsaved: the stops AND the name. Changing only the name is a real
  // edit - two stages of a corridor are told apart by being renamed - and it
  // used to leave Save disabled with nothing saying why.
  const startedName = stage ? stage.name : r.name;
  const candidateEl = h("ul.stop-choices");
  const unsaved = h("span.chip.warn", { hidden: true }, "not saved yet");

  // Two editors can be open at once - this stage's, and a new stage's being
  // made from one of the lists below - with one stop search between them:
  // opening it in one closes it in the other.
  let newEditor = null;
  const editor = stopListEditor({
    rows: stage ? stage.rows || [] : [],
    editable, idPrefix: "stage", layer: "stage-review-0", color: COLOURS[0],
    empty: "No stops yet. Use one of the lists below, or add a stop.",
    what: "this stage",
    beforePick: () => { if (newEditor) newEditor.closePicker(); },
    onChange: () => drawHost(),
  });
  const kept = () => editor.kept();
  const shape = () => JSON.stringify([
    nameInput.value.trim(),
    kept().map((x) => x.stop_id || x.marker_id),
  ]);
  let started = null;
  const dirty = () => started !== null && shape() !== started;

  // what changes around the list when it does: Save, and which stops of each
  // route's list are not in it
  const drawHost = () => {
    save.disabled = !dirty();
    unsaved.hidden = !dirty();
    const inList = kept().map((x) => x.stop_id).filter(Boolean);
    clear(candidateEl, candidates.map((c) => {
      const gone = (c.routes || []).filter((rt) => movedTo.has(rt));
      const moved = gone.length ? { to: movedTo.get(gone[0]), count: gone.length } : null;
      return candidateCard(c, editable ? useList : null, inList, editable ? {
        open: splitting === c,
        start: startSplit,
        form: splitForm,
      } : null, moved, label);
    }));
  };

  // A route's list as stage rows. A stop is its id; a map point (a ROUTE
  // CORRECTION, no stop id) is only named in the review, so it is taken from
  // this stage's own map point of that name, which has its position. One the
  // stage does not have cannot be placed and is left out: written as a stop
  // with no id, it made a stage the server refused.
  const listRows = (c) => {
    const known = new Map();
    const markers = new Map();
    for (const st of r.stages || []) {
      for (const x of st.rows || []) {
        if (x.stop_type === "ROUTE CORRECTION") markers.set(stopLabel(x), x);
        else if (x.stop_id) known.set(x.stop_id, x);
      }
    }
    const out = [];
    let dropped = 0;
    c.stops.forEach((sid, i) => {
      const name = (c.stop_names || [])[i];
      if (sid) {
        const x = known.get(sid);
        out.push(x ? { ...x } : { stop_id: sid, stop_type: "INTERMEDIATE STOP", stop_name: name || sid, lat: null, lon: null });
      } else if (markers.has(name)) {
        out.push({ ...markers.get(name) });
      } else {
        dropped += 1;
      }
    });
    // the first stop is where the fare stage begins
    let first = true;
    for (const x of out) {
      if (x.stop_type === "ROUTE CORRECTION") continue;
      x.stop_type = first ? "NEW STOP" : (x.stop_type === "NEW STOP" ? "INTERMEDIATE STOP" : x.stop_type);
      first = false;
    }
    return { rows: out, dropped };
  };
  const tellDropped = (n) => {
    if (n) toast(`${plural(n, "map point")} of that list could not be placed, so ${n === 1 ? "it is" : "they are"} left out. Map points only shape the line; add them on the stage's page if you need them.`);
  };

  // ---- a new stage for one list's routes. The reviewer has decided these
  // routes are not this stage at all: they get a stage of their own, and come
  // off this one. It starts with the stops that list gives and is edited like
  // the stage above - reordered, stops changed, removed or added - before it is
  // made. One change goes into the draft, however many routes move.
  let splitting = null;
  let splitName = "";
  let splitDir = "";
  let splitBusy = false;
  let splitGo = null;

  const suggestSplitName = (c) => {
    const base = (stage ? stage.name : r.name).trim();
    const to = (c.goes_to && c.goes_to[0] && c.goes_to[0].name) || "";
    const first = (c.stop_names || [])[0] || "";
    if (to) return `${base} (to ${to})`;
    if (first && first.toUpperCase() !== base.toUpperCase()) return `${base} (from ${first})`;
    return `${base} (${plural(c.stops.length, "stop")})`;
  };
  const canSplit = (allKnown) => allKnown && !splitBusy && !!splitName.trim()
    && !!newEditor && newEditor.kept().some((x) => x.stop_type !== "ROUTE CORRECTION");

  const startSplit = (c) => {
    if (newEditor) newEditor.destroy();
    splitting = c;
    splitName = suggestSplitName(c);
    splitDir = stage ? (stage.direction || "") : (r.direction || "");
    const made = listRows(c);
    const allKnown = (c.routes || []).length > 0 && (c.routes || []).length >= (c.route_count || 0);
    newEditor = stopListEditor({
      rows: made.rows, editable: true, idPrefix: "new", layer: "stage-review-new", color: COLOURS[1],
      empty: "No stops yet. Add a stop.",
      what: "the new stage",
      beforePick: () => editor.closePicker(),
      onChange: () => { if (splitGo) splitGo.disabled = !canSplit(allKnown); },
    });
    tellDropped(made.dropped);
    drawHost();
    newEditor.draw();
    document.getElementById("split-name")?.focus();
  };
  const endSplit = () => {
    if (newEditor) newEditor.destroy();
    newEditor = null;
    splitting = null;
    splitGo = null;
    drawHost();
  };

  const splitForm = (c, allKnown) => {
    const name = h("input", {
      type: "text", id: "split-name", value: splitName, autocomplete: "off", spellcheck: "false",
      on: { input: (ev) => { splitName = ev.target.value; splitGo.disabled = !canSplit(allKnown); } },
    });
    const dir = h("select", { id: "split-direction", on: { change: (ev) => { splitDir = ev.target.value; } } },
      h("option", { value: "", selected: splitDir === "" }, "Either way"),
      h("option", { value: "up", selected: splitDir === "up" }, "Up"),
      h("option", { value: "down", selected: splitDir === "down" }, "Down"));
    splitGo = h("button.btn.small", {
      type: "button", disabled: !canSplit(allKnown),
      on: { click: () => splitOff(c, name.value, dir.value) },
    }, splitBusy ? "Creating…" : "Create the stage");
    return h("div.split-form",
      h("p.hint", `A new stage. ${(c.routes || []).length === 1 ? `Route ${label(c.routes[0])} runs` : `Routes ${(c.routes || []).map(label).join(", ")} run`} `
        + `it instead of ${stage ? stage.name : r.name}; every other route stays where it is.`),
      h("label.field", { for: "split-name" }, h("span", "Name"), name),
      h("label.field", { for: "split-direction" }, h("span", "Direction"), dir),
      h("p.hint", "Its stops, from this list. Reorder, change, remove or add them before you create it."),
      newEditor ? newEditor.listEl : null,
      newEditor ? h("div.btn-row", newEditor.addBtn) : null,
      newEditor ? newEditor.pickerSlot : null,
      h("div.btn-row", splitGo,
        h("button.btn.quiet.small", {
          type: "button", disabled: splitBusy,
          on: { click: endSplit },
        }, "Cancel")));
  };

  const splitOff = async (c, name, direction) => {
    name = (name || "").trim();
    const routeIds = c.routes || [];
    const key = stage && (stage.stage_key || stage.stage_id);
    const stops = newEditor ? newEditor.kept() : [];
    if (!name || !routeIds.length || !key || !stops.length) return;
    if (!(await requireDraft("A new stage goes into a draft. Nothing changes for passengers until someone else approves it and it is committed."))) return;
    if (!(await confirmDialog(
      `Move ${plural(routeIds.length, "route")} to a new stage?`,
      `${name} becomes a new stage with ${plural(stops.length, "stop")}, and `
        + `${labels(routeIds)} `
        + `will run it in place of ${stage.name} (stage ${stage.stage_id}). It goes into your draft; `
        + "nothing changes for passengers until the draft is approved and committed.",
      { confirm: "Create the stage" },
    ))) return;
    splitBusy = true;
    if (splitGo) { splitGo.disabled = true; splitGo.textContent = "Creating…"; }
    try {
      // One change, however many routes: the server makes the stage and moves
      // them in the same transaction. This used to be a create plus one call
      // per route, which for a stage 47 routes run was 95 requests and could
      // stop half way.
      const res = await addChange({
        entity: "stage", op: "split", entity_key: "",
        after: {
          name,
          direction: direction || null,
          description: null,
          from_stage_id: key,
          routes: routeIds,
          rows: stops.map(asStageRow),
        },
      }, { merge: false });
      if (!res) return;
      const bad = (res.problems || []).find((pb) => pb.level === "error");
      if (bad) return toast(bad.message, "error");
      toast(`${name} is in your draft, and ${plural(routeIds.length, "route")} now run it.`, "ok");
      if (newEditor) newEditor.destroy();
      newEditor = null;
      showStageReview(id, { keep: editing() });
      return;
    } catch (e) {
      toast(e.message, "error");
    } finally {
      splitBusy = false;
      if (splitGo) { splitGo.textContent = "Create the stage"; splitGo.disabled = false; }
    }
  };

  const useList = (c) => {
    const made = listRows(c);
    editor.setRows(made.rows);
    tellDropped(made.dropped);
  };

  // a row as a stage change stores it
  const asStageRow = (row) => {
    const marker = row.stop_type === "ROUTE CORRECTION";
    return {
      stop_id: marker ? null : row.stop_id, stop_type: row.stop_type,
      marker_id: marker ? row.marker_id || null : null,
      marker_name: marker ? row.marker_name || null : null,
      marker_lat: marker ? row.marker_lat ?? null : null,
      marker_lon: marker ? row.marker_lon ?? null : null,
      stop_name_override: marker ? null : row.stop_name_override ?? null,
    };
  };

  const save = h("button.btn", { type: "button", hidden: !editable, disabled: true }, "Save these stops to a draft");
  save.addEventListener("click", async () => {
    if (!kept().length) return toast("A stage needs at least one stop.", "error");
    save.disabled = true;
    try {
      const res = await addChange({
        entity: "stage", op: "update", entity_key: stage.stage_key || stage.stage_id,
        after: {
          name: nameInput.value.trim() || stage.name,
          direction: stage.direction || null,
          description: stage.description || null,
          rows: kept().map(asStageRow),
        },
      });
      if (!res) return void (save.disabled = false);
      const bad = (res.problems || []).find((pb) => pb.level === "error");
      if (bad) {
        toast(bad.message, "error");
        return void (save.disabled = false);
      }
      toast(`Saved to draft “${(state.draft && state.draft.title) || "untitled"}”. Mark the review fixed when you are happy.`, "ok");
      showStageReview(id);
    } catch (e) {
      toast(e.message, "error");
      save.disabled = false;
    }
  });

  const nameInput = h("input", {
    type: "text", id: "stage-review-name", value: startedName,
    autocomplete: "off", spellcheck: "false",
    // typing a new name is an edit like any other: Save follows it
    on: { input: () => { save.disabled = !dirty(); unsaved.hidden = !dirty(); } },
  });
  started = shape();
  const editing = () => (dirty() ? { name: nameInput.value, rows: kept() } : null);
  const note = h("textarea", { id: "stage-review-note", rows: "2", placeholder: "What you found, and what you did about it" });
  // the draft that holds the fix is the one to name: offered from the feed's
  // drafts, the open one chosen when it changes this stage
  const draftChoice = draftPicker({ id: "stage-review-draft", preselect: drafted.any ? state.draft.change_set_id : null });
  // a stage replaced altogether says so, and by what, unless the note says otherwise
  if (replacement) note.value = `The stage was wrong: replaced by ${replacement.name} (stage ${replacement.stage_id}).`;

  // The other open reviews the draft's merges settle: a stage merged away has
  // nothing left to review, and closes with this one.
  // The other way of this stage, still open, is offered unticked: a draft
  // often fixes up and down together, but only the person who made it knows.
  const otherWays = family
    .filter((f) => f.status === "pending" && String(f.review_id) !== String(id))
    .map((f) => ({ review_id: f.review_id, name: r.name, stage_id: f.stage_id, direction: f.direction }));
  const merging = !!stage && (drafted.merges.length > 0 || !!replacement);
  const also = canClose && (merging || otherWays.length)
    ? alsoClose({
        gone: merging ? [...drafted.merges.map((c) => c.entity_key), ...(replacement ? [key] : [])] : [],
        kept: merging ? [replacement ? finalStage(key) : key] : [],
        ways: otherWays,
        except: { review_id: r.review_id },
      })
    : null;

  // `set` names the draft without the picker: the live draft that changed it
  const close = async (decision, { set: named = null } = {}) => {
    const body = { decision };
    const text = note.value.trim();
    if (text) body.note = text;
    if (decision === "confirmed" && !text) {
      toast("Say why the routes really do differ before leaving it alone.", "error");
      return note.focus();
    }
    const set = named || draftChoice.value();
    if (decision === "fixed" && set) body.change_set = set;
    if (decision === "fixed" && !set && stage && !(await confirmDialog("Mark it fixed without naming a draft?",
      "Nothing records what fixed it. If the fix is in a draft, choose that draft under Draft first, so whoever approves it can see why.",
      { confirm: "Mark fixed" }))) return;
    try {
      const after = await post(`stage-reviews/${enc(id)}/close`, body);
      const more = decision === "fixed" && also
        ? await also.closeAll({ decision: "fixed", note: body.note || null, change_set: body.change_set || null })
        : { closed: 0, failed: [] };
      flash = {
        title: decision === "fixed" ? `“${r.name}” marked fixed.` : `“${r.name}” left alone.`,
        text: ["The flag is off its stage.",
          more.closed ? `${plural(more.closed, "other review")} marked fixed with it.` : null,
          more.failed.length ? `${plural(more.failed.length, "other review")} could not be closed: ${more.failed.join(" ")}` : null,
        ].filter(Boolean).join(" "),
        draftId: after.change_set_id, draftTitle: after.change_set_title,
      };
      list.loaded = false;
      refreshStageReviewCount();
      location.hash = "#/stage-reviews";
    } catch (e) {
      toast(e.message, "error");
    }
  };
  const fixed = h("button.btn", { type: "button", hidden: !canClose, on: { click: () => close("fixed") } }, "Mark fixed");
  const left = h("button.btn.secondary", { type: "button", hidden: !canClose, on: { click: () => close("confirmed") } }, "Leave it alone");
  const doReopen = async () => {
    try {
      await post(`stage-reviews/${enc(id)}/reopen`, {});
      list.loaded = false;
      refreshStageReviewCount();
      showStageReview(id);
    } catch (e) { toast(e.message, "error"); }
  };
  const reopen = h("button.btn.secondary", {
    type: "button", hidden: !(can("editor") && ["fixed", "confirmed"].includes(r.status)),
    on: { click: doReopen },
  }, "Reopen");

  // A draft changed this stage after the review was raised, and is live:
  // perhaps the fix, made without anybody marking it here.
  const changed = r.status === "pending" ? r.changed : null;
  const changedEl = changed ? h("div.notice",
    h("p", h("strong", "Draft "),
      h("a", { href: `#/drafts/${enc(changed.change_set_id)}` }, `\u201c${changed.title || "untitled"}\u201d`),
      h("strong", ` changed this stage after it was raised, and went live on ${fmtDate(changed.committed_at)}.`),
      " If that fixed it, close it naming that draft; if not, carry on below."),
    canClose ? h("div.btn-row", h("button.btn.small", {
      type: "button", on: { click: () => close("fixed", { set: changed.change_set_id }) },
    }, `Mark fixed with \u201c${changed.title || "untitled"}\u201d`)) : null) : null;

  const candidates = (ev.candidates || []).filter((c) => (c.stops || []).length);

  // ---- stages that may be the same place: the others of this name, names
  // written another way (MTC's B.T, a case change), stages starting at the next
  // kerb, and the stages the draft makes. Each can be merged into this one, or
  // put in its place when this stage is wrong altogether. Only the same way:
  // the server refuses a merge across directions.
  const like = stage ? stagesLike(stage, {
    editable,
    onChange: () => showStageReview(id, { keep: editing() }),
  }) : null;

  clear(panel(),
    h("section.section",
      h("a.crumb", { href: "#/stage-reviews" }, "Stages to review"),
      h("h1", r.name, statusChip(r)),
      h("p.hint", r.reason === AGREED
        ? [h("strong", "Mapped cleanly"), " \u2014 every route gave this stage the same stops. Look it over and say it is right."]
        : [h("strong", REASON_TEXT[r.reason] || r.reason), " \u2014 ", REASON_WHY[r.reason] || ""]),
      familyEl,
      h("dl.facts",
        h("dt", "Stage id"),
        h("dd", stage ? stage.stage_id : (ev.stage_id || ""),
          // nothing invented is left to look like the real thing
          idKind(stage ? stage.stage_id : (ev.stage_id || ""))),
        // under a "Direction" label, "direction: down" says it twice
        h("dt", "Direction"), h("dd", r.direction || "either way"),
        h("dt", "Routes using it"), h("dd", String(ev.route_count || (stage && stage.route_count) || 0)),
        r.impact ? [h("dt", "Stop calls the guess gets wrong"), h("dd", String(r.impact))] : null,
        h("dt", "Lists the routes give"), h("dd", String(ev.lists || candidates.length)),
        ev.head_names && ev.head_names.length ? [h("dt", "Begins at"), h("dd", ev.head_names.join(", "))] : null,
        ev.spellings && ev.spellings.length > 1 ? [h("dt", "Spelled"), h("dd", ev.spellings.join(" / "))] : null,
        h("dt", "Raised by"), h("dd", r.batch),
        r.reviewed_by_email ? [h("dt", "Closed by"), h("dd", `${r.reviewed_by_email}${r.reviewed_at ? ` on ${fmtDate(r.reviewed_at)}` : ""}`)] : null,
        r.review_note ? [h("dt", "Note"), h("dd", r.review_note)] : null,
        r.change_set_id ? [h("dt", "Draft"), h("dd", h("a", { href: `#/drafts/${enc(r.change_set_id)}` }, r.change_set_title || r.change_set_id))] : null),
      fixedByNotice(r, { onReopen: can("editor") ? doReopen : null }),
      changedEl),
    !stage ? h("section.section", h("p.notice.error", "No stage of this name is live any more.")) : null,
    stage && replacement ? h("section.section", h("div.notice.draft.pending",
      h("p", h("strong", `Your draft “${state.draft.title}” replaces this stage with ${replacement.name} (stage ${replacement.stage_id})`
        + `${via ? `, by way of stage ${via}` : ""}.`),
        ` Every route using ${stage.name} runs ${replacement.name} instead, and ${stage.name} is deleted when the draft is committed. `
        + "This is the stage as it is live; there is nothing left to edit on it."),
      h("div.btn-row",
        h("a.btn.secondary.small", { href: `#/stage/${enc(replacement.stage_key || finalStage(key))}` }, `Open ${replacement.name}`),
        canClose ? h("button.btn.quiet.small", {
          type: "button",
          on: { click: async () => {
            if (await undoChange(drafted.replacedBy, `Replacing ${stage.name} with ${replacement.name}`)) showStageReview(id);
          } },
        }, "Undo the replacement") : null,
        h("a.btn.quiet.small", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft")))) : null,
    stage && drafted.any && !replacement ? h("section.section", h("div.notice.draft.pending",
      h("p", h("strong", `Your draft “${state.draft.title}” already changes this stage: `),
        [
          drafted.edits.length ? "its stops or name" : null,
          drafted.splits.length ? `${plural(movedTo.size, "route")} moved to ${drafted.splits.map((c) => c.after.name || c.entity_key).join(", ")}` : null,
          drafted.merges.length ? `${plural(drafted.merges.length, "stage")} merged into it` : null,
        ].filter(Boolean).join("; "), "."),
      previewFailed
        ? h("p", `The draft could not be applied to show it (${previewFailed}), so this is the live stage.`)
        : h("p", "What you see here has the draft applied. Saving changes the same draft."),
      // a change made here by mistake comes out again here: a new stage that
      // is wrong is taken out (or opened and edited), not left in the draft
      canClose ? h("ul.compact-list",
        drafted.edits.map((c) => h("li", "Your edit of its stops or name ",
          h("button.btn.quiet.small", {
            type: "button",
            on: { click: async () => { if (await undoChange(c, `Your edit of ${stage.name}`)) showStageReview(id); } },
          }, "Take it out"))),
        drafted.splits.map((c) => h("li", `New stage ${c.after.name || c.entity_key}, for ${labels(c.after.routes || [])} `,
          h("a.btn.quiet.small", { href: `#/stage/${enc(madeKey(c))}` }, "Open or edit it"),
          h("button.btn.quiet.small", {
            type: "button",
            on: { click: async () => { if (await undoSplit(c)) showStageReview(id, { keep: editing() }); } },
          }, "Take it out")))) : null,
      h("p", h("a", { href: `#/drafts/${enc(state.draft.change_set_id)}` }, "Open the draft")))) : null,
    stage ? h("section.section",
      h("h2", "The stage's stops", unsaved),
      h("p.hint", editable
        ? "This is what every route using the stage will call at. Reorder or remove them, take one of the lists below, or add any stop. Saving puts it in your draft; it reaches passengers when the draft is approved and committed."
        : "What every route using this stage calls at."),
      h("p.ids", h("label.field", { for: "stage-review-name" }, h("span", "Stage name"), nameInput)),
      editor.listEl,
      editable ? h("div.btn-row", editor.addBtn, save) : null,
      editable ? editor.pickerSlot : null,
      // a route using the stage twice is listed once
      (() => {
        const seen = new Map();
        for (const rt of stage.routes || []) if (!seen.has(rt.route_id)) seen.set(rt.route_id, rt);
        const runBy = [...seen.values()];
        // several routes share a short name here, so one that is not unique
        // carries its route id or the links all read the same
        const byName = new Map();
        for (const rt of runBy) byName.set(rt.short_name, (byName.get(rt.short_name) || 0) + 1);
        const label = (rt) => (rt.short_name && byName.get(rt.short_name) === 1
          ? rt.short_name
          : `${rt.short_name ? `${rt.short_name} ` : ""}(${rt.route_id})`);
        return h("p.hint", runBy.length
          ? ["Run by ", ...runBy.slice(0, 10).map((rt, i) => [
              i ? ", " : "", h("a", { href: `#/route/${enc(rt.route_id)}` }, label(rt))]),
            runBy.length > 10 ? ` and ${runBy.length - 10} more` : ""]
          : "No live route uses this stage.");
      })()) : null,
    candidates.length > 1 ? h("section.section",
      h("h2", "What each route says this stage is"),
      h("p.hint", "Every list the routes give it, commonest first. Take one whole, or "
        + "build your own above. A stop not in the list above is marked. A \u201cmap point\u201d "
        + "is not a stop: it is a point the route is drawn through so the line follows the road, "
        + "and no passenger boards there."),
      candidateEl) : null,
    like ? h("section.section",
      h("h2", "Stages like this one"),
      h("p.hint", `Stages ${r.direction ? `running ${r.direction}` : "running either way"} that may be the same place: `
        + "the same name, a name written another way, one starting close by, or one made in your draft. "
        + "If one is the same place, merge it into this one, and its routes call at this stage's stops. "
        + "If this stage is wrong altogether, use the right one instead: every route using this stage runs "
        + "that one, and this one goes away. Wrong on only some routes? Use \u201cOnly on some routes\u2026\u201d. "
        + "To make the right one first, use \u201cCreate a new stage\u201d on a list above, or New stage. "
        + "Each one says first which stops the routes gain and lose, and how far apart the two start. "
        + "Stages going the other way are never offered \u2014 they hold different stops."),
      like.el) : null,
    r.reason === "head_duplicate_stops" && (ev.heads || []).length > 1 ? h("section.section",
      h("p.hint", "These begin at stops that all carry one name, so the stops are the thing to fix: ",
        h("a", { href: `#/merge/${enc(ev.heads[0])}?with=${enc(ev.heads.slice(1).join(","))}` },
          `merge ${plural(ev.heads.length, "stop")} into one`),
        ". The stage comes right on its own once the stops are one.")) : null,
    canClose || !reopen.hidden ? h("section.section",
      h("h2", "Close it"),
      canClose ? h("p.hint", stage
        ? "Mark it fixed once the stops are right (name the draft if you like), or leave it alone if the routes really do differ and nothing should change."
        : "Its stages are gone, so there is nothing left to fix here: mark it fixed, naming the draft that merged or deleted them if you have it.") : null,
      canClose ? h("label.field", { for: "stage-review-note" }, h("span", "Note"), note) : null,
      canClose ? h("div.btn-row", CONFIRM_NOTES.map((t) => h("button.route-chip", {
        type: "button", title: t, on: { click: () => { note.value = t; note.focus(); } },
      }, `${t.slice(0, 40)}\u2026`))) : null,
      canClose ? h("label.field", { for: "stage-review-draft" }, h("span", "Draft"), draftChoice.el) : null,
      also ? also.el : null,
      h("div.btn-row", fixed, left, reopen)) : null);
  if (keep) {
    nameInput.value = keep.name;
    editor.setRows(keep.rows);
  } else {
    editor.draw();
  }
}
