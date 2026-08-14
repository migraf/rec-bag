//! Non-destructive backup version retention evaluation.
//!
//! `MusicAssetStore` intentionally has no delete operation. This module identifies
//! cleanup candidates and requires an explicit confirmation flow before any future
//! provider-specific deletion contract is invoked.

use chrono::{DateTime, Duration, Utc};
use rec_core::{RetentionPolicy, StorageObjectVersion};
use std::collections::BTreeSet;
use uuid::Uuid;

use crate::snapshots::SnapshotTrigger;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionStatus {
    /// Policy could be evaluated; cleanup candidates are review-only.
    ReviewRequired,
    /// Policy could be evaluated and its candidates have already been confirmed.
    ConfirmedForCleanup,
    /// The provider does not expose retained versions, so enforcement is impossible.
    VersionHistoryUnsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupCandidate {
    pub object_id: String,
    pub version_id: String,
    pub modified_at: Option<DateTime<Utc>>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionEvaluation {
    pub status: RetentionStatus,
    pub retained_version_ids: Vec<String>,
    pub cleanup_candidates: Vec<CleanupCandidate>,
}

/// Safe response to a primary deletion or tombstone. No variant instructs a
/// backup provider to delete data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryDeletionDecision {
    RetainVerifiedBackup {
        asset_id: Uuid,
        backup_storage_id: Uuid,
        object_id: String,
    },
    PreservePendingReview {
        asset_id: Uuid,
        backup_storage_id: Uuid,
        object_id: String,
    },
    BackupUnavailable {
        asset_id: Uuid,
        backup_storage_id: Uuid,
    },
}

pub fn plan_primary_deletion_retention(
    asset_id: Uuid,
    backup_storage_id: Uuid,
    backup_object_id: Option<String>,
    backup_verified: bool,
    backup_available: bool,
) -> PrimaryDeletionDecision {
    if !backup_available {
        PrimaryDeletionDecision::BackupUnavailable {
            asset_id,
            backup_storage_id,
        }
    } else if let Some(object_id) = backup_object_id {
        if backup_verified {
            PrimaryDeletionDecision::RetainVerifiedBackup {
                asset_id,
                backup_storage_id,
                object_id,
            }
        } else {
            PrimaryDeletionDecision::PreservePendingReview {
                asset_id,
                backup_storage_id,
                object_id,
            }
        }
    } else {
        PrimaryDeletionDecision::BackupUnavailable {
            asset_id,
            backup_storage_id,
        }
    }
}

/// External approval must identify the exact cleanup candidate set being shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupApproval {
    pub candidate_keys: BTreeSet<String>,
    pub approved_at: DateTime<Utc>,
    pub approved_by: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPlan {
    pub candidates: Vec<CleanupCandidate>,
    pub approved: bool,
    pub approval_error: Option<String>,
    pub required_snapshot_trigger: SnapshotTrigger,
}

impl CleanupCandidate {
    pub fn key(&self) -> String {
        format!("{}:{}", self.object_id, self.version_id)
    }
}

/// Converts review candidates into a provider-neutral cleanup plan. Persistence
/// must record the approval and successful pre-cleanup snapshot before invoking
/// any provider-specific destructive implementation.
pub fn plan_cleanup(
    evaluations: &[RetentionEvaluation],
    approval: Option<&CleanupApproval>,
) -> CleanupPlan {
    let mut candidates: Vec<_> = evaluations
        .iter()
        .flat_map(|evaluation| evaluation.cleanup_candidates.iter().cloned())
        .collect();
    candidates.sort_by(|left, right| left.key().cmp(&right.key()));
    candidates.dedup_by(|left, right| left.key() == right.key());
    let expected: BTreeSet<_> = candidates.iter().map(CleanupCandidate::key).collect();
    let approval_error = match approval {
        None if !candidates.is_empty() => Some("explicit cleanup approval is required".into()),
        Some(approval) if approval.approved_by.trim().is_empty() => {
            Some("cleanup approval must identify the approver".into())
        }
        Some(approval) if approval.candidate_keys != expected => {
            Some("cleanup approval does not match the candidate set".into())
        }
        _ => None,
    };
    CleanupPlan {
        approved: approval_error.is_none(),
        approval_error,
        candidates,
        required_snapshot_trigger: SnapshotTrigger::BeforeRetentionCleanup,
    }
}

/// Evaluates the safe default retention rule:
///
/// * the current version is always retained;
/// * replaced/deleted versions younger than `retention_days` are retained;
/// * at least `minimum_versions` most recent versions are retained;
/// * candidates are review-only while confirmation is required.
pub fn evaluate_retention(
    object_id: impl Into<String>,
    versions: &[StorageObjectVersion],
    policy: RetentionPolicy,
    now: DateTime<Utc>,
    supports_version_history: bool,
    cleanup_confirmed: bool,
) -> RetentionEvaluation {
    if !supports_version_history {
        return RetentionEvaluation {
            status: RetentionStatus::VersionHistoryUnsupported,
            retained_version_ids: versions
                .iter()
                .map(|version| version.version_id.clone())
                .collect(),
            cleanup_candidates: Vec::new(),
        };
    }

    let object_id = object_id.into();
    let cutoff = now - Duration::days(i64::from(policy.retention_days));
    let mut indexed: Vec<_> = versions.iter().enumerate().collect();
    indexed.sort_by(|(left_index, left), (right_index, right)| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| left_index.cmp(right_index))
    });

    let protected_recent: std::collections::BTreeSet<_> = indexed
        .iter()
        .take(policy.minimum_versions as usize)
        .map(|(index, _)| *index)
        .collect();
    let mut retained = Vec::new();
    let mut cleanup_candidates = Vec::new();
    for (index, version) in versions.iter().enumerate() {
        let retained_by_age = version
            .modified_at
            .is_none_or(|modified_at| modified_at >= cutoff);
        if version.is_current || protected_recent.contains(&index) || retained_by_age {
            retained.push(version.version_id.clone());
        } else {
            cleanup_candidates.push(CleanupCandidate {
                object_id: object_id.clone(),
                version_id: version.version_id.clone(),
                modified_at: version.modified_at,
                reason: format!(
                    "older than {} days and outside the {} newest versions",
                    policy.retention_days, policy.minimum_versions
                ),
            });
        }
    }
    retained.sort();
    cleanup_candidates.sort_by(|left, right| left.version_id.cmp(&right.version_id));
    RetentionEvaluation {
        status: if policy.require_cleanup_confirmation && !cleanup_confirmed {
            RetentionStatus::ReviewRequired
        } else {
            RetentionStatus::ConfirmedForCleanup
        },
        retained_version_ids: retained,
        cleanup_candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn version(id: &str, days_ago: i64, current: bool) -> StorageObjectVersion {
        let now = Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).single().unwrap();
        StorageObjectVersion {
            object_id: "object".into(),
            version_id: id.into(),
            modified_at: Some(now - Duration::days(days_ago)),
            size_bytes: None,
            hashes: Vec::new(),
            is_current: current,
        }
    }

    #[test]
    fn primary_deletion_never_produces_a_backup_delete_instruction() {
        let asset_id = Uuid::new_v4();
        let backup_id = Uuid::new_v4();
        assert!(matches!(
            plan_primary_deletion_retention(
                asset_id,
                backup_id,
                Some("backup-object".into()),
                true,
                true,
            ),
            PrimaryDeletionDecision::RetainVerifiedBackup { .. }
        ));
        assert!(matches!(
            plan_primary_deletion_retention(asset_id, backup_id, None, false, false),
            PrimaryDeletionDecision::BackupUnavailable { .. }
        ));
    }

    #[test]
    fn cleanup_requires_exact_external_approval_and_snapshot() {
        let now = Utc::now();
        let evaluation = RetentionEvaluation {
            status: RetentionStatus::ReviewRequired,
            retained_version_ids: vec![],
            cleanup_candidates: vec![CleanupCandidate {
                object_id: "object".into(),
                version_id: "old".into(),
                modified_at: Some(now),
                reason: "expired".into(),
            }],
        };
        let review = plan_cleanup(&[evaluation.clone()], None);
        assert!(!review.approved);
        assert_eq!(
            review.required_snapshot_trigger,
            SnapshotTrigger::BeforeRetentionCleanup
        );
        let approved = plan_cleanup(
            &[evaluation],
            Some(&CleanupApproval {
                candidate_keys: BTreeSet::from(["object:old".into()]),
                approved_at: now,
                approved_by: "user".into(),
            }),
        );
        assert!(approved.approved);
    }

    #[test]
    fn expiration_preserves_current_and_three_newest_versions() {
        let now = Utc.with_ymd_and_hms(2026, 1, 31, 0, 0, 0).single().unwrap();
        let policy = RetentionPolicy::default();
        let evaluation = evaluate_retention(
            "object",
            &[
                version("current", 100, true),
                version("newest", 40, false),
                version("middle", 50, false),
                version("old", 60, false),
                version("expired", 70, false),
            ],
            policy,
            now,
            true,
            false,
        );
        assert_eq!(evaluation.status, RetentionStatus::ReviewRequired);
        assert!(evaluation
            .retained_version_ids
            .contains(&"current".to_string()));
        assert!(evaluation
            .retained_version_ids
            .contains(&"newest".to_string()));
        assert!(evaluation
            .retained_version_ids
            .contains(&"middle".to_string()));
        assert_eq!(evaluation.cleanup_candidates[0].version_id, "expired");
    }
}
