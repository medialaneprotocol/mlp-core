//! MLP object lifecycle and assembly (design §6.4, §6.5) — INV-1's home.
//!
//! A partial update is never delivered, duplicates never re-open an object, the
//! table is bounded, and the assembly timeout is RTT-relative.

use std::time::{Duration, Instant};

use mlp_core::media::error::RejectReason;
use mlp_core::media::object::{
    AssemblyConfig, AssemblyOutcome, MAX_ASSEMBLY_BYTES, MAX_OPEN_OBJECTS, MediaObject,
    ObjectAssembler,
};
use mlp_core::media::pdu::{
    Carrier, Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, tlv,
};

fn ext_with_update(update_id: u64) -> HeaderExt {
    let mut ext = HeaderExt::new();
    ext.push_u64(tlv::UPDATE_ID, update_id).expect("fits");
    ext.push_u32(tlv::CELL_COUNT, 510).expect("fits");
    ext
}

fn fragment(
    class: FragmentClass,
    object_id: u32,
    index: u16,
    count: u16,
    fin: bool,
    payload: Vec<u8>,
    ext: HeaderExt,
) -> Fragment {
    let mut flags = FragmentFlags::NONE;
    if fin {
        flags = flags.union(FragmentFlags::FIN);
    }
    let priority = match class {
        FragmentClass::Media => Priority::MEDIA,
        _ => Priority::CONTROL,
    };
    Fragment::new(
        class, flags, priority, object_id, index, count, ext, payload,
    )
    .expect("valid fragment")
}

fn assembler() -> ObjectAssembler {
    ObjectAssembler::new(AssemblyConfig::default())
}

/// INV-1: a update is applied only if every chunk arrived.
#[test]
fn partial_objects_are_never_delivered() {
    let now = Instant::now();
    let mut assembler = assembler();
    // A four-fragment object missing its last fragment.
    for index in 0..3u16 {
        let outcome = assembler.accept(
            fragment(
                FragmentClass::Media,
                1,
                index,
                4,
                false,
                vec![index as u8; 10],
                if index == 0 {
                    ext_with_update(1)
                } else {
                    HeaderExt::new()
                },
            ),
            0,
            now,
        );
        assert_eq!(outcome, AssemblyOutcome::Pending);
    }
    // The last fragment never arrives: after the assembly timeout the object is
    // dropped, and no partial payload was ever handed out.
    let expired = assembler.expire(now + Duration::from_secs(5));
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].reason, RejectReason::AssemblyTimeout);
    assert_eq!(expired[0].received, 3);
    assert_eq!(expired[0].declared, 4);
    assert_eq!(assembler.open_objects(), 0);
}

#[test]
fn out_of_order_fragments_assemble_in_index_order() {
    let now = Instant::now();
    let mut assembler = assembler();
    let order = [3u16, 0, 2, 1];
    let mut complete = None;
    for index in order {
        let outcome = assembler.accept(
            fragment(
                FragmentClass::Media,
                5,
                index,
                4,
                index == 3,
                vec![index as u8],
                if index == 0 {
                    ext_with_update(9)
                } else {
                    HeaderExt::new()
                },
            ),
            0,
            now,
        );
        if let AssemblyOutcome::Object(object) = outcome {
            complete = Some(object);
        }
    }
    let object = complete.expect("the object completes when the last index and FIN arrive");
    assert_eq!(object.payload, vec![0, 1, 2, 3]);
    assert_eq!(object.update_meta().map(|meta| meta.update_id), Some(9));
    assert_eq!(ObjectAssembler::cell_count_hint(&object), Some(510));
}

#[test]
fn fin_is_required_even_when_every_index_arrived() {
    let now = Instant::now();
    let mut assembler = assembler();
    for index in 0..2u16 {
        let outcome = assembler.accept(
            fragment(
                FragmentClass::Media,
                6,
                index,
                2,
                false,
                vec![1],
                ext_with_update(2),
            ),
            0,
            now,
        );
        assert_eq!(outcome, AssemblyOutcome::Pending);
    }
    assert_eq!(assembler.open_received(6), Some((2, 2, false)));
    assert_eq!(assembler.open_objects(), 1);
}

#[test]
fn duplicate_fragments_and_completed_objects_are_dropped() {
    let now = Instant::now();
    let mut assembler = assembler();
    let fragment = fragment(
        FragmentClass::Media,
        7,
        0,
        1,
        true,
        vec![1, 2, 3],
        ext_with_update(3),
    );
    assert!(matches!(
        assembler.accept(fragment.clone(), 0, now),
        AssemblyOutcome::Object(_)
    ));
    // A retransmitted copy of a completed object is a duplicate, never a
    // second application.
    assert_eq!(
        assembler.accept(fragment, 0, now),
        AssemblyOutcome::Dropped {
            reason: RejectReason::Duplicate,
            object_id: Some(7)
        }
    );
    assert_eq!(assembler.duplicate_fragments_total(), 1);
}

#[test]
fn dedupe_window_is_bounded_and_evicts_in_order() {
    let mut config = AssemblyConfig::default();
    config.dedup_window = 16;
    let mut assembler = ObjectAssembler::new(config);
    let now = Instant::now();
    for object_id in 0..64u32 {
        assembler.accept(
            fragment(
                FragmentClass::Media,
                object_id,
                0,
                2,
                false,
                vec![0],
                ext_with_update(u64::from(object_id)),
            ),
            0,
            now,
        );
    }
    assert_eq!(assembler.dedupe_window_len(), 16);
}

#[test]
fn fragment_mismatch_is_rejected() {
    let now = Instant::now();
    let mut assembler = assembler();
    assembler.accept(
        fragment(
            FragmentClass::Media,
            8,
            0,
            3,
            false,
            vec![1],
            ext_with_update(1),
        ),
        0,
        now,
    );
    assert_eq!(
        assembler.accept(
            fragment(
                FragmentClass::Media,
                8,
                1,
                2,
                false,
                vec![1],
                HeaderExt::new()
            ),
            0,
            now
        ),
        AssemblyOutcome::Dropped {
            reason: RejectReason::FragmentMismatch,
            object_id: Some(8)
        }
    );
    // A class change on the same object id is just as wrong. (Index 1 is
    // already in the dedupe window, so the retry uses index 2.)
    assert_eq!(
        assembler.accept(
            fragment(
                FragmentClass::Cursor,
                8,
                2,
                3,
                false,
                vec![1],
                HeaderExt::new()
            ),
            0,
            now
        ),
        AssemblyOutcome::Dropped {
            reason: RejectReason::FragmentMismatch,
            object_id: Some(8)
        }
    );
}

#[test]
fn deadlines_drop_objects_but_zero_never_expires() {
    let now = Instant::now();
    let mut assembler = assembler();
    let mut deadline = HeaderExt::new();
    deadline
        .push_u64(tlv::DEADLINE_EPOCH_MS, 1_000)
        .expect("fits");
    assert_eq!(
        assembler.accept(
            fragment(
                FragmentClass::Control,
                9,
                0,
                1,
                true,
                vec![0],
                deadline.clone()
            ),
            1_001,
            now
        ),
        AssemblyOutcome::Dropped {
            reason: RejectReason::Expired,
            object_id: Some(9)
        }
    );
    // Exactly at the deadline is still in time.
    assert!(matches!(
        assembler.accept(
            fragment(FragmentClass::Control, 9, 0, 1, true, vec![0], deadline),
            1_000,
            now
        ),
        AssemblyOutcome::Object(_)
    ));
    let mut never = HeaderExt::new();
    never.push_u64(tlv::DEADLINE_EPOCH_MS, 0).expect("fits");
    assert!(matches!(
        assembler.accept(
            fragment(FragmentClass::Control, 10, 0, 1, true, vec![0], never),
            1_000_000,
            now
        ),
        AssemblyOutcome::Object(_)
    ));
}

#[test]
fn open_object_budget_evicts_the_oldest() {
    let now = Instant::now();
    let mut config = AssemblyConfig::default();
    config.max_open_objects = 2;
    let mut assembler = ObjectAssembler::new(config);
    for object_id in 1..=3u32 {
        assembler.accept(
            fragment(
                FragmentClass::Media,
                object_id,
                0,
                2,
                false,
                vec![0; 8],
                ext_with_update(u64::from(object_id)),
            ),
            0,
            now + Duration::from_millis(u64::from(object_id)),
        );
    }
    let evictions = assembler.take_evictions();
    assert_eq!(evictions.len(), 1);
    assert_eq!(evictions[0].object_id, 1);
    assert_eq!(evictions[0].reason, RejectReason::AssemblyEvicted);
    assert_eq!(assembler.open_objects(), MAX_OPEN_OBJECTS.min(2));
    assert_eq!(assembler.evicted_total(), 1);
}

#[test]
fn assembly_byte_budget_is_honoured_under_pressure() {
    let now = Instant::now();
    let mut config = AssemblyConfig::default();
    config.max_assembly_bytes = 4_096;
    let mut assembler = ObjectAssembler::new(config);
    // Each fragment is 1 KiB; the table may hold at most 4 KiB.
    for object_id in 0..8u32 {
        assembler.accept(
            fragment(
                FragmentClass::Media,
                object_id,
                0,
                4,
                false,
                vec![0; 1_024],
                ext_with_update(u64::from(object_id)),
            ),
            0,
            now,
        );
    }
    assert!(assembler.open_bytes() <= 4_096);
    assert!(assembler.open_objects() <= MAX_OPEN_OBJECTS);
    assert!(assembler.evicted_total() >= 4);
}

#[test]
fn all_open_objects_together_stay_inside_the_design_budget() {
    // The defaults are the design's numbers.
    let config = AssemblyConfig::default();
    assert_eq!(config.max_open_objects, MAX_OPEN_OBJECTS);
    assert_eq!(config.max_assembly_bytes, MAX_ASSEMBLY_BYTES);
    assert_eq!(config.dedup_window, 4_096);
}

#[test]
fn assembly_timeout_is_injected_and_bounded() {
    let now = Instant::now();
    let mut assembler = assembler();
    assembler.set_assembly_timeout(Duration::from_millis(800));
    assert_eq!(assembler.assembly_timeout(), Duration::from_millis(800));
    assembler.accept(
        fragment(
            FragmentClass::Media,
            11,
            0,
            2,
            false,
            vec![0],
            ext_with_update(1),
        ),
        0,
        now,
    );
    assert_eq!(
        assembler.poll_timeout(now),
        Some(Duration::from_millis(800))
    );
    assert!(
        assembler
            .expire(now + Duration::from_millis(799))
            .is_empty()
    );
    assert_eq!(assembler.expire(now + Duration::from_millis(800)).len(), 1);
}

#[test]
fn body_classification_rejects_media_without_metadata() {
    let now = Instant::now();
    let mut assembler = assembler();
    let outcome = assembler.accept(
        fragment(
            FragmentClass::Media,
            12,
            0,
            1,
            true,
            vec![0; 8],
            HeaderExt::new(),
        ),
        0,
        now,
    );
    match outcome {
        AssemblyOutcome::Object(object) => {
            assert_eq!(
                ObjectAssembler::classify(&object),
                Err(RejectReason::BodyInvalid)
            );
        }
        other => panic!("expected an object, got {other:?}"),
    }
}

#[test]
fn media_object_helpers_expose_the_metadata() {
    let now = Instant::now();
    let mut assembler = assembler();
    let mut ext = ext_with_update(77);
    ext.push_u32(tlv::ECHO_OF_OBJECT_ID, 5).expect("fits");
    ext.push_u64(tlv::SEND_EPOCH_MS, 1_500).expect("fits");
    let outcome = assembler.accept(
        Fragment::new(
            FragmentClass::Media,
            FragmentFlags::FIN
                .union(FragmentFlags::DUP)
                .union(FragmentFlags::ANCHOR),
            Priority::MEDIA,
            13,
            0,
            1,
            ext,
            vec![0; 100],
        )
        .expect("valid"),
        0,
        now,
    );
    match outcome {
        AssemblyOutcome::Object(object) => {
            assert!(object.is_media());
            assert!(object.is_duplicate());
            assert!(object.is_anchor());
            assert_eq!(object.echo_of(), Some(5));
            assert_eq!(object.byte_len(), 100);
            assert_eq!(object.ext.send_epoch_ms(), Some(1_500));
            assert_eq!(object.update_meta().map(|meta| meta.update_id), Some(77));
            assert_eq!(ObjectAssembler::classify(&object), Ok(()));
        }
        other => panic!("expected an object, got {other:?}"),
    }
}

#[test]
fn completed_objects_are_not_reopened_by_late_fragments() {
    // Two fragments: the second arrives, then the first twice.
    let now = Instant::now();
    let mut assembler = assembler();
    assembler.accept(
        fragment(
            FragmentClass::Media,
            14,
            1,
            2,
            true,
            vec![2],
            HeaderExt::new(),
        ),
        0,
        now,
    );
    let first = fragment(
        FragmentClass::Media,
        14,
        0,
        2,
        false,
        vec![1],
        ext_with_update(1),
    );
    match assembler.accept(first.clone(), 0, now) {
        AssemblyOutcome::Object(object) => assert_eq!(object.payload, vec![1, 2]),
        other => panic!("expected the object, got {other:?}"),
    }
    assert_eq!(
        assembler.accept(first, 0, now),
        AssemblyOutcome::Dropped {
            reason: RejectReason::Duplicate,
            object_id: Some(14)
        }
    );
}

#[test]
fn media_objects_on_the_datagram_carrier_are_single_fragment_by_construction() {
    // The carrier rule is enforced at parse time; the assembler itself only
    // ever sees objects the parser accepted.
    let fragment = fragment(
        FragmentClass::Media,
        15,
        0,
        1,
        true,
        vec![0; 1_150],
        ext_with_update(1),
    );
    let bytes = fragment.encode();
    let parsed = mlp_core::media::pdu::parse_message(&bytes, Carrier::DataChannel)
        .expect("one fragment fits the MTU");
    assert_eq!(parsed.len(), 1);
    let mut assembler = assembler();
    let outcome = assembler.accept(parsed[0].clone(), 0, Instant::now());
    assert!(matches!(outcome, AssemblyOutcome::Object(_)));
    let _: Option<&MediaObject> = None;
}
