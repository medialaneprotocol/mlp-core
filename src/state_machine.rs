//! Core sans-IO state machine trait.
//!
//! A state machine is a pure object that takes input and time from the outside
//! and emits [`Action`](super::Action)s and [`Transmit`](super::Transmit)s. It
//! never performs I/O, never spawns tasks, and never reads the system clock
//! directly.

use std::time::{Duration, Instant};

/// The canonical sans-IO state machine interface.
///
/// Implementors declare their action, transmit, and receive types as
/// associated types. Time is always passed in as an [`Instant`]; the state
/// machine must not call [`Instant::now`].
///
/// # Contract
///
/// * `handle_input` is called when new input (bytes, connection events, or
///   domain-specific input) is available.
/// * `poll_transmit` is called when the driver is ready to send. It should
///   return `Some` only when there is an outbound update ready to be handed to
///   the transport.
/// * `poll_timeout` returns the duration until the next timeout, relative to
///   the provided `now`. Return `None` if no timeout is currently armed.
/// * `handle_timeout` is called when the deadline returned by `poll_timeout`
///   has elapsed.
pub trait StateMachine {
    /// Action type emitted by the state machine.
    type Action;
    /// Outbound transmission type.
    type Transmit;
    /// Inbound input type.
    type Receive;

    /// Handle a single inbound input and emit actions.
    fn handle_input(&mut self, input: Self::Receive, now: Instant) -> Vec<Self::Action>;

    /// Produce the next outbound transmission, if any.
    fn poll_transmit(&mut self, now: Instant) -> Option<Self::Transmit>;

    /// Return the duration until the next timeout, relative to `now`.
    fn poll_timeout(&mut self, now: Instant) -> Option<Duration>;

    /// Handle a timeout expiry and emit actions.
    fn handle_timeout(&mut self, now: Instant) -> Vec<Self::Action>;
}

/// Lifecycle phase shared by session-oriented state machines.
///
/// Drivers own their protocol-specific handshake, heartbeat, and update state;
/// this enum gives every session machine the same externally observable
/// lifecycle vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    /// Waiting for the handshake to complete.
    Handshake,
    /// Session is established and processing data updates.
    Established,
    /// Session is closed; all further inputs are ignored.
    Closed,
}

#[cfg(test)]
mod tests {
    use super::SessionPhase;

    #[test]
    fn phases_compare_structurally() {
        assert_eq!(SessionPhase::Handshake, SessionPhase::Handshake);
        assert_ne!(SessionPhase::Handshake, SessionPhase::Established);
        assert_ne!(SessionPhase::Established, SessionPhase::Closed);
    }
}
