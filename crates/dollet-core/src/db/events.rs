//! The rolling system event log behind the Stats page.
//!
//! Bounded by `system_settings.max_system_events`, trimmed on write: an
//! unbounded log on a box that streams for months is a slow disk leak, and
//! nobody reads the thousandth buffering event.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value as Json;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::Result;
use crate::domain::Id;

use super::{datetime_column, json_column};

#[derive(Debug, Clone, Serialize)]
pub struct SystemEvent {
    pub id: Id,
    pub event_type: String,
    pub occurred_at: DateTime<Utc>,
    pub channel_uuid: Option<Uuid>,
    pub channel_name: Option<String>,
    pub details: Json,
}

pub async fn record(
    pool: &SqlitePool,
    event_type: &str,
    channel_uuid: Option<Uuid>,
    channel_name: Option<&str>,
    details: &Json,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO system_event (event_type, channel_uuid, channel_name, details)
         VALUES (?, ?, ?, ?)",
    )
    .bind(event_type)
    .bind(channel_uuid.map(|u| u.hyphenated().to_string()))
    .bind(channel_name)
    .bind(details.to_string())
    .execute(pool)
    .await?;

    // Trimmed here rather than on a timer: a bound nothing enforces is a
    // setting that lies, and this table is written from the streaming path
    // where "a few rows a minute, forever" is a slow disk leak.
    let keep = crate::settings::load::<crate::settings::SystemSettings>(pool)
        .await?
        .max_system_events;
    trim(pool, keep).await?;
    Ok(())
}

/// How many events are stored, which is not how many a capped list returns.
pub async fn count(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM system_event")
        .fetch_one(pool)
        .await?)
}

pub async fn list(pool: &SqlitePool, limit: i64) -> Result<Vec<SystemEvent>> {
    let rows = sqlx::query("SELECT * FROM system_event ORDER BY id DESC LIMIT ?")
        .bind(limit.clamp(1, 1000))
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|row| SystemEvent {
            id: row.get("id"),
            event_type: row.get("event_type"),
            occurred_at: datetime_column(row, "occurred_at"),
            channel_uuid: row
                .get::<Option<String>, _>("channel_uuid")
                .and_then(|raw| Uuid::parse_str(&raw).ok()),
            channel_name: row.get("channel_name"),
            details: json_column(row, "details"),
        })
        .collect())
}

pub async fn trim(pool: &SqlitePool, keep: u32) -> Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM system_event WHERE id NOT IN
           (SELECT id FROM system_event ORDER BY id DESC LIMIT ?)",
    )
    .bind(i64::from(keep))
    .execute(pool)
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
    async fn newest_first_with_details_intact() {
        let pool = pool().await;
        let uuid = Uuid::new_v4();

        record(
            &pool,
            "channel_start",
            Some(uuid),
            Some("VRIX"),
            &serde_json::json!({"streams": 2}),
        )
        .await
        .unwrap();
        record(&pool, "channel_stop", Some(uuid), Some("VRIX"), &Json::Null)
            .await
            .unwrap();

        let events = list(&pool, 10).await.unwrap();
        assert_eq!(events[0].event_type, "channel_stop");
        assert_eq!(events[1].details["streams"], 2);
        assert_eq!(events[1].channel_uuid, Some(uuid));
        assert!(events[0].occurred_at.timestamp() > 0);
    }

    #[tokio::test]
    async fn trimming_keeps_the_newest() {
        let pool = pool().await;
        for i in 0..10 {
            record(&pool, &format!("e{i}"), None, None, &Json::Null)
                .await
                .unwrap();
        }

        assert_eq!(trim(&pool, 3).await.unwrap(), 7);
        let left = list(&pool, 10).await.unwrap();
        assert_eq!(left.len(), 3);
        assert_eq!(left[0].event_type, "e9");
    }
}
