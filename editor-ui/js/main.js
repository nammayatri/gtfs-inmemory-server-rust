// Boot, the top bar and the hash router.
import { get, enc } from "./api.js";
import { state, set, subscribe, pref, setPref, can } from "./state.js";
import { ensureSignedIn, signOut } from "./auth.js";
import { h, toast, ROLE_LABEL } from "./util.js";
import * as map from "./map.js";
import { initSearch, showHome, showStop, showRoute } from "./explore.js";
import { loadActiveDraft, renderDraftChip, chooseDraft } from "./drafts.js";
import { showDraftList, showDraft } from "./review.js";
import { showPeople, showHistory } from "./admin.js";

let reauthing = false;

async function boot() {
  const me = await ensureSignedIn();
  set({ me });
  document.getElementById("app").hidden = false;

  let feeds = [];
  try {
    feeds = await get("feeds");
  } catch (e) {
    toast(e.message, "error");
  }
  const feedId = feeds.some((f) => f.gtfs_id === pref("feed")) ? pref("feed") : feeds[0]?.gtfs_id || null;
  set({ feeds, feedId });

  const select = document.getElementById("feed-select");
  select.replaceChildren(...feeds.map((f) => h("option", { value: f.gtfs_id, selected: f.gtfs_id === feedId }, f.display_name || f.gtfs_id)));
  select.addEventListener("change", async () => {
    setPref("feed", select.value);
    set({ feedId: select.value });
    await loadActiveDraft();
    map.refreshStops();
    location.hash = "#/";
    route();
  });

  document.getElementById("user-summary").textContent = me.display_name || me.email;
  document.getElementById("user-role").textContent = `${me.email}, ${ROLE_LABEL[me.role].toLowerCase()}`;
  document.getElementById("sign-out").addEventListener("click", signOut);
  document.querySelector('[data-nav="admin"]').hidden = !can("admin");
  document.getElementById("draft-chip").addEventListener("click", chooseDraftAndGo);

  subscribe(renderDraftChip);
  await loadActiveDraft();
  renderDraftChip();

  map.initMap();
  initSearch();
  window.addEventListener("hashchange", route);
  route();
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
}

function route() {
  if (!state.feedId) {
    showWorkspace(false);
    document.getElementById("page").replaceChildren(h("div.page-inner", h("h1", "No feeds"), h("p.notice", "The editor has no feeds to show. Ask an engineer to load one.")));
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
    markNav("map"); showWorkspace(true); showRoute(parts[1], { preview: params.get("draft") === "1" });
  } else if (parts[0] === "drafts" && parts[1]) {
    markNav("drafts"); showWorkspace(false); showDraft(parts[1]);
  } else if (parts[0] === "drafts") {
    markNav("drafts"); showWorkspace(false); showDraftList(params.get("status") || "draft");
  } else if (parts[0] === "admin") {
    markNav("admin"); showWorkspace(false); showPeople();
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
  set({ me });
  document.getElementById("app").hidden = false;
  reauthing = false;
  route();
});

boot();
