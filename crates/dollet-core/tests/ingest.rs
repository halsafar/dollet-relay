//! Ingest reconciliation: our `sync::*` decisions against the rows a sync must
//! store.
//!
//! The golden corpus covers what we *serve*. This covers what we *store*, and
//! the distinction matters: an output bug produces a visibly wrong byte, while
//! an ingest bug silently corrupts the catalogue. A stream recreated instead of
//! updated loses every channel assignment hanging off it, and the guide it
//! serves afterwards looks perfectly well-formed.
//!
//! Two sync runs, not one. The first proves import; the second proves
//! reconciliation — that an unchanged entry is touched rather than recreated, a
//! changed one updates in place, and a vanished one is marked rather than
//! deleted. A single-run corpus cannot see the failure that matters most.
//!
//! See `fixtures/ingest/README.md` for what each entry in the corpus is for.

mod common;

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use dollet_core::domain::M3uAccountType;
use dollet_core::parse;
use dollet_core::sync;

fn fixture(name: &str) -> String {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/ingest")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// One stream row a sync must store.
#[derive(Debug, Clone, PartialEq)]
struct ExpectedStream {
    hash: String,
    name: String,
    url: String,
    tvg_id: String,
    group: String,
    channel_number: String,
    is_catchup: bool,
    catchup_days: u32,
    is_stale: bool,
    logo_url: String,
}

fn expected_streams(name: &str) -> Vec<ExpectedStream> {
    fixture(name)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 10, "malformed row: {line}");
            ExpectedStream {
                hash: f[0].into(),
                name: f[1].into(),
                url: f[2].into(),
                tvg_id: f[3].into(),
                group: f[4].into(),
                channel_number: f[5].into(),
                is_catchup: f[6] == "t",
                catchup_days: f[7].parse().unwrap(),
                is_stale: f[8] == "t",
                logo_url: f[9].into(),
            }
        })
        .collect()
}

/// The guide with its timestamp placeholders resolved.
///
/// The committed fixture carries `@T0@`..`@T3@` so the same file can be served
/// at any instant; the expectation rows are stamped with these.
fn guide() -> String {
    let base = chrono::NaiveDate::from_ymd_opt(2026, 9, 13)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let mut text = fixture("provider-epg.xml");
    for hour in 0..4 {
        let stamp = (base + Duration::hours(hour))
            .format("%Y%m%d%H%M%S")
            .to_string();
        text = text.replace(&format!("@T{hour}@"), &stamp);
    }
    text
}

/// The account's hash key.
fn hash_keys() -> Vec<sync::hash::HashKey> {
    sync::hash::parse_keys("url")
}

fn identity<'a>(entry: &'a parse::m3u::M3uEntry, group: &'a str) -> sync::hash::StreamIdentity<'a> {
    sync::hash::StreamIdentity {
        name: &entry.name,
        url: &entry.url,
        tvg_id: entry.tvg_id().unwrap_or_default(),
        group,
        m3u_account_id: 2,
        account_type: M3uAccountType::Standard,
        provider_stream_id: None,
    }
}

/// Our view of one playlist: hash plus the fields a refresh can rewrite.
fn parsed_streams(playlist: &str) -> Vec<sync::streams::ParsedStream> {
    parse::m3u::parse_str(playlist)
        .entries
        .iter()
        .map(|entry| {
            let fields = sync::streams::StreamFields::from_entry(entry, "Default Group");
            sync::streams::ParsedStream {
                hash: sync::hash::stream_hash(&identity(entry, &fields.group), &hash_keys()),
                fields,
            }
        })
        .collect()
}

// --- Hashing -----------------------------------------------------------------

#[test]
fn every_stored_hash_is_reproduced() {
    // The function the whole sync path hangs off: get it wrong and a refresh
    // matches nothing, every stream is "new", and the user's channel
    // assignments are gone. The importer carries these hashes across verbatim,
    // so the shape is load-bearing beyond this corpus: `sha256` over a JSON
    // object with a space after each `:`, not over the bare URL.
    let mut checked = 0;
    for expectation in ["run1-streams.tsv", "run2-streams.tsv"] {
        for row in expected_streams(expectation) {
            let entry = parse::m3u::M3uEntry {
                name: row.name.clone(),
                display_name: row.name.clone(),
                url: row.url.clone(),
                duration: Some(-1.0),
                attributes: Default::default(),
                vlc_opts: Vec::new(),
                kodi_props: Vec::new(),
            };
            // `m3u_hash_key` is `url`, so only the URL is read.
            let ours = sync::hash::stream_hash(&identity(&entry, &row.group), &hash_keys());
            assert_eq!(ours, row.hash, "{expectation}: {}", row.name);
            checked += 1;
        }
    }
    assert_eq!(checked, 26, "both runs, every row");
}

#[test]
fn an_empty_hash_key_collapses_the_whole_playlist() {
    // An empty `m3u_hash_key` selects no fields, so every stream hashes
    // `sha256('{}')` and a refresh would fold the whole playlist into one row.
    // Not hypothetical: an imported instance can carry exactly that setting.
    // The refresh path refuses to write in this state, and that refusal rests
    // on the collapse pinned here.
    let keys = sync::hash::parse_keys("");
    assert!(keys.is_empty());

    let hashes: BTreeSet<String> = expected_streams("run1-streams.tsv")
        .iter()
        .map(|row| {
            let entry = parse::m3u::M3uEntry {
                name: row.name.clone(),
                display_name: row.name.clone(),
                url: row.url.clone(),
                duration: Some(-1.0),
                attributes: Default::default(),
                vlc_opts: Vec::new(),
                kodi_props: Vec::new(),
            };
            sync::hash::stream_hash(&identity(&entry, &row.group), &keys)
        })
        .collect();
    assert_eq!(hashes.len(), 1, "twelve streams, one hash");
}

// --- Run 1: initial import ---------------------------------------------------

#[test]
fn the_first_run_inserts_exactly_the_expected_rows() {
    let expected = expected_streams("run1-streams.tsv");
    assert_eq!(expected.len(), 12, "thirteen entries, one a duplicate");

    let parsed = parsed_streams(&fixture("provider-run1.m3u"));
    assert_eq!(parsed.len(), 13, "the parser sees the duplicate too");

    let groups: BTreeSet<String> = expected.iter().map(|row| row.group.clone()).collect();
    let plan = sync::streams::reconcile(
        &[],
        &parsed,
        &sync::streams::ReconcileOptions {
            started_at: Utc::now(),
            stale_after: Duration::days(7),
            active_groups: &groups,
        },
    );

    assert_eq!(plan.insert.len(), 12, "one entry deduped");
    assert_eq!(plan.duplicate_entries, 1);
    assert!(plan.update.is_empty() && plan.touch.is_empty());
    assert!(plan.mark_stale.is_empty() && plan.delete.is_empty());

    // And the hashes we would insert are exactly the expected rows.
    let ours: BTreeSet<&str> = plan
        .insert
        .iter()
        .map(|index| parsed[*index].hash.as_str())
        .collect();
    let theirs: BTreeSet<&str> = expected.iter().map(|row| row.hash.as_str()).collect();
    assert_eq!(ours, theirs);
}

// --- Run 2: reconciliation ---------------------------------------------------

/// Run 1's rows as they sit in the database when run 2 starts.
///
/// Built from our own parse of run 1, because the touch-versus-update decision
/// compares stored state against a new parse of the same shape.
fn existing_after_run1(seen_at: DateTime<Utc>) -> Vec<sync::streams::ExistingStream> {
    let mut seen = BTreeSet::new();
    parsed_streams(&fixture("provider-run1.m3u"))
        .into_iter()
        .filter(|stream| seen.insert(stream.hash.clone()))
        .enumerate()
        .map(|(index, stream)| sync::streams::ExistingStream {
            id: index as i64 + 1,
            hash: stream.hash,
            last_seen: seen_at,
            fields: stream.fields,
        })
        .collect()
}

#[test]
fn the_second_run_produces_all_four_reconciliation_outcomes() {
    // The whole reason a single-run corpus is not enough. Each of these is a
    // different silent failure if it goes wrong: recreating instead of updating
    // destroys channel assignments, and deleting instead of marking destroys
    // the lineup on one bad fetch.
    let after_run1 = expected_streams("run1-streams.tsv");
    let after_run2 = expected_streams("run2-streams.tsv");
    assert_eq!(
        after_run2.len(),
        14,
        "twelve, plus a rotated URL and a new entry"
    );

    let by_hash: BTreeMap<&str, &ExpectedStream> = after_run2
        .iter()
        .map(|row| (row.hash.as_str(), row))
        .collect();

    let started = Utc::now();
    let existing = existing_after_run1(started - Duration::hours(1));
    let parsed = parsed_streams(&fixture("provider-run2.m3u"));
    let groups: BTreeSet<String> = after_run2.iter().map(|row| row.group.clone()).collect();

    let plan = sync::streams::reconcile(
        &existing,
        &parsed,
        &sync::streams::ReconcileOptions {
            started_at: started,
            stale_after: Duration::days(7),
            active_groups: &groups,
        },
    );

    let name_of = |id: i64| existing[(id - 1) as usize].fields.name.clone();

    // Vanished: marked, never deleted. Golf, and Hotel's old URL.
    let stale: BTreeSet<String> = plan.mark_stale.iter().map(|id| name_of(*id)).collect();
    assert_eq!(
        stale,
        ["Golf Vanishes Next Run", "Hotel URL Changes"]
            .map(String::from)
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    assert!(
        plan.delete.is_empty(),
        "nothing is deleted inside the window"
    );
    let expected_stale: BTreeSet<&str> = after_run2
        .iter()
        .filter(|row| row.is_stale)
        .map(|row| row.name.as_str())
        .collect();
    assert_eq!(expected_stale.len(), 2);

    // A changed URL is a new identity, so a new row — and the expectation
    // holds both.
    let inserted: BTreeSet<String> = plan
        .insert
        .iter()
        .map(|index| parsed[*index].fields.name.clone())
        .collect();
    assert_eq!(
        inserted,
        ["Hotel URL Changes", "November Appears This Run"]
            .map(String::from)
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    for index in &plan.insert {
        assert!(
            by_hash.contains_key(parsed[*index].hash.as_str()),
            "the expectation has a row for every hash we would insert"
        );
    }

    // A changed name under the same URL updates in place. This is the one that
    // matters most: recreate it and the channel pointing at it is orphaned.
    let updated: Vec<String> = plan.update.iter().map(|(id, _)| name_of(*id)).collect();
    assert_eq!(updated, ["India Name Changes"]);
    let renamed = plan.update[0].1;
    assert_eq!(parsed[renamed].fields.name, "India Renamed");
    assert_eq!(
        by_hash[parsed[renamed].hash.as_str()].name,
        "India Renamed",
        "the same row, renamed rather than recreated"
    );

    // Everything else is seen and unchanged.
    assert_eq!(plan.touch.len(), after_run1.len() - 3);
    assert_eq!(plan.duplicate_entries, 1);
}

#[test]
fn a_vanished_stream_is_deleted_only_once_it_is_old_enough() {
    // A vanished stream is marked at once and deleted after `stale_stream_days`.
    // The two-run corpus covers the marking; this covers the other side of the
    // boundary, which two runs an hour apart cannot reach.
    let started = Utc::now();
    let existing = existing_after_run1(started - Duration::days(8));
    let parsed = parsed_streams(&fixture("provider-run2.m3u"));
    let groups: BTreeSet<String> = expected_streams("run2-streams.tsv")
        .iter()
        .map(|row| row.group.clone())
        .collect();

    let plan = sync::streams::reconcile(
        &existing,
        &parsed,
        &sync::streams::ReconcileOptions {
            started_at: started,
            stale_after: Duration::days(7),
            active_groups: &groups,
        },
    );
    assert_eq!(plan.mark_stale, Vec::<i64>::new());
    assert_eq!(plan.delete.len(), 2);
    assert!(
        plan.delete
            .iter()
            .all(|(_, reason)| *reason == sync::streams::DeleteReason::Expired)
    );
}

// --- Attributes ---------------------------------------------------------------

#[test]
fn unquoted_attributes_are_read() {
    // Playlists in the wild carry unquoted attribute values, and a scanner
    // that requires quotes drops them without a word: the stream lands in the
    // default group with no id and no number, and nothing looks wrong. Two
    // entries exercise it, one fully unquoted and one mixed.
    let expected: BTreeMap<String, ExpectedStream> = expected_streams("run1-streams.tsv")
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect();

    let charlie = &expected["Charlie Unquoted"];
    assert_eq!(charlie.group, "Adversarial");
    assert_eq!(charlie.tvg_id, "charlie.test");
    assert_eq!(charlie.channel_number, "3");

    let delta = &expected["Delta, With Comma"];
    assert_eq!(delta.group, "Adversarial");
    assert_eq!(delta.tvg_id, "delta.test");
    assert_eq!(delta.channel_number, "4");

    // And the parser is where those rows come from.
    let entries = parse::m3u::parse_str(&fixture("provider-run1.m3u")).entries;
    let ours: BTreeMap<&str, &parse::m3u::M3uEntry> = entries
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();

    let charlie = ours["Charlie Unquoted"];
    assert_eq!(charlie.group_title(), Some("Adversarial"));
    assert_eq!(charlie.tvg_id(), Some("charlie.test"));
    assert_eq!(charlie.tvg_chno(), Some(3.0));

    let delta = ours["Delta, With Comma"];
    assert_eq!(delta.group_title(), Some("Adversarial"));
    assert_eq!(delta.tvg_chno(), Some(4.0));
    assert_eq!(
        delta.name, "Delta, With Comma",
        "the comma belongs to the title, not to an attribute"
    );
}

#[test]
fn catchup_attributes_are_read_in_both_spellings() {
    // Providers spell catch-up two ways, `catchup`/`catchup-days` and
    // `tv_archive`/`tv_archive_duration`, and this entry uses the first. Both
    // are read. Catch-up is out of scope for 1.0, so nothing acts on the flag
    // yet and no output advertises it — but the row is right.
    let expected: BTreeMap<String, ExpectedStream> = expected_streams("run1-streams.tsv")
        .into_iter()
        .map(|row| (row.name.clone(), row))
        .collect();
    let juliet = &expected["Juliet Catchup"];
    assert!(juliet.is_catchup);
    assert_eq!(juliet.catchup_days, 7);

    let entries = parse::m3u::parse_str(&fixture("provider-run1.m3u")).entries;
    let juliet = entries
        .iter()
        .find(|entry| entry.name == "Juliet Catchup")
        .expect("the catch-up entry");
    assert!(juliet.is_catchup());
    assert_eq!(juliet.catchup_days(), 7);
    assert_eq!(
        juliet.catchup_source(),
        Some("http://provider.example/timeshift/{utc}")
    );
}

// --- Guide ingest ------------------------------------------------------------

fn expected_programmes() -> Vec<Vec<String>> {
    fixture("programmes.tsv")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split('\t').map(str::to_string).collect())
        .collect()
}

#[test]
fn the_guides_channels_parse_to_the_expected_rows() {
    let guide = guide();
    let channels: BTreeMap<String, parse::xmltv::ParsedChannel> =
        parse::xmltv::from_bytes(guide.as_bytes())
            .unwrap()
            .filter_map(|item| match item.unwrap() {
                parse::xmltv::XmltvItem::Channel(channel) => {
                    Some((channel.tvg_id.clone(), channel))
                }
                parse::xmltv::XmltvItem::Programme(_) => None,
            })
            .collect();

    let mut checked = 0;
    for row in fixture("epg-data.tsv").lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = row.split('\t').collect();
        let ours = &channels[f[0]];
        assert_eq!(
            ours.display_name.as_deref().unwrap_or_default(),
            f[1],
            "{}",
            f[0]
        );
        assert_eq!(
            ours.icon_url.as_deref().unwrap_or_default(),
            f[2],
            "{}",
            f[0]
        );
        checked += 1;
    }
    assert_eq!(checked, 4, "including the one no stream maps to");
}

#[test]
fn html_entities_and_non_ascii_survive_ingest() {
    // `&eacute;` in the guide, `é` in the stored row. An entity table that
    // resolved nothing would blank the title, and a guide full of empty titles
    // is still a well-formed guide.
    let programmes = expected_programmes();
    let titles: Vec<&str> = programmes.iter().map(|row| row[3].as_str()).collect();
    assert!(titles.contains(&"Alpha Caf\u{e9} Hour"), "{titles:?}");
    assert!(titles.contains(&"Lima \u{dc}berraschung"), "{titles:?}");
    assert!(titles.contains(&"Echo <Tagged> & Quoted"), "{titles:?}");

    let ours: BTreeMap<String, parse::xmltv::ParsedProgramme> =
        parse::xmltv::from_bytes(guide().as_bytes())
            .unwrap()
            .filter_map(|item| match item.unwrap() {
                parse::xmltv::XmltvItem::Programme(p) => Some((p.title.clone(), p)),
                parse::xmltv::XmltvItem::Channel(_) => None,
            })
            .collect();

    for row in &programmes {
        let title = &row[3];
        let parsed = ours
            .get(title)
            .unwrap_or_else(|| panic!("we did not parse {title:?}"));
        assert_eq!(
            &parsed.description.clone().unwrap_or_default(),
            &row[5],
            "{title}"
        );
        assert_eq!(
            parsed.start_time.format("%Y%m%d%H%M%S").to_string(),
            row[1],
            "{title} start"
        );
        assert_eq!(
            parsed.end_time.format("%Y%m%d%H%M%S").to_string(),
            row[2],
            "{title} stop"
        );
    }

    // The raw `&` inside an otherwise-valid description survives too: an
    // unresolvable entity must not blank the sentence around it.
    let cafe = &ours["Alpha Caf\u{e9} Hour"];
    assert!(cafe.description.as_ref().unwrap().contains("Acme & Co."));
    assert_eq!(
        cafe.custom_properties["categories"],
        serde_json::json!(["M\u{fa}sica"])
    );
}

#[test]
fn a_programme_with_no_stop_time_is_skipped_and_counted() {
    // A programme with no end cannot be placed on a grid, so it is skipped —
    // five rows stored where the guide has six for mapped channels — and
    // counted, because a guide quietly losing programmes has to be visible
    // somewhere.
    let titles: Vec<String> = expected_programmes()
        .into_iter()
        .map(|row| row[3].clone())
        .collect();
    assert!(
        !titles.contains(&"Alpha No Stop Time".to_string()),
        "it is not stored"
    );
    assert_eq!(titles.len(), 5);

    let mut reader = parse::xmltv::from_bytes(guide().as_bytes()).unwrap();
    let mut ours = Vec::new();
    while let Some(item) = reader.next_item().unwrap() {
        if let parse::xmltv::XmltvItem::Programme(programme) = item {
            ours.push(programme.title);
        }
    }
    assert!(!ours.contains(&"Alpha No Stop Time".to_string()));
    assert_eq!(
        reader.skipped(),
        1,
        "skipped, and counted rather than silent"
    );

    // A timestamp with no offset is read as UTC by both, which is the
    // documented default rather than a degradation.
    assert!(ours.contains(&"Alpha No Timezone".to_string()));
    assert_eq!(reader.unrecognised_zones(), 0);
}

#[test]
fn a_programme_for_an_undeclared_channel_is_yielded_and_stored_by_nobody() {
    // The parser is a pull parser over the document and yields every
    // programme, including one whose `channel` no `<channel>` element
    // declares; resolving it to a row is the caller's decision, and there is
    // nothing to resolve it to, so no row is stored.
    let titles: Vec<String> = expected_programmes()
        .into_iter()
        .map(|row| row[3].clone())
        .collect();
    assert!(!titles.contains(&"Programme For A Channel Not Declared".to_string()));

    let ours: Vec<String> = parse::xmltv::from_bytes(guide().as_bytes())
        .unwrap()
        .filter_map(|item| match item.unwrap() {
            parse::xmltv::XmltvItem::Programme(p) => Some(p.tvg_id),
            parse::xmltv::XmltvItem::Channel(_) => None,
        })
        .collect();
    assert!(ours.contains(&"nosuchchannel.test".to_string()));
}

#[test]
fn programme_metadata_lands_in_the_expected_custom_properties_shape() {
    // The JSON blob every programme row carries. Its key names and one-based
    // season/episode are what the XMLTV serializer reads back and what the
    // importer carries across, so a mismatch here is a guide that
    // re-serializes wrong.
    //
    // The programme carries two `<title>`s, `lang="fr"` first: the first one
    // is stored, whatever its language, and `lang` is not read.
    let stored: serde_json::Value = expected_programmes()
        .iter()
        .find(|row| row[3] == "Alpha Matin")
        .map(|row| serde_json::from_str(&row[6]).expect("custom_properties is JSON"))
        .expect("the fully-populated programme");

    let ours = parse::xmltv::from_bytes(guide().as_bytes())
        .unwrap()
        .filter_map(|item| match item.unwrap() {
            parse::xmltv::XmltvItem::Programme(p) if p.title == "Alpha Matin" => Some(p),
            _ => None,
        })
        .next()
        .expect("Alpha Matin");

    assert_eq!(ours.custom_properties, stored);
    assert_eq!(ours.sub_title.as_deref(), Some("Episode One"));
}

// --- EPG matching ------------------------------------------------------------

#[test]
fn fuzzy_scores_are_pinned_for_every_unmatched_channel() {
    // The scorer decides where a channel lands on the threshold ladder, so its
    // numbers are pinned to nine decimals for every channel the corpus fails to
    // match. They are rapidfuzz's `ratio` over the normalised names, which is
    // what `sync::epg` reproduces; a scorer that drifted would move channels
    // across the ladder's bands without any test of the ladder noticing.
    let candidates: Vec<(String, String)> = fixture("epg-data.tsv")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            (f[0].to_string(), f[1].to_string())
        })
        .collect();
    let settings = sync::epg::NormalizeSettings::default();

    let mut checked = 0;
    for line in fixture("fuzzy-scores.tsv")
        .lines()
        .filter(|l| !l.is_empty())
    {
        let (name, score) = line.split_once('\t').expect("name and score");
        let expected: f64 = score.parse().expect("a number");

        let best = candidates
            .iter()
            .map(|(tvg_id, display)| {
                let channel = sync::epg::normalize_name(name, &settings);
                let candidate = sync::epg::normalize_name(display, &settings);
                (rapidfuzz_ratio(&channel, &candidate), tvg_id.clone())
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .expect("candidates");

        assert!(
            (best.0 - expected).abs() < 1e-9,
            "{name}: ours {} vs pinned {expected}",
            best.0
        );
        checked += 1;
    }
    assert_eq!(checked, 9, "every channel the corpus cannot match");
}

/// The scorer `sync::epg` uses, reached through its public surface.
///
/// `best_match` applies the ladder as well, so scoring one pair directly is the
/// only way to compare a raw number against a pinned score.
fn rapidfuzz_ratio(channel: &str, candidate: &str) -> f64 {
    let settings = sync::epg::NormalizeSettings::default();
    let epg = [sync::epg::EpgCandidate {
        epg_data_id: 1,
        tvg_id: "",
        name: candidate,
        source_priority: 0,
    }];
    // A ladder that matches everything, so the score comes back untouched.
    let thresholds = sync::epg::Thresholds {
        high: 0.0,
        confident: 0.0,
        medium: 0.0,
    };
    match sync::epg::best_match(channel, &epg, &settings, thresholds, None) {
        sync::epg::Outcome::Matched { score, .. } => score,
        sync::epg::Outcome::Ambiguous { score, .. } => score,
        sync::epg::Outcome::NoMatch => 0.0,
    }
}

#[test]
fn the_matched_channels_are_the_ones_with_an_exact_tvg_id() {
    // None of the nine unmatched channels scored above even the loosest
    // threshold, so the three that matched did so by tvg_id at creation time,
    // not by fuzzy score. Pinned because it says what the ladder is *for*:
    // everything here is below it, and the band reported as ambiguous for a
    // human decision is empty on this data.
    let mut matched = Vec::new();
    for line in fixture("channels.tsv").lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        if !f[2].is_empty() {
            matched.push((f[0].to_string(), f[1].to_string(), f[2].to_string()));
        }
    }
    assert_eq!(matched.len(), 3);
    for (name, tvg_id, epg) in &matched {
        assert_eq!(tvg_id, epg, "{name} matched on an exact tvg_id");
    }

    let highest = fixture("fuzzy-scores.tsv")
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| line.split('\t').nth(1))
        .map(|score| score.parse::<f64>().unwrap())
        .fold(0.0f64, f64::max);
    assert!(
        highest < sync::epg::Thresholds::single().medium.max(50.0),
        "the best unmatched score was {highest}"
    );
}

// --- Filters -----------------------------------------------------------------

/// The rule set, read from the corpus.
///
/// Order is the whole semantics, so the file's first column is asserted to be
/// the file's order rather than assumed: a reordering that slipped in silently
/// would make this test prove the wrong thing.
fn filter_rules() -> Vec<sync::filters::StreamFilter> {
    fixture("filters.tsv")
        .lines()
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, line)| {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 5, "malformed rule: {line}");
            assert_eq!(f[0].parse::<usize>().unwrap(), index, "rules are in order");
            assert_eq!(f[1], "name", "every rule here matches on the name");
            sync::filters::StreamFilter {
                target: sync::filters::FilterTarget::Name,
                pattern: f[2].into(),
                exclude: match f[3] {
                    "exclude" => true,
                    "include" => false,
                    other => panic!("unknown disposition {other:?}"),
                },
                // A rule is case-sensitive unless it opts out, and none of
                // these do.
                case_sensitive: true,
            }
        })
        .collect()
}

/// The names the rules must let through.
fn expected_kept() -> BTreeSet<String> {
    fixture("filtered-streams.tsv")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| line.split('\t').next().unwrap().to_string())
        .collect()
}

/// Every entry in the filtered playlist, in playlist order.
fn filtered_playlist() -> Vec<parse::m3u::M3uEntry> {
    parse::m3u::parse_str(&fixture("provider-filtered.m3u")).entries
}

#[test]
fn the_first_matching_rule_decides_and_the_rest_are_dead() {
    // The ordering is not a detail: rule 0 admits `^Foxtrot` and rule 1 excludes
    // the broader `^Fox`. Evaluate them in the wrong order — or keep looking
    // after a match — and Foxtrot disappears from the user's lineup while
    // Foxglove stays, which is exactly backwards.
    let (compiled, errors) = sync::filters::compile(&filter_rules());
    assert_eq!(errors, Vec::new(), "every rule compiles");

    let entries = filtered_playlist();
    assert_eq!(entries.len(), 14);
    let ours: BTreeSet<String> = entries
        .iter()
        .filter(|entry| {
            compiled.admits(
                &entry.name,
                &entry.url,
                entry.group_title().unwrap_or_default(),
            )
        })
        .map(|entry| entry.name.clone())
        .collect();

    let expected = expected_kept();
    assert_eq!(expected.len(), 9, "nine of fourteen survive");
    assert_eq!(ours, expected);

    // And the five each rule was written to reject, named so a rule that stops
    // working says which one.
    for rejected in [
        "Foxglove Excluded",     // ^Fox, once Foxtrot has been pinned by rule 0
        "Kilo Excluded",         // a plain prefix
        "Mike Second Group",     // lookbehind
        "Delta, With Comma",     // a backreference: `(m)\1` finds the `mm` in Comma
        "Papa Echo Echo Repeat", // a JS-style backreference, see below
    ] {
        assert!(
            !ours.contains(rejected),
            "{rejected} should be filtered out"
        );
    }

    // Foxtrot is the load-bearing one: kept only because the include is first.
    assert!(ours.contains("Foxtrot Number Collision"));
}

#[test]
fn a_js_backreference_in_a_search_pattern_is_live() {
    // Rule 5 is `(\w+) $1`, written in the JavaScript dialect, where `$1` is a
    // backreference. In a search pattern `$` is an anchor, so compiled as
    // written the rule can never match anything and "Papa Echo Echo Repeat"
    // would survive a rule written to exclude it. `regex_compat` rewrites `$1`
    // to `\1` before compiling — search patterns only — so the rule does what
    // its author meant.
    let as_written = fancy_regex::Regex::new(r"(\w+) $1").unwrap();
    assert!(
        !as_written.is_match("Papa Echo Echo Repeat").unwrap(),
        "compiled as written, the rule is inert"
    );

    let (compiled, errors) = sync::filters::compile(&filter_rules());
    assert!(errors.is_empty());
    assert!(
        !compiled.admits("Papa Echo Echo Repeat", "", "Filtered"),
        "compiled through the rewrite, it fires"
    );
    assert!(!expected_kept().contains("Papa Echo Echo Repeat"));

    // And it costs exactly that one stream: the rewrite rejects nothing the
    // other rules do not. A rewrite that over-reached would show up here as a
    // whole group of channels quietly vanishing.
    let (without_rule_5, _) = sync::filters::compile(&filter_rules()[..5]);
    let only_rule_5: Vec<String> = filtered_playlist()
        .iter()
        .filter(|entry| without_rule_5.admits(&entry.name, &entry.url, "Filtered"))
        .filter(|entry| !compiled.admits(&entry.name, &entry.url, "Filtered"))
        .map(|entry| entry.name.clone())
        .collect();
    assert_eq!(only_rule_5, ["Papa Echo Echo Repeat"]);
}

#[test]
fn a_rule_that_will_not_compile_costs_one_rule_and_not_the_refresh() {
    // A broken pattern is a user typo, and the blast radius has to stay small:
    // the rules that did compile still apply, the refresh finishes with the
    // same nine streams, and the broken one is reported by index so the UI can
    // say something. Silently dropping every rule would change which streams
    // exist, which is indistinguishable from the provider changing its lineup.
    let mut filters = filter_rules();
    let broken = filters.len();
    filters.push(sync::filters::StreamFilter {
        target: sync::filters::FilterTarget::Name,
        pattern: "(unclosed".into(),
        exclude: true,
        case_sensitive: true,
    });
    let (compiled, errors) = sync::filters::compile(&filters);
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].index, broken);
    assert_eq!(errors[0].pattern, "(unclosed");
    assert!(!errors[0].message.is_empty(), "and it says why");

    // Same admissions as the intact set: the survivors did not change.
    let survivors = |compiled: &sync::filters::CompiledFilters| -> BTreeSet<String> {
        filtered_playlist()
            .iter()
            .filter(|entry| compiled.admits(&entry.name, &entry.url, "Filtered"))
            .map(|entry| entry.name.clone())
            .collect()
    };
    let (intact, _) = sync::filters::compile(&filter_rules());
    assert_eq!(survivors(&compiled), survivors(&intact));
    assert_eq!(survivors(&compiled).len(), 9, "nothing was dropped");
}

// --- Auto channel sync -------------------------------------------------------

/// One expected `auto_created` channel row: name, number, row id.
fn autosync_channels(name: &str) -> Vec<(String, String, i64)> {
    fixture(name)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            assert_eq!(f.len(), 3, "malformed row: {line}");
            (f[0].into(), f[1].into(), f[2].parse().unwrap())
        })
        .collect()
}

#[test]
fn auto_sync_numbers_and_renames_the_group_as_expected() {
    // `provider` mode with fallback 100 and end 103, over the nine streams the
    // filters let through. One group exercises every branch at once: a provider
    // number already taken by an existing channel, a free one, one far outside
    // the configured range, several with no number at all, and finally
    // exhaustion.
    let expected = autosync_channels("autosync-run1-channels.tsv");
    assert_eq!(expected.len(), 6, "nine streams, six numbers to give out");

    // Numbers the existing lineup already holds. 1..5 and 8..14 are taken, so
    // 6 and 7 are the only low numbers free — which is what makes the Foxtrot
    // and Alpha collisions land in the fallback range and Oscar's 6 land as-is.
    let mut used: Vec<f64> = fixture("channels.tsv")
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| line.split('\t').nth(3)?.parse().ok())
        .collect();
    used.sort_by(f64::total_cmp);
    assert_eq!(used.len(), 12);

    let numbering = sync::channels::Numbering {
        mode: sync::channels::NumberingMode::Provider,
        // `channel_numbering_fallback`; the corpus sets it equal to
        // `auto_sync_channel_start`, so the two are not distinguished here.
        start: 100.0,
        end: Some(103.0),
        step: 1.0,
    };
    let rule = sync::channels::Rename {
        pattern: r"^(\w+) (.*)$",
        replacement: Some("$2 [$1]"),
        max_length: 255,
    };

    // Playlist order, which is also provider stream-id order. The corpus
    // validates the assumption: allocate in any other order and the numbers
    // below land on different channels.
    let kept = expected_kept();
    let mut ours: Vec<(String, Option<f64>)> = Vec::new();
    // The group being synced is new, so what it holds is what this run gave.
    let mut own: Vec<f64> = Vec::new();
    for entry in filtered_playlist() {
        if !kept.contains(&entry.name) {
            continue;
        }
        let number = sync::channels::pick_number(&numbering, entry.tvg_chno(), &own, &used);
        if let Some(number) = number {
            sync::channels::claim(&mut used, number);
            sync::channels::claim(&mut own, number);
        }
        ours.push((sync::channels::rename(&entry.name, &rule), number));
    }
    assert_eq!(ours.len(), 9);

    // The expected channels, keyed by the renamed name.
    let theirs: BTreeMap<&str, f64> = expected
        .iter()
        .map(|(name, number, _)| (name.as_str(), number.parse().unwrap()))
        .collect();

    let mut unnumbered = Vec::new();
    for (name, number) in &ours {
        match number {
            Some(number) => assert_eq!(theirs.get(name.as_str()), Some(number), "{name}"),
            None => unnumbered.push(name.clone()),
        }
    }

    // A stream that got no number got no channel, rather than a channel with a
    // colliding or wrapped-around number.
    assert_eq!(unnumbered, ["Kept [Echo]", "Kept [Golf]", "Kept [Hotel]"]);
    for name in &unnumbered {
        assert!(!theirs.contains_key(name.as_str()), "{name}");
    }
    assert_eq!(ours.len() - unnumbered.len(), theirs.len());

    // Spelled out, because each is a different branch and a regression in one
    // would still leave the set comparison above passing on the others.
    assert_eq!(theirs["Number Collision [Foxtrot]"], 100.0); // 1 taken -> fallback
    assert_eq!(theirs["Kept [Oscar]"], 6.0); // 6 free -> provider's own
    assert_eq!(theirs["Kept [Alpha]"], 101.0); // 1 taken, 100 now taken
    assert_eq!(theirs["Kept [Bravo]"], 200.0); // outside 100..103, still honoured
    assert_eq!(theirs["Kept [Charlie]"], 102.0); // no provider number
    assert_eq!(theirs["Kept [Delta]"], 103.0); // the last slot in the range
}

#[test]
fn a_second_sync_leaves_auto_created_channels_alone() {
    // The failure this corpus exists to rule out. Re-creating auto-created
    // channels on every refresh would quietly double a user's lineup, and it
    // cannot be seen on a single run: after one sync a correct implementation
    // and a broken one look identical.
    //
    // The row ids are the evidence. Same names and numbers with new ids would
    // mean delete-and-recreate, which loses every stream assignment and every
    // manual edit hanging off the channel even when the count happens to match.
    // Unlike the tests above, this exercises no code here: choosing a number
    // and a name is pure, but deciding that a stream already *has* an
    // auto-created channel is a query, made in the refresh handler and tested
    // there. The two files state the requirement on it.
    let run1 = autosync_channels("autosync-run1-channels.tsv");
    let run2 = autosync_channels("autosync-run2-channels.tsv");

    assert_eq!(run2.len(), run1.len(), "the second sync added nothing");
    assert_eq!(run2, run1, "same names, same numbers, same row ids");

    let ids: BTreeSet<i64> = run1.iter().map(|(_, _, id)| *id).collect();
    assert_eq!(ids.len(), run1.len(), "and they are distinct rows");
}

// --- Corpus integrity --------------------------------------------------------

#[test]
fn the_ingest_corpus_references_no_unexpected_host() {
    // The corpus is synthetic, so this passes trivially; it exists so the guard
    // is in place before anyone points the corpus at a real provider.
    const ALLOWED: &[&str] = &["provider.example", "ipx.test"];
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/ingest");
    let mut stray = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for host in common::hosts_referenced(&text) {
            if !ALLOWED.contains(&host.as_str()) {
                stray.push(format!("{name}: {host}"));
            }
        }
    }
    stray.sort();
    stray.dedup();
    assert_eq!(stray, Vec::<String>::new());
}
