//! Connection migration (MLP spec §8.8): when a viewer's path changes
//! (NAT rebinding, network switch), the session migrates to the new path
//! without dropping state. The sender validates the new path with a
//! challenge-response before migrating.

use std::time::Instant;

/// State of a path migration attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationState {
    /// No migration in progress.
    Idle,
    /// Challenge sent, awaiting response.
    Probing,
    /// Migration complete, new path active.
    Migrated,
    /// Migration failed, old path retained.
    Failed,
}

/// A path migration manager.
#[derive(Debug, Clone)]
pub struct PathMigration {
    pub state: MigrationState,
    /// The current active path id.
    pub active_path: u8,
    /// The candidate path id being probed.
    pub candidate_path: u8,
    /// Challenge nonce.
    pub challenge: u64,
    /// Number of failed migration attempts.
    pub failures: u32,
}

impl PathMigration {
    pub fn new(active_path: u8) -> Self {
        Self {
            state: MigrationState::Idle,
            active_path,
            candidate_path: 0,
            challenge: 0,
            failures: 0,
        }
    }

    /// Start probing a candidate path.
    pub fn start_probe(&mut self, candidate: u8, now: Instant) {
        self.state = MigrationState::Probing;
        self.candidate_path = candidate;
        self.challenge = now.elapsed().as_nanos() as u64;
    }

    /// Complete migration if challenge response matches.
    pub fn complete(&mut self, response: u64) -> bool {
        if self.state != MigrationState::Probing {
            return false;
        }
        if response == self.challenge {
            self.active_path = self.candidate_path;
            self.state = MigrationState::Migrated;
            true
        } else {
            self.state = MigrationState::Failed;
            self.failures += 1;
            false
        }
    }

    /// Reset to idle after migration.
    pub fn reset(&mut self) {
        self.state = MigrationState::Idle;
        self.candidate_path = 0;
    }

    /// Whether migration is in progress.
    pub fn is_probing(&self) -> bool {
        self.state == MigrationState::Probing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_and_migrate() {
        let mut m = PathMigration::new(0);
        let now = Instant::now();
        m.start_probe(1, now);
        assert!(m.is_probing());
        let ch = m.challenge;
        assert!(m.complete(ch));
        assert_eq!(m.active_path, 1);
        assert_eq!(m.state, MigrationState::Migrated);
    }

    #[test]
    fn bad_response_fails() {
        let mut m = PathMigration::new(0);
        let now = Instant::now();
        m.start_probe(1, now);
        assert!(!m.complete(0));
        assert_eq!(m.active_path, 0);
        assert_eq!(m.failures, 1);
    }
}
