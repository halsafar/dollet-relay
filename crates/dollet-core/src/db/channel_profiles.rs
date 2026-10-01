//! Channel profiles: named subsets of the lineup, addressed by outputs.
//!
//! Membership rows are created by database triggers when either side appears,
//! so a profile always has a row for every channel. What varies is `enabled`.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::domain::{ChannelProfile, ChannelProfileMembership, Id};
use crate::{Error, Result};

use super::{bool_column, translate};

fn map(row: &SqliteRow) -> ChannelProfile {
    ChannelProfile {
        id: row.get("id"),
        name: row.get("name"),
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ChannelProfile>> {
    let rows = sqlx::query("SELECT * FROM channel_profile ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map).collect())
}

pub async fn get(pool: &SqlitePool, id: Id) -> Result<Option<ChannelProfile>> {
    let row = sqlx::query("SELECT * FROM channel_profile WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map))
}

/// Outputs address a profile by name in the URL (`/output/m3u/<profile>`).
pub async fn by_name(pool: &SqlitePool, name: &str) -> Result<Option<ChannelProfile>> {
    let row = sqlx::query("SELECT * FROM channel_profile WHERE name = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map))
}

/// `start_empty` disables every channel the trigger just added, for a profile
/// meant to be built up rather than pared down.
pub async fn create(pool: &SqlitePool, name: &str, start_empty: bool) -> Result<ChannelProfile> {
    let id: Id = sqlx::query_scalar("INSERT INTO channel_profile (name) VALUES (?) RETURNING id")
        .bind(name)
        .fetch_one(pool)
        .await
        .map_err(translate)?;

    if start_empty {
        sqlx::query(
            "UPDATE channel_profile_membership SET enabled = 0 WHERE channel_profile_id = ?",
        )
        .bind(id)
        .execute(pool)
        .await?;
    }

    get(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn rename(pool: &SqlitePool, id: Id, name: &str) -> Result<ChannelProfile> {
    let changed = sqlx::query("UPDATE channel_profile SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(pool)
        .await
        .map_err(translate)?
        .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn delete(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM channel_profile WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn members(pool: &SqlitePool, profile_id: Id) -> Result<Vec<ChannelProfileMembership>> {
    let rows = sqlx::query(
        "SELECT * FROM channel_profile_membership WHERE channel_profile_id = ? ORDER BY channel_id",
    )
    .bind(profile_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| ChannelProfileMembership {
            channel_profile_id: row.get("channel_profile_id"),
            channel_id: row.get("channel_id"),
            enabled: bool_column(row, "enabled"),
        })
        .collect())
}

/// Enable or disable channels within a profile, in one batch.
///
/// Upserts rather than updates: a channel created before this profile existed
/// still has its row from the trigger, but the importer writes rows directly
/// and a hand-edited database might not.
pub async fn set_enabled(
    pool: &SqlitePool,
    profile_id: Id,
    channel_ids: &[Id],
    enabled: bool,
) -> Result<()> {
    const BATCH: usize = 200;

    for chunk in channel_ids.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for channel_id in chunk {
            sqlx::query(
                "INSERT INTO channel_profile_membership (channel_profile_id, channel_id, enabled)
                 VALUES (?, ?, ?)
                 ON CONFLICT (channel_profile_id, channel_id)
                 DO UPDATE SET enabled = excluded.enabled",
            )
            .bind(profile_id)
            .bind(channel_id)
            .bind(enabled as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();

        for id in 1..=3 {
            sqlx::query("INSERT INTO channel (id, uuid, name) VALUES (?, ?, ?)")
                .bind(id)
                .bind(format!("uuid-{id}"))
                .bind(format!("Channel {id}"))
                .execute(&pool)
                .await
                .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn the_default_profile_picks_up_every_channel() {
        let pool = pool().await;
        let members = members(&pool, 1).await.unwrap();
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|m| m.enabled));
    }

    #[tokio::test]
    async fn a_new_profile_starts_full_or_empty_as_asked() {
        let pool = pool().await;

        let full = create(&pool, "Everything", false).await.unwrap();
        assert!(
            members(&pool, full.id)
                .await
                .unwrap()
                .iter()
                .all(|m| m.enabled)
        );

        let empty = create(&pool, "Curated", true).await.unwrap();
        let rows = members(&pool, empty.id).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|m| !m.enabled));
    }

    #[tokio::test]
    async fn a_channel_added_later_joins_existing_profiles() {
        let pool = pool().await;
        let profile = create(&pool, "Later", false).await.unwrap();

        sqlx::query("INSERT INTO channel (id, uuid, name) VALUES (9, 'uuid-9', 'Late')")
            .execute(&pool)
            .await
            .unwrap();

        let members = members(&pool, profile.id).await.unwrap();
        assert_eq!(members.len(), 4);
        assert!(members.iter().find(|m| m.channel_id == 9).unwrap().enabled);
    }

    #[tokio::test]
    async fn bulk_enable_and_disable() {
        let pool = pool().await;
        set_enabled(&pool, 1, &[1, 2], false).await.unwrap();

        let disabled: Vec<Id> = members(&pool, 1)
            .await
            .unwrap()
            .into_iter()
            .filter(|m| !m.enabled)
            .map(|m| m.channel_id)
            .collect();
        assert_eq!(disabled, vec![1, 2]);

        set_enabled(&pool, 1, &[1], true).await.unwrap();
        assert!(
            members(&pool, 1)
                .await
                .unwrap()
                .iter()
                .find(|m| m.channel_id == 1)
                .unwrap()
                .enabled
        );
    }

    #[tokio::test]
    async fn profiles_are_addressable_by_name_and_names_are_unique() {
        let pool = pool().await;
        assert_eq!(by_name(&pool, "All").await.unwrap().unwrap().id, 1);
        assert!(matches!(
            create(&pool, "all", false).await,
            Err(Error::Conflict(_))
        ));
    }
}
