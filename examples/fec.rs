//! FEC: XOR parity over a group of fragments.
use mlp_core::media::fec::{recover_one, xor_parity};

fn main() {
    let data = vec![vec![1u8, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
    let parity = xor_parity(&data);
    // Lose fragment 1, recover it.
    let mut received: Vec<Option<Vec<u8>>> = data.iter().map(|d| Some(d.clone())).collect();
    received[1] = None;
    let recovered = recover_one(1, &received, &parity).unwrap();
    assert_eq!(recovered[..3], data[1]);
    println!("recovered fragment 1: {:?}", &recovered[..3]);
}
