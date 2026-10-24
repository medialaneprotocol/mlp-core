//! Lifecycle example: lane policy and state.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::lifecycle::MediaLanePolicy;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine};
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let mut sender = MediaSenderMachine::new(
        MediaSenderConfig::new(Carrier::Ws, grid).with_fast_path(),
        grid,
        now,
    );
    sender.set_policy(MediaLanePolicy::default().with_media_lane_enabled(true));
    println!("policy emits media: {}", sender.policy().emits_media());
    println!("lane state: {:?}", sender.lane().state());
}
