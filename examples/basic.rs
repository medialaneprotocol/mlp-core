//! Basic usage of mlp-core: enqueue a media update and poll fragments.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine, UpdateRequest};
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let mut sender = MediaSenderMachine::new(
        MediaSenderConfig::new(Carrier::Ws, grid).with_fast_path(),
        grid,
        now,
    );

    let payload = vec![0u8; 1024];
    match sender.enqueue_update(UpdateRequest::delta(payload, vec![0, 1]), now) {
        mlp_core::media::sender::EnqueueOutcome::Accepted { object_id, fragments } => {
            println!("accepted object {} ({} fragments)", object_id, fragments);
        }
        other => println!("not accepted: {:?}", other),
    }

    while let Some(frag) = sender.poll_fragment(now) {
        println!("fragment {} bytes", frag.bytes.len());
    }
}
