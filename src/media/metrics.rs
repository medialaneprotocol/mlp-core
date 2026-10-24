//! Prometheus-style metrics export (MLP).
//!
//! Renders protocol telemetry as Prometheus text exposition format so a
//! deployment can scrape it.

use std::sync::atomic::{AtomicU64, Ordering};

/// A telemetry sink that records Prometheus counters.
pub struct PrometheusTelemetry {
    fragments_sent: AtomicU64,
    fragments_received: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_received: AtomicU64,
    updates_applied: AtomicU64,
    convergences: AtomicU64,
    losses: AtomicU64,
    echoes: AtomicU64,
}

impl Default for PrometheusTelemetry {
    fn default() -> Self {
        Self {
            fragments_sent: AtomicU64::new(0),
            fragments_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            updates_applied: AtomicU64::new(0),
            convergences: AtomicU64::new(0),
            losses: AtomicU64::new(0),
            echoes: AtomicU64::new(0),
        }
    }
}

impl PrometheusTelemetry {
    pub fn record_fragment_sent(&self, bytes: u64) {
        self.fragments_sent.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }
    pub fn record_fragment_received(&self, bytes: u64) {
        self.fragments_received.fetch_add(1, Ordering::Relaxed);
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }
    pub fn record_update_applied(&self) {
        self.updates_applied.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_convergence(&self) {
        self.convergences.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_loss(&self) {
        self.losses.fetch_add(1, Ordering::Relaxed);
    }
    pub fn record_echo(&self) {
        self.echoes.fetch_add(1, Ordering::Relaxed);
    }

    /// Render as Prometheus text exposition format.
    pub fn render(&self) -> String {
        format!(
            "# HELP mlp_fragments_sent_total Total fragments sent.\n\
             # TYPE mlp_fragments_sent_total counter\n\
             mlp_fragments_sent_total {}\n\
             # HELP mlp_fragments_received_total Total fragments received.\n\
             # TYPE mlp_fragments_received_total counter\n\
             mlp_fragments_received_total {}\n\
             # HELP mlp_bytes_sent_total Total bytes sent.\n\
             # TYPE mlp_bytes_sent_total counter\n\
             mlp_bytes_sent_total {}\n\
             # HELP mlp_bytes_received_total Total bytes received.\n\
             # TYPE mlp_bytes_received_total counter\n\
             mlp_bytes_received_total {}\n\
             # HELP mlp_updates_applied_total Total updates applied.\n\
             # TYPE mlp_updates_applied_total counter\n\
             mlp_updates_applied_total {}\n\
             # HELP mlp_convergences_total Total convergences.\n\
             # TYPE mlp_convergences_total counter\n\
             mlp_convergences_total {}\n\
             # HELP mlp_losses_total Total losses detected.\n\
             # TYPE mlp_losses_total counter\n\
             mlp_losses_total {}\n\
             # HELP mlp_echoes_total Total echoes measured.\n\
             # TYPE mlp_echoes_total counter\n\
             mlp_echoes_total {}\n",
            self.fragments_sent.load(Ordering::Relaxed),
            self.fragments_received.load(Ordering::Relaxed),
            self.bytes_sent.load(Ordering::Relaxed),
            self.bytes_received.load(Ordering::Relaxed),
            self.updates_applied.load(Ordering::Relaxed),
            self.convergences.load(Ordering::Relaxed),
            self.losses.load(Ordering::Relaxed),
            self.echoes.load(Ordering::Relaxed),
        )
    }
}

impl std::fmt::Debug for PrometheusTelemetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrometheusTelemetry")
            .field(
                "fragments_sent",
                &self.fragments_sent.load(Ordering::Relaxed),
            )
            .field(
                "fragments_received",
                &self.fragments_received.load(Ordering::Relaxed),
            )
            .field("bytes_sent", &self.bytes_sent.load(Ordering::Relaxed))
            .field(
                "bytes_received",
                &self.bytes_received.load(Ordering::Relaxed),
            )
            .field(
                "updates_applied",
                &self.updates_applied.load(Ordering::Relaxed),
            )
            .field("convergences", &self.convergences.load(Ordering::Relaxed))
            .field("losses", &self.losses.load(Ordering::Relaxed))
            .field("echoes", &self.echoes.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_metrics() {
        let t = PrometheusTelemetry::default();
        t.record_fragment_sent(100);
        t.record_update_applied();
        let out = t.render();
        assert!(out.contains("mlp_fragments_sent_total 1"));
        assert!(out.contains("mlp_bytes_sent_total 100"));
        assert!(out.contains("mlp_updates_applied_total 1"));
    }
}
