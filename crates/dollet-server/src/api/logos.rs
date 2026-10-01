//! Channel artwork.

use std::collections::HashMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use dollet_core::domain::{Id, Logo};
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::ApiResult;
use super::outputs::{self, Output};
use super::{listing, paging};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/logos/", get(list).post(create))
        .route("/logos/bulk-delete/", post(bulk_delete))
        .route("/logos/cleanup/", post(cleanup))
        .route(
            "/logos/{id}/",
            get(get_logo).patch(update).put(update).delete(delete_logo),
        )
}

#[derive(Deserialize, Default)]
struct ListQuery {
    search: Option<String>,
    ordering: Option<String>,
    page: Option<u32>,
    page_size: Option<u32>,
    /// `true` asks for the whole list. One gesture, every list endpoint.
    all: Option<String>,
}

impl ListQuery {
    fn all(&self) -> bool {
        super::wants_all(self.all.as_deref())
    }
}

fn serialize(logo: &Logo, channel_count: i64) -> Value {
    json!({
        "id": logo.id,
        "name": logo.name,
        "url": logo.url,
        "channel_count": channel_count,
        "is_used": channel_count > 0,
    })
}

async fn list(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let paging = paging(query.page, query.page_size, query.all());

    let page = db::logos::list(
        &state.db,
        query.search.as_deref(),
        query.ordering.as_deref(),
        paging,
    )
    .await?;

    let usage: HashMap<Id, i64> = db::logos::usage(&state.db).await?;
    let rows: Vec<Value> = page
        .results
        .iter()
        .map(|logo| serialize(logo, usage.get(&logo.id).copied().unwrap_or(0)))
        .collect();

    Ok(Json(listing(paging, page.count, rows)))
}

async fn get_logo(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Value>> {
    let logo = db::logos::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    let usage = db::logos::usage(&state.db).await?;
    Ok(Json(serialize(
        &logo,
        usage.get(&logo.id).copied().unwrap_or(0),
    )))
}

#[derive(Deserialize)]
struct LogoBody {
    name: Option<String>,
    url: Option<String>,
}

async fn create(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<LogoBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Some(url) = body.url.filter(|url| !url.trim().is_empty()) else {
        return Err(Error::invalid("url is required").into());
    };

    let logo = db::logos::create(
        &state.db,
        &Logo {
            id: 0,
            name: body.name.unwrap_or_else(|| url.clone()),
            url,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(serialize(&logo, 0))))
}

async fn update(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<LogoBody>,
) -> ApiResult<Json<Value>> {
    let mut logo = db::logos::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    if let Some(name) = body.name {
        logo.name = name;
    }
    if let Some(url) = body.url {
        logo.url = url;
    }

    let saved = db::logos::save(&state.db, &logo).await?;
    // `tvg-logo` in the playlist and `<icon>` in the guide both resolve through
    // this row, verbatim when the request asked for `cachedlogos=false`. A
    // logo nothing points at costs one needless re-render, which is cheaper
    // than working out whether anything does.
    outputs::invalidate(&state, &Output::BOTH).await;

    let usage = db::logos::usage(&state.db).await?;
    Ok(Json(serialize(
        &saved,
        usage.get(&saved.id).copied().unwrap_or(0),
    )))
}

async fn delete_logo(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::logos::delete(&state.db, id).await?;
    // Every channel holding it drops to no artwork at all.
    outputs::invalidate(&state, &Output::BOTH).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn bulk_delete(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<super::IdList>,
) -> ApiResult<Json<Value>> {
    let deleted = db::logos::delete_many(&state.db, &body.ids).await?;
    if deleted > 0 {
        outputs::invalidate(&state, &Output::BOTH).await;
    }
    Ok(Json(json!({ "deleted": deleted })))
}

/// Drop every logo no channel references, counting override assignments.
async fn cleanup(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({ "deleted": db::logos::delete_unused(&state.db).await? }),
    ))
}
