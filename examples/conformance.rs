//! Read and parse the conformance golden vectors.
use std::fs;

fn main() {
    let text = fs::read_to_string("conformance/vectors.json").expect("vectors");
    println!("{}", text);
    // Each vector's `hex` field is a parseable fragment.
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    for vec in v["vectors"].as_array().unwrap() {
        let name = vec["name"].as_str().unwrap();
        let hex = vec["hex"].as_str().unwrap();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        match mlp_core::media::pdu::Fragment::parse(&bytes, mlp_core::media::pdu::Carrier::Ws) {
            Ok((frag, consumed)) => println!("{}: parsed {} bytes, object_id={}", name, consumed, frag.header.object_id),
            Err(e) => println!("{}: parse error {:?}", name, e),
        }
    }
}
