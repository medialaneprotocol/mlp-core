//! Pacer example: token bucket.

use mlp_core::media::pacer::{PacerConfig, MediaPacer};
use mlp_core::media::pdu::Carrier;
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let pacer = MediaPacer::with_config(PacerConfig::for_carrier(Carrier::Ws, 8_000, 8_192), now);
    println!("pace_bps: {}", pacer.config().pace_bps);
}
