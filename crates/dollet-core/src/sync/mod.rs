//! Deciding what a refreshed provider feed means against what is already stored.
//!
//! The parsers next door turn provider bytes into structs; this turns those
//! structs plus current state into a decision. Still pure: no database, no HTTP,
//! no clock of its own — every function takes what it needs and returns what
//! should happen, and the handlers compose them with the query layer.
//!
//! These are the functions that keep an imported catalogue alive. Serving
//! imported data only gets a migration to cutover; refreshing it is what makes
//! the instance self-sufficient.
//!
//! Two of them are load-bearing far out of proportion to their size:
//! [`hash`] decides whether a refreshed stream is the same stream, and getting
//! it wrong orphans the catalogue; [`streams`] keeps "missing today" distinct
//! from "gone", and collapsing the two turns a provider hiccup into a deleted
//! lineup.

pub mod channels;
pub mod epg;
pub mod filters;
pub mod hash;
pub mod streams;
