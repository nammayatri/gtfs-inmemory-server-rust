// The only deployment-specific constants. Change TILE_URL to an internal tile
// server if the ops network cannot reach openstreetmap.org.
export const TILE_URL = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
export const TILE_ATTRIBUTION = "&copy; OpenStreetMap contributors";

// Chennai, where the first feed lives.
export const DEFAULT_VIEW = { lat: 13.0569, lon: 80.2425, zoom: 12 };

// Stops are fetched by map area only from this zoom level in.
export const STOPS_MIN_ZOOM = 15;

// The API is the directory above the dashboard: /internal/gtfs-editor/ui/ calls
// /internal/gtfs-editor/, and behind Pomerium /  calls /api/ the same way.
function apiBase() {
  const url = new URL(window.location.href);
  url.hash = "";
  url.search = "";
  // .../ui/index.html and .../ui both mean the .../ui/ directory
  const last = url.pathname.split("/").pop();
  if (last.includes(".")) url.pathname = url.pathname.slice(0, -last.length);
  if (!url.pathname.endsWith("/")) url.pathname += "/";
  return new URL("../", url).toString();
}
export const API_BASE = apiBase();
