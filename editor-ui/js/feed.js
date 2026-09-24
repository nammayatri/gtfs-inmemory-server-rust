// The feed as a whole (docs section 18): what it serves from, its GTFS zip as
// the tables give it, the feed report (what it breaks of the GTFS reference),
// and - for an admin - bringing a GTFS zip in: a seed for a feed with no rows,
// or change sets for one that has them, always checked first without writing.
import { get, postBytes, enc } from "./api.js";
import { API_BASE } from "./config.js";
import { state, isAdmin } from "./state.js";
import { h, clear, fmtCount, plural, errorText } from "./util.js";
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
      h("p.hint", "The zip the tables give: every file of the reference the feed has, as passengers' apps would read it."),
      h("div.btn-row",
        h("a.btn.secondary", { href: `${API_BASE}feeds/${g}/gtfs.zip`, download: `${state.feedId}.gtfs.zip`, id: "download-zip" }, "Download the GTFS zip"),
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
  const run = async (write) => {
    if (!bytes) return clear(out, h("p.notice.error", "Choose a GTFS zip first."));
    const how = box.querySelector("input[name=import-mode]:checked").value;
    const qs = new URLSearchParams();
    if (how === "drafts") {
      qs.set("mode", "drafts");
      qs.set("dry_run", String(!write));
      if (files.value.trim()) qs.set("files", files.value.trim());
    } else if (write) {
      qs.set("seed", "true");
    }
    clear(out, h("p.empty", write ? "Importing…" : `Checking ${name} without writing anything…`));
    try {
      const res = await postBytes(`feeds/${enc(state.feedId)}/import?${qs}`, bytes);
      clear(out, how === "drafts" ? draftsReport(res) : seedReport(res), !write && canWrite(how, res)
        ? h("div.btn-row", h("button.btn", { type: "button", id: "import-write", on: { click: () => run(true) } }, how === "drafts" ? "Draft these changes" : "Load the feed"))
        : null);
    } catch (e) {
      clear(out, h("p.notice.error", errorText(e)));
    }
  };
  clear(box,
    h("p.hint", "Only an admin imports. Nothing is written until you have seen what the import would do."),
    h("label.field", { for: "import-zip" }, h("span", "GTFS zip"), file),
    h("div.radio-cards", { role: "radiogroup", "aria-label": "How" },
      mode("seed", "Load an empty feed", "For a feed with no rows yet: the whole zip in one go, checked by exporting it again.", true),
      mode("drafts", "Bring into drafts", "For a feed that has rows: change sets for review. Stops, routes and stop orders are compared, never written.", false)),
    h("label.field", { for: "import-files" }, h("span", "Only these files (drafts)"), files),
    h("div.btn-row", h("button.btn.secondary", { type: "button", id: "import-check", on: { click: () => run(false) } }, "Check without writing")),
    out);
}

const canWrite = (how, res) => (how === "drafts" ? res.errors === 0 && res.change_sets.length > 0 : res.errors === 0 && !Object.keys(res.round_trip || {}).length);

function seedReport(res) {
  return h("div.report",
    h("p", h("strong", res.seeded ? `Loaded: feed version ${res.feed_version}.` : res.dry_run ? "Checked; nothing was written." : "Not loaded.")),
    h("dl.facts", Object.entries(res.counts || {}).map(([k, n]) => [h("dt", k), h("dd", fmtCount(n))])),
    Object.keys(res.round_trip || {}).length
      ? h("div.notice.error", h("p", "Exported again, the feed would differ from the zip:"), h("ul", Object.entries(res.round_trip).map(([k, n]) => h("li", `${k}: ${fmtCount(n)}`))))
      : h("p.notice.ok", "Exported again, the feed gives the zip back."),
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
