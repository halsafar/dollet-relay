//! Provider streams.

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use dollet_core::db::streams::StreamFilter;
use dollet_core::domain::{Id, Stream};
use dollet_core::{Error, db};
use serde::Deserialize;
use serde_json::{Value, json};

use super::auth::AdminUser;
use super::error::ApiResult;
use super::outputs::{self, Output};
use super::{listing, paging, refuse_all};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/streams/", get(list).post(create))
        .route("/streams/bulk-delete/", post(bulk_delete))
        .route(
            "/streams/{id}/",
            get(get_stream)
                .patch(update)
                .put(update)
                .delete(delete_stream),
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
    channel_group: Option<Id>,
    m3u_account: Option<Id>,
}

impl ListQuery {
    fn all(&self) -> bool {
        super::wants_all(self.all.as_deref())
    }
}

async fn list(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let filter = StreamFilter {
        search: query.search.as_deref(),
        group_id: query.channel_group,
        m3u_account_id: query.m3u_account,
    };

    // The one list that refuses `?all=true`: it is the table that can hold tens
    // of thousands of rows, and a silent cap is how a client comes to believe
    // it has everything.
    if query.all() {
        return Err(refuse_all("/api/channels/streams/").into());
    }

    let paging = paging(query.page, query.page_size, false);
    let page = db::streams::list(&state.db, &filter, query.ordering.as_deref(), paging).await?;

    Ok(Json(listing(paging, page.count, page.results)))
}

async fn get_stream(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<Json<Stream>> {
    Ok(Json(
        db::streams::get(&state.db, id)
            .await?
            .ok_or(Error::NotFound)?,
    ))
}

#[derive(Deserialize, Default)]
struct StreamBody {
    name: Option<String>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    url: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    logo_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    tvg_id: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    channel_group_id: Option<Option<Id>>,
    #[serde(default, deserialize_with = "super::explicit_null")]
    stream_profile_id: Option<Option<Id>>,
    is_adult: Option<bool>,
    is_catchup: Option<bool>,
    catchup_days: Option<u32>,
    custom_properties: Option<Value>,
}

impl StreamBody {
    fn apply(self, stream: &mut Stream) {
        if let Some(name) = self.name {
            stream.name = name;
        }
        if let Some(url) = self.url {
            stream.url = url;
        }
        if let Some(logo) = self.logo_url {
            stream.logo_url = logo;
        }
        if let Some(tvg_id) = self.tvg_id {
            stream.tvg_id = tvg_id;
        }
        if let Some(group) = self.channel_group_id {
            stream.channel_group_id = group;
        }
        if let Some(profile) = self.stream_profile_id {
            stream.stream_profile_id = profile;
        }
        if let Some(adult) = self.is_adult {
            stream.is_adult = adult;
        }
        if let Some(catchup) = self.is_catchup {
            stream.is_catchup = catchup;
        }
        if let Some(days) = self.catchup_days {
            stream.catchup_days = days;
        }
        if let Some(properties) = self.custom_properties {
            stream.custom_properties = properties;
        }
    }
}

/// Hand-added streams belong to the locked `custom` account, which M3U refresh
/// skips — otherwise the next refresh would mark them stale and delete them.
const CUSTOM_ACCOUNT: Id = 1;

async fn create(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<StreamBody>,
) -> ApiResult<(StatusCode, Json<Stream>)> {
    let mut stream = Stream {
        id: 0,
        name: String::new(),
        url: None,
        logo_url: None,
        tvg_id: None,
        channel_group_id: None,
        m3u_account_id: Some(CUSTOM_ACCOUNT),
        stream_profile_id: None,
        is_custom: true,
        is_adult: false,
        stream_id: None,
        stream_chno: None,
        stream_hash: None,
        last_seen: chrono::Utc::now(),
        is_stale: false,
        is_catchup: false,
        catchup_days: 0,
        custom_properties: json!({}),
    };
    body.apply(&mut stream);

    if stream.name.trim().is_empty() {
        return Err(Error::invalid("name is required").into());
    }
    if stream.url.as_deref().unwrap_or_default().is_empty() {
        return Err(Error::invalid("url is required for a custom stream").into());
    }

    Ok((
        StatusCode::CREATED,
        Json(db::streams::create(&state.db, &stream).await?),
    ))
}

async fn update(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
    Json(body): Json<StreamBody>,
) -> ApiResult<Json<Stream>> {
    let mut stream = db::streams::get(&state.db, id)
        .await?
        .ok_or(Error::NotFound)?;
    body.apply(&mut stream);
    let saved = db::streams::save(&state.db, &stream).await?;
    // A URL edit changes what `?direct=true` hands the client for whichever
    // channel has this stream first. Creating one changes nothing until a
    // channel points at it, and that write invalidates on its own.
    outputs::invalidate(&state, &[Output::Playlist]).await;
    Ok(Json(saved))
}

async fn delete_stream(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Id>,
) -> ApiResult<StatusCode> {
    db::streams::delete(&state.db, id).await?;
    outputs::invalidate(&state, &[Output::Playlist]).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn bulk_delete(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<super::IdList>,
) -> ApiResult<Json<Value>> {
    let deleted = db::streams::delete_many(&state.db, &body.ids).await?;
    if deleted > 0 {
        outputs::invalidate(&state, &[Output::Playlist]).await;
    }
    Ok(Json(json!({ "deleted": deleted })))
}
