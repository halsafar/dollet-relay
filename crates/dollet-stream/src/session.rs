//! One `(channel, OutputKey)` pair: a ring, the task that fills it, and the
//! clients reading out of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStderr;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use crate::OutputKey;
use crate::client::{ClientDescriptor, ClientId, ClientRecord, ClientStats, Cursor};
use crate::config::StreamConfig;
use crate::failover::{Action, Failover, GiveUp};
use crate::input::{
    SourceProfile, Spawned, StreamSource, Transcode, build_command, open_http, spawn, terminate,
};
use crate::limits::ConnectionLimits;
use crate::logs::{
    BufferingDetector, MediaInfo, Pace, Progress, apply_media_line, parse_pace, parse_progress,
};
use crate::ring::{Ring, RingStats};
use crate::ts::{Packetizer, TS_PACKET_SIZE};

/// What came of asking a session to change source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchOutcome {
    Switched(usize),
    /// No live session for that channel and output.
    NoSession,
    /// The channel has `sources` entries and the index is past the end. Zero
    /// means the session has no source list of its own, which is the case for
    /// an output profile: it reads the raw session's ring, so the raw session
    /// is where a source is chosen.
    OutOfRange {
        sources: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Connecting,
    Streaming,
    Buffering,
    Switching,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionStats {
    pub channel: Uuid,
    pub output: OutputKey,
    pub phase: Phase,
    pub healthy: bool,
    pub source_index: usize,
    pub source_id: Option<i64>,
    pub url: Option<String>,
    pub switches: u32,
    pub last_error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub total_bytes: u64,
    pub buffer: RingStats,
    pub media: MediaInfo,
    pub progress: Progress,
    pub clients: Vec<ClientStats>,
}

struct State {
    phase: Phase,
    source_count: usize,
    source_index: usize,
    source_id: Option<i64>,
    url: Option<String>,
    switches: u32,
    last_error: Option<String>,
    media: MediaInfo,
    progress: Progress,
}

pub struct Session {
    pub channel: Uuid,
    pub output: OutputKey,
    cfg: Arc<StreamConfig>,
    ring: Arc<Ring>,
    /// Owned by the session rather than by the input task: a failover replaces
    /// the input and must not disconnect anybody.
    head_tx: watch::Sender<u64>,
    clients: DashMap<ClientId, Arc<ClientRecord>>,
    client_count: watch::Sender<usize>,
    cancel: watch::Sender<bool>,
    /// Bumped by the stderr reader when buffering has gone on too long. A
    /// counter rather than a flag so a second request during a switch is not
    /// silently swallowed.
    switch_request: watch::Sender<u64>,
    /// Set by `request_source`; taken by the supervisor at the top of its
    /// loop. Separate from the counter above because a manual choice and a
    /// buffering timeout want opposite things from the failover budget.
    requested_source: Mutex<Option<usize>>,
    healthy: AtomicBool,
    stopped: AtomicBool,
    started_at: DateTime<Utc>,
    state: RwLock<State>,
}

impl Session {
    pub(crate) fn new(channel: Uuid, output: OutputKey, cfg: Arc<StreamConfig>) -> Arc<Self> {
        let ring = Arc::new(Ring::new(cfg.ring_max_bytes, cfg.ring_duration));
        Arc::new(Self {
            channel,
            output,
            cfg,
            ring,
            head_tx: watch::channel(0).0,
            clients: DashMap::new(),
            client_count: watch::channel(0).0,
            cancel: watch::channel(false).0,
            switch_request: watch::channel(0).0,
            requested_source: Mutex::new(None),
            healthy: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            started_at: Utc::now(),
            state: RwLock::new(State {
                phase: Phase::Connecting,
                source_count: 0,
                source_index: 0,
                source_id: None,
                url: None,
                switches: 0,
                last_error: None,
                media: MediaInfo::default(),
                progress: Progress::default(),
            }),
        })
    }

    pub(crate) fn config(&self) -> Arc<StreamConfig> {
        self.cfg.clone()
    }

    pub(crate) fn add_client(self: &Arc<Self>, desc: ClientDescriptor, behind: Duration) -> Cursor {
        self.attach(ClientRecord::new(desc, false), behind)
    }

    fn attach(self: &Arc<Self>, record: ClientRecord, behind: Duration) -> Cursor {
        let record = Arc::new(record);
        let at = if behind.is_zero() {
            self.ring.head()
        } else {
            self.ring.cursor_behind(behind)
        };

        self.clients.insert(record.id, record.clone());
        self.client_count.send_replace(self.clients.len());
        tracing::debug!(channel = %self.channel, client = %record.id, at, "client joined");

        Cursor::new(
            self.clone(),
            record,
            self.cfg.clone(),
            self.ring.clone(),
            self.head_tx.subscribe(),
            at,
        )
    }

    /// A consumer inside this process: the transcode behind an output profile.
    ///
    /// It reads at live, because the profile ring adds its own latency on top
    /// of whatever that ring's clients ask for, and it is never hung up on —
    /// the keepalive cap exists to stop a *network* client being held open by
    /// a dead stream, and an expired internal cursor would strand the encoder
    /// with no input for the rest of the session's life.
    pub(crate) fn add_internal_client(self: &Arc<Self>) -> Cursor {
        let record = ClientRecord::new(ClientDescriptor::default(), true);
        let mut cursor = self.attach(record, Duration::ZERO);
        cursor.disable_keepalive_cap();
        cursor
    }

    /// Evicts one client. Returns false when the id is not here, or names the
    /// internal transcode consumer, so a route can answer 404 rather than a
    /// silent success on a stale row.
    ///
    /// This is deliberately not a failure: nothing about it reaches the
    /// failover state machine, which only ever observes the *input* side, so
    /// it cannot spend a retry, count as a drop, or cause a switch. The client
    /// leaves by the same path a disconnecting player takes — its stream ends,
    /// its record is removed on drop, and the reaper sees the count fall
    /// exactly as it would otherwise, including when this was the last one.
    pub fn disconnect_client(&self, id: ClientId) -> bool {
        // Cloned out so the map guard is released before `remove_client` wants
        // the same shard.
        let record = self.clients.get(&id).map(|entry| entry.value().clone());
        let Some(record) = record else {
            return false;
        };
        if !record.evict() {
            return false;
        }

        // Deregister now rather than when the stream is next polled. The
        // client this gets reached for is usually one whose socket has stopped
        // draining, and that is precisely the client the HTTP server is not
        // polling — waiting for it to notice would leave the eviction pending
        // for as long as the wedge lasts. The cursor's own removal on drop
        // then finds nothing and does nothing.
        self.remove_client(id);

        // Wake whoever is parked at the head, so a client that *is* being
        // polled ends now instead of after a keepalive interval.
        self.head_tx.send_modify(|_| {});
        true
    }

    pub(crate) fn remove_client(&self, id: ClientId) {
        if self.clients.remove(&id).is_some() {
            self.client_count.send_replace(self.clients.len());
            tracing::debug!(channel = %self.channel, client = %id, "client left");
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.ring.total_bytes()
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub(crate) fn watch_client_count(&self) -> watch::Receiver<usize> {
        self.client_count.subscribe()
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.state.write().phase = Phase::Stopped;
        let _ = self.cancel.send(true);
        // Wake every client parked at the head so it observes the stop rather
        // than waiting out its keepalive interval first, and the reaper so it
        // stops waiting on a count that will never change again.
        self.head_tx.send_modify(|_| {});
        self.client_count.send_modify(|_| {});
        tracing::info!(channel = %self.channel, output = ?self.output, "session stopped");
    }

    /// The ring is the source of truth; the watch send is only the wakeup.
    fn publish(&self, chunks: Vec<Bytes>) {
        if chunks.is_empty() {
            return;
        }
        let mut head = 0;
        for chunk in chunks {
            head = self.ring.push(chunk);
        }
        self.head_tx.send_replace(head);
    }

    fn set_healthy(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Release);
    }

    fn set_phase(&self, phase: Phase) {
        let mut state = self.state.write();
        if state.phase != Phase::Stopped {
            state.phase = phase;
        }
    }

    fn note_error(&self, error: String) {
        tracing::warn!(channel = %self.channel, output = ?self.output, %error, "upstream failed");
        self.state.write().last_error = Some(error);
    }

    fn begin_source(&self, index: usize, source: &StreamSource) {
        let mut state = self.state.write();
        state.source_index = index;
        state.source_id = Some(source.id);
        state.url = Some(source.url.clone());
        if state.phase != Phase::Stopped {
            state.phase = Phase::Connecting;
        }
    }

    fn note_switch(&self) {
        let mut state = self.state.write();
        state.switches += 1;
        if state.phase != Phase::Stopped {
            state.phase = Phase::Switching;
        }
        // Codec facts belong to the source that is going away.
        state.media = MediaInfo::default();
        state.progress = Progress::default();
    }

    fn request_switch(&self) {
        self.switch_request.send_modify(|n| *n += 1);
    }

    /// Moves a live session onto a different source by index.
    ///
    /// Returns what a route needs to answer honestly. The switch itself is not
    /// a failure: the supervisor applies it without spending a retry or
    /// recording an error, and clients keep reading -- the ring and its wakeup
    /// belong to the session, and the packetizer is reset on every input
    /// teardown so the old source's trailing packet cannot splice onto the
    /// new one's first bytes.
    pub fn request_source(&self, index: usize) -> SwitchOutcome {
        let sources = self.state.read().source_count;
        if index >= sources {
            return SwitchOutcome::OutOfRange { sources };
        }
        *self.requested_source.lock() = Some(index);
        // Interrupt whatever the supervisor is doing -- a read, or a wait --
        // so it comes back round promptly.
        self.request_switch();
        SwitchOutcome::Switched(index)
    }

    /// The next source in the channel's order, wrapping. What the Stats page
    /// offers when a stream is technically alive but visibly bad.
    pub fn request_next_source(&self) -> SwitchOutcome {
        let (sources, current) = {
            let state = self.state.read();
            (state.source_count, state.source_index)
        };
        if sources == 0 {
            return SwitchOutcome::OutOfRange { sources };
        }
        self.request_source((current + 1) % sources)
    }

    fn take_requested_source(&self) -> Option<usize> {
        self.requested_source.lock().take()
    }

    fn has_requested_source(&self) -> bool {
        self.requested_source.lock().is_some()
    }

    fn note_source_change(&self) {
        let mut state = self.state.write();
        if state.phase != Phase::Stopped {
            state.phase = Phase::Switching;
        }
        // Codec facts belong to the source being left behind.
        state.media = MediaInfo::default();
        state.progress = Progress::default();
    }

    pub fn stats(&self) -> SessionStats {
        let state = self.state.read();
        SessionStats {
            channel: self.channel,
            output: self.output,
            phase: state.phase,
            healthy: self.is_healthy(),
            source_index: state.source_index,
            source_id: state.source_id,
            url: state.url.clone(),
            switches: state.switches,
            last_error: state.last_error.clone(),
            started_at: self.started_at,
            total_bytes: self.total_bytes(),
            buffer: self.ring.stats(),
            media: state.media.clone(),
            progress: state.progress,
            clients: self.clients.iter().map(|c| c.stats()).collect(),
        }
    }

    /// Returns false when the wait was cut short by a stop.
    ///
    /// Also wakes on a switch request, and that is not a nicety. The longest
    /// wait here is the rotation cooldown, which is armed precisely when every
    /// source has failed a pass -- which is precisely when an operator reaches
    /// for "choose source". Sleeping through it would have the request answered
    /// as a success and then do nothing for up to a minute.
    async fn sleep(&self, duration: Duration) -> bool {
        let mut cancel = self.cancel.subscribe();
        let mut switch = self.switch_request.subscribe();
        if *cancel.borrow_and_update() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(duration) => true,
            _ = switch.changed() => true,
            _ = cancel.changed() => false,
        }
    }
}

enum PumpEnd {
    Eof,
    Idle,
    Error(String),
    Cancelled,
    SwitchRequested,
}

/// Reads an input into the ring until it stops producing.
///
/// The first read gets `connection_timeout` rather than `stream_timeout`,
/// because a command that never emits a byte has not connected at all.
async fn pump<R: tokio::io::AsyncRead + Unpin>(
    session: &Session,
    reader: &mut R,
    packetizer: &mut Packetizer,
) -> PumpEnd {
    let cfg = session.config();
    let mut cancel = session.cancel.subscribe();
    let mut switch = session.switch_request.subscribe();
    let mut buf = vec![0u8; cfg.chunk_size.max(TS_PACKET_SIZE)];
    let mut idle_budget = cfg.connection_timeout;

    loop {
        if *cancel.borrow_and_update() {
            return PumpEnd::Cancelled;
        }
        let read = tokio::time::timeout(idle_budget, reader.read(&mut buf));

        let n = tokio::select! {
            biased;
            _ = cancel.changed() => return PumpEnd::Cancelled,
            _ = switch.changed() => return PumpEnd::SwitchRequested,
            result = read => match result {
                Err(_) => return PumpEnd::Idle,
                Ok(Ok(0)) => return PumpEnd::Eof,
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return PumpEnd::Error(e.to_string()),
            },
        };

        if !session.is_healthy() {
            session.set_healthy(true);
            session.set_phase(Phase::Streaming);
        }
        idle_budget = cfg.stream_timeout;
        session.publish(packetizer.push(&buf[..n]));
    }
}

async fn read_stderr(session: Arc<Session>, stderr: ChildStderr) {
    let cfg = session.config();
    let mut detector = BufferingDetector::new(cfg.buffering_speed, cfg.buffering_timeout);
    let mut reader = stderr;
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pending.extend_from_slice(&buf[..n]);

        // ffmpeg terminates its progress lines with a carriage return so the
        // console can overwrite them; splitting on newline alone never sees a
        // speed sample.
        while let Some(at) = pending.iter().position(|&b| b == b'\r' || b == b'\n') {
            let line = String::from_utf8_lossy(&pending[..at]).trim().to_owned();
            pending.drain(..=at);
            if !line.is_empty() {
                observe_stderr(&session, &line, &mut detector);
            }
        }
        // A single "line" this long is a binary payload on the wrong pipe.
        if pending.len() > 16 * 1024 {
            pending.clear();
        }
    }
}

fn observe_stderr(session: &Session, line: &str, detector: &mut BufferingDetector) {
    // ffmpeg puts the badge numbers and the pace on the same line; VLC reports
    // the pace separately; streamlink reports it not at all.
    let pace = if let Some(progress) = parse_progress(line) {
        session.state.write().progress = progress;
        progress.speed.map(Pace::Measured)
    } else {
        parse_pace(line)
    };

    if let Some(pace) = pace {
        let now = Instant::now();
        let overdue = match pace {
            Pace::Measured(speed) => detector.sample(speed, now),
            Pace::Stalled => detector.stalled(now),
        };

        if overdue {
            // An output profile has nowhere to switch to, and restarting the
            // encoder cannot make the host faster -- it would only cost every
            // client a gap. Only a channel source, where there is another
            // provider to try, acts on this.
            if session.output == OutputKey::Raw {
                tracing::warn!(
                    channel = %session.channel,
                    ?pace,
                    "buffering past the timeout, switching source"
                );
                session.request_switch();
            } else {
                tracing::warn!(
                    channel = %session.channel,
                    ?pace,
                    "transcode cannot keep up with real time"
                );
                session.set_phase(Phase::Buffering);
            }
        } else if detector.is_buffering() {
            session.set_phase(Phase::Buffering);
        } else {
            session.set_phase(Phase::Streaming);
        }
        return;
    }

    let learned = {
        let mut state = session.state.write();
        apply_media_line(&mut state.media, line)
    };
    if !learned {
        let lower = line.to_ascii_lowercase();
        if lower.contains("error") || lower.contains("failed") || lower.contains("invalid") {
            tracing::warn!(channel = %session.channel, line, "encoder reported a problem");
        } else {
            tracing::trace!(channel = %session.channel, line, "encoder");
        }
    }
}

enum Attempt {
    Cancelled,
    /// The connection was never established.
    Failed(String),
    Ended {
        uptime: Duration,
        switch_requested: bool,
        reason: String,
    },
}

async fn attempt_source(
    session: &Arc<Session>,
    source: &StreamSource,
    http: &reqwest::Client,
    limits: &ConnectionLimits,
    packetizer: &mut Packetizer,
) -> Attempt {
    // A full account counts as an ordinary failure rather than jumping
    // straight to the next source. Retrying looks pointless but is not: the
    // common way to hit this is channel surfing on a one- or two-stream
    // account, where the previous session's slot is released a moment later by
    // the reaper, and the retry backoff is exactly long enough to catch it.
    // Exhausting the retries then moves on as usual.
    let _slot = match source.limit {
        Some(limit) => match limits.acquire(limit.key, limit.max_streams) {
            Some(guard) => Some(guard),
            None => {
                return Attempt::Failed(format!(
                    "account {} is already at its {} stream limit",
                    limit.key, limit.max_streams
                ));
            }
        },
        None => None,
    };

    let cfg = session.config();
    let started = Instant::now();

    match &source.profile {
        // A redirect source reached here because failover walked onto it after
        // the client was already committed to a proxied body; there is no way
        // to 302 a client mid-stream, so it is proxied instead.
        SourceProfile::Proxy | SourceProfile::Redirect => {
            let mut reader = match open_http(
                http,
                &source.url,
                &source.user_agent,
                cfg.connection_timeout,
            )
            .await
            {
                Ok(reader) => reader,
                Err(e) => return Attempt::Failed(e.to_string()),
            };
            finish(
                session,
                pump(session, &mut reader, packetizer).await,
                started,
            )
        }
        SourceProfile::Command {
            command,
            parameters,
        } => {
            let argv = match build_command(
                command,
                parameters,
                &source.url,
                &source.user_agent,
                session.channel,
            ) {
                Ok(argv) => argv,
                Err(e) => return Attempt::Failed(e.to_string()),
            };
            let spawned = match spawn(&argv, false) {
                Ok(spawned) => spawned,
                Err(e) => return Attempt::Failed(e.to_string()),
            };
            let Spawned {
                child,
                mut stdout,
                stderr,
                ..
            } = spawned;

            let reader = tokio::spawn(read_stderr(session.clone(), stderr));
            let end = pump(session, &mut stdout, packetizer).await;
            reader.abort();
            // Close the pipe before reaping: a profile like
            // `sh -c "streamlink ... | ffmpeg ..."` leaves a grandchild that
            // only a dead pipe tells to stop.
            drop(stdout);
            terminate(child).await;
            finish(session, end, started)
        }
    }
}

fn finish(session: &Session, end: PumpEnd, started: Instant) -> Attempt {
    let uptime = started.elapsed();
    let (switch_requested, reason) = match end {
        PumpEnd::Cancelled => return Attempt::Cancelled,
        PumpEnd::SwitchRequested => (true, "buffering timeout".to_owned()),
        PumpEnd::Eof => (false, "upstream closed the stream".to_owned()),
        PumpEnd::Idle => (
            false,
            format!("no data for {}s", session.config().stream_timeout.as_secs()),
        ),
        PumpEnd::Error(e) => (false, e),
    };
    Attempt::Ended {
        uptime,
        switch_requested,
        reason,
    }
}

/// The failover loop. Owns the packetizer, so the reset that keeps a dead
/// process's trailing bytes from splicing onto the next one's first bytes
/// happens on every teardown rather than only on a URL change.
pub(crate) async fn run_upstream(
    session: Arc<Session>,
    sources: Vec<StreamSource>,
    http: reqwest::Client,
    limits: Arc<ConnectionLimits>,
) {
    let cfg = session.config();
    let mut failover = Failover::new(cfg.failover.clone(), sources.len());
    let mut packetizer = Packetizer::new(cfg.chunk_size);
    session.state.write().source_count = sources.len();

    while !session.is_stopped() {
        // Every way a choice can arrive ends up here: interrupting a read,
        // and arriving between attempts. Both the pump and `Session::sleep`
        // wake on the request rather than running to completion, so a choice
        // is neither dropped nor left waiting out a cooldown that can be a
        // minute long.
        if let Some(chosen) = session.take_requested_source()
            && failover.select(chosen)
        {
            tracing::info!(channel = %session.channel, chosen, "source chosen by request");
            session.note_source_change();
        }

        let index = failover.current();
        let Some(source) = sources.get(index) else {
            break;
        };
        session.begin_source(index, source);
        tracing::info!(
            channel = %session.channel,
            output = ?session.output,
            source = source.id,
            url = %source.url,
            "connecting"
        );

        let delivered = session.total_bytes();
        let attempt = attempt_source(&session, source, &http, &limits, &mut packetizer).await;

        // "Has this channel ever worked?" is what decides whether an exhausted
        // source list wraps or gives up, and the only honest evidence for it is
        // bytes that reached the ring.
        if session.total_bytes() > delivered {
            failover.on_connected();
        }
        session.set_healthy(false);
        packetizer.reset();

        let action = match attempt {
            Attempt::Cancelled => break,
            Attempt::Failed(error) => {
                session.note_error(error);
                failover.on_failure(Instant::now())
            }
            Attempt::Ended {
                uptime,
                switch_requested,
                reason,
            } => {
                if uptime >= cfg.failover.stable_threshold {
                    failover.on_stable();
                }
                // A read cut short by somebody choosing a source is not a
                // failure and gets no backoff: straight back round, where the
                // choice is applied.
                if switch_requested && session.has_requested_source() {
                    continue;
                }
                session.note_error(reason);
                if switch_requested {
                    failover.try_switch(Instant::now())
                } else {
                    failover.on_failure(Instant::now())
                }
            }
        };

        if !apply(&session, &mut failover, action).await {
            break;
        }
    }

    session.stop();
}

/// Carries out a failover decision. Returns false when the session is done.
async fn apply(session: &Session, failover: &mut Failover, mut action: Action) -> bool {
    loop {
        // A choice preempts any pause. It is applied at the top of the
        // supervisor loop, so all this has to do is stop waiting and let it
        // get there.
        if session.has_requested_source() {
            return true;
        }
        match action {
            Action::Retry { after } => return session.sleep(after).await,
            Action::Switch { .. } => {
                session.note_switch();
                return true;
            }
            Action::Wait { after } => {
                session.set_phase(Phase::Switching);
                if !session.sleep(after).await {
                    return false;
                }
                action = failover.try_switch(Instant::now());
            }
            Action::GiveUp(reason) => {
                tracing::warn!(
                    channel = %session.channel,
                    output = ?session.output,
                    ?reason,
                    "no usable source left"
                );
                session.set_phase(Phase::Failed);
                if reason == GiveUp::ColdStart || reason == GiveUp::NoSources {
                    session.state.write().last_error.get_or_insert_with(|| {
                        "every upstream source failed to connect".to_owned()
                    });
                }
                return false;
            }
        }
    }
}

/// An output profile: a ring downstream of a ring. The transcode is a client
/// of the raw session, so the raw session stays alive exactly as long as some
/// profile is reading it.
///
/// The cursor is acquired once for the whole function rather than per respawn.
/// A momentary gap in the raw session's client list is all the reaper needs to
/// unmap it — `channel_shutdown_delay` is legitimately zero — which would take
/// this session down with it every time the encoder restarted.
pub(crate) async fn run_transcode(
    session: Arc<Session>,
    upstream: Arc<Session>,
    transcode: Transcode,
) {
    let cfg = session.config();
    let mut packetizer = Packetizer::new(cfg.chunk_size);

    let argv = match build_command(
        &transcode.command,
        &transcode.parameters,
        "pipe:0",
        "",
        session.channel,
    ) {
        Ok(argv) => argv,
        Err(e) => {
            session.note_error(e.to_string());
            session.set_phase(Phase::Failed);
            session.stop();
            return;
        }
    };

    let mut source = upstream.add_internal_client();
    let mut failures: u32 = 0;

    while !session.is_stopped() && !upstream.is_stopped() {
        let started = Instant::now();
        let reason = match spawn(&argv, true) {
            Err(e) => e.to_string(),
            Ok(Spawned {
                child,
                mut stdout,
                stdin,
                stderr,
            }) => {
                let mut stdin = stdin.expect("stdin piped");
                let reader = tokio::spawn(read_stderr(session.clone(), stderr));

                // Both pipes have to be serviced at once or the encoder
                // deadlocks against whichever one is left unattended.
                let end = {
                    let mut feeding = std::pin::pin!(feed(&mut source, &mut stdin));
                    let mut pumping = std::pin::pin!(pump(&session, &mut stdout, &mut packetizer));
                    let mut fed = false;
                    loop {
                        tokio::select! {
                            end = &mut pumping => break end,
                            () = &mut feeding, if !fed => {
                                fed = true;
                                // The feeder normally ends because the encoder
                                // died, and the pump is about to say so. If the
                                // raw session ended instead, there is nothing
                                // left to drain.
                                if upstream.is_stopped() {
                                    break PumpEnd::Eof;
                                }
                            }
                        }
                    }
                };

                reader.abort();
                // Close both pipes before reaping, so anything the command
                // spawned behind itself sees EOF and follows it down.
                drop(stdout);
                drop(stdin);
                terminate(child).await;

                match finish(&session, end, started) {
                    Attempt::Cancelled => break,
                    Attempt::Failed(reason) => reason,
                    Attempt::Ended { uptime, reason, .. } => {
                        if uptime >= cfg.failover.stable_threshold {
                            failures = 0;
                        }
                        reason
                    }
                }
            }
        };

        session.set_healthy(false);
        packetizer.reset();
        session.note_error(reason);

        // A transcode has nowhere to fail over to, so it retries for as long
        // as it has clients instead of spending a bounded budget: three
        // encoder exits inside the retry window must not leave the channel
        // dead for the next half hour. The reaper ends this loop when the last
        // client leaves, and a client that never sees data hits its own
        // keepalive cap, so the retry is bounded by demand rather than by a
        // counter.
        failures = failures.saturating_add(1);
        let backoff = cfg
            .failover
            .retry_backoff_step
            .saturating_mul(failures)
            .min(cfg.failover.retry_backoff_max);
        if !session.sleep(backoff).await {
            break;
        }
    }

    session.stop();
}

async fn feed(cursor: &mut Cursor, stdin: &mut tokio::process::ChildStdin) {
    while let Some(chunk) = cursor.next_chunk().await {
        if stdin.write_all(&chunk).await.is_err() {
            // The encoder exited; the pump on its stdout reports that.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::{TsGenerator, ordinals, test_config};

    fn session(cfg: StreamConfig) -> Arc<Session> {
        Session::new(Uuid::new_v4(), OutputKey::Raw, Arc::new(cfg))
    }

    /// One ring chunk of synthetic transport stream.
    fn chunk(generator: &mut TsGenerator, packets: usize) -> Vec<Bytes> {
        vec![generator.bytes(packets)]
    }

    #[tokio::test]
    async fn every_client_reads_the_same_bytes_from_its_own_cursor() {
        let session = session(test_config());
        let mut generator = TsGenerator::new();

        let mut a = session.add_client(ClientDescriptor::default(), Duration::ZERO);
        let mut b = session.add_client(ClientDescriptor::default(), Duration::ZERO);
        assert_eq!(session.client_count(), 2);

        session.publish(chunk(&mut generator, 4));

        let from_a = a.next_chunk().await.expect("chunk");
        let from_b = b.next_chunk().await.expect("chunk");
        assert_eq!(from_a, from_b);
        assert_eq!(ordinals(&from_a), vec![0, 1, 2, 3]);

        drop(b);
        assert_eq!(session.client_count(), 1);
        assert_eq!(session.stats().clients.len(), 1);
        assert_eq!(session.stats().clients[0].bytes_sent, from_a.len() as u64);
    }

    #[tokio::test]
    async fn a_slow_client_is_skipped_forward_rather_than_holding_anyone_up() {
        // Room for three chunks, so the tenth push has rolled the first away.
        let mut cfg = test_config();
        cfg.ring_max_bytes = TS_PACKET_SIZE * 4 * 3;
        let session = session(cfg);
        let mut generator = TsGenerator::new();

        let mut slow = session.add_client(ClientDescriptor::default(), Duration::ZERO);
        let mut fast = session.add_client(ClientDescriptor::default(), Duration::ZERO);

        session.publish(chunk(&mut generator, 4));
        assert_eq!(
            ordinals(&fast.next_chunk().await.expect("chunk")),
            vec![0, 1, 2, 3]
        );

        // The producer never waits on the slow cursor.
        for _ in 1..10 {
            session.publish(chunk(&mut generator, 4));
        }

        let recovered = slow.next_chunk().await.expect("chunk");
        assert_eq!(
            ordinals(&recovered),
            vec![28, 29, 30, 31],
            "slow client should resume at the oldest surviving chunk"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_client_starts_the_configured_distance_behind_live() {
        let mut cfg = test_config();
        cfg.new_client_behind = Duration::from_secs(3);
        let session = session(cfg);
        let mut generator = TsGenerator::new();

        for _ in 0..10 {
            session.publish(chunk(&mut generator, 4));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        let mut client = session.add_client(ClientDescriptor::default(), Duration::from_secs(3));
        let first = client.next_chunk().await.expect("chunk");
        assert_eq!(ordinals(&first), vec![28, 29, 30, 31]);
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_holds_a_dead_stream_open_but_only_to_the_cap() {
        let session = session(test_config());
        let mut client = session.add_client(ClientDescriptor::default(), Duration::ZERO);
        session.set_healthy(false);

        let mut packets = 0;
        while let Some(data) = client.next_chunk().await {
            assert_eq!(data, crate::ts::null_packet());
            packets += 1;
            assert!(packets < 100, "keepalive never stopped");
        }

        // max_keepalive / keepalive_interval, give or take the first tick.
        assert!((9..=11).contains(&packets), "sent {packets} keepalives");
    }

    #[tokio::test(start_paused = true)]
    async fn keepalive_does_not_fire_while_the_stream_is_healthy() {
        let session = session(test_config());
        let mut client = session.add_client(ClientDescriptor::default(), Duration::ZERO);
        session.set_healthy(true);

        let mut generator = TsGenerator::new();
        let pending = tokio::spawn(async move { client.next_chunk().await });
        tokio::time::sleep(Duration::from_secs(5)).await;
        session.publish(chunk(&mut generator, 4));

        let data = pending.await.expect("task").expect("chunk");
        assert_eq!(ordinals(&data), vec![0, 1, 2, 3]);
    }

    #[tokio::test]
    async fn a_stopped_session_still_hands_over_what_it_buffered() {
        let session = session(test_config());
        let mut generator = TsGenerator::new();
        let mut client = session.add_client(ClientDescriptor::default(), Duration::ZERO);

        session.publish(chunk(&mut generator, 4));
        session.publish(chunk(&mut generator, 4));
        session.stop();

        // Several seconds of video are sitting in the ring at that moment, and
        // that tail is exactly what covers the gap while a player reconnects.
        let mut seen = Vec::new();
        while let Some(data) = client.next_chunk().await {
            seen.extend_from_slice(&data);
        }
        assert_eq!(ordinals(&seen), (0..8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_transcode_that_cannot_keep_up_is_reported_not_restarted() {
        // An output profile has nowhere to switch to, and bouncing the encoder
        // cannot make the host faster -- it would only cost every client a gap
        // every buffering_timeout for as long as the overload lasts.
        let session = Session::new(
            Uuid::new_v4(),
            OutputKey::Profile(4),
            Arc::new(test_config()),
        );
        let cfg = session.config();
        let mut detector = BufferingDetector::new(cfg.buffering_speed, cfg.buffering_timeout);
        let switches = session.switch_request.subscribe();

        let slow = "frame=2 fps=5 bitrate=400kbits/s speed=0.2x";
        observe_stderr(&session, slow, &mut detector);
        tokio::time::sleep(cfg.buffering_timeout + Duration::from_millis(20)).await;
        observe_stderr(&session, slow, &mut detector);

        assert!(!switches.has_changed().expect("live"));
        assert_eq!(session.stats().phase, Phase::Buffering);
    }

    #[tokio::test]
    async fn stopping_a_session_ends_every_client() {
        let session = session(test_config());
        let mut client = session.add_client(ClientDescriptor::default(), Duration::ZERO);
        session.set_healthy(true);

        let waiting = tokio::spawn(async move { client.next_chunk().await });
        tokio::task::yield_now().await;
        session.stop();

        assert!(waiting.await.expect("task").is_none());
        assert_eq!(session.stats().phase, Phase::Stopped);
    }

    #[tokio::test]
    async fn stderr_drives_the_stats_badges_and_the_buffering_switch() {
        let session = session(test_config());
        let cfg = session.config();
        let mut detector = BufferingDetector::new(cfg.buffering_speed, cfg.buffering_timeout);
        let switches = session.switch_request.subscribe();

        observe_stderr(&session, "Input #0, mpegts, from 'pipe:0':", &mut detector);
        observe_stderr(
            &session,
            "  Stream #0:0: Video: h264 (High), yuv420p, 1280x720, 25 fps",
            &mut detector,
        );
        observe_stderr(
            &session,
            "frame=1 fps=25 bitrate=4000kbits/s speed=1.0x",
            &mut detector,
        );

        let stats = session.stats();
        assert_eq!(stats.media.video_codec.as_deref(), Some("h264"));
        assert_eq!(stats.media.resolution().as_deref(), Some("1280x720"));
        assert_eq!(stats.progress.speed, Some(1.0));
        assert!(!switches.has_changed().expect("live"));

        // Sustained slowness asks the supervisor to move on.
        let slow = "frame=2 fps=5 bitrate=400kbits/s speed=0.2x";
        observe_stderr(&session, slow, &mut detector);
        assert_eq!(session.stats().phase, Phase::Buffering);
        tokio::time::sleep(cfg.buffering_timeout + Duration::from_millis(20)).await;
        observe_stderr(&session, slow, &mut detector);

        assert!(switches.has_changed().expect("live"));
    }

    #[tokio::test]
    async fn unparseable_stderr_is_logged_rather_than_applied() {
        let session = session(test_config());
        let cfg = session.config();
        let mut detector = BufferingDetector::new(cfg.buffering_speed, cfg.buffering_timeout);

        observe_stderr(&session, "Conversion failed!", &mut detector);
        observe_stderr(&session, "  configuration: --enable-gpl", &mut detector);
        assert_eq!(session.stats().media, MediaInfo::default());
    }
}
