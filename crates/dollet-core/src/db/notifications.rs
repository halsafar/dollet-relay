//! Conditions the operator has to act on, deduplicated and acknowledgeable.
//!
//! [`super::events`] is the neighbour this is not. That table is an append-only
//! record of what happened and is trimmed to a bound nobody tunes; this one
//! holds the small set of things that are *still true* and have not been looked
//! at. Without it, a refresh that silently kept two hundred streams it was told
//! to filter is discovered weeks later as "channels appeared in Plex that
//! should not have", with the only evidence in a log nobody tails.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sqlx::{Row, SqlitePool};

use crate::Result;
use crate::domain::Id;

use super::{NOW, datetime_column, json_column};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// An unrecognised string reads as a warning rather than failing the row.
    ///
    /// The column has no CHECK, for the reason the schema gives beside the
    /// table — so the only thing standing between a hand-edited database and
    /// an unreadable notification list is this falling back instead of
    /// erroring.
    fn parse(raw: &str) -> Self {
        match raw {
            "info" => Self::Info,
            "error" => Self::Error,
            _ => Self::Warning,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub id: Id,
    pub kind: String,
    pub subject: String,
    pub severity: Severity,
    pub title: String,
    pub message: String,
    pub detail: Json,
    pub occurrences: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

impl Notification {
    /// One to hand to [`raise`].
    ///
    /// `id`, `occurrences` and the three timestamps belong to the store — a
    /// producer cannot know whether this is the first occurrence or the
    /// fortieth, which is the entire point of raising rather than inserting.
    /// They are placeholders until `raise` answers with the stored row.
    pub fn new(
        kind: impl Into<String>,
        subject: impl Into<String>,
        severity: Severity,
        title: impl Into<String>,
        message: impl Into<String>,
        detail: Json,
    ) -> Self {
        Self {
            id: 0,
            kind: kind.into(),
            subject: subject.into(),
            severity,
            title: title.into(),
            message: message.into(),
            detail,
            occurrences: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            acknowledged_at: None,
        }
    }
}

fn map(row: &sqlx::sqlite::SqliteRow) -> Notification {
    Notification {
        id: row.get("id"),
        kind: row.get("kind"),
        subject: row.get("subject"),
        severity: Severity::parse(&row.get::<String, _>("severity")),
        title: row.get("title"),
        message: row.get("message"),
        detail: json_column(row, "detail"),
        occurrences: row.get("occurrences"),
        created_at: datetime_column(row, "created_at"),
        updated_at: datetime_column(row, "updated_at"),
        acknowledged_at: row
            .get::<Option<String>, _>("acknowledged_at")
            .map(|_| datetime_column(row, "acknowledged_at")),
    }
}

/// Report that a condition holds, for the first time or the fortieth.
///
/// The upsert is on `(kind, subject)` and deliberately leaves
/// `acknowledged_at` alone. A provider whose filter has been broken for a month
/// raises this on every nightly refresh; clearing the acknowledgement each time
/// would make the bell impossible to empty and train the operator to ignore it,
/// which is the failure mode this whole table exists to avoid.
pub async fn raise(pool: &SqlitePool, notification: &Notification) -> Result<Notification> {
    let row = sqlx::query(&format!(
        "INSERT INTO notification (kind, subject, severity, title, message, detail)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (kind, subject) DO UPDATE SET
             occurrences = notification.occurrences + 1,
             severity    = excluded.severity,
             title       = excluded.title,
             message     = excluded.message,
             detail      = excluded.detail,
             updated_at  = {NOW}
         RETURNING *"
    ))
    .bind(&notification.kind)
    .bind(&notification.subject)
    .bind(notification.severity.as_str())
    .bind(&notification.title)
    .bind(&notification.message)
    .bind(notification.detail.to_string())
    .fetch_one(pool)
    .await?;

    Ok(map(&row))
}

/// The condition was looked for and is gone.
///
/// The row is deleted rather than marked resolved: there is no history to keep
/// here — `system_event` is where "this happened once" lives — and a resolved
/// row that stays visible is one more thing to dismiss by hand.
pub async fn clear(pool: &SqlitePool, kind: &str, subject: &str) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM notification WHERE kind = ? AND subject = ?")
        .bind(kind)
        .bind(subject)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

/// Unacknowledged first, newest first within each half.
pub async fn list(pool: &SqlitePool) -> Result<Vec<Notification>> {
    let rows = sqlx::query(
        "SELECT * FROM notification
         ORDER BY (acknowledged_at IS NULL) DESC, updated_at DESC, id DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(map).collect())
}

pub async fn unacknowledged_count(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM notification WHERE acknowledged_at IS NULL")
            .fetch_one(pool)
            .await?,
    )
}

/// `COALESCE`, so acknowledging twice does not move the timestamp: the useful
/// question is when it was first dismissed.
pub async fn acknowledge(pool: &SqlitePool, id: Id) -> Result<Option<Notification>> {
    let row = sqlx::query(&format!(
        "UPDATE notification SET acknowledged_at = COALESCE(acknowledged_at, {NOW})
         WHERE id = ? RETURNING *"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(map))
}

pub async fn acknowledge_all(pool: &SqlitePool) -> Result<u64> {
    Ok(sqlx::query(&format!(
        "UPDATE notification SET acknowledged_at = {NOW} WHERE acknowledged_at IS NULL"
    ))
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn delete(pool: &SqlitePool, id: Id) -> Result<bool> {
    let affected = sqlx::query("DELETE FROM notification WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn broken_filter(account: Id, message: &str) -> Notification {
        Notification::new(
            "m3u.filter_broken",
            format!("account:{account}"),
            Severity::Warning,
            "Filters skipped",
            message,
            serde_json::json!({ "m3u_account_id": account }),
        )
    }

    #[tokio::test]
    async fn the_same_condition_from_two_accounts_is_two_rows() {
        let pool = pool().await;
        raise(&pool, &broken_filter(1, "one")).await.unwrap();
        raise(&pool, &broken_filter(2, "two")).await.unwrap();

        // The subject is what keeps them apart; without it the second account's
        // fault would look like a recurrence of the first one's.
        assert_eq!(list(&pool).await.unwrap().len(), 2);
        assert_eq!(unacknowledged_count(&pool).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_recurrence_counts_up_and_rewrites_the_text() {
        let pool = pool().await;
        let first = raise(&pool, &broken_filter(1, "1 pattern")).await.unwrap();
        let second = raise(&pool, &broken_filter(1, "2 patterns")).await.unwrap();

        assert_eq!(second.id, first.id);
        assert_eq!(second.occurrences, 2);
        // The newest description of the condition, not the oldest: the second
        // refresh is the one that describes the account as it is now.
        assert_eq!(second.message, "2 patterns");
    }

    #[tokio::test]
    async fn an_acknowledged_condition_that_recurs_stays_acknowledged() {
        let pool = pool().await;
        let raised = raise(&pool, &broken_filter(1, "one")).await.unwrap();
        let seen = acknowledge(&pool, raised.id).await.unwrap().unwrap();

        let again = raise(&pool, &broken_filter(1, "one")).await.unwrap();
        assert_eq!(again.occurrences, 2);
        assert_eq!(
            again.acknowledged_at, seen.acknowledged_at,
            "a nightly refresh un-dismissed a condition the operator had decided to live with"
        );
        assert_eq!(unacknowledged_count(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn clearing_reports_whether_there_was_anything_to_clear() {
        let pool = pool().await;
        raise(&pool, &broken_filter(1, "one")).await.unwrap();

        assert!(
            clear(&pool, "m3u.filter_broken", "account:1")
                .await
                .unwrap()
        );
        assert!(
            !clear(&pool, "m3u.filter_broken", "account:1")
                .await
                .unwrap()
        );
        assert!(list(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unacknowledged_sort_above_acknowledged() {
        let pool = pool().await;
        let old = raise(&pool, &broken_filter(1, "one")).await.unwrap();
        raise(&pool, &broken_filter(2, "two")).await.unwrap();
        raise(&pool, &broken_filter(3, "three")).await.unwrap();
        acknowledge(&pool, old.id).await.unwrap();

        let listed = list(&pool).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|n| n.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["account:3", "account:2", "account:1"],
            "an acknowledged row sorted above one nobody has looked at"
        );

        assert_eq!(acknowledge_all(&pool).await.unwrap(), 2);
        assert_eq!(unacknowledged_count(&pool).await.unwrap(), 0);
        // Idempotent: nothing left to acknowledge is zero, not an error.
        assert_eq!(acknowledge_all(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn acknowledging_twice_keeps_the_first_answer() {
        let pool = pool().await;
        let raised = raise(&pool, &broken_filter(1, "one")).await.unwrap();

        let first = acknowledge(&pool, raised.id).await.unwrap().unwrap();
        let second = acknowledge(&pool, raised.id).await.unwrap().unwrap();
        assert_eq!(first.acknowledged_at, second.acknowledged_at);
        assert!(acknowledge(&pool, 9999).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_unknown_severity_reads_as_a_warning_rather_than_failing_the_list() {
        let pool = pool().await;
        sqlx::query(
            "INSERT INTO notification (kind, subject, severity, title, message)
             VALUES ('x', '', 'catastrophe', 't', 'm')",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(list(&pool).await.unwrap()[0].severity, Severity::Warning);
    }

    #[tokio::test]
    async fn deleting_reports_whether_there_was_a_row() {
        let pool = pool().await;
        let raised = raise(&pool, &broken_filter(1, "one")).await.unwrap();

        assert!(delete(&pool, raised.id).await.unwrap());
        assert!(!delete(&pool, raised.id).await.unwrap());
    }
}
