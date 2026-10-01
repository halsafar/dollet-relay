//! Serves the built React app out of the binary, so the container needs no
//! nginx and no static volume.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// `allow_missing` keeps the backend buildable before the SPA has ever been
/// built, so `cargo test` works in a fresh checkout with no node installed.
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

/// Any path that is not a known asset falls back to `index.html`, because
/// client-side routes must survive a hard refresh.
///
/// Except under the API prefixes. A mistyped or renamed API path would
/// otherwise answer `200` with HTML, and the client — seeing `response.ok` —
/// returns that HTML as data rather than raising. So an API rename fails
/// silently and at the wrong layer. The route manifest catches renames it knows
/// about, but it is explicitly a lower bound, and this is the half that has to
/// hold for everything it does not cover.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    if is_api(path) {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"detail":"Not found."}"#,
        )
            .into_response();
    }

    let name = resolve(path);
    match Assets::get(name) {
        Some(asset) => ([(header::CONTENT_TYPE, content_type(name))], asset.data).into_response(),
        None => (StatusCode::NOT_FOUND, "frontend not built").into_response(),
    }
}

/// The embedded file a path is answered with: the asset itself when one
/// exists, `index.html` otherwise.
fn resolve(path: &str) -> &str {
    if !path.is_empty() && Assets::get(path).is_some() {
        path
    } else {
        "index.html"
    }
}

/// Guessed from the file actually being served, not from the path requested.
/// Guessing from the request would turn a hard refresh on `/settings` into a
/// download: `settings` has no extension, so the fallback page would go out
/// as `application/octet-stream` and the browser would save it to disk.
fn content_type(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .to_string()
}

/// Prefixes whose 404 must look like an API 404 rather than a web page.
///
/// `hdhr` and `output` are here for the same reason: Plex reads them, and a
/// client handed `index.html` where it expected JSON reports something far less
/// useful than a 404.
fn is_api(path: &str) -> bool {
    ["api/", "proxy/", "hdhr/", "output/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_prefixes_do_not_fall_through_to_the_page() {
        for path in [
            "api/channels/chanels/",
            "api/",
            "proxy/ts/stream/nope",
            "hdhr/lineup.jsonn",
            "output/m3uu",
        ] {
            assert!(is_api(path), "{path} would have returned index.html");
        }
    }

    #[test]
    fn the_fallback_page_is_served_as_html_whatever_the_route_was_called() {
        // The type follows the file that is served. `settings` has no
        // extension, and guessing from it would make a refresh a download.
        assert_eq!(content_type("index.html"), "text/html");
        assert_eq!(content_type("assets/index-abc.js"), "text/javascript");
        assert_eq!(content_type("assets/index-abc.css"), "text/css");
        for route in ["settings", "channels", "guide", "stats"] {
            assert_eq!(resolve(route), "index.html", "{route}");
        }
        assert_eq!(resolve(""), "index.html");
    }

    #[test]
    fn client_routes_still_reach_the_page() {
        for path in ["", "channels", "guide", "settings", "assets/index-abc.js"] {
            assert!(!is_api(path), "{path} was treated as an API path");
        }
    }
}
