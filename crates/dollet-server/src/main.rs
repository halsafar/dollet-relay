mod api;
#[cfg(test)]
mod contract;
mod health;
mod spa;
#[cfg(test)]
mod test_support;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::routing::get;
use clap::{Parser, Subcommand};
use dollet_core::config::Config;
use sqlx::SqlitePool;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
}

/// Under `--help`, because that is the first place an operator looks.
/// `the_help_text_names_every_variable_the_config_reads` keeps it in step with
/// `config::KNOWN_VARIABLES`.
const CONFIGURATION: &str = "\
Configuration is read from the environment:
  DOLLET_LISTEN               address:port to serve on (default 0.0.0.0:9191)
  DOLLET_DATA_DIR             database and cache directory (default /data)
  DOLLET_LOG                  log filter (default info)
  DOLLET_ADVERTISED_BASE_URL  base of the URLs handed to clients
  DOLLET_ARTWORK_BASE_URL     base of logo and guide-icon URLs
  DOLLET_TRUSTED_PROXIES      none | private | CIDR,CIDR (default none)
  DOLLET_IMPORT_BACKUP        A backup to import on first boot";

#[derive(Parser)]
#[command(
    name = "dollet",
    version,
    about = "Live TV relay: ingest providers, curate channels, serve Plex",
    after_help = CONFIGURATION
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve,
    /// Probe /health on the configured port and exit non-zero if it is not
    /// answering. Used as the container healthcheck.
    Health,
    /// Load a SQL file into an instance that has no users yet, for a test
    /// instance or a demo.
    Seed {
        /// A file of SQL statements, applied after the migrations.
        path: std::path::PathBuf,
    },
    /// Import a Dispatcharr instance from its backup zip.
    Import {
        /// A dispatcharr-backup-*.zip, or the database.dump inside one.
        #[arg(long)]
        backup: std::path::PathBuf,
        /// Also write the full report to this path as JSON.
        ///
        /// The log is for reading once. The JSON is for anything that has to
        /// check the result: a test, a migration script, or comparing two runs.
        #[arg(long)]
        report: Option<std::path::PathBuf>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Before the subscriber exists, so this reaches a stderr nobody has
    // filtered. A configuration that cannot be honoured stops the process
    // rather than being silently replaced by a default: falling back to
    // `0.0.0.0:9191` on an unparseable `DOLLET_LISTEN` would turn a typo in a
    // loopback bind into a published port.
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("dollet: {e}");
            std::process::exit(2);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&config.log_filter))
        .init();

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::Health => probe_health(&config),
        Command::Seed { path } => seed(config, &path).await,
        Command::Import { backup, report } => import(config, &backup, report.as_deref()).await,
    }
}

/// Ask the running server whether it is serving, over a plain socket.
///
/// Not `reqwest`: every client this project builds refuses loopback by design,
/// and the image carries no curl. The URL is ours rather than provider-supplied,
/// so none of the SSRF machinery applies to it.
fn probe_health(config: &Config) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::{Ipv6Addr, TcpStream};

    // An unspecified bind is reachable on loopback, and dialling 0.0.0.0 is not
    // portable.
    let target = match config.listen.ip() {
        ip if !ip.is_unspecified() => config.listen,
        std::net::IpAddr::V4(_) => SocketAddr::from(([127, 0, 0, 1], config.listen.port())),
        std::net::IpAddr::V6(_) => SocketAddr::from((Ipv6Addr::LOCALHOST, config.listen.port())),
    };

    let timeout = std::time::Duration::from_secs(3);
    let mut socket = TcpStream::connect_timeout(&target, timeout)
        .with_context(|| format!("connecting to {target}"))?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;

    // HTTP/1.0, so the server closes rather than holding the connection open
    // for a request that will never come.
    socket.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;

    let mut head = Vec::new();
    let mut buffer = [0u8; 256];
    while !head.contains(&b'\n') && head.len() < 1024 {
        match socket.read(&mut buffer)? {
            0 => break,
            read => head.extend_from_slice(&buffer[..read]),
        }
    }

    let status = String::from_utf8_lossy(&head);
    let status = status.lines().next().unwrap_or_default().trim();
    anyhow::ensure!(
        status.starts_with("HTTP/1.") && status.split(' ').nth(1) == Some("200"),
        "/health answered `{status}`"
    );
    Ok(())
}

/// Run the migrations on the configured data dir and apply a SQL file to it.
///
/// Refuses unless the `user` table is empty, which is the gate
/// `api::importer::maybe_import_on_boot` uses and for the same reason: a seed
/// names its own ids, so replaying one over an instance somebody has curated
/// writes rows on top of theirs with no way back. This stands up a known
/// instance for the browser tests and for demos — it is not a migration tool,
/// and the file is executed exactly as written.
async fn seed(config: Config, path: &std::path::Path) -> anyhow::Result<()> {
    // Before the database is touched, so a typo in the path does not leave an
    // empty instance behind that the gate then refuses to seed.
    let sql = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;

    let db = open_database(&config).await?;

    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user")
        .fetch_one(&db)
        .await
        .context("counting users")?;
    anyhow::ensure!(
        users == 0,
        "this instance already has {users} user(s), and a seed writes its own ids over \
         whatever is there; point DOLLET_DATA_DIR at an empty directory"
    );

    sqlx::raw_sql(&sql)
        .execute(&db)
        .await
        .with_context(|| format!("applying {}", path.display()))?;

    tracing::info!(path = %path.display(), "seeded");
    Ok(())
}

async fn import(
    config: Config,
    backup: &std::path::Path,
    report_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let source = api::ImportSource::backup(backup).await?;
    let db = open_database(&config).await?;
    let report = api::run_import(source, &db).await?;

    // Written before anything below can return early, so a run that ends in a
    // non-zero exit still leaves the full record behind.
    if let Some(path) = report_path {
        let json =
            serde_json::to_string_pretty(&report).context("serializing the import report")?;
        std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), "import report written");
    }

    api::log_report(&report);

    // The rows *are* imported — keeping them is the point, because the operator
    // fixes the pattern in the UI and the filter starts working. What did not
    // survive is the pattern's meaning: until it is fixed it matches nothing,
    // so the streams it was written to exclude are being kept.
    //
    // Still a non-zero exit: this is a one-shot migration, and a lineup
    // filtered differently from the one the operator left behind is something
    // they have to be told about while they are still watching.
    if !report.regex_failures.is_empty() {
        anyhow::bail!(
            "{} imported regex pattern(s) will not compile, so they currently match nothing; \
             fix them in Settings and the filters they belong to start applying again",
            report.regex_failures.len()
        );
    }

    tracing::info!("import complete");
    Ok(())
}

async fn open_database(config: &Config) -> anyhow::Result<SqlitePool> {
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("creating data dir {}", config.data_dir.display()))?;
    tokio::fs::create_dir_all(config.cache_dir())
        .await
        .with_context(|| format!("creating cache dir {}", config.cache_dir().display()))?;

    let db = dollet_core::db::connect(&config.db_path())
        .await
        .context("opening database")?;
    dollet_core::db::migrate(&db)
        .await
        .context("running migrations")?;
    Ok(db)
}

async fn serve(config: Config) -> anyhow::Result<()> {
    // Here rather than in `Config::from_env`, which runs before the subscriber
    // exists — and here rather than in `main`, so the container healthcheck does
    // not repeat it every thirty seconds. Once per start, where an operator who
    // has just changed a compose file is looking.
    let unknown = dollet_core::config::unknown_variables();
    if !unknown.is_empty() {
        tracing::warn!(
            variables = %unknown.join(", "),
            "unrecognised DOLLET_* variables are set and have no effect"
        );
    }

    let db = open_database(&config).await?;

    // After the migrations and before anything starts running against the
    // database. A failure here stops the process: the operator asked for a
    // migration, and serving an empty instance instead is the confusing answer.
    api::maybe_import_on_boot(&db, &config).await?;

    let listen = config.listen;
    let state = AppState {
        db,
        config: Arc::new(config),
    };

    // Registers a job per provider account and EPG source, then runs whatever
    // has come due. Started before the listener so
    // a refresh that was due during downtime is already moving when the first
    // request arrives.
    api::start_scheduler(state.clone()).await?;

    let app = Router::new()
        .route("/health", get(health::health))
        .nest("/api", api::router())
        // The streaming endpoint, HDHomeRun, the playlist and guide outputs,
        // and the Xtream Codes API. Their paths are fixed by the clients that
        // consume them, so they cannot live under `/api`.
        .merge(api::public_router())
        .fallback(spa::serve)
        // A panic in a handler otherwise drops the connection with no status,
        // which on a box whose main job is holding long-lived streaming
        // responses reads to the client as the stream dying. Job handlers have
        // their own catch_unwind; this covers the request path.
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;

    tracing::info!(
        %listen,
        resident_bytes = health::resident_bytes().unwrap_or(0),
        "dollet started"
    );

    warn_about_unusable_render_nodes();

    // Peer address must reach the handlers, or `network_access` can only be
    // enforced for requests a trusted proxy annotated with X-Forwarded-For —
    // a direct connection would have no address to match and would be allowed.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")?;

    // After the listener closes, not before: an in-flight refresh is asked to
    // stop and given a moment, so a restart does not leave a job row claiming
    // to be running and a plan half applied.
    api::stop_scheduler().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler")
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutting down");

    // Before the drain rather than after it. `with_graceful_shutdown` waits for
    // in-flight responses, and an MPEG-TS body never ends on its own — so with
    // one viewer attached the process would sit until the runtime's SIGKILL,
    // and everything sequenced after the drain, `stop_scheduler` included,
    // would never run. Ending the sessions lets each client stream finish the
    // ring tail and close.
    api::shutdown_streams();
}

/// Warn once per render node this process cannot open.
///
/// `--device /dev/dri` mounts nodes owned by `root:render` at 0660, and the
/// container's uid is in neither group unless the operator says so. The only
/// other symptom is an output profile whose ffmpeg exits immediately, which
/// reads to a viewer as the channel being broken.
#[cfg(target_os = "linux")]
fn warn_about_unusable_render_nodes() {
    use std::os::unix::fs::MetadataExt;

    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return;
    };

    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("renderD") {
            continue;
        }

        let path = entry.path();
        let Err(e) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        if e.kind() != std::io::ErrorKind::PermissionDenied {
            continue;
        }

        let (uid, gid) = std::fs::metadata(&path)
            .map(|meta| (meta.uid(), meta.gid()))
            .unwrap_or((0, 0));
        tracing::warn!(
            node = %path.display(),
            uid,
            gid,
            "cannot open this render node, so a transcoding output profile will fail; add \
             `group_add: [\"{gid}\"]` to the compose service — find the gid on the host with \
             `getent group render`"
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn warn_about_unusable_render_nodes() {}

#[cfg(test)]
mod tests {
    use super::*;
    use dollet_core::config::TrustedProxies;
    use std::path::{Path, PathBuf};

    fn config_for(dir: &Path) -> Config {
        crate::test_support::config(dir, TrustedProxies::None)
    }

    /// The binary reads its environment in exactly one place, and
    /// `config::KNOWN_VARIABLES` is the list of what it finds there. A
    /// `DOLLET_*` variable read here instead would be a real setting that the
    /// unknown-variable warning calls a typo.
    #[test]
    fn nothing_here_reads_a_dollet_variable_behind_the_configs_back() {
        let needle = concat!("std::env::var(", '"', "DOLLET_");
        assert!(
            !include_str!("main.rs").contains(needle),
            "read it in `dollet_core::config` and add it to `KNOWN_VARIABLES`"
        );
    }

    /// `--help` is where an operator finds out how to configure the binary,
    /// so every variable the config reads has to be in it.
    #[test]
    fn the_help_text_names_every_variable_the_config_reads() {
        for name in dollet_core::config::KNOWN_VARIABLES {
            assert!(
                CONFIGURATION.contains(name),
                "{name} is missing from --help"
            );
        }
    }

    /// The file `scripts/e2e.sh` stands its seeded server up from, so this is
    /// the same path the browser tests depend on.
    fn synthetic_seed() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/synthetic/instance.sql")
    }

    async fn count(db: &SqlitePool, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(db).await.expect(sql)
    }

    #[tokio::test]
    async fn seeding_an_empty_data_dir_migrates_it_and_applies_the_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config = config_for(dir.path());

        seed(config_for(dir.path()), &synthetic_seed())
            .await
            .expect("seeding a fresh instance");

        let db = dollet_core::db::connect(&config.db_path())
            .await
            .expect("reopen");
        // The four users and seventeen channels the seed's README describes: a
        // run that migrated but applied nothing would still open cleanly.
        assert_eq!(count(&db, "SELECT COUNT(*) FROM user").await, 4);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM channel").await, 17);
        db.close().await;
    }

    #[tokio::test]
    async fn seeding_an_instance_that_already_has_users_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        seed(config_for(dir.path()), &synthetic_seed())
            .await
            .expect("seeding a fresh instance");

        let refused = seed(config_for(dir.path()), &synthetic_seed())
            .await
            .expect_err("a second seed must be refused");

        assert!(
            refused.to_string().contains("already has 4 user(s)"),
            "unhelpful refusal: {refused}"
        );
    }
}
