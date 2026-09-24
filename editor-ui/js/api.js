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
// shell can show the sign-in gate instead of each screen handling it. A draft
// answered a page of changes at a time is completed here, so every screen
// works on its whole list.
export async function api(method, path, body) {
  const data = await request(method, path, body);
  if (isPagedSet(data)) await completeChanges(data);
  if (data && isPagedSet(data.change_set)) await completeChanges(data.change_set);
  return data;
}

// A change set whose `changes` are one page of more (docs section 16.5, "Big
// drafts"): the first page carries the replay (validation, conflicts), later
// pages only their slice of changes.
const isPagedSet = (v) => !!(v && v.change_set_id && Array.isArray(v.changes) && v.next_cursor);

async function completeChanges(set) {
  let cursor = set.next_cursor;
  while (cursor) {
    const page = await request("GET", `change-sets/${enc(set.change_set_id)}?limit=500&cursor=${enc(cursor)}`);
    set.changes.push(...page.changes);
    cursor = page.next_cursor;
  }
  set.next_cursor = null;
}

async function request(method, path, body, raw) {
  const headers = { Accept: "application/json" };
  if (method !== "GET") headers["X-Requested-With"] = "gtfs-editor";
  if (body !== undefined) headers["Content-Type"] = "application/json";
  if (raw) headers["Content-Type"] = raw.type;
  let res;
  try {
    res = await fetch(API_BASE + path.replace(/^\//, ""), {
      method,
      headers,
      credentials: "same-origin",
      body: raw ? raw.bytes : body === undefined ? undefined : JSON.stringify(body),
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
    // a grant taken away while the page was open (docs section 15)
    if (res.status === 403 && e.code === "no_feed_access") {
      window.dispatchEvent(new CustomEvent("access:changed", { detail: e }));
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

// A file's bytes as the body (a GTFS zip to import, section 18).
export const postBytes = (path, bytes, type = "application/zip") => request("POST", path, undefined, { bytes, type });

export const enc = encodeURIComponent;
