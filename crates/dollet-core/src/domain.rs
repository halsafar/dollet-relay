//! The types every layer agrees on.
//!
//! `db` maps rows into these, `parse` parses into them, and `output`
//! serializes out of them. A change here ripples through all three at once, so
//! it is proposed rather than made in place.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

pub type Id = i64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserLevel {
    #[default]
    Streamer = 0,
    Standard = 1,
    Admin = 10,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Id,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub is_active: bool,
    pub user_level: UserLevel,
    pub api_key: Option<String>,
    /// 0 means unlimited.
    pub stream_limit: i32,
    pub channel_profile_ids: Vec<Id>,
    pub custom_properties: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelGroup {
    pub id: Id,
    pub name: String,
    /// Where the group's channels are numbered. `None` at the start means the
    /// group has no range and a new channel falls back to the lowest free
    /// integer. The rule for placing one *inside* a range is
    /// `sync::channels::next_slot`, which appends on a step grid rather than
    /// filling the lowest gap — see its comment for why.
    pub number_start: Option<f64>,
    pub number_end: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logo {
    pub id: Id,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: Id,
    pub name: String,
    pub url: Option<String>,
    pub logo_url: Option<String>,
    pub tvg_id: Option<String>,
    pub channel_group_id: Option<Id>,
    pub m3u_account_id: Option<Id>,
    pub stream_profile_id: Option<Id>,
    pub is_custom: bool,
    pub is_adult: bool,
    /// Provider-assigned id, distinct from `id`.
    pub stream_id: Option<i64>,
    pub stream_chno: Option<f64>,
    /// Dedup key, derived per `stream_settings.m3u_hash_key`.
    pub stream_hash: Option<String>,
    pub last_seen: DateTime<Utc>,
    pub is_stale: bool,
    pub is_catchup: bool,
    pub catchup_days: u32,
    pub custom_properties: Json,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    pub uuid: Uuid,
    pub channel_number: Option<f64>,
    pub name: String,
    pub logo_id: Option<Id>,
    pub channel_group_id: Option<Id>,
    pub tvg_id: Option<String>,
    pub tvc_guide_stationid: Option<String>,
    pub epg_data_id: Option<Id>,
    pub stream_profile_id: Option<Id>,
    pub user_level: UserLevel,
    pub is_adult: bool,
    pub hidden_from_output: bool,
    pub auto_created: bool,
    pub is_catchup: bool,
    pub catchup_days: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Per-channel user edits layered over the synced `Channel`.
///
/// Every field is nullable: null means "inherit". Outputs never read `Channel`
/// directly, they read [`EffectiveChannel`], which is this coalesced in SQL so
/// sorting and filtering stay in the query rather than in Rust.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelOverride {
    pub channel_id: Id,
    pub name: Option<String>,
    pub channel_number: Option<f64>,
    pub channel_group_id: Option<Id>,
    pub logo_id: Option<Id>,
    pub tvg_id: Option<String>,
    pub tvc_guide_stationid: Option<String>,
    pub epg_data_id: Option<Id>,
    pub stream_profile_id: Option<Id>,
}

/// A channel with overrides already applied. The only channel shape outputs see.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectiveChannel {
    pub id: Id,
    pub uuid: Uuid,
    pub channel_number: Option<f64>,
    pub name: String,
    /// Both forms are carried because the Xtream live payload needs the group
    /// *id* for `category_id` while M3U and XMLTV need the name.
    pub channel_group_id: Option<Id>,
    pub group_name: Option<String>,
    /// Xtream reports this as `added`.
    pub created_at: DateTime<Utc>,
    pub logo_url: Option<String>,
    pub tvg_id: Option<String>,
    pub tvc_guide_stationid: Option<String>,
    pub epg_data_id: Option<Id>,
    pub stream_profile_id: Option<Id>,
    pub user_level: UserLevel,
    pub is_adult: bool,
    pub hidden_from_output: bool,
    pub is_catchup: bool,
    pub catchup_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelProfile {
    pub id: Id,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelProfileMembership {
    pub channel_profile_id: Id,
    pub channel_id: Id,
    pub enabled: bool,
}

/// `snake_case`, not `lowercase`: the derive would otherwise spell this
/// `xtreamcodes`, a fourth spelling of a value the database stores as
/// `xtream_codes`. Nothing serializes this enum; the first thing that does
/// must not invent another spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3uAccountType {
    Standard,
    XtreamCodes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uAccount {
    pub id: Id,
    pub name: String,
    pub server_url: Option<String>,
    pub file_path: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub account_type: M3uAccountType,
    pub max_streams: u32,
    pub is_active: bool,
    pub locked: bool,
    pub priority: u32,
    pub user_agent_id: Option<Id>,
    pub stream_profile_id: Option<Id>,
    pub refresh_interval_hours: u32,
    pub stale_stream_days: u32,
    pub custom_properties: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M3uAccountProfile {
    pub id: Id,
    pub m3u_account_id: Id,
    pub name: String,
    pub is_default: bool,
    pub is_active: bool,
    pub max_streams: u32,
    /// User-authored, PCRE-flavoured. Compile with `fancy_regex`, never `regex`.
    pub search_pattern: String,
    /// Rust replacement syntax, where `$1` is already a capture group. Never
    /// passed through `regex_compat::js_backrefs_to_rust`: that conversion is
    /// for *search* patterns, and applying it here emits the literal `\1` and
    /// silently breaks every rename.
    pub replace_pattern: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EpgSourceType {
    Xmltv,
    Dummy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgSource {
    pub id: Id,
    pub name: String,
    pub source_type: EpgSourceType,
    pub url: Option<String>,
    pub file_path: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub is_active: bool,
    pub priority: u32,
    pub refresh_interval_hours: u32,
    pub custom_properties: Json,
}

/// One guide channel, keyed by `tvg_id` within a source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpgData {
    pub id: Id,
    pub epg_source_id: Option<Id>,
    pub tvg_id: Option<String>,
    pub name: String,
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Program {
    pub id: Id,
    pub epg_data_id: Id,
    pub tvg_id: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub title: String,
    pub sub_title: Option<String>,
    pub description: Option<String>,
    pub custom_properties: Json,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAgent {
    pub id: Id,
    pub name: String,
    pub user_agent: String,
    pub is_active: bool,
}

/// How a channel's upstream bytes are obtained.
///
/// `command` empty means proxy the URL directly. The locked `redirect` profile
/// is special-cased to a 302 rather than proxied at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamProfile {
    pub id: Id,
    pub name: String,
    pub command: String,
    pub parameters: String,
    pub locked: bool,
    pub is_active: bool,
    pub user_agent_id: Option<Id>,
}

/// A transcode applied between the channel ring and the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputProfile {
    pub id: Id,
    pub name: String,
    pub command: String,
    pub parameters: String,
    pub locked: bool,
    pub is_active: bool,
}
