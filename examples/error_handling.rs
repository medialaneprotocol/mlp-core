//! Error handling: parse malformed fragments.
use mlp_core::media::pdu::{Carrier, Fragment};

fn main() {
    let bytes = [0u8; 5];
    match Fragment::parse(&bytes, Carrier::Ws) {
        Ok(_) => println!("unexpectedly parsed"),
        Err(e) => println!("truncated -> {:?}", e),
    }
    let bytes = [0u8; 22];
    match Fragment::parse(&bytes, Carrier::Ws) {
        Ok(_) => println!("unexpectedly parsed"),
        Err(e) => println!("bad magic -> {:?}", e),
    }
}
