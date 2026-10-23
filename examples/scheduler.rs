//! Scheduler example: schedule a delta update.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine, UpdateRequest};
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let mut sender = MediaSenderMachine::new(MediaSenderConfig::new(Carrier::Ws, grid), grid, now);
    let _ = sender.enqueue_update(UpdateRequest::delta(vec![0u8; 64], vec![0, 1]), now);
    println!("queued: {}", sender.queued_objects());
    println!("in_flight: {}", sender.in_flight());
}
