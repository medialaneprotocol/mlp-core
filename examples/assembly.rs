//! Object assembly example.

use mlp_core::media::object::{AssemblyConfig, ObjectAssembler};
use std::time::Instant;

fn main() {
    let assembler = ObjectAssembler::new(AssemblyConfig::default());
    println!("assembler created");
    let _ = Instant::now();
    let _ = assembler;
}
