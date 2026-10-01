//! The streaming endpoint: `/proxy/ts/stream/{uuid}`.
//!
//! This module is the whole of `dollet-server`'s contribution to playback. The
//! engine takes an ordered list of URLs and constants and knows nothing about a
//! database; everything here is the translation between the two.
//!
//! The order of `channel_stream` *is* the failover order, so it is preserved
//! end to end rather than re-sorted anywhere.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dollet_core::db;
use dollet_core::domain::{EffectiveChannel, Id};
use dollet_core::settings::{ProxySettings, StreamSettings};
use dollet_core::{Error, settings};
use dollet_stream::{
    ChannelSpec, ClientDescriptor, Connection, FailoverConfig, OutputKey, Registry, SourceLimit,
    SourceProfile, StreamConfig, StreamSource, Transcode,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::auth::{AdminUser, ClientAddr};
use super::error::ApiResult;
use super::network;
use crate::AppState;

/// Process-global, which is what it means: the sessions it owns are ffmpeg
/// children and provider sockets, not per-request state, and one process holds
/// exactly one set of them.
///
/// Built once, so a change to `proxy_settings` needs a restart to take effect.
static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub async fn registry(state: &AppState) -> Result<&'static Registry, Error> {
    if let Some(existing) = REGISTRY.get() {
        return Ok(existing);
    }

    let proxy: ProxySettings = settings::load(&state.db).await?;
    let agent = super::user_agent_for(state, None).await?;

    // `allow_private` is required, not a convenience: a LAN tuner is a
    // supported source and the guarded resolver rejects RFC1918 by default,
    // so without it every such stream fails to open.
    let http = dollet_core::http::client(&agent, true).map_err(|e| Error::Other(e.into()))?;

    Ok(REGISTRY.get_or_init(|| Registry::new(Arc::new(stream_config(&proxy)), http)))
}

/// End every live session, so the graceful drain has something to drain.
///
/// An MPEG-TS response body never completes on its own, so `axum::serve`'s
/// shutdown would otherwise wait for viewers until the runtime SIGKILLs the
/// container — and everything sequenced after the drain, the scheduler's own
/// shutdown included, never runs. Stopping a session makes each `ClientStream`
/// end once it has handed over the tail of the ring.
pub fn shutdown_streams() {
    if let Some(registry) = REGISTRY.get() {
        registry.stop_all();
    }
}

fn stream_config(proxy: &ProxySettings) -> StreamConfig {
    let ring_duration = Duration::from_secs(u64::from(proxy.ring_seconds.max(1)));
    StreamConfig {
        ring_duration,
        // The settings default is the engine's own duration-derived ceiling,
        // so the two agree unless an operator deliberately narrows it. A cap
        // picked independently of the duration silently shortens retention on
        // any source above the bitrate it was sized for.
        ring_max_bytes: usize::try_from(proxy.ring_max_bytes)
            .unwrap_or(usize::MAX)
            .max(1),
        new_client_behind: Duration::from_secs(u64::from(proxy.new_client_behind_seconds)),
        connection_timeout: Duration::from_secs(u64::from(proxy.channel_init_grace_period.max(1))),
        channel_shutdown_delay: Duration::from_secs(u64::from(proxy.channel_shutdown_delay)),
        channel_client_wait: Duration::from_secs(u64::from(proxy.channel_client_wait_period)),
        buffering_timeout: Duration::from_secs(u64::from(proxy.buffering_timeout.max(1))),
        buffering_speed: proxy.buffering_speed,
        failover: FailoverConfig::default(),
        ..StreamConfig::default()
    }
}

/// The one route here a client holds.
///
/// A channel's playback URL is in `lineup.json`, in the served M3U, and in
/// whatever the operator pasted into a player. It is pinned by the golden
/// corpus for that reason and cannot move.
pub fn public_router() -> Router<AppState> {
    Router::new().route("/proxy/ts/stream/{uuid}", get(stream))
}

/// Session control and live stats: admin-only, and called by this project's
/// own web UI and nothing else.
///
/// Mounted under `/api` with every other endpoint of that kind: no client
/// outside this repository holds these URLs.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/proxy/ts/stop/{uuid}", post(stop_channel))
        .route("/proxy/ts/stop_client/{uuid}", post(stop_client))
        .route("/proxy/ts/next_stream/{uuid}", post(next_stream))
        .route("/proxy/ts/change_stream/{uuid}", post(change_stream))
        .route("/proxy/stats/", get(stats))
}

#[derive(Deserialize, Default)]
struct StreamQuery {
    pub output_profile: Option<Id>,
}

async fn stream(
    State(state): State<AppState>,
    ClientAddr(ip): ClientAddr,
    Path(uuid): Path<Uuid>,
    Query(query): Query<StreamQuery>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    // The `STREAMS` allowlist, on the endpoint the whole class is named after.
    // Every lineup URL, every `/output/m3u` entry and Plex itself point here,
    // and this route carries no credentials — the allowlist is the only control
    // in front of it. Resolving the address without gating on it would make
    // the allowlist a suggestion.
    network::allowed(&state, ip, network::STREAMS).await?;

    let channel = db::channels::get_effective_by_uuid(&state.db, uuid)
        .await?
        .ok_or(Error::NotFound)?;

    let client = ClientDescriptor {
        ip,
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    };

    Ok(open(&state, &channel, query.output_profile, client)
        .await?
        .into_response())
}

/// Resolve a channel to a live connection.
///
/// Shared with the Xtream stream endpoints, which address the same channels
/// under a different URL shape and must behave identically once resolved.
pub async fn open(
    state: &AppState,
    channel: &EffectiveChannel,
    output_profile: Option<Id>,
    client: ClientDescriptor,
) -> Result<Response, Error> {
    let sources = sources_for(state, channel).await?;
    if sources.is_empty() {
        return Err(Error::invalid("channel has no streams assigned"));
    }

    // An output profile that does not exist falls back to no transcoding
    // rather than failing: a tuner Plex has already added must keep answering,
    // and the lineup snapshot pins it.
    let transcode = match output_profile {
        Some(id) => db::profiles::get_output_profile(&state.db, id)
            .await?
            .filter(|profile| profile.is_active && !profile.command.is_empty())
            .map(|profile| Transcode {
                command: profile.command,
                parameters: profile.parameters,
            }),
        None => None,
    };
    let output = match (&transcode, output_profile) {
        (Some(_), Some(id)) => OutputKey::Profile(id),
        _ => OutputKey::Raw,
    };

    let spec = ChannelSpec {
        channel: channel.uuid,
        sources,
        transcode,
    };

    match registry(state).await?.connect(output, spec, client) {
        Ok(Connection::Stream(body)) => Ok((
            [
                (header::CONTENT_TYPE, "video/mp2t"),
                // Nothing here is seekable and every byte is live, so a cache
                // anywhere in the path is a stall waiting to happen.
                (header::CACHE_CONTROL, "no-cache, no-store"),
            ],
            Body::from_stream(body),
        )
            .into_response()),
        Ok(Connection::Redirect { url }) => Ok(Redirect::temporary(&url).into_response()),
        Err(dollet_stream::StreamError::NoSources) => {
            Err(Error::invalid("channel has no streams assigned"))
        }
        Err(e) => {
            tracing::warn!(channel = %channel.uuid, error = %e, "stream refused");
            Err(Error::Upstream(e.to_string()))
        }
    }
}

/// Build the ordered source list, with each stream's profile, user agent and
/// account budget resolved.
///
/// Every fallback chain here is the same: the stream's own setting, then the
/// channel's, then the provider account's, then the global default.
pub async fn sources_for(
    state: &AppState,
    channel: &EffectiveChannel,
) -> Result<Vec<StreamSource>, Error> {
    let streams = db::streams::for_channel(&state.db, channel.id).await?;
    if streams.is_empty() {
        return Ok(Vec::new());
    }

    let settings: StreamSettings = settings::load(&state.db).await?;
    let redirect_id = db::profiles::redirect_profile_id(&state.db).await?;

    let profiles: HashMap<Id, _> = db::profiles::list_stream_profiles(&state.db)
        .await?
        .into_iter()
        .map(|profile| (profile.id, profile))
        .collect();
    let agents: HashMap<Id, String> = db::profiles::list_user_agents(&state.db)
        .await?
        .into_iter()
        .map(|agent| (agent.id, agent.user_agent))
        .collect();
    let accounts: HashMap<Id, _> = db::m3u::list_accounts(&state.db)
        .await?
        .into_iter()
        .map(|account| (account.id, account))
        .collect();

    let fallback_agent = super::user_agent_for(state, None).await?;

    Ok(streams
        .into_iter()
        .filter_map(|stream| {
            let url = stream.url.filter(|url| !url.is_empty())?;

            // The engine dials this exactly as written — it cannot depend on
            // `dollet-core`, by design — and the guarded resolver only runs for a
            // hostname. So a provider playlist carrying `http://127.0.0.1:9191/`
            // or `file:///etc/passwd` is judged here or nowhere. `allow_private`
            // stays on: a LAN tuner is a supported source.
            if let Err(e) = dollet_core::http::check_url(&url, true) {
                tracing::warn!(
                    stream = stream.id,
                    url = %super::ingest::redact(&url),
                    error = %e,
                    "refusing a stream URL in blocked address space"
                );
                return None;
            }

            let account = stream.m3u_account_id.and_then(|id| accounts.get(&id));

            let profile_id = stream
                .stream_profile_id
                .or(channel.stream_profile_id)
                .or_else(|| account.and_then(|a| a.stream_profile_id))
                .or(settings.default_stream_profile);
            let profile = profile_id.and_then(|id| profiles.get(&id));

            let user_agent = profile
                .and_then(|p| p.user_agent_id)
                .or_else(|| account.and_then(|a| a.user_agent_id))
                .or(settings.default_user_agent)
                .and_then(|id| agents.get(&id))
                .cloned()
                .unwrap_or_else(|| fallback_agent.clone());

            let source_profile = match profile {
                Some(p) if Some(p.id) == redirect_id => SourceProfile::Redirect,
                Some(p) if !p.command.is_empty() => SourceProfile::Command {
                    command: p.command.clone(),
                    parameters: p.parameters.clone(),
                },
                _ => SourceProfile::Proxy,
            };

            Some(StreamSource {
                id: stream.id,
                url,
                user_agent,
                profile: source_profile,
                // `max_streams` of 0 means "unlimited", and a limit of
                // zero would refuse every connection instead.
                limit: account.filter(|a| a.max_streams > 0).map(|a| SourceLimit {
                    key: a.id,
                    max_streams: a.max_streams,
                }),
            })
        })
        .collect())
}

/// Tear down every session for a channel, raw and transcodes alike.
///
/// Admin-only for the same reason `/proxy/stats/` is: the channel UUID in the
/// path plays that channel against the anonymous stream endpoint. Without this
/// a wedged stream can only be cleared by restarting the container.
async fn stop_channel(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(uuid): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let channel = db::channels::get_effective_by_uuid(&state.db, uuid)
        .await?
        .ok_or(Error::NotFound)?;

    let registry = registry(&state).await?;
    let stopped = registry
        .stats()
        .iter()
        .filter(|session| session.channel == uuid)
        .count();
    registry.stop_channel(uuid);

    db::events::record(
        &state.db,
        "channel_stop",
        Some(uuid),
        Some(&channel.name),
        &json!({ "reason": "stopped by an administrator", "sessions": stopped }),
    )
    .await?;

    Ok(Json(json!({ "stopped": stopped })))
}

#[derive(Deserialize)]
struct StopClient {
    client_id: String,
}

/// Disconnect one client of a channel.
///
/// Answers 404 for a client that is not here — a stale row on the Stats page —
/// rather than a silent success, and 409 for the transcode consumer behind an
/// output profile: evicting it would strand the encoder for the life of the
/// session, so the engine refuses and this says why.
async fn stop_client(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(uuid): Path<Uuid>,
    Query(query): Query<StopClient>,
) -> ApiResult<Json<Value>> {
    let registry = registry(&state).await?;

    // Scanned rather than looked up: the live client list is both the lookup
    // and the answer to "is this row still real", and a caller's id came from
    // a stats payload that may already be stale.
    let found = registry
        .stats()
        .into_iter()
        .filter(|session| session.channel == uuid)
        .flat_map(|session| session.clients)
        .find(|client| client.id.to_string() == query.client_id);

    let Some(client) = found else {
        return Err(Error::NotFound.into());
    };
    if client.internal {
        return Err(Error::Conflict(
            "that is the transcode reading this channel, not a viewer; stop the \
             channel instead"
                .into(),
        )
        .into());
    }

    if !registry.disconnect_client(uuid, client.id) {
        return Err(Error::NotFound.into());
    }

    db::events::record(
        &state.db,
        "client_disconnect",
        Some(uuid),
        None,
        &json!({ "reason": "disconnected by an administrator", "client": query.client_id }),
    )
    .await?;

    Ok(Json(json!({ "disconnected": true })))
}

#[derive(Deserialize, Default)]
struct SwitchQuery {
    /// Which session to move: the raw one by default, or a transcode's.
    output_profile: Option<Id>,
    /// Which stream to move to, by `stream.id` — the same id
    /// `/api/channels/channels/{id}/streams/` returns and `SessionStats`
    /// reports as `source_id`.
    ///
    /// Not an index. The engine's list comes from `sources_for`, which drops
    /// streams with no URL, so position *n* in the UI's menu is not position
    /// *n* in the session: for `[A(url), B(no url), C(url)]` an operator
    /// picking B gets C, and picking C gets an error. `stream.url` is nullable
    /// and the API accepts an explicit null, so that list is reachable. An id
    /// also survives the list being reordered or re-fetched between the menu
    /// being drawn and the click.
    source_id: Option<Id>,
}

/// Move a live session to the next source in the channel's failover order.
///
/// Admin-only, like the other session controls: the UUID in the path plays the
/// channel against the anonymous stream endpoint.
///
/// A manual switch is not a failure. The engine spends no retry, records no
/// error and leaves the failover budget alone, and this reports it the same
/// way — an operator who reaches for this because a source looks bad should not
/// find the channel closer to giving up than before they touched it.
async fn next_stream(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(uuid): Path<Uuid>,
    Query(query): Query<SwitchQuery>,
) -> ApiResult<Json<Value>> {
    let (channel, output) = switch_target(&state, uuid, &query).await?;
    let outcome = registry(&state).await?.next_source(uuid, output);
    switch_result(&state, uuid, &channel.name, "next", outcome).await
}

/// Move a live session to a specific source, by index into the channel's order.
async fn change_stream(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(uuid): Path<Uuid>,
    Query(query): Query<SwitchQuery>,
) -> ApiResult<Json<Value>> {
    let source_id = query
        .source_id
        .ok_or_else(|| Error::invalid("source_id is required: which stream to switch to"))?;

    let (channel, output) = switch_target(&state, uuid, &query).await?;

    // Resolved against the same list the session was built from, so the index
    // handed to the engine means what the caller meant. A stream the channel
    // no longer has, or one with no URL to play, is not in it.
    let sources = sources_for(&state, &channel).await?;
    let Some(index) = sources.iter().position(|source| source.id == source_id) else {
        return Err(Error::NotFound.into());
    };

    let outcome = registry(&state).await?.switch_source(uuid, output, index);
    switch_result(&state, uuid, &channel.name, "change", outcome).await
}

async fn switch_target(
    state: &AppState,
    uuid: Uuid,
    query: &SwitchQuery,
) -> Result<(EffectiveChannel, OutputKey), Error> {
    let channel = db::channels::get_effective_by_uuid(&state.db, uuid)
        .await?
        .ok_or(Error::NotFound)?;

    Ok((
        channel,
        match query.output_profile {
            Some(id) => OutputKey::Profile(id),
            None => OutputKey::Raw,
        },
    ))
}

async fn switch_result(
    state: &AppState,
    uuid: Uuid,
    name: &str,
    kind: &str,
    outcome: dollet_stream::SwitchOutcome,
) -> ApiResult<Json<Value>> {
    match outcome {
        dollet_stream::SwitchOutcome::Switched(index) => {
            // Recorded because this moved someone's stream mid-watch, and the
            // only other trace afterwards is a `switches` counter that an
            // automatic failover increments too.
            db::events::record(
                &state.db,
                "channel_switch",
                Some(uuid),
                Some(name),
                &json!({
                    "reason": "switched by an administrator",
                    "kind": kind,
                    "source_index": index,
                }),
            )
            .await?;

            Ok(Json(json!({ "switched": true, "source_index": index })))
        }
        // A session the Stats page still lists but the engine has since torn
        // down. 404 rather than a cheerful 200, for the same reason
        // `stop_client` answers 404 for a client that has gone.
        dollet_stream::SwitchOutcome::NoSession => Err(Error::NotFound.into()),
        // Zero sources means the session has no list of its own, which is what
        // an output profile is: it reads the raw session's ring, so the raw
        // session is where a source gets chosen.
        dollet_stream::SwitchOutcome::OutOfRange { sources: 0 } => Err(Error::Conflict(
            "that session is a transcode of this channel and follows whichever source \
             the channel is on; switch the channel itself"
                .into(),
        )
        .into()),
        dollet_stream::SwitchOutcome::OutOfRange { sources } => Err(Error::invalid(format!(
            "this channel has {sources} source(s), numbered 0 to {}",
            sources - 1
        ))
        .into()),
    }
}

/// Live session and client detail.
///
/// Admin-only: these payloads carry channel UUIDs usable against the anonymous
/// stream endpoint, the upstream URLs behind them, and client IP addresses.
///
/// A bare array, unlike every `/api/` list. This is a snapshot of what is
/// happening now rather than a window onto a larger set — there is no page two
/// of live sessions — and the identical payload is pushed on the `/ws` tick,
/// where wrapping it would mean the two transports disagreed about its shape.
async fn stats(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Vec<Value>>> {
    let sessions = registry(&state).await?.stats();
    Ok(Json(with_now_playing(&state, sessions).await?))
}

/// What is on each channel that currently has a session, attached to it.
///
/// The Stats page shows what is *streaming*; without this it cannot show what
/// is *on*, because the guide is joined to channels and a session is keyed by
/// UUID. One query for every live session rather than one per session: this
/// runs on the stats tick, every two seconds.
pub async fn with_now_playing(
    state: &AppState,
    sessions: Vec<dollet_stream::SessionStats>,
) -> Result<Vec<Value>, Error> {
    let now = chrono::Utc::now();
    let uuids: Vec<String> = sessions
        .iter()
        .map(|session| session.channel.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let resolved = db::epg::now_playing(&state.db, &uuids, now).await?;

    let mut out = Vec::with_capacity(sessions.len());
    for session in sessions {
        let uuid = session.channel.to_string();
        let mut value = serde_json::to_value(&session).map_err(|e| Error::Other(e.into()))?;
        let found = resolved.iter().find(|row| row.channel_uuid == uuid);

        // Always present, never null: the caller has to be able to tell "this
        // channel has no listings" from "the answer has not arrived", and a
        // missing key reads as the second.
        let playing = match found {
            None => json!({ "state": "unknown" }),
            Some(row) if row.epg_data_id.is_none() => json!({ "state": "unmapped" }),
            Some(row) => match &row.program {
                Some(program) => programme_json(program, now),
                // Dummy sources generate their listings per request, so there
                // are no stored rows to find and "nothing is on" would be
                // wrong — the generator always has an answer.
                None if row.dummy => dummy_programme_json(&row.channel_name, now),
                None => json!({ "state": "gap" }),
            },
        };

        if let Some(object) = value.as_object_mut() {
            object.insert("now_playing".into(), playing);

            // The one field here that is a provider's, not ours. An Xtream
            // stream URL carries the account password in a path segment, and
            // this payload goes to `GET /api/proxy/stats/`, to every
            // `channel_stats` frame, and onto the Stats page — while the
            // account endpoint two files away answers `has_password` so that
            // same secret cannot be read back at all.
            //
            // Redacted here rather than at either caller, because there are
            // two of them and a third would not know to do it.
            if let Some(url) = object.get("url").and_then(Value::as_str) {
                let redacted = json!(super::ingest::redact(url));
                object.insert("url".into(), redacted);
            }
        }
        out.push(value);
    }

    Ok(out)
}

fn programme_json(
    program: &dollet_core::domain::Program,
    now: chrono::DateTime<chrono::Utc>,
) -> Value {
    let total = (program.end_time - program.start_time).num_seconds().max(0);
    let elapsed = (now - program.start_time).num_seconds().clamp(0, total);

    json!({
        "state": "programme",
        "title": program.title,
        "sub_title": program.sub_title,
        "description": program.description,
        "start": program.start_time,
        "stop": program.end_time,
        // Precomputed so every client draws the same bar from the same clock.
        // A browser's own `Date.now()` can be minutes off the server's, which
        // on a half-hour programme is a visibly wrong position.
        "elapsed_seconds": elapsed,
        "remaining_seconds": total - elapsed,
        "duration_seconds": total,
    })
}

fn dummy_programme_json(channel_name: &str, now: chrono::DateTime<chrono::Utc>) -> Value {
    let generated = dollet_core::output::dummy_epg::generate(
        channel_name,
        now,
        &dollet_core::output::dummy_epg::DummyOptions {
            num_days: 1,
            max_programs: Some(1),
            export_lookback: Some(now),
            ..Default::default()
        },
    );

    match generated.first() {
        Some(block) => {
            let total = (block.end_time - block.start_time).num_seconds().max(0);
            let elapsed = (now - block.start_time).num_seconds().clamp(0, total);
            json!({
                "state": "programme",
                "generated": true,
                "title": block.title,
                "sub_title": Value::Null,
                "description": block.description,
                "start": block.start_time,
                "stop": block.end_time,
                "elapsed_seconds": elapsed,
                "remaining_seconds": total - elapsed,
                "duration_seconds": total,
            })
        }
        None => json!({ "state": "gap" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_ring_cap_matches_the_engines_own_derivation() {
        // 15 s at the 20 Mbps the engine sizes against. If the engine's
        // `RING_SIZING_BITRATE` changes, this is where the two drift apart.
        assert_eq!(
            stream_config(&ProxySettings::default()).ring_max_bytes,
            20_000_000 / 8 * 15
        );

        // A tighter configured cap is honoured, because that is the knob's
        // whole purpose on a memory-constrained box.
        let tight = ProxySettings {
            ring_max_bytes: 1024,
            ..ProxySettings::default()
        };
        assert_eq!(stream_config(&tight).ring_max_bytes, 1024);
    }

    #[test]
    fn the_configured_proxy_settings_reach_the_engine() {
        // Values nothing else in the tree uses, so each assertion fails if its
        // field is dropped, defaulted or crossed with a neighbour. Asserting
        // the defaults instead would pass for `channel_shutdown_delay` even if
        // the field never arrived, because its default is zero and so is
        // `Duration`'s.
        let config = stream_config(&ProxySettings {
            channel_shutdown_delay: 11,
            buffering_timeout: 22,
            new_client_behind_seconds: 33,
            channel_client_wait_period: 44,
            ..ProxySettings::default()
        });
        assert_eq!(config.channel_shutdown_delay, Duration::from_secs(11));
        assert_eq!(config.buffering_timeout, Duration::from_secs(22));
        assert_eq!(config.new_client_behind, Duration::from_secs(33));
        assert_eq!(config.channel_client_wait, Duration::from_secs(44));

        // And the shipped defaults arrive intact, which is what a fresh
        // install actually runs with.
        let config = stream_config(&ProxySettings::default());
        assert_eq!(config.channel_shutdown_delay, Duration::ZERO);
        assert_eq!(config.buffering_timeout, Duration::from_secs(15));
    }

    #[test]
    fn a_zero_second_ring_still_leaves_a_usable_window() {
        // Guards the `max(1)`: a zero duration would make the byte cap zero
        // too, and a ring that holds nothing serves nothing.
        let config = stream_config(&ProxySettings {
            ring_seconds: 0,
            ..ProxySettings::default()
        });
        assert!(config.ring_duration >= Duration::from_secs(1));
        assert!(config.ring_max_bytes > 0);
    }
}
