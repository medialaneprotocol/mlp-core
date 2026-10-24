//! Flow control (MLP spec §6.10): per-stream and connection-level flow
//! control, adapted from QUIC (RFC 9000 §4). The receiver advertises a
//! credit (max bytes) for each stream and for the connection as a whole;
//! the sender MUST NOT send more than the available credit.

/// Flow control state for one stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFlowControl {
    /// Maximum bytes the receiver will accept on this stream.
    pub max_stream_data: u64,
    /// Bytes sent so far on this stream.
    pub sent: u64,
    /// Bytes received (applied) so far.
    pub received: u64,
}

impl StreamFlowControl {
    pub fn new(initial_credit: u64) -> Self {
        Self {
            max_stream_data: initial_credit,
            sent: 0,
            received: 0,
        }
    }

    /// Available credit for sending.
    pub fn available(&self) -> u64 {
        self.max_stream_data.saturating_sub(self.sent)
    }

    /// Whether `n` bytes can be sent.
    pub fn can_send(&self, n: u64) -> bool {
        n <= self.available()
    }

    /// Record that `n` bytes were sent.
    pub fn on_send(&mut self, n: u64) {
        self.sent = self.sent.saturating_add(n);
    }

    /// Update the credit (receiver's MAX_STREAM_DATA).
    pub fn update_credit(&mut self, new_max: u64) {
        if new_max > self.max_stream_data {
            self.max_stream_data = new_max;
        }
    }

    /// Record received (applied) bytes.
    pub fn on_receive(&mut self, n: u64) {
        self.received = self.received.saturating_add(n);
    }
}

/// Connection-level flow control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionFlowControl {
    /// Maximum bytes the receiver will accept on the connection.
    pub max_data: u64,
    /// Total bytes sent across all streams.
    pub sent: u64,
    /// Total bytes received across all streams.
    pub received: u64,
}

impl ConnectionFlowControl {
    pub fn new(initial_credit: u64) -> Self {
        Self {
            max_data: initial_credit,
            sent: 0,
            received: 0,
        }
    }

    pub fn available(&self) -> u64 {
        self.max_data.saturating_sub(self.sent)
    }

    pub fn can_send(&self, n: u64) -> bool {
        n <= self.available()
    }

    pub fn on_send(&mut self, n: u64) {
        self.sent = self.sent.saturating_add(n);
    }

    pub fn update_credit(&mut self, new_max: u64) {
        if new_max > self.max_data {
            self.max_data = new_max;
        }
    }

    pub fn on_receive(&mut self, n: u64) {
        self.received = self.received.saturating_add(n);
    }
}

/// Flow control manager for multiple streams.
#[derive(Debug, Clone)]
pub struct FlowControl {
    pub connection: ConnectionFlowControl,
    pub streams: std::collections::BTreeMap<u32, StreamFlowControl>,
    pub default_stream_credit: u64,
}

impl FlowControl {
    pub fn new(connection_credit: u64, stream_credit: u64) -> Self {
        Self {
            connection: ConnectionFlowControl::new(connection_credit),
            streams: std::collections::BTreeMap::new(),
            default_stream_credit: stream_credit,
        }
    }

    /// Get or create flow control for a stream.
    pub fn stream(&mut self, stream_id: u32) -> &mut StreamFlowControl {
        self.streams
            .entry(stream_id)
            .or_insert_with(|| StreamFlowControl::new(self.default_stream_credit))
    }

    /// Whether `n` bytes can be sent on `stream_id`.
    pub fn can_send(&mut self, stream_id: u32, n: u64) -> bool {
        if !self.connection.can_send(n) {
            return false;
        }
        self.stream(stream_id).can_send(n)
    }

    /// Record that `n` bytes were sent on `stream_id`.
    pub fn on_send(&mut self, stream_id: u32, n: u64) {
        self.connection.on_send(n);
        self.stream(stream_id).on_send(n);
    }

    /// Update stream credit.
    pub fn update_stream_credit(&mut self, stream_id: u32, new_max: u64) {
        self.stream(stream_id).update_credit(new_max);
    }

    /// Update connection credit.
    pub fn update_connection_credit(&mut self, new_max: u64) {
        self.connection.update_credit(new_max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_credit() {
        let mut fc = StreamFlowControl::new(1000);
        assert!(fc.can_send(500));
        fc.on_send(500);
        assert!(fc.can_send(500));
        assert!(!fc.can_send(501));
        fc.update_credit(2000);
        assert!(fc.can_send(1000));
    }

    #[test]
    fn connection_credit() {
        let mut fc = ConnectionFlowControl::new(2000);
        assert!(fc.can_send(1500));
        fc.on_send(1500);
        assert!(!fc.can_send(600));
        fc.update_credit(4000);
        assert!(fc.can_send(2000));
    }

    #[test]
    fn flow_control_manager() {
        let mut fc = FlowControl::new(5000, 1000);
        assert!(fc.can_send(0, 500));
        fc.on_send(0, 500);
        assert!(fc.can_send(1, 500));
        fc.on_send(1, 500);
        // connection credit exhausted
        assert!(!fc.can_send(0, 4100));
    }
}
