//! Conformance: every golden vector in conformance/vectors.json parses.
use serde::Deserialize;
use std::fs;

#[derive(Deserialize)]
struct Vectors {
    vectors: Vec<Vector>,
}
#[derive(Deserialize)]
struct Vector {
    name: String,
    hex: String,
}

#[test]
fn every_conformance_vector_parses() {
    let text = fs::read_to_string("conformance/vectors.json").expect("vectors file");
    let v: Vectors = serde_json::from_str(&text).expect("parse json");
    assert!(!v.vectors.is_empty());
    for vec in &v.vectors {
        let bytes = hex_decode(&vec.hex);
        let (frag, consumed) = mlp_core::media::pdu::Fragment::parse(&bytes, mlp_core::media::pdu::Carrier::Ws)
            .unwrap_or_else(|e| panic!("{}: parse failed: {:?}", vec.name, e));
        assert_eq!(consumed, bytes.len(), "{}: not fully consumed", vec.name);
        let _ = frag;
    }
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
