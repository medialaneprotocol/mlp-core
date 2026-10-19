//! Deterministic in-memory test harness for sans-IO state machines.
//!
//! The harness drives a [`StateMachine`](super::StateMachine) through byte
//! inputs and explicit time advances and records every emitted action and
//! every outbound transmission. No sockets, async runtime, or system clock is
//! required.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use regex::Regex;

pub mod clock;
pub mod lossy_link;
pub mod scenario;

/// Shared epoch for deterministic sans-IO tests.
///
/// The instant is captured once per process so every machine in a test binary
/// starts from the same origin; machines themselves still receive time
/// explicitly and never call the system clock.
pub fn test_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// In-memory driver for a sans-IO state machine.
///
/// Tracks the machine, a deterministic current time, queued transmissions, and
/// a trace of all actions produced.
#[derive(Debug)]
pub struct TestHarness<SM: super::StateMachine> {
    /// The state machine under test.
    pub machine: SM,
    /// Current deterministic time.
    pub now: Instant,
    /// Outbound transmissions produced by `poll_transmit`.
    pub transmit_queue: VecDeque<SM::Transmit>,
    /// Every action emitted by the machine, in order.
    pub action_trace: Vec<SM::Action>,
    /// Cached absolute deadline from the last `poll_timeout` call.
    next_timeout: Option<Instant>,
}

impl<SM: super::StateMachine> TestHarness<SM> {
    /// Create a new harness with the given machine and starting instant.
    pub fn new(machine: SM, epoch: Instant) -> Self {
        Self {
            machine,
            now: epoch,
            transmit_queue: VecDeque::new(),
            action_trace: Vec::new(),
            next_timeout: None,
        }
    }

    /// Advance the deterministic clock by `duration`.
    pub fn advance_time(&mut self, duration: Duration) {
        self.now += duration;
    }

    /// Inject an input into the state machine and record emitted actions.
    pub fn inject(&mut self, input: SM::Receive) {
        let actions = self.machine.handle_input(input, self.now);
        self.action_trace.extend(actions);
    }

    /// Drive the state machine until it has no more immediate work.
    ///
    /// The harness drains transmissions and fires expired timeouts, up to
    /// `max_steps` iterations. This prevents infinite loops if a machine keeps
    /// producing work.
    pub fn run_until_quiescent(&mut self, max_steps: usize) {
        for _ in 0..max_steps {
            // Recompute the timeout deadline if we don't have one cached.
            if self.next_timeout.is_none()
                && let Some(relative) = self.machine.poll_timeout(self.now)
            {
                self.next_timeout = Some(self.now + relative);
            }

            // If a timeout has expired, fire it.
            if let Some(deadline) = self.next_timeout
                && self.now >= deadline
            {
                let actions = self.machine.handle_timeout(self.now);
                self.action_trace.extend(actions);
                self.next_timeout = None;
                continue;
            }

            // Otherwise drain a transmission if available.
            if let Some(tx) = self.machine.poll_transmit(self.now) {
                self.transmit_queue.push_back(tx);
                continue;
            }

            // Nothing to do.
            break;
        }
    }

    /// Return every action emitted so far.
    pub fn actions(&self) -> &[SM::Action] {
        &self.action_trace
    }

    /// Return every queued transmission.
    pub fn transmits(&self) -> &VecDeque<SM::Transmit> {
        &self.transmit_queue
    }

    /// Return a deterministic snapshot of the action trace and transmit queue.
    pub fn golden_trace(&self) -> GoldenTrace<SM>
    where
        SM::Action: Clone + PartialEq + Debug,
        SM::Transmit: Clone + PartialEq + Debug,
    {
        GoldenTrace {
            actions: self.action_trace.clone(),
            transmits: self.transmit_queue.clone(),
        }
    }

    /// Inject a sequence of inputs in order.
    pub fn inject_sequence<I>(&mut self, inputs: I)
    where
        I: IntoIterator<Item = SM::Receive>,
    {
        for input in inputs {
            self.inject(input);
        }
    }
}

/// A snapshot of every action and queued transmission produced by a state
/// machine under test.
///
/// Two golden traces are equal iff their actions and transmissions are equal
/// in the same order. This makes repeatability of a test a single assertion.
pub struct GoldenTrace<SM: super::StateMachine>
where
    SM::Action: Clone + PartialEq + Debug,
    SM::Transmit: Clone + PartialEq + Debug,
{
    /// Every action emitted by the state machine.
    pub actions: Vec<SM::Action>,
    /// Every outbound transmission queued by the state machine.
    pub transmits: VecDeque<SM::Transmit>,
}

impl<SM: super::StateMachine> GoldenTrace<SM>
where
    SM::Action: Clone + PartialEq + Debug,
    SM::Transmit: Clone + PartialEq + Debug,
{
    /// Create a golden trace from an action trace and a transmit queue.
    pub fn new(actions: Vec<SM::Action>, transmits: VecDeque<SM::Transmit>) -> Self {
        Self { actions, transmits }
    }

    /// Save the golden trace to a file using Debug format.
    ///
    /// The file is human-readable and diffable. This is meant to be called
    /// from tests or CI scripts to commit golden traces.
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let content = format!("{:#?}", self);
        std::fs::write(path, content)
    }

    /// Save only the action trace to a file (excludes transmits for stability).
    ///
    /// Actions are domain-specific side effects that represent the state
    /// machine's observable behavior. Transmits may contain protobuf-encoded
    /// bytes with timestamps that vary across builds.
    pub fn save_actions(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let content = format!("{:#?}", self.actions);
        std::fs::write(path, content)
    }

    /// Load a golden trace from a file and compare it against the current trace.
    ///
    /// Returns `Ok(true)` if the traces match, `Ok(false)` if they differ,
    /// or `Err` if the file cannot be read.
    pub fn matches_file(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<bool> {
        let expected = std::fs::read_to_string(path)?;
        let actual = format!("{:#?}", self);
        Ok(expected == actual)
    }

    /// Assert that the golden trace actions match the committed file.
    ///
    /// Only compares actions (not transmits) for stability across builds.
    /// Timing-dependent values (delays, durations) are normalized to `<DELAY>`
    /// before comparison so jitter doesn't cause false failures.
    ///
    /// A missing file is an error: golden traces are committed artifacts, and
    /// silently recreating one would hide an accidental deletion. Set
    /// `UPDATE_GOLDEN_TRACES=1` to (re)write the file instead.
    pub fn assert_matches_file(&self, path: impl AsRef<std::path::Path>) {
        let path = path.as_ref();
        if std::env::var_os("UPDATE_GOLDEN_TRACES").is_some() {
            self.save_actions(path).unwrap_or_else(|error| {
                panic!("failed to write golden file {}: {error}", path.display())
            });
            return;
        }
        let expected = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "golden trace {} is missing ({error}); commit it or set UPDATE_GOLDEN_TRACES=1",
                path.display()
            )
        });
        let actual = normalize_golden_trace(&format!("{:#?}", self.actions));
        let expected_normalized = normalize_golden_trace(&expected);
        if expected_normalized != actual {
            panic!(
                "golden trace mismatch for {}\n\n--- expected ---\n{}\n\n--- actual ---\n{}",
                path.display(),
                expected_normalized,
                actual
            );
        }
    }
}

/// Normalize a golden trace string by replacing timing-dependent values
/// with a fixed placeholder. This makes golden traces stable across builds
/// where jitter or timing may differ.
fn normalize_golden_trace(s: &str) -> String {
    static DURATION_RE: OnceLock<Regex> = OnceLock::new();
    let re = DURATION_RE.get_or_init(|| {
        // Debug output renders `Duration` as `110ms`, `1.5s`, `0ns`, `42µs`.
        Regex::new(r"\b\d+(?:\.\d+)?(?:ns|µs|us|ms|s)\b").expect("static duration regex")
    });
    re.replace_all(s, "<DELAY>").to_string()
}

impl<SM: super::StateMachine> Clone for GoldenTrace<SM>
where
    SM::Action: Clone + PartialEq + Debug,
    SM::Transmit: Clone + PartialEq + Debug,
{
    fn clone(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            transmits: self.transmits.clone(),
        }
    }
}

impl<SM: super::StateMachine> Debug for GoldenTrace<SM>
where
    SM::Action: Clone + PartialEq + Debug,
    SM::Transmit: Clone + PartialEq + Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoldenTrace")
            .field("actions", &self.actions)
            .field("transmits", &self.transmits)
            .finish()
    }
}

impl<SM: super::StateMachine> PartialEq for GoldenTrace<SM>
where
    SM::Action: Clone + PartialEq + Debug,
    SM::Transmit: Clone + PartialEq + Debug,
{
    fn eq(&self, other: &Self) -> bool {
        self.actions == other.actions && self.transmits == other.transmits
    }
}

impl<SM: super::StateMachine> Eq for GoldenTrace<SM>
where
    SM::Action: Clone + PartialEq + Debug,
    SM::Transmit: Clone + PartialEq + Debug,
{
}

#[cfg(test)]
mod tests {
    use super::super::StateMachine;
    use super::super::action::{Action, ConnectionEvent, Receive, Transmit};
    use super::clock::ManualClock;
    use super::{TestHarness, test_epoch};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    /// A tiny echo/ping-pong state machine used to exercise the harness.
    struct EchoMachine {
        outbound: VecDeque<Vec<u8>>,
        last_activity: Instant,
        heartbeat_interval: Duration,
    }

    impl EchoMachine {
        fn new(epoch: Instant, heartbeat_interval: Duration) -> Self {
            Self {
                outbound: VecDeque::new(),
                last_activity: epoch,
                heartbeat_interval,
            }
        }
    }

    impl StateMachine for EchoMachine {
        type Action = Action<()>;
        type Transmit = Transmit;
        type Receive = Receive<()>;

        fn handle_input(&mut self, input: Self::Receive, now: Instant) -> Vec<Self::Action> {
            match input {
                Receive::Bytes(bytes) => {
                    self.last_activity = now;
                    self.outbound.push_back(bytes);
                    Vec::new()
                }
                Receive::ConnectionEvent(ConnectionEvent::Closed) => {
                    vec![Action::Log("connection closed".to_string())]
                }
                _ => Vec::new(),
            }
        }

        fn poll_transmit(&mut self, _now: Instant) -> Option<Self::Transmit> {
            self.outbound.pop_front().map(|payload| Transmit {
                payload,
                flush: true,
            })
        }

        fn poll_timeout(&mut self, now: Instant) -> Option<Duration> {
            let elapsed = now.duration_since(self.last_activity);
            if elapsed >= self.heartbeat_interval {
                Some(Duration::ZERO)
            } else {
                Some(self.heartbeat_interval - elapsed)
            }
        }

        fn handle_timeout(&mut self, now: Instant) -> Vec<Self::Action> {
            self.last_activity = now;
            vec![Action::Log("heartbeat".to_string())]
        }
    }

    #[test]
    fn echo_machine_transmits_injected_bytes() {
        let epoch = test_epoch();
        let machine = EchoMachine::new(epoch, Duration::from_secs(1));
        let mut harness = TestHarness::new(machine, epoch);

        harness.inject(Receive::Bytes(b"ping".to_vec()));
        harness.run_until_quiescent(10);

        assert_eq!(harness.transmits().len(), 1);
        assert_eq!(harness.transmits()[0].payload, b"ping");
        assert!(harness.actions().is_empty());
    }

    #[test]
    fn echo_machine_emits_heartbeat_after_inactivity() {
        let epoch = test_epoch();
        let machine = EchoMachine::new(epoch, Duration::from_millis(100));
        let mut harness = TestHarness::new(machine, epoch);

        harness.advance_time(Duration::from_millis(150));
        harness.run_until_quiescent(10);

        assert_eq!(harness.actions().len(), 1);
        assert_eq!(harness.actions()[0], Action::Log("heartbeat".to_string()));
    }

    #[test]
    fn echo_machine_preserves_transmit_order() {
        let epoch = test_epoch();
        let machine = EchoMachine::new(epoch, Duration::from_secs(1));
        let mut harness = TestHarness::new(machine, epoch);

        harness.inject(Receive::Bytes(b"first".to_vec()));
        harness.inject(Receive::Bytes(b"second".to_vec()));
        harness.run_until_quiescent(10);

        let payloads: Vec<_> = harness
            .transmits()
            .iter()
            .map(|t| t.payload.clone())
            .collect();
        assert_eq!(payloads, vec![b"first".to_vec(), b"second".to_vec()]);
    }

    #[test]
    fn trace_is_deterministic_across_runs() {
        let epoch = test_epoch();

        let run = || {
            let machine = EchoMachine::new(epoch, Duration::from_millis(50));
            let mut harness = TestHarness::new(machine, epoch);
            harness.inject(Receive::Bytes(b"a".to_vec()));
            harness.advance_time(Duration::from_millis(75));
            harness.run_until_quiescent(10);
            harness.action_trace.clone()
        };

        let first = run();
        let second = run();
        assert_eq!(first, second);
    }

    #[test]
    fn connection_closed_emits_log_action() {
        let epoch = test_epoch();
        let machine = EchoMachine::new(epoch, Duration::from_secs(1));
        let mut harness = TestHarness::new(machine, epoch);

        harness.inject(Receive::ConnectionEvent(ConnectionEvent::Closed));
        harness.run_until_quiescent(10);

        assert_eq!(
            harness.actions()[0],
            Action::Log("connection closed".to_string())
        );
    }

    #[test]
    fn manual_clock_advances_deterministically() {
        let epoch = test_epoch();
        let mut clock = ManualClock::new(epoch);
        assert_eq!(clock.now(), epoch);

        clock.advance(Duration::from_millis(42));
        assert_eq!(clock.now(), epoch + Duration::from_millis(42));

        let later = epoch + Duration::from_secs(7);
        clock.set(later);
        assert_eq!(clock.now(), later);
    }

    #[test]
    fn golden_trace_is_reproducible_across_runs() {
        let epoch = test_epoch();

        let run = || {
            let machine = EchoMachine::new(epoch, Duration::from_millis(50));
            let mut harness = TestHarness::new(machine, epoch);
            harness.inject_sequence([
                Receive::Bytes(b"first".to_vec()),
                Receive::Bytes(b"second".to_vec()),
            ]);
            harness.advance_time(Duration::from_millis(75));
            harness.run_until_quiescent(10);
            harness.golden_trace()
        };

        let first = run();
        let second = run();
        assert_eq!(first, second);
    }

    #[test]
    fn golden_trace_diverges_on_changed_inputs() {
        let epoch = test_epoch();

        let run_with = |payload: &[u8]| {
            let machine = EchoMachine::new(epoch, Duration::from_millis(50));
            let mut harness = TestHarness::new(machine, epoch);
            harness.inject(Receive::Bytes(payload.to_vec()));
            harness.advance_time(Duration::from_millis(75));
            harness.run_until_quiescent(10);
            harness.golden_trace()
        };

        let ping = run_with(b"ping");
        let pong = run_with(b"pong");
        assert_ne!(ping, pong);
    }
}
