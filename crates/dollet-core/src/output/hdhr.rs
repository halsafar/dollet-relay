//! HDHomeRun emulation payloads: `discover.json`, `lineup.json`,
//! `lineup_status.json`, and `device.xml`.
//!
//! This is the primary use case — Plex consumes these — so field names, types,
//! and the hardware-describing string constants are pinned by the snapshots.
//!
//! Plex keys a configured tuner on `DeviceID`. The unscoped `/hdhr/` tuner uses
//! a constant, so a paired tuner survives every restart and upgrade; a
//! channel-profile or output-profile tuner derives its own from the profile, so
//! each scope is a distinct device to Plex, and renaming a profile means
//! re-adding its tuner once.

use serde::Serialize;

use crate::domain::{EffectiveChannel, Id};

use super::format_channel_number;

/// Values Plex reads but that describe no real hardware. Constants, because a
/// tuner already paired against them must keep working.
const MODEL_NUMBER: &str = "HDTC-2US";
const FIRMWARE_NAME: &str = "hdhomerun3_atsc";
const FIRMWARE_VERSION: &str = "20200101";
const DEVICE_AUTH: &str = "test_auth_token";

/// Device id for the unscoped `/hdhr/` tuner. A constant, because an existing
/// Plex install has it stored.
const DEFAULT_DEVICE_ID: &str = "12345678";

/// Name and id for one tuner. A channel profile or output profile in the path
/// makes a distinct tuner, so each needs its own stable identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub friendly_name: String,
    pub device_id: String,
}

impl Identity {
    pub fn new(channel_profile: Option<&str>, output_profile_id: Option<Id>) -> Self {
        let mut parts: Vec<String> = Vec::new();
        if let Some(profile) = channel_profile {
            parts.push(profile.to_string());
        }
        if let Some(id) = output_profile_id {
            parts.push(id.to_string());
        }
        if parts.is_empty() {
            return Self {
                friendly_name: "Dollet HDHomeRun".into(),
                device_id: DEFAULT_DEVICE_ID.into(),
            };
        }
        Self {
            friendly_name: format!("Dollet HDHomeRun - {}", parts.join(" / ")),
            device_id: format!("dollet-hdhr-{}", parts.join("-")),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub struct Discover {
    pub friendly_name: String,
    pub model_number: &'static str,
    pub firmware_name: &'static str,
    pub firmware_version: &'static str,
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    pub device_auth: &'static str,
    #[serde(rename = "BaseURL")]
    pub base_url: String,
    #[serde(rename = "LineupURL")]
    pub lineup_url: String,
    pub tuner_count: u32,
}

/// `base_url` is the absolute tuner root, e.g. `http://host:9191/hdhr`, with no
/// trailing slash. It must be absolute and must reflect `X-Forwarded-*`, or
/// discovery succeeds and playback fails.
pub fn discover(base_url: &str, identity: &Identity, tuner_count: u32) -> Discover {
    Discover {
        friendly_name: identity.friendly_name.clone(),
        model_number: MODEL_NUMBER,
        firmware_name: FIRMWARE_NAME,
        firmware_version: FIRMWARE_VERSION,
        device_id: identity.device_id.clone(),
        device_auth: DEVICE_AUTH,
        base_url: base_url.to_string(),
        lineup_url: format!("{base_url}/lineup.json"),
        tuner_count,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LineupEntry {
    #[serde(rename = "GuideNumber")]
    pub guide_number: String,
    #[serde(rename = "GuideName")]
    pub guide_name: String,
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "Guide_ID")]
    pub guide_id: String,
    #[serde(rename = "Station")]
    pub station: String,
}

/// Channels with no formattable number are skipped entirely: `GuideNumber` is
/// how Plex addresses a channel, and an empty one makes the entry unplayable.
/// So are channels flagged `hidden_from_output` — the query excludes those too,
/// and a caller rewriting that query is the likely place for a leak.
///
/// `stream_base_url` is the absolute origin, e.g. `http://host:9191`.
pub fn lineup(
    stream_base_url: &str,
    channels: &[EffectiveChannel],
    output_profile_id: Option<Id>,
) -> Vec<LineupEntry> {
    let query = output_profile_id.map_or(String::new(), |id| format!("?output_profile={id}"));
    channels
        .iter()
        .filter(|ch| !ch.hidden_from_output)
        .filter_map(|ch| {
            let number = format_channel_number(ch.channel_number)?;
            Some(LineupEntry {
                url: format!("{stream_base_url}/proxy/ts/stream/{}{query}", ch.uuid),
                guide_name: ch.name.clone(),
                guide_id: number.clone(),
                station: number.clone(),
                guide_number: number,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LineupStatus {
    #[serde(rename = "ScanInProgress")]
    pub scan_in_progress: u8,
    #[serde(rename = "ScanPossible")]
    pub scan_possible: u8,
    #[serde(rename = "Source")]
    pub source: &'static str,
    #[serde(rename = "SourceList")]
    pub source_list: Vec<&'static str>,
}

/// Constant: this tuner never scans, and reporting otherwise makes Plex offer a
/// scan button that cannot work.
pub fn lineup_status() -> LineupStatus {
    LineupStatus {
        scan_in_progress: 0,
        scan_possible: 0,
        source: "Cable",
        source_list: vec!["Cable"],
    }
}

/// UPnP-style device description. The indentation is pinned by the snapshot so
/// the comparison stays a byte comparison; no client cares about the whitespace.
pub fn device_xml(base_url: &str, identity: &Identity) -> String {
    let name = super::escape_xml(&identity.friendly_name);
    let id = super::escape_xml(&identity.device_id);
    let url = super::escape_xml(base_url);
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
        <root>
            <DeviceID>{id}</DeviceID>
            <FriendlyName>{name}</FriendlyName>
            <ModelNumber>{MODEL_NUMBER}</ModelNumber>
            <FirmwareName>{FIRMWARE_NAME}</FirmwareName>
            <FirmwareVersion>{FIRMWARE_VERSION}</FirmwareVersion>
            <DeviceAuth>{DEVICE_AUTH}</DeviceAuth>
            <BaseURL>{url}</BaseURL>
            <LineupURL>{url}/lineup.json</LineupURL>
        </root>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserLevel;
    use uuid::Uuid;

    fn channel(id: i64, name: &str, number: Option<f64>) -> EffectiveChannel {
        EffectiveChannel {
            id,
            uuid: Uuid::from_u128(id as u128),
            channel_number: number,
            name: name.into(),
            channel_group_id: Some(1),
            created_at: chrono::DateTime::UNIX_EPOCH,
            group_name: Some("News".into()),
            logo_url: None,
            tvg_id: None,
            tvc_guide_stationid: None,
            epg_data_id: None,
            stream_profile_id: None,
            user_level: UserLevel::Streamer,
            is_adult: false,
            hidden_from_output: false,
            is_catchup: false,
            catchup_days: 0,
        }
    }

    fn sample() -> Vec<EffectiveChannel> {
        vec![
            channel(1, "Channel A", Some(101.0)),
            channel(2, "Channel B", Some(102.5)),
            channel(3, "Unnumbered", None),
            channel(4, "Tom & Jerry", Some(4.0)),
        ]
    }

    #[test]
    fn discover_json() {
        let identity = Identity::new(None, None);
        insta::assert_json_snapshot!(discover("http://ipx.test/hdhr", &identity, 4));
    }

    #[test]
    fn discover_json_for_a_scoped_tuner() {
        let identity = Identity::new(Some("Living Room"), Some(3));
        insta::assert_json_snapshot!(discover(
            "http://ipx.test/hdhr/Living%20Room/output_profile/3",
            &identity,
            10
        ));
    }

    #[test]
    fn identity_covers_each_scope_combination() {
        let cases = [
            (None, None, "Dollet HDHomeRun", "12345678"),
            (
                Some("Kids"),
                None,
                "Dollet HDHomeRun - Kids",
                "dollet-hdhr-Kids",
            ),
            (None, Some(3), "Dollet HDHomeRun - 3", "dollet-hdhr-3"),
            (
                Some("Kids"),
                Some(3),
                "Dollet HDHomeRun - Kids / 3",
                "dollet-hdhr-Kids-3",
            ),
        ];
        for (profile, output, name, id) in cases {
            let got = Identity::new(profile, output);
            assert_eq!(
                got,
                Identity {
                    friendly_name: name.into(),
                    device_id: id.into()
                }
            );
        }
    }

    #[test]
    fn lineup_json() {
        insta::assert_json_snapshot!(lineup("http://ipx.test", &sample(), None));
    }

    #[test]
    fn lineup_json_with_an_output_profile() {
        insta::assert_json_snapshot!(lineup("http://ipx.test", &sample(), Some(3)));
    }

    #[test]
    fn channels_hidden_from_output_are_skipped() {
        let mut channels = sample();
        channels[0].hidden_from_output = true;
        let entries = lineup("http://ipx.test", &channels, None);
        assert!(
            entries.iter().all(|e| e.guide_name != "Channel A"),
            "{entries:?}"
        );
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn numberless_channels_are_skipped() {
        let entries = lineup("http://ipx.test", &sample(), None);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.guide_name != "Unnumbered"));
        // GuideNumber, Guide_ID, and Station are always the same string.
        for entry in &entries {
            assert_eq!(entry.guide_number, entry.guide_id);
            assert_eq!(entry.guide_number, entry.station);
        }
    }

    #[test]
    fn an_empty_lineup_serializes_as_an_empty_array() {
        assert_eq!(
            serde_json::to_string(&lineup("http://ipx.test", &[], None)).unwrap(),
            "[]"
        );
    }

    #[test]
    fn lineup_status_json() {
        insta::assert_json_snapshot!(lineup_status());
    }

    #[test]
    fn device_xml_document() {
        insta::assert_snapshot!(device_xml(
            "http://ipx.test/hdhr",
            &Identity::new(None, None)
        ));
    }

    #[test]
    fn device_xml_escapes_its_interpolations() {
        let identity = Identity::new(Some("A & B"), None);
        let xml = device_xml("http://ipx.test/hdhr/A%20&%20B", &identity);
        assert!(
            xml.contains("<FriendlyName>Dollet HDHomeRun - A &amp; B</FriendlyName>"),
            "{xml}"
        );
        assert!(
            xml.contains("<DeviceID>dollet-hdhr-A &amp; B</DeviceID>"),
            "{xml}"
        );
        assert!(
            xml.contains("<BaseURL>http://ipx.test/hdhr/A%20&amp;%20B</BaseURL>"),
            "{xml}"
        );
    }
}
