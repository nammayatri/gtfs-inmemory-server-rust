// The GTFS reference as the server describes it (GET gtfs-spec, docs section
// 18), and the form fields built from it: one typed input per field, an enum as
// a choice of its values, a reference offered from the rows it may name. The
// files page, the stop and route editors and the importer all build from here.
import { get, enc } from "./api.js";
import { state } from "./state.js";
import { h } from "./util.js";

let specPromise = null;

// The whole registry, fetched once per page load.
export function loadSpec() {
  if (!specPromise) {
    specPromise = get("gtfs-spec").catch((e) => {
      specPromise = null;
      throw e;
    });
  }
  return specPromise;
}

export async function fileSpec(name) {
  const spec = await loadSpec();
  return spec.files.find((f) => f.file === name) || null;
}

export const PRESENCE_LABEL = { required: "Required", optional: "Optional", conditional: "Conditionally required" };

// A file's own words for a field: its GTFS name, spaced.
export const fieldLabel = (fs) => fs.name.replace(/_/g, " ");

// The key a record file's row is named by, and whether the server makes it.
export function keyOf(fspec) {
  const k = fspec && fspec.key;
  if (!k) return { field: null, minted: false, feed: false };
  if (k.kind === "field") return { field: k.field, minted: false, feed: false };
  if (k.kind === "minted") return { field: "row_id", minted: true, feed: false, natural: k.natural };
  return { field: null, minted: false, feed: true };
}

// The row's key as the change names it.
export function rowKey(fspec, row) {
  const k = keyOf(fspec);
  if (k.feed) return state.feedId;
  return row ? String(row[k.field] ?? "") : "";
}

// A value as a cell shows it.
export function cellText(v) {
  if (v === null || v === undefined) return "";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

const INPUT_TYPE = { url: "url", email: "email", phone: "tel", date: "date", integer: "number", float: "number", latitude: "number", longitude: "number" };
const PLACEHOLDER = {
  time: "H:MM:SS, for example 6:30:00 or 25:10:00",
  color: "#RRGGBB",
  language: "for example en or ta",
  timezone: "for example Asia/Kolkata",
  currency_code: "for example INR",
  currency_amount: "for example 10.50",
  json: "JSON",
};

// Rows a reference may name, for a datalist: the first 200 of each record
// file it points at (stops, routes, trips and services are found by typing).
const refCache = new Map();
async function refValues(refs) {
  const out = new Set();
  for (const r of refs) {
    const key = `${state.feedId}|${r.file}|${r.field}`;
    if (!refCache.has(key)) {
      refCache.set(key, (async () => {
        const spec = await fileSpec(r.file);
        if (!spec || spec.storage !== "record") return [];
        try {
          const page = await get(`feeds/${enc(state.feedId)}/files/${enc(r.file)}?limit=200`);
          return [...new Set(page.items.map((row) => row[r.field]).filter((v) => v !== null && v !== undefined && v !== "").map(String))];
        } catch {
          return [];
        }
      })());
    }
    (await refCache.get(key)).forEach((v) => out.add(v));
  }
  return [...out].sort();
}

// One field of a form: {el, input, read()} where read() is {value} (null when
// blank) or {error}. `id` is unique on the page.
export function fieldControl(fs, value, id, { readOnly = false } = {}) {
  let input;
  if (fs.type === "enum") {
    const values = fs.values || [];
    const known = values.some((v) => String(v.value) === cellText(value));
    input = h("select", { id, disabled: readOnly },
      h("option", { value: "" }, "(not set)"),
      values.map((v) => h("option", { value: String(v.value), selected: String(v.value) === cellText(value) }, `${v.value} · ${v.label}`)),
      fs.extended_route_types ? h("option", { value: "__other", selected: !known && cellText(value) !== "" }, "Another extended route type (100-1702)") : null);
    if (fs.extended_route_types) {
      const other = h("input", { type: "number", id: `${id}-other`, min: "100", max: "1702", step: "1", hidden: known || cellText(value) === "", value: known ? "" : cellText(value), "aria-label": "Extended route type" });
      input.addEventListener("change", () => { other.hidden = input.value !== "__other"; });
      const wrap = h("span.enum-other", input, other);
      return finish(fs, wrap, id, () => {
        const v = input.value === "__other" ? other.value.trim() : input.value;
        if (v === "") return { value: null };
        const n = Number(v);
        return Number.isInteger(n) ? { value: n } : { error: `${fs.name} must be a route type` };
      });
    }
    return finish(fs, input, id, () => (input.value === "" ? { value: null } : { value: Number(input.value) }));
  }
  if (fs.type === "json" || (fs.type === "text" && String(value ?? "").length > 80)) {
    input = h("textarea", { id, rows: fs.type === "json" ? "4" : "2", readOnly, placeholder: PLACEHOLDER[fs.type] || "" });
    input.value = fs.type === "json" && value !== null && value !== undefined ? JSON.stringify(value, null, 2) : cellText(value);
  } else {
    input = h("input", {
      id,
      type: INPUT_TYPE[fs.type] || "text",
      step: fs.type === "integer" ? "1" : ["float", "latitude", "longitude"].includes(fs.type) ? "any" : undefined,
      min: fs.min !== undefined && fs.min !== null ? String(fs.min) : undefined,
      max: fs.max !== undefined && fs.max !== null ? String(fs.max) : undefined,
      placeholder: PLACEHOLDER[fs.type] || "",
      readOnly,
      spellcheck: "false",
      autocomplete: "off",
    });
    input.value = cellText(value);
  }
  let list = null;
  if (fs.refs && fs.refs.length && input.tagName === "INPUT" && !readOnly) {
    list = h("datalist", { id: `${id}-list` });
    input.setAttribute("list", list.id);
    refValues(fs.refs).then((vals) => list.replaceChildren(...vals.map((v) => h("option", { value: v }))));
  }
  return finish(fs, [input, list], id, () => {
    const raw = input.value.trim();
    if (raw === "") return { value: null };
    switch (fs.type) {
      case "integer": {
        const n = Number(raw);
        if (!Number.isInteger(n)) return { error: `${fs.name} must be a whole number` };
        if (fs.min !== null && fs.min !== undefined && n < fs.min) return { error: `${fs.name} must be ${fs.min} or more` };
        if (fs.max !== null && fs.max !== undefined && n > fs.max) return { error: `${fs.name} must be ${fs.max} or less` };
        return { value: n };
      }
      case "float": case "latitude": case "longitude": {
        const n = Number(raw);
        return Number.isFinite(n) ? { value: n } : { error: `${fs.name} must be a number` };
      }
      case "json":
        try {
          return { value: JSON.parse(raw) };
        } catch {
          return { error: `${fs.name} is not valid JSON` };
        }
      case "time":
        return /^\d{1,2}:\d{2}:\d{2}$/.test(raw) ? { value: raw } : { error: `${fs.name} must be a time, H:MM:SS` };
      case "color":
        return /^#?[0-9A-Fa-f]{6}$/.test(raw) ? { value: raw.startsWith("#") ? raw.toUpperCase() : `#${raw.toUpperCase()}` } : { error: `${fs.name} must be a colour, #RRGGBB` };
      default:
        return { value: raw };
    }
  });
}

function finish(fs, control, id, read) {
  const note = fs.presence === "conditional" && fs.note ? h("span.hint", fs.note) : null;
  const el = h("label.field", { for: id },
    h("span", fieldLabel(fs), fs.presence === "required" ? h("span.req", { title: "Required" }, " *") : null),
    control, note);
  const input = el.querySelector("input, select, textarea");
  return { el, input, read };
}

// A form over `fields` of a file: {el, read()} where read() is {values,
// errors}. `values` holds every field (null for blank); the caller decides
// which to send.
export function fieldsForm(fspec, fieldNames, values, idPrefix, { readOnly = [] } = {}) {
  const controls = fieldNames
    .map((name) => fspec.fields.find((f) => f.name === name))
    .filter(Boolean)
    .map((fs) => [fs, fieldControl(fs, values ? values[fs.name] : null, `${idPrefix}-${fs.name}`, { readOnly: readOnly.includes(fs.name) })]);
  const el = h("div.gtfs-fields", controls.map(([, c]) => c.el));
  const read = () => {
    const out = {}, errors = [];
    for (const [fs, c] of controls) {
      const r = c.read();
      if (r.error) errors.push(r.error);
      else out[fs.name] = r.value;
    }
    return { values: out, errors };
  };
  return { el, read, controls };
}

// The fields `values` sets differently from `before` (null clears a value).
export function changed(values, before) {
  const out = {};
  for (const [k, v] of Object.entries(values)) {
    const was = before ? before[k] ?? null : null;
    if (cellText(v) !== cellText(was)) out[k] = v;
  }
  return out;
}
