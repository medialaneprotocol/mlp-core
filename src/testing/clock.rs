//! A deterministic clock for tests.
//!
//! State machines accept an explicit [`Instant`] on every entry point and
//! never read the system clock themselves. A test therefore needs a clock it
//! controls directly, so that a trace is reproducible across runs.

use std::time::{Duration, Instant};

/// A clock the test advances by hand.
#[derive(Debug, Clone, Copy)]
pub struct ManualClock {
    now: Instant,
}

impl ManualClock {
    /// Create a new manual clock starting at `epoch`.
    #[must_use]
    pub fn new(epoch: Instant) -> Self {
        Self { now: epoch }
    }

    /// Advance the clock by `duration`.
    pub fn advance(&mut self, duration: Duration) {
        self.now += duration;
    }

    /// Set the clock to an arbitrary instant.
    pub fn set(&mut self, instant: Instant) {
        self.now = instant;
    }

    /// The current instant, without advancing the clock.
    #[must_use]
    pub fn now(&self) -> Instant {
        self.now
    }
}
