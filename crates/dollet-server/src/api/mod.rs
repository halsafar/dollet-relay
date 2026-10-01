//! HTTP handlers.
//!
//! Routes keep their paths: an existing Plex configuration or bookmark keeps
//! working across upgrades. Two things to know before adding any:
//!
//! - axum's router resolves by specificity and **panics at startup** on
//!   conflicting inserts, rather than resolving top-down. The bare
//!   `/{username}/{password}/{channel_id}` Xtream route is the one that will
//!   collide with the SPA fallback; it needs a route-level test, not an eyeball.
//! - every HDHR lineup URL is absolute, so `X-Forwarded-Proto`/`X-Forwarded-Host`
//!   handling is load-bearing. Getting it wrong fails as "discovery works,
//!   playback doesn't".
//!
//! `/ws` authenticates by **subprotocol**: the client offers
//! `['auth.jwt', <access token>]`, the server validates the second entry and
//! echoes back `auth.jwt`. A browser cannot set an `Authorization` header on a
//! WebSocket, and a token in the URL lands in `TraceLayer`'s log line and in
//! every proxy access log in front of it. Stats frames are filtered per
//! receiver and admin-only, because they carry channel UUIDs usable against the
//! anonymous stream endpoint, upstream URLs, and client IPs.

mod artwork;
pub mod auth;
mod channels;
mod core;
mod epg;
mod error;
mod hdhr;
mod importer;
mod ingest;
mod jobs;
mod logos;
mod m3u;
mod network;
mod notifications;
mod origin;
mod outputs;
mod stream;
mod streams;
mod users;
mod ws;
mod xc;

/// Percent-encode one component of a URL — a path segment or a query value.
///
/// A channel profile called `Living Room` reaches `/hdhr/Living%20Room` and an
/// Xtream password reaches `password=p%40ss`, and the two must agree about
/// which bytes are safe or one of them builds a URL its own server cannot
/// parse.
///
/// `url::Url` cannot do this: it percent-encodes when *building* a URL, and
/// these are interpolated into format strings. The unreserved set is RFC 3986's.
pub fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The `User-Agent` for an outbound provider fetch.
///
/// `override_id` is whatever the thing being fetched carries — an M3U account
/// has its own; an EPG source has no column for one, so it passes `None` and
/// gets the global default. Falls back to this build's own name, because a
/// request with no user agent is one some providers refuse outright.
pub async fn user_agent_for(
    state: &AppState,
    override_id: Option<dollet_core::domain::Id>,
) -> Result<String, dollet_core::Error> {
    let settings: dollet_core::settings::StreamSettings =
        dollet_core::settings::load(&state.db).await?;
    let id = override_id.or(settings.default_user_agent);
    let configured = match id {
        Some(id) => dollet_core::db::profiles::get_user_agent(&state.db, id).await?,
        None => None,
    };
    Ok(configured
        .map(|agent| agent.user_agent)
        .unwrap_or_else(|| format!("dollet-relay/{}", env!("CARGO_PKG_VERSION"))))
}

/// Guide sources whose listings are generated per request rather than stored.
///
/// A channel mapped to one has no `program` rows, which is not the same as
/// having nothing on — every caller that reads programmes has to know the
/// difference, so the lookup lives in one place.
pub async fn dummy_source_ids(
    state: &AppState,
) -> Result<Vec<dollet_core::domain::Id>, dollet_core::Error> {
    Ok(dollet_core::db::epg::list_sources(&state.db)
        .await?
        .into_iter()
        .filter(|source| source.source_type == dollet_core::domain::EpgSourceType::Dummy)
        .map(|source| source.id)
        .collect())
}

pub async fn is_dummy(
    state: &AppState,
    epg_data_id: dollet_core::domain::Id,
    dummy: &[dollet_core::domain::Id],
) -> Result<bool, dollet_core::Error> {
    if dummy.is_empty() {
        return Ok(false);
    }
    Ok(dollet_core::db::epg::get_data(&state.db, epg_data_id)
        .await?
        .and_then(|data| data.epg_source_id)
        .is_some_and(|source| dummy.contains(&source)))
}

#[cfg(test)]
mod tests;

use axum::Router;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use dollet_core::db::Paging;

pub use importer::{Source as ImportSource, log_report, maybe_import_on_boot, run as run_import};
pub use jobs::shutdown as stop_scheduler;
pub use jobs::start as start_scheduler;
pub use stream::shutdown_streams;

use crate::AppState;

/// Routes that do **not** live under `/api`: the playback endpoint, the
/// HDHomeRun surface, the playlist and guide outputs, the WebSocket, and the
/// Xtream Codes API.
///
/// The test of whether something belongs here is whether a client outside this
/// repository holds the URL — Plex reading `lineup.json`, a player pointed at
/// `/output/m3u`, whatever the operator pasted where. Those paths are the
/// contract and cannot be nested. Being unauthenticated is not the test, and
/// neither is being about streams: the session-control endpoints are both of
/// those things and they are admin-only, so they are under `/api`.
///
/// `main.rs` must `.merge(api::public_router())` alongside its `nest("/api")`.
pub fn public_router() -> Router<AppState> {
    stream::public_router()
        .merge(hdhr::router())
        .merge(outputs::router())
        .merge(ws::router())
        .merge(xc::router())
}

/// Nesting is one level per API area; resources inside an area are merged
/// rather than nested again, because two `nest` calls sharing a prefix insert
/// conflicting wildcards and panic at startup.
pub fn router() -> Router<AppState> {
    Router::new()
        .nest("/accounts", auth::router().merge(users::router()))
        .nest("/core", core::router())
        .nest(
            "/channels",
            channels::router()
                .merge(streams::router())
                .merge(logos::router())
                .merge(artwork::router()),
        )
        .nest("/epg", epg::router())
        .nest("/m3u", m3u::router())
        .merge(notifications::router())
        .merge(stream::router())
}

/// The refresh-status fields a provider account and a guide source both carry.
///
/// The job's own words, not a translation — `/api/core/jobs/` answers the same
/// row for the same account, and a page that reads both should not have to
/// know two names for `running`.
///
/// `updated_at` is when the account or source last *successfully* refreshed,
/// which is the question the Sources page column asks. A refresh in flight or
/// one that failed leaves it on the last good one, and null means it has never
/// had one.
fn job_status(job: Option<&dollet_core::db::jobs::Job>) -> Value {
    json!({
        "status": job.map_or(dollet_core::db::jobs::State::Idle, |job| job.state).as_str(),
        "progress": job.map_or(0.0, |job| job.progress),
        "last_message": job.and_then(|job| job.message.clone().or(job.last_error.clone())),
        "updated_at": job.and_then(|job| job.last_success_at),
        "next_run_at": job.and_then(|job| job.next_run_at),
    })
}

/// Merge `job_status` into an object being built.
fn with_job_status(mut value: Value, job: Option<&dollet_core::db::jobs::Job>) -> Value {
    if let (Some(target), Value::Object(fields)) = (value.as_object_mut(), job_status(job)) {
        target.extend(fields);
    }
    value
}

/// The longest refresh interval an account or a source may be given: a year.
///
/// Not a limit anybody asked for — it is the guard on `Utc::now() +
/// Duration::hours(n)`, which panics rather than saturating once `n` is large
/// enough to overflow the date, from a handler with no catch layer above it.
const MAX_REFRESH_HOURS: u32 = 24 * 365;

/// FNV-1a over a cache key.
///
/// The key is not safe as a filename: `/` and `..` in a profile name escape
/// the cache directory, and nothing about either caller's signature stops it
/// passing a URL.
///
/// Not a security boundary: it makes a key into a filename, and the
/// alternative is percent-encoding something unbounded into a path component.
fn cache_digest(key: &str) -> String {
    let digest = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    });
    format!("{digest:016x}")
}

/// A bulk action's payload: the rows it applies to.
///
/// `ids` only: per-resource aliases are not accepted because nothing sends
/// them and 1.0 would support them forever.
#[derive(Deserialize)]
struct IdList {
    ids: Vec<dollet_core::domain::Id>,
}

/// Tell an absent key apart from an explicit `null`.
///
/// serde collapses both to `None` for an `Option<Option<T>>` field, so without
/// this "clear the group" and "leave the group alone" arrive identically — and
/// the clear silently does nothing. Deserializing the inner value first makes
/// `null` land as `Some(None)`.
fn explicit_null<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Default page size, which the UI's table also assumes.
const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 10_000;

/// Resolve paging from the query string.
///
/// `?all=true` is the one way to ask for an unpaginated list, everywhere.
fn paging(page: Option<u32>, page_size: Option<u32>, all: bool) -> Option<Paging> {
    if all {
        return None;
    }
    Some(Paging {
        page: page.unwrap_or(1).max(1),
        page_size: page_size
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE),
    })
}

/// The shape of every `/api/` list response.
///
/// One shape: `{results, count, page, pages}`.
///
/// There are no `next`/`previous` URLs: `pages` lets a caller build any page
/// URL.
///
/// An unpaginated list still reports `page: 1, pages: 1`, so a client never has
/// to branch on their absence — which is the property that makes the shape
/// worth having at all.
fn listing<T: Serialize>(paging: Option<Paging>, count: i64, results: Vec<T>) -> Value {
    let (page, pages) = match paging {
        Some(paging) => {
            let size = i64::from(paging.page_size).max(1);
            // `div_ceil` is unstable for i64.
            (i64::from(paging.page), ((count + size - 1) / size).max(1))
        }
        None => (1, 1),
    };

    json!({
        "results": results,
        "count": count,
        "page": page,
        "pages": pages,
    })
}

/// A list that was never paginated — a handful of rows with no query for it.
///
/// Same shape as everything else. The count is the list's own length, because
/// there is no larger set it is a window onto.
fn whole_list<T: Serialize>(results: Vec<T>) -> Value {
    let count = results.len() as i64;
    listing(None, count, results)
}

/// A list the server truncated to a limit the caller named.
///
/// `whole_list` is wrong for these: its count is `results.len()`, so a capped
/// answer reports the cap as the size of the collection and says `pages: 1`.
/// The documented contract is that `count` is the collection — `refuse_all`
/// exists a few lines down because "a capped list looks complete, and a client
/// that believes it has everything deletes what it thinks is missing".
///
/// So the caller passes the real total alongside the page it got.
fn capped_list<T: Serialize>(limit: i64, count: i64, results: Vec<T>) -> Value {
    let page_size = u32::try_from(limit.max(1)).unwrap_or(u32::MAX);
    listing(Some(Paging { page: 1, page_size }), count, results)
}

/// Whether a query string asked for the whole list.
///
/// Accepts only `true`, so a typo reads as "no" and returns a page rather than
/// a catalogue.
fn wants_all(value: Option<&str>) -> bool {
    value == Some("true")
}

/// Endpoints where `?all=true` is refused rather than honoured.
///
/// `/channels/streams/` is the one table that plausibly holds tens of
/// thousands of rows: a provider with a full catalogue does. Refusing beats
/// silently capping: a capped list
/// looks complete, and a client that believes it has everything deletes what it
/// thinks is missing.
fn refuse_all(endpoint: &str) -> dollet_core::Error {
    dollet_core::Error::invalid(format!(
        "`all=true` is not available on {endpoint}, which can hold far more rows than a \
         client can use at once; page through it with `page` and `page_size` instead"
    ))
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    /// The bytes the Connect page has to reproduce.
    ///
    /// `web/src/pages/connectUrls.js` builds the same URLs in the browser,
    /// and `encodeURIComponent` leaves `!'()*` alone where this does not — so a
    /// profile called `Kids (2)` would be offered under a path this server's own
    /// lineup never advertises. Both sides assert these exact strings.
    #[test]
    fn a_path_segment_encodes_everything_outside_rfc_3986s_unreserved_set() {
        assert_eq!(urlencode("Living Room"), "Living%20Room");
        assert_eq!(urlencode("Kids (2)"), "Kids%20%282%29");

        // The unreserved set itself, which must survive untouched — `~` is in
        // it and is the one most encoders get wrong in the other direction.
        assert_eq!(urlencode("aZ09-_.~"), "aZ09-_.~");

        // Non-ASCII goes out as its UTF-8 bytes, uppercase hex.
        assert_eq!(urlencode("Kanál"), "Kan%C3%A1l");
    }

    #[test]
    fn all_is_the_only_way_to_ask_for_everything() {
        assert!(paging(None, None, true).is_none());
        assert!(paging(Some(3), Some(10), true).is_none());

        // And a typo is not a way to ask for it. `?all=1` reading as "yes"
        // would turn a mistyped filter into a full catalogue fetch.
        assert!(!wants_all(Some("1")));
        assert!(!wants_all(Some("yes")));
        assert!(!wants_all(Some("True")));
        assert!(!wants_all(None));
        assert!(wants_all(Some("true")));
    }

    #[test]
    fn omitting_paging_means_the_first_default_sized_page() {
        // Not "everything", so a client never has to know which endpoint it
        // is talking to.
        let paging = paging(None, None, false).expect("paged by default");
        assert_eq!(paging.page, 1);
        assert_eq!(paging.page_size, DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn page_size_is_clamped_and_page_is_never_zero() {
        let paging = paging(Some(0), Some(999_999), false).unwrap();
        assert_eq!(paging.page, 1);
        assert_eq!(paging.page_size, MAX_PAGE_SIZE);
        assert_eq!(paging.offset(), 0);
    }

    #[test]
    fn every_list_reports_where_it_is_and_how_far_it_goes() {
        let body = listing(
            Some(Paging {
                page: 2,
                page_size: 2,
            }),
            10,
            vec![1, 2],
        );

        assert_eq!(body["count"], 10);
        assert_eq!(body["page"], 2);
        assert_eq!(body["pages"], 5);
        assert_eq!(body["results"], json!([1, 2]));
    }

    #[test]
    fn a_partial_last_page_still_counts_as_a_page() {
        let body = listing(
            Some(Paging {
                page: 3,
                page_size: 4,
            }),
            9,
            vec![9],
        );
        assert_eq!(body["pages"], 3);
    }

    #[test]
    fn an_unpaginated_list_has_the_same_shape_as_a_paginated_one() {
        // The property the shape is for: a client never branches on a
        // missing key, so it never has to know which endpoint it asked.
        let whole = whole_list(vec!["a", "b"]);
        assert_eq!(whole["count"], 2);
        assert_eq!(whole["page"], 1);
        assert_eq!(whole["pages"], 1);
        assert_eq!(whole["results"], json!(["a", "b"]));

        // Including when it is empty: zero rows is one page of nothing, not
        // zero pages, or a client's "page < pages" loop never runs.
        let empty = whole_list(Vec::<&str>::new());
        assert_eq!(empty["count"], 0);
        assert_eq!(empty["pages"], 1);
        assert_eq!(empty["results"], json!([]));

        let paged = listing(
            Some(Paging {
                page: 1,
                page_size: 50,
            }),
            0,
            Vec::<&str>::new(),
        );
        assert_eq!(paged["pages"], 1);
    }
}
