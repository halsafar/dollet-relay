use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,

    #[error("{0}")]
    Invalid(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("upstream: {0}")]
    Upstream(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn invalid(msg: impl fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }

    pub fn upstream(msg: impl fmt::Display) -> Self {
        Self::Upstream(msg.to_string())
    }
}
