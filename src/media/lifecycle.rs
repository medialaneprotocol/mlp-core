//! Lane lifecycle, honest states and the capture/encode policy (MLP spec §8,
//! §7, §8, §10).
//!
//! Two things live here: the authoritative lane state the sender reports
//! (`MediaLaneState.state`, values 0..9) and the policy decisions a driver
//! takes from it — whether to capture, whether to speculate, and what a kill
//! switch means. Every value is data; nothing reads the environment.

use serde::{Deserialize, Serialize};

use super::MediaPath;

/// The sender's authoritative lane state (MLP spec §7.3).
///
/// The receiver's UI maps these values to the honest strings of §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum MediaLaneStateKind {
    /// Waiting for `MediaHello`/`MediaHelloAck`.
    Starting = 0,
    /// Healthy: converged or converging quickly.
    Live = 1,
    /// Delivering, but below the link's nominal capability.
    Degraded = 2,
    /// Draining a plan or a large pending set.
    Converging = 3,
    /// No applied update for a long time.
    Stale = 4,
    /// Parked: the peer is away, the state is held.
    Parked = 5,
    /// Media is switched off by policy or by the kill switch.
    Disabled = 6,
    /// The receiver is decode-bound; the sender clamped `target_fps`.
    DecodeBound = 7,
    /// No feedback from the receiver (MLP spec §8).
    NoFeedback = 8,
    /// The peer does not speak MLP v1 (MLP spec annex D, D-4).
    MediaUnsupported = 9,
}

impl MediaLaneStateKind {
    /// Decode the wire value; unknown values are `None`.
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Starting),
            1 => Some(Self::Live),
            2 => Some(Self::Degraded),
            3 => Some(Self::Converging),
            4 => Some(Self::Stale),
            5 => Some(Self::Parked),
            6 => Some(Self::Disabled),
            7 => Some(Self::DecodeBound),
            8 => Some(Self::NoFeedback),
            9 => Some(Self::MediaUnsupported),
            _ => None,
        }
    }

    /// The wire value.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// The stable metric label spelling (`mlp_session_state{state}`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Live => "live",
            Self::Degraded => "degraded",
            Self::Converging => "converging",
            Self::Stale => "stale",
            Self::Parked => "parked",
            Self::Disabled => "disabled",
            Self::DecodeBound => "decode_bound",
            Self::NoFeedback => "no_feedback",
            Self::MediaUnsupported => "media_unsupported",
        }
    }

    /// Whether media objects may be emitted in this state.
    pub const fn emits_media(self) -> bool {
        matches!(
            self,
            Self::Live | Self::Degraded | Self::Converging | Self::Stale | Self::DecodeBound
        )
    }

    /// Whether the state carries a receiver-visible quality claim.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::MediaUnsupported | Self::Disabled)
    }
}

/// `MediaClose.code` (MLP spec §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum MediaCloseCode {
    /// A normal close.
    Normal = 0,
    /// The server is shutting down.
    ServerShutdown = 1,
    /// Policy closed the lane.
    Policy = 2,
    /// A newer session superseded this one.
    Superseded = 3,
    /// Park: `park=true` is the "come back" signal.
    Park = 4,
}

impl MediaCloseCode {
    /// Decode the wire value.
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Normal),
            1 => Some(Self::ServerShutdown),
            2 => Some(Self::Policy),
            3 => Some(Self::Superseded),
            4 => Some(Self::Park),
            _ => None,
        }
    }

    /// The wire value.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::ServerShutdown => "server_shutdown",
            Self::Policy => "policy",
            Self::Superseded => "superseded",
            Self::Park => "park",
        }
    }
}

/// Whether both lanes are physically separate (MLP spec annex D, D-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LaneMode {
    /// Two connections: control and media are isolated.
    Dual,
    /// One connection: priority and reserve still apply, HOL isolation does not.
    Single,
}

impl LaneMode {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dual => "dual",
            Self::Single => "single",
        }
    }
}

/// Typed reason a peer cannot carry the media lane (MLP spec §7.3, D-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MediaUnsupportedReason {
    /// The peer speaks an incompatible protocol version.
    Protocol,
    /// Deployment policy disabled the lane.
    DisabledByPolicy,
    /// The sender cannot serve another receiver.
    NoSenderCapability,
}

impl MediaUnsupportedReason {
    /// The wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::DisabledByPolicy => "disabled_by_policy",
            Self::NoSenderCapability => "no_sender_capability",
        }
    }

    /// Parse the wire string.
    ///
    /// Named `parse` rather than `from_str` so it is not mistaken for
    /// `std::str::FromStr::from_str`, which returns a `Result`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "protocol" => Some(Self::Protocol),
            "disabled_by_policy" => Some(Self::DisabledByPolicy),
            "no_sender_capability" => Some(Self::NoSenderCapability),
            _ => None,
        }
    }
}

/// Evidence the lifecycle consumes each tick.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LaneEvidence {
    /// Session-relative ms now.
    pub now_ms: u64,
    /// Whether the lane completed its handshake.
    pub handshaken: bool,
    /// Whether the peer reported `media_supported=false`.
    pub unsupported: Option<MediaUnsupportedReason>,
    /// Whether media is disabled by a kill switch.
    pub disabled: bool,
    /// Whether the receiver is parked.
    pub parked: bool,
    /// Whether accounting says the receiver is converged.
    pub converged: bool,
    /// The pending cell fraction (`0.0..=1.0`).
    pub pending_fraction: f64,
    /// Session-relative ms of the last ack, when any.
    pub last_ack_ms: Option<u64>,
    /// Whether the receiver reported decode-bound feedback.
    pub decode_bound: bool,
    /// Whether the estimator is in `NoFeedback`.
    pub no_feedback: bool,
    /// Pace below which the link counts as degraded (the pace floor × 8).
    pub degraded_below_bps: u64,
    /// The current pace.
    pub pace_bps: u64,
}

impl Default for LaneEvidence {
    fn default() -> Self {
        Self {
            now_ms: 0,
            handshaken: false,
            unsupported: None,
            disabled: false,
            parked: false,
            converged: false,
            pending_fraction: 0.0,
            last_ack_ms: None,
            decode_bound: false,
            no_feedback: false,
            degraded_below_bps: 64_000,
            pace_bps: 1_000_000,
        }
    }
}

/// Staleness above which the lane is reported `Stale` (in units of
/// `report_interval + 1 s`).
pub const STALE_ACK_WINDOWS: u64 = 3;

/// The lane lifecycle state machine.
#[derive(Debug, Clone)]
pub struct LaneLifecycle {
    state: MediaLaneStateKind,
    since_ms: u64,
    detail: String,
    last_reported: MediaLaneStateKind,
    transitions: u64,
    path: MediaPath,
}

impl Default for LaneLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneLifecycle {
    /// A lane that has not handshaken yet.
    pub fn new() -> Self {
        Self {
            state: MediaLaneStateKind::Starting,
            since_ms: 0,
            detail: String::from("starting"),
            last_reported: MediaLaneStateKind::Starting,
            transitions: 0,
            path: MediaPath::Unknown,
        }
    }

    /// The current state.
    pub fn state(&self) -> MediaLaneStateKind {
        self.state
    }

    /// Session-relative ms when the state was entered.
    pub fn since_ms(&self) -> u64 {
        self.since_ms
    }

    /// The detail string carried by `MediaLaneState`.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// How many transitions have happened.
    pub fn transitions(&self) -> u64 {
        self.transitions
    }

    /// The carrier the lane believes is active.
    pub fn path(&self) -> MediaPath {
        self.path
    }

    /// Note a carrier switch.
    pub fn set_path(&mut self, path: MediaPath) {
        self.path = path;
    }

    /// Force a state (kill switch, park, close).
    pub fn force(&mut self, state: MediaLaneStateKind, detail: impl Into<String>, now_ms: u64) {
        let detail = detail.into();
        if state != self.state {
            self.transitions += 1;
        }
        self.state = state;
        self.detail = detail;
        self.since_ms = now_ms;
    }

    /// Consume evidence and return the new state when it changed.
    ///
    /// The state vocabulary is the honest degradation ladder of MLP spec §8:
    /// `Disabled`/`MediaUnsupported` are terminal, `Parked` is a deliberate
    /// hold, and the live states differ only in what they claim.
    pub fn update(&mut self, evidence: &LaneEvidence) -> Option<MediaLaneStateKind> {
        let next = if let Some(reason) = evidence.unsupported {
            Some((
                MediaLaneStateKind::MediaUnsupported,
                reason.as_str().to_string(),
            ))
        } else if evidence.disabled {
            Some((MediaLaneStateKind::Disabled, "disabled_by_policy".into()))
        } else if !evidence.handshaken {
            Some((MediaLaneStateKind::Starting, "starting".into()))
        } else if evidence.parked {
            Some((MediaLaneStateKind::Parked, "parked".into()))
        } else if evidence.no_feedback {
            Some((MediaLaneStateKind::NoFeedback, "no_feedback".into()))
        } else if Self::is_stale(evidence) {
            Some((MediaLaneStateKind::Stale, "no_ack_progress".into()))
        } else if evidence.decode_bound {
            Some((
                MediaLaneStateKind::DecodeBound,
                "viewer_decode_bound".into(),
            ))
        } else if !evidence.converged && evidence.pending_fraction > 0.25 {
            Some((
                MediaLaneStateKind::Converging,
                format!(
                    "converging:{}%",
                    (evidence.pending_fraction * 100.0).round() as u32
                ),
            ))
        } else if evidence.pace_bps < evidence.degraded_below_bps {
            Some((MediaLaneStateKind::Degraded, "below_nominal".into()))
        } else {
            Some((MediaLaneStateKind::Live, "live".into()))
        };
        let (next, detail) = next?;
        if next == self.state {
            return None;
        }
        self.transitions += 1;
        self.state = next;
        self.detail = detail;
        self.since_ms = evidence.now_ms;
        self.last_reported = next;
        Some(next)
    }

    /// The last state that was reported to the peer.
    pub fn last_reported(&self) -> MediaLaneStateKind {
        self.last_reported
    }

    fn is_stale(evidence: &LaneEvidence) -> bool {
        match evidence.last_ack_ms {
            // The staleness budget is the same shape as the capture stall
            // window: 3 ack windows + 250 ms, expressed in ms here.
            Some(last) => evidence.now_ms.saturating_sub(last) > 3_000,
            None => evidence.now_ms > 3_000,
        }
    }

    /// The `MediaLaneState` body for the current state.
    pub fn body(&self) -> super::MediaLaneState {
        super::MediaLaneState {
            state: self.state,
            since_epoch_ms: self.since_ms,
            detail: self.detail.clone(),
        }
    }
}

/// What the capture/encoder loop must do right now (MLP spec §9.5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CaptureDecision {
    /// Whether to capture at all.
    pub capture: bool,
    /// The minimum updates per second to emit while capturing.
    pub min_fps: f64,
    /// Whether speculative (P3) refresh is allowed.
    pub speculative_refresh: bool,
    /// Whether only P0 work is allowed (the floor case).
    pub p0_only: bool,
    /// The detail string for the lane state, when the decision narrows work.
    pub detail: Option<&'static str>,
}

/// The capture policy of MLP spec §9.5, expressed as a pure decision.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CapturePolicy {
    /// the park-hold switch: capture continues while a receiver is parked.
    pub park_hold: bool,
    /// `CAPTURE_MIN_FPS`.
    pub min_fps: f64,
    /// `loss > 5 %` counts as a known-degraded link.
    pub degraded_loss_ratio: f64,
    /// `pace < 64 kbps` counts as a known-degraded link.
    pub degraded_pace_bps: u64,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            park_hold: true,
            min_fps: 1.0,
            degraded_loss_ratio: 0.05,
            degraded_pace_bps: 64_000,
        }
    }
}

impl CapturePolicy {
    /// Decide the capture/encode posture.
    ///
    /// `live_viewers` counts receivers that are connected (parked ones are
    /// covered by `parked_viewers`); `park_expired` is true once the park
    /// window has closed for every parked receiver.
    pub fn decide(
        &self,
        live_viewers: u32,
        parked_viewers: u32,
        park_expired: bool,
        loss_ratio: f64,
        pace_bps: u64,
        pending_fraction: f64,
    ) -> CaptureDecision {
        if live_viewers == 0 && parked_viewers == 0 {
            return CaptureDecision {
                capture: false,
                min_fps: 0.0,
                speculative_refresh: false,
                p0_only: false,
                detail: Some("no_viewer"),
            };
        }
        if live_viewers == 0 {
            // Parked receivers still need the journal and pending growth.
            if park_expired && !self.park_hold {
                return CaptureDecision {
                    capture: false,
                    min_fps: 0.0,
                    speculative_refresh: false,
                    p0_only: false,
                    detail: Some("park_expired"),
                };
            }
            return CaptureDecision {
                capture: true,
                min_fps: self.min_fps,
                speculative_refresh: false,
                p0_only: false,
                detail: Some("parked"),
            };
        }
        let degraded = loss_ratio > self.degraded_loss_ratio || pace_bps < self.degraded_pace_bps;
        if degraded {
            // A stalled link never pauses capture (MLP spec annex D, D-9); it narrows it.
            return CaptureDecision {
                capture: true,
                min_fps: self.min_fps,
                speculative_refresh: false,
                p0_only: pending_fraction > 0.5,
                detail: if pending_fraction > 0.5 {
                    Some("floor")
                } else {
                    Some("degraded")
                },
            };
        }
        CaptureDecision {
            capture: true,
            min_fps: self.min_fps,
            speculative_refresh: true,
            p0_only: false,
            detail: None,
        }
    }
}

/// Deployment policy values the drivers obtain from configuration (MLP spec
/// §10). This struct carries the *decisions*, not the environment reads.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MediaLanePolicy {
    /// the lane-enable switch.
    pub media_lane_enabled: bool,
    /// the stream-emission switch.
    pub relay_stream_enabled: bool,
    /// the patch-mode switch: strict codec chaining when false.
    pub patch_mode: bool,
    /// the park-hold switch.
    pub park_hold: bool,
    /// the input deadline.
    pub input_deadline_ms: u64,
    /// the resume TTL.
    pub resume_ttl_seconds: u64,
    /// the pace floor.
    pub floor_bps: u64,
    /// the bandwidth ceiling.
    pub ceiling_bps: u64,
    /// the path-selection switch.
    pub path_selection: bool,
}

impl Default for MediaLanePolicy {
    fn default() -> Self {
        Self {
            media_lane_enabled: true,
            relay_stream_enabled: true,
            patch_mode: true,
            park_hold: true,
            input_deadline_ms: 5_000,
            resume_ttl_seconds: 900,
            floor_bps: 8_000,
            ceiling_bps: 50_000_000,
            path_selection: true,
        }
    }
}

impl MediaLanePolicy {
    /// Whether media may be emitted at all under this policy.
    pub fn emits_media(&self) -> bool {
        self.media_lane_enabled && self.relay_stream_enabled
    }

    /// Whether anchors may be applied onto any held state.
    ///
    /// With strict-chaining mode the sender keeps strict chaining and only
    /// uses anchors on an epoch change (MLP spec §10).
    pub fn anchors_on_any_state(&self) -> bool {
        self.patch_mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_state_wire_values_match_the_design() {
        for value in 0..=9u32 {
            let state = MediaLaneStateKind::from_u32(value).expect("known");
            assert_eq!(state.as_u32(), value);
        }
        assert_eq!(MediaLaneStateKind::from_u32(10), None);
        assert_eq!(MediaLaneStateKind::Live.as_u32(), 1);
        assert_eq!(MediaLaneStateKind::MediaUnsupported.as_u32(), 9);
    }

    #[test]
    fn close_codes_match_the_design() {
        for value in 0..=4u32 {
            assert_eq!(
                MediaCloseCode::from_u32(value).map(|c| c.as_u32()),
                Some(value)
            );
        }
        assert_eq!(MediaCloseCode::from_u32(5), None);
    }

    #[test]
    fn unsupported_reasons_round_trip() {
        for reason in [
            MediaUnsupportedReason::Protocol,
            MediaUnsupportedReason::DisabledByPolicy,
            MediaUnsupportedReason::NoSenderCapability,
        ] {
            assert_eq!(MediaUnsupportedReason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(MediaUnsupportedReason::parse("nonsense"), None);
    }

    #[test]
    fn lifecycle_starts_and_reaches_live() {
        let mut lane = LaneLifecycle::new();
        assert_eq!(lane.state(), MediaLaneStateKind::Starting);
        let evidence = LaneEvidence {
            now_ms: 100,
            handshaken: true,
            converged: true,
            pace_bps: 10_000_000,
            ..LaneEvidence::default()
        };
        assert_eq!(lane.update(&evidence), Some(MediaLaneStateKind::Live));
        assert_eq!(lane.since_ms(), 100);
        assert_eq!(lane.transitions(), 1);
        assert_eq!(lane.body().state, MediaLaneStateKind::Live);
        // No change, no transition.
        assert_eq!(lane.update(&evidence), None);
    }

    #[test]
    fn lifecycle_prefers_terminal_states() {
        let mut lane = LaneLifecycle::new();
        let evidence = LaneEvidence {
            handshaken: true,
            unsupported: Some(MediaUnsupportedReason::Protocol),
            ..LaneEvidence::default()
        };
        assert_eq!(
            lane.update(&evidence),
            Some(MediaLaneStateKind::MediaUnsupported)
        );
        assert!(lane.state().is_terminal());
        let disabled = LaneEvidence {
            handshaken: true,
            disabled: true,
            ..LaneEvidence::default()
        };
        assert_eq!(lane.update(&disabled), Some(MediaLaneStateKind::Disabled));
    }

    #[test]
    fn converging_then_live_as_pending_drains() {
        let mut lane = LaneLifecycle::new();
        let converging = LaneEvidence {
            handshaken: true,
            converged: false,
            pending_fraction: 0.8,
            pace_bps: 10_000_000,
            ..LaneEvidence::default()
        };
        assert_eq!(
            lane.update(&converging),
            Some(MediaLaneStateKind::Converging)
        );
        assert!(lane.detail().starts_with("converging:80"));
        let live = LaneEvidence {
            converged: true,
            ..converging
        };
        assert_eq!(lane.update(&live), Some(MediaLaneStateKind::Live));
    }

    #[test]
    fn stale_after_ack_silence() {
        let mut lane = LaneLifecycle::new();
        let evidence = LaneEvidence {
            now_ms: 10_000,
            handshaken: true,
            converged: true,
            last_ack_ms: Some(0),
            pace_bps: 10_000_000,
            ..LaneEvidence::default()
        };
        assert_eq!(lane.update(&evidence), Some(MediaLaneStateKind::Stale));
    }

    #[test]
    fn decode_bound_and_no_feedback_are_distinct_states() {
        let mut lane = LaneLifecycle::new();
        let decode = LaneEvidence {
            handshaken: true,
            converged: true,
            decode_bound: true,
            pace_bps: 10_000_000,
            ..LaneEvidence::default()
        };
        assert_eq!(lane.update(&decode), Some(MediaLaneStateKind::DecodeBound));
        let no_feedback = LaneEvidence {
            no_feedback: true,
            decode_bound: false,
            ..decode
        };
        assert_eq!(
            lane.update(&no_feedback),
            Some(MediaLaneStateKind::NoFeedback)
        );
    }

    #[test]
    fn parked_beats_degraded() {
        let mut lane = LaneLifecycle::new();
        let parked = LaneEvidence {
            handshaken: true,
            parked: true,
            pace_bps: 10_000_000,
            ..LaneEvidence::default()
        };
        assert_eq!(lane.update(&parked), Some(MediaLaneStateKind::Parked));
        assert!(
            !lane.state().emits_media(),
            "a parked lane emits no updates"
        );
    }

    #[test]
    fn slow_pace_without_convergence_is_degraded_not_stale() {
        let mut lane = LaneLifecycle::new();
        let evidence = LaneEvidence {
            now_ms: 500,
            handshaken: true,
            converged: true,
            last_ack_ms: Some(400),
            pace_bps: 20_000,
            degraded_below_bps: 64_000,
            ..LaneEvidence::default()
        };
        assert_eq!(lane.update(&evidence), Some(MediaLaneStateKind::Degraded));
    }

    #[test]
    fn force_sets_state_and_detail() {
        let mut lane = LaneLifecycle::new();
        lane.force(MediaLaneStateKind::Disabled, "kill_switch", 42);
        assert_eq!(lane.state(), MediaLaneStateKind::Disabled);
        assert_eq!(lane.detail(), "kill_switch");
        assert_eq!(lane.since_ms(), 42);
    }

    #[test]
    fn capture_policy_matches_the_table() {
        let policy = CapturePolicy::default();
        // No receiver at all: capture off.
        let none = policy.decide(0, 0, false, 0.0, 1_000_000, 0.0);
        assert!(!none.capture);
        // A parked receiver keeps capture alive while the hold is on.
        let parked = policy.decide(0, 1, true, 0.0, 1_000_000, 0.0);
        assert!(parked.capture);
        assert_eq!(parked.min_fps, 1.0);
        assert!(!parked.speculative_refresh);
        // A parked receiver with the hold off and an expired window: off.
        let park_off = CapturePolicy {
            park_hold: false,
            ..policy
        };
        assert!(!park_off.decide(0, 1, true, 0.0, 1_000_000, 0.0).capture);
        // A degraded link narrows work but never pauses capture (MLP spec annex D, D-9).
        let degraded = policy.decide(1, 0, false, 0.10, 32_000, 0.0);
        assert!(degraded.capture);
        assert!(!degraded.speculative_refresh);
        assert!(!degraded.p0_only);
        let floor = policy.decide(1, 0, false, 0.10, 32_000, 0.8);
        assert!(floor.capture);
        assert!(floor.p0_only, "pace at floor with pending > 50 %");
        // A healthy link speculates.
        let healthy = policy.decide(1, 0, false, 0.0, 10_000_000, 0.0);
        assert!(healthy.capture);
        assert!(healthy.speculative_refresh);
    }

    #[test]
    fn policy_gates_match_the_kill_switches() {
        let policy = MediaLanePolicy::default();
        assert!(policy.emits_media());
        assert!(policy.anchors_on_any_state());
        let off = MediaLanePolicy {
            media_lane_enabled: false,
            ..policy
        };
        assert!(!off.emits_media());
        let strict = MediaLanePolicy {
            patch_mode: false,
            ..policy
        };
        assert!(!strict.anchors_on_any_state());
    }
}
