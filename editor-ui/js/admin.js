// People (users, roles, two-step reset) and the history of everything done.
import { get, post, patch, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtDate, fmtMetres, plural, ROLE_LABEL, STATUS_LABEL } from "./util.js";
import { addChange } from "./drafts.js";

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

// ------------------------------------------------------------------ feed settings
// data_source is GIMS's own vocabulary, straight from gtfs_feed.data_source:
// 'db' (this feed's metadata is served live from the editor tables) or
// 'preprocessed' (the nightly build's static files - every feed's default).
// Switching it is a change like any other (feed_config/update): it goes into the
// current draft and is live only once that draft is committed.
export const DATA_SOURCE_LABEL = { db: "Database (live edits)", preprocessed: "Preprocessed build (static)" };

export async function showFeedSettings() {
  if (!can("admin")) {
    return clear(page(), h("div.page-inner", h("h1", "Feed settings"), h("p.notice", "Only admins can change a feed's data source. Ask an admin if a feed needs to move to or from the database.")));
  }
  const tableBox = h("div", h("p.empty", "Loading…"));

  const load = async () => {
    try {
      const feeds = (await get("feeds")).items;
      // the drafts that already carry a switch, per feed
      const pending = new Map(await Promise.all(feeds.map(async (f) => {
        try {
          return [f.gtfs_id, (await get(`feeds/${enc(f.gtfs_id)}/config`)).pending || []];
        } catch {
          return [f.gtfs_id, []];
        }
      })));
      clear(tableBox, h("div.table-wrap", h("table",
        h("thead", h("tr", h("th", "Feed"), h("th", "Data source"), h("th", "Feed version"), h("th", "Waiting in a draft"), h("th", ""))),
        h("tbody", feeds.length
          ? feeds.map((f) => feedRow(f, pending.get(f.gtfs_id) || [], load))
          : h("tr", h("td", { colspan: "5" }, "No feeds yet."))))));
    } catch (e) {
      clear(tableBox, h("p.notice.error", e.message));
    }
  };

  clear(page(), h("div.page-inner",
    h("div.title-block", h("h1", "Feed settings"), h("p.hint", "Which feeds GIMS serves from the live editor tables instead of the nightly preprocessed build.")),
    h("p.notice", "Switching a feed adds a change to your current draft. It takes effect only after the draft is submitted, approved by someone else and committed - like every other edit."),
    tableBox,
  ));
  load();
}

function feedRow(f, pending, reload) {
  const isDb = f.data_source === "db";
  const target = isDb ? "preprocessed" : "db";
  const name = f.display_name || f.gtfs_id;
  // a draft belongs to one feed: the one chosen in the top bar
  const current = f.gtfs_id === state.feedId;
  const switchIt = async () => {
    const ok = await confirmDialog(
      `Add a switch of ${name} to ${DATA_SOURCE_LABEL[target].toLowerCase()} to your draft?`,
      "This adds a change to your current draft. It takes effect only after the draft is submitted, approved by someone else and committed. Nothing changes for passengers until then.",
      { confirm: "Add to draft" },
    );
    if (!ok) return;
    try {
      await addChange({ entity: "feed_config", op: "update", entity_key: f.gtfs_id, after: { data_source: target } });
    } catch (e) {
      toast(e.message, "error");
    }
    reload();
  };
  return h("tr",
    h("td", h("strong", name), f.display_name ? h("div.hint", f.gtfs_id) : null),
    h("td", DATA_SOURCE_LABEL[f.data_source] || f.data_source),
    h("td", f.version),
    h("td.feed-pending", pending.length
      ? h("ul.list", pending.map((p) => h("li",
          h("a", { href: `#/drafts/${enc(p.change_set_id)}` }, p.change_set_title), " ",
          h("span", { class: `chip ${p.status}` }, STATUS_LABEL[p.status] || p.status),
          h("span.hint", ` to ${(DATA_SOURCE_LABEL[p.data_source] || p.data_source || "").toLowerCase()}`))))
      : h("span.hint", "Nothing waiting")),
    h("td", current
      ? h("button.btn.quiet.small", { type: "button", on: { click: switchIt } }, `Add to draft: switch to ${DATA_SOURCE_LABEL[target].toLowerCase()}`)
      : h("span.hint", "Choose this feed in the top bar to switch it.")));
}

// ------------------------------------------------------------------ history
// Keys are the audit actions the API writes (src/editor, docs/gtfs-editor.md),
// plus the ones nandi's scripts write (seed, release, station_proposals_built,
// position_reviews_loaded).
// Exported for dev/ui_e2e.mjs, which checks every action in the database has one.
export const ACTION_LABEL = {
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
  change_set_self_approved: "Approved their own draft (admin override — no second reviewer)",
  change_set_rejected: "Rejected",
  change_set_committed: "Committed (live)",
  change_set_reopened: "Reopened",
  change_set_discarded: "Discarded",
  bulk_imported: "Imported a CSV file",
  stop_merged: "Merged a duplicate stop",
  station_merged: "Merged two stations",
  station_proposals_built: "Suggested stations were built",
  station_proposal_approved: "Approved a suggested station into a draft",
  station_proposal_rejected: "Rejected a suggested station",
  station_proposal_reopened: "Reopened a suggested station",
  station_proposal_returned: "A suggested station went back to review",
  station_proposal_committed: "A suggested station went live",
  position_reviews_loaded: "Coordinates to review were loaded",
  position_reviews_autofix_planned: "A tool looked for same-named stops that fit the coordinates to review",
  position_review_moved: "Moved a stop from a coordinate review into a draft",
  position_review_split: "Split routes off a stop from a coordinate review into a draft",
  position_review_merged: "Merged a stop from a coordinate review into another stop, in a draft",
  position_review_confirmed: "Confirmed a stop's position",
  position_review_reopened: "Reopened a coordinate review",
  position_review_returned: "A coordinate review went back to review",
  position_review_committed: "A fix from a coordinate review went live",
  feed_data_source_changed: "Changed a feed's data source",
  webhook_created: "Added a webhook",
  webhook_updated: "Changed a webhook",
  webhook_deleted: "Deleted a webhook",
  webhook_tested: "Sent a test webhook call",
  webhook_settings_updated: "Changed where GIMS may send webhooks",
};

// Maker-checker set aside: these rows stand out in the history.
const OVERRIDE_ACTIONS = new Set(["change_set_self_approved"]);
export const OVERRIDE_CHIP_STYLE = "background:var(--danger-tint);color:var(--danger)";
const OVERRIDE_ROW_STYLE = "background:var(--danger-tint)";

// An action this page has no words for yet still reads as words.
const actionLabel = (action) => ACTION_LABEL[action] || (action.charAt(0).toUpperCase() + action.slice(1)).replace(/_/g, " ");

const RETURNED_BECAUSE = {
  change_removed: "its change was removed from the draft",
  change_set_discarded: "its draft was discarded",
};

function detailText(a) {
  const d = a.detail || {};
  const n = (count, one) => plural(Number(count) || 0, one);
  switch (a.action) {
    case "stop_merged":
      return `${d.from} merged into ${d.into}, ${n(d.routes, "route")} and ${n(d.rows, "route row")} switched`;
    case "station_merged":
      return `${d.from} merged into ${d.into}, ${n(d.platforms_moved, "platform")} moved`;
    case "bulk_imported":
      return `${String(d.kind || "").replace("_", " ")}: ${n(d.rows, "row")}, ${n(d.changes, "change")}`;
    case "station_proposals_built":
      return `${n(d.proposals, "suggested station")} (${d.batch})`;
    case "seed":
      return `${n(d.stops, "stop")}, ${n(d.routes, "route")}`;
    case "change_set_submitted":
      return n(d.changes, "change");
    case "station_proposal_committed":
      return `suggestion #${d.proposal_id}, station ${d.station_id}, feed version ${d.feed_version}`;
    case "station_proposal_returned":
      return `suggestion #${d.proposal_id}: ${RETURNED_BECAUSE[d.reason] || d.reason}`;
    case "position_reviews_loaded":
      return `${n(d.queued, "stop")} to review${d.batch ? ` (${d.batch})` : ""}`;
    case "position_reviews_autofix_planned":
      return `${n(d.reviews, "review")}: ${d.merge} to merge, ${d.move} to move, ${d.choose} with candidates to choose from, ${d.none} not helped`;
    case "position_review_returned":
      return `coordinate review #${d.review_id}, stop ${d.stop_id}: ${RETURNED_BECAUSE[d.reason] || d.reason}`;
    case "position_review_moved":
    case "position_review_split":
    case "position_review_merged":
    case "position_review_confirmed":
    case "position_review_reopened":
    case "position_review_committed":
      return [`coordinate review #${d.review_id}`, d.stop_id ? `stop ${d.stop_id}` : null,
        d.into_stop_id ? `merged into ${d.into_stop_id}` : null,
        d.moved_m != null ? `moved ${fmtMetres(d.moved_m)}` : null,
        Array.isArray(d.route_ids) ? `${n(d.route_ids.length, "route")} to new stop ${d.new_stop_id}` : null,
        d.feed_version ? `feed version ${d.feed_version}` : null,
        d.note ? `note: ${d.note}` : null].filter(Boolean).join(", ");
    case "station_proposal_approved": {
      const edits = [d.renamed ? "renamed" : null, d.moved ? "moved" : null,
        d.relabelled ? `${n(d.relabelled, "platform label")} changed` : null,
        d.dropped && d.dropped.length ? `${n(d.dropped.length, "stop")} dropped` : null].filter(Boolean);
      return [`suggestion #${d.proposal_id}, station ${d.station_id}`, d.bulk ? "as suggested, with others in an area" : null, ...edits].filter(Boolean).join(", ");
    }
    case "feed_data_source_changed":
      return `${d.gtfs_id}: ${d.from ? DATA_SOURCE_LABEL[d.from] || d.from : "no row yet"} → ${DATA_SOURCE_LABEL[d.to] || d.to}`;
    case "change_set_self_approved":
      return [`submitted by ${d.submitted_by_email || "the same person"}`, d.comment ? `comment: ${d.comment}` : null].filter(Boolean).join(", ");
    default:
      break;
  }
  const parts = [];
  if (d.title) parts.push(`"${d.title}"`);
  if (d.name) parts.push(d.name);
  if (d.proposal_id) parts.push(`suggestion #${d.proposal_id}`);
  if (d.note) parts.push(`note: ${d.note}`);
  if (d.entity) parts.push(`${d.entity.replace("_", " ")} ${d.entity_key || ""}`.trim());
  if (d.email) parts.push(d.email);
  if (d.role) parts.push(`role ${d.role}`);
  if (d.status) parts.push(d.status);
  if (d.comment) parts.push(`comment: ${d.comment}`);
  if (d.version) parts.push(`feed version ${d.version}`);
  if (d.feed_version) parts.push(`feed version ${d.feed_version}${d.changes !== undefined ? `, ${n(d.changes, "change")}` : ""}`);
  if (d.self_approved) parts.push("self-approved (admin override)");
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
      res.items.forEach((a) => rows.appendChild(h("tr", OVERRIDE_ACTIONS.has(a.action) ? { class: "audit-override", style: OVERRIDE_ROW_STYLE } : {},
        h("td", fmtDate(a.at)),
        h("td", a.actor_email || "system"),
        h("td", OVERRIDE_ACTIONS.has(a.action) ? [h("span.chip.override", { style: OVERRIDE_CHIP_STYLE }, "admin override"), " "] : null, actionLabel(a.action)),
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
