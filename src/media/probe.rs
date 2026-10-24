//! Path probing (MLP spec §8.9): active probing of candidate paths to
//! measure their capacity and loss before migrating traffic to them.

use std::time::Instant;

/// A probe result for one path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeResult {
    /// Path id.
    pub path_id: u8,
    /// Measured RTT in ms.
    pub rtt_ms: f64,
    /// Measured loss ratio (0.0..=1.0).
    pub loss_ratio: f64,
    /// Measured goodput in bps.
    pub goodput_bps: u64,
    /// When the probe completed.
    pub completed_at: Instant,
}

/// A path prober that sends probe packets and collects results.
#[derive(Debug, Clone)]
pub struct PathProber {
    /// Probe interval.
    pub interval_ms: u64,
    /// Number of probes sent per path.
    pub probes_per_path: u32,
    /// Results per path.
    pub results: Vec<ProbeResult>,
    /// Last probe time.
    pub last_probe: Option<Instant>,
}

impl PathProber {
    pub fn new(interval_ms: u64, probes_per_path: u32) -> Self {
        Self {
            interval_ms,
            probes_per_path,
            results: Vec::new(),
            last_probe: None,
        }
    }

    /// Whether it's time to send a probe.
    pub fn should_probe(&self, now: Instant) -> bool {
        match self.last_probe {
            None => true,
            Some(last) => now.duration_since(last).as_millis() as u64 >= self.interval_ms,
        }
    }

    /// Record a probe result.
    pub fn record(&mut self, result: ProbeResult) {
        self.results.push(result);
        self.last_probe = Some(result.completed_at);
    }

    /// Best path by (lowest RTT, then lowest loss).
    pub fn best_path(&self) -> Option<u8> {
        self.results
            .iter()
            .min_by(|a, b| {
                a.rtt_ms
                    .partial_cmp(&b.rtt_ms)
                    .unwrap()
                    .then(a.loss_ratio.partial_cmp(&b.loss_ratio).unwrap())
            })
            .map(|r| r.path_id)
    }

    /// Results for a specific path.
    pub fn path_results(&self, path_id: u8) -> Vec<ProbeResult> {
        self.results
            .iter()
            .filter(|r| r.path_id == path_id)
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_path_by_rtt() {
        let mut prober = PathProber::new(1000, 3);
        let now = Instant::now();
        prober.record(ProbeResult {
            path_id: 0,
            rtt_ms: 100.0,
            loss_ratio: 0.05,
            goodput_bps: 1_000_000,
            completed_at: now,
        });
        prober.record(ProbeResult {
            path_id: 1,
            rtt_ms: 50.0,
            loss_ratio: 0.01,
            goodput_bps: 2_000_000,
            completed_at: now,
        });
        assert_eq!(prober.best_path(), Some(1));
    }

    #[test]
    fn should_probe_initially() {
        let prober = PathProber::new(1000, 3);
        assert!(prober.should_probe(Instant::now()));
    }
}
