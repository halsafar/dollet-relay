//! Persistence.
//!
//! The pool setup below is load-bearing and deliberately in one place: every
//! connection gets the same pragmas, and WAL checkpointing depends on them.
//! Extend this with query modules rather than replacing it.

pub mod channel_profiles;
pub mod channels;
pub mod epg;
pub mod events;
pub mod jobs;
pub mod logos;
pub mod m3u;
pub mod notifications;
pub mod profiles;
pub mod streams;
pub mod users;

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use serde_json::Value as Json;
use sqlx::sqlite::SqliteRow;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::Result;
use crate::domain::Id;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Delete rows by id, in batches of 200 with one transaction each: a long
/// write transaction stops the WAL checkpointing, and this process always has
/// a reader holding a snapshot.
pub(crate) async fn delete_by_id(pool: &SqlitePool, table: &str, ids: &[Id]) -> Result<u64> {
    let statement = format!("DELETE FROM {table} WHERE id = ?");
    let mut deleted = 0;
    for chunk in ids.chunks(200) {
        let mut tx = pool.begin().await?;
        for id in chunk {
            deleted += sqlx::query(&statement)
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(deleted)
}

/// JSON columns are TEXT, and a row whose JSON has rotted must not take out
/// the whole list endpoint it appears in.
pub(crate) fn json_column(row: &SqliteRow, name: &str) -> Json {
    row.try_get::<Option<String>, _>(name)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| Json::Object(Default::default()))
}

pub(crate) fn bool_column(row: &SqliteRow, name: &str) -> bool {
    row.try_get::<i64, _>(name).unwrap_or(0) != 0
}

/// Timestamps are TEXT, written either by a column default or by
/// [`sql_timestamp`]. A row whose timestamp is unreadable sorts oldest rather
/// than failing the query.
pub(crate) fn datetime_column(row: &SqliteRow, name: &str) -> chrono::DateTime<chrono::Utc> {
    row.try_get::<chrono::DateTime<chrono::Utc>, _>(name)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
}

/// SQL that writes "now" in the one canonical timestamp format.
///
/// Inlined into statements rather than bound, so an UPDATE does not have to
/// thread a parameter through just to touch `updated_at`.
pub(crate) const NOW: &str = "(strftime('%Y-%m-%d %H:%M:%f', 'now') || '+00:00')";

/// Render a timestamp the way the column defaults do.
///
/// Binding a `DateTime<Utc>` directly would store sqlx's own encoding, which
/// uses a `T` separator — and since these columns are compared as text, `'T'`
/// sorting after `' '` silently inverts `ORDER BY` for rows written on the
/// same day by the two different paths.
pub fn sql_timestamp(value: chrono::DateTime<chrono::Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S%.3f+00:00").to_string()
}

/// Neutralise LIKE wildcards in a user-supplied search term.
///
/// Without this, searching for `%` matches every row and a channel with an
/// underscore in its name cannot be searched for literally. Paired with
/// `ESCAPE '\\'` on every LIKE that uses it.
pub(crate) fn like_pattern(search: &str) -> String {
    let mut escaped = String::with_capacity(search.len() + 2);
    for c in search.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("%{escaped}%")
}

/// UUIDs are TEXT so they stay readable in `sqlite3`; the streaming endpoint
/// looks channels up by this value, so an unparseable one must not panic.
pub(crate) fn uuid_column(row: &SqliteRow, name: &str) -> uuid::Uuid {
    row.try_get::<String, _>(name)
        .ok()
        .and_then(|raw| uuid::Uuid::parse_str(&raw).ok())
        .unwrap_or(uuid::Uuid::nil())
}

/// A duplicate name is a user mistake, not a server fault, and must come back
/// as 409 rather than 500.
pub(crate) fn translate(err: sqlx::Error) -> crate::Error {
    if let sqlx::Error::Database(ref db) = err
        && db.is_unique_violation()
    {
        return crate::Error::Conflict(db.message().to_owned());
    }
    crate::Error::Database(err)
}

/// One page of a list endpoint, alongside the unpaginated total.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub count: i64,
    pub results: Vec<T>,
}

/// Page number and size, as the list endpoints receive them.
#[derive(Debug, Clone, Copy)]
pub struct Paging {
    pub page: u32,
    pub page_size: u32,
}

impl Paging {
    pub fn offset(&self) -> i64 {
        i64::from(self.page.saturating_sub(1)) * i64::from(self.page_size)
    }

    pub fn limit(&self) -> i64 {
        i64::from(self.page_size)
    }
}

/// Resolve a client-supplied `ordering` value against a whitelist.
///
/// The result is interpolated into SQL, so nothing outside the whitelist may
/// ever reach it. A leading `-` means descending, as in the API's `ordering`
/// parameter. An
/// expression containing `{dir}` places the direction itself, which is how a
/// nullable column gets its `IS NULL` tie-break on both sides.
pub(crate) fn order_clause(
    requested: Option<&str>,
    allowed: &[(&str, &str)],
    fallback: &str,
) -> String {
    let Some(requested) = requested else {
        return fallback.to_owned();
    };

    let (field, descending) = match requested.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (requested, false),
    };

    let direction = if descending { "DESC" } else { "ASC" };
    match allowed.iter().find(|(name, _)| *name == field) {
        Some((_, expr)) if expr.contains("{dir}") => expr.replace("{dir}", direction),
        Some((_, expr)) => format!("{expr} {direction}"),
        None => fallback.to_owned(),
    }
}

/// Open the database, applying pragmas to every connection.
///
/// WAL keeps readers from blocking the writer. The risk it does *not* solve,
/// and the one that actually bites a server holding long-lived streaming
/// responses, is checkpoint starvation: the WAL cannot truncate while any
/// reader holds an older snapshot. Two rules follow, and they are invariants
/// rather than suggestions:
///
/// - never hold a read transaction across an `.await`
/// - commit bulk work in batches, never as one long transaction
pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await?;

    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    MIGRATOR.run(pool).await.map_err(sqlx::Error::from)?;
    Ok(())
}

/// Bytes of WAL not yet checkpointed. Surfaced on `/health` so unbounded
/// growth is visible before it becomes a disk problem.
///
/// `wal_checkpoint` returns `(busy, log_pages, checkpointed_pages)`. Both of
/// the last two are counted from the start of the WAL, so the *un*-checkpointed
/// remainder is their difference — reporting `log` alone shows the file's full
/// size, which after a bulk ingest stays at hundreds of megabytes for as long
/// as the file does, whatever the checkpointer has managed. Reading the first
/// column instead would yield the busy flag, which is 0 whenever nothing is
/// contending and so reports a healthy WAL forever.
///
/// Running PASSIVE rather than a bare query is deliberate: it also takes the
/// opportunity to checkpoint, and on a server that always has readers the
/// checkpointer needs every chance it can get.
pub async fn wal_bytes(pool: &SqlitePool) -> Result<i64> {
    let log_pages: i64 = sqlx::query_as::<_, (i64, i64, i64)>("PRAGMA wal_checkpoint(PASSIVE)")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        // Both are -1 when WAL mode is off, which must not read as a backlog.
        .map(|(_busy, log, checkpointed)| (log - checkpointed).max(0))
        .unwrap_or(0);

    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(pool)
        .await
        .unwrap_or(4096);

    Ok(log_pages.max(0) * page_size)
}

/// Check the WAL back into the database and shrink the file.
///
/// Called after a bulk ingest, which is the only thing here that writes enough
/// to matter: a PASSIVE checkpoint leaves the file at its high-water mark, so a
/// one-off import of half a million programmes costs that much disk until the
/// next restart. TRUNCATE is the only mode that gives it back.
///
/// Best-effort by design. It waits for readers to clear and gives up when they
/// do not, and this process always has streaming readers — failing an otherwise
/// successful refresh over a housekeeping step would be the wrong trade.
pub async fn checkpoint_wal(pool: &SqlitePool) {
    if let Err(e) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await
    {
        tracing::debug!(error = %e, "WAL not truncated");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_wildcards_in_a_search_term_are_literal() {
        assert_eq!(like_pattern("vrix"), "%vrix%");
        assert_eq!(like_pattern("%"), r"%\%%");
        assert_eq!(like_pattern("VRIX_HD"), r"%VRIX\_HD%");
        assert_eq!(like_pattern(r"a\b"), r"%a\\b%");
    }

    #[test]
    fn ordering_only_ever_emits_whitelisted_sql() {
        let allowed = &[("name", "name"), ("number", "n IS NULL {dir}, n {dir}")][..];

        assert_eq!(order_clause(None, allowed, "id ASC"), "id ASC");
        assert_eq!(order_clause(Some("name"), allowed, "id ASC"), "name ASC");
        assert_eq!(order_clause(Some("-name"), allowed, "id ASC"), "name DESC");
        assert_eq!(
            order_clause(Some("number"), allowed, "id ASC"),
            "n IS NULL ASC, n ASC"
        );
        assert_eq!(
            order_clause(Some("-number"), allowed, "id ASC"),
            "n IS NULL DESC, n DESC"
        );
        assert_eq!(
            order_clause(Some("name; DROP TABLE t"), allowed, "id ASC"),
            "id ASC"
        );
    }

    #[test]
    fn timestamps_render_in_the_schemas_format() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-11T03:00:00.5Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(sql_timestamp(at), "2026-09-11 03:00:00.500+00:00");
    }
}
