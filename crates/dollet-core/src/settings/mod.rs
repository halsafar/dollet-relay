//! Runtime settings, database backed.
//!
//! Grouped JSON settings: `stream_settings`, `proxy_settings`,
//! `network_access`, `system_settings`, `epg_settings`, `numbering_settings`.
//! Every default has a reason of its own, stated on the field.
//!
//! **Every field here is read by something.** A knob that is stored, shown in
//! the Settings page and consulted nowhere is worse than a missing feature: an
//! operator sets it, sees it save, and believes it took effect.
//!
//! Groups are whole JSON blobs rather than a row per key because the Settings
//! page saves a section at a time; a per-key table would let half a section
//! land. Every field carries its own serde default, so a group written by an
//! older build — or by the importer, from a source with a slightly different
//! key set — still deserializes instead of resetting the whole section.

pub mod cidr;

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::Result;
use crate::domain::Id;

pub use cidr::Cidr;

/// A settings section, addressed by its row key in `core_setting`.
pub trait Group: Serialize + DeserializeOwned + Default {
    const KEY: &'static str;
    const NAME: &'static str;
}

pub async fn load<G: Group>(pool: &SqlitePool) -> Result<G> {
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM core_setting WHERE key = ?")
        .bind(G::KEY)
        .fetch_optional(pool)
        .await?;

    match raw {
        // A section that fails to parse is a section the user can no longer
        // edit, which is worse than briefly showing defaults. Log and carry on.
        Some(json) => Ok(serde_json::from_str(&json).unwrap_or_else(|e| {
            tracing::warn!(key = G::KEY, error = %e, "settings group unreadable, using defaults");
            G::default()
        })),
        None => Ok(G::default()),
    }
}

pub async fn save<G: Group>(pool: &SqlitePool, value: &G) -> Result<()> {
    let json = serde_json::to_string(value)?;
    sqlx::query(
        "INSERT INTO core_setting (key, name, value) VALUES (?, ?, ?)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind(G::KEY)
    .bind(G::NAME)
    .bind(json)
    .execute(pool)
    .await?;
    Ok(())
}

/// The `network_access` a patch would produce, without saving it.
///
/// Merged, never replaced: an endpoint key the client did not send would
/// otherwise disappear, and an absent key allows every address — so a PATCH
/// touching only `STREAMS` would quietly open the admin UI to the internet.
///
/// Exposed so a caller can report *which* entry is wrong before delegating.
/// `patch_by_key` runs the same check and remains the authority; this cannot be
/// the only one, because nothing stops another caller reaching it directly.
pub async fn merge_network_access(
    pool: &SqlitePool,
    value: &serde_json::Value,
) -> Result<NetworkAccess> {
    let current = serde_json::to_value(load::<NetworkAccess>(pool).await?)?;
    serde_json::from_value(merge(current, value))
        .map_err(|e| crate::Error::invalid(format!("network_access: {e}")))
}

/// The numbering settings as they would be after `value` is applied, for the
/// API to report field by field before `patch_by_key` refuses it whole.
pub async fn merge_numbering(
    pool: &SqlitePool,
    value: &serde_json::Value,
) -> Result<NumberingSettings> {
    let current = serde_json::to_value(load::<NumberingSettings>(pool).await?)?;
    serde_json::from_value(merge(current, value))
        .map_err(|e| crate::Error::invalid(format!("numbering_settings: {e}")))
}

/// Overlay a partial object onto the stored one.
///
/// Two rules, and both exist because the Settings page PATCHes one field at a
/// time. An absent key keeps its stored value, so a form that sends a single
/// field does not reset the rest of the section. An explicit `null` *removes*
/// the key, which leaves the struct's own default to fill it — so clearing a
/// box resets that setting rather than storing `""` and failing to parse.
///
/// Never a wholesale replace.
fn merge(current: serde_json::Value, incoming: &serde_json::Value) -> serde_json::Value {
    match (current, incoming) {
        (serde_json::Value::Object(mut base), serde_json::Value::Object(incoming)) => {
            for (key, value) in incoming {
                if value.is_null() {
                    base.remove(key);
                } else {
                    base.insert(key.clone(), value.clone());
                }
            }
            serde_json::Value::Object(base)
        }
        (_, other) => other.clone(),
    }
}

pub async fn patch<G: Group>(pool: &SqlitePool, patch: &serde_json::Value) -> Result<G> {
    let current = serde_json::to_value(load::<G>(pool).await?)?;
    let merged = merge(current, patch);

    let value: G = serde_json::from_value(merged)
        .map_err(|e| crate::Error::invalid(format!("{}: {e}", G::KEY)))?;
    save(pool, &value).await?;
    Ok(value)
}

/// A settings row as the Settings page sees it.
#[derive(Debug, Clone, Serialize)]
pub struct SettingRow {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub value: serde_json::Value,
}

/// Every group, in the order the Settings page lists them. Reading through the
/// typed groups rather than dumping the table means a section written by an
/// older build still comes back complete, with its missing fields defaulted.
pub async fn all(pool: &SqlitePool) -> Result<Vec<SettingRow>> {
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT id, key, name FROM core_setting ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, key, name) in rows {
        if let Some(value) = typed_value(pool, &key).await? {
            out.push(SettingRow {
                id,
                key,
                name,
                value,
            });
        }
    }
    Ok(out)
}

pub async fn by_key(pool: &SqlitePool, key: &str) -> Result<Option<SettingRow>> {
    Ok(all(pool).await?.into_iter().find(|row| row.key == key))
}

/// Resolve a settings row addressed either by key or by numeric id, because
/// the API exposes the id and everything here is clearer with the key.
pub async fn by_key_or_id(pool: &SqlitePool, reference: &str) -> Result<Option<SettingRow>> {
    let rows = all(pool).await?;
    Ok(match reference.parse::<i64>() {
        Ok(id) => rows.into_iter().find(|row| row.id == id),
        Err(_) => rows.into_iter().find(|row| row.key == reference),
    })
}

async fn typed_value(pool: &SqlitePool, key: &str) -> Result<Option<serde_json::Value>> {
    let value = match key {
        StreamSettings::KEY => serde_json::to_value(load::<StreamSettings>(pool).await?)?,
        ProxySettings::KEY => serde_json::to_value(load::<ProxySettings>(pool).await?)?,
        NetworkAccess::KEY => serde_json::to_value(load::<NetworkAccess>(pool).await?)?,
        SystemSettings::KEY => serde_json::to_value(load::<SystemSettings>(pool).await?)?,
        EpgSettings::KEY => serde_json::to_value(load::<EpgSettings>(pool).await?)?,
        NumberingSettings::KEY => serde_json::to_value(load::<NumberingSettings>(pool).await?)?,
        // A row this build does not know about is not served at all, so a
        // future secret parked in this table cannot leak through the list.
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// Apply a partial update to whichever group the key names.
pub async fn patch_by_key(
    pool: &SqlitePool,
    key: &str,
    value: &serde_json::Value,
) -> Result<serde_json::Value> {
    let updated = match key {
        StreamSettings::KEY => serde_json::to_value(patch::<StreamSettings>(pool, value).await?)?,
        ProxySettings::KEY => serde_json::to_value(patch::<ProxySettings>(pool, value).await?)?,
        NetworkAccess::KEY => {
            let candidate = merge_network_access(pool, value).await?;

            // Validated before the save, not after: an entry that will not
            // parse is skipped at request time, so storing one widens access
            // just as silently.
            let invalid = candidate.invalid_entries();
            if !invalid.is_empty() {
                let list: Vec<String> = invalid
                    .iter()
                    .map(|(endpoint, entry)| format!("{endpoint}: {entry}"))
                    .collect();
                return Err(crate::Error::invalid(format!(
                    "invalid CIDRs — {}",
                    list.join(", ")
                )));
            }

            save(pool, &candidate).await?;
            serde_json::to_value(candidate)?
        }
        SystemSettings::KEY => serde_json::to_value(patch::<SystemSettings>(pool, value).await?)?,
        EpgSettings::KEY => serde_json::to_value(patch::<EpgSettings>(pool, value).await?)?,
        NumberingSettings::KEY => {
            // Checked before the save, like the CIDRs: a step of zero stored
            // here would fail every allocation on the next refresh with no
            // sign of why.
            let current = serde_json::to_value(load::<NumberingSettings>(pool).await?)?;
            let candidate: NumberingSettings = serde_json::from_value(merge(current, value))
                .map_err(|e| crate::Error::invalid(format!("{}: {e}", NumberingSettings::KEY)))?;
            let problems = candidate.problems();
            if !problems.is_empty() {
                let list: Vec<String> = problems
                    .iter()
                    .map(|(field, reason)| format!("{field} {reason}"))
                    .collect();
                return Err(crate::Error::invalid(list.join("; ")));
            }
            save(pool, &candidate).await?;
            serde_json::to_value(candidate)?
        }
        other => {
            return Err(crate::Error::NotFound).inspect_err(|_| {
                tracing::debug!(key = other, "unknown settings group");
            });
        }
    };
    Ok(updated)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub default_user_agent: Option<Id>,
    pub default_stream_profile: Option<Id>,
    /// Which EXTINF field identifies a stream across refreshes. Changing it
    /// re-hashes every stream, so it is a setting rather than a constant.
    pub m3u_hash_key: String,
    /// Applied to HDHR lineup URLs when set, so Plex gets transcoded audio
    /// without every other client paying for it.
    pub hdhr_output_profile_id: Option<Id>,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            default_user_agent: Some(1),
            default_stream_profile: Some(3),
            m3u_hash_key: "url".into(),
            hdhr_output_profile_id: None,
        }
    }
}

impl Group for StreamSettings {
    const KEY: &'static str = "stream_settings";
    const NAME: &'static str = "Stream Settings";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    /// Seconds a stream profile's command may report a pace below
    /// `buffering_speed` before the session moves to the next source. Not a
    /// no-data timeout: a source that stops sending is the engine's fixed
    /// `stream_timeout`. Fifteen: a pace that dips for a few seconds is
    /// normal, and failing over to the next stream costs the viewer a
    /// reconnect.
    pub buffering_timeout: u32,
    pub buffering_speed: f64,
    /// Ring retention. 90 s would be ~90 MB per channel at 8 Mbps — one viewer
    /// exceeding this project's entire budget. 15 s serves a client joining a
    /// few seconds behind live, and `ring_max_bytes` caps it regardless of
    /// bitrate.
    pub ring_seconds: u32,
    /// Hard byte ceiling. Defaults to what `ring_seconds` of the fattest
    /// source a home instance carries needs — an ATSC mux off a LAN
    /// HDHomeRun, ~20 Mbps — because a cap chosen independently of the
    /// duration silently shortens retention on exactly those channels.
    pub ring_max_bytes: u64,
    /// Grace period after the last client leaves, so a channel surf back does
    /// not restart ffmpeg. Zero by default: a provider connection held open
    /// for a viewer who has gone is a slot off a stream limit they pay for,
    /// and the operator can trade that back if their provider is slow to
    /// connect.
    pub channel_shutdown_delay: u32,
    pub channel_init_grace_period: u32,
    pub channel_client_wait_period: u32,
    /// How far behind live a joining client starts, to have something buffered
    /// before the first read.
    pub new_client_behind_seconds: u32,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            buffering_timeout: 15,
            buffering_speed: 1.0,
            ring_seconds: 15,
            ring_max_bytes: 20_000_000 / 8 * 15,
            channel_shutdown_delay: 0,
            channel_init_grace_period: 60,
            channel_client_wait_period: 5,
            new_client_behind_seconds: 5,
        }
    }
}

impl Group for ProxySettings {
    const KEY: &'static str = "proxy_settings";
    const NAME: &'static str = "Proxy Settings";
}

/// CIDR allowlists per endpoint class. An absent or empty entry allows
/// everything, which is what an unconfigured install must do.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NetworkAccess(pub BTreeMap<String, String>);

impl Group for NetworkAccess {
    const KEY: &'static str = "network_access";
    const NAME: &'static str = "Network Access";
}

impl NetworkAccess {
    /// Whether this endpoint has any usable rule at all.
    ///
    /// Callers need this separately from [`Self::allows`]: when the client
    /// address cannot be determined, an unrestricted endpoint must still work
    /// while a restricted one must deny, and `allows` has no address to judge.
    pub fn is_restricted(&self, endpoint: &str) -> bool {
        self.0
            .get(endpoint)
            .is_some_and(|list| list.split(',').any(|entry| !entry.trim().is_empty()))
    }

    pub fn allows(&self, endpoint: &str, client: IpAddr) -> bool {
        if !self.is_restricted(endpoint) {
            return true;
        }

        for entry in self.0[endpoint].split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            // An unparseable entry is dropped rather than treated as deny-all:
            // locking the admin out of the UI that would let them fix it is
            // the worse failure. `patch_by_key` refuses to store one.
            match entry.parse::<Cidr>() {
                Ok(cidr) if cidr.contains(client) => return true,
                Ok(_) => {}
                Err(_) => tracing::warn!(endpoint, entry, "ignoring unparseable CIDR"),
            }
        }
        false
    }

    /// Every entry that would be ignored at request time, so the API can
    /// reject a bad save instead of silently widening access.
    pub fn invalid_entries(&self) -> Vec<(String, String)> {
        self.0
            .iter()
            .flat_map(|(endpoint, list)| {
                list.split(',')
                    .map(str::trim)
                    .filter(|e| !e.is_empty() && e.parse::<Cidr>().is_err())
                    .map(|e| (endpoint.clone(), e.to_owned()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemSettings {
    /// Biases EPG matching towards one country's guide channels. Unset by
    /// default: a region picked for us decides which guide a channel gets,
    /// and getting that wrong is a channel showing someone else's listings —
    /// harder to notice than a channel showing none.
    pub preferred_region: Option<String>,
    /// `system_event` is a rolling window; older rows are trimmed past this.
    pub max_system_events: u32,
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            preferred_region: None,
            max_system_events: 100,
        }
    }
}

impl Group for SystemSettings {
    const KEY: &'static str = "system_settings";
    const NAME: &'static str = "System Settings";
}

/// How the lineup hands out channel numbers: the range is the group's and
/// appends on a grid, which keeps a curated lineup stable as a provider grows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NumberingSettings {
    /// Width of the range a group is given when one is assigned for it.
    pub group_block_size: f64,
    /// Spacing between the numbers a range hands out; 1 is plain append. Ten
    /// leaves nine free slots between neighbours for the channel that arrives
    /// later and belongs between two others.
    pub channel_step: f64,
}

impl Default for NumberingSettings {
    fn default() -> Self {
        Self {
            group_block_size: 100.0,
            channel_step: 1.0,
        }
    }
}

impl Group for NumberingSettings {
    const KEY: &'static str = "numbering_settings";
    const NAME: &'static str = "Numbering";
}

impl NumberingSettings {
    /// Field-level reasons the values cannot be used, empty when they can.
    ///
    /// A step below one would divide the grid into nothing; a block narrower
    /// than the step is a range with no slot in it.
    pub fn problems(&self) -> Vec<(&'static str, String)> {
        let mut problems = Vec::new();
        if !self.channel_step.is_finite() || self.channel_step < 1.0 {
            problems.push(("channel_step", "must be a number of at least 1".to_owned()));
        }
        if !self.group_block_size.is_finite() || self.group_block_size < 1.0 {
            problems.push((
                "group_block_size",
                "must be a number of at least 1".to_owned(),
            ));
        } else if self.channel_step.is_finite() && self.group_block_size < self.channel_step {
            problems.push((
                "group_block_size",
                "must be at least the channel step, or a range holds no slot".to_owned(),
            ));
        }
        problems
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpgSettings {
    /// Whether a scheduled refresh may assign guide data to channels that have
    /// none.
    ///
    /// Off, because this should only ever happen when a user asks for it. On, a
    /// timer rewrites `epg_data_id` across the catalogue every refresh
    /// interval, unattended, leaving a count in a job row as the only record of
    /// what it decided — and a wrong guide on a channel is harder to notice
    /// than no guide at all. `POST /api/epg/match/` runs it on demand.
    pub epg_auto_match_on_refresh: bool,
    /// Stripped from channel and guide names before fuzzy matching, so
    /// "US: VRIX" reaches "VRIX". "HD" and its kind need no entry: the matcher
    /// drops them on its own.
    pub epg_match_ignore_prefixes: Vec<String>,
    pub epg_match_ignore_suffixes: Vec<String>,
    pub epg_match_ignore_custom: Vec<String>,
}

impl Group for EpgSettings {
    const KEY: &'static str = "epg_settings";
    const NAME: &'static str = "EPG Settings";
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::MIGRATOR.run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn seeded_rows_match_the_rust_defaults() {
        let pool = pool().await;
        assert_eq!(
            load::<StreamSettings>(&pool).await.unwrap(),
            StreamSettings::default()
        );
        assert_eq!(
            load::<ProxySettings>(&pool).await.unwrap(),
            ProxySettings::default()
        );
        assert_eq!(
            load::<SystemSettings>(&pool).await.unwrap(),
            SystemSettings::default()
        );
        assert_eq!(
            load::<EpgSettings>(&pool).await.unwrap(),
            EpgSettings::default()
        );
        assert_eq!(
            load::<NetworkAccess>(&pool).await.unwrap(),
            NetworkAccess::default()
        );
    }

    /// Every behaviour-changing default, beside its reason.
    ///
    /// The test above compares our migration to our struct, so both being
    /// wrong the same way passes it. This lists each value with the reason it
    /// has, so a change to one is a change to the other.
    #[test]
    fn behaviour_changing_defaults_each_have_a_reason() {
        let stream = StreamSettings::default();
        let proxy = ProxySettings::default();
        let system = SystemSettings::default();
        let epg = EpgSettings::default();

        // Pinned, so nothing here drifts silently.
        assert_eq!(stream.hdhr_output_profile_id, None);
        assert_eq!(proxy.buffering_timeout, 15);
        assert_eq!(proxy.buffering_speed, 1.0);
        assert_eq!(proxy.channel_shutdown_delay, 0);
        assert_eq!(proxy.channel_init_grace_period, 60);
        assert_eq!(proxy.channel_client_wait_period, 5);
        assert_eq!(proxy.new_client_behind_seconds, 5);
        assert_eq!(system.max_system_events, 100);
        assert_eq!(system.preferred_region, None);
        let numbering = NumberingSettings::default();
        assert_eq!(numbering.group_block_size, 100.0);
        assert_eq!(numbering.channel_step, 1.0);
        assert!(numbering.problems().is_empty());
        assert!(epg.epg_match_ignore_prefixes.is_empty());
        assert!(epg.epg_match_ignore_suffixes.is_empty());
        assert!(epg.epg_match_ignore_custom.is_empty());

        // Each of these has a reason of its own.

        // An empty key selects no fields, so every stream in an account hashes
        // identically and one refresh marks the whole catalogue stale.
        // `ingest::m3u` refuses to write when it sees that, and a fresh install
        // should not have to meet the refusal.
        assert_eq!(stream.m3u_hash_key, "url");
        assert_ne!(stream.m3u_hash_key, "");

        // The migration seeds a user agent and a stream profile, so a fresh
        // install plays before anyone opens Settings.
        assert_eq!(stream.default_user_agent, Some(1));
        assert_eq!(stream.default_stream_profile, Some(3));

        // 90 seconds of ring per channel would be ~90 MB at 8 Mbps for one
        // viewer — more than this project's whole memory budget.
        // `ring_max_bytes` caps the trade at whatever 15 seconds of the fattest
        // source here needs.
        assert_eq!(proxy.ring_seconds, 15);
        assert_eq!(proxy.ring_max_bytes, 20_000_000 / 8 * 15);

        // Matching runs only when asked; a scheduled path that rewrote guide
        // assignments unattended is opt-in.
        assert!(!epg.epg_auto_match_on_refresh);
    }

    #[tokio::test]
    async fn patch_leaves_untouched_fields_alone() {
        let pool = pool().await;
        let patched: ProxySettings = patch(&pool, &serde_json::json!({"ring_seconds": 30}))
            .await
            .unwrap();

        assert_eq!(patched.ring_seconds, 30);
        assert_eq!(
            patched.channel_shutdown_delay,
            ProxySettings::default().channel_shutdown_delay
        );
        assert_eq!(load::<ProxySettings>(&pool).await.unwrap(), patched);
    }

    #[tokio::test]
    async fn a_group_missing_fields_still_loads() {
        let pool = pool().await;
        sqlx::query("UPDATE core_setting SET value = ? WHERE key = ?")
            .bind(r#"{"ring_seconds": 7}"#)
            .bind(ProxySettings::KEY)
            .execute(&pool)
            .await
            .unwrap();

        let loaded = load::<ProxySettings>(&pool).await.unwrap();
        assert_eq!(loaded.ring_seconds, 7);
        assert_eq!(
            loaded.buffering_timeout,
            ProxySettings::default().buffering_timeout
        );
    }

    #[tokio::test]
    async fn an_explicit_null_resets_a_field_to_its_default() {
        let pool = pool().await;
        patch::<StreamSettings>(&pool, &serde_json::json!({"m3u_hash_key": "url,tvg_id"}))
            .await
            .unwrap();

        // Clearing the box in the UI must reset rather than store "", which
        // selects no fields at all and re-hashes the whole catalogue to the
        // same value.
        let cleared: StreamSettings = patch(&pool, &serde_json::json!({"m3u_hash_key": null}))
            .await
            .unwrap();
        assert_eq!(cleared.m3u_hash_key, StreamSettings::default().m3u_hash_key);

        let cleared: StreamSettings =
            patch(&pool, &serde_json::json!({"hdhr_output_profile_id": null}))
                .await
                .unwrap();
        assert_eq!(cleared.hdhr_output_profile_id, None);
    }

    #[tokio::test]
    async fn a_network_access_patch_never_drops_an_endpoint_it_did_not_send() {
        let pool = pool().await;
        patch_by_key(
            &pool,
            NetworkAccess::KEY,
            &serde_json::json!({"UI": "10.0.0.0/8", "STREAMS": "192.168.0.0/16"}),
        )
        .await
        .unwrap();

        // An absent endpoint allows every address, so replacing rather than
        // merging here would open the admin UI to the internet.
        patch_by_key(
            &pool,
            NetworkAccess::KEY,
            &serde_json::json!({"STREAMS": "172.16.0.0/12"}),
        )
        .await
        .unwrap();

        let access = load::<NetworkAccess>(&pool).await.unwrap();
        assert_eq!(access.0.get("UI").map(String::as_str), Some("10.0.0.0/8"));
        assert_eq!(
            access.0.get("STREAMS").map(String::as_str),
            Some("172.16.0.0/12")
        );
        assert!(!access.allows("UI", "203.0.113.1".parse().unwrap()));

        // Explicitly removing an endpoint is still possible, and is the only
        // way to stop restricting one.
        patch_by_key(&pool, NetworkAccess::KEY, &serde_json::json!({"UI": null}))
            .await
            .unwrap();
        let access = load::<NetworkAccess>(&pool).await.unwrap();
        assert!(!access.0.contains_key("UI"));
        assert!(access.allows("UI", "203.0.113.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn a_malformed_cidr_is_rejected_before_it_reaches_the_database() {
        let pool = pool().await;
        patch_by_key(
            &pool,
            NetworkAccess::KEY,
            &serde_json::json!({"UI": "10.0.0.0/8"}),
        )
        .await
        .unwrap();

        let result = patch_by_key(
            &pool,
            NetworkAccess::KEY,
            &serde_json::json!({"STREAMS": "10.0.0.0/8,garbage"}),
        )
        .await;
        assert!(matches!(result, Err(crate::Error::Invalid(_))));

        let access = load::<NetworkAccess>(&pool).await.unwrap();
        assert_eq!(access.0.get("UI").map(String::as_str), Some("10.0.0.0/8"));
        assert!(!access.0.contains_key("STREAMS"), "a rejected save landed");
    }

    #[tokio::test]
    async fn unreadable_json_falls_back_rather_than_erroring() {
        let pool = pool().await;
        sqlx::query("UPDATE core_setting SET value = ? WHERE key = ?")
            .bind("not json at all")
            .bind(EpgSettings::KEY)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(
            load::<EpgSettings>(&pool).await.unwrap(),
            EpgSettings::default()
        );
    }

    #[test]
    fn an_unconfigured_endpoint_allows_everyone() {
        let access = NetworkAccess::default();
        assert!(access.allows("UI", "203.0.113.9".parse().unwrap()));
        assert!(!access.is_restricted("UI"));
    }

    #[test]
    fn an_empty_list_does_not_count_as_a_restriction() {
        let access = NetworkAccess(BTreeMap::from([
            ("UI".to_owned(), String::new()),
            ("STREAMS".to_owned(), " , ".to_owned()),
        ]));
        assert!(!access.is_restricted("UI"));
        assert!(!access.is_restricted("STREAMS"));
        assert!(access.allows("UI", "203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn a_configured_endpoint_allows_only_listed_networks() {
        let access = NetworkAccess(BTreeMap::from([(
            "UI".to_owned(),
            "10.0.0.0/8, 192.168.1.0/24".to_owned(),
        )]));

        assert!(access.allows("UI", "10.9.9.9".parse().unwrap()));
        assert!(access.allows("UI", "192.168.1.7".parse().unwrap()));
        assert!(!access.allows("UI", "192.168.2.7".parse().unwrap()));
        assert!(access.allows("STREAMS", "192.168.2.7".parse().unwrap()));
    }

    #[test]
    fn garbage_entries_are_reported_not_silently_denying() {
        let access = NetworkAccess(BTreeMap::from([(
            "UI".to_owned(),
            "10.0.0.0/8,not-a-cidr".to_owned(),
        )]));

        assert!(access.allows("UI", "10.0.0.1".parse().unwrap()));
        assert_eq!(
            access.invalid_entries(),
            vec![("UI".to_owned(), "not-a-cidr".to_owned())]
        );
    }
}
