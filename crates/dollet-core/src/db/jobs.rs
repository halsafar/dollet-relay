//! Scheduler state.
//!
//! Persisted rather than held in memory so a refresh that came due while the
//! process was down still runs, and so a job that was in flight at shutdown is
//! visibly abandoned rather than silently lost. There is no external scheduler
//! to hold this; the table is it.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value as Json;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::Result;
use crate::domain::Id;

use super::{NOW, datetime_column, json_column, sql_timestamp, translate};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Running,
    Success,
    Failed,
    Cancelled,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "running" => Self::Running,
            "success" => Self::Success,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: Id,
    /// Single-flight key, e.g. `m3u_refresh:2`. One row per schedulable unit.
    pub key: String,
    pub kind: String,
    pub payload: Json,
    /// `None` for a job that only ever runs on demand.
    pub interval_seconds: Option<i64>,
    pub state: State,
    /// 0.0 to 1.0.
    pub progress: f64,
    pub message: Option<String>,
    pub next_run_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    /// When this unit last *succeeded*, which is what "last refreshed" means to
    /// the operator: a failed or cancelled run leaves it alone, and a run in
    /// flight leaves it on the one before. `None` until the first success.
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

fn map(row: &SqliteRow) -> Job {
    let optional = |name: &str| -> Option<DateTime<Utc>> {
        row.try_get::<Option<String>, _>(name)
            .ok()
            .flatten()
            .map(|_| datetime_column(row, name))
    };

    Job {
        id: row.get("id"),
        key: row.get("key"),
        kind: row.get("kind"),
        payload: json_column(row, "payload"),
        interval_seconds: row.get("interval_seconds"),
        state: State::parse(row.get("state")),
        progress: row.get("progress"),
        message: row.get("message"),
        next_run_at: optional("next_run_at"),
        started_at: optional("started_at"),
        last_success_at: optional("last_success_at"),
        last_error: row.get("last_error"),
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Job>> {
    let rows = sqlx::query("SELECT * FROM job ORDER BY key")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map).collect())
}

pub async fn by_key(pool: &SqlitePool, key: &str) -> Result<Option<Job>> {
    let row = sqlx::query("SELECT * FROM job WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map))
}

/// Register a schedulable unit, leaving an existing row's runtime state alone.
///
/// Called on every start for every account and EPG source, so it has to be
/// idempotent: the schedule may change, but a job's history and the instant it
/// is next due must survive a restart.
pub async fn ensure(
    db: &SqlitePool,
    key: &str,
    kind: &str,
    payload: &Json,
    interval_seconds: Option<i64>,
) -> Result<Job> {
    // `Utc::now() + Duration::seconds(n)` panics rather than saturating once
    // `n` is large enough to overflow the date, and `n` comes from a
    // user-supplied `refresh_interval_hours`. A panic in a request handler with
    // no catch layer above it drops the connection instead of answering.
    let first_run = interval_seconds
        .and_then(Duration::try_seconds)
        .and_then(|interval| Utc::now().checked_add_signed(interval));

    sqlx::query(
        "INSERT INTO job (key, kind, payload, interval_seconds, next_run_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (key) DO UPDATE SET
             kind = excluded.kind,
             payload = excluded.payload,
             interval_seconds = excluded.interval_seconds,
             -- Only set when it was never scheduled, so a restart does not
             -- push every due refresh a full interval into the future.
             next_run_at = COALESCE(job.next_run_at, excluded.next_run_at)",
    )
    .bind(key)
    .bind(kind)
    .bind(payload.to_string())
    .bind(interval_seconds)
    .bind(first_run.map(sql_timestamp))
    .execute(db)
    .await
    .map_err(translate)?;

    by_key(db, key).await?.ok_or(crate::Error::NotFound)
}

pub async fn remove(db: &SqlitePool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM job WHERE key = ?")
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

/// Jobs whose next run is in the past, oldest first.
pub async fn due(db: &SqlitePool, now: DateTime<Utc>) -> Result<Vec<Job>> {
    let rows = sqlx::query(
        "SELECT * FROM job
         WHERE interval_seconds IS NOT NULL
           AND next_run_at IS NOT NULL
           AND next_run_at <= ?
           AND state != 'running'
         ORDER BY next_run_at",
    )
    .bind(sql_timestamp(now))
    .fetch_all(db)
    .await?;

    Ok(rows.iter().map(map).collect())
}

/// Claim a job for execution, returning false if something else already has it.
///
/// The compare-and-set is the guard: SQLite has one writer, so a row that
/// moves to `running` here cannot also move there for another caller.
///
/// `last_success_at` is deliberately untouched. It is the only record of when
/// this unit last had good data, and the Sources page reads it as "last
/// refreshed"; clearing it makes every refresh report the account as never
/// fetched for as long as it runs. `state` says whether a run is in flight.
pub async fn claim(db: &SqlitePool, key: &str) -> Result<bool> {
    let claimed = sqlx::query(&format!(
        "UPDATE job
         SET state = 'running', progress = 0, message = NULL, last_error = NULL,
             started_at = {NOW}
         WHERE key = ? AND state NOT IN ('running')"
    ))
    .bind(key)
    .execute(db)
    .await?
    .rows_affected();

    Ok(claimed > 0)
}

pub async fn progress(db: &SqlitePool, key: &str, fraction: f64, message: &str) -> Result<()> {
    sqlx::query("UPDATE job SET progress = ?, message = ? WHERE key = ?")
        .bind(fraction.clamp(0.0, 1.0))
        .bind(message)
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

/// Record the outcome and schedule the next run.
///
/// The next run is counted from *now* rather than from the last due time, so a
/// job that overran does not immediately fire again trying to catch up.
///
/// Only a success moves `last_success_at`: a provider that has been failing
/// every night for a week must not report itself as refreshed an hour ago.
pub async fn finish(
    db: &SqlitePool,
    key: &str,
    state: State,
    message: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(&format!(
        "UPDATE job
         SET state = ?, progress = 1.0, message = ?, last_error = ?,
             last_success_at = CASE WHEN ? THEN {NOW} ELSE last_success_at END,
             next_run_at = CASE
                 WHEN interval_seconds IS NULL THEN NULL
                 ELSE strftime('%Y-%m-%d %H:%M:%f', 'now', '+' || interval_seconds || ' seconds')
                      || '+00:00'
             END
         WHERE key = ?"
    ))
    .bind(state.as_str())
    .bind(message)
    .bind(error)
    .bind(state == State::Success)
    .bind(key)
    .execute(db)
    .await?;
    Ok(())
}

/// Mark anything left `running` as abandoned.
///
/// A process that died mid-refresh leaves a row nothing will ever finish, and
/// the single-flight guard would then refuse that job forever.
pub async fn release_orphans(db: &SqlitePool) -> Result<u64> {
    Ok(sqlx::query(
        "UPDATE job
         SET state = 'failed', last_error = 'interrupted by a restart', progress = 0
         WHERE state = 'running'",
    )
    .execute(db)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn registering_twice_keeps_the_schedule_it_already_had() {
        let db = pool().await;
        let first = ensure(
            &db,
            "epg_refresh:1",
            "epg_refresh",
            &serde_json::json!({"source": 1}),
            Some(3600),
        )
        .await
        .unwrap();
        let scheduled = first.next_run_at.expect("scheduled");

        // A restart must not push a refresh that is nearly due a full interval
        // into the future, which is how a daily job never runs again.
        let second = ensure(
            &db,
            "epg_refresh:1",
            "epg_refresh",
            &serde_json::json!({"source": 1}),
            Some(3600),
        )
        .await
        .unwrap();
        assert_eq!(second.next_run_at, Some(scheduled));
        assert_eq!(second.payload["source"], 1);
    }

    #[tokio::test]
    async fn only_one_caller_can_claim_a_job() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), None)
            .await
            .unwrap();

        assert!(claim(&db, "k").await.unwrap());
        assert!(!claim(&db, "k").await.unwrap(), "claimed twice");

        finish(&db, "k", State::Success, Some("done"), None)
            .await
            .unwrap();
        assert!(
            claim(&db, "k").await.unwrap(),
            "not reclaimable after finishing"
        );
    }

    /// Claiming must not erase when the unit last succeeded.
    ///
    /// The Sources page shows this as "last refreshed", so clearing it here
    /// would make every refresh read *Never* from the moment it starts until
    /// the moment it ends.
    #[tokio::test]
    async fn claiming_a_job_keeps_the_time_it_last_succeeded() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), None)
            .await
            .unwrap();

        claim(&db, "k").await.unwrap();
        finish(&db, "k", State::Success, Some("done"), None)
            .await
            .unwrap();
        let first = by_key(&db, "k")
            .await
            .unwrap()
            .unwrap()
            .last_success_at
            .expect("a successful job records when it succeeded");

        claim(&db, "k").await.unwrap();
        let running = by_key(&db, "k").await.unwrap().unwrap();
        assert_eq!(running.state, State::Running);
        assert_eq!(
            running.last_success_at,
            Some(first),
            "a refresh in flight lost the previous one's timestamp"
        );

        finish(&db, "k", State::Success, None, None).await.unwrap();
        let second = by_key(&db, "k").await.unwrap().unwrap();
        assert!(second.last_success_at.unwrap() >= first);
    }

    /// A run that did not succeed is not a refresh.
    ///
    /// The timestamp answers "when did this last have good data". A provider
    /// failing every night for a week must not move it every night, or the
    /// column that should be shouting week-old reads as refreshed an hour ago.
    #[tokio::test]
    async fn only_a_successful_run_counts_as_a_refresh() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), None)
            .await
            .unwrap();

        // Nothing has succeeded yet, so there is nothing to report but Never.
        claim(&db, "k").await.unwrap();
        finish(&db, "k", State::Failed, None, Some("connection refused"))
            .await
            .unwrap();
        let attempted = by_key(&db, "k").await.unwrap().unwrap();
        assert_eq!(attempted.last_success_at, None);

        claim(&db, "k").await.unwrap();
        finish(&db, "k", State::Success, Some("done"), None)
            .await
            .unwrap();
        let good = by_key(&db, "k").await.unwrap().unwrap().last_success_at;
        assert!(good.is_some());

        for (state, error) in [
            (State::Failed, Some("connection refused")),
            (State::Cancelled, Some("stopped by the operator")),
        ] {
            claim(&db, "k").await.unwrap();
            finish(&db, "k", state, None, error).await.unwrap();
            let job = by_key(&db, "k").await.unwrap().unwrap();
            assert_eq!(job.state, state);
            let outcome = state.as_str();
            assert_eq!(
                job.last_success_at, good,
                "a {outcome} run was reported as a refresh"
            );
        }
    }

    #[tokio::test]
    async fn a_due_job_is_found_and_rescheduled_from_now() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), Some(60))
            .await
            .unwrap();

        assert!(due(&db, Utc::now()).await.unwrap().is_empty());
        assert_eq!(
            due(&db, Utc::now() + Duration::hours(1))
                .await
                .unwrap()
                .len(),
            1
        );

        claim(&db, "k").await.unwrap();
        // Running jobs are not due again, or a slow refresh queues itself.
        assert!(
            due(&db, Utc::now() + Duration::hours(1))
                .await
                .unwrap()
                .is_empty()
        );

        finish(&db, "k", State::Success, None, None).await.unwrap();
        let job = by_key(&db, "k").await.unwrap().unwrap();
        assert_eq!(job.state, State::Success);
        assert!(job.next_run_at.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn an_on_demand_job_is_never_due_and_never_reschedules() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), None)
            .await
            .unwrap();

        assert!(
            due(&db, Utc::now() + Duration::days(365))
                .await
                .unwrap()
                .is_empty()
        );
        claim(&db, "k").await.unwrap();
        finish(&db, "k", State::Success, None, None).await.unwrap();
        assert_eq!(by_key(&db, "k").await.unwrap().unwrap().next_run_at, None);
    }

    #[tokio::test]
    async fn a_job_interrupted_by_a_restart_is_released_not_stuck() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), None)
            .await
            .unwrap();
        claim(&db, "k").await.unwrap();

        assert_eq!(release_orphans(&db).await.unwrap(), 1);
        let job = by_key(&db, "k").await.unwrap().unwrap();
        assert_eq!(job.state, State::Failed);
        assert!(job.last_error.unwrap().contains("restart"));
        assert!(claim(&db, "k").await.unwrap());
    }

    #[tokio::test]
    async fn progress_is_clamped_and_visible() {
        let db = pool().await;
        ensure(&db, "k", "kind", &serde_json::json!({}), None)
            .await
            .unwrap();
        claim(&db, "k").await.unwrap();

        progress(&db, "k", 2.5, "halfway").await.unwrap();
        let job = by_key(&db, "k").await.unwrap().unwrap();
        assert_eq!(job.progress, 1.0);
        assert_eq!(job.message.as_deref(), Some("halfway"));
        assert_eq!(job.state, State::Running);
    }
}
