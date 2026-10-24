//! Resume example: park and resume a session.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::resume::ResumeRejectReason;
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    // ResumeRejectReason variants
    let r = ResumeRejectReason::UnknownSession;
    println!("resume reject: {:?}", r);
    let _ = (grid, now);
}
