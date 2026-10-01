//! Matching a channel to a guide entry by name.
//!
//! A fuzzy score runs through a threshold ladder. A language model could break
//! ties in the middle band, and **this build deliberately carries none**: that
//! is ONNX Runtime plus ~90 MB of weights for a tiebreak on a few dozen
//! channels. The band is reported as [`Outcome::Ambiguous`] instead of being
//! resolved or silently dropped, so it lands in the UI for one manual decision
//! rather than looking like "no guide exists for this channel".

use rapidfuzz::fuzz;
use std::sync::LazyLock;

use crate::domain::Id;

/// Words that say nothing about which channel this is.
const EXTRANEOUS: &[&str] = &[
    "tv",
    "channel",
    "network",
    "television",
    "east",
    "west",
    "hd",
    "uhd",
    "24/7",
    "1080p",
    "720p",
    "540p",
    "480p",
    "film",
    "movie",
    "movies",
];

static BRACKETED: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[.*?\]").expect("literal"));
static PARENTHESISED: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\(.*?\)").expect("literal"));
static CALL_SIGN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\(([A-Z]{3,5})\)").expect("literal"));
static PUNCTUATION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"[^\w\s]").expect("literal"));
static DOT_REGION: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\.([a-z]{2})").expect("literal"));

/// The `epg_settings` fields that shape normalization.
///
/// All three empty by default, and applied whenever they are not: there is no
/// mode switch gating them.
#[derive(Debug, Clone, Default)]
pub struct NormalizeSettings {
    pub ignore_prefixes: Vec<String>,
    pub ignore_suffixes: Vec<String>,
    pub ignore_custom: Vec<String>,
}

/// Reduce a channel or guide name to the form both sides are compared in.
///
/// Order matters: the ignore lists run against the original
/// casing, and the call sign is recovered from the original too, because it is
/// recognised by being upper-case.
pub fn normalize_name(name: &str, settings: &NormalizeSettings) -> String {
    if name.is_empty() {
        return String::new();
    }

    let mut result = name.to_string();

    // Only the first matching prefix and suffix are stripped, not all of them.
    for prefix in &settings.ignore_prefixes {
        if !prefix.is_empty() && result.starts_with(prefix.as_str()) {
            result = result[prefix.len()..].to_string();
            break;
        }
    }
    for suffix in &settings.ignore_suffixes {
        if !suffix.is_empty() && result.ends_with(suffix.as_str()) {
            result = result[..result.len() - suffix.len()].to_string();
            break;
        }
    }
    for custom in &settings.ignore_custom {
        if !custom.is_empty() {
            result = result.replace(custom.as_str(), "");
        }
    }

    let lowered = result.to_lowercase();
    let without_brackets = BRACKETED.replace_all(&lowered, "");

    // A call sign in parentheses is the most identifying part of a name, so it
    // is rescued before the parentheses are dropped. Matched on the *original*,
    // because upper case is how it is recognised.
    let call_sign = CALL_SIGN
        .captures(name)
        .map(|caps| format!(" {}", caps[1].to_lowercase()))
        .unwrap_or_default();

    let without_parens = PARENTHESISED.replace_all(&without_brackets, "");
    let rejoined = format!("{without_parens}{call_sign}");
    let depunctuated = PUNCTUATION.replace_all(&rejoined, "");

    depunctuated
        .split_whitespace()
        .filter(|token| !EXTRANEOUS.contains(token))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A guide entry to score against.
pub struct EpgCandidate<'a> {
    pub epg_data_id: Id,
    pub tvg_id: &'a str,
    pub name: &'a str,
    /// Higher wins a tie between sources.
    pub source_priority: i32,
}

/// The ladder's cut points.
///
/// Two sets: looser ones when a user matches a single channel by hand,
/// stricter ones for a bulk run where a wrong match is harder to notice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// At or above this, matched outright.
    pub high: f64,
    /// At or above this, matched: close enough that no tiebreak would change
    /// the answer.
    pub confident: f64,
    /// At or above this, ambiguous: reported for a human decision.
    pub medium: f64,
}

impl Thresholds {
    pub fn bulk() -> Self {
        Self {
            high: 90.0,
            confident: 80.0,
            medium: 50.0,
        }
    }

    pub fn single() -> Self {
        Self {
            high: 85.0,
            confident: 75.0,
            medium: 20.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Matched {
        epg_data_id: Id,
        score: f64,
    },
    /// Scored inside the ambiguous band.
    ///
    /// Deliberately not a match and deliberately not a miss: the UI offers the
    /// candidate for one human decision. Reported so the regression is visible
    /// rather than looking like a channel with no guide.
    Ambiguous {
        epg_data_id: Id,
        score: f64,
    },
    NoMatch,
}

/// Score every candidate and apply the ladder.
///
/// `region_code` is an ISO country code taken from the channel's own name or
/// group when the caller has one; it nudges candidates whose `tvg_id` carries a
/// matching country suffix, which is what separates `vrix.us` from `vrix.uk`.
pub fn best_match(
    channel_name: &str,
    candidates: &[EpgCandidate<'_>],
    settings: &NormalizeSettings,
    thresholds: Thresholds,
    region_code: Option<&str>,
) -> Outcome {
    let normalized = normalize_name(channel_name, settings);
    if normalized.is_empty() {
        return Outcome::NoMatch;
    }

    let mut best: Option<(f64, &EpgCandidate<'_>)> = None;
    for candidate in candidates {
        let candidate_name = normalize_name(candidate.name, settings);
        if candidate_name.is_empty() {
            continue;
        }
        let score = score_candidate(&normalized, &candidate_name, candidate, region_code);
        if score <= 0.0 {
            continue;
        }
        let better = match best {
            None => true,
            Some((best_score, best_candidate)) => {
                score > best_score
                    || (score == best_score
                        && candidate.source_priority > best_candidate.source_priority)
            }
        };
        if better {
            best = Some((score, candidate));
        }
    }

    let Some((score, candidate)) = best else {
        return Outcome::NoMatch;
    };
    if score >= thresholds.high || score >= thresholds.confident {
        Outcome::Matched {
            epg_data_id: candidate.epg_data_id,
            score,
        }
    } else if score >= thresholds.medium {
        Outcome::Ambiguous {
            epg_data_id: candidate.epg_data_id,
            score,
        }
    } else {
        Outcome::NoMatch
    }
}

fn score_candidate(
    channel: &str,
    candidate_name: &str,
    candidate: &EpgCandidate<'_>,
    region_code: Option<&str>,
) -> f64 {
    // `rapidfuzz` returns 0.0-1.0 here where the thresholds are on a 0-100
    // scale. Without the scaling every cut point in the ladder sits a hundred
    // times too high and nothing ever matches.
    let base = fuzz::ratio(channel.chars(), candidate_name.chars()) * 100.0;
    let Some(region) = region_code.filter(|_| !candidate.tvg_id.is_empty()) else {
        return base;
    };

    let combined = format!(
        "{} {}",
        candidate.tvg_id.to_lowercase(),
        candidate.name.to_lowercase()
    );
    let dot_regions: Vec<String> = DOT_REGION
        .captures_iter(&combined)
        .map(|caps| caps[1].to_string())
        .collect();

    let bonus = if !dot_regions.is_empty() {
        // A country suffix that disagrees is strong evidence against, which is
        // what stops `vrix.uk` winning a US channel.
        if dot_regions.iter().any(|found| found == region) {
            15.0
        } else {
            -15.0
        }
    } else if combined.contains(region) {
        10.0
    } else {
        0.0
    };
    base + bonus
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate<'a>(id: Id, tvg_id: &'a str, name: &'a str) -> EpgCandidate<'a> {
        EpgCandidate {
            epg_data_id: id,
            tvg_id,
            name,
            source_priority: 0,
        }
    }

    fn normalized(name: &str) -> String {
        normalize_name(name, &NormalizeSettings::default())
    }

    /// The score behind a decision, whichever side of the ladder it fell.
    fn score_of(outcome: Outcome) -> f64 {
        match outcome {
            Outcome::Matched { score, .. } | Outcome::Ambiguous { score, .. } => score,
            Outcome::NoMatch => 0.0,
        }
    }

    #[test]
    fn normalization_strips_what_says_nothing_about_the_channel() {
        let cases: &[(&str, &str)] = &[
            ("VRIX", "vrix"),
            ("VRIX HD", "vrix"),
            ("VRIX TV Network", "vrix"),
            ("US: VRIX", "us vrix"),
            ("CA:ZOR 1 HD", "cazor 1"),
            ("Sports [Backup]", "sports"),
            ("Sports (1080p)", "sports"),
            ("X.Y.Z.", "xyz"),
            ("  spaced   out  ", "spaced out"),
            ("", ""),
            // Every token is extraneous, so nothing survives.
            ("HD TV", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalized(input), *want, "{input}");
        }
    }

    #[test]
    fn a_call_sign_survives_the_parentheses_being_dropped() {
        // The most identifying part of the name, so it is rescued first.
        assert_eq!(normalized("Channel 5 (QVLA)"), "5 qvla");
        assert_eq!(normalized("News (WKRU) HD"), "news wkru");
        // Lower case in the original is not a call sign, just a parenthetical.
        assert_eq!(normalized("News (local)"), "news");
        // Too short and too long are both rejected.
        assert_eq!(normalized("News (AB)"), "news");
        assert_eq!(normalized("News (ABCDEF)"), "news");
    }

    #[test]
    fn the_ignore_lists_strip_only_the_first_match_each() {
        let settings = NormalizeSettings {
            ignore_prefixes: vec!["US: ".into(), "U".into()],
            ignore_suffixes: vec![" FHD".into(), "D".into()],
            ignore_custom: vec!["(D)".into()],
        };
        // The first prefix wins and the loop stops, so "U" is never applied.
        assert_eq!(normalize_name("US: VRIX FHD", &settings), "vrix");
        // A name matching only the second entry still gets stripped.
        assert_eq!(normalize_name("UVRIX", &settings), "vrix");
        // Custom strings are removed everywhere, not just once.
        assert_eq!(normalize_name("A (D) B (D)", &settings), "a b");

        // An empty entry is skipped rather than matching everything.
        let empty = NormalizeSettings {
            ignore_prefixes: vec![String::new(), "US: ".into()],
            ignore_suffixes: vec![String::new()],
            ignore_custom: vec![String::new()],
        };
        assert_eq!(normalize_name("US: VRIX", &empty), "vrix");
    }

    #[test]
    fn an_exact_name_matches_outright() {
        let candidates = [candidate(1, "vrix.us", "VRIX")];
        assert_eq!(
            best_match(
                "VRIX",
                &candidates,
                &NormalizeSettings::default(),
                Thresholds::bulk(),
                None
            ),
            Outcome::Matched {
                epg_data_id: 1,
                score: 100.0
            }
        );
    }

    #[test]
    fn the_ladder_matches_outright_and_skips_what_is_not_close() {
        // The two ends of it. The middle band has a test of its own, because
        // what happens there — reported, never guessed — is the decision this
        // whole design turns on.
        let settings = NormalizeSettings::default();
        let thresholds = Thresholds::bulk();

        // Identical but for a noise word: well above `high`.
        assert!(matches!(
            best_match(
                "Discovery Channel",
                &[candidate(1, "", "Discovery")],
                &settings,
                thresholds,
                None
            ),
            Outcome::Matched { epg_data_id: 1, .. }
        ));

        // Nothing alike: below `medium`, so not offered at all. Asserted as
        // `NoMatch` rather than as "not matched", which `Ambiguous` also
        // satisfies — and `Ambiguous` here would put Bloomberg in front of
        // someone as a candidate for Discovery Channel.
        assert_eq!(
            best_match(
                "Discovery Channel",
                &[candidate(1, "", "Bloomberg")],
                &settings,
                thresholds,
                None
            ),
            Outcome::NoMatch
        );
    }

    #[test]
    fn the_middle_band_is_reported_rather_than_guessed_or_dropped() {
        // A language model could break this tie; this build carries none,
        // deliberately. The
        // candidate has to reach the UI, or a channel that *does* have a guide
        // looks like one that does not.
        let settings = NormalizeSettings::default();
        let candidates = [candidate(7, "", "Vro Sport Arena")];
        let outcome = best_match(
            "Vro Sports Action",
            &candidates,
            &settings,
            Thresholds::bulk(),
            None,
        );
        assert_eq!(
            outcome,
            Outcome::Ambiguous {
                epg_data_id: 7,
                score: 75.0
            }
        );
        let band = Thresholds::bulk().medium..Thresholds::bulk().confident;
        assert!(
            band.contains(&score_of(outcome)),
            "75 sits in the ambiguous band"
        );
    }

    #[test]
    fn the_single_channel_thresholds_are_looser_than_the_bulk_ones() {
        // A hand-run match is supervised, so less certainty is acceptable.
        let bulk = Thresholds::bulk();
        let single = Thresholds::single();
        assert!(single.high < bulk.high);
        assert!(single.confident < bulk.confident);
        assert!(single.medium < bulk.medium);

        let settings = NormalizeSettings::default();
        let candidates = [candidate(1, "", "Vro Sport Arena")];
        let channel = "Vro Sports Action";
        // One score, 75, that the two ladders read differently.
        assert_eq!(
            best_match(channel, &candidates, &settings, single, None),
            Outcome::Matched {
                epg_data_id: 1,
                score: 75.0
            }
        );
        assert_eq!(
            best_match(channel, &candidates, &settings, bulk, None),
            Outcome::Ambiguous {
                epg_data_id: 1,
                score: 75.0
            }
        );
    }

    #[test]
    fn nothing_to_match_against_is_no_match() {
        let settings = NormalizeSettings::default();
        let thresholds = Thresholds::bulk();
        assert_eq!(
            best_match("VRIX", &[], &settings, thresholds, None),
            Outcome::NoMatch
        );
        // A channel whose name normalizes away entirely.
        assert_eq!(
            best_match(
                "HD TV",
                &[candidate(1, "", "VRIX")],
                &settings,
                thresholds,
                None
            ),
            Outcome::NoMatch
        );
        // A candidate whose name normalizes away is skipped, not scored.
        assert_eq!(
            best_match(
                "VRIX",
                &[candidate(1, "", "HD")],
                &settings,
                thresholds,
                None
            ),
            Outcome::NoMatch
        );
        // A candidate sharing no characters scores zero and is skipped
        // before it can become the best match.
        assert_eq!(
            best_match(
                "VRIX",
                &[candidate(1, "", "wxyz")],
                &settings,
                thresholds,
                None
            ),
            Outcome::NoMatch
        );
        assert_eq!(score_of(Outcome::NoMatch), 0.0);
    }

    #[test]
    fn the_region_code_separates_two_countries_of_the_same_brand() {
        let settings = NormalizeSettings::default();
        let candidates = [
            candidate(1, "vrix.uk", "VRIX"),
            candidate(2, "vrix.us", "VRIX"),
        ];
        // Without a region the first wins on a tie, which is the wrong VRIX.
        assert_eq!(
            best_match("VRIX", &candidates, &settings, Thresholds::bulk(), None),
            Outcome::Matched {
                epg_data_id: 1,
                score: 100.0
            }
        );
        // With one, the country suffix decides.
        assert_eq!(
            best_match(
                "VRIX",
                &candidates,
                &settings,
                Thresholds::bulk(),
                Some("us")
            ),
            Outcome::Matched {
                epg_data_id: 2,
                score: 115.0
            }
        );
    }

    #[test]
    fn a_region_mentioned_without_a_dot_suffix_is_a_weaker_signal() {
        let settings = NormalizeSettings::default();
        // No `.xx` anywhere, but the region appears in the name. The names are
        // far enough apart that the bonus decides the band rather than the
        // match, which is the point: +10 is a nudge, +15 is evidence.
        let candidates = [candidate(1, "vrix", "VRIX us feed")];
        let with_region = score_of(best_match(
            "VRIX",
            &candidates,
            &settings,
            Thresholds::bulk(),
            Some("us"),
        ));
        let without_region = score_of(best_match(
            "VRIX",
            &candidates,
            &settings,
            Thresholds::bulk(),
            None,
        ));
        assert_eq!(with_region - without_region, 10.0);

        // A tvg_id with no country suffix and no mention of the region gets
        // no adjustment either way.
        let unrelated = [candidate(1, "vrix", "VRIX")];
        assert_eq!(
            score_of(best_match(
                "VRIX",
                &unrelated,
                &settings,
                Thresholds::bulk(),
                Some("us")
            )),
            100.0
        );

        // A candidate with no tvg_id gets no region adjustment at all.
        let bare = [candidate(1, "", "VRIX")];
        assert_eq!(
            best_match("VRIX", &bare, &settings, Thresholds::bulk(), Some("us")),
            Outcome::Matched {
                epg_data_id: 1,
                score: 100.0
            }
        );
    }

    #[test]
    fn source_priority_breaks_a_tie() {
        let settings = NormalizeSettings::default();
        let candidates = [
            candidate(1, "", "VRIX"),
            EpgCandidate {
                source_priority: 5,
                ..candidate(2, "", "VRIX")
            },
        ];
        assert_eq!(
            best_match("VRIX", &candidates, &settings, Thresholds::bulk(), None),
            Outcome::Matched {
                epg_data_id: 2,
                score: 100.0
            }
        );

        // And a lower-priority candidate does not displace an equal one.
        let reversed = [
            EpgCandidate {
                source_priority: 5,
                ..candidate(2, "", "VRIX")
            },
            candidate(1, "", "VRIX"),
        ];
        assert_eq!(
            best_match("VRIX", &reversed, &settings, Thresholds::bulk(), None),
            Outcome::Matched {
                epg_data_id: 2,
                score: 100.0
            }
        );
    }

    #[test]
    fn a_region_penalty_can_drive_a_candidate_out_of_contention() {
        let settings = NormalizeSettings::default();
        // The only candidate is the wrong country, and -15 drops it into the
        // ambiguous band rather than letting it match outright.
        let candidates = [candidate(1, "vrix.uk", "VRIX")];
        let outcome = best_match(
            "VRIX",
            &candidates,
            &settings,
            Thresholds::bulk(),
            Some("us"),
        );
        assert_eq!(
            outcome,
            Outcome::Matched {
                epg_data_id: 1,
                score: 85.0
            },
            "85 is still above `confident`, so it matches -- but by a narrower margin"
        );
    }
}
