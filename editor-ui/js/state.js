// Shared state and the few UI preferences worth remembering. Only preferences
// go to localStorage - never tokens, codes or data.
const listeners = new Set();

export const state = {
  me: null,
  feeds: [],
  feedId: null,
  draft: null,          // the full change set being edited, or null
};

export function set(patch) {
  Object.assign(state, patch);
  listeners.forEach((fn) => fn(state));
}

export function subscribe(fn) {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

const PREF_KEY = "gtfs-editor-prefs";

export function pref(name, fallback) {
  try {
    const all = JSON.parse(localStorage.getItem(PREF_KEY) || "{}");
    return name in all ? all[name] : fallback;
  } catch {
    return fallback;
  }
}

export function setPref(name, value) {
  try {
    const all = JSON.parse(localStorage.getItem(PREF_KEY) || "{}");
    if (value === null || value === undefined) delete all[name];
    else all[name] = value;
    localStorage.setItem(PREF_KEY, JSON.stringify(all));
  } catch {
    /* storage unavailable: preferences are a convenience */
  }
}

// Unsaved work: an editor registers a function that returns a sentence saying
// what would be lost (or null), and the router asks before leaving.
let leaveGuard = null;
export function setLeaveGuard(fn) {
  leaveGuard = fn || null;
}
export function leaveMessage() {
  try {
    return leaveGuard ? leaveGuard() : null;
  } catch {
    return null;
  }
}

const RANK = { viewer: 0, editor: 1, approver: 2, admin: 3 };
export const can = (role) => !!state.me && RANK[state.me.role] >= RANK[role];
