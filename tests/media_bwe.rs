//! Bandwidth estimation (design §7.2, D-2): the algorithm, its constants and
//! the RTT-relative behaviour the whole timeout updatework depends on.

use std::time::{Duration, Instant};

use mlp_core::media::bwe::{BandwidthEstimator, BweConfig, BweMode, PriorStore, ProbeOutcome};
use mlp_core::media::{MediaFeedback, MediaHopReport, ProbeEcho, hop};

fn feedback(rtt_ms: u32, bytes: u64, objects: u64, lost: u64, window_ms: u64) -> MediaFeedback {
    MediaFeedback {
        rtt_ms,
        rtt_min_ms: rtt_ms,
        received_bytes: bytes,
        received_objects: objects,
        lost_objects: lost,
        window_ms,
        ..MediaFeedback::default()
    }
}

fn healthy(rtt_ms: u32, goodput_bps: u64) -> MediaFeedback {
    let bytes = goodput_bps / 8;
    feedback(rtt_ms, bytes, 100, 0, 1_000)
}

fn lossy(rtt_ms: u32, goodput_bps: u64, loss: f64) -> MediaFeedback {
    let bytes = goodput_bps / 8;
    let lost = (loss * 100.0).round() as u64;
    feedback(rtt_ms, bytes, 100, lost, 1_000)
}

/// The sender's own round trip, as a probe echo measures it.
fn round_trip(estimator: &mut BandwidthEstimator, rtt_ms: f64, now: Instant) {
    estimator.on_rtt_sample(rtt_ms, now);
}

/// A window in which the sender sent at its own pace: the window carries a
/// capacity signal, so a loss or a queue delay may decay the estimate.
fn sent_at_pace(estimator: &mut BandwidthEstimator, window_ms: u64, now: Instant) {
    let bytes = (estimator.pace_bps() / 8) as usize * (window_ms as usize / 1_000);
    estimator.on_send(bytes.max(1), 1, now);
}

#[test]
fn startup_doubles_per_report_then_settles() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    assert_eq!(estimator.mode(), BweMode::StartUp);
    assert_eq!(estimator.pace_bps(), 8_000, "no prior starts at the floor");
    let mut paces = vec![estimator.pace_bps()];
    for _ in 0..4 {
        estimator.on_feedback(&healthy(10, 10_000_000), now);
        paces.push(estimator.pace_bps());
    }
    // 0.9 × btlbw where btlbw is the 10 Mbps goodput.
    assert_eq!(paces, vec![8_000, 16_000, 32_000, 64_000, 9_000_000]);
    assert_eq!(
        estimator.mode(),
        BweMode::Steady,
        "the fourth report exits StartUp"
    );
    assert!(estimator.pace_bps() <= 10_000_000);
}

#[test]
fn loss_above_two_percent_decays_btlbw_and_five_percent_degrades() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..4 {
        estimator.on_feedback(&healthy(50, 2_000_000), now);
    }
    let steady = estimator.pace_bps();
    let btlbw = estimator.btlbw_bps();
    // 3 % loss at the same goodput: the decay is re-raised by the window's
    // goodput, so btlbw holds instead of growing, and the mode stays Steady.
    sent_at_pace(&mut estimator, 1_000, now);
    estimator.on_feedback(&lossy(50, 2_000_000, 0.03), now);
    assert!(estimator.btlbw_bps() <= btlbw);
    assert_eq!(estimator.mode(), BweMode::Steady);
    assert!(estimator.pace_bps() <= steady);
    // When the loss has actually cost throughput, btlbw decays.
    sent_at_pace(&mut estimator, 1_000, now);
    estimator.on_feedback(&lossy(50, 1_500_000, 0.03), now);
    assert!(
        estimator.btlbw_bps() < btlbw,
        "decay applies to the lower goodput"
    );
    // 10 % loss: Degraded, and the pace steps down by 30 % per report.
    sent_at_pace(&mut estimator, 1_000, now);
    estimator.on_feedback(&lossy(50, 1_000_000, 0.10), now);
    assert_eq!(estimator.mode(), BweMode::Degraded);
    let degraded = estimator.pace_bps();
    sent_at_pace(&mut estimator, 1_000, now);
    estimator.on_feedback(&lossy(50, 1_000_000, 0.10), now);
    assert!(estimator.pace_bps() <= (degraded as f64 * 0.71) as u64 + 1);
}

#[test]
fn three_healthy_reports_climb_back_out_of_degraded() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..4 {
        estimator.on_feedback(&healthy(50, 2_000_000), now);
    }
    sent_at_pace(&mut estimator, 1_000, now);
    estimator.on_feedback(&lossy(50, 1_000_000, 0.20), now);
    assert!(matches!(
        estimator.mode(),
        BweMode::Degraded | BweMode::Floor
    ));
    for _ in 0..3 {
        sent_at_pace(&mut estimator, 1_000, now);
        estimator.on_feedback(&healthy(50, 1_000_000), now);
    }
    assert_eq!(estimator.mode(), BweMode::StartUp);
}

#[test]
fn the_floor_is_never_crossed() {
    let now = Instant::now();
    // A link that keeps delivering despite 60 % loss: the goodput term holds
    // the pace at what the link demonstrably carries, never below the floor.
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..100 {
        estimator.on_feedback(&lossy(1_400, 10_000, 0.6), now);
    }
    assert!(matches!(
        estimator.mode(),
        BweMode::Degraded | BweMode::Floor
    ));
    assert!(
        (8_000..=12_000).contains(&estimator.pace_bps()),
        "pace {}",
        estimator.pace_bps()
    );
    // A link that delivers nothing at all reaches the floor and stays there.
    let mut dead = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..100 {
        dead.on_feedback(&feedback(1_400, 0, 0, 50, 1_000), now);
    }
    assert_eq!(dead.mode(), BweMode::Floor);
    assert_eq!(dead.pace_bps(), 8_000);
    let estimate = dead.estimate();
    assert_eq!(estimate.pace_bps, 8_000);
    assert!(estimate.budget_bytes >= 256);
    assert!(estimate.budget_bytes <= 1_048_576);
}

#[test]
fn no_feedback_uses_the_emergency_profile_and_one_report_clears_it() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    round_trip(&mut estimator, 400.0, now);
    estimator.on_feedback(&healthy(400, 256_000), now);
    let grace = estimator.no_feedback_grace();
    assert_eq!(grace, Duration::from_millis(3 * 1_600 + 2_000));
    // The deadline is exclusive: exactly at the grace the report is still late
    // but not yet counted as a stall.
    estimator.on_timeout(now + grace);
    assert_ne!(estimator.mode(), BweMode::NoFeedback);
    estimator.on_timeout(now + grace + Duration::from_millis(1));
    assert_eq!(estimator.mode(), BweMode::NoFeedback);
    let estimate = estimator.estimate();
    assert_eq!(estimate.pace_bps, 16_000, "floor × 2");
    assert_eq!(estimate.target_fps, 1.0);
    assert_eq!(estimate.max_object_bytes, 1_024);
    assert!(!estimate.mode.allows_speculative_refresh());
    estimator.on_feedback(&healthy(400, 256_000), now + grace);
    assert_ne!(estimator.mode(), BweMode::NoFeedback);
}

#[test]
fn a_cold_session_is_not_no_feedback() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    estimator.on_timeout(now + Duration::from_secs(120));
    assert_eq!(estimator.mode(), BweMode::StartUp);
}

#[test]
fn budgets_follow_the_design_formulas() {
    let now = Instant::now();
    // 100 Mbps ceiling, T0-ish RTT: 30 fps and a budget of one update interval.
    let mut fast = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..8 {
        fast.on_feedback(&healthy(5, 10_000_000), now);
    }
    let estimate = fast.estimate();
    assert_eq!(estimate.target_fps, 30.0);
    let expected_budget = (estimate.pace_bytes_per_sec() * (1_000.0 / 30.0) / 1_000.0) as u32;
    assert!((estimate.budget_bytes as i64 - i64::from(expected_budget)).abs() <= 1);
    let expected_max_object = (estimate.pace_bytes_per_sec() * (5.0f64 / 1_000.0).max(0.5)) as u32;
    assert!(
        (estimate.max_object_bytes as i64 - i64::from(expected_max_object.min(524_288))).abs() <= 2,
        "{} vs {}",
        estimate.max_object_bytes,
        expected_max_object
    );

    // 32 kbps at 1.4 s RTT: one fps, a budget below 512 B, objects of the RTT's
    // worth of airtime.
    let mut slow = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..4 {
        slow.on_feedback(&healthy(1_400, 32_000), now);
    }
    let estimate = slow.estimate();
    assert_eq!(estimate.target_fps, 1.0, "live floors fps at 1");
    assert!(
        estimate.budget_bytes <= 4_000,
        "one second at the doubled start-up pace (32 kbps), got {}",
        estimate.budget_bytes
    );
    assert!(estimate.max_object_bytes >= 1_024);
    assert!(estimate.max_object_bytes <= 524_288);
}

#[test]
fn max_object_bytes_is_about_one_rtt_of_airtime() {
    let now = Instant::now();
    for (rtt_ms, pace_bps) in [(50u32, 5_000_000u64), (400, 256_000), (1_400, 64_000)] {
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for _ in 0..4 {
            estimator.on_feedback(&healthy(rtt_ms, pace_bps), now);
        }
        let estimate = estimator.estimate();
        let rtt_seconds = (f64::from(rtt_ms) / 1_000.0).max(0.5);
        let airtime = estimate.max_object_bytes as f64 / estimate.pace_bytes_per_sec();
        assert!(
            airtime <= rtt_seconds * 1.05 + 0.01,
            "rtt {rtt_ms}: {airtime:.2}s of airtime"
        );
    }
}

#[test]
fn probes_follow_the_documented_trigger_and_response() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..6 {
        estimator.on_feedback(&healthy(100, 1_000_000), now);
    }
    assert_eq!(estimator.mode(), BweMode::Steady);
    let before = estimator.btlbw_bps();
    let probe = estimator.probe_due(now).expect("the first probe");
    // `payload_bytes = min(8 192, 0.25 × pace/8)`.
    assert!(probe.payload_bytes <= 8_192);
    assert!(probe.rate_bps <= (1.25 * before as f64) as u64 + 1);
    assert!(
        estimator
            .probe_due(now + Duration::from_millis(500))
            .is_none()
    );
    estimator.on_probe_result(
        &ProbeOutcome {
            delivered: true,
            delivered_ratio: 1.0,
            probe_rate_bps: before * 3 / 2,
        },
        now,
    );
    assert!(estimator.btlbw_bps() >= before);
    let (sent, failed) = estimator.probe_counters();
    assert_eq!((sent, failed), (1, 0));
    estimator.on_probe_result(
        &ProbeOutcome {
            delivered: false,
            delivered_ratio: 0.0,
            probe_rate_bps: before,
        },
        now,
    );
    assert_eq!(estimator.probe_counters().1, 1);
    let after = estimator.btlbw_bps();
    estimator.on_probe_result(
        &ProbeOutcome {
            delivered: false,
            delivered_ratio: 0.0,
            probe_rate_bps: after,
        },
        now,
    );
    assert_eq!(estimator.btlbw_bps(), (after as f64 * 0.9).round() as u64);
}

#[test]
fn priors_shorten_the_cold_start() {
    let now = Instant::now();
    let cold = BandwidthEstimator::new(BweConfig::default());
    assert_eq!(cold.pace_bps(), 8_000);
    let warm = BandwidthEstimator::new(BweConfig::default().with_prior(Some(1_000_000)));
    assert_eq!(warm.pace_bps(), 250_000, "prior / 4");
    // The remembered value is capped at 2 Mbps so a stale prior cannot flood.
    let capped = BandwidthEstimator::new(BweConfig::default().with_prior(Some(80_000_000)));
    assert_eq!(capped.pace_bps(), 2_000_000);
    // A prior below the floor is clamped up.
    let tiny = BandwidthEstimator::new(BweConfig::default().with_prior(Some(100)));
    assert_eq!(tiny.pace_bps(), 8_000);
    let _ = now;
}

#[test]
fn prior_store_is_bounded_and_ttl_limited() {
    let now = Instant::now();
    let mut store = PriorStore::new(4, Duration::from_secs(3_600));
    store.remember(1, 1, 500_000, now);
    assert_eq!(store.recall(1, 1, now), Some(500_000));
    assert_eq!(store.recall(1, 2, now), None, "keys are (receiver, sender)");
    for index in 0..10u64 {
        store.remember(index, 1, 100_000, now + Duration::from_millis(index));
    }
    assert!(store.len() <= 4, "LRU cap");
    assert_eq!(
        store.recall(1, 1, now + Duration::from_secs(4_000)),
        None,
        "TTL"
    );
}

#[test]
fn hop_reports_are_subtracted_from_viewer_loss() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    estimator.on_feedback(&lossy(200, 100_000, 0.2), now);
    estimator.on_hop_report(
        &MediaHopReport {
            hop: hop::FIRST,
            objects_dropped: 5,
            coalesced: 5,
            queue_depth_peak: 3,
            window_ms: 1_000,
            ..MediaHopReport::default()
        },
        now,
    );
    estimator.on_hop_report(
        &MediaHopReport {
            hop: hop::SECOND,
            objects_dropped: 4,
            window_ms: 1_000,
            ..MediaHopReport::default()
        },
        now,
    );
    let attribution = estimator.attribution();
    assert_eq!(attribution.viewer_lost_updates, 20);
    assert_eq!(attribution.relay_dropped, 10, "drops plus coalescing");
    assert_eq!(attribution.gateway_dropped, 4);
    assert_eq!(attribution.link_lost_updates(), 6);
}

#[test]
fn rtt_samples_from_probes_and_hop_reports_do_not_carry_loss() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig::default());
    for _ in 0..4 {
        estimator.on_feedback(&healthy(100, 1_000_000), now);
    }
    let loss = estimator.loss_ratio();
    estimator.on_rtt_sample(400.0, now);
    assert!((estimator.rtt_ms() - 400.0).abs() < 1.0, "the first sample");
    assert!(estimator.rtt_min_ms() <= 400.0);
    assert_eq!(
        estimator.loss_ratio(),
        loss,
        "an RTT sample carries no loss"
    );
    estimator.on_probe_echo(
        &ProbeEcho {
            object_id: 1,
            send_epoch_ms: 0,
            echo_epoch_ms: 900,
            payload_bytes: 100,
        },
        now,
    );
    assert_eq!(estimator.loss_ratio(), loss);
}

/// §12/§13: the same scenario at 10/150/400/2000 ms RTT keeps every rule
/// inside its clamp band and converges.
#[test]
fn every_rule_holds_at_ten_fifteen_four_hundred_and_two_thousand_ms_rtt() {
    for rtt_ms in [10u32, 150, 400, 2_000] {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        // A healthy link at the tier's own goodput.
        let goodput = match rtt_ms {
            10 => 10_000_000,
            150 => 1_000_000,
            400 => 256_000,
            _ => 32_000,
        };
        for _ in 0..6 {
            round_trip(&mut estimator, f64::from(rtt_ms), now);
            estimator.on_feedback(&healthy(rtt_ms, goodput), now);
        }
        let estimate = estimator.estimate();
        assert!(
            (8_000..=50_000_000).contains(&estimate.pace_bps),
            "rtt {rtt_ms}: pace {}",
            estimate.pace_bps
        );
        assert!((0.5..=30.0).contains(&estimate.target_fps));
        assert!((256..=1_048_576).contains(&estimate.budget_bytes));
        assert!((1_024..=524_288).contains(&estimate.max_object_bytes));
        // The report cadence is clamp(4 × rtt_min, 250, 2 000).
        let expected = (4.0 * f64::from(rtt_ms)).clamp(250.0, 2_000.0);
        assert_eq!(
            estimator.report_interval(),
            Duration::from_secs_f64(expected / 1_000.0),
            "rtt {rtt_ms}"
        );
        // The no-feedback grace is a *report* timer, not the session idle
        // timer: it scales with the report cadence and stays bounded.
        let grace_ms = estimator.no_feedback_grace().as_millis() as u64;
        assert!(
            (2_750..=8_000).contains(&grace_ms),
            "rtt {rtt_ms}: grace {grace_ms} ms"
        );
    }
}

#[test]
fn the_estimator_is_deterministic_across_runs() {
    let run = || {
        let now = Instant::now();
        let mut estimator = BandwidthEstimator::new(BweConfig::default());
        for step in 0..20u32 {
            let rtt = 100 + (step % 5) * 40;
            let goodput = 200_000 + u64::from(step) * 50_000;
            let lost = if step % 7 == 0 { 5 } else { 0 };
            estimator.on_feedback(&feedback(rtt, goodput / 8, 100, lost, 1_000), now);
            estimator.on_timeout(now + Duration::from_millis(u64::from(step) * 100));
        }
        estimator.estimate()
    };
    assert_eq!(run(), run());
}

#[test]
fn the_ceiling_can_be_capped_by_policy() {
    let now = Instant::now();
    let mut estimator = BandwidthEstimator::new(BweConfig {
        ceiling_bps: 50_000_000,
        ..BweConfig::default()
    });
    estimator.set_ceiling(500_000);
    for _ in 0..8 {
        estimator.on_feedback(&healthy(50, 50_000_000), now);
    }
    assert!(
        estimator.pace_bps() <= 500_000,
        "a metered tenant can cap the estimator"
    );
    assert!(estimator.btlbw_bps() <= 500_000);
}
