// Stations to review. The nightly build suggests stations: same-named stops
// close together, each stop labelled "Towards <next stop>". A person checks each
// suggestion on the map, fixes the name, the station point or a platform label,
// or drops a stop, then approves it into a draft or rejects it. An approved
// station goes live only when its draft is approved by someone else and committed.
import { get, post, enc, ApiError } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, modal, fmtCoord, fmtDate, fmtMetres, fmtCount, haversine, plural } from "./util.js";
import * as map from "./map.js";
import { requireDraft, refreshDraft } from "./drafts.js";
import { undoScope } from "./undo.js";
import { nameHere } from "./trail.js";

const panel = () => document.getElementById("panel");
const STATUSES = [["pending", "To review"], ["approved", "In a draft"], ["rejected", "Rejected"], ["committed", "Live"]];
const PAGE = 50;
const AREA_MAX = 150;          // the most approved together, so an area is still looked at
const AREA_MIN_ZOOM = 15;
const PLATFORM_MAX = 120;
const REJECT_REASONS = ["These stops are different places.", "Not the same stop name on the ground.", "The stops are too far apart to be one station."];

// The list survives opening a suggestion and coming back, and gives "next".
const list = { feedId: null, status: "pending", q: "", area: false, items: [], cursor: null, counts: null, loaded: false };
let stopWatching = null;

const round7 = (x) => Math.round(x * 1e7) / 1e7;
const spread = (p) => (p.members.length === 2 ? `${plural(2, "stop")}, ${fmtMetres(p.spread_m)} apart` : `${plural(p.members.length, "stop")} within ${fmtMetres(p.spread_m)}`);

function resetForFeed() {
  if (list.feedId === state.feedId) return;
  Object.assign(list, { feedId: state.feedId, status: "pending", q: "", area: false, items: [], cursor: null, counts: null, loaded: false });
}

function watchMap(fn) {
  if (stopWatching) stopWatching();
  stopWatching = fn ? map.onMoveEnd(fn) : null;
}

// The router calls this before any screen, so the list stops following the map.
export function leaveStations() {
  watchMap(null);
}

// The pending count in the top bar. The link itself shows only while there are
// suggestions to review or in a draft; #/stations works either way.
export async function refreshStationCount() {
  const badge = document.getElementById("stations-count");
  if (!badge || !state.feedId) return;
  const link = badge.closest("a");
  try {
    list.counts = await get(`feeds/${enc(state.feedId)}/station-proposals/summary`);
    badge.textContent = list.counts.pending ? fmtCount(list.counts.pending) : "";
    badge.hidden = !list.counts.pending;
    if (link) link.hidden = !((list.counts.pending || 0) + (list.counts.approved || 0));
  } catch {
    badge.hidden = true;
  }
}

async function fetchPage({ append = false } = {}) {
  const params = new URLSearchParams({ status: list.status, limit: String(PAGE) });
  if (list.q) params.set("q", list.q);
  if (list.area) params.set("bbox", map.bbox());
  if (append && list.cursor) params.set("cursor", list.cursor);
  const page = await get(`feeds/${enc(state.feedId)}/station-proposals?${params}`);
  list.items = append ? list.items.concat(page.items) : page.items;
  list.cursor = page.next_cursor;
  list.loaded = true;
}

function statusChip(p) {
  const label = Object.fromEntries(STATUSES)[p.status] || p.status;
  const cls = { pending: "submitted", approved: "approved", rejected: "rejected", committed: "committed" }[p.status] || "";
  return h("span", { class: `chip ${cls}` }, label);
}

// ------------------------------------------------------------------ the list
export async function showStationsList() {
  resetForFeed();
  setLeaveGuard(null);
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  const search = h("input", { type: "search", id: "proposal-search", value: list.q, autocomplete: "off", spellcheck: "false", placeholder: "Station name or stop id" });
  const area = h("input", { type: "checkbox", id: "proposal-area", checked: list.area });
  const tabs = h("div.tabs.count-tabs", { role: "group", "aria-label": "Show suggestions that are" });
  const bulk = h("div.bulk-box");
  const items = h("div", { "aria-live": "polite" }, h("p.empty", "Loading…"));
  const more = h("button.btn.secondary", { type: "button", hidden: true }, "Show more");

  const renderTabs = () => clear(tabs, STATUSES.map(([key, label]) => h("button", {
    type: "button", "aria-pressed": String(list.status === key),
    on: { click: () => { if (list.status === key) return; list.status = key; renderTabs(); load(); } },
  }, label, list.counts ? h("span.count", fmtCount(list.counts[key] || 0)) : null)));

  const renderBulk = () => {
    if (!can("editor") || list.status !== "pending") return clear(bulk);
    if (map.zoom() < AREA_MIN_ZOOM) {
      return clear(bulk, h("p.hint", "To approve every suggestion in one neighbourhood together, zoom the map in to that neighbourhood."));
    }
    clear(bulk, h("button.btn.secondary", { type: "button", on: { click: approveArea } }, "Approve all in the map area…"));
  };

  const renderList = () => {
    more.hidden = !list.cursor;
    renderBulk();
    map.showProposalPoints(list.items, { onOpen: (p) => { location.hash = `#/stations/${p.proposal_id}`; } });
    if (!list.items.length) {
      const why = list.q ? `No ${Object.fromEntries(STATUSES)[list.status].toLowerCase()} suggestion matches “${list.q}”.`
        : { pending: "Nothing is waiting for review.", approved: "No suggestion is waiting in a draft.", rejected: "No suggestion has been rejected.", committed: "No suggested station is live yet." }[list.status];
      return clear(items, h("p.empty", list.area ? `${why} Only the map area is searched; move the map or untick “Only in the map area”.` : why));
    }
    clear(items, h("ul.proposal-list", list.items.map((p) => h("li.proposal-item",
      h("a.proposal-link", { href: `#/stations/${p.proposal_id}` },
        h("span.proposal-name", p.name),
        h("span.proposal-meta", spread(p))),
      statusChip(p),
      p.status === "approved" && p.change_set_id
        ? h("a.proposal-draft", { href: `#/drafts/${enc(p.change_set_id)}` }, `In draft “${p.change_set_title || "untitled"}”`) : null,
      p.status === "rejected" && p.review_note ? h("span.proposal-note", `Rejected: ${p.review_note}`) : null))));
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
  area.addEventListener("change", () => { list.area = area.checked; load(); });
  more.addEventListener("click", () => load({ append: true }));
  watchMap(() => {
    renderBulk();
    if (list.area) load();
  });

  clear(panel(),
    h("section.section",
      h("h1", "Stations to review"),
      h("p.hint", "Each suggestion groups same-named stops close together into one station, with a platform label for each stop. Open one, check it on the map, fix what is wrong, then approve it into your draft or reject it. Nothing goes live until that draft is approved by someone else and committed."),
      tabs,
      h("label.field", { for: "proposal-search" }, h("span", "Search"), search),
      h("label.check", { for: "proposal-area" }, area, " Only in the map area"),
      bulk),
    h("section.section", items, more));
  renderTabs();
  if (list.loaded && list.feedId === state.feedId && !list.area) renderList();
  else load();
  refreshStationCount().then(renderTabs);

  async function approveArea() {
    let found = [], cursor = null;
    try {
      do {
        const page = await get(`feeds/${enc(state.feedId)}/station-proposals?status=pending&bbox=${map.bbox()}&limit=200${cursor ? `&cursor=${enc(cursor)}` : ""}`);
        found = found.concat(page.items);
        cursor = page.next_cursor;
      } while (cursor && found.length <= AREA_MAX);
    } catch (e) {
      toast(e.message, "error");
      return;
    }
    if (!found.length) { toast("No suggestion in the map area is waiting for review."); return; }
    if (found.length > AREA_MAX) {
      toast(`There are more than ${AREA_MAX} suggestions in the map area. Zoom in and approve a smaller area at a time.`, "error");
      return;
    }
    const draft = await requireDraft("Approved stations are added to a draft. They go live only when someone else approves the draft and it is committed.");
    if (!draft) return;
    const go = await modal(`Approve ${plural(found.length, "suggested station")}?`, (close) => h("div", { style: "display:grid;gap:12px" },
      h("p", `Every suggestion now on the map is added to draft “${draft.title}” exactly as suggested: names, station points and platform labels as they are.`),
      h("ul.compact-list", found.slice(0, 10).map((p) => h("li", `${p.name} (${spread(p)})`)),
        found.length > 10 ? h("li", `and ${found.length - 10} more`) : null),
      h("p.hint", "A suggestion that cannot be added as it is now (for example a stop already belongs to a station) is skipped and stays in the list to review."),
      h("div.btn-row",
        h("button.btn", { type: "button", on: { click: () => close(true) } }, `Approve ${fmtCount(found.length)} into the draft`),
        h("button.btn.secondary", { type: "button", on: { click: () => close(false) } }, "Cancel"))));
    if (!go) return;
    try {
      const res = await post(`feeds/${enc(state.feedId)}/station-proposals/approve`, { change_set_id: draft.change_set_id, proposal_ids: found.map((p) => p.proposal_id) });
      await refreshDraft();
      const results = res.results || [];
      const added = results.filter((r) => r.ok).length;
      const skipped = results.filter((r) => !r.ok);
      const byId = new Map(found.map((p) => [p.proposal_id, p]));
      await modal(added ? `Added ${plural(added, "station")} to the draft` : "No station was added", () => h("div", { style: "display:grid;gap:12px" },
        h("p", added ? `${plural(added, "station")} ${added === 1 ? "is" : "are"} now in draft “${draft.title}”.` : "Every suggestion in the area had a problem."),
        skipped.length ? [h("p", h("strong", `${plural(skipped.length, "suggestion")} skipped, still waiting for review:`)),
          h("ul.compact-list", skipped.slice(0, 20).map((r) => h("li", `${(byId.get(r.proposal_id) || {}).name || `#${r.proposal_id}`}: ${(r.problems || []).map((x) => x.message).join(" ") || "could not be added"}`)))] : null),
      { actions: [(close) => h("button.btn", { type: "button", on: { click: () => close() } }, "Done")] });
      refreshStationCount().then(renderTabs);
      load();
    } catch (e) {
      toast(e.message, "error");
    }
  }
}

// ------------------------------------------------------------------ one suggestion
export async function showProposal(id) {
  resetForFeed();
  setLeaveGuard(null);
  watchMap(null);
  map.endModes();
  map.clearRoute();
  map.clearFocus();
  clear(panel(), h("section.section", h("p.empty", "Loading the suggestion…")));
  let p;
  try {
    p = await get(`station-proposals/${enc(id)}`);
  } catch (e) {
    return clear(panel(), h("section.section", h("a.crumb", { href: "#/stations" }, "Back to stations to review"), h("p.notice.error", e.message)));
  }
  if (!list.loaded) fetchPage().catch(() => {});

  const editable = p.status === "pending" && can("editor");
  const edit = {
    name: p.name, lat: p.lat, lon: p.lon,
    members: p.members.map((m) => ({ ...m, label: m.platform_code || "", dropped: false, routes: null, showing: null })),
  };
  let dirty = false;
  setLeaveGuard(() => (dirty ? `Your changes to the suggested station ${p.name} are not saved.` : null));
  const touch = () => { dirty = true; };
  nameHere(p.name);
  // What the reviewer changed before approving (the name, the point, a label, a
  // dropped stop) is only on the screen: each change is one step to undo.
  const history = undoScope("this suggestion");
  const take = () => ({ name: edit.name, lat: edit.lat, lon: edit.lon, members: edit.members.map((m) => ({ label: m.label, dropped: m.dropped })) });
  let snap = take();
  const remember = (label) => {
    const before = snap, after = take();
    snap = after;
    history.push({ label, undo: () => putBack(before), redo: () => putBack(after) });
  };
  function putBack(st) {
    Object.assign(edit, { name: st.name, lat: st.lat, lon: st.lon });
    edit.members.forEach((m, i) => Object.assign(m, st.members[i]));
    snap = st;
    if (nameInput) nameInput.value = st.name;
    title.textContent = st.name.trim() || p.name;
    touch();
    showPoint();
    drawMembers();
  }

  const mapMembers = () => edit.members.map((m) => ({ stop_id: m.stop_id, lat: m.lat, lon: m.lon, label: m.label.trim(), dropped: m.dropped }));
  // dragging the point fires many times a second; redraw the cards once a frame
  let frame = 0;
  const review = map.showReview({
    lat: p.lat, lon: p.lon, members: mapMembers(), editable,
    onMovePoint: (la, lo) => {
      edit.lat = la; edit.lon = lo; touch();
      if (!frame) frame = requestAnimationFrame(() => { frame = 0; showPoint(); drawMembers(); });
    },
    onMoveDone: () => remember("moved the station point"),
  });

  const title = h("h1", p.name);
  const nameInput = editable ? h("input", { type: "text", id: "proposal-name", value: p.name, autocomplete: "off" }) : null;
  const memberBox = h("ul.member-cards");
  const pointBox = h("div", { "aria-live": "polite" });
  const actionProblems = h("div");

  if (nameInput) {
    nameInput.addEventListener("input", () => { edit.name = nameInput.value; title.textContent = nameInput.value.trim() || p.name; touch(); });
    nameInput.addEventListener("change", () => remember("changed the station name"));
  }

  const showPoint = () => {
    const moved = haversine(p.lat, p.lon, edit.lat, edit.lon);
    clear(pointBox, h("p", `${fmtCoord(edit.lat)}, ${fmtCoord(edit.lon)}`, moved >= 1 ? h("span.hint", ` (moved ${fmtMetres(moved)} from the suggestion)`) : null));
  };

  function drawMembers() {
    review.update({ lat: edit.lat, lon: edit.lon }, mapMembers());
    clear(memberBox, edit.members.map((m, i) => {
      const d = haversine(edit.lat, edit.lon, m.lat, m.lon);
      const cur = m.current;
      const now = [];
      if (cur && cur.name !== m.name) now.push(`Now called ${cur.name}.`);
      if (cur && haversine(cur.lat, cur.lon, m.lat, m.lon) > 5) now.push(`Moved ${fmtMetres(haversine(cur.lat, cur.lon, m.lat, m.lon))} since it was suggested.`);
      const routes = m.routes;
      // one chip per route number; variants of a number share it
      const byNumber = new Map();
      (routes || []).forEach((r) => {
        const key = r.short_name || r.route_id;
        if (!byNumber.has(key)) byNumber.set(key, { ...r, variants: [] });
        const entry = byNumber.get(key);
        if (!entry.variants.some((v) => v.route_id === r.route_id)) entry.variants.push(r);
      });
      const unique = [...byNumber.values()];
      const shown = m.allRoutes ? unique : unique.slice(0, 12);
      return h("li.member-card", { class: m.dropped ? "dropped" : "" },
        h("div.member-card-head",
          h("span.member-title", h("strong", m.name), m.dropped ? h("span.chip.discarded", "Dropped") : null),
          h("span.meta", [m.stop_id, plural(m.route_count ?? (routes || []).length, "route"), `${fmtMetres(d)} from the station point`].join(" · "))),
        now.length ? h("p.hint", now.join(" ")) : null,
        editable
          ? h("label.field", { for: `platform-${i}` }, h("span", "Platform label"),
              h("input", { type: "text", id: `platform-${i}`, value: m.label, maxlength: String(PLATFORM_MAX), disabled: m.dropped, placeholder: "For example Towards Guindy",
                on: { input: (ev) => { m.label = ev.target.value; touch(); review.update(null, mapMembers()); }, change: () => remember(`changed the platform label of ${m.name}`) } }))
          : h("p", h("span.hint", "Platform label: "), m.platform_code || "none"),
        h("div.member-routes",
          h("span.hint", routes === null ? "Loading routes…" : unique.length ? "Routes here:" : "No route uses this stop."),
          shown.map((r) => h("button.route-chip", {
            type: "button", "aria-pressed": String(m.showing === r.route_id),
            title: r.variants.map((v) => `${v.long_name || v.route_id} (route id ${v.route_id})`).join("\n"),
            "aria-label": `Show route ${r.short_name || r.route_id} on the map${r.variants.length > 1 ? `, ${r.variants.length} variants` : ""}`,
            on: { click: () => showRouteOf(m, r) },
          }, r.short_name || r.route_id, r.variants.length > 1 ? h("span.variants", ` ×${r.variants.length}`) : null)),
          unique.length > shown.length
            ? h("button.btn.quiet.small", { type: "button", on: { click: () => { m.allRoutes = true; drawMembers(); } } }, `+${unique.length - shown.length} more`) : null),
        h("div.btn-row",
          h("button.btn.quiet.small", { type: "button", on: { click: () => map.fitPoints([m], { maxZoom: 19 }) } }, "Show on map"),
          h("a.btn.quiet.small", { href: `#/stop/${enc(m.stop_id)}` }, "Open stop"),
          editable ? h("button.btn.quiet.small", { type: "button", id: `drop-${i}`, "aria-pressed": String(m.dropped),
            on: { click: () => { m.dropped = !m.dropped; touch(); remember(m.dropped ? `dropped ${m.name}` : `put ${m.name} back`); drawMembers(); document.getElementById(`drop-${i}`)?.focus(); } } }, m.dropped ? "Put back in the station" : "Drop from the station") : null));
    }));
  }

  async function showRouteOf(m, r) {
    edit.members.forEach((x) => { if (x !== m) x.showing = null; });
    if (m.showing === r.route_id) {
      m.showing = null;
      review.showRoute(null);
      drawMembers();
      return;
    }
    m.showing = r.route_id;
    drawMembers();
    try {
      review.showRoute(await get(`feeds/${enc(state.feedId)}/routes/${enc(r.route_id)}`));
    } catch (e) {
      toast(e.message, "error");
    }
  }

  const centre = () => {
    const kept = edit.members.filter((m) => !m.dropped);
    if (!kept.length) return;
    edit.lat = kept.reduce((a, m) => a + m.lat, 0) / kept.length;
    edit.lon = kept.reduce((a, m) => a + m.lon, 0) / kept.length;
    touch();
    remember("placed the station point in the middle");
    showPoint();
    drawMembers();
  };

  // previous and next in the list this came from
  const at = list.items.findIndex((x) => x.proposal_id === p.proposal_id);
  const prev = at > 0 ? list.items[at - 1] : null;
  const next = at >= 0 ? list.items[at + 1] : null;
  const goNext = () => {
    const i = list.items.findIndex((x) => x.proposal_id === p.proposal_id);
    if (i >= 0 && list.status === "pending") list.items.splice(i, 1);
    const following = list.items[i >= 0 ? i : 0];
    location.hash = following && following.status === "pending" ? `#/stations/${following.proposal_id}` : "#/stations";
  };

  const approve = async () => {
    clear(actionProblems);
    const kept = edit.members.filter((m) => !m.dropped);
    const errs = [];
    if (!edit.name.trim()) errs.push("Give the station a name.");
    if (kept.length < 2) errs.push("A station groups at least two stops. Put a stop back, or reject this suggestion.");
    if (kept.some((m) => m.label.trim().length > PLATFORM_MAX)) errs.push(`A platform label can be at most ${PLATFORM_MAX} characters.`);
    if (errs.length) return clear(actionProblems, h("div.notice.error", { role: "alert" }, h("ul", errs.map((e) => h("li", e)))));
    const draft = await requireDraft("Approved stations are added to a draft. They go live only when someone else approves the draft and it is committed.");
    if (!draft) return;
    const body = { change_set_id: draft.change_set_id };
    if (edit.name.trim() !== p.name) body.name = edit.name.trim();
    if (edit.lat !== p.lat || edit.lon !== p.lon) { body.lat = round7(edit.lat); body.lon = round7(edit.lon); }
    const relabelled = kept.some((m) => (m.label.trim() || null) !== (m.platform_code || null));
    if (kept.length !== edit.members.length || relabelled) {
      body.members = kept.map((m) => ({ stop_id: m.stop_id, platform_code: m.label.trim() || null }));
    }
    try {
      await post(`station-proposals/${enc(p.proposal_id)}/approve`, body);
      dirty = false;
      history.clear();
      setLeaveGuard(null);
      await refreshDraft();
      toast(`${edit.name.trim()} added to draft “${draft.title}”.`);
      refreshStationCount();
      goNext();
    } catch (e) {
      const problems = (e instanceof ApiError && e.details && e.details.problems) || [];
      clear(actionProblems, h("div.notice.error", { role: "alert" },
        h("p", h("strong", e.message)),
        problems.length ? h("ul", problems.map((x) => h("li", x.message))) : null));
      if (e instanceof ApiError && e.code === "proposal_not_pending") toast("Someone else has already reviewed this suggestion.", "error");
    }
  };

  const reject = async () => {
    const note = await modal("Reject this suggestion", (close) => {
      const ta = h("textarea", { id: "reject-note", placeholder: "Why is this not one station?" });
      const err = h("p.notice.error", { role: "alert", hidden: true });
      return h("form", { style: "display:grid;gap:10px", on: { submit: (ev) => {
        ev.preventDefault();
        if (!ta.value.trim()) { err.hidden = false; err.textContent = "Write a short reason, so the next person knows why."; ta.focus(); return; }
        close(ta.value.trim());
      } } },
      h("label.field", { for: "reject-note" }, h("span", "Reason"), ta),
      h("div.btn-row", h("span.hint", "Common reasons:"), REJECT_REASONS.map((r) => h("button.btn.quiet.small", { type: "button", on: { click: () => { ta.value = r; ta.focus(); } } }, r))),
      err,
      h("div.btn-row", h("button.btn.danger", { type: "submit" }, "Reject suggestion"), h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
    });
    if (note === undefined) return;
    try {
      await post(`station-proposals/${enc(p.proposal_id)}/reject`, { note });
      dirty = false;
      setLeaveGuard(null);
      toast(`${p.name} rejected.`);
      refreshStationCount();
      goNext();
    } catch (e) {
      toast(e.message, "error");
    }
  };

  const reopen = async () => {
    try {
      await post(`station-proposals/${enc(p.proposal_id)}/reopen`);
      toast(`${p.name} is back in the list to review.`);
      refreshStationCount();
      list.loaded = false;
      showProposal(p.proposal_id);
    } catch (e) {
      toast(e.message, "error");
    }
  };

  const statusNotice = {
    pending: null,
    approved: h("div.notice.draft",
      h("p", "Approved into draft ", p.change_set_id ? h("a", { href: `#/drafts/${enc(p.change_set_id)}` }, `“${p.change_set_title || "untitled"}”`) : "(unknown)",
        p.reviewed_by_email ? ` by ${p.reviewed_by_email}` : "", p.reviewed_at ? ` on ${fmtDate(p.reviewed_at)}` : "", "."),
      h("p", "It goes live when that draft is approved by someone else and committed. To undo, remove the station from the draft.")),
    rejected: h("div.notice",
      h("p", "Rejected", p.reviewed_by_email ? ` by ${p.reviewed_by_email}` : "", p.reviewed_at ? ` on ${fmtDate(p.reviewed_at)}` : "", p.review_note ? `: “${p.review_note}”` : "."),
      can("editor") ? h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: reopen } }, "Reopen for review")) : null),
    committed: h("p.notice.ok", "This station is live", p.change_set_id ? [", committed with draft ", h("a", { href: `#/drafts/${enc(p.change_set_id)}` }, `“${p.change_set_title || "untitled"}”`)] : "", "."),
  }[p.status];

  clear(panel(),
    h("section.section",
      h("div.section-head",
        h("a.crumb", { href: "#/stations" }, "Back to stations to review"),
        h("div.btn-row.pager-mini",
          prev ? h("a.btn.quiet.small", { href: `#/stations/${prev.proposal_id}`, "aria-label": `Previous suggestion: ${prev.name}` }, "‹ Previous") : null,
          next ? h("a.btn.quiet.small", { href: `#/stations/${next.proposal_id}`, "aria-label": `Next suggestion: ${next.name}` }, "Next ›") : null)),
      h("div.title-block",
        title,
        h("p.ids", `Suggested station ${p.station_id} · ${spread(p)}`)),
      statusChip(p),
      statusNotice,
      p.problems && p.problems.length ? h("div.notice.warning", h("p", h("strong", "Check before approving")), h("ul", p.problems.map((x) => h("li", x.message)))) : null,
      editable ? h("label.field", { for: "proposal-name" }, h("span", "Station name"), nameInput) : null,
      !can("editor") ? h("p.hint", "You can look at suggestions. Ask an admin for the editor role to review them.") : null),
    h("section.section",
      h("h2", `Stops in this station (${edit.members.length})`),
      h("p.hint", "Each stop is one kerb. Its label tells passengers which way the buses there go. Click a route to see it on the map."),
      memberBox),
    h("section.section",
      h("h2", "Station point"),
      pointBox,
      editable ? [
        h("div.btn-row", h("button.btn.secondary.small", { type: "button", on: { click: centre } }, "Place in the middle of its stops")),
        h("p.hint", "Or drag the dark square on the map."),
        history.buttons()] : null),
    editable ? h("div.sticky-actions",
      actionProblems,
      h("p.hint", state.draft ? `Approving adds this station to draft “${state.draft.title}”.` : "Approving asks which draft to add it to."),
      h("div.btn-row",
        h("button.btn", { type: "button", on: { click: approve } }, "Approve into draft"),
        h("button.btn.danger", { type: "button", on: { click: reject } }, "Reject…"),
        next ? h("a.btn.quiet", { href: `#/stations/${next.proposal_id}` }, "Skip") : null)) : null);
  showPoint();
  drawMembers();
  // the routes through each kerb
  await Promise.all(edit.members.map((m) => get(`feeds/${enc(state.feedId)}/stops/${enc(m.stop_id)}`)
    .then((d) => { m.routes = d.routes; m.current = d; m.route_count = d.route_count; })
    .catch(() => { m.routes = []; })));
  if (document.body.contains(memberBox)) drawMembers();
}
