//! The media-lane sender machine (sender → receiver direction, MLP spec §6.1, §6).
//!
//! [`MediaSenderMachine`] owns the outbound policy of one `(sender, receiver)`
//! media session: update chunking into MLP objects, object ids, the priority
//! queue, the pacer, the estimator, per-receiver cell accounting and the anchor
//! plan ladder. Everything the peer must be told about (lane state, plans,
//! oversize breaches) leaves as a [`MediaSenderEvent`] so the driver can
//! serialize the matching control object and update metrics.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::action::Transmit;
use crate::state_machine::StateMachine;

use super::bwe::{BandwidthEstimator, BweConfig, Estimate};
use super::cells::{BlockAccounting, BlockAccountingConfig, BlockGrid};
use super::error::RejectReason;
use super::input::InputToken;
use super::lifecycle::{
    LaneEvidence, LaneLifecycle, MediaCloseCode, MediaLanePolicy, MediaLaneStateKind,
};
use super::pacer::{AcquireResult, MediaPacer, PacerConfig};
use super::pdu::{
    Carrier, Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, TrafficClass, UpdateMeta,
    tlv,
};
use super::scheduler::{
    PlanConfig, PlanStartReason, ScheduleDecision, ScheduleRequest, Scheduler, SchedulerConfig,
    UpdateBudget, UpdateKind,
};
use super::{
    MediaAck, MediaClose, MediaControlObject, MediaCursor, MediaFeedback, MediaHopReport,
    MediaPath, MediaProbe, ProbeEcho,
};

/// What the driver must do in response to a sender decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaSenderEvent {
    /// The lane state changed; the driver sends `MediaLaneState`.
    LaneStateChanged(MediaLaneStateKind),
    /// An anchor plan started; the driver sends `MediaAnchorPlan`.
    ///
    /// The plan's bands are not events: each band is a update the driver pulls
    /// through [`MediaSenderMachine::plan_next_frame`], and the plan advances by
    /// exactly the bands it encoded (MLP spec §6.2). A plan that advanced on a
    /// poll would skip the cells of every band it did not paint.
    PlanStarted {
        /// Plan id.
        plan_id: u32,
        /// Why it started.
        reason: PlanStartReason,
        /// Cells in the plan.
        cells: u32,
        /// Estimated updates to finish.
        updates_estimate: u32,
    },
    /// The plan ran past its deadline and must restart from the newest state.
    PlanOverdue {
        /// Plan id.
        plan_id: u32,
    },
    /// An encoder invariant was breached; the object was dropped, not sent.
    ObjectOversize {
        /// Bytes the object needed.
        bytes: usize,
        /// The cap in force.
        cap: u32,
    },
    /// A queued object was coalesced away.
    ObjectCoalesced {
        /// The object id.
        object_id: u32,
        /// Why.
        reason: RejectReason,
    },
    /// An echo-critical object was duplicated.
    DuplicateScheduled {
        /// The duplicate's object id.
        object_id: u32,
        /// The original's object id.
        echo_of: u32,
    },
    /// A probe the driver should send as a `MediaProbe` C4 object.
    ProbeDue {
        /// Probe id.
        probe_id: u32,
        /// Payload bytes.
        payload_bytes: u32,
        /// The rate to pace it at.
        rate_bps: u64,
    },
    /// The lane wants to close (park or terminate).
    CloseRequested {
        /// The close body.
        close: MediaClose,
    },
    /// An inbound object was dropped by the transport rules.
    InboundDropped {
        /// The object id, when known.
        object_id: Option<u32>,
        /// Why.
        reason: RejectReason,
    },
}

/// Counters the driver exports as `mlp_*` series.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct SenderStats {
    /// Objects accepted into the priority queue.
    pub objects_enqueued: u64,
    /// Updates accepted (any kind).
    pub updates_enqueued: u64,
    /// Fragments released to the carrier.
    pub fragments_sent: u64,
    /// Wire bytes released to the carrier.
    pub bytes_sent: u64,
    /// Objects dropped for exceeding `max_object_bytes`.
    pub object_oversize_total: u64,
    /// Objects dropped because a newer plan covers them.
    pub superseded_total: u64,
    /// Objects deferred by the in-flight cap.
    pub in_flight_deferred_total: u64,
    /// Objects dropped when the release queue hit its bound.
    pub queue_overflow_drops_total: u64,
    /// In-flight entries force-freed after `max_in_flight_wait`.
    pub in_flight_evictions_total: u64,
    /// Echo-critical duplicates scheduled.
    pub duplicates_scheduled: u64,
    /// Update kinds, indexed `delta, anchor, idle, pulse, repair`.
    pub updates_by_kind: [u64; 5],
    /// Acks applied.
    pub acks_applied: u64,
    /// Feedback reports applied.
    pub feedback_applied: u64,
    /// Probe objects enqueued.
    pub probes_sent: u64,
    /// Probe echoes turned into RTT samples.
    pub probe_echoes: u64,
    /// Cursor objects coalesced by the latest-wins slot.
    pub cursors_coalesced: u64,
    /// Idle updates emitted.
    pub idle_frames: u64,
    /// Control objects enqueued.
    pub control_objects: u64,
}

impl SenderStats {
    /// Total updates across all kinds.
    pub fn updates_total(&self) -> u64 {
        self.updates_by_kind.iter().sum()
    }
}

/// Sender policy constants (MLP spec §7.3–§6.6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MediaSenderConfig {
    /// The carrier this sender writes to.
    pub carrier: Carrier,
    /// `min(carrier cap, configured cap)` for one fragment's payload.
    pub fragment_payload_max: usize,
    /// `in_flight_cap = 2`: how many media objects may be queued *for writing*
    /// at once (D-3 rule 4: "one object being sent + one pending replacement").
    ///
    /// The cap bounds the sender's own queue and what the carrier is handed —
    /// not how many updates may be in flight across the network. Delivery
    /// across the network is the cell accounting's job (MLP spec annex D, D-7), so an ack is
    /// never a throughput gate.
    pub in_flight_cap: usize,
    /// Bound on objects queued for release (MLP spec §4.2: bounded lanes).
    pub queue_cap: usize,
    /// Bound on queued payload bytes.
    pub queue_bytes_cap: usize,
    /// After this long in the write queue without being fully released, a
    /// media object is dropped (a stalled carrier must not pin memory).
    pub max_in_flight_wait_ms: u64,
    /// `duplicate_max_bytes = 2 KiB`.
    pub duplicate_max_bytes: u32,
    /// `loss_estimate > 30‰` enables duplication.
    pub duplicate_loss_ratio: f64,
    /// Emit idle updates to keep the ack timeline alive.
    pub idle_frames: bool,
    /// The estimator policy.
    pub bwe: BweConfig,
    /// Plan policy.
    pub plan: PlanConfig,
    /// Scheduler policy.
    pub scheduler: SchedulerConfig,
    /// Cell accounting policy.
    pub accounting: BlockAccountingConfig,
}

impl MediaSenderConfig {
    /// A configuration for a carrier and geometry.
    pub fn new(carrier: Carrier, grid: BlockGrid) -> Self {
        let accounting = BlockAccountingConfig {
            datagram: carrier.is_datagram(),
            ..BlockAccountingConfig::default()
        };
        Self {
            carrier,
            fragment_payload_max: carrier.fragment_payload_max(),
            in_flight_cap: 2,
            queue_cap: 8,
            queue_bytes_cap: 4 * 1024 * 1024,
            max_in_flight_wait_ms: 10_000,
            duplicate_max_bytes: 2_048,
            duplicate_loss_ratio: 0.03,
            idle_frames: true,
            bwe: BweConfig::default(),
            plan: PlanConfig {
                anchor_threshold: (grid.cells() / 4).max(16),
                ..PlanConfig::default()
            },
            scheduler: SchedulerConfig::default(),
            accounting,
        }
    }
}

/// One update's worth of work for the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRequest {
    /// The serialized chunk payloads, in chunk order.
    pub chunks: Vec<Vec<u8>>,
    /// Cells the update's regions cover (advances `E`).
    pub cells: Vec<u32>,
    /// Whether the update may apply onto any held state.
    pub anchor: bool,
    /// The update kind (metrics + idle/pulse handling).
    pub kind: UpdateKind,
    /// Highest input sequence reflected by this update.
    pub reflects_input_seq: u64,
    /// Total codec update bytes (all chunks).
    pub update_bytes: u32,
    /// Whether the update is echo-critical (typing).
    pub echo_critical: bool,
    /// An absolute deadline in session-relative ms.
    pub deadline_epoch_ms: Option<u64>,
}

impl UpdateRequest {
    /// A single-chunk delta update.
    pub fn delta(chunk: Vec<u8>, cells: Vec<u32>) -> Self {
        Self {
            chunks: vec![chunk],
            cells,
            anchor: false,
            kind: UpdateKind::Delta,
            reflects_input_seq: 0,
            update_bytes: 0,
            echo_critical: false,
            deadline_epoch_ms: None,
        }
    }

    /// An idle update: no cells, no cells.
    pub fn idle() -> Self {
        Self {
            chunks: vec![Vec::new()],
            cells: Vec::new(),
            anchor: false,
            kind: UpdateKind::Idle,
            reflects_input_seq: 0,
            update_bytes: 0,
            echo_critical: false,
            deadline_epoch_ms: None,
        }
    }
}

/// What happened to an enqueue request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnqueueOutcome {
    /// The object (or every chunk of the update) entered the queue.
    Accepted {
        /// The object id of the first chunk.
        object_id: u32,
        /// How many fragments the object will need.
        fragments: u16,
    },
    /// The encoder breach: an object above `max_object_bytes` was dropped.
    Oversize {
        /// Bytes it needed.
        bytes: usize,
        /// The cap in force.
        cap: u32,
    },
    /// A queued object was replaced or dropped by policy.
    Coalesced {
        /// Why.
        reason: RejectReason,
    },
    /// The lane refused the object for now; the caller must retry later.
    Deferred {
        /// Why.
        reason: RejectReason,
    },
}

/// Path-selection policy (MLP spec §7.8, S2): the hysteresis that decides when a
/// media session moves between carriers.
///
/// It is pure policy on top of measured goodput — the carriers themselves
/// (WebSocket, `rm.media`/`rm.repair` data channels) are the adapters' business
/// (W10). Every rule of §6.8 lives here, so the driver carries no thresholds:
///
/// * promotion requires `goodput(dc) >= 1.2 × goodput(ws)` sustained for
///   `3 × rtt_min`;
/// * demotion requires `goodput(dc) < 0.8 × goodput(ws)` sustained for
///   `3 × rtt_min`, or a channel close, or a probe window with
///   `delivered_ratio < 0.2`;
/// * three demotions inside 10 minutes disable DC for 10 minutes
///   (`path_oscillation_total`);
/// * never switch during an anchor plan or an input pulse.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PathSelectionConfig {
    /// Promotion ratio over the incumbent path.
    pub promote_ratio: f64,
    /// Demotion ratio under the incumbent path.
    pub demote_ratio: f64,
    /// Sustained window as a multiple of `rtt_min`.
    pub sustain_rtts: f64,
    /// Demotions inside the oscillation window that disable DC.
    pub demotions_before_sticky: u32,
    /// The oscillation window.
    pub oscillation_window_ms: u64,
    /// How long DC stays disabled after oscillation.
    pub disable_ms: u64,
}

impl Default for PathSelectionConfig {
    fn default() -> Self {
        Self {
            promote_ratio: 1.2,
            demote_ratio: 0.8,
            sustain_rtts: 3.0,
            demotions_before_sticky: 3,
            oscillation_window_ms: 600_000,
            disable_ms: 600_000,
        }
    }
}

/// One carrier switch, with the reason the driver reports as
/// `mlp_path_switches_total{from,to,reason}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSwitch {
    /// The path left behind.
    pub from: MediaPath,
    /// The path now active.
    pub to: MediaPath,
    /// `promote`, `demote`, `close` or `probe`.
    pub reason: &'static str,
}

/// The path-selection state machine (MLP spec §7.8).
#[derive(Debug, Clone)]
pub struct PathSelector {
    config: PathSelectionConfig,
    active: MediaPath,
    rtt_min_ms: f64,
    /// Goodput samples per path: `(at_ms, bps)`.
    samples: Vec<(u64, MediaPath, u64)>,
    demotions: Vec<u64>,
    disabled_until_ms: Option<u64>,
    switches: u64,
}

impl PathSelector {
    /// Start on `active` (WS for S1).
    pub fn new(active: MediaPath, config: PathSelectionConfig) -> Self {
        Self {
            config,
            active,
            rtt_min_ms: 0.0,
            samples: Vec::new(),
            demotions: Vec::new(),
            disabled_until_ms: None,
            switches: 0,
        }
    }

    /// The configuration in force.
    pub fn config(&self) -> &PathSelectionConfig {
        &self.config
    }

    /// The active path.
    pub fn active(&self) -> MediaPath {
        self.active
    }

    /// Switches applied so far.
    pub fn switches(&self) -> u64 {
        self.switches
    }

    /// Demotions inside the oscillation window (drives
    /// `path_oscillation_total`).
    pub fn demotions(&self) -> usize {
        self.demotions.len()
    }

    /// Whether `path` is currently disabled by oscillation stickiness.
    pub fn is_disabled(&self, path: MediaPath, now_ms: u64) -> bool {
        self.disabled_until_ms
            .is_some_and(|until| path.is_datagram() && now_ms < until)
    }

    /// Feed the RTT minimum the sustain windows are relative to.
    pub fn set_rtt_min_ms(&mut self, rtt_min_ms: f64) {
        self.rtt_min_ms = rtt_min_ms.max(1.0);
    }

    /// Feed one goodput sample for a path.
    ///
    /// Returns a switch when the hysteresis rules fire. `plan_active` and
    /// `pulse_active` suppress switching entirely (§6.8: never switch during an
    /// anchor plan or an input pulse).
    pub fn on_goodput(
        &mut self,
        path: MediaPath,
        goodput_bps: u64,
        now_ms: u64,
        plan_active: bool,
        pulse_active: bool,
    ) -> Option<PathSwitch> {
        self.samples.push((now_ms, path, goodput_bps));
        let horizon = (self.config.sustain_rtts * self.rtt_min_ms).max(1.0) as u64;
        let cutoff = now_ms.saturating_sub(horizon);
        self.samples.retain(|(at, _, _)| *at >= cutoff);
        if plan_active || pulse_active {
            return None;
        }
        let ws = self.median_goodput(MediaPath::Ws, cutoff);
        let dc = self.median_goodput(MediaPath::DcDirect, cutoff);
        let dc_turn = self.median_goodput(MediaPath::DcTurn, cutoff);
        let datagram = dc.or(dc_turn);
        match self.active {
            MediaPath::Ws => {
                // Only a datagram sample can carry a promotion: an incumbent
                // sample says nothing about the candidate path's health, and a
                // driver feeding one path's samples must not see spurious
                // switches.
                if !path.is_datagram() {
                    return None;
                }
                let datagram = datagram?;
                let ws = ws?;
                let target = if dc.is_some() {
                    MediaPath::DcDirect
                } else {
                    MediaPath::DcTurn
                };
                if self.is_disabled(target, now_ms) {
                    return None;
                }
                let promoted = datagram as f64 >= self.config.promote_ratio * ws as f64
                    && self.sustained(
                        target,
                        |path| {
                            self.path_goodput(path, cutoff).is_some_and(|current| {
                                current as f64 >= self.config.promote_ratio * ws as f64
                            })
                        },
                        cutoff,
                    );
                if promoted {
                    return Some(self.switch_to(target, "promote", now_ms));
                }
                None
            }
            active if active.is_datagram() => {
                if path != active {
                    return None;
                }
                let ws = ws?;
                let current = datagram.unwrap_or(0);
                let threshold = self.config.demote_ratio * ws as f64;
                let below = (current as f64) < threshold;
                let sustained = self.sustained(
                    active,
                    |other| (self.path_goodput(other, cutoff).unwrap_or(0) as f64) < threshold,
                    cutoff,
                );
                if below && sustained {
                    return Some(self.demote_to_ws("demote", now_ms));
                }
                None
            }
            _ => None,
        }
    }

    /// A carrier close demotes immediately (`reason` is `close`).
    pub fn on_carrier_close(&mut self, path: MediaPath, now_ms: u64) -> Option<PathSwitch> {
        if path == self.active && path.is_datagram() {
            self.demotions.push(now_ms);
            self.note_oscillation(now_ms);
            return Some(self.demote_to_ws("close", now_ms));
        }
        None
    }

    /// A probe window with `delivered_ratio < 0.2` demotes immediately.
    pub fn on_probe_window(
        &mut self,
        path: MediaPath,
        delivered_ratio: f64,
        now_ms: u64,
    ) -> Option<PathSwitch> {
        if path == self.active && path.is_datagram() && delivered_ratio < 0.2 {
            self.demotions.push(now_ms);
            self.note_oscillation(now_ms);
            return Some(self.demote_to_ws("probe", now_ms));
        }
        None
    }

    fn switch_to(&mut self, path: MediaPath, reason: &'static str, now_ms: u64) -> PathSwitch {
        let from = self.active;
        self.active = path;
        self.switches += 1;
        self.samples.clear();
        let _ = now_ms;
        PathSwitch {
            from,
            to: path,
            reason,
        }
    }

    fn demote_to_ws(&mut self, reason: &'static str, now_ms: u64) -> PathSwitch {
        let _ = reason;
        self.switch_to(MediaPath::Ws, reason, now_ms)
    }

    fn note_oscillation(&mut self, now_ms: u64) {
        let window = self.config.oscillation_window_ms;
        self.demotions
            .retain(|at| now_ms.saturating_sub(*at) <= window);
        if self.demotions.len() as u32 >= self.config.demotions_before_sticky {
            self.disabled_until_ms = Some(now_ms + self.config.disable_ms);
        }
    }

    fn path_goodput(&self, path: MediaPath, cutoff: u64) -> Option<u64> {
        let mut values: Vec<u64> = self
            .samples
            .iter()
            .filter(|(at, sample_path, _)| *at >= cutoff && *sample_path == path)
            .map(|(_, _, bps)| *bps)
            .collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        Some(values[values.len() / 2])
    }

    fn median_goodput(&self, path: MediaPath, cutoff: u64) -> Option<u64> {
        self.path_goodput(path, cutoff)
    }

    /// Whether every sample in the window satisfies the predicate (the
    /// "sustained for 3 × rtt_min" rule).
    fn sustained(
        &self,
        path: MediaPath,
        predicate: impl Fn(MediaPath) -> bool,
        cutoff: u64,
    ) -> bool {
        let count = self
            .samples
            .iter()
            .filter(|(at, sample_path, _)| *at >= cutoff && *sample_path == path)
            .count();
        count >= 2 && predicate(path)
    }
}

/// A released fragment, ready for the carrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundFragment {
    /// The encoded wire bytes (one MLP fragment).
    pub bytes: Vec<u8>,
    /// Transport class.
    pub class: FragmentClass,
    /// Priority field.
    pub priority: u8,
    /// Object id.
    pub object_id: u32,
    /// Fragment index.
    pub fragment_index: u16,
    /// Fragment count.
    pub fragment_count: u16,
    /// Whether this is the object's last fragment.
    pub is_fin: bool,
}

impl OutboundFragment {
    /// The fragment as a [`Transmit`] for the crate's generic driver.
    pub fn to_transmit(&self) -> Transmit {
        Transmit {
            payload: self.bytes.clone(),
            flush: true,
        }
    }
}

#[derive(Debug, Clone)]
struct OutboundObject {
    object_id: u32,
    class: FragmentClass,
    traffic: TrafficClass,
    priority: Priority,
    flags: FragmentFlags,
    ext: HeaderExt,
    payload: Vec<u8>,
    next_fragment: u16,
    fragment_count: u16,
    update_id: Option<u64>,
    cells: Vec<u32>,
    enqueued_at: Instant,
    deadline_epoch_ms: Option<u64>,
    echo_critical: bool,
    duplicated: bool,
}

impl OutboundObject {
    fn is_released(&self) -> bool {
        self.next_fragment >= self.fragment_count
    }

    /// Whether every cell this object carries is covered by `plan_blocks`.
    fn superseded_by(&self, plan_blocks: &std::collections::HashSet<u32>) -> bool {
        self.update_id.is_some()
            && !self.cells.is_empty()
            && self.cells.iter().all(|cell| plan_blocks.contains(cell))
    }
}

/// The sender machine.
#[derive(Debug, Clone)]
pub struct MediaSenderMachine {
    config: MediaSenderConfig,
    estimator: BandwidthEstimator,
    pacer: MediaPacer,
    scheduler: Scheduler,
    accounting: BlockAccounting,
    lane: LaneLifecycle,
    objects: Vec<OutboundObject>,
    next_object_id: u32,
    next_update_id: u64,
    last_emitted_frame_id: u64,
    started: Instant,
    cursor_slot: Option<usize>,
    stats: SenderStats,
    pending_echoes: Vec<(u32, u64)>,
    pending_plan_reasons: Vec<PlanStartReason>,
    last_estimate: Estimate,
    policy: MediaLanePolicy,
    last_pacer_wait: Option<(u64, Duration)>,
}

impl MediaSenderMachine {
    /// Create a sender for one receiver.
    pub fn new(config: MediaSenderConfig, grid: BlockGrid, now: Instant) -> Self {
        let estimator = BandwidthEstimator::new(config.bwe);
        let mut pacer = MediaPacer::with_config(
            PacerConfig {
                max_grant_bytes: config.fragment_payload_max as u64,
                ..PacerConfig::for_carrier(config.carrier, config.bwe.pace_floor_bps, 1_024)
            },
            now,
        );
        pacer.set_rtt(Duration::from_millis(50));
        let accounting = BlockAccounting::new(grid, config.accounting);
        let scheduler = Scheduler::new(config.scheduler, config.plan);
        let estimate = estimator.estimate();
        Self {
            config,
            estimator,
            pacer,
            scheduler,
            accounting,
            lane: LaneLifecycle::new(),
            objects: Vec::new(),
            next_object_id: 1,
            next_update_id: 1,
            last_emitted_frame_id: 0,
            started: now,
            cursor_slot: None,
            stats: SenderStats::default(),
            pending_echoes: Vec::new(),
            pending_plan_reasons: Vec::new(),
            last_estimate: estimate,
            policy: MediaLanePolicy::default(),
            last_pacer_wait: None,
        }
    }

    /// Replace the kill-switch policy.
    pub fn set_policy(&mut self, policy: MediaLanePolicy) {
        self.policy = policy;
    }

    /// The policy in force.
    pub fn policy(&self) -> &MediaLanePolicy {
        &self.policy
    }

    /// The lane lifecycle.
    pub fn lane(&self) -> &LaneLifecycle {
        &self.lane
    }

    /// The lane lifecycle, mutable.
    pub fn lane_mut(&mut self) -> &mut LaneLifecycle {
        &mut self.lane
    }

    /// Counters.
    pub fn stats(&self) -> &SenderStats {
        &self.stats
    }

    /// The estimator.
    pub fn estimator(&self) -> &BandwidthEstimator {
        &self.estimator
    }

    /// The estimator, mutable.
    pub fn estimator_mut(&mut self) -> &mut BandwidthEstimator {
        &mut self.estimator
    }

    /// The scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// The scheduler, mutable.
    pub fn scheduler_mut(&mut self) -> &mut Scheduler {
        &mut self.scheduler
    }

    /// Per-receiver cell accounting.
    pub fn accounting(&self) -> &BlockAccounting {
        &self.accounting
    }

    /// Per-receiver cell accounting, mutable.
    pub fn accounting_mut(&mut self) -> &mut BlockAccounting {
        &mut self.accounting
    }

    /// The current estimate (the only budget authority).
    pub fn estimate(&self) -> Estimate {
        self.last_estimate
    }

    /// The session-relative millisecond clock.
    pub fn session_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_millis() as u64
    }

    /// The update id the next update will use.
    pub fn next_update_id(&self) -> u64 {
        self.next_update_id
    }

    /// Queued objects (diagnostics).
    pub fn queued_objects(&self) -> usize {
        self.objects.len()
    }

    /// Media objects waiting for the carrier (the bounded write queue).
    pub fn in_flight(&self) -> usize {
        self.pending_writes()
    }

    /// Media objects queued for writing but not yet fully released.
    fn pending_writes(&self) -> usize {
        self.objects
            .iter()
            .filter(|object| object.update_id.is_some() && !object.is_released())
            .count()
    }

    /// A geometry change: resets the grid, bumps the epoch and demands a plan.
    pub fn set_grid(&mut self, grid: BlockGrid, now: Instant) -> bool {
        if grid.geometry_key() == self.accounting.grid().geometry_key() {
            return false;
        }
        self.accounting.set_geometry(grid);
        self.accounting
            .set_epoch(self.accounting.epoch().wrapping_add(1));
        self.scheduler
            .plans_mut()
            .set_anchor_threshold((grid.cells() / 4).max(16));
        self.request_plan(PlanStartReason::Geometry);
        self.refresh_estimate(now);
        true
    }

    /// Note the input/focus rectangle and open a pulse window.
    pub fn request_pulse(
        &mut self,
        focus: super::cells::CellRect,
        now: Instant,
    ) -> Vec<MediaSenderEvent> {
        let rtt_ms = self.estimator.rtt_ms().max(1.0);
        self.scheduler.mark_focus(focus);
        self.scheduler.request_pulse(self.session_ms(now), rtt_ms);
        self.scheduler.set_rtt_ms(rtt_ms);
        Vec::new()
    }

    /// Ask for an anchor plan at the next scheduler opportunity.
    pub fn request_plan(&mut self, reason: PlanStartReason) {
        self.pending_plan_reasons.push(reason);
    }

    /// Enqueue a media update (MLP spec §6.1, §6.3).
    ///
    /// The sender allocates the update id, chains `update_base` from the last
    /// emitted update and attaches the update TLVs; the payload of each chunk is
    /// the driver's serialized update object.
    pub fn enqueue_frame(&mut self, update: UpdateRequest, now: Instant) -> EnqueueOutcome {
        if !self.policy.emits_media() {
            return EnqueueOutcome::Deferred {
                reason: RejectReason::LaneDisabled,
            };
        }
        if update.chunks.is_empty() {
            return EnqueueOutcome::Deferred {
                reason: RejectReason::BodyInvalid,
            };
        }
        let estimate = self.last_estimate;
        // The encoder must produce chunks that fit one object per the budget,
        // and never need segmentation on a datagram carrier (MLP spec §6.1).
        for chunk in &update.chunks {
            let cap = if self.config.carrier.is_datagram() {
                estimate
                    .max_object_bytes
                    .min(self.config.fragment_payload_max as u32)
            } else {
                estimate.max_object_bytes
            };
            if chunk.len() > cap as usize {
                self.stats.object_oversize_total += 1;
                return EnqueueOutcome::Oversize {
                    bytes: chunk.len(),
                    cap,
                };
            }
        }
        let max_chunks = self
            .scheduler
            .max_chunks_for(self.config.carrier.is_datagram(), estimate.loss_ratio);
        if update.chunks.len() > usize::from(max_chunks.max(1)) {
            self.stats.object_oversize_total += 1;
            return EnqueueOutcome::Oversize {
                bytes: update.chunks.iter().map(Vec::len).sum(),
                cap: u32::from(max_chunks),
            };
        }
        // Bounded write queue: at most `in_flight_cap` media objects may be
        // waiting for the carrier at once.
        self.reap_write_queue(now);
        if self.pending_writes() >= self.config.in_flight_cap {
            self.stats.in_flight_deferred_total += 1;
            return EnqueueOutcome::Deferred {
                reason: RejectReason::InFlightCap,
            };
        }
        if !self.admit() {
            return EnqueueOutcome::Deferred {
                reason: RejectReason::RateLimited,
            };
        }
        let update_id = self.next_update_id;
        // Chain on what the receiver confirmed (`last_applied_update_id`), not on
        // the last update this sender emitted: a update the receiver never applied
        // cannot be the base of the next delta (MLP spec §6.1).
        let update_base = self.accounting.last_applied_update_id();
        let session_ms = self.session_ms(now);
        let part_count = update.chunks.len() as u16;
        let mut first_object_id = 0;
        let mut last_fragments = 1u16;
        for (index, chunk) in update.chunks.iter().enumerate() {
            let meta = UpdateMeta {
                update_id,
                update_base,
                part_index: index as u16,
                part_count,
                update_bytes: update.update_bytes,
                chain_epoch: self.accounting.epoch(),
                reflects_input_seq: update.reflects_input_seq,
                send_epoch_ms: session_ms,
            };
            let mut ext = meta.to_ext();
            let _ = ext.push_u32(tlv::BLOCK_COUNT, self.accounting.grid().cells());
            let mut flags = FragmentFlags::NONE;
            if update.anchor {
                flags = flags.union(FragmentFlags::ANCHOR);
            }
            if update.echo_critical {
                flags = flags.union(FragmentFlags::ECHO_REQUESTED);
            }
            let fragment_count =
                match super::pdu::fragment_count_for(chunk.len(), self.config.fragment_payload_max)
                {
                    Some(count) => count,
                    None => {
                        self.stats.object_oversize_total += 1;
                        return EnqueueOutcome::Oversize {
                            bytes: chunk.len(),
                            cap: self.config.fragment_payload_max as u32,
                        };
                    }
                };
            let object_id = self.alloc_object_id();
            if index == 0 {
                first_object_id = object_id;
            }
            last_fragments = fragment_count;
            self.objects.push(OutboundObject {
                object_id,
                class: FragmentClass::Media,
                traffic: TrafficClass::Media,
                priority: Priority::new(update.kind.mlp_priority_hint()).unwrap_or(Priority::MEDIA),
                flags,
                ext,
                payload: chunk.clone(),
                next_fragment: 0,
                fragment_count,
                update_id: Some(update_id),
                cells: update.cells.clone(),
                enqueued_at: now,
                deadline_epoch_ms: update.deadline_epoch_ms,
                echo_critical: update.echo_critical,
                duplicated: false,
            });
            if update.echo_critical {
                self.pending_echoes.push((object_id, session_ms));
            }
        }
        let _ = first_object_id;
        self.next_update_id = self.next_update_id.wrapping_add(1).max(1);
        self.last_emitted_frame_id = update_id;
        self.accounting
            .mark_emitted(update_id, &update.cells, session_ms);
        self.scheduler.timer_mut().on_frame(session_ms);
        let slot = match update.kind {
            UpdateKind::Delta => 0,
            UpdateKind::Anchor => 1,
            UpdateKind::Idle => 2,
            UpdateKind::Pulse => 3,
            UpdateKind::Repair => 4,
        };
        self.stats.updates_by_kind[slot] += 1;
        self.stats.updates_enqueued += 1;
        self.stats.objects_enqueued += part_count as u64;
        EnqueueOutcome::Accepted {
            object_id: first_object_id,
            fragments: last_fragments,
        }
    }

    /// Enqueue a cursor object (latest-wins, never retransmitted).
    pub fn enqueue_cursor(
        &mut self,
        cursor: &MediaCursor,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        if cursor.validate().is_err() || payload.len() > self.config.fragment_payload_max {
            return EnqueueOutcome::Coalesced {
                reason: RejectReason::BodyInvalid,
            };
        }
        if let Some(index) = self.cursor_slot
            && index < self.objects.len()
            && !self.objects[index].is_released()
        {
            // Replace the queued cursor in place: nothing was sent, so no
            // object id or sequence is consumed.
            let old = &mut self.objects[index];
            old.payload = payload;
            self.stats.cursors_coalesced += 1;
            return EnqueueOutcome::Coalesced {
                reason: RejectReason::SupersededByPlan,
            };
        }
        if !self.admit() {
            return EnqueueOutcome::Deferred {
                reason: RejectReason::RateLimited,
            };
        }
        let object_id = self.alloc_object_id();
        self.objects.push(OutboundObject {
            object_id,
            class: FragmentClass::Cursor,
            traffic: TrafficClass::Cursor,
            priority: Priority::CURSOR,
            flags: FragmentFlags::NONE,
            ext: HeaderExt::new(),
            payload,
            next_fragment: 0,
            fragment_count: 1,
            update_id: None,
            cells: Vec::new(),
            enqueued_at: now,
            deadline_epoch_ms: None,
            echo_critical: false,
            duplicated: false,
        });
        self.cursor_slot = Some(self.objects.len() - 1);
        self.stats.objects_enqueued += 1;
        EnqueueOutcome::Accepted {
            object_id,
            fragments: 1,
        }
    }

    /// Enqueue a control object (`MediaControlObject`).
    pub fn enqueue_control(
        &mut self,
        body: &MediaControlObject,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        let urgent = body.is_urgent();
        let object_id = self.alloc_object_id();
        let mut flags = FragmentFlags::NONE;
        if urgent {
            flags = flags.union(FragmentFlags::URGENT);
        }
        let fragment_count =
            match super::pdu::fragment_count_for(payload.len(), self.config.fragment_payload_max) {
                Some(count) => count,
                None => {
                    self.stats.object_oversize_total += 1;
                    return EnqueueOutcome::Oversize {
                        bytes: payload.len(),
                        cap: self.config.fragment_payload_max as u32,
                    };
                }
            };
        self.objects.push(OutboundObject {
            object_id,
            class: FragmentClass::Control,
            traffic: TrafficClass::Control,
            priority: Priority::CONTROL,
            flags,
            ext: HeaderExt::new(),
            payload,
            next_fragment: 0,
            fragment_count,
            update_id: None,
            cells: Vec::new(),
            enqueued_at: now,
            deadline_epoch_ms: None,
            echo_critical: false,
            duplicated: false,
        });
        self.stats.objects_enqueued += 1;
        self.stats.control_objects += 1;
        EnqueueOutcome::Accepted {
            object_id,
            fragments: fragment_count,
        }
    }

    /// Enqueue a `MediaFeedback` object (the receiver-originated report, used
    /// here for the middle hops' own reports and by the receiver role).
    pub fn enqueue_feedback(
        &mut self,
        feedback: &MediaFeedback,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        let priority = if feedback.ack.is_some() {
            Priority::CONTROL
        } else {
            Priority::REPORT
        };
        self.enqueue_simple(
            FragmentClass::Feedback,
            TrafficClass::Feedback,
            priority,
            FragmentFlags::NONE,
            HeaderExt::new(),
            payload,
            now,
        )
    }

    /// Enqueue a selective `MediaAck` object (C4, priority 0).
    pub fn enqueue_ack(
        &mut self,
        _ack: &MediaAck,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        self.enqueue_simple(
            FragmentClass::Feedback,
            TrafficClass::Feedback,
            Priority::CONTROL,
            FragmentFlags::NONE,
            HeaderExt::new(),
            payload,
            now,
        )
    }

    /// Enqueue a `MediaProbe` with `ECHO_REQUESTED` set.
    pub fn enqueue_probe(
        &mut self,
        probe: &MediaProbe,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        let mut flags = FragmentFlags::PROBE;
        flags = flags.union(FragmentFlags::ECHO_REQUESTED);
        let outcome = self.enqueue_simple(
            FragmentClass::Feedback,
            TrafficClass::Feedback,
            Priority::REPORT,
            flags,
            HeaderExt::new(),
            payload,
            now,
        );
        if let EnqueueOutcome::Accepted { object_id, .. } = outcome {
            self.pending_echoes.push((object_id, probe.send_epoch_ms));
            self.stats.probes_sent += 1;
        }
        outcome
    }

    /// Enqueue a probe echo answering a peer's probe.
    pub fn enqueue_probe_echo(
        &mut self,
        echo: &ProbeEcho,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        let mut ext = HeaderExt::new();
        let _ = ext.push_u32(tlv::ECHO_OF_OBJECT_ID, echo.object_id);
        let _ = ext.push_u64(tlv::SEND_EPOCH_MS, echo.send_epoch_ms);
        self.enqueue_simple(
            FragmentClass::Feedback,
            TrafficClass::Feedback,
            Priority::REPORT,
            FragmentFlags::NONE,
            ext,
            payload,
            now,
        )
    }

    /// Enqueue a hop report (MLP spec annex D, D-16).
    pub fn enqueue_hop_report(
        &mut self,
        _report: &MediaHopReport,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        self.enqueue_simple(
            FragmentClass::Feedback,
            TrafficClass::Feedback,
            Priority::REPORT,
            FragmentFlags::NONE,
            HeaderExt::new(),
            payload,
            now,
        )
    }

    /// Enqueue a typed media error.
    pub fn enqueue_error(
        &mut self,
        code: super::error::MediaErrorCode,
        object_id: u32,
        now: Instant,
    ) -> EnqueueOutcome {
        let mut ext = HeaderExt::new();
        let _ = ext.push_u32(tlv::ECHO_OF_OBJECT_ID, object_id);
        self.enqueue_simple(
            FragmentClass::Control,
            TrafficClass::Control,
            Priority::CONTROL,
            FragmentFlags::URGENT,
            ext,
            vec![code.as_u32() as u8],
            now,
        )
    }

    /// Close the lane (park or terminate).
    pub fn close(
        &mut self,
        code: MediaCloseCode,
        reason: &str,
        park: bool,
        now: Instant,
    ) -> Option<MediaSenderEvent> {
        let close = MediaClose {
            code,
            reason: reason.to_string(),
            park,
        };
        let _ = now;
        Some(MediaSenderEvent::CloseRequested { close })
    }

    /// Feed one receiver feedback report.
    pub fn on_feedback(&mut self, feedback: MediaFeedback, now: Instant) -> Vec<MediaSenderEvent> {
        let mut events = Vec::new();
        self.stats.feedback_applied += 1;
        if let Some(ack) = &feedback.ack
            && feedback.validate().is_ok()
        {
            events.extend(self.on_ack(*ack, now));
        }
        for echo in &feedback.probes {
            events.extend(self.on_probe_echo(*echo, now));
        }
        self.estimator.on_feedback(&feedback, now);
        self.refresh_estimate(now);
        events.extend(self.poll_plans(now));
        events
    }

    /// Feed one selective ack.
    pub fn on_ack(&mut self, ack: MediaAck, now: Instant) -> Vec<MediaSenderEvent> {
        let mut events = Vec::new();
        if ack.validate().is_err() {
            return events;
        }
        let session_ms = self.session_ms(now);
        self.stats.acks_applied += 1;
        let outcome = self.accounting.apply_ack(&ack, session_ms);
        if outcome.epoch_mismatch {
            self.request_plan(PlanStartReason::Epoch);
        }
        let stuck = self.accounting.note_stuck(session_ms);
        if !stuck.is_empty() {
            self.request_plan(PlanStartReason::Stuck);
        }
        events.extend(self.poll_plans(now));
        events
    }

    /// Feed one hop report (MLP spec annex D, D-16).
    pub fn on_hop_report(&mut self, report: MediaHopReport, now: Instant) {
        self.estimator.on_hop_report(&report, now);
    }

    /// Feed one probe echo; the RTT is `echo_epoch_ms − send_epoch_ms`.
    pub fn on_probe_echo(&mut self, echo: ProbeEcho, now: Instant) -> Vec<MediaSenderEvent> {
        self.stats.probe_echoes += 1;
        if echo.echo_epoch_ms > echo.send_epoch_ms {
            let rtt_ms = (echo.echo_epoch_ms - echo.send_epoch_ms) as f64;
            self.estimator.on_rtt_sample(rtt_ms, now);
        }
        self.pending_echoes
            .retain(|(object_id, _)| *object_id != echo.object_id);
        Vec::new()
    }

    /// Feed an inbound control close from the peer.
    ///
    /// A park close holds the receiver's state; a terminal close disables the
    /// lane. Both leave the object queue untouched: the caller decides whether
    /// to drain it (a park keeps the pending set growing, §7.2).
    pub fn on_remote_close(&mut self, close: &MediaClose, now: Instant) -> Vec<MediaSenderEvent> {
        let mut events = Vec::new();
        let session_ms = self.session_ms(now);
        if close.park {
            self.lane
                .force(MediaLaneStateKind::Parked, "remote_park", session_ms);
        } else {
            self.lane
                .force(MediaLaneStateKind::Disabled, "remote_close", session_ms);
        }
        events.push(MediaSenderEvent::LaneStateChanged(self.lane.state()));
        events
    }

    /// Feed an inbound lane state report.
    pub fn on_remote_lane_state(&mut self, state: MediaLaneStateKind) {
        if state.is_terminal() {
            self.lane.force(state, "remote_lane_state", 0);
        }
    }

    /// Check the plan ladder: restart an overdue plan, and start a new one when
    /// the accounting demands it (MLP spec §7.6).
    ///
    /// A running plan's bands are pulled by the update path alone
    /// ([`Self::plan_next_frame`] → `Scheduler::schedule`): a band is only a
    /// band once it is a update, so a poll that took one would advance the plan
    /// without painting its cells.
    pub fn poll_plans(&mut self, now: Instant) -> Vec<MediaSenderEvent> {
        let mut events = Vec::new();
        let session_ms = self.session_ms(now);
        if let Some(plan_id) = self.scheduler.plans_mut().poll_overdue(session_ms) {
            events.push(MediaSenderEvent::PlanOverdue { plan_id });
            self.request_plan(PlanStartReason::Pending);
        }
        let pending = self.accounting.pending_count();
        let stuck = self.accounting.note_stuck(session_ms);
        if self.scheduler.plans().is_active() {
            // The update path owns the bands; there is nothing to take here.
            return events;
        }
        let reason = if let Some(reason) = self.pending_plan_reasons.first().copied() {
            Some(reason)
        } else if !stuck.is_empty() {
            Some(PlanStartReason::Stuck)
        } else if pending > self.accounting.anchor_threshold() {
            Some(PlanStartReason::Pending)
        } else {
            None
        };
        if let Some(reason) = reason {
            let cells: Vec<u32> = match reason {
                PlanStartReason::Stuck => {
                    let mut cells = stuck;
                    cells.extend(self.accounting.pending_cells());
                    cells.sort_unstable();
                    cells.dedup();
                    cells
                }
                PlanStartReason::Pending | PlanStartReason::Resume => {
                    self.accounting.pending_cells()
                }
                PlanStartReason::Epoch | PlanStartReason::Geometry => {
                    (0..self.accounting.grid().cells()).collect()
                }
            };
            let cell_bytes = self
                .config
                .scheduler
                .block_bytes_for(self.accounting.grid())
                .max(1);
            let updates_estimate = cells
                .len()
                .div_ceil((self.last_estimate.budget_bytes / cell_bytes).max(1) as usize)
                as u32;
            let plan_id = self
                .scheduler
                .plans_mut()
                .start(reason, cells.clone(), session_ms);
            if let Some(plan_id) = plan_id {
                self.pending_plan_reasons.clear();
                let covered: std::collections::HashSet<u32> = cells.iter().copied().collect();
                let mut index = 0;
                while index < self.objects.len() {
                    if self.objects[index].superseded_by(&covered) {
                        let object = self.objects.remove(index);
                        self.stats.superseded_total += 1;
                        events.push(MediaSenderEvent::ObjectCoalesced {
                            object_id: object.object_id,
                            reason: RejectReason::SupersededByPlan,
                        });
                        self.reindex_slots(index);
                    } else {
                        index += 1;
                    }
                }
                events.push(MediaSenderEvent::PlanStarted {
                    plan_id,
                    reason,
                    cells: cells.len() as u32,
                    updates_estimate,
                });
            }
        }
        events
    }

    /// Build the next scheduled update from the accounting and capture state.
    pub fn plan_next_frame(
        &mut self,
        fresh: &[u32],
        refresh: &[u32],
        now: Instant,
    ) -> ScheduleDecision {
        let session_ms = self.session_ms(now);
        let due = self.accounting.due_blocks(session_ms);
        let budget = UpdateBudget {
            bytes: self.last_estimate.budget_bytes,
            max_object_bytes: self.last_estimate.max_object_bytes,
            max_chunks: self.scheduler.max_chunks_for(
                self.config.carrier.is_datagram(),
                self.last_estimate.loss_ratio,
            ),
            update_interval_ms: self.last_estimate.update_interval().as_millis() as u32,
            max_chunk_wire_bytes: 491_520,
            path_chunk_cap: self.config.fragment_payload_max as u32,
        };
        // The sender sets ANCHOR whenever it knows the receiver may not be able to
        // chain: strict patch mode, a running plan, a update that repairs cells
        // (whose base the receiver may never have applied), or an input pulse
        // (the echo-critical update must apply onto whatever the receiver holds).
        // Anchoring a repair or a pulse is what makes convergence and the echo
        // promise provable without an extra round trip — a chained update that
        // followed a lost base would otherwise be refused too.
        let pulse_active = self.scheduler.timer().pulse_active(session_ms);
        // The sender knows the receiver cannot chain whenever anything it sent is
        // still unacknowledged (a lost update breaks the chain for every delta
        // behind it), so those updates are anchors — absolute writes that apply
        // onto any held state. Anchors cost the same bytes as a delta here:
        // the regions are identical, only the application rule differs.
        let anchor = !self.policy.anchors_on_any_state()
            || self.scheduler.plans().is_active()
            || !due.is_empty()
            || pulse_active
            || self.accounting.pending_count() > 0;
        // Speculation is a *remainder* activity: while this receiver owes
        // acknowledgements or repairs, P3 cells are left out entirely, which
        // is what lets `pending` actually reach zero (the convergence
        // predicate of MLP spec annex D, D-7 / annex N, N-2) instead of trickling forever.
        let speculative = self.accounting.pending_count() == 0
            && self.last_estimate.mode.allows_speculative_refresh();
        let refresh: &[u32] = if speculative { refresh } else { &[] };
        let request = ScheduleRequest {
            due: &due,
            fresh,
            refresh,
            budget,
            anchor,
        };
        let grid = *self.accounting.grid();
        self.scheduler.schedule(request, &grid, session_ms)
    }

    /// The media-lane evidence to feed the lane lifecycle.
    pub fn lane_evidence(&self, now: Instant) -> LaneEvidence {
        let session_ms = self.session_ms(now);
        LaneEvidence {
            now_ms: session_ms,
            handshaken: true,
            unsupported: None,
            disabled: !self.policy.media_lane_enabled,
            parked: false,
            converged: self.accounting.converged(),
            pending_fraction: self.accounting.stale_fraction(),
            last_ack_ms: self.accounting.last_ack_at_ms(),
            decode_bound: false,
            no_feedback: matches!(self.last_estimate.mode, super::bwe::BweMode::NoFeedback),
            // A pace with no measurement behind it is a prior, not a verdict:
            // only a session that has heard from the receiver can be "below
            // nominal".
            degraded_below_bps: if self.stats.feedback_applied > 0 {
                64_000
            } else {
                0
            },
            pace_bps: self.last_estimate.pace_bps,
        }
    }

    /// Refresh the lane state and emit the change.
    pub fn poll_lane(&mut self, now: Instant) -> Vec<MediaSenderEvent> {
        let evidence = self.lane_evidence(now);
        let mut events = Vec::new();
        if let Some(state) = self.lane.update(&evidence) {
            events.push(MediaSenderEvent::LaneStateChanged(state));
        }
        events
    }

    /// Where the lane reports itself, for diagnostics.
    pub fn lane_state(&self) -> MediaLaneStateKind {
        self.lane.state()
    }

    /// The path the lane believes is active.
    pub fn path(&self) -> MediaPath {
        self.lane.path()
    }

    /// Note a carrier switch.
    pub fn set_path(&mut self, path: MediaPath, now: Instant) {
        let session_ms = self.session_ms(now);
        let previous = self.lane.path();
        if previous != path {
            self.lane.set_path(path);
            let _ = session_ms;
        }
    }

    /// The next fragment to release, if pacing allows.
    pub fn poll_fragment(&mut self, now: Instant) -> Option<OutboundFragment> {
        self.refresh_estimate(now);
        self.reap_write_queue(now);
        let index = self.pick_object(now)?;
        let fragment = self.build_fragment(index)?;
        let cost = fragment.len() as u64;
        let class = self.objects[index].traffic;
        match self.pacer.try_acquire(class, cost, now) {
            AcquireResult::Granted => {}
            refusal => {
                // A refusal must never be silent: the queue keeps the object and
                // the caller needs the wait to arm its timer.
                let wait = refusal.wait();
                self.last_pacer_wait = Some((cost, wait));
                return None;
            }
        }
        let object = &mut self.objects[index];
        object.next_fragment += 1;
        let is_fin = object.next_fragment >= object.fragment_count;
        let outbound = OutboundFragment {
            bytes: fragment,
            class: object.class,
            priority: object.priority.get(),
            object_id: object.object_id,
            fragment_index: object.next_fragment - 1,
            fragment_count: object.fragment_count,
            is_fin,
        };
        self.stats.fragments_sent += 1;
        self.stats.bytes_sent += outbound.bytes.len() as u64;
        self.estimator
            .on_send(outbound.bytes.len(), outbound.object_id, now);
        if is_fin {
            let (echo_critical, payload_len, already_duplicated) = {
                let object = &self.objects[index];
                (
                    object.echo_critical,
                    object.payload.len(),
                    object.duplicated,
                )
            };
            if echo_critical
                && !already_duplicated
                && self.last_estimate.loss_ratio > self.config.duplicate_loss_ratio
                && payload_len <= self.config.duplicate_max_bytes as usize
            {
                self.schedule_duplicate(index, now);
            }
            let released = self.objects[index].is_released();
            if released {
                if self.cursor_slot == Some(index) {
                    self.cursor_slot = None;
                }
                self.objects.remove(index);
                self.reindex_slots(index);
            }
        }
        Some(outbound)
    }

    /// The duration until the next possible release or machine timer.
    pub fn poll_timeout(&mut self, now: Instant) -> Option<Duration> {
        self.refresh_estimate(now);
        let mut next: Option<Duration> = None;
        let mut consider = |candidate: Duration| {
            next = Some(match next {
                Some(current) if current <= candidate => current,
                _ => candidate,
            });
        };
        if let Some(index) = self.pick_object(now) {
            let cost = match self.build_fragment(index) {
                Some(fragment) => fragment.len() as u64,
                None => 0,
            };
            let class = self.objects[index].traffic;
            consider(self.pacer.time_until(class, cost, now));
        }
        if let Some(deadline) = self.estimator.poll_timeout(now) {
            consider(deadline);
        }
        let session_ms = self.session_ms(now);
        if self.scheduler.idle_due(session_ms, self.estimator.rtt_ms()) {
            consider(Duration::ZERO);
        }
        if self.scheduler.plans().is_active() {
            consider(Duration::ZERO);
        }
        next
    }

    /// Refresh the estimate-derived policy and emit probe requests.
    pub fn handle_timeout_events(&mut self, now: Instant) -> Vec<MediaSenderEvent> {
        let mut events = Vec::new();
        self.refresh_estimate(now);
        if let Some(request) = self.estimator.probe_due(now) {
            events.push(MediaSenderEvent::ProbeDue {
                probe_id: self.next_object_id,
                payload_bytes: request.payload_bytes,
                rate_bps: request.rate_bps,
            });
        }
        events.extend(self.poll_plans(now));
        events
    }

    /// An idle update request when the timeline needs one.
    pub fn poll_idle(&mut self, now: Instant) -> Option<UpdateRequest> {
        if !self.config.idle_frames {
            return None;
        }
        let session_ms = self.session_ms(now);
        if !self.scheduler.idle_due(session_ms, self.estimator.rtt_ms()) {
            return None;
        }
        self.scheduler.on_idle(session_ms);
        self.stats.idle_frames += 1;
        Some(UpdateRequest::idle())
    }

    /// The pulse target fps while the window is open.
    pub fn target_fps(&self, now: Instant) -> f64 {
        let session_ms = self.session_ms(now);
        if self.scheduler.timer().pulse_active(session_ms) {
            self.scheduler.pulse_fps(self.last_estimate.target_fps)
        } else {
            self.last_estimate.target_fps
        }
    }

    /// The last pacer refusal: `(bytes asked, wait)`.
    pub fn last_pacer_wait(&self) -> Option<(u64, Duration)> {
        self.last_pacer_wait
    }

    /// Debug view of the write queue: `(object_id, next_fragment,
    /// fragment_count, is_media_frame)`.
    pub fn debug_queue(&self) -> Vec<(u32, u16, u16, bool)> {
        self.objects
            .iter()
            .map(|object| {
                (
                    object.object_id,
                    object.next_fragment,
                    object.fragment_count,
                    object.update_id.is_some(),
                )
            })
            .collect()
    }

    /// The pacer's 1-second window budget: the sender's INV-7 contract.
    pub fn pacer_window_budget_bytes(&self) -> u64 {
        self.pacer.window_budget_bytes()
    }

    /// The largest grant the pacer will make in one release.
    pub fn pacer_max_grant_bytes(&self) -> u64 {
        self.pacer.max_grant_bytes()
    }

    /// Whether the lane is allowed to enqueue media right now.
    pub fn can_enqueue_media(&self) -> bool {
        self.policy.emits_media() && self.in_flight() < self.config.in_flight_cap
    }

    fn enqueue_simple(
        &mut self,
        class: FragmentClass,
        traffic: TrafficClass,
        priority: Priority,
        flags: FragmentFlags,
        ext: HeaderExt,
        payload: Vec<u8>,
        now: Instant,
    ) -> EnqueueOutcome {
        if !self.admit() {
            return EnqueueOutcome::Deferred {
                reason: RejectReason::RateLimited,
            };
        }
        let object_id = self.alloc_object_id();
        let fragment_count =
            match super::pdu::fragment_count_for(payload.len(), self.config.fragment_payload_max) {
                Some(count) => count,
                None => {
                    self.stats.object_oversize_total += 1;
                    return EnqueueOutcome::Oversize {
                        bytes: payload.len(),
                        cap: self.config.fragment_payload_max as u32,
                    };
                }
            };
        self.objects.push(OutboundObject {
            object_id,
            class,
            traffic,
            priority,
            flags,
            ext,
            payload,
            next_fragment: 0,
            fragment_count,
            update_id: None,
            cells: Vec::new(),
            enqueued_at: now,
            deadline_epoch_ms: None,
            echo_critical: flags.contains(FragmentFlags::ECHO_REQUESTED),
            duplicated: false,
        });
        self.stats.objects_enqueued += 1;
        EnqueueOutcome::Accepted {
            object_id,
            fragments: fragment_count,
        }
    }

    /// Make room for one more queued object, dropping the lowest-priority,
    /// oldest object beyond the bound (an unbounded queue is a memory bug, not
    /// a priority feature — MLP spec §4.2). Urgent control objects are kept.
    fn admit(&mut self) -> bool {
        let bytes: usize = self.objects.iter().map(|object| object.payload.len()).sum();
        if self.objects.len() < self.config.queue_cap && bytes < self.config.queue_bytes_cap {
            return true;
        }
        let candidate = self
            .objects
            .iter()
            .enumerate()
            .filter(|(_, object)| !object.flags.contains(FragmentFlags::URGENT))
            .min_by_key(|(_, object)| (object.priority.get(), object.enqueued_at))
            .map(|(index, _)| index);
        match candidate {
            Some(index) => {
                let dropped = self.objects.remove(index);
                self.stats.queue_overflow_drops_total += 1;
                self.reindex_slots(index);
                let _ = dropped;
                true
            }
            None => false,
        }
    }

    fn alloc_object_id(&mut self) -> u32 {
        let id = self.next_object_id;
        self.next_object_id = self.next_object_id.checked_add(1).unwrap_or(0);
        if self.next_object_id == 0 {
            // Wrap is rejected by policy: the next allocation would restart an
            // id space the peer still holds. The lane must be re-established.
            self.next_object_id = 1;
        }
        id
    }

    /// Pick the object whose next fragment should go first.
    ///
    /// Order is by MLP priority, then by age — but an object whose next
    /// fragment the pacer cannot grant right now must not cell a smaller one
    /// behind it (a large media update waiting for window room would otherwise
    /// stall control, acks and repairs, which is exactly the head-of-line
    /// coupling the pacer exists to prevent).
    fn pick_object(&self, now: Instant) -> Option<usize> {
        let session_ms = self.session_ms(now);
        let mut best: Option<(usize, u8, Instant)> = None;
        let mut deferred_fit: Option<(usize, u8, Instant)> = None;
        for (index, object) in self.objects.iter().enumerate() {
            if object.is_released() {
                continue;
            }
            if let Some(deadline) = object.deadline_epoch_ms
                && deadline != 0
                && session_ms > deadline
            {
                continue;
            }
            let priority = object.priority.get();
            let fits = self
                .build_fragment(index)
                .map(|fragment| {
                    self.pacer
                        .can_acquire(object.traffic, fragment.len() as u64)
                })
                .unwrap_or(false);
            let is_better = |candidate: Option<(usize, u8, Instant)>| match candidate {
                None => true,
                Some((_, best_priority, best_at)) => {
                    priority < best_priority
                        || (priority == best_priority && object.enqueued_at < best_at)
                }
            };
            if fits {
                if is_better(best) {
                    best = Some((index, priority, object.enqueued_at));
                }
            } else if is_better(deferred_fit) {
                deferred_fit = Some((index, priority, object.enqueued_at));
            }
        }
        best.or(deferred_fit).map(|(index, _, _)| index)
    }

    fn build_fragment(&self, index: usize) -> Option<Vec<u8>> {
        let object = self.objects.get(index)?;
        if object.is_released() {
            return None;
        }
        let per_fragment = object
            .payload
            .len()
            .div_ceil(object.fragment_count.max(1) as usize);
        let start = object.next_fragment as usize * per_fragment;
        let end = (start + per_fragment).min(object.payload.len());
        let chunk = &object.payload[start.min(object.payload.len())..end];
        let mut flags = object.flags;
        if object.next_fragment + 1 == object.fragment_count {
            flags = flags.union(FragmentFlags::FIN);
        }
        let ext = if object.next_fragment == 0 {
            object.ext.clone()
        } else {
            HeaderExt::new()
        };
        let fragment = Fragment::new(
            object.class,
            flags,
            object.priority,
            object.object_id,
            object.next_fragment,
            object.fragment_count,
            ext,
            chunk.to_vec(),
        )
        .ok()?;
        Some(fragment.encode())
    }

    fn schedule_duplicate(&mut self, index: usize, now: Instant) {
        let (echo_of, payload_len) = {
            let object = &self.objects[index];
            (object.object_id, object.payload.len())
        };
        let source = self.objects[index].clone();
        let object_id = self.alloc_object_id();
        let mut ext = HeaderExt::new();
        let _ = ext.push_u32(tlv::ECHO_OF_OBJECT_ID, echo_of);
        let _ = ext.push_u64(tlv::SEND_EPOCH_MS, self.session_ms(now));
        let flags = source
            .flags
            .union(FragmentFlags::DUP)
            .union(FragmentFlags::RETRANSMIT);
        self.objects.push(OutboundObject {
            object_id,
            class: FragmentClass::Media,
            traffic: source.traffic,
            priority: source.priority,
            flags,
            ext,
            payload: source.payload,
            next_fragment: 0,
            fragment_count: source.fragment_count,
            update_id: source.update_id,
            cells: Vec::new(),
            enqueued_at: now,
            deadline_epoch_ms: source.deadline_epoch_ms,
            echo_critical: false,
            duplicated: true,
        });
        if let Some(object) = self.objects.get_mut(index) {
            object.duplicated = true;
        }
        self.stats.duplicates_scheduled += 1;
        let _ = payload_len;
    }

    /// Drop write-queue entries that have waited past `max_in_flight_wait_ms`.
    ///
    /// A carrier that cannot accept a fragment for that long is not going to
    /// accept it later; the object leaves and the accounting repairs its
    /// cells. Urgent control objects are kept.
    fn reap_write_queue(&mut self, now: Instant) {
        let wait = Duration::from_millis(self.config.max_in_flight_wait_ms);
        let mut index = 0;
        while index < self.objects.len() {
            let object = &self.objects[index];
            let stalled = object.update_id.is_some()
                && !object.is_released()
                && now.saturating_duration_since(object.enqueued_at) >= wait;
            if stalled {
                self.objects.remove(index);
                self.stats.in_flight_evictions_total += 1;
                self.reindex_slots(index);
            } else {
                index += 1;
            }
        }
    }

    fn reindex_slots(&mut self, removed: usize) {
        if let Some(slot) = self.cursor_slot
            && slot >= removed
        {
            self.cursor_slot = slot.checked_sub(1);
        }
    }

    fn refresh_estimate(&mut self, now: Instant) {
        if matches!(self.lane.state(), MediaLaneStateKind::Disabled) {
            return;
        }
        let estimate = self.estimator.estimate();
        self.last_estimate = estimate;
        self.pacer
            .set_pace(estimate.pace_bps, estimate.max_object_bytes, now);
        self.pacer.set_rtt(Duration::from_secs_f64(
            (self.estimator.rtt_ms().max(1.0)) / 1_000.0,
        ));
        let session_ms = self.session_ms(now);
        self.accounting.set_rtt_ms(self.estimator.rtt_ms());
        self.accounting
            .set_frame_interval_ms(estimate.update_interval().as_millis().max(1) as u64);
        self.scheduler.set_rtt_ms(self.estimator.rtt_ms());
        let _ = session_ms;
    }
}

/// The update kind's default MLP priority (MLP spec §7.3.1, §6.3.5).
impl UpdateKind {
    /// `6` for interactive media.
    pub const fn mlp_priority_hint(self) -> u8 {
        match self {
            Self::Delta | Self::Anchor | Self::Pulse | Self::Repair | Self::Idle => 6,
        }
    }
}

/// Inbound inputs the sender accepts through the generic [`StateMachine`] API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaSenderInput {
    /// A receiver feedback report.
    Feedback(MediaFeedback),
    /// A selective ack.
    Ack(MediaAck),
    /// A hop report.
    HopReport(MediaHopReport),
    /// A probe echo.
    ProbeEcho(ProbeEcho),
    /// A close from the peer.
    RemoteClose(MediaClose),
    /// A update to enqueue.
    Update(Box<UpdateRequest>),
    /// A timer tick.
    Tick,
}

impl StateMachine for MediaSenderMachine {
    type Action = crate::action::Action<MediaSenderEvent>;
    type Transmit = Transmit;
    type Receive = MediaSenderInput;

    fn handle_input(&mut self, input: Self::Receive, now: Instant) -> Vec<Self::Action> {
        let events = match input {
            MediaSenderInput::Feedback(feedback) => self.on_feedback(feedback, now),
            MediaSenderInput::Ack(ack) => self.on_ack(ack, now),
            MediaSenderInput::HopReport(report) => {
                self.on_hop_report(report, now);
                Vec::new()
            }
            MediaSenderInput::ProbeEcho(echo) => self.on_probe_echo(echo, now),
            MediaSenderInput::RemoteClose(close) => self.on_remote_close(&close, now),
            MediaSenderInput::Update(update) => {
                let outcome = self.enqueue_frame(*update, now);
                match outcome {
                    EnqueueOutcome::Oversize { bytes, cap } => {
                        vec![MediaSenderEvent::ObjectOversize { bytes, cap }]
                    }
                    EnqueueOutcome::Coalesced { reason } => {
                        vec![MediaSenderEvent::ObjectCoalesced {
                            object_id: 0,
                            reason,
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            MediaSenderInput::Tick => self.handle_timeout_events(now),
        };
        events
            .into_iter()
            .map(crate::action::Action::Custom)
            .collect()
    }

    fn poll_transmit(&mut self, now: Instant) -> Option<Self::Transmit> {
        self.poll_fragment(now)
            .map(|fragment| fragment.to_transmit())
    }

    fn poll_timeout(&mut self, now: Instant) -> Option<Duration> {
        MediaSenderMachine::poll_timeout(self, now)
    }

    fn handle_timeout(&mut self, now: Instant) -> Vec<Self::Action> {
        self.handle_timeout_events(now)
            .into_iter()
            .map(crate::action::Action::Custom)
            .collect()
    }
}

/// Input tokens the driver must release when a resume says so.
pub fn release_tokens(actions: &[super::resume::ResumeAction]) -> Vec<InputToken> {
    actions
        .iter()
        .flat_map(|action| match action {
            super::resume::ResumeAction::ReleaseKeys { tokens } => tokens.clone(),
            _ => Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender(carrier: Carrier) -> MediaSenderMachine {
        let grid = BlockGrid::with_block_size(320, 240, 64);
        MediaSenderMachine::new(MediaSenderConfig::new(carrier, grid), grid, Instant::now())
    }

    #[test]
    fn enqueue_frame_segments_and_releases_in_order() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let update = UpdateRequest::delta(vec![7u8; 100], vec![0, 1, 2]);
        let outcome = sender.enqueue_frame(update, now);
        assert!(matches!(outcome, EnqueueOutcome::Accepted { .. }));
        assert_eq!(sender.accounting().pending_count(), 3);
        let mut released = 0;
        while let Some(_fragment) = sender.poll_fragment(now) {
            released += 1;
            if released > 10 {
                panic!("pacer must bound releases");
            }
        }
        assert!(released >= 1);
        assert!(sender.stats().bytes_sent > 0);
    }

    #[test]
    fn oversize_chunks_are_dropped_and_counted() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        sender.estimator_mut().set_config(BweConfig {
            ceiling_bps: 8_000,
            ..BweConfig::default()
        });
        sender.estimator_mut().set_viewer_live(true);
        // Force a tiny budget by feeding a floor-rate feedback.
        let feedback = MediaFeedback {
            rtt_ms: 1_000,
            rtt_min_ms: 1_000,
            received_bytes: 1_000,
            received_objects: 10,
            window_ms: 1_000,
            ..MediaFeedback::default()
        };
        sender.on_feedback(feedback, now);
        let cap = sender.estimate().max_object_bytes as usize;
        let big = UpdateRequest::delta(vec![0u8; cap + 1], vec![0]);
        let outcome = sender.enqueue_frame(big, now);
        assert!(matches!(outcome, EnqueueOutcome::Oversize { .. }));
        assert_eq!(sender.stats().object_oversize_total, 1);
    }

    #[test]
    fn in_flight_cap_bounds_the_write_queue() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        for index in 0..2u32 {
            let outcome =
                sender.enqueue_frame(UpdateRequest::delta(vec![1, 2, 3], vec![index]), now);
            assert!(matches!(outcome, EnqueueOutcome::Accepted { .. }));
        }
        assert_eq!(sender.in_flight(), 2);
        let third = sender.enqueue_frame(UpdateRequest::delta(vec![1, 2, 3], vec![5]), now);
        assert!(matches!(third, EnqueueOutcome::Deferred { .. }));
        assert_eq!(sender.stats().in_flight_deferred_total, 1);
        // Releasing a queued object frees the slot; an ack does not have to
        // arrive for the encoder to continue (delivery is the accounting's
        // job, not a throughput gate).
        assert!(sender.poll_fragment(now).is_some());
        assert!(sender.in_flight() <= 1);
        let accepted = sender.enqueue_frame(UpdateRequest::delta(vec![1, 2, 3], vec![6]), now);
        assert!(matches!(accepted, EnqueueOutcome::Accepted { .. }));
    }

    #[test]
    fn control_objects_go_first_and_carry_urgent() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        sender.enqueue_frame(UpdateRequest::delta(vec![0u8; 8_000], vec![0]), now);
        let body = MediaControlObject::Close(MediaClose {
            code: MediaCloseCode::Park,
            reason: "park".into(),
            park: true,
        });
        sender.enqueue_control(&body, vec![1, 2, 3], now);
        let fragment = sender.poll_fragment(now).expect("a fragment");
        assert_eq!(fragment.class, FragmentClass::Control);
        assert_eq!(fragment.priority, 0);
    }

    #[test]
    fn cursor_slot_replaces_a_queued_cursor() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let cursor = MediaCursor {
            x_px: 1,
            y_px: 2,
            visible: true,
            ..MediaCursor::default()
        };
        let first = sender.enqueue_cursor(&cursor, vec![1], now);
        let object_id = match first {
            EnqueueOutcome::Accepted { object_id, .. } => object_id,
            other => panic!("expected accepted, got {other:?}"),
        };
        let second = sender.enqueue_cursor(&cursor, vec![2], now);
        assert!(matches!(second, EnqueueOutcome::Coalesced { .. }));
        assert_eq!(sender.queued_objects(), 1);
        assert_eq!(sender.stats().cursors_coalesced, 1);
        let fragment = sender.poll_fragment(now).expect("cursor");
        assert_eq!(fragment.object_id, object_id, "the id is reused");
        assert_eq!(fragment.class, FragmentClass::Cursor);
    }

    #[test]
    fn echo_critical_objects_duplicate_under_loss() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        // Push the estimator into a lossy steady state with a payload-sized
        // budget so the duplicate check sees loss > 30‰.
        for _ in 0..6 {
            sender.on_feedback(
                MediaFeedback {
                    rtt_ms: 100,
                    rtt_min_ms: 100,
                    received_bytes: 100_000,
                    received_objects: 90,
                    lost_objects: 10,
                    window_ms: 1_000,
                    ..MediaFeedback::default()
                },
                now,
            );
        }
        let mut update = UpdateRequest::delta(vec![9u8; 100], vec![0]);
        update.echo_critical = true;
        assert!(matches!(
            sender.enqueue_frame(update, now),
            EnqueueOutcome::Accepted { .. }
        ));
        // Pacing needs time: the pacer reports a wait, the driver advances.
        let mut released = 0;
        let mut at = now;
        for _ in 0..50 {
            if sender.poll_fragment(at).is_some() {
                released += 1;
                at += Duration::from_millis(20);
            } else {
                at += sender
                    .last_pacer_wait()
                    .map(|(_, wait)| wait.max(Duration::from_millis(1)))
                    .unwrap_or(Duration::from_millis(5));
            }
        }
        assert!(released >= 1, "the update must leave the write queue");
        assert_eq!(
            sender.stats().duplicates_scheduled,
            1,
            "loss {:?} queue {:?}",
            sender.estimate().loss_ratio,
            sender.debug_queue()
        );
    }

    #[test]
    fn plans_start_on_stuck_blocks_and_survive_polls() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let mut session_ms = 0u64;
        sender.accounting_mut().mark_emitted(1, &[0], session_ms);
        for round in 2..=6u64 {
            session_ms += 400;
            sender
                .accounting_mut()
                .mark_emitted(round, &[0], session_ms);
        }
        let stuck = sender.accounting_mut().note_stuck(session_ms + 10_000);
        assert_eq!(stuck.len(), 1);
        let at = now + Duration::from_millis(session_ms + 10_000);
        let events = sender.poll_plans(at);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, MediaSenderEvent::PlanStarted { .. })),
            "a stuck cell starts a plan"
        );
        assert!(sender.scheduler().plans().is_active());
        assert!(sender.poll_plans(at).is_empty());
        assert!(
            sender.scheduler().plans().is_active(),
            "a plan poll does not consume the plan: its bands are updates"
        );
    }

    #[test]
    fn a_plans_bands_are_pulled_by_frames_and_cover_its_block_list() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let changed = sender.set_grid(BlockGrid::with_block_size(640, 480, 64), now);
        assert!(changed, "a geometry change demands a plan");
        let events = sender.poll_plans(now);
        let plan_blocks = events
            .iter()
            .find_map(|event| match event {
                MediaSenderEvent::PlanStarted { cells, .. } => Some(*cells),
                _ => None,
            })
            .expect("a geometry change starts a plan");
        assert!(plan_blocks > 1, "the plan covers the grid");
        // Polling the plan ladder — what a timer, an ack report and a feedback
        // report each do — must not take a band: a band is a update, and only
        // the update path may pull one.
        for _ in 0..(plan_blocks as usize * 4) {
            assert!(sender.poll_plans(now).is_empty(), "a poll takes no band");
        }
        assert!(
            sender.scheduler().plans().is_active(),
            "the plan is still owed its updates"
        );
        // The update path pulls the bands and together they cover the plan's
        // cell list exactly once — no band is skipped or repeated.
        let mut covered: Vec<u32> = Vec::new();
        let mut bands = 0_u32;
        while sender.scheduler().plans().is_active() {
            let decision = sender.plan_next_frame(&[], &[], now);
            assert!(
                decision.plan_id.is_some(),
                "an active plan owns the update: {:?}",
                decision.kind
            );
            covered.extend(&decision.cells);
            bands += 1;
            assert!(bands <= plan_blocks, "every band carries cells");
        }
        assert_eq!(
            covered.len() as u32,
            plan_blocks,
            "the updates cover the plan's cell count"
        );
        covered.sort_unstable();
        covered.dedup();
        assert_eq!(
            covered.len() as u32,
            plan_blocks,
            "no planned cell is skipped or sent twice"
        );
        let (started, completed, overdue) = sender.scheduler().plans().counters();
        assert_eq!(
            (started, completed, overdue),
            (1, 1, 0),
            "one plan, completed by its updates, never overdue"
        );
    }

    #[test]
    fn lane_state_follows_accounting() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let events = sender.poll_lane(now);
        assert!(events.iter().any(|event| matches!(
            event,
            MediaSenderEvent::LaneStateChanged(MediaLaneStateKind::Live)
        )));
    }

    #[test]
    fn state_machine_adapter_releases_bytes() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let input = MediaSenderInput::Update(Box::new(UpdateRequest::delta(vec![1, 2], vec![0])));
        let actions = StateMachine::handle_input(&mut sender, input, now);
        assert!(actions.is_empty());
        let transmit = StateMachine::poll_transmit(&mut sender, now);
        assert!(transmit.is_some());
    }

    #[test]
    fn release_tokens_helper_extracts_releases() {
        use super::super::resume::ResumeAction;
        let actions = vec![
            ResumeAction::Rebase { pending_cells: 0 },
            ResumeAction::ReleaseKeys {
                tokens: vec![InputToken::Key(1)],
            },
        ];
        assert_eq!(release_tokens(&actions), vec![InputToken::Key(1)]);
    }

    #[test]
    fn geometry_change_bumps_the_epoch_and_requests_a_plan() {
        let now = Instant::now();
        let mut sender = sender(Carrier::Ws);
        let changed = sender.set_grid(BlockGrid::with_block_size(640, 480, 64), now);
        assert!(changed);
        assert_eq!(sender.accounting().epoch(), 1);
        let events = sender.poll_plans(now);
        assert!(events.iter().any(|event| matches!(
            event,
            MediaSenderEvent::PlanStarted {
                reason: PlanStartReason::Geometry,
                ..
            }
        )));
    }
}
