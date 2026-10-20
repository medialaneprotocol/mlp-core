//! Pacing and the reserved control floor (design §7.4, §4.2 rule 3, INV-7).
//!
//! The pacer is the only authority for *when* bytes leave. These tests pin its
//! behaviour across the whole T0–T4 rate range, assert the 1-second window
//! contract, and prove the reserve cannot be starved by media.

use std::time::{Duration, Instant};

use mlp_core::media::pacer::{
    AcquireResult, BURST_ALLOWANCE_RATIO, DenyReason, MAX_GRANT_BYTES, MediaPacer, PACING_WINDOW,
    PacerConfig, RESERVED_RATE_FLOOR_BPS, RESERVED_RATE_FRACTION, STREAM_RELEASE_MAX, TokenBucket,
};
use mlp_core::media::pdu::{Carrier, TrafficClass};

fn config(carrier: Carrier, pace_bps: u64, max_object_bytes: u32) -> PacerConfig {
    PacerConfig::for_carrier(carrier, pace_bps, max_object_bytes)
}

#[test]
fn token_bucket_refills_at_the_rate_and_caps() {
    let start = Instant::now();
    let mut bucket = TokenBucket::new(1_000.0, 500.0, start);
    assert_eq!(bucket.tokens(), 1_000.0);
    assert!(bucket.try_consume(1_000.0));
    assert!(!bucket.try_consume(1.0));
    assert_eq!(
        bucket.time_until(500.0),
        Duration::from_secs(1),
        "500 bytes at 500 B/s"
    );
    bucket.refill(start + Duration::from_millis(500));
    assert_eq!(bucket.tokens(), 250.0);
    bucket.refill(start + Duration::from_secs(10));
    assert_eq!(bucket.tokens(), 1_000.0);
    assert_eq!(bucket.available_above(900.0), 100.0);
    assert_eq!(bucket.available_above(10_000.0), 0.0);
}

#[test]
fn reserve_rate_matches_the_design_across_the_tier_range() {
    let start = Instant::now();
    // Below 20 kbps the 2 kbps floor wins; 100 Mbps (T0) uses 10 %.
    let slow = MediaPacer::new(16_000, 8_000, start);
    assert_eq!(slow.reserved_rate_bps(), RESERVED_RATE_FLOOR_BPS);
    assert_eq!(
        MediaPacer::new(32_000, 8_000, start).reserved_rate_bps(),
        3_200,
        "T4 sits just above the floor"
    );
    let fast = MediaPacer::new(100_000_000, 524_288, start);
    assert_eq!(fast.reserved_rate_bps(), 10_000_000);
    assert_eq!(RESERVED_RATE_FRACTION, 0.10);
    for pace in [
        32_000u64,
        64_000,
        256_000,
        1_000_000,
        5_000_000,
        100_000_000,
    ] {
        let pacer = MediaPacer::new(pace, 8_192, start);
        let expected = ((pace as f64) * 0.10).max(2_000.0) as u64;
        assert_eq!(pacer.reserved_rate_bps(), expected, "pace {pace}");
    }
}

#[test]
fn media_may_not_consume_the_reserve_and_control_may_borrow_it() {
    let start = Instant::now();
    let mut pacer = MediaPacer::with_config(
        PacerConfig {
            max_grant_bytes: 1_150,
            ..config(Carrier::DataChannel, 32_000, 2_000)
        },
        start,
    );
    pacer.set_rtt(Duration::from_millis(1_000));
    // max(2 kbps, 10 % × 32 kbps) = 3.2 kbps over a 1 s horizon = 400 bytes.
    assert_eq!(pacer.reserve_watermark_bytes(), 400);
    let tokens = pacer.tokens();
    assert!(tokens >= 400);
    // Drain everything media is allowed to take.
    let mut sent = 0u64;
    while pacer.can_acquire(TrafficClass::Media, 250) {
        assert!(
            pacer
                .try_acquire(TrafficClass::Media, 250, start)
                .is_granted()
        );
        sent += 250;
    }
    assert!(sent > 0);
    // The drain stops inside one step of the watermark: media never crosses it.
    assert!(
        pacer.tokens() >= 400,
        "the reserve survives: {}",
        pacer.tokens()
    );
    assert!(!pacer.can_acquire(TrafficClass::Media, 250));
    assert!(
        pacer.can_acquire(TrafficClass::Media, 100),
        "the excess is media's"
    );
    assert!(pacer.can_acquire(TrafficClass::Control, 400));
}

#[test]
fn window_budget_is_the_documented_allowance_or_one_fragment() {
    let start = Instant::now();
    // 32 kbps: 4 000 B/s ⇒ 1.25 × 4 000 = 5 000; one 16 KiB grant is larger,
    // so the carve-out governs.
    let slow = MediaPacer::new(32_000, 8_000, start);
    assert_eq!(slow.window_budget_bytes(), MAX_GRANT_BYTES);
    // 100 Mbps: the ratio governs.
    let fast = MediaPacer::new(100_000_000, 524_288, start);
    assert_eq!(
        fast.window_budget_bytes(),
        (100_000_000.0 / 8.0 * BURST_ALLOWANCE_RATIO).round() as u64
    );
    assert_eq!(PACING_WINDOW, Duration::from_secs(1));
}

/// INV-7: sent bytes in any 1 s window never exceed the window budget.
#[test]
fn inv7_rate_cap_holds_across_the_tier_range() {
    for pace_bps in [
        32_000u64,
        64_000,
        256_000,
        1_000_000,
        5_000_000,
        100_000_000,
    ] {
        for carrier in [Carrier::Ws, Carrier::DataChannel] {
            let start = Instant::now();
            let fragment = (carrier.fragment_payload_max() as u64).min(1_150);
            let mut pacer = MediaPacer::with_config(config(carrier, pace_bps, 8_192), start);
            pacer.set_rtt(Duration::from_millis(800));
            let mut at = start;
            let mut worst = 0u64;
            let mut total = 0u64;
            for _ in 0..20_000 {
                // The sender asks for a fragment; if refused it waits the
                // reported time (or 10 ms) and asks again.
                match pacer.try_acquire(TrafficClass::Media, fragment, at) {
                    AcquireResult::Granted => {
                        total += fragment;
                        at += Duration::from_micros(100);
                    }
                    AcquireResult::Wait { wait, .. } => {
                        at += wait.max(Duration::from_millis(1));
                    }
                    AcquireResult::Oversize { .. } => {
                        panic!("the fragment must fit the grant cap")
                    }
                }
                let window = pacer.window_bytes(at);
                worst = worst.max(window);
            }
            assert!(
                worst <= pacer.window_budget_bytes(),
                "pace {pace_bps} {}: {worst} > {}",
                carrier.as_str(),
                pacer.window_budget_bytes()
            );
            // Over the long run the average is the pace, not the budget.
            let elapsed = at.duration_since(start).as_secs_f64();
            let rate = total as f64 / elapsed;
            let expected = pace_bps as f64 / 8.0;
            assert!(
                rate <= expected * 1.35 + fragment as f64,
                "pace {pace_bps}: long-run rate {rate:.0} B/s above {expected:.0}"
            );
        }
    }
}

#[test]
fn oversize_requests_are_refused_not_waited_on() {
    let start = Instant::now();
    let mut pacer = MediaPacer::new(1_000_000, 8_192, start);
    let result = pacer.try_acquire(TrafficClass::Media, MAX_GRANT_BYTES + 1, start);
    assert!(result.is_oversize());
    assert_eq!(result.reason(), None);
    assert_eq!(result.wait(), Duration::ZERO);
    let tokens = pacer.tokens();
    assert_eq!(pacer.tokens(), tokens, "nothing was consumed");
}

#[test]
fn release_budget_is_one_fragment_on_datagram_and_sixty_four_kib_on_stream() {
    let start = Instant::now();
    for carrier in [Carrier::Ws, Carrier::Grpc] {
        let mut pacer = MediaPacer::with_config(config(carrier, 100_000_000, 524_288), start);
        pacer.set_rtt(Duration::from_millis(10));
        assert_eq!(
            pacer.release_budget(carrier, TrafficClass::Media),
            STREAM_RELEASE_MAX
        );
    }
    for carrier in [Carrier::DataChannel, Carrier::RepairChannel] {
        let mut pacer = MediaPacer::with_config(config(carrier, 100_000_000, 524_288), start);
        pacer.set_rtt(Duration::from_millis(10));
        assert_eq!(
            pacer.release_budget(carrier, TrafficClass::Media),
            carrier.fragment_payload_max() as u64
        );
    }
}

#[test]
fn control_classes_are_never_blocked_by_the_reserve() {
    let start = Instant::now();
    let mut pacer = MediaPacer::with_config(
        PacerConfig {
            max_grant_bytes: 1_150,
            ..config(Carrier::DataChannel, 8_000, 1_024)
        },
        start,
    );
    pacer.set_rtt(Duration::from_secs(2));
    // Media can only take what is above the watermark; control can take it all.
    let watermark = pacer.reserve_watermark_bytes();
    assert!(watermark > 0);
    assert_eq!(
        pacer.release_budget(Carrier::DataChannel, TrafficClass::Media),
        pacer.tokens().saturating_sub(watermark).min(1_150)
    );
    assert_eq!(
        pacer.release_budget(Carrier::DataChannel, TrafficClass::Control),
        pacer.tokens().min(1_150)
    );
    for class in [
        TrafficClass::Control,
        TrafficClass::Input,
        TrafficClass::Feedback,
        TrafficClass::Cursor,
    ] {
        assert!(
            pacer.can_acquire(class, 64),
            "{class:?} must not be reserved-out"
        );
    }
}

#[test]
fn pace_changes_take_effect_immediately_and_keep_the_tokens() {
    let start = Instant::now();
    let mut pacer = MediaPacer::new(8_000, 1_024, start);
    assert_eq!(pacer.pace_bps(), 8_000);
    let before = pacer.tokens();
    pacer.set_pace(1_000_000, 32_768, start);
    assert_eq!(pacer.pace_bps(), 1_000_000);
    assert_eq!(pacer.max_object_bytes(), 32_768);
    assert_eq!(pacer.tokens(), before.min(pacer.window_budget_bytes()));
    pacer.set_pace(32_000, 8_000, start);
    assert_eq!(pacer.reserved_rate_bps(), 3_200);
}

#[test]
fn the_pacer_waits_exactly_the_deficit_time() {
    let start = Instant::now();
    let mut pacer = MediaPacer::with_config(
        PacerConfig {
            max_grant_bytes: 1_150,
            ..config(Carrier::DataChannel, 32_000, 2_000)
        },
        start,
    );
    pacer.set_rtt(Duration::from_millis(250));
    // Drain everything above the reserve.
    while pacer.can_acquire(TrafficClass::Media, 1_000) {
        assert!(
            pacer
                .try_acquire(TrafficClass::Media, 1_000, start)
                .is_granted()
        );
    }
    // The wait is exactly the deficit down to `bytes + watermark`.
    assert_eq!(pacer.tokens(), 1_000);
    assert_eq!(
        pacer.reserve_watermark_bytes(),
        100,
        "3.2 kbps over the 250 ms horizon"
    );
    let wait = pacer.time_until(TrafficClass::Media, 1_000, start);
    let expected = Duration::from_secs_f64((1_000.0 + 100.0 - 1_000.0) / 4_000.0);
    assert_eq!(wait, expected, "{wait:?}");
    assert!(
        wait < Duration::from_millis(50),
        "a 400-byte deficit at 4 000 B/s is short"
    );
    // After exactly that wait the grant is available again.
    let later = start + wait + Duration::from_millis(1);
    pacer.refill(later);
    assert!(pacer.can_acquire(TrafficClass::Media, 1_000));
}

#[test]
fn deny_reasons_are_typed() {
    let start = Instant::now();
    let mut pacer = MediaPacer::with_config(
        PacerConfig {
            max_grant_bytes: 1_150,
            ..config(Carrier::DataChannel, 32_000, 2_000)
        },
        start,
    );
    pacer.set_rtt(Duration::from_secs(1));
    while pacer.can_acquire(TrafficClass::Media, 500) {
        pacer.try_acquire(TrafficClass::Media, 500, start);
    }
    let refusal = pacer.try_acquire(TrafficClass::Media, 500, start);
    assert_eq!(refusal.reason(), Some(DenyReason::Reserved));
    // Once the reserve is large enough for a bigger rate, exhaustion inside
    // the window is reported as a window wait, not a reserve wait.
    let mut tight = MediaPacer::with_config(
        PacerConfig {
            max_grant_bytes: 1_000,
            ..config(Carrier::DataChannel, 8_000, 1_024)
        },
        start,
    );
    tight.set_rtt(Duration::from_millis(250));
    assert!(
        tight
            .try_acquire(TrafficClass::Control, 1_000, start)
            .is_granted()
    );
    let refusal = tight.try_acquire(TrafficClass::Control, 500, start);
    assert_eq!(
        refusal.reason(),
        Some(DenyReason::Window),
        "the 1 s allowance is what binds here"
    );
}

#[test]
fn a_steady_sender_gets_a_steady_rate() {
    // 64 kbps for 10 simulated seconds, one 1 150-byte fragment at a time.
    let start = Instant::now();
    let mut pacer = MediaPacer::with_config(
        PacerConfig {
            max_grant_bytes: 1_150,
            ..config(Carrier::DataChannel, 64_000, 4_000)
        },
        start,
    );
    pacer.set_rtt(Duration::from_millis(400));
    let mut at = start;
    let mut total = 0u64;
    while at.duration_since(start) < Duration::from_secs(10) {
        if pacer
            .try_acquire(TrafficClass::Media, 1_150, at)
            .is_granted()
        {
            total += 1_150;
        }
        at += Duration::from_millis(5);
    }
    let rate = total as f64 / 10.0;
    // The long-run rate is the pace plus at most one bucket of start-up burst,
    // which is the documented 1.25 allowance.
    assert!(
        (8_000.0 * 0.95..=8_000.0 * BURST_ALLOWANCE_RATIO).contains(&rate),
        "64 kbps = 8 000 B/s, got {rate}"
    );
}
