//! The dashboard's static files, served from `gtfs_editor_ui_dir`.
//!
//! Open by design: they hold no data, Pomerium fronts them, and every data call
//! the page makes is authenticated. A path is resolved strictly inside the UI
//! directory - no `..`, no absolute paths, no hidden files.

use super::EditorState;
use actix_web::http::header;
use actix_web::{web, HttpRequest, HttpResponse};
use std::path::{Component, Path, PathBuf};

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>GTFS editor</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>body{font:16px/1.5 system-ui,sans-serif;margin:0;display:grid;place-items:center;min-height:100vh;background:#f6f7f9;color:#1d2433}
main{max-width:34rem;padding:2rem}code{background:#e9ecf2;padding:.1em .3em;border-radius:4px}</style></head>
<body><main><h1>GTFS editor</h1><p>The dashboard files are not installed on this server.
The API is available under <code>/internal/gtfs-editor/</code>.</p></main></body></html>"#;

pub async fn redirect_to_slash() -> HttpResponse {
    // Relative, so it still works behind a proxy that rewrites the prefix.
    HttpResponse::PermanentRedirect()
        .insert_header((header::LOCATION, "ui/"))
        .finish()
}

/// Is this request addressed to the editor's own SSO host? Pomerium sets
/// X-Forwarded-Host; with `preserve_host_header` the Host itself carries it.
pub fn is_editor_host(req: &actix_web::dev::RequestHead, audience: &str) -> bool {
    let host_of = |name: header::HeaderName| {
        req.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|h| h.split(',').next().unwrap_or("").trim())
            .map(|h| {
                h.rsplit_once(':').map_or(h, |(name, port)| {
                    if port.chars().all(|c| c.is_ascii_digit()) {
                        name
                    } else {
                        h
                    }
                })
            })
            .map(str::to_ascii_lowercase)
    };
    let want = audience.trim().to_ascii_lowercase();
    [
        header::HeaderName::from_static("x-forwarded-host"),
        header::HOST,
    ]
    .into_iter()
    .filter_map(host_of)
    .any(|h| h == want)
}

/// `/` on the SSO host opens the dashboard. Registered with a host guard, so
/// `/` on any other host (the cluster service, the public gateway) is untouched.
pub async fn root_redirect() -> HttpResponse {
    HttpResponse::Found()
        .insert_header((header::LOCATION, "/internal/gtfs-editor/ui/"))
        .finish()
}

pub fn resolve(root: &Path, tail: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for comp in Path::new(tail).components() {
        match comp {
            Component::Normal(part) => {
                let s = part.to_str()?;
                if s.starts_with('.') {
                    return None;
                }
                path.push(part);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(path)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

pub async fn serve(req: HttpRequest, state: web::Data<EditorState>) -> HttpResponse {
    let tail = req.match_info().query("tail");
    let Some(mut path) = resolve(&state.ui_dir, tail) else {
        return HttpResponse::NotFound().finish();
    };
    if tail.is_empty() || tail.ends_with('/') || path.is_dir() {
        path.push("index.html");
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let is_index = path.file_name().is_some_and(|n| n == "index.html");
            HttpResponse::Ok()
                .insert_header((header::CONTENT_TYPE, content_type(&path)))
                .insert_header((
                    header::CACHE_CONTROL,
                    if is_index {
                        "no-cache"
                    } else {
                        "public, max-age=300"
                    },
                ))
                .insert_header(("X-Content-Type-Options", "nosniff"))
                // strict-origin-when-cross-origin, not same-origin: the map tiles come
                // from tile.openstreetmap.org, which blocks tile requests that carry no
                // Referer ("403 Access blocked"). This sends only the origin cross-site.
                .insert_header(("Referrer-Policy", "strict-origin-when-cross-origin"))
                .body(bytes)
        }
        Err(_) if path.file_name().is_some_and(|n| n == "index.html") => HttpResponse::Ok()
            .insert_header((header::CONTENT_TYPE, "text/html; charset=utf-8"))
            .insert_header((header::CACHE_CONTROL, "no-cache"))
            .body(PLACEHOLDER),
        Err(_) => HttpResponse::NotFound().finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_editor_host_is_redirected() {
        let head = |pairs: &[(&'static str, &str)]| {
            let mut req = actix_web::test::TestRequest::get();
            for (k, v) in pairs {
                req = req.insert_header((*k, *v));
            }
            req.to_srv_request().head().clone()
        };
        let aud = "gtfs.sso.example";
        assert!(is_editor_host(&head(&[("host", "gtfs.sso.example")]), aud));
        assert!(is_editor_host(
            &head(&[("host", "GTFS.sso.example:443")]),
            aud
        ));
        assert!(is_editor_host(
            &head(&[
                ("host", "gtfs-inmemory-data-server:8000"),
                ("x-forwarded-host", "gtfs.sso.example")
            ]),
            aud
        ));
        assert!(!is_editor_host(
            &head(&[("host", "gtfs-inmemory-data-server:8000")]),
            aud
        ));
        assert!(!is_editor_host(
            &head(&[("host", "api.sandbox.moving.tech")]),
            aud
        ));
    }

    #[test]
    fn paths_stay_inside_the_ui_directory() {
        let root = Path::new("/srv/ui");
        assert_eq!(
            resolve(root, "app.js").unwrap(),
            Path::new("/srv/ui/app.js")
        );
        assert_eq!(
            resolve(root, "vendor/leaflet.css").unwrap(),
            Path::new("/srv/ui/vendor/leaflet.css")
        );
        assert!(resolve(root, "../secret").is_none());
        assert!(resolve(root, "a/../../etc/passwd").is_none());
        assert!(resolve(root, "/etc/passwd").is_none());
        assert!(resolve(root, ".env").is_none());
        assert_eq!(resolve(root, "").unwrap(), Path::new("/srv/ui"));
    }
}
