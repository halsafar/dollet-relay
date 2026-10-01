//! The operator's inbox: conditions a background job hit that need a decision.
//!
//! Admin-only, like the job and event surfaces it sits beside. A notification
//! names a provider account, a group and the pattern that would not compile —
//! deployment facts, not a user's own data.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use dollet_core::domain::Id;
use dollet_core::{Error, db};
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::ApiResult;
use super::whole_list;
use crate::AppState;

/// Merged into `/api` rather than nested under `/notifications`, unlike every
/// other module here.
///
/// The collection itself is the resource, so its route inside a nested router
/// would be `/` — and axum collapses that onto the prefix *without* its
/// trailing slash, so `/api/notifications/` would 404 while `/api/notifications`
/// answered. Every list endpoint in this API ends in a slash and the SPA's
/// client appends one, so the paths are spelled out here instead.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notifications/", get(list))
        .route("/notifications/count/", get(count))
        .route("/notifications/acknowledge-all/", post(acknowledge_all))
        .route("/notifications/{id}/acknowledge/", post(acknowledge))
        .route("/notifications/{id}/", delete(delete_notification))
}

async fn list(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(db::notifications::list(&state.db).await?)))
}

/// The sidebar badge, which asks on every screen and every minute.
///
/// Its own endpoint rather than `list().len()`: the list carries a message and
/// a detail blob per row, and the badge needs one integer.
async fn count(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "unacknowledged": db::notifications::unacknowledged_count(&state.db).await?,
    })))
}

async fn acknowledge(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let acknowledged = db::notifications::acknowledge(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    Ok(Json(json!(acknowledged)))
}

async fn acknowledge_all(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "acknowledged": db::notifications::acknowledge_all(&state.db).await?,
    })))
}

/// Deleting is not acknowledging: the row is gone, so the next refresh that
/// still finds the condition raises it again from one occurrence.
async fn delete_notification(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::notifications::delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
