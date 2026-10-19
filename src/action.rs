//! Runtime-agnostic action and input types for sans-IO state machines.
//!
//! These enums intentionally avoid async runtime or socket types. They are
//! meant to be serialized and interpreted by a driver that performs the
//! actual I/O.

use serde::{Deserialize, Serialize};
use std::fmt::Debug;

/// A side effect produced by a [`super::StateMachine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action<T = ()> {
    /// Transmit a payload to the peer.
    Transmit(Transmit),
    /// Log a diagnostic message.
    Log(String),
    /// Emit a metrics counter.
    Counter { name: String, value: u64 },
    /// Domain-specific action.
    Custom(T),
}

/// An outbound transmission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transmit {
    /// Bytes to send.
    pub payload: Vec<u8>,
    /// Whether the driver should flush the underlying transport immediately.
    pub flush: bool,
}

/// An inbound event delivered to a [`super::StateMachine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Receive<T = ()> {
    /// Raw bytes received from the peer.
    Bytes(Vec<u8>),
    /// Connection-level event.
    ConnectionEvent(ConnectionEvent),
    /// Domain-specific input.
    Custom(T),
}

/// Connection-level events that a state machine may react to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionEvent {
    Established,
    Closed,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_serializes_round_trip() {
        let action = Action::<()>::Transmit(Transmit {
            payload: b"hello".to_vec(),
            flush: true,
        });
        let json = serde_json::to_string(&action).expect("serialize");
        let back: Action = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(action, back);
    }

    #[test]
    fn receive_serializes_round_trip() {
        let input = Receive::<()>::ConnectionEvent(ConnectionEvent::Closed);
        let json = serde_json::to_string(&input).expect("serialize");
        let back: Receive = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(input, back);
    }
}
