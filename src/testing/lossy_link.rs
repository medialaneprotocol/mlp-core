//! Deterministic risky-link simulator (MLP spec §12).
//!
//! [`LossyLink`] carries [`Transmits`](crate::Transmit) between two endpoints
//! with per-direction rate limits, propagation delay, jitter, loss (uniform or
//! bursty), reordering, duplication, MTU enforcement and scheduled outages.
//! Every random draw comes from an in-crate PCG64 stream seeded by the profile,
//! so a scenario replays byte-identically on every machine: this is what makes
//! the L2 suites gates rather than reports.

use serde::{Deserialize, Serialize};

/// The T0–T4 target tiers (MLP spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// LAN: ≥100 Mbps / ≤10 ms / no loss.
    T0,
    /// Broadband: ≥5 Mbps / 50 ms / ≤0.1 %.
    T1,
    /// Poor Wi-Fi or hotspot: ≥1 Mbps / 150 ms / ≤2 %.
    T2,
    /// Long-haul degraded: 256–512 kbps / 400 ms / 5–10 %.
    T3,
    /// Extreme: 32–64 kbps / 800–2000 ms / 20–50 %.
    T4,
}

impl Tier {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::T0 => "t0",
            Self::T1 => "t1",
            Self::T2 => "t2",
            Self::T3 => "t3",
            Self::T4 => "t4",
        }
    }

    /// All tiers, in order.
    pub const fn all() -> [Tier; 5] {
        [Tier::T0, Tier::T1, Tier::T2, Tier::T3, Tier::T4]
    }

    /// The nominal one-way delay the tier's RTT implies.
    pub fn nominal_rtt_ms(self) -> f64 {
        match self {
            Self::T0 => 10.0,
            Self::T1 => 50.0,
            Self::T2 => 150.0,
            Self::T3 => 400.0,
            Self::T4 => 1_400.0,
        }
    }
}

/// The jitter distribution (MLP spec §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum JitterDist {
    /// Gaussian via Box–Muller.
    #[default]
    Normal,
    /// Pareto with `alpha = 1.5`: the heavy tail that models radio.
    Pareto,
}

/// A scheduled outage window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outage {
    /// Start, in scenario milliseconds.
    pub start_ms: u64,
    /// Duration.
    pub dur_ms: u64,
}

impl Outage {
    /// Whether `at_ms` falls inside the outage.
    pub fn covers(&self, at_ms: u64) -> bool {
        at_ms >= self.start_ms && at_ms < self.start_ms + self.dur_ms
    }
}

/// The link profile (MLP spec §12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkProfile {
    /// Round-trip time in ms.
    pub rtt_ms: f64,
    /// Jitter amplitude in ms.
    pub jitter_ms: f64,
    /// The jitter distribution.
    pub jitter_dist: JitterDist,
    /// Loss in permille of transmissions.
    pub loss_permille: u32,
    /// Mean burst length when loss is bursty.
    pub burst_loss: Option<u32>,
    /// Reordering in permille (extra delay on the reordered message).
    pub reorder_permille: u32,
    /// Duplication in permille.
    pub duplicate_permille: u32,
    /// Downstream rate (sender → receiver), bits per second.
    pub rate_bps_down: u64,
    /// Upstream rate (receiver → sender), bits per second.
    pub rate_bps_up: u64,
    /// The carrier's payload budget: a datagram carrier's MTU (1 150) or a
    /// stream carrier's message cap. The MLP fixed header and header extension
    /// travel on top of it, so [`LinkProfile::admits`] is the message test.
    pub mtu_bytes: u32,
    /// Scheduled outages.
    pub outage_schedule: Vec<Outage>,
    /// Extra delay applied to the reverse direction (feedback).
    pub ack_delay_ms: u64,
    /// The RNG seed.
    pub seed: u64,
}

impl Default for LinkProfile {
    fn default() -> Self {
        Self::tier(Tier::T0)
    }
}

impl LinkProfile {
    /// The profile of a tier, exactly as MLP spec §3.1 writes it.
    ///
    /// Rates use the tier's floor; RTT uses its ceiling band; loss its upper
    /// bound (the simulator asserts at the worst case on purpose).
    pub fn tier(tier: Tier) -> Self {
        match tier {
            Tier::T0 => Self {
                rtt_ms: 10.0,
                jitter_ms: 2.0,
                rate_bps_down: 100_000_000,
                rate_bps_up: 10_000_000,
                mtu_bytes: 65_536,
                loss_permille: 0,
                ..Self::baseline()
            },
            Tier::T1 => Self {
                rtt_ms: 50.0,
                jitter_ms: 10.0,
                rate_bps_down: 5_000_000,
                rate_bps_up: 1_000_000,
                mtu_bytes: 65_536,
                loss_permille: 1,
                ..Self::baseline()
            },
            Tier::T2 => Self {
                rtt_ms: 150.0,
                jitter_ms: 40.0,
                rate_bps_down: 1_000_000,
                rate_bps_up: 256_000,
                mtu_bytes: 65_536,
                loss_permille: 20,
                ..Self::baseline()
            },
            Tier::T3 => Self {
                rtt_ms: 400.0,
                jitter_ms: 100.0,
                rate_bps_down: 256_000,
                rate_bps_up: 128_000,
                mtu_bytes: 65_536,
                loss_permille: 100,
                ..Self::baseline()
            },
            Tier::T4 => Self {
                rtt_ms: 1_400.0,
                jitter_ms: 500.0,
                jitter_dist: JitterDist::Pareto,
                rate_bps_down: 32_000,
                rate_bps_up: 32_000,
                mtu_bytes: 1_150,
                loss_permille: 350,
                ..Self::baseline()
            },
        }
    }

    fn baseline() -> Self {
        Self {
            rtt_ms: 10.0,
            jitter_ms: 0.0,
            jitter_dist: JitterDist::Normal,
            loss_permille: 0,
            burst_loss: None,
            reorder_permille: 0,
            duplicate_permille: 0,
            rate_bps_down: 100_000_000,
            rate_bps_up: 10_000_000,
            mtu_bytes: 65_536,
            outage_schedule: Vec::new(),
            ack_delay_ms: 0,
            seed: 0x5EED_1234_ABCD_0001,
        }
    }

    /// A flapping profile: `n` outages of `down_ms`, spaced by `up_ms`.
    pub fn flap(n: u32, down_ms: u64, up_ms: u64) -> Self {
        let mut profile = Self::tier(Tier::T4);
        profile.outage_schedule = (0..n)
            .map(|index| Outage {
                start_ms: 1_000 + u64::from(index) * (down_ms + up_ms),
                dur_ms: down_ms,
            })
            .collect();
        profile
    }

    /// The same profile with a different seed.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// The outage covering `at_ms`, when any.
    pub fn outage_at(&self, at_ms: u64) -> Option<&Outage> {
        self.outage_schedule
            .iter()
            .find(|outage| outage.covers(at_ms))
    }

    /// Whether a message of `message_len` bytes fits the carrier.
    ///
    /// The profile's `mtu_bytes` is the payload budget; the MLP fixed header
    /// and header extension travel on top of it (MLP spec §5.2), so the link
    /// admits `mtu_bytes + FRAMING_ALLOWANCE_BYTES`.
    pub fn admits(&self, message_len: usize) -> bool {
        message_len <= self.mtu_bytes as usize + FRAMING_ALLOWANCE_BYTES
    }

    /// A profile with no randomness at all: loss, reorder and duplication off.
    pub fn perfect() -> Self {
        Self {
            loss_permille: 0,
            reorder_permille: 0,
            duplicate_permille: 0,
            burst_loss: None,
            jitter_ms: 0.0,
            ..Self::tier(Tier::T0)
        }
    }
}

/// Which end of the link a message comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Endpoint {
    /// The sender.
    Host,
    /// The receiver (browser).
    Viewer,
}

impl Endpoint {
    /// The other end.
    pub const fn peer(self) -> Self {
        match self {
            Self::Host => Self::Viewer,
            Self::Viewer => Self::Host,
        }
    }
}

/// Framing overhead (MLP fixed header plus the longest header extension)
/// admitted on top of a profile's payload budget.
pub const FRAMING_ALLOWANCE_BYTES: usize = 128;

/// One delivered message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The receiving endpoint.
    pub to: Endpoint,
    /// The message bytes.
    pub bytes: Vec<u8>,
    /// Delivery time in scenario milliseconds.
    pub at_ms: u64,
    /// Transmit order, for deterministic sorting.
    pub sequence: u64,
}

/// What the link did, for assertions and the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LinkStats {
    /// Messages offered to the link.
    pub transmitted: u64,
    /// Messages handed to the far end.
    pub delivered: u64,
    /// Messages dropped by the uniform or bursty loss model.
    pub dropped_loss: u64,
    /// Messages dropped because an outage covered their arrival.
    pub dropped_outage: u64,
    /// Messages dropped for exceeding the MTU.
    pub dropped_mtu: u64,
    /// Deliberate duplicates delivered.
    pub duplicated: u64,
    /// Messages whose delivery was delayed for reordering.
    pub reordered: u64,
    /// Bytes offered.
    pub bytes_transmitted: u64,
    /// Bytes delivered.
    pub bytes_delivered: u64,
    /// Milliseconds the link spent serializing payloads.
    pub serialization_ms: u64,
}

/// A 64-bit PCG-XSL-RR generator: deterministic, in-crate, no dependencies.
#[derive(Debug, Clone)]
pub struct Pcg64 {
    state: u128,
    inc: u128,
    draws: u64,
}

impl Pcg64 {
    /// Seed the generator (any seed, including zero, is fine).
    pub fn new(seed: u64) -> Self {
        let inc = (u128::from(seed) << 1) | 1;
        let mut generator = Self {
            state: 0,
            inc,
            draws: 0,
        };
        generator.next_u64();
        generator.state = generator
            .state
            .wrapping_add(u128::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        generator.next_u64();
        generator
    }

    /// The next 64-bit draw.
    pub fn next_u64(&mut self) -> u64 {
        let old = self.state;
        self.state = old
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(self.inc);
        self.draws += 1;
        let xorshifted = ((old >> 64) ^ old) as u64;
        let rot = (old >> 122) as u32;
        xorshifted.rotate_right(rot)
    }

    /// The next 32-bit draw.
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// A uniform draw in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Whether an event with the given permille probability happens.
    pub fn chance_permille(&mut self, permille: u32) -> bool {
        if permille == 0 {
            return false;
        }
        if permille >= 1_000 {
            return true;
        }
        self.next_u64() % 1_000 < u64::from(permille)
    }

    /// A Pareto draw with `alpha = 1.5`, scaled by `scale`, in `[0, ∞)`.
    pub fn pareto(&mut self, scale: f64) -> f64 {
        let u = self.next_f64().max(1e-9);
        scale * (u.powf(-1.0 / 1.5) - 1.0)
    }

    /// A standard-normal draw via Box–Muller.
    pub fn normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-12);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * core::f64::consts::PI * u2).cos()
    }

    /// How many draws have been taken (a determinism fingerprint).
    pub fn draws(&self) -> u64 {
        self.draws
    }
}

#[derive(Debug, Clone)]
struct Queued {
    to: Endpoint,
    bytes: Vec<u8>,
    deliver_at_ms: u64,
    sequence: u64,
}

/// The deterministic lossy link.
#[derive(Debug, Clone)]
pub struct LossyLink {
    profile: LinkProfile,
    rng: Pcg64,
    now_ms: u64,
    queue: Vec<Queued>,
    next_sequence: u64,
    busy_up_until_ms: u64,
    busy_down_until_ms: u64,
    burst_remaining: u32,
    stats: LinkStats,
}

impl LossyLink {
    /// Create a link with a profile.
    pub fn new(profile: LinkProfile) -> Self {
        let rng = Pcg64::new(profile.seed);
        Self {
            profile,
            rng,
            now_ms: 0,
            queue: Vec::new(),
            next_sequence: 1,
            busy_up_until_ms: 0,
            busy_down_until_ms: 0,
            burst_remaining: 0,
            stats: LinkStats::default(),
        }
    }

    /// The profile in force.
    pub fn profile(&self) -> &LinkProfile {
        &self.profile
    }

    /// Replace the profile mid-scenario (`Step::Link`).
    ///
    /// A profile with a different seed restarts the RNG stream; one with the
    /// same seed keeps it, so a scenario can change the link conditions without
    /// teleporting the random sequence.
    pub fn set_profile(&mut self, profile: LinkProfile) {
        if profile.seed != self.profile.seed {
            self.rng = Pcg64::new(profile.seed);
        }
        self.profile = profile;
    }

    /// Scenario time in ms.
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }

    /// The link statistics.
    pub fn stats(&self) -> &LinkStats {
        &self.stats
    }

    /// Messages currently in flight.
    pub fn in_flight(&self) -> usize {
        self.queue.len()
    }

    /// Offer a message to the link.
    ///
    /// The message is serialized at the direction's rate after any earlier
    /// message on the same direction, then delivered one propagation delay
    /// later, subject to loss, reordering, duplication and outages.
    pub fn transmit(&mut self, from: Endpoint, bytes: Vec<u8>) {
        self.stats.transmitted += 1;
        self.stats.bytes_transmitted += bytes.len() as u64;
        let to = from.peer();
        if !self.profile.admits(bytes.len()) {
            self.stats.dropped_mtu += 1;
            return;
        }
        if self.profile.outage_at(self.now_ms).is_some() {
            self.stats.dropped_outage += 1;
            return;
        }
        let duplicates = self.rng.chance_permille(self.profile.duplicate_permille);
        let reorder = self.rng.chance_permille(self.profile.reorder_permille);
        let lost = self.roll_loss();
        if lost {
            return;
        }
        let rate_bps = match from {
            Endpoint::Host => self.profile.rate_bps_down,
            Endpoint::Viewer => self.profile.rate_bps_up,
        }
        .max(1);
        let serialization_ms = (bytes.len() as f64 * 8.0 * 1_000.0 / rate_bps as f64).ceil() as u64;
        let busy = match from {
            Endpoint::Host => &mut self.busy_down_until_ms,
            Endpoint::Viewer => &mut self.busy_up_until_ms,
        };
        let start = (*busy).max(self.now_ms);
        let finish = start + serialization_ms;
        *busy = finish;
        self.stats.serialization_ms += serialization_ms;

        let mut delay = self.profile.rtt_ms / 2.0;
        if self.profile.jitter_ms > 0.0 {
            delay += match self.profile.jitter_dist {
                JitterDist::Normal => (self.rng.normal() * self.profile.jitter_ms).abs(),
                JitterDist::Pareto => self.rng.pareto(self.profile.jitter_ms),
            };
        }
        if from == Endpoint::Viewer {
            delay += self.profile.ack_delay_ms as f64;
        }
        if reorder {
            delay += self.profile.rtt_ms / 2.0 + 1.0;
            self.stats.reordered += 1;
        }
        let deliver_at_ms = finish + delay.max(0.0).round() as u64;
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.enqueue(Queued {
            to,
            bytes: bytes.clone(),
            deliver_at_ms,
            sequence,
        });
        if duplicates {
            self.stats.duplicated += 1;
            let duplicate_sequence = self.next_sequence;
            self.next_sequence += 1;
            self.enqueue(Queued {
                to,
                bytes,
                deliver_at_ms: deliver_at_ms + 1,
                sequence: duplicate_sequence,
            });
        }
    }

    /// Advance the clock by `ms` and return everything due by then.
    pub fn advance(&mut self, ms: u64) -> Vec<Delivery> {
        self.advance_to(self.now_ms + ms)
    }

    /// Advance the clock to an absolute scenario time and return due messages.
    pub fn advance_to(&mut self, at_ms: u64) -> Vec<Delivery> {
        self.now_ms = self.now_ms.max(at_ms);
        let mut due: Vec<Queued> = Vec::new();
        let mut remaining = Vec::with_capacity(self.queue.len());
        for queued in self.queue.drain(..) {
            if queued.deliver_at_ms <= self.now_ms {
                due.push(queued);
            } else {
                remaining.push(queued);
            }
        }
        self.queue = remaining;
        due.sort_by_key(|queued| (queued.deliver_at_ms, queued.sequence));
        let mut out = Vec::with_capacity(due.len());
        for queued in due {
            if self.profile.outage_at(queued.deliver_at_ms).is_some() {
                self.stats.dropped_outage += 1;
                continue;
            }
            self.stats.delivered += 1;
            self.stats.bytes_delivered += queued.bytes.len() as u64;
            out.push(Delivery {
                to: queued.to,
                bytes: queued.bytes,
                at_ms: queued.deliver_at_ms,
                sequence: queued.sequence,
            });
        }
        out
    }

    fn enqueue(&mut self, queued: Queued) {
        self.queue.push(queued);
    }

    fn roll_loss(&mut self) -> bool {
        if self.profile.loss_permille == 0 {
            return false;
        }
        if self.burst_remaining > 0 {
            self.burst_remaining -= 1;
            self.stats.dropped_loss += 1;
            return true;
        }
        if self.rng.chance_permille(self.profile.loss_permille) {
            self.stats.dropped_loss += 1;
            if let Some(mean_burst) = self.profile.burst_loss {
                // A geometric burst length with the requested mean.
                // Length ~ Geometric with mean `mean_burst`: continue with
                // probability 1 − 1/mean.
                let mut length = 1u32;
                let probability = 1.0 - 1.0 / mean_burst.max(1) as f64;
                while self.rng.next_f64() < probability && length < 1_000 {
                    length += 1;
                }
                self.burst_remaining = length.saturating_sub(1);
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcg_is_deterministic_and_well_distributed_enough() {
        let mut first = Pcg64::new(7);
        let mut second = Pcg64::new(7);
        for _ in 0..100 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
        let mut generator = Pcg64::new(11);
        let mut low = 0;
        for _ in 0..1_000 {
            if generator.next_f64() < 0.5 {
                low += 1;
            }
        }
        assert!((400..=600).contains(&low), "uniformity: {low}");
        assert!(generator.draws() > 1_000);
    }

    #[test]
    fn perfect_link_delivers_everything_in_order() {
        let mut link = LossyLink::new(LinkProfile::perfect());
        link.transmit(Endpoint::Host, vec![1; 100]);
        link.transmit(Endpoint::Host, vec![2; 100]);
        let deliveries = link.advance_to(1_000);
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].to, Endpoint::Viewer);
        assert_eq!(deliveries[0].bytes, vec![1; 100]);
        assert!(deliveries[0].at_ms <= deliveries[1].at_ms);
        assert_eq!(link.stats().dropped_loss, 0);
    }

    #[test]
    fn tier_profiles_match_the_design() {
        let t0 = LinkProfile::tier(Tier::T0);
        assert_eq!(t0.rtt_ms, 10.0);
        assert_eq!(t0.loss_permille, 0);
        assert_eq!(t0.rate_bps_down, 100_000_000);
        let t4 = LinkProfile::tier(Tier::T4);
        assert_eq!(t4.rate_bps_down, 32_000);
        assert_eq!(t4.loss_permille, 350);
        assert_eq!(t4.mtu_bytes, 1_150);
        assert_eq!(t4.jitter_dist, JitterDist::Pareto);
        assert_eq!(LinkProfile::tier(Tier::T3).rate_bps_down, 256_000);
    }

    #[test]
    fn loss_drops_the_requested_share() {
        let mut profile = LinkProfile::perfect();
        profile.loss_permille = 500;
        profile.rtt_ms = 0.0;
        let mut link = LossyLink::new(profile);
        for _ in 0..2_000 {
            link.transmit(Endpoint::Host, vec![0; 8]);
        }
        let delivered = link.advance_to(10_000).len();
        assert!(
            (800..=1_200).contains(&delivered),
            "500‰ loss delivered {delivered}/2000"
        );
    }

    #[test]
    fn burst_loss_drops_runs() {
        let mut profile = LinkProfile::perfect();
        profile.loss_permille = 100;
        profile.burst_loss = Some(10);
        let mut link = LossyLink::new(profile);
        for _ in 0..500 {
            link.transmit(Endpoint::Host, vec![0; 8]);
        }
        let _ = link.advance_to(10_000);
        let stats = link.stats();
        assert!(stats.dropped_loss > 0);
        // A burst mean of 10 with 10 % entry loss loses more than the
        // independent-loss model would.
        assert!(
            stats.dropped_loss as f64 > 0.15 * stats.transmitted as f64,
            "burst loss should amplify: {stats:?}"
        );
    }

    #[test]
    fn rate_limits_serialization() {
        let mut profile = LinkProfile::perfect();
        profile.rate_bps_down = 8_000;
        profile.rtt_ms = 0.0;
        let mut link = LossyLink::new(profile);
        // 1 000 bytes at 1 000 B/s takes 1 s.
        link.transmit(Endpoint::Host, vec![0; 1_000]);
        let early = link.advance_to(900);
        assert!(early.is_empty());
        let late = link.advance_to(1_100);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0].at_ms, 1_000);
    }

    #[test]
    fn mtu_drops_oversized_messages() {
        let mut profile = LinkProfile::perfect();
        profile.mtu_bytes = 100;
        // The payload budget admits the MLP framing allowance on top.
        let mut link = LossyLink::new(profile);
        link.transmit(Endpoint::Host, vec![0; 100 + FRAMING_ALLOWANCE_BYTES + 1]);
        link.transmit(Endpoint::Host, vec![0; 100 + FRAMING_ALLOWANCE_BYTES]);
        let deliveries = link.advance_to(1_000);
        assert_eq!(deliveries.len(), 1);
        assert_eq!(link.stats().dropped_mtu, 1);
    }

    #[test]
    fn outages_drop_both_directions() {
        let mut profile = LinkProfile::perfect();
        profile.outage_schedule = vec![Outage {
            start_ms: 100,
            dur_ms: 100,
        }];
        let mut link = LossyLink::new(profile);
        link.transmit(Endpoint::Host, vec![0; 8]);
        assert_eq!(link.advance_to(20).len(), 1);
        // A message whose arrival lands inside the outage is lost ...
        link.advance_to(96);
        link.transmit(Endpoint::Host, vec![1; 8]);
        let during = link.advance_to(150);
        assert!(during.is_empty());
        assert_eq!(link.stats().dropped_outage, 1);
        // ... and a message offered during the outage never leaves.
        link.transmit(Endpoint::Host, vec![2; 8]);
        assert_eq!(link.stats().dropped_outage, 2);
        // After it, the link works again.
        link.advance_to(200);
        link.transmit(Endpoint::Host, vec![3; 8]);
        assert_eq!(link.advance_to(300).len(), 1);
    }

    #[test]
    fn duplication_and_reordering_are_deterministic() {
        let mut profile = LinkProfile::perfect();
        profile.duplicate_permille = 500;
        profile.reorder_permille = 250;
        profile.rtt_ms = 100.0;
        let run = || {
            let mut link = LossyLink::new(profile.clone());
            for index in 0..50u8 {
                link.transmit(Endpoint::Host, vec![index; 16]);
            }
            let deliveries = link.advance_to(10_000);
            (
                deliveries
                    .iter()
                    .map(|delivery| delivery.bytes[0])
                    .collect::<Vec<_>>(),
                *link.stats(),
            )
        };
        let first = run();
        let second = run();
        assert_eq!(first, second, "same seed, same trace");
        assert!(first.1.duplicated > 0);
        assert!(first.1.reordered > 0);
    }

    #[test]
    fn flap_profile_builds_the_expected_schedule() {
        let profile = LinkProfile::flap(5, 10_000, 10_000);
        assert_eq!(profile.outage_schedule.len(), 5);
        assert!(profile.outage_at(1_000).is_some());
        assert!(profile.outage_at(11_000).is_none());
        assert!(profile.outage_at(12_000).is_none());
        assert!(profile.outage_at(21_500).is_some(), "the second flap");
    }
}
