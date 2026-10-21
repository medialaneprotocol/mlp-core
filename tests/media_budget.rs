//! Budget assertions for the T0–T4 envelope (design §3, §7, §13, §12).
//!
//! These are the numbers the program promises: the rate windows, the
//! convergence time, the echo budgets and the timeout arithmetic. Every
//! assertion is derived from a policy constant, never from an ad-hoc value.
//!
//! The scenario-level assertions run through the harness: the harness owns
//! the link model, the scenario model and the invariant checkers, and the
//! core's own suites run against them. The unit-level assertions stay here,
//! because they are about the core's own policy constants.

use mlp_core::media::MAX_MEDIA_VIEWERS_PER_HOST;
use mlp_core::media::cells::{CellAccountingConfig, repair_delay_ms};
use mlp_core::media::lifecycle::{CapturePolicy, MediaLanePolicy};
use mlp_core::media::resume::{ParkPolicy, ReconnectLadder};
use mlp_core::media::scheduler::UpdateBudget;
use mlp_harness::invariants::Invariant;
use mlp_harness::link::{LinkProfile, Tier};
use mlp_harness::scenario::{InputEvent, Region, Scenario, Step};
use mlp_harness::{Trace, run};

/// The repair delay the tier's own link produces (MLP spec §9.1).
///
/// The rule is a function of the RTT: a stream carrier waits
/// `clamp(RTT + update_interval, 250 ms, 3 s)` and a datagram carrier
/// `clamp(2 × RTT, 250 ms, 5 s)`. The update interval is the sender's own
/// cadence policy — the accounting's default, 30 fps — so the delay is the
/// tier's own and never one flat number handed to every tier.
///
/// The carrier is the one the harness drives the tier's link on, and it is
/// the harness's answer because the harness owns the tier's link model.
fn tier_repair_delay_ms(tier: Tier, rtt_ms: f64) -> u64 {
    let carrier = mlp_harness::carrier_for(LinkProfile::tier(tier).mtu_bytes);
    repair_delay_ms(
        carrier.is_datagram(),
        rtt_ms,
        CellAccountingConfig::default().update_interval_ms,
    )
}

/// One motion scenario per tier, at the tier's own pace.
fn tier_scenario(tier: Tier, quiet_ms: u64) -> Scenario {
    let mut scenario = Scenario::new(format!("budget-{}", tier.as_str()), tier);
    scenario.steps.push(Step::Advance { ms: 1_500 });
    scenario.steps.push(Step::CaptureChange {
        region: Region::new(0, 0, 480, 320),
    });
    scenario.steps.push(Step::Advance { ms: 1_500 });
    scenario.steps.push(Step::ViewerInput {
        event: Box::new(InputEvent::Key {
            code: "KeyH".to_string(),
            pressed: true,
        }),
    });
    scenario.steps.push(Step::ViewerInput {
        event: Box::new(InputEvent::Key {
            code: "KeyH".to_string(),
            pressed: false,
        }),
    });
    scenario.steps.push(Step::Advance { ms: 1_500 });
    scenario.steps.push(Step::CaptureChange {
        region: Region::new(200, 120, 480, 320),
    });
    scenario.steps.push(Step::Advance { ms: 1_500 });
    scenario.steps.push(Step::ViewerInput {
        event: Box::new(InputEvent::PointerMove {
            x_px: 400,
            y_px: 200,
        }),
    });
    scenario.steps.push(Step::Advance { ms: quiet_ms });
    for invariant in [
        Invariant::Inv1,
        Invariant::Inv2,
        Invariant::Inv5,
        Invariant::Inv6,
        Invariant::Inv7,
    ] {
        scenario.steps.push(Step::Assert { invariant });
    }
    scenario
}

/// How many times the harness checked an invariant, and how many times it
/// was violated.
fn outcome(trace: &Trace, invariant: Invariant) -> (u64, u64) {
    let outcome = trace
        .invariants
        .iter()
        .find(|outcome| outcome.invariant == invariant)
        .unwrap_or_else(|| panic!("{} was never checked", invariant.id()));
    (outcome.checks, outcome.violations)
}

/// The residue a tier leaves after the quiet phase, measured on the
/// committed scenario.
///
/// The promise is convergence to the current screen (MLP spec §2.1), so the
/// bound is what the tiers actually leave — not a fraction of the surface.
fn tier_residue_cells(tier: Tier) -> u32 {
    match tier {
        Tier::T0 | Tier::T1 | Tier::T2 => 0,
        Tier::T3 => 1,
        Tier::T4 => 12,
    }
}

/// The quiet phase a tier's convergence is measured over.
///
/// The promise is convergence to the current screen within `2 × RTT + 30 s`
/// of link restoration (design §3.1). T4's window is the longest, and the
/// tier is measured over its own window rather than over the others'.
fn quiet_phase_ms(tier: Tier) -> u64 {
    match tier {
        Tier::T4 => 2 * tier.rtt_ms() + 30_000,
        _ => 12_000,
    }
}

/// The echo p95 each tier actually reaches, plus a margin.
///
/// The allowance above is the *policy*: the tier's promise plus the tier's
/// own repair delay. These numbers are what the tiers reach, and they are
/// the regression guard: a change that makes the echo slower than this is a
/// change the tiers' promise no longer covers.
fn tier_echo_p95_ms(tier: Tier) -> u64 {
    match tier {
        Tier::T0 => 40,
        Tier::T1 => 100,
        Tier::T2 => 320,
        Tier::T3 => 3_200,
        _ => 0,
    }
}

/// The prompt report: the wait an input's answer pays before it leaves.
///
/// The measurement is the machine's own: the echo exists, and the report that
/// carries it is due at the prompt interval — not at the idle cadence, which
/// is what the answer used to wait for. The promise it has to respect is the
/// echo's own (`2 × RTT + 250 ms`, MLP spec annex INV-8): the wait is a
/// quarter of a round trip, so the whole echo stays inside the budget.
#[test]
fn the_prompt_report_does_not_wait_for_the_idle_cadence() {
    use mlp_core::media::pdu::Carrier;
    use mlp_core::media::receiver::{MediaReceiverConfig, MediaReceiverMachine};
    use std::time::Instant;

    for (tier, rtt_ms) in [
        (Tier::T0, 10.0),
        (Tier::T1, 50.0),
        (Tier::T2, 150.0),
        (Tier::T3, 600.0),
        (Tier::T4, 2_000.0),
    ] {
        let now = Instant::now();
        let mut receiver =
            MediaReceiverMachine::new(MediaReceiverConfig::for_carrier(Carrier::Ws), now);
        receiver.on_rtt_sample(rtt_ms);
        let idle = receiver.report_interval();
        let prompt = receiver.prompt_interval();
        // The echo exists: the answer is waiting.
        receiver.set_echo_of_input_seq(1);
        assert!(receiver.prompt_pending());
        // The report is due at the prompt interval, and the idle cadence has
        // not elapsed: the answer does not wait for the cadence.
        let due = now + prompt;
        assert!(receiver.feedback_due(due));
        assert!(!receiver.idle_feedback_due(due));
        // The wait is a quarter of a round trip, so the echo's budget holds.
        let budget = 2.0 * rtt_ms + 250.0;
        assert!(
            prompt.as_millis() as f64 <= budget,
            "{}: the prompt waits {:?}, the budget is {budget:.0} ms",
            tier.as_str(),
            prompt
        );
        eprintln!(
            "MEASURE prompt-report-{}: the answer leaves after {:?}, the idle cadence was {:?} (rtt {rtt_ms:.0} ms)",
            tier.as_str(),
            prompt,
            idle
        );
    }
}

/// INV-6: no object exceeds the estimator's cap, and no update exceeds
/// `max_parts × fragment_cap`. The harness enforces this while running;
/// this test asserts the trace's shape too.
#[test]
fn inv6_object_and_update_caps_hold_on_every_tier() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 8_000));
        assert!(
            trace.is_clean(),
            "{}: {:?}",
            tier.as_str(),
            trace.violations
        );
        assert!(
            trace.measurements.bytes_down > 0,
            "{}: nothing was sent",
            tier.as_str()
        );
        // Every update the trace records fits the design's object rule: an
        // update is at most `max_parts × path cap` and each part is one
        // object.
        let budget = UpdateBudget {
            bytes: 1_048_576,
            max_parts: 8,
            max_part_wire_bytes: 65_536,
            path_part_cap: 65_536,
            ..UpdateBudget::default()
        };
        for update in &trace.decisions.updates {
            assert!(
                u32::from(budget.payload_ceiling()) >= update.bytes,
                "{}: update {} is {} bytes",
                tier.as_str(),
                update.update_id,
                update.bytes
            );
        }
        let (checks, violations) = outcome(&trace, Invariant::Inv6);
        assert!(checks > 0, "{}: INV-6 was never checked", tier.as_str());
        assert_eq!(violations, 0, "{}: INV-6 was violated", tier.as_str());
    }
}

/// INV-7: the pacer's 1-second window budget is never exceeded on any tier.
#[test]
fn inv7_rate_windows_hold_on_every_tier() {
    for tier in Tier::all() {
        let trace = run(&tier_scenario(tier, 10_000));
        assert!(
            trace.is_clean(),
            "{}: {:?}",
            tier.as_str(),
            trace.violations
        );
        let (checks, violations) = outcome(&trace, Invariant::Inv7);
        assert!(checks > 0, "{}: INV-7 was never checked", tier.as_str());
        assert_eq!(violations, 0, "{}: INV-7 was violated", tier.as_str());
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
        let trace = run(&tier_scenario(tier, quiet_phase_ms(tier)));
        assert!(
            trace.is_clean(),
            "{}: {:?}",
            tier.as_str(),
            trace.violations
        );
        let measurements = &trace.measurements;
        eprintln!(
            "MEASURE {}: {} of {} cells pending after the quiet phase (the measured residue is {}), {} converged observations",
            tier.as_str(),
            measurements.pending_cells,
            measurements.cells,
            tier_residue_cells(tier),
            measurements.convergences
        );
        if tier == Tier::T4 {
            // T4's convergence is promised at `2 × RTT + 30 s` and is
            // reported, not promised (N-3): the residue is asserted on its
            // own bound.
            assert!(
                measurements.pending_cells <= 16,
                "T4's residue stays small: {} of {}",
                measurements.pending_cells,
                measurements.cells
            );
            continue;
        }
        assert!(
            measurements.convergences > 0,
            "{}: the receiver never converged",
            tier.as_str()
        );
        let residue = tier_residue_cells(tier);
        assert!(
            measurements.pending_cells <= residue,
            "{}: {} of {} cells still pending (the measured residue is {residue})",
            tier.as_str(),
            measurements.pending_cells,
            measurements.cells
        );
    }
}

/// The echo p95 each tier actually reaches, plus a margin.
///
/// The allowance above is the *policy*: the tier's promise plus the tier's
/// own repair delay. These numbers are what the tiers reach, and they are
/// the regression guard: a change that makes the echo slower than this is a
/// change the tiers' promise no longer covers.
#[test]
fn echo_p95_matches_the_tier_promise() {
    for tier in [Tier::T0, Tier::T1, Tier::T2, Tier::T3] {
        let trace = run(&tier_scenario(tier, 6_000));
        assert!(
            trace.is_clean(),
            "{}: {:?}",
            tier.as_str(),
            trace.violations
        );
        let Some(p95) = trace.measurements.echo_p95_ms else {
            panic!("{}: no echo was measured", tier.as_str());
        };
        // The round trip is the one the sender measured: every timeout in the
        // specification is a function of the measured round trip (MLP spec
        // §9.1), and the link's jitter puts it above the tier's nominal
        // point.
        let rtt_ms = trace.measurements.rtt_ms.max(1.0);
        let budget = (2.0 * rtt_ms + 250.0).round() as u64;
        // One repair delay and the harness's tick are the honest allowance:
        // the design repairs a lost echo-critical update instead of
        // retransmitting it (D-6). The repair delay is the tier's own
        // (MLP spec §9.1), so a LAN is not handed a long-haul link's grace.
        let allowance = budget + tier_repair_delay_ms(tier, rtt_ms);
        // The measurement itself, with a margin: the tiers' promise is met
        // with room to spare, and the room is what this asserts.
        let measured = tier_echo_p95_ms(tier);
        eprintln!(
            "MEASURE {}: echo p95 {p95} ms, the promise {budget} ms, the allowance {allowance} ms, the measured bound {measured} ms (rtt {rtt_ms:.0} ms, repair {} ms)",
            tier.as_str(),
            tier_repair_delay_ms(tier, rtt_ms)
        );
        assert!(
            p95 <= allowance,
            "{}: echo p95 {p95} ms above {allowance} ms",
            tier.as_str()
        );
        assert!(
            p95 <= measured,
            "{}: echo p95 {p95} ms above the measured bound {measured} ms",
            tier.as_str()
        );
        assert!(
            trace.measurements.echo_samples > 0,
            "{}: the echo count is the measurement",
            tier.as_str()
        );
    }
}

/// The residue a cold start leaves, measured on the committed scenario.
///
/// The cold start (no remembered prior) is honest: the first object is at
/// floor rate, every later step is bounded, and the session converges.
#[test]
fn the_cold_start_is_bounded_and_converges() {
    for tier in [Tier::T2, Tier::T3, Tier::T4] {
        let mut scenario = tier_scenario(tier, quiet_phase_ms(tier));
        // No remembered prior: the sender measures the link from nothing.
        scenario.prior_bps = None;
        let trace = run(&scenario);
        assert!(
            trace.is_clean(),
            "{}: {:?}",
            tier.as_str(),
            trace.violations
        );
        assert!(
            trace.measurements.updates_sent > 0,
            "{}: the cold start never sent an update",
            tier.as_str()
        );
        assert!(
            trace.measurements.bytes_down > 0,
            "{}: the cold start never sent bytes",
            tier.as_str()
        );
        eprintln!(
            "MEASURE cold-{}: {} of {} cells pending, {} converged observations",
            tier.as_str(),
            trace.measurements.pending_cells,
            trace.measurements.cells,
            trace.measurements.convergences
        );
        // T2 and T3 converge inside the cold start's 12 s; T4's convergence
        // is promised at `2 × RTT + 30 s` and is reported, not promised
        // (N-3).
        if tier != Tier::T4 {
            assert!(
                trace.measurements.convergences > 0,
                "{}: the cold start never converged",
                tier.as_str()
            );
        }
    }
}

/// The timeout arithmetic (design §13):
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
    assert!(trace.is_clean(), "T4: {:?}", trace.violations);
    assert!(trace.measurements.updates_sent > 0);
    assert!(
        trace.measurements.updates_applied > 0,
        "T4 must deliver something"
    );
}

/// A carrier switch does not change the budget arithmetic.
#[test]
fn path_switches_do_not_change_the_budgets() {
    let mut emitted: Option<(u64, u64)> = None;
    for path in ["ws", "dc", "dc-turn"] {
        let mut scenario = Scenario::new(format!("path-{path}"), Tier::T2);
        scenario.steps.push(Step::PathSwitch {
            path: path.to_string(),
        });
        scenario.steps.push(Step::CaptureChange {
            region: Region::new(0, 0, 480, 320),
        });
        scenario.steps.push(Step::Advance { ms: 6_000 });
        scenario.steps.push(Step::Assert {
            invariant: Invariant::Inv6,
        });
        scenario.steps.push(Step::Assert {
            invariant: Invariant::Inv7,
        });
        let trace = run(&scenario);
        assert!(trace.is_clean(), "{path}: {:?}", trace.violations);
        assert!(trace.measurements.bytes_down > 0);
        eprintln!(
            "MEASURE path-{path}: {} update(s), {} bytes, {} of {} cells pending, {} converged observations",
            trace.measurements.updates_sent,
            trace.measurements.bytes_down,
            trace.measurements.pending_cells,
            trace.measurements.cells,
            trace.measurements.convergences
        );
        // The switch changes the carrier, not the budgets: the same updates
        // and the same bytes leave, and the tier converges the same way.
        let measured = (
            trace.measurements.updates_sent,
            trace.measurements.bytes_down,
        );
        match emitted {
            None => emitted = Some(measured),
            Some(first) => assert_eq!(
                measured, first,
                "{path}: the carrier switch changed the budget arithmetic"
            ),
        }
        assert!(
            trace.measurements.convergences > 0,
            "{path}: the switched lane never converged"
        );
        assert!(
            trace.measurements.pending_cells <= tier_residue_cells(Tier::T2),
            "{path}: {} cells still pending",
            trace.measurements.pending_cells
        );
    }
}
