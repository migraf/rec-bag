//! Deterministic reconciliation of catalog requirements and storage inventory.
//!
//! This crate deliberately performs no I/O and does not persist its output. Callers
//! supply a consistent catalog and inventory snapshot, then persist or schedule the
//! returned candidates, findings, and reports as appropriate.
//!
//! Backup jobs, staged replication, retention review, and verification scheduling
//! are provided by the supporting modules below.

pub mod jobs;
pub mod replication;
pub mod retention;
pub mod verification;

pub use jobs::{
    sync_error_from_storage, CancellationRequest, DurableJobPlan, JobCompletion, JobEngine,
    JobEvent, JobEventKind, JobExecutor, JobPlanner, JobStep, JobStore, JobStoreError,
};
pub use replication::{
    BackupAvailability, BackupTarget, ReplicationDecision, ReplicationError, ReplicationExecution,
    ReplicationExecutor, ReplicationJobPlan, ReplicationPlanner, ReplicationRequest,
    ReplicationStage, ReplicationStep, ReplicationTrigger, StagedReplicationRequest,
};
pub use retention::{
    evaluate_retention, plan_cleanup, plan_primary_deletion_retention, CleanupApproval,
    CleanupCandidate, CleanupPlan, PrimaryDeletionDecision, RetentionEvaluation, RetentionStatus,
};
pub use verification::{
    VerificationCadence, VerificationPlan, VerificationPlanner, VerificationRequest,
};

use rec_core::{
    AssetLocation, AssetStorageOutcome, AssetStorageStatus, CanonicalPlaylist, CanonicalTrack,
    CatalogSource, MatchCandidate, MatchEvidence, MatchMethod, MusicStorage, OriginalAsset,
    PlaylistStorageOutcome, ReconciliationAction, ReconciliationDecision, ReconciliationOutcome,
    ReconciliationResolution, ReconciliationResult, ReconciliationSummary,
    ReconciliationSuppression, SourceAvailability, SourceCopyPolicy, SourceTrackOccurrence,
    StorageObject, StorageVerificationStatus, TrackStorageOutcome, TrackStorageStatus,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use uuid::Uuid;

/// Whether an inventory snapshot is authoritative enough to distinguish an empty
/// storage root from one that could not be queried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InventoryAvailability {
    /// The inventory completed successfully; an empty object list is meaningful.
    #[default]
    Available,
    /// The provider or configured root could not be reached.
    Unavailable,
    /// Inventory failed for a non-temporary reason.
    Error,
}

/// Inventory data for one configured storage location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageInventory {
    pub storage_id: Uuid,
    pub availability: InventoryAvailability,
    pub objects: Vec<StorageObject>,
}

/// Strong identity evidence recorded outside provider inventory metadata.
///
/// Recorded transfer results prove which provider object a successful rec_bag
/// operation created. Standard identifiers are accepted only after an adapter or
/// user workflow has independently verified that the identifier identifies this
/// exact asset version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalMatchEvidence {
    pub asset_id: Uuid,
    pub storage_id: Uuid,
    pub storage_object_id: String,
    pub method: MatchMethod,
    pub value: Option<String>,
}

impl ExternalMatchEvidence {
    pub fn recorded_transfer(
        asset_id: Uuid,
        storage_id: Uuid,
        storage_object_id: impl Into<String>,
        operation_key: impl Into<String>,
    ) -> Self {
        Self {
            asset_id,
            storage_id,
            storage_object_id: storage_object_id.into(),
            method: MatchMethod::RecordedTransfer,
            value: Some(operation_key.into()),
        }
    }

    pub fn verified_standard_identifier(
        asset_id: Uuid,
        storage_id: Uuid,
        storage_object_id: impl Into<String>,
        identifier: impl Into<String>,
    ) -> Self {
        Self {
            asset_id,
            storage_id,
            storage_object_id: storage_object_id.into(),
            method: MatchMethod::VerifiedStandardIdentifier,
            value: Some(identifier.into()),
        }
    }

    fn is_valid_strong_evidence(&self) -> bool {
        matches!(
            self.method,
            MatchMethod::RecordedTransfer | MatchMethod::VerifiedStandardIdentifier
        ) && self
            .value
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    }
}

/// Deterministic evidence selected for an automatically verified object match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedObjectMatch {
    pub asset_id: Uuid,
    pub track_id: Uuid,
    pub storage_id: Uuid,
    pub storage_object_id: String,
    pub evidence: MatchEvidence,
}

impl StorageInventory {
    /// Creates a completed inventory, including the meaningful empty inventory case.
    pub fn available(storage_id: Uuid, objects: Vec<StorageObject>) -> Self {
        Self {
            storage_id,
            availability: InventoryAvailability::Available,
            objects,
        }
    }
}

/// All immutable data needed for one reconciliation pass.
#[derive(Debug, Clone, Default)]
pub struct ReconciliationInput {
    /// The active primary selected by profile configuration. No selection is not an
    /// empty primary and therefore produces `StorageUnavailable` outcomes.
    pub primary_storage_id: Option<Uuid>,
    pub sources: Vec<CatalogSource>,
    pub tracks: Vec<CanonicalTrack>,
    pub occurrences: Vec<SourceTrackOccurrence>,
    pub originals: Vec<OriginalAsset>,
    pub locations: Vec<AssetLocation>,
    pub storages: Vec<MusicStorage>,
    pub inventories: Vec<StorageInventory>,
    pub playlists: Vec<CanonicalPlaylist>,
    /// Strong evidence not representable directly by [`StorageObject`]. Invalid
    /// methods and evidence for unknown objects are ignored conservatively.
    pub external_evidence: Vec<ExternalMatchEvidence>,
    /// Previously persisted decisions consumed before findings are returned.
    pub resolutions: Vec<ReconciliationResolution>,
    /// Active durable suppressions, normally derived atomically from resolutions.
    pub suppressions: Vec<ReconciliationSuppression>,
}

/// The complete result of a pure reconciliation pass.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconciliationReport {
    pub primary_storage_id: Option<Uuid>,
    /// One aggregate status for each unique track required by enabled sources.
    pub track_outcomes: Vec<TrackStorageOutcome>,
    /// One aggregate status for every original belonging to a required track.
    pub asset_outcomes: Vec<AssetStorageOutcome>,
    pub playlist_outcomes: Vec<PlaylistStorageOutcome>,
    /// Metadata-only candidates. Strong matches are applied directly to outcomes.
    pub candidates: Vec<MatchCandidate>,
    /// The strongest selected evidence for every automatically verified object.
    pub verified_matches: Vec<VerifiedObjectMatch>,
    /// Actionable findings, including association, changed, and unknown objects.
    pub results: Vec<ReconciliationResult>,
    pub summary: ReconciliationSummary,
}

/// Runs reconciliation without provider calls, clock reads, or random identifiers.
///
/// Result ordering and generated candidate/finding IDs are derived from stable input
/// identities, making equal snapshots produce equal reports. Metadata candidates are
/// intentionally never strong evidence: they always require confirmation.
pub fn reconcile(input: &ReconciliationInput) -> ReconciliationReport {
    let primary = input.primary_storage_id;
    let source_by_id: HashMap<_, _> = input
        .sources
        .iter()
        .map(|source| (source.id, source))
        .collect();
    let track_by_id: BTreeMap<_, _> = input.tracks.iter().map(|track| (track.id, track)).collect();
    let required_track_ids = required_track_ids_from_index(input, &source_by_id, &track_by_id);
    let requirements: BTreeMap<_, _> = required_track_ids
        .iter()
        .filter_map(|id| {
            track_by_id
                .get(id)
                .map(|track| (*id, source_availability(track, input, &source_by_id)))
        })
        .collect();

    let primary_state = primary.and_then(|storage_id| {
        let storage = input
            .storages
            .iter()
            .find(|storage| storage.id == storage_id)?;
        if !storage.enabled {
            return Some(InventoryAvailability::Unavailable);
        }
        Some(
            input
                .inventories
                .iter()
                .find(|inventory| inventory.storage_id == storage_id)
                .map(|inventory| inventory.availability)
                .unwrap_or(InventoryAvailability::Unavailable),
        )
    });
    let objects = primary
        .and_then(|storage_id| {
            input
                .inventories
                .iter()
                .find(|inventory| inventory.storage_id == storage_id)
        })
        .filter(|inventory| inventory.availability == InventoryAvailability::Available)
        .map(|inventory| {
            let mut objects: Vec<_> = inventory
                .objects
                .iter()
                .filter(|object| !object.is_trashed)
                .collect();
            objects.sort_by(|left, right| left.object_id.cmp(&right.object_id));
            objects
        })
        .unwrap_or_default();

    let mut originals_by_track: BTreeMap<Uuid, Vec<&OriginalAsset>> = BTreeMap::new();
    for original in &input.originals {
        if requirements.contains_key(&original.track_id()) {
            originals_by_track
                .entry(original.track_id())
                .or_default()
                .push(original);
        }
    }
    for originals in originals_by_track.values_mut() {
        originals.sort_by_key(|asset| asset.id());
    }

    let mut evaluations = Vec::new();
    if let (Some(storage_id), Some(InventoryAvailability::Available)) = (primary, primary_state) {
        for track_id in &required_track_ids {
            if let Some(originals) = originals_by_track.get(track_id) {
                for original in originals {
                    evaluations.push(evaluate_asset(
                        original,
                        storage_id,
                        primary_state.unwrap_or(InventoryAvailability::Unavailable),
                        &objects,
                        &input.locations,
                        &input.external_evidence,
                        track_by_id[track_id],
                    ));
                }
            }
        }
    } else if let Some(storage_id) = primary {
        let availability = primary_state.unwrap_or(InventoryAvailability::Unavailable);
        for track_id in &required_track_ids {
            if let Some(originals) = originals_by_track.get(track_id) {
                for original in originals {
                    evaluations.push(evaluate_asset(
                        original,
                        storage_id,
                        availability,
                        &objects,
                        &input.locations,
                        &input.external_evidence,
                        track_by_id[track_id],
                    ));
                }
            }
        }
    }

    add_candidate_conflicts(&mut evaluations);

    let mut candidates = Vec::new();
    let mut verified_matches = Vec::new();
    let mut asset_outcomes = Vec::new();
    let mut asset_results = Vec::new();
    for evaluation in &evaluations {
        candidates.extend(evaluation.candidates.clone());
        verified_matches.extend(evaluation.verified_matches.clone());
        let status = evaluation.status();
        asset_outcomes.push(
            AssetStorageOutcome::new(evaluation.asset.id(), evaluation.asset.track_id(), status)
                .with_verified_locations(evaluation.verified_location_ids.clone()),
        );
        if status != AssetStorageStatus::Verified || evaluation.needs_association() {
            asset_results.push(asset_result(evaluation, status));
        }
    }
    if primary.is_none() {
        for track_id in &required_track_ids {
            if let Some(originals) = originals_by_track.get(track_id) {
                for original in originals {
                    asset_outcomes.push(AssetStorageOutcome::new(
                        original.id(),
                        original.track_id(),
                        AssetStorageStatus::Unavailable,
                    ));
                }
            }
        }
    }

    let mut track_outcomes = Vec::new();
    let mut track_results = Vec::new();
    for track_id in &required_track_ids {
        let evaluations: Vec<_> = evaluations
            .iter()
            .filter(|evaluation| evaluation.asset.track_id() == *track_id)
            .collect();
        let status = track_status(primary_state, &evaluations, requirements[track_id]);
        track_outcomes.push(
            TrackStorageOutcome::new(*track_id, status).with_verified_assets(
                evaluations
                    .iter()
                    .filter(|evaluation| evaluation.status() == AssetStorageStatus::Verified)
                    .map(|evaluation| evaluation.asset.id())
                    .collect(),
            ),
        );
        if status != TrackStorageStatus::StoredAndVerified {
            track_results.push(track_result(
                *track_id,
                status,
                &evaluations,
                primary,
                managed_copy_source(&evaluations, input, primary),
                source_for_copy(track_by_id[track_id], input, &source_by_id),
            ));
        }
    }

    let mut results = asset_results;
    results.extend(track_results);
    if let (Some(storage_id), Some(InventoryAvailability::Available)) = (primary, primary_state) {
        for evaluation in &evaluations {
            for object_id in &evaluation.changed_object_ids {
                results.push(changed_object_result(evaluation, storage_id, object_id));
            }
        }
        let strongly_tracked: HashSet<_> = evaluations
            .iter()
            .flat_map(|evaluation| evaluation.strong_object_ids.iter())
            .cloned()
            .collect();
        for object in &objects {
            if !strongly_tracked.contains(&object.object_id) {
                let object_candidates: Vec<_> = candidates
                    .iter()
                    .filter(|candidate| candidate.storage_object_id == object.object_id)
                    .cloned()
                    .collect();
                results.push(untracked_object_result(
                    storage_id,
                    &object.object_id,
                    object_candidates,
                ));
            }
        }
    }

    apply_resolutions_and_suppressions(
        &mut candidates,
        &mut results,
        &input.resolutions,
        &input.suppressions,
    );
    candidates.sort_by(candidate_order);
    verified_matches.sort_by(|left, right| {
        (
            left.storage_id,
            &left.storage_object_id,
            left.track_id,
            left.asset_id,
        )
            .cmp(&(
                right.storage_id,
                &right.storage_object_id,
                right.track_id,
                right.asset_id,
            ))
    });
    results.sort_by(result_order);
    asset_outcomes.sort_by_key(|outcome| outcome.asset_id);
    track_outcomes.sort_by_key(|outcome| outcome.track_id);

    let mut playlist_outcomes = Vec::new();
    let mut playlists: Vec<_> = input.playlists.iter().collect();
    playlists.sort_by_key(|playlist| playlist.id);
    for playlist in playlists {
        let track_ids = playlist
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .filter(|id| requirements.contains_key(id));
        // The outcomes already contain only enabled-source requirements.
        playlist_outcomes.push(
            PlaylistStorageOutcome::aggregate(
                playlist.id,
                track_ids,
                &track_outcomes,
                &asset_outcomes,
            )
            .expect("the engine emits one aggregate outcome per track and asset"),
        );
    }

    let summary = ReconciliationSummary::aggregate(&track_outcomes, &asset_outcomes)
        .expect("the engine emits one aggregate outcome per track and asset");
    ReconciliationReport {
        primary_storage_id: primary,
        track_outcomes,
        asset_outcomes,
        playlist_outcomes,
        candidates,
        verified_matches,
        results,
        summary,
    }
}

/// Returns the unique canonical tracks discovered through enabled source occurrences.
pub fn required_track_ids(input: &ReconciliationInput) -> Vec<Uuid> {
    let sources: HashMap<_, _> = input
        .sources
        .iter()
        .map(|source| (source.id, source))
        .collect();
    let tracks: BTreeMap<_, _> = input.tracks.iter().map(|track| (track.id, track)).collect();
    required_track_ids_from_index(input, &sources, &tracks)
}

fn required_track_ids_from_index(
    input: &ReconciliationInput,
    sources: &HashMap<Uuid, &CatalogSource>,
    tracks: &BTreeMap<Uuid, &CanonicalTrack>,
) -> Vec<Uuid> {
    let mut ids = BTreeSet::new();
    for occurrence in &input.occurrences {
        if occurrence
            .canonical_track_id
            .is_some_and(|track_id| tracks.contains_key(&track_id))
            && sources
                .get(&occurrence.identity.source_id)
                .is_some_and(|source| source.enabled)
        {
            ids.insert(occurrence.canonical_track_id.expect("checked above"));
        }
    }
    for track in tracks.values() {
        if track.occurrences.iter().any(|identity| {
            sources
                .get(&identity.source_id)
                .is_some_and(|source| source.enabled)
        }) {
            ids.insert(track.id);
        }
    }
    ids.into_iter().collect()
}

fn source_availability(
    track: &CanonicalTrack,
    input: &ReconciliationInput,
    sources: &HashMap<Uuid, &CatalogSource>,
) -> SourceAvailability {
    let mut availability = Vec::new();
    for occurrence in &input.occurrences {
        let belongs_to_track = occurrence.canonical_track_id == Some(track.id)
            || track.occurrences.contains(&occurrence.identity);
        if belongs_to_track
            && sources
                .get(&occurrence.identity.source_id)
                .is_some_and(|source| source.enabled)
        {
            availability.push(occurrence.availability);
        }
    }
    availability
        .into_iter()
        .max_by_key(|availability| availability_rank(*availability))
        .unwrap_or(SourceAvailability::MetadataOnly)
}

fn availability_rank(availability: SourceAvailability) -> u8 {
    match availability {
        SourceAvailability::Obtainable => 7,
        SourceAvailability::StreamingOnly => 6,
        SourceAvailability::PreviewOnly => 5,
        SourceAvailability::Blocked => 4,
        SourceAvailability::Removed => 3,
        SourceAvailability::Inaccessible => 2,
        SourceAvailability::MetadataOnly => 1,
    }
}

#[derive(Clone)]
struct AssetEvaluation<'a> {
    asset: &'a OriginalAsset,
    storage_id: Uuid,
    availability: InventoryAvailability,
    candidates: Vec<MatchCandidate>,
    verified_matches: Vec<VerifiedObjectMatch>,
    verified_location_ids: Vec<Uuid>,
    strong_object_ids: Vec<String>,
    changed_object_ids: Vec<String>,
    persisted_status: Option<StorageVerificationStatus>,
}

impl AssetEvaluation<'_> {
    fn status(&self) -> AssetStorageStatus {
        match self.availability {
            InventoryAvailability::Unavailable => AssetStorageStatus::Unavailable,
            InventoryAvailability::Error => AssetStorageStatus::Error,
            InventoryAvailability::Available if !self.strong_object_ids.is_empty() => {
                AssetStorageStatus::Verified
            }
            InventoryAvailability::Available if !self.changed_object_ids.is_empty() => {
                AssetStorageStatus::ChangedPendingReview
            }
            InventoryAvailability::Available
                if self
                    .candidates
                    .iter()
                    .any(|candidate| !candidate.conflicting_track_ids.is_empty()) =>
            {
                AssetStorageStatus::AmbiguousMatch
            }
            InventoryAvailability::Available if !self.candidates.is_empty() => {
                AssetStorageStatus::PossibleMatch
            }
            InventoryAvailability::Available => match self.persisted_status {
                Some(StorageVerificationStatus::Unavailable) => AssetStorageStatus::Unavailable,
                Some(StorageVerificationStatus::Error) => AssetStorageStatus::Error,
                Some(StorageVerificationStatus::PossibleMatch) => AssetStorageStatus::PossibleMatch,
                Some(StorageVerificationStatus::AmbiguousMatch) => {
                    AssetStorageStatus::AmbiguousMatch
                }
                Some(StorageVerificationStatus::ChangedPendingReview) => {
                    AssetStorageStatus::ChangedPendingReview
                }
                _ => AssetStorageStatus::Missing,
            },
        }
    }

    fn needs_association(&self) -> bool {
        self.verified_matches
            .iter()
            .any(|matched| matched.evidence.method != MatchMethod::PersistedAssociation)
    }
}

fn evaluate_asset<'a>(
    asset: &'a OriginalAsset,
    storage_id: Uuid,
    availability: InventoryAvailability,
    objects: &[&StorageObject],
    locations: &[AssetLocation],
    external_evidence: &[ExternalMatchEvidence],
    track: &CanonicalTrack,
) -> AssetEvaluation<'a> {
    let locations: Vec<_> = locations
        .iter()
        .filter(|location| location.asset_id == asset.id() && location.storage_id == storage_id)
        .collect();
    let mut strong_object_ids = BTreeSet::new();
    let mut changed_object_ids = BTreeSet::new();
    let mut verified_matches = Vec::new();
    let mut verified_location_ids = BTreeSet::new();
    let mut candidates = Vec::new();
    let persisted_status = locations
        .iter()
        .map(|location| location.verification_status)
        .max_by_key(|status| persisted_status_rank(*status));

    if availability == InventoryAvailability::Available {
        for object in objects {
            let location = locations
                .iter()
                .find(|location| location.object_id == object.object_id);
            let changed = location.is_some_and(|location| location_changed(location, object));
            if changed {
                changed_object_ids.insert(object.object_id.clone());
            }

            let selected_evidence = if location
                .is_some_and(|location| unchanged_persisted_association(location, object))
            {
                Some(MatchEvidence::new(
                    MatchMethod::PersistedAssociation,
                    location.and_then(|location| location.provider_version.clone()),
                ))
            } else if asset
                .content_hash
                .as_ref()
                .is_some_and(|hash| object.canonical_hash() == Some(hash))
            {
                Some(MatchEvidence::new(
                    MatchMethod::ExactContentHash,
                    object
                        .canonical_hash()
                        .map(|hash| format!("sha256:{}", hash.value())),
                ))
            } else if object.embedded_asset_id == Some(asset.id()) {
                Some(MatchEvidence::new(
                    MatchMethod::StableAssetIdentifier,
                    Some(asset.id().to_string()),
                ))
            } else {
                external_evidence
                    .iter()
                    .filter(|evidence| {
                        evidence.asset_id == asset.id()
                            && evidence.storage_id == storage_id
                            && evidence.storage_object_id == object.object_id
                            && evidence.is_valid_strong_evidence()
                    })
                    .min_by_key(|evidence| match evidence.method {
                        MatchMethod::RecordedTransfer => 0,
                        MatchMethod::VerifiedStandardIdentifier => 1,
                        _ => 2,
                    })
                    .map(|evidence| MatchEvidence::new(evidence.method, evidence.value.clone()))
            };

            if let Some(evidence) = selected_evidence {
                strong_object_ids.insert(object.object_id.clone());
                changed_object_ids.remove(&object.object_id);
                if let Some(location) = location {
                    verified_location_ids.insert(location.id);
                }
                verified_matches.push(VerifiedObjectMatch {
                    asset_id: asset.id(),
                    track_id: asset.track_id(),
                    storage_id,
                    storage_object_id: object.object_id.clone(),
                    evidence,
                });
                continue;
            }

            if let Some((score, detail)) = metadata_score(asset, track, object) {
                candidates.push(candidate_for(asset, storage_id, object, score, detail));
            }
        }
    }

    AssetEvaluation {
        asset,
        storage_id,
        availability,
        candidates,
        verified_matches,
        verified_location_ids: verified_location_ids.into_iter().collect(),
        strong_object_ids: strong_object_ids.into_iter().collect(),
        changed_object_ids: changed_object_ids.into_iter().collect(),
        persisted_status,
    }
}

fn persisted_status_rank(status: StorageVerificationStatus) -> u8 {
    match status {
        StorageVerificationStatus::Error => 7,
        StorageVerificationStatus::Unavailable => 6,
        StorageVerificationStatus::ChangedPendingReview => 5,
        StorageVerificationStatus::AmbiguousMatch => 4,
        StorageVerificationStatus::PossibleMatch => 3,
        StorageVerificationStatus::Missing => 2,
        StorageVerificationStatus::Verified => 1,
    }
}

fn unchanged_persisted_association(location: &AssetLocation, object: &StorageObject) -> bool {
    if location.verification_status != StorageVerificationStatus::Verified {
        return false;
    }

    match (&location.provider_version, &object.provider_version) {
        (Some(known), Some(observed)) => known == observed,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => location
            .observed_hash
            .as_ref()
            .or(location.expected_hash.as_ref())
            .zip(object.canonical_hash())
            .is_some_and(|(known, observed)| known == observed),
    }
}

fn location_changed(location: &AssetLocation, object: &StorageObject) -> bool {
    location.verification_status == StorageVerificationStatus::ChangedPendingReview
        || match (&location.provider_version, &object.provider_version) {
            (Some(known), Some(observed)) => known != observed,
            (Some(_), None) => true,
            _ => false,
        }
        || match (&location.expected_hash, object.canonical_hash()) {
            (Some(expected), Some(observed)) => expected != observed,
            (Some(_), None) => true,
            _ => false,
        }
        || match (&location.observed_hash, object.canonical_hash()) {
            (Some(observed), Some(current)) => observed != current,
            (Some(_), None) => true,
            _ => false,
        }
}

fn metadata_score(
    asset: &OriginalAsset,
    track: &CanonicalTrack,
    object: &StorageObject,
) -> Option<(u8, String)> {
    let stem = normalize(&file_stem(&object.name));
    let asset_name = normalize(&file_stem(&asset.file_name));
    let title = metadata_text(track, "title");
    let artist = metadata_text(track, "artist");
    let mut score = 0;
    let mut evidence = Vec::new();
    if !asset_name.is_empty() && stem == asset_name {
        score += 70;
        evidence.push("filename");
    }
    if let Some(title) = &title {
        if !title.is_empty() && (stem == *title || stem.contains(title)) {
            score += 60;
            evidence.push("title");
        }
    }
    if let Some(artist) = &artist {
        if !artist.is_empty() && stem.contains(artist) {
            score += 20;
            evidence.push("artist");
        }
    }
    (score >= 60).then(|| (score, evidence.join(" + ")))
}

fn metadata_text(track: &CanonicalTrack, name: &str) -> Option<String> {
    let field = match name {
        "title" => rec_core::MetadataField::Title,
        "artist" => rec_core::MetadataField::Artist,
        _ => return None,
    };
    match track
        .metadata
        .preferred(&field)
        .map(|candidate| &candidate.value)
    {
        Some(rec_core::MetadataValue::Text(value)) => Some(normalize(value)),
        _ => None,
    }
}

fn file_stem(name: &str) -> String {
    name.rsplit_once('.')
        .map_or_else(|| name.to_owned(), |(stem, _)| stem.to_owned())
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn candidate_for(
    asset: &OriginalAsset,
    storage_id: Uuid,
    object: &StorageObject,
    score: u8,
    detail: String,
) -> MatchCandidate {
    MatchCandidate {
        id: stable_id(&[
            "candidate",
            &asset.track_id().to_string(),
            &asset.id().to_string(),
            &storage_id.to_string(),
            &object.object_id,
        ]),
        track_id: asset.track_id(),
        asset_id: Some(asset.id()),
        storage_id,
        storage_object_id: object.object_id.clone(),
        evidence: vec![MatchEvidence::new(
            MatchMethod::MetadataOnly,
            Some(format!("metadata score {score}: {detail}")),
        )],
        conflicting_track_ids: Vec::new(),
    }
}

fn add_candidate_conflicts(evaluations: &mut [AssetEvaluation<'_>]) {
    let mut tracks_by_object = BTreeMap::<(Uuid, String), BTreeSet<Uuid>>::new();
    for evaluation in evaluations.iter() {
        for candidate in &evaluation.candidates {
            tracks_by_object
                .entry((candidate.storage_id, candidate.storage_object_id.clone()))
                .or_default()
                .insert(candidate.track_id);
        }
    }
    for evaluation in evaluations {
        for candidate in &mut evaluation.candidates {
            candidate.conflicting_track_ids = tracks_by_object
                .get(&(candidate.storage_id, candidate.storage_object_id.clone()))
                .into_iter()
                .flatten()
                .copied()
                .filter(|track_id| *track_id != candidate.track_id)
                .collect();
        }
    }
}

fn track_status(
    primary_state: Option<InventoryAvailability>,
    evaluations: &[&AssetEvaluation<'_>],
    availability: SourceAvailability,
) -> TrackStorageStatus {
    match primary_state {
        None | Some(InventoryAvailability::Unavailable) => TrackStorageStatus::StorageUnavailable,
        Some(InventoryAvailability::Error) => TrackStorageStatus::Error,
        Some(InventoryAvailability::Available) => {
            if evaluations
                .iter()
                .any(|evaluation| evaluation.status() == AssetStorageStatus::Verified)
            {
                TrackStorageStatus::StoredAndVerified
            } else if evaluations
                .iter()
                .any(|evaluation| evaluation.status() == AssetStorageStatus::ChangedPendingReview)
            {
                TrackStorageStatus::Error
            } else if evaluations
                .iter()
                .any(|evaluation| evaluation.status() == AssetStorageStatus::AmbiguousMatch)
            {
                TrackStorageStatus::AmbiguousMatch
            } else if evaluations
                .iter()
                .any(|evaluation| evaluation.status() == AssetStorageStatus::PossibleMatch)
            {
                TrackStorageStatus::PossibleMatch
            } else {
                match availability {
                    SourceAvailability::Obtainable => TrackStorageStatus::MissingObtainable,
                    SourceAvailability::StreamingOnly | SourceAvailability::MetadataOnly => {
                        TrackStorageStatus::NotAvailableForArchival
                    }
                    SourceAvailability::PreviewOnly => TrackStorageStatus::PreviewOnly,
                    SourceAvailability::Blocked => TrackStorageStatus::Blocked,
                    SourceAvailability::Removed => TrackStorageStatus::Removed,
                    SourceAvailability::Inaccessible => TrackStorageStatus::Inaccessible,
                }
            }
        }
    }
}

fn asset_result(
    evaluation: &AssetEvaluation<'_>,
    status: AssetStorageStatus,
) -> ReconciliationResult {
    let mut result = result_with_id(
        &[
            "asset",
            &evaluation.asset.id().to_string(),
            &evaluation.storage_id.to_string(),
            &format!("{status:?}"),
        ],
        ReconciliationOutcome::Asset(status),
        Some(evaluation.asset.track_id()),
        Some(evaluation.asset.id()),
        Some((evaluation.storage_id, String::new())),
    );
    result.candidates = evaluation.candidates.clone();
    if status == AssetStorageStatus::Verified {
        for matched in &evaluation.verified_matches {
            if matched.evidence.method != MatchMethod::PersistedAssociation {
                result.actions.push(ReconciliationAction::Associate {
                    track_id: matched.track_id,
                    asset_id: Some(matched.asset_id),
                    storage_id: matched.storage_id,
                    storage_object_id: matched.storage_object_id.clone(),
                });
            }
        }
    }
    if matches!(
        status,
        AssetStorageStatus::PossibleMatch | AssetStorageStatus::AmbiguousMatch
    ) {
        for candidate in &result.candidates {
            result.actions.push(ReconciliationAction::Confirm {
                candidate_id: candidate.id,
            });
            result.actions.push(ReconciliationAction::Associate {
                track_id: candidate.track_id,
                asset_id: candidate.asset_id,
                storage_id: candidate.storage_id,
                storage_object_id: candidate.storage_object_id.clone(),
            });
            result.actions.push(ReconciliationAction::Reject {
                candidate_id: candidate.id,
            });
        }
    }
    if matches!(status, AssetStorageStatus::Missing) {
        result.actions.push(ReconciliationAction::Locate {
            track_id: evaluation.asset.track_id(),
        });
    }
    if matches!(
        status,
        AssetStorageStatus::Unavailable
            | AssetStorageStatus::Error
            | AssetStorageStatus::ChangedPendingReview
    ) {
        result.actions.push(ReconciliationAction::Retry {
            result_id: result.id,
        });
    }
    if status != AssetStorageStatus::Verified {
        result.actions.push(ReconciliationAction::Ignore {
            result_id: result.id,
            reason: None,
        });
    }
    result
}

fn track_result(
    track_id: Uuid,
    status: TrackStorageStatus,
    evaluations: &[&AssetEvaluation<'_>],
    primary: Option<Uuid>,
    managed_copy_source: Option<(Uuid, Uuid)>,
    copy_source_id: Option<Uuid>,
) -> ReconciliationResult {
    let mut result = result_with_id(
        &["track", &track_id.to_string(), &format!("{status:?}")],
        ReconciliationOutcome::Track(status),
        Some(track_id),
        None,
        primary.map(|storage_id| (storage_id, String::new())),
    );
    match status {
        TrackStorageStatus::MissingObtainable => {
            let asset = evaluations.first().map(|evaluation| evaluation.asset);
            if let (Some((asset_id, source_storage_id)), Some(destination_storage_id)) =
                (managed_copy_source, primary)
            {
                result.actions.push(ReconciliationAction::Copy {
                    track_id,
                    asset_id,
                    source_storage_id,
                    destination_storage_id,
                });
            } else if let (Some(asset), Some(storage_id), Some(source_id)) =
                (asset, primary, copy_source_id)
            {
                result.actions.push(ReconciliationAction::CopyFromSource {
                    track_id,
                    asset_id: asset.id(),
                    source_id,
                    destination_storage_id: storage_id,
                });
            } else {
                result
                    .actions
                    .push(ReconciliationAction::Locate { track_id });
            }
        }
        TrackStorageStatus::PossibleMatch | TrackStorageStatus::AmbiguousMatch => {
            for candidate in evaluations
                .iter()
                .flat_map(|evaluation| evaluation.candidates.iter())
            {
                result.actions.push(ReconciliationAction::Confirm {
                    candidate_id: candidate.id,
                });
                result.actions.push(ReconciliationAction::Associate {
                    track_id: candidate.track_id,
                    asset_id: candidate.asset_id,
                    storage_id: candidate.storage_id,
                    storage_object_id: candidate.storage_object_id.clone(),
                });
                result.actions.push(ReconciliationAction::Reject {
                    candidate_id: candidate.id,
                });
            }
        }
        TrackStorageStatus::StorageUnavailable | TrackStorageStatus::Error => {
            result.actions.push(ReconciliationAction::Retry {
                result_id: result.id,
            });
        }
        TrackStorageStatus::StoredAndVerified
        | TrackStorageStatus::NotAvailableForArchival
        | TrackStorageStatus::PreviewOnly
        | TrackStorageStatus::Blocked
        | TrackStorageStatus::Removed
        | TrackStorageStatus::Inaccessible
        | TrackStorageStatus::Ignored => result
            .actions
            .push(ReconciliationAction::Locate { track_id }),
    }
    if status != TrackStorageStatus::StoredAndVerified {
        result.actions.push(ReconciliationAction::Ignore {
            result_id: result.id,
            reason: None,
        });
    }
    result
}

fn managed_copy_source(
    evaluations: &[&AssetEvaluation<'_>],
    input: &ReconciliationInput,
    primary: Option<Uuid>,
) -> Option<(Uuid, Uuid)> {
    let asset_ids: BTreeSet<_> = evaluations
        .iter()
        .map(|evaluation| evaluation.asset.id())
        .collect();
    input
        .locations
        .iter()
        .filter(|location| {
            asset_ids.contains(&location.asset_id)
                && Some(location.storage_id) != primary
                && location.verification_status == StorageVerificationStatus::Verified
                && input
                    .storages
                    .iter()
                    .any(|storage| storage.id == location.storage_id && storage.enabled)
        })
        .map(|location| (location.asset_id, location.storage_id))
        .min()
}

fn source_for_copy(
    track: &CanonicalTrack,
    input: &ReconciliationInput,
    sources: &HashMap<Uuid, &CatalogSource>,
) -> Option<Uuid> {
    input
        .occurrences
        .iter()
        .filter(|occurrence| {
            (occurrence.canonical_track_id == Some(track.id)
                || track.occurrences.contains(&occurrence.identity))
                && occurrence.availability == SourceAvailability::Obtainable
                && sources
                    .get(&occurrence.identity.source_id)
                    .is_some_and(|source| {
                        source.enabled && source.copy_policy != SourceCopyPolicy::CheckOnly
                    })
        })
        .map(|occurrence| occurrence.identity.source_id)
        .min()
}

fn apply_resolutions_and_suppressions(
    candidates: &mut Vec<MatchCandidate>,
    results: &mut Vec<ReconciliationResult>,
    resolutions: &[ReconciliationResolution],
    suppressions: &[ReconciliationSuppression],
) {
    let ignored_results: HashSet<_> = resolutions
        .iter()
        .filter(|resolution| resolution.decision == ReconciliationDecision::IgnoreFinding)
        .map(|resolution| resolution.result_id)
        .chain(suppressions.iter().filter_map(|suppression| {
            (suppression.decision == ReconciliationDecision::IgnoreFinding)
                .then_some(suppression.result_id)
        }))
        .collect();
    let rejected_candidates: HashSet<_> = resolutions
        .iter()
        .filter(|resolution| resolution.decision == ReconciliationDecision::RejectCandidate)
        .filter_map(|resolution| resolution.candidate_id)
        .chain(suppressions.iter().filter_map(|suppression| {
            (suppression.decision == ReconciliationDecision::RejectCandidate)
                .then_some(suppression.candidate_id)
                .flatten()
        }))
        .collect();
    let rejected_matches: HashSet<_> = candidates
        .iter()
        .filter(|candidate| rejected_candidates.contains(&candidate.id))
        .map(|candidate| {
            (
                candidate.track_id,
                candidate.asset_id,
                candidate.storage_id,
                candidate.storage_object_id.clone(),
            )
        })
        .collect();

    candidates.retain(|candidate| !rejected_candidates.contains(&candidate.id));
    results.retain_mut(|result| {
        if ignored_results.contains(&result.id) {
            return false;
        }
        let had_candidates = !result.candidates.is_empty();
        result
            .candidates
            .retain(|candidate| !rejected_candidates.contains(&candidate.id));
        result.actions.retain(|action| match action {
            ReconciliationAction::Confirm { candidate_id }
            | ReconciliationAction::Reject { candidate_id } => {
                !rejected_candidates.contains(candidate_id)
            }
            ReconciliationAction::Associate {
                track_id,
                asset_id,
                storage_id,
                storage_object_id,
            }
            | ReconciliationAction::AssociateUnknownObject {
                track_id,
                asset_id,
                storage_id,
                storage_object_id,
                ..
            } => !rejected_matches.contains(&(
                *track_id,
                *asset_id,
                *storage_id,
                storage_object_id.clone(),
            )),
            _ => true,
        });
        !(had_candidates
            && result.candidates.is_empty()
            && matches!(
                result.outcome,
                ReconciliationOutcome::Asset(
                    AssetStorageStatus::PossibleMatch | AssetStorageStatus::AmbiguousMatch
                ) | ReconciliationOutcome::Track(
                    TrackStorageStatus::PossibleMatch | TrackStorageStatus::AmbiguousMatch
                )
            ))
    });
}

fn changed_object_result(
    evaluation: &AssetEvaluation<'_>,
    storage_id: Uuid,
    object_id: &str,
) -> ReconciliationResult {
    let mut result = result_with_id(
        &[
            "changed",
            &evaluation.asset.id().to_string(),
            &storage_id.to_string(),
            object_id,
        ],
        ReconciliationOutcome::ChangedStorageObject,
        Some(evaluation.asset.track_id()),
        Some(evaluation.asset.id()),
        Some((storage_id, object_id.to_owned())),
    );
    result
        .actions
        .push(ReconciliationAction::ApproveExternalChange {
            result_id: result.id,
            storage_id,
            storage_object_id: object_id.to_owned(),
        });
    result.actions.push(ReconciliationAction::Retry {
        result_id: result.id,
    });
    result.actions.push(ReconciliationAction::Ignore {
        result_id: result.id,
        reason: None,
    });
    result
}

fn untracked_object_result(
    storage_id: Uuid,
    object_id: &str,
    candidates: Vec<MatchCandidate>,
) -> ReconciliationResult {
    let mut result = result_with_id(
        &["untracked", &storage_id.to_string(), object_id],
        ReconciliationOutcome::UntrackedStorageObject,
        None,
        None,
        Some((storage_id, object_id.to_owned())),
    );
    result.candidates = candidates;
    for candidate in &result.candidates {
        result.actions.push(ReconciliationAction::Confirm {
            candidate_id: candidate.id,
        });
        result
            .actions
            .push(ReconciliationAction::AssociateUnknownObject {
                result_id: result.id,
                track_id: candidate.track_id,
                asset_id: candidate.asset_id,
                storage_id: candidate.storage_id,
                storage_object_id: candidate.storage_object_id.clone(),
            });
        result.actions.push(ReconciliationAction::Reject {
            candidate_id: candidate.id,
        });
    }
    result
        .actions
        .push(ReconciliationAction::ImportUnknownObject {
            result_id: result.id,
            storage_id,
            storage_object_id: object_id.to_owned(),
        });
    result.actions.push(ReconciliationAction::Ignore {
        result_id: result.id,
        reason: None,
    });
    result
}

fn result_with_id(
    parts: &[&str],
    outcome: ReconciliationOutcome,
    track_id: Option<Uuid>,
    asset_id: Option<Uuid>,
    storage_object: Option<(Uuid, String)>,
) -> ReconciliationResult {
    ReconciliationResult {
        id: stable_id(parts),
        track_id,
        asset_id,
        storage_id: storage_object.as_ref().map(|(storage_id, _)| *storage_id),
        storage_object_id: storage_object
            .map(|(_, object_id)| object_id)
            .filter(|object_id| !object_id.is_empty()),
        outcome,
        candidates: Vec::new(),
        actions: Vec::new(),
    }
}

fn stable_id(parts: &[&str]) -> Uuid {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Mark it as a deterministic UUID version while retaining the digest bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn candidate_order(left: &MatchCandidate, right: &MatchCandidate) -> std::cmp::Ordering {
    (
        left.storage_id,
        &left.storage_object_id,
        left.track_id,
        left.asset_id,
    )
        .cmp(&(
            right.storage_id,
            &right.storage_object_id,
            right.track_id,
            right.asset_id,
        ))
}

fn result_order(left: &ReconciliationResult, right: &ReconciliationResult) -> std::cmp::Ordering {
    (
        left.storage_id,
        left.storage_object_id.as_deref(),
        left.track_id,
        left.asset_id,
        left.id,
    )
        .cmp(&(
            right.storage_id,
            right.storage_object_id.as_deref(),
            right.track_id,
            right.asset_id,
            right.id,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rec_core::{
        CanonicalMetadata, CatalogSourceKind, ContentHash, MetadataField, MetadataFieldValue,
        MetadataProvenance, MetadataValue, MusicStorageKind, OriginalAssetRole, SourceCapabilities,
        StorageCapabilities,
    };

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn source() -> CatalogSource {
        let mut source = CatalogSource::new("Library", CatalogSourceKind::LocalFolder);
        source.capabilities = SourceCapabilities::local_folder();
        source
    }

    fn primary() -> MusicStorage {
        MusicStorage::new(
            "Primary",
            MusicStorageKind::LocalDirectory,
            rec_core::StorageRole::Primary,
            StorageCapabilities {
                can_inventory: true,
                can_stat: true,
                can_upload: true,
                can_download: true,
                ..Default::default()
            },
        )
    }

    fn track(source_id: Uuid, title: &str) -> CanonicalTrack {
        let mut track = CanonicalTrack::new();
        track.metadata = CanonicalMetadata::new();
        track.metadata.add(MetadataFieldValue::new(
            MetadataField::Title,
            MetadataValue::Text(title.to_owned()),
            MetadataProvenance::provider(source_id, "source-track", Utc::now()),
        ));
        track
    }

    fn occurrence(
        source_id: Uuid,
        track_id: Uuid,
        availability: SourceAvailability,
    ) -> SourceTrackOccurrence {
        let mut occurrence = SourceTrackOccurrence::new(source_id, track_id.to_string());
        occurrence.canonical_track_id = Some(track_id);
        occurrence.availability = availability;
        occurrence
    }

    fn original(track_id: Uuid) -> OriginalAsset {
        OriginalAsset::new(
            track_id,
            OriginalAssetRole::Master,
            "Artist - Song.mp3",
            1,
            HASH,
        )
        .unwrap()
    }

    fn object(storage_id: Uuid, id: &str, name: &str) -> StorageObject {
        StorageObject {
            storage_id,
            object_id: id.to_owned(),
            name: name.to_owned(),
            display_path: None,
            size_bytes: 1,
            media_type: None,
            modified_at: None,
            provider_version: Some("v1".to_owned()),
            hashes: vec![ContentHash::sha256(HASH).unwrap()],
            embedded_asset_id: None,
            is_trashed: false,
        }
    }

    #[test]
    fn exact_hash_match_verifies_an_original() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original = original(track.id);
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original.clone()],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(
                storage.id,
                vec![object(storage.id, "one", "random.mp3")],
            )],
            ..Default::default()
        });
        assert_eq!(
            report.asset_outcomes[0].status,
            AssetStorageStatus::Verified
        );
        assert_eq!(
            report.track_outcomes[0].status,
            TrackStorageStatus::StoredAndVerified
        );
    }

    #[test]
    fn unchanged_persisted_association_verifies_without_a_hash() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "Song.mp3");
        let mut object = object(storage.id, "one", "unhashed.mp3");
        object.hashes.clear();
        let location = AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.id(),
            asset_type: rec_core::AssetType::Original,
            storage_id: storage.id,
            object_id: "one".to_owned(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: None,
            observed_hash: None,
            provider_version: Some("v1".to_owned()),
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        };
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            locations: vec![location],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![object])],
            ..Default::default()
        });
        assert_eq!(
            report.asset_outcomes[0].status,
            AssetStorageStatus::Verified
        );
    }

    #[test]
    fn embedded_asset_identifier_is_a_strong_drive_style_match() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "different.mp3");
        let mut object = object(storage.id, "one", "different.mp3");
        object.hashes.clear();
        object.embedded_asset_id = Some(original.id());
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![object])],
            ..Default::default()
        });
        assert_eq!(
            report.asset_outcomes[0].status,
            AssetStorageStatus::Verified
        );
    }

    #[test]
    fn metadata_match_requires_confirmation() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "unrelated.mp3");
        let mut object = object(storage.id, "one", "Song.mp3");
        object.hashes.clear();
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![object])],
            ..Default::default()
        });
        assert_eq!(
            report.asset_outcomes[0].status,
            AssetStorageStatus::PossibleMatch
        );
        assert_eq!(
            report.candidates[0].confidence(),
            rec_core::MatchConfidence::ReviewRequired
        );
    }

    #[test]
    fn alternate_versions_create_an_ambiguous_metadata_match() {
        let source = source();
        let storage = primary();
        let first = track(source.id, "Song");
        let second = track(source.id, "Song");
        let first_asset =
            OriginalAsset::new_unverified(first.id, OriginalAssetRole::Master, "a.mp3");
        let second_asset =
            OriginalAsset::new_unverified(second.id, OriginalAssetRole::Alternate, "b.mp3");
        let mut object = object(storage.id, "one", "Song Remix.mp3");
        object.hashes.clear();
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![first.clone(), second.clone()],
            occurrences: vec![
                occurrence(source.id, first.id, SourceAvailability::Obtainable),
                occurrence(source.id, second.id, SourceAvailability::Obtainable),
            ],
            originals: vec![first_asset, second_asset],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![object])],
            ..Default::default()
        });
        assert!(report
            .asset_outcomes
            .iter()
            .all(|outcome| outcome.status == AssetStorageStatus::AmbiguousMatch));
    }

    #[test]
    fn one_verified_original_is_enough_for_a_track_with_many_originals() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let first = original(track.id);
        let second =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Alternate, "missing.mp3");
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![first, second],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(
                storage.id,
                vec![object(storage.id, "one", "random.mp3")],
            )],
            ..Default::default()
        });
        assert_eq!(
            report.track_outcomes[0].status,
            TrackStorageStatus::StoredAndVerified
        );
        assert_eq!(report.summary.total_assets, 2);
        assert_eq!(report.summary.verified_assets, 1);
    }

    #[test]
    fn playlist_reports_unique_tracks_and_soundcloud_availability() {
        let mut source = source();
        source.kind = CatalogSourceKind::SoundCloud;
        let storage = primary();
        let streaming = track(source.id, "Streaming");
        let removed = track(source.id, "Removed");
        let inaccessible = track(source.id, "Inaccessible");
        let mut playlist = CanonicalPlaylist::new("Set");
        playlist.replace_tracks([streaming.id, streaming.id, removed.id]);
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![streaming.clone(), removed.clone(), inaccessible.clone()],
            occurrences: vec![
                occurrence(source.id, streaming.id, SourceAvailability::StreamingOnly),
                occurrence(source.id, removed.id, SourceAvailability::Removed),
                occurrence(source.id, inaccessible.id, SourceAvailability::Inaccessible),
            ],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![])],
            playlists: vec![playlist],
            ..Default::default()
        });
        let status_for = |track_id| {
            report
                .track_outcomes
                .iter()
                .find(|outcome| outcome.track_id == track_id)
                .map(|outcome| outcome.status)
        };
        assert_eq!(
            status_for(streaming.id),
            Some(TrackStorageStatus::NotAvailableForArchival)
        );
        assert_eq!(status_for(removed.id), Some(TrackStorageStatus::Removed));
        assert_eq!(
            status_for(inaccessible.id),
            Some(TrackStorageStatus::Inaccessible)
        );
        assert_eq!(report.playlist_outcomes[0].unique_track_ids.len(), 2);
        assert_eq!(report.playlist_outcomes[0].summary.total_tracks, 2);
    }

    #[test]
    fn exact_hash_is_not_suppressed_by_a_provider_version_change() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original = original(track.id);
        let mut changed = object(storage.id, "one", "renamed.mp3");
        changed.provider_version = Some("v2".into());
        let location = AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.id(),
            asset_type: rec_core::AssetType::Original,
            storage_id: storage.id,
            object_id: changed.object_id.clone(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: original.content_hash.clone(),
            observed_hash: original.content_hash.clone(),
            provider_version: Some("v1".into()),
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        };
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            locations: vec![location],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![changed])],
            ..Default::default()
        });
        assert_eq!(
            report.asset_outcomes[0].status,
            AssetStorageStatus::Verified
        );
        assert_eq!(
            report.verified_matches[0].evidence.method,
            MatchMethod::ExactContentHash
        );
        assert!(!report
            .results
            .iter()
            .any(|result| result.outcome == ReconciliationOutcome::ChangedStorageObject));
    }

    #[test]
    fn missing_comparison_evidence_does_not_verify_a_persisted_association() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "unhashed.mp3");
        let mut observed = object(storage.id, "one", "unhashed.mp3");
        observed.hashes.clear();
        observed.provider_version = None;
        let location = AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.id(),
            asset_type: rec_core::AssetType::Original,
            storage_id: storage.id,
            object_id: observed.object_id.clone(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: None,
            observed_hash: None,
            provider_version: Some("v1".into()),
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        };
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            locations: vec![location],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![observed])],
            ..Default::default()
        });
        assert_eq!(
            report.asset_outcomes[0].status,
            AssetStorageStatus::ChangedPendingReview
        );
        assert!(report.results.iter().any(|result| result
            .actions
            .iter()
            .any(|action| matches!(action, ReconciliationAction::ApproveExternalChange { .. }))));
    }

    #[test]
    fn recorded_transfer_and_verified_standard_identifier_are_strong_evidence() {
        for method in [
            MatchMethod::RecordedTransfer,
            MatchMethod::VerifiedStandardIdentifier,
        ] {
            let source = source();
            let storage = primary();
            let track = track(source.id, "Song");
            let original =
                OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "different.mp3");
            let mut observed = object(storage.id, "one", "unrelated.bin");
            observed.hashes.clear();
            let evidence = match method {
                MatchMethod::RecordedTransfer => ExternalMatchEvidence::recorded_transfer(
                    original.id(),
                    storage.id,
                    "one",
                    "operation-key",
                ),
                MatchMethod::VerifiedStandardIdentifier => {
                    ExternalMatchEvidence::verified_standard_identifier(
                        original.id(),
                        storage.id,
                        "one",
                        "isrc:verified",
                    )
                }
                _ => unreachable!(),
            };
            let report = reconcile(&ReconciliationInput {
                primary_storage_id: Some(storage.id),
                sources: vec![source.clone()],
                tracks: vec![track.clone()],
                occurrences: vec![occurrence(
                    source.id,
                    track.id,
                    SourceAvailability::Obtainable,
                )],
                originals: vec![original],
                storages: vec![storage.clone()],
                inventories: vec![StorageInventory::available(storage.id, vec![observed])],
                external_evidence: vec![evidence],
                ..Default::default()
            });
            assert_eq!(
                report.asset_outcomes[0].status,
                AssetStorageStatus::Verified
            );
            assert_eq!(report.verified_matches[0].evidence.method, method);
            assert!(report.results[0]
                .actions
                .iter()
                .any(|action| matches!(action, ReconciliationAction::Associate { .. })));
        }
    }

    #[test]
    fn unavailable_and_error_inventory_emit_asset_outcomes_and_retry_actions() {
        for (availability, expected) in [
            (
                InventoryAvailability::Unavailable,
                AssetStorageStatus::Unavailable,
            ),
            (InventoryAvailability::Error, AssetStorageStatus::Error),
        ] {
            let source = source();
            let storage = primary();
            let track = track(source.id, "Song");
            let report = reconcile(&ReconciliationInput {
                primary_storage_id: Some(storage.id),
                sources: vec![source.clone()],
                tracks: vec![track.clone()],
                occurrences: vec![occurrence(
                    source.id,
                    track.id,
                    SourceAvailability::Obtainable,
                )],
                originals: vec![original(track.id)],
                storages: vec![storage.clone()],
                inventories: vec![StorageInventory {
                    storage_id: storage.id,
                    availability,
                    objects: vec![],
                }],
                ..Default::default()
            });
            assert_eq!(report.asset_outcomes[0].status, expected);
            assert!(report.results.iter().any(|result| result
                .actions
                .iter()
                .any(|action| matches!(action, ReconciliationAction::Retry { .. }))));
        }
    }

    #[test]
    fn reconciliation_report_is_deterministic_for_equal_snapshots() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let mut unknown = object(storage.id, "unknown", "Song.mp3");
        unknown.hashes.clear();
        let input = ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![OriginalAsset::new_unverified(
                track.id,
                OriginalAssetRole::Master,
                "other.mp3",
            )],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![unknown])],
            ..Default::default()
        };
        assert_eq!(reconcile(&input), reconcile(&input));
    }

    #[test]
    fn one_track_in_many_playlists_is_counted_once_per_playlist() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let mut first = CanonicalPlaylist::new("First");
        first.replace_tracks([track.id]);
        let mut second = CanonicalPlaylist::new("Second");
        second.replace_tracks([track.id]);
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::StreamingOnly,
            )],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![])],
            playlists: vec![second, first],
            ..Default::default()
        });
        assert_eq!(report.track_outcomes.len(), 1);
        assert_eq!(report.playlist_outcomes.len(), 2);
        assert!(report
            .playlist_outcomes
            .iter()
            .all(|outcome| outcome.summary.total_tracks == 1));
    }

    #[test]
    fn verified_managed_original_offers_copy_to_primary() {
        let source = source();
        let primary_storage = primary();
        let mut secondary = primary();
        secondary.role = rec_core::StorageRole::Unassigned;
        let track = track(source.id, "Song");
        let original = original(track.id);
        let location = AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.id(),
            asset_type: rec_core::AssetType::Original,
            storage_id: secondary.id,
            object_id: "secondary-object".into(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: original.content_hash.clone(),
            observed_hash: original.content_hash.clone(),
            provider_version: Some("v1".into()),
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        };
        let report = reconcile(&ReconciliationInput {
            primary_storage_id: Some(primary_storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            locations: vec![location],
            storages: vec![primary_storage.clone(), secondary],
            inventories: vec![StorageInventory::available(primary_storage.id, vec![])],
            ..Default::default()
        });
        assert!(report.results.iter().any(|result| result
            .actions
            .iter()
            .any(|action| matches!(action, ReconciliationAction::Copy { .. }))));
    }

    #[test]
    fn unavailable_primary_is_not_an_empty_primary_and_unknown_files_are_reviewed() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original = original(track.id);
        let unavailable = ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original.clone()],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory {
                storage_id: storage.id,
                availability: InventoryAvailability::Unavailable,
                objects: vec![],
            }],
            ..Default::default()
        };
        assert_eq!(
            reconcile(&unavailable).track_outcomes[0].status,
            TrackStorageStatus::StorageUnavailable
        );

        let empty = ReconciliationInput {
            inventories: vec![StorageInventory::available(storage.id, vec![])],
            ..unavailable.clone()
        };
        assert_eq!(
            reconcile(&empty).track_outcomes[0].status,
            TrackStorageStatus::MissingObtainable
        );

        let mut unknown = object(storage.id, "unknown", "unknown.mp3");
        unknown.hashes.clear();
        let with_unknown = ReconciliationInput {
            inventories: vec![StorageInventory::available(storage.id, vec![unknown])],
            ..unavailable
        };
        let report = reconcile(&with_unknown);
        let unknown_result = report
            .results
            .iter()
            .find(|result| result.outcome == ReconciliationOutcome::UntrackedStorageObject)
            .expect("unknown object finding");
        assert!(unknown_result
            .actions
            .iter()
            .any(|action| matches!(action, ReconciliationAction::ImportUnknownObject { .. })));
    }

    #[test]
    fn persisted_rejections_and_ignores_do_not_reappear() {
        let source = source();
        let storage = primary();
        let track = track(source.id, "Song");
        let original =
            OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "unrelated.mp3");
        let mut observed = object(storage.id, "one", "Song.mp3");
        observed.hashes.clear();
        let mut input = ReconciliationInput {
            primary_storage_id: Some(storage.id),
            sources: vec![source.clone()],
            tracks: vec![track.clone()],
            occurrences: vec![occurrence(
                source.id,
                track.id,
                SourceAvailability::Obtainable,
            )],
            originals: vec![original],
            storages: vec![storage.clone()],
            inventories: vec![StorageInventory::available(storage.id, vec![observed])],
            ..Default::default()
        };
        let initial = reconcile(&input);
        let candidate_id = initial.candidates[0].id;
        let candidate_result_id = initial
            .results
            .iter()
            .find(|result| {
                matches!(
                    result.outcome,
                    ReconciliationOutcome::Asset(AssetStorageStatus::PossibleMatch)
                )
            })
            .unwrap()
            .id;
        let unknown_result_id = initial
            .results
            .iter()
            .find(|result| result.outcome == ReconciliationOutcome::UntrackedStorageObject)
            .unwrap()
            .id;
        input.resolutions = vec![
            ReconciliationResolution::new(
                candidate_result_id,
                ReconciliationDecision::RejectCandidate,
            )
            .for_candidate(candidate_id),
            ReconciliationResolution::new(unknown_result_id, ReconciliationDecision::IgnoreFinding),
        ];

        let reconciled = reconcile(&input);
        assert!(reconciled
            .candidates
            .iter()
            .all(|candidate| candidate.id != candidate_id));
        assert!(reconciled.results.iter().all(|result| {
            result.id != candidate_result_id
                && result.id != unknown_result_id
                && result
                    .candidates
                    .iter()
                    .all(|candidate| candidate.id != candidate_id)
        }));
    }
}
