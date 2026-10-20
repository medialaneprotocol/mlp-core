//! The media-lane receiver machine (receiver → sender direction, MLP spec §6.4).
//!
//! [`MediaReceiverMachine`] turns carrier messages into complete objects,
//! enforces the transport rules (magic/version/class/flags/priority, the
//! carrier rule, deadlines, dedupe, assembly timeout, body classification),
//! and produces the receiver-side reports the specification requires: the selective
//! `MediaAck`, the `MediaFeedback` window counters, probe echoes and RTT
//! samples. Applying cells is the driver's job; idempotence by
//! `(update_id, part_index)` is provided here as a service.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::error::RejectReason;
use super::object::{AssemblyConfig, AssemblyOutcome, MediaObject, ObjectAssembler};
use super::pdu::{Carrier, FragmentClass, FragmentFlags};
use super::{MediaAck, MediaFeedback, MediaPath, MediaProbe, ProbeEcho};

/// Default report cadence floor (`clamp(4 × rtt_min, 250, 2 000)`).
pub const REPORT_INTERVAL_FLOOR_MS: u64 = 250;
/// Default report cadence ceiling.
pub const REPORT_INTERVAL_CEILING_MS: u64 = 2_000;
/// Cadence factor: `report_interval = 4 × rtt_min`.
pub const REPORT_INTERVAL_FACTOR: f64 = 4.0;
/// How many applied `(update_id, part_index)` pairs are remembered.
pub const APPLIED_CHUNK_WINDOW: usize = 512;

/// Receiver configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaReceiverConfig {
    /// The carrier messages arrive on.
    pub carrier: Carrier,
    /// Assembly bounds and timeout.
    pub assembly: AssemblyConfig,
    /// The receiver's own probe cadence (unused when the sender probes).
    pub probe_interval_ms: u64,
    /// Maximum probe payload.
    pub probe_payload_max: u32,
}

impl MediaReceiverConfig {
    /// A configuration for a carrier.
    pub fn for_carrier(carrier: Carrier) -> Self {
        Self {
            carrier,
            assembly: AssemblyConfig::default(),
            probe_interval_ms: 30_000,
            probe_payload_max: 8_192,
        }
    }
}

/// What the driver must act on.
#[derive(Debug, Clone, PartialEq)]
pub enum MediaReceiveEvent {
    /// A complete object.
    Object(MediaObject),
    /// A fragment was dropped before assembly completed.
    FragmentDropped {
        /// The object id, when it was parsed.
        object_id: Option<u32>,
        /// Why.
        reason: RejectReason,
    },
    /// An incomplete object left the assembly table.
    ObjectDropped {
        /// The object id.
        object_id: u32,
        /// `ASSEMBLY_TIMEOUT` or `ASSEMBLY_EVICTED`.
        reason: RejectReason,
    },
    /// The report cadence is due: build and send a `MediaFeedback`.
    FeedbackDue,
    /// Answer the peer's probe with this echo.
    ProbeEchoDue(ProbeEcho),
    /// Send our own probe to measure RTT.
    ProbeDue(MediaProbe),
}

/// Receiver counters for the receiver/sender metrics.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ReceiverStats {
    /// Transport messages accepted (fragments parsed).
    pub messages: u64,
    /// Fragments parsed.
    pub fragments: u64,
    /// Payload bytes delivered in complete objects.
    pub bytes: u64,
    /// Complete objects by class, indexed control/cursor/media/feedback/path.
    pub objects_by_class: [u64; 5],
    /// Fragments dropped by a transport rule.
    pub dropped_fragments: u64,
    /// Objects that left the assembly table incomplete.
    pub dropped_objects: u64,
    /// Duplicate fragments dropped by the dedupe window.
    pub duplicate_fragments: u64,
    /// Duplicate objects observed (`DUP` flag).
    pub duplicate_objects: u64,
    /// Objects that arrived out of order.
    pub reordered_objects: u64,
    /// Gaps in `object_id` observed (the receiver's loss signal).
    pub lost_objects: u64,
    /// Fragments dropped by an explicit deadline.
    pub expired: u64,
    /// Assembly timeouts.
    pub assembly_timeouts: u64,
    /// Assembly evictions.
    pub assembly_evictions: u64,
    /// Echo replies emitted for `ECHO_REQUESTED` objects.
    pub echo_replies: u64,
    /// Probes sent by this receiver.
    pub probes_sent: u64,
    /// Echoes received for our own probes.
    pub probe_echoes: u64,
    /// Feedback reports built.
    pub feedback_reports: u64,
    /// Updates the driver reported as applied.
    pub applied_frames: u64,
}

/// The receiver machine.
#[derive(Debug, Clone)]
pub struct MediaReceiverMachine {
    config: MediaReceiverConfig,
    assembler: ObjectAssembler,
    started: Instant,
    stats: ReceiverStats,
    expected_object_id: Option<u32>,
    /// Largest update epoch seen.
    epoch: u32,
    /// The sender's advertised pace (from the `PACE_BPS` TLV).
    advertised_pace_bps: Option<u64>,
    /// The sender's advertised budget (from the `BUDGET_BYTES` TLV).
    advertised_budget_bytes: Option<u32>,
    // Ack state.
    highest_applied: u64,
    bitmap: u64,
    applied_count: u32,
    stale_cells: u32,
    converged: bool,
    render_latency_ms: f64,
    echo_of_input_seq: u64,
    // Applied-chunk idempotence window.
    applied_chunks: VecDeque<(u64, u16)>,
    applied_set: HashSet<(u64, u16)>,
    // Report window counters.
    window_bytes: u64,
    window_objects: u64,
    window_lost: u64,
    window_duplicates: u64,
    window_reordered: u64,
    window_input_submitted: u64,
    window_input_expired: u64,
    last_report: Instant,
    // RTT state.
    rtt_ms: f64,
    rtt_min_ms: f64,
    jitter_ms: f64,
    rtt_last_ms: f64,
    pending_probes: VecDeque<(u32, u64)>,
    next_probe_id: u32,
    last_probe: Option<Instant>,
    pending_echoes: Vec<ProbeEcho>,
    // Driver-supplied diagnostics.
    display_fps: u32,
    render_ms_p50: u32,
    render_ms_p95: u32,
    input_queue_depth: u32,
    buffered_amount_bytes: u32,
    path_switches: u64,
    active_path: MediaPath,
    app_version: String,
}

impl MediaReceiverMachine {
    /// Create a receiver.
    pub fn new(config: MediaReceiverConfig, now: Instant) -> Self {
        Self {
            assembler: ObjectAssembler::new(config.assembly),
            config,
            started: now,
            stats: ReceiverStats::default(),
            expected_object_id: None,
            epoch: 0,
            advertised_pace_bps: None,
            advertised_budget_bytes: None,
            highest_applied: 0,
            bitmap: 0,
            applied_count: 0,
            stale_cells: 0,
            converged: false,
            render_latency_ms: 0.0,
            echo_of_input_seq: 0,
            applied_chunks: VecDeque::new(),
            applied_set: HashSet::new(),
            window_bytes: 0,
            window_objects: 0,
            window_lost: 0,
            window_duplicates: 0,
            window_reordered: 0,
            window_input_submitted: 0,
            window_input_expired: 0,
            last_report: now,
            rtt_ms: 0.0,
            rtt_min_ms: 0.0,
            jitter_ms: 0.0,
            rtt_last_ms: 0.0,
            pending_probes: VecDeque::new(),
            next_probe_id: 1,
            last_probe: None,
            pending_echoes: Vec::new(),
            display_fps: 0,
            render_ms_p50: 0,
            render_ms_p95: 0,
            input_queue_depth: 0,
            buffered_amount_bytes: 0,
            path_switches: 0,
            active_path: MediaPath::Unknown,
            app_version: String::new(),
        }
    }

    /// The carrier.
    pub fn carrier(&self) -> Carrier {
        self.config.carrier
    }

    /// Counters.
    pub fn stats(&self) -> &ReceiverStats {
        &self.stats
    }

    /// The assembly table, for diagnostics.
    pub fn assembler(&self) -> &ObjectAssembler {
        &self.assembler
    }

    /// The largest update epoch seen.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The sender's advertised pace, when media objects carried it.
    pub fn advertised_pace_bps(&self) -> Option<u64> {
        self.advertised_pace_bps
    }

    /// The sender's advertised budget, when media objects carried it.
    pub fn advertised_budget_bytes(&self) -> Option<u32> {
        self.advertised_budget_bytes
    }

    /// The session-relative millisecond clock.
    pub fn session_ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_millis() as u64
    }

    /// The current RTT estimate.
    pub fn rtt_ms(&self) -> f64 {
        self.rtt_ms
    }

    /// The RTT minimum over this session.
    pub fn rtt_min_ms(&self) -> f64 {
        self.rtt_min_ms
    }

    /// The report cadence: `clamp(4 × rtt_min, 250, 2 000)`.
    pub fn report_interval(&self) -> Duration {
        let rtt_min = if self.rtt_min_ms > 0.0 {
            self.rtt_min_ms
        } else {
            REPORT_INTERVAL_FLOOR_MS as f64 / REPORT_INTERVAL_FACTOR
        };
        let interval = (REPORT_INTERVAL_FACTOR * rtt_min).clamp(
            REPORT_INTERVAL_FLOOR_MS as f64,
            REPORT_INTERVAL_CEILING_MS as f64,
        );
        Duration::from_secs_f64(interval / 1_000.0)
    }

    /// Whether the report is due.
    pub fn feedback_due(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_report) >= self.report_interval()
    }

    /// Parse and assemble one carrier message.
    ///
    /// On stream carriers the message may hold 1..N fragments; on datagram
    /// carriers exactly one (MLP spec §6.1).
    pub fn on_message(&mut self, bytes: &[u8], now: Instant) -> Vec<MediaReceiveEvent> {
        let fragments = match super::pdu::parse_message(bytes, self.config.carrier) {
            Ok(fragments) => fragments,
            Err(reason) => {
                self.stats.dropped_fragments += 1;
                return vec![MediaReceiveEvent::FragmentDropped {
                    object_id: None,
                    reason,
                }];
            }
        };
        self.stats.messages += 1;
        let session_ms = self.session_ms(now);
        let mut events = Vec::new();
        for fragment in fragments {
            self.stats.fragments += 1;
            let object_id = fragment.header.object_id;
            let first_of_object = fragment.header.fragment_index == 0;
            let was_known = self.assembler.open_class(object_id).is_some();
            // Track the advertised diagnostics before assembly (they arrive on
            // the first fragment of a media object).
            if let Some(pace) = fragment.ext.pace_bps() {
                self.advertised_pace_bps = Some(u64::from(pace) * 1_000);
            }
            if let Some(budget) = fragment.ext.budget_bytes() {
                self.advertised_budget_bytes = Some(budget);
            }
            match self.assembler.accept(fragment, session_ms, now) {
                AssemblyOutcome::Pending => {
                    if first_of_object && !was_known {
                        self.note_new_object(object_id);
                    }
                }
                AssemblyOutcome::Object(object) => {
                    if first_of_object && !was_known {
                        self.note_new_object(object_id);
                    }
                    if object.ext.chain_epoch().unwrap_or(self.epoch) > self.epoch {
                        self.epoch = object.ext.chain_epoch().unwrap_or(self.epoch);
                    }
                    if object.class == FragmentClass::Media {
                        let epoch = object.update_meta().map(|meta| meta.chain_epoch);
                        if let Some(epoch) = epoch {
                            self.epoch = self.epoch.max(epoch);
                        }
                    }
                    if object.is_duplicate() {
                        self.stats.duplicate_objects += 1;
                        self.window_duplicates += 1;
                    }
                    if object.flags.contains(FragmentFlags::ECHO_REQUESTED) {
                        let echo = ProbeEcho {
                            object_id: object.object_id,
                            send_epoch_ms: object.ext.send_epoch_ms().unwrap_or(0),
                            echo_epoch_ms: session_ms,
                            payload_bytes: object.payload.len() as u32,
                        };
                        self.stats.echo_replies += 1;
                        events.push(MediaReceiveEvent::ProbeEchoDue(echo));
                    }
                    match ObjectAssembler::classify(&object) {
                        Ok(()) => {
                            self.stats.bytes += object.payload.len() as u64;
                            self.window_bytes += object.payload.len() as u64;
                            self.stats.objects_by_class[class_slot(object.class)] += 1;
                            self.window_objects += 1;
                            events.push(MediaReceiveEvent::Object(object));
                        }
                        Err(reason) => {
                            self.stats.dropped_objects += 1;
                            events.push(MediaReceiveEvent::FragmentDropped {
                                object_id: Some(object.object_id),
                                reason,
                            });
                        }
                    }
                }
                AssemblyOutcome::Dropped { reason, object_id } => {
                    match reason {
                        RejectReason::Duplicate => {
                            self.stats.duplicate_fragments += 1;
                        }
                        RejectReason::Expired => {
                            self.stats.expired += 1;
                            self.stats.dropped_fragments += 1;
                        }
                        _ => {
                            self.stats.dropped_fragments += 1;
                        }
                    }
                    events.push(MediaReceiveEvent::FragmentDropped { object_id, reason });
                }
            }
            for evicted in self.assembler.take_evictions() {
                self.stats.assembly_evictions += 1;
                self.stats.dropped_objects += 1;
                events.push(MediaReceiveEvent::ObjectDropped {
                    object_id: evicted.object_id,
                    reason: evicted.reason,
                });
            }
        }
        events
    }

    /// Alias used by the S2 datagram adapters.
    pub fn on_datagram(&mut self, bytes: &[u8], now: Instant) -> Vec<MediaReceiveEvent> {
        self.on_message(bytes, now)
    }

    /// Expire assembly timers and emit the report/probe obligations.
    pub fn tick(&mut self, now: Instant) -> Vec<MediaReceiveEvent> {
        let mut events = Vec::new();
        for expired in self.assembler.expire(now) {
            self.stats.assembly_timeouts += 1;
            self.stats.dropped_objects += 1;
            events.push(MediaReceiveEvent::ObjectDropped {
                object_id: expired.object_id,
                reason: expired.reason,
            });
        }
        if self.feedback_due(now) {
            events.push(MediaReceiveEvent::FeedbackDue);
        }
        if self.probe_due(now) {
            let payload_bytes = self.probe_payload_bytes();
            let probe = MediaProbe {
                probe_id: self.next_probe_id,
                payload_bytes,
                send_epoch_ms: self.session_ms(now),
            };
            self.next_probe_id = self.next_probe_id.wrapping_add(1).max(1);
            self.pending_probes
                .push_back((probe.probe_id, probe.send_epoch_ms));
            self.last_probe = Some(now);
            self.stats.probes_sent += 1;
            events.push(MediaReceiveEvent::ProbeDue(probe));
        }
        events
    }

    /// The duration until the next receiver obligation.
    pub fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        let mut next: Option<Duration> = None;
        let mut consider = |candidate: Duration| {
            next = Some(match next {
                Some(current) if current <= candidate => current,
                _ => candidate,
            });
        };
        if let Some(assembly) = self.assembler.poll_timeout(now) {
            consider(assembly);
        }
        let report_deadline = self.last_report + self.report_interval();
        consider(report_deadline.saturating_duration_since(now));
        if let Some(last) = self.last_probe {
            let deadline = last + Duration::from_millis(self.config.probe_interval_ms);
            consider(deadline.saturating_duration_since(now));
        } else {
            consider(Duration::ZERO);
        }
        next
    }

    /// Answer an echo for one of our probes.
    pub fn on_probe_echo(&mut self, echo: ProbeEcho, _now: Instant) {
        self.stats.probe_echoes += 1;
        if echo.echo_epoch_ms > echo.send_epoch_ms {
            let sample = (echo.echo_epoch_ms - echo.send_epoch_ms) as f64;
            self.observe_rtt(sample);
        }
        self.pending_probes
            .retain(|(probe_id, _)| *probe_id != echo.object_id);
    }

    /// Feed an RTT sample measured elsewhere (for example by a hop).
    pub fn on_rtt_sample(&mut self, rtt_ms: f64) {
        self.observe_rtt(rtt_ms.clamp(1.0, 60_000.0));
    }

    /// Note that the driver applied a update.
    pub fn note_applied(&mut self, update_id: u64) {
        if update_id > self.highest_applied {
            let shift = update_id - self.highest_applied;
            self.bitmap = if shift >= 64 {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest_applied = update_id;
        } else if self.highest_applied - update_id < 64 {
            self.bitmap |= 1 << (self.highest_applied - update_id);
        }
        self.stats.applied_frames += 1;
    }

    /// Note that the driver applied a chunk: the idempotence record for `DUP`
    /// objects (MLP spec §7.1).
    pub fn note_applied_chunk(&mut self, update_id: u64, part_index: u16) {
        let key = (update_id, part_index);
        if self.applied_set.insert(key) {
            self.applied_chunks.push_back(key);
            while self.applied_chunks.len() > APPLIED_CHUNK_WINDOW {
                if let Some(old) = self.applied_chunks.pop_front() {
                    self.applied_set.remove(&old);
                }
            }
        }
    }

    /// Whether this `(update_id, part_index)` was already applied.
    pub fn is_chunk_applied(&self, update_id: u64, part_index: u16) -> bool {
        self.applied_set.contains(&(update_id, part_index))
    }

    /// Update the receiver's own convergence belief.
    pub fn set_viewer_state(&mut self, stale_cells: u32, applied_cell_count: u32, converged: bool) {
        self.stale_cells = stale_cells;
        self.applied_count = applied_cell_count;
        self.converged = converged;
    }

    /// Update the render latency the next ack reports.
    pub fn set_render_latency(&mut self, latency_ms: f64) {
        self.render_latency_ms = latency_ms;
    }

    /// Update the highest input sequence visible in applied cells (MLP spec annex N, N-1).
    pub fn set_echo_of_input_seq(&mut self, input_seq: u64) {
        self.echo_of_input_seq = input_seq;
    }

    /// Update the input queue diagnostics for the next report.
    pub fn set_input_stats(&mut self, queue_depth: u32, submitted: u64, expired: u64) {
        self.input_queue_depth = queue_depth;
        self.window_input_submitted += submitted;
        self.window_input_expired += expired;
    }

    /// Update outbound backlog diagnostics.
    pub fn set_buffered_amount(&mut self, bytes: u32) {
        self.buffered_amount_bytes = bytes;
    }

    /// Update display/render diagnostics.
    pub fn set_display_stats(&mut self, fps: u32, render_p50: u32, render_p95: u32) {
        self.display_fps = fps;
        self.render_ms_p50 = render_p50;
        self.render_ms_p95 = render_p95;
    }

    /// Note a carrier switch.
    pub fn note_path(&mut self, path: MediaPath) {
        if path != self.active_path {
            self.path_switches += 1;
            self.active_path = path;
        }
    }

    /// Set the app version carried in reports (diagnostics only).
    pub fn set_app_version(&mut self, version: impl Into<String>) {
        self.app_version = version.into();
    }

    /// Build the selective ack from the applied-update window.
    pub fn build_ack(&self, now: Instant) -> MediaAck {
        MediaAck {
            highest_applied_update_id: self.highest_applied,
            ack_bitmap: self.bitmap,
            chain_epoch: self.epoch,
            applied_cell_count: self.applied_count,
            stale_cells: self.stale_cells,
            converged: self.converged,
            render_latency_ms: self.render_latency_ms,
            has_render_latency_ms: self.render_latency_ms > 0.0,
            echo_of_input_seq: self.echo_of_input_seq,
            ack_epoch_ms: self.session_ms(now),
        }
    }

    /// Build the feedback report for the current window and reset it.
    pub fn build_feedback(&mut self, now: Instant) -> MediaFeedback {
        let window_ms = now.saturating_duration_since(self.last_report).as_millis() as u64;
        let report = MediaFeedback {
            rtt_ms: self.rtt_ms.round().clamp(1.0, 60_000.0) as u32,
            rtt_min_ms: self.rtt_min_ms.round().clamp(1.0, 60_000.0) as u32,
            jitter_ms: self.jitter_ms.round() as u32,
            received_bytes: self.window_bytes,
            received_objects: self.window_objects,
            lost_objects: self.window_lost,
            duplicated_objects: self.window_duplicates,
            reordered_objects: self.window_reordered,
            window_ms,
            ack: Some(self.build_ack(now)),
            probes: core::mem::take(&mut self.pending_echoes),
            buffered_amount_bytes: self.buffered_amount_bytes,
            display_fps: self.display_fps,
            render_ms_p50: self.render_ms_p50,
            render_ms_p95: self.render_ms_p95,
            input_queue_depth: self.input_queue_depth,
            input_submitted: self.window_input_submitted,
            input_expired: self.window_input_expired,
            path_switches: self.path_switches,
            active_path: self.active_path,
            app_version: self.app_version.clone(),
        };
        self.stats.feedback_reports += 1;
        self.last_report = now;
        self.window_bytes = 0;
        self.window_objects = 0;
        self.window_lost = 0;
        self.window_duplicates = 0;
        self.window_reordered = 0;
        self.window_input_submitted = 0;
        self.window_input_expired = 0;
        report
    }

    /// The echo queue, for a driver that prefers pulling to events.
    pub fn take_pending_echoes(&mut self) -> Vec<ProbeEcho> {
        core::mem::take(&mut self.pending_echoes)
    }

    fn note_new_object(&mut self, object_id: u32) {
        match self.expected_object_id {
            None => {}
            Some(expected) => {
                if object_id > expected {
                    let lost = u64::from(object_id - expected);
                    self.stats.lost_objects += lost;
                    self.window_lost += lost;
                } else if object_id < expected {
                    self.stats.reordered_objects += 1;
                    self.window_reordered += 1;
                }
            }
        }
        self.expected_object_id = Some(object_id.saturating_add(1));
    }

    fn observe_rtt(&mut self, sample: f64) {
        if self.rtt_ms == 0.0 {
            self.rtt_ms = sample;
            self.rtt_min_ms = sample;
        } else {
            let previous = self.rtt_ms;
            let alpha = 1.0 / 8.0;
            self.rtt_ms = previous + alpha * (sample - previous);
            let deviation = (sample - previous).abs();
            self.jitter_ms += (1.0 / 4.0) * (deviation - self.jitter_ms);
            self.rtt_min_ms = self.rtt_min_ms.min(sample);
        }
        self.rtt_last_ms = sample;
    }

    fn probe_due(&self, now: Instant) -> bool {
        match self.last_probe {
            Some(last) => {
                now.saturating_duration_since(last)
                    >= Duration::from_millis(self.config.probe_interval_ms)
            }
            None => true,
        }
    }

    fn probe_payload_bytes(&self) -> u32 {
        // A quarter-second of the sender's advertised pace, never cells.
        match self.advertised_pace_bps {
            Some(pace) => ((pace / 8 / 4) as u32).clamp(1, self.config.probe_payload_max),
            None => 256.min(self.config.probe_payload_max),
        }
    }
}

fn class_slot(class: FragmentClass) -> usize {
    match class {
        FragmentClass::Control => 0,
        FragmentClass::Cursor => 1,
        FragmentClass::Media => 2,
        FragmentClass::Feedback => 3,
        FragmentClass::Path => 4,
    }
}

/// The last RTT sample, for diagnostics.
impl MediaReceiverMachine {
    /// The most recent RTT sample.
    pub fn rtt_last_ms(&self) -> f64 {
        self.rtt_last_ms
    }
}

#[cfg(test)]
mod tests {
    use super::super::pdu::{Fragment, FragmentFlags, HeaderExt, Priority, UpdateMeta, tlv};
    use super::*;

    fn receiver(carrier: Carrier) -> MediaReceiverMachine {
        MediaReceiverMachine::new(MediaReceiverConfig::for_carrier(carrier), Instant::now())
    }

    fn receiver_at(carrier: Carrier, now: Instant) -> MediaReceiverMachine {
        MediaReceiverMachine::new(MediaReceiverConfig::for_carrier(carrier), now)
    }

    fn media_fragment(
        object_id: u32,
        update_id: u64,
        part_index: u16,
        part_count: u16,
        payload: Vec<u8>,
    ) -> Fragment {
        let meta = UpdateMeta {
            update_id,
            update_base: 0,
            part_index,
            part_count,
            update_bytes: 100,
            chain_epoch: 0,
            reflects_input_seq: 0,
            send_epoch_ms: 0,
        };
        let mut ext = meta.to_ext();
        let _ = ext.push_u32(tlv::CELL_COUNT, 4);
        Fragment::new(
            FragmentClass::Media,
            FragmentFlags::FIN.union(FragmentFlags::ANCHOR),
            Priority::MEDIA,
            object_id,
            0,
            1,
            ext,
            payload,
        )
        .expect("valid fragment")
    }

    #[test]
    fn a_message_yields_a_complete_object_and_counters() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let fragment = media_fragment(1, 1, 0, 1, vec![1, 2, 3, 4]);
        let events = receiver.on_message(&fragment.encode(), now);
        assert_eq!(events.len(), 1);
        match &events[0] {
            MediaReceiveEvent::Object(object) => {
                assert_eq!(object.object_id, 1);
                assert_eq!(object.payload, vec![1, 2, 3, 4]);
                assert!(object.is_anchor());
            }
            other => panic!("expected an object, got {other:?}"),
        }
        assert_eq!(receiver.stats().messages, 1);
        assert_eq!(receiver.stats().objects_by_class[2], 1);
        assert_eq!(receiver.stats().bytes, 4);
    }

    #[test]
    fn malformed_messages_are_rejected_without_panicking() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let mut bytes = media_fragment(1, 1, 0, 1, vec![0]).encode();
        bytes[0] = b'X';
        let events = receiver.on_message(&bytes, now);
        assert!(matches!(
            events.as_slice(),
            [MediaReceiveEvent::FragmentDropped {
                reason: RejectReason::BadMagic,
                ..
            }]
        ));
        let events = receiver.on_message(&[1, 2, 3], now);
        assert!(matches!(
            events.as_slice(),
            [MediaReceiveEvent::FragmentDropped {
                reason: RejectReason::BadLength,
                ..
            }]
        ));
        assert_eq!(receiver.stats().dropped_fragments, 2);
    }

    #[test]
    fn gap_detection_counts_lost_objects() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let first = media_fragment(1, 1, 0, 1, vec![0]);
        receiver.on_message(&first.encode(), now);
        // Object 2 is lost; object 3 arrives.
        let third = media_fragment(3, 3, 0, 1, vec![0]);
        receiver.on_message(&third.encode(), now);
        assert_eq!(receiver.stats().lost_objects, 1);
        let feedback = receiver.build_feedback(now);
        assert_eq!(feedback.lost_objects, 1);
        assert_eq!(feedback.received_objects, 2);
        // The window resets.
        assert_eq!(receiver.build_feedback(now).lost_objects, 0);
    }

    #[test]
    fn duplicate_fragments_and_objects_are_counted() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let fragment = media_fragment(1, 1, 0, 1, vec![0]);
        receiver.on_message(&fragment.encode(), now);
        receiver.on_message(&fragment.encode(), now);
        assert_eq!(receiver.stats().duplicate_fragments, 1);
        // A DUP-flagged object is counted as a duplicate object but delivered.
        let mut dup = media_fragment(2, 1, 0, 1, vec![0]);
        dup.header.flags = FragmentFlags::DUP.union(FragmentFlags::FIN);
        let events = receiver.on_message(&dup.encode(), now);
        assert!(matches!(events.as_slice(), [MediaReceiveEvent::Object(_)]));
        assert_eq!(receiver.stats().duplicate_objects, 1);
    }

    #[test]
    fn echo_requests_produce_replies() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let mut fragment = media_fragment(1, 1, 0, 1, vec![0; 10]);
        fragment.header.flags = FragmentFlags::FIN.union(FragmentFlags::ECHO_REQUESTED);
        let events = receiver.on_message(&fragment.encode(), now);
        let echo = events
            .iter()
            .find_map(|event| match event {
                MediaReceiveEvent::ProbeEchoDue(echo) => Some(*echo),
                _ => None,
            })
            .expect("an echo is due");
        assert_eq!(echo.object_id, 1);
        assert_eq!(echo.payload_bytes, 10);
        assert_eq!(receiver.stats().echo_replies, 1);
    }

    #[test]
    fn applied_frames_build_a_selective_ack() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        receiver.note_applied(1);
        receiver.note_applied(2);
        receiver.note_applied(4);
        receiver.set_viewer_state(0, 510, true);
        let ack = receiver.build_ack(now);
        assert_eq!(ack.highest_applied_update_id, 4);
        assert!(ack.is_applied(4));
        assert!(ack.is_applied(2));
        assert!(ack.is_applied(1));
        assert!(!ack.is_applied(3));
        assert_eq!(ack.applied_cell_count, 510);
        assert!(ack.converged);
        assert_eq!(ack.validate(), Ok(()));
    }

    #[test]
    fn ack_window_slides_beyond_64_frames() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        for update in 1..=100u64 {
            receiver.note_applied(update);
        }
        let ack = receiver.build_ack(now);
        assert_eq!(ack.highest_applied_update_id, 100);
        assert!(ack.is_applied(100));
        assert!(ack.is_applied(37));
        assert!(!ack.is_applied(36), "outside the 64-update window");
        assert_eq!(ack.validate(), Ok(()));
    }

    #[test]
    fn applied_chunk_idempotence() {
        let mut receiver = receiver(Carrier::Ws);
        assert!(!receiver.is_chunk_applied(7, 1));
        receiver.note_applied_chunk(7, 1);
        assert!(receiver.is_chunk_applied(7, 1));
        assert!(!receiver.is_chunk_applied(7, 2));
    }

    #[test]
    fn feedback_reports_the_window_and_resets_it() {
        let now = Instant::now();
        let mut receiver = receiver_at(Carrier::Ws, now);
        let fragment = media_fragment(1, 1, 0, 1, vec![0; 100]);
        receiver.on_message(&fragment.encode(), now);
        receiver.set_display_stats(12, 3, 9);
        receiver.set_input_stats(2, 5, 1);
        receiver.note_path(MediaPath::Ws);
        let later = now + Duration::from_millis(500);
        let feedback = receiver.build_feedback(later);
        assert_eq!(feedback.received_bytes, 100);
        assert_eq!(feedback.received_objects, 1);
        assert_eq!(feedback.window_ms, 500);
        assert_eq!(feedback.display_fps, 12);
        assert_eq!(feedback.render_ms_p95, 9);
        assert_eq!(feedback.input_submitted, 5);
        assert_eq!(feedback.path_switches, 1);
        assert_eq!(feedback.active_path, MediaPath::Ws);
        assert!(feedback.ack.is_some());
        let second = receiver.build_feedback(later);
        assert_eq!(second.received_bytes, 0);
        assert_eq!(second.input_submitted, 0);
    }

    #[test]
    fn probe_cadence_and_echo_rtt() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let events = receiver.tick(now);
        let probe = events
            .iter()
            .find_map(|event| match event {
                MediaReceiveEvent::ProbeDue(probe) => Some(*probe),
                _ => None,
            })
            .expect("first probe");
        receiver.on_probe_echo(
            ProbeEcho {
                object_id: probe.probe_id,
                send_epoch_ms: probe.send_epoch_ms,
                echo_epoch_ms: probe.send_epoch_ms + 150,
                payload_bytes: probe.payload_bytes,
            },
            now,
        );
        assert!((receiver.rtt_ms() - 150.0).abs() < 1.0);
        assert_eq!(receiver.stats().probe_echoes, 1);
        assert_eq!(receiver.report_interval(), Duration::from_millis(600));
    }

    #[test]
    fn assembly_timeout_surfaces_as_a_dropped_object() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let mut first = media_fragment(9, 1, 0, 2, vec![0]);
        first.header.fragment_count = 2;
        first.header.flags = FragmentFlags::NONE;
        assert_eq!(first.header.fragment_index, 0);
        receiver.on_message(&first.encode(), now);
        assert_eq!(receiver.stats().messages, 1);
        let later = now + Duration::from_secs(10);
        let events = receiver.tick(later);
        assert!(events.iter().any(|event| matches!(
            event,
            MediaReceiveEvent::ObjectDropped {
                reason: RejectReason::AssemblyTimeout,
                ..
            }
        )));
    }

    #[test]
    fn datagram_carrier_rejects_multi_fragment_objects() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::DataChannel);
        let mut fragment = media_fragment(1, 1, 0, 2, vec![0]);
        fragment.header.fragment_count = 2;
        let events = receiver.on_message(&fragment.encode(), now);
        assert!(matches!(
            events.as_slice(),
            [MediaReceiveEvent::FragmentDropped {
                reason: RejectReason::MultiFragmentOnDatagram,
                ..
            }]
        ));
    }

    #[test]
    fn media_without_frame_metadata_is_dropped() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let fragment = Fragment::new(
            FragmentClass::Media,
            FragmentFlags::FIN,
            Priority::MEDIA,
            1,
            0,
            1,
            HeaderExt::new(),
            vec![0; 4],
        )
        .expect("valid");
        let events = receiver.on_message(&fragment.encode(), now);
        assert!(matches!(
            events.as_slice(),
            [MediaReceiveEvent::FragmentDropped {
                reason: RejectReason::BodyInvalid,
                ..
            }]
        ));
    }

    #[test]
    fn advertised_diagnostics_are_learned() {
        let now = Instant::now();
        let mut receiver = receiver(Carrier::Ws);
        let mut fragment = media_fragment(1, 1, 0, 1, vec![0]);
        let _ = fragment.ext.push_u32(tlv::PACE_BPS, 32);
        let _ = fragment.ext.push_u32(tlv::BUDGET_BYTES, 1_000);
        receiver.on_message(&fragment.encode(), now);
        assert_eq!(receiver.advertised_pace_bps(), Some(32_000));
        assert_eq!(receiver.advertised_budget_bytes(), Some(1_000));
        // The probe payload is a quarter-second of the advertised pace.
        let events = receiver.tick(now);
        if let Some(MediaReceiveEvent::ProbeDue(probe)) = events
            .iter()
            .find(|event| matches!(event, MediaReceiveEvent::ProbeDue(_)))
        {
            assert_eq!(probe.payload_bytes, 1_000);
        }
    }
}
