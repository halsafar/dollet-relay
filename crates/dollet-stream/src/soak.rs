//! Memory under load: the number this project exists to move.
//!
//! `docs/ARCHITECTURE.md` commits to `base + Σ(ring_seconds × bitrate) + Σ(ffmpeg)`
//! and to plateauing rather than climbing. Unit tests cannot see a slow leak —
//! it is what a user finds after a fortnight of uptime — so this drives real
//! traffic through real child processes and watches the number.
//!
//! Two instruments, because they answer different questions.
//!
//! **Live heap**, from a counting allocator installed only in the test binary,
//! answers "did the engine give it back". It is exact and independent of what
//! the allocator chooses to return to the kernel. Other tests share this
//! process, so every sample is a floor taken as the minimum over a window:
//! concurrent work can only ever add, never subtract.
//!
//! **RSS**, from `/proc/self/statm` as `health.rs` reads it, answers "what
//! would `podman stats` show". It is reported rather than asserted on, because
//! a test binary's RSS includes the harness and every other test.
//!
//! What this catches is a *structural* leak — a session, ring, cursor or child
//! process that churn does not release. It is not sensitive enough to see a
//! few bytes per client, and says so rather than implying otherwise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;

use crate::client::{ClientDescriptor, ClientStream};
use crate::config::{RING_SIZING_BITRATE, StreamConfig, ring_bytes_for};
use crate::registry::{ChannelSpec, Connection, Registry};
use crate::ring::Ring;
use crate::synthetic::{TsGenerator, shell_source, test_config, ts_file, ts_loop};
use crate::{OutputKey, Transcode};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every method forwards to the system allocator with the same layout
// it was given, and only adds bookkeeping around it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn live_bytes() -> usize {
    LIVE.load(Ordering::Relaxed)
}

/// The floor of the live heap over `window`, which is the estimate that
/// survives other tests running alongside this one.
async fn heap_floor(window: Duration) -> usize {
    let step = window / 20;
    let mut floor = usize::MAX;
    for _ in 0..20 {
        floor = floor.min(live_bytes());
        tokio::time::sleep(step).await;
    }
    floor
}

/// What `podman stats` would show, by the same route `health.rs` uses.
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn soak_config() -> StreamConfig {
    StreamConfig {
        // Production chunking, so the ring holds what it would in the field
        // rather than a test-sized approximation.
        chunk_size: 188 * 1361,
        ring_duration: Duration::from_secs(2),
        ring_max_bytes: ring_bytes_for(Duration::from_secs(2), RING_SIZING_BITRATE),
        ..test_config()
    }
}

fn connect(registry: &Registry, spec: ChannelSpec, output: OutputKey) -> ClientStream {
    match registry
        .connect(output, spec, ClientDescriptor::default())
        .expect("connected")
    {
        Connection::Stream(stream) => stream,
        Connection::Redirect { .. } => panic!("unexpected redirect"),
    }
}

/// Reads as fast as the ring will hand data over.
async fn drain(mut stream: ClientStream, until: tokio::time::Instant) {
    while tokio::time::Instant::now() < until {
        match tokio::time::timeout(Duration::from_millis(200), stream.next()).await {
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => return,
            Err(_) => {}
        }
    }
}

/// Reads far slower than the producer, so its cursor falls out of the
/// retention window and takes the skip-forward path continuously rather than
/// once.
async fn dawdle(mut stream: ClientStream, until: tokio::time::Instant) {
    while tokio::time::Instant::now() < until {
        match tokio::time::timeout(Duration::from_millis(200), stream.next()).await {
            Ok(Some(Ok(_))) => tokio::time::sleep(Duration::from_millis(120)).await,
            Ok(Some(Err(_)) | None) => return,
            Err(_) => {}
        }
    }
}

const CHANNELS: usize = 4;
const CLIENTS_PER_CHANNEL: usize = 3;

/// The sources for one channel of the soak. Channel 1 has two dead sources
/// ahead of its live one, so the failover path runs; the rest connect first
/// time.
fn soak_sources(channel: usize, script: &str) -> Vec<crate::StreamSource> {
    if channel == 1 {
        vec![
            shell_source(1, "exit 1"),
            shell_source(2, "exit 1"),
            shell_source(3, script),
        ]
    } else {
        vec![shell_source(1, script)]
    }
}

/// Channel 2 is served through an output profile, so the
/// ring-downstream-of-a-ring path carries load too.
fn soak_output(channel: usize, round: i64) -> (OutputKey, Option<Transcode>) {
    if channel == 2 {
        (
            OutputKey::Profile(round),
            Some(Transcode {
                command: "cat".to_owned(),
                parameters: String::new(),
            }),
        )
    } else {
        (OutputKey::Raw, None)
    }
}

/// Opens every channel with its clients, one of them deliberately slow, and
/// returns the reader tasks.
fn open_all(
    registry: &Arc<Registry>,
    script: &str,
    round: i64,
    until: tokio::time::Instant,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut readers = Vec::new();
    for channel in 0..CHANNELS {
        // A fresh uuid every round, so nothing is reused and every session
        // must be built and torn down again.
        let uuid = uuid::Uuid::new_v4();
        let sources = soak_sources(channel, script);
        let (output, transcode) = soak_output(channel, round);

        for client in 0..CLIENTS_PER_CHANNEL {
            let spec = ChannelSpec {
                channel: uuid,
                sources: sources.clone(),
                transcode: transcode.clone(),
            };
            let stream = connect(registry, spec, output);
            readers.push(if client == 0 {
                tokio::spawn(dawdle(stream, until))
            } else {
                tokio::spawn(drain(stream, until))
            });
        }
    }
    readers
}

async fn wait_for_reap(registry: &Registry) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if registry.stats().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("sessions outlived their clients: {:?}", registry.stats());
}

/// What the engine is holding right now, by its own accounting: the sum of
/// every live ring.
fn accounted_bytes(registry: &Registry) -> usize {
    registry.stats().iter().map(|s| s.buffer.bytes).sum()
}

fn soak_registry() -> Arc<Registry> {
    Arc::new(Registry::new(
        Arc::new(soak_config()),
        reqwest::Client::new(),
    ))
}

/// The whole memory claim, in one test: what the engine holds under load, and
/// whether it gives it back under churn.
///
/// Deliberately one test and not two. Both halves measure a process-wide heap
/// floor, so running them concurrently makes each one's rings part of the
/// other's baseline — which passes or fails depending on which finishes first.
/// That is the shape of a test that gets deleted for being flaky.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_tracks_the_ring_caps_and_comes_back() {
    const CYCLES: i64 = 5;

    let file = ts_file(4_000);
    let script = ts_loop(&file);
    let registry = soak_registry();
    let cfg = soak_config();

    let cycle = async |round: i64, load: Duration| {
        let until = tokio::time::Instant::now() + load;
        for reader in open_all(&registry, &script, round, until) {
            let _ = reader.await;
        }
        wait_for_reap(&registry).await;
    };

    // Warm up: allocator arenas, the runtime and the child-process machinery
    // all reach steady state in the first rounds, and counting that as growth
    // would flag every run.
    cycle(0, Duration::from_millis(300)).await;
    cycle(1, Duration::from_millis(300)).await;
    let idle = heap_floor(Duration::from_millis(300)).await;
    let rss_idle = resident_bytes();

    // --- Under load -------------------------------------------------------
    let until = tokio::time::Instant::now() + Duration::from_secs(2);
    let readers = open_all(&registry, &script, 2, until);
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    let rings = registry.stats().len();
    let clients: usize = registry.stats().iter().map(|s| s.clients.len()).sum();
    let accounted = accounted_bytes(&registry);
    let loaded = heap_floor(Duration::from_millis(400)).await;
    let rss_loaded = resident_bytes();

    for reader in readers {
        let _ = reader.await;
    }
    wait_for_reap(&registry).await;

    let held = loaded.saturating_sub(idle);
    eprintln!(
        "soak/load: {rings} rings x {:.1} MiB cap, {clients} clients; \
         accounted {:.1} MiB, heap {:.1} MiB ({:.2}x), rss {:.1} -> {:.1} MiB",
        mib(cfg.ring_max_bytes),
        mib(accounted),
        mib(held),
        held as f64 / accounted.max(1) as f64,
        rss_idle.map_or(0.0, |b| mib(b as usize)),
        rss_loaded.map_or(0.0, |b| mib(b as usize)),
    );

    assert!(rings >= CHANNELS, "only {rings} rings carried load");
    assert!(
        accounted <= cfg.ring_max_bytes * rings,
        "accounted {accounted} exceeds {rings} caps of {}",
        cfg.ring_max_bytes
    );
    assert!(
        accounted > cfg.ring_max_bytes / 2 * rings,
        "the caps never bound: {accounted} across {rings} rings"
    );

    // Overhead above the rings is fixed per session and per client -- a
    // packetizer, a read buffer, and whatever chunk each cursor is holding --
    // and must not be proportional to the ring. The per-client term is the
    // one that matters: if a chunk were ever copied per client instead of
    // being a refcount clone, this is where it would show, and it would be
    // enormous rather than marginal.
    let budget = rings * cfg.chunk_size * 6 + clients * cfg.chunk_size * 3;
    assert!(
        held <= accounted + budget,
        "held {:.1} MiB against {:.1} MiB of rings plus a {:.1} MiB budget",
        mib(held),
        mib(accounted),
        mib(budget)
    );

    // --- Under churn ------------------------------------------------------
    let baseline = heap_floor(Duration::from_millis(300)).await;
    let mut floors = Vec::new();
    let mut rss_trail = Vec::new();
    for round in 3..3 + CYCLES {
        cycle(round, Duration::from_millis(300)).await;
        floors.push(heap_floor(Duration::from_millis(200)).await);
        rss_trail.push(resident_bytes().unwrap_or(0));
    }

    let last = *floors.last().expect("cycles ran");
    let shown: Vec<String> = floors.iter().map(|f| format!("{:.2}", mib(*f))).collect();
    // RSS is reported, not asserted on: in a test binary it carries the
    // harness and every other test. It is here so a human can see it plateau
    // next to the heap that is asserted on.
    let rss_shown: Vec<String> = rss_trail
        .iter()
        .map(|r| format!("{:.1}", mib(*r as usize)))
        .collect();
    eprintln!(
        "soak/churn: baseline {:.2} MiB, final {:.2} MiB; heap floors {shown:?}; rss {rss_shown:?}",
        mib(baseline),
        mib(last),
    );

    // One leaked ring is megabytes and climbs every round, which is the shape
    // this is built to catch. It is not sensitive to a few bytes per client,
    // and does not pretend to be.
    let growth = last.saturating_sub(baseline);
    assert!(
        growth < 8 * 1024 * 1024,
        "live heap grew {:.1} MiB across {CYCLES} churn cycles: {shown:?}",
        mib(growth),
    );

    // A leak too small to cross that bound in this many rounds still shows up
    // as a floor that only ever rises.
    let half = floors.len() / 2;
    let early: usize = floors[..half].iter().sum::<usize>() / half;
    let late: usize = floors[half..].iter().sum::<usize>() / (floors.len() - half);
    assert!(
        late.saturating_sub(early) < 4 * 1024 * 1024,
        "live heap is still climbing: {:.1} -> {:.1} MiB",
        mib(early),
        mib(late)
    );
}

#[test]
fn the_byte_cap_binds_above_the_sizing_bitrate() {
    let window = Duration::from_secs(15);
    let cap = ring_bytes_for(window, RING_SIZING_BITRATE);
    let ring = Ring::new(cap, window);

    // Twice the rate the cap was sized for, delivered faster than the duration
    // window can evict it: the byte cap is the only thing holding the line.
    let mut generator = TsGenerator::new();
    let chunk = 188 * 1361;
    let chunks = (cap / chunk) * 3;
    for _ in 0..chunks {
        ring.push(generator.bytes(chunk / 188));
        let stats = ring.stats();
        assert!(
            stats.bytes <= cap,
            "ring held {} bytes against a {cap} cap",
            stats.bytes
        );
    }

    // And it is actually full, not merely under the cap by accident.
    assert!(ring.stats().bytes > cap - chunk);
}

#[test]
fn an_oversized_chunk_is_kept_but_does_not_accumulate() {
    // A source that hands over more in one read than the whole cap allows must
    // still be servable — one chunk is retained so a client has something —
    // but the ring must not grow past it.
    let cap = 1024;
    let ring = Ring::new(cap, Duration::from_secs(3600));
    let mut generator = TsGenerator::new();

    for _ in 0..50 {
        ring.push(generator.bytes(100));
        let stats = ring.stats();
        assert_eq!(stats.chunks, 1, "oversized chunks accumulated");
        assert!(stats.bytes > cap, "the chunk really is oversized");
    }
}

#[tokio::test(start_paused = true)]
async fn the_duration_cap_binds_at_or_below_the_sizing_bitrate() {
    let window = Duration::from_secs(15);
    let cap = ring_bytes_for(window, RING_SIZING_BITRATE);
    let ring = Ring::new(cap, window);

    // 8 Mbps, a typical provider stream: a megabyte a second, published four
    // times a second the way the packetizer does.
    let mut generator = TsGenerator::new();
    let chunk = 188 * 1361;
    for _ in 0..(4 * 60) {
        ring.push(generator.bytes(chunk / 188));
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let stats = ring.stats();
    assert!(
        stats.bytes < cap / 2,
        "the byte cap bound at 8 Mbps: {} of {cap}",
        stats.bytes
    );
    // Retention is the window, give or take the chunk that has not aged out.
    assert!(
        (stats.seconds - window.as_secs_f64()).abs() < 1.0,
        "held {:.1}s against a {:?} window",
        stats.seconds,
        window
    );
}
