// The trail: where a drill-down came from. Opening a route, then one of its
// stops, then another route through that stop leaves a trail "Map › 45B › Luz ›
// 12C", shown under the top bar with one Back control. It is not the browser's
// history: it holds each place once, going to a place already in it goes BACK to
// it (dropping what came after), and starting again from the top bar or the
// search box starts a new trail. It lasts for the tab's session only.
import { h, clear } from "./util.js";

const KEY = "gtfs-editor-trail";
const MAX = 8;
let crumbs = [];            // [{label, href}], oldest first; the last is where we are
let fresh = 0;              // when a new trail was asked for (ms), or 0

// The same place whatever its query says: #/route/12?draft=1 is #/route/12.
const place = (href) => String(href || "#/").split("?")[0].replace(/\/+$/, "") || "#";

function save() {
  try {
    sessionStorage.setItem(KEY, JSON.stringify(crumbs));
  } catch {
    /* no session storage: the trail lives as long as the page */
  }
}

function restore() {
  try {
    const saved = JSON.parse(sessionStorage.getItem(KEY) || "[]");
    if (Array.isArray(saved)) crumbs = saved.filter((c) => c && typeof c.href === "string" && typeof c.label === "string").slice(-MAX);
  } catch {
    crumbs = [];
  }
}

// Top-level pages: the root of a trail, never a step inside one.
const ROOTS = [
  [/^#\/?$/, "Map"], [/^#\/coordinates$/, "Coordinates to review"], [/^#\/stations$/, "Stations to review"],
  [/^#\/drafts$/, "Drafts"], [/^#\/audit$/, "History"], [/^#\/admin$/, "People"], [/^#\/feed-settings$/, "Feed settings"],
  [/^#\/import$/, "Import"],
];
const rootLabel = (href) => (ROOTS.find(([re]) => re.test(place(href))) || [])[1] || null;

// Words for a place before its page has loaded and named itself.
function provisional(href) {
  const [, kind, id] = place(href).split("/").map(decodeURIComponent);
  const word = { stop: "Stop", route: "Route", stations: "Suggested station", coordinates: "Coordinate review", drafts: "Draft", merge: "Merge", new: "New" }[kind];
  return word ? `${word} ${id || ""}`.trim() : "Here";
}

// The top bar, the brand and a search result start a new trail. The mark is a
// time, not a flag: a click that goes nowhere (the page already open) must not
// reset whatever drill-down comes next.
export function startFresh() {
  fresh = Date.now();
}

// The router calls this for every screen it shows.
export function arrive(href) {
  const at = place(href);
  const root = rootLabel(href);
  if ((fresh && Date.now() - fresh < 3000) || root) {
    crumbs = [{ label: root || provisional(href), href }];
  } else {
    const i = crumbs.findIndex((c) => place(c.href) === at);
    if (i >= 0) {
      crumbs = crumbs.slice(0, i + 1);
      crumbs[i].href = href;
    } else {
      crumbs.push({ label: provisional(href), href });
      // too long: the oldest steps after the root go
      if (crumbs.length > MAX) crumbs.splice(1, crumbs.length - MAX);
    }
  }
  fresh = 0;
  save();
  render();
}

// A page names itself once it has loaded ("Luz", "45B").
export function nameHere(label, href = location.hash || "#/") {
  const last = crumbs[crumbs.length - 1];
  if (!last || place(last.href) !== place(href) || !label) return;
  last.label = String(label);
  save();
  render();
}

export function resetTrail() {
  crumbs = [];
  fresh = 0;
  save();
  render();
}

export const trail = () => crumbs.map((c) => ({ ...c }));
export const parent = () => (crumbs.length > 1 ? { ...crumbs[crumbs.length - 2] } : null);

function render() {
  const bar = document.getElementById("trail");
  if (!bar) return;
  if (crumbs.length < 2) {
    bar.classList.add("is-empty");
    clear(bar);
    return;
  }
  bar.classList.remove("is-empty");
  const back = crumbs[crumbs.length - 2];
  clear(bar,
    h("a.btn.quiet.small.trail-back", { href: back.href, "aria-label": `Back to ${back.label}` }, "‹ Back"),
    h("ol.trail-list", crumbs.map((c, i) => h("li", i === crumbs.length - 1
      ? h("span.trail-here", { "aria-current": "page", title: c.label }, c.label)
      : h("a", { href: c.href, title: c.label }, c.label)))));
}

export function initTrail() {
  restore();
  // what the restored trail ends on is not where this page load is: drop it
  // unless the address is still its last place (a reload mid drill-down)
  const last = crumbs[crumbs.length - 1];
  if (!last || place(last.href) !== place(location.hash || "#/")) crumbs = [];
  render();
}
