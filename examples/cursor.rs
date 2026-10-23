//! Cursor channel example: latest-wins cursor updates.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::MediaCursor;
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine};
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let mut sender = MediaSenderMachine::new(
        MediaSenderConfig::new(Carrier::Ws, grid),
        grid,
        now,
    );

    let cursor = MediaCursor::default();
    match sender.enqueue_cursor(&cursor, vec![1, 2, 3], now) {
        mlp_core::media::sender::EnqueueOutcome::Accepted { object_id, .. } => {
            println!("cursor accepted: object {}", object_id);
        }
        other => println!("cursor not accepted: {:?}", other),
    }
    // second cursor replaces the first (latest-wins)
    let cursor2 = MediaCursor::default();
    let _ = sender.enqueue_cursor(&cursor2, vec![4, 5, 6], now);
    println!("cursors coalesced: {}", sender.stats().cursors_coalesced);
}
