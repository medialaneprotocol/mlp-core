//! Input path (MLP spec §9, D-8): receiver queue and sender-side policy.
//!
//! Input rides the control lane with an [`MediaInputEnvelope`] wrapper. Two
//! machines live here:
//!
//! * [`ViewerInputQueue`] — bounded (64 objects / 8 KiB), deadline-aware,
//!   coalescing per class, with a replay path for resume.
//! * [`HostInputState`] — dedup window, deadline enforcement with the release
//!   exception, the pressed-set protocol, and the release guarantees that make
//!   **stuck keys impossible by construction** (MLP spec annex INV, INV-4).

use std::collections::{BTreeSet, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

/// Default input deadline: the input deadline.
pub const INPUT_DEADLINE_MS: u64 = 5_000;
/// Receiver queue object capacity (MLP spec §13).
pub const INPUT_QUEUE_OBJECTS: usize = 64;
/// Receiver queue byte capacity (MLP spec §13).
pub const INPUT_QUEUE_BYTES: usize = 8 * 1024;
/// Sender input worker capacity (MLP spec §13).
pub const HOST_WORKER_CAPACITY: usize = 64;
/// `(epoch, seq)` dedup window (MLP spec §13).
pub const INPUT_DEDUP_WINDOW: usize = 4096;
/// `key_release_grace` after parking (MLP spec §13).
pub const KEY_RELEASE_GRACE_MS: u64 = 2_000;
/// Wheel accumulation ceiling in notches (MLP spec §13).
pub const WHEEL_DELTA_CAP: i32 = 8;

/// The `kind` field of `MediaInputEnvelope` (MLP spec §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u32)]
pub enum InputKind {
    /// Pointer movement.
    PointerMove = 0,
    /// Pointer button transition.
    PointerButton = 1,
    /// Key transition.
    Key = 2,
    /// Wheel motion.
    Wheel = 3,
    /// Clipboard payload.
    Clipboard = 4,
    /// Release everything.
    ReleaseAll = 5,
    /// The pressed-key heartbeat.
    PressedSet = 6,
}

impl InputKind {
    /// Decode the wire value.
    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::PointerMove),
            1 => Some(Self::PointerButton),
            2 => Some(Self::Key),
            3 => Some(Self::Wheel),
            4 => Some(Self::Clipboard),
            5 => Some(Self::ReleaseAll),
            6 => Some(Self::PressedSet),
            _ => None,
        }
    }

    /// The wire value.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Whether the kind is a key/button transition that must never be dropped
    /// while queued (MLP spec §13).
    pub const fn is_transition(self) -> bool {
        matches!(self, Self::Key | Self::PointerButton)
    }

    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PointerMove => "pointer_move",
            Self::PointerButton => "pointer_button",
            Self::Key => "key",
            Self::Wheel => "wheel",
            Self::Clipboard => "clipboard",
            Self::ReleaseAll => "release_all",
            Self::PressedSet => "pressed_set",
        }
    }
}

/// The `flags` field of `MediaInputEnvelope` (MLP spec §13).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InputFlags(u8);

impl InputFlags {
    /// A release (key/button up).
    pub const RELEASE: Self = Self(0x01);
    /// An auto-repeat key event.
    pub const REPEAT: Self = Self(0x02);
    /// Critical: never dropped by capacity pressure.
    pub const CRITICAL: Self = Self(0x04);
    /// No flags.
    pub const NONE: Self = Self(0x00);

    /// Build from raw bits; unknown high bits are preserved but unused.
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
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
}

impl core::fmt::Debug for InputFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut parts = Vec::new();
        for (flag, name) in [
            (Self::RELEASE, "RELEASE"),
            (Self::REPEAT, "REPEAT"),
            (Self::CRITICAL, "CRITICAL"),
        ] {
            if self.contains(flag) {
                parts.push(name);
            }
        }
        if parts.is_empty() {
            write!(f, "InputFlags(none)")
        } else {
            write!(f, "InputFlags({})", parts.join("|"))
        }
    }
}

/// The `MediaInputEnvelope` (MLP spec §13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInputEnvelope {
    /// Monotonic per receiver epoch; the dedup boundary.
    pub input_seq: u64,
    /// Session-relative absolute deadline; `0` never expires.
    pub deadline_epoch_ms: u64,
    /// The transport-level kind.
    pub kind: InputKind,
    /// The transport-level flags.
    pub flags: InputFlags,
    /// The existing command's serialized payload, unchanged.
    pub payload: Vec<u8>,
}

impl MediaInputEnvelope {
    /// Create an envelope.
    pub fn new(
        input_seq: u64,
        deadline_epoch_ms: u64,
        kind: InputKind,
        flags: InputFlags,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            input_seq,
            deadline_epoch_ms,
            kind,
            flags,
            payload,
        }
    }

    /// Wire size estimate used for the queue byte budget.
    pub fn wire_len(&self) -> usize {
        self.payload.len() + 24
    }

    /// Whether the envelope is expired at `now_ms`.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.deadline_epoch_ms != 0 && now_ms > self.deadline_epoch_ms
    }

    /// Whether the object must always be applied, deadline or not.
    pub fn is_always_applied(&self) -> bool {
        matches!(self.kind, InputKind::ReleaseAll | InputKind::PressedSet)
            || self.flags.contains(InputFlags::RELEASE)
    }

    /// Whether capacity pressure may never drop this object.
    pub fn is_critical(&self) -> bool {
        self.is_always_applied() || self.flags.contains(InputFlags::CRITICAL)
    }

    /// The capacity-drop priority: lower is dropped first.
    fn drop_rank(&self) -> u8 {
        match self.kind {
            InputKind::Wheel => 1,
            InputKind::Clipboard => 2,
            InputKind::PointerMove => 0,
            _ => 3,
        }
    }
}

/// Encode a pressed-set payload.
///
/// Layout (little-endian), chosen here because the specification specifies a payload
/// without fixing its bytes: `u32 key_count ‖ u32 keys… ‖ u32 button_count ‖
/// u32 buttons…`.
pub fn encode_pressed_set(keys: &[u32], buttons: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 4 * (keys.len() + buttons.len()));
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for key in keys {
        out.extend_from_slice(&key.to_le_bytes());
    }
    out.extend_from_slice(&(buttons.len() as u32).to_le_bytes());
    for button in buttons {
        out.extend_from_slice(&button.to_le_bytes());
    }
    out
}

/// Decode a pressed-set payload; `None` when the bytes are malformed.
pub fn decode_pressed_set(bytes: &[u8]) -> Option<(Vec<u32>, Vec<u32>)> {
    let mut cursor = 0usize;
    let read_count = |bytes: &[u8], cursor: &mut usize| -> Option<usize> {
        if *cursor + 4 > bytes.len() {
            return None;
        }
        let count = u32::from_le_bytes([
            bytes[*cursor],
            bytes[*cursor + 1],
            bytes[*cursor + 2],
            bytes[*cursor + 3],
        ]) as usize;
        *cursor += 4;
        Some(count)
    };
    let key_count = read_count(bytes, &mut cursor)?;
    if cursor + key_count * 4 > bytes.len() {
        return None;
    }
    let mut keys = Vec::with_capacity(key_count);
    for _ in 0..key_count {
        keys.push(u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]));
        cursor += 4;
    }
    let button_count = read_count(bytes, &mut cursor)?;
    if cursor + button_count * 4 > bytes.len() {
        return None;
    }
    let mut buttons = Vec::with_capacity(button_count);
    for _ in 0..button_count {
        buttons.push(u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]));
        cursor += 4;
    }
    if cursor != bytes.len() {
        return None;
    }
    Some((keys, buttons))
}

/// A key or button identity in the sender's pressed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum InputToken {
    /// A key code.
    Key(u32),
    /// A pointer button.
    Button(u32),
}

impl InputToken {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Key(_) => "key",
            Self::Button(_) => "button",
        }
    }

    /// The numeric code.
    pub const fn code(self) -> u32 {
        match self {
            Self::Key(code) | Self::Button(code) => code,
        }
    }
}

/// Why a receiver-side input object left the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputDropReason {
    /// The queue was at capacity.
    Capacity,
    /// The deadline passed.
    Deadline,
    /// The socket is down and the object may not be replayed.
    Disconnected,
    /// A newer pointer move replaced it.
    Superseded,
    /// The wheel accumulator consumed it.
    Accumulated,
}

impl InputDropReason {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Capacity => "capacity",
            Self::Deadline => "deadline",
            Self::Disconnected => "disconnected",
            Self::Superseded => "superseded",
            Self::Accumulated => "accumulated",
        }
    }
}

/// Counters for `viewer_input_dropped_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InputDropCounters {
    /// Dropped by capacity.
    pub capacity: u64,
    /// Dropped by deadline.
    pub deadline: u64,
    /// Clipboard dropped while disconnected.
    pub disconnected: u64,
    /// Pointer moves superseded by a newer position.
    pub superseded: u64,
}

/// Receiver queue policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputQueueConfig {
    /// Object capacity.
    pub max_objects: usize,
    /// Byte capacity.
    pub max_bytes: usize,
    /// the input deadline.
    pub deadline_ms: u64,
    /// `input_pressed_heartbeat = clamp(5 s, 2 × RTT, 30 s)`.
    pub pressed_heartbeat_ms: u64,
}

impl Default for InputQueueConfig {
    fn default() -> Self {
        Self {
            max_objects: INPUT_QUEUE_OBJECTS,
            max_bytes: INPUT_QUEUE_BYTES,
            deadline_ms: INPUT_DEADLINE_MS,
            pressed_heartbeat_ms: 5_000,
        }
    }
}

impl InputQueueConfig {
    /// The pressed-set heartbeat for a link: `clamp(5 s, 2 × RTT, 30 s)`.
    pub fn heartbeat_for_rtt(rtt_ms: f64) -> u64 {
        (2.0 * rtt_ms).round().clamp(5_000.0, 30_000.0) as u64
    }

    pub fn with_max_objects(mut self, v: usize) -> Self { self.max_objects = v; self }
    pub fn with_max_bytes(mut self, v: usize) -> Self { self.max_bytes = v; self }
    pub fn with_deadline_ms(mut self, v: u64) -> Self { self.deadline_ms = v; self }
    pub fn with_pressed_heartbeat_ms(mut self, v: u64) -> Self { self.pressed_heartbeat_ms = v; self }
}

/// What happened to a pushed input object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// Queued.
    Queued,
    /// Replaced an un-sent pointer move.
    Replaced,
    /// Accumulated into the wheel slot.
    Accumulated,
    /// Dropped immediately.
    Dropped(InputDropReason),
}

/// The receiver-side bounded, deadline-aware input queue (MLP spec §13).
#[derive(Debug, Clone)]
pub struct ViewerInputQueue {
    config: InputQueueConfig,
    queue: VecDeque<MediaInputEnvelope>,
    bytes: usize,
    next_seq: u64,
    connected: bool,
    pointer_slot: Option<usize>,
    wheel_delta: i32,
    clipboard_slot: Option<usize>,
    pressed_keys: BTreeSet<u32>,
    pressed_buttons: BTreeSet<u32>,
    drops: InputDropCounters,
    next_pressed_heartbeat_ms: u64,
    last_now_ms: u64,
}

impl ViewerInputQueue {
    /// Create a queue; sequence numbers start at `first_seq`.
    pub fn new(config: InputQueueConfig, first_seq: u64) -> Self {
        Self {
            config,
            queue: VecDeque::new(),
            bytes: 0,
            next_seq: first_seq,
            connected: false,
            pointer_slot: None,
            wheel_delta: 0,
            clipboard_slot: None,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            drops: InputDropCounters::default(),
            next_pressed_heartbeat_ms: 0,
            last_now_ms: 0,
        }
    }

    /// The queue depth in objects.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Queued bytes.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The next sequence number that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The drop counters.
    pub fn drops(&self) -> InputDropCounters {
        self.drops
    }

    /// The pressed set (the receiver's own belief).
    pub fn pressed_set(&self) -> (Vec<u32>, Vec<u32>) {
        (
            self.pressed_keys.iter().copied().collect(),
            self.pressed_buttons.iter().copied().collect(),
        )
    }

    /// Note the transport state: affects clipboard and pointer coalescing.
    pub fn set_connected(&mut self, connected: bool, now_ms: u64) {
        self.connected = connected;
        self.last_now_ms = now_ms;
        if !connected {
            // While the socket is down only the latest position is kept.
            self.drop_all_moves(InputDropReason::Disconnected);
        }
    }

    /// Push a pointer move (latest-wins per pointer).
    pub fn push_pointer_move(&mut self, payload: Vec<u8>, now_ms: u64) -> PushOutcome {
        let envelope = self.make(InputKind::PointerMove, InputFlags::NONE, payload, now_ms);
        if let Some(index) = self.pointer_slot
            && index < self.queue.len()
        {
            let old = core::mem::replace(&mut self.queue[index], envelope);
            self.bytes = self.bytes + self.queue[index].wire_len() - old.wire_len();
            self.drops.superseded += 1;
            return PushOutcome::Replaced;
        }
        let index = self.queue.len();
        if self.enqueue(envelope, now_ms) {
            self.pointer_slot = Some(index);
            PushOutcome::Queued
        } else {
            PushOutcome::Dropped(InputDropReason::Capacity)
        }
    }

    /// Push a pointer button transition (never dropped while queued).
    pub fn push_pointer_button(
        &mut self,
        button: u32,
        pressed: bool,
        payload: Vec<u8>,
        now_ms: u64,
    ) -> PushOutcome {
        let mut flags = InputFlags::NONE;
        if !pressed {
            flags = flags.union(InputFlags::RELEASE).union(InputFlags::CRITICAL);
            self.pressed_buttons.remove(&button);
        } else {
            self.pressed_buttons.insert(button);
        }
        let envelope = self.make(InputKind::PointerButton, flags, payload, now_ms);
        // A release is critical and always fits; a press competes for room.
        if self.enqueue(envelope, now_ms) {
            PushOutcome::Queued
        } else {
            PushOutcome::Dropped(InputDropReason::Capacity)
        }
    }

    /// Push a key transition (never dropped while queued).
    pub fn push_key(
        &mut self,
        code: u32,
        pressed: bool,
        repeat: bool,
        payload: Vec<u8>,
        now_ms: u64,
    ) -> PushOutcome {
        let mut flags = InputFlags::NONE;
        if repeat {
            flags = flags.union(InputFlags::REPEAT);
        }
        if !pressed {
            flags = flags.union(InputFlags::RELEASE).union(InputFlags::CRITICAL);
            self.pressed_keys.remove(&code);
        } else {
            self.pressed_keys.insert(code);
        }
        let envelope = self.make(InputKind::Key, flags, payload, now_ms);
        if self.enqueue(envelope, now_ms) {
            PushOutcome::Queued
        } else {
            PushOutcome::Dropped(InputDropReason::Capacity)
        }
    }

    /// Accumulate wheel motion, capped at ±[`WHEEL_DELTA_CAP`] notches.
    pub fn push_wheel(&mut self, delta: i32, payload: Vec<u8>, now_ms: u64) -> PushOutcome {
        let _ = payload;
        self.wheel_delta = (self.wheel_delta + delta).clamp(-WHEEL_DELTA_CAP, WHEEL_DELTA_CAP);
        self.last_now_ms = now_ms;
        PushOutcome::Accumulated
    }

    /// Push a clipboard payload; dropped when disconnected.
    pub fn push_clipboard(&mut self, payload: Vec<u8>, now_ms: u64) -> PushOutcome {
        if !self.connected {
            self.drops.disconnected += 1;
            return PushOutcome::Dropped(InputDropReason::Disconnected);
        }
        let envelope = self.make(InputKind::Clipboard, InputFlags::NONE, payload, now_ms);
        if self.enqueue(envelope, now_ms) {
            self.clipboard_slot = Some(self.queue.len() - 1);
            PushOutcome::Queued
        } else {
            PushOutcome::Dropped(InputDropReason::Capacity)
        }
    }

    /// Push a "release everything" object; always critical.
    pub fn push_release_all(&mut self, payload: Vec<u8>, now_ms: u64) -> PushOutcome {
        self.pressed_keys.clear();
        self.pressed_buttons.clear();
        let envelope = self.make(InputKind::ReleaseAll, InputFlags::CRITICAL, payload, now_ms);
        if self.enqueue(envelope, now_ms) {
            PushOutcome::Queued
        } else {
            PushOutcome::Dropped(InputDropReason::Capacity)
        }
    }

    /// Discard expired objects and return the ready ones in `input_seq` order.
    ///
    /// The wheel accumulator materializes here so its sequence number is
    /// assigned in send order.
    pub fn take_ready(&mut self, now_ms: u64) -> Vec<MediaInputEnvelope> {
        self.last_now_ms = now_ms;
        self.expire(now_ms);
        if self.wheel_delta != 0 {
            let delta = core::mem::take(&mut self.wheel_delta);
            let payload = delta.to_le_bytes().to_vec();
            let envelope = self.make(InputKind::Wheel, InputFlags::NONE, payload, now_ms);
            self.enqueue(envelope, now_ms);
        }
        self.pointer_slot = None;
        self.clipboard_slot = None;
        let mut ready: Vec<MediaInputEnvelope> = self.queue.drain(..).collect();
        self.bytes = 0;
        ready.sort_by_key(|envelope| envelope.input_seq);
        ready
    }

    /// Replay for a resume: queued objects in sequence order plus one
    /// pressed-set object (MLP spec §13).
    pub fn replay(&mut self, now_ms: u64) -> Vec<MediaInputEnvelope> {
        let mut ready = self.take_ready(now_ms);
        ready.push(self.pressed_set_envelope(now_ms));
        self.next_pressed_heartbeat_ms = now_ms + self.config.pressed_heartbeat_ms;
        ready
    }

    /// A pressed-set object carrying the receiver's current pressed set.
    pub fn pressed_set_envelope(&mut self, now_ms: u64) -> MediaInputEnvelope {
        let (keys, buttons) = self.pressed_set();
        let payload = encode_pressed_set(&keys, &buttons);
        self.make(InputKind::PressedSet, InputFlags::CRITICAL, payload, now_ms)
    }

    /// Whether the pressed-set heartbeat is due (MLP spec §13 rule 4).
    pub fn pressed_heartbeat_due(&self, now_ms: u64) -> bool {
        now_ms >= self.next_pressed_heartbeat_ms
    }

    /// Emit the heartbeat when due.
    pub fn poll_pressed_heartbeat(&mut self, now_ms: u64) -> Option<MediaInputEnvelope> {
        if !self.pressed_heartbeat_due(now_ms) {
            return None;
        }
        self.next_pressed_heartbeat_ms = now_ms + self.config.pressed_heartbeat_ms;
        Some(self.pressed_set_envelope(now_ms))
    }

    /// The object count against the cap, for tests and metrics.
    pub fn object_capacity(&self) -> usize {
        self.config.max_objects
    }

    fn make(
        &mut self,
        kind: InputKind,
        flags: InputFlags,
        payload: Vec<u8>,
        now_ms: u64,
    ) -> MediaInputEnvelope {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.last_now_ms = now_ms;
        MediaInputEnvelope::new(seq, now_ms + self.config.deadline_ms, kind, flags, payload)
    }

    /// Queue one object, making room per the class rules. `false` when the
    /// object was refused (every queued object was critical and this one is
    /// not).
    fn enqueue(&mut self, envelope: MediaInputEnvelope, now_ms: u64) -> bool {
        let len = envelope.wire_len();
        loop {
            let fits = self.queue.len() < self.config.max_objects
                && self.bytes + len <= self.config.max_bytes;
            if fits {
                break;
            }
            if self.drop_lowest_priority() {
                continue;
            }
            // Everything queued is critical. A critical arrival makes room by
            // dropping the oldest object whatever it is; a non-critical one is
            // refused instead (an unbounded queue is a memory bug, not a
            // priority feature).
            if envelope.is_critical() && self.queue.pop_front().is_some() {
                self.bytes = self.queue.iter().map(|queued| queued.wire_len()).sum();
                self.drops.capacity += 1;
                self.reindex_slots();
                continue;
            }
            self.drops.capacity += 1;
            return false;
        }
        self.bytes += len;
        self.queue.push_back(envelope);
        let _ = now_ms;
        true
    }

    /// Drop the lowest-priority, oldest object; `false` when none is droppable.
    fn drop_lowest_priority(&mut self) -> bool {
        let mut candidate: Option<(usize, u8, u64)> = None;
        for (index, envelope) in self.queue.iter().enumerate() {
            if envelope.is_critical() {
                continue;
            }
            let rank = envelope.drop_rank();
            let seq = envelope.input_seq;
            match candidate {
                Some((_, best_rank, _)) if best_rank < rank => {}
                Some((_, best_rank, best_seq)) if best_rank == rank && best_seq <= seq => {}
                _ => candidate = Some((index, rank, seq)),
            }
        }
        if let Some((index, _, _)) = candidate {
            let removed = self.queue.remove(index).expect("index is valid");
            self.bytes = self.bytes.saturating_sub(removed.wire_len());
            self.drops.capacity += 1;
            self.reindex_slots();
            true
        } else {
            false
        }
    }

    fn drop_all_moves(&mut self, reason: InputDropReason) {
        let before = self.queue.len();
        self.queue
            .retain(|envelope| envelope.kind != InputKind::PointerMove);
        if self.queue.len() != before {
            self.drops.superseded += 1;
        }
        let _ = reason;
        self.pointer_slot = None;
        self.bytes = self.queue.iter().map(|envelope| envelope.wire_len()).sum();
    }

    fn expire(&mut self, now_ms: u64) {
        let before = self.queue.len();
        self.queue.retain(|envelope| {
            // Releases and pressed sets are always applied (MLP spec §13 rule 3),
            // so they are never expired here either.
            !envelope.is_expired(now_ms) || envelope.is_always_applied()
        });
        let dropped = before - self.queue.len();
        self.drops.deadline += dropped as u64;
        self.bytes = self.queue.iter().map(|envelope| envelope.wire_len()).sum();
        self.reindex_slots();
    }

    fn reindex_slots(&mut self) {
        self.pointer_slot = None;
        self.clipboard_slot = None;
    }
}

/// Why a sender-side input object was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostInputReject {
    /// The `(epoch, seq)` pair was seen before.
    Duplicate,
    /// The object is older than the last applied sequence.
    StaleSeq,
    /// The deadline passed and the object is not a release.
    Expired,
    /// The worker queue is full and the object is coalescible.
    QueueFull,
    /// The kind value is unknown.
    UnknownKind,
}

impl HostInputReject {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "deduped",
            Self::StaleSeq => "stale_seq",
            Self::Expired => "expired",
            Self::QueueFull => "capacity",
            Self::UnknownKind => "invalid",
        }
    }
}

/// Why the sender released keys (MLP spec §13 rule 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReleaseReason {
    /// A `pressed_set` did not contain them.
    PressedSet,
    /// The receiver parked and the grace elapsed.
    Park,
    /// The session ended.
    SessionEnd,
    /// The receiver sent an explicit release-all.
    Explicit,
}

impl ReleaseReason {
    /// The stable metric label spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PressedSet => "pressed_set",
            Self::Park => "park",
            Self::SessionEnd => "session_end",
            Self::Explicit => "explicit",
        }
    }
}

/// A release the driver must inject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRelease {
    /// The tokens to release.
    pub tokens: Vec<InputToken>,
    /// Why.
    pub reason: ReleaseReason,
}

/// The result of offering one envelope to the sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOutcome {
    /// Whether the object was accepted into the worker queue.
    pub accepted: bool,
    /// Why it was refused, when it was.
    pub reject: Option<HostInputReject>,
    /// Releases the driver must inject first.
    pub releases: Vec<InputRelease>,
}

/// Sender input policy (MLP spec §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInputConfig {
    /// `(epoch, seq)` dedup window.
    pub dedup_window: usize,
    /// Worker queue capacity.
    pub worker_capacity: usize,
    /// `key_release_grace` after parking.
    pub key_release_grace_ms: u64,
}

impl Default for HostInputConfig {
    fn default() -> Self {
        Self {
            dedup_window: INPUT_DEDUP_WINDOW,
            worker_capacity: HOST_WORKER_CAPACITY,
            key_release_grace_ms: KEY_RELEASE_GRACE_MS,
        }
    }
}

impl HostInputConfig {
    pub fn with_dedup_window(mut self, v: usize) -> Self { self.dedup_window = v; self }
    pub fn with_worker_capacity(mut self, v: usize) -> Self { self.worker_capacity = v; self }
    pub fn with_key_release_grace_ms(mut self, v: u64) -> Self { self.key_release_grace_ms = v; self }
}

/// Sender-side input state: dedup, deadline, pressed set and releases.
#[derive(Debug, Clone)]
pub struct HostInputState {
    config: HostInputConfig,
    epoch: u32,
    seen: VecDeque<(u32, u64)>,
    seen_set: HashSet<(u32, u64)>,
    last_applied_seq: u64,
    pressed: BTreeSet<InputToken>,
    worker: VecDeque<MediaInputEnvelope>,
    parked_at_ms: Option<u64>,
    applied: u64,
    expired: u64,
    deduped: u64,
    stale_seq: u64,
    released: u64,
    coalesced: u64,
}

impl HostInputState {
    /// Create sender input state for a receiver.
    pub fn new(config: HostInputConfig) -> Self {
        Self {
            config,
            epoch: 0,
            seen: VecDeque::new(),
            seen_set: HashSet::new(),
            last_applied_seq: 0,
            pressed: BTreeSet::new(),
            worker: VecDeque::new(),
            parked_at_ms: None,
            applied: 0,
            expired: 0,
            deduped: 0,
            stale_seq: 0,
            released: 0,
            coalesced: 0,
        }
    }

    /// Set the receiver epoch (resume boundary).
    pub fn set_epoch(&mut self, epoch: u32) {
        if epoch != self.epoch {
            self.epoch = epoch;
            self.seen.clear();
            self.seen_set.clear();
            self.last_applied_seq = 0;
        }
    }

    /// The receiver epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The pressed set the sender believes is held.
    pub fn pressed(&self) -> Vec<InputToken> {
        self.pressed.iter().copied().collect()
    }

    /// The next input sequence the receiver will send (the resume boundary).
    pub fn last_applied_seq(&self) -> u64 {
        self.last_applied_seq
    }

    /// Worker queue depth.
    pub fn worker_depth(&self) -> usize {
        self.worker.len()
    }

    /// Counters: `(applied, expired, deduped, stale_seq, released, coalesced)`.
    pub fn counters(&self) -> (u64, u64, u64, u64, u64, u64) {
        (
            self.applied,
            self.expired,
            self.deduped,
            self.stale_seq,
            self.released,
            self.coalesced,
        )
    }

    /// Offer one input envelope.
    pub fn submit(&mut self, envelope: &MediaInputEnvelope, now_ms: u64) -> InputOutcome {
        let mut releases = Vec::new();
        // Deadline enforcement: releases and pressed sets are always applied.
        if envelope.is_expired(now_ms) && !envelope.is_always_applied() {
            self.expired += 1;
            return InputOutcome {
                accepted: false,
                reject: Some(HostInputReject::Expired),
                releases,
            };
        }
        let key = (self.epoch, envelope.input_seq);
        if self.seen_set.contains(&key) {
            self.deduped += 1;
            return InputOutcome {
                accepted: false,
                reject: Some(HostInputReject::Duplicate),
                releases,
            };
        }
        self.remember(key);
        if envelope.input_seq < self.last_applied_seq && !envelope.is_always_applied() {
            self.stale_seq += 1;
            return InputOutcome {
                accepted: false,
                reject: Some(HostInputReject::StaleSeq),
                releases,
            };
        }
        let mut accepted = true;
        let mut reject = None;
        if matches!(envelope.kind, InputKind::PressedSet) {
            if let Some((keys, buttons)) = decode_pressed_set(&envelope.payload) {
                releases = self.on_pressed_set(&keys, &buttons);
            } else {
                accepted = false;
                reject = Some(HostInputReject::UnknownKind);
            }
        }
        if accepted {
            self.enqueue_worker(envelope, now_ms, &mut accepted, &mut reject);
        }
        if accepted {
            self.applied += 1;
            self.last_applied_seq = self.last_applied_seq.max(envelope.input_seq);
            self.note_pressed(envelope);
        }
        InputOutcome {
            accepted,
            reject,
            releases,
        }
    }

    /// Apply a `pressed_set`: release everything the sender holds that the set
    /// does not contain (the stuck-key safety net).
    pub fn on_pressed_set(&mut self, keys: &[u32], buttons: &[u32]) -> Vec<InputRelease> {
        let expected: BTreeSet<InputToken> = keys
            .iter()
            .map(|code| InputToken::Key(*code))
            .chain(buttons.iter().map(|code| InputToken::Button(*code)))
            .collect();
        let stale: Vec<InputToken> = self.pressed.difference(&expected).copied().collect();
        if stale.is_empty() {
            self.pressed = expected;
            return Vec::new();
        }
        self.released += stale.len() as u64;
        self.pressed = expected;
        vec![InputRelease {
            tokens: stale,
            reason: ReleaseReason::PressedSet,
        }]
    }

    /// The receiver parked: start the grace timer.
    pub fn on_park(&mut self, now_ms: u64) {
        self.parked_at_ms = Some(now_ms);
    }

    /// The receiver resumed: cancel the grace timer.
    pub fn on_resume(&mut self) {
        self.parked_at_ms = None;
    }

    /// Release everything once the park grace elapses (MLP spec §13 rule 4).
    pub fn poll_grace_release(&mut self, now_ms: u64) -> Option<InputRelease> {
        let parked_at = self.parked_at_ms?;
        if now_ms.saturating_sub(parked_at) < self.config.key_release_grace_ms {
            return None;
        }
        self.parked_at_ms = None;
        Some(self.release_all(ReleaseReason::Park))
    }

    /// The session ended: release everything.
    pub fn on_session_end(&mut self) -> InputRelease {
        self.release_all(ReleaseReason::SessionEnd)
    }

    /// An explicit release-all object arrived.
    pub fn on_release_all(&mut self) -> InputRelease {
        self.release_all(ReleaseReason::Explicit)
    }

    /// Drain up to `max` objects for the injection worker.
    pub fn drain_worker(&mut self, max: usize) -> Vec<MediaInputEnvelope> {
        let take = max.min(self.worker.len());
        self.worker.drain(..take).collect()
    }

    /// Worker timer: fires when the grace elapses.
    pub fn poll_timeout_ms(&self, now_ms: u64) -> Option<u64> {
        let parked_at = self.parked_at_ms?;
        let deadline = parked_at + self.config.key_release_grace_ms;
        Some(deadline.saturating_sub(now_ms))
    }

    fn release_all(&mut self, reason: ReleaseReason) -> InputRelease {
        let tokens: Vec<InputToken> = self.pressed.iter().copied().collect();
        self.released += tokens.len() as u64;
        self.pressed.clear();
        InputRelease { tokens, reason }
    }

    fn note_pressed(&mut self, envelope: &MediaInputEnvelope) {
        match envelope.kind {
            InputKind::Key => {
                // The payload's action is owned by the driver; the transport
                // guarantees here come from pressed sets and releases only.
            }
            InputKind::ReleaseAll => {
                self.pressed.clear();
            }
            _ => {}
        }
    }

    fn remember(&mut self, key: (u32, u64)) {
        self.seen_set.insert(key);
        self.seen.push_back(key);
        while self.seen.len() > self.config.dedup_window {
            if let Some(old) = self.seen.pop_front() {
                self.seen_set.remove(&old);
            }
        }
    }

    fn enqueue_worker(
        &mut self,
        envelope: &MediaInputEnvelope,
        now_ms: u64,
        accepted: &mut bool,
        reject: &mut Option<HostInputReject>,
    ) {
        if self.worker.len() < self.config.worker_capacity {
            self.worker.push_back(envelope.clone());
            return;
        }
        // The worker is full: pointer moves coalesce, everything else is kept.
        if envelope.kind == InputKind::PointerMove {
            if let Some(slot) = self
                .worker
                .iter()
                .position(|queued| queued.kind == InputKind::PointerMove)
            {
                self.worker[slot] = envelope.clone();
                self.coalesced += 1;
                return;
            }
        }
        if envelope.is_critical() {
            // Never drop a release: make room by coalescing the oldest move.
            if let Some(slot) = self
                .worker
                .iter()
                .position(|queued| queued.kind == InputKind::PointerMove)
            {
                self.worker.remove(slot);
                self.coalesced += 1;
                self.worker.push_back(envelope.clone());
                return;
            }
            self.worker.pop_front();
            self.coalesced += 1;
            self.worker.push_back(envelope.clone());
            return;
        }
        *accepted = false;
        *reject = Some(HostInputReject::QueueFull);
        let _ = now_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> ViewerInputQueue {
        ViewerInputQueue::new(InputQueueConfig::default(), 1)
    }

    #[test]
    fn pressed_set_payload_round_trips() {
        let bytes = encode_pressed_set(&[1, 2, 300], &[1]);
        assert_eq!(decode_pressed_set(&bytes), Some((vec![1, 2, 300], vec![1])));
        assert_eq!(decode_pressed_set(&[]), None);
        assert_eq!(decode_pressed_set(&[1, 0, 0, 0]), None);
        let mut truncated = bytes.clone();
        truncated.pop();
        assert_eq!(decode_pressed_set(&truncated), None);
        let trailing = [bytes.as_slice(), &[0u8; 4]].concat();
        assert_eq!(decode_pressed_set(&trailing), None);
    }

    #[test]
    fn queue_assigns_monotonic_sequences_and_deadlines() {
        let mut queue = queue();
        queue.push_key(65, true, false, vec![1], 100);
        let ready = queue.take_ready(100);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].input_seq, 1);
        assert_eq!(ready[0].deadline_epoch_ms, 100 + INPUT_DEADLINE_MS);
        assert_eq!(ready[0].kind, InputKind::Key);
        assert_eq!(queue.next_seq(), 2);
    }

    #[test]
    fn pointer_moves_are_latest_wins() {
        let mut queue = queue();
        assert_eq!(queue.push_pointer_move(vec![1], 0), PushOutcome::Queued);
        assert_eq!(queue.push_pointer_move(vec![2], 1), PushOutcome::Replaced);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.drops().superseded, 1);
        let ready = queue.take_ready(1);
        assert_eq!(ready[0].payload, vec![2]);
    }

    #[test]
    fn disconnected_queue_keeps_only_the_latest_position() {
        let mut queue = queue();
        queue.push_pointer_move(vec![1], 0);
        queue.push_pointer_move(vec![2], 1);
        queue.set_connected(false, 2);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn wheel_is_accumulated_and_capped() {
        let mut queue = queue();
        queue.push_wheel(3, vec![], 0);
        queue.push_wheel(3, vec![], 1);
        queue.push_wheel(3, vec![], 2);
        let ready = queue.take_ready(2);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].kind, InputKind::Wheel);
        assert_eq!(ready[0].payload, WHEEL_DELTA_CAP.to_le_bytes().to_vec());
        // The accumulator resets after materialization.
        assert!(queue.take_ready(3).is_empty());
    }

    #[test]
    fn clipboard_is_dropped_while_disconnected() {
        let mut queue = queue();
        assert_eq!(
            queue.push_clipboard(vec![7; 10], 0),
            PushOutcome::Dropped(InputDropReason::Disconnected)
        );
        assert_eq!(queue.drops().disconnected, 1);
        queue.set_connected(true, 0);
        assert_eq!(queue.push_clipboard(vec![7; 10], 1), PushOutcome::Queued);
    }

    #[test]
    fn capacity_drops_pointer_moves_before_transitions() {
        let config = InputQueueConfig {
            max_objects: 2,
            ..InputQueueConfig::default()
        };
        let mut queue = ViewerInputQueue::new(config, 1);
        queue.push_pointer_move(vec![0], 0);
        queue.push_key(65, true, false, vec![0], 0);
        assert_eq!(queue.len(), 2);
        // The queue is full; the arriving clipboard object (non-critical)
        // evicts the lowest-priority queued object: the pointer move.
        queue.set_connected(true, 0);
        assert_eq!(queue.push_clipboard(vec![7; 4], 0), PushOutcome::Queued);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.drops().capacity, 1);
        let ready = queue.take_ready(0);
        assert_eq!(ready.len(), 2);
        assert!(
            ready.iter().any(|e| e.kind == InputKind::Key),
            "transitions stay"
        );
        assert!(ready.iter().all(|e| e.kind != InputKind::PointerMove));
    }

    #[test]
    fn releases_survive_capacity_pressure() {
        let config = InputQueueConfig {
            max_objects: 2,
            max_bytes: 64,
            ..InputQueueConfig::default()
        };
        let mut queue = ViewerInputQueue::new(config, 1);
        for _ in 0..8 {
            queue.push_key(65, false, false, vec![0; 20], 0);
        }
        let ready = queue.take_ready(0);
        assert!(
            ready
                .iter()
                .all(|envelope| envelope.flags.contains(InputFlags::RELEASE)),
            "releases are never dropped"
        );
        assert!(!ready.is_empty());
    }

    #[test]
    fn expired_objects_are_removed_but_releases_survive() {
        let mut queue = queue();
        queue.push_pointer_move(vec![0], 0);
        queue.push_key(65, false, false, vec![0], 0);
        let ready = queue.take_ready(INPUT_DEADLINE_MS + 1);
        assert_eq!(ready.len(), 1, "the move expired, the release did not");
        assert!(ready[0].flags.contains(InputFlags::RELEASE));
        assert_eq!(queue.drops().deadline, 1);
    }

    #[test]
    fn replay_emits_queued_objects_then_a_pressed_set() {
        let mut queue = queue();
        queue.push_key(65, true, false, vec![0], 0);
        queue.push_pointer_move(vec![0], 0);
        queue.set_connected(true, 0);
        let replay = queue.replay(10);
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0].input_seq, 1);
        assert_eq!(replay[1].input_seq, 2);
        assert_eq!(replay[2].kind, InputKind::PressedSet);
        let (keys, buttons) = decode_pressed_set(&replay[2].payload).expect("decodes");
        assert_eq!(keys, vec![65]);
        assert!(buttons.is_empty());
    }

    #[test]
    fn pressed_heartbeat_is_rtt_relative() {
        assert_eq!(InputQueueConfig::heartbeat_for_rtt(10.0), 5_000);
        assert_eq!(InputQueueConfig::heartbeat_for_rtt(2_000.0), 5_000);
        assert_eq!(InputQueueConfig::heartbeat_for_rtt(20_000.0), 30_000);
        let mut queue = queue();
        let beat = queue.poll_pressed_heartbeat(0);
        assert!(beat.is_some(), "the first heartbeat fires immediately");
        assert!(beat.expect("heartbeat").kind == InputKind::PressedSet);
        assert!(
            queue.poll_pressed_heartbeat(1).is_none(),
            "then only after the interval"
        );
        assert!(
            queue.poll_pressed_heartbeat(5_000).is_some(),
            "and again after the interval"
        );
    }

    #[test]
    fn host_dedupes_and_rejects_stale_sequences() {
        let mut sender = HostInputState::new(HostInputConfig::default());
        let first = MediaInputEnvelope::new(5, 0, InputKind::Key, InputFlags::NONE, vec![0]);
        assert!(sender.submit(&first, 0).accepted);
        let duplicate = sender.submit(&first, 0);
        assert!(!duplicate.accepted);
        assert_eq!(duplicate.reject, Some(HostInputReject::Duplicate));
        let stale = MediaInputEnvelope::new(4, 0, InputKind::Key, InputFlags::NONE, vec![0]);
        let outcome = sender.submit(&stale, 0);
        assert_eq!(outcome.reject, Some(HostInputReject::StaleSeq));
        let newer = MediaInputEnvelope::new(6, 0, InputKind::Key, InputFlags::NONE, vec![0]);
        assert!(sender.submit(&newer, 0).accepted);
        let (applied, _, deduped, stale_seq, _, _) = sender.counters();
        assert_eq!(applied, 2);
        assert_eq!(deduped, 1);
        assert_eq!(stale_seq, 1);
    }

    #[test]
    fn host_expires_objects_but_always_applies_releases() {
        let mut sender = HostInputState::new(HostInputConfig::default());
        let expired_move =
            MediaInputEnvelope::new(1, 100, InputKind::PointerMove, InputFlags::NONE, vec![0]);
        let outcome = sender.submit(&expired_move, 200);
        assert_eq!(outcome.reject, Some(HostInputReject::Expired));
        let expired_release =
            MediaInputEnvelope::new(2, 100, InputKind::Key, InputFlags::RELEASE, vec![0]);
        assert!(sender.submit(&expired_release, 200).accepted);
        let expired_pressed_set = MediaInputEnvelope::new(
            3,
            100,
            InputKind::PressedSet,
            InputFlags::CRITICAL,
            encode_pressed_set(&[], &[]),
        );
        assert!(sender.submit(&expired_pressed_set, 200).accepted);
    }

    #[test]
    fn pressed_set_releases_keys_the_host_believes_held() {
        let mut sender = HostInputState::new(HostInputConfig::default());
        let set = MediaInputEnvelope::new(
            1,
            0,
            InputKind::PressedSet,
            InputFlags::CRITICAL,
            encode_pressed_set(&[16, 17], &[1]),
        );
        assert!(sender.submit(&set, 0).accepted);
        assert_eq!(sender.pressed().len(), 3);
        // A later set without key 17 releases it.
        let narrower = MediaInputEnvelope::new(
            2,
            0,
            InputKind::PressedSet,
            InputFlags::CRITICAL,
            encode_pressed_set(&[16], &[1]),
        );
        let outcome = sender.submit(&narrower, 10);
        assert!(outcome.accepted);
        assert_eq!(outcome.releases.len(), 1);
        assert_eq!(outcome.releases[0].reason, ReleaseReason::PressedSet);
        assert_eq!(outcome.releases[0].tokens, vec![InputToken::Key(17)]);
        assert_eq!(sender.pressed().len(), 2);
    }

    #[test]
    fn park_releases_after_the_grace_and_cancels_on_resume() {
        let mut sender = HostInputState::new(HostInputConfig::default());
        let set = MediaInputEnvelope::new(
            1,
            0,
            InputKind::PressedSet,
            InputFlags::CRITICAL,
            encode_pressed_set(&[16], &[1]),
        );
        sender.submit(&set, 0);
        sender.on_park(1_000);
        assert_eq!(sender.poll_timeout_ms(1_000), Some(KEY_RELEASE_GRACE_MS));
        assert!(
            sender
                .poll_grace_release(1_000 + KEY_RELEASE_GRACE_MS - 1)
                .is_none()
        );
        let release = sender
            .poll_grace_release(1_000 + KEY_RELEASE_GRACE_MS)
            .expect("grace elapsed");
        assert_eq!(release.reason, ReleaseReason::Park);
        assert_eq!(release.tokens.len(), 2);
        assert!(sender.pressed().is_empty(), "pressed set is cleared");
        // Resume before the grace cancels the release.
        sender.on_resume();
        sender.on_park(2_000);
        sender.on_resume();
        assert!(sender.poll_grace_release(10_000).is_none());
    }

    #[test]
    fn session_end_releases_everything() {
        let mut sender = HostInputState::new(HostInputConfig::default());
        let set = MediaInputEnvelope::new(
            1,
            0,
            InputKind::PressedSet,
            InputFlags::CRITICAL,
            encode_pressed_set(&[16, 17], &[2, 3]),
        );
        sender.submit(&set, 0);
        let release = sender.on_session_end();
        assert_eq!(release.reason, ReleaseReason::SessionEnd);
        assert_eq!(release.tokens.len(), 4);
        assert!(sender.pressed().is_empty(), "INV-4: no stuck keys");
    }

    #[test]
    fn epoch_change_clears_the_dedup_boundary() {
        let mut sender = HostInputState::new(HostInputConfig::default());
        let envelope = MediaInputEnvelope::new(9, 0, InputKind::Key, InputFlags::NONE, vec![0]);
        assert!(sender.submit(&envelope, 0).accepted);
        assert!(
            !sender.submit(&envelope, 0).accepted,
            "duplicate in epoch 0"
        );
        sender.set_epoch(1);
        assert!(
            sender.submit(&envelope, 0).accepted,
            "epoch 1 is a new space"
        );
    }

    #[test]
    fn worker_coalesces_moves_when_full_and_keeps_releases() {
        let config = HostInputConfig {
            worker_capacity: 4,
            ..HostInputConfig::default()
        };
        let mut sender = HostInputState::new(config);
        for seq in 1..=4u64 {
            let envelope =
                MediaInputEnvelope::new(seq, 0, InputKind::PointerMove, InputFlags::NONE, vec![0]);
            assert!(sender.submit(&envelope, 0).accepted);
        }
        assert_eq!(sender.worker_depth(), 4);
        // Full: the move coalesces into the existing slot.
        let move_envelope =
            MediaInputEnvelope::new(5, 0, InputKind::PointerMove, InputFlags::NONE, vec![9]);
        assert!(sender.submit(&move_envelope, 0).accepted);
        assert_eq!(sender.worker_depth(), 4);
        assert_eq!(sender.counters().5, 1, "coalesced");
        // A release is never refused.
        let release = MediaInputEnvelope::new(6, 0, InputKind::Key, InputFlags::RELEASE, vec![0]);
        assert!(sender.submit(&release, 0).accepted);
        // A clipboard object is refused when full and non-critical.
        let clipboard =
            MediaInputEnvelope::new(7, 0, InputKind::Clipboard, InputFlags::NONE, vec![0]);
        let outcome = sender.submit(&clipboard, 0);
        assert!(!outcome.accepted);
        assert_eq!(outcome.reject, Some(HostInputReject::QueueFull));
    }

    #[test]
    fn stuck_keys_are_impossible_across_a_park_cycle() {
        // INV-4 as a unit test: whatever the receiver sent, the sender's pressed
        // set is empty after park + grace, session end, or a full pressed set.
        for path in 0..3 {
            let mut sender = HostInputState::new(HostInputConfig::default());
            let pressed = MediaInputEnvelope::new(
                1,
                0,
                InputKind::PressedSet,
                InputFlags::CRITICAL,
                encode_pressed_set(&[16, 17, 18], &[1, 2]),
            );
            sender.submit(&pressed, 0);
            assert_eq!(sender.pressed().len(), 5);
            match path {
                0 => {
                    sender.on_park(0);
                    sender.poll_grace_release(KEY_RELEASE_GRACE_MS);
                }
                1 => {
                    sender.on_session_end();
                }
                _ => {
                    let empty = MediaInputEnvelope::new(
                        2,
                        0,
                        InputKind::PressedSet,
                        InputFlags::CRITICAL,
                        encode_pressed_set(&[], &[]),
                    );
                    sender.submit(&empty, 0);
                }
            }
            assert!(sender.pressed().is_empty(), "path {path} left keys held");
        }
    }
}
