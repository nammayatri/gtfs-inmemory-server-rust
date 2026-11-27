//! The dashboard's base map (docs/gtfs-editor.md section 30): OpenStreetMap,
//! which needs nothing, or Google Maps through the Maps JavaScript API when the
//! deployment gives a key.
//!
//! GIMS makes no call to Google. `GET /map-tiles` tells the signed-in dashboard
//! which map a person sees first and, when there is a key, the key and where
//! Google's script is; the browser loads the Maps JavaScript API itself and
//! draws Google's map under the Leaflet one. Google bills a map load - once per
//! page, whatever the person then pans and zooms - and draws its own logo and
//! copyright. The key reaches the browser, as every Maps JavaScript key does:
//! restrict it to the editor's host (HTTP referrer) and to the Maps JavaScript
//! API in the Google Cloud console.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The Maps JavaScript API's loader. The dashboard adds the key, the version,
/// the language and region, and its callback.
pub const GOOGLE_MAPS_JS: &str = "https://maps.googleapis.com/maps/api/js";

/// dhall `gtfs_editor_map_tiles`, every field optional. The key is never
/// serialized and never in a Debug line.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct MapTilesConfig {
    /// `"osm"` or `"google"`: the map a person sees until they choose one.
    #[serde(default)]
    pub default: Option<String>,
    /// A Google Maps Platform key with the Maps JavaScript API enabled. It
    /// reaches the browser: restrict it to the editor's host.
    #[serde(default, skip_serializing)]
    pub google_api_key: Option<String>,
    /// The map's labels: a language code Google supports. Default: the
    /// browser's language.
    #[serde(default)]
    pub google_language: Option<String>,
    /// Borders and names as seen from: a region code. Default `IN`.
    #[serde(default)]
    pub google_region: Option<String>,
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

struct Google {
    key: String,
    language: Option<String>,
    region: String,
}

/// What the dashboard may draw under its map.
pub struct MapTiles {
    default: BaseMap,
    google: Option<Google>,
}

impl Default for MapTiles {
    /// OpenStreetMap only: no key.
    fn default() -> Self {
        Self {
            default: BaseMap::Osm,
            google: None,
        }
    }
}

impl MapTiles {
    /// From the dhall record. A default of `google` with no key is an error:
    /// the caller logs it and keeps OpenStreetMap.
    pub fn from_config(c: Option<&MapTilesConfig>) -> Result<Self, String> {
        let Some(c) = c else {
            return Ok(Self::default());
        };
        let trimmed = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let key = trimmed(&c.google_api_key);
        let default = match c.default.as_deref().map(str::trim) {
            None | Some("") | Some("osm") => BaseMap::Osm,
            Some("google") if key.is_some() => BaseMap::Google,
            Some("google") => return Err("default is google but google_api_key is not set".into()),
            Some(other) => return Err(format!("default is osm or google, not {other:?}")),
        };
        Ok(Self {
            default,
            google: key.map(|key| Google {
                key,
                language: trimmed(&c.google_language),
                region: trimmed(&c.google_region).unwrap_or_else(|| "IN".to_string()),
            }),
        })
    }

    /// `GET /map-tiles`: which map a person sees first, and what the browser
    /// needs to load Google's.
    pub fn describe(&self) -> Value {
        json!({
            "default": self.default.name(),
            "google": self.google.as_ref().map(|g| json!({
                "key": g.key,
                "script_url": GOOGLE_MAPS_JS,
                "language": g.language,
                "region": g.region,
            })),
            "google_error": null,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            osm.describe(),
            json!({"default": "osm", "google": null, "google_error": null})
        );
        let g = MapTiles::from_config(Some(&config(Some("google"), Some(" k1 ")))).unwrap();
        assert_eq!(
            g.describe(),
            json!({
                "default": "google",
                "google": {"key": "k1", "script_url": GOOGLE_MAPS_JS, "language": null, "region": "IN"},
                "google_error": null,
            })
        );
        // a key, OSM first: Google is there to choose
        let both = MapTiles::from_config(Some(&MapTilesConfig {
            google_language: Some("ta".into()),
            google_region: Some(" ".into()),
            ..config(None, Some("k1"))
        }))
        .unwrap();
        let d = both.describe();
        assert_eq!(d["default"], "osm");
        assert_eq!(
            (
                d["google"]["language"].as_str(),
                d["google"]["region"].as_str()
            ),
            (Some("ta"), Some("IN"))
        );
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
        // only what is set: the rest default; a record from the Map Tiles
        // days (google_base_url) still reads
        let c: MapTilesConfig = serde_dhall::from_str(
            r#"{ default = Some "google", google_api_key = Some "k1", google_region = Some "IN", google_base_url = None Text }"#,
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
}
