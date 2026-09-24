import { API_BASE } from "./config.js";

export class ApiError extends Error {
  constructor(status, code, message, details) {
    super(message);
    this.status = status;
    this.code = code;
    this.details = details || {};
  }
}

// Every call goes through here: same-origin cookies, JSON both ways, the
// mutation header, and one error shape. An auth failure is announced so the
// shell can show the sign-in gate instead of each screen handling it.
export async function api(method, path, body) {
  const headers = { Accept: "application/json" };
  if (method !== "GET") headers["X-Requested-With"] = "gtfs-editor";
  if (body !== undefined) headers["Content-Type"] = "application/json";
  let res;
  try {
    res = await fetch(API_BASE + path.replace(/^\//, ""), {
      method,
      headers,
      credentials: "same-origin",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  } catch (e) {
    throw new ApiError(0, "network", "The editor server could not be reached. Check your VPN connection and try again.");
  }
  if (res.status === 204) return null;
  let data = null;
  const text = await res.text();
  if (text) {
    try { data = JSON.parse(text); } catch { data = null; }
  }
  if (!res.ok) {
    const err = (data && data.error) || {};
    const e = new ApiError(res.status, err.code || `http_${res.status}`,
      err.message || `The server answered ${res.status}.`, err.details);
    if (res.status === 401 && path !== "auth/session" && path !== "auth/totp/confirm") {
      window.dispatchEvent(new CustomEvent("auth:required", { detail: e }));
    }
    throw e;
  }
  return data;
}

export const get = (path) => api("GET", path);
export const post = (path, body) => api("POST", path, body === undefined ? {} : body);
export const put = (path, body) => api("PUT", path, body);
export const patch = (path, body) => api("PATCH", path, body);
export const del = (path) => api("DELETE", path);

export const enc = encodeURIComponent;
