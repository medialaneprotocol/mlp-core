//! Ack example: build and inspect an acknowledgement.

use mlp_core::media::receiver::{MediaReceiverConfig, MediaReceiverMachine};
use mlp_core::media::pdu::Carrier;
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let mut rx = MediaReceiverMachine::new(MediaReceiverConfig::for_carrier(Carrier::Ws), now);
    let ack = rx.build_ack(now);
    println!("ack: highest_applied={}", ack.highest_applied_update_id);
}
