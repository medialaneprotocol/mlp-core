//! Priority scheduling, update timing and anchor plans (MLP spec §7.3, §6.6, D-19).
//!
//! The scheduler answers one question per capture cycle: *which cells should
//! the next update for this receiver cover, under the current budget?* It owns
//! the priority bands P0–P3, the input pulse window, the idle-update cadence
//! and the anchor-plan machinery.

use serde::{Deserialize, Serialize};

use super::cells::{BlockGrid, CellRect, DueBlock, DueReason};

/// The design's priority bands (MLP spec §7.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PriorityBand {
    /// P0 — input/focus region and cells pending beyond the ladder (≥50 %).
    P0,
    /// P1 — pending cells (sent, unacked, due).
    P1,
    /// P2 — fresh changes (`E ≠ capture`).
    P2,
    /// P3 — background refresh.
    P3,
}

impl PriorityBand {
    /// The MLP priority field this band maps to.
    pub const fn mlp_priority(self) -> u8 {
        match self {
            Self::P0 | Self::P1 => 6,
            Self::P2 | Self::P3 => 7,
        }
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::P0 => "p0",
            Self::P1 => "p1",
            Self::P2 => "p2",
            Self::P3 => "p3",
        }
    }
}

/// The kind of update a decision produces (`mlp_frames_total{kind}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateKind {
    /// A chained delta update.
    Delta,
    /// A update whose regions may apply onto any held state.
    Anchor,
    /// An empty update that keeps the ack timeline alive.
    Idle,
    /// An immediate update carrying the input pulse region.
    Pulse,
    /// A update sent to repair cells (chained unless an anchor plan).
    Repair,
}

impl UpdateKind {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delta => "delta",
            Self::Anchor => "anchor",
            Self::Idle => "idle",
            Self::Pulse => "pulse",
            Self::Repair => "repair",
        }
    }
}

/// The per-update budget the estimator produced (MLP spec §7.2.5, D-19).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UpdateBudget {
    /// `budget_bytes`: what this update may spend.
    pub bytes: u32,
    /// `max_object_bytes`: the largest single object.
    pub max_object_bytes: u32,
    /// `max_chunks_per_frame` for the carrier and current loss.
    pub max_chunks: u16,
    /// The update interval implied by `target_fps`.
    pub update_interval_ms: u32,
    /// The codec's chunk wire ceiling (491 520 B) — a ceiling, not a budget.
    pub max_chunk_wire_bytes: u32,
    /// The carrier's per-chunk cap.
    pub path_chunk_cap: u32,
}

impl Default for UpdateBudget {
    fn default() -> Self {
        Self {
            bytes: 4_096,
            max_object_bytes: 8_192,
            max_chunks: 8,
            update_interval_ms: 33,
            max_chunk_wire_bytes: 491_520,
            path_chunk_cap: 65_536,
        }
    }
}

impl UpdateBudget {
    /// How many chunks a payload of `payload_bytes` needs (MLP spec §7.3.4).
    ///
    /// `None` when the update cannot be carried at all: the caller must
    /// decompose it into more updates rather than emit an oversized object.
    pub fn part_count(&self, payload_bytes: u32) -> Option<u16> {
        let per_chunk = self
            .max_chunk_wire_bytes
            .min(self.bytes)
            .min(self.path_chunk_cap)
            .max(1);
        let count = payload_bytes.div_ceil(per_chunk);
        if count > u32::from(self.max_chunks) {
            return None;
        }
        Some(count.max(1) as u16)
    }

    /// The largest payload this update may carry.
    pub fn payload_ceiling(&self) -> u32 {
        self.max_chunk_wire_bytes
            .min(self.bytes)
            .min(self.path_chunk_cap)
            .saturating_mul(u32::from(self.max_chunks.max(1)))
    }
}

/// What the scheduler is asked to consider for one receiver.
#[derive(Debug, Clone, Copy)]
pub struct ScheduleRequest<'a> {
    /// Cells due from cell accounting (P0/P1).
    pub due: &'a [DueBlock],
    /// Cells whose cells changed since the emitted state (P2).
    pub fresh: &'a [u32],
    /// Cells never sent this epoch (P3).
    pub refresh: &'a [u32],
    /// The budget for this update.
    pub budget: UpdateBudget,
    /// Whether the sender knows the receiver cannot chain (gap, resume, epoch).
    pub anchor: bool,
}

/// The scheduler's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDecision {
    /// Cells the update must cover, in emission order.
    pub cells: Vec<u32>,
    /// The highest band in the update (drives the MLP priority field).
    pub band: PriorityBand,
    /// Whether the update is an anchor.
    pub anchor: bool,
    /// The update kind.
    pub kind: UpdateKind,
    /// Estimated payload bytes (an upper bound from the cell cost).
    pub estimated_bytes: u32,
    /// Cells that were due but did not fit.
    pub deferred: u32,
    /// The plan this update belongs to, when a plan is active.
    pub plan_id: Option<u32>,
}

impl ScheduleDecision {
    /// Whether the update would carry no cells and is therefore an idle update.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The bounding rectangle of the update's cells.
    pub fn rect(&self, grid: &BlockGrid) -> CellRect {
        grid.bounding_rect(&self.cells)
    }
}

/// Why an anchor plan started (`mlp_anchor_plans_total{reason}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStartReason {
    /// The pending set crossed `anchor_threshold`.
    Pending,
    /// The receiver's epoch disagreed with the sender's.
    Epoch,
    /// The geometry changed.
    Geometry,
    /// Repair cells stayed stuck after the escalation ladder.
    Stuck,
    /// The receiver resumed onto the parked state.
    Resume,
}

impl PlanStartReason {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Epoch => "epoch",
            Self::Geometry => "geometry",
            Self::Stuck => "stuck",
            Self::Resume => "resume",
        }
    }
}

/// Plan policy constants (MLP spec §7.6).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlanConfig {
    /// `anchor_threshold = max(16, cells / 4)`.
    pub anchor_threshold: u32,
    /// `RESET_MIN_INTERVAL_MS`, per receiver: plans are rate-limited per receiver.
    pub min_interval_ms: u64,
    /// `plan_deadline = 2 × RTT + 30 s` on the T4 profile.
    pub rtt_ms: f64,
    /// The plan deadline floor.
    pub deadline_floor_ms: u64,
    /// Plan priority field.
    pub priority: u8,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            anchor_threshold: 16,
            min_interval_ms: 250,
            rtt_ms: 50.0,
            deadline_floor_ms: 30_000,
            priority: PriorityBand::P0.mlp_priority(),
        }
    }
}

impl PlanConfig {
    /// `plan_deadline = 2 × RTT + 30 s`.
    pub fn deadline_ms(&self) -> u64 {
        (2.0 * self.rtt_ms).round() as u64 + self.deadline_floor_ms
    }
}

#[derive(Debug, Clone)]
struct AnchorPlanState {
    plan_id: u32,
    cells: Vec<u32>,
    next_index: usize,
    updates_emitted: u32,
    started_at_ms: u64,
    reason: PlanStartReason,
}

/// The last band [`PlanManager::next_band`] pulled, remembered until the update
/// that was meant to paint it reports what it actually painted (T3).
///
/// A band is the update that carries it, and a pull alone must never advance the
/// plan: when the update is refused (in-flight cap, rate limit, oversize) or
/// preempted by a pulse, [`PlanManager::unclaim`] returns the unpainted cells
/// to the plan instead of losing them.
#[derive(Debug, Clone)]
struct PulledBand {
    plan_id: u32,
    cells: Vec<u32>,
    updates_emitted: u32,
    started_at_ms: u64,
    reason: PlanStartReason,
    /// The pull drained the plan: without `unclaim` the plan completed here.
    completed_at_pull: bool,
}

/// The anchor-plan machinery: one plan slot per receiver.
#[derive(Debug, Clone)]
pub struct PlanManager {
    config: PlanConfig,
    next_plan_id: u32,
    active: Option<AnchorPlanState>,
    last_started_at_ms: Option<u64>,
    last_pulled: Option<PulledBand>,
    plans_started: u64,
    plans_overdue: u64,
    plans_completed: u64,
}

impl PlanManager {
    /// Create a manager with the given policy.
    pub fn new(config: PlanConfig) -> Self {
        Self {
            config,
            next_plan_id: 1,
            active: None,
            last_started_at_ms: None,
            last_pulled: None,
            plans_started: 0,
            plans_overdue: 0,
            plans_completed: 0,
        }
    }

    /// The policy in force.
    pub fn config(&self) -> &PlanConfig {
        &self.config
    }

    /// Update the RTT that drives the plan deadline.
    pub fn set_rtt_ms(&mut self, rtt_ms: f64) {
        self.config.rtt_ms = rtt_ms.max(1.0);
    }

    /// Update the anchor threshold when the grid changes.
    pub fn set_anchor_threshold(&mut self, threshold: u32) {
        self.config.anchor_threshold = threshold.max(1);
    }

    /// The active plan, when one is running.
    pub fn active(&self) -> Option<(u32, u32, PlanStartReason)> {
        self.active.as_ref().map(|plan| {
            (
                plan.plan_id,
                (plan.cells.len() - plan.next_index) as u32,
                plan.reason,
            )
        })
    }

    /// Whether a plan is running.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Start a plan covering `cells`.
    ///
    /// Returns `None` when the per-receiver rate limit refuses it, when the plan
    /// would be empty, or when an identical plan is already running.
    pub fn start(
        &mut self,
        reason: PlanStartReason,
        mut cells: Vec<u32>,
        now_ms: u64,
    ) -> Option<u32> {
        cells.sort_unstable();
        cells.dedup();
        if cells.is_empty() {
            return None;
        }
        if let Some(last) = self.last_started_at_ms
            && now_ms.saturating_sub(last) < self.config.min_interval_ms
        {
            return None;
        }
        let plan_id = self.next_plan_id;
        self.next_plan_id = self.next_plan_id.wrapping_add(1).max(1);
        self.active = Some(AnchorPlanState {
            plan_id,
            cells,
            next_index: 0,
            updates_emitted: 0,
            started_at_ms: now_ms,
            reason,
        });
        self.last_started_at_ms = Some(now_ms);
        self.plans_started += 1;
        Some(plan_id)
    }

    /// Whether a plan should start (MLP spec §7.6 condition 1).
    pub fn should_start_for_pending(&self, pending: u32, stuck: usize, now_ms: u64) -> bool {
        if self.active.is_some() {
            return false;
        }
        if let Some(last) = self.last_started_at_ms
            && now_ms.saturating_sub(last) < self.config.min_interval_ms
        {
            return false;
        }
        pending > self.config.anchor_threshold || stuck > 0
    }

    /// Take the next band of the active plan.
    ///
    /// Each band is a contiguous run of the plan's cell list, bounded by
    /// `budget_bytes` and `max_object_bytes` (MLP spec §7.6). The pull is
    /// remembered in [`Self::unclaim`]'s terms: the band is consumed only by
    /// the update that paints it.
    pub fn next_band(&mut self, budget_bytes: u32, cell_bytes: u32) -> Option<PlanBand> {
        let plan = self.active.as_mut()?;
        if plan.next_index >= plan.cells.len() {
            return None;
        }
        let cell_bytes = cell_bytes.max(1);
        let max_blocks = (budget_bytes / cell_bytes).max(1) as usize;
        let remaining = plan.cells.len() - plan.next_index;
        let take = remaining.min(max_blocks);
        let cells = plan.cells[plan.next_index..plan.next_index + take].to_vec();
        plan.next_index += take;
        plan.updates_emitted += 1;
        let remaining_after = (plan.cells.len() - plan.next_index) as u32;
        let band = PlanBand {
            plan_id: plan.plan_id,
            cells,
            anchor: true,
            remaining_after,
            priority: self.config.priority,
            updates_emitted: plan.updates_emitted,
        };
        let pulled = PulledBand {
            plan_id: band.plan_id,
            cells: band.cells.clone(),
            updates_emitted: band.updates_emitted,
            started_at_ms: plan.started_at_ms,
            reason: plan.reason,
            completed_at_pull: remaining_after == 0,
        };
        if pulled.completed_at_pull {
            self.plans_completed += 1;
            self.active = None;
        }
        self.last_pulled = Some(pulled);
        Some(band)
    }

    /// Return the cells of the last pulled band that `painted` did not cover
    /// to the front of the plan (T3).
    ///
    /// The pull is a claim on the update to come; this is its settlement. When
    /// the update never happened (refused: in-flight cap, rate limit, oversize)
    /// or painted only part of the band (shrunk to what fit the object cap),
    /// the unpainted cells are owed again — in order, at the head of what the
    /// plan has left. A band whose pull drained the plan reopens it rather than
    /// staying completed, so the plan is only ever completed by the cells it
    /// painted. Returns whether any cell went back to the plan.
    pub fn unclaim(&mut self, painted: &[u32]) -> bool {
        let Some(pulled) = self.last_pulled.take() else {
            return false;
        };
        let owed: Vec<u32> = pulled
            .cells
            .iter()
            .copied()
            .filter(|cell| !painted.contains(cell))
            .collect();
        if owed.is_empty() {
            // The update painted the whole band: the plan keeps its position.
            return false;
        }
        // The plan that pulled the band is still running: roll the consumed
        // segment back. The segment is exactly the pulled band — nothing else
        // mutates a plan between its pull and the update's settlement.
        if let Some(plan) = self.active.as_mut()
            && plan.plan_id == pulled.plan_id
        {
            let consumed = pulled.cells.len();
            if plan.next_index >= consumed
                && plan.cells[plan.next_index - consumed..plan.next_index] == pulled.cells[..]
            {
                let start = plan.next_index - consumed;
                let rest = plan.cells.split_off(plan.next_index);
                plan.cells.truncate(start);
                plan.cells.extend(owed);
                plan.next_index = start;
                plan.cells.extend(rest);
                return true;
            }
            return false;
        }
        // The pull drained the plan: reopen it. The completion was counted at
        // the pull (the manager reports the drain there), so it is un-counted
        // here — the plan is not complete until its cells are painted.
        if pulled.completed_at_pull && self.active.is_none() {
            self.plans_completed -= 1;
            self.active = Some(AnchorPlanState {
                plan_id: pulled.plan_id,
                cells: owed,
                next_index: 0,
                updates_emitted: pulled.updates_emitted,
                started_at_ms: pulled.started_at_ms,
                reason: pulled.reason,
            });
            return true;
        }
        false
    }

    /// Mark the plan complete (all bands emitted).
    pub fn complete(&mut self) -> Option<u32> {
        let plan = self.active.take()?;
        self.plans_completed += 1;
        Some(plan.plan_id)
    }

    /// Cancel the plan, for example on a lane reset.
    pub fn cancel(&mut self) -> Option<u32> {
        self.active.take().map(|plan| plan.plan_id)
    }

    /// Whether the active plan has run past its deadline.
    ///
    /// `Some(plan_id)` means the caller must restart the plan with the newest
    /// cell set: freshness beats completeness on a collapsing link.
    pub fn poll_overdue(&mut self, now_ms: u64) -> Option<u32> {
        let plan = self.active.as_ref()?;
        if now_ms.saturating_sub(plan.started_at_ms) > self.config.deadline_ms() {
            let plan_id = plan.plan_id;
            self.active = None;
            self.plans_overdue += 1;
            // The restart is not rate-limited: an overdue plan is already a
            // signal that the slow path is not working.
            self.last_started_at_ms = None;
            return Some(plan_id);
        }
        None
    }

    /// Plans started, completed and overdue.
    pub fn counters(&self) -> (u64, u64, u64) {
        (self.plans_started, self.plans_completed, self.plans_overdue)
    }
}

/// One update's worth of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBand {
    /// The plan id.
    pub plan_id: u32,
    /// Cells the update must cover.
    pub cells: Vec<u32>,
    /// Always true: every plan update is an anchor.
    pub anchor: bool,
    /// Cells still to send after this band.
    pub remaining_after: u32,
    /// The MLP priority field.
    pub priority: u8,
    /// How many updates the plan has emitted including this one.
    pub updates_emitted: u32,
}

/// Scheduler policy constants.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Estimated *encoded* bytes per cell, used to truncate a update to the
    /// budget before the encoder runs.
    ///
    /// The raw cell size of a cell is `block_size² × 2` (8 KiB at 64 px),
    /// which is the ceiling the encoder must respect; the working estimate is
    /// far below it because a cell is carried as one or two region ops. The
    /// sender pipeline calibrates this from its bench; `0` means "use the raw
    /// ceiling", which is deliberately conservative.
    pub cell_bytes: u32,
    /// `IDLE_FRAME_INTERVAL_MS` floor.
    pub idle_interval_floor_ms: u64,
    /// `IDLE_FRAME_INTERVAL_MS` ceiling.
    pub idle_interval_ceiling_ms: u64,
    /// The input pulse window floor (`max(300 ms, 1 × RTT)`).
    pub pulse_window_floor_ms: u64,
    /// `input_pulse_fps = min(30, max(4, target_fps × 2))` multiplier.
    pub pulse_fps_multiplier: f64,
    /// `max_chunks_per_frame` on a stream carrier.
    pub max_chunks_stream: u16,
    /// `max_chunks_per_frame` on a datagram carrier without heavy loss.
    pub max_chunks_datagram: u16,
    /// Loss ratio above which a datagram update must be a single chunk.
    pub single_chunk_loss_ratio: f64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            cell_bytes: DEFAULT_BLOCK_BYTES,
            idle_interval_floor_ms: 1_000,
            idle_interval_ceiling_ms: 3_000,
            pulse_window_floor_ms: 300,
            pulse_fps_multiplier: 2.0,
            max_chunks_stream: 8,
            max_chunks_datagram: 4,
            single_chunk_loss_ratio: 0.05,
        }
    }
}

impl SchedulerConfig {
    /// Estimated bytes for one cell at the given grid.
    pub fn block_bytes_for(&self, grid: &BlockGrid) -> u32 {
        if self.cell_bytes != 0 {
            return self.cell_bytes;
        }
        // a raw cell is the upper bound of any codec payload.
        let size = grid.cell_size();
        size.saturating_mul(size).saturating_mul(2)
    }
}

/// Smallest gap between updates for a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateTimer {
    last_frame_ms: Option<u64>,
    pulse_until_ms: Option<u64>,
}

impl UpdateTimer {
    /// A timer with no updates emitted yet.
    pub fn new() -> Self {
        Self {
            last_frame_ms: None,
            pulse_until_ms: None,
        }
    }

    /// Record that a update was emitted.
    pub fn on_frame(&mut self, now_ms: u64) {
        self.last_frame_ms = Some(now_ms);
    }

    /// Open an input-pulse window.
    pub fn pulse(&mut self, now_ms: u64, window_ms: u64) {
        let until = now_ms + window_ms;
        self.pulse_until_ms = Some(
            self.pulse_until_ms
                .map_or(until, |current| current.max(until)),
        );
    }

    /// Whether the pulse window is open.
    pub fn pulse_active(&self, now_ms: u64) -> bool {
        self.pulse_until_ms.is_some_and(|until| now_ms < until)
    }

    /// Close an expired pulse window.
    pub fn expire_pulse(&mut self, now_ms: u64) {
        if !self.pulse_active(now_ms) {
            self.pulse_until_ms = None;
        }
    }

    /// The time of the last update.
    pub fn last_frame_ms(&self) -> Option<u64> {
        self.last_frame_ms
    }

    /// Whether a update is due at `target_fps`.
    pub fn due(&self, now_ms: u64, target_fps: f64) -> bool {
        if self.pulse_active(now_ms) {
            return true;
        }
        let interval = (1_000.0 / target_fps.max(0.5)).round() as u64;
        match self.last_frame_ms {
            Some(last) => now_ms.saturating_sub(last) >= interval.max(1),
            None => true,
        }
    }

    /// The idle-update interval for a link: `clamp(1 000, 2 × RTT, 3 000)`.
    pub fn idle_interval_ms(rtt_ms: f64) -> u64 {
        (2.0 * rtt_ms).round().clamp(
            IDLE_FRAME_INTERVAL_FLOOR_MS as f64,
            IDLE_FRAME_INTERVAL_CEILING_MS as f64,
        ) as u64
    }
}

impl Default for UpdateTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// Working estimate of one cell's encoded size, in bytes.
///
/// 512 B/cell keeps a update inside the floor budget (8 kbps ⇒ 1 000 B) so the
/// scheduler always makes progress; the encoder's own bound is the raw cell
/// size and the sender rejects anything above `max_object_bytes` (§5.5.1).
pub const DEFAULT_BLOCK_BYTES: u32 = 512;
/// `IDLE_FRAME_INTERVAL_MS` floor (MLP spec §10).
pub const IDLE_FRAME_INTERVAL_FLOOR_MS: u64 = 1_000;
/// `IDLE_FRAME_INTERVAL_MS` ceiling (MLP spec §10).
pub const IDLE_FRAME_INTERVAL_CEILING_MS: u64 = 3_000;

/// The per-receiver scheduler: priority bands, pulse window and plans.
#[derive(Debug, Clone)]
pub struct Scheduler {
    config: SchedulerConfig,
    plans: PlanManager,
    focus: Option<CellRect>,
    timer: UpdateTimer,
    last_idle_ms: Option<u64>,
    updates_by_kind: [u64; 5],
    deferred_total: u64,
}

impl Scheduler {
    /// Create a scheduler for one receiver.
    pub fn new(config: SchedulerConfig, plans: PlanConfig) -> Self {
        Self {
            config,
            plans: PlanManager::new(plans),
            focus: None,
            timer: UpdateTimer::new(),
            last_idle_ms: None,
            updates_by_kind: [0; 5],
            deferred_total: 0,
        }
    }

    /// The plan manager (mutable, for plan start/completion).
    pub fn plans_mut(&mut self) -> &mut PlanManager {
        &mut self.plans
    }

    /// The plan manager (read-only).
    pub fn plans(&self) -> &PlanManager {
        &self.plans
    }

    /// The update timer.
    pub fn timer(&self) -> &UpdateTimer {
        &self.timer
    }

    /// The update timer, mutable.
    pub fn timer_mut(&mut self) -> &mut UpdateTimer {
        &mut self.timer
    }

    /// Update the RTT used for pulse and idle intervals.
    pub fn set_rtt_ms(&mut self, rtt_ms: f64) {
        self.plans.set_rtt_ms(rtt_ms);
    }

    /// Note the input/focus rectangle (P0).
    pub fn mark_focus(&mut self, rect: CellRect) {
        self.focus = Some(match self.focus {
            Some(current) => current.union(&rect),
            None => rect,
        });
    }

    /// The current focus rectangle.
    pub fn focus(&self) -> Option<CellRect> {
        self.focus
    }

    /// Clear the focus rectangle (after it was included in a update).
    pub fn clear_focus(&mut self) {
        self.focus = None;
    }

    /// The cells of the current focus rectangle.
    pub fn focus_blocks(&self, grid: &BlockGrid) -> Vec<u32> {
        self.focus
            .map(|rect| grid.blocks_in_rect(&rect))
            .unwrap_or_default()
    }

    /// Trigger an immediate input pulse (MLP spec §7.3.5).
    pub fn request_pulse(&mut self, now_ms: u64, rtt_ms: f64) {
        let window = self.config.pulse_window_floor_ms.max(rtt_ms.round() as u64);
        self.timer.pulse(now_ms, window);
    }

    /// The target fps during a pulse: `min(30, max(4, target_fps × 2))`.
    pub fn pulse_fps(&self, target_fps: f64) -> f64 {
        (target_fps * self.config.pulse_fps_multiplier).clamp(4.0, 30.0)
    }

    /// The pulse budget: `min(budget_bytes, max(1 024, pace/8 × 0.25 s))`.
    pub fn pulse_budget_bytes(&self, budget_bytes: u32, pace_bps: u64) -> u32 {
        let quarter_second = (pace_bps / 8) / 4;
        budget_bytes.min(quarter_second.max(1_024) as u32)
    }

    /// `max_chunks_per_frame` for a carrier and the current loss (MLP spec annex D, D-19).
    pub fn max_chunks_for(&self, datagram: bool, loss_ratio: f64) -> u16 {
        if datagram {
            if loss_ratio > self.config.single_chunk_loss_ratio {
                1
            } else {
                self.config.max_chunks_datagram
            }
        } else {
            self.config.max_chunks_stream
        }
    }

    /// The idle-update interval this scheduler's configuration implies.
    pub fn idle_interval_ms_for(&self, rtt_ms: f64) -> u64 {
        (2.0 * rtt_ms).round().clamp(
            self.config.idle_interval_floor_ms as f64,
            self.config.idle_interval_ceiling_ms as f64,
        ) as u64
    }

    /// Whether an idle update is due (MLP spec §7.4).
    pub fn idle_due(&self, now_ms: u64, rtt_ms: f64) -> bool {
        if self.timer.pulse_active(now_ms) {
            return false;
        }
        let interval = self.idle_interval_ms_for(rtt_ms);
        match self.last_idle_ms {
            Some(last) => now_ms.saturating_sub(last) >= interval,
            None => true,
        }
    }

    /// Record that an idle update was emitted.
    pub fn on_idle(&mut self, now_ms: u64) {
        self.last_idle_ms = Some(now_ms);
    }

    /// Counters keyed by update kind.
    pub fn updates_by_kind(&self) -> [u64; 5] {
        self.updates_by_kind
    }

    /// Cells that were due but deferred in the last decisions.
    pub fn deferred_total(&self) -> u64 {
        self.deferred_total
    }

    /// Produce the next update plan.
    ///
    /// Plan bands win over fresh work: a running plan is what makes a
    /// non-chaining receiver whole again.
    pub fn schedule(
        &mut self,
        request: ScheduleRequest<'_>,
        grid: &BlockGrid,
        now_ms: u64,
    ) -> ScheduleDecision {
        let cell_bytes = self.config.block_bytes_for(grid);
        // A running plan owns the update.
        if let Some(band) = self.plans.next_band(request.budget.bytes, cell_bytes) {
            let estimated = (band.cells.len() as u32).saturating_mul(cell_bytes);
            self.on_frame_kind(UpdateKind::Anchor);
            self.timer.on_frame(now_ms);
            return ScheduleDecision {
                cells: band.cells,
                band: PriorityBand::P0,
                anchor: true,
                kind: UpdateKind::Anchor,
                estimated_bytes: estimated,
                deferred: band.remaining_after,
                plan_id: Some(band.plan_id),
            };
        }

        let focus_blocks = self.focus_blocks(grid);
        let mut buckets: [Vec<(PriorityBand, u32)>; 4] =
            [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        // P0: the focus rectangle, then escalated/stuck repair cells.
        for index in &focus_blocks {
            buckets[0].push((PriorityBand::P0, *index));
        }
        for due in request.due {
            let band = match due.reason {
                DueReason::Repair => PriorityBand::P1,
                DueReason::Escalated | DueReason::Stuck => PriorityBand::P0,
            };
            if band == PriorityBand::P1 {
                buckets[1].push((band, due.index));
            } else {
                buckets[0].push((band, due.index));
            }
        }
        for index in request.fresh {
            buckets[2].push((PriorityBand::P2, *index));
        }
        for index in request.refresh {
            buckets[3].push((PriorityBand::P3, *index));
        }
        for bucket in &mut buckets {
            bucket.sort_unstable();
            bucket.dedup_by_key(|(_, index)| *index);
        }
        // A cell may appear in two buckets; the highest band wins.
        let mut chosen: Vec<(PriorityBand, u32)> = Vec::new();
        let mut seen: Vec<u32> = Vec::new();
        let budget = request.budget.bytes.max(1);
        let mut used: u32 = 0;
        // P0 first: filling the highest band first is what guarantees it at
        // least half the update budget whenever it has work.
        for (band, index) in &buckets[0] {
            if seen.contains(index) {
                continue;
            }
            if used + cell_bytes > budget {
                break;
            }
            chosen.push((*band, *index));
            seen.push(*index);
            used += cell_bytes;
        }
        for bucket in buckets.iter().skip(1) {
            for (band, index) in bucket {
                if seen.contains(index) {
                    continue;
                }
                if used + cell_bytes > budget {
                    break;
                }
                chosen.push((*band, *index));
                seen.push(*index);
                used += cell_bytes;
            }
        }
        chosen.sort_unstable();
        let cells: Vec<u32> = chosen.iter().map(|(_, index)| *index).collect();
        let band = chosen
            .iter()
            .map(|(band, _)| *band)
            .min()
            .unwrap_or(PriorityBand::P2);
        let has_repair = request.due.iter().any(|due| cells.contains(&due.index));
        let kind = if cells.is_empty() {
            UpdateKind::Idle
        } else if request.anchor {
            UpdateKind::Anchor
        } else if has_repair {
            UpdateKind::Repair
        } else {
            UpdateKind::Delta
        };
        let candidate_count = buckets.iter().map(|bucket| bucket.len()).sum::<usize>();
        let deferred = candidate_count.saturating_sub(cells.len()) as u32;
        self.deferred_total += u64::from(deferred);
        if !cells.is_empty() {
            self.focus = None;
        }
        self.timer.on_frame(now_ms);
        self.on_frame_kind(kind);
        ScheduleDecision {
            estimated_bytes: (cells.len() as u32).saturating_mul(cell_bytes),
            cells,
            band,
            anchor: request.anchor || kind == UpdateKind::Anchor,
            kind,
            deferred,
            plan_id: None,
        }
    }

    fn on_frame_kind(&mut self, kind: UpdateKind) {
        let slot = match kind {
            UpdateKind::Delta => 0,
            UpdateKind::Anchor => 1,
            UpdateKind::Idle => 2,
            UpdateKind::Pulse => 3,
            UpdateKind::Repair => 4,
        };
        self.updates_by_kind[slot] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::cells::BlockGrid;

    fn scheduler() -> Scheduler {
        Scheduler::new(SchedulerConfig::default(), PlanConfig::default())
    }

    fn request<'a>(
        due: &'a [DueBlock],
        fresh: &'a [u32],
        refresh: &'a [u32],
        budget: UpdateBudget,
    ) -> ScheduleRequest<'a> {
        ScheduleRequest {
            due,
            fresh,
            refresh,
            budget,
            anchor: false,
        }
    }

    /// A budget with room for many cells, for ordering assertions.
    fn roomy_budget() -> UpdateBudget {
        UpdateBudget {
            bytes: 1_048_576,
            max_object_bytes: 524_288,
            ..UpdateBudget::default()
        }
    }

    fn due(index: u32, reason: DueReason, rounds: u8) -> DueBlock {
        DueBlock {
            index,
            reason,
            rounds,
        }
    }

    #[test]
    fn fresh_blocks_are_scheduled_as_deltas() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(320, 320, 32);
        let fresh: Vec<u32> = vec![1, 2, 3];
        let decision = scheduler.schedule(request(&[], &fresh, &[], roomy_budget()), &grid, 0);
        assert_eq!(decision.cells, vec![1, 2, 3]);
        assert_eq!(decision.band, PriorityBand::P2);
        assert_eq!(decision.kind, UpdateKind::Delta);
        assert!(!decision.anchor);
    }

    #[test]
    fn budget_truncates_the_frame_and_counts_deferred() {
        let scheduler = scheduler();
        let _ = scheduler.idle_interval_ms_for(50.0);
        let grid = BlockGrid::with_block_size(320, 320, 32);
        let budget = UpdateBudget {
            bytes: 2_048,
            ..UpdateBudget::default()
        };
        // Calibrate the truncation estimate to the raw cell ceiling so the
        // arithmetic is exact: 2 048 bytes per cell, 2 KiB of budget.
        let config = SchedulerConfig {
            cell_bytes: 2_048,
            ..SchedulerConfig::default()
        };
        let mut scheduler = Scheduler::new(config, PlanConfig::default());
        let fresh: Vec<u32> = (0..40).collect();
        let decision = scheduler.schedule(request(&[], &fresh, &[], budget), &grid, 0);
        assert_eq!(decision.cells.len(), 1);
        assert_eq!(decision.deferred, 39);
        assert!(decision.estimated_bytes <= budget.bytes);
    }

    #[test]
    fn priority_order_is_p0_then_p1_then_p2_then_p3() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(320, 320, 32);
        let due = [due(10, DueReason::Repair, 1)];
        let fresh = [20];
        let refresh = [30];
        let decision =
            scheduler.schedule(request(&due, &fresh, &refresh, roomy_budget()), &grid, 0);
        assert_eq!(
            decision.cells,
            vec![10, 20, 30],
            "ordered by band, then index"
        );
        assert_eq!(decision.band, PriorityBand::P1);
    }

    #[test]
    fn focus_rect_wins_p0() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(256, 256, 32);
        scheduler.mark_focus(CellRect::new(64, 64, 64, 64));
        let fresh = [0];
        let decision = scheduler.schedule(request(&[], &fresh, &[], roomy_budget()), &grid, 0);
        // The focus rectangle covers cells 18, 19, 26, 27, which come first.
        assert_eq!(decision.cells[0..4], [18, 19, 26, 27]);
        assert_eq!(decision.band, PriorityBand::P0);
        assert!(scheduler.focus().is_none(), "focus consumed by the update");
    }

    #[test]
    fn escalated_repair_becomes_p0() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(256, 256, 32);
        let due = [
            due(5, DueReason::Repair, 1),
            due(6, DueReason::Escalated, 5),
        ];
        let decision = scheduler.schedule(request(&due, &[], &[], roomy_budget()), &grid, 0);
        assert_eq!(decision.cells, vec![6, 5], "P0 cell first");
        assert_eq!(decision.band, PriorityBand::P0);
        assert_eq!(decision.kind, UpdateKind::Repair);
    }

    #[test]
    fn anchor_request_marks_the_frame() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(256, 256, 32);
        let fresh = [0, 1];
        let mut req = request(&[], &fresh, &[], roomy_budget());
        req.anchor = true;
        let decision = scheduler.schedule(req, &grid, 0);
        assert!(decision.anchor);
        assert_eq!(decision.kind, UpdateKind::Anchor);
    }

    #[test]
    fn an_empty_decision_is_an_idle_frame() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(256, 256, 32);
        let decision =
            scheduler.schedule(request(&[], &[], &[], UpdateBudget::default()), &grid, 0);
        assert!(decision.is_empty());
        assert_eq!(decision.kind, UpdateKind::Idle);
        assert_eq!(scheduler.updates_by_kind()[2], 1);
    }

    #[test]
    fn plan_banding_respects_the_budget() {
        let mut plans = PlanManager::new(PlanConfig::default());
        let cells: Vec<u32> = (0..100).collect();
        let plan_id = plans
            .start(PlanStartReason::Pending, cells, 0)
            .expect("plan");
        assert_eq!(
            plans.active(),
            Some((plan_id, 100, PlanStartReason::Pending))
        );
        let band = plans.next_band(4_096, 2_048).expect("band");
        assert_eq!(band.cells, vec![0, 1]);
        assert_eq!(band.remaining_after, 98);
        assert!(band.anchor);
        let huge = plans.next_band(1_048_576, 2_048).expect("band");
        assert_eq!(huge.cells.len(), 98);
        assert_eq!(plans.active(), None, "plan completes when drained");
        let (started, completed, overdue) = plans.counters();
        assert_eq!((started, completed, overdue), (1, 1, 0));
    }

    #[test]
    fn an_unpainted_band_stays_owed_by_the_plan() {
        let mut plans = PlanManager::new(PlanConfig::default());
        plans
            .start(PlanStartReason::Pending, (0..10).collect(), 0)
            .expect("plan");
        // The update never happened (refused): the whole band is owed again,
        // at the front, and nothing was consumed.
        let band = plans.next_band(4_096, 2_048).expect("band");
        assert_eq!(band.cells, vec![0, 1]);
        assert!(plans.unclaim(&[]), "an unpainted band comes back");
        assert_eq!(
            plans.active().map(|(_, remaining, _)| remaining),
            Some(10),
            "cell for cell"
        );
        let again = plans.next_band(4_096, 2_048).expect("band");
        assert_eq!(again.cells, vec![0, 1], "the same band is pulled again");
        // A update shrunk to a partial claim owes the cells it did not paint.
        assert!(plans.unclaim(&[0]), "cell 1 was not painted");
        let rest = plans.next_band(4_096, 2_048).expect("band");
        assert_eq!(rest.cells, vec![1, 2], "the owed cell leads the band");
        // A fully painted band is consumed by its update.
        assert!(!plans.unclaim(&[1, 2]));
        assert_eq!(plans.active().map(|(_, remaining, _)| remaining), Some(7));
        // A settlement with no pull is a no-op, not a rollback of an older one.
        assert!(!plans.unclaim(&[]));
        assert_eq!(plans.active().map(|(_, remaining, _)| remaining), Some(7));
    }

    #[test]
    fn a_refused_last_band_reopens_the_plan_instead_of_completing_it() {
        let mut plans = PlanManager::new(PlanConfig::default());
        plans
            .start(PlanStartReason::Pending, vec![1, 2], 0)
            .expect("plan");
        let band = plans.next_band(4_096, 2_048).expect("the whole plan");
        assert_eq!(band.cells, vec![1, 2]);
        assert_eq!(band.remaining_after, 0);
        assert!(!plans.is_active(), "the pull drained the plan");
        assert_eq!(plans.counters().1, 1, "complete at the pull, manager-side");
        // The update was refused: the plan is not complete until painted.
        assert!(plans.unclaim(&[]));
        assert!(plans.is_active(), "the plan reopens");
        assert_eq!(plans.counters().1, 0, "the completion is un-counted");
        assert_eq!(plans.active().map(|(_, remaining, _)| remaining), Some(2));
        // Painting the reopened band completes the plan for real.
        let band = plans.next_band(4_096, 2_048).expect("the reopened band");
        assert_eq!(band.cells, vec![1, 2]);
        assert!(!plans.unclaim(&[1, 2]));
        assert_eq!(plans.counters(), (1, 1, 0));
    }

    #[test]
    fn plans_are_rate_limited_per_viewer() {
        let mut plans = PlanManager::new(PlanConfig::default());
        assert!(
            plans
                .start(PlanStartReason::Pending, vec![1], 1_000)
                .is_some()
        );
        assert!(
            plans
                .start(PlanStartReason::Stuck, vec![2], 1_100)
                .is_none(),
            "RESET_MIN_INTERVAL_MS is per receiver"
        );
        assert!(
            plans
                .start(PlanStartReason::Stuck, vec![2], 1_300)
                .is_some()
        );
    }

    #[test]
    fn plan_start_condition_uses_pending_and_stuck() {
        let plans = PlanManager::new(PlanConfig::default());
        assert!(!plans.should_start_for_pending(16, 0, 0));
        assert!(plans.should_start_for_pending(17, 0, 0), "above threshold");
        assert!(plans.should_start_for_pending(0, 1, 0), "stuck cell");
    }

    #[test]
    fn overdue_plan_restarts_without_the_rate_limit() {
        let mut plans = PlanManager::new(PlanConfig {
            rtt_ms: 2_000.0,
            ..PlanConfig::default()
        });
        let plan_id = plans
            .start(PlanStartReason::Pending, (0..50).collect(), 0)
            .expect("plan");
        assert_eq!(plans.poll_overdue(1_000), None);
        let overdue = plans.poll_overdue(60_000).expect("overdue");
        assert_eq!(overdue, plan_id);
        assert_eq!(plans.counters().2, 1);
        // The restart is immediate: freshness beats completeness.
        assert!(
            plans
                .start(PlanStartReason::Pending, vec![7, 8], 60_000)
                .is_some()
        );
    }

    #[test]
    fn plan_deadline_is_two_rtt_plus_thirty_seconds() {
        let config = PlanConfig {
            rtt_ms: 2_000.0,
            ..PlanConfig::default()
        };
        assert_eq!(config.deadline_ms(), 34_000);
    }

    #[test]
    fn plan_frames_win_over_fresh_work() {
        let mut scheduler = scheduler();
        let grid = BlockGrid::with_block_size(256, 256, 32);
        let plan_id = scheduler
            .plans_mut()
            .start(PlanStartReason::Resume, vec![1, 2, 3], 0)
            .expect("plan");
        let fresh = [40];
        let decision = scheduler.schedule(request(&[], &fresh, &[], roomy_budget()), &grid, 0);
        assert_eq!(decision.plan_id, Some(plan_id));
        assert_eq!(decision.cells, vec![1, 2, 3]);
        assert_eq!(decision.kind, UpdateKind::Anchor);
    }

    #[test]
    fn pulse_window_raises_the_fps_and_expires() {
        let mut scheduler = scheduler();
        assert!(!scheduler.timer().pulse_active(0));
        scheduler.request_pulse(1_000, 400.0);
        assert!(scheduler.timer().pulse_active(1_200));
        assert!(
            scheduler.timer().due(1_200, 5.0),
            "pulse bypasses the fps gate"
        );
        assert!(!scheduler.timer().pulse_active(1_500));
        assert_eq!(scheduler.pulse_fps(5.0), 10.0);
        assert_eq!(scheduler.pulse_fps(30.0), 30.0);
        assert_eq!(scheduler.pulse_fps(1.0), 4.0);
    }

    #[test]
    fn pulse_budget_is_a_quarter_second_capped() {
        let scheduler = scheduler();
        assert_eq!(scheduler.pulse_budget_bytes(4_096, 32_000), 1_024);
        assert_eq!(scheduler.pulse_budget_bytes(4_096, 1_000_000), 4_096);
    }

    #[test]
    fn idle_interval_clamps_between_one_and_three_seconds() {
        assert_eq!(UpdateTimer::idle_interval_ms(10.0), 1_000);
        assert_eq!(UpdateTimer::idle_interval_ms(1_000.0), 2_000);
        assert_eq!(UpdateTimer::idle_interval_ms(2_000.0), 3_000);
        let mut scheduler = scheduler();
        assert!(scheduler.idle_due(0, 1_000.0));
        scheduler.on_idle(0);
        assert!(!scheduler.idle_due(1_500, 1_000.0));
        assert!(scheduler.idle_due(2_000, 1_000.0));
    }

    #[test]
    fn max_chunks_follows_carrier_and_loss() {
        let scheduler = scheduler();
        assert_eq!(scheduler.max_chunks_for(false, 0.5), 8);
        assert_eq!(scheduler.max_chunks_for(true, 0.0), 4);
        assert_eq!(scheduler.max_chunks_for(true, 0.06), 1);
    }

    #[test]
    fn chunk_count_never_exceeds_max_chunks() {
        let budget = UpdateBudget {
            bytes: 65_536,
            max_chunks: 4,
            max_chunk_wire_bytes: 65_536,
            path_chunk_cap: 65_536,
            ..UpdateBudget::default()
        };
        assert_eq!(budget.part_count(0), Some(1));
        assert_eq!(budget.part_count(65_536), Some(1));
        assert_eq!(budget.part_count(65_537), Some(2));
        assert_eq!(budget.part_count(262_144), Some(4));
        assert_eq!(budget.part_count(262_145), None, "decompose instead");
        assert_eq!(budget.payload_ceiling(), 262_144);
    }

    #[test]
    fn update_timer_gates_the_update_rate() {
        let mut timer = UpdateTimer::new();
        assert!(timer.due(0, 10.0), "no update yet");
        timer.on_frame(0);
        assert!(!timer.due(50, 10.0));
        assert!(timer.due(100, 10.0));
    }
}
