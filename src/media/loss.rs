//! Loss detection (MLP spec §6.12): detect lost fragments based on gaps
//! in the received sequence numbers, adapted from QUIC (RFC 9002).
//!
//! A fragment is declared lost when a later fragment in the same packet
//! number space has been acknowledged, and enough time has passed.

use std::collections::BTreeSet;

/// A loss detector for one packet number space.
#[derive(Debug, Clone, Default)]
pub struct LossDetector {
    /// Highest acknowledged packet number.
    pub largest_acked: Option<u64>,
    /// Packet numbers that have been sent.
    pub sent: BTreeSet<u64>,
    /// Packet numbers that have been acknowledged.
    pub acked: BTreeSet<u64>,
    /// Packet numbers declared lost.
    pub lost: BTreeSet<u64>,
    /// Loss delay (time-based threshold).
    pub loss_delay: std::time::Duration,
}

impl LossDetector {
    pub fn new(loss_delay: std::time::Duration) -> Self {
        Self {
            loss_delay,
            ..Self::default()
        }
    }

    /// Record that a packet was sent.
    pub fn on_send(&mut self, pn: u64) {
        self.sent.insert(pn);
    }

    /// Record an acknowledgement.
    pub fn on_ack(&mut self, acked_pns: &[u64]) {
        for &pn in acked_pns {
            self.acked.insert(pn);
            if self.largest_acked.is_none() || pn > self.largest_acked.unwrap() {
                self.largest_acked = Some(pn);
            }
        }
        self.detect_losses();
    }

    /// Detect lost packets based on gaps.
    pub fn detect_losses(&mut self) {
        let Some(largest) = self.largest_acked else {
            return;
        };
        // A packet is lost if it's less than largest_acked and not acked,
        // and (largest_acked - pn) >= 3 (packet threshold, QUIC-style).
        let threshold: u64 = 3;
        for &pn in &self.sent.clone() {
            if pn >= largest {
                continue;
            }
            if self.acked.contains(&pn) {
                continue;
            }
            if self.lost.contains(&pn) {
                continue;
            }
            if largest - pn >= threshold {
                self.lost.insert(pn);
            }
        }
    }

    /// Whether a packet is considered lost.
    pub fn is_lost(&self, pn: u64) -> bool {
        self.lost.contains(&pn)
    }

    /// Number of lost packets.
    pub fn lost_count(&self) -> usize {
        self.lost.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn detects_gap() {
        let mut ld = LossDetector::new(Duration::from_millis(100));
        for pn in 0..10u64 {
            ld.on_send(pn);
        }
        // ack 0,1,2,4,5 (3 is lost)
        ld.on_ack(&[0, 1, 2, 4, 5, 6, 7, 8, 9]);
        assert!(ld.is_lost(3));
    }

    #[test]
    fn no_loss_when_contiguous() {
        let mut ld = LossDetector::new(Duration::from_millis(100));
        for pn in 0..5u64 {
            ld.on_send(pn);
        }
        ld.on_ack(&[0, 1, 2, 3, 4]);
        assert_eq!(ld.lost_count(), 0);
    }

    #[test]
    fn largest_acked_tracked() {
        let mut ld = LossDetector::new(Duration::from_millis(100));
        ld.on_send(0);
        ld.on_send(1);
        ld.on_ack(&[0, 1]);
        assert_eq!(ld.largest_acked, Some(1));
    }
}
