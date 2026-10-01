//! Channels, their groups, their per-user overrides, and their failover order.
//!
//! Everything an output touches goes through the `effective_channel` view.
//! Reading `channel` directly is only correct in the editor, where the base
//! value and the override must be shown separately.

use std::cmp::Ordering;
use std::iter::Peekable;
use std::str::Chars;

use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::auth::level_from_i64;
use crate::domain::{Channel, ChannelGroup, ChannelOverride, EffectiveChannel, Id};
use crate::sync::channels::align_up;
use crate::{Error, Result};

use super::{
    NOW, Page, Paging, bool_column, datetime_column, like_pattern, order_clause, translate,
    uuid_column,
};

/// `channel_number` is nullable, and SQLite sorts NULLs *first* on ASC where
/// PostgreSQL sorts them last. Without the explicit `IS NULL` tie-break an
/// unnumbered channel leads the HDHR lineup and the M3U, which is the first
/// thing Plex shows.
const ORDERING: &[(&str, &str)] = &[
    (
        "channel_number",
        "channel_number IS NULL {dir}, channel_number {dir}",
    ),
    ("name", "name COLLATE NOCASE"),
    ("group_name", "group_name COLLATE NOCASE"),
    ("id", "id"),
];

/// The default channel ordering, and the one Plex expects in a lineup: by
/// number, with unnumbered channels last.
const DEFAULT_ORDER: &str = "channel_number IS NULL, channel_number ASC, name COLLATE NOCASE ASC";

fn map_group(row: &SqliteRow) -> ChannelGroup {
    ChannelGroup {
        id: row.get("id"),
        name: row.get("name"),
        number_start: row.get("number_start"),
        number_end: row.get("number_end"),
    }
}

fn map_channel(row: &SqliteRow) -> Channel {
    Channel {
        id: row.get("id"),
        uuid: uuid_column(row, "uuid"),
        channel_number: row.get("channel_number"),
        name: row.get("name"),
        logo_id: row.get("logo_id"),
        channel_group_id: row.get("channel_group_id"),
        tvg_id: row.get("tvg_id"),
        tvc_guide_stationid: row.get("tvc_guide_stationid"),
        epg_data_id: row.get("epg_data_id"),
        stream_profile_id: row.get("stream_profile_id"),
        user_level: level_from_i64(row.get("user_level")),
        is_adult: bool_column(row, "is_adult"),
        hidden_from_output: bool_column(row, "hidden_from_output"),
        auto_created: bool_column(row, "auto_created"),
        is_catchup: bool_column(row, "is_catchup"),
        catchup_days: row.get::<i64, _>("catchup_days").max(0) as u32,
        created_at: datetime_column(row, "created_at"),
        updated_at: datetime_column(row, "updated_at"),
    }
}

fn map_effective(row: &SqliteRow) -> EffectiveChannel {
    EffectiveChannel {
        id: row.get("id"),
        uuid: uuid_column(row, "uuid"),
        channel_number: row.get("channel_number"),
        name: row.get("name"),
        channel_group_id: row.get("channel_group_id"),
        group_name: row.get("group_name"),
        created_at: datetime_column(row, "created_at"),
        logo_url: row.get("logo_url"),
        tvg_id: row.get("tvg_id"),
        tvc_guide_stationid: row.get("tvc_guide_stationid"),
        epg_data_id: row.get("epg_data_id"),
        stream_profile_id: row.get("stream_profile_id"),
        user_level: level_from_i64(row.get("user_level")),
        is_adult: bool_column(row, "is_adult"),
        hidden_from_output: bool_column(row, "hidden_from_output"),
        is_catchup: bool_column(row, "is_catchup"),
        catchup_days: row.get::<i64, _>("catchup_days").max(0) as u32,
    }
}

fn map_override(row: &SqliteRow) -> ChannelOverride {
    ChannelOverride {
        channel_id: row.get("channel_id"),
        name: row.get("name"),
        channel_number: row.get("channel_number"),
        channel_group_id: row.get("channel_group_id"),
        logo_id: row.get("logo_id"),
        tvg_id: row.get("tvg_id"),
        tvc_guide_stationid: row.get("tvc_guide_stationid"),
        epg_data_id: row.get("epg_data_id"),
        stream_profile_id: row.get("stream_profile_id"),
    }
}

// ------------------------------------------------------------------ groups

pub async fn list_groups(pool: &SqlitePool) -> Result<Vec<ChannelGroup>> {
    let rows = sqlx::query("SELECT * FROM channel_group ORDER BY name COLLATE NOCASE")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(map_group).collect())
}

pub async fn get_group(pool: &SqlitePool, id: Id) -> Result<Option<ChannelGroup>> {
    let row = sqlx::query("SELECT * FROM channel_group WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_group))
}

pub async fn create_group(
    pool: &SqlitePool,
    name: &str,
    range: (Option<f64>, Option<f64>),
) -> Result<ChannelGroup> {
    let id: Id = sqlx::query_scalar(
        "INSERT INTO channel_group (name, number_start, number_end) VALUES (?, ?, ?) RETURNING id",
    )
    .bind(name)
    .bind(range.0)
    .bind(range.1)
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get_group(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save_group(pool: &SqlitePool, group: &ChannelGroup) -> Result<ChannelGroup> {
    let changed = sqlx::query(
        "UPDATE channel_group SET name = ?, number_start = ?, number_end = ? WHERE id = ?",
    )
    .bind(&group.name)
    .bind(group.number_start)
    .bind(group.number_end)
    .bind(group.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get_group(pool, group.id).await?.ok_or(Error::NotFound)
}

pub async fn delete_group(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM channel_group WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();

    if deleted == 0 {
        return Err(Error::NotFound);
    }
    Ok(())
}

/// Channels and streams per group, for the group list's badges.
pub async fn group_usage(pool: &SqlitePool) -> Result<Vec<(Id, i64, i64)>> {
    let rows = sqlx::query(
        "SELECT g.id AS id,
                (SELECT COUNT(*) FROM effective_channel c WHERE c.channel_group_id = g.id) AS channels,
                (SELECT COUNT(*) FROM stream s WHERE s.channel_group_id = g.id) AS streams
         FROM channel_group g",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| {
            (
                row.get::<Id, _>("id"),
                row.get::<i64, _>("channels"),
                row.get::<i64, _>("streams"),
            )
        })
        .collect())
}

// ---------------------------------------------------------------- channels

pub async fn get(pool: &SqlitePool, id: Id) -> Result<Option<Channel>> {
    let row = sqlx::query("SELECT * FROM channel WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_channel))
}

/// Filters applied by both the editor list and the output paths.
#[derive(Debug, Default, Clone)]
pub struct ChannelFilter<'a> {
    pub search: Option<&'a str>,
    pub group_id: Option<Id>,
    pub profile_id: Option<Id>,
    /// Outputs set this; the editor does not, because hidden channels still
    /// have to be visible somewhere to be un-hidden.
    pub visible_only: bool,
}

/// Parameter positions are fixed (`?1` search, `?2` pattern, `?3` group,
/// `?4` profile) so the count query and the page query bind identically.
fn effective_filter(filter: &ChannelFilter<'_>) -> String {
    let mut clauses = vec![
        "(?1 = '' OR name LIKE ?2 ESCAPE '\\' OR group_name LIKE ?2 ESCAPE '\\')",
        "(?3 IS NULL OR channel_group_id = ?3)",
        "(?4 IS NULL OR id IN (SELECT channel_id FROM channel_profile_membership
                               WHERE channel_profile_id = ?4 AND enabled = 1))",
    ];

    if filter.visible_only {
        clauses.push("hidden_from_output = 0");
    }

    format!("WHERE {}", clauses.join(" AND "))
}

/// Channels with overrides applied, ordered, filtered and paginated in SQL.
pub async fn list_effective(
    pool: &SqlitePool,
    filter: &ChannelFilter<'_>,
    ordering: Option<&str>,
    paging: Option<Paging>,
) -> Result<Page<EffectiveChannel>> {
    let where_clause = effective_filter(filter);
    let search = filter.search.unwrap_or_default();
    let pattern = like_pattern(search);

    let count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM effective_channel {where_clause}"
    ))
    .bind(search)
    .bind(&pattern)
    .bind(filter.group_id)
    .bind(filter.profile_id)
    .fetch_one(pool)
    .await?;

    let order = order_clause(ordering, ORDERING, DEFAULT_ORDER);
    let mut sql = format!("SELECT * FROM effective_channel {where_clause} ORDER BY {order}");
    if paging.is_some() {
        sql.push_str(" LIMIT ?5 OFFSET ?6");
    }

    let mut query = sqlx::query(&sql)
        .bind(search)
        .bind(&pattern)
        .bind(filter.group_id)
        .bind(filter.profile_id);
    if let Some(paging) = paging {
        query = query.bind(paging.limit()).bind(paging.offset());
    }

    let rows = query.fetch_all(pool).await?;
    Ok(Page {
        count,
        results: rows.iter().map(map_effective).collect(),
    })
}

pub async fn get_effective(pool: &SqlitePool, id: Id) -> Result<Option<EffectiveChannel>> {
    let row = sqlx::query("SELECT * FROM effective_channel WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_effective))
}

/// The streaming endpoint's entry point: `/proxy/ts/stream/<uuid>`.
pub async fn get_effective_by_uuid(
    pool: &SqlitePool,
    uuid: Uuid,
) -> Result<Option<EffectiveChannel>> {
    let row = sqlx::query("SELECT * FROM effective_channel WHERE uuid = ?")
        .bind(uuid.hyphenated().to_string())
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_effective))
}

pub async fn create(pool: &SqlitePool, channel: &Channel) -> Result<Channel> {
    let uuid = if channel.uuid.is_nil() {
        Uuid::new_v4()
    } else {
        channel.uuid
    };

    let id: Id = sqlx::query_scalar(
        "INSERT INTO channel (uuid, channel_number, name, logo_id, channel_group_id, tvg_id,
                              tvc_guide_stationid, epg_data_id, stream_profile_id, user_level,
                              is_adult, hidden_from_output, auto_created, is_catchup, catchup_days)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(uuid.hyphenated().to_string())
    .bind(channel.channel_number)
    .bind(&channel.name)
    .bind(channel.logo_id)
    .bind(channel.channel_group_id)
    .bind(&channel.tvg_id)
    .bind(&channel.tvc_guide_stationid)
    .bind(channel.epg_data_id)
    .bind(channel.stream_profile_id)
    .bind(crate::auth::level_value(channel.user_level))
    .bind(channel.is_adult as i64)
    .bind(channel.hidden_from_output as i64)
    .bind(channel.auto_created as i64)
    .bind(channel.is_catchup as i64)
    .bind(i64::from(channel.catchup_days))
    .fetch_one(pool)
    .await
    .map_err(translate)?;

    get(pool, id).await?.ok_or(Error::NotFound)
}

pub async fn save(pool: &SqlitePool, channel: &Channel) -> Result<Channel> {
    let changed = sqlx::query(&format!(
        "UPDATE channel SET channel_number = ?, name = ?, logo_id = ?, channel_group_id = ?,
                            tvg_id = ?, tvc_guide_stationid = ?, epg_data_id = ?,
                            stream_profile_id = ?, user_level = ?, is_adult = ?,
                            hidden_from_output = ?, auto_created = ?, is_catchup = ?,
                            catchup_days = ?, updated_at = {NOW}
         WHERE id = ?"
    ))
    .bind(channel.channel_number)
    .bind(&channel.name)
    .bind(channel.logo_id)
    .bind(channel.channel_group_id)
    .bind(&channel.tvg_id)
    .bind(&channel.tvc_guide_stationid)
    .bind(channel.epg_data_id)
    .bind(channel.stream_profile_id)
    .bind(crate::auth::level_value(channel.user_level))
    .bind(channel.is_adult as i64)
    .bind(channel.hidden_from_output as i64)
    .bind(channel.auto_created as i64)
    .bind(channel.is_catchup as i64)
    .bind(i64::from(channel.catchup_days))
    .bind(channel.id)
    .execute(pool)
    .await
    .map_err(translate)?
    .rows_affected();

    if changed == 0 {
        return Err(Error::NotFound);
    }
    get(pool, channel.id).await?.ok_or(Error::NotFound)
}

pub async fn delete(pool: &SqlitePool, id: Id) -> Result<()> {
    let deleted = sqlx::query("DELETE FROM channel WHERE id = ?")
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
    super::delete_by_id(pool, "channel", ids).await
}

// --------------------------------------------------------------- overrides

pub async fn get_override(pool: &SqlitePool, channel_id: Id) -> Result<Option<ChannelOverride>> {
    let row = sqlx::query("SELECT * FROM channel_override WHERE channel_id = ?")
        .bind(channel_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(map_override))
}

pub async fn save_override(pool: &SqlitePool, value: &ChannelOverride) -> Result<ChannelOverride> {
    sqlx::query(&format!(
        "INSERT INTO channel_override (channel_id, name, channel_number, channel_group_id,
                                       logo_id, tvg_id, tvc_guide_stationid, epg_data_id,
                                       stream_profile_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (channel_id) DO UPDATE SET
             name = excluded.name,
             channel_number = excluded.channel_number,
             channel_group_id = excluded.channel_group_id,
             logo_id = excluded.logo_id,
             tvg_id = excluded.tvg_id,
             tvc_guide_stationid = excluded.tvc_guide_stationid,
             epg_data_id = excluded.epg_data_id,
             stream_profile_id = excluded.stream_profile_id,
             updated_at = {NOW}"
    ))
    .bind(value.channel_id)
    .bind(&value.name)
    .bind(value.channel_number)
    .bind(value.channel_group_id)
    .bind(value.logo_id)
    .bind(&value.tvg_id)
    .bind(&value.tvc_guide_stationid)
    .bind(value.epg_data_id)
    .bind(value.stream_profile_id)
    .execute(pool)
    .await
    .map_err(translate)?;

    get_override(pool, value.channel_id)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn clear_override(pool: &SqlitePool, channel_id: Id) -> Result<()> {
    sqlx::query("DELETE FROM channel_override WHERE channel_id = ?")
        .bind(channel_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ----------------------------------------------------------- failover list

/// Comma-separated id list for an `IN (...)` clause.
///
/// Interpolated rather than bound because SQLite has no array parameter and
/// the values are `i64` that came from the database, so they cannot carry SQL.
fn id_list(ids: &[Id]) -> String {
    ids.iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Base rows for a page of channels, so the editor can show what the provider
/// sent alongside what the user overrode.
pub async fn get_many(pool: &SqlitePool, ids: &[Id]) -> Result<Vec<Channel>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(&format!(
        "SELECT * FROM channel WHERE id IN ({})",
        id_list(ids)
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(map_channel).collect())
}

pub async fn overrides_for(pool: &SqlitePool, ids: &[Id]) -> Result<Vec<ChannelOverride>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(&format!(
        "SELECT * FROM channel_override WHERE channel_id IN ({})",
        id_list(ids)
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(map_override).collect())
}

/// Groups that have at least one visible channel, in the order those channels
/// first appear in the lineup.
///
/// Not alphabetical and not every group: an Xtream client renders categories
/// in the order given, and a category with nothing in it is a dead end a user
/// has to back out of. Ordered by each group's earliest channel so the
/// category list reads like the lineup does.
pub async fn groups_in_lineup_order(
    pool: &SqlitePool,
    profile_id: Option<Id>,
) -> Result<Vec<ChannelGroup>> {
    let rows = sqlx::query(
        "SELECT g.*
         FROM channel_group g
         JOIN effective_channel c ON c.channel_group_id = g.id
         WHERE c.hidden_from_output = 0
           AND (?1 IS NULL OR c.id IN (SELECT channel_id FROM channel_profile_membership
                                       WHERE channel_profile_id = ?1 AND enabled = 1))
         GROUP BY g.id
         ORDER BY MIN(c.channel_number IS NULL), MIN(c.channel_number),
                  MIN(c.name COLLATE NOCASE)",
    )
    .bind(profile_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(map_group).collect())
}

/// Every number the lineup currently holds, sorted for `next_available`.
///
/// Effective numbers, because an override is what the outputs serve: a base
/// number hidden under an override is not free, and a number set only in an
/// override is not either.
pub async fn numbers_in_use(pool: &SqlitePool) -> Result<Vec<f64>> {
    let mut numbers: Vec<f64> = sqlx::query_scalar(
        "SELECT channel_number FROM effective_channel WHERE channel_number IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    numbers.sort_by(f64::total_cmp);
    Ok(numbers)
}

/// One group's renumber: each member channel and the number it will take.
#[derive(Debug, Clone, PartialEq)]
pub struct RenumberPlan {
    pub group_id: Id,
    pub assigned: Vec<(Id, f64)>,
}

/// What decides the order a group's channels are laid out in.
///
/// `Current` compacts the lineup as it stands onto the grid and re-sorts
/// nothing, which is the only one that cannot surprise anyone. The rest sort
/// the group, and a channel the key says nothing about — no guide match, no `tvg-id` — keeps
/// its place relative to the others and goes last.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenumberOrder {
    #[default]
    Current,
    Name,
    Guide,
    TvgId,
}

/// Compare with runs of digits read as numbers, so `TSN2` comes before
/// `TSN10`.
///
/// Plain string order is wrong for every naming scheme a channel list uses,
/// and it is wrong in the way people notice: one provider's `TSN10` lands
/// between `TSN1` and `TSN2`. SQLite cannot express this in `ORDER BY`, so the
/// sort happens here, over keys the caller has already lowercased.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let digits = |chars: &mut Peekable<Chars>| {
        let mut run = String::new();
        while chars.peek().is_some_and(char::is_ascii_digit) {
            run.push(chars.next().expect("peeked"));
        }
        run
    };

    let (mut left, mut right) = (a.chars().peekable(), b.chars().peekable());
    loop {
        let order = match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (one, two) = (digits(&mut left), digits(&mut right));
                // Leading zeros are decoration: `007` and `7` are one number,
                // and the longer run only wins once they are gone.
                let (one, two) = (one.trim_start_matches('0'), two.trim_start_matches('0'));
                one.len().cmp(&two.len()).then_with(|| one.cmp(two))
            }
            (Some(x), Some(y)) => {
                left.next();
                right.next();
                x.cmp(&y)
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

/// What one group member sorts on, lowercased for [`natural_cmp`].
///
/// `None` sorts last, and a blank key is no key: a channel with no guide match
/// and no `tvg-id` has nothing to say about where it belongs, and putting it
/// first would bury the channels that do.
fn sort_key(row: &SqliteRow, order: RenumberOrder) -> Option<String> {
    let value: Option<String> = match order {
        RenumberOrder::Current => None,
        RenumberOrder::Name => row.get("name"),
        RenumberOrder::Guide => row.get("guide_name"),
        RenumberOrder::TvgId => row.get("tvg_id"),
    };
    value
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
}

/// The numbers one group's channels hold, sorted, for [`crate::sync::channels::next_slot`].
pub async fn numbers_in_group(pool: &SqlitePool, group_id: Id) -> Result<Vec<f64>> {
    let mut numbers: Vec<f64> = sqlx::query_scalar(
        "SELECT channel_number FROM effective_channel
         WHERE channel_number IS NOT NULL AND channel_group_id = ?",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    numbers.sort_by(f64::total_cmp);
    Ok(numbers)
}

/// A group with the plan made for it.
#[derive(Debug, Clone)]
pub struct ChannelGroupPlan {
    pub group: ChannelGroup,
    pub plan: RenumberPlan,
}

/// Lay each group's channels out on its range, in the order `order` puts them
/// in, without writing anything.
///
/// Numbers are the step's grid from the range's start; numbers held by
/// channels outside the planned groups are stepped over rather than taken, so
/// two groups given overlapping ranges cannot end up with two channels on one
/// number. The planned groups' own current numbers do not count as taken —
/// they are the ones moving — and each group's new numbers count for the
/// groups planned after it, which is what makes one preview of several
/// groups honest. Refuses at the first group whose range has no room, before
/// anything is written.
pub async fn plan_renumber(
    pool: &SqlitePool,
    groups: &[ChannelGroup],
    step: f64,
    order: RenumberOrder,
) -> Result<Vec<RenumberPlan>> {
    let step = if step.is_finite() && step >= 1.0 {
        step
    } else {
        1.0
    };
    let moving: Vec<Id> = groups.iter().map(|group| group.id).collect();

    let mut used: Vec<f64> = sqlx::query_scalar::<_, Option<f64>>(&format!(
        "SELECT channel_number FROM effective_channel
         WHERE channel_number IS NOT NULL
           AND (channel_group_id IS NULL OR channel_group_id NOT IN ({}))",
        moving
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(","),
    ))
    .fetch_all(pool)
    .await?
    .into_iter()
    .flatten()
    .collect();
    used.sort_by(f64::total_cmp);

    let mut plans = Vec::with_capacity(groups.len());
    for group in groups {
        let Some(start) = group.number_start else {
            return Err(Error::invalid(format!(
                "{} has no number range to renumber into",
                group.name
            )));
        };
        let end = group.number_end;
        // The lineup's own order, which is what `Current` means and what every
        // other order falls back to for the channels its key cannot tell
        // apart: the sort below is stable.
        let rows = sqlx::query(
            "SELECT c.id AS id, c.name AS name, c.tvg_id AS tvg_id, e.name AS guide_name
             FROM effective_channel c
             LEFT JOIN epg_data e ON e.id = c.epg_data_id
             WHERE c.channel_group_id = ?
             ORDER BY c.channel_number IS NULL, c.channel_number, c.name COLLATE NOCASE",
        )
        .bind(group.id)
        .fetch_all(pool)
        .await?;
        let mut members: Vec<(Option<String>, Id)> = rows
            .iter()
            .map(|row| (sort_key(row, order), row.get("id")))
            .collect();
        // Stable, so channels whose key is equal or missing stay in the order
        // the lineup already had them in.
        members.sort_by(|(one, _), (two, _)| match (one, two) {
            (Some(one), Some(two)) => natural_cmp(one, two),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });

        let mut assigned = Vec::with_capacity(members.len());
        let mut slot = align_up(start, step);
        for (_, id) in &members {
            while used
                .binary_search_by(|probe| probe.total_cmp(&slot))
                .is_ok()
            {
                slot += step;
            }
            if end.is_some_and(|limit| slot > limit) {
                return Err(Error::invalid(format!(
                    "the range from {start} to {} has room for fewer than {}'s {} channels at a step of {step}",
                    end.map_or_else(|| "the end".to_owned(), |end| end.to_string()),
                    group.name,
                    members.len(),
                )));
            }
            used.insert(used.partition_point(|probe| *probe < slot), slot);
            assigned.push((*id, slot));
            slot += step;
        }
        plans.push(RenumberPlan {
            group_id: group.id,
            assigned,
        });
    }
    Ok(plans)
}

/// Write a plan. The one write that moves numbers already assigned, so
/// nothing calls it as a side effect: a refresh never renumbers, and this is
/// a request an operator makes knowing Plex will need to re-scan. A number
/// override on a renumbered channel is cleared, since the view would
/// otherwise keep serving the old number after reporting the new one.
pub async fn apply_renumber(pool: &SqlitePool, plans: &[RenumberPlan]) -> Result<()> {
    let assigned: Vec<(Id, f64)> = plans
        .iter()
        .flat_map(|plan| plan.assigned.iter().copied())
        .collect();
    apply_numbers(pool, &assigned).await
}

/// The write itself: numbers, and the number overrides they replace.
pub async fn apply_numbers(pool: &SqlitePool, assigned: &[(Id, f64)]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for (id, number) in assigned {
        sqlx::query(&format!(
            "UPDATE channel SET channel_number = ?, updated_at = {NOW} WHERE id = ?"
        ))
        .bind(number)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE channel_override SET channel_number = NULL WHERE channel_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    // An override with nothing left in it only slows the view down, and would
    // report "has override" in the editor for a channel nobody customised.
    sqlx::query(
        "DELETE FROM channel_override
         WHERE channel_number IS NULL AND name IS NULL AND channel_group_id IS NULL
           AND logo_id IS NULL AND tvg_id IS NULL AND tvc_guide_stationid IS NULL
           AND epg_data_id IS NULL AND stream_profile_id IS NULL",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// One channel's group and number as the lineup sees them, override included.
async fn slot_of(pool: &SqlitePool, id: Id) -> Result<Option<(Option<Id>, Option<f64>)>> {
    let row =
        sqlx::query("SELECT channel_group_id, channel_number FROM effective_channel WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|row| (row.get("channel_group_id"), row.get("channel_number"))))
}

/// The nearest number one group holds on the far side of `pivot`.
async fn nearest(
    pool: &SqlitePool,
    group_id: Id,
    exclude: Id,
    pivot: f64,
    below: bool,
) -> Result<Option<f64>> {
    let sql = if below {
        "SELECT MAX(channel_number) FROM effective_channel
         WHERE channel_group_id = ? AND id <> ? AND channel_number < ?"
    } else {
        "SELECT MIN(channel_number) FROM effective_channel
         WHERE channel_group_id = ? AND id <> ? AND channel_number > ?"
    };
    Ok(sqlx::query_scalar(sql)
        .bind(group_id)
        .bind(exclude)
        .bind(pivot)
        .fetch_one(pool)
        .await?)
}

/// Two decimal places, which is as fine as the number a channel can be given
/// by hand. A number the editor cannot show is one nobody can correct.
fn round_hundredths(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// A free number strictly between two others, as round as the gap allows.
///
/// Whole numbers first, working outwards from the middle, because a channel
/// number is something a person reads off a guide and types into a remote:
/// splitting 20045 and 20050 down the middle gives 20047.5 when four whole
/// numbers were free, and doing that twice more gives 20048.75. A fraction is
/// correct — `channel_number` is REAL so that an OTA `5.1` can sit next to
/// `5.2` — but it should be what a gap smaller than 1 costs, not the first
/// answer.
///
/// Both searches are bounded: the neighbours are adjacent in the group, so
/// anything in between belongs to another group's overlapping range or to a
/// row a filter hid, and a few candidates either way covers that.
fn between(low: f64, high: f64, taken: impl Fn(f64) -> bool) -> Option<f64> {
    let middle = (low + high) / 2.0;
    for offset in 0..4 {
        let offset = f64::from(offset);
        for candidate in [middle.floor() - offset, middle.ceil() + offset] {
            if candidate > low && candidate < high && !taken(candidate) {
                return Some(candidate);
            }
        }
    }

    // Neighbours less than a whole number apart. Halving in from the top
    // rather than taking the midpoint once, for the same reason as above: the
    // midpoint may be exactly where a hidden row sits.
    let mut ceiling = high;
    for _ in 0..4 {
        let candidate = round_hundredths((low + ceiling) / 2.0);
        if candidate <= low || candidate >= ceiling {
            break;
        }
        if !taken(candidate) {
            return Some(candidate);
        }
        ceiling = candidate;
    }
    None
}

/// The number a channel takes when it is dropped between two others.
///
/// A number *is* the order, so a drop is one assignment: the channel takes a
/// free number between its new neighbours and **nothing else moves**. That is
/// what keeps moving one channel from costing a re-scan of the whole group in
/// Plex, and it is what the gaps in the step grid are for. When a pair has no
/// room left between them this refuses and says to renumber the group, rather
/// than pushing every channel below the drop down one.
///
/// `after` and `before` are only what the operator could see — the rows either
/// side of the drop on their page — so the bound they did not name is read
/// back from the group rather than taken to be its edge. A neighbour with no
/// number yet holds no position, and counts as no bound at all.
pub async fn plan_move(
    pool: &SqlitePool,
    channel: Id,
    after: Option<Id>,
    before: Option<Id>,
    step: f64,
) -> Result<f64> {
    let step = if step.is_finite() && step >= 1.0 {
        step
    } else {
        1.0
    };
    let (group_id, _) = slot_of(pool, channel).await?.ok_or(Error::NotFound)?;
    let Some(group_id) = group_id else {
        return Err(Error::invalid(
            "the channel is in no group, so it has no range to move inside",
        ));
    };
    let group = get_group(pool, group_id).await?.ok_or(Error::NotFound)?;

    let mut bounds: [Option<f64>; 2] = [None, None];
    for (index, neighbour) in [after, before].into_iter().enumerate() {
        let Some(id) = neighbour else { continue };
        let (neighbour_group, number) = slot_of(pool, id).await?.ok_or(Error::NotFound)?;
        if neighbour_group != Some(group_id) {
            return Err(Error::invalid(format!(
                "a channel moves inside its own group; drop it between two of {}'s channels",
                group.name
            )));
        }
        bounds[index] = number;
    }
    let [mut low, mut high] = bounds;
    if low.is_none()
        && let Some(above) = high
    {
        low = nearest(pool, group_id, channel, above, true).await?;
    }
    if high.is_none()
        && let Some(below) = low
    {
        high = nearest(pool, group_id, channel, below, false).await?;
    }

    // The moved channel's own number is not in the way of itself.
    let mut used: Vec<f64> = sqlx::query_scalar(
        "SELECT channel_number FROM effective_channel
         WHERE channel_number IS NOT NULL AND id <> ?",
    )
    .bind(channel)
    .fetch_all(pool)
    .await?;
    used.sort_by(f64::total_cmp);
    let taken = |number: f64| {
        used.binary_search_by(|probe| probe.total_cmp(&number))
            .is_ok()
    };
    let no_room = |detail: String| {
        Err(Error::invalid(format!(
            "{detail}; renumber {} to spread its numbers out",
            group.name
        )))
    };

    match (low, high) {
        (Some(low), Some(high)) => between(low, high, taken).map_or_else(
            || no_room(format!("there is no room between {low} and {high}")),
            Ok,
        ),
        // Dropped past the last channel of the group: the next slot on the
        // grid, which is where a new channel would have gone anyway.
        (Some(low), None) => {
            let mut slot = align_up(low, step);
            if slot <= low {
                slot += step;
            }
            while group.number_end.is_none_or(|end| slot <= end) {
                if !taken(slot) {
                    return Ok(slot);
                }
                slot += step;
            }
            Err(Error::invalid(format!(
                "{}'s range has no free number above {low}",
                group.name
            )))
        }
        // Dropped above the first: a whole step below it when the range has
        // the room, and the gap down to the range's start shared out when it
        // does not.
        (None, Some(high)) => {
            let floor = group.number_start.unwrap_or(0.0);
            let stepped = round_hundredths(high - step);
            if stepped >= floor && !taken(stepped) {
                return Ok(stepped);
            }
            // The range's own start is a number the group may use, and
            // `between` is exclusive at both ends.
            if floor < high && !taken(floor) {
                return Ok(floor);
            }
            between(floor, high, taken).map_or_else(
                || {
                    no_room(format!(
                        "there is no room below {high} in the group's range"
                    ))
                },
                Ok,
            )
        }
        (None, None) => Err(Error::invalid(
            "there is nothing to drop between: the group has no other numbered channel",
        )),
    }
}

/// Effective logo id per channel.
///
/// Separate from [`EffectiveChannel`], which carries the resolved *URL*: the
/// outputs rewrite that to the artwork-cache endpoint, and the cache is
/// addressed by id. Coalesced over the override like everything else, so a
/// hand-assigned logo is the one that gets cached.
pub async fn logo_ids(pool: &SqlitePool, ids: &[Id]) -> Result<Vec<(Id, Id)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(&format!(
        "SELECT id, logo_id FROM effective_channel
         WHERE logo_id IS NOT NULL AND id IN ({})",
        id_list(ids)
    ))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| (row.get("id"), row.get("logo_id")))
        .collect())
}

/// Failover lists for a page of channels, in one query rather than one each.
pub async fn stream_ids_for(pool: &SqlitePool, ids: &[Id]) -> Result<Vec<(Id, Id)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(&format!(
        "SELECT channel_id, stream_id FROM channel_stream
         WHERE channel_id IN ({}) ORDER BY channel_id, sort_order, stream_id",
        id_list(ids)
    ))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| (row.get("channel_id"), row.get("stream_id")))
        .collect())
}

/// Replace a channel's failover list. Position in `stream_ids` *is* the order,
/// so the caller never has to think about gaps or renumbering.
pub async fn set_streams(pool: &SqlitePool, channel_id: Id, stream_ids: &[Id]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM channel_stream WHERE channel_id = ?")
        .bind(channel_id)
        .execute(&mut *tx)
        .await?;

    for (position, stream_id) in stream_ids.iter().enumerate() {
        sqlx::query(
            "INSERT OR REPLACE INTO channel_stream (channel_id, stream_id, sort_order)
             VALUES (?, ?, ?)",
        )
        .bind(channel_id)
        .bind(stream_id)
        .bind(position as i64)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserLevel;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    fn draft(name: &str, number: f64) -> Channel {
        Channel {
            id: 0,
            uuid: Uuid::nil(),
            channel_number: Some(number),
            name: name.to_owned(),
            logo_id: None,
            channel_group_id: None,
            tvg_id: None,
            tvc_guide_stationid: None,
            epg_data_id: None,
            stream_profile_id: None,
            user_level: UserLevel::Streamer,
            is_adult: false,
            hidden_from_output: false,
            auto_created: false,
            is_catchup: false,
            catchup_days: 0,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    /// A channel in `group`, numbered, with an optional `tvg-id`.
    async fn member(
        pool: &SqlitePool,
        group: Id,
        name: &str,
        number: f64,
        tvg: Option<&str>,
    ) -> Id {
        let mut channel = draft(name, number);
        channel.channel_group_id = Some(group);
        channel.tvg_id = tvg.map(str::to_owned);
        create(pool, &channel).await.unwrap().id
    }

    async fn numbers(pool: &SqlitePool, ids: &[Id]) -> Vec<Option<f64>> {
        let mut out = Vec::new();
        for id in ids {
            out.push(slot_of(pool, *id).await.unwrap().unwrap().1);
        }
        out
    }

    #[test]
    fn digits_in_a_name_sort_as_numbers() {
        let mut names = vec!["tsn10", "tsn2", "tsn1", "tsn 4k", "sportsnet one"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["sportsnet one", "tsn 4k", "tsn1", "tsn2", "tsn10"]
        );

        // Leading zeros are decoration, and a longer number still wins.
        assert_eq!(natural_cmp("ch007", "ch7"), Ordering::Equal);
        assert_eq!(natural_cmp("ch7", "ch07b"), Ordering::Less);
        assert_eq!(natural_cmp("abc", "abcd"), Ordering::Less);
    }

    /// A group whose numbers carry click order rather than any sort, with the
    /// TSN channels split apart.
    async fn shuffled_sports(pool: &SqlitePool) -> (ChannelGroup, Vec<Id>) {
        let group = create_group(pool, "Canada Sports", (Some(20000.0), Some(20999.0)))
            .await
            .unwrap();
        let ids = vec![
            member(pool, group.id, "CA TSN1 (FHD)", 1.0, Some("TSN1.ca")).await,
            member(pool, group.id, "CA: TSN 4K", 2.0, Some("TSN4K.ca")).await,
            member(pool, group.id, "CA: Sportsnet One 4K", 3.0, None).await,
            member(pool, group.id, "CA TSN10 (FHD)", 4.0, Some("TSN10.ca")).await,
            member(pool, group.id, "CA TSN2 (FHD)", 5.0, Some("TSN2.ca")).await,
        ];
        (group, ids)
    }

    #[tokio::test]
    async fn renumbering_in_the_current_order_only_compacts_the_lineup() {
        let pool = pool().await;
        let (group, ids) = shuffled_sports(&pool).await;

        let plans = plan_renumber(&pool, &[group], 10.0, RenumberOrder::Current)
            .await
            .unwrap();

        // Every channel moves onto the grid, and none of them past another.
        assert_eq!(
            plans[0].assigned,
            ids.iter()
                .copied()
                .zip([20000.0, 20010.0, 20020.0, 20030.0, 20040.0])
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn renumbering_by_name_puts_tsn2_before_tsn10() {
        let pool = pool().await;
        let (group, ids) = shuffled_sports(&pool).await;
        let [tsn1, tsn4k, sportsnet, tsn10, tsn2] = ids[..] else {
            unreachable!()
        };

        let plans = plan_renumber(&pool, &[group], 10.0, RenumberOrder::Name)
            .await
            .unwrap();

        // "CA " sorts before "CA:", so the TSN family lands together and the
        // colon-prefixed names follow — and 10 lands after 2, not after 1.
        assert_eq!(
            plans[0].assigned,
            vec![
                (tsn1, 20000.0),
                (tsn2, 20010.0),
                (tsn10, 20020.0),
                (sportsnet, 20030.0),
                (tsn4k, 20040.0),
            ]
        );
    }

    #[tokio::test]
    async fn renumbering_by_tvg_id_leaves_the_channels_without_one_at_the_end() {
        let pool = pool().await;
        let (group, ids) = shuffled_sports(&pool).await;
        let [tsn1, tsn4k, sportsnet, tsn10, tsn2] = ids[..] else {
            unreachable!()
        };

        let plans = plan_renumber(&pool, &[group], 10.0, RenumberOrder::TvgId)
            .await
            .unwrap();

        assert_eq!(
            plans[0].assigned,
            vec![
                (tsn1, 20000.0),
                (tsn2, 20010.0),
                (tsn4k, 20020.0),
                (tsn10, 20030.0),
                // No tvg-id: last, rather than first under an empty key.
                (sportsnet, 20040.0),
            ]
        );
    }

    #[tokio::test]
    async fn renumbering_by_guide_name_follows_the_matched_channel() {
        let pool = pool().await;
        let (group, ids) = shuffled_sports(&pool).await;
        let [tsn1, tsn4k, sportsnet, tsn10, tsn2] = ids[..] else {
            unreachable!()
        };

        // The guide names the provider's channels differently from the M3U,
        // which is the reason to sort by them at all.
        for (channel, guide) in [
            (tsn1, "TSN 1"),
            (tsn2, "TSN 2"),
            (tsn10, "TSN 10"),
            (tsn4k, "TSN 4K"),
            (sportsnet, "Sportsnet One"),
        ] {
            let epg: Id = sqlx::query_scalar(
                "INSERT INTO epg_data (tvg_id, name) VALUES (?, ?) RETURNING id",
            )
            .bind(guide)
            .bind(guide)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query("UPDATE channel SET epg_data_id = ? WHERE id = ?")
                .bind(epg)
                .bind(channel)
                .execute(&pool)
                .await
                .unwrap();
        }

        let plans = plan_renumber(&pool, &[group], 10.0, RenumberOrder::Guide)
            .await
            .unwrap();

        assert_eq!(
            plans[0].assigned,
            vec![
                (sportsnet, 20000.0),
                (tsn1, 20010.0),
                (tsn2, 20020.0),
                (tsn4k, 20030.0),
                (tsn10, 20040.0),
            ]
        );
    }

    #[tokio::test]
    async fn a_dropped_channel_takes_a_number_from_the_gap_and_moves_nothing_else() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (Some(20000.0), Some(20999.0)))
            .await
            .unwrap();
        let first = member(&pool, group.id, "One", 20000.0, None).await;
        let second = member(&pool, group.id, "Two", 20010.0, None).await;
        let third = member(&pool, group.id, "Three", 20020.0, None).await;
        let last = member(&pool, group.id, "Four", 20030.0, None).await;

        // Dropped between the first two.
        let number = plan_move(&pool, last, Some(first), Some(second), 10.0)
            .await
            .unwrap();
        assert_eq!(number, 20005.0);

        apply_numbers(&pool, &[(last, number)]).await.unwrap();
        assert_eq!(
            numbers(&pool, &[first, second, third]).await,
            vec![Some(20000.0), Some(20010.0), Some(20020.0)],
            "the channels it was dropped between must not move"
        );
    }

    #[tokio::test]
    async fn a_channel_dropped_at_either_end_lands_on_the_grid() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (Some(20000.0), Some(20999.0)))
            .await
            .unwrap();
        let first = member(&pool, group.id, "One", 20010.0, None).await;
        let middle = member(&pool, group.id, "Two", 20020.0, None).await;
        let last = member(&pool, group.id, "Three", 20030.0, None).await;

        // Past the end: the next slot up, which is where a new channel goes.
        assert_eq!(
            plan_move(&pool, first, Some(last), None, 10.0)
                .await
                .unwrap(),
            20040.0
        );
        // Above the first: a whole step below it, still inside the range.
        assert_eq!(
            plan_move(&pool, last, None, Some(first), 10.0)
                .await
                .unwrap(),
            20000.0
        );
        // With the range's own start taken there is nowhere below it to go:
        // the answer is to renumber, not to invent a number outside the range
        // and land in the group below.
        sqlx::query("UPDATE channel SET channel_number = 20000 WHERE id = ?")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let refused = plan_move(&pool, last, None, Some(first), 10.0)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("no room below 20000"),
            "{refused}"
        );

        // Free, but less than a step above the start: the start itself, which
        // is the roundest number the range has to offer.
        sqlx::query("UPDATE channel SET channel_number = 20005 WHERE id = ?")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            plan_move(&pool, last, None, Some(first), 10.0)
                .await
                .unwrap(),
            20000.0
        );
        assert_eq!(numbers(&pool, &[middle]).await, vec![Some(20020.0)]);
    }

    #[tokio::test]
    async fn a_drop_reads_the_neighbour_the_page_did_not_show() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (Some(20000.0), Some(20999.0)))
            .await
            .unwrap();
        // The group's numbers start well inside its range, which is what tells
        // the two readings apart.
        let first = member(&pool, group.id, "One", 20500.0, None).await;
        let second = member(&pool, group.id, "Two", 20510.0, None).await;
        let moved = member(&pool, group.id, "Three", 20520.0, None).await;

        // Dropped at the top of a page, so the client can only name the row
        // below it. Read as the group's own edge, the answer would be halfway
        // from the range's start — 20255, which is above nothing and lands the
        // channel before the one it was dropped after.
        let number = plan_move(&pool, moved, None, Some(second), 10.0)
            .await
            .unwrap();
        assert_eq!(number, 20505.0);
        assert_eq!(numbers(&pool, &[first]).await, vec![Some(20500.0)]);
    }

    #[tokio::test]
    async fn a_drop_steps_around_a_channel_a_filter_was_hiding() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (Some(20000.0), Some(20999.0)))
            .await
            .unwrap();
        let low = member(&pool, group.id, "One", 20000.0, None).await;
        // Sitting exactly on the midpoint, and filtered out of the table.
        member(&pool, group.id, "Hidden", 20010.0, None).await;
        let high = member(&pool, group.id, "Three", 20020.0, None).await;
        let moved = member(&pool, group.id, "Four", 20030.0, None).await;

        let number = plan_move(&pool, moved, Some(low), Some(high), 10.0)
            .await
            .unwrap();
        assert_eq!(
            number, 20009.0,
            "the middle is taken, so the next whole number down"
        );
    }

    /// Halving a gap that holds whole numbers gives 20047.5, then 20048.75 —
    /// correct, unreadable, and not what the step grid's slack is for.
    #[tokio::test]
    async fn a_drop_spends_the_whole_numbers_in_the_gap_before_any_fraction() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (Some(20000.0), Some(20999.0)))
            .await
            .unwrap();
        let low = member(&pool, group.id, "One", 20045.0, None).await;
        let high = member(&pool, group.id, "Two", 20050.0, None).await;
        let moved = member(&pool, group.id, "Three", 20090.0, None).await;
        let again = member(&pool, group.id, "Four", 20091.0, None).await;

        assert_eq!(
            plan_move(&pool, moved, Some(low), Some(high), 10.0)
                .await
                .unwrap(),
            20047.0
        );
        apply_numbers(&pool, &[(moved, 20047.0)]).await.unwrap();

        // And the one after it, rather than splitting 20045 and 20047.
        assert_eq!(
            plan_move(&pool, again, Some(moved), Some(high), 10.0)
                .await
                .unwrap(),
            20048.0
        );
    }

    /// The fraction is still there for the gap it was meant for: an OTA tuner's
    /// 5.1 and 5.2 are a whole number apart from nothing.
    #[tokio::test]
    async fn a_drop_between_subchannels_takes_the_fraction() {
        let pool = pool().await;
        let group = create_group(&pool, "Locals", (Some(5.0), Some(5.9)))
            .await
            .unwrap();
        let low = member(&pool, group.id, "KAZ-DT", 5.1, None).await;
        let high = member(&pool, group.id, "KAZ-DT2", 5.2, None).await;
        let moved = member(&pool, group.id, "KAZ-DT3", 5.3, None).await;

        assert_eq!(
            plan_move(&pool, moved, Some(low), Some(high), 1.0)
                .await
                .unwrap(),
            5.15
        );
    }

    #[tokio::test]
    async fn a_drop_with_no_room_refuses_rather_than_renumbering_the_group() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (Some(1.0), Some(99.0)))
            .await
            .unwrap();
        let low = member(&pool, group.id, "One", 1.0, None).await;
        let high = member(&pool, group.id, "Two", 1.01, None).await;
        let moved = member(&pool, group.id, "Three", 5.0, None).await;

        let refused = plan_move(&pool, moved, Some(low), Some(high), 1.0)
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("no room between 1 and 1.01"),
            "{refused}"
        );
        assert!(refused.to_string().contains("renumber Sports"), "{refused}");
    }

    #[tokio::test]
    async fn a_channel_does_not_move_into_another_group() {
        let pool = pool().await;
        let sports = create_group(&pool, "Sports", (Some(100.0), Some(199.0)))
            .await
            .unwrap();
        let news = create_group(&pool, "News", (Some(200.0), Some(299.0)))
            .await
            .unwrap();
        let moved = member(&pool, sports.id, "One", 100.0, None).await;
        let elsewhere = member(&pool, news.id, "Two", 200.0, None).await;

        let refused = plan_move(&pool, moved, Some(elsewhere), None, 10.0)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("its own group"), "{refused}");
    }

    #[tokio::test]
    async fn a_created_channel_gets_a_uuid_and_timestamps() {
        let pool = pool().await;
        let created = create(&pool, &draft("VRIX", 1.0)).await.unwrap();

        assert!(!created.uuid.is_nil());
        assert!(
            created.created_at.timestamp() > 0,
            "timestamp did not decode"
        );
        assert_eq!(
            get_effective_by_uuid(&pool, created.uuid)
                .await
                .unwrap()
                .unwrap()
                .id,
            created.id
        );
    }

    #[tokio::test]
    async fn the_override_wins_and_the_base_row_is_left_alone() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (None, None)).await.unwrap();
        let other = create_group(&pool, "News", (None, None)).await.unwrap();

        let mut channel = draft("Provider Name", 5.0);
        channel.channel_group_id = Some(group.id);
        let channel = create(&pool, &channel).await.unwrap();

        save_override(
            &pool,
            &ChannelOverride {
                channel_id: channel.id,
                name: Some("My Name".into()),
                channel_number: Some(101.0),
                channel_group_id: Some(other.id),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let effective = get_effective(&pool, channel.id).await.unwrap().unwrap();
        assert_eq!(effective.name, "My Name");
        assert_eq!(effective.channel_number, Some(101.0));
        assert_eq!(effective.group_name.as_deref(), Some("News"));

        // Sync keeps writing to the base row; it must be untouched.
        let base = get(&pool, channel.id).await.unwrap().unwrap();
        assert_eq!(base.name, "Provider Name");
        assert_eq!(base.channel_group_id, Some(group.id));

        clear_override(&pool, channel.id).await.unwrap();
        let reverted = get_effective(&pool, channel.id).await.unwrap().unwrap();
        assert_eq!(reverted.name, "Provider Name");
        assert_eq!(reverted.group_name.as_deref(), Some("Sports"));
    }

    #[tokio::test]
    async fn a_null_override_field_inherits_rather_than_blanking() {
        let pool = pool().await;
        let channel = create(&pool, &draft("Base", 3.0)).await.unwrap();

        save_override(
            &pool,
            &ChannelOverride {
                channel_id: channel.id,
                channel_number: Some(77.0),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let effective = get_effective(&pool, channel.id).await.unwrap().unwrap();
        assert_eq!(effective.name, "Base");
        assert_eq!(effective.channel_number, Some(77.0));
    }

    #[tokio::test]
    async fn sorting_and_searching_happen_in_sql_over_effective_values() {
        let pool = pool().await;
        let a = create(&pool, &draft("Alpha", 3.0)).await.unwrap();
        create(&pool, &draft("Bravo", 2.0)).await.unwrap();
        create(&pool, &draft("Charlie", 1.0)).await.unwrap();

        // Renaming and renumbering through the override must reorder the list.
        save_override(
            &pool,
            &ChannelOverride {
                channel_id: a.id,
                name: Some("Zulu".into()),
                channel_number: Some(0.5),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let by_number = list_effective(&pool, &ChannelFilter::default(), None, None)
            .await
            .unwrap();
        let names: Vec<&str> = by_number.results.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Zulu", "Charlie", "Bravo"]);

        let by_name = list_effective(&pool, &ChannelFilter::default(), Some("-name"), None)
            .await
            .unwrap();
        assert_eq!(by_name.results[0].name, "Zulu");

        let searched = list_effective(
            &pool,
            &ChannelFilter {
                search: Some("ulu"),
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(searched.count, 1);
        assert_eq!(searched.results[0].name, "Zulu");
    }

    #[tokio::test]
    async fn profile_and_visibility_filters_apply() {
        let pool = pool().await;
        let visible = create(&pool, &draft("Visible", 1.0)).await.unwrap();
        let mut hidden = draft("Hidden", 2.0);
        hidden.hidden_from_output = true;
        let hidden = create(&pool, &hidden).await.unwrap();

        let all = list_effective(&pool, &ChannelFilter::default(), None, None)
            .await
            .unwrap();
        assert_eq!(all.count, 2);

        let output = list_effective(
            &pool,
            &ChannelFilter {
                visible_only: true,
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.count, 1);
        assert_eq!(output.results[0].id, visible.id);

        // Both channels joined profile 1 automatically; disabling one removes
        // it from that profile's output without hiding it everywhere.
        sqlx::query(
            "UPDATE channel_profile_membership SET enabled = 0
             WHERE channel_profile_id = 1 AND channel_id = ?",
        )
        .bind(hidden.id)
        .execute(&pool)
        .await
        .unwrap();

        let in_profile = list_effective(
            &pool,
            &ChannelFilter {
                profile_id: Some(1),
                ..Default::default()
            },
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(in_profile.count, 1);
    }

    #[tokio::test]
    async fn the_failover_list_keeps_the_order_it_was_given() {
        let pool = pool().await;
        let channel = create(&pool, &draft("Ordered", 1.0)).await.unwrap();
        for id in 1..=3 {
            sqlx::query("INSERT INTO stream (id, name) VALUES (?, ?)")
                .bind(id)
                .bind(format!("s{id}"))
                .execute(&pool)
                .await
                .unwrap();
        }

        set_streams(&pool, channel.id, &[3, 1, 2]).await.unwrap();
        assert_eq!(
            stream_ids_for(&pool, &[channel.id])
                .await
                .unwrap()
                .into_iter()
                .map(|(_, stream)| stream)
                .collect::<Vec<_>>(),
            vec![3, 1, 2]
        );

        set_streams(&pool, channel.id, &[2, 3]).await.unwrap();
        assert_eq!(
            super::super::streams::for_channel(&pool, channel.id)
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[tokio::test]
    async fn categories_follow_the_lineup_and_skip_empty_groups() {
        let pool = pool().await;
        let sports = create_group(&pool, "Sports", (None, None)).await.unwrap();
        let news = create_group(&pool, "News", (None, None)).await.unwrap();
        create_group(&pool, "Nobody Here", (None, None))
            .await
            .unwrap();

        // "News" sorts first alphabetically but appears second in the lineup.
        for (name, number, group) in [("Later", 9.0, news.id), ("First", 1.0, sports.id)] {
            let mut channel = draft(name, number);
            channel.channel_group_id = Some(group);
            create(&pool, &channel).await.unwrap();
        }

        let mut hidden = draft("Hidden", 0.5);
        hidden.channel_group_id = Some(news.id);
        hidden.hidden_from_output = true;
        create(&pool, &hidden).await.unwrap();

        let groups = groups_in_lineup_order(&pool, None).await.unwrap();
        assert_eq!(
            groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            vec!["Sports", "News"],
            "a hidden channel promoted its group, or an empty group appeared"
        );
    }

    #[tokio::test]
    async fn logo_ids_follow_the_override() {
        let pool = pool().await;
        sqlx::query("INSERT INTO logo (id, name, url) VALUES (1, 'a', 'u1'), (2, 'b', 'u2')")
            .execute(&pool)
            .await
            .unwrap();

        let mut channel = draft("Art", 1.0);
        channel.logo_id = Some(1);
        let channel = create(&pool, &channel).await.unwrap();
        assert_eq!(
            logo_ids(&pool, &[channel.id]).await.unwrap(),
            vec![(channel.id, 1)]
        );

        save_override(
            &pool,
            &ChannelOverride {
                channel_id: channel.id,
                logo_id: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            logo_ids(&pool, &[channel.id]).await.unwrap(),
            vec![(channel.id, 2)]
        );
    }

    #[tokio::test]
    async fn groups_report_what_references_them() {
        let pool = pool().await;
        let group = create_group(&pool, "Sports", (None, None)).await.unwrap();
        let mut channel = draft("VRIX", 1.0);
        channel.channel_group_id = Some(group.id);
        create(&pool, &channel).await.unwrap();

        let usage = group_usage(&pool).await.unwrap();
        assert_eq!(usage, vec![(group.id, 1, 0)]);

        assert!(matches!(
            create_group(&pool, "sports", (None, None)).await,
            Err(Error::Conflict(_))
        ));
    }
}
