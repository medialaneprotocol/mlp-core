//! Parking example: park a session and resume.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine};
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let mut sender = MediaSenderMachine::new(MediaSenderConfig::new(Carrier::Ws, grid), grid, now);
    println!("lane state: {:?}", sender.lane().state());
    let _ = sender;
}
