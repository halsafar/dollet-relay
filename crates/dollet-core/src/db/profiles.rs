//! User agents, stream profiles, and output profiles.
//!
//! All three are small catalogues the UI edits directly. Rows marked `locked`
//! ship with the product — `proxy` and `redirect` in particular are referenced
//! by name in the streaming path, so deleting them would break playback in a
//! way that only shows up at the next channel start.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::domain::{Id, OutputProfile, StreamProfile, UserAgent};
use crate::{Error, Result};

use super::{NOW, bool_column, translate};

fn map_user_agent(row: &SqliteRow) -> UserAgent {
    UserAgent {
        id: row.get("id"),
        name: row.get("name"),
        user_agent: row.get("user_agent"),
        is_active: bool_column(row, "is_active"),
    }
}

fn map_stream_profile(row: &SqliteRow) -> StreamProfile {
    StreamProfile {
        id: row.get("id"),
        name: row.get("name"),
        command: row.get("command"),
        parameters: row.get("parameters"),
        locked: bool_column(row, "locked"),
        is_active: bool_column(row, "is_active"),
        user_agent_id: row.get("user_agent_id"),
    }
}

fn map_output_profile(row: &SqliteRow) -> OutputProfile {
    OutputProfile {
        id: row.get("id"),
        name: row.get("name"),
        command: row.get("command"),
        parameters: row.get("parameters"),
        locked: bool_column(row, "locked"),
        is_active: bool_column(row, "is_active"),
    }
}

pub async fn list_user_agents(pool: &SqlitePool) -> Result<Vec<UserAgent>> {
    let rows = sqlx::query("SELECT * FROM user_agent ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_user_agent).collect())
}

pub async fn get_user_agent(pool: &SqlitePool, id: Id) -> Result<Option<UserAgent>> {
    let row = sqlx::query("SELECT * FROM user_agent WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_user_agent))
}

pub async fn create_user_agent(pool: &SqlitePool, agent: &UserAgent) -> Result<UserAgent> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO user_agent (name, user_agent, is_active) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(&agent.name)
    .bind(&agent.user_agent)
    .bind(agent.is_active as i64)
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_user_agent(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_user_agent(pool: &SqlitePool, agent: &UserAgent) -> Result<UserAgent> {
    let changed = sqlx::query(&format!(
        "UPDATE user_agent SET name = ?, user_agent = ?, is_active = ?, updated_at = {NOW}
         WHERE id = ?"
    ))
    .bind(&agent.name)
    .bind(&agent.user_agent)
    .bind(agent.is_active as i64)
    .bind(agent.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_user_agent(pool, agent.id).await?.ok_or(Error::NotFound)
}

pub async fn delete_user_agent(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM user_agent WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn list_stream_profiles(pool: &SqlitePool) -> Result<Vec<StreamProfile>> {
    let rows = sqlx::query("SELECT * FROM stream_profile ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_stream_profile).collect())
}

pub async fn get_stream_profile(pool: &SqlitePool, id: Id) -> Result<Option<StreamProfile>> {
    let row = sqlx::query("SELECT * FROM stream_profile WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_stream_profile))
}

/// The locked profile whose name the proxy special-cases into a 302.
pub async fn redirect_profile_id(pool: &SqlitePool) -> Result<Option<Id>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM stream_profile WHERE name = 'redirect' AND locked = 1")
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn create_stream_profile(
    pool: &SqlitePool,
    profile: &StreamProfile,
) -> Result<StreamProfile> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO stream_profile (name, command, parameters, is_active, user_agent_id)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&profile.name)
    .bind(&profile.command)
    .bind(&profile.parameters)
    .bind(profile.is_active as i64)
    .bind(profile.user_agent_id)
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_stream_profile(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_stream_profile(
    pool: &SqlitePool,
    profile: &StreamProfile,
) -> Result<StreamProfile> {
    let changed = sqlx::query(
        "UPDATE stream_profile SET name = ?, command = ?, parameters = ?, is_active = ?,
                                   user_agent_id = ?
         WHERE id = ?",
    )
    .bind(&profile.name)
    .bind(&profile.command)
    .bind(&profile.parameters)
    .bind(profile.is_active as i64)
    .bind(profile.user_agent_id)
    .bind(profile.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_stream_profile(pool, profile.id)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn delete_stream_profile(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM stream_profile WHERE id = ? AND locked = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn list_output_profiles(pool: &SqlitePool) -> Result<Vec<OutputProfile>> {
    let rows = sqlx::query("SELECT * FROM output_profile ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_output_profile).collect())
}

pub async fn get_output_profile(pool: &SqlitePool, id: Id) -> Result<Option<OutputProfile>> {
    let row = sqlx::query("SELECT * FROM output_profile WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_output_profile))
}

pub async fn create_output_profile(
    pool: &SqlitePool,
    profile: &OutputProfile,
) -> Result<OutputProfile> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO output_profile (name, command, parameters, is_active)
         VALUES (?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&profile.name)
    .bind(&profile.command)
    .bind(&profile.parameters)
    .bind(profile.is_active as i64)
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_output_profile(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_output_profile(
    pool: &SqlitePool,
    profile: &OutputProfile,
) -> Result<OutputProfile> {
    let changed = sqlx::query(
        "UPDATE output_profile SET name = ?, command = ?, parameters = ?, is_active = ?
         WHERE id = ?",
    )
    .bind(&profile.name)
    .bind(&profile.command)
    .bind(&profile.parameters)
    .bind(profile.is_active as i64)
    .bind(profile.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_output_profile(pool, profile.id)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn delete_output_profile(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM output_profile WHERE id = ? AND locked = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
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
    async fn the_locked_defaults_are_seeded() {
        let pool = pool().await;
        let profiles = list_stream_profiles(&pool).await.unwrap();
        let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ffmpeg", "streamlink", "proxy", "redirect", "vlc"]
        );
        assert!(profiles.iter().all(|p| p.locked));

        // The two empty-command profiles are the ones the proxy branches on.
        let proxy = profiles.iter().find(|p| p.name == "proxy").unwrap();
        assert!(proxy.command.is_empty());
        assert_eq!(redirect_profile_id(&pool).await.unwrap(), Some(4));

        assert_eq!(list_user_agents(&pool).await.unwrap().len(), 3);
        assert_eq!(list_output_profiles(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn locked_profiles_cannot_be_deleted() {
        let pool = pool().await;
        assert!(matches!(
            delete_stream_profile(&pool, 3).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            delete_output_profile(&pool, 1).await,
            Err(Error::NotFound)
        ));
        assert_eq!(list_stream_profiles(&pool).await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn user_profiles_round_trip() {
        let pool = pool().await;
        let created = create_stream_profile(
            &pool,
            &StreamProfile {
                id: 0,
                name: "mine".into(),
                command: "ffmpeg".into(),
                parameters: "-i {streamUrl} -f mpegts pipe:1".into(),
                locked: false,
                is_active: true,
                user_agent_id: Some(1),
            },
        )
        .await
        .unwrap();

        assert!(!created.locked);

        let mut edited = created.clone();
        edited.is_active = false;
        assert!(!save_stream_profile(&pool, &edited).await.unwrap().is_active);

        delete_stream_profile(&pool, created.id).await.unwrap();
        assert!(
            get_stream_profile(&pool, created.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn duplicate_names_are_a_conflict_not_a_crash() {
        let pool = pool().await;
        let result = create_output_profile(
            &pool,
            &OutputProfile {
                id: 0,
                name: "AC3 audio (media servers)".into(),
                command: "ffmpeg".into(),
                parameters: String::new(),
                locked: false,
                is_active: true,
            },
        )
        .await;

        assert!(matches!(result, Err(Error::Conflict(_))));
    }
}
