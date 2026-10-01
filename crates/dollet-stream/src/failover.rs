use std::time::Duration;

use tokio::time::Instant;

use crate::config::FailoverConfig;

/// What the session should do next. The caller performs the I/O; this type
/// holds every decision, which is what makes the machine exhaustively testable
/// without a network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Reconnect to the current source after a backoff.
    Retry {
        after: Duration,
    },
    /// Move to `index` in the source list.
    Switch {
        index: usize,
    },
    /// Every source has been tried; sleep, then ask again.
    Wait {
        after: Duration,
    },
    GiveUp(GiveUp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// The channel has no sources at all.
    NoSources,
    /// Nothing worked, and nothing ever had. A channel that has never
    /// delivered a byte is misconfigured or gone, and an honest error now
    /// beats five minutes of keepalive and then an error anyway.
    ColdStart,
}

/// Ordered failover across a channel's sources: a retry counter that decays, a
/// set of sources already tried, a cooldown before wrapping back to the top,
/// and a stability threshold that clears all of it.
///
/// **A session that still has viewers never gives up.** A fixed switch ceiling
/// ends a single-source channel -- the common case -- after three failed
/// reconnects inside a second, turning a ten-second provider blip into a dead
/// channel. No viewer is served by that. The bounds that end
/// a failing session are demand-shaped instead: a client seeing no data hits
/// its keepalive cap and leaves, and a session with no clients is reaped. Both
/// are already enforced elsewhere, and neither can spin forever.
///
/// The one exception is a channel that has never delivered a byte, which is
/// misconfigured rather than unlucky; that fails fast so the viewer gets an
/// error instead of a stall.
///
/// Deliberately pure — every method takes `now` — so the whole machine can be
/// driven from a table of instants with no I/O.
pub struct Failover {
    cfg: FailoverConfig,
    count: usize,
    current: usize,
    tried: Vec<bool>,
    retries: u32,
    last_failure: Option<Instant>,
    rotations: u32,
    switches: u32,
    cooldown_until: Option<Instant>,
    connected_once: bool,
}

impl Failover {
    pub fn new(cfg: FailoverConfig, count: usize) -> Self {
        let mut tried = vec![false; count];
        if let Some(first) = tried.first_mut() {
            *first = true;
        }
        Self {
            cfg,
            count,
            current: 0,
            tried,
            retries: 0,
            last_failure: None,
            rotations: 0,
            switches: 0,
            cooldown_until: None,
            connected_once: false,
        }
    }

    pub fn current(&self) -> usize {
        self.current
    }

    /// Complete wraparounds through the source list, and individual switches.
    /// Both are statistics rather than budgets; they exist so the machine's
    /// bookkeeping can be asserted directly.
    #[cfg(test)]
    pub fn rotations(&self) -> u32 {
        self.rotations
    }

    #[cfg(test)]
    pub fn switches(&self) -> u32 {
        self.switches
    }

    /// An operator picked this source. Deliberately not a switch: it spends no
    /// retry, records no failure, and leaves the pass and switch counts where
    /// they were. A viewer choosing a different source is a preference, not an
    /// error, and the failover budget is for errors.
    ///
    /// The source is marked tried so automatic failover does not immediately
    /// come back to the one that was just moved away from.
    ///
    /// An armed cooldown is cleared, because it describes a situation that no
    /// longer holds: it means "everything was tried and we are backing off",
    /// and someone has just said otherwise. The accumulated pass count is
    /// kept, so an operator picking sources during a real outage does not
    /// reset the engine's patience each time they try one.
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.count {
            return false;
        }
        self.tried[index] = true;
        self.current = index;
        self.retries = 0;
        self.last_failure = None;
        self.cooldown_until = None;
        true
    }

    pub fn on_connected(&mut self) {
        self.connected_once = true;
    }

    /// Sustained playback clears *every* failure budget.
    ///
    /// The retry window decays on time between failures, not on uptime, so
    /// without this a single-source channel that plays flawlessly for ten
    /// minutes between drops still accumulates retries and is dead after
    /// three of them — while a genuinely flapping source never reaches
    /// `stable_threshold` and so never gets here in the first place. Most
    /// channels have exactly one source and providers drop routinely, which
    /// makes this the difference between a two-second gap and a channel that
    /// stays dark.
    pub fn on_stable(&mut self) {
        self.tried.iter_mut().for_each(|t| *t = false);
        if let Some(slot) = self.tried.get_mut(self.current) {
            *slot = true;
        }
        self.retries = 0;
        self.last_failure = None;
        self.rotations = 0;
        self.switches = 0;
        self.cooldown_until = None;
    }

    /// A connection attempt failed, or a working one ended.
    pub fn on_failure(&mut self, now: Instant) -> Action {
        if self
            .last_failure
            .is_some_and(|t| now.saturating_duration_since(t) > self.cfg.retry_window)
        {
            self.retries = 0;
        }
        self.last_failure = Some(now);
        self.retries += 1;

        if self.retries < self.cfg.max_retries {
            let backoff = self
                .cfg
                .retry_backoff_step
                .saturating_mul(self.retries)
                .min(self.cfg.retry_backoff_max);
            return Action::Retry { after: backoff };
        }
        self.try_switch(now)
    }

    /// Called after an `Action::Wait` elapses, and directly by the buffering
    /// detector — which switches without spending retries, because a source
    /// delivering bytes too slowly will not be fixed by reconnecting to it.
    pub fn try_switch(&mut self, now: Instant) -> Action {
        if let Some(index) = self.next_untried() {
            return self.switch_to(index);
        }
        if self.count == 0 {
            return Action::GiveUp(GiveUp::NoSources);
        }
        // Nothing here has ever worked. Fail now rather than holding a viewer
        // on keepalives against a channel that is simply not there.
        if !self.connected_once {
            return Action::GiveUp(GiveUp::ColdStart);
        }

        // Everything has been tried, and this channel has worked before, so it
        // is worth waiting for. Nothing below ever gives up: a session that
        // still has viewers keeps trying, and the bounds that end it are
        // demand-shaped -- a client that sees no data hits its keepalive cap
        // and leaves, and a session with no clients is reaped. See the module
        // header for why a counter is the wrong bound.
        if self.count == 1 {
            // One source, and pausing for a full rotation cooldown would make
            // a ten-second provider blip into a minute of dead air. The capped
            // backoff is already gentle enough at one connection every few
            // seconds.
            return Action::Retry {
                after: self.cfg.retry_backoff_max,
            };
        }
        match self.cooldown_until {
            None => {
                let wait = self.cooldown_after(self.rotations);
                self.rotations += 1;
                // An overflowing deadline means the cooldown is already past,
                // which wraps on the next call rather than never.
                self.cooldown_until = Some(now.checked_add(wait).unwrap_or(now));
                Action::Wait { after: wait }
            }
            Some(until) if now < until => Action::Wait {
                after: until.saturating_duration_since(now),
            },
            Some(_) => {
                self.cooldown_until = None;
                self.tried.iter_mut().for_each(|t| *t = false);
                self.tried[self.current] = true;
                let index = self
                    .next_untried()
                    .expect("more than one source leaves an untried one");
                self.switch_to(index)
            }
        }
    }

    /// Doubles per completed pass. Reset by `on_stable` along with the pass
    /// count, so a channel that plays properly and later has a bad hour starts
    /// its patience over rather than inheriting it.
    fn cooldown_after(&self, passes: u32) -> Duration {
        self.cfg
            .rotation_cooldown_base
            .saturating_mul(1u32 << passes.min(16))
            .min(self.cfg.rotation_cooldown_max)
    }

    fn next_untried(&self) -> Option<usize> {
        (0..self.count).find(|&i| i != self.current && !self.tried[i])
    }

    fn switch_to(&mut self, index: usize) -> Action {
        self.switches += 1;
        self.tried[index] = true;
        self.current = index;
        // A fresh source starts with a fresh retry budget.
        self.retries = 0;
        self.last_failure = None;
        Action::Switch { index }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FailoverConfig {
        FailoverConfig::default()
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn retries_the_current_source_then_switches() {
        let mut f = Failover::new(cfg(), 3);
        let t = now();

        assert_eq!(
            f.on_failure(t),
            Action::Retry {
                after: Duration::from_millis(250)
            }
        );
        assert_eq!(
            f.on_failure(t),
            Action::Retry {
                after: Duration::from_millis(500)
            }
        );
        assert_eq!(f.on_failure(t), Action::Switch { index: 1 });
        assert_eq!(f.current(), 1);
        assert_eq!(f.switches(), 1);
        assert_eq!(f.rotations(), 0, "one switch is not a rotation");
    }

    #[test]
    fn backoff_is_capped() {
        let c = FailoverConfig {
            max_retries: 100,
            retry_backoff_step: Duration::from_secs(2),
            ..cfg()
        };
        let mut f = Failover::new(c, 2);
        let t = now();

        assert_eq!(
            f.on_failure(t),
            Action::Retry {
                after: Duration::from_secs(2)
            }
        );
        assert_eq!(
            f.on_failure(t),
            Action::Retry {
                after: Duration::from_secs(3)
            }
        );
    }

    #[test]
    fn retry_counter_decays_after_the_window() {
        let mut f = Failover::new(cfg(), 2);
        let t = now();
        f.on_failure(t);
        f.on_failure(t);

        // A long quiet period means the next failure counts as the first.
        assert_eq!(
            f.on_failure(t + Duration::from_secs(1801)),
            Action::Retry {
                after: Duration::from_millis(250)
            }
        );
    }

    #[test]
    fn failures_inside_the_window_accumulate() {
        let mut f = Failover::new(cfg(), 2);
        let t = now();
        f.on_failure(t);
        let inside = t + Duration::from_secs(1799);
        assert_eq!(
            f.on_failure(inside),
            Action::Retry {
                after: Duration::from_millis(500)
            }
        );
        assert_eq!(f.on_failure(inside), Action::Switch { index: 1 });
    }

    #[test]
    fn walks_the_whole_list_in_order() {
        let mut f = Failover::new(cfg(), 4);
        let t = now();
        f.on_connected();

        for expected in 1..4 {
            assert_eq!(f.try_switch(t), Action::Switch { index: expected });
        }
        // List exhausted: arm the wrap cooldown rather than switching.
        assert_eq!(
            f.try_switch(t),
            Action::Wait {
                after: Duration::from_secs(5)
            }
        );
    }

    #[test]
    fn a_channel_with_no_sources_at_all_gives_up() {
        let mut f = Failover::new(cfg(), 0);
        f.on_connected();
        // Reaching stability with nothing to be stable on is not reachable in
        // practice; it must still not index past the end.
        f.on_stable();
        assert_eq!(f.try_switch(now()), Action::GiveUp(GiveUp::NoSources));
    }

    #[test]
    fn a_source_that_does_not_exist_cannot_be_selected() {
        // The registry range-checks before it gets here, but the machine is
        // the thing that owns the list and answers for it.
        let mut f = Failover::new(cfg(), 2);
        assert!(!f.select(2));
        assert!(!f.select(usize::MAX));
        assert_eq!(f.current(), 0, "a refused choice moved the session");
        assert!(f.select(1));
        assert_eq!(f.current(), 1);
    }

    #[test]
    fn a_lone_source_that_never_worked_fails_fast() {
        // Nothing to rotate to and nothing ever delivered: an honest error now
        // is better for the viewer than a stall that ends in one anyway.
        let mut f = Failover::new(cfg(), 1);
        assert_eq!(f.try_switch(now()), Action::GiveUp(GiveUp::ColdStart));
    }

    #[test]
    fn cold_start_does_not_wrap() {
        let mut f = Failover::new(cfg(), 2);
        let t = now();
        assert_eq!(f.try_switch(t), Action::Switch { index: 1 });
        assert_eq!(f.try_switch(t), Action::GiveUp(GiveUp::ColdStart));
    }

    #[test]
    fn the_wrap_cooldown_grows_with_the_evidence_for_an_outage() {
        // One failed pass could be a single bad moment catching every source;
        // five is a provider having an hour. Waiting the same for both means
        // waiting the long one, and a viewer pays that for every glitch.
        let c = FailoverConfig {
            rotation_cooldown_base: Duration::from_secs(5),
            rotation_cooldown_max: Duration::from_secs(60),
            ..cfg()
        };
        let mut f = Failover::new(c, 2);
        let mut t = now();
        f.on_connected();

        let mut waits = Vec::new();
        for _ in 0..7 {
            assert!(matches!(f.try_switch(t), Action::Switch { .. }));
            let Action::Wait { after } = f.try_switch(t) else {
                panic!("expected a cooldown");
            };
            waits.push(after.as_secs());
            t += after + Duration::from_secs(1);
        }
        assert_eq!(waits, vec![5, 10, 20, 40, 60, 60, 60]);

        // Sustained playback starts the patience over.
        f.on_stable();
        assert!(matches!(f.try_switch(t), Action::Switch { .. }));
        assert_eq!(
            f.try_switch(t),
            Action::Wait {
                after: Duration::from_secs(5)
            }
        );
    }

    #[test]
    fn wrap_waits_out_the_cooldown_then_restarts_the_list() {
        let mut f = Failover::new(cfg(), 2);
        let t = now();
        f.on_connected();
        assert_eq!(f.try_switch(t), Action::Switch { index: 1 });
        assert_eq!(
            f.try_switch(t),
            Action::Wait {
                after: Duration::from_secs(5)
            }
        );

        // Still cooling: report the remaining time, do not re-arm.
        assert_eq!(
            f.try_switch(t + Duration::from_secs(2)),
            Action::Wait {
                after: Duration::from_secs(3)
            }
        );

        assert_eq!(
            f.try_switch(t + Duration::from_secs(6)),
            Action::Switch { index: 0 }
        );
    }

    #[test]
    fn an_exhausted_list_keeps_rotating_for_as_long_as_it_is_wanted() {
        // A fixed switch ceiling is a counter deciding whether a viewer is
        // still watching when the provider comes back, and no counter knows
        // the answer -- so there isn't one. The
        // session ends when nobody is left to serve, which is enforced by the
        // keepalive cap and the reaper, not here.
        let c = FailoverConfig {
            rotation_cooldown_base: Duration::from_secs(5),
            rotation_cooldown_max: Duration::from_secs(5),
            ..cfg()
        };
        let mut f = Failover::new(c, 4);
        let mut t = now();
        f.on_connected();

        for pass in 1..=20 {
            for _ in 0..3 {
                assert!(
                    matches!(f.try_switch(t), Action::Switch { .. }),
                    "gave up during pass {pass}"
                );
            }
            assert!(
                matches!(f.try_switch(t), Action::Wait { .. }),
                "gave up rather than waiting out pass {pass}"
            );
            t += Duration::from_secs(6);
        }
        assert_eq!(f.rotations(), 20);
        assert_eq!(f.switches(), 60);
    }

    #[test]
    fn a_lone_source_that_has_worked_is_retried_rather_than_abandoned() {
        // The shape that matters most here: most channels
        // have exactly one source. Three failed reconnects inside a second
        // must not end the channel, and the pause between attempts must not be
        // a full rotation cooldown either -- a ten-second blip would become a
        // minute of dead air.
        let c = FailoverConfig {
            rotation_cooldown_base: Duration::from_secs(60),
            rotation_cooldown_max: Duration::from_secs(60),
            retry_backoff_max: Duration::from_secs(3),
            ..cfg()
        };
        let mut f = Failover::new(c, 1);
        let t = now();
        f.on_connected();

        for _ in 0..50 {
            assert_eq!(
                f.try_switch(t),
                Action::Retry {
                    after: Duration::from_secs(3)
                }
            );
        }
    }

    #[test]
    fn a_single_source_channel_survives_drops_it_recovers_from() {
        // The regression that matters most here: most
        // channels have one source, and a provider dropping every few minutes
        // is routine. Ten minutes of clean playback between drops is well
        // inside the 30-minute retry window, so only on_stable can refill the
        // budget -- without it the third drop is fatal and every client goes.
        let mut f = Failover::new(cfg(), 1);
        let mut t = now();

        for drop in 0..10 {
            f.on_connected();
            f.on_stable();
            t += Duration::from_secs(600);
            assert_eq!(
                f.on_failure(t),
                Action::Retry {
                    after: Duration::from_millis(250)
                },
                "gave up on drop {drop} after clean playback"
            );
        }
    }

    #[test]
    fn stability_clears_every_failure_budget() {
        let mut f = Failover::new(cfg(), 3);
        let t = now();
        f.on_connected();
        f.on_failure(t);
        f.on_failure(t);
        assert_eq!(f.on_failure(t), Action::Switch { index: 1 });
        f.on_failure(t);

        f.on_stable();
        assert_eq!(f.rotations(), 0);
        // Source 0 is eligible again, and the retry budget is whole.
        assert_eq!(
            f.on_failure(t),
            Action::Retry {
                after: Duration::from_millis(250)
            }
        );
        assert_eq!(f.try_switch(t), Action::Switch { index: 0 });
    }

    #[test]
    fn a_switch_resets_the_retry_budget() {
        let mut f = Failover::new(cfg(), 2);
        let t = now();
        f.on_failure(t);
        f.on_failure(t);
        assert_eq!(f.on_failure(t), Action::Switch { index: 1 });

        // The new source gets all three attempts of its own.
        assert!(matches!(f.on_failure(t), Action::Retry { .. }));
        assert!(matches!(f.on_failure(t), Action::Retry { .. }));
        assert_eq!(f.on_failure(t), Action::GiveUp(GiveUp::ColdStart));
    }

    #[test]
    fn a_buffering_switch_does_not_consume_retries() {
        let mut f = Failover::new(cfg(), 2);
        let t = now();
        f.on_connected();

        assert_eq!(f.try_switch(t), Action::Switch { index: 1 });
        assert!(matches!(f.on_failure(t), Action::Retry { .. }));
    }
}
