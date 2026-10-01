//! Provider playlist refresh.
//!
//! The hash is the dangerous part. It decides whether a refreshed entry is the
//! same stream as one already stored, so changing what it keys on orphans every
//! existing row, rebuilds the catalogue, and leaves every channel pointing at
//! streams that no longer exist. `sync::hash` is verified against the
//! committed fixtures; nothing here second-guesses it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{Duration, Utc};
use dollet_core::db::jobs::Job;
use dollet_core::db::m3u::GroupAccountLink;
use dollet_core::domain::{
    Channel, ChannelGroup, Id, M3uAccount, M3uAccountType, Stream, UserLevel,
};
use dollet_core::parse::m3u as parse_m3u;
use dollet_core::settings::{self, EpgSettings, NumberingSettings, StreamSettings};
use dollet_core::sync::filters::{FilterTarget, StreamFilter};
use dollet_core::sync::streams::{ExistingStream, ParsedStream, ReconcileOptions, StreamFields};
use dollet_core::sync::{channels as sync_channels, filters, hash, streams as sync_streams};
use dollet_core::{Error, db};
use serde_json::Value;

use crate::AppState;
use crate::api::jobs::{JobHandle, payload_id};
use crate::api::outputs::{self, Output};

pub const KIND: &str = "m3u_refresh";

pub fn job_key(account_id: Id) -> String {
    format!("{KIND}:{account_id}")
}

/// Rows per commit, for the same reason as everywhere else.
const BATCH: usize = 200;

/// Group a provider entry lands in when it declares none. Matches the seeded
/// `channel_group` row.
const DEFAULT_GROUP: &str = "Default Group";

/// The longest name a rename may produce: a rule that expanded without bound
/// would make names no screen can show, so the rule caps instead.
const CHANNEL_NAME_MAX: usize = 255;

pub async fn run(state: AppState, job: Job, handle: JobHandle) -> Result<String, Error> {
    let account_id = payload_id(&job, "m3u_account_id")?;
    let account = db::m3u::get_account(&state.db, account_id)
        .await?
        .ok_or(Error::NotFound)?;

    refresh(&state, &account, &handle).await
}

pub async fn refresh(
    state: &AppState,
    account: &M3uAccount,
    handle: &JobHandle,
) -> Result<String, Error> {
    if account.locked {
        return Ok("the built-in account holds hand-added streams and is never refreshed".into());
    }

    let started_at = Utc::now();
    handle.progress(0.05, "downloading").await;

    let entries = read_feed(state, account, handle).await?;
    if entries.is_empty() {
        return Err(Error::upstream("the provider declared no streams"));
    }

    // Compiled before anything is written: a pattern that will not compile
    // changes which streams survive, and the user has to see that rather than
    // discover a shorter lineup.
    let stored_filters = db::m3u::list_filters(&state.db, Some(account.id)).await?;
    let (compiled, broken) = filters::compile(
        &stored_filters
            .iter()
            .map(|filter| StreamFilter {
                target: match filter.filter_type.as_str() {
                    "group" => FilterTarget::Group,
                    "url" => FilterTarget::Url,
                    _ => FilterTarget::Name,
                },
                pattern: filter.regex_pattern.clone(),
                exclude: filter.exclude,
                case_sensitive: false,
            })
            .collect::<Vec<_>>(),
    );
    for error in &broken {
        tracing::error!(
            account = account.id,
            pattern = %error.pattern,
            message = %error.message,
            "filter will not compile and was skipped"
        );
    }

    // Logged is not noticed. This refresh reports success — correctly, the
    // streams it did write are right — and the operator meets the consequence
    // weeks later as channels in Plex the filters were supposed to remove.
    // Raised every refresh so the count is the evidence of how long it has
    // been true, and cleared by the first refresh that compiles everything.
    const FILTER_BROKEN: &str = "m3u.filter_broken";
    let subject = format!("account:{}", account.id);
    if broken.is_empty() {
        db::notifications::clear(&state.db, FILTER_BROKEN, &subject).await?;
    } else {
        let patterns = broken
            .iter()
            .map(|error| format!("`{}`", error.pattern))
            .collect::<Vec<_>>()
            .join(", ");
        db::notifications::raise(
            &state.db,
            &db::notifications::Notification::new(
                FILTER_BROKEN,
                &subject,
                db::notifications::Severity::Warning,
                format!("Filters skipped on {}", account.name),
                format!(
                    "{} of this account's filter patterns will not compile, so every refresh \
                     keeps more streams than the filters intended: {patterns}. Fix or remove \
                     them under the account's filters.",
                    broken.len(),
                ),
                serde_json::json!({
                    "m3u_account_id": account.id,
                    "patterns": broken
                        .iter()
                        .map(|error| serde_json::json!({
                            "pattern": error.pattern,
                            "message": error.message,
                        }))
                        .collect::<Vec<_>>(),
                }),
            ),
        )
        .await?;
    }

    let settings: StreamSettings = settings::load(&state.db).await?;
    let keys = hash::parse_keys(&settings.m3u_hash_key);

    handle.progress(0.35, "reconciling").await;

    let mut parsed: Vec<ParsedStream> = Vec::with_capacity(entries.len());
    let mut filtered = 0usize;
    for fields in entries {
        if !compiled.admits(&fields.name, &fields.url, &fields.group) {
            filtered += 1;
            continue;
        }

        let identity = hash::StreamIdentity {
            name: &fields.name,
            url: &fields.url,
            tvg_id: &fields.tvg_id,
            group: &fields.group,
            m3u_account_id: account.id,
            account_type: account.account_type,
            provider_stream_id: fields.provider_stream_id,
        };
        parsed.push(ParsedStream {
            hash: hash::stream_hash(&identity, &keys),
            fields,
        });
    }

    // Every group the feed still carries, plus the ones the operator disabled
    // for this account — reconcile deletes streams whose group left either set.
    let groups = ensure_groups(state, account, &parsed).await?;
    let active: BTreeSet<String> = groups
        .iter()
        .filter(|(_, (_, enabled))| *enabled)
        .map(|(name, _)| name.clone())
        .collect();

    let existing = load_existing(state, account.id).await?;
    let plan = sync_streams::reconcile(
        &existing,
        &parsed,
        &ReconcileOptions {
            started_at,
            stale_after: Duration::days(i64::from(account.stale_stream_days).max(1)),
            active_groups: &active,
        },
    );

    // A dedup key that selects no fields hashes every entry in the playlist to
    // the same value. `reconcile` then sees one stream where there are
    // hundreds: the rest of the catalogue matched nothing, so it all goes
    // stale, and `stale_stream_days` later the same refresh deletes it — taking
    // every `channel_stream` row with it, and with those every failover list
    // the user built. The refresh reports success the whole way.
    //
    // An imported instance can carry `m3u_hash_key` empty, so this is a
    // configuration that exists rather than a corner. Nothing is written; the
    // catalogue already on disk is the one worth keeping.
    if plan.hash_key_selects_nothing {
        return Err(Error::invalid(format!(
            "refusing to write: every one of the {} playlist entries hashed to the same value, \
             so this refresh would mark the whole account stale and later delete it. \
             The `m3u_hash_key` setting is `{}`, which selects no fields — set it to `url` \
             (or a comma-separated list of group, m3u_id, name, tvg_id, url) and refresh again.",
            parsed.len(),
            settings.m3u_hash_key,
        )));
    }

    if handle.cancelled() {
        return Err(super::cancelled());
    }

    handle.progress(0.55, "writing streams").await;
    let group_ids: HashMap<&str, Id> = groups
        .iter()
        .map(|(name, (id, _))| (name.as_str(), *id))
        .collect();
    let written = apply(
        state, account, &plan, &parsed, &group_ids, started_at, handle,
    )
    .await?;

    handle.progress(0.85, "syncing channels").await;
    let created = auto_create_channels(state, account, handle).await?;

    // Only the channels this refresh made, and only when the operator opted
    // into matching on refresh. The guide job's own pass runs on its schedule,
    // which can be a day away; a channel created here would show up in Plex
    // with an empty strip until then.
    let matched = if created.is_empty() {
        0
    } else {
        let epg: EpgSettings = settings::load(&state.db).await?;
        if epg.epg_auto_match_on_refresh {
            handle.progress(0.92, "matching new channels").await;
            super::epg::match_new_channels(state, &created)
                .await?
                .matched
        } else {
            0
        }
    };

    let summary = format!(
        "{} streams: {} new, {} updated, {} unchanged, {} stale, {} removed\
         {}{}{}{}{}",
        parsed.len(),
        written.inserted,
        plan.update.len(),
        plan.touch.len(),
        plan.mark_stale.len(),
        plan.delete.len(),
        match filtered {
            0 => String::new(),
            n => format!(", {n} filtered out"),
        },
        match plan.duplicate_entries {
            0 => String::new(),
            n => format!(", {n} duplicate entries"),
        },
        match created.len() {
            0 => String::new(),
            n => format!(", {n} channels created"),
        },
        match matched {
            0 => String::new(),
            n => format!(", {n} matched to a guide"),
        },
        // Carried in the summary rather than raised as an error: the refresh
        // itself succeeded, and reporting it as failed both hides the counts
        // above and leaves the account showing a red state that no retry can
        // clear. The skipped pattern still changed which streams survived, so
        // it is named here, logged above, and counted in the event.
        match broken.len() {
            0 => String::new(),
            n => format!(
                "; {n} filter pattern(s) would not compile and were skipped, \
                 so more streams were kept than the filters intended"
            ),
        },
    );

    db::events::record(
        &state.db,
        "m3u_refresh",
        None,
        None,
        &serde_json::json!({
            "account_name": account.name,
            "total_processed": parsed.len(),
            "streams_created": written.inserted,
            "streams_updated": plan.update.len(),
            "streams_stale": plan.mark_stale.len(),
            "streams_deleted": plan.delete.len(),
            "filtered_out": filtered,
            "duplicate_entries": plan.duplicate_entries,
            "broken_filters": broken.len(),
            "elapsed_time": (Utc::now() - started_at).num_milliseconds() as f64 / 1000.0,
        }),
    )
    .await?;

    // The ingest just wrote a large fraction of the database through the WAL.
    // A PASSIVE checkpoint leaves the file at its high-water mark.
    db::checkpoint_wal(&state.db).await;

    // Both outputs: a channel this refresh created appears in each, and a
    // stream that came, went or changed URL moves the playlist's `?direct=true`
    // links. `touch` and `mark_stale` are left out on purpose — they write
    // `last_seen` and `is_stale`, which no output reads.
    let catalogue_moved =
        !plan.insert.is_empty() || !plan.update.is_empty() || !plan.delete.is_empty();
    if catalogue_moved || !created.is_empty() {
        outputs::invalidate(state, &Output::BOTH).await;
    }

    Ok(summary)
}

/// The account's streams, from whichever of the three sources it has.
///
/// An Xtream account goes through `player_api.php`, not `get.php`. The two
/// return the same catalogue, but only the JSON carries the provider's
/// `stream_id`, and without it `sync::hash` cannot substitute that id for the
/// URL — which is the one thing standing between a credential rotation and a
/// full catalogue rebuild. See `super::xtream`.
///
/// A file on disk wins over both: it is how an operator feeds a playlist this
/// server could not fetch, and answering it with a provider request would
/// ignore the file they pointed at.
async fn read_feed(
    state: &AppState,
    account: &M3uAccount,
    handle: &JobHandle,
) -> Result<Vec<StreamFields>, Error> {
    let agent = crate::api::user_agent_for(state, account.user_agent_id).await?;

    if let Some(file) = &account.file_path
        && !file.is_empty()
        && tokio::fs::metadata(file).await.is_ok()
    {
        return parse_playlist(&std::path::PathBuf::from(file)).await;
    }

    if account.account_type == M3uAccountType::XtreamCodes {
        return super::xtream::catalogue(state, account, &agent, DEFAULT_GROUP, handle).await;
    }

    let url = account
        .server_url
        .as_deref()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| Error::invalid("account has neither a server URL nor a file"))?;
    let destination = super::feed_path(state, "m3u", account.id);
    super::download(url, &agent, &destination).await?;
    parse_playlist(&destination).await
}

async fn parse_playlist(path: &std::path::Path) -> Result<Vec<StreamFields>, Error> {
    let bytes = tokio::fs::read(path).await?;
    Ok(parse_m3u::parse(&bytes)?
        .entries
        .iter()
        .map(|entry| StreamFields::from_entry(entry, DEFAULT_GROUP))
        .collect())
}

/// Create any group the feed introduced and report which are enabled.
///
/// A group new to this account defaults to enabled or not per the account's
/// `auto_enable_new_groups_live`.
async fn ensure_groups(
    state: &AppState,
    account: &M3uAccount,
    parsed: &[ParsedStream],
) -> Result<BTreeMap<String, (Id, bool)>, Error> {
    let auto_enable = account
        .custom_properties
        .get("auto_enable_new_groups_live")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    let existing: HashMap<String, Id> = db::channels::list_groups(&state.db)
        .await?
        .into_iter()
        .map(|group| (group.name.to_lowercase(), group.id))
        .collect();
    let links: HashMap<Id, bool> = db::m3u::list_group_links(&state.db, Some(account.id))
        .await?
        .into_iter()
        .map(|link| (link.channel_group_id, link.enabled))
        .collect();

    let mut out = BTreeMap::new();
    for name in parsed
        .iter()
        .map(|stream| stream.fields.group.clone())
        .collect::<BTreeSet<_>>()
    {
        let id = match existing.get(&name.to_lowercase()) {
            Some(id) => *id,
            None => {
                db::channels::create_group(&state.db, &name, (None, None))
                    .await?
                    .id
            }
        };

        let enabled = match links.get(&id) {
            Some(enabled) => *enabled,
            None => {
                db::m3u::upsert_group_link(
                    &state.db,
                    &db::m3u::GroupAccountLink {
                        id: 0,
                        channel_group_id: id,
                        m3u_account_id: account.id,
                        enabled: auto_enable,
                        auto_channel_sync: false,
                        auto_sync_channel_start: None,
                        auto_sync_channel_end: None,
                        custom_properties: serde_json::json!({}),
                    },
                )
                .await?;
                auto_enable
            }
        };
        out.insert(name, (id, enabled));
    }

    // A group this feed did not mention still has to appear, with the enabled
    // flag the operator gave it. Reconcile deletes a stream whose group is
    // *disabled*; a group merely absent from one refresh must leave its streams
    // stale instead, or a provider omitting a category for an hour deletes
    // every channel built on it.
    for link in db::m3u::list_group_links(&state.db, Some(account.id)).await? {
        if let Some(group) = db::channels::get_group(&state.db, link.channel_group_id).await?
            && !out.contains_key(&group.name)
        {
            out.insert(group.name, (group.id, link.enabled));
        }
    }

    Ok(out)
}

async fn load_existing(state: &AppState, account_id: Id) -> Result<Vec<ExistingStream>, Error> {
    let groups: HashMap<Id, String> = db::channels::list_groups(&state.db)
        .await?
        .into_iter()
        .map(|group| (group.id, group.name))
        .collect();

    Ok(db::streams::list(
        &state.db,
        &db::streams::StreamFilter {
            m3u_account_id: Some(account_id),
            ..Default::default()
        },
        None,
        None,
    )
    .await?
    .results
    .into_iter()
    .filter_map(|stream| {
        Some(ExistingStream {
            id: stream.id,
            hash: stream.stream_hash.clone()?,
            last_seen: stream.last_seen,
            fields: StreamFields {
                name: stream.name.clone(),
                url: stream.url.clone().unwrap_or_default(),
                logo_url: stream.logo_url.clone().unwrap_or_default(),
                tvg_id: stream.tvg_id.clone().unwrap_or_default(),
                group: stream
                    .channel_group_id
                    .and_then(|id| groups.get(&id).cloned())
                    .unwrap_or_else(|| DEFAULT_GROUP.to_owned()),
                is_adult: stream.is_adult,
                provider_stream_id: stream.stream_id,
                provider_channel_number: stream.stream_chno,
                is_catchup: stream.is_catchup,
                catchup_days: stream.catchup_days,
            },
        })
    })
    .collect())
}

#[derive(Default)]
struct Written {
    inserted: usize,
}

/// Apply the plan, each operation as its own batch.
async fn apply(
    state: &AppState,
    account: &M3uAccount,
    plan: &sync_streams::Plan,
    parsed: &[ParsedStream],
    groups: &HashMap<&str, Id>,
    started_at: chrono::DateTime<Utc>,
    handle: &JobHandle,
) -> Result<Written, Error> {
    let mut written = Written::default();

    let row = |entry: &ParsedStream, id: Id| Stream {
        id,
        name: entry.fields.name.clone(),
        url: Some(entry.fields.url.clone()),
        logo_url: (!entry.fields.logo_url.is_empty()).then(|| entry.fields.logo_url.clone()),
        tvg_id: (!entry.fields.tvg_id.is_empty()).then(|| entry.fields.tvg_id.clone()),
        channel_group_id: groups.get(entry.fields.group.as_str()).copied(),
        m3u_account_id: Some(account.id),
        stream_profile_id: None,
        is_custom: false,
        is_adult: entry.fields.is_adult,
        stream_id: entry.fields.provider_stream_id,
        stream_chno: entry.fields.provider_channel_number,
        stream_hash: Some(entry.hash.clone()),
        last_seen: started_at,
        is_stale: false,
        is_catchup: entry.fields.is_catchup,
        catchup_days: entry.fields.catchup_days,
        custom_properties: serde_json::json!({}),
    };

    for chunk in plan.insert.chunks(BATCH) {
        for index in chunk {
            db::streams::create(&state.db, &row(&parsed[*index], 0)).await?;
            written.inserted += 1;
        }
        handle
            .progress(0.6, format!("{} new streams", written.inserted))
            .await;
    }

    for chunk in plan.update.chunks(BATCH) {
        for (id, index) in chunk {
            // The stream profile is a user choice attached to the row, not a
            // provider field, so it survives the refresh that rewrites the rest.
            let existing = db::streams::get(&state.db, *id).await?;
            let mut updated = row(&parsed[*index], *id);
            updated.stream_profile_id = existing.as_ref().and_then(|s| s.stream_profile_id);
            updated.custom_properties = existing
                .map(|s| s.custom_properties)
                .unwrap_or_else(|| serde_json::json!({}));
            db::streams::save(&state.db, &updated).await?;
        }
    }

    for chunk in plan.touch.chunks(BATCH) {
        db::streams::mark_seen(&state.db, chunk, started_at).await?;
    }
    for chunk in plan.mark_stale.chunks(BATCH) {
        db::streams::mark_stale(&state.db, chunk).await?;
    }

    // A stream some channel has no alternative to is kept, whatever the plan
    // says. Deleting it cascades the `channel_stream` row away and leaves a
    // channel that still appears in every output and plays nothing — and since
    // re-enabling the group recreates the stream under a new id, the assignment
    // does not come back. Held stale instead: visible as "the provider stopped
    // carrying this", still playable if it was only the group that changed.
    let (doomed, spared) = partition_last_streams(state, &plan.delete).await?;
    db::streams::delete_many(&state.db, &doomed).await?;
    if !spared.is_empty() {
        db::streams::mark_stale(&state.db, &spared).await?;
        tracing::warn!(
            account = account.id,
            count = spared.len(),
            "kept streams the refresh would have deleted: each is the only stream on a channel"
        );
    }

    Ok(written)
}

/// Split a delete plan into the rows that may go and the ones some channel
/// depends on entirely.
async fn partition_last_streams(
    state: &AppState,
    delete: &[(Id, sync_streams::DeleteReason)],
) -> Result<(Vec<Id>, Vec<Id>), Error> {
    if delete.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let planned: BTreeSet<Id> = delete.iter().map(|(id, _)| *id).collect();

    // Channel to its full stream list, so "would this leave the channel with
    // nothing?" is answered against every stream it has rather than only the
    // ones this account contributed.
    let rows: Vec<(Id, Id)> = sqlx::query_as("SELECT channel_id, stream_id FROM channel_stream")
        .fetch_all(&state.db)
        .await?;

    let mut by_channel: HashMap<Id, Vec<Id>> = HashMap::new();
    for (channel, stream) in rows {
        by_channel.entry(channel).or_default().push(stream);
    }

    let mut spared: BTreeSet<Id> = BTreeSet::new();
    for streams in by_channel.values() {
        let survivors = streams.iter().filter(|id| !planned.contains(id)).count();
        if survivors == 0 {
            // Keep one — the first, which is the channel's own priority order.
            if let Some(keep) = streams.first() {
                spared.insert(*keep);
            }
        }
    }

    Ok((
        planned
            .iter()
            .filter(|id| !spared.contains(id))
            .copied()
            .collect(),
        spared.into_iter().collect(),
    ))
}

/// A channel auto-sync would create, and the number and name it would carry.
pub(crate) struct PlannedChannel {
    pub stream: Stream,
    pub name: String,
    pub number: f64,
}

pub(crate) struct AutoSyncPlan {
    pub channels: Vec<PlannedChannel>,
    /// Streams that got no number because the group's range ran out.
    pub unnumbered: usize,
    /// What the range was, for the sentence that says it is full.
    pub group: ChannelGroup,
    pub step: f64,
}

/// The group's numbering: the range from the group, the step from the lineup's
/// settings, the mode and its fallback from the link's `custom_properties`,
/// which is where the import finds them.
fn numbering_for(
    group: &ChannelGroup,
    link: &GroupAccountLink,
    step: f64,
) -> sync_channels::Numbering {
    let options = &link.custom_properties;
    sync_channels::Numbering {
        mode: match options
            .get("channel_numbering_mode")
            .and_then(Value::as_str)
            .unwrap_or("fixed")
        {
            "provider" => sync_channels::NumberingMode::Provider,
            "next_available" => sync_channels::NumberingMode::NextAvailable,
            _ => sync_channels::NumberingMode::Fixed,
        },
        // `Provider` falls back to this when the stream carries no number; the
        // other modes start at the range.
        start: options
            .get("channel_numbering_fallback")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| group.number_start.unwrap_or(1.0)),
        end: group.number_end,
        step,
    }
}

fn rename_for(link: &GroupAccountLink) -> Option<sync_channels::Rename<'_>> {
    let options = &link.custom_properties;
    options
        .get("name_regex_pattern")
        .and_then(Value::as_str)
        .filter(|pattern| !pattern.is_empty())
        .map(|pattern| sync_channels::Rename {
            pattern,
            replacement: options.get("name_replace_pattern").and_then(Value::as_str),
            max_length: CHANNEL_NAME_MAX,
        })
}

/// What auto-sync would create for one group link, right now.
///
/// Shared by the refresh and by the preview the API shows before auto-sync is
/// switched on, so the numbers the operator is told are the numbers the next
/// refresh assigns. Only streams attached to no channel are eligible — that is
/// what makes a second run leave the first run's channels alone, and what lets
/// an operator keep a provider's duplicate out of the lineup by attaching it as
/// a failover instead.
///
/// `used` is every number the lineup holds, sorted; a refresh planning several
/// groups threads one list through so the second group sees what the first
/// just claimed. The options live in `custom_properties` because that is where
/// the UI puts them.
pub(crate) async fn plan_auto_sync(
    state: &AppState,
    link: &GroupAccountLink,
    used: &mut Vec<f64>,
) -> Result<AutoSyncPlan, Error> {
    let attached: BTreeSet<Id> =
        sqlx::query_as::<_, (Id,)>("SELECT DISTINCT stream_id FROM channel_stream")
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|(id,)| id)
            .collect();

    let group = db::channels::get_group(&state.db, link.channel_group_id)
        .await?
        .ok_or(Error::NotFound)?;
    let policy: NumberingSettings = settings::load(&state.db).await?;
    let numbering = numbering_for(&group, link, policy.channel_step);
    let rename = rename_for(link);
    let mut own = db::channels::numbers_in_group(&state.db, group.id).await?;

    let streams = db::streams::list(
        &state.db,
        &db::streams::StreamFilter {
            m3u_account_id: Some(link.m3u_account_id),
            group_id: Some(link.channel_group_id),
            ..Default::default()
        },
        None,
        None,
    )
    .await?
    .results;

    let mut plan = AutoSyncPlan {
        channels: Vec::new(),
        unnumbered: 0,
        group: group.clone(),
        step: policy.channel_step,
    };
    for stream in streams {
        if attached.contains(&stream.id) {
            continue;
        }

        // `pick_number` binary-searches `used`, so it stays sorted as numbers
        // are claimed — pushing would make every later lookup miss and hand
        // out the same number twice.
        let Some(number) = sync_channels::pick_number(&numbering, stream.stream_chno, &own, used)
        else {
            plan.unnumbered += 1;
            continue;
        };
        sync_channels::claim(used, number);
        sync_channels::claim(&mut own, number);

        let name = match &rename {
            Some(rule) => sync_channels::rename(&stream.name, rule),
            None => stream.name.clone(),
        };
        plan.channels.push(PlannedChannel {
            stream,
            name,
            number,
        });
    }
    Ok(plan)
}

/// A stream that got no channel because its group's range had no slot left is
/// the one outcome of a refresh that shows up nowhere: the refresh succeeded,
/// the stream is in the catalogue, and Plex simply never sees the channel.
/// Raised on the bell until a refresh finds the range has room again — which
/// is what widening it, or renumbering into a larger one, produces — and
/// cleared then.
async fn report_range(
    state: &AppState,
    link: &GroupAccountLink,
    plan: &AutoSyncPlan,
) -> Result<(), Error> {
    const RANGE_FULL: &str = "auto_sync.range_full";
    let subject = format!("group:{}", link.channel_group_id);
    if plan.unnumbered == 0 {
        db::notifications::clear(&state.db, RANGE_FULL, &subject).await?;
        return Ok(());
    }

    let group = &plan.group;
    let range = match (group.number_start, group.number_end) {
        (Some(start), Some(end)) => format!("{start}–{end}"),
        (Some(start), None) => format!("from {start}"),
        (None, _) => "its range".to_owned(),
    };
    tracing::warn!(
        group = group.id,
        streams = plan.unnumbered,
        "auto sync ran out of channel numbers in its configured range"
    );
    db::notifications::raise(
        &state.db,
        &db::notifications::Notification::new(
            RANGE_FULL,
            &subject,
            db::notifications::Severity::Warning,
            format!("No room left in {}", group.name),
            format!(
                "{} new {} in {} got no channel: the range {range} has no free slot at a \
                 step of {}. Widen the range on the Groups page, or renumber the group into \
                 a larger one, then refresh the provider.",
                plan.unnumbered,
                if plan.unnumbered == 1 {
                    "stream"
                } else {
                    "streams"
                },
                group.name,
                plan.step,
            ),
            serde_json::json!({
                "channel_group_id": group.id,
                "m3u_account_id": link.m3u_account_id,
                "unnumbered": plan.unnumbered,
            }),
        ),
    )
    .await?;
    Ok(())
}

/// Create channels for groups the operator marked `auto_channel_sync`, and
/// report which.
///
/// `plan_auto_sync` decides what to create; this writes it. The ids come back
/// so the refresh can offer the new channels to the guide matcher without
/// touching anything the operator mapped by hand.
async fn auto_create_channels(
    state: &AppState,
    account: &M3uAccount,
    handle: &JobHandle,
) -> Result<Vec<Id>, Error> {
    let links: Vec<_> = db::m3u::list_group_links(&state.db, Some(account.id))
        .await?
        .into_iter()
        .filter(|link| link.enabled && link.auto_channel_sync)
        .collect();
    if links.is_empty() {
        return Ok(Vec::new());
    }

    let mut used = db::channels::numbers_in_use(&state.db).await?;
    let mut created = Vec::new();
    for link in links {
        if handle.cancelled() {
            return Err(super::cancelled());
        }

        let plan = plan_auto_sync(state, &link, &mut used).await?;
        report_range(state, &link, &plan).await?;

        for planned in plan.channels {
            let stream = planned.stream;
            let channel = db::channels::create(
                &state.db,
                &Channel {
                    id: 0,
                    uuid: uuid::Uuid::nil(),
                    channel_number: Some(planned.number),
                    name: planned.name,
                    logo_id: None,
                    channel_group_id: stream.channel_group_id,
                    tvg_id: stream.tvg_id.clone(),
                    tvc_guide_stationid: None,
                    epg_data_id: None,
                    stream_profile_id: None,
                    user_level: UserLevel::Streamer,
                    is_adult: stream.is_adult,
                    hidden_from_output: false,
                    auto_created: true,
                    is_catchup: stream.is_catchup,
                    catchup_days: stream.catchup_days,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                },
            )
            .await?;

            db::channels::set_streams(&state.db, channel.id, &[stream.id]).await?;
            created.push(channel.id);
        }
    }

    Ok(created)
}
