//! Settings, the profile catalogues, and the system event log.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use dollet_core::domain::{Id, OutputProfile, StreamProfile, UserAgent};
use dollet_core::{Error, db, settings};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::ApiResult;
use super::whole_list;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/version/", get(version))
        .route("/settings/", get(list_settings))
        .route("/settings/env/", get(environment))
        .route("/origins/", get(origins))
        .route(
            "/settings/{key}/",
            get(get_setting).patch(patch_setting).put(patch_setting),
        )
        .route("/system-events/", get(system_events))
        .route("/jobs/", get(list_jobs))
        .route("/jobs/{key}/", get(get_job))
        .route("/jobs/{key}/cancel/", axum::routing::post(cancel_job))
        .route(
            "/useragents/",
            get(list_user_agents).post(create_user_agent),
        )
        .route(
            "/useragents/{id}/",
            get(get_user_agent)
                .patch(update_user_agent)
                .put(update_user_agent)
                .delete(delete_user_agent),
        )
        .route(
            "/streamprofiles/",
            get(list_stream_profiles).post(create_stream_profile),
        )
        .route(
            "/streamprofiles/{id}/",
            get(get_stream_profile)
                .patch(update_stream_profile)
                .put(update_stream_profile)
                .delete(delete_stream_profile),
        )
        .route(
            "/outputprofiles/",
            get(list_output_profiles).post(create_output_profile),
        )
        .route(
            "/outputprofiles/{id}/",
            get(get_output_profile)
                .patch(update_output_profile)
                .put(update_output_profile)
                .delete(delete_output_profile),
        )
}

async fn version() -> Json<Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

/// What the UI needs to know about the deployment it is talking to.
async fn environment(State(state): State<AppState>, _admin: AdminUser) -> Json<Value> {
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "advertised_base_url": state.config.advertised_base_url,
        "artwork_base_url": state.config.artwork_base_url,
    }))
}

/// Which addresses clients have actually fetched a lineup, a playlist or a
/// guide on.
///
/// The Connect page cannot be told which base URL to show — the server has no
/// way to know how a client reaches it — so this is the evidence it offers
/// instead. Admin-only: it is a list of the names an instance answers to.
async fn origins(_admin: AdminUser) -> Json<Value> {
    Json(whole_list(super::origin::seen()))
}

async fn list_settings(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(settings::all(&state.db).await?)))
}

async fn get_setting(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(key): Path<String>,
) -> ApiResult<Json<settings::SettingRow>> {
    let row = settings::by_key_or_id(&state.db, &key)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(row))
}

#[derive(Deserialize)]
struct SettingPatch {
    /// The Settings page sends the whole row back; only `value` is writable.
    value: Value,
}

async fn patch_setting(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(key): Path<String>,
    Json(body): Json<SettingPatch>,
) -> ApiResult<Json<settings::SettingRow>> {
    let row = settings::by_key_or_id(&state.db, &key)
        .await?
        .ok_or(Error::NotFound)?;

    // Reported per endpoint class before delegating, because that is how the
    // Settings page renders it: one input per class, and a message under the
    // one that is wrong beats a sentence listing all of them above the form.
    // `patch_by_key` validates again and is the authority — this cannot be the
    // only check, because nothing stops another caller reaching it directly.
    if row.key == <settings::NetworkAccess as settings::Group>::KEY {
        let merged = settings::merge_network_access(&state.db, &body.value).await?;
        let invalid = merged.invalid_entries();
        if !invalid.is_empty() {
            let mut fields: std::collections::BTreeMap<String, String> =
                std::collections::BTreeMap::new();
            for (endpoint, entry) in invalid {
                let message = fields.entry(endpoint).or_default();
                if !message.is_empty() {
                    message.push_str(", ");
                }
                message.push_str(&entry);
            }
            let fields: Vec<(String, String)> = fields
                .into_iter()
                .map(|(endpoint, entries)| (endpoint, format!("not a CIDR range: {entries}")))
                .collect();
            return Err(super::error::ApiError::invalid_fields(
                "some entries are not CIDR ranges, and an entry that will not parse is \
                 skipped at request time — so saving one widens access rather than \
                 narrowing it",
                fields,
            ));
        }
    }

    if row.key == <settings::NumberingSettings as settings::Group>::KEY {
        let problems = settings::merge_numbering(&state.db, &body.value)
            .await?
            .problems();
        if !problems.is_empty() {
            return Err(super::error::ApiError::invalid_fields(
                "the numbering settings would leave a range with no slot in it",
                problems,
            ));
        }
    }

    settings::patch_by_key(&state.db, &row.key, &body.value).await?;
    let updated = settings::by_key(&state.db, &row.key)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(updated))
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    limit: Option<i64>,
}

async fn system_events(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<EventQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.unwrap_or(100);
    let count = db::events::count(&state.db).await?;
    let rows = db::events::list(&state.db, limit).await?;
    Ok(Json(super::capped_list(limit, count, rows)))
}

/// The scheduler's own view of a job: the persisted row plus whether this
/// process is running it now. After a crash those disagree, and the row alone
/// would show a refresh as still in flight forever.
pub(super) fn serialize_job(job: &db::jobs::Job) -> Value {
    let mut value = serde_json::to_value(job).unwrap_or_else(|_| json!({}));
    value["running"] = json!(super::jobs::is_running(&job.key));
    value
}

/// What the scheduler is doing. Pollable as well as pushed on the `/ws` tick,
/// because a page that opens mid-refresh needs the current state before the
/// next frame arrives.
async fn list_jobs(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::jobs::list(&state.db)
            .await?
            .iter()
            .map(serialize_job)
            .collect(),
    )))
}

async fn get_job(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let job = db::jobs::by_key(&state.db, &key)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(serialize_job(&job)))
}

/// Cooperative: the handler observes the token at its next checkpoint, so a
/// long download finishes its current chunk first. 404 when nothing by that key
/// is running, which is the honest answer to cancelling something already done.
async fn cancel_job(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    db::jobs::by_key(&state.db, &key)
        .await?
        .ok_or(Error::NotFound)?;

    if !super::jobs::cancel(&key) {
        return Err(Error::Conflict("that job is not running".into()).into());
    }
    Ok(Json(json!({ "cancelling": true })))
}

// ------------------------------------------------------------- user agents

#[derive(Deserialize)]
struct UserAgentBody {
    name: Option<String>,
    user_agent: Option<String>,
    is_active: Option<bool>,
}

async fn list_user_agents(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::profiles::list_user_agents(&state.db).await?,
    )))
}

async fn get_user_agent(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<UserAgent>> {
    Ok(Json(
        db::profiles::get_user_agent(&state.db, id)
            .await?
            .ok_or(Error::NotFound)?,
    ))
}

async fn create_user_agent(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<UserAgentBody>,
) -> ApiResult<(StatusCode, Json<UserAgent>)> {
    let (Some(name), Some(user_agent)) = (body.name, body.user_agent) else {
        return Err(Error::invalid("name and user_agent are required").into());
    };

    let created = db::profiles::create_user_agent(
        &state.db,
        &UserAgent {
            id: 0,
            name,
            user_agent,
            is_active: body.is_active.unwrap_or(true),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn update_user_agent(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<UserAgentBody>,
) -> ApiResult<Json<UserAgent>> {
    let mut agent = db::profiles::get_user_agent(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;

    if let Some(name) = body.name {
        agent.name = name;
    }
    if let Some(value) = body.user_agent {
        agent.user_agent = value;
    }
    if let Some(active) = body.is_active {
        agent.is_active = active;
    }

    Ok(Json(
        db::profiles::save_user_agent(&state.db, &agent).await?,
    ))
}

async fn delete_user_agent(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::profiles::delete_user_agent(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------- stream profiles

#[derive(Deserialize)]
struct StreamProfileBody {
    name: Option<String>,
    command: Option<String>,
    parameters: Option<String>,
    is_active: Option<bool>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    user_agent_id: Option<Option<Id>>,
}

async fn list_stream_profiles(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::profiles::list_stream_profiles(&state.db).await?,
    )))
}

async fn get_stream_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<StreamProfile>> {
    Ok(Json(
        db::profiles::get_stream_profile(&state.db, id)
            .await?
            .ok_or(Error::NotFound)?,
    ))
}

async fn create_stream_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<StreamProfileBody>,
) -> ApiResult<(StatusCode, Json<StreamProfile>)> {
    let Some(name) = body.name else {
        return Err(Error::invalid("name is required").into());
    };

    let created = db::profiles::create_stream_profile(
        &state.db,
        &StreamProfile {
            id: 0,
            name,
            command: body.command.unwrap_or_default(),
            parameters: body.parameters.unwrap_or_default(),
            locked: false,
            is_active: body.is_active.unwrap_or(true),
            user_agent_id: body.user_agent_id.flatten(),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn update_stream_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<StreamProfileBody>,
) -> ApiResult<Json<StreamProfile>> {
    let mut profile = db::profiles::get_stream_profile(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;

    // A locked profile's command is what the proxy branches on, so only the
    // switch that turns it off is editable.
    if profile.locked {
        if let Some(active) = body.is_active {
            profile.is_active = active;
        }
        return Ok(Json(
            db::profiles::save_stream_profile(&state.db, &profile).await?,
        ));
    }

    if let Some(name) = body.name {
        profile.name = name;
    }
    if let Some(command) = body.command {
        profile.command = command;
    }
    if let Some(parameters) = body.parameters {
        profile.parameters = parameters;
    }
    if let Some(active) = body.is_active {
        profile.is_active = active;
    }
    if let Some(agent) = body.user_agent_id {
        profile.user_agent_id = agent;
    }

    Ok(Json(
        db::profiles::save_stream_profile(&state.db, &profile).await?,
    ))
}

async fn delete_stream_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::profiles::delete_stream_profile(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------- output profiles

#[derive(Deserialize)]
struct OutputProfileBody {
    name: Option<String>,
    command: Option<String>,
    parameters: Option<String>,
    is_active: Option<bool>,
}

async fn list_output_profiles(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        db::profiles::list_output_profiles(&state.db).await?,
    )))
}

async fn get_output_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<OutputProfile>> {
    Ok(Json(
        db::profiles::get_output_profile(&state.db, id)
            .await?
            .ok_or(Error::NotFound)?,
    ))
}

async fn create_output_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<OutputProfileBody>,
) -> ApiResult<(StatusCode, Json<OutputProfile>)> {
    let Some(name) = body.name else {
        return Err(Error::invalid("name is required").into());
    };

    let created = db::profiles::create_output_profile(
        &state.db,
        &OutputProfile {
            id: 0,
            name,
            command: body.command.unwrap_or_else(|| "ffmpeg".into()),
            parameters: body.parameters.unwrap_or_default(),
            locked: false,
            is_active: body.is_active.unwrap_or(true),
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn update_output_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<OutputProfileBody>,
) -> ApiResult<Json<OutputProfile>> {
    let mut profile = db::profiles::get_output_profile(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;

    if profile.locked {
        if let Some(active) = body.is_active {
            profile.is_active = active;
        }
        return Ok(Json(
            db::profiles::save_output_profile(&state.db, &profile).await?,
        ));
    }

    if let Some(name) = body.name {
        profile.name = name;
    }
    if let Some(command) = body.command {
        profile.command = command;
    }
    if let Some(parameters) = body.parameters {
        profile.parameters = parameters;
    }
    if let Some(active) = body.is_active {
        profile.is_active = active;
    }

    Ok(Json(
        db::profiles::save_output_profile(&state.db, &profile).await?,
    ))
}

async fn delete_output_profile(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::profiles::delete_output_profile(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
