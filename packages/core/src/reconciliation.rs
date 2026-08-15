//! Provider-independent reconciliation contracts.
//!
//! Reconciliation compares canonical catalog requirements with original assets and
//! storage inventory. Metadata can propose a match, but it can never verify one
//! without stronger identity evidence or explicit user confirmation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    error::Error,
    fmt,
};
use uuid::Uuid;

/// The mechanism that produced a piece of match evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMethod {
    /// A previously persisted asset-to-storage-object association still matches.
    PersistedAssociation,
    /// The canonical SHA-256 checksum is identical.
    ExactContentHash,
    /// A stable rec_bag asset identifier was found in provider metadata.
    StableAssetIdentifier,
    /// The object is the recorded result of a rec_bag copy or upload operation.
    RecordedTransfer,
    /// A stable standard identifier was independently verified.
    VerifiedStandardIdentifier,
    /// Artist, title, filename, duration, or similar descriptive metadata matched.
    MetadataOnly,
}

impl MatchMethod {
    /// Whether this method is sufficient to automatically verify a match.
    pub const fn is_strong(self) -> bool {
        !matches!(self, Self::MetadataOnly)
    }
}

/// Confidence assigned to a complete match candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchConfidence {
    /// Strong identity evidence permits automatic verification.
    Verified,
    /// Evidence suggests a match, but a user must confirm it.
    ReviewRequired,
    /// Evidence points to more than one canonical identity.
    Ambiguous,
    /// No useful evidence is available.
    Insufficient,
}

impl MatchConfidence {
    /// Whether this confidence permits automatic association.
    pub const fn is_automatically_verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// One fact supporting a candidate association.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchEvidence {
    pub method: MatchMethod,
    /// An optional non-secret explanation, checksum, or provider identifier.
    pub value: Option<String>,
}

impl MatchEvidence {
    pub fn new(method: MatchMethod, value: Option<String>) -> Self {
        Self { method, value }
    }

    pub fn metadata(value: impl Into<String>) -> Self {
        Self::new(MatchMethod::MetadataOnly, Some(value.into()))
    }

    pub const fn is_strong(&self) -> bool {
        self.method.is_strong()
    }
}

/// A possible association between a canonical track or asset and a storage object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchCandidate {
    pub id: Uuid,
    pub track_id: Uuid,
    pub asset_id: Option<Uuid>,
    pub storage_id: Uuid,
    pub storage_object_id: String,
    pub evidence: Vec<MatchEvidence>,
    /// Other canonical tracks supported by the same evidence.
    pub conflicting_track_ids: Vec<Uuid>,
}

impl MatchCandidate {
    pub fn new(
        track_id: Uuid,
        asset_id: Option<Uuid>,
        storage_id: Uuid,
        storage_object_id: impl Into<String>,
        evidence: Vec<MatchEvidence>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            track_id,
            asset_id,
            storage_id,
            storage_object_id: storage_object_id.into(),
            evidence,
            conflicting_track_ids: Vec::new(),
        }
    }

    pub fn with_conflicts(mut self, conflicting_track_ids: Vec<Uuid>) -> Self {
        let mut seen = HashSet::new();
        self.conflicting_track_ids = conflicting_track_ids
            .into_iter()
            .filter(|id| *id != self.track_id && seen.insert(*id))
            .collect();
        self
    }

    /// Derive confidence from evidence instead of accepting a caller-supplied value.
    /// This prevents metadata-only candidates from being marked verified.
    pub fn confidence(&self) -> MatchConfidence {
        if !self.conflicting_track_ids.is_empty() {
            MatchConfidence::Ambiguous
        } else if self.evidence.iter().any(MatchEvidence::is_strong) {
            MatchConfidence::Verified
        } else if self
            .evidence
            .iter()
            .any(|evidence| evidence.method == MatchMethod::MetadataOnly)
        {
            MatchConfidence::ReviewRequired
        } else {
            MatchConfidence::Insufficient
        }
    }

    pub fn requires_confirmation(&self) -> bool {
        !self.confidence().is_automatically_verified()
    }
}

/// Catalog-level storage state for one unique canonical track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackStorageStatus {
    StoredAndVerified,
    MissingObtainable,
    PossibleMatch,
    AmbiguousMatch,
    NotAvailableForArchival,
    PreviewOnly,
    Blocked,
    Removed,
    Inaccessible,
    StorageUnavailable,
    Error,
    Ignored,
}

impl TrackStorageStatus {
    /// Whether the track belongs in the archivable-completeness denominator.
    pub const fn is_archivable(self) -> bool {
        matches!(
            self,
            Self::StoredAndVerified
                | Self::MissingObtainable
                | Self::PossibleMatch
                | Self::AmbiguousMatch
        )
    }

    pub const fn is_archived(self) -> bool {
        matches!(self, Self::StoredAndVerified)
    }

    /// Whether storage or archival availability has been determined.
    pub const fn is_catalog_covered(self) -> bool {
        !matches!(self, Self::StorageUnavailable | Self::Error)
    }
}

/// Verification state for one known original asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetStorageStatus {
    Verified,
    Missing,
    PossibleMatch,
    AmbiguousMatch,
    ChangedPendingReview,
    Unavailable,
    Error,
    Ignored,
}

impl AssetStorageStatus {
    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// Reconciliation outcome stored for a canonical track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackStorageOutcome {
    pub track_id: Uuid,
    /// Storage evaluated by this outcome. `None` represents an aggregate view.
    pub storage_id: Option<Uuid>,
    pub status: TrackStorageStatus,
    /// Every verified original asset associated with this track.
    pub verified_asset_ids: Vec<Uuid>,
}

impl TrackStorageOutcome {
    pub fn new(track_id: Uuid, status: TrackStorageStatus) -> Self {
        Self {
            track_id,
            storage_id: None,
            status,
            verified_asset_ids: Vec::new(),
        }
    }

    pub fn for_storage(mut self, storage_id: Uuid) -> Self {
        self.storage_id = Some(storage_id);
        self
    }

    pub fn with_verified_assets(mut self, asset_ids: Vec<Uuid>) -> Self {
        let mut seen = HashSet::new();
        self.verified_asset_ids = asset_ids
            .into_iter()
            .filter(|asset_id| seen.insert(*asset_id))
            .collect();
        self
    }
}

/// Reconciliation outcome stored for one original asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetStorageOutcome {
    pub asset_id: Uuid,
    pub track_id: Uuid,
    /// Storage evaluated by this outcome. `None` represents an aggregate view.
    pub storage_id: Option<Uuid>,
    pub status: AssetStorageStatus,
    pub verified_location_ids: Vec<Uuid>,
}

impl AssetStorageOutcome {
    pub fn new(asset_id: Uuid, track_id: Uuid, status: AssetStorageStatus) -> Self {
        Self {
            asset_id,
            track_id,
            storage_id: None,
            status,
            verified_location_ids: Vec::new(),
        }
    }

    pub fn for_storage(mut self, storage_id: Uuid) -> Self {
        self.storage_id = Some(storage_id);
        self
    }

    pub fn with_verified_locations(mut self, location_ids: Vec<Uuid>) -> Self {
        let mut seen = HashSet::new();
        self.verified_location_ids = location_ids
            .into_iter()
            .filter(|location_id| seen.insert(*location_id))
            .collect();
        self
    }
}

/// The explicitly selected outcome scope used to construct a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "storage_id")]
pub enum ReconciliationSummaryScope {
    Aggregate,
    Storage(Uuid),
}

impl ReconciliationSummaryScope {
    fn includes(self, storage_id: Option<Uuid>) -> bool {
        match self {
            Self::Aggregate => storage_id.is_none(),
            Self::Storage(expected) => matches!(storage_id, Some(actual) if actual == expected),
        }
    }

    const fn storage_id(self) -> Option<Uuid> {
        match self {
            Self::Aggregate => None,
            Self::Storage(storage_id) => Some(storage_id),
        }
    }
}

/// One distinct outcome found for a duplicated asset ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConflictingAssetSummaryOutcome {
    pub track_id: Uuid,
    pub status: AssetStorageStatus,
}

/// A deterministic validation error encountered while constructing a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReconciliationSummaryError {
    ConflictingTrackOutcomes {
        scope: ReconciliationSummaryScope,
        track_id: Uuid,
        statuses: Vec<TrackStorageStatus>,
    },
    ConflictingAssetOutcomes {
        scope: ReconciliationSummaryScope,
        asset_id: Uuid,
        outcomes: Vec<ConflictingAssetSummaryOutcome>,
    },
}

impl fmt::Display for ReconciliationSummaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConflictingTrackOutcomes {
                scope,
                track_id,
                statuses,
            } => write!(
                formatter,
                "conflicting track outcomes for {track_id} in {scope:?}: {statuses:?}"
            ),
            Self::ConflictingAssetOutcomes {
                scope,
                asset_id,
                outcomes,
            } => write!(
                formatter,
                "conflicting asset outcomes for {asset_id} in {scope:?}: {outcomes:?}"
            ),
        }
    }
}

impl Error for ReconciliationSummaryError {}

/// Aggregate completeness and coverage counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationSummary {
    pub total_tracks: u64,
    pub archivable_tracks: u64,
    pub archived_tracks: u64,
    pub covered_tracks: u64,
    pub total_assets: u64,
    pub verified_assets: u64,
}

impl ReconciliationSummary {
    /// Summarize only aggregate outcomes (`storage_id == None`).
    pub fn aggregate(
        track_outcomes: &[TrackStorageOutcome],
        asset_outcomes: &[AssetStorageOutcome],
    ) -> Result<Self, ReconciliationSummaryError> {
        Self::from_scoped_outcomes(
            ReconciliationSummaryScope::Aggregate,
            track_outcomes,
            asset_outcomes,
        )
    }

    /// Summarize only outcomes for the selected storage.
    pub fn for_storage(
        storage_id: Uuid,
        track_outcomes: &[TrackStorageOutcome],
        asset_outcomes: &[AssetStorageOutcome],
    ) -> Result<Self, ReconciliationSummaryError> {
        Self::from_scoped_outcomes(
            ReconciliationSummaryScope::Storage(storage_id),
            track_outcomes,
            asset_outcomes,
        )
    }

    fn from_scoped_outcomes(
        scope: ReconciliationSummaryScope,
        track_outcomes: &[TrackStorageOutcome],
        asset_outcomes: &[AssetStorageOutcome],
    ) -> Result<Self, ReconciliationSummaryError> {
        let mut track_statuses = BTreeMap::<Uuid, BTreeSet<TrackStorageStatus>>::new();
        for outcome in track_outcomes
            .iter()
            .filter(|outcome| scope.includes(outcome.storage_id))
        {
            track_statuses
                .entry(outcome.track_id)
                .or_default()
                .insert(outcome.status);
        }

        let mut tracks = BTreeMap::new();
        for (track_id, statuses) in track_statuses {
            if statuses.len() != 1 {
                return Err(ReconciliationSummaryError::ConflictingTrackOutcomes {
                    scope,
                    track_id,
                    statuses: statuses.into_iter().collect(),
                });
            }
            tracks.insert(
                track_id,
                *statuses
                    .first()
                    .expect("a grouped track always has at least one status"),
            );
        }

        let mut asset_statuses = BTreeMap::<Uuid, BTreeSet<ConflictingAssetSummaryOutcome>>::new();
        for outcome in asset_outcomes
            .iter()
            .filter(|outcome| scope.includes(outcome.storage_id))
        {
            asset_statuses.entry(outcome.asset_id).or_default().insert(
                ConflictingAssetSummaryOutcome {
                    track_id: outcome.track_id,
                    status: outcome.status,
                },
            );
        }

        let mut assets = BTreeMap::new();
        for (asset_id, outcomes) in asset_statuses {
            if outcomes.len() != 1 {
                return Err(ReconciliationSummaryError::ConflictingAssetOutcomes {
                    scope,
                    asset_id,
                    outcomes: outcomes.into_iter().collect(),
                });
            }
            assets.insert(
                asset_id,
                outcomes
                    .first()
                    .expect("a grouped asset always has at least one outcome")
                    .status,
            );
        }

        Ok(Self {
            total_tracks: tracks.len() as u64,
            archivable_tracks: tracks
                .values()
                .filter(|status| status.is_archivable())
                .count() as u64,
            archived_tracks: tracks
                .values()
                .filter(|status| status.is_archived())
                .count() as u64,
            covered_tracks: tracks
                .values()
                .filter(|status| status.is_catalog_covered())
                .count() as u64,
            total_assets: assets.len() as u64,
            verified_assets: assets
                .values()
                .filter(|status| status.is_verified())
                .count() as u64,
        })
    }

    /// Verified archivable tracks divided by all archivable tracks.
    pub fn archivable_completeness(&self) -> Option<f64> {
        ratio(self.archived_tracks, self.archivable_tracks)
    }

    /// Tracks with a determined status divided by all catalog tracks.
    pub fn catalog_coverage(&self) -> Option<f64> {
        ratio(self.covered_tracks, self.total_tracks)
    }

    /// Verified original assets divided by all known original assets.
    pub fn asset_completeness(&self) -> Option<f64> {
        ratio(self.verified_assets, self.total_assets)
    }
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator != 0).then_some(numerator as f64 / denominator as f64)
}

/// Per-playlist outcome for either aggregate data or one explicitly selected storage.
/// Repeated playlist entries count once in the summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistStorageOutcome {
    pub playlist_id: Uuid,
    /// `None` represents aggregate outcomes; `Some` identifies the selected storage.
    pub storage_id: Option<Uuid>,
    pub unique_track_ids: Vec<Uuid>,
    pub summary: ReconciliationSummary,
}

impl PlaylistStorageOutcome {
    /// Build a playlist outcome from only aggregate outcomes (`storage_id == None`).
    pub fn aggregate(
        playlist_id: Uuid,
        track_ids: impl IntoIterator<Item = Uuid>,
        track_outcomes: &[TrackStorageOutcome],
        asset_outcomes: &[AssetStorageOutcome],
    ) -> Result<Self, ReconciliationSummaryError> {
        Self::from_track_ids_in_scope(
            playlist_id,
            ReconciliationSummaryScope::Aggregate,
            track_ids,
            track_outcomes,
            asset_outcomes,
        )
    }

    /// Build a playlist outcome from only outcomes for the selected storage.
    pub fn for_storage(
        playlist_id: Uuid,
        storage_id: Uuid,
        track_ids: impl IntoIterator<Item = Uuid>,
        track_outcomes: &[TrackStorageOutcome],
        asset_outcomes: &[AssetStorageOutcome],
    ) -> Result<Self, ReconciliationSummaryError> {
        Self::from_track_ids_in_scope(
            playlist_id,
            ReconciliationSummaryScope::Storage(storage_id),
            track_ids,
            track_outcomes,
            asset_outcomes,
        )
    }

    fn from_track_ids_in_scope(
        playlist_id: Uuid,
        scope: ReconciliationSummaryScope,
        track_ids: impl IntoIterator<Item = Uuid>,
        track_outcomes: &[TrackStorageOutcome],
        asset_outcomes: &[AssetStorageOutcome],
    ) -> Result<Self, ReconciliationSummaryError> {
        let mut seen = HashSet::new();
        let unique_track_ids: Vec<_> = track_ids
            .into_iter()
            .filter(|track_id| seen.insert(*track_id))
            .collect();
        let included: HashSet<_> = unique_track_ids.iter().copied().collect();
        let playlist_tracks: Vec<_> = track_outcomes
            .iter()
            .filter(|outcome| included.contains(&outcome.track_id))
            .cloned()
            .collect();
        let playlist_assets: Vec<_> = asset_outcomes
            .iter()
            .filter(|outcome| included.contains(&outcome.track_id))
            .cloned()
            .collect();
        let summary = match scope {
            ReconciliationSummaryScope::Aggregate => {
                ReconciliationSummary::aggregate(&playlist_tracks, &playlist_assets)
            }
            ReconciliationSummaryScope::Storage(storage_id) => {
                ReconciliationSummary::for_storage(storage_id, &playlist_tracks, &playlist_assets)
            }
        }?;

        Ok(Self {
            playlist_id,
            storage_id: scope.storage_id(),
            unique_track_ids,
            summary,
        })
    }
}

/// The subject and outcome represented by a reconciliation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "status")]
pub enum ReconciliationOutcome {
    Track(TrackStorageStatus),
    Asset(AssetStorageStatus),
    UntrackedStorageObject,
    ChangedStorageObject,
}

/// A user-visible or executable follow-up produced by reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReconciliationAction {
    /// Copy an asset already managed by one storage to another, such as a backup.
    Copy {
        track_id: Uuid,
        asset_id: Uuid,
        source_storage_id: Uuid,
        destination_storage_id: Uuid,
    },
    /// Copy an obtainable file directly from a catalog source into primary storage.
    CopyFromSource {
        track_id: Uuid,
        asset_id: Uuid,
        source_id: Uuid,
        destination_storage_id: Uuid,
    },
    Associate {
        track_id: Uuid,
        asset_id: Option<Uuid>,
        storage_id: Uuid,
        storage_object_id: String,
    },
    Locate {
        track_id: Uuid,
    },
    Confirm {
        candidate_id: Uuid,
    },
    Reject {
        candidate_id: Uuid,
    },
    /// Accept the newly observed bytes/version for an associated external object.
    ApproveExternalChange {
        result_id: Uuid,
        storage_id: Uuid,
        storage_object_id: String,
    },
    /// Create a new original asset and associate an otherwise unknown object with it.
    ImportUnknownObject {
        result_id: Uuid,
        storage_id: Uuid,
        storage_object_id: String,
    },
    /// Associate an unknown object with an existing canonical track or asset.
    AssociateUnknownObject {
        result_id: Uuid,
        track_id: Uuid,
        asset_id: Option<Uuid>,
        storage_id: Uuid,
        storage_object_id: String,
    },
    Ignore {
        result_id: Uuid,
        reason: Option<String>,
    },
    Retry {
        result_id: Uuid,
    },
}

/// Durable user or worker decision for a reconciliation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationDecision {
    ConfirmCandidate,
    RejectCandidate,
    ApproveExternalChange,
    ImportUnknownObject,
    AssociateUnknownObject,
    IgnoreFinding,
    RetryFinding,
}

impl ReconciliationDecision {
    /// Decisions that intentionally prevent an identical finding from being offered again.
    pub const fn creates_suppression(self) -> bool {
        matches!(self, Self::RejectCandidate | Self::IgnoreFinding)
    }
}

/// A durable record of a decision made against a stable result and optional candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationResolution {
    pub id: Uuid,
    pub result_id: Uuid,
    #[serde(default)]
    pub candidate_id: Option<Uuid>,
    pub decision: ReconciliationDecision,
    #[serde(default)]
    pub reason: Option<String>,
    pub decided_at: DateTime<Utc>,
}

impl ReconciliationResolution {
    pub fn new(result_id: Uuid, decision: ReconciliationDecision) -> Self {
        Self {
            id: Uuid::new_v4(),
            result_id,
            candidate_id: None,
            decision,
            reason: None,
            decided_at: Utc::now(),
        }
    }

    pub fn for_candidate(mut self, candidate_id: Uuid) -> Self {
        self.candidate_id = Some(candidate_id);
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Durable suppression generated by a rejecting or ignoring resolution.
///
/// Stable result/candidate IDs make suppression deterministic across reconciliation runs.
/// `expires_at == None` means the suppression remains until explicitly removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationSuppression {
    pub id: Uuid,
    pub resolution_id: Uuid,
    pub result_id: Uuid,
    #[serde(default)]
    pub candidate_id: Option<Uuid>,
    pub decision: ReconciliationDecision,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl ReconciliationSuppression {
    pub fn from_resolution(resolution: &ReconciliationResolution) -> Option<Self> {
        resolution.decision.creates_suppression().then(|| Self {
            id: Uuid::new_v4(),
            resolution_id: resolution.id,
            result_id: resolution.result_id,
            candidate_id: resolution.candidate_id,
            decision: resolution.decision,
            created_at: resolution.decided_at,
            expires_at: None,
        })
    }

    pub fn is_active_at(&self, at: DateTime<Utc>) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > at)
    }
}

/// A persisted reconciliation finding and its available next steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationResult {
    pub id: Uuid,
    pub track_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub storage_id: Option<Uuid>,
    pub storage_object_id: Option<String>,
    pub outcome: ReconciliationOutcome,
    pub candidates: Vec<MatchCandidate>,
    pub actions: Vec<ReconciliationAction>,
}

impl ReconciliationResult {
    pub fn new(outcome: ReconciliationOutcome) -> Self {
        Self {
            id: Uuid::new_v4(),
            track_id: None,
            asset_id: None,
            storage_id: None,
            storage_object_id: None,
            outcome,
            candidates: Vec::new(),
            actions: Vec::new(),
        }
    }

    pub fn with_track(mut self, track_id: Uuid) -> Self {
        self.track_id = Some(track_id);
        self
    }

    pub fn with_asset(mut self, asset_id: Uuid) -> Self {
        self.asset_id = Some(asset_id);
        self
    }

    pub fn with_storage_object(
        mut self,
        storage_id: Uuid,
        storage_object_id: impl Into<String>,
    ) -> Self {
        self.storage_id = Some(storage_id);
        self.storage_object_id = Some(storage_object_id.into());
        self
    }

    pub fn with_candidate(mut self, candidate: MatchCandidate) -> Self {
        self.candidates.push(candidate);
        self
    }

    pub fn with_action(mut self, action: ReconciliationAction) -> Self {
        self.actions.push(action);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_only_candidate_requires_review() {
        let candidate = MatchCandidate::new(
            Uuid::new_v4(),
            None,
            Uuid::new_v4(),
            "provider/object",
            vec![MatchEvidence::metadata("artist, title, and duration")],
        );

        assert_eq!(candidate.confidence(), MatchConfidence::ReviewRequired);
        assert!(candidate.requires_confirmation());
        assert!(!candidate.confidence().is_automatically_verified());
    }

    #[test]
    fn only_reject_and_ignore_decisions_create_suppressions() {
        let result_id = Uuid::new_v4();
        let candidate_id = Uuid::new_v4();
        let rejected =
            ReconciliationResolution::new(result_id, ReconciliationDecision::RejectCandidate)
                .for_candidate(candidate_id);
        let suppression = ReconciliationSuppression::from_resolution(&rejected).unwrap();
        assert_eq!(suppression.result_id, result_id);
        assert_eq!(suppression.candidate_id, Some(candidate_id));
        assert!(suppression.is_active_at(rejected.decided_at));

        let retry = ReconciliationResolution::new(result_id, ReconciliationDecision::RetryFinding);
        assert!(ReconciliationSuppression::from_resolution(&retry).is_none());
    }

    #[test]
    fn summary_counts_unique_tracks_and_computes_distinct_ratios() {
        let archived = Uuid::new_v4();
        let missing = Uuid::new_v4();
        let streaming_only = Uuid::new_v4();
        let unavailable = Uuid::new_v4();
        let outcomes = vec![
            TrackStorageOutcome::new(archived, TrackStorageStatus::StoredAndVerified),
            // A repeated playlist/catalog input must not inflate totals.
            TrackStorageOutcome::new(archived, TrackStorageStatus::StoredAndVerified),
            TrackStorageOutcome::new(missing, TrackStorageStatus::MissingObtainable),
            TrackStorageOutcome::new(streaming_only, TrackStorageStatus::NotAvailableForArchival),
            TrackStorageOutcome::new(unavailable, TrackStorageStatus::StorageUnavailable),
        ];

        let summary = ReconciliationSummary::aggregate(&outcomes, &[]).unwrap();

        assert_eq!(summary.total_tracks, 4);
        assert_eq!(summary.archivable_tracks, 2);
        assert_eq!(summary.archived_tracks, 1);
        assert_eq!(summary.covered_tracks, 3);
        assert_eq!(summary.archivable_completeness(), Some(0.5));
        assert_eq!(summary.catalog_coverage(), Some(0.75));
    }

    #[test]
    fn playlist_entries_are_counted_by_unique_canonical_track() {
        let playlist_id = Uuid::new_v4();
        let track_id = Uuid::new_v4();
        let outcomes = vec![TrackStorageOutcome::new(
            track_id,
            TrackStorageStatus::StoredAndVerified,
        )];

        let playlist = PlaylistStorageOutcome::aggregate(
            playlist_id,
            [track_id, track_id, track_id],
            &outcomes,
            &[],
        )
        .unwrap();

        assert_eq!(playlist.storage_id, None);
        assert_eq!(playlist.unique_track_ids, vec![track_id]);
        assert_eq!(playlist.summary.total_tracks, 1);
        assert_eq!(playlist.summary.archived_tracks, 1);
    }

    #[test]
    fn conflicting_track_outcomes_are_rejected_independently_of_input_order() {
        let track_id = Uuid::new_v4();
        let first = TrackStorageOutcome::new(track_id, TrackStorageStatus::StoredAndVerified);
        let second = TrackStorageOutcome::new(track_id, TrackStorageStatus::MissingObtainable);

        let forward =
            ReconciliationSummary::aggregate(&[first.clone(), second.clone()], &[]).unwrap_err();
        let reverse = ReconciliationSummary::aggregate(&[second, first], &[]).unwrap_err();

        assert_eq!(forward, reverse);
        assert_eq!(
            forward,
            ReconciliationSummaryError::ConflictingTrackOutcomes {
                scope: ReconciliationSummaryScope::Aggregate,
                track_id,
                statuses: vec![
                    TrackStorageStatus::StoredAndVerified,
                    TrackStorageStatus::MissingObtainable,
                ],
            }
        );
    }

    #[test]
    fn conflicting_asset_outcomes_are_rejected_independently_of_input_order() {
        let track_id = Uuid::new_v4();
        let asset_id = Uuid::new_v4();
        let first = AssetStorageOutcome::new(asset_id, track_id, AssetStorageStatus::Verified);
        let second = AssetStorageOutcome::new(asset_id, track_id, AssetStorageStatus::Missing);

        let forward =
            ReconciliationSummary::aggregate(&[], &[first.clone(), second.clone()]).unwrap_err();
        let reverse = ReconciliationSummary::aggregate(&[], &[second, first]).unwrap_err();

        assert_eq!(forward, reverse);
        assert_eq!(
            forward,
            ReconciliationSummaryError::ConflictingAssetOutcomes {
                scope: ReconciliationSummaryScope::Aggregate,
                asset_id,
                outcomes: vec![
                    ConflictingAssetSummaryOutcome {
                        track_id,
                        status: AssetStorageStatus::Verified,
                    },
                    ConflictingAssetSummaryOutcome {
                        track_id,
                        status: AssetStorageStatus::Missing,
                    },
                ],
            }
        );
    }

    #[test]
    fn primary_verified_and_backup_missing_have_independent_summaries() {
        let playlist_id = Uuid::new_v4();
        let track_id = Uuid::new_v4();
        let asset_id = Uuid::new_v4();
        let primary_storage_id = Uuid::new_v4();
        let backup_storage_id = Uuid::new_v4();
        let track_outcomes = vec![
            TrackStorageOutcome::new(track_id, TrackStorageStatus::StoredAndVerified)
                .for_storage(primary_storage_id),
            TrackStorageOutcome::new(track_id, TrackStorageStatus::MissingObtainable)
                .for_storage(backup_storage_id),
        ];
        let asset_outcomes = vec![
            AssetStorageOutcome::new(asset_id, track_id, AssetStorageStatus::Verified)
                .for_storage(primary_storage_id),
            AssetStorageOutcome::new(asset_id, track_id, AssetStorageStatus::Missing)
                .for_storage(backup_storage_id),
        ];

        let primary = PlaylistStorageOutcome::for_storage(
            playlist_id,
            primary_storage_id,
            [track_id],
            &track_outcomes,
            &asset_outcomes,
        )
        .unwrap();
        let backup = PlaylistStorageOutcome::for_storage(
            playlist_id,
            backup_storage_id,
            [track_id],
            &track_outcomes,
            &asset_outcomes,
        )
        .unwrap();

        assert_eq!(primary.storage_id, Some(primary_storage_id));
        assert_eq!(primary.summary.total_tracks, 1);
        assert_eq!(primary.summary.archived_tracks, 1);
        assert_eq!(primary.summary.verified_assets, 1);
        assert_eq!(backup.storage_id, Some(backup_storage_id));
        assert_eq!(backup.summary.total_tracks, 1);
        assert_eq!(backup.summary.archived_tracks, 0);
        assert_eq!(backup.summary.verified_assets, 0);
        assert_eq!(
            ReconciliationSummary::aggregate(&track_outcomes, &asset_outcomes).unwrap(),
            ReconciliationSummary::default()
        );
    }
}
