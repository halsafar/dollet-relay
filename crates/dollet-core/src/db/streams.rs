//! Provider streams.
//!
//! This is the largest table the UI paginates, and the one whose search is
//! `LIKE '%x%'` rather than a token index: `HD` has to keep matching
//! `SPORTSHD`, which is exactly what FTS5 would stop doing.

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::domain::{Id, Stream};
use crate::{Error, Result};

use super::{
    NOW, Page, Paging, bool_column, datetime_column, json_column, like_pattern, order_clause,
    sql_timestamp, translate,
};
use chrono::Utc;

const ORDERING: &[(&str, &str)] = &[
    ("name", "s.name COLLATE NOCASE"),
    ("group_name", "g.name COLLATE NOCASE"),
    ("tvg_id", "s.tvg_id"),
    ("id", "s.id"),
];

const SELECT: &str =
    "SELECT s.* FROM stream s LEFT JOIN channel_group g ON g.id = s.channel_group_id";

/// `?1` search, `?2` pattern, `?3` group, `?4` account.
const FILTER: &str = "WHERE (?1 = '' OR s.name LIKE ?2 ESCAPE '\\' OR g.name LIKE ?2 ESCAPE '\\')
                        AND (?3 IS NULL OR s.channel_group_id = ?3)
                        AND (?4 IS NULL OR s.m3u_account_id = ?4)";

#[derive(Debug, Default, Clone)]
pub struct StreamFilter<'a> {
    pub search: Option<&'a str>,
    pub group_id: Option<Id>,
    pub m3u_account_id: Option<Id>,
}

fn map(row: &SqliteRow) -> Stream {
    Stream {
        id: row.get("id"),
        name: row.get("name"),
        url: row.get("url"),
        logo_url: row.get("logo_url"),
        tvg_id: row.get("tvg_id"),
        channel_group_id: row.get("channel_group_id"),
        m3u_account_id: row.get("m3u_account_id"),
        stream_profile_id: row.get("stream_profile_id"),
        is_custom: bool_column(row, "is_custom"),
        is_adult: bool_column(row, "is_adult"),
        stream_id: row.get("stream_id"),
        stream_chno: row.get("stream_chno"),
        stream_hash: row.get("stream_hash"),
        last_seen: datetime_column(row, "last_seen"),
        is_stale: bool_column(row, "is_stale"),
        is_catchup: bool_column(row, "is_catchup"),
        catchup_days: row.get::<i64, _>("catchup_days").max(0) as u32,
        custom_properties: json_column(row, "custom_properties"),
    }
}

pub async fn list(
    pool: &SqlitePool,
    filter: &StreamFilter<'_>,
    ordering: Option<&str>,
    paging: Option<Paging>,
) -> Result<Page<Stream>> {
    let search = filter.search.unwrap_or_default();
    let pattern = like_pattern(search);

    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM stream s
         LEFT JOIN channel_group g ON g.id = s.channel_group_id {FILTER}"
    ))
    .bind(search)
    .bind(&pattern)
    .bind(filter.group_id)
    .bind(filter.m3u_account_id)
    .fetch_one(pool)
    .await?;

    let order = order_clause(ordering, ORDERING, "s.name COLLATE NOCASE ASC");
    let mut sql = format!("{SELECT} {FILTER} ORDER BY {order}");
    if paging.is_some() {
        sql.push_str(" LIMIT ?5 OFFSET ?6");
    }

    let mut query = sqlx::query(&sql)
        .bind(search)
        .bind(&pattern)
        .bind(filter.group_id)
        .bind(filter.m3u_account_id);
    if let Some(paging) = paging {
        query = query.bind(paging.limit()).bind(paging.offset());
    }

    let rows = query.fetch_all(pool).await?;
    Ok(Page {
        count,
        results: rows.iter().map(map).collect(),
    })
}

pub async fn get(pool: &SqlitePool, id: Id) -> Result<Option<Stream>> {
    let row = sqlx::query("SELECT * FROM stream WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map))
}

/// Streams for a channel, already in failover order. The streaming engine
/// takes this list and nothing else from the database.
pub async fn for_channel(pool: &SqlitePool, channel_id: Id) -> Result<Vec<Stream>> {
    let rows = sqlx::query(
        "SELECT s.* FROM stream s
         JOIN channel_stream cs ON cs.stream_id = s.id
         WHERE cs.channel_id = ?
         ORDER BY cs.sort_order, s.id",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(map).collect())
}

pub async fn create(pool: &SqlitePool, stream: &Stream) -> Result<Stream> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO stream (name, url, logo_url, tvg_id, channel_group_id, m3u_account_id,
                             stream_profile_id, is_custom, is_adult, stream_id, stream_chno,
                             stream_hash, is_stale, is_catchup, catchup_days, last_seen,
                             custom_properties)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&stream.name)
    .bind(&stream.url)
    .bind(&stream.logo_url)
    .bind(&stream.tvg_id)
    .bind(stream.channel_group_id)
    .bind(stream.m3u_account_id)
    .bind(stream.stream_profile_id)
    .bind(stream.is_custom as i64)
    .bind(stream.is_adult as i64)
    .bind(stream.stream_id)
    .bind(stream.stream_chno)
    .bind(&stream.stream_hash)
    .bind(stream.is_stale as i64)
    .bind(stream.is_catchup as i64)
    .bind(i64::from(stream.catchup_days))
    .bind(sql_timestamp(stream.last_seen))
    .bind(stream.custom_properties.to_string())
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save(pool: &SqlitePool, stream: &Stream) -> Result<Stream> {
    let changed = sqlx::query(&format!(
        "UPDATE stream SET name = ?, url = ?, logo_url = ?, tvg_id = ?, channel_group_id = ?,
                           m3u_account_id = ?, stream_profile_id = ?, is_custom = ?, is_adult = ?,
                           stream_id = ?, stream_chno = ?, stream_hash = ?, is_stale = ?,
                           is_catchup = ?, catchup_days = ?, last_seen = ?,
                           custom_properties = ?, updated_at = {NOW}
         WHERE id = ?"
    ))
    .bind(&stream.name)
    .bind(&stream.url)
    .bind(&stream.logo_url)
    .bind(&stream.tvg_id)
    .bind(stream.channel_group_id)
    .bind(stream.m3u_account_id)
    .bind(stream.stream_profile_id)
    .bind(stream.is_custom as i64)
    .bind(stream.is_adult as i64)
    .bind(stream.stream_id)
    .bind(stream.stream_chno)
    .bind(&stream.stream_hash)
    .bind(stream.is_stale as i64)
    .bind(stream.is_catchup as i64)
    .bind(i64::from(stream.catchup_days))
    .bind(sql_timestamp(stream.last_seen))
    .bind(stream.custom_properties.to_string())
    .bind(stream.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get(pool, stream.id).await?.ok_or(Error::NotFound)
}

pub async fn delete(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM stream WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Record that a batch of streams was seen again in this refresh.
///
/// Separate from `save` because reconcile already decided nothing else about
/// them changed: rewriting every column would be 61 pointless writes per
/// refresh, and on a full provider feed rather more.
pub async fn mark_seen(pool: &SqlitePool, ids: &[Id], at: chrono::DateTime<Utc>) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    let mut touched = 0;
    for id in ids {
        touched += sqlx::query("UPDATE stream SET last_seen = ?, is_stale = 0 WHERE id = ?")
            .bind(sql_timestamp(at))
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(touched)
}

/// Flag streams the provider did not send this time.
///
/// Stale is not deleted: a provider dropping a stream for one refresh is
/// routine, and deleting on the first miss would take the channel's failover
/// list with it. Deletion waits for the retention window.
pub async fn mark_stale(pool: &SqlitePool, ids: &[Id]) -> Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }

    let mut tx = pool.begin().await?;
    let mut marked = 0;
    for id in ids {
        marked += sqlx::query("UPDATE stream SET is_stale = 1 WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    tx.commit().await?;
    Ok(marked)
}

pub async fn delete_many(pool: &SqlitePool, ids: &[Id]) -> Result<u64> {
    super::delete_by_id(pool, "stream", ids).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn draft(name: &str) -> Stream {
        Stream {
            id: 0,
            name: name.to_owned(),
            url: Some("https://provider.example/live/1".into()),
            logo_url: None,
            tvg_id: None,
            channel_group_id: None,
            m3u_account_id: Some(1),
            stream_profile_id: None,
            is_custom: false,
            is_adult: false,
            stream_id: None,
            stream_chno: None,
            stream_hash: None,
            last_seen: chrono::Utc::now(),
            is_stale: false,
            is_catchup: false,
            catchup_days: 0,
            custom_properties: serde_json::json!({"group-title": "Sports"}),
        }
    }

    #[tokio::test]
    async fn custom_properties_survive_the_round_trip() {
        let pool = pool().await;
        let created = create(&pool, &draft("VRIX")).await.unwrap();
        assert_eq!(created.custom_properties["group-title"], "Sports");
        assert!(
            created.last_seen.timestamp() > 0,
            "timestamp did not decode"
        );
    }

    #[tokio::test]
    async fn substring_search_matches_mid_word() {
        let pool = pool().await;
        create(&pool, &draft("SPORTSHD")).await.unwrap();
        create(&pool, &draft("News SD")).await.unwrap();

        let page = list(
            &pool,
            &StreamFilter {
                search: Some("HD"),
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(page.count, 1);
        assert_eq!(page.results[0].name, "SPORTSHD");
    }

    #[tokio::test]
    async fn search_also_matches_the_group_name() {
        let pool = pool().await;
        let group: Id =
            sqlx::query_scalar("INSERT INTO channel_group (name) VALUES ('Canada') RETURNING id")
                .fetch_one(&pool)
                .await
                .unwrap();

        let mut stream = draft("ZOR1");
        stream.channel_group_id = Some(group);
        create(&pool, &stream).await.unwrap();
        create(&pool, &draft("Unrelated")).await.unwrap();

        let page = list(
            &pool,
            &StreamFilter {
                search: Some("canad"),
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(page.count, 1);
    }

    #[tokio::test]
    async fn for_channel_returns_failover_order() {
        let pool = pool().await;
        let a = create(&pool, &draft("first")).await.unwrap();
        let b = create(&pool, &draft("second")).await.unwrap();
        sqlx::query("INSERT INTO channel (id, uuid, name) VALUES (1, 'u', 'C')")
            .execute(&pool)
            .await
            .unwrap();
        super::super::channels::set_streams(&pool, 1, &[b.id, a.id])
            .await
            .unwrap();

        let streams = for_channel(&pool, 1).await.unwrap();
        assert_eq!(
            streams.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![b.id, a.id]
        );
    }

    #[tokio::test]
    async fn bulk_delete_batches_and_reports_a_count() {
        let pool = pool().await;
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(create(&pool, &draft(&format!("s{i}"))).await.unwrap().id);
        }

        assert_eq!(delete_many(&pool, &ids).await.unwrap(), 5);
        assert_eq!(
            list(&pool, &StreamFilter::default(), None, None)
                .await
                .unwrap()
                .count,
            0
        );
    }

    #[tokio::test]
    async fn deleting_an_account_takes_its_streams_with_it() {
        let pool = pool().await;
        create(&pool, &draft("orphan")).await.unwrap();

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM m3u_account WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            list(&pool, &StreamFilter::default(), None, None)
                .await
                .unwrap()
                .count,
            0
        );
    }
}
