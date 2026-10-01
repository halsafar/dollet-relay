//! XMLTV refresh.
//!
//! Two passes over the downloaded file, not one. The `<channel>` elements
//! decide which `<programme>` elements are worth storing, and XMLTV only
//! conventionally puts channels first — a full feed carries thousands of guide
//! channels against a couple of dozen the lineup maps, so importing programmes
//! for all of them is hundreds of thousands of rows nobody reads. The file is
//! already on disk, so a second pass is a read, not a refetch.

use std::collections::{BTreeMap, HashMap, HashSet};

use dollet_core::db::jobs::Job;
use dollet_core::domain::{EpgData, EpgSource, EpgSourceType, Id, Program};
use dollet_core::parse::xmltv::{self, XmltvItem};
use dollet_core::settings::{self, EpgSettings};
use dollet_core::sync::epg::{
    self as sync_epg, EpgCandidate, NormalizeSettings, Outcome, Thresholds,
};
use dollet_core::{Error, db};
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::api::jobs::{JobHandle, payload_id};
use crate::api::outputs::{self, Output};

pub const KIND: &str = "epg_refresh";

pub fn job_key(source_id: Id) -> String {
    format!("{KIND}:{source_id}")
}

/// Programme rows per commit. Same reasoning as everywhere else: bulk work is
/// never one long transaction, because the WAL cannot checkpoint while one is
/// open and this process always has streaming readers.
const BATCH: usize = 500;

pub async fn run(state: AppState, job: Job, handle: JobHandle) -> Result<String, Error> {
    let source_id = payload_id(&job, "epg_source_id")?;
    let source = db::epg::get_source(&state.db, source_id)
        .await?
        .ok_or(Error::NotFound)?;

    refresh(&state, &source, &handle).await
}

pub async fn refresh(
    state: &AppState,
    source: &EpgSource,
    handle: &JobHandle,
) -> Result<String, Error> {
    // A dummy source generates its guide at request time; there is nothing to
    // fetch and nothing to store.
    if source.source_type == EpgSourceType::Dummy {
        return Ok("dummy source: generated on demand".into());
    }

    let path = match &source.file_path {
        // A local file the operator dropped in.
        Some(file) if !file.is_empty() && tokio::fs::metadata(file).await.is_ok() => {
            std::path::PathBuf::from(file)
        }
        _ => {
            let url = source
                .url
                .as_deref()
                .filter(|url| !url.is_empty())
                .ok_or_else(|| Error::invalid("EPG source has neither a URL nor a file"))?;

            handle.progress(0.05, "downloading").await;
            let destination = super::feed_path(state, "epg", source.id);
            let bytes = super::download(
                url,
                &crate::api::user_agent_for(state, None).await?,
                &destination,
            )
            .await?;
            tracing::info!(source = source.id, bytes, "EPG feed downloaded");
            destination
        }
    };

    handle.progress(0.2, "reading guide channels").await;
    let channels = read_channels(path.clone(), handle.cancel.clone()).await?;
    if channels.is_empty() {
        return Err(Error::upstream("the feed declared no channels"));
    }

    handle.progress(0.35, "storing guide channels").await;
    let guide_channels = channels.len();
    let rows: Vec<EpgData> = channels
        .into_values()
        .map(|channel| channel.into_epg_data(0, Some(source.id)))
        .collect();
    db::epg::upsert_data(&state.db, &rows).await?;

    if handle.cancelled() {
        return Err(super::cancelled());
    }

    // Auto-match before selecting programmes: a channel matched on this pass
    // should get its listings on this pass too, not the next one.
    //
    // Off unless the operator turned it on. This is the one step of a refresh
    // that writes to the user's *channels* rather than to guide data, it runs
    // unattended on a timer, and its only record is a count — see
    // `EpgSettings::epg_auto_match_on_refresh`.
    let epg: EpgSettings = settings::load(&state.db).await?;
    let matched = if epg.epg_auto_match_on_refresh {
        handle.progress(0.45, "matching channels").await;
        auto_match(state, source.id).await?
    } else {
        MatchReport::default()
    };

    handle.progress(0.55, "reading programmes").await;
    let wanted = mapped_tvg_ids(state, source.id).await?;
    let by_tvg_id = data_ids(state, source.id).await?;

    let stored = read_programmes(state, source.id, &path, &wanted, &by_tvg_id, handle).await?;

    let summary = format!(
        "{} guide channels, {} programmes for {} mapped, {} auto-matched{}",
        guide_channels,
        stored.programmes,
        wanted.len(),
        matched.matched,
        match matched.ambiguous.len() {
            0 => String::new(),
            n => format!(", {n} need a decision"),
        }
    );

    db::events::record(
        &state.db,
        "epg_refresh",
        None,
        None,
        &serde_json::json!({
            "source_name": source.name,
            "channels": guide_channels,
            "programs": stored.programmes,
            "skipped_programs": stored.skipped,
            "unmapped_channels": guide_channels.saturating_sub(wanted.len()),
            "auto_matched": matched.matched,
            "ambiguous": matched.ambiguous.len(),
        }),
    )
    .await?;

    // Same reason as the M3U refresh: this is the bulk write, so this is where
    // the WAL is worth giving back.
    db::checkpoint_wal(&state.db).await;

    // Only when the guide actually moved. A scheduled refresh of a feed the
    // provider has not republished promotes nothing, and forcing every client's
    // next fetch to rescan the programme table for identical bytes is the cost
    // the cache exists to avoid.
    if stored.programmes > 0 || stored.cleared > 0 || matched.matched > 0 {
        outputs::invalidate(state, &[Output::Guide]).await;
    }

    Ok(summary)
}

/// First pass: every `<channel>`, keyed by tvg id.
///
/// On a blocking thread because parsing tens of megabytes of XML is solid CPU
/// with no await points, and the runtime threads it would otherwise occupy are
/// the ones serving streams.
async fn read_channels(
    path: std::path::PathBuf,
    cancel: CancellationToken,
) -> Result<BTreeMap<String, xmltv::ParsedChannel>, Error> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let mut reader = xmltv::from_reader(file)?;
        let mut channels = BTreeMap::new();

        while let Some(item) = reader.next_item()? {
            if cancel.is_cancelled() {
                return Err(super::cancelled());
            }
            if let XmltvItem::Channel(channel) = item
                && !channel.tvg_id.is_empty()
            {
                channels.insert(channel.tvg_id.clone(), channel);
            }
        }
        Ok(channels)
    })
    .await
    .map_err(|e| Error::Other(e.into()))?
}

#[derive(Default)]
struct Stored {
    programmes: usize,
    skipped: usize,
    /// Rows deleted from a mapped guide channel this feed carried nothing for.
    /// The guide changed even though nothing was promoted, which is the
    /// difference between dropping the rendered cache and leaving it.
    cleared: u64,
}

/// Second pass: programmes, but only for guide channels a channel maps to.
///
/// Nothing the user can see is written until the parse has finished. The rows
/// go to `program_incoming` as they are read and are promoted per guide channel
/// in one transaction each, so a cancellation, a SIGTERM or a truncated feed
/// leaves the previous guide whole instead of replacing it with however much of
/// the new one had arrived.
async fn read_programmes(
    state: &AppState,
    source_id: Id,
    path: &std::path::Path,
    wanted: &HashSet<String>,
    by_tvg_id: &HashMap<String, Id>,
    handle: &JobHandle,
) -> Result<Stored, Error> {
    // A previous refresh that died before promoting left rows here. They are
    // superseded by what this pass is about to read.
    db::epg::discard_staged(&state.db, source_id).await?;

    // The parse runs on a blocking thread and hands batches back over a
    // channel: the reader is not `Send` and the work is CPU-bound, so doing it
    // inline would both fail to compile and occupy a thread that serves
    // streams. The bounded channel is the backpressure — the parser waits when
    // SQLite is the slower side rather than buffering the whole feed.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<Program>>(4);
    let wanted_ids = wanted.clone();
    let ids_by_tvg_id = by_tvg_id.clone();
    let cancel = handle.cancel.clone();
    let source = path.to_path_buf();

    let parse = tokio::task::spawn_blocking(move || -> Result<usize, Error> {
        let file = std::fs::File::open(&source)?;
        let mut reader = xmltv::from_reader(file)?;
        let mut batch: Vec<Program> = Vec::with_capacity(BATCH);
        let mut skipped = 0usize;

        loop {
            let Some(item) = reader.next_item()? else {
                break;
            };
            if cancel.is_cancelled() {
                return Err(super::cancelled());
            }

            let XmltvItem::Programme(programme) = item else {
                continue;
            };

            let Some(epg_data_id) = wanted_ids
                .contains(&programme.tvg_id)
                .then(|| ids_by_tvg_id.get(&programme.tvg_id))
                .flatten()
            else {
                skipped += 1;
                continue;
            };

            batch.push(programme.into_program(0, *epg_data_id));
            if batch.len() >= BATCH && tx.blocking_send(std::mem::take(&mut batch)).is_err() {
                return Ok(skipped);
            }
        }

        if !batch.is_empty() {
            let _ = tx.blocking_send(batch);
        }
        Ok(skipped)
    });

    let mut stored = Stored::default();
    let mut staged = 0usize;
    while let Some(batch) = rx.recv().await {
        staged += db::epg::stage_programs(&state.db, source_id, &batch).await? as usize;
        handle
            .progress(0.7, format!("{staged} programmes read"))
            .await;
    }

    // Awaited before the swap, so a parse that failed half way through takes
    // its staged rows with it rather than promoting them as a complete guide.
    let skipped = match parse.await.map_err(|e| Error::Other(e.into()))? {
        Ok(skipped) => skipped,
        Err(e) => {
            db::epg::discard_staged(&state.db, source_id).await?;
            return Err(e);
        }
    };
    if handle.cancelled() {
        db::epg::discard_staged(&state.db, source_id).await?;
        return Err(super::cancelled());
    }

    handle.progress(0.85, "replacing guide").await;
    let promoted: HashSet<Id> = db::epg::staged_data_ids(&state.db, source_id)
        .await?
        .into_iter()
        .collect();
    for epg_data_id in &promoted {
        stored.programmes +=
            db::epg::promote_staged(&state.db, source_id, *epg_data_id).await? as usize;
    }

    // A guide channel the lineup maps but this feed carried nothing for. Its
    // old programmes are stale by definition, and the swap above never touched
    // it, so it is cleared here — outside the staging path because there is
    // nothing to put in its place.
    for id in by_tvg_id
        .iter()
        .filter(|(tvg_id, _)| wanted.contains(*tvg_id))
        .map(|(_, id)| *id)
    {
        if !promoted.contains(&id) {
            stored.cleared += db::epg::clear_programs(&state.db, id).await?;
        }
    }

    stored.skipped = skipped;
    Ok(stored)
}

/// The `tvg_id`s some channel actually points at, including through an
/// override — a hand-assigned mapping is exactly the one a user cares about.
async fn mapped_tvg_ids(state: &AppState, source_id: Id) -> Result<HashSet<String>, Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT d.tvg_id FROM epg_data d
         JOIN effective_channel c ON c.epg_data_id = d.id
         WHERE d.epg_source_id = ? AND d.tvg_id IS NOT NULL",
    )
    .bind(source_id)
    .fetch_all(&state.db)
    .await?;

    Ok(rows.into_iter().map(|(tvg_id,)| tvg_id).collect())
}

async fn data_ids(state: &AppState, source_id: Id) -> Result<HashMap<String, Id>, Error> {
    Ok(
        db::epg::list_data(&state.db, Some(source_id), None, 1_000_000)
            .await?
            .into_iter()
            .filter_map(|data| Some((data.tvg_id?, data.id)))
            .collect(),
    )
}

#[derive(Debug, Default)]
pub struct MatchReport {
    pub matched: usize,
    /// Scored in the ambiguous band, which a language model could resolve and
    /// this build deliberately does not carry. Reported so the user makes one
    /// decision rather than finding a channel with no guide and no explanation.
    pub ambiguous: Vec<(Id, Id, f64)>,
}

/// Assign guide data to channels that have none, from one source.
///
/// Only unassigned channels: a mapping the user made by hand must never be
/// overwritten by a fuzzy score.
pub async fn auto_match(state: &AppState, source_id: Id) -> Result<MatchReport, Error> {
    match_unassigned(state, Some(source_id), None).await
}

/// The same pass, over exactly the channels a playlist refresh just created,
/// against every guide.
///
/// Scoped by id rather than by "unassigned" so a refresh cannot reach a channel
/// the operator left unmapped on purpose; the guide job's own pass is where
/// that wider decision belongs.
pub async fn match_new_channels(state: &AppState, ids: &[Id]) -> Result<MatchReport, Error> {
    match_unassigned(state, None, Some(ids)).await
}

async fn match_unassigned(
    state: &AppState,
    source_id: Option<Id>,
    only: Option<&[Id]>,
) -> Result<MatchReport, Error> {
    let epg: EpgSettings = settings::load(&state.db).await?;
    let normalize = NormalizeSettings {
        ignore_prefixes: epg.epg_match_ignore_prefixes.clone(),
        ignore_suffixes: epg.epg_match_ignore_suffixes.clone(),
        ignore_custom: epg.epg_match_ignore_custom.clone(),
    };

    let system: settings::SystemSettings = settings::load(&state.db).await?;
    let region = system.preferred_region.clone();

    let data = db::epg::list_data(&state.db, source_id, None, 1_000_000).await?;
    let candidates: Vec<EpgCandidate<'_>> = data
        .iter()
        .filter_map(|row| {
            Some(EpgCandidate {
                epg_data_id: row.id,
                tvg_id: row.tvg_id.as_deref()?,
                name: row.name.as_str(),
                source_priority: 0,
            })
        })
        .collect();

    let mut report = MatchReport::default();
    if candidates.is_empty() {
        return Ok(report);
    }

    let unassigned = db::channels::list_effective(
        &state.db,
        &db::channels::ChannelFilter::default(),
        None,
        None,
    )
    .await?
    .results
    .into_iter()
    .filter(|channel| {
        channel.epg_data_id.is_none() && only.is_none_or(|ids| ids.contains(&channel.id))
    });

    for channel in unassigned {
        match sync_epg::best_match(
            &channel.name,
            &candidates,
            &normalize,
            Thresholds::bulk(),
            region.as_deref(),
        ) {
            Outcome::Matched { epg_data_id, .. } => {
                let mut row = db::channels::get(&state.db, channel.id)
                    .await?
                    .ok_or(Error::NotFound)?;
                row.epg_data_id = Some(epg_data_id);
                db::channels::save(&state.db, &row).await?;
                // A confident match answers whatever question was outstanding.
                db::epg::clear_suggestion(&state.db, channel.id).await?;
                report.matched += 1;
            }
            Outcome::Ambiguous { epg_data_id, score } => {
                // Persisted rather than only logged: this is the band a model
                // this build does not carry would resolve, and without
                // somewhere to put the answer it degrades to "no guide" with no
                // explanation. The grid shows it as a question.
                db::epg::suggest_match(
                    &state.db,
                    &db::epg::MatchSuggestion {
                        channel_id: channel.id,
                        epg_data_id,
                        score,
                    },
                )
                .await?;
                report.ambiguous.push((channel.id, epg_data_id, score));
            }
            Outcome::NoMatch => {}
        }
    }

    for (channel, candidate, score) in &report.ambiguous {
        tracing::info!(
            channel,
            candidate,
            score,
            "EPG match left for review: the best candidate scored in the ambiguous band"
        );
    }

    Ok(report)
}
