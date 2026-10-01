//! Xtream Codes server API payloads, live actions only.
//!
//! Xtream clients are strict about types in ways the format's documentation is
//! not: `stream_id` is a number, `category_id` is a *string* of the same number,
//! and EPG titles are base64. Every deviation here shows up as a client that
//! silently lists nothing.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value as Json;

use crate::domain::{ChannelGroup, EffectiveChannel, Id, Program};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Category {
    pub category_id: String,
    pub category_name: String,
    pub parent_id: i64,
}

/// Categories in the order the caller supplies, which is lineup order: by the
/// lowest effective channel number in each group.
pub fn live_categories(groups: &[ChannelGroup]) -> Vec<Category> {
    groups
        .iter()
        .map(|g| Category {
            category_id: g.id.to_string(),
            category_name: g.name.clone(),
            parent_id: 0,
        })
        .collect()
}

/// The two values the live-streams payload needs that are properties of the
/// request rather than of a channel.
pub struct LiveStreamOptions {
    /// Category for a channel belonging to no group. Xtream clients reject a
    /// null `category_id`, so there has to be somewhere to put them.
    pub default_group_id: Id,
    /// True when catch-up is enabled for the requesting user *and* globally.
    pub catchup_allowed: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LiveStream {
    pub num: i64,
    pub name: String,
    pub stream_type: &'static str,
    pub stream_id: Id,
    pub stream_icon: Option<String>,
    pub epg_channel_id: String,
    pub added: String,
    pub is_adult: u8,
    pub category_id: String,
    pub category_ids: Vec<Id>,
    pub custom_sid: &'static str,
    pub tv_archive: u8,
    pub direct_source: &'static str,
    pub tv_archive_duration: u32,
}

/// Channels flagged `hidden_from_output` are dropped here rather than trusted
/// to have been filtered already, for the same reason as in the M3U output.
pub fn live_streams(channels: &[EffectiveChannel], opts: &LiveStreamOptions) -> Vec<LiveStream> {
    let visible: Vec<&EffectiveChannel> =
        channels.iter().filter(|c| !c.hidden_from_output).collect();
    let numbers = assign_channel_numbers(&visible);

    visible
        .iter()
        .map(|ch| {
            let num = numbers.get(&ch.id).copied().unwrap_or(ch.id);
            let catchup = opts.catchup_allowed && ch.is_catchup;
            let group_id = ch.channel_group_id.unwrap_or(opts.default_group_id);
            LiveStream {
                num,
                name: ch.name.clone(),
                stream_type: "live",
                stream_id: ch.id,
                stream_icon: ch.logo_url.clone(),
                epg_channel_id: num.to_string(),
                added: ch.created_at.timestamp().to_string(),
                is_adult: u8::from(ch.is_adult),
                category_id: group_id.to_string(),
                category_ids: vec![group_id],
                custom_sid: "",
                tv_archive: u8::from(catchup),
                direct_source: "",
                tv_archive_duration: if catchup { ch.catchup_days } else { 0 },
            }
        })
        .collect()
}

/// Xtream clients address channels by a bare integer, so fractional and missing
/// channel numbers have to be flattened without two channels colliding.
///
/// Integer numbers are claimed first, then the rest take the next free integer
/// at or above their truncated value. The caller's ordering decides who wins a
/// contested slot, so it must be the same ordering used for `get_live_streams`
/// and for the guide, or a client's EPG lands on the wrong row.
///
/// The allocation is stable rather than clever: `num` is what an Xtream client
/// stores as a channel's identity, so a different assignment would renumber
/// every channel in every client.
pub fn assign_channel_numbers(channels: &[&EffectiveChannel]) -> BTreeMap<Id, i64> {
    let mut assigned = BTreeMap::new();
    let mut used = std::collections::BTreeSet::new();
    let mut deferred = Vec::new();

    for ch in channels {
        match ch.channel_number.filter(|n| n.is_finite()) {
            Some(number) if number == number.trunc() => {
                let number = number as i64;
                assigned.insert(ch.id, number);
                used.insert(number);
            }
            other => deferred.push((ch.id, other)),
        }
    }

    for (id, number) in deferred {
        let mut candidate = number.map_or(1, |n| n as i64);
        while used.contains(&candidate) {
            candidate += 1;
        }
        assigned.insert(id, candidate);
        used.insert(candidate);
    }
    assigned
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EpgListings {
    pub epg_listings: Vec<EpgListing>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EpgListing {
    pub id: String,
    pub epg_id: String,
    /// Base64, which is what the format specifies and what clients decode.
    pub title: String,
    pub lang: &'static str,
    pub start: String,
    pub end: String,
    pub description: String,
    pub channel_id: String,
    pub start_timestamp: String,
    pub stop_timestamp: String,
    pub stream_id: String,
    pub has_archive: u8,
    /// Absent for `get_short_epg`, present for `get_simple_data_table`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing: Option<u8>,
}

pub struct EpgContext {
    /// The channel's row id, which is the Xtream `stream_id`.
    pub stream_id: Id,
    /// The collision-free integer from [`assign_channel_numbers`].
    pub channel_number: i64,
    /// The `EpgData` row backing this channel, if any.
    pub epg_data_id: Option<Id>,
    pub now: DateTime<Utc>,
    /// Catch-up reach. `None` means no programme can be replayed.
    pub archive_window: Option<Duration>,
    /// `get_short_epg` omits `now_playing`; `get_simple_data_table` includes it.
    pub short: bool,
}

/// `get_short_epg` and `get_simple_data_table`. The caller does the filtering
/// and limiting in SQL; the shape is identical either way.
pub fn epg_listings(programs: &[Program], ctx: &EpgContext) -> EpgListings {
    EpgListings {
        epg_listings: programs
            .iter()
            .map(|p| listing(p.id.to_string(), p, ctx))
            .collect(),
    }
}

/// Same payload for a channel on a dummy EPG source, where there is no stored
/// row to take an id from.
pub fn dummy_epg_listings(
    programs: &[super::dummy_epg::DummyProgram],
    ctx: &EpgContext,
) -> EpgListings {
    let listings = programs
        .iter()
        .map(|p| {
            // Synthetic but stable: a client that re-fetches must see the same
            // ids for the same slots.
            let id = format!("{}{:010}", ctx.stream_id, p.start_time.timestamp());
            let program = Program {
                id: 0,
                epg_data_id: ctx.epg_data_id.unwrap_or_default(),
                tvg_id: None,
                start_time: p.start_time,
                end_time: p.end_time,
                title: p.title.clone(),
                sub_title: None,
                description: Some(p.description.clone()),
                custom_properties: Json::Null,
            };
            listing(id, &program, ctx)
        })
        .collect();
    EpgListings {
        epg_listings: listings,
    }
}

fn listing(id: String, program: &Program, ctx: &EpgContext) -> EpgListing {
    let has_archive = ctx
        .archive_window
        .is_some_and(|window| program.end_time < ctx.now && program.end_time > ctx.now - window);
    EpgListing {
        id,
        epg_id: ctx
            .epg_data_id
            .map_or_else(|| "0".into(), |v| v.to_string()),
        title: BASE64.encode(&program.title),
        lang: "",
        start: format_time(program.start_time),
        end: format_time(program.end_time),
        description: BASE64.encode(program.description.as_deref().unwrap_or_default()),
        channel_id: ctx.channel_number.to_string(),
        start_timestamp: program.start_time.timestamp().to_string(),
        stop_timestamp: program.end_time.timestamp().to_string(),
        stream_id: ctx.stream_id.to_string(),
        has_archive: u8::from(has_archive),
        // Half-open, deliberately: inclusive at both ends would, for the
        // instant one programme ends and the next begins, flag both.
        // The listings are ordered by start time, so a client taking the first
        // match is handed the programme that has just *finished*. No client
        // benefits from being told two things are on at once, and this is the
        // payload a player reads to decide what to highlight. See
        // `fixtures/golden/README.md`.
        now_playing: (!ctx.short)
            .then(|| u8::from(program.start_time <= ctx.now && ctx.now < program.end_time)),
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AccountInfo {
    pub user_info: UserInfo,
    pub server_info: ServerInfo,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UserInfo {
    pub username: String,
    pub password: String,
    pub message: String,
    pub auth: u8,
    pub status: &'static str,
    /// Seconds since the epoch, as a string. Clients that parse it as a number
    /// still accept the string; the reverse is not true.
    pub exp_date: String,
    pub active_cons: String,
    pub max_connections: String,
    pub allowed_output_formats: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ServerInfo {
    pub url: String,
    pub server_protocol: String,
    pub port: String,
    /// Always UTC, and EPG times are emitted to match. Clients interpret guide
    /// timestamps through this field, so a host with a mis-set clock zone would
    /// otherwise shift every programme.
    pub timezone: &'static str,
    pub timestamp_now: i64,
    pub time_now: String,
    pub process: bool,
}

pub struct AccountOptions<'a> {
    pub username: &'a str,
    pub password: &'a str,
    /// Shown in some clients' account screens.
    pub message: &'a str,
    pub host: &'a str,
    pub port: &'a str,
    pub scheme: &'a str,
    pub active_connections: u32,
    pub max_connections: u32,
    pub now: DateTime<Utc>,
}

/// The `user_info` / `server_info` payload, returned for `get_account_info` and
/// for any action the API does not recognise.
pub fn account_info(opts: &AccountOptions<'_>) -> AccountInfo {
    AccountInfo {
        user_info: UserInfo {
            username: opts.username.to_string(),
            password: opts.password.to_string(),
            message: opts.message.to_string(),
            auth: 1,
            status: "Active",
            // No expiry is modelled, and clients treat a past date as a dead
            // account, so the window is always 90 days out.
            exp_date: (opts.now + Duration::days(90)).timestamp().to_string(),
            active_cons: opts.active_connections.to_string(),
            max_connections: opts.max_connections.to_string(),
            allowed_output_formats: vec!["ts", "mp4"],
        },
        server_info: ServerInfo {
            url: opts.host.to_string(),
            server_protocol: opts.scheme.to_string(),
            port: opts.port.to_string(),
            timezone: "UTC",
            timestamp_now: opts.now.timestamp(),
            time_now: opts.now.format("%Y-%m-%d %H:%M:%S").to_string(),
            process: true,
        },
    }
}

/// VOD and series are out of scope for 1.0.
///
/// These return empty collections rather than an error because an Xtream client
/// treats a failed catalogue call as a broken account and stops asking for live
/// channels too. An empty catalogue is the honest answer; fabricated entries
/// would not be.
pub fn vod_categories() -> Vec<Category> {
    Vec::new()
}

/// See [`vod_categories`].
pub fn vod_streams() -> Vec<Json> {
    Vec::new()
}

/// See [`vod_categories`].
pub fn series_categories() -> Vec<Category> {
    Vec::new()
}

/// See [`vod_categories`].
pub fn series() -> Vec<Json> {
    Vec::new()
}

fn format_time(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::UserLevel;
    use uuid::Uuid;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn channel(id: i64, name: &str, number: Option<f64>) -> EffectiveChannel {
        EffectiveChannel {
            id,
            uuid: Uuid::from_u128(id as u128),
            channel_number: number,
            name: name.into(),
            channel_group_id: Some(1),
            created_at: chrono::DateTime::UNIX_EPOCH,
            group_name: Some("News".into()),
            logo_url: Some(format!("http://ipx.test/logo/{id}.png")),
            tvg_id: Some(format!("ch{id}.us")),
            tvc_guide_stationid: None,
            epg_data_id: Some(id),
            stream_profile_id: None,
            user_level: UserLevel::Streamer,
            is_adult: false,
            hidden_from_output: false,
            is_catchup: false,
            catchup_days: 0,
        }
    }

    fn program(id: i64, start: &str, end: &str, title: &str) -> Program {
        Program {
            id,
            epg_data_id: 5,
            tvg_id: Some("ch1.us".into()),
            start_time: utc(start),
            end_time: utc(end),
            title: title.into(),
            sub_title: None,
            description: Some(format!("{title} description")),
            custom_properties: Json::Null,
        }
    }

    fn context() -> EpgContext {
        EpgContext {
            stream_id: 1,
            channel_number: 101,
            epg_data_id: Some(5),
            now: utc("2026-09-11T15:45:00Z"),
            archive_window: None,
            short: false,
        }
    }

    #[test]
    fn live_categories_json() {
        let groups = [
            ChannelGroup {
                id: 1,
                name: "News".into(),
                number_start: None,
                number_end: None,
            },
            ChannelGroup {
                id: 2,
                name: "Sports & More".into(),
                number_start: None,
                number_end: None,
            },
        ];
        insta::assert_json_snapshot!(live_categories(&groups));
        assert_eq!(serde_json::to_string(&live_categories(&[])).unwrap(), "[]");
    }

    fn live_options() -> LiveStreamOptions {
        LiveStreamOptions {
            default_group_id: 99,
            catchup_allowed: true,
        }
    }

    #[test]
    fn live_streams_json() {
        let mut a = channel(1, "Channel A", Some(101.0));
        a.is_adult = true;
        let mut b = channel(2, "Channel B", Some(102.5));
        b.is_catchup = true;
        b.catchup_days = 7;
        let mut c = channel(3, "Channel C", None);
        c.logo_url = None;
        // No group: falls back to the caller's default category.
        c.channel_group_id = None;
        let channels = [a, b, c];

        insta::assert_json_snapshot!(live_streams(&channels, &live_options()));
    }

    #[test]
    fn catchup_fields_are_zero_when_the_user_may_not_use_it() {
        let mut ch = channel(1, "A", Some(1.0));
        ch.is_catchup = true;
        ch.catchup_days = 7;
        let opts = LiveStreamOptions {
            catchup_allowed: false,
            ..live_options()
        };
        let got = live_streams(std::slice::from_ref(&ch), &opts);
        assert_eq!(got[0].tv_archive, 0);
        assert_eq!(got[0].tv_archive_duration, 0);

        let allowed = live_streams(std::slice::from_ref(&ch), &live_options());
        assert_eq!(allowed[0].tv_archive, 1);
        assert_eq!(allowed[0].tv_archive_duration, 7);
    }

    #[test]
    fn channels_hidden_from_output_are_dropped_and_do_not_claim_a_number() {
        let visible = channel(1, "Visible", Some(101.0));
        let mut hidden = channel(2, "Hidden", Some(101.0));
        hidden.hidden_from_output = true;
        let after = channel(3, "After", Some(101.5));

        let got = live_streams(&[visible, hidden, after], &live_options());
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|s| s.name != "Hidden"));
        // 101 is taken by the visible channel, so the fractional one gets 102 --
        // the hidden channel must not have consumed a slot on the way past.
        assert_eq!(got[1].num, 102);
    }

    #[test]
    fn channel_numbers_flatten_without_collisions() {
        let channels = [
            channel(1, "Integer", Some(101.0)),
            channel(2, "Fraction of an existing", Some(101.5)),
            channel(3, "Second fraction", Some(101.7)),
            channel(4, "No number", None),
            channel(5, "Takes 1's neighbour", Some(1.0)),
            channel(6, "Not finite", Some(f64::NAN)),
        ];
        let refs: Vec<&EffectiveChannel> = channels.iter().collect();
        let got = assign_channel_numbers(&refs);
        assert_eq!(got[&1], 101);
        assert_eq!(got[&2], 102);
        assert_eq!(got[&3], 103);
        assert_eq!(got[&5], 1);
        // The numberless pair starts at 1 and walks up past what is taken.
        assert_eq!(got[&4], 2);
        assert_eq!(got[&6], 3);
        assert_eq!(got.len(), 6);

        assert!(assign_channel_numbers(&[]).is_empty());
    }

    #[test]
    fn epg_listings_json_for_the_simple_data_table() {
        let programs = [
            program(
                1,
                "2026-09-11T15:30:00Z",
                "2026-09-11T16:00:00Z",
                "Now Playing",
            ),
            program(
                2,
                "2026-09-11T16:00:00Z",
                "2026-09-11T16:30:00Z",
                "Tom & Jerry",
            ),
        ];
        insta::assert_json_snapshot!(epg_listings(&programs, &context()));
    }

    #[test]
    fn epg_listings_json_for_the_short_epg() {
        let programs = [program(
            1,
            "2026-09-11T16:00:00Z",
            "2026-09-11T16:30:00Z",
            "Upcoming",
        )];
        let ctx = EpgContext {
            short: true,
            ..context()
        };
        insta::assert_json_snapshot!(epg_listings(&programs, &ctx));
    }

    #[test]
    fn has_archive_marks_only_replayable_past_programmes() {
        let programs = [
            program(1, "2026-09-01T00:00:00Z", "2026-09-01T01:00:00Z", "Too old"),
            program(
                2,
                "2026-09-11T10:00:00Z",
                "2026-09-11T11:00:00Z",
                "In window",
            ),
            program(
                3,
                "2026-09-11T15:30:00Z",
                "2026-09-11T16:00:00Z",
                "Airing now",
            ),
            program(4, "2026-09-11T20:00:00Z", "2026-09-11T21:00:00Z", "Future"),
        ];
        let ctx = EpgContext {
            archive_window: Some(Duration::days(2)),
            ..context()
        };
        let got = epg_listings(&programs, &ctx);
        let flags: Vec<u8> = got.epg_listings.iter().map(|l| l.has_archive).collect();
        assert_eq!(flags, [0, 1, 0, 0]);

        let without = epg_listings(&programs, &context());
        assert!(without.epg_listings.iter().all(|l| l.has_archive == 0));
    }

    #[test]
    fn now_playing_is_present_only_for_the_simple_data_table() {
        let programs = [
            program(1, "2026-09-11T15:30:00Z", "2026-09-11T16:00:00Z", "Now"),
            program(2, "2026-09-11T16:00:00Z", "2026-09-11T17:00:00Z", "Next"),
        ];
        let table = epg_listings(&programs, &context());
        assert_eq!(table.epg_listings[0].now_playing, Some(1));
        assert_eq!(table.epg_listings[1].now_playing, Some(0));

        let short = epg_listings(
            &programs,
            &EpgContext {
                short: true,
                ..context()
            },
        );
        assert_eq!(short.epg_listings[0].now_playing, None);
        let json = serde_json::to_string(&short).unwrap();
        assert!(!json.contains("now_playing"), "{json}");
    }

    #[test]
    fn at_a_boundary_exactly_one_programme_is_playing() {
        // A deliberate choice. A comparison inclusive at both ends would, for
        // the instant one programme ends and the next begins, flag both — and
        // since the listings are ordered by start time, a client taking the
        // first match is handed the programme that has just finished. Ours is
        // half-open, so the answer is the one that is actually on.
        //
        // Pinned rather than left to the reader because the golden corpus
        // cannot see it: `now_playing` is a reading of the clock and the
        // snapshot instant lands mid-block, where both spellings agree.
        let boundary = utc("2026-09-11T16:00:00Z");
        let programs = [
            program(1, "2026-09-11T15:00:00Z", "2026-09-11T16:00:00Z", "Ends"),
            program(2, "2026-09-11T16:00:00Z", "2026-09-11T17:00:00Z", "Starts"),
        ];

        for (offset, want) in [
            (-1, [Some(1), Some(0)]),
            (0, [Some(0), Some(1)]),
            (1, [Some(0), Some(1)]),
        ] {
            let got = epg_listings(
                &programs,
                &EpgContext {
                    now: boundary + Duration::seconds(offset),
                    ..context()
                },
            );
            let flags: Vec<Option<u8>> = got.epg_listings.iter().map(|l| l.now_playing).collect();
            assert_eq!(flags, want, "{offset}s from the boundary");
            assert_eq!(
                flags.iter().filter(|f| **f == Some(1)).count(),
                1,
                "exactly one programme is current at {offset}s"
            );
        }

        // The end of the last programme is the one instant where nothing is
        // current, and that is the honest answer rather than a gap to paper
        // over: after it, nothing is known to be on.
        let after = epg_listings(
            &programs,
            &EpgContext {
                now: utc("2026-09-11T17:00:00Z"),
                ..context()
            },
        );
        assert!(
            after.epg_listings.iter().all(|l| l.now_playing == Some(0)),
            "nothing is on once the last programme has ended"
        );
    }

    #[test]
    fn a_channel_with_no_epg_row_reports_epg_id_zero() {
        let programs = [program(
            1,
            "2026-09-11T15:30:00Z",
            "2026-09-11T16:00:00Z",
            "X",
        )];
        let ctx = EpgContext {
            epg_data_id: None,
            ..context()
        };
        assert_eq!(epg_listings(&programs, &ctx).epg_listings[0].epg_id, "0");
    }

    #[test]
    fn titles_and_descriptions_are_base64() {
        let mut p = program(
            1,
            "2026-09-11T15:30:00Z",
            "2026-09-11T16:00:00Z",
            "Tom & Jerry",
        );
        p.description = None;
        let got = epg_listings(std::slice::from_ref(&p), &context());
        assert_eq!(got.epg_listings[0].title, BASE64.encode("Tom & Jerry"));
        assert_eq!(got.epg_listings[0].description, "");
    }

    #[test]
    fn dummy_epg_listings_json() {
        let programs = [
            super::super::dummy_epg::DummyProgram {
                start_time: utc("2026-09-11T16:00:00Z"),
                end_time: utc("2026-09-11T20:00:00Z"),
                title: "Channel A".into(),
                description: "No guide data.".into(),
            },
            super::super::dummy_epg::DummyProgram {
                start_time: utc("2026-09-11T20:00:00Z"),
                end_time: utc("2026-09-12T00:00:00Z"),
                title: "Channel A".into(),
                description: "Still no guide data.".into(),
            },
        ];
        let ctx = EpgContext {
            epg_data_id: None,
            ..context()
        };
        insta::assert_json_snapshot!(dummy_epg_listings(&programs, &ctx));
        assert!(dummy_epg_listings(&[], &ctx).epg_listings.is_empty());
    }

    #[test]
    fn account_info_json() {
        insta::assert_json_snapshot!(account_info(&AccountOptions {
            username: "bob",
            password: "s3cret",
            message: "dollet-relay XC API",
            host: "ipx.test",
            port: "9191",
            scheme: "http",
            active_connections: 1,
            max_connections: 4,
            now: utc("2026-09-11T15:45:00Z"),
        }));
    }

    #[test]
    fn vod_and_series_actions_are_empty_by_design() {
        assert_eq!(serde_json::to_string(&vod_categories()).unwrap(), "[]");
        assert_eq!(serde_json::to_string(&vod_streams()).unwrap(), "[]");
        assert_eq!(serde_json::to_string(&series_categories()).unwrap(), "[]");
        assert_eq!(serde_json::to_string(&series()).unwrap(), "[]");
    }
}
