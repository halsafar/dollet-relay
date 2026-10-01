//! Turning a provider's stream into a channel: its name and its number.
//!
//! Both halves are the group's configuration applied to one stream, and both
//! are where a refresh can quietly renumber a user's lineup. Plex addresses
//! channels by number, so a number that moves is a channel the user's
//! recordings and favourites no longer point at.

use crate::regex_compat::js_backrefs_to_rust;

/// How a group allocates numbers to the channels it creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberingMode {
    /// The provider's own number wins when it is free. The range takes only
    /// the streams the provider did not number, and the collisions.
    Provider,
    /// The lowest free number from 1. Kept for links imported with
    /// it; `end` deliberately does not apply, because the UI for this mode
    /// exposed no range and a stale stored `end` must not cap it.
    NextAvailable,
    /// The range, appended to on the step grid.
    Fixed,
}

pub struct Numbering {
    pub mode: NumberingMode,
    /// Where the range begins, and where `Provider` falls back to.
    pub start: f64,
    pub end: Option<f64>,
    /// Spacing between the numbers a range hands out: a new channel takes the
    /// next multiple of this above the range's highest. 1 is plain append.
    pub step: f64,
}

/// The lowest free number at or above `from`, counting by one.
///
/// Used by `NextAvailable` only: filling the lowest gap hands a slot the
/// operator left on purpose — or a deleted channel's number — to whatever
/// stream arrives next, which is exactly the surprise a curated lineup does
/// not want. The range rule is [`next_slot`].
pub fn next_available(used: &[f64], from: f64, end: Option<f64>) -> Option<f64> {
    let mut candidate = from;
    while is_used(used, candidate) {
        candidate += 1.0;
        if end.is_some_and(|limit| candidate > limit) {
            return None;
        }
    }
    if end.is_some_and(|limit| candidate > limit) {
        return None;
    }
    Some(candidate)
}

/// The smallest multiple of `step` at or above `value`.
pub fn align_up(value: f64, step: f64) -> f64 {
    (value / step).ceil() * step
}

fn grid_step(step: f64) -> f64 {
    if step.is_finite() && step >= 1.0 {
        step
    } else {
        1.0
    }
}

/// The slot a new channel takes in a range.
///
/// The next multiple of `step` above the highest number the *group* holds in
/// the range — appending, so a gap left on purpose stays free and a deleted
/// channel's number is not handed to the next arrival — stepping over any
/// number another group holds there. Only when that would pass the end does a
/// free grid slot inside the range get used, and only when there is none is
/// the range full. A hand-placed `104.1` counts like any other number: with
/// step 1 the next channel is 105.
///
/// `own` is the group's numbers, `used` every number in the lineup (the
/// group's included); both sorted.
pub fn next_slot(
    own: &[f64],
    used: &[f64],
    start: f64,
    end: Option<f64>,
    step: f64,
) -> Option<f64> {
    let step = grid_step(step);
    let in_range = |number: f64| number >= start && end.is_none_or(|limit| number <= limit);

    let highest = own
        .iter()
        .copied()
        .filter(|number| in_range(*number))
        .fold(None, |best: Option<f64>, number| {
            Some(best.map_or(number, |b| b.max(number)))
        });
    let mut candidate = match highest {
        None => align_up(start, step),
        Some(highest) => (highest / step).floor() * step + step,
    };
    while in_range(candidate) {
        if !is_used(used, candidate) {
            return Some(candidate);
        }
        candidate += step;
    }

    let mut probe = align_up(start, step);
    while in_range(probe) {
        if !is_used(used, probe) {
            return Some(probe);
        }
        probe += step;
    }
    None
}

/// The number a stream should claim, or `None` when the range is exhausted.
///
/// `own` is what the group already holds, `used` what the whole lineup holds;
/// both sorted, and the caller keeps them that way as it allocates.
pub fn pick_number(
    numbering: &Numbering,
    provider_number: Option<f64>,
    own: &[f64],
    used: &[f64],
) -> Option<f64> {
    match numbering.mode {
        NumberingMode::Provider => match provider_number {
            // The provider's number is authoritative whenever it is free.
            Some(number) if number.is_finite() && !is_used(used, number) => Some(number),
            _ => next_slot(own, used, numbering.start, numbering.end, numbering.step),
        },
        NumberingMode::NextAvailable => next_available(used, 1.0, None),
        NumberingMode::Fixed => {
            next_slot(own, used, numbering.start, numbering.end, numbering.step)
        }
    }
}

/// Keep a sorted number list sorted as a number is claimed.
pub fn claim(numbers: &mut Vec<f64>, number: f64) {
    numbers.insert(numbers.partition_point(|probe| *probe < number), number);
}

fn is_used(used: &[f64], number: f64) -> bool {
    used.binary_search_by(|probe| probe.total_cmp(&number))
        .is_ok()
}

/// The group's rename rule, as the UI stores it.
pub struct Rename<'a> {
    pub pattern: &'a str,
    /// A `None` replacement deletes what the pattern matched, which is how the
    /// UI expresses "strip this prefix".
    pub replacement: Option<&'a str>,
    /// `Channel.name`'s column width. A rename that expands past it would
    /// otherwise fail the insert and abort the whole sync, so it is capped here
    /// and the UI preview applies the same cap.
    pub max_length: usize,
}

/// Apply a group's rename to one stream name.
///
/// A pattern that will not compile leaves the name untouched rather than
/// failing the sync: a broken rule costs the user a tidy
/// name, not their channels.
pub fn rename(name: &str, rule: &Rename<'_>) -> String {
    // Only the *search* pattern is rewritten. Rust's replacement syntax is
    // already `$1`, so converting a replacement template would emit the literal
    // text `\1` and break every rename that uses a capture.
    let Ok(regex) = fancy_regex::Regex::new(&js_backrefs_to_rust(rule.pattern)) else {
        return truncate(name, rule.max_length);
    };
    let replaced = regex.replace_all(name, rule.replacement.unwrap_or(""));
    truncate(&replaced, rule.max_length)
}

/// Truncate on a character boundary, so a multi-byte name cannot panic.
fn truncate(value: &str, max_length: usize) -> String {
    match value.char_indices().nth(max_length) {
        Some((index, _)) => value[..index].to_string(),
        None => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME_MAX: usize = 255;

    /// `(used, from, end, expected)`.
    type NumberCase<'a> = (&'a [f64], f64, Option<f64>, Option<f64>);

    fn numbering(mode: NumberingMode, start: f64, end: Option<f64>) -> Numbering {
        Numbering {
            mode,
            start,
            end,
            step: 1.0,
        }
    }

    fn renamed(name: &str, pattern: &str, replacement: Option<&str>) -> String {
        rename(
            name,
            &Rename {
                pattern,
                replacement,
                max_length: NAME_MAX,
            },
        )
    }

    #[test]
    fn the_next_free_number_skips_what_is_taken() {
        let cases: &[NumberCase<'_>] = &[
            (&[], 1.0, None, Some(1.0)),
            (&[1.0], 1.0, None, Some(2.0)),
            (&[1.0, 2.0, 4.0], 1.0, None, Some(3.0)),
            (&[1.0, 2.0, 3.0], 1.0, Some(3.0), None),
            // Exactly at the limit is allowed; one past it is not.
            (&[1.0, 2.0], 1.0, Some(3.0), Some(3.0)),
            (&[], 5.0, Some(3.0), None),
            // Fractional numbers are legal and are matched exactly.
            (&[1.5], 1.5, None, Some(2.5)),
        ];
        for (used, from, end, want) in cases {
            assert_eq!(next_available(used, *from, *end), *want, "{used:?} {from}");
        }
    }

    #[test]
    fn provider_mode_prefers_the_providers_own_number() {
        let rules = numbering(NumberingMode::Provider, 100.0, Some(200.0));
        assert_eq!(pick_number(&rules, Some(7.0), &[], &[]), Some(7.0));

        // Taken: fall into the configured range rather than colliding.
        assert_eq!(pick_number(&rules, Some(7.0), &[7.0], &[7.0]), Some(100.0));

        // Unnumbered by the provider: same fallback.
        assert_eq!(pick_number(&rules, None, &[], &[]), Some(100.0));
        assert_eq!(pick_number(&rules, None, &[100.0], &[100.0]), Some(101.0));
    }

    #[test]
    fn next_available_mode_ignores_the_configured_range() {
        // Its UI exposes no range, so a stale stored `end` must not cap it.
        let rules = numbering(NumberingMode::NextAvailable, 500.0, Some(2.0));
        assert_eq!(pick_number(&rules, Some(9.0), &[], &[]), Some(1.0));
        assert_eq!(
            pick_number(&rules, None, &[1.0, 2.0], &[1.0, 2.0]),
            Some(3.0)
        );
    }

    #[test]
    fn fixed_mode_appends_within_the_range_and_respects_the_end() {
        let rules = numbering(NumberingMode::Fixed, 10.0, Some(12.0));
        assert_eq!(pick_number(&rules, Some(9.0), &[], &[]), Some(10.0));
        assert_eq!(
            pick_number(&rules, None, &[10.0, 11.0], &[10.0, 11.0]),
            Some(12.0)
        );
        assert_eq!(
            pick_number(&rules, None, &[10.0, 11.0, 12.0], &[10.0, 11.0, 12.0]),
            None
        );
    }

    /// Filling the lowest free number would hand a gap the operator left on
    /// purpose — or a deleted channel's slot — to whatever stream arrives
    /// next. This appends: the gap is the operator's.
    /// `own` and `used` are the same list: the group is alone in its range.
    fn slot(own: &[f64], start: f64, end: Option<f64>, step: f64) -> Option<f64> {
        next_slot(own, own, start, end, step)
    }

    #[test]
    fn a_new_number_appends_after_the_ranges_highest_leaving_gaps_alone() {
        assert_eq!(
            slot(&[100.0, 101.0, 105.0], 100.0, Some(199.0), 1.0),
            Some(106.0)
        );
        // Numbers outside the range do not count as its highest.
        assert_eq!(
            slot(&[1.0, 2.0, 500.0], 100.0, Some(199.0), 1.0),
            Some(100.0)
        );
        // A hand-placed sub-number is a number like any other.
        assert_eq!(slot(&[100.0, 104.1], 100.0, None, 1.0), Some(105.0));
    }

    /// Another group's channel sitting inside this range — an overlap, or a
    /// number from before the range existed — is stepped over, not appended
    /// after: the range is this group's, and its highest is what counts.
    #[test]
    fn a_stray_number_from_another_group_is_avoided_not_built_on() {
        let own = [100.0, 101.0];
        assert_eq!(
            next_slot(&own, &[100.0, 101.0, 150.0], 100.0, Some(199.0), 1.0),
            Some(102.0)
        );
        assert_eq!(
            next_slot(&own, &[100.0, 101.0, 102.0], 100.0, Some(199.0), 1.0),
            Some(103.0)
        );
        // With nothing of its own yet, the group starts at its start, past
        // whatever squats there.
        assert_eq!(
            next_slot(&[], &[500.0, 501.0], 500.0, None, 1.0),
            Some(502.0)
        );
    }

    #[test]
    fn the_step_snaps_new_numbers_to_its_grid() {
        // 105 was placed by hand; the next automatic number is the next
        // multiple of ten, not 115.
        assert_eq!(slot(&[100.0, 105.0], 100.0, Some(199.0), 10.0), Some(110.0));
        assert_eq!(slot(&[100.0, 110.0], 100.0, Some(199.0), 10.0), Some(120.0));
        // An empty range starts on the grid, even from an odd start.
        assert_eq!(slot(&[], 100.0, None, 10.0), Some(100.0));
        assert_eq!(slot(&[], 105.0, None, 10.0), Some(110.0));
        // A step below one is plain append; nothing should ever divide by it.
        assert_eq!(slot(&[7.0], 1.0, None, 0.0), Some(8.0));
    }

    #[test]
    fn a_range_fills_a_grid_gap_only_once_its_end_is_reached() {
        // 130 would pass the end, so the free slot at 110 is used; once every
        // grid slot is taken the range is full and nothing wraps.
        assert_eq!(slot(&[100.0, 120.0], 100.0, Some(120.0), 10.0), Some(110.0));
        assert_eq!(slot(&[100.0, 110.0, 120.0], 100.0, Some(120.0), 10.0), None);
        // Off-grid numbers between slots do not make the slots taken.
        assert_eq!(
            slot(&[100.0, 115.0, 120.0], 100.0, Some(120.0), 10.0),
            Some(110.0)
        );
    }

    #[test]
    fn an_exhausted_range_reports_rather_than_wrapping() {
        // Silently wrapping would renumber channels the user already has.
        let rules = numbering(NumberingMode::Provider, 1.0, Some(2.0));
        assert_eq!(pick_number(&rules, None, &[1.0, 2.0], &[1.0, 2.0]), None);
    }

    #[test]
    fn renaming_substitutes_capture_groups() {
        // Rust's replacement syntax is `$1`, the same as the JS the UI is
        // authored in, so a capture must survive untouched.
        let cases: &[(&str, &str, Option<&str>, &str)] = &[
            ("US: VRIX", r"^US:\s*", Some(""), "VRIX"),
            ("US: VRIX", r"^US:\s*", None, "VRIX"),
            (
                "CA:ZOR 1 HD",
                r"^(\w+):(.*)$",
                Some("$2 ($1)"),
                "ZOR 1 HD (CA)",
            ),
            ("Sports HD", r"\s*HD$", Some(""), "Sports"),
            ("a b c", r"\s", Some("-"), "a-b-c"),
            // A named group works too.
            ("US: VRIX", r"^\w+:\s*(?<rest>.*)$", Some("$rest"), "VRIX"),
        ];
        for (name, pattern, replacement, want) in cases {
            assert_eq!(renamed(name, pattern, *replacement), *want, "{name}");
        }
    }

    #[test]
    fn a_js_backreference_in_the_search_pattern_is_rewritten() {
        assert_eq!(
            renamed("echo echo tail", r"(\w+) $1", Some("once")),
            "once tail"
        );
    }

    #[test]
    fn a_replacement_template_is_left_alone() {
        // The direction that matters: `$1` becomes `\1` in search patterns,
        // where it is a JavaScript-dialect backreference. Rust's replacement
        // syntax is already `$1`, and converting would emit the literal text
        // `\1`.
        let got = renamed("US: VRIX", r"^(\w+): (.*)$", Some("$1/$2"));
        assert_eq!(got, "US/VRIX");
        assert!(!got.contains('\\'));
    }

    #[test]
    fn an_uncompilable_pattern_leaves_the_name_untouched() {
        assert_eq!(renamed("US: VRIX", "(unclosed", Some("x")), "US: VRIX");
    }

    #[test]
    fn a_rename_is_capped_at_the_column_width() {
        let rule = Rename {
            pattern: "^",
            replacement: Some("prefix "),
            max_length: 10,
        };
        assert_eq!(rename("channel name", &rule), "prefix cha");

        // And the cap lands on a character boundary, not inside one.
        let wide = Rename {
            pattern: "^",
            replacement: Some(""),
            max_length: 3,
        };
        assert_eq!(
            rename("\u{e9}\u{e9}\u{e9}\u{e9}", &wide),
            "\u{e9}\u{e9}\u{e9}"
        );

        // A name already within the cap is untouched.
        assert_eq!(rename("short", &wide), "sho");
        let generous = Rename {
            max_length: 255,
            ..wide
        };
        assert_eq!(rename("short", &generous), "short");
    }

    #[test]
    fn allocating_a_whole_group_in_order() {
        // The realistic shape: walk the streams, taking each number and adding
        // it to the used set, which is what the caller does.
        // The last two exhaust the range, so the walk also covers a stream
        // that gets no number at all.
        let rules = numbering(NumberingMode::Provider, 100.0, Some(102.0));
        let provider_numbers = [Some(3.0), Some(3.0), None, Some(9.0), None, None, Some(3.0)];

        let mut used: Vec<f64> = Vec::new();
        let mut allocated = Vec::new();
        for provider in provider_numbers {
            let number = pick_number(&rules, provider, &used, &used);
            if let Some(number) = number {
                claim(&mut used, number);
            }
            allocated.push(number);
        }
        assert_eq!(
            allocated,
            [
                Some(3.0),
                Some(100.0),
                Some(101.0),
                Some(9.0),
                Some(102.0),
                None,
                None,
            ]
        );
    }
}
