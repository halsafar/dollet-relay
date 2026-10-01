//! The Xtream Codes server API.
//!
//! Live actions only. VOD and series return empty collections rather than an
//! error, because an Xtream client treats a failed catalogue call as a broken
//! account and stops asking for live channels too.
//!
//! Credentials are a username plus `custom_properties.xc_password`, never the
//! account password: an Xtream URL carries them in the query string, where
//! they land in every proxy log between here and the client.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;
use dollet_core::Error;
use dollet_core::db;
use dollet_core::domain::{EffectiveChannel, Id, User};
use dollet_core::output::{TvgIdSource, dummy_epg, m3u, xc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::ClientAddr;
use super::error::ApiResult;
use super::network::{self, XC_API};
use super::origin::Origin;
use crate::AppState;

/// Shown in a client's account screen.
const ACCOUNT_MESSAGE: &str = "dollet-relay XC API";

/// Group shown for a channel belonging to none. An Xtream client rejects a
/// null `category_id`, so there has to be somewhere to put them.
const DEFAULT_GROUP_ID: Id = 1;

/// Stand-in `max_connections` when a provider profile declares unlimited
/// streams. Far higher than the HDHomeRun equivalent because an Xtream client
/// treats the number as a quota to display, not tuners to allocate.
const UNLIMITED_CONNECTIONS: u32 = 50;

/// Bad credentials answer with a 404 page rather than a 401, because a
/// client that gets a 401 prompts for a password it was never given.
const NOT_FOUND_HTML: &str = concat!(
    "<!doctype html>\n<html lang=\"en\">\n<head>\n  <title>Not Found</title>\n</head>\n",
    "<body>\n  <h1>Not Found</h1><p>The requested resource was not found on this server.</p>\n",
    "</body>\n</html>\n"
);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/player_api.php", get(player_api).post(player_api))
        .route("/panel_api.php", get(panel_api).post(panel_api))
        .route("/get.php", get(get_php))
        .route("/xmltv.php", get(xmltv_php))
        .route("/live/{username}/{password}/{stream_id}", get(live_stream))
        // The bare three-segment form. It is the last route matchit tries for
        // a three-segment path and it is *not* a wildcard, so it neither
        // shadows nor is shadowed by the SPA fallback — but that is a property
        // of the router, so `the_bare_xtream_route_does_not_shadow_the_spa`
        // asserts it rather than leaving it to inspection.
        .route("/{username}/{password}/{stream_id}", get(live_stream))
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        NOT_FOUND_HTML,
    )
        .into_response()
}

#[derive(Deserialize, Default)]
struct XcQuery {
    username: Option<String>,
    password: Option<String>,
    action: Option<String>,
    category_id: Option<Id>,
    stream_id: Option<Id>,
    limit: Option<usize>,
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// Resolve Xtream credentials to a user.
///
/// The password is `custom_properties.xc_password`, which is a separate secret
/// precisely because it travels in URLs; comparing against the login password
/// would put that in the same logs.
async fn authenticate(
    state: &AppState,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<Option<User>, Error> {
    let (Some(username), Some(password)) = (username, password) else {
        return Ok(None);
    };

    let Some(user) = db::users::by_username(&state.db, username).await? else {
        return Ok(None);
    };
    if !user.is_active {
        return Ok(None);
    }

    let expected = user
        .custom_properties
        .get("xc_password")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // Constant-time, because this is compared on every request an Xtream
    // client makes and the secret is short enough for timing to matter.
    Ok(dollet_core::auth::secret_eq(expected, password).then_some(user))
}

/// Channels the credential's owner may see, with artwork pointed at this
/// server's cache.
///
/// Every Xtream payload that carries a logo carries the cached form: there is
/// no `cachedlogos` parameter in this API, so the choice is not the client's.
/// The base is `artwork_base` rather than this request's origin for the reason
/// given there — a logo is rendered by a browser, not by whatever fetched the
/// catalogue.
async fn visible_channels(
    state: &AppState,
    user: &User,
    origin: &Origin,
) -> Result<Vec<EffectiveChannel>, Error> {
    let mut channels = channels_for(state, user).await?;
    let artwork = super::outputs::artwork_base(state, origin);
    super::outputs::use_cached_logos(state, &mut channels, &artwork).await?;
    Ok(channels)
}

/// A user pinned to channel profiles sees the first of them; with none, the
/// whole lineup.
async fn channels_for(state: &AppState, user: &User) -> Result<Vec<EffectiveChannel>, Error> {
    super::outputs::visible_channels(state, user.channel_profile_ids.first().copied()).await
}

/// The live catalogue, optionally narrowed to one category. Shared by
/// `get_live_streams` and `panel_api.php`, which must not disagree about what
/// the lineup is.
async fn live_streams(
    state: &AppState,
    user: &User,
    origin: &Origin,
    category_id: Option<Id>,
) -> Result<Vec<xc::LiveStream>, Error> {
    let channels = visible_channels(state, user, origin).await?;
    let filtered: Vec<EffectiveChannel> = match category_id {
        Some(id) => channels
            .into_iter()
            .filter(|channel| channel.channel_group_id.unwrap_or(DEFAULT_GROUP_ID) == id)
            .collect(),
        None => channels,
    };

    Ok(xc::live_streams(
        &filtered,
        &xc::LiveStreamOptions {
            default_group_id: DEFAULT_GROUP_ID,
            // Catch-up is out of scope for 1.0, so no channel may advertise an
            // archive it cannot serve.
            catchup_allowed: false,
        },
    ))
}

async fn account(state: &AppState, origin: &Origin, user: &User) -> Result<xc::AccountInfo, Error> {
    let active = super::stream::registry(state)
        .await
        .map(|registry| {
            registry
                .stats()
                .iter()
                .map(|session| session.clients.len() as u32)
                .sum()
        })
        .unwrap_or(0);

    let password = user
        .custom_properties
        .get("xc_password")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // A per-user limit wins; without one the answer is what the providers
    // allow, because "0" reads to a client as an expired account rather than
    // as "no personal cap".
    let max_connections = match u32::try_from(user.stream_limit).unwrap_or(0) {
        0 => db::m3u::tuner_count(&state.db, UNLIMITED_CONNECTIONS).await?,
        limit => limit,
    };

    Ok(xc::account_info(&xc::AccountOptions {
        username: &user.username,
        password,
        message: ACCOUNT_MESSAGE,
        host: &origin.host,
        // The advertised port, not the one off the `Host` header: read from
        // the header, a request without an explicit port would report "80"
        // while every URL in the same response carries :9191.
        port: &origin.port_str(),
        scheme: &origin.scheme,
        // Every client, not this user's: sessions carry no user attribution, so
        // a per-user figure would be a guess. Over-reporting costs a client
        // nothing; under-reporting tells it a slot is free when it is not.
        active_connections: active,
        max_connections,
        now: Utc::now(),
    }))
}

async fn player_api(
    State(state): State<AppState>,
    client: ClientAddr,
    origin: Origin,
    Query(query): Query<XcQuery>,
) -> ApiResult<Response> {
    network::allowed(&state, client.0, XC_API).await?;

    let Some(user) =
        authenticate(&state, query.username.as_deref(), query.password.as_deref()).await?
    else {
        return Ok(not_found());
    };

    let body = match query.action.as_deref() {
        Some("get_live_categories") => {
            let profile = user.channel_profile_ids.first().copied();
            let groups = db::channels::groups_in_lineup_order(&state.db, profile).await?;
            json!(xc::live_categories(&groups))
        }
        Some("get_live_streams") => {
            json!(live_streams(&state, &user, &origin, query.category_id).await?)
        }
        Some("get_vod_categories") => json!(xc::vod_categories()),
        Some("get_vod_streams") => json!(xc::vod_streams()),
        Some("get_series_categories") => json!(xc::series_categories()),
        Some("get_series") => json!(xc::series()),
        Some("get_short_epg") => json!(listings(&state, &user, &query, true).await?),
        Some("get_simple_data_table") => json!(listings(&state, &user, &query, false).await?),
        // Any unrecognised action, and the credential check itself, answer
        // with the account block, which is what a client probing for
        // capabilities expects.
        _ => json!(account(&state, &origin, &user).await?),
    };

    Ok(Json(body).into_response())
}

/// `panel_api.php` is the account block plus the whole catalogue in one
/// response — categories and every channel, keyed by stream id.
///
/// Panel clients call this once instead of `get_live_categories` followed by
/// `get_live_streams`, and read the channel list from nowhere else; answering
/// with only the account block leaves them showing an empty lineup and no
/// error.
async fn panel_api(
    State(state): State<AppState>,
    client: ClientAddr,
    origin: Origin,
    Query(query): Query<XcQuery>,
) -> ApiResult<Response> {
    network::allowed(&state, client.0, XC_API).await?;

    let Some(user) =
        authenticate(&state, query.username.as_deref(), query.password.as_deref()).await?
    else {
        return Ok(not_found());
    };

    let mut body = json!(account(&state, &origin, &user).await?);
    let profile = user.channel_profile_ids.first().copied();
    let groups = db::channels::groups_in_lineup_order(&state.db, profile).await?;
    let channels = live_streams(&state, &user, &origin, query.category_id).await?;

    let map = channels
        .into_iter()
        .map(|channel| (channel.stream_id.to_string(), json!(channel)))
        .collect::<serde_json::Map<String, Value>>();

    let object = body.as_object_mut().expect("account info is an object");
    object.insert(
        "categories".into(),
        json!({
            "series": xc::series_categories(),
            "movie": xc::vod_categories(),
            "live": xc::live_categories(&groups),
        }),
    );
    object.insert("available_channels".into(), Value::Object(map));

    Ok(Json(body).into_response())
}

async fn listings(
    state: &AppState,
    user: &User,
    query: &XcQuery,
    short: bool,
) -> Result<xc::EpgListings, Error> {
    // No artwork in this payload, so the un-rewritten list is what it needs.
    let channels = channels_for(state, user).await?;
    let numbers = xc::assign_channel_numbers(&channels.iter().collect::<Vec<_>>());

    let Some(stream_id) = query.stream_id else {
        return Ok(xc::EpgListings {
            epg_listings: Vec::new(),
        });
    };
    let Some(channel) = channels.iter().find(|channel| channel.id == stream_id) else {
        return Ok(xc::EpgListings {
            epg_listings: Vec::new(),
        });
    };

    let now = Utc::now();
    let ctx = xc::EpgContext {
        stream_id: channel.id,
        channel_number: numbers.get(&channel.id).copied().unwrap_or(0),
        epg_data_id: channel.epg_data_id,
        now,
        archive_window: None,
        short,
    };

    let Some(epg_data_id) = channel.epg_data_id else {
        return Ok(xc::EpgListings {
            epg_listings: Vec::new(),
        });
    };

    let dummy = db::epg::get_data(&state.db, epg_data_id)
        .await?
        .and_then(|data| data.epg_source_id);
    let is_dummy = match dummy {
        Some(source) => db::epg::get_source(&state.db, source)
            .await?
            .is_some_and(|s| s.source_type == dollet_core::domain::EpgSourceType::Dummy),
        None => false,
    };

    if is_dummy {
        let programs = dummy_epg::generate(
            &channel.name,
            now,
            &dummy_epg::DummyOptions {
                max_programs: query.limit.or(Some(4)),
                ..Default::default()
            },
        );
        return Ok(xc::dummy_epg_listings(&programs, &ctx));
    }

    let mut programs =
        db::epg::programs(&state.db, epg_data_id, now, now + chrono::Duration::days(7)).await?;
    programs.truncate(query.limit.unwrap_or(4));
    Ok(xc::epg_listings(&programs, &ctx))
}

async fn get_php(
    State(state): State<AppState>,
    client: ClientAddr,
    origin: Origin,
    Query(query): Query<XcQuery>,
) -> ApiResult<Response> {
    network::allowed(&state, client.0, XC_API).await?;

    let Some(user) =
        authenticate(&state, query.username.as_deref(), query.password.as_deref()).await?
    else {
        return Ok(not_found());
    };

    // `type` is accepted and ignored: every value an Xtream client sends means
    // the same playlist, and refusing an unknown one breaks clients for no gain.
    let _ = &query.kind;

    let channels = visible_channels(&state, &user, &origin).await?;
    let entries: Vec<m3u::M3uChannel<'_>> = channels.iter().map(m3u::M3uChannel::proxied).collect();

    let password = user
        .custom_properties
        .get("xc_password")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let body = m3u::render(
        &m3u::M3uOptions {
            base_url: &origin.base_url(),
            epg_url: &format!(
                "{}/xmltv.php?username={}&password={}",
                origin.base_url(),
                super::urlencode(&user.username),
                super::urlencode(password)
            ),
            tvg_id_source: TvgIdSource::default(),
            stream_query: &[],
            xtream_credentials: Some((&user.username, password)),
        },
        &entries,
    );

    Ok(([(header::CONTENT_TYPE, "audio/x-mpegurl")], body).into_response())
}

async fn xmltv_php(
    State(state): State<AppState>,
    client: ClientAddr,
    origin: Origin,
    Query(query): Query<XcQuery>,
) -> ApiResult<Response> {
    network::allowed(&state, client.0, XC_API).await?;

    let Some(user) =
        authenticate(&state, query.username.as_deref(), query.password.as_deref()).await?
    else {
        return Ok(not_found());
    };

    // Keyed by the id `get_live_streams` reported, not by the channel's own
    // `tvg_id`. An Xtream client joins this guide to that catalogue on
    // `epg_channel_id`, so any channel where the two disagree shows no
    // listings — which is every channel whose number is fractional or absent,
    // because those are exactly the ones the Xtream numbering had to reassign.
    let mut channels = visible_channels(&state, &user, &origin).await?;
    let ids: HashMap<Id, String> = live_streams(&state, &user, &origin, None)
        .await?
        .into_iter()
        .map(|stream| (stream.stream_id, stream.epg_channel_id))
        .collect();
    for channel in &mut channels {
        channel.tvg_id = ids.get(&channel.id).cloned();
    }

    // `TvgId` rather than the default, because the ids above were just written
    // into that field; the default source ignores it and derives an id from the
    // channel number, which is the disagreement this is fixing.
    let body = super::outputs::guide_bytes(&state, &channels, TvgIdSource::TvgId, None).await?;

    Ok(([(header::CONTENT_TYPE, "application/xml")], body).into_response())
}

async fn live_stream(
    State(state): State<AppState>,
    client: ClientAddr,
    Path((username, password, stream_id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Response> {
    network::allowed(&state, client.0, network::STREAMS).await?;

    let Some(user) = authenticate(&state, Some(&username), Some(&password)).await? else {
        return Ok(not_found());
    };

    // Clients append a container extension: `1234.ts`.
    let id: Id = match stream_id.split('.').next().and_then(|id| id.parse().ok()) {
        Some(id) => id,
        None => return Ok(not_found()),
    };

    let Some(channel) = channels_for(&state, &user)
        .await?
        .into_iter()
        .find(|channel| channel.id == id)
    else {
        return Ok(not_found());
    };

    // No output profile: an Xtream URL carries no way to ask for one, and
    // applying a transcode a client did not request would surprise it.
    Ok(super::stream::open(
        &state,
        &channel,
        None,
        dollet_stream::ClientDescriptor {
            ip: client.0,
            user_agent: headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        },
    )
    .await?)
}
