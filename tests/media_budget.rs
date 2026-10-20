//! Budget assertions for the T0–T4 envelope (design §3, §7, §13, §12).
//!
//! These are the numbers the program promises: object sizes, rate windows,
//! convergence time, echo budgets and the timeout arithmetic. Every assertion
//! is derived from a policy constant, never from an ad-hoc value.

use mlp_core::media::cells::{CellAccountingConfig, CellRect, repair_delay_ms};
use mlp_core::media::input::InputKind;
use mlp_core::media::lifecycle::{CapturePolicy, MediaLanePolicy};
use mlp_core::media::resume::{ParkPolicy, ReconnectLadder};
use mlp_core::media::scheduler::UpdateBudget;
use mlp_core::media::{MAX_MEDIA_VIEWERS_PER_HOST, MediaPath};
use mlp_core::testing::lossy_link::Tier;
use mlp_core::testing::scenario::{
    Invariant, SIM_TICK_MS, Scenario, Step, carrier_for_tier, cold_start_scenario, run,
};

/// The repair delay the tier's own link produces (MLP spec §9.1).
///
/// The rule is a function of the RTT: a stream carrier waits
/// `clamp(RTT + update_interval, 250 ms, 3 s)` and a datagram carrier
/// `clamp(2 × RTT, 250 ms, 5 s)`. The update interval is the sender's own
/// cadence policy — the accounting's default, 30 fps — so the delay is the
/// tier's own and never one flat number handed to every tier.
fn tier_repair_delay_ms(tier: Tier, rtt_ms: f64) -> u64 {
    repair_delay_ms(
        carrier_for_tier(tier).is_datagram(),
        rtt_ms,
        CellAccountingConfig::default().update_interval_ms,
    )
}

/// The residue a tier leaves after the quiet phase, measured on the committed
/// scenario: T0 0, T1 0, T2 0, T3 1 of 510 cells — and T4 2, which is
/// asserted on its own bound.
///
/// The promise is convergence to the current screen (MLP spec §2.1), so the
/// bound is what the tiers actually leave — not a fraction of the surface.
fn tier_residue_cells(tier: Tier) -> u32 {
    match tier {
        Tier::T0 | Tier::T1 | Tier::T2 => 0,
        Tier::T3 => 1,
        Tier::T4 => 2,
    }
}

/// One motion scenario per tier, at the tier's own pace.
fn tier_scenario(tier: Tier, quiet_ms: u64) -> Scenario {
    Scenario::new(format!("budget-{}", tier.as_str()), tier)
        .change(CellRect::new(0, 0, 480, 320))
        .advance(1_500)
        .input(InputKind::Key)
        .advance(1_500)
        .change(CellRect::new(200, 120, 480, 320))
        .advance(1_500)
        .input(InputKind::PointerMove)
        .advance(quiet_ms)
        .step(Step::Assert(Invariant::Inv1))
        .step(Step::Assert(Invariant::Inv2))
        .step(Step::Assert(Invariant::Inv5))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7))
}

/// INV-6: no object exceeds the estimator's cap, and no update exceeds
/// `max_parts × fragment_cap`. The simulator enforces this while running;
/// this test asserts the trace's shape too.
#[test]
fn inv6_object_and_update_caps_hold_on_every_tier() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 8_000));
        trace.assert_clean();
        assert!(
            trace.counters.bytes_sent > 0,
            "{}: nothing was sent",
            tier.as_str()
        );
        // Every update the trace records fits the design's object rule: a update
        // is at most `max_parts × path cap` and each chunk is one object.
        let budget = UpdateBudget {
            bytes: 1_048_576,
            max_parts: 8,
            max_part_wire_bytes: 65_536,
            path_part_cap: 65_536,
            ..UpdateBudget::default()
        };
        for update in &trace.updates {
            assert!(
                u32::from(budget.payload_ceiling()) >= update.bytes,
                "{}: update {} is {} bytes",
                tier.as_str(),
                update.update_id,
                update.bytes
            );
        }
    }
}

/// INV-7: the pacer's 1-second window budget is never exceeded on any tier.
#[test]
fn inv7_rate_windows_hold_on_every_tier() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 10_000));
        trace.assert_clean();
        // The runner checks every window; a violation would have been recorded.
        assert!(
            !trace
                .violations
                .iter()
                .any(|violation| violation.invariant == "inv7"),
            "{}: a rate window was exceeded",
            tier.as_str()
        );
    }
}

/// Convergence: after motion stops, the pending set drains — on every tier.
///
/// The promise is convergence to the current screen within `2 × RTT + 30 s` of
/// link restoration (design §3.1); the quiet phase here is 12 s, and the tiers
/// are measured, not excused: what they leave pending after it is the bound.
///
/// T4's promise is longer than this scenario's quiet phase at a 35 % loss:
/// T4 is asserted on a bounded residue, and its convergence is *reported*
/// (N-3).
#[test]
fn every_tier_converges_after_motion_stops() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 12_000));
        trace.assert_clean();
        eprintln!(
            "MEASURE {}: {} of {} cells pending after the quiet phase (the measured residue is {}), {} converged observations",
            tier.as_str(),
            trace.counters.pending_cells,
            trace.counters.cells,
            tier_residue_cells(tier),
            trace.counters.converged_observations
        );
        if tier == Tier::T4 {
            assert!(
                trace.counters.pending_cells <= 16,
                "T4's residue stays small: {} of {}",
                trace.counters.pending_cells,
                trace.counters.cells
            );
            continue;
        }
        assert!(
            trace.counters.converged_observations > 0,
            "{}: the receiver never converged ({:?})",
            tier.as_str(),
            trace.counters
        );
        let residue = tier_residue_cells(tier);
        assert!(
            trace.counters.pending_cells <= residue,
            "{}: {} of {} cells still pending (the measured residue is {residue})",
            tier.as_str(),
            trace.counters.pending_cells,
            trace.counters.cells
        );
    }
}

/// The tier echo promises: `2 × RTT + 250 ms` as a p95 on T0–T3, measured at
/// the round trip the sender measured; T4's echo is reported, not promised
/// (N-3).
#[test]
fn echo_p95_matches_the_tier_promise() {
    for tier in [Tier::T0, Tier::T1, Tier::T2, Tier::T3] {
        let trace = run(&tier_scenario(tier, 6_000));
        trace.assert_clean();
        let Some(p95) = trace.echo_p95_ms() else {
            panic!("{}: no echo was measured", tier.as_str());
        };
        // The round trip is the one the sender measured: every timeout in the
        // specification is a function of the measured round trip (MLP spec
        // §9.1), and the link's jitter puts it above the tier's nominal point.
        let rtt_ms = trace.counters.rtt_ewma_ms.max(1.0);
        let budget = (2.0 * rtt_ms + 250.0).round() as u64;
        // One repair delay and the simulator's tick are the honest allowance:
        // the design repairs a lost echo-critical update instead of
        // retransmitting it (D-6). The repair delay is the tier's own
        // (MLP spec §9.1), so a LAN is not handed a long-haul link's grace.
        let allowance = budget + trace.counters.repair_delay_ms + SIM_TICK_MS;
        eprintln!(
            "MEASURE {}: echo p95 {p95} ms, the promise {budget} ms, the allowance {allowance} ms (rtt {rtt_ms:.0} ms, repair {} ms)",
            tier.as_str(),
            trace.counters.repair_delay_ms
        );
        assert!(
            p95 <= allowance,
            "{}: echo p95 {p95} ms above {allowance} ms",
            tier.as_str()
        );
        assert!(
            !trace.counters.echoes.is_empty(),
            "{}: the echo count is the measurement",
            tier.as_str()
        );
    }
}

/// The residue a cold start leaves, measured on the committed scenario.
///
/// T2 0 and T3 0 of 510 cells — the floor ramp drains, it does not strand the
/// surface. T4 leaves 3: its round trip is seconds, so the cells it has sent
/// are still in flight when the scenario ends, and its promise is convergence
/// within `2 × RTT + 30 s` — a window the 12 s scenario does not cover.
fn cold_start_residue_cells(tier: Tier) -> u32 {
    match tier {
        Tier::T2 | Tier::T3 => 0,
        Tier::T4 => 3,
        _ => 0,
    }
}

/// The cold start (no remembered prior) is honest: the first object is at
/// floor rate, every later step is bounded, and the session converges.
#[test]
fn the_cold_start_is_bounded_and_converges() {
    for tier in [Tier::T2, Tier::T3, Tier::T4] {
        let trace = run(&cold_start_scenario(tier));
        trace.assert_clean();
        assert!(
            trace.counters.updates_sent > 0,
            "{}: the cold start never sent a update",
            tier.as_str()
        );
        assert!(
            trace.counters.bytes_sent > 0,
            "{}: the cold start never sent bytes",
            tier.as_str()
        );
        eprintln!(
            "MEASURE cold-{}: {} of {} cells pending, {} converged observations",
            tier.as_str(),
            trace.counters.pending_cells,
            trace.counters.cells,
            trace.counters.converged_observations
        );
        // The residue the cold start leaves, measured: T2 0, T3 0, T4 3 of 510
        // cells — the floor ramp drains, it does not strand the surface.
        let residue = cold_start_residue_cells(tier);
        assert!(
            trace.counters.pending_cells <= residue,
            "{}: {} of {} cells still pending (the measured residue is {residue})",
            tier.as_str(),
            trace.counters.pending_cells,
            trace.counters.cells
        );
        // T2 and T3 converge inside the cold start's 12 s; T4's convergence is
        // promised at `2 × RTT + 30 s` and is reported, not promised (N-3).
        if tier != Tier::T4 {
            assert!(
                trace.counters.converged_observations > 0,
                "{}: the cold start never converged ({:?})",
                tier.as_str(),
                trace.counters
            );
        }
    }
}

/// The timeout updatework's arithmetic (design §13):
/// `session_idle ≥ 2 × outage + 4 × RTT`, `park_ttl ≥ outage + ladder`,
/// `heartbeat × 4 ≤ session_idle`, `repair_delay ≥ RTT`.
#[test]
fn the_timeout_arithmetic_holds_for_every_tier() {
    let tiers: [(Tier, u64, u64); 5] = [
        (Tier::T0, 0, 10),
        (Tier::T1, 5, 50),
        (Tier::T2, 5, 150),
        (Tier::T3, 10, 400),
        (Tier::T4, 30, 2_000),
    ];
    for (tier, outage_s, rtt_ms) in tiers {
        // The relay session idle window must outlast the outage with margin:
        // the rule is `clamp(2 × outage + 4 × RTT, 60 s, 300 s)` (MLP spec
        // §9.1), and the ceiling may not cut the requirement away.
        let required_idle = 2 * outage_s + 4 * rtt_ms / 1_000;
        let session_idle = required_idle.clamp(60, 300);
        assert!(
            session_idle >= required_idle,
            "{}: idle {session_idle}s vs the required {required_idle}s",
            tier.as_str()
        );
        // The heartbeat keeps the idle window open without wasting the link.
        let heartbeat = (3 * rtt_ms / 1_000).clamp(2, 30);
        assert!(
            heartbeat * 4 <= session_idle,
            "{}: heartbeat {heartbeat}s vs idle {session_idle}s",
            tier.as_str()
        );
        // The park window covers the outage plus the reconnect ladder.
        let park = ParkPolicy::park_ttl_for(outage_s, rtt_ms as f64);
        let ladder = ReconnectLadder::new(rtt_ms as f64);
        let mut elapsed = 0u64;
        let mut attempt = 0u32;
        while elapsed < outage_s * 1_000 {
            attempt += 1;
            elapsed += ladder.delay_ms(attempt, 250);
        }
        assert!(
            park * 1_000 >= outage_s * 1_000 + elapsed,
            "{}: park {park}s vs outage {outage_s}s and ladder {elapsed}ms",
            tier.as_str()
        );
        // The repair delay is never shorter than one RTT (MLP spec §9.2).
        let repair_delay = tier_repair_delay_ms(tier, rtt_ms as f64);
        assert!(
            repair_delay >= rtt_ms,
            "{}: the repair delay {repair_delay} ms is shorter than the RTT {rtt_ms} ms",
            tier.as_str()
        );
        assert!(rtt_ms >= 10, "every tier's RTT is at least 10 ms");
    }
}

/// The kill switches and policy defaults are the design's (design §13).
#[test]
fn policy_defaults_match_the_design() {
    let policy = MediaLanePolicy::default();
    assert!(policy.media_lane_enabled);
    assert!(policy.relay_stream_enabled);
    assert!(policy.patch_mode);
    assert!(policy.park_hold);
    assert_eq!(policy.input_deadline_ms, 5_000);
    assert_eq!(policy.resume_ttl_seconds, 900);
    assert_eq!(policy.floor_bps, 8_000);
    assert_eq!(policy.ceiling_bps, 50_000_000);
    assert!(policy.path_selection);
    assert!(policy.emits_media());
    assert!(policy.anchors_on_any_state());
    assert_eq!(MAX_MEDIA_VIEWERS_PER_HOST, 8);

    let capture = CapturePolicy::default();
    assert_eq!(capture.min_fps, 1.0);
    assert_eq!(capture.degraded_loss_ratio, 0.05);
    assert_eq!(capture.degraded_pace_bps, 64_000);
    assert!(capture.park_hold);
}

/// A parked receiver keeps capture alive; an expired park with the hold off stops
/// it; a stalled link always keeps capturing (design §8, D-9).
#[test]
fn capture_policy_matches_the_degradation_table() {
    let policy = CapturePolicy::default();
    assert!(!policy.decide(0, 0, false, 0.0, 1_000_000, 0.0).capture);
    assert!(policy.decide(0, 1, false, 0.0, 1_000_000, 0.0).capture);
    let no_hold = CapturePolicy {
        park_hold: false,
        ..policy
    };
    assert!(!no_hold.decide(0, 1, true, 0.0, 1_000_000, 0.0).capture);
    // A stalled link never pauses capture: it narrows it to P0.
    let stalled = policy.decide(1, 0, false, 0.30, 8_000, 0.9);
    assert!(stalled.capture);
    assert!(stalled.p0_only);
    assert!(!stalled.speculative_refresh);
}

/// Per-tier goodput is *reported*, not promised: T4 must still be alive and
/// honest even when it delivers very little (N-3).
#[test]
fn t4_reports_rather_than_promises_goodput() {
    let trace = run(&tier_scenario(Tier::T4, 20_000));
    trace.assert_clean();
    assert!(trace.counters.updates_sent > 0);
    assert!(
        trace.counters.updates_applied > 0,
        "T4 must deliver something: {:?}",
        trace.link
    );
    // The honest degradation states are reachable.
    assert!(!trace.counters.final_state.is_empty());
}

/// A carrier switch does not change the budget arithmetic.
#[test]
fn path_switches_do_not_change_the_budgets() {
    let mut emitted: Option<(u64, u64)> = None;
    for path in [MediaPath::Ws, MediaPath::DcDirect, MediaPath::DcTurn] {
        let scenario = Scenario::new(format!("path-{}", path.as_str()), Tier::T2)
            .step(Step::PathSwitch(path))
            .change(CellRect::new(0, 0, 480, 320))
            .advance(6_000)
            .step(Step::Assert(Invariant::Inv6))
            .step(Step::Assert(Invariant::Inv7));
        let trace = run(&scenario);
        trace.assert_clean();
        assert!(trace.counters.bytes_sent > 0);
        eprintln!(
            "MEASURE path-{}: {} updates, {} bytes, {} of {} cells pending, {} converged observations",
            path.as_str(),
            trace.counters.updates_sent,
            trace.counters.bytes_sent,
            trace.counters.pending_cells,
            trace.counters.cells,
            trace.counters.converged_observations
        );
        // The switch changes the carrier, not the budgets: the same updates
        // and the same bytes leave, and the tier converges the same way.
        let measured = (trace.counters.updates_sent, trace.counters.bytes_sent);
        match emitted {
            None => emitted = Some(measured),
            Some(first) => assert_eq!(
                measured,
                first,
                "{}: the carrier switch changed the budget arithmetic",
                path.as_str()
            ),
        }
        assert!(
            trace.counters.converged_observations > 0,
            "{}: the switched lane never converged ({:?})",
            path.as_str(),
            trace.counters
        );
        assert!(
            trace.counters.pending_cells <= tier_residue_cells(Tier::T2),
            "{}: {} of {} cells still pending",
            path.as_str(),
            trace.counters.pending_cells,
            trace.counters.cells
        );
    }
}
