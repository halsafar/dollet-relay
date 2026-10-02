//! Backups through the router: taking, listing, downloading, uploading,
//! deleting and restoring, and the boot that applies a restore.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use axum::http::HeaderMap;
use dollet_core::backup::{self, Trigger};

use super::*;

/// The manual endpoint refuses while any backup in this process is being
/// written, and every test here shares one process. Each test that takes a
/// backup, by any route, holds this for its whole run — the authorization
/// matrix too, whose admin pass presses the button — so a refusal here is
/// always the code under test and never a neighbour.
pub(super) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

impl TestApp {
    /// A request whose body, and whose answer, need not be JSON.
    async fn exchange(&self, method: &str, uri: &str, token: &str, body: Option<Vec<u8>>) -> Reply {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            builder = builder.header("content-type", "application/zip");
        }
        let mut request = builder
            .body(body.map_or_else(Body::empty, Body::from))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(PEER.parse::<SocketAddr>().unwrap()));

        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    fn backups_dir(&self) -> PathBuf {
        self.state.config.backups_dir()
    }

    /// What is in `backups/`, finished or not.
    fn on_disk(&self) -> BTreeSet<String> {
        match std::fs::read_dir(self.backups_dir()) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => BTreeSet::new(),
        }
    }

    async fn channels(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM channel")
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}

/// The database inside an archive, written out where a pool can open it.
fn database_in(archive: &[u8], dir: &Path) -> PathBuf {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
    let mut entry = zip.by_name("dollet.sqlite").unwrap();
    let path = dir.join(format!("{}.sqlite", uuid::Uuid::new_v4()));
    std::io::copy(&mut entry, &mut std::fs::File::create(&path).unwrap()).unwrap();
    path
}

async fn open(path: &Path) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap()
}

async fn count(path: &Path, sql: &str) -> i64 {
    let pool = open(path).await;
    let value = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
    pool.close().await;
    value
}

/// An archive with these two entries, for the shapes the server never writes.
fn archive_of(metadata: &[u8], database: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("backup.json", options).unwrap();
    zip.write_all(metadata).unwrap();
    zip.start_file("dollet.sqlite", options).unwrap();
    zip.write_all(database).unwrap();
    zip.finish().unwrap().into_inner()
}

/// A real backup of `app`, as bytes.
async fn backup_of(app: &TestApp) -> Vec<u8> {
    let entry = backup::take(&app.state.db, &app.backups_dir(), Trigger::Manual)
        .await
        .unwrap();
    let path = app.backups_dir().join(&entry.name);
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    bytes
}

fn metadata_in(archive: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).unwrap();
    let mut metadata = Vec::new();
    std::io::Read::read_to_end(&mut zip.by_name("backup.json").unwrap(), &mut metadata).unwrap();
    metadata
}

/// A backup of `app` whose database has been through `change` first.
async fn altered_backup_of(app: &TestApp, scratch: &Path, change: &str) -> Vec<u8> {
    let original = backup_of(app).await;
    let database = database_in(&original, scratch);
    let pool = open(&database).await;
    sqlx::raw_sql(change).execute(&pool).await.unwrap();
    pool.close().await;

    archive_of(&metadata_in(&original), &std::fs::read(database).unwrap())
}

const NEWER_BUILD: &str = "INSERT INTO _sqlx_migrations
    (version, description, success, checksum, execution_time)
    VALUES (9999, 'from a later release', 1, x'00', 0)";

#[tokio::test]
async fn a_backup_is_taken_listed_downloaded_and_deleted() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;

    let created = app
        .exchange("POST", "/api/core/backups/", &token, None)
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json());
    let entry = created.json();
    let name = entry["name"].as_str().unwrap().to_owned();
    assert!(name.ends_with("-manual.zip"), "{name}");
    assert_eq!(entry["trigger"], "manual");
    assert_eq!(entry["version"], env!("CARGO_PKG_VERSION"));
    let newest = dollet_core::db::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap();
    assert_eq!(entry["schema"], newest);

    let (status, listed) = app.json("GET", "/api/core/backups/", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows(&listed), &vec![entry.clone()]);

    let download = app
        .exchange(
            "GET",
            &format!("/api/core/backups/{name}/download/"),
            &token,
            None,
        )
        .await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.headers["content-type"], "application/zip");
    assert_eq!(
        download.headers["content-length"],
        download.body.len().to_string()
    );
    assert_eq!(
        download.headers["content-disposition"],
        format!("attachment; filename=\"{name}\"")
    );
    assert_eq!(entry["size_bytes"], download.body.len() as u64);

    // The bytes are the instance: its channels, read back out of the archive.
    let scratch = tempfile::tempdir().unwrap();
    let database = database_in(&download.body, scratch.path());
    let live = app.channels().await;
    assert!(live > 0);
    assert_eq!(count(&database, "SELECT COUNT(*) FROM channel").await, live);

    let deleted = app
        .exchange(
            "DELETE",
            &format!("/api/core/backups/{name}/"),
            &token,
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(app.on_disk().is_empty());

    let (_, listed) = app.json("GET", "/api/core/backups/", &token, None).await;
    assert!(rows(&listed).is_empty());
    for (method, uri) in [
        ("GET", format!("/api/core/backups/{name}/download/")),
        ("DELETE", format!("/api/core/backups/{name}/")),
    ] {
        let gone = app.exchange(method, &uri, &token, None).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

#[tokio::test]
async fn a_second_manual_backup_while_one_is_running_is_refused() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;

    let running = super::super::backups::RUNNING.lock().await;
    let refused = app
        .exchange("POST", "/api/core/backups/", &token, None)
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.json()["detail"], "a backup is already running");
    drop(running);

    assert!(app.on_disk().is_empty());
}

#[tokio::test]
async fn a_name_that_is_not_a_backup_is_not_found_and_touches_nothing() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;

    std::fs::create_dir_all(app.backups_dir()).unwrap();
    std::fs::write(app.backups_dir().join("notes.txt"), b"not a backup").unwrap();
    let database = app.state.config.db_path();
    let before = std::fs::metadata(&database).unwrap().len();

    for name in [
        "..",
        "..%2F..%2Fdollet.sqlite",
        "%2E%2E",
        "dollet.sqlite",
        "notes.txt",
        "dollet-backup-20261001-000000-weekly.zip",
        "dollet-backup-20261001-000000-manual.zip%2F..",
        // Well formed, and simply not there.
        "dollet-backup-20261001-000000-manual.zip",
    ] {
        for (method, uri) in [
            ("GET", format!("/api/core/backups/{name}/download/")),
            ("DELETE", format!("/api/core/backups/{name}/")),
            ("POST", format!("/api/core/backups/{name}/restore/")),
        ] {
            let reply = app.exchange(method, &uri, &token, None).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
    }

    assert_eq!(app.on_disk(), BTreeSet::from(["notes.txt".to_owned()]));
    assert_eq!(std::fs::metadata(&database).unwrap().len(), before);
    assert!(!app.state.config.staged_restore_path().exists());
}

#[tokio::test]
async fn an_uploaded_backup_is_listed_as_uploaded() {
    let _serial = SERIAL.lock().await;
    let source = TestApp::new().await;
    let bytes = backup_of(&source).await;
    let made = {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        let metadata: Value = serde_json::from_reader(zip.by_name("backup.json").unwrap()).unwrap();
        metadata["created_at"].as_str().unwrap().to_owned()
    };

    let app = TestApp::synthetic().await;
    let token = match app.login_as(Principal::Admin).await {
        Credential::Bearer(token) => token,
        other => panic!("{other:?}"),
    };
    let uploaded = app
        .exchange(
            "POST",
            "/api/core/backups/upload/",
            &token,
            Some(bytes.clone()),
        )
        .await;
    assert_eq!(uploaded.status, StatusCode::CREATED, "{}", uploaded.json());
    let entry = uploaded.json();
    assert_eq!(entry["trigger"], "uploaded");
    // Named for when it was made, not when it arrived.
    assert_eq!(
        entry["created_at"].as_str().unwrap().replace("+00:00", "Z"),
        made
    );
    assert_eq!(entry["size_bytes"], bytes.len() as u64);
    assert_eq!(
        app.on_disk(),
        BTreeSet::from([entry["name"].as_str().unwrap().to_owned()])
    );

    let (_, listed) = app.json("GET", "/api/core/backups/", &token, None).await;
    assert_eq!(rows(&listed), &vec![entry]);

    // The same file twice is one backup, not two.
    let again = app
        .exchange("POST", "/api/core/backups/upload/", &token, Some(bytes))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.json());
    assert_eq!(app.on_disk().len(), 1);
}

/// axum refuses a body over 2 MB unless the route says otherwise, which is
/// every real backup.
#[tokio::test]
async fn an_upload_larger_than_the_default_body_limit_is_accepted() {
    let _serial = SERIAL.lock().await;
    let source = TestApp::new().await;
    // Random bytes, so the archive is as large as the database.
    sqlx::raw_sql(
        "CREATE TABLE ballast (data BLOB);
         INSERT INTO ballast VALUES (randomblob(3000000));",
    )
    .execute(&source.state.db)
    .await
    .unwrap();
    let bytes = backup_of(&source).await;
    assert!(bytes.len() > 2 * 1024 * 1024, "{} bytes", bytes.len());

    let app = TestApp::new().await;
    let token = app.login().await;
    let uploaded = app
        .exchange("POST", "/api/core/backups/upload/", &token, Some(bytes))
        .await;
    assert_eq!(uploaded.status, StatusCode::CREATED, "{}", uploaded.json());
}

#[tokio::test]
async fn an_upload_that_is_not_a_backup_is_refused_and_leaves_nothing_behind() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;
    let existing = backup::take(&app.state.db, &app.backups_dir(), Trigger::Manual)
        .await
        .unwrap();
    let before = app.on_disk();
    assert_eq!(before, BTreeSet::from([existing.name]));

    let real = backup_of(&app).await;
    let not_sqlite = archive_of(&metadata_in(&real), b"definitely not a database");
    for (body, reason) in [
        (b"this is not a zip".to_vec(), "not a zip archive"),
        (not_sqlite, "not a SQLite database"),
        (real[..100].to_vec(), "not a zip archive"),
    ] {
        let refused = app
            .exchange("POST", "/api/core/backups/upload/", &token, Some(body))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        let detail = refused.json()["detail"].as_str().unwrap().to_owned();
        assert!(detail.contains(reason), "{detail}");
        assert_eq!(app.on_disk(), before);
    }
}

/// A connection that drops halfway leaves a partial file, which must go with
/// the request rather than sit in `backups/` until the next boot sweeps it.
#[tokio::test]
async fn an_upload_that_breaks_off_is_refused_and_leaves_nothing_behind() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;

    let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = vec![
        Ok(axum::body::Bytes::from_static(
            b"PK\x03\x04 the first chunk",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "the browser went away",
        )),
    ];
    let mut request = Request::builder()
        .method("POST")
        .uri("/api/core/backups/upload/")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/zip")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(PEER.parse::<SocketAddr>().unwrap()));

    let (status, body) = app.send_raw(request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("did not arrive whole"),
        "{body}"
    );
    assert!(app.on_disk().is_empty(), "{:?}", app.on_disk());
}

#[tokio::test]
async fn a_backup_from_a_newer_build_is_refused_at_upload_and_at_restore() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;
    let scratch = tempfile::tempdir().unwrap();
    let newer = altered_backup_of(&app, scratch.path(), NEWER_BUILD).await;

    let refused = app
        .exchange(
            "POST",
            "/api/core/backups/upload/",
            &token,
            Some(newer.clone()),
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let detail = refused.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("newer build"), "{detail}");
    assert!(app.on_disk().is_empty());

    // Already on disk, the way an archive copied in by hand would be.
    let name = "dollet-backup-20260101-000000-uploaded.zip";
    std::fs::create_dir_all(app.backups_dir()).unwrap();
    std::fs::write(app.backups_dir().join(name), &newer).unwrap();

    let refused = app
        .exchange(
            "POST",
            &format!("/api/core/backups/{name}/restore/"),
            &token,
            None,
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    let detail = refused.json()["detail"].as_str().unwrap().to_owned();
    assert!(detail.contains("newer build"), "{detail}");

    // Refused before anything was written: no safety backup, nothing staged.
    assert_eq!(app.on_disk(), BTreeSet::from([name.to_owned()]));
    assert!(!app.state.config.staged_restore_path().exists());
}

#[tokio::test]
async fn a_restore_stages_the_backup_takes_a_safety_backup_and_asks_to_restart() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;

    let created = app
        .exchange("POST", "/api/core/backups/", &token, None)
        .await;
    let name = created.json()["name"].as_str().unwrap().to_owned();
    let channels = app.channels().await;
    // Changed after the backup, so the staged database and the safety backup
    // can be told apart.
    sqlx::query("DELETE FROM channel")
        .execute(&app.state.db)
        .await
        .unwrap();

    let accepted = app
        .exchange(
            "POST",
            &format!("/api/core/backups/{name}/restore/"),
            &token,
            None,
        )
        .await;
    assert_eq!(accepted.status, StatusCode::ACCEPTED, "{}", accepted.json());
    let body = accepted.json();
    assert_eq!(body["restarting"], true);
    let safety = body["pre_restore"].as_str().unwrap().to_owned();
    assert!(safety.ends_with("-pre-restore.zip"), "{safety}");

    let staged = app.state.config.staged_restore_path();
    assert_eq!(
        count(&staged, "SELECT COUNT(*) FROM channel").await,
        channels
    );
    // Staged beside the database, never left half-written in the data dir.
    assert!(
        std::fs::read_dir(&app.state.config.data_dir)
            .unwrap()
            .all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp-"))
    );

    let scratch = tempfile::tempdir().unwrap();
    let safety_bytes = std::fs::read(app.backups_dir().join(&safety)).unwrap();
    assert_eq!(
        count(
            &database_in(&safety_bytes, scratch.path()),
            "SELECT COUNT(*) FROM channel"
        )
        .await,
        0,
        "the safety backup is not of the state the restore replaces"
    );
    assert_eq!(app.on_disk(), BTreeSet::from([name, safety]));

    assert!(crate::restart::is_requested());
}

/// The function `serve` opens the database with, so this is the boot an
/// operator gets after a restore.
#[tokio::test]
async fn a_staged_restore_is_applied_when_the_database_is_next_opened() {
    let _serial = SERIAL.lock().await;
    let source = TestApp::new().await;
    let channels = source.channels().await;
    assert_eq!(channels, 17);

    // As the build before this one would have written it, so the boot has to
    // migrate it forward as well as move it.
    let scratch = tempfile::tempdir().unwrap();
    let older = altered_backup_of(
        &source,
        scratch.path(),
        "DELETE FROM core_setting WHERE key = 'backup_settings';
         DELETE FROM _sqlx_migrations WHERE version = 2;",
    )
    .await;
    let archive = scratch.path().join("older.zip");
    std::fs::write(&archive, older).unwrap();

    // A data dir with an instance of its own, WAL and all.
    let target = tempfile::tempdir().unwrap();
    let config = crate::test_support::config(target.path(), TrustedProxies::None);
    let replaced = crate::test_support::migrated_db(target.path()).await;
    sqlx::query("INSERT INTO channel_group (name) VALUES ('Replaced')")
        .execute(&replaced)
        .await
        .unwrap();
    replaced.close().await;
    std::fs::write(target.path().join("dollet.sqlite-wal"), b"stale").unwrap();

    let checked = backup::validate(&archive, target.path()).await.unwrap();
    checked
        .database
        .persist(&config.staged_restore_path())
        .unwrap();

    let db = crate::open_database(&config).await.unwrap();
    let restored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(restored, channels);
    let replaced_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM channel_group WHERE name = 'Replaced'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(replaced_rows, 0);

    let newest = dollet_core::db::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap();
    let schema: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(schema, newest);
    assert!(
        dollet_core::settings::by_key(&db, "backup_settings")
            .await
            .unwrap()
            .is_some()
    );
    assert!(!config.staged_restore_path().exists());
    db.close().await;
}

#[tokio::test]
async fn a_staged_restore_that_cannot_be_moved_stops_the_boot() {
    let _serial = SERIAL.lock().await;
    let target = tempfile::tempdir().unwrap();
    let config = crate::test_support::config(target.path(), TrustedProxies::None);
    crate::test_support::migrated_db(target.path())
        .await
        .close()
        .await;
    // A directory cannot be renamed over a file.
    std::fs::create_dir(config.staged_restore_path()).unwrap();

    let refused = crate::open_database(&config)
        .await
        .expect_err("the boot went on without the restore");
    assert!(
        format!("{refused:#}").contains("a restore is staged"),
        "{refused:#}"
    );
}

#[tokio::test]
async fn an_interrupted_backup_is_swept_at_the_next_boot() {
    let _serial = SERIAL.lock().await;
    let target = tempfile::tempdir().unwrap();
    let config = crate::test_support::config(target.path(), TrustedProxies::None);
    std::fs::create_dir_all(config.backups_dir()).unwrap();
    for dir in [config.data_dir.clone(), config.backups_dir()] {
        std::fs::write(dir.join(".tmp-interrupted.sqlite"), b"half").unwrap();
    }

    crate::open_database(&config).await.unwrap().close().await;

    for dir in [config.data_dir.clone(), config.backups_dir()] {
        assert!(!dir.join(".tmp-interrupted.sqlite").exists());
    }
    assert!(config.db_path().exists());
}

// --- The schedule ------------------------------------------------------------

async fn backup_job_interval(app: &TestApp) -> Option<i64> {
    dollet_core::db::jobs::by_key(&app.state.db, "backup")
        .await
        .unwrap()
        .expect("no backup job")
        .interval_seconds
}

#[tokio::test]
async fn the_backup_job_follows_its_settings_without_a_restart() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;
    super::super::jobs::sync_schedule(&app.state).await.unwrap();
    assert_eq!(backup_job_interval(&app).await, Some(24 * 3600));

    let (status, body) = app
        .json(
            "PATCH",
            "/api/core/settings/backup_settings/",
            &token,
            Some(json!({ "value": { "interval_hours": 6 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(backup_job_interval(&app).await, Some(6 * 3600));

    // Zero is off: the row stays, with nothing for `due` to schedule.
    let (status, _) = app
        .json(
            "PATCH",
            "/api/core/settings/backup_settings/",
            &token,
            Some(json!({ "value": { "interval_hours": 0 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(backup_job_interval(&app).await, None);
    let far_future = chrono::Utc::now() + chrono::Duration::days(10_000);
    assert!(
        dollet_core::db::jobs::due(&app.state.db, far_future)
            .await
            .unwrap()
            .iter()
            .all(|job| job.key != "backup")
    );

    // And refused field by field, before anything is stored.
    let (status, refused) = app
        .json(
            "PATCH",
            "/api/core/settings/backup_settings/",
            &token,
            Some(json!({ "value": { "keep": 0 } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(refused["fields"]["keep"].is_string(), "{refused}");
}

#[tokio::test]
async fn a_backup_setting_of_the_wrong_type_is_refused_and_nothing_is_stored() {
    let app = TestApp::new().await;
    let token = app.login().await;

    let (status, refused) = app
        .json(
            "PATCH",
            "/api/core/settings/backup_settings/",
            &token,
            Some(json!({ "value": { "interval_hours": 6, "keep": "seven" } })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .starts_with("backup_settings:"),
        "{refused}"
    );

    let stored: dollet_core::settings::BackupSettings =
        dollet_core::settings::load(&app.state.db).await.unwrap();
    assert_eq!(stored, dollet_core::settings::BackupSettings::default());
}

/// What production does: the row `sync_schedule` writes is dispatched by its
/// kind to whatever `handlers` registered, so a kind spelled differently in
/// the two places would leave the job failing every night with "no handler".
#[tokio::test]
async fn the_backup_job_runs_through_the_scheduler_by_its_kind() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    super::super::jobs::register(super::super::backups::handlers());
    super::super::jobs::sync_schedule(&app.state).await.unwrap();

    super::super::jobs::start_now(&app.state, "backup")
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let finished = loop {
        let job = dollet_core::db::jobs::by_key(&app.state.db, "backup")
            .await
            .unwrap()
            .unwrap();
        if job.last_success_at.is_some() || job.last_error.is_some() {
            break job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the job never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(finished.last_error, None);
    assert!(
        finished
            .message
            .as_deref()
            .is_some_and(|message| message.starts_with("wrote dollet-backup-")),
        "{:?}",
        finished.message
    );

    let on_disk = app.on_disk();
    assert_eq!(on_disk.len(), 1, "{on_disk:?}");
    assert!(
        on_disk.iter().all(|name| name.ends_with("-scheduled.zip")),
        "{on_disk:?}"
    );
}

#[tokio::test]
async fn a_scheduled_backup_keeps_only_the_newest_scheduled_ones() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    let token = app.login().await;
    let (status, _) = app
        .json(
            "PATCH",
            "/api/core/settings/backup_settings/",
            &token,
            Some(json!({ "value": { "keep": 2 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let manual = app
        .exchange("POST", "/api/core/backups/", &token, None)
        .await
        .json()["name"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut summaries = Vec::new();
    for _ in 0..3 {
        summaries.push(super::super::backups::scheduled(&app.state).await.unwrap());
    }
    assert!(
        summaries[0].starts_with("wrote dollet-backup-"),
        "{summaries:?}"
    );
    assert!(
        summaries[2].ends_with("removed 1 older scheduled backup"),
        "{summaries:?}"
    );

    let on_disk = app.on_disk();
    let scheduled: Vec<&String> = on_disk
        .iter()
        .filter(|name| name.ends_with("-scheduled.zip"))
        .collect();
    assert_eq!(scheduled.len(), 2, "{on_disk:?}");
    assert!(
        on_disk.contains(&manual),
        "retention deleted a manual backup"
    );
    // The two kept are the two newest, which the third run wrote last.
    assert!(
        summaries[1..]
            .iter()
            .all(|summary| scheduled.iter().any(|name| summary.contains(name.as_str()))),
        "{summaries:?} / {scheduled:?}"
    );
}

#[tokio::test]
async fn a_failed_scheduled_backup_is_raised_and_the_next_success_clears_it() {
    let _serial = SERIAL.lock().await;
    let app = TestApp::new().await;
    // A file where the directory should be: the same refusal a full or
    // read-only disk gives, and one a test can take away again.
    std::fs::write(app.backups_dir(), b"in the way").unwrap();

    let failure = super::super::backups::scheduled(&app.state).await;
    assert!(failure.is_err());
    let raised = dollet_core::db::notifications::list(&app.state.db)
        .await
        .unwrap();
    let raised: Vec<_> = raised
        .iter()
        .filter(|n| n.kind == "backup_failed" && n.subject == "backup")
        .collect();
    assert_eq!(raised.len(), 1);
    assert_eq!(
        raised[0].severity,
        dollet_core::db::notifications::Severity::Error
    );

    std::fs::remove_file(app.backups_dir()).unwrap();
    super::super::backups::scheduled(&app.state).await.unwrap();
    assert!(
        dollet_core::db::notifications::list(&app.state.db)
            .await
            .unwrap()
            .iter()
            .all(|n| n.kind != "backup_failed"),
        "the backup worked and the notification stayed"
    );
}
