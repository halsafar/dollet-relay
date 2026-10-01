//! `/api/epg/` — guide sources, their data, and manual refresh.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use dollet_core::db::jobs;
use dollet_core::domain::{EpgSource, EpgSourceType, Id, Program};
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::ApiResult;
use super::ingest;
use super::outputs::{self, Output};
use super::whole_list;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sources/", get(list_sources).post(create_source))
        .route(
            "/sources/{id}/",
            get(get_source)
                .patch(update_source)
                .put(update_source)
                .delete(delete_source),
        )
        .route("/grid/", get(grid))
        .route("/ambiguous/", get(list_ambiguous))
        .route("/epgdata/", get(list_data))
        .route(
            "/suggestions/{channel_id}/",
            axum::routing::post(accept_suggestion).delete(dismiss_suggestion),
        )
        .route("/match/", post(match_channels))
        // `/refresh/` and `/refresh/{id}/`, matching `/api/m3u/`; "import"
        // means the one-shot migration in this codebase.
        .route("/refresh/", post(refresh_all))
        .route("/refresh/{id}/", post(refresh_source))
}

fn serialize(source: &EpgSource, job: Option<&jobs::Job>) -> Value {
    let base = json!({
        "id": source.id,
        "name": source.name,
        "source_type": match source.source_type {
            EpgSourceType::Dummy => "dummy",
            EpgSourceType::Xmltv => "xmltv",
        },
        "url": source.url,
        "file_path": source.file_path,
        "username": source.username,
        // Never echoed back: it reaches this server once and only ever leaves
        // it towards the provider.
        "has_password": source.password.is_some(),
        "is_active": source.is_active,
        "priority": source.priority,
        "refresh_interval_hours": source.refresh_interval_hours,
        "custom_properties": source.custom_properties,
    });
    super::with_job_status(base, job)
}

async fn job_for(state: &AppState, source_id: Id) -> Result<Option<jobs::Job>, Error> {
    jobs::by_key(&state.db, &ingest::epg::job_key(source_id)).await
}

async fn list_sources(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let sources = db::epg::list_sources(&state.db).await?;
    let mut out = Vec::with_capacity(sources.len());
    for source in &sources {
        out.push(serialize(
            source,
            job_for(&state, source.id).await?.as_ref(),
        ));
    }
    Ok(Json(whole_list(out)))
}

async fn get_source(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let source = db::epg::get_source(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(serialize(
        &source,
        job_for(&state, id).await?.as_ref(),
    )))
}

#[derive(Deserialize, Default)]
struct SourceBody {
    name: Option<String>,
    source_type: Option<String>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    url: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    file_path: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    username: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    password: Option<Option<String>>,
    is_active: Option<bool>,
    priority: Option<u32>,
    /// Hours, and the name says so. `refresh_interval` is accepted as an alias
    /// because that is what this API took before 1.0.
    #[serde(alias = "refresh_interval")]
    refresh_interval_hours: Option<u32>,
    custom_properties: Option<Value>,
}

impl SourceBody {
    fn apply(self, source: &mut EpgSource) -> Result<(), Error> {
        if let Some(name) = self.name {
            source.name = name;
        }
        if let Some(kind) = self.source_type {
            // Strict, for the same reason the account type is: falling
            // through to `Xmltv` would turn a typo into a source that fetches
            // a URL a dummy source does not have.
            source.source_type = db::epg::source_type(&kind)?;
        }
        if let Some(url) = self.url {
            source.url = url;
        }
        if let Some(path) = self.file_path {
            source.file_path = path;
        }
        if let Some(username) = self.username {
            source.username = username;
        }
        if let Some(password) = self.password {
            source.password = password;
        }
        if let Some(active) = self.is_active {
            source.is_active = active;
        }
        if let Some(priority) = self.priority {
            source.priority = priority;
        }
        if let Some(interval) = self.refresh_interval_hours {
            // Clamped rather than trusted: the value is turned into a
            // `chrono::Duration` added to the current instant, which panics on
            // overflow. A year is longer than any refresh anyone schedules and
            // nowhere near the range where that happens.
            source.refresh_interval_hours = interval.clamp(0, super::MAX_REFRESH_HOURS);
        }
        if let Some(properties) = self.custom_properties {
            source.custom_properties = properties;
        }
        Ok(())
    }
}

async fn create_source(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<SourceBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut source = EpgSource {
        id: 0,
        name: String::new(),
        source_type: EpgSourceType::Xmltv,
        url: None,
        file_path: None,
        username: None,
        password: None,
        is_active: true,
        priority: 0,
        refresh_interval_hours: 24,
        custom_properties: json!({}),
    };
    body.apply(&mut source)?;

    if source.name.trim().is_empty() {
        return Err(Error::invalid("name is required").into());
    }

    let created = db::epg::create_source(&state.db, &source).await?;
    // Registered immediately: a source added at 09:00 with a 24-hour interval
    // should refresh tomorrow morning, not on the next restart.
    super::jobs::sync_schedule(&state).await?;

    Ok((
        StatusCode::CREATED,
        Json(serialize(
            &created,
            job_for(&state, created.id).await?.as_ref(),
        )),
    ))
}

async fn update_source(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<SourceBody>,
) -> ApiResult<Json<Value>> {
    let mut source = db::epg::get_source(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    body.apply(&mut source)?;

    let saved = db::epg::save_source(&state.db, &source).await?;
    super::jobs::sync_schedule(&state).await?;
    // A source that becomes `dummy` generates its listings from this request
    // onwards, with no refresh in between — the channels on it go from whatever
    // was stored to a generated schedule.
    outputs::invalidate(&state, &[Output::Guide]).await;
    Ok(Json(serialize(&saved, job_for(&state, id).await?.as_ref())))
}

async fn delete_source(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::epg::delete_source(&state.db, id).await?;
    super::jobs::sync_schedule(&state).await?;
    // Its guide data goes with it, and every channel mapped to that data loses
    // its listings on the spot.
    outputs::invalidate(&state, &[Output::Guide]).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn refresh_source(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    db::epg::get_source(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(
        super::jobs::start_now(&state, &ingest::epg::job_key(id)).await?,
    ))
}

async fn refresh_all(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let keys = db::epg::list_sources(&state.db)
        .await?
        .into_iter()
        .map(|source| ingest::epg::job_key(source.id));
    let started = super::jobs::start_each(&state, keys).await?;
    Ok(Json(json!({ "started": started })))
}

#[derive(Deserialize, Default)]
struct MatchQuery {
    epg_source: Option<Id>,
}

/// Assign guide data to channels that have none, on demand.
///
/// Deliberately a button and not a step of every refresh: it writes
/// `epg_data_id` across the user's catalogue, and a fuzzy score getting one
/// wrong is harder to spot than a channel with no guide. Channels the user
/// mapped by hand are never touched either way.
async fn match_channels(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<MatchQuery>,
) -> ApiResult<Json<Value>> {
    let sources = match query.epg_source {
        Some(id) => vec![
            db::epg::get_source(&state.db, id)
                .await?
                .ok_or(Error::NotFound)?,
        ],
        None => db::epg::list_sources(&state.db).await?,
    };

    let mut matched = 0usize;
    let mut ambiguous = 0usize;
    for source in sources {
        let report = ingest::epg::auto_match(&state, source.id).await?;
        matched += report.matched;
        ambiguous += report.ambiguous.len();
    }

    // Each match is an `epg_data_id` written to a channel. A run that only
    // raised questions wrote nothing an output reads.
    if matched > 0 {
        outputs::invalidate(&state, &[Output::Guide]).await;
    }

    Ok(Json(json!({
        "matched": matched,
        "need_a_decision": ambiguous,
    })))
}

#[derive(Deserialize, Default)]
struct DataQuery {
    epg_source: Option<Id>,
    search: Option<String>,
    limit: Option<i64>,
}

/// Guide channels, for the "assign EPG" picker on the Channels page.
async fn list_data(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<DataQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(200);
    let search = query.search.as_deref();
    let count = db::epg::count_data(&state.db, query.epg_source, search).await?;
    let rows = db::epg::list_data(&state.db, query.epg_source, search, limit).await?;
    Ok(Json(super::capped_list(limit, count, rows)))
}

// ------------------------------------------------------------------- grid

/// The window the TV Guide opens on: the last hour so a
/// programme already running is visible, and a day ahead.
const DEFAULT_LOOKBACK_HOURS: i64 = 1;
const DEFAULT_WINDOW_HOURS: i64 = 24;

/// The window is client-supplied, so it is bounded. A request for a decade is
/// not a guide, and answering it would scan the whole programme table.
const MAX_WINDOW_DAYS: i64 = 14;

#[derive(Deserialize, Default)]
struct GridQuery {
    channel_profile: Option<Id>,
    from: Option<chrono::DateTime<chrono::Utc>>,
    to: Option<chrono::DateTime<chrono::Utc>>,
}

/// Every channel on screen and the programmes overlapping the window, in one
/// response.
///
/// Shaped by channel rather than as a flat programme list: that shape makes
/// the browser join thousands of programmes onto dozens of channels by
/// `tvg_id`. Channels come
/// from `effective_channel`, so overrides and ordering match the lineup Plex
/// sees — a guide ordered differently from the lineup is its own confusion.
async fn grid(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<GridQuery>,
) -> ApiResult<Json<Value>> {
    let now = chrono::Utc::now();
    let from = query
        .from
        .unwrap_or_else(|| now - chrono::Duration::hours(DEFAULT_LOOKBACK_HOURS));

    // `from` is client-supplied, and `DateTime + TimeDelta` panics rather than
    // saturating once the sum leaves the representable range. A date far enough
    // out is a bad request, not a dead connection.
    let ceiling = from
        .checked_add_signed(chrono::Duration::days(MAX_WINDOW_DAYS))
        .ok_or_else(|| Error::invalid("`from` is too far in the future"))?;

    // Unchecked past that: a day is well inside the fourteen just added.
    let to = query
        .to
        .unwrap_or(from + chrono::Duration::hours(DEFAULT_WINDOW_HOURS))
        .min(ceiling);

    if to <= from {
        return Err(Error::invalid("`to` must be after `from`").into());
    }

    let channels = db::channels::list_effective(
        &state.db,
        &db::channels::ChannelFilter {
            profile_id: query.channel_profile,
            visible_only: true,
            ..Default::default()
        },
        None,
        None,
    )
    .await?
    .results;

    let epg_ids: Vec<Id> = channels.iter().filter_map(|c| c.epg_data_id).collect();
    let mut by_epg_data: std::collections::HashMap<Id, Vec<Value>> =
        std::collections::HashMap::new();
    for program in db::epg::programs_for(&state.db, &epg_ids, from, to).await? {
        by_epg_data
            .entry(program.epg_data_id)
            .or_default()
            .push(serialize_program(&program));
    }

    let dummy_sources = super::dummy_source_ids(&state).await?;
    let suggestions: std::collections::HashMap<Id, db::epg::MatchSuggestion> =
        db::epg::suggestions(&state.db)
            .await?
            .into_iter()
            .map(|suggestion| (suggestion.channel_id, suggestion))
            .collect();
    // Only the candidates a row on this screen will name. Reading the whole
    // `epg_data` table to resolve a few dozen names is thousands of rows per
    // guide request on any full-size guide.
    let suggested: Vec<Id> = channels
        .iter()
        .filter_map(|channel| suggestions.get(&channel.id))
        .map(|suggestion| suggestion.epg_data_id)
        .collect();
    let guide_names = db::epg::names_for(&state.db, &suggested).await?;

    let mut rows = Vec::with_capacity(channels.len());
    for channel in &channels {
        // A channel on a dummy source has no stored programmes at all; its
        // guide is generated, and without this its row is permanently empty.
        let programs = match channel.epg_data_id {
            Some(id) if super::is_dummy(&state, id, &dummy_sources).await? => {
                dummy_programs(&channel.name, now, from, to)
            }
            Some(id) => by_epg_data.remove(&id).unwrap_or_default(),
            None => Vec::new(),
        };

        rows.push(json!({
            "id": channel.id,
            "uuid": channel.uuid,
            "name": channel.name,
            "channel_number": channel.channel_number,
            "logo_url": channel.logo_url,
            "group_name": channel.group_name,
            "epg_data_id": channel.epg_data_id,
            // A row with an empty strip says "this channel has no guide"; a
            // missing row looks like the channel is gone.
            "programs": programs,
            // The third outcome `sync::epg` reports: scored in the ambiguous
            // band, which a human breaks — once.
            "epg_suggestion": suggestions.get(&channel.id).map(|suggestion| json!({
                "epg_data_id": suggestion.epg_data_id,
                "name": guide_names.get(&suggestion.epg_data_id),
                "score": suggestion.score,
            })),
        }));
    }

    Ok(Json(json!({
        "start": from,
        "end": to,
        "channels": rows,
    })))
}

fn serialize_program(program: &Program) -> Value {
    let properties = &program.custom_properties;
    let flag = |key: &str| {
        properties
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };

    json!({
        "id": program.id,
        "start_time": program.start_time,
        "end_time": program.end_time,
        "title": program.title,
        "sub_title": program.sub_title,
        "description": program.description,
        "season": properties.get("season"),
        "episode": properties.get("episode"),
        "is_new": flag("new"),
        "is_live": flag("live"),
        "is_premiere": flag("premiere"),
    })
}

fn dummy_programs(
    name: &str,
    now: chrono::DateTime<chrono::Utc>,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> Vec<Value> {
    dollet_core::output::dummy_epg::generate(
        name,
        now,
        &dollet_core::output::dummy_epg::DummyOptions {
            num_days: ((to - from).num_days() + 1).clamp(1, MAX_WINDOW_DAYS) as u32,
            export_lookback: Some(from),
            export_cutoff: Some(to),
            ..Default::default()
        },
    )
    .iter()
    .map(|program| {
        json!({
            // Generated, so there is no row id; stable within the hour because
            // `generate` truncates `now`, which keeps a re-fetch from shifting
            // every block on screen.
            "id": format!("dummy-{}", program.start_time.timestamp()),
            "start_time": program.start_time,
            "end_time": program.end_time,
            "title": program.title,
            "sub_title": Value::Null,
            "description": program.description,
            // Present and null, not absent. Every other programme in the same
            // array carries these, and the shape harness unions keys across
            // elements, so an omission here is invisible to it and lands on
            // whatever reads the grid instead.
            "season": Value::Null,
            "episode": Value::Null,
            "is_new": false,
            "is_live": false,
            "is_premiere": false,
        })
    })
    .collect()
}

/// Everything waiting on a guide decision, as a list.
///
/// The per-channel suggestion in the grid answers "what is the candidate for
/// the channel I am looking at". This answers "which channels need me", which
/// is the question the refresh's own "3 need a decision" raises and cannot
/// otherwise resolve — a count tells the user something is wrong without
/// telling them what, and that is precisely the failure the three-outcome
/// model exists to avoid.
///
/// Read-only on purpose: accepting one is a `PATCH` of the channel's
/// `epg_data_id`, which the Channels editor already does.
async fn list_ambiguous(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<Json<Value>> {
    let suggestions = db::epg::suggestions(&state.db).await?;
    if suggestions.is_empty() {
        return Ok(Json(whole_list(Vec::<Value>::new())));
    }

    let channels: std::collections::HashMap<Id, String> = db::channels::list_effective(
        &state.db,
        &db::channels::ChannelFilter::default(),
        None,
        None,
    )
    .await?
    .results
    .into_iter()
    .map(|channel| (channel.id, channel.name))
    .collect();

    // Only the rows these suggestions point at. `list_data` clamps its limit
    // to ten thousand rows ordered by name, so a larger guide would answer
    // with a window that need not contain them.
    let suggested: Vec<Id> = suggestions.iter().map(|s| s.epg_data_id).collect();
    let guide = db::epg::data_for(&state.db, &suggested).await?;

    let rows: Vec<Value> = suggestions
        .iter()
        // A suggestion whose channel is gone is not actionable, and the
        // cascade that removes it has not necessarily run yet.
        .filter_map(|suggestion| {
            let candidate = guide.get(&suggestion.epg_data_id)?;
            Some(json!({
                "channel_id": suggestion.channel_id,
                "channel_name": channels.get(&suggestion.channel_id)?,
                "epg_data_id": suggestion.epg_data_id,
                "candidate_name": candidate.name,
                "candidate_tvg_id": candidate.tvg_id,
                "score": suggestion.score,
            }))
        })
        .collect();

    Ok(Json(whole_list(rows)))
}

/// Take the suggested match. One decision, and the question goes away.
async fn accept_suggestion(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(channel_id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let suggestion = db::epg::get_suggestion(&state.db, channel_id)
        .await?
        .ok_or(Error::NotFound)?;

    let mut channel = db::channels::get(&state.db, channel_id)
        .await?
        .ok_or(Error::NotFound)?;
    channel.epg_data_id = Some(suggestion.epg_data_id);
    db::channels::save(&state.db, &channel).await?;
    db::epg::clear_suggestion(&state.db, channel_id).await?;
    // The channel's listings, which is the whole reason the decision was asked
    // for. Dismissing one changes nothing any output renders.
    outputs::invalidate(&state, &[Output::Guide]).await;

    Ok(Json(json!({ "epg_data_id": suggestion.epg_data_id })))
}

/// Reject it. The channel keeps whatever it had, and a later refresh may ask
/// again with better guide data — which is the right behaviour, since the
/// answer depends on what the provider published.
async fn dismiss_suggestion(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(channel_id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::epg::get_suggestion(&state.db, channel_id)
        .await?
        .ok_or(Error::NotFound)?;
    db::epg::clear_suggestion(&state.db, channel_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
