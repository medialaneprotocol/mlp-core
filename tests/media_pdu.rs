//! MLP v1 byte-level conformance (design §6.1–§6.3, §6, §6.7).
//!
//! These tests pin the *wire*: every field offset, every validation reason and
//! every carrier rule. A change here is a protocol change and needs a corpus
//! digest bump (design §13).

use mlp_core::media::error::{MediaErrorCode, RejectReason};
use mlp_core::media::pdu::{
    Carrier, FRAGMENT_FIXED_HEADER, FRAGMENT_MAGIC, Fragment, FragmentClass, FragmentFlags,
    HeaderExt, MAX_FRAGMENT_COUNT, MAX_GRPC_COALESCE, MAX_HEADER_EXT, MAX_WS_FRAGMENT,
    MEDIA_PROTOCOL_VERSION, Priority, TARGET_FRAGMENT_PAYLOAD, UpdateMeta, tlv,
};
use mlp_core::media::{
    MediaAck, MediaAnchorPlan, MediaClose, MediaControlObject, MediaCursor, MediaFeedback,
    MediaHello, MediaHelloAck, MediaPath, capability,
};

/// The design's frozen byte layout, byte for byte.
#[test]
fn fragment_header_layout_is_frozen() {
    let mut ext = HeaderExt::new();
    ext.push_u64(tlv::UPDATE_ID, 0x0807_0605_0403_0201)
        .expect("fits");
    let fragment = Fragment::new(
        FragmentClass::Media,
        FragmentFlags::FIN.union(FragmentFlags::ANCHOR),
        Priority::MEDIA,
        0x0D0C_0B0A,
        0,
        1,
        ext,
        vec![0xAA, 0xBB],
    )
    .expect("valid fragment");
    let bytes = fragment.encode();
    assert_eq!(&bytes[0..4], &FRAGMENT_MAGIC);
    assert_eq!(&bytes[0..4], b"MLP\0");
    assert_eq!(bytes[4], MEDIA_PROTOCOL_VERSION);
    assert_eq!(bytes[5], 2, "MEDIA class");
    assert_eq!(bytes[6], 0x03, "FIN | ANCHOR");
    assert_eq!(bytes[7], 6, "interactive media priority");
    assert_eq!(&bytes[8..12], &0x0D0C_0B0Au32.to_le_bytes(), "object_id LE");
    assert_eq!(&bytes[12..14], &0u16.to_le_bytes());
    assert_eq!(&bytes[14..16], &1u16.to_le_bytes());
    assert_eq!(&bytes[16..20], &2u32.to_le_bytes(), "payload_len");
    assert_eq!(&bytes[20..22], &10u16.to_le_bytes(), "one u64 TLV");
    assert_eq!(bytes[22], tlv::UPDATE_ID);
    assert_eq!(bytes[23], 8);
    assert_eq!(
        &bytes[24..32],
        &0x0807_0605_0403_0201u64.to_le_bytes(),
        "TLV value LE"
    );
    assert_eq!(&bytes[32..34], &[0xAA, 0xBB]);
    assert_eq!(FRAGMENT_FIXED_HEADER, 22);
    assert_eq!(bytes.len(), FRAGMENT_FIXED_HEADER + 10 + 2);
}

#[test]
fn constants_match_the_design() {
    assert_eq!(MAX_WS_FRAGMENT, 65_536);
    assert_eq!(MAX_GRPC_COALESCE, 65_536);
    assert_eq!(TARGET_FRAGMENT_PAYLOAD, 1_150);
    assert_eq!(MAX_FRAGMENT_COUNT, 4_096);
    assert_eq!(MAX_HEADER_EXT, 256);
    assert_eq!(FRAGMENT_FIXED_HEADER, 22);
    assert_eq!(Carrier::Ws.fragment_payload_max(), MAX_WS_FRAGMENT);
    assert_eq!(
        Carrier::DataChannel.fragment_payload_max(),
        TARGET_FRAGMENT_PAYLOAD
    );
    assert_eq!(Carrier::RepairChannel.fragment_payload_max(), 1_150);
    assert_eq!(Carrier::ControlChannel.fragment_payload_max(), 1_150);
    assert!(Carrier::DataChannel.is_datagram());
    assert!(Carrier::Ws.is_stream());
    assert!(Carrier::Grpc.is_stream());
}

#[test]
fn every_class_round_trips_on_every_carrier() {
    for carrier in [
        Carrier::Ws,
        Carrier::Grpc,
        Carrier::DataChannel,
        Carrier::RepairChannel,
        Carrier::ControlChannel,
    ] {
        let cap = carrier.fragment_payload_max();
        let payload = vec![0x5A; cap.min(64)];
        for (class, priority) in [
            (FragmentClass::Control, Priority::CONTROL),
            (FragmentClass::Cursor, Priority::CURSOR),
            (FragmentClass::Media, Priority::MEDIA),
            (FragmentClass::Feedback, Priority::REPORT),
            (FragmentClass::Path, Priority::CONTROL),
        ] {
            let mut ext = HeaderExt::new();
            ext.push_u32(tlv::BUDGET_BYTES, 4_096).expect("fits");
            ext.push_u8(tlv::PATH_HINT, MediaPath::DcDirect.as_u8())
                .expect("fits");
            let fragment = Fragment::new(
                class,
                FragmentFlags::FIN,
                priority,
                42,
                0,
                1,
                ext,
                payload.clone(),
            )
            .expect("valid");
            let bytes = fragment.encode();
            let (parsed, consumed) = Fragment::parse(&bytes, carrier).expect("parses");
            assert_eq!(consumed, bytes.len());
            assert_eq!(parsed, fragment, "{} {}", carrier.as_str(), class.as_str());
            let message =
                mlp_core::media::pdu::parse_message(&bytes, carrier).expect("one message");
            assert_eq!(message.len(), 1);
        }
    }
}

#[test]
fn multi_fragment_objects_are_legal_only_on_stream_carriers() {
    let first = Fragment::new(
        FragmentClass::Feedback,
        FragmentFlags::NONE,
        Priority::REPORT,
        9,
        0,
        3,
        HeaderExt::new(),
        vec![1; 8],
    )
    .expect("valid");
    let second = Fragment::new(
        FragmentClass::Feedback,
        FragmentFlags::FIN,
        Priority::REPORT,
        9,
        2,
        3,
        HeaderExt::new(),
        vec![2; 8],
    )
    .expect("valid");
    let middle = Fragment::new(
        FragmentClass::Feedback,
        FragmentFlags::NONE,
        Priority::REPORT,
        9,
        1,
        3,
        HeaderExt::new(),
        vec![3; 8],
    )
    .expect("valid");
    let mut message = first.encode();
    message.extend(middle.encode());
    message.extend(second.encode());

    let parsed = mlp_core::media::pdu::parse_message(&message, Carrier::Grpc).expect("stream");
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[2].header.fragment_index, 2);
    assert!(parsed[2].is_fin());

    assert_eq!(
        mlp_core::media::pdu::parse_message(&message, Carrier::DataChannel),
        Err(RejectReason::MultiFragmentOnDatagram)
    );
}

#[test]
fn every_validation_reason_is_reachable() {
    let valid = Fragment::new(
        FragmentClass::Media,
        FragmentFlags::FIN,
        Priority::MEDIA,
        7,
        0,
        1,
        HeaderExt::new(),
        vec![0; 4],
    )
    .expect("valid");
    let base = valid.encode();

    // BAD_MAGIC: the whole transport message is rejected.
    let mut broken = base.clone();
    broken[0] = b'X';
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadMagic)
    );

    // UNSUPPORTED_VERSION.
    let mut broken = base.clone();
    broken[4] = 2;
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::UnsupportedVersion)
    );

    // BAD_CLASS.
    let mut broken = base.clone();
    broken[5] = 9;
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadClass)
    );

    // BAD_FLAGS (bit 7 reserved).
    let mut broken = base.clone();
    broken[6] = 0x80;
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadFlags)
    );

    // BAD_PRIORITY outside 0..=7 ...
    let mut broken = base.clone();
    broken[7] = 200;
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadPriority)
    );
    // ... and inside, but reserved for control on a media fragment.
    let mut broken = base.clone();
    broken[7] = 1;
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadPriority)
    );

    // BAD_FRAGMENT: index >= count, count 0, count > 4096.
    let mut broken = base.clone();
    broken[12..14].copy_from_slice(&1u16.to_le_bytes());
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadFragment)
    );
    let mut broken = base.clone();
    broken[14..16].copy_from_slice(&0u16.to_le_bytes());
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadFragment)
    );
    let mut broken = base.clone();
    broken[14..16].copy_from_slice(&4_097u16.to_le_bytes());
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadFragment)
    );

    // BAD_LENGTH: above the carrier cap and truncation.
    let mut broken = base.clone();
    broken[16..20].copy_from_slice(&((TARGET_FRAGMENT_PAYLOAD + 1) as u32).to_le_bytes());
    assert_eq!(
        Fragment::parse(&broken, Carrier::DataChannel),
        Err(RejectReason::BadLength)
    );
    assert_eq!(
        Fragment::parse(&base[..base.len() - 1], Carrier::Ws),
        Err(RejectReason::BadLength)
    );

    // BAD_EXT: odd length, above 256, and a length that runs past the range.
    let mut broken = base.clone();
    broken[20..22].copy_from_slice(&2u16.to_le_bytes());
    assert_eq!(
        Fragment::parse(&broken[..FRAGMENT_FIXED_HEADER + 1], Carrier::Ws),
        Err(RejectReason::BadLength)
    );
    let mut broken = base.clone();
    broken[20..22].copy_from_slice(&300u16.to_le_bytes());
    assert_eq!(
        Fragment::parse(&broken, Carrier::Ws),
        Err(RejectReason::BadExt)
    );

    // META_MISMATCH is a comparison, not a parse failure.
    let meta = UpdateMeta {
        update_id: 1,
        update_base: 0,
        part_index: 0,
        part_count: 1,
        update_bytes: 10,
        chain_epoch: 0,
        reflects_input_seq: 0,
        send_epoch_ms: 0,
    };
    let mut other = meta;
    other.part_count = 2;
    assert_eq!(meta.check_against(&other), Err(RejectReason::MetaMismatch));
    assert_eq!(meta.check_against(&meta), Ok(()));
}

#[test]
fn tlv_registry_is_frozen() {
    assert_eq!(tlv::UPDATE_ID, 0x01);
    assert_eq!(tlv::UPDATE_BASE, 0x02);
    assert_eq!(tlv::PART, 0x03);
    assert_eq!(tlv::UPDATE_BYTES, 0x04);
    assert_eq!(tlv::CHAIN_EPOCH, 0x05);
    assert_eq!(tlv::REFLECTS_INPUT_SEQ, 0x06);
    assert_eq!(tlv::SEND_EPOCH_MS, 0x07);
    assert_eq!(tlv::DEADLINE_EPOCH_MS, 0x08);
    assert_eq!(tlv::ECHO_OF_OBJECT_ID, 0x09);
    assert_eq!(tlv::BUDGET_BYTES, 0x0A);
    assert_eq!(tlv::PACE_BPS, 0x0B);
    assert_eq!(tlv::PATH_HINT, 0x0C);
    assert_eq!(tlv::INPUT_SEQ, 0x0D);
    assert_eq!(tlv::CELL_COUNT, 0x0E);
    assert_eq!(tlv::COST_MS, 0x0F);
    assert!(tlv::is_reserved(0x10));
    assert!(tlv::is_reserved(0xEF));
    assert!(tlv::is_experimental(0xF0));
    assert!(tlv::is_experimental(0xFF));
    assert!(tlv::is_known(0x0F));
    assert!(!tlv::is_known(0x10));
}

#[test]
fn unknown_tlvs_are_skipped_and_experimental_ones_counted() {
    let mut ext = HeaderExt::new();
    ext.push_raw(0x10, vec![1, 2, 3]).expect("reserved fits");
    ext.push_raw(0xF5, vec![9]).expect("experimental fits");
    ext.push_u64(tlv::SEND_EPOCH_MS, 1_234).expect("fits");
    let encoded = ext.encode().expect("encodes");
    let decoded = HeaderExt::parse(&encoded).expect("parses");
    assert_eq!(decoded.reserved_count(), 1);
    assert_eq!(decoded.experimental_count(), 1);
    assert_eq!(decoded.send_epoch_ms(), Some(1_234));
    assert_eq!(decoded.entries().len(), 3);
}

#[test]
fn header_extension_cap_is_enforced_by_the_encoder() {
    let mut ext = HeaderExt::new();
    for _ in 0..25 {
        ext.push_raw(0x80, vec![0; 8]).expect("250 B fits");
    }
    assert_eq!(ext.encoded_len(), 250);
    assert_eq!(ext.encode().map(|bytes| bytes.len()), Ok(250));
    assert_eq!(ext.push_raw(0x80, vec![0; 8]), Err(RejectReason::BadExt));
    assert_eq!(
        ext.push_raw(0x80, vec![0; 300]),
        Err(RejectReason::BadExt),
        "a TLV value is at most 255 bytes"
    );
}

#[test]
fn control_bodies_validate_their_limits() {
    let hello = MediaHello {
        protocol_version: 1,
        media_session_id: String::new(),
        session_id: "s".into(),
        max_fragment_payload: 65_536,
        cell_size: 64,
        cell_cols: 30,
        cell_rows: 17,
        capabilities: vec![capability::ANCHOR.into(), capability::PATCH.into()],
        encoder_max_fps: 30,
        start_epoch_ms: 0,
    };
    assert_eq!(MediaControlObject::Hello(hello.clone()).validate(), Ok(()));
    let mut wrong = hello;
    wrong.protocol_version = 2;
    assert_eq!(
        MediaControlObject::Hello(wrong).validate(),
        Err(RejectReason::UnsupportedVersion)
    );

    let ack = MediaHelloAck {
        protocol_version: 1,
        media_session_id: "m".into(),
        accepted_fragment_payload: 1_150,
        cell_size: 64,
        cell_cols: 30,
        cell_rows: 17,
        capabilities: vec![],
        resume_ttl_seconds: 900,
        media_supported: false,
        media_unsupported_reason: None,
    };
    assert_eq!(
        MediaControlObject::HelloAck(ack).validate(),
        Err(RejectReason::BodyInvalid),
        "unsupported needs a typed reason"
    );

    let plan = MediaAnchorPlan {
        plan_id: 1,
        total_frames_estimate: 3,
        total_cells: 0,
        priority: 6,
    };
    assert_eq!(
        MediaControlObject::AnchorPlan(plan).validate(),
        Err(RejectReason::BodyInvalid)
    );

    let close = MediaClose {
        code: mlp_core::media::lifecycle::MediaCloseCode::Park,
        reason: "park".into(),
        park: true,
    };
    assert_eq!(MediaControlObject::Close(close).validate(), Ok(()));
}

#[test]
fn cursor_objects_are_never_fragmented_and_stay_small() {
    let cursor = MediaCursor {
        x_px: -5,
        y_px: 100,
        visible: true,
        shape_seq: 0,
        epoch_ms: 7,
        hot_spot_x: 0,
        hot_spot_y: 0,
        shape_rgba: Vec::new(),
    };
    assert_eq!(cursor.validate(), Ok(()));
    // A shape is only legal with a generation and at most 8 KiB.
    let mut shaped = cursor.clone();
    shaped.shape_seq = 3;
    shaped.shape_rgba = vec![0; 8_192];
    assert_eq!(shaped.validate(), Ok(()));
    shaped.shape_rgba.push(0);
    assert_eq!(shaped.validate(), Err(RejectReason::BodyInvalid));
}

#[test]
fn ack_validation_matches_the_design() {
    // Bit i = frame (highest - i); frame ids start at 1.
    let good = MediaAck {
        highest_applied_update_id: 10,
        ack_bitmap: 0b1_0101,
        ..MediaAck::default()
    };
    assert_eq!(good.validate(), Ok(()));
    assert!(good.is_applied(10));
    assert!(good.is_applied(8));
    assert!(!good.is_applied(9));
    assert!(!good.is_applied(0));

    let above = MediaAck {
        highest_applied_update_id: 0,
        ack_bitmap: 1,
        ..MediaAck::default()
    };
    assert_eq!(above.validate(), Err(RejectReason::AckBadBitmap));

    let counts = MediaAck {
        highest_applied_update_id: 1,
        ack_bitmap: 1,
        applied_cell_count: 10,
        stale_cells: 11,
        ..MediaAck::default()
    };
    assert_eq!(counts.validate(), Err(RejectReason::AckBadCount));
}

#[test]
fn feedback_measurements_follow_the_design() {
    let feedback = MediaFeedback {
        rtt_ms: 0,
        received_bytes: 4_000,
        received_objects: 19,
        lost_objects: 1,
        window_ms: 500,
        ack: Some(MediaAck {
            highest_applied_update_id: 3,
            ack_bitmap: 0b111,
            ..MediaAck::default()
        }),
        ..MediaFeedback::default()
    };
    assert_eq!(feedback.goodput_bps(), Some(64_000));
    let loss = feedback.loss_ratio().expect("a window with objects");
    assert!((loss - 0.05).abs() < 1e-12, "loss {loss}");
    assert_eq!(feedback.clamped_rtt_ms(), 1, "rtt is clamped to 1..=60000");
    assert_eq!(feedback.validate(), Ok(()));

    let empty = MediaFeedback::default();
    assert_eq!(empty.goodput_bps(), None);
    assert_eq!(empty.loss_ratio(), None, "no objects is not zero loss");
}

#[test]
fn error_codes_are_frozen() {
    for (code, value) in [
        (MediaErrorCode::BadMagic, 1u32),
        (MediaErrorCode::UnsupportedVersion, 2),
        (MediaErrorCode::BadClass, 3),
        (MediaErrorCode::BadFlags, 4),
        (MediaErrorCode::BadPriority, 5),
        (MediaErrorCode::BadFragment, 6),
        (MediaErrorCode::BadLength, 7),
        (MediaErrorCode::BadExt, 8),
        (MediaErrorCode::MetaMismatch, 9),
        (MediaErrorCode::AssemblyTimeout, 10),
        (MediaErrorCode::AssemblyEvicted, 11),
        (MediaErrorCode::ObjectIdWrap, 12),
        (MediaErrorCode::BodyInvalid, 13),
        (MediaErrorCode::RateLimited, 14),
        (MediaErrorCode::LaneDisabled, 15),
    ] {
        assert_eq!(code.as_u32(), value);
        assert_eq!(MediaErrorCode::from_u32(value), Some(code));
    }
    assert_eq!(MediaErrorCode::from_u32(0), None);
    assert_eq!(MediaErrorCode::from_u32(16), None);
}

#[test]
fn frame_meta_round_trips_and_checks_consistency() {
    let meta = UpdateMeta {
        update_id: 100,
        update_base: 99,
        part_index: 1,
        part_count: 2,
        update_bytes: 40_000,
        chain_epoch: 5,
        reflects_input_seq: 42,
        send_epoch_ms: 1_000,
    };
    let ext = meta.to_ext();
    assert_eq!(ext.update_id(), Some(100));
    assert_eq!(ext.update_base(), Some(99));
    assert_eq!(ext.chunk(), Some((1, 2)));
    assert_eq!(ext.update_bytes(), Some(40_000));
    assert_eq!(ext.chain_epoch(), Some(5));
    assert_eq!(ext.reflects_input_seq(), Some(42));
    assert_eq!(ext.send_epoch_ms(), Some(1_000));
    assert_eq!(UpdateMeta::from_ext(&ext), Some(meta));
    // UPDATE_ID is the one required field.
    assert_eq!(UpdateMeta::from_ext(&HeaderExt::new()), None);
    let mut without_frame = HeaderExt::new();
    without_frame.push_u64(tlv::SEND_EPOCH_MS, 5).expect("fits");
    assert_eq!(UpdateMeta::from_ext(&without_frame), None);
}
