//! Shared domain, persistence, and pure-function layer.
//!
//! `domain`, `error`, `config` and `http` are the spine every other module
//! builds on. `db`, `settings` and `auth` reach the database; `parse`, `output`
//! and `sync` are pure and take neither a database nor a clock.

pub mod auth;
pub mod backup;
pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod http;
pub mod output;
pub mod parse;
pub mod regex_compat;
pub mod settings;
pub mod sync;

pub use error::{Error, Result};
