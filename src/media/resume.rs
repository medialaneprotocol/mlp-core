//! Session continuity: resume, parking, rebase and the reconnect ladder
//! (MLP spec §9.1–§8.4, D-5).
//!
//! The token *signature* is the edge's (HMAC-SHA256 with a Vault key); what
//! lives here is every decision around it: claim validation, the park table
//! and its TTLs, the rebase decision, and the receiver's unbounded reconnect
//! ladder. Nothing here mints or verifies cryptography, and nothing reads a
//! clock.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::MediaPath;
use super::input::InputToken;
use super::scheduler::PlanStartReason;

/// `resume_ttl`: the resume TTL default 900 (MLP spec §9.1).
pub const RESUME_TTL_SECONDS: u64 = 900;
/// The park TTL floor (MLP spec §9.2).
pub const PARK_TTL_FLOOR_SECONDS: u64 = 120;
/// The park TTL ceiling (MLP spec §9.2).
pub const PARK_TTL_CEIL_SECONDS: u64 = 900;
/// The default park TTL when no outage target is configured.
pub const PARK_TTL_DEFAULT_SECONDS: u64 = 300;
/// The reconnect ladder's floor delay.
pub const RECONNECT_FLOOR_MS: u64 = 500;
/// The reconnect ladder's ceiling delay.
pub const RECONNECT_CAP_MS: u64 = 30_000;
/// The reconnect ladder's jitter ceiling.
pub const RECONNECT_JITTER_MS: u64 = 250;
/// Attempts after which the UI enters `Suspended`.
pub const SUSPENDED_AFTER_ATTEMPTS: u32 = 6;
/// Attempts after which `POST /api/session/bootstrap` is re-run (MLP spec §9.4).
pub const BOOTSTRAP_AFTER_ATTEMPTS: u32 = 3;

/// Why a resume was rejected (`MediaResumeAck.reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResumeRejectReason {
    /// The resume attached to parked state.
    Ok,
    /// No such session exists.
    UnknownSession,
    /// The token's `exp` has passed.
    TokenExpired,
    /// The token signature or shape is invalid.
    TokenInvalid,
    /// The token names another receiver or sender.
    PeerMismatch,
    /// The receiver's epoch is older than the sender's.
    EpochStale,
    /// The receiver's epoch differs from the sender's but is not older.
    EpochMismatch,
    /// The parked state was evicted before the receiver came back.
    ParkEvicted,
}

impl ResumeRejectReason {
    /// The wire string carried by `MediaResumeAck.reason`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::UnknownSession => "unknown_session",
            Self::TokenExpired => "token_expired",
            Self::TokenInvalid => "token_invalid",
            Self::PeerMismatch => "peer_mismatch",
            Self::EpochStale => "epoch_stale",
            Self::EpochMismatch => "epoch_mismatch",
            Self::ParkEvicted => "park_evicted",
        }
    }

    /// Parse the wire string.
    ///
    /// Named `parse` rather than `from_str` so it is not mistaken for
    /// `std::str::FromStr::from_str`, which returns a `Result`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ok" => Some(Self::Ok),
            "unknown_session" => Some(Self::UnknownSession),
            "token_expired" => Some(Self::TokenExpired),
            "token_invalid" => Some(Self::TokenInvalid),
            "peer_mismatch" => Some(Self::PeerMismatch),
            "epoch_stale" => Some(Self::EpochStale),
            "epoch_mismatch" => Some(Self::EpochMismatch),
            "park_evicted" => Some(Self::ParkEvicted),
            _ => None,
        }
    }

    /// Whether the receiver should start a fresh media session for the same
    /// receiver instead of giving up (MLP spec §9.3).
    pub const fn starts_fresh_media_session(self) -> bool {
        matches!(self, Self::ParkEvicted | Self::EpochStale)
    }
}

/// The `claims` of a resume token (MLP spec §9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeClaims {
    /// Token format version; 2.
    pub version: u32,
    /// The edge session uuid.
    pub session_id: String,
    /// The receiver uuid.
    pub viewer_id: String,
    /// The sender uuid.
    pub host_id: String,
    /// The media session id, or empty.
    pub media_session_id: String,
    /// The session epoch.
    pub epoch: u64,
    /// Issued-at, seconds.
    pub issued_at_secs: u64,
    /// Expiry, seconds.
    pub expires_at_secs: u64,
}

/// The identity a resume must match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeBinding {
    /// The edge session id of the reconnecting control session.
    pub session_id: String,
    /// The receiver asking to resume.
    pub viewer_id: String,
    /// The sender the media session belongs to.
    pub host_id: String,
}

impl ResumeClaims {
    /// The token format version this module understands.
    pub const VERSION: u32 = 2;

    /// The canonical byte string a edge signs.
    ///
    /// The design fixes the claim *fields* but not their encoding, so W1 fixes
    /// one here (little-endian lengths, UTF-8, fields in claim order) and W4/W5
    /// must use exactly these bytes for the HMAC input.
    pub fn to_signing_bytes(&self) -> Vec<u8> {
        fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&self.version.to_le_bytes());
        push_field(&mut out, self.session_id.as_bytes());
        push_field(&mut out, self.viewer_id.as_bytes());
        push_field(&mut out, self.host_id.as_bytes());
        push_field(&mut out, self.media_session_id.as_bytes());
        out.extend_from_slice(&self.epoch.to_le_bytes());
        out.extend_from_slice(&self.issued_at_secs.to_le_bytes());
        out.extend_from_slice(&self.expires_at_secs.to_le_bytes());
        out
    }

    /// Validate the claims against a binding at a point in time.
    pub fn validate_at(
        &self,
        now_secs: u64,
        binding: &ResumeBinding,
    ) -> Result<(), ResumeRejectReason> {
        if self.version != Self::VERSION {
            return Err(ResumeRejectReason::TokenInvalid);
        }
        if self.expires_at_secs <= now_secs {
            return Err(ResumeRejectReason::TokenExpired);
        }
        if self.session_id != binding.session_id
            || self.viewer_id != binding.viewer_id
            || self.host_id != binding.host_id
        {
            return Err(ResumeRejectReason::PeerMismatch);
        }
        Ok(())
    }

    /// Seconds until expiry (zero when already expired).
    pub fn remaining_secs(&self, now_secs: u64) -> u64 {
        self.expires_at_secs.saturating_sub(now_secs)
    }
}

/// The sender's held per-receiver media state across a park (MLP spec §9.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeldViewerState {
    /// The chain epoch.
    pub epoch: u32,
    /// The next update id the sender will use.
    pub next_update_id: u64,
    /// The last update id the sender emitted.
    pub last_sent_frame_id: u64,
    /// Cells the sender knows are unacked.
    pub pending_cells: u64,
    /// Held keys and buttons.
    pub pressed: Vec<InputToken>,
    /// The receiver's next input sequence.
    pub input_seq_next: u64,
    /// The carrier the receiver last reported.
    pub active_path: MediaPath,
    /// The estimator's last pace, so a resume starts at a useful rate.
    pub pace_bps: u64,
    /// The estimator's last per-update budget.
    pub budget_bytes: u32,
    /// The estimator's last target fps.
    pub target_fps: f64,
}

impl Default for HeldViewerState {
    fn default() -> Self {
        Self {
            epoch: 0,
            next_update_id: 1,
            last_sent_frame_id: 0,
            pending_cells: 0,
            pressed: Vec::new(),
            input_seq_next: 0,
            active_path: MediaPath::Unknown,
            pace_bps: 8_000,
            budget_bytes: 1_024,
            target_fps: 1.0,
        }
    }
}

/// What the sender does about a resume once the token is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResumeAction {
    /// Rebase the receiver's accounting onto the held state.
    Rebase {
        /// Cells the sender will re-send.
        pending_cells: u64,
    },
    /// Start an anchor plan with the given reason.
    StartAnchorPlan {
        /// Why the plan started.
        reason: PlanStartReason,
    },
    /// Release keys the receiver no longer holds.
    ReleaseKeys {
        /// The tokens to release.
        tokens: Vec<InputToken>,
    },
    /// Start a fresh media session for the same receiver.
    FreshMediaSession {
        /// The epoch of the new session.
        epoch: u32,
    },
}

/// The sender's answer to a `MediaResume` (MLP spec §9.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeOutcome {
    /// The `MediaResumeAck` the peer receives.
    pub accepted: bool,
    /// Why.
    pub reason: ResumeRejectReason,
    /// The next update id.
    pub next_update_id: u64,
    /// Cells the sender will re-send.
    pub pending_cells: u64,
    /// Current pace.
    pub pace_bps: u32,
    /// Current budget.
    pub budget_bytes: u32,
    /// Current target fps.
    pub target_fps: u32,
    /// Whether a plan had to start.
    pub anchor_plan_started: bool,
    /// What the sender must do next.
    pub actions: Vec<ResumeAction>,
}

/// Everything the sender needs to answer a resume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeRequest {
    /// Validated claims (or claims to validate).
    pub claims: ResumeClaims,
    /// The identity the reconnecting receiver presents.
    pub binding: ResumeBinding,
    /// The receiver's epoch from `MediaResume`.
    pub viewer_epoch: u32,
    /// The receiver's applied update id.
    pub applied_update_id: u64,
    /// The receiver's ack bitmap.
    pub ack_bitmap: u64,
    /// The receiver's next input sequence.
    pub input_seq_next: u64,
    /// Keys the receiver currently holds.
    pub pressed_keys: Vec<u32>,
    /// Buttons the receiver currently holds.
    pub pressed_buttons: Vec<u32>,
    /// The carrier the receiver believes is active.
    pub active_path: MediaPath,
}

/// Decide the sender's answer to a resume and mutate the held state.
///
/// The rules are MLP spec §9.2/§8.3:
/// * a mismatched or expired token never attaches;
/// * an epoch mismatch attaches but starts an anchor plan (`epoch_mismatch`);
/// * an evicted park starts a fresh media session for the same receiver;
/// * an unattached receiver costs no reset update: the held `next_update_id`
///   continues and the receiver rebases onto its own canvas.
pub fn decide_resume(
    request: &ResumeRequest,
    now_secs: u64,
    state: &mut HeldViewerState,
    parked: bool,
    park_expired: bool,
) -> ResumeOutcome {
    let mut actions = Vec::new();
    let rejection = if !parked && park_expired {
        Some(ResumeRejectReason::ParkEvicted)
    } else if !parked {
        Some(ResumeRejectReason::UnknownSession)
    } else {
        request.claims.validate_at(now_secs, &request.binding).err()
    };
    if let Some(reason) = rejection {
        let starts_fresh = reason.starts_fresh_media_session();
        if starts_fresh {
            state.epoch = state.epoch.wrapping_add(1);
            state.next_update_id = state.next_update_id.max(1);
            state.last_sent_frame_id = 0;
            state.pending_cells = state.pending_cells.max(1);
            actions.push(ResumeAction::StartAnchorPlan {
                reason: PlanStartReason::Resume,
            });
            actions.push(ResumeAction::FreshMediaSession { epoch: state.epoch });
        }
        return ResumeOutcome {
            accepted: false,
            reason,
            next_update_id: state.next_update_id,
            pending_cells: state.pending_cells,
            pace_bps: state.pace_bps.min(u64::from(u32::MAX)) as u32,
            budget_bytes: state.budget_bytes,
            target_fps: state.target_fps.round().max(1.0) as u32,
            anchor_plan_started: starts_fresh,
            actions,
        };
    }

    let mut anchor_plan_started = false;
    if request.viewer_epoch != state.epoch {
        // The receiver's canvas trust is reset: everything the sender sent is
        // suspect (MLP spec §7.1). The cells are kept; only trust is lost.
        anchor_plan_started = true;
        state.epoch = state.epoch.max(request.viewer_epoch);
        actions.push(ResumeAction::StartAnchorPlan {
            reason: PlanStartReason::Epoch,
        });
    }
    // Rebase: cells the receiver never applied stay pending.
    let pending = state.pending_cells;
    actions.push(ResumeAction::Rebase {
        pending_cells: pending,
    });
    if pending > 0 && !anchor_plan_started {
        anchor_plan_started = true;
        actions.push(ResumeAction::StartAnchorPlan {
            reason: PlanStartReason::Pending,
        });
    }
    let held: Vec<InputToken> = state.pressed.clone();
    let expected: Vec<InputToken> = request
        .pressed_keys
        .iter()
        .map(|code| InputToken::Key(*code))
        .chain(
            request
                .pressed_buttons
                .iter()
                .map(|code| InputToken::Button(*code)),
        )
        .collect();
    let stale: Vec<InputToken> = held
        .into_iter()
        .filter(|token| !expected.contains(token))
        .collect();
    if !stale.is_empty() {
        actions.push(ResumeAction::ReleaseKeys { tokens: stale });
    }
    state.pressed = expected;
    state.input_seq_next = state.input_seq_next.max(request.input_seq_next);
    state.active_path = request.active_path;

    ResumeOutcome {
        accepted: true,
        reason: ResumeRejectReason::Ok,
        next_update_id: state.next_update_id,
        pending_cells: pending,
        pace_bps: state.pace_bps.min(u64::from(u32::MAX)) as u32,
        budget_bytes: state.budget_bytes,
        target_fps: state.target_fps.round().max(1.0) as u32,
        anchor_plan_started,
        actions,
    }
}

/// Why a parked session left the park table (`*_park_evictions_total{reason}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParkEvictionReason {
    /// The park window closed.
    Ttl,
    /// The table was at capacity.
    Capacity,
    /// The session ended cleanly.
    SessionEnd,
}

impl ParkEvictionReason {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ttl => "ttl",
            Self::Capacity => "capacity",
            Self::SessionEnd => "session_end",
        }
    }
}

/// A parked media session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkEntry {
    /// The media session id: the park key, stable across sender reconnects.
    pub media_session_id: String,
    /// The edge session id.
    pub session_id: String,
    /// The receiver.
    pub viewer_id: String,
    /// The sender.
    pub host_id: String,
    /// Seconds when the session was parked.
    pub parked_at_secs: u64,
    /// The chain epoch at park time.
    pub epoch: u32,
}

/// Park policy: TTLs derived from the outage target (MLP spec §9.2, N-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkPolicy {
    /// `park_ttl = clamp(2 × outage_target + 60 s, 120, 900)`.
    pub park_ttl_seconds: u64,
    /// `resume_ttl` for the edge record.
    pub resume_ttl_seconds: u64,
    /// `max_parked_sessions` per sender.
    pub max_parked_sessions: usize,
}

impl Default for ParkPolicy {
    fn default() -> Self {
        Self {
            park_ttl_seconds: PARK_TTL_DEFAULT_SECONDS,
            resume_ttl_seconds: RESUME_TTL_SECONDS,
            max_parked_sessions: 8,
        }
    }
}

impl ParkPolicy {
    /// The park TTL for an outage target and RTT.
    pub fn park_ttl_for(outage_target_secs: u64, _rtt_ms: f64) -> u64 {
        (2 * outage_target_secs + 60).clamp(PARK_TTL_FLOOR_SECONDS, PARK_TTL_CEIL_SECONDS)
    }

    /// Whether an entry has expired at `now_secs`.
    pub fn is_expired(&self, entry: &ParkEntry, now_secs: u64) -> bool {
        now_secs.saturating_sub(entry.parked_at_secs) >= self.park_ttl_seconds
    }
}

/// The bounded park table (sender or intermediary side).
#[derive(Debug, Clone, Default)]
pub struct ParkTable {
    entries: HashMap<String, ParkEntry>,
    capacity: usize,
    evictions: HashMap<&'static str, u64>,
}

impl ParkTable {
    /// Create a park table with a capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            evictions: HashMap::new(),
        }
    }

    /// Park a session. Returns evicted entries.
    pub fn park(&mut self, entry: ParkEntry, now_secs: u64, policy: &ParkPolicy) -> Vec<ParkEntry> {
        let mut evicted = self.evict_expired(now_secs, policy);
        if self.entries.len() >= self.capacity
            && !self.entries.contains_key(&entry.media_session_id)
        {
            // Evict the oldest parked session.
            if let Some(oldest) = self
                .entries
                .values()
                .min_by_key(|candidate| candidate.parked_at_secs)
                .cloned()
            {
                self.entries.remove(&oldest.media_session_id);
                self.note_eviction(ParkEvictionReason::Capacity);
                evicted.push(oldest);
            }
        }
        self.entries.insert(entry.media_session_id.clone(), entry);
        evicted
    }

    /// Look up a parked session.
    pub fn get(&self, media_session_id: &str) -> Option<&ParkEntry> {
        self.entries.get(media_session_id)
    }

    /// Attach to a parked session, removing it from the table.
    pub fn resume(&mut self, media_session_id: &str) -> Option<ParkEntry> {
        self.entries.remove(media_session_id)
    }

    /// End a session deliberately.
    pub fn end(&mut self, media_session_id: &str) -> Option<ParkEntry> {
        let entry = self.entries.remove(media_session_id);
        if entry.is_some() {
            self.note_eviction(ParkEvictionReason::SessionEnd);
        }
        entry
    }

    /// Evict everything past its TTL.
    pub fn evict_expired(&mut self, now_secs: u64, policy: &ParkPolicy) -> Vec<ParkEntry> {
        let expired: Vec<ParkEntry> = self
            .entries
            .values()
            .filter(|entry| policy.is_expired(entry, now_secs))
            .cloned()
            .collect();
        for entry in &expired {
            self.entries.remove(&entry.media_session_id);
            self.note_eviction(ParkEvictionReason::Ttl);
        }
        expired
    }

    /// Number of parked sessions.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no session is parked.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Eviction counters keyed by reason.
    pub fn evictions(&self, reason: ParkEvictionReason) -> u64 {
        self.evictions.get(reason.as_str()).copied().unwrap_or(0)
    }

    fn note_eviction(&mut self, reason: ParkEvictionReason) {
        *self.evictions.entry(reason.as_str()).or_insert(0) += 1;
    }
}

/// What the receiver's ladder does at an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconnectStage {
    /// Retry the control WebSocket.
    ControlRetry,
    /// Also re-run `POST /api/session/bootstrap`.
    Bootstrap,
    /// The UI shows `Suspended`; retries continue forever.
    Suspended,
}

impl ReconnectStage {
    /// The stable metric label / UI state spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ControlRetry => "retry",
            Self::Bootstrap => "bootstrap",
            Self::Suspended => "suspended",
        }
    }
}

/// A terminal condition: the ladder must stop instead of retrying (MLP spec §9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalReason {
    /// 401/403 from bootstrap or the handshake.
    Unauthorized,
    /// The sender is retired or revoked.
    HostRetired,
    /// The operator disconnected the session.
    OperatorDisconnect,
    /// The edge refused the media protocol version.
    UnsupportedMediaVersion,
    /// Two consecutive `park_ttl_expired` answers.
    ParkTtlExpiredTwice,
    /// An explicit operator action ended the session.
    OperatorAction,
}

/// The receiver's reconnect ladder (MLP spec §9.4): RTT-relative and unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReconnectLadder {
    /// `max(500 ms, 1 × rtt_smoothed)`.
    base_ms: u64,
    /// The per-attempt ceiling.
    cap_ms: u64,
    /// The jitter ceiling.
    jitter_ms: u64,
}

impl Default for ReconnectLadder {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl ReconnectLadder {
    /// Build a ladder for a smoothed RTT.
    pub fn new(rtt_ms: f64) -> Self {
        Self {
            base_ms: (rtt_ms.round() as u64).max(RECONNECT_FLOOR_MS),
            cap_ms: RECONNECT_CAP_MS,
            jitter_ms: RECONNECT_JITTER_MS,
        }
    }

    /// The base delay.
    pub fn base_ms(&self) -> u64 {
        self.base_ms
    }

    /// The delay before `attempt` (`0` means "now"), with injected jitter.
    ///
    /// `jitter_sample_ms` is clamped to `0..=250`; the driver supplies it.
    pub fn delay_ms(&self, attempt: u32, jitter_sample_ms: u64) -> u64 {
        if attempt == 0 {
            return 0;
        }
        let exponent = attempt.saturating_sub(1).min(16);
        let scaled = self.base_ms.saturating_mul(1u64 << exponent);
        let delay = scaled.clamp(RECONNECT_FLOOR_MS, self.cap_ms);
        delay + jitter_sample_ms.min(self.jitter_ms)
    }

    /// The stage an attempt belongs to.
    pub fn stage(&self, attempt: u32) -> ReconnectStage {
        if attempt >= SUSPENDED_AFTER_ATTEMPTS {
            ReconnectStage::Suspended
        } else if attempt >= BOOTSTRAP_AFTER_ATTEMPTS {
            ReconnectStage::Bootstrap
        } else {
            ReconnectStage::ControlRetry
        }
    }

    /// Cumulative ladder time through `attempts` attempts (no jitter).
    pub fn cumulative_ms(&self, attempts: u32) -> u64 {
        (0..attempts).map(|attempt| self.delay_ms(attempt, 0)).sum()
    }

    /// Whether the ladder keeps retrying for a terminal reason.
    pub fn retries_for(&self, _reason: TerminalReason) -> bool {
        // Every terminal reason means "do not retry" (MLP spec §9.4).
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> ResumeClaims {
        ResumeClaims {
            version: ResumeClaims::VERSION,
            session_id: "sess-1".into(),
            viewer_id: "receiver-1".into(),
            host_id: "sender-1".into(),
            media_session_id: "media-1".into(),
            epoch: 3,
            issued_at_secs: 1_000,
            expires_at_secs: 1_900,
        }
    }

    fn binding() -> ResumeBinding {
        ResumeBinding {
            session_id: "sess-1".into(),
            viewer_id: "receiver-1".into(),
            host_id: "sender-1".into(),
        }
    }

    fn entry(parked_at_secs: u64) -> ParkEntry {
        ParkEntry {
            media_session_id: "media-1".into(),
            session_id: "sess-1".into(),
            viewer_id: "receiver-1".into(),
            host_id: "sender-1".into(),
            parked_at_secs,
            epoch: 3,
        }
    }

    #[test]
    fn claim_validation_happy_path() {
        assert_eq!(claims().validate_at(1_500, &binding()), Ok(()));
        assert_eq!(claims().remaining_secs(1_500), 400);
        assert_eq!(claims().remaining_secs(2_000), 0);
    }

    #[test]
    fn expired_token_is_typed() {
        assert_eq!(
            claims().validate_at(1_900, &binding()),
            Err(ResumeRejectReason::TokenExpired)
        );
        assert_eq!(
            claims().validate_at(1_899, &binding()),
            Ok(()),
            "exp is exclusive"
        );
    }

    #[test]
    fn wrong_version_and_mismatched_identity_are_typed() {
        let mut wrong_version = claims();
        wrong_version.version = 1;
        assert_eq!(
            wrong_version.validate_at(1_500, &binding()),
            Err(ResumeRejectReason::TokenInvalid)
        );
        let mut wrong_viewer = claims();
        wrong_viewer.viewer_id = "someone-else".into();
        assert_eq!(
            wrong_viewer.validate_at(1_500, &binding()),
            Err(ResumeRejectReason::PeerMismatch)
        );
    }

    #[test]
    fn signing_bytes_are_deterministic_and_field_sensitive() {
        let first = claims().to_signing_bytes();
        assert_eq!(first, claims().to_signing_bytes());
        let mut other = claims();
        other.media_session_id = "media-2".into();
        assert_ne!(first, other.to_signing_bytes());
        assert!(first.len() > 40);
    }

    #[test]
    fn resume_reasons_round_trip() {
        for reason in [
            ResumeRejectReason::Ok,
            ResumeRejectReason::UnknownSession,
            ResumeRejectReason::TokenExpired,
            ResumeRejectReason::TokenInvalid,
            ResumeRejectReason::PeerMismatch,
            ResumeRejectReason::EpochStale,
            ResumeRejectReason::EpochMismatch,
            ResumeRejectReason::ParkEvicted,
        ] {
            assert_eq!(ResumeRejectReason::parse(reason.as_str()), Some(reason));
        }
    }

    fn request(viewer_epoch: u32) -> ResumeRequest {
        ResumeRequest {
            claims: claims(),
            binding: binding(),
            viewer_epoch,
            applied_update_id: 40,
            ack_bitmap: 0b1,
            input_seq_next: 100,
            pressed_keys: vec![16],
            pressed_buttons: vec![1],
            active_path: MediaPath::Ws,
        }
    }

    #[test]
    fn a_clean_resume_rebases_without_a_plan() {
        let mut state = HeldViewerState {
            epoch: 3,
            next_update_id: 41,
            pending_cells: 0,
            pressed: vec![InputToken::Key(16), InputToken::Button(1)],
            input_seq_next: 90,
            ..HeldViewerState::default()
        };
        let outcome = decide_resume(&request(3), 1_500, &mut state, true, false);
        assert!(outcome.accepted);
        assert_eq!(outcome.reason, ResumeRejectReason::Ok);
        assert_eq!(outcome.next_update_id, 41, "the update chain continues");
        assert!(!outcome.anchor_plan_started);
        assert_eq!(
            outcome.actions,
            vec![ResumeAction::Rebase { pending_cells: 0 }]
        );
        assert_eq!(state.input_seq_next, 100);
        assert_eq!(state.active_path, MediaPath::Ws);
    }

    #[test]
    fn pending_cells_start_a_plan_on_resume() {
        let mut state = HeldViewerState {
            epoch: 3,
            pending_cells: 12,
            ..HeldViewerState::default()
        };
        let outcome = decide_resume(&request(3), 1_500, &mut state, true, false);
        assert!(outcome.accepted);
        assert!(outcome.anchor_plan_started);
        assert!(outcome.actions.contains(&ResumeAction::StartAnchorPlan {
            reason: PlanStartReason::Pending
        }));
    }

    #[test]
    fn epoch_mismatch_starts_a_plan_but_attaches() {
        let mut state = HeldViewerState {
            epoch: 5,
            pending_cells: 0,
            ..HeldViewerState::default()
        };
        let outcome = decide_resume(&request(7), 1_500, &mut state, true, false);
        assert!(outcome.accepted);
        assert!(outcome.anchor_plan_started);
        assert!(outcome.actions.contains(&ResumeAction::StartAnchorPlan {
            reason: PlanStartReason::Epoch
        }));
        assert_eq!(state.epoch, 7);
    }

    #[test]
    fn evicted_park_starts_a_fresh_media_session() {
        let mut state = HeldViewerState {
            epoch: 3,
            next_update_id: 90,
            pending_cells: 0,
            ..HeldViewerState::default()
        };
        let outcome = decide_resume(&request(3), 2_000, &mut state, false, true);
        assert!(!outcome.accepted);
        assert_eq!(outcome.reason, ResumeRejectReason::ParkEvicted);
        assert!(outcome.anchor_plan_started);
        assert_eq!(state.epoch, 4, "chain_epoch++");
        assert!(matches!(
            outcome.actions.last(),
            Some(ResumeAction::FreshMediaSession { .. })
        ));
    }

    #[test]
    fn unknown_session_and_expired_token_are_refused() {
        let mut state = HeldViewerState::default();
        let unknown = decide_resume(&request(0), 1_500, &mut state, false, false);
        assert_eq!(unknown.reason, ResumeRejectReason::UnknownSession);
        assert!(!unknown.accepted);
        let mut expired_request = request(3);
        expired_request.claims.expires_at_secs = 100;
        let expired = decide_resume(&expired_request, 1_500, &mut state, true, false);
        assert_eq!(expired.reason, ResumeRejectReason::TokenExpired);
        assert!(!expired.accepted);
    }

    #[test]
    fn resume_releases_keys_the_viewer_no_longer_holds() {
        let mut state = HeldViewerState {
            epoch: 3,
            pressed: vec![
                InputToken::Key(16),
                InputToken::Key(17),
                InputToken::Button(1),
            ],
            ..HeldViewerState::default()
        };
        let mut incoming = request(3);
        incoming.pressed_keys = vec![16];
        incoming.pressed_buttons = vec![];
        let outcome = decide_resume(&incoming, 1_500, &mut state, true, false);
        assert_eq!(
            outcome.actions,
            vec![
                ResumeAction::Rebase { pending_cells: 0 },
                ResumeAction::ReleaseKeys {
                    tokens: vec![InputToken::Key(17), InputToken::Button(1)]
                },
            ]
        );
        assert_eq!(state.pressed, vec![InputToken::Key(16)]);
    }

    #[test]
    fn park_ttl_follows_the_outage_target() {
        assert_eq!(ParkPolicy::park_ttl_for(30, 2_000.0), 120);
        assert_eq!(ParkPolicy::park_ttl_for(10, 100.0), 120);
        assert_eq!(ParkPolicy::park_ttl_for(120, 100.0), 300);
        assert_eq!(ParkPolicy::park_ttl_for(600, 100.0), 900, "ceiling");
    }

    #[test]
    fn park_table_ttl_capacity_and_resume() {
        let policy = ParkPolicy {
            park_ttl_seconds: 300,
            ..ParkPolicy::default()
        };
        let mut table = ParkTable::new(2);
        assert!(table.park(entry(1_000), 1_000, &policy).is_empty());
        let evicted = table.park(
            ParkEntry {
                media_session_id: "media-2".into(),
                ..entry(1_010)
            },
            1_010,
            &policy,
        );
        assert!(evicted.is_empty());
        // Third entry evicts the oldest by capacity.
        let evicted = table.park(
            ParkEntry {
                media_session_id: "media-3".into(),
                ..entry(1_020)
            },
            1_020,
            &policy,
        );
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].media_session_id, "media-1");
        assert_eq!(table.evictions(ParkEvictionReason::Capacity), 1);
        // Resume removes the entry.
        assert!(table.resume("media-2").is_some());
        assert_eq!(table.len(), 1);
        // TTL eviction.
        let expired = table.evict_expired(1_020 + 300, &policy);
        assert_eq!(expired.len(), 1);
        assert_eq!(table.evictions(ParkEvictionReason::Ttl), 1);
        assert!(table.is_empty());
    }

    #[test]
    fn park_table_session_end_is_counted() {
        let policy = ParkPolicy::default();
        let mut table = ParkTable::new(4);
        table.park(entry(0), 0, &policy);
        assert!(table.end("media-1").is_some());
        assert_eq!(table.evictions(ParkEvictionReason::SessionEnd), 1);
    }

    #[test]
    fn ladder_delays_are_rtt_relative_and_unbounded() {
        let ladder = ReconnectLadder::new(1_000.0);
        assert_eq!(ladder.delay_ms(0, 0), 0);
        assert_eq!(ladder.delay_ms(1, 0), 1_000);
        assert_eq!(ladder.delay_ms(2, 0), 2_000);
        assert_eq!(ladder.delay_ms(3, 0), 4_000);
        assert_eq!(ladder.delay_ms(4, 0), 8_000);
        assert_eq!(ladder.delay_ms(5, 0), 16_000);
        assert_eq!(ladder.delay_ms(6, 0), 30_000, "capped");
        assert_eq!(ladder.delay_ms(20, 0), 30_000, "still capped, never stops");
        assert_eq!(ladder.delay_ms(1, 999), 1_250, "jitter capped at 250 ms");
    }

    #[test]
    fn ladder_stages_match_the_design() {
        let ladder = ReconnectLadder::new(100.0);
        assert_eq!(ladder.stage(0), ReconnectStage::ControlRetry);
        assert_eq!(ladder.stage(2), ReconnectStage::ControlRetry);
        assert_eq!(ladder.stage(3), ReconnectStage::Bootstrap);
        assert_eq!(ladder.stage(6), ReconnectStage::Suspended);
        assert_eq!(ladder.stage(50), ReconnectStage::Suspended);
    }

    #[test]
    fn t4_flap_fits_inside_the_park_ttl() {
        // Design §8.4: 5 × 10 s outages need ≈ 60 s of ladder time at 2 s RTT,
        // so park_ttl ≥ 120 s covers it.
        let ladder = ReconnectLadder::new(2_000.0);
        let ladder_time = ladder.cumulative_ms(6);
        assert!(ladder_time <= 61_500, "ladder time {ladder_time} ms");
        let park_ttl = ParkPolicy::park_ttl_for(30, 2_000.0);
        assert!(
            park_ttl * 1_000 >= 5 * 10_000 + ladder_time,
            "park_ttl {park_ttl}s must cover 5 flaps plus the ladder"
        );
    }

    #[test]
    fn terminal_reasons_never_retry() {
        let ladder = ReconnectLadder::default();
        for reason in [
            TerminalReason::Unauthorized,
            TerminalReason::HostRetired,
            TerminalReason::OperatorDisconnect,
            TerminalReason::UnsupportedMediaVersion,
            TerminalReason::ParkTtlExpiredTwice,
            TerminalReason::OperatorAction,
        ] {
            assert!(!ladder.retries_for(reason));
        }
    }

    #[test]
    fn resume_reason_classification_for_fresh_sessions() {
        assert!(ResumeRejectReason::ParkEvicted.starts_fresh_media_session());
        assert!(ResumeRejectReason::EpochStale.starts_fresh_media_session());
        assert!(!ResumeRejectReason::TokenExpired.starts_fresh_media_session());
        assert!(!ResumeRejectReason::UnknownSession.starts_fresh_media_session());
    }
}
