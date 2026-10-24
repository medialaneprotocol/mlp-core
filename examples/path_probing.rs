//! Path probing: pick the best path among candidates.
use mlp_core::media::probe::{PathProber, ProbeResult};
use std::time::Instant;

fn main() {
    let mut prober = PathProber::new(1000, 3);
    let now = Instant::now();
    prober.record(ProbeResult {
        path_id: 0,
        rtt_ms: 100.0,
        loss_ratio: 0.05,
        goodput_bps: 1_000_000,
        completed_at: now,
    });
    prober.record(ProbeResult {
        path_id: 1,
        rtt_ms: 50.0,
        loss_ratio: 0.01,
        goodput_bps: 2_000_000,
        completed_at: now,
    });
    println!("best path: {:?}", prober.best_path());
}
