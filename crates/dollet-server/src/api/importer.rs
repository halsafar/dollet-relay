//! One-shot migration from a Dispatcharr instance.
//!
//! The source is the `database.dump` inside a backup zip, read in process and
//! the only input there is. Ids are preserved throughout: the HDHR
//! lineup, the Xtream Codes API and every client bookmark address channels by
//! id and uuid, so renumbering would break configurations that the whole point
//! of this migration is to keep working.
//!
//! Nothing here aborts on a bad row. A migration that stops two thirds of the
//! way through leaves a database nobody can reason about; a migration that
//! finishes and hands back a list of what it could not carry is actionable.
//! The exception is a column that will not decode at all, which means the
//! source is not shaped the way this code believes: that stops the run, because
//! the alternative is dropping rows on a guess.
//!
//! Every write is `ON CONFLICT DO UPDATE`, never `INSERT OR REPLACE`. The
//! latter is a DELETE followed by an INSERT, so re-running an import would
//! fire `ON DELETE CASCADE` on `m3u_account` and `channel` and take the
//! streams and the whole failover catalogue with it before the rows that
//! replace them are written.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use chrono::{DateTime, Utc};
use dollet_core::config::Config;
use dollet_core::db::m3u::PatternCheck;
use dollet_core::db::sql_timestamp;
use dollet_core::parse::pgdump::{Archive, Backup, BackupMetadata, TextRow};
use dollet_core::settings;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use uuid::Uuid;

/// Upstream's locked catalogue rows are replaced by this project's own, whose
/// ffmpeg parameters are derived from ffmpeg's documentation rather than
/// copied. Importing them would both reintroduce that text and collide with
/// the seeded names, which are unique.
const SKIP_LOCKED: &str = "locked row replaced by the shipped default";

#[derive(Debug, Default, Serialize)]
pub struct ImportReport {
    pub counts: BTreeMap<String, u64>,
    /// Patterns that will not compile. These change which streams are filtered
    /// and how channels are renamed, so they are the loudest thing here.
    pub regex_failures: Vec<PatternProblem>,
    /// Patterns that compiled only after normalisation.
    pub regex_rewritten: Vec<PatternProblem>,
    pub warnings: Vec<String>,
    /// What the backup zip said about itself, when the input was a zip rather
    /// than a bare `database.dump`. Recorded so a report from a failed or
    /// surprising migration carries the version it was taken from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupMetadata>,
}

#[derive(Debug, Serialize)]
pub struct PatternProblem {
    pub source: String,
    pub pattern: String,
    pub detail: String,
}

impl ImportReport {
    fn add(&mut self, table: &str, n: u64) {
        *self.counts.entry(table.to_owned()).or_default() += n;
    }

    fn warn(&mut self, message: impl Into<String>) {
        let message = message.into();
        tracing::warn!(%message, "import warning");
        self.warnings.push(message);
    }
}

/// Compile a stored pattern, reporting rather than failing.
fn check_pattern(report: &mut ImportReport, source: &str, pattern: &str) {
    match dollet_core::db::m3u::check_pattern(pattern) {
        PatternCheck::Ok => {}
        PatternCheck::Rewritten(normalised) => report.regex_rewritten.push(PatternProblem {
            source: source.to_owned(),
            pattern: pattern.to_owned(),
            detail: format!("JS backreference rewritten as `{normalised}`"),
        }),
        PatternCheck::Failed(error) => report.regex_failures.push(PatternProblem {
            source: source.to_owned(),
            pattern: pattern.to_owned(),
            detail: error,
        }),
    }
}

/// Where the rows come from: the `pg_dump` archive inside a backup zip.
///
/// It answers the one question the import asks — every row of this table, as
/// text — and every `import_*` function below is written against that alone.
pub struct Source {
    archive: Archive,
    metadata: Option<BackupMetadata>,
}

impl Source {
    /// A `dispatcharr-backup-*.zip`, or the bare `database.dump` inside one.
    pub async fn backup(path: &Path) -> anyhow::Result<Self> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let backup = Backup::read(&bytes).with_context(|| format!("reading {}", path.display()))?;

        // The metadata describes the backup rather than the data, so it is
        // logged and not acted on: it is what tells an operator reading the log
        // afterwards which backup this instance was built from. Whitespace
        // collapsed because it is pretty-printed JSON, and a log record that
        // spans eight lines is not one a person reads.
        if let Some(metadata) = &backup.metadata {
            tracing::info!(?metadata, "reading Dispatcharr backup");
        }
        Ok(Self {
            archive: backup.archive,
            metadata: backup.metadata,
        })
    }

    /// Named columns of a named table, or an error saying which is missing.
    ///
    /// This is the whole surface the schema can drift under: a table renamed
    /// or a column dropped upstream fails here, by name, before a single row
    /// is written — rather than importing a silently emptier instance.
    async fn rows(&self, table: &str, columns: &[&str]) -> anyhow::Result<Vec<Row>> {
        let present = self
            .archive
            .columns(table)
            .with_context(|| format!("the backup has no table `{table}`"))?;
        for column in columns {
            anyhow::ensure!(
                present.iter().any(|name| name == column),
                "`{table}` in the backup has no column `{column}`"
            );
        }
        Ok(self.archive.rows(table)?.into_iter().map(Row).collect())
    }
}

/// One row of a table, as `COPY` wrote it.
pub struct Row(TextRow);

impl Row {
    fn get<T: Column>(&self, column: &str) -> anyhow::Result<T> {
        T::from_text(
            self.0
                .get(column)
                .with_context(|| format!("the backup has no column `{column}`"))?,
        )
        .with_context(|| format!("column `{column}`"))
    }
}

/// Decoding one column out of the text `COPY` wrote: `t`/`f` for a boolean,
/// `2026-09-10 19:16:21.781437+00` for a timestamp, JSON for a jsonb, and the
/// digits for everything numeric.
trait Column: Sized {
    fn from_text(value: Option<&str>) -> anyhow::Result<Self>;
}

macro_rules! column {
    ($type:ty, $parse:expr) => {
        impl Column for $type {
            fn from_text(value: Option<&str>) -> anyhow::Result<Self> {
                let parse: fn(&str) -> anyhow::Result<$type> = $parse;
                parse(value.context("NULL in a column that cannot hold one")?)
            }
        }

        impl Column for Option<$type> {
            fn from_text(value: Option<&str>) -> anyhow::Result<Self> {
                let parse: fn(&str) -> anyhow::Result<$type> = $parse;
                value.map(parse).transpose()
            }
        }
    };
}

column!(i64, |text| Ok(text.parse()?));
column!(i32, |text| Ok(text.parse()?));
column!(f64, |text| Ok(text.parse()?));
column!(String, |text| Ok(text.to_owned()));
column!(Uuid, |text| Ok(text.parse()?));
column!(Value, |text| Ok(serde_json::from_str(text)?));
column!(DateTime<Utc>, |text| parse_timestamp(text));
column!(bool, |text| match text {
    "t" => Ok(true),
    "f" => Ok(false),
    other => anyhow::bail!("`{other}` is not a COPY boolean"),
});

/// `%#z` is the only chrono specifier that accepts the hours-only offset COPY
/// writes (`+00`), and `%.f` the only one that makes the fractional seconds
/// optional — both of which real rows need.
fn parse_timestamp(text: &str) -> anyhow::Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%#z")
        .with_context(|| format!("`{text}` is not a timestamp in COPY's spelling"))?;
    Ok(parsed.with_timezone(&Utc))
}

pub async fn run(source: Source, sqlite: &SqlitePool) -> anyhow::Result<ImportReport> {
    let mut report = ImportReport {
        backup: source.metadata.clone(),
        ..ImportReport::default()
    };

    // Order follows the foreign keys. Channel profiles precede channels so the
    // membership trigger has profiles to attach them to.
    import_user_agents(&source, sqlite, &mut report).await?;
    import_stream_profiles(&source, sqlite, &mut report).await?;
    import_output_profiles(&source, sqlite, &mut report).await?;
    import_server_groups(&source, sqlite, &mut report).await?;
    import_m3u_accounts(&source, sqlite, &mut report).await?;
    import_m3u_account_profiles(&source, sqlite, &mut report).await?;
    import_m3u_filters(&source, sqlite, &mut report).await?;
    import_channel_groups(&source, sqlite, &mut report).await?;
    import_group_links(&source, sqlite, &mut report).await?;
    carry_group_ranges(sqlite).await?;
    import_logos(&source, sqlite, &mut report).await?;
    import_epg_sources(&source, sqlite, &mut report).await?;
    import_epg_data(&source, sqlite, &mut report).await?;
    import_programs(&source, sqlite, &mut report).await?;
    import_streams(&source, sqlite, &mut report).await?;
    import_channel_profiles(&source, sqlite, &mut report).await?;
    import_channels(&source, sqlite, &mut report).await?;
    import_channel_overrides(&source, sqlite, &mut report).await?;
    import_channel_streams(&source, sqlite, &mut report).await?;
    import_memberships(&source, sqlite, &mut report).await?;
    import_users(&source, sqlite, &mut report).await?;
    import_settings(&source, sqlite, &mut report).await?;

    Ok(report)
}

/// Rows per transaction. Bulk work is never one long transaction: SQLite's WAL
/// cannot checkpoint while one is open.
const BATCH: usize = 500;

/// A jsonb column as the text SQLite stores. An absent or undecodable value
/// becomes `{}` rather than failing the row: these are Dispatcharr's own
/// free-form property bags, and none of them is load-bearing.
fn json_of(row: &Row, column: &str) -> String {
    row.get::<Option<Value>>(column)
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}))
        .to_string()
}

async fn import_user_agents(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "core_useragent",
            &["id", "name", "user_agent", "description", "is_active"],
        )
        .await?;

    for row in &rows {
        let result = sqlx::query(
            "INSERT INTO user_agent (id, name, user_agent, description, is_active)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 user_agent = excluded.user_agent,
                 description = excluded.description,
                 is_active = excluded.is_active",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<String>("name")?)
        .bind(row.get::<String>("user_agent")?)
        .bind(row.get::<String>("description")?)
        .bind(row.get::<bool>("is_active")? as i64)
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("user_agent", 1),
            Err(e) => report.warn(format!(
                "user agent `{}` not imported: {e}",
                row.get::<String>("name")?
            )),
        }
    }
    Ok(())
}

async fn import_stream_profiles(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "core_streamprofile",
            &[
                "id",
                "name",
                "command",
                "parameters",
                "is_active",
                "user_agent_id",
                "locked",
            ],
        )
        .await?;

    for row in &rows {
        if row.get::<bool>("locked")? {
            report.add("stream_profile_skipped", 1);
            tracing::debug!(name = %row.get::<String>("name")?, SKIP_LOCKED);
            continue;
        }

        let result = sqlx::query(
            "INSERT INTO stream_profile (id, name, command, parameters, is_active, user_agent_id)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 command = excluded.command,
                 parameters = excluded.parameters,
                 is_active = excluded.is_active,
                 user_agent_id = excluded.user_agent_id",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<String>("name")?)
        .bind(row.get::<String>("command")?)
        .bind(row.get::<String>("parameters")?)
        .bind(row.get::<bool>("is_active")? as i64)
        .bind(row.get::<Option<i64>>("user_agent_id")?)
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("stream_profile", 1),
            Err(e) => report.warn(format!(
                "stream profile `{}` not imported: {e}",
                row.get::<String>("name")?
            )),
        }
    }
    Ok(())
}

async fn import_output_profiles(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "core_outputprofile",
            &["id", "name", "command", "parameters", "is_active", "locked"],
        )
        .await?;

    for row in &rows {
        if row.get::<bool>("locked")? {
            report.add("output_profile_skipped", 1);
            tracing::debug!(name = %row.get::<String>("name")?, SKIP_LOCKED);
            continue;
        }

        let result = sqlx::query(
            "INSERT INTO output_profile (id, name, command, parameters, is_active)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 command = excluded.command,
                 parameters = excluded.parameters,
                 is_active = excluded.is_active",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<String>("name")?)
        .bind(row.get::<String>("command")?)
        .bind(row.get::<String>("parameters")?)
        .bind(row.get::<bool>("is_active")? as i64)
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("output_profile", 1),
            Err(e) => report.warn(format!(
                "output profile `{}` not imported: {e}",
                row.get::<String>("name")?
            )),
        }
    }
    Ok(())
}

async fn import_server_groups(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source.rows("m3u_servergroup", &["id", "name"]).await?;

    for row in &rows {
        sqlx::query(
            "INSERT INTO server_group (id, name) VALUES (?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<String>("name")?)
        .execute(sqlite)
        .await?;
        report.add("server_group", 1);
    }
    Ok(())
}

/// Upstream stores `STD` or `XC`.
fn account_type(raw: &str) -> &'static str {
    match raw {
        "XC" => "xtream_codes",
        _ => "standard",
    }
}

async fn import_m3u_accounts(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "m3u_m3uaccount",
            &[
                "id",
                "name",
                "account_type",
                "server_url",
                "file_path",
                "username",
                "password",
                "max_streams",
                "is_active",
                "locked",
                "priority",
                "server_group_id",
                "user_agent_id",
                "stream_profile_id",
                "refresh_interval",
                "stale_stream_days",
                "status",
                "last_message",
                "custom_properties",
            ],
        )
        .await?;

    for row in &rows {
        let result = sqlx::query(
            "INSERT INTO m3u_account (id, name, account_type, server_url, file_path, username,
                                      password, max_streams, is_active, locked, priority,
                                      server_group_id, user_agent_id, stream_profile_id,
                                      refresh_interval_hours, stale_stream_days, status,
                                      last_message, custom_properties)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 account_type = excluded.account_type,
                 server_url = excluded.server_url,
                 file_path = excluded.file_path,
                 username = excluded.username,
                 password = excluded.password,
                 max_streams = excluded.max_streams,
                 is_active = excluded.is_active,
                 priority = excluded.priority,
                 server_group_id = excluded.server_group_id,
                 user_agent_id = excluded.user_agent_id,
                 stream_profile_id = excluded.stream_profile_id,
                 refresh_interval_hours = excluded.refresh_interval_hours,
                 stale_stream_days = excluded.stale_stream_days,
                 status = excluded.status,
                 last_message = excluded.last_message,
                 custom_properties = excluded.custom_properties",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<String>("name")?)
        .bind(account_type(&row.get::<String>("account_type")?))
        .bind(row.get::<Option<String>>("server_url")?)
        .bind(row.get::<Option<String>>("file_path")?)
        .bind(row.get::<Option<String>>("username")?)
        .bind(row.get::<Option<String>>("password")?)
        .bind(i64::from(row.get::<i32>("max_streams")?))
        .bind(row.get::<bool>("is_active")? as i64)
        .bind(row.get::<bool>("locked")? as i64)
        .bind(i64::from(row.get::<i32>("priority")?))
        .bind(row.get::<Option<i64>>("server_group_id")?)
        .bind(row.get::<Option<i64>>("user_agent_id")?)
        .bind(row.get::<Option<i64>>("stream_profile_id")?)
        .bind(i64::from(row.get::<i32>("refresh_interval")?))
        .bind(i64::from(row.get::<i32>("stale_stream_days")?))
        .bind(row.get::<String>("status")?)
        .bind(row.get::<Option<String>>("last_message")?)
        .bind(json_of(row, "custom_properties"))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("m3u_account", 1),
            Err(e) => report.warn(format!(
                "M3U account `{}` not imported: {e}",
                row.get::<String>("name")?
            )),
        }
    }
    Ok(())
}

async fn import_m3u_account_profiles(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "m3u_m3uaccountprofile",
            &[
                "id",
                "m3u_account_id",
                "name",
                "is_default",
                "is_active",
                "max_streams",
                "search_pattern",
                "replace_pattern",
                "custom_properties",
            ],
        )
        .await?;

    for row in &rows {
        let name: String = row.get("name")?;
        let search: String = row.get("search_pattern")?;
        check_pattern(report, &format!("m3u profile `{name}` search"), &search);

        let result = sqlx::query(
            "INSERT INTO m3u_account_profile (id, m3u_account_id, name, is_default, is_active,
                                              max_streams, search_pattern, replace_pattern,
                                              custom_properties)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 is_default = excluded.is_default,
                 is_active = excluded.is_active,
                 max_streams = excluded.max_streams,
                 search_pattern = excluded.search_pattern,
                 replace_pattern = excluded.replace_pattern,
                 custom_properties = excluded.custom_properties",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<i64>("m3u_account_id")?)
        .bind(&name)
        .bind(row.get::<bool>("is_default")? as i64)
        .bind(row.get::<bool>("is_active")? as i64)
        .bind(i64::from(row.get::<i32>("max_streams")?))
        .bind(&search)
        .bind(row.get::<String>("replace_pattern")?)
        .bind(json_of(row, "custom_properties"))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("m3u_account_profile", 1),
            Err(e) => report.warn(format!("M3U profile `{name}` not imported: {e}")),
        }
    }
    Ok(())
}

async fn import_m3u_filters(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "m3u_m3ufilter",
            &[
                "id",
                "m3u_account_id",
                "filter_type",
                "regex_pattern",
                "exclude",
                "order",
            ],
        )
        .await?;

    for row in &rows {
        let pattern: String = row.get("regex_pattern")?;
        let id: i64 = row.get("id")?;
        check_pattern(report, &format!("m3u filter {id}"), &pattern);

        let result = sqlx::query(
            "INSERT INTO m3u_filter (id, m3u_account_id, filter_type, regex_pattern,
                                     exclude, sort_order)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 m3u_account_id = excluded.m3u_account_id,
                 filter_type = excluded.filter_type,
                 regex_pattern = excluded.regex_pattern,
                 exclude = excluded.exclude,
                 sort_order = excluded.sort_order",
        )
        .bind(id)
        .bind(row.get::<i64>("m3u_account_id")?)
        .bind(row.get::<String>("filter_type")?.to_lowercase())
        .bind(&pattern)
        .bind(row.get::<bool>("exclude")? as i64)
        .bind(i64::from(row.get::<i32>("order")?))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("m3u_filter", 1),
            Err(e) => report.warn(format!("M3U filter {id} not imported: {e}")),
        }
    }
    Ok(())
}

async fn import_channel_groups(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows("dispatcharr_channels_channelgroup", &["id", "name"])
        .await?;

    for row in &rows {
        let name: String = row.get("name")?;
        match sqlx::query(
            "INSERT INTO channel_group (id, name) VALUES (?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name",
        )
        .bind(row.get::<i64>("id")?)
        .bind(&name)
        .execute(sqlite)
        .await
        {
            Ok(_) => report.add("channel_group", 1),
            Err(e) => report.warn(format!("channel group `{name}` not imported: {e}")),
        }
    }
    Ok(())
}

async fn import_group_links(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "dispatcharr_channels_channelgroupm3uaccount",
            &[
                "id",
                "channel_group_id",
                "m3u_account_id",
                "enabled",
                "auto_channel_sync",
                "auto_sync_channel_start",
                "auto_sync_channel_end",
                "is_stale",
                "last_seen",
                "custom_properties",
            ],
        )
        .await?;

    for row in &rows {
        let result = sqlx::query(
            "INSERT INTO channel_group_m3u_account
                 (id, channel_group_id, m3u_account_id, enabled, auto_channel_sync,
                  auto_sync_channel_start, auto_sync_channel_end, is_stale, last_seen,
                  custom_properties)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 enabled = excluded.enabled,
                 auto_channel_sync = excluded.auto_channel_sync,
                 auto_sync_channel_start = excluded.auto_sync_channel_start,
                 auto_sync_channel_end = excluded.auto_sync_channel_end,
                 is_stale = excluded.is_stale,
                 last_seen = excluded.last_seen,
                 custom_properties = excluded.custom_properties",
        )
        .bind(row.get::<i64>("id")?)
        .bind(row.get::<i64>("channel_group_id")?)
        .bind(row.get::<i64>("m3u_account_id")?)
        .bind(row.get::<bool>("enabled")? as i64)
        .bind(row.get::<bool>("auto_channel_sync")? as i64)
        .bind(row.get::<Option<f64>>("auto_sync_channel_start")?)
        .bind(row.get::<Option<f64>>("auto_sync_channel_end")?)
        .bind(row.get::<bool>("is_stale")? as i64)
        .bind(sql_timestamp(row.get::<DateTime<Utc>>("last_seen")?))
        .bind(json_of(row, "custom_properties"))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("channel_group_m3u_account", 1),
            Err(e) => report.warn(format!("group/account link not imported: {e}")),
        }
    }
    Ok(())
}

/// A source keeps a group's number range on its provider link, for auto-sync
/// alone; here it is the group's, read by every create. So the import lifts it
/// across: a range an operator set is carried, and the untouched model default
/// — start 1, no end, auto-sync off, present on every link — is left behind
/// rather than presented as a range nobody chose.
pub(crate) async fn carry_group_ranges(sqlite: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE channel_group SET
             number_start = (SELECT l.auto_sync_channel_start FROM channel_group_m3u_account l
                              WHERE l.channel_group_id = channel_group.id
                                AND l.enabled = 1
                                AND l.auto_sync_channel_start IS NOT NULL
                                AND NOT (l.auto_sync_channel_start = 1
                                         AND l.auto_sync_channel_end IS NULL
                                         AND l.auto_channel_sync = 0)
                              ORDER BY l.id LIMIT 1),
             number_end   = (SELECT l.auto_sync_channel_end FROM channel_group_m3u_account l
                              WHERE l.channel_group_id = channel_group.id
                                AND l.enabled = 1
                                AND l.auto_sync_channel_start IS NOT NULL
                                AND NOT (l.auto_sync_channel_start = 1
                                         AND l.auto_sync_channel_end IS NULL
                                         AND l.auto_channel_sync = 0)
                              ORDER BY l.id LIMIT 1)
         WHERE number_start IS NULL",
    )
    .execute(sqlite)
    .await
    .context("carrying group number ranges off their provider links")?;
    Ok(())
}

async fn import_logos(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows("dispatcharr_channels_logo", &["id", "name", "url"])
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO logo (id, name, url) VALUES (?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name, url = excluded.url",
            )
            .bind(row.get::<i64>("id")?)
            .bind(row.get::<String>("name")?)
            .bind(row.get::<String>("url")?)
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("logo row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("logo", written);
    Ok(())
}

async fn import_epg_sources(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "epg_epgsource",
            &[
                "id",
                "name",
                "source_type",
                "url",
                "file_path",
                "username",
                "password",
                "is_active",
                "priority",
                "refresh_interval",
                "status",
                "last_message",
                "custom_properties",
            ],
        )
        .await?;

    for row in &rows {
        let name: String = row.get("name")?;
        let source_type = match row.get::<String>("source_type")?.as_str() {
            "dummy" => "dummy",
            "schedules_direct" => {
                report.warn(format!(
                    "EPG source `{name}` is Schedules Direct, which is out of scope; imported as XMLTV and left inactive"
                ));
                "xmltv"
            }
            _ => "xmltv",
        };

        let result = sqlx::query(
            "INSERT INTO epg_source (id, name, source_type, url, file_path, username, password,
                                     is_active, priority, refresh_interval_hours, status,
                                     last_message, custom_properties)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 name = excluded.name,
                 source_type = excluded.source_type,
                 url = excluded.url,
                 file_path = excluded.file_path,
                 username = excluded.username,
                 password = excluded.password,
                 is_active = excluded.is_active,
                 priority = excluded.priority,
                 refresh_interval_hours = excluded.refresh_interval_hours,
                 status = excluded.status,
                 last_message = excluded.last_message,
                 custom_properties = excluded.custom_properties",
        )
        .bind(row.get::<i64>("id")?)
        .bind(&name)
        .bind(source_type)
        .bind(row.get::<Option<String>>("url")?)
        .bind(row.get::<Option<String>>("file_path")?)
        .bind(row.get::<Option<String>>("username")?)
        .bind(row.get::<Option<String>>("password")?)
        .bind(row.get::<bool>("is_active")? as i64)
        .bind(i64::from(row.get::<i32>("priority")?))
        .bind(i64::from(row.get::<i32>("refresh_interval")?))
        .bind(row.get::<String>("status")?)
        .bind(row.get::<Option<String>>("last_message")?)
        .bind(json_of(row, "custom_properties"))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("epg_source", 1),
            Err(e) => report.warn(format!("EPG source `{name}` not imported: {e}")),
        }
    }
    Ok(())
}

async fn import_epg_data(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "epg_epgdata",
            &["id", "epg_source_id", "tvg_id", "name", "icon_url"],
        )
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO epg_data (id, epg_source_id, tvg_id, name, icon_url)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET
                     epg_source_id = excluded.epg_source_id,
                     tvg_id = excluded.tvg_id,
                     name = excluded.name,
                     icon_url = excluded.icon_url",
            )
            .bind(row.get::<i64>("id")?)
            .bind(row.get::<Option<i64>>("epg_source_id")?)
            .bind(row.get::<Option<String>>("tvg_id")?)
            .bind(row.get::<String>("name")?)
            .bind(row.get::<Option<String>>("icon_url")?)
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("epg_data row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("epg_data", written);
    Ok(())
}

async fn import_programs(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "epg_programdata",
            &[
                "id",
                "epg_id",
                "tvg_id",
                "start_time",
                "end_time",
                "title",
                "sub_title",
                "description",
                "custom_properties",
            ],
        )
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO program (id, epg_data_id, tvg_id, start_time, end_time,
                                      title, sub_title, description, custom_properties)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET
                     epg_data_id = excluded.epg_data_id,
                     tvg_id = excluded.tvg_id,
                     start_time = excluded.start_time,
                     end_time = excluded.end_time,
                     title = excluded.title,
                     sub_title = excluded.sub_title,
                     description = excluded.description,
                     custom_properties = excluded.custom_properties",
            )
            .bind(row.get::<i64>("id")?)
            .bind(row.get::<i64>("epg_id")?)
            .bind(row.get::<Option<String>>("tvg_id")?)
            .bind(sql_timestamp(row.get::<DateTime<Utc>>("start_time")?))
            .bind(sql_timestamp(row.get::<DateTime<Utc>>("end_time")?))
            .bind(row.get::<String>("title")?)
            .bind(row.get::<Option<String>>("sub_title")?)
            .bind(row.get::<Option<String>>("description")?)
            .bind(json_of(row, "custom_properties"))
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("program row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("program", written);
    Ok(())
}

async fn import_streams(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "dispatcharr_channels_stream",
            &[
                "id",
                "name",
                "url",
                "logo_url",
                "tvg_id",
                "channel_group_id",
                "m3u_account_id",
                "stream_profile_id",
                "is_custom",
                "is_adult",
                "stream_id",
                "stream_chno",
                "stream_hash",
                "last_seen",
                "is_stale",
                "is_catchup",
                "catchup_days",
                "custom_properties",
                "stream_stats",
                "stream_stats_updated_at",
                "updated_at",
            ],
        )
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO stream (id, name, url, logo_url, tvg_id, channel_group_id,
                                     m3u_account_id, stream_profile_id, is_custom,
                                     is_adult, stream_id, stream_chno, stream_hash,
                                     last_seen, is_stale, is_catchup, catchup_days,
                                     custom_properties, stream_stats,
                                     stream_stats_updated_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET
                     name = excluded.name, url = excluded.url, logo_url = excluded.logo_url,
                     tvg_id = excluded.tvg_id, channel_group_id = excluded.channel_group_id,
                     m3u_account_id = excluded.m3u_account_id,
                     stream_profile_id = excluded.stream_profile_id,
                     is_custom = excluded.is_custom, is_adult = excluded.is_adult,
                     stream_id = excluded.stream_id, stream_chno = excluded.stream_chno,
                     stream_hash = excluded.stream_hash, last_seen = excluded.last_seen,
                     is_stale = excluded.is_stale, is_catchup = excluded.is_catchup,
                     catchup_days = excluded.catchup_days,
                     custom_properties = excluded.custom_properties,
                     stream_stats = excluded.stream_stats,
                     stream_stats_updated_at = excluded.stream_stats_updated_at,
                     updated_at = excluded.updated_at",
            )
            .bind(row.get::<i64>("id")?)
            .bind(row.get::<String>("name")?)
            .bind(row.get::<Option<String>>("url")?)
            .bind(row.get::<Option<String>>("logo_url")?)
            .bind(row.get::<Option<String>>("tvg_id")?)
            .bind(row.get::<Option<i64>>("channel_group_id")?)
            .bind(row.get::<Option<i64>>("m3u_account_id")?)
            .bind(row.get::<Option<i64>>("stream_profile_id")?)
            .bind(row.get::<bool>("is_custom")? as i64)
            .bind(row.get::<bool>("is_adult")? as i64)
            .bind(row.get::<Option<i32>>("stream_id")?.map(i64::from))
            .bind(row.get::<Option<f64>>("stream_chno")?)
            .bind(row.get::<Option<String>>("stream_hash")?)
            .bind(sql_timestamp(row.get::<DateTime<Utc>>("last_seen")?))
            .bind(row.get::<bool>("is_stale")? as i64)
            .bind(row.get::<bool>("is_catchup")? as i64)
            .bind(i64::from(row.get::<i32>("catchup_days")?))
            .bind(json_of(row, "custom_properties"))
            .bind(
                row.get::<Option<Value>>("stream_stats")
                    .ok()
                    .flatten()
                    .map(|v| v.to_string()),
            )
            .bind(
                row.get::<Option<DateTime<Utc>>>("stream_stats_updated_at")?
                    .map(sql_timestamp),
            )
            .bind(sql_timestamp(row.get::<DateTime<Utc>>("updated_at")?))
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("stream row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("stream", written);
    Ok(())
}

async fn import_channel_profiles(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows("dispatcharr_channels_channelprofile", &["id", "name"])
        .await?;

    for row in &rows {
        let name: String = row.get("name")?;
        match sqlx::query(
            "INSERT INTO channel_profile (id, name) VALUES (?, ?)
             ON CONFLICT (id) DO UPDATE SET name = excluded.name",
        )
        .bind(row.get::<i64>("id")?)
        .bind(&name)
        .execute(sqlite)
        .await
        {
            Ok(_) => report.add("channel_profile", 1),
            Err(e) => report.warn(format!("channel profile `{name}` not imported: {e}")),
        }
    }
    Ok(())
}

async fn import_channels(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "dispatcharr_channels_channel",
            &[
                "id",
                "uuid",
                "channel_number",
                "name",
                "logo_id",
                "channel_group_id",
                "tvg_id",
                "tvc_guide_stationid",
                "epg_data_id",
                "stream_profile_id",
                "user_level",
                "is_adult",
                "hidden_from_output",
                "auto_created",
                "is_catchup",
                "catchup_days",
                "created_at",
                "updated_at",
            ],
        )
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO channel (id, uuid, channel_number, name, logo_id,
                                      channel_group_id, tvg_id, tvc_guide_stationid,
                                      epg_data_id, stream_profile_id, user_level,
                                      is_adult, hidden_from_output, auto_created,
                                      is_catchup, catchup_days, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET
                     uuid = excluded.uuid, channel_number = excluded.channel_number,
                     name = excluded.name, logo_id = excluded.logo_id,
                     channel_group_id = excluded.channel_group_id, tvg_id = excluded.tvg_id,
                     tvc_guide_stationid = excluded.tvc_guide_stationid,
                     epg_data_id = excluded.epg_data_id,
                     stream_profile_id = excluded.stream_profile_id,
                     user_level = excluded.user_level, is_adult = excluded.is_adult,
                     hidden_from_output = excluded.hidden_from_output,
                     auto_created = excluded.auto_created, is_catchup = excluded.is_catchup,
                     catchup_days = excluded.catchup_days, created_at = excluded.created_at,
                     updated_at = excluded.updated_at",
            )
            .bind(row.get::<i64>("id")?)
            .bind(row.get::<Uuid>("uuid")?.hyphenated().to_string())
            .bind(row.get::<Option<f64>>("channel_number")?)
            .bind(row.get::<String>("name")?)
            .bind(row.get::<Option<i64>>("logo_id")?)
            .bind(row.get::<Option<i64>>("channel_group_id")?)
            .bind(row.get::<Option<String>>("tvg_id")?)
            .bind(row.get::<Option<String>>("tvc_guide_stationid")?)
            .bind(row.get::<Option<i64>>("epg_data_id")?)
            .bind(row.get::<Option<i64>>("stream_profile_id")?)
            .bind(i64::from(row.get::<i32>("user_level")?))
            .bind(row.get::<bool>("is_adult")? as i64)
            .bind(row.get::<bool>("hidden_from_output")? as i64)
            .bind(row.get::<bool>("auto_created")? as i64)
            .bind(row.get::<bool>("is_catchup")? as i64)
            .bind(i64::from(row.get::<i32>("catchup_days")?))
            .bind(sql_timestamp(row.get::<DateTime<Utc>>("created_at")?))
            .bind(sql_timestamp(row.get::<DateTime<Utc>>("updated_at")?))
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("channel row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("channel", written);
    Ok(())
}

async fn import_channel_overrides(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "dispatcharr_channels_channeloverride",
            &[
                "channel_id",
                "name",
                "channel_number",
                "channel_group_id",
                "logo_id",
                "tvg_id",
                "tvc_guide_stationid",
                "epg_data_id",
                "stream_profile_id",
                "created_at",
                "updated_at",
            ],
        )
        .await?;

    for row in &rows {
        let channel_id: i64 = row.get("channel_id")?;
        let result = sqlx::query(
            "INSERT INTO channel_override (channel_id, name, channel_number,
                                           channel_group_id, logo_id, tvg_id,
                                           tvc_guide_stationid, epg_data_id,
                                           stream_profile_id, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (channel_id) DO UPDATE SET
                 name = excluded.name, channel_number = excluded.channel_number,
                 channel_group_id = excluded.channel_group_id, logo_id = excluded.logo_id,
                 tvg_id = excluded.tvg_id,
                 tvc_guide_stationid = excluded.tvc_guide_stationid,
                 epg_data_id = excluded.epg_data_id,
                 stream_profile_id = excluded.stream_profile_id,
                 created_at = excluded.created_at, updated_at = excluded.updated_at",
        )
        .bind(channel_id)
        .bind(row.get::<Option<String>>("name")?)
        .bind(row.get::<Option<f64>>("channel_number")?)
        .bind(row.get::<Option<i64>>("channel_group_id")?)
        .bind(row.get::<Option<i64>>("logo_id")?)
        .bind(row.get::<Option<String>>("tvg_id")?)
        .bind(row.get::<Option<String>>("tvc_guide_stationid")?)
        .bind(row.get::<Option<i64>>("epg_data_id")?)
        .bind(row.get::<Option<i64>>("stream_profile_id")?)
        .bind(sql_timestamp(row.get::<DateTime<Utc>>("created_at")?))
        .bind(sql_timestamp(row.get::<DateTime<Utc>>("updated_at")?))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("channel_override", 1),
            Err(e) => report.warn(format!(
                "override for channel {channel_id} not imported: {e}"
            )),
        }
    }
    Ok(())
}

async fn import_channel_streams(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "dispatcharr_channels_channelstream",
            &["channel_id", "stream_id", "order"],
        )
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO channel_stream (channel_id, stream_id, sort_order)
                 VALUES (?, ?, ?)
                 ON CONFLICT (channel_id, stream_id)
                 DO UPDATE SET sort_order = excluded.sort_order",
            )
            .bind(row.get::<i64>("channel_id")?)
            .bind(row.get::<i64>("stream_id")?)
            .bind(i64::from(row.get::<i32>("order")?))
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("channel_stream row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("channel_stream", written);
    Ok(())
}

async fn import_memberships(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "dispatcharr_channels_channelprofilemembership",
            &["channel_profile_id", "channel_id", "enabled"],
        )
        .await?;

    let mut written = 0;
    for chunk in rows.chunks(BATCH) {
        let mut tx = sqlite.begin().await?;
        for row in chunk {
            let result = sqlx::query(
                "INSERT INTO channel_profile_membership (channel_profile_id, channel_id, enabled)
                 VALUES (?, ?, ?)
                 ON CONFLICT (channel_profile_id, channel_id)
                 DO UPDATE SET enabled = excluded.enabled",
            )
            .bind(row.get::<i64>("channel_profile_id")?)
            .bind(row.get::<i64>("channel_id")?)
            .bind(row.get::<bool>("enabled")? as i64)
            .execute(&mut *tx)
            .await;
            match result {
                Ok(_) => written += 1,
                Err(e) => report.warn(format!("channel_profile_membership row not imported: {e}")),
            }
        }
        tx.commit().await?;
    }
    report.add("channel_profile_membership", written);
    Ok(())
}

async fn import_users(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source
        .rows(
            "accounts_user",
            &[
                "id",
                "username",
                "email",
                "password",
                "is_active",
                "user_level",
                "api_key",
                "stream_limit",
                "custom_properties",
                "avatar_config",
                "first_name",
                "last_name",
                "last_login",
                "date_joined",
                "is_superuser",
            ],
        )
        .await?;

    for row in &rows {
        let username: String = row.get("username")?;
        let password: String = row.get("password")?;

        // A hash this build cannot verify means that account can never log in,
        // and the operator needs to know before cutover, not after.
        if !dollet_core::auth::password::is_supported(&password) {
            report.warn(format!(
                "user `{username}` has a password hash this build cannot verify; \
                 set a new password after import"
            ));
        }

        // Django keeps `is_superuser` separate from `user_level`; here the
        // level is the only source of truth, so a superuser must land as admin.
        let level: i64 = if row.get::<bool>("is_superuser")? {
            10
        } else {
            row.get::<i32>("user_level")?.into()
        };

        let email: String = row.get("email")?;
        let result = sqlx::query(
            "INSERT INTO user (id, username, email, password, is_active, user_level, api_key,
                               stream_limit, custom_properties, avatar_config, first_name,
                               last_name, last_login, date_joined)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                 username = excluded.username,
                 email = excluded.email,
                 password = excluded.password,
                 is_active = excluded.is_active,
                 user_level = excluded.user_level,
                 api_key = excluded.api_key,
                 stream_limit = excluded.stream_limit,
                 custom_properties = excluded.custom_properties",
        )
        .bind(row.get::<i64>("id")?)
        .bind(&username)
        .bind((!email.is_empty()).then_some(email))
        .bind(&password)
        .bind(row.get::<bool>("is_active")? as i64)
        .bind(level)
        .bind(row.get::<Option<String>>("api_key")?)
        .bind(i64::from(row.get::<i32>("stream_limit")?))
        .bind(json_of(row, "custom_properties"))
        .bind(json_of(row, "avatar_config"))
        .bind(row.get::<String>("first_name")?)
        .bind(row.get::<String>("last_name")?)
        .bind(
            row.get::<Option<DateTime<Utc>>>("last_login")?
                .map(sql_timestamp),
        )
        .bind(sql_timestamp(row.get::<DateTime<Utc>>("date_joined")?))
        .execute(sqlite)
        .await;

        match result {
            Ok(_) => report.add("user", 1),
            Err(e) => report.warn(format!("user `{username}` not imported: {e}")),
        }
    }

    let links = source
        .rows(
            "accounts_user_channel_profiles",
            &["user_id", "channelprofile_id"],
        )
        .await?;

    for row in &links {
        sqlx::query(
            "INSERT OR IGNORE INTO user_channel_profile (user_id, channel_profile_id)
             VALUES (?, ?)",
        )
        .bind(row.get::<i64>("user_id")?)
        .bind(row.get::<i64>("channelprofile_id")?)
        .execute(sqlite)
        .await?;
        report.add("user_channel_profile", 1);
    }
    Ok(())
}

async fn import_settings(
    source: &Source,
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let rows = source.rows("core_coresettings", &["key", "value"]).await?;

    for row in &rows {
        let key: String = row.get("key")?;
        let mut value: Value = row.get("value").unwrap_or_else(|_| json!({}));

        // Dispatcharr stores chunk retention as a Redis TTL, defaulting to 90
        // seconds, which is ~90 MB per channel at 8 Mbps and would defeat the
        // memory budget this project exists for. The ring window is left at
        // its own default.
        if key == "proxy_settings"
            && let Some(ttl) = value.get("redis_chunk_ttl").and_then(Value::as_i64)
        {
            report.warn(format!(
                "proxy retention of {ttl}s not carried over; the ring keeps 15s by default, \
                 tunable as proxy_settings.ring_seconds"
            ));
            if let Value::Object(map) = &mut value {
                map.remove("redis_chunk_ttl");
            }
        }

        // Same key, different meaning: the source schedules by frequency and
        // time of day, this build by an interval, and none of the fields
        // carry over. Patched, it would be counted as imported while changing
        // nothing; skipped, the instance keeps this build's default schedule.
        if key == <settings::BackupSettings as settings::Group>::KEY {
            tracing::debug!(key, "settings group not carried over");
            continue;
        }

        match settings::patch_by_key(sqlite, &key, &value).await {
            Ok(_) => report.add("core_setting", 1),
            Err(dollet_core::Error::NotFound) => {
                // dvr_settings backs a feature that is out of scope. Skipping
                // it is a documented gap, not a failure.
                tracing::debug!(key, "settings group not in scope");
            }
            Err(e) => report.warn(format!("settings group `{key}` not imported: {e}")),
        }
    }

    check_settings_references(sqlite, report).await?;
    Ok(())
}

/// Settings carry ids into other tables, and nothing in the JSON enforces that
/// they exist. A dangling `default_stream_profile` is not a parse error — it is
/// every channel silently falling back at the first play.
async fn check_settings_references(
    sqlite: &SqlitePool,
    report: &mut ImportReport,
) -> anyhow::Result<()> {
    let mut stream: settings::StreamSettings = settings::load(sqlite).await?;
    let mut repaired = false;

    if let Some(id) = stream.default_user_agent
        && !exists(sqlite, "user_agent", id).await?
    {
        report.warn(format!("default user agent {id} does not exist; cleared"));
        stream.default_user_agent = None;
        repaired = true;
    }

    if let Some(id) = stream.hdhr_output_profile_id
        && !exists(sqlite, "output_profile", id).await?
    {
        report.warn(format!("HDHR output profile {id} does not exist; cleared"));
        stream.hdhr_output_profile_id = None;
        repaired = true;
    }

    match stream.default_stream_profile {
        Some(id) if !exists(sqlite, "stream_profile", id).await? => {
            report.warn(format!(
                "default stream profile {id} does not exist; cleared"
            ));
            stream.default_stream_profile = None;
            repaired = true;
        }
        Some(id) => {
            let command: String =
                sqlx::query_scalar("SELECT command FROM stream_profile WHERE id = ?")
                    .bind(id)
                    .fetch_one(sqlite)
                    .await?;
            // A carried-over profile may spawn an ffmpeg per channel, which
            // undoes the memory budget this project exists for. Say so rather
            // than let it be discovered by `podman stats`.
            if !command.is_empty() {
                report.warn(format!(
                    "default stream profile runs `{command}`, so every channel spawns a \
                     subprocess; the `proxy` profile relays bytes directly and is what \
                     this build ships as its default"
                ));
            }
        }
        None => {}
    }

    if repaired {
        settings::save(sqlite, &stream).await?;
    }
    Ok(())
}

async fn exists(sqlite: &SqlitePool, table: &str, id: i64) -> anyhow::Result<bool> {
    // `table` is a literal at every call site, never user input.
    let found: Option<i64> = sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE id = ?"))
        .bind(id)
        .fetch_optional(sqlite)
        .await?;
    Ok(found.is_some())
}

/// Import a Dispatcharr backup at boot, when `DOLLET_IMPORT_BACKUP` names one.
///
/// The gate is **the `user` table being empty** — not the file existing, and
/// not the catalogue being empty:
///
/// - after a successful import there are users, so every later restart skips
///   this and the variable can be left in the compose file;
/// - a fresh install whose owner has already created the first admin has a
///   user, so a stray variable can never overwrite a real instance;
/// - an earlier import that died partway has none, because users are written
///   near the end, so the next boot retries — which the upserts make safe.
///
/// Set with users present is one `info` line. Set with no users and a file that
/// is missing, unreadable or unparseable is an **error**, which stops `serve`:
/// the operator asked for a migration and did not get one, and serving them an
/// empty instance instead is the confusing failure. A migration that runs and
/// reports uncompilable patterns is *not* an error here, where it is one for
/// the CLI: the rows are imported, the filters they belong to match nothing
/// until they are fixed in Settings, and this process's job is to serve.
pub async fn maybe_import_on_boot(db: &SqlitePool, config: &Config) -> anyhow::Result<()> {
    let Some(backup) = config.import_backup.as_deref() else {
        return Ok(());
    };

    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user")
        .fetch_one(db)
        .await
        .context("counting users")?;
    if users > 0 {
        tracing::info!(
            backup = %backup.display(),
            "DOLLET_IMPORT_BACKUP is set but this instance already has users, so it is ignored"
        );
        return Ok(());
    }

    tracing::info!(backup = %backup.display(), "importing a Dispatcharr backup");
    let report = run(Source::backup(backup).await?, db).await?;

    let path = config.data_dir.join("import-report.json");
    let json = serde_json::to_string_pretty(&report).context("serializing the import report")?;
    tokio::fs::write(&path, json)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    tracing::info!(path = %path.display(), "import report written");

    log_report(&report);
    Ok(())
}

/// The report as log lines. Shared so the boot import and the CLI say the same
/// things about the same run; only what they do afterwards differs.
pub fn log_report(report: &ImportReport) {
    for (table, count) in &report.counts {
        tracing::info!(table, count, "imported");
    }
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    for rewritten in &report.regex_rewritten {
        tracing::warn!(?rewritten, "pattern rewritten for the Rust regex dialect");
    }
    for failure in &report.regex_failures {
        tracing::error!(?failure, "imported, but the pattern will not compile");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookaround_compiles_and_is_not_reported() {
        let mut report = ImportReport::default();
        check_pattern(&mut report, "test", r"^(?=.*HD)(.*)$");
        assert!(report.regex_failures.is_empty());
        assert!(report.regex_rewritten.is_empty());
    }

    #[test]
    fn an_uncompilable_pattern_is_reported_with_its_source() {
        let mut report = ImportReport::default();
        check_pattern(&mut report, "m3u filter 3", "(unclosed");

        assert_eq!(report.regex_failures.len(), 1);
        assert_eq!(report.regex_failures[0].source, "m3u filter 3");
        assert_eq!(report.regex_failures[0].pattern, "(unclosed");
    }

    #[test]
    fn a_rewritten_pattern_is_reported_even_when_it_compiles() {
        let mut report = ImportReport::default();
        check_pattern(&mut report, "m3u profile `x` search", "(a)$1");

        assert!(report.regex_failures.is_empty());
        assert_eq!(report.regex_rewritten.len(), 1);
        assert!(report.regex_rewritten[0].detail.contains(r"(a)\1"));
    }

    #[test]
    fn account_types_map_to_the_new_spelling() {
        assert_eq!(account_type("XC"), "xtream_codes");
        assert_eq!(account_type("STD"), "standard");
        assert_eq!(account_type("anything else"), "standard");
    }
}
