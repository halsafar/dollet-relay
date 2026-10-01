//! A synthetic transport stream, so the engine can be driven end to end with
//! no network, no database and no ffmpeg.

use std::io::Write;
use std::time::Duration;

use bytes::{Bytes, BytesMut};

use crate::config::{FailoverConfig, StreamConfig};
use crate::input::{SourceLimit, SourceProfile, StreamSource};
use crate::ts::TS_PACKET_SIZE;

/// Valid-shaped MPEG-TS: real sync byte, one PID, a wrapping continuity
/// counter, and a payload whose first eight bytes are the packet ordinal —
/// which is what lets a test prove *which* packets a client saw, and so
/// whether a slow cursor skipped forward rather than stalling the producer.
pub struct TsGenerator {
    pid: u16,
    continuity: u8,
    next: u64,
}

impl TsGenerator {
    pub fn new() -> Self {
        Self::starting_at(0)
    }

    /// Ordinals from `start`, so two synthetic sources in one test are
    /// distinguishable and a splice between them can be checked for gaps and
    /// duplicates rather than only for "something arrived".
    pub fn starting_at(start: u64) -> Self {
        Self {
            pid: 0x0100,
            continuity: 0,
            next: start,
        }
    }

    pub fn packet(&mut self) -> [u8; TS_PACKET_SIZE] {
        let mut packet = [0xFFu8; TS_PACKET_SIZE];
        packet[0] = 0x47;
        packet[1] = ((self.pid >> 8) as u8) & 0x1F;
        packet[2] = (self.pid & 0xFF) as u8;
        packet[3] = 0x10 | (self.continuity & 0x0F);
        packet[4..12].copy_from_slice(&self.next.to_be_bytes());

        self.continuity = self.continuity.wrapping_add(1);
        self.next += 1;
        packet
    }

    pub fn bytes(&mut self, packets: usize) -> Bytes {
        let mut out = BytesMut::with_capacity(packets * TS_PACKET_SIZE);
        for _ in 0..packets {
            out.extend_from_slice(&self.packet());
        }
        out.freeze()
    }
}

/// Packet ordinals in receive order, for asserting continuity or a skip.
/// Keepalive null packets are filtered out: they are wire padding, not data.
pub fn ordinals(data: &[u8]) -> Vec<u64> {
    data.as_chunks::<TS_PACKET_SIZE>()
        .0
        .iter()
        .filter(|p| u16::from_be_bytes([p[1], p[2]]) & 0x1FFF == 0x0100)
        .map(|p| u64::from_be_bytes(p[4..12].try_into().expect("8 bytes")))
        .collect()
}

pub fn is_aligned(data: &[u8]) -> bool {
    !data.is_empty()
        && data.len().is_multiple_of(TS_PACKET_SIZE)
        && data
            .as_chunks::<TS_PACKET_SIZE>()
            .0
            .iter()
            .all(|p| p[0] == 0x47)
}

/// A shell command that behaves like live TV: it never ends and never stalls
/// long enough to look dead.
pub fn ts_loop(file: &tempfile::NamedTempFile) -> String {
    format!(
        "while :; do cat {}; sleep 0.05; done",
        shell_words::quote(&file.path().to_string_lossy())
    )
}

/// The same, prefixing every pass with an ffmpeg progress line reporting half
/// real-time speed, which is what buffering detection watches for.
pub fn slow_ts_loop(file: &tempfile::NamedTempFile) -> String {
    format!(
        "while :; do printf 'frame= 1 fps= 12 bitrate= 400kbits/s speed=0.5x\r' >&2; cat {}; sleep 0.05; done",
        shell_words::quote(&file.path().to_string_lossy())
    )
}

/// Like `slow_ts_loop`, but reporting the way VLC does: a rebuffer in
/// progress, with no speed attached. The percentage line is what VLC emits
/// while it is still filling, which is the shape of a stall that never
/// completes.
pub fn vlc_stall_loop(file: &tempfile::NamedTempFile) -> String {
    format!(
        "while :; do printf 'main input debug: Buffering 12%%%%\r' >&2; cat {}; sleep 0.05; done",
        shell_words::quote(&file.path().to_string_lossy())
    )
}

/// A source that works, goes away, and comes back -- a provider blip, which is
/// the single most ordinary thing that happens to an IPTV stream.
///
/// Serves `first` once, refuses the next `outage` connections, then serves
/// `second` indefinitely. The counter lives in a file so it survives the
/// process exiting between attempts, which is the whole point.
pub fn blip_script(
    first: &tempfile::NamedTempFile,
    second: &tempfile::NamedTempFile,
    counter: &std::path::Path,
    outage: u32,
) -> String {
    let (a, b, c) = (
        shell_words::quote(&first.path().to_string_lossy()).into_owned(),
        shell_words::quote(&second.path().to_string_lossy()).into_owned(),
        shell_words::quote(&counter.to_string_lossy()).into_owned(),
    );
    format!(
        "n=$(cat {c} 2>/dev/null || echo 0); echo $((n+1)) > {c}; \
         if [ \"$n\" -eq 0 ]; then cat {a}; exit 0; fi; \
         if [ \"$n\" -le {outage} ]; then exit 1; fi; \
         while :; do cat {b}; sleep 0.05; done"
    )
}

/// Connects, and then says nothing at all. A provider answering 200 and
/// sending no bytes is indistinguishable from a working one until the read
/// times out, which is what makes it worth a test.
pub fn silent_script() -> String {
    "sleep 30".to_owned()
}

/// A transport stream on disk, for feeding a spawned command.
pub fn ts_file(packets: usize) -> tempfile::NamedTempFile {
    ts_file_from(0, packets)
}

pub fn ts_file_from(start: u64, packets: usize) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    let mut generator = TsGenerator::starting_at(start);
    file.write_all(&generator.bytes(packets)).expect("write");
    file.flush().expect("flush");
    file
}

/// Wraps a shell script as a stream-profile command source, which is how a
/// test stands in for ffmpeg without one installed.
pub fn shell_source(id: i64, script: &str) -> StreamSource {
    StreamSource {
        id,
        url: format!("http://upstream.invalid/{id}.ts"),
        user_agent: "dollet-test".to_owned(),
        profile: SourceProfile::Command {
            command: "sh".to_owned(),
            parameters: format!("-c {}", shell_words::quote(script)),
        },
        limit: None,
    }
}

/// A plain proxied HTTP source, the default stream profile.
pub fn http_source(id: i64, url: &str) -> StreamSource {
    StreamSource {
        id,
        url: url.to_owned(),
        user_agent: "dollet-test/1.0".to_owned(),
        profile: SourceProfile::Proxy,
        limit: None,
    }
}

pub fn with_limit(mut source: StreamSource, key: i64, max_streams: u32) -> StreamSource {
    source.limit = Some(SourceLimit { key, max_streams });
    source
}

/// Small and fast everywhere: tests must not wait out production timeouts.
pub fn test_config() -> StreamConfig {
    StreamConfig {
        chunk_size: TS_PACKET_SIZE * 4,
        ring_duration: Duration::from_secs(30),
        ring_max_bytes: 1 << 20,
        new_client_behind: Duration::ZERO,
        keepalive_interval: Duration::from_millis(20),
        max_keepalive: Duration::from_millis(200),
        connection_timeout: Duration::from_millis(600),
        stream_timeout: Duration::from_millis(600),
        channel_shutdown_delay: Duration::ZERO,
        channel_client_wait: Duration::from_secs(5),
        buffering_timeout: Duration::from_millis(100),
        buffering_speed: 1.0,
        failover: FailoverConfig {
            max_retries: 1,
            retry_window: Duration::from_secs(1800),
            rotation_cooldown_base: Duration::from_millis(50),
            rotation_cooldown_max: Duration::from_millis(50),
            stable_threshold: Duration::from_secs(30),
            retry_backoff_step: Duration::from_millis(5),
            retry_backoff_max: Duration::from_millis(20),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_aligned_packets_with_readable_ordinals() {
        let mut generator = TsGenerator::new();
        let data = generator.bytes(20);

        assert!(is_aligned(&data));
        assert_eq!(ordinals(&data), (0..20).collect::<Vec<_>>());
        // Continuity wraps at 16, as a real encoder's does.
        assert_eq!(data[3 + TS_PACKET_SIZE * 16] & 0x0F, 0);
    }

    #[test]
    fn detects_misalignment() {
        let mut generator = TsGenerator::new();
        let data = generator.bytes(2);
        assert!(!is_aligned(&[]));
        assert!(!is_aligned(&data[..TS_PACKET_SIZE + 1]));

        let mut lost_sync = data.to_vec();
        lost_sync[TS_PACKET_SIZE] = 0x00;
        assert!(!is_aligned(&lost_sync));
    }

    #[test]
    fn keepalive_packets_are_not_counted_as_data() {
        let mut generator = TsGenerator::new();
        let mut wire = generator.bytes(1).to_vec();
        wire.extend_from_slice(&crate::ts::null_packet());
        wire.extend_from_slice(&generator.bytes(1));

        assert!(is_aligned(&wire));
        assert_eq!(ordinals(&wire), vec![0, 1]);
    }
}
