// Bulk import: new stops, new routes, whole route stop lists, the details of
// existing stops (platform label, description, name), or the map lines of
// existing routes, from a CSV file.
// The file is read in the browser, checked by the server against the draft
// without changing anything (dry run), and only then added to the draft in one
// go. The draft is reviewed and committed like any other.
import { get, post, enc, ApiError } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtCount, fmtMetres, plural, decodePolyline, STOP_TYPE_LABEL } from "./util.js";
import { parseCsv, toCsv, CsvError } from "./csv.js";
import * as map from "./map.js";
import { requireDraft, refreshDraft, useDraft } from "./drafts.js";

const page = () => document.getElementById("page");
const MAX_ROWS = 5000;
const TABLE_PAGE = 100;
const STOP_TYPES = ["NEW STOP", "INTERMEDIATE STOP", "JUMP STOP", "HIDDEN STOP"];

const text = (v) => v.trim();
const optional = (v) => (v.trim() === "" ? undefined : v.trim());
function number(name) {
  return (v) => {
    const s = v.trim();
    const n = Number(s);
    if (s === "" || !Number.isFinite(n)) throw new Error(`${name} must be a number, for example ${name === "lat" ? "13.0827" : "80.2707"}.`);
    return n;
  };
}
function whole(name) {
  return (v) => {
    const s = v.trim();
    if (!/^-?\d+$/.test(s)) throw new Error(`${name} must be a whole number.`);
    return Number(s);
  };
}
function stopType(v) {
  const s = v.trim().toUpperCase().replace(/\s+/g, " ");
  if (!STOP_TYPES.includes(s)) throw new Error(`stop_type must be one of ${STOP_TYPES.join(", ")}.`);
  return s;
}
const YES = ["yes", "y", "true", "1"], NO = ["no", "n", "false", "0"];
function yesNo(v) {
  const s = v.trim().toLowerCase();
  if (s === "") return undefined;
  if (YES.includes(s)) return true;
  if (NO.includes(s)) return false;
  throw new Error("replace must be yes or no.");
}
// An encoded line is read here only to say how long it is and to draw it; the
// server is the one that decides whether it may be stored.
function encodedLine(v) {
  const s = v.trim();
  if (s === "") throw new Error("encoded_polyline is empty.");
  let pts;
  try {
    pts = decodePolyline(s);
  } catch {
    throw new Error("encoded_polyline is not an encoded polyline. Paste the line exactly as it was given to you.");
  }
  if (pts.length < 2) throw new Error(`encoded_polyline has ${pts.length === 1 ? "only one point" : "no points"}; a map line needs at least two.`);
  return s;
}
function polylineSource(v) {
  const s = v.trim().toLowerCase();
  if (s === "") return undefined;
  if (!["osrm", "manual", "upload"].includes(s)) throw new Error("polyline_source must be osrm, manual or upload.");
  return s;
}

// What each kind of file holds. `read` turns a cell into the API value (undefined
// leaves the field out) or throws a message for that row.
const KINDS = {
  stops: {
    label: "Stops",
    what: "New stops. Each row becomes one new stop in the draft.",
    columns: [
      { name: "stop_id", required: false, read: optional, help: "Optional. Leave empty and the editor makes an id." },
      { name: "name", required: true, read: text, help: "The name passengers see." },
      { name: "lat", required: true, read: number("lat"), help: "Latitude, for example 13.0827." },
      { name: "lon", required: true, read: number("lon"), help: "Longitude, for example 80.2707." },
      { name: "platform_code", required: false, read: optional, help: "Optional platform label, for example Towards Guindy." },
    ],
    show: ["stop_id", "name", "lat", "lon", "platform_code"],
  },
  routes: {
    label: "Routes",
    what: "New routes. Each row becomes one new route; import their stop lists next.",
    columns: [
      { name: "route_id", required: true, read: text, help: "A new, unused id." },
      { name: "short_name", required: true, read: text, help: "The route number passengers see, for example 570X." },
      { name: "long_name", required: false, read: optional, help: "Optional route name." },
      { name: "color", required: false, read: optional, help: "Optional colour as #RRGGBB." },
    ],
    show: ["route_id", "short_name", "long_name", "color"],
  },
  route_stops: {
    label: "Route stop lists",
    what: "Whole stop lists. All rows of one route_id replace that route's stop list, in sequence order.",
    columns: [
      { name: "route_id", required: true, read: text, help: "An existing route, or one new in your draft." },
      { name: "sequence", required: true, read: whole("sequence"), help: "Position in the route: 1, 2, 3…" },
      { name: "stop_id", required: true, read: text, help: "An existing stop, or one new in your draft." },
      { name: "stop_type", required: true, read: stopType, help: STOP_TYPES.join(", ") },
      { name: "stage_no", required: true, read: whole("stage_no"), help: "Fare stage number." },
      { name: "stage_name", required: true, read: text, help: "Fare stage name." },
    ],
    show: ["route_id", "sequence", "stop_id", "stop_type", "stage_no", "stage_name"],
  },
  stop_updates: {
    label: "Stop details (platform label, description)",
    short: "stop details",
    what: "Details of stops that exist. Each row changes one stop; an empty cell leaves that detail as it is.",
    columns: [
      { name: "stop_id", required: true, read: text, help: "An existing stop. A station's id may be used for its description or name." },
      { name: "platform_code", required: false, read: optional, help: "The platform or direction passengers see, for example Towards Guindy. At most 120 characters. Not for a station." },
      { name: "description", required: false, read: optional, help: "Where the stop is, in words. At most 500 characters." },
      { name: "name", required: false, read: optional, help: "A new name. Leave empty to keep the name." },
    ],
    show: ["stop_id", "platform_code", "description", "name"],
    // the server says which stop each row names, and where it is
    now: true,
  },
  polylines: {
    label: "Route map lines",
    short: "route map lines",
    what: "Map lines for routes that exist. Each row gives one route the road path drawn between its stops.",
    columns: [
      { name: "route_id", required: true, read: text, help: "An existing route, or one new in your draft." },
      { name: "encoded_polyline", required: true, read: encodedLine, help: "The line as an encoded polyline (Google precision 5). At least two points, all of them in the area these buses run in." },
      { name: "polyline_source", required: false, read: polylineSource, help: "Where the line came from: osrm, manual or upload. Empty means upload." },
      { name: "replace", required: false, read: yesNo, help: "yes to put this line over the one the route already has. Empty leaves a route that has a line alone." },
    ],
    show: ["route_id", "polyline_source", "replace"],
    // the server measures each line against the route's stops
    line: true,
  },
};

// A little edit distance, to suggest the column name someone meant.
function closeTo(word, options) {
  const d = (a, b) => {
    const m = Array.from({ length: a.length + 1 }, (_, i) => [i, ...Array(b.length).fill(0)]);
    for (let j = 1; j <= b.length; j++) m[0][j] = j;
    for (let i = 1; i <= a.length; i++) for (let j = 1; j <= b.length; j++) {
      m[i][j] = Math.min(m[i - 1][j] + 1, m[i][j - 1] + 1, m[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
    }
    return m[a.length][b.length];
  };
  const best = options.map((o) => [o, d(word, o)]).sort((x, y) => x[1] - y[1])[0];
  return best && best[1] <= 3 ? best[0] : null;
}

// CSV text -> {rows, sheetRows, fileProblems, rowProblems}. Rows are the API
// row objects; sheetRows[i] is the spreadsheet row number of rows[i] (the header
// is row 1). Nothing here decides what is valid data; it only reads the file.
export function readFile(kind, csvText) {
  const spec = KINDS[kind];
  const fileProblems = [];
  let parsed;
  try {
    parsed = parseCsv(csvText);
  } catch (e) {
    if (e instanceof CsvError) return { rows: [], sheetRows: [], fileProblems: [e.message], rowProblems: new Map() };
    throw e;
  }
  const records = parsed.records;
  const headerAt = records.findIndex((r) => r.some((c) => c.trim() !== ""));
  if (headerAt < 0) return { rows: [], sheetRows: [], fileProblems: ["The file is empty."], rowProblems: new Map() };
  const header = records[headerAt].map((c) => c.trim().toLowerCase());
  const known = spec.columns.map((c) => c.name);
  const seen = new Set();
  header.forEach((name) => {
    if (!name) return;
    if (seen.has(name)) fileProblems.push(`The column ${name} appears twice.`);
    seen.add(name);
    if (!known.includes(name)) {
      const guess = closeTo(name, known);
      fileProblems.push(`The column “${name}” is not used for ${spec.short || spec.label.toLowerCase()}.${guess ? ` Did you mean “${guess}”?` : ""} Use the template's column names.`);
    }
  });
  if (header.some((c, i) => !c && records.slice(headerAt + 1).some((r) => (r[i] || "").trim()))) {
    fileProblems.push("A column with values has no name in the header row.");
  }
  const missing = spec.columns.filter((c) => c.required && !header.includes(c.name)).map((c) => c.name);
  if (missing.length) fileProblems.push(`The file has no ${missing.join(", ")} column${missing.length === 1 ? "" : "s"}. The header row must name them.`);
  const rows = [], sheetRows = [], rowProblems = new Map();
  if (fileProblems.length) return { rows, sheetRows, fileProblems, rowProblems };
  for (let r = headerAt + 1; r < records.length; r++) {
    const rec = records[r];
    if (rec.every((c) => c.trim() === "")) continue;           // blank lines
    const row = {}, problems = [];
    if (rec.length > header.length && rec.slice(header.length).some((c) => c.trim() !== "")) {
      problems.push(`This row has ${rec.length} values but the header has ${header.length} columns.`);
    }
    spec.columns.forEach((col) => {
      const i = header.indexOf(col.name);
      if (i < 0) return;
      const raw = rec[i] ?? "";
      if (col.required && raw.trim() === "") { problems.push(`${col.name} is empty.`); return; }
      try {
        const v = col.read(raw);
        if (v !== undefined) row[col.name] = v;
      } catch (e) {
        problems.push(e.message);
      }
    });
    sheetRows.push(r + 1);
    if (problems.length) rowProblems.set(rows.length, problems);
    rows.push(row);
  }
  if (!rows.length) fileProblems.push("The file has a header row but no data rows.");
  if (rows.length > MAX_ROWS) fileProblems.push(`The file has ${fmtCount(rows.length)} rows. Import at most ${fmtCount(MAX_ROWS)} at a time; split the file.`);
  return { rows, sheetRows, fileProblems, rowProblems };
}

const templates = {};
function templateHref(kind) {
  if (!templates[kind]) {
    // a byte order mark, so spreadsheet programs open Tamil names as UTF-8
    const blob = new Blob(["\ufeff" + toCsv([KINDS[kind].columns.map((c) => c.name)])], { type: "text/csv;charset=utf-8" });
    templates[kind] = URL.createObjectURL(blob);
  }
  return templates[kind];
}

// ------------------------------------------------------------------ page
const view = { kind: "stops", file: null, read: null, result: null, resultFor: null, filter: "problems", page: 1, added: null };

export function showImport(kind) {
  if (kind && KINDS[kind]) view.kind = kind;
  if (!can("editor")) {
    return clear(page(), h("div.page-inner", h("h1", "Import from a CSV file"), h("p.notice", "Importing needs the editor role. Ask an admin.")));
  }
  const root = h("div.page-inner.import");
  clear(page(), root);
  let insetMap = null;

  const render = () => {
    if (insetMap) { insetMap.remove(); insetMap = null; }
    const spec = KINDS[view.kind];
    const kinds = h("div.radio-cards.kind-cards", { role: "radiogroup", "aria-label": "What the file holds" },
      Object.entries(KINDS).map(([key, k]) => h("label.radio-card", { for: `kind-${key}` },
        h("input", { type: "radio", name: "import-kind", id: `kind-${key}`, value: key, checked: key === view.kind,
          on: { change: () => { view.kind = key; resetFile(); render(); } } }),
        h("span.radio-card-body", h("span.radio-card-title", k.label), h("span.hint", k.what)))));

    const fileInput = h("input.visually-hidden", { type: "file", id: "import-file", accept: ".csv,text/csv" });
    fileInput.addEventListener("change", () => {
      const file = fileInput.files && fileInput.files[0];
      fileInput.value = "";            // choosing the same (fixed) file again still counts
      if (file) takeFile(file);
    });
    const drop = h("label.dropzone", { for: "import-file" },
      h("span.dropzone-title", view.file ? "Choose the fixed file, or another file" : "Choose a CSV file"),
      h("span.hint", "or drop it here. UTF-8, with a header row, at most 5,000 rows."));
    drop.addEventListener("dragover", (ev) => { ev.preventDefault(); drop.classList.add("over"); });
    drop.addEventListener("dragleave", () => drop.classList.remove("over"));
    drop.addEventListener("drop", (ev) => {
      ev.preventDefault();
      drop.classList.remove("over");
      const file = ev.dataTransfer.files && ev.dataTransfer.files[0];
      if (file) takeFile(file);
    });

    clear(root,
      h("div.title-block",
        h("h1", "Import from a CSV file"),
        h("p.hint", "Add many new stops or routes, replace route stop lists, set the platform labels and descriptions of many stops, or give many routes their map line, at once. The file is checked first and nothing is added until you choose to. Added changes wait in your draft until someone else approves it and it is committed.")),
      view.added ? h("div.notice.ok", { role: "status" },
        h("p", h("strong", view.added.message)),
        h("div.btn-row", h("a.btn.small", { href: `#/drafts/${enc(view.added.draftId)}` }, "Open the draft"))) : null,
      h("section.import-step",
        h("h2", "1. What does the file hold?"),
        kinds,
        h("details.columns",
          h("summary", `Columns for ${spec.short || spec.label.toLowerCase()}`),
          h("table.column-table", h("tbody", spec.columns.map((c) => h("tr", h("th", { scope: "row" }, h("code", c.name)), h("td", c.required ? "Required" : "Optional"), h("td", c.help)))))),
        h("div.btn-row", h("a.btn.secondary.small", { href: templateHref(view.kind), download: `${view.kind}-template.csv` }, `Download the ${spec.short || spec.label.toLowerCase()} template`))),
      h("section.import-step",
        h("h2", "2. Choose the file"),
        fileInput, drop,
        view.file ? h("p", h("strong", view.file.name), ` · ${plural(view.read ? view.read.rows.length : 0, "row")}`) : null),
      view.read ? h("section.import-step", h("h2", "3. Check"), checkSection()) : null);

    // the map's box is on the page and sized by now, so the map can be made at once
    if (view.result && (view.kind === "stops" || KINDS[view.kind].now)) {
      const el = root.querySelector(".import-map");
      if (el) insetMap = stopsMap(el);
    }
  };

  const resetFile = () => Object.assign(view, { file: null, read: null, result: null, resultFor: null, filter: "problems", page: 1 });

  async function takeFile(file) {
    view.added = null;
    let textContent;
    try {
      textContent = await file.text();
    } catch (e) {
      toast(`The file could not be read: ${e.message}`, "error");
      return;
    }
    Object.assign(view, { file: { name: file.name }, read: readFile(view.kind, textContent), result: null, resultFor: null, filter: "problems", page: 1 });
    render();
    if (!view.read.fileProblems.length && !view.read.rowProblems.size) preview();
  }

  async function preview() {
    const draft = await requireDraft("A file is checked against a draft, and its changes go into that draft. Nothing changes for passengers until someone else approves it and it is committed.");
    if (!draft) {
      toast("Choose or start a draft to check the file against.", "error");
      render();
      return;
    }
    view.checking = true;
    render();
    try {
      const res = await post(`change-sets/${enc(draft.change_set_id)}/bulk`, { kind: view.kind, rows: view.read.rows, dry_run: true });
      Object.assign(view, { result: res, resultFor: { read: view.read, draftId: draft.change_set_id }, page: 1 });
      view.filter = res.summary.errors || res.summary.warnings ? "problems" : "all";
    } catch (e) {
      view.result = null;
      toast(e.message, "error");
    } finally {
      view.checking = false;
      render();
    }
  }

  async function addToDraft() {
    const res = view.result;
    const draft = state.draft;
    if (!draft || draft.change_set_id !== view.resultFor.draftId) {
      toast("The draft changed since the check. Check the file again.", "error");
      return;
    }
    const n = res.summary.changes;
    const ok = await confirmDialog(`Add ${plural(n, "change")} to the draft?`,
      `${plural(n, "change")} from ${view.file.name} go into draft “${draft.title}”.${res.summary.warnings ? ` ${plural(res.summary.warnings, "row")} ${res.summary.warnings === 1 ? "has" : "have"} warnings; they do not block adding.` : ""} They are checked again as they are added.`,
      { confirm: `Add ${fmtCount(n)} to draft` });
    if (!ok) return;
    try {
      const done = await post(`change-sets/${enc(draft.change_set_id)}/bulk`, { kind: view.kind, rows: view.read.rows, dry_run: false });
      if (done && done.change_set && done.change_set.change_set_id) useDraft(done.change_set);
      else await refreshDraft();
      view.added = { message: `Added ${plural(done.summary.changes, "change")} from ${view.file.name} to draft “${draft.title}”.`, draftId: draft.change_set_id };
      resetFile();
      map.refreshStops();
      render();
      window.scrollTo(0, 0);
    } catch (e) {
      if (e instanceof ApiError && e.code === "bulk_has_errors") {
        toast("The server found new errors, so nothing was added. The check below is up to date.", "error");
        preview();
      } else {
        toast(e.message, "error");
      }
    }
  }

  function checkSection() {
    const read = view.read;
    if (read.fileProblems.length) {
      return h("div.notice.error", { role: "alert" },
        h("p", h("strong", "The file cannot be checked yet")),
        h("ul", read.fileProblems.map((x) => h("li", x))),
        h("p", "Fix the file and choose it again."));
    }
    if (read.rowProblems.size) {
      const rows = [...read.rowProblems.entries()].map(([i, messages]) => ({ i, status: "error", messages: messages.map((message) => ({ message, level: "error" })) }));
      return [
        h("div.notice.error", { role: "alert" },
          h("p", h("strong", `${plural(read.rowProblems.size, "row")} cannot be read`)),
          h("p", "Fix these rows and choose the file again. The full check runs once every row reads cleanly.")),
        resultTable(rows, read)];
    }
    if (view.checking) return h("p.empty", "Checking the file against your draft…");
    const res = view.result;
    if (!res) {
      return h("div.btn-row", h("button.btn", { type: "button", on: { click: preview } }, "Check the file"));
    }
    const s = res.summary;
    const spec = KINDS[view.kind];
    const rows = res.rows.map((r) => ({ i: r.row - 1, status: r.status, messages: (r.messages || []).map((m) => ({ ...m, level: m.level || (r.status === "warning" ? "warning" : "error") })), change: r.change, stop: r.stop || null, polyline: r.polyline || null }));
    const stale = !state.draft || state.draft.change_set_id !== view.resultFor.draftId;
    return [
      h("div.summary-chips", { role: "status" },
        h("span.chip", `${plural(s.rows, "row")}`),
        h("span.chip.committed", `${fmtCount(s.ok)} ok`),
        h("span", { class: `chip ${s.warnings ? "submitted" : ""}` }, plural(s.warnings, "warning")),
        h("span", { class: `chip ${s.errors ? "rejected" : ""}` }, plural(s.errors, "error")),
        s.unchanged ? h("span.chip.unchanged-count", `${fmtCount(s.unchanged)} unchanged, not added`) : null,
        h("span.chip", `${plural(s.changes, "change")} to add`)),
      s.errors
        ? h("div.notice.error", h("p", h("strong", `${plural(s.errors, "row")} ${s.errors === 1 ? "has" : "have"} errors.`), " Nothing can be added until they are fixed. Fix the file, then choose it again above."))
        : !s.changes
          ? h("div.notice", h("p", h("strong", "Nothing to add."), ` Every row already says what its ${view.kind === "polylines" ? "route" : "stop"} has, counting what is in your draft.`))
          : h("div.notice.ok", h("p", h("strong", "The file can be added."), s.warnings ? ` Please look at the ${plural(s.warnings, "warning")} first.` : "")),
      view.kind === "stops" || spec.now ? h("div.import-map.inset", { role: "img", "aria-label": "Map of the stops in the file, coloured by result" }) : null,
      resultTable(rows, view.resultFor.read),
      h("div.actionbar",
        stale ? h("p.notice.warning", "You switched drafts after the check. Check the file again against the draft you are using now.") : null,
        h("div.btn-row",
          h("button.btn", { type: "button", disabled: !!s.errors || stale || !s.changes, on: { click: addToDraft } },
            `Add ${plural(s.changes, "change")} to draft${state.draft ? ` “${state.draft.title}”` : ""}`),
          h("button.btn.secondary", { type: "button", on: { click: preview } }, "Check again")),
        s.errors ? h("p.why-not", "Adding is off because some rows have errors.") : null),
    ];
  }

  function resultTable(rows, read) {
    const spec = KINDS[view.kind];
    const counts = { problems: rows.filter((r) => r.status !== "ok").length, error: rows.filter((r) => r.status === "error").length, warning: rows.filter((r) => r.status === "warning").length, ok: rows.filter((r) => r.status === "ok").length, all: rows.length };
    if (!counts.problems && view.filter === "problems") view.filter = "all";
    const filtered = rows.filter((r) => view.filter === "all" || (view.filter === "problems" ? r.status !== "ok" : r.status === view.filter));
    const pages = Math.max(1, Math.ceil(filtered.length / TABLE_PAGE));
    view.page = Math.min(view.page, pages);
    const slice = filtered.slice((view.page - 1) * TABLE_PAGE, view.page * TABLE_PAGE);
    const refresh = () => document.getElementById("import-results")?.replaceWith(resultTable(rows, read));
    const setFilter = (f) => { view.filter = f; view.page = 1; refresh(); document.querySelector(`#import-results [data-filter="${f}"]`)?.focus(); };
    const filters = h("div.tabs", { role: "group", "aria-label": "Show rows" },
      [["problems", "With problems"], ["error", "Errors"], ["warning", "Warnings"], ["ok", "OK"], ["all", "All rows"]].map(([key, label]) =>
        h("button", { type: "button", "data-filter": key, "aria-pressed": String(view.filter === key), on: { click: () => setFilter(key) } }, label, h("span.count", fmtCount(counts[key])))));
    const cell = (row, name) => {
      const v = row[name];
      if (v === undefined || v === null || v === "") return h("td.empty-cell", "");
      if (name === "stop_type") return h("td", STOP_TYPE_LABEL[v] || v);
      if (name === "description") return h("td.long-cell", String(v));
      return h("td", String(v));
    };
    const statusLabel = { ok: "OK", warning: "Warning", error: "Error" };
    const statusClass = { ok: "committed", warning: "submitted", error: "rejected" };
    // the line itself is thousands of characters, so the table shows what it is
    // instead: how long it runs, and whether it goes over one the route has
    const lineCell = (line) => (line
      ? h("td.route-line", fmtMetres(line.length_m),
          h("span.sub", `${fmtCount(line.points)} points`),
          line.had_polyline ? h("span.sub.replacing", `replaces the ${line.polyline_source || "saved"} line`) : null)
      : h("td.empty-cell", ""));
    return h("div.result-block", { id: "import-results" },
      filters,
      filtered.length ? h("div.table-wrap", h("table.result-table",
        h("thead", h("tr", h("th", { scope: "col" }, "Row"), h("th", { scope: "col" }, "Result"), spec.now ? h("th", { scope: "col" }, "Stop now") : null, spec.line ? h("th", { scope: "col" }, "The line") : null, spec.show.map((c) => h("th", { scope: "col" }, c)), h("th", { scope: "col" }, "What to fix"))),
        h("tbody", slice.map((r) => h("tr", { id: `import-row-${r.i}`, class: `result-${r.status}` },
          h("td.num", String(read.sheetRows[r.i] ?? r.i + 2)),
          h("td", h("span", { class: `chip ${statusClass[r.status]}` }, statusLabel[r.status])),
          spec.now ? (r.stop ? h("td.stop-now", { title: r.stop.description || null }, r.stop.name, r.stop.platform_code ? h("span.sub", r.stop.platform_code) : null) : h("td.empty-cell", "")) : null,
          spec.line ? lineCell(r.polyline) : null,
          spec.show.map((c) => cell(read.rows[r.i] || {}, c)),
          h("td.messages", r.messages.length ? h("ul", r.messages.map((m) => h("li", { class: m.level === "warning" ? "warn" : "err" }, m.message))) : ""),
        ))))) : h("p.empty", "No rows to show here."),
      pages > 1 ? pager(view.page, pages, (p) => { view.page = p; refresh(); document.getElementById("import-results")?.scrollIntoView({ block: "start" }); }) : null,
      h("p.hint", "Row is the row number in your spreadsheet; the header is row 1."));
  }

  function stopsMap(el) {
    const read = view.resultFor.read;
    const byRow = new Map(view.result.rows.map((r) => [r.row - 1, r.status]));
    const colors = { ok: "#0b6660", warning: "#c98a00", error: "#b42318" };
    // a new stop is where its row says; an existing one where the server found it
    const found = new Map(view.result.rows.map((r) => [r.row - 1, r.stop || null]));
    const points = read.rows.map((row, i) => (KINDS[view.kind].now ? { ...row, ...(found.get(i) || {}) } : row)).map((row, i) => (Number.isFinite(row.lat) && Number.isFinite(row.lon) && Math.abs(row.lat) <= 90 && Math.abs(row.lon) <= 180
      ? {
          lat: row.lat, lon: row.lon, radius: 5, weight: 1, ring: "#ffffff", color: colors[byRow.get(i)] || colors.ok,
          title: `Row ${read.sheetRows[i]}: ${row.name || ""}`,
          onClick: () => {
            view.filter = "all";
            view.page = Math.floor(i / TABLE_PAGE) + 1;
            const rows = view.result.rows.map((r) => ({ i: r.row - 1, status: r.status, stop: r.stop || null, messages: (r.messages || []).map((m) => ({ ...m, level: m.level || (r.status === "warning" ? "warning" : "error") })) }));
            document.getElementById("import-results")?.replaceWith(resultTable(rows, read));
            requestAnimationFrame(() => {
              const tr = document.getElementById(`import-row-${i}`);
              if (tr) { tr.scrollIntoView({ block: "center" }); tr.classList.add("flash"); }
            });
          },
        }
      : null)).filter(Boolean);
    return map.inset(el, { points, maxZoom: 17 });
  }

  render();
}

export function pager(current, pages, go) {
  const select = h("select", { "aria-label": "Page", on: { change: (ev) => go(Number(ev.target.value)) } },
    Array.from({ length: pages }, (_, i) => h("option", { value: String(i + 1), selected: i + 1 === current }, `Page ${i + 1} of ${pages}`)));
  return h("nav.pager", { "aria-label": "Pages" },
    h("button.btn.secondary.small", { type: "button", disabled: current <= 1, on: { click: () => go(current - 1) } }, "‹ Previous"),
    select,
    h("button.btn.secondary.small", { type: "button", disabled: current >= pages, on: { click: () => go(current + 1) } }, "Next ›"));
}
