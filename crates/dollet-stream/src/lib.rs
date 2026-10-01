//! Live streaming engine.
//!
//! This crate must not depend on `dollet-core` or on any database. Its whole
//! input is an ordered list of upstream URLs, a user agent, a command
//! template, and tuning constants; resolving a channel UUID to that list is
//! `dollet-server`'s job. That boundary is what makes the engine testable
//! against a synthetic transport stream with nothing else built.
//!
//! Shape decisions, not to be redesigned without saying so:
//!
//! - the registry is keyed by `(channel, OutputKey)`, not by channel — an
//!   output profile is a ring downstream of a ring
//! - the ring is the source of truth; `watch` is only a wakeup, because
//!   `broadcast` cannot serve a client that starts N seconds behind live
//! - the `watch` sender is owned by the session, not the input task, so
//!   clients survive a failover
//! - never hold a buffer guard across an `.await`
//! - reset buffer position on stream switch, or a partial packet from the old
//!   process gets concatenated onto the new one and breaks decoder sync
//!
//! # Using it from `dollet-server`
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use dollet_stream::*;
//! # async fn example(http: reqwest::Client, sources: Vec<StreamSource>) {
//! let registry = Registry::new(Arc::new(StreamConfig::default()), http);
//! let spec = ChannelSpec {
//!     channel: uuid::Uuid::new_v4(),
//!     sources,
//!     transcode: None,
//! };
//! match registry.connect(OutputKey::Raw, spec, ClientDescriptor::default()) {
//!     Ok(Connection::Stream(body)) => { /* axum::body::Body::from_stream(body) */ }
//!     Ok(Connection::Redirect { url }) => { /* 302 to `url` */ }
//!     Err(e) => tracing::warn!(%e, "stream refused"),
//! }
//! # }
//! ```

mod client;
mod config;
mod failover;
mod input;
mod limits;
mod logs;
mod registry;
mod ring;
mod session;
mod ts;

#[cfg(test)]
mod soak;
#[cfg(test)]
mod synthetic;

pub use client::{ClientDescriptor, ClientId, ClientStats, ClientStream};
pub use config::{FailoverConfig, RING_SIZING_BITRATE, StreamConfig, ring_bytes_for};
pub use input::{SourceLimit, SourceProfile, StreamSource, Transcode};
pub use logs::{MediaInfo, Pace, Progress};
pub use registry::{ChannelSpec, Connection, Registry};
pub use ring::RingStats;
pub use session::{Phase, Session, SessionStats, SwitchOutcome};

/// Which byte stream a client is reading: the channel as received, or the
/// output of a transcode applied to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputKey {
    Raw,
    Profile(i64),
}

/// Serialized as `{"kind": "raw" | "profile", "profile_id": null | n}` rather
/// than as whatever shape the enum happens to have. The derived form is
/// `"Raw"` or `{"Profile": 3}`, which makes a consumer branch on the *shape*
/// of the value and leaks the variant names as API. A newtype variant cannot
/// be internally tagged, so the wire form is written out here instead of
/// bending the type to suit a derive.
impl serde::Serialize for OutputKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Wire {
            kind: &'static str,
            profile_id: Option<i64>,
        }
        let wire = match self {
            OutputKey::Raw => Wire {
                kind: "raw",
                profile_id: None,
            },
            OutputKey::Profile(id) => Wire {
                kind: "profile",
                profile_id: Some(*id),
            },
        };
        wire.serialize(serializer)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("channel has no upstream sources")]
    NoSources,
    #[error("an output profile session needs a transcode command")]
    MissingTranscode,
    #[error("command template is not parseable: {0}")]
    BadCommand(String),
    #[error("upstream failed: {0}")]
    Upstream(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_key_names_itself_on_the_wire() {
        // A consumer reads a field rather than branching on the shape of the
        // value, and the Rust variant names are not API.
        assert_eq!(
            serde_json::to_string(&OutputKey::Raw).expect("serialize"),
            r#"{"kind":"raw","profile_id":null}"#
        );
        assert_eq!(
            serde_json::to_string(&OutputKey::Profile(3)).expect("serialize"),
            r#"{"kind":"profile","profile_id":3}"#
        );
    }
}
