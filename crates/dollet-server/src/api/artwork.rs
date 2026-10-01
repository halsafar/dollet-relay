//! The artwork proxy and its disk cache.
//!
//! Every output already emits `/api/channels/logos/{id}/cache/` — the M3U's
//! `tvg-logo`, the guide's `<icon>`, Xtream's `stream_icon` — so without this
//! endpoint a client shows every logo broken. It exists for three reasons
//! beyond that: provider CDNs are slow and rate-limit, artwork outlives the
//! provider that served it, and a client fetching a provider URL directly tells
//! that provider who is watching.
//!
//! One implementation for logos and EPG posters both.

use std::path::PathBuf;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use dollet_core::domain::Id;
use dollet_core::{Error, db};
use futures_util::StreamExt;

use super::error::ApiResult;
use crate::AppState;

/// Artwork does not change, so a client that has it never needs to ask again.
const CLIENT_CACHE: &str = "public, max-age=604800";

/// A logo is a few tens of kilobytes. This is loose enough never to bite and
/// tight enough that a hostile URL fills a log line rather than the disk.
const MAX_BYTES: usize = 16 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        // Both spellings: some clients strip the trailing slash from artwork
        // URLs, and a redirect costs a round trip on every image.
        .route("/logos/{id}/cache/", get(logo))
        .route("/logos/{id}/cache", get(logo))
}

async fn logo(State(state): State<AppState>, Path(id): Path<Id>) -> ApiResult<Response> {
    let logo = db::logos::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(serve(&state, &format!("logo-{id}"), &logo.url).await)
}

/// Serve from the cache, fetching once on a miss.
///
/// A fetch failure falls back to redirecting the client at the provider rather
/// than returning an error: a broken image is worse than a slow one, and the
/// provider may well answer a browser when it refused us.
async fn serve(state: &AppState, key: &str, url: &str) -> Response {
    let path = cache_path(state, key);

    if let Ok(bytes) = tokio::fs::read(&path).await {
        return (headers_for(&bytes), bytes).into_response();
    }

    match fetch(url).await {
        Ok(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let temporary = path.with_extension("part");
            if tokio::fs::write(&temporary, &bytes).await.is_ok()
                && let Err(e) = tokio::fs::rename(&temporary, &path).await
            {
                tracing::debug!(error = %e, "artwork not cached");
                let _ = tokio::fs::remove_file(&temporary).await;
            }

            (headers_for(&bytes), bytes).into_response()
        }
        Err(e) => {
            tracing::debug!(url, error = %e, "artwork fetch failed; redirecting");
            axum::response::Redirect::temporary(url).into_response()
        }
    }
}

/// These bytes are provider-controlled and served from this origin, and the SPA
/// keeps its tokens in `localStorage` — so an SVG sniffed out of a logo URL
/// would otherwise be a script running with the session's own storage. The
/// sandbox drops scripts and the origin both, and `nosniff` stops a browser
/// picking a type of its own regardless of what [`sniff`] decided.
fn headers_for(bytes: &[u8]) -> [(header::HeaderName, &'static str); 4] {
    [
        (header::CONTENT_TYPE, sniff(bytes)),
        (header::CACHE_CONTROL, CLIENT_CACHE),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; sandbox",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ]
}

async fn fetch(url: &str) -> Result<Vec<u8>, Error> {
    // `allow_private` is false: an artwork URL comes from a provider feed, so
    // its content is provider-controlled and must not reach the LAN. `http::get`
    // enforces that on both halves — the resolver never runs for a bare address.
    let client = dollet_core::http::client(
        &format!("dollet-relay/{}", env!("CARGO_PKG_VERSION")),
        false,
    )
    .map_err(|e| Error::Other(e.into()))?;

    let response = dollet_core::http::get(&client, url, false)
        .await
        .map_err(|e| Error::upstream(e.to_string()))?;
    if !response.status().is_success() {
        return Err(Error::upstream(format!(
            "{url} returned {}",
            response.status()
        )));
    }

    // Streamed and aborted at the cap rather than buffered and then measured.
    // This endpoint is unauthenticated and the URL is provider-controlled, so
    // `.bytes().await` first would make a 10 GB response 10 GB of this
    // process's memory — and with transparent decompression, a few megabytes on
    // the wire is enough to do it.
    //
    // `Content-Length` is checked first because it is free, but it is a claim,
    // not a guarantee: the running total below is what actually holds.
    if response
        .content_length()
        .is_some_and(|declared| declared > MAX_BYTES as u64)
    {
        return Err(Error::upstream(format!(
            "{url} declares more than {MAX_BYTES} bytes"
        )));
    }

    let mut body = response.bytes_stream();
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::upstream(e.to_string()))?;
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(Error::upstream(format!(
                "{url} is larger than {MAX_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

pub(super) fn cache_path(state: &AppState, key: &str) -> PathBuf {
    // Hashed rather than used as a filename: the key is derived from a row id
    // today, but nothing about the signature stops a caller passing a URL.
    let digest = super::cache_digest(key);
    state.config.cache_dir().join("artwork").join(digest)
}

/// Content type from the magic bytes rather than the URL's extension, which
/// providers routinely get wrong.
fn sniff(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xff, 0xd8, 0xff, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => "image/webp",
        [b'<', b's', b'v', b'g', ..] | [b'<', b'?', b'x', b'm', b'l', ..] => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_bytes_decide_the_content_type() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G', 13, 10]), "image/png");
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]), "image/jpeg");
        assert_eq!(sniff(b"GIF89a"), "image/gif");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff(b"<svg xmlns="), "image/svg+xml");
        assert_eq!(sniff(b"not an image"), "application/octet-stream");
    }

    /// Both response paths build their headers here, and a fetch cannot be
    /// exercised end to end from a test: `allow_private` is off for artwork, so
    /// the guard refuses the loopback server a mock would run on.
    #[test]
    fn every_artwork_response_is_isolated_from_this_origin() {
        for bytes in [b"<svg xmlns=".as_slice(), b"\x89PNG\r\n", b"junk"] {
            let headers = headers_for(bytes);
            assert!(
                headers.contains(&(
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; sandbox"
                )),
                "{bytes:?}"
            );
            assert!(
                headers.contains(&(header::X_CONTENT_TYPE_OPTIONS, "nosniff")),
                "{bytes:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_cache_key_cannot_escape_the_cache_directory() {
        let state = crate::api::tests::bare_state();
        let path = cache_path(&state, "../../etc/passwd");
        assert!(path.starts_with(state.config.cache_dir().join("artwork")));
        assert!(!path.to_string_lossy().contains(".."));
    }
}
