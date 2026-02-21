use actix_web::{web, HttpRequest, HttpResponse};
use tracing::{error, info, warn};

use crate::environment::{reload_conductor_data, AppState};

pub async fn conductor_reload_webhook(
    app_state: web::Data<AppState>,
    req: HttpRequest,
) -> HttpResponse {
    // 1. Verify the shared secret
    let expected_secret = match &app_state.config.webhook_secret {
        Some(secret) => secret.as_str(),
        None => {
            warn!("Webhook called but no webhook_secret configured — rejecting");
            return HttpResponse::Unauthorized().body("Webhook secret not configured");
        }
    };

    let provided_secret = req
        .headers()
        .get("X-Webhook-Secret")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if provided_secret != expected_secret {
        warn!("Webhook called with invalid secret");
        return HttpResponse::Unauthorized().body("Invalid webhook secret");
    }

    // 2. Determine the Sheets CSV export URL
    let sheet_url = match &app_state.config.conductor_sheet_url {
        Some(url) => url.clone(),
        None => {
            error!("Webhook called but no conductor_sheet_url configured");
            return HttpResponse::InternalServerError().body("conductor_sheet_url not configured");
        }
    };

    // 3. Spawn reload in background — return 200 OK immediately
    let conductor_map = app_state.conductor_details.clone();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .unwrap_or_default();

    tokio::spawn(async move {
        match reload_conductor_data(&conductor_map, &client, &sheet_url).await {
            Ok(_) => info!("Conductor reload via webhook successful"),
            Err(e) => error!("Conductor reload via webhook failed: {}", e),
        }
    });

    HttpResponse::Ok().body("Reload triggered")
}
