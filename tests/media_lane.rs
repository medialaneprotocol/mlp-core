//! The media lane's own policy: the path it takes, and what it does when
//! the peer cannot carry it.
//!
//! These are the core's own decisions, asserted on the core's own
//! machines: the path selector's hysteresis, its demotions and its
//! stickiness, and the lane's refusal of media on a peer that cannot carry
//! it. The scenario-level stories live with the scenarios that carry them.

use std::time::Instant;

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

/// Promotion hysteresis: the direct carrier is promoted only after sustained
/// goodput, never on a single sample.
#[test]
fn path_selection_promotes_only_after_a_sustained_window() {
    let config = PathSelectionConfig::default();
    let mut selector = PathSelector::new(Path::Ws, config);
    selector.set_rtt_min_ms(150.0);
    // One lucky sample is not enough.
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
    let switch = promoted.expect("the direct carrier is promoted after the sustain window");
    assert_eq!(switch.from, Path::Ws);
    assert_eq!(switch.to, Path::DcDirect);
    assert_eq!(switch.reason, "promote");
    assert_eq!(selector.active(), Path::DcDirect);
    assert_eq!(selector.switches(), 1);
}

/// Demotion on degradation, on a channel close and on a failing probe
/// window; three demotions disable the direct carrier for ten minutes.
#[test]
fn path_demotion_on_degradation_a_close_and_a_failing_probe() {
    let config = PathSelectionConfig::default();
    let mut selector = PathSelector::new(Path::Ws, config);
    selector.set_rtt_min_ms(150.0);
    for step in 1..=8u64 {
        let at = step * 200;
        selector.on_goodput(Path::Ws, 1_000_000, at, false, false);
        selector.on_goodput(Path::DcDirect, 1_500_000, at, false, false);
    }
    assert_eq!(selector.active(), Path::DcDirect, "precondition");
    // The direct leg degrades below 0.8×: demote after the sustain window.
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

    // Three demotions inside ten minutes disable the direct carrier for ten
    // minutes. Each cycle re-promotes (sustained samples) and then fails a
    // probe window.
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
        "after three demotions the direct carrier is disabled for the session"
    );
    assert!(
        !selector.is_disabled(Path::DcDirect, 9_000 + 600_001),
        "the stickiness window closes"
    );
}

/// The selector never switches during an anchor plan or an input pulse.
#[test]
fn a_plan_or_a_pulse_suppresses_the_switch() {
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
    // With neither active, the same samples promote once the sustain window
    // is filled again (3 × rtt_min = 450 ms).
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

/// A receiver against a sender that cannot carry the media lane degrades
/// instead of failing: the lane reports the typed unsupported state, media is
/// refused, and control and input keep working.
#[test]
fn a_lane_that_cannot_carry_the_media_degrades_instead_of_failing() {
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
    let media = machine.enqueue_update(
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
