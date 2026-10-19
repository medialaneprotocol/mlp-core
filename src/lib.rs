//! The Media Lane Protocol (MLP) — sans-IO reference implementation.
//!
//! MLP is a transport for real-time interactive media over links that are
//! slow, lossy, or both. This crate is the protocol's core: it is where every
//! byte of the wire format is defined, encoded, and validated.
//!
//! # What the protocol does
//!
//! A *lane* carries three kinds of traffic at once and keeps them apart:
//!
//! * **control** — the handshake, the session's continuity, and its teardown;
//! * **media** — the updates a sender produces, moved whole and in order;
//! * **feedback** — what the receiver applied, how fast the link is, and what
//!   an intermediary dropped.
//!
//! The design goal is that a receiver never holds a state it cannot verify,
//! and a sender always knows what to re-send, under a rate it measured.
//!
//! # What the protocol does not do
//!
//! MLP does not define what an update's bytes *mean*. That is a codec's
//! business: the protocol moves the bytes, accounts for them, and paces them,
//! and it never interprets their content. A codec therefore plugs in without
//! the protocol learning anything about it.
//!
//! # The two boundaries
//!
//! The crate is sans-IO: it never touches a socket, never reads a clock, and
//! never allocates on behalf of a runtime.
//!
//! * Time arrives as an [`std::time::Instant`] parameter.
//! * Outbound bytes leave as [`Action`](action::Action) values.
//! * State changes are driven by [`StateMachine`](state_machine::StateMachine).
//!
//! The two full-duplex machines are
//! [`sender::MediaSenderMachine`] and [`receiver::MediaReceiverMachine`].
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`pdu`] | the fragment header, the TLV registry, and their validation |
//! | [`object`] | fragment assembly, deduplication, and the open-object table |
//! | [`bwe`] | the bandwidth estimator |
//! | [`pacer`] | the token-bucket pacer |
//! | [`scheduler`] | priority scheduling and repair plans |
//! | [`cells`] | per-receiver convergence accounting over a repair-cell grid |
//! | [`resume`] | parking, resume, and the reconnect ladder |
//! | [`input`] | the input path: sequencing, deadlines, and the pressed set |
//! | [`lifecycle`] | the lane's state vocabulary |
//! | [`error`] | the rejection vocabulary |
//! | [`testing`] | the deterministic harness and the lossy-link simulator |
//!
//! # Example
//!
//! ```
//! use mlp_core::media::pdu::{Carrier, Fragment, FragmentClass, Priority};
//!
//! // A minimal, valid control fragment.
//! let mut bytes = Vec::new();
//! bytes.extend_from_slice(&mlp_core::media::pdu::FRAGMENT_MAGIC);
//! bytes.push(mlp_core::media::pdu::MEDIA_PROTOCOL_VERSION);
//! bytes.push(FragmentClass::Control as u8);
//! bytes.push(0); // flags
//! bytes.push(Priority::CONTROL.get());
//! bytes.extend_from_slice(&1_u32.to_le_bytes()); // object id
//! bytes.extend_from_slice(&0_u16.to_le_bytes()); // fragment index
//! bytes.extend_from_slice(&1_u16.to_le_bytes()); // fragment count
//! bytes.extend_from_slice(&0_u32.to_le_bytes()); // payload length
//! bytes.extend_from_slice(&0_u16.to_le_bytes()); // header extension length
//!
//! let (fragment, consumed) = Fragment::parse(&bytes, Carrier::Ws).expect("a valid fragment");
//! assert_eq!(fragment.header.class, FragmentClass::Control);
//! assert_eq!(consumed, bytes.len());
//! ```

pub mod action;
pub mod media;
pub mod state_machine;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use action::{Action, ConnectionEvent, Receive, Transmit};
pub use state_machine::{SessionPhase, StateMachine};
#[cfg(any(test, feature = "testing"))]
pub use testing::{GoldenTrace, TestHarness, test_epoch};
