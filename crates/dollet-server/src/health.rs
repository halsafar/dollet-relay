//! Liveness plus the numbers this project exists to move.
//!
//! Resident memory is reported from the first commit rather than measured once
//! at the end: memory is the budget this project is built around, and a
//! regression here is a regression in the whole point of it.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct Health {
    status: &'static str,
    version: &'static str,
    resident_bytes: Option<u64>,
    wal_bytes: Option<i64>,
}

/// Resident set size from `/proc/self/statm`, in bytes.
///
/// Field 2 is resident pages. Returns `None` off Linux, where the container
/// this ships in does not run anyway.
pub fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

pub async fn health(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        resident_bytes: resident_bytes(),
        wal_bytes: dollet_core::db::wal_bytes(&state.db).await.ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore)]
    fn reports_a_plausible_resident_size() {
        let rss = resident_bytes().expect("statm readable on linux");
        assert!(rss > 1 << 20, "implausibly small rss: {rss}");
    }
}
