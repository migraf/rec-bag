//! Debounced catalog-snapshot scheduling.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SnapshotTrigger {
    SuccessfulSync,
    ReconciliationChange,
    CatalogEdit,
    BeforePrimaryMigration,
    BeforeRetentionCleanup,
    Daily,
}

/// In-memory scheduling state that persistence adapters can serialize alongside their
/// settings. Repeated triggers inside the debounce window coalesce into one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDebouncer {
    debounce_window: Duration,
    last_created_at: Option<DateTime<Utc>>,
    pending: BTreeSet<SnapshotTrigger>,
}

impl SnapshotDebouncer {
    pub fn new(debounce_window: Duration) -> Self {
        Self {
            debounce_window: debounce_window.max(Duration::zero()),
            last_created_at: None,
            pending: BTreeSet::new(),
        }
    }

    pub fn record(&mut self, trigger: SnapshotTrigger) {
        self.pending.insert(trigger);
    }

    pub fn is_due_at(&self, now: DateTime<Utc>) -> bool {
        !self.pending.is_empty()
            && self
                .last_created_at
                .is_none_or(|last| now >= last + self.debounce_window)
    }

    /// Marks the pending batch created after its snapshot payload has been persisted.
    /// Returns the coalesced triggers and clears only that batch.
    pub fn mark_created_at(&mut self, now: DateTime<Utc>) -> Vec<SnapshotTrigger> {
        if !self.is_due_at(now) {
            return Vec::new();
        }
        self.last_created_at = Some(now);
        std::mem::take(&mut self.pending).into_iter().collect()
    }

    /// Atomically consumes a due batch from persistable scheduling state.
    pub fn take_due_at(&mut self, now: DateTime<Utc>) -> Option<Vec<SnapshotTrigger>> {
        self.is_due_at(now).then(|| self.mark_created_at(now))
    }

    pub const fn last_created_at(&self) -> Option<DateTime<Utc>> {
        self.last_created_at
    }

    pub fn pending_triggers(&self) -> impl Iterator<Item = SnapshotTrigger> + '_ {
        self.pending.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn coalesces_snapshot_triggers_inside_debounce_window() {
        let first = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
        let mut debounce = SnapshotDebouncer::new(Duration::minutes(5));
        debounce.record(SnapshotTrigger::CatalogEdit);
        assert!(debounce.is_due_at(first));
        assert_eq!(
            debounce.mark_created_at(first),
            vec![SnapshotTrigger::CatalogEdit]
        );
        debounce.record(SnapshotTrigger::SuccessfulSync);
        assert!(!debounce.is_due_at(first + Duration::minutes(4)));
        assert!(debounce.is_due_at(first + Duration::minutes(5)));
    }
}
