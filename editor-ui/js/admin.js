// People (who may work on which feed, admins, two-step reset) and the history
// of everything done.
import { get, post, put, patch, del, enc } from "./api.js";
import { state, isAdmin } from "./state.js";
import { h, clear, toast, confirmDialog, fmtDate, fmtMetres, plural, ROLE_LABEL, STATUS_LABEL } from "./util.js";
import { addChange } from "./drafts.js";

const page = () => document.getElementById("page");

// Roles are held per feed (docs/gtfs-editor.md section 15); only an admin is
// global. Someone who is not an admin is a "member" of the feeds they are given.
const FEED_ROLES = ["viewer", "editor", "approver"];
const ROLE_HELP = {
  viewer: "can look at the feed",
  editor: "can also make drafts on it and submit them",
  approver: "can also approve, reject and commit other people's drafts on it",
  admin: "has every feed at every role, and adds people and gives them feeds",
};
const feedName = (f) => f.display_name || f.gtfs_id;

export async function showPeople() {
  if (!isAdmin()) {
    return clear(page(), h("div.page-inner", h("h1", "People"), h("p.notice", "Only admins can manage people. Ask an admin if someone needs access.")));
  }
  const tableBox = h("div", h("p.empty", "Loading…"));
  const current = state.feeds.find((f) => f.gtfs_id === state.feedId);
  const email = h("input", { type: "email", id: "new-user-email", placeholder: "name@nammayatri.in", required: true });
  const name = h("input", { type: "text", id: "new-user-name", placeholder: "Full name" });
  const feedRole = h("select", { id: "new-user-feed-role" },
    h("option", { value: "" }, "No access"),
    FEED_ROLES.map((r) => h("option", { value: r, selected: r === "editor" }, ROLE_LABEL[r])));
  const admin = h("input", { type: "checkbox", id: "new-user-admin" });
  admin.addEventListener("change", () => { feedRole.disabled = admin.checked; });
  const formError = h("p.notice.error", { role: "alert", hidden: true });

  const load = async () => {
    try {
      const [users, feeds] = await Promise.all([get("users"), get("feeds")]);
      clear(tableBox, h("div.table-wrap", h("table.people",
        h("thead", h("tr", h("th", "Person"), h("th", "Admin"),
          feeds.items.map((f) => h("th.feed-col", { scope: "col", title: f.gtfs_id }, feedName(f))),
          h("th", "Access"), h("th", "Two-step sign-in"), h("th", "Last signed in"), h("th", ""))),
        h("tbody", users.items.map((u) => userRow(u, feeds.items, load))))));
    } catch (e) {
      clear(tableBox, h("p.notice.error", e.message));
    }
  };

  const add = async (ev) => {
    ev.preventDefault();
    formError.hidden = true;
    const role = admin.checked ? "" : feedRole.value;
    try {
      const u = await post("users", {
        email: email.value.trim(), display_name: name.value.trim() || null, admin: admin.checked,
        feeds: role && current ? [{ gtfs_id: current.gtfs_id, role }] : [],
      });
      const as = u.is_admin ? "an admin" : role ? `${ROLE_LABEL[role].toLowerCase()} on ${feedName(current)}` : "a member with no feeds yet";
      toast(`${u.email} added as ${as}. They set up two-step sign-in the first time they open the editor.`);
      email.value = ""; name.value = ""; admin.checked = false; feedRole.disabled = false;
      load();
    } catch (e) {
      formError.hidden = false;
      formError.textContent = e.message;
    }
  };

  clear(page(), h("div.page-inner",
    h("div.title-block", h("h1", "People"), h("p.hint", "Everyone signs in with their nammayatri.in account and a code from an authenticator app. Viewer, editor and approver are given one feed at a time; an admin has every feed.")),
    h("ul.timeline", Object.entries(ROLE_HELP).map(([r, text]) => h("li", h("strong", ROLE_LABEL[r]), ` ${text}.`))),
    tableBox,
    h("form.actionbar", { on: { submit: add } },
      h("h2", "Add a person"),
      h("div", { style: "display:grid;grid-template-columns:2fr 2fr 1fr auto auto;gap:10px;align-items:end" },
        h("label.field", { for: "new-user-email" }, h("span", "Email"), email),
        h("label.field", { for: "new-user-name" }, h("span", "Name"), name),
        h("label.field", { for: "new-user-feed-role" }, h("span", `Role on ${current ? feedName(current) : "this feed"}`), feedRole),
        h("label.check", { for: "new-user-admin" }, admin, h("span", "Admin")),
        h("button.btn", { type: "submit" }, "Add person")),
      h("p.hint", "Give them more feeds, or change a role, in the table above."),
      formError),
  ));
  load();
}

// One row: the person, the Admin switch, a role picker per feed (an admin has
// them all), and the account's housekeeping.
function userRow(u, feeds, reload) {
  const self = u.email === state.me.email;
  const system = u.kind === "system";
  const act = async (send, message) => {
    try {
      await send();
      toast(message);
    } catch (e) {
      toast(e.message, "error");
    }
    reload();
  };
  const adminSwitch = h("input", {
    type: "checkbox", role: "switch", class: "admin-switch", checked: !!u.is_admin, disabled: self || system,
    "aria-label": `Admin: ${u.email}`,
    title: self ? "You cannot remove your own admin access." : system ? "A system account is never an admin." : null,
    on: { change: async (ev) => {
      const on = ev.target.checked;
      const ok = await confirmDialog(on ? `Make ${u.email} an admin?` : `Remove ${u.email}'s admin access?`,
        on ? "An admin works on every feed at every role, adds people and gives them feeds, and may approve their own drafts as an override. The feeds they were given are replaced by all of them."
          : "They keep no feed at all until you give them some, one feed at a time, in this table.",
        { confirm: on ? "Make admin" : "Remove admin access", danger: !on });
      if (!ok) { ev.target.checked = !on; return; }
      act(() => patch(`users/${enc(u.user_id)}`, { admin: on }),
        on ? `${u.email} is now an admin.` : `${u.email} is no longer an admin, and has no feeds until you give some.`);
    } },
  });
  const grants = new Map((u.feeds || []).map((g) => [g.gtfs_id, g]));
  const cells = feeds.map((f) => {
    if (u.is_admin) return h("td.feed-cell", h("span.hint", "Every role"));
    const g = grants.get(f.gtfs_id);
    const pick = h("select.feed-role-pick", {
      "aria-label": `${u.email} on ${feedName(f)}`, dataset: { user: u.email, feed: f.gtfs_id },
      title: g ? `Given by ${g.granted_by_email || "the migration that introduced feed access"} on ${fmtDate(g.granted_at)}` : null,
      on: { change: (ev) => {
        const role = ev.target.value;
        if (role) {
          act(() => put(`users/${enc(u.user_id)}/feeds/${enc(f.gtfs_id)}`, { role }),
            `${u.email} is now ${ROLE_LABEL[role].toLowerCase()} on ${feedName(f)}.`);
        } else {
          act(() => del(`users/${enc(u.user_id)}/feeds/${enc(f.gtfs_id)}`), `${u.email} no longer has ${feedName(f)}.`);
        }
      } },
    }, h("option", { value: "", selected: !g }, "No access"),
    FEED_ROLES.map((r) => h("option", { value: r, selected: g?.role === r }, ROLE_LABEL[r])));
    return h("td.feed-cell", pick);
  });
  const reset = async () => {
    const ok = await confirmDialog("Reset two-step sign-in?",
      `${u.email} is signed out and must scan a new QR code next time. Do this when someone loses or replaces their phone.`, { confirm: "Reset", danger: true });
    if (ok) act(() => post(`users/${enc(u.user_id)}/reset-totp`), `Two-step sign-in reset for ${u.email}.`);
  };
  return h("tr", { dataset: { email: u.email } },
    h("td", h("strong", u.display_name || u.email), u.display_name ? h("div.hint", u.email) : null,
      self ? h("div.hint", "This is you") : null,
      system ? h("div", h("span.chip.system-account", { title: "An account a program uses; nobody signs in with it." }, "System account")) : null),
    h("td", h("label.check", adminSwitch, h("span", u.is_admin ? "Admin" : "Member"))),
    cells,
    h("td", u.status === "active" ? "Active" : h("span.chip.rejected", "Turned off")),
    h("td", system ? h("span.hint", "Never signs in") : u.totp_enabled ? "Set up" : "Not set up yet"),
    h("td", u.last_login_at ? fmtDate(u.last_login_at) : "Never"),
    h("td", h("div.btn-row",
      u.totp_enabled ? h("button.btn.quiet.small", { type: "button", on: { click: reset } }, "Reset two-step") : null,
      self ? null : u.status === "active"
        ? h("button.btn.quiet.small", { type: "button", on: { click: async () => {
            if (await confirmDialog("Turn off access?", `${u.email} will not be able to sign in until access is turned back on.`, { confirm: "Turn off", danger: true })) {
              act(() => patch(`users/${enc(u.user_id)}`, { status: "disabled" }), `${u.email} can no longer sign in.`);
            }
          } } }, "Turn off access")
        : h("button.btn.quiet.small", { type: "button", on: { click: () => act(() => patch(`users/${enc(u.user_id)}`, { status: "active" }), `${u.email} can sign in again.`) } }, "Turn on access"))));
}

// ------------------------------------------------------------------ feed settings
// data_source is GIMS's own vocabulary, straight from gtfs_feed.data_source:
// 'db' (this feed's metadata is served live from the editor tables) or
// 'preprocessed' (the nightly build's static files - every feed's default).
// Switching it is a change like any other (feed_config/update): it goes into the
// current draft and is live only once that draft is committed.
export const DATA_SOURCE_LABEL = { db: "Database (live edits)", preprocessed: "Preprocessed build (static)" };

export async function showFeedSettings() {
  if (!isAdmin()) {
    return clear(page(), h("div.page-inner", h("h1", "Feed settings"), h("p.notice", "Only admins can change a feed's data source. Ask an admin if a feed needs to move to or from the database.")));
  }
  const tableBox = h("div", h("p.empty", "Loading…"));

  const load = async () => {
    try {
      const feeds = (await get("feeds")).items;
      // each feed's settings, and the drafts that already carry a switch
      const configs = new Map(await Promise.all(feeds.map(async (f) => {
        try {
          return [f.gtfs_id, await get(`feeds/${enc(f.gtfs_id)}/config`)];
        } catch {
          return [f.gtfs_id, {}];
        }
      })));
      const pending = new Map([...configs.entries()].map(([g, c]) => [g, c.pending || []]));
      clear(tableBox, h("div.table-wrap", h("table",
        h("thead", h("tr", h("th", "Feed"), h("th", "Data source"), h("th", "Trips from"), h("th", "Feed version"), h("th", "Waiting in a draft"), h("th", ""))),
        h("tbody", feeds.length
          ? feeds.map((f) => feedRow(f, pending.get(f.gtfs_id) || [], configs.get(f.gtfs_id) || {}, load))
          : h("tr", h("td", { colspan: "6" }, "No feeds yet."))))));
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

function feedRow(f, pending, config, reload) {
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
  // trips from the tables need the feed served from them (trips_need_db)
  const trips = config.trips_source;
  const tripsTarget = trips === "db" ? "preprocessed" : "db";
  const switchTrips = async () => {
    const ok = await confirmDialog(
      `Add a switch of ${name}'s trips to ${DATA_SOURCE_LABEL[tripsTarget].toLowerCase()} to your draft?`,
      "Trips, their stop times and calendars then come from where you choose, once the draft is committed. Check the feed's trips first: the feed report and the Trips pages show them.",
      { confirm: "Add to draft" },
    );
    if (!ok) return;
    try {
      await addChange({ entity: "feed_config", op: "update", entity_key: f.gtfs_id, after: { trips_source: tripsTarget } });
    } catch (e) {
      toast(e.message, "error");
    }
    reload();
  };
  return h("tr",
    h("td", h("strong", name), f.display_name ? h("div.hint", f.gtfs_id) : null),
    h("td", DATA_SOURCE_LABEL[f.data_source] || f.data_source),
    h("td", trips ? DATA_SOURCE_LABEL[trips] || trips : "",
      trips && current ? h("div", h("button.btn.quiet.small", { type: "button", on: { click: switchTrips } }, `Switch to ${DATA_SOURCE_LABEL[tripsTarget].toLowerCase()}`)) : null),
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
  gtfs_draft_import: "Drafted changes from a GTFS zip",
  release: "Released the feed",
  user_bootstrapped: "First admin sign-in",
  totp_enroll_started: "Started two-step sign-in set-up",
  totp_confirmed: "Set up two-step sign-in",
  totp_failed: "Entered a wrong code",
  session_created: "Signed in",
  user_totp_reset: "Reset someone's two-step sign-in",
  user_created: "Added a person",
  user_updated: "Changed a person's access",
  user_admin_changed: "Made someone an admin, or took it away",
  feed_access_granted: "Let someone into this feed",
  feed_access_changed: "Changed someone's role on this feed",
  feed_access_revoked: "Took someone's access to this feed away",
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
  feed_config_changed: "Changed a feed setting",
  webhook_created: "Added a webhook",
  webhook_updated: "Changed a webhook",
  webhook_deleted: "Deleted a webhook",
  webhook_tested: "Sent a test webhook call",
  release_requested: "Asked for a Nandi release",
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
    case "feed_config_changed":
      return `${d.gtfs_id}: ${d.setting} ${d.from ?? "—"} → ${d.to ?? "—"}`;
    case "feed_access_granted":
    case "feed_access_changed":
    case "feed_access_revoked": {
      const role = (r) => (r ? ROLE_LABEL[r]?.toLowerCase() || r : "no access");
      return `${d.email}: ${role(d.role_before)} → ${role(d.role_after)}`;
    }
    case "user_admin_changed":
      return `${d.email}: ${d.admin_after ? "now an admin" : "no longer an admin"}`;
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
