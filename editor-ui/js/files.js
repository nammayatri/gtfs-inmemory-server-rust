// Every file of the GTFS reference (docs section 18): the feed's files with
// their row counts, one file's rows (paged, searched, with or without the
// draft), and one row - its fields as a form, what points at it - edited into
// the draft like any other change. Stops, routes, trips and calendars keep
// their own screens; the files page links to them.
import { get, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtCount, plural, errorText } from "./util.js";
import { addChange } from "./drafts.js";
import { nameHere } from "./trail.js";
import { loadSpec, fileSpec, fieldsForm, keyOf, rowKey, cellText, changed, PRESENCE_LABEL } from "./gtfs.js";

const page = () => document.getElementById("page");
const PAGE_SIZE = 50;
// the columns a file's table shows before "and N more fields"
const TABLE_FIELDS = 8;

// where the files the editor keeps in its own tables are edited
const OWN_SCREEN = {
  "stops.txt": ["#/", "On the map: open a stop, then Edit"],
  "routes.txt": ["#/", "Search a route, then Edit details"],
  "trips.txt": ["#/", "Search a route, then Trips and timing"],
  "stop_times.txt": ["#/", "Stop orders on the route; timings on its Trips and timing page"],
  "frequencies.txt": ["#/", "Headway windows, on a route's Trips and timing page"],
  "calendar.txt": ["#/calendar", "On the Calendar page"],
  "calendar_dates.txt": ["#/calendar", "On the Calendar page"],
};

// ------------------------------------------------------------------ every file
export async function showFiles() {
  const root = h("div.page-inner.files", h("p.empty", "Loading…"));
  clear(page(), root);
  try {
    const [spec, counts] = await Promise.all([loadSpec(), get(`feeds/${enc(state.feedId)}/files`)]);
    const rows = new Map(counts.items.map((f) => [f.file, f.rows]));
    const groups = new Map();
    spec.files.forEach((f) => {
      if (!groups.has(f.group)) groups.set(f.group, []);
      groups.get(f.group).push(f);
    });
    const missing = spec.files.filter((f) => f.presence === "required" && !rows.get(f.file));
    clear(root,
      h("div.title-block",
        h("h1", "GTFS files"),
        h("p.hint", "Every file of the GTFS reference and how many rows this feed has in each. Open a file to see its rows and change them in your draft; stops, routes, trips and calendars have their own screens.")),
      missing.length ? h("p.notice.warning", `This feed has no rows in ${missing.map((f) => f.file).join(", ")}, which every feed needs. The feed report says more.`) : null,
      h("div.btn-row",
        h("a.btn.secondary.small", { href: "#/feed" }, "Feed report, download and import")),
      [...groups.entries()].map(([group, files]) => h("section.files-group",
        h("h2", group),
        h("div.table-wrap", h("table",
          h("thead", h("tr", h("th", "File"), h("th", "What it holds"), h("th", "In the reference"), h("th.num", "Rows"), h("th", ""))),
          h("tbody", files.map((f) => fileRow(f, rows.get(f.file) || 0))))))));
    nameHere("GTFS files");
  } catch (e) {
    clear(root, h("h1", "GTFS files"), h("p.notice.error", errorText(e)));
  }
}

function fileRow(f, n) {
  const own = OWN_SCREEN[f.file];
  const presence = h("span", { title: f.presence_note || "" }, PRESENCE_LABEL[f.presence] || f.presence);
  return h("tr", { dataset: { file: f.file } },
    h("td", f.storage === "record" ? h("a", { href: `#/files/${enc(f.file)}` }, h("code", f.file)) : h("code", f.file)),
    h("td", f.label),
    h("td", presence, f.presence_note ? h("div.hint", f.presence_note) : null),
    h("td.num", fmtCount(n)),
    h("td", f.storage === "record"
      ? h("a.btn.quiet.small", { href: `#/files/${enc(f.file)}` }, "Open")
      : own ? h("a.hint", { href: own[0] }, own[1]) : null));
}

// ------------------------------------------------------------------ one file
const view = { file: null, q: "", cursors: [null], at: 0, draft: false };

export async function showFile(file) {
  if (view.file !== file) Object.assign(view, { file, q: "", cursors: [null], at: 0, draft: false });
  const root = h("div.page-inner.file", h("p.empty", "Loading…"));
  clear(page(), root);
  let fspec;
  try {
    fspec = await fileSpec(file);
  } catch (e) {
    return clear(root, h("p.notice.error", errorText(e)));
  }
  if (!fspec) return clear(root, h("a", { href: "#/files" }, "All files"), h("h1", file), h("p.notice.error", `${file} is not a file of the GTFS reference.`));
  if (fspec.storage !== "record") {
    const own = OWN_SCREEN[file];
    return clear(root, h("a", { href: "#/files" }, "All files"), h("h1", fspec.label),
      h("p.notice", `${file} is kept in the editor's own tables. `, own ? h("a", { href: own[0] }, own[1]) : null, "."));
  }
  nameHere(file);
  const key = keyOf(fspec);
  const tableBox = h("div", h("p.empty", "Loading…"));
  const search = h("input", { type: "search", id: "file-search", placeholder: "Search any value", value: view.q });
  const draftToggle = state.draft ? h("label.check", h("input", { type: "checkbox", id: "file-draft", checked: view.draft,
    on: { change: (ev) => { view.draft = ev.target.checked; view.cursors = [null]; view.at = 0; load(); } } }), " With my draft applied") : null;
  search.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") { ev.preventDefault(); view.q = search.value.trim(); view.cursors = [null]; view.at = 0; load(); }
  });
  const shown = fspec.fields.slice(0, TABLE_FIELDS);
  const hidden = fspec.fields.length - shown.length;

  async function load() {
    clear(tableBox, h("p.empty", "Loading…"));
    const cursor = view.cursors[view.at];
    const qs = new URLSearchParams({ limit: String(PAGE_SIZE) });
    if (view.q) qs.set("q", view.q);
    if (cursor) qs.set("cursor", cursor);
    const path = view.draft && state.draft
      ? `change-sets/${enc(state.draft.change_set_id)}/preview/files/${enc(file)}?${qs}`
      : `feeds/${enc(state.feedId)}/files/${enc(file)}?${qs}`;
    try {
      const res = await get(path);
      view.cursors[view.at + 1] = res.next_cursor || null;
      const pending = new Set((state.draft ? state.draft.changes : []).filter((c) => c.entity === fspec.entity).map((c) => c.entity_key));
      clear(tableBox,
        res.items.length ? h("div.table-wrap", h("table.file-table",
          h("thead", h("tr",
            key.minted ? h("th", "row") : null,
            shown.map((f) => h("th", h("code", f.name))),
            hidden > 0 ? h("th.hint", `+${hidden} fields`) : null,
            h("th", ""))),
          h("tbody", res.items.map((row) => {
            const k = rowKey(fspec, row);
            return h("tr", { dataset: { key: k } },
              key.minted ? h("td", h("code", k)) : null,
              shown.map((f) => h("td", f.name === "points" ? plural((row.points || []).length, "point") : cellText(row[f.name]))),
              hidden > 0 ? h("td") : null,
              h("td", h("a.btn.quiet.small", { href: `#/files/${enc(file)}/${enc(k)}` }, pending.has(k) ? "Open (in your draft)" : "Open")));
          })))) : h("p.empty", view.q ? "No row matches." : "This feed has no rows in this file."),
        h("div.btn-row.pager",
          h("button.btn.secondary.small", { type: "button", disabled: view.at === 0, on: { click: () => { view.at -= 1; load(); } } }, "Previous"),
          h("span.hint", `Page ${view.at + 1}`),
          h("button.btn.secondary.small", { type: "button", disabled: !res.next_cursor, on: { click: () => { view.at += 1; load(); } } }, "Next")));
    } catch (e) {
      clear(tableBox, h("p.notice.error", errorText(e)));
    }
  }

  clear(root,
    h("a", { href: "#/files" }, "All files"),
    h("div.page-head",
      h("div.title-block", h("h1", fspec.label), h("p.hint", h("code", file), ` · ${PRESENCE_LABEL[fspec.presence] || fspec.presence}${fspec.presence_note ? ` (${fspec.presence_note})` : ""}`)),
      h("div.btn-row",
        can("editor") && !(key.feed) ? h("a.btn.small", { href: `#/files/${enc(file)}/new` }, "Add a row") : null,
        can("editor") && key.feed ? h("a.btn.small", { href: `#/files/${enc(file)}/${enc(state.feedId)}` }, "Edit") : null,
        can("editor") ? h("a.btn.secondary.small", { href: `#/import?kind=records&file=${enc(file)}` }, "Upload a CSV of rows") : null)),
    h("div.toolbar", h("label.field.inline-field", { for: "file-search" }, h("span", "Search"), search), draftToggle),
    tableBox);
  load();
}

// ------------------------------------------------------------------ one row
export async function showRecord(file, key) {
  const root = h("div.page-inner.record", h("p.empty", "Loading…"));
  clear(page(), root);
  const creating = key === "new";
  let fspec, row = null;
  try {
    fspec = await fileSpec(file);
    if (!fspec || fspec.storage !== "record") throw new Error(`${file} has no rows to edit here.`);
    if (!creating) row = await get(`feeds/${enc(state.feedId)}/files/${enc(file)}/${enc(key)}`).catch((e) => {
      // feed_info before it has its row, or a row this draft creates
      if (e.status === 404) return null;
      throw e;
    });
  } catch (e) {
    return clear(root, h("a", { href: `#/files/${enc(file)}` }, "Back to the file"), h("p.notice.error", errorText(e)));
  }
  const k = keyOf(fspec);
  const entity = fspec.entity;
  // a change the draft already carries for this row is what the form starts from
  const inDraft = !creating && state.draft ? state.draft.changes.find((c) => c.entity === entity && c.entity_key === key && c.op !== "delete") : null;
  const start = { ...(row || {}), ...(inDraft && inDraft.after ? inDraft.after : {}) };
  const isNew = creating || (!row && k.feed);
  nameHere(creating ? `New ${fspec.stem} row` : `${fspec.stem} ${key}`);
  const isShape = entity === "shape";
  const names = fspec.fields.map((f) => f.name).filter((n) => !(isShape && n !== "shape_id"));
  const readOnly = !isNew && k.field && !k.minted ? [k.field] : [];
  const form = fieldsForm(fspec, names, start, "rec", { readOnly });
  const points = isShape ? h("textarea", { id: "rec-points", rows: "10", spellcheck: "false" }) : null;
  if (points) points.value = (start.points || []).map((p) => [p.lat, p.lon, p.dist ?? ""].join(",").replace(/,$/, "")).join("\n");
  const problems = h("div");
  const editable = can("editor");

  const save = async (ev) => {
    ev.preventDefault();
    const { values, errors } = form.read();
    let pts = null;
    if (isShape) {
      pts = points.value.split("\n").map((l) => l.trim()).filter(Boolean).map((l, i) => {
        const [lat, lon, dist] = l.split(",").map((x) => x.trim());
        const p = { sequence: i + 1, lat: Number(lat), lon: Number(lon) };
        if (dist) p.dist = Number(dist);
        if (!Number.isFinite(p.lat) || !Number.isFinite(p.lon)) errors.push(`point ${i + 1} is not lat,lon`);
        return p;
      });
      if (pts.length < 2) errors.push("A shape has at least two points, one per line: lat,lon[,distance]");
    }
    if (errors.length) return clear(problems, h("div.notice.error", h("ul", errors.map((e) => h("li", e)))));
    let after, op, entityKey;
    if (isNew) {
      op = "create";
      after = Object.fromEntries(Object.entries(values).filter(([, v]) => v !== null));
      if (pts) after.points = pts;
      entityKey = k.feed ? state.feedId : k.minted ? "" : String(values[k.field] ?? "");
    } else {
      op = "update";
      after = changed(values, row);
      if (pts) after.points = pts;
      entityKey = key;
      if (!Object.keys(after).length) return clear(problems, h("p.notice", "Nothing has changed yet."));
    }
    try {
      const res = await addChange({ entity, op, entity_key: entityKey, after, base_row_version: row ? row.row_version : undefined });
      if (!res) return;
      if (res.problems.length) clear(problems, h("div.notice", { class: res.problems.some((p) => p.level === "error") ? "error" : "warning" }, h("ul", res.problems.map((p) => h("li", p.message)))));
      else clear(problems);
      if (!res.problems.some((p) => p.level === "error")) location.hash = `#/files/${enc(file)}`;
    } catch (e) {
      clear(problems, h("p.notice.error", errorText(e)));
    }
  };
  const remove = async () => {
    const users = (row && row.used_by) || [];
    const note = users.length ? ` It is named by ${users.map((u) => `${plural(u.rows, "row")} of ${u.file}`).join(", ")}; the draft will refuse the delete until those change.` : "";
    if (!(await confirmDialog(`Delete this ${fspec.stem} row in your draft?`, `The row goes when the draft is committed.${note}`, { confirm: "Delete in draft", danger: true }))) return;
    try {
      const res = await addChange({ entity, op: "delete", entity_key: key, after: null, base_row_version: row.row_version }, { merge: false });
      if (res && !res.problems.some((p) => p.level === "error")) location.hash = `#/files/${enc(file)}`;
      else if (res) clear(problems, h("div.notice.error", h("ul", res.problems.map((p) => h("li", p.message)))));
    } catch (e) {
      toast(errorText(e), "error");
    }
  };

  clear(root,
    h("a", { href: `#/files/${enc(file)}` }, `Back to ${file}`),
    h("div.page-head", h("div.title-block",
      h("h1", isNew ? `New row of ${file}` : `${fspec.stem} ${k.minted ? key : rowKey(fspec, start) || key}`),
      h("p.hint", isNew
        ? (k.minted ? "Its row id is made when the change is added." : k.feed ? "The feed's one row." : `The ${k.field} names it; it cannot change once made.`)
        : `Row version ${row ? row.row_version : "(new in your draft)"}${inDraft ? ", changed in your draft" : ""}.`))),
    row && row.used_by && row.used_by.length ? h("div.notice", h("p", h("strong", "Named by")),
      h("ul", row.used_by.map((u) => h("li", `${plural(u.rows, "row")} of `, h("a", { href: `#/files/${enc(u.file)}` }, u.file), ` (${u.field})`)))) : null,
    h("form.record-form", { novalidate: true, on: { submit: save } },
      form.el,
      points ? h("label.field", { for: "rec-points" }, h("span", "Points, one per line: lat,lon[,distance]"), points) : null,
      problems,
      editable ? h("div.sticky-actions", h("div.btn-row",
        h("button.btn", { type: "submit" }, isNew ? "Add to draft" : inDraft ? "Update in draft" : "Add change to draft"),
        !isNew && row ? h("button.btn.danger.secondary", { type: "button", on: { click: remove } }, "Delete in draft") : null,
        h("a.btn.secondary", { href: `#/files/${enc(file)}` }, "Cancel")))
        : h("p.notice", "Changing rows needs the editor role.")));
  if (!editable) root.querySelectorAll("input, select, textarea").forEach((el) => { el.disabled = true; });
}
