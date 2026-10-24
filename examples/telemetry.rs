//! Telemetry: read the sender's stats.
use mlp_core::media::cells::CellGrid;
use mlp_core::media::pdu::Carrier;
use mlp_core::media::sender::MediaSenderMachine;

fn main() {
    let grid = CellGrid::with_cell_size(320, 240, 64);
    let epoch = std::time::Instant::now();
    let sender = MediaSenderMachine::new(
        mlp_core::media::sender::MediaSenderConfig::new(Carrier::Ws, grid),
        grid,
        epoch,
    );
    let stats = sender.stats();
    println!("updates_enqueued={}", stats.updates_enqueued);
    println!("bytes_sent={}", stats.bytes_sent);
}
