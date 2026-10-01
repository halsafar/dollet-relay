//! User administration.
//!
//! `user_level` is emitted as the integer the database stores, not as the
//! lowercase name [`UserLevel`]'s `Serialize` would produce: the UI compares
//! it numerically.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;
use dollet_core::auth::{level_from_i64, level_value, password};
use dollet_core::domain::{Id, User, UserLevel};
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::{AdminUser, CurrentUser};
use super::error::ApiResult;
use super::whole_list;
use crate::AppState;

pub fn serialize(user: &User) -> Value {
    json!({
        "id": user.id,
        "username": user.username,
        "email": user.email,
        "user_level": level_value(user.user_level),
        "api_key": user.api_key,
        "stream_limit": user.stream_limit,
        "channel_profiles": user.channel_profile_ids,
        "custom_properties": user.custom_properties,
        "is_active": user.is_active,
        // Imported users carried these as separate flags; here they are a view of the
        // level, so they can never disagree with it.
        "is_staff": user.user_level >= UserLevel::Admin,
        "is_superuser": user.user_level >= UserLevel::Admin,
    })
}

#[derive(Deserialize)]
struct UserPatch {
    username: Option<String>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    email: Option<Option<String>>,
    password: Option<String>,
    user_level: Option<i64>,
    stream_limit: Option<i32>,
    channel_profiles: Option<Vec<Id>>,
    custom_properties: Option<Value>,
    is_active: Option<bool>,
}

impl UserPatch {
    /// `async` only because hashing a new password runs on a blocking thread;
    /// everything else here is a field assignment.
    async fn apply(self, user: &mut User) {
        if let Some(username) = self.username {
            user.username = username;
        }
        if let Some(email) = self.email {
            user.email = email;
        }
        if let Some(raw) = self.password {
            user.password_hash = password::hash_offthread(&raw).await;
        }
        if let Some(level) = self.user_level {
            user.user_level = level_from_i64(level);
        }
        if let Some(limit) = self.stream_limit {
            user.stream_limit = limit.max(0);
        }
        if let Some(profiles) = self.channel_profiles {
            user.channel_profile_ids = profiles;
        }
        // Merged rather than replaced: the UI PATCHes one preference at a time
        // and would otherwise wipe the rest of the object.
        if let Some(Value::Object(incoming)) = self.custom_properties {
            let mut merged = match user.custom_properties.take() {
                Value::Object(existing) => existing,
                _ => Default::default(),
            };
            for (key, value) in incoming {
                if value.is_null() {
                    merged.remove(&key);
                } else {
                    merged.insert(key, value);
                }
            }
            user.custom_properties = Value::Object(merged);
        }
        if let Some(active) = self.is_active {
            user.is_active = active;
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/users/", get(list).post(create))
        .route("/users/me/", get(me).patch(patch_me))
        .route(
            "/users/{id}/",
            get(get_user).patch(update).put(update).delete(delete_user),
        )
}

async fn list(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    let users = db::users::list(&state.db).await?;
    Ok(Json(whole_list(users.iter().map(serialize).collect())))
}

async fn get_user(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let user = db::users::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(serialize(&user)))
}

async fn me(user: CurrentUser) -> Json<Value> {
    Json(serialize(&user.0))
}

/// Self-service edits. Deliberately narrower than the admin path: a streamer
/// PATCHing their own `user_level` would be a privilege escalation.
async fn patch_me(
    State(state): State<AppState>,
    user: CurrentUser,
    Json(patch): Json<UserPatch>,
) -> ApiResult<Json<Value>> {
    let mut updated = user.0;
    UserPatch {
        user_level: None,
        stream_limit: None,
        is_active: None,
        channel_profiles: None,
        ..patch
    }
    .apply(&mut updated)
    .await;

    Ok(Json(serialize(
        &db::users::save(&state.db, &updated).await?,
    )))
}

#[derive(Deserialize)]
struct NewUser {
    username: String,
    password: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    user_level: Option<i64>,
    #[serde(default)]
    stream_limit: Option<i32>,
    #[serde(default)]
    channel_profiles: Vec<Id>,
    /// An account can be created already disabled, so a batch of users can be
    /// set up before any of them can sign in.
    #[serde(default)]
    is_active: Option<bool>,
    /// Carries `xc_password`, the secret the Xtream API authenticates on.
    ///
    /// The Add-user modal sends it; without the field serde drops it silently
    /// and the created user cannot sign in to any Xtream player.
    #[serde(default)]
    custom_properties: Option<Value>,
}

async fn create(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<NewUser>,
) -> ApiResult<(axum::http::StatusCode, Json<Value>)> {
    if body.username.trim().is_empty() || body.password.is_empty() {
        return Err(Error::invalid("username and password are required").into());
    }

    let user = db::users::create(
        &state.db,
        &User {
            id: 0,
            username: body.username,
            email: body.email,
            password_hash: password::hash_offthread(&body.password).await,
            is_active: body.is_active.unwrap_or(true),
            user_level: level_from_i64(body.user_level.unwrap_or(0)),
            api_key: None,
            stream_limit: body.stream_limit.unwrap_or(0).max(0),
            channel_profile_ids: body.channel_profiles,
            // An explicit non-object is ignored rather than stored: this
            // column is read with `json_extract` throughout, which answers
            // NULL for a scalar and would turn a typo into a user whose
            // Xtream password is unset and unsettable.
            custom_properties: body
                .custom_properties
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({})),
        },
    )
    .await?;

    Ok((axum::http::StatusCode::CREATED, Json(serialize(&user))))
}

async fn update(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(patch): Json<UserPatch>,
) -> ApiResult<Json<Value>> {
    let mut user = db::users::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    let was_admin = user.user_level >= UserLevel::Admin;
    patch.apply(&mut user).await;

    if was_admin && !(user.user_level >= UserLevel::Admin && user.is_active) {
        refuse_if_last_admin(&state).await?;
    }

    Ok(Json(serialize(&db::users::save(&state.db, &user).await?)))
}

async fn delete_user(
    State(state): State<AppState>,
    admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<axum::http::StatusCode> {
    if admin.0.id == id {
        return Err(Error::invalid("the signed-in account cannot delete itself").into());
    }

    let user = db::users::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    if user.user_level >= UserLevel::Admin {
        refuse_if_last_admin(&state).await?;
    }

    db::users::delete(&state.db, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// An instance with no active admin cannot be administered, and the only fix
/// is editing the database by hand. Refuse the change instead.
async fn refuse_if_last_admin(state: &AppState) -> Result<(), Error> {
    if db::users::admin_count(&state.db).await? <= 1 {
        return Err(Error::invalid("the last admin account cannot be removed"));
    }
    Ok(())
}
