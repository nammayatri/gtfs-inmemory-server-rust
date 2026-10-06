// The feed as a whole (docs section 18): what it serves from, its GTFS zip as
// the tables give it, the feed report (what it breaks of the GTFS reference),
// and - for an admin - bringing a GTFS zip in: a seed for a feed with no rows,
// change sets for one that has them, or a fresh reload that replaces all of
// it (confirmed twice with the authenticator), always checked first without
// writing; and a new feed made from its zip.
import { get, postBytes, enc, ApiError } from "./api.js";
import { API_BASE } from "./config.js";
import { state, isAdmin } from "./state.js";
import { h, clear, fmtCount, plural, errorText, toast, modal } from "./util.js";
import { nameHere } from "./trail.js";

const page = () => document.getElementById("page");

export async function showFeed() {
  const root = h("div.page-inner.feed", h("p.empty", "Loading…"));
  clear(page(), root);
  nameHere("Feed");
  const g = enc(state.feedId);
  let config = {}, files = null;
  try {
    [config, files] = await Promise.all([get(`feeds/${g}/config`), get(`feeds/${g}/files`)]);
  } catch (e) {
    return clear(root, h("h1", "Feed"), h("p.notice.error", errorText(e)));
  }
  const rows = new Map(files.items.map((f) => [f.file, f.rows]));
  const reportBox = h("div");
  const importBox = h("div");

  const check = async () => {
    clear(reportBox, h("p.empty", "Checking the whole feed…"));
    try {
      const v = await get(`feeds/${g}/validation`);
      clear(reportBox, reportView(v.report, `Checked against the GTFS reference on ${v.today}.`));
    } catch (e) {
      clear(reportBox, h("p.notice.error", errorText(e)));
    }
  };

  clear(root,
    h("div.title-block", h("h1", `Feed ${state.feedId}`),
      h("p.hint", `Version ${config.version}. Stops, stations and routes are served from ${config.data_source === "db" ? "these tables" : "the nightly build"}; trips from ${config.trips_source === "db" ? "these tables" : "the nightly build"}.`)),
    h("dl.facts",
      h("dt", "Stops"), h("dd", fmtCount(rows.get("stops.txt"))),
      h("dt", "Routes"), h("dd", fmtCount(rows.get("routes.txt"))),
      h("dt", "Trips"), h("dd", fmtCount(rows.get("trips.txt"))),
      h("dt", "Services"), h("dd", fmtCount(rows.get("calendar.txt"))),
      h("dt", "Default timing"), h("dd", `${config.default_run_s} s a hop, ${config.default_dwell_s} s at a stop`)),
    h("section.feed-section",
      h("h2", "The feed's GTFS"),
      h("p.hint", "The zip the tables give: every file of the reference the feed has. It keeps inactive routes, marked route_active = 0, so reloading it gives them back as they were. The published zip, the one passengers' apps read, leaves them and their trips out."),
      h("div.btn-row",
        h("a.btn.secondary", { href: `${API_BASE}feeds/${g}/gtfs.zip`, download: `${state.feedId}.gtfs.zip`, id: "download-zip" }, "Download the GTFS zip"),
        h("a.btn.secondary", { href: `${API_BASE}feeds/${g}/gtfs.zip?as=published`, download: `${state.feedId}.published.gtfs.zip`, id: "download-published-zip" }, "Download as published"),
        h("a.btn.secondary", { href: "#/files" }, "Browse the files"))),
    h("section.feed-section",
      h("h2", "Feed report"),
      h("p.hint", "What the feed breaks of the GTFS reference: missing files and fields, references to rows that do not exist, stations and pathways, times that go backwards, calendars, and what nothing uses."),
      h("div.btn-row", h("button.btn.secondary", { type: "button", id: "check-feed", on: { click: check } }, "Check the feed")),
      reportBox),
    isAdmin() ? h("section.feed-section", h("h2", "Import a GTFS zip"), importBox) : null);
  if (isAdmin()) importForm(importBox);
}

// A report's findings by kind: errors first, a few messages of each.
export function reportView(report, intro) {
  return h("div.report",
    h("p", h("strong", `${plural(report.errors, "error")}, ${plural(report.warnings, "warning")}.`), intro ? ` ${intro}` : ""),
    report.codes.length ? h("div.table-wrap", h("table.report-table",
      h("thead", h("tr", h("th", ""), h("th", "What"), h("th", "File"), h("th.num", "How many"), h("th", "For example"))),
      h("tbody", report.codes.map((c) => h("tr", { dataset: { code: c.code } },
        h("td", h("span.chip", { class: c.level === "error" ? "rejected" : "draft" }, c.level)),
        h("td", h("code", c.code)),
        h("td", c.file ? h("code", c.file) : ""),
        h("td.num", fmtCount(c.count)),
        h("td", h("ul.samples", c.samples.map((s) => h("li", s))))))))) : h("p.notice.ok", "Nothing to report."));
}

function importForm(box) {
  const file = h("input", { type: "file", id: "import-zip", accept: ".zip,application/zip" });
  const mode = (value, title, text, checked) => h("label.radio-card", { for: `mode-${value}` },
    h("input", { type: "radio", name: "import-mode", id: `mode-${value}`, value, checked }),
    h("span.radio-card-body", h("span.radio-card-title", title), h("span.hint", text)));
  const files = h("input", { type: "text", id: "import-files", placeholder: "optional: agency.txt,calendar.txt,trips.txt" });
  const out = h("div", { "aria-live": "polite" });
  let bytes = null, name = null;
  file.addEventListener("change", async () => {
    const f = file.files && file.files[0];
    bytes = f ? await f.arrayBuffer() : null;
    name = f ? f.name : null;
    clear(out);
  });
  // A fresh reload (docs section 18.16): checked, then done, each step
  // confirmed with a code of its own from the authenticator.
  const g = enc(state.feedId);
  const reloadCheck = async () => {
    const code = await askCode(1, [
      h("p", "This checks ", h("strong", name), ` as a fresh load of ${state.feedId}. The whole reload runs and is undone, so nothing is written yet.`),
    ], "Check the reload");
    if (!code) return;
    const doing = `Checking ${name} as a fresh load of ${state.feedId}`;
    clear(out, h("p.empty", `${doing}…`));
    let res;
    try {
      res = await runImport(`feeds/${g}/reload`, bytes, (s) => clear(out, h("p.empty", `${doing}… ${s} s`)), { "X-Editor-Code": code });
    } catch (e) {
      return clear(out, codeError(e), h("div.btn-row", h("button.btn.secondary", { type: "button", on: { click: reloadCheck } }, "Try again")));
    }
    clear(out,
      replacedView(res.replaced),
      reactivatedView(res.reactivated),
      seedReport(res),
      res.confirm_token
        ? h("div.btn-row", h("button.btn.danger", { type: "button", id: "reload-confirm", on: { click: () => reloadDo(res) } }, `Reload ${state.feedId}…`))
        : h("p.notice.error", "This zip cannot be loaded as it is; nothing was written."));
  };
  const reloadDo = async (checked) => {
    const code = await askCode(2, [
      h("p.notice.warning", "This deletes ", h("strong", replacedWords(checked.replaced)), ` of ${state.feedId}, and loads `, h("strong", name), " in their place. It cannot be undone."),
      h("p.hint", "Drafts already committed, releases, the history, reviews, webhooks and who may work on the feed are kept."),
    ], "Delete and reload", state.feedId);
    if (!code) return;
    const doing = `Reloading ${state.feedId} from ${name}`;
    clear(out, h("p.empty", `${doing}…`));
    try {
      const res = await runImport(`feeds/${g}/reload?confirm=${enc(checked.confirm_token)}`, bytes,
        (s) => clear(out, h("p.empty", `${doing}… ${s} s`)), { "X-Editor-Code": code });
      toast(`Feed ${state.feedId} is reloaded from ${name}: version ${res.feed_version}.`);
      window.dispatchEvent(new CustomEvent("feeds:changed", { detail: { select: state.feedId } }));
    } catch (e) {
      // a wrong or used code leaves the check good: confirm again
      const again = e instanceof ApiError && ["invalid_code", "code_reused"].includes(e.code);
      clear(out, codeError(e), h("div.btn-row", again
        ? h("button.btn.danger", { type: "button", on: { click: () => reloadDo(checked) } }, "Confirm again")
        : h("button.btn.secondary", { type: "button", on: { click: reloadCheck } }, "Check again")));
    }
  };
  const run = async (write) => {
    if (!bytes) return clear(out, h("p.notice.error", "Choose a GTFS zip first."));
    const how = box.querySelector("input[name=import-mode]:checked").value;
    if (how === "reload") return reloadCheck();
    const qs = new URLSearchParams();
    if (how === "drafts") {
      qs.set("mode", "drafts");
      qs.set("dry_run", String(!write));
      if (files.value.trim()) qs.set("files", files.value.trim());
    } else if (write) {
      qs.set("seed", "true");
    }
    const doing = write ? "Importing" : `Checking ${name} without writing anything`;
    clear(out, h("p.empty", `${doing}…`));
    try {
      const res = await runImport(`feeds/${enc(state.feedId)}/import?${qs}`, bytes,
        (s) => clear(out, h("p.empty", `${doing}… ${s} s`)));
      clear(out, how === "drafts" ? draftsReport(res) : seedReport(res), !write && canWrite(how, res)
        ? h("div.btn-row", h("button.btn", { type: "button", id: "import-write", on: { click: () => run(true) } }, how === "drafts" ? "Draft these changes" : "Load the feed"))
        : null);
    } catch (e) {
      clear(out, h("p.notice.error", errorText(e)));
    }
  };
  clear(box,
    h("p.hint", "Only an admin imports. Nothing is written until you have seen what the import would do. This imports into ",
      h("strong", state.feedId), "; a zip of another feed goes in as ", h("a", { href: "#/new-feed" }, "a new feed"), "."),
    h("label.field", { for: "import-zip" }, h("span", "GTFS zip"), file),
    h("div.radio-cards", { role: "radiogroup", "aria-label": "How" },
      mode("seed", "Load an empty feed", "For a feed with no rows yet: the whole zip in one go, checked by exporting it again.", true),
      mode("drafts", "Bring into drafts", "For a feed that has rows: change sets for review. Stops, routes and stop orders are compared, never written.", false),
      mode("reload", "Reload the feed fresh", "Deletes every stop, route, trip and file the feed has and loads the zip in their place, with no drafts. Confirmed twice with your authenticator app.", false)),
    h("label.field", { for: "import-files" }, h("span", "Only these files (drafts)"), files),
    h("div.btn-row", h("button.btn.secondary", { type: "button", id: "import-check", on: { click: () => run(false) } }, "Check without writing")),
    out);
}

// A code from the authenticator, for confirmation `n` of a reload's two;
// `typed`, when given, must be typed out too. Undefined when cancelled.
function askCode(n, body, action, typed = null) {
  return modal(`Reload the feed: confirmation ${n} of 2`, (close) => {
    const feed = typed ? h("input#reload-feed-id", { type: "text", autocomplete: "off", spellcheck: false }) : null;
    const code = h("input.code-input#reload-code", {
      type: "text", inputmode: "numeric", autocomplete: "one-time-code", maxlength: "6", pattern: "[0-9]{6}",
    });
    code.addEventListener("input", () => { code.value = code.value.replace(/\D/g, "").slice(0, 6); });
    const err = h("p.notice.error", { role: "alert", hidden: true });
    const fail = (text, el) => { err.hidden = false; err.textContent = text; el.focus(); };
    const submit = (ev) => {
      ev.preventDefault();
      if (feed && feed.value.trim() !== typed) return fail(`Type ${typed} to confirm.`, feed);
      if (code.value.length !== 6) return fail("Enter the 6-digit code your authenticator app shows.", code);
      close(code.value);
    };
    return h("form", { style: "display:grid;gap:12px", on: { submit } },
      ...body,
      feed ? h("label.field", { for: "reload-feed-id" }, h("span", `Type ${typed} to confirm`), feed) : null,
      h("label.field", { for: "reload-code" }, h("span", n === 1 ? "Code from your authenticator app" : "A new code from your authenticator app"), code),
      n === 2 ? h("p.hint", "The code you used for the check does not work again: wait for the app to show the next one.") : null,
      err,
      h("div.btn-row",
        h(n === 2 ? "button.btn.danger" : "button.btn", { type: "submit" }, action),
        h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
  });
}

// What a wrong code, or anything else a reload was refused for, means.
function codeError(e) {
  if (e instanceof ApiError && e.code === "invalid_code") {
    const left = e.details.attempts_left;
    return h("p.notice.error", `That code is not right.${left !== undefined ? ` ${left} more tr${left === 1 ? "y" : "ies"} before sign-in pauses for 10 minutes.` : ""} Codes change every 30 seconds; use the one showing now.`);
  }
  if (e instanceof ApiError && e.code === "code_reused") {
    return h("p.notice.error", "That code was already used. Wait for the app to show a new one.");
  }
  return h("p.notice.error", errorText(e));
}

const TABLE_WORDS = {
  gtfs_stop: "stop", gtfs_route: "route", gtfs_route_stop: "stop list row", gtfs_trip: "trip", gtfs_service: "service",
};

// The rows a reload deletes, in words: the main tables by name, the rest counted.
function replacedWords(replaced) {
  const r = replaced || {};
  const named = Object.entries(TABLE_WORDS).filter(([t]) => r[t]).map(([t, w]) => plural(r[t], w));
  const rest = Object.entries(r).filter(([t]) => !TABLE_WORDS[t]).reduce((n, [, v]) => n + v, 0);
  if (rest) named.push(plural(rest, "row") + " of other files");
  return named.length ? named.join(", ") : "nothing (the feed is empty)";
}

// Inactive routes (docs section 18.17) a zip that does not mark them would
// list again: any zip but this feed's own download.
function reactivatedView(ids) {
  if (!ids || !ids.length) return null;
  const shown = ids.slice(0, 12);
  return h("p.notice.warning", { id: "reload-reactivated" },
    h("strong", `${plural(ids.length, "inactive route")} would be active again: `),
    shown.join(", "), ids.length > shown.length ? ` and ${ids.length - shown.length} more` : "",
    ". This zip does not mark them inactive. Reload from the feed's own download to keep them inactive, or make them inactive again afterwards.");
}

function replacedView(replaced) {
  return h("p.notice.warning", h("strong", "A reload deletes what the feed holds now: "), replacedWords(replaced),
    ". The zip's rows below take their place.");
}

// An import in the background (docs section 18.15): the request is answered
// at once with a job, and its report polled for, so an import may take longer
// than the 30 s the load balancer and Pomerium in front of the editor wait.
// `waiting(seconds)` hears how long it has been running; `headers` go with the
// zip (a reload's code).
async function runImport(path, bytes, waiting, headers = {}) {
  const { job_id } = await postBytes(`${path}${path.includes("?") ? "&" : "?"}background=true`, bytes, "application/zip", headers);
  const started = Date.now();
  for (;;) {
    await new Promise((resolve) => setTimeout(resolve, 1500));
    const job = await get(`import-jobs/${enc(job_id)}`);
    if (job.status === "done") return job.report;
    if (job.status !== "running") {
      const e = job.error || {};
      throw new ApiError(e.status || 500, e.code || "import_failed", e.message || "The import failed.");
    }
    if (waiting) waiting(Math.round((Date.now() - started) / 1000));
  }
}

// A new feed from its GTFS zip (docs section 18.14): the feed the zip's
// feed_info.txt names, checked without writing, then made and loaded in one
// go. An admin's, like every import; it needs no feed chosen, so an admin can
// make the first one.
export function showNewFeed() {
  nameHere("New feed");
  if (!isAdmin()) {
    return clear(page(), h("div.page-inner", h("h1", "New feed from a GTFS zip"),
      h("p.notice", "Only an admin makes a feed. Ask an admin if a feed you need is missing.")));
  }
  const file = h("input", { type: "file", id: "new-feed-zip", accept: ".zip,application/zip" });
  const id = h("input", { type: "text", id: "new-feed-id", autocomplete: "off", spellcheck: false, placeholder: "from the zip's feed_info.txt" });
  const out = h("div", { "aria-live": "polite" });
  let bytes = null, name = null;
  file.addEventListener("change", async () => {
    const f = file.files && file.files[0];
    bytes = f ? await f.arrayBuffer() : null;
    name = f ? f.name : null;
    clear(out);
  });
  id.addEventListener("input", () => clear(out));
  const run = async (write) => {
    if (!bytes) return clear(out, h("p.notice.error", "Choose a GTFS zip first."));
    const qs = new URLSearchParams();
    if (id.value.trim()) qs.set("gtfs_id", id.value.trim());
    if (write) qs.set("seed", "true");
    const doing = write ? "Loading the feed" : `Checking ${name} without writing anything`;
    clear(out, h("p.empty", `${doing}…`));
    try {
      const res = await runImport(`feeds?${qs}`, bytes, (s) => clear(out, h("p.empty", `${doing}… ${s} s`)));
      if (res.seeded) {
        toast(`Feed ${res.gtfs_id} is loaded.`);
        // main.js offers it in the switcher and opens it
        window.dispatchEvent(new CustomEvent("feeds:changed", { detail: { select: res.gtfs_id } }));
        return;
      }
      const verb = res.new_feed ? "Make" : "Load";
      clear(out,
        h("p", h("strong", res.new_feed
          ? `This makes a new feed, ${res.gtfs_id}.`
          : `Feed ${res.gtfs_id} exists but has no rows yet: this loads it.`)),
        seedReport(res),
        !write && canWrite("seed", res)
          ? h("div.btn-row", h("button.btn", { type: "button", id: "new-feed-create", on: { click: () => run(true) } }, `${verb} feed ${res.gtfs_id}`))
          : null);
    } catch (e) {
      clear(out, h("p.notice.error", errorText(e)));
    }
  };
  clear(page(), h("div.page-inner.feed",
    h("div.title-block", h("h1", "New feed from a GTFS zip"),
      h("p.hint", "The whole feed from its zip: stops, stations, routes, trips, calendars and every other file. It is checked first without writing anything, and loaded only if exporting it again gives the zip back.")),
    h("p.notice", "GIMS serves the new feed from these tables only once it is switched to the database on Feed settings. After it is loaded it changes through drafts, like any other feed."),
    h("label.field", { for: "new-feed-zip" }, h("span", "GTFS zip"), file),
    h("label.field", { for: "new-feed-id" }, h("span", "Feed id - only for a zip whose feed_info.txt names none"), id),
    h("div.btn-row", h("button.btn.secondary", { type: "button", id: "new-feed-check", on: { click: () => run(false) } }, "Check without writing")),
    out));
}

const canWrite = (how, res) => (how === "drafts" ? res.errors === 0 && res.change_sets.length > 0 : res.errors === 0 && !Object.keys(res.round_trip || {}).length);

function seedReport(res) {
  return h("div.report",
    h("p", h("strong", res.seeded ? `Loaded: feed version ${res.feed_version}.` : res.dry_run ? "Checked; nothing was written." : "Not loaded.")),
    h("dl.facts", Object.entries(res.counts || {}).map(([k, n]) => [h("dt", k), h("dd", fmtCount(n))])),
    Object.keys(res.round_trip || {}).length
      ? h("div.notice.error", h("p", "Exported again, the feed would differ from the zip:"), h("ul", Object.entries(res.round_trip).map(([k, n]) => h("li", `${k}: ${fmtCount(n)}`))))
      : h("p.notice.ok", "Exported again, the feed gives the zip back."),
    res.stage_stops
      ? h("p", `Fare stages read from the stop headsigns: ${plural(res.stage_stops, "stage stop")}. Each stage is named after its stage stop; the headsigns are served from the stages.`)
      : null,
    res.findings && res.findings.length ? h("details", h("summary", plural(res.findings.length, "finding")), h("ul", res.findings.slice(0, 100).map((f) => h("li", `${f.level}: ${f.code} ${f.message}`)))) : null,
    res.validation ? reportView(res.validation, "The feed report on the zip.") : null);
}

function draftsReport(res) {
  const so = res.stop_orders || {};
  return h("div.report",
    h("p", h("strong", res.change_sets.length
      ? `${res.dry_run ? "Would draft" : "Drafted"} ${plural(res.change_sets.length, "change set")}.`
      : "The feed already holds what the zip says.")),
    res.next ? h("p.notice.warning", res.next) : null,
    res.change_sets.length ? h("ul.list", res.change_sets.map((s) => h("li.list-item",
      s.change_set_id ? h("a", { href: `#/drafts/${enc(s.change_set_id)}` }, s.title) : h("span", s.title),
      h("span.hint", ` ${plural(s.changes, "change")}${s.trips ? `, ${plural(s.trips, "trip")} on ${plural(s.routes, "route")}` : ""}`)))) : null,
    h("dl.facts",
      h("dt", "Stop orders the feed has"), h("dd", fmtCount(so.same)),
      h("dt", "Split off them"), h("dd", fmtCount(so.split)),
      h("dt", "Routes whose trips move onto the feed's stop order"), h("dd", fmtCount(so.moved)),
      h("dt", "Routes left as they are"), h("dd", `${fmtCount(so.routes_left)}${(so.routes_left_sample || []).length ? ` (${so.routes_left_sample.join(", ")}…)` : ""}`)),
    Object.keys(res.differences || {}).length ? h("details", h("summary", "How the zip differs from the feed (stops and routes are never written)"),
      h("ul", Object.entries(res.differences).map(([k, n]) => h("li", `${k}: ${fmtCount(n)}`)))) : null,
    res.errors ? h("div.notice.error", h("p", `${plural(res.errors, "problem")} stop the import:`), h("ul", res.problems.filter((p) => p.level !== "warning").slice(0, 50).map((p) => h("li", `${p.code}: ${p.message}`)))) : null,
    res.validation ? reportView(res.validation, "The feed report on the zip.") : null);
}
