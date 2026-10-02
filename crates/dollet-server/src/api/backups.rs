//! Backups of this instance: taken by hand or on a schedule, downloaded,
//! uploaded, deleted, and restored by restarting into one.
//!
//! Admin-only throughout. A backup is the whole database: every password hash,
//! every provider credential and the key that signs sessions.

use std::collections::HashMap;

use axum::Json;
use axum::RequestExt;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use dollet_core::Error;
use dollet_core::backup::{self, BackupName, Entry, Trigger};
use dollet_core::db::jobs::Job;
use dollet_core::db::notifications::{self, Notification, Severity};
use dollet_core::settings::{self, BackupSettings};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;

use super::auth::AdminUser;
use super::error::{ApiError, ApiResult};
use super::jobs::{Handler, JobHandle};
use super::whole_list;
use crate::AppState;

pub const KIND: &str = "backup";
pub const JOB_KEY: &str = "backup";

const FAILED: &str = "backup_failed";

/// Held for as long as a backup is being written, and for the instant an
/// upload is named and moved into place. A second backup started alongside
/// the first would double the disk and the database read for nothing, and two
/// writers choosing a name at once could choose the same one.
pub(super) static RUNNING: Mutex<()> = Mutex::const_new(());

/// What an upload may weigh. Room for a large instance's database, which
/// compresses well; past it, an upload is not a backup of this program.
/// Arbitrary beyond that.
const MAX_UPLOAD_BYTES: usize = 2 * 1024 * 1024 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/backups/", get(list).post(create))
        // The only route that takes more than axum's 2 MB default, which would
        // refuse every real backup.
        .route(
            "/backups/upload/",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/backups/{name}/", delete(remove))
        .route("/backups/{name}/download/", get(download))
        .route("/backups/{name}/restore/", post(restore))
}

pub fn handlers() -> HashMap<&'static str, Handler> {
    HashMap::from([(KIND, super::jobs::handler(run))])
}

async fn list(State(state): State<AppState>, _admin: AdminUser) -> ApiResult<Json<Value>> {
    Ok(Json(whole_list(
        backup::list(&state.config.backups_dir()).await?,
    )))
}

/// 409 while another backup is being written, which is the honest answer to
/// pressing the button twice.
async fn create(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> ApiResult<(StatusCode, Json<Entry>)> {
    let Ok(_running) = RUNNING.try_lock() else {
        return Err(Error::Conflict("a backup is already running".into()).into());
    };
    let entry = backup::take(&state.db, &state.config.backups_dir(), Trigger::Manual).await?;
    Ok((StatusCode::CREATED, Json(entry)))
}

async fn download(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    let path = backup::resolve(&state.config.backups_dir(), &name).ok_or(Error::NotFound)?;
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound.into()),
        Err(e) => return Err(Error::from(e).into()),
    };
    let size = file.metadata().await.map_err(Error::from)?.len();

    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
            // The name has been parsed, so it carries nothing that could close
            // the quotes.
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        Body::from_stream(ReaderStream::with_capacity(file, 64 * 1024)),
    )
        .into_response())
}

async fn remove(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let path = backup::resolve(&state.config.backups_dir(), &name).ok_or(Error::NotFound)?;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound.into()),
        Err(e) => Err(Error::from(e).into()),
    }
}

/// The body is the zip itself, so a browser posts a `File` without a
/// multipart envelope to unwrap. Written to disk as it arrives and checked
/// there, never held in memory.
async fn upload(
    State(state): State<AppState>,
    _admin: AdminUser,
    request: Request,
) -> ApiResult<(StatusCode, Json<Entry>)> {
    let dir = state.config.backups_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(Error::from)?;

    let part = backup::Scratch::new(&dir, "part");
    let file = tokio::fs::File::create_new(part.path())
        .await
        .map_err(Error::from)?;
    let mut writer = tokio::io::BufWriter::with_capacity(256 * 1024, file);
    // Limited by the route's `DefaultBodyLimit`, which the plain `Body`
    // extractor would ignore.
    let mut body = request.into_limited_body().into_data_stream();
    while let Some(chunk) = body.next().await {
        let chunk =
            chunk.map_err(|e| Error::invalid(format!("the upload did not arrive whole: {e}")))?;
        writer.write_all(&chunk).await.map_err(Error::from)?;
    }
    writer.shutdown().await.map_err(Error::from)?;

    let checked = backup::validate(part.path(), &dir).await?;
    drop(checked.database);
    let name = BackupName::new(checked.metadata.created_at, Trigger::Uploaded);

    let _running = RUNNING.lock().await;
    let destination = dir.join(name.to_string());
    if tokio::fs::try_exists(&destination)
        .await
        .map_err(Error::from)?
    {
        return Err(Error::Conflict(format!("{name} is already here")).into());
    }
    part.persist(&destination).map_err(Error::from)?;
    Ok((StatusCode::CREATED, Json(backup::entry(&dir, name).await?)))
}

/// Stage a backup to replace this instance's database, and restart into it.
///
/// Checked first, so a backup this build could not boot on is refused while
/// the current database is still in place. Then a backup of what is about to
/// be replaced, so a wrong restore is undone from the same list; if that cannot
/// be written, nothing is restored.
async fn restore(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(name): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let path = backup::resolve(&state.config.backups_dir(), &name).ok_or(Error::NotFound)?;
    if !tokio::fs::try_exists(&path).await.map_err(Error::from)? {
        return Err(Error::NotFound.into());
    }

    // Extracted into the data directory, so staging it is a rename beside the
    // database rather than a copy from wherever `backups/` is mounted.
    let checked = backup::validate(&path, &state.config.data_dir).await?;

    let _running = RUNNING.lock().await;
    let safety = backup::take(&state.db, &state.config.backups_dir(), Trigger::PreRestore)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "the pre-restore backup failed; nothing was restored");
            ApiError::Status(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "the backup of this instance could not be written, so nothing was restored: {e}"
                ),
            )
        })?;

    checked
        .database
        .persist(&state.config.staged_restore_path())
        .map_err(Error::from)?;
    tracing::info!(
        restored = %name,
        pre_restore = %safety.name,
        "restore staged; restarting to apply it"
    );
    crate::restart::request();

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "restarting": true, "pre_restore": safety.name })),
    ))
}

async fn run(state: AppState, _job: Job, _handle: JobHandle) -> Result<String, Error> {
    scheduled(&state).await
}

/// One scheduled backup and the retention after it.
///
/// A failure is raised on the bell rather than left in the job row: a backup
/// that has silently stopped is found on the day it is needed.
pub(super) async fn scheduled(state: &AppState) -> Result<String, Error> {
    let keep = settings::load::<BackupSettings>(&state.db).await?.keep;
    let dir = state.config.backups_dir();

    let outcome = async {
        let _running = RUNNING.lock().await;
        let written = backup::take(&state.db, &dir, Trigger::Scheduled).await?;
        let removed = backup::prune(&dir, keep as usize).await?;
        Ok::<_, Error>((written, removed))
    }
    .await;

    match outcome {
        Ok((written, removed)) => {
            notifications::clear(&state.db, FAILED, JOB_KEY).await?;
            Ok(match removed.len() {
                0 => format!("wrote {}", written.name),
                1 => format!("wrote {}; removed 1 older scheduled backup", written.name),
                n => format!(
                    "wrote {}; removed {n} older scheduled backups",
                    written.name
                ),
            })
        }
        Err(e) => {
            let raised = notifications::raise(
                &state.db,
                &Notification::new(
                    FAILED,
                    JOB_KEY,
                    Severity::Error,
                    "Scheduled backup failed",
                    format!(
                        "The scheduled backup could not be written: {e}. The backups already \
                         taken are untouched; Settings → Backups lists them and can take one \
                         by hand."
                    ),
                    json!({ "error": e.to_string() }),
                ),
            )
            .await;
            if let Err(failure) = raised {
                tracing::warn!(error = %failure, "backup failure not raised as a notification");
            }
            Err(e)
        }
    }
}
