//! Wire-format snapshots: our serializers against the bytes the outputs serve.
//!
//! These sit above any coverage number, and the reason shows in what they
//! catch: a fully covered serializer can still emit a field spelled
//! differently, in an order no client expects, and Plex or an Xtream player is
//! what notices.
//!
//! The corpus is described in `fixtures/golden/README.md`. Each snapshot is
//! reconstructible from its own bytes — a lineup entry carries the channel
//! number, name and UUID it was built from — so most of these run against
//! nothing but the serializers. The end-to-end path from `sample.sql` through
//! the query layer is at the bottom.
//!
//! A snapshot is compared whole. Nothing here tolerates a known difference, so
//! a snapshot that no longer matches is either a deliberate change to what
//! clients receive, to be reviewed and re-pinned, or a regression.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use dollet_core::domain::{ChannelGroup, EffectiveChannel, UserLevel};
use dollet_core::output::{self, TvgIdSource};
use dollet_core::parse;
use serde_json::Value as Json;
use uuid::Uuid;

/// Host every snapshot was requested with, so absolute URLs are stable.
const BASE_URL: &str = "http://ipx.test:9191";

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden")
}

fn golden(name: &str) -> String {
    let path = golden_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn golden_json(name: &str) -> Json {
    serde_json::from_str(&golden(name)).expect("golden file is JSON")
}

fn to_json<T: serde::Serialize>(value: &T) -> Json {
    serde_json::to_value(value).expect("serializes")
}

/// A channel with every field unset, for tests that fill in only what they use.
fn blank_channel(id: i64) -> EffectiveChannel {
    EffectiveChannel {
        id,
        uuid: Uuid::nil(),
        channel_number: None,
        name: String::new(),
        channel_group_id: None,
        group_name: None,
        created_at: chrono::DateTime::UNIX_EPOCH,
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

/// Rebuild the channel table the snapshots were rendered from, by joining two
/// of them.
///
/// Neither alone is enough and the pairing is deliberate. Names come from the
/// Xtream JSON, which escapes them properly; the M3U has no escaping rule, so
/// a quote in a name is written as `&quot;` and read back that way. Numbers
/// come from the M3U, which prints them unflattened, where Xtream's `num` is
/// the collision-free integer our own allocator derives — feeding that back in
/// would prove nothing.
///
/// Both snapshots are ordered by effective channel number and both exclude
/// hidden channels, so they line up positionally.
fn reconstructed_channels() -> Vec<EffectiveChannel> {
    let playlist = parse::m3u::parse_str(&golden("output-m3u.m3u"));
    let live = golden_json("xc-get-live-streams.json");

    // `?tvg_id_source=tvg_id` is the only snapshot that prints the stored
    // tvg_id; everywhere else the channel number stands in for it.
    let by_tvg_id: BTreeMap<Uuid, String> = parse::m3u::parse_str(&golden("output-m3u-tvg-id.m3u"))
        .entries
        .iter()
        .filter_map(|entry| {
            let uuid = Uuid::parse_str(entry.url.rsplit('/').next()?).ok()?;
            Some((uuid, entry.attr("tvg-id")?.to_string()))
        })
        .collect();
    let rows = live.as_array().expect("an array");
    assert_eq!(
        playlist.entries.len(),
        rows.len(),
        "the two snapshots must describe the same channels"
    );

    playlist
        .entries
        .iter()
        .zip(rows)
        .map(|(entry, row)| {
            let uuid = entry
                .url
                .rsplit('/')
                .next()
                .and_then(|s| Uuid::parse_str(s.split('?').next().unwrap_or(s)).ok())
                .expect("a UUID in the stream URL");
            EffectiveChannel {
                id: row["stream_id"].as_i64().expect("stream_id"),
                uuid,
                channel_number: entry.tvg_chno(),
                name: row["name"].as_str().expect("name").to_string(),
                channel_group_id: row["category_id"].as_str().and_then(|c| c.parse().ok()),
                group_name: entry.group_title().map(str::to_string),
                created_at: chrono::DateTime::from_timestamp(
                    row["added"]
                        .as_str()
                        .expect("added")
                        .parse()
                        .expect("a timestamp"),
                    0,
                )
                .expect("a valid timestamp"),
                logo_url: entry.tvg_logo().map(str::to_string),
                // From the snapshot that prints the real tvg_id: the default
                // playlist puts the channel number there instead.
                tvg_id: by_tvg_id.get(&uuid).cloned(),
                tvc_guide_stationid: entry.attr("tvc-guide-stationid").map(str::to_string),
                is_adult: row["is_adult"].as_i64() == Some(1),
                ..blank_channel(row["stream_id"].as_i64().expect("stream_id"))
            }
        })
        .collect()
}

/// Line by line, so a failure names the first line that differs rather than
/// printing two whole documents.
fn assert_lines_match(ours: &str, snapshot: &str, what: &str) {
    for (index, (mine, theirs)) in ours.lines().zip(snapshot.lines()).enumerate() {
        assert_eq!(
            mine,
            theirs,
            "{what}: line {} differs from the snapshot",
            index + 1
        );
    }
    assert_eq!(
        ours.lines().count(),
        snapshot.lines().count(),
        "{what}: line count differs from the snapshot"
    );
    assert_eq!(ours, snapshot, "{what}");
}

fn m3u_options<'a>(epg_url: &'a str, source: TvgIdSource) -> output::m3u::M3uOptions<'a> {
    output::m3u::M3uOptions {
        base_url: BASE_URL,
        epg_url,
        tvg_id_source: source,
        stream_query: &[],
        xtream_credentials: None,
    }
}

// --- /output/m3u -------------------------------------------------------------

fn m3u_entries(channels: &[EffectiveChannel]) -> Vec<output::m3u::M3uChannel<'_>> {
    channels
        .iter()
        .map(output::m3u::M3uChannel::proxied)
        .collect()
}

#[test]
fn m3u_playlist_matches_the_snapshot() {
    let snapshot = golden("output-m3u.m3u");
    let channels = reconstructed_channels();
    assert_eq!(
        channels.len(),
        16,
        "the lineup plus the hard cases, less the hidden one"
    );

    let epg_url = parse::m3u::parse_str(&snapshot).header["x-tvg-url"].clone();
    let ours = output::m3u::render(
        &m3u_options(&epg_url, TvgIdSource::ChannelNumber),
        &m3u_entries(&channels),
    );
    assert_lines_match(&ours, &snapshot, "output-m3u.m3u");
}

#[test]
fn m3u_carries_a_fractional_and_a_missing_channel_number() {
    // Both are absent from the sample instance, so without the adversarial
    // rows the harness could not tell the number formatter from a plain
    // integer cast.
    let snapshot = golden("output-m3u.m3u");
    assert!(
        snapshot.contains(r#"tvg-chno="901.5""#),
        "a fractional number must print unrounded"
    );
    // No number at all: an empty tvg-chno and the row id standing in for tvg-id.
    assert!(snapshot.contains(r#"tvg-id="902" tvg-name="No Number" tvg-logo="" tvg-chno=""#));
}

#[test]
fn a_channel_hidden_from_output_appears_in_no_snapshot() {
    // The corpus carries a hidden channel so the filter is exercised, not
    // assumed.
    for (name, _) in snapshots() {
        assert!(
            !golden(&name).contains("Hidden Channel"),
            "{name} leaks a channel marked hidden_from_output"
        );
    }

    // And our own serializers drop it rather than relying on the query.
    let mut channels = reconstructed_channels();
    let mut hidden = channels[0].clone();
    hidden.id = 903;
    hidden.name = "Hidden Channel".into();
    hidden.hidden_from_output = true;
    channels.push(hidden);

    let epg_url = "http://ipx.test:9191/output/epg";
    let rendered = output::m3u::render(
        &m3u_options(epg_url, TvgIdSource::ChannelNumber),
        &m3u_entries(&channels),
    );
    assert!(!rendered.contains("Hidden Channel"));
    assert!(
        !to_json(&output::hdhr::lineup(BASE_URL, &channels, None))
            .to_string()
            .contains("Hidden Channel")
    );
    assert!(
        !to_json(&output::xc::live_streams(
            &channels,
            &output::xc::LiveStreamOptions {
                default_group_id: 1,
                catchup_allowed: true,
            },
        ))
        .to_string()
        .contains("Hidden Channel")
    );
}

#[test]
fn m3u_playlist_by_tvg_id_matches_the_snapshot() {
    let snapshot = golden("output-m3u-tvg-id.m3u");
    let channels = reconstructed_channels();
    let epg_url = parse::m3u::parse_str(&snapshot).header["x-tvg-url"].clone();
    let ours = output::m3u::render(
        &m3u_options(&epg_url, TvgIdSource::TvgId),
        &m3u_entries(&channels),
    );
    assert_lines_match(&ours, &snapshot, "output-m3u-tvg-id.m3u");
}

#[test]
fn m3u_playlist_with_an_output_profile_matches_the_snapshot() {
    let snapshot = golden("output-m3u-output-profile.m3u");
    let channels = reconstructed_channels();
    let epg_url = parse::m3u::parse_str(&snapshot).header["x-tvg-url"].clone();
    let query = [("output_profile", "1".to_string())];
    let opts = output::m3u::M3uOptions {
        stream_query: &query,
        ..m3u_options(&epg_url, TvgIdSource::ChannelNumber)
    };
    let ours = output::m3u::render(&opts, &m3u_entries(&channels));
    assert_lines_match(&ours, &snapshot, "output-m3u-output-profile.m3u");
}

#[test]
fn m3u_playlist_with_direct_urls_matches_the_snapshot() {
    // The one snapshot whose stream URLs are the provider's rather than ours,
    // and so the only one that exercises the VLC multicast restore.
    let snapshot = golden("output-m3u-direct.m3u");
    let parsed = parse::m3u::parse_str(&snapshot);
    let channels = reconstructed_channels();

    let entries: Vec<_> = channels
        .iter()
        .zip(&parsed.entries)
        .map(|(channel, entry)| output::m3u::M3uChannel {
            channel,
            direct_url: Some(entry.url.as_str()),
        })
        .collect();
    let epg_url = parsed.header["x-tvg-url"].clone();
    let ours = output::m3u::render(&m3u_options(&epg_url, TvgIdSource::ChannelNumber), &entries);
    assert_lines_match(&ours, &snapshot, "output-m3u-direct.m3u");
}

#[test]
fn the_gracenote_playlist_uses_the_station_id_where_there_is_one() {
    // Only one channel carries a station id, so this is the assertion that the
    // option is read at all rather than silently falling through.
    let gracenote = golden("output-m3u-gracenote.m3u");
    let default = golden("output-m3u.m3u");
    assert!(gracenote.contains(r#"tvg-id="99999""#));
    assert!(!default.contains(r#"tvg-id="99999""#));

    let channels = reconstructed_channels();
    let epg_url = parse::m3u::parse_str(&gracenote).header["x-tvg-url"].clone();
    let ours = output::m3u::render(
        &m3u_options(&epg_url, TvgIdSource::Gracenote),
        &m3u_entries(&channels),
    );
    assert_lines_match(&ours, &gracenote, "output-m3u-gracenote.m3u");
}

#[test]
fn the_xtream_playlist_matches_the_snapshot() {
    // `get.php` addresses channels by row id under the caller's credentials
    // rather than by proxy UUID, which is the one place the M3U serializer
    // builds a different URL shape.
    let snapshot = golden("xc-get-php.m3u");
    let channels = reconstructed_channels();
    let epg_url = parse::m3u::parse_str(&snapshot).header["x-tvg-url"].clone();
    let opts = output::m3u::M3uOptions {
        xtream_credentials: Some(("fixtureadmin", "fixturepass")),
        ..m3u_options(&epg_url, TvgIdSource::ChannelNumber)
    };
    let ours = output::m3u::render(&opts, &m3u_entries(&channels));
    assert_lines_match(&ours, &snapshot, "xc-get-php.m3u");
}

// --- /output/epg -------------------------------------------------------------

/// Re-serialize a guide we parsed from the snapshot's own bytes.
///
/// The strongest check available for the XMLTV pair: the parser and the writer
/// run against every channel and every programme at once, and a single dropped
/// attribute anywhere shows up as a byte diff.
///
/// The counts are asserted: without them a snapshot with every `<programme>`
/// deleted round-trips cleanly over zero programmes.
fn xmltv_round_trip(name: &str, want_channels: usize, want_programmes: usize) {
    let snapshot = golden(name);
    let items: Vec<_> = parse::xmltv::from_bytes(snapshot.as_bytes())
        .expect("golden guide decodes")
        .map(|item| item.expect("golden guide parses"))
        .collect();

    let channels: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            parse::xmltv::XmltvItem::Channel(channel) => Some(channel),
            parse::xmltv::XmltvItem::Programme(_) => None,
        })
        .collect();
    let programmes: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            parse::xmltv::XmltvItem::Programme(programme) => Some(programme),
            parse::xmltv::XmltvItem::Channel(_) => None,
        })
        .collect();
    assert_eq!(channels.len(), want_channels, "{name} channels");
    assert_eq!(programmes.len(), want_programmes, "{name} programmes");

    let mut writer = output::xmltv::XmltvWriter::new(Vec::new());
    writer
        .start(&output::xmltv::XmltvOptions::default())
        .unwrap();
    for channel in &channels {
        writer
            .write_channel(
                &channel.tvg_id,
                channel.display_name.as_deref().unwrap_or_default(),
                channel.icon_url.as_deref().unwrap_or_default(),
            )
            .unwrap();
    }
    for programme in &programmes {
        let extra = programme.custom_properties.clone();
        writer
            .write_programme(&output::xmltv::Programme {
                channel_id: &programme.tvg_id,
                start: programme.start_time,
                stop: programme.end_time,
                title: &programme.title,
                sub_title: programme.sub_title.as_deref(),
                desc: programme.description.as_deref(),
                extra: Some(&extra),
            })
            .unwrap();
    }
    writer.finish().unwrap();
    let ours = String::from_utf8(writer.into_inner()).unwrap();
    assert_lines_match(&ours, &snapshot, name);
}

#[test]
fn xmltv_guide_round_trips_byte_for_byte() {
    xmltv_round_trip("output-epg.xml", 16, 116);
}

#[test]
fn xmltv_guide_by_tvg_id_round_trips_byte_for_byte() {
    xmltv_round_trip("output-epg-tvg-id.xml", 16, 116);
}

#[test]
fn xmltv_guide_by_gracenote_round_trips_byte_for_byte() {
    xmltv_round_trip("output-epg-gracenote.xml", 16, 116);
}

#[test]
fn xmltv_guide_for_one_day_round_trips_byte_for_byte() {
    xmltv_round_trip("output-epg-days-1.xml", 16, 56);
}

#[test]
fn the_xtream_guide_round_trips_too() {
    // Not a copy of `/output/epg`: an authenticated export renumbers every
    // channel to the collision-free integers Xtream clients require.
    xmltv_round_trip("xc-xmltv.xml", 16, 116);
    assert_ne!(golden("xc-xmltv.xml"), golden("output-epg.xml"));
}

#[test]
fn the_guide_carries_text_no_ascii_corpus_would_have_caught() {
    // Each is a mutation a plain-ASCII corpus cannot see: dropped attribute
    // escaping, `<sub-title>` never written, `<`/`>` unescaped, `write_extra`
    // skipped.
    let guide = golden("output-epg.xml");
    assert!(guide.contains("<title>Caf\u{e9} Concert &lt;Live&gt; &amp; Encore</title>"));
    assert!(guide.contains("<sub-title>Episode \u{e9}p\u{e9}e</sub-title>"));
    assert!(guide.contains("<category>M\u{fa}sica &amp; Arts</category>"));
    assert!(guide.contains(r#"<episode-num system="xmltv_ns">1.5.</episode-num>"#));
    assert!(guide.contains("<actor role=\"Lead\">Ann\u{e9}</actor>"));

    // And an attribute value carrying characters that must be escaped there.
    let by_tvg_id = golden("output-epg-tvg-id.xml");
    assert!(by_tvg_id.contains(r#"<channel id="test&amp;&lt;id&gt;.us">"#));
}

// --- HDHomeRun ---------------------------------------------------------------

/// Rebuild the channels a lineup snapshot was built from, from the lineup itself.
fn channels_from_lineup(lineup: &Json) -> Vec<EffectiveChannel> {
    lineup
        .as_array()
        .expect("lineup is an array")
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let url = entry["URL"].as_str().expect("URL");
            let uuid = url
                .rsplit('/')
                .next()
                .and_then(|s| Uuid::parse_str(s.split('?').next().unwrap_or(s)).ok())
                .expect("a UUID in the stream URL");
            EffectiveChannel {
                uuid,
                channel_number: entry["GuideNumber"].as_str().and_then(|n| n.parse().ok()),
                name: entry["GuideName"].as_str().unwrap_or_default().to_string(),
                ..blank_channel(i as i64 + 1)
            }
        })
        .collect()
}

#[test]
fn hdhr_lineup_matches_the_snapshot() {
    let snapshot = golden_json("hdhr-lineup.json");
    let channels = channels_from_lineup(&snapshot);
    assert_eq!(channels.len(), 15, "the numberless channel is skipped");

    let ours = output::hdhr::lineup(BASE_URL, &channels, None);
    assert_eq!(to_json(&ours), snapshot);
}

#[test]
fn hdhr_lineup_with_an_output_profile_matches_the_snapshot() {
    let snapshot = golden_json("hdhr-op1-lineup.json");
    let channels = channels_from_lineup(&snapshot);
    let ours = output::hdhr::lineup(BASE_URL, &channels, Some(1));
    assert_eq!(to_json(&ours), snapshot);
}

#[test]
fn an_unknown_output_profile_falls_back_to_no_transcoding() {
    // A tuner Plex has already added must keep answering, so an unknown id
    // serves the plain lineup rather than an error. The serializer takes an
    // already-resolved `Option`, so this pins the contract the caller has to
    // honour.
    assert_eq!(golden("hdhr-op99-lineup.json"), golden("hdhr-lineup.json"));
}

#[test]
fn an_unknown_channel_profile_yields_an_empty_lineup_not_an_error() {
    let snapshot = golden_json("hdhr-missing-cp-lineup.json");
    assert_eq!(snapshot, serde_json::json!([]));
    assert_eq!(
        to_json(&output::hdhr::lineup(BASE_URL, &[], None)),
        snapshot
    );
}

#[test]
fn a_channel_profile_lineup_carries_only_its_enabled_members() {
    // The sample instance has no channel profile at all, so without the
    // adversarial rows the profile-scoped paths could only be pinned against a
    // missing one.
    let snapshot = golden_json("hdhr-cp-lineup.json");
    let names: Vec<&str> = snapshot
        .as_array()
        .expect("an array")
        .iter()
        .map(|entry| entry["GuideName"].as_str().unwrap())
        .collect();
    // The profile's one real member is the lineup's first channel. Read from
    // the other snapshot rather than written down, so a re-pin edits one file,
    // not two that must agree.
    let lineup = golden_json("hdhr-lineup.json");
    let first = lineup[0]["GuideName"].as_str().expect("a first channel");

    assert_eq!(
        names,
        [first, "Test, \"Quoted\" & <Tagged>", "Fractional Number"],
        "the disabled member and the hidden channel are both absent"
    );

    let channels = channels_from_lineup(&snapshot);
    assert_eq!(
        to_json(&output::hdhr::lineup(BASE_URL, &channels, None)),
        snapshot
    );
}

#[test]
fn hdhr_lineup_status_matches_the_snapshot() {
    let snapshot = golden_json("hdhr-lineup-status.json");
    assert_eq!(to_json(&output::hdhr::lineup_status()), snapshot);

    // The payload is constant across every path variant.
    for name in [
        "hdhr-op1-lineup-status.json",
        "hdhr-cp-lineup-status.json",
        "hdhr-cp-op1-lineup-status.json",
    ] {
        assert_eq!(golden_json(name), snapshot, "{name}");
    }
}

#[test]
fn hdhr_discover_matches_the_snapshot() {
    let snapshot = golden_json("hdhr-discover.json");
    let identity = output::hdhr::Identity::new(None, None);
    let tuner_count = snapshot["TunerCount"].as_u64().expect("TunerCount") as u32;
    let ours = to_json(&output::hdhr::discover(
        &format!("{BASE_URL}/hdhr"),
        &identity,
        tuner_count,
    ));

    // Plex keys a paired tuner on this, so the unscoped id is a constant rather
    // than something derived from the name.
    assert_eq!(ours["DeviceID"], "12345678");
    assert_eq!(ours, snapshot);
}

#[test]
fn hdhr_discover_for_a_scoped_tuner_matches_the_snapshot() {
    let snapshot = golden_json("hdhr-op1-discover.json");
    let identity = output::hdhr::Identity::new(None, Some(1));
    let tuner_count = snapshot["TunerCount"].as_u64().unwrap() as u32;
    let ours = to_json(&output::hdhr::discover(
        &format!("{BASE_URL}/hdhr/output_profile/1"),
        &identity,
        tuner_count,
    ));

    // A scoped tuner is a distinct device to Plex, so its id has to differ from
    // the unscoped one and stay stable across restarts.
    assert_eq!(ours["DeviceID"], "dollet-hdhr-1");
    assert_eq!(ours, snapshot);
}

#[test]
fn hdhr_device_xml_matches_the_snapshot() {
    let snapshot = golden("hdhr-device.xml");
    let identity = output::hdhr::Identity::new(None, None);
    let ours = output::hdhr::device_xml(&format!("{BASE_URL}/hdhr"), &identity);
    assert_lines_match(&ours, &snapshot, "hdhr-device.xml");
}

// --- Xtream Codes ------------------------------------------------------------

#[test]
fn xc_live_categories_match_the_snapshot() {
    let snapshot = golden_json("xc-get-live-categories.json");
    let groups: Vec<ChannelGroup> = snapshot
        .as_array()
        .unwrap()
        .iter()
        .map(|c| ChannelGroup {
            id: c["category_id"].as_str().unwrap().parse().unwrap(),
            name: c["category_name"].as_str().unwrap().to_string(),
            number_start: None,
            number_end: None,
        })
        .collect();
    assert!(!groups.is_empty());

    assert_eq!(to_json(&output::xc::live_categories(&groups)), snapshot);
}

#[test]
fn xc_live_streams_match_the_snapshot() {
    let snapshot = golden_json("xc-get-live-streams.json");
    let rows = snapshot.as_array().expect("an array");
    assert_eq!(rows.len(), 16);

    let channels: Vec<EffectiveChannel> = rows
        .iter()
        .map(|row| EffectiveChannel {
            id: row["stream_id"].as_i64().unwrap(),
            // `num` is the collision-free integer our own allocator derives, so
            // feeding it back in would prove nothing. The real channel number is
            // recovered from the M3U snapshot, which carries it unflattened.
            channel_number: None,
            name: row["name"].as_str().unwrap().to_string(),
            channel_group_id: row["category_id"].as_str().unwrap().parse().ok(),
            created_at: chrono::DateTime::from_timestamp(
                row["added"].as_str().unwrap().parse().unwrap(),
                0,
            )
            .unwrap(),
            logo_url: row["stream_icon"].as_str().map(str::to_string),
            is_adult: row["is_adult"].as_i64() == Some(1),
            ..blank_channel(row["stream_id"].as_i64().unwrap())
        })
        .collect();

    // Take the channel numbers from the playlist snapshot, matched by name.
    let playlist = parse::m3u::parse_str(&golden("output-m3u.m3u"));
    let numbers: BTreeMap<&str, f64> = playlist
        .entries
        .iter()
        .filter_map(|e| Some((e.name.as_str(), e.tvg_chno()?)))
        .collect();
    let channels: Vec<EffectiveChannel> = channels
        .into_iter()
        .map(|c| EffectiveChannel {
            channel_number: numbers.get(c.name.as_str()).copied(),
            ..c
        })
        .collect();

    let ours = output::xc::live_streams(
        &channels,
        &output::xc::LiveStreamOptions {
            default_group_id: 1,
            catchup_allowed: true,
        },
    );
    assert_eq!(to_json(&ours), snapshot);
}

#[test]
fn xc_vod_and_series_actions_are_empty() {
    // Out of scope for 1.0. An Xtream client asks for these on every sign-in,
    // so they answer with an empty catalogue rather than an error.
    for name in [
        "xc-get-vod-categories.json",
        "xc-get-vod-streams.json",
        "xc-get-series-categories.json",
        "xc-get-series.json",
    ] {
        assert_eq!(golden_json(name), serde_json::json!([]), "{name}");
    }
    assert_eq!(
        to_json(&output::xc::vod_categories()),
        serde_json::json!([])
    );
    assert_eq!(to_json(&output::xc::vod_streams()), serde_json::json!([]));
    assert_eq!(
        to_json(&output::xc::series_categories()),
        serde_json::json!([])
    );
    assert_eq!(to_json(&output::xc::series()), serde_json::json!([]));
}

#[test]
fn xc_account_info_matches_the_snapshot_but_for_the_clock() {
    let snapshot = golden_json("xc-account-info.json");
    let now = chrono::Utc::now();

    let mut ours = to_json(&output::xc::account_info(&output::xc::AccountOptions {
        username: "fixtureadmin",
        password: "fixturepass",
        message: snapshot["user_info"]["message"].as_str().unwrap(),
        host: "ipx.test",
        // The advertised port, which the server resolves from its origin. An
        // Xtream client builds stream URLs from `server_info.url` and `port`,
        // so it has to be the one every other URL in the response carries.
        port: snapshot["server_info"]["port"].as_str().unwrap(),
        scheme: "http",
        active_connections: snapshot["user_info"]["active_cons"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        max_connections: snapshot["user_info"]["max_connections"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        now,
    }));

    // Whole-object, so a field the snapshot does not have is a failure rather
    // than something a hand-written key list quietly omits. Only the three
    // readings of the clock are substituted, and each is checked for shape
    // first.
    let exp: i64 = ours["user_info"]["exp_date"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(exp > now.timestamp(), "exp_date is in the future");
    assert!(ours["server_info"]["timestamp_now"].is_i64());
    assert!(snapshot["server_info"]["timestamp_now"].is_i64());
    let time_now = ours["server_info"]["time_now"].as_str().unwrap();
    assert_eq!(time_now.len(), 19, "`YYYY-MM-DD HH:MM:SS`");
    assert_eq!(
        snapshot["server_info"]["time_now"].as_str().unwrap().len(),
        19
    );

    ours["user_info"]["exp_date"] = snapshot["user_info"]["exp_date"].clone();
    ours["server_info"]["timestamp_now"] = snapshot["server_info"]["timestamp_now"].clone();
    ours["server_info"]["time_now"] = snapshot["server_info"]["time_now"].clone();
    assert_eq!(ours, snapshot);
}

// --- Xtream EPG actions ------------------------------------------------------

fn epg_context(
    stream_id: i64,
    channel_number: i64,
    epg_data_id: Option<i64>,
) -> output::xc::EpgContext {
    output::xc::EpgContext {
        stream_id,
        channel_number,
        epg_data_id,
        now: chrono::Utc::now(),
        archive_window: None,
        short: false,
    }
}

/// Rebuild the programmes behind an Xtream EPG snapshot, from the snapshot.
///
/// Base64 in, base64 out: the titles and descriptions are only comparable once
/// decoded, which is also why the leak scan decodes them.
fn programmes_from_listings(listings: &Json) -> Vec<dollet_core::domain::Program> {
    listings["epg_listings"]
        .as_array()
        .expect("epg_listings")
        .iter()
        .map(|listing| {
            let decode = |key: &str| {
                let raw = listing[key].as_str().unwrap_or_default();
                String::from_utf8(BASE64.decode(raw).expect("base64")).expect("utf-8")
            };
            dollet_core::domain::Program {
                id: listing["id"].as_str().unwrap().parse().unwrap_or(0),
                epg_data_id: listing["epg_id"].as_str().unwrap().parse().unwrap_or(0),
                tvg_id: None,
                start_time: chrono::DateTime::from_timestamp(
                    listing["start_timestamp"]
                        .as_str()
                        .unwrap()
                        .parse()
                        .unwrap(),
                    0,
                )
                .unwrap(),
                end_time: chrono::DateTime::from_timestamp(
                    listing["stop_timestamp"].as_str().unwrap().parse().unwrap(),
                    0,
                )
                .unwrap(),
                title: decode("title"),
                sub_title: None,
                description: Some(decode("description")),
                custom_properties: Json::Null,
            }
        })
        .collect()
}

#[test]
fn xc_simple_data_table_matches_the_snapshot() {
    // The most error-prone payload Xtream has: base64 title and description,
    // string-typed timestamps beside integer ones, and `now_playing` present
    // here but absent from the short EPG.
    let snapshot = golden_json("xc-get-simple-data-table.json");
    let programmes = programmes_from_listings(&snapshot);
    assert!(!programmes.is_empty(), "the snapshot has listings");

    let first = &snapshot["epg_listings"][0];
    let ctx = output::xc::EpgContext {
        now: chrono::DateTime::from_timestamp(
            first["start_timestamp"].as_str().unwrap().parse().unwrap(),
            0,
        )
        .unwrap(),
        ..epg_context(
            first["stream_id"].as_str().unwrap().parse().unwrap(),
            first["channel_id"].as_str().unwrap().parse().unwrap(),
            Some(first["epg_id"].as_str().unwrap().parse().unwrap()),
        )
    };

    let ours = to_json(&output::xc::epg_listings(&programmes, &ctx));
    // `now_playing` is a reading of the clock; everything else must match.
    let strip = |value: &Json| {
        let mut listings = value["epg_listings"].clone();
        for listing in listings.as_array_mut().unwrap() {
            listing["now_playing"] = Json::Null;
        }
        listings
    };
    assert_eq!(strip(&ours), strip(&snapshot));
    assert!(
        snapshot["epg_listings"][0]["now_playing"].is_i64(),
        "the simple data table carries now_playing"
    );
}

#[test]
fn xc_short_epg_omits_now_playing_where_the_data_table_carries_it() {
    // The one structural difference between the two actions, and a field we
    // emit conditionally — so getting it wrong is invisible without a snapshot.
    let short = golden_json("xc-get-short-epg.json");
    for listing in short["epg_listings"].as_array().expect("listings") {
        assert!(
            listing.get("now_playing").is_none(),
            "get_short_epg must not carry now_playing"
        );
        assert!(listing["has_archive"].is_i64());
    }

    let limited = golden_json("xc-get-short-epg-limit-2.json");
    assert_eq!(limited["epg_listings"].as_array().unwrap().len(), 2);

    // And ours agrees, given the same programmes.
    let programmes = programmes_from_listings(&short);
    let first = &short["epg_listings"][0];
    let ctx = output::xc::EpgContext {
        short: true,
        ..epg_context(
            first["stream_id"].as_str().unwrap().parse().unwrap(),
            first["channel_id"].as_str().unwrap().parse().unwrap(),
            Some(first["epg_id"].as_str().unwrap().parse().unwrap()),
        )
    };
    assert_eq!(to_json(&output::xc::epg_listings(&programmes, &ctx)), short);
}

// --- Dummy EPG ---------------------------------------------------------------
//
// The synthetic guide a channel gets when it has no EPG source. It matters out
// of proportion to its size: a channel with no `<programme>` at all is one Plex
// may not show, so a schedule that drifts is a channel that quietly stops
// appearing. Five served channels in this corpus are on dummy sources.

/// The instant the snapshots represent — the clock the generator read.
fn captured_at() -> chrono::DateTime<chrono::Utc> {
    golden("captured-at.txt")
        .trim()
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("captured-at.txt is an RFC 3339 instant")
}

/// Every identifier of a channel with no `epg_data` row, in every form an
/// output prints: channel numbers, `tvg_id`s, station ids, row ids and names,
/// because an authenticated Xtream export renumbers channels and none of the
/// id forms survive that.
fn dummy_identifiers() -> BTreeSet<String> {
    golden("dummy-channels.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The raw `<programme>` elements of a snapshot, in document order.
///
/// Raw text rather than parsed structs, because half the point is that our
/// generated programmes serialize to the same bytes — including the escaping
/// of a channel name containing a quote, an ampersand and angle brackets.
fn programme_blocks(guide: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut rest = guide;
    while let Some(open) = rest.find("  <programme ") {
        let close = rest[open..]
            .find("</programme>\n")
            .expect("a programme element closes");
        let block = &rest[open..open + close + "</programme>\n".len()];
        let channel = block
            .split("channel=\"")
            .nth(1)
            .and_then(|tail| tail.split('"').next())
            .expect("a programme names its channel");
        blocks.push((unescape_attr(channel), block.to_string()));
        rest = &rest[open + close..];
    }
    blocks
}

/// The entities the writer escapes in an attribute, undone, so a raw
/// `channel="…"` can be looked up against parsed ids.
fn unescape_attr(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The dummy channels in a guide snapshot: id, display name, blocks.
fn dummy_channels_in(name: &str) -> Vec<(String, String, Vec<String>)> {
    let guide = golden(name);
    let listed = dummy_identifiers();

    let mut names: BTreeMap<String, String> = BTreeMap::new();
    for item in parse::xmltv::from_bytes(guide.as_bytes()).expect("the guide decodes") {
        if let parse::xmltv::XmltvItem::Channel(channel) = item.expect("the guide parses") {
            names.insert(
                channel.tvg_id.clone(),
                channel.display_name.unwrap_or_default(),
            );
        }
    }

    let mut by_channel: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (channel, block) in programme_blocks(&guide) {
        by_channel.entry(channel).or_default().push(block);
    }

    by_channel
        .into_iter()
        .filter(|(id, _)| listed.contains(id))
        .map(|(id, blocks)| {
            let display = names.get(&id).cloned().expect("the channel is declared");
            (id, display, blocks)
        })
        .collect()
}

/// Our block for one generated programme, through the same writer the guide
/// uses.
fn dummy_block(channel_id: &str, programme: &output::dummy_epg::DummyProgram) -> String {
    let mut writer = output::xmltv::XmltvWriter::new(Vec::new());
    writer
        .write_programme(&output::xmltv::Programme::from_dummy(channel_id, programme))
        .unwrap();
    String::from_utf8(writer.into_inner()).unwrap()
}

/// Every guide snapshot, with the blocks each carries per dummy channel: the
/// default export is three days of four-hour blocks, and `?days=1` the first
/// six of the same run.
const DUMMY_GUIDES: &[(&str, usize)] = &[
    ("output-epg.xml", 18),
    ("output-epg-tvg-id.xml", 18),
    ("output-epg-gracenote.xml", 18),
    ("output-epg-days-1.xml", 6),
    ("xc-xmltv.xml", 18),
];

#[test]
fn the_dummy_guide_matches_the_snapshot_block_for_block() {
    // Block length, day count, hour alignment, the window's two ends, the
    // title and the description, over every dummy channel in every guide
    // rather than a convenient one — each guide names the channel differently,
    // and the escaping of a name carrying `"`, `&` and `<` is part of the bytes.
    let mut checked = 0;
    for (name, per_channel) in DUMMY_GUIDES {
        let channels = dummy_channels_in(name);
        assert_eq!(
            channels.len(),
            5,
            "{name}: every served channel with no EPG row"
        );

        for (id, display_name, blocks) in &channels {
            let ours = output::dummy_epg::generate(
                display_name,
                captured_at(),
                &output::dummy_epg::DummyOptions::default(),
            );
            assert_eq!(
                blocks.len(),
                *per_channel,
                "{name}: channel {id} block count"
            );
            assert_eq!(ours.len(), 18, "three days of four-hour blocks");

            for (ours, theirs) in ours.iter().zip(blocks) {
                assert_eq!(
                    dummy_block(id, ours),
                    *theirs,
                    "{name}: channel {id}, block starting {}",
                    ours.start_time
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 4 * 90 + 30, "five channels in every guide");
}

#[test]
fn every_dummy_block_carries_a_description_naming_the_channel() {
    // A programme with no description is what some clients treat as no
    // programme, so the element has to be present on every block — and it
    // names the channel, because a grid cell reading as filler beside a
    // channel name reads as broken.
    let channels = dummy_channels_in("output-epg.xml");

    let mut seen = 0;
    for (_, display_name, blocks) in &channels {
        for block in blocks {
            let (_, tail) = block.split_once("<desc>").expect("a desc element");
            let (desc, _) = tail.split_once("</desc>").expect("a desc element closes");
            assert!(!desc.is_empty(), "every block gets a sentence");
            seen += 1;
        }

        for ours in output::dummy_epg::generate(
            display_name,
            captured_at(),
            &output::dummy_epg::DummyOptions::default(),
        ) {
            assert!(!ours.description.is_empty(), "every block gets a sentence");
            assert!(
                ours.description.contains(display_name.trim()) || display_name.trim().is_empty(),
                "ours names the channel: {:?}",
                ours.description
            );
        }
    }
    assert_eq!(seen, 90, "five dummy channels, eighteen blocks each");
}

/// The dummy data table our serializer produces for the snapshot's channel and
/// instant.
fn dummy_data_table(snapshot: &Json) -> Json {
    let first = &snapshot["epg_listings"][0];
    let stream_id: i64 = first["stream_id"].as_str().unwrap().parse().unwrap();
    let channel_number: i64 = first["channel_id"].as_str().unwrap().parse().unwrap();

    // The channel's name, from the live-stream snapshot rather than written
    // down here — a dummy programme's title is its channel's name, so taking
    // it from a second snapshot is what makes the title comparison mean
    // anything.
    let name = golden_json("xc-get-live-streams.json")
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["stream_id"].as_i64() == Some(stream_id))
        .and_then(|row| row["name"].as_str().map(str::to_string))
        .expect("the channel behind the dummy snapshot");

    let generated = output::dummy_epg::generate(
        &name,
        captured_at(),
        &output::dummy_epg::DummyOptions::default(),
    );
    let ctx = output::xc::EpgContext {
        now: captured_at(),
        ..epg_context(stream_id, channel_number, None)
    };
    to_json(&output::xc::dummy_epg_listings(&generated, &ctx))
}

#[test]
fn the_dummy_data_table_matches_the_snapshot() {
    // The Xtream surface for the same channel and the same instant. Stronger
    // than the stored-programme comparison next door in one respect:
    // `now_playing` is asserted exactly rather than stripped, because the
    // instant is recorded and the block boundaries are deterministic.
    let snapshot = golden_json("xc-get-simple-data-table-dummy.json");
    let listings = snapshot["epg_listings"].as_array().expect("listings");
    assert_eq!(listings.len(), 18);

    let ours = dummy_data_table(&snapshot);
    assert_eq!(ours, snapshot);

    // `now_playing` is the reason the instant is committed: exactly one block
    // is live, and it is the first, because the run starts at the current
    // hour. A generator that drifted an hour would still look plausible.
    let live: Vec<usize> = listings
        .iter()
        .enumerate()
        .filter(|(_, l)| l["now_playing"] == 1)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(live, [0], "the first block is the one playing");

    // A client caches listings by id, so re-requesting has to hand back the
    // same ones. Each names its stream and the slot's start, so it can be read
    // when debugging a client.
    let stream_id = listings[0]["stream_id"].as_str().unwrap();
    assert_eq!(
        dummy_data_table(&snapshot),
        ours,
        "a client re-fetching sees the same ids"
    );
    for listing in listings {
        let id = listing["id"].as_str().unwrap();
        assert!(id.starts_with(stream_id), "the id names its stream: {id}");
    }
}

/// Rewrite the parts of the corpus only this crate can produce: the dummy
/// channels' programme blocks in every guide snapshot, and the dummy data
/// table. The server suite's `repin_the_served_snapshots` rewrites what the
/// router serves, and both recompute `checksums.tsv`.
///
/// ```text
/// cargo test -p dollet-core --test golden -- --ignored repin
/// ```
#[test]
#[ignore = "rewrites the snapshots; run on purpose after a deliberate output change"]
fn repin_the_dummy_guide_snapshots() {
    for (name, _) in DUMMY_GUIDES {
        let mut guide = golden(name);
        for (id, display_name, blocks) in dummy_channels_in(name) {
            let ours = output::dummy_epg::generate(
                &display_name,
                captured_at(),
                &output::dummy_epg::DummyOptions::default(),
            );
            // A one-day export carries the first six blocks of the same run.
            for (theirs, ours) in blocks.iter().zip(&ours) {
                assert_eq!(guide.matches(theirs.as_str()).count(), 1, "{name}: {id}");
                guide = guide.replacen(theirs, &dummy_block(&id, ours), 1);
            }
        }
        std::fs::write(golden_dir().join(name), guide).unwrap();
    }

    let name = "xc-get-simple-data-table-dummy.json";
    let ours = dummy_data_table(&golden_json(name));
    std::fs::write(
        golden_dir().join(name),
        serde_json::to_string_pretty(&ours).unwrap() + "\n",
    )
    .unwrap();

    write_checksums();
}

/// `checksums.tsv`, over every snapshot the manifest lists.
fn write_checksums() {
    let mut names: Vec<String> = snapshots().into_iter().map(|(name, _)| name).collect();
    names.sort();
    let mut out = String::new();
    for name in names {
        let bytes = std::fs::read(golden_dir().join(&name)).unwrap();
        out.push_str(&format!("{name}\t{}\t{}\n", bytes.len(), fnv1a64(&bytes)));
    }
    std::fs::write(golden_dir().join("checksums.tsv"), out).unwrap();
}

#[test]
fn xc_live_streams_by_category_renumbers_within_the_filtered_set() {
    // Not simply a filter: `num` and `epg_channel_id` are allocated over
    // whatever the request returns, so the same channel carries a different
    // number depending on the category asked for. A client that caches by `num`
    // across both calls is looking at two different things.
    let all = golden_json("xc-get-live-streams.json");
    let filtered = golden_json("xc-get-live-streams-cat3.json");
    let rows = filtered.as_array().expect("an array");
    assert!(!rows.is_empty() && rows.len() < all.as_array().unwrap().len());

    let mut renumbered = 0;
    for row in rows {
        assert_eq!(row["category_id"], "3");
        let unfiltered = all
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["stream_id"] == row["stream_id"])
            .expect("the row also appears unfiltered");
        // Everything but the allocated number is identical.
        for key in ["name", "stream_id", "category_id", "added", "stream_icon"] {
            assert_eq!(row[key], unfiltered[key], "{key}");
        }
        assert_eq!(
            row["epg_channel_id"],
            row["num"].as_i64().unwrap().to_string()
        );
        if row["num"] != unfiltered["num"] {
            renumbered += 1;
        }
    }
    assert!(renumbered > 0, "the filtered set allocates its own numbers");
}

#[test]
fn xc_panel_api_embeds_the_same_categories_and_channels() {
    // `panel_api.php` is `player_api.php` plus an embedded catalogue, so the
    // pieces have to agree with the endpoints that serve them alone.
    let panel = golden_json("xc-panel-api.json");
    assert_eq!(
        panel["categories"]["live"],
        golden_json("xc-get-live-categories.json")
    );
    assert_eq!(panel["categories"]["movie"], serde_json::json!([]));
    assert_eq!(panel["categories"]["series"], serde_json::json!([]));

    let available = panel["available_channels"].as_object().expect("an object");
    let live = golden_json("xc-get-live-streams.json");
    let rows = live.as_array().unwrap();
    assert_eq!(available.len(), rows.len());
    for row in rows {
        let key = row["stream_id"].as_i64().unwrap().to_string();
        assert_eq!(&available[&key], row, "channel {key}");
    }
}

#[test]
fn xc_flattens_a_fractional_channel_number_without_a_collision() {
    // 901.5 and a numberless channel both have to become free integers. Both
    // are adversarial rows in the sample; without them the allocator is never
    // exercised.
    let live = golden_json("xc-get-live-streams.json");
    let rows = live.as_array().unwrap();
    let numbers: Vec<i64> = rows.iter().map(|r| r["num"].as_i64().unwrap()).collect();

    let mut sorted = numbers.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), numbers.len(), "every num is distinct");

    let by_name = |name: &str| {
        rows.iter()
            .find(|r| r["name"] == name)
            .map(|r| r["num"].as_i64().unwrap())
            .expect(name)
    };
    // The fractional channel keeps its truncated value because it was free.
    assert_eq!(by_name("Fractional Number"), 901);
    // The numberless one takes the lowest integer not already claimed: 1, 2
    // and 3 are lineup numbers, so 4.
    assert_eq!(by_name("No Number"), 4);
    // And `epg_channel_id` is the flattened number, not the original.
    for row in rows {
        assert_eq!(
            row["epg_channel_id"],
            row["num"].as_i64().unwrap().to_string()
        );
    }
}

// --- Corpus integrity ---------------------------------------------------------

/// The snapshot files, as opposed to the metadata that documents them.
fn snapshots() -> Vec<(String, String)> {
    golden("manifest.txt")
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| (name.to_string(), golden(name)))
        .collect()
}

/// Every host a snapshot may mention: the request host, the fixture's
/// stand-ins, and the project's own, which the XMLTV generator URL names.
const ALLOWED_HOSTS: &[&str] = &[
    "ipx.test",
    "provider.example",
    "logos.example",
    "guide.example",
    "github.com",
];

/// Every committed file, not only the snapshots.
///
/// Read from the directory, not the manifest: `captured-at.txt` and
/// `dummy-channels.txt` are not snapshots and a credential in either is still
/// a leak.
fn every_golden_file() -> Vec<(String, String)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(golden_dir()).unwrap() {
        let path = entry.unwrap().path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        files.push((
            path.file_name().unwrap().to_string_lossy().into_owned(),
            text,
        ));
    }
    files.sort();
    assert!(files.len() > 40, "the corpus is present");
    files
}

/// The README describes the corpus rather than being served output, and uses
/// placeholders that would otherwise trip the credential heuristics.
fn is_documentation(name: &str) -> bool {
    name == "README.md"
}

/// `text`, plus anything base64 inside it that decodes to more text.
///
/// The Xtream EPG actions carry titles and descriptions base64-encoded. A scan
/// of the raw bytes sees none of it.
fn with_decoded_payloads(text: &str) -> Vec<String> {
    let mut payloads = vec![text.to_string()];
    let bytes = text.as_bytes();
    let mut start = None;
    for index in 0..=bytes.len() {
        let is_b64 = index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'+' | b'/' | b'='));
        match (is_b64, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                let candidate = &text[from..index];
                if candidate.len() >= 16
                    && let Ok(decoded) = BASE64.decode(candidate)
                    && let Ok(text) = String::from_utf8(decoded)
                {
                    payloads.push(text);
                }
                start = None;
            }
            _ => {}
        }
    }
    payloads
}

/// Whether a candidate is base64 of valid UTF-8, padding restored.
///
/// The splitter above treats `=` as a delimiter, so a padded payload arrives
/// here stripped of it and would otherwise fail to decode and be reported as an
/// opaque token.
fn decodes_as_text(candidate: &str) -> bool {
    let mut padded = candidate.to_string();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    BASE64
        .decode(&padded)
        .is_ok_and(|bytes| String::from_utf8(bytes).is_ok())
}

#[test]
fn the_corpus_references_no_host_outside_the_allowlist() {
    // An allowlist, not a list of secrets to search for. A denylist would have
    // to name the provider's hostname and stream credentials to look for them,
    // which would put the very strings it is guarding against into the
    // repository — and it would only ever catch the ones somebody thought of.
    let mut stray: Vec<String> = Vec::new();
    for (name, text) in every_golden_file() {
        if is_documentation(&name) {
            continue;
        }
        for payload in with_decoded_payloads(&text) {
            for host in common::hosts_referenced(&payload) {
                if !ALLOWED_HOSTS.contains(&host.as_str()) {
                    stray.push(format!("{name}: {host}"));
                }
            }
        }
    }
    stray.sort();
    stray.dedup();
    assert_eq!(stray, Vec::<String>::new());
}

#[test]
fn the_corpus_carries_no_credential_shaped_path_segments() {
    // Stream URLs are `/live/<user>/<pass>/<id>`; anything but the fixture's
    // own pair in those segments is a credential.
    let mut leaked = Vec::new();
    for (name, text) in every_golden_file() {
        if is_documentation(&name) {
            continue;
        }
        for payload in with_decoded_payloads(&text) {
            for (index, _) in payload.match_indices("/live/") {
                let rest = &payload[index + "/live/".len()..];
                let mut parts = rest.split('/');
                let (Some(user), Some(password)) = (parts.next(), parts.next()) else {
                    continue;
                };
                if (user, password) != ("fixtureuser", "fixturepass")
                    && (user, password) != ("fixtureadmin", "fixturepass")
                {
                    leaked.push(format!("{name}: /live/{user}/{password}/"));
                }
            }
        }
    }
    leaked.sort();
    leaked.dedup();
    assert_eq!(leaked, Vec::<String>::new());
}

#[test]
fn the_corpus_carries_no_opaque_high_entropy_path_segments() {
    // Hostname and `/live/` checks both depend on knowing where a secret sits;
    // a portal path like `/m3u/<token>/<token>` is in neither place.
    //
    // Flags any path segment that looks like a token: long enough to be one and
    // mixing upper and lower case, which real path words do not.
    let mut suspicious = Vec::new();
    for (name, text) in every_golden_file() {
        if is_documentation(&name) {
            continue;
        }
        for payload in with_decoded_payloads(&text) {
            for segment in payload.split(['/', '"', '?', '&', '=', '<', '>', ' ']) {
                let opaque = segment.len() >= 8
                    && segment.len() <= 64
                    && segment.chars().all(|c| c.is_ascii_alphanumeric())
                    && segment.chars().any(|c| c.is_ascii_uppercase())
                    && segment.chars().any(|c| c.is_ascii_lowercase())
                    && segment.chars().any(|c| c.is_ascii_digit())
                    // A base64 payload is not a token: it is Xtream's
                    // encoding of a title, and it is already scanned above
                    // in decoded form.
                    && !decodes_as_text(segment);
                if opaque {
                    suspicious.push(format!("{name}: {segment}"));
                }
            }
        }
    }
    suspicious.sort();
    suspicious.dedup();
    assert_eq!(suspicious, Vec::<String>::new());
}

#[test]
fn every_snapshot_is_present_unmodified_and_accounted_for() {
    // Length and a content hash together, so a zeroed or half-written snapshot
    // fails where `.exists()` passes.
    let mut checked = 0;
    for line in golden("checksums.tsv").lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(size), Some(digest)) = (fields.next(), fields.next(), fields.next())
        else {
            panic!("malformed checksum line: {line}");
        };
        let bytes = std::fs::read(golden_dir().join(name))
            .unwrap_or_else(|e| panic!("reading {name}: {e}"));
        assert_eq!(bytes.len().to_string(), size, "{name} length");
        assert_eq!(fnv1a64(&bytes), digest, "{name} content");
        checked += 1;
    }
    assert_eq!(checked, 42, "every snapshot has a checksum");

    // And the manifest and the checksum file describe the same set.
    let manifest: Vec<String> = snapshots().into_iter().map(|(name, _)| name).collect();
    let mut hashed: Vec<String> = golden("checksums.tsv")
        .lines()
        .map(|line| line.split('\t').next().unwrap().to_string())
        .collect();
    let mut manifest_sorted = manifest.clone();
    manifest_sorted.sort();
    hashed.sort();
    assert_eq!(hashed, manifest_sorted);
}

/// FNV-1a, because the integration target has no hashing dependency and this is
/// a tripwire against truncation rather than a defence against a forger.
fn fnv1a64(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in bytes {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01B3);
    }
    format!("{hash:016x}")
}

#[test]
fn every_snapshot_in_the_manifest_is_present() {
    let manifest = golden("manifest.txt");
    let mut checked = 0;
    for line in manifest.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(status)) = (fields.next(), fields.next()) else {
            continue;
        };
        assert!(
            golden_dir().join(name).exists(),
            "{name} is in the manifest but not on disk"
        );
        // Only two snapshots are error responses.
        let expected_404 = matches!(
            name,
            "output-m3u-missing-profile.m3u" | "xc-unauthorized.json"
        );
        assert_eq!(status == "404", expected_404, "{name} returned {status}");
        checked += 1;
    }
    assert_eq!(checked, 42, "the manifest lists every snapshot");
}

// --- End to end ---------------------------------------------------------------

/// The end-to-end path: seed `fixtures/sample.sql`, read it back through the
/// query layer, and render.
///
/// The tests above reconstruct their inputs from the snapshots themselves,
/// which proves the serializers but not the queries that feed them —
/// `effective_*` coalescing, the lineup ordering and the `hidden_from_output`
/// filter all live in SQL. This is the one test where a mistake in that SQL
/// shows up as a byte difference rather than as a passing serializer fed the
/// wrong rows.
#[tokio::test]
async fn end_to_end_from_the_fixture_database() {
    let db = seeded().await;
    let channels = effective_channels(&db).await;
    assert_eq!(
        channels.len(),
        16,
        "the lineup plus the hard cases, less the hidden one"
    );

    // Ordering is the query's, not the serializer's: the lineup Plex reads is
    // in `channel_number` order, and an unnumbered channel must not lead it.
    let snapshot_lineup = golden_json("hdhr-lineup.json");
    let ours = output::hdhr::lineup(BASE_URL, &channels, None);
    assert_eq!(to_json(&ours), snapshot_lineup, "lineup from the database");

    // Logo URLs are rewritten to this server's artwork cache, which is what
    // `/output/m3u` does by default and what the snapshot holds. The cache is
    // addressed by logo id, which the serializer never sees.
    let mut cached = channels.clone();
    let logos: BTreeMap<i64, i64> =
        dollet_core::db::channels::logo_ids(&db, &cached.iter().map(|c| c.id).collect::<Vec<_>>())
            .await
            .unwrap()
            .into_iter()
            .collect();
    for channel in &mut cached {
        if let Some(logo) = logos.get(&channel.id) {
            channel.logo_url = Some(format!("{BASE_URL}/api/channels/logos/{logo}/cache/"));
        }
    }

    let entries: Vec<_> = cached
        .iter()
        .map(output::m3u::M3uChannel::proxied)
        .collect();
    let playlist = output::m3u::render(
        &m3u_options(
            &format!("{BASE_URL}/output/epg"),
            TvgIdSource::ChannelNumber,
        ),
        &entries,
    );
    assert_lines_match(&playlist, &golden("output-m3u.m3u"), "output-m3u.m3u");

    // And with the cache turned off, the provider's own URLs come through.
    let raw = output::m3u::render(
        &m3u_options(
            &format!("{BASE_URL}/output/epg?cachedlogos=false"),
            TvgIdSource::ChannelNumber,
        ),
        &channels
            .iter()
            .map(output::m3u::M3uChannel::proxied)
            .collect::<Vec<_>>(),
    );
    assert_lines_match(
        &raw,
        &golden("output-m3u-no-cached-logos.m3u"),
        "output-m3u-no-cached-logos.m3u",
    );

    // The guide's channel elements, which is the part the fixture can prove:
    // `sample.sql` samples the programme table where the snapshot carries every
    // row, so the programmes themselves are compared by the round-trip tests
    // above rather than here.
    let mut writer = output::xmltv::XmltvWriter::new(Vec::new());
    writer
        .start(&output::xmltv::XmltvOptions {
            tvg_id_source: TvgIdSource::ChannelNumber,
            ..Default::default()
        })
        .unwrap();
    for channel in &cached {
        writer
            .write_effective_channel(channel, TvgIdSource::ChannelNumber)
            .unwrap();
    }
    let ours = String::from_utf8(writer.into_inner()).unwrap();
    let snapshot_channels: String = golden("output-epg.xml")
        .lines()
        .take_while(|line| !line.trim_start().starts_with("<programme"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_lines_match(
        &ours,
        &snapshot_channels,
        "guide channels from the database",
    );

    // Xtream addresses channels by row id and numbers them itself, so this is
    // the query layer's ids meeting the serializer's numbering.
    let live = output::xc::live_streams(
        &cached,
        &output::xc::LiveStreamOptions {
            default_group_id: 1,
            catchup_allowed: false,
        },
    );
    assert_eq!(
        to_json(&live),
        golden_json("xc-get-live-streams.json"),
        "live streams from the database"
    );

    // Categories are the groups the lineup actually uses, in the order it uses
    // them — not alphabetical, and not the ones with nothing in them.
    let groups = dollet_core::db::channels::groups_in_lineup_order(&db, None)
        .await
        .unwrap();
    assert_eq!(
        to_json(&output::xc::live_categories(&groups)),
        golden_json("xc-get-live-categories.json"),
        "live categories from the database"
    );
}

/// A hidden channel is filtered in SQL, so it never reaches a serializer.
#[tokio::test]
async fn hiding_a_channel_removes_it_from_every_output() {
    let db = seeded().await;
    let before = effective_channels(&db).await;

    sqlx::query("UPDATE channel SET hidden_from_output = 1 WHERE id = 171")
        .execute(&db)
        .await
        .unwrap();

    let after = effective_channels(&db).await;
    assert_eq!(after.len(), before.len() - 1);
    assert!(!after.iter().any(|channel| channel.id == 171));
}

/// The override layer is what every output reads, so a rename has to reach the
/// lineup without touching the row provider sync keeps rewriting.
#[tokio::test]
async fn an_override_reaches_the_lineup() {
    let db = seeded().await;

    sqlx::query(
        "INSERT INTO channel_override (channel_id, name, channel_number) VALUES (171, ?, 0.5)",
    )
    .bind("Renamed In The Lineup")
    .execute(&db)
    .await
    .unwrap();

    let channels = effective_channels(&db).await;
    let lineup = output::hdhr::lineup(BASE_URL, &channels, None);

    assert_eq!(lineup[0].guide_name, "Renamed In The Lineup");
    assert_eq!(lineup[0].guide_number, "0.5");
}

/// A temporary database seeded from the committed fixture.
async fn seeded() -> sqlx::SqlitePool {
    let db = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    dollet_core::db::MIGRATOR.run(&db).await.unwrap();

    let fixture = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sample.sql"),
    )
    .expect("reading fixtures/sample.sql");
    sqlx::raw_sql(&fixture).execute(&db).await.unwrap();
    db
}

/// What every output path asks for: effective values, visible only, in lineup
/// order. The same call the HTTP handlers make.
async fn effective_channels(db: &sqlx::SqlitePool) -> Vec<EffectiveChannel> {
    dollet_core::db::channels::list_effective(
        db,
        &dollet_core::db::channels::ChannelFilter {
            visible_only: true,
            ..Default::default()
        },
        None,
        None,
    )
    .await
    .unwrap()
    .results
}
