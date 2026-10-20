//! Scheduling, frame timing and anchor plans (design §7.3, §7.6, D-19).
//!
//! Everything the scheduler decides must be a function of injected inputs:
//! these tests pin the priority bands, the budget truncation, the input pulse,
//! the idle cadence and the plan ladder.

use std::time::Duration;

use mlp_core::media::cells::{CellGrid, CellRect, DueCell, DueReason};
use mlp_core::media::scheduler::{
    DEFAULT_CELL_BYTES, IDLE_FRAME_INTERVAL_CEILING_MS, IDLE_FRAME_INTERVAL_FLOOR_MS, PlanConfig,
    PlanManager, PlanStartReason, PriorityBand, ScheduleRequest, Scheduler, SchedulerConfig,
    UpdateBudget, UpdateKind, UpdateTimer,
};

fn grid() -> CellGrid {
    CellGrid::with_cell_size(1280, 720, 64)
}

fn due(index: u32, reason: DueReason, rounds: u8) -> DueCell {
    DueCell {
        index,
        reason,
        rounds,
    }
}

fn request<'a>(
    due_cells: &'a [DueCell],
    fresh: &'a [u32],
    refresh: &'a [u32],
    budget: UpdateBudget,
) -> ScheduleRequest<'a> {
    ScheduleRequest {
        due: due_cells,
        fresh,
        refresh,
        budget,
        anchor: false,
    }
}

fn roomy() -> UpdateBudget {
    UpdateBudget {
        bytes: 1_048_576,
        max_object_bytes: 524_288,
        ..UpdateBudget::default()
    }
}

#[test]
fn blocks_are_ordered_p0_p1_p2_p3() {
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    let due_cells = [
        due(50, DueReason::Repair, 1),
        due(51, DueReason::Escalated, 5),
        due(52, DueReason::Stuck, 9),
    ];
    let fresh = [60, 61];
    let refresh = [70];
    scheduler.mark_focus(CellRect::new(0, 0, 64, 64));
    let decision = scheduler.schedule(request(&due_cells, &fresh, &refresh, roomy()), &grid(), 0);
    // Focus (P0, block 0) → escalated/stuck (P0) → repair (P1) → fresh (P2) →
    // refresh (P3), each band internally by block index.
    assert_eq!(decision.cells, vec![0, 51, 52, 50, 60, 61, 70]);
    assert_eq!(decision.band, PriorityBand::P0);
    assert_eq!(decision.kind, UpdateKind::Repair);
    assert_eq!(PriorityBand::P0.mlp_priority(), 6);
    assert_eq!(PriorityBand::P1.mlp_priority(), 6);
    assert_eq!(PriorityBand::P2.mlp_priority(), 7);
    assert_eq!(PriorityBand::P3.mlp_priority(), 7);
}

#[test]
fn the_frame_budget_truncates_and_counts_the_deferred_work() {
    let grid = grid();
    let config = SchedulerConfig {
        cell_bytes: 2_048,
        ..SchedulerConfig::default()
    };
    let mut scheduler = Scheduler::new(config, PlanConfig::default());
    let budget = UpdateBudget {
        bytes: 8_192,
        ..UpdateBudget::default()
    };
    let fresh: Vec<u32> = (0..40).collect();
    let decision = scheduler.schedule(request(&[], &fresh, &[], budget), &grid, 0);
    assert_eq!(decision.cells.len(), 4, "8 192 / 2 048");
    assert_eq!(decision.estimated_bytes, 8_192);
    assert!(decision.estimated_bytes <= budget.bytes);
    assert_eq!(decision.deferred, 36);
    assert_eq!(scheduler.deferred_total(), 36);
}

#[test]
fn the_default_block_estimate_always_lets_the_floor_budget_make_progress() {
    // A monotonically shrinking estimate would starve the loop; the default is
    // chosen so the 8 kbps floor budget still carries one block.
    let grid = grid();
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    let floor_budget = UpdateBudget {
        bytes: 1_000,
        max_object_bytes: 1_024,
        ..UpdateBudget::default()
    };
    let fresh: Vec<u32> = (0..10).collect();
    let decision = scheduler.schedule(request(&[], &fresh, &[], floor_budget), &grid, 0);
    assert_eq!(decision.cells.len(), 1);
    assert!(DEFAULT_CELL_BYTES <= 1_000);
}

#[test]
fn an_input_pulse_raises_the_frame_rate_and_has_its_own_budget() {
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    let grid = grid();
    let now = 1_000u64;
    assert!(!scheduler.timer().pulse_active(now));
    scheduler.request_pulse(now, 400.0);
    assert!(scheduler.timer().pulse_active(now + 100));
    assert_eq!(
        scheduler.timer().pulse_active(now + 401),
        false,
        "the window is max(300 ms, 1 × RTT)"
    );
    assert_eq!(scheduler.timer().pulse_active(now + 400), false);
    assert_eq!(scheduler.pulse_fps(10.0), 20.0);
    assert_eq!(scheduler.pulse_fps(30.0), 30.0);
    assert_eq!(scheduler.pulse_fps(1.0), 4.0);
    let budget = scheduler.pulse_budget_bytes(4_096, 32_000);
    assert_eq!(budget, 1_024, "a quarter second of the pace, floored");
    let capped = scheduler.pulse_budget_bytes(4_096, 1_000_000);
    assert_eq!(capped, 4_096, "never above the frame budget");
    let _ = grid;
}

#[test]
fn the_frame_timer_gates_the_rate_and_the_pulse_bypasses_it() {
    let mut timer = UpdateTimer::new();
    assert!(timer.due(0, 10.0));
    timer.on_frame(0);
    assert!(!timer.due(50, 10.0), "10 fps = 100 ms");
    assert!(timer.due(100, 10.0));
    timer.pulse(100, 300);
    assert!(timer.due(110, 10.0), "a pulse bypasses the fps gate");
    timer.expire_pulse(500);
    assert!(!timer.pulse_active(500));
    assert_eq!(timer.last_frame_ms(), Some(0));
}

#[test]
fn idle_frames_are_clamped_between_one_and_three_seconds() {
    assert_eq!(IDLE_FRAME_INTERVAL_FLOOR_MS, 1_000);
    assert_eq!(IDLE_FRAME_INTERVAL_CEILING_MS, 3_000);
    assert_eq!(UpdateTimer::idle_interval_ms(10.0), 1_000);
    assert_eq!(UpdateTimer::idle_interval_ms(1_500.0), 3_000);
    assert_eq!(UpdateTimer::idle_interval_ms(2_400.0), 3_000);
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    assert!(scheduler.idle_due(0, 100.0));
    scheduler.on_idle(0);
    assert!(!scheduler.idle_due(999, 100.0));
    assert!(scheduler.idle_due(1_000, 100.0));
}

#[test]
fn chunk_limits_follow_the_carrier_and_the_loss_ratio() {
    let scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    assert_eq!(
        scheduler.max_chunks_for(false, 0.9),
        8,
        "stream carriers: 8"
    );
    assert_eq!(scheduler.max_chunks_for(true, 0.0), 4, "datagram: 4");
    assert_eq!(
        scheduler.max_chunks_for(true, 0.06),
        1,
        "datagram above 5 % loss: 1 (D-19)"
    );
    let budget = UpdateBudget {
        bytes: 65_536,
        max_chunks: 4,
        max_chunk_wire_bytes: 65_536,
        path_chunk_cap: 65_536,
        ..UpdateBudget::default()
    };
    assert_eq!(budget.part_count(1), Some(1));
    assert_eq!(budget.part_count(65_536), Some(1));
    assert_eq!(budget.part_count(65_537), Some(2));
    assert_eq!(budget.part_count(262_144), Some(4));
    assert_eq!(
        budget.part_count(262_145),
        None,
        "the frame must be decomposed, never oversized (§7.3.4)"
    );
    assert_eq!(budget.payload_ceiling(), 262_144);
}

#[test]
fn plans_band_the_block_list_within_the_budget() {
    let mut plans = PlanManager::new(PlanConfig::default());
    let cells: Vec<u32> = (0..100).collect();
    let plan_id = plans
        .start(PlanStartReason::Pending, cells, 0)
        .expect("a plan starts");
    assert_eq!(plans.active().map(|(id, ..)| id), Some(plan_id));
    assert!(plans.is_active());
    let band = plans.next_band(4_096, 2_048).expect("first band");
    assert_eq!(band.cells, vec![0, 1]);
    assert!(band.anchor);
    assert_eq!(band.remaining_after, 98);
    assert_eq!(band.priority, 6);
    assert_eq!(band.updates_emitted, 1);
    let last = plans.next_band(1_000_000, 2_048).expect("the rest");
    assert_eq!(last.cells.len(), 98);
    assert_eq!(last.remaining_after, 0);
    assert!(!plans.is_active(), "the plan completes when drained");
    let (started, completed, overdue) = plans.counters();
    assert_eq!((started, completed, overdue), (1, 1, 0));
    let _ = plan_id;
}

#[test]
fn plans_are_rate_limited_per_viewer_not_globally() {
    let mut first = PlanManager::new(PlanConfig::default());
    let mut second = PlanManager::new(PlanConfig::default());
    assert!(
        first
            .start(PlanStartReason::Pending, vec![1, 2], 1_000)
            .is_some()
    );
    assert!(
        first
            .start(PlanStartReason::Stuck, vec![3], 1_100)
            .is_none(),
        "250 ms per receiver"
    );
    assert!(
        second
            .start(PlanStartReason::Stuck, vec![9], 1_100)
            .is_some(),
        "another receiver is not rate-limited"
    );
    assert!(
        first
            .start(PlanStartReason::Stuck, vec![3], 1_250)
            .is_some()
    );
}

#[test]
fn the_anchor_threshold_is_a_quarter_of_the_grid_with_a_floor() {
    let config = PlanConfig {
        anchor_threshold: (grid().cells() / 4).max(16),
        ..PlanConfig::default()
    };
    let plans = PlanManager::new(config);
    let threshold = grid().cells() / 4;
    assert!(!plans.should_start_for_pending(threshold, 0, 0));
    assert!(plans.should_start_for_pending(threshold + 1, 0, 0));
    assert!(plans.should_start_for_pending(0, 1, 0), "a stuck block");
}

#[test]
fn an_overdue_plan_restarts_from_the_newest_state() {
    let mut plans = PlanManager::new(PlanConfig {
        rtt_ms: 2_000.0,
        ..PlanConfig::default()
    });
    assert_eq!(plans.config().deadline_ms(), 34_000);
    let plan_id = plans
        .start(PlanStartReason::Pending, (0..50).collect(), 0)
        .expect("a plan");
    assert_eq!(plans.poll_overdue(33_999), None);
    assert_eq!(plans.poll_overdue(34_001), Some(plan_id));
    assert_eq!(plans.counters().2, 1, "plan_overdue_total");
    // A restart is not rate-limited: freshness beats completeness.
    assert!(
        plans
            .start(PlanStartReason::Pending, vec![1, 2, 3], 34_001)
            .is_some()
    );
}

#[test]
fn plan_bands_win_over_fresh_work() {
    let grid = grid();
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    let plan_id = scheduler
        .plans_mut()
        .start(PlanStartReason::Resume, vec![5, 6, 7], 0)
        .expect("a plan");
    let fresh = [100, 101];
    let decision = scheduler.schedule(request(&[], &fresh, &[], roomy()), &grid, 0);
    assert_eq!(decision.plan_id, Some(plan_id));
    assert_eq!(decision.cells, vec![5, 6, 7]);
    assert!(decision.anchor);
    assert_eq!(decision.kind, UpdateKind::Anchor);
    // The next schedule call falls through to normal work.
    let decision = scheduler.schedule(request(&[], &fresh, &[], roomy()), &grid, 10);
    assert_eq!(decision.plan_id, None);
    assert_eq!(decision.cells, vec![100, 101]);
}

#[test]
fn an_empty_decision_is_an_idle_frame_and_counts_as_one() {
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    let decision = scheduler.schedule(request(&[], &[], &[], roomy()), &grid(), 0);
    assert!(decision.is_empty());
    assert_eq!(decision.kind, UpdateKind::Idle);
    assert_eq!(scheduler.updates_by_kind()[2], 1);
    assert_eq!(UpdateKind::Idle.as_str(), "idle");
}

#[test]
fn frame_kinds_map_to_the_documented_labels() {
    assert_eq!(UpdateKind::Delta.as_str(), "delta");
    assert_eq!(UpdateKind::Anchor.as_str(), "anchor");
    assert_eq!(UpdateKind::Pulse.as_str(), "pulse");
    assert_eq!(UpdateKind::Repair.as_str(), "repair");
    assert_eq!(PlanStartReason::Pending.as_str(), "pending");
    assert_eq!(PlanStartReason::Epoch.as_str(), "epoch");
    assert_eq!(PlanStartReason::Geometry.as_str(), "geometry");
    assert_eq!(PlanStartReason::Stuck.as_str(), "stuck");
    assert_eq!(PlanStartReason::Resume.as_str(), "resume");
}

#[test]
fn the_focus_rect_is_consumed_by_the_frame_that_carried_it() {
    let mut scheduler = Scheduler::new(SchedulerConfig::default(), PlanConfig::default());
    let grid = grid();
    scheduler.mark_focus(CellRect::new(0, 0, 128, 128));
    scheduler.mark_focus(CellRect::new(256, 0, 64, 64));
    let cells = scheduler.focus_cells(&grid);
    // The union bounding box covers both rectangles.
    assert!(cells.contains(&0));
    assert!(cells.contains(&4));
    assert!(cells.len() >= 6);
    let fresh = [500u32];
    let decision = scheduler.schedule(request(&[], &fresh, &[], roomy()), &grid, 0);
    assert!(decision.cells.contains(&0));
    assert!(scheduler.focus().is_none());
}

#[test]
fn every_policy_knob_comes_from_configuration() {
    // The scheduler is the only place a threshold lives: changing the config
    // changes the behaviour without touching any other module.
    let grid = grid();
    let strict = SchedulerConfig {
        cell_bytes: 8_192,
        max_chunks_datagram: 2,
        single_chunk_loss_ratio: 0.01,
        idle_interval_floor_ms: 2_000,
        ..SchedulerConfig::default()
    };
    let mut scheduler = Scheduler::new(strict, PlanConfig::default());
    let budget = UpdateBudget {
        bytes: 16_384,
        max_chunks: 10,
        ..UpdateBudget::default()
    };
    let fresh: Vec<u32> = (0..10).collect();
    let decision = scheduler.schedule(request(&[], &fresh, &[], budget), &grid, 0);
    // The configured estimate governs both the block count and the frame's
    // estimated size, and the frame never exceeds the budget.
    assert!(!decision.cells.is_empty());
    assert_eq!(
        decision.estimated_bytes,
        decision.cells.len() as u32 * 8_192,
        "the configured block estimate is what truncates"
    );
    assert!(
        decision.estimated_bytes <= budget.bytes,
        "{} > {}",
        decision.estimated_bytes,
        budget.bytes
    );
    assert_eq!(
        scheduler.max_chunks_for(true, 0.005),
        2,
        "the configured datagram cap below the loss threshold"
    );
    assert_eq!(
        scheduler.max_chunks_for(true, 0.02),
        1,
        "above the configured loss threshold a datagram frame is one chunk"
    );
    // The floor is a config knob, not a literal: 2 000 ms means the first
    // second of a fresh scheduler is still not idle-due.
    assert!(scheduler.idle_due(0, 50.0), "no idle frame yet");
    scheduler.on_idle(0);
    assert!(!scheduler.idle_due(1_000, 50.0));
    assert!(scheduler.idle_due(2_000, 50.0));
}

#[test]
fn plan_config_deadline_is_rtt_relative() {
    for rtt_ms in [10.0f64, 150.0, 400.0, 2_000.0] {
        let config = PlanConfig {
            rtt_ms,
            ..PlanConfig::default()
        };
        assert_eq!(
            config.deadline_ms(),
            (2.0 * rtt_ms).round() as u64 + 30_000,
            "rtt {rtt_ms}"
        );
        assert!(Duration::from_millis(config.deadline_ms()) >= Duration::from_secs(30));
    }
}
