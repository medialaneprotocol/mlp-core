//! Greasing: send an unknown TLV kind so receivers exercise skip paths.
use mlp_core::media::pdu::{HeaderExt, tlv};

fn main() {
    let mut ext = HeaderExt::new();
    let _ = ext.push_u64(tlv::UPDATE_ID, 42);
    let _ = ext.push_u64(tlv::GREASE, 0xDEAD_BEEF);
    let bytes = ext.encode().unwrap();
    let parsed = HeaderExt::parse(&bytes).unwrap();
    // The greasing TLV survives the round trip.
    assert!(parsed.get(tlv::GREASE).is_some());
    println!("greasing TLV round-tripped, total bytes={}", bytes.len());
}
