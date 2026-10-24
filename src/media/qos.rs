//! Quality of Service (MLP spec §6.11): per-stream priority and bandwidth
//! allocation. Maps application-level quality tiers to MLP priorities and
//! pacer weights.

/// QoS tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QosTier {
    /// Background (lowest priority).
    Background = 0,
    /// Best effort.
    BestEffort = 1,
    /// Interactive (default for media).
    Interactive = 2,
    /// Real-time (highest priority).
    Realtime = 3,
}

impl QosTier {
    /// Map to MLP priority (lower = higher).
    pub const fn mlp_priority(self) -> u8 {
        match self {
            QosTier::Background => 7,
            QosTier::BestEffort => 6,
            QosTier::Interactive => 5,
            QosTier::Realtime => 2,
        }
    }

    /// Bandwidth weight (higher = more share).
    pub const fn weight(self) -> u32 {
        match self {
            QosTier::Background => 1,
            QosTier::BestEffort => 2,
            QosTier::Interactive => 4,
            QosTier::Realtime => 8,
        }
    }
}

/// QoS policy for a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QosPolicy {
    pub tier: QosTier,
    pub max_bandwidth_bps: u64,
    pub min_bandwidth_bps: u64,
}

impl QosPolicy {
    pub fn new(tier: QosTier) -> Self {
        Self {
            tier,
            max_bandwidth_bps: u64::MAX,
            min_bandwidth_bps: 0,
        }
    }

    pub fn with_max_bandwidth(mut self, bps: u64) -> Self {
        self.max_bandwidth_bps = bps;
        self
    }

    pub fn with_min_bandwidth(mut self, bps: u64) -> Self {
        self.min_bandwidth_bps = bps;
        self
    }
}

/// Allocate bandwidth among streams by weight.
pub fn allocate_bandwidth(policies: &[QosPolicy], total_bps: u64) -> Vec<u64> {
    let total_weight: u32 = policies.iter().map(|p| p.tier.weight()).sum();
    if total_weight == 0 {
        return vec![0; policies.len()];
    }
    policies
        .iter()
        .map(|p| {
            let share = (total_bps as u128 * p.tier.weight() as u128 / total_weight as u128) as u64;
            share.clamp(p.min_bandwidth_bps, p.max_bandwidth_bps)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering() {
        assert!(QosTier::Realtime.mlp_priority() < QosTier::Interactive.mlp_priority());
        assert!(QosTier::Interactive.mlp_priority() < QosTier::BestEffort.mlp_priority());
    }

    #[test]
    fn allocation_sums() {
        let policies = vec![
            QosPolicy::new(QosTier::Realtime),
            QosPolicy::new(QosTier::Interactive),
        ];
        let alloc = allocate_bandwidth(&policies, 1000);
        assert_eq!(alloc.len(), 2);
        // realtime gets more (weight 8 vs 4)
        assert!(alloc[0] > alloc[1]);
    }

    #[test]
    fn allocation_clamps() {
        let policies = vec![QosPolicy::new(QosTier::Realtime).with_max_bandwidth(100)];
        let alloc = allocate_bandwidth(&policies, 10000);
        assert_eq!(alloc[0], 100);
    }
}
