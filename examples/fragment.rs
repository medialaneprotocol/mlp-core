//! Fragment example: build and inspect a fragment.

use mlp_core::media::pdu::{Carrier, Fragment, FragmentClass, Priority};
use std::time::Instant;

fn main() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&mlp_core::media::pdu::FRAGMENT_MAGIC);
    bytes.push(mlp_core::media::pdu::MEDIA_PROTOCOL_VERSION);
    bytes.push(FragmentClass::Control as u8);
    bytes.push(0); // flags
    bytes.push(Priority::CONTROL.get());
    bytes.extend_from_slice(&1_u32.to_le_bytes()); // object id
    bytes.extend_from_slice(&0_u16.to_le_bytes()); // fragment index
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // fragment count
    bytes.extend_from_slice(&0_u32.to_le_bytes()); // payload length
    bytes.extend_from_slice(&0_u16.to_le_bytes()); // header ext length

    let (fragment, consumed) = Fragment::parse(&bytes, Carrier::Ws).unwrap();
    println!(
        "parsed fragment: class={:?}, consumed={}",
        fragment.header.class, consumed
    );
    let _ = Instant::now();
}
