//! Channels, channel groups, and channel profiles.
//!
//! A channel is returned with three views of itself: the base row provider
//! sync writes, the override row the user writes, and the coalesced
//! `effective_*` values every output reads. The UI needs all three to show
//! "inherited" versus "overridden" without guessing.

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use dollet_core::auth::{level_from_i64, level_value};
use dollet_core::db::channels::{ChannelFilter, RenumberOrder};
use dollet_core::domain::{Channel, ChannelOverride, ChannelProfile, Id};
use dollet_core::settings::{self, NumberingSettings};
use dollet_core::sync::channels as sync_channels;
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::{ApiError, ApiResult};
use super::outputs::{self, Output};
use super::{listing, paging, whole_list};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/groups/", get(list_groups).post(create_group))
        .route(
            "/groups/{id}/",
            get(get_group)
                .patch(update_group)
                .put(update_group)
                .delete(delete_group),
        )
        .route("/groups/plan-ranges/", post(plan_ranges))
        .route("/groups/assign-ranges/", post(assign_ranges))
        .route("/groups/plan-renumber/", post(plan_renumber_all))
        .route("/groups/renumber-all/", post(renumber_all))
        .route("/groups/{id}/renumber/", post(renumber_group))
        .route("/channels/", get(list).post(create))
        .route("/channels/bulk-delete/", post(bulk_delete))
        .route(
            "/channels/{id}/",
            get(get_channel)
                .patch(update)
                .put(update)
                .delete(delete_channel),
        )
        .route(
            "/channels/{id}/streams/",
            get(channel_streams).put(set_channel_streams),
        )
        .route("/channels/{id}/move/", post(move_channel))
        .route("/profiles/", get(list_profiles).post(create_profile))
        .route(
            "/profiles/{id}/",
            get(get_profile)
                .patch(update_profile)
                .put(update_profile)
                .delete(delete_profile),
        )
        .route(
            "/profiles/{profile_id}/channels/bulk-update/",
            post(bulk_membership),
        )
        .route(
            "/profiles/{profile_id}/channels/{channel_id}/",
            axum::routing::patch(set_membership),
        )
}

// ------------------------------------------------------------------ groups

/// One group as the Groups page shows it: its range, how many channels and
/// streams it holds, and its standing with each provider.
fn group_json(
    group: &dollet_core::domain::ChannelGroup,
    usage: (i64, i64),
    links: &[dollet_core::db::m3u::GroupAccountLink],
) -> Value {
    json!({
        "id": group.id,
        "name": group.name,
        "number_start": group.number_start,
        "number_end": group.number_end,
        "channel_count": usage.0,
        "stream_count": usage.1,
        "links": links
            .iter()
            .filter(|link| link.channel_group_id == group.id)
            .map(|link| json!({
                "m3u_account_id": link.m3u_account_id,
                "enabled": link.enabled,
                "auto_channel_sync": link.auto_channel_sync,
                "numbering_mode": super::m3u::numbering_mode(link),
            }))
            .collect::<Vec<_>>(),
    })
}

async fn list_groups(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let groups = db::channels::list_groups(&state.db).await?;
    let usage: HashMap<Id, (i64, i64)> = db::channels::group_usage(&state.db)
        .await?
        .into_iter()
        .map(|(id, channels, streams)| (id, (channels, streams)))
        .collect();
    let links = db::m3u::list_group_links(&state.db, None).await?;

    Ok(Json(whole_list(
        groups
            .iter()
            .map(|group| {
                group_json(
                    group,
                    usage.get(&group.id).copied().unwrap_or((0, 0)),
                    &links,
                )
            })
            .collect(),
    )))
}

async fn one_group(state: &AppState, id: Id) -> Result<Value, Error> {
    let group = db::channels::get_group(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    let usage = db::channels::group_usage(&state.db)
        .await?
        .into_iter()
        .find(|(group_id, ..)| *group_id == id)
        .map_or((0, 0), |(_, channels, streams)| (channels, streams));
    let links = db::m3u::list_group_links(&state.db, None).await?;
    Ok(group_json(&group, usage, &links))
}

/// The range fields distinguish absent from `null`, like `OverrideBody`: an
/// absent key leaves the range alone, an explicit `null` takes it off.
#[derive(Deserialize, Default)]
struct GroupBody {
    name: Option<String>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    number_start: Option<Option<f64>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    number_end: Option<Option<f64>>,
}

/// A range is a non-negative start and, if bounded, an end at or above it.
/// Anything else would make every allocation in the group fail on the first
/// refresh with no sign of why.
fn check_range(start: Option<f64>, end: Option<f64>) -> Result<(), ApiError> {
    if start.is_some_and(|start| !start.is_finite() || start < 0.0) {
        return Err(ApiError::invalid_fields(
            "the range start must be a number at or above 0",
            [("number_start", "must be at or above 0")],
        ));
    }
    if end.is_some_and(|end| !end.is_finite() || end < start.unwrap_or(0.0)) {
        return Err(ApiError::invalid_fields(
            "the range end must be at or above its start",
            [("number_end", "must be at or above the start")],
        ));
    }
    Ok(())
}

async fn get_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    Ok(Json(one_group(&state, id).await?))
}

async fn create_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<GroupBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = body.name.as_deref().map(str::trim).unwrap_or_default();
    if name.is_empty() {
        return Err(Error::invalid("name is required").into());
    }
    let range = (body.number_start.flatten(), body.number_end.flatten());
    check_range(range.0, range.1)?;

    let group = db::channels::create_group(&state.db, name, range).await?;
    Ok((
        StatusCode::CREATED,
        Json(one_group(&state, group.id).await?),
    ))
}

async fn update_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<GroupBody>,
) -> ApiResult<Json<Value>> {
    let mut group = db::channels::get_group(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    let renamed = match body.name.as_deref().map(str::trim) {
        Some("") => return Err(Error::invalid("name is required").into()),
        Some(name) if name != group.name => {
            group.name = name.to_owned();
            true
        }
        _ => false,
    };
    if let Some(start) = body.number_start {
        group.number_start = start;
    }
    if let Some(end) = body.number_end {
        group.number_end = end;
    }
    check_range(group.number_start, group.number_end)?;

    db::channels::save_group(&state.db, &group).await?;
    // `group-title` on every entry the group holds. The guide carries no group
    // at all, so it is untouched — and a range change alone touches nothing
    // rendered, since it decides only where the *next* channel goes.
    if renamed {
        outputs::invalidate(&state, &[Output::Playlist]).await;
    }
    Ok(Json(one_group(&state, id).await?))
}

async fn delete_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::channels::delete_group(&state.db, id).await?;
    // Its channels keep their entries and fall back to the default group name.
    outputs::invalidate(&state, &[Output::Playlist]).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- channels

#[derive(Deserialize, Default)]
struct ListQuery {
    search: Option<String>,
    ordering: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
    /// `true` asks for the whole list. One gesture, every list endpoint.
    all: Option<String>,
    channel_group: Option<Id>,
    channel_profile: Option<Id>,
}

impl ListQuery {
    fn all(&self) -> bool {
        super::wants_all(self.all.as_deref())
    }
}

fn serialize(
    effective: &dollet_core::domain::EffectiveChannel,
    base: Option<&Channel>,
    overridden: Option<&ChannelOverride>,
    streams: &[Id],
    epg_name: Option<&str>,
) -> Value {
    let base_value = |pick: fn(&Channel) -> Value| base.map(pick).unwrap_or(Value::Null);

    json!({
        "id": effective.id,
        "uuid": effective.uuid,
        "name": base_value(|c| json!(c.name)),
        "channel_number": base_value(|c| json!(c.channel_number)),
        "channel_group_id": base_value(|c| json!(c.channel_group_id)),
        "logo_id": base_value(|c| json!(c.logo_id)),
        "tvg_id": base_value(|c| json!(c.tvg_id)),
        "tvc_guide_stationid": base_value(|c| json!(c.tvc_guide_stationid)),
        "epg_data_id": base_value(|c| json!(c.epg_data_id)),
        "stream_profile_id": base_value(|c| json!(c.stream_profile_id)),
        "user_level": level_value(effective.user_level),
        "is_adult": effective.is_adult,
        "is_catchup": effective.is_catchup,
        "catchup_days": effective.catchup_days,
        "hidden_from_output": effective.hidden_from_output,
        "auto_created": base.map(|c| c.auto_created).unwrap_or(false),
        "streams": streams,
        "override": overridden.map(|o| json!({
            "name": o.name,
            "channel_number": o.channel_number,
            "channel_group_id": o.channel_group_id,
            "logo_id": o.logo_id,
            "tvg_id": o.tvg_id,
            "tvc_guide_stationid": o.tvc_guide_stationid,
            "epg_data_id": o.epg_data_id,
            "stream_profile_id": o.stream_profile_id,
        })),
        "effective_name": effective.name,
        "effective_channel_number": effective.channel_number,
        "effective_tvg_id": effective.tvg_id,
        "effective_tvc_guide_stationid": effective.tvc_guide_stationid,
        "effective_epg_data_id": effective.epg_data_id,
        // The guide channel the mapping points at, by name. `effective_tvg_id`
        // is a label the provider put in its playlist and says nothing about
        // whether this channel has listings: auto-matching writes `epg_data_id`
        // on its own, so most mapped channels have no `tvg_id` at all.
        "epg_name": epg_name,
        "effective_stream_profile_id": effective.stream_profile_id,
        "group_name": effective.group_name,
        "logo_url": effective.logo_url,
    })
}

/// Assemble the page in five queries rather than five per row: the effective
/// view, then base rows, overrides, failover lists and guide names for exactly
/// those ids.
async fn hydrate(
    state: &AppState,
    effective: Vec<dollet_core::domain::EffectiveChannel>,
) -> ApiResult<Vec<Value>> {
    let ids: Vec<Id> = effective.iter().map(|c| c.id).collect();

    let bases: HashMap<Id, Channel> = db::channels::get_many(&state.db, &ids)
        .await?
        .into_iter()
        .map(|channel| (channel.id, channel))
        .collect();

    let overrides: HashMap<Id, ChannelOverride> = db::channels::overrides_for(&state.db, &ids)
        .await?
        .into_iter()
        .map(|row| (row.channel_id, row))
        .collect();

    let mut streams: HashMap<Id, Vec<Id>> = HashMap::new();
    for (channel_id, stream_id) in db::channels::stream_ids_for(&state.db, &ids).await? {
        streams.entry(channel_id).or_default().push(stream_id);
    }

    let mapped: Vec<Id> = effective.iter().filter_map(|c| c.epg_data_id).collect();
    let epg_names = db::epg::names_for(&state.db, &mapped).await?;

    Ok(effective
        .iter()
        .map(|channel| {
            serialize(
                channel,
                bases.get(&channel.id),
                overrides.get(&channel.id),
                streams.get(&channel.id).map(Vec::as_slice).unwrap_or(&[]),
                channel
                    .epg_data_id
                    .and_then(|id| epg_names.get(&id))
                    .map(String::as_str),
            )
        })
        .collect())
}

async fn list(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let filter = ChannelFilter {
        search: query.search.as_deref(),
        group_id: query.channel_group,
        profile_id: query.channel_profile,
        visible_only: false,
    };

    // The channel table sorts the whole lineup client-side, so it asks for all
    // of it — `?all=true`, the same gesture as everywhere else. See `paging`.
    let paging = paging(query.page, query.page_size, query.all());
    let page =
        db::channels::list_effective(&state.db, &filter, query.ordering.as_deref(), paging).await?;
    let count = page.count;
    let rows = hydrate(&state, page.results).await?;

    Ok(Json(listing(paging, count, rows)))
}

async fn get_channel(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let effective = db::channels::get_effective(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    let mut rows = hydrate(&state, vec![effective]).await?;
    Ok(Json(rows.remove(0)))
}

#[derive(Deserialize, Default)]
struct ChannelBody {
    name: Option<String>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    channel_number: Option<Option<f64>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    channel_group_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    logo_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    tvg_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    tvc_guide_stationid: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    epg_data_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    stream_profile_id: Option<Option<Id>>,
    user_level: Option<i64>,
    is_adult: Option<bool>,
    is_catchup: Option<bool>,
    catchup_days: Option<u32>,
    hidden_from_output: Option<bool>,
    streams: Option<Vec<Id>>,
    /// An object merges into the override row; an explicit `null` drops it.
    #[serde(
        rename = "override",
        default,
        deserialize_with = "super::explicit_null"
    )]
    overrides: Option<Option<Value>>,
}

impl ChannelBody {
    fn apply(&self, channel: &mut Channel) {
        if let Some(name) = &self.name {
            channel.name = name.clone();
        }
        if let Some(number) = self.channel_number {
            channel.channel_number = number;
        }
        if let Some(group) = self.channel_group_id {
            channel.channel_group_id = group;
        }
        if let Some(logo) = self.logo_id {
            channel.logo_id = logo;
        }
        if let Some(tvg_id) = &self.tvg_id {
            channel.tvg_id = tvg_id.clone();
        }
        if let Some(station) = &self.tvc_guide_stationid {
            channel.tvc_guide_stationid = station.clone();
        }
        if let Some(epg) = self.epg_data_id {
            channel.epg_data_id = epg;
        }
        if let Some(profile) = self.stream_profile_id {
            channel.stream_profile_id = profile;
        }
        if let Some(level) = self.user_level {
            channel.user_level = level_from_i64(level);
        }
        if let Some(adult) = self.is_adult {
            channel.is_adult = adult;
        }
        if let Some(catchup) = self.is_catchup {
            channel.is_catchup = catchup;
        }
        if let Some(days) = self.catchup_days {
            channel.catchup_days = days;
        }
        if let Some(hidden) = self.hidden_from_output {
            channel.hidden_from_output = hidden;
        }
    }
}

async fn create(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<ChannelBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut channel = Channel {
        id: 0,
        uuid: uuid::Uuid::nil(),
        channel_number: None,
        name: String::new(),
        logo_id: None,
        channel_group_id: None,
        tvg_id: None,
        tvc_guide_stationid: None,
        epg_data_id: None,
        stream_profile_id: None,
        user_level: level_from_i64(0),
        is_adult: false,
        hidden_from_output: false,
        auto_created: false,
        is_catchup: false,
        catchup_days: 0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    body.apply(&mut channel);

    if channel.name.trim().is_empty() {
        return Err(Error::invalid("name is required").into());
    }
    if channel.channel_number.is_none() {
        channel.channel_number = Some(next_free_number(&state, channel.channel_group_id).await?);
    }

    let created = db::channels::create(&state.db, &channel).await?;
    if let Some(streams) = &body.streams {
        db::channels::set_streams(&state.db, created.id, streams).await?;
    }

    // A channel is a line in the playlist and a `<channel>` in the guide, so
    // every write to one supersedes both rendered outputs.
    outputs::invalidate(&state, &Output::BOTH).await;

    let effective = db::channels::get_effective(&state.db, created.id)
        .await?
        .ok_or(Error::NotFound)?;
    let mut rows = hydrate(&state, vec![effective]).await?;
    Ok((StatusCode::CREATED, Json(rows.remove(0))))
}

/// The number a channel created without one takes.
///
/// Its group's range when the group has one — the next slot on the step's
/// grid, so a channel added by hand lands beside the ones auto-sync creates
/// and after them rather than in a gap left on purpose. Otherwise the lowest
/// free integer, the rule that produced every number an imported
/// lineup carries. Never left empty: the HDHR lineup drops an
/// unnumbered channel, so "create" would have quietly meant "hide from Plex".
async fn next_free_number(state: &AppState, group: Option<Id>) -> Result<f64, Error> {
    let used = db::channels::numbers_in_use(&state.db).await?;
    let range = match group {
        Some(id) => db::channels::get_group(&state.db, id)
            .await?
            .and_then(|group| group.number_start.map(|start| (start, group.number_end))),
        None => None,
    };

    match range {
        Some((start, end)) => {
            let policy: NumberingSettings = settings::load(&state.db).await?;
            let own = db::channels::numbers_in_group(&state.db, group.unwrap_or_default()).await?;
            sync_channels::next_slot(&own, &used, start, end, policy.channel_step).ok_or_else(
                || {
                    Error::Conflict(format!(
                        "the group's number range from {start} to {} is full",
                        end.map_or_else(|| "the end".to_owned(), |end| end.to_string()),
                    ))
                },
            )
        }
        None => sync_channels::next_available(&used, 1.0, None)
            .ok_or_else(|| Error::Conflict("no channel number is free".into())),
    }
}

/// The providers whose streams in this group keep the provider's own
/// numbers. A group with any such feed must not be renumbered: an OTA tuner's
/// `5.1` is that channel's identity, not a slot in a range.
async fn provider_numbered_by(state: &AppState, group_id: Id) -> Result<Vec<String>, Error> {
    let mut names = Vec::new();
    for link in db::m3u::list_group_links(&state.db, None).await? {
        if link.channel_group_id == group_id
            && link.enabled
            && super::m3u::numbering_mode(&link) == "provider"
        {
            let name = db::m3u::get_account(&state.db, link.m3u_account_id)
                .await?
                .map(|account| account.name)
                .unwrap_or_else(|| format!("account {}", link.m3u_account_id));
            names.push(name);
        }
    }
    Ok(names)
}

fn assigned_json(plan: &db::channels::RenumberPlan) -> Vec<Value> {
    plan.assigned
        .iter()
        .map(|(id, number)| json!({ "id": id, "channel_number": number }))
        .collect()
}

/// How to sort a group before laying it out on its range.
///
/// A query parameter rather than a body so that the calls that predate it —
/// and the ones that want the lineup's own order — need send nothing at all.
#[derive(Deserialize, Default)]
struct OrderQuery {
    #[serde(default)]
    order: RenumberOrder,
}

/// Walk one group's channels through its range, in the order asked for.
///
/// The one request that moves numbers already assigned — nothing in a refresh
/// calls it. The caller has been told Plex maps channels by number and will
/// need a re-scan; that is the price of moving from the click-order lineup an
/// import carries over to one where a group's channels sit together.
async fn renumber_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Query(query): Query<OrderQuery>,
) -> ApiResult<Json<Value>> {
    let group = db::channels::get_group(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    if group.number_start.is_none() {
        return Err(Error::invalid("the group has no number range to renumber into").into());
    }
    let providers = provider_numbered_by(&state, id).await?;
    if !providers.is_empty() {
        return Err(Error::invalid(format!(
            "{} is numbered by {}; its channels keep the provider's numbers",
            group.name,
            providers.join(", "),
        ))
        .into());
    }

    let policy: NumberingSettings = settings::load(&state.db).await?;
    let plans =
        db::channels::plan_renumber(&state.db, &[group], policy.channel_step, query.order).await?;
    db::channels::apply_renumber(&state.db, &plans).await?;
    outputs::invalidate(&state, &Output::BOTH).await;

    Ok(Json(json!({
        "renumbered": plans[0].assigned.len(),
        "channels": assigned_json(&plans[0]),
    })))
}

/// Every group that can be renumbered, laid out as `renumber_all` would lay
/// it out, plus the ones it would leave alone and why. Nothing is written:
/// this is the table the operator reads before pressing the button.
async fn renumber_all_plan(
    state: &AppState,
    order: RenumberOrder,
) -> Result<(Vec<db::channels::ChannelGroupPlan>, Vec<Value>), Error> {
    let policy: NumberingSettings = settings::load(&state.db).await?;
    let usage: HashMap<Id, i64> = db::channels::group_usage(&state.db)
        .await?
        .into_iter()
        .map(|(id, channels, _)| (id, channels))
        .collect();

    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    for group in db::channels::list_groups(&state.db).await? {
        if group.number_start.is_none() {
            skipped.push(json!({ "id": group.id, "name": group.name, "reason": "no range" }));
            continue;
        }
        if usage.get(&group.id).copied().unwrap_or(0) == 0 {
            skipped.push(json!({ "id": group.id, "name": group.name, "reason": "no channels" }));
            continue;
        }
        let providers = provider_numbered_by(state, group.id).await?;
        if !providers.is_empty() {
            skipped.push(json!({
                "id": group.id,
                "name": group.name,
                "reason": format!("numbered by {}", providers.join(", ")),
            }));
            continue;
        }
        eligible.push(group);
    }

    let plans =
        db::channels::plan_renumber(&state.db, &eligible, policy.channel_step, order).await?;
    let planned = eligible
        .into_iter()
        .zip(plans)
        .map(|(group, plan)| db::channels::ChannelGroupPlan { group, plan })
        .collect();
    Ok((planned, skipped))
}

async fn plan_json(
    state: &AppState,
    planned: &[db::channels::ChannelGroupPlan],
) -> Result<Value, Error> {
    let channels: HashMap<Id, (String, Option<f64>)> =
        db::channels::list_effective(&state.db, &ChannelFilter::default(), None, None)
            .await?
            .results
            .into_iter()
            .map(|channel| (channel.id, (channel.name, channel.channel_number)))
            .collect();

    Ok(Value::Array(
        planned
            .iter()
            .map(|entry| {
                json!({
                    "id": entry.group.id,
                    "name": entry.group.name,
                    "number_start": entry.group.number_start,
                    "number_end": entry.group.number_end,
                    "channels": entry.plan.assigned.iter().map(|(id, number)| {
                        let (name, from) = channels.get(id).cloned().unwrap_or_default();
                        json!({ "id": id, "name": name, "from": from, "to": number })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect(),
    ))
}

async fn plan_renumber_all(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<OrderQuery>,
) -> ApiResult<Json<Value>> {
    let (planned, skipped) = renumber_all_plan(&state, query.order).await?;
    Ok(Json(json!({
        "groups": plan_json(&state, &planned).await?,
        "skipped": skipped,
    })))
}

/// The plan, applied. Recomputed here rather than accepted from the client,
/// so what is written is what the lineup looks like now — the preview and
/// this share every line of the planning.
async fn renumber_all(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<OrderQuery>,
) -> ApiResult<Json<Value>> {
    let (planned, skipped) = renumber_all_plan(&state, query.order).await?;
    let plans: Vec<db::channels::RenumberPlan> =
        planned.iter().map(|entry| entry.plan.clone()).collect();
    db::channels::apply_renumber(&state.db, &plans).await?;
    if plans.iter().any(|plan| !plan.assigned.is_empty()) {
        outputs::invalidate(&state, &Output::BOTH).await;
    }
    Ok(Json(json!({
        "renumbered": plans.iter().map(|plan| plan.assigned.len()).sum::<usize>(),
        "groups": plans.len(),
        "skipped": skipped,
    })))
}

#[derive(Deserialize)]
struct PlanRangesBody {
    /// Groups in the order they should receive blocks — the table as the
    /// operator has it sorted. Ones that already have a range are left alone.
    order: Vec<Id>,
}

/// Blocks for the groups that have none, in the order given, each
/// `group_block_size` wide, starting at the first block boundary above
/// anything the lineup already uses. Nothing existing moves: a group with a
/// range keeps it, and the new blocks land after every number in use, so a
/// new group is always at the end of the guide until the operator says
/// otherwise.
fn lay_out_blocks(
    order: &[Id],
    groups: &HashMap<Id, dollet_core::domain::ChannelGroup>,
    in_use: &[f64],
    block: f64,
) -> Vec<(Id, f64, f64)> {
    let ceiling = groups
        .values()
        .filter_map(|group| group.number_end.or(group.number_start))
        .chain(in_use.iter().copied())
        .fold(0.0_f64, f64::max);
    let mut next = (ceiling / block).floor() * block + block;

    let mut seen = std::collections::BTreeSet::new();
    let mut ranges = Vec::new();
    for id in order {
        let Some(group) = groups.get(id) else {
            continue;
        };
        if group.number_start.is_some() || !seen.insert(*id) {
            continue;
        }
        ranges.push((*id, next, next + block - 1.0));
        next += block;
    }
    ranges
}

async fn plan_ranges(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<PlanRangesBody>,
) -> ApiResult<Json<Value>> {
    let policy: NumberingSettings = settings::load(&state.db).await?;
    let groups: HashMap<Id, dollet_core::domain::ChannelGroup> =
        db::channels::list_groups(&state.db)
            .await?
            .into_iter()
            .map(|group| (group.id, group))
            .collect();
    let in_use = db::channels::numbers_in_use(&state.db).await?;

    let ranges = lay_out_blocks(&body.order, &groups, &in_use, policy.group_block_size);
    Ok(Json(json!({
        "block_size": policy.group_block_size,
        "ranges": ranges
            .iter()
            .map(|(id, start, end)| json!({
                "id": id,
                "name": groups[id].name,
                "number_start": start,
                "number_end": end,
            }))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct AssignRangesBody {
    ranges: Vec<AssignedRange>,
}

#[derive(Deserialize)]
struct AssignedRange {
    id: Id,
    number_start: f64,
    number_end: Option<f64>,
}

/// Write the ranges the operator was shown. Refuses a group that has a range
/// already: assigning never moves an existing block, and a plan made before
/// someone typed a range by hand must not overwrite it.
async fn assign_ranges(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<AssignRangesBody>,
) -> ApiResult<Json<Value>> {
    let mut assigned = 0usize;
    for range in &body.ranges {
        let mut group = db::channels::get_group(&state.db, range.id)
            .await?
            .ok_or(Error::NotFound)?;
        if group.number_start.is_some() {
            return Err(Error::Conflict(format!("{} already has a range", group.name)).into());
        }
        check_range(Some(range.number_start), range.number_end)?;
        group.number_start = Some(range.number_start);
        group.number_end = range.number_end;
        db::channels::save_group(&state.db, &group).await?;
        assigned += 1;
    }
    Ok(Json(json!({ "assigned": assigned })))
}

#[derive(Deserialize)]
struct MoveBody {
    /// The channels the drop landed between, as the operator saw them: the row
    /// above and the row below. Either may be absent at the ends of a page.
    #[serde(default)]
    after: Option<Id>,
    #[serde(default)]
    before: Option<Id>,
}

/// Put one channel between two others.
///
/// The counterpart to renumbering a whole group: this writes exactly one
/// number, so the re-scan it costs in Plex is one channel rather than a
/// lineup. It refuses instead of making room, because making room here would
/// mean renumbering everything below the drop — the thing the operator did
/// not ask for.
async fn move_channel(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<MoveBody>,
) -> ApiResult<Json<Value>> {
    let policy: NumberingSettings = settings::load(&state.db).await?;
    let number =
        db::channels::plan_move(&state.db, id, body.after, body.before, policy.channel_step)
            .await?;
    db::channels::apply_numbers(&state.db, &[(id, number)]).await?;
    outputs::invalidate(&state, &Output::BOTH).await;

    Ok(Json(json!({ "id": id, "channel_number": number })))
}

async fn update(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<ChannelBody>,
) -> ApiResult<Json<Value>> {
    let mut channel = db::channels::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    body.apply(&mut channel);
    db::channels::save(&state.db, &channel).await?;

    if let Some(streams) = &body.streams {
        db::channels::set_streams(&state.db, id, streams).await?;
    }

    if let Some(overrides) = &body.overrides {
        apply_override(&state, id, overrides.as_ref()).await?;
    }
    outputs::invalidate(&state, &Output::BOTH).await;

    get_channel(State(state), _admin, Path(id)).await
}

/// Per-field override edits.
///
/// The nesting is load-bearing: an absent key leaves that override alone, an
/// explicit `null` stops overriding the field, and a value sets it. Collapsing
/// those two nulls into one would make "clear this one override" impossible to
/// express without resending the whole row.
#[derive(Deserialize, Default)]
struct OverrideBody {
    #[serde(default, deserialize_with = "super::explicit_null")]
    name: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    channel_number: Option<Option<f64>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    channel_group_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    logo_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    tvg_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    tvc_guide_stationid: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    epg_data_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    stream_profile_id: Option<Option<Id>>,
}

impl OverrideBody {
    fn merge_into(self, target: &mut ChannelOverride) {
        if let Some(name) = self.name {
            target.name = name;
        }
        if let Some(number) = self.channel_number {
            target.channel_number = number;
        }
        if let Some(group) = self.channel_group_id {
            target.channel_group_id = group;
        }
        if let Some(logo) = self.logo_id {
            target.logo_id = logo;
        }
        if let Some(tvg_id) = self.tvg_id {
            target.tvg_id = tvg_id;
        }
        if let Some(station) = self.tvc_guide_stationid {
            target.tvc_guide_stationid = station;
        }
        if let Some(epg) = self.epg_data_id {
            target.epg_data_id = epg;
        }
        if let Some(profile) = self.stream_profile_id {
            target.stream_profile_id = profile;
        }
    }

    /// Whether the override says nothing, and so should not exist as a row.
    ///
    /// Compared against a default-constructed one rather than field by field,
    /// so a field added to `ChannelOverride` cannot be forgotten here and make
    /// a one-field PATCH look empty.
    fn is_empty(target: &ChannelOverride) -> bool {
        let empty = ChannelOverride {
            channel_id: target.channel_id,
            ..ChannelOverride::default()
        };
        serde_json::to_value(target).ok() == serde_json::to_value(&empty).ok()
    }
}

/// `None` drops the override row; an object merges into it.
async fn apply_override(state: &AppState, channel_id: Id, body: Option<&Value>) -> ApiResult<()> {
    let Some(body) = body else {
        db::channels::clear_override(&state.db, channel_id).await?;
        return Ok(());
    };

    let patch: OverrideBody = serde_json::from_value(body.clone())
        .map_err(|e| Error::invalid(format!("override: {e}")))?;

    let mut current = db::channels::get_override(&state.db, channel_id)
        .await?
        .unwrap_or(ChannelOverride {
            channel_id,
            ..Default::default()
        });
    patch.merge_into(&mut current);

    // An override with nothing left in it is a row that only slows the view
    // down; dropping it keeps "has_override" honest in the UI.
    if OverrideBody::is_empty(&current) {
        db::channels::clear_override(&state.db, channel_id).await?;
    } else {
        db::channels::save_override(&state.db, &current).await?;
    }
    Ok(())
}

async fn delete_channel(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::channels::delete(&state.db, id).await?;
    outputs::invalidate(&state, &Output::BOTH).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn bulk_delete(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<super::IdList>,
) -> ApiResult<Json<Value>> {
    let deleted = db::channels::delete_many(&state.db, &body.ids).await?;
    if deleted > 0 {
        outputs::invalidate(&state, &Output::BOTH).await;
    }
    Ok(Json(json!({ "deleted": deleted })))
}

async fn channel_streams(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::streams::for_channel(&state.db, id).await?,
    )))
}

async fn set_channel_streams(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<super::IdList>,
) -> ApiResult<Json<Value>> {
    db::channels::set_streams(&state.db, id, &body.ids).await?;
    // The failover list decides which provider URL `?direct=true` hands out.
    // The guide has no stream URLs in it.
    outputs::invalidate(&state, &[Output::Playlist]).await;
    // The same shape the GET returns, so a caller can use the response of a
    // write exactly as it uses the read it replaces.
    Ok(Json(whole_list(
        db::streams::for_channel(&state.db, id).await?,
    )))
}

// ---------------------------------------------------------------- profiles

#[derive(Deserialize)]
struct NamedBody {
    name: String,
}

async fn list_profiles(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let profiles = db::channel_profiles::list(&state.db).await?;
    let mut out = Vec::with_capacity(profiles.len());

    for profile in profiles {
        let members = db::channel_profiles::members(&state.db, profile.id).await?;
        out.push(json!({
            "id": profile.id,
            "name": profile.name,
            "channels": members
                .iter()
                .map(|m| json!({ "channel": m.channel_id, "enabled": m.enabled }))
                .collect::<Vec<_>>(),
        }));
    }
    Ok(Json(whole_list(out)))
}

async fn get_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let profile = db::channel_profiles::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    let members = db::channel_profiles::members(&state.db, id).await?;

    Ok(Json(json!({
        "id": profile.id,
        "name": profile.name,
        "channels": members
            .iter()
            .map(|m| json!({ "channel": m.channel_id, "enabled": m.enabled }))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct ProfileBody {
    name: String,
    #[serde(default)]
    start_empty: bool,
}

async fn create_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<ProfileBody>,
) -> ApiResult<(StatusCode, Json<ChannelProfile>)> {
    let profile =
        db::channel_profiles::create(&state.db, body.name.trim(), body.start_empty).await?;
    Ok((StatusCode::CREATED, Json(profile)))
}

async fn update_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<NamedBody>,
) -> ApiResult<Json<ChannelProfile>> {
    Ok(Json(
        db::channel_profiles::rename(&state.db, id, body.name.trim()).await?,
    ))
}

async fn delete_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::channel_profiles::delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct MembershipBody {
    enabled: bool,
}

async fn set_membership(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path((profile_id, channel_id)): Path<(Id, Id)>,
    Json(body): Json<MembershipBody>,
) -> ApiResult<Json<Value>> {
    db::channel_profiles::set_enabled(&state.db, profile_id, &[channel_id], body.enabled).await?;
    // Membership decides which channels `/output/m3u/{profile}` and
    // `/output/epg/{profile}` carry at all.
    outputs::invalidate(&state, &Output::BOTH).await;
    Ok(Json(
        json!({ "channel": channel_id, "enabled": body.enabled }),
    ))
}

#[derive(Deserialize)]
struct BulkMembershipBody {
    channel_ids: Vec<Id>,
    enabled: bool,
}

async fn bulk_membership(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(profile_id): Path<Id>,
    Json(body): Json<BulkMembershipBody>,
) -> ApiResult<Json<Value>> {
    db::channel_profiles::set_enabled(&state.db, profile_id, &body.channel_ids, body.enabled)
        .await?;
    outputs::invalidate(&state, &Output::BOTH).await;
    Ok(Json(json!({ "updated": body.channel_ids.len() })))
}
