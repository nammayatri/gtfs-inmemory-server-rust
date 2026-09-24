// The feed's calendar (docs section 16.9): every service with its days, its
// date range, the dates added or taken away, and how many trips run on it.
// A service is added, changed or deleted in the draft; one with trips cannot be
// deleted until they move.
import { get, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, modal, confirmDialog, fmtCount, errorText } from "./util.js";
import { addChange } from "./drafts.js";
import { nameHere } from "./trail.js";
import { daysText } from "./trips.js";

const page = () => document.getElementById("page");
const DAYS = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
const EXCEPTION = { 1: "runs", 2: "does not run" };

export async function showCalendar() {
  const root = h("div.page-inner.calendar", h("p.empty", "Loading…"));
  clear(page(), root);
  nameHere("Calendar");
  let services;
  try {
    const res = await get(`feeds/${enc(state.feedId)}/services`);
    services = Array.isArray(res) ? res : res.items || [];
  } catch (e) {
    return clear(root, h("h1", "Calendar"), h("p.notice.error", errorText(e)));
  }
  const pending = new Map((state.draft ? state.draft.changes : []).filter((c) => c.entity === "service").map((c) => [c.entity_key, c]));
  const editable = can("editor");
  clear(root,
    h("div.page-head",
      h("div.title-block", h("h1", "Calendar"),
        h("p.hint", "When trips run. A trip runs on every day its service runs: the days of the week between its first and last date, plus the dates added, minus the dates taken away.")),
      editable ? h("button.btn.small", { type: "button", on: { click: () => editService(null) } }, "Add a service") : null),
    services.length ? h("div.table-wrap", h("table.calendar-table",
      h("thead", h("tr", h("th", "Service"), h("th", "Days"), h("th", "From"), h("th", "To"), h("th", "Dates"), h("th.num", "Trips"), h("th", ""))),
      h("tbody", services.map((s) => {
        const change = pending.get(s.service_id);
        return h("tr", { dataset: { service: s.service_id } },
          h("td", h("strong", s.service_id), s.label ? h("div.hint", s.label) : null, change ? h("span.chip.draft", "in your draft") : null),
          h("td", h("code", daysText(s.days))),
          h("td", s.start_date || ""),
          h("td", s.end_date || ""),
          h("td", (s.dates || []).length ? (s.dates || []).map((d) => `${d.date} ${EXCEPTION[d.exception_type] || d.exception_type}`).join(", ") : ""),
          h("td.num", fmtCount(s.trip_count)),
          h("td", editable ? h("div.btn-row",
            h("button.btn.quiet.small", { type: "button", on: { click: () => editService(s) } }, "Edit"),
            h("button.btn.quiet.small", { type: "button", disabled: s.trip_count > 0, title: s.trip_count > 0 ? "Trips run on it: move them to another service first" : "",
              on: { click: () => deleteService(s) } }, "Delete")) : null));
      })))) : h("p.empty", "This feed has no services yet."));
}

async function editService(s) {
  const creating = !s;
  const after = await modal(creating ? "Add a service" : `Service ${s.service_id}`, (close) => {
    const id = h("input", { type: "text", id: "svc-id", value: s ? s.service_id : "", readOnly: !creating, placeholder: "for example WEEKDAY" });
    const label = h("input", { type: "text", id: "svc-label", value: (s && s.label) || "" });
    const days = DAYS.map((d) => h("input", { type: "checkbox", id: `svc-${d}`, checked: !!(s && s.days && s.days[d]) }));
    const start = h("input", { type: "date", id: "svc-start", value: (s && s.start_date) || "" });
    const end = h("input", { type: "date", id: "svc-end", value: (s && s.end_date) || "" });
    const dates = ((s && s.dates) || []).map((d) => ({ ...d }));
    const datesBox = h("div");
    const drawDates = () => clear(datesBox,
      h("h3", "Dates added or taken away"),
      dates.length ? h("ul.list", dates.map((d, k) => h("li.list-item",
        h("input", { type: "date", value: d.date, "aria-label": "Date", on: { change: (ev) => { d.date = ev.target.value; } } }),
        h("select", { "aria-label": "On that date", on: { change: (ev) => { d.exception_type = Number(ev.target.value); } } },
          h("option", { value: "1", selected: d.exception_type === 1 }, "runs (added)"),
          h("option", { value: "2", selected: d.exception_type === 2 }, "does not run (taken away)")),
        h("button.btn.quiet.small", { type: "button", on: { click: () => { dates.splice(k, 1); drawDates(); } } }, "Remove")))) : h("p.hint", "None."),
      h("button.btn.quiet.small", { type: "button", on: { click: () => { dates.push({ date: "", exception_type: 2 }); drawDates(); } } }, "Add a date"));
    drawDates();
    const err = h("p.notice.error", { hidden: true, role: "alert" });
    const ok = () => {
      const fail = (m) => { err.hidden = false; err.textContent = m; };
      if (creating && !id.value.trim()) return fail("Give the service an id.");
      if (!!start.value !== !!end.value) return fail("Give both the first and the last date, or neither.");
      if (start.value && end.value < start.value) return fail("The last date is on or after the first.");
      if (dates.some((d) => !d.date)) return fail("Every added or taken-away date needs its date.");
      const out = {
        days: Object.fromEntries(DAYS.map((d, i) => [d, days[i].checked])),
        start_date: start.value || null,
        end_date: end.value || null,
        label: label.value.trim() || null,
        dates: dates.map((d) => ({ date: d.date, exception_type: d.exception_type })),
      };
      if (creating) out.service_id = id.value.trim();
      close(out);
    };
    return h("div", { style: "display:grid;gap:10px" },
      h("div.field-row",
        h("label.field", { for: "svc-id" }, h("span", "Service id"), id),
        h("label.field", { for: "svc-label" }, h("span", "Label (optional)"), label)),
      h("fieldset.days", h("legend", "Runs on"), DAYS.map((d, i) => h("label.check", { for: `svc-${d}` }, days[i], ` ${d[0].toUpperCase()}${d.slice(1, 3)}`))),
      h("div.field-row",
        h("label.field", { for: "svc-start" }, h("span", "From"), start),
        h("label.field", { for: "svc-end" }, h("span", "To"), end)),
      datesBox, err,
      h("div.btn-row", h("button.btn", { type: "button", on: { click: ok } }, "Add to draft"),
        h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
  }, { wide: true });
  if (!after) return;
  try {
    const res = await addChange(creating
      ? { entity: "service", op: "create", entity_key: after.service_id, after }
      : { entity: "service", op: "update", entity_key: s.service_id, after, base_row_version: s.row_version });
    if (res && !res.problems.some((p) => p.level === "error")) showCalendar();
  } catch (e) {
    await confirmDialog("The service was not added", errorText(e), { confirm: "OK" });
  }
}

async function deleteService(s) {
  if (!(await confirmDialog(`Delete service ${s.service_id} in your draft?`, "It goes when the draft is committed. A timeframe or booking rule that names it has to change first.", { confirm: "Delete in draft", danger: true }))) return;
  try {
    const res = await addChange({ entity: "service", op: "delete", entity_key: s.service_id, after: null, base_row_version: s.row_version }, { merge: false });
    if (res) showCalendar();
  } catch (e) {
    await confirmDialog("The service was not deleted", errorText(e), { confirm: "OK" });
  }
}
