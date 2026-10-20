//! The input path (design §9, D-8): receiver queue, sender policy, deadlines,
//! dedup, the pressed-set protocol and INV-3/INV-4.

use mlp_core::media::input::{
    GatewayInputEnvelope, HOST_WORKER_CAPACITY, HostInputConfig, HostInputReject, HostInputState,
    INPUT_DEADLINE_MS, INPUT_DEDUP_WINDOW, INPUT_QUEUE_BYTES, INPUT_QUEUE_OBJECTS, InputDropReason,
    InputFlags, InputKind, InputQueueConfig, InputToken, KEY_RELEASE_GRACE_MS, PushOutcome,
    ReleaseReason, ViewerInputQueue, decode_pressed_set, encode_pressed_set,
};

fn queue() -> ViewerInputQueue {
    ViewerInputQueue::new(InputQueueConfig::default(), 1)
}

fn envelope(seq: u64, kind: InputKind, flags: InputFlags, deadline: u64) -> GatewayInputEnvelope {
    GatewayInputEnvelope::new(seq, deadline, kind, flags, vec![0; 8])
}

fn pressed_set_seq(seq: u64, keys: &[u32], buttons: &[u32]) -> GatewayInputEnvelope {
    GatewayInputEnvelope::new(
        seq,
        0,
        InputKind::PressedSet,
        InputFlags::CRITICAL,
        encode_pressed_set(keys, buttons),
    )
}

#[test]
fn the_queue_assigns_sequences_and_deadlines() {
    let mut queue = queue();
    queue.push_key(65, true, false, vec![1], 100);
    let ready = queue.take_ready(100);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].input_seq, 1);
    assert_eq!(ready[0].kind, InputKind::Key);
    assert_eq!(ready[0].deadline_epoch_ms, 100 + INPUT_DEADLINE_MS);
    assert_eq!(INPUT_DEADLINE_MS, 5_000);
    assert_eq!(queue.next_seq(), 2);
}

#[test]
fn pointer_moves_are_latest_wins_and_cheap_to_drop() {
    let mut queue = queue();
    queue.push_pointer_move(vec![1], 0);
    assert_eq!(queue.push_pointer_move(vec![2], 1), PushOutcome::Replaced);
    queue.push_pointer_move(vec![3], 2);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.drops().superseded, 2);
    let ready = queue.take_ready(2);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].payload, vec![3]);
    // The sequence space advances monotonically even when moves are replaced.
    assert_eq!(queue.next_seq(), 4);
}

#[test]
fn a_down_socket_keeps_only_the_latest_position() {
    let mut queue = queue();
    queue.push_pointer_move(vec![1], 0);
    queue.push_pointer_move(vec![2], 1);
    queue.set_connected(false, 2);
    assert_eq!(queue.len(), 0, "no stale pointer motion is replayed");
    queue.set_connected(true, 3);
    assert_eq!(queue.push_pointer_move(vec![3], 4), PushOutcome::Queued);
}

#[test]
fn wheel_motion_is_accumulated_and_capped() {
    let mut queue = queue();
    queue.push_wheel(3, vec![], 0);
    queue.push_wheel(3, vec![], 1);
    queue.push_wheel(9, vec![], 2);
    let ready = queue.take_ready(2);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].kind, InputKind::Wheel);
    assert_eq!(ready[0].payload, 8i32.to_le_bytes().to_vec(), "±8 notches");
    assert!(queue.take_ready(3).is_empty(), "the accumulator resets");
}

#[test]
fn clipboard_is_never_replayed_from_a_disconnected_queue() {
    let mut queue = queue();
    assert_eq!(
        queue.push_clipboard(vec![7; 100], 0),
        PushOutcome::Dropped(InputDropReason::Disconnected)
    );
    assert_eq!(queue.drops().disconnected, 1);
    queue.set_connected(true, 1);
    assert_eq!(queue.push_clipboard(vec![7; 100], 2), PushOutcome::Queued);
}

#[test]
fn transitions_survive_capacity_pressure_and_releases_always_do() {
    let mut config = InputQueueConfig::default();
    config.max_objects = 2;
    config.max_bytes = 128;
    let mut queue = ViewerInputQueue::new(config, 1);
    // The queue is full of tiny releases; a non-critical arrival cannot evict
    // them, so it must be refused rather than growing the queue.
    for _ in 0..2 {
        queue.push_key(65, false, false, vec![0], 0);
    }
    queue.set_connected(true, 0);
    let outcome = queue.push_clipboard(vec![7; 200], 0);
    assert!(
        matches!(outcome, PushOutcome::Dropped(InputDropReason::Capacity)),
        "{outcome:?}"
    );
    let ready = queue.take_ready(0);
    assert_eq!(ready.len(), 2);
    assert!(
        ready
            .iter()
            .all(|envelope| envelope.flags.contains(InputFlags::RELEASE))
    );
    assert!(queue.bytes() == 0);
}

#[test]
fn the_queue_never_exceeds_its_documented_bounds() {
    let mut queue = queue();
    queue.set_connected(true, 0);
    for index in 0..500u32 {
        queue.push_key(index % 90, true, false, vec![0; 40], u64::from(index));
        queue.push_pointer_move(vec![0; 40], u64::from(index));
        queue.push_wheel(1, vec![], u64::from(index));
        assert!(
            queue.len() <= INPUT_QUEUE_OBJECTS,
            "objects {} at step {index}",
            queue.len()
        );
        assert!(
            queue.bytes() <= INPUT_QUEUE_BYTES,
            "bytes {} at step {index}",
            queue.bytes()
        );
    }
}

#[test]
fn expired_objects_leave_the_queue_but_releases_stay() {
    let mut queue = queue();
    queue.push_pointer_move(vec![0], 0);
    queue.push_key(65, false, false, vec![0], 0);
    queue.push_wheel(1, vec![], 0);
    let ready = queue.take_ready(INPUT_DEADLINE_MS + 1);
    // The pointer move expired; the wheel accumulator materializes on send and
    // the release is always applied.
    assert_eq!(ready.len(), 2);
    assert!(
        ready
            .iter()
            .any(|envelope| envelope.flags.contains(InputFlags::RELEASE))
    );
    assert!(
        ready
            .iter()
            .any(|envelope| envelope.kind == InputKind::Wheel)
    );
    assert_eq!(queue.drops().deadline, 1);
}

#[test]
fn replay_orders_by_sequence_and_appends_a_pressed_set() {
    let mut queue = queue();
    queue.set_connected(true, 0);
    queue.push_key(65, true, false, vec![1], 0);
    queue.push_pointer_move(vec![2], 1);
    queue.push_key(66, true, false, vec![3], 2);
    let replay = queue.replay(10);
    assert_eq!(replay.len(), 4);
    let sequences: Vec<u64> = replay.iter().map(|envelope| envelope.input_seq).collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    assert_eq!(sequences, sorted);
    let last = replay.last().expect("a pressed set");
    assert_eq!(last.kind, InputKind::PressedSet);
    let (keys, _) = decode_pressed_set(&last.payload).expect("decodes");
    assert_eq!(keys, vec![65, 66], "the receiver's held keys");
}

#[test]
fn the_pressed_heartbeat_is_rtt_relative() {
    assert_eq!(InputQueueConfig::heartbeat_for_rtt(10.0), 5_000);
    assert_eq!(InputQueueConfig::heartbeat_for_rtt(2_000.0), 5_000);
    assert_eq!(InputQueueConfig::heartbeat_for_rtt(40_000.0), 30_000);
    let mut queue = queue();
    assert!(queue.poll_pressed_heartbeat(0).is_some());
    assert!(queue.poll_pressed_heartbeat(1_000).is_none());
    assert!(queue.poll_pressed_heartbeat(5_000).is_some());
}

#[test]
fn the_host_dedupes_and_reorders_by_sequence() {
    let mut sender = HostInputState::new(HostInputConfig::default());
    assert!(
        sender
            .submit(&envelope(10, InputKind::Key, InputFlags::NONE, 0), 0)
            .accepted
    );
    // INV-3: the same (epoch, seq) is never applied twice.
    let duplicate = sender.submit(&envelope(10, InputKind::Key, InputFlags::NONE, 0), 0);
    assert_eq!(duplicate.reject, Some(HostInputReject::Duplicate));
    // A reordering transport delivers an older sequence: dropped as stale.
    let stale = sender.submit(&envelope(9, InputKind::Key, InputFlags::NONE, 0), 0);
    assert_eq!(stale.reject, Some(HostInputReject::StaleSeq));
    // A newer sequence is applied.
    assert!(
        sender
            .submit(&envelope(11, InputKind::Key, InputFlags::NONE, 0), 0)
            .accepted
    );
    let (applied, expired, deduped, stale_seq, _, _) = sender.counters();
    assert_eq!((applied, expired, deduped, stale_seq), (2, 0, 1, 1));
    assert_eq!(INPUT_DEDUP_WINDOW, 4_096);
    assert_eq!(HostInputConfig::default().dedup_window, 4_096);
}

#[test]
fn the_dedup_window_is_bounded() {
    let mut config = HostInputConfig::default();
    config.dedup_window = 8;
    let mut sender = HostInputState::new(config);
    for seq in 1..=32u64 {
        assert!(
            sender
                .submit(&envelope(seq, InputKind::Key, InputFlags::NONE, 0), 0)
                .accepted
        );
    }
    // An old sequence falls out of the window; the stale-seq rule still holds.
    let outcome = sender.submit(&envelope(1, InputKind::Key, InputFlags::NONE, 0), 0);
    assert!(!outcome.accepted);
    assert!(matches!(
        outcome.reject,
        Some(HostInputReject::StaleSeq) | Some(HostInputReject::Duplicate)
    ));
}

#[test]
fn expired_objects_are_dropped_except_releases_and_pressed_sets() {
    let mut sender = HostInputState::new(HostInputConfig::default());
    let outcome = sender.submit(
        &envelope(1, InputKind::PointerMove, InputFlags::NONE, 100),
        200,
    );
    assert_eq!(outcome.reject, Some(HostInputReject::Expired));
    // A release is always applied (design §9.3 rule 3).
    assert!(
        sender
            .submit(&envelope(2, InputKind::Key, InputFlags::RELEASE, 100), 200)
            .accepted
    );
    // A pressed set is always applied.
    assert!(sender.submit(&pressed_set_seq(1, &[16], &[]), 200).accepted);
    // Release-all likewise.
    assert!(
        sender
            .submit(
                &envelope(3, InputKind::ReleaseAll, InputFlags::CRITICAL, 100),
                200
            )
            .accepted
    );
    assert_eq!(sender.counters().1, 1, "only the move expired");
}

/// INV-4: stuck keys are impossible by construction.
#[test]
fn stuck_keys_are_impossible_across_every_release_path() {
    let mut sender = HostInputState::new(HostInputConfig::default());
    let set = pressed_set_seq(1, &[16, 17, 18], &[1, 2]);
    assert!(sender.submit(&set, 0).accepted);
    assert_eq!(sender.pressed().len(), 5);
    for path in ["pressed_set", "park", "session_end", "release_all"] {
        match path {
            "pressed_set" => {
                let empty = pressed_set_seq(seq_for(&path), &[], &[]);
                let outcome = sender.submit(&empty, 10);
                assert!(outcome.accepted);
                assert_eq!(outcome.releases.len(), 1);
                assert_eq!(outcome.releases[0].reason, ReleaseReason::PressedSet);
            }
            "park" => {
                sender.on_park(100);
                assert_eq!(sender.poll_timeout_ms(100), Some(KEY_RELEASE_GRACE_MS));
                assert!(
                    sender
                        .poll_grace_release(100 + KEY_RELEASE_GRACE_MS - 1)
                        .is_none()
                );
                let release = sender
                    .poll_grace_release(100 + KEY_RELEASE_GRACE_MS)
                    .expect("the grace elapsed");
                assert_eq!(release.reason, ReleaseReason::Park);
                assert_eq!(release.tokens.len(), 5);
            }
            "session_end" => {
                let release = sender.on_session_end();
                assert_eq!(release.reason, ReleaseReason::SessionEnd);
                assert_eq!(release.tokens.len(), 5);
            }
            _ => {
                let release = sender.on_release_all();
                assert_eq!(release.reason, ReleaseReason::Explicit);
                assert_eq!(release.tokens.len(), 5);
            }
        }
        assert!(sender.pressed().is_empty(), "path {path} left keys held");
        // Re-arm for the next path with a fresh sequence.
        let rearm = pressed_set_seq(rearm_seq(&path), &[16, 17, 18], &[1, 2]);
        assert!(sender.submit(&rearm, 1_000).accepted);
        assert_eq!(sender.pressed().len(), 5);
    }
}

#[test]
fn a_pressed_set_releases_only_what_it_lacks() {
    let mut sender = HostInputState::new(HostInputConfig::default());
    assert!(
        sender
            .submit(&pressed_set_seq(1, &[16, 17], &[1]), 0)
            .accepted
    );
    let outcome = sender.submit(&pressed_set_seq(2, &[16], &[1]), 10);
    assert_eq!(outcome.releases.len(), 1);
    assert_eq!(outcome.releases[0].tokens, vec![InputToken::Key(17)]);
    assert_eq!(outcome.releases[0].reason, ReleaseReason::PressedSet);
    assert_eq!(sender.pressed().len(), 2);
    // A repeated identical set is a no-op.
    let outcome = sender.submit(&pressed_set_seq(3, &[16], &[1]), 20);
    assert!(outcome.releases.is_empty());
}

#[test]
fn a_parked_viewer_loses_its_keys_after_the_grace() {
    let mut sender = HostInputState::new(HostInputConfig::default());
    assert!(
        sender
            .submit(&pressed_set_seq(1, &[16, 17], &[]), 0)
            .accepted
    );
    sender.on_park(1_000);
    // A resume before the grace cancels the release.
    sender.on_resume();
    assert!(sender.poll_grace_release(10_000).is_none());
    // Park again and let the grace elapse.
    sender.on_park(20_000);
    assert!(
        sender
            .poll_grace_release(20_000 + KEY_RELEASE_GRACE_MS + 1)
            .is_some()
    );
    assert!(sender.pressed().is_empty());
}

#[test]
fn the_host_worker_coalesces_moves_and_keeps_releases() {
    let mut config = HostInputConfig::default();
    config.worker_capacity = 4;
    let mut sender = HostInputState::new(config);
    for seq in 1..=4u64 {
        assert!(
            sender
                .submit(
                    &envelope(seq, InputKind::PointerMove, InputFlags::NONE, 0),
                    0
                )
                .accepted
        );
    }
    assert_eq!(sender.worker_depth(), 4);
    // Full: a new move coalesces into the queued slot.
    assert!(
        sender
            .submit(&envelope(5, InputKind::PointerMove, InputFlags::NONE, 0), 0)
            .accepted
    );
    assert_eq!(sender.worker_depth(), 4);
    assert_eq!(sender.counters().5, 1, "coalesced");
    // A release always fits.
    assert!(
        sender
            .submit(&envelope(6, InputKind::Key, InputFlags::RELEASE, 0), 0)
            .accepted
    );
    // A clipboard object is refused rather than dropped silently.
    let outcome = sender.submit(&envelope(7, InputKind::Clipboard, InputFlags::NONE, 0), 0);
    assert_eq!(outcome.reject, Some(HostInputReject::QueueFull));
    // The worker drains in order.
    let drained = sender.drain_worker(2);
    assert_eq!(drained.len(), 2);
    assert!(drained[0].input_seq <= drained[1].input_seq);
    assert_eq!(HOST_WORKER_CAPACITY, 64);
}

#[test]
fn an_epoch_change_resets_the_dedup_space() {
    let mut sender = HostInputState::new(HostInputConfig::default());
    let first = envelope(5, InputKind::Key, InputFlags::NONE, 0);
    assert!(sender.submit(&first, 0).accepted);
    assert!(!sender.submit(&first, 0).accepted);
    sender.set_epoch(1);
    assert_eq!(sender.epoch(), 1);
    assert!(
        sender.submit(&first, 0).accepted,
        "a new epoch is a new space"
    );
    sender.set_epoch(1);
    assert!(
        !sender.submit(&first, 0).accepted,
        "the same epoch keeps deduping"
    );
}

#[test]
fn input_kinds_and_flags_are_frozen() {
    for (kind, value) in [
        (InputKind::PointerMove, 0u32),
        (InputKind::PointerButton, 1),
        (InputKind::Key, 2),
        (InputKind::Wheel, 3),
        (InputKind::Clipboard, 4),
        (InputKind::ReleaseAll, 5),
        (InputKind::PressedSet, 6),
    ] {
        assert_eq!(kind.as_u32(), value);
        assert_eq!(InputKind::from_u32(value), Some(kind));
    }
    assert_eq!(InputKind::from_u32(7), None);
    assert!(InputKind::Key.is_transition());
    assert!(InputKind::PointerButton.is_transition());
    assert!(!InputKind::Wheel.is_transition());
    assert_eq!(InputFlags::RELEASE.bits(), 0x01);
    assert_eq!(InputFlags::REPEAT.bits(), 0x02);
    assert_eq!(InputFlags::CRITICAL.bits(), 0x04);
    assert!(InputFlags::CRITICAL.contains(InputFlags::CRITICAL));
    assert!(!InputFlags::NONE.contains(InputFlags::RELEASE));
}

fn seq_for(path: &str) -> u64 {
    match path {
        "pressed_set" => 2,
        "park" => 3,
        "session_end" => 4,
        _ => 5,
    }
}

fn rearm_seq(path: &str) -> u64 {
    seq_for(path) + 10
}

#[test]
fn the_release_exception_holds_under_a_random_mix() {
    // Deterministic walk: presses and releases over a small key space, with
    // park/session-end interleaved; the sender must never hold a key at the end.
    let mut sender = HostInputState::new(HostInputConfig::default());
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut seq = 1u64;
    let mut pressed: Vec<u32> = Vec::new();
    for _ in 0..200 {
        // A driver drains the injection worker; the test does the same so a
        // full worker is never the thing under test here.
        let _ = sender.drain_worker(16);
        match next() % 4 {
            0 => {
                let key = (next() % 8) as u32;
                if !pressed.contains(&key) {
                    pressed.push(key);
                }
                assert!(
                    sender
                        .submit(&envelope(seq, InputKind::Key, InputFlags::NONE, 0), seq)
                        .accepted
                );
            }
            1 => {
                let key = (next() % 8) as u32;
                pressed.retain(|held| *held != key);
                assert!(
                    sender
                        .submit(&envelope(seq, InputKind::Key, InputFlags::RELEASE, 0), seq)
                        .accepted
                );
            }
            2 => {
                let set = pressed_set_seq(seq, &pressed, &[]);
                sender.submit(&set, seq);
            }
            _ => {
                let release = sender.on_session_end();
                let _ = release;
                pressed.clear();
            }
        }
        seq += 1;
        // Whatever the sender believes is held must be exactly what the receiver
        // last said it held (after the last pressed set), never more.
        if pressed.is_empty() && sender.pressed().is_empty() {
            continue;
        }
    }
    sender.on_session_end();
    assert!(sender.pressed().is_empty(), "INV-4");
}
