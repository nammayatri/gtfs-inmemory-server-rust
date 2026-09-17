// End-to-end smoke test of the dashboard against dev/mock_server.py, driving a
// headless Chrome over the DevTools protocol (Node 22+, no dependencies).
//
//   python dev/mock_server.py &            # port 8765
//   node dev/ui_smoke.mjs [--shots /tmp/gtfs-editor-shots]
//
// Every flow runs through the real UI: sign-in, TOTP enrolment, search, the route
// ladder and its stop-list editor, a stop move, a station, a map line, submit,
// approve and commit by a second person, a commit conflict, people and history.
import { spawn } from "node:child_process";
import { createHmac } from "node:crypto";
import { mkdirSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BASE = "http://127.0.0.1:8765";
const UI = `${BASE}/internal/gtfs-editor/ui/`;
const CHROME = process.env.CHROME || "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const shotsArg = process.argv.indexOf("--shots");
const SHOTS = shotsArg > 0 ? process.argv[shotsArg + 1] : join(tmpdir(), "gtfs-editor-shots");
mkdirSync(SHOTS, { recursive: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const failures = [];
const check = (ok, what) => { console.log(`${ok ? "ok  " : "FAIL"} ${what}`); if (!ok) failures.push(what); };

function totp(secret) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  let bits = "";
  for (const c of secret.replace(/\s|=/g, "").toUpperCase()) bits += alphabet.indexOf(c).toString(2).padStart(5, "0");
  const key = Buffer.from(bits.match(/.{8}/g).map((b) => parseInt(b, 2)));
  const msg = Buffer.alloc(8);
  msg.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30000)));
  const mac = createHmac("sha1", key).update(msg).digest();
  const off = mac[mac.length - 1] & 15;
  return String((mac.readUInt32BE(off) & 0x7fffffff) % 1000000).padStart(6, "0");
}

// ------------------------------------------------------------------ CDP
const profile = mkdtempSync(join(tmpdir(), "gtfs-editor-chrome-"));
const chrome = spawn(CHROME, ["--headless=new", "--remote-debugging-port=9333", `--user-data-dir=${profile}`,
  "--no-first-run", "--window-size=1440,900", "about:blank"], { stdio: "ignore" });
let ws, nextId = 1;
const pending = new Map();
const consoleErrors = [];

async function connect() {
  for (let i = 0; i < 50; i++) {
    try {
      const targets = await (await fetch("http://127.0.0.1:9333/json")).json();
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
async function go(hash) { await evaluate(`location.hash = ${JSON.stringify(hash)}`); await sleep(400); }
async function load(url) { await send("Page.navigate", { url }); await sleep(1200); }
// click the first visible button/link whose text includes `text`, optionally inside a selector
async function click(text, scope = "body") {
  const ok = await evaluate(`(() => {
    const els = [...document.querySelectorAll(${JSON.stringify(scope)} + " button, " + ${JSON.stringify(scope)} + " a")]
      .filter((e) => e.offsetParent !== null && !e.disabled && e.textContent.trim().includes(${JSON.stringify(text)}));
    if (!els.length) return false;
    els[0].click();
    return true;
  })()`);
  check(ok, `click "${text}"`);
  await sleep(350);
  return ok;
}
async function type(selector, value) {
  await evaluate(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); el.focus(); el.value = ${JSON.stringify(value)};
    el.dispatchEvent(new Event("input", { bubbles: true })); el.dispatchEvent(new Event("change", { bubbles: true })); return true; })()`);
  await sleep(300);
}
const text = (sel = "body") => evaluate(`document.querySelector(${JSON.stringify(sel)})?.innerText || ""`);
async function actAs(email) {
  await evaluate(`fetch("/__dev/as", {method:"POST", credentials:"same-origin", headers:{"Content-Type":"application/json"}, body: JSON.stringify({email: ${JSON.stringify(email)}})}).then(() => true)`);
  await load(UI);
}
async function signIn(email) {
  await actAs(email);
  await waitFor(`!!document.querySelector(".code-input")`, `code prompt for ${email}`);
  const code = await evaluate(`fetch("/__dev/state").then(r => r.json()).then(s => s.current_code)`);
  await type(".code-input", code);
  await waitFor(`!document.getElementById("app").hidden`, `app after sign-in as ${email}`);
  await sleep(600);
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
  await type(".code-input", totp(secret));
  await waitFor(`!document.getElementById("app").hidden`, "app after enrolment");
  check(true, "enrolment with a real authenticator code signs in");

  // editor: search, route ladder, stop-list edit
  await signIn("editor1@nammayatri.in");
  await shot("02-home");
  await type("#search-input", "45BET");
  await waitFor(`document.querySelectorAll(".search-item").length > 0`, "search results");
  await evaluate(`location.hash = "#/route/1369"`);
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 3`, "stage ladder");
  check((await text("#panel")).includes("ALWARPET ANJANEYAR TEMPLE"), "route 1369 lists Alwarpet Anjaneyar Temple");
  await sleep(1200);
  await shot("03-route-ladder");

  await click("Edit stop list");
  await waitFor(`document.querySelector("dialog") !== null`, "draft chooser");
  await type("#new-draft-title", "Smoke test: Alwarpet kerbs");
  await click("Start draft", "dialog");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length > 10`, "stop list editor");
  check((await text("#draft-chip")).includes("Smoke test"), "draft chip shows the new draft");
  // move an intermediate row above its stage stop to break the fare rule
  const brokeRule = await evaluate(`(() => {
    const rows = [...document.querySelectorAll(".ladder.editing .row")];
    const i = rows.findIndex((r, k) => k > 1 && r.classList.contains("new"));
    const btn = rows[i].querySelector('button[title="Move up"]');
    btn.click();
    return i;
  })()`);
  await sleep(400);
  const summary = await text("#panel");
  check(/intermediate stop in stage \d+ but carries stage \d+|go back from|must start at/.test(summary) || /problem/.test(summary), `moving stop ${brokeRule} up reports a fare-stage problem`);
  await shot("04-route-edit-error");
  // throw that edit away, then insert a stop
  await click("Cancel", ".sticky-actions");
  await waitFor(`document.querySelectorAll(".ladder .stage").length > 3`, "ladder again");
  await click("Edit stop list");
  await waitFor(`document.querySelectorAll(".ladder.editing .row").length > 10`, "stop list editor again");
  await evaluate(`document.querySelectorAll(".ladder.editing .row")[2].querySelector('button[title="Insert a stop after this one"]').click()`);
  await sleep(300);
  await type("#insert-search", "LUZ");
  await waitFor(`[...document.querySelectorAll(".insert-box button")].some(b => b.textContent === "Insert")`, "insert search results");
  await click("Insert", ".insert-box");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("with my draft") || document.body.innerText.includes("draft applied")`, "route shown with draft");
  check(true, "route stop list change added to the draft");

  // stop move
  await go("#/stop/de9014549c");
  await waitFor(`document.body.innerText.includes("Routes stopping here")`, "stop panel");
  await shot("05-stop");
  await click("Edit stop");
  await waitFor(`!!document.getElementById("stop-lat")`, "stop editor");
  await type("#stop-lat", "13.0385800");
  await type("#stop-name", "ALWARPET ANJANEYAR TEMPLE (TEYNAMPET SIDE)");
  await sleep(500);
  check((await text("#panel")).includes("Moved"), "stop editor shows the distance moved");
  await shot("06-stop-edit");
  await click("Add to draft", ".sticky-actions");
  await waitFor(`document.body.innerText.includes("changes this stop")`, "stop shows pending draft notice");

  // station
  await go("#/stop/68e3cdc2e0");
  await waitFor(`document.body.innerText.includes("Club into a station")`, "stop with club action");
  await click("Club into a station");
  await waitFor(`!!document.getElementById("station-name")`, "station editor");
  await waitFor(`[...document.querySelectorAll("#panel button")].some(b => b.textContent === "Add")`, "nearby suggestions");
  await click("Add", "#panel");
  await type("#station-name", "Alwarpet Anjaneyar Temple");
  await shot("07-station");
  await click("Add to draft", ".sticky-actions");
  await sleep(800);
  check(!(await text("#panel")).includes("Fix before submitting"), "station change accepted");

  // map line
  await go("#/route/1369");
  await waitFor(`document.body.innerText.includes("Edit name, colour and map line")`, "route actions");
  await click("Edit name, colour and map line");
  await click("Suggest a map line");
  await waitFor(`document.body.innerText.includes("New map line ready")`, "proposed map line");
  await type("#route-color", "#0B6660");
  await click("Add to draft", ".sticky-actions");
  await sleep(800);

  // draft page and submit
  const draftId = await evaluate(`Object.entries(JSON.parse(localStorage.getItem("gtfs-editor-prefs"))).find(([k, v]) => k.startsWith("draft:" + document.getElementById("user-role").textContent.split(",")[0] + ":"))?.[1]`);
  await go(`#/drafts/${draftId}`);
  await waitFor(`document.querySelectorAll(".change").length >= 4`, "draft page with 4 changes");
  const draftText = await text("#page");
  check(draftText.includes("moved") || draftText.includes("added"), "route stop list diff renders");
  check(draftText.includes("joins the station"), "station membership diff renders");
  await sleep(1200);
  await shot("08-draft-review");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await waitFor(`document.body.innerText.includes("Waiting for review")`, "submitted status");
  const approveDisabled = await evaluate(`[...document.querySelectorAll("button")].find(b => b.textContent === "Approve")?.disabled`);
  check(approveDisabled === true || approveDisabled === undefined, "editor cannot approve");

  // a second draft edits the same stop and is committed first -> conflict later
  await signIn("editor2@nammayatri.in");
  await go("#/stop/de9014549c");
  await waitFor(`document.body.innerText.includes("Edit stop")`, "stop for editor2");
  await click("Edit stop");
  await waitFor(`document.querySelector("dialog") !== null`, "draft chooser for editor2");
  await type("#new-draft-title", "Competing edit");
  await click("Start draft", "dialog");
  await waitFor(`!!document.getElementById("stop-lat")`, "editor2 stop editor");
  await type("#stop-lon", "80.2588800");
  await click("Add to draft", ".sticky-actions");
  await sleep(600);
  const draft2 = await evaluate(`Object.entries(JSON.parse(localStorage.getItem("gtfs-editor-prefs"))).find(([k, v]) => k.startsWith("draft:" + document.getElementById("user-role").textContent.split(",")[0] + ":"))?.[1]`);
  await go(`#/drafts/${draft2}`);
  await waitFor(`document.body.innerText.includes("Submit for review")`, "draft2 page");
  await click("Submit for review");
  await click("Submit for review", "dialog");
  await sleep(600);

  // approver: approve + commit draft 2, then approve draft 1 and hit the conflict
  await signIn("approver1@nammayatri.in");
  for (const id of [draft2, draftId]) {
    await go(`#/drafts/${id}`);
    await waitFor(`document.body.innerText.includes("Approve")`, `approve button on ${id}`);
    await click("Approve");
    await click("Approve", "dialog");
    await waitFor(`document.body.innerText.includes("Commit and go live")`, `commit button on ${id}`);
    await click("Commit and go live");
    await click("Commit and go live", "dialog");
    await sleep(900);
  }
  check((await text("#page")).includes("overtaken by another commit"), "commit conflict is explained");
  await shot("09-conflict");

  // admin + history
  await signIn("admin@nammayatri.in");
  await go("#/admin");
  await waitFor(`document.querySelectorAll("tbody tr").length >= 6`, "people table");
  await type("#new-user-email", "ops.person@nammayatri.in");
  await click("Add person");
  await waitFor(`document.body.innerText.includes("ops.person@nammayatri.in")`, "added person");
  await shot("10-people");
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
console.log(`\n${failures.length ? `${failures.length} failure(s)` : "all passed"}; screenshots in ${SHOTS}`);
process.exit(failures.length ? 1 : 0);
