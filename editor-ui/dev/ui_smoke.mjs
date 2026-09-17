// End-to-end smoke test of the dashboard against dev/mock_server.py, driving a
// headless Chrome over the DevTools protocol (Node 22+, no dependencies).
//
//   python dev/mock_server.py &            # port 8765, fresh state for every run
//   node dev/ui_smoke.mjs [--shots /tmp/gtfs-editor-shots]
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
// away); importing CSV files into a draft of 1,200 changes; submit, approve and
// commit by a second person, a commit conflict, people and history; the
// "Stations to review" link hiding once nothing is left to review.
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
  await click("Suggest a map line");
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
  await clickMapAt(gateA.lat, gateA.lon);
  await waitFor(`!!document.getElementById("platform-CHNS03131")`, "first member from a map click");
  await clickMapAt(gateB.lat, gateB.lon);
  await waitFor(`!!document.getElementById("platform-CHNS06298")`, "second member from a map click");
  check((await evaluate(`document.getElementById("station-id").value`)) === "stn_CHNS03131", "the station id is made from the first stop");
  // finding a stop by name, then giving up, leaves clicking on the map working
  await click("Find a stop to add", "#panel");
  await waitFor(`!!document.querySelector("#panel .picker input")`, "the station's stop finder");
  await click("Cancel", "#panel .picker");
  await clickMapAt(gateB.lat, gateB.lon);
  await waitFor(`!document.getElementById("platform-CHNS06298")`, "a map click takes a stop out again");
  await clickMapAt(gateB.lat, gateB.lon);
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
  await click("Compare", "#panel");
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
  check(template.includes("stop_id,name,lat,lon,platform_code"), "the stops template downloads with its header row");
  // a row that cannot be read is caught in the browser
  await setFile("stops.csv", "stop_id,name,lat,lon,platform_code\n,Smoke A,13.1,eighty,\n");
  await waitFor(`document.getElementById("page").innerText.includes("cannot be read")`, "unreadable row");
  check((await text("#page")).includes("lon must be a number"), "the row that cannot be read says what is wrong");
  // a misspelt column
  await setFile("stops.csv", "stop_id,name,lat,lon,platfrom_code\n,Smoke A,13.1,80.1,x\n");
  await waitFor(`document.getElementById("page").innerText.includes("Did you mean")`, "unknown column");
  // server errors: an id in use and a duplicate in the file
  await setFile("stops.csv", "stop_id,name,lat,lon,platform_code\nde9014549c,Smoke taken,12.91,80.11,\nsmk_dup,Smoke dup 1,12.911,80.111,\nsmk_dup,Smoke dup 2,12.912,80.112,\n");
  await waitFor(`document.getElementById("page").innerText.includes("have errors")`, "dry run with errors");
  check(await evaluate(`[...document.querySelectorAll(".actionbar button")].find((b) => b.textContent.startsWith("Add"))?.disabled === true`), "adding is off while rows have errors");
  check((await text(".result-table")).includes("already exists") && (await text(".result-table")).includes("same stop id"), "the table says what to fix on each row");
  // the fixed file: 1,200 stops, one of them close to a same-named stop
  const lines = ["stop_id,name,lat,lon,platform_code"];
  for (let i = 0; i < 1199; i++) lines.push(`,Smoke import ${String(i).padStart(4, "0")},${(12.9 + Math.floor(i / 40) * 0.001).toFixed(6)},${(80.1 + (i % 40) * 0.001).toFixed(6)},`);
  lines.push(`,"SIVAN TEMPLE-1",${sivan.lat + 0.0001},${sivan.lon},"Towards Luz, north"`);
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
  await click("Switch to");
  await waitFor(`document.querySelector("dialog")?.innerText.includes("takes effect immediately")`, "the confirm dialog warns this is immediate, with no draft");
  await click("Switch", "dialog");
  await waitFor(`document.querySelector("#toasts .toast:last-child")?.textContent.includes("now serves from")`, "a toast confirms the switch");
  const afterConfig = await api("feeds/chennai_bus/config");
  check(afterConfig.data_source !== beforeConfig.data_source, `chennai_bus data_source flipped (${beforeConfig.data_source} -> ${afterConfig.data_source})`);
  check(afterConfig.version === beforeConfig.version + 1, "the switch bumped the feed version");
  await shot("21-feed-settings");

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
