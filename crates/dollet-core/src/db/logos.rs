//! Channel artwork.
//!
//! A logo is identified by its URL, which is why the column is unique: M3U
//! refresh re-encounters the same artwork on every pass and must reuse the row
//! rather than accumulate duplicates.

use std::collections::HashMap;

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::domain::{Id, Logo};
use crate::{Error, Result};

use super::{Page, Paging, like_pattern, order_clause, translate};

const ORDERING: &[(&str, &str)] = &[
    ("name", "name COLLATE NOCASE"),
    ("url", "url"),
    ("id", "id"),
];

fn map(row: &SqliteRow) -> Logo {
    Logo {
        id: row.get("id"),
        name: row.get("name"),
        url: row.get("url"),
    }
}

pub async fn list(
    pool: &SqlitePool,
    search: Option<&str>,
    ordering: Option<&str>,
    paging: Option<Paging>,
) -> Result<Page<Logo>> {
    let search = search.unwrap_or_default();
    let pattern = like_pattern(search);
    let filter = "WHERE (?1 = '' OR name LIKE ?2 ESCAPE '\\' OR url LIKE ?2 ESCAPE '\\')";

    let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM logo {filter}"))
        .bind(search)
        .bind(&pattern)
        .fetch_one(pool)
        .await?;

    let order = order_clause(ordering, ORDERING, "name COLLATE NOCASE ASC");
    let mut sql = format!("SELECT * FROM logo {filter} ORDER BY {order}");
    if paging.is_some() {
        sql.push_str(" LIMIT ?3 OFFSET ?4");
    }

    let mut query = sqlx::query(&sql).bind(search).bind(&pattern);
    if let Some(paging) = paging {
        query = query.bind(paging.limit()).bind(paging.offset());
    }

    let rows = query.fetch_all(pool).await?;
    Ok(Page {
        count,
        results: rows.iter().map(map).collect(),
    })
}

pub async fn get(pool: &SqlitePool, id: Id) -> Result<Option<Logo>> {
    let row = sqlx::query("SELECT * FROM logo WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map))
}

pub async fn create(pool: &SqlitePool, logo: &Logo) -> Result<Logo> {
    let id: Id = sqlx::query_scalar("INSERT INTO logo (name, url) VALUES (?, ?) RETURNING id")
        .bind(&logo.name)
        .bind(&logo.url)
        .fetch_one(pool)
        .await
        .map_err(translate)?;

    get(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save(pool: &SqlitePool, logo: &Logo) -> Result<Logo> {
    let changed = sqlx::query("UPDATE logo SET name = ?, url = ? WHERE id = ?")
        .bind(&logo.name)
        .bind(&logo.url)
        .bind(logo.id)
        .execute(pool)
        .await
        .map_err(translate)?
        .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get(pool, logo.id).await?.ok_or(Error::NotFound)
}

pub async fn delete(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM logo WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

pub async fn delete_many(pool: &SqlitePool, ids: &[Id]) -> Result<u64> {
    super::delete_by_id(pool, "logo", ids).await
}

/// How many channels reference each logo, counting override assignments.
///
/// The override table is the reason this is not a plain `GROUP BY`: a
/// hand-assigned logo lives there, and missing it would report an in-use logo
/// as orphaned and offer to delete it.
pub async fn usage(pool: &SqlitePool) -> Result<HashMap<Id, i64>> {
    let rows = sqlx::query(
        "SELECT logo_id AS id, COUNT(*) AS n FROM effective_channel
         WHERE logo_id IS NOT NULL GROUP BY logo_id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| (row.get::<Id, _>("id"), row.get::<i64, _>("n")))
        .collect())
}

pub async fn delete_unused(pool: &SqlitePool) -> Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM logo WHERE id NOT IN
           (SELECT logo_id FROM effective_channel WHERE logo_id IS NOT NULL)",
    )
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

    async fn seed(pool: &SqlitePool) -> Vec<Logo> {
        let mut logos = Vec::new();
        for (name, url) in [
            ("VRIX", "https://art.example/vrix.png"),
            ("vrix 2", "https://art.example/vrix2.png"),
            ("ZOR", "https://art.example/zor.png"),
        ] {
            logos.push(
                create(
                    pool,
                    &Logo {
                        id: 0,
                        name: name.into(),
                        url: url.into(),
                    },
                )
                .await
                .unwrap(),
            );
        }
        logos
    }

    #[tokio::test]
    async fn search_is_case_insensitive_substring_matching() {
        let pool = pool().await;
        seed(&pool).await;

        // The reason FTS5 is not used: a token index matches on prefixes, so
        // it would not find `RIX` inside `VRIX`. Search here is substring and
        // case-insensitive throughout.
        let page = list(&pool, Some("rix"), None, None).await.unwrap();
        assert_eq!(page.count, 2);
        assert_eq!(page.results.len(), 2);
    }

    #[tokio::test]
    async fn paging_reports_the_unpaginated_total() {
        let pool = pool().await;
        seed(&pool).await;

        let page = list(
            &pool,
            None,
            Some("name"),
            Some(Paging {
                page: 2,
                page_size: 2,
            }),
        )
        .await
        .unwrap();

        assert_eq!(page.count, 3);
        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].name, "ZOR");
    }

    #[tokio::test]
    async fn an_unknown_ordering_falls_back_instead_of_reaching_sql() {
        let pool = pool().await;
        seed(&pool).await;

        let page = list(&pool, None, Some("name; DROP TABLE logo"), None)
            .await
            .unwrap();
        assert_eq!(page.results.len(), 3);
    }

    #[tokio::test]
    async fn usage_counts_override_assignments_too() {
        let pool = pool().await;
        let logos = seed(&pool).await;

        sqlx::query("INSERT INTO channel (id, uuid, name, logo_id) VALUES (1, 'u1', 'A', ?)")
            .bind(logos[0].id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO channel (id, uuid, name, logo_id) VALUES (2, 'u2', 'B', ?)")
            .bind(logos[0].id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO channel_override (channel_id, logo_id) VALUES (2, ?)")
            .bind(logos[1].id)
            .execute(&pool)
            .await
            .unwrap();

        let usage = usage(&pool).await.unwrap();
        assert_eq!(usage.get(&logos[0].id), Some(&1));
        assert_eq!(usage.get(&logos[1].id), Some(&1));
        assert_eq!(usage.get(&logos[2].id), None);

        assert_eq!(delete_unused(&pool).await.unwrap(), 1);
        assert!(get(&pool, logos[2].id).await.unwrap().is_none());
        assert!(get(&pool, logos[1].id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn the_same_url_cannot_be_stored_twice() {
        let pool = pool().await;
        let logos = seed(&pool).await;

        let duplicate = create(
            &pool,
            &Logo {
                id: 0,
                name: "VRIX copy".into(),
                url: logos[0].url.clone(),
            },
        )
        .await;
        assert!(matches!(duplicate, Err(Error::Conflict(_))));
    }
}
