use mlp_core::media::pdu::{Carrier, Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, UpdateMeta, tlv};

fn main() {
    // Vector 1: minimal control fragment (no ext)
    let frag = Fragment::new(FragmentClass::Control, FragmentFlags::NONE, Priority::CONTROL, 1, 0, 1, HeaderExt::new(), vec![]).unwrap();
    println!("control: {}", hex(&frag.encode()));

    // Vector 2: cursor fragment
    let frag = Fragment::new(FragmentClass::Cursor, FragmentFlags::NONE, Priority::CURSOR, 2, 0, 1, HeaderExt::new(), vec![0xAA, 0xBB]).unwrap();
    println!("cursor: {}", hex(&frag.encode()));

    // Vector 3: feedback fragment with echo
    let mut ext = HeaderExt::new();
    ext.push_u64(tlv::UPDATE_ID, 99);
    let frag = Fragment::new(FragmentClass::Feedback, FragmentFlags::NONE, Priority::REPORT, 3, 0, 1, ext, vec![]).unwrap();
    println!("feedback: {}", hex(&frag.encode()));

    // Vector 4: media with anchor flag
    let meta = UpdateMeta { update_id: 5, update_base: 0, part_index: 0, part_count: 1, update_bytes: 10, chain_epoch: 1, reflects_input_seq: 0, send_epoch_ms: 500 };
    let ext = meta.to_ext();
    let frag = Fragment::new(FragmentClass::Media, FragmentFlags::ANCHOR, Priority::MEDIA, 4, 0, 1, ext, vec![0; 10]).unwrap();
    println!("media-anchor: {}", hex(&frag.encode()));
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}
