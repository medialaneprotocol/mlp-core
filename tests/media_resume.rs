//! Session continuity: resume, parking, rebase and the reconnect ladder
//! (design §8.1–§8.4, D-5) — including INV-9's structural guarantees.

use std::time::Duration;

use mlp_core::media::MediaPath;
use mlp_core::media::input::InputToken;
use mlp_core::media::resume::{
    BOOTSTRAP_AFTER_ATTEMPTS, HeldViewerState, PARK_TTL_CEIL_SECONDS, PARK_TTL_FLOOR_SECONDS,
    ParkEntry, ParkPolicy, ParkTable, RECONNECT_CAP_MS, RECONNECT_JITTER_MS, RESUME_TTL_SECONDS,
    ReconnectLadder, ReconnectStage, ResumeAction, ResumeBinding, ResumeClaims, ResumeRejectReason,
    ResumeRequest, SUSPENDED_AFTER_ATTEMPTS, TerminalReason, decide_resume,
};
use mlp_core::media::scheduler::PlanStartReason;

fn claims() -> ResumeClaims {
    ResumeClaims {
        version: ResumeClaims::VERSION,
        session_id: "sess-1".into(),
        viewer_id: "receiver-1".into(),
        host_id: "sender-1".into(),
        media_session_id: "media-1".into(),
        epoch: 3,
        issued_at_secs: 1_000,
        expires_at_secs: 1_000 + RESUME_TTL_SECONDS,
    }
}

fn binding() -> ResumeBinding {
    ResumeBinding {
        session_id: "sess-1".into(),
        viewer_id: "receiver-1".into(),
        host_id: "sender-1".into(),
    }
}

fn request(viewer_epoch: u32) -> ResumeRequest {
    ResumeRequest {
        claims: claims(),
        binding: binding(),
        viewer_epoch,
        applied_update_id: 40,
        ack_bitmap: 0b1,
        input_seq_next: 100,
        pressed_keys: vec![16],
        pressed_buttons: vec![1],
        active_path: MediaPath::Ws,
    }
}

fn held() -> HeldViewerState {
    HeldViewerState {
        epoch: 3,
        next_update_id: 41,
        last_sent_update_id: 40,
        pending_cells: 0,
        pressed: vec![InputToken::Key(16), InputToken::Button(1)],
        input_seq_next: 90,
        active_path: MediaPath::Ws,
        pace_bps: 64_000,
        budget_bytes: 2_000,
        target_fps: 30.0,
    }
}

#[test]
fn resume_tokens_bind_the_session_the_viewer_and_the_host() {
    assert_eq!(claims().validate_at(1_100, &binding()), Ok(()));
    let mut wrong_viewer = claims();
    wrong_viewer.viewer_id = "someone-else".into();
    assert_eq!(
        wrong_viewer.validate_at(1_100, &binding()),
        Err(ResumeRejectReason::PeerMismatch)
    );
    let mut wrong_host = claims();
    wrong_host.host_id = "sender-2".into();
    assert_eq!(
        wrong_host.validate_at(1_100, &binding()),
        Err(ResumeRejectReason::PeerMismatch)
    );
    let mut wrong_session = claims();
    wrong_session.session_id = "sess-2".into();
    assert_eq!(
        wrong_session.validate_at(1_100, &binding()),
        Err(ResumeRejectReason::PeerMismatch)
    );
    // `exp` is exclusive.
    assert_eq!(
        claims().validate_at(1_900, &binding()),
        Err(ResumeRejectReason::TokenExpired)
    );
    assert_eq!(claims().validate_at(1_899, &binding()), Ok(()));
    // The version is part of the token.
    let mut old = claims();
    old.version = 1;
    assert_eq!(
        old.validate_at(1_100, &binding()),
        Err(ResumeRejectReason::TokenInvalid)
    );
}

#[test]
fn resume_reasons_round_trip_as_wire_strings() {
    for reason in [
        ResumeRejectReason::Ok,
        ResumeRejectReason::UnknownSession,
        ResumeRejectReason::TokenExpired,
        ResumeRejectReason::TokenInvalid,
        ResumeRejectReason::PeerMismatch,
        ResumeRejectReason::EpochStale,
        ResumeRejectReason::EpochMismatch,
        ResumeRejectReason::ParkEvicted,
    ] {
        assert_eq!(ResumeRejectReason::parse(reason.as_str()), Some(reason));
    }
    assert!(ResumeRejectReason::ParkEvicted.starts_fresh_media_session());
    assert!(ResumeRejectReason::EpochStale.starts_fresh_media_session());
    assert!(!ResumeRejectReason::TokenExpired.starts_fresh_media_session());
}

#[test]
fn the_signing_bytes_are_canonical() {
    let first = claims().to_signing_bytes();
    assert_eq!(first, claims().to_signing_bytes());
    for mutate in [
        |claims: &mut ResumeClaims| claims.session_id = "x".into(),
        |claims: &mut ResumeClaims| claims.viewer_id = "x".into(),
        |claims: &mut ResumeClaims| claims.host_id = "x".into(),
        |claims: &mut ResumeClaims| claims.media_session_id = "x".into(),
        |claims: &mut ResumeClaims| claims.epoch += 1,
        |claims: &mut ResumeClaims| claims.expires_at_secs += 1,
    ] {
        let mut other = claims();
        mutate(&mut other);
        assert_ne!(first, other.to_signing_bytes());
    }
}

#[test]
fn a_clean_resume_rebases_and_keeps_the_chain() {
    let mut state = held();
    let outcome = decide_resume(&request(3), 1_100, &mut state, true, false);
    assert!(outcome.accepted);
    assert_eq!(outcome.reason, ResumeRejectReason::Ok);
    assert_eq!(outcome.next_update_id, 41, "no update id restart");
    assert_eq!(outcome.pending_cells, 0);
    assert!(!outcome.anchor_plan_started);
    assert_eq!(
        outcome.actions[0],
        ResumeAction::Rebase { pending_cells: 0 }
    );
    assert_eq!(state.input_seq_next, 100, "the dedup boundary continues");
}

#[test]
fn pending_blocks_start_a_plan_without_a_reset_update() {
    let mut state = held();
    state.pending_cells = 37;
    let outcome = decide_resume(&request(3), 1_100, &mut state, true, false);
    assert!(outcome.accepted);
    assert!(outcome.anchor_plan_started);
    assert!(outcome.actions.contains(&ResumeAction::StartAnchorPlan {
        reason: PlanStartReason::Pending
    }));
    assert_eq!(outcome.pending_cells, 37, "diagnostics only");
}

#[test]
fn an_epoch_change_attaches_with_a_plan_and_keeps_the_pixels() {
    let mut state = held();
    let outcome = decide_resume(&request(9), 1_100, &mut state, true, false);
    assert!(outcome.accepted, "a stale epoch never kills the session");
    assert!(outcome.anchor_plan_started);
    assert!(outcome.actions.contains(&ResumeAction::StartAnchorPlan {
        reason: PlanStartReason::Epoch
    }));
    assert_eq!(state.epoch, 9);
}

#[test]
fn an_evicted_park_starts_a_fresh_media_session_for_the_same_viewer() {
    let mut state = held();
    let outcome = decide_resume(&request(3), 1_100, &mut state, false, true);
    assert!(!outcome.accepted);
    assert_eq!(outcome.reason, ResumeRejectReason::ParkEvicted);
    assert_eq!(state.epoch, 4, "chain_epoch++ for the fresh session");
    assert!(matches!(
        outcome.actions.last(),
        Some(ResumeAction::FreshMediaSession { epoch: 4 })
    ));
}

#[test]
fn an_unknown_session_or_expired_token_never_attaches() {
    let mut state = held();
    let unknown = decide_resume(&request(3), 1_100, &mut state, false, false);
    assert_eq!(unknown.reason, ResumeRejectReason::UnknownSession);
    assert!(!unknown.accepted);
    assert!(unknown.actions.is_empty());

    let mut expired = request(3);
    expired.claims.expires_at_secs = 500;
    let outcome = decide_resume(&expired, 1_100, &mut state, true, false);
    assert_eq!(outcome.reason, ResumeRejectReason::TokenExpired);
    assert!(!outcome.accepted);
}

#[test]
fn resume_releases_keys_the_viewer_no_longer_holds() {
    let mut state = held();
    let mut incoming = request(3);
    incoming.pressed_keys = vec![16];
    incoming.pressed_buttons = vec![];
    let outcome = decide_resume(&incoming, 1_100, &mut state, true, false);
    let releases: Vec<Vec<InputToken>> = outcome
        .actions
        .iter()
        .filter_map(|action| match action {
            ResumeAction::ReleaseKeys { tokens } => Some(tokens.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(releases, vec![vec![InputToken::Button(1)]]);
    assert_eq!(state.pressed, vec![InputToken::Key(16)]);
}

#[test]
fn park_ttl_is_derived_from_the_outage_target() {
    assert_eq!(ParkPolicy::park_ttl_for(30, 2_000.0), 120, "T4 floor");
    assert_eq!(ParkPolicy::park_ttl_for(10, 400.0), 120);
    assert_eq!(ParkPolicy::park_ttl_for(120, 100.0), 300);
    assert_eq!(ParkPolicy::park_ttl_for(600, 10.0), 900, "ceiling");
    assert_eq!(PARK_TTL_FLOOR_SECONDS, 120);
    assert_eq!(PARK_TTL_CEIL_SECONDS, 900);
    assert!(ParkPolicy::default().park_ttl_seconds >= 120);
}

#[test]
fn the_park_table_bounds_its_state_and_counts_evictions() {
    let policy = ParkPolicy {
        park_ttl_seconds: 300,
        max_parked_sessions: 2,
        ..ParkPolicy::default()
    };
    let entry = |id: &str, at: u64| ParkEntry {
        media_session_id: id.into(),
        session_id: "s".into(),
        viewer_id: "v".into(),
        host_id: "h".into(),
        parked_at_secs: at,
        epoch: 1,
    };
    let mut table = ParkTable::new(2);
    table.park(entry("m1", 0), 0, &policy);
    table.park(entry("m2", 1), 1, &policy);
    let evicted = table.park(entry("m3", 2), 2, &policy);
    assert_eq!(evicted.len(), 1);
    assert_eq!(evicted[0].media_session_id, "m1");
    assert_eq!(table.len(), 2);
    // The key is the media session id: it survives a sender reconnect.
    assert!(table.get("m3").is_some());
    assert!(table.resume("m3").is_some());
    assert!(table.get("m3").is_none());
    // TTL eviction.
    let expired = table.evict_expired(1 + 300, &policy);
    assert_eq!(expired.len(), 1);
    assert!(table.is_empty());
    assert_eq!(
        table.evictions(mlp_core::media::resume::ParkEvictionReason::Ttl),
        1
    );
    assert_eq!(
        table.evictions(mlp_core::media::resume::ParkEvictionReason::Capacity),
        1
    );
}

#[test]
fn the_reconnect_ladder_is_rtt_relative_and_unbounded() {
    let slow = ReconnectLadder::new(2_000.0);
    assert_eq!(slow.base_ms(), 2_000);
    assert_eq!(slow.delay_ms(0, 0), 0);
    assert_eq!(slow.delay_ms(1, 0), 2_000);
    assert_eq!(slow.delay_ms(2, 0), 4_000);
    assert_eq!(slow.delay_ms(3, 0), 8_000);
    assert_eq!(slow.delay_ms(4, 0), 16_000);
    assert_eq!(slow.delay_ms(5, 0), 30_000, "the cap");
    assert_eq!(slow.delay_ms(6, 0), 30_000);
    assert_eq!(slow.delay_ms(100, 0), 30_000, "retries never stop");
    assert_eq!(slow.delay_ms(1, 10_000), 2_000 + RECONNECT_JITTER_MS);
    assert_eq!(RECONNECT_CAP_MS, 30_000);
    let fast = ReconnectLadder::new(20.0);
    assert_eq!(fast.base_ms(), 500, "the floor");
    assert_eq!(fast.delay_ms(1, 0), 500);
}

#[test]
fn the_ladder_escalates_through_bootstrap_to_suspended() {
    let ladder = ReconnectLadder::new(200.0);
    assert_eq!(ladder.stage(0), ReconnectStage::ControlRetry);
    assert_eq!(ladder.stage(2), ReconnectStage::ControlRetry);
    assert_eq!(ladder.stage(3), ReconnectStage::Bootstrap);
    assert_eq!(ladder.stage(5), ReconnectStage::Bootstrap);
    assert_eq!(ladder.stage(6), ReconnectStage::Suspended);
    assert_eq!(ladder.stage(500), ReconnectStage::Suspended);
    assert_eq!(BOOTSTRAP_AFTER_ATTEMPTS, 3);
    assert_eq!(SUSPENDED_AFTER_ATTEMPTS, 6);
    for reason in [
        TerminalReason::Unauthorized,
        TerminalReason::HostRetired,
        TerminalReason::OperatorDisconnect,
        TerminalReason::UnsupportedMediaVersion,
        TerminalReason::ParkTtlExpiredTwice,
        TerminalReason::OperatorAction,
    ] {
        assert!(!ladder.retries_for(reason));
    }
}

/// The design's §8.4 guarantee: a five-flap outage pattern fits inside the TTL.
#[test]
fn five_ten_second_flaps_fit_inside_the_ladder_and_the_park_ttl() {
    let down_ms = 10_000u64;
    let flaps = 5u64;
    for rtt in [400.0f64, 2_000.0] {
        let ladder = ReconnectLadder::new(rtt);
        let mut elapsed = 0u64;
        let mut attempt = 0u32;
        while elapsed < flaps * down_ms {
            attempt += 1;
            elapsed += ladder.delay_ms(attempt, RECONNECT_JITTER_MS);
        }
        let park_ttl_ms = ParkPolicy::park_ttl_for(30, rtt) * 1_000;
        assert!(
            elapsed <= park_ttl_ms,
            "rtt {rtt}: the ladder needs {elapsed} ms, the park holds {park_ttl_ms} ms"
        );
        assert!(elapsed >= flaps * down_ms);
    }
}

#[test]
fn t4_recovery_arithmetic_holds() {
    // N-4: `idle_timeout ≥ 2 × outage_target + 4 × RTT` for T4's 30 s.
    let outage = 30u64;
    let rtt_ms = 2_000u64;
    let idle = 2 * outage + 4 * rtt_ms / 1_000;
    assert!(idle >= 68, "the documented floor is 68 s");
    let park = ParkPolicy::park_ttl_for(outage, rtt_ms as f64);
    assert!(park >= 120, "the park window covers the ladder");
    assert!(Duration::from_secs(park) >= Duration::from_secs(outage));
}

#[test]
fn resume_carries_the_held_state_diagnostics() {
    let mut state = held();
    state.pace_bps = 256_000;
    state.budget_bytes = 8_000;
    state.target_fps = 5.0;
    let outcome = decide_resume(&request(3), 1_100, &mut state, true, false);
    // The receiver learns the sender's current budget straight away (design §6.7).
    assert_eq!(outcome.pace_bps, 256_000);
    assert_eq!(outcome.budget_bytes, 8_000);
    assert_eq!(outcome.target_fps, 5);
}
