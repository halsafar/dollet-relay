//! Users, their API keys, and the channel profiles they may see.
//!
//! Writes are read-modify-write over the whole [`User`]: the table is tiny and
//! single-writer, and a per-field patch struct would need a double `Option` to
//! distinguish "absent" from "set to null" for every nullable column.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::auth::{level_from_i64, level_value};
use crate::domain::{Id, User, UserLevel};
use crate::{Error, Result};

use super::{NOW, bool_column, json_column, translate};

const COLUMNS: &str = "id, username, email, password, is_active, user_level, api_key, \
                       stream_limit, custom_properties";

fn map(row: &SqliteRow) -> User {
    User {
        id: row.get("id"),
        username: row.get("username"),
        email: row.get("email"),
        password_hash: row.get("password"),
        is_active: bool_column(row, "is_active"),
        user_level: level_from_i64(row.get("user_level")),
        api_key: row.get("api_key"),
        stream_limit: row.get::<i64, _>("stream_limit") as i32,
        channel_profile_ids: Vec::new(),
        custom_properties: json_column(row, "custom_properties"),
    }
}

async fn attach_profiles(pool: &SqlitePool, users: &mut [User]) -> Result<()> {
    if users.is_empty() {
        return Ok(());
    }

    let rows = sqlx::query("SELECT user_id, channel_profile_id FROM user_channel_profile")
        .fetch_all(pool)
        .await?;

    for row in rows {
        let user_id: Id = row.get("user_id");
        if let Some(user) = users.iter_mut().find(|u| u.id == user_id) {
            user.channel_profile_ids.push(row.get("channel_profile_id"));
        }
    }
    Ok(())
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<User>> {
    let rows = sqlx::query(&format!("SELECT {COLUMNS} FROM user ORDER BY id"))
        .fetch_all(pool)
        .await?;

    let mut users: Vec<User> = rows.iter().map(map).collect();
    attach_profiles(pool, &mut users).await?;
    Ok(users)
}

pub async fn get(pool: &SqlitePool, id: Id) -> Result<Option<User>> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM user WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await?;

    let Some(row) = row else { return Ok(None) };
    let mut users = vec![map(&row)];
    attach_profiles(pool, &mut users).await?;
    Ok(users.pop())
}

pub async fn by_username(pool: &SqlitePool, username: &str) -> Result<Option<User>> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM user WHERE username = ?"))
        .bind(username)
        .fetch_optional(pool)
        .await?;

    let Some(row) = row else { return Ok(None) };
    let mut users = vec![map(&row)];
    attach_profiles(pool, &mut users).await?;
    Ok(users.pop())
}

pub async fn by_api_key(pool: &SqlitePool, key: &str) -> Result<Option<User>> {
    // An empty key must never match the many users who have none.
    if key.is_empty() {
        return Ok(None);
    }

    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM user WHERE api_key = ? AND is_active = 1"
    ))
    .bind(key)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(None) };
    let mut users = vec![map(&row)];
    attach_profiles(pool, &mut users).await?;
    Ok(users.pop())
}

pub async fn create(pool: &SqlitePool, user: &User) -> Result<User> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO user (username, email, password, is_active, user_level, api_key,
                           stream_limit, custom_properties)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&user.username)
    .bind(&user.email)
    .bind(&user.password_hash)
    .bind(user.is_active as i64)
    .bind(level_value(user.user_level))
    .bind(&user.api_key)
    .bind(user.stream_limit as i64)
    .bind(user.custom_properties.to_string())
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    set_channel_profiles(pool, id, &user.channel_profile_ids).await?;
    get(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save(pool: &SqlitePool, user: &User) -> Result<User> {
    let changed = sqlx::query(
        "UPDATE user SET username = ?, email = ?, password = ?, is_active = ?, user_level = ?,
                         api_key = ?, stream_limit = ?, custom_properties = ?
         WHERE id = ?",
    )
    .bind(&user.username)
    .bind(&user.email)
    .bind(&user.password_hash)
    .bind(user.is_active as i64)
    .bind(level_value(user.user_level))
    .bind(&user.api_key)
    .bind(user.stream_limit as i64)
    .bind(user.custom_properties.to_string())
    .bind(user.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }

    set_channel_profiles(pool, user.id, &user.channel_profile_ids).await?;
    get(pool, user.id).await?.ok_or(Error::NotFound)
}

async fn set_channel_profiles(pool: &SqlitePool, id: Id, profiles: &[Id]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_channel_profile WHERE user_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;

    for profile in profiles {
        sqlx::query(
            "INSERT OR IGNORE INTO user_channel_profile (user_id, channel_profile_id)
             VALUES (?, ?)",
        )
        .bind(id)
        .bind(profile)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM user WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Create the first admin, or refuse because one already exists.
///
/// The check and the insert share a transaction: SQLite has one writer, so
/// that makes the setup endpoint atomic. Checking first and inserting after
/// leaves a window where two requests each see an empty instance.
///
/// Counts admins regardless of `is_active`: a disabled admin is still an
/// account somebody owns, and letting setup run again would hand the instance
/// to whoever asked first.
pub async fn create_first_admin(pool: &SqlitePool, user: &User) -> Result<Option<User>> {
    let mut tx = pool.begin().await?;

    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE user_level >= ?")
        .bind(level_value(UserLevel::Admin))
        .fetch_one(&mut *tx)
        .await?;
    if existing > 0 {
        return Ok(None);
    }

    let id: Id = sqlx::query_scalar(
        "INSERT INTO user (username, email, password, is_active, user_level, stream_limit,
                           custom_properties)
         VALUES (?, ?, ?, 1, ?, ?, ?)
         RETURNING id",
    )
    .bind(&user.username)
    .bind(&user.email)
    .bind(&user.password_hash)
    .bind(level_value(UserLevel::Admin))
    .bind(user.stream_limit as i64)
    .bind(user.custom_properties.to_string())
    .fetch_one(&mut *tx)
    .await
    .map_err(translate)?;

    tx.commit().await?;
    get(pool, id).await
}

/// Whether the instance has been set up at all, ignoring `is_active`.
pub async fn any_admin_exists(pool: &SqlitePool) -> Result<bool> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE user_level >= ?")
        .bind(level_value(UserLevel::Admin))
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

/// Guards the last-admin case: an instance with no *usable* admin cannot be
/// administered, and the only fix is editing the database by hand.
pub async fn admin_count(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM user WHERE user_level >= ? AND is_active = 1")
            .bind(level_value(UserLevel::Admin))
            .fetch_one(pool)
            .await?,
    )
}

pub async fn touch_last_login(pool: &SqlitePool, id: Id) -> Result<()> {
    sqlx::query(&format!("UPDATE user SET last_login = {NOW} WHERE id = ?"))
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn draft(username: &str) -> User {
        User {
            id: 0,
            username: username.to_owned(),
            email: None,
            password_hash:
                "pbkdf2_sha256$1000$saltysalt$TGvLvWQ3FS0hm6LiY2GRRjIIFTC/Q5rp07HyYd7TgIA=".into(),
            is_active: true,
            user_level: UserLevel::Admin,
            api_key: None,
            stream_limit: 0,
            channel_profile_ids: Vec::new(),
            custom_properties: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn creates_reads_and_updates() {
        let pool = pool().await;
        let created = create(&pool, &draft("alice")).await.unwrap();
        assert_eq!(created.username, "alice");
        assert_eq!(created.user_level, UserLevel::Admin);

        let mut edited = created.clone();
        edited.email = Some("alice@example.test".into());
        edited.stream_limit = 3;
        edited.user_level = UserLevel::Standard;
        let saved = save(&pool, &edited).await.unwrap();

        assert_eq!(saved.email.as_deref(), Some("alice@example.test"));
        assert_eq!(saved.stream_limit, 3);
        assert_eq!(saved.user_level, UserLevel::Standard);
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_imported_password_hash_still_verifies() {
        let pool = pool().await;
        let user = create(&pool, &draft("bob")).await.unwrap();
        let found = by_username(&pool, "bob").await.unwrap().unwrap();

        assert_eq!(found.password_hash, user.password_hash);
        assert!(password::verify("correct horse battery staple", &found.password_hash).unwrap());
    }

    #[tokio::test]
    async fn usernames_are_matched_case_insensitively() {
        let pool = pool().await;
        create(&pool, &draft("Carol")).await.unwrap();
        assert!(by_username(&pool, "carol").await.unwrap().is_some());
        assert!(create(&pool, &draft("CAROL")).await.is_err());
    }

    #[tokio::test]
    async fn api_key_lookup_ignores_empty_and_inactive() {
        let pool = pool().await;
        let mut user = draft("dave");
        user.api_key = Some("secret-key".into());
        let created = create(&pool, &user).await.unwrap();

        assert_eq!(
            by_api_key(&pool, "secret-key").await.unwrap().unwrap().id,
            created.id
        );
        assert!(by_api_key(&pool, "").await.unwrap().is_none());
        assert!(by_api_key(&pool, "other").await.unwrap().is_none());

        let mut disabled = created.clone();
        disabled.is_active = false;
        save(&pool, &disabled).await.unwrap();
        assert!(by_api_key(&pool, "secret-key").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn channel_profile_membership_round_trips() {
        let pool = pool().await;
        sqlx::query("INSERT INTO channel_profile (id, name) VALUES (2, 'Kids')")
            .execute(&pool)
            .await
            .unwrap();

        let mut user = draft("erin");
        user.channel_profile_ids = vec![1, 2];
        let created = create(&pool, &user).await.unwrap();
        assert_eq!(created.channel_profile_ids, vec![1, 2]);

        let mut narrowed = created.clone();
        narrowed.channel_profile_ids = vec![2];
        assert_eq!(
            save(&pool, &narrowed).await.unwrap().channel_profile_ids,
            vec![2]
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_user_is_an_error_not_a_silent_success() {
        let pool = pool().await;
        assert!(matches!(delete(&pool, 404).await, Err(Error::NotFound)));
    }

    #[tokio::test]
    async fn the_first_admin_can_only_be_created_once() {
        let pool = pool().await;
        assert!(!any_admin_exists(&pool).await.unwrap());

        let created = create_first_admin(&pool, &draft("first"))
            .await
            .unwrap()
            .expect("first admin created");
        assert_eq!(created.user_level, UserLevel::Admin);
        assert!(any_admin_exists(&pool).await.unwrap());

        assert!(
            create_first_admin(&pool, &draft("second"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn setup_stays_closed_even_if_the_only_admin_is_disabled() {
        let pool = pool().await;
        let mut disabled = draft("locked-out");
        disabled.is_active = false;
        create(&pool, &disabled).await.unwrap();

        // `admin_count` is about who can still administer; `any_admin_exists`
        // is about whether the instance is claimed. Conflating them would let
        // a stranger take over an instance whose admin was suspended.
        assert_eq!(admin_count(&pool).await.unwrap(), 0);
        assert!(any_admin_exists(&pool).await.unwrap());
        assert!(
            create_first_admin(&pool, &draft("usurper"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_duplicate_username_is_a_conflict_not_a_server_error() {
        let pool = pool().await;
        create(&pool, &draft("taken")).await.unwrap();
        assert!(matches!(
            create(&pool, &draft("taken")).await,
            Err(Error::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn admin_count_only_counts_active_admins() {
        let pool = pool().await;
        create(&pool, &draft("admin1")).await.unwrap();
        assert_eq!(admin_count(&pool).await.unwrap(), 1);

        let mut streamer = draft("streamer");
        streamer.user_level = UserLevel::Streamer;
        create(&pool, &streamer).await.unwrap();
        assert_eq!(admin_count(&pool).await.unwrap(), 1);
    }
}
