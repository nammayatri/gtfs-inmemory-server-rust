//! Copying routes (docs/gtfs-editor.md section 34): a route cloned into a new
//! route in a draft, routes exported as the CSV files the bulk import reads
//! (section 5) - so that an export, edited or not, imports back into the same
//! feed or another - and the routes list these start from, as a draft makes
//! the feed.
//!
//! Each reads the feed as the draft it names makes it, the way a route preview
//! does: the draft applied inside the transaction, read, rolled back - on the
//! feed's lock, like every other replay.

use super::auth::{self, Ctx};
use super::bulk;
use super::error::{EditorError, EditorResult};
use super::feed_lock::{lock_feed, lock_feed_of_set, retry_transient};
use super::records::{self, ROUTE_GTFS_FIELDS};
use super::service::{self, ChangeSet, NewChange, Page, FIRST_PATTERN};
use super::stages;
use super::trips::{self, StoredProfile, PROFILE_SOURCES};
use super::validation::{check_entity_id, decode_polyline, RouteRow, StageLink, ROUTE_CORRECTION};
use super::EditorState;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use uuid::Uuid;

/// Where a map line came from, as `route/update` takes it.
const POLYLINE_SOURCES: [&str; 4] = ["osrm", "gps", "manual", "imported"];

// ---------------------------------------------------------------- the draft

fn feed_mismatch(draft: &str, g: &str) -> EditorError {
    EditorError::bad_request(
        "feed_mismatch",
        format!("the draft is for feed {draft}, not {g}"),
    )
}

/// Draft `id`, which must be a change set of feed `g`.
async fn draft_of(
    conn: &mut PgConnection,
    id: Uuid,
    g: &str,
    lock: bool,
) -> EditorResult<ChangeSet> {
    let set = service::load_set(conn, id, lock).await?;
    if set.gtfs_id != g {
        return Err(feed_mismatch(&set.gtfs_id, g));
    }
    Ok(set)
}

/// Apply draft `set` inside the caller's transaction, which holds the feed's
/// lock: what is read next is the feed as the draft makes it. The caller rolls
/// it back. A change that does not apply is left out, as a preview leaves it.
async fn apply_draft(conn: &mut PgConnection, set: &ChangeSet, actor: &str) -> EditorResult<()> {
    let changes = service::load_changes_to_apply(conn, set.change_set_id).await?;
    service::evaluate(conn, &set.gtfs_id, &changes, actor).await?;
    Ok(())
}

// ---------------------------------------------------------------- the list

/// `GET /feeds/{g}/routes?change_set=`: the routes list as draft `id` makes the
/// feed - its new routes listed, its deleted ones not, its edits shown.
pub async fn list_routes_in_draft(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    g: &str,
    q: Option<&str>,
    active: Option<bool>,
    page: &Page,
) -> EditorResult<Value> {
    retry_transient(|| async {
        let mut tx = state.pool.begin().await?;
        let set = draft_of(&mut tx, id, g, false).await?;
        lock_feed(&mut tx, g).await?;
        apply_draft(&mut tx, &set, &ctx.user.email).await?;
        let out = service::list_routes_on(&mut tx, g, q, active, page).await;
        tx.rollback().await?;
        out
    })
    .await
}

// ---------------------------------------------------------------- clone

/// `POST /feeds/{g}/routes/{route_id}:clone` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneRequest {
    /// The new route's id: unused in the feed and in its open drafts.
    pub route_id: String,
    /// Left out or blank: the route's own.
    #[serde(default)]
    pub short_name: Option<String>,
    /// Left out: the route's own; `null` or blank: none.
    #[serde(default, deserialize_with = "given")]
    pub long_name: Option<Option<String>>,
}

/// A field that is there, even as `null`, as `Some`.
fn given<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

/// A route as a clone copies it, read with the draft applied.
struct Source {
    route: Value,
    /// Its other routes.txt fields, by their GTFS names; null where unset.
    gtfs: Value,
    /// Its stop orders, in key order.
    patterns: Vec<trips::PatternRow>,
    rows: HashMap<i16, Vec<RouteRow>>,
    profiles: Vec<StoredProfile>,
    /// In the shape a `route_trips` change sends them.
    trips: Vec<Value>,
    /// Its fare stages, on a feed built from them; none on any other.
    stages: Vec<StageLink>,
    uses_stages: bool,
}

async fn read_source(conn: &mut PgConnection, g: &str, id: &str) -> EditorResult<Source> {
    let route = service::route_row(conn, g, id)
        .await?
        .ok_or_else(|| EditorError::not_found("route_not_found", format!("no route {id}")))?;
    if route["deleted"] == json!(true) {
        return Err(EditorError::conflict(
            "route_deleted",
            format!("route {id} is deleted; there is nothing to clone"),
        ));
    }
    let gtfs = records::gtfs_fields(conn, g, "routes.txt", id, ROUTE_GTFS_FIELDS).await?;
    let patterns = trips::load_patterns(conn, g, id).await?;
    let keys: Vec<(String, i16)> = patterns
        .iter()
        .map(|p| (id.to_string(), p.pattern_key))
        .collect();
    let rows = service::load_patterns_rows(conn, g, &keys)
        .await?
        .into_iter()
        .map(|((_, k), rows)| (k, rows))
        .collect();
    let profiles = trips::load_profiles(conn, g, id, None).await?;
    let trips = trips::route_trips_json(conn, g, id).await?;
    let uses_stages = stages::feed_uses_stages(conn, g).await?;
    let stages = if uses_stages {
        stages::route_links(conn, g, id, None).await?
    } else {
        Vec::new()
    };
    Ok(Source {
        route,
        gtfs,
        patterns,
        rows,
        profiles,
        trips,
        stages,
        uses_stages,
    })
}

fn new_change(entity: &str, op: &str, key: &str, after: Value) -> NewChange {
    NewChange {
        entity: entity.into(),
        op: op.into(),
        entity_key: key.into(),
        after,
        base_row_version: None,
    }
}

/// `#RRGGBB`, upper case, from a colour as stored; None for anything else.
fn hex_color(c: &str) -> Option<String> {
    let hex = c.trim().trim_start_matches('#');
    (hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex.to_ascii_uppercase()))
}

/// What a clone adds to the draft, in order, and what it says it copied and
/// left out.
struct ClonePlan {
    changes: Vec<NewChange>,
    copied: Value,
    left_out: Value,
}

fn clone_plan(src: &Source, to: &str, short: &str, long: Option<&str>) -> ClonePlan {
    let r = &src.route;
    let text = |k: &str| {
        r[k].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // The stop orders the new route gets. Pattern 1 comes with the route; another
    // one is made by its stop list, which a feed built from stages does not take
    // (it serves pattern 1 alone, from the stages).
    let has_rows = |k: i16| {
        src.rows
            .get(&k)
            .is_some_and(|rows| rows.iter().any(|x| !x.is_marker()))
    };
    let copied: Vec<i16> = src
        .patterns
        .iter()
        .map(|p| p.pattern_key)
        .filter(|&k| k == FIRST_PATTERN || (!src.uses_stages && has_rows(k)))
        .collect();
    let left_patterns: Vec<i16> = src
        .patterns
        .iter()
        .map(|p| p.pattern_key)
        .filter(|k| !copied.contains(k))
        .collect();
    let on_copied = |k: Option<i64>| k.is_some_and(|k| copied.contains(&(k as i16)));
    let profiles: Vec<&StoredProfile> = src
        .profiles
        .iter()
        .filter(|p| copied.contains(&p.pattern_key))
        .collect();
    let trips: Vec<Value> = src
        .trips
        .iter()
        .filter(|t| on_copied(t["pattern_key"].as_i64()))
        .map(|t| {
            // a new trip: its own id (minted when the change is added), no block
            // shared with the route it came from, nothing saying where the
            // original came from; the read-only fields go
            let mut t = t.clone();
            if let Some(m) = t.as_object_mut() {
                for k in [
                    "trip_id",
                    "block_id",
                    "source_ref",
                    "source",
                    "sort_key",
                    "ref_s",
                ] {
                    m.remove(k);
                }
                m.retain(|_, v| !v.is_null());
            }
            t
        })
        .collect();
    let mut changes = Vec::new();

    // 1. the route, with the source's fields
    let mut create = Map::new();
    create.insert("route_id".into(), json!(to));
    create.insert("short_name".into(), json!(short));
    if let Some(l) = long {
        create.insert("long_name".into(), json!(l));
    }
    if let Some(t) = r["route_type"].as_i64() {
        create.insert("route_type".into(), json!(t));
    }
    if let Some(a) = text("agency_id") {
        create.insert("agency_id".into(), json!(a));
    }
    for k in ["color", "text_color"] {
        if let Some(c) = text(k).as_deref().and_then(hex_color) {
            create.insert(k.into(), json!(c));
        }
    }
    for f in ROUTE_GTFS_FIELDS {
        if !src.gtfs[*f].is_null() {
            create.insert(f.to_string(), src.gtfs[*f].clone());
        }
    }
    changes.push(new_change("route", "create", to, Value::Object(create)));

    // 2. what a create does not carry: the map line, the fare tag, the status,
    // and who owns its trips
    let mut update = Map::new();
    let line =
        text("encoded_polyline").filter(|p| decode_polyline(p).is_some_and(|pts| pts.len() >= 2));
    if let Some(p) = &line {
        update.insert("encoded_polyline".into(), json!(p));
        let source = text("polyline_source")
            .filter(|s| POLYLINE_SOURCES.contains(&s.as_str()))
            .unwrap_or_else(|| "imported".into());
        update.insert("polyline_source".into(), json!(source));
    }
    if let Some(tag) = text("route_tag") {
        update.insert("route_tag".into(), json!(tag));
    }
    if r["active"] == json!(false) {
        update.insert("active".into(), json!(false));
    }
    // its trips are made here, and no schedule sync knows the new route
    if !trips.is_empty() {
        update.insert("schedule_source".into(), json!("editor"));
    }
    if !update.is_empty() {
        changes.push(new_change("route", "update", to, Value::Object(update)));
    }

    // 3. its stops: the stages a feed built from them takes, else each stop
    // order's rows - every one based on nothing, the route being new
    let mut stop_lists = 0;
    if src.uses_stages {
        if !src.stages.is_empty() {
            changes.push(new_change(
                "route_stages",
                "replace",
                to,
                json!({"stages": src.stages, "base_stages_hash": trips::empty_hash()}),
            ));
        }
    } else {
        for &k in &copied {
            let Some(rows) = src.rows.get(&k).filter(|_| has_rows(k)) else {
                continue;
            };
            let mut after = json!({"rows": rows, "base_rows_hash": trips::empty_hash()});
            if k != FIRST_PATTERN {
                after["pattern_key"] = json!(k);
            }
            changes.push(new_change("route_stops", "replace", to, after));
            stop_lists += 1;
        }
    }

    // 4. the stop orders' names and directions
    for p in src
        .patterns
        .iter()
        .filter(|p| copied.contains(&p.pattern_key))
    {
        if p.name.is_none() && p.direction_id.is_none() {
            continue;
        }
        let mut after = json!({"pattern_key": p.pattern_key});
        if let Some(n) = &p.name {
            after["name"] = json!(n);
        }
        if let Some(d) = p.direction_id {
            after["direction_id"] = json!(d);
        }
        changes.push(new_change("pattern", "update", to, after));
    }

    // 5. the timings, under the same keys, so the trips find them
    for p in &profiles {
        let mut after = json!({
            "pattern_key": p.pattern_key, "profile_key": p.profile_key,
            "arrival_s": p.offsets.arrival, "departure_s": p.offsets.departure,
            "base_hash": trips::empty_hash(),
        });
        if let Some(l) = &p.label {
            after["label"] = json!(l);
        }
        // `interpolated` is the carry-over's to write; a copy of one is manual
        if PROFILE_SOURCES.contains(&p.source.as_str()) {
            after["source"] = json!(p.source);
        }
        changes.push(new_change("timing_profile", "replace", to, after));
    }

    // 6. the trips
    if !trips.is_empty() {
        changes.push(new_change(
            "route_trips",
            "replace",
            to,
            json!({"trips": trips, "base_trips_hash": trips::empty_hash()}),
        ));
    }

    let left_profiles = src.profiles.len() - profiles.len();
    let left_trips = src.trips.len() - trips.len();
    ClonePlan {
        changes,
        copied: json!({
            "patterns": copied.len(),
            "stop_lists": stop_lists,
            "stages": src.stages.len(),
            "profiles": profiles.len(),
            "trips": trips.len(),
            "map_line": line.is_some(),
        }),
        left_out: json!({
            "patterns": left_patterns,
            "profiles": left_profiles,
            "trips": left_trips,
        }),
    }
}

/// Refuse a new route id something already has: a route of the feed, deleted
/// ones included, or a create in any of the feed's open drafts.
async fn route_id_free(conn: &mut PgConnection, g: &str, id: &str) -> EditorResult<()> {
    let row = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2) AS live, \
                (SELECT cs.title FROM gtfs_change ch \
                   JOIN gtfs_change_set cs ON cs.change_set_id = ch.change_set_id \
                  WHERE cs.gtfs_id = $1 AND cs.status NOT IN ('committed', 'discarded') \
                    AND ch.entity = 'route' AND ch.op = 'create' AND ch.entity_key = $2 \
                  ORDER BY cs.created_at LIMIT 1) AS drafted",
    )
    .bind(g)
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    let live: bool = row.try_get("live")?;
    let drafted: Option<String> = row.try_get("drafted")?;
    let message = match (live, drafted) {
        (true, _) => format!("route {id} already exists in feed {g}; choose another id"),
        (false, Some(title)) => {
            format!(
                "route {id} is already created in draft \u{201c}{title}\u{201d}; choose another id"
            )
        }
        (false, None) => return Ok(()),
    };
    Err(EditorError::conflict("route_exists", message).with_details(json!({"route_id": id})))
}

/// `POST /feeds/{g}/routes/{from}:clone?change_set=`: a new route `req.route_id`
/// made from route `from` as draft `id` makes it, as the changes that build it,
/// all added to the draft in one transaction. Answers `{route_id,
/// from_route_id, change_ids, copied, left_out}`.
pub async fn clone_route(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    g: &str,
    from: &str,
    req: CloneRequest,
) -> EditorResult<Value> {
    retry_transient(|| clone_once(state, ctx, id, g, from, req.clone())).await
}

async fn clone_once(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
    g: &str,
    from: &str,
    req: CloneRequest,
) -> EditorResult<Value> {
    let to = req.route_id.trim().to_string();
    check_entity_id("route_id", &to).map_err(|f| {
        EditorError::bad_request("invalid_change", f.message.clone())
            .with_details(json!({"code": f.code}))
    })?;
    let mut tx = state.pool.begin().await?;
    lock_feed_of_set(&mut tx, id).await?;
    let set = draft_of(&mut tx, id, g, true).await?;
    service::editable(&set)?;
    // the route as the draft makes it: applied in a savepoint, read, rolled back
    let src = {
        let mut sp = sqlx::Connection::begin(&mut *tx).await?;
        apply_draft(&mut sp, &set, &ctx.user.email).await?;
        let src = read_source(&mut sp, g, from).await;
        sp.rollback().await?;
        src?
    };
    route_id_free(&mut tx, g, &to).await?;
    let short = req
        .short_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| src.route["short_name"].as_str().map(str::to_string))
        .unwrap_or_default();
    let long = match &req.long_name {
        None => src.route["long_name"].as_str().map(str::to_string),
        Some(l) => l.as_deref().map(str::trim).map(str::to_string),
    }
    .filter(|l| !l.trim().is_empty());
    let plan = clone_plan(&src, &to, &short, long.as_deref());
    let mut change_ids = Vec::with_capacity(plan.changes.len());
    for c in plan.changes {
        change_ids.push(service::add_change_to(&mut tx, ctx, &set, c).await?);
    }
    let out = json!({
        "route_id": to,
        "from_route_id": from,
        "change_ids": change_ids,
        "copied": plan.copied,
        "left_out": plan.left_out,
    });
    auth::audit(
        &mut *tx,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "route_cloned",
        Some(g),
        Some(id),
        json!({
            "from": from, "to": to, "change_ids": out["change_ids"],
            "copied": out["copied"], "left_out": out["left_out"],
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

// ---------------------------------------------------------------- export

/// What the rows of an export say to do: `add` makes the routes (in another
/// feed, or under new ids), `update` replaces them where they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportAction {
    Add,
    Update,
}

impl ExportAction {
    /// `add` when left out.
    pub fn parse(s: Option<&str>) -> EditorResult<ExportAction> {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("add") => Ok(ExportAction::Add),
            Some("update") => Ok(ExportAction::Update),
            Some(other) => Err(EditorError::bad_request(
                "invalid_query",
                format!("action is add or update, not {other:?}"),
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            ExportAction::Add => "add",
            ExportAction::Update => "update",
        }
    }
}

/// The zip an export answers with.
pub struct ExportFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub routes: usize,
}

/// `GET /feeds/{g}/routes:export`: routes `wanted` (every live route when
/// none) as a zip of CSV files the bulk import reads, with the draft
/// `change_set` applied when given.
pub async fn export_routes(
    state: &EditorState,
    ctx: &Ctx,
    g: &str,
    wanted: Option<&[String]>,
    action: ExportAction,
    change_set: Option<Uuid>,
) -> EditorResult<ExportFile> {
    retry_transient(|| async {
        let mut tx = state.pool.begin().await?;
        let draft = match change_set {
            Some(id) => {
                let set = draft_of(&mut tx, id, g, false).await?;
                lock_feed(&mut tx, g).await?;
                apply_draft(&mut tx, &set, &ctx.user.email).await?;
                Some(set.json["title"].as_str().unwrap_or("").to_string())
            }
            None => None,
        };
        let out = build_export(&mut tx, g, wanted, action, draft.as_deref()).await;
        tx.rollback().await?;
        out
    })
    .await
}

/// One route of an export, as its routes.csv row reads it.
struct ExportRoute {
    route_id: String,
    cells: HashMap<&'static str, String>,
}

const ROUTE_TEXT: [&str; 12] = [
    "short_name",
    "long_name",
    "color",
    "text_color",
    "route_desc",
    "route_url",
    "network_id",
    "agency_id",
    "route_tag",
    "polyline_source",
    "encoded_polyline",
    "route_id",
];
const ROUTE_NUMBERS: [&str; 4] = [
    "route_sort_order",
    "continuous_pickup",
    "continuous_drop_off",
    "route_type",
];

async fn export_route_rows(
    conn: &mut PgConnection,
    g: &str,
    wanted: Option<&[String]>,
) -> EditorResult<Vec<ExportRoute>> {
    let ids: Option<Vec<String>> = wanted.map(|w| {
        let mut seen = HashSet::new();
        w.iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && seen.insert(s.clone()))
            .collect()
    });
    let rows = sqlx::query(
        "SELECT route_id, short_name, long_name, color, text_color, route_desc, route_url, \
                route_sort_order::bigint AS route_sort_order, \
                continuous_pickup::bigint AS continuous_pickup, \
                continuous_drop_off::bigint AS continuous_drop_off, \
                network_id, agency_id, route_type::bigint AS route_type, route_tag, active, \
                polyline_source, encoded_polyline, deleted \
         FROM gtfs_route \
         WHERE gtfs_id = $1 AND ($2::text[] IS NULL OR route_id = ANY($2)) \
         ORDER BY short_name, route_id",
    )
    .bind(g)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    let mut by_id: HashMap<String, ExportRoute> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for r in &rows {
        if r.try_get::<bool, _>("deleted")? {
            continue;
        }
        let mut cells = HashMap::new();
        for k in ROUTE_TEXT {
            if let Some(v) = r.try_get::<Option<String>, _>(k)? {
                cells.insert(k, v);
            }
        }
        for k in ROUTE_NUMBERS {
            if let Some(v) = r.try_get::<Option<i64>, _>(k)? {
                cells.insert(k, v.to_string());
            }
        }
        let active: bool = r.try_get("active")?;
        cells.insert("active", if active { "1" } else { "0" }.to_string());
        let id: String = r.try_get("route_id")?;
        order.push(id.clone());
        by_id.insert(
            id.clone(),
            ExportRoute {
                route_id: id,
                cells,
            },
        );
    }
    match ids {
        None => Ok(order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect()),
        Some(ids) => {
            let missing: Vec<&String> = ids.iter().filter(|id| !by_id.contains_key(*id)).collect();
            if !missing.is_empty() {
                return Err(EditorError::not_found(
                    "route_not_found",
                    format!(
                        "no route {} in feed {g}",
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
                .with_details(json!({"route_ids": missing})));
            }
            // in the order asked for
            Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
        }
    }
}

/// A cell as a CSV writes it: text as it is, a number as its digits, nothing
/// for null.
fn cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        other => other.to_string(),
    }
}

/// The rows of one CSV file, each with the group it belongs to: a route, a
/// profile, a service - a group is uploaded whole, so a file split in parts
/// keeps each group in one part.
struct Table {
    kind: bulk::Kind,
    file: &'static str,
    rows: Vec<(String, Vec<String>)>,
}

impl Table {
    fn new(kind: bulk::Kind, file: &'static str) -> Table {
        Table {
            kind,
            file,
            rows: Vec::new(),
        }
    }

    /// A row by column name; a column it does not name is blank.
    fn push(&mut self, group: &str, cells: &HashMap<&str, String>) {
        let row = self
            .kind
            .columns()
            .iter()
            .map(|c| cells.get(c).cloned().unwrap_or_default())
            .collect();
        self.rows.push((group.to_string(), row));
    }

    /// The file's parts: at most `bulk::MAX_ROWS` rows each, a group never
    /// split, named `routes.csv` or `routes-1.csv`, `routes-2.csv`, ...
    fn parts(&self) -> Vec<(String, Vec<&Vec<String>>)> {
        let mut parts: Vec<Vec<&Vec<String>>> = Vec::new();
        let mut current: Vec<&Vec<String>> = Vec::new();
        let mut i = 0;
        while i < self.rows.len() {
            let group = &self.rows[i].0;
            let end = self.rows[i..]
                .iter()
                .position(|(g, _)| g != group)
                .map_or(self.rows.len(), |n| i + n);
            if !current.is_empty() && current.len() + (end - i) > bulk::MAX_ROWS {
                parts.push(std::mem::take(&mut current));
            }
            current.extend(self.rows[i..end].iter().map(|(_, r)| r));
            i = end;
        }
        if !current.is_empty() {
            parts.push(current);
        }
        let stem = self.file.trim_end_matches(".csv");
        let n = parts.len();
        parts
            .into_iter()
            .enumerate()
            .map(|(k, rows)| {
                let name = if n == 1 {
                    self.file.to_string()
                } else {
                    format!("{stem}-{}.csv", k + 1)
                };
                (name, rows)
            })
            .collect()
    }
}

fn csv_bytes(header: &[&str], rows: &[&Vec<String>]) -> EditorResult<Vec<u8>> {
    let failed = |e: String| EditorError::internal(format!("the export could not be written: {e}"));
    // a byte order mark, so spreadsheet programs open Tamil names as UTF-8, as
    // the dashboard's templates do
    let mut w = csv::Writer::from_writer(b"\xEF\xBB\xBF".to_vec());
    w.write_record(header).map_err(|e| failed(e.to_string()))?;
    for r in rows {
        w.write_record(r.iter())
            .map_err(|e| failed(e.to_string()))?;
    }
    w.into_inner().map_err(|e| failed(e.to_string()))
}

async fn build_export(
    conn: &mut PgConnection,
    g: &str,
    wanted: Option<&[String]>,
    action: ExportAction,
    draft: Option<&str>,
) -> EditorResult<ExportFile> {
    let routes = export_route_rows(conn, g, wanted).await?;
    if routes.is_empty() {
        return Err(EditorError::not_found(
            "route_not_found",
            format!("feed {g} has no routes to export"),
        ));
    }
    let ids: Vec<String> = routes.iter().map(|r| r.route_id.clone()).collect();
    let uses_stages = stages::feed_uses_stages(conn, g).await?;
    let act = action.name().to_string();

    // routes.csv
    let mut routes_csv = Table::new(bulk::Kind::Routes, "routes.csv");
    for r in &routes {
        let mut cells: HashMap<&str, String> =
            r.cells.iter().map(|(k, v)| (*k, v.clone())).collect();
        cells.insert("action", act.clone());
        routes_csv.push(&r.route_id, &cells);
    }

    // the stop lists, or the stages a feed built from them runs through
    let mut stops_csv = Table::new(bulk::Kind::RouteStops, "route_stops.csv");
    let mut stages_csv = Table::new(bulk::Kind::RouteStages, "route_stages.csv");
    let mut markers = 0usize;
    if uses_stages {
        let links = sqlx::query(
            "SELECT route_id, stage_id, direction, stage_no FROM gtfs_route_stage \
             WHERE gtfs_id = $1 AND route_id = ANY($2) AND variant_id IS NULL \
             ORDER BY route_id, position",
        )
        .bind(g)
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?;
        let mut by_route: HashMap<String, Vec<(String, String, i32)>> = HashMap::new();
        for l in &links {
            by_route.entry(l.try_get("route_id")?).or_default().push((
                l.try_get("stage_id")?,
                l.try_get("direction")?,
                l.try_get("stage_no")?,
            ));
        }
        for id in &ids {
            for (k, (stage, direction, no)) in by_route
                .remove(id)
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                let cells: HashMap<&str, String> = HashMap::from([
                    ("action", act.clone()),
                    ("route_id", id.clone()),
                    ("position", (k + 1).to_string()),
                    ("stage_id", stage),
                    ("direction", direction),
                    ("stage_no", no.to_string()),
                ]);
                stages_csv.push(id, &cells);
            }
        }
    } else {
        let keys: Vec<(String, i16)> = sqlx::query(
            "SELECT route_id, pattern_key FROM gtfs_pattern \
             WHERE gtfs_id = $1 AND route_id = ANY($2) ORDER BY route_id, pattern_key",
        )
        .bind(g)
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?
        .iter()
        .map(|r| -> Result<(String, i16), sqlx::Error> {
            Ok((r.try_get("route_id")?, r.try_get("pattern_key")?))
        })
        .collect::<Result<_, _>>()?;
        let mut rows = service::load_patterns_rows(conn, g, &keys).await?;
        let mut patterns: BTreeMap<&str, Vec<i16>> = BTreeMap::new();
        for (r, k) in &keys {
            patterns.entry(r.as_str()).or_default().push(*k);
        }
        for id in &ids {
            for k in patterns.get(id.as_str()).cloned().unwrap_or_default() {
                let list = rows.remove(&(id.clone(), k)).unwrap_or_default();
                let mut sequence = 0;
                for row in list {
                    // a map shaping point is not a stop: the map line carries
                    // the shape, and the stop list upload takes stops only
                    if row.stop_type == ROUTE_CORRECTION {
                        markers += 1;
                        continue;
                    }
                    sequence += 1;
                    let mut cells: HashMap<&str, String> = HashMap::from([
                        ("action", act.clone()),
                        ("route_id", id.clone()),
                        ("sequence", sequence.to_string()),
                        ("stop_id", row.stop_id.clone().unwrap_or_default()),
                        ("stop_type", row.stop_type.clone()),
                        ("stage_no", row.stage_no.to_string()),
                        ("stage_name", row.stage_name.clone()),
                    ]);
                    if k != FIRST_PATTERN {
                        cells.insert("pattern_key", k.to_string());
                    }
                    for (c, v) in [
                        ("pickup_type", row.pickup_type),
                        ("drop_off_type", row.drop_off_type),
                        ("timepoint", row.timepoint),
                        ("continuous_pickup", row.continuous_pickup),
                        ("continuous_drop_off", row.continuous_drop_off),
                    ] {
                        if let Some(v) = v {
                            cells.insert(c, v.to_string());
                        }
                    }
                    for (c, v) in [
                        ("stop_headsign", &row.stop_headsign),
                        ("pickup_booking_rule_id", &row.pickup_booking_rule_id),
                        ("drop_off_booking_rule_id", &row.drop_off_booking_rule_id),
                    ] {
                        if let Some(v) = v {
                            cells.insert(c, v.clone());
                        }
                    }
                    if let Some(n) = row.stop_sequence {
                        cells.insert("stop_sequence", n.to_string());
                    }
                    if let Some(d) = row.shape_dist_traveled {
                        cells.insert("shape_dist_traveled", d.to_string());
                    }
                    stops_csv.push(&format!("{id}#{k}"), &cells);
                }
            }
        }
    }

    // the timings, one row per stop of each
    let mut profiles_csv = Table::new(bulk::Kind::TimingProfiles, "timing_profiles.csv");
    let mut firsts: HashMap<(String, i16, i32), i32> = HashMap::new();
    for p in sqlx::query(
        "SELECT route_id, pattern_key, profile_key, arrival_s, departure_s, label \
         FROM gtfs_timing_profile WHERE gtfs_id = $1 AND route_id = ANY($2) \
         ORDER BY route_id, pattern_key, profile_key",
    )
    .bind(g)
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?
    {
        let route: String = p.try_get("route_id")?;
        let (pattern, profile): (i16, i32) = (p.try_get("pattern_key")?, p.try_get("profile_key")?);
        let arrival: Vec<i32> = p.try_get("arrival_s")?;
        let departure: Vec<i32> = p.try_get("departure_s")?;
        let label: Option<String> = p.try_get("label")?;
        firsts.insert(
            (route.clone(), pattern, profile),
            arrival.first().copied().unwrap_or(0),
        );
        for (i, (a, d)) in arrival.iter().zip(&departure).enumerate() {
            let mut cells: HashMap<&str, String> = HashMap::from([
                ("action", act.clone()),
                ("route_id", route.clone()),
                ("pattern_key", pattern.to_string()),
                ("profile_key", profile.to_string()),
                ("stop_sequence", (i + 1).to_string()),
                ("arrival_offset", a.to_string()),
                ("departure_offset", d.to_string()),
            ]);
            if let Some(l) = &label {
                cells.insert("label", l.clone());
            }
            profiles_csv.push(&format!("{route}#{pattern}#{profile}"), &cells);
        }
    }
    // in the routes' order, each route's profiles together
    let order: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, r)| (r.as_str(), i))
        .collect();
    profiles_csv
        .rows
        .sort_by_key(|(group, _)| order.get(group.split('#').next().unwrap_or("")).copied());

    // the trips, and the services they run on
    let mut trips_csv = Table::new(bulk::Kind::RouteTrips, "route_trips.csv");
    let mut trips = trips::load_routes_trips(conn, g, &ids).await?;
    let mut services: Vec<String> = Vec::new();
    for id in &ids {
        for t in trips.remove(id).unwrap_or_default() {
            let first = t
                .profile_key
                .and_then(|p| firsts.get(&(id.clone(), t.pattern_key, p)).copied())
                .unwrap_or(0);
            let v = trips::trip_json(&t, first);
            let mut cells: HashMap<&str, String> =
                HashMap::from([("action", act.clone()), ("route_id", id.clone())]);
            for c in [
                "trip_id",
                "pattern_key",
                "profile_key",
                "service_id",
                "direction_id",
                "start_time",
                "headsign",
                "short_name",
                "block_id",
                "shape_id",
                "wheelchair_accessible",
                "bikes_allowed",
                "cars_allowed",
            ] {
                let text = cell(&v[c]);
                if !text.is_empty() {
                    cells.insert(c, text);
                }
            }
            // JSON, written as text in its cell
            let frequencies: Vec<Value> = v["frequencies"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut f| {
                    if let Some(m) = f.as_object_mut() {
                        m.retain(|_, x| !x.is_null());
                    }
                    f
                })
                .collect();
            if !frequencies.is_empty() {
                cells.insert("frequencies", Value::Array(frequencies).to_string());
            }
            if !v["source_ref"].is_null() {
                cells.insert("source_ref", v["source_ref"].to_string());
            }
            if !services.contains(&t.service_id) {
                services.push(t.service_id.clone());
            }
            trips_csv.push(id, &cells);
        }
    }
    let mut services_csv = Table::new(bulk::Kind::Services, "services.csv");
    services.sort();
    for s in &services {
        let Some(v) = trips::service_json(conn, g, s).await? else {
            continue;
        };
        let mut cells: HashMap<&str, String> =
            HashMap::from([("action", act.clone()), ("service_id", s.clone())]);
        for d in trips::DAYS {
            cells.insert(
                d,
                if v["days"][d] == json!(true) {
                    "1"
                } else {
                    "0"
                }
                .into(),
            );
        }
        for c in ["start_date", "end_date", "label"] {
            let text = cell(&v[c]);
            if !text.is_empty() {
                cells.insert(c, text);
            }
        }
        let dates = v["dates"].as_array().cloned().unwrap_or_default();
        if dates.is_empty() {
            services_csv.push(s, &cells);
        }
        for d in dates {
            let mut row = cells.clone();
            row.insert("date", cell(&d["date"]));
            row.insert("exception_type", cell(&d["exception_type"]));
            services_csv.push(s, &row);
        }
    }

    // the files, in the order they are imported, and what to do with them
    let tables = [
        &routes_csv,
        if uses_stages { &stages_csv } else { &stops_csv },
        &profiles_csv,
        &services_csv,
        &trips_csv,
    ];
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let zip_failed =
        |e: String| EditorError::internal(format!("the export could not be zipped: {e}"));
    let mut written: Vec<(String, usize, &'static str)> = Vec::new();
    for t in tables {
        for (name, rows) in t.parts() {
            let bytes = csv_bytes(t.kind.columns(), &rows)?;
            z.start_file(name.as_str(), opts)
                .map_err(|e| zip_failed(e.to_string()))?;
            z.write_all(&bytes).map_err(|e| zip_failed(e.to_string()))?;
            written.push((name, rows.len(), t.kind_label()));
        }
    }
    let readme = readme(
        g,
        routes.len(),
        action,
        draft,
        uses_stages,
        &written,
        markers,
    );
    z.start_file("README.txt", opts)
        .map_err(|e| zip_failed(e.to_string()))?;
    z.write_all(readme.as_bytes())
        .map_err(|e| zip_failed(e.to_string()))?;
    let bytes = z
        .finish()
        .map_err(|e| zip_failed(e.to_string()))?
        .into_inner();
    let name = if routes.len() == 1 {
        format!("{g}-route-{}.zip", routes[0].route_id)
    } else {
        format!("{g}-routes.zip")
    };
    Ok(ExportFile {
        name,
        bytes,
        routes: routes.len(),
    })
}

impl Table {
    /// What the import page calls this kind of file.
    fn kind_label(&self) -> &'static str {
        match self.kind {
            bulk::Kind::Routes => "Routes",
            bulk::Kind::RouteStops => "Route stop lists",
            bulk::Kind::RouteStages => "Route stages",
            bulk::Kind::TimingProfiles => "Timings",
            bulk::Kind::Services => "Services (calendar)",
            bulk::Kind::RouteTrips => "Trips",
            _ => "",
        }
    }
}

/// README.txt: what the files are, and how to import them.
fn readme(
    g: &str,
    routes: usize,
    action: ExportAction,
    draft: Option<&str>,
    uses_stages: bool,
    written: &[(String, usize, &str)],
    markers: usize,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{routes} route{} of feed {g}{}, exported {}.\n\n",
        if routes == 1 { "" } else { "s" },
        draft
            .map(|t| format!(" as draft \u{201c}{t}\u{201d} makes them"))
            .unwrap_or_default(),
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
    ));
    s.push_str(&match action {
        ExportAction::Add => "Every row says action add: import the files into a feed that does not have these routes yet, to make them there. To make them again in this feed under new ids, change route_id in every file and empty the trip_id column, and the editor makes new trip ids (or use Clone on a route's page).\n\n".to_string(),
        ExportAction::Update => "Every row says action update: import the files, edited or not, back into this feed to replace these routes' fields, stop lists, timings and trips with what the files say.\n\n".to_string(),
    });
    s.push_str("Import them on the editor's \"Import from a CSV file\" page (New > Import from a CSV file), one file at a time and in this order, choosing what the file holds. Each file is checked against your draft first, and its changes go into the draft only when you choose; the draft is reviewed and committed like any other.\n\n");
    for (i, (name, rows, label)) in written.iter().enumerate() {
        s.push_str(&format!(
            "  {}. {name:<24} {label} ({rows} row{})\n",
            i + 1,
            if *rows == 1 { "" } else { "s" }
        ));
    }
    s.push('\n');
    if written.iter().any(|(n, _, _)| n.contains('-')) {
        s.push_str("A file of more than 5,000 rows is split into parts (route_trips-1.csv, route_trips-2.csv, ...), each route whole in one part: import every part.\n");
    }
    s.push_str("A kind of file with nothing in it is left out. services.csv holds the services the trips run on: import it only into a feed that does not have them yet, and skip it otherwise.\n");
    if uses_stages {
        s.push_str("This feed builds its routes from fare stages: route_stages.csv says which stages each route runs through. The stages themselves are not in the files; the feed you import into needs stages with these ids.\n");
    }
    s.push_str("\nNot in the files: a route's own spelling of a stop's name, the names of its stop orders, its temporary routes (diversions), and the records that name it (fare rules, transfers, attributions).");
    if markers > 0 {
        s.push_str(&format!(
            " Nor its {markers} map shaping point{} (ROUTE CORRECTION rows): the map line in routes.csv (encoded_polyline) carries the shape, and a stop list upload takes stops only.",
            if markers == 1 { "" } else { "s" }
        ));
    }
    s.push('\n');
    s
}
