use actix_web::web::{Data, Json, Path};
use actix_web::HttpResponse;
use serde::Deserialize;

use crate::environment::AppState;
use crate::services::fleet_operator::{TripAction, WaybillAnchor};
use crate::tools::error::{AppError, AppResult};

// ─── Request bodies ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AnchorRequest {
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub vehicle_number: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TripActionRequest {
    pub action: String,
    pub trip_number: i32,
    pub timestamp: Option<i64>,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub vehicle_number: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CurrentTripDetailsRequest {
    pub previous_trip_number: i32,
    pub conductor_token: Option<String>,
    pub driver_token: Option<String>,
    pub vehicle_number: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    pub driver_token: Option<String>,
    pub conductor_token: Option<String>,
    pub obu_serial_no: Option<String>,
    pub etm_serial_no: Option<String>,
}

// ─── Anchor parsing ────────────────────────────────────────────────────────────

fn parse_anchor(
    conductor_token: Option<String>,
    driver_token: Option<String>,
    vehicle_number: Option<String>,
) -> AppResult<WaybillAnchor> {
    let anchors_provided = conductor_token.is_some() as u8
        + driver_token.is_some() as u8
        + vehicle_number.is_some() as u8;

    if anchors_provided != 1 {
        return Err(AppError::BadRequest(
            "Exactly one of conductor_token, driver_token, or vehicle_number must be provided."
                .to_string(),
        ));
    }

    if let Some(token) = conductor_token {
        return Ok(WaybillAnchor::ConductorToken(token));
    }
    if let Some(token) = driver_token {
        return Ok(WaybillAnchor::DriverToken(token));
    }
    Ok(WaybillAnchor::VehicleNumber(vehicle_number.unwrap()))
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

pub async fn current_operation(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<AnchorRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();
    let anchor = parse_anchor(req.conductor_token, req.driver_token, req.vehicle_number)?;
    let response = app_state
        .fleet_operator_service
        .current_operation(&gtfs_id, anchor)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

pub async fn trip_action(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<TripActionRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    let action = match req.action.as_str() {
        "start" => TripAction::Start,
        "end" => TripAction::End,
        other => {
            return Err(AppError::BadRequest(format!(
                "Invalid action '{}'. Must be 'start' or 'end'.",
                other
            )))
        }
    };

    let anchor = parse_anchor(req.conductor_token, req.driver_token, req.vehicle_number)?;
    let response = app_state
        .fleet_operator_service
        .trip_action(&gtfs_id, anchor, action, req.trip_number, req.timestamp)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

pub async fn current_trip_details(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<CurrentTripDetailsRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();
    let anchor = parse_anchor(req.conductor_token, req.driver_token, req.vehicle_number)?;
    let response = app_state
        .fleet_operator_service
        .current_trip_details(&gtfs_id, anchor, req.previous_trip_number)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}

pub async fn verify(
    app_state: Data<AppState>,
    path: Path<String>,
    body: Json<VerifyRequest>,
) -> AppResult<HttpResponse> {
    let gtfs_id = path.into_inner();
    let req = body.into_inner();

    // Exactly one of driver_token / conductor_token
    let token_count = req.driver_token.is_some() as u8 + req.conductor_token.is_some() as u8;
    if token_count != 1 {
        return Err(AppError::BadRequest(
            "Exactly one of driver_token or conductor_token must be provided.".to_string(),
        ));
    }

    // Exactly one of obu_serial_no / etm_serial_no
    let device_count = req.obu_serial_no.is_some() as u8 + req.etm_serial_no.is_some() as u8;
    if device_count != 1 {
        return Err(AppError::BadRequest(
            "Exactly one of obu_serial_no or etm_serial_no must be provided.".to_string(),
        ));
    }

    let token = req.driver_token.or(req.conductor_token).unwrap();
    let (device_serial_no, is_obu) = if let Some(obu) = req.obu_serial_no {
        (obu, true)
    } else {
        (req.etm_serial_no.unwrap(), false)
    };

    let response = app_state
        .fleet_operator_service
        .verify(&gtfs_id, &token, &device_serial_no, is_obu)
        .await?;
    Ok(HttpResponse::Ok().json(response))
}
