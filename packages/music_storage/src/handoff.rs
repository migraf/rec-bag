//! The seam through which verified primary writes reach replication.

use rec_core::BoxFuture;
use uuid::Uuid;

/// Receives every verified write to the primary so replication can plan
/// copies to backups. Implemented by the desktop's backup service.
pub trait ReplicationHandoff: Send + Sync {
    /// Plans replication for `asset_ids`; an empty slice means every verified
    /// primary asset. Never fails the primary write: problems are reported in
    /// the returned [`HandoffReport`].
    fn verified_primary_write<'a>(&'a self, asset_ids: &'a [Uuid]) -> BoxFuture<'a, HandoffReport>;
}

/// What a replication handoff planned after a verified primary write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffReport {
    pub planned_assets: usize,
    pub queued_jobs: usize,
    pub deferred_targets: usize,
    /// Non-blocking problems; they never undo the primary write.
    pub warnings: Vec<String>,
}

impl HandoffReport {
    /// A one-line summary suitable for an operation outcome message.
    pub fn message(&self) -> String {
        if self.planned_assets == 0 && self.warnings.is_empty() {
            return "Backup handoff complete: no verified primary assets required planning".into();
        }
        let mut message = format!(
            "Backup handoff: {} verified asset(s), {} durable job(s) queued, {} unavailable target(s) deferred",
            self.planned_assets, self.queued_jobs, self.deferred_targets
        );
        if !self.warnings.is_empty() {
            message.push_str("; non-blocking issues: ");
            message.push_str(&self.warnings.join("; "));
        }
        message
    }
}

/// A handoff that plans nothing, for compositions without replication.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHandoff;

impl ReplicationHandoff for NoHandoff {
    fn verified_primary_write<'a>(
        &'a self,
        _asset_ids: &'a [Uuid],
    ) -> BoxFuture<'a, HandoffReport> {
        Box::pin(std::future::ready(HandoffReport::default()))
    }
}
