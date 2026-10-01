//! `/api/m3u/` — provider accounts, their profiles and filters, and refresh.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use dollet_core::db::jobs;
use dollet_core::db::m3u::{GroupAccountLink, M3uFilter, PatternCheck};
use dollet_core::domain::{Id, M3uAccount, M3uAccountProfile, M3uAccountType};
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::{ApiError, ApiResult};
use super::ingest;
use super::outputs::{self, Output};
use super::whole_list;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/accounts/", get(list_accounts).post(create_account))
        .route(
            "/accounts/{id}/",
            get(get_account)
                .patch(update_account)
                .put(update_account)
                .delete(delete_account),
        )
        .route(
            "/accounts/{id}/profiles/",
            get(list_profiles).post(create_profile),
        )
        .route(
            "/accounts/{account_id}/profiles/{id}/",
            axum::routing::patch(update_profile).delete(delete_profile),
        )
        .route(
            "/accounts/{id}/filters/",
            get(list_filters).post(create_filter),
        )
        .route(
            "/accounts/{account_id}/filters/{id}/",
            axum::routing::delete(delete_filter),
        )
        .route(
            "/accounts/{id}/groups/",
            get(list_groups).post(update_group),
        )
        .route("/refresh/", post(refresh_all))
        .route("/refresh/{id}/", post(refresh_account))
        .route("/server-groups/", get(list_server_groups))
}

async fn job_for(state: &AppState, account_id: Id) -> Result<Option<jobs::Job>, Error> {
    jobs::by_key(&state.db, &ingest::m3u::job_key(account_id)).await
}

fn serialize(account: &M3uAccount, job: Option<&jobs::Job>) -> Value {
    let base = json!({
        "id": account.id,
        "name": account.name,
        "account_type": db::m3u::account_type_str(account.account_type),
        "server_url": account.server_url,
        "file_path": account.file_path,
        "username": account.username,
        // The provider password is write-only for the same reason the EPG
        // source's is: it leaves this server only towards the provider.
        "has_password": account.password.is_some(),
        "max_streams": account.max_streams,
        "is_active": account.is_active,
        "locked": account.locked,
        "priority": account.priority,
        "user_agent_id": account.user_agent_id,
        "stream_profile_id": account.stream_profile_id,
        "refresh_interval_hours": account.refresh_interval_hours,
        "stale_stream_days": account.stale_stream_days,
        "custom_properties": account.custom_properties,
    });
    super::with_job_status(base, job)
}

async fn list_accounts(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let accounts = db::m3u::list_accounts(&state.db).await?;
    let mut out = Vec::with_capacity(accounts.len());
    for account in &accounts {
        out.push(serialize(
            account,
            job_for(&state, account.id).await?.as_ref(),
        ));
    }
    Ok(Json(whole_list(out)))
}

async fn get_account(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let account = db::m3u::get_account(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(serialize(
        &account,
        job_for(&state, id).await?.as_ref(),
    )))
}

#[derive(Deserialize, Default)]
struct AccountBody {
    name: Option<String>,
    account_type: Option<String>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    server_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    file_path: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    username: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    password: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    user_agent_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    stream_profile_id: Option<Option<Id>>,
    max_streams: Option<u32>,
    is_active: Option<bool>,
    priority: Option<u32>,
    /// Hours, and the name says so. `refresh_interval` is accepted as an alias
    /// because that is what this API took before 1.0.
    #[serde(alias = "refresh_interval")]
    refresh_interval_hours: Option<u32>,
    stale_stream_days: Option<u32>,
    custom_properties: Option<Value>,
}

/// The account type a request named.
///
/// Strict: an unrecognised value is a 400. Falling through to `Standard` would
/// create a plain-playlist account pointed at an Xtream provider.
///
/// `XC` and `STD` are accepted because that is what this API emitted before
/// 1.0 and what the SPA sent. They are not what it emits now.
fn parse_account_type(raw: &str) -> Result<M3uAccountType, Error> {
    match raw {
        "XC" => Ok(M3uAccountType::XtreamCodes),
        "STD" => Ok(M3uAccountType::Standard),
        other => db::m3u::account_type(other),
    }
}

impl AccountBody {
    fn apply(self, account: &mut M3uAccount) -> Result<(), Error> {
        if let Some(name) = self.name {
            account.name = name;
        }
        if let Some(kind) = self.account_type {
            account.account_type = parse_account_type(&kind)?;
        }
        if let Some(url) = self.server_url {
            account.server_url = url;
        }
        if let Some(path) = self.file_path {
            account.file_path = path;
        }
        if let Some(username) = self.username {
            account.username = username;
        }
        if let Some(password) = self.password {
            account.password = password;
        }
        if let Some(agent) = self.user_agent_id {
            account.user_agent_id = agent;
        }
        if let Some(profile) = self.stream_profile_id {
            account.stream_profile_id = profile;
        }
        if let Some(max) = self.max_streams {
            account.max_streams = max;
        }
        if let Some(active) = self.is_active {
            account.is_active = active;
        }
        if let Some(priority) = self.priority {
            account.priority = priority;
        }
        if let Some(interval) = self.refresh_interval_hours {
            // Clamped rather than trusted: the value is turned into a
            // `chrono::Duration` added to the current instant, which panics on
            // overflow. A year is longer than any refresh anyone schedules and
            // nowhere near the range where that happens.
            account.refresh_interval_hours = interval.clamp(0, super::MAX_REFRESH_HOURS);
        }
        if let Some(days) = self.stale_stream_days {
            account.stale_stream_days = days;
        }
        if let Some(properties) = self.custom_properties {
            account.custom_properties = properties;
        }
        Ok(())
    }
}

async fn create_account(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<AccountBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut account = M3uAccount {
        id: 0,
        name: String::new(),
        server_url: None,
        file_path: None,
        username: None,
        password: None,
        account_type: M3uAccountType::Standard,
        max_streams: 0,
        is_active: true,
        locked: false,
        priority: 0,
        user_agent_id: None,
        stream_profile_id: None,
        refresh_interval_hours: 24,
        stale_stream_days: 7,
        custom_properties: json!({}),
    };
    body.apply(&mut account)?;

    if account.name.trim().is_empty() {
        return Err(Error::invalid("name is required").into());
    }

    let created = db::m3u::create_account(&state.db, &account).await?;

    // Every account gets a default profile, because the streaming path resolves
    // one and an account without it has no way to spend its connection budget.
    db::m3u::create_account_profile(
        &state.db,
        &M3uAccountProfile {
            id: 0,
            m3u_account_id: created.id,
            name: format!("{} Default", created.name),
            is_default: true,
            is_active: true,
            max_streams: created.max_streams,
            search_pattern: "^(.*)$".into(),
            replace_pattern: "$1".into(),
        },
    )
    .await?;

    super::jobs::sync_schedule(&state).await?;
    Ok((
        StatusCode::CREATED,
        Json(serialize(
            &created,
            job_for(&state, created.id).await?.as_ref(),
        )),
    ))
}

async fn update_account(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<AccountBody>,
) -> ApiResult<Json<Value>> {
    let mut account = db::m3u::get_account(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    if account.locked {
        return Err(Error::invalid("the built-in account cannot be edited").into());
    }

    body.apply(&mut account)?;
    let saved = db::m3u::save_account(&state.db, &account).await?;
    super::jobs::sync_schedule(&state).await?;
    Ok(Json(serialize(&saved, job_for(&state, id).await?.as_ref())))
}

async fn delete_account(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::m3u::delete_account(&state.db, id).await?;
    super::jobs::sync_schedule(&state).await?;
    // Its streams cascade away with it, so every `?direct=true` URL they were
    // answering for goes too.
    outputs::invalidate(&state, &[Output::Playlist]).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn refresh_account(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    db::m3u::get_account(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(
        super::jobs::start_now(&state, &ingest::m3u::job_key(id)).await?,
    ))
}

async fn refresh_all(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let keys = db::m3u::list_accounts(&state.db)
        .await?
        .into_iter()
        .filter(|account| !account.locked)
        .map(|account| ingest::m3u::job_key(account.id));
    let started = super::jobs::start_each(&state, keys).await?;
    Ok(Json(json!({ "started": started })))
}

// -------------------------------------------------------------- profiles

async fn list_profiles(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::m3u::list_account_profiles(&state.db, Some(id)).await?,
    )))
}

#[derive(Deserialize)]
struct ProfileBody {
    name: Option<String>,
    is_active: Option<bool>,
    max_streams: Option<u32>,
    search_pattern: Option<String>,
    replace_pattern: Option<String>,
}

/// A pattern that will not compile changes which streams a profile rewrites,
/// so it is refused at the door rather than discovered at the next refresh.
fn check(pattern: &str) -> Result<(), Error> {
    match db::m3u::check_pattern(pattern) {
        PatternCheck::Ok | PatternCheck::Rewritten(_) => Ok(()),
        PatternCheck::Failed(message) => Err(Error::invalid(format!(
            "`{pattern}` will not compile: {message}"
        ))),
    }
}

async fn create_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(account_id): Path<Id>,
    Json(body): Json<ProfileBody>,
) -> ApiResult<(StatusCode, Json<M3uAccountProfile>)> {
    let Some(name) = body.name else {
        return Err(Error::invalid("name is required").into());
    };
    let search = body.search_pattern.unwrap_or_else(|| "^(.*)$".into());
    check(&search)?;

    let created = db::m3u::create_account_profile(
        &state.db,
        &M3uAccountProfile {
            id: 0,
            m3u_account_id: account_id,
            name,
            is_default: false,
            is_active: body.is_active.unwrap_or(true),
            max_streams: body.max_streams.unwrap_or(0),
            search_pattern: search,
            replace_pattern: body.replace_pattern.unwrap_or_else(|| "$1".into()),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn update_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path((_account_id, id)): Path<(Id, Id)>,
    Json(body): Json<ProfileBody>,
) -> ApiResult<Json<M3uAccountProfile>> {
    let mut profile = db::m3u::get_account_profile(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;

    if let Some(name) = body.name {
        profile.name = name;
    }
    if let Some(active) = body.is_active {
        profile.is_active = active;
    }
    if let Some(max) = body.max_streams {
        profile.max_streams = max;
    }
    if let Some(search) = body.search_pattern {
        check(&search)?;
        profile.search_pattern = search;
    }
    if let Some(replace) = body.replace_pattern {
        profile.replace_pattern = replace;
    }

    Ok(Json(
        db::m3u::save_account_profile(&state.db, &profile).await?,
    ))
}

async fn delete_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path((_account_id, id)): Path<(Id, Id)>,
) -> ApiResult<StatusCode> {
    db::m3u::delete_account_profile(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// --------------------------------------------------------------- filters

fn serialize_filter(filter: &M3uFilter) -> Value {
    json!({
        "id": filter.id,
        "m3u_account_id": filter.m3u_account_id,
        "filter_type": filter.filter_type,
        "regex_pattern": filter.regex_pattern,
        "exclude": filter.exclude,
        "order": filter.sort_order,
    })
}

async fn list_filters(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::m3u::list_filters(&state.db, Some(id))
            .await?
            .iter()
            .map(serialize_filter)
            .collect(),
    )))
}

#[derive(Deserialize)]
struct FilterBody {
    filter_type: String,
    regex_pattern: String,
    #[serde(default)]
    exclude: bool,
    #[serde(default, rename = "order")]
    sort_order: i64,
}

async fn create_filter(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(account_id): Path<Id>,
    Json(body): Json<FilterBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    check(&body.regex_pattern)?;

    let created = db::m3u::create_filter(
        &state.db,
        &M3uFilter {
            id: 0,
            m3u_account_id: account_id,
            filter_type: body.filter_type,
            regex_pattern: body.regex_pattern,
            exclude: body.exclude,
            sort_order: body.sort_order,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(serialize_filter(&created))))
}

async fn delete_filter(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path((_account_id, id)): Path<(Id, Id)>,
) -> ApiResult<StatusCode> {
    db::m3u::delete_filter(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- groups

async fn list_groups(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let groups: std::collections::HashMap<Id, String> = db::channels::list_groups(&state.db)
        .await?
        .into_iter()
        .map(|group| (group.id, group.name))
        .collect();

    Ok(Json(whole_list(
        db::m3u::list_group_links(&state.db, Some(id))
            .await?
            .iter()
            .map(|link| {
                json!({
                    "channel_group": link.channel_group_id,
                    "name": groups.get(&link.channel_group_id),
                    "enabled": link.enabled,
                    "auto_channel_sync": link.auto_channel_sync,
                    "numbering_mode": numbering_mode(link),
                    "custom_properties": link.custom_properties,
                })
            })
            .collect(),
    )))
}

#[derive(Deserialize)]
struct GroupBody {
    channel_group: Id,
    enabled: Option<bool>,
    auto_channel_sync: Option<bool>,
    /// `range` or `provider`: whether this provider's streams in the group
    /// take numbers from the group's range or keep the ones the provider
    /// sends. Stored as `channel_numbering_mode` in the link's
    /// `custom_properties`, so an imported link and one set here read the same
    /// way.
    numbering_mode: Option<String>,
    /// The auto-sync options with no column of their own: the numbering mode
    /// and its fallback, and the rename pattern and its replacement.
    custom_properties: Option<Value>,
    /// Acknowledgement of the counts a prior unconfirmed call reported.
    #[serde(default)]
    confirm: bool,
}

/// What disabling a group would cost, in rows the user can name.
///
/// Unticking one checkbox makes the next refresh delete every stream in that
/// group for this account, and `channel_stream ON DELETE CASCADE` takes the
/// assignments with them. Re-enabling brings the streams back with new ids, so
/// the assignments do not come back — the action is not undoable, and the
/// checkbox gives no sign of that.
async fn disable_cost(state: &AppState, account_id: Id, group_id: Id) -> Result<Value, Error> {
    let row: (i64, i64, i64) = sqlx::query_as(
        "WITH doomed AS (
             SELECT id FROM stream WHERE m3u_account_id = ? AND channel_group_id = ?
         )
         SELECT
             (SELECT COUNT(*) FROM doomed),
             (SELECT COUNT(DISTINCT cs.channel_id)
                FROM channel_stream cs WHERE cs.stream_id IN (SELECT id FROM doomed)),
             (SELECT COUNT(*) FROM (
                  SELECT cs.channel_id
                    FROM channel_stream cs
                   GROUP BY cs.channel_id
                  HAVING SUM(CASE WHEN cs.stream_id IN (SELECT id FROM doomed) THEN 0 ELSE 1 END) = 0
                    AND SUM(CASE WHEN cs.stream_id IN (SELECT id FROM doomed) THEN 1 ELSE 0 END) > 0
              ))",
    )
    .bind(account_id)
    .bind(group_id)
    .fetch_one(&state.db)
    .await?;

    Ok(json!({
        "streams_deleted": row.0,
        "channels_affected": row.1,
        "channels_left_unplayable": row.2,
    }))
}

async fn update_group(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(account_id): Path<Id>,
    Json(body): Json<GroupBody>,
) -> ApiResult<Json<Value>> {
    let existing = db::m3u::list_group_links(&state.db, Some(account_id))
        .await?
        .into_iter()
        .find(|link| link.channel_group_id == body.channel_group);

    // Turning the group off is a delete, so it is confirmed rather than
    // performed. The counts come back with the refusal so the caller can say
    // what is about to be lost instead of "are you sure?".
    let turning_off = body.enabled == Some(false) && existing.as_ref().is_none_or(|l| l.enabled);
    if turning_off && !body.confirm {
        let cost = disable_cost(&state, account_id, body.channel_group).await?;
        if cost["streams_deleted"].as_i64().unwrap_or(0) > 0 {
            return Err(ApiError::with(
                StatusCode::CONFLICT,
                "disabling this group deletes its streams on the next refresh, and the \
                 channel assignments that point at them do not come back if it is \
                 re-enabled; repeat with `confirm: true` to proceed",
                json!({ "cost": cost }),
            ));
        }
    }

    let link = GroupAccountLink {
        id: existing.as_ref().map_or(0, |link| link.id),
        channel_group_id: body.channel_group,
        m3u_account_id: account_id,
        enabled: body
            .enabled
            .or(existing.as_ref().map(|link| link.enabled))
            .unwrap_or(true),
        auto_channel_sync: body
            .auto_channel_sync
            .or(existing.as_ref().map(|link| link.auto_channel_sync))
            .unwrap_or(false),
        // The link's own range columns. The range itself lives on
        // `channel_group`; these are where the import lands a source's
        // link-level range before `carry_group_ranges` lifts it, so they are
        // written once and read once. Preserved rather than cleared here: an
        // edit through this handler must not erase what the import recorded.
        auto_sync_channel_start: existing
            .as_ref()
            .and_then(|link| link.auto_sync_channel_start),
        auto_sync_channel_end: existing
            .as_ref()
            .and_then(|link| link.auto_sync_channel_end),
        custom_properties: {
            let mut properties = body.custom_properties.unwrap_or_else(|| {
                existing
                    .as_ref()
                    .map(|link| link.custom_properties.clone())
                    .unwrap_or_else(|| json!({}))
            });
            if let Some(mode) = body.numbering_mode.as_deref() {
                let stored = match mode {
                    "provider" => "provider",
                    "range" => "fixed",
                    other => {
                        return Err(ApiError::invalid_fields(
                            format!("`{other}` is not a numbering mode"),
                            [("numbering_mode", "must be `range` or `provider`")],
                        ));
                    }
                };
                if let Value::Object(map) = &mut properties {
                    map.insert("channel_numbering_mode".into(), json!(stored));
                }
            }
            properties
        },
    };

    // Switching auto-sync on is a create, so like the delete above it is
    // confirmed rather than performed: every stream in the group that no
    // channel uses becomes a channel on the next refresh, and a provider's
    // HD/FHD twin is exactly the kind of stream an operator kept out of the
    // lineup on purpose. The preview names each one with the number it would
    // take, so the answer can be "attach that one as a failover first".
    let enabling =
        link.auto_channel_sync && !existing.as_ref().is_some_and(|l| l.auto_channel_sync);
    if enabling && !body.confirm {
        let preview = sync_preview(&state, &link).await?;
        if preview["channels_created"].as_u64().unwrap_or(0) > 0 {
            return Err(ApiError::with(
                StatusCode::CONFLICT,
                "switching auto-sync on creates a channel for every stream in this group \
                 that no channel uses yet, on the next refresh; repeat with `confirm: true` \
                 to proceed, or attach any that should stay out of the lineup as failovers \
                 first",
                json!({ "cost": preview }),
            ));
        }
    }

    db::m3u::upsert_group_link(&state.db, &link).await?;
    Ok(Json(json!({
        "channel_group": link.channel_group_id,
        "enabled": link.enabled,
        "auto_channel_sync": link.auto_channel_sync,
        "numbering_mode": numbering_mode(&link),
    })))
}

/// How the link reads, whichever of the three stored modes it carries:
/// `provider` keeps the provider's numbers; everything else is the range.
pub(crate) fn numbering_mode(link: &GroupAccountLink) -> &'static str {
    match link
        .custom_properties
        .get("channel_numbering_mode")
        .and_then(Value::as_str)
    {
        Some("provider") => "provider",
        _ => "range",
    }
}

/// What the next refresh would create for this link, as the same planner that
/// creates it sees things right now.
async fn sync_preview(state: &AppState, link: &GroupAccountLink) -> Result<Value, Error> {
    let mut used = db::channels::numbers_in_use(&state.db).await?;
    let plan = ingest::m3u::plan_auto_sync(state, link, &mut used).await?;
    Ok(json!({
        "channels_created": plan.channels.len(),
        "unnumbered": plan.unnumbered,
        "channels": plan
            .channels
            .iter()
            .map(|planned| json!({
                "stream": planned.stream.id,
                "stream_name": planned.stream.name,
                "name": planned.name,
                "channel_number": planned.number,
            }))
            .collect::<Vec<_>>(),
    }))
}

async fn list_server_groups(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::m3u::list_server_groups(&state.db)
            .await?
            .into_iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect(),
    )))
}
