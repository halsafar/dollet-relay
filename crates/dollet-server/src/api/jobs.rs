//! The background scheduler.
//!
//! One budget of slots and a per-key guard, because two refreshes of the same
//! account would reconcile against each other's half-written state.
//!
//! Nothing here is a work queue in the distributed sense: there is one process,
//! so a permit and a `DashMap` entry do what a broker did, and the persisted
//! row is what survives a restart.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use dashmap::DashMap;
use dollet_core::db::jobs::{self, Job, State};
use dollet_core::{Error, db};
use futures_util::FutureExt;
use serde_json::Value as Json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::AppState;

/// How many jobs may run at once.
///
/// Wide because every one of them is a provider refresh waiting on someone
/// else's HTTP, not on this process. One number rather than a pool per class:
/// a second pool would exist so a three-hour ingest cannot occupy a slot a
/// user-facing action needs, and there are no user-facing jobs here to
/// protect.
const CONCURRENT_JOBS: usize = 20;

/// How often the scheduler looks for work that has come due. Finer than this
/// buys nothing: the shortest interval anything here uses is an hour.
const TICK: Duration = Duration::from_secs(15);

/// A unit of work, resolved by kind at dispatch time.
pub type Handler = Arc<
    dyn Fn(AppState, Job, JobHandle) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send>>
        + Send
        + Sync,
>;

/// What a running job uses to report on itself.
#[derive(Clone)]
pub struct JobHandle {
    db: sqlx::SqlitePool,
    key: String,
    pub cancel: CancellationToken,
}

impl JobHandle {
    pub async fn progress(&self, fraction: f64, message: impl AsRef<str>) {
        // A failure to record progress must not fail the job it describes.
        if let Err(e) = jobs::progress(&self.db, &self.key, fraction, message.as_ref()).await {
            tracing::debug!(key = %self.key, error = %e, "progress not recorded");
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

struct Scheduler {
    slots: Arc<Semaphore>,
    /// Keys currently executing, and the token that stops them. Also the
    /// single-flight guard: an entry here means "already running".
    running: DashMap<String, CancellationToken>,
}

static SCHEDULER: LazyLock<Scheduler> = LazyLock::new(|| Scheduler {
    slots: Arc::new(Semaphore::new(CONCURRENT_JOBS)),
    running: DashMap::new(),
});

/// Handlers by `kind`. Registered at startup so `trigger` can dispatch a row
/// read out of the database without the caller naming a function.
///
/// Merged rather than set once: a second `register` silently losing its
/// handlers would make dispatch depend on which caller ran first, and the
/// symptom — "no handler for job kind" on a job that plainly has one — points
/// nowhere near the cause.
static HANDLERS: LazyLock<DashMap<&'static str, Handler>> = LazyLock::new(DashMap::new);

pub fn register(handlers: HashMap<&'static str, Handler>) {
    for (kind, handler) in handlers {
        HANDLERS.insert(kind, handler);
    }
}

/// Wrap an async fn into the shape [`register`] takes.
pub fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(AppState, Job, JobHandle) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, Error>> + Send + 'static,
{
    Arc::new(move |state, job, handle| Box::pin(f(state, job, handle)))
}

/// Start the scheduler loop and register everything schedulable.
///
/// Returns immediately; the loop lives for the process.
pub async fn start(state: AppState) -> Result<(), Error> {
    // Anything still marked running belonged to a process that is gone. Left
    // alone, the single-flight guard would refuse those jobs forever.
    let released = jobs::release_orphans(&state.db).await?;
    if released > 0 {
        tracing::warn!(released, "jobs were interrupted by a restart");
    }

    register(super::ingest::handlers());
    register(super::backups::handlers());
    sync_schedule(&state).await?;

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(e) = tick(&state).await {
                tracing::warn!(error = %e, "scheduler tick failed");
            }
        }
    });

    Ok(())
}

/// Register one job per active provider account and EPG source, and the
/// scheduled backup, and drop the rows for anything that no longer exists.
pub async fn sync_schedule(state: &AppState) -> Result<(), Error> {
    let mut wanted: Vec<String> = Vec::new();

    let backup: dollet_core::settings::BackupSettings =
        dollet_core::settings::load(&state.db).await?;
    jobs::ensure(
        &state.db,
        super::backups::JOB_KEY,
        super::backups::KIND,
        &serde_json::json!({}),
        (backup.interval_hours > 0).then(|| i64::from(backup.interval_hours) * 3600),
    )
    .await?;
    wanted.push(super::backups::JOB_KEY.to_owned());

    for source in db::epg::list_sources(&state.db).await? {
        let key = super::ingest::epg::job_key(source.id);
        let interval = (source.is_active && source.refresh_interval_hours > 0)
            .then(|| i64::from(source.refresh_interval_hours) * 3600);
        jobs::ensure(
            &state.db,
            &key,
            super::ingest::epg::KIND,
            &serde_json::json!({ "epg_source_id": source.id }),
            interval,
        )
        .await?;
        wanted.push(key);
    }

    for account in db::m3u::list_accounts(&state.db).await? {
        // The built-in `custom` account holds hand-added streams and has no
        // provider to refresh from.
        if account.locked {
            continue;
        }
        let key = super::ingest::m3u::job_key(account.id);
        let interval = (account.is_active && account.refresh_interval_hours > 0)
            .then(|| i64::from(account.refresh_interval_hours) * 3600);
        jobs::ensure(
            &state.db,
            &key,
            super::ingest::m3u::KIND,
            &serde_json::json!({ "m3u_account_id": account.id }),
            interval,
        )
        .await?;
        wanted.push(key);
    }

    // Only the kinds this function is responsible for. It reaps every row it
    // does not recognise, and it runs on every EPG source and provider account
    // save — so anything else that ever models itself as a job (a DVR recording
    // is the obvious one) would disappear the next time the operator edited a
    // source, with no error and no log line.
    let owned = [
        super::ingest::epg::KIND,
        super::ingest::m3u::KIND,
        super::backups::KIND,
    ];
    for job in jobs::list(&state.db).await? {
        if owned.contains(&job.kind.as_str()) && !wanted.contains(&job.key) {
            jobs::remove(&state.db, &job.key).await?;
        }
    }
    Ok(())
}

async fn tick(state: &AppState) -> Result<(), Error> {
    for job in jobs::due(&state.db, chrono::Utc::now()).await? {
        spawn(state.clone(), job);
    }
    Ok(())
}

/// Run a job now, if it is not already running.
///
/// Returns false when something else holds it, which is the honest answer to a
/// user pressing refresh twice.
pub fn trigger(state: &AppState, job: Job) -> bool {
    spawn(state.clone(), job)
}

/// Start the job under `key` now, for a refresh button. 409 when it is
/// already running, which is the honest answer to pressing the button twice.
pub async fn start_now(state: &AppState, key: &str) -> Result<Json, Error> {
    let job = jobs::by_key(&state.db, key).await?.ok_or(Error::NotFound)?;
    if !trigger(state, job) {
        return Err(Error::Conflict("a refresh is already running".into()));
    }
    Ok(serde_json::json!({ "started": true, "job": key }))
}

/// Start every job under `keys` that is not already running, and say which.
pub async fn start_each(
    state: &AppState,
    keys: impl IntoIterator<Item = String>,
) -> Result<Vec<String>, Error> {
    let mut started = Vec::new();
    for key in keys {
        if let Some(job) = jobs::by_key(&state.db, &key).await?
            && trigger(state, job)
        {
            started.push(key);
        }
    }
    Ok(started)
}

fn spawn(state: AppState, job: Job) -> bool {
    let key = job.key.clone();
    let cancel = CancellationToken::new();

    // The map entry *is* the single-flight guard: occupied means running.
    // Claimed through the entry API rather than `insert`, which would replace
    // the running job's cancellation token with one nothing holds — and taken
    // before the permit, so duplicates are refused rather than queued behind it.
    match SCHEDULER.running.entry(key.clone()) {
        dashmap::mapref::entry::Entry::Occupied(_) => {
            tracing::debug!(%key, "already running");
            return false;
        }
        dashmap::mapref::entry::Entry::Vacant(slot) => {
            slot.insert(cancel.clone());
        }
    }

    let semaphore = Arc::clone(&SCHEDULER.slots);

    tokio::spawn(async move {
        let _permit = semaphore.acquire_owned().await;
        // Releases the single-flight guard on every exit path, unwinding
        // included. A bare `running.remove` after the await would not: a panic
        // in a handler skips the statements that follow it, leaving the key held
        // by a task that no longer exists, so `trigger` answers 409 and `due`
        // skips the row until the process restarts.
        let _guard = Running(job.key.clone());

        // A panic is a failed job, not a lost one. Without this the persisted
        // row stays `Running` for the same reason, and nothing but
        // `release_orphans` at the next boot ever clears it.
        let outcome = match std::panic::AssertUnwindSafe(run(&state, &job, cancel.clone()))
            .catch_unwind()
            .await
        {
            Ok(outcome) => outcome,
            Err(payload) => {
                tracing::error!(key = %job.key, panic = %panic_message(&payload), "job panicked");
                Err(Error::Other(anyhow::anyhow!(
                    "job panicked: {}",
                    panic_message(&payload)
                )))
            }
        };

        let (result, message, error) = match outcome {
            Ok(summary) => (State::Success, Some(summary), None),
            Err(e) if cancel.is_cancelled() => (State::Cancelled, None, Some(scrubbed(&e))),
            Err(e) => {
                tracing::warn!(key = %job.key, error = %scrubbed(&e), "job failed");
                (State::Failed, None, Some(scrubbed(&e)))
            }
        };

        if let Err(e) = jobs::finish(
            &state.db,
            &job.key,
            result,
            message.as_deref(),
            error.as_deref(),
        )
        .await
        {
            tracing::warn!(key = %job.key, error = %e, "job outcome not recorded");
        }
    });

    true
}

/// Holds a key in the single-flight map for as long as it is alive.
struct Running(String);

impl Drop for Running {
    fn drop(&mut self) {
        SCHEDULER.running.remove(&self.0);
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_owned())
}

/// Job errors are persisted and served, so a provider URL in one carries the
/// provider password with it.
fn scrubbed(error: &Error) -> String {
    super::ingest::scrub(&error.to_string())
}

async fn run(state: &AppState, job: &Job, cancel: CancellationToken) -> Result<String, Error> {
    if !jobs::claim(&state.db, &job.key).await? {
        return Err(Error::Conflict("job is already running".into()));
    }

    let handler = HANDLERS
        .get(job.kind.as_str())
        .ok_or_else(|| Error::invalid(format!("no handler for job kind `{}`", job.kind)))?
        .clone();

    let handle = JobHandle {
        db: state.db.clone(),
        key: job.key.clone(),
        cancel: cancel.clone(),
    };

    // Awaited directly, never raced against the token. A `select!` on the two
    // returns as soon as cancellation fires and *drops the handler's future
    // wherever it happens to be suspended* — mid-batch, between a delete and
    // the insert that was going to replace it. Every cooperative
    // `handle.cancelled()` check in the handlers is unreachable under that
    // race, and the guide truncates.
    //
    // So cancellation is what the handlers already document it to be: a flag
    // they poll at a point where stopping is safe. A handler that never polls
    // it runs to completion, which is the correct trade against tearing a
    // half-written catalogue.
    handler(state.clone(), job.clone(), handle).await
}

/// Stop a running job. Cooperative: the handler observes the token.
pub fn cancel(key: &str) -> bool {
    match SCHEDULER.running.get(key) {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}

pub fn is_running(key: &str) -> bool {
    SCHEDULER.running.contains_key(key)
}

/// Longest a shutdown waits for in-flight jobs before leaving without them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// Ask every running job to stop, and wait for them.
///
/// Without this, a SIGTERM mid-refresh kills the process at whatever await the
/// handler was sitting on. The staged-programme swap keeps a guide whole
/// through that, but the job row is still left saying `Running` until the next
/// boot's `release_orphans`, and a half-applied stream plan is half applied.
/// Signalling and waiting lets each handler stop at the checkpoint it already
/// polls.
///
/// Bounded, because a container runtime will send SIGKILL soon enough anyway
/// and an unbounded wait only chooses which signal ends the process.
pub async fn shutdown() {
    let keys: Vec<String> = SCHEDULER
        .running
        .iter()
        .map(|entry| {
            entry.value().cancel();
            entry.key().clone()
        })
        .collect();
    if keys.is_empty() {
        return;
    }

    tracing::info!(jobs = keys.len(), "waiting for jobs to stop");
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    while !SCHEDULER.running.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let stragglers: Vec<String> = SCHEDULER
        .running
        .iter()
        .map(|entry| entry.key().clone())
        .collect();
    if !stragglers.is_empty() {
        tracing::warn!(
            jobs = ?stragglers,
            "jobs did not stop within the grace period and are being left behind"
        );
    }
}

/// A handle that reports progress against a real job row, for tests that drive
/// a refresh directly rather than through the scheduler.
#[cfg(test)]
pub fn test_handle(state: &AppState, key: &str) -> JobHandle {
    JobHandle {
        db: state.db.clone(),
        key: key.to_owned(),
        cancel: CancellationToken::new(),
    }
}

/// Payload accessor shared by the handlers.
pub fn payload_id(job: &Job, field: &str) -> Result<dollet_core::domain::Id, Error> {
    job.payload
        .get(field)
        .and_then(Json::as_i64)
        .ok_or_else(|| Error::invalid(format!("job `{}` has no {field}", job.key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelling_something_that_is_not_running_is_not_an_error() {
        assert!(!cancel("nothing:0"));
        assert!(!is_running("nothing:0"));
    }
}
