//! Provider and guide parsers.
//!
//! Everything here is `bytes -> struct`: no database, no HTTP, no async. That
//! is what makes total coverage cheap, which is why both parsers are on the
//! must-be-100% list.

pub mod compress;
mod entities;
pub mod m3u;
pub mod pgdump;
pub mod xmltv;
