//! Guide sources, their channels, and their programmes.
//!
//! Programme writes are the only bulk path in this project that is not tiny:
//! a full XMLTV feed is hundreds of thousands of rows. They are committed in
//! batches, never as one transaction, because a long write transaction keeps
//! the WAL from checkpointing while streaming readers hold snapshots.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use super::datetime_column;
use crate::domain::{EpgData, EpgSource, EpgSourceType, Id, Program};
use crate::{Error, Result};

use super::{NOW, bool_column, json_column, like_pattern, sql_timestamp, translate};

/// Rows per transaction for programme import. Large enough that per-commit
/// fsync cost is amortised, small enough that the WAL can checkpoint between.
const BATCH: usize = 500;

/// Read a stored `source_type`.
///
/// An unknown value is an error rather than XMLTV, for the same reason
/// `db::m3u::account_type` refuses to guess: the column has no CHECK, so a
/// build that predates a new source type must say it cannot read the row
/// instead of fetching a Schedules Direct source as if it were an XMLTV file.
pub fn source_type(raw: &str) -> Result<EpgSourceType> {
    match raw {
        "dummy" => Ok(EpgSourceType::Dummy),
        "xmltv" => Ok(EpgSourceType::Xmltv),
        other => Err(crate::Error::invalid(format!(
            "unknown EPG source type `{other}`; this build understands \
             `xmltv` and `dummy`"
        ))),
    }
}

fn source_type_str(value: EpgSourceType) -> &'static str {
    match value {
        EpgSourceType::Dummy => "dummy",
        EpgSourceType::Xmltv => "xmltv",
    }
}

fn map_source(row: &SqliteRow) -> Result<EpgSource> {
    Ok(EpgSource {
        id: row.get("id"),
        name: row.get("name"),
        source_type: source_type(row.get("source_type"))?,
        url: row.get("url"),
        file_path: row.get("file_path"),
        username: row.get("username"),
        password: row.get("password"),
        is_active: bool_column(row, "is_active"),
        priority: row.get::<i64, _>("priority").max(0) as u32,
        refresh_interval_hours: row.get::<i64, _>("refresh_interval_hours").max(0) as u32,
        custom_properties: json_column(row, "custom_properties"),
    })
}

fn map_data(row: &SqliteRow) -> EpgData {
    EpgData {
        id: row.get("id"),
        epg_source_id: row.get("epg_source_id"),
        tvg_id: row.get("tvg_id"),
        name: row.get("name"),
        icon_url: row.get("icon_url"),
    }
}

fn map_program(row: &SqliteRow) -> Program {
    Program {
        id: row.get("id"),
        epg_data_id: row.get("epg_data_id"),
        tvg_id: row.get("tvg_id"),
        start_time: super::datetime_column(row, "start_time"),
        end_time: super::datetime_column(row, "end_time"),
        title: row.get("title"),
        sub_title: row.get("sub_title"),
        description: row.get("description"),
        custom_properties: json_column(row, "custom_properties"),
    }
}

pub async fn list_sources(pool: &SqlitePool) -> Result<Vec<EpgSource>> {
    let rows = sqlx::query("SELECT * FROM epg_source ORDER BY priority, id")
        .fetch_all(pool)
        .await?;
    rows.iter().map(map_source).collect()
}

pub async fn get_source(pool: &SqlitePool, id: Id) -> Result<Option<EpgSource>> {
    let row = sqlx::query("SELECT * FROM epg_source WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.as_ref().map(map_source).transpose()
}

pub async fn create_source(pool: &SqlitePool, source: &EpgSource) -> Result<EpgSource> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO epg_source (name, source_type, url, file_path, username, password,
                                 is_active, priority, refresh_interval_hours, custom_properties)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&source.name)
    .bind(source_type_str(source.source_type))
    .bind(&source.url)
    .bind(&source.file_path)
    .bind(&source.username)
    .bind(&source.password)
    .bind(source.is_active as i64)
    .bind(i64::from(source.priority))
    .bind(i64::from(source.refresh_interval_hours))
    .bind(source.custom_properties.to_string())
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_source(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_source(pool: &SqlitePool, source: &EpgSource) -> Result<EpgSource> {
    let changed = sqlx::query(&format!(
        "UPDATE epg_source SET name = ?, source_type = ?, url = ?, file_path = ?, username = ?,
                               password = ?, is_active = ?, priority = ?,
                               refresh_interval_hours = ?, custom_properties = ?,
                               updated_at = {NOW}
         WHERE id = ?"
    ))
    .bind(&source.name)
    .bind(source_type_str(source.source_type))
    .bind(&source.url)
    .bind(&source.file_path)
    .bind(&source.username)
    .bind(&source.password)
    .bind(source.is_active as i64)
    .bind(i64::from(source.priority))
    .bind(i64::from(source.refresh_interval_hours))
    .bind(source.custom_properties.to_string())
    .bind(source.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_source(pool, source.id).await?.ok_or(Error::NotFound)
}

pub async fn delete_source(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM epg_source WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// How many guide channels a `list_data` call would have to choose from.
///
/// Its own query rather than a second `SELECT *`: the point is to report a
/// collection size the capped page cannot, and counting the page would be the
/// bug it exists to fix.
pub async fn count_data(
    pool: &SqlitePool,
    source_id: Option<Id>,
    search: Option<&str>,
) -> Result<i64> {
    let search = search.unwrap_or_default();
    let pattern = like_pattern(search);

    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM epg_data
         WHERE (?1 IS NULL OR epg_source_id = ?1)
           AND (?2 = '' OR name LIKE ?3 ESCAPE '\\' OR tvg_id LIKE ?3 ESCAPE '\\')",
    )
    .bind(source_id)
    .bind(search)
    .bind(&pattern)
    .fetch_one(pool)
    .await?)
}

/// Guide channels, optionally filtered by a substring of the name or tvg id.
/// This is what the "assign EPG" picker searches.
pub async fn list_data(
    pool: &SqlitePool,
    source_id: Option<Id>,
    search: Option<&str>,
    limit: i64,
) -> Result<Vec<EpgData>> {
    let search = search.unwrap_or_default();
    let pattern = like_pattern(search);

    let rows = sqlx::query(
        "SELECT * FROM epg_data
         WHERE (?1 IS NULL OR epg_source_id = ?1)
           AND (?2 = '' OR name LIKE ?3 ESCAPE '\\' OR tvg_id LIKE ?3 ESCAPE '\\')
         ORDER BY name COLLATE NOCASE
         LIMIT ?4",
    )
    .bind(source_id)
    .bind(search)
    .bind(&pattern)
    .bind(limit.clamp(1, 10_000))
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(map_data).collect())
}

/// The names of a known set of guide channels, and nothing else.
///
/// What a channel row shows in its EPG column is the name of the guide channel
/// it maps to, and a page needs a few dozen of those; reading the whole
/// `epg_data` table to answer that is a whole-table scan per request.
///
/// Interpolated rather than bound, as `programs_for` is: SQLite has no array
/// parameter, and binding a placeholder each would put a full lineup against
/// the 999-variable ceiling. These are `i64` that came from the database and
/// cannot carry SQL.
pub async fn names_for(pool: &SqlitePool, ids: &[Id]) -> Result<HashMap<Id, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut wanted: Vec<Id> = ids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let list = wanted
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let rows: Vec<(Id, String)> = sqlx::query_as(&format!(
        "SELECT id, name FROM epg_data WHERE id IN ({list})"
    ))
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().collect())
}

/// The whole guide rows for a set of ids, for callers that need more than the
/// name.
///
/// `list_data` is clamped to ten thousand rows ordered by name, so a lookup
/// through it silently misses any id past the clamp.
pub async fn data_for(pool: &SqlitePool, ids: &[Id]) -> Result<HashMap<Id, EpgData>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut wanted: Vec<Id> = ids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let list = wanted
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let rows = sqlx::query(&format!("SELECT * FROM epg_data WHERE id IN ({list})"))
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(map_data)
        .map(|data| (data.id, data))
        .collect())
}

pub async fn get_data(pool: &SqlitePool, id: Id) -> Result<Option<EpgData>> {
    let row = sqlx::query("SELECT * FROM epg_data WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_data))
}

/// Insert or refresh guide channels in batches, keyed by `(source, tvg_id)`.
pub async fn upsert_data(pool: &SqlitePool, rows: &[EpgData]) -> Result<u64> {
    let mut written = 0;

    for chunk in rows.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for row in chunk {
            written += sqlx::query(
                "INSERT INTO epg_data (epg_source_id, tvg_id, name, icon_url)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT (epg_source_id, tvg_id) DO UPDATE SET
                     name = excluded.name,
                     icon_url = excluded.icon_url",
            )
            .bind(row.epg_source_id)
            .bind(&row.tvg_id)
            .bind(&row.name)
            .bind(&row.icon_url)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(written)
}

/// Programmes for one guide channel within a window — the shape `/output/epg`
/// and the TV Guide page both walk.
pub async fn programs(
    pool: &SqlitePool,
    epg_data_id: Id,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Program>> {
    let rows = sqlx::query(
        "SELECT * FROM program
         WHERE epg_data_id = ? AND end_time > ? AND start_time < ?
         ORDER BY start_time",
    )
    .bind(epg_data_id)
    .bind(sql_timestamp(from))
    .bind(sql_timestamp(to))
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(map_program).collect())
}

/// A guide match that scored in the ambiguous band, waiting for a decision.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchSuggestion {
    pub channel_id: Id,
    pub epg_data_id: Id,
    pub score: f64,
}

/// Record the best ambiguous candidate for a channel.
///
/// Replaces any previous suggestion: a later refresh has newer guide data, and
/// two competing suggestions is a worse question than one.
pub async fn suggest_match(pool: &SqlitePool, suggestion: &MatchSuggestion) -> Result<()> {
    sqlx::query(
        "INSERT INTO epg_match_suggestion (channel_id, epg_data_id, score)
         VALUES (?, ?, ?)
         ON CONFLICT (channel_id) DO UPDATE SET
             epg_data_id = excluded.epg_data_id,
             score = excluded.score",
    )
    .bind(suggestion.channel_id)
    .bind(suggestion.epg_data_id)
    .bind(suggestion.score)
    .execute(pool)
    .await
    .map_err(translate)?;
    Ok(())
}

pub async fn suggestions(pool: &SqlitePool) -> Result<Vec<MatchSuggestion>> {
    let rows = sqlx::query("SELECT * FROM epg_match_suggestion ORDER BY channel_id")
        .fetch_all(pool)
        .await?;

    Ok(rows
        .iter()
        .map(|row| MatchSuggestion {
            channel_id: row.get("channel_id"),
            epg_data_id: row.get("epg_data_id"),
            score: row.get("score"),
        })
        .collect())
}

pub async fn get_suggestion(pool: &SqlitePool, channel_id: Id) -> Result<Option<MatchSuggestion>> {
    let row = sqlx::query("SELECT * FROM epg_match_suggestion WHERE channel_id = ?")
        .bind(channel_id)
        .fetch_optional(pool)
        .await?;

    Ok(row.map(|row| MatchSuggestion {
        channel_id: row.get("channel_id"),
        epg_data_id: row.get("epg_data_id"),
        score: row.get("score"),
    }))
}

pub async fn clear_suggestion(pool: &SqlitePool, channel_id: Id) -> Result<()> {
    sqlx::query("DELETE FROM epg_match_suggestion WHERE channel_id = ?")
        .bind(channel_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// What is on one channel right now, and whether there was anything to look in.
///
/// The three states are deliberately distinct. A channel with no guide is the
/// normal case, so a caller that cannot tell "this channel has no listings"
/// from "the answer has not arrived yet" shows a spinner that never resolves.
#[derive(Debug, Clone)]
pub struct ChannelNow {
    pub channel_uuid: String,
    pub channel_name: String,
    /// `None` when the channel maps to no guide data at all.
    pub epg_data_id: Option<Id>,
    /// The guide data is generated per request rather than stored, so an empty
    /// `program` here means "ask the generator", not "nothing is on".
    pub dummy: bool,
    pub program: Option<Program>,
}

/// Resolve the current programme for a handful of channels, by UUID.
///
/// One query for the whole set rather than one per channel: this runs on the
/// stats tick, every two seconds, for however many sessions are live.
///
/// The window is half-open — `start <= now < end`. Listings are contiguous and
/// start-time ordered, so testing `now <= end` as well returns the programme
/// that has just finished alongside the one that just started, and the first
/// row wins.
pub async fn now_playing(
    pool: &SqlitePool,
    channel_uuids: &[String],
    now: DateTime<Utc>,
) -> Result<Vec<ChannelNow>> {
    if channel_uuids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = vec!["?"; channel_uuids.len()].join(",");
    let sql = format!(
        "SELECT c.uuid          AS channel_uuid,
                c.name          AS channel_name,
                c.epg_data_id   AS epg_data_id,
                s.source_type   AS source_type,
                p.id            AS program_id,
                p.epg_data_id   AS program_epg_data_id,
                p.tvg_id        AS tvg_id,
                p.start_time    AS start_time,
                p.end_time      AS end_time,
                p.title         AS title,
                p.sub_title     AS sub_title,
                p.description   AS description,
                p.custom_properties AS custom_properties
           FROM effective_channel c
           LEFT JOIN epg_data s_data ON s_data.id = c.epg_data_id
           LEFT JOIN epg_source s ON s.id = s_data.epg_source_id
           LEFT JOIN program p ON p.epg_data_id = c.epg_data_id
                -- The lower bound is what keeps this off the whole programme
                -- history: without it every tick scans everything ever stored
                -- for the channel, and this runs every two seconds per session
                -- per connected admin. Nothing legitimately runs longer than a
                -- day, so anything starting before then cannot still be on.
                AND p.start_time > ? AND p.start_time <= ? AND p.end_time > ?
          WHERE c.uuid IN ({placeholders})
          ORDER BY c.uuid, p.start_time"
    );
    // Bound twice rather than written as `?1` twice: this statement also
    // carries an `IN` list of bare `?`, and mixing the numbered and unnumbered
    // forms in one statement is exactly the kind of thing that silently binds
    // the wrong value into a comparison.
    let mut query = sqlx::query(&sql)
        .bind(sql_timestamp(now - chrono::Duration::days(1)))
        .bind(sql_timestamp(now))
        .bind(sql_timestamp(now));
    for uuid in channel_uuids {
        query = query.bind(uuid);
    }

    let rows = query.fetch_all(pool).await?;

    // A provider whose listings overlap yields two rows for one instant. The
    // earliest start is the one a player would be showing, and `ORDER BY`
    // above put it first.
    let mut out: Vec<ChannelNow> = Vec::with_capacity(channel_uuids.len());
    for row in &rows {
        let channel_uuid: String = row.get("channel_uuid");
        if out.iter().any(|seen| seen.channel_uuid == channel_uuid) {
            continue;
        }

        let program = row
            .try_get::<Option<Id>, _>("program_id")
            .ok()
            .flatten()
            .map(|id| Program {
                id,
                epg_data_id: row.get("program_epg_data_id"),
                tvg_id: row.get("tvg_id"),
                start_time: datetime_column(row, "start_time"),
                end_time: datetime_column(row, "end_time"),
                title: row.get("title"),
                sub_title: row.get("sub_title"),
                description: row.get("description"),
                custom_properties: json_column(row, "custom_properties"),
            });

        out.push(ChannelNow {
            channel_uuid,
            channel_name: row.get("channel_name"),
            epg_data_id: row.try_get("epg_data_id").ok().flatten(),
            dummy: row
                .try_get::<Option<String>, _>("source_type")
                .ok()
                .flatten()
                .as_deref()
                == Some("dummy"),
            program,
        });
    }

    Ok(out)
}

/// Programmes for many guide channels at once, within a window.
///
/// One query rather than one per channel: the guide grid asks for every row on
/// screen, and 49 round trips to render one screen is the shape this exists to
/// avoid. The window test is `end > from AND start < to`, so a programme that
/// began before the window still appears — a three-hour film already running is
/// the most visible thing a grid can get wrong.
pub async fn programs_for(
    pool: &SqlitePool,
    epg_data_ids: &[Id],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Program>> {
    if epg_data_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Interpolated because SQLite has no array parameter; these are `i64` that
    // came from the database and cannot carry SQL.
    let ids = epg_data_ids
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let rows = sqlx::query(&format!(
        "SELECT * FROM program
         WHERE epg_data_id IN ({ids}) AND end_time > ? AND start_time < ?
         ORDER BY epg_data_id, start_time"
    ))
    .bind(sql_timestamp(from))
    .bind(sql_timestamp(to))
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(map_program).collect())
}

pub async fn insert_programs(pool: &SqlitePool, rows: &[Program]) -> Result<u64> {
    let mut written = 0;

    for chunk in rows.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for row in chunk {
            written += sqlx::query(
                "INSERT INTO program (epg_data_id, tvg_id, start_time, end_time, title,
                                      sub_title, description, custom_properties)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(row.epg_data_id)
            .bind(&row.tvg_id)
            .bind(sql_timestamp(row.start_time))
            .bind(sql_timestamp(row.end_time))
            .bind(&row.title)
            .bind(&row.sub_title)
            .bind(&row.description)
            .bind(row.custom_properties.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(written)
}

/// Drop a guide channel's programmes ahead of reimporting them.
pub async fn clear_programs(pool: &SqlitePool, epg_data_id: Id) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM program WHERE epg_data_id = ?")
        .bind(epg_data_id)
        .execute(pool)
        .await?
        .rows_affected())
}

/// Stage programmes for a later swap. `program_incoming`'s comment in the
/// schema says why they do not go straight into `program`.
pub async fn stage_programs(pool: &SqlitePool, source_id: Id, rows: &[Program]) -> Result<u64> {
    let mut written = 0;

    for chunk in rows.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for row in chunk {
            written += sqlx::query(
                "INSERT INTO program_incoming (epg_source_id, epg_data_id, tvg_id, start_time,
                                               end_time, title, sub_title, description,
                                               custom_properties)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(source_id)
            .bind(row.epg_data_id)
            .bind(&row.tvg_id)
            .bind(sql_timestamp(row.start_time))
            .bind(sql_timestamp(row.end_time))
            .bind(&row.title)
            .bind(&row.sub_title)
            .bind(&row.description)
            .bind(row.custom_properties.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
    }
    Ok(written)
}

/// Throw away anything a previous refresh of this source left staged.
pub async fn discard_staged(pool: &SqlitePool, source_id: Id) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM program_incoming WHERE epg_source_id = ?")
            .bind(source_id)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// Which guide channels the staged rows cover.
pub async fn staged_data_ids(pool: &SqlitePool, source_id: Id) -> Result<Vec<Id>> {
    let rows: Vec<(Id,)> = sqlx::query_as(
        "SELECT DISTINCT epg_data_id FROM program_incoming WHERE epg_source_id = ?
         ORDER BY epg_data_id",
    )
    .bind(source_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Replace one guide channel's programmes with the staged ones, atomically.
///
/// One transaction per channel rather than one for the whole feed: a reader
/// must never see a channel mid-swap, and the WAL cannot checkpoint while a
/// transaction that spans the entire import is open.
pub async fn promote_staged(pool: &SqlitePool, source_id: Id, epg_data_id: Id) -> Result<u64> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM program WHERE epg_data_id = ?")
        .bind(epg_data_id)
        .execute(&mut *tx)
        .await?;

    let promoted = sqlx::query(
        "INSERT INTO program (epg_data_id, tvg_id, start_time, end_time, title, sub_title,
                              description, custom_properties)
         SELECT epg_data_id, tvg_id, start_time, end_time, title, sub_title,
                description, custom_properties
           FROM program_incoming
          WHERE epg_source_id = ? AND epg_data_id = ?",
    )
    .bind(source_id)
    .bind(epg_data_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    sqlx::query("DELETE FROM program_incoming WHERE epg_source_id = ? AND epg_data_id = ?")
        .bind(source_id)
        .bind(epg_data_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(promoted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn source() -> EpgSource {
        EpgSource {
            id: 0,
            name: "Provider EPG".into(),
            source_type: EpgSourceType::Xmltv,
            url: Some("https://guide.example/epg.xml".into()),
            file_path: None,
            username: None,
            password: None,
            is_active: true,
            priority: 0,
            refresh_interval_hours: 24,
            custom_properties: serde_json::json!({"auto_apply_epg_logos": true}),
        }
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 11, hour, 0, 0).unwrap()
    }

    fn program_row(
        epg_data_id: Id,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
        title: &str,
    ) -> Program {
        Program {
            id: 0,
            epg_data_id,
            tvg_id: None,
            start_time,
            end_time,
            title: title.to_owned(),
            sub_title: None,
            description: None,
            custom_properties: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn sources_round_trip() {
        let pool = pool().await;
        let created = create_source(&pool, &source()).await.unwrap();
        assert_eq!(created.source_type, EpgSourceType::Xmltv);
        assert_eq!(created.custom_properties["auto_apply_epg_logos"], true);

        let mut dummy = created.clone();
        dummy.source_type = EpgSourceType::Dummy;
        assert_eq!(
            save_source(&pool, &dummy).await.unwrap().source_type,
            EpgSourceType::Dummy
        );
    }

    #[tokio::test]
    async fn guide_channels_upsert_on_tvg_id_rather_than_duplicating() {
        let pool = pool().await;
        let source = create_source(&pool, &source()).await.unwrap();

        let mut rows = vec![EpgData {
            id: 0,
            epg_source_id: Some(source.id),
            tvg_id: Some("vrix.us".into()),
            name: "VRIX".into(),
            icon_url: None,
        }];
        upsert_data(&pool, &rows).await.unwrap();

        rows[0].name = "VRIX HD".into();
        rows[0].icon_url = Some("https://art.example/vrix.png".into());
        upsert_data(&pool, &rows).await.unwrap();

        let found = list_data(&pool, Some(source.id), None, 100).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "VRIX HD");
        assert_eq!(
            found[0].icon_url.as_deref(),
            Some("https://art.example/vrix.png")
        );
    }

    #[tokio::test]
    async fn guide_channel_search_matches_name_or_tvg_id() {
        let pool = pool().await;
        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("vrix.us".into()),
                    name: "VRIX".into(),
                    icon_url: None,
                },
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("zor1.ca".into()),
                    name: "ZOR 1".into(),
                    icon_url: None,
                },
            ],
        )
        .await
        .unwrap();

        assert_eq!(
            list_data(&pool, None, Some("zor"), 10).await.unwrap().len(),
            1
        );
        assert_eq!(
            list_data(&pool, None, Some(".us"), 10).await.unwrap().len(),
            1
        );
        assert_eq!(list_data(&pool, None, None, 10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn names_are_fetched_for_the_ids_asked_for_and_no_others() {
        let pool = pool().await;
        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("vrix.us".into()),
                    name: "VRIX".into(),
                    icon_url: None,
                },
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("zor1.ca".into()),
                    name: "ZOR 1".into(),
                    icon_url: None,
                },
            ],
        )
        .await
        .unwrap();
        let ids: Vec<Id> = list_data(&pool, None, None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|data| data.id)
            .collect();

        // The whole point is that the second row is never read.
        let found = names_for(&pool, &[ids[0]]).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found.get(&ids[0]).map(String::as_str), Some("VRIX"));

        // A channel whose guide row has been deleted since the page was built
        // is simply absent, which is what the caller serializes as null.
        let mixed = names_for(&pool, &[ids[0], ids[1], 9_999]).await.unwrap();
        assert_eq!(mixed.len(), 2);
        assert!(!mixed.contains_key(&9_999));

        assert!(names_for(&pool, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn programmes_are_batched_and_queried_by_window() {
        let pool = pool().await;
        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[EpgData {
                id: 0,
                epg_source_id: Some(source.id),
                tvg_id: Some("vrix.us".into()),
                name: "VRIX".into(),
                icon_url: None,
            }],
        )
        .await
        .unwrap();
        let data_id = list_data(&pool, None, None, 1).await.unwrap()[0].id;

        let rows: Vec<Program> = (0..1200)
            .map(|i| Program {
                id: 0,
                epg_data_id: data_id,
                tvg_id: Some("vrix.us".into()),
                start_time: at(0) + chrono::Duration::minutes(i * 30),
                end_time: at(0) + chrono::Duration::minutes(i * 30 + 30),
                title: format!("Show {i}"),
                sub_title: None,
                description: None,
                custom_properties: serde_json::json!({}),
            })
            .collect();

        assert_eq!(insert_programs(&pool, &rows).await.unwrap(), 1200);

        // A programme straddling the window boundary must still be returned,
        // or the first entry in a guide is always missing.
        let window = programs(&pool, data_id, at(1), at(2)).await.unwrap();
        assert_eq!(window.len(), 2);
        assert_eq!(window[0].title, "Show 2");

        assert_eq!(clear_programs(&pool, data_id).await.unwrap(), 1200);
    }

    #[tokio::test]
    async fn a_windowed_batch_includes_programmes_that_straddle_the_edges() {
        let pool = pool().await;
        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("a".into()),
                    name: "A".into(),
                    icon_url: None,
                },
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("b".into()),
                    name: "B".into(),
                    icon_url: None,
                },
            ],
        )
        .await
        .unwrap();
        let ids: Vec<Id> = list_data(&pool, None, None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();

        let program = |epg_data_id, title: &str, start, end| Program {
            id: 0,
            epg_data_id,
            tvg_id: None,
            start_time: at(start),
            end_time: at(end),
            title: title.into(),
            sub_title: None,
            description: None,
            custom_properties: serde_json::json!({}),
        };

        insert_programs(
            &pool,
            &[
                // Began three hours before the window and is still running.
                program(ids[0], "long film", 9, 15),
                program(ids[0], "inside", 12, 13),
                // Starts before the window ends and runs past it.
                program(ids[0], "overruns", 13, 20),
                program(ids[0], "after", 20, 21),
                program(ids[1], "other channel", 12, 13),
            ],
        )
        .await
        .unwrap();

        let found = programs_for(&pool, &ids, at(12), at(14)).await.unwrap();
        let titles: Vec<&str> = found.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["long film", "inside", "overruns", "other channel"],
            "a programme straddling an edge was dropped"
        );

        // A channel with nothing in the window simply contributes nothing; the
        // caller is what keeps its row.
        let narrow = programs_for(&pool, &[ids[1]], at(20), at(21))
            .await
            .unwrap();
        assert!(narrow.is_empty());
        assert!(
            programs_for(&pool, &[], at(0), at(23))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_suggestion_is_one_per_channel_and_dies_with_its_row() {
        let pool = pool().await;
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();

        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("a".into()),
                    name: "A".into(),
                    icon_url: None,
                },
                EpgData {
                    id: 0,
                    epg_source_id: Some(source.id),
                    tvg_id: Some("b".into()),
                    name: "B".into(),
                    icon_url: None,
                },
            ],
        )
        .await
        .unwrap();
        let ids: Vec<Id> = list_data(&pool, None, None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();

        sqlx::query("INSERT INTO channel (id, uuid, name) VALUES (1, 'u1', 'Ambiguous')")
            .execute(&pool)
            .await
            .unwrap();

        suggest_match(
            &pool,
            &MatchSuggestion {
                channel_id: 1,
                epg_data_id: ids[0],
                score: 62.5,
            },
        )
        .await
        .unwrap();

        // A later refresh has newer guide data, so it replaces rather than
        // accumulating a list of near-misses nobody can answer.
        suggest_match(
            &pool,
            &MatchSuggestion {
                channel_id: 1,
                epg_data_id: ids[1],
                score: 71.0,
            },
        )
        .await
        .unwrap();

        let all = suggestions(&pool).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].epg_data_id, ids[1]);
        assert_eq!(
            get_suggestion(&pool, 1).await.unwrap(),
            Some(all[0].clone())
        );

        clear_suggestion(&pool, 1).await.unwrap();
        assert!(get_suggestion(&pool, 1).await.unwrap().is_none());

        // And a suggestion never outlives the guide row it points at.
        suggest_match(
            &pool,
            &MatchSuggestion {
                channel_id: 1,
                epg_data_id: ids[1],
                score: 71.0,
            },
        )
        .await
        .unwrap();
        delete_source(&pool, source.id).await.unwrap();
        assert!(suggestions(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleting_a_source_takes_its_guide_data_and_programmes() {
        let pool = pool().await;
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .unwrap();

        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[EpgData {
                id: 0,
                epg_source_id: Some(source.id),
                tvg_id: Some("vrix.us".into()),
                name: "VRIX".into(),
                icon_url: None,
            }],
        )
        .await
        .unwrap();
        let data_id = list_data(&pool, None, None, 1).await.unwrap()[0].id;
        insert_programs(
            &pool,
            &[Program {
                id: 0,
                epg_data_id: data_id,
                tvg_id: None,
                start_time: at(0),
                end_time: at(1),
                title: "Show".into(),
                sub_title: None,
                description: None,
                custom_properties: serde_json::json!({}),
            }],
        )
        .await
        .unwrap();

        delete_source(&pool, source.id).await.unwrap();
        assert!(get_data(&pool, data_id).await.unwrap().is_none());
        assert!(
            programs(&pool, data_id, at(0), at(2))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn now_playing_uses_a_half_open_window() {
        let pool = pool().await;
        let source = create_source(&pool, &source()).await.unwrap();
        upsert_data(
            &pool,
            &[EpgData {
                id: 0,
                epg_source_id: Some(source.id),
                tvg_id: Some("vrix.us".into()),
                name: "VRIX".into(),
                icon_url: None,
            }],
        )
        .await
        .unwrap();
        let data = list_data(&pool, Some(source.id), None, 10).await.unwrap();
        let data_id = data[0].id;

        // Contiguous, as a real feed is: one ends exactly where the next starts.
        insert_programs(
            &pool,
            &[
                program_row(data_id, at(12), at(13), "Finished"),
                program_row(data_id, at(13), at(14), "Playing"),
            ],
        )
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO channel (id, uuid, name, epg_data_id)
             VALUES (1, 'aaaaaaaa-0000-4000-8000-000000000001', 'Sports', ?)",
        )
        .bind(data_id)
        .execute(&pool)
        .await
        .unwrap();

        // Exactly on the boundary, which is the case that catches it: an
        // inclusive `now <= end` matches the programme that has just finished
        // as well, and start-time ordering hands that one back first.
        let rows = now_playing(
            &pool,
            &["aaaaaaaa-0000-4000-8000-000000000001".to_owned()],
            at(13),
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].program.as_ref().map(|p| p.title.as_str()),
            Some("Playing")
        );

        // And one second before it, the other way round.
        let rows = now_playing(
            &pool,
            &["aaaaaaaa-0000-4000-8000-000000000001".to_owned()],
            at(13) - chrono::Duration::seconds(1),
        )
        .await
        .unwrap();
        assert_eq!(
            rows[0].program.as_ref().map(|p| p.title.as_str()),
            Some("Finished")
        );
    }

    #[tokio::test]
    async fn a_channel_with_no_guide_is_not_a_channel_with_no_answer() {
        let pool = pool().await;
        sqlx::query(
            "INSERT INTO channel (id, uuid, name) VALUES
                 (1, 'aaaaaaaa-0000-4000-8000-000000000001', 'Unmapped')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let rows = now_playing(
            &pool,
            &[
                "aaaaaaaa-0000-4000-8000-000000000001".to_owned(),
                // A session whose channel has since been deleted.
                "aaaaaaaa-0000-4000-8000-0000000000ff".to_owned(),
            ],
            Utc::now(),
        )
        .await
        .unwrap();

        // The unmapped channel answers; the missing one is simply absent, and
        // the caller can tell those apart.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].channel_name, "Unmapped");
        assert_eq!(rows[0].epg_data_id, None);
        assert!(rows[0].program.is_none());
        assert!(!rows[0].dummy);
    }

    #[tokio::test]
    async fn a_dummy_source_is_flagged_so_the_caller_can_generate() {
        let pool = pool().await;
        let mut draft = source();
        draft.source_type = EpgSourceType::Dummy;
        let source = create_source(&pool, &draft).await.unwrap();
        upsert_data(
            &pool,
            &[EpgData {
                id: 0,
                epg_source_id: Some(source.id),
                tvg_id: Some("dummy.1".into()),
                name: "Dummy".into(),
                icon_url: None,
            }],
        )
        .await
        .unwrap();
        let data_id = list_data(&pool, Some(source.id), None, 10).await.unwrap()[0].id;

        sqlx::query(
            "INSERT INTO channel (id, uuid, name, epg_data_id)
             VALUES (1, 'aaaaaaaa-0000-4000-8000-000000000001', 'Dummy Channel', ?)",
        )
        .bind(data_id)
        .execute(&pool)
        .await
        .unwrap();

        let rows = now_playing(
            &pool,
            &["aaaaaaaa-0000-4000-8000-000000000001".to_owned()],
            Utc::now(),
        )
        .await
        .unwrap();

        // Mapped, generated, and no stored rows — which must not read as "this
        // channel has nothing on".
        assert!(rows[0].dummy);
        assert!(rows[0].epg_data_id.is_some());
        assert!(rows[0].program.is_none());
    }

    #[tokio::test]
    async fn an_unknown_source_type_is_refused_rather_than_read_as_xmltv() {
        let pool = pool().await;
        create_source(&pool, &source()).await.unwrap();

        // Schedules Direct is the third source type that will want adding.
        // A build that predates it must say it cannot read the row, not fetch
        // it as if it were an XMLTV file.
        sqlx::query("UPDATE epg_source SET source_type = 'schedules_direct' WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();

        let error = get_source(&pool, 1).await.expect_err("read as xmltv");
        assert!(error.to_string().contains("schedules_direct"), "{error}");
    }
}
