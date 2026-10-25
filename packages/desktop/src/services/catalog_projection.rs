//! Provider-neutral projection of catalog state into the Music Management view.
//!
//! Reconciliation and storage health probing belong to `music_storage`. This
//! module turns the catalog and that projection into a [`MusicManagementView`] and
//! knows nothing about any individual provider, so a Catalog Source or Music
//! Storage cannot be dropped or mislabelled because of which slice built the view.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use chrono::{DateTime, Utc};
use music_storage::{catalog_io, MusicStorages, StorageError};
use rec_core::{
    CanonicalPlaylist, CanonicalTrack, CatalogSource, CatalogSourceKind, HealthStatus,
    MatchCandidate, MetadataField, MetadataValue, MusicStorage, MusicStorageKind,
    MusicStorageRepository, ReconciliationOutcome, RepositoryError, StorageObject, StorageRole,
    TrackStorageStatus,
};
use rec_persistence::TursoCatalog;
use rec_sync::ReconciliationReport;
use ui::{
    CompletenessView, ExternalChangeView, FailedReconciliationView, HealthTone, MatchCandidateView,
    MusicManagementView, PlaylistCompletenessView, ProviderChoice, SourceView, StorageRoleView,
    StorageView, TrackStorageStatusView, TrackStorageView, UnknownPrimaryFileView,
};
use uuid::Uuid;

/// Storage configuration key holding a user-facing folder name.
const CONFIG_FOLDER_DISPLAY_NAME: &str = "folder_display_name";
/// Local source configuration key holding the scanned path.
const CONFIG_PATH: &str = "path";

/// Everything the Music Management view is projected from.
///
/// Health is supplied per Catalog Source and per Music Storage because probing a
/// provider is I/O the caller already performed. A missing entry is not an error:
/// it falls back to whether the record is enabled.
pub struct CatalogProjection<'a> {
    /// Reconciliation report the completeness and match sections are derived from.
    pub report: &'a ReconciliationReport,
    /// Every configured Catalog Source, of any kind.
    pub sources: &'a [CatalogSource],
    /// Every configured Music Storage, of any kind.
    pub storages: &'a [MusicStorage],
    /// Canonical Tracks used to resolve display titles.
    pub tracks: &'a [CanonicalTrack],
    /// Canonical Playlists used to resolve playlist names.
    pub playlists: &'a [CanonicalPlaylist],
    /// Storage Objects observed across every storage in `storages`.
    pub objects: &'a [StorageObject],
    /// Storage currently holding the Primary role, when one is active.
    pub active_primary_id: Option<Uuid>,
    /// Probed health per Catalog Source id.
    pub source_health: &'a BTreeMap<Uuid, HealthTone>,
    /// Probed health per Music Storage id.
    pub storage_health: &'a BTreeMap<Uuid, HealthTone>,
}

impl CatalogProjection<'_> {
    /// Build the complete Music Management view.
    pub fn project(&self) -> MusicManagementView {
        let track_by_id: HashMap<_, _> =
            self.tracks.iter().map(|track| (track.id, track)).collect();
        let object_by_key: HashMap<_, _> = self
            .objects
            .iter()
            .map(|object| ((object.storage_id, object.object_id.clone()), object))
            .collect();
        let status_by_track: HashMap<_, _> = self
            .report
            .track_outcomes
            .iter()
            .map(|outcome| (outcome.track_id, outcome.status))
            .collect();

        let sources = self
            .sources
            .iter()
            .map(|source| self.source_view(source))
            .collect();

        let mut primary_storage = None;
        let mut backup_storages = Vec::new();
        let mut unassigned_storages = Vec::new();
        for storage in self.storages {
            let view = self.storage_view(storage);
            match storage.role {
                StorageRole::Primary
                    if self.active_primary_id.is_some_and(|id| id == storage.id) =>
                {
                    primary_storage = Some(view)
                }
                StorageRole::Backup => backup_storages.push(view),
                _ => unassigned_storages.push(view),
            }
        }

        let tracks = self
            .report
            .track_outcomes
            .iter()
            .map(|outcome| {
                let track = track_by_id.get(&outcome.track_id);
                let status = status_view(outcome.status);
                TrackStorageView {
                    id: outcome.track_id.to_string(),
                    title: track
                        .and_then(|track| preferred_text(track, MetadataField::Title))
                        .unwrap_or_else(|| "Unknown track".into()),
                    artist: track
                        .and_then(|track| preferred_text(track, MetadataField::Artist))
                        .unwrap_or_else(|| "Unknown artist".into()),
                    status,
                    badges: vec![format!("{status:?}")],
                    pinned_offline: false,
                }
            })
            .collect();

        let possible_matches = self
            .report
            .candidates
            .iter()
            .map(|candidate| candidate_view(candidate, &track_by_id, &object_by_key))
            .collect();

        let unknown_primary_files = self
            .report
            .results
            .iter()
            .filter(|result| result.outcome == ReconciliationOutcome::UntrackedStorageObject)
            .map(|result| {
                let object_id = result.storage_object_id.clone().unwrap_or_default();
                let object = result
                    .storage_id
                    .and_then(|storage_id| object_by_key.get(&(storage_id, object_id.clone())));
                UnknownPrimaryFileView {
                    id: result.id.to_string(),
                    filename: object
                        .map(|value| value.name.clone())
                        .unwrap_or_else(|| object_id.clone()),
                    location: object
                        .and_then(|value| value.display_path.clone())
                        .unwrap_or(object_id),
                    discovered_at: "Current inventory".into(),
                }
            })
            .collect();

        let external_changes = self
            .report
            .results
            .iter()
            .filter(|result| result.outcome == ReconciliationOutcome::ChangedStorageObject)
            .map(|result| ExternalChangeView {
                result_id: result.id.to_string(),
                name: result
                    .storage_object_id
                    .clone()
                    .unwrap_or_else(|| "Changed object".into()),
                detail: "The primary object changed outside rec_bag.".into(),
            })
            .collect();

        let failed_reconciliations = self
            .report
            .results
            .iter()
            .filter(|result| {
                matches!(
                    result.outcome,
                    ReconciliationOutcome::Track(TrackStorageStatus::Error)
                )
            })
            .map(|result| FailedReconciliationView {
                result_id: result.id.to_string(),
                name: result
                    .track_id
                    .and_then(|id| track_by_id.get(&id))
                    .and_then(|track| preferred_text(track, MetadataField::Title))
                    .unwrap_or_else(|| "Reconciliation failure".into()),
                error: "Storage reconciliation failed".into(),
            })
            .collect();

        let playlists = self
            .report
            .playlist_outcomes
            .iter()
            .map(|outcome| {
                let playlist = self
                    .playlists
                    .iter()
                    .find(|playlist| playlist.id == outcome.playlist_id);
                let needs_review = outcome
                    .unique_track_ids
                    .iter()
                    .filter(|id| {
                        status_by_track.get(id).is_some_and(|status| {
                            matches!(
                                status,
                                TrackStorageStatus::PossibleMatch
                                    | TrackStorageStatus::AmbiguousMatch
                            )
                        })
                    })
                    .count() as u32;
                PlaylistCompletenessView {
                    id: outcome.playlist_id.to_string(),
                    name: playlist
                        .map(|value| value.name.clone())
                        .unwrap_or_else(|| "Playlist".into()),
                    archived_tracks: outcome.summary.archived_tracks as u32,
                    total_tracks: outcome.summary.archivable_tracks as u32,
                    needs_review,
                }
            })
            .collect();

        let count = |status: TrackStorageStatus| {
            self.report
                .track_outcomes
                .iter()
                .filter(|outcome| outcome.status == status)
                .count() as u32
        };
        let unknown_primary_file_count = self
            .report
            .results
            .iter()
            .filter(|result| result.outcome == ReconciliationOutcome::UntrackedStorageObject)
            .count() as u32;

        MusicManagementView {
            sources,
            primary_storage,
            backup_storages,
            unassigned_storages,
            completeness: CompletenessView {
                canonical_tracks: self.report.summary.total_tracks as u32,
                catalog_covered: self.report.summary.covered_tracks as u32,
                verified_assets: self.report.summary.verified_assets as u32,
                missing_obtainable: count(TrackStorageStatus::MissingObtainable),
                streaming_only: count(TrackStorageStatus::NotAvailableForArchival),
                restricted: count(TrackStorageStatus::Blocked)
                    + count(TrackStorageStatus::Inaccessible),
                possible_matches: count(TrackStorageStatus::PossibleMatch),
                ambiguous_matches: count(TrackStorageStatus::AmbiguousMatch),
                unknown_primary_files: unknown_primary_file_count,
                backup_gaps: 0,
                provider_errors: count(TrackStorageStatus::Error)
                    + count(TrackStorageStatus::StorageUnavailable),
                playlists,
            },
            tracks,
            possible_matches,
            unknown_primary_files,
            external_changes,
            failed_reconciliations,
            cleanup_candidates: Vec::new(),
        }
    }

    fn source_view(&self, source: &CatalogSource) -> SourceView {
        SourceView {
            id: source.id.to_string(),
            provider: source_provider(&source.kind),
            name: source.name.clone(),
            account_identity: None,
            detail: source_detail(source),
            health: self
                .source_health
                .get(&source.id)
                .copied()
                .unwrap_or(enabled_tone(source.enabled)),
            last_sync: source.last_synced_at.map(format_time),
            next_sync: None,
            progress: None,
            warning: None,
            error: None,
            failed_sync_run_id: None,
            collections: Vec::new(),
            connected: source.enabled,
        }
    }

    fn storage_view(&self, storage: &MusicStorage) -> StorageView {
        let is_backup = storage.role == StorageRole::Backup;
        StorageView {
            id: storage.id.to_string(),
            provider: storage_provider(&storage.kind),
            name: storage.name.clone(),
            location: storage
                .config
                .get(CONFIG_FOLDER_DISPLAY_NAME)
                .cloned()
                .or_else(|| storage.root_identifier.clone())
                .unwrap_or_default(),
            health: self
                .storage_health
                .get(&storage.id)
                .copied()
                .unwrap_or(enabled_tone(storage.enabled)),
            role: match storage.role {
                StorageRole::Primary => StorageRoleView::Primary,
                StorageRole::Backup => StorageRoleView::Backup,
                StorageRole::Unassigned => StorageRoleView::Unassigned,
            },
            last_inventory: storage.last_inventory_at.map(format_time),
            last_verified: None,
            pending_jobs: 0,
            failed_jobs: 0,
            replication_mode: is_backup.then(|| format!("{:?}", storage.replication_mode)),
            retention_summary: is_backup.then(|| {
                format!(
                    "{} days / {} versions",
                    storage.retention_policy.retention_days,
                    storage.retention_policy.minimum_versions
                )
            }),
        }
    }
}

/// Provider a Catalog Source is displayed as.
///
/// A custom source has no first-class provider, so it is shown as a local folder
/// rather than being hidden from the dashboard.
pub fn source_provider(kind: &CatalogSourceKind) -> ProviderChoice {
    match kind {
        CatalogSourceKind::LocalFolder => ProviderChoice::LocalFolder,
        CatalogSourceKind::SoundCloud => ProviderChoice::SoundCloud,
        CatalogSourceKind::Custom(_) => ProviderChoice::LocalFolder,
    }
}

/// Provider a Music Storage is displayed as.
///
/// A storage of an unrecognized kind is shown as a local folder rather than being
/// hidden, because a hidden storage cannot be disconnected or reassigned.
pub fn storage_provider(kind: &MusicStorageKind) -> ProviderChoice {
    match kind {
        MusicStorageKind::LocalDirectory => ProviderChoice::LocalFolder,
        MusicStorageKind::GoogleDrive => ProviderChoice::GoogleDrive,
        MusicStorageKind::Other(_) => ProviderChoice::LocalFolder,
    }
}

/// Map a probed provider health status onto its display tone.
pub fn health_tone(status: HealthStatus) -> HealthTone {
    match status {
        HealthStatus::Healthy => HealthTone::Healthy,
        HealthStatus::Degraded => HealthTone::Warning,
        HealthStatus::Unavailable => HealthTone::Offline,
        HealthStatus::AuthenticationRequired => HealthTone::Error,
    }
}

/// Display tone for a record that was not probed.
pub fn enabled_tone(enabled: bool) -> HealthTone {
    if enabled {
        HealthTone::Healthy
    } else {
        HealthTone::Offline
    }
}

/// Preferred text value for a Canonical Track metadata field.
pub fn preferred_text(track: &CanonicalTrack, field: MetadataField) -> Option<String> {
    match &track.metadata.preferred(&field)?.value {
        MetadataValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}

/// Render a timestamp for display.
pub fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}

/// Map a reconciled track status onto its display status.
pub fn status_view(status: TrackStorageStatus) -> TrackStorageStatusView {
    match status {
        TrackStorageStatus::StoredAndVerified => TrackStorageStatusView::StoredAndVerified,
        TrackStorageStatus::MissingObtainable => TrackStorageStatusView::MissingObtainable,
        TrackStorageStatus::PossibleMatch => TrackStorageStatusView::PossibleMatch,
        TrackStorageStatus::AmbiguousMatch => TrackStorageStatusView::AmbiguousMatch,
        TrackStorageStatus::NotAvailableForArchival => {
            TrackStorageStatusView::NotAvailableForArchival
        }
        TrackStorageStatus::PreviewOnly => TrackStorageStatusView::PreviewOnly,
        TrackStorageStatus::Blocked => TrackStorageStatusView::Blocked,
        TrackStorageStatus::Removed => TrackStorageStatusView::Removed,
        TrackStorageStatus::Inaccessible => TrackStorageStatusView::Inaccessible,
        TrackStorageStatus::StorageUnavailable => TrackStorageStatusView::StorageUnavailable,
        TrackStorageStatus::Error => TrackStorageStatusView::Error,
        TrackStorageStatus::Ignored => TrackStorageStatusView::Ignored,
    }
}

fn source_detail(source: &CatalogSource) -> String {
    match &source.kind {
        CatalogSourceKind::LocalFolder => source.config.get(CONFIG_PATH).cloned(),
        _ => source
            .config
            .get(CONFIG_PATH)
            .or_else(|| source.config.values().next())
            .cloned(),
    }
    .unwrap_or_default()
}

fn candidate_view(
    candidate: &MatchCandidate,
    tracks: &HashMap<Uuid, &CanonicalTrack>,
    objects: &HashMap<(Uuid, String), &StorageObject>,
) -> MatchCandidateView {
    let track_name = tracks
        .get(&candidate.track_id)
        .and_then(|track| preferred_text(track, MetadataField::Title))
        .unwrap_or_else(|| "Unknown track".into());
    let object = objects.get(&(candidate.storage_id, candidate.storage_object_id.clone()));
    MatchCandidateView {
        candidate_id: candidate.id.to_string(),
        track_id: candidate.track_id.to_string(),
        track_name,
        candidate_name: object
            .map(|value| value.name.clone())
            .unwrap_or_else(|| candidate.storage_object_id.clone()),
        evidence: candidate
            .evidence
            .iter()
            .map(|evidence| format!("{:?}", evidence.method))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Projects the whole catalog into the dashboard view.
///
/// `music_storage` reconciles against the active primary and probes every open
/// store; this turns that projection into the provider-neutral view, so every
/// Catalog Source and Music Storage is included whichever slice asked.
pub async fn project_dashboard(
    catalog: &TursoCatalog,
    storages: &MusicStorages,
) -> Result<MusicManagementView, StorageError> {
    let projection = storages.project().await?;
    let storage_health = projection
        .storages
        .iter()
        .filter_map(|view| {
            view.health
                .as_ref()
                .map(|health| (view.storage.id, health.status))
        })
        .collect();
    Ok(CatalogViewBuilder {
        catalog,
        report: &projection.report,
        storage_health: &storage_health,
    }
    .build()
    .await?)
}

/// Gathers catalog state and projects it into the Music Management view.
///
/// The Reconciliation report and storage health are supplied by the caller;
/// everything else is read here so that one view is built the same way
/// regardless of which slice handled the request.
pub struct CatalogViewBuilder<'a> {
    /// Catalog the view is read from.
    pub catalog: &'a TursoCatalog,
    /// Report the completeness and match sections are derived from.
    pub report: &'a ReconciliationReport,
    /// Probed health per Music Storage id; storages without an entry fall
    /// back to whether they are enabled.
    pub storage_health: &'a BTreeMap<Uuid, HealthStatus>,
}

impl CatalogViewBuilder<'_> {
    /// Read the catalog and project the view.
    pub async fn build(&self) -> Result<MusicManagementView, RepositoryError> {
        let sources = catalog_io::sources(self.catalog).await?;
        let storages = MusicStorageRepository::list(self.catalog).await?;
        let tracks = catalog_io::tracks(self.catalog).await?;
        let playlists = catalog_io::playlists(self.catalog).await?;
        let mut objects = Vec::new();
        for storage in &storages {
            objects.extend(catalog_io::storage_objects(self.catalog, storage.id).await?);
        }
        let active_primary_id = MusicStorageRepository::get_active_primary(self.catalog)
            .await?
            .map(|storage| storage.id);

        let mut source_health = BTreeMap::new();
        for source in &sources {
            if let Some(tone) = source_tone(source) {
                source_health.insert(source.id, tone);
            }
        }
        let storage_health = self
            .storage_health
            .iter()
            .map(|(id, status)| (*id, health_tone(*status)))
            .collect();

        Ok(CatalogProjection {
            report: self.report,
            sources: &sources,
            storages: &storages,
            tracks: &tracks,
            playlists: &playlists,
            objects: &objects,
            active_primary_id,
            source_health: &source_health,
            storage_health: &storage_health,
        }
        .project())
    }
}

/// Health of a Catalog Source, where it can be determined without a provider call.
///
/// A local folder is checked on the filesystem. Any other kind returns `None` and
/// falls back to whether the source is enabled.
fn source_tone(source: &CatalogSource) -> Option<HealthTone> {
    if source.kind != CatalogSourceKind::LocalFolder {
        return None;
    }
    Some(match source.config.get(CONFIG_PATH) {
        Some(path) if Path::new(path).is_dir() => HealthTone::Healthy,
        Some(_) => HealthTone::Offline,
        None => HealthTone::Error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{
        MusicStorageKind, ReplicationMode, RetentionPolicy, StorageCapabilities, StorageRole,
    };
    use rec_sync::ReconciliationReport;

    fn empty_report() -> ReconciliationReport {
        ReconciliationReport {
            primary_storage_id: None,
            track_outcomes: Vec::new(),
            asset_outcomes: Vec::new(),
            playlist_outcomes: Vec::new(),
            candidates: Vec::new(),
            verified_matches: Vec::new(),
            results: Vec::new(),
            summary: Default::default(),
        }
    }

    fn storage(name: &str, kind: MusicStorageKind, role: StorageRole) -> MusicStorage {
        MusicStorage::new(name, kind, role, StorageCapabilities::default())
            .with_root_identifier("root")
    }

    struct Fixture {
        report: ReconciliationReport,
        sources: Vec<CatalogSource>,
        storages: Vec<MusicStorage>,
        source_health: BTreeMap<Uuid, HealthTone>,
        storage_health: BTreeMap<Uuid, HealthTone>,
        active_primary_id: Option<Uuid>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                report: empty_report(),
                sources: Vec::new(),
                storages: Vec::new(),
                source_health: BTreeMap::new(),
                storage_health: BTreeMap::new(),
                active_primary_id: None,
            }
        }

        fn project(&self) -> MusicManagementView {
            CatalogProjection {
                report: &self.report,
                sources: &self.sources,
                storages: &self.storages,
                tracks: &[],
                playlists: &[],
                objects: &[],
                active_primary_id: self.active_primary_id,
                source_health: &self.source_health,
                storage_health: &self.storage_health,
            }
            .project()
        }
    }

    #[test]
    fn every_storage_kind_reaches_the_view() {
        let mut fixture = Fixture::new();
        let drive = storage("Drive", MusicStorageKind::GoogleDrive, StorageRole::Primary);
        let local = storage(
            "Local backup",
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
        );
        let other = storage(
            "Future",
            MusicStorageKind::Other("s3".into()),
            StorageRole::Unassigned,
        );
        fixture.active_primary_id = Some(drive.id);
        fixture.storages = vec![drive.clone(), local, other];

        let view = fixture.project();

        // A Google Drive primary used to be filtered out entirely, which left the
        // dashboard unable to disconnect or reassign it.
        assert_eq!(
            view.primary_storage.as_ref().map(|value| value.id.clone()),
            Some(drive.id.to_string())
        );
        assert_eq!(
            view.primary_storage.unwrap().provider,
            ProviderChoice::GoogleDrive
        );
        assert_eq!(view.backup_storages.len(), 1);
        assert_eq!(view.unassigned_storages.len(), 1);
    }

    #[test]
    fn a_primary_role_that_is_not_active_is_not_shown_as_primary() {
        let mut fixture = Fixture::new();
        let stale = storage(
            "Former primary",
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
        );
        fixture.storages = vec![stale];
        fixture.active_primary_id = Some(Uuid::new_v4());

        let view = fixture.project();

        assert!(view.primary_storage.is_none());
        assert_eq!(view.unassigned_storages.len(), 1);
    }

    #[test]
    fn a_source_is_labelled_with_its_own_provider() {
        let mut fixture = Fixture::new();
        fixture.sources = vec![
            CatalogSource::local_folder("Folder", "/music"),
            CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud),
            CatalogSource::new("Future", CatalogSourceKind::Custom("bandcamp".into())),
        ];

        let view = fixture.project();

        // Every source used to be stamped as a local folder regardless of its kind.
        let providers: Vec<_> = view.sources.iter().map(|value| value.provider).collect();
        assert_eq!(
            providers,
            vec![
                ProviderChoice::LocalFolder,
                ProviderChoice::SoundCloud,
                ProviderChoice::LocalFolder,
            ]
        );
        assert_eq!(view.sources[0].detail, "/music");
    }

    #[test]
    fn a_local_source_detail_shows_its_path() {
        let mut fixture = Fixture::new();
        fixture.sources = vec![CatalogSource::local_folder("Folder", "/music/library")];

        assert_eq!(fixture.project().sources[0].detail, "/music/library");
    }

    #[test]
    fn probed_health_wins_over_the_enabled_fallback() {
        let mut fixture = Fixture::new();
        let drive = storage(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
        );
        let unprobed = storage(
            "Unprobed",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
        );
        fixture.storage_health.insert(drive.id, HealthTone::Warning);
        fixture.storages = vec![drive, unprobed];

        let view = fixture.project();
        let tones: Vec<_> = view
            .unassigned_storages
            .iter()
            .map(|value| value.health)
            .collect();

        assert_eq!(tones, vec![HealthTone::Warning, HealthTone::Healthy]);
    }

    #[test]
    fn a_disabled_unprobed_source_reads_as_offline() {
        let mut fixture = Fixture::new();
        let mut source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        source.enabled = false;
        fixture.sources = vec![source];

        let view = fixture.project();

        assert_eq!(view.sources[0].health, HealthTone::Offline);
        assert!(!view.sources[0].connected);
    }

    #[test]
    fn only_a_backup_reports_replication_and_retention() {
        let mut fixture = Fixture::new();
        let mut backup = storage(
            "Backup",
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
        );
        backup.replication_mode = ReplicationMode::default();
        backup.retention_policy = RetentionPolicy::default();
        let unassigned = storage(
            "Spare",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
        );
        fixture.storages = vec![backup, unassigned];

        let view = fixture.project();

        // A local backup previously reported neither, because only the Drive slice
        // filled these in.
        assert!(view.backup_storages[0].replication_mode.is_some());
        assert!(view.backup_storages[0].retention_summary.is_some());
        assert!(view.unassigned_storages[0].replication_mode.is_none());
        assert!(view.unassigned_storages[0].retention_summary.is_none());
    }

    #[test]
    fn authentication_required_is_an_error_not_a_warning() {
        assert_eq!(
            health_tone(HealthStatus::AuthenticationRequired),
            HealthTone::Error
        );
        assert_eq!(health_tone(HealthStatus::Degraded), HealthTone::Warning);
        assert_eq!(health_tone(HealthStatus::Unavailable), HealthTone::Offline);
    }
}
