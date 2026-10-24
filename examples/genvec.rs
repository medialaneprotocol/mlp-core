use mlp_core::media::pdu::{Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, UpdateMeta};
fn main() {
    let meta = UpdateMeta {
        update_id: 7,
        update_base: 6,
        part_index: 1,
        part_count: 4,
        update_bytes: 1234,
        chain_epoch: 3,
        reflects_input_seq: 99,
        stream_id: 0,
        path_id: 0,
        send_epoch_ms: 1000,
    };
    let ext = meta.to_ext();
    let frag = Fragment::new(
        FragmentClass::Media,
        FragmentFlags::NONE,
        Priority::MEDIA,
        42,
        0,
        2,
        ext,
        vec![1,2,3,4],
    ).unwrap();
    let bytes = frag.encode();
    for b in &bytes { print!("{:02x}", b); }
    println!();
}
