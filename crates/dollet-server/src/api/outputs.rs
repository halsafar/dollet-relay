//! `/output/m3u` and `/output/epg`.
//!
//! Both are cached to disk and served from there. Plex refetches the guide
//! constantly and regenerating per request rescans the programme table each
//! time; a disk cache also costs nothing extra to write and does not degrade
//! if this instance ever ingests a full XMLTV feed rather than the sample's
//! few thousand rows.
//!
//! Generation is single-flight per cache key. Without it, the first Plex
//! refresh after an expiry starts as many full scans as there are concurrent
//! requests, which is exactly when the box is least able to afford them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use dollet_core::Error;
use dollet_core::db;
use dollet_core::db::channels::ChannelFilter;
use dollet_core::domain::{EffectiveChannel, Id};
use dollet_core::output::{TvgIdSource, dummy_epg, m3u, xmltv};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

use super::error::ApiResult;
use super::network::NetworkGate;
use super::origin::Origin;
use crate::AppState;

/// How long a rendered output is served before it is rendered again.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Longest guide an `/output/epg?days=` may ask for.
///
/// The value is client-supplied and unauthenticated, and it is added to the
/// current instant — `chrono` panics rather than saturating on overflow, so
/// `?days=4000000000` is a dropped connection. `epg::grid` bounds its own
/// window for the same reason and says so; this is the same rule on the
/// endpoint Plex actually fetches. Fourteen days is longer than any provider
/// publishes.
const MAX_GUIDE_DAYS: u32 = 14;

/// One mutex per cache key, so two requests for the same playlist share a
/// generation while a request for a different one is not blocked behind it.
///
/// Bounded, because the key includes the `Host` header: an unauthenticated
/// client varying it grows this map for the life of the process. Entries are
/// only ever held for the duration of one generation, so clearing the map when
/// it gets large costs at most a duplicated render.
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Distinct cache keys to keep before starting over. Far above the handful a
/// real deployment produces — a few output shapes times a few hostnames.
const MAX_LOCKS: usize = 256;

/// Age at which a cached output file is deleted rather than merely ignored.
///
/// `read_if_fresh` stops *serving* one after `CACHE_TTL`, which is not the same
/// as removing it: the cache key includes the `Host` header, so an
/// unauthenticated client varying it leaves a guide-sized file per value,
/// forever. Well past the TTL, so this never races a reader.
const CACHE_SWEEP_AFTER: Duration = Duration::from_secs(3600);

/// One of the two rendered outputs, and the only description of how its cached
/// artefacts are named.
///
/// `cache_path` writes by it, and `sweep_stale_cache` and `invalidate` decide by
/// it which files in the cache directory are this module's — that directory also
/// holds downloaded provider feeds and the artwork cache, which have different
/// lifetimes and different owners. A second copy of the naming would eventually
/// disagree, and the failure would be silent: files nobody sweeps, or a write
/// that invalidates nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output {
    Playlist,
    Guide,
}

impl Output {
    /// For the writes that change what both render — a rename moves the
    /// playlist's entry and the guide's `<display-name>` together.
    pub const BOTH: [Self; 2] = [Self::Playlist, Self::Guide];

    /// Filename prefix and extension.
    const fn naming(self) -> (&'static str, &'static str) {
        match self {
            Self::Playlist => ("m3u-", "m3u"),
            Self::Guide => ("epg-", "xml"),
        }
    }

    fn owns(self, name: &str) -> bool {
        let (prefix, extension) = self.naming();
        name.starts_with(prefix)
            && Path::new(name)
                .extension()
                .is_some_and(|found| found == extension)
    }
}

/// Drop the cached artefacts of the given outputs, because the data behind them
/// changed.
///
/// The cache exists so that a *read* does not rescan the programme table. It is
/// not there to keep serving bytes a write has already superseded: without this,
/// an operator who maps a channel and refreshes the guide watches the UI show
/// the new listings while Plex fetches — and then caches — the old ones, for up
/// to `CACHE_TTL`.
///
/// Best-effort, like the sweep: a file that will not delete must never fail the
/// write that noticed. Deleting one a request is reading is safe, because that
/// reader already holds its handle.
pub async fn invalidate(state: &AppState, kinds: &[Output]) {
    let Ok(mut entries) = tokio::fs::read_dir(state.config.cache_dir()).await else {
        return;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        if kinds.iter().any(|kind| kind.owns(&name.to_string_lossy())) {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/output/m3u", get(playlist))
        .route("/output/m3u/{profile}", get(playlist_for_profile))
        .route("/output/epg", get(guide))
        .route("/output/epg/{profile}", get(guide_for_profile))
}

#[derive(Deserialize, Default, Clone)]
struct OutputQuery {
    tvg_id_source: Option<String>,
    output_profile: Option<Id>,
    output_format: Option<String>,
    direct: Option<String>,
    cachedlogos: Option<String>,
    days: Option<u32>,
}

impl OutputQuery {
    fn tvg_id_source(&self) -> TvgIdSource {
        self.tvg_id_source
            .as_deref()
            .map(TvgIdSource::from_query)
            .unwrap_or_default()
    }

    fn is_true(value: &Option<String>) -> bool {
        value
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    }

    /// Whether logo URLs are rewritten to the artwork cache. Default on: the
    /// provider's own CDN is slow, rate-limited, and sometimes gone, and a
    /// client with every logo broken looks broken.
    fn cached_logos(&self) -> bool {
        self.cachedlogos
            .as_deref()
            .is_none_or(|value| !value.eq_ignore_ascii_case("false"))
    }

    /// The guide URL a playlist advertises, carrying only the parameters that
    /// change the guide itself — not `output_profile` or `direct`, which are
    /// properties of the stream URLs.
    fn epg_url(&self, base_url: &str) -> String {
        let mut query: Vec<String> = Vec::new();
        if let Some(source) = &self.tvg_id_source {
            query.push(format!("tvg_id_source={source}"));
        }
        if !self.cached_logos() {
            query.push("cachedlogos=false".to_owned());
        }

        match query.is_empty() {
            true => format!("{base_url}/output/epg"),
            false => format!("{base_url}/output/epg?{}", query.join("&")),
        }
    }

    /// Distinguishes the cached artefacts. Two requests differing in any of
    /// these must not share a file.
    fn cache_key(
        &self,
        kind: Output,
        profile: Option<&str>,
        origin: &Origin,
        artwork_base: &str,
    ) -> String {
        let (prefix, _) = kind.naming();
        format!(
            "{prefix}{}-{}-{}-{:?}-{:?}-{:?}-{}-{}-{:?}",
            // Absolute URLs are baked into the body, so two clients reaching
            // this server by different names must not share a file.
            origin.base_url(),
            // So is the artwork base, and unlike everything else here it cannot
            // vary between two live requests — it is fixed for the process. It
            // still belongs in the key, because the cache is on disk and
            // outlives the process: a guide rendered before the operator set
            // `DOLLET_ARTWORK_BASE_URL` would otherwise be served for another
            // five minutes after the restart that was meant to apply it, which
            // is exactly the "I set it and nothing changed" this variable exists
            // to end.
            artwork_base,
            profile.unwrap_or("all"),
            self.tvg_id_source().resolve(Some("t"), Some("g")),
            self.output_profile,
            self.output_format,
            Self::is_true(&self.direct),
            self.cached_logos(),
            self.days,
        )
    }
}

async fn playlist(
    State(state): State<AppState>,
    gate: NetworkGate,
    origin: Origin,
    Query(query): Query<OutputQuery>,
) -> ApiResult<Response> {
    render_playlist(&state, gate, &origin, &query, None).await
}

async fn playlist_for_profile(
    State(state): State<AppState>,
    gate: NetworkGate,
    origin: Origin,
    axum::extract::Path(profile): axum::extract::Path<String>,
    Query(query): Query<OutputQuery>,
) -> ApiResult<Response> {
    render_playlist(&state, gate, &origin, &query, Some(profile)).await
}

async fn guide(
    State(state): State<AppState>,
    gate: NetworkGate,
    origin: Origin,
    Query(query): Query<OutputQuery>,
) -> ApiResult<Response> {
    render_guide(&state, gate, &origin, &query, None).await
}

async fn guide_for_profile(
    State(state): State<AppState>,
    gate: NetworkGate,
    origin: Origin,
    axum::extract::Path(profile): axum::extract::Path<String>,
    Query(query): Query<OutputQuery>,
) -> ApiResult<Response> {
    render_guide(&state, gate, &origin, &query, Some(profile)).await
}

/// Resolve a profile name to its id.
///
/// A name that does not exist is a 404 here, unlike the HDHR lineup: a client
/// asking for a named playlist has been misconfigured, and an empty playlist
/// would look like "the provider has no channels today".
async fn profile_id(state: &AppState, name: Option<&str>) -> Result<Option<Id>, Error> {
    match name {
        Some(name) => db::channel_profiles::by_name(&state.db, name)
            .await?
            .map(|profile| Some(profile.id))
            .ok_or(Error::NotFound),
        None => Ok(None),
    }
}

/// The lineup an output serves: visible channels, narrowed to one profile
/// when the request names one. Shared by every output, so they cannot
/// disagree about what "visible" means.
pub(super) async fn visible_channels(
    state: &AppState,
    profile: Option<Id>,
) -> Result<Vec<EffectiveChannel>, Error> {
    Ok(db::channels::list_effective(
        &state.db,
        &ChannelFilter {
            profile_id: profile,
            visible_only: true,
            ..Default::default()
        },
        None,
        None,
    )
    .await?
    .results)
}

async fn render_playlist(
    state: &AppState,
    _gate: NetworkGate,
    origin: &Origin,
    query: &OutputQuery,
    profile: Option<String>,
) -> ApiResult<Response> {
    super::origin::record("m3u", origin);
    let id = profile_id(state, profile.as_deref()).await?;
    let artwork = artwork_base(state, origin);
    let key = query.cache_key(Output::Playlist, profile.as_deref(), origin, &artwork);

    let body = cached(state, Output::Playlist, &key, || async {
        let channels = visible_channels(state, id).await?;

        let mut stream_query: Vec<(&str, String)> = Vec::new();
        if let Some(output) = query.output_profile {
            stream_query.push(("output_profile", output.to_string()));
        }
        if let Some(format) = &query.output_format {
            stream_query.push(("output_format", format.clone()));
        }

        let direct = OutputQuery::is_true(&query.direct);
        let direct_urls = if direct {
            first_source_urls(state, &channels).await?
        } else {
            HashMap::new()
        };

        let mut channels = channels;
        if query.cached_logos() {
            use_cached_logos(state, &mut channels, &artwork).await?;
        }

        let entries: Vec<m3u::M3uChannel<'_>> = channels
            .iter()
            .map(|channel| m3u::M3uChannel {
                channel,
                direct_url: direct_urls.get(&channel.id).map(String::as_str),
            })
            .collect();

        Ok(m3u::render(
            &m3u::M3uOptions {
                base_url: &origin.base_url(),
                epg_url: &query.epg_url(&origin.base_url()),
                tvg_id_source: query.tvg_id_source(),
                stream_query: &stream_query,
                xtream_credentials: None,
            },
            &entries,
        )
        .into_bytes())
    })
    .await?;

    Ok(serve(body, "audio/x-mpegurl"))
}

/// Which base artwork URLs are built from: `DOLLET_ARTWORK_BASE_URL` when the
/// deployment sets one, otherwise the address this request arrived on.
///
/// Artwork is the one class of URL a *browser* resolves rather than the server
/// that fetched the document holding it. Plex fetches the lineup and the guide
/// itself, but hands the guide's `<icon>` URLs to whatever renders the guide —
/// so on a deployment Plex reaches over a docker network, every logo points at
/// an address no viewer's browser can route to. The two cannot be separated by
/// `DOLLET_ADVERTISED_BASE_URL`, because moving that moves the stream URLs with
/// it, and those have to stay on the address that works for the server.
///
/// Resolved at the call sites rather than inside `use_cached_logos`, because
/// the request's origin — the fallback — is in scope there and not here.
pub fn artwork_base(state: &AppState, origin: &Origin) -> String {
    state
        .config
        .artwork_base_url
        .as_deref()
        .and_then(super::origin::from_configured)
        .map_or_else(|| origin.base_url(), |configured| configured.base_url())
}

/// Point every logo at this server's artwork cache rather than the provider's
/// CDN.
///
/// The cache is addressed by logo id, which `EffectiveChannel` does not carry
/// — it carries the resolved URL — so the ids come back from the same view in
/// a second query and the URLs are rewritten here, where the request
/// parameters that decide it are in scope.
pub async fn use_cached_logos(
    state: &AppState,
    channels: &mut [EffectiveChannel],
    base_url: &str,
) -> Result<(), Error> {
    let ids: Vec<Id> = channels.iter().map(|channel| channel.id).collect();
    let logos: HashMap<Id, Id> = db::channels::logo_ids(&state.db, &ids)
        .await?
        .into_iter()
        .collect();

    for channel in channels {
        if let Some(logo) = logos.get(&channel.id) {
            channel.logo_url = Some(format!("{base_url}/api/channels/logos/{logo}/cache/"));
        }
    }
    Ok(())
}

/// Provider URLs for `?direct=true`, which hands the client the upstream
/// instead of proxying it.
async fn first_source_urls(
    state: &AppState,
    channels: &[EffectiveChannel],
) -> Result<HashMap<Id, String>, Error> {
    let mut out = HashMap::new();
    for channel in channels {
        if let Some(url) = db::streams::for_channel(&state.db, channel.id)
            .await?
            .into_iter()
            .find_map(|stream| stream.url.filter(|url| !url.is_empty()))
        {
            out.insert(channel.id, url);
        }
    }
    Ok(out)
}

async fn render_guide(
    state: &AppState,
    _gate: NetworkGate,
    origin: &Origin,
    query: &OutputQuery,
    profile: Option<String>,
) -> ApiResult<Response> {
    super::origin::record("epg", origin);
    let id = profile_id(state, profile.as_deref()).await?;
    let artwork = artwork_base(state, origin);
    let key = query.cache_key(Output::Guide, profile.as_deref(), origin, &artwork);
    let source = query.tvg_id_source();
    let days = query.days;
    let cache_logos = query.cached_logos();

    let body = cached(state, Output::Guide, &key, || async move {
        let mut channels = visible_channels(state, id).await?;
        // The guide's `<icon>` elements carry the same artwork URLs the
        // playlist does, so the same choice applies.
        if cache_logos {
            use_cached_logos(state, &mut channels, &artwork).await?;
        }
        write_guide(state, &channels, source, days).await
    })
    .await?;

    Ok(serve(body, "application/xml"))
}

/// Render a guide for an explicit channel list. `xmltv.php` serves the same
/// bytes under Xtream credentials, so it shares the generator rather than
/// reimplementing the two-pass structure.
pub async fn guide_bytes(
    state: &AppState,
    channels: &[EffectiveChannel],
    source: TvgIdSource,
    days: Option<u32>,
) -> Result<Vec<u8>, Error> {
    write_guide(state, channels, source, days).await
}

/// Two passes, channels then programmes, because XMLTV requires every
/// `<channel>` before the `<programme>` elements that reference it.
async fn write_guide(
    state: &AppState,
    channels: &[EffectiveChannel],
    source: TvgIdSource,
    days: Option<u32>,
) -> Result<Vec<u8>, Error> {
    let mut writer = xmltv::XmltvWriter::new(Vec::new());
    writer.start(&xmltv::XmltvOptions {
        tvg_id_source: source,
        ..Default::default()
    })?;

    for channel in channels {
        writer.write_effective_channel(channel, source)?;
    }

    let now = Utc::now();
    let window = days.map(|days| now + chrono::Duration::days(i64::from(days.min(MAX_GUIDE_DAYS))));
    let dummy_sources = super::dummy_source_ids(state).await?;

    for channel in channels {
        let id = xmltv::channel_id(channel, source);

        let Some(epg_data_id) = channel.epg_data_id else {
            continue;
        };

        // A channel on a dummy source has no stored programmes at all: its
        // guide is generated, and without this it simply has no listings.
        if super::is_dummy(state, epg_data_id, &dummy_sources).await? {
            for program in dummy_epg::generate(
                &channel.name,
                now,
                &dummy_epg::DummyOptions {
                    // Clamped like `window` above: `generate` walks
                    // `start + Duration::days(day)` per day, so an unclamped
                    // `?days=4000000000` burns seconds of CPU and then panics
                    // on the date overflow.
                    num_days: days.map_or(3, |days| days.min(MAX_GUIDE_DAYS)),
                    export_cutoff: window,
                    ..Default::default()
                },
            ) {
                writer.write_programme(&xmltv::Programme::from_dummy(&id, &program))?;
            }
            continue;
        }

        let until = window.unwrap_or_else(|| now + chrono::Duration::days(365));
        for program in db::epg::programs(
            &state.db,
            epg_data_id,
            now - chrono::Duration::days(1),
            until,
        )
        .await?
        {
            writer.write_programme(&xmltv::Programme::from_program(&id, &program))?;
        }
    }

    writer.finish()?;
    Ok(writer.into_inner())
}

fn serve(body: Vec<u8>, content_type: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}

/// Read the cached artefact, generating it under a per-key lock if it is
/// missing or stale.
///
/// Falls back to generating in-process whenever the cache cannot be used, so a
/// read-only or full `/data` degrades performance rather than the service.
async fn cached<F, Fut>(
    state: &AppState,
    kind: Output,
    key: &str,
    generate: F,
) -> Result<Vec<u8>, Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<u8>, Error>>,
{
    let path = cache_path(state, kind, key);

    if let Some(fresh) = read_if_fresh(&path).await {
        return Ok(fresh);
    }

    let lock = {
        let mut locks = LOCKS.lock().await;
        // Dropped wholesale rather than evicted one by one: an entry is only
        // useful while someone holds it, and a holder keeps its `Arc` alive
        // across this. The worst case is two callers rendering the same output
        // once, which is what happens on a cold cache anyway.
        if locks.len() >= MAX_LOCKS {
            locks.clear();
        }
        Arc::clone(locks.entry(key.to_owned()).or_default())
    };
    let _guard = lock.lock().await;

    // Re-checked under the lock: whoever held it was generating exactly this,
    // so the follower reads their result instead of repeating the scan.
    if let Some(fresh) = read_if_fresh(&path).await {
        return Ok(fresh);
    }

    // Cheap, and only on a miss — which after the first request per output
    // shape is rare. Awaited rather than spawned so a burst of misses cannot
    // start a sweep each.
    sweep_stale_cache(state).await;

    let body = generate().await?;

    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Written to a temporary name and renamed, so a concurrent reader never
    // sees a half-written playlist.
    let temporary = path.with_extension(format!("{}.tmp", kind.naming().1));
    if tokio::fs::write(&temporary, &body).await.is_ok()
        && let Err(e) = tokio::fs::rename(&temporary, &path).await
    {
        tracing::warn!(error = %e, "output cache not written");
        let _ = tokio::fs::remove_file(&temporary).await;
    }

    Ok(body)
}

fn cache_path(state: &AppState, kind: Output, key: &str) -> PathBuf {
    // The key contains query values, so it is hashed rather than used as a
    // filename: `/` and `..` in a profile name would otherwise escape the
    // cache directory.
    let digest = super::cache_digest(key);
    let (prefix, extension) = kind.naming();
    state
        .config
        .cache_dir()
        .join(format!("{prefix}{digest}.{extension}"))
}

/// Delete cached output files nothing will serve again.
///
/// Best-effort: a failure here is a directory that stays larger than it needs
/// to be, which must not fail the request that noticed.
async fn sweep_stale_cache(state: &AppState) {
    let Ok(mut entries) = tokio::fs::read_dir(state.config.cache_dir()).await else {
        return;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !Output::BOTH.iter().any(|kind| kind.owns(&name)) {
            continue;
        }

        let stale = entry
            .metadata()
            .await
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > CACHE_SWEEP_AFTER);
        if stale {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
}

async fn read_if_fresh(path: &Path) -> Option<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await.ok()?;
    let age = SystemTime::now()
        .duration_since(metadata.modified().ok()?)
        .ok()?;
    if age > CACHE_TTL {
        return None;
    }

    // Streamed rather than read whole so a large guide does not spike memory,
    // which is the number this project exists to move.
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut reader = ReaderStream::new(file);
    let mut body = Vec::with_capacity(metadata.len() as usize);
    use futures_util::StreamExt;
    while let Some(chunk) = reader.next().await {
        body.extend_from_slice(&chunk.ok()?);
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_difference_produces_a_different_cache_key() {
        let base = OutputQuery::default();
        let origin = Origin {
            scheme: "http".into(),
            host: "ipx.test".into(),
            port: Some(9191),
        };
        let elsewhere = Origin {
            host: "tv.example".into(),
            ..origin.clone()
        };
        let here = &origin.base_url();
        let keys = [
            base.cache_key(Output::Playlist, None, &origin, here),
            base.cache_key(Output::Playlist, Some("Kids"), &origin, here),
            base.cache_key(Output::Guide, None, &origin, here),
            base.cache_key(Output::Playlist, None, &elsewhere, here),
            // The artwork base is baked into the body like every other absolute
            // URL, and the cache outlives the process that wrote it — so a file
            // rendered before the operator set `DOLLET_ARTWORK_BASE_URL` must
            // not be what they get after setting it.
            base.cache_key(Output::Playlist, None, &origin, "https://art.example"),
            base.cache_key(Output::Guide, None, &origin, "https://art.example"),
            OutputQuery {
                tvg_id_source: Some("tvg_id".into()),
                ..base.clone()
            }
            .cache_key(Output::Playlist, None, &origin, here),
            OutputQuery {
                output_profile: Some(1),
                ..base.clone()
            }
            .cache_key(Output::Playlist, None, &origin, here),
            OutputQuery {
                direct: Some("true".into()),
                ..base.clone()
            }
            .cache_key(Output::Playlist, None, &origin, here),
            OutputQuery {
                cachedlogos: Some("false".into()),
                ..base.clone()
            }
            .cache_key(Output::Playlist, None, &origin, here),
            OutputQuery {
                days: Some(1),
                ..base.clone()
            }
            .cache_key(Output::Guide, None, &origin, here),
        ];

        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len(), "two requests would share a file");
    }

    #[tokio::test]
    async fn a_profile_name_cannot_escape_the_cache_directory() {
        let config = dollet_core::config::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            data_dir: PathBuf::from("/data"),
            advertised_base_url: None,
            artwork_base_url: None,
            trusted_proxies: dollet_core::config::TrustedProxies::None,
            import_backup: None,
            log_filter: "off".into(),
        };
        let state = AppState {
            db: sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
            config: Arc::new(config),
        };

        let path = cache_path(&state, Output::Playlist, "m3u-../../etc/passwd");
        assert!(path.starts_with("/data/cache"), "{path:?} escaped");
        assert!(!path.to_string_lossy().contains(".."));
    }

    /// An `AppState` whose cache directory is a real, empty temporary one.
    fn state_in(dir: &tempfile::TempDir) -> AppState {
        let mut state = super::super::tests::bare_state();
        let mut config = (*state.config).clone();
        config.data_dir = dir.path().to_path_buf();
        state.config = Arc::new(config);
        state
    }

    #[tokio::test]
    async fn invalidating_one_output_spares_the_other_and_everything_that_is_not_ours() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(&dir);
        let cache = state.config.cache_dir();
        tokio::fs::create_dir_all(cache.join("artwork"))
            .await
            .unwrap();

        // Two of ours, and the neighbours: a downloaded provider feed the next
        // refresh reads, and an artwork file the logo endpoint serves. Deleting
        // either of those costs a re-download nobody asked for.
        let files = [
            cache.join("m3u-000000000000000a.m3u"),
            cache.join("epg-000000000000000b.xml"),
            cache.join("m3u-1.feed"),
            cache.join("epg-1.feed"),
            cache.join("artwork").join("000000000000000c"),
        ];
        for file in &files {
            tokio::fs::write(file, b"x").await.unwrap();
        }

        invalidate(&state, &[Output::Guide]).await;
        assert!(files[0].exists(), "the playlist was dropped with the guide");
        assert!(!files[1].exists(), "the guide survived its invalidation");
        for neighbour in &files[2..] {
            assert!(neighbour.exists(), "{neighbour:?} is not ours to delete");
        }

        invalidate(&state, &Output::BOTH).await;
        assert!(!files[0].exists(), "the playlist survived its invalidation");
        for neighbour in &files[2..] {
            assert!(neighbour.exists(), "{neighbour:?} is not ours to delete");
        }
    }

    #[tokio::test]
    async fn invalidating_a_cache_directory_that_does_not_exist_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(&dir);
        assert!(!state.config.cache_dir().exists());

        // Before the first render there is no directory, and a write must not
        // fail because there was nothing to drop.
        invalidate(&state, &Output::BOTH).await;
    }

    #[tokio::test]
    async fn the_artwork_base_falls_back_to_the_request_and_is_normalised_when_set() {
        let mut state = super::super::tests::bare_state();
        let origin = Origin {
            scheme: "http".into(),
            host: "172.25.0.41".into(),
            port: Some(9191),
        };

        // Unset, artwork follows the request like every other URL in the
        // document.
        assert_eq!(artwork_base(&state, &origin), "http://172.25.0.41:9191");

        // Set, it wins — and through the same parse the advertised base uses, so
        // a trailing slash and a default port normalise identically rather than
        // producing `https://tv.example.com//api/channels/logos/…`.
        let mut config = (*state.config).clone();
        config.artwork_base_url = Some("https://tv.example.com:443/".into());
        state.config = Arc::new(config);
        assert_eq!(artwork_base(&state, &origin), "https://tv.example.com");
    }
}
