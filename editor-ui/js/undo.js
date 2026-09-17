// Undo and redo for what is on the screen and NOT yet in a draft: a pin that was
// placed or dragged, a checkbox, a choice in a select, a row added to or moved
// in a stop list, a field once it has been left. Never data: a change already in
// a draft is taken out on the draft page, the normal way, and nothing here calls
// the API.
//
// (state.js had nothing to reuse: it keeps the leave guard, which only knows
// whether there is unsaved work, not what it was.)
//
// One stack at a time, owned by the panel on screen. The router drops it on
// every navigation, a panel starts a new one when it opens, and clears it when
// its state is saved into a draft. Ctrl/Cmd+Z undoes, Ctrl/Cmd+Shift+Z and
// Ctrl/Cmd+Y redo - except while the caret is in a text field, where the
// browser's own undo of the typing keeps working.
import { h, toast } from "./util.js";

const LIMIT = 100;
let scope = null;

const isMac = /Mac|iPhone|iPad/.test(navigator.platform || "");
export const UNDO_KEYS = isMac ? "⌘Z" : "Ctrl+Z";
export const REDO_KEYS = isMac ? "⇧⌘Z" : "Ctrl+Y";

// Fields whose own edit history the browser keeps: leave Ctrl+Z to them.
function editsText(el) {
  if (!el) return false;
  if (el.isContentEditable || el.tagName === "TEXTAREA") return true;
  if (el.tagName !== "INPUT") return false;
  return !["checkbox", "radio", "button", "submit", "reset", "range", "color", "file"].includes(el.type);
}

// One short line, replacing the one before it: several undos in a row do not
// pile up a column of toasts.
function say(message) {
  document.querySelectorAll("#toasts .toast.undo-hint").forEach((t) => t.remove());
  toast(message, "undo-hint");
}

function refresh() {
  if (!scope) return;
  for (const [btn, which] of scope.buttons) {
    if (!document.body.contains(btn)) { scope.buttons.delete(btn); continue; }
    const next = which === "undo" ? scope.done[scope.done.length - 1] : scope.undone[scope.undone.length - 1];
    btn.disabled = !next;
    btn.title = next ? `${which === "undo" ? "Undo" : "Redo"}: ${next.label} (${which === "undo" ? UNDO_KEYS : REDO_KEYS})`
      : `Nothing to ${which} (${which === "undo" ? UNDO_KEYS : REDO_KEYS})`;
  }
}

function step(which) {
  if (!scope) return false;
  const from = which === "undo" ? scope.done : scope.undone;
  const to = which === "undo" ? scope.undone : scope.done;
  const action = from.pop();
  if (!action) {
    say(which === "undo" ? "Nothing to undo here." : "Nothing to redo.");
    return false;
  }
  scope.replaying = true;
  try {
    action[which]();
  } finally {
    scope.replaying = false;
  }
  to.push(action);
  say(`${which === "undo" ? "Undid" : "Redid"}: ${action.label}`);
  refresh();
  return true;
}

export const undo = () => step("undo");
export const redo = () => step("redo");

// The router calls this on every navigation: another entity, another stack.
export function resetUndo() {
  scope = null;
}

// A panel's stack. `what` names it for the buttons' labels ("the stop list").
// Returns {push, clear, buttons, fields, size}.
export function undoScope(what = "this panel") {
  const mine = { what, done: [], undone: [], buttons: new Map(), replaying: false };
  scope = mine;
  const live = () => scope === mine;
  return {
    // {label, undo(), redo()}: label reads after "Undid: " ("moved the pin")
    push(action) {
      if (!live() || mine.replaying) return;
      mine.done.push(action);
      if (mine.done.length > LIMIT) mine.done.shift();
      mine.undone.length = 0;
      refresh();
    },
    // saved into a draft (or thrown away): what came before cannot be undone here
    clear() {
      mine.done.length = 0;
      mine.undone.length = 0;
      refresh();
    },
    size: () => mine.done.length,
    replaying: () => mine.replaying,
    // Undo and Redo buttons, for wherever a pin or a stop list is being edited
    buttons() {
      const u = h("button.btn.quiet.small.undo-btn", { type: "button", dataset: { undo: "undo" }, on: { click: () => { if (live()) undo(); } } }, "↶ Undo");
      const r = h("button.btn.quiet.small.undo-btn", { type: "button", dataset: { undo: "redo" }, on: { click: () => { if (live()) redo(); } } }, "↷ Redo");
      mine.buttons.set(u, "undo");
      mine.buttons.set(r, "redo");
      refresh();
      return h("div.btn-row.undo-row", { role: "group", "aria-label": `Undo and redo in ${what}` }, u, r,
        h("span.hint", `${UNDO_KEYS} undoes what is not in a draft yet.`));
    },
    // Whole-field commits: when a field is left with a new value (its `change`
    // event; at once for a checkbox, radio or select), that is one step. Undoing
    // puts the old value back and fires `input`, so the panel reacts as if it had
    // been typed. `names` maps each element to words ("the name").
    // Returns sync(): take the fields' values as they are now as the baseline
    // (after the panel itself wrote to them, for instance from a dragged pin).
    fields(names) {
      const committed = new Map();
      const read = (el) => (el.type === "checkbox" || el.type === "radio" ? el.checked : el.value);
      const write = (el, v) => {
        if (el.type === "checkbox" || el.type === "radio") el.checked = v; else el.value = v;
        committed.set(el, v);
        el.dispatchEvent(new Event("input", { bubbles: true }));
      };
      for (const [el, words] of names) {
        committed.set(el, read(el));
        el.addEventListener("change", () => {
          const before = committed.get(el), after = read(el);
          if (before === after || mine.replaying) return;
          committed.set(el, after);
          this.push({ label: `changed ${words}`, undo: () => write(el, before), redo: () => write(el, after) });
        });
      }
      return () => { for (const el of committed.keys()) committed.set(el, read(el)); };
    },
  };
}

// The shortcuts, once for the page.
document.addEventListener("keydown", (ev) => {
  if (!(ev.metaKey || ev.ctrlKey) || ev.altKey) return;
  const key = ev.key.toLowerCase();
  const wantsUndo = key === "z" && !ev.shiftKey;
  const wantsRedo = (key === "z" && ev.shiftKey) || (key === "y" && !ev.shiftKey);
  if (!wantsUndo && !wantsRedo) return;
  // typing keeps the browser's own undo; a dialog is its own small world
  if (editsText(ev.target) || editsText(document.activeElement)) return;
  if (document.querySelector("dialog[open]")) return;
  if (!scope) return;
  ev.preventDefault();
  step(wantsUndo ? "undo" : "redo");
});
