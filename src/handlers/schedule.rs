use actix_web::{
    web::{Data, Json, Path, Query},
    HttpResponse,
};
use tracing::info;

use crate::environment::AppState;
use crate::models::{ArrivalQuery, DepartureQuery, NextServicesRequest, TripsBetweenRequest};
use crate::services::schedule::ScheduleService;
use crate::tools::error::AppResult;

pub async fn get_departures_at_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<DepartureQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let q = query.into_inner();
    let window = q.window_minutes.unwrap_or(60);
    let limit = q.limit.unwrap_or(10);

    info!(
        "GET /schedule/departures/{}/{} time={:?} window={} limit={}",
        gtfs_id, stop_code, q.time, window, limit
    );

    let schedule_service = ScheduleService::new(app_state.gtfs_service.clone());
    let departures = schedule_service
        .get_departures_at_stop(
            &gtfs_id,
            &stop_code,
            q.time.as_deref(),
            q.date.as_deref(),
            window,
            limit,
        )
        .await?;

    Ok(HttpResponse::Ok().json(departures))
}

pub async fn get_arrivals_at_stop(
    app_state: Data<AppState>,
    path: Path<(String, String)>,
    query: Query<ArrivalQuery>,
) -> AppResult<HttpResponse> {
    let (gtfs_id, stop_code) = path.into_inner();
    let q = query.into_inner();
    let window = q.window_minutes.unwrap_or(60);
    let limit = q.limit.unwrap_or(10);

    info!(
        "GET /schedule/arrivals/{}/{} time={:?} window={} limit={}",
        gtfs_id, stop_code, q.time, window, limit
    );

    let schedule_service = ScheduleService::new(app_state.gtfs_service.clone());
    let arrivals = schedule_service
        .get_arrivals_at_stop(
            &gtfs_id,
            &stop_code,
            q.time.as_deref(),
            q.date.as_deref(),
            window,
            limit,
        )
        .await?;

    Ok(HttpResponse::Ok().json(arrivals))
}

pub async fn get_trips_between_stops(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<TripsBetweenRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    info!(
        "POST /schedule/trips-between/{} origin={} dest={}",
        gtfs_id, req.origin_stop_code, req.destination_stop_code
    );

    let window = req.window_minutes.unwrap_or(120);
    let schedule_service = ScheduleService::new(app_state.gtfs_service.clone());
    let trips = schedule_service
        .get_trips_between_stops(
            &gtfs_id,
            &req.origin_stop_code,
            &req.destination_stop_code,
            req.date.as_deref(),
            req.depart_after.as_deref(),
            req.arrive_before.as_deref(),
            window,
        )
        .await?;

    Ok(HttpResponse::Ok().json(trips))
}

pub async fn get_next_services(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<NextServicesRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();
    let limit = req.limit.unwrap_or(10);

    info!(
        "POST /schedule/next-services/{} stop={} time={:?} limit={}",
        gtfs_id, req.stop_code, req.time, limit
    );

    let schedule_service = ScheduleService::new(app_state.gtfs_service.clone());
    let modes_ref = req.modes.as_deref();
    let services = schedule_service
        .get_next_services(
            &gtfs_id,
            &req.stop_code,
            req.date.as_deref(),
            req.time.as_deref(),
            modes_ref,
            limit,
        )
        .await?;

    Ok(HttpResponse::Ok().json(services))
}

pub async fn get_quality_report(
    app_state: Data<AppState>,
    path: Path<String>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();

    info!("GET /internal/gtfs/{}/quality-report", gtfs_id);

    let schedule_service = ScheduleService::new(app_state.gtfs_service.clone());
    let report = schedule_service.get_quality_report(&gtfs_id).await?;

    Ok(HttpResponse::Ok().json(report))
}
