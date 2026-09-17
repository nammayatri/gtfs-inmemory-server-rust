// End-to-end test of the REAL dashboard against a REAL local GIMS, through the
// dev Pomerium stand-in, driving headless Chrome over the DevTools protocol
// (Node 22+, no dependencies). Local only.
//
//   node scripts/dev_pomerium_proxy.mjs --listen 18080 --upstream http://127.0.0.1:18010 \
//        --audience gtfs.editor.localhost --jwks-out /tmp/jwks.json &
//   DHALL_CONFIG=<config with gtfs_db_feeds=["chennai_bus"], the editor enabled,
//        jwks file://..., audience gtfs.editor.localhost, bootstrap admin
//        admin@nammayatri.in, poll 3 s> target/debug/gtfs-routes-service &
//   PSQL=psql node editor-ui/dev/ui_e2e.mjs [--shots DIR] [--only map,stations,...]
//
// It needs a freshly seeded local database: it enrols admin@nammayatri.in and
// commits real changes to chennai_bus. E2E_DATABASE_URL (default the local
// mtc_internal_master) is read with psql to check committed rows; any other
// host is refused.
//
// Core flow: no identity -> admin enrols -> admin adds an editor and an approver
// -> the editor edits a route's stop order, moves and renames a stop, clubs two
// stops into a station, submits -> the editor cannot approve -> the approver
// reviews, approves, commits -> the change is live in the PUBLIC GIMS APIs
// without a restart -> a stale draft's commit shows the conflict panel.
//
// Then one flow per screen, each drafted by the editor and approved and
// committed by the approver, each checked in the database or the public APIs:
//   map       labels from zoom 17, clicking a stop marker while a route is open
//   stations  review a suggested station: rename and relabel, approve into a
//             draft, reject another with a note; live in /station-children
//   create    New stop (id minted, placed on the map), New route with a two-stop
//             list built with the stop picker
//   route     Change stop by search, a stage name from the list; a fare-rule
//             error blocks submitting; renumber fixes it
//   merge     two same-named stops at one point: choose the id that stays
//   import    a stops CSV with one bad row, then the fixed file
import { spawn, spawnSync } from "node:child_process";
import { createHmac } from "node:crypto";
import { mkdirSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const PROXY = process.env.PROXY || "http://localhost:18080";
const GIMS = process.env.GIMS || "http://127.0.0.1:18010";
const UI_PATH = "/internal/gtfs-editor/ui/";
const MAPJS = `${PROXY}${UI_PATH}js/map.js`;
const FEED = "chennai_bus";
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const CDP_PORT = Number(process.env.CDP_PORT || 9334);
const PSQL = process.env.PSQL || "psql";
const DB_URL = process.env.E2E_DATABASE_URL || "postgres://postgres@127.0.0.1:55432/mtc_internal_master";
const arg = (name) => { const i = process.argv.indexOf(name); return i > 0 ? process.argv[i + 1] : null; };
const SHOTS = arg("--shots") || join(tmpdir(), "gtfs-editor-e2e-shots");
const ONLY = arg("--only") ? new Set(arg("--only").split(",")) : null;
mkdirSync(SHOTS, { recursive: true });
if (!/@(127\.0\.0\.1|localhost)[:/]/.test(DB_URL)) {
  console.log("FAIL refusing to read a non-local database");
  process.exit(2);
}

const ADMIN = "admin@nammayatri.in", EDITOR = "editor1@nammayatri.in", APPROVER = "approver1@nammayatri.in";
const ROUTE = "1369", STOP = "de9014549c", CLUB = "68e3cdc2e0", CONTESTED = "13f0ef7b8d";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const timings = {};
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failures.push(what); return ok; };
class Abort extends Error {}
// a wait the rest of a flow cannot do without
const must = async (p) => { if (!(await p)) throw new Abort("stopping this flow"); };

// ------------------------------------------------------------------ TOTP
const secrets = {}, lastStep = {};
function totpAt(secret, step) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  let bits = "";
  for (const c of secret.replace(/\s|=/g, "").toUpperCase()) bits += alphabet.indexOf(c).toString(2).padStart(5, "0");
  const key = Buffer.from(bits.match(/.{8}/g).map((b) => parseInt(b, 2)));
  const msg = Buffer.alloc(8);
  msg.writeBigUInt64BE(BigInt(step));
  const mac = createHmac("sha1", key).update(msg).digest();
  const off = mac[mac.length - 1] & 15;
  return String((mac.readUInt32BE(off) & 0x7fffffff) % 1000000).padStart(6, "0");
}
// A code for `email` from a time step it has not used yet (the server rejects replays).
async function freshCode(email) {
  for (;;) {
    const step = Math.floor(Date.now() / 30000);
    if (lastStep[email] === undefined || step > lastStep[email]) {
      lastStep[email] = step;
      return totpAt(secrets[email], step);
    }
    await sleep(1000);
  }
}

// ------------------------------------------------------------------ CDP
const profile = mkdtempSync(join(tmpdir(), "gtfs-editor-e2e-chrome-"));
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
async function waitFor(expr, what, timeout = 10000) {
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
async function load(url) { await send("Page.navigate", { url }); await sleep(1500); }
async function click(text, scope = "body") {
  const ok = await evaluate(`(() => {
    const els = [...document.querySelectorAll(${JSON.stringify(scope)} + " button, " + ${JSON.stringify(scope)} + " a")]
      .filter((e) => e.offsetParent !== null && !e.disabled && e.textContent.trim().includes(${JSON.stringify(text)}));
    if (!els.length) return false;
    els[0].click();
    return true;
  })()`);
  check(ok, `click "${text}"`);
  await sleep(450);
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
  await sleep(350);
}
async function choose(selector, value) {
  await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); el.value = ${JSON.stringify(value)};
    el.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`);
  await sleep(350);
}
const text = (sel = "body") => evaluate(`document.querySelector(${JSON.stringify(sel)})?.innerText || ""`);
const draftIdOf = (email) => evaluate(`JSON.parse(localStorage.getItem("gtfs-editor-prefs") || "{}")[${JSON.stringify(`draft:${email}:${FEED}`)}]`);
// the editor API as the signed-in person, from the page (cookies, mutation header)
const editorApi = (method, path, body) => evaluate(`fetch(${JSON.stringify(`/internal/gtfs-editor/${path}`)}, {
  method: ${JSON.stringify(method)}, credentials: "same-origin",
  headers: { "Content-Type": "application/json", "X-Requested-With": "gtfs-editor" },
  body: ${body === undefined ? "undefined" : JSON.stringify(JSON.stringify(body))},
}).then(async (r) => ({ status: r.status, body: await r.json().catch(() => null) }))`);

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
// a label being taken off fades out (opacity 0) before it leaves the page
const shownLabels = (cls) => `[...document.querySelectorAll(".leaflet-tooltip.${cls}")].filter((e) => e.style.opacity !== "0")`;
async function mouseClick(x, y) {
  await send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
  await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
  await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
  await sleep(500);
}
async function clickMapAt(lat, lon) {
  const p = await mapCall(`const q = map.latLngToContainerPoint([${lat}, ${lon}]); const r = map.getContainer().getBoundingClientRect(); return { x: r.left + q.x, y: r.top + q.y };`);
  await mouseClick(p.x, p.y);
}
async function chooseNewDraft(title) {
  await must(waitFor(`document.querySelector("dialog #new-draft-title") !== null`, `draft chooser for "${title}"`));
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
  await load(`${PROXY}/.dev-pomerium/as?email=${encodeURIComponent(email)}&next=${encodeURIComponent(UI_PATH)}`);
}
// Sign in through the real gate: enrol on first sight, otherwise a fresh code.
async function signIn(email) {
  await actAs(email);
  await waitFor(`!!document.querySelector(".qr") || !!document.querySelector(".code-input") || !document.getElementById("app").hidden`, `gate or app for ${email}`);
  if (await evaluate(`!!document.querySelector(".qr")`)) {
    secrets[email] = (await text(".secret")).replace(/\s/g, "");
    await shot(`01-enrol-${email.split("@")[0]}`);
    await type(".code-input", await freshCode(email));
  } else if (await evaluate(`!!document.querySelector(".code-input")`)) {
    await type(".code-input", await freshCode(email));
  }
  await waitFor(`!document.getElementById("app").hidden`, `app after sign-in as ${email}`);
  await sleep(700);
}
let signedInAs = null;
async function become(email) {
  if (signedInAs !== email) await signIn(email);
  signedInAs = email;
}
// Each flow drafts into a draft of its own: forget the draft this browser was
// editing into, and start from the map.
async function freshStart(email) {
  await become(email);
  await evaluate(`(() => { const k = "gtfs-editor-prefs"; const p = JSON.parse(localStorage.getItem(k) || "{}");
    for (const key of Object.keys(p)) if (key.startsWith("draft:")) delete p[key];
    localStorage.setItem(k, JSON.stringify(p)); return true; })()`);
  await load(`${PROXY}${UI_PATH}`);
  await must(waitFor(`!document.getElementById("app").hidden && document.getElementById("draft-chip").textContent.includes("No draft")`, `the dashboard for ${email} with no draft open`));
}

async function gims(path) {
  const r = await fetch(GIMS + path);
  return r.ok ? r.json() : null;
}
// poll until fn() is truthy; resolves with its value, or null after `ms`
async function until(fn, ms) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try { const v = await fn(); if (v) return v; } catch { /* retry */ }
    await sleep(250);
  }
  return null;
}
// rows from the local database, as arrays of text cells
function db(sql) {
  const r = spawnSync(PSQL, [DB_URL, "-XAtq", "-F", "\t", "-c", sql], { encoding: "utf8" });
  if (r.error || r.status !== 0) throw new Error(`psql: ${r.error ? r.error.message : r.stderr.trim()}`);
  return r.stdout.split("\n").filter(Boolean).map((l) => l.split("\t"));
}
const sqlText = (s) => `'${String(s).replace(/'/g, "''")}'`;

async function submitDraft(draftId, label) {
  await go(`#/drafts/${draftId}`);
  await must(waitFor(`[...document.querySelectorAll("button")].some((b) => b.textContent === "Submit for review" && !b.disabled)`, `Submit for review on ${label}`));
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await must(waitFor(`document.body.innerText.includes("Waiting for review")`, `${label} submitted`));
}
// as the approver: approve and commit; resolves with the time commit was confirmed
async function approveAndCommit(draftId, label) {
  await become(APPROVER);
  await go(`#/drafts/${draftId}`);
  await must(waitFor(`[...document.querySelectorAll("button")].some((b) => b.textContent === "Approve" && !b.disabled)`, `Approve on ${label}`));
  await click("Approve");
  await click("Approve", "dialog");
  await must(waitFor(`document.body.innerText.includes("Commit and go live")`, `Commit on ${label}`));
  await click("Commit and go live");
  const at = Date.now();
  await click("Commit and go live", "dialog");
  await must(waitFor(`document.body.innerText.includes("is live")`, `${label} committed`));
  return at;
}

// Runs one flow: its time, and a failure that stops only this flow.
async function flow(name, fn) {
  if (ONLY && !ONLY.has(name)) return;
  console.log(`\n== ${name}`);
  const started = Date.now();
  const before = failures.length;
  try {
    await fn();
  } catch (e) {
    if (!(e instanceof Abort)) check(false, `${name}: ${e.message}`);
    else console.log(`     (${name} stopped early)`);
  }
  timings[`${name}_ms`] = Date.now() - started;
  console.log(`== ${name}: ${failures.length === before ? "passed" : `${failures.length - before} failure(s)`} in ${((Date.now() - started) / 1000).toFixed(1)} s`);
}

// ------------------------------------------------------------------ core flow
async function coreFlow() {
  // the SSO host's / opens the dashboard
  await load(`${PROXY}/.dev-pomerium/as?email=&next=/`);
  await waitFor(`location.pathname === ${JSON.stringify(UI_PATH)}`, "/ redirects to the dashboard");
  await waitFor(`document.body.innerText.includes("Open the editor from its sign-in address")`, "no-identity message");
  check(true, "gate explains a missing SSO identity");
  await shot("00-no-identity");

  // a signed-in stranger
  await actAs("stranger@nammayatri.in");
  await waitFor(`document.body.innerText.includes("You have not been added yet")`, "not-registered message");
  check(true, "gate explains a missing editor account");

  // bootstrap admin enrols; a wrong code is explained with tries left
  await actAs(ADMIN);
  await must(waitFor(`!!document.querySelector(".qr")`, "admin enrolment QR (the local database must be freshly seeded)"));
  secrets[ADMIN] = (await text(".secret")).replace(/\s/g, "");
  await type(".code-input", "000000");
  await waitFor(`document.body.innerText.includes("more tries before sign-in pauses")`, "wrong code with tries left");
  check(true, "wrong code says how many tries are left");
  await type(".code-input", await freshCode(ADMIN));
  await waitFor(`!document.getElementById("app").hidden`, "app after admin enrolment");
  check(true, "bootstrap admin enrols with a real authenticator code");
  signedInAs = ADMIN;

  // admin adds the editor and the approver
  await go("#/admin");
  await waitFor(`!!document.getElementById("new-user-email")`, "people page");
  for (const [email, role] of [[EDITOR, "editor"], [APPROVER, "approver"]]) {
    await type("#new-user-email", email);
    await evaluate(`(() => { const s = document.getElementById("new-user-role"); s.value = ${JSON.stringify(role)}; s.dispatchEvent(new Event("change", {bubbles:true})); return true; })()`);
    await click("Add person");
    await waitFor(`document.body.innerText.includes(${JSON.stringify(email)})`, `${email} listed`);
  }
  await shot("02-people");
  if (ONLY && !ONLY.has("core")) {
    // enrol the other two, so the flows can sign them in
    await become(EDITOR);
    await become(APPROVER);
    return;
  }

  // ---- editor: route stop order, stop move + rename, station; submit
  await become(EDITOR);
  await shot("03-home");
  await type("#search-input", "45BET");
  await waitFor(`document.querySelectorAll(".search-item").length > 0`, "search results");
  await go(`#/route/${ROUTE}`);
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 3`, "stage ladder");
  check((await text("#panel")).includes("ALWARPET ANJANEYAR TEMPLE"), `route ${ROUTE} lists Alwarpet Anjaneyar Temple`);
  await sleep(1000);
  await shot("04-route");
  const before = await gims(`/route-stop-mapping/${FEED}/route/${ROUTE}`);

  await click("Edit stop list");
  await waitFor(`document.querySelector("dialog") !== null`, "draft chooser");
  await type("#new-draft-title", "E2E: Alwarpet kerbs and station");
  await click("Start draft", "dialog");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length > 10`, "stop list editor");
  const liveWarn = await text("#panel");
  check(!/to fix before submitting/.test(liveWarn), "an unedited route shows no blocking problems");
  // add a stop between stop 3 and stop 4 with the stop picker
  const anchor = await evaluate(`document.querySelector("#row-2 .row-sub .meta")?.textContent.split(" · ")[0].trim()`);
  await evaluate(`document.getElementById("add-3").click()`);
  await sleep(300);
  await type(".picker input", "PANAIYUR");
  await waitFor(`[...document.querySelectorAll(".picker-item")].some((b) => b.innerText.includes("PANAIYUR"))`, "stop picker results");
  const inserted = await evaluate(`[...document.querySelectorAll(".picker-item")].find((b) => b.innerText.includes("PANAIYUR")).dataset.stop`);
  const insertedName = await evaluate(`[...document.querySelectorAll(".picker-item")].find((b) => b.innerText.includes("PANAIYUR")).querySelector(".picker-name").textContent.trim()`);
  await evaluate(`[...document.querySelectorAll(".picker-item")].find((b) => b.innerText.includes("PANAIYUR")).click()`);
  await sleep(400);
  await shot("05-route-edit");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("draft applied") || document.body.innerText.includes("Show what is live now")`, "route shown with draft");
  check(true, `route ${ROUTE} stop list change (insert ${inserted}) added to the draft`);

  await go(`#/stop/${STOP}`);
  await waitFor(`document.body.innerText.includes("Routes stopping here")`, "stop panel");
  await click("Edit stop");
  await waitFor(`!!document.getElementById("stop-lat")`, "stop editor");
  await type("#stop-lat", "13.0385800");
  await type("#stop-name", "ALWARPET ANJANEYAR TEMPLE (TEYNAMPET SIDE)");
  await sleep(500);
  check((await text("#panel")).includes("Moved"), "stop editor shows the distance moved");
  await shot("06-stop-edit");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("changes this stop")`, "stop shows pending draft notice");

  await go(`#/stop/${CLUB}`);
  await waitFor(`document.body.innerText.includes("Club into a station")`, "stop with club action");
  await click("Club into a station");
  await waitFor(`!!document.getElementById("station-name")`, "station editor");
  await waitFor(`[...document.querySelectorAll("#panel button")].some(b => b.textContent === "Add")`, "nearby suggestions");
  const joined = await evaluate(`[...document.querySelectorAll("#panel .list-item")].find(li => [...li.querySelectorAll("button")].some(b => b.textContent === "Add"))?.querySelector(".sub")?.textContent.split(",")[0].trim()`);
  await click("Add", "#panel");
  await type("#station-name", "Alwarpet Anjaneyar Temple");
  const stationId = await evaluate(`document.getElementById("station-id").value`);
  await shot("07-station");
  await click("Add to draft", ".sticky-actions");
  await sleep(900);
  check(!(await text("#panel")).includes("Fix before submitting"), `station ${stationId} (${CLUB} + ${joined}) accepted`);

  const draftId = await draftIdOf(EDITOR);
  check(!!draftId, "the draft id is remembered for the editor");
  await go(`#/drafts/${draftId}`);
  await waitFor(`document.querySelectorAll(".change").length >= 3`, "draft page with 3 changes");
  const draftText = await text("#page");
  check(draftText.includes("added"), "route stop list diff renders");
  check(draftText.includes(`${insertedName} (${inserted})`), `the added stop is shown by name (${insertedName})`);
  check(draftText.includes("joins the station"), "station membership diff renders");
  check(draftText.includes("moved"), "stop move diff renders");
  await sleep(1200);
  await shot("08-draft-review");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "submitted status");
  const approveDisabled = await evaluate(`[...document.querySelectorAll("button")].find(b => b.textContent === "Approve")?.disabled`);
  check(approveDisabled === true, "editor cannot approve (button disabled with the reason)");

  // ---- approver: review, approve, commit; then the change is live in GIMS
  await become(APPROVER);
  await go(`#/drafts/${draftId}`);
  await waitFor(`document.body.innerText.includes("Approve")`, "approve button");
  await click("Approve");
  await click("Approve", "dialog");
  await waitFor(`document.body.innerText.includes("Commit and go live")`, "commit button");
  await click("Commit and go live");
  const committedAt = Date.now();
  await click("Commit and go live", "dialog");
  await waitFor(`document.body.innerText.includes("is live")`, "committed status");
  await shot("09-committed");

  let live = null;
  const deadline = committedAt + 15000;
  while (Date.now() < deadline) {
    const [stop, mapping, children] = await Promise.all([
      gims(`/stop/${FEED}/${STOP}`),
      gims(`/route-stop-mapping/${FEED}/route/${ROUTE}`),
      gims(`/station-children/${FEED}/${stationId}`),
    ]);
    const renamed = stop && stop.stopName === "ALWARPET ANJANEYAR TEMPLE (TEYNAMPET SIDE)" && Math.abs(stop.stopPoint.lat - 13.03858) < 1e-6;
    const ordered = Array.isArray(mapping) ? [...mapping].sort((a, b) => a.sequenceNum - b.sequenceNum) : [];
    const at = ordered.findIndex((m) => m.stopCode === inserted);
    const insertedLive = ordered.length === (before?.length || 0) + 1 && at > 0 && ordered[at - 1].stopCode === anchor;
    const kids = JSON.stringify(children || "");
    const stationLive = kids.includes(CLUB) && (!joined || kids.includes(joined));
    if (renamed && insertedLive && stationLive) { live = Date.now() - committedAt; break; }
    await sleep(250);
  }
  timings.core_commit_to_live_ms = live;
  check(live !== null, `public GIMS APIs show the rename+move, the inserted stop and the station ${live !== null ? `${live} ms after commit` : "(not within 15 s)"}`);

  // ---- a stale draft: two drafts edit the same stop; the second commit conflicts
  const stale = {};
  for (const [who, title, name] of [[EDITOR, "E2E: LUZ rename A", "LUZ A"], [ADMIN, "E2E: LUZ rename B", "LUZ B"]]) {
    await become(who);
    await go(`#/stop/${CONTESTED}`);
    await waitFor(`document.body.innerText.includes("Edit stop")`, `stop for ${who}`);
    await click("Edit stop");
    await waitFor(`document.querySelector("dialog") !== null || !!document.getElementById("stop-name")`, `draft chooser for ${who}`);
    if (await evaluate(`document.querySelector("dialog") !== null`)) {
      await type("#new-draft-title", title);
      await click("Start draft", "dialog");
    }
    await waitFor(`!!document.getElementById("stop-name")`, `stop editor for ${who}`);
    await type("#stop-name", name);
    await click("Add to draft", ".sticky-actions");
    await sleep(700);
    const id = await draftIdOf(who);
    await go(`#/drafts/${id}`);
    await waitFor(`document.body.innerText.includes("Submit for review")`, `draft page for ${who}`);
    await click("Submit for review");
    await click("Submit for review", "dialog");
    await waitFor(`document.body.innerText.includes("Waiting for review")`, `${title} submitted`);
    stale[who] = id;
  }
  await become(APPROVER);
  for (const id of [stale[ADMIN], stale[EDITOR]]) {
    await go(`#/drafts/${id}`);
    await waitFor(`document.body.innerText.includes("Approve")`, `approve button on ${id}`);
    await click("Approve");
    await click("Approve", "dialog");
    await waitFor(`document.body.innerText.includes("Commit and go live")`, `commit button on ${id}`);
    await click("Commit and go live");
    await click("Commit and go live", "dialog");
    await sleep(1200);
  }
  check((await text("#page")).includes("overtaken by another commit"), "the stale draft's commit shows the conflict panel");
  await shot("10-conflict");

  await go("#/audit");
  await waitFor(`document.body.innerText.includes("Committed (live)")`, "history shows the commit");
  await shot("11-history");
}

// ------------------------------------------------------------------ map
// Labels from zoom 17 in a busy area; clicking a stop marker switches the panel
// to that stop while a route is open, and while another stop is open.
async function mapFlow() {
  await freshStart(EDITOR);
  await go(`#/route/${ROUTE}`);
  await must(waitFor(`document.querySelectorAll(".ladder .stage").length > 3`, "route ladder"));
  // Alwarpet at zoom 18: stops and their names
  await setView(13.03866, 80.25891, 18);
  await must(waitStopDrawn(STOP));
  await waitFor(`${shownLabels("stop-label")}.length > 2`, "stop labels at zoom 18");
  await sleep(300);
  const labels18 = await evaluate(`${shownLabels("stop-label")}.map((e) => e.textContent)`);
  check(labels18.length > 2, `stop names are labelled at zoom 18 (${labels18.length}: ${labels18.slice(0, 4).join(", ")}…)`);
  await shot("12-map-labels-z18");
  await setView(13.03866, 80.25891, 16);
  await waitFor(`${shownLabels("stop-label")}.length === 0`, "no labels at zoom 16");
  const markers16 = await evaluate(`import(${JSON.stringify(MAPJS)}).then((m) => m.loadedStops().length)`);
  check(markers16 > 0, `below zoom 17 the ${markers16} stop markers stay and the labels go`);
  await setView(13.03866, 80.25891, 17);
  check(await waitFor(`${shownLabels("stop-label")}.length > 0`, "labels at zoom 17"), "labels come back at zoom 17");

  // the route is still open in the panel; click a stop marker on the map
  check((await evaluate("location.hash")) === `#/route/${ROUTE}`, "the route is open while clicking the map");
  const citA = (await editorApi("GET", `feeds/${FEED}/stops/f68ae67c0a`)).body;
  const citB = (await editorApi("GET", `feeds/${FEED}/stops/8de4c3c4ac`)).body;
  await setView(citA.lat, citA.lon, 18);
  await must(waitStopDrawn("f68ae67c0a"));
  await clickMapAt(citA.lat, citA.lon);
  await waitFor(`location.hash === "#/stop/f68ae67c0a" && document.getElementById("panel").innerText.includes("Routes stopping here")`, "stop panel after clicking its marker");
  const panelA = await text("#panel");
  check(panelA.includes(citA.name) && panelA.includes("f68ae67c0a"), `clicking the marker of ${citA.name} while route ${ROUTE} is open switches the panel to that stop`);
  await shot("13-map-click-stop");
  await setView(citB.lat, citB.lon, 18);
  await must(waitStopDrawn("8de4c3c4ac"));
  await clickMapAt(citB.lat, citB.lon);
  await waitFor(`location.hash === "#/stop/8de4c3c4ac"`, "the second stop panel");
  check((await text("#panel")).includes(citB.name), `clicking ${citB.name} (8de4c3c4ac) while another stop is open switches to it`);
}

// ------------------------------------------------------------------ stations
const PROPOSAL = 3, PROPOSAL_STATION = "stn_49fdb86094", REJECTED = 13;
async function stationsFlow() {
  await freshStart(EDITOR);
  const summary = (await editorApi("GET", `feeds/${FEED}/station-proposals/summary`)).body;
  await go("#/stations");
  await must(waitFor(`document.querySelectorAll(".proposal-item").length > 10`, "suggested stations"));
  const pendingText = summary.pending.toLocaleString("en-IN");
  await waitFor(`document.querySelector(".count-tabs").innerText.includes(${JSON.stringify(pendingText)})`, "pending count in the tabs");
  check((await text(".count-tabs")).includes(pendingText), `"Stations to review" counts ${pendingText} suggestions to review`);
  await shot("14-stations-list");
  // the search is by trigram similarity: A.M.S.HOSPITAL first, a few look-alikes after it
  await type("#proposal-search", "A.M.S");
  await must(waitFor(`document.querySelectorAll(".proposal-item").length < 10 && (document.querySelector(".proposal-item .proposal-name")?.textContent || "") === "A.M.S.HOSPITAL"`, "search narrows the list to A.M.S.HOSPITAL first"));
  await click("A.M.S.HOSPITAL", ".proposal-list");
  await must(waitFor(`document.querySelectorAll(".member-card").length === 3`, "the suggestion with its three stops"));
  await waitFor(`document.querySelectorAll(".route-chip").length > 3`, "routes through each stop");
  const detail = (await editorApi("GET", `station-proposals/${PROPOSAL}`)).body;
  const members = detail.members.map((m) => m.stop_id);

  // a station groups at least two stops: dropping two of three is refused
  await clickSel("#drop-1", "Drop the second stop");
  await clickSel("#drop-2", "Drop the third stop");
  await click("Approve into draft", ".sticky-actions");
  check((await text("#panel")).includes("A station groups at least two stops"), "approving a station of one stop is refused before anything is sent");
  await clickSel("#drop-1", "Put the second stop back");
  await clickSel("#drop-2", "Put the third stop back");

  // rename and relabel, then approve into a new draft
  await type("#proposal-name", "A.M.S. Hospital (E2E)");
  await type("#platform-0", "Towards Adyar (E2E)");
  check(await evaluate(`${shownLabels("member-label")}.some((e) => e.textContent === "Towards Adyar (E2E)")`), "the relabelled platform updates on the map");
  await shot("15-station-review");
  await click("Approve into draft", ".sticky-actions");
  await chooseNewDraft("E2E: stations to review");
  await must(waitFor(`location.hash !== "#/stations/${PROPOSAL}"`, "moved on after approving"));
  const draftId = await draftIdOf(EDITOR);
  const approved = (await editorApi("GET", `station-proposals/${PROPOSAL}`)).body;
  check(approved.status === "approved" && approved.change_set_id === draftId, `suggestion #${PROPOSAL} is approved into draft "E2E: stations to review"`);
  const set = (await editorApi("GET", `change-sets/${draftId}`)).body;
  const change = set.changes.find((c) => c.entity === "station");
  check(change && change.after.name === "A.M.S. Hospital (E2E)" && change.after.proposal_id === PROPOSAL
    && change.after.members.find((m) => m.stop_id === members[0])?.platform_code === "Towards Adyar (E2E)",
  "the station change carries the new name, the new platform label and the suggestion id");

  // the API agrees: one stop is not a station, and says so
  const one = await editorApi("POST", `station-proposals/${REJECTED}/approve`, { change_set_id: draftId, members: [{ stop_id: "c8540da4b9" }] });
  const oneWhy = [one.body?.error?.message, ...((one.body?.error?.details?.problems) || []).map((p) => p.message)].join(" ");
  check(one.status === 400 && /at least two/.test(oneWhy), `the API refuses approving a one-stop station: ${one.status} ${oneWhy}`);
  const oneCreate = await editorApi("POST", `change-sets/${draftId}/changes`, {
    entity: "station", op: "create", entity_key: "stn_e2e_single",
    after: { station_id: "stn_e2e_single", name: "E2E single", lat: 13.0185, lon: 80.2189, members: [{ stop_id: "c8540da4b9" }] },
  });
  check(oneCreate.status === 400 && /at least two/.test(oneCreate.body?.error?.message || ""), `the API refuses a new station of one stop: ${oneCreate.status} ${oneCreate.body?.error?.message}`);

  // reject another with a note
  await go(`#/stations/${REJECTED}`);
  await must(waitFor(`document.getElementById("panel").innerText.includes("Adduthotti Bridge") && !!document.querySelector(".sticky-actions")`, `suggestion #${REJECTED}`));
  await click("Reject…", ".sticky-actions");
  await must(waitFor(`!!document.getElementById("reject-note")`, "reject dialog"));
  await click("Reject suggestion", "dialog");
  check((await text("dialog")).includes("Write a short reason"), "rejecting needs a note");
  await type("#reject-note", "E2E: the two kerbs are on different roads.");
  await click("Reject suggestion", "dialog");
  await must(waitFor(`location.hash !== "#/stations/${REJECTED}"`, "moved on after rejecting"));
  const rejected = (await editorApi("GET", `station-proposals/${REJECTED}`)).body;
  check(rejected.status === "rejected" && rejected.review_note === "E2E: the two kerbs are on different roads.", `suggestion #${REJECTED} is rejected with the note`);

  await submitDraft(draftId, "the stations draft");
  const committedAt = await approveAndCommit(draftId, "the stations draft");
  const expected = new Map(detail.members.map((m, i) => [m.stop_id, i === 0 ? "Towards Adyar (E2E)" : m.platform_code]));
  const live = await until(async () => {
    const kids = await gims(`/station-children/${FEED}/${PROPOSAL_STATION}`);
    if (!Array.isArray(kids) || members.some((m) => !kids.includes(m))) return null;
    const stops = await Promise.all(members.map((m) => gims(`/stop/${FEED}/${m}`)));
    return stops.every((s) => s && s.parentStopCode === PROPOSAL_STATION && s.platform === expected.get(s.stopCode)) ? Date.now() - committedAt : null;
  }, 10000);
  timings.stations_commit_to_live_ms = live;
  check(live !== null, `/station-children/${FEED}/${PROPOSAL_STATION} lists its ${members.length} stops, each with its platform label, ${live !== null ? `${live} ms after commit` : "(not within 10 s)"}`);
  check((await editorApi("GET", `station-proposals/${PROPOSAL}`)).body.status === "committed", "the suggestion is marked live");
}

// ------------------------------------------------------------------ create
async function createFlow() {
  await freshStart(EDITOR);
  await setView(12.9725, 80.2208, 18);
  await clickSel("#new-menu summary", "the New menu");
  await click("New stop", "#new-menu");
  await chooseNewDraft("E2E: a new stop and a new route");
  await must(waitFor(`document.getElementById("panel").innerText.includes("Not placed yet")`, "new stop form"));
  const spot = await mapCall(`const c = map.getCenter(); return { lat: c.lat + 0.0002, lon: c.lng + 0.0003 };`);
  check(await evaluate(`import(${JSON.stringify(MAPJS)}).then((m) => !m.loadedStops().some((s) => Math.abs(s.lat - ${spot.lat}) < 0.0004 && Math.abs(s.lon - ${spot.lon}) < 0.0004))`), "the spot for the new stop is clear of existing stops");
  await clickMapAt(spot.lat, spot.lon);
  await must(waitFor(`document.getElementById("new-stop-lat").value !== ""`, "the clicked position in the form"));
  const placed = { lat: Number(await evaluate(`document.getElementById("new-stop-lat").value`)), lon: Number(await evaluate(`document.getElementById("new-stop-lon").value`)) };
  check(Math.abs(placed.lat - spot.lat) < 0.00005 && Math.abs(placed.lon - spot.lon) < 0.00005, "clicking the map places the new stop there");
  await type("#new-stop-name", "E2E NEW KERB");
  await type("#new-stop-platform", "Towards Velachery (E2E)");
  await shot("16-new-stop");
  await click("Add stop to draft", ".sticky-actions");
  await must(waitFor(`document.getElementById("panel").innerText.includes("New stop added to your draft")`, "new stop added"));
  const newStop = (await text("#panel")).match(/ed_[0-9a-f]{10}/)?.[0];
  check(!!newStop, `the server minted the new stop's id (${newStop})`);
  const draftId = await draftIdOf(EDITOR);

  await clickSel("#new-menu summary", "the New menu");
  await click("New route", "#new-menu");
  await must(waitFor(`!!document.getElementById("new-route-id")`, "new route form"));
  await type("#new-route-id", ROUTE);
  await waitFor(`document.getElementById("new-route-id-status").innerText.includes("already used")`, "a used route id is caught");
  await type("#new-route-id", "E2E-R1");
  await waitFor(`document.getElementById("new-route-id-status").innerText.includes("free")`, "a free route id");
  await type("#new-route-short", "E2E1");
  await type("#new-route-long", "E2E NEW KERB - Velachery");
  await type("#new-route-color", "#1F5FBF");
  await click("Add route, then add its stops", ".sticky-actions");
  await must(waitFor(`document.getElementById("panel").innerText.includes("Stops of the new route E2E1")`, "stop list editor for the new route"));
  await clickSel("#add-0", "Add the first stop");
  await must(waitFor(`!!document.querySelector(".picker input")`, "picker for the first stop"));
  await type(".picker input", "E2E NEW");
  await must(waitFor(`[...document.querySelectorAll(".picker-item")].some((b) => b.dataset.stop === ${JSON.stringify(newStop)})`, "the stop new in the draft is offered"));
  await clickSel(`.picker-item[data-stop="${newStop}"]`, "the new stop");
  check((await evaluate(`document.querySelector("#row-0 select")?.value`)) === "NEW STOP", "the first row is a stage stop (NEW STOP)");
  check((await evaluate(`document.getElementById("stage-name-0").selectedOptions[0].textContent`)) === "E2E NEW KERB", "its stage name is the stop's own name");
  await clickSel("#add-1", "Add a stop at the end");
  await must(waitFor(`!!document.querySelector(".picker input")`, "picker for the second stop"));
  await type(".picker input", "VELACHERY");
  await must(waitFor(`[...document.querySelectorAll(".picker-item")].some((b) => /velachery/i.test(b.innerText))`, "results for the second stop"));
  const second = await evaluate(`[...document.querySelectorAll(".picker-item")].find((b) => /velachery/i.test(b.innerText)).dataset.stop`);
  await clickSel(`.picker-item[data-stop="${second}"]`, `stop ${second}`);
  await must(waitFor(`document.querySelectorAll(".ladder.editing .row").length === 2`, "two stops in the list"));
  await shot("17-new-route-stops");
  await click("Add to draft", ".sticky-actions");
  await must(waitFor(`location.hash === "#/route/E2E-R1?draft=1" && document.getElementById("panel").innerText.includes("New route in your draft")`, "the new route with its stops"));

  await become(EDITOR);
  await submitDraft(draftId, "the create draft");
  await approveAndCommit(draftId, "the create draft");
  const stopRow = db(`SELECT name, lat, lon, platform_code, location_type, deleted, stop_code FROM gtfs_stop WHERE gtfs_id = '${FEED}' AND stop_id = ${sqlText(newStop)}`)[0];
  check(!!stopRow && stopRow[0] === "E2E NEW KERB" && Math.abs(Number(stopRow[1]) - placed.lat) < 1e-6 && Math.abs(Number(stopRow[2]) - placed.lon) < 1e-6
    && stopRow[3] === "Towards Velachery (E2E)" && stopRow[4] === "0" && stopRow[5] === "f",
  `gtfs_stop ${newStop}: ${stopRow ? stopRow.join(" | ") : "missing"}`);
  const routeRow = db(`SELECT short_name, long_name, color, route_type, coalesce(agency_id, ''), deleted FROM gtfs_route WHERE gtfs_id = '${FEED}' AND route_id = 'E2E-R1'`)[0];
  check(!!routeRow && routeRow[0] === "E2E1" && routeRow[1] === "E2E NEW KERB - Velachery" && routeRow[2] === "#1F5FBF" && routeRow[3] === "3" && routeRow[4] !== "" && routeRow[5] === "f",
    `gtfs_route E2E-R1: ${routeRow ? routeRow.join(" | ") : "missing"}`);
  const rows = db(`SELECT sequence, stop_id, stop_type, stage_no, stage_name FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND route_id = 'E2E-R1' ORDER BY sequence`);
  check(rows.length === 2 && rows[0].join("|") === `1|${newStop}|NEW STOP|1|E2E NEW KERB` && rows[1].join("|") === `2|${second}|INTERMEDIATE STOP|1|E2E NEW KERB`,
    `gtfs_route_stop E2E-R1: ${rows.map((r) => r.join(" ")).join("; ")}`);
}

// ------------------------------------------------------------------ route stop list editor
const EDIT_ROUTE = "309";   // 5B Mylapore Tank to T. Nagar, clean fare stages
async function routeEditorFlow() {
  await freshStart(EDITOR);
  await go(`#/route/${EDIT_ROUTE}`);
  await must(waitFor(`document.body.innerText.includes("Edit stop list")`, `route ${EDIT_ROUTE}`));
  await click("Edit stop list");
  await chooseNewDraft("E2E: route 5B stop list");
  await must(waitFor(`document.querySelectorAll(".ladder.editing .row").length === 19`, "the stop list editor with 19 stops"));
  check((await text("#panel")).includes("The fare stages are consistent."), "the unedited route has consistent fare stages");

  // Change stop on row 8 by searching
  const oldId = (await text("#row-7 .row-sub .meta")).split(" · ")[0].trim();
  await clickSel("#change-7", "Change stop on stop 8");
  await must(waitFor(`!!document.querySelector("#row-7 .picker input")`, "the change-stop picker in the row"));
  await type("#row-7 .picker input", "GANDHI NAGAR");
  await must(waitFor(`document.querySelectorAll("#row-7 .picker-item").length > 0`, "change-stop search results"));
  const picked = await evaluate(`(() => { const b = document.querySelector("#row-7 .picker-item"); return { id: b.dataset.stop, name: b.querySelector(".picker-name").textContent.trim() }; })()`);
  await clickSel("#row-7 .picker-item", `${picked.name} (${picked.id})`);
  const newId = (await text("#row-7 .row-sub .meta")).split(" · ")[0].trim();
  check(newId === picked.id && newId !== oldId, `Change stop replaced stop 8 ${oldId} with ${picked.name} (${newId})`);
  check(await evaluate(`!document.querySelector(".ladder.editing input:not([type=number]):not(.stage-name-input):not(.picker input)")`), "no free-text field says which stop a row is");

  // the stage name of stop 2 is chosen from a list: the stop's own name
  const options = await evaluate(`[...document.querySelectorAll("#stage-name-1 option")].map((o) => o.textContent)`);
  check(options.includes("MANDAVELI B.T") && options.includes("Other name…") && options.includes("a.m.s.hospital"), `stage names offered for stop 2: ${options.join(", ")}`);
  await choose("#stage-name-1", "name:MANDAVELI B.T");
  check((await text("#row-2")).includes("MANDAVELI B.T"), "the intermediate stop of that stage takes the chosen stage name");
  await shot("18-route-editor");
  await click("Add to draft", ".sticky-actions");
  await must(waitFor(`document.body.innerText.includes("Show what is live now")`, "the route with the draft applied"));
  const draftId = await draftIdOf(EDITOR);

  // a fare-rule error: stage 3 numbered 1 after stage 2
  await click("Show what is live now");
  await click("Edit stop list");
  await must(waitFor(`document.querySelectorAll(".ladder.editing .row").length === 19 && document.getElementById("row-7").innerText.includes(${JSON.stringify(picked.name)})`, "the editor with the draft's stops"));
  await type("#stage-no-3", "1");
  check((await text("#panel")).includes("go back from 2 to 1"), "the editor marks stage numbers going back");
  await click("Update in draft", ".sticky-actions");
  // the server's findings for the change, under the editor's own
  await must(waitFor(`document.getElementById("panel").innerText.includes("Fix before submitting")`, "the server's problems"));
  const serverSays = await text("#panel");
  check(/stage/i.test(serverSays.slice(serverSays.indexOf("Fix before submitting"))), "the server reports the fare-stage error on the change");
  await sleep(500);
  await shot("19-route-editor-fare-error");
  await go(`#/drafts/${draftId}`);
  const left = await evaluate(`document.querySelector("dialog")?.innerText.includes("Leave without saving") || false`);
  check(!left, "leaving after the change is in the draft does not warn about unsaved edits");
  if (left) await click("Leave without saving", "dialog");
  await must(waitFor(`[...document.querySelectorAll("button")].some((b) => b.textContent === "Submit for review")`, "the draft page"));
  const submit = await evaluate(`(() => { const b = [...document.querySelectorAll("button")].find((x) => x.textContent === "Submit for review"); return { disabled: b.disabled, why: document.getElementById(b.getAttribute("aria-describedby"))?.textContent || "" }; })()`);
  check(submit.disabled && /Fix the \d+ problems? below first/.test(submit.why), `Submit for review is blocked: ${submit.why}`);
  const refused = await editorApi("POST", `change-sets/${draftId}/submit`);
  check(refused.status === 400 && refused.body?.error?.code === "validation_failed", `the API refuses to submit it too (${refused.status} ${refused.body?.error?.code})`);

  // renumbering fixes it
  await go(`#/route/${EDIT_ROUTE}`);
  await must(waitFor(`document.body.innerText.includes("Edit stop list")`, "the route again"));
  await click("Edit stop list");
  await must(waitFor(`document.getElementById("stage-no-3")?.value === "1"`, "the editor with the error"));
  await click("Renumber stages in order");
  await must(waitFor(`!!document.getElementById("renumber-start")`, "the renumber dialog"));
  await click("Renumber stages", "dialog");
  check((await evaluate(`document.getElementById("stage-no-3").value`)) === "3", "renumbering puts stage 3 back");
  await click("Update in draft", ".sticky-actions");
  await must(waitFor(`document.body.innerText.includes("Show what is live now")`, "the route with the fixed draft"));
  await submitDraft(draftId, "the route editor draft");
  const committedAt = await approveAndCommit(draftId, "the route editor draft");
  const live = await until(async () => {
    const mapping = await gims(`/route-stop-mapping/${FEED}/route/${EDIT_ROUTE}`);
    const ordered = Array.isArray(mapping) ? [...mapping].sort((a, b) => a.sequenceNum - b.sequenceNum) : [];
    return ordered.length === 19 && ordered[7].stopCode === newId ? Date.now() - committedAt : null;
  }, 10000);
  timings.route_commit_to_live_ms = live;
  check(live !== null, `the public route-stop-mapping of ${EDIT_ROUTE} calls at ${newId} as stop 8 ${live !== null ? `${live} ms after commit` : "(not within 10 s)"}`);
  const stages = db(`SELECT sequence, stop_id, stage_no, stage_name FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND route_id = '${EDIT_ROUTE}' AND sequence IN (2, 3, 4, 8) ORDER BY sequence`);
  check(stages.map((r) => r.join("|")).join(";") === `2|b76f46d86f|2|MANDAVELI B.T;3|4f16bc7cab|2|MANDAVELI B.T;4|a0db25ec4b|3|a.m.s.hospital;8|${newId}|4|ADYAR B.T`,
    `gtfs_route_stop ${EDIT_ROUTE}: ${stages.map((r) => r.join(" ")).join("; ")}`);
}

// ------------------------------------------------------------------ merge
const MERGE_FROM = "16831f722e", MERGE_KEEP = "a936526266";   // THORAIPAKKAM 230 K V TOWER, twice at one point
async function mergeFlow() {
  await freshStart(EDITOR);
  const [a, b] = db(`SELECT stop_id, name, lat, lon FROM gtfs_stop WHERE gtfs_id = '${FEED}' AND stop_id IN ('${MERGE_FROM}', '${MERGE_KEEP}') AND NOT deleted ORDER BY stop_id`);
  check(!!a && !!b && a[1] === b[1] && a[2] === b[2] && a[3] === b[3], `${MERGE_FROM} and ${MERGE_KEEP} have the same name and position (${a?.[1]})`);
  const fromRows = db(`SELECT route_id, sequence FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_FROM}' ORDER BY route_id, sequence`);
  const keepRows = db(`SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_KEEP}'`)[0][0];
  await go(`#/stop/${MERGE_FROM}`);
  await must(waitFor(`document.body.innerText.includes("Merge with a duplicate")`, "the stop's merge action"));
  await click("Merge with a duplicate");
  await must(waitFor(`document.getElementById("panel").innerText.includes("Same name")`, "nearby stops with the same name"));
  await clickSel(`#panel button[aria-label$="(${MERGE_KEEP})"]`, `Compare with ${MERGE_KEEP}`);
  await must(waitFor(`!!document.querySelector("table.compare")`, "the two stops side by side"));
  const compare = await text("#panel");
  check(compare.includes("Which stop id should stay?") && compare.includes("They are 0 m apart"), "asks which stop id stays, for two stops 0 m apart");
  // keep the other id than the one the merge started from
  await clickSel(`#keep-${MERGE_KEEP}`, `keep ${MERGE_KEEP}`);
  check((await text(".sticky-actions")).includes(`keep ${MERGE_KEEP}`), `the add button names ${MERGE_KEEP} as the id that stays`);
  check((await text("#panel")).includes(`will switch from ${MERGE_FROM} to ${MERGE_KEEP}`), "says which routes switch before adding");
  await shot("20-merge-compare");
  await click("Add merge to draft", ".sticky-actions");
  await must(waitFor(`document.querySelector("dialog")?.innerText.includes("Add this merge")`, "merge confirmation"));
  await click("Add merge to draft", "dialog");
  await chooseNewDraft("E2E: merge duplicate kerbs");
  await must(waitFor(`document.getElementById("panel").innerText.includes("Merge added to your draft")`, "the merge in the draft"));
  const draftId = await draftIdOf(EDITOR);
  await submitDraft(draftId, "the merge draft");
  await approveAndCommit(draftId, "the merge draft");
  const moved = db(`SELECT route_id, sequence FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_KEEP}' AND (route_id, sequence) IN (${fromRows.map((r) => `(${sqlText(r[0])}, ${r[1]})`).join(", ")})`);
  const leftOn = db(`SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_FROM}'`)[0][0];
  const keepNow = db(`SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_KEEP}'`)[0][0];
  check(moved.length === fromRows.length && leftOn === "0" && Number(keepNow) === Number(keepRows) + fromRows.length,
    `the ${fromRows.length} route rows of ${MERGE_FROM} now call at ${MERGE_KEEP} (${keepRows} + ${fromRows.length} = ${keepNow}), none left on ${MERGE_FROM}`);
  const gone = db(`SELECT deleted, parent_station IS NULL, provenance->>'merged_into' FROM gtfs_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_FROM}'`)[0];
  const kept = db(`SELECT deleted FROM gtfs_stop WHERE gtfs_id = '${FEED}' AND stop_id = '${MERGE_KEEP}'`)[0];
  check(gone?.join("|") === `t|t|${MERGE_KEEP}` && kept?.[0] === "f", `${MERGE_FROM} is soft-deleted (merged_into ${gone?.[2]}), ${MERGE_KEEP} stays`);
  const audit = db(`SELECT detail->>'from', detail->>'into', detail->>'rows' FROM gtfs_audit_log WHERE action = 'stop_merged' AND change_set_id = ${sqlText(draftId)}`);
  check(audit.length === 1 && audit[0].join("|") === `${MERGE_FROM}|${MERGE_KEEP}|${fromRows.length}`, `the commit audits stop_merged (${audit.map((r) => r.join(" ")).join("; ")})`);
}

// ------------------------------------------------------------------ import
async function importFlow() {
  await freshStart(EDITOR);
  await clickSel("#draft-chip", "the draft chip");
  await chooseNewDraft("E2E: import stops");
  const draftId = await draftIdOf(EDITOR);
  await clickSel("#new-menu summary", "the New menu");
  await click("Import from a CSV file", "#new-menu");
  await must(waitFor(`!!document.getElementById("import-file")`, "the import page"));
  const header = "stop_id,name,lat,lon,platform_code";
  const good1 = ",E2E IMPORT ONE,12.973100,80.221200,Towards Guindy (E2E)";
  const good3 = ",E2E IMPORT THREE,12.973500,80.221800,";
  // row 2 uses a stop id that already exists
  await setFile("e2e-stops.csv", [header, good1, `${STOP},E2E IMPORT TWO,12.973300,80.221500,`, good3].join("\n") + "\n");
  await must(waitFor(`document.getElementById("page").innerText.includes("has errors") || document.getElementById("page").innerText.includes("have errors")`, "the check with an error"));
  const table = await text(".result-table");
  check(table.includes(`stop ${STOP} already exists`), "the preview says what is wrong on the bad row");
  check((await text(".summary-chips")).includes("1 error"), `the summary counts one error (${(await text(".summary-chips")).replace(/\n/g, " ")})`);
  check(await evaluate(`[...document.querySelectorAll(".actionbar button")].find((b) => b.textContent.startsWith("Add"))?.disabled === true`), "adding is off while a row has an error");
  await shot("21-import-error");
  // the fixed file: the bad row gets no id, so one is minted
  await setFile("e2e-stops-fixed.csv", [header, good1, ",E2E IMPORT TWO,12.973300,80.221500,", good3].join("\n") + "\n");
  await must(waitFor(`document.getElementById("page").innerText.includes("can be added")`, "the check of the fixed file"));
  const chips = await text(".summary-chips");
  check(chips.includes("3 rows") && chips.includes("0 errors") && chips.includes("3 changes"), `the fixed file checks clean (${chips.replace(/\n/g, " ")})`);
  await waitFor(`!!document.querySelector(".import-map.leaflet-container, .import-map .leaflet-container")`, "the map of the uploaded stops");
  await shot("22-import-ok");
  await click("Add 3 changes to draft");
  await click("Add 3 to draft", "dialog");
  await must(waitFor(`document.getElementById("page").innerText.includes("Added 3 changes")`, "the import added to the draft"));
  await submitDraft(draftId, "the import draft");
  await approveAndCommit(draftId, "the import draft");
  const rows = db(`SELECT stop_id, name, lat, lon, coalesce(platform_code, ''), deleted FROM gtfs_stop WHERE gtfs_id = '${FEED}' AND name LIKE 'E2E IMPORT %' ORDER BY lat`);
  check(rows.length === 3 && rows.every((r) => /^ed_[0-9a-f]{10}$/.test(r[0]) && r[5] === "f")
    && rows.map((r) => `${r[1]}@${Number(r[2]).toFixed(6)},${Number(r[3]).toFixed(6)}:${r[4]}`).join(";")
      === "E2E IMPORT ONE@12.973100,80.221200:Towards Guindy (E2E);E2E IMPORT TWO@12.973300,80.221500:;E2E IMPORT THREE@12.973500,80.221800:",
  `gtfs_stop has the 3 imported stops with minted ids: ${rows.map((r) => `${r[0]} ${r[1]}`).join("; ")}`);
  const audit = db(`SELECT detail->>'kind', detail->>'rows', detail->>'changes' FROM gtfs_audit_log WHERE action = 'bulk_imported' AND change_set_id = ${sqlText(draftId)}`);
  check(audit.length === 1 && audit[0].join("|") === "stops|3|3", `the import is audited as bulk_imported (${audit.map((r) => r.join(" ")).join("; ")})`);
}

// ------------------------------------------------------------------ history
async function historyFlow() {
  await become(APPROVER);
  await go("#/audit");
  await must(waitFor(`document.body.innerText.includes("Committed (live)")`, "history"));
  // every action in the database (this run's, and the API tests' on their own
  // feeds) has words; none falls back to its raw name
  const actions = db("SELECT DISTINCT action FROM gtfs_audit_log ORDER BY 1").map((r) => r[0]);
  const labels = await evaluate(`import(${JSON.stringify(`${PROXY}${UI_PATH}js/admin.js`)}).then((m) => m.ACTION_LABEL)`);
  const missing = actions.filter((a) => !labels[a]);
  check(missing.length === 0, `every audit action in the database has a history label${missing.length ? ` (missing: ${missing.join(", ")})` : ` (${actions.length} actions)`}`);
  // the page shows 100 entries at a time; load them all
  for (let i = 0; i < 20 && (await evaluate(`[...document.querySelectorAll("#page button")].some((b) => b.textContent === "Show older" && !b.hidden)`)); i++) {
    const rows = await evaluate(`document.querySelectorAll("#page tbody tr").length`);
    await click("Show older", "#page");
    await waitFor(`document.querySelectorAll("#page tbody tr").length > ${rows}`, "older history");
  }
  const page = await text("#page");
  for (const label of ["Imported a CSV file", "Merged a duplicate stop", "Approved a suggested station into a draft", "Rejected a suggested station", "A suggested station went live"]) {
    check(page.includes(label), `history shows "${label}"`);
  }
  await shot("23-history");
}

try {
  await connect();
  await send("Page.enable");
  await send("Runtime.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  db("SELECT 1");
  const coreStarted = Date.now();
  await coreFlow();
  timings.core_ms = Date.now() - coreStarted;
  await flow("map", mapFlow);
  await flow("stations", stationsFlow);
  await flow("create", createFlow);
  await flow("route", routeEditorFlow);
  await flow("merge", mergeFlow);
  await flow("import", importFlow);
  await flow("history", historyFlow);
  check(consoleErrors.length === 0, `no console errors${consoleErrors.length ? `: ${consoleErrors.slice(0, 5).join(" | ")}` : ""}`);
} catch (e) {
  failures.push(e.message);
  console.log(`FAIL ${e.message}`);
} finally {
  try { ws?.close(); } catch { /* ignore */ }
  chrome.kill();
}
console.log(`\ntimings: ${JSON.stringify(timings)}`);
console.log(`${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
process.exit(failures.length ? 1 : 0);
