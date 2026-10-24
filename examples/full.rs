//! Full example: sender + receiver + feedback.

use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::receiver::{MediaReceiverConfig, MediaReceiverMachine};
use mlp_core::media::sender::{MediaSenderConfig, MediaSenderMachine, UpdateRequest};
use std::time::Instant;

fn main() {
    let now = Instant::now();
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let mut sender = MediaSenderMachine::new(
        MediaSenderConfig::new(Carrier::Ws, grid)
            .with_fast_path()
            .with_in_flight_cap(4),
        grid,
        now,
    );
    let mut receiver =
        MediaReceiverMachine::new(MediaReceiverConfig::for_carrier(Carrier::Ws), now);

    let _ = sender.enqueue_update(UpdateRequest::delta(vec![0u8; 64], vec![0, 1]), now);
    while let Some(f) = sender.poll_fragment(now) {
        let evs = receiver.on_message(&f.bytes, now);
        for ev in evs {
            if let mlp_core::media::receiver::MediaReceiveEvent::Object(_) = ev {
                receiver.note_applied(1);
            }
        }
    }
    let ack = receiver.build_ack(now);
    let _ = sender.on_ack(ack, now);
    let fb = receiver.build_feedback(now);
    let _ = sender.on_feedback(fb, now);
    println!("stats: fragments_sent={}", sender.stats().fragments_sent);
}
