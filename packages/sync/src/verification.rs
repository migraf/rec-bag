//! Deterministic backup verification scheduling.

use chrono::{DateTime, Datelike, Utc};
use rec_core::{SyncJobPayload, SYNC_JOB_PAYLOAD_VERSION};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationCadence {
    ImmediateAfterReplication,
    ProviderVersionChanged,
    DailyRotatingSample,
    MonthlyFullInventory,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationPlan {
    Verify(SyncJobPayload),
    Inventory(SyncJobPayload),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationRequest {
    pub cadence: VerificationCadence,
    pub asset_id: Uuid,
    pub storage_id: Uuid,
    pub object_id: String,
    pub object_key: Option<String>,
    pub expected_checksum: Option<String>,
    pub known_provider_version: Option<String>,
    pub observed_provider_version: Option<String>,
    /// Stable position in object-ID sorted backup inventory.
    pub inventory_ordinal: u64,
}

pub struct VerificationPlanner;

impl VerificationPlanner {
    /// Plans immediate, changed-version, daily, and manual object verification.
    /// Monthly full inventory remains available through [`Self::monthly_inventory`].
    pub fn schedule(
        request: &VerificationRequest,
        now: DateTime<Utc>,
        sample_modulus: u64,
    ) -> Option<VerificationPlan> {
        let due = match request.cadence {
            VerificationCadence::ImmediateAfterReplication | VerificationCadence::Manual => true,
            VerificationCadence::ProviderVersionChanged => {
                request.known_provider_version != request.observed_provider_version
                    && request.observed_provider_version.is_some()
            }
            VerificationCadence::DailyRotatingSample => {
                Self::is_daily_sample(request.inventory_ordinal, now, sample_modulus)
            }
            VerificationCadence::MonthlyFullInventory => false,
        };
        due.then(|| {
            Self::verify_asset(
                request.asset_id,
                request.storage_id,
                request.object_id.clone(),
                request.object_key.clone(),
                request.expected_checksum.clone(),
            )
        })
    }
    pub fn verify_asset(
        asset_id: Uuid,
        storage_id: Uuid,
        object_id: impl Into<String>,
        object_key: Option<String>,
        expected_checksum: Option<String>,
    ) -> VerificationPlan {
        VerificationPlan::Verify(SyncJobPayload::VerifyAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id,
            storage_id,
            object_id: object_id.into(),
            object_key,
            expected_checksum,
        })
    }

    /// Selects a stable rotating daily sample without using random order. `ordinal`
    /// should be the object's stable position in a sorted backup inventory.
    pub fn is_daily_sample(ordinal: u64, day: DateTime<Utc>, sample_modulus: u64) -> bool {
        let modulus = sample_modulus.max(1);
        ordinal % modulus == day.ordinal() as u64 % modulus
    }

    /// Creates the monthly full inventory plan on the first day of each month. The
    /// caller enqueues it only if one has not already succeeded for that month.
    pub fn monthly_inventory(
        storage_id: Uuid,
        now: DateTime<Utc>,
        inventory_root: Option<String>,
    ) -> Option<VerificationPlan> {
        (now.day() == 1).then(|| {
            VerificationPlan::Inventory(SyncJobPayload::InventoryStoragePage {
                version: SYNC_JOB_PAYLOAD_VERSION,
                storage_id,
                inventory_root,
                change_cursor: None,
                page_cursor: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn immediate_changed_daily_and_manual_verification_are_scheduled() {
        let now = Utc.with_ymd_and_hms(2026, 2, 2, 0, 0, 0).single().unwrap();
        let mut request = VerificationRequest {
            cadence: VerificationCadence::ImmediateAfterReplication,
            asset_id: Uuid::new_v4(),
            storage_id: Uuid::new_v4(),
            object_id: "object".into(),
            object_key: None,
            expected_checksum: Some("sha256:value".into()),
            known_provider_version: Some("v1".into()),
            observed_provider_version: Some("v2".into()),
            inventory_ordinal: u64::from(now.ordinal()) % 5,
        };
        assert!(VerificationPlanner::schedule(&request, now, 5).is_some());
        request.cadence = VerificationCadence::ProviderVersionChanged;
        assert!(VerificationPlanner::schedule(&request, now, 5).is_some());
        request.observed_provider_version = Some("v1".into());
        assert!(VerificationPlanner::schedule(&request, now, 5).is_none());
        request.cadence = VerificationCadence::DailyRotatingSample;
        assert!(VerificationPlanner::schedule(&request, now, 5).is_some());
        request.cadence = VerificationCadence::Manual;
        assert!(VerificationPlanner::schedule(&request, now, 5).is_some());
    }

    #[test]
    fn daily_sample_is_deterministic_and_monthly_inventory_is_gated() {
        let first = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).single().unwrap();
        assert_eq!(
            VerificationPlanner::is_daily_sample(4, first, 5),
            VerificationPlanner::is_daily_sample(4, first, 5)
        );
        assert!(VerificationPlanner::monthly_inventory(Uuid::nil(), first, None).is_some());
        assert!(VerificationPlanner::monthly_inventory(
            Uuid::nil(),
            first + chrono::Duration::days(1),
            None
        )
        .is_none());
    }
}
