//! Provider-independent catalog schema and migration contracts.
//!
//! This module defines the logical structures a persistence crate must preserve and
//! the rules for planning catalog migrations. It intentionally contains no SQL,
//! database connection types, or migration execution code.

use chrono::{DateTime, Utc};
use serde::{de, Deserialize, Deserializer, Serialize};
use std::error::Error;
use std::fmt;
use uuid::Uuid;

/// The existing, pre-provider catalog layout.
///
/// Persistence implementations should treat an unversioned legacy catalog that has
/// the original tracks, playlists, and local sources layout as this version.
pub const LEGACY_CATALOG_SCHEMA_VERSION: CatalogSchemaVersion = CatalogSchemaVersion(1);

/// The current provider-independent catalog layout required by the core contracts.
pub const CURRENT_CATALOG_SCHEMA_VERSION: CatalogSchemaVersion = CatalogSchemaVersion(4);

/// A validated catalog schema version.
///
/// Versions begin at one. Zero is reserved for missing or corrupt metadata and is
/// never a valid persisted schema version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CatalogSchemaVersion(u32);

impl CatalogSchemaVersion {
    /// Creates a schema version, rejecting the reserved zero value.
    pub const fn new(value: u32) -> Result<Self, CatalogSchemaVersionError> {
        if value == 0 {
            Err(CatalogSchemaVersionError { value })
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the numeric representation used in persistent schema metadata.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns the immediately following version, if the numeric range permits it.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

impl TryFrom<u32> for CatalogSchemaVersion {
    type Error = CatalogSchemaVersionError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CatalogSchemaVersion> for u32 {
    fn from(version: CatalogSchemaVersion) -> Self {
        version.get()
    }
}

impl fmt::Display for CatalogSchemaVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for CatalogSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Error returned when a reserved or otherwise invalid schema version is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSchemaVersionError {
    pub value: u32,
}

impl fmt::Display for CatalogSchemaVersionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "catalog schema version must be greater than zero, got {}",
            self.value
        )
    }
}

impl Error for CatalogSchemaVersionError {}

/// Declares whether a migration can discard catalog information or modify media.
///
/// Stage 1 plans accept only [`MigrationSafety::NonDestructive`] steps. The
/// destructive marker exists so a future, separately confirmed maintenance flow
/// cannot accidentally pass a destructive operation through the normal planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationSafety {
    /// Preserves logical catalog records and does not move, rename, copy, or delete
    /// audio files.
    NonDestructive,
    /// May discard logical data or alter media and therefore requires a future,
    /// explicit workflow outside [`MigrationPlan`].
    Destructive,
}

/// A declarative transition between two catalog schema versions.
///
/// A step describes intent only. Persistence crates own the implementation and must
/// execute accepted plans transactionally where their backend permits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStep {
    pub from_version: CatalogSchemaVersion,
    pub to_version: CatalogSchemaVersion,
    /// Stable human-readable intent suitable for diagnostics and migration reports.
    pub description: String,
    pub safety: MigrationSafety,
}

impl MigrationStep {
    /// Creates a migration step descriptor. The step is validated when included in a
    /// [`MigrationPlan`].
    pub fn new(
        from_version: CatalogSchemaVersion,
        to_version: CatalogSchemaVersion,
        description: impl Into<String>,
        safety: MigrationSafety,
    ) -> Self {
        Self {
            from_version,
            to_version,
            description: description.into(),
            safety,
        }
    }

    /// Creates a step eligible for the normal catalog migration planner.
    pub fn non_destructive(
        from_version: CatalogSchemaVersion,
        to_version: CatalogSchemaVersion,
        description: impl Into<String>,
    ) -> Self {
        Self::new(
            from_version,
            to_version,
            description,
            MigrationSafety::NonDestructive,
        )
    }

    /// Creates a descriptor that the normal planner will reject.
    pub fn destructive(
        from_version: CatalogSchemaVersion,
        to_version: CatalogSchemaVersion,
        description: impl Into<String>,
    ) -> Self {
        Self::new(
            from_version,
            to_version,
            description,
            MigrationSafety::Destructive,
        )
    }
}

/// A validated, ordered sequence of forward catalog migrations.
///
/// Every accepted step advances exactly one version, starts at the preceding step's
/// destination, and is non-destructive. A plan with no steps is valid only when its
/// starting and target versions are equal. Downgrade plans are not supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationPlan {
    from_version: CatalogSchemaVersion,
    target_version: CatalogSchemaVersion,
    steps: Vec<MigrationStep>,
}

impl MigrationPlan {
    /// Validates and creates a migration plan.
    pub fn new(
        from_version: CatalogSchemaVersion,
        target_version: CatalogSchemaVersion,
        steps: Vec<MigrationStep>,
    ) -> Result<Self, MigrationPlanError> {
        if target_version < from_version {
            return Err(MigrationPlanError::TargetBeforeStart {
                from_version,
                target_version,
            });
        }

        let mut expected_from = from_version;

        for (index, step) in steps.iter().enumerate() {
            if step.safety != MigrationSafety::NonDestructive {
                return Err(MigrationPlanError::DestructiveStep {
                    index,
                    from_version: step.from_version,
                    to_version: step.to_version,
                });
            }

            if step.to_version <= step.from_version {
                return Err(MigrationPlanError::StepNotForward {
                    index,
                    from_version: step.from_version,
                    to_version: step.to_version,
                });
            }

            if step.from_version != expected_from {
                return Err(MigrationPlanError::StepNotContiguous {
                    index,
                    expected_from,
                    actual_from: step.from_version,
                });
            }

            let expected_to =
                step.from_version
                    .checked_next()
                    .ok_or(MigrationPlanError::VersionOverflow {
                        index,
                        from_version: step.from_version,
                    })?;

            if step.to_version != expected_to {
                return Err(MigrationPlanError::StepSkipsVersion {
                    index,
                    from_version: step.from_version,
                    expected_to,
                    actual_to: step.to_version,
                });
            }

            expected_from = step.to_version;
        }

        if expected_from != target_version {
            return Err(MigrationPlanError::TargetNotReached {
                target_version,
                reached_version: expected_from,
            });
        }

        Ok(Self {
            from_version,
            target_version,
            steps,
        })
    }

    pub const fn from_version(&self) -> CatalogSchemaVersion {
        self.from_version
    }

    pub const fn target_version(&self) -> CatalogSchemaVersion {
        self.target_version
    }

    pub fn steps(&self) -> &[MigrationStep] {
        &self.steps
    }

    pub fn into_steps(self) -> Vec<MigrationStep> {
        self.steps
    }

    pub fn is_noop(&self) -> bool {
        self.steps.is_empty()
    }
}

#[derive(Deserialize)]
struct UncheckedMigrationPlan {
    from_version: CatalogSchemaVersion,
    target_version: CatalogSchemaVersion,
    #[serde(default)]
    steps: Vec<MigrationStep>,
}

impl<'de> Deserialize<'de> for MigrationPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let unchecked = UncheckedMigrationPlan::deserialize(deserializer)?;
        Self::new(
            unchecked.from_version,
            unchecked.target_version,
            unchecked.steps,
        )
        .map_err(de::Error::custom)
    }
}

/// Why a sequence of migration steps is not a safe contiguous forward plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MigrationPlanError {
    /// The requested plan is a downgrade.
    TargetBeforeStart {
        from_version: CatalogSchemaVersion,
        target_version: CatalogSchemaVersion,
    },
    /// A step stays on the same version or moves backwards.
    StepNotForward {
        index: usize,
        from_version: CatalogSchemaVersion,
        to_version: CatalogSchemaVersion,
    },
    /// A step does not begin where the prior step ended.
    StepNotContiguous {
        index: usize,
        expected_from: CatalogSchemaVersion,
        actual_from: CatalogSchemaVersion,
    },
    /// A single step attempts to skip one or more schema versions.
    StepSkipsVersion {
        index: usize,
        from_version: CatalogSchemaVersion,
        expected_to: CatalogSchemaVersion,
        actual_to: CatalogSchemaVersion,
    },
    /// Advancing from a step's version would overflow the version representation.
    VersionOverflow {
        index: usize,
        from_version: CatalogSchemaVersion,
    },
    /// A destructive operation was submitted to the normal migration planner.
    DestructiveStep {
        index: usize,
        from_version: CatalogSchemaVersion,
        to_version: CatalogSchemaVersion,
    },
    /// The supplied steps end before or after the requested target.
    TargetNotReached {
        target_version: CatalogSchemaVersion,
        reached_version: CatalogSchemaVersion,
    },
}

impl fmt::Display for MigrationPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TargetBeforeStart {
                from_version,
                target_version,
            } => write!(
                formatter,
                "catalog schema downgrade from {from_version} to {target_version} is not supported"
            ),
            Self::StepNotForward {
                index,
                from_version,
                to_version,
            } => write!(
                formatter,
                "migration step {index} must move forward, got {from_version} to {to_version}"
            ),
            Self::StepNotContiguous {
                index,
                expected_from,
                actual_from,
            } => write!(
                formatter,
                "migration step {index} must start at {expected_from}, got {actual_from}"
            ),
            Self::StepSkipsVersion {
                index,
                from_version,
                expected_to,
                actual_to,
            } => write!(
                formatter,
                "migration step {index} from {from_version} must end at {expected_to}, got {actual_to}"
            ),
            Self::VersionOverflow {
                index,
                from_version,
            } => write!(
                formatter,
                "migration step {index} cannot advance beyond schema version {from_version}"
            ),
            Self::DestructiveStep {
                index,
                from_version,
                to_version,
            } => write!(
                formatter,
                "migration step {index} from {from_version} to {to_version} is destructive"
            ),
            Self::TargetNotReached {
                target_version,
                reached_version,
            } => write!(
                formatter,
                "migration steps reached schema version {reached_version}, expected {target_version}"
            ),
        }
    }
}

impl Error for MigrationPlanError {}

/// Lifecycle of a catalog migration attempt or one of its steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    /// No migration was needed because the catalog was already at the target.
    NotRequired,
    /// Validated and waiting to begin.
    Pending,
    /// Currently being applied.
    Running,
    /// Applied completely and verified by the persistence implementation.
    Succeeded,
    /// Stopped after an error.
    Failed,
    /// Began but did not record a terminal result, such as after process shutdown.
    Interrupted,
}

impl MigrationStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::NotRequired | Self::Succeeded | Self::Failed | Self::Interrupted
        )
    }

    pub const fn is_successful(self) -> bool {
        matches!(self, Self::NotRequired | Self::Succeeded)
    }
}

/// Persistable outcome for one step in a migration attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationStepReport {
    pub from_version: CatalogSchemaVersion,
    pub to_version: CatalogSchemaVersion,
    pub description: String,
    pub status: MigrationStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Non-secret diagnostic detail. Implementations should not place credentials,
    /// tokens, or provider payloads here.
    pub message: Option<String>,
}

impl MigrationStepReport {
    fn pending(step: &MigrationStep) -> Self {
        Self {
            from_version: step.from_version,
            to_version: step.to_version,
            description: step.description.clone(),
            status: MigrationStatus::Pending,
            started_at: None,
            finished_at: None,
            message: None,
        }
    }
}

/// Persistable report for one validated catalog migration attempt.
///
/// This is an execution-neutral record. A persistence crate updates it while applying
/// a plan and is responsible for transaction boundaries, backups, and recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    pub id: Uuid,
    pub from_version: CatalogSchemaVersion,
    pub target_version: CatalogSchemaVersion,
    /// Last schema version durably reached by the attempt.
    pub reached_version: CatalogSchemaVersion,
    pub status: MigrationStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub steps: Vec<MigrationStepReport>,
    /// Non-secret terminal diagnostic detail, if the attempt failed or was
    /// interrupted.
    pub message: Option<String>,
}

impl MigrationReport {
    /// Creates the initial report for a validated plan at a caller-supplied time.
    pub fn for_plan(plan: &MigrationPlan, created_at: DateTime<Utc>) -> Self {
        let no_migration_needed = plan.is_noop();

        Self {
            id: Uuid::new_v4(),
            from_version: plan.from_version(),
            target_version: plan.target_version(),
            reached_version: plan.from_version(),
            status: if no_migration_needed {
                MigrationStatus::NotRequired
            } else {
                MigrationStatus::Pending
            },
            created_at,
            started_at: None,
            finished_at: no_migration_needed.then_some(created_at),
            steps: plan
                .steps()
                .iter()
                .map(MigrationStepReport::pending)
                .collect(),
            message: None,
        }
    }
}

/// Logical catalog structures required by the provider-independent persistence
/// contract.
///
/// Variants describe durable responsibilities, not physical SQL table names. A
/// backend may split or combine physical structures as long as it preserves the
/// identities, ordering, provenance, relationships, and recovery data represented
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredCatalogTable {
    /// Canonical tracks with stable application UUIDs.
    Tracks,
    /// Field-level canonical metadata values and their provenance.
    TrackMetadataValues,
    /// The user's own rating, comment, and play history per canonical track.
    LibraryEntries,
    /// Cached audio analysis per canonical track.
    TrackAnalysis,
    /// Canonical user playlists with stable UUIDs.
    CanonicalPlaylists,
    /// Ordered canonical playlist-to-track memberships, with position stored apart
    /// from track identity.
    CanonicalPlaylistMemberships,
    /// Configured provider-neutral catalog sources.
    CatalogSources,
    /// Source-scoped account identities and non-secret profile metadata.
    SourceAccounts,
    /// Durable account-collection membership without duplicating source entities.
    SourceCollectionRelationships,
    /// Source-scoped track occurrences keyed by source and external identifier.
    SourceTrackOccurrences,
    /// Source-owned or source-observed external playlists.
    ExternalPlaylists,
    /// Ordered external playlist memberships and their canonical track links.
    ExternalPlaylistMemberships,
    /// Run- and provider-snapshot-scoped entries awaiting atomic playlist replacement.
    ExternalPlaylistEntryStaging,
    /// Configured primary, candidate, and backup music storages.
    MusicStorages,
    /// Profile-level active primary assignment, distinct from storage configuration.
    ProfileStorageSettings,
    /// Common identity and track ownership for every original or derived asset.
    Assets,
    /// Owned original audio asset details.
    OriginalAssets,
    /// Reproducible derived asset details, kept distinct from originals.
    DerivedAssets,
    /// Attempted storage inventories and their activation lifecycle.
    InventoryGenerations,
    /// Provider-native storage inventory objects, including generation staging.
    StorageObjects,
    /// Durable removal observations for provider-native storage objects.
    StorageObjectTombstones,
    /// Verified or candidate relationships between assets and storage objects.
    AssetLocations,
    /// Reconciliation candidates and the evidence requiring verification or review.
    MatchCandidates,
    /// Persisted reconciliation findings, statuses, and available follow-up actions.
    ReconciliationResults,
    /// Durable top-level synchronization attempts and checkpoints.
    SyncRuns,
    /// Durable units of work belonging to synchronization runs.
    SyncJobs,
    /// Structured synchronization warnings and failures.
    SyncIssues,
    /// Non-secret application and persistence settings.
    Settings,
}

impl RequiredCatalogTable {
    /// Stable logical key for diagnostics and capability declarations.
    /// It does not require a backend to use the key as a physical table name.
    pub const fn logical_name(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::TrackMetadataValues => "track_metadata_values",
            Self::LibraryEntries => "library_entries",
            Self::TrackAnalysis => "track_analysis",
            Self::CanonicalPlaylists => "canonical_playlists",
            Self::CanonicalPlaylistMemberships => "canonical_playlist_memberships",
            Self::CatalogSources => "catalog_sources",
            Self::SourceAccounts => "source_accounts",
            Self::SourceCollectionRelationships => "source_collection_relationships",
            Self::SourceTrackOccurrences => "source_track_occurrences",
            Self::ExternalPlaylists => "external_playlists",
            Self::ExternalPlaylistMemberships => "external_playlist_memberships",
            Self::ExternalPlaylistEntryStaging => "external_playlist_entry_staging",
            Self::MusicStorages => "music_storages",
            Self::ProfileStorageSettings => "profile_storage_settings",
            Self::Assets => "assets",
            Self::OriginalAssets => "original_assets",
            Self::DerivedAssets => "derived_assets",
            Self::InventoryGenerations => "inventory_generations",
            Self::StorageObjects => "storage_objects",
            Self::StorageObjectTombstones => "storage_object_tombstones",
            Self::AssetLocations => "asset_locations",
            Self::MatchCandidates => "match_candidates",
            Self::ReconciliationResults => "reconciliation_results",
            Self::SyncRuns => "sync_runs",
            Self::SyncJobs => "sync_jobs",
            Self::SyncIssues => "sync_issues",
            Self::Settings => "settings",
        }
    }
}

impl fmt::Display for RequiredCatalogTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.logical_name())
    }
}

/// Complete Stage 1 logical catalog requirements in dependency-friendly order.
pub const REQUIRED_CATALOG_TABLES: &[RequiredCatalogTable] = &[
    RequiredCatalogTable::Tracks,
    RequiredCatalogTable::TrackMetadataValues,
    RequiredCatalogTable::LibraryEntries,
    RequiredCatalogTable::TrackAnalysis,
    RequiredCatalogTable::CanonicalPlaylists,
    RequiredCatalogTable::CanonicalPlaylistMemberships,
    RequiredCatalogTable::CatalogSources,
    RequiredCatalogTable::SourceAccounts,
    RequiredCatalogTable::SourceTrackOccurrences,
    RequiredCatalogTable::ExternalPlaylists,
    RequiredCatalogTable::ExternalPlaylistMemberships,
    RequiredCatalogTable::SourceCollectionRelationships,
    RequiredCatalogTable::ExternalPlaylistEntryStaging,
    RequiredCatalogTable::MusicStorages,
    RequiredCatalogTable::ProfileStorageSettings,
    RequiredCatalogTable::Assets,
    RequiredCatalogTable::OriginalAssets,
    RequiredCatalogTable::DerivedAssets,
    RequiredCatalogTable::InventoryGenerations,
    RequiredCatalogTable::StorageObjects,
    RequiredCatalogTable::StorageObjectTombstones,
    RequiredCatalogTable::AssetLocations,
    RequiredCatalogTable::MatchCandidates,
    RequiredCatalogTable::ReconciliationResults,
    RequiredCatalogTable::SyncRuns,
    RequiredCatalogTable::SyncJobs,
    RequiredCatalogTable::SyncIssues,
    RequiredCatalogTable::Settings,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: u32) -> CatalogSchemaVersion {
        CatalogSchemaVersion::new(value).unwrap()
    }

    fn step(from: u32, to: u32) -> MigrationStep {
        MigrationStep::non_destructive(version(from), version(to), format!("v{from} to v{to}"))
    }

    #[test]
    fn accepts_a_contiguous_forward_non_destructive_plan() {
        let plan = MigrationPlan::new(
            version(1),
            version(4),
            vec![step(1, 2), step(2, 3), step(3, 4)],
        )
        .unwrap();

        assert_eq!(plan.from_version(), version(1));
        assert_eq!(plan.target_version(), version(4));
        assert_eq!(plan.steps().len(), 3);
        assert!(!plan.is_noop());
    }

    #[test]
    fn rejects_a_gap_between_steps() {
        let error =
            MigrationPlan::new(version(1), version(4), vec![step(1, 2), step(3, 4)]).unwrap_err();

        assert_eq!(
            error,
            MigrationPlanError::StepNotContiguous {
                index: 1,
                expected_from: version(2),
                actual_from: version(3),
            }
        );
    }

    #[test]
    fn rejects_a_step_that_skips_a_version() {
        let error = MigrationPlan::new(version(1), version(3), vec![step(1, 3)]).unwrap_err();

        assert_eq!(
            error,
            MigrationPlanError::StepSkipsVersion {
                index: 0,
                from_version: version(1),
                expected_to: version(2),
                actual_to: version(3),
            }
        );
    }

    #[test]
    fn rejects_backwards_plans_and_steps() {
        let plan_error = MigrationPlan::new(version(2), version(1), Vec::new()).unwrap_err();
        assert!(matches!(
            plan_error,
            MigrationPlanError::TargetBeforeStart { .. }
        ));

        let step_error = MigrationPlan::new(version(2), version(1), vec![step(2, 1)]).unwrap_err();
        assert!(matches!(
            step_error,
            MigrationPlanError::TargetBeforeStart { .. }
        ));

        let step_error = MigrationPlan::new(version(2), version(3), vec![step(2, 1)]).unwrap_err();
        assert!(matches!(
            step_error,
            MigrationPlanError::StepNotForward { index: 0, .. }
        ));
    }

    #[test]
    fn rejects_destructive_steps() {
        let destructive =
            MigrationStep::destructive(version(1), version(2), "discard legacy catalog records");
        let error = MigrationPlan::new(version(1), version(2), vec![destructive]).unwrap_err();

        assert!(matches!(
            error,
            MigrationPlanError::DestructiveStep { index: 0, .. }
        ));
    }

    #[test]
    fn version_deserialization_preserves_validation() {
        assert_eq!(
            serde_json::from_str::<CatalogSchemaVersion>("2").unwrap(),
            version(2)
        );
        assert!(serde_json::from_str::<CatalogSchemaVersion>("0").is_err());
    }

    #[test]
    fn stage_one_required_tables_are_unique_and_include_atomic_sync_state() {
        use std::collections::HashSet;

        let unique: HashSet<_> = REQUIRED_CATALOG_TABLES.iter().copied().collect();
        assert_eq!(unique.len(), REQUIRED_CATALOG_TABLES.len());

        for required in [
            RequiredCatalogTable::Assets,
            RequiredCatalogTable::SourceCollectionRelationships,
            RequiredCatalogTable::ExternalPlaylistEntryStaging,
            RequiredCatalogTable::ReconciliationResults,
            RequiredCatalogTable::InventoryGenerations,
            RequiredCatalogTable::StorageObjectTombstones,
            RequiredCatalogTable::ProfileStorageSettings,
            RequiredCatalogTable::SyncJobs,
        ] {
            assert!(unique.contains(&required), "missing {required}");
        }
    }
}
