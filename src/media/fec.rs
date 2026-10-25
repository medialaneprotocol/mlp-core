//! Forward error correction on the wire (MLP spec §6.9): one XOR parity
//! object per multi-part update, so a single lost part costs nothing — no
//! round trip, no repair plan, no re-encoded cells.
//!
//! The parity object carries the group's parts XOR'd in a self-describing
//! block form, plus a CRC of the group, and it travels under the update's
//! own priority immediately after the parts. A receiver that does not know
//! the parity TLVs skips them and waits for repair; a receiver that does
//! recovers the one missing part on the spot and proves it against the
//! group's CRC — a parity that lies (or arrived garbled) fails the check
//! and the normal repair ladder answers, so wrong pixels never leave the
//! recovery path.
//!
//! On a single-part update the parity would be a byte copy of the part:
//! that case is the existing deliberate-duplicate rule (§6.9), not this
//! module's.

/// The block form every part is XOR'd in: `u16 LE length ‖ payload`,
/// zero-padded to the group's longest block. Two bytes buy back the
/// missing part's exact length — a codec's trailing bytes are data.
///
/// The parity payload is `XOR of the blocks ‖ u32 LE CRC-32 of the
/// concatenated blocks`, so recovery is verified, not trusted.
use super::pdu::{Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, UpdateMeta, tlv};

/// The largest group a parity object may name (MLP spec §6.9): a group is
/// one update's parts, and the wire caps parts at eight.
pub const FEC_MAX_K: usize = 8;

/// The measured loss band where the sender attaches parity to every
/// multi-part update (MLP spec §6.9). Below `FEC_LOSS_RATIO` a round of
/// repair is cheaper than the parity bytes; above `FEC_LOSS_RATIO_MAX` two
/// losses in one group are common enough that XOR parity answers nothing
/// while doubling the bytes a deep-loss lane cannot afford — there the
/// repair ladder and its anchors are the honest tool. The band is where
/// one lost part per group is the common case and is recovered for free.
pub const FEC_LOSS_RATIO: f64 = 0.02;
pub const FEC_LOSS_RATIO_MAX: f64 = 0.15;

/// CRC-32 (IEEE 802.3), table-free: the polynomial walks a byte at a time.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The block form of one part: `u16 LE length ‖ payload`.
fn block(part: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + part.len());
    out.extend_from_slice(&(part.len() as u16).to_le_bytes());
    out.extend_from_slice(part);
    out
}

/// Build the parity payload for a group of parts.
pub fn group_parity(parts: &[Vec<u8>]) -> Vec<u8> {
    let blocks: Vec<Vec<u8>> = parts.iter().map(|p| block(p)).collect();
    let width = blocks.iter().map(Vec::len).max().unwrap_or(2);
    let mut parity = vec![0u8; width];
    for b in &blocks {
        for (i, byte) in b.iter().enumerate() {
            parity[i] ^= byte;
        }
    }
    let mut joined = Vec::new();
    for b in &blocks {
        joined.extend_from_slice(b);
    }
    parity.extend_from_slice(&crc32(&joined).to_le_bytes());
    parity
}

/// The recovery: `k` parts of which exactly `k - 1` are held, and the
/// group's parity. Returns the missing part's `(part_index, payload)`, or
/// `None` when the parity does not check out — the caller falls through to
/// the repair ladder, never to wrong bytes.
pub fn recover_part(parity: &[u8], held: &[(u16, Vec<u8>)], k: usize) -> Option<(u16, Vec<u8>)> {
    if !(2..=FEC_MAX_K).contains(&k) || held.len() != k - 1 || parity.len() < 6 {
        return None;
    }
    // Which index is missing.
    let held_indices: Vec<u16> = held.iter().map(|(index, _)| *index).collect();
    let mut missing = None;
    for candidate in 0..k {
        let candidate = candidate as u16;
        if !held_indices.contains(&candidate) {
            missing = Some(candidate);
            break;
        }
    }
    let missing = missing?;
    if held_indices.contains(&missing) || held.iter().any(|(index, _)| *index >= k as u16) {
        return None;
    }

    // XOR the held blocks onto the parity.
    let width = parity.len() - 4;
    let mut acc = parity[..width].to_vec();
    for (_, part) in held {
        let b = block(part);
        if b.len() > width {
            return None;
        }
        for (i, byte) in b.iter().enumerate() {
            acc[i] ^= byte;
        }
    }
    if acc.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([acc[0], acc[1]]) as usize;
    if 2 + len > acc.len() {
        return None;
    }
    let payload = acc[2..2 + len].to_vec();

    // The proof: the group's CRC must hold over all k blocks, the
    // recovered one included, in index order.
    let mut all: Vec<(u16, Vec<u8>)> = held.to_vec();
    all.push((missing, payload.clone()));
    all.sort_by_key(|(index, _)| *index);
    let mut joined = Vec::new();
    for (_, part) in &all {
        joined.extend_from_slice(&block(part));
    }
    let stored = u32::from_le_bytes([
        parity[parity.len() - 4],
        parity[parity.len() - 3],
        parity[parity.len() - 2],
        parity[parity.len() - 1],
    ]);
    if crc32(&joined) != stored {
        return None;
    }
    Some((missing, payload))
}

/// The parity object's extension: the protected update's own metadata,
/// minus the part slot and minus the update id (the parity object is
/// invisible to the update-id space — `FEC_GROUP_ID` names the group).
pub fn parity_ext(meta: &UpdateMeta, k: u16, cell_count: u32) -> HeaderExt {
    let mut ext = HeaderExt::new();
    let _ = ext.push_u64(tlv::UPDATE_BASE, meta.update_base);
    let _ = ext.push_u32(tlv::UPDATE_BYTES, meta.update_bytes);
    let _ = ext.push_u32(tlv::CHAIN_EPOCH, meta.chain_epoch);
    let _ = ext.push_u64(tlv::REFLECTS_INPUT_SEQ, meta.reflects_input_seq);
    let _ = ext.push_u64(tlv::SEND_EPOCH_MS, meta.send_epoch_ms);
    let _ = ext.push_u32(tlv::CELL_COUNT, cell_count);
    let _ = ext.push_u64(tlv::FEC_GROUP_ID, meta.update_id);
    let _ = ext.push_u16(tlv::FEC_K, k);
    ext
}

/// Whether an object's extension names a parity group.
pub fn parity_group(ext: &HeaderExt) -> Option<(u64, u16)> {
    let group = ext.get_u64(tlv::FEC_GROUP_ID)?;
    let k = ext.get_u16(tlv::FEC_K)?;
    if k < 2 || k as usize > FEC_MAX_K {
        return None;
    }
    Some((group, k))
}

/// Build the parity fragment's wire object (one fragment, always FIN).
pub fn build_parity_fragment(
    meta: &UpdateMeta,
    k: u16,
    parity: Vec<u8>,
    object_id: u32,
    priority: Priority,
    cell_count: u32,
) -> Option<Fragment> {
    let ext = parity_ext(meta, k, cell_count);
    Fragment::new(
        FragmentClass::Media,
        FragmentFlags::FIN,
        priority,
        object_id,
        0,
        1,
        ext,
        parity,
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parity_recovers_the_one_missing_part() {
        let parts: Vec<Vec<u8>> = [vec![1, 2, 3], vec![4, 5], vec![6, 7, 8, 9]]
            .into_iter()
            .map(Vec::from)
            .collect();
        let parity = group_parity(&parts);
        // Lose part 1.
        let held: Vec<(u16, Vec<u8>)> = vec![(0, parts[0].clone()), (2, parts[2].clone())];
        let (index, payload) = recover_part(&parity, &held, 3).expect("recovered");
        assert_eq!(index, 1);
        assert_eq!(payload, parts[1]);
        // Lose the longest part instead: lengths must come back exact.
        let held: Vec<(u16, Vec<u8>)> = vec![(0, parts[0].clone()), (1, parts[1].clone())];
        let (index, payload) = recover_part(&parity, &held, 3).expect("recovered");
        assert_eq!(index, 2);
        assert_eq!(payload, parts[2]);
    }

    #[test]
    fn a_parity_that_lies_fails_the_proof() {
        let parts: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![4, 5], vec![6, 7, 8, 9]];
        let mut parity = group_parity(&parts);
        // Corrupt one byte of the XOR body.
        parity[0] ^= 0x40;
        let held: Vec<(u16, Vec<u8>)> = vec![(0, parts[0].clone()), (2, parts[2].clone())];
        assert!(recover_part(&parity, &held, 3).is_none());
        // Corrupt the stored CRC instead.
        let mut parity = group_parity(&parts);
        let last = parity.len() - 1;
        parity[last] ^= 1;
        assert!(recover_part(&parity, &held, 3).is_none());
    }

    #[test]
    fn two_missing_parts_are_not_this_paritys_problem() {
        let parts: Vec<Vec<u8>> = vec![vec![1], vec![2], vec![3]];
        let parity = group_parity(&parts);
        let held: Vec<(u16, Vec<u8>)> = vec![(0, parts[0].clone())];
        assert!(recover_part(&parity, &held, 3).is_none());
    }

    #[test]
    fn an_empty_part_keeps_its_exact_length() {
        let parts: Vec<Vec<u8>> = vec![vec![9, 9, 9], Vec::new(), vec![7]];
        let parity = group_parity(&parts);
        let held: Vec<(u16, Vec<u8>)> = vec![(0, parts[0].clone()), (2, parts[2].clone())];
        let (index, payload) = recover_part(&parity, &held, 3).expect("recovered");
        assert_eq!(index, 1);
        assert!(payload.is_empty());
    }

    #[test]
    fn the_crc_is_the_reference_polynomial() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
