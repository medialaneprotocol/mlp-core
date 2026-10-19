//! Per-receiver block accounting (MLP spec §7.5, D-7): the convergence engine.
//!
//! One [`BlockAccounting`] per receiver holds the block grid, the per-block
//! `(update_id, sent_at_ms)` facts, the 64-update ack window and the update ring.
//! It answers exactly two questions for the scheduler: *which blocks are due
//! for this receiver* and *is this receiver converged*.
//!
//! All time is the session-relative millisecond clock the caller injects; the
//! module never reads a clock.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::MediaAck;

/// Maximum blocks in a grid (MLP spec §7.5.1).
pub const MAX_BLOCKS: u32 = 2_048;
/// Minimum block size in cells (MLP spec §7.5.1).
pub const MIN_BLOCK_SIZE: u32 = 32;
/// Maximum block size in cells (MLP spec §7.5.1).
pub const MAX_BLOCK_SIZE: u32 = 256;
/// Repair rounds after which a block escalates to P0 (MLP spec §7.5.3).
pub const REPAIR_MAX_ROUNDS: u8 = 4;
/// The floor of the repair delay on datagram carriers.
pub const REPAIR_DELAY_DATAGRAM_FLOOR_MS: u64 = 250;
/// The ceiling of the repair delay on datagram carriers.
pub const REPAIR_DELAY_DATAGRAM_CEIL_MS: u64 = 5_000;
/// The ceiling of the repair delay on stream carriers.
pub const REPAIR_DELAY_STREAM_CEIL_MS: u64 = 3_000;
/// The floor of the ack ring, in updates (MLP spec §7.5.2).
pub const ACK_RING_FLOOR: usize = 64;

/// A cell rectangle in surface coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockRect {
    /// Left edge in cells.
    pub x: u32,
    /// Top edge in cells.
    pub y: u32,
    /// Width in cells.
    pub w: u32,
    /// Height in cells.
    pub h: u32,
}

impl BlockRect {
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

/// The shared block grid for a geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockGrid {
    width: u32,
    height: u32,
    cell_size: u32,
    cols: u32,
    rows: u32,
}

impl BlockGrid {
    /// Build the grid with the specification's formula: the smallest power-of-two
    /// block size at least `sqrt(cells / 2048)`, clamped to `[32, 256]`.
    pub fn for_geometry(width: u32, height: u32) -> Self {
        let cells = u64::from(width) * u64::from(height);
        let target = (cells as f64 / f64::from(MAX_BLOCKS)).sqrt().ceil() as u32;
        let mut cell_size = MIN_BLOCK_SIZE;
        while cell_size < target {
            cell_size *= 2;
        }
        Self::with_block_size(
            width,
            height,
            cell_size.clamp(MIN_BLOCK_SIZE, MAX_BLOCK_SIZE),
        )
    }

    /// Build the grid with an explicit block size.
    pub fn with_block_size(width: u32, height: u32, cell_size: u32) -> Self {
        let cell_size = cell_size.clamp(MIN_BLOCK_SIZE, MAX_BLOCK_SIZE);
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

    /// Block size in cells.
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

    /// Total blocks (never above [`MAX_BLOCKS`] for a legal geometry).
    pub fn blocks(&self) -> u32 {
        self.cols * self.rows
    }

    /// A geometry fingerprint: changes when the grid changes.
    pub fn geometry_key(&self) -> u64 {
        (u64::from(self.width) << 32) ^ (u64::from(self.height) << 8) ^ u64::from(self.cell_size)
    }

    /// The block index of a `(col, row)` pair.
    pub fn index(&self, col: u32, row: u32) -> Option<u32> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        Some(row * self.cols + col)
    }

    /// The `(col, row)` of a block index.
    pub fn coords(&self, index: u32) -> Option<(u32, u32)> {
        if index >= self.blocks() {
            return None;
        }
        Some((index % self.cols, index / self.cols))
    }

    /// The cell rectangle of a block index.
    pub fn rect(&self, index: u32) -> Option<BlockRect> {
        let (col, row) = self.coords(index)?;
        let x = col * self.cell_size;
        let y = row * self.cell_size;
        let w = self.cell_size.min(self.width.saturating_sub(x));
        let h = self.cell_size.min(self.height.saturating_sub(y));
        Some(BlockRect::new(x, y, w, h))
    }

    /// The block containing a cell, when the cell is on the surface.
    pub fn block_at(&self, x_px: u32, y_px: u32) -> Option<u32> {
        if x_px >= self.width || y_px >= self.height {
            return None;
        }
        self.index(x_px / self.cell_size, y_px / self.cell_size)
    }

    /// Every block intersecting a rectangle.
    ///
    /// Clamps a rectangle that runs off-screen, so a focus rect during a
    /// geometry change cannot panic.
    pub fn blocks_in_rect(&self, rect: &BlockRect) -> Vec<u32> {
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
        let mut blocks = Vec::new();
        for row in row0..=row1 {
            for col in col0..=col1 {
                if let Some(index) = self.index(col, row) {
                    blocks.push(index);
                }
            }
        }
        blocks
    }

    /// The cell rectangle covering a set of block indices.
    pub fn bounding_rect(&self, blocks: &[u32]) -> BlockRect {
        let mut rect = BlockRect::EMPTY;
        for index in blocks {
            if let Some(block) = self.rect(*index) {
                rect = rect.union(&block);
            }
        }
        rect
    }
}

/// Why a block is due for a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DueReason {
    /// The block is pending, was sent, and the repair delay has passed.
    Repair,
    /// The block passed `repair_max_rounds` repair rounds.
    Escalated,
    /// The block is still due after `2 × repair_delay` past escalation.
    Stuck,
}

/// A block the scheduler must consider for a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DueBlock {
    /// Block index.
    pub index: u32,
    /// Why it is due.
    pub reason: DueReason,
    /// Repair rounds this block has accumulated.
    pub rounds: u8,
}

/// Counters for `mlp_*` series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BlockCounters {
    /// Acks whose update fell out of the ring (conservative re-send).
    pub ack_ring_misses: u64,
    /// Blocks that were re-sent after the repair delay.
    pub repair_rounds_resent: u64,
    /// Blocks still due after the escalation ladder.
    pub repair_stuck_blocks: u64,
    /// Blocks marked stale by a update-epoch change.
    pub epoch_resets: u64,
    /// Updates the accounting accepted from acks.
    pub applied_frames: u64,
}

/// The result of processing one ack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AckOutcome {
    /// Applied updates named by the bitmap.
    pub applied_frames: u32,
    /// Blocks removed from pending.
    pub cleared_blocks: u32,
    /// Whether the ack named a update outside the ring.
    pub ring_miss: bool,
    /// Whether the ack's epoch differs from ours.
    pub epoch_mismatch: bool,
    /// Whether the receiver is converged after this ack.
    pub converged: bool,
}

#[derive(Debug, Clone, Copy)]
struct BlockState {
    update_id: u64,
    sent_at_ms: u64,
    rounds: u8,
    due_override: bool,
    ever_sent: bool,
    /// When the block first passed `REPAIR_MAX_ROUNDS`. "Still due after
    /// `2 × repair_delay`" is measured from here, not from the last emission:
    /// a block that keeps being re-sent every repair delay is exactly the case
    /// the anchor plan exists for (MLP spec §7.5.3).
    escalated_at_ms: Option<u64>,
}

impl BlockState {
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
    blocks: Vec<u32>,
}

/// Accounting configuration; every value is a policy constant of §6.5.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlockAccountingConfig {
    /// Whether the carrier is datagram-class (selects the repair delay rule).
    pub datagram: bool,
    /// The update interval implied by the target fps.
    pub update_interval_ms: u64,
    /// The RTT smoothing used for the repair delay.
    pub rtt_ms: f64,
    /// The number of updates kept for ack resolution.
    pub ack_ring: usize,
}

impl Default for BlockAccountingConfig {
    fn default() -> Self {
        Self {
            datagram: false,
            update_interval_ms: 33,
            rtt_ms: 50.0,
            ack_ring: ACK_RING_FLOOR,
        }
    }
}

/// One receiver's block accounting.
#[derive(Debug, Clone)]
pub struct BlockAccounting {
    grid: BlockGrid,
    config: BlockAccountingConfig,
    pending: Vec<BlockState>,
    /// Whether a block was ever emitted in this epoch. An epoch change makes
    /// every such block suspect again (MLP spec §6.1), even when its ack had
    /// already cleared it.
    emitted_any: Vec<bool>,
    ring: VecDeque<UpdateRecord>,
    epoch: u32,
    last_applied_frame_id: u64,
    last_sent_frame_id: u64,
    last_ack_at_ms: Option<u64>,
    counters: BlockCounters,
    stuck_blocks: Vec<u32>,
}

impl BlockAccounting {
    /// Create accounting for a grid with no emitted history.
    pub fn new(grid: BlockGrid, config: BlockAccountingConfig) -> Self {
        Self {
            grid,
            config,
            pending: vec![BlockState::fresh(); grid.blocks() as usize],
            emitted_any: vec![false; grid.blocks() as usize],
            ring: VecDeque::new(),
            epoch: 0,
            last_applied_frame_id: 0,
            last_sent_frame_id: 0,
            last_ack_at_ms: None,
            counters: BlockCounters::default(),
            stuck_blocks: Vec::new(),
        }
    }

    /// The grid in force.
    pub fn grid(&self) -> &BlockGrid {
        &self.grid
    }

    /// Change the geometry: every block becomes stale (canvas trust resets).
    ///
    /// Returns the previous grid so the caller can count the change.
    pub fn set_geometry(&mut self, grid: BlockGrid) -> BlockGrid {
        let previous = self.grid;
        if previous.geometry_key() == grid.geometry_key() {
            return previous;
        }
        self.grid = grid;
        self.pending = vec![BlockState::fresh(); grid.blocks() as usize];
        self.emitted_any = vec![false; grid.blocks() as usize];
        self.ring.clear();
        self.last_applied_frame_id = 0;
        self.last_sent_frame_id = 0;
        self.stuck_blocks.clear();
        self.counters.epoch_resets += 1;
        previous
    }

    /// The chain epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Set the chain epoch. A change resets canvas trust (MLP spec §6.1).
    ///
    /// Every block emitted in the previous epoch becomes due again — the
    /// receiver's chain may be from a different capture — but nothing is
    /// *cleared*: the cells stay until fresh ones arrive.
    pub fn set_epoch(&mut self, epoch: u32) {
        if epoch == self.epoch {
            return;
        }
        self.epoch = epoch;
        for (index, state) in self.pending.iter_mut().enumerate() {
            if self.emitted_any[index] {
                *state = BlockState {
                    update_id: 0,
                    sent_at_ms: 0,
                    rounds: 0,
                    due_override: true,
                    ever_sent: true,
                    escalated_at_ms: None,
                };
            }
        }
        self.last_applied_frame_id = 0;
        self.counters.epoch_resets += 1;
    }

    /// Update the RTT that drives the repair delay.
    pub fn set_rtt_ms(&mut self, rtt_ms: f64) {
        self.config.rtt_ms = rtt_ms.max(1.0);
    }

    /// Update the update interval that drives the repair delay and ring.
    pub fn set_frame_interval_ms(&mut self, interval_ms: u64) {
        self.config.update_interval_ms = interval_ms.max(1);
        let ring = self.required_ack_ring();
        self.config.ack_ring = ring;
        while self.ring.len() > ring {
            self.ring.pop_front();
        }
    }

    /// Record that `update_id` was emitted for this receiver.
    ///
    /// `blocks` are the block indices the update's regions cover; `E` advances
    /// exactly at these blocks (MLP spec §7.3.3).
    pub fn mark_emitted(&mut self, update_id: u64, blocks: &[u32], now_ms: u64) {
        let blocks: Vec<u32> = blocks
            .iter()
            .copied()
            .filter(|index| *index < self.grid.blocks())
            .collect();
        for index in &blocks {
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
        self.last_sent_frame_id = self.last_sent_frame_id.max(update_id);
        self.ring.push_back(UpdateRecord { update_id, blocks });
        let ring = self.required_ack_ring();
        self.config.ack_ring = ring;
        while self.ring.len() > ring {
            self.ring.pop_front();
        }
    }

    /// Apply one ack (MLP spec §7.5.2).
    pub fn apply_ack(&mut self, ack: &MediaAck, now_ms: u64) -> AckOutcome {
        let mut outcome = AckOutcome::default();
        if ack.chain_epoch != self.epoch {
            outcome.epoch_mismatch = true;
            self.set_epoch(ack.chain_epoch);
        }
        self.last_ack_at_ms = Some(now_ms);
        // An ack can never name a update we did not send; clamping keeps a
        // bogus (or cross-session) ack from clearing unemitted history.
        let effective = ack.highest_applied_update_id.min(self.last_sent_frame_id);
        let applied: Vec<u64> = ack
            .applied_frames()
            .filter(|update| *update <= self.last_sent_frame_id)
            .collect();
        outcome.applied_frames = applied.len() as u32;
        self.counters.applied_frames += u64::from(outcome.applied_frames);

        // A update outside the ring means the sender cannot reconstruct the
        // coverage: mark every block emitted after the last applied id as due
        // (MLP spec §7.5.2). Correct, at the cost of re-sending.
        if effective > self.last_applied_frame_id {
            let ring_covers = self
                .ring
                .front()
                .map(|record| record.update_id <= effective)
                .unwrap_or(false);
            if !ring_covers {
                outcome.ring_miss = true;
                self.counters.ack_ring_misses += 1;
                for state in &mut self.pending {
                    if state.ever_sent && state.update_id > self.last_applied_frame_id {
                        state.due_override = true;
                    }
                }
            }
        }

        // A block is acked exactly when the update that last emitted it is
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
                .map(|record| record.blocks.clone())
            else {
                // Outside the ring: nothing can be cleared safely.
                continue;
            };
            for block in covered {
                let state = &mut self.pending[block as usize];
                if state.ever_sent && state.update_id == update {
                    *state = BlockState::fresh();
                    outcome.cleared_blocks += 1;
                }
            }
        }
        if effective > self.last_applied_frame_id {
            self.last_applied_frame_id = effective;
        }
        // The stuck set is recomputed from the current truth.
        self.stuck_blocks
            .retain(|index| self.pending[*index as usize].ever_sent);
        outcome.converged = self.converged();
        outcome
    }

    /// The repair delay in ms (MLP spec §7.5.3).
    pub fn repair_delay_ms(&self) -> u64 {
        if self.config.datagram {
            (2.0 * self.config.rtt_ms).round().clamp(
                REPAIR_DELAY_DATAGRAM_FLOOR_MS as f64,
                REPAIR_DELAY_DATAGRAM_CEIL_MS as f64,
            ) as u64
        } else {
            (self.config.rtt_ms + self.config.update_interval_ms as f64)
                .round()
                .clamp(
                    REPAIR_DELAY_DATAGRAM_FLOOR_MS as f64,
                    REPAIR_DELAY_STREAM_CEIL_MS as f64,
                ) as u64
        }
    }

    /// Blocks that are due for this receiver, ordered `(reason, index)`.
    ///
    /// Fresh changes and background refresh are *not* represented here: they
    /// come from the shared capture diff, not from this receiver's history.
    pub fn due_blocks(&self, now_ms: u64) -> Vec<DueBlock> {
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
            due.push(DueBlock {
                index: index as u32,
                reason,
                rounds: state.rounds,
            });
        }
        due.sort_by_key(|block| (block.reason, block.index));
        due
    }

    /// Recompute and return the blocks that are stuck after the ladder.
    ///
    /// The list is what an anchor plan must cover (MLP spec §7.5.3, §6.6).
    pub fn note_stuck(&mut self, now_ms: u64) -> Vec<u32> {
        let repair_delay = self.repair_delay_ms();
        let mut stuck = Vec::new();
        for (index, state) in self.pending.iter().enumerate() {
            if !state.ever_sent || state.rounds <= REPAIR_MAX_ROUNDS {
                continue;
            }
            // Still unacked `2 × repair_delay` after the escalation: the repair
            // loop is not converging, so the block belongs in an anchor plan.
            let escalated = state.escalated_at_ms.unwrap_or(state.sent_at_ms);
            if now_ms.saturating_sub(escalated) >= 2 * repair_delay {
                stuck.push(index as u32);
            }
        }
        stuck.sort_unstable();
        if stuck.len() > self.stuck_blocks.len() {
            self.counters.repair_stuck_blocks += (stuck.len() - self.stuck_blocks.len()) as u64;
        }
        self.stuck_blocks = stuck.clone();
        stuck
    }

    /// Blocks the ladder escalated to P0.
    pub fn escalated_blocks(&self) -> Vec<u32> {
        self.pending
            .iter()
            .enumerate()
            .filter(|(_, state)| state.ever_sent && state.rounds > REPAIR_MAX_ROUNDS)
            .map(|(index, _)| index as u32)
            .collect()
    }

    /// Blocks that are pending (sent and not acked).
    pub fn pending_blocks(&self) -> Vec<u32> {
        self.pending
            .iter()
            .enumerate()
            .filter(|(_, state)| state.ever_sent)
            .map(|(index, _)| index as u32)
            .collect()
    }

    /// The number of pending blocks.
    pub fn pending_count(&self) -> u32 {
        self.pending.iter().filter(|state| state.ever_sent).count() as u32
    }

    /// The pending fraction, for `mlp_stale_fraction`.
    pub fn stale_fraction(&self) -> f64 {
        f64::from(self.pending_count()) / f64::from(self.grid.blocks().max(1))
    }

    /// Convergence: `pending = ∅` and the last update is applied.
    pub fn converged(&self) -> bool {
        self.pending.iter().all(|state| !state.ever_sent)
            && self.last_applied_frame_id >= self.last_sent_frame_id
    }

    /// The last update id the receiver acked as applied.
    pub fn last_applied_frame_id(&self) -> u64 {
        self.last_applied_frame_id
    }

    /// The last update id this sender emitted for the receiver.
    pub fn last_sent_frame_id(&self) -> u64 {
        self.last_sent_frame_id
    }

    /// The last ack's session time.
    pub fn last_ack_at_ms(&self) -> Option<u64> {
        self.last_ack_at_ms
    }

    /// The number of update records in the ring.
    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// The ring size this RTT and update interval require (MLP spec §7.5.2).
    pub fn required_ack_ring(&self) -> usize {
        let rtt = self.config.rtt_ms;
        let interval = self.config.update_interval_ms.max(1) as f64;
        let needed = (2.0 * rtt / interval).ceil() as usize + 8;
        needed.max(ACK_RING_FLOOR)
    }

    /// Counters for metrics.
    pub fn counters(&self) -> BlockCounters {
        self.counters
    }

    /// The anchor threshold for this grid: `max(16, blocks / 4)`.
    pub fn anchor_threshold(&self) -> u32 {
        (self.grid.blocks() / 4).max(16)
    }

    /// Whether the pending set has crossed the anchor threshold.
    pub fn needs_anchor(&self) -> bool {
        self.pending_count() > self.anchor_threshold()
    }

    /// Update ids currently in the ring, oldest first.
    pub fn ring_frames(&self) -> impl Iterator<Item = u64> + '_ {
        self.ring.iter().map(|record| record.update_id)
    }

    /// The blocks the ring attributes to a update id, when known.
    pub fn update_blocks(&self, update_id: u64) -> Option<&[u32]> {
        self.ring
            .iter()
            .find(|record| record.update_id == update_id)
            .map(|record| record.blocks.as_slice())
    }

    /// Forget everything (park eviction, fresh media session).
    pub fn reset(&mut self) {
        for state in &mut self.pending {
            *state = BlockState::fresh();
        }
        self.emitted_any.iter_mut().for_each(|flag| *flag = false);
        self.ring.clear();
        self.last_applied_frame_id = 0;
        self.last_sent_frame_id = 0;
        self.last_ack_at_ms = None;
        self.stuck_blocks.clear();
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

    fn accounting(width: u32, height: u32, datagram: bool) -> BlockAccounting {
        let grid = BlockGrid::for_geometry(width, height);
        BlockAccounting::new(
            grid,
            BlockAccountingConfig {
                datagram,
                update_interval_ms: 33,
                rtt_ms: 150.0,
                ack_ring: ACK_RING_FLOOR,
            },
        )
    }

    #[test]
    fn grid_formula_matches_the_design() {
        // 1080p: sqrt(2 073 600 / 2 048) ≈ 31.8 ⇒ 32 px, 60 × 34 = 2040 blocks.
        let grid = BlockGrid::for_geometry(1920, 1080);
        assert_eq!(grid.cell_size(), 32);
        assert_eq!(grid.cols(), 60);
        assert_eq!(grid.rows(), 34);
        assert_eq!(grid.blocks(), 2040);
        assert!(grid.blocks() <= MAX_BLOCKS);
        // The design's documented example grid: 64 px ⇒ 510 blocks at 1080p.
        let coarse = BlockGrid::with_block_size(1920, 1080, 64);
        assert_eq!(coarse.blocks(), 510);
    }

    #[test]
    fn grid_formula_clamps_to_32_and_256() {
        let tiny = BlockGrid::for_geometry(64, 64);
        assert_eq!(tiny.cell_size(), MIN_BLOCK_SIZE);
        let big = BlockGrid::for_geometry(4_096, 4_096);
        assert_eq!(big.cell_size(), 128);
        assert!(big.blocks() <= MAX_BLOCKS);
        // The 256 px clamp is the hard bound: a geometry that would need
        // larger blocks saturates it (and the 2048-block budget with it, so
        // the driver must downscale before encoding).
        let beyond = BlockGrid::for_geometry(12_288, 12_288);
        assert_eq!(beyond.cell_size(), MAX_BLOCK_SIZE);
        assert!(beyond.blocks() > MAX_BLOCKS, "documented saturation");
    }

    #[test]
    fn grid_rect_and_block_mapping_round_trip() {
        let grid = BlockGrid::with_block_size(100, 60, 32);
        assert_eq!(grid.cols(), 4);
        assert_eq!(grid.rows(), 2);
        assert_eq!(grid.blocks(), 8);
        assert_eq!(grid.index(3, 1), Some(7));
        assert_eq!(grid.coords(7), Some((3, 1)));
        assert_eq!(grid.rect(7), Some(BlockRect::new(96, 32, 4, 28)));
        assert_eq!(grid.block_at(0, 0), Some(0));
        assert_eq!(grid.block_at(99, 59), Some(7));
        assert_eq!(grid.block_at(200, 10), None);
        assert!(grid.rect(8).is_none());
    }

    #[test]
    fn blocks_in_rect_covers_the_rectangle() {
        let grid = BlockGrid::with_block_size(128, 128, 32);
        let blocks = grid.blocks_in_rect(&BlockRect::new(0, 0, 64, 64));
        assert_eq!(blocks, vec![0, 1, 4, 5]);
        let clamped = grid.blocks_in_rect(&BlockRect::new(120, 120, 500, 500));
        assert_eq!(clamped, vec![15]);
        assert!(grid.blocks_in_rect(&BlockRect::EMPTY).is_empty());
        assert_eq!(grid.bounding_rect(&[0, 5]), BlockRect::new(0, 0, 64, 64));
    }

    #[test]
    fn emitted_blocks_become_pending_and_an_ack_clears_them() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0, 1, 2], 0);
        assert_eq!(accounting.pending_count(), 3);
        assert!(!accounting.converged());
        let outcome = accounting.apply_ack(&ack(1, 0b1), 10);
        assert_eq!(outcome.cleared_blocks, 3);
        assert!(outcome.converged);
        assert!(accounting.converged());
    }

    #[test]
    fn unacked_blocks_become_due_after_the_repair_delay() {
        let mut accounting = accounting(200, 200, false);
        // Stream repair delay = clamp(rtt + update_interval, 250, 3000) = 250.
        assert_eq!(accounting.repair_delay_ms(), 250);
        accounting.mark_emitted(1, &[0, 1], 0);
        assert!(accounting.due_blocks(100).is_empty(), "not due yet");
        let due = accounting.due_blocks(300);
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
    fn stream_repair_delay_is_rtt_plus_frame_interval() {
        let mut accounting = accounting(200, 200, false);
        accounting.set_rtt_ms(1_000.0);
        accounting.set_frame_interval_ms(200);
        assert_eq!(accounting.repair_delay_ms(), 1_200);
        accounting.set_rtt_ms(10_000.0);
        assert_eq!(accounting.repair_delay_ms(), 3_000, "ceiling");
    }

    #[test]
    fn repair_ladder_escalates_then_reports_stuck_blocks() {
        let mut accounting = accounting(200, 200, false);
        let mut now = 0u64;
        accounting.mark_emitted(1, &[0], now);
        for round in 2..=6 {
            now += 300;
            // The block is due each round and is re-emitted: the ladder counts.
            accounting.mark_emitted(round, &[0], now);
        }
        let due = accounting.due_blocks(now + 400);
        assert_eq!(due.len(), 1);
        assert!(matches!(
            due[0].reason,
            DueReason::Escalated | DueReason::Stuck
        ));
        assert!(due[0].rounds > REPAIR_MAX_ROUNDS);
        let stuck = accounting.note_stuck(now + 2 * accounting.repair_delay_ms() + 1);
        assert_eq!(stuck, vec![0]);
        assert_eq!(accounting.counters().repair_stuck_blocks, 1);
        assert_eq!(accounting.escalated_blocks(), vec![0]);
        assert!(
            !accounting.needs_anchor(),
            "one block is below the threshold"
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
        let due = subject.due_blocks(300 + subject.repair_delay_ms());
        assert!(!due.is_empty(), "conservative re-send");
        // An ack inside the ring is not a miss.
        let mut fresh = accounting(200, 200, false);
        for update in 1..=10u64 {
            fresh.mark_emitted(update, &[0], update);
        }
        let outcome = fresh.apply_ack(&ack(10, 0b1), 20);
        assert!(!outcome.ring_miss);
        assert_eq!(outcome.cleared_blocks, 1);
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
        let due = accounting.due_blocks(0);
        assert_eq!(due.len(), 2, "everything sent is due again");
        assert_eq!(accounting.last_applied_frame_id(), 0);
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
    fn convergence_requires_pending_empty_and_last_frame_applied() {
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
        assert_eq!(accounting.last_applied_frame_id(), 2);
        assert_eq!(accounting.last_sent_frame_id(), 2);
    }

    #[test]
    fn re_emission_after_repair_clears_the_due_marker() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0], 0);
        assert_eq!(accounting.due_blocks(500).len(), 1);
        accounting.mark_emitted(2, &[0], 500);
        assert!(accounting.due_blocks(500).is_empty());
        assert_eq!(accounting.counters().repair_rounds_resent, 1);
    }

    #[test]
    fn ack_ring_scales_with_rtt_and_frame_interval() {
        let mut accounting = accounting(200, 200, true);
        accounting.set_rtt_ms(2_000.0);
        accounting.set_frame_interval_ms(1_000);
        assert_eq!(accounting.required_ack_ring(), 64, "floor wins");
        accounting.set_frame_interval_ms(33);
        assert_eq!(accounting.required_ack_ring(), 130);
        assert_eq!(accounting.ring_len(), 0);
    }

    #[test]
    fn geometry_change_resets_everything() {
        let mut accounting = accounting(200, 200, false);
        accounting.mark_emitted(1, &[0, 1], 0);
        let blocks_before = accounting.grid().blocks();
        let previous = accounting.set_geometry(BlockGrid::with_block_size(640, 480, 64));
        assert_eq!(previous.blocks(), blocks_before);
        assert_eq!(accounting.pending_count(), 0);
        assert_eq!(accounting.last_sent_frame_id(), 0);
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

    fn accounting_grid(width: u32, height: u32) -> BlockAccounting {
        accounting(width, height, false)
    }

    #[test]
    fn stale_fraction_tracks_the_pending_share() {
        let mut accounting = accounting(200, 200, false);
        assert_eq!(accounting.stale_fraction(), 0.0);
        let all: Vec<u32> = (0..accounting.grid().blocks()).collect();
        accounting.mark_emitted(1, &all, 0);
        assert_eq!(accounting.stale_fraction(), 1.0);
        assert!(accounting.needs_anchor());
    }

    #[test]
    fn convergence_property_under_random_acks() {
        // Every block emitted is eventually acked; a converged receiver can only
        // be observed when the accounting agrees (INV-2's sender-side half).
        let mut accounting = accounting(320, 200, false);
        let grid = *accounting.grid();
        let mut now = 0u64;
        let mut update = 0u64;
        for cycle in 0..40u32 {
            let blocks: Vec<u32> = (0..grid.blocks())
                .filter(|index| (index + cycle) % 3 == 0)
                .collect();
            update += 1;
            accounting.mark_emitted(update, &blocks, now);
            now += 40;
            let outcome = accounting.apply_ack(&ack(update, 0b1), now);
            if outcome.converged {
                assert_eq!(accounting.pending_count(), 0);
                assert!(accounting.last_applied_frame_id() >= accounting.last_sent_frame_id());
            }
        }
    }
}
