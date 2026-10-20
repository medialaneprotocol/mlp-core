//! A measurement harness: the tier envelope, reported.
use mlp_core::testing::lossy_link::Tier;
use mlp_core::testing::scenario::{cold_start_scenario, run};

fn main() {
    let tier = Tier::T4;
    let cold = run(&cold_start_scenario(tier));
    for u in cold.updates.iter().take(24) {
        println!(
            "     update t={} id={} kind={} cells={} bytes={}",
            u.at_ms, u.update_id, u.kind, u.cells, u.bytes
        );
    }
    for a in cold.acks.iter().take(24) {
        println!(
            "     ack t={} highest={} pending={} converged={}",
            a.at_ms, a.highest_applied_update_id, a.pending_cells, a.converged
        );
    }
    println!(
        "   cold: updates {} pending {} converged {}",
        cold.counters.updates_sent,
        cold.counters.pending_cells,
        cold.counters.converged_observations
    );
}
