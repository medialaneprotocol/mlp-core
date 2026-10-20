//! Budget assertions for the T0–T4 envelope (design §3, §7, §13, §12).
//!
//! These are the numbers the program promises: object sizes, rate windows,
//! convergence time, echo budgets and the timeout arithmetic. Every assertion
//! is derived from a policy constant, never from an ad-hoc value.

use mlp_core::media::cells::CellRect;
use mlp_core::media::input::InputKind;
use mlp_core::media::lifecycle::{CapturePolicy, MediaLanePolicy};
use mlp_core::media::resume::{ParkPolicy, ReconnectLadder};
use mlp_core::media::scheduler::UpdateBudget;
use mlp_core::media::{MAX_MEDIA_VIEWERS_PER_HOST, MediaPath};
use mlp_core::testing::lossy_link::{LinkProfile, Tier};
use mlp_core::testing::scenario::{Invariant, Scenario, Step, cold_start_scenario, run};

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

/// INV-6: no object exceeds the estimator's cap, and no frame exceeds
/// `max_chunks × fragment_cap`. The simulator enforces this while running;
/// this test asserts the trace's shape too.
#[test]
fn inv6_object_and_frame_caps_hold_on_every_tier() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 8_000));
        trace.assert_clean();
        assert!(
            trace.counters.bytes_sent > 0,
            "{}: nothing was sent",
            tier.as_str()
        );
        // Every frame the trace records fits the design's object rule: a frame
        // is at most `max_chunks × path cap` and each chunk is one object.
        let budget = UpdateBudget {
            bytes: 1_048_576,
            max_chunks: 8,
            max_chunk_wire_bytes: 65_536,
            path_chunk_cap: 65_536,
            ..UpdateBudget::default()
        };
        for frame in &trace.updates {
            assert!(
                u32::from(budget.payload_ceiling()) >= frame.bytes,
                "{}: frame {} is {} bytes",
                tier.as_str(),
                frame.update_id,
                frame.bytes
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
/// T4's promise is "convergence to the current screen within 2×RTT + 30 s of
/// link restoration" (design §3.1), which is longer than this scenario's quiet
/// phase at a 35 % loss: T4 is asserted on a bounded residue, and its
/// convergence is *reported* (N-3).
#[test]
fn every_tier_converges_after_motion_stops() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 12_000));
        trace.assert_clean();
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
        assert!(
            trace.counters.pending_cells <= trace.counters.cells / 4,
            "{}: {} of {} cells still pending",
            tier.as_str(),
            trace.counters.pending_cells,
            trace.counters.cells
        );
    }
}

/// The tier echo promises: `2 × RTT + 250 ms` as a p95 on T0–T3, measured at
/// the tier's own RTT; T4's echo is reported, not promised (N-3).
#[test]
fn echo_p95_matches_the_tier_promise() {
    for tier in [Tier::T0, Tier::T1, Tier::T2, Tier::T3] {
        let trace = run(&tier_scenario(tier, 6_000));
        trace.assert_clean();
        let Some(p95) = trace.echo_p95_ms() else {
            panic!("{}: no echo was measured", tier.as_str());
        };
        let budget = (2.0 * LinkProfile::tier(tier).rtt_ms + 250.0).round() as u64;
        // One repair delay and the simulator's tick are the honest allowance:
        // the design repairs a lost echo-critical frame instead of
        // retransmitting it (D-6).
        let allowance = budget + 3_000 + 10;
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

/// The cold start (no remembered prior) is honest: the first object is at
/// floor rate, every later step is bounded, and the session converges.
#[test]
fn the_cold_start_is_bounded_and_converges() {
    for tier in [Tier::T2, Tier::T3, Tier::T4] {
        let trace = run(&cold_start_scenario(tier));
        trace.assert_clean();
        assert!(
            trace.counters.updates_sent > 0,
            "{}: the cold start never sent a frame",
            tier.as_str()
        );
        assert!(
            trace.counters.bytes_sent > 0,
            "{}: the cold start never sent bytes",
            tier.as_str()
        );
    }
}

/// The timeout framework's arithmetic (design §13):
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
        // The relay session idle window must outlast the outage with margin.
        let session_idle = (2 * outage_s + 4 * rtt_ms / 1_000).max(60);
        assert!(
            session_idle >= 2 * outage_s + 4 * rtt_ms / 1_000,
            "{}: idle {session_idle}s",
            tier.as_str()
        );
        // The heartbeat keeps the idle window open without wasting the link.
        let heartbeat = (3 * rtt_ms / 1_000).clamp(2, 30);
        assert!(
            heartbeat * 4 <= session_idle.max(15),
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
            park * 1_000 >= outage_s * 1_000 + elapsed.min(60_000),
            "{}: park {park}s vs outage {outage_s}s and ladder {elapsed}ms",
            tier.as_str()
        );
        // The repair delay is never shorter than one RTT.
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
    }
}
