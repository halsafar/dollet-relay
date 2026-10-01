use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::Stream;
use tokio::sync::watch;
use tokio::time::Instant;
use uuid::Uuid;

use crate::StreamError;
use crate::config::StreamConfig;
use crate::ring::{Fetched, Ring};
use crate::session::Session;
use crate::ts::null_packet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub struct ClientId(Uuid);

/// The inverse of `Display`. Production never needs it — a client is found by
/// scanning the live list, which answers "is this row still real" at the same
/// time — but a test building a fixture with a fixed id does.
impl std::str::FromStr for ClientId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What `dollet-server` knows about the requester, for the Stats page.
#[derive(Debug, Clone, Default)]
pub struct ClientDescriptor {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ClientStats {
    pub id: ClientId,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub connected_at: DateTime<Utc>,
    pub bytes_sent: u64,
    /// A consumer inside this process rather than someone watching: the
    /// transcode behind an output profile. It has no address to show and
    /// cannot be disconnected, so the Stats page should render it as the
    /// explanation for why a channel with no viewers is still up, not as a
    /// row with an action on it.
    pub internal: bool,
}

pub(crate) struct ClientRecord {
    pub id: ClientId,
    ip: Option<IpAddr>,
    user_agent: Option<String>,
    connected_at: DateTime<Utc>,
    bytes_sent: AtomicU64,
    internal: bool,
    evicted: AtomicBool,
}

impl ClientRecord {
    pub(crate) fn new(desc: ClientDescriptor, internal: bool) -> Self {
        Self {
            id: ClientId(Uuid::new_v4()),
            ip: desc.ip,
            user_agent: desc.user_agent,
            connected_at: Utc::now(),
            bytes_sent: AtomicU64::new(0),
            internal,
            evicted: AtomicBool::new(false),
        }
    }

    pub(crate) fn stats(&self) -> ClientStats {
        ClientStats {
            id: self.id,
            ip: self.ip,
            user_agent: self.user_agent.clone(),
            connected_at: self.connected_at,
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            internal: self.internal,
        }
    }

    /// Marks the client for eviction. Refused for an internal consumer: the
    /// transcode's cursor is the only thing feeding an encoder, and ending it
    /// would strand that encoder with no input for the rest of the session.
    pub(crate) fn evict(&self) -> bool {
        if self.internal {
            return false;
        }
        self.evicted.store(true, Ordering::Release);
        true
    }

    fn is_evicted(&self) -> bool {
        self.evicted.load(Ordering::Acquire)
    }
}

/// A client is an index into the ring plus a wakeup. Nothing more: a recorder
/// that writes to a file is the same thing with a different sink, which is why
/// this is the shape rather than something HTTP-flavoured.
///
/// Falling behind is the client's own problem by construction — a stalled
/// cursor holds nothing but itself, and when the ring rolls past it the cursor
/// is moved forward rather than the producer being slowed down.
pub(crate) struct Cursor {
    session: Arc<Session>,
    record: Arc<ClientRecord>,
    cfg: Arc<StreamConfig>,
    ring: Arc<Ring>,
    head: watch::Receiver<u64>,
    at: u64,
    keepalive_since: Option<Instant>,
    cap_keepalive: bool,
}

impl Cursor {
    pub(crate) fn new(
        session: Arc<Session>,
        record: Arc<ClientRecord>,
        cfg: Arc<StreamConfig>,
        ring: Arc<Ring>,
        head: watch::Receiver<u64>,
        at: u64,
    ) -> Self {
        Self {
            session,
            record,
            cfg,
            ring,
            head,
            at,
            keepalive_since: None,
            cap_keepalive: true,
        }
    }

    /// See `Session::add_internal_client`.
    pub(crate) fn disable_keepalive_cap(&mut self) {
        self.cap_keepalive = false;
    }

    pub(crate) async fn next_chunk(&mut self) -> Option<Bytes> {
        let cfg = self.cfg.clone();
        loop {
            // Unlike a session stop, an eviction is immediate and is checked
            // before the ring: the operator has decided to close this socket,
            // so there is no tail worth spending time writing into it.
            if self.record.is_evicted() {
                tracing::info!(client = %self.record.id, "client disconnected by request");
                return None;
            }

            // A stop is checked only once the cursor has reached the head, so
            // a client is handed the tail already sitting in the ring rather
            // than losing it. That tail is several seconds of video, and it is
            // exactly what covers the gap while a player reconnects.
            match self.ring.fetch(self.at) {
                Fetched::Chunk { data, next } => {
                    self.at = next;
                    self.keepalive_since = None;
                    self.record
                        .bytes_sent
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                    return Some(data);
                }
                Fetched::Skipped { to } => {
                    tracing::debug!(
                        client = %self.record.id,
                        from = self.at,
                        to,
                        "client fell out of the retention window, skipping forward"
                    );
                    self.at = to;
                }
                Fetched::AtHead => {
                    if self.session.is_stopped() {
                        return None;
                    }
                    match tokio::time::timeout(cfg.keepalive_interval, self.head.changed()).await {
                        // Producer published, or the session poked us to notice
                        // it has stopped.
                        Ok(Ok(())) => {}
                        // The session is gone.
                        Ok(Err(_)) => return None,
                        Err(_) => {
                            if self.session.is_healthy() {
                                continue;
                            }
                            let now = Instant::now();
                            let since = *self.keepalive_since.get_or_insert(now);
                            if self.cap_keepalive
                                && now.saturating_duration_since(since) > cfg.max_keepalive
                            {
                                tracing::info!(
                                    client = %self.record.id,
                                    "keepalive cap reached with no recovery, disconnecting"
                                );
                                return None;
                            }
                            let packet = null_packet();
                            self.record
                                .bytes_sent
                                .fetch_add(packet.len() as u64, Ordering::Relaxed);
                            return Some(packet);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn id(&self) -> ClientId {
        self.record.id
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        self.session.remove_client(self.record.id);
    }
}

/// What `dollet-server` hands to `axum::body::Body::from_stream`. A concrete type
/// rather than `impl Stream` so it can sit in `Connection`, and boxed so this
/// crate exposes no axum or hyper types.
pub struct ClientStream {
    id: ClientId,
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, StreamError>> + Send>>,
}

impl ClientStream {
    pub(crate) fn new(cursor: Cursor) -> Self {
        let id = cursor.id();
        let inner = futures_util::stream::unfold(cursor, |mut cursor| async move {
            cursor.next_chunk().await.map(|data| (Ok(data), cursor))
        });
        Self {
            id,
            inner: Box::pin(inner),
        }
    }

    pub fn id(&self) -> ClientId {
        self.id
    }
}

impl Stream for ClientStream {
    type Item = Result<Bytes, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `axum::body::Body::from_stream` needs both bounds, and this crate
    /// deliberately cannot depend on axum to find out the hard way.
    #[test]
    fn a_client_id_survives_the_trip_through_a_url() {
        let id = ClientId(Uuid::new_v4());
        let parsed: ClientId = id.to_string().parse().expect("round trip");
        assert_eq!(parsed, id);
        assert!("not-a-uuid".parse::<ClientId>().is_err());
    }

    #[test]
    fn the_client_stream_is_what_a_response_body_needs() {
        fn assert_body_shape<S: Stream<Item = Result<Bytes, StreamError>> + Send + 'static>() {}
        assert_body_shape::<ClientStream>();
    }
}
