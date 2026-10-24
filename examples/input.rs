//! Input queue example: push pointer moves and take ready.

use mlp_core::media::input::{InputQueueConfig, ViewerInputQueue};

fn main() {
    let now_ms = 0u64;
    let mut queue = ViewerInputQueue::new(InputQueueConfig::default(), 1);
    queue.set_connected(true, now_ms);
    let outcome = queue.push_pointer_move(vec![1, 2, 3], now_ms);
    println!("push outcome: {:?}", outcome);
    let ready = queue.take_ready(now_ms + 100);
    println!("ready: {}", ready.len());
}
