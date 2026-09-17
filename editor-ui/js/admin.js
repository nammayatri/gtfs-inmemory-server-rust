// People (users, roles, two-step reset) and the history of everything done.
import { get, post, patch, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtDate, ROLE_LABEL } from "./util.js";

const page = () => document.getElementById("page");

const ROLE_HELP = {
  viewer: "can look at everything",
  editor: "can make drafts and submit them",
  approver: "can also approve, reject and commit other people's drafts",
  admin: "can also add people and change roles",
};

export async function showPeople() {
  if (!can("admin")) {
    return clear(page(), h("div.page-inner", h("h1", "People"), h("p.notice", "Only admins can manage people. Ask an admin if someone needs access.")));
  }
  const tableBox = h("div", h("p.empty", "Loading…"));
  const email = h("input", { type: "email", id: "new-user-email", placeholder: "name@nammayatri.in", required: true });
  const name = h("input", { type: "text", id: "new-user-name", placeholder: "Full name" });
  const role = h("select", { id: "new-user-role" }, Object.keys(ROLE_LABEL).map((r) => h("option", { value: r, selected: r === "editor" }, ROLE_LABEL[r])));
  const formError = h("p.notice.error", { role: "alert", hidden: true });

  const load = async () => {
    try {
      const users = (await get("users")).items;
      clear(tableBox, h("div.table-wrap", h("table",
        h("thead", h("tr", h("th", "Person"), h("th", "Role"), h("th", "Access"), h("th", "Two-step sign-in"), h("th", "Last signed in"), h("th", ""))),
        h("tbody", users.map((u) => userRow(u, load))))));
    } catch (e) {
      clear(tableBox, h("p.notice.error", e.message));
    }
  };

  const add = async (ev) => {
    ev.preventDefault();
    formError.hidden = true;
    try {
      const u = await post("users", { email: email.value.trim(), display_name: name.value.trim() || null, role: role.value });
      toast(`${u.email} added as ${ROLE_LABEL[u.role].toLowerCase()}. They set up two-step sign-in the first time they open the editor.`);
      email.value = ""; name.value = "";
      load();
    } catch (e) {
      formError.hidden = false;
      formError.textContent = e.message;
    }
  };

  clear(page(), h("div.page-inner",
    h("div.title-block", h("h1", "People"), h("p.hint", "Everyone signs in with their nammayatri.in account and a code from an authenticator app.")),
    h("ul.timeline", Object.entries(ROLE_HELP).map(([r, text]) => h("li", h("strong", ROLE_LABEL[r]), ` ${text}.`))),
    tableBox,
    h("form.actionbar", { on: { submit: add } },
      h("h2", "Add a person"),
      h("div", { style: "display:grid;grid-template-columns:2fr 2fr 1fr auto;gap:10px;align-items:end" },
        h("label.field", { for: "new-user-email" }, h("span", "Email"), email),
        h("label.field", { for: "new-user-name" }, h("span", "Name"), name),
        h("label.field", { for: "new-user-role" }, h("span", "Role"), role),
        h("button.btn", { type: "submit" }, "Add person")),
      formError),
  ));
  load();
}

function userRow(u, reload) {
  const self = u.email === state.me.email;
  const update = async (body, message) => {
    try {
      await patch(`users/${enc(u.user_id)}`, body);
      toast(message);
    } catch (e) {
      toast(e.message, "error");
    }
    reload();
  };
  const roleSelect = h("select", {
    "aria-label": `Role for ${u.email}`, disabled: self, title: self ? "You cannot change your own role." : null,
    on: { change: (ev) => update({ role: ev.target.value }, `${u.email} is now ${ROLE_LABEL[ev.target.value].toLowerCase()}.`) },
  }, Object.keys(ROLE_LABEL).map((r) => h("option", { value: r, selected: r === u.role }, ROLE_LABEL[r])));
  const reset = async () => {
    const ok = await confirmDialog("Reset two-step sign-in?",
      `${u.email} is signed out and must scan a new QR code next time. Do this when someone loses or replaces their phone.`, { confirm: "Reset", danger: true });
    if (!ok) return;
    try {
      await post(`users/${enc(u.user_id)}/reset-totp`);
      toast(`Two-step sign-in reset for ${u.email}.`);
    } catch (e) {
      toast(e.message, "error");
    }
    reload();
  };
  return h("tr",
    h("td", h("strong", u.display_name || u.email), u.display_name ? h("div.hint", u.email) : null, self ? h("div.hint", "This is you") : null),
    h("td", roleSelect),
    h("td", u.status === "active" ? "Active" : h("span.chip.rejected", "Turned off")),
    h("td", u.totp_enabled ? "Set up" : "Not set up yet"),
    h("td", u.last_login_at ? fmtDate(u.last_login_at) : "Never"),
    h("td", h("div.btn-row",
      u.totp_enabled ? h("button.btn.quiet.small", { type: "button", on: { click: reset } }, "Reset two-step") : null,
      self ? null : u.status === "active"
        ? h("button.btn.quiet.small", { type: "button", on: { click: async () => {
            if (await confirmDialog("Turn off access?", `${u.email} will not be able to sign in until access is turned back on.`, { confirm: "Turn off", danger: true })) {
              update({ status: "disabled" }, `${u.email} can no longer sign in.`);
            }
          } } }, "Turn off access")
        : h("button.btn.quiet.small", { type: "button", on: { click: () => update({ status: "active" }, `${u.email} can sign in again.`) } }, "Turn on access"))));
}

// ------------------------------------------------------------------ history
// Keys are the audit actions the API writes (src/editor, docs/gtfs-editor.md).
const ACTION_LABEL = {
  seed: "Loaded the feed data",
  release: "Released the feed",
  user_bootstrapped: "First admin sign-in",
  totp_enroll_started: "Started two-step sign-in set-up",
  totp_confirmed: "Set up two-step sign-in",
  totp_failed: "Entered a wrong code",
  session_created: "Signed in",
  user_totp_reset: "Reset someone's two-step sign-in",
  user_created: "Added a person",
  user_updated: "Changed a person's access",
  change_set_created: "Started a draft",
  change_added: "Added a change",
  change_updated: "Updated a change",
  change_removed: "Removed a change",
  change_set_submitted: "Submitted for review",
  change_set_approved: "Approved",
  change_set_rejected: "Rejected",
  change_set_committed: "Committed (live)",
  change_set_reopened: "Reopened",
  change_set_discarded: "Discarded",
  change_set_bulk_imported: "Imported a CSV file",
  station_proposal_approved: "Approved a suggested station into a draft",
  station_proposals_approved: "Approved the suggested stations in an area",
  station_proposal_rejected: "Rejected a suggested station",
  station_proposal_reopened: "Reopened a suggested station",
  station_proposal_returned: "A suggested station went back to review",
  station_proposal_committed: "A suggested station went live",
};

// An action this page has no words for yet still reads as words.
const actionLabel = (action) => ACTION_LABEL[action] || (action.charAt(0).toUpperCase() + action.slice(1)).replace(/_/g, " ");

function detailText(a) {
  const d = a.detail || {};
  const parts = [];
  if (d.title) parts.push(`"${d.title}"`);
  if (d.name) parts.push(d.name);
  if (d.proposal_id) parts.push(`suggestion #${d.proposal_id}`);
  if (d.kind) parts.push(`${d.kind.replace("_", " ")}: ${d.rows} rows, ${d.changes} changes`);
  if (d.approved !== undefined) parts.push(`${d.approved} approved, ${d.skipped} skipped`);
  if (d.note) parts.push(`note: ${d.note}`);
  if (d.reason) parts.push(d.reason);
  if (d.entity) parts.push(`${d.entity.replace("_", " ")} ${d.entity_key || ""}`.trim());
  if (d.email) parts.push(d.email);
  if (d.role) parts.push(`role ${d.role}`);
  if (d.status) parts.push(d.status);
  if (d.comment) parts.push(`comment: ${d.comment}`);
  if (d.version) parts.push(`feed version ${d.version}`);
  if (d.feed_version) parts.push(`feed version ${d.feed_version}, ${d.changes} change${d.changes === 1 ? "" : "s"}`);
  if (d.from) parts.push(`was ${d.from}`);
  return parts.join(", ");
}

export async function showHistory(changeSet) {
  const rows = h("tbody");
  const more = h("button.btn.secondary", { type: "button", hidden: true }, "Show older");
  let cursor = null;
  const load = async () => {
    try {
      const res = await get(`feeds/${enc(state.feedId)}/audit?limit=100${cursor ? `&cursor=${enc(cursor)}` : ""}${changeSet ? `&change_set=${enc(changeSet)}` : ""}`);
      res.items.forEach((a) => rows.appendChild(h("tr",
        h("td", fmtDate(a.at)),
        h("td", a.actor_email || "system"),
        h("td", actionLabel(a.action)),
        h("td", a.change_set_id ? h("a", { href: `#/drafts/${enc(a.change_set_id)}` }, "draft") : ""),
        h("td", detailText(a)))));
      cursor = res.next_cursor;
      more.hidden = !cursor;
      if (!rows.children.length) rows.appendChild(h("tr", h("td", { colspan: "5" }, "Nothing has happened yet.")));
    } catch (e) {
      toast(e.message, "error");
    }
  };
  more.addEventListener("click", load);
  clear(page(), h("div.page-inner",
    h("div.title-block", h("h1", "History"), h("p.hint", changeSet ? "Everything done to one draft." : "Everything people have done in the editor, newest first. This record cannot be edited.")),
    changeSet ? h("a", { href: "#/audit" }, "Show all history") : null,
    h("div.table-wrap", h("table", h("thead", h("tr", h("th", "When"), h("th", "Who"), h("th", "What"), h("th", "Draft"), h("th", "Details"))), rows)),
    h("div.btn-row", more)));
  load();
}
