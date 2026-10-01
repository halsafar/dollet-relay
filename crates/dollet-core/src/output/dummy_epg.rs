//! Synthetic guide programmes for channels with no real EPG.
//!
//! Without this, a channel whose EPG source is `dummy` — or that has no source
//! at all — has no `<programme>` elements, and Plex hides it from the guide
//! entirely. The channel is still tunable, but a user browsing the guide cannot
//! find it, which reads as the channel having disappeared.
//!
//! Programmes are fixed-length blocks aligned to the hour, generated on demand
//! rather than stored: they are a function of the channel name and the clock, so
//! persisting them would only create rows to expire.

use chrono::{DateTime, Duration, Timelike, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DummyProgram {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub title: String,
    pub description: String,
}

pub struct DummyOptions<'a> {
    /// Days of programmes to generate forward from `now`.
    pub num_days: u32,
    /// Block length, four hours by default.
    pub program_length_hours: u32,
    /// Drop blocks that end at or before this, so an export does not carry
    /// programmes older than the window the client asked for.
    pub export_lookback: Option<DateTime<Utc>>,
    /// Drop blocks starting at or after this.
    pub export_cutoff: Option<DateTime<Utc>>,
    /// Stop after this many blocks, for the Xtream short-EPG action.
    pub max_programs: Option<usize>,
    /// Replaces the channel name as every block's title.
    pub fallback_title: Option<&'a str>,
    /// Replaces the generated description on every block.
    pub fallback_description: Option<&'a str>,
}

impl Default for DummyOptions<'_> {
    fn default() -> Self {
        Self {
            num_days: 3,
            program_length_hours: 4,
            export_lookback: None,
            export_cutoff: None,
            max_programs: None,
            fallback_title: None,
            fallback_description: None,
        }
    }
}

/// Placeholder descriptions, chosen by time of day so a guide grid does not
/// show the same sentence in every cell.
const DESCRIPTIONS: [(u32, u32, [&str; 3]); 6] = [
    (
        0,
        4,
        [
            "Overnight on {channel}. Nothing scheduled, everything still on.",
            "{channel} keeps the lights on while the schedule sleeps.",
            "Late night filler on {channel}.",
        ],
    ),
    (
        4,
        8,
        [
            "Early hours on {channel}. The guide has not woken up yet.",
            "{channel}, before anyone published a schedule.",
            "Morning placeholder on {channel}.",
        ],
    ),
    (
        8,
        12,
        [
            "Late morning on {channel}, guide data pending.",
            "{channel} is broadcasting; the schedule is not.",
            "Daytime placeholder on {channel}.",
        ],
    ),
    (
        12,
        16,
        [
            "Afternoon on {channel}. No listing was published for this slot.",
            "{channel} continues, unannounced.",
            "Midday placeholder on {channel}.",
        ],
    ),
    (
        16,
        20,
        [
            "Early evening on {channel}, no guide data available.",
            "{channel} is on air without a listing.",
            "Evening placeholder on {channel}.",
        ],
    ),
    (
        20,
        24,
        [
            "Prime time on {channel}, schedule unknown.",
            "{channel} in the evening. The guide stayed quiet.",
            "Night-time placeholder on {channel}.",
        ],
    ),
];

/// Generate the blocks for one channel.
///
/// `now` is truncated to the hour so every channel's blocks line up in the grid
/// and so repeated calls within an hour return identical programmes — a client
/// that re-fetches the guide must not see the whole schedule shift.
pub fn generate(
    channel_name: &str,
    now: DateTime<Utc>,
    opts: &DummyOptions<'_>,
) -> Vec<DummyProgram> {
    let step = opts.program_length_hours.max(1);
    // Subtracting the elapsed part of the hour rather than using `with_minute`
    // keeps this total: the setter variants are fallible for no reachable reason.
    let into_hour = i64::from(now.minute()) * 60 + i64::from(now.second());
    let start_of_run =
        now - Duration::seconds(into_hour) - Duration::nanoseconds(i64::from(now.nanosecond()));

    let mut programs = Vec::new();
    for day in 0..opts.num_days {
        let day_start = start_of_run + Duration::days(i64::from(day));
        for hour_offset in (0..24).step_by(step as usize) {
            if opts.max_programs.is_some_and(|max| programs.len() >= max) {
                return programs;
            }
            let start_time = day_start + Duration::hours(i64::from(hour_offset));
            let end_time = start_time + Duration::hours(i64::from(step));
            if opts.export_lookback.is_some_and(|cut| end_time <= cut) {
                continue;
            }
            if opts.export_cutoff.is_some_and(|cut| start_time >= cut) {
                continue;
            }
            programs.push(DummyProgram {
                start_time,
                end_time,
                title: opts.fallback_title.unwrap_or(channel_name).to_string(),
                description: match opts.fallback_description {
                    Some(text) => text.to_string(),
                    None => describe(channel_name, start_time.hour(), day),
                },
            });
        }
    }
    programs
}

fn describe(channel_name: &str, hour: u32, day: u32) -> String {
    let Some((_, _, options)) = DESCRIPTIONS
        .iter()
        .find(|(lo, hi, _)| hour >= *lo && hour < *hi)
    else {
        return format!("No guide data is available for {channel_name}.");
    };
    options[((hour + day) as usize) % options.len()].replace("{channel}", channel_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn now() -> DateTime<Utc> {
        utc("2026-09-11T15:37:41Z")
    }

    fn summarize(programs: &[DummyProgram]) -> String {
        programs
            .iter()
            .map(|p| {
                format!(
                    "{} -> {} | {} | {}",
                    p.start_time.format("%Y-%m-%d %H:%M"),
                    p.end_time.format("%Y-%m-%d %H:%M"),
                    p.title,
                    p.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_single_day_of_four_hour_blocks() {
        let opts = DummyOptions {
            num_days: 1,
            ..DummyOptions::default()
        };
        insta::assert_snapshot!(summarize(&generate("Channel A", now(), &opts)));
    }

    #[test]
    fn three_days_of_two_hour_blocks() {
        let opts = DummyOptions {
            num_days: 3,
            program_length_hours: 2,
            ..DummyOptions::default()
        };
        let programs = generate("Channel A", now(), &opts);
        assert_eq!(programs.len(), 36);
        insta::assert_snapshot!(summarize(&programs[..14]));
    }

    #[test]
    fn blocks_are_aligned_to_the_hour_and_stable_within_it() {
        let a = generate("A", utc("2026-09-11T15:00:00Z"), &DummyOptions::default());
        let b = generate(
            "A",
            utc("2026-09-11T15:59:59.999Z"),
            &DummyOptions::default(),
        );
        assert_eq!(a, b);
        assert_eq!(a[0].start_time, utc("2026-09-11T15:00:00Z"));
        assert_eq!(a[0].end_time, utc("2026-09-11T19:00:00Z"));
    }

    #[test]
    fn the_export_window_trims_both_ends() {
        let opts = DummyOptions {
            num_days: 2,
            export_lookback: Some(utc("2026-09-12T00:00:00Z")),
            export_cutoff: Some(utc("2026-09-12T12:00:00Z")),
            ..DummyOptions::default()
        };
        let programs = generate("Channel A", now(), &opts);
        assert!(
            programs
                .iter()
                .all(|p| p.end_time > utc("2026-09-12T00:00:00Z"))
        );
        assert!(
            programs
                .iter()
                .all(|p| p.start_time < utc("2026-09-12T12:00:00Z"))
        );
        insta::assert_snapshot!(summarize(&programs));
    }

    #[test]
    fn max_programs_caps_the_run() {
        let opts = DummyOptions {
            num_days: 7,
            max_programs: Some(4),
            ..DummyOptions::default()
        };
        let programs = generate("Channel A", now(), &opts);
        assert_eq!(programs.len(), 4);

        // The cap is also honoured across a day boundary.
        let across_days = DummyOptions {
            num_days: 7,
            max_programs: Some(8),
            program_length_hours: 6,
            ..DummyOptions::default()
        };
        assert_eq!(generate("Channel A", now(), &across_days).len(), 8);
    }

    #[test]
    fn fallback_templates_replace_the_generated_text() {
        let opts = DummyOptions {
            num_days: 1,
            fallback_title: Some("Always On"),
            fallback_description: Some("Check the provider's own guide."),
            ..DummyOptions::default()
        };
        let programs = generate("Channel A", now(), &opts);
        assert!(programs.iter().all(|p| p.title == "Always On"));
        assert!(
            programs
                .iter()
                .all(|p| p.description == "Check the provider's own guide.")
        );
    }

    #[test]
    fn descriptions_vary_by_time_band_and_day() {
        let opts = DummyOptions {
            num_days: 2,
            program_length_hours: 1,
            ..DummyOptions::default()
        };
        let programs = generate("A", utc("2026-09-11T00:00:00Z"), &opts);
        let distinct: std::collections::BTreeSet<_> =
            programs.iter().map(|p| p.description.clone()).collect();
        assert!(distinct.len() > 6, "expected variety, got {distinct:?}");
        assert!(programs.iter().all(|p| p.description.contains('A')));
    }

    #[test]
    fn a_default_export_gives_every_slot_its_own_sentence() {
        // A guide grid shows a dummy channel's blocks side by side, so a
        // sentence repeated across a day reads as a broken guide rather than a
        // placeholder. Every one of the eighteen blocks in a default export
        // gets its own.
        let programs = generate("A", utc("2026-09-12T05:29:19Z"), &DummyOptions::default());
        assert_eq!(programs.len(), 18);

        let distinct: std::collections::BTreeSet<_> =
            programs.iter().map(|p| p.description.as_str()).collect();
        assert_eq!(distinct.len(), 18, "every slot differs");

        // It holds because the six four-hour bands each recur once a day and
        // the variant rotates with the day, so no (band, variant) pair repeats
        // inside a three-day run. A fourth day would start reusing them, which
        // is the default export length rather than a limit worth engineering
        // around.
        let four_days = generate(
            "A",
            utc("2026-09-12T05:29:19Z"),
            &DummyOptions {
                num_days: 4,
                ..DummyOptions::default()
            },
        );
        let distinct: std::collections::BTreeSet<_> =
            four_days.iter().map(|p| p.description.as_str()).collect();
        assert_eq!(four_days.len(), 24);
        assert_eq!(distinct.len(), 18, "the fourth day reuses the first day's");
    }

    #[test]
    fn every_hour_of_the_day_has_a_description() {
        for hour in 0..24 {
            for day in 0..3 {
                assert!(
                    !describe("A", hour, day).is_empty(),
                    "hour {hour} day {day}"
                );
            }
        }
        // Out of range only reachable if the table ever loses a band.
        assert_eq!(describe("A", 24, 0), "No guide data is available for A.");
    }

    #[test]
    fn degenerate_options_produce_nothing_rather_than_looping() {
        let none = DummyOptions {
            num_days: 0,
            ..DummyOptions::default()
        };
        assert!(generate("A", now(), &none).is_empty());

        // A zero-hour block would divide the day into infinitely many slots.
        let zero_length = DummyOptions {
            num_days: 1,
            program_length_hours: 0,
            ..DummyOptions::default()
        };
        assert_eq!(generate("A", now(), &zero_length).len(), 24);

        let capped_at_zero = DummyOptions {
            num_days: 1,
            max_programs: Some(0),
            ..DummyOptions::default()
        };
        assert!(generate("A", now(), &capped_at_zero).is_empty());

        let window_excludes_everything = DummyOptions {
            num_days: 1,
            export_cutoff: Some(utc("2020-01-01T00:00:00Z")),
            ..DummyOptions::default()
        };
        assert!(generate("A", now(), &window_excludes_everything).is_empty());
    }
}
