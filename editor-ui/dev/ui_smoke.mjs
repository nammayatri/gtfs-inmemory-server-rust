// End-to-end smoke test of the dashboard against dev/mock_server.py, driving a
// headless Chrome over the DevTools protocol (Node 22+, no dependencies).
//
//   python dev/mock_server.py &            # port 8765, fresh state for every run
//   node dev/ui_smoke.mjs [--shots /tmp/gtfs-editor-shots]
//   node dev/ui_smoke.mjs --station-merge    # only the merge-two-stations screen
//   node dev/ui_smoke.mjs --feed-access      # only who may work on which feed
//
// Every flow runs through the real UI: sign-in, TOTP enrolment; the map (stop
// labels, clicking stops while a route or stop is open, the search list above the
// map controls, the map following the panel's width); the route stop list editor
// (add stop, change stop, stage names, renumbering); a stop move, a station, a
// map line; New stop, New route with its stop list, New station; stations to
// review (edit, approve, reject, reopen, approve an area); merging duplicate
// stops; coordinates to review (the list, a move by suggestion, raw point, map
// click and drag, position is correct and reopen, splitting routes off a stop
// with routes from three original stops and its draft conflict, a stop merged
// away); importing CSV files into a draft of 1,200 changes, which the API answers
// 200 at a time and the dashboard pages through; submit, approve and
// commit by a second person, a commit conflict, people and history; a feed's
// data source switched through a draft, approved by the admin who submitted it
// (the override, its confirm, badge and history row); the "Stations to review"
// link hiding once nothing is left to review. Then the round 4 flows
// (round4Flows below; `--round4` runs only those): what the map shows, stops
// sharing a point, stations listed once, undo and redo, the trail, the cleanup
// context, candidates and merge on a review, and pending changes shown on the
// pages they change. Then the round 5 flows (round5Flows; `--round5`): the lines
// that tie a station to its platforms, a description and a platform label on
// stops and stations, and importing stop details. Then the map line flows
// (mapLineFlows; `--map-line`): a line through the stops and from GPS, and the
// reason when either fails. Last, who may work on which feed (feedAccessFlows;
// `--feed-access`): a system account at the gate, the no-feeds screen, the
// switcher, badges and buttons by the role on the chosen feed, the People grid,
// and a grant taken away while the page is open.
import { spawn } from "node:child_process";
import { mkdirSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BASE = process.env.MOCK_URL || "http://127.0.0.1:8765";
const UI = `${BASE}/internal/gtfs-editor/ui/`;
const MAPJS = `${UI}js/map.js`;
const CDP_PORT = Number(process.env.CDP_PORT || 9333);
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const shotsArg = process.argv.indexOf("--shots");
const SHOTS = shotsArg > 0 ? process.argv[shotsArg + 1] : join(tmpdir(), "gtfs-editor-shots");
mkdirSync(SHOTS, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failures.push(what); return ok; };

// ------------------------------------------------------------------ CDP
try {
  await fetch(`http://127.0.0.1:${CDP_PORT}/json/version`);
  console.log(`FAIL something already listens on port ${CDP_PORT} (a Chrome left from an earlier run?). Stop it or set CDP_PORT.`);
  process.exit(1);
} catch { /* free, as it should be */ }
const profile = mkdtempSync(join(tmpdir(), "gtfs-editor-chrome-"));
// the throttling flags keep timers and animation frames running at full speed
// in a headless window, as they do in a visible one
const chrome = spawn(CHROME, ["--headless=new", `--remote-debugging-port=${CDP_PORT}`, `--user-data-dir=${profile}`,
  "--no-first-run", "--window-size=1440,900", "--disable-background-timer-throttling",
  "--disable-renderer-backgrounding", "--disable-backgrounding-occluded-windows", "about:blank"], { stdio: "ignore" });
let ws, nextId = 1;
const pending = new Map();
const consoleErrors = [];

async function connect() {
  for (let i = 0; i < 50; i++) {
    try {
      const targets = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json`)).json();
      const page = targets.find((t) => t.type === "page");
      if (page) {
        ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((r) => ws.addEventListener("open", r, { once: true }));
        ws.addEventListener("message", (ev) => {
          const msg = JSON.parse(ev.data);
          if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
          if (msg.method === "Runtime.exceptionThrown") consoleErrors.push(msg.params.exceptionDetails.exception?.description || msg.params.exceptionDetails.text);
          if (msg.method === "Runtime.consoleAPICalled" && msg.params.type === "error") consoleErrors.push(msg.params.args.map((a) => a.value || a.description).join(" "));
        });
        return;
      }
    } catch { /* not up yet */ }
    await sleep(200);
  }
  throw new Error("Chrome did not start");
}
const send = (method, params = {}) => new Promise((resolve, reject) => {
  const id = nextId++;
  pending.set(id, (msg) => (msg.error ? reject(new Error(`${method}: ${msg.error.message}`)) : resolve(msg.result)));
  ws.send(JSON.stringify({ id, method, params }));
});
async function evaluate(expr) {
  const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(`${expr.slice(0, 80)}: ${r.exceptionDetails.exception?.description || r.exceptionDetails.text}`);
  return r.result.value;
}
async function waitFor(expr, what, timeout = 8000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    try { if (await evaluate(expr)) return true; } catch { /* retry */ }
    await sleep(150);
  }
  check(false, `timed out waiting for ${what}`);
  return false;
}
async function shot(name) {
  const r = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(join(SHOTS, `${name}.png`), Buffer.from(r.data, "base64"));
}
async function go(hash) { await evaluate(`location.hash = ${JSON.stringify(hash)}`); await sleep(500); }
async function load(url) { await send("Page.navigate", { url }); await sleep(1200); }
// click the first visible button/link/summary whose text includes `text`, optionally inside a selector
async function click(text, scope = "body") {
  const ok = await evaluate(`(() => {
    const sel = ["button", "a", "summary"].map((t) => ${JSON.stringify(scope)} + " " + t).join(", ");
    const els = [...document.querySelectorAll(sel)]
      .filter((e) => e.offsetParent !== null && !e.disabled && e.textContent.trim().includes(${JSON.stringify(text)}));
    if (!els.length) return false;
    els[0].click();
    return true;
  })()`);
  check(ok, `click "${text}"${scope !== "body" ? ` in ${scope}` : ""}`);
  await sleep(400);
  return ok;
}
async function clickSel(selector, what = selector) {
  const ok = await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return false; el.click(); return true; })()`);
  check(ok, `click ${what}`);
  await sleep(400);
  return ok;
}
async function type(selector, value) {
  await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); el.focus(); el.value = ${JSON.stringify(value)};
    el.dispatchEvent(new Event("input", { bubbles: true })); el.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`);
  await sleep(300);
}
async function choose(selector, value) {
  await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); el.value = ${JSON.stringify(value)};
    el.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`);
  await sleep(300);
}
const text = (sel = "body") => evaluate(`document.querySelector(${JSON.stringify(sel)})?.innerText || ""`);
const has = (sel) => evaluate(`!!document.querySelector(${JSON.stringify(sel)})`);
const api = (path) => evaluate(`fetch(${JSON.stringify(`${BASE}/internal/gtfs-editor/`)} + ${JSON.stringify(path)}, { credentials: "same-origin" }).then((r) => r.json())`);
// a mutation as the signed-in person, for setting up a situation the UI then meets
const apiSend = (method, path, body) => evaluate(`fetch(${JSON.stringify(`${BASE}/internal/gtfs-editor/`)} + ${JSON.stringify(path)}, {
  method: ${JSON.stringify(method)}, credentials: "same-origin",
  headers: { "Content-Type": "application/json", "X-Requested-With": "gtfs-editor" },
  body: ${body === undefined ? "undefined" : JSON.stringify(JSON.stringify(body))},
}).then(async (r) => ({ status: r.status, body: await r.json().catch(() => null) }))`);
const activeDraftId = () => evaluate(`Object.entries(JSON.parse(localStorage.getItem("gtfs-editor-prefs"))).find(([k]) => k.startsWith("draft:" + document.getElementById("user-role").textContent.split(",")[0] + ":"))?.[1]`);

// the map, through the page's own module instance
const mapCall = (body) => evaluate(`import(${JSON.stringify(MAPJS)}).then((m) => { const map = m.getMap(); ${body} })`);
// A zoom animation still running (a route panel fitting its line) moves the map
// back to its own target when it ends, so wait for it and check the view stuck.
async function setView(lat, lon, zoom) {
  for (let attempt = 0; attempt < 4; attempt++) {
    await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => !m.getMap()._animatingZoom)`, "the map to settle", 5000);
    await mapCall(`map.setView([${lat}, ${lon}], ${zoom}, { animate: false }); return true;`);
    await sleep(700);
    const stuck = await mapCall(`const c = map.getCenter(); return Math.abs(c.lat - ${lat}) < 1e-5 && Math.abs(c.lng - ${lon}) < 1e-5 && map.getZoom() === ${zoom};`);
    if (stuck) return;
  }
  check(false, `the map stays at ${lat}, ${lon} zoom ${zoom}`);
}
const waitStopDrawn = (id) => waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().some((s) => s.stop_id === ${JSON.stringify(id)}))`, `stop ${id} on the map`);
async function mouseClick(x, y) {
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
  await sleep(500);
}
async function mouseDrag(x1, y1, x2, y2) {
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: x1, y: y1 });
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x: x1, y: y1, button: "left", buttons: 1, clickCount: 1 });
  for (let i = 1; i <= 8; i++) {
    await send("Input.dispatchMouseEvent", { type: "mouseMoved", x: x1 + ((x2 - x1) * i) / 8, y: y1 + ((y2 - y1) * i) / 8, button: "left", buttons: 1 });
    await sleep(30);
  }
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x: x2, y: y2, button: "left", buttons: 0, clickCount: 1 });
  await sleep(500);
}
async function clickMapAt(lat, lon) {
  const p = await mapCall(`const q = map.latLngToContainerPoint([${lat}, ${lon}]); const r = map.getContainer().getBoundingClientRect(); return { x: r.left + q.x, y: r.top + q.y };`);
  await mouseClick(p.x, p.y);
}
async function chooseNewDraft(title) {
  await waitFor(`document.querySelector("dialog #new-draft-title") !== null`, `draft chooser for "${title}"`);
  await type("#new-draft-title", title);
  await click("Start draft", "dialog");
  await sleep(500);
}
async function setFile(name, content) {
  await evaluate(`(() => { const input = document.getElementById("import-file"); const dt = new DataTransfer();
    dt.items.add(new File([${JSON.stringify(content)}], ${JSON.stringify(name)}, { type: "text/csv" }));
    input.files = dt.files; input.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`);
  await sleep(600);
}

async function actAs(email) {
  await evaluate(`fetch("/__dev/as", {method:"POST", credentials:"same-origin", headers:{"Content-Type":"application/json"}, body: JSON.stringify({email: ${JSON.stringify(email)}})}).then(() => true)`);
  await load(UI);
}
// The mock's TOTP check refuses a code that maps to the same 30s step as that
// account's last accepted one ("code_reused" - it mirrors the real server's replay
// guard). Signing in as the same email twice within one window, easy for a fast
// CDP run to do, would submit that same code again and hang; wait for a step this
// email hasn't used yet instead of just reading whatever /__dev/state has right now.
const lastCodeStepByEmail = new Map();
async function freshCode(email) {
  for (let i = 0; i < 40; i++) {
    const { code, step } = await evaluate(
      `fetch("/__dev/state").then(r => r.json()).then(s => ({ code: s.current_code, step: Math.floor(Date.now() / 30000) }))`,
    );
    if (code && step !== lastCodeStepByEmail.get(email)) {
      lastCodeStepByEmail.set(email, step);
      return code;
    }
    await sleep(750);
  }
  throw new Error(`timed out waiting for a fresh TOTP code for ${email}`);
}
async function signIn(email) {
  await actAs(email);
  await waitFor(`!!document.querySelector(".code-input")`, `code prompt for ${email}`);
  const code = await freshCode(email);
  await type(".code-input", code);
  await waitFor(`!document.getElementById("app").hidden`, `app after sign-in as ${email}`);
  await sleep(600);
}

function totp(secret) {
  return import("node:crypto").then(({ createHmac }) => {
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let bits = "";
    for (const c of secret.replace(/\s|=/g, "").toUpperCase()) bits += alphabet.indexOf(c).toString(2).padStart(5, "0");
    const key = Buffer.from(bits.match(/.{8}/g).map((b) => parseInt(b, 2)));
    const msg = Buffer.alloc(8);
    msg.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30000)));
    const mac = createHmac("sha1", key).update(msg).digest();
    const off = mac[mac.length - 1] & 15;
    return String((mac.readUInt32BE(off) & 0x7fffffff) % 1000000).padStart(6, "0");
  });
}

// ------------------------------------------------------------------ coordinates
// The detour a stop's routes take, computed here independently of the page: the
// median over routes of d(prev, p) + d(p, next) - d(prev, next) (docs section 8).
function metres(aLat, aLon, bLat, bLon) {
  const R = 6371000, rad = (x) => (x * Math.PI) / 180;
  const s = Math.sin(rad(bLat - aLat) / 2) ** 2 + Math.cos(rad(aLat)) * Math.cos(rad(bLat)) * Math.sin(rad(bLon - aLon) / 2) ** 2;
  return 2 * R * Math.asin(Math.sqrt(s));
}
function medianDetour(legs, p) {
  const v = legs.filter((l) => l.prev && l.next)
    .map((l) => metres(l.prev.lat, l.prev.lon, p.lat, p.lon) + metres(p.lat, p.lon, l.next.lat, l.next.lon) - metres(l.prev.lat, l.prev.lon, l.next.lat, l.next.lon))
    .sort((a, b) => a - b);
  if (!v.length) return null;
  return v.length % 2 ? v[v.length >> 1] : (v[v.length / 2 - 1] + v[v.length / 2]) / 2;
}
// the dashboard's way of writing a distance
const distance = (m) => (m >= 1000 ? `${(m / 1000).toFixed(m >= 10000 ? 0 : 1)} km` : `${Math.round(Math.max(0, m))} m`);
const legKinds = () => mapCall(`const k = {}; map.eachLayer((l) => { const kind = l.options && l.options.legKind; if (kind) k[kind] = (k[kind] || 0) + 1; }); return k;`);
const panelText = () => text("#panel");
// Toggling a checkbox re-renders the whole route list (a fresh DOM each time), so
// clicking a stale, pre-captured NodeList in one go only half-works: click one at
// a time, re-querying the live DOM before each click.
async function clickAllUnchecked(selector) {
  for (let i = 0; i < 50; i++) {
    const did = await evaluate(`(() => { const b = [...document.querySelectorAll(${JSON.stringify(selector)})].find((x) => !x.checked); if (!b) return false; b.click(); return true; })()`);
    if (!did) break;
  }
}
async function clickAllChecked(selector) {
  for (let i = 0; i < 50; i++) {
    const did = await evaluate(`(() => { const b = [...document.querySelectorAll(${JSON.stringify(selector)})].find((x) => x.checked); if (!b) return false; b.click(); return true; })()`);
    if (!did) break;
  }
}
// A group's checkbox (or its pre-checked suspect routes) may already be in the
// wanted state - e.g. once one split reloads the review, the next suspect group
// is pre-checked again - so only click it when that would actually change it.
async function ensureGroupChecked(groupExpr, want) {
  const already = await evaluate(`${groupExpr}.querySelector(".route-group-head input")?.checked`);
  if (already !== want) await evaluate(`${groupExpr}.querySelector(".route-group-head input").click(); true`);
}

// ====================================================================== round 4 (UX), 2026-09-17
// The flows of docs/gtfs-editor.md section 10, kept together: what the map shows
// (and remembers), stops sharing a point, stations listed once, undo and redo of
// what is not in a draft, the trail, the cleanup context (and an older server
// without it), the clean-up tool's verdict, candidates and merge on a coordinate
// review, and every kind of change shown as pending on the page it changes.
// `node dev/ui_smoke.mjs --round4` runs only these, as the admin, on a fresh mock.
const DRAFTSJS = `${UI}js/drafts.js`;
const TRAILJS = `${UI}js/trail.js`;
async function pressKey(key, { ctrl = false, shift = false, meta = false, commands = undefined } = {}) {
  const modifiers = (ctrl ? 2 : 0) | (meta ? 4 : 0) | (shift ? 8 : 0);
  const vk = key.toUpperCase().charCodeAt(0);
  const base = { modifiers, key: shift ? key.toUpperCase() : key, code: `Key${key.toUpperCase()}`, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
  await send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...base, ...(commands ? { commands } : {}) });
  await send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
  await sleep(350);
}
// the old page answers until the new one replaces it: give the reload time to start
const reloadPage = async () => { await send("Page.reload"); await sleep(1500); };
// Click a stop's marker; where several stops share its point the chooser opens
// (section 10.6), and the stop meant is picked from it.
async function clickStopOnMap(stop) {
  await clickMapAt(stop.lat, stop.lon);
  const pick = `.stack-picker .stack-item[data-stop="${stop.stop_id}"]`;
  if (await has(pick)) await clickSel(pick, `${stop.stop_id} in the chooser of stops sharing its point`);
}
const blurAll = () => evaluate(`(document.activeElement && document.activeElement.blur(), true)`);
const undoKey = async () => { await blurAll(); await pressKey("z", { ctrl: true }); };
const redoKey = async () => { await blurAll(); await pressKey("z", { ctrl: true, shift: true }); };
const lastToastText = () => text("#toasts .toast:last-child");
// markers of the stop layer: [{ids, lat, lon}], and how many of them are stations
const stopMarkers = () => mapCall(`const out = []; map.eachLayer((l) => { if (l.options && l.options.stopIds) { const q = l.getLatLng(); out.push({ ids: l.options.stopIds, lat: q.lat, lon: q.lng }); } }); return out;`);
const countLayers = (option, value) => mapCall(`let n = 0; map.eachLayer((l) => { if (l.options && l.options[${JSON.stringify(option)}] !== undefined && (${JSON.stringify(value)} === null || l.options[${JSON.stringify(option)}] === ${JSON.stringify(value)})) n++; }); return n;`);
const refreshUiDraft = () => evaluate(`import(${JSON.stringify(DRAFTSJS)}).then((m) => m.refreshDraft()).then(() => true)`);
const trailNow = () => evaluate(`import(${JSON.stringify(TRAILJS)}).then((m) => m.trail())`);
const reviewPin = () => mapCall(`let p = null; map.eachLayer((l) => { const html = l.options && l.options.icon && l.options.icon.options.html; if (String(html || "").includes("stop-pin") && l.getLatLng) { const q = l.getLatLng(); p = { lat: q.lat, lon: q.lng }; } }); return p;`);
const newestAudit = async () => ((await api("feeds/chennai_bus/audit?limit=1")).items[0] || {}).audit_id || 0;
const toggleLayer = async (key, on) => {
  const now = await evaluate(`document.getElementById("show-${key}").checked`);
  if (now !== on) await clickSel(`#show-${key}`, `${on ? "show" : "hide"} ${key} on the map`);
};

async function round4Flows() {
  // ---- what is set up: the mock's fixtures (mock_server.py seed_round4)
  const mergeList = (await api("feeds/chennai_bus/position-reviews?status=pending&auto_fix=merge&limit=5")).items;
  check(mergeList.length === 1, `set-up: one review where the tool says merge (${mergeList.length})`);
  const main = await api(`position-reviews/${mergeList[0].review_id}`);
  const S = main.stop_id;
  const candidates = main.evidence.same_name_candidates || [];
  check(candidates.length >= 1 && !!main.evidence.auto_fix, `set-up: that review has same-named candidates (${candidates.length}) and a verdict`);
  const platforms = (await api("feeds/chennai_bus/stops?station=stn_r4_shared&limit=10")).items;
  const station = await api("feeds/chennai_bus/stops/stn_r4_shared");
  check(platforms.length === 2 && station.location_type === 1, "set-up: a station whose two platforms share the reviewed stop's point");

  // start from a known place: no draft open, everything shown
  if (!(await text("#draft-chip")).includes("No draft open")) {
    await clickSel("#draft-chip", "the draft chip");
    await click("Stop using a draft", "dialog");
  }
  await go("#/");
  await waitFor(`!!document.getElementById("show-stops")`, "the Show control on the map");
  for (const k of ["stations", "routes", "stops"]) await toggleLayer(k, true);

  // ================================================================ 1. what the map shows
  const live = await api(`feeds/chennai_bus/stops/${S}`);
  await setView(live.lat, live.lon, 17);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().length > 0)`, "stops in the area");
  await sleep(400);
  const allMarkers = await stopMarkers();
  const isStationIds = new Set((await api(`feeds/chennai_bus/stops?station=true&bbox=${(live.lat - 0.02).toFixed(6)},${(live.lon - 0.02).toFixed(6)},${(live.lat + 0.02).toFixed(6)},${(live.lon + 0.02).toFixed(6)}&limit=500`)).items.map((x) => x.stop_id));
  check(allMarkers.length > 0 && allMarkers.some((m) => m.ids.includes("stn_r4_shared")), `stops and the station are drawn to begin with (${allMarkers.length} markers)`);
  check(await evaluate(`["stations", "routes", "stops"].every((k) => document.getElementById("show-" + k).checked) && document.querySelector(".layer-toggle").innerText.includes("Show")`), "the map has a Show control with Stations, Routes and Stops ticked");
  await toggleLayer("stops", false);
  const onlyStations = await stopMarkers();
  check(onlyStations.length > 0 && onlyStations.every((m) => m.ids.every((id) => isStationIds.has(id))), `with Stops off only stations are drawn (${onlyStations.length})`);
  check((await evaluate(`document.querySelectorAll(".stack-badge").length`)) === 0 || onlyStations.length > 0, "the count badges of hidden stops go with them");
  // hidden stays hidden as the map moves and new stops load
  await setView(live.lat + 0.004, live.lon + 0.004, 17);
  await sleep(900);
  await setView(live.lat, live.lon, 18);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().some((s) => s.stop_id === ${JSON.stringify(S)}))`, "the area loaded again after moving the map");
  await sleep(400);
  check((await stopMarkers()).every((m) => m.ids.every((id) => isStationIds.has(id))), "stops stay hidden after the map moved and loaded new stops");
  check(await evaluate(`[...document.querySelectorAll(".leaflet-tooltip.stop-label")].filter((e) => e.style.opacity !== "0").every((e) => e.textContent.includes("(station)"))`), "hidden stops have no name labels either");
  // it survives a reload
  await reloadPage();
  await waitFor(`!document.getElementById("app").hidden && !!document.getElementById("show-stops")`, "the app after a reload");
  check((await evaluate(`document.getElementById("show-stops").checked`)) === false && (await evaluate(`document.getElementById("show-stations").checked`)) === true, "the Show choices survive a reload");
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().length > 0)`, "stops loaded after the reload");
  await sleep(400);
  check((await stopMarkers()).every((m) => m.ids.every((id) => isStationIds.has(id))), "and stops are still hidden after it");
  // the open stop is still drawn, and the control says why it is alone
  await go(`#/stop/${S}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Routes stopping here")`, "a stop opened while stops are hidden");
  await sleep(500);
  const whileOpen = (await stopMarkers()).filter((m) => !m.ids.every((id) => isStationIds.has(id)));
  check(whileOpen.length === 1 && whileOpen[0].ids.includes(S), "with Stops off the open stop is the one stop still drawn");
  check((await text(".layer-note")).includes("Only the open stop is drawn"), "and the control says so");
  await toggleLayer("stations", false);
  check((await stopMarkers()).every((m) => m.ids.includes(S)), "with Stations off too, no station is drawn");
  // routes
  const viaRoute = live.routes[0].route_id;
  await go(`#/route/${viaRoute}`);
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 0`, "a route opened");
  check((await countLayers("routeLine", "route")) > 0, "the open route is drawn");
  await toggleLayer("routes", false);
  check((await countLayers("routeLine", "route")) === 0, "with Routes off the route line is not drawn");
  check((await text(".layer-note")).includes("The open route is not drawn"), "and the control says the open route is hidden");
  await go("#/");
  await go(`#/route/${viaRoute}`);
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 0`, "the route opened again");
  check((await countLayers("routeLine", "route")) === 0, "a route opened later stays hidden too");
  await toggleLayer("routes", true);
  check((await countLayers("routeLine", "route")) > 0, "ticking Routes draws it again");
  // storage that throws: the page still works, with everything shown
  const blocker = await send("Page.addScriptToEvaluateOnNewDocument", { source: `for (const k of ["getItem", "setItem", "removeItem"]) Storage.prototype[k] = () => { throw new Error("storage is blocked"); };` });
  await reloadPage();
  await waitFor(`!document.getElementById("app").hidden && !!document.getElementById("show-stops")`, "the app with storage blocked");
  check(await evaluate(`["stations", "routes", "stops"].every((k) => document.getElementById("show-" + k).checked)`), "with storage blocked the page works and shows everything");
  await toggleLayer("stops", false);
  check((await evaluate(`document.getElementById("show-stops").checked`)) === false, "and the Show control still works, without remembering");
  await send("Page.removeScriptToEvaluateOnNewDocument", { identifier: blocker.identifier });
  await reloadPage();
  await waitFor(`!document.getElementById("app").hidden && !!document.getElementById("show-stops")`, "the app with storage back");
  for (const k of ["stations", "routes", "stops"]) await toggleLayer(k, true);

  // ================================================================ 6. stops sharing a point
  await go("#/");
  await setView(live.lat, live.lon, 18);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().some((s) => s.stop_id === ${JSON.stringify(S)}))`, "the stops around the reviewed stop");
  await sleep(500);
  const stack = (await stopMarkers()).find((m) => m.ids.includes(S));
  check(!!stack && stack.ids.length >= 3, `the reviewed stop shares its point: one marker stands for ${stack ? stack.ids.length : 0} stops`);
  check(await evaluate(`[...document.querySelectorAll(".stack-badge")].some((b) => b.textContent === ${JSON.stringify(String(stack.ids.length))})`), "the marker carries a badge with how many stops it holds");
  check(await evaluate(`[...document.querySelectorAll(".leaflet-tooltip.stop-label")].filter((e) => e.style.opacity !== "0").length > 0`), "name labels are still drawn beside the badges at zoom 18");
  await clickMapAt(stack.lat, stack.lon);
  await waitFor(`document.querySelectorAll(".stack-picker .stack-item").length === ${stack.ids.length}`, "the chooser listing every stop at the point");
  const offered = await evaluate(`[...document.querySelectorAll(".stack-picker .stack-item")].map((b) => ({ id: b.dataset.stop, text: b.innerText }))`);
  check(offered.every((o) => o.text.includes(o.id)) && offered.some((o) => /\d+ routes?/.test(o.text)), "each is listed by name, id and route count");
  check((await evaluate("location.hash")) === "#/", "clicking a marker of several stops opens none of them by itself");
  const second = offered[1].id;
  await clickSel(`.stack-picker .stack-item[data-stop="${second}"]`, "the second stop in the chooser");
  await waitFor(`location.hash === ${JSON.stringify(`#/stop/${second}`)}`, "the second stop's panel");
  check((await evaluate("location.hash")) === `#/stop/${second}`, "the chooser opens the second stop, which a plain click could never reach");
  check(!(await has(".stack-picker")), "the chooser closes once a stop is chosen");
  // a stop alone on its point still opens straight away
  const lone = await api("feeds/chennai_bus/stops/f68ae67c0a");
  await setView(lone.lat, lone.lon, 18);
  await waitStopDrawn("f68ae67c0a");
  await sleep(300);
  const loneMarker = (await stopMarkers()).find((m) => m.ids.includes("f68ae67c0a"));
  if (loneMarker && loneMarker.ids.length === 1) {
    await clickMapAt(lone.lat, lone.lon);
    await waitFor(`location.hash === "#/stop/f68ae67c0a"`, "a single stop opening directly");
    check(!(await has(".stack-picker")), "a stop alone on its point opens directly, with no chooser");
  }
  // with stops hidden a station that shares the point is reached directly
  await toggleLayer("stops", false);
  await go("#/");
  await setView(station.lat, station.lon, 18);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().some((s) => s.stop_id === "stn_r4_shared"))`, "the station on the map");
  await sleep(400);
  const stationMarker = (await stopMarkers()).find((m) => m.ids.includes("stn_r4_shared"));
  check(!!stationMarker && stationMarker.ids.length === 1, "with Stops off the station's marker holds only the station");
  await clickMapAt(station.lat, station.lon);
  await waitFor(`location.hash === "#/stop/stn_r4_shared"`, "the station opened directly");
  check(!(await has(".stack-picker")), "the chooser only counts what is shown");
  await toggleLayer("stops", true);

  // ================================================================ 2. a station once, not each platform
  await go(`#/stop/${S}`);
  await waitFor(`!!document.querySelector(".nearby-stops .station-entry")`, "the nearby list with the station folded");
  const near = await text(".nearby-stops");
  check(near.includes(`2 of these are already platforms of ${station.name}`), "the nearby list says 2 of its entries are already platforms of the station");
  check(await has(`.nearby-stops .platforms-note a[href="#/stop/stn_r4_shared"]`), "and links the station");
  check((await evaluate(`document.querySelectorAll(".nearby-stops .station-entry").length`)) === 1, "the station is listed once");
  check(await evaluate(`${JSON.stringify(platforms.map((x) => x.stop_id))}.every((id) => ![...document.querySelectorAll(".nearby-stops > div > ul.list > li:not(.station-entry) > a")].some((a) => a.getAttribute("href") === "#/stop/" + id))`), "its platforms are not also listed as bare stops");
  check(await evaluate(`document.querySelectorAll(".nearby-stops .platforms-fold li").length === 2 && !document.querySelector(".nearby-stops .platforms-fold").open`), "the platforms are one click away, folded under the station");
  const foldedRows = live.nearby.filter((n) => n.parent_station === "stn_r4_shared" || n.stop_id === "stn_r4_shared").length;
  check((await evaluate(`document.querySelectorAll(".nearby-stops > div > ul.list > li").length`)) === live.nearby.length - foldedRows + 1, `the ${foldedRows} rows of the station and its platforms (of ${live.nearby.length} from the API) became one entry`);
  await shot("30-nearby-station-once");

  // ================================================================ 5. cleanup context
  await waitFor(`!!document.querySelector(".cleanup-context:not([hidden])")`, "the cleanup context of the stop");
  const ctx = await api(`feeds/chennai_bus/stops/${S}/context`);
  const ctxText = await text(".cleanup-context");
  check(ctxText.includes("Detour") && ctxText.includes(distance(ctx.detour_m)) && ctxText.includes("How much further"), `the context shows the detour (${distance(ctx.detour_m)}) and says in plain words what it means`);
  check(ctxText.includes("Coordinate reviews (1)") && (await has(`.cleanup-context a[href="#/coordinates/${main.review_id}"]`)), "it counts the coordinate reviews naming the stop and links into the Coordinates page");
  check(ctxText.includes(`Same-named stops nearby (${ctx.same_name.length})`), `it lists the same-named stops nearby (${ctx.same_name.length})`);
  if (ctx.same_name.length) {
    check((await countLayers("faintStop", null)) === ctx.same_name.length, "each same-named stop is a faint marker on the map");
    check(ctx.same_name.every((n) => ctxText.includes(distance(n.distance_m))) && /\d+ routes?/.test(ctxText), "with its distance and route count");
    if (ctx.same_name.some((n) => n.parent_station === "stn_r4_shared")) check(ctxText.includes("already") && (await has(`.cleanup-context .station-entry`)), "and a same-named platform is shown through its station, once");
  }
  // an older server has no such endpoint: the section is simply not there
  await evaluate(`fetch("/__dev/context", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ off: true }) }).then(() => true)`);
  const errorsBefore = consoleErrors.length;
  await go("#/");
  await go(`#/stop/${S}`);
  await waitFor(`document.querySelector(".cleanup-context")?.dataset.context === "unavailable"`, "the context endpoint answering 404");
  check(await evaluate(`document.querySelector(".cleanup-context").hidden && !document.querySelector("#toasts .toast.error")`), "when the endpoint is a 404 the section is hidden, without an error");
  check((await text("#panel")).includes("Routes stopping here") && (await has(".nearby-stops .station-entry")), "and the rest of the stop page is as usual");
  // the browser itself logs a failed fetch; that one line is expected here
  consoleErrors.splice(errorsBefore, consoleErrors.length - errorsBefore, ...consoleErrors.slice(errorsBefore).filter((e) => !/404/.test(String(e))));
  await evaluate(`fetch("/__dev/context", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ off: false }) }).then(() => true)`);
  // the route
  await go(`#/route/${viaRoute}`);
  await waitFor(`!!document.querySelector(".cleanup-context:not([hidden])")`, "the cleanup context of the route");
  const rctx = await api(`feeds/chennai_bus/routes/${viaRoute}/context`);
  const rText = await text(".cleanup-context");
  check(rText.includes(`Stops under coordinate review (${rctx.stops_with_reviews.length})`) && (await has(`.cleanup-context a[href="#/coordinates/${main.review_id}"]`)), "the route's context lists its stops under coordinate review, linked");
  check((await evaluate(`document.querySelectorAll(".ladder .chip.under-review").length`)) === rctx.stops_with_reviews.filter((f) => f.status === "pending").length, "rows whose stop has a pending review are marked in the stop list");
  check(rText.includes("Longest detours") && rctx.worst_detours.every((w) => rText.includes(distance(w.detour_m))), `and it lists the worst detours (${rctx.worst_detours.length})`);
  await shot("31-route-context");

  // ================================================================ 4. the trail
  await clickSel('[data-nav="map"]', "Map in the top bar");
  await waitFor(`location.hash === "#/"`, "the map page");
  check(await evaluate(`document.getElementById("trail").classList.contains("is-empty")`), "a top-level page has no trail to show");
  await go(`#/route/${viaRoute}`);
  await waitFor(`document.querySelectorAll(".ladder .row.clickable").length > 2`, "the route for the trail");
  const routeName = (await api(`feeds/chennai_bus/routes/${viaRoute}`)).short_name;
  await waitFor(`document.querySelectorAll("#trail .trail-list li").length === 2`, "a trail of two");
  check((await text("#trail")).includes("Map") && (await text("#trail .trail-here")) === routeName, `the trail reads Map › ${routeName}`);
  await evaluate(`document.querySelectorAll(".ladder .row.clickable")[1].click(); true`);
  await waitFor(`location.hash.startsWith("#/stop/") && document.getElementById("panel").innerText.includes("Routes stopping here")`, "a stop opened from the route");
  const trailStop = await evaluate("location.hash");
  await waitFor(`document.querySelectorAll("#trail .trail-list li").length === 3`, "a trail of three");
  const otherRoute = await evaluate(`[...document.querySelectorAll('#panel a[href^="#/route/"]')].map((a) => a.getAttribute("href")).find((x) => x !== ${JSON.stringify(`#/route/${viaRoute}`)}) || null`);
  if (otherRoute) {
    await go(otherRoute);
    await waitFor(`document.querySelectorAll("#trail .trail-list li").length === 4`, "a trail of four: route, stop, another route");
    check((await trailNow()).length === 4, "drilling down grows the trail: Map › route › stop › another route");
    check(!(await has("#panel a.crumb")), "with a trail to go back along, the panel has no second back link");
    // going to a place already in the trail goes BACK to it
    await go(`#/route/${viaRoute}`);
    await waitFor(`document.querySelectorAll("#trail .trail-list li").length === 2`, "the trail popped back to the first route");
    check((await trailNow()).length === 2, "opening a place already in the trail pops back to it instead of growing");
    await go(trailStop);
    await go(otherRoute);
    // a crumb pops to itself
    await evaluate(`[...document.querySelectorAll("#trail .trail-list a")].find((a) => a.getAttribute("href") === ${JSON.stringify(trailStop)}).click(); true`);
    await waitFor(`location.hash === ${JSON.stringify(trailStop)} && document.querySelectorAll("#trail .trail-list li").length === 3`, "the trail after clicking a crumb");
    check((await trailNow()).length === 3, "clicking a crumb pops the trail to it");
  }
  // it lasts for the session: a reload keeps it, and it is not in localStorage
  await reloadPage();
  await waitFor(`!document.getElementById("app").hidden && document.querySelectorAll("#trail .trail-list li").length === 3`, "the trail after a reload");
  check(await evaluate(`!!sessionStorage.getItem("gtfs-editor-trail") && !Object.keys(localStorage).some((k) => (localStorage.getItem(k) || "").includes(${JSON.stringify(trailStop)}))`), "the trail survives a reload in sessionStorage, and nothing of it is in localStorage");
  // the one Back control
  await clickSel("#trail .trail-back", "Back on the trail");
  await waitFor(`location.hash === ${JSON.stringify(`#/route/${viaRoute}`)}`, "one step back");
  check((await trailNow()).length === 2, "Back goes one step up the trail");
  // it is capped
  const many = (await api(`feeds/chennai_bus/routes/${viaRoute}`)).rows.filter((x) => x.stop_id).map((x) => x.stop_id);
  const distinct = [...new Set(many)].slice(0, 10);
  for (const id of distinct) await go(`#/stop/${id}`);
  const capped = await trailNow();
  check(distinct.length < 8 || (capped.length === 8 && capped[0].label === "Map"), `the trail is capped (${capped.length} of ${distinct.length + 2} places), keeping where it started`);
  // a search result starts a new trail
  await type("#search-input", live.stop_id);
  await waitFor(`document.querySelectorAll(".search-item").length > 0`, "search results for the trail");
  await evaluate(`document.querySelector(".search-item").dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true })); true`);
  await waitFor(`location.hash === ${JSON.stringify(`#/stop/${S}`)}`, "the searched stop");
  await sleep(400);
  check((await trailNow()).length === 1 && await evaluate(`document.getElementById("trail").classList.contains("is-empty")`), "a search result starts a new trail");
  check(await has("#panel a.crumb"), "and with no trail the panel offers its own way back");
  await type("#search-input", "");

  // ================================================================ 5b. the tool's verdict, candidates, merge
  await go("#/coordinates");
  await waitFor(`!!document.querySelector(".filter-chips:not([hidden])")`, "the tool's verdicts as filter chips");
  const sums = await api("feeds/chennai_bus/position-reviews/summary");
  const chipsText = await text(".filter-chips");
  check(sums.auto_fix && sums.auto_fix.merge === 1 && sums.auto_fix.move === 1 && sums.auto_fix.choose === 1, `the summary counts the tool's verdicts (${JSON.stringify(sums.auto_fix)})`);
  check([["Merge", "merge"], ["Move", "move"], ["Choose", "choose"], ["No fix", "none"]].every(([label, k]) => chipsText.replace(/\s+/g, "").includes(`${label.replace(" ", "")}${sums.auto_fix[k].toLocaleString("en-IN")}`)), `the chips show each verdict with its count (${chipsText.replace(/\s+/g, " ")})`);
  await clickSel('.filter-chips [data-auto-fix="merge"]', "the Merge chip");
  await waitFor(`document.querySelectorAll(".proposal-item").length === 1`, "only the reviews the tool would merge");
  check((await text(".proposal-item")).includes(main.stop_name) && (await text(".proposal-item")).includes("tool: merge"), "the Merge chip narrows the list to the review the tool would merge");
  await clickSel('.filter-chips [data-auto-fix="any"]', "the Anything chip");
  await waitFor(`document.querySelectorAll(".proposal-item").length > 1`, "every review again");

  await go(`#/coordinates/${main.review_id}`);
  await waitFor(`!!document.querySelector(".auto-fix")`, "the tool's verdict on the review");
  const banner = await text(".auto-fix");
  const fix = main.evidence.auto_fix;
  check(banner.includes("The tool suggests merging into") && banner.includes(fix.into_stop_id) && banner.includes(`detour ${distance(fix.detour_m)} → ${distance(fix.detour_m_after)}`), `the banner gives the verdict and the detour before and after (${distance(fix.detour_m)} → ${distance(fix.detour_m_after)})`);
  check(banner.includes(fix.reason.slice(0, 40)), "and the tool's reason");
  check((await evaluate(`document.querySelectorAll(".candidate").length`)) === candidates.length && (await countLayers("candidateStop", null)) === candidates.length, `the ${candidates.length} same-named candidates are a list and markers on the map`);
  check(candidates.every((c) => true) && (await text(".candidates")).includes(candidates[0].name) && (await text(".candidates")).includes(distance(candidates[0].distance_m)), "each candidate shows its name and distance");
  // the sharing list folds the station's platforms too
  await waitFor(`!!document.querySelector("#panel .platforms-note")`, "the stops sharing its point, with the station folded");
  check((await text("#panel .platforms-note")).includes(`already platforms of ${station.name}`) && (await has(`#panel .platforms-note a[href="#/stop/stn_r4_shared"]`)), "the stops that shared its point name the station once, linked, not its two platforms");
  await shot("32-review-candidates");

  // ================================================================ 3. undo and redo (nothing here is in a draft)
  const auditBefore = await newestAudit();
  const c0 = candidates[0];
  await clickSel(`#use-candidate-${c0.stop_id}`, "Use this position on the first candidate");
  await sleep(500);
  const onCandidate = await reviewPin();
  check(onCandidate && metres(onCandidate.lat, onCandidate.lon, c0.lat, c0.lon) < 0.5, "Use this position puts the pin on the candidate");
  check(await has("#panel .undo-row"), "Undo and Redo buttons sit where the pin is edited");
  await setView(live.lat, live.lon, 17);
  const aside = await mapCall(`const s = map.getSize(); const q = map.containerPointToLatLng([s.x / 2 + 90, s.y / 2 + 70]); return { lat: q.lat, lon: q.lng };`);
  await clickMapAt(aside.lat, aside.lon);
  const moved = await reviewPin();
  check(moved && metres(moved.lat, moved.lon, c0.lat, c0.lon) > 1, "a click on the map moves the pin");
  await undoKey();
  const undone = await reviewPin();
  check(undone && metres(undone.lat, undone.lon, c0.lat, c0.lon) < 0.5, "Ctrl+Z puts the pin back where it was");
  check((await lastToastText()).includes("Undid: moved the pin"), "a small hint says what was undone");
  await undoKey();
  check((await reviewPin()) === null && (await text("#panel")).includes("No new position yet"), "a second Ctrl+Z takes the pin off again");
  await redoKey();
  const redone = await reviewPin();
  check(redone && metres(redone.lat, redone.lon, c0.lat, c0.lon) < 0.5, "Ctrl+Shift+Z redoes the placement");
  await blurAll();
  await pressKey("y", { ctrl: true });
  const redone2 = await reviewPin();
  check(redone2 && metres(redone2.lat, redone2.lon, moved.lat, moved.lon) < 0.5 && (await lastToastText()).includes("Redid: moved the pin"), "Ctrl+Y redoes too");
  // inside a text field the browser's own undo is left alone
  await evaluate(`(() => { window.__prevented = null; document.addEventListener("keydown", (ev) => { if (ev.key.toLowerCase() === "z") setTimeout(() => { window.__prevented = ev.defaultPrevented; }, 0); }); const el = document.getElementById("review-note"); el.focus(); return true; })()`);
  await send("Input.insertText", { text: "typed note" });
  await pressKey("z", { meta: true, commands: ["undo"] });
  check((await evaluate("window.__prevented")) === false, "with the caret in a text field the shortcut is not taken from the browser");
  check((await evaluate(`document.getElementById("review-note").value`)) === "", "and the browser's own undo of the typing works");
  const stillThere = await reviewPin();
  check(stillThere && metres(stillThere.lat, stillThere.lon, moved.lat, moved.lon) < 0.5, "the pin did not move for an undo meant for the text");
  // checkboxes: a review of a stop with several routes
  let multi = null;
  for (const rv of (await api("feeds/chennai_bus/position-reviews?status=pending&limit=500")).items.reverse()) {
    if (rv.review_id === main.review_id) continue;
    const d = await api(`position-reviews/${rv.review_id}`);
    if (d.stop && !d.stop.deleted && !d.problems.length && new Set(d.routes.map((l) => l.route_id)).size >= 2) { multi = d; break; }
  }
  check(!!multi, "set-up: a review of a stop with several routes");
  // another entity: what could be undone on the last review is gone
  await evaluate(`location.hash = ${JSON.stringify(`#/coordinates/${multi.review_id}`)}; true`);
  await waitFor(`document.querySelector("dialog")?.innerText.includes("Leave without saving")`, "the leave question for the unsaved pin");
  await click("Leave without saving", "dialog");
  await waitFor(`document.querySelectorAll(".split-route input").length >= 2`, "the review with route checkboxes");
  await undoKey();
  check((await lastToastText()).includes("Nothing to undo") && (await reviewPin()) === null, "opening another review starts with nothing to undo");
  const before = await evaluate(`[...document.querySelectorAll(".split-route input:checked")].length`);
  await evaluate(`(() => { const b = [...document.querySelectorAll(".split-route input")].find((x) => !x.checked); b.click(); return true; })()`);
  await sleep(300);
  check((await evaluate(`[...document.querySelectorAll(".split-route input:checked")].length`)) === before + 1, "a route is ticked");
  await undoKey();
  check((await evaluate(`[...document.querySelectorAll(".split-route input:checked")].length`)) === before, "Ctrl+Z unticks it");
  await clickSel('#panel .undo-row [data-undo="redo"]', "the Redo button");
  check((await evaluate(`[...document.querySelectorAll(".split-route input:checked")].length`)) === before + 1, "the Redo button ticks it again");
  await clickSel('#panel .undo-row [data-undo="undo"]', "the Undo button");
  check((await newestAudit()) === auditBefore, "none of that touched the database: no audit entry, no draft");

  // the stop list editor: row operations, in a draft of its own
  await go(`#/route/${viaRoute}`);
  await waitFor(`document.body.innerText.includes("Edit stop list")`, "route actions for the editor");
  await click("Edit stop list");
  await chooseNewDraft("Smoke: round 4");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length > 3`, "the stop list editor");
  const r4Draft = await activeDraftId();
  const auditInEditor = await newestAudit();
  check(await has("#panel .undo-row"), "the stop list editor has Undo and Redo buttons");
  const rowCount = await evaluate(`document.querySelectorAll(".ladder.editing .row").length`);
  // a row is told apart by its stop id: the two kerbs of a road often share a name
  const rowSig = (i) => evaluate(`(document.querySelector("#row-${i} .row-sub .meta") || document.querySelector("#row-${i} .name")).innerText`);
  const thirdName = await text("#row-2 .name");
  const thirdSig = await rowSig(2);
  const selRow = await evaluate(`[...document.querySelectorAll(".ladder.editing .row")].findIndex((r, k) => k > 2 && !!r.querySelector("select"))`);
  await clickSel('button[aria-label="Remove stop 3"]', "remove stop 3");
  check((await evaluate(`document.querySelectorAll(".ladder.editing .row").length`)) === rowCount - 1, "a row is removed");
  await undoKey();
  check((await evaluate(`document.querySelectorAll(".ladder.editing .row").length`)) === rowCount && (await rowSig(2)) === thirdSig, "Ctrl+Z brings the removed row back where it was");
  check((await lastToastText()).startsWith("Undid: removed"), "the hint names the row operation");
  // two rows that differ (a jump stop may repeat the stop before it)
  let mv = 0;
  while (mv < rowCount - 2 && (await rowSig(mv)) === (await rowSig(mv + 1))) mv++;
  const upperSig = await rowSig(mv), lowerSig = await rowSig(mv + 1);
  await clickSel(`#row-${mv} button[title="Move down"]`, `move stop ${mv + 1} down`);
  check((await rowSig(mv)) === lowerSig && (await rowSig(mv + 1)) === upperSig, "a row is moved");
  const typeBefore = await evaluate(`document.querySelector("#row-${selRow} select").value`);
  await choose(`#row-${selRow} select`, typeBefore === "JUMP STOP" ? "INTERMEDIATE STOP" : "JUMP STOP");
  check((await evaluate(`document.querySelector("#row-${selRow} select").value`)) !== typeBefore, "a stop's type is changed in its select");
  await undoKey();
  check((await evaluate(`document.querySelector("#row-${selRow} select").value`)) === typeBefore, "Ctrl+Z puts a select back");
  await undoKey();
  check((await rowSig(mv)) === upperSig && (await rowSig(mv + 1)) === lowerSig, "and then the move before it");
  await redoKey();
  check((await rowSig(mv)) === lowerSig, "Ctrl+Shift+Z redoes the move");
  await undoKey();
  const draftNow = await api(`change-sets/${r4Draft}`);
  check(draftNow.changes.length === 0 && (await newestAudit()) === auditInEditor, "undo and redo in the editor changed nothing in the draft or the database");
  // saved into the draft: the steps are gone, and Ctrl+Z does not take the change out
  await clickSel('button[aria-label="Remove stop 3"]', "remove stop 3 for real");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("Show what is live now")`, "the route with the draft applied");
  await undoKey();
  check((await api(`change-sets/${r4Draft}`)).changes.length === 1, "Ctrl+Z never removes a change that is already in a draft");

  // a stop's pin
  const pinStop = await api("feeds/chennai_bus/stops/b9f23c05b1");
  await go("#/stop/b9f23c05b1");
  await waitFor(`document.body.innerText.includes("Edit stop")`, "the stop whose pin is dragged");
  await click("Edit stop");
  await waitFor(`!!document.getElementById("stop-lat")`, "the stop editor for the pin");
  await sleep(700);
  const latBefore = await evaluate(`document.getElementById("stop-lat").value`);
  const pinBox2 = await evaluate(`(() => { const r = document.querySelector(".stop-pin").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
  await mouseDrag(pinBox2.x, pinBox2.y, pinBox2.x + 50, pinBox2.y + 40);
  const latDragged = await evaluate(`document.getElementById("stop-lat").value`);
  check(latDragged !== latBefore, "dragging the stop's pin changes its latitude");
  await undoKey();
  check((await evaluate(`document.getElementById("stop-lat").value`)) === latBefore && (await text("#panel")).includes("Not moved"), "Ctrl+Z puts the dragged pin back exactly");
  await redoKey();
  check((await evaluate(`document.getElementById("stop-lat").value`)) === latDragged, "Ctrl+Shift+Z drags it out again");
  // a field, once left, is one step
  await type("#stop-name", "R4 RENAMED STOP");
  await undoKey();
  check((await evaluate(`document.getElementById("stop-name").value`)) === pinStop.name, "a whole field commit is undone in one step");
  await redoKey();
  check((await evaluate(`document.getElementById("stop-name").value`)) === "R4 RENAMED STOP", "and redone");

  // ================================================================ 7. pending in the draft, on the pages it changes
  // stop/update: moved and renamed
  await click("Add to draft", ".sticky-actions");
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the stop shown as the draft leaves it");
  const moveChange = (await api(`change-sets/${r4Draft}`)).changes.find((c) => c.entity === "stop" && c.entity_key === "b9f23c05b1");
  const stopPage = await text("#panel");
  check(stopPage.includes(`Pending in draft “Smoke: round 4”, not live`) && stopPage.includes("changes this stop"), "a stop changed in the draft is labelled pending, not live");
  check((await text("#panel h1")).includes("R4 RENAMED STOP") && (await text("#panel h1 .live-value")).includes(pinStop.name), "it shows the drafted name with the live one beside it");
  check(stopPage.includes(moveChange.after.lat.toFixed(6)) && (await text("#panel .facts .live-value")).includes(pinStop.lat.toFixed(6)), "and the drafted position with the live one");
  check((await countLayers("draftedKind", "drafted")) === 1 && (await countLayers("draftedKind", "live")) === 1, "the map marks the drafted position and a ghost on the live one");
  await waitStopDrawn("b9f23c05b1");
  await sleep(400);
  const areaMarker = (await stopMarkers()).find((m) => m.ids.includes("b9f23c05b1"));
  check(areaMarker && metres(areaMarker.lat, areaMarker.lon, moveChange.after.lat, moveChange.after.lon) < 0.5 && (await countLayers("ghostOf", "b9f23c05b1")) === 1, "the map's area stops draw it at the drafted position, with its live place ghosted");
  await shot("33-stop-pending");
  // the context now knows the draft and the history
  await waitFor(`document.querySelector(".cleanup-context")?.innerText.includes("Open drafts touching it (1)")`, "the open draft in the stop's context");
  check((await text(".cleanup-context")).includes("Smoke: round 4") && (await text(".cleanup-context")).includes("Added a change"), "the context lists the open draft, and history in the words the History page uses");

  // stop/create: a stop that is not live is viewable
  const made = await apiSend("POST", `change-sets/${r4Draft}/changes`, { entity: "stop", op: "create", entity_key: "r4_new_stop", after: { stop_id: "r4_new_stop", name: "R4 New Stop", lat: pinStop.lat + 0.001, lon: pinStop.lon + 0.001 } });
  check(made.status === 201, "set-up: a stop created in the draft");
  await refreshUiDraft();
  await go("#/stop/r4_new_stop");
  await waitFor(`document.getElementById("panel").innerText.includes("R4 New Stop")`, "the stop that exists only in the draft");
  check((await text("#panel")).includes("Pending in draft “Smoke: round 4”, not live") && (await text("#panel")).includes("Creates it"), "a stop created in the draft can be opened, labelled pending");
  // stop/delete
  const doomed = distinct.find((id) => id !== S && id !== "b9f23c05b1");
  check((await apiSend("POST", `change-sets/${r4Draft}/changes`, { entity: "stop", op: "delete", entity_key: doomed, after: null })).status === 201, "set-up: a stop deleted in the draft");
  await refreshUiDraft();
  await go(`#/stop/${doomed}`);
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the stop the draft deletes");
  check((await text("#panel .notice.pending")).includes("deletes this stop") && (await text("#panel .notice.pending")).includes("still live"), "a stop deleted in the draft says so, and that it is still live");
  // station/create, seen from the station and from a member
  const pair = (await api(`feeds/chennai_bus/stops/${doomed}`)).nearby.filter((n) => n.location_type === 0 && !n.parent_station).slice(0, 2);
  const members = pair.length === 2 ? pair : [await api("feeds/chennai_bus/stops/CHNS03131"), await api("feeds/chennai_bus/stops/f68ae67c0a")];
  const stationMade = await apiSend("POST", `change-sets/${r4Draft}/changes`, { entity: "station", op: "create", entity_key: "stn_r4_new",
    after: { station_id: "stn_r4_new", name: "R4 New Station", lat: members[0].lat, lon: members[0].lon, members: members.map((m, i) => ({ stop_id: m.stop_id, platform_code: `Bay ${i + 1}` })) } });
  if (stationMade.status === 201) {
    await refreshUiDraft();
    await go("#/stop/stn_r4_new");
    await waitFor(`document.getElementById("panel").innerText.includes("R4 New Station")`, "the station that exists only in the draft");
    check((await text("#panel")).includes("Pending in draft") && (await text("#panel")).includes("Stops it will group (2)"), "a station created in the draft can be opened, with the stops it will group");
    await go(`#/stop/${members[0].stop_id}`);
    await waitFor(`!!document.querySelector("#panel .notice.pending")`, "a member of the new station");
    check((await text("#panel")).includes("Joins station") && (await text("#panel")).includes("Bay 1"), "its member says it joins the station, with its platform label");
  } else {
    check(false, `set-up: a station created in the draft (${JSON.stringify(stationMade.body)})`);
  }
  // station/update, then station/delete
  const renamed = await apiSend("POST", `change-sets/${r4Draft}/changes`, { entity: "station", op: "update", entity_key: "stn_r4_shared", after: { name: "R4 Renamed Station" } });
  check(renamed.status === 201, "set-up: a station renamed in the draft");
  await refreshUiDraft();
  await go("#/stop/stn_r4_shared");
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the renamed station");
  check((await text("#panel h1")).includes("R4 Renamed Station") && (await text("#panel h1 .live-value")).includes(station.name), "a station renamed in the draft shows the new name and the live one");
  await apiSend("DELETE", `change-sets/${r4Draft}/changes/${renamed.body.change_id}`);
  check((await apiSend("POST", `change-sets/${r4Draft}/changes`, { entity: "station", op: "delete", entity_key: "stn_r4_shared", after: null })).status === 201, "set-up: the station dissolved in the draft");
  await refreshUiDraft();
  await go("#/");
  await go("#/stop/stn_r4_shared");
  await waitFor(`document.querySelector("#panel .notice.pending")?.innerText.includes("dissolves this station")`, "the dissolved station");
  check(true, "a station dissolved in the draft says so");
  await go(`#/stop/${platforms[0].stop_id}`);
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "a platform of the dissolved station");
  check((await text("#panel")).includes("Leaves station") && (await text("#panel")).includes("Leaves its station in the draft"), "and its platform says it leaves the station");
  // route/update and route_stops/replace
  const liveRoute = await api(`feeds/chennai_bus/routes/${viaRoute}`);
  check((await apiSend("POST", `change-sets/${r4Draft}/changes`, { entity: "route", op: "update", entity_key: viaRoute, after: { long_name: "R4 RENAMED ROUTE", color: "#1F5FBF" }, base_row_version: liveRoute.row_version })).status === 201, "set-up: the route renamed in the draft");
  await refreshUiDraft();
  await go("#/");
  await go(`#/route/${viaRoute}`);
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the route as the draft leaves it");
  const routePage = await text("#panel");
  check(routePage.includes("Pending in draft “Smoke: round 4”, not live") && routePage.includes("R4 RENAMED ROUTE") && (await text("#panel .title-block .live-value")).includes(liveRoute.long_name || "none"), "a route renamed in the draft shows the drafted name with the live one");
  check(routePage.includes("Taken off the route in the draft") && routePage.includes(thirdName), "the stop the draft takes off the route is named");
  check((await evaluate(`document.querySelectorAll(".ladder .row:not(.marker)").length`)) === liveRoute.rows.filter((x) => x.stop_type !== "ROUTE CORRECTION").length - 1, "the stop list is the drafted one");
  check((await countLayers("routeLine", "proposal")) > 0, "the live route stays on the map, dashed, under the drafted one");
  await click("Show what is live now");
  await waitFor(`document.getElementById("panel").innerText.includes("Show it with my draft")`, "the live route");
  check((await evaluate(`document.querySelectorAll(".ladder .row:not(.marker)").length`)) === liveRoute.rows.filter((x) => x.stop_type !== "ROUTE CORRECTION").length && !(await text("#panel h1, #panel .title-block")).includes("R4 RENAMED ROUTE"), "Show what is live now shows the route without the draft");
  await shot("34-route-pending");

  // the merge from the review, into the same draft
  await go(`#/coordinates/${main.review_id}`);
  await waitFor(`!!document.getElementById("merge-candidate-${c0.stop_id}")`, "the review with its merge buttons");
  // refusals first, straight at the API: a station is not a stop to merge into
  const refused = await apiSend("POST", `position-reviews/${main.review_id}/merge`, { change_set_id: r4Draft, into_stop_id: "stn_r4_shared" });
  check(refused.status === 400 && refused.body.error.code === "review_has_problems" && refused.body.error.details.problems.length > 0, "the merge endpoint refuses a station with review_has_problems and the problems");
  await clickSel(`#merge-candidate-${c0.stop_id}`, "Merge into this stop on the first candidate");
  await waitFor(`!!document.getElementById("merge-confirm")`, "the merge question");
  check((await text("dialog")).includes(c0.stop_id) && (await text("dialog")).includes("switches to"), "the question says which stop stays and what happens to the routes");
  await clickSel("#merge-confirm", "Add merge to draft");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes("Merged")`, "a toast confirms the merge");
  await waitFor(`document.querySelector(".draft-actions")?.innerText.includes("Merges this stop into")`, "the merge among the review's actions");
  const merged = await api(`position-reviews/${main.review_id}`);
  check(merged.status === "approved" && merged.draft_actions.length === 1 && merged.draft_actions[0].kind === "merge" && merged.draft_actions[0].into_stop_id === c0.stop_id, "the review is approved with a merge action in the draft");
  const mergeChange = (await api(`change-sets/${r4Draft}`)).changes.find((c) => c.entity === "stop" && c.op === "merge");
  check(!!mergeChange && mergeChange.entity_key === S && mergeChange.after.into_stop_id === c0.stop_id && mergeChange.after.position_review_id === main.review_id, "the draft has the stop/merge change, tied to the review");
  check(await evaluate(`document.getElementById("review-move").disabled && document.getElementById("why-move").innerText.includes("merged into")`), "Move is off: the stop is merged away in this draft");
  check((await legKinds()).drafted > 0, "the routes are drawn to the stop that stays");
  const again = await apiSend("POST", `position-reviews/${main.review_id}/merge`, { change_set_id: r4Draft, into_stop_id: c0.stop_id });
  check(again.status === 409 && again.body.error.code === "draft_conflict", "a second merge of the same review is a 409 draft_conflict");
  await shot("35-review-merged");
  // both stops say what the draft does to them
  await go(`#/stop/${S}`);
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the stop that is merged away");
  check((await text("#panel .notice.pending")).includes(`merges this stop into ${c0.stop_id}`), "the stop being merged says it will be merged into the other");
  await go(`#/stop/${c0.stop_id}`);
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the stop that stays");
  check((await text("#panel .notice.pending")).includes("is merged into this stop") && (await text("#panel .notice.pending")).includes(S), "and the stop that stays says which stop is merged into it");
  // taking the change out of the draft, the normal way, takes the overlay with it
  await apiSend("DELETE", `change-sets/${r4Draft}/changes/${mergeChange.change_id}`);
  check((await api(`position-reviews/${main.review_id}`)).status === "pending", "removing the merge from the draft returns the review to pending");
  await refreshUiDraft();
  await go("#/");
  await go(`#/stop/${c0.stop_id}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Routes stopping here")`, "the stop that stays, after the merge was removed");
  check(!(await has("#panel .notice.pending")), "once the change is out of the draft, the page stops showing it");
  // another active draft: the overlay follows it
  await clickSel("#draft-chip", "the draft chip");
  await click("Stop using a draft", "dialog");
  await go("#/");
  await go("#/stop/b9f23c05b1");
  await waitFor(`document.getElementById("panel").innerText.includes("Routes stopping here")`, "the moved stop with no draft open");
  check(!(await has("#panel .notice.pending")) && (await text("#panel h1")).trim() === pinStop.name, "with no draft open the stop is shown as it is live");
  await sleep(500);
  const liveMarker = (await stopMarkers()).find((m) => m.ids.includes("b9f23c05b1"));
  check(liveMarker && metres(liveMarker.lat, liveMarker.lon, pinStop.lat, pinStop.lon) < 0.5, "and the map draws it where it is live");
  // leave nothing behind for a later flow
  await apiSend("POST", `change-sets/${r4Draft}/discard`, {});
}

// ---- only round 4
if (process.argv.includes("--round4")) {
  try {
    await connect();
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await load(UI);
    await signIn("admin@nammayatri.in");
    await round4Flows();
    check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
  } catch (e) {
    failures.push(e.message);
    console.log(`FAIL ${e.message}`);
  } finally {
    try { ws?.close(); } catch { /* ignore */ }
    chrome.kill();
    await sleep(800);
    if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  }
  console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
  process.exit(failures.length ? 1 : 0);
}
// ====================================================================== end of round 4 (UX)

// ====================================================================== round 5 (stop details, station links)
// docs/gtfs-editor.md section 11, against the mock's seed_round5 fixtures: the
// thin lines from a station to its platforms (the "Station links" tick, remembered;
// the open station's standing out; a platform beyond the loaded area read once,
// never again on a pan; a drafted move and a drafted station change redrawn as
// pending), the platform label offered to a stop in no station, the description
// on the stop page, the station editor, New stop and New station (counter, pending
// in the draft, hover titles, the marker's tooltip), and the fourth import kind.
// `node dev/ui_smoke.mjs --round5` runs only these, as the admin, on a fresh mock.
const stationLinks = (sid = null) => mapCall(`const out = []; map.eachLayer((l) => { const o = l.options || {}; if (o.stationLink && (${JSON.stringify(sid)} === null || o.stationLink === ${JSON.stringify(sid)})) {
  const pts = l.getLatLngs(); out.push({ station: o.stationLink, platform: o.platform, pending: !!o.pendingLink, strong: !!o.strongLink, interactive: !!o.interactive, pane: o.renderer && o.renderer.options.pane,
    from: { lat: pts[0].lat, lon: pts[0].lng }, to: { lat: pts[1].lat, lon: pts[1].lng } }); } }); return out;`);
// every request the page makes from now on, across reloads (the browser's own
// resource list fills up with map tiles)
const asked = [];
async function watchRequests() {
  if (asked.watching) return;
  asked.watching = true;
  await send("Network.enable");
  ws.addEventListener("message", (ev) => { const msg = JSON.parse(ev.data); if (msg.method === "Network.requestWillBeSent") asked.push(msg.params.request.url); });
}
const requestsFor = (part) => asked.filter((u) => u.includes(part)).length;

async function round5Flows() {
  const fx = (await evaluate(`fetch("/__dev/state").then((r) => r.json())`)).round5;
  if (!check(!!fx && !!fx.far_station, "set-up: the mock's round 5 fixtures")) return;
  const station = await api(`feeds/chennai_bus/stops/${fx.station}`);
  const [pa, pb] = fx.platforms.map((id) => station.children.find((c) => c.stop_id === id));
  check(station.location_type === 1 && station.platform_count === 2 && !!pa && !!pb && metres(pa.lat, pa.lon, pb.lat, pb.lon) > 30,
    "set-up: a station whose two platforms stand apart, and says how many it has");
  check(!!pa.description && !!pa.platform_code, "set-up: one platform has a description and a label");
  const solo = await api(`feeds/chennai_bus/stops/${fx.solo}`);
  check(!solo.parent_station && !solo.platform_code && !solo.description, "set-up: a stop in no station, with no label and no description");

  if (!(await text("#draft-chip")).includes("No draft open")) {
    await clickSel("#draft-chip", "the draft chip");
    await click("Stop using a draft", "dialog");
  }
  await watchRequests();
  await go("#/");
  await waitFor(`!!document.getElementById("show-links")`, "the Station links tick in the Show control");
  for (const k of ["stations", "routes", "stops", "links"]) await toggleLayer(k, true);

  // ================================================================ 1. station links
  await setView(station.lat, station.lon, 17);
  await waitStopDrawn(fx.station);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => { let n = 0; m.getMap().eachLayer((l) => { if (l.options && l.options.stationLink === ${JSON.stringify(fx.station)}) n++; }); return n === 2; })`, "a line from the station to each platform");
  let links = await stationLinks(fx.station);
  check(links.length === 2 && fx.platforms.every((id) => links.some((l) => l.platform === id)), "the station is tied to each of its two platforms");
  check(links.every((l) => metres(l.from.lat, l.from.lon, station.lat, station.lon) < 0.5) && links.some((l) => metres(l.to.lat, l.to.lon, pa.lat, pa.lon) < 0.5),
    "each line runs from the station point to the platform");
  check(links.every((l) => !l.interactive && l.pane === "lines"), "the lines take no clicks and sit in the pane under the markers");
  check(links.every((l) => !l.strong && !l.pending), "with nothing open every line is faint, and none is pending");
  await shot("r5-01-station-links");
  // clicking a platform through its line still opens the platform
  await clickStopOnMap(pa);
  await waitFor(`location.hash.includes(${JSON.stringify(pa.stop_id)})`, "a platform still opens by a click on its marker");
  await waitFor(`document.querySelector("#panel h1")?.innerText.includes(${JSON.stringify(pa.name)})`, "the platform's panel");
  await sleep(300);
  links = await stationLinks();
  check(links.filter((l) => l.station === fx.station).every((l) => l.strong) && links.filter((l) => l.station !== fx.station).every((l) => !l.strong),
    "with a platform open its station's lines stand out and the others stay faint");
  await go(`#/stop/${fx.station}`);
  await waitFor(`document.querySelector("#panel h1")?.innerText.includes(${JSON.stringify(station.name)})`, "the station's panel");
  await sleep(300);
  check((await stationLinks(fx.station)).every((l) => l.strong), "with the station open its lines stand out");
  await shot("r5-02-station-open");
  // hover titles in a list of stops, and the marker's tooltip, carry the label and the description
  check(await evaluate(`[...document.querySelectorAll("#panel .list-item")].some((li) => (li.title || "").includes(${JSON.stringify(pa.description)}) && li.title.includes(${JSON.stringify(pa.platform_code)}))`),
    "a platform's row in the station's list says its label and description on hover");
  const tip = await mapCall(`let t = null; map.eachLayer((l) => { if (l.options && l.options.stopIds && l.options.stopIds.includes(${JSON.stringify(pa.stop_id)})) t = l.getTooltip().getContent().textContent; }); return t;`);
  check(String(tip).includes(pa.platform_code) && String(tip).includes(pa.description), `the marker's tooltip carries the label and the description (${tip})`);

  // the tick: off, remembered over a reload, on again; Stations off takes the lines too
  await go("#/");
  await toggleLayer("links", false);
  check((await stationLinks()).length === 0, "Station links off: no line is drawn");
  check(await evaluate(`JSON.parse(localStorage.getItem("gtfs-editor-prefs")).mapLayers.links === false`), "the choice is kept with the other layers");
  await reloadPage();
  await waitFor(`!!document.getElementById("show-links")`, "the Show control after a reload");
  await waitStopDrawn(fx.station);
  await sleep(400);
  check((await evaluate(`document.getElementById("show-links").checked`)) === false && (await stationLinks()).length === 0, "after a reload the lines are still off");
  await toggleLayer("links", true);
  check((await stationLinks(fx.station)).length === 2, "ticked again, the lines are back");
  await toggleLayer("stations", false);
  check((await stationLinks()).length === 0, "with Stations off there are no station lines");
  await toggleLayer("stations", true);
  await toggleLayer("stops", false);
  check((await stationLinks()).length === 0 && (await text(".layer-note")).includes("Station links"), "with Stops off the lines have nowhere to run, and the control says so");
  await toggleLayer("stops", true);
  await setView(station.lat, station.lon, 14);
  await sleep(500);
  check((await stationLinks()).length === 0, "below zoom 15 no line is drawn");

  // a platform beyond the loaded area: read once in a session (the page was
  // reloaded above, so twice by now), then never again however the map moves
  const farNear = await api(`feeds/chennai_bus/stops/${fx.far_near}`);
  const farPlatform = await api(`feeds/chennai_bus/stops/${fx.far_platform}`);
  await setView(farNear.lat, farNear.lon, 17);
  await waitStopDrawn(fx.far_station);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => { let n = 0; m.getMap().eachLayer((l) => { if (l.options && l.options.stationLink === ${JSON.stringify(fx.far_station)}) n++; }); return n === 2; })`, "the line to the platform outside the map area");
  links = await stationLinks(fx.far_station);
  check(links.some((l) => l.platform === fx.far_platform && metres(l.to.lat, l.to.lon, farPlatform.lat, farPlatform.lon) < 0.5), "a platform outside the loaded area is tied to its station too");
  for (const [dLat, dLon] of [[0.0006, 0], [0, 0.0006], [-0.0006, -0.0006], [0, 0]]) {
    await setView(farNear.lat + dLat, farNear.lon + dLon, 17);
    await sleep(300);
  }
  check(requestsFor(`station=${fx.far_station}`) === 2 && requestsFor(`/stops/${fx.far_station}`) === 0 && requestsFor(`station=${fx.station}`) === 0,
    `its platforms were asked for once a session, not on every move of the map; a station with every platform in view is never asked about (${requestsFor(`station=${fx.far_station}`)}, ${requestsFor(`/stops/${fx.far_station}`)}, ${requestsFor(`station=${fx.station}`)})`);
  // seen from the far platform, the station is what lies outside: read once as well
  await setView(farPlatform.lat, farPlatform.lon, 17);
  await waitStopDrawn(fx.far_platform);
  await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => { let n = 0; m.getMap().eachLayer((l) => { if (l.options && l.options.platform === ${JSON.stringify(fx.far_platform)}) n++; }); return n === 1; })`, "the line from a platform to its station outside the map area");
  await setView(farPlatform.lat + 0.0005, farPlatform.lon, 17);
  await sleep(400);
  await setView(farPlatform.lat, farPlatform.lon, 17);
  await sleep(400);
  check(requestsFor(`/stops/${fx.far_station}`) === 1 && requestsFor(`station=${fx.far_station}`) === 2,
    `and the station was asked for once (${requestsFor(`/stops/${fx.far_station}`)}, ${requestsFor(`station=${fx.far_station}`)})`);

  // ================================================================ 2. the lines follow the draft
  await clickSel("#draft-chip", "the draft chip");
  await chooseNewDraft("Smoke: round 5");
  const r5Draft = await activeDraftId();
  await go("#/");
  await setView(station.lat, station.lon, 17);
  await waitStopDrawn(fx.station);
  const movedTo = { lat: pa.lat + 0.0003, lon: pa.lon + 0.0003 };
  const move = await apiSend("POST", `change-sets/${r5Draft}/changes`, { entity: "stop", op: "update", entity_key: pa.stop_id, after: movedTo, base_row_version: pa.row_version });
  check(move.status === 201, "set-up: the draft moves a platform");
  await refreshUiDraft();
  await sleep(500);
  links = await stationLinks(fx.station);
  const movedLink = links.find((l) => l.platform === pa.stop_id);
  check(!!movedLink && movedLink.pending && metres(movedLink.to.lat, movedLink.to.lon, movedTo.lat, movedTo.lon) < 0.5, "a platform the draft moves is tied where the draft puts it, as pending");
  check(links.some((l) => l.platform === pb.stop_id && !l.pending), "the other platform's line is as it was");
  const join = await apiSend("POST", `change-sets/${r5Draft}/changes`, { entity: "station", op: "update", entity_key: fx.station, base_row_version: station.row_version,
    after: { members: [{ stop_id: pa.stop_id }, { stop_id: pb.stop_id }, { stop_id: solo.stop_id, platform_code: "Towards the smoke test" }] } });
  check(join.status === 201, "set-up: the draft takes a third stop into the station");
  await refreshUiDraft();
  await setView((station.lat + solo.lat) / 2, (station.lon + solo.lon) / 2, 17);
  await waitStopDrawn(solo.stop_id);
  await sleep(500);
  links = await stationLinks(fx.station);
  check(links.length === 3 && links.some((l) => l.platform === solo.stop_id && l.pending), "a stop the draft takes into the station is tied to it, as pending");
  const joinTip = await mapCall(`let t = null; map.eachLayer((l) => { if (l.options && l.options.stopIds && l.options.stopIds.includes(${JSON.stringify(solo.stop_id)})) t = l.getTooltip().getContent().textContent; }); return t;`);
  check(String(joinTip).includes("joins station") && String(joinTip).includes("not live"), `its marker says it joins the station in the draft (${joinTip})`);
  await shot("r5-03-pending-links");
  for (const c of [join, move]) await apiSend("DELETE", `change-sets/${r5Draft}/changes/${c.body.change_id}`);
  await refreshUiDraft();
  await sleep(500);
  links = await stationLinks(fx.station);
  check(links.length === 2 && links.every((l) => !l.pending), "with the changes taken out of the draft the lines are live again");

  // ================================================================ 3. a label and a description on a stop in no station
  await go(`#/stop/${solo.stop_id}`);
  await waitFor(`document.querySelector("#panel h1")?.innerText.includes(${JSON.stringify(solo.name)})`, "the stop in no station");
  check(!(await has("#panel .platform-line")) && !(await has("#panel .stop-description")), "a stop with neither shows neither");
  await click("Edit stop", "#panel");
  await waitFor(`!!document.getElementById("stop-description")`, "the stop editor with a description");
  check((await evaluate(`document.getElementById("stop-platform").placeholder`)) === "Towards <next stop>", "the platform label is offered to a stop in no station, with its placeholder");
  check((await text("#stop-platform-help")).includes("platform or direction"), "and says it is what passengers see as the platform or direction");
  check(await evaluate(`document.getElementById("stop-description").tagName === "TEXTAREA" && document.getElementById("stop-description").maxLength === 500`), "the description is a textarea of at most 500 characters");
  check((await text("#panel .char-count")).includes("0 of 500"), "with a counter");
  await type("#stop-platform", "Towards Smoke Nagar");
  await type("#stop-description", "Outside the smoke test, by the tea stall");
  check((await text("#panel .char-count")).includes("40 of 500"), "the counter follows what is typed");
  await click("Add to draft", "#panel");
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the stop page with its pending change");
  check((await text("#panel .platform-line")).includes("Towards Smoke Nagar") && (await text("#panel .stop-description")).includes("by the tea stall"), "the header shows the drafted label and description");
  check((await text("#panel .title-block")).includes("live:") && (await text("#panel .notice.pending")).includes("platform label, description"), "as pending in the draft, beside what is live");
  const soloChange = (await api(`change-sets/${r5Draft}`)).changes.find((c) => c.entity_key === solo.stop_id);
  check(!!soloChange && soloChange.after.platform_code === "Towards Smoke Nagar" && soloChange.after.description === "Outside the smoke test, by the tea stall" && Object.keys(soloChange.after).length === 2,
    "the draft holds one stop update with exactly the label and the description");
  await shot("r5-04-stop-details-pending");
  // editing again starts from the draft, and clearing the description is a change too
  await click("Edit stop", "#panel");
  await waitFor(`document.getElementById("stop-description")?.value.includes("tea stall")`, "the editor opens with the drafted description");
  await type("#stop-description", "");
  await click("Update in draft", "#panel");
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the stop page again");
  check(!(await has("#panel .stop-description")) && (await text("#panel .platform-line")).includes("Towards Smoke Nagar"), "a cleared description is gone from the page, the label stays");

  // ================================================================ 4. a station's description
  await go(`#/stop/${fx.station}`);
  await waitFor(`document.querySelector("#panel h1")?.innerText.includes(${JSON.stringify(station.name)})`, "the station page");
  await click("Edit station", "#panel");
  await waitFor(`!!document.getElementById("station-description")`, "the station editor with a description");
  check((await text("#panel .char-count")).includes("0 of 500"), "the station's description has its counter");
  check(await evaluate(`[...document.querySelectorAll('#panel input[id^="platform-"]')].every((el) => el.placeholder === "Towards <next stop>")`), "each platform's label has the placeholder");
  await type("#station-description", "Stops on both sides of the junction");
  await click("Add to draft", "#panel");
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "the station page with its pending change");
  check((await text("#panel .stop-description")).includes("both sides of the junction") && (await text("#panel .notice.pending")).includes("description"), "the station page shows the drafted description as pending");
  const stationChange = (await api(`change-sets/${r5Draft}`)).changes.find((c) => c.entity === "station" && c.entity_key === fx.station);
  check(!!stationChange && stationChange.after.description === "Stops on both sides of the junction", "the station change carries the description");
  await go(`#/drafts/${r5Draft}`);
  await waitFor(`document.querySelectorAll(".change").length === 2`, "the draft's two changes");
  check((await text("#page")).includes("Description") && (await text("#page")).includes("both sides of the junction"), "the draft's diff shows the description");

  // ================================================================ 5. New stop and New station
  await go("#/new/stop");
  await waitFor(`!!document.getElementById("new-stop-description")`, "New stop with a description");
  check((await evaluate(`document.getElementById("new-stop-platform").placeholder`)) === "Towards <next stop>" && (await text("#panel")).includes("platform or direction"), "New stop offers the label with its placeholder and help");
  await setView(solo.lat + 0.004, solo.lon + 0.004, 17);
  await clickMapAt(solo.lat + 0.004, solo.lon + 0.004);
  await type("#new-stop-name", "Smoke round five");
  await type("#new-stop-platform", "Towards Smoke Depot");
  await type("#new-stop-description", "A new kerb for the smoke test");
  check((await text("#panel .char-count")).includes("29 of 500"), "New stop counts the description");
  await click("Add stop to draft", "#panel");
  await waitFor(`document.getElementById("panel").innerText.includes("New stop added")`, "the new stop in the draft");
  check((await text("#panel")).includes("A new kerb for the smoke test") && (await text("#panel")).includes("Towards Smoke Depot"), "what was added says its label and description");
  const created = (await api(`change-sets/${r5Draft}`)).changes.find((c) => c.op === "create" && c.entity === "stop");
  check(!!created && created.after.description === "A new kerb for the smoke test" && created.after.platform_code === "Towards Smoke Depot", "the stop create carries both");
  await go(`#/stop/${created.entity_key}`);
  await waitFor(`document.getElementById("panel").innerText.includes("A new kerb for the smoke test")`, "the draft-only stop's page with its description");
  await go("#/new/station");
  await waitFor(`!!document.getElementById("station-description")`, "New station with a description");
  check((await text("#panel .char-count")).includes("0 of 500"), "New station has the description and its counter");
  await click("Cancel", "#panel");

  // ================================================================ 6. importing stop details
  await go("#/import");
  await waitFor(`!!document.getElementById("kind-stop_updates")`, "the fourth kind of import");
  check((await text('label[for="kind-stop_updates"]')).includes("Stop details (platform label, description)"), "it is called Stop details (platform label, description)");
  await clickSel("#kind-stop_updates", "Stop details");
  const template = await evaluate(`fetch(document.querySelector('a[download="stop_updates-template.csv"]').href).then((r) => r.text())`);
  check(template.includes("action,stop_id,platform_code,description,name"), "its template downloads with its header row");
  // read in the browser: a column that is not one of the kind's
  await setFile("details.csv", "action,stop_id,platform_code,descripton\nupdate,x,y,z\n");
  await waitFor(`document.getElementById("page").innerText.includes("Did you mean")`, "a misspelt column is caught in the browser");
  // the server's errors, row by row
  const csvCell = (v) => `"${String(v).replace(/"/g, '""')}"`;
  await setFile("details.csv", ["action,stop_id,platform_code,description,name",
    `update,${pb.stop_id},,Opposite the smoke test bakery,`,
    "update,no_such_stop_r5,Towards nowhere,,",
    `update,${fx.station},Towards X,,`,
    `update,${pb.stop_id},Towards twice,,`,
    `update,${fx.far_near},,,`].join("\n") + "\n");
  await waitFor(`document.getElementById("page").innerText.includes("have errors")`, "the dry run with errors", 15000);
  const table = await text(".result-table");
  check(table.includes("no stop no_such_stop_r5") && table.includes("is a station") && table.includes("rows 1, 4") && table.includes("gives no platform_code"), "the table says what is wrong on each row");
  check(await evaluate(`[...document.querySelectorAll(".actionbar button")].find((b) => b.textContent.startsWith("Add"))?.disabled === true`), "adding is off while rows have errors");
  // the fixed file: one row already true, one for a stop the draft already updates
  const fixed = ["action,stop_id,platform_code,description,name",
    `update,${pa.stop_id},${csvCell(pa.platform_code)},,`,
    `update,${pb.stop_id},,Opposite the smoke test bakery,`,
    `update,${solo.stop_id},Towards Smoke Colony,,`,
    `update,${fx.far_station},,A station described by a file,`].join("\n") + "\n";
  await setFile("details-fixed.csv", fixed);
  await waitFor(`document.getElementById("page").innerText.includes("can be added")`, "the dry run of the fixed file", 15000);
  const chips = (await text(".summary-chips")).replace(/\n/g, " ");
  check(chips.includes("4 rows") && chips.includes("0 errors") && chips.includes("1 unchanged, not added") && chips.includes("3 changes to add"), `the summary counts the unchanged row apart (${chips})`);
  const fixedTable = await text(".result-table");
  check(fixedTable.includes("changes nothing") && fixedTable.includes("already updates"), "the unchanged row and the stop already in the draft are warnings that say so");
  check(fixedTable.includes(pb.name) && await has(".result-table td.stop-now"), "each row names the stop it changes");
  check(await waitFor(`!!document.querySelector(".import-map.leaflet-container, .import-map .leaflet-container")`, "the map of the stops in the file"), "the stops of the file are on a map");
  await shot("r5-05-import-stop-details");
  await click("Add 3 changes to draft");
  await click("Add 3 to draft", "dialog");
  await waitFor(`document.getElementById("page").innerText.includes("Added 3 changes")`, "the stop details added to the draft", 15000);
  const afterImport = await api(`change-sets/${r5Draft}`);
  const imported = afterImport.changes.find((c) => c.entity_key === pb.stop_id);
  check(afterImport.changes.length === 6 && !!imported && imported.op === "update" && imported.after.description === "Opposite the smoke test bakery" && Object.keys(imported.after).length === 1 && imported.base_row_version === pb.row_version,
    "the draft gained one update per row, each with exactly the cells given and the stop's version");
  // the same file again adds nothing
  await clickSel("#kind-stop_updates", "Stop details");
  await setFile("details-fixed.csv", fixed);
  await waitFor(`document.getElementById("page").innerText.includes("Nothing to add")`, "the same file again", 15000);
  check((await text(".summary-chips")).includes("4 unchanged") && await evaluate(`[...document.querySelectorAll(".actionbar button")].find((b) => b.textContent.startsWith("Add"))?.disabled === true`),
    "every row is unchanged once the draft applies, and there is nothing to add");
  // pending on the stop's page like any other change
  await go(`#/stop/${pb.stop_id}`);
  await waitFor(`!!document.querySelector("#panel .notice.pending")`, "an imported change on the stop's page");
  check((await text("#panel .stop-description")).includes("smoke test bakery"), "the imported description shows as pending in the draft");

  // released: live on the stop, the station and the map
  await apiSend("POST", `change-sets/${r5Draft}/submit`);
  const approved = await apiSend("POST", `change-sets/${r5Draft}/approve`, { self_approve: true });
  const committed = await apiSend("POST", `change-sets/${r5Draft}/commit`);
  check(approved.status === 200 && committed.status === 200, `the draft commits (${approved.status}, ${committed.status})`);
  const liveB = await api(`feeds/chennai_bus/stops/${pb.stop_id}`);
  const liveSolo = await api(`feeds/chennai_bus/stops/${solo.stop_id}`);
  const liveStation = await api(`feeds/chennai_bus/stops/${fx.station}`);
  check(liveB.description === "Opposite the smoke test bakery" && liveSolo.platform_code === "Towards Smoke Colony" && !liveSolo.parent_station && !liveSolo.description
    && liveStation.description === "Stops on both sides of the junction", "every detail is live: the description, the label on a stop in no station, the station's description");
  await reloadPage();
  await waitFor(`!document.getElementById("app").hidden`, "the app after the commit");
  await go(`#/stop/${solo.stop_id}`);
  await waitFor(`document.querySelector("#panel .platform-line")?.innerText.includes("Towards Smoke Colony")`, "the committed label on the stop's page");
  check(!(await has("#panel .notice.pending")), "and nothing is pending any more");

  // ================================================================ 7. the same lines on the review pages
  // a suggested station's page: the live stations around it keep their lines
  const proposal = (await api("feeds/chennai_bus/station-proposals?status=pending,superseded&limit=1")).items[0];
  if (check(!!proposal, "set-up: a suggested station to open")) {
    await go(`#/stations/${proposal.proposal_id}`);
    await waitFor(`document.getElementById("panel").innerText.includes(${JSON.stringify(proposal.name)})`, "the suggested station's page");
    await setView(station.lat, station.lon, 17);
    await waitStopDrawn(fx.station);
    await sleep(400);
    links = await stationLinks(fx.station);
    check(links.length === 2 && links.every((l) => !l.strong), "on Stations to review the live stations in view are tied to their platforms");
  }
  // a coordinate review of a platform: its station's lines stand out
  const reviews = (await api("feeds/chennai_bus/position-reviews?status=pending&limit=50")).items;
  let reviewed = null;
  for (const rv of reviews) {
    const st = await api(`feeds/chennai_bus/stops/${rv.stop_id}`);
    if (st && st.location_type === 0 && !st.parent_station && !st.deleted) { reviewed = { rv, st }; break; }
  }
  if (check(!!reviewed, "set-up: a stop under review, in no station")) {
    const made = await apiSend("POST", "feeds/chennai_bus/change-sets", { title: "Smoke: round 5, a platform under review" });
    const setId = made.body.change_set_id;
    const now = await api(`feeds/chennai_bus/stops/${fx.station}`);
    await apiSend("POST", `change-sets/${setId}/changes`, { entity: "station", op: "update", entity_key: fx.station, base_row_version: now.row_version,
      after: { members: [...now.children.map((c) => ({ stop_id: c.stop_id })), { stop_id: reviewed.st.stop_id }] } });
    await apiSend("POST", `change-sets/${setId}/submit`);
    await apiSend("POST", `change-sets/${setId}/approve`, { self_approve: true });
    const done = await apiSend("POST", `change-sets/${setId}/commit`);
    check(done.status === 200, `set-up: the reviewed stop is now a platform of the station (${done.status})`);
    await reloadPage();
    await waitFor(`!document.getElementById("app").hidden`, "the app after that commit");
    await go(`#/coordinates/${reviewed.rv.review_id}`);
    await waitFor(`document.getElementById("panel").innerText.includes(${JSON.stringify(reviewed.st.name)})`, "the coordinate review of the platform");
    // a review fits its routes' legs, which can be far below the zoom stops are drawn at
    await setView(reviewed.st.lat, reviewed.st.lon, 17);
    await waitStopDrawn(reviewed.st.stop_id);
    await waitFor(`import(${JSON.stringify(MAPJS)}).then((m) => { let n = 0; m.getMap().eachLayer((l) => { if (l.options && l.options.stationLink === ${JSON.stringify(fx.station)} && l.options.strongLink) n++; }); return n >= 1; })`,
      "the reviewed platform's station lines on the Coordinates page", 15000);
    links = await stationLinks(fx.station);
    check(links.some((l) => l.platform === reviewed.st.stop_id) && links.every((l) => l.strong), "on Coordinates to review the reviewed platform's station is tied to its platforms, standing out");
    await shot("r5-06-review-links");
  }
}

// ====================================================================== delivery
// The Delivery page (docs/gtfs-editor.md section 12): what each pod is serving,
// and the webhooks GIMS calls when an edit goes live everywhere.
// `node dev/ui_smoke.mjs --delivery` runs only these, as the admin.
async function deliveryFlows() {
  // The page only has a live version for the pods to follow when the feed is served
  // from the DB, and the feed-config flow above leaves chennai_bus on "preprocessed".
  // State this flow's precondition instead of depending on the order flows run in.
  await evaluate(`fetch("/__dev/feed-source", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ gtfs_id: "chennai_bus", data_source: "db" }) }).then(() => true)`);
  await go("#/delivery");
  await waitFor(`document.querySelector("#page h1")?.innerText === "Delivery"`, "the Delivery page opens");
  check((await text("#page")).includes("Pods"), "it shows the pods section");
  check(/All 2 pods are serving version/.test(await text("#page")), "it says every pod is serving the committed version");
  check((await text("#page")).includes("gtfs-inmemory-data-server-7a93ed-abc12"), "each pod is named with what it serves");
  check((await text("#page")).includes("No webhook yet."), "with nothing configured it says so");
  check((await text("#page")).includes("Where GIMS may send"), "the policy block is on the page");
  check((await text("#page")).includes("going by this deployment's own configuration"),
    "with nothing saved it says the deployment's configuration is what GIMS goes by");
  await shot("wh-01-empty");

  // the Nandi section (docs section 12.6): the button says why it cannot be pressed
  const rel = (body) => evaluate(`fetch("/__dev/feed-release", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: ${JSON.stringify(JSON.stringify({ gtfs_id: "chennai_bus", ...body }))} }).then(() => true)`);
  await rel({ version: 7, released_version: 5 });
  await waitFor(`document.querySelector("#page").innerText.includes("Released to Nandi v5")`, "the Nandi section shows what Nandi has");
  check((await text("#page")).includes("2 committed versions are not on Nandi yet"), "it counts what is waiting");
  check((await text("#page")).includes("Add a webhook for release_requested"), "with no release webhook it says what to add");
  check(await evaluate(`[...document.querySelectorAll("#page button")].find((b) => b.innerText === "Release to Nandi")?.disabled === true`), "and the button is disabled");
  await shot("wh-01b-nandi-no-hook");

  // the policy is editable here: a host that is not a host name is refused,
  // and a host the deployment never allowed can be added (docs section 12.5)
  await click("Change these settings", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the policy form opens");
  await type("dialog textarea", "https://jenkins.example.com/job/rebuild");
  await click("Save", "dialog");
  await waitFor(`document.querySelector("dialog .notice.error")?.hidden === false`, "a URL in the host list is refused");
  check((await text("dialog .notice.error")).includes("scheme"), "the message says what is wrong with it");
  await type("dialog textarea", "jenkins.mock.invalid\njenkins.c2.sso.internal.svc.movingtech.net\n  JENKINS.MOCK.INVALID  ");
  await click("Save", "dialog");
  await waitFor(`!document.querySelector("dialog")`, "the policy saves and the form closes");
  await waitFor(`document.querySelector("#page").innerText.includes("saved here")`, "the block says the settings now come from here");
  check((await text("#page")).includes("jenkins.mock.invalid, jenkins.c2.sso.internal.svc.movingtech.net"),
    "a host the deployment never allowed is now allowed, and the repeat was dropped");
  await shot("wh-02-policy");

  // a release webhook turns the button on; pressing it asks Jenkins once
  await click("Add a webhook", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the webhook form opens for the release hook");
  await type("dialog input[type=text]:nth-of-type(1)", "nandi release");
  await evaluate(`(() => { const s = document.querySelector("dialog select"); s.value = "release_requested"; s.dispatchEvent(new Event("change", { bubbles: true })); })()`);
  await evaluate(`(() => { const el = document.querySelectorAll("dialog input[type=text]")[1]; el.value = "https://jenkins.mock.invalid/job/ny-internal/job/nandi/job/main/buildWithParameters?token=\${JENKINS_TOKEN}&releaseFromEditor=true"; el.dispatchEvent(new Event("input", { bubbles: true })); })()`);
  await click("Add", "dialog");
  await waitFor(`!document.querySelector("dialog")`, "the release webhook is added");
  check((await text("#page")).includes("An approver asks for a Nandi release"), "the event is named in words");
  check(await evaluate(`![...document.querySelectorAll("#page button")].some((b) => b.innerText === "Test")`), "a release webhook has no Test button: it would start a real release");
  await waitFor(`[...document.querySelectorAll("#page button")].find((b) => b.innerText === "Release to Nandi")?.disabled === false`, "the button is enabled once a release webhook exists");
  await click("Release to Nandi", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "pressing it asks to confirm");
  await click("Release", "dialog");
  await waitFor(`document.querySelector("#page").innerText.includes("A release is already on its way")`, "once asked, it says a release is on its way");
  check((await text("#page")).includes("by admin@nammayatri.in"), "and who asked");
  await shot("wh-02b-nandi-requested");
  await rel({ released_version: 7 });
  await waitFor(`document.querySelector("#page").innerText.includes("Nandi already has v7")`, "once Jenkins marks it, the section says Nandi has it", 12000);
  await shot("wh-02c-nandi-released");
  await click("Delete", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the delete confirm opens for the release hook");
  await click("Delete", "dialog");
  await waitFor(`document.querySelector("#page").innerText.includes("No webhook yet.")`, "the release hook is gone");

  // a URL the deployment does not allow is refused, in the form
  await click("Add a webhook", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the webhook form opens");
  await type("dialog input[type=text]:nth-of-type(1)", "frontline rebuild");
  const urlBox = `document.querySelectorAll("dialog input[type=text]")[1]`;
  await evaluate(`(() => { const el = ${urlBox}; el.value = "https://evil.example.com/build"; el.dispatchEvent(new Event("input", { bubbles: true })); })()`);
  await click("Add", "dialog");
  await waitFor(`document.querySelector("dialog .notice.error")?.hidden === false`, "a host outside the allow-list is refused in the form");
  check((await text("dialog .notice.error")).includes("allow-list"), "the message says why");
  await shot("wh-03-host-refused");

  // the real one
  await evaluate(`(() => { const el = ${urlBox}; el.value = "https://jenkins.mock.invalid/job/rebuild/buildWithParameters?token=\${JENKINS_TOKEN}"; el.dispatchEvent(new Event("input", { bubbles: true })); })()`);
  await click("Add", "dialog");
  await waitFor(`!document.querySelector("dialog")`, "the webhook is added and the form closes");
  await waitFor(`document.querySelector("#page").innerText.includes("frontline rebuild")`, "it appears in the table");
  check((await text("#page")).includes("Every pod is serving the edit"), "the event is named in words");
  check((await text("#page")).includes("${JENKINS_TOKEN}"), "the stored URL keeps the placeholder, not a secret");
  await shot("wh-04-added");

  // a test call is recorded in the history
  await click("Test", "#page");
  await waitFor(`document.querySelector("#page").innerText.includes("Delivered")`, "a test call shows in Recent calls", 12000);
  check((await text("#page")).includes("test"), "the history marks it as a test");
  await shot("wh-05-delivered");

  // turning it off, and removing it
  await click("Edit", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the edit form opens");
  await clickSel("dialog input[type=checkbox]", "the On switch");
  await click("Save", "dialog");
  await waitFor(`!document.querySelector("dialog")`, "the change saves");
  await waitFor(`document.querySelector("#page").innerText.includes("Off")`, "the webhook reads as off");
  await click("Delete", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the delete confirm opens");
  await click("Delete", "dialog");
  await waitFor(`document.querySelector("#page").innerText.includes("No webhook yet.")`, "it is gone");
  await shot("wh-06-deleted");

  // the switch itself: off means nothing is sent and the pods stop reporting
  await click("Change these settings", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the policy form opens again");
  await clickSel("dialog input[type=checkbox]", "the Send webhooks switch");
  await click("Save", "dialog");
  await waitFor(`document.querySelector("#page").innerText.includes("Webhooks are off")`, "turning them off is shown on the page");
  check((await text("#page")).includes("Turn them on under"), "and the webhooks section says where to turn them back on");
  await shot("wh-07-off");
  await click("Change these settings", "#page");
  await waitFor(`!!document.querySelector("dialog")`, "the policy form opens once more");
  await clickSel("dialog input[type=checkbox]", "the Send webhooks switch");
  await click("Save", "dialog");
  await waitFor(`document.querySelector("#page").innerText.includes("Webhooks are on")`, "and back on again");

  // the history names the actions in words, not codes
  await go("#/audit");
  await waitFor(`document.querySelector("#page h1")?.innerText === "History"`, "the History page opens");
  const history = await text("#page");
  check(history.includes("Added a webhook"), "the history says a webhook was added");
  check(history.includes("Sent a test webhook call"), "the history says a test was sent");
  check(history.includes("Deleted a webhook"), "the history says a webhook was deleted");
  check(history.includes("Changed where GIMS may send webhooks"), "the history says the policy was changed");
  check(history.includes("Asked for a Nandi release"), "the history says a release was asked for");
}

// ---- only delivery
if (process.argv.includes("--delivery")) {
  try {
    await connect();
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await load(UI);
    await signIn("admin@nammayatri.in");
    await deliveryFlows();
    check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
  } catch (e) {
    failures.push(e.message);
    console.log(`FAIL ${e.message}`);
  } finally {
    try { ws?.close(); } catch { /* ignore */ }
    chrome.kill();
    await sleep(800);
    if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  }
  console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
  process.exit(failures.length ? 1 : 0);
}
// ====================================================================== end of delivery


// ====================================================================== merging two stations
// docs section 5, "Merging duplicate stations". The fixture (mock
// seed_station_merge) is two stations a few hundred metres apart, each over two
// platforms, named alike but not identically.
async function stationMergeFlows() {
  const fx = (await evaluate(`fetch("/__dev/state").then((r) => r.json())`)).station_merge;
  if (!check(!!fx && !!fx.keep && !!fx.gone, "the mock has a station-merge fixture")) return;

  // ---- the action is on a station, and only on a station
  await go(`#/stop/${fx.keep}`);
  await waitFor(`document.body.innerText.includes("Merge into another station")`, "a station offers the merge action");
  check(!(await text("#panel")).includes("Merge with a duplicate"), "a station is not offered the two-stop merge");
  await go(`#/stop/${fx.keep_platforms[0]}`);
  await waitFor(`document.body.innerText.includes("Merge with a duplicate")`, "a platform offers the two-stop merge");
  check(!(await text("#panel")).includes("Merge into another station"), "a platform is not offered the station merge");

  // ---- choose the other station
  await go(`#/stop/${fx.keep}`);
  await waitFor(`document.body.innerText.includes("Merge into another station")`, "back on the station");
  await click("Merge into another station");
  await waitFor(`document.getElementById("panel").innerText.includes("Stations close by")`, "the station picker");
  check((await text("#panel")).includes("Merge stations only when they are one place entered more than once"), "says when stations should be merged");
  check((await text("#panel")).includes(fx.gone), "the other station is listed close by");
  await shot("sm-01-choose");

  // ---- compare
  await go(`#/station-merge/${fx.keep}?with=${fx.gone}`);
  await waitFor(`!!document.querySelector("table.compare")`, "the two stations side by side");
  const compare = await text("#panel");
  check(compare.includes("Which station id should stay?") && compare.includes("Suggested: has more platforms"), "asks which station id stays and suggests one");
  check(/\d+ platforms? \(.*routes?\) moves? from \w+ to \w+/.test(compare), "says how many platforms move, and their routes, before adding");
  check(compare.includes("No route changes"), "says plainly that no route changes");
  check(compare.includes("Platforms") && compare.includes("Routes through them"), "compares platform and route counts");
  for (const p of fx.gone_platforms) check(compare.includes(p), `the moving platform ${p} is listed`);
  await shot("sm-02-compare");

  // ---- the other id may be kept instead
  const keepOther = await evaluate(`[...document.querySelectorAll('input[name="keep-station-id"]')].find((r) => !r.checked).id`);
  await clickSel(`#${keepOther}`, "the other station id");
  check((await text(".sticky-actions")).includes(keepOther.replace("keep-station-", "")), "the add button names the station id that stays");
  // put it back: the fixture's "gone" station is the one to retire
  await clickSel(`#keep-station-${fx.keep}`, `keeping ${fx.keep}`);

  // ---- add it to a draft
  await click("Add merge to draft", ".sticky-actions");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("Add this station merge")`, "the station merge confirmation");
  const confirm = await text("dialog");
  check(confirm.includes(`Station id ${fx.keep} stays`), "the confirm says which id survives");
  check(/\d+ platforms? moves? to it/.test(confirm), "the confirm says how many platforms move");
  check(confirm.includes(`Station id ${fx.gone} is removed`), "the confirm says which id is removed");
  await shot("sm-03-confirm");
  await click("Add merge to draft", "dialog");
  await chooseNewDraft("Smoke: station merges");
  await waitFor(`document.getElementById("panel").innerText.includes("Station merge added to your draft")`, "the station merge in the draft");
  const added = await text("#panel");
  for (const p of fx.gone_platforms) check(added.includes(p), `the draft page lists the moved platform ${p}`);

  // ---- it renders in the draft view
  const smDraft = await activeDraftId();
  await go(`#/drafts/${smDraft}`);
  await waitFor(`document.querySelectorAll(".change").length === 1`, "the station merge draft page");
  const draftText = await text("#page");
  check(/merged into/.test(draftText) && /platforms? moved/.test(draftText), "the draft shows the station merge and how many platforms move");
  check(draftText.includes("station merge"), "the draft counts it as a station merge");
  check(draftText.includes("a route calls at a platform, never at a station"), "the draft says why no route changes");
  await shot("sm-04-draft");

  // ---- and in the pending overlay, on both stations and on a moving platform
  await go(`#/stop/${fx.gone}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Pending in draft")`, "the retired station shows the merge as pending");
  check((await text("#panel")).includes(`merges this station into ${fx.keep}`), "the retired station says where it goes");
  await go(`#/stop/${fx.keep}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Pending in draft")`, "the surviving station shows the merge as pending");
  check(/is merged into this station, which takes its \d+ platforms?/.test(await text("#panel")), "the surviving station says what it takes");
  await go(`#/stop/${fx.gone_platforms[0]}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Pending in draft")`, "a moving platform shows the merge as pending");
  check((await text("#panel")).includes(`Moves from station ${fx.gone} to`), "the platform says which station it moves to");
  await shot("sm-05-pending");
}

// ====================================================================== map lines
// docs section 17: the two ways to suggest a route's map line - through the
// stops, and from the buses' GPS - and, when either fails, why. The mock's
// /__dev/map-line switch makes each suggestion answer as the server can.
async function mapLineFlows() {
  const setMode = (m) => evaluate(`fetch("/__dev/map-line", { method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" }, body: ${JSON.stringify(JSON.stringify(m))} }).then((r) => r.json())`);
  const routes = (await api("feeds/chennai_bus/routes?q=21G&limit=30")).items
    .filter((r) => r.route_id !== "1369" && r.stop_count >= 5 && !r.deleted);
  if (!check(routes.length > 0, "a route to suggest a map line for")) return;
  const route = routes[0];
  await go(`#/route/${route.route_id}`);
  await waitFor(`document.body.innerText.includes("Edit name, colour and map line")`, "route actions");
  await click("Edit name, colour and map line");
  await sleep(300);
  if (await evaluate(`!!document.querySelector("dialog #new-draft-title")`)) await chooseNewDraft("Map line from GPS");
  const buttons = () => evaluate(`[...document.querySelectorAll("#panel button")].map((b) => b.textContent)`);
  await waitFor(`[...document.querySelectorAll("#panel button")].some((b) => b.textContent === "Suggest from GPS (last 14 days)")`, "the GPS suggestion");
  check((await buttons()).includes("Route through stops"), "the suggestion through the stops sits beside it");

  // ---- the road router: why it failed, and where
  const osrmFails = async (mode, says) => {
    await setMode({ osrm: mode });
    await click("Route through stops");
    await waitFor(`!!document.querySelector('#panel [data-failure-reason="${mode}"]')`, `the ${mode} reason`);
    check((await text("#panel")).includes(says), `OSRM ${mode}: "${says}"`);
  };
  await osrmFails("no_segment", "there is no road near");
  await osrmFails("no_route", "there is no road route from");
  await osrmFails("timeout", "did not answer in time");
  await shot("ml-01-osrm-reason");

  // ---- GPS: not enough evidence, and not set up
  await setMode({ osrm: "ok", gps: "not_enough_runs" });
  await click("Suggest from GPS (last 14 days)");
  await waitFor(`!!document.querySelector('#panel [data-failure-reason="gps_not_enough_runs"]')`, "the not-enough-runs reason");
  const why = await text("#panel");
  check(why.includes("Of 9 bus runs seen") && why.includes("1 passed this route's stops in order; at least 3 are needed"), "says how many runs there were and how many passed");
  // cut short to answer in time: says so, and that asking again reads further
  await setMode({ gps: "budget" });
  await click("Suggest from GPS (last 14 days)");
  await waitFor(`!!document.querySelector('#panel [data-stopped="budget"]')`, "the reading-stopped-early hint");
  check((await text("#panel")).includes("Reading stopped after 6 days to answer in time"), "says reading stopped early, and after how many days");
  await setMode({ gps: "unavailable" });
  await click("Suggest from GPS (last 14 days)");
  await waitFor(`!!document.querySelector('#panel [data-failure-reason="gps_unavailable"]')`, "the unavailable reason");

  // ---- GPS: partly snapped, then snapped all through
  await setMode({ gps: "partial" });
  await click("Suggest from GPS (last 14 days)");
  await waitFor(`document.getElementById("panel").innerText.includes("New map line ready")`, "a partly snapped line");
  const partial = await text("[data-gps-evidence]");
  check(partial.includes("partly snapped by OSRM (82% of the line"), `the evidence says it is partly snapped: ${partial}`);
  check((await text("#panel")).includes("OSRM said:"), "and what OSRM said");
  await setMode({ gps: "ok" });
  await click("Suggest from GPS (last 14 days)");
  await waitFor(`(document.querySelector("[data-gps-evidence]")?.innerText || "").includes("snapped by OSRM.")`, "a snapped line");
  const ev = await text("[data-gps-evidence]");
  check(/^31 runs by 12 buses, \d+(–| \w+ – )\d+ \w+, 94% of stops on the line, snapped by OSRM\.$/.test(ev.trim()), `the evidence reads as a sentence: ${ev}`);
  check((await text("#panel")).includes("from GPS"), "the line says it is from GPS");
  await shot("ml-02-gps-line");

  // ---- into the draft exactly as the OSRM suggestion goes, source gps
  await click("Add to draft", ".sticky-actions");
  await sleep(800);
  const set = await api(`change-sets/${await activeDraftId()}`);
  const change = (set.changes || []).find((c) => c.entity === "route" && c.entity_key === route.route_id);
  check(!!change && change.after.polyline_source === "gps" && !!change.after.encoded_polyline, "the draft holds the line with polyline_source gps");
  check(!!change && !("evidence" in change.after), "and not the evidence shown beside it");
  await setMode({ osrm: "ok", gps: "ok" });
}

// ---- only the map lines
if (process.argv.includes("--map-line")) {
  try {
    await connect();
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await load(UI);
    await signIn("editor1@nammayatri.in");
    await mapLineFlows();
    check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
  } catch (e) {
    failures.push(e.message);
    console.log(`FAIL ${e.message}`);
  } finally {
    try { ws?.close(); } catch { /* ignore */ }
    chrome.kill();
    await sleep(800);
    if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  }
  console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
  process.exit(failures.length ? 1 : 0);
}
// ====================================================================== end of map lines

// ---- only round 5
if (process.argv.includes("--round5")) {
  try {
    await connect();
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await load(UI);
    await signIn("admin@nammayatri.in");
    await round5Flows();
    check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
  } catch (e) {
    failures.push(e.message);
    console.log(`FAIL ${e.message}`);
  } finally {
    try { ws?.close(); } catch { /* ignore */ }
    chrome.kill();
    await sleep(800);
    if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  }
  console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
  process.exit(failures.length ? 1 : 0);
}
// ====================================================================== end of round 5

// ---- only the station merge
if (process.argv.includes("--station-merge")) {
  try {
    await connect();
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await load(UI);
    await signIn("editor1@nammayatri.in");
    await stationMergeFlows();
    check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
  } catch (e) {
    failures.push(e.message);
    console.log(`FAIL ${e.message}`);
  } finally {
    try { ws?.close(); } catch { /* ignore */ }
    chrome.kill();
    await sleep(800);
    if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  }
  console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
  process.exit(failures.length ? 1 : 0);
}
// ====================================================================== end of the station merge

// ====================================================================== feed access
// docs section 15: who may work on which feed. The mock's fixture
// (seed_feed_access): a second, small feed; every member granted the first feed
// at their role; a member who is a viewer on the first feed and an editor on
// the second; one with no feed at all; and a system account.
// `node dev/ui_smoke.mjs --feed-access` runs only these, on a fresh mock.
async function feedAccessFlows() {
  const fx = (await evaluate(`fetch("/__dev/state").then((r) => r.json())`)).feed_access;
  if (!check(!!fx && !!fx.second_feed, "the mock has the feed-access fixture")) return;
  const toastSays = (words) => waitFor(`[...document.querySelectorAll("#toasts .toast")].some((t) => t.textContent.includes(${JSON.stringify(words)}))`, `a toast saying "${words}"`);
  const grant = (email, gtfsId, role) => evaluate(`fetch("/__dev/grant", { method: "POST", credentials: "same-origin",
    headers: { "Content-Type": "application/json" }, body: JSON.stringify(${JSON.stringify({ email, gtfs_id: gtfsId, role })}) }).then((r) => r.json())`);
  const role = () => evaluate(`document.getElementById("feed-role").hidden ? "" : document.getElementById("feed-role").textContent`);

  // ---- a system account is refused at the gate
  await actAs(fx.system);
  await waitFor(`document.body.innerText.includes("Your editor access is turned off")`, "the gate refuses the system account");
  check(await evaluate(`document.getElementById("app").hidden`), "a system account never reaches the dashboard");

  // ---- no feed: a screen that says so, not an empty dashboard
  await signIn(fx.no_feeds);
  await waitFor(`!!document.getElementById("no-feeds")`, "the no-feeds screen");
  check((await text("#page")).includes("You have no feeds yet — ask an admin"), "a member with no feeds is told to ask an admin");
  check(await evaluate(`["feed-picker", "search", "nav"].every((c) => document.querySelector("." + c).hidden) && document.getElementById("new-menu").hidden`),
    "no switcher, search, pages or New menu without a feed");
  check((await evaluate(`document.getElementById("user-role").textContent`)).endsWith("no feeds yet"), "the account menu says there are no feeds yet");
  await shot("fa-01-no-feeds");

  // ---- the switcher lists the feeds granted, and badges and buttons read the chosen one
  await signIn(fx.second_editor);
  const options = await evaluate(`[...document.querySelectorAll("#feed-select option")].map((o) => o.value)`);
  const first = options.find((o) => o !== fx.second_feed);
  check(options.length === 2 && options.includes(fx.second_feed) && !!first, `the switcher lists the member's two feeds (${options.join(", ")})`);
  if (await evaluate(`document.getElementById("feed-select").value`) !== first) await choose("#feed-select", first);
  await waitFor(`document.getElementById("feed-role").textContent === "Viewer"`, "the badge reads viewer on the first feed");
  check(await evaluate(`document.getElementById("new-menu").hidden`), "a viewer on this feed gets no New menu");
  check((await evaluate(`document.getElementById("user-role").textContent`)).includes("viewer on"), "the account menu says viewer on this feed");
  await go("#/drafts");
  await waitFor(`document.getElementById("page").innerText.includes("Drafts")`, "drafts as a viewer");
  check(!(await text("#page")).includes("Start or open a draft"), "a viewer on this feed cannot start a draft");
  await choose("#feed-select", fx.second_feed);
  await waitFor(`document.getElementById("feed-role").textContent === "Editor"`, "the badge reads editor on the second feed");
  check(!(await evaluate(`document.getElementById("new-menu").hidden`)), "an editor on this feed gets the New menu");
  await go("#/drafts");
  await waitFor(`document.getElementById("page").innerText.includes("Start or open a draft")`, "an editor on this feed can start a draft");
  check(await evaluate(`document.querySelector('[data-nav="admin"]').offsetParent === null`), "a member never sees People");
  await shot("fa-02-second-feed");

  // ---- admin: one row per person, one column per feed
  await signIn("admin@nammayatri.in");
  check(await role() === "Admin", "an admin's badge reads admin");
  await go("#/admin");
  await waitFor(`!!document.querySelector("table.people tbody tr")`, "the people grid");
  const cols = await evaluate(`[...document.querySelectorAll("table.people th.feed-col")].map((th) => th.title)`);
  check(cols.includes(first) && cols.includes(fx.second_feed), `one column per feed (${cols.join(", ")})`);
  const row = (email) => `tr[data-email="${email}"]`;
  check(await evaluate(`(() => { const tr = document.querySelector(${JSON.stringify(row(fx.system))});
    return !!tr && tr.innerText.includes("System account") && tr.querySelector(".admin-switch").disabled; })()`),
  "a system account is labelled as such, and can never be made an admin");
  check(await evaluate(`document.querySelector(${JSON.stringify(`${row("admin@nammayatri.in")} .admin-switch`)}).disabled`), "an admin cannot switch off their own admin access");
  check(await evaluate(`[...document.querySelectorAll(${JSON.stringify(`${row("admin@nammayatri.in")} td.feed-cell`)})].every((td) => td.innerText.includes("Every role") && !td.querySelector("select"))`),
    "an admin's feed cells say they hold every role");
  const picker = `${row(fx.no_feeds)} select[data-feed="${fx.second_feed}"]`;
  check(await evaluate(`document.querySelector(${JSON.stringify(picker)}).value === ""`), "a member with no feed shows No access in every cell");
  await choose(picker, "approver");
  await toastSays("is now approver on");
  let nf = (await api("users")).items.find((u) => u.email === fx.no_feeds);
  check(nf.feeds.length === 1 && nf.feeds[0].gtfs_id === fx.second_feed && nf.feeds[0].role === "approver" && nf.feeds[0].granted_by_email === "admin@nammayatri.in",
    "the cell's picker gives the grant");
  await waitFor(`document.querySelector(${JSON.stringify(picker)})?.value === "approver"`, "the grid shows the grant");
  await shot("fa-03-people-grid");
  await choose(picker, "");
  await toastSays("no longer has");
  nf = (await api("users")).items.find((u) => u.email === fx.no_feeds);
  check(nf.feeds.length === 0, "No access takes the grant away");

  // the Admin switch, both ways, behind a confirm
  await waitFor(`!!document.querySelector(${JSON.stringify(`${row(fx.no_feeds)} .admin-switch`)})`, "the admin switch");
  await clickSel(`${row(fx.no_feeds)} .admin-switch`, "the Admin switch");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("every feed at every role")`, "the confirm says what an admin can do");
  await click("Make admin", "dialog");
  await toastSays("is now an admin");
  nf = (await api("users")).items.find((u) => u.email === fx.no_feeds);
  check(nf.is_admin === true && nf.feeds.length === 0, "the switch makes an admin");
  await waitFor(`[...document.querySelectorAll(${JSON.stringify(`${row(fx.no_feeds)} td.feed-cell`)})].every((td) => td.innerText.includes("Every role"))`, "the new admin's row holds every role");
  await clickSel(`${row(fx.no_feeds)} .admin-switch`, "the Admin switch again");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("no feed at all until you give them some")`, "the confirm says a demoted admin keeps no feed");
  await click("Remove admin access", "dialog");
  await toastSays("is no longer an admin");
  nf = (await api("users")).items.find((u) => u.email === fx.no_feeds);
  check(nf.is_admin === false && nf.feeds.length === 0, "the switch takes admin away, and leaves no feed");

  // the feed's history says who was let in and out
  await choose("#feed-select", fx.second_feed);
  await go("#/audit");
  await waitFor(`document.body.innerText.includes("Let someone into this feed")`, "history of the second feed");
  const history = await text("#page");
  check(history.includes("Took someone's access to this feed away") && history.includes(`${fx.no_feeds}: approver → no access`),
    "the feed's history names the grant and its revocation");

  // ---- the only feed is preselected
  await grant(fx.second_editor, first, null);
  await signIn(fx.second_editor);
  check(await evaluate(`document.querySelectorAll("#feed-select option").length === 1 && document.getElementById("feed-select").value === ${JSON.stringify(fx.second_feed)}`),
    "a member with one feed has it chosen");
  check(await role() === "Editor", "and the badge reads their role there");

  // ---- a grant taken away while the page is open: the next call says so, and the page follows
  await grant(fx.second_editor, fx.second_feed, null);
  await go("#/drafts");
  await waitFor(`!!document.getElementById("no-feeds")`, "the dashboard notices the feed is gone", 10000);
  await toastSays("access to that feed was taken away");
  check(await evaluate(`document.querySelector(".feed-picker").hidden`), "the switcher goes with the last feed");
  await shot("fa-04-revoked");
}

// ---- only feed access
if (process.argv.includes("--feed-access")) {
  try {
    await connect();
    await send("Page.enable");
    await send("Runtime.enable");
    await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await load(UI);
    await feedAccessFlows();
    check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
  } catch (e) {
    failures.push(e.message);
    console.log(`FAIL ${e.message}`);
  } finally {
    try { ws?.close(); } catch { /* ignore */ }
    chrome.kill();
    await sleep(800);
    if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  }
  console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
  process.exit(failures.length ? 1 : 0);
}
// ====================================================================== end of feed access

// ------------------------------------------------------------------ flows
try {
  await connect();
  await send("Page.enable");
  await send("Runtime.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });

  // gate: no identity
  await load(UI);
  await actAs("");
  await waitFor(`document.body.innerText.includes("Open the editor from its sign-in address")`, "no-identity message");
  check(true, "gate explains a missing SSO identity");

  // enrolment for a new user
  await actAs("newuser@nammayatri.in");
  await waitFor(`!!document.querySelector(".qr")`, "enrolment QR");
  const secret = (await text(".secret")).replace(/\s/g, "");
  check(secret.length >= 16, "enrolment shows the setup key");
  await shot("01-enrol");
  await type(".code-input", "000000");
  await waitFor(`document.body.innerText.includes("That code is not right")`, "wrong code message");
  await type(".code-input", await totp(secret));
  await waitFor(`!document.getElementById("app").hidden`, "app after enrolment");
  check(true, "enrolment with a real authenticator code signs in");

  // ================================================================ editor 1
  await signIn("editor1@nammayatri.in");
  await shot("02-home");
  // the top bar: coordinates to review with the pending count; stations to review while any is open
  const coordSummary = await api("feeds/chennai_bus/position-reviews/summary");
  await waitFor(`document.getElementById("coordinates-count").textContent === ${JSON.stringify(coordSummary.pending.toLocaleString("en-IN"))}`, "the coordinates count in the top bar");
  check(await evaluate(`(() => { const a = document.querySelector('[data-nav="coordinates"]'); return a.offsetParent !== null && a.innerText.startsWith("Coordinates to review"); })()`), `the top bar links to "Coordinates to review" with ${coordSummary.pending} pending`);
  await waitFor(`document.querySelector('[data-nav="stations"]').offsetParent !== null`, "the stations link while suggestions are open");
  check(true, "the top bar links to \"Stations to review\" while suggestions are waiting");
  check((await panelText()).includes("Coordinates to review"), "the home panel points to the coordinates to review");

  // ---- map: the search list sits above the map's zoom control
  await type("#search-input", "45BET");
  await waitFor(`document.querySelectorAll(".search-item").length > 0`, "search results");
  const covered = await evaluate(`(() => {
    const zoom = document.querySelector(".leaflet-control-zoom").getBoundingClientRect();
    const item = document.querySelector(".search-item").getBoundingClientRect();
    const left = Math.max(zoom.left, item.left), right = Math.min(zoom.right, item.right);
    const top = Math.max(zoom.top, item.top), bottom = Math.min(zoom.bottom, item.bottom);
    const overlap = left < right && top < bottom;
    const x = overlap ? (left + right) / 2 : item.left + 8, y = overlap ? (top + bottom) / 2 : item.top + item.height / 2;
    return { overlap, onItem: !!document.elementFromPoint(x, y)?.closest(".search-item"), x, y };
  })()`);
  check(covered.overlap, "the first search result lies over the map's zoom control");
  check(covered.onItem, "the search result, not the zoom control, is on top where they meet");
  await mouseClick(covered.x, covered.y);
  await waitFor(`location.hash.startsWith("#/route/")`, "a real click on the first result opens it");
  check((await evaluate("location.hash")).startsWith("#/route/"), "clicking the first search result opens the route, not the zoom control");

  // ---- map: the map follows the side panel's width
  await go("#/route/1369");
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 3`, "stage ladder");
  check((await text("#panel")).includes("ALWARPET ANJANEYAR TEMPLE"), "route 1369 lists Alwarpet Anjaneyar Temple");
  await evaluate(`document.documentElement.style.setProperty("--panel-w", "640px"); true`);
  await sleep(700);
  const sized = await mapCall(`const c = map.getContainer(); return map.getSize().x === c.clientWidth && map.getSize().y === c.clientHeight && c.clientWidth < 900;`);
  check(sized, "the map resizes itself when the side panel changes width");
  await evaluate(`document.documentElement.style.removeProperty("--panel-w"); true`);
  await sleep(500);
  await shot("03-route-ladder");

  // ---- map: clicking a stop marker while a route is open, then while a stop is open
  const sivan = await api("feeds/chennai_bus/stops/12e10ffb03");
  const citA = await api("feeds/chennai_bus/stops/f68ae67c0a");
  const citB = await api("feeds/chennai_bus/stops/8de4c3c4ac");
  await setView(citA.lat, citA.lon, 18);
  await waitStopDrawn("f68ae67c0a");
  await clickMapAt(citA.lat, citA.lon);
  await waitFor(`location.hash === "#/stop/f68ae67c0a" && document.getElementById("panel").innerText.includes("Cit Colony")`, "stop panel after clicking its marker over a route");
  check((await evaluate("location.hash")) === "#/stop/f68ae67c0a", "clicking a stop marker while a route is open switches the panel to that stop");
  await setView(citB.lat, citB.lon, 18);
  await waitStopDrawn("8de4c3c4ac");
  await clickMapAt(citB.lat, citB.lon);
  await waitFor(`location.hash === "#/stop/8de4c3c4ac"`, "second stop panel");
  check((await text("#panel")).includes("CIT COLONY"), "clicking another stop marker while a stop is open switches to it");

  // ---- map: names from zoom 17, one label for the two kerbs of one name
  await setView(13.03866, 80.25891, 18);
  await waitStopDrawn("de9014549c");
  // a label being taken off fades out (opacity 0) before it leaves the page
  const shownLabels = (cls) => `[...document.querySelectorAll(".leaflet-tooltip.${cls}")].filter((e) => e.style.opacity !== "0")`;
  await waitFor(`${shownLabels("stop-label")}.length > 2`, "stop labels at zoom 18");
  await sleep(300);
  const labels = await evaluate(`${shownLabels("stop-label")}.map((e) => e.textContent)`);
  check(labels.filter((l) => l === "ALWARPET ANJANEYAR TEMPLE").length === 1, `the two kerbs of ALWARPET ANJANEYAR TEMPLE share one label (${labels.length} labels)`);
  await shot("04-map-labels");
  await setView(13.03866, 80.25891, 16);
  await waitFor(`${shownLabels("stop-label")}.length === 0`, "no permanent labels below zoom 17");
  check(true, "labels are hidden below zoom 17 while markers stay");

  // ---- route stop list editor
  await go("#/route/1369");
  await waitFor(`document.body.innerText.includes("Edit stop list")`, "route actions");
  await click("Edit stop list");
  await chooseNewDraft("Smoke test: Alwarpet kerbs");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length > 10`, "stop list editor");
  check((await text("#draft-chip")).includes("Smoke test"), "draft chip shows the new draft");
  check(await evaluate(`document.querySelectorAll(".add-stop").length === document.querySelectorAll(".ladder.editing .row").length + 1`), "every gap and the end has a labelled Add stop button");
  check((await text("#add-3")).includes("Add stop here") && (await evaluate(`document.getElementById("add-3").getAttribute("aria-label")`)) === "Add a stop between stop 3 and stop 4", "the Add stop button between rows is labelled and says where");
  // move a stage stop up to break the fare rule
  const brokeRule = await evaluate(`(() => {
    const rows = [...document.querySelectorAll(".ladder.editing .row")];
    const i = rows.findIndex((r, k) => k > 1 && r.classList.contains("new"));
    rows[i].querySelector('button[title="Move up"]').click();
    return i;
  })()`);
  await sleep(400);
  check(/intermediate stop in stage \d+ but carries stage \d+|go back from|must start at|problem/.test(await text("#panel")), `moving stop ${brokeRule} up reports a fare-stage problem`);
  await shot("05-route-edit-error");
  await click("Cancel", ".sticky-actions");
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 3`, "ladder again");
  await click("Edit stop list");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length > 10`, "stop list editor again");
  const rowsBefore = await evaluate(`document.querySelectorAll(".ladder.editing .row").length`);
  // add a stop between stop 3 and 4, from suggestions near stop 3 or a search
  await clickSel("#add-3", "Add stop here between stop 3 and 4");
  await waitFor(`!!document.querySelector(".picker input")`, "stop picker");
  check(await evaluate(`document.activeElement === document.querySelector(".picker input")`), "the picker's search box has focus");
  await waitFor(`document.querySelectorAll(".picker-item").length > 0`, "nearby suggestions in the picker");
  check((await text(".picker")).includes("from stop 3"), "suggestions say how far they are from stop 3");
  await type(".picker input", "LUZ");
  await waitFor(`[...document.querySelectorAll(".picker-item")].some((b) => b.innerText.includes("LUZ"))`, "search results in the picker");
  await clickSel(".picker-item", "the first matching stop");
  check((await evaluate(`document.querySelectorAll(".ladder.editing .row").length`)) === rowsBefore + 1, "the chosen stop is added as a row");
  check((await text("#row-3 .name")).includes("LUZ"), "the stop is added between stop 3 and stop 4");
  // change which stop a row is
  const oldRow6 = await text("#row-6 .name");
  const oldId6 = await text("#row-6 .row-sub .meta");
  await clickSel("#change-6", "Change stop on row 7");
  await waitFor(`!!document.querySelector("#row-6 .picker input")`, "change-stop picker in the row");
  await type("#row-6 .picker input", "SIVAN TEMPLE");
  await waitFor(`/sivan temple/i.test(document.querySelector("#row-6 .picker-item")?.innerText || "")`, "change-stop search results");
  await clickSel("#row-6 .picker-item", "a stop to change to");
  const newRow6 = await text("#row-6 .name");
  check(/sivan temple/i.test(newRow6) && (await text("#row-6 .row-sub .meta")) !== oldId6, `Change stop replaced ${oldRow6} with ${newRow6}`);
  check(await evaluate(`!document.querySelector(".ladder.editing input:not([type=number]):not(.stage-name-input):not(.picker input)")`), "no free-text field says which stop a row is");
  // a stage stop's name comes from a list; "Other name…" is an explicit choice
  const stageRow = await evaluate(`[...document.querySelectorAll(".ladder.editing .row")].findIndex((r, k) => k > 0 && r.classList.contains("new"))`);
  const options = await evaluate(`[...document.querySelectorAll("#stage-name-${stageRow} option")].map((o) => o.textContent)`);
  check(options.includes("Other name…") && options.length > 3, `the stage name is chosen from the stop's name and the route's stage names (${options.length} options)`);
  await choose(`#stage-name-${stageRow}`, "other");
  await waitFor(`!!document.getElementById("stage-other-${stageRow}")`, "the Other name box");
  await type(`#stage-other-${stageRow}`, "SMOKE STAGE");
  check((await text(`#row-${stageRow + 1}`)).includes("SMOKE STAGE"), "the stops of that stage take the new stage name");
  // break the numbering, then renumber in order
  await type(`#stage-no-${stageRow}`, "9");
  check((await text("#panel")).includes("go back"), "a stage number out of order is reported");
  await click("Renumber stages in order");
  await waitFor(`!!document.getElementById("renumber-start")`, "renumber dialog");
  check((await text("dialog")).includes("will change"), "the renumber dialog says how many stops change");
  await shot("06-renumber");
  await click("Renumber stages", "dialog");
  check((await evaluate(`document.getElementById("stage-no-${stageRow}").value`)) === "2", "renumbering puts the stage back in order");
  await shot("07-route-editor");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("Show what is live now") || document.body.innerText.includes("draft applied")`, "route shown with draft");
  check(true, "route stop list change added to the draft");

  // ---- stop move
  await go("#/stop/de9014549c");
  await waitFor(`document.body.innerText.includes("Routes stopping here")`, "stop panel");
  check((await text("#panel")).includes("Merge with a duplicate"), "the stop panel offers Merge with a duplicate");
  await shot("08-stop");
  await click("Edit stop");
  await waitFor(`!!document.getElementById("stop-lat")`, "stop editor");
  await type("#stop-lat", "13.0385800");
  await type("#stop-name", "ALWARPET ANJANEYAR TEMPLE (TEYNAMPET SIDE)");
  check(await waitFor(`document.getElementById("panel").innerText.includes("Moved")`, "the distance moved"), "stop editor shows the distance moved");
  // leaving with unsaved edits asks first
  await evaluate(`location.hash = "#/drafts"; true`);
  await waitFor(`document.querySelector("dialog")?.innerText.includes("Leave without saving")`, "leave guard");
  check((await evaluate("location.hash")) === "#/stop/de9014549c", "the address stays while the leave question is open");
  await click("Cancel", "dialog");
  check(await has("#stop-lat"), "cancelling the leave question keeps the edits");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("changes this stop")`, "stop shows pending draft notice");

  // ---- station
  await go("#/stop/68e3cdc2e0");
  await waitFor(`document.body.innerText.includes("Club into a station")`, "stop with club action");
  await click("Club into a station");
  await waitFor(`!!document.getElementById("station-name")`, "station editor");
  await waitFor(`[...document.querySelectorAll("#panel button")].some(b => b.textContent === "Add")`, "nearby suggestions");
  await click("Add", "#panel");
  await type("#station-name", "Alwarpet Anjaneyar Temple");
  await type('[id^="platform-"]', "Towards Luz");
  await shot("09-station");
  await click("Add to draft", ".sticky-actions");
  await sleep(800);
  check(!(await text("#panel")).includes("Fix before submitting"), "station change accepted");

  // ---- map line
  await go("#/route/1369");
  await waitFor(`document.body.innerText.includes("Edit name, colour and map line")`, "route actions");
  await click("Edit name, colour and map line");
  await click("Route through stops");
  await waitFor(`document.body.innerText.includes("New map line ready")`, "proposed map line");
  await type("#route-color", "#0B6660");
  await click("Add to draft", ".sticky-actions");
  await sleep(800);

  // ---- draft page and submit
  const draftId = await activeDraftId();
  await go(`#/drafts/${draftId}`);
  await waitFor(`document.querySelectorAll(".change").length >= 4`, "draft page with 4 changes");
  const draftText = await text("#page");
  check(draftText.includes("moved") || draftText.includes("added"), "route stop list diff renders");
  check(draftText.includes("joins the station") && draftText.includes("Towards Luz"), "station membership diff renders with the platform label");
  await sleep(1200);
  await shot("10-draft-review");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "submitted status");
  const approveDisabled = await evaluate(`[...document.querySelectorAll("button")].find(b => b.textContent === "Approve")?.disabled`);
  check(approveDisabled === true || approveDisabled === undefined, "editor cannot approve");

  // ================================================================ editor 2
  // a second draft edits the same stop and is committed first -> conflict later
  await signIn("editor2@nammayatri.in");
  await go("#/stop/de9014549c");
  await waitFor(`document.body.innerText.includes("Edit stop")`, "stop for editor2");
  await click("Edit stop");
  await chooseNewDraft("Competing edit");
  await waitFor(`!!document.getElementById("stop-lat")`, "editor2 stop editor");
  await type("#stop-lon", "80.2588800");
  await click("Add to draft", ".sticky-actions");
  await sleep(600);
  const draft2 = await activeDraftId();
  await go(`#/drafts/${draft2}`);
  await waitFor(`document.body.innerText.includes("Submit for review")`, "draft2 page");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await sleep(600);

  // ---- New stop: place it by clicking the map
  await setView(12.9725, 80.2208, 18);
  await clickSel("#new-menu summary", "the New menu");
  check(await evaluate(`document.getElementById("new-menu").open`), "the New menu opens");
  await click("New stop", "#new-menu");
  await chooseNewDraft("Smoke: new things and stations");
  await waitFor(`document.getElementById("panel").innerText.includes("Not placed yet")`, "new stop form");
  const spot = await mapCall(`const c = map.getCenter(); return { lat: c.lat + 0.0002, lon: c.lng + 0.0003 };`);
  check(await evaluate(`import(${JSON.stringify(MAPJS)}).then((m) => !m.loadedStops().some((s) => Math.abs(s.lat - ${spot.lat}) < 0.0004 && Math.abs(s.lon - ${spot.lon}) < 0.0004))`), "the spot for the new stop is clear of existing stops");
  await clickMapAt(spot.lat, spot.lon);
  await waitFor(`document.getElementById("new-stop-lat").value !== ""`, "the clicked position in the form");
  check(Math.abs(Number(await evaluate(`document.getElementById("new-stop-lat").value`)) - spot.lat) < 0.00005, "clicking the map places the new stop there");
  await type("#new-stop-name", "Smoke New Stop");
  await type("#new-stop-platform", "Towards Velachery");
  await shot("11-new-stop");
  await click("Add stop to draft", ".sticky-actions");
  await waitFor(`document.getElementById("panel").innerText.includes("New stop added to your draft")`, "new stop added");
  const newStopId = (await text("#panel")).match(/ed_[0-9a-f]{10}/)?.[0];
  check(!!newStopId, `the server made the new stop's id (${newStopId})`);

  // ---- New route, then its stop list with the pickers
  await clickSel("#new-menu summary", "the New menu");
  await click("New route", "#new-menu");
  await waitFor(`!!document.getElementById("new-route-id")`, "new route form");
  check((await text("#panel")).includes("nightly GTFS build"), "the new route form says when passengers will see it");
  await type("#new-route-id", "1369");
  await waitFor(`document.getElementById("new-route-id-status").innerText.includes("already used")`, "a used route id is caught");
  await type("#new-route-id", "SMOKE-R1");
  await waitFor(`document.getElementById("new-route-id-status").innerText.includes("free")`, "a free route id");
  await type("#new-route-short", "S1");
  await type("#new-route-long", "Smoke New Stop - Velachery");
  await type("#new-route-color", "#1F5FBF");
  await click("Add route, then add its stops", ".sticky-actions");
  await waitFor(`document.getElementById("panel").innerText.includes("Stops of the new route S1")`, "stop list editor for the new route");
  await clickSel("#add-0", "Add the first stop");
  await waitFor(`!!document.querySelector(".picker input")`, "picker for the first stop");
  await type(".picker input", "Smoke New");
  await waitFor(`[...document.querySelectorAll(".picker-item")].some((b) => b.innerText.includes("Smoke New Stop"))`, "the stop new in the draft is offered");
  check((await text(".picker")).includes("New"), "the picker marks stops new in the draft");
  await clickSel(".picker-item", "the new stop");
  check((await text("#row-0")).includes("Stage stop") || (await evaluate(`document.querySelector("#row-0 select").value`)) === "NEW STOP", "the first stop starts a fare stage");
  check((await evaluate(`document.getElementById("stage-name-0").selectedOptions[0].textContent`)) === "Smoke New Stop", "its stage name defaults to the stop's own name");
  await clickSel("#add-1", "Add a stop at the end");
  check(!(await text(".picker")).includes("Smoke New Stop"), "the picker does not offer the stop right before the gap");
  await type(".picker input", "VELACHERY");
  await waitFor(`[...document.querySelectorAll(".picker-item")].some((b) => /velachery/i.test(b.innerText)) && /velachery/i.test(document.querySelector(".picker-item").innerText)`, "results for the second stop");
  await clickSel(".picker-item", "the second stop");
  // a stop can also be picked on the map (Velachery Ram Nagar has no other stop within 80 m)
  await clickSel("#add-2", "Add a stop at the end");
  const ramNagar = await api("feeds/chennai_bus/stops/b9f23c05b1");
  await setView(ramNagar.lat, ramNagar.lon, 18);
  await waitStopDrawn("b9f23c05b1");
  await click("Pick on the map", ".picker");
  check((await text("#map-banner")).includes("Click the stop"), "the map asks for a click on the stop");
  await clickMapAt(ramNagar.lat, ramNagar.lon);
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length === 3`, "the stop picked on the map is added");
  check((await text("#row-2 .row-sub .meta")).startsWith("b9f23c05b1"), "the stop clicked on the map became stop 3");
  await clickSel('button[aria-label="Remove stop 3"]', "remove stop 3 again");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length === 2`, "back to two stops");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`location.hash === "#/route/SMOKE-R1?draft=1" && document.getElementById("panel").innerText.includes("New route in your draft")`, "the new route with its stops");
  check((await text("#panel")).includes("nightly GTFS build"), "the new route says it needs the nightly build for trips");
  await shot("12-new-route");

  // ---- New station from the menu: click its stops on the map
  const gateA = await api("feeds/chennai_bus/stops/CHNS03131");
  const gateB = await api("feeds/chennai_bus/stops/CHNS06298");
  await clickSel("#new-menu summary", "the New menu");
  await click("New station", "#new-menu");
  await waitFor(`document.getElementById("panel").innerText.includes("New station")`, "new station editor");
  await setView((gateA.lat + gateB.lat) / 2, (gateA.lon + gateB.lon) / 2, 19);
  await waitStopDrawn("CHNS03131");
  await clickStopOnMap(gateA);
  await waitFor(`!!document.getElementById("platform-CHNS03131")`, "first member from a map click");
  await clickStopOnMap(gateB);
  await waitFor(`!!document.getElementById("platform-CHNS06298")`, "second member from a map click");
  check((await evaluate(`document.getElementById("station-id").value`)) === "stn_CHNS03131", "the station id is made from the first stop");
  // finding a stop by name, then giving up, leaves clicking on the map working
  await click("Find a stop to add", "#panel");
  await waitFor(`!!document.querySelector("#panel .picker input")`, "the station's stop finder");
  await click("Cancel", "#panel .picker");
  await clickStopOnMap(gateB);
  await waitFor(`!document.getElementById("platform-CHNS06298")`, "a map click takes a stop out again");
  await clickStopOnMap(gateB);
  await waitFor(`!!document.getElementById("platform-CHNS06298")`, "a map click puts it back");
  check(true, "clicking stops on the map still adds and removes them after using the stop finder");
  await type("#platform-CHNS03131", "Towards Adam Gate North");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`location.hash === "#/stop/CHNS03131"`, "back at the first member after adding the station");

  // ---- stations to review
  await go("#/stations");
  await waitFor(`document.querySelectorAll(".proposal-item").length > 10`, "suggested stations");
  await waitFor(`document.querySelector(".count-tabs").innerText.includes("2,258")`, "the list shows how many are waiting");
  await shot("13-stations-list");
  await type("#proposal-search", "A.M.S");
  await waitFor(`document.querySelectorAll(".proposal-item").length === 1`, "search narrows the list");
  await click("A.M.S.HOSPITAL", ".proposal-list");
  await waitFor(`document.querySelectorAll(".member-card").length === 3`, "the suggestion with its three stops");
  await waitFor(`document.querySelectorAll(".route-chip").length > 3`, "routes through each stop");
  const memberLabels = await evaluate(`${shownLabels("member-label")}.map((e) => e.textContent)`);
  check(memberLabels.length === 3 && memberLabels.every((l) => l.startsWith("Towards")), `each member stop is labelled on the map with its platform label (${memberLabels.join(" / ")})`);
  await clickSel(".route-chip", "a route through the first stop");
  await waitFor(`document.querySelector(".route-chip[aria-pressed=true]") !== null`, "the route shown on the map");
  await type("#proposal-name", "A.M.S. Hospital");
  await type("#platform-0", "Towards Adyar");
  check(await evaluate(`${shownLabels("member-label")}.some((e) => e.textContent === "Towards Adyar")`), "a relabelled platform updates on the map");
  const pin = await evaluate(`(() => { const r = document.querySelector(".station-pin").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
  await mouseDrag(pin.x, pin.y, pin.x + 30, pin.y - 20);
  await waitFor(`document.getElementById("panel").innerText.includes("moved")`, "the dragged station point");
  check(/moved \d+ m from the suggestion/.test(await text("#panel")), "dragging the station point moves it, and says how far");
  await click("Place in the middle of its stops", "#panel");
  await clickSel("#drop-2", "Drop the third stop");
  check((await text(".member-card.dropped")).includes("Dropped"), "the third stop is marked dropped");
  await shot("14-station-review");
  await click("Approve into draft", ".sticky-actions");
  await waitFor(`location.hash !== "#/stations/3"`, "moved on after approving");
  const approved = await api("station-proposals/3");
  check(approved.status === "approved" && approved.change_set_title === "Smoke: new things and stations", "the suggestion is approved into the draft");
  await go("#/stations/3");
  await waitFor(`document.getElementById("panel").innerText.includes("Approved into draft")`, "approved suggestion");
  check(await has(`#panel a[href="#/drafts/${approved.change_set_id}"]`), "a suggestion already in a draft links to that draft");
  // reject with a note, then reopen
  await go("#/stations/13");
  await waitFor(`document.getElementById("panel").innerText.includes("Adduthotti Bridge")`, "suggestion 13");
  await click("Reject…", ".sticky-actions");
  await waitFor(`!!document.getElementById("reject-note")`, "reject dialog");
  await click("Reject suggestion", "dialog");
  check((await text("dialog")).includes("Write a short reason"), "rejecting needs a note");
  await click("These stops are different places.", "dialog");
  await click("Reject suggestion", "dialog");
  await waitFor(`location.hash !== "#/stations/13"`, "moved on after rejecting");
  check((await api("station-proposals/13")).status === "rejected", "the suggestion is rejected");
  await go("#/stations/13");
  await waitFor(`document.getElementById("panel").innerText.includes("Reopen for review")`, "rejected suggestion");
  await click("Reopen for review");
  await waitFor(`!!document.getElementById("proposal-name")`, "reopened suggestion is editable again");
  check((await api("station-proposals/13")).status === "pending", "reopening puts it back to review");
  // approve every suggestion in one area
  await go("#/stations");
  await waitFor(`!!document.getElementById("proposal-search")`, "list again");
  await type("#proposal-search", "");
  await waitFor(`document.querySelectorAll(".proposal-item").length > 10`, "the whole list without the search");
  await setView(12.80723, 80.19818, 16);
  await clickSel("#proposal-area", "Only in the map area");
  await waitFor(`document.querySelectorAll(".proposal-item").length >= 1 && document.querySelectorAll(".proposal-item").length < 20`, "only suggestions in the map area");
  const inArea = await evaluate(`document.querySelectorAll(".proposal-item").length`);
  await click("Approve all in the map area");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("into the draft")`, "area approval dialog");
  await click("into the draft", "dialog");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("Added")`, "area approval result");
  check((await text("dialog")).includes(`Added ${inArea} station`), `all ${inArea} suggestions in the map area were added`);
  await click("Done", "dialog");
  await clickSel("#proposal-area", "untick Only in the map area");
  const newThingsDraft = await activeDraftId();
  await go(`#/drafts/${newThingsDraft}`);
  await waitFor(`document.querySelectorAll(".change").length >= 6`, "the new things and stations draft");
  const newThings = await text("#page");
  check(newThings.includes("suggested station #3") && newThings.includes("Towards Adyar"), "the draft shows the approved suggestion with its platform labels");
  check(newThings.includes("new route") && newThings.includes("nightly GTFS build"), "the draft shows the new route and when passengers see it");
  const areaChange = await evaluate(`(() => {
    const article = [...document.querySelectorAll(".change")].find((a) => /suggested station #(\\d+)/.test(a.innerText) && !a.innerText.includes("suggested station #3."));
    const id = Number(article.innerText.match(/suggested station #(\\d+)/)[1]);
    [...article.querySelectorAll("button")].find((b) => b.textContent === "Remove from draft").click();
    return id;
  })()`);
  await waitFor(`document.querySelector("dialog")?.innerText.includes("back to the list of stations to review")`, "the remove question says the suggestion goes back");
  await click("Remove change", "dialog");
  await waitFor(`document.querySelectorAll(".change").length >= 6 && !document.getElementById("page").innerText.includes("suggested station #${areaChange}.")`, "the draft without that station");
  check((await api(`station-proposals/${areaChange}`)).status === "pending", `removing suggested station #${areaChange} from the draft puts it back to review`);
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "stations draft submitted");

  // ---- merge duplicate stops
  await go("#/stop/5b3d5a84a7");
  await waitFor(`document.body.innerText.includes("Merge with a duplicate")`, "stop with merge action");
  await click("Merge with a duplicate");
  await waitFor(`document.getElementById("panel").innerText.includes("Same name")`, "nearby duplicates with the same name first");
  await click("Add", "#panel .list");
  await click("Compare 2 stops", "#panel");
  await waitFor(`!!document.querySelector("table.compare")`, "the two stops side by side");
  const compare = await text("#panel");
  check(compare.includes("Which stop id should stay?") && compare.includes("Suggested: used by more routes"), "asks which stop id stays and suggests one");
  check(/will switch from \w+ to \w+/.test(compare), "says which routes will switch before adding");
  await shot("15-merge-compare");
  const keepOther = await evaluate(`[...document.querySelectorAll('input[name="keep-id"]')].find((r) => !r.checked).id`);
  await clickSel(`#${keepOther}`, "the other stop id");
  check((await text(".sticky-actions")).includes(keepOther.replace("keep-", "")), "the add button names the stop id that stays");
  await click("Add merge to draft", ".sticky-actions");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("Add this merge")`, "merge confirmation");
  await click("Add merge to draft", "dialog");
  await chooseNewDraft("Smoke: merges");
  await waitFor(`document.getElementById("panel").innerText.includes("Merge added to your draft")`, "merge in the draft");
  // a merge that would put the same stop twice in a row
  await go("#/merge/6115708e0a?with=4e77fe0fcd");
  await waitFor(`!!document.querySelector("table.compare")`, "compare for a back-to-back merge");
  check((await text("#panel")).includes("one after the other"), "a back-to-back merge is flagged before adding");
  await click("Add merge to draft", ".sticky-actions");
  await click("Add merge to draft", "dialog");
  await waitFor(`document.getElementById("panel").innerText.includes("cannot be submitted")`, "the server's error for the merge");
  check(/Route .*1369.* twice in a row, at stops 1 and 2/.test(await text("#panel")), "the back-to-back error names the route and the stops in plain words");
  await click("Remove the merge from the draft");
  await sleep(600);
  const mergeDraft = await activeDraftId();
  await go(`#/drafts/${mergeDraft}`);
  await waitFor(`document.querySelectorAll(".change").length === 1`, "merge draft page");
  check(/merged into .*routes? updated/.test(await text("#page")), "the draft shows the merge with the routes it updates");

  // ---- coordinates to review
  // Reviews are chosen from what the mock was seeded with (real rows from the local
  // database, or made-up ones), leaving out stops and routes the drafts of this run
  // already change, so the commits at the end do not collide.
  const touched = new Set(), touchedRoutes = new Set();
  for (const open of (await api("feeds/chennai_bus/change-sets?status=draft,submitted,approved&limit=100")).items) {
    for (const c of (await api(`change-sets/${open.change_set_id}`)).changes) {
      const a = c.after || {};
      if (c.entity === "stop" || c.entity === "station") touched.add(c.entity_key);
      if (a.into_stop_id) touched.add(a.into_stop_id);
      if (a.into_station_id) touched.add(a.into_station_id);
      ((c.before || {}).moving_platforms || []).forEach((m) => touched.add(m.stop_id));
      (a.members || []).forEach((m) => touched.add(m.stop_id));
      (a.member_stop_ids || []).forEach((id) => touched.add(id));
      if (c.entity === "route_stops") {
        touchedRoutes.add(c.entity_key);
        (a.rows || []).forEach((r) => r.stop_id && touched.add(r.stop_id));
      }
    }
  }
  const pendingReviews = (await api("feeds/chennai_bus/position-reviews?status=pending&limit=500")).items;
  const reviewDetails = new Map();
  const reviewDetail = async (id) => {
    if (!reviewDetails.has(id)) reviewDetails.set(id, await api(`position-reviews/${id}`));
    return reviewDetails.get(id);
  };
  // needs 2 DISTINCT raw-bearing suspect groups (for group A and group B below) plus
  // a non-suspect group; real loaded data varies, the mock's synthetic fixture guarantees it
  const mixed = pendingReviews.find((r) => r.evidence?.mixed_origins && !touched.has(r.stop_id)
    && r.evidence.route_groups.filter((g) => g.suspect && g.raw_lat != null).length >= 2
    && r.evidence.route_groups.some((g) => !g.suspect));
  const fx = mixed ? await reviewDetail(mixed.review_id) : null;
  check(!!fx, "there is a stop whose routes came from several original stops (the THANDALAM fixture)");
  (fx ? fx.routes : []).forEach((l) => touchedRoutes.add(l.route_id));
  const usedStops = new Set(fx ? [fx.stop_id] : []);
  const pickReview = async (want) => {
    for (const r of pendingReviews) {
      if (usedStops.has(r.stop_id) || touched.has(r.stop_id) || r.evidence?.mixed_origins || !want(r)) continue;
      const d = await reviewDetail(r.review_id);
      if (!d.stop || d.stop.deleted || d.problems.length || !d.routes.some((l) => l.prev && l.next)
        || d.routes.some((l) => touchedRoutes.has(l.route_id))) continue;
      usedStops.add(r.stop_id);
      return d;
    }
    return null;
  };
  const revA = await pickReview((r) => r.suggested_lat != null);
  const revB = await pickReview((r) => r.raw_lat != null);
  const revC = await pickReview(() => true);
  const revD = await pickReview(() => true);
  const revE = await pickReview(() => true);
  check(!!(fx && revA && revB && revC && revD && revE), "enough coordinate reviews to try every action");
  const pendingText = coordSummary.pending.toLocaleString("en-IN");
  const lastToast = () => text("#toasts .toast:last-child");

  // the list: counts, search, only in the map area
  await go("#/coordinates");
  await waitFor(`document.querySelectorAll(".proposal-item").length > 1`, "coordinates to review");
  await waitFor(`/To review\\s*${pendingText}/.test(document.querySelector(".count-tabs").innerText)`, "the pending count on the list");
  check(await evaluate(`document.querySelectorAll(".coord-pin").length === document.querySelectorAll(".proposal-item").length`), "each review in the list is a pin on the map");
  const listed = await evaluate(`document.querySelectorAll(".proposal-item").length`);
  await shot("21-coordinates-list");
  await type("#review-search", fx.stop_id);
  await waitFor(`document.querySelectorAll(".proposal-item").length === 1 && document.querySelector(".proposal-item").innerText.includes(${JSON.stringify(fx.stop_name)})`, "search by stop id");
  check(true, "the list searches by stop id");
  await type("#review-search", "");
  await waitFor(`document.querySelectorAll(".proposal-item").length === ${listed}`, "the whole list again");
  await setView(fx.stop.lat, fx.stop.lon, 15);
  await clickSel("#review-area", "Only in the map area");
  await waitFor(`document.querySelectorAll(".proposal-item").length >= 1 && document.querySelectorAll(".proposal-item").length < ${listed}`, "only reviews in the map area");
  check((await text("#panel")).includes(fx.stop_name), "only the reviews in the map area are listed");
  await clickSel("#review-area", "untick Only in the map area");
  await waitFor(`document.querySelectorAll(".proposal-item").length === ${listed}`, "the whole list after unticking");

  // no draft open, so the first move asks which draft to use
  await clickSel("#draft-chip", "the draft chip");
  await click("Stop using a draft", "dialog");
  await waitFor(`document.getElementById("draft-chip").innerText.includes("No draft open")`, "no draft open");

  // a move to the suggestion, with the details a reviewer needs; the panel stays
  // on the review afterwards, so a second action can follow (docs section 8.1)
  await go(`#/coordinates/${revA.review_id}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Why it is flagged")`, "a review with a suggestion");
  const aText = await text("#panel");
  const aNow = { lat: revA.stop.lat, lon: revA.stop.lon };
  check(aText.includes(revA.reason.slice(0, 60)), "the review says why the stop is flagged");
  check(aText.includes(`Detour now\n${distance(revA.detour_m)}`), `the review shows the detour in metres (${distance(revA.detour_m)})`);
  check([...new Set(revA.routes.map((l) => l.short_name || l.route_id))].every((n) => aText.includes(n)), "the review lists the numbers of its routes");
  if ((revA.evidence.chalo_nearby || "").trim()) check(aText.includes("Chalo has nearby"), "the review shows the Chalo hint");
  check(await evaluate(`[...document.querySelectorAll("#panel a[target=_blank]")].some((a) => a.href.startsWith("https://www.google.com/maps/search/?api=1&query="))`), "the review links to Google Maps");
  const aLabels = await evaluate(`${shownLabels("coord-label")}.map((e) => e.textContent)`);
  check(aLabels.includes("Now") && aLabels.some((l) => l.startsWith("Suggestion")) && (revA.raw_lat == null || aLabels.includes("MTC raw point")),
    `the map marks the point now, the raw MTC point and the suggestion with its source (${aLabels.join(" / ")})`);
  const aLegs = await legKinds();
  check(aLegs.now > 0 && aLegs.direct > 0, "the map draws every route's previous, stop, next legs beside the straight line without the stop");
  await clickSel("#use-suggestion", "Use suggestion");
  const suggestion = { lat: revA.suggested_lat, lon: revA.suggested_lon };
  const aAfter = medianDetour(revA.routes, suggestion);
  await waitFor(`document.getElementById("panel").innerText.includes("Moving the stop here")`, "the detour at the suggestion");
  check((await text("#panel")).includes(`goes from ${distance(revA.detour_m)} to ${distance(aAfter)}`), `the panel works out the detour the suggestion gives from the legs (${distance(aAfter)})`);
  const movedLegs = await legKinds();
  check(movedLegs.move > 0 && movedLegs.was > 0, "the legs to the new position are drawn beside the old ones");
  await shot("22-coordinate-move");
  await click("Add move to draft", ".sticky-actions");
  await chooseNewDraft("Smoke: coordinates");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes(${JSON.stringify(`Moved ${revA.stop.name}`)})`, "a toast confirms the move");
  const moveToast = await lastToast();
  check(moveToast.includes(`Detour now ${distance(aAfter)}`), `the toast says the detour the server measured (${moveToast})`);
  const coordDraft = await activeDraftId();
  await waitFor(`location.hash === "#/coordinates/${revA.review_id}"`, "the panel stays on the review after the move");
  await waitFor(`document.getElementById("panel").innerText.includes("In draft")`, "the review now shows it is in a draft");
  check(await has(`#panel .review-status a[href="#/drafts/${coordDraft}"]`), "and links the draft the move went into");
  check(await evaluate(`document.querySelector(".draft-actions")?.innerText.includes("Move")`), "the actions list in the draft shows the move");
  const aInDraft = await api(`position-reviews/${revA.review_id}`);
  check(aInDraft.status === "approved" && aInDraft.change_set_title === "Smoke: coordinates" && aInDraft.draft_actions?.length === 1, "the review is approved with one action in the draft");
  check(await evaluate(`document.getElementById("review-move").disabled && document.getElementById("why-move").innerText.includes("already moved")`), "Move is now disabled: the stop was already moved in this draft");

  // the raw point, then a click on the map, then dragging the pin
  await go(`#/coordinates/${revB.review_id}`);
  await waitFor(`!!document.getElementById("use-raw")`, "a review with a raw point");
  const pinAt = () => mapCall(`let p = null; map.eachLayer((l) => { const html = l.options && l.options.icon && l.options.icon.options.html; if (String(html || "").includes("stop-pin") && l.getLatLng) { const q = l.getLatLng(); p = { lat: q.lat, lon: q.lng }; } }); return p;`);
  await clickSel("#use-raw", "Use raw point");
  await sleep(400);
  const atRaw = await pinAt();
  check(atRaw && metres(atRaw.lat, atRaw.lon, revB.raw_lat, revB.raw_lon) < 0.5, "Use raw point puts the pin on the MTC raw point");
  await sleep(600);
  const beside = await mapCall(`const s = map.getSize(); const at = map.latLngToContainerPoint([${atRaw ? atRaw.lat : 0}, ${atRaw ? atRaw.lon : 0}]);
    const x = at.x + 70 < s.x - 20 ? at.x + 70 : at.x - 70, y = at.y - 50 > 20 ? at.y - 50 : at.y + 50;
    const q = map.containerPointToLatLng([x, y]); return { lat: q.lat, lon: q.lng };`);
  await clickMapAt(beside.lat, beside.lon);
  const clicked = await pinAt();
  check(clicked && metres(clicked.lat, clicked.lon, beside.lat, beside.lon) < 100 && metres(clicked.lat, clicked.lon, atRaw.lat, atRaw.lon) > 1, "a click on the map moves the pin there");
  const pinBox = await evaluate(`(() => { const r = document.querySelector(".stop-pin").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
  await mouseDrag(pinBox.x, pinBox.y, pinBox.x + 40, pinBox.y + 30);
  const dragged = await pinAt();
  check(dragged && metres(dragged.lat, dragged.lon, clicked.lat, clicked.lon) > 1, "dragging the pin moves it");
  await waitFor(`document.getElementById("panel").innerText.includes(${JSON.stringify(`to ${distance(medianDetour(revB.routes, dragged))}.`)})`, "the detour at the dragged position");
  check(true, "the detour follows the pin as it is dragged");
  await evaluate(`location.hash = "#/drafts"; true`);
  await waitFor(`document.querySelector("dialog")?.innerText.includes("Leave without saving")`, "the leave question for an unsaved position");
  await click("Cancel", "dialog");
  check((await evaluate("location.hash")) === `#/coordinates/${revB.review_id}`, "leaving with a new position not in a draft asks first");
  await click("Add move to draft", ".sticky-actions");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes(${JSON.stringify(`Moved ${revB.stop.name}`)})`, "a toast confirms the second move");
  await waitFor(`location.hash === "#/coordinates/${revB.review_id}" && document.getElementById("panel").innerText.includes("In draft")`, "the panel stays on this review too");
  check((await api(`position-reviews/${revB.review_id}`)).status === "approved", "the dragged position is in the draft");

  // position is correct, with a note, then reopened - this is the one action that
  // still moves on, since a confirmed review has nothing left to do
  await go(`#/coordinates/${revC.review_id}`);
  await waitFor(`!!document.getElementById("review-confirm")`, "a review to confirm");
  await clickSel("#review-confirm", "Position is correct…");
  await waitFor(`!!document.getElementById("confirm-note")`, "the confirm dialog");
  await click("Checked on Google Maps", "dialog");
  await click("Confirm position", "dialog");
  await waitFor(`location.hash !== "#/coordinates/${revC.review_id}"`, "moved on after confirming");
  const cDone = await api(`position-reviews/${revC.review_id}`);
  check(cDone.status === "confirmed" && /Google Maps/.test(cDone.review_note || ""), "Position is correct closes the review with its note");
  check((await text("#panel .notice.ok")).includes("confirmed as correct"), "the next review says the last one was confirmed");
  await go("#/coordinates");
  await click("Confirmed", ".count-tabs");
  await waitFor(`[...document.querySelectorAll(".proposal-item")].some((li) => li.innerText.includes(${JSON.stringify(revC.stop_name)}))`, "the confirmed review in its tab");
  await go(`#/coordinates/${revC.review_id}`);
  await waitFor(`document.getElementById("panel").innerText.includes("Confirmed as correct")`, "a confirmed review");
  check(!(await has("#review-confirm")), "a confirmed review offers no second confirm");
  await click("Reopen", "#panel");
  await waitFor(`!!document.getElementById("review-confirm")`, "the reopened review");
  check((await api(`position-reviews/${revC.review_id}`)).status === "pending", "Reopen puts a confirmed review back to review");
  await go("#/coordinates");
  await click("To review", ".count-tabs");
  await waitFor(`document.querySelectorAll(".proposal-item").length > 0`, "the reviews to do again");

  // Next leaves a review for later
  await go(`#/coordinates/${revD.review_id}`);
  await waitFor(`!!document.getElementById("review-confirm")`, "a review to skip");
  const nextHref = await evaluate(`[...document.querySelectorAll(".sticky-actions a")].find((a) => a.textContent.startsWith("Next"))?.getAttribute("href")`);
  check(!!nextHref && nextHref !== `#/coordinates/${revD.review_id}`, "Next offers the following review");
  await click("Next ›", ".sticky-actions");
  await waitFor(`location.hash === ${JSON.stringify(nextHref || "")}`, "the next review");
  check((await api(`position-reviews/${revD.review_id}`)).status === "pending", "Next leaves the skipped review waiting");

  // splitting routes off a stop whose routes came from three original stops: this
  // review takes several actions in the same draft (docs section 8.1)
  await go(`#/coordinates/${fx.review_id}`);
  await waitFor(`document.querySelectorAll(".split-route").length > 3`, "the stop with routes from several original stops");
  const groupsFx = fx.evidence.route_groups;
  const liveIds = [...new Set(fx.routes.map((l) => l.route_id))];
  const suspectIds = liveIds.filter((id) => groupsFx.some((g) => g.suspect && g.route_ids.includes(id)));
  const checkedIds = () => evaluate(`[...document.querySelectorAll(".split-route input:checked")].map((i) => i.id.replace("split-route-", "")).sort()`);
  check((await text("#panel")).includes(`This stop serves routes from ${groupsFx.length} original stops`), "a banner warns that moving the stop moves the routes of every original stop");
  check(JSON.stringify(await checkedIds()) === JSON.stringify([...suspectIds].sort()), `the routes of the suspect groups start checked (${suspectIds.length})`);
  check(await evaluate(`document.querySelector(".route-group").classList.contains("suspect") && document.querySelector(".route-group").innerText.includes(${JSON.stringify(groupsFx.find((g) => g.suspect).origin_name)})`), "suspect groups come first, headed by their original stop");
  if (liveIds.some((id) => !groupsFx.some((g) => g.route_ids.includes(id)))) check((await text("#panel")).includes("Other routes"), "routes in no group are listed under Other routes");
  await clickAllUnchecked(".split-route input");
  await waitFor(`document.querySelectorAll(".split-route input:checked").length === document.querySelectorAll(".split-route input").length`, "every route checked");
  check(await evaluate(`document.getElementById("review-split").disabled && document.getElementById("why-split").innerText.includes("all routes: use Move")`), "with every route checked Split is off: all routes: use Move");
  await clickAllChecked(".split-route input");

  // an approved review in ANOTHER draft is refused with a link to the one it is in
  const groupA = groupsFx.find((g) => g.suspect && g.raw_lat != null);
  const splitIdsA = liveIds.filter((id) => groupA.route_ids.includes(id)).sort();
  const groupElA = `[...document.querySelectorAll(".route-group")].find((el) => el.innerText.includes(${JSON.stringify(groupA.origin_stop_id)}))`;
  await ensureGroupChecked(groupElA, true);
  await waitFor(`document.querySelectorAll(".split-route input:checked").length === ${splitIdsA.length}`, "group A's routes checked");
  await evaluate(`${groupElA}.querySelector("button").click(); true`);
  await waitFor(`!document.getElementById("review-split").disabled`, "Split ready with a pin and checked routes");
  const preOther = await apiSend("POST", "feeds/chennai_bus/change-sets", { title: "Smoke: elsewhere" });
  const splitFromElsewhere = await apiSend("POST", `position-reviews/${fx.review_id}/split`, {
    change_set_id: preOther.body.change_set_id, route_ids: splitIdsA, lat: groupA.raw_lat, lon: groupA.raw_lon,
  });
  check(splitFromElsewhere.status === 200, "set-up: the review already has an action in a first draft");
  await clickSel("#draft-chip", "the draft chip");
  await chooseNewDraft("Smoke: active elsewhere");
  await click("Split checked routes to a new stop here", ".sticky-actions");
  await waitFor(`(document.querySelector(".sticky-actions .notice.error")?.innerText || "").includes("In draft")`, "the review-in-another-draft notice");
  check(await has(`.sticky-actions .notice.error a[href="#/drafts/${preOther.body.change_set_id}"]`), "and it links to the draft the review is already in");
  // undo the API-made split so the draft the UI actually uses starts clean
  await apiSend("DELETE", `change-sets/${preOther.body.change_set_id}/changes/${splitFromElsewhere.body.change_id}`);
  check((await api(`position-reviews/${fx.review_id}`)).status === "pending", "removing that change returns the review to pending");
  await clickSel("#draft-chip", "the draft chip");
  await chooseNewDraft("Smoke: coordinates for THANDALAM");
  const fxDraft = await activeDraftId();

  // group A, with a draft conflict first: a change to one of its routes is already there
  const blockedRoute = await api(`feeds/chennai_bus/routes/${splitIdsA[0]}`);
  const blocker = await apiSend("POST", `change-sets/${fxDraft}/changes`, {
    entity: "route_stops", op: "replace", entity_key: splitIdsA[0],
    after: {
      base_rows_hash: blockedRoute.rows_hash,
      rows: blockedRoute.rows.map((r) => ({ stop_id: r.stop_id, stop_type: r.stop_type, stage_no: r.stage_no, stage_name: r.stage_name, marker_id: r.marker_id, marker_name: r.marker_name, marker_lat: r.marker_lat, marker_lon: r.marker_lon, stop_name_override: r.stop_name_override, provider_id: r.provider_id })),
    },
  });
  check(blocker.status === 201, "set-up: the active draft already changes one of group A's routes");
  // group A is still checked with its pin still placed from before: switching the
  // active draft (above) does not touch either, so the same Split click now goes
  // into the new draft and meets the blocker
  check(!(await evaluate(`document.getElementById("review-split")?.disabled`)), "Split is still ready: checking routes and placing the pin do not depend on which draft is active");
  const splitLegsA = fx.routes.filter((l) => splitIdsA.includes(l.route_id));
  const fxNow = { lat: fx.stop.lat, lon: fx.stop.lon };
  const splitAtA = { lat: groupA.raw_lat, lon: groupA.raw_lon };
  check((await text("#panel")).includes(`their detour goes from ${distance(medianDetour(splitLegsA, fxNow))} to ${distance(medianDetour(splitLegsA, splitAtA))}`), "the panel shows the detour the split routes would get at the pin");
  const splitKinds = await legKinds();
  check(splitKinds.split > 0 && splitKinds.now > 0 && !splitKinds.move, "the routes being split off are drawn to the pin in their own colour, the others stay");
  await shot("23-coordinate-split");
  await click("Split checked routes to a new stop here", ".sticky-actions");
  await waitFor(`(document.querySelector(".sticky-actions .notice.error")?.innerText || "").includes("#${blocker.body.change_id}")`, "the draft conflict next to the actions");
  check((await text(".sticky-actions .notice.error")).includes("in the way"), "a 409 draft conflict is shown inline, naming the change in the way");
  check((await api(`position-reviews/${fx.review_id}`)).status === "pending", "the refused split changed nothing");
  await apiSend("DELETE", `change-sets/${fxDraft}/changes/${blocker.body.change_id}`);
  await click("Split checked routes to a new stop here", ".sticky-actions");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes("Split")`, "a toast confirms the split");
  const splitAToast = await lastToast();
  const splitStopIdA = splitAToast.match(/new stop (ed_[0-9a-f]{10})/)?.[1];
  check(!!splitStopIdA, `the toast names the new stop (${splitStopIdA})`);
  await waitFor(`location.hash === "#/coordinates/${fx.review_id}" && document.getElementById("panel").innerText.includes("In draft")`, "the panel stays on the review after the split");
  check(await has(`#panel .review-status a[href="#/drafts/${fxDraft}"]`), "the review links to the draft its actions are in");
  check(await evaluate(`document.querySelector(".draft-actions")?.innerText.includes("Split")`), "the actions list shows the split");

  // group A's routes are now locked: no checkbox, and a "split off" badge
  const groupElA2 = `[...document.querySelectorAll(".route-group")].find((el) => el.innerText.includes(${JSON.stringify(groupA.origin_stop_id)}))`;
  check(await evaluate(`${groupElA2}.querySelectorAll(".split-route input").length === 0`), "group A's routes have no checkbox any more");
  check(await evaluate(`${groupElA2}.innerText.includes("Split off") && ${groupElA2}.innerText.includes(${JSON.stringify(splitStopIdA)})`), `group A's routes show "Split off" naming the new stop (${splitStopIdA})`);
  check(!(await evaluate(`[...document.querySelectorAll(".split-route input")].some((i) => ${JSON.stringify(splitIdsA)}.includes(i.id.replace("split-route-", "")))`)), "group A's routes cannot be checked again");

  // group B, split into the same draft as a second action
  const groupB = groupsFx.find((g) => g.suspect && g.raw_lat != null && g !== groupA);
  const splitIdsB = liveIds.filter((id) => groupB.route_ids.includes(id)).sort();
  const groupElB = `[...document.querySelectorAll(".route-group")].find((el) => el.innerText.includes(${JSON.stringify(groupB.origin_stop_id)}))`;
  // the reload after group A's split pre-checks EVERY still-open suspect group, not just
  // "the next one" - a review with a third raw suspect group (real data has some) leaves it
  // checked too, so start from nothing rather than assume only group B came back checked
  await clickAllChecked(".split-route input");
  await ensureGroupChecked(groupElB, true);
  await waitFor(`document.querySelectorAll(".split-route input:checked").length === ${splitIdsB.length}`, "group B's routes checked");
  await evaluate(`${groupElB}.querySelector("button").click(); true`);
  await waitFor(`!document.getElementById("review-split").disabled`, "Split ready for group B");
  await click("Split checked routes to a new stop here", ".sticky-actions");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes("Split")`, "a second toast confirms the second split");
  const splitBToast = await lastToast();
  const splitStopIdB = splitBToast.match(/new stop (ed_[0-9a-f]{10})/)?.[1];
  check(!!splitStopIdB && splitStopIdB !== splitStopIdA, `the second split makes a different new stop (${splitStopIdB})`);
  await waitFor(`document.getElementById("panel").innerText.includes("In draft")`, "the review again, with two actions now");
  const afterTwoSplits = await api(`position-reviews/${fx.review_id}`);
  check(afterTwoSplits.draft_actions?.length === 2 && afterTwoSplits.draft_actions.every((a) => a.kind === "split"), `the draft has both splits (${afterTwoSplits.draft_actions?.length})`);

  // the remaining, own routes still need their own fix: a move, as the review's
  // third action in the same draft
  const ownIds = liveIds.filter((id) => !splitIdsA.includes(id) && !splitIdsB.includes(id));
  check(ownIds.length > 0, "the stop still has its own routes after both splits");
  const target = { lat: fxNow.lat + 0.0003, lon: fxNow.lon + 0.0002 };
  // the review fits the map to every route end at this stop, not only the kerb
  // itself, so it can be zoomed far out; zoom back in on the stop before clicking
  // a point close to it
  await setView(fxNow.lat, fxNow.lon, 17);
  await clickMapAt(target.lat, target.lon);
  await waitFor(`!!document.getElementById("review-move") && !document.getElementById("review-move").disabled`, "Move ready for the stop's own routes");
  // this move is for the stop's OWN remaining routes, not a further split - but the
  // reload after group B's split pre-checks any suspect group still un-split (here,
  // the third one this test never splits), and moving with routes checked asks the
  // reviewer to confirm "move every route?" first (correctly - see splitBox's own
  // warning for the same case). Clear that so the move goes straight through.
  await clickAllChecked(".split-route input");
  await click("Add move to draft", ".sticky-actions");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes(${JSON.stringify(`Moved ${fx.stop.name}`)})`, "a third toast confirms the move");
  await waitFor(`document.getElementById("panel").innerText.includes("In draft")`, "the review with all three actions");
  const afterMove = await api(`position-reviews/${fx.review_id}`);
  check(afterMove.draft_actions?.length === 3 && afterMove.draft_actions.filter((a) => a.kind === "move").length === 1, `the draft now has 2 splits and a move (${afterMove.draft_actions?.length})`);
  check(await evaluate(`document.getElementById("review-move").disabled && document.getElementById("why-move").innerText.includes("already moved")`), "Move is off now: already moved in this draft");
  check(await evaluate(`document.getElementById("why-split")?.innerText.includes("already moved") ?? true`), "Split is off too, for the same reason");
  await shot("24-coordinate-actions");

  // a stop merged away leaves nothing to move or split
  const eRoutes = new Set(revE.routes.map((l) => l.route_id));
  let mergeInto = null;
  for (const s of (await api("feeds/chennai_bus/stops?limit=300")).items) {
    if (s.location_type !== 0 || !s.route_count || s.parent_station || touched.has(s.stop_id) || usedStops.has(s.stop_id)
      || pendingReviews.some((r) => r.stop_id === s.stop_id)) continue;
    const d = await api(`feeds/chennai_bus/stops/${s.stop_id}`);
    if (d.routes.some((r) => eRoutes.has(r.route_id) || touchedRoutes.has(r.route_id))) continue;
    mergeInto = d;
    break;
  }
  check(!!mergeInto, "set-up: a stop to merge a reviewed stop into");
  await clickSel("#draft-chip", "the draft chip");
  await chooseNewDraft("Smoke: merge a reviewed stop");
  const eMergeDraft = await activeDraftId();
  await go(`#/merge/${revE.stop_id}?with=${mergeInto.stop_id}`);
  await waitFor(`!!document.querySelector("table.compare")`, "the merge of a reviewed stop");
  await clickSel(`#keep-${mergeInto.stop_id}`, "keep the other stop's id");
  await click("Add merge to draft", ".sticky-actions");
  await click("Add merge to draft", "dialog");
  await waitFor(`document.getElementById("panel").innerText.includes("Merge added to your draft")`, "the merge in its own draft");
  // a merge is only live once its draft is committed; until then the review does
  // not yet see stop_merged_away (checked below, once the approver commits it)
  await go(`#/drafts/${eMergeDraft}`);
  await waitFor(`document.body.innerText.includes("Submit for review")`, "the merge draft page");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "the merge draft submitted");

  // the draft reads naturally
  await go(`#/drafts/${coordDraft}`);
  await waitFor(`document.querySelectorAll(".change").length === 2`, "the coordinates draft: the two moves");
  const moveOf = (review) => `[...document.querySelectorAll(".change")].find((a) => a.querySelector('a[href="#/coordinates/${review.review_id}"]'))`;
  const movedBy = distance(metres(aNow.lat, aNow.lon, suggestion.lat, suggestion.lon));
  check(await evaluate(`(${moveOf(revA)}?.querySelector("h3").innerText || "").includes(${JSON.stringify(`${movedBy} coordinate review`)})`), `a move from a review reads "Moved ${revA.stop.name} ${movedBy}" (coordinate review)`);
  check(await evaluate(`!!${moveOf(revA)}?.querySelector('a[href="#/coordinates/${revA.review_id}"]')`), "the move links back to its review");
  await evaluate(`${moveOf(revA)}.scrollIntoView(); true`);
  await waitFor(`!!${moveOf(revA)}.querySelector(".inset.leaflet-container")`, "the before and after map of the move");
  check(true, "the move has a before and after map inset");
  await evaluate(`[...${moveOf(revB)}.querySelectorAll("button")].find((b) => b.textContent === "Remove from draft").click(); true`);
  await waitFor(`document.querySelector("dialog")?.innerText.includes("coordinate review goes back")`, "the remove question for a move from a review");
  await click("Remove change", "dialog");
  await waitFor(`document.querySelectorAll(".change").length === 1`, "the draft without that move");
  check((await api(`position-reviews/${revB.review_id}`)).status === "pending", "removing a move from its draft puts the review back to review");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "the coordinates draft submitted");

  // the THANDALAM draft: two splits and a move
  await go(`#/drafts/${fxDraft}`);
  await waitFor(`document.querySelectorAll(".change").length === 2 + ${splitIdsA.length} + ${splitIdsB.length} + 1`, "the THANDALAM draft: 2 stop/create, their route lists, and the move");
  const fxDraftText = await text("#page");
  check(fxDraftText.includes(`Takes the place of ${fx.stop.name} (${fx.stop_id}) on ${splitIdsA.length} routes`) || fxDraftText.includes(`Takes the place of ${fx.stop.name} (${fx.stop_id}) on ${splitIdsB.length} routes`),
    "the split's new stop says which routes it takes from which stop");
  check(new RegExp(`switched to [^\\n]*\\(${splitStopIdA}\\), new in this draft`).test(fxDraftText) || new RegExp(`switched to [^\\n]*\\(${splitStopIdB}\\), new in this draft`).test(fxDraftText),
    "each route stop list of a split reads as a stop switched to the new one");
  await shot("26-coordinates-draft");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "the THANDALAM draft submitted");

  // ---- bulk import, into a draft of its own
  // ---- bulk import, into a draft of its own
  await clickSel("#draft-chip", "the draft chip");
  await chooseNewDraft("Smoke: import");
  await clickSel("#new-menu summary", "the New menu");
  await click("Import from a CSV file", "#new-menu");
  await waitFor(`!!document.getElementById("import-file")`, "import page");
  const template = await evaluate(`fetch(document.querySelector('a[download="stops-template.csv"]').href).then((r) => r.text())`);
  check(template.includes("action,stop_id,name,lat,lon,platform_code"), "the stops template downloads with its header row");
  // a row that cannot be read is caught in the browser
  await setFile("stops.csv", "action,stop_id,name,lat,lon,platform_code\nadd,,Smoke A,13.1,eighty,\n");
  await waitFor(`document.getElementById("page").innerText.includes("cannot be read")`, "unreadable row");
  check((await text("#page")).includes("lon must be a number"), "the row that cannot be read says what is wrong");
  // a misspelt column
  await setFile("stops.csv", "action,stop_id,name,lat,lon,platfrom_code\nadd,,Smoke A,13.1,80.1,x\n");
  await waitFor(`document.getElementById("page").innerText.includes("Did you mean")`, "unknown column");
  // server errors: an id in use and a duplicate in the file
  await setFile("stops.csv", "action,stop_id,name,lat,lon,platform_code\nadd,de9014549c,Smoke taken,12.91,80.11,\nadd,smk_dup,Smoke dup 1,12.911,80.111,\nadd,smk_dup,Smoke dup 2,12.912,80.112,\n");
  await waitFor(`document.getElementById("page").innerText.includes("have errors")`, "dry run with errors");
  check(await evaluate(`[...document.querySelectorAll(".actionbar button")].find((b) => b.textContent.startsWith("Add"))?.disabled === true`), "adding is off while rows have errors");
  check((await text(".result-table")).includes("already exists") && (await text(".result-table")).includes("same stop id"), "the table says what to fix on each row");
  // the fixed file: 1,200 stops, one of them close to a same-named stop
  const lines = ["action,stop_id,name,lat,lon,platform_code"];
  for (let i = 0; i < 1199; i++) lines.push(`add,,Smoke import ${String(i).padStart(4, "0")},${(12.9 + Math.floor(i / 40) * 0.001).toFixed(6)},${(80.1 + (i % 40) * 0.001).toFixed(6)},`);
  lines.push(`add,,"SIVAN TEMPLE-1",${sivan.lat + 0.0001},${sivan.lon},"Towards Luz, north"`);
  await setFile("stops-fixed.csv", lines.join("\r\n") + "\r\n");
  await waitFor(`document.getElementById("page").innerText.includes("can be added")`, "dry run of the fixed file", 20000);
  const summary = await text(".summary-chips");
  check(summary.includes("1,200 rows") && summary.includes("1 warning") && summary.includes("0 errors"), `the fixed file checks clean with one warning (${summary.replace(/\n/g, " ")})`);
  check(await waitFor(`!!document.querySelector(".import-map.leaflet-container, .import-map .leaflet-container")`, "the map of uploaded stops"), "the uploaded stops are on a map");
  await shot("16-import-preview");
  await click("Add 1,200 changes to draft");
  await click("Add 1,200 to draft", "dialog");
  await waitFor(`document.getElementById("page").innerText.includes("Added 1,200 changes")`, "import added", 30000);
  await click("Open the draft");
  await waitFor(`document.querySelectorAll(".change").length > 0`, "the draft of 1,200 changes", 20000);
  check((await evaluate(`document.querySelectorAll(".change").length`)) === 50, "the draft page shows 50 changes at a time");
  check(await evaluate(`document.querySelector(".pager select")?.options.length === 24`), "the draft page is paged, 24 pages");
  await choose(".pager select", "24");
  await waitFor(`document.querySelector(".pager select").value === "24"`, "last page");
  check(await evaluate(`document.querySelectorAll(".change").length === 50 && document.getElementById("page").innerText.includes("Showing 1,151 to 1,200")`), "the last page shows the last 50 changes");
  // the API answers it 200 changes at a time (section 16.5), and the page above
  // paged through all of them
  const bigDraft = (await evaluate("location.hash")).split("/").pop();
  const firstPage = await api(`change-sets/${bigDraft}`);
  check(firstPage.changes.length === 200 && !!firstPage.next_cursor && firstPage.change_count === 1200
    && firstPage.validation_summary && Array.isArray(firstPage.validation), "the API answers a big draft 200 changes at a time, with its count and the replay on the first page");
  const lastPage = await api(`change-sets/${bigDraft}?limit=500&cursor=1000`);
  check(lastPage.changes.length === 200 && lastPage.next_cursor === null && lastPage.validation === undefined, "a later page carries only its changes");
  await shot("17-big-draft");

  // ================================================================ approver
  // approve + commit draft 2, then approve draft 1 and hit the conflict; then the stations draft
  await signIn("approver1@nammayatri.in");
  for (const id of [draft2, draftId, newThingsDraft]) {
    await go(`#/drafts/${id}`);
    await waitFor(`document.body.innerText.includes("Approve")`, `approve button on ${id}`);
    await click("Approve");
    await click("Approve", "dialog");
    await waitFor(`document.body.innerText.includes("Commit and go live")`, `commit button on ${id}`);
    await click("Commit and go live");
    await click("Commit and go live", "dialog");
    await sleep(1200);
    if (id === draftId) {
      check((await text("#page")).includes("overtaken by another commit"), "commit conflict is explained");
      await shot("18-conflict");
    }
  }
  check((await text("#page")).includes("is live"), "the draft with new things and stations is committed");
  check((await api("station-proposals/3")).status === "committed", "a committed suggestion is marked live");
  await go("#/stations");
  await waitFor(`document.querySelector(".count-tabs")?.innerText.includes("Live")`, "stations list for the approver");
  check(/Live\s*\d/.test(await text(".count-tabs")) && !(await text(".count-tabs")).match(/Live\s*0\b/), "the stations list counts live suggestions");
  await go("#/route/SMOKE-R1");
  await waitFor(`document.getElementById("panel").innerText.includes("S1")`, "the committed new route");
  check((await text("#panel")).includes("nightly GTFS build"), "the committed new route says it has no trips until the nightly build");

  // the coordinates and THANDALAM drafts, and the merge, go live
  for (const cid of [coordDraft, fxDraft, eMergeDraft]) {
    await go(`#/drafts/${cid}`);
    await waitFor(`document.body.innerText.includes("Approve")`, `approve button on ${cid}`);
    await click("Approve");
    await click("Approve", "dialog");
    await waitFor(`document.body.innerText.includes("Commit and go live")`, `commit button on ${cid}`);
    await click("Commit and go live");
    await click("Commit and go live", "dialog");
    await waitFor(`document.getElementById("page").innerText.includes("is live")`, `${cid} committed`);
  }
  check((await api(`position-reviews/${revA.review_id}`)).status === "committed", "committing the draft marks the review of the move committed");
  check((await api(`position-reviews/${fx.review_id}`)).status === "committed", "committing the draft marks the review of the splits and move committed");
  const liveStopA = await api(`feeds/chennai_bus/stops/${splitStopIdA}`);
  check([...new Set((liveStopA.routes || []).map((r) => r.route_id))].sort().join() === splitIdsA.join(), `group A's routes now call at the new stop ${splitStopIdA}`);
  const liveStopB = await api(`feeds/chennai_bus/stops/${splitStopIdB}`);
  check([...new Set((liveStopB.routes || []).map((r) => r.route_id))].sort().join() === splitIdsB.join(), `group B's routes now call at the new stop ${splitStopIdB}`);
  const liveFx = await api(`feeds/chennai_bus/stops/${fx.stop_id}`);
  // a map click's pixel rounds to a slightly different point than the exact
  // target; a few metres of tolerance covers that, not the underlying position
  check(metres(liveFx.lat, liveFx.lon, target.lat, target.lon) < 20, `the original stop's own routes moved to the fixed position (${metres(liveFx.lat, liveFx.lon, target.lat, target.lon).toFixed(1)} m off)`);

  // now the merge is actually live, so the review of the stop it took shows why
  await go(`#/coordinates/${revE.review_id}`);
  await waitFor(`document.getElementById("panel").innerText.includes("cannot be moved or split")`, "the review of a stop merged away");
  check(await evaluate(`document.getElementById("review-move").disabled && document.getElementById("why-move").innerText.includes("gone")`), "a stop merged away cannot be moved, and the page says why");
  check(await has(`#panel a[href="#/stop/${mergeInto.stop_id}"]`), "the review links to the stop that stayed");
  await shot("25-coordinate-merged-away");

  // ================================================================ admin
  await signIn("admin@nammayatri.in");
  await go("#/admin");
  await waitFor(`document.querySelectorAll("tbody tr").length >= 6`, "people table");
  await type("#new-user-email", "ops.person@nammayatri.in");
  await click("Add person");
  await waitFor(`document.body.innerText.includes("ops.person@nammayatri.in")`, "added person");
  await shot("19-people");
  await go("#/audit");
  await waitFor(`document.body.innerText.includes("Committed (live)")`, "history shows the commit");
  const history = await text("#page");
  check(history.includes("Approved a suggested station into a draft") && history.includes("Imported a CSV file"), "history names the new actions");
  for (const label of ["Moved a stop from a coordinate review into a draft", "Split routes off a stop from a coordinate review into a draft",
    "Confirmed a stop's position", "Reopened a coordinate review", "A coordinate review went back to review", "A fix from a coordinate review went live"]) {
    check(history.includes(label), `history says "${label}"`);
  }
  await shot("20-history");

  // ================================================================ feed settings (admin only)
  check(await evaluate(`document.querySelector('[data-nav="feed-settings"]').offsetParent !== null`), "an admin sees the Feed settings nav entry");
  await go("#/feed-settings");
  await waitFor(`document.body.innerText.includes("Feed settings")`, "feed settings page");
  await waitFor(`document.querySelectorAll("#page table tbody tr").length >= 1`, "feed settings table");
  const beforeConfig = await api("feeds/chennai_bus/config");
  check(["db", "preprocessed"].includes(beforeConfig.data_source), `chennai_bus config readable (${beforeConfig.data_source})`);

  // ---- policy: the switch goes through a draft (docs section 3, "Feed data source")
  check(Array.isArray(beforeConfig.pending) && beforeConfig.pending.length === 0, "the feed config lists no pending draft yet");
  check((await text("#page")).includes("approved by someone else and committed"), "the page says a switch goes through a draft");
  const directWrite = await apiSend("POST", "feeds/chennai_bus/config", { data_source: "db" });
  check(directWrite.status === 404 && directWrite.body?.error?.code === "endpoint_not_found", "the direct POST of a feed's config is gone");
  await click("Add to draft: switch to");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("only after the draft is submitted, approved by someone else and committed")`, "the confirm dialog says the switch waits for submit, approval and commit");
  check(!(await text("dialog")).includes("immediately"), "the confirm dialog no longer says the switch is immediate");
  await click("Add to draft", "dialog");
  await chooseNewDraft("Smoke: switch the feed's data source");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes("Added to")`, "a toast says the switch went into the draft");
  const configDraft = await activeDraftId();
  const drafted = await api("feeds/chennai_bus/config");
  check(drafted.data_source === beforeConfig.data_source && drafted.version === beforeConfig.version, "drafting the switch changed nothing live");
  check(drafted.pending.length === 1 && drafted.pending[0].change_set_id === configDraft && drafted.pending[0].status === "draft"
    && drafted.pending[0].data_source !== beforeConfig.data_source, "the feed config lists the draft as pending");
  await waitFor(`!!document.querySelector('#page .feed-pending a[href="#/drafts/${configDraft}"]')`, "the feed settings row links to the draft carrying the switch");
  await shot("21-feed-settings");
  const asEditor = await apiSend("PUT", `change-sets/${configDraft}/changes/${drafted.pending[0].change_id}`, { after: { data_source: "nonsense" } });
  check(asEditor.status === 400 && asEditor.body?.error?.details?.code === "invalid_data_source", "a bad data source is refused as an invalid change");
  await go(`#/drafts/${configDraft}`);
  await waitFor(`document.getElementById("page").innerText.includes("Data source of feed chennai_bus")`, "the draft shows the data source switch");
  check((await text("#page")).includes("Served from") && (await text("#page")).includes("keeps serving this feed as it does now"), "the change reads as a before and after that waits for the commit");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`!!document.getElementById("self-approve")`, "the submitted draft, for the admin who submitted it");

  // ---- policy: admin self-approval (docs section 2)
  check(await evaluate(`[...document.querySelectorAll("#page .actionbar button")].some((b) => b.textContent.trim() === "Approve" && b.disabled)`), "the ordinary Approve stays off for the submitter, admin or not");
  check(await evaluate(`(() => { const b = document.getElementById("self-approve"); return b.textContent.includes("Approve it myself (admin override)") && b.classList.contains("danger"); })()`), "an admin sees a separate override button on their own draft, not the main action");
  const noFlag = await apiSend("POST", `change-sets/${configDraft}/approve`, {});
  check(noFlag.status === 403 && noFlag.body?.error?.code === "own_change_set" && noFlag.body.error.details.can_self_approve === true, "without self_approve the admin is refused, and told the override exists");
  await clickSel("#self-approve", "Approve it myself (admin override)");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("You submitted this draft. Approving it yourself skips the second reviewer. Continue?")`, "the override asks first, in the agreed words");
  await click("Approve it myself", "dialog");
  await waitFor(`!!document.querySelector("#page .page-head .chip.self-approved")`, "the draft's header carries a self-approved badge");
  check((await text("#page .timeline")).includes("admin override"), "the draft's timeline says it was an admin override");
  const approvedSelf = await api(`change-sets/${configDraft}`);
  check(approvedSelf.status === "approved" && approvedSelf.self_approved === true, "the draft is approved and marked self_approved");
  check((await api("feeds/chennai_bus/config")).data_source === beforeConfig.data_source, "approval alone switches nothing");
  await go("#/drafts?status=approved");
  await waitFor(`!!document.querySelector("#page tbody .chip.self-approved")`, "the drafts list marks the self-approved draft");
  await go(`#/drafts/${configDraft}`);
  await waitFor(`document.body.innerText.includes("Commit and go live")`, "commit button for the admin's self-approved draft");
  await click("Commit and go live");
  await click("Commit and go live", "dialog");
  await waitFor(`document.getElementById("page").innerText.includes("is live")`, "the self-approved draft committed by the same admin");
  const afterConfig = await api("feeds/chennai_bus/config");
  check(afterConfig.data_source !== beforeConfig.data_source, `chennai_bus data_source flipped at commit (${beforeConfig.data_source} -> ${afterConfig.data_source})`);
  check(afterConfig.version === beforeConfig.version + 1, "the commit bumped the feed version once");
  check(afterConfig.pending.length === 0, "nothing is pending once committed");
  await go("#/audit");
  await waitFor(`document.body.innerText.includes("Approved their own draft (admin override")`, "history names the self-approval for what it was");
  check(await evaluate(`(() => { const tr = document.querySelector("#page tr.audit-override"); return !!tr && !!tr.querySelector(".chip.override") && tr.innerText.includes("no second reviewer"); })()`), "the self-approval row stands out in the history, with a badge");
  const policyHistory = await text("#page");
  check(policyHistory.includes("Changed a feed's data source"), "history shows the data source change, written at commit");
  check(policyHistory.includes("self-approved (admin override)"), "the commit's history row says it was self-approved");
  await shot("21b-self-approved-history");

  // nobody but an admin has the override
  await signIn("approver1@nammayatri.in");
  const ownSet = (await apiSend("POST", "feeds/chennai_bus/change-sets", { title: "Smoke: an approver's own draft" })).body;
  const anyStop = (await api("feeds/chennai_bus/stops?limit=1")).items[0];
  await apiSend("POST", `change-sets/${ownSet.change_set_id}/changes`, { entity: "stop", op: "update", entity_key: anyStop.stop_id, after: { platform_code: "Smoke kerb" } });
  await apiSend("POST", `change-sets/${ownSet.change_set_id}/submit`);
  const notAdmin = await apiSend("POST", `change-sets/${ownSet.change_set_id}/approve`, { self_approve: true });
  check(notAdmin.status === 403 && notAdmin.body?.error?.code === "own_change_set" && notAdmin.body.error.details.can_self_approve === false, "an approver cannot self-approve, flag or not");
  await go(`#/drafts/${ownSet.change_set_id}`);
  await waitFor(`document.getElementById("page").innerText.includes("Smoke: an approver's own draft")`, "the approver's own submitted draft");
  check(!(await has("#self-approve")), "a non-admin never sees the override button");
  await apiSend("POST", `change-sets/${ownSet.change_set_id}/discard`);

  // a non-admin never sees the nav entry, or the page's controls by address
  await signIn("editor1@nammayatri.in");
  check(await evaluate(`document.querySelector('[data-nav="feed-settings"]').offsetParent === null`), "a non-admin never sees the Feed settings nav entry");
  await go("#/feed-settings");
  await waitFor(`document.body.innerText.includes("Feed settings")`, "feed settings page reachable by address for a non-admin");
  check(!(await has("#page table")), "a non-admin sees no feed settings table or switch controls");
  check((await text("#page")).includes("Only admins can change"), "a non-admin sees the admin-only notice instead");
  await signIn("admin@nammayatri.in");

  // with every station suggestion superseded (as on master), the top bar hides the link
  const superseded = await evaluate(`fetch("/__dev/supersede-station-proposals", { method: "POST", credentials: "same-origin" }).then((r) => r.json())`);
  check(superseded.superseded > 0, `set-up: every open station suggestion superseded (${superseded.superseded})`);
  await load(UI);
  await waitFor(`!document.getElementById("app").hidden && performance.getEntriesByType("resource").some((e) => e.name.includes("/station-proposals/summary"))`, "the top bar after reloading");
  await sleep(500);
  check(await evaluate(`document.querySelector('[data-nav="stations"]').offsetParent === null`), "with nothing to review or in a draft, the top bar hides Stations to review");
  check(await evaluate(`document.querySelector('[data-nav="coordinates"]').offsetParent !== null`), "Coordinates to review stays in the top bar");
  await go("#/stations");
  await waitFor(`document.getElementById("panel").innerText.includes("Nothing is waiting for review")`, "the stations page by its address");
  check(await evaluate(`document.querySelector('[data-nav="stations"]').offsetParent === null`), "#/stations still opens while its link is hidden");

  // ================================================================ round 4 (UX): see round4Flows above
  await round4Flows();

  // ================================================================ round 5 (stop details, station links): see round5Flows above
  await round5Flows();

  // ================================================================ delivery (pods, webhooks): see deliveryFlows above
  await deliveryFlows();
  // ================================================================ merging two stations: see stationMergeFlows above
  await stationMergeFlows();
  // ================================================================ feed access: see feedAccessFlows above
  await feedAccessFlows();

  // ================================================================ map lines through the stops and from GPS: see mapLineFlows above
  await mapLineFlows();

  check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
} catch (e) {
  failures.push(e.message);
  console.log(`FAIL ${e.message}`);
} finally {
  try { ws?.close(); } catch { /* ignore */ }
  chrome.kill();
  await sleep(800);
  if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
}
console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
process.exit(failures.length ? 1 : 0);
