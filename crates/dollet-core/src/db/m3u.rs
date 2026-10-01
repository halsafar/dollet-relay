//! M3U / Xtream Codes accounts and the patterns attached to them.
//!
//! The patterns need care: they are user-authored, PCRE-flavoured and
//! persisted, so they survive an import intact and compile with `fancy_regex`
//! rather than `regex`, which rejects the lookaround and backreferences they
//! may carry.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::domain::{Id, M3uAccount, M3uAccountProfile, M3uAccountType};
use crate::regex_compat::js_backrefs_to_rust;
use crate::{Error, Result};

use super::{NOW, bool_column, json_column, translate};

/// What happened when a stored pattern was compiled.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternCheck {
    Ok,
    /// Compiled, but only after `$1` was rewritten as `\1`.
    Rewritten(String),
    /// Will not compile, so the filter or rename it drives silently changes
    /// behaviour. The importer reports these loudly.
    Failed(String),
}

/// Compile a user-authored pattern the way the ingest path will.
///
/// `fancy_regex`, never `regex`: these were written for PCRE-flavoured engines
/// and may use lookaround or backreferences, which Rust's `regex` crate
/// rejects by design.
///
/// Lives beside the table that stores the patterns because `dollet-server` has no
/// `fancy-regex` dependency and must not grow one just to validate them.
pub fn check_pattern(pattern: &str) -> PatternCheck {
    if pattern.is_empty() {
        return PatternCheck::Ok;
    }

    let normalised = js_backrefs_to_rust(pattern);
    match fancy_regex::Regex::new(&normalised) {
        Err(error) => PatternCheck::Failed(error.to_string()),
        Ok(_) if normalised != pattern => PatternCheck::Rewritten(normalised),
        Ok(_) => PatternCheck::Ok,
    }
}

/// One include/exclude rule applied to a provider's stream list.
#[derive(Debug, Clone)]
pub struct M3uFilter {
    pub id: Id,
    pub m3u_account_id: Id,
    /// `name` or `group`.
    pub filter_type: String,
    /// User-authored; compile with `fancy_regex`.
    pub regex_pattern: String,
    pub exclude: bool,
    pub sort_order: i64,
}

/// Per-account settings for one provider group: whether its streams are
/// imported at all, and whether channels are created from them automatically.
#[derive(Debug, Clone)]
pub struct GroupAccountLink {
    pub id: Id,
    pub channel_group_id: Id,
    pub m3u_account_id: Id,
    pub enabled: bool,
    pub auto_channel_sync: bool,
    pub auto_sync_channel_start: Option<f64>,
    pub auto_sync_channel_end: Option<f64>,
    /// Everything else the auto-sync UI stores here rather than in a column:
    /// `channel_numbering_mode`, `channel_numbering_fallback`,
    /// `name_regex_pattern`, `name_replace_pattern`.
    pub custom_properties: serde_json::Value,
}

/// Read a stored `account_type`.
///
/// A value this build does not know is an error, not a `Standard` account. The
/// column carries no CHECK — it cannot, because SQLite would make widening it a
/// table rebuild that cascade-deletes `stream` — so this is where the set is
/// enforced. Falling back would mean a downgrade after a migration that added a
/// third type fetched those accounts as plain M3U playlists: wrong URL, wrong
/// hash, and a rebuilt catalogue on the first refresh.
pub fn account_type(raw: &str) -> Result<M3uAccountType> {
    match raw {
        "xtream_codes" => Ok(M3uAccountType::XtreamCodes),
        "standard" => Ok(M3uAccountType::Standard),
        other => Err(crate::Error::invalid(format!(
            "unknown m3u account type `{other}`; this build understands \
             `standard` and `xtream_codes`"
        ))),
    }
}

pub fn account_type_str(value: M3uAccountType) -> &'static str {
    match value {
        M3uAccountType::XtreamCodes => "xtream_codes",
        M3uAccountType::Standard => "standard",
    }
}

fn map_account(row: &SqliteRow) -> Result<M3uAccount> {
    Ok(M3uAccount {
        id: row.get("id"),
        name: row.get("name"),
        server_url: row.get("server_url"),
        file_path: row.get("file_path"),
        username: row.get("username"),
        password: row.get("password"),
        account_type: account_type(row.get("account_type"))?,
        max_streams: row.get::<i64, _>("max_streams").max(0) as u32,
        is_active: bool_column(row, "is_active"),
        locked: bool_column(row, "locked"),
        priority: row.get::<i64, _>("priority").max(0) as u32,
        user_agent_id: row.get("user_agent_id"),
        stream_profile_id: row.get("stream_profile_id"),
        refresh_interval_hours: row.get::<i64, _>("refresh_interval_hours").max(0) as u32,
        stale_stream_days: row.get::<i64, _>("stale_stream_days").max(0) as u32,
        custom_properties: json_column(row, "custom_properties"),
    })
}

fn map_profile(row: &SqliteRow) -> M3uAccountProfile {
    M3uAccountProfile {
        id: row.get("id"),
        m3u_account_id: row.get("m3u_account_id"),
        name: row.get("name"),
        is_default: bool_column(row, "is_default"),
        is_active: bool_column(row, "is_active"),
        max_streams: row.get::<i64, _>("max_streams").max(0) as u32,
        search_pattern: row.get("search_pattern"),
        replace_pattern: row.get("replace_pattern"),
    }
}

pub async fn list_accounts(pool: &SqlitePool) -> Result<Vec<M3uAccount>> {
    let rows = sqlx::query("SELECT * FROM m3u_account ORDER BY id")
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_account).collect()
}

pub async fn get_account(pool: &SqlitePool, id: Id) -> Result<Option<M3uAccount>> {
    let row = sqlx::query("SELECT * FROM m3u_account WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_account).transpose()
}

pub async fn create_account(pool: &SqlitePool, account: &M3uAccount) -> Result<M3uAccount> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO m3u_account (name, account_type, server_url, file_path, username, password,
                                  max_streams, is_active, priority, user_agent_id,
                                  stream_profile_id, refresh_interval_hours, stale_stream_days,
                                  custom_properties)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&account.name)
    .bind(account_type_str(account.account_type))
    .bind(&account.server_url)
    .bind(&account.file_path)
    .bind(&account.username)
    .bind(&account.password)
    .bind(i64::from(account.max_streams))
    .bind(account.is_active as i64)
    .bind(i64::from(account.priority))
    .bind(account.user_agent_id)
    .bind(account.stream_profile_id)
    .bind(i64::from(account.refresh_interval_hours))
    .bind(i64::from(account.stale_stream_days))
    .bind(account.custom_properties.to_string())
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_account(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_account(pool: &SqlitePool, account: &M3uAccount) -> Result<M3uAccount> {
    let changed = sqlx::query(&format!(
        "UPDATE m3u_account SET name = ?, account_type = ?, server_url = ?, file_path = ?,
                                username = ?, password = ?, max_streams = ?, is_active = ?,
                                priority = ?, user_agent_id = ?, stream_profile_id = ?,
                                refresh_interval_hours = ?, stale_stream_days = ?,
                                custom_properties = ?, updated_at = {NOW}
         WHERE id = ?"
    ))
    .bind(&account.name)
    .bind(account_type_str(account.account_type))
    .bind(&account.server_url)
    .bind(&account.file_path)
    .bind(&account.username)
    .bind(&account.password)
    .bind(i64::from(account.max_streams))
    .bind(account.is_active as i64)
    .bind(i64::from(account.priority))
    .bind(account.user_agent_id)
    .bind(account.stream_profile_id)
    .bind(i64::from(account.refresh_interval_hours))
    .bind(i64::from(account.stale_stream_days))
    .bind(account.custom_properties.to_string())
    .bind(account.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_account(pool, account.id).await?.ok_or(Error::NotFound)
}

/// The built-in `custom` account is what hand-added streams hang off; deleting
/// it would cascade them away.
pub async fn delete_account(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM m3u_account WHERE id = ? AND locked = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn list_account_profiles(
    pool: &SqlitePool,
    account_id: Option<Id>,
) -> Result<Vec<M3uAccountProfile>> {
    let rows = sqlx::query(
        "SELECT * FROM m3u_account_profile
         WHERE (?1 IS NULL OR m3u_account_id = ?1)
         ORDER BY m3u_account_id, id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(map_profile).collect())
}

pub async fn get_account_profile(pool: &SqlitePool, id: Id) -> Result<Option<M3uAccountProfile>> {
    let row = sqlx::query("SELECT * FROM m3u_account_profile WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_profile))
}

pub async fn create_account_profile(
    pool: &SqlitePool,
    profile: &M3uAccountProfile,
) -> Result<M3uAccountProfile> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO m3u_account_profile (m3u_account_id, name, is_default, is_active,
                                          max_streams, search_pattern, replace_pattern)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(profile.m3u_account_id)
    .bind(&profile.name)
    .bind(profile.is_default as i64)
    .bind(profile.is_active as i64)
    .bind(i64::from(profile.max_streams))
    .bind(&profile.search_pattern)
    .bind(&profile.replace_pattern)
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_account_profile(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_account_profile(
    pool: &SqlitePool,
    profile: &M3uAccountProfile,
) -> Result<M3uAccountProfile> {
    let changed = sqlx::query(
        "UPDATE m3u_account_profile SET name = ?, is_default = ?, is_active = ?, max_streams = ?,
                                        search_pattern = ?, replace_pattern = ?
         WHERE id = ?",
    )
    .bind(&profile.name)
    .bind(profile.is_default as i64)
    .bind(profile.is_active as i64)
    .bind(i64::from(profile.max_streams))
    .bind(&profile.search_pattern)
    .bind(&profile.replace_pattern)
    .bind(profile.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_account_profile(pool, profile.id)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn delete_account_profile(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM m3u_account_profile WHERE id = ? AND is_default = 0")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn list_filters(pool: &SqlitePool, account_id: Option<Id>) -> Result<Vec<M3uFilter>> {
    let rows = sqlx::query(
        "SELECT * FROM m3u_filter WHERE (?1 IS NULL OR m3u_account_id = ?1)
         ORDER BY m3u_account_id, sort_order, id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| M3uFilter {
            id: row.get("id"),
            m3u_account_id: row.get("m3u_account_id"),
            filter_type: row.get("filter_type"),
            regex_pattern: row.get("regex_pattern"),
            exclude: bool_column(row, "exclude"),
            sort_order: row.get("sort_order"),
        })
        .collect())
}

/// The three things a filter can match on, as `sync::filters::FilterTarget`
/// spells them.
///
/// Checked here rather than by the column, which carries no CHECK for the
/// reason `migrations/0001_initial.sql` gives.
const FILTER_TARGETS: [&str; 3] = ["name", "group", "url"];

pub async fn create_filter(pool: &SqlitePool, filter: &M3uFilter) -> Result<M3uFilter> {
    if !FILTER_TARGETS.contains(&filter.filter_type.as_str()) {
        return Err(Error::invalid(format!(
            "unknown filter type `{}`; expected one of {}",
            filter.filter_type,
            FILTER_TARGETS.join(", ")
        )));
    }

    let id: Id = sqlx::query_scalar(
        "INSERT INTO m3u_filter (m3u_account_id, filter_type, regex_pattern, exclude, sort_order)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(filter.m3u_account_id)
    .bind(&filter.filter_type)
    .bind(&filter.regex_pattern)
    .bind(filter.exclude as i64)
    .bind(filter.sort_order)
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    list_filters(pool, Some(filter.m3u_account_id))
        .await?
        .into_iter()
        .find(|f| f.id == id)
        .ok_or(Error::NotFound)
}

pub async fn delete_filter(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM m3u_filter WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn list_group_links(
    pool: &SqlitePool,
    account_id: Option<Id>,
) -> Result<Vec<GroupAccountLink>> {
    let rows = sqlx::query(
        "SELECT * FROM channel_group_m3u_account WHERE (?1 IS NULL OR m3u_account_id = ?1)
         ORDER BY m3u_account_id, channel_group_id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| GroupAccountLink {
            id: row.get("id"),
            channel_group_id: row.get("channel_group_id"),
            m3u_account_id: row.get("m3u_account_id"),
            enabled: bool_column(row, "enabled"),
            auto_channel_sync: bool_column(row, "auto_channel_sync"),
            auto_sync_channel_start: row.get("auto_sync_channel_start"),
            auto_sync_channel_end: row.get("auto_sync_channel_end"),
            custom_properties: json_column(row, "custom_properties"),
        })
        .collect())
}

pub async fn upsert_group_link(pool: &SqlitePool, link: &GroupAccountLink) -> Result<()> {
    sqlx::query(
        "INSERT INTO channel_group_m3u_account
             (channel_group_id, m3u_account_id, enabled, auto_channel_sync,
              auto_sync_channel_start, auto_sync_channel_end, custom_properties)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (channel_group_id, m3u_account_id) DO UPDATE SET
             enabled = excluded.enabled,
             auto_channel_sync = excluded.auto_channel_sync,
             auto_sync_channel_start = excluded.auto_sync_channel_start,
             auto_sync_channel_end = excluded.auto_sync_channel_end,
             custom_properties = excluded.custom_properties",
    )
    .bind(link.channel_group_id)
    .bind(link.m3u_account_id)
    .bind(link.enabled as i64)
    .bind(link.auto_channel_sync as i64)
    .bind(link.auto_sync_channel_start)
    .bind(link.auto_sync_channel_end)
    .bind(link.custom_properties.to_string())
    .execute(pool)
    .await
    .map_err(translate)?;
    Ok(())
}

pub async fn list_server_groups(pool: &SqlitePool) -> Result<Vec<(Id, String)>> {
    let rows = sqlx::query("SELECT id, name FROM server_group ORDER BY name COLLATE NOCASE")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get("id"), row.get("name")))
        .collect())
}

/// How many streams the proxy will admit to serving at once.
///
/// HDHomeRun advertises this as `TunerCount` and Xtream as `max_connections`,
/// and Plex will not start an `n+1`th recording, so a number below the real
/// provider allowance silently costs the user tuners they paid for.
///
/// Derived from the provider allowances rather than configured: the sum of
/// every enabled profile's `max_streams` on every enabled account, plus the
/// hand-added streams, which no provider limit covers. A profile with
/// `max_streams = 0` means unlimited, and there is no honest number for that —
/// callers pass what their protocol's clients tolerate as a stand-in.
///
/// Profiles of `locked` accounts are excluded: that is the built-in `custom`
/// account, whose streams are already counted individually below.
pub async fn tuner_count(pool: &SqlitePool, unlimited_default: u32) -> Result<u32> {
    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE p.max_streams = 0) AS unlimited,
                COALESCE(SUM(p.max_streams), 0)           AS limited
           FROM m3u_account_profile p
           JOIN m3u_account a ON a.id = p.m3u_account_id
          WHERE p.is_active = 1 AND a.is_active = 1 AND a.locked = 0",
    )
    .fetch_one(pool)
    .await?;
    let unlimited: i64 = row.get("unlimited");
    let limited: i64 = row.get("limited");

    let custom: i64 = sqlx::query("SELECT COUNT(*) AS n FROM stream WHERE is_custom = 1")
        .fetch_one(pool)
        .await?
        .get("n");

    let base = if unlimited > 0 {
        i64::from(unlimited_default)
    } else {
        limited
    };
    // At least one: advertising zero tuners makes Plex drop the device.
    Ok((base + custom).clamp(1, i64::from(u32::MAX)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn draft(name: &str) -> M3uAccount {
        M3uAccount {
            id: 0,
            name: name.to_owned(),
            server_url: Some("https://provider.example/get.php".into()),
            file_path: None,
            username: Some("user".into()),
            password: Some("pass".into()),
            account_type: M3uAccountType::Standard,
            max_streams: 2,
            is_active: true,
            locked: false,
            priority: 0,
            user_agent_id: None,
            stream_profile_id: None,
            refresh_interval_hours: 24,
            stale_stream_days: 7,
            custom_properties: serde_json::json!({"enable_vod": false}),
        }
    }

    #[tokio::test]
    async fn the_custom_account_ships_locked_and_undeletable() {
        let pool = pool().await;
        let custom = get_account(&pool, 1).await.unwrap().unwrap();
        assert_eq!(custom.name, "custom");
        assert!(custom.locked);
        assert!(matches!(
            delete_account(&pool, 1).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn accounts_round_trip_including_credentials_and_type() {
        let pool = pool().await;
        let mut account = draft("Provider");
        account.account_type = M3uAccountType::XtreamCodes;
        let created = create_account(&pool, &account).await.unwrap();

        assert_eq!(created.account_type, M3uAccountType::XtreamCodes);
        assert_eq!(created.username.as_deref(), Some("user"));
        assert_eq!(created.custom_properties["enable_vod"], false);

        let mut edited = created.clone();
        edited.max_streams = 5;
        assert_eq!(save_account(&pool, &edited).await.unwrap().max_streams, 5);

        delete_account(&pool, created.id).await.unwrap();
        assert!(get_account(&pool, created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn account_profiles_keep_their_patterns_verbatim() {
        let pool = pool().await;
        let account = create_account(&pool, &draft("Provider")).await.unwrap();

        // A lookahead: legal in the PCRE-flavoured engines user patterns come
        // from, and the exact thing Rust's `regex` crate refuses to compile.
        let created = create_account_profile(
            &pool,
            &M3uAccountProfile {
                id: 0,
                m3u_account_id: account.id,
                name: "HD only".into(),
                is_default: false,
                is_active: true,
                max_streams: 1,
                search_pattern: r"^(?=.*HD)(.*)$".into(),
                replace_pattern: "$1".into(),
            },
        )
        .await
        .unwrap();

        let reloaded = get_account_profile(&pool, created.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.search_pattern, r"^(?=.*HD)(.*)$");
        assert_eq!(reloaded.replace_pattern, "$1");
    }

    #[tokio::test]
    async fn the_default_profile_cannot_be_deleted() {
        let pool = pool().await;
        assert!(matches!(
            delete_account_profile(&pool, 1).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn tuners_come_from_the_provider_allowances() {
        let pool = pool().await;

        // Only the built-in `custom` account exists on a fresh install, and its
        // profile is excluded, so there is nothing to count. One, not zero:
        // a tuner advertising zero tuners is a tuner Plex drops.
        assert_eq!(tuner_count(&pool, 10).await.unwrap(), 1);

        let account = create_account(&pool, &draft("Provider")).await.unwrap();
        create_account_profile(
            &pool,
            &M3uAccountProfile {
                id: 0,
                m3u_account_id: account.id,
                name: "Provider Default".into(),
                is_default: true,
                is_active: true,
                max_streams: 3,
                search_pattern: String::new(),
                replace_pattern: String::new(),
            },
        )
        .await
        .unwrap();
        assert_eq!(tuner_count(&pool, 10).await.unwrap(), 3);

        // A hand-added stream is covered by no provider limit, so it adds one.
        sqlx::query("INSERT INTO stream (name, url, is_custom) VALUES ('Hand added', 'x', 1)")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(tuner_count(&pool, 10).await.unwrap(), 4);

        // Unlimited has no honest number, so the caller's stand-in stands in —
        // and the two protocols pass different ones.
        sqlx::query("UPDATE m3u_account_profile SET max_streams = 0 WHERE m3u_account_id = ?")
            .bind(account.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(tuner_count(&pool, 10).await.unwrap(), 11);
        assert_eq!(tuner_count(&pool, 50).await.unwrap(), 51);

        // A disabled account contributes nothing: its streams cannot be served.
        sqlx::query("UPDATE m3u_account SET is_active = 0 WHERE id = ?")
            .bind(account.id)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(tuner_count(&pool, 10).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn group_links_upsert_rather_than_duplicate() {
        let pool = pool().await;
        let group = super::super::channels::create_group(&pool, "Sports", (None, None))
            .await
            .unwrap();

        let mut link = GroupAccountLink {
            id: 0,
            channel_group_id: group.id,
            m3u_account_id: 1,
            enabled: true,
            auto_channel_sync: false,
            auto_sync_channel_start: None,
            auto_sync_channel_end: None,
            custom_properties: serde_json::json!({}),
        };
        upsert_group_link(&pool, &link).await.unwrap();

        link.auto_channel_sync = true;
        link.auto_sync_channel_start = Some(100.0);
        upsert_group_link(&pool, &link).await.unwrap();

        let links = list_group_links(&pool, Some(1)).await.unwrap();
        assert_eq!(links.len(), 1);
        assert!(links[0].auto_channel_sync);
        assert_eq!(links[0].auto_sync_channel_start, Some(100.0));
    }

    #[tokio::test]
    async fn filters_are_ordered_and_typed() {
        let pool = pool().await;
        create_filter(
            &pool,
            &M3uFilter {
                id: 0,
                m3u_account_id: 1,
                filter_type: "group".into(),
                regex_pattern: r"(?i)adult".into(),
                exclude: true,
                sort_order: 1,
            },
        )
        .await
        .unwrap();

        let filters = list_filters(&pool, Some(1)).await.unwrap();
        assert_eq!(filters.len(), 1);
        assert!(filters[0].exclude);

        let bad = create_filter(
            &pool,
            &M3uFilter {
                id: 0,
                m3u_account_id: 1,
                filter_type: "nonsense".into(),
                regex_pattern: ".*".into(),
                exclude: false,
                sort_order: 0,
            },
        )
        .await;
        assert!(bad.is_err());
    }

    /// The column has no CHECK, so these are the only thing standing between a
    /// row this build cannot interpret and it being read as something else.
    #[tokio::test]
    async fn an_unknown_account_type_is_refused_rather_than_read_as_standard() {
        let pool = pool().await;
        create_account(&pool, &draft("Provider")).await.unwrap();

        // What a downgrade after a migration that added a third type would
        // meet. Reading it as `standard` would fetch an Xtream-like provider as
        // a plain playlist: wrong URL, wrong hash, catalogue rebuilt.
        sqlx::query("UPDATE m3u_account SET account_type = 'schedules_direct' WHERE id = 2")
            .execute(&pool)
            .await
            .unwrap();

        let error = get_account(&pool, 2).await.expect_err("read as standard");
        assert!(error.to_string().contains("schedules_direct"), "{error}");
        assert!(list_accounts(&pool).await.is_err());
    }
}

#[cfg(test)]
mod pattern_tests {
    use super::*;

    #[test]
    fn js_backreferences_become_rust_ones() {
        assert_eq!(js_backrefs_to_rust("^(.*)$1$"), r"^(.*)\1$");
        assert_eq!(js_backrefs_to_rust("$1$2"), r"\1\2");
    }

    #[test]
    fn anchors_and_escapes_are_left_alone() {
        // A trailing `$` is an anchor, not a backreference.
        assert_eq!(js_backrefs_to_rust("^(.*)$"), "^(.*)$");
        assert_eq!(js_backrefs_to_rust(r"price\$1"), r"price\$1");
        assert_eq!(js_backrefs_to_rust(""), "");
    }

    #[test]
    // The second assertion is the point of the test — `regex` must reject what
    // `fancy_regex` accepts — but clippy const-evaluates the literal and turns
    // that rejection into a hard error before the test can observe it.
    #[allow(clippy::invalid_regex)]
    fn lookaround_compiles_where_the_regex_crate_would_refuse() {
        assert_eq!(check_pattern(r"^(?=.*HD)(.*)$"), PatternCheck::Ok);
        assert!(regex::Regex::new(r"^(?=.*HD)(.*)$").is_err());
    }

    #[test]
    fn a_backreference_pattern_is_reported_as_rewritten() {
        assert_eq!(
            check_pattern("(a)$1"),
            PatternCheck::Rewritten(r"(a)\1".to_owned())
        );
    }

    #[test]
    fn an_uncompilable_pattern_is_reported_as_failed() {
        assert!(matches!(
            check_pattern("(unclosed"),
            PatternCheck::Failed(_)
        ));
        assert_eq!(check_pattern(""), PatternCheck::Ok);
    }
}
