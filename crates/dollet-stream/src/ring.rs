use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::RwLock;
use tokio::time::Instant;

/// `received_at` is what makes "start this client five seconds behind live" a
/// local binary search instead of a sorted set in an external store.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub index: u64,
    pub data: Bytes,
    pub received_at: Instant,
}

/// What a client's cursor found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    Chunk {
        data: Bytes,
        next: u64,
    },
    /// The cursor fell out of the retention window while the client was slow.
    /// It is moved forward rather than the producer being held back.
    Skipped {
        to: u64,
    },
    /// Caught up with the producer; wait on the wakeup channel.
    AtHead,
}

/// Bounded by bytes *and* by duration: the duration cap is what the operator
/// reasons about, the byte cap is what keeps a surprise 30 Mbps source from
/// eating the process.
pub struct Ring {
    inner: RwLock<Inner>,
    max_bytes: usize,
    max_age: Duration,
}

struct Inner {
    chunks: VecDeque<Chunk>,
    bytes: usize,
    /// Index the next pushed chunk will receive. Indices are monotonic for the
    /// life of the session and deliberately survive a stream switch, so a
    /// client's cursor stays valid while the upstream URL changes underneath.
    head: u64,
    total_bytes: u64,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RingStats {
    pub chunks: usize,
    pub bytes: usize,
    pub head: u64,
    pub oldest: Option<u64>,
    pub seconds: f64,
}

impl Ring {
    pub fn new(max_bytes: usize, max_age: Duration) -> Self {
        Self {
            inner: RwLock::new(Inner {
                chunks: VecDeque::new(),
                bytes: 0,
                head: 0,
                total_bytes: 0,
            }),
            max_bytes,
            max_age,
        }
    }

    /// Returns the new head index. Eviction happens here rather than on a
    /// timer, so a session that stops receiving data holds what it has instead
    /// of draining to nothing while clients are still reading it.
    pub fn push(&self, data: Bytes) -> u64 {
        let now = Instant::now();
        let len = data.len();

        let mut inner = self.inner.write();
        let index = inner.head;
        inner.chunks.push_back(Chunk {
            index,
            data,
            received_at: now,
        });
        inner.bytes += len;
        inner.head += 1;
        inner.total_bytes += len as u64;

        let deadline = now.checked_sub(self.max_age);
        while inner.chunks.len() > 1 {
            let front = &inner.chunks[0];
            let too_old = deadline.is_some_and(|d| front.received_at < d);
            if !too_old && inner.bytes <= self.max_bytes {
                break;
            }
            let dropped = inner.chunks.pop_front().expect("len > 1");
            inner.bytes -= dropped.data.len();
        }

        inner.head
    }

    pub fn fetch(&self, cursor: u64) -> Fetched {
        let inner = self.inner.read();
        if cursor >= inner.head {
            return Fetched::AtHead;
        }
        let Some(front) = inner.chunks.front() else {
            return Fetched::Skipped { to: inner.head };
        };
        if cursor < front.index {
            return Fetched::Skipped { to: front.index };
        }
        let chunk = &inner.chunks[(cursor - front.index) as usize];
        Fetched::Chunk {
            data: chunk.data.clone(),
            next: cursor + 1,
        }
    }

    /// Cursor for a client that should start `behind` seconds back. Falls
    /// forward to the head when the ring holds nothing that recent (a stalled
    /// stream) and back to the oldest chunk when the ring is shorter than the
    /// request.
    pub fn cursor_behind(&self, behind: Duration) -> u64 {
        let inner = self.inner.read();
        let Some(target) = Instant::now().checked_sub(behind) else {
            return inner.head;
        };
        let at = inner.chunks.partition_point(|c| c.received_at < target);
        match inner.chunks.get(at) {
            Some(chunk) => chunk.index,
            None => inner.head,
        }
    }

    pub fn head(&self) -> u64 {
        self.inner.read().head
    }

    pub fn total_bytes(&self) -> u64 {
        self.inner.read().total_bytes
    }

    pub fn stats(&self) -> RingStats {
        let inner = self.inner.read();
        let seconds = match (inner.chunks.front(), inner.chunks.back()) {
            (Some(front), Some(back)) => (back.received_at - front.received_at).as_secs_f64(),
            _ => 0.0,
        };
        RingStats {
            chunks: inner.chunks.len(),
            bytes: inner.bytes,
            head: inner.head,
            oldest: inner.chunks.front().map(|c| c.index),
            seconds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(max_bytes: usize, max_age: Duration) -> Ring {
        Ring::new(max_bytes, max_age)
    }

    fn chunk(n: usize) -> Bytes {
        Bytes::from(vec![0xABu8; n])
    }

    #[tokio::test]
    async fn fetch_walks_the_ring_then_reports_head() {
        let r = ring(1 << 20, Duration::from_secs(60));
        r.push(chunk(10));
        r.push(chunk(10));

        assert!(matches!(r.fetch(0), Fetched::Chunk { next: 1, .. }));
        assert!(matches!(r.fetch(1), Fetched::Chunk { next: 2, .. }));
        assert_eq!(r.fetch(2), Fetched::AtHead);
    }

    #[tokio::test]
    async fn empty_ring_is_at_head() {
        let r = ring(1 << 20, Duration::from_secs(60));
        assert_eq!(r.fetch(0), Fetched::AtHead);
        assert_eq!(r.stats().chunks, 0);
        assert_eq!(r.stats().seconds, 0.0);
    }

    #[tokio::test]
    async fn evicts_by_bytes_and_skips_a_cursor_that_fell_out() {
        let r = ring(100, Duration::from_secs(3600));
        for _ in 0..5 {
            r.push(chunk(40));
        }

        let stats = r.stats();
        assert!(stats.bytes <= 100, "{stats:?}");
        assert_eq!(stats.head, 5);
        assert_eq!(stats.oldest, Some(3));
        assert_eq!(r.fetch(0), Fetched::Skipped { to: 3 });
    }

    #[tokio::test(start_paused = true)]
    async fn evicts_by_age() {
        let r = ring(1 << 30, Duration::from_secs(10));
        r.push(chunk(10));
        tokio::time::sleep(Duration::from_secs(6)).await;
        r.push(chunk(10));
        tokio::time::sleep(Duration::from_secs(6)).await;
        r.push(chunk(10));

        // The first chunk is 12 s old and gone; the second is 6 s old and kept.
        assert_eq!(r.stats().oldest, Some(1));
        assert_eq!(r.stats().chunks, 2);
    }

    #[tokio::test]
    async fn always_keeps_the_newest_chunk_even_when_it_alone_exceeds_the_cap() {
        let r = ring(10, Duration::from_secs(3600));
        r.push(chunk(500));
        assert_eq!(r.stats().chunks, 1);
        assert!(matches!(r.fetch(0), Fetched::Chunk { .. }));
    }

    #[tokio::test]
    async fn a_cursor_past_a_fully_evicted_ring_lands_on_the_head() {
        let r = ring(1 << 20, Duration::from_secs(3600));
        r.push(chunk(10));
        {
            // Force the "ring is empty but head has advanced" state that a
            // client can observe between eviction and the next push.
            let mut inner = r.inner.write();
            inner.chunks.clear();
            inner.bytes = 0;
        }
        assert_eq!(r.fetch(0), Fetched::Skipped { to: 1 });
    }

    #[tokio::test(start_paused = true)]
    async fn cursor_behind_binary_searches_by_receive_time() {
        let r = ring(1 << 30, Duration::from_secs(3600));
        for _ in 0..10 {
            r.push(chunk(10));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // Chunk 9 landed 1 s ago, chunk 5 landed 5 s ago.
        assert_eq!(r.cursor_behind(Duration::from_secs(5)), 5);
        assert_eq!(r.cursor_behind(Duration::from_secs(0)), 10);
    }

    #[tokio::test(start_paused = true)]
    async fn cursor_behind_clamps_to_the_oldest_chunk_when_the_ring_is_short() {
        let r = ring(1 << 30, Duration::from_secs(3600));
        r.push(chunk(10));
        tokio::time::sleep(Duration::from_secs(1)).await;
        r.push(chunk(10));

        assert_eq!(r.cursor_behind(Duration::from_secs(600)), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn cursor_behind_starts_live_when_nothing_is_recent() {
        let r = ring(1 << 30, Duration::from_secs(3600));
        r.push(chunk(10));
        tokio::time::sleep(Duration::from_secs(60)).await;

        assert_eq!(r.cursor_behind(Duration::from_secs(5)), 1);
    }

    #[tokio::test]
    async fn stats_track_lifetime_bytes_independently_of_eviction() {
        let r = ring(50, Duration::from_secs(3600));
        for _ in 0..4 {
            r.push(chunk(40));
        }
        assert_eq!(r.total_bytes(), 160);
        assert!(r.stats().bytes < 160);
        assert_eq!(r.head(), 4);
    }
}
