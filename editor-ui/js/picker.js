// Choosing a stop: suggestions near a place, a search by name or stop id, or a
// click on the map. The choice is always a stop that exists (live, or created in
// the current draft); nothing about which stop it is can be typed free text.
import { get, enc } from "./api.js";
import { state } from "./state.js";
import { h, clear, debounce, fmtMetres, haversine, plural, normName, toast } from "./util.js";
import * as map from "./map.js";
import { createdStops } from "./drafts.js";

let seq = 0;
const NEAR_DEGREES = 0.006;      // about 650 m each way
const SEARCH_NEAR_DEGREES = 0.045; // about 5 km each way
const box = (at, d) => [at.lat - d, at.lon - d, at.lat + d, at.lon + d].map((x) => x.toFixed(6)).join(",");

// Returns {el, focus, close}. `near` = {lat, lon, label} ranks and measures
// results from that place; `exclude` = stop ids not to offer.
export function stopPicker({ title, near = null, exclude = [], excludeReason = "it cannot be chosen here.", includeDraft = true, suggest = true, onPick, onCancel, mapMessage }) {
  const id = `picker-${++seq}`;
  const excluded = new Set(exclude);
  const input = h("input", {
    type: "search", id: `${id}-q`, autocomplete: "off", spellcheck: "false",
    placeholder: "Stop name or stop id", "aria-describedby": `${id}-status`, "aria-controls": `${id}-results`,
  });
  const status = h("p.hint", { id: `${id}-status`, "aria-live": "polite" });
  const list = h("ol.picker-results", { id: `${id}-results`, "aria-label": "Stops to choose from" });
  let shown = [];
  let highlight = () => {};
  let closed = false;
  let mine = 0;

  const close = () => {
    closed = true;
    map.clearCandidates();
    map.cancelPick();
  };
  const pick = (s) => {
    if (s.location_type === 1) {
      toast(`${s.name} is a station. Choose one of its stops.`, "error");
      return;
    }
    close();
    onPick(s);
  };
  const distance = (s) => (near && s.lat != null ? haversine(near.lat, near.lon, s.lat, s.lon) : null);
  const meta = (s) => {
    const d = distance(s);
    return [
      s.stop_id,
      s.draft ? "new in your draft" : plural(s.route_count ?? 0, "route"),
      s.platform_code || null,
      s.parent_station ? `in station ${s.parent_station}` : null,
      d !== null ? `${fmtMetres(d)} from ${near.label || "here"}` : null,
    ].filter(Boolean).join(" · ");
  };
  const render = (items, message) => {
    if (closed) return;
    shown = items;
    highlight = map.showCandidates(items, pick);
    status.textContent = message;
    clear(list, items.map((s, i) => h("li",
      h("button.picker-item", {
        type: "button", "data-stop": s.stop_id,
        on: {
          click: () => pick(s),
          mouseenter: () => highlight(s.stop_id),
          focus: () => highlight(s.stop_id),
          keydown: (ev) => moveFocus(ev, i),
        },
      },
      h("span.picker-no", { "aria-hidden": "true" }, String(i + 1)),
      h("span.picker-main", h("span.picker-name", s.name), s.draft ? h("span.chip.draft", "New") : null),
      h("span.picker-meta", meta(s))))));
  };
  const moveFocus = (ev, i) => {
    const buttons = [...list.querySelectorAll("button")];
    if (ev.key === "ArrowDown") { ev.preventDefault(); (buttons[i + 1] || buttons[i]).focus(); }
    if (ev.key === "ArrowUp") { ev.preventDefault(); (i === 0 ? input : buttons[i - 1]).focus(); }
    if (ev.key === "Escape") { ev.preventDefault(); cancel(); }
  };

  const draftMatches = (q) => {
    if (!includeDraft) return [];
    const term = normName(q);
    return createdStops().filter((s) => !excluded.has(s.stop_id)
      && (s.stop_id.toLowerCase() === q.toLowerCase() || normName(s.name).includes(term)));
  };

  const suggestions = async () => {
    if (!near || !suggest) {
      render([], "Type at least two letters of the name, or the stop id.");
      return;
    }
    const run = ++mine;
    status.textContent = `Looking for stops near ${near.label || "here"}…`;
    try {
      const page = await get(`feeds/${enc(state.feedId)}/stops?bbox=${box(near, NEAR_DEGREES)}&limit=300`);
      if (run !== mine) return;
      const drafts = includeDraft ? createdStops() : [];
      const items = [...drafts, ...page.items]
        .filter((s) => s.location_type !== 1 && !excluded.has(s.stop_id))
        .map((s) => ({ s, d: distance(s) }))
        .filter((x) => x.d !== null && x.d <= 700)
        .sort((a, b) => a.d - b.d)
        .slice(0, 8)
        .map((x) => x.s);
      render(items, items.length ? `Closest stops to ${near.label || "here"}. Or search by name or stop id.` : "No stops close by. Search by name or stop id.");
    } catch (e) {
      if (run === mine) status.textContent = e.message;
    }
  };

  const search = debounce(async () => {
    const q = input.value.trim();
    if (q.length < 2) return suggestions();
    const run = ++mine;
    try {
      // The server ranks a name search by similarity over the whole feed, so a
      // common name (GANDHI NAGAR) fills a page with stops across the city. Near a
      // place, also search around it, so the kerb meant is among the results.
      const [page, around] = await Promise.all([
        get(`feeds/${enc(state.feedId)}/stops?q=${enc(q)}&limit=12`),
        near ? get(`feeds/${enc(state.feedId)}/stops?q=${enc(q)}&bbox=${box(near, SEARCH_NEAR_DEGREES)}&limit=12`) : null,
      ]);
      if (run !== mine) return;
      const seen = new Set();
      const live = [...(around ? around.items : []), ...page.items]
        .filter((s) => !seen.has(s.stop_id) && seen.add(s.stop_id))
        .filter((s) => s.location_type !== 1 && !excluded.has(s.stop_id));
      let items = [...draftMatches(q), ...live];
      // near a place, the closest match is usually the kerb meant
      if (near) items = items.map((s, i) => ({ s, i, d: distance(s) ?? Infinity })).sort((a, b) => a.d - b.d || a.i - b.i).map((x) => x.s);
      items = items.slice(0, 10);
      render(items, items.length ? `${plural(items.length, "stop")} match “${q}”.` : `No stop matches “${q}”. Try part of the name, or the stop id.`);
    } catch (e) {
      if (run === mine) status.textContent = e.message;
    }
  }, 250);

  const cancel = () => {
    close();
    if (onCancel) onCancel();
  };

  input.addEventListener("input", search);
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "ArrowDown") {
      const first = list.querySelector("button");
      if (first) { ev.preventDefault(); first.focus(); }
    }
    if (ev.key === "Escape") { ev.preventDefault(); cancel(); }
    if (ev.key === "Enter") ev.preventDefault();
  });

  const onMap = () => {
    map.pickStop(mapMessage || "Click the stop on the map.", (s) => {
      if (s.location_type === 1 || excluded.has(s.stop_id)) {
        toast(s.location_type === 1 ? `${s.name} is a station. Click one of its stops.` : `${s.name} (${s.stop_id}): ${excludeReason}`, "error");
        onMap();
        return;
      }
      pick(s);
    });
  };

  const el = h("div.picker", { role: "group", "aria-labelledby": `${id}-title` },
    h("p.picker-title", { id: `${id}-title` }, title),
    h("label.visually-hidden", { for: `${id}-q` }, "Search stops by name or stop id"),
    h("div.picker-bar", input,
      h("button.btn.secondary.small", { type: "button", on: { click: onMap } }, "Pick on the map"),
      onCancel ? h("button.btn.quiet.small", { type: "button", on: { click: cancel } }, "Cancel") : null),
    status,
    list);
  suggestions();
  return { el, focus: () => input.focus(), close, results: () => shown };
}
