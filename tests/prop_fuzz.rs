//! Property tests: the PDU codec never panics on arbitrary input.
use proptest::prelude::*;
use mlp_core::media::pdu::{Carrier, Fragment};

proptest! {
    #[test]
    fn fragment_parse_never_panics(data in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = Fragment::parse(&data, Carrier::Ws);
    }

    #[test]
    fn header_ext_parse_never_panics(data in proptest::collection::vec(any::<u8>(), 0..128)) {
        let _ = mlp_core::media::pdu::HeaderExt::parse(&data);
    }
}

#[test]
fn fragment_roundtrip() {
    use mlp_core::media::pdu::{Fragment, FragmentClass, FragmentFlags, Priority, UpdateMeta};
    let meta = UpdateMeta {
        update_id: 7, update_base: 6, part_index: 1, part_count: 4,
        update_bytes: 1234, chain_epoch: 3, reflects_input_seq: 99, stream_id: 0, path_id: 0, send_epoch_ms: 1000,
    };
    let ext = meta.to_ext();
    let frag = Fragment::new(FragmentClass::Media, FragmentFlags::NONE, Priority::MEDIA, 42, 0, 2, ext, vec![1,2,3,4]).unwrap();
    let bytes = frag.encode();
    let (parsed, consumed) = Fragment::parse(&bytes, Carrier::Ws).unwrap();
    assert_eq!(consumed, bytes.len());
    assert_eq!(parsed.header.object_id, 42);
    assert_eq!(parsed.payload, vec![1,2,3,4]);
}

proptest! {
    #[test]
    fn fragment_roundtrip_arbitrary(payload in proptest::collection::vec(any::<u8>(), 0..64)) {
        use mlp_core::media::pdu::{Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, UpdateMeta};
        let meta = UpdateMeta {
            update_id: 42, update_base: 0, part_index: 0, part_count: 1,
            update_bytes: payload.len() as u32, chain_epoch: 1, reflects_input_seq: 0,
            stream_id: 0, path_id: 0, send_epoch_ms: 100,
        };
        let ext = meta.to_ext();
        let frag = Fragment::new(FragmentClass::Media, FragmentFlags::NONE, Priority::MEDIA, 7, 0, 1, ext, payload.clone()).unwrap();
        let bytes = frag.encode();
        let (parsed, consumed) = Fragment::parse(&bytes, Carrier::Ws).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed.payload, payload);
        assert_eq!(parsed.header.object_id, 7);
    }
}
