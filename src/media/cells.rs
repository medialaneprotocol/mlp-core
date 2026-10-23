//! Per-receiver cell accounting (MLP spec §8, D-7): the convergence engine.
//!
//! One [`CellAccounting`] per receiver holds the cell grid, the per-cell
//! `(update_id, sent_at_ms)` facts, the 64-update ack window and the update ring.
//! It answers exactly two questions for the scheduler: *which cells are due
//! for this receiver* and *is this receiver converged*.
//!
//! All time is the session-relative millisecond clock the caller injects; the
//! module never reads a clock.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::MediaAck;

/// Maximum cells in a grid (MLP spec §8).
pub const MAX_CELLS: u32 = 2_048;
/// Minimum cell size in cells (MLP spec §8).
pub const MIN_CELL_SIZE: u32 = 32;
/// Maximum cell size in cells (MLP spec §8).
pub const MAX_CELL_SIZE: u32 = 256;
/// Repair rounds after which a cell escalates to P0 (MLP spec §8).
pub const REPAIR_MAX_ROUNDS: u8 = 4;
/// The floor of the repair delay on datagram carriers.
pub const REPAIR_DELAY_DATAGRAM_FLOOR_MS: u64 = 250;
/// The ceiling of the repair delay on datagram carriers.
pub const REPAIR_DELAY_DATAGRAM_CEIL_MS: u64 = 5_000;
/// The ceiling of the repair delay on stream carriers.
pub const REPAIR_DELAY_STREAM_CEIL_MS: u64 = 3_000;
/// The floor of the ack ring, in updates (MLP spec §8).
pub const ACK_RING_FLOOR: usize = 64;

/// The repair delay the specification's rule gives a link (MLP spec §9.1).
///
/// A stream carrier waits `clamp(RTT + update_interval, 250 ms, 3 s)` before
/// it re-sends a cell the receiver has not acknowledged, and a datagram
/// carrier `clamp(2 × RTT, 250 ms, 5 s)`. The delay is a function of the
/// link's own RTT: a fast link repairs sooner, and no tier is handed another
/// tier's grace.
pub fn repair_delay_ms(datagram: bool, rtt_ms: f64, update_interval_ms: u64) -> u64 {
    if datagram {
        (2.0 * rtt_ms).round().clamp(
            REPAIR_DELAY_DATAGRAM_FLOOR_MS as f64,
            REPAIR_DELAY_DATAGRAM_CEIL_MS as f64,
        ) as u64
    } else {
        (rtt_ms + update_interval_ms as f64).round().clamp(
            REPAIR_DELAY_DATAGRAM_FLOOR_MS as f64,
            REPAIR_DELAY_STREAM_CEIL_MS as f64,
        ) as u64
    }
}

/// A cell rectangle in surface coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CellRect {
    /// Left edge in cells.
    pub x: u32,
    /// Top edge in cells.
    pub y: u32,
    /// Width in cells.
    pub w: u32,
    /// Height in cells.
    pub h: u32,
}

impl CellRect {
    /// An empty rectangle.
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    /// Create a rectangle.
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Whether the rectangle covers no cells.
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// The bounding box of two rectangles (used to grow the input-focus rect).
    pub fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Self::new(x, y, right - x, bottom - y)
    }
}

/// The shared cell grid for a geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellGrid {
    width: u32,
    height: u32,
    cell_size: u32,
    cols: u32,
    rows: u32,
}

impl CellGrid {
    /// Build the grid with the specification's formula: the smallest power-of-two
    /// cell size at least `sqrt(cells / 2048)`, clamped to `[32, 256]`.
    pub fn for_geometry(width: u32, height: u32) -> Self {
        let cells = u64::from(width) * u64::from(height);
        let target = (cells as f64 / f64::from(MAX_CELLS)).sqrt().ceil() as u32;
        let mut cell_size = MIN_CELL_SIZE;
        while cell_size < target {
            cell_size *= 2;
        }
        Self::with_cell_size(width, height, cell_size.clamp(MIN_CELL_SIZE, MAX_CELL_SIZE))
    }

    /// Build the grid with an explicit cell size.
    pub fn with_cell_size(width: u32, height: u32, cell_size: u32) -> Self {
        let cell_size = cell_size.clamp(MIN_CELL_SIZE, MAX_CELL_SIZE);
        let cols = width.div_ceil(cell_size).max(1);
        let rows = height.div_ceil(cell_size).max(1);
        Self {
            width,
            height,
            cell_size,
            cols,
            rows,
        }
    }

    /// Surface width in cells.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Surface height in cells.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Cell size in cells.
    pub fn cell_size(&self) -> u32 {
        self.cell_size
    }

    /// Columns.
    pub fn cols(&self) -> u32 {
        self.cols
    }

    /// Rows.
    pub fn rows(&self) -> u32 {
        self.rows
    }

    /// Total cells (never above [`MAX_CELLS`] for a legal geometry).
    pub fn cells(&self) -> u32 {
        self.cols * self.rows
    }

    /// A geometry fingerprint: changes when the grid changes.
    pub fn geometry_key(&self) -> u64 {
        (u64::from(self.width) << 32) ^ (u64::from(self.height) << 8) ^ u64::from(self.cell_size)
    }

    /// The cell index of a `(col, row)` pair.
    pub fn index(&self, col: u32, row: u32) -> Option<u32> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        Some(row * self.cols + col)
    }

    /// The `(col, row)` of a cell index.
    pub fn coords(&self, index: u32) -> Option<(u32, u32)> {
        if index >= self.cells() {
            return None;
        }
        Some((index % self.cols, index / self.cols))
    }

    /// The cell rectangle of a cell index.
    pub fn rect(&self, index: u32) -> Option<CellRect> {
        let (col, row) = self.coords(index)?;
        let x = col * self.cell_size;
        let y = row * self.cell_size;
        let w = self.cell_size.min(self.width.saturating_sub(x));
        let h = self.cell_size.min(self.height.saturating_sub(y));
        Some(CellRect::new(x, y, w, h))
    }

    /// The cell containing a cell, when the cell is on the surface.
    pub fn cell_at(&self, x_px: u32, y_px: u32) -> Option<u32> {
        if x_px >= self.width || y_px >= self.height {
            return None;
        }
        self.index(x_px / self.cell_size, y_px / self.cell_size)
    }

    /// Every cell intersecting a rectangle.
    ///
    /// Clamps a rectangle that runs off-screen, so a focus rect during a
    /// geometry change cannot panic.
    pub fn cells_in_rect(&self, rect: &CellRect) -> Vec<u32> {
        if rect.is_empty() {
            return Vec::new();
        }
        let x0 = rect.x.min(self.width.saturating_sub(1));
        let y0 = rect.y.min(self.height.saturating_sub(1));
        let x1 = (rect.x + rect.w)
            .saturating_sub(1)
            .min(self.width.saturating_sub(1));
        let y1 = (rect.y + rect.h)
            .saturating_sub(1)
            .min(self.height.saturating_sub(1));
        let col0 = x0 / self.cell_size;
        let col1 = x1 / self.cell_size;
        let row0 = y0 / self.cell_size;
        let row1 = y1 / self.cell_size;
        let mut cells = Vec::new();
        for row in row0..=row1 {
            for col in col0..=col1 {
                if let Some(index) = self.index(col, row) {
                    cells.push(index);
                }
            }
        }
        cells
    }

    /// The cell rectangle covering a set of cell indices.
    pub fn bounding_rect(&self, cells: &[u32]) -> CellRect {
        let mut rect = CellRect::EMPTY;
        for index in cells {
            if let Some(cell) = self.rect(*index) {
                rect = rect.union(&cell);
            }
        }
        rect
    }
}

/// Why a cell is due for a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DueReason {
    /// The cell is pending, was sent, and the repair delay has passed.
    Repair,
    /// The cell passed `repair_max_rounds` repair rounds.
    Escalated,
    /// The cell is still due after `2 × repair_delay` past escalation.
    Stuck,
}

/// A cell the scheduler must consider for a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DueCell {
    /// Cell index.
    pub index: u32,
    /// Why it is due.
    pub reason: DueReason,
    /// Repair rounds this cell has accumulated.
    pub rounds: u8,
}

/// Counters for `mlp_*` series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CellCounters {
    /// Acks whose update fell out of the ring (conservative re-send).
    pub ack_ring_misses: u64,
    /// Cells that were re-sent after the repair delay.
    pub repair_rounds_resent: u64,
    /// Cells still due after the escalation ladder.
    pub repair_stuck_cells: u64,
    /// Cells marked stale by a update-epoch change.
    pub epoch_resets: u64,
    /// Acks that named an older chain epoch than the sender's.
    pub epoch_stale_acks: u64,
    /// Updates the accounting accepted from acks.
    pub applied_updates: u64,
}

/// The result of processing one ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AckOutcome {
    /// Applied updates named by the bitmap.
    pub applied_updates: u32,
    /// Cells removed from pending.
    pub cleared_cells: u32,
    /// Whether the ack named a update outside the ring.
    pub ring_miss: bool,
    /// Whether the ack's epoch differs from ours.
    pub epoch_mismatch: bool,
    /// Whether the ack named an **older** epoch than ours.
    ///
    /// The chain epoch is the sender's, and it only moves forward (MLP spec
    /// §5.1): an acknowledgement that names an older epoch is a stale
    /// acknowledgement from a receiver that has not seen the break yet. It is
    /// counted, never obeyed — obeying it would reset the applied-update
    /// watermark and re-mark every cell due, which is a re-send storm for a
    /// chain the receiver still holds.
    pub epoch_stale: bool,
    /// Whether the receiver is converged after this ack.
    pub converged: bool,
}

#[derive(Debug, Clone, Copy)]
struct CellState {
    update_id: u64,
    sent_at_ms: u64,
    rounds: u8,
    due_override: bool,
    ever_sent: bool,
    /// When the cell first passed `REPAIR_MAX_ROUNDS`. "Still due after
    /// `2 × repair_delay`" is measured from here, not from the last emission:
    /// a cell that keeps being re-sent every repair delay is exactly the case
    /// the anchor plan exists for (MLP spec §8).
    escalated_at_ms: Option<u64>,
}

impl CellState {
    fn fresh() -> Self {
        Self {
            update_id: 0,
            sent_at_ms: 0,
            rounds: 0,
            due_override: false,
            ever_sent: false,
            escalated_at_ms: None,
        }
    }
}

#[derive(Debug, Clone)]
struct UpdateRecord {
    update_id: u64,
    cells: Vec<u32>,
}

/// Accounting configuration; every value is a policy constant of §7.5.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CellAccountingConfig {
    /// Whether the carrier is datagram-class (selects the repair delay rule).
    pub datagram: bool,
    /// The update interval implied by the target fps.
    pub update_interval_ms: u64,
    /// The RTT smoothing used for the repair delay.
    pub rtt_ms: f64,
    /// The number of updates kept for ack resolution.
    pub ack_ring: usize,
}

impl Default for CellAccountingConfig {
    fn default() -> Self {
        Self {
            datagram: false,
            update_interval_ms: 33,
            rtt_ms: 50.0,
            ack_ring: ACK_RING_FLOOR,
        }
    }
}

impl CellAccountingConfig {
    pub fn with_datagram(mut self, datagram: bool) -> Self {
        self.datagram = datagram;
        self
    }

    pub fn with_update_interval_ms(mut self, ms: u64) -> Self {
        self.update_interval_ms = ms;
        self
    }

    pub fn with_rtt_ms(mut self, rtt: f64) -> Self {
        self.rtt_ms = rtt;
        self
    }

    pub fn with_ack_ring(mut self, ring: usize) -> Self {
        self.ack_ring = ring;
        self
    }
}

/// One receiver's cell accounting.
#[derive(Debug, Clone)]
pub struct CellAccounting {
    grid: CellGrid,
    config: CellAccountingConfig,
    pending: Vec<CellState>,
    /// Whether a cell was ever emitted in this epoch. An epoch change makes
    /// every such cell suspect again (MLP spec §7.1), even when its ack had
    /// already cleared it.
    emitted_any: Vec<bool>,
    ring: VecDeque<UpdateRecord>,
    epoch: u32,
    last_applied_update_id: u64,
    last_sent_update_id: u64,
    last_ack_at_ms: Option<u64>,
    /// The session-relative ms of the last **progress**: the moment the
    /// receiver's applied-update watermark advanced.
    ///
    /// This is the freshness fact of MLP spec §6.7.1 — the content
    /// advanced. `last_ack_at_ms` is a different fact: an acknowledgement
    /// says the lane is alive, and a lane can be alive with a content that
    /// stands still.
    last_progress_at_ms: Option<u64>,
    counters: CellCounters,
    stuck_cells: Vec<u32>,
}

impl CellAccounting {
    /// Create accounting for a grid with no emitted history.
    pub fn new(grid: CellGrid, config: CellAccountingConfig) -> Self {
        Self {
            grid,
            config,
            pending: vec![CellState::fresh(); grid.cells() as usize],
            emitted_any: vec![false; grid.cells() as usize],
            ring: VecDeque::new(),
            epoch: 0,
            last_applied_update_id: 0,
            last_sent_update_id: 0,
            last_ack_at_ms: None,
            last_progress_at_ms: None,
            counters: CellCounters::default(),
            stuck_cells: Vec::new(),
        }
    }

    /// The grid in force.
    pub fn grid(&self) -> &CellGrid {
        &self.grid
    }

    /// Change the geometry: every cell becomes stale (canvas trust resets).
    ///
    /// Returns the previous grid so the caller can count the change.
    pub fn set_geometry(&mut self, grid: CellGrid) -> CellGrid {
        let previous = self.grid;
        if previous.geometry_key() == grid.geometry_key() {
            return previous;
        }
        self.grid = grid;
        self.pending = vec![CellState::fresh(); grid.cells() as usize];
        self.emitted_any = vec![false; grid.cells() as usize];
        self.ring.clear();
        self.last_applied_update_id = 0;
        self.last_sent_update_id = 0;
        self.stuck_cells.clear();
        self.counters.epoch_resets += 1;
        previous
    }

    /// The chain epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Set the chain epoch. A change resets canvas trust (MLP spec §7.1).
    ///
    /// Every cell emitted in the previous epoch becomes due again — the
    /// receiver's chain may be from a different capture — but nothing is
    /// *cleared*: the cells stay until fresh ones arrive.
    pub fn set_epoch(&mut self, epoch: u32) {
        if epoch == self.epoch {
            return;
        }
        self.epoch = epoch;
        for (index, state) in self.pending.iter_mut().enumerate() {
            if self.emitted_any[index] {
                *state = CellState {
                    update_id: 0,
                    sent_at_ms: 0,
                    rounds: 0,
                    due_override: true,
                    ever_sent: true,
                    escalated_at_ms: None,
                };
            }
        }
        self.last_applied_update_id = 0;
        self.counters.epoch_resets += 1;
    }

    /// Update the RTT that drives the repair delay.
    pub fn set_rtt_ms(&mut self, rtt_ms: f64) {
        self.config.rtt_ms = rtt_ms.max(1.0);
    }

    /// Update the update interval that drives the repair delay and ring.
    pub fn set_update_interval_ms(&mut self, interval_ms: u64) {
        self.config.update_interval_ms = interval_ms.max(1);
        let ring = self.required_ack_ring();
        self.config.ack_ring = ring;
        while self.ring.len() > ring {
            self.ring.pop_front();
        }
    }

    /// Record that `update_id` was emitted for this receiver.
    ///
    /// `cells` are the cell indices the update's regions cover; `E` advances
    /// exactly at these cells (MLP spec §8).
    pub fn mark_emitted(&mut self, update_id: u64, cells: &[u32], now_ms: u64) {
        let cells: Vec<u32> = cells
            .iter()
            .copied()
            .filter(|index| *index < self.grid.cells())
            .collect();
        for index in &cells {
            let state = &mut self.pending[*index as usize];
            if state.ever_sent {
                // A re-emission is a repair round.
                state.rounds = state.rounds.saturating_add(1);
                self.counters.repair_rounds_resent += 1;
                if state.rounds > REPAIR_MAX_ROUNDS && state.escalated_at_ms.is_none() {
                    state.escalated_at_ms = Some(now_ms);
                }
            }
            state.update_id = update_id;
            state.sent_at_ms = now_ms;
            state.ever_sent = true;
            state.due_override = false;
            self.emitted_any[*index as usize] = true;
        }
        self.last_sent_update_id = self.last_sent_update_id.max(update_id);
        self.ring.push_back(UpdateRecord { update_id, cells });
        let ring = self.required_ack_ring();
        self.config.ack_ring = ring;
        while self.ring.len() > ring {
            self.ring.pop_front();
        }
    }

    /// Apply one ack (MLP spec §8).
    pub fn apply_ack(&mut self, ack: &MediaAck, now_ms: u64) -> AckOutcome {
        let mut outcome = AckOutcome::default();
        // The chain epoch belongs to the sender, and it only moves forward
        // (MLP spec §5.1): the sender increments it when it breaks the chain,
        // and the receiver adopts a *greater* one. An acknowledgement that
        // names an older epoch is a stale acknowledgement — the receiver has
        // not seen the break yet — and obeying it would reset the
        // applied-update watermark and re-mark every emitted cell due. That
        // is a re-send storm for a chain the receiver still holds, so the
        // epoch is counted and left alone.
        if ack.chain_epoch > self.epoch {
            outcome.epoch_mismatch = true;
            self.set_epoch(ack.chain_epoch);
        } else if ack.chain_epoch < self.epoch {
            outcome.epoch_stale = true;
            self.counters.epoch_stale_acks += 1;
        }
        self.last_ack_at_ms = Some(now_ms);
        // An ack can never name a update we did not send; clamping keeps a
        // bogus (or cross-session) ack from clearing unemitted history.
        let effective = ack.highest_applied_update_id.min(self.last_sent_update_id);
        let applied: Vec<u64> = ack
            .applied_updates()
            .filter(|update| *update <= self.last_sent_update_id)
            .collect();
        outcome.applied_updates = applied.len() as u32;
        self.counters.applied_updates += u64::from(outcome.applied_updates);

        // A update outside the ring means the sender cannot reconstruct the
        // coverage: mark every cell emitted after the last applied id as due
        // (MLP spec §8). Correct, at the cost of re-sending.
        if effective > self.last_applied_update_id {
            let ring_covers = self
                .ring
                .front()
                .map(|record| record.update_id <= effective)
                .unwrap_or(false);
            if !ring_covers {
                outcome.ring_miss = true;
                self.counters.ack_ring_misses += 1;
                for state in &mut self.pending {
                    if state.ever_sent && state.update_id > self.last_applied_update_id {
                        state.due_override = true;
                    }
                }
            }
        }

        // A cell is acked exactly when the update that last emitted it is
        // applied. The design's "updates <= that id" rule leans on codec
        // chaining, which an anchor update does not preserve (it may be applied
        // onto any held state), so the accounting clears the coverage of the
        // applied updates themselves: strictly safer, and identical in the
        // intact-chain case because every applied update is acked in turn.
        for update in applied {
            let Some(covered) = self
                .ring
                .iter()
                .find(|record| record.update_id == update)
                .map(|record| record.cells.clone())
            else {
                // Outside the ring: nothing can be cleared safely.
                continue;
            };
            for cell in covered {
                let state = &mut self.pending[cell as usize];
                if state.ever_sent && state.update_id == update {
                    *state = CellState::fresh();
                    outcome.cleared_cells += 1;
                }
            }
        }
        if effective > self.last_applied_update_id {
            self.last_applied_update_id = effective;
            // The content advanced: this is the progress the lane's freshness
            // is measured against (MLP spec §6.7.1).
            self.last_progress_at_ms = Some(now_ms);
        }
        // The stuck set is recomputed from the current truth.
        self.stuck_cells
            .retain(|index| self.pending[*index as usize].ever_sent);
        outcome.converged = self.converged();
        outcome
    }

    /// The repair delay in ms (MLP spec §9.1).
    pub fn repair_delay_ms(&self) -> u64 {
        repair_delay_ms(
            self.config.datagram,
            self.config.rtt_ms,
            self.config.update_interval_ms,
        )
    }

    /// Cells that are due for this receiver, ordered `(reason, index)`.
    ///
    /// Fresh changes and background refresh are *not* represented here: they
    /// come from the shared capture diff, not from this receiver's history.
    pub fn due_cells(&self, now_ms: u64) -> Vec<DueCell> {
        let repair_delay = self.repair_delay_ms();
        let mut due = Vec::new();
        for (index, state) in self.pending.iter().enumerate() {
            if !state.ever_sent {
                continue;
            }
            let elapsed = now_ms.saturating_sub(state.sent_at_ms);
            let overdue = elapsed >= repair_delay;
            if !overdue && !state.due_override {
                continue;
            }
            let reason = if state.rounds > REPAIR_MAX_ROUNDS && elapsed >= 2 * repair_delay {
                DueReason::Stuck
            } else if state.rounds > REPAIR_MAX_ROUNDS {
                DueReason::Escalated
            } else {
                DueReason::Repair
            };
            due.push(DueCell {
                index: index as u32,
                reason,
                rounds: state.rounds,
            });
        }
        due.sort_by_key(|cell| (cell.reason, cell.index));
        due
    }

    /// Recompute and return the cells that are stuck after the ladder.
    ///
    /// The list is what an anchor plan must cover (MLP spec §8, §7.6).
    pub fn note_stuck(&mut self, now_ms: u64) -> Vec<u32> {
        let repair_delay = self.repair_delay_ms();
        let mut stuck = Vec::new();
        for (index, state) in self.pending.iter().enumerate() {
            if !state.ever_sent || state.rounds <= REPAIR_MAX_ROUNDS {
                continue;
            }
            // Still unacked `2 × repair_delay` after the escalation: the repair
            // loop is not converging, so the cell belongs in an anchor plan.
            let escalated = state.escalated_at_ms.unwrap_or(state.sent_at_ms);
            if now_ms.saturating_sub(escalated) >= 2 * repair_delay {
                stuck.push(index as u32);
            }
        }
        stuck.sort_unstable();
        if stuck.len() > self.stuck_cells.len() {
            self.counters.repair_stuck_cells += (stuck.len() - self.stuck_cells.len()) as u64;
        }
        self.stuck_cells = stuck.clone();
        stuck
    }

    /// Cells the ladder escalated to P0.
    pub fn escalated_cells(&self) -> Vec<u32> {
        self.pending
            .iter()
            .enumerate()
            .filter(|(_, state)| state.ever_sent && state.rounds > REPAIR_MAX_ROUNDS)
            .map(|(index, _)| index as u32)
            .collect()
    }

    /// Cells that are pending (sent and not acked).
    pub fn pending_cells(&self) -> Vec<u32> {
        self.pending
            .iter()
            .enumerate()
            .filter(|(_, state)| state.ever_sent)
            .map(|(index, _)| index as u32)
            .collect()
    }

    /// The number of pending cells.
    pub fn pending_count(&self) -> u32 {
        self.pending.iter().filter(|state| state.ever_sent).count() as u32
    }

    /// The pending fraction, for `mlp_stale_fraction`.
    pub fn stale_fraction(&self) -> f64 {
        f64::from(self.pending_count()) / f64::from(self.grid.cells().max(1))
    }

    /// Convergence: `pending = ∅` and the last update is applied.
    pub fn converged(&self) -> bool {
        self.pending.iter().all(|state| !state.ever_sent)
            && self.last_applied_update_id >= self.last_sent_update_id
    }

    /// The last update id the receiver acked as applied.
    pub fn last_applied_update_id(&self) -> u64 {
        self.last_applied_update_id
    }

    /// The last update id this sender emitted for the receiver.
    pub fn last_sent_update_id(&self) -> u64 {
        self.last_sent_update_id
    }

    /// The last ack's session time.
    pub fn last_ack_at_ms(&self) -> Option<u64> {
        self.last_ack_at_ms
    }

    /// The session time of the last **progress**: the moment the receiver's
    /// applied-update watermark advanced (MLP spec §6.7.1).
    ///
    /// The lane's freshness is measured against this and not against the
    /// last acknowledgement: an acknowledgement that names no new update
    /// says the lane is alive, and the lane's state is about the content.
    pub fn last_progress_at_ms(&self) -> Option<u64> {
        self.last_progress_at_ms
    }

    /// The number of update records in the ring.
    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// The ring size this RTT and update interval require (MLP spec §8).
    pub fn required_ack_ring(&self) -> usize {
        let rtt = self.config.rtt_ms;
        let interval = self.config.update_interval_ms.max(1) as f64;
        let needed = (2.0 * rtt / interval).ceil() as usize + 8;
        needed.max(ACK_RING_FLOOR)
    }

    /// Counters for metrics.
    pub fn counters(&self) -> CellCounters {
        self.counters
    }

    /// The anchor threshold for this grid: `max(16, cells / 4)`.
    pub fn anchor_threshold(&self) -> u32 {
        (self.grid.cells() / 4).max(16)
    }

    /// Whether the pending set has crossed the anchor threshold.
    pub fn needs_anchor(&self) -> bool {
        self.pending_count() > self.anchor_threshold()
    }

    /// Update ids currently in the ring, oldest first.
    pub fn ring_updates(&self) -> impl Iterator<Item = u64> + '_ {
        self.ring.iter().map(|record| record.update_id)
    }

    /// The cells the ring attributes to a update id, when known.
    pub fn update_cells(&self, update_id: u64) -> Option<&[u32]> {
        self.ring
            .iter()
            .find(|record| record.update_id == update_id)
            .map(|record| record.cells.as_slice())
    }

    /// Forget everything (park eviction, fresh media session).
    pub fn reset(&mut self) {
        for state in &mut self.pending {
            *state = CellState::fresh();
        }
        self.emitted_any.iter_mut().for_each(|flag| *flag = false);
        self.ring.clear();
        self.last_applied_update_id = 0;
        self.last_sent_update_id = 0;
        self.last_ack_at_ms = None;
        self.last_progress_at_ms = None;
        self.stuck_cells.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(highest: u64, bitmap: u64) -> MediaAck {
        MediaAck {
            highest_applied_update_id: highest,
            ack_bitmap: bitmap,
            ..MediaAck::default()
        }
    }

    fn accounting(width: u32, height: u32, datagram: bool) -> CellAccounting {
        let grid = CellGrid::for_geometry(width, height);
        CellAccounting::new(
            grid,
            CellAccountingConfig {
                datagram,
                update_interval_ms: 33,
                rtt_ms: 150.0,
                ack_ring: ACK_RING_FLOOR,
            },
        )
    }

    #[test]
    fn grid_formula_matches_the_design() {
        // 1080p: sqrt(2 073 600 / 2 048) ≈ 31.8 ⇒ 32 px, 60 × 34 = 2040 cells.
        let grid = CellGrid::for_geometry(1920, 1080);
        assert_eq!(grid.cell_size(), 32);
        assert_eq!(grid.cols(), 60);
        assert_eq!(grid.rows(), 34);
        assert_eq!(grid.cells(), 2040);
        assert!(grid.cells() <= MAX_CELLS);
        // The design's documented example grid: 64 px ⇒ 510 cells at 1080p.
        let coarse = CellGrid::with_cell_size(1920, 1080, 64);
        assert_eq!(coarse.cells(), 510);
    }

    #[test]
    fn grid_formula_clamps_to_32_and_256() {
        let tiny = CellGrid::for_geometry(64, 64);
        assert_eq!(tiny.cell_size(), MIN_CELL_SIZE);
        let big = CellGrid::for_geometry(4_096, 4_096);
        assert_eq!(big.cell_size(), 128);
        assert!(big.cells() <= MAX_CELLS);
        // The 256 px clamp is the hard bound: a geometry that would need
        // larger cells saturates it (and the 2048-cell budget with it, so
        // the driver must downscale before encoding).
        let beyond = CellGrid::for_geometry(12_288, 12_288);
        assert_eq!(beyond.cell_size(), MAX_CELL_SIZE);
        assert!(beyond.cells() > MAX_CELLS, "documented saturation");
    }

    #[test]
    fn grid_rect_and_cell_mapping_round_trip() {
        let grid = CellGrid::with_cell_size(100, 60, 32);
        assert_eq!(grid.cols(), 4);
        assert_eq!(grid.rows(), 2);
        assert_eq!(grid.cells(), 8);
        assert_eq!(grid.index(3, 1), Some(7));
        assert_eq!(grid.coords(7), Some((3, 1)));
        assert_eq!(grid.rect(7), Some(CellRect::new(96, 32, 4, 28)));
        assert_eq!(grid.cell_at(0, 0), Some(0));
        assert_eq!(grid.cell_at(99, 59), Some(7));
        assert_eq!(grid.cell_at(200, 10), None);
        assert!(grid.rect(8).is_none());
    }

    #[test]
    fn cells_in_rect_covers_the_rectangle() {
        let grid = CellGrid::with_cell_size(128, 128, 32);
        let cells = grid.cells_in_rect(&CellRect::new(0, 0, 64, 64));
        assert_eq!(cells, vec![0, 1, 4, 5]);
        let clamped = grid.cells_in_rect(&CellRect::new(120, 120, 500, 500));
        assert_eq!(clamped, vec![15]);
        assert!(grid.cells_in_rect(&CellRect::EMPTY).is_empty());
        assert_eq!(grid.bounding_rect(&[0, 5]), CellRect::new(0, 0, 64, 64));
    }

    #[test]
    fn emitted_cells_become_pending_and_an_ack_clears_them() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0, 1, 2], 0);
        assert_eq!(accounting.pending_count(), 3);
        assert!(!accounting.converged());
        let outcome = accounting.apply_ack(&ack(1, 0b1), 10);
        assert_eq!(outcome.cleared_cells, 3);
        assert!(outcome.converged);
        assert!(accounting.converged());
    }

    #[test]
    fn unacked_cells_become_due_after_the_repair_delay() {
        let mut accounting = accounting(200, 200, false);
        // Stream repair delay = clamp(rtt + update_interval, 250, 3000) = 250.
        assert_eq!(accounting.repair_delay_ms(), 250);
        accounting.mark_emitted(1, &[0, 1], 0);
        assert!(accounting.due_cells(100).is_empty(), "not due yet");
        let due = accounting.due_cells(300);
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].reason, DueReason::Repair);
        assert_eq!(due[1].index, 1);
    }

    #[test]
    fn datagram_repair_delay_is_twice_the_rtt() {
        let mut accounting = accounting(200, 200, true);
        accounting.set_rtt_ms(800.0);
        assert_eq!(accounting.repair_delay_ms(), 1_600);
        accounting.set_rtt_ms(3_000.0);
        assert_eq!(accounting.repair_delay_ms(), 5_000, "ceiling");
        accounting.set_rtt_ms(50.0);
        assert_eq!(accounting.repair_delay_ms(), 250, "floor");
    }

    #[test]
    fn stream_repair_delay_is_rtt_plus_update_interval() {
        let mut accounting = accounting(200, 200, false);
        accounting.set_rtt_ms(1_000.0);
        accounting.set_update_interval_ms(200);
        assert_eq!(accounting.repair_delay_ms(), 1_200);
        accounting.set_rtt_ms(10_000.0);
        assert_eq!(accounting.repair_delay_ms(), 3_000, "ceiling");
    }

    #[test]
    fn repair_ladder_escalates_then_reports_stuck_cells() {
        let mut accounting = accounting(200, 200, false);
        let mut now = 0u64;
        accounting.mark_emitted(1, &[0], now);
        for round in 2..=6 {
            now += 300;
            // The cell is due each round and is re-emitted: the ladder counts.
            accounting.mark_emitted(round, &[0], now);
        }
        let due = accounting.due_cells(now + 400);
        assert_eq!(due.len(), 1);
        assert!(matches!(
            due[0].reason,
            DueReason::Escalated | DueReason::Stuck
        ));
        assert!(due[0].rounds > REPAIR_MAX_ROUNDS);
        let stuck = accounting.note_stuck(now + 2 * accounting.repair_delay_ms() + 1);
        assert_eq!(stuck, vec![0]);
        assert_eq!(accounting.counters().repair_stuck_cells, 1);
        assert_eq!(accounting.escalated_cells(), vec![0]);
        assert!(
            !accounting.needs_anchor(),
            "one cell is below the threshold"
        );
        assert!(
            accounting.counters().repair_rounds_resent >= 5,
            "every re-emission is a repair round"
        );
    }

    #[test]
    fn ack_ring_miss_marks_everything_after_the_last_applied_as_due() {
        let mut subject = accounting(200, 200, false);
        for update in 1..=200u64 {
            subject.mark_emitted(update, &[update as u32 % 4], update);
        }
        assert_eq!(subject.ring_len(), ACK_RING_FLOOR);
        // The receiver fell silent while updates 137..=200 were emitted, then
        // acked update 100, which is below the ring: conservative re-send.
        let outcome = subject.apply_ack(&ack(100, 0), 300);
        assert!(outcome.ring_miss);
        assert_eq!(subject.counters().ack_ring_misses, 1);
        let due = subject.due_cells(300 + subject.repair_delay_ms());
        assert!(!due.is_empty(), "conservative re-send");
        // An ack inside the ring is not a miss.
        let mut fresh = accounting(200, 200, false);
        for update in 1..=10u64 {
            fresh.mark_emitted(update, &[0], update);
        }
        let outcome = fresh.apply_ack(&ack(10, 0b1), 20);
        assert!(!outcome.ring_miss);
        assert_eq!(outcome.cleared_cells, 1);
    }

    #[test]
    fn epoch_change_resets_canvas_trust() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0, 1], 0);
        assert_eq!(accounting.epoch(), 0);
        accounting.set_epoch(3);
        assert_eq!(accounting.epoch(), 3);
        assert_eq!(accounting.counters().epoch_resets, 1);
        assert!(!accounting.converged());
        let due = accounting.due_cells(0);
        assert_eq!(due.len(), 2, "everything sent is due again");
        assert_eq!(accounting.last_applied_update_id(), 0);
    }

    #[test]
    fn ack_with_a_different_epoch_is_detected() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0], 0);
        let mut stale = ack(1, 0b1);
        stale.chain_epoch = 1;
        let outcome = accounting.apply_ack(&stale, 10);
        assert!(outcome.epoch_mismatch);
        assert_eq!(accounting.epoch(), 1);
    }

    #[test]
    fn an_ack_with_an_older_epoch_never_moves_the_epoch_back() {
        // The chain epoch is the sender's and it only moves forward (MLP spec
        // §5.1). A receiver that has not seen the break yet acknowledges with
        // the older epoch; obeying it would reset the applied-update
        // watermark and re-mark every emitted cell due — a re-send storm for a
        // chain the receiver still holds.
        let mut accounting = accounting(200, 200, false);
        accounting.set_epoch(3);
        accounting.mark_emitted(1, &[0], 0);
        let mut behind = ack(1, 0b1);
        behind.chain_epoch = 2;
        let outcome = accounting.apply_ack(&behind, 10);
        assert!(outcome.epoch_stale);
        assert!(!outcome.epoch_mismatch);
        assert_eq!(accounting.epoch(), 3, "the epoch never goes back");
        assert_eq!(
            accounting.counters().epoch_resets,
            1,
            "only the raise counted"
        );
        assert_eq!(accounting.counters().epoch_stale_acks, 1);
        // The applied-update watermark survives: the chain the receiver holds
        // is the one the sender is still accounting for.
        assert_eq!(accounting.last_applied_update_id(), 1);
        assert!(
            accounting.due_cells(0).is_empty(),
            "the chain the receiver holds is the one the sender accounts for"
        );
    }

    #[test]
    fn convergence_requires_pending_empty_and_last_update_applied() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0, 1], 0);
        accounting.mark_emitted(2, &[2], 10);
        assert!(!accounting.converged());
        // Applying only update 1 leaves update 2 pending.
        accounting.apply_ack(&ack(1, 0b1), 20);
        assert!(!accounting.converged());
        assert_eq!(accounting.pending_count(), 1);
        accounting.apply_ack(&ack(2, 0b1), 30);
        assert!(accounting.converged());
        assert_eq!(accounting.last_applied_update_id(), 2);
        assert_eq!(accounting.last_sent_update_id(), 2);
    }

    #[test]
    fn re_emission_after_repair_clears_the_due_marker() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0], 0);
        assert_eq!(accounting.due_cells(500).len(), 1);
        accounting.mark_emitted(2, &[0], 500);
        assert!(accounting.due_cells(500).is_empty());
        assert_eq!(accounting.counters().repair_rounds_resent, 1);
    }

    #[test]
    fn ack_ring_scales_with_rtt_and_update_interval() {
        let mut accounting = accounting(200, 200, true);
        accounting.set_rtt_ms(2_000.0);
        accounting.set_update_interval_ms(1_000);
        assert_eq!(accounting.required_ack_ring(), 64, "floor wins");
        accounting.set_update_interval_ms(33);
        assert_eq!(accounting.required_ack_ring(), 130);
        assert_eq!(accounting.ring_len(), 0);
    }

    #[test]
    fn geometry_change_resets_everything() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0, 1], 0);
        let cells_before = accounting.grid().cells();
        let previous = accounting.set_geometry(CellGrid::with_cell_size(640, 480, 64));
        assert_eq!(previous.cells(), cells_before);
        assert_eq!(accounting.pending_count(), 0);
        assert_eq!(accounting.last_sent_update_id(), 0);
        assert_eq!(accounting.counters().epoch_resets, 1);
        // A geometry that maps to the same key is a no-op.
        let same = accounting.set_geometry(*accounting.grid());
        assert_eq!(same, *accounting.grid());
    }

    #[test]
    fn anchor_threshold_is_a_quarter_of_the_grid() {
        let accounting = accounting(1920, 1080, false);
        assert_eq!(accounting.anchor_threshold(), 510);
        let small = accounting_grid(64, 64);
        assert_eq!(small.anchor_threshold(), 16, "floor");
    }

    fn accounting_grid(width: u32, height: u32) -> CellAccounting {
        accounting(width, height, false)
    }

    #[test]
    fn stale_fraction_tracks_the_pending_share() {
        let mut accounting = accounting(200, 200, false);
        assert_eq!(accounting.stale_fraction(), 0.0);
        let all: Vec<u32> = (0..accounting.grid().cells()).collect();
        accounting.mark_emitted(1, &all, 0);
        assert_eq!(accounting.stale_fraction(), 1.0);
        assert!(accounting.needs_anchor());
    }

    #[test]
    fn convergence_property_under_random_acks() {
        // Every cell emitted is eventually acked; a converged receiver can only
        // be observed when the accounting agrees (INV-2's sender-side half).
        let mut accounting = accounting(320, 200, false);
        let grid = *accounting.grid();
        let mut now = 0u64;
        let mut update = 0u64;
        for cycle in 0..40u32 {
            let cells: Vec<u32> = (0..grid.cells())
                .filter(|index| (index + cycle) % 3 == 0)
                .collect();
            update += 1;
            accounting.mark_emitted(update, &cells, now);
            now += 40;
            let outcome = accounting.apply_ack(&ack(update, 0b1), now);
            if outcome.converged {
                assert_eq!(accounting.pending_count(), 0);
                assert!(accounting.last_applied_update_id() >= accounting.last_sent_update_id());
            }
        }
    }
}
