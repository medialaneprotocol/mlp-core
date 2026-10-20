//! Bandwidth estimation (MLP spec §8.2, D-2): goodput/loss/delay hybrid.
//!
//! One [`BandwidthEstimator`] per `(sender, receiver)` media session. It is fed by
//! the receiver's `MediaFeedback`, by per-hop `MediaHopReport`s and by the
//! sender's own send schedule, and it is the **only** source of truth for what
//! the encoder is allowed to spend.
//!
//! Split of responsibilities, taken verbatim from the specification: goodput and
//! priors *increase* the rate (liveness), delay and loss *reduce* it (safety).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{MediaFeedback, MediaHopReport, ProbeEcho};

/// The estimator's operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BweMode {
    /// Session start: the pace doubles per report until it overshoots.
    StartUp,
    /// Normal operation: `0.9 × btlbw`.
    Steady,
    /// Loss or queue delay above threshold: back off by 30 % per report.
    Degraded,
    /// At the floor rate: only P0 and idle updates are allowed.
    Floor,
    /// No `MediaFeedback` for `3 × report_interval + 2 s` (MLP spec §8).
    NoFeedback,
}

impl BweMode {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StartUp => "startup",
            Self::Steady => "steady",
            Self::Degraded => "degraded",
            Self::Floor => "floor",
            Self::NoFeedback => "no_feedback",
        }
    }

    /// Whether speculative refresh (P3) may be scheduled.
    pub const fn allows_speculative_refresh(self) -> bool {
        matches!(self, Self::StartUp | Self::Steady)
    }
}

/// Estimator configuration; every field is a policy constant of MLP spec §8.2.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BweConfig {
    /// the pace floor, default 8 000.
    pub pace_floor_bps: u64,
    /// the bandwidth ceiling, default 50 Mbps.
    pub ceiling_bps: u64,
    /// A remembered prior for this `(receiver, sender)` pair, when one exists.
    pub prior_bps: Option<u64>,
    /// The encoder's requested update rate before budget pressure.
    pub requested_fps: f64,
    /// Whether at least one receiver is live (floors `target_fps` at 1).
    pub viewer_live: bool,
    /// The RTT-min sliding horizon.
    pub rtt_horizon: Duration,
    /// Regular probe interval in `Steady`.
    pub probe_interval: Duration,
    /// Maximum reports in `StartUp` before the state machine forces `Steady`.
    pub startup_max_reports: u32,
    /// `min_delta_bytes` below which `target_fps` is reduced (MLP spec §8).
    pub min_delta_bytes: u64,
    /// Maximum probe payload.
    pub probe_payload_max: u32,
    /// Prior TTL for [`PriorStore`] (the prior TTL).
    pub prior_ttl: Duration,
}

impl Default for BweConfig {
    fn default() -> Self {
        Self {
            pace_floor_bps: 8_000,
            ceiling_bps: 50_000_000,
            prior_bps: None,
            requested_fps: 30.0,
            viewer_live: true,
            rtt_horizon: Duration::from_secs(60),
            probe_interval: Duration::from_secs(30),
            startup_max_reports: 4,
            min_delta_bytes: 8_192,
            probe_payload_max: 8_192,
            prior_ttl: Duration::from_secs(3_600),
        }
    }
}

impl BweConfig {
    /// Apply a remembered prior to a fresh session configuration.
    pub fn with_prior(mut self, remembered_bps: Option<u64>) -> Self {
        self.prior_bps = remembered_bps;
        self
    }
}

/// The estimator's output: the only numbers the encoder may spend.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    /// Current pace in bits per second.
    pub pace_bps: u64,
    /// Current per-update budget in bytes.
    pub budget_bytes: u32,
    /// Largest single object the sender may emit, in bytes.
    pub max_object_bytes: u32,
    /// Current target updates per second.
    pub target_fps: f64,
    /// The operating mode.
    pub mode: BweMode,
    /// Receiver-reported loss ratio of the last window.
    pub loss_ratio: f64,
    /// Smoothed RTT in ms.
    pub rtt_ms: f64,
    /// Jitter (EWMA of `|rtt - rtt_ewma|`) in ms.
    pub jitter_ms: f64,
}

impl Estimate {
    /// The update interval implied by `target_fps`.
    pub fn update_interval(&self) -> Duration {
        let fps = self.target_fps.max(0.001);
        Duration::from_secs_f64(1.0 / fps)
    }

    /// Bytes per second of the current pace.
    pub fn pace_bytes_per_sec(&self) -> f64 {
        self.pace_bps as f64 / 8.0
    }
}

/// A probe the sender must emit (MLP spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRequest {
    /// Probe payload bytes, `≤ probe_payload_max`, never cells.
    pub payload_bytes: u32,
    /// The rate the probe should be paced at.
    pub rate_bps: u64,
}

/// The outcome of a probe once the feedback for it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    /// Whether the probe (or its echo) came back.
    pub delivered: bool,
    /// The delivered ratio of the probe window.
    pub delivered_ratio: f64,
    /// The rate the probe was sent at.
    pub probe_rate_bps: u64,
}

/// A probe the sender has put on the wire and is waiting for.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PendingProbe {
    /// The request the estimator asked for.
    request: ProbeRequest,
    /// The epoch the probe carries, once the sender stamped it.
    send_epoch_ms: Option<u64>,
    /// When the probe went out.
    sent_at: Option<Instant>,
}

/// Loss attribution across hops (MLP spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HopAttribution {
    /// Loss reported by the receiver's object-id gap detection.
    pub viewer_lost_objects: u64,
    /// Objects the intermediary reported dropping.
    pub relay_dropped: u64,
    /// Objects the edge reported dropping.
    pub gateway_dropped: u64,
    /// Other hops' reported drops.
    pub other_dropped: u64,
}

impl HopAttribution {
    /// The unattributed remainder: the number that answers "is it the last
    /// mile or our stack?" (MLP spec §12).
    pub fn link_lost_objects(&self) -> u64 {
        self.viewer_lost_objects
            .saturating_sub(self.relay_dropped + self.gateway_dropped + self.other_dropped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InternalState {
    StartUp,
    Steady,
    Degraded,
    Floor,
}

impl InternalState {
    fn mode(self) -> BweMode {
        match self {
            Self::StartUp => BweMode::StartUp,
            Self::Steady => BweMode::Steady,
            Self::Degraded => BweMode::Degraded,
            Self::Floor => BweMode::Floor,
        }
    }

    /// Whether a probe may be issued in this state.
    ///
    /// A probe is how the sender measures a link the reports do not describe,
    /// so a *degraded* estimator needs it most: with the probe reserved for
    /// the steady state, a degraded estimator could never find out that the
    /// link carries more than it is sending (MLP spec §6.2.6).
    fn probes(self) -> bool {
        matches!(self, Self::Steady | Self::Degraded)
    }
}

/// The bandwidth estimator (MLP spec §8.2).
///
/// Deterministic: every measurement derives from injected [`Instant`]s and the
/// integer counters the receiver reports.
#[derive(Debug, Clone)]
pub struct BandwidthEstimator {
    config: BweConfig,
    state: InternalState,
    pace_bps: f64,
    btlbw_bps: f64,
    rtt_ewma_ms: f64,
    rtt_min_ms: f64,
    jitter_ms: f64,
    rtt_samples: VecDeque<(Instant, f64)>,
    /// Reports observed in the current state (used by StartUp and Degraded).
    reports_in_state: u32,
    /// Consecutive healthy reports in Degraded.
    healthy_reports: u32,
    last_report_at: Option<Instant>,
    no_feedback: bool,
    loss_ratio: f64,
    goodput_bps: f64,
    queue_delay_ms: f64,
    demand_bps: f64,
    demand_since: Option<Instant>,
    last_probe_at: Option<Instant>,
    probe_due: bool,
    probe_pending: Option<PendingProbe>,
    sent_objects: u64,
    sent_bytes: u64,
    /// Bytes sent since the last report: the window's own traffic.
    window_sent_bytes: u64,
    /// Whether the last window was capacity-limited: the sender sent at (or
    /// above) its own pace, so the window says something about the link.
    capacity_limited: bool,
    attribution: HopAttribution,
    probes_sent: u64,
    probes_failed: u64,
    /// Reports compared with the sender's own round-trip measurement.
    rtt_views_compared: u64,
    /// Reports whose round trip disagreed with the sender's own.
    rtt_view_disagreements: u64,
    /// The largest disagreement between the two views, in ms.
    rtt_disagreement_ms: f64,
}

impl BandwidthEstimator {
    /// Start an estimator for a fresh session.
    pub fn new(config: BweConfig) -> Self {
        let ceiling = config.ceiling_bps.max(config.pace_floor_bps);
        let pace_0 = match config.prior_bps {
            Some(prior) => (prior / 4).clamp(config.pace_floor_bps, 2_000_000.min(ceiling)),
            None => config.pace_floor_bps,
        };
        Self {
            config,
            state: InternalState::StartUp,
            pace_bps: pace_0 as f64,
            btlbw_bps: pace_0 as f64,
            rtt_ewma_ms: 0.0,
            rtt_min_ms: 0.0,
            jitter_ms: 0.0,
            rtt_samples: VecDeque::new(),
            reports_in_state: 0,
            healthy_reports: 0,
            last_report_at: None,
            // A session that has never heard from the receiver is not "stalled",
            // it is "not yet started": the no-feedback mode requires at least
            // one report so a cold receiver cannot be misclassified.
            no_feedback: false,
            loss_ratio: 0.0,
            goodput_bps: 0.0,
            queue_delay_ms: 0.0,
            demand_bps: 0.0,
            demand_since: None,
            last_probe_at: None,
            probe_due: false,
            probe_pending: None,
            sent_objects: 0,
            sent_bytes: 0,
            window_sent_bytes: 0,
            capacity_limited: false,
            attribution: HopAttribution::default(),
            probes_sent: 0,
            probes_failed: 0,
            rtt_views_compared: 0,
            rtt_view_disagreements: 0,
            rtt_disagreement_ms: 0.0,
        }
    }

    /// The configuration in force.
    pub fn config(&self) -> &BweConfig {
        &self.config
    }

    /// Replace the configuration (it arrives from the driver, which owns env).
    pub fn set_config(&mut self, config: BweConfig) {
        self.config = config;
    }

    /// Tell the estimator whether any receiver is live (floors `target_fps`).
    pub fn set_viewer_live(&mut self, live: bool) {
        self.config.viewer_live = live;
    }

    /// Cap the ceiling at runtime (the bandwidth ceiling × `link_hint`).
    pub fn set_ceiling(&mut self, ceiling_bps: u64) {
        self.config.ceiling_bps = ceiling_bps.max(self.config.pace_floor_bps);
    }

    /// The current internal state as a public mode.
    pub fn mode(&self) -> BweMode {
        if self.no_feedback {
            BweMode::NoFeedback
        } else {
            self.state.mode()
        }
    }

    /// The max-filter output (`mlp_btlbw_bps`).
    pub fn btlbw_bps(&self) -> u64 {
        self.btlbw_bps.round() as u64
    }

    /// The current pace in bits per second.
    pub fn pace_bps(&self) -> u64 {
        self.pace_bps.round() as u64
    }

    /// The last receiver-reported loss ratio.
    pub fn loss_ratio(&self) -> f64 {
        self.loss_ratio
    }

    /// Smoothed RTT in ms.
    pub fn rtt_ms(&self) -> f64 {
        self.rtt_ewma_ms
    }

    /// RTT minimum over the sliding horizon, in ms.
    pub fn rtt_min_ms(&self) -> f64 {
        self.rtt_min_ms
    }

    /// Jitter estimate in ms.
    pub fn jitter_ms(&self) -> f64 {
        self.jitter_ms
    }

    /// `rtt_ewma − rtt_min`: the delay-based congestion signal.
    pub fn queue_delay_ms(&self) -> f64 {
        self.queue_delay_ms
    }

    /// `report_interval_ms = clamp(4 × rtt_min_ms, 250, 2 000)`.
    pub fn report_interval(&self) -> Duration {
        let interval = (4.0 * self.rtt_min_ms).clamp(250.0, 2_000.0);
        Duration::from_secs_f64(interval / 1_000.0)
    }

    /// The no-feedback grace window `3 × report_interval + 2 s`.
    pub fn no_feedback_grace(&self) -> Duration {
        3 * self.report_interval() + Duration::from_secs(2)
    }

    /// Record that the sender emitted `bytes` for `object_id`.
    pub fn on_send(&mut self, bytes: usize, object_id: u32, _now: Instant) {
        self.sent_objects += 1;
        self.sent_bytes += bytes as u64;
        self.window_sent_bytes += bytes as u64;
        let _ = object_id;
    }

    /// Record that the sender wants `demand_bps` (the caller's queue pressure).
    pub fn on_demand(&mut self, demand_bps: u64, now: Instant) {
        if demand_bps as f64 > self.pace_bps {
            self.demand_since.get_or_insert(now);
        } else {
            self.demand_since = None;
        }
        self.demand_bps = demand_bps as f64;
    }

    /// Feed one receiver feedback object (MLP spec §8).
    pub fn on_feedback(&mut self, feedback: &MediaFeedback, now: Instant) {
        let report_interval = self.report_interval();
        let _ = report_interval;
        self.last_report_at = Some(now);
        self.no_feedback = false;

        // The receiver's report is its own view of the round trip, measured on
        // the receiver's clock. The estimator's filters — a minimum over a
        // horizon, an EWMA, and the difference between the two — are only
        // meaningful when every sample is the same measurement taken on one
        // clock, so the report is a cross-check and never a sample
        // (MLP spec §6.2.2).
        self.cross_check_rtt(feedback);

        let goodput = feedback.goodput_bps().map(|value| value as f64);
        if let Some(goodput) = goodput {
            self.goodput_bps = goodput;
        }
        let mut loss = feedback.loss_ratio().unwrap_or(self.loss_ratio);
        if feedback.loss_ratio().is_none() {
            // A window with no object accounting carries no loss signal; the
            // previous estimate stands (a receiver that reports only bytes must
            // not be read as "zero loss").
            loss = self.loss_ratio;
        }
        self.loss_ratio = loss;
        self.attribution.viewer_lost_objects += feedback.lost_objects;

        // A window in which the sender sent less than its own pace was
        // application-limited: the link was never asked to carry more, so the
        // window carries no capacity signal. Only a window that ran at (or
        // above) the pace may decay the estimate (MLP spec §6.2.3).
        let window_ms = feedback.window_ms.max(1);
        let sent_bps = self.window_sent_bytes as f64 * 8_000.0 / window_ms as f64;
        self.capacity_limited = sent_bps >= 0.9 * self.pace_bps;
        self.window_sent_bytes = 0;

        self.apply_max_filter(goodput);
        self.step_state_machine(goodput, loss);
        self.pace_bps = self
            .pace_bps
            .clamp(self.config.pace_floor_bps as f64, self.ceiling());
    }

    /// Feed one RTT sample measured outside a feedback report.
    ///
    /// Probe echoes (`ECHO` round trips) and hop reports use this path: an RTT
    /// sample never carries a loss or goodput signal by itself.
    pub fn on_rtt_sample(&mut self, rtt_ms: f64, now: Instant) {
        self.observe_rtt(rtt_ms.clamp(1.0, 60_000.0), now);
    }

    /// Compare the receiver's reported round trip with the sender's own
    /// measurement.
    ///
    /// The two ends measure the same path on their own clocks, so they do not
    /// have to agree exactly — but a large disagreement says the path is not
    /// the same in both directions, or that one of the two views is stale.
    /// It is counted, never mixed into the estimator's filters.
    fn cross_check_rtt(&mut self, feedback: &MediaFeedback) {
        let reported = f64::from(feedback.rtt_min_ms);
        if self.rtt_min_ms <= 0.0 || reported <= 0.0 {
            return;
        }
        self.rtt_views_compared += 1;
        let disagreement = (reported - self.rtt_min_ms).abs();
        if disagreement > self.jitter_ms.max(1.0) {
            self.rtt_view_disagreements += 1;
            self.rtt_disagreement_ms = disagreement;
        }
    }

    /// How many reports were compared with the sender's own measurement.
    pub fn rtt_view_counters(&self) -> (u64, u64) {
        (self.rtt_views_compared, self.rtt_view_disagreements)
    }

    /// The largest disagreement between the two views, in ms.
    pub fn rtt_disagreement_ms(&self) -> f64 {
        self.rtt_disagreement_ms
    }

    fn observe_rtt(&mut self, rtt_sample: f64, now: Instant) {
        self.rtt_samples.push_back((now, rtt_sample));
        let horizon = self.config.rtt_horizon;
        while let Some((at, _)) = self.rtt_samples.front() {
            if now.saturating_duration_since(*at) > horizon {
                self.rtt_samples.pop_front();
            } else {
                break;
            }
        }
        let rtt_min = self
            .rtt_samples
            .iter()
            .map(|(_, sample)| *sample)
            .fold(f64::INFINITY, f64::min);
        self.rtt_min_ms = if rtt_min.is_finite() {
            rtt_min
        } else {
            rtt_sample
        };
        if self.rtt_ewma_ms == 0.0 {
            self.rtt_ewma_ms = rtt_sample;
        } else {
            let previous = self.rtt_ewma_ms;
            let alpha = 1.0 / 8.0;
            self.rtt_ewma_ms = previous + alpha * (rtt_sample - previous);
            let jitter_alpha = 1.0 / 4.0;
            let deviation = (rtt_sample - previous).abs();
            self.jitter_ms += jitter_alpha * (deviation - self.jitter_ms);
        }
        self.queue_delay_ms = (self.rtt_ewma_ms - self.rtt_min_ms).max(0.0);
    }

    fn ceiling(&self) -> f64 {
        self.config
            .ceiling_bps
            .min(50_000_000)
            .max(self.config.pace_floor_bps) as f64
    }

    /// `btlbw` max-filter with decay (MLP spec §8).
    fn apply_max_filter(&mut self, goodput: Option<f64>) {
        let delay_threshold = 50.0f64.max(0.5 * self.rtt_min_ms);
        if self.capacity_limited
            && (self.loss_ratio > 0.02 || self.queue_delay_ms > delay_threshold)
        {
            self.btlbw_bps *= 0.85;
        }
        if let Some(goodput) = goodput {
            self.btlbw_bps = self.btlbw_bps.max(goodput.min(self.ceiling()));
        }
        self.btlbw_bps = self.btlbw_bps.max(1.0);
    }

    /// The state machine and pace rule of MLP spec §8.
    fn step_state_machine(&mut self, goodput: Option<f64>, loss: f64) {
        self.reports_in_state += 1;
        match self.state {
            InternalState::StartUp => {
                let overshoot = goodput.map(|g| g < 0.9 * self.pace_bps).unwrap_or(false);
                if loss >= 0.01
                    || overshoot
                    || self.reports_in_state >= self.config.startup_max_reports.max(1)
                {
                    self.transition(InternalState::Steady);
                    self.pace_bps = 0.9 * self.btlbw_bps;
                } else {
                    self.pace_bps = (2.0 * self.pace_bps).min(self.ceiling());
                }
            }
            InternalState::Steady => {
                self.pace_bps = 0.9 * self.btlbw_bps;
                if loss > 0.05 || self.queue_delay_ms > 1.0 * self.rtt_min_ms {
                    self.transition(InternalState::Degraded);
                }
            }
            InternalState::Degraded => {
                // The safety signal decays btlbw by 30 % per report, but a
                // link that keeps *delivering* data must not be ratcheted
                // below what it demonstrably carries: the max-filter's goodput
                // term is re-applied here (D-2: loss reduces, goodput raises),
                // so persistent benign loss settles instead of marching to the
                // floor. With no delivery at all the goodput term is nil and
                // the decay reaches the floor as designed.
                //
                // A window in which the sender sent less than its own pace
                // was application-limited and carries no capacity signal at
                // all: the decay waits for a window that ran at the pace.
                if self.capacity_limited {
                    self.btlbw_bps *= 0.7;
                }
                if let Some(goodput) = goodput {
                    self.btlbw_bps = self.btlbw_bps.max(goodput.min(self.ceiling()));
                }
                self.pace_bps = 0.9 * self.btlbw_bps;
                // The delay signal exists only where a round trip was
                // measured: no measurement means no queue delay, and a link
                // that reports no delay is healthy.
                let delay_ok =
                    self.queue_delay_ms == 0.0 || self.queue_delay_ms < 0.5 * self.rtt_min_ms;
                let healthy = loss < 0.01 && delay_ok;
                if healthy {
                    self.healthy_reports += 1;
                } else {
                    self.healthy_reports = 0;
                }
                if self.pace_bps < self.config.pace_floor_bps as f64 {
                    self.transition(InternalState::Floor);
                    self.pace_bps = self.config.pace_floor_bps as f64;
                } else if self.healthy_reports >= 3 {
                    self.transition(InternalState::StartUp);
                }
            }
            InternalState::Floor => {
                self.pace_bps = self.config.pace_floor_bps as f64;
                if loss < 0.01 {
                    self.transition(InternalState::StartUp);
                }
            }
        }
    }

    fn transition(&mut self, next: InternalState) {
        self.state = next;
        self.reports_in_state = 0;
        self.healthy_reports = 0;
    }

    /// Feed one hop report (MLP spec §8, D-16).
    pub fn on_hop_report(&mut self, report: &MediaHopReport, _now: Instant) {
        let drops = report.objects_dropped + report.coalesced;
        if report.hop == super::hop::FIRST {
            self.attribution.relay_dropped += drops;
        } else if report.hop == super::hop::SECOND {
            self.attribution.gateway_dropped += drops;
        } else {
            self.attribution.other_dropped += drops;
        }
    }

    /// Advance timers: no-feedback detection and probe cadence.
    pub fn on_timeout(&mut self, now: Instant) {
        if let Some(last) = self.last_report_at
            && now.saturating_duration_since(last) > self.no_feedback_grace()
        {
            self.no_feedback = true;
        }
        // A probe whose echo did not come back in time is a lost probe: the
        // link did not carry it, and the estimate says so.
        if let Some(deadline) = self.probe_deadline()
            && now >= deadline
        {
            let rate = self
                .probe_pending
                .map(|pending| pending.request.rate_bps)
                .unwrap_or_default();
            let outcome = ProbeOutcome {
                delivered: false,
                delivered_ratio: 0.0,
                probe_rate_bps: rate,
            };
            self.on_probe_result(&outcome, now);
        }
        if self.state.probes() && !self.no_feedback {
            // Demand must persist for `max(3 × rtt_min, 1 s)` and a probe may
            // not be re-armed faster than that window: a probe flood would
            // itself be the traffic problem the estimator exists to prevent.
            let demand_window =
                Duration::from_secs_f64((3.0 * self.rtt_min_ms).max(1_000.0) / 1_000.0);
            let congested_demand = self
                .demand_since
                .is_some_and(|since| now.saturating_duration_since(since) > demand_window);
            let since_last = self
                .last_probe_at
                .map(|at| now.saturating_duration_since(at));
            let demand_ready =
                congested_demand && since_last.is_none_or(|elapsed| elapsed >= demand_window);
            let interval_elapsed = since_last
                .map(|elapsed| elapsed >= self.config.probe_interval)
                .unwrap_or(true);
            if demand_ready || interval_elapsed {
                self.probe_due = true;
            }
        }
    }

    /// Ask whether a probe must be emitted now (MLP spec §8).
    pub fn probe_due(&mut self, now: Instant) -> Option<ProbeRequest> {
        self.on_timeout(now);
        if !self.probe_due || self.no_feedback || !self.state.probes() {
            return None;
        }
        let payload_bytes = (0.25 * self.pace_bps / 8.0) as u32;
        let payload_bytes = payload_bytes.min(self.config.probe_payload_max).max(1);
        let rate_bps = (1.25 * self.btlbw_bps).min(1.25 * self.pace_bps).round() as u64;
        let request = ProbeRequest {
            payload_bytes,
            rate_bps,
        };
        self.probe_due = false;
        self.last_probe_at = Some(now);
        self.probe_pending = Some(PendingProbe {
            request,
            send_epoch_ms: None,
            sent_at: Some(now),
        });
        self.probes_sent += 1;
        Some(request)
    }

    /// Feed the outcome of the pending probe (MLP spec §8).
    pub fn on_probe_result(&mut self, outcome: &ProbeOutcome, _now: Instant) {
        self.probe_pending = None;
        if outcome.delivered {
            let raised = outcome.probe_rate_bps as f64 * outcome.delivered_ratio;
            self.btlbw_bps = self.btlbw_bps.max(raised).min(self.ceiling());
        } else {
            self.btlbw_bps *= 0.9;
            self.probes_failed += 1;
        }
    }

    /// Note that the sender put a probe on the wire.
    ///
    /// The epoch is the one the probe carries (`SEND_EPOCH_MS`): the echo that
    /// comes back quotes it, and that is how the estimator tells the probe's
    /// own echo from any other echo on the lane.
    pub fn note_probe_sent(&mut self, send_epoch_ms: u64, now: Instant) {
        if let Some(pending) = self.probe_pending.as_mut() {
            pending.send_epoch_ms = Some(send_epoch_ms);
            pending.sent_at = Some(now);
        }
    }

    /// The deadline a probe's echo must answer within: the echo promise
    /// `2 × RTT + 250 ms` (MLP spec §9.1), measured on the sender's own clock.
    ///
    /// A probe sent before the first round-trip measurement has no deadline:
    /// that probe's answer *is* the measurement, and a link that has never
    /// answered anything is the no-feedback case, not a lost probe.
    fn probe_deadline(&self) -> Option<Instant> {
        let pending = self.probe_pending.as_ref()?;
        let sent_at = pending.sent_at?;
        if self.rtt_ewma_ms <= 0.0 {
            return None;
        }
        let budget = Duration::from_secs_f64((2.0 * self.rtt_ewma_ms + 250.0) / 1_000.0);
        Some(sent_at + budget)
    }

    /// Feed a probe echo received from the peer (the peer echoes our probe).
    ///
    /// The echo that quotes the pending probe's epoch is that probe's answer:
    /// the probe was delivered, and the link carried it at the rate the probe
    /// was sent at.
    pub fn on_probe_echo(&mut self, echo: &ProbeEcho, now: Instant) {
        let pending = self.probe_pending.as_ref();
        let Some(pending) = pending else {
            return;
        };
        if pending.send_epoch_ms != Some(echo.send_epoch_ms) {
            return;
        }
        let outcome = ProbeOutcome {
            delivered: true,
            delivered_ratio: 1.0,
            probe_rate_bps: pending.request.rate_bps,
        };
        self.on_probe_result(&outcome, now);
    }

    /// Attribution of loss across hops (MLP spec §8).
    pub fn attribution(&self) -> HopAttribution {
        self.attribution
    }

    /// Probes emitted, and probes that failed.
    pub fn probe_counters(&self) -> (u64, u64) {
        (self.probes_sent, self.probes_failed)
    }

    /// Object and byte counters the sender has reported.
    pub fn send_counters(&self) -> (u64, u64) {
        (self.sent_objects, self.sent_bytes)
    }

    /// Compute the current estimate (MLP spec §8).
    pub fn estimate(&self) -> Estimate {
        let ceiling = self.ceiling();
        if self.no_feedback {
            // Design §7.2.8: floor × 2, one fps, small objects, no speculation.
            let pace = self.config.pace_floor_bps * 2;
            let target_fps = 1.0;
            let budget_bytes = self.budget_for(pace, target_fps, ceiling).max(256.0) as u32;
            return Estimate {
                pace_bps: pace,
                budget_bytes,
                max_object_bytes: 1_024,
                target_fps,
                mode: BweMode::NoFeedback,
                loss_ratio: self.loss_ratio,
                rtt_ms: self.rtt_ewma_ms,
                jitter_ms: self.jitter_ms,
            };
        }

        let pace = self
            .pace_bps
            .clamp(self.config.pace_floor_bps as f64, ceiling) as u64;
        let mut target_fps = self.config.requested_fps.clamp(0.5, 30.0);
        let mut budget = self.budget_for(pace, target_fps, ceiling);
        if budget < self.config.min_delta_bytes as f64 {
            // The update must carry a minimum amount of information; when it
            // cannot, the update rate drops so each update is worth sending.
            let scaled = 30.0 * budget / self.config.min_delta_bytes as f64;
            target_fps = scaled.max(0.5);
            if self.config.viewer_live {
                target_fps = target_fps.max(1.0);
            }
            budget = self.budget_for(pace, target_fps, ceiling);
        }
        let rtt_seconds = (self.rtt_min_ms / 1_000.0).max(0.5);
        let max_object_bytes = (self.pace_bytes_for(pace) * rtt_seconds).clamp(1_024.0, 524_288.0);

        Estimate {
            pace_bps: pace,
            budget_bytes: budget.round().clamp(256.0, 1_048_576.0) as u32,
            max_object_bytes: max_object_bytes.round() as u32,
            target_fps,
            mode: self.mode(),
            loss_ratio: self.loss_ratio,
            rtt_ms: self.rtt_ewma_ms,
            jitter_ms: self.jitter_ms,
        }
    }

    fn pace_bytes_for(&self, pace_bps: u64) -> f64 {
        pace_bps as f64 / 8.0
    }

    fn budget_for(&self, pace_bps: u64, target_fps: f64, _ceiling: f64) -> f64 {
        let update_interval_ms = 1_000.0 / target_fps.max(0.5);
        self.pace_bytes_for(pace_bps) * update_interval_ms / 1_000.0
    }

    /// Duration until the next estimator timer (no-feedback or probe).
    pub fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        let mut next: Option<Duration> = None;
        let mut consider = |candidate: Duration| {
            next = Some(match next {
                Some(current) if current <= candidate => current,
                _ => candidate,
            });
        };
        if let Some(last) = self.last_report_at
            && !self.no_feedback
        {
            let deadline = last + self.no_feedback_grace();
            consider(deadline.saturating_duration_since(now));
        }
        if self.state == InternalState::Steady && !self.no_feedback {
            if let Some(at) = self.last_probe_at {
                let deadline = at + self.config.probe_interval;
                consider(deadline.saturating_duration_since(now));
            } else {
                consider(Duration::ZERO);
            }
        }
        next
    }
}

/// A bounded, TTL'd store of `remembered_bps` per `(receiver, sender)` pair
/// (MLP spec §8). Keys are hashed identities owned by the driver.
#[derive(Debug, Clone)]
pub struct PriorStore {
    capacity: usize,
    ttl: Duration,
    entries: VecDeque<(u64, u64, u64, Instant)>,
}

impl PriorStore {
    /// Create a store with an LRU cap and a TTL.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: VecDeque::new(),
        }
    }

    /// Remember the pace observed for a pair.
    pub fn remember(&mut self, receiver: u64, sender: u64, pace_bps: u64, now: Instant) {
        self.evict_expired(now);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(v, h, _, _)| *v == receiver && *h == sender)
        {
            entry.2 = pace_bps;
            entry.3 = now;
            let entry = self
                .entries
                .iter()
                .find(|(v, h, _, _)| *v == receiver && *h == sender)
                .copied()
                .expect("just found");
            self.touch(entry);
            return;
        }
        self.entries.push_back((receiver, sender, pace_bps, now));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
        }
    }

    /// Recall the remembered pace, if any and not expired.
    pub fn recall(&mut self, receiver: u64, sender: u64, now: Instant) -> Option<u64> {
        self.evict_expired(now);
        let entry = self
            .entries
            .iter()
            .find(|(v, h, _, _)| *v == receiver && *h == sender)
            .copied()?;
        self.touch(entry);
        Some(entry.2)
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn touch(&mut self, entry: (u64, u64, u64, Instant)) {
        self.entries
            .retain(|(v, h, _, _)| *v != entry.0 || *h != entry.1);
        self.entries.push_back(entry);
    }

    fn evict_expired(&mut self, now: Instant) {
        let ttl = self.ttl;
        self.entries
            .retain(|(_, _, _, at)| now.saturating_duration_since(*at) <= ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaFeedback;

    fn feedback(rtt_ms: u32, bytes: u64, objects: u64, lost: u64, window_ms: u64) -> MediaFeedback {
        MediaFeedback {
            rtt_ms,
            rtt_min_ms: rtt_ms,
            received_bytes: bytes,
            received_objects: objects,
            lost_objects: lost,
            window_ms,
            ..MediaFeedback::default()
        }
    }

    /// A window in which the sender sent at its own pace: the window carries
    /// a capacity signal, so a loss or a queue delay may decay the estimate.
    fn sent_at_pace(estimator: &mut BandwidthEstimator, window_ms: u64, now: Instant) {
        let bytes = (estimator.pace_bps() / 8) as usize * (window_ms as usize / 1_000);
        estimator.on_send(bytes.max(1), 1, now);
    }

    #[test]
    fn startup_doubles_then_settles_at_ninety_percent_of_btlbw() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        assert_eq!(estimator.mode(), BweMode::StartUp);
        assert_eq!(estimator.pace_bps(), 8_000);
        // Three healthy reports: 8k → 16k → 32k → 64k.
        for step in 0..3 {
            estimator.on_feedback(
                &feedback(10, 8_000 * 4, 100, 0, 1_000),
                now + Duration::from_millis(step * 100),
            );
        }
        assert_eq!(estimator.pace_bps(), 64_000, "doubling per report");
        // The fourth report exits StartUp: pace becomes 0.9 × btlbw.
        estimator.on_feedback(&feedback(10, 64_000, 100, 0, 1_000), now);
        assert_eq!(estimator.mode(), BweMode::Steady);
        assert!(estimator.pace_bps() >= 57_600);
    }

    #[test]
    fn loss_decays_btlbw_and_enters_degraded() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..4 {
            sent_at_pace(&mut estimator, 1_000, now);
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        let steady = estimator.pace_bps();
        assert_eq!(estimator.mode(), BweMode::Steady);
        sent_at_pace(&mut estimator, 1_000, now);
        estimator.on_feedback(&feedback(10, 50_000, 90, 10, 1_000), now);
        assert_eq!(estimator.mode(), BweMode::Degraded);
        assert!(estimator.pace_bps() < steady);
    }

    #[test]
    fn a_window_the_sender_did_not_fill_carries_no_capacity_signal() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..4 {
            sent_at_pace(&mut estimator, 1_000, now);
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        let steady = estimator.pace_bps();
        // The same loss, in a window the sender did not fill: the estimate
        // stands, because the link was never asked to carry more.
        estimator.on_feedback(&feedback(10, 50_000, 90, 10, 1_000), now);
        assert_eq!(estimator.pace_bps(), steady, "the estimate stands");
    }

    #[test]
    fn three_healthy_reports_return_to_startup() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..4 {
            sent_at_pace(&mut estimator, 1_000, now);
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        sent_at_pace(&mut estimator, 1_000, now);
        estimator.on_feedback(&feedback(10, 50_000, 90, 10, 1_000), now);
        assert_eq!(estimator.mode(), BweMode::Degraded);
        for _ in 0..3 {
            sent_at_pace(&mut estimator, 1_000, now);
            estimator.on_feedback(&feedback(10, 50_000, 100, 0, 1_000), now);
        }
        assert_eq!(estimator.mode(), BweMode::StartUp);
    }

    #[test]
    fn queue_delay_above_one_rtt_degrades() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        estimator.on_rtt_sample(10.0, now);
        for _ in 0..4 {
            sent_at_pace(&mut estimator, 1_000, now);
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        assert_eq!(estimator.mode(), BweMode::Steady);
        // rtt_min stays 10 ms while rtt_ewma climbs: queue delay > 1 × rtt_min.
        for _ in 0..24 {
            estimator.on_rtt_sample(500.0, now);
            sent_at_pace(&mut estimator, 1_000, now);
            estimator.on_feedback(&feedback(500, 100_000, 100, 0, 1_000), now);
        }
        assert!(
            matches!(estimator.mode(), BweMode::Degraded | BweMode::Floor),
            "mode was {:?}",
            estimator.mode()
        );
    }

    #[test]
    fn pace_never_leaves_the_floor_ceiling_band() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..200 {
            estimator.on_feedback(&feedback(10, 0, 0, 100, 1_000), now);
        }
        assert_eq!(estimator.mode(), BweMode::Floor);
        assert_eq!(estimator.pace_bps(), 8_000);
        assert_eq!(estimator.estimate().pace_bps, 8_000);
    }

    #[test]
    fn slow_link_selects_one_fps_and_the_floor_budget() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        estimator.on_feedback(&feedback(800, 4_000, 100, 0, 1_000), now);
        let estimate = estimator.estimate();
        assert_eq!(estimate.target_fps, 1.0, "slow link floors fps at 1 live");
        // One second at the doubled start-up pace (16 kbps) is the budget.
        assert!(estimate.budget_bytes <= 2_100, "{}", estimate.budget_bytes);
        assert!(estimate.max_object_bytes >= 1_024);
        assert!(estimate.max_object_bytes <= 524_288);
    }

    #[test]
    fn fast_link_uses_thirty_fps_and_a_ceiling_bounded_pace() {
        let now = Instant::now();
        let config = BweConfig {
            ceiling_bps: 100_000_000,
            ..BweConfig::default()
        };
        let mut estimator = BandwidthEstimator::new(config);
        for _ in 0..10 {
            estimator.on_feedback(&feedback(5, 12_500_000, 1_000, 0, 1_000), now);
        }
        let estimate = estimator.estimate();
        assert_eq!(estimate.target_fps, 30.0);
        assert!(
            estimate.pace_bps <= 50_000_000,
            "the hard 50 Mbps cap holds"
        );
        assert!(estimate.max_object_bytes <= 524_288);
    }

    #[test]
    fn no_feedback_after_grace_sets_the_emergency_profile() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        let grace = estimator.no_feedback_grace();
        assert!(grace >= Duration::from_secs(2));
        estimator.on_timeout(now + grace + Duration::from_millis(1));
        assert_eq!(estimator.mode(), BweMode::NoFeedback);
        let estimate = estimator.estimate();
        assert_eq!(estimate.pace_bps, 16_000);
        assert_eq!(estimate.target_fps, 1.0);
        assert_eq!(estimate.max_object_bytes, 1_024);
        // One report clears it.
        estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        assert_ne!(estimator.mode(), BweMode::NoFeedback);
    }

    #[test]
    fn cold_start_has_no_feedback_mode_until_the_first_report() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        estimator.on_timeout(now + Duration::from_secs(60));
        assert_eq!(estimator.mode(), BweMode::StartUp);
    }

    #[test]
    fn prior_raises_the_initial_pace() {
        let estimator = BandwidthEstimator::new(BweConfig::default().with_prior(Some(800_000)));
        assert_eq!(estimator.pace_bps(), 200_000, "prior / 4");
        let no_prior = BandwidthEstimator::new(BweConfig::default());
        assert_eq!(no_prior.pace_bps(), 8_000);
        let huge = BandwidthEstimator::new(BweConfig::default().with_prior(Some(50_000_000)));
        assert_eq!(
            huge.pace_bps(),
            2_000_000,
            "prior start is capped at 2 Mbps"
        );
    }

    #[test]
    fn probes_are_issued_in_steady_and_move_btlbw() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..5 {
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        assert_eq!(estimator.mode(), BweMode::Steady);
        let before = estimator.btlbw_bps();
        let probe = estimator.probe_due(now).expect("a probe is due");
        assert!(probe.payload_bytes <= 8_192);
        assert!(probe.rate_bps >= 100_000);
        estimator.on_probe_result(
            &ProbeOutcome {
                delivered: true,
                delivered_ratio: 1.0,
                probe_rate_bps: before * 2,
            },
            now,
        );
        assert!(estimator.btlbw_bps() >= before);
        // A failed probe decays btlbw.
        let after = estimator.btlbw_bps();
        estimator.on_probe_result(
            &ProbeOutcome {
                delivered: false,
                delivered_ratio: 0.0,
                probe_rate_bps: after,
            },
            now,
        );
        assert_eq!(estimator.btlbw_bps(), (after as f64 * 0.9).round() as u64);
    }

    #[test]
    fn no_probe_without_demand_or_interval() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..5 {
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        assert!(
            estimator.probe_due(now).is_some(),
            "first probe after Steady"
        );
        assert!(
            estimator.probe_due(now + Duration::from_secs(1)).is_none(),
            "not again before the interval"
        );
        // A report keeps the link out of NoFeedback, and the interval fires.
        estimator.on_feedback(
            &feedback(10, 100_000, 100, 0, 1_000),
            now + Duration::from_secs(30),
        );
        assert!(
            estimator.probe_due(now + Duration::from_secs(31)).is_some(),
            "the 30 s interval fires"
        );
    }

    #[test]
    fn demand_raises_probe_urgency() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..5 {
            estimator.on_feedback(&feedback(10, 100_000, 100, 0, 1_000), now);
        }
        let _ = estimator.probe_due(now);
        estimator.on_demand(1_000_000, now);
        // A report keeps the link out of NoFeedback while demand persists.
        estimator.on_feedback(
            &feedback(10, 100_000, 100, 0, 1_000),
            now + Duration::from_secs(2),
        );
        assert!(
            estimator.probe_due(now + Duration::from_secs(3)).is_some(),
            "demand above pace for max(3×rtt_min, 1s) triggers a probe"
        );
    }

    #[test]
    fn hop_attribution_subtracts_hop_drops() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        estimator.on_feedback(&feedback(10, 100_000, 90, 10, 1_000), now);
        estimator.on_hop_report(
            &MediaHopReport {
                hop: super::super::hop::FIRST,
                objects_dropped: 4,
                coalesced: 1,
                ..MediaHopReport::default()
            },
            now,
        );
        estimator.on_hop_report(
            &MediaHopReport {
                hop: super::super::hop::SECOND,
                objects_dropped: 3,
                ..MediaHopReport::default()
            },
            now,
        );
        let attribution = estimator.attribution();
        assert_eq!(attribution.viewer_lost_objects, 10);
        assert_eq!(attribution.relay_dropped, 5);
        assert_eq!(attribution.gateway_dropped, 3);
        assert_eq!(attribution.link_lost_objects(), 2);
    }

    #[test]
    fn report_interval_is_rtt_relative_and_clamped() {
        let now = Instant::now();
        // The interval follows the 60 s rtt minimum, so each case gets its own
        // estimator: the minimum is a horizon statistic, not the last sample.
        // The samples are the sender's own round trips, measured from the
        // probe echoes.
        let mut short = BandwidthEstimator::new(BweConfig::default());
        short.on_rtt_sample(10.0, now);
        assert_eq!(short.report_interval(), Duration::from_millis(250));
        let mut long = BandwidthEstimator::new(BweConfig::default());
        long.on_rtt_sample(2_000.0, now);
        assert_eq!(long.report_interval(), Duration::from_millis(2_000));
        let mut mid = BandwidthEstimator::new(BweConfig::default());
        mid.on_rtt_sample(100.0, now);
        assert_eq!(mid.report_interval(), Duration::from_millis(400));
        // A later high sample does not raise it above the horizon minimum's.
        mid.on_rtt_sample(500.0, now);
        assert_eq!(mid.report_interval(), Duration::from_millis(400));
    }

    #[test]
    fn a_reports_round_trip_is_a_cross_check_and_not_a_sample() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        estimator.on_rtt_sample(120.0, now);
        estimator.on_feedback(&feedback(1, 100, 1, 0, 1_000), now);
        assert_eq!(
            estimator.rtt_min_ms(),
            120.0,
            "the report does not lower the sender's own measurement"
        );
        let (compared, disagreements) = estimator.rtt_view_counters();
        assert_eq!(compared, 1);
        assert_eq!(disagreements, 1, "the two views disagree");
    }

    #[test]
    fn poll_timeout_tracks_the_no_feedback_deadline() {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        assert_eq!(estimator.poll_timeout(now), None);
        estimator.on_feedback(&feedback(100, 100, 1, 0, 1_000), now);
        let grace = estimator.no_feedback_grace();
        // A probe is due immediately in Steady, so the first deadline is zero;
        // once the probe is taken the no-feedback deadline is what remains.
        assert_eq!(estimator.poll_timeout(now), Some(Duration::ZERO));
        let _ = estimator.probe_due(now);
        let timeout = estimator.poll_timeout(now).expect("deadline armed");
        assert!(timeout <= grace, "{timeout:?} vs {grace:?}");
        assert_eq!(estimator.poll_timeout(now + grace), Some(Duration::ZERO));
    }

    #[test]
    fn prior_store_honours_capacity_and_ttl() {
        let now = Instant::now();
        let mut store = PriorStore::new(2, Duration::from_secs(60));
        store.remember(1, 10, 100_000, now);
        store.remember(2, 10, 200_000, now);
        assert_eq!(store.recall(1, 10, now), Some(100_000));
        // The recall refreshed 1's recency, so inserting 3 evicts 2.
        store.remember(3, 10, 300_000, now);
        assert_eq!(store.len(), 2, "LRU cap");
        assert_eq!(
            store.recall(2, 10, now),
            None,
            "least recently used evicted"
        );
        assert_eq!(store.recall(1, 10, now), Some(100_000));
        assert_eq!(
            store.recall(3, 10, now + Duration::from_secs(61)),
            None,
            "TTL"
        );
    }

    #[test]
    fn estimate_is_deterministic() {
        let now = Instant::now();
        let run = || {
            let mut estimator = BandwidthEstimator::new(BweConfig::default());
            for _ in 0..7 {
                estimator.on_feedback(&feedback(150, 60_000, 100, 2, 1_000), now);
            }
            estimator.estimate()
        };
        assert_eq!(run(), run());
    }
}
