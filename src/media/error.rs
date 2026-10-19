//! Error taxonomy for the media lane (MLP v1).
//!
//! The numeric codes are the wire values of `MediaError.code` (MLP spec §6.3);
//! [`RejectReason`] is the richer receiver-side vocabulary, including drops
//! that are never reported back to the peer (expiry, duplicates, coalescing).

use serde::{Deserialize, Serialize};

/// Numeric `MediaError.code` values (MLP spec §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum MediaErrorCode {
    /// The transport message did not start with `MLP` (MLP spec §5.2).
    BadMagic = 1,
    /// `version` was not `2`.
    UnsupportedVersion = 2,
    /// `class` was outside `0..=4`.
    BadClass = 3,
    /// A reserved flag bit was set.
    BadFlags = 4,
    /// `priority` was outside `0..=7`, or media used `0`/`1`.
    BadPriority = 5,
    /// `fragment_index >= fragment_count`, or `fragment_count` outside `1..=4096`.
    BadFragment = 6,
    /// `payload_len` exceeded the carrier cap, or the message was truncated.
    BadLength = 7,
    /// The header extension did not parse as whole TLVs.
    BadExt = 8,
    /// Contradictory duplicate metadata (for example two different `UPDATE_ID`s).
    MetaMismatch = 9,
    /// An incomplete object exceeded `ASSEMBLY_TIMEOUT`.
    AssemblyTimeout = 10,
    /// An incomplete object was evicted by the open-object or byte budget.
    AssemblyEvicted = 11,
    /// The sender's `object_id` counter wrapped.
    ObjectIdWrap = 12,
    /// A class body failed its own validation (including ack bitmap rules).
    BodyInvalid = 13,
    /// A hop rate limit discarded the object.
    RateLimited = 14,
    /// The lane is disabled (kill switch, unsupported peer, sender offline).
    LaneDisabled = 15,
}

impl MediaErrorCode {
    /// The wire value of this code.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a wire value; unknown values are `None` (forward compatible).
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::BadMagic),
            2 => Some(Self::UnsupportedVersion),
            3 => Some(Self::BadClass),
            4 => Some(Self::BadFlags),
            5 => Some(Self::BadPriority),
            6 => Some(Self::BadFragment),
            7 => Some(Self::BadLength),
            8 => Some(Self::BadExt),
            9 => Some(Self::MetaMismatch),
            10 => Some(Self::AssemblyTimeout),
            11 => Some(Self::AssemblyEvicted),
            12 => Some(Self::ObjectIdWrap),
            13 => Some(Self::BodyInvalid),
            14 => Some(Self::RateLimited),
            15 => Some(Self::LaneDisabled),
            _ => None,
        }
    }

    /// The stable metric label spelling of this code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadMagic => "bad_magic",
            Self::UnsupportedVersion => "unsupported_version",
            Self::BadClass => "bad_class",
            Self::BadFlags => "bad_flags",
            Self::BadPriority => "bad_priority",
            Self::BadFragment => "bad_fragment",
            Self::BadLength => "bad_length",
            Self::BadExt => "bad_ext",
            Self::MetaMismatch => "meta_mismatch",
            Self::AssemblyTimeout => "assembly_timeout",
            Self::AssemblyEvicted => "assembly_evicted",
            Self::ObjectIdWrap => "object_id_wrap",
            Self::BodyInvalid => "body_invalid",
            Self::RateLimited => "rate_limited",
            Self::LaneDisabled => "lane_disabled",
        }
    }
}

/// Why a fragment or object was rejected or dropped.
///
/// The first fifteen variants mirror [`MediaErrorCode`] one-to-one and carry a
/// wire code. The trailing variants are local drops that the sender already
/// accounted for or that must not be reported back (a duplicate is not an
/// error, and a drop past the assembly deadline is a consequence of loss).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum RejectReason {
    /// `magic != "MLP"`.
    BadMagic,
    /// `version != 2`.
    UnsupportedVersion,
    /// `class` outside `0..=4`.
    BadClass,
    /// A reserved flag bit (bit 7) was set.
    BadFlags,
    /// `priority` outside `0..=7`, or a media fragment using `0`/`1`.
    BadPriority,
    /// `fragment_index >= fragment_count`, or `fragment_count` outside `1..=4096`.
    BadFragment,
    /// `payload_len` above the carrier cap, or a truncated message.
    BadLength,
    /// The header extension is not a sequence of whole TLVs, or exceeds 256 B.
    BadExt,
    /// Contradictory metadata between the fragment header and the body.
    MetaMismatch,
    /// The open object exceeded the RTT-relative assembly timeout.
    AssemblyTimeout,
    /// The oldest incomplete object was evicted to honour the open-object budget.
    AssemblyEvicted,
    /// The sender's `object_id` counter would wrap.
    ObjectIdWrap,
    /// The class body failed validation (`MediaAck` bitmap or count rules).
    BodyInvalid,
    /// A hop rate limit discarded the object.
    RateLimited,
    /// The lane is disabled.
    LaneDisabled,
    /// A datagram carrier received a fragment with `fragment_count > 1`.
    MultiFragmentOnDatagram,
    /// `DEADLINE_EPOCH_MS` (or the sender's `SEND_EPOCH_MS` deadline) has passed.
    Expired,
    /// The `(object_id, fragment_index)` pair was already seen in the window.
    Duplicate,
    /// A fragment disagreed with the open object (fragment count, class).
    FragmentMismatch,
    /// The ack bitmap had bits set above `highest_applied_update_id`.
    AckBadBitmap,
    /// `stale_blocks > applied_block_count`.
    AckBadCount,
    /// The object was dropped because a newer plan covers its cells.
    SupersededByPlan,
    /// The object was dropped by the bounded in-flight cap.
    InFlightCap,
}

impl RejectReason {
    /// The `MediaError` code the lane may report for this reason, if any.
    ///
    /// Variants without a code are local drops: reporting them back would be
    /// noise (the peer's accounting already knows) or impossible (the drop
    /// happened after the deadline).
    pub const fn code(self) -> Option<MediaErrorCode> {
        match self {
            Self::BadMagic => Some(MediaErrorCode::BadMagic),
            Self::UnsupportedVersion => Some(MediaErrorCode::UnsupportedVersion),
            Self::BadClass => Some(MediaErrorCode::BadClass),
            Self::BadFlags => Some(MediaErrorCode::BadFlags),
            Self::BadPriority => Some(MediaErrorCode::BadPriority),
            Self::BadFragment => Some(MediaErrorCode::BadFragment),
            Self::BadLength => Some(MediaErrorCode::BadLength),
            Self::BadExt => Some(MediaErrorCode::BadExt),
            Self::MetaMismatch => Some(MediaErrorCode::MetaMismatch),
            Self::AssemblyTimeout => Some(MediaErrorCode::AssemblyTimeout),
            Self::AssemblyEvicted => Some(MediaErrorCode::AssemblyEvicted),
            Self::ObjectIdWrap => Some(MediaErrorCode::ObjectIdWrap),
            Self::BodyInvalid | Self::AckBadBitmap | Self::AckBadCount => {
                Some(MediaErrorCode::BodyInvalid)
            }
            Self::RateLimited => Some(MediaErrorCode::RateLimited),
            Self::LaneDisabled => Some(MediaErrorCode::LaneDisabled),
            Self::MultiFragmentOnDatagram | Self::FragmentMismatch => {
                Some(MediaErrorCode::BadFragment)
            }
            Self::Expired | Self::Duplicate | Self::SupersededByPlan | Self::InFlightCap => None,
        }
    }

    /// The stable metric label spelling of this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BadMagic => "bad_magic",
            Self::UnsupportedVersion => "unsupported_version",
            Self::BadClass => "bad_class",
            Self::BadFlags => "bad_flags",
            Self::BadPriority => "bad_priority",
            Self::BadFragment => "bad_fragment",
            Self::BadLength => "bad_length",
            Self::BadExt => "bad_ext",
            Self::MetaMismatch => "meta_mismatch",
            Self::AssemblyTimeout => "assembly_timeout",
            Self::AssemblyEvicted => "assembly_evicted",
            Self::ObjectIdWrap => "object_id_wrap",
            Self::BodyInvalid => "body_invalid",
            Self::RateLimited => "rate_limited",
            Self::LaneDisabled => "lane_disabled",
            Self::MultiFragmentOnDatagram => "multi_fragment_on_datagram",
            Self::Expired => "expired",
            Self::Duplicate => "duplicate",
            Self::FragmentMismatch => "fragment_mismatch",
            Self::AckBadBitmap => "ack_bad_bitmap",
            Self::AckBadCount => "ack_bad_count",
            Self::SupersededByPlan => "superseded_by_plan",
            Self::InFlightCap => "in_flight_cap",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for code in 1..=15u32 {
            let parsed = MediaErrorCode::from_u32(code).expect("code in range");
            assert_eq!(parsed.as_u32(), code);
        }
        assert_eq!(MediaErrorCode::from_u32(0), None);
        assert_eq!(MediaErrorCode::from_u32(16), None);
    }

    #[test]
    fn ack_failures_map_to_body_invalid() {
        assert_eq!(
            RejectReason::AckBadBitmap.code(),
            Some(MediaErrorCode::BodyInvalid)
        );
        assert_eq!(
            RejectReason::AckBadCount.code(),
            Some(MediaErrorCode::BodyInvalid)
        );
    }

    #[test]
    fn local_drops_have_no_wire_code() {
        for reason in [
            RejectReason::Expired,
            RejectReason::Duplicate,
            RejectReason::SupersededByPlan,
            RejectReason::InFlightCap,
        ] {
            assert_eq!(reason.code(), None, "{reason:?} must not be reportable");
        }
    }
}
