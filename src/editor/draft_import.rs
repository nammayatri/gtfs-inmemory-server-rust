//! A GTFS zip brought into a feed the editor already holds, through drafts
//! (docs/gtfs-editor.md section 18): what a seed cannot do once a feed has
//! rows, and how chennai_bus gets its agency, calendars and trips.
//!
//! The zip is read into the model a seed builds, with the feed's own default
//! timing, and set beside the feed as its tables hold it. What differs becomes
//! changes, in two steps, because the second needs the first live:
//!
//! 1. **records and calendars**: every record file the zip has (agency,
//!    feed_info, shapes, fares, ...), its services, the stop orders its trips
//!    need that the feed does not have yet - a *split*: a stop order the feed
//!    has, with other pickups, headsigns or numbering - and the timing
//!    profiles. One change set.
//! 2. **trips**: once that set is committed, a `route_trips/replace` for each
//!    route whose trips differ, at most [`MAX_TRIPS_PER_SET`] trips a set.
//!
//! Stops, routes and the feed's own stop orders are never written: they are
//! compared, and what differs is reported. A trip carries only its start time
//! when it runs on the feed's default timing, so a route whose one stop order
//! in the zip the feed has otherwise gets its trips on the feed's first stop
//! order, timed as the feed times it (a zip made before the stop order was
//! edited says when the route runs, not where). Any other route whose trips
//! run a stop order the feed does not have keeps its trips as they are; it is
//! reported, as is a route with trips the zip does not mention. A file the zip
//! does not have is left as it is.
//!
//! The changes are planned exactly as an upload of the same rows would be
//! (`bulk`), and each set is then replayed as a review would replay it. When
//! anything would be an error nothing is written, and the report says why.

use super::bulk::{self, Kind};
use super::error::{EditorError, EditorResult};
use super::feed_io;
use super::feed_lock::lock_feed;
use super::records;
use super::service::{self, ChangeInsert};
use crate::gtfs::model::{self, FeedModel, Pattern, Profile, Record, Trip, PATTERN_STOP_FIELDS};
use crate::gtfs::read::RawFeed;
use crate::gtfs::spec::{self, FileSpec, Key};
use crate::gtfs::{compare, read, write, Finding};
use crate::services::gtfs_db_source::{headsign, HeadsignSource};
use crate::services::gtfs_timing::format_time;
use actix_web::http::StatusCode;
use serde::Serialize;
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row as _};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// The most trips one change set of the second step carries; a route's trips
/// are never split between sets.
pub const MAX_TRIPS_PER_SET: usize = 5_000;

/// Files a draft import compares but never writes: the editor's own.
pub const COMPARED_ONLY: &[&str] = &["stops.txt", "routes.txt"];

/// The timetable: in or out of an import together, and written as trips.
const TIMETABLE: &[&str] = &["trips.txt", "stop_times.txt", "frequencies.txt"];

/// What to import, and as whom.
pub struct DraftImport {
    /// The files it may write, by their GTFS names; None: every file the zip
    /// has but [`COMPARED_ONLY`].
    pub files: Option<Vec<String>>,
    pub dry_run: bool,
    /// Who the change sets are drafted by.
    pub user_id: Uuid,
    pub email: String,
}

#[derive(Debug, Default, Serialize)]
pub struct DraftImportReport {
    pub gtfs_id: String,
    pub zip_sha256: String,
    pub dry_run: bool,
    /// The files this import may write.
    pub files: Vec<String>,
    /// What reading the zip and the feed noticed.
    pub findings: Vec<Finding>,
    /// How the zip differs from the feed, file by file and field by field, the
    /// timetable aside (its changes are the trip sets).
    pub differences: BTreeMap<String, usize>,
    pub differences_sample: Vec<compare::Diff>,
    pub stop_orders: StopOrders,
    /// Which step this run drafted: `records` or `trips`; `none` when the feed
    /// already holds what the zip says.
    pub step: &'static str,
    pub change_sets: Vec<SetReport>,
    /// What planning and replaying the changes found; an error stops the
    /// import.
    pub errors: usize,
    pub problems: Vec<Value>,
    /// What to do next, when there is something.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// What the zip breaks of the GTFS reference, as the feed report reads it.
    pub validation: crate::gtfs::validate::Report,
}

/// How the zip's stop orders sit on the feed's.
#[derive(Debug, Default, Serialize)]
pub struct StopOrders {
    /// Stop orders the feed has as the zip does.
    pub same: usize,
    /// Stop orders the feed has with other per-stop fields: a new pattern each.
    pub split: usize,
    /// Routes whose one stop order in the zip the feed has otherwise, and
    /// whose trips all run on the feed's default timing: their trips are put
    /// on the feed's first stop order, timed as the feed times it.
    pub moved: usize,
    pub moved_sample: Vec<String>,
    /// Routes whose trips run a stop order the feed does not have, or which
    /// the feed does not have at all: their trips are left as they are.
    pub routes_left: usize,
    pub routes_left_sample: Vec<String>,
    /// Routes with trips in the feed and none in the zip: left as they are.
    pub routes_not_in_zip: usize,
}

#[derive(Debug, Serialize)]
pub struct SetReport {
    /// None on a dry run.
    pub change_set_id: Option<Uuid>,
    pub title: String,
    pub changes: usize,
    pub routes: usize,
    pub trips: usize,
}

fn bad(code: &'static str, message: String) -> EditorError {
    EditorError::new(StatusCode::BAD_REQUEST, code, message)
}

fn present(raw: &RawFeed, name: &str) -> bool {
    raw.files.contains_key(name) || (name == "locations.geojson" && raw.locations.is_some())
}

/// The files the import may write: those asked for (the timetable's files go
/// together), or every file the zip has but the editor's own.
fn scope(raw: &RawFeed, asked: Option<&[String]>) -> EditorResult<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    match asked {
        None => {
            for f in spec::FILES {
                if present(raw, f.name) && !COMPARED_ONLY.contains(&f.name) {
                    out.insert(f.name.to_string());
                }
            }
        }
        Some(list) => {
            for name in list.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                let Some(f) = spec::file(name) else {
                    return Err(bad(
                        "unknown_file",
                        format!("{name} is not a file of the GTFS reference"),
                    ));
                };
                if COMPARED_ONLY.contains(&f.name) {
                    return Err(bad(
                        "compared_only",
                        format!(
                            "{} is the editor's own: a draft import compares it and never writes it",
                            f.name
                        ),
                    ));
                }
                if !present(raw, f.name) {
                    return Err(bad("file_not_in_zip", format!("the zip has no {}", f.name)));
                }
                if TIMETABLE.contains(&f.name) {
                    out.extend(
                        TIMETABLE
                            .iter()
                            .filter(|t| present(raw, t))
                            .map(|t| t.to_string()),
                    );
                } else {
                    out.insert(f.name.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Bring `bytes`, a GTFS zip, into feed `gtfs_id` (default: the feed its
/// feed_info names) through drafts, as the module says.
pub async fn draft_import(
    pool: &sqlx::PgPool,
    bytes: &[u8],
    gtfs_id: Option<&str>,
    opts: &DraftImport,
) -> EditorResult<DraftImportReport> {
    let (raw, findings) = read::read_zip(bytes).map_err(|e| bad("invalid_zip", e))?;
    let g = match (gtfs_id, feed_io::feed_id_of(&raw)) {
        (Some(g), _) => g.to_string(),
        (None, Some(id)) => id,
        (None, None) => {
            return Err(bad(
                "gtfs_id_required",
                "the zip's feed_info.txt names no feed_id; say which feed it is".into(),
            ))
        }
    };
    let files = scope(&raw, opts.files.as_deref())?;
    let timetable = files.contains("trips.txt") && files.contains("stop_times.txt");
    let mut report = DraftImportReport {
        gtfs_id: g.clone(),
        zip_sha256: super::crypto::sha256_hex(bytes),
        dry_run: opts.dry_run,
        files: files.iter().cloned().collect(),
        findings,
        step: "none",
        ..Default::default()
    };

    let mut tx = pool.begin().await?;
    lock_feed(&mut tx, &g).await?;
    let (live, more) = feed_io::load_model(&mut tx, &g).await?;
    report.findings.extend(more);
    let (zip, more) = FeedModel::from_raw(
        &raw,
        &g,
        model::BuildOptions {
            default_timing: Some(live.default_timing),
        },
    );
    report.findings.extend(more);
    report.validation = crate::gtfs::validate::summarise(
        &crate::gtfs::validate::validate(&zip, &chrono::Utc::now().date_naive().to_string()),
        5,
    );
    if crate::gtfs::has_errors(&report.findings) {
        tx.rollback().await?;
        return Ok(report);
    }

    // ---- what differs, the timetable aside
    let (mut theirs, mut ours) = (raw.clone(), write::to_raw(&live));
    for f in TIMETABLE {
        theirs.files.remove(*f);
        ours.files.remove(*f);
    }
    let diffs = compare::compare(&theirs, &ours, &zip.dropped);
    report.differences = compare::summarise(&diffs);
    report.differences_sample = diffs.into_iter().take(50).collect();

    let source: String =
        sqlx::query_scalar("SELECT headsign_source FROM gtfs_feed WHERE gtfs_id = $1")
            .bind(&g)
            .fetch_one(&mut *tx)
            .await?;
    let source = HeadsignSource::from_column(&source);

    // ---- step 1: records, calendars, stop orders, profiles
    let mut problems = Problems::default();
    let record_changes =
        record_changes(&mut tx, &g, &zip, &live, &files, &raw, &mut problems).await?;
    let service_rows = if files.contains("calendar.txt") || files.contains("calendar_dates.txt") {
        service_rows(&zip, &live, &files)
    } else {
        vec![]
    };
    let mut mapping = if timetable {
        map_stop_orders(&mut tx, &g, &zip, &live, source).await?
    } else {
        Mapping::default()
    };
    report.stop_orders = StopOrders {
        routes_left: mapping.left.len(),
        routes_left_sample: mapping.left.iter().take(20).cloned().collect(),
        ..std::mem::take(&mut mapping.report)
    };
    for route in &mapping.moved {
        report.findings.push(
            Finding::warning(
                "route_trips_moved",
                format!(
                    "route {route}: the feed's stop order is not the zip's; its trips run the feed's, timed as the feed times it"
                ),
            )
            .at("stop_times.txt", None, Some("stop_id")),
        );
    }
    for route in &mapping.left {
        report.findings.push(
            Finding::warning(
                "route_trips_left",
                format!(
                    "route {route}: the feed has no route or no stop order its trips in the zip run; its trips are left as they are"
                ),
            )
            .at("trips.txt", None, Some("route_id")),
        );
    }
    let step1 = !record_changes.is_empty()
        || !service_rows.is_empty()
        || !mapping.route_stop_rows.is_empty()
        || !mapping.profile_rows.is_empty();
    let tag = &report.zip_sha256[..8];
    let who = (opts.user_id, opts.email.as_str());

    if step1 {
        report.step = "records";
        let title = format!("GTFS import {tag}: records, calendars and stop orders");
        let set = new_set(&mut tx, &g, opts.user_id, &title, bytes.len()).await?;
        let mut changes = service::insert_changes(&mut tx, set, opts.user_id, &record_changes)
            .await?
            .len();
        for (kind, rows) in [
            (Kind::Services, service_rows),
            (Kind::RouteStops, mapping.route_stop_rows),
            (Kind::TimingProfiles, mapping.profile_rows),
        ] {
            if rows.is_empty() {
                continue;
            }
            let (found, ids) = bulk::plan_into_set(&mut tx, &g, set, who, kind, rows).await?;
            problems.rows(&title, kind, &found);
            changes += ids.len();
        }
        problems
            .replay(&mut tx, &g, set, &title, &opts.email)
            .await?;
        report.change_sets.push(SetReport {
            change_set_id: (!opts.dry_run).then_some(set),
            title,
            changes,
            routes: 0,
            trips: 0,
        });
        if timetable {
            report.next = Some(
                "commit this change set, then run the import again: its trips need these records live"
                    .into(),
            );
        }
    } else if timetable {
        let (routes, not_in_zip) = trip_rows(&zip, &live, &mapping);
        report.stop_orders.routes_not_in_zip = not_in_zip;
        let groups = group_routes(routes);
        let n = groups.len();
        for (k, group) in groups.into_iter().enumerate() {
            report.step = "trips";
            let trips: usize = group.iter().map(|(_, rows)| rows.len()).sum();
            let title = format!(
                "GTFS import {tag}: trips {} of {n} ({} routes, {trips} trips)",
                k + 1,
                group.len()
            );
            let set = new_set(&mut tx, &g, opts.user_id, &title, bytes.len()).await?;
            let routes = group.len();
            let rows: Vec<Value> = group.into_iter().flat_map(|(_, rows)| rows).collect();
            let (found, ids) =
                bulk::plan_into_set(&mut tx, &g, set, who, Kind::RouteTrips, rows).await?;
            problems.rows(&title, Kind::RouteTrips, &found);
            problems
                .replay(&mut tx, &g, set, &title, &opts.email)
                .await?;
            report.change_sets.push(SetReport {
                change_set_id: (!opts.dry_run).then_some(set),
                title,
                changes: ids.len(),
                routes,
                trips,
            });
        }
    }

    report.errors = problems.errors;
    report.problems = problems.list;
    if opts.dry_run || report.errors > 0 || report.change_sets.is_empty() {
        tx.rollback().await?;
        if report.errors > 0 && !opts.dry_run {
            report.next =
                Some("nothing was written: fix what the problems say and import again".into());
        }
        return Ok(report);
    }
    super::auth::audit(
        &mut *tx,
        Some(opts.user_id),
        Some(&opts.email),
        "gtfs_draft_import",
        Some(&g),
        None,
        json!({
            "zip_sha256": report.zip_sha256,
            "step": report.step,
            "files": report.files,
            "change_sets": report.change_sets.iter().map(|s| json!({
                "change_set_id": s.change_set_id, "changes": s.changes, "routes": s.routes, "trips": s.trips,
            })).collect::<Vec<_>>(),
            "stop_orders": report.stop_orders,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(report)
}

/// A new change set, drafted by the importing user.
async fn new_set(
    conn: &mut PgConnection,
    g: &str,
    user_id: Uuid,
    title: &str,
    zip_bytes: usize,
) -> EditorResult<Uuid> {
    let version = service::feed_version(conn, g).await?;
    Ok(sqlx::query(
        "INSERT INTO gtfs_change_set (gtfs_id, title, description, created_by, base_version) \
         VALUES ($1, $2, $3, $4, $5) RETURNING change_set_id",
    )
    .bind(g)
    .bind(title)
    .bind(format!(
        "Drafted by the GTFS import from a zip of {zip_bytes} bytes (docs/gtfs-editor.md section 18)"
    ))
    .bind(user_id)
    .bind(version)
    .fetch_one(&mut *conn)
    .await?
    .try_get("change_set_id")?)
}

/// What planning and replaying found, as the report lists it.
#[derive(Default)]
struct Problems {
    errors: usize,
    list: Vec<Value>,
}

impl Problems {
    /// At most this many are listed; `errors` counts every one.
    const LISTED: usize = 200;

    fn push(&mut self, error: bool, v: Value) {
        if error {
            self.errors += 1;
        }
        if self.list.len() < Self::LISTED {
            self.list.push(v);
        }
    }

    fn rows(&mut self, set: &str, kind: Kind, found: &[Vec<super::validation::Finding>]) {
        for (i, fs) in found.iter().enumerate() {
            for f in fs {
                let error = f.level == super::validation::Level::Error;
                self.push(
                    error,
                    json!({"set": set, "kind": kind_name(kind), "row": i + 1,
                           "level": f.level, "code": f.code, "message": f.message}),
                );
            }
        }
    }

    /// The set's changes replayed as a review replays them, and rolled back.
    async fn replay(
        &mut self,
        conn: &mut PgConnection,
        g: &str,
        set: Uuid,
        title: &str,
        actor: &str,
    ) -> EditorResult<()> {
        let all = service::load_changes_to_apply(conn, set).await?;
        sqlx::query("SAVEPOINT draft_import")
            .execute(&mut *conn)
            .await?;
        let ev = service::evaluate(conn, g, &all, actor).await;
        sqlx::query("ROLLBACK TO SAVEPOINT draft_import")
            .execute(&mut *conn)
            .await?;
        sqlx::query("RELEASE SAVEPOINT draft_import")
            .execute(&mut *conn)
            .await?;
        let ev = ev?;
        for v in ev.validation {
            let error = v["level"] != "warning";
            let mut v = v;
            v["set"] = json!(title);
            self.push(error, v);
        }
        for mut v in ev.conflicts {
            v["set"] = json!(title);
            self.push(true, v);
        }
        Ok(())
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Services => "services",
        Kind::RouteStops => "route_stops",
        Kind::TimingProfiles => "timing_profiles",
        Kind::RouteTrips => "route_trips",
        _ => "records",
    }
}

// ---------------------------------------------------------------- records

/// A row's values as a change sends them.
fn api_values(fspec: &FileSpec, values: &model::Row) -> Map<String, Value> {
    values
        .iter()
        .filter(|(f, _)| !(matches!(fspec.key(), Some(Key::Feed)) && **f == "feed_id"))
        .filter_map(|(f, v)| {
            let fs = fspec.field(f)?;
            Some((f.to_string(), spec::to_api(fs, &Some(v.clone()))))
        })
        .collect()
}

fn shape_points(s: &model::Shape) -> Value {
    json!(s
        .points
        .iter()
        .map(|p| {
            let mut o = json!({"sequence": p.sequence, "lat": p.lat, "lon": p.lon});
            if let Some(d) = p.dist {
                o["dist"] = json!(d);
            }
            o
        })
        .collect::<Vec<_>>())
}

/// The record changes that make each record file the import writes say what
/// the zip says: rows it adds, rows whose fields differ (a field the zip leaves
/// out is cleared), rows it no longer has. A file with no id of its own is
/// matched on its natural key.
async fn record_changes(
    conn: &mut PgConnection,
    g: &str,
    zip: &FeedModel,
    live: &FeedModel,
    files: &BTreeSet<String>,
    raw: &RawFeed,
    problems: &mut Problems,
) -> EditorResult<Vec<ChangeInsert>> {
    let mut out = Vec::new();
    for fspec in spec::FILES {
        let (Some(entity), Some(key)) = (fspec.entity(), fspec.key()) else {
            continue;
        };
        if !files.contains(fspec.name) || !present(raw, fspec.name) {
            continue;
        }
        // (op, key, after) for this file
        let mut planned: Vec<(&'static str, String, Value)> = Vec::new();
        if entity == "shape" {
            let ours: HashMap<&str, &model::Shape> = live
                .shapes
                .iter()
                .map(|s| (s.shape_id.as_str(), s))
                .collect();
            let theirs: HashSet<&str> = zip.shapes.iter().map(|s| s.shape_id.as_str()).collect();
            for s in &zip.shapes {
                match ours.get(s.shape_id.as_str()) {
                    Some(o) if o.points == s.points => {}
                    found => planned.push((
                        if found.is_some() { "update" } else { "create" },
                        s.shape_id.clone(),
                        json!({"shape_id": s.shape_id, "points": shape_points(s)}),
                    )),
                }
            }
            for s in live
                .shapes
                .iter()
                .filter(|s| !theirs.contains(s.shape_id.as_str()))
            {
                planned.push(("delete", s.shape_id.clone(), Value::Null));
            }
        } else {
            let id = |r: &Record| match key {
                Key::Minted { .. } => model::natural_key(fspec, &r.values),
                _ => r.key.clone(),
            };
            let empty = Vec::new();
            let ours: HashMap<String, &Record> = live
                .records
                .get(entity)
                .unwrap_or(&empty)
                .iter()
                .map(|r| (id(r), r))
                .collect();
            let theirs = zip.records.get(entity).unwrap_or(&empty);
            let named: HashSet<String> = theirs.iter().map(id).collect();
            for t in theirs {
                let values = api_values(fspec, &t.values);
                match ours.get(&id(t)) {
                    None => {
                        let k = match key {
                            Key::Field(_) => t.key.clone(),
                            Key::Minted { .. } => records::mint_row_id(),
                            Key::Feed => g.to_string(),
                        };
                        planned.push(("create", k, Value::Object(values)));
                    }
                    Some(o) => {
                        let was = api_values(fspec, &o.values);
                        let mut after = Map::new();
                        for (f, v) in &values {
                            if was.get(f) != Some(v) {
                                after.insert(f.clone(), v.clone());
                            }
                        }
                        for f in was.keys().filter(|f| !values.contains_key(*f)) {
                            after.insert(f.clone(), Value::Null);
                        }
                        if !after.is_empty() {
                            planned.push(("update", o.key.clone(), Value::Object(after)));
                        }
                    }
                }
            }
            let mut gone: Vec<&&Record> = ours
                .iter()
                .filter(|(k, _)| !named.contains(*k))
                .map(|(_, r)| r)
                .collect();
            gone.sort_by(|a, b| a.key.cmp(&b.key));
            for r in gone {
                planned.push(("delete", r.key.clone(), Value::Null));
            }
        }
        if planned.is_empty() {
            continue;
        }
        let keys: Vec<String> = planned
            .iter()
            .filter(|(op, _, _)| *op != "create")
            .map(|(_, k, _)| k.clone())
            .collect();
        let before = records::live_rows(conn, g, fspec, &keys).await?;
        for (op, k, after) in planned {
            if let Err(f) = records::check_payload(fspec, op, &k, &after) {
                problems.push(
                    true,
                    json!({"file": fspec.name, "key": k, "level": "error", "code": f.code, "message": f.message}),
                );
                continue;
            }
            let (b, version) = match before.get(&k) {
                Some((b, v)) => (b.clone(), Some(*v)),
                None => (Value::Null, None),
            };
            out.push(ChangeInsert {
                entity: entity.to_string(),
                op: op.to_string(),
                entity_key: k,
                base_row_version: version,
                before: b,
                after,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- calendars

/// The `services` upload rows that give every service of the zip its days,
/// range and dates, for the services that differ. A calendar file the import
/// does not write leaves that part of a service as the feed has it.
fn service_rows(zip: &FeedModel, live: &FeedModel, files: &BTreeSet<String>) -> Vec<Value> {
    let (days_too, dates_too) = (
        files.contains("calendar.txt"),
        files.contains("calendar_dates.txt"),
    );
    let ours: HashMap<&str, &model::Service> = live
        .services
        .iter()
        .map(|s| (s.service_id.as_str(), s))
        .collect();
    let sorted = |d: &[(String, i16)]| {
        let mut d = d.to_vec();
        d.sort();
        d
    };
    let mut out = Vec::new();
    for s in &zip.services {
        let was = ours.get(s.service_id.as_str()).copied();
        let (days, start, end) = match (days_too, was) {
            (true, _) => (s.days, s.start_date.clone(), s.end_date.clone()),
            (false, Some(w)) => (w.days, w.start_date.clone(), w.end_date.clone()),
            (false, None) => (None, None, None),
        };
        let dates = match (dates_too, was) {
            (true, _) => sorted(&s.dates),
            (false, Some(w)) => sorted(&w.dates),
            (false, None) => vec![],
        };
        if let Some(w) = was {
            if w.days == days
                && w.start_date == start
                && w.end_date == end
                && sorted(&w.dates) == dates
            {
                continue;
            }
        }
        let mut row = json!({
            "action": if was.is_some() { "update" } else { "add" },
            "service_id": s.service_id,
            "start_date": start,
            "end_date": end,
        });
        let days = days.unwrap_or([false; 7]);
        for (d, name) in super::trips::DAYS.iter().enumerate() {
            row[*name] = json!(days[d]);
        }
        if dates.is_empty() {
            out.push(row);
            continue;
        }
        for (date, kind) in dates {
            let mut r = row.clone();
            r["date"] = json!(date);
            r["exception_type"] = json!(kind);
            out.push(r);
        }
    }
    out
}

// ---------------------------------------------------------------- stop orders

/// Where the zip's stop orders and timings sit on the feed's.
#[derive(Default)]
struct Mapping {
    /// The zip's `(route, pattern)` -> the feed's pattern.
    patterns: HashMap<(String, i16), i16>,
    /// The zip's `(route, pattern, profile)` -> the feed's profile.
    profiles: HashMap<(String, i16, i32), i32>,
    /// The zip's `(route, pattern)` -> the feed's profile that holds exactly
    /// the default timing, for its trips on the default timing: a seeded feed
    /// stores that timing explicitly, and is not rewritten for it.
    default_profiles: HashMap<(String, i16), i32>,
    /// `route_stops` upload rows for the splits.
    route_stop_rows: Vec<Value>,
    /// `timing_profiles` upload rows for the timings the feed does not have.
    profile_rows: Vec<Value>,
    /// Routes whose trips stay as the feed has them.
    left: BTreeSet<String>,
    /// Routes whose trips move onto the feed's first stop order.
    moved: BTreeSet<String>,
    report: StopOrders,
}

async fn map_stop_orders(
    conn: &mut PgConnection,
    g: &str,
    zip: &FeedModel,
    live: &FeedModel,
    source: HeadsignSource,
) -> EditorResult<Mapping> {
    let mut m = Mapping::default();
    let live_routes: HashSet<&str> = live.routes.iter().map(|r| r.route_id.as_str()).collect();
    let mut live_patterns: HashMap<&str, Vec<&Pattern>> = HashMap::new();
    for p in &live.patterns {
        live_patterns
            .entry(p.route_id.as_str())
            .or_default()
            .push(p);
    }
    // only the stop orders and timings some trip of the zip runs
    let used: HashSet<(&str, i16)> = zip
        .trips
        .iter()
        .map(|t| (t.route_id.as_str(), t.pattern_key))
        .collect();
    // routes whose trips all run on the feed's default timing
    let timed: HashSet<&str> = zip
        .trips
        .iter()
        .filter(|t| t.profile_key.is_some())
        .map(|t| t.route_id.as_str())
        .collect();
    let mut by_route: BTreeMap<&str, Vec<&Pattern>> = BTreeMap::new();
    for p in zip
        .patterns
        .iter()
        .filter(|p| used.contains(&(p.route_id.as_str(), p.pattern_key)))
    {
        by_route.entry(p.route_id.as_str()).or_default().push(p);
    }

    // (route, the feed's pattern it clones, its new key, the zip's pattern)
    let mut splits: Vec<(&str, i16, i16, &Pattern)> = Vec::new();
    for (route, zps) in by_route {
        if !live_routes.contains(route) {
            m.left.insert(route.to_string());
            continue;
        }
        let lps = live_patterns.get(route).cloned().unwrap_or_default();
        let mut next = lps.iter().map(|p| p.pattern_key).max().unwrap_or(0);
        let mut mine = Vec::new();
        let (mut same, mut split) = (0, 0);
        let mut missing = false;
        let only = zps.len() == 1;
        for zp in zps {
            if let Some(lp) = lps.iter().find(|lp| lp.stops == zp.stops) {
                mine.push((zp.pattern_key, lp.pattern_key, None));
                same += 1;
            } else if let Some(lp) = lps.iter().find(|lp| lp.stop_ids() == zp.stop_ids()) {
                next += 1;
                mine.push((zp.pattern_key, next, Some((lp.pattern_key, zp))));
                split += 1;
            } else if only && !timed.contains(route) && lps.iter().any(|lp| lp.pattern_key == 1) {
                // when it runs, not where: onto the feed's first stop order
                mine.push((zp.pattern_key, 1, None));
                m.report.moved += 1;
                if m.report.moved_sample.len() < 20 {
                    m.report.moved_sample.push(route.to_string());
                }
                m.moved.insert(route.to_string());
            } else {
                missing = true;
                break;
            }
        }
        if missing {
            m.left.insert(route.to_string());
            continue;
        }
        m.report.same += same;
        m.report.split += split;
        for (theirs, ours, clone) in mine {
            m.patterns.insert((route.to_string(), theirs), ours);
            if let Some((from, zp)) = clone {
                splits.push((route, from, ours, zp));
            }
        }
    }

    // the splits: the stop order they share, row for row, with the zip's
    // per-stop fields
    let keys: Vec<(String, i16)> = splits
        .iter()
        .map(|(r, from, _, _)| (r.to_string(), *from))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let rows = service::load_patterns_rows(conn, g, &keys).await?;
    for (route, from, key, zp) in splits {
        let served: Vec<_> = rows
            .get(&(route.to_string(), from))
            .map(|r| r.iter().filter(|r| r.is_served()).collect())
            .unwrap_or_default();
        for (i, (r, zs)) in served.iter().zip(&zp.stops).enumerate() {
            let mut row = json!({
                "action": "add", "route_id": route, "pattern_key": key, "sequence": i + 1,
                "stop_id": r.stop_id, "stop_type": r.stop_type,
                "stage_no": r.stage_no, "stage_name": r.stage_name,
            });
            for f in PATTERN_STOP_FIELDS {
                let Some(v) = zs.values.get(f) else { continue };
                if *f == "stop_headsign" {
                    // never a headsign the fare stage already gives
                    let synthesised = headsign(None, source, r.stage_no, &r.stop_type);
                    if v.as_str() == synthesised.as_deref() {
                        continue;
                    }
                }
                row[*f] = v.clone();
            }
            m.route_stop_rows.push(row);
        }
    }

    // the timings: the feed's where it has the same, else a new profile
    let mut live_profiles: HashMap<(&str, i16), Vec<&Profile>> = HashMap::new();
    for p in &live.profiles {
        live_profiles
            .entry((p.route_id.as_str(), p.pattern_key))
            .or_default()
            .push(p);
    }
    let (run, dwell) = live.default_timing;
    for ((route, theirs), ours) in &m.patterns {
        let Some(lp) = live_patterns
            .get(route.as_str())
            .and_then(|ps| ps.iter().find(|p| p.pattern_key == *ours))
        else {
            continue;
        };
        let d = crate::services::gtfs_timing::default_offsets(lp.stops.len(), run, dwell);
        let found = live_profiles.get(&(route.as_str(), *ours)).and_then(|ps| {
            ps.iter()
                .find(|p| p.arrival == d.arrival && p.departure == d.departure)
        });
        if let Some(p) = found {
            m.default_profiles
                .insert((route.clone(), *theirs), p.profile_key);
        }
    }
    let used: HashSet<(&str, i16, i32)> = zip
        .trips
        .iter()
        .filter_map(|t| Some((t.route_id.as_str(), t.pattern_key, t.profile_key?)))
        .collect();
    let mut next_profile: HashMap<(String, i16), i32> = HashMap::new();
    for zpf in zip
        .profiles
        .iter()
        .filter(|p| used.contains(&(p.route_id.as_str(), p.pattern_key, p.profile_key)))
    {
        let Some(&ours) = m.patterns.get(&(zpf.route_id.clone(), zpf.pattern_key)) else {
            continue;
        };
        let lpfs = live_profiles
            .get(&(zpf.route_id.as_str(), ours))
            .cloned()
            .unwrap_or_default();
        let key = (zpf.route_id.clone(), zpf.pattern_key, zpf.profile_key);
        if let Some(lpf) = lpfs
            .iter()
            .find(|l| l.arrival == zpf.arrival && l.departure == zpf.departure)
        {
            m.profiles.insert(key, lpf.profile_key);
            continue;
        }
        let next = next_profile
            .entry((zpf.route_id.clone(), ours))
            .or_insert_with(|| lpfs.iter().map(|l| l.profile_key).max().unwrap_or(0));
        *next += 1;
        m.profiles.insert(key, *next);
        for (i, (a, d)) in zpf.arrival.iter().zip(&zpf.departure).enumerate() {
            m.profile_rows.push(json!({
                "action": "add", "route_id": zpf.route_id, "pattern_key": ours,
                "profile_key": *next, "stop_sequence": i + 1,
                "arrival_offset": a, "departure_offset": d,
            }));
        }
    }
    Ok(m)
}

// ---------------------------------------------------------------- trips

/// A trip as both sides can say it, on the feed's patterns and profiles.
type Comparable<'a> = (
    &'a str,
    &'a str,
    i16,
    Option<i32>,
    i32,
    &'a model::Row,
    &'a [model::Frequency],
);

/// The `route_trips` upload rows of every route whose trips differ, route by
/// route in the zip's order, and how many routes have trips in the feed but
/// none in the zip.
fn trip_rows(zip: &FeedModel, live: &FeedModel, m: &Mapping) -> (Vec<(String, Vec<Value>)>, usize) {
    let mut theirs: Vec<(&str, Vec<&Trip>)> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&Trip> = zip.trips.iter().collect();
    order.sort_by_key(|t| t.sort_key);
    for t in order {
        let k = *at.entry(t.route_id.as_str()).or_insert_with(|| {
            theirs.push((t.route_id.as_str(), vec![]));
            theirs.len() - 1
        });
        theirs[k].1.push(t);
    }
    let mut ours: HashMap<&str, Vec<&Trip>> = HashMap::new();
    for t in &live.trips {
        ours.entry(t.route_id.as_str()).or_default().push(t);
    }
    let not_in_zip = ours.keys().filter(|r| !at.contains_key(*r)).count();
    let firsts: HashMap<(&str, i16, i32), i32> = zip
        .profiles
        .iter()
        .map(|p| {
            (
                (p.route_id.as_str(), p.pattern_key, p.profile_key),
                p.arrival.first().copied().unwrap_or(0),
            )
        })
        .collect();

    let mut out = Vec::new();
    for (route, trips) in theirs {
        if m.left.contains(route) {
            continue;
        }
        let mapped = |t: &Trip| -> Option<(i16, Option<i32>)> {
            let pattern = *m.patterns.get(&(route.to_string(), t.pattern_key))?;
            let profile = match t.profile_key {
                Some(p) => Some(*m.profiles.get(&(route.to_string(), t.pattern_key, p))?),
                None => m
                    .default_profiles
                    .get(&(route.to_string(), t.pattern_key))
                    .copied(),
            };
            Some((pattern, profile))
        };
        let mut a: Vec<Comparable> = Vec::new();
        for t in &trips {
            let Some((pattern, profile)) = mapped(t) else {
                continue;
            };
            a.push((
                &t.trip_id,
                &t.service_id,
                pattern,
                profile,
                t.ref_s,
                &t.values,
                &t.frequencies,
            ));
        }
        let had = ours.get(route).cloned().unwrap_or_default();
        let mut b: Vec<Comparable> = had
            .iter()
            .map(|t| {
                (
                    t.trip_id.as_str(),
                    t.service_id.as_str(),
                    t.pattern_key,
                    t.profile_key,
                    t.ref_s,
                    &t.values,
                    t.frequencies.as_slice(),
                )
            })
            .collect();
        a.sort_by(|x, y| x.0.cmp(y.0));
        b.sort_by(|x, y| x.0.cmp(y.0));
        if a == b {
            continue;
        }
        let action = if had.is_empty() { "add" } else { "update" };
        let rows = trips
            .iter()
            .filter_map(|t| {
                let (pattern, profile) = mapped(t)?;
                let first = t
                    .profile_key
                    .and_then(|p| firsts.get(&(route, t.pattern_key, p)).copied())
                    .unwrap_or(0);
                let mut row = json!({
                    "action": action, "route_id": route, "trip_id": t.trip_id,
                    "pattern_key": pattern, "service_id": t.service_id,
                    "start_time": format_time(t.ref_s + first),
                });
                if let Some(p) = profile {
                    row["profile_key"] = json!(p);
                }
                for (field, column) in [
                    ("trip_headsign", "headsign"),
                    ("trip_short_name", "short_name"),
                    ("direction_id", "direction_id"),
                    ("block_id", "block_id"),
                    ("shape_id", "shape_id"),
                    ("wheelchair_accessible", "wheelchair_accessible"),
                    ("bikes_allowed", "bikes_allowed"),
                    ("cars_allowed", "cars_allowed"),
                ] {
                    if let Some(v) = t.values.get(field) {
                        row[column] = v.clone();
                    }
                }
                if !t.frequencies.is_empty() {
                    row["frequencies"] = json!(t
                        .frequencies
                        .iter()
                        .map(|f| {
                            let mut o = json!({
                                "start_time": format_time(f.start_s),
                                "end_time": format_time(f.end_s),
                                "headway_s": f.headway_s,
                            });
                            if let Some(e) = f.exact_times {
                                o["exact_times"] = json!(e);
                            }
                            o
                        })
                        .collect::<Vec<_>>());
                }
                Some(row)
            })
            .collect();
        out.push((route.to_string(), rows));
    }
    (out, not_in_zip)
}

/// Routes into sets of at most [`MAX_TRIPS_PER_SET`] trips, in order; a route
/// with more trips than that is a set of its own.
fn group_routes(routes: Vec<(String, Vec<Value>)>) -> Vec<Vec<(String, Vec<Value>)>> {
    let mut out: Vec<Vec<(String, Vec<Value>)>> = Vec::new();
    let mut size = 0;
    for r in routes {
        let n = r.1.len();
        match out.last_mut() {
            Some(last) if size + n <= MAX_TRIPS_PER_SET => {
                last.push(r);
                size += n;
            }
            _ => {
                out.push(vec![r]);
                size = n;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(id: &str, trips: usize) -> (String, Vec<Value>) {
        (id.to_string(), vec![json!({}); trips])
    }

    #[test]
    fn trips_go_into_sets_of_at_most_five_thousand_a_route_whole() {
        let groups = group_routes(vec![
            route("A", 3_000),
            route("B", 2_000),
            route("C", 1),
            route("D", 6_000),
            route("E", 10),
        ]);
        let names: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.iter().map(|(r, _)| r.as_str()).collect())
            .collect();
        assert_eq!(names, vec![vec!["A", "B"], vec!["C"], vec!["D"], vec!["E"]]);
    }

    #[test]
    fn the_scope_is_every_file_but_the_editors_own_and_the_timetable_goes_whole() {
        let mut raw = RawFeed::default();
        for f in [
            "agency.txt",
            "stops.txt",
            "routes.txt",
            "trips.txt",
            "stop_times.txt",
        ] {
            raw.files.insert(f.into(), Default::default());
        }
        let all = scope(&raw, None).unwrap();
        assert_eq!(
            all.into_iter().collect::<Vec<_>>(),
            vec!["agency.txt", "stop_times.txt", "trips.txt"]
        );
        let asked = scope(&raw, Some(&["trips.txt".to_string()])).unwrap();
        assert_eq!(
            asked.into_iter().collect::<Vec<_>>(),
            vec!["stop_times.txt", "trips.txt"]
        );
        let code = |files: &[&str]| {
            let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            scope(&raw, Some(&files)).unwrap_err().code
        };
        assert_eq!(code(&["stops.txt"]), "compared_only");
        assert_eq!(code(&["pathways.txt"]), "file_not_in_zip");
        assert_eq!(code(&["notes.txt"]), "unknown_file");
    }
}
