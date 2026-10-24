//! Forward error correction (MLP spec §6.9): XOR parity over groups of
//! fragments, so a single lost fragment per group can be recovered.
//!
//! A group of `k` data fragments produces one parity fragment whose payload
//! is the byte-wise XOR of the (zero-padded) data payloads. The receiver
//! recovers a single missing data fragment by XOR-ing the parity with the
//! other `k-1` data fragments.

use crate::media::pdu::Fragment;

/// XOR `k` data payloads into a parity payload of length `max_len`.
pub fn xor_parity(data: &[Vec<u8>]) -> Vec<u8> {
    let max_len = data.iter().map(|d| d.len()).max().unwrap_or(0);
    let mut parity = vec![0u8; max_len];
    for d in data {
        for (i, b) in d.iter().enumerate() {
            parity[i] ^= b;
        }
    }
    parity
}

/// Recover a missing fragment given the parity and the other data fragments.
/// Returns the recovered payload, or `None` if more than one is missing.
pub fn recover_one(missing_index: usize, data: &[Option<Vec<u8>>], parity: &[u8]) -> Option<Vec<u8>> {
    let missing_count = data.iter().filter(|d| d.is_none()).count();
    if missing_count != 1 {
        return None;
    }
    let max_len = parity.len();
    let mut out = parity.to_vec();
    for (i, d) in data.iter().enumerate() {
        if i == missing_index {
            continue;
        }
        if let Some(d) = d {
            for (j, b) in d.iter().enumerate() {
                if j < out.len() {
                    out[j] ^= b;
                }
            }
        }
    }
    // trim trailing zeros? Keep full length for safety.
    let _ = max_len;
    Some(out)
}

/// Build a parity Fragment from a group of data fragments.
pub fn build_parity_fragment(group_id: u32, k: u16, data: &[Vec<u8>], object_id: u32) -> Fragment {
    let parity = xor_parity(data);
    let mut ext = crate::media::pdu::HeaderExt::new();
    let _ = ext.push_u32(crate::media::pdu::tlv::FEC_GROUP_ID, group_id);
    let _ = ext.push_u16(crate::media::pdu::tlv::FEC_K, k);
    Fragment::new(
        crate::media::pdu::FragmentClass::Fec,
        crate::media::pdu::FragmentFlags::NONE,
        crate::media::pdu::Priority::MEDIA,
        object_id,
        0,
        1,
        ext,
        parity,
    )
    .unwrap()
}

// HeaderExt helpers
impl crate::media::pdu::HeaderExt {
    // already has push_u32 etc.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_roundtrip() {
        let data = vec![vec![1u8, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
        let parity = xor_parity(&data);
        // lose fragment 1
        let mut received: Vec<Option<Vec<u8>>> = data.iter().map(|d| Some(d.clone())).collect();
        received[1] = None;
        let recovered = recover_one(1, &received, &parity).unwrap();
        assert_eq!(recovered[..3], data[1]);
    }

    #[test]
    fn recover_fails_on_two_losses() {
        let data = vec![vec![1u8, 2], vec![3, 4]];
        let parity = xor_parity(&data);
        let received = vec![None, None];
        assert!(recover_one(0, &received, &parity).is_none());
    }
}
