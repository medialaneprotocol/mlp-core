//! MLP object assembly (MLP spec §6.4) — the receiver-side lifecycle.
//!
//! An object is complete only when **every** fragment index arrived *and* the
//! `FIN` flag was seen. Partial objects are never delivered (MLP spec annex INV, INV-1), are
//! bounded by `MAX_OPEN_OBJECTS`/`MAX_ASSEMBLY_BYTES`, and expire after the
//! RTT-relative assembly timeout.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::error::RejectReason;
use super::pdu::{
    Carrier, Fragment, FragmentClass, FragmentFlags, HeaderExt, Priority, UpdateMeta,
};

/// Bound on simultaneously open objects per lane (MLP spec §6.4).
pub const MAX_OPEN_OBJECTS: usize = 4;
/// Bound on the bytes held by incomplete objects per lane (MLP spec §6.4).
pub const MAX_ASSEMBLY_BYTES: usize = 1024 * 1024;
/// Size of the `(object_id, fragment_index)` dedupe window (MLP spec §6.4 step 5).
pub const DEDUP_WINDOW: usize = 4096;

/// Assembly tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssemblyConfig {
    /// Maximum simultaneously open objects.
    pub max_open_objects: usize,
    /// Maximum bytes held by incomplete objects.
    pub max_assembly_bytes: usize,
    /// `(object_id, fragment_index)` dedupe window size.
    pub dedup_window: usize,
    /// `ASSEMBLY_TIMEOUT = 3 × RTT + 500 ms`, clamped by the driver.
    pub assembly_timeout: Duration,
}

impl Default for AssemblyConfig {
    fn default() -> Self {
        Self {
            max_open_objects: MAX_OPEN_OBJECTS,
            max_assembly_bytes: MAX_ASSEMBLY_BYTES,
            dedup_window: DEDUP_WINDOW,
            assembly_timeout: Duration::from_millis(3_500),
        }
    }
}

impl AssemblyConfig {
    pub fn with_max_open_objects(mut self, v: usize) -> Self {
        self.max_open_objects = v;
        self
    }
    pub fn with_max_assembly_bytes(mut self, v: usize) -> Self {
        self.max_assembly_bytes = v;
        self
    }
    pub fn with_dedup_window(mut self, v: usize) -> Self {
        self.dedup_window = v;
        self
    }
    pub fn with_assembly_timeout(mut self, v: Duration) -> Self {
        self.assembly_timeout = v;
        self
    }
}

/// A reassembled media-lane object.
///
/// The two `Instant`s are the machine's injected time; they are never
/// serialized (the corpus carries decisions, not machine internals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaObject {
    /// Transport class.
    pub class: FragmentClass,
    /// Flags of the object's fragments.
    pub flags: FragmentFlags,
    /// Priority field.
    pub priority: Priority,
    /// Monotonic object id.
    pub object_id: u32,
    /// Header extension carried by fragment index 0.
    pub ext: HeaderExt,
    /// Concatenated payload.
    pub payload: Vec<u8>,
    /// When the first fragment arrived.
    pub first_seen: Instant,
    /// When the object completed.
    pub completed_at: Instant,
}

impl MediaObject {
    /// Total wire bytes of the object's payload.
    pub fn byte_len(&self) -> usize {
        self.payload.len()
    }

    /// The update metadata carried by the object, when it is a media object.
    pub fn update_meta(&self) -> Option<UpdateMeta> {
        UpdateMeta::from_ext(&self.ext)
    }

    /// Whether the object is a deliberate duplicate.
    pub fn is_duplicate(&self) -> bool {
        self.flags.contains(FragmentFlags::DUP)
    }

    /// Whether the object is an anchor (may apply onto any held state).
    pub fn is_anchor(&self) -> bool {
        self.flags.contains(FragmentFlags::ANCHOR)
    }

    /// The `ECHO_OF_OBJECT_ID` value, when present.
    pub fn echo_of(&self) -> Option<u32> {
        self.ext.echo_of_object_id()
    }

    /// The `DEADLINE_EPOCH_MS` value, when present.
    pub fn deadline_epoch_ms(&self) -> Option<u64> {
        self.ext.deadline_epoch_ms()
    }

    /// Whether this object is a media update chunk.
    pub fn is_media(&self) -> bool {
        self.class == FragmentClass::Media
    }

    /// Whether the object must be drained before queued media.
    pub fn is_urgent(&self) -> bool {
        self.flags.contains(FragmentFlags::URGENT)
    }
}

/// The result of feeding one fragment to the assembler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyOutcome {
    /// The fragment completed an object.
    Object(MediaObject),
    /// The fragment joined an open object.
    Pending,
    /// The fragment was rejected or dropped.
    Dropped {
        /// Why.
        reason: RejectReason,
        /// The object the fragment belonged to, when the id was parsed.
        object_id: Option<u32>,
    },
}

/// An incomplete object that left the assembly table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvictedObject {
    /// The object id.
    pub object_id: u32,
    /// `ASSEMBLY_TIMEOUT` or `ASSEMBLY_EVICTED`.
    pub reason: RejectReason,
    /// Bytes held for the object when it left.
    pub bytes: usize,
    /// Fragments received before it left.
    pub received: u16,
    /// Fragments the object declared.
    pub declared: u16,
}

#[derive(Debug, Clone)]
struct OpenObject {
    object_id: u32,
    class: FragmentClass,
    flags: FragmentFlags,
    priority: Priority,
    declared: u16,
    received: u16,
    fin_seen: bool,
    ext: HeaderExt,
    parts: Vec<Option<Vec<u8>>>,
    bytes: usize,
    first_seen: Instant,
}

impl OpenObject {
    fn complete(&self) -> bool {
        self.fin_seen && self.received == self.declared
    }

    fn into_object(self, completed_at: Instant) -> MediaObject {
        let mut payload = Vec::with_capacity(self.bytes);
        for bytes in self.parts.into_iter().flatten() {
            payload.extend_from_slice(&bytes);
        }
        MediaObject {
            class: self.class,
            flags: self.flags,
            priority: self.priority,
            object_id: self.object_id,
            ext: self.ext,
            payload,
            first_seen: self.first_seen,
            completed_at,
        }
    }
}

/// FIFO window over `(object_id, fragment_index)` pairs.
#[derive(Debug, Clone)]
struct FragmentDedupe {
    order: VecDeque<(u32, u16)>,
    seen: HashSet<(u32, u16)>,
    capacity: usize,
}

impl FragmentDedupe {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            seen: HashSet::new(),
            capacity: capacity.max(1),
        }
    }

    /// Record a fragment; `true` when it was already seen.
    fn check_and_insert(&mut self, key: (u32, u16)) -> bool {
        if !self.seen.insert(key) {
            return true;
        }
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        false
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

/// Receiver-side object assembler: one per lane and direction.
#[derive(Debug, Clone)]
pub struct ObjectAssembler {
    config: AssemblyConfig,
    open: Vec<OpenObject>,
    dedupe: FragmentDedupe,
    pending_evictions: Vec<EvictedObject>,
    expired_total: u64,
    evicted_total: u64,
    duplicate_fragments_total: u64,
}

impl ObjectAssembler {
    /// Create an assembler with the given bounds.
    pub fn new(config: AssemblyConfig) -> Self {
        Self {
            dedupe: FragmentDedupe::new(config.dedup_window),
            config,
            open: Vec::new(),
            pending_evictions: Vec::new(),
            expired_total: 0,
            evicted_total: 0,
            duplicate_fragments_total: 0,
        }
    }

    /// Update the RTT-relative assembly timeout.
    pub fn set_assembly_timeout(&mut self, timeout: Duration) {
        self.config.assembly_timeout = timeout;
    }

    /// The configured assembly timeout.
    pub fn assembly_timeout(&self) -> Duration {
        self.config.assembly_timeout
    }

    /// Number of incomplete objects currently held.
    pub fn open_objects(&self) -> usize {
        self.open.len()
    }

    /// Bytes held by incomplete objects.
    pub fn open_bytes(&self) -> usize {
        self.open.iter().map(|o| o.bytes).sum()
    }

    /// Objects dropped after the assembly timeout.
    pub fn expired_total(&self) -> u64 {
        self.expired_total
    }

    /// Objects evicted to honour the open-object budget.
    pub fn evicted_total(&self) -> u64 {
        self.evicted_total
    }

    /// Duplicate fragments dropped by the dedupe window.
    pub fn duplicate_fragments_total(&self) -> u64 {
        self.duplicate_fragments_total
    }

    /// The number of remembered `(object_id, fragment_index)` pairs.
    pub fn dedupe_window_len(&self) -> usize {
        self.dedupe.len()
    }

    /// Feed one parsed fragment.
    ///
    /// `session_ms` is the receiver's own session-relative millisecond clock
    /// (MLP spec §6.3: deadlines and echoes are session-relative, so no wall
    /// clock is ever read).
    pub fn accept(&mut self, fragment: Fragment, session_ms: u64, now: Instant) -> AssemblyOutcome {
        let object_id = fragment.header.object_id;

        // Step 4: an explicit deadline that has passed drops the object.
        if let Some(deadline) = fragment.ext.deadline_epoch_ms()
            && deadline != 0
            && session_ms > deadline
        {
            return AssemblyOutcome::Dropped {
                reason: RejectReason::Expired,
                object_id: Some(object_id),
            };
        }

        // Step 5: dedupe by (object_id, fragment_index).
        if self
            .dedupe
            .check_and_insert((object_id, fragment.header.fragment_index))
        {
            self.duplicate_fragments_total += 1;
            return AssemblyOutcome::Dropped {
                reason: RejectReason::Duplicate,
                object_id: Some(object_id),
            };
        }

        // Step 6: join (or open) the object's fragment set.
        let index = fragment.header.fragment_index as usize;
        let existing = self.open.iter().position(|o| o.object_id == object_id);
        let slot = match existing {
            Some(position) => {
                if self.open[position].declared != fragment.header.fragment_count
                    || self.open[position].class != fragment.header.class
                {
                    return AssemblyOutcome::Dropped {
                        reason: RejectReason::FragmentMismatch,
                        object_id: Some(object_id),
                    };
                }
                if self.open[position].parts[index].is_some() {
                    self.duplicate_fragments_total += 1;
                    return AssemblyOutcome::Dropped {
                        reason: RejectReason::Duplicate,
                        object_id: Some(object_id),
                    };
                }
                position
            }
            None => {
                if !self.make_room(fragment.payload.len()) {
                    return AssemblyOutcome::Dropped {
                        reason: RejectReason::AssemblyEvicted,
                        object_id: Some(object_id),
                    };
                }
                self.open.push(OpenObject {
                    object_id,
                    class: fragment.header.class,
                    flags: fragment.header.flags,
                    priority: fragment.header.priority,
                    declared: fragment.header.fragment_count,
                    received: 0,
                    fin_seen: false,
                    ext: HeaderExt::new(),
                    parts: (0..fragment.header.fragment_count as usize)
                        .map(|_| None)
                        .collect(),
                    bytes: 0,
                    first_seen: now,
                });
                self.open.len() - 1
            }
        };

        {
            let object = &mut self.open[slot];
            if index == 0 {
                object.ext = fragment.ext.clone();
            }
            object.bytes += fragment.payload.len();
            object.parts[index] = Some(fragment.payload);
            object.received += 1;
            if fragment.header.flags.contains(FragmentFlags::FIN) {
                object.fin_seen = true;
            }
        }

        if self.open[slot].complete() {
            let object = self.open.remove(slot);
            return AssemblyOutcome::Object(object.into_object(now));
        }
        AssemblyOutcome::Pending
    }

    /// Drop every object whose first fragment arrived more than
    /// `assembly_timeout` before `now`.
    pub fn expire(&mut self, now: Instant) -> Vec<EvictedObject> {
        let timeout = self.config.assembly_timeout;
        let mut expired = Vec::new();
        let mut remaining = Vec::with_capacity(self.open.len());
        let open = core::mem::take(&mut self.open);
        for object in open {
            if now.saturating_duration_since(object.first_seen) >= timeout {
                expired.push(EvictedObject {
                    object_id: object.object_id,
                    reason: RejectReason::AssemblyTimeout,
                    bytes: object.bytes,
                    received: object.received,
                    declared: object.declared,
                });
            } else {
                remaining.push(object);
            }
        }
        self.open = remaining;
        self.expired_total += expired.len() as u64;
        expired
    }

    /// Evictions that happened while making room, drained by the caller.
    pub fn take_evictions(&mut self) -> Vec<EvictedObject> {
        core::mem::take(&mut self.pending_evictions)
    }

    /// Duration until the earliest assembly timeout relative to `now`.
    pub fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        let timeout = self.config.assembly_timeout;
        self.open
            .iter()
            .map(|object| {
                let deadline = object.first_seen + timeout;
                deadline.saturating_duration_since(now)
            })
            .min()
    }

    /// Drop all open objects, for example on lane reset.
    pub fn clear(&mut self) {
        self.open.clear();
    }

    /// Ensure a new fragment of `incoming_bytes` fits the budgets.
    ///
    /// Evicts the oldest incomplete object when the object count or the byte
    /// budget would be exceeded (MLP spec §6.4). Returns `false` only when even
    /// an empty table cannot hold the fragment (an impossibility in practice:
    /// the fragment cap is 64 KiB and the byte budget 1 MiB).
    fn make_room(&mut self, incoming_bytes: usize) -> bool {
        if incoming_bytes > self.config.max_assembly_bytes {
            return false;
        }
        while !self.open.is_empty()
            && (self.open.len() >= self.config.max_open_objects
                || self.open_bytes() + incoming_bytes > self.config.max_assembly_bytes)
        {
            let oldest = self
                .open
                .iter()
                .enumerate()
                .min_by_key(|(_, object)| object.first_seen)
                .map(|(index, _)| index)
                .expect("open is not empty");
            let object = self.open.remove(oldest);
            self.evicted_total += 1;
            self.pending_evictions.push(EvictedObject {
                object_id: object.object_id,
                reason: RejectReason::AssemblyEvicted,
                bytes: object.bytes,
                received: object.received,
                declared: object.declared,
            });
        }
        true
    }

    /// Whether a fragment carrying `tlv::DEADLINE_EPOCH_MS` is already under
    /// the open-object budget.
    pub fn would_accept_open(&self, payload_bytes: usize) -> bool {
        if payload_bytes > self.config.max_assembly_bytes {
            return false;
        }
        self.open.len() < self.config.max_open_objects
            || self.open_bytes() + payload_bytes <= self.config.max_assembly_bytes
    }

    /// Debug helper: the class of an open object, when present.
    pub fn open_class(&self, object_id: u32) -> Option<FragmentClass> {
        self.open
            .iter()
            .find(|o| o.object_id == object_id)
            .map(|o| o.class)
    }

    /// Debug helper: how many fragments of an open object arrived.
    pub fn open_received(&self, object_id: u32) -> Option<(u16, u16, bool)> {
        self.open
            .iter()
            .find(|o| o.object_id == object_id)
            .map(|o| (o.received, o.declared, o.fin_seen))
    }

    /// Whether the fragment's class is a legal media-lane class for its fields.
    ///
    /// Media objects must carry `UPDATE_ID` metadata; a media object without it
    /// cannot be acked and is a body-invalid object (MLP spec §7.1).
    pub fn classify(object: &MediaObject) -> Result<(), RejectReason> {
        if object.class == FragmentClass::Media && object.ext.update_id().is_none() {
            return Err(RejectReason::BodyInvalid);
        }
        if object.class == FragmentClass::Media && object.priority.is_reserved_for_control() {
            return Err(RejectReason::BadPriority);
        }
        Ok(())
    }

    /// Whether a media object is a cursor-tagged object (never fragmented).
    pub fn is_cursor_too_large(object: &MediaObject) -> bool {
        object.class == FragmentClass::Cursor && object.payload.len() > 1_150
    }

    /// The `tlv::CELL_COUNT` hint of an object, when present.
    pub fn cell_count_hint(object: &MediaObject) -> Option<u32> {
        object.ext.cell_count()
    }

    /// The carrier a fragment claims to be for, as a debug aid.
    pub fn carrier_ok(carrier: Carrier, fragment: &Fragment) -> bool {
        !carrier.is_datagram() || fragment.header.fragment_count == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::pdu::{FragmentClass, FragmentFlags, Priority, tlv};

    fn fragment(
        object_id: u32,
        index: u16,
        count: u16,
        fin: bool,
        payload: Vec<u8>,
        ext: HeaderExt,
    ) -> Fragment {
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
            ext,
            payload,
        )
        .expect("valid fragment")
    }

    fn assemble_three() -> (ObjectAssembler, Instant) {
        let now = Instant::now();
        (ObjectAssembler::new(AssemblyConfig::default()), now)
    }

    #[test]
    fn object_completes_only_with_every_index_and_fin() {
        let (mut assembler, now) = assemble_three();
        assert_eq!(
            assembler.accept(fragment(5, 0, 3, false, vec![1], HeaderExt::new()), 0, now),
            AssemblyOutcome::Pending
        );
        assert_eq!(
            assembler.accept(fragment(5, 1, 3, false, vec![2], HeaderExt::new()), 0, now),
            AssemblyOutcome::Pending
        );
        // Indices complete but FIN is missing: the object must not be delivered.
        assert_eq!(
            assembler.accept(fragment(5, 2, 3, false, vec![3], HeaderExt::new()), 0, now),
            AssemblyOutcome::Pending
        );
        // FIN arrives as a duplicate index: the object is still incomplete.
        assert_eq!(
            assembler.accept(fragment(5, 2, 3, true, vec![3], HeaderExt::new()), 0, now),
            AssemblyOutcome::Dropped {
                reason: RejectReason::Duplicate,
                object_id: Some(5)
            }
        );
        assert_eq!(assembler.open_objects(), 1);
        assert_eq!(
            assembler.open_received(5),
            Some((3, 3, false)),
            "FIN must be required"
        );
    }

    #[test]
    fn object_reassembles_payload_in_index_order() {
        let (mut assembler, now) = assemble_three();
        assembler.accept(
            fragment(7, 2, 3, true, vec![3, 3], HeaderExt::new()),
            0,
            now,
        );
        assembler.accept(fragment(7, 0, 3, false, vec![1], HeaderExt::new()), 0, now);
        let outcome = assembler.accept(
            fragment(7, 1, 3, false, vec![2, 2], HeaderExt::new()),
            0,
            now,
        );
        match outcome {
            AssemblyOutcome::Object(object) => {
                assert_eq!(object.object_id, 7);
                assert_eq!(object.payload, vec![1, 2, 2, 3, 3]);
                assert!(!object.is_anchor());
            }
            other => panic!("expected a complete object, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_fragments_are_dropped() {
        let (mut assembler, now) = assemble_three();
        assembler.accept(fragment(9, 0, 2, false, vec![1], HeaderExt::new()), 0, now);
        assert_eq!(
            assembler.accept(fragment(9, 0, 2, false, vec![1], HeaderExt::new()), 0, now),
            AssemblyOutcome::Dropped {
                reason: RejectReason::Duplicate,
                object_id: Some(9)
            }
        );
        assert_eq!(assembler.duplicate_fragments_total(), 1);
        // A duplicate of a completed object is dropped too.
        let outcome = assembler.accept(fragment(9, 1, 2, true, vec![2], HeaderExt::new()), 0, now);
        assert!(matches!(outcome, AssemblyOutcome::Object(_)));
        assert_eq!(
            assembler.accept(fragment(9, 1, 2, true, vec![2], HeaderExt::new()), 0, now),
            AssemblyOutcome::Dropped {
                reason: RejectReason::Duplicate,
                object_id: Some(9)
            }
        );
    }

    #[test]
    fn dedupe_window_is_bounded() {
        let config = AssemblyConfig {
            dedup_window: 8,
            ..AssemblyConfig::default()
        };
        let mut assembler = ObjectAssembler::new(config);
        let now = Instant::now();
        for object_id in 0..32u32 {
            assembler.accept(
                fragment(object_id, 0, 2, false, vec![0], HeaderExt::new()),
                0,
                now,
            );
        }
        assert_eq!(assembler.dedupe_window_len(), 8);
    }

    #[test]
    fn fragment_count_mismatch_is_rejected() {
        let (mut assembler, now) = assemble_three();
        assembler.accept(fragment(11, 0, 3, false, vec![1], HeaderExt::new()), 0, now);
        assert_eq!(
            assembler.accept(fragment(11, 1, 2, false, vec![2], HeaderExt::new()), 0, now),
            AssemblyOutcome::Dropped {
                reason: RejectReason::FragmentMismatch,
                object_id: Some(11)
            }
        );
    }

    #[test]
    fn expired_deadline_drops_the_fragment() {
        let (mut assembler, now) = assemble_three();
        let mut ext = HeaderExt::new();
        ext.push_u64(tlv::DEADLINE_EPOCH_MS, 1_000).expect("fits");
        let outcome = assembler.accept(fragment(13, 0, 1, true, vec![1], ext.clone()), 1_001, now);
        assert_eq!(
            outcome,
            AssemblyOutcome::Dropped {
                reason: RejectReason::Expired,
                object_id: Some(13)
            }
        );
        // Zero means "never expires".
        ext = HeaderExt::new();
        ext.push_u64(tlv::DEADLINE_EPOCH_MS, 0).expect("fits");
        let outcome = assembler.accept(fragment(13, 0, 1, true, vec![1], ext), 10_000_000, now);
        assert!(matches!(outcome, AssemblyOutcome::Object(_)));
    }

    #[test]
    fn assembly_timeout_expires_the_oldest_object() {
        let (mut assembler, now) = assemble_three();
        assembler.accept(fragment(21, 0, 2, false, vec![1], HeaderExt::new()), 0, now);
        assembler.accept(
            fragment(22, 0, 2, false, vec![1], HeaderExt::new()),
            0,
            now + Duration::from_millis(1_000),
        );
        let expired = assembler.expire(now + Duration::from_millis(4_000));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].object_id, 21);
        assert_eq!(expired[0].reason, RejectReason::AssemblyTimeout);
        assert_eq!(assembler.expired_total(), 1);
        assert_eq!(assembler.open_objects(), 1);
    }

    #[test]
    fn open_object_budget_evicts_the_oldest() {
        let config = AssemblyConfig {
            max_open_objects: 2,
            ..AssemblyConfig::default()
        };
        let mut assembler = ObjectAssembler::new(config);
        let now = Instant::now();
        assembler.accept(fragment(1, 0, 2, false, vec![1], HeaderExt::new()), 0, now);
        assembler.accept(fragment(2, 0, 2, false, vec![1], HeaderExt::new()), 0, now);
        assembler.accept(fragment(3, 0, 2, false, vec![1], HeaderExt::new()), 0, now);
        let evictions = assembler.take_evictions();
        assert_eq!(evictions.len(), 1);
        assert_eq!(evictions[0].object_id, 1);
        assert_eq!(evictions[0].reason, RejectReason::AssemblyEvicted);
        assert_eq!(assembler.evicted_total(), 1);
        assert_eq!(assembler.open_objects(), 2);
    }

    #[test]
    fn assembly_bytes_budget_is_honoured() {
        let config = AssemblyConfig {
            max_assembly_bytes: 100,
            ..AssemblyConfig::default()
        };
        let mut assembler = ObjectAssembler::new(config);
        let now = Instant::now();
        assembler.accept(
            fragment(1, 0, 2, false, vec![0; 80], HeaderExt::new()),
            0,
            now,
        );
        assembler.accept(
            fragment(2, 0, 2, false, vec![0; 80], HeaderExt::new()),
            0,
            now,
        );
        assert_eq!(assembler.open_objects(), 1);
        assert!(assembler.open_bytes() <= 100);
    }

    #[test]
    fn poll_timeout_tracks_the_earliest_expiry() {
        let (mut assembler, now) = assemble_three();
        assert_eq!(assembler.poll_timeout(now), None);
        assembler.accept(fragment(31, 0, 2, false, vec![1], HeaderExt::new()), 0, now);
        let timeout = assembler.poll_timeout(now).expect("an object is open");
        assert_eq!(timeout, assembler.assembly_timeout());
        let later = now + Duration::from_millis(1_000);
        let remaining = assembler.poll_timeout(later).expect("still open");
        assert!(remaining < assembler.assembly_timeout());
    }

    #[test]
    fn media_object_helpers() {
        let (mut assembler, now) = assemble_three();
        let mut ext = HeaderExt::new();
        ext.push_u64(tlv::UPDATE_ID, 42).expect("fits");
        ext.push_u32(tlv::CELL_COUNT, 510).expect("fits");
        let outcome = assembler.accept(fragment(41, 0, 1, true, vec![9], ext), 0, now);
        match outcome {
            AssemblyOutcome::Object(object) => {
                let meta = object.update_meta().expect("update meta");
                assert_eq!(meta.update_id, 42);
                assert_eq!(ObjectAssembler::cell_count_hint(&object), Some(510));
                assert!(object.is_media());
                assert_eq!(ObjectAssembler::classify(&object), Ok(()));
                assert_eq!(object.byte_len(), 1);
            }
            other => panic!("expected object, got {other:?}"),
        }
    }

    #[test]
    fn media_without_update_id_is_body_invalid() {
        let (mut assembler, now) = assemble_three();
        let outcome = assembler.accept(fragment(43, 0, 1, true, vec![9], HeaderExt::new()), 0, now);
        match outcome {
            AssemblyOutcome::Object(object) => {
                assert_eq!(
                    ObjectAssembler::classify(&object),
                    Err(RejectReason::BodyInvalid)
                );
            }
            other => panic!("expected object, got {other:?}"),
        }
    }
}
