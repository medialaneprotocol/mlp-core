//! MLP v1 fragment framing (MLP spec §6.1–§6.4, §7.1) — byte level.
//!
//! Little-endian throughout; offsets are from the first byte of the fragment:
//!
//! | off | size | field |
//! |---|---|---|
//! | 0 | 4 | `magic` = `52 4D 4C 50` ("MLP") |
//! | 4 | 1 | `version` = 2 |
//! | 5 | 1 | `class` (0=CONTROL, 1=CURSOR, 2=MEDIA, 3=FEEDBACK, 4=PATH) |
//! | 6 | 1 | `flags` (bit 7 reserved, must be 0) |
//! | 7 | 1 | `priority` (0..=7; media may not use 0/1) |
//! | 8 | 4 | `object_id` |
//! | 12 | 2 | `fragment_index` |
//! | 14 | 2 | `fragment_count` (1..=4096) |
//! | 16 | 4 | `payload_len` |
//! | 20 | 2 | `header_ext_len` (0 or 2..=256) |
//! | 22 | `header_ext_len` | header extension TLVs |
//! | 22+ext | `payload_len` | class-specific body |
//!
//! The bytes are frozen by the specification: this module is the single encoder and
//! the single validator for them. It performs no I/O and takes no time.

use serde::{Deserialize, Serialize};

use super::error::RejectReason;

/// The fragment magic: `4D 4C 50 00` ("MLP" followed by a zero byte).
///
/// The zero terminator keeps the magic a fixed four bytes without implying
/// a version: the version travels in its own header field.
pub const FRAGMENT_MAGIC: [u8; 4] = *b"MLP\0";
/// The MLP protocol version carried by every fragment.
pub const MEDIA_PROTOCOL_VERSION: u8 = 1;
/// Fixed header size (bytes before the header extension).
pub const FRAGMENT_FIXED_HEADER: usize = 22;
/// Maximum `fragment_count` the wire field admits (MLP spec §6.2).
pub const MAX_FRAGMENT_COUNT: u16 = 4096;
/// Maximum `header_ext_len` (MLP spec §6.2).
pub const MAX_HEADER_EXT: u16 = 256;
/// Payload cap of a WebSocket carrier message (MLP spec §6.1).
pub const MAX_WS_FRAGMENT: usize = 65_536;
/// Cap of concatenated fragments in one gRPC `MediaEnvelope` (MLP spec §6.1).
pub const MAX_GRPC_COALESCE: usize = 65_536;
/// Fragment payload target on the S2 datagram carriers (MLP spec §6.1).
pub const TARGET_FRAGMENT_PAYLOAD: usize = 1_150;
/// The control lane never carries a media fragment above this size (MLP spec §4.2).
pub const MAX_CONTROL_LANE_OBJECT_BYTES: usize = 16 * 1024;

/// The header-extension TLV type registry (MLP spec §6.3).
pub mod tlv {
    /// `u64` — the update id this part belongs to.
    pub const UPDATE_ID: u8 = 0x01;
    /// `u64` — the update this one continues (`0` = self-contained).
    pub const UPDATE_BASE: u8 = 0x02;
    /// `u16 index ‖ u16 count` — the part's index within its update.
    pub const PART: u8 = 0x03;
    /// `u32` — total payload bytes of the whole update.
    pub const UPDATE_BYTES: u8 = 0x04;
    /// `u32` — chain epoch; changes when the sender breaks the chain.
    pub const CHAIN_EPOCH: u8 = 0x05;
    /// `u64` — highest input `input_seq` reflected by this update.
    pub const REFLECTS_INPUT_SEQ: u8 = 0x06;
    /// `u64` — sender's session-relative ms at first emission.
    pub const SEND_EPOCH_MS: u8 = 0x07;
    /// `u64` — absolute session-relative deadline.
    pub const DEADLINE_EPOCH_MS: u8 = 0x08;
    /// `u32` — object being echoed or acked.
    pub const ECHO_OF_OBJECT_ID: u8 = 0x09;
    /// `u32` — the sender's current per-update budget.
    pub const BUDGET_BYTES: u8 = 0x0A;
    /// `u32` — the sender's current pace.
    pub const PACE_BPS: u8 = 0x0B;
    /// `u8` — which carrier the sender believes is active.
    pub const PATH_HINT: u8 = 0x0C;
    /// `u64` — input object sequence.
    pub const INPUT_SEQ: u8 = 0x0D;
    /// `u32` — the sender's cell grid size for the update's geometry.
    pub const CELL_COUNT: u8 = 0x0E;
    /// `u16` — measured processing cost reported by a hop.
    pub const COST_MS: u8 = 0x0F;

    /// First reserved type: receivers skip these silently (MLP spec §6.3).
    pub const RESERVED_FIRST: u8 = 0x10;
    /// Last reserved type.
    pub const RESERVED_LAST: u8 = 0xEF;
    /// First experimental type: skipped and counted.
    pub const EXPERIMENTAL_FIRST: u8 = 0xF0;
    /// Last experimental type.
    pub const EXPERIMENTAL_LAST: u8 = 0xFF;

    /// Whether a type is in the reserved band `0x10..=0xEF`.
    pub const fn is_reserved(kind: u8) -> bool {
        kind >= RESERVED_FIRST && kind <= RESERVED_LAST
    }

    /// Whether a type is experimental `0xF0..=0xFF`.
    pub const fn is_experimental(kind: u8) -> bool {
        kind >= EXPERIMENTAL_FIRST
    }

    /// Whether a type is named by this registry.
    pub const fn is_known(kind: u8) -> bool {
        kind >= UPDATE_ID && kind <= COST_MS
    }
}

/// The transport class field of a fragment (MLP spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum FragmentClass {
    /// Media-lane control (C0-equivalent).
    Control = 0,
    /// Cursor (C2).
    Cursor = 1,
    /// Media (C3).
    Media = 2,
    /// Feedback/telemetry (C4).
    Feedback = 3,
    /// Path/probe control (C5).
    Path = 4,
}

impl FragmentClass {
    /// Decode the wire value; `5..=255` is `None` (`BAD_CLASS`).
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Control),
            1 => Some(Self::Cursor),
            2 => Some(Self::Media),
            3 => Some(Self::Feedback),
            4 => Some(Self::Path),
            _ => None,
        }
    }

    /// The wire value.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The traffic class this fragment class maps to (MLP spec §4.1, §6.2).
    pub const fn traffic_class(self) -> TrafficClass {
        match self {
            Self::Control | Self::Path => TrafficClass::Control,
            Self::Cursor => TrafficClass::Cursor,
            Self::Media => TrafficClass::Media,
            Self::Feedback => TrafficClass::Feedback,
        }
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Cursor => "cursor",
            Self::Media => "media",
            Self::Feedback => "feedback",
            Self::Path => "path",
        }
    }
}

/// Traffic classes of MLP spec §4.1, including the control-lane classes C0/C1.
///
/// C0, C1 and C4 (reports) ride the control lane; C2, C3 and C4 (acks) ride the
/// media lane. The enum is the shared vocabulary for scheduling, pacing and
/// priority decisions on both lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum TrafficClass {
    /// C0 — control/lifecycle (highest priority).
    Control = 0,
    /// C1 — input and command feedback.
    Input = 1,
    /// C4 — feedback/telemetry.
    Feedback = 2,
    /// C2 — cursor.
    Cursor = 3,
    /// C3 — media (interactive).
    Media = 4,
    /// C3 — media (background refresh).
    Refresh = 5,
}

impl TrafficClass {
    /// Default MLP priority field for this class (MLP spec §4.1).
    pub const fn default_priority(self) -> u8 {
        match self {
            Self::Control => 0,
            Self::Input => 1,
            Self::Feedback => 2,
            Self::Cursor => 5,
            Self::Media => 6,
            Self::Refresh => 7,
        }
    }

    /// Whether objects of this class may be dropped by the pacer's reserve rule.
    pub const fn is_media(self) -> bool {
        matches!(self, Self::Media | Self::Refresh)
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Input => "input",
            Self::Feedback => "feedback",
            Self::Cursor => "cursor",
            Self::Media => "media",
            Self::Refresh => "refresh",
        }
    }
}

/// The `flags` byte (MLP spec §6.2): bit 7 is reserved and must be zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FragmentFlags(u8);

impl FragmentFlags {
    /// Last fragment of the object.
    pub const FIN: Self = Self(0x01);
    /// Media: absolute writes that apply onto any held state.
    pub const ANCHOR: Self = Self(0x02);
    /// A resend by the sender's own policy.
    pub const RETRANSMIT: Self = Self(0x04);
    /// A deliberate duplicate for echo-critical objects.
    pub const DUP: Self = Self(0x08);
    /// Receiver must reply with an echo of the object.
    pub const ECHO_REQUESTED: Self = Self(0x10);
    /// A probe object.
    pub const PROBE: Self = Self(0x20);
    /// Control: drain before queued media.
    pub const URGENT: Self = Self(0x40);
    /// No flags set.
    pub const NONE: Self = Self(0x00);

    /// Build from a raw byte; bit 7 set is `None` (`BAD_FLAGS`).
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & 0x80 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// The raw bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union of two flag sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether no flag is set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::fmt::Debug for FragmentFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut parts = Vec::new();
        for (flag, name) in [
            (Self::FIN, "FIN"),
            (Self::ANCHOR, "ANCHOR"),
            (Self::RETRANSMIT, "RETRANSMIT"),
            (Self::DUP, "DUP"),
            (Self::ECHO_REQUESTED, "ECHO_REQUESTED"),
            (Self::PROBE, "PROBE"),
            (Self::URGENT, "URGENT"),
        ] {
            if self.contains(flag) {
                parts.push(name);
            }
        }
        if parts.is_empty() {
            write!(f, "FragmentFlags(none)")
        } else {
            write!(f, "FragmentFlags({})", parts.join("|"))
        }
    }
}

/// The `priority` field: `0` is highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Priority(u8);

impl Priority {
    /// Control/ack priority (`0`).
    pub const CONTROL: Self = Self(0);
    /// Input priority (`1`).
    pub const INPUT: Self = Self(1);
    /// Feedback report priority (`2`).
    pub const REPORT: Self = Self(2);
    /// Cursor priority (`5`).
    pub const CURSOR: Self = Self(5);
    /// Interactive media priority (`6`).
    pub const MEDIA: Self = Self(6);
    /// Background refresh priority (`7`).
    pub const REFRESH: Self = Self(7);

    /// Build from `0..=7`; any other value is `None`.
    pub const fn new(value: u8) -> Option<Self> {
        if value <= 7 { Some(Self(value)) } else { None }
    }

    /// The raw field value.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Whether this priority is reserved for control, cursor and feedback.
    ///
    /// Media fragments may not use `0` or `1` (MLP spec §6.2): those are what the
    /// pacer's reserved rate protects.
    pub const fn is_reserved_for_control(self) -> bool {
        self.0 <= 1
    }
}

/// The carrier a transport message was received from or will be sent on
/// (MLP spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Carrier {
    /// WebSocket: exactly one fragment per message, 64 KiB payload cap.
    Ws,
    /// gRPC: 1..N concatenated fragments per message, 64 KiB coalesce cap.
    Grpc,
    /// WebRTC data channel `rm.media`: one fragment per SCTP message.
    DataChannel,
    /// WebRTC data channel `rm.repair`.
    RepairChannel,
    /// WebRTC data channel `rm.control` (media-lane control only).
    ControlChannel,
}

impl Carrier {
    /// The largest payload a single fragment may carry on this carrier.
    pub const fn fragment_payload_max(self) -> usize {
        match self {
            Self::Ws | Self::Grpc => MAX_WS_FRAGMENT,
            Self::DataChannel | Self::RepairChannel | Self::ControlChannel => {
                TARGET_FRAGMENT_PAYLOAD
            }
        }
    }

    /// The largest concatenated fragment message this carrier accepts.
    pub const fn message_cap(self) -> usize {
        match self {
            Self::Ws | Self::ControlChannel | Self::DataChannel | Self::RepairChannel => {
                self.fragment_payload_max()
            }
            Self::Grpc => MAX_GRPC_COALESCE,
        }
    }

    /// Whether this carrier is datagram-class (one fragment per message).
    pub const fn is_datagram(self) -> bool {
        matches!(
            self,
            Self::DataChannel | Self::RepairChannel | Self::ControlChannel
        )
    }

    /// Whether this carrier is stream-class (1..N fragments per message).
    pub const fn is_stream(self) -> bool {
        !self.is_datagram()
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::Grpc => "grpc",
            Self::DataChannel => "dc",
            Self::RepairChannel => "dc-repair",
            Self::ControlChannel => "dc-control",
        }
    }
}

/// The parsed fragment header (fixed part).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentHeader {
    /// `version` field; always the one media protocol version for an accepted fragment.
    pub version: u8,
    /// Transport class.
    pub class: FragmentClass,
    /// Flags (bit 7 always zero after parsing).
    pub flags: FragmentFlags,
    /// Priority (`0` highest).
    pub priority: Priority,
    /// Monotonic object id, per lane and direction.
    pub object_id: u32,
    /// Index of this fragment inside the object.
    pub fragment_index: u16,
    /// Total fragments of the object.
    pub fragment_count: u16,
    /// Payload byte count.
    pub payload_len: u32,
    /// Header extension byte count.
    pub header_ext_len: u16,
}

/// One header-extension entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlvEntry {
    /// TLV type byte.
    pub kind: u8,
    /// TLV value bytes.
    pub value: Vec<u8>,
}

impl TlvEntry {
    /// Create an entry from raw parts.
    pub fn new(kind: u8, value: Vec<u8>) -> Self {
        Self { kind, value }
    }
}

/// The header extension: zero or more `u8 type, u8 len, len bytes value` TLVs.
///
/// Unknown types are skipped on parse and preserved on re-encode so a hop can
/// forward a fragment it does not interpret. Experimental types are counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderExt {
    entries: Vec<TlvEntry>,
}

impl HeaderExt {
    /// An empty extension.
    pub fn new() -> Self {
        Self::default()
    }

    /// The entries in wire order.
    pub fn entries(&self) -> &[TlvEntry] {
        &self.entries
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the extension carries no TLV.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Encoded size in bytes.
    pub fn encoded_len(&self) -> usize {
        self.entries.iter().map(|e| 2 + e.value.len()).sum()
    }

    fn push(&mut self, kind: u8, value: Vec<u8>) -> Result<(), RejectReason> {
        if value.len() > u8::MAX as usize {
            return Err(RejectReason::BadExt);
        }
        let next = self.encoded_len() + 2 + value.len();
        if next > MAX_HEADER_EXT as usize {
            return Err(RejectReason::BadExt);
        }
        self.entries.push(TlvEntry { kind, value });
        Ok(())
    }

    /// Append an arbitrary TLV; fails when the extension would exceed 256 B.
    pub fn push_raw(&mut self, kind: u8, value: Vec<u8>) -> Result<(), RejectReason> {
        self.push(kind, value)
    }

    /// Append a `u64` TLV.
    pub fn push_u64(&mut self, kind: u8, value: u64) -> Result<(), RejectReason> {
        self.push(kind, value.to_le_bytes().to_vec())
    }

    /// Append a `u32` TLV.
    pub fn push_u32(&mut self, kind: u8, value: u32) -> Result<(), RejectReason> {
        self.push(kind, value.to_le_bytes().to_vec())
    }

    /// Append a `u16 ‖ u16` TLV (the `CHUNK` shape).
    pub fn push_u16_pair(&mut self, kind: u8, first: u16, second: u16) -> Result<(), RejectReason> {
        self.push(kind, [first.to_le_bytes(), second.to_le_bytes()].concat())
    }

    /// Append a `u16` TLV.
    pub fn push_u16(&mut self, kind: u8, value: u16) -> Result<(), RejectReason> {
        self.push(kind, value.to_le_bytes().to_vec())
    }

    /// Append a `u8` TLV.
    pub fn push_u8(&mut self, kind: u8, value: u8) -> Result<(), RejectReason> {
        self.push(kind, vec![value])
    }

    /// The first value with the given type.
    pub fn get(&self, kind: u8) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|e| e.kind == kind)
            .map(|e| e.value.as_slice())
    }

    /// Every value with the given type, in wire order.
    pub fn get_all(&self, kind: u8) -> impl Iterator<Item = &[u8]> {
        self.entries
            .iter()
            .filter(move |e| e.kind == kind)
            .map(|e| e.value.as_slice())
    }

    /// Number of experimental (`0xF0..=0xFF`) entries.
    pub fn experimental_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| tlv::is_experimental(e.kind))
            .count()
    }

    /// Number of reserved (`0x10..=0xEF`) entries.
    pub fn reserved_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| tlv::is_reserved(e.kind))
            .count()
    }

    fn get_u64(&self, kind: u8) -> Option<u64> {
        let value = self.get(kind)?;
        if value.len() != 8 {
            return None;
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(value);
        Some(u64::from_le_bytes(bytes))
    }

    fn get_u32(&self, kind: u8) -> Option<u32> {
        let value = self.get(kind)?;
        if value.len() != 4 {
            return None;
        }
        let mut bytes = [0u8; 4];
        bytes.copy_from_slice(value);
        Some(u32::from_le_bytes(bytes))
    }

    fn get_u16(&self, kind: u8) -> Option<u16> {
        let value = self.get(kind)?;
        if value.len() != 2 {
            return None;
        }
        let mut bytes = [0u8; 2];
        bytes.copy_from_slice(value);
        Some(u16::from_le_bytes(bytes))
    }

    /// The `UPDATE_ID` value, when present and well-formed.
    pub fn update_id(&self) -> Option<u64> {
        self.get_u64(tlv::UPDATE_ID)
    }

    /// The `UPDATE_BASE` value.
    pub fn update_base(&self) -> Option<u64> {
        self.get_u64(tlv::UPDATE_BASE)
    }

    /// The `CHUNK` pair `(index, count)`.
    pub fn chunk(&self) -> Option<(u16, u16)> {
        let value = self.get(tlv::PART)?;
        if value.len() != 4 {
            return None;
        }
        let index = u16::from_le_bytes([value[0], value[1]]);
        let count = u16::from_le_bytes([value[2], value[3]]);
        Some((index, count))
    }

    /// The `UPDATE_BYTES` value.
    pub fn update_bytes(&self) -> Option<u32> {
        self.get_u32(tlv::UPDATE_BYTES)
    }

    /// The `CHAIN_EPOCH` value.
    pub fn chain_epoch(&self) -> Option<u32> {
        self.get_u32(tlv::CHAIN_EPOCH)
    }

    /// The `REFLECTS_INPUT_SEQ` value.
    pub fn reflects_input_seq(&self) -> Option<u64> {
        self.get_u64(tlv::REFLECTS_INPUT_SEQ)
    }

    /// The `SEND_EPOCH_MS` value.
    pub fn send_epoch_ms(&self) -> Option<u64> {
        self.get_u64(tlv::SEND_EPOCH_MS)
    }

    /// The `DEADLINE_EPOCH_MS` value.
    pub fn deadline_epoch_ms(&self) -> Option<u64> {
        self.get_u64(tlv::DEADLINE_EPOCH_MS)
    }

    /// The `ECHO_OF_OBJECT_ID` value.
    pub fn echo_of_object_id(&self) -> Option<u32> {
        self.get_u32(tlv::ECHO_OF_OBJECT_ID)
    }

    /// The `BUDGET_BYTES` value.
    pub fn budget_bytes(&self) -> Option<u32> {
        self.get_u32(tlv::BUDGET_BYTES)
    }

    /// The `PACE_BPS` value.
    pub fn pace_bps(&self) -> Option<u32> {
        self.get_u32(tlv::PACE_BPS)
    }

    /// The `PATH_HINT` value as a path enum, when known.
    pub fn path_hint(&self) -> Option<super::MediaPath> {
        let raw = self.get(tlv::PATH_HINT)?;
        if raw.len() != 1 {
            return None;
        }
        Some(super::MediaPath::from_u8(raw[0]))
    }

    /// The `INPUT_SEQ` value.
    pub fn input_seq(&self) -> Option<u64> {
        self.get_u64(tlv::INPUT_SEQ)
    }

    /// The `CELL_COUNT` value.
    pub fn cell_count(&self) -> Option<u32> {
        self.get_u32(tlv::CELL_COUNT)
    }

    /// The `COST_MS` value.
    pub fn cost_ms(&self) -> Option<u16> {
        self.get_u16(tlv::COST_MS)
    }

    /// Encode the extension; the result is at most 256 bytes.
    pub fn encode(&self) -> Result<Vec<u8>, RejectReason> {
        let mut out = Vec::with_capacity(self.encoded_len());
        for entry in &self.entries {
            if entry.value.len() > u8::MAX as usize {
                return Err(RejectReason::BadExt);
            }
            out.push(entry.kind);
            out.push(entry.value.len() as u8);
            out.extend_from_slice(&entry.value);
        }
        if out.len() > MAX_HEADER_EXT as usize {
            return Err(RejectReason::BadExt);
        }
        Ok(out)
    }

    /// Parse an extension from its exact byte range.
    pub fn parse(bytes: &[u8]) -> Result<Self, RejectReason> {
        let mut entries = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            if offset + 2 > bytes.len() {
                return Err(RejectReason::BadExt);
            }
            let kind = bytes[offset];
            let len = bytes[offset + 1] as usize;
            offset += 2;
            if offset + len > bytes.len() {
                return Err(RejectReason::BadExt);
            }
            entries.push(TlvEntry {
                kind,
                value: bytes[offset..offset + len].to_vec(),
            });
            offset += len;
        }
        Ok(Self { entries })
    }
}

/// Media update metadata carried by the header extension (MLP spec §7.1).
///
/// An *update* is the protocol's unit of media state: a self-contained one
/// carries its whole surface, a chained one continues the surface the update
/// named by `update_base` established. A codec decides what an update's bytes
/// mean; the protocol only moves them and accounts for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UpdateMeta {
    /// The update id.
    pub update_id: u64,
    /// The update this one continues (`0` = self-contained).
    pub update_base: u64,
    /// Part index inside the update.
    pub part_index: u16,
    /// Part count of the update.
    pub part_count: u16,
    /// Total payload bytes of the whole update.
    pub update_bytes: u32,
    /// Chain epoch.
    pub chain_epoch: u32,
    /// Highest input sequence reflected by this update's content.
    pub reflects_input_seq: u64,
    /// Sender's session-relative ms at first emission.
    pub send_epoch_ms: u64,
}

impl UpdateMeta {
    /// Encode the update metadata into a header extension.
    pub fn to_ext(&self) -> HeaderExt {
        let mut ext = HeaderExt::new();
        // The extension is at most 46 bytes; the pushes cannot overflow 256.
        let _ = ext.push_u64(tlv::UPDATE_ID, self.update_id);
        let _ = ext.push_u64(tlv::UPDATE_BASE, self.update_base);
        let _ = ext.push_u16_pair(tlv::PART, self.part_index, self.part_count);
        let _ = ext.push_u32(tlv::UPDATE_BYTES, self.update_bytes);
        let _ = ext.push_u32(tlv::CHAIN_EPOCH, self.chain_epoch);
        let _ = ext.push_u64(tlv::REFLECTS_INPUT_SEQ, self.reflects_input_seq);
        let _ = ext.push_u64(tlv::SEND_EPOCH_MS, self.send_epoch_ms);
        ext
    }

    /// Read the update metadata out of an extension.
    pub fn from_ext(ext: &HeaderExt) -> Option<Self> {
        let (part_index, part_count) = ext.chunk().unwrap_or((0, 1));
        Some(Self {
            update_id: ext.update_id()?,
            update_base: ext.update_base().unwrap_or(0),
            part_index,
            part_count,
            update_bytes: ext.update_bytes().unwrap_or(0),
            chain_epoch: ext.chain_epoch().unwrap_or(0),
            reflects_input_seq: ext.reflects_input_seq().unwrap_or(0),
            send_epoch_ms: ext.send_epoch_ms().unwrap_or(0),
        })
    }

    /// Verify fragment metadata against an already-decoded update.
    ///
    /// Both directions are checked: a TLV may not disagree with the decoded
    /// body, and the body may not disagree with the TLV. Disagreement is
    /// `META_MISMATCH` (never "trust one of the two", MLP spec §6.3).
    pub fn check_against(&self, live: &UpdateMeta) -> Result<(), RejectReason> {
        if self != live {
            return Err(RejectReason::MetaMismatch);
        }
        Ok(())
    }
}

/// A fully parsed fragment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fragment {
    /// Parsed fixed header.
    pub header: FragmentHeader,
    /// Parsed header extension.
    pub ext: HeaderExt,
    /// Class-specific body bytes.
    pub payload: Vec<u8>,
}

impl Fragment {
    /// Assemble a fragment from parts, validating the header invariants.
    pub fn new(
        class: FragmentClass,
        flags: FragmentFlags,
        priority: Priority,
        object_id: u32,
        fragment_index: u16,
        fragment_count: u16,
        ext: HeaderExt,
        payload: Vec<u8>,
    ) -> Result<Self, RejectReason> {
        validate_header_fields(class, flags, priority, fragment_index, fragment_count)?;
        let header_ext_len = u16::try_from(ext.encoded_len()).map_err(|_| RejectReason::BadExt)?;
        if header_ext_len != 0 && header_ext_len < 2 {
            return Err(RejectReason::BadExt);
        }
        if header_ext_len > MAX_HEADER_EXT {
            return Err(RejectReason::BadExt);
        }
        Ok(Self {
            header: FragmentHeader {
                version: MEDIA_PROTOCOL_VERSION,
                class,
                flags,
                priority,
                object_id,
                fragment_index,
                fragment_count,
                payload_len: payload.len() as u32,
                header_ext_len,
            },
            ext,
            payload,
        })
    }

    /// Encode the fragment to wire bytes.
    pub fn encode(&self) -> Vec<u8> {
        let ext = self
            .ext
            .encode()
            .expect("header extension is validated at construction");
        let mut out = Vec::with_capacity(FRAGMENT_FIXED_HEADER + ext.len() + self.payload.len());
        out.extend_from_slice(&FRAGMENT_MAGIC);
        out.push(self.header.version);
        out.push(self.header.class.as_u8());
        out.push(self.header.flags.bits());
        out.push(self.header.priority.get());
        out.extend_from_slice(&self.header.object_id.to_le_bytes());
        out.extend_from_slice(&self.header.fragment_index.to_le_bytes());
        out.extend_from_slice(&self.header.fragment_count.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(ext.len() as u16).to_le_bytes());
        out.extend_from_slice(&ext);
        out.extend_from_slice(&self.payload);
        out
    }

    /// Whether this is the last fragment of its object.
    pub fn is_fin(&self) -> bool {
        self.header.flags.contains(FragmentFlags::FIN)
    }

    /// Parse one fragment from the front of `buf`, returning it and the number
    /// of bytes consumed.
    pub fn parse(buf: &[u8], carrier: Carrier) -> Result<(Self, usize), RejectReason> {
        if buf.len() < FRAGMENT_FIXED_HEADER {
            return Err(RejectReason::BadLength);
        }
        if buf[0..4] != FRAGMENT_MAGIC {
            return Err(RejectReason::BadMagic);
        }
        let version = buf[4];
        if version != MEDIA_PROTOCOL_VERSION {
            return Err(RejectReason::UnsupportedVersion);
        }
        let class = FragmentClass::from_u8(buf[5]).ok_or(RejectReason::BadClass)?;
        let flags = FragmentFlags::from_bits(buf[6]).ok_or(RejectReason::BadFlags)?;
        let priority_raw = buf[7];
        if priority_raw > 7 {
            return Err(RejectReason::BadPriority);
        }
        let priority = Priority::new(priority_raw).ok_or(RejectReason::BadPriority)?;
        let object_id = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let fragment_index = u16::from_le_bytes([buf[12], buf[13]]);
        let fragment_count = u16::from_le_bytes([buf[14], buf[15]]);
        let payload_len = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let header_ext_len = u16::from_le_bytes([buf[20], buf[21]]);

        validate_header_fields(class, flags, priority, fragment_index, fragment_count)?;

        if header_ext_len != 0 && header_ext_len < 2 {
            return Err(RejectReason::BadExt);
        }
        if header_ext_len > MAX_HEADER_EXT {
            return Err(RejectReason::BadExt);
        }
        if payload_len as usize > carrier.fragment_payload_max() {
            return Err(RejectReason::BadLength);
        }
        let ext_end = FRAGMENT_FIXED_HEADER + header_ext_len as usize;
        let payload_end = ext_end + payload_len as usize;
        if buf.len() < payload_end {
            return Err(RejectReason::BadLength);
        }
        let ext = HeaderExt::parse(&buf[FRAGMENT_FIXED_HEADER..ext_end])?;
        let payload = buf[ext_end..payload_end].to_vec();
        let fragment = Self {
            header: FragmentHeader {
                version,
                class,
                flags,
                priority,
                object_id,
                fragment_index,
                fragment_count,
                payload_len,
                header_ext_len,
            },
            ext,
            payload,
        };
        Ok((fragment, payload_end))
    }
}

fn validate_header_fields(
    class: FragmentClass,
    _flags: FragmentFlags,
    priority: Priority,
    fragment_index: u16,
    fragment_count: u16,
) -> Result<(), RejectReason> {
    if fragment_count == 0
        || fragment_count > MAX_FRAGMENT_COUNT
        || fragment_index >= fragment_count
    {
        return Err(RejectReason::BadFragment);
    }
    if class == FragmentClass::Media && priority.is_reserved_for_control() {
        return Err(RejectReason::BadPriority);
    }
    Ok(())
}

/// Parse a whole transport message into its fragments.
///
/// Stream carriers accept 1..N concatenated fragments; datagram carriers
/// accept exactly one (MLP spec §6.1, §6.4 step 3).
pub fn parse_message(buf: &[u8], carrier: Carrier) -> Result<Vec<Fragment>, RejectReason> {
    let mut fragments = Vec::new();
    let mut offset = 0usize;
    while offset < buf.len() {
        let (fragment, consumed) = Fragment::parse(&buf[offset..], carrier)?;
        fragments.push(fragment);
        offset += consumed;
    }
    if fragments.is_empty() {
        return Err(RejectReason::BadLength);
    }
    if carrier.is_datagram() {
        if fragments.len() != 1 {
            return Err(RejectReason::MultiFragmentOnDatagram);
        }
        if fragments[0].header.fragment_count != 1 {
            return Err(RejectReason::MultiFragmentOnDatagram);
        }
    }
    if fragments.len() > 1 && fragments.len() > usize::from(MAX_FRAGMENT_COUNT) {
        return Err(RejectReason::BadLength);
    }
    Ok(fragments)
}

/// Split an encoded payload into fragment payload slices for a carrier.
///
/// `fragment_payload_max` is `min(carrier cap, config cap)`. A payload that
/// does not fit `MAX_FRAGMENT_COUNT` fragments is an encoder invariant breach;
/// the caller decides what to do with it (the specification counts
/// `mlp_object_oversize_total` and drops it).
pub fn fragment_count_for(payload_len: usize, fragment_payload_max: usize) -> Option<u16> {
    if fragment_payload_max == 0 {
        return None;
    }
    let count = payload_len.div_ceil(fragment_payload_max);
    if count == 0 {
        return Some(1);
    }
    u16::try_from(count)
        .ok()
        .filter(|c| *c <= MAX_FRAGMENT_COUNT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext_with_update(update_id: u64) -> HeaderExt {
        let mut ext = HeaderExt::new();
        ext.push_u64(tlv::UPDATE_ID, update_id).expect("fits");
        ext
    }

    fn media_fragment(object_id: u32, index: u16, count: u16, fin: bool) -> Fragment {
        let mut flags = FragmentFlags::NONE;
        if fin {
            flags = flags.union(FragmentFlags::FIN);
        }
        Fragment::new(
            FragmentClass::Media,
            flags,
            Priority::MEDIA,
            object_id,
            index,
            count,
            HeaderExt::new(),
            vec![0xAB; 16],
        )
        .expect("valid fragment")
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let mut flags = FragmentFlags::FIN.union(FragmentFlags::ANCHOR);
        flags = flags.union(FragmentFlags::ECHO_REQUESTED);
        let fragment = Fragment::new(
            FragmentClass::Feedback,
            flags,
            Priority::REPORT,
            0xDEAD_BEEF,
            1,
            3,
            ext_with_update(42),
            vec![1, 2, 3, 4, 5],
        )
        .expect("valid");
        let bytes = fragment.encode();
        let (parsed, consumed) = Fragment::parse(&bytes, Carrier::Ws).expect("parses");
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, fragment);
    }

    #[test]
    fn fixed_header_is_22_bytes() {
        let fragment = media_fragment(1, 0, 1, true);
        let bytes = fragment.encode();
        assert_eq!(&bytes[0..4], &FRAGMENT_MAGIC);
        assert_eq!(bytes[4], MEDIA_PROTOCOL_VERSION);
        assert_eq!(bytes[5], 2);
        assert_eq!(bytes.len(), FRAGMENT_FIXED_HEADER + 16);
    }

    #[test]
    fn empty_payload_is_legal() {
        let fragment = Fragment::new(
            FragmentClass::Media,
            FragmentFlags::FIN,
            Priority::MEDIA,
            7,
            0,
            1,
            HeaderExt::new(),
            Vec::new(),
        )
        .expect("valid");
        let bytes = fragment.encode();
        let (parsed, consumed) = Fragment::parse(&bytes, Carrier::Ws).expect("parses");
        assert_eq!(consumed, FRAGMENT_FIXED_HEADER);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn bad_magic_is_rejected_whole() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[0] = b'X';
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadMagic)
        );
    }

    #[test]
    fn unknown_version_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[4] = 3;
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::UnsupportedVersion)
        );
    }

    #[test]
    fn reserved_class_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[5] = 5;
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadClass)
        );
    }

    #[test]
    fn reserved_flag_bit_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[6] |= 0x80;
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadFlags)
        );
    }

    #[test]
    fn media_may_not_use_control_priorities() {
        for priority in [Priority::CONTROL, Priority::INPUT] {
            let err = Fragment::new(
                FragmentClass::Media,
                FragmentFlags::FIN,
                priority,
                1,
                0,
                1,
                HeaderExt::new(),
                vec![0; 4],
            );
            assert_eq!(err, Err(RejectReason::BadPriority));
        }
        // The same priorities are legal for control and cursor classes.
        for class in [FragmentClass::Control, FragmentClass::Cursor] {
            Fragment::new(
                class,
                FragmentFlags::FIN,
                Priority::CONTROL,
                1,
                0,
                1,
                HeaderExt::new(),
                vec![0; 4],
            )
            .expect("control class may use priority 0");
        }
    }

    #[test]
    fn out_of_range_priority_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[7] = 8;
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadPriority)
        );
    }

    #[test]
    fn fragment_index_must_be_below_count() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[14..16].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadFragment)
        );

        let fragment = media_fragment(1, 2, 3, true);
        let mut bytes = fragment.encode();
        bytes[12..14].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadFragment)
        );
    }

    #[test]
    fn fragment_count_above_4096_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[14..16].copy_from_slice(&4097u16.to_le_bytes());
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadFragment)
        );
    }

    #[test]
    fn payload_above_carrier_cap_is_rejected() {
        let fragment = Fragment::new(
            FragmentClass::Media,
            FragmentFlags::FIN,
            Priority::MEDIA,
            1,
            0,
            1,
            HeaderExt::new(),
            vec![0; TARGET_FRAGMENT_PAYLOAD],
        )
        .expect("valid");
        let bytes = fragment.encode();
        // 1150 bytes fit a datagram carrier ...
        Fragment::parse(&bytes, Carrier::DataChannel).expect("datagram cap 1150");
        // ... but a fabricated header claiming more does not.
        let mut lying = bytes.clone();
        let big = (TARGET_FRAGMENT_PAYLOAD + 1) as u32;
        lying[16..20].copy_from_slice(&big.to_le_bytes());
        assert_eq!(
            Fragment::parse(&lying, Carrier::DataChannel),
            Err(RejectReason::BadLength)
        );
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let bytes = fragment.encode();
        assert_eq!(
            Fragment::parse(&bytes[..bytes.len() - 1], Carrier::Ws),
            Err(RejectReason::BadLength)
        );
    }

    #[test]
    fn odd_header_ext_length_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[20..22].copy_from_slice(&1u16.to_le_bytes());
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadExt)
        );
    }

    #[test]
    fn header_ext_overflow_is_rejected() {
        let fragment = media_fragment(1, 0, 1, true);
        let mut bytes = fragment.encode();
        bytes[20..22].copy_from_slice(&300u16.to_le_bytes());
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadExt)
        );
    }

    #[test]
    fn tlv_with_len_past_the_extension_is_rejected() {
        // One TLV claiming 8 bytes of value with only 4 present.
        let mut bytes = vec![0u8; FRAGMENT_FIXED_HEADER + 2 + 4];
        bytes[0..4].copy_from_slice(&FRAGMENT_MAGIC);
        bytes[4] = MEDIA_PROTOCOL_VERSION;
        bytes[5] = 2;
        bytes[6] = 0x01;
        bytes[7] = 6;
        bytes[12..14].copy_from_slice(&0u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&1u16.to_le_bytes());
        bytes[16..20].copy_from_slice(&0u32.to_le_bytes());
        bytes[20..22].copy_from_slice(&6u16.to_le_bytes());
        bytes[22] = tlv::UPDATE_ID;
        bytes[23] = 8;
        assert_eq!(
            Fragment::parse(&bytes, Carrier::Ws),
            Err(RejectReason::BadExt)
        );
    }

    #[test]
    fn unknown_and_experimental_tlvs_are_skipped_and_counted() {
        let mut ext = HeaderExt::new();
        ext.push_u64(tlv::UPDATE_ID, 9).expect("fits");
        ext.push_raw(0x80, vec![1, 2, 3]).expect("reserved fits");
        ext.push_raw(0xF1, vec![9, 9]).expect("experimental fits");
        assert_eq!(ext.experimental_count(), 1);
        assert_eq!(ext.reserved_count(), 1);
        assert_eq!(ext.update_id(), Some(9));
        let bytes = ext.encode().expect("encodes");
        let parsed = HeaderExt::parse(&bytes).expect("parses");
        assert_eq!(parsed, ext);
    }

    #[test]
    fn ext_beyond_256_bytes_is_refused_by_push() {
        let mut ext = HeaderExt::new();
        // 25 entries of 10 bytes fit exactly in the 256-byte extension.
        for _ in 0..25 {
            assert!(ext.push_raw(0x80, vec![0; 8]).is_ok());
        }
        assert_eq!(ext.encoded_len(), 250);
        assert_eq!(ext.push_raw(0x80, vec![0; 8]), Err(RejectReason::BadExt));
    }

    #[test]
    fn parse_message_accepts_n_fragments_on_stream() {
        let first = media_fragment(3, 0, 2, false);
        let second = media_fragment(3, 1, 2, true);
        let mut bytes = first.encode();
        bytes.extend(second.encode());
        let parsed = parse_message(&bytes, Carrier::Grpc).expect("two fragments");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].header.fragment_index, 1);
    }

    #[test]
    fn parse_message_rejects_multi_fragment_objects_on_datagram() {
        let fragment = media_fragment(3, 0, 2, false);
        let bytes = fragment.encode();
        assert_eq!(
            parse_message(&bytes, Carrier::DataChannel),
            Err(RejectReason::MultiFragmentOnDatagram)
        );
    }

    #[test]
    fn parse_message_rejects_a_second_fragment_on_datagram() {
        let first = media_fragment(3, 0, 1, true);
        let second = media_fragment(4, 0, 1, true);
        let mut bytes = first.encode();
        bytes.extend(second.encode());
        assert_eq!(
            parse_message(&bytes, Carrier::DataChannel),
            Err(RejectReason::MultiFragmentOnDatagram)
        );
    }

    #[test]
    fn parse_message_requires_at_least_one_fragment() {
        assert_eq!(
            parse_message(&[], Carrier::Ws),
            Err(RejectReason::BadLength)
        );
    }

    #[test]
    fn update_meta_round_trips_through_tlvs() {
        let meta = UpdateMeta {
            update_id: 7,
            update_base: 6,
            part_index: 1,
            part_count: 4,
            update_bytes: 1234,
            chain_epoch: 3,
            reflects_input_seq: 99,
            send_epoch_ms: 1_000,
        };
        let ext = meta.to_ext();
        assert_eq!(UpdateMeta::from_ext(&ext), Some(meta));
        assert_eq!(meta.check_against(&meta), Ok(()));
        let other = UpdateMeta {
            update_id: 8,
            ..meta
        };
        assert_eq!(meta.check_against(&other), Err(RejectReason::MetaMismatch));
    }

    #[test]
    fn fragment_count_arithmetic() {
        assert_eq!(fragment_count_for(0, 1150), Some(1));
        assert_eq!(fragment_count_for(1, 1150), Some(1));
        assert_eq!(fragment_count_for(1150, 1150), Some(1));
        assert_eq!(fragment_count_for(1151, 1150), Some(2));
        assert_eq!(fragment_count_for(4096 * 1150, 1150), Some(4096));
        assert_eq!(fragment_count_for(4096 * 1150 + 1, 1150), None);
    }

    #[test]
    fn carrier_caps_match_the_design() {
        assert_eq!(Carrier::Ws.fragment_payload_max(), 65_536);
        assert_eq!(Carrier::DataChannel.fragment_payload_max(), 1_150);
        assert_eq!(Carrier::Grpc.message_cap(), 65_536);
        assert!(Carrier::DataChannel.is_datagram());
        assert!(Carrier::Ws.is_stream());
    }

    #[test]
    fn class_to_traffic_class_mapping() {
        assert_eq!(
            FragmentClass::Control.traffic_class(),
            TrafficClass::Control
        );
        assert_eq!(FragmentClass::Cursor.traffic_class(), TrafficClass::Cursor);
        assert_eq!(FragmentClass::Media.traffic_class(), TrafficClass::Media);
        assert_eq!(
            FragmentClass::Feedback.traffic_class(),
            TrafficClass::Feedback
        );
        assert_eq!(FragmentClass::Path.traffic_class(), TrafficClass::Control);
        assert_eq!(TrafficClass::Media.default_priority(), 6);
        assert_eq!(TrafficClass::Refresh.default_priority(), 7);
        assert_eq!(TrafficClass::Control.default_priority(), 0);
    }
}
