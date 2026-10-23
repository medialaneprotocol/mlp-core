//! BWE example: feed goodput and observe estimate.

use mlp_core::media::bwe::BandwidthEstimator;
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let mut est = BandwidthEstimator::new(mlp_core::media::bwe::BweConfig::default());
    println!("initial pace: {} bps", est.pace_bps());
    est.on_evidence(now);
    println!("after evidence: {} bps", est.pace_bps());
}
