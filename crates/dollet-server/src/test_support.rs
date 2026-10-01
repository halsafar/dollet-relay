//! Shared by every test module in this crate.

use std::net::SocketAddr;
use std::path::Path;

use dollet_core::config::{Config, TrustedProxies};
use sqlx::SqlitePool;

/// The shipped defaults on a loopback port: nothing advertised, nothing to
/// import, and no proxy trusted unless the test says so.
pub fn config(data_dir: &Path, trusted_proxies: TrustedProxies) -> Config {
    Config {
        listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        data_dir: data_dir.to_path_buf(),
        advertised_base_url: None,
        artwork_base_url: None,
        trusted_proxies,
        import_backup: None,
        log_filter: "off".into(),
    }
}

/// A migrated, empty database in `dir`, which is what a first boot has.
pub async fn migrated_db(dir: &Path) -> SqlitePool {
    let db = dollet_core::db::connect(&dir.join("dollet.sqlite"))
        .await
        .expect("open database");
    dollet_core::db::migrate(&db).await.expect("migrate");
    db
}
