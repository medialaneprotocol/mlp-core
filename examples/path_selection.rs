//! Path selection example: promote/demote carriers.

use mlp_core::media::sender::{PathSelectionConfig, PathSelector};
use mlp_core::media::MediaPath;
use std::time::Instant;

fn main() {
    let mut selector = PathSelector::new(MediaPath::Ws, PathSelectionConfig::default());
    selector.set_rtt_min_ms(150.0);
    // Sustained goodput on direct carrier promotes.
    for step in 1..=8u64 {
        let at = step * 200;
        selector.on_goodput(MediaPath::Ws, 1_000_000, at, false, false);
        if let Some(switch) = selector.on_goodput(MediaPath::DcDirect, 1_500_000, at, false, false) {
            println!("promoted: {:?} -> {:?}", switch.from, switch.to);
            break;
        }
    }
    println!("active: {:?}", selector.active());
}
