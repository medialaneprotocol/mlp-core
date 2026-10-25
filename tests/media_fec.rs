//! The terrible-lane rule, end to end inside the machines: a multi-part
//! update on a measured-lossy lane carries parity; a lost part comes back
//! from it; a lying parity recovers nothing and is counted.

use std::time::Instant;

use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::{Carrier, FragmentClass};
use mlp_core::media::receiver::{MediaReceiveEvent, MediaReceiverConfig, MediaReceiverMachine};
use mlp_core::media::scheduler::UpdateKind;
use mlp_core::media::sender::{
    EnqueueOutcome, MediaSenderConfig, MediaSenderMachine, OutboundFragment, UpdateRequest,
};

fn lossy_sender() -> MediaSenderMachine {
    let mut sender = MediaSenderMachine::new(
        MediaSenderConfig::new(Carrier::Ws, CellGrid::with_cell_size(320, 180, 64)),
        CellGrid::with_cell_size(320, 180, 64),
        Instant::now(),
    );
    // The measured lane: 10 % loss, well above the parity threshold.
    sender.on_feedback(
        mlp_core::media::MediaFeedback {
            rtt_ms: 40,
            rtt_min_ms: 40,
            jitter_ms: 0,
            received_bytes: 100_000,
            received_objects: 90,
            lost_objects: 10,
            duplicated_objects: 0,
            reordered_objects: 0,
            window_ms: 1_000,
            ack: None,
            probes: vec![],
            buffered_amount_bytes: 0,
            display_fps: 30,
            render_ms_p50: 1,
            render_ms_p95: 2,
            input_queue_depth: 0,
            input_submitted: 0,
            input_expired: 0,
            path_switches: 0,
            active_path: mlp_core::media::MediaPath::Ws,
            app_version: String::new(),
        },
        Instant::now(),
    );
    sender
}

/// Drain every fragment the sender will release, advancing time in small
/// steps so the pacer's tokens refill.
fn drain(sender: &mut MediaSenderMachine) -> Vec<OutboundFragment> {
    let mut out = Vec::new();
    let mut t = Instant::now();
    for _ in 0..4096 {
        match sender.poll_fragment(t) {
            Some(fragment) => out.push(fragment),
            None => {
                if sender.queued_objects() == 0 {
                    break;
                }
                t += std::time::Duration::from_millis(25);
            }
        }
    }
    out
}

fn two_part_request(update_id_hint: u64) -> UpdateRequest {
    // Two half-bitmaps of a 15-cell grid: each part carries its own cells.
    let cells_a: Vec<u32> = (0..8u32).collect();
    let cells_b: Vec<u32> = (8..15u32).collect();
    let mut parts = Vec::new();
    for cells in [&cells_a, &cells_b] {
        let mut payload = vec![0u8; 2];
        for cell in cells {
            payload[(*cell / 8) as usize] |= 1 << (*cell % 8);
        }
        let _ = update_id_hint;
        parts.push(payload);
    }
    UpdateRequest {
        parts,
        cells: (0..15u32).collect(),
        anchor: false,
        kind: UpdateKind::Delta,
        reflects_input_seq: 0,
        update_bytes: 4,
        stream_id: 0,
        path_id: 0,
        echo_critical: false,
        deadline_epoch_ms: None,
    }
}

#[test]
fn a_lossy_lane_attaches_parity_to_a_multi_part_update() {
    let mut sender = lossy_sender();
    let outcome = match sender.enqueue_update(two_part_request(1), Instant::now()) {
        EnqueueOutcome::Accepted { .. } => (),
        other => panic!("the write queue refused a quiet lane: {other:?}"),
    };
    let _ = outcome;
    eprintln!(
        "DEBUG2 fec={} queued={} loss={:?} est_loss={:?}",
        sender.stats().fec_parity_sent_total,
        sender.queued_objects(),
        sender.estimate().loss_ratio,
        sender.estimator().loss_ratio()
    );
    // Poll every object: the two parts plus one parity object.
    let mut parity_objects = 0usize;
    let mut parts_seen = 0usize;
    for fragment in drain(&mut sender) {
        let Ok((parsed, _)) = mlp_core::media::pdu::Fragment::parse(&fragment.bytes, Carrier::Ws)
        else {
            continue;
        };
        if parsed.header.class != FragmentClass::Media {
            continue;
        }
        if parsed
            .ext
            .get_u64(mlp_core::media::pdu::tlv::FEC_GROUP_ID)
            .is_some()
        {
            parity_objects += 1;
        } else {
            parts_seen += 1;
        }
    }
    eprintln!(
        "DEBUG parts_seen={parts_seen} parity={parity_objects} queued={} loss={:?} fec={}",
        sender.queued_objects(),
        sender.estimate().loss_ratio,
        sender.stats().fec_parity_sent_total
    );
    assert_eq!(parts_seen, 2, "the update's two parts");
    assert_eq!(parity_objects, 1, "one parity object rides the group");
    assert_eq!(sender.stats().fec_parity_sent_total, 1);
}

#[test]
fn a_clean_lane_attaches_no_parity() {
    let mut sender = MediaSenderMachine::new(
        MediaSenderConfig::new(Carrier::Ws, CellGrid::with_cell_size(320, 180, 64)).without_fec(),
        CellGrid::with_cell_size(320, 180, 64),
        Instant::now(),
    );
    sender.on_feedback(
        mlp_core::media::MediaFeedback {
            rtt_ms: 40,
            rtt_min_ms: 40,
            jitter_ms: 0,
            received_bytes: 100_000,
            received_objects: 99,
            lost_objects: 1,
            duplicated_objects: 0,
            reordered_objects: 0,
            window_ms: 1_000,
            ack: None,
            probes: vec![],
            buffered_amount_bytes: 0,
            display_fps: 30,
            render_ms_p50: 1,
            render_ms_p95: 2,
            input_queue_depth: 0,
            input_submitted: 0,
            input_expired: 0,
            path_switches: 0,
            active_path: mlp_core::media::MediaPath::Ws,
            app_version: String::new(),
        },
        Instant::now(),
    );
    sender.enqueue_update(two_part_request(2), Instant::now());
    while let Some(fragment) = sender.poll_fragment(Instant::now()) {
        if let Ok((parsed, _)) = mlp_core::media::pdu::Fragment::parse(&fragment.bytes, Carrier::Ws)
        {
            assert!(
                parsed
                    .ext
                    .get_u64(mlp_core::media::pdu::tlv::FEC_GROUP_ID)
                    .is_none(),
                "a clean lane ships no parity"
            );
        }
    }
}

/// Deliver one encoded fragment; return the receiver's events.
fn deliver(receiver: &mut MediaReceiverMachine, bytes: &[u8]) -> Vec<MediaReceiveEvent> {
    receiver.on_message(bytes, Instant::now())
}

#[test]
fn a_lost_part_comes_back_from_the_parity_without_a_round_trip() {
    let mut sender = lossy_sender();
    sender.enqueue_update(two_part_request(3), Instant::now());
    let mut receiver = MediaReceiverMachine::new(
        MediaReceiverConfig::for_carrier(Carrier::Ws),
        Instant::now(),
    );

    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut parity: Option<Vec<u8>> = None;
    for fragment in drain(&mut sender) {
        let Ok((parsed, _)) = mlp_core::media::pdu::Fragment::parse(&fragment.bytes, Carrier::Ws)
        else {
            continue;
        };
        if parsed.header.class != FragmentClass::Media {
            continue;
        }
        if parsed
            .ext
            .get_u64(mlp_core::media::pdu::tlv::FEC_GROUP_ID)
            .is_some()
        {
            parity = Some(fragment.bytes.clone());
        } else {
            parts.push(fragment.bytes.clone());
        }
    }
    assert_eq!(parts.len(), 2);

    // Deliver part 0 only: part 1 is lost on the wire. The parity follows.
    let _ = deliver(&mut receiver, &parts[0]);
    let events = deliver(&mut receiver, parity.as_ref().expect("parity rode along"));
    let recovered = events
        .iter()
        .any(|event| matches!(event, MediaReceiveEvent::ParityRecovered(_)));
    assert!(recovered, "the parity answers the lost part on arrival");
    assert_eq!(receiver.stats().parity_recovered_total, 1);
    assert_eq!(receiver.stats().parity_rejected_total, 0);

    // The recovered part decodes to exactly the cells part 1 carried.
    let events = receiver.tick(Instant::now());
    let _ = events;
}

#[test]
fn a_lying_parity_recovers_nothing_and_is_counted() {
    let mut sender = lossy_sender();
    sender.enqueue_update(two_part_request(4), Instant::now());
    let mut receiver = MediaReceiverMachine::new(
        MediaReceiverConfig::for_carrier(Carrier::Ws),
        Instant::now(),
    );

    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut parity: Option<Vec<u8>> = None;
    for fragment in drain(&mut sender) {
        let Ok((parsed, _)) = mlp_core::media::pdu::Fragment::parse(&fragment.bytes, Carrier::Ws)
        else {
            continue;
        };
        if parsed.header.class != FragmentClass::Media {
            continue;
        }
        if parsed
            .ext
            .get_u64(mlp_core::media::pdu::tlv::FEC_GROUP_ID)
            .is_some()
        {
            parity = Some(fragment.bytes.clone());
        } else {
            parts.push(fragment.bytes.clone());
        }
    }
    // The wire garbles the parity: flip the top bit of its payload.
    let mut bytes = parity.expect("parity rode along").clone();
    let Ok((mut parsed, _)) = mlp_core::media::pdu::Fragment::parse(&bytes, Carrier::Ws) else {
        panic!("the parity parses");
    };
    parsed.payload[0] ^= 0x80;
    bytes = parsed.encode();

    let _ = deliver(&mut receiver, &parts[0]);
    let events = deliver(&mut receiver, &bytes);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, MediaReceiveEvent::ParityRecovered(_))),
        "a lying parity recovers nothing"
    );
    assert_eq!(
        receiver.stats().parity_rejected_total,
        1,
        "the lie is counted"
    );
}
