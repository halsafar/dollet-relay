//! Refreshing provider data: the thing that lets the old instance be switched
//! off rather than merely stopped being used for playback.
//!
//! The decisions all live in `dollet_core::sync`, which is pure and separately
//! covered. What is here is the I/O around them: fetch, stream to disk, walk
//! the parser, apply the plan in batches, report progress.

pub mod epg;
pub mod m3u;
pub mod xtream;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use dollet_core::Error;
use futures_util::StreamExt;

use super::jobs::{Handler, handler};
use crate::AppState;

/// Ceiling on a downloaded feed. A full XMLTV feed is tens of megabytes; this
/// is loose enough never to bite legitimately and tight enough that a provider
/// serving something pathological fills a log line rather than the disk.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// What a handler returns when its job was cancelled under it. `jobs` tells
/// cancellation from failure by the token, not by this value.
fn cancelled() -> Error {
    Error::invalid("cancelled")
}

pub fn handlers() -> HashMap<&'static str, Handler> {
    HashMap::from([
        (epg::KIND, handler(epg::run)),
        (m3u::KIND, handler(m3u::run)),
    ])
}

/// Download a provider feed to disk.
///
/// To a file rather than memory because a full XMLTV feed is hundreds of
/// megabytes decompressed and the parser reads it streaming — holding it as a
/// `Vec` would undo the memory target for the duration of every refresh.
///
/// `allow_private` is **false** for everything here. Unlike the stream path,
/// where the operator's own LAN tuner is a legitimate target, these URLs are
/// configuration whose *content* the provider controls. That policy is the
/// call site's; enforcing it is `dollet_core::http`'s.
async fn download(url: &str, user_agent: &str, destination: &Path) -> Result<u64, Error> {
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // `allow_private` is **false** for everything here, and `http::get` is what
    // applies it: the guarded resolver alone cannot see a URL written as a bare
    // address, because reqwest dials those without resolving them.
    let client =
        dollet_core::http::client(user_agent, false).map_err(|e| Error::Other(e.into()))?;
    // Every message below names the redacted URL, never `url` itself — and
    // goes through `scrub`, because the transport error carries its own copy.
    let safe = redact(url);
    let response = dollet_core::http::get(&client, url, false)
        .await
        .map_err(|e| Error::upstream(scrub(&format!("fetching {safe}: {e}"))))?;

    if !response.status().is_success() {
        return Err(Error::upstream(format!(
            "{safe} returned {}",
            response.status()
        )));
    }

    // Written under a temporary name and renamed, so a failed download never
    // replaces the last good copy with a truncated one.
    let temporary = destination.with_extension("part");
    let mut file = tokio::fs::File::create(&temporary).await?;
    let mut body = response.bytes_stream();
    let mut written = 0u64;

    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| Error::upstream(scrub(&format!("reading {safe}: {e}"))))?;
        written += chunk.len() as u64;
        if written > MAX_DOWNLOAD_BYTES {
            drop(file);
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(Error::upstream(format!(
                "{safe} exceeded {MAX_DOWNLOAD_BYTES} bytes"
            )));
        }
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
    }

    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);
    tokio::fs::rename(&temporary, destination).await?;

    Ok(written)
}

/// Where a downloaded feed lives between refreshes.
fn feed_path(state: &AppState, kind: &str, id: i64) -> PathBuf {
    state.config.cache_dir().join(format!("{kind}-{id}.feed"))
}

/// Query parameters that carry a secret, lowercased.
///
/// Xtream puts the account password in `password`; the rest are the names other
/// providers use for the same thing.
const SECRET_PARAMS: &[&str] = &["password", "pass", "pwd", "token", "auth", "key", "api_key"];

/// A URL safe to put in an error, a log line or a database column.
///
/// An Xtream playlist URL carries the provider password in its query string.
/// Errors from this module land in `job.last_error`, which is served by
/// `GET /api/m3u/accounts/` as `last_message`, pushed to every `/ws` subscriber
/// and printed to container stdout — four places the serializer two files away
/// deliberately withholds that same password from. Redact before it can reach
/// any of them.
///
/// Unparseable input is reported by shape rather than passed through: a string
/// this cannot read is a string whose secrets it cannot find either.
pub fn redact(url: &str) -> String {
    let Ok(mut parsed) = url::Url::parse(url) else {
        return "<unparseable url>".to_owned();
    };

    if !parsed.username().is_empty() {
        let _ = parsed.set_username("");
    }
    if parsed.password().is_some() {
        let _ = parsed.set_password(None);
    }

    let redacted: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(key, value)| {
            let secret = SECRET_PARAMS.contains(&key.to_lowercase().as_str());
            (
                key.into_owned(),
                if secret {
                    "REDACTED".to_owned()
                } else {
                    value.into_owned()
                },
            )
        })
        .collect();

    if redacted.is_empty() {
        parsed.set_query(None);
    } else {
        parsed
            .query_pairs_mut()
            .clear()
            .extend_pairs(redacted)
            .finish();
    }

    redact_credential_path(&mut parsed);
    parsed.into()
}

/// The other place a provider puts the same password: a path segment.
///
/// Xtream stream URLs are `{base}/live/{username}/{password}/{id}.ts`, which
/// `ingest::xtream` builds for every stream it stores. Neither the userinfo
/// strip nor the query rewrite above can see that, so without this the
/// password rides in `SessionStats.url` — into `GET /api/proxy/stats/`, into
/// every `channel_stats` frame, and onto the Stats page.
///
/// Anchored on the `live` segment and on the arity after it, not on position
/// alone: a provider whose playlist simply lives under a directory of that
/// name has two segments rather than three, and rewriting it would break the
/// URL an operator reads to diagnose their own provider.
fn redact_credential_path(url: &mut url::Url) {
    let Some(segments) = url.path_segments().map(|s| s.collect::<Vec<_>>()) else {
        return;
    };

    let Some(marker) = segments.iter().position(|s| *s == "live") else {
        return;
    };
    // `live`, username, password, id — anything shorter is a different shape.
    if segments.len() < marker + 4 {
        return;
    }

    let mut rewritten = segments.clone();
    rewritten[marker + 1] = "REDACTED";
    rewritten[marker + 2] = "REDACTED";
    url.set_path(&rewritten.join("/"));
}

/// Every URL inside a free-text message, redacted in place.
///
/// Needed because the prefix is not the only way a URL reaches an error string:
/// `reqwest::Error`'s own `Display` appends ` for url (…)` with the URL it
/// dialled, so scrubbing only what this module interpolates would leave the
/// provider password in the same sentence, one clause later.
pub fn scrub(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;

    while let Some(start) = [rest.find("http://"), rest.find("https://")]
        .into_iter()
        .flatten()
        .min()
    {
        out.push_str(&rest[..start]);

        // A URL in prose ends at the first character that cannot be in one.
        // `)` is included because that is how reqwest wraps it.
        let tail = &rest[start..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | '`'))
            .unwrap_or(tail.len());
        // Punctuation that ended the sentence rather than the URL. Left in, it
        // is parsed as part of the query and comes back percent-encoded, which
        // turns `...&type=m3u: failed` into `...&type=m3u%3A failed`.
        while end > 0 && matches!(tail.as_bytes()[end - 1], b':' | b',' | b'.' | b';') {
            end -= 1;
        }
        out.push_str(&redact(&tail[..end]));
        rest = &tail[end..];
    }

    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_feed_path_stays_inside_the_cache_directory() {
        let state = crate::api::tests::bare_state();
        let path = feed_path(&state, "epg", 1);
        assert!(path.starts_with(state.config.cache_dir()));
        assert!(path.to_string_lossy().ends_with("epg-1.feed"));
    }
}
