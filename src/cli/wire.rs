//! The byte-level helpers the tool needs: hex, SHA-256, and the protocol's
//! class-body encoding.
//!
//! The class bodies of MLP spec §7.2/§7.3 travel as protobuf, and the
//! acknowledgement's field numbers are the schema's normative table
//! ([`mlp-proto`](https://github.com/medialaneprotocol/mlp-proto)). The tool
//! encodes and decodes that schema itself rather than taking a dependency on a
//! protobuf runtime: the message it carries has ten scalar fields, and the
//! encoding it must produce is the schema's canonical one — a field equal to
//! its default is omitted, every other field is emitted.

use crate::cli::CliError;
use crate::media::MediaAck;

/// Parse a hexadecimal string into bytes.
///
/// Whitespace and `:` separators are ignored, so a byte string may be written
/// the way a packet capture writes it.
pub fn parse_hex(text: &str) -> Result<Vec<u8>, CliError> {
    let mut nibbles: Vec<u8> = Vec::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii_whitespace() || character == ':' {
            continue;
        }
        match character.to_digit(16) {
            Some(value) => nibbles.push(value as u8),
            None => {
                return Err(CliError::Input(format!(
                    "the hex text holds {character:?}, which is not a hexadecimal digit"
                )));
            }
        }
    }
    if nibbles.len() % 2 != 0 {
        return Err(CliError::Input(
            "the hex text holds an odd number of digits".to_string(),
        ));
    }
    Ok(nibbles
        .chunks_exact(2)
        .map(|pair| (pair[0] << 4) | pair[1])
        .collect())
}

/// The SHA-256 of `bytes`, as 32 bytes.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = bytes.to_vec();
    let bit_length = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for block in message.chunks_exact(64) {
        let mut schedule = [0_u32; 64];
        for (index, word) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0_u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// The SHA-256 of `bytes`, as 64 lowercase hexadecimal digits.
pub fn sha256_hex(bytes: &[u8]) -> String {
    crate::cli::hex(&sha256(bytes))
}

/// A protobuf writer: the schema's canonical encoding.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// The bytes written so far.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn tag(&mut self, field: u32, wire_type: u32) {
        self.varint(((field << 3) | wire_type) as u64);
    }

    fn varint(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.bytes.push(byte);
                return;
            }
            self.bytes.push(byte | 0x80);
        }
    }

    /// Write a `uint64` field, omitting it when it is the default.
    pub fn uint64(&mut self, field: u32, value: u64) {
        if value == 0 {
            return;
        }
        self.tag(field, 0);
        self.varint(value);
    }

    /// Write a `uint32` field, omitting it when it is the default.
    pub fn uint32(&mut self, field: u32, value: u32) {
        if value == 0 {
            return;
        }
        self.tag(field, 0);
        self.varint(u64::from(value));
    }

    /// Write a `bool` field, omitting it when it is the default.
    pub fn bool(&mut self, field: u32, value: bool) {
        if !value {
            return;
        }
        self.tag(field, 0);
        self.varint(1);
    }

    /// Write a `double` field, omitting it when it is the default.
    pub fn double(&mut self, field: u32, value: f64) {
        if value == 0.0 {
            return;
        }
        self.tag(field, 1);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }
}

/// A protobuf reader over one message.
#[derive(Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    /// Whether every byte was consumed.
    pub fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn varint(&mut self) -> Result<u64, CliError> {
        let mut value = 0_u64;
        let mut shift = 0_u32;
        loop {
            let byte = *self
                .bytes
                .get(self.offset)
                .ok_or_else(|| CliError::Input("the message ends inside a varint".to_string()))?;
            self.offset += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 64 {
                return Err(CliError::Input(
                    "the message holds an overlong varint".to_string(),
                ));
            }
        }
    }

    fn fixed64(&mut self) -> Result<u64, CliError> {
        let end = self.offset + 8;
        if end > self.bytes.len() {
            return Err(CliError::Input(
                "the message ends inside a 64-bit field".to_string(),
            ));
        }
        let mut word = [0_u8; 8];
        word.copy_from_slice(&self.bytes[self.offset..end]);
        self.offset = end;
        Ok(u64::from_le_bytes(word))
    }

    fn length_delimited(&mut self) -> Result<&'a [u8], CliError> {
        let length = self.varint()? as usize;
        let end = self.offset + length;
        if end > self.bytes.len() {
            return Err(CliError::Input(
                "the message ends inside a length-delimited field".to_string(),
            ));
        }
        let slice = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(slice)
    }

    fn skip(&mut self, wire_type: u32) -> Result<(), CliError> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed64()?;
            }
            2 => {
                self.length_delimited()?;
            }
            5 => {
                let end = self.offset + 4;
                if end > self.bytes.len() {
                    return Err(CliError::Input(
                        "the message ends inside a 32-bit field".to_string(),
                    ));
                }
                self.offset = end;
            }
            other => {
                return Err(CliError::Input(format!(
                    "the message holds the wire type {other}, which is not a protobuf wire type"
                )));
            }
        }
        Ok(())
    }
}

/// Encode an acknowledgement as the schema's `MediaAck`.
pub fn encode_ack(ack: &MediaAck) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.uint64(1, ack.highest_applied_update_id);
    writer.uint64(2, ack.ack_bitmap);
    writer.uint32(3, ack.chain_epoch);
    writer.uint32(4, ack.applied_cell_count);
    writer.uint32(5, ack.stale_cells);
    writer.bool(6, ack.converged);
    writer.double(7, ack.render_latency_ms);
    writer.bool(8, ack.has_render_latency_ms);
    writer.uint64(9, ack.echo_of_input_seq);
    writer.uint64(10, ack.ack_epoch_ms);
    writer.into_bytes()
}

/// Decode an acknowledgement from the schema's `MediaAck`.
///
/// A field the schema does not name is skipped, so a message that carries
/// more than this tool knows still decodes.
pub fn decode_ack(bytes: &[u8]) -> Result<MediaAck, CliError> {
    let mut reader = Reader::new(bytes);
    let mut ack = MediaAck::default();
    let mut render_latency_ms = 0.0_f64;
    while !reader.is_empty() {
        let tag = reader.varint()?;
        let field = (tag >> 3) as u32;
        let wire_type = (tag & 7) as u32;
        match (field, wire_type) {
            (1, 0) => ack.highest_applied_update_id = reader.varint()?,
            (2, 0) => ack.ack_bitmap = reader.varint()?,
            (3, 0) => ack.chain_epoch = reader.varint()? as u32,
            (4, 0) => ack.applied_cell_count = reader.varint()? as u32,
            (5, 0) => ack.stale_cells = reader.varint()? as u32,
            (6, 0) => ack.converged = reader.varint()? != 0,
            (7, 1) => {
                let word = reader.fixed64()?;
                render_latency_ms = f64::from_bits(word);
            }
            (8, 0) => ack.has_render_latency_ms = reader.varint()? != 0,
            (9, 0) => ack.echo_of_input_seq = reader.varint()?,
            (10, 0) => ack.ack_epoch_ms = reader.varint()?,
            _ => reader.skip(wire_type)?,
        }
    }
    ack.render_latency_ms = render_latency_ms;
    Ok(ack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_published_test_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hex_text_is_read_with_separators_and_whitespace() {
        assert_eq!(
            parse_hex("4d 4c:50 00").expect("hex"),
            vec![0x4d, 0x4c, 0x50, 0x00]
        );
    }

    #[test]
    fn an_odd_hex_text_is_refused() {
        let error = parse_hex("abc").expect_err("odd");
        assert_eq!(error.exit_code(), crate::cli::EXIT_INPUT);
    }

    #[test]
    fn a_non_hex_digit_is_refused() {
        let error = parse_hex("zz").expect_err("not hex");
        assert!(error.message().contains("not a hexadecimal digit"));
    }

    #[test]
    fn an_acknowledgement_round_trips() {
        let ack = MediaAck {
            highest_applied_update_id: 10,
            ack_bitmap: 0b1010_0101,
            chain_epoch: 3,
            applied_cell_count: 510,
            stale_cells: 16,
            converged: true,
            render_latency_ms: 12.5,
            has_render_latency_ms: true,
            echo_of_input_seq: 7,
            ack_epoch_ms: 900,
        };
        let bytes = encode_ack(&ack);
        assert_eq!(decode_ack(&bytes).expect("decodes"), ack);
    }

    #[test]
    fn a_default_acknowledgement_is_an_empty_message() {
        assert!(encode_ack(&MediaAck::default()).is_empty());
    }

    #[test]
    fn an_unknown_field_is_skipped() {
        let mut writer = Writer::new();
        writer.uint64(1, 4);
        writer.uint32(99, 7);
        writer.uint64(2, 1);
        let ack = decode_ack(&writer.into_bytes()).expect("decodes");
        assert_eq!(ack.highest_applied_update_id, 4);
        assert_eq!(ack.ack_bitmap, 1);
    }

    #[test]
    fn a_truncated_message_is_refused() {
        let mut writer = Writer::new();
        writer.uint64(1, 4);
        let mut bytes = writer.into_bytes();
        bytes.push(0x08);
        let error = decode_ack(&bytes).expect_err("truncated");
        assert_eq!(error.exit_code(), crate::cli::EXIT_INPUT);
    }
}
