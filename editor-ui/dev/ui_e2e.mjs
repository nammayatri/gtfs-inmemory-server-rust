// End-to-end test of the REAL dashboard against a REAL local GIMS, through the
// dev Pomerium stand-in, driving headless Chrome over the DevTools protocol
// (Node 22+, no dependencies). Local only.
//
//   node scripts/dev_pomerium_proxy.mjs --listen 18080 --upstream http://127.0.0.1:18010 \
//        --audience gtfs.editor.localhost --jwks-out /tmp/jwks.json &
//   DHALL_CONFIG=<config with gtfs_db_feeds=["chennai_bus"], the editor enabled,
//        jwks file://..., audience gtfs.editor.localhost, bootstrap admin
//        admin@nammayatri.in, poll 3 s> target/debug/gtfs-routes-service &
//   node editor-ui/dev/ui_e2e.mjs [--shots DIR]
//
// Flow: no identity -> admin enrols -> admin adds an editor and an approver ->
// the editor edits a route's stop order, moves and renames a stop, clubs two
// stops into a station, submits -> the editor cannot approve -> the approver
// reviews, approves, commits -> the change is live in the PUBLIC GIMS APIs
// without a restart -> a stale draft's commit shows the conflict panel.
import { spawn } from "node:child_process";
import { createHmac } from "node:crypto";
import { mkdirSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const PROXY = process.env.PROXY || "http://localhost:18080";
const GIMS = process.env.GIMS || "http://127.0.0.1:18010";
const UI_PATH = "/internal/gtfs-editor/ui/";
const FEED = "chennai_bus";
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const shotsArg = process.argv.indexOf("--shots");
const SHOTS = shotsArg > 0 ? process.argv[shotsArg + 1] : join(tmpdir(), "gtfs-editor-e2e-shots");
mkdirSync(SHOTS, { recursive: true });

const ADMIN = "admin@nammayatri.in", EDITOR = "editor1@nammayatri.in", APPROVER = "approver1@nammayatri.in";
const ROUTE = "1369", STOP = "de9014549c", CLUB = "68e3cdc2e0", CONTESTED = "13f0ef7b8d";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const timings = {};
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failures.push(what); return ok; };

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
const chrome = spawn(CHROME, ["--headless=new", "--remote-debugging-port=9334", `--user-data-dir=${profile}`,
  "--no-first-run", "--window-size=1440,900", "--disable-background-timer-throttling",
  "--disable-renderer-backgrounding", "--disable-backgrounding-occluded-windows", "about:blank"], { stdio: "ignore" });
let ws, nextId = 1;
const pending = new Map();
const consoleErrors = [];

async function connect() {
  for (let i = 0; i < 50; i++) {
    try {
      const targets = await (await fetch("http://127.0.0.1:9334/json")).json();
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
async function type(selector, value) {
  await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); el.focus(); el.value = ${JSON.stringify(value)};
    el.dispatchEvent(new Event("input", { bubbles: true })); el.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`);
  await sleep(350);
}
const text = (sel = "body") => evaluate(`document.querySelector(${JSON.stringify(sel)})?.innerText || ""`);
const draftIdOf = (email) => evaluate(`JSON.parse(localStorage.getItem("gtfs-editor-prefs") || "{}")[${JSON.stringify(`draft:${email}:${FEED}`)}]`);

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

async function gims(path) {
  const r = await fetch(GIMS + path);
  return r.ok ? r.json() : null;
}

// ------------------------------------------------------------------ flows
try {
  await connect();
  await send("Page.enable");
  await send("Runtime.enable");
  await send("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });

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
  await waitFor(`!!document.querySelector(".qr")`, "admin enrolment QR");
  secrets[ADMIN] = (await text(".secret")).replace(/\s/g, "");
  await type(".code-input", "000000");
  await waitFor(`document.body.innerText.includes("more tries before sign-in pauses")`, "wrong code with tries left");
  check(true, "wrong code says how many tries are left");
  await type(".code-input", await freshCode(ADMIN));
  await waitFor(`!document.getElementById("app").hidden`, "app after admin enrolment");
  check(true, "bootstrap admin enrols with a real authenticator code");

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

  // ---- editor: route stop order, stop move + rename, station; submit
  await signIn(EDITOR);
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
  await signIn(APPROVER);
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
  timings.commit_to_live_ms = live;
  check(live !== null, `public GIMS APIs show the rename+move, the inserted stop and the station ${live !== null ? `${live} ms after commit` : "(not within 15 s)"}`);

  // ---- a stale draft: two drafts edit the same stop; the second commit conflicts
  await signIn(EDITOR);
  for (const [who, title, name] of [[EDITOR, "E2E: LUZ rename A", "LUZ A"], [ADMIN, "E2E: LUZ rename B", "LUZ B"]]) {
    if (who !== EDITOR) await signIn(who);
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
    if (who === EDITOR) timings.draftA = id; else timings.draftB = id;
  }
  await signIn(APPROVER);
  for (const id of [timings.draftB, timings.draftA]) {
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
