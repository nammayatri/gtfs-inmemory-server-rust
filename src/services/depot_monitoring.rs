//! Depot-day reads over the MTC replica: a depot's waybills with their trips, routes, crew and
//! whole-day counts. The replica holds only Chennai and has no gtfs_id column, so the handler only
//! sends feeds with an external store here.
//!
//! Every query binds the same parameters: $1 depot (bigint), $2 service date (text, as stored in
//! `waybills.duty_date`), $3 search, and for pages $4 limit and $5 offset.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgPool, Postgres, Transaction};

use crate::tools::error::{AppError, AppResult};

const SOURCE: &str = "mtc_replica";
const MAX_SEARCH_CHARS: usize = 100;

#[derive(Debug)]
pub struct DepotDayRequest {
    depot_id: i64,
    service_date: String,
    page: i64,
    page_size: i64,
    search: String,
}

impl DepotDayRequest {
    pub fn new(
        depot_id: &str,
        service_date: NaiveDate,
        page: Option<i64>,
        page_size: Option<i64>,
        search: Option<&str>,
    ) -> AppResult<Self> {
        let depot_id_int = depot_id
            .parse::<i64>()
            .map_err(|_| AppError::BadRequest(format!("Invalid depot_id: {}", depot_id)))?;
        let search = search.unwrap_or("").trim();
        if search.chars().count() > MAX_SEARCH_CHARS {
            return Err(AppError::BadRequest(format!(
                "q is longer than {} characters",
                MAX_SEARCH_CHARS
            )));
        }
        Ok(Self {
            depot_id: depot_id_int,
            service_date: service_date.format("%Y-%m-%d").to_string(),
            page: page.unwrap_or(1).max(1),
            page_size: page_size.unwrap_or(50).clamp(1, 200),
            search: search.to_owned(),
        })
    }

    fn offset(&self) -> i64 {
        (self.page - 1).saturating_mul(self.page_size)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaybillTrip {
    pub trip_id: String,
    pub route_id: Option<String>,
    pub route_number: Option<String>,
    pub route_name: Option<String>,
    pub direction: Option<String>,
    pub trip_number: i32,
    pub trip_order: i32,
    pub trip_type: Option<String>,
    pub scheduled_start: Option<String>,
    pub scheduled_end: Option<String>,
    pub recorded_active: bool,
    /// The replica has no completion flag; kept so the response shape stays stable.
    pub recorded_completed: Option<bool>,
    /// Recorded start/end epoch (text), only when it falls on the service day in IST.
    pub actual_start: Option<String>,
    pub actual_end: Option<String>,
    /// Scheduled trip distance in km.
    pub planned_km: Option<f64>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct DepotWaybill {
    pub waybill_id: String,
    pub waybill_no: Option<String>,
    pub depot_id: String,
    pub service_date: String,
    /// Raw waybill vehicle identifier (fleet or registration number), not joined to the registry.
    pub vehicle_identifier: Option<String>,
    pub driver_token: Option<String>,
    pub conductor_token: Option<String>,
    pub schedule_no: Option<String>,
    pub status: Option<String>,
    pub is_flexi: bool,
    pub updated_at: Option<DateTime<Utc>>,
    pub trips: sqlx::types::Json<Vec<WaybillTrip>>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct DepotRoute {
    pub route_id: String,
    pub route_number: Option<String>,
    pub route_name: Option<String>,
    pub direction: Option<String>,
    /// Depot schedules effective on the day that run this route; 0 = only seen on a waybill.
    pub scheduled_schedules: i64,
    /// 0 = scheduled but nothing allocated that day.
    pub allocated_waybills: i64,
    /// Distinct raw waybill vehicle identifiers.
    pub allocated_vehicles: i64,
}

/// Roster assignment for the day, not attendance. No names or contacts.
#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct DepotCrew {
    pub employee_id: String,
    pub token: Option<String>,
    /// Source designation name (e.g. Driver, Conductor).
    pub designation: Option<String>,
    pub master_status: Option<String>,
    /// Waybills on the day naming this token.
    pub assigned_waybills: i64,
    pub assigned_roles: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotDayPage<T> {
    pub depot_id: String,
    pub service_date: String,
    pub source: &'static str,
    pub observed_at: DateTime<Utc>,
    pub rows: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

pub type WaybillPage = DepotDayPage<DepotWaybill>;
pub type RoutePage = DepotDayPage<DepotRoute>;
pub type CrewPage = DepotDayPage<DepotCrew>;

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct OverviewCounts {
    pub registered_vehicles: i64,
    pub waybills: i64,
    /// Distinct raw vehicle identifiers on the day's waybills.
    pub allocated_vehicle_identifiers: i64,
    /// Registry vehicles whose fleet_no or vehicle_no appears on a waybill.
    pub matched_vehicles: i64,
    /// Waybill buses registered under another depot.
    pub other_depot_vehicles: i64,
    /// Waybill identifiers not in the registry at all.
    pub unmatched_vehicle_identifiers: i64,
    pub rostered_crew: i64,
    pub assigned_crew_tokens: i64,
    /// Assigned tokens not on this depot's roster (visiting or unknown crew).
    pub unrostered_crew_tokens: i64,
    pub waybills_by_status: sqlx::types::Json<serde_json::Value>,
}

#[derive(Debug, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RouteCounts {
    pub scheduled_routes: i64,
    pub allocated_routes: i64,
}

/// Trips on the day's waybills. Regular trips share their schedule's rows, so recorded
/// start/end on a past day can belong to a later run of the same schedule.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TripCounts {
    pub planned_trips: i64,
    pub started_trips: i64,
    pub completed_trips: i64,
    pub active_trips: i64,
    /// Planned start passed (IST) with no recorded start; null unless the service date is today.
    pub overdue_unstarted_trips: Option<i64>,
}

/// Trips by planned start hour (IST, 0-23).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TripHour {
    pub hour: i32,
    pub planned: i64,
    pub started: i64,
    pub completed: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DepotOverview {
    pub depot_id: String,
    pub service_date: String,
    pub source: &'static str,
    pub observed_at: DateTime<Utc>,
    #[serde(flatten)]
    pub counts: OverviewCounts,
    #[serde(flatten)]
    pub routes: RouteCounts,
    #[serde(flatten)]
    pub trips: TripCounts,
    pub trips_by_hour: Vec<TripHour>,
}

/// One row per hour plus a grand-total row (`total`), from a single GROUPING SETS query.
#[derive(FromRow)]
struct TripGroup {
    total: bool,
    hour: Option<i32>,
    planned: i64,
    started: i64,
    completed: i64,
    active: i64,
    overdue: Option<i64>,
}

const DAY_WAYBILLS: &str =
    "SELECT * FROM waybills w WHERE w.entity_id = $1 AND w.deleted = false AND w.duty_date = $2";

/// Recorded start (epoch ms as text), kept only when it falls on the service date in IST.
/// Regular trips share their schedule's row, so every later run overwrites these columns.
fn recorded_start(t: &str) -> String {
    format!(
        "CASE WHEN {t}.trip_start_time >= (extract(epoch FROM $2::date::timestamp AT TIME ZONE 'Asia/Kolkata') * 1000)::bigint::text \
         AND {t}.trip_start_time < (extract(epoch FROM ($2::date + 1)::timestamp AT TIME ZONE 'Asia/Kolkata') * 1000)::bigint::text \
         THEN {t}.trip_start_time END"
    )
}

/// Recorded end of that run: after its start, so a trip past midnight keeps its end.
fn recorded_end(t: &str) -> String {
    format!(
        "CASE WHEN {t}.trip_end_time >= {start} THEN {t}.trip_end_time END",
        start = recorded_start(t)
    )
}

/// Trip rows of one waybill `w`: regular trips by schedule_trip_id, flexi trips by waybill_id.
fn waybill_trips(columns_regular: &str, columns_flexi: &str) -> String {
    format!(
        "SELECT {columns_regular} FROM bus_schedule_trip_detail d
         WHERE NOT w.is_flexi AND d.schedule_trip_id = w.schedule_trip_id AND d.deleted = false
         UNION ALL
         SELECT {columns_flexi} FROM bus_schedule_trip_flexi f
         WHERE w.is_flexi AND f.waybill_id = w.waybill_id AND f.deleted = false"
    )
}

/// Routes run by the depot's schedules effective on the day, plus routes on that day's waybills.
fn routes_cte() -> String {
    format!(
        r#"
        WITH day_w AS ({DAY_WAYBILLS}),
        sched AS (
            SELECT DISTINCT st.schedule_id, d.route_number_id::text AS route_id
            FROM bus_schedule s
            JOIN bus_schedule_trip st ON st.schedule_id = s.schedule_id AND st.deleted = false
                AND (st.effective_start_date IS NULL OR st.effective_start_date::date <= $2::date)
                AND (st.effective_end_date IS NULL OR st.effective_end_date::date >= $2::date)
            JOIN bus_schedule_trip_detail d ON d.schedule_trip_id = st.schedule_trip_id AND d.deleted = false
            WHERE s.entity_id = $1 AND s.deleted = false
                AND (s.effective_from IS NULL OR s.effective_from::date <= $2::date)
                AND (s.effective_till IS NULL OR s.effective_till::date >= $2::date)
        ),
        alloc AS (
            SELECT t.route_id, w.waybill_id, w.vehicle_no
            FROM day_w w
            CROSS JOIN LATERAL ({trips}) t
        ),
        items AS (
            SELECT ids.route_id, r.route_number, r.route_name, r.route_direction AS direction,
                (SELECT count(DISTINCT schedule_id) FROM sched WHERE sched.route_id = ids.route_id) AS scheduled_schedules,
                (SELECT count(DISTINCT waybill_id) FROM alloc WHERE alloc.route_id = ids.route_id) AS allocated_waybills,
                (SELECT count(DISTINCT vehicle_no) FROM alloc WHERE alloc.route_id = ids.route_id) AS allocated_vehicles
            FROM (SELECT route_id FROM sched UNION SELECT route_id FROM alloc) ids
            LEFT JOIN bus_route r ON r.route_id::text = ids.route_id AND r.deleted = false
            WHERE $3 = '' OR position(lower($3) in lower(concat_ws(' ', r.route_number, r.route_name))) > 0
        )"#,
        trips = waybill_trips(
            "d.route_number_id::text AS route_id",
            "f.route_number_id::text"
        ),
    )
}

fn db_error(error: sqlx::Error) -> AppError {
    tracing::error!(%error, "Depot monitoring query failed");
    AppError::NotReady("Depot monitoring source unavailable".into())
}

/// Count and page read from one snapshot, so `total` matches `rows`.
async fn read_only_tx(pool: &PgPool) -> AppResult<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await.map_err(db_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    Ok(tx)
}

/// Total and one page of `items`, a relation the `cte` defines.
async fn read_page<T>(
    pool: &PgPool,
    request: &DepotDayRequest,
    cte: &str,
    order: &str,
) -> AppResult<DepotDayPage<T>>
where
    T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    let mut tx = read_only_tx(pool).await?;
    let total: i64 = sqlx::query_scalar(&format!("{cte} SELECT count(*) FROM items"))
        .bind(request.depot_id)
        .bind(&request.service_date)
        .bind(&request.search)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
    let rows = sqlx::query_as::<_, T>(&format!(
        "{cte} SELECT * FROM items ORDER BY {order} LIMIT $4 OFFSET $5"
    ))
    .bind(request.depot_id)
    .bind(&request.service_date)
    .bind(&request.search)
    .bind(request.page_size)
    .bind(request.offset())
    .fetch_all(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;
    Ok(DepotDayPage {
        depot_id: request.depot_id.to_string(),
        service_date: request.service_date.clone(),
        source: SOURCE,
        observed_at: Utc::now(),
        rows,
        total,
        page: request.page,
        page_size: request.page_size,
    })
}

/// All shifts and recorded statuses of the day; a waybill count is not a bus count.
pub async fn read_waybills(pool: &PgPool, request: &DepotDayRequest) -> AppResult<WaybillPage> {
    let trips = waybill_trips(
        &format!(
            "d.schedule_trip_detail_id::text AS trip_id, d.route_number_id::text AS route_id, d.trip_number, d.trip_order, d.trip_type, d.start_time, d.end_time, d.is_active_trip, NULL::boolean AS is_completed, {} AS actual_start, {} AS actual_end, round(d.distance::numeric, 2)::float8 AS planned_km",
            recorded_start("d"),
            recorded_end("d")
        ),
        &format!(
            "f.schedule_trip_flexi_id::text, f.route_number_id::text, f.trip_number, f.trip_order, f.trip_type, f.start_time, f.end_time, f.is_active_trip, NULL::boolean, {}, {}, round(f.distance::numeric, 2)::float8",
            recorded_start("f"),
            recorded_end("f")
        ),
    );
    let cte = format!(
        r#"
        WITH items AS (
            SELECT w.waybill_id::text AS waybill_id, w.waybill_no, w.entity_id::text AS depot_id,
                   w.duty_date AS service_date, w.vehicle_no AS vehicle_identifier,
                   NULLIF(w.driver_token_no, '') AS driver_token, NULLIF(w.conductor_token_no, '') AS conductor_token,
                   w.schedule_no, w.status, w.is_flexi, w.updated_at::timestamptz AS updated_at,
                   COALESCE((SELECT jsonb_agg(jsonb_build_object(
                       'tripId', t.trip_id, 'routeId', t.route_id, 'routeNumber', r.route_number,
                       'routeName', r.route_name, 'direction', r.route_direction,
                       'tripNumber', t.trip_number, 'tripOrder', t.trip_order, 'tripType', t.trip_type,
                       'scheduledStart', t.start_time, 'scheduledEnd', t.end_time,
                       'recordedActive', t.is_active_trip, 'recordedCompleted', t.is_completed,
                       'actualStart', t.actual_start, 'actualEnd', t.actual_end, 'plannedKm', t.planned_km
                   ) ORDER BY t.trip_order, t.trip_id)
                   FROM ({trips}) t LEFT JOIN bus_route r ON r.route_id::text = t.route_id AND r.deleted = false
                   ), '[]'::jsonb) AS trips
            FROM ({DAY_WAYBILLS}) w
            WHERE $3 = '' OR position(lower($3) in lower(concat_ws(' ', w.vehicle_no, w.waybill_no, w.schedule_no, w.driver_token_no, w.conductor_token_no))) > 0
        )"#
    );
    read_page(pool, request, &cte, "waybill_id::bigint").await
}

pub async fn read_routes(pool: &PgPool, request: &DepotDayRequest) -> AppResult<RoutePage> {
    read_page(
        pool,
        request,
        &routes_cte(),
        "route_number NULLS LAST, route_id",
    )
    .await
}

/// Depot roster with that day's waybill assignments.
pub async fn read_crew(pool: &PgPool, request: &DepotDayRequest) -> AppResult<CrewPage> {
    let cte = format!(
        r#"
        WITH day_w AS ({DAY_WAYBILLS}),
        assigned AS (
            -- The source records an absent crew member as '' rather than NULL.
            SELECT driver_token_no AS token, 'driver' AS role FROM day_w WHERE NULLIF(driver_token_no, '') IS NOT NULL
            UNION ALL
            SELECT conductor_token_no, 'conductor' FROM day_w WHERE NULLIF(conductor_token_no, '') IS NOT NULL
        ),
        items AS (
            SELECT e.emp_id::text AS employee_id, e.token_no AS token, dsg.designation_name AS designation,
                   e.status AS master_status, count(a.token) AS assigned_waybills,
                   array_remove(array_agg(DISTINCT a.role), NULL) AS assigned_roles
            FROM employees e
            LEFT JOIN designations dsg ON dsg.designation_id = e.designation_id AND dsg.deleted = false
            LEFT JOIN assigned a ON a.token = e.token_no
            WHERE e.entity_id = $1 AND e.deleted = false
              AND ($3 = '' OR position(lower($3) in lower(concat_ws(' ', e.token_no, dsg.designation_name))) > 0)
            GROUP BY e.emp_id, e.token_no, dsg.designation_name, e.status
        )"#
    );
    read_page(
        pool,
        request,
        &cte,
        "assigned_waybills DESC, token NULLS LAST, employee_id",
    )
    .await
}

/// Counts over the whole depot-day; page and search are ignored.
pub async fn read_overview(pool: &PgPool, request: &DepotDayRequest) -> AppResult<DepotOverview> {
    let counts_sql = format!(
        r#"
        WITH day_w AS ({DAY_WAYBILLS}),
        veh AS (SELECT vehicle_id, fleet_no, vehicle_no FROM vehicles WHERE entity_id = $1 AND deleted = false),
        -- One row per alias (fleet or registration), so waybill matching is a plain equality join Postgres can hash.
        veh_alias AS (SELECT vehicle_id, unnest(ARRAY[fleet_no, vehicle_no]) AS alias FROM veh),
        roster AS (SELECT DISTINCT token_no FROM employees WHERE entity_id = $1 AND deleted = false AND NULLIF(token_no, '') IS NOT NULL),
        feed_alias AS (SELECT unnest(ARRAY[fleet_no, vehicle_no]) AS alias FROM vehicles WHERE entity_id <> $1 AND deleted = false),
        tokens AS (
            SELECT driver_token_no AS token FROM day_w WHERE NULLIF(driver_token_no, '') IS NOT NULL
            UNION SELECT conductor_token_no FROM day_w WHERE NULLIF(conductor_token_no, '') IS NOT NULL
        )
        SELECT
            (SELECT count(*) FROM veh) AS registered_vehicles,
            (SELECT count(*) FROM day_w) AS waybills,
            (SELECT count(DISTINCT vehicle_no) FROM day_w WHERE NULLIF(vehicle_no, '') IS NOT NULL) AS allocated_vehicle_identifiers,
            (SELECT count(DISTINCT va.vehicle_id) FROM veh_alias va JOIN day_w w ON w.vehicle_no = va.alias) AS matched_vehicles,
            (SELECT count(DISTINCT w.vehicle_no) FROM day_w w WHERE NULLIF(w.vehicle_no, '') IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM veh_alias va WHERE va.alias = w.vehicle_no)
                AND EXISTS (SELECT 1 FROM feed_alias f WHERE f.alias = w.vehicle_no)) AS other_depot_vehicles,
            (SELECT count(DISTINCT w.vehicle_no) FROM day_w w WHERE NULLIF(w.vehicle_no, '') IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM veh_alias va WHERE va.alias = w.vehicle_no)
                AND NOT EXISTS (SELECT 1 FROM feed_alias f WHERE f.alias = w.vehicle_no)) AS unmatched_vehicle_identifiers,
            (SELECT count(*) FROM roster) AS rostered_crew,
            (SELECT count(*) FROM tokens) AS assigned_crew_tokens,
            (SELECT count(*) FROM tokens t WHERE NOT EXISTS (SELECT 1 FROM roster r WHERE r.token_no = t.token)) AS unrostered_crew_tokens,
            (SELECT coalesce(jsonb_object_agg(coalesce(status, ''), n), '{{}}'::jsonb)
               FROM (SELECT status, count(*) AS n FROM day_w GROUP BY status) s) AS waybills_by_status
        "#
    );
    let route_sql = format!(
        "{} SELECT count(*) FILTER (WHERE scheduled_schedules > 0) AS scheduled_routes, count(*) FILTER (WHERE allocated_waybills > 0) AS allocated_routes FROM items",
        routes_cte()
    );
    let trips_sql = format!(
        r#"
        WITH day_w AS ({DAY_WAYBILLS}),
        t AS (SELECT t.* FROM day_w w CROSS JOIN LATERAL ({trips}) t),
        ist AS (SELECT now() AT TIME ZONE 'Asia/Kolkata' AS now),
        x AS (
            SELECT t.*, extract(hour FROM start_time::time)::int AS h, start_time::time AS start_clock
            FROM t
        )
        SELECT GROUPING(h) = 1 AS total, h AS hour,
               count(*) AS planned,
               count(*) FILTER (WHERE actual_start IS NOT NULL) AS started,
               count(*) FILTER (WHERE actual_end IS NOT NULL) AS completed,
               count(*) FILTER (WHERE is_active_trip IS TRUE) AS active,
               CASE WHEN $2::date = (SELECT now::date FROM ist) THEN
                   count(*) FILTER (WHERE actual_start IS NULL AND start_clock < (SELECT now::time FROM ist))
               END AS overdue
        FROM x
        GROUP BY GROUPING SETS ((h), ())
        "#,
        trips = waybill_trips(
            &format!(
                "d.start_time, d.is_active_trip, {} AS actual_start, {} AS actual_end",
                recorded_start("d"),
                recorded_end("d")
            ),
            &format!(
                "f.start_time, f.is_active_trip, {}, {}",
                recorded_start("f"),
                recorded_end("f")
            ),
        ),
    );
    let mut tx = read_only_tx(pool).await?;
    let counts = sqlx::query_as::<_, OverviewCounts>(&counts_sql)
        .bind(request.depot_id)
        .bind(&request.service_date)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
    let routes = sqlx::query_as::<_, RouteCounts>(&route_sql)
        .bind(request.depot_id)
        .bind(&request.service_date)
        .bind("")
        .fetch_one(&mut *tx)
        .await
        .map_err(db_error)?;
    let groups = sqlx::query_as::<_, TripGroup>(&trips_sql)
        .bind(request.depot_id)
        .bind(&request.service_date)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_error)?;
    tx.commit().await.map_err(db_error)?;

    let mut trips = TripCounts::default();
    let mut trips_by_hour = Vec::new();
    for g in groups {
        if g.total {
            trips = TripCounts {
                planned_trips: g.planned,
                started_trips: g.started,
                completed_trips: g.completed,
                active_trips: g.active,
                overdue_unstarted_trips: g.overdue,
            };
        } else if let Some(hour @ 0..=23) = g.hour {
            trips_by_hour.push(TripHour {
                hour,
                planned: g.planned,
                started: g.started,
                completed: g.completed,
            });
        }
    }
    trips_by_hour.sort_by_key(|h| h.hour);
    Ok(DepotOverview {
        depot_id: request.depot_id.to_string(),
        service_date: request.service_date.clone(),
        source: SOURCE,
        observed_at: Utc::now(),
        counts,
        routes,
        trips,
        trips_by_hour,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
    }

    #[test]
    fn depot_id_must_be_a_replica_bigint() {
        assert_eq!(
            DepotDayRequest::new("4", day(), None, None, None)
                .unwrap()
                .depot_id,
            4
        );
        for id in ["", "abc", "1 OR 1=1", "9223372036854775808"] {
            assert!(
                DepotDayRequest::new(id, day(), None, None, None).is_err(),
                "{id}"
            );
        }
    }

    #[test]
    fn paging_is_clamped_like_other_gims_lists() {
        let r = DepotDayRequest::new("4", day(), Some(0), Some(500), Some("  J07  ")).unwrap();
        assert_eq!((r.page, r.page_size, r.search.as_str()), (1, 200, "J07"));
        assert_eq!(r.service_date, "2026-10-15");
        let far = DepotDayRequest::new("4", day(), Some(i64::MAX), Some(200), None).unwrap();
        assert_eq!(far.offset(), i64::MAX); // saturates instead of overflowing
    }

    #[test]
    fn search_is_capped_at_100_characters() {
        assert!(DepotDayRequest::new("4", day(), None, None, Some(&"அ".repeat(100))).is_ok());
        assert!(DepotDayRequest::new("4", day(), None, None, Some(&"அ".repeat(101))).is_err());
    }
}
