//! Pacer: token bucket + 1-second window.
use mlp_core::media::pacer::MediaPacer;
use mlp_core::media::pacer::PacerConfig;
use mlp_core::media::pdu::Carrier;
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let cfg = PacerConfig::for_carrier(Carrier::Ws, 8_000, 8_192);
    let mut pacer = MediaPacer::with_config(cfg, now);
    println!("budget={}", pacer.window_budget_bytes());
    println!("max_grant={}", pacer.max_grant_bytes());
    println!("pace_bps={}", pacer.pace_bps());
}
