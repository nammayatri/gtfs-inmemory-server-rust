// The one Leaflet map, and the few modes screens put it in: browsing stops,
// showing a route, dragging a stop, picking a stop, selecting stops for a station.
import { TILE_URL, TILE_ATTRIBUTION, TILE_REFERRER_POLICY, DEFAULT_VIEW, STOPS_MIN_ZOOM } from "./config.js";
import { get, enc } from "./api.js";
import { state, pref, setPref } from "./state.js";
import { debounce, decodePolyline, h } from "./util.js";

const L = window.L;
let map;
let renderer;
const layers = {};
let clickHandler = null;      // overrides the default "open stop" click
let selection = null;         // Set of stop ids highlighted for station selection
let lastStops = [];
const ACTION = "#0b6660", INK = "#14252a", MUTED = "#536569";

export function initMap() {
  const saved = pref("mapView", null);
  map = L.map("map", { zoomControl: true, preferCanvas: true }).setView(
    saved ? [saved.lat, saved.lon] : [DEFAULT_VIEW.lat, DEFAULT_VIEW.lon],
    saved ? saved.zoom : DEFAULT_VIEW.zoom);
  renderer = L.canvas({ padding: 0.3 });
  L.tileLayer(TILE_URL, { maxZoom: 19, attribution: TILE_ATTRIBUTION, referrerPolicy: TILE_REFERRER_POLICY }).addTo(map);
  for (const name of ["stops", "route", "proposal", "focus", "edit", "select"]) {
    layers[name] = L.layerGroup().addTo(map);
  }
  map.on("moveend", debounce(onMove, 250));
  onMove();
  return map;
}

export const getMap = () => map;

export function invalidate() {
  if (map) setTimeout(() => map.invalidateSize(), 0);
}

async function onMove() {
  const c = map.getCenter();
  setPref("mapView", { lat: c.lat, lon: c.lng, zoom: map.getZoom() });
  const hint = document.getElementById("map-hint");
  if (!state.feedId) return;
  if (map.getZoom() < STOPS_MIN_ZOOM) {
    layers.stops.clearLayers();
    lastStops = [];
    hint.hidden = false;
    return;
  }
  hint.hidden = true;
  const b = map.getBounds();
  const bbox = [b.getSouth(), b.getWest(), b.getNorth(), b.getEast()].map((x) => x.toFixed(6)).join(",");
  try {
    const page = await get(`feeds/${enc(state.feedId)}/stops?bbox=${bbox}&limit=500`);
    lastStops = page.items;
    drawStops();
  } catch {
    /* auth or network errors are announced by the api module */
  }
}

export function refreshStops() {
  if (map) onMove();
}

function drawStops() {
  layers.stops.clearLayers();
  for (const s of lastStops) {
    const selected = selection && selection.has(s.stop_id);
    const isStation = s.location_type === 1;
    const m = L.circleMarker([s.lat, s.lon], {
      renderer,
      radius: isStation ? 8 : selected ? 8 : 6,
      color: selected ? ACTION : isStation ? INK : "#ffffff",
      weight: selected ? 4 : 2,
      fillColor: isStation ? "#ffffff" : selected ? ACTION : s.parent_station ? INK : ACTION,
      fillOpacity: 1,
    });
    m.bindTooltip(`${s.name}${isStation ? " (station)" : ""}`, { direction: "top", offset: [0, -6] });
    m.on("click", () => {
      if (clickHandler) clickHandler(s);
      else location.hash = `#/stop/${enc(s.stop_id)}`;
    });
    m.addTo(layers.stops);
  }
}

// ------------------------------------------------------------------ focus
export function focusStop(stop, { zoom = 17 } = {}) {
  layers.focus.clearLayers();
  if (!stop || stop.lat == null) return;
  L.circleMarker([stop.lat, stop.lon], { radius: 14, color: ACTION, weight: 3, fill: false, interactive: false })
    .addTo(layers.focus);
  if (!map.getBounds().pad(-0.2).contains([stop.lat, stop.lon]) || map.getZoom() < STOPS_MIN_ZOOM) {
    map.setView([stop.lat, stop.lon], Math.max(map.getZoom(), zoom));
  }
}

export function clearFocus() {
  layers.focus.clearLayers();
}

// ------------------------------------------------------------------ routes
export function showRoute(route, { fit = true, layer = "route", dashed = false, color } = {}) {
  const group = layers[layer];
  group.clearLayers();
  const served = route.rows.filter((r) => r.stop_type !== "ROUTE CORRECTION" && r.lat != null);
  let line = null;
  if (route.encoded_polyline) {
    try { line = decodePolyline(route.encoded_polyline); } catch { line = null; }
  }
  const lineColor = color || route.color || INK;
  if (line && line.length > 1) {
    L.polyline(line, { color: lineColor, weight: 5, opacity: 0.85, dashArray: dashed ? "8 8" : null, interactive: false }).addTo(group);
  } else if (served.length > 1) {
    L.polyline(served.map((r) => [r.lat, r.lon]), { color: lineColor, weight: 3, opacity: 0.6, dashArray: "4 8", interactive: false }).addTo(group);
  }
  route.rows.forEach((r, i) => {
    if (r.stop_type === "ROUTE CORRECTION") {
      if (r.marker_lat == null) return;
      L.marker([r.marker_lat, r.marker_lon], {
        icon: L.divIcon({ className: "", html: '<div class="marker-pin"></div>', iconSize: [10, 10] }),
        keyboard: false,
      }).bindTooltip(`Map shaping point: ${r.marker_name || r.marker_id}`).addTo(group);
      return;
    }
    if (r.lat == null) return;
    const isNew = r.stop_type === "NEW STOP";
    L.circleMarker([r.lat, r.lon], {
      renderer,
      radius: isNew ? 7 : 5,
      color: r.stop_type === "JUMP STOP" ? MUTED : INK,
      weight: 2,
      dashArray: r.stop_type === "JUMP STOP" ? "2 2" : null,
      fillColor: isNew ? INK : "#ffffff",
      fillOpacity: 1,
    })
      .bindTooltip(`${i + 1}. ${r.stop_name}${isNew ? ` (stage ${r.stage_no})` : ""}`, { direction: "top" })
      .on("click", () => {
        if (clickHandler) clickHandler({ stop_id: r.stop_id, name: r.stop_name, lat: r.lat, lon: r.lon });
        else location.hash = `#/stop/${enc(r.stop_id)}`;
      })
      .addTo(group);
  });
  if (fit) {
    const pts = line && line.length > 1 ? line : served.map((r) => [r.lat, r.lon]);
    if (pts.length) map.fitBounds(L.latLngBounds(pts).pad(0.08), { maxZoom: 16 });
  }
}

export function clearRoute(layer = "route") {
  layers[layer].clearLayers();
}

// ------------------------------------------------------------------ modes
function banner(text, onCancel) {
  const el = document.getElementById("map-banner");
  if (!text) {
    el.hidden = true;
    el.replaceChildren();
    return;
  }
  el.replaceChildren(h("span", text),
    onCancel ? h("button.btn.quiet.small", { type: "button", on: { click: onCancel } }, "Cancel") : null);
  el.hidden = false;
}

export function endModes() {
  clickHandler = null;
  selection = null;
  layers.edit.clearLayers();
  layers.select.clearLayers();
  banner(null);
  drawStops();
}

// Drag a stop to a new place. onMove(lat, lon) fires as it moves; returns
// setPosition(lat, lon) so typed coordinates can move the pin.
export function dragStop(stop, onMove) {
  endModes();
  layers.edit.clearLayers();
  L.circleMarker([stop.lat, stop.lon], { radius: 7, color: MUTED, weight: 2, fillColor: "#fff", fillOpacity: 1, interactive: false })
    .bindTooltip("Current position", { permanent: false }).addTo(layers.edit);
  const pin = L.marker([stop.lat, stop.lon], {
    draggable: true,
    autoPan: true,
    icon: L.divIcon({ className: "", html: '<div class="stop-pin"></div>', iconSize: [16, 16] }),
    title: "Drag to move this stop",
  }).addTo(layers.edit);
  const link = L.polyline([[stop.lat, stop.lon], [stop.lat, stop.lon]], { color: ACTION, dashArray: "4 6", weight: 2, interactive: false }).addTo(layers.edit);
  const update = (lat, lon) => link.setLatLngs([[stop.lat, stop.lon], [lat, lon]]);
  pin.on("drag", () => { const p = pin.getLatLng(); update(p.lat, p.lng); onMove(p.lat, p.lng); });
  focusStop(stop);
  banner("Drag the teal pin to the right kerb, or type the coordinates.", null);
  return (lat, lon) => { pin.setLatLng([lat, lon]); update(lat, lon); };
}

// Click a stop (from the stop layer or a shown route) to pick it.
export function pickStop(message, onPick) {
  clickHandler = (s) => { clickHandler = null; banner(null); onPick(s); };
  banner(message, () => { clickHandler = null; banner(null); });
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

// A small read-only map for a diff: markers and lines, fitted.
export function inset(el, { points = [], lines = [] }) {
  const m = L.map(el, { zoomControl: false, attributionControl: false, dragging: true, scrollWheelZoom: false });
  L.tileLayer(TILE_URL, { maxZoom: 19, referrerPolicy: TILE_REFERRER_POLICY }).addTo(m);
  const all = [];
  for (const ln of lines) {
    if (ln.pts.length > 1) {
      L.polyline(ln.pts, { color: ln.color || INK, weight: ln.weight || 4, dashArray: ln.dashed ? "6 6" : null, opacity: 0.85 }).addTo(m);
      all.push(...ln.pts);
    }
  }
  for (const p of points) {
    L.circleMarker([p.lat, p.lon], {
      radius: 7, weight: 3, color: p.kind === "before" ? MUTED : ACTION,
      fillColor: p.kind === "before" ? "#fff" : ACTION, fillOpacity: 1,
    }).bindTooltip(p.label || "", { permanent: !!p.label, direction: "top" }).addTo(m);
    all.push([p.lat, p.lon]);
  }
  if (all.length) m.fitBounds(L.latLngBounds(all).pad(0.4), { maxZoom: 18 });
  setTimeout(() => m.invalidateSize(), 50);
  return m;
}
