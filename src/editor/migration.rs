//! Moving master's edits to prod (docs/gtfs-editor.md section 23).
//!
//! Master and prod each have their own editor database, and nothing done on one
//! reaches the other. A migration carries master's work to prod as a file:
//! master writes it, an admin on prod reads it in, and what it brings becomes
//! ordinary prod drafts, which a second person approves and commits. No
//! connection or credential joins the two environments.
//!
//! Two kinds of file:
//!
//! - **One draft** (`change_set`): a committed master draft, change by change.
//!   A change goes in when prod still has what master had when the change was
//!   made - the change's `before`, or the hash of the stop list, stage list,
//!   trips or timing profile it was based on. Otherwise it is a *conflict*: left
//!   out and listed with both sides, and the admin may take master's side of any
//!   of them. A change prod already has is left out as done.
//! - **The whole feed** (`full`): master's stops and stations, routes, stages,
//!   the routes' lists of stages and its timetable, set beside prod's. What
//!   differs becomes changes in steps, each needing the one before it
//!   committed: stops and routes; stages; the routes' stage lists; turning
//!   `use_stages` on; the timetable (through [`draft_import`]). Each run drafts
//!   the first step with anything to do. What prod changed itself - after the
//!   version the first full migration names (`since_version`), and after the
//!   last migration that set it - is prod's own edit: kept, and listed as a
//!   conflict the admin may still take master's side of.
//!
//! A change prod refuses - a stop still in use, a fare rule a stage list would
//! break - is dropped from its draft and listed, and the draft keeps the rest.
//! Feed settings never move: each environment keeps its own, but for the one
//! switch a full migration drafts, turning stages on once prod has them.
//!
//! [`draft_import`]: super::draft_import

use super::auth::{self, Ctx};
use super::draft_import;
use super::error::{EditorError, EditorResult};
use super::feed_lock::lock_feed;
use super::service::{self, ChangeSet, NewChange};
use super::stages::{self, StageKey};
use super::validation::{RouteRow, StageLink};
use super::EditorState;
use actix_web::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, PgPool, Row as _};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Read as _, Write as _};
use uuid::Uuid;

/// What a migration file says it is, in its `manifest.json`.
pub const FORMAT: &str = "gtfs-editor-migration/1";

/// What a full migration may carry, in the order its steps need them.
pub const SCOPES: &[&str] = &["stops", "routes", "stages", "timetables"];

/// The most stages one draft of the stages step makes or changes.
const STAGES_PER_SET: usize = 1_000;
/// The most routes one draft of the stage-list step sets: every read of a
/// draft's page replays it, and a route's list replays with its fare rules, so
/// these stay the size master's own stage drafts were.
const LISTS_PER_SET: usize = 250;
/// How many conflicts, dropped changes and notes a report lists; its counts are
/// whole.
const LISTED: usize = 500;
/// The files of master's GTFS the timetable step brings, and only those.
const TIMETABLE_FILES: &[&str] = &[
    "calendar.txt",
    "calendar_dates.txt",
    "trips.txt",
    "stop_times.txt",
    "frequencies.txt",
];
/// A change may need others taken out before it replays clean; this many rounds
/// of that is far more than any real draft needs.
const MAX_ROUNDS: usize = 12;

// ---------------------------------------------------------------- the file

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    /// `change_set` (one committed draft) or `full` (the whole feed).
    pub kind: String,
    pub gtfs_id: String,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_set: Option<SourceSet>,
    /// `full`: what it carries, of [`SCOPES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Every other file of the migration, and the sha256 of its bytes.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    /// `master` or `prod`: the dhall `is_master` of the deployment that wrote it.
    pub env: String,
    /// The feed's version when the file was written.
    pub version: i64,
    pub use_stages: bool,
    pub exported_at: DateTime<Utc>,
    pub exported_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSet {
    pub change_set_id: Uuid,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub committed_version: Option<i64>,
    #[serde(default)]
    pub committed_at: Option<String>,
    #[serde(default)]
    pub committed_by: Option<String>,
    pub changes: usize,
}

/// One change of a master draft as the file carries it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceChange {
    pub position: i32,
    pub entity: String,
    pub op: String,
    pub entity_key: String,
    #[serde(default)]
    pub base_row_version: Option<i32>,
    #[serde(default)]
    pub before: Value,
    #[serde(default)]
    pub after: Value,
}

pub struct ExportFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

fn export_failed(e: impl std::fmt::Display) -> EditorError {
    EditorError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "export_failed",
        e.to_string(),
    )
}

fn invalid(message: impl Into<String>) -> EditorError {
    EditorError::bad_request("invalid_migration", message)
}

fn to_json_bytes<T: Serialize>(v: &T) -> EditorResult<Vec<u8>> {
    serde_json::to_vec(v).map_err(export_failed)
}

/// The migration as a zip: `manifest.json`, then the other files. A GTFS zip
/// inside it is stored as it is; everything else is deflated.
fn write_zip(manifest: &mut Manifest, files: Vec<(&str, Vec<u8>)>) -> EditorResult<Vec<u8>> {
    for (name, bytes) in &files {
        manifest
            .files
            .insert(name.to_string(), super::crypto::sha256_hex(bytes));
    }
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    let stored = deflated.compression_method(zip::CompressionMethod::Stored);
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file("manifest.json", deflated)
        .map_err(export_failed)?;
    w.write_all(&serde_json::to_vec_pretty(manifest).map_err(export_failed)?)
        .map_err(export_failed)?;
    for (name, bytes) in files {
        let opts = if name.ends_with(".zip") {
            stored
        } else {
            deflated
        };
        w.start_file(name, opts).map_err(export_failed)?;
        w.write_all(&bytes).map_err(export_failed)?;
    }
    Ok(w.finish().map_err(export_failed)?.into_inner())
}

/// A migration file read back: its manifest, checked, and its files.
pub struct Package {
    pub manifest: Manifest,
    files: HashMap<String, Vec<u8>>,
    pub sha256: String,
}

impl Package {
    fn file(&self, name: &str) -> EditorResult<&[u8]> {
        self.files
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| invalid(format!("the file has no {name}")))
    }

    fn json<T: serde::de::DeserializeOwned>(&self, name: &str) -> EditorResult<T> {
        serde_json::from_slice(self.file(name)?).map_err(|e| invalid(format!("{name}: {e}")))
    }
}

pub fn read_package(bytes: &[u8]) -> EditorResult<Package> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| invalid(format!("not a migration file (not a zip: {e})")))?;
    let mut files = HashMap::new();
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| invalid(format!("an entry of the file is unreadable: {e}")))?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let mut buf = Vec::new();
        entry
            .read_to_end(&mut buf)
            .map_err(|e| invalid(format!("{name} is unreadable: {e}")))?;
        files.insert(name, buf);
    }
    let manifest: Manifest = serde_json::from_slice(
        files
            .get("manifest.json")
            .ok_or_else(|| invalid("the file has no manifest.json: it is not a migration file"))?,
    )
    .map_err(|e| invalid(format!("manifest.json: {e}")))?;
    if manifest.format != FORMAT {
        return Err(invalid(format!(
            "the file is {:?}, and this editor reads {FORMAT}",
            manifest.format
        )));
    }
    for (name, sha) in &manifest.files {
        let got = files
            .get(name)
            .ok_or_else(|| invalid(format!("the file has no {name}")))?;
        if &super::crypto::sha256_hex(got) != sha {
            return Err(EditorError::bad_request(
                "file_damaged",
                format!("{name} does not match its checksum; download the file again"),
            ));
        }
    }
    Ok(Package {
        manifest,
        files,
        sha256: super::crypto::sha256_hex(bytes),
    })
}

async fn source(
    conn: &mut PgConnection,
    state: &EditorState,
    ctx: &Ctx,
    g: &str,
) -> EditorResult<Source> {
    Ok(Source {
        env: if state.is_master { "master" } else { "prod" }.to_string(),
        version: service::feed_version(conn, g).await?,
        use_stages: stages::feed_uses_stages(conn, g).await?,
        exported_at: Utc::now(),
        exported_by: ctx.user.email.clone(),
    })
}

// ---------------------------------------------------------------- export

/// A committed draft as a migration file.
pub async fn export_change_set(
    state: &EditorState,
    ctx: &Ctx,
    id: Uuid,
) -> EditorResult<ExportFile> {
    ctx.require_admin()?;
    let mut conn = state.pool.acquire().await?;
    let set = service::load_set(&mut conn, id, false).await?;
    if set.status != "committed" {
        return Err(EditorError::conflict(
            "not_committed",
            format!(
                "draft {id} is {}; only a committed draft moves to prod",
                set.status
            ),
        ));
    }
    let g = set.gtfs_id.clone();
    let changes: Vec<SourceChange> = service::load_changes(&mut conn, id)
        .await?
        .into_iter()
        .map(|c| SourceChange {
            position: c.position,
            entity: c.entity,
            op: c.op,
            entity_key: c.entity_key,
            base_row_version: c.base_row_version,
            before: c.before,
            after: c.after,
        })
        .collect();
    let j = &set.json;
    let committed_version = j["committed_version"].as_i64();
    let mut manifest = Manifest {
        format: FORMAT.to_string(),
        kind: "change_set".to_string(),
        gtfs_id: g.clone(),
        source: source(&mut conn, state, ctx, &g).await?,
        change_set: Some(SourceSet {
            change_set_id: id,
            title: j["title"].as_str().unwrap_or("").to_string(),
            description: j["description"].as_str().map(str::to_string),
            committed_version,
            committed_at: j["committed_at"].as_str().map(str::to_string),
            committed_by: j["committed_by_email"].as_str().map(str::to_string),
            changes: changes.len(),
        }),
        scopes: Vec::new(),
        files: BTreeMap::new(),
    };
    let bytes = write_zip(
        &mut manifest,
        vec![("changes.json", to_json_bytes(&changes)?)],
    )?;
    auth::audit(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "migration_exported",
        Some(&g),
        Some(id),
        json!({"kind": "change_set", "changes": changes.len(), "sha256": super::crypto::sha256_hex(&bytes)}),
    )
    .await?;
    let short = id.to_string();
    Ok(ExportFile {
        name: format!(
            "{g}-draft-{}-v{}.migration.zip",
            &short[..8],
            committed_version.unwrap_or(0)
        ),
        bytes,
    })
}

/// Which of [`SCOPES`] a full migration carries: all of them unless asked.
pub fn parse_scopes(asked: Option<&str>) -> EditorResult<Vec<String>> {
    let Some(asked) = asked.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(SCOPES.iter().map(|s| s.to_string()).collect());
    };
    let mut out = BTreeSet::new();
    for s in asked.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !SCOPES.contains(&s) {
            return Err(EditorError::bad_request(
                "invalid_scope",
                format!("{s:?} is not one of {}", SCOPES.join(", ")),
            ));
        }
        out.insert(s);
    }
    // in step order
    Ok(SCOPES
        .iter()
        .filter(|s| out.contains(*s))
        .map(|s| s.to_string())
        .collect())
}

/// The whole feed, as much of it as `scopes` says, as a migration file. Read in
/// one repeatable-read snapshot, so the files agree with each other.
pub async fn export_feed(
    state: &EditorState,
    ctx: &Ctx,
    g: &str,
    scopes: &[String],
) -> EditorResult<ExportFile> {
    ctx.require_admin()?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let src = source(&mut tx, state, ctx, g).await?;
    let has = |s: &str| scopes.iter().any(|x| x == s);
    let mut files: Vec<(&str, Vec<u8>)> = Vec::new();
    if has("stops") {
        files.push(("stops.json", to_json_bytes(&read_stops(&mut tx, g).await?)?));
    }
    if has("routes") {
        files.push((
            "routes.json",
            to_json_bytes(&read_routes(&mut tx, g).await?)?,
        ));
    }
    if has("stages") {
        files.push((
            "stages.json",
            to_json_bytes(&read_stages(&mut tx, g).await?)?,
        ));
        files.push((
            "route_stages.json",
            to_json_bytes(&read_lists(&mut tx, g).await?)?,
        ));
    }
    if has("timetables") {
        let (model, _) = super::feed_io::load_model(&mut tx, g).await?;
        let raw = crate::gtfs::write::to_raw(&model);
        files.push((
            "gtfs.zip",
            crate::gtfs::write::zip_bytes(&raw).map_err(export_failed)?,
        ));
    }
    tx.rollback().await?;
    let version = src.version;
    let mut manifest = Manifest {
        format: FORMAT.to_string(),
        kind: "full".to_string(),
        gtfs_id: g.to_string(),
        source: src,
        change_set: None,
        scopes: scopes.to_vec(),
        files: BTreeMap::new(),
    };
    let bytes = write_zip(&mut manifest, files)?;
    auth::audit(
        &state.pool,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "migration_exported",
        Some(g),
        None,
        json!({"kind": "full", "scopes": scopes, "version": version,
               "sha256": super::crypto::sha256_hex(&bytes)}),
    )
    .await?;
    Ok(ExportFile {
        name: format!("{g}-full-v{version}.migration.zip"),
        bytes,
    })
}

// ---------------------------------------------------------------- state

/// The fields of a stop a full migration carries, by their column names; the
/// GTFS ones are also the names a stop change takes them by.
const STOP_FIELDS: &[&str] = &[
    "stop_id",
    "stop_code",
    "name",
    "lat",
    "lon",
    "location_type",
    "parent_station",
    "platform_code",
    "description",
    "cluster_id",
    "regional_name",
    "hindi_name",
    "unserviceable",
    "tts_stop_name",
    "zone_id",
    "stop_url",
    "stop_timezone",
    "wheelchair_boarding",
    "level_id",
    "stop_access",
    "deleted",
];

/// What a `stop/create` takes besides its id, name and position.
const STOP_CREATE_FIELDS: &[&str] = &[
    "stop_code",
    "platform_code",
    "description",
    "cluster_id",
    "regional_name",
    "hindi_name",
    "tts_stop_name",
    "zone_id",
    "stop_url",
    "stop_timezone",
    "wheelchair_boarding",
    "level_id",
    "stop_access",
    "location_type",
];

/// What a `stop/update` changes; `lat` and `lon` always go together.
/// `parent_station` is compared only where neither side's parent is a station:
/// a station's platforms move with the station's members.
const STOP_UPDATE_FIELDS: &[&str] = &[
    "name",
    "lat",
    "lon",
    "platform_code",
    "description",
    "cluster_id",
    "regional_name",
    "hindi_name",
    "unserviceable",
    "tts_stop_name",
    "zone_id",
    "stop_url",
    "stop_timezone",
    "wheelchair_boarding",
    "level_id",
    "stop_access",
    "location_type",
    "parent_station",
];

const ROUTE_FIELDS: &[&str] = &[
    "route_id",
    "short_name",
    "long_name",
    "route_type",
    "color",
    "text_color",
    "agency_id",
    "active",
    "encoded_polyline",
    "polyline_source",
    "schedule_source",
    "route_desc",
    "route_url",
    "route_sort_order",
    "continuous_pickup",
    "continuous_drop_off",
    "network_id",
    "deleted",
];

/// What a `route/create` takes besides the route's id and short name.
const ROUTE_CREATE_FIELDS: &[&str] = &[
    "long_name",
    "route_type",
    "color",
    "text_color",
    "agency_id",
    "route_desc",
    "route_url",
    "route_sort_order",
    "continuous_pickup",
    "continuous_drop_off",
    "network_id",
];

/// What a `route/update` changes.
const ROUTE_UPDATE_FIELDS: &[&str] = &[
    "short_name",
    "long_name",
    "route_type",
    "color",
    "text_color",
    "agency_id",
    "active",
    "encoded_polyline",
    "polyline_source",
    "schedule_source",
    "route_desc",
    "route_url",
    "route_sort_order",
    "continuous_pickup",
    "continuous_drop_off",
    "network_id",
];

fn pick(row: &Value, fields: &[&str]) -> Value {
    let mut out = Map::new();
    for f in fields {
        out.insert(f.to_string(), row.get(*f).cloned().unwrap_or(Value::Null));
    }
    Value::Object(out)
}

/// Rows of `SELECT to_jsonb(t)::text ...`, as JSON (this build of sqlx decodes
/// no JSON, so a row crosses as text).
async fn json_rows(conn: &mut PgConnection, sql: &str, g: &str) -> EditorResult<Vec<Value>> {
    let texts: Vec<String> = sqlx::query_scalar(sql)
        .bind(g)
        .fetch_all(&mut *conn)
        .await?;
    texts
        .iter()
        .map(|t| {
            serde_json::from_str(t)
                .map_err(|e| EditorError::internal(format!("a stored row is not JSON: {e}")))
        })
        .collect()
}

/// Every stop row of the feed, deleted ones too, with `merged_into` where a
/// merge retired it.
pub async fn read_stops(conn: &mut PgConnection, g: &str) -> EditorResult<Vec<Value>> {
    let rows = json_rows(
        conn,
        "SELECT to_jsonb(t)::text FROM gtfs_stop t WHERE t.gtfs_id = $1 ORDER BY t.stop_id",
        g,
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let mut v = pick(r, STOP_FIELDS);
            v["merged_into"] = r["provenance"]["merged_into"].clone();
            v
        })
        .collect())
}

pub async fn read_routes(conn: &mut PgConnection, g: &str) -> EditorResult<Vec<Value>> {
    let rows = json_rows(
        conn,
        "SELECT to_jsonb(t)::text FROM gtfs_route t WHERE t.gtfs_id = $1 ORDER BY t.route_id",
        g,
    )
    .await?;
    Ok(rows.iter().map(|r| pick(r, ROUTE_FIELDS)).collect())
}

/// Every stage of the feed, deleted ones too, with its rows in order.
pub async fn read_stages(conn: &mut PgConnection, g: &str) -> EditorResult<Vec<Value>> {
    json_rows(
        conn,
        "SELECT json_build_object('stage_id', s.stage_id, 'direction', s.direction, \
                'name', s.name, 'description', s.description, 'deleted', s.deleted, \
                'rows', coalesce((SELECT json_agg(json_build_object( \
                        'stop_id', ss.stop_id, 'stop_type', ss.stop_type, \
                        'marker_id', ss.marker_id, 'marker_name', ss.marker_name, \
                        'marker_lat', ss.marker_lat, 'marker_lon', ss.marker_lon, \
                        'stop_name_override', ss.stop_name_override) ORDER BY ss.position) \
                    FROM gtfs_stage_stop ss \
                    WHERE ss.gtfs_id = s.gtfs_id AND ss.stage_id = s.stage_id \
                      AND ss.direction = s.direction), '[]'::json))::text \
         FROM gtfs_stage s WHERE s.gtfs_id = $1 ORDER BY s.stage_id, s.direction",
        g,
    )
    .await
}

/// Every route's normal list of stages (a temporary route's list stays where
/// it is), as `{route_id, stages: [{stage_id, direction, stage_no}]}`.
pub async fn read_lists(conn: &mut PgConnection, g: &str) -> EditorResult<Vec<Value>> {
    json_rows(
        conn,
        "SELECT json_build_object('route_id', route_id, 'stages', json_agg(json_build_object( \
                'stage_id', stage_id, 'direction', direction, 'stage_no', stage_no) \
                ORDER BY position))::text \
         FROM gtfs_route_stage WHERE gtfs_id = $1 AND variant_id IS NULL \
         GROUP BY route_id ORDER BY route_id",
        g,
    )
    .await
}

/// A feed's state as a full migration sets master's beside prod's.
#[derive(Debug, Default)]
pub struct State {
    pub stops: BTreeMap<String, Value>,
    pub routes: BTreeMap<String, Value>,
    /// By `<stage_id>|<direction>`.
    pub stages: BTreeMap<String, Value>,
    /// route -> its normal list of stages.
    pub lists: BTreeMap<String, Vec<Link>>,
    pub use_stages: bool,
}

/// One stage of a route's list, as both sides compare it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub stage_id: String,
    pub direction: String,
    pub stage_no: i32,
}

impl Link {
    fn key(&self) -> String {
        format!("{}|{}", self.stage_id, self.direction)
    }
}

fn by_key(rows: Vec<Value>, key: impl Fn(&Value) -> String) -> BTreeMap<String, Value> {
    rows.into_iter().map(|r| (key(&r), r)).collect()
}

fn stage_key_of(v: &Value) -> String {
    format!(
        "{}|{}",
        v["stage_id"].as_str().unwrap_or(""),
        v["direction"].as_str().unwrap_or("")
    )
}

fn lists_of(rows: Vec<Value>) -> BTreeMap<String, Vec<Link>> {
    rows.into_iter()
        .map(|r| {
            let links = r["stages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|l| Link {
                            stage_id: l["stage_id"].as_str().unwrap_or("").to_string(),
                            direction: l["direction"].as_str().unwrap_or("").to_string(),
                            stage_no: l["stage_no"].as_i64().unwrap_or(0) as i32,
                        })
                        .collect()
                })
                .unwrap_or_default();
            (r["route_id"].as_str().unwrap_or("").to_string(), links)
        })
        .collect()
}

impl State {
    fn from_files(pkg: &Package) -> EditorResult<State> {
        let has = |s: &str| pkg.manifest.scopes.iter().any(|x| x == s);
        let mut st = State {
            use_stages: pkg.manifest.source.use_stages,
            ..State::default()
        };
        if has("stops") {
            st.stops = by_key(pkg.json("stops.json")?, |r| {
                r["stop_id"].as_str().unwrap_or("").to_string()
            });
        }
        if has("routes") {
            st.routes = by_key(pkg.json("routes.json")?, |r| {
                r["route_id"].as_str().unwrap_or("").to_string()
            });
        }
        if has("stages") {
            st.stages = by_key(pkg.json("stages.json")?, stage_key_of);
            st.lists = lists_of(pkg.json("route_stages.json")?);
        }
        Ok(st)
    }

    async fn read(conn: &mut PgConnection, g: &str, scopes: &[String]) -> EditorResult<State> {
        let has = |s: &str| scopes.iter().any(|x| x == s);
        let mut st = State {
            use_stages: stages::feed_uses_stages(conn, g).await?,
            ..State::default()
        };
        if has("stops") {
            st.stops = by_key(read_stops(conn, g).await?, |r| {
                r["stop_id"].as_str().unwrap_or("").to_string()
            });
        }
        if has("routes") {
            st.routes = by_key(read_routes(conn, g).await?, |r| {
                r["route_id"].as_str().unwrap_or("").to_string()
            });
        }
        if has("stages") {
            st.stages = by_key(read_stages(conn, g).await?, stage_key_of);
            st.lists = lists_of(read_lists(conn, g).await?);
        }
        Ok(st)
    }
}

// ---------------------------------------------------------------- import

/// What an import may do.
#[derive(Debug, Clone, Default)]
pub struct ImportOpts {
    pub dry_run: bool,
    /// A full migration: the prod version after which what prod changed counts
    /// as prod's own edit. Needed by the first full migration of a feed; later
    /// ones default to the one it named.
    pub since_version: Option<i64>,
    /// Conflicts to take master's side of anyway, by their `id`.
    pub take: BTreeSet<String>,
    /// Take master's side of every conflict.
    pub take_all: bool,
}

impl ImportOpts {
    fn takes(&self, id: &str) -> bool {
        self.take_all || self.take.contains(id)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct SetReport {
    /// None on a dry run, where nothing is kept.
    pub change_set_id: Option<Uuid>,
    pub title: String,
    pub changes: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct StepReport {
    pub step: &'static str,
    pub title: &'static str,
    /// What the step would change now, by kind of change.
    pub planned: BTreeMap<String, usize>,
    pub conflicts: usize,
    /// Whether this run drafted it.
    pub drafted: bool,
    /// Changes this run took out because prod refused them.
    pub dropped: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub import_id: Option<Uuid>,
    pub gtfs_id: String,
    pub kind: String,
    pub dry_run: bool,
    pub file_sha256: String,
    pub source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_set: Option<SourceSet>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_version: Option<i64>,
    /// A full migration: the step this run drafted, and what every step would do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepReport>,
    pub change_sets: Vec<SetReport>,
    /// added, done, conflicts, taken, dropped, not_migrated, ...
    pub counts: BTreeMap<String, usize>,
    /// Left out because prod has something else: `{id, ...}`, takeable by id.
    pub conflicts: Vec<Value>,
    /// Taken out because prod refused them: `{id, what, findings}`.
    pub dropped: Vec<Value>,
    /// What a migration does not move, and what only prod has.
    pub notes: Vec<Value>,
    /// Errors that are no change's: nothing is written while there are any.
    pub problems: Vec<Value>,
    pub warnings: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timetable: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

impl Report {
    fn count(&mut self, what: &str, n: usize) {
        *self.counts.entry(what.to_string()).or_default() += n;
    }

    fn listed(list: &mut Vec<Value>, v: Value) {
        if list.len() < LISTED {
            list.push(v);
        }
    }

    fn conflict(&mut self, v: Value) {
        self.count("conflicts", 1);
        Self::listed(&mut self.conflicts, v);
    }

    fn note(&mut self, v: Value) {
        Self::listed(&mut self.notes, v);
    }
}

/// Read a migration file into feed `g` (section 23): checked, then drafted as
/// its kind says. Admin only, and only where `is_master` is false.
pub async fn import(
    pool: &PgPool,
    is_master: bool,
    ctx: &Ctx,
    g: &str,
    bytes: &[u8],
    opts: &ImportOpts,
) -> EditorResult<Report> {
    ctx.require_admin()?;
    if is_master {
        return Err(EditorError::bad_request(
            "import_on_master",
            "this is master: a migration from master is read in on prod",
        ));
    }
    let pkg = read_package(bytes)?;
    let m = &pkg.manifest;
    if m.source.env != "master" {
        return Err(EditorError::bad_request(
            "not_from_master",
            format!(
                "this file was written on {}; only a file from master is read in",
                m.source.env
            ),
        ));
    }
    if m.gtfs_id != g {
        return Err(EditorError::bad_request(
            "feed_mismatch",
            format!("this file is feed {}'s, not feed {g}'s", m.gtfs_id),
        ));
    }
    match m.kind.as_str() {
        "change_set" => import_change_set(pool, ctx, g, &pkg, opts).await,
        "full" => import_full(pool, ctx, g, &pkg, opts).await,
        other => Err(invalid(format!("kind {other:?} is not change_set or full"))),
    }
}

/// A new draft of the importing admin's, based on the feed as it is.
async fn new_set(
    conn: &mut PgConnection,
    g: &str,
    ctx: &Ctx,
    title: &str,
    description: &str,
) -> EditorResult<ChangeSet> {
    let version = service::feed_version(conn, g).await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO gtfs_change_set (gtfs_id, title, description, created_by, base_version) \
         VALUES ($1, $2, $3, $4, $5) RETURNING change_set_id",
    )
    .bind(g)
    .bind(title)
    .bind(description)
    .bind(ctx.user.user_id)
    .bind(version)
    .fetch_one(&mut *conn)
    .await?;
    service::load_set(conn, id, true).await
}

async fn savepoint(conn: &mut PgConnection, name: &str) -> EditorResult<()> {
    sqlx::query(&format!("SAVEPOINT {name}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn release(conn: &mut PgConnection, name: &str) -> EditorResult<()> {
    sqlx::query(&format!("RELEASE SAVEPOINT {name}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn roll_back_to(conn: &mut PgConnection, name: &str) -> EditorResult<()> {
    sqlx::query(&format!("ROLLBACK TO SAVEPOINT {name}"))
        .execute(&mut *conn)
        .await?;
    release(conn, name).await
}

/// Whether a change the import tried to add was refused for what it is (its
/// shape, a row it names), which drops it, rather than for a failure that
/// stops the import.
fn refused(e: &EditorError) -> bool {
    e.status.is_client_error()
}

/// One change an import wants in a draft, under the id a report names it by.
struct Item {
    id: String,
    what: Value,
    change: NewChange,
}

/// Add `items` to `set` in order, replay the draft, and take out every change
/// the replay finds an error or a conflict in - and again, until it replays
/// clean. Whatever is taken out is reported as dropped, with what was found.
/// Returns how many changes the draft keeps. The caller's transaction holds
/// the feed lock.
async fn add_and_settle(
    conn: &mut PgConnection,
    ctx: &Ctx,
    set: &ChangeSet,
    g: &str,
    items: &[Item],
    report: &mut Report,
) -> EditorResult<usize> {
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    for _ in 0..MAX_ROUNDS {
        savepoint(conn, "migration_round").await?;
        let mut added: HashMap<i64, usize> = HashMap::new();
        for (i, item) in items.iter().enumerate() {
            if out.contains_key(&item.id) {
                continue;
            }
            savepoint(conn, "migration_add").await?;
            match service::add_change_to(conn, ctx, set, item.change.clone()).await {
                Ok(id) => {
                    release(conn, "migration_add").await?;
                    added.insert(id, i);
                }
                Err(e) if refused(&e) => {
                    roll_back_to(conn, "migration_add").await?;
                    out.insert(
                        item.id.clone(),
                        vec![json!({"level": "error", "code": e.code, "message": e.message})],
                    );
                }
                Err(e) => return Err(e),
            }
        }
        if added.is_empty() {
            release(conn, "migration_round").await?;
            break;
        }
        let all = service::load_changes_to_apply(conn, set.change_set_id).await?;
        savepoint(conn, "migration_replay").await?;
        let ev = service::evaluate(conn, g, &all, &ctx.user.email).await;
        roll_back_to(conn, "migration_replay").await?;
        let ev = ev?;
        let mut bad: HashMap<i64, Vec<Value>> = HashMap::new();
        let mut global = Vec::new();
        for v in ev.validation.iter().chain(&ev.conflicts) {
            let error = v.get("level").is_none() || v["level"] == "error";
            if !error {
                continue;
            }
            match v["change_id"].as_i64() {
                Some(id) => bad.entry(id).or_default().push(v.clone()),
                None => global.push(v.clone()),
            }
        }
        if bad.is_empty() {
            report.warnings += ev
                .validation
                .iter()
                .filter(|v| v["level"] == "warning")
                .count();
            for v in global {
                Report::listed(&mut report.problems, v);
            }
            release(conn, "migration_round").await?;
            break;
        }
        roll_back_to(conn, "migration_round").await?;
        for (id, found) in bad {
            if let Some(&i) = added.get(&id) {
                out.insert(items[i].id.clone(), found);
            }
        }
    }
    let mut dropped = 0;
    for item in items {
        if let Some(found) = out.get(&item.id) {
            dropped += 1;
            Report::listed(
                &mut report.dropped,
                json!({"id": item.id, "what": item.what, "findings": found}),
            );
        }
    }
    report.count("dropped", dropped);
    Ok(items.len() - dropped)
}

/// What a change set row says it is, for a report.
fn what_of(c: &SourceChange) -> Value {
    json!({"position": c.position, "entity": c.entity, "op": c.op, "entity_key": c.entity_key})
}

/// Record what an import drafted; returns its id.
#[allow(clippy::too_many_arguments)]
async fn record(
    conn: &mut PgConnection,
    import_id: Uuid,
    g: &str,
    pkg: &Package,
    step: Option<&str>,
    since: Option<i64>,
    sets: &[Uuid],
    report: &Report,
    ctx: &Ctx,
) -> EditorResult<()> {
    let summary = json!({
        "counts": report.counts,
        "change_sets": report.change_sets,
        "conflicts": report.conflicts.iter().take(100).collect::<Vec<_>>(),
        "dropped": report.dropped.iter().take(100).collect::<Vec<_>>(),
        "steps": report.steps,
    });
    sqlx::query(
        "INSERT INTO gtfs_migration_import (import_id, gtfs_id, kind, step, source, file_sha256, \
                                            since_version, change_set_ids, report, imported_by) \
         VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9::jsonb, $10)",
    )
    .bind(import_id)
    .bind(g)
    .bind(&pkg.manifest.kind)
    .bind(step)
    .bind(
        json!({"source": pkg.manifest.source, "change_set": pkg.manifest.change_set,
               "scopes": pkg.manifest.scopes})
        .to_string(),
    )
    .bind(&pkg.sha256)
    .bind(since)
    .bind(sets)
    .bind(summary.to_string())
    .bind(ctx.user.user_id)
    .execute(&mut *conn)
    .await?;
    auth::audit(
        &mut *conn,
        Some(ctx.user.user_id),
        Some(&ctx.user.email),
        "migration_imported",
        Some(g),
        None,
        json!({"import_id": import_id, "kind": pkg.manifest.kind, "step": step,
               "source_version": pkg.manifest.source.version,
               "change_set_ids": sets, "counts": report.counts}),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- one draft

/// Earlier imports of master draft `source` that left a draft not discarded.
async fn earlier_imports(
    conn: &mut PgConnection,
    g: &str,
    source: Uuid,
) -> EditorResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT m.import_id, m.imported_at, \
                array_agg(s.change_set_id ORDER BY s.created_at) AS sets, \
                array_agg(s.status ORDER BY s.created_at) AS statuses \
         FROM gtfs_migration_import m \
         JOIN gtfs_change_set s ON s.change_set_id = ANY (m.change_set_ids) \
         WHERE m.gtfs_id = $1 AND m.kind = 'change_set' \
           AND m.source -> 'change_set' ->> 'change_set_id' = $2 AND s.status <> 'discarded' \
         GROUP BY m.import_id, m.imported_at ORDER BY m.imported_at",
    )
    .bind(g)
    .bind(source.to_string())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(json!({
                "import_id": r.try_get::<Uuid, _>("import_id")?,
                "imported_at": r.try_get::<DateTime<Utc>, _>("imported_at")?,
                "change_set_ids": r.try_get::<Vec<Uuid>, _>("sets")?,
                "statuses": r.try_get::<Vec<String>, _>("statuses")?,
            }))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

async fn import_change_set(
    pool: &PgPool,
    ctx: &Ctx,
    g: &str,
    pkg: &Package,
    opts: &ImportOpts,
) -> EditorResult<Report> {
    let m = &pkg.manifest;
    let src = m
        .change_set
        .clone()
        .ok_or_else(|| invalid("the manifest names no draft"))?;
    let mut changes: Vec<SourceChange> = pkg.json("changes.json")?;
    changes.sort_by_key(|c| c.position);
    let mut report = Report {
        gtfs_id: g.to_string(),
        kind: m.kind.clone(),
        dry_run: opts.dry_run,
        file_sha256: pkg.sha256.clone(),
        source: Some(m.source.clone()),
        change_set: Some(src.clone()),
        ..Report::default()
    };

    let mut tx = pool.begin().await?;
    lock_feed(&mut tx, g).await?;
    let earlier = earlier_imports(&mut tx, g, src.change_set_id).await?;
    if !earlier.is_empty() {
        if !opts.dry_run {
            return Err(EditorError::conflict(
                "already_imported",
                format!(
                    "master draft {} was already brought to prod; discard that draft first to bring it again",
                    src.change_set_id
                ),
            )
            .with_details(json!({"imports": earlier})));
        }
        report.note(json!({"code": "already_imported", "imports": earlier}));
    }

    let import_id = Uuid::new_v4();
    let title = format!("From master: {}", src.title.trim());
    let description = format!(
        "Brought from master's committed draft {} (v{}{}) by migration {import_id}, \
         docs/gtfs-editor.md section 23.",
        src.change_set_id,
        src.committed_version.unwrap_or(0),
        src.committed_by
            .as_deref()
            .map(|e| format!(", committed by {e}"))
            .unwrap_or_default(),
    );
    let set = new_set(&mut tx, g, ctx, &title, &description).await?;

    // Classify each change against prod as it stands with the changes before it
    // in this draft: a change can need one made earlier in the same draft.
    let mut items = Vec::new();
    let mut done = 0;
    let mut taken = 0;
    let mut conflicts = Vec::new();
    savepoint(&mut tx, "migration_classify").await?;
    for c in &changes {
        if c.entity == "feed_config" {
            report.count("not_migrated", 1);
            report.note(json!({"code": "feed_setting", "what": what_of(c),
                               "message": "feed settings stay each environment's own"}));
            continue;
        }
        let id = format!("change:{}", c.position);
        let verdict = classify(&mut tx, set.change_set_id, g, c).await?;
        let rebase = match verdict {
            Verdict::Apply => None,
            Verdict::Done(why) => {
                done += 1;
                let _ = why;
                continue;
            }
            Verdict::Conflict {
                reason,
                prod,
                master,
                rebase,
            } => {
                if !opts.takes(&id) {
                    conflicts.push(json!({"id": id, "what": what_of(c), "reason": reason,
                                          "prod": prod, "master": master}));
                    continue;
                }
                taken += 1;
                Some(rebase)
            }
        };
        let change = prepare(c, rebase.flatten());
        // add it now, so the next change is classified with it in the draft
        savepoint(&mut tx, "migration_add").await?;
        match service::add_change_to(&mut tx, ctx, &set, change.clone()).await {
            Ok(_) => release(&mut tx, "migration_add").await?,
            Err(e) if refused(&e) => roll_back_to(&mut tx, "migration_add").await?,
            Err(e) => return Err(e),
        }
        items.push(Item {
            id,
            what: what_of(c),
            change,
        });
    }
    // the draft is rebuilt from `items` by add_and_settle
    roll_back_to(&mut tx, "migration_classify").await?;
    report.count("done", done);
    report.count("taken", taken);
    for c in conflicts {
        report.conflict(c);
    }

    let kept = add_and_settle(&mut tx, ctx, &set, g, &items, &mut report).await?;
    report.count("added", kept);
    if kept > 0 {
        report.change_sets.push(SetReport {
            change_set_id: (!opts.dry_run).then_some(set.change_set_id),
            title: title.clone(),
            changes: kept,
        });
    }
    if opts.dry_run || kept == 0 || !report.problems.is_empty() {
        tx.rollback().await?;
        if kept == 0 && !opts.dry_run {
            report.next = Some(
                "nothing to bring: prod already has this draft, or every change was left out"
                    .into(),
            );
        }
        return Ok(report);
    }
    record(
        &mut tx,
        import_id,
        g,
        pkg,
        None,
        None,
        &[set.change_set_id],
        &report,
        ctx,
    )
    .await?;
    tx.commit().await?;
    report.import_id = Some(import_id);
    report.next = Some("review the draft on prod; a second person approves and commits it".into());
    Ok(report)
}

/// How a master change sits on prod.
enum Verdict {
    /// Prod has what master had when the change was made: it goes in as it is.
    Apply,
    /// Prod already has what the change makes.
    Done(&'static str),
    /// Prod has something else. `rebase` is what the change must carry to go in
    /// anyway - the hash it is then based on - when it carries one.
    Conflict {
        reason: &'static str,
        prod: Value,
        master: Value,
        rebase: Option<(&'static str, String)>,
    },
}

fn conflict(reason: &'static str, prod: Value, master: Value) -> Verdict {
    Verdict::Conflict {
        reason,
        prod,
        master,
        rebase: None,
    }
}

/// A change carrying a hash of what it was based on: in when prod's matches,
/// done when prod already has what it makes, a conflict otherwise.
fn by_hash(field: &'static str, prod: String, base: Option<&str>, made: Option<String>) -> Verdict {
    if base == Some(prod.as_str()) {
        return Verdict::Apply;
    }
    if made.as_deref() == Some(prod.as_str()) {
        return Verdict::Done("prod already has it");
    }
    Verdict::Conflict {
        reason: "changed_on_prod",
        prod: json!({field: prod}),
        master: json!({field: base}),
        rebase: Some((field, prod)),
    }
}

/// A list of stages as `route_links` reads it, however the change spells it.
fn normal_links(v: &Value) -> Option<Vec<StageLink>> {
    let links: Vec<StageLink> = serde_json::from_value(v.clone()).ok()?;
    Some(
        links
            .into_iter()
            .zip(super::validation::stage_numbers(
                &serde_json::from_value::<Vec<StageLink>>(v.clone()).ok()?,
            ))
            .map(|(l, n)| {
                let k = StageKey::parse(l.stage_id.trim());
                let direction = if l.stage_id.contains('|') {
                    k.direction
                } else {
                    l.direction.unwrap_or_default()
                };
                StageLink {
                    stage_id: k.stage_id,
                    direction: Some(direction),
                    stage_no: Some(n),
                }
            })
            .collect(),
    )
}

/// Fields of a change's `after` that say what it is based on, or what it is
/// for, rather than what it sets.
const CONTROL: &[&str] = &[
    "base_rows_hash",
    "base_stages_hash",
    "base_trips_hash",
    "base_hash",
    "into_row_version",
    "position_review_id",
    "proposal_id",
    "pattern_key",
    "profile_key",
];

/// A field of a snapshot as a comparison sees it: the field itself, or under
/// `gtfs` where a stop or route keeps its other GTFS fields; a station's
/// members as their sorted ids; a stage's rows without what is read from the
/// stops they name.
fn field_of(snap: &Value, f: &str) -> Value {
    match f {
        "member_stop_ids" | "members" => {
            let mut ids: Vec<String> = snap["member_stop_ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .or_else(|| {
                    snap["members"].as_array().map(|a| {
                        a.iter()
                            .filter_map(|m| m["stop_id"].as_str().map(str::to_string))
                            .collect()
                    })
                })
                .unwrap_or_default();
            ids.sort();
            json!(ids)
        }
        "rows" => match snap["rows"].as_array() {
            Some(rows) => Value::Array(
                rows.iter()
                    .map(|r| {
                        pick(
                            r,
                            &[
                                "stop_id",
                                "stop_type",
                                "marker_id",
                                "marker_name",
                                "marker_lat",
                                "marker_lon",
                                "stop_name_override",
                            ],
                        )
                    })
                    .collect(),
            ),
            None => Value::Null,
        },
        _ => match snap.get(f) {
            Some(v) => v.clone(),
            None => snap["gtfs"].get(f).cloned().unwrap_or(Value::Null),
        },
    }
}

/// The id fields of a create: prod's row is found by them, and holds them by
/// another name (a station's id is its `stop_id`).
const ID_FIELDS: &[&str] = &[
    "stop_id",
    "station_id",
    "route_id",
    "stage_id",
    "service_id",
];

/// Whether prod's row already has every field a create sets.
fn has_all(prod: &Value, after: &Value) -> bool {
    let Some(m) = after.as_object() else {
        return false;
    };
    m.keys()
        .map(String::as_str)
        .filter(|k| !CONTROL.contains(k) && !ID_FIELDS.contains(k))
        .all(|f| {
            let mut wanted = Map::new();
            wanted.insert(f.to_string(), after[f].clone());
            field_of(prod, f) == field_of(&Value::Object(wanted), f)
        })
}

/// An update set beside prod: in when prod has, in every field it sets, what
/// master had; done when prod already has what it sets.
fn by_fields(prod: &Value, before: &Value, after: &Value) -> Verdict {
    let fields: Vec<&str> = after
        .as_object()
        .map(|m| {
            m.keys()
                .map(String::as_str)
                .filter(|k| !CONTROL.contains(k))
                .collect()
        })
        .unwrap_or_default();
    let differs: Vec<&str> = fields
        .iter()
        .copied()
        .filter(|f| field_of(prod, f) != field_of(before, f))
        .collect();
    if differs.is_empty() {
        return Verdict::Apply;
    }
    let made = |f: &str| {
        let mut wanted = Map::new();
        wanted.insert(f.to_string(), after[f].clone());
        field_of(&Value::Object(wanted), f)
    };
    if fields.iter().all(|f| field_of(prod, f) == made(f)) {
        return Verdict::Done("prod already has it");
    }
    let side = |snap: &Value| {
        Value::Object(
            differs
                .iter()
                .map(|f| (f.to_string(), field_of(snap, f)))
                .collect(),
        )
    };
    conflict("changed_on_prod", side(prod), side(before))
}

/// How master's change `c` sits on prod, with the changes added before it to
/// draft `set` counting as there.
async fn classify(
    conn: &mut PgConnection,
    set: Uuid,
    g: &str,
    c: &SourceChange,
) -> EditorResult<Verdict> {
    let key = c.entity_key.as_str();
    let after = &c.after;
    let base = |f: &str| after[f].as_str().map(str::to_string);
    match (c.entity.as_str(), c.op.as_str()) {
        ("route_stops", "replace") => {
            let rows = service::load_pattern_rows(conn, g, key, service::pattern_of(after)).await?;
            let made = serde_json::from_value::<Vec<RouteRow>>(after["rows"].clone())
                .ok()
                .map(|r| service::rows_hash(&r));
            Ok(by_hash(
                "base_rows_hash",
                service::rows_hash(&rows),
                base("base_rows_hash").as_deref(),
                made,
            ))
        }
        ("route_stages", "replace") => {
            let live = stages::live_links_hash(conn, g, key, None).await?;
            let made = normal_links(&after["stages"]).map(|l| stages::links_hash(&l));
            Ok(by_hash(
                "base_stages_hash",
                live,
                base("base_stages_hash").as_deref(),
                made,
            ))
        }
        ("route_trips", "replace") => {
            let live =
                super::trips::trips_hash(&super::trips::load_route_trips(conn, g, key).await?);
            Ok(by_hash(
                "base_trips_hash",
                live,
                base("base_trips_hash").as_deref(),
                None,
            ))
        }
        ("timing_profile", "replace") => {
            let pattern = service::pattern_of(after);
            let profile = after["profile_key"].as_i64();
            let live = super::trips::load_profiles(conn, g, key, Some(pattern))
                .await?
                .into_iter()
                .find(|p| Some(p.profile_key as i64) == profile);
            let expected = base("base_hash").unwrap_or_else(super::trips::empty_hash);
            Ok(by_hash(
                "base_hash",
                super::trips::profile_hash(live.as_ref()),
                Some(&expected),
                None,
            ))
        }
        ("stop" | "station", "create") => Ok(match service::stop_row(conn, g, key).await? {
            None => Verdict::Apply,
            Some(row) if row["deleted"] == true => conflict("deleted_on_prod", row, after.clone()),
            Some(row) if has_all(&row, after) => Verdict::Done("prod already has it"),
            Some(row) => conflict("exists_on_prod", row, after.clone()),
        }),
        ("route", "create") => Ok(match service::route_row(conn, g, key).await? {
            None => Verdict::Apply,
            Some(row) if row["deleted"] == true => conflict("deleted_on_prod", row, after.clone()),
            Some(row) if has_all(&row, after) => Verdict::Done("prod already has it"),
            Some(row) => conflict("exists_on_prod", row, after.clone()),
        }),
        ("stage", "create" | "split") => {
            let sk = StageKey::new(key, after["direction"].as_str().filter(|d| !d.is_empty()));
            Ok(match stages::load_stage(conn, g, &sk, false).await? {
                None => Verdict::Apply,
                Some(s) if s.deleted => conflict(
                    "deleted_on_prod",
                    json!({"stage": sk.entity_key()}),
                    after.clone(),
                ),
                Some(s) if c.op == "create" && s.name == after["name"].as_str().unwrap_or("") => {
                    Verdict::Done("prod already has it")
                }
                Some(_) => conflict(
                    "exists_on_prod",
                    json!({"stage": sk.entity_key()}),
                    after.clone(),
                ),
            })
        }
        ("stop" | "station", "merge") => {
            let into_field = if c.entity == "station" {
                "into_station_id"
            } else {
                "into_stop_id"
            };
            let into = after[into_field].as_str().unwrap_or("").trim().to_string();
            if let Some(row) = service::stop_row(conn, g, key).await? {
                if row["deleted"] == true {
                    return Ok(
                        if row["provenance"]["merged_into"].as_str() == Some(into.as_str()) {
                            Verdict::Done("prod already merged it")
                        } else {
                            conflict("deleted_on_prod", row, c.before.clone())
                        },
                    );
                }
            }
            let mut a = after.clone();
            Ok(
                match service::snapshot(conn, set, g, &c.entity, &c.op, key, &mut a).await {
                    Ok(_) => Verdict::Apply,
                    Err(e) if refused(&e) => conflict(
                        "missing_on_prod",
                        json!({"code": e.code, "message": e.message}),
                        c.before.clone(),
                    ),
                    Err(e) => return Err(e),
                },
            )
        }
        ("stage", "merge") => {
            let gone = stages::key_of_change(conn, g, key).await;
            let into =
                stages::key_of_change(conn, g, after["into_stage_id"].as_str().unwrap_or("")).await;
            match stages::load_stage(conn, g, &gone, false).await? {
                None => return Ok(conflict("missing_on_prod", Value::Null, c.before.clone())),
                Some(s) if s.deleted => return Ok(Verdict::Done("prod already took it out")),
                Some(_) => {}
            }
            Ok(match stages::load_stage(conn, g, &into, false).await? {
                Some(s) if !s.deleted => Verdict::Apply,
                _ => conflict(
                    "missing_on_prod",
                    json!({"into_stage_id": into.entity_key()}),
                    c.before.clone(),
                ),
            })
        }
        (_, "delete") => {
            let mut a = after.clone();
            Ok(
                match service::snapshot(conn, set, g, &c.entity, &c.op, key, &mut a).await {
                    Ok((row, _)) if row["deleted"] == true => {
                        Verdict::Done("prod already took it out")
                    }
                    Ok(_) => Verdict::Apply,
                    Err(e) if e.status == StatusCode::NOT_FOUND => {
                        Verdict::Done("prod does not have it")
                    }
                    Err(e) if refused(&e) => conflict(
                        "differs_on_prod",
                        json!({"code": e.code, "message": e.message}),
                        c.before.clone(),
                    ),
                    Err(e) => return Err(e),
                },
            )
        }
        (_, "update") => {
            let mut a = after.clone();
            Ok(
                match service::snapshot(conn, set, g, &c.entity, &c.op, key, &mut a).await {
                    Ok((row, _)) => by_fields(&row, &c.before, after),
                    Err(e) if refused(&e) => conflict(
                        "missing_on_prod",
                        json!({"code": e.code, "message": e.message}),
                        c.before.clone(),
                    ),
                    Err(e) => return Err(e),
                },
            )
        }
        // everything else - a service or record created, a temporary route -
        // goes in, and the replay says what prod makes of it
        _ => Ok(Verdict::Apply),
    }
}

/// Master's change as prod takes it: what only master's database means taken
/// out (a review or proposal id, the version of the stop a merge keeps), and a
/// conflict taken anyway based on prod's hash instead of master's.
fn prepare(c: &SourceChange, rebase: Option<(&'static str, String)>) -> NewChange {
    let mut after = c.after.clone();
    if let Some(m) = after.as_object_mut() {
        for f in ["position_review_id", "proposal_id", "into_row_version"] {
            m.remove(f);
        }
        if let Some((field, hash)) = rebase {
            m.insert(field.to_string(), json!(hash));
        }
    }
    NewChange {
        entity: c.entity.clone(),
        op: c.op.clone(),
        entity_key: c.entity_key.clone(),
        after,
        base_row_version: None,
    }
}

// ---------------------------------------------------------------- the whole feed

/// What a full migration does, in order: each step needs the one before it
/// committed.
const STEPS: &[(&str, &str)] = &[
    ("stops_routes", "Stops, stations and routes"),
    ("stages", "Stages"),
    ("route_stages", "The routes' stage lists"),
    ("switch", "Serve the feed from its stages"),
    ("timetables", "Timetables"),
];

/// A row of prod's history, as a full migration reads it: what a committed
/// change touched, and at which version.
#[derive(Default)]
struct History {
    /// (kind, key) -> the last version a change of prod's own committed it at.
    own: HashMap<(&'static str, String), i64>,
    /// (kind, key) -> the last version a migration's change committed it at.
    migrated: HashMap<(&'static str, String), i64>,
    /// The `since_version` the feed's first full migration named.
    first_since: Option<i64>,
}

impl History {
    /// The version prod changed `key` at itself, when that is after `base` and
    /// after the last migration that set it.
    fn changed_on_prod(&self, kind: &'static str, key: &str, base: i64) -> Option<i64> {
        let k = (kind, key.to_string());
        let own = *self.own.get(&k)?;
        let migrated = self.migrated.get(&k).copied().unwrap_or(i64::MIN);
        (own > base.max(migrated)).then_some(own)
    }
}

/// A stage as a change names it, as `<stage_id>|<direction>`; a bare id is a
/// stage that runs either way, or the one `direction` says for a create.
fn stage_label(key: &str, direction: Option<&str>) -> String {
    if key.contains('|') {
        return key.to_string();
    }
    format!("{key}|{}", direction.unwrap_or(""))
}

/// What a committed change touched, as a full migration keys it: a stop or
/// station, a route, a route's list of stops or stages, a stage, a route's
/// timetable, or a service.
fn touched(
    entity: &str,
    op: &str,
    key: &str,
    after: &Value,
    affected: &Value,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    match (entity, op) {
        ("stop" | "station", "merge") => {
            out.push(("stop", key.to_string()));
            for f in ["into_stop_id", "into_station_id"] {
                if let Some(into) = after[f].as_str() {
                    out.push(("stop", into.to_string()));
                }
            }
            for r in affected.as_array().into_iter().flatten() {
                if let Some(route) = r["route_id"].as_str() {
                    out.push(("route_list", route.to_string()));
                }
            }
        }
        ("stop" | "station", _) => out.push(("stop", key.to_string())),
        ("route", _) => out.push(("route", key.to_string())),
        ("route_stops" | "route_stages" | "route_variant", _) => {
            out.push(("route_list", key.to_string()))
        }
        ("stage", "merge") => {
            out.push(("stage", stage_label(key, None)));
            if let Some(into) = after["into_stage_id"].as_str() {
                out.push(("stage", stage_label(into, None)));
            }
        }
        ("stage", "split") => {
            out.push(("stage", stage_label(key, after["direction"].as_str())));
            if let Some(from) = after["from_stage_id"].as_str() {
                out.push(("stage", stage_label(from, None)));
            }
            for r in after["routes"].as_array().into_iter().flatten() {
                if let Some(route) = r.as_str() {
                    out.push(("route_list", route.to_string()));
                }
            }
        }
        ("stage", "create") => out.push(("stage", stage_label(key, after["direction"].as_str()))),
        ("stage", _) => out.push(("stage", stage_label(key, None))),
        ("pattern" | "timing_profile" | "route_trips", _) => {
            out.push(("timetable", key.to_string()))
        }
        ("service", _) => out.push(("service", key.to_string())),
        _ => {}
    }
    out
}

async fn read_history(conn: &mut PgConnection, g: &str) -> EditorResult<History> {
    let rows = sqlx::query(
        "SELECT s.committed_version, c.entity, c.entity_key, c.op, \
                CASE WHEN c.op IN ('merge', 'split') OR (c.entity = 'stage' AND c.op = 'create') \
                     THEN c.after::text END AS after, \
                CASE WHEN c.entity = 'stop' AND c.op = 'merge' \
                     THEN (c.before -> 'affected')::text END AS affected, \
                EXISTS (SELECT 1 FROM gtfs_migration_import m \
                         WHERE s.change_set_id = ANY (m.change_set_ids)) AS migrated \
         FROM gtfs_change c JOIN gtfs_change_set s ON s.change_set_id = c.change_set_id \
         WHERE s.gtfs_id = $1 AND s.status = 'committed' AND s.committed_version IS NOT NULL",
    )
    .bind(g)
    .fetch_all(&mut *conn)
    .await?;
    let mut h = History::default();
    for r in &rows {
        let version: i64 = r.try_get("committed_version")?;
        let text = |col: &str| -> Result<Value, sqlx::Error> {
            Ok(r.try_get::<Option<String>, _>(col)?
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Value::Null))
        };
        let (after, affected) = (text("after")?, text("affected")?);
        let entity: String = r.try_get("entity")?;
        let op: String = r.try_get("op")?;
        let key: String = r.try_get("entity_key")?;
        let map = if r.try_get::<bool, _>("migrated")? {
            &mut h.migrated
        } else {
            &mut h.own
        };
        for k in touched(&entity, &op, &key, &after, &affected) {
            let at = map.entry(k).or_insert(version);
            *at = (*at).max(version);
        }
    }
    h.first_since = sqlx::query_scalar(
        "SELECT since_version FROM gtfs_migration_import \
         WHERE gtfs_id = $1 AND kind = 'full' AND since_version IS NOT NULL \
         ORDER BY imported_at LIMIT 1",
    )
    .bind(g)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    Ok(h)
}

/// Migration drafts of the feed still open: a full migration plans against
/// what is committed, so it waits for them.
async fn open_migration_sets(conn: &mut PgConnection, g: &str) -> EditorResult<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT s.change_set_id, s.title, s.status FROM gtfs_change_set s \
         WHERE s.gtfs_id = $1 AND s.status IN ('draft', 'submitted', 'approved') \
           AND EXISTS (SELECT 1 FROM gtfs_migration_import m \
                        WHERE m.gtfs_id = $1 AND s.change_set_id = ANY (m.change_set_ids)) \
         ORDER BY s.created_at",
    )
    .bind(g)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(json!({
                "change_set_id": r.try_get::<Uuid, _>("change_set_id")?,
                "title": r.try_get::<String, _>("title")?,
                "status": r.try_get::<String, _>("status")?,
            }))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(Into::into)
}

/// One change a full migration plans.
struct Planned {
    step: usize,
    /// What a conflict on it is taken by: `<kind>:<key>`.
    id: String,
    /// The history keys it overwrites; prod's own edit of any is a conflict.
    guards: Vec<(&'static str, String)>,
    change: NewChange,
    what: Value,
}

fn plan(
    step: usize,
    kind: &'static str,
    key: &str,
    entity: &str,
    op: &str,
    entity_key: &str,
    after: Value,
    what: Value,
) -> Planned {
    Planned {
        step,
        id: format!("{kind}:{key}"),
        guards: vec![(kind, key.to_string())],
        change: NewChange {
            entity: entity.to_string(),
            op: op.to_string(),
            entity_key: entity_key.to_string(),
            after,
            base_row_version: None,
        },
        what,
    }
}

fn is_station(s: &Value) -> bool {
    s["location_type"].as_i64() == Some(1)
}

fn live(s: &Value) -> bool {
    s["deleted"] != true
}

/// Where a merge on master left a stop: the live stop at the end of its
/// `merged_into` chain, if any.
fn survivor<'a>(stops: &'a BTreeMap<String, Value>, id: &str) -> Option<&'a str> {
    let mut at = stops.get_key_value(id)?;
    for _ in 0..64 {
        if live(at.1) {
            return Some(at.0.as_str());
        }
        let next = at.1["merged_into"].as_str()?;
        at = stops.get_key_value(next)?;
    }
    None
}

/// A station's live platforms, by id.
fn members(stops: &BTreeMap<String, Value>, station: &str) -> Vec<String> {
    stops
        .iter()
        .filter(|(_, s)| live(s) && s["parent_station"].as_str() == Some(station))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Whether `parent` is a station in `stops`.
fn station_parent(stops: &BTreeMap<String, Value>, s: &Value) -> bool {
    s["parent_station"]
        .as_str()
        .and_then(|p| stops.get(p))
        .is_some_and(is_station)
}

/// The fields of `fields` master has otherwise than prod, as an update sets
/// them. `lat` and `lon` go together.
fn changed_fields(master: &Value, prod: &Value, fields: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for f in fields {
        if master[*f] != prod[*f] {
            out.insert(f.to_string(), master[*f].clone());
        }
    }
    if out.contains_key("lat") || out.contains_key("lon") {
        out.insert("lat".into(), master["lat"].clone());
        out.insert("lon".into(), master["lon"].clone());
    }
    out
}

fn non_null(master: &Value, fields: &[&str], into: &mut Map<String, Value>) {
    for f in fields {
        if !master[*f].is_null() {
            into.insert(f.to_string(), master[*f].clone());
        }
    }
}

/// Stops, stations and routes: step 0.
fn plan_stops_routes(
    master: &State,
    prod: &State,
    scopes: &[String],
    out: &mut Vec<Planned>,
    report: &mut Report,
) {
    let has = |s: &str| scopes.iter().any(|x| x == s);
    if has("stops") {
        let (ms, ps) = (&master.stops, &prod.stops);
        let mut creates = Vec::new();
        let mut stations = Vec::new();
        let mut updates = Vec::new();
        let mut merges = Vec::new();
        let mut deletes = Vec::new();
        for (id, m) in ms {
            let station = is_station(m);
            let entity = if station { "station" } else { "stop" };
            let what = json!({"entity": entity, "key": id, "name": m["name"]});
            match ps.get(id) {
                None if live(m) && station => {
                    let ids = members(ms, id);
                    if ids.len() < 2 {
                        report.note(json!({"code": "station_too_small", "key": id,
                            "message": "a station groups at least two stops; master's has fewer, so it stays on master"}));
                        continue;
                    }
                    let mut after = Map::new();
                    after.insert("station_id".into(), json!(id));
                    non_null(m, &["name", "lat", "lon", "description"], &mut after);
                    after.insert("member_stop_ids".into(), json!(ids));
                    stations.push(plan(
                        0,
                        "stop",
                        id,
                        "station",
                        "create",
                        id,
                        Value::Object(after),
                        what,
                    ));
                }
                None if live(m) => {
                    let mut after = Map::new();
                    after.insert("stop_id".into(), json!(id));
                    non_null(m, &["name", "lat", "lon"], &mut after);
                    non_null(m, STOP_CREATE_FIELDS, &mut after);
                    if after.get("location_type").and_then(Value::as_i64) == Some(0) {
                        after.remove("location_type");
                    }
                    if !m["parent_station"].is_null() && !station_parent(ms, m) {
                        after.insert("parent_station".into(), m["parent_station"].clone());
                    }
                    creates.push(plan(
                        0,
                        "stop",
                        id,
                        "stop",
                        "create",
                        id,
                        Value::Object(after),
                        what.clone(),
                    ));
                    if m["unserviceable"] == true {
                        updates.push(plan(
                            0,
                            "stop",
                            id,
                            "stop",
                            "update",
                            id,
                            json!({"unserviceable": true}),
                            what,
                        ));
                    }
                }
                // a stop master no longer has that prod never had
                None => {}
                Some(p) if !live(p) && live(m) => {
                    report.conflict(json!({"id": format!("stop:{id}"), "what": what,
                        "reason": "deleted_on_prod",
                        "message": "prod took this stop out; a migration does not bring a stop back"}));
                }
                Some(p) if !live(p) => {}
                Some(p) if live(m) => {
                    if is_station(p) != station {
                        report.note(json!({"code": "kind_differs", "key": id,
                            "message": "a station on one side and a stop on the other; left as prod has it"}));
                        continue;
                    }
                    if station {
                        let mut after =
                            changed_fields(m, p, &["name", "lat", "lon", "description"]);
                        let (mm, pm) = (members(ms, id), members(ps, id));
                        if mm != pm && mm.len() >= 2 {
                            after.insert("member_stop_ids".into(), json!(mm));
                        }
                        if !after.is_empty() {
                            updates.push(plan(
                                0,
                                "stop",
                                id,
                                "station",
                                "update",
                                id,
                                Value::Object(after),
                                what,
                            ));
                        }
                        continue;
                    }
                    let fields: Vec<&str> = STOP_UPDATE_FIELDS
                        .iter()
                        .copied()
                        .filter(|f| {
                            *f != "parent_station"
                                || !(station_parent(ms, m) || station_parent(ps, p))
                        })
                        .filter(|f| {
                            // a stop never becomes a station by an update
                            *f != "location_type"
                                || !(m["location_type"].as_i64() == Some(1)
                                    || p["location_type"].as_i64() == Some(1))
                        })
                        .collect();
                    let after = changed_fields(m, p, &fields);
                    if m["stop_code"] != p["stop_code"] {
                        report.note(json!({"code": "stop_code_differs", "key": id,
                            "master": m["stop_code"], "prod": p["stop_code"],
                            "message": "a stop's code is set when it is made and is not changed by a migration"}));
                    }
                    if !after.is_empty() {
                        updates.push(plan(
                            0,
                            "stop",
                            id,
                            "stop",
                            "update",
                            id,
                            Value::Object(after),
                            what,
                        ));
                    }
                }
                // master took it out: merged into another stop, or deleted
                Some(_) => match m["merged_into"].as_str().and(survivor(ms, id)) {
                    Some(into) if into != id => {
                        let (field, entity) = if station {
                            ("into_station_id", "station")
                        } else {
                            ("into_stop_id", "stop")
                        };
                        let mut p = plan(
                            0,
                            "stop",
                            id,
                            entity,
                            "merge",
                            id,
                            json!({field: into, "keep_name": "into", "keep_position": "into"}),
                            json!({"entity": entity, "key": id, "into": into}),
                        );
                        p.guards.push(("stop", into.to_string()));
                        merges.push(p);
                    }
                    _ => deletes.push(plan(0, "stop", id, entity, "delete", id, Value::Null, what)),
                },
            }
        }
        let only: Vec<&String> = ps.keys().filter(|k| !ms.contains_key(*k)).collect();
        if !only.is_empty() {
            report.note(json!({"code": "prod_only_stops", "count": only.len(),
                "sample": only.iter().take(20).collect::<Vec<_>>(),
                "message": "stops only prod has stay as they are"}));
        }
        out.extend(creates);
        out.extend(stations);
        out.extend(updates);
        out.extend(merges);
        out.extend(deletes);
    }
    if has("routes") {
        let (mr, pr) = (&master.routes, &prod.routes);
        let mut later = Vec::new();
        for (id, m) in mr {
            let what = json!({"entity": "route", "key": id, "short_name": m["short_name"]});
            match pr.get(id) {
                None if live(m) => {
                    let mut after = Map::new();
                    after.insert("route_id".into(), json!(id));
                    non_null(m, &["short_name"], &mut after);
                    non_null(m, ROUTE_CREATE_FIELDS, &mut after);
                    out.push(plan(
                        0,
                        "route",
                        id,
                        "route",
                        "create",
                        id,
                        Value::Object(after),
                        what.clone(),
                    ));
                    // what a create does not take, set by an update in the same draft
                    let mut rest = Map::new();
                    if m["active"] == false {
                        rest.insert("active".into(), json!(false));
                    }
                    non_null(
                        m,
                        &["encoded_polyline", "polyline_source", "schedule_source"],
                        &mut rest,
                    );
                    if !rest.is_empty() {
                        later.push(plan(
                            0,
                            "route",
                            id,
                            "route",
                            "update",
                            id,
                            Value::Object(rest),
                            what,
                        ));
                    }
                }
                None => {}
                Some(p) if !live(p) && live(m) => {
                    report.conflict(json!({"id": format!("route:{id}"), "what": what,
                        "reason": "deleted_on_prod",
                        "message": "prod took this route out; a migration does not bring a route back"}));
                }
                Some(p) if !live(p) => {}
                Some(p) if live(m) => {
                    let after = changed_fields(m, p, ROUTE_UPDATE_FIELDS);
                    if !after.is_empty() {
                        later.push(plan(
                            0,
                            "route",
                            id,
                            "route",
                            "update",
                            id,
                            Value::Object(after),
                            what,
                        ));
                    }
                }
                Some(_) => later.push(plan(
                    0,
                    "route",
                    id,
                    "route",
                    "delete",
                    id,
                    Value::Null,
                    what,
                )),
            }
        }
        let only = pr.keys().filter(|k| !mr.contains_key(*k)).count();
        if only > 0 {
            report.note(json!({"code": "prod_only_routes", "count": only,
                "message": "routes only prod has stay as they are"}));
        }
        out.extend(later);
    }
}

/// Stages (step 1), and the routes' lists of them with the stages master took
/// out (step 2).
fn plan_stages(master: &State, prod: &State, out: &mut Vec<Planned>, report: &mut Report) {
    let (ms, ps) = (&master.stages, &prod.stages);
    let mut deletes = Vec::new();
    for (k, m) in ms {
        let sk = StageKey::parse(k);
        let what = json!({"entity": "stage", "key": k, "name": m["name"]});
        match ps.get(k) {
            None if live(m) => {
                let mut after = Map::new();
                after.insert("stage_id".into(), json!(sk.stage_id));
                after.insert("name".into(), m["name"].clone());
                if !sk.direction.is_empty() {
                    after.insert("direction".into(), json!(sk.direction));
                }
                non_null(m, &["description"], &mut after);
                after.insert("rows".into(), m["rows"].clone());
                out.push(plan(
                    1,
                    "stage",
                    k,
                    "stage",
                    "create",
                    &sk.stage_id,
                    Value::Object(after),
                    what,
                ));
            }
            None => {}
            Some(p) if !live(p) && live(m) => {
                report.conflict(json!({"id": format!("stage:{k}"), "what": what,
                    "reason": "deleted_on_prod",
                    "message": "prod took this stage out; a migration does not bring a stage back"}));
            }
            Some(p) if !live(p) => {}
            Some(p) if live(m) => {
                let after = changed_fields(m, p, &["name", "description", "rows"]);
                if !after.is_empty() {
                    out.push(plan(
                        1,
                        "stage",
                        k,
                        "stage",
                        "update",
                        k,
                        Value::Object(after),
                        what,
                    ));
                }
            }
            Some(_) => deletes.push(plan(2, "stage", k, "stage", "delete", k, Value::Null, what)),
        }
    }
    let routes_live = |id: &str| master.routes.get(id).map_or(true, live);
    for (route, links) in &master.lists {
        if !routes_live(route) {
            continue;
        }
        let theirs = prod.lists.get(route).cloned().unwrap_or_default();
        if *links == theirs {
            continue;
        }
        let base = stages::links_hash(
            &theirs
                .iter()
                .map(|l| StageLink {
                    stage_id: l.stage_id.clone(),
                    direction: Some(l.direction.clone()),
                    stage_no: Some(l.stage_no),
                })
                .collect::<Vec<_>>(),
        );
        let stages: Vec<Value> = links
            .iter()
            .map(|l| json!({"stage_id": l.key(), "stage_no": l.stage_no}))
            .collect();
        out.push(plan(
            2,
            "route_list",
            route,
            "route_stages",
            "replace",
            route,
            json!({"stages": stages, "base_stages_hash": base}),
            json!({"entity": "route_stages", "key": route, "stages": links.len()}),
        ));
    }
    let left: Vec<&String> = prod
        .lists
        .keys()
        .filter(|r| !master.lists.contains_key(*r))
        .collect();
    if !left.is_empty() {
        report.note(json!({"code": "lists_only_on_prod", "count": left.len(),
            "sample": left.iter().take(20).collect::<Vec<_>>(),
            "message": "routes with stages on prod and none on master keep prod's"}));
    }
    out.extend(deletes);
}

fn kind_of(c: &NewChange) -> String {
    format!("{}/{}", c.entity, c.op)
}

async fn import_full(
    pool: &PgPool,
    ctx: &Ctx,
    g: &str,
    pkg: &Package,
    opts: &ImportOpts,
) -> EditorResult<Report> {
    let m = &pkg.manifest;
    let scopes = m.scopes.clone();
    let mut report = Report {
        gtfs_id: g.to_string(),
        kind: m.kind.clone(),
        dry_run: opts.dry_run,
        file_sha256: pkg.sha256.clone(),
        source: Some(m.source.clone()),
        ..Report::default()
    };
    let master = State::from_files(pkg)?;

    let mut tx = pool.begin().await?;
    lock_feed(&mut tx, g).await?;
    let version = service::feed_version(&mut tx, g).await?;
    let history = read_history(&mut tx, g).await?;
    let since = match opts.since_version.or(history.first_since) {
        Some(v) if v > version => {
            return Err(EditorError::bad_request(
                "invalid_since_version",
                format!("since_version {v} is after prod's version {version}"),
            ))
        }
        Some(v) => v,
        None => {
            return Err(EditorError::bad_request(
                "since_version_required",
                format!(
                    "this is feed {g}'s first full migration: say from which prod version on \
                     what prod changed is its own (since_version), so a migration keeps it. \
                     Prod is at version {version}."
                ),
            ))
        }
    };
    report.since_version = Some(since);
    let prod = State::read(&mut tx, g, &scopes).await?;

    let mut planned = Vec::new();
    plan_stops_routes(&master, &prod, &scopes, &mut planned, &mut report);
    if scopes.iter().any(|s| s == "stages") {
        plan_stages(&master, &prod, &mut planned, &mut report);
    }
    // turning stages on, once prod has them (step 3)
    if master.use_stages && !prod.use_stages && scopes.iter().any(|s| s == "stages") {
        planned.push(Planned {
            step: 3,
            id: "feed:use_stages".into(),
            guards: Vec::new(),
            change: NewChange {
                entity: "feed_config".into(),
                op: "update".into(),
                entity_key: g.to_string(),
                after: json!({"use_stages": true}),
                base_row_version: None,
            },
            what: json!({"entity": "feed_config", "key": g, "use_stages": true}),
        });
    } else if !master.use_stages && prod.use_stages {
        report.note(json!({"code": "stages_on_prod_only",
            "message": "prod serves this feed from its stages and master does not; a migration does not turn that off"}));
    }

    // prod's own edits are kept
    let mut steps: Vec<Vec<Planned>> = (0..STEPS.len()).map(|_| Vec::new()).collect();
    let mut conflicted: HashMap<usize, usize> = HashMap::new();
    for p in planned {
        let mine = p
            .guards
            .iter()
            .filter_map(|(k, key)| history.changed_on_prod(k, key, since).map(|v| (k, key, v)))
            .max_by_key(|(_, _, v)| *v);
        if let (Some((kind, key, at)), false) = (mine, opts.takes(&p.id)) {
            *conflicted.entry(p.step).or_default() += 1;
            report.conflict(json!({"id": p.id, "what": p.what, "reason": "changed_on_prod",
                "prod_version": at, "touched": format!("{kind}:{key}"),
                "message": format!("prod changed this itself at version {at}, after {since}; prod's is kept")}));
            continue;
        }
        if mine.is_some() {
            report.count("taken", 1);
        }
        let step = p.step;
        steps[step].push(p);
    }
    for (i, (step, title)) in STEPS.iter().enumerate() {
        let mut planned_counts = BTreeMap::new();
        for p in &steps[i] {
            *planned_counts.entry(kind_of(&p.change)).or_default() += 1;
        }
        report.steps.push(StepReport {
            step,
            title,
            planned: planned_counts,
            conflicts: conflicted.get(&i).copied().unwrap_or(0),
            ..StepReport::default()
        });
    }

    let open = open_migration_sets(&mut tx, g).await?;
    if !open.is_empty() {
        report.note(json!({"code": "migration_open", "change_sets": open,
            "message": "migration drafts are still open; commit or discard them before the next step is drafted"}));
        if !opts.dry_run {
            return Err(EditorError::conflict(
                "migration_open",
                "migration drafts of this feed are still open: commit or discard them, then import again",
            )
            .with_details(json!({"change_sets": open})));
        }
    }

    // draft the first step with anything that goes in
    let import_id = Uuid::new_v4();
    let mut sets: Vec<Uuid> = Vec::new();
    let mut drafted: Option<usize> = None;
    let mut timetable_turn = false;
    for i in 0..STEPS.len() {
        if STEPS[i].0 == "timetables" {
            timetable_turn = true;
            break;
        }
        if steps[i].is_empty() {
            continue;
        }
        let chunk = match STEPS[i].0 {
            "stages" => STAGES_PER_SET,
            "route_stages" => LISTS_PER_SET,
            _ => usize::MAX,
        };
        let items: Vec<Item> = std::mem::take(&mut steps[i])
            .into_iter()
            .map(|p| Item {
                id: p.id,
                what: p.what,
                change: p.change,
            })
            .collect();
        let parts: Vec<&[Item]> = items.chunks(chunk.min(items.len().max(1))).collect();
        let n = parts.len();
        let mut kept_step = 0;
        let dropped_before = report.counts.get("dropped").copied().unwrap_or(0);
        for (k, part) in parts.into_iter().enumerate() {
            let title = if n == 1 {
                format!("From master v{}: {}", m.source.version, STEPS[i].1)
            } else {
                format!(
                    "From master v{}: {} {} of {n}",
                    m.source.version,
                    STEPS[i].1,
                    k + 1
                )
            };
            let description = format!(
                "Brought from master's feed {g} at version {} (written {} by {}) by migration \
                 {import_id}; docs/gtfs-editor.md section 23. What prod changed itself after \
                 version {since} is kept, and the import's report lists it.",
                m.source.version,
                m.source.exported_at.format("%Y-%m-%d %H:%M UTC"),
                m.source.exported_by,
            );
            let set = new_set(&mut tx, g, ctx, &title, &description).await?;
            let kept = add_and_settle(&mut tx, ctx, &set, g, part, &mut report).await?;
            if kept == 0 {
                sqlx::query("DELETE FROM gtfs_change_set WHERE change_set_id = $1")
                    .bind(set.change_set_id)
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            kept_step += kept;
            sets.push(set.change_set_id);
            report.change_sets.push(SetReport {
                change_set_id: (!opts.dry_run).then_some(set.change_set_id),
                title,
                changes: kept,
            });
        }
        let dropped = report.counts.get("dropped").copied().unwrap_or(0) - dropped_before;
        report.steps[i].dropped = dropped;
        if kept_step > 0 {
            report.steps[i].drafted = true;
            drafted = Some(i);
            break;
        }
    }
    for (i, s) in report.steps.iter_mut().enumerate() {
        if drafted.is_some_and(|d| i > d) && !s.planned.is_empty() {
            s.waiting = Some(format!(
                "after the {} drafts are committed",
                STEPS[drafted.unwrap_or(0)].0
            ));
        }
    }
    report.count("added", report.change_sets.iter().map(|s| s.changes).sum());

    if let Some(i) = drafted {
        report.step = Some(STEPS[i].0);
        if opts.dry_run || !report.problems.is_empty() {
            tx.rollback().await?;
            return Ok(report);
        }
        record(
            &mut tx,
            import_id,
            g,
            pkg,
            Some(STEPS[i].0),
            Some(since),
            &sets,
            &report,
            ctx,
        )
        .await?;
        tx.commit().await?;
        report.import_id = Some(import_id);
        report.next = Some(format!(
            "review the {} draft(s) on prod; once a second person has committed them, read the same file in again for the next step",
            sets.len()
        ));
        return Ok(report);
    }
    tx.rollback().await?;

    // the timetable, once everything before it is in: master's trips run on
    // stop orders prod serves only once both serve the same stages
    let has_timetable = scopes.iter().any(|s| s == "timetables");
    let dropped_now = report.counts.get("dropped").copied().unwrap_or(0);
    if !(timetable_turn && has_timetable) {
        report.next = Some(if dropped_now > 0 {
            "nothing could be drafted: prod refused what is left (see dropped)".into()
        } else {
            "prod has everything this file brings".into()
        });
        return Ok(report);
    }
    if master.use_stages != prod.use_stages && scopes.iter().any(|s| s == "stages") {
        report.next = Some(
            "the timetable waits until prod serves the feed from its stages as master does".into(),
        );
        return Ok(report);
    }
    let i = STEPS.len() - 1;
    report.step = Some(STEPS[i].0);
    let exclude: HashSet<String> = history
        .own
        .iter()
        .filter(|((kind, _), _)| *kind == "timetable")
        .filter_map(|((_, route), _)| {
            history
                .changed_on_prod("timetable", route, since)
                .filter(|_| !opts.takes(&format!("timetable:{route}")))
                .map(|_| route.clone())
        })
        .collect();
    if !exclude.is_empty() {
        let mut sample: Vec<&String> = exclude.iter().collect();
        sample.sort();
        report.steps[i].conflicts = exclude.len();
        report.count("conflicts", exclude.len());
        for route in sample.iter().take(LISTED) {
            Report::listed(
                &mut report.conflicts,
                json!({"id": format!("timetable:{route}"),
                       "what": {"entity": "route_trips", "key": route},
                       "reason": "changed_on_prod",
                       "message": format!("prod changed this route's timetable itself after version {since}; prod's is kept")}),
            );
        }
    }
    let di = draft_import::DraftImport {
        files: Some(TIMETABLE_FILES.iter().map(|s| s.to_string()).collect()),
        dry_run: opts.dry_run,
        user_id: ctx.user.user_id,
        email: ctx.user.email.clone(),
        exclude_routes: exclude,
        title: Some(format!("From master v{}: timetable", m.source.version)),
    };
    let tt = draft_import::draft_import(pool, pkg.file("gtfs.zip")?, Some(g), &di).await?;
    let tt_sets: Vec<Uuid> = tt
        .change_sets
        .iter()
        .filter_map(|s| s.change_set_id)
        .collect();
    for s in &tt.change_sets {
        report.change_sets.push(SetReport {
            change_set_id: s.change_set_id,
            title: s.title.clone(),
            changes: s.changes,
        });
    }
    report.steps[i].drafted = !tt.change_sets.is_empty();
    report.next = tt.next.clone().or_else(|| {
        (!tt.change_sets.is_empty()).then(|| {
            "review the timetable draft(s) on prod; once committed, read the same file in again"
                .to_string()
        })
    });
    report.timetable = Some(json!(tt));
    if !opts.dry_run && !tt_sets.is_empty() {
        let mut conn = pool.acquire().await?;
        record(
            &mut conn,
            import_id,
            g,
            pkg,
            Some("timetables"),
            Some(since),
            &tt_sets,
            &report,
            ctx,
        )
        .await?;
        report.import_id = Some(import_id);
    }
    if tt.change_sets.is_empty() && tt.errors == 0 {
        report.next = Some("prod has everything this file brings".into());
    }
    Ok(report)
}

/// The migrations read into feed `g`, newest first, with what became of their
/// drafts.
pub async fn list_imports(pool: &PgPool, ctx: &Ctx, g: &str) -> EditorResult<Value> {
    ctx.require_admin()?;
    let rows = sqlx::query(
        "SELECT m.import_id, m.kind, m.step, m.source::text AS source, m.file_sha256, \
                m.since_version, m.report::text AS report, m.imported_at, u.email, \
                coalesce((SELECT json_agg(json_build_object('change_set_id', s.change_set_id, \
                                  'title', s.title, 'status', s.status) ORDER BY s.created_at) \
                          FROM gtfs_change_set s WHERE s.change_set_id = ANY (m.change_set_ids)), \
                         '[]'::json)::text AS sets \
         FROM gtfs_migration_import m LEFT JOIN gtfs_editor_user u ON u.user_id = m.imported_by \
         WHERE m.gtfs_id = $1 ORDER BY m.imported_at DESC LIMIT 100",
    )
    .bind(g)
    .fetch_all(pool)
    .await?;
    let col = |r: &sqlx::postgres::PgRow, c: &str| -> Result<Value, sqlx::Error> {
        Ok(r.try_get::<Option<String>, _>(c)?
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null))
    };
    let items = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            Ok(json!({
                "import_id": r.try_get::<Uuid, _>("import_id")?,
                "kind": r.try_get::<String, _>("kind")?,
                "step": r.try_get::<Option<String>, _>("step")?,
                "source": col(r, "source")?,
                "file_sha256": r.try_get::<String, _>("file_sha256")?,
                "since_version": r.try_get::<Option<i64>, _>("since_version")?,
                "report": col(r, "report")?,
                "imported_at": r.try_get::<DateTime<Utc>, _>("imported_at")?,
                "imported_by": r.try_get::<Option<String>, _>("email")?,
                "change_sets": col(r, "sets")?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"gtfs_id": g, "items": items}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stops(rows: Vec<Value>) -> BTreeMap<String, Value> {
        by_key(rows, |r| r["stop_id"].as_str().unwrap_or("").to_string())
    }

    fn stop(id: &str, name: &str) -> Value {
        json!({"stop_id": id, "name": name, "lat": 13.0, "lon": 80.2, "location_type": 0,
               "parent_station": null, "deleted": false, "merged_into": null, "stop_code": id})
    }

    #[test]
    fn a_merge_chain_lands_on_the_stop_that_survives() {
        let mut a = stop("A", "A");
        a["deleted"] = json!(true);
        a["merged_into"] = json!("B");
        let mut b = stop("B", "B");
        b["deleted"] = json!(true);
        b["merged_into"] = json!("C");
        let all = stops(vec![a, b, stop("C", "C")]);
        assert_eq!(survivor(&all, "A"), Some("C"));
        assert_eq!(survivor(&all, "C"), Some("C"));
        assert_eq!(survivor(&all, "Z"), None);
    }

    #[test]
    fn a_stop_differing_in_name_and_position_is_one_update_with_both_coordinates() {
        let mut m = stop("A", "ADYAR DEPOT");
        m["lat"] = json!(13.01);
        let p = stop("A", "Adyar Depot");
        let after = changed_fields(&m, &p, STOP_UPDATE_FIELDS);
        assert_eq!(after.get("name"), Some(&json!("ADYAR DEPOT")));
        assert_eq!(after.get("lat"), Some(&json!(13.01)));
        assert_eq!(after.get("lon"), Some(&json!(80.2)));
        assert_eq!(after.len(), 3);
    }

    #[test]
    fn full_plan_creates_merges_and_deletes_stops_and_leaves_prod_only_ones() {
        let mut gone = stop("OLD", "OLD");
        gone["deleted"] = json!(true);
        gone["merged_into"] = json!("NEW");
        let mut dropped = stop("DEL", "DEL");
        dropped["deleted"] = json!(true);
        let master = State {
            stops: stops(vec![
                stop("NEW", "NEW"),
                gone,
                dropped,
                stop("SAME", "SAME"),
            ]),
            ..State::default()
        };
        let prod = State {
            stops: stops(vec![
                stop("OLD", "OLD"),
                stop("DEL", "DEL"),
                stop("SAME", "SAME"),
                stop("MINE", "MINE"),
            ]),
            ..State::default()
        };
        let mut out = Vec::new();
        let mut report = Report::default();
        plan_stops_routes(
            &master,
            &prod,
            &["stops".to_string()],
            &mut out,
            &mut report,
        );
        let kinds: Vec<(String, String)> = out
            .iter()
            .map(|p| (kind_of(&p.change), p.change.entity_key.clone()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("stop/create".to_string(), "NEW".to_string()),
                ("stop/merge".to_string(), "OLD".to_string()),
                ("stop/delete".to_string(), "DEL".to_string()),
            ]
        );
        assert_eq!(out[1].change.after["into_stop_id"], json!("NEW"));
        assert!(out[1].guards.contains(&("stop", "NEW".to_string())));
        assert!(report
            .notes
            .iter()
            .any(|n| n["code"] == "prod_only_stops" && n["count"] == 1));
    }

    #[test]
    fn a_route_list_is_replaced_on_prods_own_hash_and_names_stages_with_their_direction() {
        let link = |id: &str, dir: &str, no: i32| Link {
            stage_id: id.into(),
            direction: dir.into(),
            stage_no: no,
        };
        let master = State {
            lists: [(
                "21G".to_string(),
                vec![link("143", "down", 1), link("stg_a", "", 2)],
            )]
            .into(),
            ..State::default()
        };
        let prod = State::default();
        let mut out = Vec::new();
        plan_stages(&master, &prod, &mut out, &mut Report::default());
        assert_eq!(out.len(), 1);
        let after = &out[0].change.after;
        assert_eq!(after["stages"][0]["stage_id"], json!("143|down"));
        assert_eq!(after["stages"][1]["stage_id"], json!("stg_a|"));
        // a route without stages hashes like []
        assert_eq!(after["base_stages_hash"], json!(stages::links_hash(&[])));
        assert_eq!(out[0].id, "route_list:21G");
    }

    #[test]
    fn prods_own_edit_counts_only_after_since_and_after_the_last_migration() {
        let mut h = History::default();
        h.own.insert(("stop", "A".into()), 120);
        h.own.insert(("stop", "B".into()), 205);
        h.migrated.insert(("stop", "B".into()), 210);
        h.own.insert(("stop", "C".into()), 230);
        h.migrated.insert(("stop", "C".into()), 210);
        assert_eq!(h.changed_on_prod("stop", "A", 190), None);
        assert_eq!(h.changed_on_prod("stop", "B", 190), None);
        assert_eq!(h.changed_on_prod("stop", "C", 190), Some(230));
        assert_eq!(h.changed_on_prod("stop", "D", 190), None);
    }

    #[test]
    fn a_merge_touches_both_stops_and_every_route_it_rewrote() {
        let t = touched(
            "stop",
            "merge",
            "A",
            &json!({"into_stop_id": "B"}),
            &json!([{"route_id": "21G"}, {"route_id": "5B"}]),
        );
        assert_eq!(
            t,
            vec![
                ("stop", "A".to_string()),
                ("stop", "B".to_string()),
                ("route_list", "21G".to_string()),
                ("route_list", "5B".to_string()),
            ]
        );
        assert_eq!(
            touched(
                "stage",
                "create",
                "stg_a",
                &json!({"direction": "up"}),
                &Value::Null
            ),
            vec![("stage", "stg_a|up".to_string())]
        );
        assert_eq!(
            touched("route_trips", "replace", "21G", &Value::Null, &Value::Null),
            vec![("timetable", "21G".to_string())]
        );
    }

    #[test]
    fn an_update_goes_in_when_prod_has_masters_before_is_done_when_it_has_the_after() {
        let before = json!({"name": "OLD", "lat": 1.0, "gtfs": {"zone_id": "Z1"}});
        let after = json!({"name": "NEW", "zone_id": "Z2", "position_review_id": 7});
        assert!(matches!(
            by_fields(&before, &before, &after),
            Verdict::Apply
        ));
        let prod = json!({"name": "NEW", "gtfs": {"zone_id": "Z2"}});
        assert!(matches!(
            by_fields(&prod, &before, &after),
            Verdict::Done(_)
        ));
        let prod = json!({"name": "OTHER", "gtfs": {"zone_id": "Z1"}});
        match by_fields(&prod, &before, &after) {
            Verdict::Conflict { prod, master, .. } => {
                assert_eq!(prod, json!({"name": "OTHER"}));
                assert_eq!(master, json!({"name": "OLD"}));
            }
            _ => panic!("expected a conflict"),
        }
    }

    #[test]
    fn stage_rows_compare_without_what_is_read_from_their_stops() {
        let read = json!({"rows": [{"position": 1, "stop_id": "A", "stop_name": "A", "lat": 1.0,
                                    "stop_type": "NEW STOP", "marker_id": null}]});
        let written = json!({"rows": [{"stop_id": "A", "stop_type": "NEW STOP"}]});
        assert_eq!(field_of(&read, "rows"), field_of(&written, "rows"));
    }

    #[test]
    fn a_hashed_change_goes_in_on_the_same_base_and_is_rebased_when_taken() {
        assert!(matches!(
            by_hash("base_rows_hash", "h1".into(), Some("h1"), None),
            Verdict::Apply
        ));
        assert!(matches!(
            by_hash("base_rows_hash", "h2".into(), Some("h1"), Some("h2".into())),
            Verdict::Done(_)
        ));
        match by_hash("base_rows_hash", "h3".into(), Some("h1"), Some("h2".into())) {
            Verdict::Conflict { rebase, .. } => {
                assert_eq!(rebase, Some(("base_rows_hash", "h3".to_string())))
            }
            _ => panic!("expected a conflict"),
        }
        let c = SourceChange {
            position: 1,
            entity: "route_stops".into(),
            op: "replace".into(),
            entity_key: "21G".into(),
            base_row_version: None,
            before: Value::Null,
            after: json!({"rows": [], "base_rows_hash": "h1", "position_review_id": 3}),
        };
        let p = prepare(&c, Some(("base_rows_hash", "h3".into())));
        assert_eq!(p.after, json!({"rows": [], "base_rows_hash": "h3"}));
        assert_eq!(p.base_row_version, None);
    }

    #[test]
    fn links_spelt_either_way_hash_alike() {
        let a = normal_links(&json!([{"stage_id": "143|down", "stage_no": 1}, {"stage_id": "9|"}]))
            .unwrap();
        let b = normal_links(&json!([{"stage_id": "143", "direction": "down"}, {"stage_id": "9", "direction": "", "stage_no": 2}])).unwrap();
        assert_eq!(stages::links_hash(&a), stages::links_hash(&b));
    }

    #[test]
    fn scopes_are_checked_and_put_in_step_order() {
        assert_eq!(
            parse_scopes(None).unwrap(),
            SCOPES.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(
            parse_scopes(Some("stages, stops")).unwrap(),
            vec!["stops".to_string(), "stages".to_string()]
        );
        assert!(parse_scopes(Some("trips")).is_err());
    }

    #[test]
    fn a_file_reads_back_and_a_damaged_one_is_refused() {
        let mut manifest = Manifest {
            format: FORMAT.into(),
            kind: "change_set".into(),
            gtfs_id: "g".into(),
            source: Source {
                env: "master".into(),
                version: 7,
                use_stages: true,
                exported_at: Utc::now(),
                exported_by: "a@b".into(),
            },
            change_set: None,
            scopes: vec![],
            files: BTreeMap::new(),
        };
        let bytes = write_zip(&mut manifest, vec![("changes.json", b"[]".to_vec())]).unwrap();
        let pkg = read_package(&bytes).unwrap();
        assert_eq!(pkg.manifest.source.version, 7);
        assert_eq!(pkg.file("changes.json").unwrap(), b"[]");
        // a manifest whose checksum does not match its file
        let mut bad = manifest.clone();
        bad.files.clear();
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let o = zip::write::SimpleFileOptions::default();
        bad.files.insert("changes.json".into(), "0".repeat(64));
        w.start_file("manifest.json", o).unwrap();
        w.write_all(&serde_json::to_vec(&bad).unwrap()).unwrap();
        w.start_file("changes.json", o).unwrap();
        w.write_all(b"[]").unwrap();
        let damaged = w.finish().unwrap().into_inner();
        assert_eq!(read_package(&damaged).err().unwrap().code, "file_damaged");
        assert_eq!(
            read_package(b"not a zip").err().unwrap().code,
            "invalid_migration"
        );
    }
}
