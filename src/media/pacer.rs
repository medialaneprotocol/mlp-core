//! Media-lane pacing (MLP spec §8.4, D-3 rule 3): one token bucket plus a
//! bounded 1-second window, with a reserved floor for control traffic.
//!
//! The pacer is the only authority for *when* a fragment may leave. The sender
//! asks, the pacer grants or reports how long to wait; nothing here reads the
//! clock except through the injected [`Instant`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::pdu::{Carrier, TrafficClass};

/// Maximum bytes a stream carrier may release in one poll (MLP spec §8.4).
pub const STREAM_RELEASE_MAX: u64 = 64 * 1024;
/// Reserved-rate floor: `reserved_rate = max(2 kbps, 10 % × pace)` (MLP spec annex D, D-3).
pub const RESERVED_RATE_FLOOR_BPS: u64 = 2_000;
/// Reserved-rate fraction of the pace.
pub const RESERVED_RATE_FRACTION: f64 = 0.10;
/// The reserve horizon floor: `max(1 × RTT, 250 ms)` (MLP spec §4.2 rule 3).
pub const RESERVE_HORIZON_FLOOR: Duration = Duration::from_millis(250);
/// The window over which the burst ratio is enforced (MLP spec annex INV, INV-7).
pub const PACING_WINDOW: Duration = Duration::from_secs(1);
/// The most the pacer may hand over in a window, as a multiple of the rate.
pub const BURST_ALLOWANCE_RATIO: f64 = 1.25;
/// The largest single grant the pacer admits, as a fragment cap.
///
/// The window budget is `max(BURST_ALLOWANCE_RATIO × rate, one fragment)`: the
/// second term is what keeps a single legal fragment from deadlocking a slow
/// lane forever, and this cap is what bounds that carve-out. A media object
/// above this size is segmented by the sender before it reaches the pacer.
pub const MAX_GRANT_BYTES: u64 = 16 * 1024;

/// Pacer configuration. Every value is a policy constant of MLP spec §8.4.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PacerConfig {
    /// Current pace in bits per second.
    pub pace_bps: u64,
    /// The largest object the sender may emit.
    pub max_object_bytes: u32,
    /// Reserved-rate floor.
    pub reserved_floor_bps: u64,
    /// Reserved fraction of the pace.
    pub reserved_fraction: f64,
    /// The window the burst allowance is measured over.
    pub window: Duration,
    /// Burst allowance as a multiple of the rate.
    pub burst_allowance_ratio: f64,
    /// The largest single grant (one fragment on this carrier).
    pub max_grant_bytes: u64,
}

impl PacerConfig {
    /// A configuration for a carrier with the given object cap.
    pub fn for_carrier(carrier: Carrier, pace_bps: u64, max_object_bytes: u32) -> Self {
        Self {
            pace_bps,
            max_object_bytes,
            reserved_floor_bps: RESERVED_RATE_FLOOR_BPS,
            reserved_fraction: RESERVED_RATE_FRACTION,
            window: PACING_WINDOW,
            burst_allowance_ratio: BURST_ALLOWANCE_RATIO,
            max_grant_bytes: (carrier.fragment_payload_max() as u64).min(MAX_GRANT_BYTES),
        }
    }
}

impl Default for PacerConfig {
    fn default() -> Self {
        Self::for_carrier(Carrier::Ws, 8_000, 8_192)
    }
}

/// A deterministic token bucket: `capacity` bytes, refilled at a byte rate.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a bucket that starts full.
    pub fn new(capacity: f64, refill_per_sec: f64, now: Instant) -> Self {
        Self {
            capacity: capacity.max(0.0),
            refill_per_sec: refill_per_sec.max(0.0),
            tokens: capacity.max(0.0),
            last_refill: now,
        }
    }

    /// Refill according to the elapsed time.
    pub fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }

    /// Current tokens.
    pub fn tokens(&self) -> f64 {
        self.tokens
    }

    /// Bucket capacity.
    pub fn capacity(&self) -> f64 {
        self.capacity
    }

    /// Byte rate.
    pub fn refill_per_sec(&self) -> f64 {
        self.refill_per_sec
    }

    /// Change the refill rate, keeping the current tokens.
    pub fn set_refill(&mut self, refill_per_sec: f64, now: Instant) {
        self.refill(now);
        self.refill_per_sec = refill_per_sec.max(0.0);
    }

    /// Change the capacity, clamping the tokens.
    pub fn set_capacity(&mut self, capacity: f64, now: Instant) {
        self.refill(now);
        self.capacity = capacity.max(0.0);
        self.tokens = self.tokens.min(self.capacity);
    }

    /// Consume `amount` when available.
    pub fn try_consume(&mut self, amount: f64) -> bool {
        if self.tokens + f64::EPSILON >= amount {
            self.tokens -= amount;
            true
        } else {
            false
        }
    }

    /// How long until `amount` bytes are available.
    pub fn time_until(&self, amount: f64) -> Duration {
        let deficit = amount - self.tokens;
        if deficit <= 0.0 {
            return Duration::ZERO;
        }
        if self.refill_per_sec <= 0.0 {
            return Duration::from_secs(u64::MAX / 4);
        }
        Duration::from_secs_f64(deficit / self.refill_per_sec)
    }

    /// Tokens above an absolute floor, saturating at zero.
    pub fn available_above(&self, floor: f64) -> f64 {
        (self.tokens - floor).max(0.0)
    }
}

/// Why a grant was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The token bucket is short.
    Tokens,
    /// The 1-second window budget is short.
    Window,
    /// Granting would starve the reserved control floor.
    Reserved,
}

/// The pacer's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireResult {
    /// The bytes may be written now.
    Granted,
    /// Wait at least this long, then ask again.
    Wait {
        /// The wait.
        wait: Duration,
        /// Why the wait is needed.
        reason: DenyReason,
    },
    /// The request is larger than one fragment may be; the sender must
    /// segment it. This is never a wait: waiting cannot make it legal.
    Oversize {
        /// The largest grant the pacer will make.
        max_grant_bytes: u64,
    },
}

impl AcquireResult {
    /// Whether the grant was immediate.
    pub fn is_granted(self) -> bool {
        matches!(self, Self::Granted)
    }

    /// The wait, zero when granted.
    pub fn wait(self) -> Duration {
        match self {
            Self::Granted | Self::Oversize { .. } => Duration::ZERO,
            Self::Wait { wait, .. } => wait,
        }
    }

    /// The deny reason, when refused with a wait.
    pub fn reason(self) -> Option<DenyReason> {
        match self {
            Self::Granted | Self::Oversize { .. } => None,
            Self::Wait { reason, .. } => Some(reason),
        }
    }

    /// Whether the request must be segmented before retrying.
    pub fn is_oversize(self) -> bool {
        matches!(self, Self::Oversize { .. })
    }
}

/// The media-lane pacer: a token bucket plus the burst window and the
/// reserved control floor.
#[derive(Debug, Clone)]
pub struct MediaPacer {
    config: PacerConfig,
    bucket: TokenBucket,
    window: VecDeque<(Instant, u64)>,
    rtt: Duration,
}

impl MediaPacer {
    /// Create a pacer for the given rate and object cap.
    pub fn new(pace_bps: u64, max_object_bytes: u32, now: Instant) -> Self {
        Self::with_config(
            PacerConfig::for_carrier(Carrier::Ws, pace_bps, max_object_bytes),
            now,
        )
    }

    /// Create a pacer with an explicit configuration.
    pub fn with_config(config: PacerConfig, now: Instant) -> Self {
        let bucket = Self::fresh_bucket(&config, now);
        Self {
            config,
            bucket,
            window: VecDeque::new(),
            rtt: Duration::ZERO,
        }
    }

    /// A token bucket dimensioned for a configuration and starting full.
    fn fresh_bucket(config: &PacerConfig, now: Instant) -> TokenBucket {
        let rate = config.pace_bps as f64 / 8.0;
        let capacity = (2.0 * config.max_object_bytes as f64)
            .max(rate * Duration::from_millis(20).as_secs_f64());
        let allowance =
            (rate * config.window.as_secs_f64() * config.burst_allowance_ratio).round() as u64;
        let budget = allowance.max(config.max_grant_bytes);
        TokenBucket::new(capacity.min(budget as f64), rate, now)
    }

    /// The configuration in force.
    pub fn config(&self) -> &PacerConfig {
        &self.config
    }

    /// The current pace.
    pub fn pace_bps(&self) -> u64 {
        self.config.pace_bps
    }

    /// The current object cap.
    pub fn max_object_bytes(&self) -> u32 {
        self.config.max_object_bytes
    }

    /// Update the pace and object cap (from the estimator).
    pub fn set_pace(&mut self, pace_bps: u64, max_object_bytes: u32, now: Instant) {
        self.bucket.refill(now);
        self.config.pace_bps = pace_bps.max(1);
        self.config.max_object_bytes = max_object_bytes;
        self.reconfigure(now);
    }

    /// Update the RTT used for the reserve horizon.
    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    /// The reserved control rate: `max(2 kbps, 10 % × pace)`.
    pub fn reserved_rate_bps(&self) -> u64 {
        (self.config.pace_bps as f64 * self.config.reserved_fraction)
            .max(self.config.reserved_floor_bps as f64)
            .round() as u64
    }

    /// The reserve watermark media may never dip below, in bytes.
    pub fn reserve_watermark_bytes(&self) -> u64 {
        let horizon = self.rtt.max(RESERVE_HORIZON_FLOOR);
        (self.reserved_rate_bps() as f64 / 8.0 * horizon.as_secs_f64()).round() as u64
    }

    /// Refill and return the current token count.
    pub fn refill(&mut self, now: Instant) -> u64 {
        self.bucket.refill(now);
        self.purge_window(now);
        self.bucket.tokens() as u64
    }

    /// Tokens currently available.
    pub fn tokens(&self) -> u64 {
        self.bucket.tokens() as u64
    }

    /// Bytes the pacer handed over inside the rolling window.
    pub fn window_bytes(&self, now: Instant) -> u64 {
        let cutoff = now.checked_sub(self.config.window).unwrap_or(now);
        self.window
            .iter()
            .filter(|(at, _)| *at > cutoff)
            .map(|(_, bytes)| *bytes)
            .sum()
    }

    /// The window budget: `max(1.25 × rate, one fragment)`.
    ///
    /// The second term is what keeps a single fragment from deadlocking the
    /// lane when it is larger than the ratio allowance; it is the only case in
    /// which a 1-second window may exceed the ratio, and the sender never
    /// emits a fragment above `max_grant_bytes`.
    pub fn window_budget_bytes(&self) -> u64 {
        let rate = self.config.pace_bps as f64 / 8.0;
        let allowance = rate * self.config.window.as_secs_f64() * self.config.burst_allowance_ratio;
        (allowance.round() as u64).max(self.config.max_grant_bytes)
    }

    /// The largest grant the pacer will ever make in one release.
    pub fn max_grant_bytes(&self) -> u64 {
        self.config.max_grant_bytes
    }

    /// Whether `bytes` may be handed over now.
    pub fn can_acquire(&self, class: TrafficClass, bytes: u64) -> bool {
        matches!(self.check(class, bytes), AcquireResult::Granted)
    }

    /// Try to acquire `bytes` for `class`; on success the tokens and the window
    /// budget are consumed.
    pub fn try_acquire(&mut self, class: TrafficClass, bytes: u64, now: Instant) -> AcquireResult {
        self.bucket.refill(now);
        self.purge_window(now);
        match self.check(class, bytes) {
            AcquireResult::Granted => {
                let granted = self.bucket.try_consume(bytes as f64);
                debug_assert!(granted, "check said the tokens were there");
                self.window.push_back((now, bytes));
                AcquireResult::Granted
            }
            refusal => refusal,
        }
    }

    /// The wait until `bytes` could be granted, without consuming anything.
    pub fn time_until(&self, class: TrafficClass, bytes: u64, now: Instant) -> Duration {
        let _ = now;
        match self.check(class, bytes) {
            AcquireResult::Granted => Duration::ZERO,
            AcquireResult::Oversize { .. } => Duration::ZERO,
            AcquireResult::Wait { wait, .. } => wait,
        }
    }

    /// How many bytes may be released right now for a carrier.
    ///
    /// Stream carriers release up to 64 KiB per poll; datagram carriers
    /// release exactly one fragment's worth (MLP spec §8.4).
    pub fn release_budget(&self, carrier: Carrier, class: TrafficClass) -> u64 {
        let tokens = self.bucket.tokens().max(0.0) as u64;
        let usable = if class.is_media() {
            tokens.saturating_sub(self.reserve_watermark_bytes())
        } else {
            tokens
        };
        match carrier {
            Carrier::Ws | Carrier::Grpc => usable.min(STREAM_RELEASE_MAX),
            _ => usable.min(carrier.fragment_payload_max() as u64),
        }
    }

    fn check(&self, class: TrafficClass, bytes: u64) -> AcquireResult {
        if bytes > self.config.max_grant_bytes {
            // One fragment must never exceed the carrier cap; a larger request
            // is the sender's segmentation bug, not a pacing problem.
            return AcquireResult::Oversize {
                max_grant_bytes: self.config.max_grant_bytes,
            };
        }
        let window_budget = self.window_budget_bytes();
        let window_used: u64 = self.window.iter().map(|(_, bytes)| *bytes).sum();
        if window_used + bytes > window_budget {
            let rate = (self.config.pace_bps as f64 / 8.0).max(1.0);
            let deficit = (window_used + bytes).saturating_sub(window_budget);
            return AcquireResult::Wait {
                wait: Duration::from_secs_f64(deficit as f64 / rate),
                reason: DenyReason::Window,
            };
        }
        let watermark = if class.is_media() {
            self.reserve_watermark_bytes() as f64
        } else {
            0.0
        };
        if self.bucket.available_above(watermark) + f64::EPSILON < bytes as f64 {
            let token_wait = self.bucket.time_until(bytes as f64 + watermark);
            if class.is_media() {
                return AcquireResult::Wait {
                    wait: token_wait,
                    reason: DenyReason::Reserved,
                };
            }
            return AcquireResult::Wait {
                wait: self.bucket.time_until(bytes as f64),
                reason: DenyReason::Tokens,
            };
        }
        AcquireResult::Granted
    }

    fn purge_window(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.config.window).unwrap_or(now);
        while let Some((at, _)) = self.window.front() {
            if *at <= cutoff {
                self.window.pop_front();
            } else {
                break;
            }
        }
    }

    fn reconfigure(&mut self, now: Instant) {
        let rate = self.config.pace_bps as f64 / 8.0;
        let capacity = (2.0 * self.config.max_object_bytes as f64)
            .max(rate * Duration::from_millis(20).as_secs_f64());
        // Keep the tokens inside the 1-second window budget so a pace change
        // can never hand the carrier a burst the window would refuse.
        self.bucket
            .set_capacity(capacity.min(self.window_budget_bytes() as f64), now);
        self.bucket.set_refill(rate, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn token_bucket_refills_at_the_rate_and_caps_at_capacity() {
        let start = now();
        let mut bucket = TokenBucket::new(1_000.0, 500.0, start);
        assert_eq!(bucket.tokens(), 1_000.0);
        assert!(bucket.try_consume(1_000.0));
        assert!(!bucket.try_consume(1.0));
        bucket.refill(start + Duration::from_secs(1));
        assert_eq!(bucket.tokens(), 500.0);
        bucket.refill(start + Duration::from_secs(10));
        assert_eq!(bucket.tokens(), 1_000.0, "capacity caps the refill");
    }

    #[test]
    fn reserved_rate_is_the_documented_floor() {
        let start = now();
        let slow = MediaPacer::new(8_000, 1_024, start);
        assert_eq!(slow.reserved_rate_bps(), 2_000, "floor wins at low pace");
        let fast = MediaPacer::new(1_000_000, 32_768, start);
        assert_eq!(fast.reserved_rate_bps(), 100_000, "10 % above the floor");
    }

    #[test]
    fn media_cannot_dip_into_the_reserve() {
        let start = now();
        let mut pacer = MediaPacer::with_config(
            PacerConfig {
                max_grant_bytes: 1_150,
                ..PacerConfig::for_carrier(Carrier::DataChannel, 8_000, 1_024)
            },
            start,
        );
        pacer.set_rtt(Duration::from_millis(1_000));
        // Watermark: max(2 kbps, 10 % × 8 kbps) / 8 × 1 s = 250 bytes.
        assert_eq!(pacer.reserve_watermark_bytes(), 250);
        // Drain the bucket down to the watermark: 1 250 − 1 000 = 250.
        assert!(
            pacer
                .try_acquire(TrafficClass::Media, 1_000, start)
                .is_granted()
        );
        assert_eq!(pacer.tokens(), 250);
        // Media may not touch the reserve ...
        assert!(!pacer.can_acquire(TrafficClass::Media, 1));
        assert!(pacer.time_until(TrafficClass::Media, 1, start) > Duration::ZERO);
        // ... but control may borrow it.
        assert!(pacer.can_acquire(TrafficClass::Control, 100));
        assert!(
            pacer
                .try_acquire(TrafficClass::Control, 100, start)
                .is_granted()
        );
        assert_eq!(pacer.tokens(), 150);
    }

    #[test]
    fn control_borrows_from_media_never_the_reverse() {
        let start = now();
        let mut pacer = MediaPacer::new(64_000, 8_192, start);
        pacer.set_rtt(Duration::from_millis(500));
        // Watermark at 64 kbps: 6 400 bps / 8 × 0.5 s = 400 bytes.
        assert_eq!(pacer.reserve_watermark_bytes(), 400);
        let budget = pacer.release_budget(Carrier::Grpc, TrafficClass::Media);
        let tokens = pacer.tokens();
        assert_eq!(budget, tokens.saturating_sub(400).min(STREAM_RELEASE_MAX));
        // Control may use the whole bucket.
        let control_budget = pacer.release_budget(Carrier::Grpc, TrafficClass::Control);
        assert!(control_budget >= budget);
    }

    #[test]
    fn window_limits_a_burst_to_the_alias_allowance() {
        let start = now();
        let config = PacerConfig {
            pace_bps: 8_000,
            max_object_bytes: 1_024,
            max_grant_bytes: 1_150,
            ..PacerConfig::default()
        };
        let mut pacer = MediaPacer::with_config(config, start);
        // Rate is 1 000 B/s; the window budget is max(1 250, 1 150) = 1 250.
        assert_eq!(pacer.window_budget_bytes(), 1_250);
        let mut granted = 0u64;
        for _ in 0..20 {
            if pacer
                .try_acquire(TrafficClass::Control, 1_000, start)
                .is_granted()
            {
                granted += 1_000;
            }
        }
        assert_eq!(granted, 1_000, "one grant per window budget");
        // After the window slides, the next grant is allowed again.
        let later = start + Duration::from_millis(1_001);
        assert!(
            pacer
                .try_acquire(TrafficClass::Control, 1_000, later)
                .is_granted()
        );
    }

    #[test]
    fn burst_accounting_matches_the_token_bucket() {
        let start = now();
        let mut pacer = MediaPacer::with_config(
            PacerConfig {
                max_grant_bytes: 16_384,
                ..PacerConfig::for_carrier(Carrier::Grpc, 100_000, 8_192)
            },
            start,
        );
        let mut sent = 0u64;
        let mut at = start;
        for _ in 0..50 {
            if pacer
                .try_acquire(TrafficClass::Media, 1_000, at)
                .is_granted()
            {
                sent += 1_000;
            }
            at += Duration::from_millis(100);
        }
        // 5 seconds at 12 500 B/s minus the untouched reserve.
        assert!(sent >= 50_000, "sent {sent}");
        assert!(sent <= 12_500 * 5, "sent {sent} above the rate");
        let window = pacer.window_bytes(at);
        assert!(window <= pacer.window_budget_bytes());
    }

    #[test]
    fn oversize_requests_are_refused_without_consuming() {
        let start = now();
        let mut pacer = MediaPacer::new(8_000, 1_024, start);
        let before = pacer.tokens();
        let result = pacer.try_acquire(TrafficClass::Media, 2_000_000, start);
        assert!(result.is_oversize());
        assert_eq!(result.reason(), None);
        assert_eq!(pacer.tokens(), before);
    }

    #[test]
    fn release_budget_is_one_fragment_on_datagram_and_64k_on_stream() {
        let start = now();
        let mut pacer = MediaPacer::new(10_000_000, 524_288, start);
        pacer.set_rtt(Duration::from_millis(10));
        let stream = pacer.release_budget(Carrier::Grpc, TrafficClass::Media);
        assert!(stream <= STREAM_RELEASE_MAX);
        let mut small = MediaPacer::with_config(
            PacerConfig::for_carrier(Carrier::DataChannel, 10_000_000, 8_192),
            start,
        );
        small.set_rtt(Duration::from_millis(10));
        assert!(small.release_budget(Carrier::DataChannel, TrafficClass::Media) <= 1_150);
    }

    #[test]
    fn pace_change_is_reflected_immediately() {
        let start = now();
        let mut pacer = MediaPacer::new(8_000, 1_024, start);
        assert_eq!(pacer.pace_bps(), 8_000);
        pacer.set_pace(1_000_000, 32_768, start);
        assert_eq!(pacer.pace_bps(), 1_000_000);
        assert_eq!(pacer.reserved_rate_bps(), 100_000);
        assert!(pacer.window_budget_bytes() >= 125_000);
    }

    #[test]
    fn capacity_is_at_least_two_objects_or_twenty_ms_of_rate() {
        let start = now();
        let pacer = MediaPacer::new(8_000, 8_192, start);
        // rate = 1 000 B/s, 20 ms = 20 B; 2 × max_object = 16 384 wins.
        assert_eq!(pacer.tokens(), 16_384);
        assert!(pacer.tokens() <= pacer.window_budget_bytes());
        // The window budget is the ratio allowance unless one fragment is
        // larger than it (the documented carve-out).
        let tight = MediaPacer::with_config(
            PacerConfig {
                max_grant_bytes: 1_150,
                ..PacerConfig::for_carrier(Carrier::DataChannel, 8_000, 1_024)
            },
            start,
        );
        assert_eq!(tight.window_budget_bytes(), 1_250);
        assert_eq!(tight.tokens(), 1_250, "capacity is capped by the window");
    }

    #[test]
    fn a_closed_loop_pacer_never_exceeds_the_window_ratio_invoc_7() {
        let start = now();
        let mut pacer = MediaPacer::new(32_000, 8_000, start);
        pacer.set_rtt(Duration::from_millis(800));
        // Emulate a sender that always asks for the largest legal fragment.
        let mut at = start;
        let mut total = 0u64;
        for _ in 0..2_000 {
            if pacer
                .try_acquire(TrafficClass::Media, 1_150, at)
                .is_granted()
            {
                total += 1_150;
                at += Duration::from_millis(1);
            } else {
                at += Duration::from_millis(10);
            }
        }
        let elapsed = at.duration_since(start).as_secs_f64();
        let allowed = 32_000.0 / 8.0 * elapsed * 1.25 + pacer.max_grant_bytes() as f64;
        assert!(
            total as f64 <= allowed,
            "sent {total} in {elapsed:.2}s, allowed {allowed:.0}"
        );
    }
}
