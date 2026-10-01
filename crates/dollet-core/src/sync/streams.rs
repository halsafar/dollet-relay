//! What a refreshed playlist means against the streams already stored.
//!
//! The distinction this module exists to preserve is between *missing today*
//! and *gone*. A stream is marked stale the moment a refresh does not
//! mention it, but only deletes after `stale_stream_days` — so a provider
//! serving a truncated playlist for an hour costs the user nothing, where a
//! delete-on-absence policy would cost them their whole lineup and every
//! channel assignment hanging off it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};

use crate::domain::Id;
use crate::parse::m3u::M3uEntry;

/// The fields a refresh can rewrite on an existing stream.
///
/// Comparing these is what decides between writing every column and only
/// touching `last_seen`, which at 61 streams is cosmetic but at 50,000 is the
/// difference between a refresh that finishes and one that does not.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamFields {
    pub name: String,
    pub url: String,
    pub logo_url: String,
    pub tvg_id: String,
    pub group: String,
    pub is_adult: bool,
    /// The provider's own id, distinct from our row id.
    pub provider_stream_id: Option<i64>,
    pub provider_channel_number: Option<f64>,
    pub is_catchup: bool,
    pub catchup_days: u32,
}

impl StreamFields {
    /// Read a parsed playlist entry the way the ingest path does.
    ///
    /// `group` is passed in because an entry with no `group-title` inherits the
    /// account's default, which this module has no view of.
    pub fn from_entry(entry: &M3uEntry, default_group: &str) -> Self {
        Self {
            name: entry.name.clone(),
            url: entry.url.clone(),
            logo_url: entry.tvg_logo().unwrap_or_default().to_string(),
            tvg_id: entry.tvg_id().unwrap_or_default().to_string(),
            group: entry.group_title().unwrap_or(default_group).to_string(),
            is_adult: entry.is_adult(),
            provider_stream_id: entry.attr("stream_id").and_then(|v| v.trim().parse().ok()),
            provider_channel_number: entry.tvg_chno(),
            is_catchup: entry.is_catchup(),
            catchup_days: entry.catchup_days(),
        }
    }
}

/// A stream already in the database, as far as reconciliation is concerned.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingStream {
    pub id: Id,
    pub hash: String,
    pub last_seen: DateTime<Utc>,
    pub fields: StreamFields,
}

/// One entry from the refreshed playlist, already hashed and filtered.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedStream {
    pub hash: String,
    pub fields: StreamFields,
}

pub struct ReconcileOptions<'a> {
    /// When this refresh started. Everything seen is stamped with it, and
    /// everything older was missed.
    pub started_at: DateTime<Utc>,
    /// `stale_stream_days`. A stream missing longer than this is deleted.
    pub stale_after: Duration,
    /// Groups still enabled for the account. A stream in a group the user has
    /// since disabled is deleted regardless of age.
    pub active_groups: &'a BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteReason {
    /// Missing for longer than the retention window.
    Expired,
    /// Its group is no longer enabled for this account.
    GroupDisabled,
}

/// What the caller should write. Grouped by operation rather than returned as a
/// flat list so each can be committed as one batch — the schema invariant is
/// that bulk work never runs in a single long transaction.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// Indices into `parsed` whose hash is not in the database yet.
    pub insert: Vec<usize>,
    /// Existing row and the parsed index whose fields differ from it.
    pub update: Vec<(Id, usize)>,
    /// Seen again and unchanged: only `last_seen` and `is_stale` need writing.
    pub touch: Vec<Id>,
    /// Missed this refresh but still inside the retention window.
    pub mark_stale: Vec<Id>,
    pub delete: Vec<(Id, DeleteReason)>,
    /// Parsed entries dropped because an earlier entry had the same hash.
    ///
    /// Surfaced rather than silently swallowed: a provider emitting the same
    /// stream twice is normal, but a *sudden* jump here means the hash key no
    /// longer distinguishes streams that used to be distinct.
    pub duplicate_entries: usize,
    /// Every parsed entry hashed to the same value.
    ///
    /// The symptom of a dedup key that selects no fields, which an imported
    /// instance that never had `m3u_hash_key` set actually carries — the
    /// whole playlist collapses onto one row, and `duplicate_entries` alone
    /// cannot distinguish that from a provider legitimately repeating itself.
    ///
    /// Measured from the effect rather than from the key set, so it catches any
    /// other way the key could degenerate. Two entries that are genuinely the
    /// same stream will set it too; at that size it is not a catastrophe worth
    /// distinguishing, and at playlist size it always means the key is wrong.
    pub hash_key_selects_nothing: bool,
}

pub fn reconcile(
    existing: &[ExistingStream],
    parsed: &[ParsedStream],
    opts: &ReconcileOptions<'_>,
) -> Plan {
    let mut plan = Plan::default();

    let by_hash: BTreeMap<&str, &ExistingStream> = existing
        .iter()
        .map(|stream| (stream.hash.as_str(), stream))
        .collect();

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (index, entry) in parsed.iter().enumerate() {
        // First occurrence wins.
        if !seen.insert(entry.hash.as_str()) {
            plan.duplicate_entries += 1;
            continue;
        }
        match by_hash.get(entry.hash.as_str()) {
            Some(stream) if stream.fields == entry.fields => plan.touch.push(stream.id),
            Some(stream) => plan.update.push((stream.id, index)),
            None => plan.insert.push(index),
        }
    }

    plan.hash_key_selects_nothing = parsed.len() > 1 && seen.len() == 1;

    let expiry = opts.started_at - opts.stale_after;
    for stream in existing {
        if seen.contains(stream.hash.as_str()) {
            // A group can be disabled while its streams are still in the
            // playlist; those go too.
            if !opts.active_groups.contains(&stream.fields.group) {
                plan.delete.push((stream.id, DeleteReason::GroupDisabled));
            }
            continue;
        }
        if !opts.active_groups.contains(&stream.fields.group) {
            plan.delete.push((stream.id, DeleteReason::GroupDisabled));
        } else if stream.last_seen < expiry {
            plan.delete.push((stream.id, DeleteReason::Expired));
        } else {
            plan.mark_stale.push(stream.id);
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn at(day: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-09-{day:02}T00:00:00Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn fields(name: &str, url: &str) -> StreamFields {
        StreamFields {
            name: name.into(),
            url: url.into(),
            group: "News".into(),
            ..StreamFields::default()
        }
    }

    fn existing(id: Id, hash: &str, last_seen: DateTime<Utc>) -> ExistingStream {
        ExistingStream {
            id,
            hash: hash.into(),
            last_seen,
            fields: fields("A", "http://x/a"),
        }
    }

    fn parsed(hash: &str, name: &str) -> ParsedStream {
        ParsedStream {
            hash: hash.into(),
            fields: fields(name, "http://x/a"),
        }
    }

    fn options(active: &BTreeSet<String>) -> ReconcileOptions<'_> {
        ReconcileOptions {
            started_at: at(11),
            stale_after: Duration::days(7),
            active_groups: active,
        }
    }

    #[test]
    fn an_unchanged_stream_is_only_touched() {
        let active = groups(&["News"]);
        let plan = reconcile(
            &[existing(1, "h1", at(10))],
            &[parsed("h1", "A")],
            &options(&active),
        );
        assert_eq!(
            plan,
            Plan {
                touch: vec![1],
                ..Plan::default()
            }
        );
    }

    #[test]
    fn a_changed_stream_is_updated_against_its_parsed_entry() {
        let active = groups(&["News"]);
        let plan = reconcile(
            &[existing(1, "h1", at(10))],
            &[parsed("h1", "A renamed")],
            &options(&active),
        );
        assert_eq!(plan.update, [(1, 0)]);
        assert!(plan.touch.is_empty());
    }

    #[test]
    fn an_unknown_hash_is_an_insert() {
        let active = groups(&["News"]);
        let plan = reconcile(&[], &[parsed("h1", "A")], &options(&active));
        assert_eq!(plan.insert, [0]);
    }

    #[test]
    fn a_stream_missing_this_refresh_is_marked_not_deleted() {
        // The whole point of the module: a provider serving a truncated
        // playlist must not cost the user their lineup.
        let active = groups(&["News"]);
        let plan = reconcile(&[existing(1, "h1", at(10))], &[], &options(&active));
        assert_eq!(plan.mark_stale, [1]);
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn a_stream_missing_longer_than_the_window_is_deleted() {
        let active = groups(&["News"]);
        let cases: &[(DateTime<Utc>, bool)] = &[
            // Exactly at the cutoff is not yet expired: the comparison is `<`.
            (at(4), false),
            (at(3), true),
            (at(10), false),
        ];
        for (last_seen, expired) in cases {
            let plan = reconcile(&[existing(1, "h1", *last_seen)], &[], &options(&active));
            if *expired {
                assert_eq!(plan.delete, [(1, DeleteReason::Expired)], "{last_seen}");
                assert!(plan.mark_stale.is_empty());
            } else {
                assert_eq!(plan.mark_stale, [1], "{last_seen}");
                assert!(plan.delete.is_empty());
            }
        }
    }

    #[test]
    fn a_stream_in_a_disabled_group_is_deleted_whatever_its_age() {
        let active = groups(&["Sports"]);

        // Still in the playlist, but the user turned its group off.
        let plan = reconcile(
            &[existing(1, "h1", at(10))],
            &[parsed("h1", "A")],
            &options(&active),
        );
        assert_eq!(plan.delete, [(1, DeleteReason::GroupDisabled)]);
        assert_eq!(plan.touch, [1], "it was still seen, so it is also touched");

        // Absent and fresh: the group decides, not the age.
        let plan = reconcile(&[existing(1, "h1", at(10))], &[], &options(&active));
        assert_eq!(plan.delete, [(1, DeleteReason::GroupDisabled)]);
        assert!(plan.mark_stale.is_empty());

        // Absent and old: still the group, since it is checked first.
        let plan = reconcile(&[existing(1, "h1", at(1))], &[], &options(&active));
        assert_eq!(plan.delete, [(1, DeleteReason::GroupDisabled)]);
    }

    #[test]
    fn a_duplicated_hash_keeps_the_first_entry_and_is_counted() {
        let active = groups(&["News"]);
        let plan = reconcile(
            &[],
            &[parsed("h1", "First"), parsed("h1", "Second")],
            &options(&active),
        );
        assert_eq!(plan.insert, [0]);
        assert_eq!(plan.duplicate_entries, 1);
    }

    #[test]
    fn a_whole_refresh_at_once() {
        let active = groups(&["News"]);
        let existing_streams = [
            existing(1, "unchanged", at(10)),
            existing(2, "changed", at(10)),
            existing(3, "missing-recently", at(10)),
            existing(4, "missing-for-ages", at(1)),
            ExistingStream {
                fields: StreamFields {
                    group: "Retired".into(),
                    ..fields("A", "http://x/a")
                },
                ..existing(5, "disabled-group", at(10))
            },
        ];
        let parsed_streams = [
            parsed("unchanged", "A"),
            parsed("changed", "A renamed"),
            parsed("brand-new", "New"),
        ];

        let plan = reconcile(&existing_streams, &parsed_streams, &options(&active));
        assert_eq!(
            plan,
            Plan {
                insert: vec![2],
                update: vec![(2, 1)],
                touch: vec![1],
                mark_stale: vec![3],
                delete: vec![(4, DeleteReason::Expired), (5, DeleteReason::GroupDisabled)],
                duplicate_entries: 0,
                hash_key_selects_nothing: false,
            }
        );
    }

    #[test]
    fn a_key_set_that_selects_nothing_is_reported_not_swallowed() {
        // Every entry hashing the same is what an imported instance with an
        // unset `m3u_hash_key` actually does, and `duplicate_entries` alone
        // reads as "the provider repeated itself" rather than "the catalogue
        // just collapsed".
        let active = groups(&["News"]);
        let collapsed = [
            parsed("same", "A"),
            parsed("same", "B"),
            parsed("same", "C"),
        ];
        let plan = reconcile(&[], &collapsed, &options(&active));
        assert!(plan.hash_key_selects_nothing);
        assert_eq!(plan.insert.len(), 1);
        assert_eq!(plan.duplicate_entries, 2);

        // A healthy refresh does not set it, and neither does a single entry,
        // which cannot tell the two cases apart.
        let healthy = [parsed("a", "A"), parsed("b", "B")];
        assert!(!reconcile(&[], &healthy, &options(&active)).hash_key_selects_nothing);
        let single = [parsed("a", "A")];
        assert!(!reconcile(&[], &single, &options(&active)).hash_key_selects_nothing);
        assert!(!reconcile(&[], &[], &options(&active)).hash_key_selects_nothing);
    }

    #[test]
    fn an_empty_refresh_against_an_empty_database_does_nothing() {
        let active = groups(&[]);
        assert_eq!(reconcile(&[], &[], &options(&active)), Plan::default());
    }

    #[test]
    fn fields_are_read_off_a_parsed_entry() {
        let playlist = crate::parse::m3u::parse_str(
            "#EXTINF:-1 tvg-id=\"a.us\" tvg-logo=\"http://l/a.png\" tvg-chno=\"7\" \
             group-title=\"News\" is_adult=\"1\" catchup=\"default\" catchup-days=\"3\" \
             stream_id=\"9001\",Channel A\n\
             http://p/a.ts\n",
        );
        assert_eq!(
            StreamFields::from_entry(&playlist.entries[0], "Default Group"),
            StreamFields {
                name: "Channel A".into(),
                url: "http://p/a.ts".into(),
                logo_url: "http://l/a.png".into(),
                tvg_id: "a.us".into(),
                group: "News".into(),
                is_adult: true,
                provider_stream_id: Some(9001),
                provider_channel_number: Some(7.0),
                is_catchup: true,
                catchup_days: 3,
            }
        );
    }

    #[test]
    fn an_entry_with_no_group_inherits_the_accounts_default() {
        let playlist = crate::parse::m3u::parse_str("#EXTINF:-1,A\nhttp://p/a.ts\n");
        let got = StreamFields::from_entry(&playlist.entries[0], "Default Group");
        assert_eq!(got.group, "Default Group");
        assert_eq!(got.tvg_id, "");
        assert_eq!(got.logo_url, "");
        assert_eq!(got.provider_stream_id, None);
        assert_eq!(got.provider_channel_number, None);
        assert!(!got.is_adult);
        assert!(!got.is_catchup);
        assert_eq!(got.catchup_days, 0);
    }

    #[test]
    fn a_non_numeric_provider_stream_id_is_dropped_rather_than_guessed() {
        let playlist =
            crate::parse::m3u::parse_str("#EXTINF:-1 stream_id=\"abc\",A\nhttp://p/a.ts\n");
        assert_eq!(
            StreamFields::from_entry(&playlist.entries[0], "Default").provider_stream_id,
            None
        );
    }
}
