//! Record changes (docs/gtfs-editor.md section 18): every GTFS file the editor
//! keeps as records - agency, feed_info, shapes, levels, pathways, transfers,
//! Fares v1 and v2, translations, attributions and the Flex files - edited
//! through drafts like everything else.
//!
//! One engine serves all of them, driven by the spec registry
//! (`gtfs::spec`): a record change is `{entity, op, entity_key, after}` where
//! `entity` is the file's (`pathway`), `op` is `create`, `update` or `delete`,
//! `entity_key` the row's id (its own id field; the feed for feed_info; a
//! minted `r_` id for a file with no id of its own), and `after` the row's
//! fields by their GTFS names, values as the API takes them (`spec::from_api`).
//! A shape is `{shape_id, points: [{lat, lon, sequence?, dist?}]}`, its points
//! replaced whole.
//!
//! - **When a change is added** ([`check_payload`], pure): every field is one of
//!   the file's and holds a value of its type; a create has every required
//!   field; a key is not renamed.
//! - **When the draft replays** ([`apply`]): a create's id is unused and its
//!   natural key unique; whatever it points at exists, live or earlier in the
//!   draft; the row keeps the reference's rules for one row
//!   (`gtfs::rules`); a delete leaves nothing pointing at nothing. As
//!   everywhere, a problem the live row already had is a warning, not a block.
//! - **Conflicts**: an update or a delete is based on the row's `row_version`.

use super::error::{EditorError, EditorResult};
use super::feed_io::{ROUTE_COLUMNS, STOP_COLUMNS, TRIP_COLUMNS};
use super::service::{fail, ApplyError, ChangeRow, Page};
use super::validation::{grade_against_live, Finding};
use crate::gtfs::model::Row;
use crate::gtfs::rules;
use crate::gtfs::spec::{self, FileSpec, Key};
use serde_json::{json, Map, Value};
use sqlx::{PgConnection, Row as _};
use uuid::Uuid;

/// Minted record ids: `r_` + 10 lower-case hex digits.
pub const MINTED_PREFIX: &str = "r_";

/// The record file a change entity edits, if it is one.
pub fn spec_for(entity: &str) -> Option<&'static FileSpec> {
    spec::record_file(entity)
}

/// The column a record table is keyed by.
pub fn key_column(fspec: &FileSpec) -> &'static str {
    match fspec.key() {
        Some(Key::Field(f)) => f,
        Some(Key::Minted { .. }) => "row_id",
        _ => "gtfs_id",
    }
}

/// The field a create names its row by, for `settle_create_key`.
pub fn create_key_field(entity: &str, op: &str) -> Option<&'static str> {
    match (spec_for(entity)?.key()?, op) {
        (Key::Field(f), "create") => Some(f),
        _ => None,
    }
}

/// Whether a create's key is minted by the server.
pub fn mints_key(entity: &str, op: &str) -> bool {
    op == "create"
        && matches!(
            spec_for(entity).and_then(FileSpec::key),
            Some(Key::Minted { .. })
        )
}

pub fn mint_row_id() -> String {
    format!(
        "{MINTED_PREFIX}{}",
        hex::encode(super::crypto::random_bytes(5))
    )
}

fn table(fspec: &FileSpec) -> String {
    fspec.table().expect("a record file")
}

fn entity(fspec: &FileSpec) -> &'static str {
    fspec.entity().expect("a record file")
}

fn invalid(code: &str, key: &str, message: impl Into<String>) -> Finding {
    Finding::error(code, key, message)
}

/// What a person calls a row: "Pathway PW1", "Transfer r_1a2b3c4d5e".
pub fn describe(fspec: &FileSpec, key: &str) -> String {
    let label = match entity(fspec) {
        "agency" => "Agency",
        "feed_info" => return "The feed information".into(),
        "fare_attribute" => "Fare",
        "fare_rule" => "Fare rule",
        "timeframe" => "Timeframe",
        "rider_category" => "Rider category",
        "fare_media" => "Fare medium",
        "fare_product" => "Fare product",
        "fare_leg_rule" => "Fare leg rule",
        "fare_leg_join_rule" => "Fare leg join rule",
        "fare_transfer_rule" => "Fare transfer rule",
        "area" => "Area",
        "stop_area" => "Stop in an area",
        "network" => "Network",
        "route_network" => "Route in a network",
        "shape" => "Shape",
        "transfer" => "Transfer",
        "pathway" => "Pathway",
        "level" => "Level",
        "location_group" => "Location group",
        "location_group_stop" => "Stop in a location group",
        "location" => "Flex zone",
        "booking_rule" => "Booking rule",
        "translation" => "Translation",
        "attribution" => "Attribution",
        other => other,
    };
    if key.is_empty() {
        format!("{label} with no id")
    } else {
        format!("{label} {key}")
    }
}

// ---------------------------------------------------------------- shape checks

/// A shape's points as the API takes them.
fn check_points(m: &Map<String, Value>, what: &str, create: bool) -> Result<(), Finding> {
    let Some(points) = m.get("points") else {
        return if create {
            Err(invalid(
                "missing_field",
                "points",
                format!("{what}: a shape needs its points"),
            ))
        } else {
            Ok(())
        };
    };
    let points = points.as_array().ok_or_else(|| {
        invalid(
            "invalid_value",
            "points",
            format!("{what}: points is a list of {{lat, lon}}"),
        )
    })?;
    if points.len() < 2 {
        return Err(invalid(
            "shape_too_short",
            "points",
            format!("{what}: a shape has at least two points"),
        ));
    }
    let shapes = spec::file("shapes.txt").expect("shapes.txt");
    let (lat_f, lon_f, seq_f, dist_f) = (
        shapes.field("shape_pt_lat").expect("a field"),
        shapes.field("shape_pt_lon").expect("a field"),
        shapes.field("shape_pt_sequence").expect("a field"),
        shapes.field("shape_dist_traveled").expect("a field"),
    );
    let mut last_seq: Option<i64> = None;
    let mut last_dist: Option<f64> = None;
    for (i, p) in points.iter().enumerate() {
        let p = p.as_object().ok_or_else(|| {
            invalid(
                "invalid_value",
                "points",
                format!("{what}: point {} is not {{lat, lon}}", i + 1),
            )
        })?;
        if let Some(k) = p
            .keys()
            .find(|k| !["lat", "lon", "sequence", "dist"].contains(&k.as_str()))
        {
            return Err(invalid(
                "unknown_field",
                "points",
                format!(
                    "{what}: point {}: {k:?} is not lat, lon, sequence or dist",
                    i + 1
                ),
            ));
        }
        for (k, fs, required) in [("lat", lat_f, true), ("lon", lon_f, true)] {
            match spec::from_api(fs, p.get(k).unwrap_or(&Value::Null)) {
                Ok(Some(_)) => {}
                Ok(None) if !required => {}
                Ok(None) => {
                    return Err(invalid(
                        "missing_field",
                        "points",
                        format!("{what}: point {} has no {k}", i + 1),
                    ))
                }
                Err(why) => {
                    return Err(invalid(
                        "invalid_value",
                        "points",
                        format!("{what}: point {}: {k} {why}", i + 1),
                    ))
                }
            }
        }
        if let Some(v) = p.get("sequence").filter(|v| !v.is_null()) {
            let seq = spec::from_api(seq_f, v)
                .map_err(|why| {
                    invalid(
                        "invalid_value",
                        "points",
                        format!("{what}: point {}: sequence {why}", i + 1),
                    )
                })?
                .and_then(|v| v.as_i64());
            if let (Some(a), Some(b)) = (last_seq, seq) {
                if b <= a {
                    return Err(invalid(
                        "invalid_value",
                        "points",
                        format!(
                            "{what}: point {}: the sequence goes up, point by point",
                            i + 1
                        ),
                    ));
                }
            }
            last_seq = seq;
        }
        if let Some(v) = p.get("dist").filter(|v| !v.is_null()) {
            let d = spec::from_api(dist_f, v)
                .map_err(|why| {
                    invalid(
                        "invalid_value",
                        "points",
                        format!("{what}: point {}: dist {why}", i + 1),
                    )
                })?
                .and_then(|v| v.as_f64());
            if let (Some(a), Some(b)) = (last_dist, d) {
                if b < a {
                    return Err(invalid(
                        "invalid_value",
                        "points",
                        format!(
                            "{what}: point {}: the distance travelled never goes down",
                            i + 1
                        ),
                    ));
                }
            }
            last_dist = d.or(last_dist);
        }
    }
    Ok(())
}

/// Shape checks run when a record change is added: nothing that needs the
/// tables. `key` is the change's `entity_key` (settled, or minted).
pub fn check_payload(fspec: &FileSpec, op: &str, key: &str, after: &Value) -> Result<(), Finding> {
    let what = format!("{}/{op}", entity(fspec));
    let what = what.as_str();
    match op {
        "delete" => {
            if after.is_null() || after.as_object().is_some_and(Map::is_empty) {
                Ok(())
            } else {
                Err(invalid(
                    "invalid_payload",
                    "",
                    format!("{what}: a delete carries no fields"),
                ))
            }
        }
        "create" | "update" => {
            let m = after.as_object().ok_or_else(|| {
                invalid(
                    "invalid_payload",
                    "",
                    format!("{what}: `after` must be an object"),
                )
            })?;
            if op == "update" && m.is_empty() {
                return Err(invalid(
                    "invalid_payload",
                    "",
                    format!("{what}: nothing to change"),
                ));
            }
            if key.trim().is_empty() {
                return Err(invalid(
                    "invalid_payload",
                    "",
                    format!("{what}: the row's id is required"),
                ));
            }
            let is_shape = entity(fspec) == "shape";
            for k in m.keys() {
                let known = if is_shape {
                    k == "shape_id" || k == "points"
                } else {
                    fspec.field(k).is_some()
                };
                if !known {
                    return Err(invalid(
                        "unknown_field",
                        k,
                        format!(
                            "{what}: {k:?} is not a field of {} (fields: {})",
                            fspec.name,
                            if is_shape {
                                "shape_id, points".to_string()
                            } else {
                                fspec
                                    .fields
                                    .iter()
                                    .map(|f| f.name)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        ),
                    ));
                }
            }
            if matches!(fspec.key(), Some(Key::Feed)) && m.contains_key("feed_id") {
                return Err(invalid(
                    "invalid_payload",
                    "feed_id",
                    format!("{what}: feed_id is the feed's own id, not an editable field"),
                ));
            }
            if is_shape {
                check_points(m, what, op == "create")?;
            } else {
                for (k, v) in m {
                    let fs = fspec.field(k).expect("checked above");
                    if let Err(why) = spec::from_api(fs, v) {
                        return Err(invalid("invalid_value", k, format!("{what}: {k} {why}")));
                    }
                }
            }
            // the row's own id is its key and does not change
            if let Some(Key::Field(f)) = fspec.key() {
                let given = m.get(f).map(|v| match v {
                    Value::String(s) => s.trim().to_string(),
                    other => other.to_string(),
                });
                if let Some(given) = given {
                    if given != key {
                        return Err(invalid(
                            "key_mismatch",
                            f,
                            format!(
                                "{what}: {f} is the row's id, {key}; to give it another, delete this row and create one"
                            ),
                        ));
                    }
                }
                if key.contains(char::is_control) {
                    return Err(invalid(
                        "invalid_id",
                        f,
                        format!("{what}: {f} holds a control character"),
                    ));
                }
            }
            if op == "create" && !is_shape {
                for fs in fspec.fields {
                    let is_key = matches!(fspec.key(), Some(Key::Field(f)) if f == fs.name);
                    if fs.presence == spec::Presence::Required
                        && !is_key
                        && m.get(fs.name)
                            .is_none_or(|v| v.is_null() || v.as_str() == Some(""))
                    {
                        return Err(invalid(
                            "missing_field",
                            fs.name,
                            format!("{what}: {} is required", fs.name),
                        ));
                    }
                }
            }
            if op == "update" {
                for (k, v) in m {
                    let required = fspec
                        .field(k)
                        .is_some_and(|f| f.presence == spec::Presence::Required);
                    if required && (v.is_null() || v.as_str() == Some("")) {
                        return Err(invalid(
                            "missing_field",
                            k,
                            format!("{what}: {k} is required and cannot be cleared"),
                        ));
                    }
                }
            }
            Ok(())
        }
        _ => Err(invalid(
            "invalid_change",
            "",
            format!("unsupported change {}/{op}", entity(fspec)),
        )),
    }
}

// ---------------------------------------------------------------- rows

/// A stored row's fields, canonical. None for a shape (see [`shape_json`]).
fn row_of(fspec: &FileSpec, stored: &Value) -> Row {
    let mut out = Row::new();
    for fs in fspec.fields {
        let v = &stored[fs.name];
        if v.is_null() {
            continue;
        }
        if let Ok(Some(c)) = spec::from_api(fs, v) {
            out.insert(fs.name, c);
        }
    }
    out
}

/// A row's fields as the API gives them out.
fn api_of(fspec: &FileSpec, row: &Row) -> Map<String, Value> {
    let mut out = Map::new();
    for fs in fspec.fields {
        let v = row.get(fs.name).cloned();
        out.insert(fs.name.into(), spec::to_api(fs, &v));
    }
    out
}

/// A stored shape as the API gives it out.
fn shape_json(stored: &Value) -> Value {
    let arr = |k: &str| stored[k].as_array().cloned().unwrap_or_default();
    let (seq, lat, lon, dist) = (
        arr("shape_pt_sequence"),
        arr("shape_pt_lat"),
        arr("shape_pt_lon"),
        arr("shape_dist_traveled"),
    );
    json!({
        "shape_id": stored["shape_id"],
        "points": (0..lat.len()).map(|i| json!({
            "sequence": seq.get(i).cloned().unwrap_or(Value::Null),
            "lat": lat[i],
            "lon": lon.get(i).cloned().unwrap_or(Value::Null),
            "dist": dist.get(i).cloned().unwrap_or(Value::Null),
        })).collect::<Vec<_>>(),
    })
}

/// A stored row in read shape: its fields, its key and its version.
fn read_json(fspec: &FileSpec, stored: &Value) -> Value {
    let mut out = if entity(fspec) == "shape" {
        shape_json(stored).as_object().cloned().unwrap_or_default()
    } else {
        api_of(fspec, &row_of(fspec, stored))
    };
    if matches!(fspec.key(), Some(Key::Minted { .. })) {
        out.insert("row_id".into(), stored["row_id"].clone());
    }
    if matches!(fspec.key(), Some(Key::Feed)) {
        out.insert("feed_id".into(), stored["gtfs_id"].clone());
    }
    out.insert("row_version".into(), stored["row_version"].clone());
    Value::Object(out)
}

async fn stored_row(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    key: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let sql = format!(
        "SELECT to_jsonb(t)::text FROM {} t WHERE t.gtfs_id = $1 AND t.{} = $2",
        table(fspec),
        key_column(fspec)
    );
    let row = sqlx::query(&sql)
        .bind(g)
        .bind(key)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(match row {
        Some(r) => Some(serde_json::from_str(&r.try_get::<String, _>(0)?).unwrap_or(Value::Null)),
        None => None,
    })
}

/// The canonical row a create or an update leaves: `base` (the live row) with
/// `after`'s fields over it; a field sent as null or blank is cleared.
fn merged(fspec: &FileSpec, base: &Row, after: &Value) -> Row {
    let mut out = base.clone();
    if let Some(m) = after.as_object() {
        for (k, v) in m {
            let Some(fs) = fspec.field(k) else { continue };
            match spec::from_api(fs, v) {
                Ok(Some(c)) => {
                    out.insert(fs.name, c);
                }
                Ok(None) => {
                    out.remove(fs.name);
                }
                Err(_) => {}
            }
        }
    }
    out
}

/// A shape's point arrays from a change's `points`.
fn shape_columns(after: &Value) -> Map<String, Value> {
    let points = after["points"].as_array().cloned().unwrap_or_default();
    let has_seq = points.iter().any(|p| !p["sequence"].is_null());
    let has_dist = points.iter().any(|p| !p["dist"].is_null());
    let num = |v: &Value| -> Value {
        match v {
            Value::String(s) => s
                .trim()
                .parse::<f64>()
                .map(|x| json!(x))
                .unwrap_or(Value::Null),
            other => other.clone(),
        }
    };
    let mut m = Map::new();
    m.insert(
        "shape_pt_sequence".into(),
        json!(points
            .iter()
            .enumerate()
            .map(|(i, p)| if has_seq {
                num(&p["sequence"])
            } else {
                json!(i + 1)
            })
            .collect::<Vec<_>>()),
    );
    m.insert(
        "shape_pt_lat".into(),
        json!(points.iter().map(|p| num(&p["lat"])).collect::<Vec<_>>()),
    );
    m.insert(
        "shape_pt_lon".into(),
        json!(points.iter().map(|p| num(&p["lon"])).collect::<Vec<_>>()),
    );
    m.insert(
        "shape_dist_traveled".into(),
        if has_dist {
            json!(points.iter().map(|p| num(&p["dist"])).collect::<Vec<_>>())
        } else {
            Value::Null
        },
    );
    m
}

// ---------------------------------------------------------------- where a field points

/// Where the rows of a file's field live: its table, its column, and what
/// makes a row of that table live.
struct Target {
    table: String,
    column: &'static str,
    live: &'static str,
}

fn column_in(columns: &[(&'static str, &'static str)], field: &str) -> Option<&'static str> {
    columns.iter().find(|(f, _)| *f == field).map(|(_, c)| *c)
}

const ROUTE_LIVE: &str = "EXISTS (SELECT 1 FROM gtfs_route r WHERE r.gtfs_id = t.gtfs_id \
                          AND r.route_id = t.route_id AND NOT r.deleted)";

/// The table and column a GTFS `file.field` is stored in.
fn target(file: &str, field: &str) -> Option<Target> {
    let t = |table: &str, column: &'static str, live: &'static str| {
        Some(Target {
            table: table.to_string(),
            column,
            live,
        })
    };
    match file {
        "stops.txt" => match field {
            "stop_id" => t("gtfs_stop", "stop_id", "NOT t.deleted"),
            f => t("gtfs_stop", column_in(STOP_COLUMNS, f)?, "NOT t.deleted"),
        },
        "routes.txt" => match field {
            "route_id" => t("gtfs_route", "route_id", "NOT t.deleted"),
            f => t("gtfs_route", column_in(ROUTE_COLUMNS, f)?, "NOT t.deleted"),
        },
        "trips.txt" => match field {
            "trip_id" => t("gtfs_trip", "trip_id", ROUTE_LIVE),
            "route_id" => t("gtfs_trip", "route_id", ROUTE_LIVE),
            "service_id" => t("gtfs_trip", "service_id", ROUTE_LIVE),
            f => t("gtfs_trip", column_in(TRIP_COLUMNS, f)?, ROUTE_LIVE),
        },
        "stop_times.txt" => match field {
            "stop_id" => t("gtfs_route_stop", "stop_id", ROUTE_LIVE),
            "pickup_booking_rule_id" => t("gtfs_route_stop", "pickup_booking_rule_id", ROUTE_LIVE),
            "drop_off_booking_rule_id" => {
                t("gtfs_route_stop", "drop_off_booking_rule_id", ROUTE_LIVE)
            }
            _ => None,
        },
        "calendar.txt" | "calendar_dates.txt" if field == "service_id" => {
            t("gtfs_service", "service_id", "true")
        }
        "frequencies.txt" if field == "trip_id" => t("gtfs_frequency", "trip_id", "true"),
        _ => {
            let fspec = spec::file(file)?;
            let table = fspec.table()?;
            let column = fspec.field(field)?.name;
            Some(Target {
                table,
                column,
                live: "true",
            })
        }
    }
}

async fn value_exists(
    conn: &mut PgConnection,
    g: &str,
    t: &Target,
    value: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {} t WHERE t.gtfs_id = $1 AND t.{}::text = $2 AND {})",
        t.table, t.column, t.live
    ))
    .bind(g)
    .bind(value)
    .fetch_one(&mut *conn)
    .await
}

async fn count_using(
    conn: &mut PgConnection,
    g: &str,
    t: &Target,
    value: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {} t WHERE t.gtfs_id = $1 AND t.{}::text = $2 AND {}",
        t.table, t.column, t.live
    ))
    .bind(g)
    .bind(value)
    .fetch_one(&mut *conn)
    .await
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Every field of `row` that points at something that is not there, among
/// `fields` (the ones the edit set).
async fn dangling(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    row: &Row,
    fields: &[&str],
) -> Result<Vec<Finding>, sqlx::Error> {
    let mut out = Vec::new();
    for fs in fspec.fields {
        if fs.refs.is_empty() || !fields.contains(&fs.name) {
            continue;
        }
        let Some(value) = row.get(fs.name).and_then(as_text) else {
            continue;
        };
        let mut found = false;
        for r in fs.refs {
            if let Some(t) = target(r.file, r.field) {
                if value_exists(conn, g, &t, &value).await? {
                    found = true;
                    break;
                }
            }
        }
        if !found {
            let names: Vec<String> = fs
                .refs
                .iter()
                .map(|r| format!("{}.{}", r.file, r.field))
                .collect();
            out.push(Finding::error(
                "reference_not_found",
                fs.name,
                format!("{} {value:?} is not a {}", fs.name, names.join(" or ")),
            ));
        }
    }
    Ok(out)
}

/// What still points at `file.field = value`: `(file, field, rows)` for each
/// field of any file that does.
pub(super) async fn used_by(
    conn: &mut PgConnection,
    g: &str,
    file: &str,
    field: &str,
    value: &str,
) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    let mut out = Vec::new();
    for (src, fs) in spec::references_to(file, field) {
        if is_part_of(src.name, file) {
            continue;
        }
        let Some(t) = target(src.name, fs.name) else {
            continue;
        };
        // a stop is not its own user
        if src.name == file && fs.name == "parent_station" {
            let n: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM {} t WHERE t.gtfs_id = $1 AND t.{}::text = $2 \
                 AND t.stop_id <> $2 AND {}",
                t.table, t.column, t.live
            ))
            .bind(g)
            .bind(value)
            .fetch_one(&mut *conn)
            .await?;
            if n > 0 {
                out.push((src.name.to_string(), fs.name.to_string(), n));
            }
            continue;
        }
        let n = count_using(conn, g, &t, value).await?;
        if n > 0 {
            out.push((src.name.to_string(), fs.name.to_string(), n));
        }
    }
    Ok(out)
}

/// Rows of `src` that are parts of a row of `file`, which go with it rather than
/// use it: a trip's stop times and headways, a service's dates.
fn is_part_of(src: &str, file: &str) -> bool {
    matches!(
        (src, file),
        ("stop_times.txt" | "frequencies.txt", "trips.txt")
            | ("calendar_dates.txt", "calendar.txt")
            | ("calendar.txt", "calendar_dates.txt")
    )
}

/// "3 rows of pathways.txt (from_stop_id), 1 of transfers.txt (to_stop_id)"
pub(super) fn say_users(users: &[(String, String, i64)]) -> String {
    users
        .iter()
        .map(|(f, field, n)| format!("{n} of {f} ({field})"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------- apply

fn rule_findings(fspec: &FileSpec, row: &Row) -> Vec<Finding> {
    rules::row_findings(fspec.name, row)
        .into_iter()
        .map(|f| {
            if f.error {
                Finding::error(f.code, f.field, f.message)
            } else {
                Finding::warning(f.code, f.field, f.message)
            }
        })
        .collect()
}

/// A row of the file with the same natural key, other than `key`.
async fn natural_twin(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    row: &Row,
    key: &str,
) -> Result<Option<String>, sqlx::Error> {
    let Some(Key::Minted { natural }) = fspec.key() else {
        return Ok(None);
    };
    let mut sql = format!(
        "SELECT row_id FROM {} WHERE gtfs_id = $1 AND row_id <> $2",
        table(fspec)
    );
    for (i, f) in natural.iter().enumerate() {
        sql.push_str(&format!(" AND coalesce({f}::text, '') = ${}", i + 3));
    }
    sql.push_str(" LIMIT 1");
    let mut q = sqlx::query_scalar(&sql).bind(g).bind(key);
    for f in natural.iter() {
        let v = row.get(f).map(|v| {
            // compare as Postgres writes the column: a date ISO, a time its
            // seconds, a number as written
            as_text(v).unwrap_or_default()
        });
        q = q.bind(v.unwrap_or_default());
    }
    q.fetch_optional(&mut *conn).await
}

/// The columns a write names: every field of the file, and who wrote it.
fn write_columns(fspec: &FileSpec) -> Vec<&'static str> {
    let mut cols: Vec<&'static str> = if entity(fspec) == "shape" {
        vec![
            "shape_pt_sequence",
            "shape_pt_lat",
            "shape_pt_lon",
            "shape_dist_traveled",
        ]
    } else {
        fspec
            .fields
            .iter()
            .map(|f| f.name)
            .filter(|f| !matches!(fspec.key(), Some(Key::Field(k)) if k == *f))
            .collect()
    };
    cols.push("updated_by");
    cols
}

fn write_object(fspec: &FileSpec, row: &Row, after: &Value, actor: &str) -> Map<String, Value> {
    let mut o = if entity(fspec) == "shape" {
        shape_columns(after)
    } else {
        let mut o = Map::new();
        for fs in fspec.fields {
            o.insert(
                fs.name.into(),
                row.get(fs.name).cloned().unwrap_or(Value::Null),
            );
        }
        o
    };
    o.insert("updated_by".into(), json!(actor));
    o
}

/// Apply one record change inside the draft's replay; see the module doc.
pub(super) async fn apply(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    c: &ChangeRow,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    let key = c.entity_key.as_str();
    let tbl = table(fspec);
    let key_col = key_column(fspec);
    let live = stored_row(conn, g, fspec, key).await?;
    let is_shape = entity(fspec) == "shape";
    match c.op.as_str() {
        "create" => {
            if live.is_some() {
                return Err(fail(
                    "record_exists",
                    format!("{} already exists", describe(fspec, key)),
                ));
            }
            let row = merged(fspec, &Row::new(), &c.after);
            if let Some(twin) = natural_twin(conn, g, fspec, &row, key).await? {
                return Err(fail(
                    "duplicate_record",
                    format!(
                        "{} says what {} already says",
                        describe(fspec, key),
                        describe(fspec, &twin)
                    ),
                ));
            }
            let mut o = write_object(fspec, &row, &c.after, actor);
            o.insert("gtfs_id".into(), json!(g));
            if key_col != "gtfs_id" {
                o.insert(key_col.into(), json!(key));
            }
            let mut cols = write_columns(fspec);
            cols.push("gtfs_id");
            if key_col != "gtfs_id" {
                cols.push(key_col);
            }
            let cols = cols.join(", ");
            sqlx::query(&format!(
                "INSERT INTO {tbl} ({cols}) SELECT {cols} FROM jsonb_populate_record(NULL::{tbl}, $1::jsonb)"
            ))
            .bind(Value::Object(o).to_string())
            .execute(&mut *conn)
            .await?;
            if is_shape {
                return Ok(vec![]);
            }
            let fields: Vec<&str> = row.keys().copied().collect();
            let mut found = rule_findings(fspec, &row);
            found.extend(dangling(conn, g, fspec, &row, &fields).await?);
            // an error here blocks submit, but the row stays written, as a stop
            // list that breaks a fare rule does: later changes of the draft see
            // what the person drafted
            Ok(found)
        }
        "update" => {
            let Some(stored) = live else {
                return Err(fail(
                    "record_not_found",
                    format!("{} does not exist", describe(fspec, key)),
                ));
            };
            let before = row_of(fspec, &stored);
            let row = merged(fspec, &before, &c.after);
            if let Some(twin) = natural_twin(conn, g, fspec, &row, key).await? {
                return Err(fail(
                    "duplicate_record",
                    format!(
                        "{} would say what {} already says",
                        describe(fspec, key),
                        describe(fspec, &twin)
                    ),
                ));
            }
            let after = if is_shape && c.after.get("points").is_none() {
                // an update that names no points keeps the ones it has
                shape_json(&stored)
            } else {
                c.after.clone()
            };
            let o = write_object(fspec, &row, &after, actor);
            let cols = write_columns(fspec).join(", ");
            sqlx::query(&format!(
                "UPDATE {tbl} SET ({cols}) = (SELECT {cols} FROM jsonb_populate_record(NULL::{tbl}, $3::jsonb)) \
                 WHERE gtfs_id = $1 AND {key_col} = $2"
            ))
            .bind(g)
            .bind(key)
            .bind(Value::Object(o).to_string())
            .execute(&mut *conn)
            .await?;
            if is_shape {
                return Ok(vec![]);
            }
            // what the edit changed is checked; what the row already broke is a
            // warning
            let changed: Vec<&str> = fspec
                .fields
                .iter()
                .map(|f| f.name)
                .filter(|f| row.get(f) != before.get(f))
                .collect();
            let mut found =
                grade_against_live(rule_findings(fspec, &row), &rule_findings(fspec, &before));
            found.extend(dangling(conn, g, fspec, &row, &changed).await?);
            // an error here blocks submit, but the row stays written, as a stop
            // list that breaks a fare rule does: later changes of the draft see
            // what the person drafted
            Ok(found)
        }
        "delete" => {
            let Some(stored) = live else {
                return Err(fail(
                    "record_not_found",
                    format!("{} does not exist", describe(fspec, key)),
                ));
            };
            let row = row_of(fspec, &stored);
            let mut users = Vec::new();
            for fs in fspec.fields {
                if spec::references_to(fspec.name, fs.name).is_empty() {
                    continue;
                }
                let value = if is_shape && fs.name == "shape_id" {
                    Some(key.to_string())
                } else {
                    row.get(fs.name).and_then(as_text)
                };
                let Some(value) = value else { continue };
                // a value another row of the file still carries stays pointed at
                let is_key = key_col == fs.name;
                if !is_key {
                    let others: bool = sqlx::query_scalar(&format!(
                        "SELECT EXISTS (SELECT 1 FROM {tbl} WHERE gtfs_id = $1 AND {}::text = $2 AND {key_col} <> $3)",
                        fs.name
                    ))
                    .bind(g)
                    .bind(&value)
                    .bind(key)
                    .fetch_one(&mut *conn)
                    .await?;
                    if others {
                        continue;
                    }
                }
                users.extend(used_by(conn, g, fspec.name, fs.name, &value).await?);
            }
            if !users.is_empty() {
                return Err(fail(
                    "record_in_use",
                    format!(
                        "{} is still used by {}; change or remove those first",
                        describe(fspec, key),
                        say_users(&users)
                    ),
                ));
            }
            sqlx::query(&format!(
                "DELETE FROM {tbl} WHERE gtfs_id = $1 AND {key_col} = $2"
            ))
            .bind(g)
            .bind(key)
            .execute(&mut *conn)
            .await?;
            Ok(vec![])
        }
        op => Err(fail(
            "invalid_change",
            format!("unsupported change {}/{op}", entity(fspec)),
        )),
    }
}

// ---------------------------------------------------------------- adding a change

/// A record's `before` in read shape and its version, when a change is added;
/// a row an earlier change of the draft creates has no version, and its
/// `before` is that create's `after`.
pub(super) async fn snapshot(
    conn: &mut PgConnection,
    change_set_id: Uuid,
    g: &str,
    fspec: &FileSpec,
    op: &str,
    key: &str,
) -> EditorResult<(Value, Option<i32>)> {
    if op == "create" {
        return Ok((Value::Null, None));
    }
    if let Some(stored) = stored_row(conn, g, fspec, key).await? {
        let version = stored["row_version"].as_i64().map(|v| v as i32);
        return Ok((read_json(fspec, &stored), version));
    }
    let created: Option<String> = sqlx::query_scalar(
        "SELECT after::text FROM gtfs_change WHERE change_set_id = $1 AND entity = $2 \
         AND entity_key = $3 AND op = 'create' ORDER BY position DESC LIMIT 1",
    )
    .bind(change_set_id)
    .bind(entity(fspec))
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    match created {
        Some(after) => Ok((serde_json::from_str(&after).unwrap_or(Value::Null), None)),
        None => Err(EditorError::not_found(
            "entity_not_found",
            format!("{} does not exist", describe(fspec, key)),
        )),
    }
}

/// The live rows among `keys`, in read shape with their versions.
pub(super) async fn live_rows(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    keys: &[String],
) -> EditorResult<std::collections::HashMap<String, (Value, i32)>> {
    let sql = format!(
        "SELECT t.{key}::text AS key, to_jsonb(t)::text AS row FROM {} t \
         WHERE t.gtfs_id = $1 AND t.{key} = ANY($2)",
        table(fspec),
        key = key_column(fspec)
    );
    let mut out = std::collections::HashMap::new();
    for r in sqlx::query(&sql)
        .bind(g)
        .bind(keys)
        .fetch_all(&mut *conn)
        .await?
    {
        let stored: Value =
            serde_json::from_str(&r.try_get::<String, _>("row")?).unwrap_or(Value::Null);
        let version = stored["row_version"].as_i64().unwrap_or(0) as i32;
        out.insert(
            r.try_get::<String, _>("key")?,
            (read_json(fspec, &stored), version),
        );
    }
    Ok(out)
}

/// The live version of a record, for a conflict check.
pub(super) async fn live_version(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    key: &str,
) -> EditorResult<Option<i32>> {
    Ok(stored_row(conn, g, fspec, key)
        .await?
        .and_then(|r| r["row_version"].as_i64().map(|v| v as i32)))
}

/// Stops a record change names, for the merged-away check.
pub(super) fn referenced_stops(fspec: &FileSpec, after: &Value) -> Vec<String> {
    fspec
        .fields
        .iter()
        .filter(|fs| {
            fs.refs
                .iter()
                .any(|r| r.file == "stops.txt" && r.field == "stop_id")
        })
        .filter_map(|fs| after[fs.name].as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------- reads

/// Every file of the reference, with how the editor keeps it and how many rows
/// feed `g` has in it.
pub async fn list_files(conn: &mut PgConnection, g: &str) -> EditorResult<Value> {
    let count = |sql: &'static str| sql;
    let bespoke: [(&str, &str); 7] = [
        ("stops.txt", count("SELECT count(*) FROM gtfs_stop WHERE gtfs_id = $1 AND NOT deleted")),
        ("routes.txt", count("SELECT count(*) FROM gtfs_route WHERE gtfs_id = $1 AND NOT deleted")),
        (
            "trips.txt",
            count(
                "SELECT count(*) FROM gtfs_trip t WHERE t.gtfs_id = $1 AND EXISTS (SELECT 1 FROM gtfs_route r \
                 WHERE r.gtfs_id = t.gtfs_id AND r.route_id = t.route_id AND NOT r.deleted)",
            ),
        ),
        (
            "stop_times.txt",
            count(
                "SELECT coalesce(sum(n), 0)::bigint FROM gtfs_trip t JOIN (SELECT gtfs_id, route_id, pattern_key, count(*) n \
                 FROM gtfs_route_stop WHERE gtfs_id = $1 AND stop_type IN ('NEW STOP', 'INTERMEDIATE STOP') \
                 GROUP BY 1, 2, 3) p ON p.gtfs_id = t.gtfs_id AND p.route_id = t.route_id AND p.pattern_key = t.pattern_key \
                 JOIN gtfs_route r ON r.gtfs_id = t.gtfs_id AND r.route_id = t.route_id AND NOT r.deleted \
                 WHERE t.gtfs_id = $1",
            ),
        ),
        ("calendar.txt", count("SELECT count(*) FROM gtfs_service WHERE gtfs_id = $1 AND start_date IS NOT NULL")),
        ("calendar_dates.txt", count("SELECT count(*) FROM gtfs_service_date WHERE gtfs_id = $1")),
        ("frequencies.txt", count("SELECT count(*) FROM gtfs_frequency WHERE gtfs_id = $1")),
    ];
    let mut items = Vec::new();
    for fspec in spec::FILES {
        let n: i64 = match fspec.table() {
            Some(t) => {
                sqlx::query_scalar(&format!("SELECT count(*) FROM {t} WHERE gtfs_id = $1"))
                    .bind(g)
                    .fetch_one(&mut *conn)
                    .await?
            }
            None => match bespoke.iter().find(|(f, _)| *f == fspec.name) {
                Some((_, sql)) => {
                    sqlx::query_scalar(sql)
                        .bind(g)
                        .fetch_one(&mut *conn)
                        .await?
                }
                None => 0,
            },
        };
        items.push(json!({
            "file": fspec.name,
            "stem": fspec.stem(),
            "label": fspec.label,
            "group": fspec.group,
            "storage": if fspec.table().is_some() { "record" } else { "bespoke" },
            "entity": fspec.entity(),
            "rows": n,
        }));
    }
    Ok(json!({"items": items, "next_cursor": null}))
}

/// A record file's rows, in the order the feed had them, `q` matching any of
/// their text.
pub async fn list_records(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    q: Option<&str>,
    page: &Page,
) -> EditorResult<Value> {
    let key_col = key_column(fspec);
    let q = q.map(str::trim).filter(|q| !q.is_empty());
    let sql = format!(
        "SELECT to_jsonb(t)::text FROM {} t WHERE t.gtfs_id = $1 \
         AND ($2::text IS NULL OR to_jsonb(t)::text ILIKE '%' || $2 || '%') \
         ORDER BY t.sort_key NULLS LAST, t.{key_col} LIMIT $3 OFFSET $4",
        table(fspec)
    );
    let rows = sqlx::query(&sql)
        .bind(g)
        .bind(q)
        .bind(page.limit + 1)
        .bind(page.offset)
        .fetch_all(&mut *conn)
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| -> Result<Value, sqlx::Error> {
            let text: String = r.try_get(0)?;
            Ok(read_json(
                fspec,
                &serde_json::from_str(&text).unwrap_or(Value::Null),
            ))
        })
        .collect::<Result<_, _>>()?;
    Ok(page.wrap(items))
}

/// One record, with what points at it.
pub async fn record_detail(
    conn: &mut PgConnection,
    g: &str,
    fspec: &FileSpec,
    key: &str,
) -> EditorResult<Value> {
    let stored = stored_row(conn, g, fspec, key).await?.ok_or_else(|| {
        EditorError::not_found(
            "record_not_found",
            format!("{} does not exist", describe(fspec, key)),
        )
    })?;
    let mut out = read_json(fspec, &stored);
    let row = row_of(fspec, &stored);
    let mut users = Vec::new();
    for fs in fspec.fields {
        if spec::references_to(fspec.name, fs.name).is_empty() {
            continue;
        }
        let value = if entity(fspec) == "shape" && fs.name == "shape_id" {
            Some(key.to_string())
        } else {
            row.get(fs.name).and_then(as_text)
        };
        if let Some(v) = value {
            for (file, field, n) in used_by(conn, g, fspec.name, fs.name, &v).await? {
                users.push(json!({"file": file, "field": field, "rows": n}));
            }
        }
    }
    out["used_by"] = json!(users);
    Ok(out)
}

// ---------------------------------------------------------------- the editor's own tables' GTFS fields

/// Fields of stops.txt the stop changes set by their GTFS names since 0023:
/// each is a `gtfs_stop` column of the same name.
pub const STOP_GTFS_FIELDS: &[&str] = &[
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

/// Fields of routes.txt the route changes set by their GTFS names since 0023:
/// each is a `gtfs_route` column of the same name.
pub const ROUTE_GTFS_FIELDS: &[&str] = &[
    "route_desc",
    "route_url",
    "route_sort_order",
    "continuous_pickup",
    "continuous_drop_off",
    "network_id",
];

/// Shape check of the `fields` of `file` a change sends: each a value of its
/// type (null or blank clears it).
pub fn check_fields(
    file: &str,
    m: &Map<String, Value>,
    fields: &[&str],
    what: &str,
) -> Result<(), Finding> {
    let fspec = spec::file(file).expect("a file of the reference");
    for f in fields {
        let Some(v) = m.get(*f) else { continue };
        let fs = fspec.field(f).expect("a field of the file");
        if let Err(why) = spec::from_api(fs, v) {
            return Err(invalid("invalid_value", f, format!("{what}: {f} {why}")));
        }
    }
    Ok(())
}

/// Write the `fields` of `file` a change sends into row `key` of `table`:
/// canonical values, a null or blank one clearing the column.
#[allow(clippy::too_many_arguments)]
pub(super) async fn write_fields(
    conn: &mut PgConnection,
    g: &str,
    table: &str,
    key_col: &str,
    key: &str,
    file: &str,
    m: &Map<String, Value>,
    fields: &[&str],
) -> Result<(), sqlx::Error> {
    let fspec = spec::file(file).expect("a file of the reference");
    let mut given = Map::new();
    for f in fields {
        let Some(v) = m.get(*f) else { continue };
        let fs = fspec.field(f).expect("a field of the file");
        given.insert(
            f.to_string(),
            spec::from_api(fs, v).ok().flatten().unwrap_or(Value::Null),
        );
    }
    if given.is_empty() {
        return Ok(());
    }
    let cols: Vec<&str> = given.keys().map(String::as_str).collect();
    let cols = cols.join(", ");
    // a single column still takes the row form
    let sql = format!(
        "UPDATE {table} SET ({cols}) = (SELECT {cols} FROM jsonb_populate_record(NULL::{table}, $3::jsonb)) \
         WHERE gtfs_id = $1 AND {key_col} = $2"
    );
    let sql = if given.len() == 1 {
        format!(
            "UPDATE {table} SET {cols} = (SELECT {cols} FROM jsonb_populate_record(NULL::{table}, $3::jsonb)) \
             WHERE gtfs_id = $1 AND {key_col} = $2"
        )
    } else {
        sql
    };
    sqlx::query(&sql)
        .bind(g)
        .bind(key)
        .bind(Value::Object(given).to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A stop or a route as its GTFS row (stops.txt / routes.txt field names,
/// canonical), live or not.
pub(super) async fn gtfs_row(
    conn: &mut PgConnection,
    g: &str,
    file: &str,
    key: &str,
) -> Result<Option<Row>, sqlx::Error> {
    let (table, key_col, columns) = match file {
        "stops.txt" => ("gtfs_stop", "stop_id", STOP_COLUMNS),
        "routes.txt" => ("gtfs_route", "route_id", ROUTE_COLUMNS),
        _ => return Ok(None),
    };
    let row: Option<String> = sqlx::query_scalar(&format!(
        "SELECT to_jsonb(t)::text FROM {table} t WHERE t.gtfs_id = $1 AND t.{key_col} = $2"
    ))
    .bind(g)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|text| {
        let stored: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let fspec = spec::file(file).expect("a file of the reference");
        super::feed_io::fields_of(fspec, &stored, columns, key, &mut Vec::new())
    }))
}

/// The GTFS `fields` of stop or route `key` as the API writes them, `null`
/// where unset: what the stop and route details carry as `gtfs` for the
/// dashboard's forms.
pub async fn gtfs_fields(
    conn: &mut PgConnection,
    g: &str,
    file: &str,
    key: &str,
    fields: &[&str],
) -> Result<Value, sqlx::Error> {
    let row = gtfs_row(conn, g, file, key).await?.unwrap_or_default();
    let fspec = spec::file(file).expect("a file of the reference");
    let mut out = Map::new();
    for f in fields {
        let fs = fspec.field(f).expect("a field of the file");
        out.insert(f.to_string(), spec::to_api(fs, &row.get(f).cloned()));
    }
    Ok(Value::Object(out))
}

/// What a stop change leaves broken, by the reference: the row's own rules and
/// the stops and levels it names, graded against the stop as it was; and what
/// its parent is - a station above a platform, an entrance or a node, a
/// platform above a boarding area.
pub(super) async fn stop_findings(
    conn: &mut PgConnection,
    g: &str,
    id: &str,
    before: &Row,
    after: &Row,
) -> Result<Vec<Finding>, sqlx::Error> {
    let fspec = spec::file("stops.txt").expect("stops.txt");
    let mut found = grade_against_live(rule_findings(fspec, after), &rule_findings(fspec, before));
    let changed: Vec<&str> = fspec
        .fields
        .iter()
        .map(|f| f.name)
        .filter(|f| after.get(f) != before.get(f))
        .collect();
    found.extend(dangling(conn, g, fspec, after, &changed).await?);
    let lt = after
        .get("location_type")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let parent = after.get("parent_station").and_then(Value::as_str);
    if changed.contains(&"parent_station") || changed.contains(&"location_type") {
        if let Some(p) = parent.filter(|p| *p != id) {
            let parent_type: Option<i16> = sqlx::query_scalar(
                "SELECT location_type FROM gtfs_stop WHERE gtfs_id = $1 AND stop_id = $2 AND NOT deleted",
            )
            .bind(g)
            .bind(p)
            .fetch_optional(&mut *conn)
            .await?;
            let want: i16 = if lt == 4 { 0 } else { 1 };
            if parent_type.is_some_and(|t| t != want) {
                found.push(Finding::error(
                    "parent_wrong_type",
                    "parent_station",
                    if want == 0 {
                        format!("a boarding area stands on a platform; {p} is not one")
                    } else {
                        format!("{id}'s parent_station is a station; {p} is not one")
                    },
                ));
            }
        }
        if parent == Some(id) {
            found.push(Finding::error(
                "parent_is_itself",
                "parent_station",
                format!("stop {id} cannot be its own parent_station"),
            ));
        }
    }
    Ok(found)
}

/// What a route change leaves broken, by the reference: the row's own rules
/// and the agency it names, graded against the route as it was.
pub(super) async fn route_findings(
    conn: &mut PgConnection,
    g: &str,
    before: &Row,
    after: &Row,
) -> Result<Vec<Finding>, sqlx::Error> {
    let fspec = spec::file("routes.txt").expect("routes.txt");
    let mut found = grade_against_live(rule_findings(fspec, after), &rule_findings(fspec, before));
    let changed: Vec<&str> = fspec
        .fields
        .iter()
        .map(|f| f.name)
        .filter(|f| after.get(f) != before.get(f))
        .collect();
    // an agency_id is a reference only in a feed that keeps agencies (0023
    // gives every feed its one)
    let has_agencies: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM gtfs_agency WHERE gtfs_id = $1)")
            .bind(g)
            .fetch_one(&mut *conn)
            .await?;
    let changed: Vec<&str> = changed
        .into_iter()
        .filter(|f| *f != "agency_id" || has_agencies)
        .collect();
    found.extend(dangling(conn, g, fspec, after, &changed).await?);
    Ok(found)
}

/// Point every record that names stop `from` at `into` (a merge, section 18):
/// pathways, transfers, stops in areas and in location groups, fare leg join
/// rules - and, for a stop merge, the boarding areas standing on it. A row the
/// switch would make say what another row already says is removed rather than
/// kept twice. Returns how many rows now name `into`.
pub(super) async fn repoint_stop(
    conn: &mut PgConnection,
    g: &str,
    from: &str,
    into: &str,
    boarding_areas: bool,
) -> Result<u64, sqlx::Error> {
    let mut moved = 0;
    for (src, fs) in spec::references_to("stops.txt", "stop_id") {
        let Some(tbl) = src.table() else { continue };
        let col = fs.name;
        if let Some(Key::Minted { natural }) = src.key() {
            let same: Vec<String> = natural
                .iter()
                .map(|n| {
                    if *n == col {
                        format!("coalesce(o.{n}::text, '') = $3")
                    } else {
                        format!("coalesce(o.{n}::text, '') = coalesce(t.{n}::text, '')")
                    }
                })
                .collect();
            sqlx::query(&format!(
                "DELETE FROM {tbl} t WHERE t.gtfs_id = $1 AND t.{col} = $2 AND EXISTS \
                 (SELECT 1 FROM {tbl} o WHERE o.gtfs_id = t.gtfs_id AND o.row_id <> t.row_id AND {})",
                same.join(" AND ")
            ))
            .bind(g)
            .bind(from)
            .bind(into)
            .execute(&mut *conn)
            .await?;
        }
        moved += sqlx::query(&format!(
            "UPDATE {tbl} SET {col} = $3 WHERE gtfs_id = $1 AND {col} = $2"
        ))
        .bind(g)
        .bind(from)
        .bind(into)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    if boarding_areas {
        moved += sqlx::query(
            "UPDATE gtfs_stop SET parent_station = $3 \
             WHERE gtfs_id = $1 AND parent_station = $2 AND location_type = 4 AND NOT deleted",
        )
        .bind(g)
        .bind(from)
        .bind(into)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str) -> &'static FileSpec {
        spec::file(name).unwrap()
    }

    #[test]
    fn a_create_names_every_required_field_and_nothing_else() {
        let pw = f("pathways.txt");
        let ok = json!({"pathway_id": "PW1", "from_stop_id": "A", "to_stop_id": "B",
                        "pathway_mode": 2, "is_bidirectional": 1, "length": "12.5"});
        assert!(check_payload(pw, "create", "PW1", &ok).is_ok());
        let e = check_payload(
            pw,
            "create",
            "PW1",
            &json!({"pathway_id": "PW1", "from_stop_id": "A"}),
        )
        .unwrap_err();
        assert_eq!(e.code, "missing_field");
        let e = check_payload(
            pw,
            "create",
            "PW1",
            &json!({"pathway_id": "PW1", "colour": 1}),
        )
        .unwrap_err();
        assert_eq!(e.code, "unknown_field");
        let mut bad = ok.clone();
        bad["pathway_mode"] = json!(9);
        assert_eq!(
            check_payload(pw, "create", "PW1", &bad).unwrap_err().code,
            "invalid_value"
        );
        // the id is the key, and is not renamed
        let e = check_payload(pw, "update", "PW1", &json!({"pathway_id": "PW2"})).unwrap_err();
        assert_eq!(e.code, "key_mismatch");
        assert_eq!(
            check_payload(pw, "update", "PW1", &json!({"length": null})).map(|_| ()),
            Ok(())
        );
        let e = check_payload(pw, "update", "PW1", &json!({"pathway_mode": null})).unwrap_err();
        assert_eq!(e.code, "missing_field", "a required field is not cleared");
        assert_eq!(check_payload(pw, "delete", "PW1", &Value::Null), Ok(()));
        assert!(check_payload(pw, "delete", "PW1", &json!({"x": 1})).is_err());
    }

    #[test]
    fn feed_info_is_the_feeds_and_its_id_is_not_edited() {
        let fi = f("feed_info.txt");
        let e = check_payload(fi, "update", "g", &json!({"feed_id": "other"})).unwrap_err();
        assert_eq!(e.code, "invalid_payload");
        assert!(check_payload(fi, "update", "g", &json!({"feed_version": "v8"})).is_ok());
    }

    #[test]
    fn a_shape_is_its_points() {
        let sh = f("shapes.txt");
        let pts = json!({"shape_id": "S", "points": [{"lat": 13.0, "lon": 80.0}, {"lat": 13.1, "lon": 80.1, "dist": 1.2}]});
        assert!(check_payload(sh, "create", "S", &pts).is_ok());
        let one = json!({"shape_id": "S", "points": [{"lat": 13.0, "lon": 80.0}]});
        assert_eq!(
            check_payload(sh, "create", "S", &one).unwrap_err().code,
            "shape_too_short"
        );
        let back = json!({"shape_id": "S", "points": [{"lat": 13.0, "lon": 80.0, "dist": 2}, {"lat": 13.1, "lon": 80.1, "dist": 1}]});
        assert_eq!(
            check_payload(sh, "create", "S", &back).unwrap_err().code,
            "invalid_value"
        );
        let cols = shape_columns(&pts);
        assert_eq!(cols["shape_pt_sequence"], json!([1, 2]));
        assert_eq!(cols["shape_dist_traveled"], json!([null, 1.2]));
    }

    #[test]
    fn a_file_with_no_id_is_keyed_by_a_minted_one() {
        assert!(mints_key("transfer", "create"));
        assert!(!mints_key("pathway", "create"));
        assert_eq!(create_key_field("pathway", "create"), Some("pathway_id"));
        assert_eq!(create_key_field("transfer", "create"), None);
        let id = mint_row_id();
        assert!(id.starts_with("r_") && id.len() == 12, "{id}");
        assert_eq!(key_column(f("transfers.txt")), "row_id");
        assert_eq!(key_column(f("feed_info.txt")), "gtfs_id");
    }

    #[test]
    fn every_reference_the_spec_names_has_a_table_to_look_in() {
        for fspec in spec::FILES {
            for fs in fspec.fields {
                for r in fs.refs {
                    assert!(
                        target(r.file, r.field).is_some(),
                        "{}.{} -> {}.{}",
                        fspec.name,
                        fs.name,
                        r.file,
                        r.field
                    );
                }
                if !spec::references_to(fspec.name, fs.name).is_empty() {
                    for (src, sf) in spec::references_to(fspec.name, fs.name) {
                        // a Flex zone call (stop_times naming a location or a
                        // location group) is not kept (section 18)
                        let flex_call = src.name == "stop_times.txt"
                            && matches!(sf.name, "location_group_id" | "location_id");
                        if is_part_of(src.name, fspec.name) || flex_call {
                            continue;
                        }
                        assert!(
                            target(src.name, sf.name).is_some(),
                            "{}.{} is read from nowhere",
                            src.name,
                            sf.name
                        );
                    }
                }
            }
        }
    }
}
