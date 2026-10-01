use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;

/// Per-account provider connection accounting. One process, so a counter here
/// is the whole truth; nothing external has to be consulted or kept in step.
///
/// A slot is held by a *session*, not by a client: one upstream connection
/// serves every client watching that channel, which is the entire point of
/// the ring.
#[derive(Default)]
pub struct ConnectionLimits {
    counts: DashMap<i64, Arc<AtomicU32>>,
}

pub struct LimitGuard {
    slot: Arc<AtomicU32>,
}

impl ConnectionLimits {
    /// `max == 0` means unlimited, matching the provider account field.
    pub fn acquire(&self, key: i64, max: u32) -> Option<LimitGuard> {
        let slot = self
            .counts
            .entry(key)
            .or_insert_with(|| Arc::new(AtomicU32::new(0)))
            .clone();

        if max == 0 {
            slot.fetch_add(1, Ordering::Relaxed);
            return Some(LimitGuard { slot });
        }

        let mut current = slot.load(Ordering::Relaxed);
        loop {
            if current >= max {
                return None;
            }
            match slot.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(LimitGuard { slot }),
                Err(actual) => current = actual,
            }
        }
    }

    #[cfg(test)]
    pub fn in_use(&self, key: i64) -> u32 {
        self.counts
            .get(&key)
            .map_or(0, |slot| slot.load(Ordering::Relaxed))
    }
}

impl Drop for LimitGuard {
    fn drop(&mut self) {
        self.slot.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_the_cap_and_releases_on_drop() {
        let limits = ConnectionLimits::default();

        let a = limits.acquire(7, 2).expect("first");
        let b = limits.acquire(7, 2).expect("second");
        assert!(limits.acquire(7, 2).is_none());
        assert_eq!(limits.in_use(7), 2);

        drop(b);
        assert_eq!(limits.in_use(7), 1);
        let _c = limits.acquire(7, 2).expect("slot freed");
        drop(a);
        assert_eq!(limits.in_use(7), 1);
    }

    #[test]
    fn zero_means_unlimited() {
        let limits = ConnectionLimits::default();
        let guards: Vec<_> = (0..50).map(|_| limits.acquire(1, 0).unwrap()).collect();
        assert_eq!(limits.in_use(1), 50);
        drop(guards);
        assert_eq!(limits.in_use(1), 0);
    }

    #[test]
    fn accounts_are_independent() {
        let limits = ConnectionLimits::default();
        let _a = limits.acquire(1, 1).expect("account 1");
        assert!(limits.acquire(1, 1).is_none());
        assert!(limits.acquire(2, 1).is_some());
        assert_eq!(limits.in_use(3), 0);
    }

    #[test]
    fn concurrent_acquires_never_exceed_the_cap() {
        let limits = Arc::new(ConnectionLimits::default());
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let limits = limits.clone();
                std::thread::spawn(move || limits.acquire(9, 4))
            })
            .collect();

        let granted: Vec<_> = threads
            .into_iter()
            .filter_map(|t| t.join().expect("thread"))
            .collect();
        assert_eq!(granted.len(), 4);
        assert_eq!(limits.in_use(9), 4);
    }
}
