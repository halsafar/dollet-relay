//! Backups of this instance.
//!
//! A backup is one zip holding exactly two entries: `dollet.sqlite`, a
//! consistent snapshot of the database, and `backup.json`, which says which
//! build wrote it, when, why, and at which migration. The database is the whole
//! instance — the cache beside it is rebuilt on demand — so nothing else needs
//! to travel with it.
//!
//! The snapshot is `VACUUM INTO`, never a file copy. This process always has
//! writers, and a copy of a WAL database taken while one commits is torn: the
//! main file and the WAL disagree about which pages are current. `VACUUM INTO`
//! reads through a single snapshot and writes a compacted, self-contained file
//! without stopping anyone.
//!
//! Every file here is streamed. A database is the one thing in this project
//! that can be hundreds of megabytes, and reading one into memory to zip or
//! unzip it would spend the budget the rest of the design exists to keep.

use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, SubsecRound, Timelike, Utc};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{ConnectOptions, Connection, SqlitePool};
use zip::ZipArchive;
use zip::write::SimpleFileOptions;

use crate::{Error, Result};

const DATABASE_ENTRY: &str = "dollet.sqlite";
const METADATA_ENTRY: &str = "backup.json";
const NAME_PREFIX: &str = "dollet-backup-";
const STAMP_FORMAT: &str = "%Y%m%d-%H%M%S";
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Prefix of every file this module writes before it is finished. Nothing
/// else in the data directory uses it, which is what lets [`sweep`] remove
/// these without asking what they were.
const SCRATCH_PREFIX: &str = ".tmp-";

/// `backup.json` is a handful of fields. A cap, because the entry's declared
/// size is the archive's claim and an uploaded archive is not ours.
const METADATA_LIMIT: u64 = 64 * 1024;

/// Why a backup exists, which decides whether retention may delete it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    Manual,
    Scheduled,
    Uploaded,
    PreRestore,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
            Self::Uploaded => "uploaded",
            Self::PreRestore => "pre-restore",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "manual" => Self::Manual,
            "scheduled" => Self::Scheduled,
            "uploaded" => Self::Uploaded,
            "pre-restore" => Self::PreRestore,
            _ => return None,
        })
    }
}

/// A backup's file name, which is also how the API addresses it:
/// `dollet-backup-YYYYMMDD-HHMMSS-<trigger>.zip`.
///
/// Always derived here and parsed strictly, so a name that arrives in a
/// request is never joined onto a path until it has been shown to be a fixed
/// prefix, fourteen digits, a known trigger and `.zip` — nothing that can name
/// a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupName {
    created_at: DateTime<Utc>,
    trigger: Trigger,
}

impl BackupName {
    /// To the whole second, because that is all the name can carry.
    pub fn new(created_at: DateTime<Utc>, trigger: Trigger) -> Self {
        Self {
            created_at: created_at.trunc_subsecs(0),
            trigger,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        let rest = name.strip_prefix(NAME_PREFIX)?.strip_suffix(".zip")?;
        let stamp = rest.get(..15)?;
        let trigger = Trigger::parse(rest.get(15..)?.strip_prefix('-')?)?;

        // chrono skips whitespace before a number and reads a leap second,
        // neither of which this program ever writes. The leap second renders
        // back as `60`, so it is refused here; everything else is caught by
        // the round trip, so exactly one spelling of each backup parses.
        let created_at = NaiveDateTime::parse_from_str(stamp, STAMP_FORMAT).ok()?;
        if created_at.nanosecond() >= 1_000_000_000 {
            return None;
        }

        let parsed = Self::new(created_at.and_utc(), trigger);
        (parsed.to_string() == name).then_some(parsed)
    }

    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn trigger(&self) -> Trigger {
        self.trigger
    }

    fn next_second(self) -> Self {
        Self {
            created_at: self.created_at + chrono::Duration::seconds(1),
            ..self
        }
    }
}

impl fmt::Display for BackupName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{NAME_PREFIX}{}-{}.zip",
            self.created_at.format(STAMP_FORMAT),
            self.trigger.as_str()
        )
    }
}

/// The file a request names, if it names one of ours.
///
/// The parse is the guard. The parent check is a second one, the same kind
/// `artwork::cache_path` keeps, so a later change to the name format cannot
/// quietly leave the parse standing alone.
pub fn resolve(dir: &Path, requested: &str) -> Option<PathBuf> {
    let name = BackupName::parse(requested)?;
    let path = dir.join(name.to_string());
    (path.parent() == Some(dir)).then_some(path)
}

/// `backup.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    /// The build that wrote it.
    pub version: String,
    pub created_at: DateTime<Utc>,
    pub trigger: Trigger,
    /// The highest migration applied to the database inside.
    pub schema: i64,
}

/// One backup, as the list shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// From the name rather than `backup.json`: an uploaded archive's metadata
    /// records how it was first made, and the name records how it came to be
    /// here.
    pub trigger: Trigger,
    pub size_bytes: u64,
    /// From `backup.json`, and null when it cannot be read, so a damaged
    /// archive is still listed and can be deleted.
    pub version: Option<String>,
    pub schema: Option<i64>,
}

/// A file being built in a directory this module owns, removed when dropped
/// unless it was moved into place.
///
/// So every exit from every operation here, a refused upload included, leaves
/// the directory as it found it. A process that dies mid-write leaves the file
/// behind instead, which is what [`sweep`] at boot is for.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    kept: bool,
}

impl Scratch {
    pub fn new(dir: &Path, extension: &str) -> Self {
        Self {
            path: dir.join(format!(
                "{SCRATCH_PREFIX}{}.{extension}",
                uuid::Uuid::new_v4()
            )),
            kept: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Atomic when `to` is in the same directory, which every caller arranges.
    pub fn persist(mut self, to: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, to)?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Remove what an interrupted backup, upload or restore left in `dir`.
///
/// Only at boot, when nothing can be mid-write. A snapshot is as large as the
/// database and these never parse as a backup name, so without this one crash
/// is that much disk nobody can see in the list or delete from it.
pub fn sweep(dir: &Path) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(SCRATCH_PREFIX)
            && entry.file_type()?.is_file()
        {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Write a new backup into `dir` and describe it.
///
/// Callers serialise: the name is the first free second at or after now, and
/// two writers choosing at once could choose the same one.
pub async fn take(pool: &SqlitePool, dir: &Path, trigger: Trigger) -> Result<Entry> {
    tokio::fs::create_dir_all(dir).await?;
    let schema = schema_version(pool).await?;

    let snapshot = Scratch::new(dir, "sqlite");
    let target = snapshot.path().to_str().ok_or_else(|| {
        Error::invalid(format!("{} is not a UTF-8 path", snapshot.path().display()))
    })?;
    // Bound rather than spliced into the SQL: the argument to `VACUUM INTO`
    // is an expression, so a parameter is accepted and no path needs quoting.
    // The target must not exist, which the scratch name guarantees.
    sqlx::query("VACUUM INTO ?")
        .bind(target)
        .execute(pool)
        .await?;

    // Two backups inside one second would share a name, and the rename below
    // replaces whatever it lands on.
    let mut name = BackupName::new(Utc::now(), trigger);
    while tokio::fs::try_exists(dir.join(name.to_string())).await? {
        name = name.next_second();
    }

    let metadata = Metadata {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: name.created_at(),
        trigger,
        schema,
    };
    let archive = Scratch::new(dir, "zip");
    let destination = dir.join(name.to_string());
    let written = metadata.clone();
    let size_bytes = blocking(move || {
        write_archive(snapshot.path(), &written, archive.path())?;
        drop(snapshot);
        let size = std::fs::metadata(archive.path())?.len();
        archive.persist(&destination)?;
        Ok(size)
    })
    .await?;

    Ok(Entry {
        name: name.to_string(),
        created_at: name.created_at(),
        trigger,
        size_bytes,
        version: Some(metadata.version),
        schema: Some(schema),
    })
}

async fn schema_version(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations")
            .fetch_one(pool)
            .await?,
    )
}

fn write_archive(database: &Path, metadata: &Metadata, target: &Path) -> Result<()> {
    let mut source = File::open(database)?;
    let size = source.metadata()?.len();

    let mut zip = zip::ZipWriter::new(File::create_new(target)?);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    zip.start_file(METADATA_ENTRY, options).map_err(zip_write)?;
    zip.write_all(&serde_json::to_vec_pretty(metadata)?)?;

    // ZIP64 only past the 4 GiB a plain entry can describe, so an ordinary
    // backup still opens in every unzip tool an operator might reach for.
    zip.start_file(
        DATABASE_ENTRY,
        options.large_file(size >= u64::from(u32::MAX)),
    )
    .map_err(zip_write)?;
    std::io::copy(&mut source, &mut zip)?;

    zip.finish().map_err(zip_write)?.sync_all()?;
    Ok(())
}

fn zip_write(e: zip::result::ZipError) -> Error {
    Error::Other(anyhow::anyhow!("writing the backup archive: {e}"))
}

/// Every backup in `dir`, newest first.
pub async fn list(dir: &Path) -> Result<Vec<Entry>> {
    let dir = dir.to_owned();
    blocking(move || {
        let mut entries: Vec<Entry> = names(&dir)?
            .into_iter()
            .filter_map(|name| describe(&dir, name).ok())
            .collect();
        entries.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.name.cmp(&a.name))
        });
        Ok(entries)
    })
    .await
}

/// One backup, read from disk.
pub async fn entry(dir: &Path, name: BackupName) -> Result<Entry> {
    let dir = dir.to_owned();
    blocking(move || Ok(describe(&dir, name)?)).await
}

fn describe(dir: &Path, name: BackupName) -> std::io::Result<Entry> {
    let path = dir.join(name.to_string());
    let size_bytes = std::fs::metadata(&path)?.len();
    let metadata = File::open(&path)
        .ok()
        .and_then(|file| ZipArchive::new(BufReader::new(file)).ok())
        .and_then(|mut zip| read_metadata(&mut zip).ok());

    Ok(Entry {
        name: name.to_string(),
        created_at: name.created_at(),
        trigger: name.trigger(),
        size_bytes,
        version: metadata.as_ref().map(|m| m.version.clone()),
        schema: metadata.map(|m| m.schema),
    })
}

/// The names in `dir` that are backups. Anything else is not listed and not
/// touched.
fn names(dir: &Path) -> std::io::Result<Vec<BackupName>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for entry in entries {
        if let Some(name) = entry?.file_name().to_str().and_then(BackupName::parse) {
            names.push(name);
        }
    }
    Ok(names)
}

/// Delete the oldest scheduled backups beyond `keep`, and say which.
///
/// Scheduled ones only. Retention exists to bound what the timer writes; a
/// backup somebody made on purpose — by hand, by upload, or as the safety net
/// before a restore — is theirs to delete, and counting it here would let a
/// timer remove it.
pub async fn prune(dir: &Path, keep: usize) -> Result<Vec<String>> {
    let dir = dir.to_owned();
    blocking(move || {
        let mut scheduled: Vec<BackupName> = names(&dir)?
            .into_iter()
            .filter(|name| name.trigger() == Trigger::Scheduled)
            .collect();
        scheduled.sort_by_key(|name| std::cmp::Reverse(name.created_at()));

        let mut removed = Vec::new();
        for name in scheduled.into_iter().skip(keep) {
            std::fs::remove_file(dir.join(name.to_string()))?;
            removed.push(name.to_string());
        }
        Ok(removed)
    })
    .await
}

/// A backup that has been opened and checked, with its database extracted.
#[derive(Debug)]
pub struct Validated {
    pub metadata: Metadata,
    /// Removed when dropped, unless a restore moves it into place.
    pub database: Scratch,
}

/// Open a backup and check that this build could restore it.
///
/// The database is extracted into `scratch_dir`, which a restore points at the
/// data directory so the staged file can be renamed into place rather than
/// copied across filesystems. Every refusal is [`Error::Invalid`] and names
/// its reason, because the person reading it has a file in their hand and
/// needs to know what is wrong with it.
pub async fn validate(archive: &Path, scratch_dir: &Path) -> Result<Validated> {
    let archive = archive.to_owned();
    let database = Scratch::new(scratch_dir, "sqlite");
    let (metadata, database) = blocking(move || extract(&archive, database)).await?;
    check_database(database.path()).await?;
    Ok(Validated { metadata, database })
}

fn refuse(reason: impl fmt::Display) -> Error {
    Error::Invalid(format!("not a usable backup: {reason}"))
}

fn extract(archive: &Path, database: Scratch) -> Result<(Metadata, Scratch)> {
    let mut zip = ZipArchive::new(BufReader::new(File::open(archive)?))
        .map_err(|e| refuse(format!("it is not a zip archive ({e})")))?;

    // Entries are looked up by these two fixed names and never used as a
    // path, so an archive naming `../../etc/passwd` is refused here rather
    // than extracted anywhere.
    let mut found: Vec<&str> = zip.file_names().collect();
    found.sort_unstable();
    if found != [METADATA_ENTRY, DATABASE_ENTRY] {
        return Err(refuse(format!(
            "a backup holds exactly `{METADATA_ENTRY}` and `{DATABASE_ENTRY}`, and this holds {}",
            if found.is_empty() {
                "nothing".to_owned()
            } else {
                found
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )));
    }

    let metadata = read_metadata(&mut zip)
        .map_err(|e| refuse(format!("`{METADATA_ENTRY}` is unreadable ({e})")))?;

    let mut entry = zip
        .by_name(DATABASE_ENTRY)
        .map_err(|e| refuse(format!("`{DATABASE_ENTRY}` is unreadable ({e})")))?;

    // Checked before anything is written, so a large entry that is not a
    // database costs no disk.
    let mut header = [0u8; 16];
    if entry.read_exact(&mut header).is_err() || &header != SQLITE_MAGIC {
        return Err(refuse(format!(
            "`{DATABASE_ENTRY}` is not a SQLite database"
        )));
    }

    let mut out = File::create_new(database.path())?;
    out.write_all(&header)?;
    std::io::copy(&mut entry, &mut out)
        .map_err(|e| refuse(format!("`{DATABASE_ENTRY}` could not be extracted ({e})")))?;
    out.sync_all()?;

    Ok((metadata, database))
}

fn read_metadata<R: Read + Seek>(zip: &mut ZipArchive<R>) -> std::result::Result<Metadata, String> {
    let entry = zip.by_name(METADATA_ENTRY).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    entry
        .take(METADATA_LIMIT)
        .read_to_end(&mut raw)
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&raw).map_err(|e| e.to_string())
}

async fn check_database(path: &Path) -> Result<()> {
    // Immutable as well as read-only: the extracted copy is this process's
    // alone, and a file written by a WAL-mode database says so in its header,
    // which a read-only open would otherwise answer by looking for a `-shm`
    // it may not create.
    let mut connection = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .immutable(true)
        .connect()
        .await
        .map_err(|e| refuse(format!("`{DATABASE_ENTRY}` will not open ({e})")))?;
    let outcome = inspect(&mut connection).await;
    let _ = connection.close().await;
    outcome
}

async fn inspect(connection: &mut SqliteConnection) -> Result<()> {
    let integrity = |detail: String| {
        refuse(format!(
            "`{DATABASE_ENTRY}` fails SQLite's integrity check ({detail})"
        ))
    };
    let verdict: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut *connection)
        .await
        .map_err(|e| integrity(e.to_string()))?;
    if verdict != ["ok"] {
        return Err(integrity(
            verdict.into_iter().take(3).collect::<Vec<_>>().join("; "),
        ));
    }

    let tracked: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master
                        WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if !tracked {
        return Err(refuse(format!(
            "`{DATABASE_ENTRY}` has no migration history, so it is not a database this \
             program wrote"
        )));
    }

    let applied: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&mut *connection)
            .await?;

    // An older schema is fine: the boot that applies the restore migrates it
    // forward. A newer one would fail that boot, after the database it
    // replaced is already gone.
    let known = crate::db::MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0);
    if let Some(newest) = applied.iter().map(|(version, ..)| *version).max()
        && newest > known
    {
        return Err(refuse(format!(
            "it was made by a newer build, at migration {newest}, and this build knows \
             migrations up to {known}; upgrade, then restore it"
        )));
    }

    // The same checks the migrator makes at boot, made here instead, where a
    // refusal costs nothing.
    for (version, checksum, success) in applied {
        let Some(migration) = crate::db::MIGRATOR
            .iter()
            .find(|migration| migration.version == version)
        else {
            return Err(refuse(format!(
                "its migration {version} is not one this build has"
            )));
        };
        if *migration.checksum != *checksum {
            return Err(refuse(format!(
                "its migration {version} differs from this build's, which would stop the \
                 server at boot"
            )));
        }
        if !success {
            return Err(refuse(format!(
                "its migration {version} was left half-applied"
            )));
        }
    }
    Ok(())
}

/// Move a staged restore over `database`. Called at boot, before anything
/// opens it.
///
/// The old WAL and shared-memory files go first. Left beside the new
/// database, SQLite would replay the old instance's pages onto it at the next
/// open: corruption that looks like a successful restore. A crash between the
/// two steps leaves the old database short of its WAL and the restore still
/// staged, and the next boot finishes the job; the pre-restore backup already
/// holds everything that WAL did.
pub fn apply_staged(staged: &Path, database: &Path) -> std::io::Result<()> {
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = database.as_os_str().to_owned();
        sidecar.push(suffix);
        match std::fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    std::fs::rename(staged, database)
}

/// Run file work off the async workers, which stall every stream on this
/// process while they wait on a disk.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("backup task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn source(dir: &Path) -> SqlitePool {
        let pool = crate::db::connect(&dir.join("source.sqlite"))
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    fn at(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc)
    }

    /// An archive with exactly these entries, for the shapes `take` never
    /// writes.
    fn archive_of(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    fn metadata_json() -> Vec<u8> {
        serde_json::to_vec(&Metadata {
            version: "0.0.0".into(),
            created_at: at("2026-10-01T12:00:00Z"),
            trigger: Trigger::Manual,
            schema: 1,
        })
        .unwrap()
    }

    /// A real backup's database, copied out where a test can change it.
    async fn database_from_a_backup(dir: &Path) -> PathBuf {
        let pool = source(dir).await;
        let backups = dir.join("backups");
        let entry = take(&pool, &backups, Trigger::Manual).await.unwrap();
        let checked = validate(&backups.join(&entry.name), dir).await.unwrap();
        let copy = dir.join("copy.sqlite");
        std::fs::copy(checked.database.path(), &copy).unwrap();
        copy
    }

    async fn refusal(archive: &Path, scratch: &Path) -> String {
        match validate(archive, scratch).await {
            Err(Error::Invalid(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_name_round_trips_through_its_file_name() {
        for trigger in [
            Trigger::Manual,
            Trigger::Scheduled,
            Trigger::Uploaded,
            Trigger::PreRestore,
        ] {
            let name = BackupName::new(at("2026-10-01T09:08:07.654Z"), trigger);
            let rendered = name.to_string();
            assert_eq!(
                rendered,
                format!("dollet-backup-20261001-090807-{}.zip", trigger.as_str())
            );

            let parsed = BackupName::parse(&rendered).expect(&rendered);
            assert_eq!(parsed, name);
            // Whole seconds, because the name cannot carry more.
            assert_eq!(parsed.created_at(), at("2026-10-01T09:08:07Z"));
            assert_eq!(parsed.trigger(), trigger);
        }
    }

    #[test]
    fn anything_but_a_backup_name_is_refused() {
        for bad in [
            "",
            "dollet-backup-20261001-090807-manual",
            "dollet-backup-20261001-090807-manual.ZIP",
            "dollet-backup-20261001-090807-manual.zip.part",
            "dollet-backup-20261001-090807-weekly.zip",
            "dollet-backup-20261001-090807manual.zip",
            "dollet-backup-20261001_090807-manual.zip",
            "dollet-backup-2026101-0908070-manual.zip",
            "dollet-backup-2026100a-090807-manual.zip",
            "dollet-backup-+2026100-090807-manual.zip",
            "dollet-backup-20261301-090807-manual.zip",
            "dollet-backup-20261001-250807-manual.zip",
            "dollet-backup-20261001-235960-manual.zip",
            "dollet-backup-2026 101-090807-manual.zip",
            "Dollet-backup-20261001-090807-manual.zip",
            "../dollet-backup-20261001-090807-manual.zip",
            "dollet-backup-20261001-090807-manual.zip/..",
            "dollet-backup-20261001-090807-../manual.zip",
            "dollet-backup-20261001-090807-manual.zip\0",
            "dollet-backup-２0261001-090807-manual.zip",
            ".tmp-0.zip",
        ] {
            assert_eq!(BackupName::parse(bad), None, "{bad:?} parsed");
        }
    }

    #[test]
    fn a_resolved_name_never_leaves_its_directory() {
        let dir = Path::new("/data/backups");
        assert_eq!(
            resolve(dir, "dollet-backup-20261001-090807-manual.zip"),
            Some(dir.join("dollet-backup-20261001-090807-manual.zip"))
        );
        for bad in ["..", "../dollet.sqlite", "/etc/passwd", "dollet.sqlite"] {
            assert_eq!(resolve(dir, bad), None, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_backup_is_a_zip_of_the_database_and_what_made_it() {
        let dir = tempfile::tempdir().unwrap();
        let pool = source(dir.path()).await;
        sqlx::query("INSERT INTO channel_group (name) VALUES ('Taken Before The Backup')")
            .execute(&pool)
            .await
            .unwrap();
        let backups = dir.path().join("backups");

        let entry = take(&pool, &backups, Trigger::Manual).await.unwrap();
        let path = backups.join(&entry.name);
        assert_eq!(
            BackupName::parse(&entry.name).unwrap().trigger(),
            Trigger::Manual
        );
        assert_eq!(entry.size_bytes, std::fs::metadata(&path).unwrap().len());

        // Exactly two entries, compressed, and the metadata says who and when.
        let mut zip = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut names: Vec<&str> = zip.file_names().collect();
        names.sort_unstable();
        assert_eq!(names, ["backup.json", "dollet.sqlite"]);
        assert_eq!(
            zip.by_name(DATABASE_ENTRY).unwrap().compression(),
            zip::CompressionMethod::Deflated
        );
        let metadata = read_metadata(&mut zip).unwrap();
        let newest = crate::db::MIGRATOR.iter().map(|m| m.version).max().unwrap();
        assert_eq!(
            metadata,
            Metadata {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                created_at: entry.created_at,
                trigger: Trigger::Manual,
                schema: newest,
            }
        );

        // Only the finished archive is left behind.
        let left: Vec<String> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, std::slice::from_ref(&entry.name));

        // And it reads back as the database it was taken from.
        let checked = validate(&path, dir.path()).await.unwrap();
        assert_eq!(checked.metadata, metadata);
        let copy = SqlitePool::connect(&format!("sqlite://{}", checked.database.path().display()))
            .await
            .unwrap();
        let groups: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM channel_group WHERE name = 'Taken Before The Backup'",
        )
        .fetch_one(&copy)
        .await
        .unwrap();
        assert_eq!(groups, 1);
        copy.close().await;

        assert_eq!(list(&backups).await.unwrap(), [entry]);
    }

    #[tokio::test]
    async fn two_backups_inside_one_second_do_not_share_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let pool = source(dir.path()).await;
        let backups = dir.path().join("backups");

        let mut names = Vec::new();
        for _ in 0..3 {
            names.push(
                take(&pool, &backups, Trigger::Scheduled)
                    .await
                    .unwrap()
                    .name,
            );
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 3, "a backup replaced another: {names:?}");
        assert_eq!(list(&backups).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_list_is_newest_first_and_keeps_an_archive_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path();
        archive_of(
            &backups.join("dollet-backup-20261001-120000-scheduled.zip"),
            &[(METADATA_ENTRY, &metadata_json())],
        );
        std::fs::write(
            backups.join("dollet-backup-20261002-120000-uploaded.zip"),
            b"not a zip",
        )
        .unwrap();
        // A zip, but with no `backup.json` in it.
        archive_of(
            &backups.join("dollet-backup-20261001-120000-manual.zip"),
            &[(DATABASE_ENTRY, b"SQLite format 3\0")],
        );
        std::fs::write(backups.join("notes.txt"), b"not ours").unwrap();

        let listed = list(backups).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        // Two in one second are ordered by name, so the list does not reshuffle
        // between one load and the next.
        assert_eq!(
            names,
            [
                "dollet-backup-20261002-120000-uploaded.zip",
                "dollet-backup-20261001-120000-scheduled.zip",
                "dollet-backup-20261001-120000-manual.zip",
            ]
        );
        // Listed from its name, so it can still be deleted.
        assert_eq!(listed[0].trigger, Trigger::Uploaded);
        assert_eq!(listed[0].version, None);
        assert_eq!(listed[0].schema, None);
        assert_eq!(listed[1].version.as_deref(), Some("0.0.0"));
        assert_eq!(listed[1].schema, Some(1));
        assert_eq!(listed[2].version, None);
        assert_eq!(listed[2].schema, None);
    }

    #[tokio::test]
    async fn retention_deletes_only_the_oldest_scheduled_backups() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path();
        for name in [
            "dollet-backup-20261001-000000-scheduled.zip",
            "dollet-backup-20261002-000000-scheduled.zip",
            "dollet-backup-20261003-000000-scheduled.zip",
            "dollet-backup-20260901-000000-manual.zip",
            "dollet-backup-20260902-000000-uploaded.zip",
            "dollet-backup-20260903-000000-pre-restore.zip",
        ] {
            std::fs::write(backups.join(name), b"").unwrap();
        }

        let removed = prune(backups, 2).await.unwrap();
        assert_eq!(removed, ["dollet-backup-20261001-000000-scheduled.zip"]);

        let mut left: Vec<String> = list(backups)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "dollet-backup-20260901-000000-manual.zip",
                "dollet-backup-20260902-000000-uploaded.zip",
                "dollet-backup-20260903-000000-pre-restore.zip",
                "dollet-backup-20261002-000000-scheduled.zip",
                "dollet-backup-20261003-000000-scheduled.zip",
            ]
        );
    }

    #[tokio::test]
    async fn a_file_that_is_not_a_zip_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");
        std::fs::write(&path, b"SQLite format 3\0 but not zipped").unwrap();

        assert!(
            refusal(&path, dir.path())
                .await
                .contains("not a zip archive")
        );
    }

    #[tokio::test]
    async fn an_archive_without_both_entries_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");

        archive_of(&path, &[(METADATA_ENTRY, &metadata_json())]);
        let missing = refusal(&path, dir.path()).await;
        assert!(missing.contains("holds `backup.json`"), "{missing}");

        let database = std::fs::read(database_from_a_backup(dir.path()).await).unwrap();
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, &metadata_json()),
                (DATABASE_ENTRY, &database),
                ("../../escape", b"x"),
            ],
        );
        let extra = refusal(&path, dir.path()).await;
        assert!(extra.contains("`../../escape`"), "{extra}");
        assert!(!dir.path().parent().unwrap().join("escape").exists());

        archive_of(&path, &[]);
        assert!(
            refusal(&path, dir.path())
                .await
                .contains("this holds nothing")
        );
    }

    #[tokio::test]
    async fn a_database_entry_that_is_not_sqlite_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, &metadata_json()),
                (DATABASE_ENTRY, b"PRAGMA nothing; this is text"),
            ],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("is not a SQLite database"), "{refused}");
    }

    #[tokio::test]
    async fn unreadable_metadata_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let database = std::fs::read(database_from_a_backup(dir.path()).await).unwrap();
        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, b"{\"version\":"),
                (DATABASE_ENTRY, &database),
            ],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("`backup.json` is unreadable"), "{refused}");
    }

    #[tokio::test]
    async fn a_database_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let copy = database_from_a_backup(dir.path()).await;
        let pool = SqlitePool::connect(&format!("sqlite://{}", copy.display()))
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES (9999, 'from a later release', 1, x'00', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, &metadata_json()),
                (DATABASE_ENTRY, &std::fs::read(&copy).unwrap()),
            ],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("made by a newer build"), "{refused}");
        assert!(refused.contains("migration 9999"), "{refused}");
    }

    #[tokio::test]
    async fn a_migration_history_this_build_would_refuse_at_boot_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let copy = database_from_a_backup(dir.path()).await;
        let pool = SqlitePool::connect(&format!("sqlite://{}", copy.display()))
            .await
            .unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = x'00' WHERE version = 1")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, &metadata_json()),
                (DATABASE_ENTRY, &std::fs::read(&copy).unwrap()),
            ],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("migration 1 differs"), "{refused}");
    }

    #[tokio::test]
    async fn a_database_with_no_migration_history_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("foreign.sqlite");
        let pool = crate::db::connect(&foreign).await.unwrap();
        sqlx::query("CREATE TABLE somebody_else (id INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, &metadata_json()),
                (DATABASE_ENTRY, &std::fs::read(&foreign).unwrap()),
            ],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("no migration history"), "{refused}");
    }

    #[tokio::test]
    async fn a_database_with_a_corrupted_page_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let copy = database_from_a_backup(dir.path()).await;
        let mut bytes = std::fs::read(&copy).unwrap();

        // A page header in the middle of the file, well clear of the 100-byte
        // file header that the magic check reads.
        let page = 4096;
        let middle = (bytes.len() / 2) / page * page;
        assert!(middle > page, "the database is too small to corrupt a page");
        for byte in &mut bytes[middle..middle + 64] {
            *byte = !*byte;
        }

        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[(METADATA_ENTRY, &metadata_json()), (DATABASE_ENTRY, &bytes)],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("integrity check"), "{refused}");
    }

    /// SQLite reports some damage as an error, as the page above does, and
    /// some as a list of problems with every page still readable. This is the
    /// second kind.
    #[tokio::test]
    async fn an_index_that_disagrees_with_its_table_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let copy = database_from_a_backup(dir.path()).await;
        let pool = SqlitePool::connect(&format!("sqlite://{}", copy.display()))
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE marker (v TEXT);
             CREATE INDEX marker_v ON marker (v);
             INSERT INTO marker VALUES ('dollet-integrity-marker');",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        // The value is on disk twice, in the table and in its index; changing
        // the first copy leaves the two disagreeing.
        let mut bytes = std::fs::read(&copy).unwrap();
        let marker = b"dollet-integrity-marker";
        let at = bytes
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("the marker is not on disk");
        bytes[at] = b'D';

        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[(METADATA_ENTRY, &metadata_json()), (DATABASE_ENTRY, &bytes)],
        );

        let refused = refusal(&path, dir.path()).await;
        assert!(refused.contains("integrity check"), "{refused}");
        assert!(refused.contains("marker_v"), "{refused}");
    }

    /// A download cut short or a bit flipped on a disk: the archive still
    /// opens, and what is inside it no longer reads back.
    #[tokio::test]
    async fn an_archive_damaged_inside_an_entry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let pool = source(dir.path()).await;
        let backups = dir.path().join("backups");
        let entry = take(&pool, &backups, Trigger::Manual).await.unwrap();
        let original = std::fs::read(backups.join(&entry.name)).unwrap();

        for (damaged, reason) in [
            (METADATA_ENTRY, "`backup.json` is unreadable"),
            (DATABASE_ENTRY, "`dollet.sqlite` could not be extracted"),
        ] {
            let middle = {
                let mut zip = ZipArchive::new(std::io::Cursor::new(&original)).unwrap();
                let file = zip.by_name(damaged).unwrap();
                usize::try_from(file.data_start() + file.compressed_size() / 2).unwrap()
            };
            let mut bytes = original.clone();
            bytes[middle] = !bytes[middle];
            let path = dir.path().join("damaged.zip");
            std::fs::write(&path, &bytes).unwrap();

            let refused = refusal(&path, dir.path()).await;
            assert!(refused.contains(reason), "{damaged}: {refused}");
        }
    }

    /// SQLite is handed the snapshot's path as text, so a directory whose name
    /// is not text cannot hold a backup; the refusal says why rather than
    /// surfacing whatever SQLite makes of the bytes.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_backups_directory_whose_path_is_not_text_is_refused() {
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let pool = source(dir.path()).await;
        let backups = dir
            .path()
            .join(std::ffi::OsStr::from_bytes(b"backups-\xff"));

        let refused = take(&pool, &backups, Trigger::Manual).await;
        assert!(
            matches!(&refused, Err(Error::Invalid(message)) if message.contains("not a UTF-8 path")),
            "{refused:?}"
        );
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_refused_archive_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let path = dir.path().join("upload");
        archive_of(
            &path,
            &[
                (METADATA_ENTRY, &metadata_json()),
                (
                    DATABASE_ENTRY,
                    b"SQLite format 3\0and then nothing a database has",
                ),
            ],
        );

        refusal(&path, &scratch).await;
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
    }

    #[test]
    fn a_scratch_file_is_removed_unless_it_was_moved_into_place() {
        let dir = tempfile::tempdir().unwrap();

        let dropped = Scratch::new(dir.path(), "part");
        std::fs::write(dropped.path(), b"half").unwrap();
        let dropped_path = dropped.path().to_owned();
        drop(dropped);
        assert!(!dropped_path.exists());

        let kept = Scratch::new(dir.path(), "part");
        std::fs::write(kept.path(), b"whole").unwrap();
        let target = dir.path().join("kept");
        kept.persist(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"whole");
    }

    #[test]
    fn the_boot_sweep_removes_only_unfinished_files() {
        let dir = tempfile::tempdir().unwrap();
        let unfinished = Scratch::new(dir.path(), "sqlite");
        std::fs::write(unfinished.path(), b"interrupted").unwrap();
        let unfinished_path = unfinished.path().to_owned();
        // What a process killed mid-write leaves: the guard never runs.
        std::mem::forget(unfinished);
        std::fs::write(
            dir.path().join("dollet-backup-20261001-000000-manual.zip"),
            b"",
        )
        .unwrap();
        std::fs::write(dir.path().join("dollet.sqlite"), b"").unwrap();

        assert_eq!(sweep(dir.path()).unwrap(), 1);
        assert!(!unfinished_path.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        assert_eq!(sweep(&dir.path().join("absent")).unwrap(), 0);
    }

    #[test]
    fn a_staged_restore_replaces_the_database_and_drops_the_old_wal() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("dollet.sqlite");
        let staged = dir.path().join("dollet.sqlite.restore");
        std::fs::write(&database, b"old").unwrap();
        std::fs::write(dir.path().join("dollet.sqlite-wal"), b"old wal").unwrap();
        std::fs::write(dir.path().join("dollet.sqlite-shm"), b"old shm").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        apply_staged(&staged, &database).unwrap();

        assert_eq!(std::fs::read(&database).unwrap(), b"new");
        assert!(!staged.exists());
        assert!(!dir.path().join("dollet.sqlite-wal").exists());
        assert!(!dir.path().join("dollet.sqlite-shm").exists());
    }
}
