use std::time::Duration;

/// Engine tuning.
///
/// `dollet-server` builds this from the stored `ProxySettings`. Nothing here is
/// read from a database by this crate.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Bytes accumulated before a chunk is published to the ring. `188 * 1361`
    /// is 1361 transport-stream packets; at 8 Mbps that is ~4 publishes/second,
    /// which is also the ring's write-lock rate.
    pub chunk_size: usize,

    /// Retention window. 90 s would be ~90 MB per ring at 8 Mbps and blow the
    /// project's memory target on one channel, so the default is 15 s: enough
    /// for a client joining a few seconds behind live.
    pub ring_duration: Duration,

    /// Hard ceiling regardless of `ring_duration`, so an unexpectedly
    /// high-bitrate source cannot grow a ring without bound. Derived from the
    /// duration with `ring_bytes_for`, because a cap picked independently
    /// silently shortens the retention window on any source above it.
    pub ring_max_bytes: usize,

    /// New clients start this far behind live. Starting exactly at the head
    /// leaves a player with nothing to decode until the next chunk lands.
    pub new_client_behind: Duration,

    /// Gap between synthesized null packets while a client waits at the head
    /// of a dead stream.
    pub keepalive_interval: Duration,

    /// Keepalive packets keep a player's socket alive, so without a wall-clock
    /// cap a permanently dead upstream holds every client open forever.
    pub max_keepalive: Duration,

    /// Budget for establishing the upstream connection (or for a spawned
    /// command to produce its first bytes).
    pub connection_timeout: Duration,

    /// A connected upstream that goes this long without delivering bytes is
    /// treated as dead and enters the failover path.
    pub stream_timeout: Duration,

    /// How long a session stays up after its last client leaves. Channel
    /// surfing reconnects within this window pay no reconnect cost.
    pub channel_shutdown_delay: Duration,

    /// How long a freshly opened session waits for its first client before it
    /// counts as idle. Separate from `channel_shutdown_delay` because that one
    /// is legitimately zero, and a zero grace here would race the connect that
    /// created the session.
    pub channel_client_wait: Duration,

    /// How long ffmpeg may report a below-`buffering_speed` rate before the
    /// session gives up on the source and switches.
    pub buffering_timeout: Duration,

    /// ffmpeg `speed=` below this means the transcode cannot keep up with
    /// real time, which the viewer experiences as buffering. 1.0 is real time.
    pub buffering_speed: f64,

    pub failover: FailoverConfig,
}

/// Bitrate the byte cap is sized against.
///
/// Not the expected bitrate — the ceiling below which the *duration* cap is
/// the one that binds. An ATSC mux off a LAN HDHomeRun runs ~19.4 Mbps, which
/// is the fattest source this supports, so a cap sized for a typical
/// 8 Mbps IPTV stream would quietly hand those channels a third of the
/// retention they were configured for. Above this rate the byte cap binds
/// first, which is the intended backstop.
pub const RING_SIZING_BITRATE: u64 = 20_000_000;

/// 90 s of retention would be ~90 MB per ring at 8 Mbps and blow the project's
/// memory target on a single channel.
const RING_DURATION: Duration = Duration::from_secs(15);

/// Byte cap that lets `duration` of `bitrate_bps` fit. The ring only ever
/// holds what the source actually produces, so this is a ceiling, not an
/// allocation.
pub fn ring_bytes_for(duration: Duration, bitrate_bps: u64) -> usize {
    let bytes = bitrate_bps / 8 * duration.as_secs().max(1);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            chunk_size: 188 * 1361,
            ring_duration: RING_DURATION,
            ring_max_bytes: ring_bytes_for(RING_DURATION, RING_SIZING_BITRATE),
            new_client_behind: Duration::from_secs(5),
            keepalive_interval: Duration::from_millis(500),
            max_keepalive: Duration::from_secs(300),
            connection_timeout: Duration::from_secs(10),
            stream_timeout: Duration::from_secs(20),
            channel_shutdown_delay: Duration::ZERO,
            channel_client_wait: Duration::from_secs(5),
            buffering_timeout: Duration::from_secs(15),
            buffering_speed: 1.0,
            failover: FailoverConfig::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FailoverConfig {
    /// Connection attempts against one source before moving to the next.
    pub max_retries: u32,

    /// A source that has not failed for this long starts its retry count over,
    /// so a stream that drops once a week never exhausts its retries.
    pub retry_window: Duration,

    /// First pause before wrapping back to the top of an exhausted source
    /// list, doubling per completed pass up to `rotation_cooldown_max`.
    ///
    /// One failed pass is weak evidence — a single bad moment can catch every
    /// source at once — and the fifth is strong evidence of a provider-wide
    /// outage. A flat wait treats those the same, so it has to be long enough
    /// for the outage, which means a transient glitch costs a viewer a minute
    /// of dead screen after the provider is already back.
    pub rotation_cooldown_base: Duration,

    /// Where the growth settles: a sustained outage paces a full pass at this
    /// interval, so a provider is never hammered.
    pub rotation_cooldown_max: Duration,

    /// Uptime that counts as a genuinely working stream. Reaching it clears
    /// the switch bookkeeping, so the next outage starts from a clean slate.
    pub stable_threshold: Duration,

    pub retry_backoff_step: Duration,
    pub retry_backoff_max: Duration,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_window: Duration::from_secs(1800),
            rotation_cooldown_base: Duration::from_secs(5),
            rotation_cooldown_max: Duration::from_secs(60),
            stable_threshold: Duration::from_secs(30),
            retry_backoff_step: Duration::from_millis(250),
            retry_backoff_max: Duration::from_secs(3),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_pinned() {
        let cfg = StreamConfig::default();

        // Pinned, so a change is deliberate.
        assert_eq!(cfg.chunk_size, 188 * 1361);
        assert_eq!(cfg.connection_timeout, Duration::from_secs(10));
        assert_eq!(cfg.stream_timeout, Duration::from_secs(20));
        assert_eq!(cfg.max_keepalive, Duration::from_secs(300));
        assert_eq!(cfg.new_client_behind, Duration::from_secs(5));
        assert_eq!(cfg.failover.max_retries, 3);
        assert_eq!(cfg.failover.retry_window, Duration::from_secs(1800));
        // The cooldown grows to a minute and settles there.
        assert_eq!(cfg.failover.rotation_cooldown_max, Duration::from_secs(60));
        assert_eq!(cfg.failover.stable_threshold, Duration::from_secs(30));

        // 90 s of retention would be ~90 MB per ring at 8 Mbps and blow the
        // memory target on one channel.
        assert_eq!(cfg.ring_duration, Duration::from_secs(15));
    }

    #[test]
    fn the_byte_cap_does_not_shorten_the_window_below_the_sizing_bitrate() {
        let cfg = StreamConfig::default();

        // An ATSC mux from a LAN HDHomeRun still gets the full window.
        let atsc = ring_bytes_for(cfg.ring_duration, 19_400_000);
        assert!(
            cfg.ring_max_bytes >= atsc,
            "byte cap {} binds before {} s of a 19.4 Mbps mux ({atsc} bytes)",
            cfg.ring_max_bytes,
            cfg.ring_duration.as_secs()
        );

        // Above the sizing bitrate it is the backstop it is meant to be.
        assert!(cfg.ring_max_bytes < ring_bytes_for(cfg.ring_duration, 30_000_000));
    }

    #[test]
    fn a_sub_second_window_still_holds_a_second() {
        assert_eq!(
            ring_bytes_for(Duration::from_millis(100), 8_000_000),
            1_000_000
        );
    }
}
