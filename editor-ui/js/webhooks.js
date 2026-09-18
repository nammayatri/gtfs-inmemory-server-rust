// Delivery: which version of the feed each pod is serving, the webhooks GIMS
// calls when something happens to it, and what was delivered.
//
// The point of the page is the first table. An edit is committed in a second,
// but it is only *live* once every pod has reloaded, and anything downstream
// that caches GIMS's answers - the S3/CloudFront frontline layer - must be
// rebuilt after that, not before. See docs/gtfs-editor.md section 12.
import { get, post, patch, del, enc } from "./api.js";
import { state, can } from "./state.js";
import { h, clear, toast, confirmDialog, fmtDate, plural, modal, errorText } from "./util.js";

const page = () => document.getElementById("page");

const EVENT_LABEL = {
  feed_in_sync: "Every pod is serving the edit",
  feed_committed: "A draft was committed",
  feed_reload_failed: "A pod could not load the data",
};

const EVENT_HELP = {
  feed_in_sync: "The safe moment to rebuild a cache that sits in front of GIMS. Waits until no pod is still serving older data.",
  feed_committed: "Fires at once, without waiting for the pods. Use it to tell people, not to rebuild a cache.",
  feed_reload_failed: "An alert: a pod is stuck on older data and needs looking at.",
};

const DELIVERY_LABEL = {
  pending: "Waiting to be sent",
  in_flight: "Being sent",
  succeeded: "Delivered",
  failed: "Gave up",
  abandoned: "Never sent",
};

// The page polls while it is open; leaving it stops that.
let timer = null;

export function leaveWebhooks() {
  if (timer) { clearInterval(timer); timer = null; }
}

export async function showDelivery() {
  const feed = state.feedId;
  const fleetBox = h("div", h("p.empty", "Loading…"));
  const hooksBox = h("div");
  const historyBox = h("div");

  const load = async () => {
    if (state.feedId !== feed) return leaveWebhooks();
    const [fleet, hooks, history] = await Promise.all([
      get(`feeds/${enc(feed)}/cache-state`).catch((e) => ({ error: e })),
      get(`feeds/${enc(feed)}/webhooks`).catch((e) => ({ error: e })),
      get(`feeds/${enc(feed)}/webhook-deliveries?limit=25`).catch((e) => ({ error: e })),
    ]);
    renderFleet(fleetBox, fleet);
    renderHooks(hooksBox, hooks, load);
    renderHistory(historyBox, history);
  };

  clear(page(), h("div.page-inner",
    h("div.title-block",
      h("h1", "Delivery"),
      h("p.hint", "Where each pod has got to, and who is told when an edit goes live.")),
    fleetBox,
    hooksBox,
    historyBox,
  ));
  await load();
  leaveWebhooks();
  timer = setInterval(load, 5000);
}

// ------------------------------------------------------------------ fleet

function renderFleet(box, fleet) {
  if (fleet.error) return clear(box, h("p.notice.error", errorText(fleet.error)));
  const pods = fleet.pods || [];
  const waiting = fleet.waiting_for || [];
  const summary = fleet.data_source !== "db"
    ? h("p.notice", "This feed is served from the nightly preprocessed build, so there is no live version for the pods to follow.")
    : !pods.length
      ? h("p.notice", "No pod is reporting. Either this build does not have webhooks turned on, or its migration has not been applied yet.")
      : fleet.in_sync
        ? h("p.notice.ok", `All ${plural(fleet.live_pods, "pod")} are serving version ${fleet.version}. Anything that caches these answers can be rebuilt now.`)
        : h("p.notice.warning", `${plural(waiting.length, "pod")} still serving older data: ${waiting.map((p) => `${p.pod_id} (v${p.loaded_version})`).join(", ")}.`);

  clear(box, h("section.section",
    h("h2", "Pods"),
    h("p.hint",
      `Committed version ${fleet.version ?? "?"}`,
      fleet.version_at ? `, ${fmtDate(fleet.version_at)}` : "",
      fleet.stale_pods ? ` · ${plural(fleet.stale_pods, "pod")} not heard from in ${fleet.stale_after_seconds}s, not counted` : ""),
    summary,
    pods.length ? h("div.table-wrap", h("table",
      h("thead", h("tr", h("th", "Pod"), h("th", "Serving"), h("th", "Since"), h("th", "Last heard from"), h("th", "Image"), h("th", ""))),
      h("tbody", pods.map((p) => h("tr",
        h("td", h("strong", p.pod_id)),
        h("td", p.data_source === "db" ? `v${p.loaded_version}` : h("span.hint", "preprocessed build")),
        h("td", fmtDate(p.loaded_at)),
        h("td", fmtDate(p.last_seen_at)),
        h("td", p.image_tag ? h("code", p.image_tag) : h("span.hint", "—")),
        h("td", p.last_error
          ? h("span.chip.error", { title: p.last_error }, `Failed on v${p.failing_version ?? "?"}`)
          : p.up_to_date ? h("span.chip.ok", "Up to date")
          : p.data_source === "db" ? h("span.chip.warn", "Behind") : ""))))))
      : null));
}

// ------------------------------------------------------------------ webhooks

function renderHooks(box, data, reload) {
  if (data.error) return clear(box, h("p.notice.error", errorText(data.error)));
  const policy = data.policy || {};
  const items = data.items || [];
  const admin = can("admin");

  const head = h("div.title-block",
    h("h2", "Webhooks"),
    h("p.hint", "A URL GIMS calls when something happens to this feed."));

  const policyNote = !policy.enabled
    ? h("p.notice", "This build has webhooks turned off, so nothing is sent. An engineer turns them on in the deployment config.")
    : !policy.allowed_hosts?.length
      ? h("p.notice.warning", "This build allows no webhook hosts, so nothing can be sent. An engineer sets the allowed hosts in the deployment config.")
      : h("p.hint", `A URL must point at: ${policy.allowed_hosts.join(", ")}. An engineer changes that list in the deployment config.`);

  clear(box, h("section.section",
    head,
    policyNote,
    items.length ? h("div.table-wrap", h("table",
      h("thead", h("tr", h("th", "Name"), h("th", "When"), h("th", "Calls"), h("th", "Status"), h("th", ""))),
      h("tbody", items.map((w) => h("tr",
        h("td", h("strong", w.name), h("div.hint", `Waits ${w.settle_seconds}s after the last pod, gives up after ${Math.round(w.give_up_after_seconds / 60)} min`)),
        h("td", EVENT_LABEL[w.event] || w.event),
        h("td", h("code.break", `${w.method} ${w.url}`)),
        h("td", w.enabled ? h("span.chip.ok", "On") : h("span.chip", "Off")),
        h("td.row-actions", admin ? [
          h("button.btn.quiet.small", { type: "button", on: { click: () => hookForm(w, reload) } }, "Edit"),
          h("button.btn.quiet.small", { type: "button", on: { click: () => testHook(w, reload) } }, "Test"),
          h("button.btn.quiet.small.danger", { type: "button", on: { click: () => removeHook(w, reload) } }, "Delete"),
        ] : h("span.hint", "Admins only")))))))
      : h("p.empty", "No webhook yet."),
    admin ? h("div.row-actions",
      h("button.btn.small", { type: "button", on: { click: () => hookForm(null, reload) } }, "Add a webhook")) : null));
}

async function testHook(w, reload) {
  try {
    await post(`webhooks/${enc(w.webhook_id)}/test`);
    toast(`Queued a test call to "${w.name}". A pod sends it within a few seconds; watch the history below.`);
  } catch (e) {
    toast(errorText(e), "error");
  }
  reload();
}

async function removeHook(w, reload) {
  const ok = await confirmDialog(`Delete "${w.name}"?`,
    "GIMS will stop calling it. Nothing else about the feed changes.",
    { confirm: "Delete", danger: true });
  if (!ok) return;
  try {
    await del(`webhooks/${enc(w.webhook_id)}`);
    toast(`Deleted "${w.name}".`);
  } catch (e) {
    toast(errorText(e), "error");
  }
  reload();
}

// One form for both new and existing: a PATCH sends only what is on it, and a
// POST needs all of it, so the same fields serve.
function hookForm(existing, reload) {
  const w = existing || {};
  const name = h("input", { type: "text", value: w.name || "", placeholder: "frontline rebuild", required: true });
  const event = h("select", Object.keys(EVENT_LABEL).map((e) =>
    h("option", { value: e, selected: (w.event || "feed_in_sync") === e }, EVENT_LABEL[e])));
  const method = h("select", ["POST", "PUT", "GET"].map((m) =>
    h("option", { value: m, selected: (w.method || "POST") === m }, m)));
  const url = h("input", {
    type: "text", value: w.url || "", required: true,
    placeholder: "https://jenkins.example/job/rebuild/buildWithParameters?token=${JENKINS_TOKEN}",
  });
  const headers = h("textarea", { rows: "3", placeholder: '{"Authorization": "Bearer ${JENKINS_TOKEN}"}' },
    w.headers && Object.keys(w.headers).length ? JSON.stringify(w.headers, null, 2) : "");
  const body = h("textarea", { rows: "3", placeholder: "Leave empty to send the built-in payload" },
    w.body ? JSON.stringify(w.body, null, 2) : "");
  const settle = h("input", { type: "number", min: "0", max: "3600", value: w.settle_seconds ?? 30 });
  const stale = h("input", { type: "number", min: "10", max: "3600", value: w.stale_after_seconds ?? 60 });
  const enabled = h("input", { type: "checkbox", checked: w.enabled !== false });
  const help = h("p.hint");
  const error = h("p.notice.error", { role: "alert", hidden: true });

  const showHelp = () => { help.textContent = EVENT_HELP[event.value] || ""; };
  event.addEventListener("change", showHelp);
  showHelp();

  const parseJson = (el, what) => {
    const text = el.value.trim();
    if (!text) return what === "headers" ? {} : null;
    try {
      const v = JSON.parse(text);
      if (typeof v !== "object" || v === null || Array.isArray(v)) throw new Error();
      return v;
    } catch {
      throw new Error(`${what} must be a JSON object, for example {"Name": "value"}.`);
    }
  };

  modal(existing ? `Edit "${w.name}"` : "Add a webhook", () => h("div.section",
    h("label.field", h("span", "Name"), name),
    h("label.field", h("span", "When to call"), event),
    help,
    h("label.field", h("span", "Method"), method),
    h("label.field", h("span", "URL"), url),
    h("p.hint",
      "Put a credential in as ", h("code", "${JENKINS_TOKEN}"),
      " and set that variable on the GIMS pods; it is never stored here. ",
      h("code", "${event:gtfs_id}"), " and ", h("code", "${event:feed_version}"), " are filled in too."),
    h("label.field", h("span", "Headers (JSON, optional)"), headers),
    h("label.field", h("span", "Body (JSON, optional)"), body),
    h("div.field-row",
      h("label.field", h("span", "Wait after the last pod (seconds)"), settle),
      h("label.field", h("span", "Treat a pod as gone after (seconds)"), stale)),
    h("label.field", h("span", "On"), enabled),
    error,
  ), {
    actions: [
      (close) => h("button.btn.secondary", { type: "button", on: { click: () => close() } }, "Cancel"),
      (close) => h("button.btn", { type: "button", on: { click: async () => {
        error.hidden = true;
        let payload;
        try {
          payload = {
            name: name.value.trim(),
            event: event.value,
            url: url.value.trim(),
            method: method.value,
            headers: parseJson(headers, "headers"),
            body: parseJson(body, "body"),
            enabled: enabled.checked,
            settle_seconds: Number(settle.value),
            stale_after_seconds: Number(stale.value),
          };
        } catch (e) {
          error.textContent = e.message;
          error.hidden = false;
          return;
        }
        if (!payload.name || !payload.url) {
          error.textContent = "A name and a URL are needed.";
          error.hidden = false;
          return;
        }
        try {
          if (existing) await patch(`webhooks/${enc(w.webhook_id)}`, payload);
          else await post(`feeds/${enc(state.feedId)}/webhooks`, payload);
          toast(existing ? `Saved "${payload.name}".` : `Added "${payload.name}".`);
          close();
          reload();
        } catch (e) {
          error.textContent = errorText(e);
          error.hidden = false;
        }
      } } }, existing ? "Save" : "Add"),
    ],
  });
}

// ------------------------------------------------------------------ history

function renderHistory(box, data) {
  if (data.error) return clear(box, h("p.notice.error", errorText(data.error)));
  const items = data.items || [];
  clear(box, h("section.section",
    h("h2", "Recent calls"),
    items.length ? h("div.table-wrap", h("table",
      h("thead", h("tr", h("th", "When"), h("th", "Webhook"), h("th", "Version"), h("th", "Result"), h("th", "Detail"))),
      h("tbody", items.map((d) => h("tr",
        h("td", fmtDate(d.created_at)),
        h("td", d.webhook || h("span.hint", "deleted"), d.kind === "test" ? h("span.chip", " test") : null),
        h("td", `v${d.feed_version}`),
        h("td", h("span", { class: `chip ${chipFor(d.status)}` }, DELIVERY_LABEL[d.status] || d.status),
          d.attempts > 1 ? h("div.hint", `${plural(d.attempts, "try", "tries")}`) : null),
        h("td",
          d.response_status ? h("div", `Answered ${d.response_status}`) : null,
          d.last_error ? h("div.hint.break", d.last_error) : null,
          d.pod_count ? h("div.hint", `${plural(d.pod_count, "pod")} in sync`) : null))))))
      : h("p.empty", "Nothing sent yet.")));
}

function chipFor(status) {
  if (status === "succeeded") return "ok";
  if (status === "failed" || status === "abandoned") return "error";
  return "warn";
}
