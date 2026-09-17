//! GTFS metadata editor: see docs/gtfs-editor.md.
//!
//! Everything lives under `/internal/gtfs-editor`. The dashboard's static files
//! (`/ui/`) are open - Pomerium sits in front of them - and every API call goes
//! through [`auth::guard`], which verifies Pomerium's signed JWT, then each
//! handler checks the session and role it needs. Nothing else in GIMS changes:
//! the editor has its own small, lazily-connected pool on the internal DB, so a
//! database outage cannot stop GIMS from booting.

pub mod auth;
pub mod bulk;
pub mod crypto;
pub mod draft;
pub mod error;
pub mod handlers;
pub mod jwt;
pub mod position_reviews;
pub mod proposals;
pub mod service;
pub mod static_ui;
pub mod validation;

use crate::environment::AppConfig;
use actix_web::{middleware::from_fn, web};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

pub struct EditorState {
    pub pool: PgPool,
    pub jwks: jwt::JwksCache,
    pub audience: String,
    pub bootstrap_admins: HashSet<String>,
    pub secrets: crypto::SecretBox,
    pub session_hours: i64,
    pub ui_dir: PathBuf,
    pub osrm_url: Option<String>,
}

pub struct EditorSettings {
    pub jwks_url: String,
    pub audience: String,
    pub bootstrap_admins: Vec<String>,
    pub totp_key_b64: String,
    pub session_hours: i64,
    pub ui_dir: PathBuf,
    pub osrm_url: Option<String>,
}

impl EditorState {
    pub fn build(pool: PgPool, s: EditorSettings) -> Result<Self, String> {
        if s.audience.trim().is_empty() {
            return Err("gtfs_editor_audience is empty".into());
        }
        if !(s.jwks_url.starts_with("https://")
            || s.jwks_url.starts_with("http://")
            || s.jwks_url.starts_with("file://"))
        {
            return Err("gtfs_editor_pomerium_jwks_url must be http(s):// or file://".into());
        }
        Ok(Self {
            pool,
            jwks: jwt::JwksCache::new(s.jwks_url),
            audience: s.audience.trim().to_string(),
            bootstrap_admins: s
                .bootstrap_admins
                .iter()
                .map(|e| e.trim().to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            secrets: crypto::SecretBox::from_base64(&s.totp_key_b64)?,
            session_hours: s.session_hours.clamp(1, 24 * 7),
            ui_dir: s.ui_dir,
            osrm_url: s.osrm_url,
        })
    }

    /// Build from the GIMS config. `None` when the editor is disabled or its
    /// config is incomplete - logged, never fatal, so GIMS still serves.
    pub async fn init(config: &AppConfig) -> Option<Arc<Self>> {
        if !config.gtfs_editor_enabled {
            info!("GTFS editor disabled");
            return None;
        }
        fn missing(what: &str) -> Option<Arc<EditorState>> {
            error!("GTFS editor enabled but {what} is not set; editor disabled");
            None
        }
        let Some(db_url) = config.internal_database_url.clone() else {
            return missing("internal_database_url");
        };
        let Some(jwks_url) = config.gtfs_editor_pomerium_jwks_url.clone() else {
            return missing("gtfs_editor_pomerium_jwks_url");
        };
        let Some(audience) = config.gtfs_editor_audience.clone() else {
            return missing("gtfs_editor_audience");
        };
        let Some(totp_key_b64) = config.gtfs_editor_totp_key.clone() else {
            return missing("gtfs_editor_totp_key");
        };
        let pool = match PgPoolOptions::new()
            .max_connections(5)
            .min_connections(0)
            .acquire_timeout(Duration::from_secs(10))
            .idle_timeout(Duration::from_secs(600))
            .connect_lazy(&db_url)
        {
            Ok(p) => p,
            Err(_) => {
                error!("GTFS editor database url is invalid; editor disabled");
                return None;
            }
        };
        let settings = EditorSettings {
            jwks_url,
            audience,
            bootstrap_admins: config.gtfs_editor_bootstrap_admins.clone(),
            totp_key_b64,
            session_hours: config.gtfs_editor_session_hours.unwrap_or(12) as i64,
            ui_dir: PathBuf::from(
                config
                    .gtfs_editor_ui_dir
                    .clone()
                    .unwrap_or_else(|| "./editor-ui".to_string()),
            ),
            osrm_url: config.osrm_url.clone(),
        };
        match Self::build(pool, settings) {
            Ok(state) => {
                info!("GTFS editor enabled at /internal/gtfs-editor");
                Some(Arc::new(state))
            }
            Err(e) => {
                error!("GTFS editor config is invalid: {e}; editor disabled");
                None
            }
        }
    }
}

/// Register the editor's routes. A disabled editor registers nothing.
pub fn configure(cfg: &mut web::ServiceConfig, state: Option<Arc<EditorState>>) {
    let Some(state) = state else {
        return;
    };
    let audience = state.audience.clone();
    let data = web::Data::from(state);

    // `/` on the SSO host only; every other host keeps GIMS's own `/` handling.
    cfg.service(
        web::resource("/")
            .guard(actix_web::guard::fn_guard(move |ctx| {
                static_ui::is_editor_host(ctx.head(), &audience)
            }))
            .route(web::get().to(static_ui::root_redirect)),
    );

    // The UI scope must be registered first: the API scope's prefix also
    // matches /internal/gtfs-editor/ui.
    cfg.service(
        web::scope("/internal/gtfs-editor/ui")
            .app_data(data.clone())
            .route("", web::get().to(static_ui::redirect_to_slash))
            .route("/{tail:.*}", web::get().to(static_ui::serve)),
    );

    use handlers as h;
    cfg.service(
        web::scope("/internal/gtfs-editor")
            .app_data(data)
            .app_data(
                web::JsonConfig::default()
                    .limit(8 * 1024 * 1024)
                    .error_handler(h::json_error),
            )
            .app_data(web::QueryConfig::default().error_handler(h::query_error))
            .app_data(web::PathConfig::default().error_handler(h::path_error))
            .wrap(from_fn(auth::guard))
            // auth
            .route("/auth/me", web::get().to(h::me))
            .route("/auth/totp/enroll", web::post().to(h::totp_enroll))
            .route("/auth/totp/confirm", web::post().to(h::totp_confirm))
            .route("/auth/session", web::post().to(h::session_create))
            .route("/auth/session", web::delete().to(h::session_delete))
            // live data
            .route("/feeds", web::get().to(h::feeds))
            .route("/feeds/{gtfs_id}/config", web::get().to(h::feed_config))
            .route(
                "/feeds/{gtfs_id}/config",
                web::post().to(h::feed_config_update),
            )
            .route("/feeds/{gtfs_id}/stops", web::get().to(h::stops))
            .route("/feeds/{gtfs_id}/stops/{stop_id}", web::get().to(h::stop))
            .route("/feeds/{gtfs_id}/routes", web::get().to(h::routes))
            .route(
                "/feeds/{gtfs_id}/routes/{route_id}",
                web::get().to(h::route),
            )
            .route(
                "/feeds/{gtfs_id}/routes/{route_id}/polyline:osrm",
                web::post().to(h::polyline_osrm),
            )
            .route("/feeds/{gtfs_id}/audit", web::get().to(h::audit))
            // drafts
            .route(
                "/feeds/{gtfs_id}/change-sets",
                web::get().to(h::change_sets),
            )
            .route(
                "/feeds/{gtfs_id}/change-sets",
                web::post().to(h::change_set_create),
            )
            .route("/change-sets/{id}", web::get().to(h::change_set))
            .route("/change-sets/{id}/changes", web::post().to(h::change_add))
            .route(
                "/change-sets/{id}/changes/{change_id}",
                web::put().to(h::change_update),
            )
            .route(
                "/change-sets/{id}/changes/{change_id}",
                web::delete().to(h::change_delete),
            )
            .route(
                "/change-sets/{id}/preview/routes/{route_id}",
                web::get().to(h::change_set_preview_route),
            )
            .route("/change-sets/{id}/submit", web::post().to(h::submit))
            .route("/change-sets/{id}/reopen", web::post().to(h::reopen))
            .route("/change-sets/{id}/approve", web::post().to(h::approve))
            .route("/change-sets/{id}/reject", web::post().to(h::reject))
            .route("/change-sets/{id}/commit", web::post().to(h::commit))
            .route("/change-sets/{id}/discard", web::post().to(h::discard))
            .route("/change-sets/{id}/bulk", web::post().to(h::bulk_import))
            // station proposals
            .route(
                "/feeds/{gtfs_id}/station-proposals",
                web::get().to(h::station_proposals),
            )
            .route(
                "/feeds/{gtfs_id}/station-proposals/summary",
                web::get().to(h::station_proposal_summary),
            )
            .route(
                "/feeds/{gtfs_id}/station-proposals/approve",
                web::post().to(h::station_proposals_approve),
            )
            .route(
                "/station-proposals/{id}",
                web::get().to(h::station_proposal),
            )
            .route(
                "/station-proposals/{id}/approve",
                web::post().to(h::station_proposal_approve),
            )
            .route(
                "/station-proposals/{id}/reject",
                web::post().to(h::station_proposal_reject),
            )
            .route(
                "/station-proposals/{id}/reopen",
                web::post().to(h::station_proposal_reopen),
            )
            // coordinate reviews
            .route(
                "/feeds/{gtfs_id}/position-reviews",
                web::get().to(h::position_reviews),
            )
            .route(
                "/feeds/{gtfs_id}/position-reviews/summary",
                web::get().to(h::position_review_summary),
            )
            .route("/position-reviews/{id}", web::get().to(h::position_review))
            .route(
                "/position-reviews/{id}/move",
                web::post().to(h::position_review_move),
            )
            .route(
                "/position-reviews/{id}/split",
                web::post().to(h::position_review_split),
            )
            .route(
                "/position-reviews/{id}/confirm",
                web::post().to(h::position_review_confirm),
            )
            .route(
                "/position-reviews/{id}/reopen",
                web::post().to(h::position_review_reopen),
            )
            // admin
            .route("/users", web::get().to(h::users))
            .route("/users", web::post().to(h::user_create))
            .route("/users/{user_id}", web::patch().to(h::user_update))
            .route(
                "/users/{user_id}/reset-totp",
                web::post().to(h::user_reset_totp),
            )
            .default_service(web::to(h::not_found)),
    );
}

#[cfg(test)]
mod tests {
    use crate::environment::read_dhall_config;

    #[test]
    fn existing_configs_parse_with_the_editor_off() {
        let cfg =
            read_dhall_config("./dhall-configs/dev/gtfs_in_memory_server_rust.dhall").unwrap();
        assert!(!cfg.gtfs_editor_enabled);
        assert!(cfg.gtfs_editor_bootstrap_admins.is_empty());
        assert!(cfg.gtfs_editor_totp_key.is_none());
    }

    #[test]
    fn editor_fields_parse_when_set() {
        let dev =
            std::fs::canonicalize("./dhall-configs/dev/gtfs_in_memory_server_rust.dhall").unwrap();
        let dir =
            std::env::temp_dir().join(format!("editor-dhall-{}", super::crypto::random_token()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("editor.dhall");
        std::fs::write(
            &path,
            format!(
                "{} // {{ gtfs_editor_enabled = True, \
                   gtfs_editor_pomerium_jwks_url = Some \"https://authenticate.example/.well-known/pomerium/jwks.json\", \
                   gtfs_editor_audience = Some \"gtfs.sso.example\", \
                   gtfs_editor_bootstrap_admins = [\"ops@example.com\"], \
                   gtfs_editor_totp_key = Some \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\", \
                   gtfs_editor_session_hours = Some 8, \
                   gtfs_editor_ui_dir = Some \"./editor-ui\" }}",
                dev.display()
            ),
        )
        .unwrap();
        let cfg = read_dhall_config(path.to_str().unwrap()).unwrap();
        assert!(cfg.gtfs_editor_enabled);
        assert_eq!(cfg.gtfs_editor_bootstrap_admins, vec!["ops@example.com"]);
        assert_eq!(cfg.gtfs_editor_session_hours, Some(8));
        assert_eq!(
            cfg.gtfs_editor_audience.as_deref(),
            Some("gtfs.sso.example")
        );
        std::fs::remove_dir_all(dir).ok();
    }
}
