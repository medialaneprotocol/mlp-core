//! The core's own suite, driven by the harness.
//!
//! The harness is the transport's verification layer: it drives this crate's
//! machines over a modelled link, checks the protocol's invariants as the run
//! proceeds, and writes a trace a machine can diff. This suite is the core's
//! half of that arrangement — the harness's canonical scenario at every tier,
//! driven over *this* core, holds every invariant, and the tier's own
//! promises are met.
//!
//! The cycle between the two crates runs through a dev-dependency, which
//! cargo allows, and the core's manifest patches the harness's dependency on
//! the core back to this checkout. That patch is what makes the cycle
//! type-consistent: the harness's driver takes *these* machines, and the
//! types in the two crates are the same types. The first test below is the
//! proof — it would not compile if the graph held two copies of the core.

use std::time::Duration;

use mlp_core::media::MediaSenderInput;
use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine, UpdateRequest};
use mlp_harness::invariants::Invariant;
use mlp_harness::link::{LinkProfile, Tier};
use mlp_harness::predict;
use mlp_harness::sans_io::TestHarness;
use mlp_harness::scenario::Scenario;
use mlp_harness::{Trace, run};

/// The outcome of one invariant in a run, if it was checked.
fn outcome(trace: &Trace, invariant: Invariant) -> Option<(u64, u64)> {
    trace
        .invariants
        .iter()
        .find(|outcome| outcome.invariant == invariant)
        .map(|outcome| (outcome.checks, outcome.violations))
}

/// The harness's driver takes this crate's sender machine.
///
/// The types are the same types on both sides of the cycle, and that is the
/// patch's work: without it the harness would link the published core while
/// the core under test is this one, and this line would not compile.
#[test]
fn the_harness_drives_this_cores_machines() {
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let epoch = mlp_harness::epoch();
    let sender = MediaSenderMachine::new(MediaSenderConfig::new(Carrier::Ws, grid), grid, epoch);
    let mut driver = TestHarness::new(sender, epoch);

    driver.inject(MediaSenderInput::Update(Box::new(UpdateRequest::delta(
        vec![0u8; 64],
        vec![0, 1],
    ))));
    driver.run_until_quiescent(64);

    let stats = driver.machine.stats();
    assert_eq!(stats.updates_enqueued, 1, "the update reached the machine");
    assert_eq!(stats.objects_enqueued, 1, "the update became an object");
    assert_eq!(stats.object_oversize_total, 0, "the object fitted its cap");
    assert!(
        driver.actions().is_empty(),
        "a well-formed update is accepted without a complaint"
    );
}

/// The tier's canonical scenario holds every invariant the harness checks.
///
/// The harness checks the cheap invariants on every tick, so a clean run is
/// the protocol's promises holding over the whole session, not at one
/// moment.
#[test]
fn the_canonical_scenario_at_every_tier_holds_every_invariant() {
    for tier in Tier::all() {
        let trace = run(&predict::canonical_scenario(tier));
        assert!(
            trace.is_clean(),
            "{}: the run broke {} invariant(s): {:?}",
            tier.as_str(),
            trace.violations.len(),
            trace.violations
        );
        for invariant in Invariant::all() {
            match outcome(&trace, invariant) {
                Some((_, violations)) => assert_eq!(
                    violations,
                    0,
                    "{}: {} was violated",
                    tier.as_str(),
                    invariant.id()
                ),
                None => assert!(
                    !invariant.is_cheap(),
                    "{}: {} was never checked",
                    tier.as_str(),
                    invariant.id()
                ),
            }
        }
        for invariant in Invariant::cheap() {
            let (checks, _) = outcome(&trace, invariant)
                .unwrap_or_else(|| panic!("{} was never checked", invariant.id()));
            assert!(
                checks > 0,
                "{}: {} was never checked",
                tier.as_str(),
                invariant.id()
            );
        }
    }
}

/// The canonical scenario at every tier converges, and delivers something.
#[test]
fn the_canonical_scenario_at_every_tier_converges() {
    for tier in Tier::all() {
        let trace = run(&predict::canonical_scenario(tier));
        let measurements = &trace.measurements;
        eprintln!(
            "MEASURE canonical-{}: {} update(s) sent, {} applied, {} object(s) delivered, {} dropped, {} convergence(s) in {:?} ms, echo p95 {:?} ms, {} bytes down, {} bytes up, {} of {} cells pending",
            tier.as_str(),
            measurements.updates_sent,
            measurements.updates_applied,
            measurements.objects_delivered,
            measurements.objects_dropped,
            measurements.convergences,
            measurements.convergence_ms,
            measurements.echo_p95_ms,
            measurements.bytes_down,
            measurements.bytes_up,
            measurements.pending_cells,
            measurements.cells,
        );
        assert!(
            measurements.updates_sent > 0,
            "{}: the session was silent",
            tier.as_str()
        );
        assert!(
            measurements.updates_applied > 0,
            "{}: nothing was delivered",
            tier.as_str()
        );
        if tier == Tier::T4 {
            // T4 is reported rather than promised (MLP spec §2.1): the
            // extreme tier's goodput is honest, not a promise, and the
            // residue is asserted on its own bound.
            assert!(
                measurements.pending_cells <= 16,
                "{}: {} cells were still pending",
                tier.as_str(),
                measurements.pending_cells
            );
            continue;
        }
        assert!(
            measurements.convergences > 0,
            "{}: the receiver never converged",
            tier.as_str()
        );
        assert_eq!(
            measurements.pending_cells,
            0,
            "{}: {} cells were still pending",
            tier.as_str(),
            measurements.pending_cells
        );
    }
}

/// The tier's echo promise holds where the promise applies.
///
/// The promise is `2 × RTT + 250 ms` (MLP spec §2.1), and it applies to a
/// link inside the tier's envelope. T4 is reported rather than promised, so
/// the promise is asserted for the tiers it is made for.
#[test]
fn the_canonical_scenario_holds_the_tiers_echo_promise() {
    for tier in [Tier::T0, Tier::T1, Tier::T2, Tier::T3] {
        let prediction = predict::predict_tier(tier);
        assert_eq!(prediction.tier, tier);
        assert_eq!(prediction.echo_p95_budget_ms, 2 * tier.rtt_ms() + 250);
        assert!(
            prediction.echo_promise_applies,
            "{}: the promise should apply to the tier's own link",
            tier.as_str()
        );
        assert!(
            prediction.echo_within_promise,
            "{}: the echo p95 {:?} ms is above the {} ms promise",
            tier.as_str(),
            prediction.echo_p95_ms,
            prediction.echo_p95_budget_ms
        );
        assert!(prediction.clean, "{}: the run was not clean", tier.as_str());
    }
}

/// The same scenario runs identically, twice.
///
/// The trace is the golden record: a scenario that runs twice must produce
/// the same bytes, on any machine.
#[test]
fn a_scenario_runs_identically_twice() {
    for tier in Tier::all() {
        let scenario = predict::canonical_scenario(tier);
        let first = run(&scenario);
        let second = run(&scenario);
        assert_eq!(first.digest, second.digest, "{}", tier.as_str());
        assert_eq!(first.to_json(), second.to_json(), "{}", tier.as_str());
    }
}

/// A scenario over the tier's own link profile is clean at every tier.
#[test]
fn a_scenario_over_the_tiers_own_link_is_clean() {
    for tier in Tier::all() {
        let mut scenario = Scenario::new(format!("core-tier-{}", tier.as_str()), tier);
        scenario.link = LinkProfile::tier(tier);
        scenario
            .steps
            .push(mlp_harness::scenario::Step::Advance { ms: 2_000 });
        scenario
            .steps
            .push(mlp_harness::scenario::Step::CaptureChange {
                region: mlp_harness::scenario::Region::new(320, 240, 640, 200),
            });
        scenario
            .steps
            .push(mlp_harness::scenario::Step::Advance { ms: 4_000 });
        let trace = run(&scenario);
        assert!(
            trace.is_clean(),
            "{}: {:?}",
            tier.as_str(),
            trace.violations
        );
        assert!(trace.measurements.duration_ms >= 6_000);
    }
}

/// The harness's driver ticks a millisecond at a time.
///
/// The driver is deterministic down to its unit, and a scenario that
/// advances by a millisecond advances by exactly a millisecond.
#[test]
fn the_driver_advances_by_exactly_what_the_scenario_asks_for() {
    let mut scenario = Scenario::new("core-tick", Tier::T0);
    scenario
        .steps
        .push(mlp_harness::scenario::Step::Advance { ms: 250 });
    let trace = run(&scenario);
    assert_eq!(trace.measurements.duration_ms, 250);
    assert_eq!(trace.scenario.duration_ms, 250);
    let _ = Duration::from_millis(0);
}
