//! A restart this process asks for itself, which is how a restore is applied.
//!
//! The pool is held by value in `AppState`, by every stream session and by the
//! scheduler, so the database cannot be swapped under a running process. A
//! restore is staged beside it instead and moved into place at the next boot,
//! and getting to that boot is an ordinary graceful shutdown: `shutdown_signal`
//! waits on this beside SIGTERM, so streams end and jobs drain exactly as they
//! do for `docker stop`. The process then exits 0 and its supervisor starts it
//! again.

use std::sync::LazyLock;

use tokio_util::sync::CancellationToken;

static REQUESTED: LazyLock<CancellationToken> = LazyLock::new(CancellationToken::new);

pub fn request() {
    REQUESTED.cancel();
}

pub async fn requested() {
    REQUESTED.cancelled().await;
}

#[cfg(test)]
pub fn is_requested() -> bool {
    REQUESTED.is_cancelled()
}
