// The one Leaflet map, and the modes screens put it in: browsing stops (named
// when zoomed in), showing a route, dragging a stop, placing a new stop, picking
// a stop, selecting stops for a station, reviewing a suggested station, reviewing
// a suspected coordinate, comparing two stops, and the small read-only insets.
import {
  TILE_URL, TILE_ATTRIBUTION, TILE_REFERRER_POLICY, DEFAULT_VIEW, STOPS_MIN_ZOOM, LABELS_MIN_ZOOM, LABEL_GROUP_METRES,
} from "./config.js";
import { get, enc } from "./api.js";
import { state, pref, setPref } from "./state.js";
import { debounce, decodePolyline, h, haversine, normName, fmtMetres } from "./util.js";

const L = window.L;
let map;
let lines;                    // canvas for what is drawn but never clicked
let dots;                     // canvas for everything clickable
const layers = {};
let clickHandler = null;      // overrides the default "open stop" click
let picking = null;           // while picking one stop: the mode and banner to go back to
let bannerNow = [null, null];
let selection = null;         // Set of stop ids highlighted for station selection
let placeCleanup = null;
let lastStops = [];
let draftStops = [];
let draftKey = "";
let loadSeq = 0;
const moveListeners = new Set();
// While a suggested station or a merge is on the map, its own labels come
// first: stops it names get no ordinary label, and ordinary labels keep clear
// of its labels.
let quietIds = new Set();
let reserved = [];            // [{lat, lon, text, side}]
const ACTION = "#0b6660", INK = "#14252a", MUTED = "#536569", DRAFT_FILL = "#f2b42c", DRAFT_RING = "#7a5000";
const DANGER = "#b42318", FOCUS = "#1f5fbf";
const MAX_STOPS = 2000;

const text = (s) => h("span", s);

export function initMap() {
  const saved = pref("mapView", null);
  map = L.map("map", { zoomControl: true }).setView(
    saved ? [saved.lat, saved.lon] : [DEFAULT_VIEW.lat, DEFAULT_VIEW.lon],
    saved ? saved.zoom : DEFAULT_VIEW.zoom);
  // Leaflet hit-tests a canvas only against its own layers, and a canvas takes
  // every click over it. A second canvas stacked above the stops (the focus ring
  // and route lines each made one) left the stops under it unclickable. So lines
  // and rings go in a pane below that ignores the pointer, and everything
  // clickable shares the one canvas above it.
  const linePane = map.createPane("lines");
  linePane.style.zIndex = "390";
  linePane.style.pointerEvents = "none";
  map.createPane("dots").style.zIndex = "420";
  lines = L.canvas({ pane: "lines", padding: 0.3 });
  dots = L.canvas({ pane: "dots", padding: 0.3 });
  L.tileLayer(TILE_URL, { maxZoom: 19, attribution: TILE_ATTRIBUTION, referrerPolicy: TILE_REFERRER_POLICY }).addTo(map);
  for (const name of ["route", "proposal", "review", "reviewRoute", "coord", "pair", "focus", "stops", "proposals", "coordPoints", "candidates", "edit", "select", "place", "labels"]) {
    layers[name] = L.layerGroup().addTo(map);
  }
  map.on("moveend", () => {
    const c = map.getCenter();
    setPref("mapView", { lat: c.lat, lon: c.lng, zoom: map.getZoom() });
    redrawLabels();
    loadStops();
    moveListeners.forEach((fn) => fn());
  });
  // The side panel changes width with what it shows, and pages hide the map
  // altogether; keep Leaflet's idea of the map size in step with its box, or
  // tiles cover only part of the map.
  new ResizeObserver(() => map.invalidateSize()).observe(map.getContainer());
  loadStops();
  return map;
}

export const getMap = () => map;

export function invalidate() {
  if (map) setTimeout(() => map.invalidateSize(), 0);
}

export function onMoveEnd(fn) {
  moveListeners.add(fn);
  return () => moveListeners.delete(fn);
}

export function zoom() {
  return map.getZoom();
}

// minLat,minLon,maxLat,maxLon of what is on screen
export function bbox() {
  const b = map.getBounds();
  return [b.getSouth(), b.getWest(), b.getNorth(), b.getEast()].map((x) => x.toFixed(6)).join(",");
}

export function fitPoints(points, { maxZoom = 18, pad = 0.35 } = {}) {
  const pts = points.filter((p) => p && p.lat != null).map((p) => [p.lat, p.lon]);
  if (!pts.length) return;
  if (pts.length === 1) map.setView(pts[0], Math.max(map.getZoom(), Math.min(maxZoom, 17)));
  else map.fitBounds(L.latLngBounds(pts).pad(pad), { maxZoom });
}

// ------------------------------------------------------------------ stops
const loadStops = debounce(async () => {
  const hint = document.getElementById("map-hint");
  if (!state.feedId || !map) return;
  const mine = ++loadSeq;
  if (map.getZoom() < STOPS_MIN_ZOOM) {
    lastStops = [];
    hint.hidden = false;
    drawStops();
    return;
  }
  hint.hidden = true;
  const area = bbox();
  try {
    let items = [], cursor = null;
    do {
      const page = await get(`feeds/${enc(state.feedId)}/stops?bbox=${area}&limit=500${cursor ? `&cursor=${enc(cursor)}` : ""}`);
      if (mine !== loadSeq) return;
      items = items.concat(page.items);
      cursor = page.next_cursor;
    } while (cursor && items.length < MAX_STOPS);
    lastStops = items;
    drawStops();
  } catch {
    /* auth or network errors are announced by the api module */
  }
}, 200);

export function refreshStops() {
  if (map) loadStops();
}

export function loadedStops() {
  return lastStops;
}

// Stops the current draft creates: drawn in amber, clickable, labelled.
export function setDraftStops(stops) {
  const key = stops.map((s) => `${s.stop_id}@${s.lat},${s.lon},${s.name}`).join("|");
  if (key === draftKey) return;
  draftKey = key;
  draftStops = stops;
  if (map) drawStops();
}

function stopTooltip(s) {
  const bits = [s.name];
  if (s.platform_code) bits.push(s.platform_code);
  if (s.location_type === 1) bits.push("station");
  else if (s.draft) bits.push("new, in your draft");
  else if (s.route_count != null) bits.push(`${s.route_count} route${s.route_count === 1 ? "" : "s"}`);
  return text(bits.join(" · "));
}

function stopClicked(s) {
  if (clickHandler) clickHandler(s);
  else location.hash = `#/stop/${enc(s.stop_id)}`;
}

function drawStops() {
  layers.stops.clearLayers();
  const seen = new Set();
  for (const s of [...draftStops, ...lastStops]) {
    if (seen.has(s.stop_id)) continue;
    seen.add(s.stop_id);
    const selected = selection && selection.has(s.stop_id);
    const isStation = s.location_type === 1;
    const m = L.circleMarker([s.lat, s.lon], {
      renderer: dots,
      bubblingMouseEvents: false,
      radius: isStation || selected ? 8 : 6,
      color: selected ? ACTION : isStation ? INK : s.draft ? DRAFT_RING : "#ffffff",
      weight: selected ? 4 : 2,
      fillColor: s.draft ? DRAFT_FILL : isStation ? "#ffffff" : selected ? ACTION : s.parent_station ? INK : ACTION,
      fillOpacity: 1,
    });
    m.bindTooltip(stopTooltip(s), { direction: "top", offset: [0, -6] });
    m.on("click", () => stopClicked(s));
    m.addTo(layers.stops);
  }
  redrawLabels();
}

// One label per place: the platforms of a station share the station's label,
// and same-named stops within LABEL_GROUP_METRES (the two kerbs of one road)
// share one. Stations, then the stops with most routes, get space first; a
// label that would overlap one already placed is left out (the stop still
// names itself on hover).
function labelGroups(stops) {
  const stations = new Map();
  const loose = [];
  for (const s of stops) {
    const sid = s.location_type === 1 ? s.stop_id : s.parent_station;
    if (!sid) { loose.push(s); continue; }
    if (!stations.has(sid)) stations.set(sid, { station: null, members: [] });
    const g = stations.get(sid);
    if (s.location_type === 1) g.station = s; else g.members.push(s);
  }
  const centre = (ms) => ({ lat: ms.reduce((a, m) => a + m.lat, 0) / ms.length, lon: ms.reduce((a, m) => a + m.lon, 0) / ms.length });
  const groups = [];
  for (const g of stations.values()) {
    const at = g.station || centre(g.members);
    groups.push({ name: (g.station || g.members[0]).name, lat: at.lat, lon: at.lon, weight: 1e6 + g.members.length });
  }
  const named = [];
  loose.sort((a, b) => (b.route_count || 0) - (a.route_count || 0));
  for (const s of loose) {
    const key = normName(s.name);
    const g = named.find((x) => x.key === key && haversine(x.lat, x.lon, s.lat, s.lon) <= LABEL_GROUP_METRES);
    if (g) {
      g.members.push(s);
      Object.assign(g, centre(g.members));
      g.weight += s.route_count || 0;
    } else {
      named.push({ key, name: s.name, lat: s.lat, lon: s.lon, members: [s], weight: s.route_count || 0 });
    }
  }
  return [...groups, ...named].sort((a, b) => b.weight - a.weight);
}

const labelBox = (lat, lon, textValue, side = "right") => {
  const p = map.latLngToContainerPoint([lat, lon]);
  const w = Math.min(280, 12 + textValue.length * 7);
  return { x: side === "right" ? p.x + 10 : p.x - 14 - w, y: p.y - 10, w, h: 20 };
};

// Labels on the map now, by place, so a redraw keeps the ones that stay put
// (Leaflet takes a removed tooltip off the page only 200 ms later).
const shownLabels = new Map();

function redrawLabels() {
  if (!map) return;
  const want = new Map();
  if (map.getZoom() >= LABELS_MIN_ZOOM) {
    const bounds = map.getBounds().pad(0.05);
    const pool = [...draftStops, ...lastStops].filter((s) => bounds.contains([s.lat, s.lon]) && !quietIds.has(s.stop_id));
    const size = map.getSize();
    const placed = reserved.map((r) => labelBox(r.lat, r.lon, r.text, r.side));
    for (const g of labelGroups(pool)) {
      const p = map.latLngToContainerPoint([g.lat, g.lon]);
      const box = { x: p.x + 10, y: p.y - 10, w: Math.min(280, 12 + g.name.length * 7), h: 20 };
      if (box.x > size.x || box.y > size.y || box.x + box.w < 0 || box.y + box.h < 0) continue;
      if (placed.some((b) => b.x < box.x + box.w && box.x < b.x + b.w && b.y < box.y + box.h && box.y < b.y + b.h)) continue;
      placed.push(box);
      want.set(`${g.name}|${g.lat.toFixed(6)}|${g.lon.toFixed(6)}`, g);
    }
  }
  for (const [key, tooltip] of shownLabels) {
    if (!want.has(key)) {
      layers.labels.removeLayer(tooltip);
      shownLabels.delete(key);
    }
  }
  for (const [key, g] of want) {
    if (shownLabels.has(key)) continue;
    const tooltip = L.tooltip({ permanent: true, direction: "right", offset: [9, 0], className: "stop-label", interactive: false, opacity: 1 })
      .setLatLng([g.lat, g.lon]).setContent(text(g.name));
    layers.labels.addLayer(tooltip);
    shownLabels.set(key, tooltip);
  }
}

// ------------------------------------------------------------------ focus
export function focusStop(stop, { zoom: z = 17 } = {}) {
  layers.focus.clearLayers();
  if (!stop || stop.lat == null) return;
  L.circleMarker([stop.lat, stop.lon], { renderer: lines, radius: 14, color: ACTION, weight: 3, fill: false, interactive: false })
    .addTo(layers.focus);
  if (!map.getBounds().pad(-0.2).contains([stop.lat, stop.lon]) || map.getZoom() < STOPS_MIN_ZOOM) {
    map.setView([stop.lat, stop.lon], Math.max(map.getZoom(), z));
  }
}

export function clearFocus() {
  layers.focus.clearLayers();
}

// ------------------------------------------------------------------ routes
export function showRoute(route, { fit = true, layer = "route", dashed = false, color, weight = 5, markers = true } = {}) {
  const group = layers[layer];
  group.clearLayers();
  const served = route.rows.filter((r) => r.stop_type !== "ROUTE CORRECTION" && r.lat != null);
  let line = null;
  if (route.encoded_polyline) {
    try { line = decodePolyline(route.encoded_polyline); } catch { line = null; }
  }
  const lineColor = color || route.color || INK;
  if (line && line.length > 1) {
    L.polyline(line, { renderer: lines, color: lineColor, weight, opacity: 0.85, dashArray: dashed ? "8 8" : null, interactive: false }).addTo(group);
  } else if (served.length > 1) {
    L.polyline(served.map((r) => [r.lat, r.lon]), { renderer: lines, color: lineColor, weight: Math.max(2, weight - 2), opacity: 0.6, dashArray: "4 8", interactive: false }).addTo(group);
  }
  if (markers) {
    route.rows.forEach((r, i) => {
      if (r.stop_type === "ROUTE CORRECTION") {
        if (r.marker_lat == null) return;
        L.marker([r.marker_lat, r.marker_lon], {
          icon: L.divIcon({ className: "", html: '<div class="marker-pin"></div>', iconSize: [10, 10] }),
          keyboard: false, interactive: false,
        }).addTo(group);
        return;
      }
      if (r.lat == null) return;
      const isNew = r.stop_type === "NEW STOP";
      L.circleMarker([r.lat, r.lon], {
        renderer: dots,
        bubblingMouseEvents: false,
        radius: isNew ? 7 : 5,
        color: r.stop_type === "JUMP STOP" ? MUTED : INK,
        weight: 2,
        dashArray: r.stop_type === "JUMP STOP" ? "2 2" : null,
        fillColor: isNew ? INK : "#ffffff",
        fillOpacity: 1,
      })
        .bindTooltip(text(`${i + 1}. ${r.stop_name || r.stop_id}${isNew ? ` (stage ${r.stage_no})` : ""}`), { direction: "top" })
        .on("click", () => stopClicked({ stop_id: r.stop_id, name: r.stop_name, lat: r.lat, lon: r.lon, parent_station: r.parent_station, location_type: 0 }))
        .addTo(group);
    });
  }
  if (fit) {
    const pts = line && line.length > 1 ? line : served.map((r) => [r.lat, r.lon]);
    if (pts.length) map.fitBounds(L.latLngBounds(pts).pad(0.08), { maxZoom: 16 });
  }
}

export function clearRoute(layer = "route") {
  layers[layer].clearLayers();
}

// ------------------------------------------------------------------ modes
function banner(message, onCancel) {
  bannerNow = [message, onCancel];
  const el = document.getElementById("map-banner");
  if (!message) {
    el.hidden = true;
    el.replaceChildren();
    return;
  }
  el.replaceChildren(h("span", message),
    ...(onCancel ? [h("button.btn.quiet.small", { type: "button", on: { click: onCancel } }, "Cancel")] : []));
  el.hidden = false;
}

export function endModes() {
  clickHandler = null;
  picking = null;
  selection = null;
  if (placeCleanup) placeCleanup();
  placeCleanup = null;
  for (const name of ["edit", "select", "candidates", "pair", "review", "reviewRoute", "proposals", "coord", "coordPoints"]) layers[name].clearLayers();
  quietIds = new Set();
  reserved = [];
  banner(null);
  drawStops();
}

// Drag a stop to a new place. onMove(lat, lon) fires as it moves; returns
// setPosition(lat, lon) so typed coordinates can move the pin.
export function dragStop(stop, onMove) {
  endModes();
  L.circleMarker([stop.lat, stop.lon], { renderer: lines, radius: 7, color: MUTED, weight: 2, fillColor: "#fff", fillOpacity: 1, interactive: false })
    .addTo(layers.edit);
  const pin = L.marker([stop.lat, stop.lon], {
    draggable: true,
    autoPan: true,
    icon: L.divIcon({ className: "", html: '<div class="stop-pin"></div>', iconSize: [16, 16] }),
    title: "Drag to move this stop",
  }).addTo(layers.edit);
  const link = L.polyline([[stop.lat, stop.lon], [stop.lat, stop.lon]], { renderer: lines, color: ACTION, dashArray: "4 6", weight: 2, interactive: false }).addTo(layers.edit);
  const update = (lat, lon) => link.setLatLngs([[stop.lat, stop.lon], [lat, lon]]);
  pin.on("drag", () => { const p = pin.getLatLng(); update(p.lat, p.lng); onMove(p.lat, p.lng); });
  focusStop(stop);
  banner("Drag the teal pin to the right kerb, or type the coordinates.", null);
  return (lat, lon) => { pin.setLatLng([lat, lon]); update(lat, lon); };
}

// Click the map to place a new stop; the pin can then be dragged. onPlace(lat,
// lon) fires on every placement. Returns set(lat, lon) for typed positions.
export function placePoint(onPlace, { at = null } = {}) {
  endModes();
  const box = map.getContainer();
  let pin = null;
  const put = (lat, lon) => {
    if (!pin) {
      pin = L.marker([lat, lon], {
        draggable: true, autoPan: true, title: "The new stop. Drag to adjust.",
        icon: L.divIcon({ className: "", html: '<div class="stop-pin new"></div>', iconSize: [18, 18] }),
      }).addTo(layers.place);
      pin.on("drag", () => { const p = pin.getLatLng(); onPlace(p.lat, p.lng); });
    } else {
      pin.setLatLng([lat, lon]);
    }
    banner("Drag the amber pin to the kerb, or click the map to move it.", null);
  };
  const onClick = (ev) => { put(ev.latlng.lat, ev.latlng.lng); onPlace(ev.latlng.lat, ev.latlng.lng); };
  map.on("click", onClick);
  box.classList.add("placing");
  placeCleanup = () => { map.off("click", onClick); box.classList.remove("placing"); layers.place.clearLayers(); };
  if (at) {
    put(at.lat, at.lon);
    if (!map.getBounds().contains([at.lat, at.lon])) map.setView([at.lat, at.lon], Math.max(map.getZoom(), 17));
  } else {
    banner("Click the map where buses stop to place the new stop.", null);
  }
  return (lat, lon) => {
    put(lat, lon);
    if (!map.getBounds().pad(-0.1).contains([lat, lon])) map.panTo([lat, lon]);
  };
}

// Click a stop (from the stop layer or a shown route) to pick it. Whatever mode
// the map was in (selecting a station's stops, say) comes back afterwards.
export function pickStop(message, onPick, onCancel) {
  if (!picking) picking = { handler: clickHandler, banner: bannerNow };
  clickHandler = (s) => { cancelPick(); onPick(s); };
  banner(message, () => { cancelPick(); if (onCancel) onCancel(); });
}

export function cancelPick() {
  if (!picking) return;
  clickHandler = picking.handler;
  banner(...picking.banner);
  picking = null;
}

// Station selection: clicks toggle membership, the station pin can be dragged.
export function selectStops(initialIds, onToggle) {
  selection = new Set(initialIds);
  clickHandler = (s) => {
    if (s.location_type === 1) return;
    if (selection.has(s.stop_id)) selection.delete(s.stop_id);
    else selection.add(s.stop_id);
    drawStops();
    onToggle(s, selection.has(s.stop_id));
  };
  banner("Click stops to add them to the station or take them out.", null);
  drawStops();
  return {
    set(ids) { selection = new Set(ids); drawStops(); },
  };
}

export function stationPin(lat, lon, onMove) {
  layers.select.clearLayers();
  const pin = L.marker([lat, lon], {
    draggable: true,
    icon: L.divIcon({ className: "", html: '<div class="station-pin"></div>', iconSize: [18, 18] }),
    title: "Drag to place the station",
  }).addTo(layers.select);
  pin.on("drag", () => { const p = pin.getLatLng(); onMove(p.lat, p.lng); });
  return (la, lo) => pin.setLatLng([la, lo]);
}

// Search results or suggestions for a stop picker, numbered like the list.
// Clicking one picks it. Returns highlight(stopId).
export function showCandidates(stops, onPick) {
  layers.candidates.clearLayers();
  const rings = new Map();
  stops.forEach((s, i) => {
    if (s.lat == null) return;
    const ring = L.circleMarker([s.lat, s.lon], {
      renderer: dots, bubblingMouseEvents: false, radius: 11, color: ACTION, weight: 3, fillColor: "#ffffff", fillOpacity: 0.55,
    }).bindTooltip(text(String(i + 1)), { permanent: true, direction: "center", className: "candidate-label", interactive: false })
      .on("click", () => onPick(s))
      .addTo(layers.candidates);
    rings.set(s.stop_id, ring);
  });
  return (stopId) => rings.forEach((r, id) => r.setStyle({ color: id === stopId ? "#1f5fbf" : ACTION, weight: id === stopId ? 5 : 3 }));
}

export function clearCandidates() {
  layers.candidates.clearLayers();
}

// ------------------------------------------------------------------ station proposals
// The suggested stations in a list, as small dark squares; the open one is ringed.
export function showProposalPoints(items, { selectedId = null, onOpen } = {}) {
  layers.proposals.clearLayers();
  for (const p of items) {
    const selected = p.proposal_id === selectedId;
    L.marker([p.lat, p.lon], {
      icon: L.divIcon({ className: "", html: `<div class="proposal-pin ${p.status}${selected ? " selected" : ""}"></div>`, iconSize: [14, 14] }),
      title: `Suggested station: ${p.name}`,
      keyboard: false,
      zIndexOffset: selected ? 500 : 0,
    }).on("click", () => onOpen(p)).addTo(layers.proposals);
  }
}

// One suggested station being reviewed: the station point (draggable when
// editable), each member kerb ringed with its platform label, and a dashed tie
// from the point to each member. Returns {update(point, members), showRoute(route)}.
export function showReview({ lat, lon, members, editable, onMovePoint }) {
  layers.review.clearLayers();
  layers.reviewRoute.clearLayers();
  const ties = L.layerGroup().addTo(layers.review);
  // one label per member, updated in place: Leaflet removes a tooltip 200 ms
  // after it is taken off the map, so redrawing them on every keystroke would
  // leave fading copies behind
  const labelGroup = L.layerGroup().addTo(layers.review);
  const labels = [];
  let lastReserved = "";
  let point = { lat, lon };
  let current = members;
  const pin = L.marker([lat, lon], {
    draggable: !!editable,
    icon: L.divIcon({ className: "", html: '<div class="station-pin"></div>', iconSize: [18, 18] }),
    title: editable ? "The station point. Drag to move it." : "The station point",
    keyboard: false,
    zIndexOffset: 1000,
  }).addTo(layers.review);
  if (editable) pin.on("drag", () => { const p = pin.getLatLng(); point = { lat: p.lat, lon: p.lng }; draw(); onMovePoint(p.lat, p.lng); });
  const draw = () => {
    ties.clearLayers();
    quietIds = new Set(current.map((m) => m.stop_id).filter(Boolean));
    reserved = current.map((m) => ({ lat: m.lat, lon: m.lon, text: m.label || "No platform label", side: m.lon >= point.lon ? "right" : "left" }));
    const reservedKey = JSON.stringify(reserved);
    if (reservedKey !== lastReserved) {
      lastReserved = reservedKey;
      redrawLabels();
    }
    current.forEach((m, i) => {
      const color = m.dropped ? MUTED : ACTION;
      L.polyline([[point.lat, point.lon], [m.lat, m.lon]], { renderer: lines, color, weight: 2, dashArray: "3 6", interactive: false }).addTo(ties);
      L.circleMarker([m.lat, m.lon], { renderer: lines, radius: 12, color, weight: m.dropped ? 2 : 3, dashArray: m.dropped ? "3 4" : null, fill: false, interactive: false })
        .addTo(ties);
      const side = m.lon >= point.lon ? "right" : "left";
      const words = m.dropped ? `${m.label || "No label"} (dropped)` : m.label || "No platform label";
      let label = labels[i];
      if (label && label.options.direction !== side) {
        labelGroup.removeLayer(label);
        label = null;
      }
      if (!label) {
        label = L.tooltip({ permanent: true, direction: side, offset: [side === "right" ? 14 : -14, 0], className: "member-label", interactive: false, opacity: 1 });
        labels[i] = label;
        label.setLatLng([m.lat, m.lon]).setContent(text(words));
        labelGroup.addLayer(label);
      } else {
        label.setLatLng([m.lat, m.lon]);
        if (label.getElement()?.textContent !== words) label.setContent(text(words));
      }
      label.getElement()?.classList.toggle("dropped", !!m.dropped);
    });
    labels.splice(current.length).forEach((l) => labelGroup.removeLayer(l));
  };
  draw();
  fitPoints([point, ...members], { maxZoom: 19, pad: 0.6 });
  return {
    update(nextPoint, nextMembers) {
      if (nextPoint) { point = nextPoint; pin.setLatLng([point.lat, point.lon]); }
      if (nextMembers) current = nextMembers;
      draw();
    },
    showRoute(route) {
      if (!route) { layers.reviewRoute.clearLayers(); return; }
      showRoute(route, { layer: "reviewRoute", fit: false, color: "#1f5fbf", weight: 4, markers: false });
    },
  };
}

// ------------------------------------------------------------------ coordinate reviews
const REVIEW_STATUSES = new Set(["pending", "approved", "committed", "confirmed", "superseded"]);

// The reviews in a list, as small round pins; the open one is ringed.
export function showReviewPoints(items, { selectedId = null, onOpen } = {}) {
  layers.coordPoints.clearLayers();
  for (const r of items) {
    const status = REVIEW_STATUSES.has(r.status) ? r.status : "";
    const selected = r.review_id === selectedId;
    L.marker([r.lat, r.lon], {
      icon: L.divIcon({ className: "", html: `<div class="coord-pin ${status}${selected ? " selected" : ""}"></div>`, iconSize: [14, 14] }),
      title: `Coordinate to review: ${r.stop_name}`,
      keyboard: false,
      zIndexOffset: selected ? 500 : 0,
    }).on("click", () => onOpen(r)).addTo(layers.coordPoints);
  }
}

// How a route's legs are drawn: through the stop as it is now (red), faded where
// the new point takes the route away, and to the new point for a move (teal), a
// split (blue) or a move already in a draft (amber). `legKind` lets tests count them.
const LEG_STYLE = {
  now: { color: DANGER, weight: 3, opacity: 0.8 },
  was: { color: MUTED, weight: 2, opacity: 0.6, dashArray: "4 6" },
  move: { color: ACTION, weight: 4, opacity: 0.9 },
  split: { color: FOCUS, weight: 4, opacity: 0.9 },
  drafted: { color: DRAFT_RING, weight: 4, opacity: 0.85 },
};

// One suspected coordinate: the stop's point now, where it was when the review
// was loaded, the raw MTC point, the suggestion, the raw points of the stops its
// routes came from, the stops sharing its point, and each route's previous ->
// stop -> next legs beside the straight line the bus would take without the
// stop, so a detour is plain to see. With `onPlace(lat, lon, how)`, a click on
// the map or on a stop, or dragging the pin, sets a new point.
// Returns {setLegs(specs), setPin(point), setDrafted(points), setSharing(stops), fit(points)};
// a leg spec is {prev, next, via: {lat, lon}, kind, routes: [route numbers]}.
export function showPositionReview({ stopId, current, loaded = null, raw = null, suggestion = null, origins = [], onPlace = null }) {
  layers.coord.clearLayers();
  const direct = L.layerGroup().addTo(layers.coord);
  const legs = L.layerGroup().addTo(layers.coord);
  const ends = L.layerGroup().addTo(layers.coord);
  const marks = L.layerGroup().addTo(layers.coord);
  const sharingGroup = L.layerGroup().addTo(layers.coord);
  const draftedGroup = L.layerGroup().addTo(layers.coord);
  let fixedLabels = [], sharingLabels = [], draftedLabels = [], pinLabel = null;
  let sharingIds = [];
  const box = map.getContainer();

  const labelAt = (group, lat, lon, words, cls, side = "right") => {
    L.tooltip({ permanent: true, direction: side, offset: [side === "right" ? 14 : -14, 0], className: `coord-label ${cls}`, interactive: false, opacity: 1 })
      .setLatLng([lat, lon]).setContent(text(words)).addTo(group);
    return { lat, lon, text: words, side };
  };
  const pinIcon = (cls) => L.divIcon({ className: "", html: `<div class="${cls}"></div>`, iconSize: [14, 14] });
  // ordinary stop labels stay clear of this review's labels, and do not repeat its stops
  const reserve = () => {
    quietIds = new Set([stopId, ...sharingIds]);
    reserved = [...fixedLabels, ...sharingLabels, ...draftedLabels, pinLabel].filter(Boolean);
    redrawLabels();
  };

  L.circleMarker([current.lat, current.lon], { renderer: lines, radius: 13, color: DANGER, weight: 3, fill: false, interactive: false }).addTo(marks);
  fixedLabels.push(labelAt(marks, current.lat, current.lon, "Now", "now", "left"));
  if (loaded && haversine(loaded.lat, loaded.lon, current.lat, current.lon) > 5) {
    L.circleMarker([loaded.lat, loaded.lon], { renderer: lines, radius: 9, color: MUTED, weight: 2, dashArray: "3 4", fill: false, interactive: false }).addTo(marks);
    fixedLabels.push(labelAt(marks, loaded.lat, loaded.lon, "When loaded", "muted", "left"));
  }
  if (raw) {
    L.marker([raw.lat, raw.lon], { icon: pinIcon("raw-pin"), keyboard: false, interactive: false }).addTo(marks);
    const nearSuggestion = suggestion && haversine(raw.lat, raw.lon, suggestion.lat, suggestion.lon) < 30;
    fixedLabels.push(labelAt(marks, raw.lat, raw.lon, "MTC raw point", "raw", nearSuggestion ? "left" : "right"));
  }
  if (suggestion) {
    L.marker([suggestion.lat, suggestion.lon], { icon: pinIcon("suggest-pin"), keyboard: false, interactive: false }).addTo(marks);
    const source = suggestion.source || "";
    fixedLabels.push(labelAt(marks, suggestion.lat, suggestion.lon, `Suggestion${source ? `: ${source.length > 60 ? `${source.slice(0, 58)}…` : source}` : ""}`, "suggest"));
  }
  for (const o of origins) {
    L.marker([o.lat, o.lon], { icon: pinIcon("raw-pin origin"), keyboard: false, interactive: false }).addTo(marks);
    fixedLabels.push(labelAt(marks, o.lat, o.lon, o.label, "raw"));
  }

  let pin = null, pinLine = null;
  if (onPlace) {
    const onClick = (ev) => onPlace(ev.latlng.lat, ev.latlng.lng, "click");
    map.on("click", onClick);
    box.classList.add("placing");
    clickHandler = (s) => onPlace(s.lat, s.lon, "stop");
    placeCleanup = () => { map.off("click", onClick); box.classList.remove("placing"); };
  }
  reserve();

  return {
    setLegs(specs) {
      direct.clearLayers();
      legs.clearLayers();
      ends.clearLayers();
      const drawn = new Set(), straight = new Set(), stops = new Map();
      for (const s of specs) {
        const key = `${s.kind}|${s.prev?.stop_id}|${s.next?.stop_id}|${s.via.lat.toFixed(6)},${s.via.lon.toFixed(6)}`;
        for (const end of [s.prev, s.next]) {
          if (!end) continue;
          if (!stops.has(end.stop_id)) stops.set(end.stop_id, { end, routes: new Set() });
          (s.routes || []).forEach((x) => stops.get(end.stop_id).routes.add(x));
        }
        if (drawn.has(key)) continue;
        drawn.add(key);
        const pts = [s.prev && [s.prev.lat, s.prev.lon], [s.via.lat, s.via.lon], s.next && [s.next.lat, s.next.lon]].filter(Boolean);
        if (pts.length > 1) L.polyline(pts, { renderer: lines, interactive: false, legKind: s.kind, ...LEG_STYLE[s.kind] }).addTo(legs);
        const pair = s.prev && s.next ? `${s.prev.stop_id}|${s.next.stop_id}` : null;
        if (pair && !straight.has(pair)) {
          straight.add(pair);
          L.polyline([[s.prev.lat, s.prev.lon], [s.next.lat, s.next.lon]], { renderer: lines, color: INK, weight: 1.5, opacity: 0.5, dashArray: "2 5", interactive: false, legKind: "direct" }).addTo(direct);
        }
      }
      for (const { end, routes } of stops.values()) {
        const m = L.circleMarker([end.lat, end.lon], { renderer: dots, bubblingMouseEvents: false, radius: 5, color: INK, weight: 2, fillColor: "#ffffff", fillOpacity: 1 })
          .bindTooltip(text(`${end.name} (${end.stop_id})${routes.size ? `, next to this stop on ${[...routes].slice(0, 8).join(", ")}${routes.size > 8 ? "…" : ""}` : ""}`), { direction: "top" });
        if (onPlace) m.on("click", () => onPlace(end.lat, end.lon, "stop"));
        m.addTo(ends);
      }
    },
    setPin(p) {
      if (!p) {
        if (pin) { layers.coord.removeLayer(pin); layers.coord.removeLayer(pinLine); }
        pin = null;
        pinLine = null;
        pinLabel = null;
        reserve();
        return;
      }
      // the pin's label goes on the side away from a label already there (the
      // suggestion's, when the pin is put on the suggestion)
      const at = map.latLngToContainerPoint([p.lat, p.lon]);
      const side = [...fixedLabels, ...sharingLabels].some((l) => l.side === "right" && map.latLngToContainerPoint([l.lat, l.lon]).distanceTo(at) < 24) ? "left" : "right";
      const bindLabel = () => pin.unbindTooltip().bindTooltip(text("New position"), {
        permanent: true, direction: side, offset: [side === "right" ? 12 : -12, 0], className: "coord-label new", interactive: false,
      });
      if (!pin) {
        pin = L.marker([p.lat, p.lon], {
          draggable: !!onPlace, autoPan: true, keyboard: false, zIndexOffset: 1000, title: "The new position. Drag it to the kerb.",
          icon: L.divIcon({ className: "", html: '<div class="stop-pin"></div>', iconSize: [16, 16] }),
        });
        pinLine = L.polyline([[current.lat, current.lon], [p.lat, p.lon]], { renderer: lines, color: ACTION, weight: 2, dashArray: "4 6", interactive: false }).addTo(layers.coord);
        bindLabel();
        pin.addTo(layers.coord);
        pin.on("drag", () => {
          const q = pin.getLatLng();
          pinLine.setLatLngs([[current.lat, current.lon], [q.lat, q.lng]]);
          onPlace(q.lat, q.lng, "drag");
        });
        pin.on("dragend", () => {
          const q = pin.getLatLng();
          pinLabel = { lat: q.lat, lon: q.lng, text: "New position", side: pinLabel ? pinLabel.side : "right" };
          reserve();
        });
      } else {
        pin.setLatLng([p.lat, p.lon]);
        pinLine.setLatLngs([[current.lat, current.lon], [p.lat, p.lon]]);
        if (!pinLabel || pinLabel.side !== side) bindLabel();
      }
      pinLabel = { lat: p.lat, lon: p.lon, text: "New position", side };
      reserve();
    },
    // one marker per action already in the review's draft (a move, or a split's
    // new stop); `points` is an array of {lat, lon, label}, or empty/null to clear
    setDrafted(points) {
      draftedGroup.clearLayers();
      draftedLabels = [];
      for (const p of points || []) {
        L.circleMarker([p.lat, p.lon], { renderer: lines, radius: 8, color: DRAFT_RING, weight: 3, fillColor: DRAFT_FILL, fillOpacity: 1, interactive: false }).addTo(draftedGroup);
        draftedLabels.push(labelAt(draftedGroup, p.lat, p.lon, p.label || "In the draft", "drafted"));
      }
      reserve();
    },
    // stops that share (or shared) the point: one label for those still on it
    setSharing(stops) {
      sharingGroup.clearLayers();
      sharingLabels = [];
      const same = stops.filter((s) => s.lat != null && haversine(s.lat, s.lon, current.lat, current.lon) <= 3);
      sharingIds = same.map((s) => s.stop_id);
      if (same.length) {
        const names = same.slice(0, 2).map((s) => s.name).join(", ");
        sharingLabels.push(labelAt(sharingGroup, current.lat, current.lon, `Same point: ${names}${same.length > 2 ? ` +${same.length - 2}` : ""}`, "muted"));
      }
      stops.filter((s) => s.lat != null && !same.includes(s)).forEach((s) => {
        L.circleMarker([s.lat, s.lon], { renderer: lines, radius: 6, color: INK, weight: 2, fillColor: "#ffffff", fillOpacity: 1, interactive: false }).addTo(sharingGroup);
        sharingLabels.push(labelAt(sharingGroup, s.lat, s.lon, s.name, "muted"));
      });
      reserve();
    },
    fit(points, { maxZoom = 18 } = {}) {
      fitPoints(points, { maxZoom, pad: 0.25 });
    },
  };
}

// Two stops side by side for a merge: the one that stays and the one that goes.
export function showPair(keep, drop) {
  layers.pair.clearLayers();
  if (!keep || !drop) return;
  quietIds = new Set([keep.stop_id, drop.stop_id]);
  reserved = [{ lat: keep.lat, lon: keep.lon, text: `Stays: ${keep.stop_id}`, side: "left" }, { lat: drop.lat, lon: drop.lon, text: `Goes: ${drop.stop_id}`, side: "right" }];
  L.polyline([[keep.lat, keep.lon], [drop.lat, drop.lon]], { renderer: lines, color: INK, weight: 2, dashArray: "4 6", interactive: false }).addTo(layers.pair);
  for (const [s, label, color] of [[keep, `Stays: ${keep.stop_id}`, ACTION], [drop, `Goes: ${drop.stop_id}`, "#b42318"]]) {
    L.circleMarker([s.lat, s.lon], { renderer: lines, radius: 13, color, weight: 3, fill: false, interactive: false }).addTo(layers.pair);
    L.tooltip({ permanent: true, direction: s === keep ? "left" : "right", offset: [s === keep ? -15 : 15, 0], className: "member-label", interactive: false, opacity: 1 })
      .setLatLng([s.lat, s.lon]).setContent(text(label)).addTo(layers.pair);
  }
  fitPoints([keep, drop], { maxZoom: 19, pad: 0.8 });
  redrawLabels();
}

// ------------------------------------------------------------------ insets
// A small read-only map: points and lines, fitted. A point may carry `color`,
// `label` (permanent) or `title` (on hover), and `onClick`.
export function inset(el, { points = [], lines: paths = [], maxZoom = 18 }) {
  const m = L.map(el, { zoomControl: false, attributionControl: false, dragging: true, scrollWheelZoom: false });
  const canvas = L.canvas({ padding: 0.2 });
  L.tileLayer(TILE_URL, { maxZoom: 19, referrerPolicy: TILE_REFERRER_POLICY }).addTo(m);
  const all = [];
  for (const ln of paths) {
    if (ln.pts.length > 1) {
      L.polyline(ln.pts, { renderer: canvas, color: ln.color || INK, weight: ln.weight || 4, dashArray: ln.dashed ? "6 6" : null, opacity: 0.85, interactive: false }).addTo(m);
      all.push(...ln.pts);
    }
  }
  for (const p of points) {
    const color = p.color || (p.kind === "before" ? MUTED : ACTION);
    const marker = L.circleMarker([p.lat, p.lon], {
      renderer: canvas, radius: p.radius || 7, weight: p.weight ?? 3, color: p.ring || color,
      fillColor: p.kind === "before" ? "#fff" : color, fillOpacity: 1, interactive: !!(p.onClick || p.title),
    });
    if (p.label) marker.bindTooltip(text(p.label), { permanent: true, direction: "top", className: "member-label" });
    else if (p.title) marker.bindTooltip(text(p.title), { direction: "top" });
    if (p.onClick) marker.on("click", p.onClick);
    marker.addTo(m);
    all.push([p.lat, p.lon]);
  }
  if (all.length) m.fitBounds(L.latLngBounds(all).pad(0.4), { maxZoom });
  // by the time this fires the change list may have re-rendered and dropped this
  // inset (map.remove()'d, its container gone): invalidateSize on a removed map
  // throws deep in Leaflet, so skip it once the container is no longer attached
  setTimeout(() => { if (document.body.contains(el)) m.invalidateSize(); }, 50);
  return m;
}

// A human sentence for the distance between two points.
export function apart(a, b) {
  return fmtMetres(haversine(a.lat, a.lon, b.lat, b.lon));
}
