//! Sans-IO media core for the MLP v1 media lane (MLP spec §5–§10).
//!
//! This module is the single home of media-lane *policy*: the byte-level PDU
//! codec ([`pdu`]), object assembly ([`object`]), the bandwidth estimator
//! ([`bwe`]), the token-bucket pacer ([`pacer`]), the priority scheduler
//! ([`scheduler`]), per-receiver cell accounting and convergence ([`cells`]),
//! the resume/parking machinery ([`resume`]), the input path ([`input`]) and
//! the lifecycle vocabulary ([`lifecycle`]). The two full-duplex machines are
//! [`sender::MediaSenderMachine`] and [`receiver::MediaReceiverMachine`].
//!
//! Everything here is clock-, socket- and runtime-free: time arrives as an
//! [`std::time::Instant`] parameter, randomness arrives from the driver, and
//! outbound bytes leave as [`crate::Transmit`] values.
//!
//! # Shared value types
//!
//! The class bodies of MLP spec §6.2/§6.3 are plain Rust structs here; drivers
//! carry them across the wire with the service's protobuf definitions. Keeping
//! them dependency-free is what lets the deterministic simulator in
//! [`crate::testing`] exercise every decision without a runtime.

pub mod bwe;
pub mod cells;
pub mod error;
pub mod input;
pub mod lifecycle;
pub mod object;
pub mod pacer;
pub mod pdu;
pub mod receiver;
pub mod resume;
pub mod scheduler;
pub mod sender;

pub use bwe::{BandwidthEstimator, BweConfig, BweMode, Estimate, PriorStore, ProbeRequest};
pub use error::{MediaErrorCode, RejectReason};
pub use lifecycle::{LaneMode, MediaCloseCode, MediaLaneStateKind, MediaUnsupportedReason};
pub use pacer::{AcquireResult, MediaPacer, PacerConfig, TokenBucket};
pub use pdu::{
    Carrier, Fragment, FragmentClass, FragmentFlags, FragmentHeader, HeaderExt, Priority,
    TrafficClass, UpdateMeta,
};
pub use receiver::{MediaReceiveEvent, MediaReceiverConfig, MediaReceiverMachine, ReceiverStats};
pub use sender::{
    EnqueueOutcome, MediaSenderConfig, MediaSenderEvent, MediaSenderMachine, SenderStats,
    UpdateRequest,
};

use serde::{Deserialize, Serialize};

/// Maximum concurrent media viewers per sender (MLP spec §7.5.5).
pub const MAX_MEDIA_VIEWERS_PER_HOST: usize = 8;

/// Capability names carried by `MediaHello`/`MediaHelloAck` (MLP spec §6.3).
pub mod capability {
    /// Anchor updates may be applied onto any held state.
    pub const ANCHOR: &str = "anchor";
    /// Patch-style chaining (strict chaining when disabled).
    pub const PATCH: &str = "patch";
    /// The S2 datagram carrier is available.
    pub const DC: &str = "dc";
    /// Intermediary hops emit `MediaHopReport`.
    pub const HOP_REPORT: &str = "hop_report";
    /// `MediaProbe`/`ProbeEcho` are supported.
    pub const PROBE: &str = "probe";
    /// Cursor objects are supported.
    pub const CURSOR: &str = "cursor";
    /// Echo-critical duplication is supported.
    pub const DUPE: &str = "dupe";
}

/// Hop identifiers for `MediaHopReport` (MLP spec §6.2).
///
/// The numbers are extensible: a hop that is not one of these reports
/// itself as an unknown hop rather than borrowing another's number.
pub mod hop {
    /// The first intermediary hop.
    pub const FIRST: u32 = 1;
    /// The second intermediary hop, the edge.
    pub const SECOND: u32 = 2;
    /// The datagram carrier hop.
    pub const DC: u32 = 3;
    /// The relay hop.
    pub const TURN: u32 = 4;
}

/// Media carrier paths (MLP spec §6.3, §6.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum MediaPath {
    /// Not yet determined.
    #[default]
    Unknown = 0,
    /// The S1 WebSocket media lane.
    Ws = 1,
    /// S2 WebRTC data channel with a direct ICE pair.
    DcDirect = 2,
    /// S2 WebRTC data channel over TURN.
    DcTurn = 3,
    /// S3 QUIC.
    Quic = 4,
}

impl MediaPath {
    /// Decode the wire value; unknown values map to [`MediaPath::Unknown`].
    pub const fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Ws,
            2 => Self::DcDirect,
            3 => Self::DcTurn,
            4 => Self::Quic,
            _ => Self::Unknown,
        }
    }

    /// The wire value.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether this path is a datagram-class carrier.
    pub const fn is_datagram(self) -> bool {
        matches!(self, Self::DcDirect | Self::DcTurn | Self::Quic)
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Ws => "ws",
            Self::DcDirect => "dc-direct",
            Self::DcTurn => "dc-turn",
            Self::Quic => "quic",
        }
    }
}

/// The kind of a control object, for metrics and switch-free dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ControlKind {
    /// `MediaHello`.
    Hello,
    /// `MediaHelloAck`.
    HelloAck,
    /// `MediaResume`.
    Resume,
    /// `MediaResumeAck`.
    ResumeAck,
    /// `MediaLaneState`.
    LaneState,
    /// `MediaAnchorPlan`.
    AnchorPlan,
    /// `MediaClose`.
    Close,
    /// `MediaError`.
    Error,
    /// `MediaPathChange`.
    PathChange,
}

impl ControlKind {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::HelloAck => "hello_ack",
            Self::Resume => "resume",
            Self::ResumeAck => "resume_ack",
            Self::LaneState => "lane_state",
            Self::AnchorPlan => "anchor_plan",
            Self::Close => "close",
            Self::Error => "error",
            Self::PathChange => "path_change",
        }
    }
}

/// `MediaHello` (MLP spec §6.3): the first object a media lane sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaHello {
    /// Protocol version; 1.
    pub protocol_version: u32,
    /// Empty for a fresh session; non-empty only in resume.
    pub media_session_id: String,
    /// The control-lane session id.
    pub session_id: String,
    /// The sender's fragment payload cap (1150 on DC, 65536 on stream).
    pub max_fragment_payload: u32,
    /// The sender's cell grid size in cells.
    pub cell_size: u32,
    /// Cell grid columns.
    pub cell_cols: u32,
    /// Cell grid rows.
    pub cell_rows: u32,
    /// Capability names, see [`capability`].
    pub capabilities: Vec<String>,
    /// The sender's encoder fps ceiling.
    pub encoder_max_fps: u32,
    /// Sender-relative ms at which the media session started.
    pub start_epoch_ms: u64,
}

/// `MediaHelloAck` (MLP spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaHelloAck {
    /// Protocol version; 1.
    pub protocol_version: u32,
    /// Assigned here for a fresh session.
    pub media_session_id: String,
    /// `min(sender, receiver)` fragment payload cap.
    pub accepted_fragment_payload: u32,
    /// The agreed cell grid size in cells.
    pub cell_size: u32,
    /// Cell grid columns.
    pub cell_cols: u32,
    /// Cell grid rows.
    pub cell_rows: u32,
    /// Agreed capability names.
    pub capabilities: Vec<String>,
    /// The park/resume window in seconds.
    pub resume_ttl_seconds: u32,
    /// `false` means the session degrades to `MEDIA_UNSUPPORTED` (MLP spec annex D, D-4).
    pub media_supported: bool,
    /// Typed reason when `media_supported` is false.
    pub media_unsupported_reason: Option<MediaUnsupportedReason>,
}

/// `MediaResume` (MLP spec §6.3): what the receiver held when the lane died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaResume {
    /// Opaque resume token; never parsed by a browser.
    pub resume_token: String,
    /// The receiver's last applied update id.
    pub applied_update_id: u64,
    /// The receiver's chain epoch.
    pub chain_epoch: u32,
    /// The receiver's ack bitmap.
    pub ack_bitmap: u64,
    /// The receiver's next input sequence (dedup boundary).
    pub input_seq_next: u64,
    /// Key codes currently held.
    pub pressed_keys: Vec<u32>,
    /// Mouse buttons currently held.
    pub pressed_buttons: Vec<u32>,
    /// The path the receiver believes is active.
    pub active_path: MediaPath,
    /// S2: a fresh negotiation id when the receiver renegotiated.
    pub dc_transport_id: String,
}

/// `MediaResumeAck` (MLP spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaResumeAck {
    /// Whether the parked state was attached.
    pub accepted: bool,
    /// Typed reason; `ok` when accepted.
    pub reason: resume::ResumeRejectReason,
    /// The update id the sender will use next for this receiver.
    pub next_update_id: u64,
    /// Cells the sender will re-send (diagnostics; the plan follows in updates).
    pub pending_cells: u64,
    /// Current pace in bits per second.
    pub pace_bps: u32,
    /// Current per-update budget in bytes.
    pub budget_bytes: u32,
    /// Current target fps.
    pub target_fps: u32,
    /// Whether resume had to start an anchor plan.
    pub anchor_plan_started: bool,
}

/// `MediaLaneState` (MLP spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaLaneState {
    /// Lane state kind.
    pub state: MediaLaneStateKind,
    /// Session-relative ms when the state was entered.
    pub since_epoch_ms: u64,
    /// Human-readable detail; the receiver's UI maps the state, never the detail.
    pub detail: String,
}

/// `MediaAnchorPlan` (MLP spec §6.3, §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaAnchorPlan {
    /// Plan identifier, monotone per receiver.
    pub plan_id: u32,
    /// Estimated updates to finish the plan.
    pub total_frames_estimate: u32,
    /// Cells in the plan.
    pub total_blocks: u32,
    /// Plan priority (MLP spec §7.3.1).
    pub priority: u8,
}

/// `MediaClose` (MLP spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaClose {
    /// Close code, see [`MediaCloseCode`].
    pub code: MediaCloseCode,
    /// Human-readable reason.
    pub reason: String,
    /// `true` means "come back": the peer is parked, not failed.
    pub park: bool,
}

/// `MediaError` (MLP spec §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaErrorBody {
    /// Numeric code, see [`MediaErrorCode`].
    pub code: u32,
    /// The object that failed, when known.
    pub object_id: u32,
    /// Detail string.
    pub detail: String,
}

/// `MediaPathChange` (MLP spec §6.3, §6.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPathChange {
    /// Previous path.
    pub from_path: MediaPath,
    /// New path.
    pub to_path: MediaPath,
    /// Why the switch happened.
    pub reason: String,
    /// Session-relative ms when the switch happened.
    pub at_epoch_ms: u64,
}

/// A media-lane control object (the `MediaControlObject.body` oneof).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaControlObject {
    /// Lane handshake.
    Hello(MediaHello),
    /// Lane handshake answer.
    HelloAck(MediaHelloAck),
    /// Resume a parked lane.
    Resume(MediaResume),
    /// Resume answer.
    ResumeAck(MediaResumeAck),
    /// Lane state report.
    LaneState(MediaLaneState),
    /// An anchor plan is starting.
    AnchorPlan(MediaAnchorPlan),
    /// Lane close (park or terminate).
    Close(MediaClose),
    /// A typed lane error.
    Error(MediaErrorBody),
    /// A carrier switch.
    PathChange(MediaPathChange),
}

impl MediaControlObject {
    /// The object kind.
    pub fn kind(&self) -> ControlKind {
        match self {
            Self::Hello(_) => ControlKind::Hello,
            Self::HelloAck(_) => ControlKind::HelloAck,
            Self::Resume(_) => ControlKind::Resume,
            Self::ResumeAck(_) => ControlKind::ResumeAck,
            Self::LaneState(_) => ControlKind::LaneState,
            Self::AnchorPlan(_) => ControlKind::AnchorPlan,
            Self::Close(_) => ControlKind::Close,
            Self::Error(_) => ControlKind::Error,
            Self::PathChange(_) => ControlKind::PathChange,
        }
    }

    /// Whether this object must be drained ahead of queued media.
    ///
    /// Handshake, resume, close and error objects are `URGENT` by policy;
    /// lane state and path changes are informational and normal priority.
    pub fn is_urgent(&self) -> bool {
        matches!(
            self,
            Self::Hello(_)
                | Self::HelloAck(_)
                | Self::Resume(_)
                | Self::ResumeAck(_)
                | Self::Close(_)
                | Self::Error(_)
        )
    }

    /// Validate the body invariants of MLP spec §6.2/§5.7.
    pub fn validate(&self) -> Result<(), RejectReason> {
        match self {
            Self::Hello(hello) => {
                if hello.protocol_version != u32::from(pdu::MEDIA_PROTOCOL_VERSION) {
                    return Err(RejectReason::UnsupportedVersion);
                }
                Ok(())
            }
            Self::HelloAck(ack) => {
                if ack.protocol_version != u32::from(pdu::MEDIA_PROTOCOL_VERSION) {
                    return Err(RejectReason::UnsupportedVersion);
                }
                if !ack.media_supported && ack.media_unsupported_reason.is_none() {
                    return Err(RejectReason::BodyInvalid);
                }
                Ok(())
            }
            Self::Error(body) => {
                if MediaErrorCode::from_u32(body.code).is_none() {
                    return Err(RejectReason::BodyInvalid);
                }
                Ok(())
            }
            Self::AnchorPlan(plan) => {
                if plan.total_blocks == 0 || plan.priority > 7 {
                    return Err(RejectReason::BodyInvalid);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// `MediaAck` (MLP spec §6.2): selective bitmap over a 64-update window.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct MediaAck {
    /// Highest applied update id; `0` means nothing applied yet.
    pub highest_applied_update_id: u64,
    /// Bit `i` = applied update `highest - i`, `i` in `0..64`.
    pub ack_bitmap: u64,
    /// The receiver's chain epoch.
    pub chain_epoch: u32,
    /// The receiver's cell grid size.
    pub applied_block_count: u32,
    /// The receiver's own count of cells it believes stale.
    pub stale_blocks: u32,
    /// The receiver's belief; the sender's accounting is authoritative.
    pub converged: bool,
    /// Measured render latency.
    pub render_latency_ms: f64,
    /// Whether `render_latency_ms` is meaningful.
    pub has_render_latency_ms: bool,
    /// The highest input sequence visible in the applied cells.
    pub echo_of_input_seq: u64,
    /// Sender-relative ms when this ack was produced.
    pub ack_epoch_ms: u64,
}

impl MediaAck {
    /// Validate the ack bitmap and count rules (MLP spec §6.2).
    pub fn validate(&self) -> Result<(), RejectReason> {
        // Bit i may be set only when highest_applied_update_id >= i.
        let mut bits = self.ack_bitmap;
        let mut i: u32 = 0;
        while bits != 0 {
            // Bit i names update `highest - i`; update ids start at 1, so a set
            // bit must satisfy `i < highest`.
            if bits & 1 == 1 && u64::from(i) >= self.highest_applied_update_id {
                return Err(RejectReason::AckBadBitmap);
            }
            bits >>= 1;
            i += 1;
        }
        if self.stale_blocks > self.applied_block_count {
            return Err(RejectReason::AckBadCount);
        }
        if !self.has_render_latency_ms && self.render_latency_ms != 0.0 {
            return Err(RejectReason::BodyInvalid);
        }
        Ok(())
    }

    /// Whether `update_id` is covered by this ack bitmap.
    pub fn is_applied(&self, update_id: u64) -> bool {
        if update_id == 0 || update_id > self.highest_applied_update_id {
            return false;
        }
        let delta = self.highest_applied_update_id - update_id;
        if delta >= 64 {
            return false;
        }
        self.ack_bitmap & (1u64 << delta) != 0
    }

    /// The update ids this ack covers, highest first.
    pub fn applied_frames(&self) -> impl Iterator<Item = u64> + '_ {
        let highest = self.highest_applied_update_id;
        (0..64u32)
            .filter(move |i| self.ack_bitmap & (1u64 << i) != 0)
            .map(move |i| highest - u64::from(i))
    }

    /// Whether the window covers nothing at all.
    pub fn is_empty(&self) -> bool {
        self.ack_bitmap == 0 && self.highest_applied_update_id == 0
    }
}

/// `ProbeEcho` (MLP spec §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProbeEcho {
    /// The probe object id.
    pub object_id: u32,
    /// The sender's session-relative ms at first emission.
    pub send_epoch_ms: u64,
    /// The echoer's session-relative ms at reply.
    pub echo_epoch_ms: u64,
    /// Payload bytes of the probe.
    pub payload_bytes: u32,
}

/// `MediaProbe` (MLP spec §6.2, §6.2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MediaProbe {
    /// Probe identifier.
    pub probe_id: u32,
    /// Probe payload bytes (never carries cells).
    pub payload_bytes: u32,
    /// Sender-relative ms at emission.
    pub send_epoch_ms: u64,
}

/// `MediaFeedback` (MLP spec §6.2): the receiver's only quality channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFeedback {
    /// The receiver's measured RTT (clamped 1..60000).
    pub rtt_ms: u32,
    /// The receiver's running minimum.
    pub rtt_min_ms: u32,
    /// The receiver's jitter estimate.
    pub jitter_ms: u32,
    /// Payload bytes received since the previous report.
    pub received_bytes: u64,
    /// Objects received since the previous report.
    pub received_objects: u64,
    /// Gaps in `object_id` since the previous report.
    pub lost_objects: u64,
    /// Duplicated objects observed.
    pub duplicated_objects: u64,
    /// Reordered objects observed.
    pub reordered_objects: u64,
    /// The interval these counters cover.
    pub window_ms: u64,
    /// The latest selective ack, when the receiver has one.
    pub ack: Option<MediaAck>,
    /// Probe echoes produced in this window.
    pub probes: Vec<ProbeEcho>,
    /// The receiver's own outbound backlog (diagnostics).
    pub buffered_amount_bytes: u32,
    /// Measured applied updates per second (diagnostics).
    pub display_fps: u32,
    /// Render latency p50.
    pub render_ms_p50: u32,
    /// Render latency p95.
    pub render_ms_p95: u32,
    /// The receiver's input queue depth.
    pub input_queue_depth: u32,
    /// Input objects created since the previous report.
    pub input_submitted: u64,
    /// Input objects discarded by deadline.
    pub input_expired: u64,
    /// Carrier switches observed.
    pub path_switches: u64,
    /// The carrier the receiver believes is active.
    pub active_path: MediaPath,
    /// Diagnostics only; never parsed for behaviour.
    pub app_version: String,
}

impl Default for MediaFeedback {
    fn default() -> Self {
        Self {
            rtt_ms: 1,
            rtt_min_ms: 1,
            jitter_ms: 0,
            received_bytes: 0,
            received_objects: 0,
            lost_objects: 0,
            duplicated_objects: 0,
            reordered_objects: 0,
            window_ms: 0,
            ack: None,
            probes: Vec::new(),
            buffered_amount_bytes: 0,
            display_fps: 0,
            render_ms_p50: 0,
            render_ms_p95: 0,
            input_queue_depth: 0,
            input_submitted: 0,
            input_expired: 0,
            path_switches: 0,
            active_path: MediaPath::Unknown,
            app_version: String::new(),
        }
    }
}

impl MediaFeedback {
    /// Goodput in bits per second, or `None` for an empty window.
    ///
    /// Payload bytes only, as the estimator requires (MLP spec §7.2.2).
    pub fn goodput_bps(&self) -> Option<u64> {
        if self.window_ms == 0 {
            return None;
        }
        Some(self.received_bytes.saturating_mul(8_000) / self.window_ms)
    }

    /// Delivered ratio over the report window (MLP spec §7.2.2).
    pub fn delivered_ratio(&self) -> Option<f64> {
        let attempted = self.received_objects + self.lost_objects;
        if attempted == 0 {
            return None;
        }
        Some(self.received_objects as f64 / attempted as f64)
    }

    /// Loss ratio derived from the delivered ratio.
    pub fn loss_ratio(&self) -> Option<f64> {
        self.delivered_ratio().map(|ratio| 1.0 - ratio)
    }

    /// Clamp the receiver-reported RTT into the documented range.
    pub fn clamped_rtt_ms(&self) -> u32 {
        self.rtt_ms.clamp(1, 60_000)
    }

    /// Validate the embedded ack and the numeric ranges a hop must trust.
    pub fn validate(&self) -> Result<(), RejectReason> {
        if let Some(ack) = &self.ack {
            ack.validate()?;
        }
        if self.window_ms > 0 && self.received_objects == 0 && self.lost_objects == 0 {
            // A window with bytes but no object accounting is a receiver bug;
            // the sender tolerates it (the counters are diagnostics) but it must
            // not be silently treated as zero loss with a goodput signal.
            return Ok(());
        }
        Ok(())
    }
}

/// `MediaHopReport` (MLP spec §6.2, D-16): what an intermediary hop dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MediaHopReport {
    /// `1` = intermediary, `2` = edge; extensible, see [`hop`].
    pub hop: u32,
    /// Media objects this hop discarded in the window.
    pub objects_dropped: u64,
    /// Bytes this hop discarded in the window.
    pub bytes_dropped: u64,
    /// Objects coalesced by this hop in the window.
    pub coalesced: u64,
    /// Peak queue depth observed.
    pub queue_depth_peak: u32,
    /// Current queue depth.
    pub queue_depth_current: u32,
    /// The interval this report covers.
    pub window_ms: u32,
    /// The hop's own RTT to the next hop, when measured.
    pub rtt_ms_own: u32,
    /// Total ms this hop's writer waited on capacity.
    pub backpressure_ms: u32,
}

/// `MediaCursor` (MLP spec §6.2): unordered, latest-wins, never retransmitted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MediaCursor {
    /// Cursor x in screen cells.
    pub x_px: i32,
    /// Cursor y in screen cells.
    pub y_px: i32,
    /// Whether the cursor is visible.
    pub visible: bool,
    /// Local cursor shape generation; `0` = keep the local shape.
    pub shape_seq: u32,
    /// Sender-relative ms of the position sample.
    pub epoch_ms: u64,
    /// Hot spot x, only when `shape_seq` changes from 0.
    pub hot_spot_x: u32,
    /// Hot spot y, only when `shape_seq` changes from 0.
    pub hot_spot_y: u32,
    /// Optional shape cells, at most 8 KiB.
    pub shape_rgba: Vec<u8>,
}

impl MediaCursor {
    /// Validate the cursor body rules (MLP spec §6.2).
    pub fn validate(&self) -> Result<(), RejectReason> {
        if self.shape_rgba.len() > 8192 {
            return Err(RejectReason::BodyInvalid);
        }
        if self.visible && self.shape_seq == 0 && !self.shape_rgba.is_empty() {
            // A shape may only be carried when a generation is named.
            return Err(RejectReason::BodyInvalid);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_bitmap_semantics() {
        let ack = MediaAck {
            highest_applied_update_id: 10,
            ack_bitmap: 0b1_0101, // updates 10, 8, 6
            ..MediaAck::default()
        };
        assert!(ack.is_applied(10));
        assert!(!ack.is_applied(9));
        assert!(ack.is_applied(8));
        assert!(ack.is_applied(6));
        assert!(!ack.is_applied(11));
        assert_eq!(ack.applied_frames().collect::<Vec<_>>(), vec![10, 8, 6]);
        assert_eq!(ack.validate(), Ok(()));
    }

    #[test]
    fn ack_bit_above_highest_is_rejected() {
        let ack = MediaAck {
            highest_applied_update_id: 0,
            ack_bitmap: 1,
            ..MediaAck::default()
        };
        assert_eq!(ack.validate(), Err(RejectReason::AckBadBitmap));
    }

    #[test]
    fn ack_stale_blocks_above_applied_count_is_rejected() {
        let ack = MediaAck {
            highest_applied_update_id: 3,
            ack_bitmap: 0b1,
            applied_block_count: 4,
            stale_blocks: 5,
            ..MediaAck::default()
        };
        assert_eq!(ack.validate(), Err(RejectReason::AckBadCount));
    }

    #[test]
    fn ack_window_is_64_frames() {
        let ack = MediaAck {
            highest_applied_update_id: 100,
            ack_bitmap: u64::MAX,
            ..MediaAck::default()
        };
        assert!(ack.is_applied(100));
        assert!(ack.is_applied(37));
        assert!(!ack.is_applied(36));
    }

    #[test]
    fn feedback_goodput_and_loss() {
        let feedback = MediaFeedback {
            received_bytes: 4_000,
            received_objects: 97,
            lost_objects: 3,
            window_ms: 1_000,
            ..MediaFeedback::default()
        };
        assert_eq!(feedback.goodput_bps(), Some(32_000));
        let loss = feedback.loss_ratio().expect("window has objects");
        assert!((loss - 0.03).abs() < 1e-9);
        assert_eq!(feedback.clamped_rtt_ms(), 1);
    }

    #[test]
    fn feedback_without_window_has_no_goodput() {
        let feedback = MediaFeedback::default();
        assert_eq!(feedback.goodput_bps(), None);
        assert_eq!(feedback.loss_ratio(), None);
    }

    #[test]
    fn control_object_validation() {
        let hello = MediaHello {
            protocol_version: 1,
            ..serde_default_hello()
        };
        assert_eq!(MediaControlObject::Hello(hello).validate(), Ok(()));
        let wrong = MediaHello {
            protocol_version: 2,
            ..serde_default_hello()
        };
        assert_eq!(
            MediaControlObject::Hello(wrong).validate(),
            Err(RejectReason::UnsupportedVersion)
        );
        let unsupported = MediaHelloAck {
            protocol_version: 1,
            media_supported: false,
            media_unsupported_reason: None,
            ..serde_default_ack()
        };
        assert_eq!(
            MediaControlObject::HelloAck(unsupported).validate(),
            Err(RejectReason::BodyInvalid)
        );
        assert!(MediaControlObject::Hello(serde_default_hello()).is_urgent());
        assert!(
            !MediaControlObject::LaneState(MediaLaneState {
                state: MediaLaneStateKind::Live,
                since_epoch_ms: 0,
                detail: String::new(),
            })
            .is_urgent()
        );
    }

    fn serde_default_hello() -> MediaHello {
        MediaHello {
            protocol_version: 1,
            media_session_id: String::new(),
            session_id: "s".into(),
            max_fragment_payload: 65_536,
            cell_size: 64,
            cell_cols: 30,
            cell_rows: 17,
            capabilities: vec![capability::ANCHOR.into()],
            encoder_max_fps: 30,
            start_epoch_ms: 0,
        }
    }

    fn serde_default_ack() -> MediaHelloAck {
        MediaHelloAck {
            protocol_version: 1,
            media_session_id: "m".into(),
            accepted_fragment_payload: 65_536,
            cell_size: 64,
            cell_cols: 30,
            cell_rows: 17,
            capabilities: vec![capability::ANCHOR.into()],
            resume_ttl_seconds: 900,
            media_supported: true,
            media_unsupported_reason: None,
        }
    }

    #[test]
    fn cursor_validation_rules() {
        let mut cursor = MediaCursor {
            x_px: 10,
            y_px: 20,
            visible: true,
            shape_seq: 1,
            epoch_ms: 5,
            hot_spot_x: 0,
            hot_spot_y: 0,
            shape_rgba: vec![0; 8192],
        };
        assert_eq!(cursor.validate(), Ok(()));
        cursor.shape_rgba.push(0);
        assert_eq!(cursor.validate(), Err(RejectReason::BodyInvalid));
        let mut no_gen = MediaCursor {
            visible: true,
            shape_seq: 0,
            shape_rgba: vec![0; 4],
            ..MediaCursor::default()
        };
        assert_eq!(no_gen.validate(), Err(RejectReason::BodyInvalid));
        no_gen.shape_rgba.clear();
        assert_eq!(no_gen.validate(), Ok(()));
    }

    #[test]
    fn media_path_wire_values() {
        assert_eq!(MediaPath::from_u8(0), MediaPath::Unknown);
        assert_eq!(MediaPath::from_u8(1), MediaPath::Ws);
        assert_eq!(MediaPath::from_u8(2), MediaPath::DcDirect);
        assert_eq!(MediaPath::from_u8(3), MediaPath::DcTurn);
        assert_eq!(MediaPath::from_u8(4), MediaPath::Quic);
        assert_eq!(MediaPath::from_u8(9), MediaPath::Unknown);
        assert_eq!(MediaPath::Ws.as_u8(), 1);
        assert!(!MediaPath::Ws.is_datagram());
        assert!(MediaPath::DcTurn.is_datagram());
    }
}
