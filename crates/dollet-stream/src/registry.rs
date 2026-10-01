use std::sync::{Arc, Weak};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use uuid::Uuid;

use crate::client::{ClientDescriptor, ClientId, ClientStream};
use crate::config::StreamConfig;
use crate::input::{SourceProfile, StreamSource, Transcode};
use crate::limits::ConnectionLimits;
use crate::session::{Session, SessionStats, SwitchOutcome, run_transcode, run_upstream};
use crate::{OutputKey, StreamError};

/// Everything the engine needs to serve a channel. Resolving a channel UUID
/// into this is `dollet-server`'s job; the engine never reads a database.
#[derive(Debug, Clone)]
pub struct ChannelSpec {
    pub channel: Uuid,
    /// Tried in order. The first entry is the preferred source.
    pub sources: Vec<StreamSource>,
    /// Required when connecting to an `OutputKey::Profile`.
    pub transcode: Option<Transcode>,
}

pub enum Connection {
    Stream(ClientStream),
    /// The source's stream profile says to hand the client the upstream URL
    /// instead of proxying it. Answer with a 302 and stay out of the data path.
    Redirect {
        url: String,
    },
}

struct Inner {
    cfg: Arc<StreamConfig>,
    http: reqwest::Client,
    limits: Arc<ConnectionLimits>,
    sessions: DashMap<(Uuid, OutputKey), Arc<Session>>,
}

/// The live sessions, keyed by `(channel, output)`.
///
/// Keyed by the pair and not by the channel because an output profile is a
/// transcode whose output is itself a buffer clients read: a ring downstream
/// of a ring, one process per active pair however many clients arrive.
pub struct Registry {
    inner: Arc<Inner>,
}

impl Registry {
    /// `http` comes from the caller so this crate never builds a bare
    /// `reqwest::Client`: every upstream URL is attacker-influenced and must
    /// go through the SSRF-guarded client in `dollet-core`.
    pub fn new(cfg: Arc<StreamConfig>, http: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg,
                http,
                limits: Arc::new(ConnectionLimits::default()),
                sessions: DashMap::new(),
            }),
        }
    }

    /// Attach a client, opening the session and its input task if this is the
    /// first one.
    ///
    /// Synchronous but **must be called from inside a tokio runtime**: opening
    /// a session spawns its supervisor. It is not `async` because nothing here
    /// awaits, and making a handler await a lock it does not need would be
    /// worse than the constraint.
    pub fn connect(
        &self,
        output: OutputKey,
        spec: ChannelSpec,
        client: ClientDescriptor,
    ) -> Result<Connection, StreamError> {
        let first = spec.sources.first().ok_or(StreamError::NoSources)?;

        // A redirect profile takes the server out of the data path entirely,
        // so there is nothing to transcode and no session to open.
        if first.profile == SourceProfile::Redirect {
            return Ok(Connection::Redirect {
                url: first.url.clone(),
            });
        }

        // Checked before anything is created, so a malformed profile request
        // does not leave an orphan raw session behind to be reaped.
        if output != OutputKey::Raw && spec.transcode.is_none() {
            return Err(StreamError::MissingTranscode);
        }

        // The reaper stops a session before taking it out of the map, so a
        // connect that lands in that window sees the stop and asks again;
        // the second pass replaces the dead entry.
        for _ in 0..2 {
            let raw = self.ensure_raw(spec.channel, &spec.sources);
            let session = match (output, spec.transcode.clone()) {
                (OutputKey::Raw, _) => raw,
                (OutputKey::Profile(id), Some(transcode)) => {
                    self.ensure_profile(spec.channel, id, raw, transcode)
                }
                (OutputKey::Profile(_), None) => return Err(StreamError::MissingTranscode),
            };

            let cursor = session.add_client(client.clone(), self.inner.cfg.new_client_behind);
            if !session.is_stopped() {
                return Ok(Connection::Stream(ClientStream::new(cursor)));
            }
        }
        Err(StreamError::Upstream(
            "session was shut down while connecting".into(),
        ))
    }

    pub fn stats(&self) -> Vec<SessionStats> {
        self.inner
            .sessions
            .iter()
            .map(|entry| entry.value().stats())
            .collect()
    }

    pub fn session(&self, channel: Uuid, output: OutputKey) -> Option<Arc<Session>> {
        self.inner
            .sessions
            .get(&(channel, output))
            .map(|entry| entry.value().clone())
    }

    /// Stops every session for a channel, raw and profiles alike. Clients are
    /// disconnected; this is the admin action, not the idle path.
    pub fn stop_channel(&self, channel: Uuid) {
        for entry in self.inner.sessions.iter() {
            if entry.key().0 == channel {
                entry.value().stop();
            }
        }
    }

    /// Stops every session this registry holds.
    ///
    /// Reachable without dropping the registry because `dollet-server` keeps one
    /// in a `static`, which is never dropped: a shutdown that cannot reach this
    /// leaves every client stream open and the drain waits for bodies that by
    /// construction never end.
    pub fn stop_all(&self) {
        for entry in self.inner.sessions.iter() {
            entry.value().stop();
        }
    }

    /// Disconnects one client of a channel, whichever of its outputs it is
    /// reading. Returns false when no such client is here — a stale row on the
    /// Stats page, or the internal transcode consumer — so the route can
    /// answer 404 instead of a silent success.
    ///
    /// The pair to `stop_channel`: this evicts one viewer and leaves the
    /// channel running for everyone else, that one ends the channel.
    pub fn disconnect_client(&self, channel: Uuid, client: ClientId) -> bool {
        self.inner
            .sessions
            .iter()
            .filter(|entry| entry.key().0 == channel)
            .any(|entry| entry.value().disconnect_client(client))
    }

    /// Moves a live session onto a specific source, by index into the list the
    /// caller supplied when the channel was opened.
    ///
    /// The pair to `next_source`. Neither is a failure: no retry is spent, no
    /// error is recorded, the failover budget is untouched, and clients keep
    /// reading across the change exactly as they do across an automatic
    /// failover.
    pub fn switch_source(&self, channel: Uuid, output: OutputKey, index: usize) -> SwitchOutcome {
        match self.session(channel, output) {
            Some(session) => session.request_source(index),
            None => SwitchOutcome::NoSession,
        }
    }

    /// Advances to the next source in the channel's order, wrapping. What the
    /// Stats page offers for a stream that is technically alive but visibly
    /// bad, where stopping the channel is the only alternative.
    pub fn next_source(&self, channel: Uuid, output: OutputKey) -> SwitchOutcome {
        match self.session(channel, output) {
            Some(session) => session.request_next_source(),
            None => SwitchOutcome::NoSession,
        }
    }

    #[cfg(test)]
    pub fn connections_in_use(&self, account: i64) -> u32 {
        self.inner.limits.in_use(account)
    }

    fn ensure_raw(&self, channel: Uuid, sources: &[StreamSource]) -> Arc<Session> {
        self.ensure((channel, OutputKey::Raw), |session| {
            tokio::spawn(run_upstream(
                session,
                sources.to_vec(),
                self.inner.http.clone(),
                self.inner.limits.clone(),
            ));
        })
    }

    fn ensure_profile(
        &self,
        channel: Uuid,
        id: i64,
        upstream: Arc<Session>,
        transcode: Transcode,
    ) -> Arc<Session> {
        self.ensure((channel, OutputKey::Profile(id)), move |session| {
            tokio::spawn(run_transcode(session, upstream, transcode));
        })
    }

    /// Get or create, replacing a session that has already stopped. `start`
    /// must not touch the registry: it runs while the map shard is locked.
    fn ensure(&self, key: (Uuid, OutputKey), start: impl FnOnce(Arc<Session>)) -> Arc<Session> {
        let create = || {
            let session = Session::new(key.0, key.1, self.inner.cfg.clone());
            start(session.clone());
            tokio::spawn(reap(
                session.clone(),
                Arc::downgrade(&self.inner),
                key,
                self.inner.cfg.clone(),
            ));
            session
        };

        match self.inner.sessions.entry(key) {
            Entry::Occupied(mut occupied) => {
                if occupied.get().is_stopped() {
                    let session = create();
                    occupied.insert(session.clone());
                    session
                } else {
                    occupied.get().clone()
                }
            }
            Entry::Vacant(vacant) => {
                let session = create();
                vacant.insert(session.clone());
                session
            }
        }
    }
}

/// Holds a session open past its last client, then takes it out of the map.
///
/// Channel surfing reconnects within `channel_shutdown_delay` pay no
/// reconnect cost, and a freshly created session gets `channel_client_wait`
/// to acquire its first client before it counts as idle.
async fn reap(
    session: Arc<Session>,
    registry: Weak<Inner>,
    key: (Uuid, OutputKey),
    cfg: Arc<StreamConfig>,
) {
    let mut clients = session.watch_client_count();
    let mut grace = cfg.channel_client_wait;

    loop {
        if session.is_stopped() {
            break;
        }
        let idle = *clients.borrow_and_update() == 0;
        if idle {
            let expired = tokio::select! {
                _ = tokio::time::sleep(grace) => session.client_count() == 0,
                changed = clients.changed() => changed.is_err(),
            };
            if expired {
                break;
            }
        } else {
            grace = cfg.channel_shutdown_delay;
            if clients.changed().await.is_err() {
                break;
            }
        }
    }

    // Stop first, then unmap: a connect racing this sees a stopped session and
    // retries onto a fresh one rather than attaching to a corpse.
    session.stop();
    if let Some(inner) = registry.upgrade() {
        inner
            .sessions
            .remove_if(&key, |_, live| Arc::ptr_eq(live, &session));
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt;

    use super::*;
    use crate::client::ClientStream;
    use crate::input::SourceLimit;
    use crate::session::Phase;
    use crate::synthetic::{
        TsGenerator, blip_script, http_source, is_aligned, ordinals, shell_source, silent_script,
        slow_ts_loop, test_config, ts_file, ts_file_from, ts_loop, vlc_stall_loop, with_limit,
    };

    const PACKETS: usize = 200;

    fn registry(cfg: StreamConfig) -> Registry {
        Registry::new(Arc::new(cfg), reqwest::Client::new())
    }

    fn spec(channel: Uuid, sources: Vec<StreamSource>) -> ChannelSpec {
        ChannelSpec {
            channel,
            sources,
            transcode: None,
        }
    }

    fn stream(connection: Connection) -> ClientStream {
        match connection {
            Connection::Stream(stream) => stream,
            Connection::Redirect { url } => panic!("unexpected redirect to {url}"),
        }
    }

    /// Reads until `packets` payload packets have arrived, so a test never
    /// hangs on a source that stopped producing.
    async fn read_packets(stream: &mut ClientStream, packets: usize) -> Vec<u8> {
        let collect = async {
            let mut out = Vec::new();
            while ordinals(&out).len() < packets {
                match stream.next().await {
                    Some(Ok(data)) => out.extend_from_slice(&data),
                    Some(Err(e)) => panic!("stream error: {e}"),
                    None => break,
                }
            }
            out
        };
        tokio::time::timeout(Duration::from_secs(15), collect)
            .await
            .expect("timed out waiting for packets")
    }

    /// Reads on until a packet from `at_least` onwards arrives. The ring still
    /// holds seconds of the previous source when a switch lands, and a viewer
    /// watches through that backlog rather than skipping it -- so a test that
    /// only reads the next few packets is still looking at the old source.
    ///
    /// Returns false if the stream ends first, which is what a disconnected
    /// client looks like from here.
    async fn read_until_ordinal(stream: &mut ClientStream, at_least: u64) -> bool {
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(data) if ordinals(&data).iter().any(|&o| o >= at_least) => return true,
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }

    async fn eventually(mut done: impl FnMut() -> bool) {
        let poll = async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(15), poll)
            .await
            .expect("condition never held");
    }

    #[tokio::test]
    async fn serves_a_channel_from_a_spawned_command() {
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, vec![shell_source(1, &script)]),
                    ClientDescriptor {
                        ip: Some("203.0.113.9".parse().expect("ip")),
                        user_agent: Some("Plex/1.0".to_owned()),
                    },
                )
                .expect("connected"),
        );

        let data = read_packets(&mut client, 40).await;
        assert!(is_aligned(&data));
        assert_eq!(ordinals(&data)[..8], [0, 1, 2, 3, 4, 5, 6, 7]);

        let stats = registry.stats();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].source_id, Some(1));
        // The id the server logs must be the one the stats payload carries.
        assert_eq!(stats[0].clients[0].id, client.id());
        assert_eq!(client.id().to_string(), stats[0].clients[0].id.to_string());
        assert_eq!(stats[0].phase, Phase::Streaming);
        assert_eq!(stats[0].clients.len(), 1);
        assert_eq!(stats[0].clients[0].user_agent.as_deref(), Some("Plex/1.0"));
        assert!(stats[0].clients[0].bytes_sent > 0);
    }

    #[tokio::test]
    async fn a_second_client_shares_the_session_rather_than_opening_another() {
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();
        let sources = vec![shell_source(1, &script)];

        let mut first = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources.clone()),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut first, 8).await;

        let mut second = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut second, 8).await;

        let stats = registry.stats();
        assert_eq!(stats.len(), 1, "one session for one channel");
        assert_eq!(stats[0].clients.len(), 2);
    }

    #[tokio::test]
    async fn fails_over_through_the_source_list_in_order() {
        let file = ts_file(PACKETS);
        let good = ts_loop(&file);
        let registry = registry(test_config());

        let sources = vec![
            shell_source(10, "exit 7"),
            shell_source(11, "exit 7"),
            shell_source(12, &good),
        ];
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(Uuid::new_v4(), sources),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        // The client survives both switches: it was never disconnected, it
        // just waited at the head of a ring that had no data yet.
        let data = read_packets(&mut client, 20).await;
        assert_eq!(ordinals(&data)[0], 0);

        let stats = registry.stats();
        assert_eq!(stats[0].source_id, Some(12));
        assert_eq!(stats[0].source_index, 2);
        assert_eq!(stats[0].switches, 2);
    }

    #[tokio::test]
    async fn a_client_survives_the_url_swap_underneath_it() {
        // The first source hands over a fixed 40 packets and exits; the second
        // is a live stream whose ordinals start at 1000, so the seam between
        // them is visible and can be checked for loss and duplication rather
        // than only for "bytes kept arriving".
        let first = ts_file_from(0, 40);
        let second = ts_file_from(1000, PACKETS);
        let dies = format!(
            "cat {}",
            shell_words::quote(&first.path().to_string_lossy())
        );

        let registry = registry(test_config());
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        Uuid::new_v4(),
                        vec![shell_source(1, &dies), shell_source(2, &ts_loop(&second))],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        let data = read_packets(&mut client, 100).await;
        assert!(is_aligned(&data));
        let seen = ordinals(&data);

        let seam = seen
            .iter()
            .position(|&o| o >= 1000)
            .expect("never reached the second source");
        assert_eq!(
            seen[..seam],
            (0..40).collect::<Vec<_>>()[..],
            "the first source's packets were lost or duplicated"
        );
        assert_eq!(seen[seam], 1000, "the second source lost its opening");
        for pair in seen[seam..].windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "gap or duplicate across the splice: {seen:?}"
            );
        }

        assert_eq!(registry.stats()[0].source_id, Some(2));
        assert_eq!(registry.stats()[0].switches, 1);
    }

    #[tokio::test]
    async fn an_encoder_that_keeps_exiting_does_not_take_the_channel_down() {
        let file = ts_file(PACKETS);
        let cfg = test_config();
        // Zero is the default, and a raw session left momentarily clientless
        // is unmapped immediately at that setting, so an encoder respawn must
        // not leave one.
        assert_eq!(cfg.channel_shutdown_delay, Duration::ZERO);

        let registry = registry(cfg);
        let channel = Uuid::new_v4();
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Profile(9),
                    ChannelSpec {
                        channel,
                        sources: vec![shell_source(1, &ts_loop(&file))],
                        // Copies its input and exits after a few chunks,
                        // forcing repeated respawns.
                        transcode: Some(Transcode {
                            command: "sh".to_owned(),
                            parameters: "-c 'head -c 5000'".to_owned(),
                        }),
                    },
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        read_packets(&mut client, 20).await;
        let raw = registry.session(channel, OutputKey::Raw).expect("raw ring");

        // Far more than one encoder's worth, so several respawns must have
        // happened while the client kept reading.
        let data = read_packets(&mut client, 150).await;
        assert!(is_aligned(&data));

        let survivor = registry
            .session(channel, OutputKey::Raw)
            .expect("raw ring was reaped mid-respawn");
        assert!(
            Arc::ptr_eq(&raw, &survivor),
            "the raw session was torn down and rebuilt"
        );
        assert_eq!(survivor.client_count(), 1, "the transcode lost its cursor");
        assert!(
            registry
                .session(channel, OutputKey::Profile(9))
                .is_some_and(|s| !s.is_stopped())
        );
    }

    #[tokio::test]
    async fn an_exhausted_source_list_wraps_after_the_cooldown() {
        let file = ts_file(40);
        let dies = format!("cat {}", shell_words::quote(&file.path().to_string_lossy()));

        let registry = registry(test_config());
        let channel = Uuid::new_v4();
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        channel,
                        vec![shell_source(1, &dies), shell_source(2, "exit 1")],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        // Source 1 worked once, so the list wraps back to it rather than
        // giving up the way a cold start would.
        eventually(|| {
            registry
                .session(channel, OutputKey::Raw)
                .is_some_and(|s| s.stats().switches >= 2 && s.stats().source_id == Some(1))
        })
        .await;
    }

    #[tokio::test]
    async fn a_lone_source_blipping_does_not_cost_the_viewer_the_channel() {
        // Most channels have exactly one source, and a
        // provider dropping for a few seconds is ordinary. The viewer must
        // still be watching when it comes back -- which they are not if the
        // engine gives up on a counter.
        let before = ts_file_from(0, 40);
        let after = ts_file_from(1000, PACKETS);
        let mut counter = tempfile::NamedTempFile::new().expect("counter");
        std::io::Write::write_all(&mut counter, b"0\n").expect("seed the counter");
        let script = blip_script(&before, &after, counter.path(), 4);

        let registry = registry(test_config());
        let channel = Uuid::new_v4();
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, vec![shell_source(1, &script)]),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        // Packets from after the outage, on the same client that was watching
        // before it.
        let data = read_packets(&mut client, 80).await;
        let seen = ordinals(&data);
        assert_eq!(seen[0], 0, "missed the pre-outage stream");
        assert!(
            seen.iter().any(|&o| o >= 1000),
            "the viewer never saw the source come back: {:?}",
            &seen[..seen.len().min(8)]
        );

        let stats = registry.stats();
        assert_eq!(stats[0].clients.len(), 1, "the viewer was disconnected");
        assert!(stats[0].healthy);
    }

    #[tokio::test]
    async fn a_source_that_answers_and_then_says_nothing_is_moved_on_from() {
        // A provider that accepts the connection and sends no bytes looks
        // healthy to everything except a clock. Without the read timeout the
        // channel would sit on it forever with a live-looking session.
        let file = ts_file(PACKETS);
        // A client's keepalive cap has to outlast the engine's own attempt at
        // the silent source, or the viewer is dropped before the failover it
        // was waiting for even happens. In production that is 300s against a
        // 10s connection timeout; the test config's ratio is inverted, so it
        // is corrected here rather than the engine being bent around it.
        let mut cfg = test_config();
        cfg.max_keepalive = Duration::from_secs(5);

        let registry = registry(cfg);
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        Uuid::new_v4(),
                        vec![
                            shell_source(1, &silent_script()),
                            shell_source(2, &ts_loop(&file)),
                        ],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        assert!(is_aligned(&read_packets(&mut client, 20).await));
        let stats = registry.stats();
        assert_eq!(stats[0].source_id, Some(2));
    }

    #[tokio::test]
    async fn a_list_that_flaps_below_the_stability_threshold_keeps_rotating() {
        // Every source dies too quickly to count as stable, so nothing ever
        // clears the failure bookkeeping. With a switch ceiling this is where
        // the channel died; without one it just keeps going round, which is
        // what a viewer wants from a provider having a bad ten minutes.
        let file = ts_file(40);
        let brief = format!("cat {}", shell_words::quote(&file.path().to_string_lossy()));
        let mut cfg = test_config();
        cfg.failover.rotation_cooldown_base = Duration::from_millis(20);
        cfg.failover.rotation_cooldown_max = Duration::from_millis(20);

        let registry = registry(cfg);
        let channel = Uuid::new_v4();
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        channel,
                        vec![
                            shell_source(1, &brief),
                            shell_source(2, &brief),
                            shell_source(3, &brief),
                        ],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        // Forty packets per run, so reaching this count takes more switches
        // than any fixed ceiling would allow.
        read_packets(&mut client, 800).await;
        let stats = registry.stats();
        assert!(
            stats[0].switches > 14,
            "only {} switches: the rotation stopped early",
            stats[0].switches
        );
        assert_eq!(stats[0].clients.len(), 1, "the viewer was disconnected");
    }

    #[tokio::test]
    async fn a_chosen_source_is_switched_to_without_it_counting_as_a_failure() {
        // A viewer watching something technically alive but visibly bad, whose
        // only other option is stopping the channel for everyone.
        let first = ts_file_from(0, PACKETS);
        let third = ts_file_from(5000, PACKETS);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        channel,
                        vec![
                            shell_source(1, &ts_loop(&first)),
                            shell_source(2, "exit 1"),
                            shell_source(3, &ts_loop(&third)),
                        ],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        let before = registry.stats().remove(0);
        assert_eq!(before.source_id, Some(1));

        assert_eq!(
            registry.switch_source(channel, OutputKey::Raw, 2),
            SwitchOutcome::Switched(2)
        );

        // The same client, never disconnected, reads through the backlog and
        // out the other side onto the source that was chosen.
        assert!(
            read_until_ordinal(&mut client, 5000).await,
            "the viewer never reached the source they chose"
        );

        let after = registry.stats().remove(0);
        assert_eq!(after.source_id, Some(3));
        assert_eq!(after.clients.len(), 1, "the viewer was disconnected");
        // A preference is not an error, and must not be charged as one.
        assert_eq!(after.switches, before.switches, "a choice spent a failover");
        assert_eq!(
            after.last_error, before.last_error,
            "a choice logged an error"
        );
    }

    #[tokio::test]
    async fn a_choice_made_during_a_rotation_cooldown_is_not_left_waiting() {
        // The cooldown is armed exactly when every source has failed a pass,
        // which is exactly when somebody reaches for "choose source". A
        // request answered as a success and then sat on for the length of the
        // cooldown is worse than one refused.
        let before = ts_file_from(0, 40);
        let after = ts_file_from(5000, PACKETS);
        let mut counter = tempfile::NamedTempFile::new().expect("counter");
        std::io::Write::write_all(&mut counter, b"0\n").expect("seed the counter");

        let mut cfg = test_config();
        // Long enough that sleeping through it fails this test rather than
        // slowing it down.
        cfg.failover.rotation_cooldown_base = Duration::from_secs(30);
        cfg.failover.rotation_cooldown_max = Duration::from_secs(30);
        cfg.max_keepalive = Duration::from_secs(30);

        let registry = registry(cfg);
        let channel = Uuid::new_v4();
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        channel,
                        vec![
                            // Serves once -- enough to count as a working
                            // channel -- and refuses until chosen again.
                            shell_source(1, &blip_script(&before, &after, counter.path(), 0)),
                            shell_source(2, "exit 1"),
                            shell_source(3, "exit 1"),
                        ],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        // Every source has now failed a pass, so the wrap cooldown is armed.
        eventually(|| registry.stats()[0].phase == Phase::Switching).await;

        assert_eq!(
            registry.switch_source(channel, OutputKey::Raw, 0),
            SwitchOutcome::Switched(0)
        );
        assert!(
            read_until_ordinal(&mut client, 5000).await,
            "the choice was reported as a success and then sat on"
        );
    }

    #[tokio::test]
    async fn next_source_advances_and_wraps() {
        let file = ts_file(PACKETS);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();
        let sources = vec![
            shell_source(1, &ts_loop(&file)),
            shell_source(2, &ts_loop(&file)),
        ];

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;
        assert_eq!(registry.stats()[0].source_id, Some(1));

        assert_eq!(
            registry.next_source(channel, OutputKey::Raw),
            SwitchOutcome::Switched(1)
        );
        eventually(|| registry.stats()[0].source_id == Some(2)).await;

        // And round again.
        assert_eq!(
            registry.next_source(channel, OutputKey::Raw),
            SwitchOutcome::Switched(0)
        );
        eventually(|| registry.stats()[0].source_id == Some(1)).await;
        assert_eq!(registry.stats()[0].clients.len(), 1);
    }

    #[tokio::test]
    async fn a_switch_request_answers_honestly_when_it_cannot_be_served() {
        let file = ts_file(PACKETS);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        assert_eq!(
            registry.switch_source(Uuid::new_v4(), OutputKey::Raw, 0),
            SwitchOutcome::NoSession
        );

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, vec![shell_source(1, &ts_loop(&file))]),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        assert_eq!(
            registry.switch_source(channel, OutputKey::Raw, 4),
            SwitchOutcome::OutOfRange { sources: 1 }
        );
        // A profile reads the raw session's ring and has no source list of its
        // own, which the answer says rather than pretending otherwise.
        assert_eq!(
            registry.next_source(channel, OutputKey::Profile(1)),
            SwitchOutcome::NoSession
        );
    }

    #[tokio::test]
    async fn a_dead_source_list_gives_up_and_the_session_is_reaped() {
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        channel,
                        vec![shell_source(1, "exit 1"), shell_source(2, "exit 1")],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        // Nothing ever arrives, and the client is released rather than held on
        // keepalives forever.
        let tail = tokio::time::timeout(Duration::from_secs(15), async {
            while client.next().await.is_some() {}
        })
        .await;
        assert!(tail.is_ok(), "client was never released");

        eventually(|| registry.session(channel, OutputKey::Raw).is_none()).await;
    }

    #[tokio::test]
    async fn buffering_past_the_timeout_switches_source() {
        let file = ts_file(PACKETS);
        // Produces data, so it is healthy, but reports half real-time speed
        // for longer than buffering_timeout.
        let slow = slow_ts_loop(&file);
        let good = ts_loop(&file);

        let registry = registry(test_config());
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        Uuid::new_v4(),
                        vec![shell_source(1, &slow), shell_source(2, &good)],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        eventually(|| registry.stats()[0].source_id == Some(2)).await;
        assert_eq!(registry.stats()[0].switches, 1);
    }

    #[tokio::test]
    async fn a_vlc_profile_stalling_switches_source_too() {
        // VLC never prints a speed, so without its own buffering messages this
        // falls back to stream_timeout alone -- and a stalled but not dead
        // source holds viewers through the whole dead-air window instead of
        // switching.
        let file = ts_file(PACKETS);
        let registry = registry(test_config());
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(
                        Uuid::new_v4(),
                        vec![
                            shell_source(1, &vlc_stall_loop(&file)),
                            shell_source(2, &ts_loop(&file)),
                        ],
                    ),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        eventually(|| registry.stats()[0].source_id == Some(2)).await;
        assert_eq!(registry.stats()[0].switches, 1);
    }

    #[tokio::test]
    async fn an_output_profile_is_a_ring_downstream_of_a_ring() {
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Profile(3),
                    ChannelSpec {
                        channel,
                        sources: vec![shell_source(1, &script)],
                        // `cat` is a transcode that changes nothing, which is
                        // what makes the bytes assertable.
                        transcode: Some(Transcode {
                            command: "cat".to_owned(),
                            parameters: String::new(),
                        }),
                    },
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        let data = read_packets(&mut client, 40).await;
        assert!(is_aligned(&data));
        assert_eq!(ordinals(&data)[..8], [0, 1, 2, 3, 4, 5, 6, 7]);

        // Both rings exist, and the transcode is a client of the raw one.
        assert!(registry.session(channel, OutputKey::Raw).is_some());
        assert!(registry.session(channel, OutputKey::Profile(3)).is_some());
        let raw = registry.session(channel, OutputKey::Raw).expect("raw");
        assert_eq!(raw.client_count(), 1);
        assert_eq!(registry.stats().len(), 2);

        // Dropping the profile's only client cascades: the transcode releases
        // the raw session, which then has no clients of its own.
        drop(client);
        eventually(|| registry.session(channel, OutputKey::Profile(3)).is_none()).await;
        eventually(|| registry.session(channel, OutputKey::Raw).is_none()).await;
    }

    #[tokio::test]
    async fn the_session_outlives_the_last_client_by_the_shutdown_delay() {
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let mut cfg = test_config();
        cfg.channel_shutdown_delay = Duration::from_millis(700);
        let registry = registry(cfg);
        let channel = Uuid::new_v4();
        let sources = vec![shell_source(1, &script)];

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources.clone()),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;
        let first_start = registry.stats()[0].started_at;
        drop(client);

        // Still alive immediately after the last client left.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let session = registry
            .session(channel, OutputKey::Raw)
            .expect("held open");
        assert!(!session.is_stopped());

        // Reconnecting inside the window rejoins the same session.
        let mut again = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut again, 4).await;
        assert_eq!(registry.stats()[0].started_at, first_start);

        drop(again);
        eventually(|| registry.session(channel, OutputKey::Raw).is_none()).await;
    }

    #[tokio::test]
    async fn proxies_an_http_source_and_fails_over_between_them() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = TsGenerator::new().bytes(PACKETS);

        Mock::given(method("GET"))
            .and(path("/dead.ts"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/live.ts"))
            // The provider's own user agent has to survive to the wire, or
            // some providers simply refuse the stream.
            .and(header("user-agent", "dollet-test/1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;

        let registry = registry(test_config());
        let sources = vec![
            http_source(1, &format!("{}/dead.ts", server.uri())),
            http_source(2, &format!("{}/live.ts", server.uri())),
        ];
        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(Uuid::new_v4(), sources),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );

        let data = read_packets(&mut client, 40).await;
        let stats = registry.stats();

        assert!(is_aligned(&data));
        assert_eq!(ordinals(&data)[..8], [0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(stats[0].source_id, Some(2));
        assert_eq!(stats[0].switches, 1);
    }

    #[tokio::test]
    async fn a_redirect_profile_never_opens_a_session() {
        let registry = registry(test_config());
        let channel = Uuid::new_v4();
        let source = StreamSource {
            id: 1,
            url: "http://provider.invalid/live.ts".to_owned(),
            user_agent: "dollet-relay".to_owned(),
            profile: SourceProfile::Redirect,
            limit: None,
        };

        match registry
            .connect(
                OutputKey::Raw,
                spec(channel, vec![source]),
                ClientDescriptor::default(),
            )
            .expect("connected")
        {
            Connection::Redirect { url } => assert_eq!(url, "http://provider.invalid/live.ts"),
            Connection::Stream(_) => panic!("should not have proxied"),
        }
        assert!(registry.session(channel, OutputKey::Raw).is_none());
    }

    #[tokio::test]
    async fn a_source_at_its_account_limit_fails_over_to_one_that_is_not() {
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let registry = registry(test_config());

        let busy = with_limit(shell_source(1, &script), 42, 1);
        let spare = shell_source(2, &script);

        let mut first = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(Uuid::new_v4(), vec![busy.clone()]),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut first, 8).await;
        assert_eq!(registry.connections_in_use(42), 1);

        // A different channel wanting the same account is pushed onto the
        // alternate source rather than stealing the slot.
        let mut second = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(Uuid::new_v4(), vec![busy, spare]),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut second, 8).await;

        let other = registry
            .stats()
            .into_iter()
            .find(|s| s.source_id == Some(2))
            .expect("switched to the unlimited source");
        assert_eq!(other.switches, 1);
        assert_eq!(registry.connections_in_use(42), 1);
    }

    #[tokio::test]
    async fn one_client_can_be_disconnected_without_disturbing_the_channel() {
        let file = ts_file(PACKETS);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();
        let sources = vec![shell_source(1, &ts_loop(&file))];

        let mut evicted = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources.clone()),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        let mut kept = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, sources),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut evicted, 8).await;
        read_packets(&mut kept, 8).await;

        let id = evicted.id();
        assert!(registry.disconnect_client(channel, id));

        let released = tokio::time::timeout(Duration::from_secs(10), async {
            while evicted.next().await.is_some() {}
        })
        .await;
        assert!(
            released.is_ok(),
            "the disconnected client was never released"
        );

        // Nobody else notices: the other client keeps reading, and none of
        // this reached the failover bookkeeping.
        assert!(is_aligned(&read_packets(&mut kept, 40).await));

        let stats = registry.stats();
        assert_eq!(stats.len(), 1, "the channel was taken down with the client");
        assert_eq!(stats[0].clients.len(), 1);
        assert_ne!(stats[0].clients[0].id, id);
        assert_eq!(stats[0].phase, Phase::Streaming);
        assert!(stats[0].healthy);
        assert_eq!(stats[0].switches, 0, "an eviction spent a failover switch");
        assert_eq!(
            stats[0].last_error, None,
            "an eviction was recorded as a stream failure"
        );

        // A stale row answers 404 rather than a silent success.
        assert!(!registry.disconnect_client(channel, id));
    }

    #[tokio::test]
    async fn disconnecting_the_last_client_hands_over_to_the_reaper() {
        let file = ts_file(PACKETS);
        let mut cfg = test_config();
        cfg.channel_shutdown_delay = Duration::from_millis(700);
        let registry = registry(cfg);
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Raw,
                    spec(channel, vec![shell_source(1, &ts_loop(&file))]),
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;
        assert!(registry.disconnect_client(channel, client.id()));

        // The shutdown delay applies exactly as it would to a player that hung
        // up: an eviction is not a second teardown path.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let session = registry
            .session(channel, OutputKey::Raw)
            .expect("held open");
        assert!(!session.is_stopped());
        assert_eq!(session.client_count(), 0);

        eventually(|| registry.session(channel, OutputKey::Raw).is_none()).await;
    }

    #[tokio::test]
    async fn stop_all_ends_every_open_client_stream() {
        // `dollet-server` holds its registry in a `static`, which is never
        // dropped, so `Drop` never runs and a graceful shutdown would wait on
        // bodies that by construction never end.
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let registry = registry(test_config());

        // Two clients on one channel and a third on another, so "every" means
        // across sessions as well as within one.
        let shared = Uuid::new_v4();
        let mut clients = Vec::new();
        for channel in [shared, shared, Uuid::new_v4()] {
            let mut client = stream(
                registry
                    .connect(
                        OutputKey::Raw,
                        spec(channel, vec![shell_source(1, &script)]),
                        ClientDescriptor::default(),
                    )
                    .expect("connected"),
            );
            read_packets(&mut client, 8).await;
            clients.push(client);
        }
        assert_eq!(registry.stats().len(), 2);

        registry.stop_all();

        for (index, mut client) in clients.into_iter().enumerate() {
            let drained = tokio::time::timeout(Duration::from_secs(15), async {
                while client.next().await.is_some() {}
            })
            .await;
            assert!(drained.is_ok(), "client {index} never reached the end");
        }
    }

    #[tokio::test]
    async fn the_transcode_consumer_is_not_disconnectable() {
        let file = ts_file(PACKETS);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Profile(2),
                    ChannelSpec {
                        channel,
                        sources: vec![shell_source(1, &ts_loop(&file))],
                        transcode: Some(Transcode {
                            command: "cat".to_owned(),
                            parameters: String::new(),
                        }),
                    },
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        let raw = registry.session(channel, OutputKey::Raw).expect("raw ring");
        let consumer = raw.stats().clients.first().cloned().expect("the transcode");
        assert!(consumer.internal, "the transcode is not flagged for the UI");

        // Evicting it would strand the encoder with no input for the rest of
        // the session's life, so it is refused and the route 404s.
        assert!(!registry.disconnect_client(channel, consumer.id));
        assert!(is_aligned(&read_packets(&mut client, 40).await));
        assert_eq!(raw.client_count(), 1);
    }

    #[tokio::test]
    async fn stopping_a_channel_takes_every_output_with_it() {
        let file = ts_file(PACKETS);
        let script = ts_loop(&file);
        let registry = registry(test_config());
        let channel = Uuid::new_v4();

        let mut client = stream(
            registry
                .connect(
                    OutputKey::Profile(1),
                    ChannelSpec {
                        channel,
                        sources: vec![shell_source(1, &script)],
                        transcode: Some(Transcode {
                            command: "cat".to_owned(),
                            parameters: String::new(),
                        }),
                    },
                    ClientDescriptor::default(),
                )
                .expect("connected"),
        );
        read_packets(&mut client, 8).await;

        registry.stop_channel(channel);
        let tail = tokio::time::timeout(Duration::from_secs(10), async {
            while client.next().await.is_some() {}
        })
        .await;
        assert!(tail.is_ok(), "clients were not released");
    }

    #[tokio::test]
    async fn rejects_a_spec_it_cannot_serve() {
        let registry = registry(test_config());

        let empty = registry.connect(
            OutputKey::Raw,
            spec(Uuid::new_v4(), Vec::new()),
            ClientDescriptor::default(),
        );
        assert!(matches!(empty, Err(StreamError::NoSources)));

        let no_transcode = registry.connect(
            OutputKey::Profile(1),
            spec(Uuid::new_v4(), vec![shell_source(1, "exit 0")]),
            ClientDescriptor::default(),
        );
        assert!(matches!(no_transcode, Err(StreamError::MissingTranscode)));
    }

    #[tokio::test]
    async fn a_limit_key_with_no_session_reports_nothing_in_use() {
        let registry = registry(test_config());
        assert_eq!(registry.connections_in_use(99), 0);
        assert!(registry.stats().is_empty());
        let _ = SourceLimit {
            key: 1,
            max_streams: 1,
        };
    }
}
