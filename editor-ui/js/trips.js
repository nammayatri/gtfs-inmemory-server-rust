// A route's timetable (docs section 16.9): its trips as a departure board per
// stop order and direction - add, remove and shift departures, add a run of
// them "every N minutes from … to …", edit a trip's headway windows - and the
// timing of a stop order beside its stops, where editing hop times makes or
// changes a timing profile. What is edited here collects on the page and goes
// into the draft as one route_trips/replace (or timing_profile/replace).
import { get, enc } from "./api.js";
import { state, can, setLeaveGuard } from "./state.js";
import { h, clear, toast, modal, confirmDialog, plural, fmtCount, errorText } from "./util.js";
import { addChange, existingChange } from "./drafts.js";
import { nameHere } from "./trail.js";

const page = () => document.getElementById("page");
const DAY_SHORT = ["M", "T", "W", "T", "F", "S", "S"];
const DAYS = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

// ------------------------------------------------------------------ times
export function toSeconds(text) {
  const m = String(text || "").trim().match(/^(\d{1,2}):(\d{2})(?::(\d{2}))?$/);
  if (!m) return null;
  const s = Number(m[1]) * 3600 + Number(m[2]) * 60 + Number(m[3] || 0);
  return Number(m[2]) < 60 && Number(m[3] || 0) < 60 && s < 48 * 3600 ? s : null;
}

export function fromSeconds(s) {
  const t = Math.max(0, Math.round(s));
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(Math.floor(t / 3600))}:${pad(Math.floor((t % 3600) / 60))}:${pad(t % 60)}`;
}

const hhmm = (text) => fromSeconds(toSeconds(text) ?? 0).slice(0, 5);
const minutes = (s) => (Math.round((s / 60) * 10) / 10).toString();

// The default timing's offsets for `n` stops (section 16.3).
export function defaultOffsets(n, run, dwell) {
  const arrival = [], departure = [];
  for (let i = 0; i < n; i++) {
    arrival.push(i * (run + dwell));
    departure.push(i === n - 1 ? arrival[i] : arrival[i] + dwell);
  }
  return { arrival, departure };
}

export const daysText = (days) => (days ? DAYS.map((d, i) => (days[d] ? DAY_SHORT[i] : "·")).join("") : "");

// ------------------------------------------------------------------ the page
export async function showTrips(routeId, { pattern } = {}) {
  const root = h("div.page-inner.trips", h("p.empty", "Loading…"));
  clear(page(), root);
  const g = enc(state.feedId), r = enc(routeId);
  const prior = existingChange("route_trips", routeId);
  let route, live, services, config;
  try {
    [route, live, services, config] = await Promise.all([
      get(`feeds/${g}/routes/${r}`),
      get(`feeds/${g}/routes/${r}/trips`),
      get(`feeds/${g}/services`),
      get(`feeds/${g}/config`).catch(() => ({})),
    ]);
  } catch (e) {
    return clear(root, h("a", { href: `#/route/${r}` }, "Back to the route"), h("p.notice.error", errorText(e)));
  }
  services = Array.isArray(services) ? services : services.items || [];
  const run = config.default_run_s ?? 120, dwell = config.default_dwell_s ?? 15;
  const name = route.short_name && route.short_name !== routeId ? `${route.short_name} (${routeId})` : routeId;
  nameHere(`Trips of ${route.short_name || routeId}`);

  // the list being edited: the draft's, when the draft already replaces it
  const base = prior ? prior.after.base_trips_hash : live.trips_hash;
  let trips = (prior ? prior.after.trips : live.items).map((t) => ({ ...t }));
  const startJson = JSON.stringify(trips);
  const patterns = route.patterns && route.patterns.length ? route.patterns : [{ pattern_key: 1, stop_count: route.stop_count, trip_count: live.trip_count }];
  let selected = Number(pattern) || patterns[0].pattern_key;
  const profiles = route.profiles || [];
  const editable = can("editor");
  const dirty = () => JSON.stringify(trips) !== startJson;
  setLeaveGuard(() => (dirty() ? `Your trip edits on route ${name} are not in the draft yet.` : null));

  const board = h("section.board");
  const timing = h("section.timing");
  const status = h("div", { "aria-live": "polite" });

  const profileLabel = (t) => {
    if (t.profile_key == null) return "default timing";
    const p = profiles.find((x) => x.pattern_key === t.pattern_key && x.profile_key === t.profile_key);
    return p && p.label ? p.label : `timing ${t.profile_key}`;
  };
  const serviceText = (id) => {
    const s = services.find((x) => x.service_id === id);
    return s ? `${id}${s.label ? ` (${s.label})` : ""} ${daysText(s.days)}` : id;
  };

  function renderBoard() {
    const mine = trips.map((t, i) => ({ t, i })).filter(({ t }) => t.pattern_key === selected);
    const groups = new Map();
    for (const x of mine) {
      const key = `${x.t.direction_id ?? "-"}|${x.t.service_id}`;
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(x);
    }
    const sorted = [...groups.entries()].sort(([a], [b]) => a.localeCompare(b));
    clear(board,
      h("h2", `Departures on stop order ${selected}`, h("span.hint", ` ${plural(mine.length, "trip")}`)),
      mine.length ? sorted.map(([key, list]) => {
        const [dir, service] = key.split("|");
        list.sort((a, b) => (toSeconds(a.t.start_time) ?? 0) - (toSeconds(b.t.start_time) ?? 0));
        return h("div.board-group",
          h("h3", `${dir === "-" ? "No direction" : `Direction ${dir}`} · ${serviceText(service)}`),
          h("div.table-wrap", h("table.board-table",
            h("thead", h("tr", h("th", "Starts"), h("th", "Timing"), h("th", "Headsign"), h("th", "Trip"), h("th", "Every"), h("th", ""))),
            h("tbody", list.map(({ t, i }) => h("tr", { dataset: { trip: t.trip_id || "" } },
              h("td", h("strong", hhmm(t.start_time))),
              h("td", profileLabel(t)),
              h("td", t.headsign || ""),
              h("td", h("code", t.trip_id || "new")),
              h("td", (t.frequencies || []).map((f) => `${hhmm(f.start_time)}-${hhmm(f.end_time)} every ${minutes(f.headway_s)} min`).join(", ")),
              h("td", editable ? h("div.btn-row",
                h("button.btn.quiet.small", { type: "button", title: "5 minutes earlier", on: { click: () => shift(i, -300) } }, "−5"),
                h("button.btn.quiet.small", { type: "button", title: "5 minutes later", on: { click: () => shift(i, 300) } }, "+5"),
                h("button.btn.quiet.small", { type: "button", on: { click: () => editTrip(i) } }, "Edit"),
                h("button.btn.quiet.small", { type: "button", on: { click: () => { trips.splice(i, 1); renderAll(); } } }, "Remove")) : null)))))));
      }) : h("p.empty", "No trips run this stop order."),
      editable ? addForm() : null);
  }

  function shift(i, by) {
    const s = toSeconds(trips[i].start_time);
    if (s === null) return;
    trips[i] = { ...trips[i], start_time: fromSeconds(Math.min(Math.max(s + by, 0), 48 * 3600 - 1)) };
    renderAll();
  }

  const serviceSelect = (id, value) => h("select", { id },
    services.map((s) => h("option", { value: s.service_id, selected: s.service_id === value }, serviceText(s.service_id))));
  const profileSelect = (id, value) => h("select", { id },
    h("option", { value: "", selected: value == null }, `Default timing (${minutes(run)} min a hop, ${dwell} s at a stop)`),
    profiles.filter((p) => p.pattern_key === selected).map((p) => h("option", { value: String(p.profile_key), selected: p.profile_key === value }, p.label || `Timing ${p.profile_key}`)));

  function addForm() {
    const start = h("input", { type: "text", id: "add-start", placeholder: "06:00", size: "6" });
    const until = h("input", { type: "text", id: "add-until", placeholder: "optional, 22:00", size: "6" });
    const every = h("input", { type: "number", id: "add-every", min: "1", step: "1", placeholder: "10", size: "4" });
    const service = serviceSelect("add-service", services[0] && services[0].service_id);
    const profile = profileSelect("add-profile", null);
    const direction = h("select", { id: "add-direction" }, h("option", { value: "" }, "none"), h("option", { value: "0" }, "0"), h("option", { value: "1" }, "1"));
    const headsign = h("input", { type: "text", id: "add-headsign", placeholder: "optional" });
    const err = h("p.notice.error", { hidden: true, role: "alert" });
    const add = (ev) => {
      ev.preventDefault();
      err.hidden = true;
      const s = toSeconds(start.value);
      if (s === null) { err.hidden = false; err.textContent = "Give the first departure as HH:MM."; return; }
      if (!services.length) { err.hidden = false; err.textContent = "The feed has no service yet: add one on the Calendar page."; return; }
      const end = until.value.trim() ? toSeconds(until.value) : s;
      const step = Number(every.value || 0) * 60;
      if (end === null || end < s) { err.hidden = false; err.textContent = "The last departure is HH:MM, after the first."; return; }
      if (end > s && !(step > 0)) { err.hidden = false; err.textContent = "Say every how many minutes, from the first departure to the last."; return; }
      const added = [];
      for (let t = s; t <= end; t += step || 1) {
        added.push({
          pattern_key: selected,
          profile_key: profile.value === "" ? null : Number(profile.value),
          service_id: service.value,
          direction_id: direction.value === "" ? null : Number(direction.value),
          start_time: fromSeconds(t),
          headsign: headsign.value.trim() || null,
        });
        if (!step) break;
        if (added.length > 500) break;
      }
      trips.push(...added);
      toast(`${plural(added.length, "departure")} added here. Save to put them in the draft.`);
      renderAll();
    };
    return h("form.add-trips", { on: { submit: add } },
      h("h3", "Add departures"),
      h("div.field-row",
        h("label.field", { for: "add-start" }, h("span", "First departure"), start),
        h("label.field", { for: "add-every" }, h("span", "Every (minutes)"), every),
        h("label.field", { for: "add-until" }, h("span", "Until"), until)),
      h("div.field-row",
        h("label.field", { for: "add-service" }, h("span", "Service"), service),
        h("label.field", { for: "add-profile" }, h("span", "Timing"), profile),
        h("label.field", { for: "add-direction" }, h("span", "Direction"), direction),
        h("label.field", { for: "add-headsign" }, h("span", "Headsign"), headsign)),
      err,
      h("div.btn-row", h("button.btn.secondary", { type: "submit" }, "Add")));
  }

  async function editTrip(i) {
    const t = trips[i];
    const next = await modal(`Trip ${t.trip_id || "(new)"}`, (close) => {
      const start = h("input", { type: "text", id: "trip-start", value: t.start_time });
      const service = serviceSelect("trip-service", t.service_id);
      const profile = profileSelect("trip-profile", t.profile_key);
      const headsign = h("input", { type: "text", id: "trip-headsign", value: t.headsign || "" });
      const windows = (t.frequencies || []).map((f) => ({ ...f }));
      const box = h("div");
      const drawWindows = () => clear(box,
        h("h3", "Runs every … (headway windows)"),
        windows.length ? h("ul.list", windows.map((f, k) => h("li.list-item",
          h("input", { type: "text", value: f.start_time, size: "8", "aria-label": "From", on: { change: (ev) => { f.start_time = ev.target.value.trim(); } } }),
          h("input", { type: "text", value: f.end_time, size: "8", "aria-label": "To", on: { change: (ev) => { f.end_time = ev.target.value.trim(); } } }),
          h("input", { type: "number", value: String(Math.round(f.headway_s / 60)), min: "1", size: "4", "aria-label": "Every (minutes)", on: { change: (ev) => { f.headway_s = Number(ev.target.value) * 60; } } }),
          h("span.hint", "minutes"),
          h("button.btn.quiet.small", { type: "button", on: { click: () => { windows.splice(k, 1); drawWindows(); } } }, "Remove")))) : h("p.hint", "The trip runs once, at its start time."),
        h("button.btn.quiet.small", { type: "button", on: { click: () => { windows.push({ start_time: t.start_time, end_time: fromSeconds((toSeconds(t.start_time) ?? 0) + 3600), headway_s: 600 }); drawWindows(); } } }, "Add a window"));
      drawWindows();
      const err = h("p.notice.error", { hidden: true });
      const ok = () => {
        if (toSeconds(start.value) === null) { err.hidden = false; err.textContent = "The start time is HH:MM or HH:MM:SS."; return; }
        close({
          ...t,
          start_time: fromSeconds(toSeconds(start.value)),
          service_id: service.value,
          profile_key: profile.value === "" ? null : Number(profile.value),
          headsign: headsign.value.trim() || null,
          frequencies: windows,
        });
      };
      return h("div", { style: "display:grid;gap:10px" },
        h("div.field-row",
          h("label.field", { for: "trip-start" }, h("span", "Starts"), start),
          h("label.field", { for: "trip-service" }, h("span", "Service"), service)),
        h("div.field-row",
          h("label.field", { for: "trip-profile" }, h("span", "Timing"), profile),
          h("label.field", { for: "trip-headsign" }, h("span", "Headsign"), headsign)),
        box, err,
        h("div.btn-row", h("button.btn", { type: "button", on: { click: ok } }, "Done"),
          h("button.btn.secondary", { type: "button", on: { click: () => close(undefined) } }, "Cancel")));
    }, { wide: true });
    if (next) {
      trips[i] = next;
      renderAll();
    }
  }

  async function save() {
    try {
      const res = await addChange({ entity: "route_trips", op: "replace", entity_key: routeId, after: { base_trips_hash: base, trips } });
      if (!res) return;
      const errors = res.problems.filter((p) => p.level === "error");
      clear(status, res.problems.length ? h("div.notice", { class: errors.length ? "error" : "warning" }, h("ul", res.problems.map((p) => h("li", p.message)))) : null);
      if (!errors.length) {
        setLeaveGuard(null);
        showTrips(routeId, { pattern: selected });
      }
    } catch (e) {
      clear(status, h("p.notice.error", errorText(e)));
    }
  }

  // ---------------------------------------------------------------- timing
  async function renderTiming() {
    clear(timing, h("p.empty", "Loading the stop order…"));
    let detail;
    try {
      detail = selected === (route.pattern_key ?? 1) ? route : await get(`feeds/${g}/routes/${r}/patterns/${selected}`);
    } catch (e) {
      return clear(timing, h("p.notice.error", errorText(e)));
    }
    const stops = (detail.rows || []).filter((x) => !["ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"].includes(x.stop_type));
    const mine = profiles.filter((p) => p.pattern_key === selected);
    let current = null;
    const pick = h("select", { id: "timing-profile", on: { change: () => { current = mine.find((p) => String(p.profile_key) === pick.value) || null; draw(); } } },
      h("option", { value: "" }, "Default timing"),
      mine.map((p) => h("option", { value: String(p.profile_key) }, `${p.label || `Timing ${p.profile_key}`}${p.source ? ` (${p.source})` : ""}`)));
    const table = h("div");
    const label = h("input", { type: "text", id: "timing-label", placeholder: "for example Peak" });
    const note = h("div", { "aria-live": "polite" });
    let hops = [], dwells = [];
    const draw = () => {
      const off = current ? { arrival: current.arrival_s, departure: current.departure_s } : defaultOffsets(stops.length, run, dwell);
      hops = off.arrival.map((a, i) => (i === 0 ? 0 : a - off.departure[i - 1]));
      dwells = off.arrival.map((a, i) => off.departure[i] - a);
      label.value = current && current.label ? current.label : "";
      const rows = stops.map((s, i) => h("tr",
        h("td", String(i + 1)),
        h("td", s.stop_name || s.stop_id),
        h("td", i === 0 ? "" : h("input.hop", { type: "number", min: "0", step: "0.5", value: minutes(hops[i]), size: "4", "aria-label": `Minutes to ${s.stop_name || s.stop_id}`, disabled: !editable,
          on: { change: (ev) => { hops[i] = Math.round(Number(ev.target.value) * 60); totals(); } } })),
        h("td", h("input.dwell", { type: "number", min: "0", step: "5", value: String(dwells[i]), size: "4", "aria-label": `Seconds at ${s.stop_name || s.stop_id}`, disabled: !editable || i === stops.length - 1,
          on: { change: (ev) => { dwells[i] = Math.round(Number(ev.target.value)); totals(); } } })),
        h("td.arrive", "")));
      clear(table, h("div.table-wrap", h("table.timing-table",
        h("thead", h("tr", h("th", "#"), h("th", "Stop"), h("th", "Minutes from the stop before"), h("th", "Seconds at the stop"), h("th", "Arrives after"))),
        h("tbody", rows))), h("p.hint.timing-total"));
      totals();
    };
    const offsets = () => {
      const arrival = [], departure = [];
      stops.forEach((_, i) => {
        const a = i === 0 ? 0 : departure[i - 1] + (hops[i] || 0);
        arrival.push(a);
        departure.push(i === stops.length - 1 ? a : a + (dwells[i] || 0));
      });
      return { arrival, departure };
    };
    const totals = () => {
      const off = offsets();
      table.querySelectorAll("td.arrive").forEach((td, i) => { td.textContent = `${minutes(off.arrival[i])} min`; });
      const total = table.querySelector(".timing-total");
      if (total) total.textContent = stops.length ? `The whole trip takes ${minutes(off.arrival[off.arrival.length - 1])} minutes.` : "";
    };
    const saveTiming = async (asNew) => {
      const off = offsets();
      const after = { pattern_key: selected, arrival_s: off.arrival, departure_s: off.departure, label: label.value.trim() || null };
      if (!asNew && current) {
        after.profile_key = current.profile_key;
        after.base_hash = current.hash;
      }
      try {
        const res = await addChange({ entity: "timing_profile", op: "replace", entity_key: routeId, after }, { merge: false });
        if (!res) return;
        clear(note, res.problems.length ? h("div.notice", { class: res.problems.some((p) => p.level === "error") ? "error" : "warning" }, h("ul", res.problems.map((p) => h("li", p.message)))) : h("p.notice.ok", "The timing is in your draft. Trips run to it once the draft is committed; choose it for a trip above."));
      } catch (e) {
        clear(note, h("p.notice.error", errorText(e)));
      }
    };
    clear(timing,
      h("h2", `Timing of stop order ${selected}`),
      h("p.hint", `${plural(stops.length, "stop")} a passenger can board. A trip's times follow its timing from its start time: change the minutes between stops to make a timing of your own.`),
      h("label.field.inline-field", { for: "timing-profile" }, h("span", "Timing"), pick),
      table,
      editable ? h("div.field-row", h("label.field", { for: "timing-label" }, h("span", "Name of the timing"), label)) : null,
      editable ? h("div.btn-row",
        h("button.btn.secondary", { type: "button", on: { click: () => saveTiming(true) } }, "Add as a new timing to the draft"),
        h("button.btn.secondary", { type: "button", id: "timing-update", on: { click: () => (current ? saveTiming(false) : toast("Choose a timing to change, or add this one as new.", "error")) } }, "Change this timing in the draft")) : null,
      note);
    draw();
  }

  function renderAll() {
    renderBoard();
    const edited = dirty();
    const bar = root.querySelector(".trips-save");
    if (bar) bar.hidden = !edited;
  }

  clear(root,
    h("a", { href: `#/route/${r}` }, "Back to the route"),
    h("div.page-head",
      h("div.title-block", h("h1", `Trips of route ${name}`),
        h("p.hint", `${plural(live.trip_count, "trip")} live${prior ? "; your draft already replaces them, and what you see here is the draft's list" : ""}. Trips run every day their service runs.`))),
    h("nav.tabs", { "aria-label": "Stop orders" }, patterns.map((p) => h("button.tab", {
      type: "button", "aria-pressed": String(p.pattern_key === selected),
      on: { click: () => { selected = p.pattern_key; root.querySelectorAll(".tab").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.pattern === String(selected)))); renderBoard(); renderTiming(); } },
      dataset: { pattern: String(p.pattern_key) },
    }, `Stop order ${p.pattern_key}${p.name ? ` · ${p.name}` : ""} (${plural(p.stop_count, "stop")}, ${fmtCount(p.trip_count)} live trips)`))),
    status,
    editable ? h("div.sticky-actions.trips-save", { hidden: true }, h("div.btn-row",
      h("button.btn", { type: "button", on: { click: save } }, prior ? "Update the trips in the draft" : "Save the trips to the draft"),
      h("button.btn.secondary", { type: "button", on: { click: async () => {
        if (await confirmDialog("Discard your trip edits?", "What you changed on this page since it opened goes.", { confirm: "Discard", danger: true })) { setLeaveGuard(null); showTrips(routeId, { pattern: selected }); }
      } } }, "Discard"))) : null,
    board,
    timing);
  renderAll();
  renderTiming();
}
