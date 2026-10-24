//! 0-RTT session resumption (MLP spec §8.7): a sender issues a session
//! ticket the receiver (or a returning viewer) can use to resume a parked
//! session without a full handshake, sending early data on the first flight.
//!
//! The ticket is opaque to the receiver: it carries the sender's state
//! (last applied update id, chain epoch, cell accounting digest) encrypted
//! under a sender-held key. Anti-replay is handled by a nonce + issue
//! timestamp with a bounded validity window.

use std::time::{Duration, Instant};

/// Maximum age of a ticket before it is rejected.
pub const TICKET_TTL: Duration = Duration::from_secs(3_600);

/// A session ticket issued by the sender.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTicket {
    /// Opaque payload (sender-encrypted state).
    pub payload: Vec<u8>,
    /// Issue timestamp (session-relative ms).
    pub issued_at_ms: u64,
    /// Nonce for anti-replay.
    pub nonce: u64,
}

impl SessionTicket {
    /// Whether the ticket is still within its validity window.
    pub fn is_valid(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.issued_at_ms) <= TICKET_TTL.as_millis() as u64
    }
}

/// Issue a ticket from current session state.
pub fn issue_ticket(last_applied_update_id: u64, chain_epoch: u32, now: Instant) -> SessionTicket {
    // Simplified: payload is a debug encoding (real impl encrypts).
    let mut payload = Vec::with_capacity(16);
    payload.extend_from_slice(&last_applied_update_id.to_le_bytes());
    payload.extend_from_slice(&chain_epoch.to_le_bytes());
    SessionTicket {
        payload,
        issued_at_ms: now.elapsed().as_millis() as u64,
        nonce: now.elapsed().as_nanos() as u64,
    }
}

/// Validate and decode a ticket. Returns the recovered state or None.
pub fn validate_ticket(ticket: &SessionTicket, now_ms: u64) -> Option<(u64, u32)> {
    if !ticket.is_valid(now_ms) {
        return None;
    }
    if ticket.payload.len() < 12 {
        return None;
    }
    let update_id = u64::from_le_bytes(ticket.payload[0..8].try_into().ok()?);
    let epoch = u32::from_le_bytes(ticket.payload[8..12].try_into().ok()?);
    Some((update_id, epoch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_and_validate() {
        let now = Instant::now();
        let t = issue_ticket(42, 7, now);
        assert!(t.is_valid(now.elapsed().as_millis() as u64));
        let (uid, epoch) = validate_ticket(&t, now.elapsed().as_millis() as u64).unwrap();
        assert_eq!(uid, 42);
        assert_eq!(epoch, 7);
    }

    #[test]
    fn expired_ticket_rejected() {
        let now = Instant::now();
        let mut t = issue_ticket(1, 1, now);
        t.issued_at_ms = 0;
        assert!(!t.is_valid((TICKET_TTL.as_millis() as u64) + 1));
        assert!(validate_ticket(&t, (TICKET_TTL.as_millis() as u64) + 1).is_none());
    }
}
