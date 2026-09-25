// Boot, the top bar and the hash router.
import { get, enc } from "./api.js";
import { state, set, subscribe, pref, setPref, can, isAdmin, feedRole, leaveMessage, setLeaveGuard } from "./state.js";
import { ensureSignedIn, signOut } from "./auth.js";
import { h, toast, confirmDialog, ROLE_LABEL } from "./util.js";
import * as map from "./map.js";
import { initSearch, showHome, showStop, showRoute } from "./explore.js";
import { loadActiveDraft, renderDraftChip, chooseDraft, requireDraft, createdStops } from "./drafts.js";
import { showDraftList, showDraft } from "./review.js";
import { showPeople, showHistory, showFeedSettings } from "./admin.js";
import { showDelivery, leaveWebhooks } from "./webhooks.js";
import { showStationsList, showProposal, refreshStationCount, leaveStations } from "./stations.js";
import { showCoordinatesList, showCoordinateReview, refreshCoordinateCount, leaveCoordinates } from "./coordinates.js";
import { newStop, newRoute } from "./create.js";
import { editStation } from "./editors.js";
import { showMerge } from "./merge.js";
import { showStationMerge } from "./station_merge.js";
import { showImport } from "./importer.js";
import { showFiles, showFile, showRecord } from "./files.js";
import { showTrips } from "./trips.js";
import { showCalendar } from "./calendar.js";
import { showFeed } from "./feed.js";
import { initTrail, arrive, startFresh, resetTrail } from "./trail.js";
import { resetUndo } from "./undo.js";

let reauthing = false;
let refreshingAccess = false;
let currentHash = location.hash || "#/";

// The feeds this person may work on come with /auth/me (docs/gtfs-editor.md
// section 15): every feed for an admin, else the ones granted, each with the
// role held there. The switcher offers only those, and keeps the one chosen
// before while it is still among them.
function applyMe(me) {
  const feeds = me.feeds || [];
  const keep = [state.feedId, pref("feed")].find((id) => id && feeds.some((f) => f.gtfs_id === id));
  set({ me, feeds, feedId: keep || feeds[0]?.gtfs_id || null });
  const select = document.getElementById("feed-select");
  select.replaceChildren(...feeds.map((f) => h("option", { value: f.gtfs_id, selected: f.gtfs_id === state.feedId }, f.display_name || f.gtfs_id)));
  // the switcher is kept narrow; its whole name shows on hover
  const chosen = feeds.find((f) => f.gtfs_id === state.feedId);
  select.title = chosen ? `${chosen.display_name || chosen.gtfs_id} (${chosen.gtfs_id})` : "";
  renderAccess();
}

// What this person may do on the chosen feed: the role badge, the admin pages,
// the New menu. Everything else asks can() when it draws.
function renderAccess() {
  const me = state.me;
  const role = feedRole();
  const feed = state.feeds.find((f) => f.gtfs_id === state.feedId);
  const where = me.is_admin ? "every feed" : feed?.display_name || state.feedId;
  document.getElementById("user-role").textContent = `${me.email}, ${role ? `${ROLE_LABEL[role].toLowerCase()} on ${where}` : "no feeds yet"}`;
  const badge = document.getElementById("feed-role");
  badge.hidden = !role;
  badge.textContent = role ? ROLE_LABEL[role] : "";
  badge.title = role ? `Your role on ${where}` : "";
  const noFeed = !state.feedId;
  for (const sel of [".feed-picker", ".search", ".nav"]) document.querySelector(sel).hidden = noFeed;
  document.querySelector('[data-nav="admin"]').hidden = !isAdmin();
  document.querySelector('[data-nav="feed-settings"]').hidden = !isAdmin();
  document.getElementById("new-menu").hidden = noFeed || !can("editor");
}

async function switchFeed(feedId) {
  setPref("feed", feedId);
  set({ feedId });
  renderAccess();
  await loadActiveDraft();
  map.refreshStops();
  refreshStationCount();
  refreshCoordinateCount();
  setLeaveGuard(null);
  resetTrail();
  location.hash = "#/";
  route();
}

async function boot() {
  const me = await ensureSignedIn();
  document.getElementById("app").hidden = false;
  applyMe(me);

  const select = document.getElementById("feed-select");
  select.addEventListener("change", () => switchFeed(select.value));
  // a link in the top bar (the brand, the pages, the New menu) starts a new trail;
  // so does a search result (explore.js)
  document.querySelector(".topbar").addEventListener("click", (ev) => { if (ev.target.closest("a")) startFresh(); });
  initTrail();

  document.getElementById("user-summary").textContent = me.display_name || me.email;
  document.getElementById("sign-out").addEventListener("click", signOut);
  document.getElementById("draft-chip").addEventListener("click", chooseDraftAndGo);
  initNewMenu();

  subscribe(renderDraftChip);
  await loadActiveDraft();
  renderDraftChip();

  map.initMap();
  // stops the draft creates are drawn on the map, and offered by the stop pickers
  subscribe(() => map.setDraftStops(createdStops()));
  map.setDraftStops(createdStops());
  initSearch();
  refreshStationCount();
  refreshCoordinateCount();
  window.addEventListener("hashchange", onHashChange);
  window.addEventListener("beforeunload", (ev) => {
    if (leaveMessage()) { ev.preventDefault(); ev.returnValue = ""; }
  });
  route();
}

// The top bar's menus (New, Feed data): small disclosures that close on a
// choice, Escape or a click away; opening one closes the others.
function initNewMenu() {
  const menus = [document.getElementById("new-menu"), ...document.querySelectorAll(".nav-menu")];
  for (const menu of menus) {
    const close = () => { menu.open = false; };
    menu.querySelectorAll("a").forEach((a) => a.addEventListener("click", close));
    menu.addEventListener("keydown", (ev) => {
      if (ev.key === "Escape" && menu.open) { close(); menu.querySelector("summary").focus(); }
    });
    menu.addEventListener("toggle", () => {
      if (menu.open) menus.filter((m) => m !== menu).forEach((m) => { m.open = false; });
    });
    document.addEventListener("click", (ev) => { if (menu.open && !menu.contains(ev.target)) close(); });
  }
}

async function chooseDraftAndGo() {
  const before = state.draft?.change_set_id;
  const cs = await chooseDraft();
  if (cs && cs.change_set_id !== before) toast(`Edits now go into "${cs.title}".`);
  if (cs && location.hash.startsWith("#/drafts")) location.hash = `#/drafts/${enc(cs.change_set_id)}`;
}

function showWorkspace(show) {
  document.getElementById("workspace").hidden = !show;
  document.getElementById("page").hidden = show;
  if (show) map.invalidate();
  else map.endModes();
}

function markNav(name) {
  document.querySelectorAll("[data-nav]").forEach((a) => {
    if (a.dataset.nav === name) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  // a menu of the top bar reads as current when one of its pages is
  document.querySelectorAll(".nav-menu").forEach((m) => {
    m.querySelector("summary").classList.toggle("current", !!m.querySelector(`[data-nav="${name}"]`));
  });
}

// Leaving a screen with unsaved edits asks first; staying puts the address back.
async function onHashChange() {
  const target = location.hash || "#/";
  if (target === currentHash) return;
  const message = leaveMessage();
  if (message) {
    history.replaceState(null, "", currentHash);
    const leave = await confirmDialog("Leave without saving?", `${message} If you leave now, those changes are lost.`, { confirm: "Leave without saving", danger: true });
    if (!leave) return;
    setLeaveGuard(null);
    history.pushState(null, "", target);
  }
  currentHash = target;
  route();
}

async function newStation() {
  if (!(await requireDraft("New stations are added to a draft. Nothing changes for passengers until someone else approves it and it is committed."))) {
    location.hash = "#/";
    return;
  }
  editStation(null, []);
}

function editorsOnly(what, fn) {
  if (can("editor")) return fn();
  document.getElementById("panel").replaceChildren(h("section.section",
    h("a.crumb", { href: "#/" }, "Back to the map"),
    h("h1", what),
    h("p.notice", "This needs the editor role. Ask an admin if you should be making changes.")));
  return null;
}

function route() {
  currentHash = location.hash || "#/";
  leaveStations();
  leaveCoordinates();
  leaveWebhooks();
  setLeaveGuard(null);
  // what could be undone belonged to the screen being left
  resetUndo();
  arrive(currentHash);
  if (!state.feedId) {
    showWorkspace(false);
    document.getElementById("page").replaceChildren(isAdmin()
      ? h("div.page-inner", h("h1", "No feeds"), h("p.notice", "The editor has no feeds to show. Ask an engineer to load one."))
      : h("div.page-inner#no-feeds", h("h1", "You have no feeds yet — ask an admin"),
        h("p.notice", "You are signed in, but nobody has given you a feed to work on yet. Ask a GTFS editor admin to add you to the feeds you need, then reload this page.")));
    return;
  }
  const raw = location.hash.replace(/^#/, "") || "/";
  const [path, query = ""] = raw.split("?");
  const params = new URLSearchParams(query);
  const parts = path.split("/").filter(Boolean).map(decodeURIComponent);
  window.scrollTo(0, 0);

  if (parts[0] === "stop" && parts[1]) {
    markNav("map"); showWorkspace(true); showStop(parts[1]);
  } else if (parts[0] === "route" && parts[1]) {
    markNav("map"); showWorkspace(true); // ?draft=1 asks for the draft applied; without it the route page decides (it
    // applies the draft whenever the draft touches the route)
    showRoute(parts[1], { preview: params.get("draft") === "1" ? true : undefined });
  } else if (parts[0] === "stations" && parts[1]) {
    markNav("stations"); showWorkspace(true); showProposal(parts[1]);
  } else if (parts[0] === "stations") {
    markNav("stations"); showWorkspace(true); showStationsList();
  } else if (parts[0] === "coordinates" && parts[1]) {
    markNav("coordinates"); showWorkspace(true); showCoordinateReview(parts[1]);
  } else if (parts[0] === "coordinates") {
    markNav("coordinates"); showWorkspace(true); showCoordinatesList();
  } else if (parts[0] === "new" && ["stop", "route", "station"].includes(parts[1])) {
    markNav("map"); showWorkspace(true);
    const [title, fn] = { stop: ["New stop", newStop], route: ["New route", newRoute], station: ["New station", newStation] }[parts[1]];
    editorsOnly(title, () => fn());
  } else if (parts[0] === "merge" && parts[1]) {
    markNav("map"); showWorkspace(true); showMerge(parts[1], params.get("with"));
  } else if (parts[0] === "station-merge" && parts[1]) {
    markNav("map"); showWorkspace(true); showStationMerge(parts[1], params.get("with"));
  } else if (parts[0] === "import") {
    markNav("import"); showWorkspace(false); showImport(params.get("kind"), params.get("file"));
  } else if (parts[0] === "files" && parts[1] && parts[2]) {
    markNav("files"); showWorkspace(false); showRecord(parts[1], parts[2]);
  } else if (parts[0] === "files" && parts[1]) {
    markNav("files"); showWorkspace(false); showFile(parts[1]);
  } else if (parts[0] === "files") {
    markNav("files"); showWorkspace(false); showFiles();
  } else if (parts[0] === "trips" && parts[1]) {
    markNav("map"); showWorkspace(false); showTrips(parts[1], { pattern: params.get("pattern") });
  } else if (parts[0] === "calendar") {
    markNav("calendar"); showWorkspace(false); showCalendar();
  } else if (parts[0] === "feed") {
    markNav("feed"); showWorkspace(false); showFeed();
  } else if (parts[0] === "drafts" && parts[1]) {
    markNav("drafts"); showWorkspace(false); showDraft(parts[1]);
  } else if (parts[0] === "drafts") {
    markNav("drafts"); showWorkspace(false); showDraftList(params.get("status") || "draft");
  } else if (parts[0] === "admin") {
    markNav("admin"); showWorkspace(false); showPeople();
  } else if (parts[0] === "feed-settings") {
    markNav("feed-settings"); showWorkspace(false); showFeedSettings();
  } else if (parts[0] === "delivery") {
    markNav("delivery"); showWorkspace(false); showDelivery();
  } else if (parts[0] === "audit") {
    markNav("audit"); showWorkspace(false); showHistory(params.get("change_set"));
  } else {
    markNav("map"); showWorkspace(true); showHome();
  }
}

// A session can end mid-use (12 h expiry, an admin reset). Send the person back
// through the gate, then to where they were.
window.addEventListener("auth:required", async () => {
  if (reauthing || !state.me) return;
  reauthing = true;
  const me = await ensureSignedIn();
  document.getElementById("app").hidden = false;
  applyMe(me);
  reauthing = false;
  route();
});

// A grant can change while the page is open: the next call on that feed says
// so (403 no_feed_access), and the switcher and the buttons follow what the
// person holds now. A feed taken away is left for the first one still held.
// A draft of some other feed opened by its address says the same thing with
// nothing changed; then the page's own message is the whole answer.
window.addEventListener("access:changed", async () => {
  if (refreshingAccess || !state.me) return;
  refreshingAccess = true;
  try {
    const before = state.feedId;
    const held = (me) => JSON.stringify([!!me.is_admin, me.feeds || []]);
    const me = await get("auth/me");
    if (held(me) === held(state.me)) return;
    applyMe(me);
    if (state.feedId !== before) {
      toast("Your access to that feed was taken away. Ask an admin if you still need it.", "error");
      await switchFeed(state.feedId);
    } else {
      route();
    }
  } catch {
    /* the call that failed has said so already */
  } finally {
    refreshingAccess = false;
  }
});

boot();
