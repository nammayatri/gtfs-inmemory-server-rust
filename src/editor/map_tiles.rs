//! The dashboard's base map (docs/gtfs-editor.md section 30): OpenStreetMap,
//! which needs nothing, or Google Maps through the Map Tiles API when the
//! deployment gives a key.
//!
//! The Map Tiles API serves a tile only with a session token, made by
//! `POST /v1/createSession` with the key and lasting about two weeks. GIMS makes
//! one and shares it with every signed-in person until a day before it expires,
//! so a page load costs no session call. The browser fetches the tiles itself:
//! the key is in each tile's address, so it must be restricted to the
//! dashboard's host and to the Map Tiles API in the Google Cloud console.
//!
//! Google's copyright for what is on screen must be shown under its tiles, and
//! it changes with the view: `GET /map-tiles/viewport` asks Google for it from
//! here, so the dashboard never depends on Google allowing a browser to ask.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{error, info};

const GOOGLE_BASE: &str = "https://tile.googleapis.com";
/// A session is made again this long before it expires.
const RENEW_BEFORE_SECS: i64 = 24 * 3600;
/// After a failed createSession, the error is answered this long before
/// asking Google again: a page load must not become a call to Google each.
const RETRY_AFTER_SECS: i64 = 60;
const SESSION_TIMEOUT: Duration = Duration::from_secs(8);

/// dhall `gtfs_editor_map_tiles`, every field optional. The key is never
/// serialized and never in a Debug line.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MapTilesConfig {
    /// `"osm"` or `"google"`: the map a person sees until they choose one.
    #[serde(default)]
    pub default: Option<String>,
    /// A Google Maps Platform key with the Map Tiles API enabled. It reaches
    /// the browser in tile addresses: restrict it to the editor's host.
    #[serde(default, skip_serializing)]
    pub google_api_key: Option<String>,
    /// The map's labels: an IETF language tag. Default `en-IN`.
    #[serde(default)]
    pub google_language: Option<String>,
    /// Borders and names as seen from: a CLDR region code. Default `IN`.
    #[serde(default)]
    pub google_region: Option<String>,
    /// Where the Map Tiles API is; only tests change it.
    #[serde(default)]
    pub google_base_url: Option<String>,
}

impl std::fmt::Debug for MapTilesConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapTilesConfig")
            .field("default", &self.default)
            .field(
                "google_api_key",
                &self.google_api_key.as_ref().map(|_| "<set>"),
            )
            .field("google_language", &self.google_language)
            .field("google_region", &self.google_region)
            .field("google_base_url", &self.google_base_url)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseMap {
    Osm,
    Google,
}

impl BaseMap {
    fn name(self) -> &'static str {
        match self {
            BaseMap::Osm => "osm",
            BaseMap::Google => "google",
        }
    }
}

#[derive(Debug, Clone)]
struct Session {
    token: String,
    /// seconds since the epoch
    expiry: i64,
}

enum Cached {
    None,
    Session(Session),
    /// the message, and when it may be asked again
    Failed(String, i64),
}

struct Google {
    key: String,
    language: String,
    region: String,
    base: String,
    cached: Mutex<Cached>,
}

/// What the dashboard may draw under its map.
pub struct MapTiles {
    default: BaseMap,
    google: Option<Google>,
}

impl Default for MapTiles {
    /// OpenStreetMap only: no key, nothing to ask.
    fn default() -> Self {
        Self {
            default: BaseMap::Osm,
            google: None,
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl MapTiles {
    /// From the dhall record. A default of `google` with no key is an error:
    /// the caller logs it and keeps OpenStreetMap.
    pub fn from_config(c: Option<&MapTilesConfig>) -> Result<Self, String> {
        let Some(c) = c else {
            return Ok(Self::default());
        };
        let key = c
            .google_api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty());
        let default = match c.default.as_deref().map(str::trim) {
            None | Some("") | Some("osm") => BaseMap::Osm,
            Some("google") if key.is_some() => BaseMap::Google,
            Some("google") => return Err("default is google but google_api_key is not set".into()),
            Some(other) => return Err(format!("default is osm or google, not {other:?}")),
        };
        let text = |v: &Option<String>, or: &str| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(or)
                .to_string()
        };
        Ok(Self {
            default,
            google: key.map(|k| Google {
                key: k.to_string(),
                language: text(&c.google_language, "en-IN"),
                region: text(&c.google_region, "IN"),
                base: text(&c.google_base_url, GOOGLE_BASE)
                    .trim_end_matches('/')
                    .to_string(),
                cached: Mutex::new(Cached::None),
            }),
        })
    }

    /// `GET /map-tiles`: the default, and Google's tile and viewport addresses
    /// with a live session - or why there are none.
    pub async fn describe(&self, http: &reqwest::Client) -> Value {
        let Some(g) = &self.google else {
            return json!({ "default": BaseMap::Osm.name(), "google": null, "google_error": null });
        };
        match g.session(http).await {
            Ok(s) => {
                let auth = format!("session={}&key={}", enc(&s.token), enc(&g.key));
                json!({
                    "default": self.default.name(),
                    "google": {
                        "url": format!("{}/v1/2dtiles/{{z}}/{{x}}/{{y}}?{auth}", g.base),
                        "max_zoom": 22,
                        "expires_at": s.expiry,
                    },
                    "google_error": null,
                })
            }
            // the dashboard keeps OpenStreetMap and says why
            Err(e) => json!({ "default": BaseMap::Osm.name(), "google": null, "google_error": e }),
        }
    }
}

/// A view of the map: its zoom and edges, for Google's copyright of it.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Viewport {
    pub zoom: u8,
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
}

impl MapTiles {
    /// `GET /map-tiles/viewport`: Google's copyright for the view, to show in
    /// full under its tiles. `Ok(None)` when Google is not configured.
    pub async fn copyright(
        &self,
        http: &reqwest::Client,
        v: Viewport,
    ) -> Result<Option<String>, String> {
        let Some(g) = &self.google else {
            return Ok(None);
        };
        // Google takes north and south strictly inside (-90, 90), east and west inside (-180, 180)
        let lat = |x: f64| x.clamp(-89.999, 89.999);
        let lon = |x: f64| x.clamp(-179.999, 179.999);
        if ![v.north, v.south, v.east, v.west]
            .iter()
            .all(|x| x.is_finite())
            || v.zoom > 22
        {
            return Err("zoom is 0 to 22 and the edges are numbers".into());
        }
        let s = g.session(http).await?;
        let resp = http
            .get(format!("{}/tile/v1/viewport", g.base))
            .query(&[
                ("session", s.token.as_str()),
                ("key", g.key.as_str()),
                ("zoom", &v.zoom.to_string()),
                ("north", &lat(v.north).to_string()),
                ("south", &lat(v.south).to_string()),
                ("east", &lon(v.east).to_string()),
                ("west", &lon(v.west).to_string()),
            ])
            .timeout(SESSION_TIMEOUT)
            .send()
            .await
            .map_err(|_| "Google's copyright for the view could not be read".to_string())?;
        if !resp.status().is_success() {
            return Err(format!(
                "Google refused the copyright for the view (HTTP {})",
                resp.status().as_u16()
            ));
        }
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        Ok(Some(body["copyright"].as_str().unwrap_or("").to_string()))
    }
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

impl Google {
    async fn session(&self, http: &reqwest::Client) -> Result<Session, String> {
        let mut cached = self.cached.lock().await;
        let t = now();
        match &*cached {
            Cached::Session(s) if s.expiry - RENEW_BEFORE_SECS > t => return Ok(s.clone()),
            Cached::Failed(e, until) if *until > t => return Err(e.clone()),
            _ => {}
        }
        match self.create(http).await {
            Ok(s) => {
                info!("Google map tiles: a session until {}", s.expiry);
                *cached = Cached::Session(s.clone());
                Ok(s)
            }
            Err(e) => {
                error!("Google map tiles: {e}");
                // a session not yet expired still serves while a new one fails
                if let Cached::Session(s) = &*cached {
                    if s.expiry > t + 3600 {
                        return Ok(s.clone());
                    }
                }
                *cached = Cached::Failed(e.clone(), t + RETRY_AFTER_SECS);
                Err(e)
            }
        }
    }

    async fn create(&self, http: &reqwest::Client) -> Result<Session, String> {
        let resp = http
            .post(format!("{}/v1/createSession", self.base))
            .query(&[("key", self.key.as_str())])
            .timeout(SESSION_TIMEOUT)
            .json(&json!({
                "mapType": "roadmap",
                "language": self.language,
                "region": self.region,
            }))
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    "Google did not answer the session request in time".to_string()
                } else {
                    "Google's Map Tiles API could not be reached".to_string()
                }
            })?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            // Google's message, never the key: it is not in what Google answers
            let said = body["error"]["message"].as_str().unwrap_or("no message");
            return Err(format!(
                "Google refused the map session (HTTP {}): {said}",
                status.as_u16()
            ));
        }
        let token = body["session"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Google's session answer has no session")?
            .to_string();
        // a string of seconds since the epoch in the documentation
        let expiry = match &body["expiry"] {
            Value::String(s) => s.parse::<i64>().ok(),
            Value::Number(n) => n.as_i64(),
            _ => None,
        }
        .ok_or("Google's session answer has no expiry")?;
        Ok(Session { token, expiry })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer};
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::Arc;

    fn config(default: Option<&str>, key: Option<&str>) -> MapTilesConfig {
        MapTilesConfig {
            default: default.map(str::to_string),
            google_api_key: key.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn what_the_config_allows() {
        let osm = MapTiles::from_config(None).unwrap();
        assert_eq!(osm.default, BaseMap::Osm);
        assert!(osm.google.is_none());
        let g = MapTiles::from_config(Some(&config(Some("google"), Some(" k1 ")))).unwrap();
        assert_eq!(g.default, BaseMap::Google);
        let google = g.google.unwrap();
        assert_eq!(
            (
                google.key.as_str(),
                google.language.as_str(),
                google.region.as_str()
            ),
            ("k1", "en-IN", "IN")
        );
        // a key, OSM first: Google is there to choose
        let both = MapTiles::from_config(Some(&config(None, Some("k1")))).unwrap();
        assert_eq!(both.default, BaseMap::Osm);
        assert!(both.google.is_some());
        assert!(MapTiles::from_config(Some(&config(Some("google"), None))).is_err());
        assert!(MapTiles::from_config(Some(&config(Some("google"), Some("  ")))).is_err());
        assert!(MapTiles::from_config(Some(&config(Some("bing"), Some("k1")))).is_err());
        // the key in neither a log line nor a serialized config
        let c = config(Some("google"), Some("secret-key"));
        assert!(!format!("{c:?}").contains("secret-key"));
        assert!(!serde_json::to_string(&c).unwrap().contains("secret-key"));
    }

    #[test]
    fn the_dhall_record() {
        // only what is set: the rest default
        let c: MapTilesConfig = serde_dhall::from_str(
            r#"{ default = Some "google", google_api_key = Some "k1", google_region = Some "IN" }"#,
        )
        .parse()
        .unwrap();
        assert_eq!(c.default.as_deref(), Some("google"));
        assert_eq!(c.google_api_key.as_deref(), Some("k1"));
        assert_eq!(c.google_language, None);
        assert_eq!(
            MapTiles::from_config(Some(&c)).unwrap().default,
            BaseMap::Google
        );
    }

    struct Fake {
        calls: AtomicUsize,
        /// 0: a session that expires in `ttl`; 1: refuse
        mode: AtomicUsize,
        ttl: AtomicI64,
    }

    async fn fake_google(
        req: HttpRequest,
        body: web::Bytes,
        st: web::Data<Arc<Fake>>,
    ) -> HttpResponse {
        if req.path() == "/tile/v1/viewport" {
            let q = req.query_string().to_string();
            assert!(
                q.contains("key=k%2B1") && q.contains("zoom=12") && q.contains("north=89.999"),
                "{q}"
            );
            return HttpResponse::Ok()
                .json(json!({"copyright": "Map data ©2026 Google", "maxZoomRects": []}));
        }
        st.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(req.path(), "/v1/createSession");
        assert_eq!(req.query_string(), "key=k%2B1");
        let b: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            b,
            json!({"mapType": "roadmap", "language": "en-IN", "region": "IN"})
        );
        if st.mode.load(Ordering::SeqCst) == 1 {
            return HttpResponse::Forbidden().json(json!({"error": {"code": 403, "message": "Map Tiles API has not been used in project 1"}}));
        }
        let n = st.calls.load(Ordering::SeqCst);
        HttpResponse::Ok().json(json!({
            "session": format!("S{n}/x"),
            "expiry": (now() + st.ttl.load(Ordering::SeqCst)).to_string(),
            "tileWidth": 256, "tileHeight": 256, "imageFormat": "png",
        }))
    }

    #[actix_web::test]
    async fn one_session_shared_until_a_day_before_it_expires() {
        let fake = Arc::new(Fake {
            calls: AtomicUsize::new(0),
            mode: AtomicUsize::new(0),
            ttl: AtomicI64::new(14 * 24 * 3600),
        });
        let f = fake.clone();
        let server = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(f.clone()))
                .default_service(web::to(fake_google))
        })
        .workers(1)
        .bind(("127.0.0.1", 0))
        .unwrap();
        let base = format!("http://127.0.0.1:{}", server.addrs()[0].port());
        actix_web::rt::spawn(server.run());

        let tiles = MapTiles::from_config(Some(&MapTilesConfig {
            default: Some("google".into()),
            google_api_key: Some("k+1".into()),
            google_base_url: Some(format!("{base}/")),
            ..Default::default()
        }))
        .unwrap();
        let http = reqwest::Client::new();

        let a = tiles.describe(&http).await;
        assert_eq!(a["default"], "google");
        assert_eq!(
            a["google"]["url"],
            format!("{base}/v1/2dtiles/{{z}}/{{x}}/{{y}}?session=S1%2Fx&key=k%2B1")
        );
        let b = tiles.describe(&http).await;
        assert_eq!(a, b, "the same session for the next person");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        // the copyright of a view, through the same session; a pole clamped
        let view = Viewport {
            zoom: 12,
            north: 90.0,
            south: 12.9,
            east: 80.3,
            west: 80.1,
        };
        assert_eq!(
            tiles.copyright(&http, view).await.unwrap().as_deref(),
            Some("Map data ©2026 Google")
        );
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert!(tiles
            .copyright(&http, Viewport { zoom: 23, ..view })
            .await
            .is_err());
        assert_eq!(MapTiles::default().copyright(&http, view).await, Ok(None));

        // within a day of expiring: a new one
        if let Cached::Session(s) = &mut *tiles.google.as_ref().unwrap().cached.lock().await {
            s.expiry = now() + RENEW_BEFORE_SECS - 10;
        }
        let c = tiles.describe(&http).await;
        assert!(
            c["google"]["url"]
                .as_str()
                .unwrap()
                .contains("session=S2%2Fx"),
            "{c}"
        );

        // Google refuses: a session that still has hours to run keeps serving
        fake.mode.store(1, Ordering::SeqCst);
        if let Cached::Session(s) = &mut *tiles.google.as_ref().unwrap().cached.lock().await {
            s.expiry = now() + 3 * 3600;
        }
        let d = tiles.describe(&http).await;
        assert!(
            d["google"]["url"]
                .as_str()
                .unwrap()
                .contains("session=S2%2Fx"),
            "{d}"
        );
        // ... and once it is about to expire, the reason, OSM, and no second ask within a minute
        if let Cached::Session(s) = &mut *tiles.google.as_ref().unwrap().cached.lock().await {
            s.expiry = now() + 60;
        }
        let calls = fake.calls.load(Ordering::SeqCst);
        let e = tiles.describe(&http).await;
        assert_eq!(e["default"], "osm");
        assert!(e["google"].is_null());
        let why = e["google_error"].as_str().unwrap();
        assert!(
            why.contains("HTTP 403") && why.contains("has not been used"),
            "{why}"
        );
        assert!(
            !why.contains("k+1") && !why.contains("k%2B1"),
            "the key never in a message"
        );
        let _ = tiles.describe(&http).await;
        assert_eq!(fake.calls.load(Ordering::SeqCst), calls + 1);
    }

    #[actix_web::test]
    async fn google_unreachable_is_said() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let tiles = MapTiles::from_config(Some(&MapTilesConfig {
            google_api_key: Some("k".into()),
            google_base_url: Some(format!("http://127.0.0.1:{port}")),
            ..Default::default()
        }))
        .unwrap();
        let e = tiles.describe(&reqwest::Client::new()).await;
        assert_eq!(e["default"], "osm");
        assert_eq!(
            e["google_error"],
            "Google's Map Tiles API could not be reached"
        );
    }
}
