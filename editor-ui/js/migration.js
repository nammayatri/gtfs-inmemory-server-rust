// Moving master's work to prod (docs section 23). Master and prod each have
// their own database. On master an admin downloads a committed draft, or the
// whole feed, as a migration file; on prod an admin reads the file in -
// checked first, nothing written - and gets ordinary prod drafts that a second
// person approves and commits. What prod changed itself is kept and listed,
// and can be taken from master one by one.
import { get, enc, runImport } from "./api.js";
import { API_BASE } from "./config.js";
import { state, isAdmin } from "./state.js";
import { h, clear, fmtCount, plural, errorText, fmtDate } from "./util.js";

// Which side of a migration this editor is: master writes files, prod reads them.
export const environment = () => (state.me && state.me.environment) || "prod";

const SCOPES = [
  ["stops", "Stops and stations"],
  ["routes", "Routes"],
  ["stages", "Stages, and each route's list of them"],
  ["timetables", "Timetables: calendars and trips"],
];

const STEP_TITLE = {
  stops_routes: "Stops, stations and routes",
  stages: "Stages",
  route_stages: "The routes' stage lists",
  switch: "Serve the feed from its stages",
  timetables: "Timetables",
};

const REASON = {
  changed_on_prod: "changed on prod",
  deleted_on_prod: "taken out on prod",
  exists_on_prod: "already on prod, otherwise",
  missing_on_prod: "not on prod",
  differs_on_prod: "differs on prod",
};

// A committed draft's "Download for prod" (its page, on master).
export function draftDownload(cs) {
  if (environment() !== "master" || !isAdmin() || cs.status !== "committed") return null;
  return h("a.btn.secondary", { href: `${API_BASE}change-sets/${enc(cs.change_set_id)}/migration`, download: "", id: "draft-to-prod" },
    "Download for prod");
}

// The Feed page's section: on master the whole feed as a file, on prod reading
// one in. An admin's.
export function migrationSection() {
  if (!isAdmin()) return null;
  return environment() === "master" ? exportSection() : importSection();
}

function exportSection() {
  const g = enc(state.feedId);
  const inputs = SCOPES.map(([k]) => h("input", { type: "checkbox", value: k, id: `scope-${k}`, checked: true }));
  const link = h("a.btn.secondary", { id: "download-migration", download: "" }, "Download for prod");
  const none = h("p.hint", { hidden: true }, "Choose at least one.");
  const sync = () => {
    // the scopes from the page's own list, never read back from the DOM
    const picked = SCOPES.filter((_, i) => inputs[i].checked).map(([k]) => enc(k));
    link.href = `${API_BASE}feeds/${g}/migration?scopes=${picked.join(",")}`;
    link.hidden = !picked.length;
    none.hidden = !!picked.length;
  };
  inputs.forEach((i) => i.addEventListener("change", sync));
  sync();
  return h("section.feed-section#migration",
    h("h2", "Move to prod"),
    h("p.hint", "Master and prod each have their own database. Download what prod should have, then read the file in on prod's Feed page: it becomes prod drafts that a second person approves and commits there. One committed draft downloads from its own page."),
    h("fieldset.migration-scopes", h("legend", "What the file carries"),
      SCOPES.map(([, label], i) => h("label.check", { for: inputs[i].id }, inputs[i], label))),
    h("div.btn-row", link, none));
}

function importSection() {
  const g = enc(state.feedId);
  const file = h("input", { type: "file", id: "migration-file", accept: ".zip,application/zip" });
  const since = h("input", { type: "number", id: "migration-since", min: "0", step: "1", inputmode: "numeric" });
  const out = h("div", { "aria-live": "polite" });
  const history = h("div", h("p.empty", "Loading…"));
  let bytes = null, name = null;
  file.addEventListener("change", async () => {
    const f = file.files && file.files[0];
    bytes = f ? await f.arrayBuffer() : null;
    name = f ? f.name : null;
    clear(out);
  });

  const run = async (write, take = []) => {
    if (!bytes) return clear(out, h("p.notice.error", "Choose a migration file first."));
    const qs = new URLSearchParams({ dry_run: write ? "false" : "true" });
    if (since.value.trim()) qs.set("since_version", since.value.trim());
    if (take.length) qs.set("take", take.join(","));
    const doing = write ? `Drafting from ${name}` : `Checking ${name}`;
    clear(out, h("p.empty", `${doing}…`));
    let report;
    try {
      report = await runImport(`feeds/${g}/migrations?${qs}`, bytes, (s) => clear(out, h("p.empty", `${doing}… ${s} s`)));
    } catch (e) {
      return clear(out, h("p.notice.error", errorText(e)));
    }
    clear(out, reportView(report, write, (picked) => run(true, picked)));
    if (write) loadHistory();
  };

  const loadHistory = async () => {
    try {
      const res = await get(`feeds/${g}/migrations`);
      clear(history, historyView(res.items));
    } catch (e) {
      clear(history, h("p.notice.error", errorText(e)));
    }
  };
  loadHistory();

  return h("section.feed-section#migration",
    h("h2", "Bring from master"),
    h("p.hint", "Read in a file downloaded from master: one committed draft, or the whole feed. It is checked first and nothing is written; then it becomes prod drafts that a second person approves and commits. What prod changed itself is kept and listed: tick any you want master's version of. The whole feed comes a step at a time: commit each step's drafts, then read the same file in again."),
    h("label.field", { for: "migration-file" }, h("span", "Migration file"), file),
    h("label.field", { for: "migration-since" },
      h("span", "Prod's own edits count from version"), since,
      h("span.hint", "Needed by the feed's first whole-feed migration: what prod committed after this version is kept as prod's own. Later ones remember it.")),
    h("div.btn-row", h("button.btn.secondary", { type: "button", id: "migration-check", on: { click: () => run(false) } }, "Check")),
    out,
    h("h3", "Read in before"),
    history);
}

const what = (w) => {
  if (!w) return "";
  const name = w.name || w.short_name || w.title;
  const key = w.key || w.entity_key || "";
  const into = w.into ? ` into ${w.into}` : "";
  const op = w.op ? `${w.op} ` : "";
  return `${w.entity || ""} ${op}${key}${into}${name ? ` (${name})` : ""}`.trim();
};

// a value as a short line: a scalar as it is, anything else as compact JSON
const shortValue = (v) => {
  if (v === null || v === undefined) return "—";
  if (typeof v !== "object") return String(v);
  const s = JSON.stringify(v);
  return s.length > 160 ? `${s.slice(0, 157)}…` : s;
};

function sides(c) {
  const p = c.prod, m = c.master;
  if (p && m && typeof p === "object" && typeof m === "object" && !Array.isArray(p)) {
    const keys = [...new Set([...Object.keys(p), ...Object.keys(m)])].slice(0, 6);
    return h("dl.facts", keys.map((k) => [
      h("dt", k), h("dd", `prod: ${shortValue(p[k])} · master had: ${shortValue(m[k])}`)]));
  }
  return c.message ? h("span", c.message) : "";
}

function reportView(r, written, onWrite) {
  const src = r.source || {};
  const head = r.kind === "change_set"
    ? `Master's draft "${r.change_set ? r.change_set.title : ""}", committed as master version ${r.change_set ? r.change_set.committed_version : "?"}; written ${fmtDate(src.exported_at)} by ${src.exported_by}.`
    : `Master's feed ${r.gtfs_id} at version ${src.version}; written ${fmtDate(src.exported_at)} by ${src.exported_by}.${r.since_version !== undefined ? ` Prod's own edits count from version ${r.since_version}.` : ""}`;

  // each Take box with the conflict id from the report, kept here rather than
  // read back from the box
  const takes = [];
  const conflictRows = (r.conflicts || []).map((c) => {
    const box = c.id ? h("input", { type: "checkbox", id: `take-${c.id}`, "aria-label": `Take master's version of ${c.id}` }) : null;
    if (box) takes.push({ box, id: c.id });
    return h("tr",
      h("td", box),
      h("td", h("code", c.id || ""), h("div.hint", what(c.what))),
      h("td", REASON[c.reason] || c.reason || ""),
      h("td", sides(c)));
  });
  const count = (k) => (r.counts && r.counts[k]) || 0;

  const steps = (r.steps || []).length ? h("div.table-wrap", h("table.report-table.migration-steps",
    h("thead", h("tr", h("th", "Step"), h("th", "Would change"), h("th.num", "Prod's own"), h("th", ""))),
    h("tbody", r.steps.map((s) => h("tr", { class: s.drafted ? "current" : null },
      h("td", STEP_TITLE[s.step] || s.title),
      h("td", Object.keys(s.planned).length ? Object.entries(s.planned).map(([k, n]) => `${fmtCount(n)} ${k}`).join(", ") : "nothing"),
      h("td.num", fmtCount(s.conflicts)),
      h("td", s.drafted ? h("span.chip.approved", written ? "drafted" : "would be drafted")
        : s.dropped ? h("span.chip.rejected", `${fmtCount(s.dropped)} refused`)
        : s.waiting ? h("span.hint", s.waiting) : "")))))) : null;

  const sets = r.change_sets.length
    ? h("ul.migration-sets", r.change_sets.map((s) => h("li",
      s.change_set_id ? h("a", { href: `#/drafts/${enc(s.change_set_id)}` }, s.title) : s.title,
      ` — ${plural(s.changes, "change")}`)))
    : h("p.empty", written ? "No draft was made." : "Nothing would be drafted.");

  const canWrite = !written && (r.change_sets.length || takes.length) && !(r.problems || []).length;
  const write = canWrite ? h("button.btn", { type: "button", id: "migration-write", on: { click: () => {
    onWrite(takes.filter((t) => t.box.checked).map((t) => t.id));
  } } }, "Make the drafts") : null;

  return h("div.migration-report",
    h("p", head),
    h("p", h("strong", written ? "Drafted: " : "Would draft: "),
      `${plural(count("added"), "change")}; ${plural(count("conflicts"), "conflict")} left as prod has them`,
      count("done") ? `; ${fmtCount(count("done"))} prod already has` : "",
      count("dropped") ? `; ${fmtCount(count("dropped"))} refused by prod` : "",
      count("taken") ? `; ${fmtCount(count("taken"))} taken from master` : "", "."),
    steps,
    sets,
    (r.problems || []).length ? h("div.notice.error", h("p", h("strong", "Nothing can be written until these are fixed:")),
      h("ul", r.problems.map((p) => h("li", p.message || p.code)))) : null,
    conflictRows.length ? h("section",
      h("h3", `Prod's own: ${plural(r.conflicts.length, "conflict")}${count("conflicts") > r.conflicts.length ? ` (${fmtCount(count("conflicts"))} in all)` : ""}`),
      h("p.hint", "Left as prod has them. Tick any you want master's version of; it goes into the drafts made below."),
      h("div.table-wrap", h("table.report-table.migration-conflicts",
        h("thead", h("tr", h("th", "Take"), h("th", "What"), h("th", "Why"), h("th", "Prod and master"))),
        h("tbody", conflictRows)))) : null,
    (r.dropped || []).length ? h("section",
      h("h3", `Refused by prod: ${plural(count("dropped"), "change")}`),
      h("p.hint", "Taken out of the drafts, which keep the rest."),
      h("ul.migration-dropped", r.dropped.map((d) => h("li", h("code", d.id), ` ${what(d.what)}: `,
        (d.findings || []).map((f) => f.message || f.code).join("; "))))) : null,
    (r.notes || []).length ? h("details.migration-notes", h("summary", plural(r.notes.length, "note")),
      h("ul", r.notes.map((n) => h("li", n.message || n.code, n.count ? ` (${fmtCount(n.count)})` : ""))))
      : null,
    r.next ? h("p.notice", r.next) : null,
    write ? h("div.btn-row", write) : null);
}

function historyView(items) {
  if (!items.length) return h("p.empty", "Nothing has been read in from master yet.");
  return h("div.table-wrap", h("table.report-table.migration-history",
    h("thead", h("tr", h("th", "When"), h("th", "By"), h("th", "What"), h("th", "Drafts"))),
    h("tbody", items.map((m) => {
      const src = (m.source && m.source.source) || {};
      const set = m.source && m.source.change_set;
      return h("tr",
        h("td", fmtDate(m.imported_at)),
        h("td", m.imported_by || ""),
        h("td", m.kind === "change_set"
          ? `Draft "${set ? set.title : ""}" (master v${set ? set.committed_version : "?"})`
          : `Whole feed at master v${src.version}: ${STEP_TITLE[m.step] || m.step}`),
        h("td", h("ul.migration-sets", (m.change_sets || []).map((s) => h("li",
          h("a", { href: `#/drafts/${enc(s.change_set_id)}` }, s.title), " ",
          h("span.chip", { class: s.status }, s.status))))));
    }))));
}
