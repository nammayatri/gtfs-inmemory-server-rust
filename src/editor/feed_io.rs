//! A feed's whole GTFS in and out of the editor's tables (docs/gtfs-editor.md
//! section 18).
//!
//! - [`load_model`] reads every table of a feed into a
//!   [`FeedModel`](crate::gtfs::model::FeedModel), which `gtfs::write` turns
//!   into the zip the feed publishes. A stop time's headsign is the one the feed
//!   serves (the row's own, or the fare stage a fare-stage feed synthesises), so
//!   the export says what GIMS says.
//! - [`seed`] writes a model into a feed that has no rows yet: the first load
//!   of a feed from its shipped zip, in one transaction, audited `seed`. After
//!   that, a feed changes through drafts only.
//!
//! Stops, routes, trips, stop orders and calendars live in the editor's own
//! tables, whose columns are named for the editor (`name`, `lat`, `color`);
//! [`STOP_COLUMNS`] and its siblings say which GTFS field each one is. Every
//! other file is a record table whose columns are the file's own fields.

use super::error::{EditorError, EditorResult};
use crate::gtfs::model::{
    FeedModel, Frequency, Pattern, PatternStop, Profile, Record, Route, Row, Service, Shape,
    ShapePoint, Stop, Trip, PATTERN_STOP_FIELDS,
};
use crate::gtfs::spec::{self, FileSpec, Key};
use crate::gtfs::Finding;
use crate::services::gtfs_db_source::{headsign, HeadsignSource};
use actix_web::http::StatusCode;
use serde_json::{json, Value};
use sqlx::{PgConnection, Row as _};
use std::collections::{BTreeMap, HashMap};

/// `stops.txt` field -> `gtfs_stop` column (every field but `stop_id`).
pub const STOP_COLUMNS: &[(&str, &str)] = &[
    ("stop_code", "stop_code"),
    ("stop_name", "name"),
    ("tts_stop_name", "tts_stop_name"),
    ("stop_desc", "description"),
    ("stop_lat", "lat"),
    ("stop_lon", "lon"),
    ("zone_id", "zone_id"),
    ("stop_url", "stop_url"),
    ("location_type", "location_type"),
    ("parent_station", "parent_station"),
    ("stop_timezone", "stop_timezone"),
    ("wheelchair_boarding", "wheelchair_boarding"),
    ("level_id", "level_id"),
    ("platform_code", "platform_code"),
    ("stop_access", "stop_access"),
    ("info_json", "info_json"),
];

/// `routes.txt` field -> `gtfs_route` column (every field but `route_id`).
pub const ROUTE_COLUMNS: &[(&str, &str)] = &[
    ("agency_id", "agency_id"),
    ("route_short_name", "short_name"),
    ("route_long_name", "long_name"),
    ("route_desc", "route_desc"),
    ("route_type", "route_type"),
    ("route_url", "route_url"),
    ("route_color", "color"),
    ("route_text_color", "text_color"),
    ("route_sort_order", "route_sort_order"),
    ("continuous_pickup", "continuous_pickup"),
    ("continuous_drop_off", "continuous_drop_off"),
    ("network_id", "network_id"),
];

/// `trips.txt` field -> `gtfs_trip` column (its own fields; the ids and the
/// timetable are columns of their own).
pub const TRIP_COLUMNS: &[(&str, &str)] = &[
    ("trip_headsign", "headsign"),
    ("trip_short_name", "short_name"),
    ("direction_id", "direction_id"),
    ("block_id", "block_id"),
    ("shape_id", "shape_id"),
    ("wheelchair_accessible", "wheelchair_accessible"),
    ("bikes_allowed", "bikes_allowed"),
    ("cars_allowed", "cars_allowed"),
];

/// The `stop_times.txt` fields a stop order's row keeps; each is a
/// `gtfs_route_stop` column of the same name.
pub const PATTERN_STOP_COLUMNS: &[&str] = PATTERN_STOP_FIELDS;

fn file(name: &str) -> &'static FileSpec {
    spec::file(name).expect("a file of the reference")
}

/// One stored row's value for a field, canonical - read through the spec as a
/// value sent to the API is, so what comes out of the tables compares with
/// what went in. A stored value the spec does not accept is left out, and said.
fn canonical(
    fspec: &FileSpec,
    field: &str,
    v: &Value,
    key: &str,
    findings: &mut Vec<Finding>,
) -> Option<Value> {
    let fs = fspec.field(field)?;
    match spec::from_api(fs, v) {
        Ok(v) => v,
        Err(why) => {
            findings.push(
                Finding::warning(
                    "invalid_stored_value",
                    format!("{key}: the stored {field} {v} {why}; it is not exported"),
                )
                .at(fspec.name, None, Some(field)),
            );
            None
        }
    }
}

/// The GTFS fields of a stored row, by a field -> column map.
pub(super) fn fields_of(
    fspec: &FileSpec,
    row: &Value,
    columns: &[(&str, &str)],
    key: &str,
    findings: &mut Vec<Finding>,
) -> Row {
    let mut out = Row::new();
    for (field, column) in columns {
        let v = &row[*column];
        if v.is_null() {
            continue;
        }
        if let Some(c) = canonical(fspec, field, v, key, findings) {
            let name = fspec.field(field).expect("a field").name;
            out.insert(name, c);
        }
    }
    out
}

fn text(v: &Value, k: &str) -> String {
    match &v[k] {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn int(v: &Value, k: &str) -> Option<i64> {
    v[k].as_i64()
}

/// The rows a `SELECT to_jsonb(t)::text ...` query gives, as JSON. (This build
/// of sqlx decodes no JSON, so a row crosses as text.)
async fn rows(conn: &mut PgConnection, sql: &str, g: &str) -> EditorResult<Vec<Value>> {
    let rows = sqlx::query(sql).bind(g).fetch_all(&mut *conn).await?;
    rows.iter()
        .map(|r| {
            let text: String = r.try_get(0)?;
            serde_json::from_str(&text).map_err(|e| {
                EditorError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    format!("a stored row is not JSON: {e}"),
                )
            })
        })
        .collect()
}

/// Every table of feed `g` as a model, and what could not be exported.
pub async fn load_model(
    conn: &mut PgConnection,
    g: &str,
) -> EditorResult<(FeedModel, Vec<Finding>)> {
    let feed = sqlx::query(
        "SELECT headsign_source, default_run_s, default_dwell_s FROM gtfs_feed WHERE gtfs_id = $1",
    )
    .bind(g)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| {
        EditorError::new(
            StatusCode::NOT_FOUND,
            "feed_not_found",
            format!("no feed {g}"),
        )
    })?;
    let headsign_source =
        HeadsignSource::from_column(&feed.try_get::<String, _>("headsign_source")?);
    let mut m = FeedModel {
        gtfs_id: g.to_string(),
        default_timing: (
            feed.try_get("default_run_s")?,
            feed.try_get("default_dwell_s")?,
        ),
        ..Default::default()
    };
    let mut findings = Vec::new();

    // ---- stops: every live one, in the order the feed first had them
    let stops_spec = file("stops.txt");
    for s in rows(
        conn,
        "SELECT to_jsonb(s)::text FROM gtfs_stop s WHERE gtfs_id = $1 AND NOT deleted \
         ORDER BY sort_key NULLS LAST, stop_id",
        g,
    )
    .await?
    {
        let stop_id = text(&s, "stop_id");
        let mut values = fields_of(stops_spec, &s, STOP_COLUMNS, &stop_id, &mut findings);
        // a stop's cluster is what GIMS reads from info_json; the editor keeps
        // it as a column of its own
        if let Some(cluster) = s["cluster_id"].as_str().filter(|c| !c.is_empty()) {
            let mut info = match values.remove("info_json") {
                Some(Value::Object(o)) => o,
                _ => serde_json::Map::new(),
            };
            info.entry("clusterId").or_insert_with(|| json!(cluster));
            values.insert("info_json", Value::Object(info));
        }
        m.stops.push(Stop {
            stop_id,
            values,
            sort_key: int(&s, "sort_key").map(|k| k as i32),
        });
    }

    // ---- routes
    let routes_spec = file("routes.txt");
    for r in rows(
        conn,
        "SELECT to_jsonb(r)::text FROM gtfs_route r WHERE gtfs_id = $1 AND NOT deleted \
         ORDER BY sort_key NULLS LAST, route_id",
        g,
    )
    .await?
    {
        let route_id = text(&r, "route_id");
        let values = fields_of(routes_spec, &r, ROUTE_COLUMNS, &route_id, &mut findings);
        m.routes.push(Route {
            route_id,
            values,
            sort_key: int(&r, "sort_key").map(|k| k as i32),
        });
    }

    // ---- stop orders: the served rows of every stop order of a live route
    let times_spec = file("stop_times.txt");
    let mut patterns: BTreeMap<(String, i16), Pattern> = BTreeMap::new();
    for r in rows(
        conn,
        "SELECT to_jsonb(rs)::text FROM gtfs_route_stop rs \
         JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted \
         WHERE rs.gtfs_id = $1 AND rs.stop_type IN ('NEW STOP', 'INTERMEDIATE STOP') \
         ORDER BY rs.route_id, rs.pattern_key, rs.sequence",
        g,
    )
    .await?
    {
        let route_id = text(&r, "route_id");
        let pattern_key = int(&r, "pattern_key").unwrap_or(1) as i16;
        let key = format!("{route_id} stop order {pattern_key}");
        let mut values = Row::new();
        for field in PATTERN_STOP_COLUMNS {
            if *field == "stop_headsign" {
                continue;
            }
            let v = &r[*field];
            if v.is_null() {
                continue;
            }
            if let Some(c) = canonical(times_spec, field, v, &key, &mut findings) {
                values.insert(times_spec.field(field).expect("a field").name, c);
            }
        }
        // the headsign the feed serves: the row's own, or a fare stage
        let served = headsign(
            r["stop_headsign"].as_str(),
            headsign_source,
            int(&r, "stage_no").unwrap_or(0) as i32,
            r["stop_type"].as_str().unwrap_or(""),
        );
        if let Some(h) = served {
            values.insert("stop_headsign", json!(h));
        }
        patterns
            .entry((route_id.clone(), pattern_key))
            .or_insert_with(|| Pattern {
                route_id,
                pattern_key,
                stops: vec![],
            })
            .stops
            .push(PatternStop {
                stop_id: text(&r, "stop_id"),
                values,
            });
    }
    m.patterns = patterns.into_values().collect();

    for p in rows(
        conn,
        "SELECT to_jsonb(p)::text FROM gtfs_timing_profile p \
         JOIN gtfs_route r ON r.gtfs_id = p.gtfs_id AND r.route_id = p.route_id AND NOT r.deleted \
         WHERE p.gtfs_id = $1 ORDER BY p.route_id, p.pattern_key, p.profile_key",
        g,
    )
    .await?
    {
        let ints = |k: &str| -> Vec<i32> {
            p[k].as_array()
                .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0) as i32).collect())
                .unwrap_or_default()
        };
        m.profiles.push(Profile {
            route_id: text(&p, "route_id"),
            pattern_key: int(&p, "pattern_key").unwrap_or(1) as i16,
            profile_key: int(&p, "profile_key").unwrap_or(1) as i32,
            arrival: ints("arrival_s"),
            departure: ints("departure_s"),
        });
    }

    // ---- trips and their headway windows
    let mut windows: HashMap<String, Vec<Frequency>> = HashMap::new();
    for f in rows(
        conn,
        "SELECT to_jsonb(f)::text FROM gtfs_frequency f WHERE gtfs_id = $1 ORDER BY trip_id, start_s",
        g,
    )
    .await?
    {
        windows.entry(text(&f, "trip_id")).or_default().push(Frequency {
            start_s: int(&f, "start_s").unwrap_or(0) as i32,
            end_s: int(&f, "end_s").unwrap_or(0) as i32,
            headway_s: int(&f, "headway_s").unwrap_or(0) as i32,
            exact_times: int(&f, "exact_times").map(|e| e as i16),
        });
    }
    let trips_spec = file("trips.txt");
    for t in rows(
        conn,
        "SELECT to_jsonb(t)::text FROM gtfs_trip t \
         JOIN gtfs_route r ON r.gtfs_id = t.gtfs_id AND r.route_id = t.route_id AND NOT r.deleted \
         WHERE t.gtfs_id = $1 ORDER BY t.sort_key, t.trip_id",
        g,
    )
    .await?
    {
        let trip_id = text(&t, "trip_id");
        let values = fields_of(trips_spec, &t, TRIP_COLUMNS, &trip_id, &mut findings);
        m.trips.push(Trip {
            frequencies: windows.remove(&trip_id).unwrap_or_default(),
            route_id: text(&t, "route_id"),
            service_id: text(&t, "service_id"),
            pattern_key: int(&t, "pattern_key").unwrap_or(1) as i16,
            profile_key: int(&t, "profile_key").map(|k| k as i32),
            ref_s: int(&t, "ref_s").unwrap_or(0) as i32,
            values,
            sort_key: int(&t, "sort_key").unwrap_or(0) as i32,
            trip_id,
        });
    }

    // ---- calendars
    let mut dates: HashMap<String, Vec<(String, i16)>> = HashMap::new();
    for d in rows(
        conn,
        "SELECT to_jsonb(d)::text FROM gtfs_service_date d WHERE gtfs_id = $1 \
         ORDER BY service_id, service_date",
        g,
    )
    .await?
    {
        dates.entry(text(&d, "service_id")).or_default().push((
            text(&d, "service_date"),
            int(&d, "exception_type").unwrap_or(1) as i16,
        ));
    }
    for s in rows(
        conn,
        "SELECT to_jsonb(s)::text FROM gtfs_service s WHERE gtfs_id = $1 ORDER BY service_id",
        g,
    )
    .await?
    {
        let service_id = text(&s, "service_id");
        // a service with no date range is one calendar.txt does not list
        let ranged = !s["start_date"].is_null();
        let days = ranged.then(|| {
            let mut d = [false; 7];
            for (i, name) in [
                "monday",
                "tuesday",
                "wednesday",
                "thursday",
                "friday",
                "saturday",
                "sunday",
            ]
            .iter()
            .enumerate()
            {
                d[i] = s[*name].as_bool().unwrap_or(false);
            }
            d
        });
        m.services.push(Service {
            dates: dates.remove(&service_id).unwrap_or_default(),
            start_date: ranged.then(|| text(&s, "start_date")),
            end_date: ranged.then(|| text(&s, "end_date")),
            days,
            service_id,
        });
    }

    // ---- every record file
    for fspec in spec::FILES {
        let (Some(entity), Some(key)) = (fspec.entity(), fspec.key()) else {
            continue;
        };
        let order = match key {
            Key::Field(f) => format!("sort_key NULLS LAST, {f}"),
            Key::Minted { .. } => "sort_key NULLS LAST, row_id".into(),
            Key::Feed => "gtfs_id".into(),
        };
        let found = rows(
            conn,
            &format!(
                "SELECT to_jsonb(t)::text FROM gtfs_{entity} t WHERE gtfs_id = $1 ORDER BY {order}"
            ),
            g,
        )
        .await?;
        if entity == "shape" {
            for s in found {
                let nums = |k: &str| -> Vec<Value> { s[k].as_array().cloned().unwrap_or_default() };
                let (seq, lat, lon, dist) = (
                    nums("shape_pt_sequence"),
                    nums("shape_pt_lat"),
                    nums("shape_pt_lon"),
                    nums("shape_dist_traveled"),
                );
                m.shapes.push(Shape {
                    shape_id: text(&s, "shape_id"),
                    points: (0..seq.len())
                        .map(|i| ShapePoint {
                            sequence: seq[i].as_i64().unwrap_or(i as i64),
                            lat: lat.get(i).and_then(Value::as_f64).unwrap_or(0.0),
                            lon: lon.get(i).and_then(Value::as_f64).unwrap_or(0.0),
                            dist: dist.get(i).and_then(Value::as_f64),
                        })
                        .collect(),
                });
            }
            continue;
        }
        let mut records = Vec::new();
        for r in found {
            let key_value = match key {
                Key::Field(f) => text(&r, f),
                Key::Minted { .. } => text(&r, "row_id"),
                Key::Feed => g.to_string(),
            };
            let mut values = Row::new();
            for fs in fspec.fields {
                let v = &r[fs.name];
                if v.is_null() {
                    continue;
                }
                if let Some(c) = canonical(fspec, fs.name, v, &key_value, &mut findings) {
                    values.insert(fs.name, c);
                }
            }
            if matches!(key, Key::Feed) {
                // feed_info names the feed: the preprocessor takes the gtfs_id
                // from it
                values.insert("feed_id", json!(g));
            }
            records.push(Record {
                key: key_value,
                values,
                sort_key: int(&r, "sort_key").map(|k| k as i32),
            });
        }
        if !records.is_empty() {
            m.records.insert(entity, records);
        }
    }
    Ok((m, findings))
}

/// What a seed wrote, for its audit row and its answer.
#[derive(Debug, Default, serde::Serialize)]
pub struct SeedReport {
    pub gtfs_id: String,
    pub counts: BTreeMap<String, usize>,
    pub feed_version: i64,
}

// ---------------------------------------------------------------- seed

/// The largest number of rows one insert statement carries.
const BATCH: usize = 5_000;

/// `INSERT INTO table (cols) SELECT cols FROM jsonb_populate_recordset(...)`,
/// in batches: each row an object keyed by column name. A column a row leaves
/// out is NULL, so every column with a default is named by every row.
async fn insert_rows(
    conn: &mut PgConnection,
    table: &str,
    columns: &[&str],
    rows: &[Value],
    on_conflict_nothing: bool,
) -> EditorResult<()> {
    let cols = columns.join(", ");
    let sql = format!(
        "INSERT INTO {table} ({cols}) SELECT {cols} FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb){}",
        if on_conflict_nothing { " ON CONFLICT DO NOTHING" } else { "" }
    );
    for chunk in rows.chunks(BATCH) {
        sqlx::query(&sql)
            .bind(Value::Array(chunk.to_vec()).to_string())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

fn row_of(g: &str, pairs: Vec<(&str, Value)>) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("gtfs_id".into(), json!(g));
    for (k, v) in pairs {
        if !v.is_null() {
            o.insert(k.to_string(), v);
        }
    }
    Value::Object(o)
}

type Columns = &'static [(&'static str, &'static str)];

/// A row's GTFS values under their column names.
fn mapped(values: &Row, columns: Columns) -> Vec<(&'static str, Value)> {
    columns
        .iter()
        .filter_map(|(field, column)| Some((*column, values.get(field)?.clone())))
        .collect()
}

/// Every column of a table that a seed may fill, for its insert.
fn columns_of(columns: Columns, extra: &[&'static str]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = extra.to_vec();
    for (_, c) in columns {
        if !out.contains(c) {
            out.push(c);
        }
    }
    out
}

/// Why a feed cannot be seeded: it already has rows, or open drafts.
async fn refuse_unless_empty(conn: &mut PgConnection, g: &str) -> EditorResult<()> {
    let mut held: Vec<String> = Vec::new();
    for table in ["gtfs_stop", "gtfs_route", "gtfs_trip", "gtfs_service"] {
        let n: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE gtfs_id = $1"))
                .bind(g)
                .fetch_one(&mut *conn)
                .await?;
        if n > 0 {
            held.push(format!("{n} rows in {table}"));
        }
    }
    for fspec in spec::FILES {
        let Some(table) = fspec.table() else { continue };
        // the one agency 0023 gave every feed that existed is replaced by the
        // seed's, not a reason to refuse it
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM {table} WHERE gtfs_id = $1 \
             AND ('{table}' <> 'gtfs_agency' OR updated_by IS DISTINCT FROM '0023_gtfs_full_spec')"
        ))
        .bind(g)
        .fetch_one(&mut *conn)
        .await?;
        if n > 0 {
            held.push(format!("{n} rows in {table}"));
        }
    }
    if !held.is_empty() {
        return Err(EditorError::new(
            StatusCode::CONFLICT,
            "feed_not_empty",
            format!(
                "feed {g} already has data ({}); a seed only loads a feed that has none - change it through drafts",
                held.join(", ")
            ),
        ));
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gtfs_change_set WHERE gtfs_id = $1 \
         AND status IN ('draft', 'submitted', 'approved')",
    )
    .bind(g)
    .fetch_one(&mut *conn)
    .await?;
    if open > 0 {
        return Err(EditorError::new(
            StatusCode::CONFLICT,
            "feed_has_open_drafts",
            format!("feed {g} has {open} open drafts; a seed only loads a feed nobody is editing"),
        ));
    }
    Ok(())
}

/// Write `m` into feed `g`, which must have no rows yet, inside the caller's
/// transaction: the feed row, every table, the version bump. The caller holds
/// nothing else of the feed; this takes the feed's lock first.
pub async fn seed(
    conn: &mut PgConnection,
    g: &str,
    m: &FeedModel,
    actor: &str,
) -> EditorResult<SeedReport> {
    super::feed_lock::lock_feed(conn, g).await?;
    // the feed row: made when there is none, told it now serves every stop
    let agency_name = m
        .records
        .get("agency")
        .and_then(|a| a.first())
        .and_then(|a| a.values.get("agency_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    // named after who runs it: the publisher is often a city body that
    // publishes several feeds (CUMTA publishes Chennai's bus and metro)
    let display = agency_name
        .clone()
        .or_else(|| {
            m.records
                .get("feed_info")
                .and_then(|f| f.first())
                .and_then(|f| f.values.get("feed_publisher_name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| g.to_string());
    sqlx::query(
        "INSERT INTO gtfs_feed (gtfs_id, display_name, agency_name) VALUES ($1, $2, $3) \
         ON CONFLICT (gtfs_id) DO NOTHING",
    )
    .bind(g)
    .bind(&display)
    .bind(&agency_name)
    .execute(&mut *conn)
    .await?;
    sqlx::query("SELECT 1 FROM gtfs_feed WHERE gtfs_id = $1 FOR UPDATE")
        .bind(g)
        .execute(&mut *conn)
        .await?;
    refuse_unless_empty(conn, g).await?;
    sqlx::query(
        "UPDATE gtfs_feed SET stops_scope = 'all', agency_name = coalesce($2, agency_name), \
         default_run_s = $3, default_dwell_s = $4 WHERE gtfs_id = $1",
    )
    .bind(g)
    .bind(&agency_name)
    .bind(m.default_timing.0)
    .bind(m.default_timing.1)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM gtfs_agency WHERE gtfs_id = $1")
        .bind(g)
        .execute(&mut *conn)
        .await?;

    // ---- record files
    for fspec in spec::FILES {
        let (Some(entity), Some(key)) = (fspec.entity(), fspec.key()) else {
            continue;
        };
        if entity == "shape" {
            continue;
        }
        let Some(records) = m.records.get(entity) else {
            continue;
        };
        let mut cols: Vec<&str> = vec!["gtfs_id", "sort_key", "updated_by"];
        if matches!(key, Key::Minted { .. }) {
            cols.push("row_id");
        }
        cols.extend(fspec.fields.iter().map(|f| f.name));
        let rows: Vec<Value> = records
            .iter()
            .map(|r| {
                let mut pairs: Vec<(&str, Value)> =
                    r.values.iter().map(|(k, v)| (*k, v.clone())).collect();
                pairs.push(("sort_key", json!(r.sort_key)));
                pairs.push(("updated_by", json!(actor)));
                match key {
                    Key::Minted { .. } => pairs.push(("row_id", json!(r.key))),
                    Key::Field(f) if !r.values.contains_key(f) => pairs.push((f, json!(r.key))),
                    _ => {}
                }
                if matches!(key, Key::Feed) {
                    pairs.push(("feed_id", json!(g)));
                }
                row_of(g, pairs)
            })
            .collect();
        insert_rows(conn, &format!("gtfs_{entity}"), &cols, &rows, false).await?;
    }
    let shapes: Vec<Value> = m
        .shapes
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let has_dist = s.points.iter().any(|p| p.dist.is_some());
            row_of(
                g,
                vec![
                    ("shape_id", json!(s.shape_id)),
                    (
                        "shape_pt_sequence",
                        json!(s.points.iter().map(|p| p.sequence).collect::<Vec<_>>()),
                    ),
                    (
                        "shape_pt_lat",
                        json!(s.points.iter().map(|p| p.lat).collect::<Vec<_>>()),
                    ),
                    (
                        "shape_pt_lon",
                        json!(s.points.iter().map(|p| p.lon).collect::<Vec<_>>()),
                    ),
                    (
                        "shape_dist_traveled",
                        if has_dist {
                            json!(s.points.iter().map(|p| p.dist).collect::<Vec<_>>())
                        } else {
                            Value::Null
                        },
                    ),
                    ("sort_key", json!(i)),
                    ("updated_by", json!(actor)),
                ],
            )
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_shape",
        &[
            "gtfs_id",
            "shape_id",
            "shape_pt_sequence",
            "shape_pt_lat",
            "shape_pt_lon",
            "shape_dist_traveled",
            "sort_key",
            "updated_by",
        ],
        &shapes,
        false,
    )
    .await?;

    // ---- stops
    let stops: Vec<Value> = m
        .stops
        .iter()
        .map(|s| {
            let mut pairs = mapped(&s.values, STOP_COLUMNS);
            if !s.values.contains_key("stop_name") {
                // a generic node or a boarding area may have no name; the
                // table holds none as ""
                pairs.push(("name", json!("")));
            }
            if let Some(c) = s
                .values
                .get("info_json")
                .and_then(|i| i.get("clusterId"))
                .and_then(Value::as_str)
            {
                pairs.push(("cluster_id", json!(c)));
            }
            pairs.push(("stop_id", json!(s.stop_id)));
            pairs.push(("sort_key", json!(s.sort_key)));
            pairs.push(("updated_by", json!(actor)));
            pairs.push((
                "location_type",
                json!(s.values.get("location_type").cloned().unwrap_or(json!(0))),
            ));
            row_of(g, pairs)
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_stop",
        &columns_of(
            STOP_COLUMNS,
            &["gtfs_id", "stop_id", "cluster_id", "sort_key", "updated_by"],
        ),
        &stops,
        false,
    )
    .await?;

    // ---- routes (a trigger gives each its pattern 1), then the other stop
    // orders, their rows and their timings
    let routes: Vec<Value> = m
        .routes
        .iter()
        .map(|r| {
            let mut pairs = mapped(&r.values, ROUTE_COLUMNS);
            pairs.push(("route_id", json!(r.route_id)));
            pairs.push(("sort_key", json!(r.sort_key)));
            pairs.push(("updated_by", json!(actor)));
            row_of(g, pairs)
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_route",
        &columns_of(
            ROUTE_COLUMNS,
            &["gtfs_id", "route_id", "sort_key", "updated_by"],
        ),
        &routes,
        false,
    )
    .await?;
    let patterns: Vec<Value> = m
        .patterns
        .iter()
        .map(|p| {
            row_of(
                g,
                vec![
                    ("route_id", json!(p.route_id)),
                    ("pattern_key", json!(p.pattern_key)),
                    ("updated_by", json!(actor)),
                ],
            )
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_pattern",
        &["gtfs_id", "route_id", "pattern_key", "updated_by"],
        &patterns,
        true,
    )
    .await?;
    let mut stop_rows: Vec<Value> = Vec::new();
    for p in &m.patterns {
        for (i, s) in p.stops.iter().enumerate() {
            let mut pairs: Vec<(&str, Value)> =
                s.values.iter().map(|(k, v)| (*k, v.clone())).collect();
            pairs.extend([
                ("route_id", json!(p.route_id)),
                ("pattern_key", json!(p.pattern_key)),
                ("sequence", json!(i + 1)),
                ("stop_id", json!(s.stop_id)),
                ("stop_type", json!("NEW STOP")),
                ("stage_no", json!(0)),
                ("stage_name", json!("")),
                ("updated_by", json!(actor)),
            ]);
            stop_rows.push(row_of(g, pairs));
        }
    }
    let mut rs_cols = vec![
        "gtfs_id",
        "route_id",
        "pattern_key",
        "sequence",
        "stop_id",
        "stop_type",
        "stage_no",
        "stage_name",
        "updated_by",
    ];
    rs_cols.extend(PATTERN_STOP_COLUMNS.iter().copied());
    insert_rows(conn, "gtfs_route_stop", &rs_cols, &stop_rows, false).await?;
    let profiles: Vec<Value> = m
        .profiles
        .iter()
        .map(|p| {
            row_of(
                g,
                vec![
                    ("route_id", json!(p.route_id)),
                    ("pattern_key", json!(p.pattern_key)),
                    ("profile_key", json!(p.profile_key)),
                    ("arrival_s", json!(p.arrival)),
                    ("departure_s", json!(p.departure)),
                    ("source", json!("import")),
                    ("updated_by", json!(actor)),
                ],
            )
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_timing_profile",
        &[
            "gtfs_id",
            "route_id",
            "pattern_key",
            "profile_key",
            "arrival_s",
            "departure_s",
            "source",
            "updated_by",
        ],
        &profiles,
        false,
    )
    .await?;

    // ---- calendars
    let days = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let services: Vec<Value> = m
        .services
        .iter()
        .map(|s| {
            let d = s.days.unwrap_or([false; 7]);
            let mut pairs: Vec<(&str, Value)> = days
                .iter()
                .enumerate()
                .map(|(i, n)| (*n, json!(d[i])))
                .collect();
            pairs.extend([
                ("service_id", json!(s.service_id)),
                ("start_date", json!(s.start_date)),
                ("end_date", json!(s.end_date)),
                ("updated_by", json!(actor)),
            ]);
            row_of(g, pairs)
        })
        .collect();
    let mut service_cols = vec![
        "gtfs_id",
        "service_id",
        "start_date",
        "end_date",
        "updated_by",
    ];
    service_cols.extend(days);
    insert_rows(conn, "gtfs_service", &service_cols, &services, false).await?;
    let dates: Vec<Value> = m
        .services
        .iter()
        .flat_map(|s| {
            s.dates.iter().map(move |(d, k)| {
                row_of(
                    g,
                    vec![
                        ("service_id", json!(s.service_id)),
                        ("service_date", json!(d)),
                        ("exception_type", json!(k)),
                    ],
                )
            })
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_service_date",
        &["gtfs_id", "service_id", "service_date", "exception_type"],
        &dates,
        false,
    )
    .await?;

    // ---- trips and their headway windows
    let trips: Vec<Value> = m
        .trips
        .iter()
        .map(|t| {
            let mut pairs = mapped(&t.values, TRIP_COLUMNS);
            pairs.extend([
                ("trip_id", json!(t.trip_id)),
                ("route_id", json!(t.route_id)),
                ("pattern_key", json!(t.pattern_key)),
                ("profile_key", json!(t.profile_key)),
                ("service_id", json!(t.service_id)),
                ("ref_s", json!(t.ref_s)),
                ("sort_key", json!(t.sort_key)),
                ("source", json!("import")),
                ("updated_by", json!(actor)),
            ]);
            row_of(g, pairs)
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_trip",
        &columns_of(
            TRIP_COLUMNS,
            &[
                "gtfs_id",
                "trip_id",
                "route_id",
                "pattern_key",
                "profile_key",
                "service_id",
                "ref_s",
                "sort_key",
                "source",
                "updated_by",
            ],
        ),
        &trips,
        false,
    )
    .await?;
    let windows: Vec<Value> = m
        .trips
        .iter()
        .flat_map(|t| {
            t.frequencies.iter().map(move |f| {
                row_of(
                    g,
                    vec![
                        ("trip_id", json!(t.trip_id)),
                        ("start_s", json!(f.start_s)),
                        ("end_s", json!(f.end_s)),
                        ("headway_s", json!(f.headway_s)),
                        ("exact_times", json!(f.exact_times)),
                    ],
                )
            })
        })
        .collect();
    insert_rows(
        conn,
        "gtfs_frequency",
        &[
            "gtfs_id",
            "trip_id",
            "start_s",
            "end_s",
            "headway_s",
            "exact_times",
        ],
        &windows,
        false,
    )
    .await?;

    // a parent station that is not there fails here, not at COMMIT
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *conn)
        .await?;
    sqlx::query("SET CONSTRAINTS ALL DEFERRED")
        .execute(&mut *conn)
        .await?;
    let version: i64 = sqlx::query_scalar(
        "UPDATE gtfs_feed SET version = version + 1 WHERE gtfs_id = $1 RETURNING version",
    )
    .bind(g)
    .fetch_one(&mut *conn)
    .await?;
    Ok(SeedReport {
        gtfs_id: g.to_string(),
        counts: m.counts(),
        feed_version: version,
    })
}

// ---------------------------------------------------------------- a zip, seeded

/// Who is importing: a signed-in admin, or the command line.
#[derive(Debug, Clone)]
pub struct Importer {
    pub user_id: Option<uuid::Uuid>,
    pub email: Option<String>,
    /// What `updated_by` says on every row the seed writes.
    pub label: String,
}

/// What an import found, and whether it wrote anything.
#[derive(Debug, serde::Serialize)]
pub struct ImportReport {
    pub gtfs_id: String,
    pub dry_run: bool,
    pub zip_sha256: String,
    pub counts: BTreeMap<String, usize>,
    pub errors: usize,
    pub warnings: usize,
    pub findings: Vec<Finding>,
    /// Differences between the zip and what the tables give back, per file
    /// and field; empty when the feed round-trips.
    pub round_trip: BTreeMap<String, usize>,
    pub round_trip_sample: Vec<crate::gtfs::compare::Diff>,
    pub seeded: bool,
    pub feed_version: Option<i64>,
    /// What the zip breaks of the GTFS reference, as the feed report reads it:
    /// kept, as the import keeps what the feed ships.
    pub validation: crate::gtfs::validate::Report,
}

/// The gtfs_id a zip names in feed_info.txt, as nandi's preprocessor reads it.
pub fn feed_id_of(raw: &crate::gtfs::read::RawFeed) -> Option<String> {
    let t = raw.table("feed_info.txt")?;
    let row = t.rows.first()?;
    Some(t.cell(row, "feed_id").trim().to_string()).filter(|s| !s.is_empty())
}

/// Read a feed's zip and seed feed `gtfs_id` (or the feed_id the zip names)
/// with it: the model is written, read back out of the tables and compared
/// with the zip in the same transaction, and committed only when nothing
/// differs and `dry_run` is false. A zip with an error finding writes nothing.
pub async fn import_zip(
    pool: &sqlx::PgPool,
    bytes: &[u8],
    gtfs_id: Option<&str>,
    dry_run: bool,
    who: &Importer,
) -> EditorResult<ImportReport> {
    use crate::gtfs::{compare, model, read, write, Level};
    let bad = |code: &'static str, m: String| EditorError::new(StatusCode::BAD_REQUEST, code, m);
    let (raw, mut findings) = read::read_zip(bytes).map_err(|e| bad("invalid_zip", e))?;
    let g = match (gtfs_id, feed_id_of(&raw)) {
        (Some(g), _) => g.to_string(),
        (None, Some(id)) => id,
        (None, None) => {
            return Err(bad(
                "gtfs_id_required",
                "the zip's feed_info.txt names no feed_id; say which feed it is".into(),
            ))
        }
    };
    let (m, more) = model::FeedModel::from_raw(&raw, &g, model::BuildOptions::default());
    findings.extend(more);
    let mut report = ImportReport {
        gtfs_id: g.clone(),
        dry_run,
        zip_sha256: super::crypto::sha256_hex(bytes),
        counts: m.counts(),
        errors: findings.iter().filter(|f| f.level == Level::Error).count(),
        warnings: findings
            .iter()
            .filter(|f| f.level == Level::Warning)
            .count(),
        findings,
        round_trip: BTreeMap::new(),
        round_trip_sample: vec![],
        seeded: false,
        feed_version: None,
        validation: crate::gtfs::validate::summarise(
            &crate::gtfs::validate::validate(&m, &chrono::Utc::now().date_naive().to_string()),
            5,
        ),
    };
    if report.errors > 0 {
        return Ok(report);
    }

    let mut tx = pool.begin().await?;
    let seeded = seed(&mut tx, &g, &m, &who.label).await?;
    let (back, export_findings) = load_model(&mut tx, &g).await?;
    report.findings.extend(export_findings);
    let diffs = compare::compare(&raw, &write::to_raw(&back), &m.dropped);
    report.round_trip = compare::summarise(&diffs);
    report.round_trip_sample = diffs.iter().take(20).cloned().collect();
    if !diffs.is_empty() || dry_run {
        tx.rollback().await?;
        return Ok(report);
    }
    let summary: Vec<Value> = report
        .findings
        .iter()
        .take(200)
        .map(|f| json!({"level": f.level, "code": f.code, "file": f.file, "message": f.message}))
        .collect();
    super::auth::audit(
        &mut *tx,
        who.user_id,
        who.email.as_deref(),
        "seed",
        Some(&g),
        None,
        json!({
            "tool": "gtfs import",
            "zip_sha256": report.zip_sha256,
            "counts": report.counts,
            "warnings": report.warnings,
            "findings": summary,
            "feed_version": seeded.feed_version,
        }),
    )
    .await?;
    tx.commit().await?;
    report.seeded = true;
    report.feed_version = Some(seeded.feed_version);
    Ok(report)
}
