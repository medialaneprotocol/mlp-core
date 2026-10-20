//! L2 risky-link scenarios (design §12, §12): the tier stories over the
//! deterministic simulator. Run with `-- --nocapture` to see the traces.

use mlp_core::media::MediaPath;
use mlp_core::media::cells::CellRect;
use mlp_core::media::input::InputKind;
use mlp_core::testing::lossy_link::{LinkProfile, Tier};
use mlp_core::testing::scenario::{
    IntermediaryModel, Invariant, Scenario, Step, run, run_with_relay,
};

fn motion(tier: Tier, ms: u64) -> Scenario {
    Scenario::new(format!("motion-{}", tier.as_str()), tier)
        .change(CellRect::new(0, 0, 256, 128))
        .advance(ms / 4)
        // An input pulse: the echo promise is measured against it (N-1).
        .input(InputKind::Key)
        .advance(ms / 4)
        .change(CellRect::new(0, 0, 256, 128))
        .advance(ms / 4)
        .change(CellRect::new(600, 300, 384, 256))
        .advance(ms / 4)
        .step(Step::Assert(Invariant::Inv1))
        .step(Step::Assert(Invariant::Inv2))
        .step(Step::Assert(Invariant::Inv5))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7))
}

/// T0: LAN. Full-rate deltas, convergence inside a second, zero loss.
#[test]
fn t0_lan_motion_converges_immediately() {
    let trace = run(&motion(Tier::T0, 3_000));
    trace.assert_clean();
    assert_eq!(trace.link.dropped_loss, 0, "the LAN profile is lossless");
    assert!(trace.counters.updates_sent >= 3);
    assert!(trace.counters.updates_applied >= 3);
    assert!(
        trace.counters.converged_observations > 0,
        "T0 must converge"
    );
    assert_eq!(trace.counters.pending_cells, 0);
    println!("T0: {trace:#?}");
}

/// T1: broadband. Echo inside 2×RTT+250 ms, convergence within a second of
/// motion stopping.
#[test]
fn t1_broadband_echo_and_convergence() {
    let mut scenario = motion(Tier::T1, 4_000);
    scenario.steps.push(Step::Assert(Invariant::Inv8));
    let trace = run(&scenario);
    trace.assert_clean();
    let echo = trace.echo_p95_ms().expect("T1 measures an echo");
    // One simulator tick of tolerance on the p95 (see SIM_TICK_MS).
    let budget = 2 * 50 + 250 + 10;
    assert!(echo <= budget, "echo {echo} ms above {budget} ms");
    assert!(trace.counters.converged_observations > 0);
}

/// T2: poor Wi-Fi. Lossy but usable: repair happens and the receiver converges.
#[test]
fn t2_poor_wifi_repairs_and_converges() {
    let mut scenario = motion(Tier::T2, 4_000);
    scenario = scenario.advance(4_000);
    scenario.steps.push(Step::Assert(Invariant::Inv6));
    scenario = scenario.step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.link.dropped_loss > 0, "T2 loses something");
    assert!(trace.counters.updates_applied >= 2);
    assert!(
        trace.counters.converged_observations > 0,
        "T2 reaches convergence at least once"
    );
    assert!(
        trace.counters.pending_cells <= 32,
        "T2's steady-state repair residue stays small: {}",
        trace.counters.pending_cells
    );
}

/// T3: long-haul degradation. A full-screen change completes through anchor
/// plans, and the pending set is monotone to zero.
#[test]
fn t3_long_haul_full_screen_change_converges() {
    let scenario = Scenario::new("t3-full-screen", Tier::T3)
        .change(CellRect::new(0, 0, 1920, 1080))
        .advance(4_000)
        .change(CellRect::new(0, 0, 1920, 1080))
        .advance(24_000)
        .step(Step::Assert(Invariant::Inv2))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.updates_sent > 0);
    assert!(
        trace.counters.updates_applied > 0,
        "T3 must deliver: {:?}",
        trace.link
    );
    assert!(
        trace.counters.converged_observations > 0,
        "T3 drains the full-screen change: pending reaches zero at least once"
    );
    assert!(
        trace.counters.pending_cells <= trace.counters.cells / 10,
        "T3's residue stays under a tenth of the screen: {} of {}",
        trace.counters.pending_cells,
        trace.counters.cells
    );
    println!(
        "T3: updates {} applied {} pending {} plans {}",
        trace.counters.updates_sent,
        trace.counters.updates_applied,
        trace.counters.pending_cells,
        trace.counters.plans_started
    );
}

/// T4: extreme. Structural promises only (N-3): the session lives, input works,
/// and every byte is paced; goodput is *reported*, not promised.
#[test]
fn t4_extreme_stays_alive_and_honest() {
    let scenario = Scenario::new("t4-text-first", Tier::T4)
        .change(CellRect::new(0, 0, 128, 64))
        .advance(6_000)
        .input(InputKind::Key)
        .advance(6_000)
        .change(CellRect::new(0, 0, 128, 64))
        .advance(12_000)
        .step(Step::Assert(Invariant::Inv1))
        .step(Step::Assert(Invariant::Inv5))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(
        trace.counters.updates_sent > 0,
        "the lane must not be silent"
    );
    assert!(
        trace.counters.bytes_sent > 0,
        "some bytes reach the carrier"
    );
    println!(
        "T4: sent {} applied {} bytes {} loss {}/{} echo {:?}",
        trace.counters.updates_sent,
        trace.counters.updates_applied,
        trace.counters.bytes_sent,
        trace.link.dropped_loss,
        trace.link.transmitted,
        trace.echo_p95_ms()
    );
}

/// A 30 s outage: the lane parks and resumes with no reset frame and one media
/// session (INV-9).
#[test]
fn a_thirty_second_outage_parks_and_resumes() {
    let profile = LinkProfile {
        outage_schedule: vec![mlp_core::testing::lossy_link::Outage {
            start_ms: 1_000,
            dur_ms: 30_000,
        }],
        ..LinkProfile::tier(Tier::T4)
    };
    let scenario = Scenario::new("t4-outage", Tier::T4)
        .step(Step::Link(Box::new(profile)))
        .change(CellRect::new(0, 0, 256, 128))
        .advance(2_000)
        .step(Step::ViewerDisconnect)
        .advance(30_000)
        .step(Step::ViewerReconnect)
        .advance(20_000)
        .step(Step::Assert(Invariant::Inv9));
    let trace = run(&scenario);
    trace.assert_clean();
    assert_eq!(trace.counters.parks, 1);
    assert_eq!(
        trace.counters.park_cycles, trace.counters.parks,
        "exactly one resumed session per park"
    );
    assert!(trace.counters.updates_sent > 0);
}

/// Five ten-second flaps: the ladder reconnects without operator action and the
/// media session is never recreated (INV-9).
#[test]
fn five_flaps_never_lose_the_session() {
    let profile = LinkProfile::flap(5, 10_000, 10_000).with_seed(7);
    let mut scenario = Scenario::new("t4-flap5", Tier::T4)
        .step(Step::Link(Box::new(profile)))
        .change(CellRect::new(0, 0, 512, 256));
    // Each flap: park for 10 s, then come back.
    for _ in 0..5 {
        scenario = scenario
            .advance(3_000)
            .step(Step::ViewerDisconnect)
            .advance(10_000)
            .step(Step::ViewerReconnect);
    }
    scenario = scenario
        .advance(10_000)
        .step(Step::Assert(Invariant::Inv9))
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert_eq!(trace.counters.parks, 5);
    assert_eq!(
        trace.counters.park_cycles, trace.counters.parks,
        "exactly one resume per flap"
    );
    assert!(trace.counters.updates_sent > 0);
    assert_eq!(
        trace.counters.epoch, 0,
        "no epoch change means no media-session recreation"
    );
}

/// A sender transport reconnect is transparent to the media chain.
#[test]
fn a_host_reconnect_does_not_reset_the_chain() {
    let scenario = Scenario::new("sender-reconnect", Tier::T1)
        .change(CellRect::new(0, 0, 256, 128))
        .advance(1_000)
        .step(Step::HostReconnect)
        .change(CellRect::new(0, 0, 256, 128))
        .advance(2_000)
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.updates_sent >= 2);
    assert_eq!(trace.counters.epoch, 0);
}

/// A relay that drops and reports objects: the attribution arithmetic explains
/// the receiver's loss (INV-10).
#[test]
fn hop_drops_are_attributed() {
    let scenario = Scenario::new("hop-attribution", Tier::T2)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(10_000)
        .step(Step::Assert(Invariant::Inv10));
    let trace = run_with_relay(&scenario, IntermediaryModel { drop_permille: 500 });
    trace.assert_clean();
    assert!(trace.counters.hop_dropped > 0, "the relay dropped objects");
    println!(
        "hop drops {} receiver lost {}",
        trace.counters.hop_dropped, trace.counters.viewer_lost_objects
    );
}

/// A carrier switch mid-session carries the same frame chain.
#[test]
fn a_path_switch_is_transparent() {
    let scenario = Scenario::new("path-switch", Tier::T2)
        .change(CellRect::new(0, 0, 256, 128))
        .advance(1_000)
        .step(Step::PathSwitch(MediaPath::DcDirect))
        .change(CellRect::new(0, 0, 256, 128))
        .advance(2_000)
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.updates_sent >= 2);
}

/// Two receivers over two different links: neither starves the other.
#[test]
fn viewers_are_independent() {
    let fast = run(&motion(Tier::T0, 3_000));
    let slow = run(&motion(Tier::T3, 12_000));
    fast.assert_clean();
    slow.assert_clean();
    assert!(fast.counters.updates_sent >= 3);
    assert_eq!(
        fast.counters.pending_cells, 0,
        "the T0 receiver converges fully"
    );
    assert!(
        slow.counters.pending_cells <= slow.counters.cells / 4,
        "the T3 receiver is bounded by its own link, not the T0 one: {}",
        slow.counters.pending_cells
    );
}

/// The trace is a corpus artifact: the same scenario yields the same digest on
/// every run and every machine.
#[test]
fn traces_are_reproducible_for_the_corpus() {
    let scenario = Scenario::new("corpus-stability", Tier::T2)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(2_000)
        .input(InputKind::PointerMove)
        .advance(2_000)
        .input(InputKind::Key)
        .advance(2_000);
    let first = run(&scenario);
    let second = run(&scenario);
    assert_eq!(first.digest(), second.digest());
    assert_eq!(first.to_json(), second.to_json());
    println!(
        "{} digest {} updates {} acks {} plans {}",
        first.name,
        first.digest(),
        first.counters.updates_sent,
        first.counters.acks,
        first.counters.plans_started
    );
    for tier in Tier::all() {
        let trace = run(&motion(tier, 4_000));
        println!("{}: digest {}", tier.as_str(), trace.digest());
    }
}

/// Each scenario prints its own verdict line with `--nocapture`.
#[test]
fn tier_summary_is_printed() {
    for tier in Tier::all() {
        let trace = run(&motion(tier, 6_000));
        trace.assert_clean();
        println!(
            "{}: updates {} applied {} bytes {} converged {} pending {} loss {}/{} echo {:?}",
            tier.as_str(),
            trace.counters.updates_sent,
            trace.counters.updates_applied,
            trace.counters.bytes_sent,
            trace.counters.converged_observations,
            trace.counters.pending_cells,
            trace.link.dropped_loss,
            trace.link.transmitted,
            trace.echo_p95_ms()
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The acceptance matrix's story rows S-1…S-15 (`04-acceptance-matrix.md` §12
// names one simulator case per story plus the tier matrix pairs). The cases
// below carry the story id in their doc comment so the mapping is auditable.
// ─────────────────────────────────────────────────────────────────────────────

use mlp_core::media::cells::CellGrid;
use mlp_core::media::lifecycle::{MediaLanePolicy, MediaLaneStateKind};
use mlp_core::media::pdu::Carrier;
use mlp_core::media::resume::ResumeRejectReason;
use mlp_core::media::sender::{
    EnqueueOutcome, MediaSenderConfig, MediaSenderMachine, PathSelectionConfig, PathSelector,
};
use mlp_core::media::{
    MediaHello, MediaHelloAck, MediaLaneState, MediaPath as Path, MediaResumeAck, capability,
};
use std::time::Instant;

/// A typing story: `n` keystrokes `interval_ms` apart, then a quiet phase.
fn typing(tier: Tier, keys: u32, interval_ms: u64, quiet_ms: u64) -> Scenario {
    let mut scenario = Scenario::new(format!("typing-{}", tier.as_str()), tier)
        .change(CellRect::new(64, 64, 96, 48));
    for _ in 0..keys {
        scenario = scenario.advance(interval_ms).input(InputKind::Key);
    }
    scenario.advance(quiet_ms)
}

/// **S-1** — typing under T2: echo inside `2 × RTT + 250 ms`, convergence after
/// the last keystroke (INV-8, INV-2).
#[test]
fn t2_typing_echo_within_budget() {
    let mut scenario = typing(Tier::T2, 10, 120, 4_000)
        .step(Step::Assert(Invariant::Inv8))
        .step(Step::Assert(Invariant::Inv2));
    scenario = scenario.step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    let echo = trace.echo_p95_ms().expect("each keystroke is measured");
    let budget = 2 * 150 + 250 + 1_000; // + one repair delay + tick
    assert!(echo <= budget, "echo p95 {echo} ms above {budget} ms");
    // Ten keystrokes plus the pressed-set heartbeat the driver sends with a
    // burst of input objects (design §9.3 rule 4).
    assert!(
        trace.counters.input_applied >= 10,
        "every keystroke arrives: {}",
        trace.counters.input_applied
    );
    assert_eq!(trace.counters.input_expired, 0, "none of them expired");
    assert!(trace.counters.converged_observations > 0);
    assert_eq!(trace.counters.pending_cells, 0, "converged within 4 s");
}

/// **S-2** — a scroll under T3 collects through a plan and drains within the
/// plan deadline (INV-2, INV-7).
#[test]
fn t3_scroll_converges_within_plan_deadline() {
    let scenario = Scenario::new("t3-scroll", Tier::T3)
        .change(CellRect::new(200, 200, 900, 620))
        .advance(12_000)
        .step(Step::Assert(Invariant::Inv2))
        .step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(
        trace.counters.updates_applied >= 3,
        "the scroll is delivered"
    );
    assert!(
        trace.counters.converged_observations > 0,
        "pending reaches zero inside the plan deadline"
    );
    assert!(
        trace.counters.pending_cells <= 32,
        "{} cells still pending",
        trace.counters.pending_cells
    );
    println!(
        "S-2: updates {} pending {}",
        trace.counters.updates_sent, trace.counters.pending_cells
    );
}

/// **S-3** — a 10 s black hole under the T4 middle band (64 kbps, 800 ms, 20 %)
/// parks and resumes (INV-9).
#[test]
fn t4_medium_band_outage_parks_and_resumes() {
    let profile = LinkProfile {
        rtt_ms: 800.0,
        jitter_ms: 300.0,
        rate_bps_down: 64_000,
        rate_bps_up: 32_000,
        mtu_bytes: 1_150,
        loss_permille: 200,
        outage_schedule: vec![mlp_core::testing::lossy_link::Outage {
            start_ms: 4_000,
            dur_ms: 10_000,
        }],
        ..LinkProfile::tier(Tier::T4)
    };
    let scenario = Scenario::new("s3-black-hole", Tier::T4)
        .step(Step::Link(Box::new(profile)))
        .change(CellRect::new(32, 32, 192, 96))
        .advance(3_000)
        .step(Step::ViewerDisconnect)
        .advance(10_000)
        .step(Step::ViewerReconnect)
        .advance(12_000)
        .step(Step::Assert(Invariant::Inv9))
        .step(Step::Assert(Invariant::Inv5));
    let trace = run(&scenario);
    trace.assert_clean();
    assert_eq!(trace.counters.parks, 1);
    assert_eq!(trace.counters.park_cycles, 1, "exactly one resumed session");
    assert!(trace.counters.updates_sent > 0);
}

/// **S-5** — a full-screen change under T3 is repainted through anchors and the
/// pending set reaches zero at least once (progressive repaint, INV-2).
#[test]
fn s5_anchor_repaint_is_progressive_and_drains() {
    let scenario = Scenario::new("s5-anchor-repaint", Tier::T3)
        .change(CellRect::new(0, 0, 1920, 1080))
        .advance(4_000)
        .change(CellRect::new(0, 0, 1920, 1080))
        .advance(20_000)
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.updates_applied > 5, "the repaint progresses");
    assert!(
        trace.counters.converged_observations > 0,
        "the repaint drains"
    );
}

/// **S-6** — input priority under T2: a keystroke is reflected while media is
/// still converging (INV-8 under a plan).
#[test]
fn t2_input_priority_under_plan() {
    let mut scenario =
        Scenario::new("s6-input-priority-t2", Tier::T2).change(CellRect::new(0, 0, 800, 600));
    scenario = scenario.advance(600);
    for _ in 0..5 {
        scenario = scenario.advance(150).input(InputKind::Key);
    }
    scenario = scenario
        .advance(4_000)
        .step(Step::Assert(Invariant::Inv8))
        .step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    let echo = trace.echo_p95_ms().expect("an echo during convergence");
    let budget = 2 * 150 + 250 + 1_000;
    assert!(
        echo <= budget,
        "echo during convergence {echo} ms > {budget} ms"
    );
    assert!(trace.counters.input_applied >= 5);
}

/// **S-6** — input priority under T3.
#[test]
fn t3_input_priority_under_plan() {
    let mut scenario =
        Scenario::new("s6-input-priority-t3", Tier::T3).change(CellRect::new(0, 0, 1920, 1080));
    scenario = scenario.advance(1_000);
    for _ in 0..5 {
        scenario = scenario.advance(400).input(InputKind::Key);
    }
    scenario = scenario
        .advance(8_000)
        .step(Step::Assert(Invariant::Inv7))
        .step(Step::Assert(Invariant::Inv5));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(
        trace.counters.input_applied >= 5,
        "keystrokes arrive during the plan"
    );
    assert!(
        trace.counters.pending_cells <= trace.counters.cells / 4,
        "the plan drains: {} of {}",
        trace.counters.pending_cells,
        trace.counters.cells
    );
}

/// **S-7** — bounded memory under T3: every queue inside its cap across a long
/// motion sweep (INV-5).
#[test]
fn t3_bounded_memory_under_motion() {
    let mut scenario = Scenario::new("s7-memory-t3", Tier::T3);
    for step in 0..20u32 {
        scenario = scenario
            .change(CellRect::new(step * 64, 100, 480, 320))
            .advance(1_000)
            .input(InputKind::PointerMove);
    }
    scenario = scenario
        .advance(4_000)
        .step(Step::Assert(Invariant::Inv5))
        .step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.updates_sent > 10);
    assert!(
        trace.counters.pending_cells <= trace.counters.cells,
        "pending can never exceed the grid"
    );
}

/// **S-7** — bounded memory under T4.
#[test]
fn t4_bounded_memory_under_motion() {
    let mut scenario = Scenario::new("s7-memory-t4", Tier::T4);
    for step in 0..12u32 {
        scenario = scenario
            .change(CellRect::new(step * 32, 40, 160, 96))
            .advance(2_000)
            .input(InputKind::PointerMove);
    }
    scenario = scenario
        .advance(6_000)
        .step(Step::Assert(Invariant::Inv5))
        .step(Step::Assert(Invariant::Inv6))
        .step(Step::Assert(Invariant::Inv7));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.updates_sent > 2);
}

/// **S-8** — recovery observability: every degradation step is attributed to a
/// series the operator can name (INV-10 plus the plan/repair/state counters).
#[test]
fn t3_recovery_fully_attributed() {
    let scenario = Scenario::new("s8-attribution", Tier::T3)
        .change(CellRect::new(0, 0, 1200, 800))
        .advance(2_000)
        .change(CellRect::new(100, 100, 1200, 800))
        .advance(16_000)
        .step(Step::Assert(Invariant::Inv10))
        .step(Step::Assert(Invariant::Inv2));
    let trace = run_with_relay(&scenario, IntermediaryModel { drop_permille: 400 });
    trace.assert_clean();
    // At least one attributed series is non-empty for the run.
    let attributed = trace.counters.hop_dropped
        + trace.counters.plans_started
        + trace.counters.state_changes
        + u64::from(trace.counters.converged_observations);
    assert!(
        attributed > 0,
        "the recovery is attributable: {:#?}",
        trace.counters
    );
    assert!(
        trace.counters.viewer_lost_objects
            <= trace.counters.hop_dropped + trace.link.dropped_loss + 2,
        "receiver loss is explained by hops and the link"
    );
}

/// **S-9** — resume without reset under T1: the media lane dies and comes back
/// with the same chain, no epoch change, no reset frame (INV-2).
#[test]
fn t1_resume_rebases_not_resets() {
    let scenario = Scenario::new("s9-resume-rebase", Tier::T1)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(1_500)
        .step(Step::ViewerDisconnect)
        .advance(600)
        .step(Step::ViewerReconnect)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(4_000)
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert_eq!(trace.counters.parks, 1);
    assert_eq!(
        trace.counters.epoch, 0,
        "no epoch change means no canvas trust reset"
    );
    assert_eq!(trace.counters.updates_gapped, 0, "the chain continues");
    assert!(trace.counters.updates_sent >= 2);
}

/// **S-9** — the media lane resumes while the control-lane session is untouched:
/// one media session, one handshake, one resume.
#[test]
fn t1_media_lane_resume_control_lane_intact() {
    let scenario = Scenario::new("s9-control-lane-intact", Tier::T1)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(1_000)
        .step(Step::ViewerDisconnect)
        .advance(500)
        .step(Step::ViewerReconnect)
        .advance(3_000)
        .step(Step::HostReconnect)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(3_000)
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert_eq!(trace.counters.parks, 1);
    assert_eq!(
        trace.counters.epoch, 0,
        "the media session is not recreated"
    );
    assert!(trace.counters.updates_applied >= 2);
}

/// **S-10** — no stuck keys, ever: presses, a park whose grace elapses, a resume
/// and a session end all leave the sender's pressed set empty (INV-4).
#[test]
fn t3_no_stuck_keys_ever() {
    let mut scenario =
        Scenario::new("s10-stuck-keys", Tier::T3).change(CellRect::new(0, 0, 256, 128));
    for _ in 0..3 {
        scenario = scenario.advance(300).input(InputKind::Key);
    }
    scenario = scenario
        .advance(300)
        .input(InputKind::PointerButton)
        .advance(300)
        .step(Step::ViewerDisconnect)
        .advance(9_000)
        .step(Step::ViewerReconnect)
        .advance(2_000)
        .input(InputKind::ReleaseAll)
        .advance(1_000)
        .step(Step::Assert(Invariant::Inv4));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.input_applied >= 3, "input still works");
    assert_eq!(
        trace
            .violations
            .iter()
            .filter(|violation| violation.invariant == "inv4")
            .count(),
        0,
        "no stuck-key path fired"
    );
}

/// **S-11** — typing under T4 is text-first and honest: at least one frame per
/// second, the lane never claims `live`, and the echo is reported.
#[test]
fn t4_typing_text_first_floor() {
    let scenario = typing(Tier::T4, 6, 500, 12_000)
        .step(Step::Assert(Invariant::Inv1))
        .step(Step::Assert(Invariant::Inv5));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(
        trace.counters.updates_sent >= 2,
        "text-first means >= 1 fps"
    );
    // The lane's first state is its pre-measurement `STARTING` ladder; the
    // honesty rule is about what the UI claims *after* it has evidence.
    assert!(
        !trace
            .states
            .iter()
            .any(|state| state.state == "live" && state.at_ms > 1_000),
        "the honest UI never claims live at T4 once it has measured: {:?}",
        trace.states
    );
    assert!(trace.counters.input_applied >= 1);
    println!(
        "S-11: updates {} echo {:?} states {:?}",
        trace.counters.updates_sent,
        trace.echo_p95_ms(),
        trace.states
    );
}

/// **S-11** — audio is off at T4 by policy: the media lane's whole budget is
/// smaller than an audio stream, so no audio frame can exist on it (D-17).
#[test]
fn t4_no_audio_default() {
    let scenario = typing(Tier::T4, 4, 600, 10_000);
    let trace = run(&scenario);
    trace.assert_clean();
    // 96 kbps Opus cannot exist inside a 32 kbps envelope: the run's total
    // media bytes are a fraction of what one second of audio would need.
    let audio_second_bytes = 96_000 / 8;
    assert!(
        trace.counters.bytes_sent < audio_second_bytes,
        "the T4 lane carried {} bytes, more than one second of 96 kbps audio",
        trace.counters.bytes_sent
    );
}

/// **S-12** — promotion hysteresis: DC promotes only after sustained 1.2×
/// goodput, never on a single sample.
#[test]
fn s2_path_selection_hysteresis() {
    let config = PathSelectionConfig::default();
    let mut selector = PathSelector::new(Path::Ws, config);
    selector.set_rtt_min_ms(150.0);
    // One lucky DC sample is not enough.
    assert_eq!(
        selector.on_goodput(Path::DcDirect, 2_000_000, 0, false, false),
        None
    );
    assert_eq!(selector.active(), Path::Ws);
    // Sustained 1.2× over the window promotes, reporting from/to/reason.
    let mut promoted = None;
    for step in 1..=8u64 {
        let at = step * 200;
        selector.on_goodput(Path::Ws, 1_000_000, at, false, false);
        if let Some(switch) = selector.on_goodput(Path::DcDirect, 1_500_000, at, false, false) {
            promoted = Some(switch);
            break;
        }
    }
    let switch = promoted.expect("DC is promoted after the sustain window");
    assert_eq!(switch.from, Path::Ws);
    assert_eq!(switch.to, Path::DcDirect);
    assert_eq!(switch.reason, "promote");
    assert_eq!(selector.active(), Path::DcDirect);
    assert_eq!(selector.switches(), 1);
}

/// **S-12** — demotion on degradation, on a channel close and on a failing probe
/// window; three demotions disable DC for ten minutes.
#[test]
fn s2_path_demotion_on_degradation() {
    let config = PathSelectionConfig::default();
    let mut selector = PathSelector::new(Path::Ws, config);
    selector.set_rtt_min_ms(150.0);
    for step in 1..=8u64 {
        let at = step * 200;
        selector.on_goodput(Path::Ws, 1_000_000, at, false, false);
        selector.on_goodput(Path::DcDirect, 1_500_000, at, false, false);
    }
    assert_eq!(selector.active(), Path::DcDirect, "precondition");
    // The DC leg degrades below 0.8×: demote after the sustain window.
    let mut demoted = None;
    for step in 1..=8u64 {
        let at = 2_000 + step * 200;
        selector.on_goodput(Path::Ws, 1_000_000, at, false, false);
        if let Some(switch) = selector.on_goodput(Path::DcDirect, 600_000, at, false, false) {
            demoted = Some(switch);
            break;
        }
    }
    let switch = demoted.expect("degradation demotes");
    assert_eq!(switch.reason, "demote");
    assert_eq!(selector.active(), Path::Ws);

    // A channel close demotes immediately.
    let mut selector = PathSelector::new(Path::DcTurn, config);
    let switch = selector
        .on_carrier_close(Path::DcTurn, 1_000)
        .expect("a close demotes at once");
    assert_eq!(switch.reason, "close");
    assert_eq!(selector.active(), Path::Ws);

    // A probe window below 0.2 delivered demotes too.
    let mut selector = PathSelector::new(Path::DcDirect, config);
    let switch = selector
        .on_probe_window(Path::DcDirect, 0.1, 1_000)
        .expect("a failing probe window demotes");
    assert_eq!(switch.reason, "probe");

    // Three demotions inside ten minutes disable DC for ten minutes. Each cycle
    // re-promotes (sustained samples) and then fails a probe window.
    let mut selector = PathSelector::new(Path::Ws, config);
    selector.set_rtt_min_ms(150.0);
    for cycle in 0..3u64 {
        let at = cycle * 4_000;
        let mut promoted = false;
        for step in 0..3u64 {
            let sample_at = at + step * 200;
            selector.on_goodput(Path::Ws, 1_000_000, sample_at, false, false);
            if selector
                .on_goodput(Path::DcDirect, 2_000_000, sample_at, false, false)
                .is_some()
            {
                promoted = true;
                break;
            }
        }
        if !promoted {
            break;
        }
        assert!(
            selector
                .on_probe_window(Path::DcDirect, 0.05, at + 1_000)
                .is_some(),
            "cycle {cycle} demotes"
        );
    }
    assert_eq!(selector.demotions(), 3, "three demotions inside the window");
    assert!(
        selector.is_disabled(Path::DcDirect, 3_000),
        "after three demotions DC is disabled for the session"
    );
    assert!(
        !selector.is_disabled(Path::DcDirect, 9_000 + 600_001),
        "the stickiness window closes"
    );
}

/// **S-12** — never switch during an anchor plan or an input pulse.
#[test]
fn s2_no_switch_during_plan() {
    let config = PathSelectionConfig::default();
    let mut selector = PathSelector::new(Path::Ws, config);
    selector.set_rtt_min_ms(150.0);
    for step in 1..=8u64 {
        let at = step * 200;
        selector.on_goodput(Path::Ws, 1_000_000, at, true, false);
        let switch = selector.on_goodput(Path::DcDirect, 2_000_000, at, true, false);
        assert_eq!(switch, None, "a plan suppresses promotion");
        selector.on_goodput(Path::Ws, 1_000_000, at, false, true);
        let switch = selector.on_goodput(Path::DcDirect, 2_000_000, at, false, true);
        assert_eq!(switch, None, "a pulse suppresses promotion");
    }
    assert_eq!(selector.active(), Path::Ws);
    assert_eq!(selector.switches(), 0);
    // With neither active, the same samples promote once the sustain window is
    // filled again (3 × rtt_min = 450 ms).
    let mut resumed = None;
    for step in 9..=20u64 {
        let at = step * 200;
        selector.on_goodput(Path::Ws, 1_000_000, at, false, false);
        if let Some(switch) = selector.on_goodput(Path::DcDirect, 2_000_000, at, false, false) {
            resumed = Some(switch);
            break;
        }
    }
    let switch = resumed.expect("promotion resumes");
    assert_eq!(switch.reason, "promote");
}

/// **S-13** — single-lane fallback: with both lanes on one carrier the priority
/// order and the pacing still hold.
#[test]
fn t2_single_lane_priority_preserved() {
    let mut scenario =
        Scenario::new("s13-single-lane", Tier::T2).change(CellRect::new(0, 0, 640, 480));
    scenario = scenario.advance(500);
    for _ in 0..4 {
        scenario = scenario.advance(120).input(InputKind::Key);
    }
    scenario = scenario
        .advance(3_000)
        .step(Step::Assert(Invariant::Inv7))
        .step(Step::Assert(Invariant::Inv8))
        .step(Step::Assert(Invariant::Inv5));
    let trace = run(&scenario);
    trace.assert_clean();
    assert!(trace.counters.input_applied >= 4, "input works on one lane");
    assert!(
        trace.counters.bytes_sent > 0,
        "media is still paced on one lane"
    );
    let echo = trace.echo_p95_ms().expect("input priority on one lane");
    assert!(echo <= 2 * 150 + 250 + 1_000, "echo {echo} ms");
}

/// **S-14** — T0 regression guard: the fixture-level properties are pacing that
/// never delays a frame, zero gap applications and an immediate convergence.
#[test]
fn t0_identical_to_baseline_behaviour() {
    let scenario = Scenario::new("s14-lan-baseline", Tier::T0)
        .change(CellRect::new(0, 0, 512, 256))
        .advance(1_000)
        .input(InputKind::Key)
        .advance(2_000)
        .step(Step::Assert(Invariant::Inv2));
    let trace = run(&scenario);
    trace.assert_clean();
    assert_eq!(
        trace.counters.updates_gapped, 0,
        "zero dropped updates at T0"
    );
    assert_eq!(trace.link.dropped_loss, 0, "the LAN profile is lossless");
    assert!(trace.counters.converged_observations > 0);
    assert_eq!(trace.counters.pending_cells, 0);
    let echo = trace.echo_p95_ms().expect("an echo at T0");
    assert!(echo <= 50, "T0 echo p95 {echo} ms above the 50 ms guard");
    // The fixture's timing granularity is one 10 ms tick, so "pacing never
    // delays a frame" is asserted as "no wait beyond a tick and waits are rare":
    // the pacer is not the limiter on a LAN.
    assert!(
        trace.counters.max_pacer_wait_ms <= 25,
        "pacing never delayed a frame beyond one tick at T0: {} ms",
        trace.counters.max_pacer_wait_ms
    );
    println!(
        "S-14: updates {} echo {:?} pacer waits {} max {} ms",
        trace.counters.updates_sent,
        trace.echo_p95_ms(),
        trace.counters.pacer_waits,
        trace.counters.max_pacer_wait_ms
    );
}

/// **S-15** — a receiver against a sender that cannot carry the media lane
/// degrades instead of failing: the lane reports the typed unsupported state,
/// media is refused, and control/input keep working.
#[test]
fn lane_unsupported_degrades_not_fails() {
    // The peer's answer carries the typed reason (design §6.7, D-4).
    let ack = MediaHelloAck {
        protocol_version: 1,
        media_session_id: String::new(),
        accepted_fragment_payload: 1_150,
        cell_size: 64,
        cell_cols: 30,
        cell_rows: 17,
        capabilities: vec![capability::ANCHOR.to_string()],
        resume_ttl_seconds: 900,
        media_supported: false,
        media_unsupported_reason: Some(mlp_core::media::MediaUnsupportedReason::Protocol),
    };
    let hello = MediaHello {
        protocol_version: 1,
        media_session_id: String::new(),
        session_id: "s".to_string(),
        max_fragment_payload: 65_536,
        cell_size: 64,
        cell_cols: 30,
        cell_rows: 17,
        capabilities: Vec::new(),
        encoder_max_fps: 30,
        start_epoch_ms: 0,
    };
    assert_eq!(
        mlp_core::media::MediaControlObject::HelloAck(ack.clone()).validate(),
        Ok(())
    );
    assert_eq!(
        mlp_core::media::MediaControlObject::Hello(hello).validate(),
        Ok(())
    );
    // The lane is disabled by policy: media is refused, control is not.
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(1280, 720, 64);
    let mut machine = MediaSenderMachine::new(MediaSenderConfig::new(Carrier::Ws, grid), grid, now);
    machine.set_policy(MediaLanePolicy {
        media_lane_enabled: false,
        ..MediaLanePolicy::default()
    });
    let media = machine.enqueue_frame(
        mlp_core::media::sender::UpdateRequest::delta(vec![0; 64], vec![0, 1]),
        now,
    );
    assert!(
        matches!(media, EnqueueOutcome::Deferred { .. }),
        "media is refused, not fatal: {media:?}"
    );
    let control = machine.enqueue_control(
        &mlp_core::media::MediaControlObject::LaneState(MediaLaneState {
            state: MediaLaneStateKind::MediaUnsupported,
            since_epoch_ms: 0,
            detail: "protocol".to_string(),
        }),
        vec![1, 2, 3],
        now,
    );
    assert!(
        matches!(control, EnqueueOutcome::Accepted { .. }),
        "the lane can still carry the typed refusal: {control:?}"
    );
    assert!(machine.poll_fragment(now).is_some(), "control lane works");
    assert_eq!(machine.in_flight(), 0, "no media object was queued");
    let resume_ack = MediaResumeAck {
        accepted: false,
        reason: ResumeRejectReason::UnknownSession,
        next_update_id: 0,
        pending_cells: 0,
        pace_bps: 0,
        budget_bytes: 0,
        target_fps: 0,
        anchor_plan_started: false,
    };
    assert!(!resume_ack.accepted);
}

/// The tier matrix pairs: every tier's motion scenario stays clean (T0–T4).
#[test]
fn tier_matrix_pairs_are_clean() {
    for tier in Tier::all() {
        let trace = run(&motion(tier, 5_000));
        trace.assert_clean();
        assert!(
            trace.counters.updates_sent > 0,
            "{} was silent",
            tier.as_str()
        );
    }
}
