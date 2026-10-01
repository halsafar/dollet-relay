//! Reading a stream profile's stderr: the numbers the Stats page shows, and
//! the one signal that drives a failover decision — whether the source is
//! keeping up with real time.
//!
//! Three tools appear in stream profiles and each says something different.
//! ffmpeg reports `speed=` directly. VLC does not, but it reports how much
//! media it buffered and how long that took, which is the same ratio from its
//! own two measurements. **streamlink reports neither**: its progress line
//! carries throughput in bytes per second, and without a nominal bitrate that
//! cannot tell a healthy 2 Mbps stream from an 8 Mbps one running at a
//! quarter speed. A streamlink profile is therefore left with `stream_timeout`
//! alone, which is a documented gap rather than a guess — a fabricated speed
//! driving a stream switch would be worse than no detection at all.

use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::time::Instant;

/// Codec/format facts, accumulated across the banner ffmpeg prints once at
/// startup. Every field is optional because encoders differ and a source may
/// have no audio at all.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct MediaInfo {
    pub input_format: Option<String>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_fps: Option<f64>,
    pub pixel_format: Option<String>,
    pub video_bitrate_kbps: Option<f64>,
    pub audio_codec: Option<String>,
    pub sample_rate: Option<u32>,
    pub audio_channels: Option<String>,
    pub audio_bitrate_kbps: Option<f64>,
    /// The quality label a source advertised for itself, verbatim — `1080p`,
    /// `best`, `720p_alt`. Only streamlink reports one, and it is deliberately
    /// not translated into pixels: `1080p` is a label, not a resolution, and
    /// an anamorphic source makes the obvious guess wrong.
    pub quality: Option<String>,
}

impl MediaInfo {
    pub fn resolution(&self) -> Option<String> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some(format!("{w}x{h}")),
            _ => None,
        }
    }
}

/// A sample from ffmpeg's periodic progress line.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct Progress {
    pub speed: Option<f64>,
    pub fps: Option<f64>,
    /// Frames per second of the *source*, inferred by removing the encoder's
    /// speed multiplier. A 2x-speed catch-up on a 25 fps source reads as
    /// `fps=50`, which would otherwise look like a 50 fps channel.
    pub actual_fps: Option<f64>,
    pub bitrate_kbps: Option<f64>,
}

/// What a log line says about the source keeping up with real time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pace {
    /// Media time advanced this many times faster than the wall clock. Below
    /// 1.0 the source cannot sustain playback.
    Measured(f64),
    /// The tool says it is re-buffering right now and attaches no ratio. Not
    /// converted into a number: the buffering timeout is what decides whether
    /// it matters, which is exactly the judgement it exists to make.
    Stalled,
}

static PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?: \bspeed= \s* (?P<speed>[0-9]+(?:\.[0-9]+)?) x )
      | (?: \bfps= \s* (?P<fps>[0-9]+(?:\.[0-9]+)?) )
      | (?: \bbitrate= \s* (?P<rate>[0-9]+(?:\.[0-9]+)?) \s* (?P<unit>[kKmMgG]?) bits/s )
    ",
    )
    .expect("static pattern")
});

static VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    // The pixel format must start with a letter, or an aspect-ratio note like
    // `16x9` in the same position is mistaken for one.
    Regex::new(r"Video:\s*(?P<codec>[A-Za-z0-9_]+)[^,]*(?:,\s*(?P<pixfmt>[a-z][a-z0-9]*))?")
        .expect("static pattern")
});
static AUDIO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Audio:\s*(?P<codec>[A-Za-z0-9_]+)").expect("static pattern"));
static RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{3,5})x(\d{3,5})\b").expect("static pattern"));
static FPS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+(?:\.\d+)?)\s*fps").expect("static pattern"));
static KBPS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+(?:\.\d+)?)\s*kb/s").expect("static pattern"));
static SAMPLE_RATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\d+)\s*Hz").expect("static pattern"));
static CHANNELS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(mono|stereo|quad|5\.1|7\.1|2\.1)\b").expect("static pattern")
});
static INPUT_FORMAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Input #\d+,\s*([^,]+)").expect("static pattern"));

// VLC 3.0 `src/input/es_out.c`:
//   msg_Dbg(p_input, "Buffering %d%%", i_level);
//   msg_Dbg(p_input, "Stream buffering done (%d ms in %d ms)",
//           stream_duration_ms, system_duration_ms);
// The pair is media time against wall-clock time — the same quantity ffmpeg
// prints as `speed=`, from VLC's own two numbers.
static VLC_BUFFERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Stream buffering done \((\d+) ms in (\d+) ms\)").expect("static pattern")
});
static VLC_BUFFERING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Buffering (\d+)%").expect("static pattern"));

// VLC 3.0 `modules/stream_out/transcode/video.c`:
//   "source fps %u/%u, destination %u/%u"
//   "source %ix%i, destination %ix%i"
//   "source chroma: %4.4s, destination %4.4s"
static VLC_SOURCE_FPS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"source fps (\d+)/(\d+)").expect("static pattern"));
static VLC_SOURCE_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"source (\d{3,5})x(\d{3,5})").expect("static pattern"));
static VLC_SOURCE_CHROMA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"source chroma: (\S{1,4})").expect("static pattern"));

// VLC 3.0 `modules/demux/mpeg/ts_psi.c`:
//   msg_Dbg(p_demux, "   => pid %d has now es fcc=%4.4s", pid, codec);
// The fourcc is VLC naming the codec itself, which beats inferring it from the
// PMT stream-type byte.
static VLC_FOURCC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"es fcc=(\S{1,4})").expect("static pattern"));

// streamlink `src/streamlink_cli/main.py`:
//   log.info(f"Opening stream: {name} ({stream_type})")
static STREAMLINK_OPENING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Opening stream:\s*(\S+)\s*\(([^)]+)\)").expect("static pattern"));

/// The source's own report of whether it is keeping up.
///
/// Only VLC is handled here: ffmpeg's arrives with the badge numbers through
/// `parse_progress`, and streamlink does not report the quantity at all.
pub fn parse_pace(line: &str) -> Option<Pace> {
    if let Some(caps) = VLC_BUFFERED.captures(line) {
        let media: f64 = caps[1].parse().ok()?;
        let wall: f64 = caps[2].parse().ok()?;
        // VLC rounds to whole milliseconds, so a buffer that filled instantly
        // reads as zero elapsed. That is as healthy as it gets.
        return Some(Pace::Measured(if wall <= 0.0 {
            f64::INFINITY
        } else {
            media / wall
        }));
    }
    if VLC_BUFFERING.is_match(line) {
        return Some(Pace::Stalled);
    }
    None
}

/// VLC's codec fourccs, as it prints them for a transport stream. An unknown
/// one is recorded verbatim rather than guessed at; the pair says which of the
/// two codec fields it belongs in.
fn vlc_codec(fourcc: &str) -> Option<(bool, &'static str)> {
    Some(match fourcc.trim().to_ascii_lowercase().as_str() {
        "h264" | "avc1" => (true, "h264"),
        "hevc" | "hvc1" => (true, "hevc"),
        "mpgv" | "mp2v" => (true, "mpeg2video"),
        "mp4v" => (true, "mpeg4"),
        "mp4a" => (false, "aac"),
        "a52" => (false, "ac3"),
        "eac3" => (false, "eac3"),
        "mpga" => (false, "mp2"),
        "dts" => (false, "dts"),
        _ => return None,
    })
}

fn apply_vlc_line(info: &mut MediaInfo, line: &str) -> bool {
    let mut learned = false;
    if let Some(caps) = VLC_SOURCE_SIZE.captures(line) {
        let (w, h) = (caps[1].parse().unwrap_or(0), caps[2].parse().unwrap_or(0));
        if (100..=10_000).contains(&w) && (100..=10_000).contains(&h) {
            info.width = Some(w);
            info.height = Some(h);
            learned = true;
        }
    }
    if let Some(caps) = VLC_SOURCE_FPS.captures(line) {
        let (num, den) = (caps[1].parse::<f64>(), caps[2].parse::<f64>());
        if let (Ok(num), Ok(den)) = (num, den)
            && den > 0.0
        {
            info.source_fps = Some(num / den);
            learned = true;
        }
    }
    if let Some(caps) = VLC_SOURCE_CHROMA.captures(line) {
        info.pixel_format = Some(caps[1].trim().to_ascii_lowercase());
        learned = true;
    }
    if let Some(caps) = VLC_FOURCC.captures(line)
        && let Some((video, codec)) = vlc_codec(&caps[1])
    {
        if video {
            info.video_codec = Some(codec.to_owned());
        } else {
            info.audio_codec = Some(codec.to_owned());
        }
        learned = true;
    }
    learned
}

fn apply_streamlink_line(info: &mut MediaInfo, line: &str) -> bool {
    let Some(caps) = STREAMLINK_OPENING.captures(line) else {
        return false;
    };
    info.quality = Some(caps[1].to_owned());
    info.input_format = Some(caps[2].trim().to_ascii_lowercase());
    true
}

/// ffmpeg emits progress lines terminated by `\r`, not `\n`, so a line-based
/// reader that only splits on newline sees one enormous line and never
/// reports a speed. Returns `None` when the line is not a progress update.
pub fn parse_progress(line: &str) -> Option<Progress> {
    if !line.contains("frame=") && !line.contains("speed=") {
        return None;
    }
    let mut p = Progress::default();
    for caps in PROGRESS.captures_iter(line) {
        if let Some(m) = caps.name("speed") {
            p.speed = m.as_str().parse().ok();
        } else if let Some(m) = caps.name("fps") {
            p.fps = m.as_str().parse().ok();
        } else if let Some(m) = caps.name("rate") {
            let value: f64 = m.as_str().parse().unwrap_or_default();
            let scale = match caps.name("unit").map(|u| u.as_str()) {
                Some("m" | "M") => 1_000.0,
                Some("g" | "G") => 1_000_000.0,
                _ => 1.0,
            };
            p.bitrate_kbps = Some(value * scale);
        }
    }
    if let (Some(fps), Some(speed)) = (p.fps, p.speed)
        && speed > 0.0
    {
        p.actual_fps = Some(fps / speed);
    }
    if p == Progress::default() {
        return None;
    }
    Some(p)
}

/// Folds one stderr line into `info`. Returns true when something was learned,
/// so the caller can avoid publishing an unchanged stats payload.
pub fn apply_media_line(info: &mut MediaInfo, line: &str) -> bool {
    let lower = line.to_ascii_lowercase();

    if lower.starts_with("input #")
        && let Some(caps) = INPUT_FORMAT.captures(line)
    {
        info.input_format = Some(caps[1].trim().to_owned());
        return true;
    }

    if !lower.contains("stream #") {
        // Not an ffmpeg banner line; the other two tools say different things.
        return apply_vlc_line(info, line) || apply_streamlink_line(info, line);
    }

    if lower.contains("video:") {
        let mut learned = false;
        if let Some(caps) = VIDEO.captures(line) {
            info.video_codec = Some(caps["codec"].to_owned());
            // Only overwrite when this line actually carries one: ffmpeg
            // prints several `Stream #` lines and the later ones are often
            // terser than the first.
            if let Some(pixfmt) = caps.name("pixfmt") {
                info.pixel_format = Some(pixfmt.as_str().to_owned());
            }
            learned = true;
        }
        if let Some(caps) = RESOLUTION.captures(line) {
            // ffmpeg also prints aspect ratios like 16:9 and timestamps that
            // can look dimension-ish; bound the values to plausible video.
            let (w, h) = (caps[1].parse().unwrap_or(0), caps[2].parse().unwrap_or(0));
            if (100..=10_000).contains(&w) && (100..=10_000).contains(&h) {
                info.width = Some(w);
                info.height = Some(h);
                learned = true;
            }
        }
        if let Some(caps) = FPS.captures(line) {
            info.source_fps = caps[1].parse().ok();
            learned = true;
        }
        if let Some(caps) = KBPS.captures(line) {
            info.video_bitrate_kbps = caps[1].parse().ok();
            learned = true;
        }
        return learned;
    }

    if lower.contains("audio:") {
        let mut learned = false;
        if let Some(caps) = AUDIO.captures(line) {
            info.audio_codec = Some(caps["codec"].to_owned());
            learned = true;
        }
        if let Some(caps) = SAMPLE_RATE.captures(line) {
            info.sample_rate = caps[1].parse().ok();
            learned = true;
        }
        if let Some(caps) = CHANNELS.captures(line) {
            info.audio_channels = Some(caps[1].to_ascii_lowercase());
            learned = true;
        }
        if let Some(caps) = KBPS.captures(line) {
            info.audio_bitrate_kbps = caps[1].parse().ok();
            learned = true;
        }
        return learned;
    }

    false
}

/// Sustained below-real-time encoding. One slow sample is normal during
/// startup; `timeout` seconds of them means the source cannot be played at
/// the rate it is being consumed, which the viewer sees as buffering.
pub struct BufferingDetector {
    threshold: f64,
    timeout: Duration,
    slow_since: Option<Instant>,
}

impl BufferingDetector {
    pub fn new(threshold: f64, timeout: Duration) -> Self {
        Self {
            threshold,
            timeout,
            slow_since: None,
        }
    }

    pub fn is_buffering(&self) -> bool {
        self.slow_since.is_some()
    }

    /// Returns true when the source should be abandoned.
    pub fn sample(&mut self, speed: f64, now: Instant) -> bool {
        if speed >= self.threshold {
            self.slow_since = None;
            return false;
        }
        self.slow(now)
    }

    /// The source says it is re-buffering and gave no rate. Timed the same way
    /// as a below-threshold sample, because the question — has this gone on
    /// long enough to matter — is the same one.
    pub fn stalled(&mut self, now: Instant) -> bool {
        self.slow(now)
    }

    fn slow(&mut self, now: Instant) -> bool {
        match self.slow_since {
            None => {
                self.slow_since = Some(now);
                false
            }
            Some(since) => {
                if now.saturating_duration_since(since) > self.timeout {
                    // The caller switches sources; clear the state so the next
                    // source starts its own measurement.
                    self.slow_since = None;
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATS: &str = "frame= 1234 fps= 30 q=28.0 size=    2048kB time=00:00:41.33 bitrate= 406.1kbits/s speed=1.02x";

    #[test]
    fn parses_a_progress_line() {
        let p = parse_progress(STATS).expect("progress");
        assert_eq!(p.speed, Some(1.02));
        assert_eq!(p.fps, Some(30.0));
        assert_eq!(p.bitrate_kbps, Some(406.1));
        assert_eq!(p.actual_fps, Some(30.0 / 1.02));
    }

    #[test]
    fn scales_bitrate_units() {
        let p = parse_progress("frame=1 bitrate= 8.5Mbits/s speed=1x").expect("progress");
        assert_eq!(p.bitrate_kbps, Some(8500.0));
        let p = parse_progress("frame=1 bitrate=0.01gbits/s speed=1x").expect("progress");
        assert_eq!(p.bitrate_kbps, Some(10_000.0));
    }

    #[test]
    fn ignores_non_progress_lines() {
        assert!(parse_progress("Input #0, mpegts, from 'pipe:0':").is_none());
        // "frame=" present but nothing parseable.
        assert!(parse_progress("frame=").is_none());
    }

    #[test]
    fn a_zero_speed_yields_no_actual_fps() {
        let p = parse_progress("frame=1 fps=25 speed=0.0x").expect("progress");
        assert_eq!(p.speed, Some(0.0));
        assert_eq!(p.actual_fps, None);
    }

    #[test]
    fn parses_the_input_banner() {
        let mut info = MediaInfo::default();
        assert!(apply_media_line(
            &mut info,
            "Input #0, mpegts, from 'http://example/x':"
        ));
        assert_eq!(info.input_format.as_deref(), Some("mpegts"));
    }

    #[test]
    fn parses_a_video_stream_line() {
        let mut info = MediaInfo::default();
        let line = "  Stream #0:0[0x100]: Video: h264 (High), yuv420p(tv, bt709), 1920x1080 [SAR 1:1 DAR 16:9], 5000 kb/s, 25 fps, 25 tbr, 90k tbn";
        assert!(apply_media_line(&mut info, line));

        assert_eq!(info.video_codec.as_deref(), Some("h264"));
        assert_eq!(info.resolution().as_deref(), Some("1920x1080"));
        assert_eq!(info.source_fps, Some(25.0));
        assert_eq!(info.pixel_format.as_deref(), Some("yuv420p"));
        assert_eq!(info.video_bitrate_kbps, Some(5000.0));
    }

    #[test]
    fn parses_an_audio_stream_line() {
        let mut info = MediaInfo::default();
        let line = "  Stream #0:1[0x101]: Audio: ac3 (AC-3 / 0x332D4341), 48000 Hz, 5.1(side), fltp, 384 kb/s";
        assert!(apply_media_line(&mut info, line));

        assert_eq!(info.audio_codec.as_deref(), Some("ac3"));
        assert_eq!(info.sample_rate, Some(48_000));
        assert_eq!(info.audio_channels.as_deref(), Some("5.1"));
        assert_eq!(info.audio_bitrate_kbps, Some(384.0));
    }

    #[test]
    fn a_terser_later_line_does_not_erase_what_was_learned() {
        let mut info = MediaInfo::default();
        apply_media_line(
            &mut info,
            "  Stream #0:0: Video: h264 (High), yuv420p, 1920x1080, 25 fps",
        );
        apply_media_line(&mut info, "  Stream #0:3: Video: mjpeg, 600x600");

        assert_eq!(info.pixel_format.as_deref(), Some("yuv420p"));
    }

    #[test]
    fn rejects_implausible_resolutions() {
        let mut info = MediaInfo::default();
        // A 16x9 aspect note must not become the resolution.
        assert!(apply_media_line(
            &mut info,
            "  Stream #0:0: Video: h264, 16x9, 25 fps"
        ));
        assert_eq!(info.width, None);
        assert_eq!(info.source_fps, Some(25.0));
    }

    #[test]
    fn ignores_lines_that_teach_nothing() {
        let mut info = MediaInfo::default();
        assert!(!apply_media_line(&mut info, "  Metadata:"));
        assert!(!apply_media_line(
            &mut info,
            "  Stream #0:2: Subtitle: dvb_teletext"
        ));
        assert!(!apply_media_line(&mut info, "Input #0"));
        assert_eq!(info, MediaInfo::default());
        assert_eq!(info.resolution(), None);
    }

    #[test]
    fn video_line_without_a_recognisable_codec_learns_nothing() {
        let mut info = MediaInfo::default();
        assert!(!apply_media_line(&mut info, "Stream #0:0: video:"));
    }

    // Sample lines are the real format strings from each tool's source, with
    // the log prefix each one actually emits.
    const VLC_SLOW: &str =
        "[7f8c0c000e00] main input debug: Stream buffering done (1000 ms in 4000 ms)";
    const VLC_FAST: &str =
        "[7f8c0c000e00] main input debug: Stream buffering done (1000 ms in 200 ms)";

    #[test]
    fn vlc_pace_is_its_own_two_measurements_divided() {
        // VLC does not print a speed, but it prints how much media it buffered
        // and how long that took, which is the same ratio.
        assert_eq!(parse_pace(VLC_SLOW), Some(Pace::Measured(0.25)));
        assert_eq!(parse_pace(VLC_FAST), Some(Pace::Measured(5.0)));
    }

    #[test]
    fn an_instant_vlc_buffer_reads_as_healthy_not_as_a_divide_by_zero() {
        let line = "main input debug: Stream buffering done (1000 ms in 0 ms)";
        let Some(Pace::Measured(speed)) = parse_pace(line) else {
            panic!("no pace");
        };
        assert!(speed.is_infinite() && speed.is_sign_positive());
    }

    #[test]
    fn a_vlc_rebuffer_in_progress_carries_no_number() {
        assert_eq!(
            parse_pace("[7f8c0c000e00] main input debug: Buffering 37%"),
            Some(Pace::Stalled)
        );
    }

    #[test]
    fn streamlink_throughput_is_not_a_pace() {
        // Records the documented gap: bytes per second cannot distinguish a
        // healthy low-bitrate stream from a high-bitrate one running slow, so
        // nothing here may be turned into a speed.
        assert!(parse_pace("[download] Written 12.34 MiB (1m23s @ 456.78 KiB/s)").is_none());
        assert!(parse_pace("[cli][info] Opening stream: 1080p (hls)").is_none());
    }

    #[test]
    fn vlc_transcode_lines_fill_the_badges() {
        let mut info = MediaInfo::default();
        let prefix = "[7f8c0c0a1234] stream_out_transcode stream debug:";
        assert!(apply_media_line(
            &mut info,
            &format!("{prefix} source 1920x1080, destination 1280x720")
        ));
        assert!(apply_media_line(
            &mut info,
            &format!("{prefix} source fps 30000/1001, destination 30000/1001")
        ));
        assert!(apply_media_line(
            &mut info,
            &format!("{prefix} source chroma: I420, destination I420")
        ));

        assert_eq!(info.resolution().as_deref(), Some("1920x1080"));
        assert_eq!(info.pixel_format.as_deref(), Some("i420"));
        let fps = info.source_fps.expect("fps");
        assert!((fps - 29.97).abs() < 0.01, "{fps}");
    }

    #[test]
    fn vlc_names_its_codecs_by_fourcc() {
        let mut info = MediaInfo::default();
        let prefix = "[7f8c0c0b0000] ts demux debug:";
        assert!(apply_media_line(
            &mut info,
            &format!("{prefix}    => pid 256 has now es fcc=h264")
        ));
        // %4.4s pads a three-character fourcc with a space.
        assert!(apply_media_line(
            &mut info,
            &format!("{prefix}    => pid 257 has now es fcc=a52 ")
        ));

        assert_eq!(info.video_codec.as_deref(), Some("h264"));
        assert_eq!(info.audio_codec.as_deref(), Some("ac3"));
    }

    #[test]
    fn an_unrecognised_fourcc_is_left_alone_rather_than_guessed_at() {
        let mut info = MediaInfo::default();
        assert!(!apply_media_line(
            &mut info,
            "ts demux debug:    => pid 258 has now es fcc=zzzz"
        ));
        assert_eq!(info, MediaInfo::default());
    }

    #[test]
    fn streamlink_reports_a_quality_label_and_a_transport() {
        let mut info = MediaInfo::default();
        assert!(apply_media_line(
            &mut info,
            "[cli][info] Opening stream: 1080p (hls)"
        ));

        // Verbatim: `1080p` is a label, and an anamorphic source makes the
        // obvious translation into pixels wrong.
        assert_eq!(info.quality.as_deref(), Some("1080p"));
        assert_eq!(info.input_format.as_deref(), Some("hls"));
        assert_eq!(info.width, None);
        assert_eq!(info.resolution(), None);
    }

    #[test]
    fn a_stall_with_no_number_still_times_out() {
        let mut d = BufferingDetector::new(1.0, Duration::from_secs(5));
        let t = Instant::now();

        assert!(!d.stalled(t));
        assert!(d.is_buffering());
        assert!(!d.stalled(t + Duration::from_secs(4)));
        assert!(d.stalled(t + Duration::from_secs(6)));
    }

    #[test]
    fn vlc_finishing_a_rebuffer_quickly_clears_the_timer() {
        let mut d = BufferingDetector::new(1.0, Duration::from_secs(5));
        let t = Instant::now();

        assert!(!d.stalled(t));
        let Some(Pace::Measured(speed)) = parse_pace(VLC_FAST) else {
            panic!("no pace");
        };
        assert!(!d.sample(speed, t + Duration::from_secs(1)));
        assert!(!d.is_buffering());
    }

    #[test]
    fn buffering_needs_sustained_slowness() {
        let mut d = BufferingDetector::new(1.0, Duration::from_secs(5));
        let t = Instant::now();

        assert!(!d.sample(0.5, t));
        assert!(d.is_buffering());
        assert!(!d.sample(0.5, t + Duration::from_secs(4)));
        assert!(d.sample(0.5, t + Duration::from_secs(6)));
        // Cleared, so the next source measures from scratch.
        assert!(!d.is_buffering());
    }

    #[test]
    fn recovering_speed_clears_the_timer() {
        let mut d = BufferingDetector::new(1.0, Duration::from_secs(5));
        let t = Instant::now();

        assert!(!d.sample(0.2, t));
        assert!(!d.sample(1.1, t + Duration::from_secs(1)));
        assert!(!d.is_buffering());
        assert!(!d.sample(0.2, t + Duration::from_secs(10)));
        assert!(!d.sample(0.2, t + Duration::from_secs(12)));
    }
}
