// Stages that may be the same place as a stage, and the two ways to settle it
// (docs/gtfs-editor.md section 19.1): merge one into this stage, or replace
// this stage with one. Replacing is how a stage that is wrong altogether is
// put right - every route using it, temporary routes too, runs the other stage
// instead, and this one goes away. Both are a stage/merge in the draft; they
// differ only in which stage stays.
//
// Offered: the other stages of its name, stages named nearly alike (ISLAND
// GROUND and ISLAND GROUND B.T, Parrys Corner and PARRYS CORNER), any stage
// starting a couple of hundred metres away whatever it is called, the stages
// the open draft makes, and whatever the search finds. Only the same
// direction: the server refuses a merge across directions. The other way's
// stage, and a stage being made in somebody else's draft, are mentioned and
// never offered.
//
// Nothing is merged without first asking the server what it would do: which
// stops the routes stop and start calling at (these are fare stages), how far
// apart the two start, and whether the draft would refuse it.
import { get, post, enc } from "./api.js";
import { state } from "./state.js";
import { h, clear, toast, modal, debounce, fmtMetres, plural, STATUS_LABEL } from "./util.js";
import { addChange, removeChange, createdChange, createdStages, requireDraft, sameStageKey, stageChanges, stageKey } from "./drafts.js";
import { idKind } from "./stage_reviews.js";
import { editRouteStages } from "./stages.js";

const WHY = {
  same_name: ["same name", "Carries this stage's name, perhaps with case, spaces or dots written differently."],
  like_name: ["similar name", "Named nearly alike and starts close by: the same place written another way, or a different place sharing a word."],
  nearby: ["starts nearby", "Starts within a couple of hundred metres of this stage, whatever it is called."],
};

export const routeLabel = (r) => (r.short_name && r.short_name !== r.route_id ? `${r.short_name} (${r.route_id})` : r.route_id);
const sameWay = (a, b) => (a.direction || "") === (b.direction || "");
const idOf = (key) => String(key || "").split("|")[0];

/// The draft's merges about stage `key`: the ones merging other stages into
/// it, and the one replacing it, if there is one.
export function draftMerges(key) {
  const merges = state.draft
    ? state.draft.changes.filter((c) => c.entity === "stage" && c.op === "merge" && c.after)
    : [];
  return {
    into: merges.filter((c) => sameStageKey(c.after.into_stage_id || "", key)),
    replacedBy: merges.find((c) => sameStageKey(c.entity_key, key)) || null,
  };
}

/// Where the draft's replacements of `key` end: replaced by B, and B in turn by
/// C, is C. `key` itself when the draft does not replace it.
export function finalStage(key) {
  let k = key;
  for (let i = 0; i < 20; i += 1) {
    const next = draftMerges(k).replacedBy;
    if (!next) return k;
    k = next.after.into_stage_id;
  }
  return k;
}

/// Whether the open draft changes stage `id` (either direction): edits it,
/// merges it or into it, or takes routes off it. For the queues' "in your
/// draft" chips.
export function draftTouchesStage(id) {
  if (!state.draft) return false;
  return state.draft.changes.some((c) => c.entity === "stage"
    && [c.entity_key, c.after && c.after.into_stage_id, c.after && c.after.from_stage_id].some((k) => k && idOf(k) === id));
}

/// Name and counts of a stage by its key, from the draft or the feed: for a
/// replacement the lists here did not bring.
export async function stageBrief(key) {
  const made = createdStages().find((s) => sameStageKey(s.stage_key, key));
  if (made) return made;
  try {
    return await get(`feeds/${enc(state.feedId)}/stages/${enc(key)}`);
  } catch {
    return { stage_id: idOf(key), stage_key: key, name: idOf(key) };
  }
}

/// Take a change back out of the draft, after asking. True when it went.
export async function undoChange(change, what) {
  const ok = await modal("Take it out of the draft?", () => h("p",
    `${what} comes out of draft “${state.draft.title}”. Nothing else in the draft changes.`), {
    actions: [
      (close) => h("button.btn.secondary", { type: "button", on: { click: () => close(false) } }, "Cancel"),
      (close) => h("button.btn", { type: "button", on: { click: () => close(true) } }, "Take it out"),
    ],
  });
  if (!ok) return false;
  try {
    await removeChange(change.change_id);
    toast("Taken out of the draft.", "ok");
    return true;
  } catch (e) {
    toast(e.message, "error");
    return false;
  }
}

// A merge the server refused stays in the draft with an error and stops it
// being submitted, so it is taken straight back out and the reason shown.
// True when it went in.
export async function addMerge(gone, into) {
  const res = await addChange({
    entity: "stage", op: "merge", entity_key: gone, after: { into_stage_id: into },
  }, { merge: false, quiet: true });
  if (!res) return false;
  const bad = (res.problems || []).find((p) => p.level === "error");
  if (bad) {
    try { await removeChange(res.changeId); } catch { /* the toast says what went wrong */ }
    toast(`${bad.message} Nothing was added to the draft.`, "error");
    return false;
  }
  return true;
}

/// Open the stage list of route `routeId` (its normal list), with `swap` -
/// {from, to}: stage keys - already made, for the person to check and add.
export async function openRouteStages(routeId, swap = null) {
  try {
    const route = await get(`feeds/${enc(state.feedId)}/routes/${enc(routeId)}`);
    await editRouteStages(route, { swap });
  } catch (e) {
    toast(e.message, "error");
  }
}

// Each route a button opening its stages: `close` shuts a dialog first.
const routeButtons = (routes, close = null) => h("div.btn-row", routes.map((r) => h("button.btn.secondary.small", {
  type: "button",
  on: { click: () => { if (close) close(false); openRouteStages(r.route_id); } },
}, `Change the stages of ${routeLabel(r)}`)));

const stopNames = (list) => h("ul.compact-list", list.slice(0, 12).map((s) => h("li", s.name, h("span.ids", s.stop_id))),
  list.length > 12 ? h("li.hint", `and ${list.length - 12} more`) : null);

/// Merge stage `gone` into `into`, after asking the server what it would do and
/// showing the person: the routes that change, the stops they stop and start
/// calling at, how far apart the two stages start, and the open reviews it
/// touches. A refusal (a route running both, a route edited outside its
/// stages) is shown before anything is drafted. `mode` "replace" puts it as
/// `gone` being wrong; "merge" as `gone` being the same place as `into`.
///
/// A stage the draft already replaces is replaced by where that ends: merging
/// into B when the draft replaces B with C merges into C. True when it went in.
export async function confirmMerge({ gone, into, mode = "replace" }) {
  if (!(await requireDraft(`A ${mode === "replace" ? "replacement" : "merge"} goes into a draft. Nothing changes for passengers until someone else approves it and it is committed.`))) return false;
  const target = finalStage(into);
  if (sameStageKey(target, gone)) {
    toast(`Your draft already replaces ${idOf(into)} with ${idOf(gone)}, so it cannot go the other way as well. Undo that first.`, "error");
    return false;
  }
  if (draftMerges(gone).replacedBy) {
    toast(`Your draft already replaces ${idOf(gone)} with ${idOf(finalStage(gone))}. Undo that first.`, "error");
    return false;
  }
  let m;
  try {
    m = await get(`change-sets/${enc(state.draft.change_set_id)}/preview/stages/${enc(gone)}/merge?into=${enc(target)}`);
  } catch (e) {
    toast(e.message, "error");
    return false;
  }
  const errors = (m.problems || []).filter((p) => p.level === "error");
  const a = m.stage;
  const b = m.into;
  const goneName = `${a.name} (stage ${a.stage_id})`;
  const intoName = `${b.name} (stage ${b.stage_id})`;
  const routes = m.routes || [];
  const temporary = routes.filter((r) => (r.temporary || []).length).length;
  const reviews = (m.reviews && m.reviews.stage_reviews) || [];
  const issues = (m.reviews && m.reviews.route_issues) || [];
  const title = errors.length
    ? (mode === "replace" ? `${a.name} cannot be replaced with ${b.name} yet` : `${a.name} cannot be merged into ${b.name} yet`)
    : (mode === "replace" ? `Replace ${a.name} with ${b.name}?` : `Merge ${a.name} into ${b.name}?`);
  const word = mode === "replace" ? "Replace it" : "Merge it";
  const ok = await modal(title, (close) => h("div.merge-check",
    target !== into ? h("p.notice", `Your draft already replaces ${idOf(into)} with ${intoName}, so ${a.name} goes there too.`) : null,
    errors.length ? [
      h("div.notice.error", errors.map((p) => h("p", p.message))),
      (m.shared_routes || []).length ? [
        h("p", `A route runs a stage once, so ${plural(m.shared_routes.length, "route")} running both must come off one of them first. `
          + "Open its stages, remove the one it should not run, and add that to your draft; then come back here."),
        routeButtons(m.shared_routes, close),
      ] : null,
    ] : [
      m.far ? h("div.notice.warning", h("p", h("strong", `These start ${fmtMetres(m.distance_m)} apart.`),
        " One name is often several places in the city — a bus stand, a church, a post office in every area. "
        + `Make sure ${b.name} is this place before every route is moved to it.`)) : null,
      h("p", routes.length
        ? [`${plural(routes.length, "route")} using ${goneName} — `, h("strong", routes.map(routeLabel).join(", ")),
            temporary ? `, ${plural(temporary, "temporary route")} among them` : "",
            ` — will run ${intoName} instead`]
        : `No route uses ${goneName}`,
        `, and ${a.name} goes away. It goes into your draft; nothing changes for passengers until the draft is approved and committed.`),
      m.distance_m != null && !m.far ? h("p.hint", m.distance_m < 10 ? "They start at the same place." : `They start ${fmtMetres(m.distance_m)} apart.`) : null,
      routes.length && (m.stops_lost || []).length ? [
        h("p", h("strong", `${routes.length === 1 ? "It stops" : "They stop"} calling at ${plural(m.stops_lost.length, "stop")}`), ` that ${b.name} does not have:`),
        stopNames(m.stops_lost)] : null,
      routes.length && (m.stops_gained || []).length ? [
        h("p", h("strong", `${routes.length === 1 ? "It starts" : "They start"} calling at ${plural(m.stops_gained.length, "stop")}`), ` that ${a.name} does not have:`),
        stopNames(m.stops_gained)] : null,
      routes.length && !(m.stops_lost || []).length && !(m.stops_gained || []).length
        ? h("p.hint", `Both have the same ${plural(m.stops_kept, "stop")}: the routes call where they did.`) : null,
      routes.length && ((m.stops_lost || []).length || (m.stops_gained || []).length)
        ? h("p.hint", "These are fare stages: where a stage begins and ends sets the fare, so a stop lost or gained changes fares from it.") : null,
      reviews.length || issues.length ? h("p.hint", "Open reviews this touches: ",
        [...reviews.map((r) => `stage ${r.stage_id}${r.direction ? ` ${r.direction}` : ""}`),
          ...issues.map((i) => `route ${routeLabel(i)}`)].join(", "),
        ". Mark them fixed with the same draft once you are done.") : null,
    ]), {
    actions: errors.length
      ? [(close) => h("button.btn", { type: "button", on: { click: () => close(false) } }, "Close")]
      : [
          (close) => h("button.btn.secondary", { type: "button", on: { click: () => close(false) } }, "Cancel"),
          (close) => h("button.btn", { type: "button", class: m.far ? "danger" : "", on: { click: () => close(true) } },
            m.far ? `${word} anyway` : word),
        ],
  });
  if (!ok) return false;
  try {
    if (await addMerge(gone, target)) {
      toast(mode === "replace"
        ? `${a.name} is replaced by ${b.name} in your draft “${state.draft.title}”.`
        : `${a.name} is merged into ${b.name} in your draft “${state.draft.title}”.`, "ok");
      return true;
    }
  } catch (e) {
    toast(e.message, "error");
  }
  return false;
}

/// Use stage `s` instead of `stage` on some of its routes only: each route
/// opens its stage list with the one swapped for the other, to check and add.
async function onSomeRoutes(stage, s) {
  const seen = new Map();
  for (const r of stage.routes || []) if (!r.variant_id && !seen.has(r.route_id)) seen.set(r.route_id, r);
  const routes = [...seen.values()];
  await modal(`Use ${s.name} on some routes only`, (close) => h("div",
    h("p", `${stage.name} is right for most of its routes and wrong for some? Change it on those routes only: `
      + `each opens its stage list with ${stage.name} swapped for ${s.name} (stage ${s.stage_id}), for you to check and add to your draft. `
      + `${stage.name} stays as it is for every other route.`),
    routes.length
      ? h("ul.compact-list", routes.map((r) => h("li", routeLabel(r),
          h("button.btn.secondary.small", {
            type: "button",
            on: { click: () => { close(); openRouteStages(r.route_id, { from: stageKey(stage), to: s }); } },
          }, "Swap it on this route"))))
      : h("p.empty", "No route runs this stage on its normal list."),
    h("p.hint", "A temporary route is changed from its route's page, under Temporary routes.")), {
    actions: [(close) => h("button.btn.secondary", { type: "button", on: { click: () => close() } }, "Close")],
  });
}

/// `stage`: {stage_id, stage_key, name, direction, route_count, routes}.
/// `editable` offers merging and replacing; `onChange` runs after either goes
/// into or comes out of the draft, so the page can draw itself again.
export function stagesLike(stage, { editable = false, onChange = () => {} } = {}) {
  const key = stageKey(stage);
  const list = h("ul.stop-choices", h("li", h("p.empty", "Looking for stages like this one…")));
  const elsewhere = h("div");
  const status = h("p.hint", { "aria-live": "polite" });
  const search = h("input", {
    type: "search", id: "stages-like-search", autocomplete: "off", spellcheck: "false",
    placeholder: "Stage name or stage id",
  });
  let similar = [];
  let otherWay = [];
  let inOtherDrafts = [];
  let found = [];
  let extra = [];
  let failed = null;
  let busy = false;

  // every stage on offer, once each: the draft's own first, then the ones the
  // server found alike, then the search's. A stage merged in or put in this
  // one's place by the draft is kept on the list so it can be undone.
  const rows = () => {
    const out = [];
    const add = (s) => {
      if (!s || !s.stage_key || sameStageKey(s.stage_key, key) || !sameWay(s, stage)) return;
      if (out.some((o) => sameStageKey(o.stage_key, s.stage_key))) return;
      out.push(s);
    };
    createdStages().forEach(add);
    similar.forEach(add);
    for (const c of draftMerges(key).into) {
      const b = c.before || {};
      add({
        stage_id: idOf(c.entity_key), stage_key: c.entity_key, direction: b.direction ?? stage.direction,
        name: b.name || c.entity_key, stop_count: b.stop_count, route_count: b.route_count,
      });
    }
    extra.forEach(add);
    found.forEach(add);
    return out;
  };

  const act = async (fn) => {
    if (busy) return;
    busy = true;
    draw();
    let changed = false;
    try {
      changed = await fn();
    } finally {
      busy = false;
    }
    if (changed) onChange(); else draw();
  };

  const draw = () => {
    const { into, replacedBy } = draftMerges(key);
    const all = rows();
    if (!all.length) {
      clear(list, h("li", h("p.empty", failed
        ? `Could not look for stages like this one (${failed}). Search for one below.`
        : "No stage going the same way has this name, a name like it, or starts nearby. Search for one below.")));
    } else {
      clear(list, all.map((s) => {
        const mergedIn = into.find((c) => sameStageKey(c.entity_key, s.stage_key));
        const replacing = replacedBy && sameStageKey(replacedBy.after.into_stage_id || "", s.stage_key);
        // a stage the draft replaces in turn is offered as where that ends
        const goesOn = !replacing && draftMerges(s.stage_key).replacedBy;
        const shared = s.shared_routes || [];
        const why = WHY[s.why];
        const free = editable && !replacedBy && !mergedIn && !shared.length && !busy;
        return h("li.stop-choice", { class: replacing ? "stop-choice is-current" : "stop-choice" },
          h("div.stop-choice-head",
            h("strong", s.name),
            h("span.hint", `stage ${s.stage_id}`),
            idKind(s.stage_id),
            s.stop_count != null ? h("span.hint", plural(s.stop_count, "stop")) : null,
            s.route_count != null ? h("span.hint", plural(s.route_count, "route")) : null,
            why ? h("span.chip", { title: why[1] }, why[0]) : null,
            s.far ? h("span.chip.warn", { title: "Starts far enough away to be another place of this name." }, `${fmtMetres(s.distance_m)} away`) : null,
            s.draft ? h("span.chip.draft", "new in your draft") : null,
            s.review ? h("span.chip.warn", "to review") : null,
            mergedIn ? h("span.chip.draft", "merged into this one in your draft") : null,
            replacing ? h("span.chip.draft", "replaces this one in your draft") : null,
            goesOn ? h("span.chip.draft", `replaced by ${idOf(finalStage(s.stage_key))} in your draft`) : null),
          s.first_stop && s.first_stop.name
            ? h("p.hint", `Starts at ${s.first_stop.name}`,
                s.distance_m != null ? (s.distance_m < 10 ? ", the same place as this one" : `, ${fmtMetres(s.distance_m)} from where this one starts`) : "")
            : null,
          shared.length ? [
            h("p.hint", h("strong", `${shared.map(routeLabel).join(", ")} ${shared.length === 1 ? "runs" : "run"} both`),
              `, so neither can be merged into the other until ${shared.length === 1 ? "that route is" : "those routes are"} off one of them.`),
            editable ? routeButtons(shared) : null,
          ] : null,
          h("div.btn-row",
            free && !s.draft && !goesOn ? h("button.btn.secondary.small", {
              type: "button",
              on: { click: () => act(() => confirmMerge({ gone: s.stage_key, into: key, mode: "merge" })) },
            }, "Merge it into this one") : null,
            free ? h("button.btn.secondary.small", {
              type: "button",
              on: { click: () => act(() => confirmMerge({ gone: key, into: s.stage_key, mode: "replace" })) },
            }, "Use it instead of this one") : null,
            free && (stage.routes || []).length ? h("button.btn.quiet.small", {
              type: "button", on: { click: () => onSomeRoutes(stage, s) },
            }, "Only on some routes…") : null,
            mergedIn && state.draft ? h("button.btn.quiet.small", {
              type: "button",
              on: { click: () => act(() => undoChange(mergedIn, `Merging ${s.name} into ${stage.name}`)) },
            }, "Undo the merge") : null,
            replacing && state.draft ? h("button.btn.quiet.small", {
              type: "button",
              on: { click: () => act(() => undoChange(replacedBy, `Replacing ${stage.name} with ${s.name}`)) },
            }, "Undo the replacement") : null,
            h("a.btn.quiet.small", { href: `#/stage/${enc(s.stage_key)}` }, "Open")));
      }));
    }

    // only mentioned: the other way's stage of this place, and stages being
    // made in other people's drafts
    const mine = state.draft ? state.draft.change_set_id : null;
    const others = inOtherDrafts.filter((d) => d.change_set_id !== mine);
    const way = stage.direction || "either way";
    clear(elsewhere,
      otherWay.length ? h("div.notice",
        h("p", h("strong", "Going the other way: "), otherWay.map((s, i) => [i ? ", " : "",
          h("a", { href: `#/stage/${enc(s.stage_key)}` }, `${s.name} (stage ${s.stage_id}, ${s.direction || "either way"})`)])),
        h("p.hint", "Never merged with this one: the two directions hold the opposite kerbs. "
          + `If this stage has the other way's stops, edit its stops to the kerbs going ${way}. `
          + "If a route runs this stage but goes the other way, change that route's stages to the other way's stage.")) : null,
      others.length ? h("div.notice",
        h("p", h("strong", "Being made in other drafts:")),
        h("ul.compact-list", others.map((d) => h("li",
          h("strong", d.name), " ",
          h("span.chip.draft", STATUS_LABEL[d.change_set_status] || d.change_set_status), " in ",
          h("a", { href: `#/drafts/${enc(d.change_set_id)}` }, `“${d.change_set_title}”`),
          d.author_email ? ` by ${d.author_email}` : "",
          d.distance_m != null ? h("span.hint", ` · starts ${fmtMetres(d.distance_m)} from this one`) : null))),
        h("p.hint", "Not live yet, so nothing can be merged into one or replaced by it until that draft is committed. "
          + "If one is the stage you were about to make, wait for it rather than making it twice.")) : null);
  };

  let seq = 0;
  search.addEventListener("input", debounce(async () => {
    const q = search.value.trim();
    const mine = ++seq;
    if (q.length < 2) {
      found = [];
      status.textContent = "";
      return draw();
    }
    try {
      const params = new URLSearchParams({ q, limit: "20" });
      if (stage.direction) params.set("direction", stage.direction);
      const page = await get(`feeds/${enc(state.feedId)}/stages?${params}`);
      if (mine !== seq) return;
      found = page.items.filter((s) => sameWay(s, stage));
      const drafts = createdStages().filter((s) => sameWay(s, stage)
        && ((s.name || "").toLowerCase().includes(q.toLowerCase()) || s.stage_id === q));
      found = [...drafts, ...found];
      status.textContent = found.length
        ? `${plural(found.length, "stage")} going the same way match “${q}”, listed above.`
        : `No stage going the same way matches “${q}”. A stage being made in somebody else's draft is not listed until it is live.`;
      draw();
    } catch (e) {
      status.textContent = e.message;
    }
  }, 250));

  (async () => {
    // a stage the draft makes is only there with the draft applied
    const made = !!createdChange("stage", key);
    try {
      const res = made && state.draft
        ? await get(`change-sets/${enc(state.draft.change_set_id)}/preview/stages/${enc(key)}/similar`)
        : await get(`feeds/${enc(state.feedId)}/stages/${enc(key)}/similar`);
      similar = res.items || [];
      otherWay = res.other_way || [];
      inOtherDrafts = res.in_other_drafts || [];
    } catch (e) {
      failed = e.message;
    }
    // the stage replacing this one, when neither list brought it
    const rep = draftMerges(key).replacedBy;
    if (rep && !rows().some((s) => sameStageKey(s.stage_key, rep.after.into_stage_id || ""))) {
      extra = [await stageBrief(rep.after.into_stage_id)];
    }
    draw();
  })();

  const el = h("div.stages-like",
    list,
    elsewhere,
    editable ? h("label.field", { for: "stages-like-search" }, h("span", "Find another stage going the same way"), search) : null,
    editable ? status : null);
  return { el, redraw: draw };
}

/// The open reviews a draft's merges settle or touch, with a box each, for the
/// page closing one review to close them too with the same draft and note.
/// `gone`: stage keys the draft merges away, whose own reviews are settled and
/// ticked; `kept`: stages that take their routes, whose reviews are not. The
/// routes to review of every route that ran a gone stage are listed unticked:
/// only somebody who has looked at a route's whole list can say it is right.
/// `ways`: the other open reviews of the same stage ({review_id, name,
/// stage_id, direction}) - its other direction - listed unticked.
/// `except`: {review_id, issue_id} of the review being closed.
export function alsoClose({ gone = [], kept = [], ways = [], except = {} }) {
  const box = h("fieldset.also-close", { hidden: true });
  const picks = [];
  const about = (keys) => (keys.length
    ? get(`feeds/${enc(state.feedId)}/open-reviews?stages=${keys.map(enc).join(",")}`)
    : Promise.resolve({ stage_reviews: [], route_issues: [] }));
  if (gone.length || ways.length) {
    // the kept stages' own reviews, but not their routes': those routes run
    // the kept stage before and after, and nothing changes for them
    Promise.all([about(gone), about(gone.length ? kept : [])]).then(([g, k]) => {
      const seen = new Set();
      const reviews = [...(g.stage_reviews || []), ...(k.stage_reviews || [])]
        .filter((r) => r.review_id !== except.review_id && !seen.has(r.review_id) && seen.add(r.review_id));
      const others = ways.filter((w) => w.review_id !== except.review_id && !seen.has(w.review_id) && seen.add(w.review_id));
      const issues = (g.route_issues || []).filter((i) => i.issue_id !== except.issue_id);
      if (!reviews.length && !others.length && !issues.length) return;
      const check = (id, checked, label, hint, kind) => {
        const input = h("input", { type: "checkbox", id, checked });
        picks.push({ input, kind, id: Number(id.split("-").pop()) });
        return h("label.check", { for: id }, input, h("span", label, hint ? h("span.hint", ` — ${hint}`) : null));
      };
      clear(box,
        h("legend", "Also mark these fixed, with the same draft and note"),
        reviews.map((r) => {
          const settled = gone.some((k) => sameStageKey(k, r.stage_key));
          return check(`also-review-${r.review_id}`, settled, `Stage ${r.name} (${r.stage_id}${r.direction ? `, ${r.direction}` : ""})`,
            settled ? "its stage is merged away in your draft" : "the stage that takes the routes; tick it only if that settles it too", "review");
        }),
        others.map((w) => check(`also-review-${w.review_id}`, false, `Stage ${w.name} (${w.stage_id}, ${w.direction || "either way"})`,
          "the other way of this stage; tick it only if the draft fixed that way too", "review")),
        issues.map((i) => check(`also-issue-${i.issue_id}`, false, `Route to review ${routeLabel(i)}`,
          `ran a stage your draft replaces; tick it only once the route's whole list is right`, "issue")));
      box.hidden = false;
    }).catch(() => {});
  }
  return {
    el: box,
    /// Close every ticked one with `body` ({decision, note, change_set}).
    /// Resolves to how many closed, and the messages of any that did not.
    async closeAll(body) {
      let closed = 0;
      const failed = [];
      for (const p of picks.filter((x) => x.input.checked)) {
        try {
          await post(p.kind === "review" ? `stage-reviews/${p.id}/close` : `route-issues/${p.id}/close`, body);
          closed += 1;
        } catch (e) {
          failed.push(e.message);
        }
      }
      return { closed, failed };
    },
  };
}

/// What a review closed as fixed says about the draft it named: thrown away
/// (discarded or rejected - the fix never went live), still on its way, or
/// live. Null when it names none.
export function fixedByNotice(r, { onReopen = null } = {}) {
  if (r.status !== "fixed" || !r.change_set_id) return null;
  const link = h("a", { href: `#/drafts/${enc(r.change_set_id)}` }, `“${r.change_set_title || "untitled"}”`);
  const s = r.change_set_status;
  if (s === "discarded" || s === "rejected") {
    return h("div.notice.error",
      h("p", h("strong", `Marked fixed with draft `), link, h("strong", `, which was ${s === "rejected" ? "rejected" : "discarded"}.`),
        " The fix never went live, so this is not fixed. Put it back in the queue and fix it again."),
      onReopen ? h("div.btn-row", h("button.btn.small", { type: "button", on: { click: onReopen } }, "Put it back in the queue")) : null);
  }
  if (s === "committed") return h("p.notice.ok", "Fixed by draft ", link, ", which is live.");
  return h("p.notice", "Fixed by draft ", link, `, which is ${(STATUS_LABEL[s] || s || "").toLowerCase()}: the fix is live once it is approved and committed.`);
}
