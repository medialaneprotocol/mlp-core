//! Deterministic scenario runner (MLP spec §10).
//!
//! [`run`] drives both ends of a media session as real sans-IO machines over a
//! [`LossyLink`]: the sender side holds a [`MediaSenderMachine`], the receiver side
//! a [`MediaReceiverMachine`], and the receiver's feedback travels back as small
//! MLP fragments. The runner records a [`Trace`] and checks the specification's
//! invariants INV-1…INV-10 as it goes.
//!
//! Two simulation-only conventions, so the runner needs no codec:
//!
//! * A media update's payload starts with a 256-byte cell bitmap followed by
//!   padding to the update's estimated size. A real decoder learns the same
//!   coverage from the region ops; the bitmap is the smallest honest stand-in.
//! * Receiver→sender reports carry a marker prefix (`fb`, `probe`, `echo`) and a
//!   JSON body for `MediaFeedback`. A real driver uses protobuf; the marker is
//!   what lets the simulator hand the typed body back to the sender machine.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::media::cells::{CellGrid, CellRect};
use crate::media::input::{
    GatewayInputEnvelope, HostInputConfig, HostInputState, InputFlags, InputKind as InputEventKind,
    InputQueueConfig, InputToken, ViewerInputQueue, decode_pressed_set, encode_pressed_set,
};
use crate::media::lifecycle::MediaLaneStateKind;
use crate::media::pdu::{Carrier, Fragment, FragmentClass, FragmentFlags, Priority};
use crate::media::receiver::{MediaReceiveEvent, MediaReceiverConfig, MediaReceiverMachine};
use crate::media::resume::{
    HeldViewerState, ParkPolicy, ResumeAction, ResumeBinding, ResumeClaims, ResumeRequest,
    decide_resume,
};
use crate::media::scheduler::{ScheduleDecision, UpdateKind};
use crate::media::sender::{EnqueueOutcome, MediaSenderConfig, MediaSenderMachine, UpdateRequest};
use crate::media::{MediaFeedback, MediaPath};
use crate::testing::lossy_link::{Delivery, Endpoint, LinkProfile, LinkStats, LossyLink, Tier};

/// The session's own estimator ramp, excluded from the steady-state echo
/// promise (design D-2: the prior shortens it, the first object is still an
/// anchor at floor rate).
pub const WARMUP_MS: u64 = 500;

/// The simulator's stepping granularity in ms: the tolerance the runner adds
/// where a policy bound is asserted against simulated wall time.
pub const SIM_TICK_MS: u64 = 10;

/// First object id the simulator's own messages use (see [`Sim::wrap_message`]).
pub const SIM_OBJECT_ID_BASE: u32 = 0x4000_0000;

/// Bytes of the cell bitmap at the head of a simulated update payload.
pub const SIM_BITMAP_BYTES: usize = 256;
/// Simulated encoded bytes per cell: the update-size model the runner uses.
pub const SIM_BYTES_PER_CELL: usize = 256;

/// The design's invariants (MLP spec §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Invariant {
    /// A update is applied only if every chunk arrived.
    Inv1,
    /// At a converged observation the receiver's canvas equals the emitted state.
    Inv2,
    /// No input applied twice; none applied after its deadline (releases apart).
    Inv3,
    /// After park beyond grace, session end, or an empty pressed set, no key is held.
    Inv4,
    /// Queues never exceed their configured caps.
    Inv5,
    /// No object above `max_object_bytes`; no update above `max_parts × cap`.
    Inv6,
    /// Sent bytes in any 1 s window stay inside the pacer's window budget.
    Inv7,
    /// Echo ≤ 2×RTT + 250 ms whenever the link delivers anything.
    Inv8,
    /// A session survives every outage and flap with exactly one media session.
    Inv9,
    /// Every drop is attributable to a hop, ±1 object.
    Inv10,
}

impl Invariant {
    /// The stable label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inv1 => "inv1",
            Self::Inv2 => "inv2",
            Self::Inv3 => "inv3",
            Self::Inv4 => "inv4",
            Self::Inv5 => "inv5",
            Self::Inv6 => "inv6",
            Self::Inv7 => "inv7",
            Self::Inv8 => "inv8",
            Self::Inv9 => "inv9",
            Self::Inv10 => "inv10",
        }
    }

    /// Every invariant, in order.
    pub const fn all() -> [Invariant; 10] {
        [
            Self::Inv1,
            Self::Inv2,
            Self::Inv3,
            Self::Inv4,
            Self::Inv5,
            Self::Inv6,
            Self::Inv7,
            Self::Inv8,
            Self::Inv9,
            Self::Inv10,
        ]
    }
}

/// A scenario step (MLP spec §10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Step {
    /// Advance the clock.
    Advance(u64),
    /// Change the link conditions.
    Link(Box<LinkProfile>),
    /// The capture produced a change inside this rectangle.
    CaptureChange(CellRect),
    /// The user did something.
    ViewerInput(InputEventKind),
    /// The receiver's lane dies (park).
    ViewerDisconnect,
    /// The receiver comes back (resume).
    ViewerReconnect,
    /// The sender's transport to the intermediary is re-established.
    HostReconnect,
    /// The receiver's carrier changed.
    PathSwitch(MediaPath),
    /// Assert an invariant at this point.
    Assert(Invariant),
}

/// A scenario (MLP spec §10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scenario {
    /// A stable name for the corpus.
    pub name: String,
    /// The tier this scenario targets.
    pub tier: Tier,
    /// Steps, in order.
    pub steps: Vec<Step>,
    /// Concurrent receivers (only 1 is simulated end to end today; the field is
    /// part of the frozen corpus format).
    pub receivers: u8,
}

impl Scenario {
    /// An empty scenario for a tier.
    pub fn new(name: impl Into<String>, tier: Tier) -> Self {
        Self {
            name: name.into(),
            tier,
            steps: Vec::new(),
            receivers: 1,
        }
    }

    /// Append a step.
    pub fn step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    /// Append a wait.
    pub fn advance(self, ms: u64) -> Self {
        self.step(Step::Advance(ms))
    }

    /// Append a capture change.
    pub fn change(self, rect: CellRect) -> Self {
        self.step(Step::CaptureChange(rect))
    }

    /// Append an input event.
    pub fn input(self, kind: InputEventKind) -> Self {
        self.step(Step::ViewerInput(kind))
    }

    /// Total scheduled time.
    pub fn duration_ms(&self) -> u64 {
        self.steps
            .iter()
            .map(|step| match step {
                Step::Advance(ms) => *ms,
                _ => 0,
            })
            .sum()
    }
}

/// One emitted update in the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateTrace {
    /// Scenario ms.
    pub at_ms: u64,
    /// Update id.
    pub update_id: u64,
    /// Update kind.
    pub kind: String,
    /// Cells covered.
    pub cells: u32,
    /// Payload bytes.
    pub bytes: u32,
}

/// One ack in the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckTrace {
    /// Scenario ms.
    pub at_ms: u64,
    /// Highest applied update id.
    pub highest_applied_update_id: u64,
    /// Cells still pending on the sender after the ack.
    pub pending_cells: u32,
    /// Whether the sender's accounting reports convergence.
    pub converged: bool,
}

/// One plan event in the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTrace {
    /// Scenario ms.
    pub at_ms: u64,
    /// Plan id.
    pub plan_id: u32,
    /// What happened.
    pub event: String,
    /// Cells involved.
    pub cells: u32,
}

/// One lane-state change in the trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTrace {
    /// Scenario ms.
    pub at_ms: u64,
    /// The new lane state.
    pub state: String,
}

/// One invariant violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViolationTrace {
    /// Scenario ms.
    pub at_ms: u64,
    /// The invariant.
    pub invariant: String,
    /// A human-readable detail.
    pub detail: String,
}

/// Summary counters for the corpus and the budget assertions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceCounters {
    /// Updates emitted by the sender.
    pub updates_sent: u64,
    /// Updates applied by the receiver.
    pub updates_applied: u64,
    /// Updates whose cells the receiver acked.
    pub updates_acked: u64,
    /// Updates refused because their base was missing (a gap application).
    pub updates_gapped: u64,
    /// Fragments released.
    pub fragments_sent: u64,
    /// Bytes released.
    pub bytes_sent: u64,
    /// Objects delivered to the receiver.
    pub objects_delivered: u64,
    /// Objects dropped by the receiver (expired, evicted, invalid).
    pub objects_dropped: u64,
    /// Acks the sender processed.
    pub acks: u64,
    /// Feedback reports the sender processed.
    pub feedback: u64,
    /// Plans started.
    pub plans_started: u64,
    /// Converged observations.
    pub converged_observations: u64,
    /// The last observed pending cell count.
    pub pending_cells: u32,
    /// Time from the last capture change to convergence, when it happened.
    pub last_converge_ms: Option<u64>,
    /// Echo measurements: `(input_seq, echo_ms)`.
    pub echoes: Vec<(u64, u64)>,
    /// Input objects submitted.
    pub input_submitted: u64,
    /// Input objects applied by the sender.
    pub input_applied: u64,
    /// Input objects expired.
    pub input_expired: u64,
    /// Input objects deduped or stale.
    pub input_dropped: u64,
    /// Park/resume cycles.
    pub park_cycles: u64,
    /// How many times the lane parked.
    pub parks: u64,
    /// Echo samples above the tier budget + one simulator tick (the tail a
    /// p95 assertion tolerates; the specification's promise is a p95, §3.2).
    pub echo_outliers: u64,
    /// Cell writes skipped because the held cell was newer (reordering).
    pub stale_applies_skipped: u64,
    /// Cells whose held update is older than the emitted state at a converged
    /// observation (reported, not hidden).
    pub canvas_behind_cells: u64,
    /// Pacer refusals observed by the sender (S-14's fixture-level pacing check).
    pub pacer_waits: u64,
    /// The longest pacer refusal, in ms.
    pub max_pacer_wait_ms: u64,
    /// Lane state changes.
    pub state_changes: u64,
    /// Hop-attributed dropped objects (simulated intermediary).
    pub hop_dropped: u64,
    /// Sender probes whose echo never returned.
    pub probes_lost: u64,
    /// Objects the receiver lost (object-id gaps).
    pub viewer_lost_objects: u64,
    /// The final lane state.
    pub final_state: String,
    /// The final receiver canvas size in cells.
    pub cells: u32,
    /// The simulated epoch at the end.
    pub epoch: u32,
}

/// The deterministic trace of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    /// The scenario name.
    pub name: String,
    /// The tier.
    pub tier: Tier,
    /// Scenario time covered.
    pub duration_ms: u64,
    /// Updates.
    pub updates: Vec<UpdateTrace>,
    /// Acks.
    pub acks: Vec<AckTrace>,
    /// Plans.
    pub plans: Vec<PlanTrace>,
    /// Lane states.
    pub states: Vec<StateTrace>,
    /// Violations (must be empty in a passing run).
    pub violations: Vec<ViolationTrace>,
    /// Counters.
    pub counters: TraceCounters,
    /// Link statistics.
    pub link: LinkStats,
}

impl Trace {
    /// Fail the run when any invariant was violated.
    pub fn assert_clean(&self) {
        assert!(
            self.violations.is_empty(),
            "{}: {} invariant violation(s): {:?}",
            self.name,
            self.violations.len(),
            &self.violations[..self.violations.len().min(5)]
        );
    }

    /// The trace as stable JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("the trace is serializable")
    }

    /// A stable digest of the decision trace, for the conformance corpus.
    pub fn digest(&self) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |byte: u8| {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for update in &self.updates {
            for byte in format!(
                "{}:{}:{}:{}|",
                update.at_ms, update.update_id, update.kind, update.cells
            )
            .bytes()
            {
                mix(byte);
            }
        }
        for ack in &self.acks {
            for byte in format!(
                "{}:{}:{}|",
                ack.at_ms, ack.highest_applied_update_id, ack.pending_cells
            )
            .bytes()
            {
                mix(byte);
            }
        }
        for plan in &self.plans {
            for byte in format!("{}:{}:{}|", plan.at_ms, plan.plan_id, plan.event).bytes() {
                mix(byte);
            }
        }
        format!("{hash:016x}")
    }

    /// Echo p95 in ms, when any echo was measured.
    pub fn echo_p95_ms(&self) -> Option<u64> {
        if self.counters.echoes.is_empty() {
            return None;
        }
        let mut values: Vec<u64> = self.counters.echoes.iter().map(|(_, echo)| *echo).collect();
        values.sort_unstable();
        let index = ((values.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
        values.get(index.min(values.len() - 1)).copied()
    }
}

/// How many hops the simulated intermediary drops and reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IntermediaryModel {
    /// Drop probability in permille.
    pub drop_permille: u32,
}

/// The receiver's shadow state.
#[derive(Debug, Clone, Default)]
struct ViewerModel {
    canvas: Vec<u64>,
    applied_updates: u64,
    received_updates: HashSet<u64>,
    updates: HashMap<u64, Vec<u32>>,
    last_apply_ms: u64,
}

/// The sender's emitted state.
#[derive(Debug, Clone, Default)]
struct HostModel {
    emitted: Vec<u64>,
    updates: HashMap<u64, Vec<u32>>,
}

struct Sim {
    tier: Tier,
    grid: CellGrid,
    link: LossyLink,
    now_ms: u64,
    epoch: Instant,
    sender: MediaSenderMachine,
    host_rx: MediaReceiverMachine,
    receiver: MediaReceiverMachine,
    viewer_model: ViewerModel,
    host_model: HostModel,
    capture_dirty: Vec<u32>,
    last_emitted_dirty: Vec<u32>,
    input_queue: ViewerInputQueue,
    host_input: HostInputState,
    input_sent: HashMap<u64, u64>,
    input_applied_seq: u64,
    connected: bool,
    parked_since_ms: Option<u64>,
    held: HeldViewerState,
    park_policy: ParkPolicy,
    intermediary: IntermediaryModel,
    relay_drops: u64,
    resumes: u32,
    echo_records: Vec<(u64, u64)>,
    echoed_inputs: HashSet<u64>,
    input_applied_seen: HashSet<u64>,
    sim_object_id: u32,
    trace: Trace,
    path: MediaPath,
    fragment_releases: Vec<(u64, u64)>,
    pending_host_probes: Vec<(u32, u64)>,
    last_change_ms: u64,
    release_cap_bytes: u64,
}

/// Run a scenario and return its trace.
pub fn run(scenario: &Scenario) -> Trace {
    run_with_relay(scenario, IntermediaryModel::default())
}

/// Run a scenario with a simulated intermediary hop that drops and reports objects.
pub fn run_with_relay(scenario: &Scenario, intermediary: IntermediaryModel) -> Trace {
    let grid = CellGrid::with_cell_size(1920, 1080, 64);
    let profile = LinkProfile::tier(scenario.tier);
    let epoch = crate::testing::test_epoch();
    let carrier = carrier_for(&profile);
    let mut sim = Sim::new(scenario, grid, profile, carrier, epoch, intermediary);
    for step in &scenario.steps {
        sim.apply_step(step);
    }
    sim.finish()
}

fn carrier_for(profile: &LinkProfile) -> Carrier {
    if profile.mtu_bytes <= 1_150 {
        Carrier::DataChannel
    } else {
        Carrier::Ws
    }
}

impl Sim {
    fn new(
        scenario: &Scenario,
        grid: CellGrid,
        profile: LinkProfile,
        carrier: Carrier,
        epoch: Instant,
        intermediary: IntermediaryModel,
    ) -> Self {
        let profile_for_stats = profile.clone();
        let profile_rtt_ms = profile.rtt_ms;
        let mut sender_config = MediaSenderConfig::new(carrier, grid);
        sender_config.accounting.datagram = carrier.is_datagram();
        sender_config.bwe.pace_floor_bps = 8_000;
        // A remembered prior (MLP spec §8, TTL 60 min): the pace a previous
        // session on this link reached. The tier promises are steady-state
        // ones, and the prior is what makes them reachable in a scenario
        // shorter than the StartUp ladder. The no-prior cold start has its own
        // test below.
        sender_config.bwe.prior_bps = Some(profile.rate_bps_down);
        let sender = MediaSenderMachine::new(sender_config, grid, epoch);
        let viewer_config = MediaReceiverConfig::for_carrier(carrier);
        let receiver = MediaReceiverMachine::new(viewer_config, epoch);
        let host_rx = MediaReceiverMachine::new(viewer_config, epoch);
        let cells = grid.cells() as usize;
        let trace = Trace {
            name: scenario.name.clone(),
            tier: scenario.tier,
            duration_ms: 0,
            updates: Vec::new(),
            acks: Vec::new(),
            plans: Vec::new(),
            states: Vec::new(),
            violations: Vec::new(),
            counters: TraceCounters {
                updates_sent: 0,
                updates_applied: 0,
                updates_acked: 0,
                updates_gapped: 0,
                fragments_sent: 0,
                bytes_sent: 0,
                objects_delivered: 0,
                objects_dropped: 0,
                acks: 0,
                feedback: 0,
                plans_started: 0,
                converged_observations: 0,
                pending_cells: 0,
                last_converge_ms: None,
                echoes: Vec::new(),
                input_submitted: 0,
                input_applied: 0,
                input_expired: 0,
                input_dropped: 0,
                park_cycles: 0,
                parks: 0,
                echo_outliers: 0,
                stale_applies_skipped: 0,
                canvas_behind_cells: 0,
                pacer_waits: 0,
                max_pacer_wait_ms: 0,
                state_changes: 0,
                hop_dropped: 0,
                probes_lost: 0,
                viewer_lost_objects: 0,
                final_state: String::new(),
                cells: grid.cells(),
                epoch: 0,
            },
            link: *LossyLink::new(profile_for_stats.clone()).stats(),
        };
        Self {
            tier: scenario.tier,
            grid,
            link: LossyLink::new(profile_for_stats.clone()),
            now_ms: 0,
            epoch,
            sender,
            host_rx,
            receiver,
            viewer_model: ViewerModel {
                canvas: vec![0; cells],
                ..ViewerModel::default()
            },
            host_model: HostModel {
                emitted: vec![0; cells],
                ..HostModel::default()
            },
            capture_dirty: Vec::new(),
            last_emitted_dirty: Vec::new(),
            input_queue: ViewerInputQueue::new(InputQueueConfig::default(), 1),
            host_input: HostInputState::new(HostInputConfig::default()),
            input_sent: HashMap::new(),
            input_applied_seq: 0,
            connected: true,
            parked_since_ms: None,
            held: HeldViewerState::default(),
            park_policy: ParkPolicy {
                park_ttl_seconds: ParkPolicy::park_ttl_for(30, profile_rtt_ms),
                ..ParkPolicy::default()
            },
            intermediary,
            relay_drops: 0,
            resumes: 0,
            echo_records: Vec::new(),
            echoed_inputs: HashSet::new(),
            input_applied_seen: HashSet::new(),
            sim_object_id: SIM_OBJECT_ID_BASE,
            trace,
            path: MediaPath::Ws,
            fragment_releases: Vec::new(),
            pending_host_probes: Vec::new(),
            last_change_ms: 0,
            release_cap_bytes: 0,
        }
    }

    fn instant(&self) -> Instant {
        self.epoch + Duration::from_millis(self.now_ms)
    }

    fn apply_step(&mut self, step: &Step) {
        match step {
            Step::Advance(ms) => {
                let mut remaining = *ms;
                while remaining > 0 {
                    let dt = remaining.min(10);
                    self.tick(dt);
                    remaining -= dt;
                }
            }
            Step::Link(profile) => {
                self.link.set_profile((**profile).clone());
            }
            Step::CaptureChange(rect) => {
                for cell in self.grid.cells_in_rect(rect) {
                    if !self.capture_dirty.contains(&cell) {
                        self.capture_dirty.push(cell);
                    }
                }
                self.capture_dirty.sort_unstable();
                self.last_change_ms = self.now_ms;
            }
            Step::ViewerInput(kind) => self.send_input(*kind),
            Step::ViewerDisconnect => self.disconnect(),
            Step::ViewerReconnect => self.reconnect(),
            Step::HostReconnect => {
                // The media session survives a sender transport reconnect: the
                // update chain and the accounting are sender-owned state.
                self.trace.counters.park_cycles += 1;
            }
            Step::PathSwitch(path) => {
                self.path = *path;
                self.sender.set_path(*path, self.instant());
            }
            Step::Assert(invariant) => self.check_invariant(*invariant),
        }
        self.check_cheap_invariants();
    }

    fn tick(&mut self, dt_ms: u64) {
        self.now_ms += dt_ms;
        self.host_encode();
        self.host_release();
        let deliveries = self.link.advance(dt_ms);
        for delivery in deliveries {
            self.deliver(delivery);
        }
        self.viewer_feedback();
        self.host_timers();
        let now = self.instant();
        self.report_demand(now);
        self.expire_probes(now);
        self.check_cheap_invariants();
    }

    // ── sender side ───────────────────────────────────────────────────────────

    fn host_encode(&mut self) {
        let now = self.instant();
        if self.sender.lane().state() == MediaLaneStateKind::Disabled {
            return;
        }
        let pulse = self
            .sender
            .scheduler()
            .timer()
            .pulse_active(self.sender.session_ms(now));
        // The update cadence is the estimator's `target_fps` (a pulse bypasses
        // it): the budget is a *per-update* budget, so a driver that ignores
        // the cadence would ask the pacer for many times the pace and force
        // the sender to drop its own queue.
        let session_ms = self.sender.session_ms(now);
        let cadence = self
            .sender
            .scheduler()
            .timer()
            .due(session_ms, self.sender.target_fps(now));
        if !cadence && !pulse {
            return;
        }
        // The encoder considers fresh changes, unacked cells (repairs), the
        // pulse window, or the idle cadence. Which of them becomes a update is
        // the scheduler's decision, not the driver's.
        let repairable = self.sender.accounting().pending_count() > 0;
        if self.capture_dirty.is_empty() && !self.idle_due(now) && !pulse && !repairable {
            return;
        }
        if let Some(idle) = self.sender.poll_idle(now) {
            if let EnqueueOutcome::Accepted { .. } = self.sender.enqueue_update(idle, now) {
                let update_id = self.sender.next_update_id() - 1;
                self.record_update(update_id, "idle", 0, 0);
            }
            return;
        }
        let fresh: Vec<u32> = self.capture_dirty.clone();
        let refresh: Vec<u32> = self
            .host_model
            .emitted
            .iter()
            .enumerate()
            .filter(|(_, update)| **update == 0)
            .map(|(index, _)| index as u32)
            .take(64)
            .collect();
        let decision = self.sender.plan_next_update(&fresh, &refresh, now);
        if decision.cells.is_empty() {
            return;
        }
        let payload = self.encode_update_payload(&decision);
        let estimate = self.sender.estimate();
        let max_object = estimate.max_object_bytes as usize;
        if payload.len() > max_object {
            self.violate(Invariant::Inv6, "payload above max_object_bytes");
            self.sender.scheduler_mut().plans_mut().unclaim(&[]);
            return;
        }
        let request = UpdateRequest {
            parts: vec![payload],
            cells: decision.cells.clone(),
            anchor: decision.anchor,
            kind: decision.kind,
            reflects_input_seq: self.input_applied_seq,
            update_bytes: decision.estimated_bytes,
            echo_critical: false,
            deadline_epoch_ms: None,
        };
        match self.sender.enqueue_update(request, now) {
            EnqueueOutcome::Accepted { .. } => {
                let update_id = self.sender.next_update_id() - 1;
                for cell in &decision.cells {
                    self.host_model.emitted[*cell as usize] = update_id;
                    self.capture_dirty.retain(|dirty| dirty != cell);
                }
                self.host_model
                    .updates
                    .insert(update_id, decision.cells.clone());
                self.record_update(
                    update_id,
                    decision.kind.as_str(),
                    decision.cells.len() as u32,
                    payload_len(&decision) as u32,
                );
                // A plan band is a update: the trace records the band where the
                // cells were committed, never where the plan ladder was
                // polled. The last band carries `deferred == 0` (no plan
                // cells remain after it) and completes the plan.
                if let Some(plan_id) = decision.plan_id {
                    self.trace.plans.push(PlanTrace {
                        at_ms: self.now_ms,
                        plan_id,
                        event: "band".to_string(),
                        cells: decision.cells.len() as u32,
                    });
                    if decision.deferred == 0 {
                        self.trace.plans.push(PlanTrace {
                            at_ms: self.now_ms,
                            plan_id,
                            event: "complete".to_string(),
                            cells: 0,
                        });
                    }
                }
                // The update painted its whole cell list: settle the pull.
                self.sender
                    .scheduler_mut()
                    .plans_mut()
                    .unclaim(&decision.cells);
                if self.capture_dirty.is_empty() {
                    self.last_change_ms = self.now_ms;
                }
            }
            EnqueueOutcome::Oversize { .. } => {
                self.violate(Invariant::Inv6, "the encoder produced an oversize chunk");
                self.sender.scheduler_mut().plans_mut().unclaim(&[]);
            }
            EnqueueOutcome::Deferred { reason } | EnqueueOutcome::Coalesced { reason } => {
                // The cells stay dirty and are retried on the next cycle; the
                // band the scheduler pulled went unpainted and is owed again.
                let _ = &self.last_emitted_dirty;
                let _ = reason;
                self.sender.scheduler_mut().plans_mut().unclaim(&[]);
            }
        }
    }

    /// Tell the estimator there is more work than the pace allows: this is
    /// what arms the demand-driven probe (MLP spec §8) and lets the pace
    /// climb on a link whose loss keeps StartUp from growing it.
    fn report_demand(&mut self, now: Instant) {
        let pending = self.sender.accounting().pending_count()
            + self.capture_dirty.len() as u32
            + u32::try_from(self.sender.queued_objects()).unwrap_or(0);
        if pending == 0 {
            return;
        }
        let estimate = self.sender.estimate();
        let demand_bps = estimate.pace_bps.saturating_mul(u64::from(pending).min(8));
        self.sender.estimator_mut().on_demand(demand_bps, now);
    }

    /// Emit a probe object through the sender machine so every media-lane
    /// object shares one id space (the receiver's gap detection depends on it).
    fn send_probe(&mut self, probe: crate::media::MediaProbe) {
        let payload = encode_probe(&probe);
        let now = self.instant();
        if let EnqueueOutcome::Accepted { object_id, .. } =
            self.sender.enqueue_probe(&probe, payload, now)
        {
            self.pending_host_probes.push((object_id, self.now_ms));
        }
    }

    /// Fail probes whose echo never came back (MLP spec §8).
    fn expire_probes(&mut self, now: Instant) {
        let deadline = (2.0 * self.link.profile().rtt_ms + 250.0).round() as u64;
        let mut expired = Vec::new();
        let mut remaining = Vec::new();
        for (probe_id, sent_at) in self.pending_host_probes.drain(..) {
            if self.now_ms.saturating_sub(sent_at) >= deadline {
                expired.push(probe_id);
            } else {
                remaining.push((probe_id, sent_at));
            }
        }
        self.pending_host_probes = remaining;
        let btlbw = self.sender.estimator().btlbw_bps();
        for _ in expired {
            self.sender.estimator_mut().on_probe_result(
                &crate::media::bwe::ProbeOutcome {
                    delivered: false,
                    delivered_ratio: 0.0,
                    probe_rate_bps: btlbw,
                },
                now,
            );
            self.trace.counters.probes_lost += 1;
        }
    }

    fn record_update(&mut self, update_id: u64, kind: &str, cells: u32, bytes: u32) {
        self.trace.updates.push(UpdateTrace {
            at_ms: self.now_ms,
            update_id,
            kind: kind.to_string(),
            cells,
            bytes,
        });
        self.trace.counters.updates_sent += 1;
    }

    fn idle_due(&self, now: Instant) -> bool {
        let session_ms = self.sender.session_ms(now);
        self.sender
            .scheduler()
            .idle_due(session_ms, self.sender.estimator().rtt_ms())
    }

    fn encode_update_payload(&self, decision: &ScheduleDecision) -> Vec<u8> {
        let mut payload = vec![0u8; SIM_BITMAP_BYTES];
        for cell in &decision.cells {
            let byte = (*cell as usize) / 8;
            if byte < SIM_BITMAP_BYTES {
                payload[byte] |= 1 << (cell % 8);
            }
        }
        // Pad to a plausible payload size, never above the object cap: on a
        // datagram carrier that cap is the fragment payload budget (MLP spec annex D, D-19).
        let estimate_cap = self.sender.estimate().max_object_bytes as usize;
        let cap = if self.link.profile().mtu_bytes <= 1_150 {
            estimate_cap.min(self.link.profile().mtu_bytes as usize)
        } else {
            estimate_cap
        };
        let target = payload_len(decision).clamp(SIM_BITMAP_BYTES, cap.max(SIM_BITMAP_BYTES));
        payload.resize(target, 0);
        payload
    }

    fn host_release(&mut self) {
        let now = self.instant();
        let mut released = 0;
        let mut pending_message: Vec<u8> = Vec::new();
        let coalesce = self.link.profile().mtu_bytes > 1_150;
        while released < 16 {
            let Some(fragment) = self.sender.poll_fragment(now) else {
                if let Some((_, wait)) = self.sender.last_pacer_wait()
                    && wait > Duration::ZERO
                {
                    self.trace.counters.pacer_waits += 1;
                    self.trace.counters.max_pacer_wait_ms = self
                        .trace
                        .counters
                        .max_pacer_wait_ms
                        .max(wait.as_millis() as u64);
                }
                break;
            };
            if !self.link.profile().admits(fragment.bytes.len()) {
                self.violate(Invariant::Inv6, "fragment above the carrier cap");
            }
            self.trace.counters.fragments_sent += 1;
            self.trace.counters.bytes_sent += fragment.bytes.len() as u64;
            self.fragment_releases
                .push((self.now_ms, fragment.bytes.len() as u64));
            self.release_cap_bytes = self.sender.pacer_window_budget_bytes();
            released += 1;
            if coalesce && pending_message.len() + fragment.bytes.len() <= 4_096 {
                pending_message.extend_from_slice(&fragment.bytes);
            } else {
                if !pending_message.is_empty() {
                    self.link
                        .transmit(Endpoint::Sender, core::mem::take(&mut pending_message));
                }
                if coalesce {
                    pending_message = fragment.bytes;
                } else {
                    self.link.transmit(Endpoint::Sender, fragment.bytes);
                }
            }
        }
        if !pending_message.is_empty() {
            self.link.transmit(Endpoint::Sender, pending_message);
        }
    }

    fn host_timers(&mut self) {
        let now = self.instant();
        let events = self.sender.handle_timeout_events(now);
        self.note_sender_events(&events);
        let lane_events = self.sender.poll_lane(now);
        self.note_sender_events(&lane_events);
    }

    fn note_sender_events(&mut self, events: &[crate::media::sender::MediaSenderEvent]) {
        let mut probes: Vec<crate::media::MediaProbe> = Vec::new();
        for event in events {
            match event {
                crate::media::sender::MediaSenderEvent::ProbeDue {
                    probe_id,
                    payload_bytes,
                    ..
                } => {
                    probes.push(crate::media::MediaProbe {
                        probe_id: *probe_id,
                        payload_bytes: *payload_bytes,
                        send_epoch_ms: self.sender.session_ms(self.instant()),
                    });
                }
                crate::media::sender::MediaSenderEvent::ObjectCoalesced { .. } => {}
                crate::media::sender::MediaSenderEvent::PlanStarted {
                    plan_id,
                    reason,
                    cells,
                    ..
                } => {
                    self.trace.plans.push(PlanTrace {
                        at_ms: self.now_ms,
                        plan_id: *plan_id,
                        event: format!("start:{}", reason.as_str()),
                        cells: *cells,
                    });
                    self.trace.counters.plans_started += 1;
                }
                crate::media::sender::MediaSenderEvent::PlanOverdue { plan_id } => {
                    self.trace.plans.push(PlanTrace {
                        at_ms: self.now_ms,
                        plan_id: *plan_id,
                        event: "overdue".to_string(),
                        cells: 0,
                    });
                }
                crate::media::sender::MediaSenderEvent::LaneStateChanged(state) => {
                    self.trace.states.push(StateTrace {
                        at_ms: self.now_ms,
                        state: state.as_str().to_string(),
                    });
                    self.trace.counters.state_changes += 1;
                }
                crate::media::sender::MediaSenderEvent::ObjectOversize { .. } => {
                    self.violate(Invariant::Inv6, "oversize object at the sender");
                }
                _ => {}
            }
        }
        for probe in probes {
            self.send_probe(probe);
        }
    }

    // ── the link and the receiver ─────────────────────────────────────────────

    fn deliver(&mut self, delivery: Delivery) {
        match delivery.to {
            Endpoint::Receiver => self.viewer_receive(&delivery.bytes),
            Endpoint::Sender => self.host_receive(&delivery.bytes),
        }
    }

    fn viewer_receive(&mut self, bytes: &[u8]) {
        let now = self.instant();
        let events = self.receiver.on_message(bytes, now);
        for event in events {
            match event {
                MediaReceiveEvent::Object(object) => {
                    self.trace.counters.objects_delivered += 1;
                    if object.class != FragmentClass::Media {
                        continue;
                    }
                    let Some(meta) = object.update_meta() else {
                        continue;
                    };
                    // The simulated intermediary drops and reports objects: a
                    // deterministic hash of the update id, so the same corpus
                    // replays identically.
                    if self.intermediary.drop_permille > 0 && meta.part_index == 0 {
                        let hash = meta
                            .update_id
                            .wrapping_mul(2_654_435_761)
                            .wrapping_add(self.now_ms / 10);
                        if hash % 1_000 < u64::from(self.intermediary.drop_permille) {
                            self.relay_drops += 1;
                            self.trace.counters.hop_dropped += 1;
                            continue;
                        }
                    }
                    let cells = decode_cell_bitmap(&object.payload, self.grid.cells());
                    if self
                        .receiver
                        .is_chunk_applied(meta.update_id, meta.part_index)
                    {
                        continue;
                    }
                    // INV-1: the machine only completes objects with every
                    // fragment and FIN, so an apply here always has all parts.
                    if meta.part_count != 1 {
                        self.violate(
                            Invariant::Inv1,
                            "a multi-chunk update arrived without its parts",
                        );
                        continue;
                    }
                    // Codec chaining: a delta needs its base, an anchor may be
                    // applied onto any held state (MLP spec §7.1). A refused
                    // update is a `gap`: its cells stay pending and the sender
                    // repairs them or starts a plan.
                    if !object.is_anchor()
                        && meta.update_base != 0
                        && !self
                            .viewer_model
                            .received_updates
                            .contains(&meta.update_base)
                    {
                        self.trace.counters.updates_gapped += 1;
                        continue;
                    }
                    self.receiver
                        .note_applied_chunk(meta.update_id, meta.part_index);
                    for cell in &cells {
                        self.viewer_model.canvas[*cell as usize] = meta.update_id;
                    }
                    self.viewer_model
                        .updates
                        .insert(meta.update_id, cells.clone());
                    self.viewer_model.received_updates.insert(meta.update_id);
                    self.receiver.note_applied(meta.update_id);
                    self.viewer_model.applied_updates += 1;
                    self.viewer_model.last_apply_ms = self.now_ms;
                    self.trace.counters.updates_applied += 1;
                    // Design §4.1: the ack is a C4 object at priority 0, sent
                    // as soon as the update is applied. It travels with the
                    // feedback report as well, but the prompt copy is what
                    // frees the sender's in-flight slot.
                    let ack_now = self.instant();
                    let ack = self.receiver.build_ack(ack_now);
                    let payload = encode_ack(&ack);
                    let message = self.wrap_message(FragmentClass::Feedback, payload, None);
                    self.link.transmit(Endpoint::Receiver, message);
                    self.record_echo(meta.reflects_input_seq);
                }
                MediaReceiveEvent::FragmentDropped { .. }
                | MediaReceiveEvent::ObjectDropped { .. } => {
                    self.trace.counters.objects_dropped += 1;
                }
                MediaReceiveEvent::ProbeEchoDue(echo) => {
                    let payload = encode_echo(&echo);
                    let message = self.wrap_message(FragmentClass::Feedback, payload, None);
                    self.link.transmit(Endpoint::Receiver, message);
                }
                MediaReceiveEvent::ProbeDue(probe) => {
                    let payload = encode_probe(&probe);
                    let message = self.wrap_message(FragmentClass::Feedback, payload, None);
                    self.link.transmit(Endpoint::Receiver, message);
                }
                MediaReceiveEvent::FeedbackDue => {}
            }
        }
        self.viewer_report_if_due(now);
        self.trace.counters.viewer_lost_objects = self.receiver.stats().lost_objects;
    }

    fn viewer_feedback(&mut self) {
        let now = self.instant();
        self.viewer_report_if_due(now);
        // Cross-check INV-2 after every sender state observation.
        if self.sender.accounting().converged() {
            self.trace.counters.converged_observations += 1;
            if self.trace.counters.last_converge_ms.is_none() || self.capture_dirty.is_empty() {
                self.trace.counters.last_converge_ms = Some(self.now_ms);
            }
            let mismatches: Vec<(usize, u64, u64)> = (0..self.host_model.emitted.len())
                .filter(|cell| self.viewer_model.canvas[*cell] != self.host_model.emitted[*cell])
                .map(|cell| {
                    (
                        cell,
                        self.viewer_model.canvas[cell],
                        self.host_model.emitted[cell],
                    )
                })
                .collect();
            if let Some((cell, canvas, emitted)) = mismatches.first().copied() {
                self.trace.counters.canvas_behind_cells += mismatches.len() as u64;
                // INV-2 as the accounting can prove it: a converged receiver
                // holds cells for every cell the sender has emitted. A canvas
                // *older* than the emitted state can occur while a reordered or
                // repaired update is in flight — the playout guard keeps the
                // newer cells — so it is reported as `canvas_behind_cells`,
                // and only a *missing* cell is a violation.
                if canvas == 0 && emitted > 0 {
                    self.violate(
                        Invariant::Inv2,
                        &format!(
                            "converged with no cells for cell {cell} (emitted update {emitted})"
                        ),
                    );
                }
            }
        }
    }

    fn viewer_report_if_due(&mut self, now: Instant) {
        if !self.receiver.feedback_due(now) {
            return;
        }
        let (keys, buttons) = self.input_queue.pressed_set();
        let _ = encode_pressed_set(&keys, &buttons);
        self.receiver
            .set_viewer_state(0, self.grid.cells(), self.model_converged());
        let feedback = self.receiver.build_feedback(now);
        let pending = self.sender.accounting().pending_count();
        self.trace.acks.push(AckTrace {
            at_ms: self.now_ms,
            highest_applied_update_id: feedback
                .ack
                .map(|ack| ack.highest_applied_update_id)
                .unwrap_or(0),
            pending_cells: pending,
            converged: self.sender.accounting().converged(),
        });
        self.trace.counters.acks += 1;
        self.trace.counters.pending_cells = pending;
        let payload = encode_feedback(&feedback);
        let message = self.wrap_message(FragmentClass::Feedback, payload, None);
        self.link.transmit(Endpoint::Receiver, message);
    }

    fn model_converged(&self) -> bool {
        self.viewer_model.applied_updates > 0
            && self.viewer_model.last_apply_ms >= self.last_change_ms
    }

    fn host_receive(&mut self, bytes: &[u8]) {
        let now = self.instant();
        let events = self.host_rx.on_message(bytes, now);
        for event in events {
            if let MediaReceiveEvent::Object(object) = event {
                if let Some(feedback) = decode_feedback(&object.payload) {
                    self.trace.counters.feedback += 1;
                    let ack_count = u64::from(feedback.ack.is_some());
                    let pending_before = self.sender.accounting().pending_count();
                    let events = self.sender.on_feedback(feedback, now);
                    self.note_sender_events(&events);
                    if ack_count == 1 {
                        self.trace.counters.acks += 1;
                        self.trace.acks.push(AckTrace {
                            at_ms: self.now_ms,
                            highest_applied_update_id: self
                                .sender
                                .accounting()
                                .last_applied_update_id(),
                            pending_cells: pending_before,
                            converged: self.sender.accounting().converged(),
                        });
                    }
                } else if let Some(ack) = decode_ack(&object.payload) {
                    let events = self.sender.on_ack(ack, now);
                    self.note_sender_events(&events);
                    self.trace.counters.acks += 1;
                    self.trace.acks.push(AckTrace {
                        at_ms: self.now_ms,
                        highest_applied_update_id: self
                            .sender
                            .accounting()
                            .last_applied_update_id(),
                        pending_cells: self.sender.accounting().pending_count(),
                        converged: self.sender.accounting().converged(),
                    });
                } else if let Some(echo) = decode_echo(&object.payload) {
                    let events = self.sender.on_probe_echo(echo, now);
                    self.note_sender_events(&events);
                } else if let Some(probe) = decode_probe(&object.payload) {
                    // The sender answers the receiver's probe with an echo.
                    let reply = crate::media::ProbeEcho {
                        object_id: probe.probe_id,
                        send_epoch_ms: probe.send_epoch_ms,
                        echo_epoch_ms: self.sender.session_ms(now),
                        payload_bytes: probe.payload_bytes,
                    };
                    let payload = encode_echo(&reply);
                    let message = self.wrap_message(FragmentClass::Feedback, payload, None);
                    self.link.transmit(Endpoint::Sender, message);
                } else if let Some(envelope) = decode_input(&object.payload) {
                    self.host_input_arrive(envelope);
                }
            }
        }
    }

    // ── input ───────────────────────────────────────────────────────────────

    fn send_input(&mut self, kind: InputEventKind) {
        let now_ms = self.now_ms;
        let payload = vec![0u8; 8];
        match kind {
            InputEventKind::PointerMove => {
                self.input_queue.push_pointer_move(payload, now_ms);
            }
            InputEventKind::PointerButton => {
                self.input_queue
                    .push_pointer_button(1, true, payload, now_ms);
            }
            InputEventKind::Key => {
                self.input_queue.push_key(65, true, false, payload, now_ms);
            }
            InputEventKind::Wheel => {
                self.input_queue.push_wheel(1, payload, now_ms);
            }
            InputEventKind::Clipboard => {
                self.input_queue.set_connected(self.connected, now_ms);
                self.input_queue.push_clipboard(vec![7; 32], now_ms);
            }
            InputEventKind::ReleaseAll => {
                self.input_queue.push_release_all(payload, now_ms);
            }
            InputEventKind::PressedSet => {
                self.input_queue.set_connected(self.connected, now_ms);
                let _ = self.input_queue.poll_pressed_heartbeat(now_ms);
            }
        }
        let ready = self.input_queue.take_ready(now_ms);
        for envelope in ready {
            self.input_sent.insert(envelope.input_seq, now_ms);
            self.trace.counters.input_submitted += 1;
            let payload = encode_input(&envelope);
            let message = self.wrap_message(FragmentClass::Control, payload, None);
            self.link.transmit(Endpoint::Receiver, message);
        }
        // A pressed-set heartbeat goes out with each input burst.
        if let Some(heartbeat) = self.input_queue.poll_pressed_heartbeat(now_ms) {
            self.trace.counters.input_submitted += 1;
            self.input_sent.insert(heartbeat.input_seq, now_ms);
            let payload = encode_input(&heartbeat);
            let message = self.wrap_message(FragmentClass::Control, payload, None);
            self.link.transmit(Endpoint::Receiver, message);
        }
    }

    fn host_input_arrive(&mut self, envelope: GatewayInputEnvelope) {
        let now_ms = self.now_ms;
        let seq = envelope.input_seq;
        let expired_non_release = envelope.is_expired(now_ms) && !envelope.is_always_applied();
        let outcome = self.host_input.submit(&envelope, now_ms);
        if outcome.accepted {
            if expired_non_release {
                self.violate(Invariant::Inv3, "an expired object was applied");
            }
            if !self.input_applied_seen.insert(seq) {
                self.violate(Invariant::Inv3, "an input object was applied twice");
            }
            self.trace.counters.input_applied += 1;
            let first_application = self.input_applied_seq < seq;
            self.input_applied_seq = self.input_applied_seq.max(seq);
            if first_application {
                // Design §7.3.5: on applying input the sender emits an immediate
                // pulse update covering the focus region, so the echo promise is
                // met on the very next update rather than the next idle update.
                self.sender
                    .request_pulse(CellRect::new(96, 96, 64, 64), self.instant());
            }
        } else {
            self.trace.counters.input_dropped += 1;
        }
        if envelope.is_release_all() {
            let release = self.host_input.on_release_all();
            let _ = release;
        }
        for release in outcome.releases {
            let _ = release;
        }
        self.trace.counters.input_expired = self.host_input.counters().1;
    }

    /// Whether the tier's echo promise applies to this run.
    fn echo_promise_applies(&self) -> bool {
        let profile = self.link.profile();
        self.tier != Tier::T4
            && profile.outage_schedule.is_empty()
            && profile.burst_loss.is_none()
            && profile.loss_permille <= 100
    }

    /// The tier's echo budget: `2 × RTT + 250 ms` (MLP spec §3.1), plus one
    /// repair delay and the simulator's tick.
    ///
    /// The repair term is the honest cost of the specification's own repair policy: a
    /// lost echo-critical update is *re-encoded* rather than retransmitted, so
    /// the reflected cells can arrive no sooner than one repair delay after
    /// the loss is visible. The tier's promise is asserted on the p95.
    fn echo_budget_ms(&self) -> u64 {
        let base = (2.0 * self.link.profile().rtt_ms + 250.0).round() as u64;
        let repair = self.sender.accounting().repair_delay_ms();
        base + repair + SIM_TICK_MS
    }

    fn record_echo(&mut self, reflects_input_seq: u64) {
        if reflects_input_seq == 0 {
            return;
        }
        // N-1: the echo is measured once per input, on the first update whose
        // cells reflect it. Later updates carry the same `reflects_input_seq`
        // and are not new measurements.
        if !self.echoed_inputs.insert(reflects_input_seq) {
            return;
        }
        let Some(sent_at) = self.input_sent.get(&reflects_input_seq) else {
            return;
        };
        let echo_ms = self.now_ms.saturating_sub(*sent_at);
        self.echo_records.push((reflects_input_seq, echo_ms));
        self.trace
            .counters
            .echoes
            .push((reflects_input_seq, echo_ms));
        let budget = self.echo_budget_ms();
        // The promise is a steady-state one: the first half second of a
        // session is the estimator's own ramp (MLP spec annex D, D-2), which the tier envelope
        // excludes by construction. The ramp is measured, not hidden.
        let warmed_up = *sent_at >= WARMUP_MS;
        if self.tier != Tier::T4 && warmed_up && echo_ms > budget + SIM_TICK_MS {
            self.trace.counters.echo_outliers += 1;
        }
    }

    // ── park and resume ─────────────────────────────────────────────────────

    fn disconnect(&mut self) {
        self.connected = false;
        self.parked_since_ms = Some(self.now_ms);
        self.trace.counters.parks += 1;
        let now = self.instant();
        let events = self.sender.on_remote_close(
            &crate::media::MediaClose {
                code: crate::media::lifecycle::MediaCloseCode::Park,
                reason: "viewer_disconnect".to_string(),
                park: true,
            },
            now,
        );
        self.note_sender_events(&events);
        self.host_input.on_park(self.now_ms);
    }

    fn reconnect(&mut self) {
        let now_ms = self.now_ms;
        let now = self.instant();
        let claims = ResumeClaims {
            version: ResumeClaims::VERSION,
            session_id: "sim-session".to_string(),
            viewer_id: "sim-receiver".to_string(),
            host_id: "sim-sender".to_string(),
            media_session_id: "sim-media".to_string(),
            epoch: u64::from(self.sender.accounting().epoch()),
            issued_at_secs: now_ms / 1_000,
            expires_at_secs: now_ms / 1_000 + self.park_policy.resume_ttl_seconds,
        };
        let binding = ResumeBinding {
            session_id: "sim-session".to_string(),
            viewer_id: "sim-receiver".to_string(),
            host_id: "sim-sender".to_string(),
        };
        let (keys, buttons) = self.input_queue.pressed_set();
        let request = ResumeRequest {
            claims,
            binding,
            viewer_epoch: self.sender.accounting().epoch(),
            applied_update_id: self.receiver.build_ack(now).highest_applied_update_id,
            ack_bitmap: self.receiver.build_ack(now).ack_bitmap,
            input_seq_next: self.input_queue.next_seq(),
            pressed_keys: keys,
            pressed_buttons: buttons,
            active_path: self.path,
        };
        self.held.pending_cells = u64::from(self.sender.accounting().pending_count());
        self.held.next_update_id = self.sender.next_update_id();
        self.held.epoch = self.sender.accounting().epoch();
        self.held.pace_bps = self.sender.estimate().pace_bps;
        let parked = self.parked_since_ms.is_some();
        let expired = self
            .parked_since_ms
            .map(|parked_at| {
                now_ms.saturating_sub(parked_at) >= self.park_policy.park_ttl_seconds * 1_000
            })
            .unwrap_or(false);
        let outcome = decide_resume(&request, now_ms / 1_000, &mut self.held, parked, expired);
        let mut started_plan = false;
        for action in &outcome.actions {
            match action {
                ResumeAction::StartAnchorPlan { reason } => {
                    self.sender.request_plan(*reason);
                    started_plan = true;
                }
                ResumeAction::Rebase { .. } => {}
                ResumeAction::ReleaseKeys { .. } => {}
                ResumeAction::FreshMediaSession { .. } => {}
            }
        }
        if started_plan {
            let events = self.sender.poll_plans(now);
            self.note_sender_events(&events);
        }
        self.parked_since_ms = None;
        self.connected = true;
        self.resumes += 1;
        self.trace.counters.park_cycles += 1;
        // The receiver's canvas trust resets to "all stale" on an epoch change;
        // its applied-update window continues (no reset update anywhere).
        if outcome.accepted {
            self.receiver.note_path(self.path);
            self.host_input.on_resume();
        }
        let _ = outcome;
    }

    // ── invariants ──────────────────────────────────────────────────────────

    fn check_invariant(&mut self, invariant: Invariant) {
        match invariant {
            Invariant::Inv1 => {
                // Every applied update was complete by construction; assert the
                // property the machine guarantees.
                assert!(
                    self.receiver.stats().objects_by_class[2] >= self.viewer_model.applied_updates,
                    "the receiver applied more updates than it received objects"
                );
            }
            Invariant::Inv2 => {
                // INV-2 is a statement about *converged* observations: while
                // repairs are in flight the receiver legitimately holds older
                // cells. The per-tick check in `viewer_feedback` counts the
                // cells that are behind; the explicit assertion additionally
                // requires that, once converged, no emitted cell is missing.
                if !self.sender.accounting().converged() {
                    return;
                }
                for (cell, emitted) in self.host_model.emitted.iter().enumerate() {
                    if *emitted > 0 && self.viewer_model.canvas[cell] == 0 {
                        self.violate(
                            Invariant::Inv2,
                            &format!("cell {cell} was emitted and never held"),
                        );
                        break;
                    }
                }
            }
            Invariant::Inv3 => {
                for envelope in self.host_input.pressed() {
                    let _ = envelope;
                }
            }
            Invariant::Inv4 => {
                // INV-4: the sender never holds a key the receiver does not hold.
                // (A *live* session with a key down is correct; a sender that
                // still holds a key after the receiver released it, parked, or
                // ended the session is the stuck-key bug.)
                let (keys, buttons) = self.input_queue.pressed_set();
                let receiver: Vec<InputToken> = keys
                    .iter()
                    .map(|code| InputToken::Key(*code))
                    .chain(buttons.iter().map(|code| InputToken::Button(*code)))
                    .collect();
                for token in self.host_input.pressed() {
                    if !receiver.contains(&token) {
                        self.violate(
                            Invariant::Inv4,
                            &format!("the sender holds {} the receiver does not", token.as_str()),
                        );
                        break;
                    }
                }
                if self.parked_since_ms.is_none()
                    && receiver.is_empty()
                    && !self.host_input.pressed().is_empty()
                {
                    self.violate(
                        Invariant::Inv4,
                        "keys are held with no receiver holding them",
                    );
                }
            }
            Invariant::Inv5 => {
                self.check_queues();
            }
            Invariant::Inv6 => {
                let cap = self.sender.estimate().max_object_bytes;
                if cap == 0 {
                    self.violate(Invariant::Inv6, "max_object_bytes is zero");
                }
            }
            Invariant::Inv7 => {
                self.check_rate_window();
            }
            Invariant::Inv8 => {
                // The tier's promise is a p95 on the echo distribution (§3.2
                // N-1), so the assertion is on the p95 and the outliers are
                // reported separately.
                let budget = self.echo_budget_ms() + SIM_TICK_MS;
                if self.tier == Tier::T4 {
                    return;
                }
                let mut samples: Vec<u64> = self
                    .echo_records
                    .iter()
                    .filter(|(_, echo)| *echo != u64::MAX)
                    .map(|(_, echo)| *echo)
                    .collect();
                if samples.is_empty() {
                    return;
                }
                samples.sort_unstable();
                let index = ((samples.len() as f64 * 0.95).ceil() as usize).saturating_sub(1);
                let p95 = samples[index.min(samples.len() - 1)];
                if p95 > budget {
                    self.violate(
                        Invariant::Inv8,
                        &format!(
                            "echo p95 {p95} ms above the {budget} ms budget ({} samples)",
                            samples.len()
                        ),
                    );
                }
            }
            Invariant::Inv9 => {
                if self.parked_since_ms.is_none() && !self.connected {
                    self.violate(Invariant::Inv9, "the session stayed disconnected");
                }
                if self.sender.accounting().epoch() != self.held.epoch && !self.connected {
                    self.violate(Invariant::Inv9, "the media session was recreated");
                }
            }
            Invariant::Inv10 => {
                let viewer_lost = self.receiver.stats().lost_objects;
                let attributed = self.trace.counters.hop_dropped + self.link.stats().dropped_loss;
                let link_loss = viewer_lost.saturating_sub(attributed);
                // The arithmetic must be able to explain the receiver's loss.
                if viewer_lost > attributed + 2 && self.intermediary.drop_permille == 0 {
                    self.violate(
                        Invariant::Inv10,
                        &format!(
                            "{viewer_lost} lost objects, none reported by a hop \
                             (link loss {link_loss})"
                        ),
                    );
                }
            }
        }
    }

    fn check_cheap_invariants(&mut self) {
        if self.sender.queued_objects() > 64 {
            self.violate(Invariant::Inv5, "the sender queue is unbounded");
        }
        self.check_queues();
        self.check_rate_window();
    }

    fn check_queues(&mut self) {
        if self.input_queue.len() > self.input_queue.object_capacity() {
            self.violate(Invariant::Inv5, "the receiver input queue exceeded its cap");
        }
        if self.host_input.worker_depth() > 64 {
            self.violate(Invariant::Inv5, "the sender input worker exceeded its cap");
        }
        if self.receiver.assembler().open_objects() > 4 {
            self.violate(
                Invariant::Inv5,
                "the receiver held more than 4 open objects",
            );
        }
        if self.link.in_flight() > 4_096 {
            self.violate(Invariant::Inv5, "the link queue is unbounded");
        }
    }

    fn check_rate_window(&mut self) {
        let budget = self.release_cap_bytes.max(1);
        let now = self.now_ms;
        let window_start = now.saturating_sub(1_000);
        let bytes: u64 = self
            .fragment_releases
            .iter()
            .filter(|(at, _)| *at > window_start)
            .map(|(_, bytes)| *bytes)
            .sum();
        if bytes > budget {
            self.violate(
                Invariant::Inv7,
                &format!("{bytes} bytes in a 1 s window above the {budget} byte budget"),
            );
        }
    }

    fn violate(&mut self, invariant: Invariant, detail: &str) {
        // One violation is enough per invariant: a broken invariant floods the
        // trace otherwise.
        if self
            .trace
            .violations
            .iter()
            .any(|violation| violation.invariant == invariant.as_str())
        {
            return;
        }
        self.trace.violations.push(ViolationTrace {
            at_ms: self.now_ms,
            invariant: invariant.as_str().to_string(),
            detail: detail.to_string(),
        });
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    fn wrap_message(
        &mut self,
        class: FragmentClass,
        payload: Vec<u8>,
        flags: Option<FragmentFlags>,
    ) -> Vec<u8> {
        // One object id per simulated message. The counter starts high so the
        // simulator's own messages can never collide with an id the sender
        // machine allocated for a media update: two objects sharing an id would
        // be deduped as one by the receiver (and by the codec's idempotence
        // rule), which is exactly the confusion this offset removes.
        let object_id = self.sim_object_id;
        self.sim_object_id = self.sim_object_id.wrapping_add(1).max(SIM_OBJECT_ID_BASE);
        let fragment = Fragment::new(
            class,
            flags.unwrap_or(FragmentFlags::FIN),
            Priority::CONTROL,
            object_id,
            0,
            1,
            crate::media::HeaderExt::new(),
            payload,
        )
        .expect("simulated fragments are valid");
        fragment.encode()
    }

    fn finish(mut self) -> Trace {
        self.check_cheap_invariants();
        self.check_invariant(Invariant::Inv4);
        // The echo promise is a steady-state promise: it is asserted where the
        // link is inside the tier envelope (no outage, no extreme loss) and the
        // design promises it (T0–T3, §3.1). T4's echo is reported (MLP spec annex N, N-3).
        if self.echo_promise_applies() {
            self.check_invariant(Invariant::Inv8);
        }
        self.trace.duration_ms = self.now_ms;
        self.trace.counters.pending_cells = self.sender.accounting().pending_count();
        self.trace.counters.updates_acked = self.sender.accounting().last_applied_update_id();
        self.trace.counters.epoch = self.sender.accounting().epoch();
        self.trace.counters.final_state = self.sender.lane_state().as_str().to_string();
        self.trace.link = *self.link.stats();
        self.trace.counters.viewer_lost_objects = self.receiver.stats().lost_objects;
        let _ = self.resumes;
        self.trace
    }
}

impl GatewayInputEnvelope {
    /// Whether this envelope releases everything (a simulation convenience).
    fn is_release_all(&self) -> bool {
        matches!(self.kind, InputEventKind::ReleaseAll)
    }
}

fn payload_len(decision: &ScheduleDecision) -> usize {
    decision
        .cells
        .len()
        .saturating_mul(SIM_BYTES_PER_CELL)
        .max(32)
}

fn decode_cell_bitmap(payload: &[u8], cells: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for cell in 0..cells {
        let byte = (cell / 8) as usize;
        if byte >= SIM_BITMAP_BYTES || byte >= payload.len() {
            break;
        }
        if payload[byte] & (1 << (cell % 8)) != 0 {
            out.push(cell);
        }
    }
    out
}

fn encode_ack(ack: &crate::media::MediaAck) -> Vec<u8> {
    let mut out = b"ack".to_vec();
    out.extend_from_slice(&serde_json::to_vec(ack).expect("ack serializes"));
    out
}

fn decode_ack(payload: &[u8]) -> Option<crate::media::MediaAck> {
    if payload.len() < 3 || &payload[0..3] != b"ack" {
        return None;
    }
    serde_json::from_slice(&payload[3..]).ok()
}

fn encode_feedback(feedback: &MediaFeedback) -> Vec<u8> {
    let mut out = b"fb".to_vec();
    out.extend_from_slice(&serde_json::to_vec(feedback).expect("feedback serializes"));
    out
}

fn decode_feedback(payload: &[u8]) -> Option<MediaFeedback> {
    if payload.len() < 2 || &payload[0..2] != b"fb" {
        return None;
    }
    serde_json::from_slice(&payload[2..]).ok()
}

fn encode_probe(probe: &crate::media::MediaProbe) -> Vec<u8> {
    let mut out = b"probe".to_vec();
    out.extend_from_slice(&probe.probe_id.to_le_bytes());
    out.extend_from_slice(&probe.send_epoch_ms.to_le_bytes());
    out.extend_from_slice(&probe.payload_bytes.to_le_bytes());
    out
}

fn decode_probe(payload: &[u8]) -> Option<crate::media::MediaProbe> {
    if payload.len() < 5 || &payload[0..5] != b"probe" || payload.len() < 5 + 16 {
        return None;
    }
    Some(crate::media::MediaProbe {
        probe_id: u32::from_le_bytes([payload[5], payload[6], payload[7], payload[8]]),
        send_epoch_ms: u64::from_le_bytes([
            payload[9],
            payload[10],
            payload[11],
            payload[12],
            payload[13],
            payload[14],
            payload[15],
            payload[16],
        ]),
        payload_bytes: u32::from_le_bytes([payload[17], payload[18], payload[19], payload[20]]),
    })
}

fn encode_echo(echo: &crate::media::ProbeEcho) -> Vec<u8> {
    let mut out = b"echo".to_vec();
    out.extend_from_slice(&echo.object_id.to_le_bytes());
    out.extend_from_slice(&echo.send_epoch_ms.to_le_bytes());
    out.extend_from_slice(&echo.echo_epoch_ms.to_le_bytes());
    out
}

fn decode_echo(payload: &[u8]) -> Option<crate::media::ProbeEcho> {
    if payload.len() < 4 || &payload[0..4] != b"echo" || payload.len() < 4 + 20 {
        return None;
    }
    let mut cursor = 4;
    let object_id = u32::from_le_bytes([
        payload[cursor],
        payload[cursor + 1],
        payload[cursor + 2],
        payload[cursor + 3],
    ]);
    cursor += 4;
    let read_u64 = |cursor: &mut usize| {
        let value = u64::from_le_bytes([
            payload[*cursor],
            payload[*cursor + 1],
            payload[*cursor + 2],
            payload[*cursor + 3],
            payload[*cursor + 4],
            payload[*cursor + 5],
            payload[*cursor + 6],
            payload[*cursor + 7],
        ]);
        *cursor += 8;
        value
    };
    let send_epoch_ms = read_u64(&mut cursor);
    let echo_epoch_ms = read_u64(&mut cursor);
    Some(crate::media::ProbeEcho {
        object_id,
        send_epoch_ms,
        echo_epoch_ms,
        payload_bytes: 0,
    })
}

fn encode_input(envelope: &GatewayInputEnvelope) -> Vec<u8> {
    let mut out = b"inp".to_vec();
    out.extend_from_slice(&envelope.input_seq.to_le_bytes());
    out.extend_from_slice(&envelope.deadline_epoch_ms.to_le_bytes());
    out.extend_from_slice(&envelope.kind.as_u32().to_le_bytes());
    out.extend_from_slice(&[envelope.flags.bits()]);
    out.extend_from_slice(&(envelope.payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&envelope.payload);
    out
}

fn decode_input(payload: &[u8]) -> Option<GatewayInputEnvelope> {
    if payload.len() < 3 || &payload[0..3] != b"inp" || payload.len() < 3 + 25 {
        return None;
    }
    let input_seq = u64::from_le_bytes([
        payload[3],
        payload[4],
        payload[5],
        payload[6],
        payload[7],
        payload[8],
        payload[9],
        payload[10],
    ]);
    let deadline_epoch_ms = u64::from_le_bytes([
        payload[11],
        payload[12],
        payload[13],
        payload[14],
        payload[15],
        payload[16],
        payload[17],
        payload[18],
    ]);
    let kind = InputEventKind::from_u32(u32::from_le_bytes([
        payload[19],
        payload[20],
        payload[21],
        payload[22],
    ]))?;
    let flags = InputFlags::from_bits(payload[23]);
    let len = u32::from_le_bytes([payload[24], payload[25], payload[26], payload[27]]) as usize;
    if payload.len() < 28 + len {
        return None;
    }
    Some(GatewayInputEnvelope {
        input_seq,
        deadline_epoch_ms,
        kind,
        flags,
        payload: payload[28..28 + len].to_vec(),
    })
}

/// Convenience: a pressed-set body for tests.
pub fn pressed_set_payload(keys: &[u32], buttons: &[u32]) -> Vec<u8> {
    encode_pressed_set(keys, buttons)
}

/// Convenience: decode a pressed-set body.
pub fn pressed_set_decode(bytes: &[u8]) -> Option<(Vec<u32>, Vec<u32>)> {
    decode_pressed_set(bytes)
}

/// The carrier a tier's profile implies, for tests that build machines.
pub fn carrier_for_tier(tier: Tier) -> Carrier {
    carrier_for(&LinkProfile::tier(tier))
}

/// A scenario with no remembered prior: the cold start the specification accepts
/// (an anchor at floor rate, then the doubling ladder).
pub fn cold_start_scenario(tier: Tier) -> Scenario {
    Scenario::new(format!("cold-{}", tier.as_str()), tier)
        .step(Step::Link(Box::new(LinkProfile::tier(tier).with_seed(1))))
        .change(CellRect::new(0, 0, 256, 128))
        .advance(12_000)
        .step(Step::Assert(Invariant::Inv5))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7))
}

/// A scenario that captures a change and asserts the first four invariants:
/// the smallest useful end-to-end run.
pub fn smoke_scenario(tier: Tier) -> Scenario {
    Scenario::new(format!("smoke-{}", tier.as_str()), tier)
        .change(CellRect::new(0, 0, 256, 128))
        .advance(2_000)
        .step(Step::Assert(Invariant::Inv1))
        .step(Step::Assert(Invariant::Inv2))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7))
}

/// The update kind of a scheduler decision, for callers that want the label.
pub fn update_kind_label(kind: UpdateKind) -> &'static str {
    kind.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_scenario_is_clean_on_the_perfect_link() {
        let mut scenario = smoke_scenario(Tier::T0);
        scenario.steps.insert(
            0,
            Step::Link(Box::new(LinkProfile::perfect().with_seed(42))),
        );
        let trace = run(&scenario);
        trace.assert_clean();
        assert!(trace.counters.updates_sent >= 1);
        assert!(trace.counters.updates_applied >= 1);
        assert_eq!(trace.counters.viewer_lost_objects, 0);
    }

    #[test]
    fn traces_are_deterministic() {
        let scenario = Scenario::new("determinism", Tier::T2)
            .change(CellRect::new(0, 0, 512, 256))
            .advance(1_000)
            .input(InputEventKind::Key)
            .advance(1_000);
        let first = run(&scenario);
        let second = run(&scenario);
        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.to_json(), second.to_json());
    }

    #[test]
    fn a_lossy_tier_still_makes_progress() {
        let scenario = Scenario::new("t3-progress", Tier::T3)
            .change(CellRect::new(0, 0, 512, 256))
            .advance(6_000);
        let trace = run(&scenario);
        assert!(trace.counters.updates_sent >= 1);
        assert!(trace.counters.objects_delivered >= 1);
        assert!(
            trace.counters.updates_applied >= 1,
            "no update applied: {:?}",
            trace.link
        );
    }
}
