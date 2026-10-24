//! Provider-independent music source, storage, and reconciliation UI.
//!
//! The types in this module are intentionally view models rather than provider
//! implementations. Platform composition supplies data and handles emitted
//! events; [`MusicManagementMock`] is available for the Stage 2 UI preview.

use dioxus::prelude::*;

const MUSIC_MANAGEMENT_CSS: Asset = asset!("/assets/components/music-management.css");

/// A provider choice presented while creating a catalog source or storage target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderChoice {
    LocalFolder,
    SoundCloud,
    GoogleDrive,
}

impl ProviderChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::LocalFolder => "Local folder",
            Self::SoundCloud => "SoundCloud",
            Self::GoogleDrive => "Google Drive",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::LocalFolder => "📁",
            Self::SoundCloud => "☁",
            Self::GoogleDrive => "△",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::LocalFolder => "Scan audio files that are already available on this device.",
            Self::SoundCloud => "Sync uploads, likes, and playlists into the catalog.",
            Self::GoogleDrive => "Select a Drive folder for primary or backup music storage.",
        }
    }
}

/// Visual status used for source and storage health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthTone {
    Healthy,
    Working,
    Warning,
    Error,
    Offline,
}

impl HealthTone {
    fn label(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Working => "In progress",
            Self::Warning => "Needs attention",
            Self::Error => "Error",
            Self::Offline => "Unavailable",
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Working => "working",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Offline => "offline",
        }
    }
}

/// Progress details for a currently active or recently completed job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobProgressView {
    pub label: String,
    pub processed: u32,
    /// The expected item count, or `None` when the provider cannot determine it yet.
    pub total: Option<u32>,
    pub current_item: Option<String>,
}

impl JobProgressView {
    fn percent(&self) -> Option<u32> {
        self.total.map(|total| {
            if total == 0 {
                0
            } else {
                (self.processed.saturating_mul(100) / total).min(100)
            }
        })
    }
}

/// A source collection such as SoundCloud uploads or liked playlists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceCollectionView {
    pub name: String,
    pub item_count: u32,
    pub read_only: bool,
}

/// UI state for a catalog source. It intentionally has no credential fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceView {
    pub id: String,
    pub provider: ProviderChoice,
    pub name: String,
    pub account_identity: Option<String>,
    pub detail: String,
    pub health: HealthTone,
    pub last_sync: Option<String>,
    pub next_sync: Option<String>,
    pub progress: Option<JobProgressView>,
    pub warning: Option<String>,
    pub error: Option<String>,
    /// Stable ID of the failed run that can be retried, when applicable.
    pub failed_sync_run_id: Option<String>,
    pub collections: Vec<SourceCollectionView>,
    pub connected: bool,
}

/// UI state for primary or backup music storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageView {
    pub id: String,
    pub provider: ProviderChoice,
    pub name: String,
    pub location: String,
    pub health: HealthTone,
    pub role: StorageRoleView,
    pub last_inventory: Option<String>,
    pub last_verified: Option<String>,
    pub pending_jobs: u32,
    pub failed_jobs: u32,
    pub replication_mode: Option<String>,
    pub retention_summary: Option<String>,
}

/// Responsibility displayed for a configured music storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageRoleView {
    Unassigned,
    Primary,
    Backup,
}

impl StorageRoleView {
    fn label(self) -> &'static str {
        match self {
            Self::Unassigned => "Unassigned",
            Self::Primary => "Primary",
            Self::Backup => "Backup",
        }
    }
}

/// A playlist's coverage in the primary archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistCompletenessView {
    pub id: String,
    pub name: String,
    pub archived_tracks: u32,
    pub total_tracks: u32,
    pub needs_review: u32,
}

impl PlaylistCompletenessView {
    fn percent(&self) -> u32 {
        if self.total_tracks == 0 {
            100
        } else {
            (self.archived_tracks.saturating_mul(100) / self.total_tracks).min(100)
        }
    }
}

/// Aggregate completeness and reconciliation state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletenessView {
    pub canonical_tracks: u32,
    pub catalog_covered: u32,
    pub verified_assets: u32,
    pub missing_obtainable: u32,
    pub streaming_only: u32,
    pub restricted: u32,
    pub possible_matches: u32,
    pub ambiguous_matches: u32,
    pub unknown_primary_files: u32,
    pub backup_gaps: u32,
    pub provider_errors: u32,
    pub playlists: Vec<PlaylistCompletenessView>,
}

/// Filters for the compact track storage review list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackFilter {
    #[default]
    All,
    Missing,
    NeedsReview,
    PinnedOffline,
}

impl TrackFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Missing => "Missing",
            Self::NeedsReview => "Needs review",
            Self::PinnedOffline => "Offline pins",
        }
    }
}

/// Provider-neutral, machine-readable storage state for a canonical track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackStorageStatusView {
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

impl TrackStorageStatusView {
    fn is_missing(self) -> bool {
        matches!(self, Self::MissingObtainable)
    }

    fn needs_review(self) -> bool {
        matches!(self, Self::PossibleMatch | Self::AmbiguousMatch)
    }
}

/// Provider-neutral per-track storage state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackStorageView {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub status: TrackStorageStatusView,
    /// Supplemental presentation labels; logic must use [`TrackStorageStatusView`].
    pub badges: Vec<String>,
    pub pinned_offline: bool,
}

/// A weak storage association that requires user confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchCandidateView {
    pub candidate_id: String,
    pub track_id: String,
    pub track_name: String,
    pub candidate_name: String,
    pub evidence: String,
}

/// An audio file found in primary storage with no known catalog association.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownPrimaryFileView {
    pub id: String,
    pub filename: String,
    pub location: String,
    pub discovered_at: String,
}

/// A retained backup item that is eligible for explicit cleanup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupItemView {
    pub id: String,
    pub name: String,
    pub age: String,
    pub reason: String,
}

/// A provider object changed outside rec_bag and requires explicit approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalChangeView {
    pub result_id: String,
    pub name: String,
    pub detail: String,
}

/// A failed reconciliation finding that can be retried by stable result ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedReconciliationView {
    pub result_id: String,
    pub name: String,
    pub error: String,
}

/// Provider-neutral values submitted by the primary storage editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryConfigurationView {
    pub provider: ProviderChoice,
    pub name: String,
    pub location: String,
    pub organize_by_artist: bool,
    pub verify_after_copy: bool,
}

/// Public desktop OAuth values submitted for the SoundCloud connection.
///
/// The broker secret is deliberately absent and must never be collected or stored by this UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundCloudConfigurationView {
    pub client_id: String,
    pub redirect_uri: String,
}

/// Mutable metadata and copy policy for an already configured primary.
///
/// Provider and location are intentionally absent: changing either requires a migration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryMetadataUpdateView {
    pub storage_id: String,
    pub name: String,
    pub organize_by_artist: bool,
    pub verify_after_copy: bool,
}

/// Replication and retention values submitted by a backup policy editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupPolicyView {
    pub target_id: String,
    pub replicate_verified_only: bool,
    pub retention_days: u32,
    pub minimum_versions: u32,
}

/// User choices collected before a primary migration is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryMigrationPlanView {
    pub destination: PrimaryConfigurationView,
    pub verify_checksums: bool,
    pub keep_old_as_backup: bool,
}

/// All inputs needed to render the music management workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MusicManagementView {
    pub sources: Vec<SourceView>,
    pub primary_storage: Option<StorageView>,
    pub backup_storages: Vec<StorageView>,
    pub unassigned_storages: Vec<StorageView>,
    pub completeness: CompletenessView,
    pub tracks: Vec<TrackStorageView>,
    pub possible_matches: Vec<MatchCandidateView>,
    pub unknown_primary_files: Vec<UnknownPrimaryFileView>,
    pub external_changes: Vec<ExternalChangeView>,
    pub failed_reconciliations: Vec<FailedReconciliationView>,
    pub cleanup_candidates: Vec<CleanupItemView>,
}

/// Events emitted by [`MusicManagementDashboard`].
///
/// These events are deliberately provider-independent. Runtime code maps them
/// onto provider-specific flows, jobs, and repositories in a later stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MusicManagementEvent {
    AddSource(ProviderChoice),
    /// Adds a local folder source at a path the user already named, rather than one
    /// chosen through the provider's own folder picker.
    AddLocalFolderSource {
        path: String,
    },
    ConfigureSoundCloud(SoundCloudConfigurationView),
    SyncSource(String),
    RetrySyncRun {
        sync_run_id: String,
    },
    /// Stops future synchronization without deleting cached or catalog data.
    DisconnectSource {
        source_id: String,
    },
    /// Detaches a Google storage configuration without deleting provider files.
    DisconnectStorage {
        storage_id: String,
    },
    /// Revokes rec_bag's provider authorization grant. No credential is carried here.
    RevokeProviderGrant {
        provider: ProviderChoice,
        connection_id: String,
    },
    /// Removes locally cached provider content while retaining catalog records.
    ClearLocalCache {
        source_id: String,
    },
    /// Removes catalog records derived from a provider while retaining provider content.
    ForgetProviderCatalogData {
        source_id: String,
    },
    CreatePrimary(PrimaryConfigurationView),
    UpdatePrimaryMetadata(PrimaryMetadataUpdateView),
    AddBackup(ProviderChoice),
    SaveBackupPolicy(BackupPolicyView),
    PreparePrimaryMigration(PrimaryMigrationPlanView),
    RetryStorage(String),
    SetTrackFilter(TrackFilter),
    ToggleOfflinePin(String, bool),
    CopyToPrimary(String),
    ReviewMatch(String),
    ConfirmMatch(String),
    RejectMatch(String),
    AssociateLocalFile {
        candidate_id: String,
        track_id: String,
        file_path: String,
    },
    AssociateUnknownFile {
        file_id: String,
        track_id: String,
    },
    ApproveExternalChange {
        result_id: String,
    },
    RetryReconciliationResult {
        result_id: String,
    },
    ImportUnknownFile(String),
    IgnoreUnknownFile(String),
    StartCleanupReview,
    ConfirmCleanup(Vec<String>),
}

/// A lifecycle intent used to keep reversible disconnects separate from destructive actions.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LifecycleAction {
    DisconnectSource {
        source_id: String,
    },
    /// Unlike other sources, disconnecting SoundCloud deletes its catalog data (API Terms).
    DisconnectSoundCloud {
        source_id: String,
        source_name: String,
    },
    DisconnectStorage {
        storage_id: String,
    },
    RevokeProviderGrant {
        provider: ProviderChoice,
        connection_id: String,
        subject_name: String,
    },
    ClearLocalCache {
        source_id: String,
        source_name: String,
    },
    ForgetProviderCatalogData {
        source_id: String,
        source_name: String,
    },
}

impl LifecycleAction {
    fn requires_confirmation(&self) -> bool {
        !matches!(
            self,
            Self::DisconnectSource { .. } | Self::DisconnectStorage { .. }
        )
    }

    fn can_confirm(&self, acknowledged: bool) -> bool {
        !self.requires_confirmation() || acknowledged
    }

    fn title(&self) -> &'static str {
        match self {
            Self::DisconnectSource { .. } => "Disconnect source",
            Self::DisconnectSoundCloud { .. } => "Disconnect SoundCloud",
            Self::DisconnectStorage { .. } => "Disconnect storage",
            Self::RevokeProviderGrant { .. } => "Revoke provider access",
            Self::ClearLocalCache { .. } => "Clear local cache",
            Self::ForgetProviderCatalogData { .. } => "Forget provider catalog data",
        }
    }

    fn subject_name(&self) -> Option<&str> {
        match self {
            Self::RevokeProviderGrant { subject_name, .. } => Some(subject_name),
            Self::ClearLocalCache { source_name, .. }
            | Self::ForgetProviderCatalogData { source_name, .. }
            | Self::DisconnectSoundCloud { source_name, .. } => Some(source_name),
            Self::DisconnectSource { .. } | Self::DisconnectStorage { .. } => None,
        }
    }

    fn warning(&self) -> &'static str {
        match self {
            Self::RevokeProviderGrant { .. } => {
                "This revokes rec_bag's authorization at the provider. Synchronization will stop until you grant access again."
            }
            Self::ClearLocalCache { .. } => {
                "Cached files and sync checkpoints on this device are removed; the next sync re-reads everything from the provider. Catalog records and provider content are retained."
            }
            Self::DisconnectSoundCloud { .. } => {
                "rec_bag's SoundCloud access is revoked and SoundCloud tracks, likes, and playlists are removed from this device, as SoundCloud's API terms require. Canonical tracks, your own files, and everything on SoundCloud are kept. Reconnect at any time to import them again."
            }
            Self::ForgetProviderCatalogData { .. } => {
                "Provider-derived tracks, playlists, and associations will be removed from the local catalog. Provider content is not deleted."
            }
            Self::DisconnectSource { .. } | Self::DisconnectStorage { .. } => {
                "Disconnecting is reversible and does not delete catalog data or provider files."
            }
        }
    }

    fn confirmation_label(&self) -> &'static str {
        match self {
            Self::RevokeProviderGrant { .. } => "I understand provider access will be revoked.",
            Self::ClearLocalCache { .. } => "I understand cached content will be removed.",
            Self::DisconnectSoundCloud { .. } => {
                "I understand SoundCloud catalog data will be removed from this device."
            }
            Self::ForgetProviderCatalogData { .. } => {
                "I understand provider-derived catalog data will be removed."
            }
            Self::DisconnectSource { .. } | Self::DisconnectStorage { .. } => {
                "I understand this provider will be disconnected."
            }
        }
    }

    fn into_event(self) -> MusicManagementEvent {
        match self {
            Self::DisconnectSource { source_id } | Self::DisconnectSoundCloud { source_id, .. } => {
                MusicManagementEvent::DisconnectSource { source_id }
            }
            Self::DisconnectStorage { storage_id } => {
                MusicManagementEvent::DisconnectStorage { storage_id }
            }
            Self::RevokeProviderGrant {
                provider,
                connection_id,
                ..
            } => MusicManagementEvent::RevokeProviderGrant {
                provider,
                connection_id,
            },
            Self::ClearLocalCache { source_id, .. } => {
                MusicManagementEvent::ClearLocalCache { source_id }
            }
            Self::ForgetProviderCatalogData { source_id, .. } => {
                MusicManagementEvent::ForgetProviderCatalogData { source_id }
            }
        }
    }
}

/// Props for the provider-independent music management workspace.
#[derive(Props, Clone, PartialEq)]
pub struct MusicManagementDashboardProps {
    pub view: MusicManagementView,
    pub on_event: EventHandler<MusicManagementEvent>,
}

/// Sources, storage, and completeness workspace.
#[component]
pub fn MusicManagementDashboard(props: MusicManagementDashboardProps) -> Element {
    let mut selected_filter = use_signal(TrackFilter::default);
    let mut open_match = use_signal(|| None::<String>);
    let mut open_unknown = use_signal(|| None::<String>);
    let mut cleanup_open = use_signal(|| false);
    let mut soundcloud_configuration_open = use_signal(|| false);
    let mut migration_open = use_signal(|| false);
    let mut primary_editor = use_signal(|| None::<ProviderChoice>);
    let mut backup_policy_editor = use_signal(|| None::<String>);
    let on_event = props.on_event;

    let visible_tracks: Vec<TrackStorageView> = props
        .view
        .tracks
        .iter()
        .filter(|track| track_matches_filter(track, selected_filter()))
        .cloned()
        .collect();

    rsx! {
        document::Stylesheet { href: MUSIC_MANAGEMENT_CSS }
        div { id: "music-management-settings", class: "music-management",
            section { id: "music-sources", class: "mm-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Sources" }
                        p { "Catalog connections are independent from where music files are stored." }
                    }
                    div { class: "mm-provider-chooser",
                        button {
                            class: "mm-button secondary",
                            "aria-haspopup": "dialog",
                            onclick: move |_| soundcloud_configuration_open.set(true),
                            "Configure SoundCloud"
                        }
                        ProviderChooser {
                            providers: vec![ProviderChoice::LocalFolder, ProviderChoice::SoundCloud],
                            action_label: "Add source".to_string(),
                            on_choose: move |provider| on_event.call(MusicManagementEvent::AddSource(provider)),
                        }
                    }
                }
                div { class: "mm-source-grid",
                    for source in props.view.sources.iter() {
                        SourceCard { key: "{source.id}", source: source.clone(), on_event }
                    }
                }
            }

            section { id: "music-storage-settings", class: "mm-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Music storage" }
                        p { "Primary storage is authoritative; backups retain verified copies." }
                    }
                    button {
                        class: "mm-button secondary",
                        disabled: props.view.primary_storage.is_none(),
                        onclick: move |_| migration_open.set(true),
                        "Migrate primary"
                    }
                }
                div { class: "mm-storage-layout",
                    div { class: "mm-storage-column",
                        h3 { "Primary" }
                        if let Some(primary) = props.view.primary_storage.clone() {
                            StorageCard {
                                storage: primary.clone(),
                                on_event,
                                on_edit_policy: move |_| {},
                            }
                            button {
                                class: "mm-link-button",
                                onclick: move |_| primary_editor.set(Some(primary.provider)),
                                "Edit primary configuration"
                            }
                        } else {
                            ProviderChooser {
                                providers: vec![ProviderChoice::LocalFolder, ProviderChoice::GoogleDrive],
                                action_label: "Configure primary".to_string(),
                                on_choose: move |provider| primary_editor.set(Some(provider)),
                            }
                        }
                    }
                    div { class: "mm-storage-column",
                        div { class: "mm-subheading",
                            h3 { "Backups" }
                            div { class: "mm-provider-chooser",
                                for provider in [ProviderChoice::LocalFolder, ProviderChoice::GoogleDrive] {
                                    button {
                                        class: "mm-link-button",
                                        title: provider.description(),
                                        onclick: move |_| on_event.call(MusicManagementEvent::AddBackup(provider)),
                                        "+ {provider.label()}"
                                    }
                                }
                            }
                        }
                        if props.view.backup_storages.is_empty() {
                            p { class: "mm-empty-inline", "No backup targets configured." }
                        } else {
                            for storage in props.view.backup_storages.iter() {
                                StorageCard {
                                    key: "{storage.id}",
                                    storage: storage.clone(),
                                    on_event,
                                    on_edit_policy: move |id| backup_policy_editor.set(Some(id)),
                                }
                            }
                        }
                        if !props.view.unassigned_storages.is_empty() {
                            div { class: "mm-subheading", h3 { "Unassigned" } }
                            p { class: "mm-empty-inline", "Configured storage without an active primary or backup role." }
                            for storage in props.view.unassigned_storages.iter() {
                                StorageCard {
                                    key: "{storage.id}",
                                    storage: storage.clone(),
                                    on_event,
                                    on_edit_policy: move |_| {},
                                }
                            }
                        }
                    }
                }
            }

            section { class: "mm-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Completeness" }
                        p { "Archive coverage reflects verified original assets, not streaming availability." }
                    }
                }
                CompletenessDashboard { completeness: props.view.completeness.clone() }
            }

            section { class: "mm-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Track storage" }
                        p { "Review storage state and pin verified tracks for offline playback." }
                    }
                    div { class: "mm-filter-group", "aria-label": "Track storage filter",
                        for filter in [TrackFilter::All, TrackFilter::Missing, TrackFilter::NeedsReview, TrackFilter::PinnedOffline] {
                            button {
                                class: if selected_filter() == filter { "mm-filter active" } else { "mm-filter" },
                                onclick: move |_| {
                                    selected_filter.set(filter);
                                    on_event.call(MusicManagementEvent::SetTrackFilter(filter));
                                },
                                "{filter.label()}"
                            }
                        }
                    }
                }
                div { class: "mm-track-list",
                    if visible_tracks.is_empty() {
                        p { class: "mm-empty-inline", "No tracks match this filter." }
                    }
                    for track in visible_tracks {
                        TrackStorageRow { key: "{track.id}", track, on_event }
                    }
                }
            }

            section { class: "mm-section mm-review-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Review queues" }
                        p { "Ambiguous associations, new primary files, and cleanup always require an explicit decision." }
                    }
                    button {
                        class: "mm-button secondary",
                        onclick: move |_| {
                            cleanup_open.set(true);
                            on_event.call(MusicManagementEvent::StartCleanupReview);
                        },
                        "Review cleanup ({props.view.cleanup_candidates.len()})"
                    }
                }
                div { class: "mm-review-grid",
                    ReviewQueue {
                        title: "Possible matches".to_string(),
                        count: props.view.possible_matches.len(),
                        empty: "No possible matches waiting for review.".to_string(),
                        for candidate in props.view.possible_matches.iter() {
                            button {
                                key: "{candidate.candidate_id}",
                                class: "mm-queue-item",
                                onclick: {
                                    let id = candidate.candidate_id.clone();
                                    move |_| open_match.set(Some(id.clone()))
                                },
                                strong { "{candidate.track_name}" }
                                span { "Candidate: {candidate.candidate_name}" }
                            }
                        }
                    }
                    ReviewQueue {
                        title: "External changes".to_string(),
                        count: props.view.external_changes.len(),
                        empty: "No external changes waiting for approval.".to_string(),
                        for change in props.view.external_changes.iter() {
                            div { key: "{change.result_id}", class: "mm-queue-item",
                                strong { "{change.name}" }
                                span { "{change.detail}" }
                                button {
                                    class: "mm-link-button",
                                    onclick: {
                                        let result_id = change.result_id.clone();
                                        move |_| on_event.call(MusicManagementEvent::ApproveExternalChange { result_id: result_id.clone() })
                                    },
                                    "Approve change"
                                }
                            }
                        }
                    }
                    ReviewQueue {
                        title: "Failed reconciliation".to_string(),
                        count: props.view.failed_reconciliations.len(),
                        empty: "No reconciliation results need retrying.".to_string(),
                        for failure in props.view.failed_reconciliations.iter() {
                            div { key: "{failure.result_id}", class: "mm-queue-item",
                                strong { "{failure.name}" }
                                span { "{safe_error_guidance(&failure.error)}" }
                                button {
                                    class: "mm-link-button",
                                    onclick: {
                                        let result_id = failure.result_id.clone();
                                        move |_| on_event.call(MusicManagementEvent::RetryReconciliationResult { result_id: result_id.clone() })
                                    },
                                    "Retry reconciliation"
                                }
                            }
                        }
                    }
                    ReviewQueue {
                        title: "Unknown primary files".to_string(),
                        count: props.view.unknown_primary_files.len(),
                        empty: "No unassociated files found in primary storage.".to_string(),
                        for file in props.view.unknown_primary_files.iter() {
                            button {
                                key: "{file.id}",
                                class: "mm-queue-item",
                                onclick: {
                                    let id = file.id.clone();
                                    move |_| open_unknown.set(Some(id.clone()))
                                },
                                strong { "{file.filename}" }
                                span { "{file.location}" }
                            }
                        }
                    }
                }
            }

            if soundcloud_configuration_open() {
                SoundCloudConfigurationDialog {
                    on_close: move |_| soundcloud_configuration_open.set(false),
                    on_event,
                }
            }
            if let Some(id) = open_match() {
                if let Some(candidate) = props.view.possible_matches.iter().find(|candidate| candidate.candidate_id == id).cloned() {
                    PossibleMatchDialog {
                        candidate,
                        on_close: move |_| open_match.set(None),
                        on_event,
                    }
                }
            }
            if let Some(id) = open_unknown() {
                if let Some(file) = props.view.unknown_primary_files.iter().find(|file| file.id == id).cloned() {
                    UnknownPrimaryFileDialog {
                        file,
                        tracks: props.view.tracks.clone(),
                        on_close: move |_| open_unknown.set(None),
                        on_event,
                    }
                }
            }
            if let Some(provider) = primary_editor() {
                PrimaryConfigurationDialog {
                    provider,
                    existing: props.view.primary_storage.clone(),
                    on_close: move |_| primary_editor.set(None),
                    on_event,
                }
            }
            if let Some(id) = backup_policy_editor() {
                if let Some(storage) = props.view.backup_storages.iter().find(|storage| storage.id == id).cloned() {
                    BackupPolicyDialog {
                        storage,
                        on_close: move |_| backup_policy_editor.set(None),
                        on_event,
                    }
                }
            }
            if migration_open() {
                PrimaryMigrationDialog {
                    current: props.view.primary_storage.clone(),
                    on_close: move |_| migration_open.set(false),
                    on_event,
                }
            }
            if cleanup_open() {
                CleanupReviewDialog {
                    items: props.view.cleanup_candidates.clone(),
                    on_close: move |_| cleanup_open.set(false),
                    on_event,
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ProviderChooserProps {
    providers: Vec<ProviderChoice>,
    action_label: String,
    on_choose: EventHandler<ProviderChoice>,
}

#[component]
fn ProviderChooser(props: ProviderChooserProps) -> Element {
    rsx! {
        div { class: "mm-provider-options", "aria-label": "{props.action_label}",
            for provider in props.providers {
                button {
                    class: "mm-provider-option",
                    onclick: move |_| props.on_choose.call(provider),
                    span { class: "mm-provider-icon", "{provider.icon()}" }
                    span {
                        strong { "{provider.label()}" }
                        small { "{provider.description()}" }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct SourceCardProps {
    source: SourceView,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn SourceCard(props: SourceCardProps) -> Element {
    let source_id = props.source.id.clone();
    let retry_sync_run_id = props.source.failed_sync_run_id.clone();
    let disconnect_id = props.source.id.clone();
    let revoke_id = props.source.id.clone();
    let clear_cache_id = props.source.id.clone();
    let forget_data_id = props.source.id.clone();
    let source_name = props.source.name.clone();
    let disconnect_source_name = source_name.clone();
    let is_soundcloud = props.source.provider == ProviderChoice::SoundCloud;
    let revoke_source_name = source_name.clone();
    let cache_source_name = source_name.clone();
    let forget_source_name = source_name.clone();
    let mut pending_lifecycle_action = use_signal(|| None::<LifecycleAction>);
    let last_sync = props
        .source
        .last_sync
        .as_deref()
        .map(display_timestamp)
        .unwrap_or_else(|| "Never".to_string());
    let next_sync = props.source.next_sync.as_deref().map(display_timestamp);
    rsx! {
        article { class: "mm-card mm-source-card",
            div { class: "mm-card-header",
                div { class: "mm-card-title",
                    span { class: "mm-provider-icon", "{props.source.provider.icon()}" }
                    div {
                        h3 { "{props.source.name}" }
                        if let Some(account) = props.source.account_identity.clone() {
                            p { "{account}" }
                        }
                    }
                }
                HealthBadge { tone: props.source.health }
            }
            p { class: "mm-detail", "{props.source.detail}" }
            div { class: "mm-source-times",
                span { "Last sync: {last_sync}" }
                if let Some(next_sync) = next_sync {
                    span { "Next: {next_sync}" }
                }
            }
            if let Some(progress) = props.source.progress.clone() {
                JobProgress { progress }
            }
            if let Some(warning) = props.source.warning.clone() {
                p { class: "mm-message warning", "{warning}" }
            }
            if let Some(error) = props.source.error.as_deref() {
                p { class: "mm-message error", role: "alert",
                    strong { "Sync needs attention. " }
                    "{safe_error_guidance(error)}"
                }
            }
            div { class: "mm-collection-list",
                for collection in props.source.collections.iter() {
                    div { key: "{collection.name}", class: "mm-collection",
                        span { "{collection.name}" }
                        span { class: "mm-count", "{collection.item_count}" }
                        if collection.read_only {
                            span { class: "mm-read-only", "Read-only" }
                        }
                    }
                }
            }
            div { class: "mm-card-actions",
                if props.source.connected {
                    button {
                        class: "mm-button secondary",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::SyncSource(source_id.clone())),
                        "Sync now"
                    }
                } else if is_soundcloud {
                    button {
                        class: "mm-button primary",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::AddSource(ProviderChoice::SoundCloud)),
                        "Reconnect SoundCloud"
                    }
                }
                // A SoundCloud sync is a fresh snapshot, so "Sync now" already is the retry.
                if let Some(sync_run_id) = retry_sync_run_id.clone().filter(|_| !is_soundcloud) {
                    button {
                        class: "mm-button secondary",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::RetrySyncRun {
                            sync_run_id: sync_run_id.clone(),
                        }),
                        "Retry failed sync"
                    }
                }
                if is_soundcloud {
                    button {
                        class: "mm-link-button",
                        "aria-haspopup": "dialog",
                        title: "Revokes SoundCloud access and removes SoundCloud catalog data from this device.",
                        onclick: move |_| pending_lifecycle_action.set(Some(LifecycleAction::DisconnectSoundCloud {
                            source_id: disconnect_id.clone(),
                            source_name: disconnect_source_name.clone(),
                        })),
                        "Disconnect SoundCloud"
                    }
                } else if props.source.connected {
                    button {
                        class: "mm-link-button",
                        title: "Stops synchronization without deleting catalog data or cached files.",
                        onclick: move |_| props.on_event.call(
                            LifecycleAction::DisconnectSource {
                                source_id: disconnect_id.clone(),
                            }.into_event()
                        ),
                        "Disconnect source"
                    }
                }
            }
            div { class: "mm-lifecycle-actions", "aria-label": "Provider data and access actions",
                if props.source.provider != ProviderChoice::LocalFolder {
                    button {
                        class: "mm-link-button danger",
                        "aria-haspopup": "dialog",
                        onclick: move |_| pending_lifecycle_action.set(Some(LifecycleAction::RevokeProviderGrant {
                            provider: props.source.provider,
                            connection_id: revoke_id.clone(),
                            subject_name: revoke_source_name.clone(),
                        })),
                        "Revoke provider access"
                    }
                }
                button {
                    class: "mm-link-button danger",
                    "aria-haspopup": "dialog",
                    onclick: move |_| pending_lifecycle_action.set(Some(LifecycleAction::ClearLocalCache {
                        source_id: clear_cache_id.clone(),
                        source_name: cache_source_name.clone(),
                    })),
                    "Clear local cache"
                }
                button {
                    class: "mm-link-button danger",
                    "aria-haspopup": "dialog",
                    onclick: move |_| pending_lifecycle_action.set(Some(LifecycleAction::ForgetProviderCatalogData {
                        source_id: forget_data_id.clone(),
                        source_name: forget_source_name.clone(),
                    })),
                    "Forget catalog data"
                }
            }
        }
        if let Some(action) = pending_lifecycle_action() {
            LifecycleConfirmationDialog {
                action,
                on_cancel: move |_| pending_lifecycle_action.set(None),
                on_confirm: move |action: LifecycleAction| {
                    props.on_event.call(action.into_event());
                    pending_lifecycle_action.set(None);
                },
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct StorageCardProps {
    storage: StorageView,
    on_event: EventHandler<MusicManagementEvent>,
    on_edit_policy: EventHandler<String>,
}

#[component]
fn StorageCard(props: StorageCardProps) -> Element {
    let storage_id = props.storage.id.clone();
    let disconnect_storage_id = props.storage.id.clone();
    let revoke_storage_connection_id = props.storage.id.clone();
    let revoke_storage_name = props.storage.name.clone();
    let policy_id = props.storage.id.clone();
    let mut pending_lifecycle_action = use_signal(|| None::<LifecycleAction>);
    let last_inventory = props
        .storage
        .last_inventory
        .clone()
        .unwrap_or_else(|| "Never".to_string());
    let last_verified = props
        .storage
        .last_verified
        .clone()
        .unwrap_or_else(|| "Never".to_string());
    rsx! {
        article { class: "mm-card mm-storage-card",
            div { class: "mm-card-header",
                div { class: "mm-card-title",
                    span { class: "mm-provider-icon", "{props.storage.provider.icon()}" }
                    div {
                        h4 { "{props.storage.name}" }
                        p { "{props.storage.role.label()} · {props.storage.location}" }
                    }
                }
                HealthBadge { tone: props.storage.health }
            }
            div { class: "mm-storage-meta",
                span { "Inventory: {last_inventory}" }
                span { "Verified: {last_verified}" }
                span { "Jobs: {props.storage.pending_jobs} pending · {props.storage.failed_jobs} failed" }
                if let Some(replication) = props.storage.replication_mode.clone() {
                    span { "Replication: {replication}" }
                }
                if let Some(retention) = props.storage.retention_summary.clone() {
                    span { "Retention: {retention}" }
                }
            }
            div { class: "mm-card-actions",
                button {
                    class: "mm-button secondary",
                    onclick: move |_| props.on_event.call(MusicManagementEvent::RetryStorage(storage_id.clone())),
                    "Check health"
                }
                if props.storage.role == StorageRoleView::Backup {
                    button {
                        class: "mm-link-button",
                        onclick: move |_| props.on_edit_policy.call(policy_id.clone()),
                        "Edit policy"
                    }
                }
                if props.storage.provider == ProviderChoice::GoogleDrive {
                    button {
                        class: "mm-link-button",
                        title: "Disconnects this storage without deleting files from Google Drive.",
                        onclick: move |_| props.on_event.call(
                            LifecycleAction::DisconnectStorage {
                                storage_id: disconnect_storage_id.clone(),
                            }.into_event()
                        ),
                        "Disconnect Google storage"
                    }
                    button {
                        class: "mm-link-button danger",
                        "aria-haspopup": "dialog",
                        onclick: move |_| pending_lifecycle_action.set(Some(LifecycleAction::RevokeProviderGrant {
                            provider: ProviderChoice::GoogleDrive,
                            connection_id: revoke_storage_connection_id.clone(),
                            subject_name: revoke_storage_name.clone(),
                        })),
                        "Revoke Google access"
                    }
                }
            }
        }
        if let Some(action) = pending_lifecycle_action() {
            LifecycleConfirmationDialog {
                action,
                on_cancel: move |_| pending_lifecycle_action.set(None),
                on_confirm: move |action: LifecycleAction| {
                    props.on_event.call(action.into_event());
                    pending_lifecycle_action.set(None);
                },
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct EmptyStorageProps {
    role: StorageRoleView,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn EmptyStorage(props: EmptyStorageProps) -> Element {
    rsx! {
        div { class: "mm-card mm-empty-card",
            p { "No {props.role.label().to_lowercase()} storage configured." }
            div { class: "mm-provider-chooser",
                for provider in [ProviderChoice::LocalFolder, ProviderChoice::GoogleDrive] {
                    button {
                        class: "mm-button secondary",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::CreatePrimary(PrimaryConfigurationView {
                            provider,
                            name: "Music archive".to_string(),
                            location: String::new(),
                            organize_by_artist: true,
                            verify_after_copy: true,
                        })),
                        "{provider.icon()} Use {provider.label()}"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct HealthBadgeProps {
    tone: HealthTone,
}

#[component]
fn HealthBadge(props: HealthBadgeProps) -> Element {
    rsx! { span { class: "mm-health {props.tone.class()}", "{props.tone.label()}" } }
}

#[derive(Props, Clone, PartialEq)]
struct JobProgressProps {
    progress: JobProgressView,
}

#[component]
fn JobProgress(props: JobProgressProps) -> Element {
    let percent = props.progress.percent();
    rsx! {
        div { class: "mm-progress",
            div { class: "mm-progress-label",
                span { "{props.progress.label}" }
                if let Some(total) = props.progress.total {
                    span { "{props.progress.processed}/{total}" }
                } else {
                    span { "{props.progress.processed} processed" }
                }
            }
            div {
                class: if percent.is_some() { "mm-progress-track" } else { "mm-progress-track indeterminate" },
                role: "progressbar",
                "aria-valuenow": percent.map(|value| value.to_string()),
                div {
                    class: "mm-progress-fill",
                    style: percent.map(|value| format!("width: {value}%;")),
                }
            }
            if let Some(item) = props.progress.current_item.clone() {
                span { class: "mm-progress-item", "{item}" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CompletenessDashboardProps {
    completeness: CompletenessView,
}

#[component]
fn CompletenessDashboard(props: CompletenessDashboardProps) -> Element {
    let catalog_percent = percentage(
        props.completeness.catalog_covered,
        props.completeness.canonical_tracks,
    );
    let asset_percent = percentage(
        props.completeness.verified_assets,
        props.completeness.canonical_tracks,
    );
    rsx! {
        div { class: "mm-completeness",
            div { class: "mm-metric-grid",
                Metric { label: "Canonical tracks".to_string(), value: props.completeness.canonical_tracks.to_string(), detail: "Unique tracks across all sources".to_string() }
                Metric { label: "Catalog coverage".to_string(), value: format!("{catalog_percent}%"), detail: format!("{} represented in sources", props.completeness.catalog_covered) }
                Metric { label: "Archivable assets".to_string(), value: format!("{asset_percent}%"), detail: format!("{} verified originals", props.completeness.verified_assets) }
                Metric { label: "Missing but obtainable".to_string(), value: props.completeness.missing_obtainable.to_string(), detail: "Eligible for a primary copy".to_string() }
            }
            div { class: "mm-completeness-details",
                span { "Streaming only: {props.completeness.streaming_only}" }
                span { "Restricted: {props.completeness.restricted}" }
                span { "Possible/ambiguous: {props.completeness.possible_matches}/{props.completeness.ambiguous_matches}" }
                span { "Unknown primary: {props.completeness.unknown_primary_files}" }
                span { "Backup gaps: {props.completeness.backup_gaps}" }
                span { "Provider errors: {props.completeness.provider_errors}" }
            }
            h3 { "Playlist completeness" }
            div { class: "mm-playlist-list",
                for playlist in props.completeness.playlists.iter() {
                    div { key: "{playlist.id}", class: "mm-playlist-row",
                        div {
                            strong { "{playlist.name}" }
                            span { "{playlist.archived_tracks}/{playlist.total_tracks} archived" }
                        }
                        div { class: "mm-playlist-bar", div { style: "width: {playlist.percent()}%;" } }
                        if playlist.needs_review > 0 {
                            span { class: "mm-review-count", "{playlist.needs_review} review" }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct MetricProps {
    label: String,
    value: String,
    detail: String,
}

#[component]
fn Metric(props: MetricProps) -> Element {
    rsx! {
        div { class: "mm-metric",
            span { class: "mm-metric-label", "{props.label}" }
            strong { "{props.value}" }
            span { class: "mm-metric-detail", "{props.detail}" }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct TrackStorageRowProps {
    track: TrackStorageView,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn TrackStorageRow(props: TrackStorageRowProps) -> Element {
    let track_id = props.track.id.clone();
    let copy_id = props.track.id.clone();
    let pin_id = props.track.id.clone();
    let new_pin_state = !props.track.pinned_offline;
    rsx! {
        div { class: "mm-track-row",
            div { class: "mm-track-name",
                strong { "{props.track.title}" }
                span { "{props.track.artist}" }
            }
            div { class: "mm-badges",
                for badge in props.track.badges.iter() {
                    span { key: "{badge}", class: "mm-track-badge", "{badge}" }
                }
            }
            div { class: "mm-track-actions",
                if props.track.status.is_missing() {
                    button {
                        class: "mm-link-button",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::CopyToPrimary(copy_id.clone())),
                        "Copy to primary"
                    }
                }
                if props.track.status.needs_review() {
                    button {
                        class: "mm-link-button",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::ReviewMatch(track_id.clone())),
                        "Review"
                    }
                }
                button {
                    class: "mm-link-button",
                    onclick: move |_| props.on_event.call(MusicManagementEvent::ToggleOfflinePin(pin_id.clone(), new_pin_state)),
                    if props.track.pinned_offline { "Unpin offline" } else { "Pin offline" }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ReviewQueueProps {
    title: String,
    count: usize,
    empty: String,
    children: Element,
}

#[component]
fn ReviewQueue(props: ReviewQueueProps) -> Element {
    rsx! {
        div { class: "mm-review-queue",
            h3 { "{props.title} ({props.count})" }
            if props.count == 0 {
                p { class: "mm-empty-inline", "{props.empty}" }
            } else {
                div { class: "mm-queue-list", {props.children} }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct SoundCloudConfigurationDialogProps {
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn SoundCloudConfigurationDialog(props: SoundCloudConfigurationDialogProps) -> Element {
    let mut client_id = use_signal(String::new);
    let mut redirect_uri = use_signal(|| "http://127.0.0.1:17841/oauth/soundcloud".to_string());

    rsx! {
        Modal { title: "Configure SoundCloud".to_string(), on_close: props.on_close,
            p { class: "mm-message warning",
                "Optional. By default rec_bag uses the client ID and redirect URI of its SoundCloud broker. Set them here only to override that; the broker secret is never entered or stored here."
            }
            label { class: "mm-field",
                span { "Client ID" }
                input {
                    r#type: "text",
                    value: "{client_id}",
                    oninput: move |event| client_id.set(event.value()),
                }
            }
            label { class: "mm-field",
                span { "Redirect URI" }
                input {
                    r#type: "url",
                    value: "{redirect_uri}",
                    oninput: move |event| redirect_uri.set(event.value()),
                }
            }
            div { class: "mm-dialog-actions",
                button {
                    class: "mm-button secondary",
                    onclick: move |_| props.on_close.call(()),
                    "Cancel"
                }
                button {
                    class: "mm-button primary",
                    disabled: client_id().trim().is_empty() || redirect_uri().trim().is_empty(),
                    onclick: move |_| {
                        props.on_event.call(MusicManagementEvent::ConfigureSoundCloud(
                            SoundCloudConfigurationView {
                                client_id: client_id().trim().to_string(),
                                redirect_uri: redirect_uri().trim().to_string(),
                            },
                        ));
                        props.on_close.call(());
                    },
                    "Save SoundCloud setup"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct PossibleMatchDialogProps {
    candidate: MatchCandidateView,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn PossibleMatchDialog(props: PossibleMatchDialogProps) -> Element {
    let id = props.candidate.candidate_id.clone();
    let reject_id = props.candidate.candidate_id.clone();
    let association_candidate_id = props.candidate.candidate_id.clone();
    let association_track_id = props.candidate.track_id.clone();
    let mut local_file_path = use_signal(String::new);
    rsx! {
        Modal { title: "Review possible match".to_string(), on_close: props.on_close,
            p { "Associate this candidate only if it is the same recording." }
            dl { class: "mm-dialog-details",
                dt { "Catalog track" } dd { "{props.candidate.track_name}" }
                dt { "Candidate file" } dd { "{props.candidate.candidate_name}" }
                dt { "Evidence" } dd { "{props.candidate.evidence}" }
            }
            fieldset { class: "mm-fieldset",
                legend { "Or associate a local audio file" }
                label {
                    span { "File path" }
                    input {
                        r#type: "text",
                        placeholder: "/Music/Artist/Track.flac",
                        value: "{local_file_path}",
                        oninput: move |event| local_file_path.set(event.value()),
                    }
                }
                button {
                    class: "mm-button secondary",
                    disabled: local_file_path().trim().is_empty(),
                    onclick: move |_| props.on_event.call(MusicManagementEvent::AssociateLocalFile {
                        candidate_id: association_candidate_id.clone(),
                        track_id: association_track_id.clone(),
                        file_path: local_file_path().trim().to_string(),
                    }),
                    "Associate local file"
                }
            }
            div { class: "mm-dialog-actions",
                button { class: "mm-button secondary", onclick: move |_| props.on_event.call(MusicManagementEvent::RejectMatch(reject_id.clone())), "Reject" }
                button { class: "mm-button primary", onclick: move |_| props.on_event.call(MusicManagementEvent::ConfirmMatch(id.clone())), "Confirm candidate" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct UnknownPrimaryFileDialogProps {
    file: UnknownPrimaryFileView,
    tracks: Vec<TrackStorageView>,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn UnknownPrimaryFileDialog(props: UnknownPrimaryFileDialogProps) -> Element {
    let associate_id = props.file.id.clone();
    let import_id = props.file.id.clone();
    let ignore_id = props.file.id.clone();
    let mut selected_track_id = use_signal(|| None::<String>);
    rsx! {
        Modal { title: "Review primary file".to_string(), on_close: props.on_close,
            p { "This file was found in primary storage without a known catalog association." }
            dl { class: "mm-dialog-details",
                dt { "File" } dd { "{props.file.filename}" }
                dt { "Location" } dd { "{props.file.location}" }
                dt { "Discovered" } dd { "{props.file.discovered_at}" }
            }
            label { class: "mm-field",
                span { "Existing catalog track" }
                select {
                    value: selected_track_id().unwrap_or_default(),
                    onchange: move |event| {
                        let value = event.value();
                        selected_track_id.set((!value.is_empty()).then_some(value));
                    },
                    option { value: "", "Select a catalog track…" }
                    for track in props.tracks.iter() {
                        option { key: "{track.id}", value: "{track.id}", "{track.title} — {track.artist}" }
                    }
                }
            }
            div { class: "mm-dialog-actions mm-wrap-actions",
                button {
                    class: "mm-button secondary",
                    disabled: selected_track_id().is_none(),
                    onclick: move |_| if let Some(track_id) = selected_track_id() {
                        props.on_event.call(MusicManagementEvent::AssociateUnknownFile {
                            file_id: associate_id.clone(),
                            track_id,
                        });
                    },
                    "Associate"
                }
                button { class: "mm-button primary", onclick: move |_| props.on_event.call(MusicManagementEvent::ImportUnknownFile(import_id.clone())), "Import as new track" }
                button { class: "mm-link-button danger", onclick: move |_| props.on_event.call(MusicManagementEvent::IgnoreUnknownFile(ignore_id.clone())), "Ignore file" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct PrimaryConfigurationDialogProps {
    provider: ProviderChoice,
    existing: Option<StorageView>,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn PrimaryConfigurationDialog(props: PrimaryConfigurationDialogProps) -> Element {
    let initial_name = props
        .existing
        .as_ref()
        .map(|storage| storage.name.clone())
        .unwrap_or_else(|| "Music archive".to_string());
    let initial_location = props
        .existing
        .as_ref()
        .map(|storage| storage.location.clone())
        .unwrap_or_default();
    let mut name = use_signal(|| initial_name);
    let mut location = use_signal(|| initial_location);
    let mut organize_by_artist = use_signal(|| true);
    let mut verify_after_copy = use_signal(|| true);
    let provider = props.provider;
    let existing_storage_id = props.existing.as_ref().map(|storage| storage.id.clone());
    let editing_existing = existing_storage_id.is_some();

    rsx! {
        Modal {
            title: if editing_existing { "Edit primary metadata".to_string() } else { "Create primary storage".to_string() },
            on_close: props.on_close,
            div { class: "mm-provider-summary",
                span { class: "mm-provider-icon", "{provider.icon()}" }
                div { strong { "{provider.label()}" } small { "{provider.description()}" } }
            }
            label { class: "mm-field",
                span { "Display name" }
                input { value: "{name}", oninput: move |event| name.set(event.value()) }
            }
            label { class: "mm-field",
                span { if provider == ProviderChoice::LocalFolder { "Folder path" } else { "Provider folder" } }
                input {
                    value: "{location}",
                    disabled: editing_existing,
                    placeholder: if provider == ProviderChoice::LocalFolder { "/Music/rec_bag" } else { "My Drive / rec_bag" },
                    oninput: move |event| location.set(event.value()),
                }
                if editing_existing {
                    small { "To change provider or location, prepare a primary migration." }
                }
            }
            label { class: "mm-check-field",
                input { r#type: "checkbox", checked: organize_by_artist(), onclick: move |_| organize_by_artist.set(!organize_by_artist()) }
                span { "Organize copied files by artist and album" }
            }
            label { class: "mm-check-field",
                input { r#type: "checkbox", checked: verify_after_copy(), onclick: move |_| verify_after_copy.set(!verify_after_copy()) }
                span { "Verify checksums after every copy" }
            }
            p { class: "mm-message warning",
                if editing_existing {
                    "This updates display metadata and copy policy only; active storage authority is unchanged."
                } else {
                    "Creating a primary is only available when no active primary exists."
                }
            }
            div { class: "mm-dialog-actions",
                button {
                    class: "mm-button primary",
                    disabled: name().trim().is_empty() || location().trim().is_empty(),
                    onclick: move |_| {
                        if let Some(storage_id) = existing_storage_id.clone() {
                            props.on_event.call(MusicManagementEvent::UpdatePrimaryMetadata(PrimaryMetadataUpdateView {
                                storage_id,
                                name: name().trim().to_string(),
                                organize_by_artist: organize_by_artist(),
                                verify_after_copy: verify_after_copy(),
                            }));
                        } else {
                            props.on_event.call(MusicManagementEvent::CreatePrimary(PrimaryConfigurationView {
                                provider,
                                name: name().trim().to_string(),
                                location: location().trim().to_string(),
                                organize_by_artist: organize_by_artist(),
                                verify_after_copy: verify_after_copy(),
                            }));
                        }
                    },
                    if editing_existing { "Save metadata" } else { "Create primary" }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct BackupPolicyDialogProps {
    storage: StorageView,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn BackupPolicyDialog(props: BackupPolicyDialogProps) -> Element {
    let target_id = props.storage.id.clone();
    let mut verified_only = use_signal(|| true);
    let mut retention_days = use_signal(|| 30_u32);
    let mut minimum_versions = use_signal(|| 3_u32);

    rsx! {
        Modal { title: format!("Backup policy · {}", props.storage.name), on_close: props.on_close,
            p { "Choose what is replicated and how long superseded versions remain available." }
            label { class: "mm-check-field",
                input { r#type: "checkbox", checked: verified_only(), onclick: move |_| verified_only.set(!verified_only()) }
                span { "Replicate only checksum-verified primary assets" }
            }
            div { class: "mm-field-row",
                label { class: "mm-field",
                    span { "Retention days" }
                    input {
                        r#type: "number", min: "1", value: "{retention_days}",
                        oninput: move |event| if let Ok(value) = event.value().parse() { retention_days.set(value); }
                    }
                }
                label { class: "mm-field",
                    span { "Minimum versions" }
                    input {
                        r#type: "number", min: "1", value: "{minimum_versions}",
                        oninput: move |event| if let Ok(value) = event.value().parse() { minimum_versions.set(value); }
                    }
                }
            }
            p { class: "mm-message warning", "Expired versions only become cleanup candidates; removal still requires confirmation." }
            div { class: "mm-dialog-actions",
                button {
                    class: "mm-button primary",
                    onclick: move |_| props.on_event.call(MusicManagementEvent::SaveBackupPolicy(BackupPolicyView {
                        target_id: target_id.clone(),
                        replicate_verified_only: verified_only(),
                        retention_days: retention_days().max(1),
                        minimum_versions: minimum_versions().max(1),
                    })),
                    "Save policy"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct PrimaryMigrationDialogProps {
    current: Option<StorageView>,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn PrimaryMigrationDialog(props: PrimaryMigrationDialogProps) -> Element {
    let mut step = use_signal(|| 1_u8);
    let mut provider = use_signal(|| ProviderChoice::LocalFolder);
    let mut destination_name = use_signal(|| "New music archive".to_string());
    let mut destination_location = use_signal(String::new);
    let mut verify_checksums = use_signal(|| true);
    let mut keep_old_as_backup = use_signal(|| true);

    rsx! {
        Modal { title: "Migrate primary storage".to_string(), on_close: props.on_close,
            ol { class: "mm-stepper", "aria-label": "Migration progress",
                for number in 1_u8..=3 {
                    li { class: if number == step() { "active" } else if number < step() { "complete" } else { "" }, "{number}" }
                }
            }
            if step() == 1 {
                div { class: "mm-wizard-page",
                    h3 { "1. Choose destination" }
                    ProviderChooser {
                        providers: vec![ProviderChoice::LocalFolder, ProviderChoice::GoogleDrive],
                        action_label: "Migration destination provider".to_string(),
                        on_choose: move |choice| provider.set(choice),
                    }
                    p { class: "mm-selection", "Selected: {provider().label()}" }
                    label { class: "mm-field", span { "Archive name" } input { value: "{destination_name}", oninput: move |event| destination_name.set(event.value()) } }
                    label { class: "mm-field",
                        span { "Destination path or folder" }
                        input { value: "{destination_location}", placeholder: "/Music/New archive", oninput: move |event| destination_location.set(event.value()) }
                    }
                }
            } else if step() == 2 {
                div { class: "mm-wizard-page",
                    h3 { "2. Verification and fallback" }
                    if let Some(current) = props.current.clone() {
                        p { "Current primary: {current.name} · {current.location}" }
                    }
                    label { class: "mm-check-field",
                        input { r#type: "checkbox", checked: verify_checksums(), onclick: move |_| verify_checksums.set(!verify_checksums()) }
                        span { "Verify checksums before switching authority" }
                    }
                    label { class: "mm-check-field",
                        input { r#type: "checkbox", checked: keep_old_as_backup(), onclick: move |_| keep_old_as_backup.set(!keep_old_as_backup()) }
                        span { "Keep the old primary configured as a backup" }
                    }
                    p { class: "mm-message warning", "The old primary is never deleted by migration." }
                }
            } else {
                div { class: "mm-wizard-page",
                    h3 { "3. Review migration" }
                    dl { class: "mm-dialog-details",
                        dt { "Destination" } dd { "{provider().label()} · {destination_location}" }
                        dt { "Verification" } dd { if verify_checksums() { "Checksum verification required" } else { "Copy verification skipped" } }
                        dt { "Old primary" } dd { if keep_old_as_backup() { "Retain as backup" } else { "Leave unchanged and unassigned" } }
                    }
                    p { "The mock event starts inventory, copy, verification, and authority-switch jobs in that order." }
                }
            }
            div { class: "mm-dialog-actions",
                button { class: "mm-button secondary", disabled: step() == 1, onclick: move |_| step.set(step().saturating_sub(1)), "Back" }
                if step() < 3 {
                    button {
                        class: "mm-button primary",
                        disabled: step() == 1 && (destination_name().trim().is_empty() || destination_location().trim().is_empty()),
                        onclick: move |_| step.set((step() + 1).min(3)),
                        "Continue"
                    }
                } else {
                    button {
                        class: "mm-button primary",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::PreparePrimaryMigration(PrimaryMigrationPlanView {
                            destination: PrimaryConfigurationView {
                                provider: provider(),
                                name: destination_name().trim().to_string(),
                                location: destination_location().trim().to_string(),
                                organize_by_artist: true,
                                verify_after_copy: verify_checksums(),
                            },
                            verify_checksums: verify_checksums(),
                            keep_old_as_backup: keep_old_as_backup(),
                        })),
                        "Start migration"
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CleanupReviewDialogProps {
    items: Vec<CleanupItemView>,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn CleanupReviewDialog(props: CleanupReviewDialogProps) -> Element {
    let initial_ids: Vec<String> = props.items.iter().map(|item| item.id.clone()).collect();
    let mut selected_ids = use_signal(|| initial_ids);
    rsx! {
        Modal { title: "Backup retention and cleanup".to_string(), on_close: props.on_close,
            p { "Nothing is permanently removed until you confirm this review." }
            div { class: "mm-cleanup-list",
                for item in props.items.iter() {
                    label { key: "{item.id}", class: "mm-cleanup-item",
                        input {
                            r#type: "checkbox",
                            checked: selected_ids().contains(&item.id),
                            onclick: {
                                let id = item.id.clone();
                                move |_| selected_ids.with_mut(|selected| toggle_selection(selected, &id))
                            }
                        }
                        span { strong { "{item.name}" } small { "{item.age} · {item.reason}" } }
                    }
                }
            }
            div { class: "mm-dialog-actions",
                button { class: "mm-button primary", disabled: selected_ids().is_empty(), onclick: move |_| props.on_event.call(MusicManagementEvent::ConfirmCleanup(selected_ids())), "Confirm cleanup ({selected_ids().len()})" }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct LifecycleConfirmationDialogProps {
    action: LifecycleAction,
    on_cancel: EventHandler<()>,
    on_confirm: EventHandler<LifecycleAction>,
}

#[component]
fn LifecycleConfirmationDialog(props: LifecycleConfirmationDialogProps) -> Element {
    let mut acknowledged = use_signal(|| false);
    let action = props.action.clone();
    let confirm_action = props.action.clone();
    let title = action.title().to_string();
    let subject = action.subject_name().map(ToOwned::to_owned);

    rsx! {
        Modal { title, on_close: props.on_cancel,
            div { class: "mm-destructive-confirmation",
                p { class: "mm-message error", role: "alert", "{action.warning()}" }
                if let Some(subject) = subject {
                    p { "Affected provider connection: " strong { "{subject}" } }
                }
                p { "This action never requests, displays, or submits provider credentials." }
                label { class: "mm-check-field",
                    input {
                        r#type: "checkbox",
                        checked: acknowledged(),
                        onchange: move |event| acknowledged.set(event.checked()),
                    }
                    span { "{action.confirmation_label()}" }
                }
            }
            div { class: "mm-dialog-actions",
                button {
                    class: "mm-button secondary",
                    autofocus: true,
                    onclick: move |_| props.on_cancel.call(()),
                    "Cancel"
                }
                button {
                    class: "mm-button danger",
                    disabled: !confirm_action.can_confirm(acknowledged()),
                    onclick: move |_| props.on_confirm.call(confirm_action.clone()),
                    "Confirm destructive action"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ModalProps {
    title: String,
    on_close: EventHandler<()>,
    children: Element,
}

#[component]
fn Modal(props: ModalProps) -> Element {
    rsx! {
        div { class: "mm-modal-backdrop", role: "presentation",
            div { class: "mm-modal", role: "dialog", "aria-modal": "true", "aria-label": "{props.title}",
                div { class: "mm-modal-header",
                    h2 { "{props.title}" }
                    button { class: "mm-modal-close", "aria-label": "Close", onclick: move |_| props.on_close.call(()), "×" }
                }
                div { class: "mm-modal-body", {props.children} }
            }
        }
    }
}

/// Converts an untrusted provider/repository error into fixed, actionable copy.
///
/// The input is inspected only for broad classification. It is never included in
/// the returned text, which prevents credentials, URLs, file paths, or response
/// bodies from reaching the UI.
/// Shows provider timestamps (RFC 3339) in local time; anything unparsable is shown as is.
fn display_timestamp(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| value.to_string())
}

fn safe_error_guidance(untrusted: &str) -> &'static str {
    let normalized = untrusted.to_ascii_lowercase();
    if [
        "oauth",
        "auth",
        "credential",
        "token",
        "unauthorized",
        "401",
        "403",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        "Reconnect the provider, then try again."
    } else if ["rate limit", "too many requests", "429"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        "The provider is temporarily limiting requests. Wait a moment, then retry."
    } else if ["offline", "network", "timeout", "timed out", "unavailable"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        "Check the connection and retry."
    } else if ["permission", "access denied", "read-only"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        "Check provider permissions or choose a location you can access."
    } else if ["configuration", "configured", "not registered", "not found"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        "Review this provider's configuration, then retry."
    } else {
        "Retry the operation. If it fails again, copy the safe diagnostics from the error details."
    }
}

fn toggle_selection(selected: &mut Vec<String>, id: &str) {
    if let Some(index) = selected.iter().position(|selected_id| selected_id == id) {
        selected.remove(index);
    } else {
        selected.push(id.to_string());
    }
}

fn track_matches_filter(track: &TrackStorageView, filter: TrackFilter) -> bool {
    match filter {
        TrackFilter::All => true,
        TrackFilter::Missing => track.status.is_missing(),
        TrackFilter::NeedsReview => track.status.needs_review(),
        TrackFilter::PinnedOffline => track.pinned_offline,
    }
}

fn percentage(value: u32, total: u32) -> u32 {
    if total == 0 {
        100
    } else {
        (value.saturating_mul(100) / total).min(100)
    }
}

/// A realistic, static workspace for exercising UI wiring before providers are integrated.
#[component]
pub fn MusicManagementMock() -> Element {
    let mut last_event = use_signal(|| None::<MusicManagementEvent>);
    rsx! {
        MusicManagementDashboard {
            view: mock_music_management_view(),
            on_event: move |event| last_event.set(Some(event)),
        }
        if let Some(event) = last_event() {
            p { class: "mm-mock-event", "Mock event: {event:?}" }
        }
    }
}

/// Returns representative mock data for [`MusicManagementMock`] and UI previews.
pub fn mock_music_management_view() -> MusicManagementView {
    MusicManagementView {
        sources: vec![
            SourceView {
                id: "soundcloud-main".to_string(),
                provider: ProviderChoice::SoundCloud,
                name: "SoundCloud".to_string(),
                account_identity: Some("@micha".to_string()),
                detail: "Connected account catalog".to_string(),
                health: HealthTone::Working,
                last_sync: Some("12 minutes ago".to_string()),
                next_sync: Some("In 48 minutes".to_string()),
                progress: Some(JobProgressView {
                    label: "Syncing liked tracks".to_string(),
                    processed: 184,
                    total: Some(241),
                    current_item: Some("Night Transit — Nia Archives".to_string()),
                }),
                warning: Some("3 tracks are streaming-only and cannot be archived.".to_string()),
                error: None,
                failed_sync_run_id: None,
                collections: vec![
                    SourceCollectionView {
                        name: "Uploads".to_string(),
                        item_count: 31,
                        read_only: false,
                    },
                    SourceCollectionView {
                        name: "Created playlists".to_string(),
                        item_count: 12,
                        read_only: true,
                    },
                    SourceCollectionView {
                        name: "Liked tracks".to_string(),
                        item_count: 241,
                        read_only: false,
                    },
                    SourceCollectionView {
                        name: "Liked playlists".to_string(),
                        item_count: 18,
                        read_only: true,
                    },
                ],
                connected: true,
            },
            SourceView {
                id: "local-crate".to_string(),
                provider: ProviderChoice::LocalFolder,
                name: "DJ crate".to_string(),
                account_identity: None,
                detail: "/Music/DJ crate".to_string(),
                health: HealthTone::Healthy,
                last_sync: Some("Yesterday, 18:42".to_string()),
                next_sync: None,
                progress: None,
                warning: None,
                error: None,
                failed_sync_run_id: None,
                collections: vec![SourceCollectionView {
                    name: "Local tracks".to_string(),
                    item_count: 948,
                    read_only: false,
                }],
                connected: true,
            },
        ],
        primary_storage: Some(StorageView {
            id: "primary-local".to_string(),
            provider: ProviderChoice::LocalFolder,
            name: "Music archive".to_string(),
            location: "/Volumes/Archive/rec_bag".to_string(),
            health: HealthTone::Healthy,
            role: StorageRoleView::Primary,
            last_inventory: Some("Today, 09:10".to_string()),
            last_verified: Some("Today, 09:21".to_string()),
            pending_jobs: 2,
            failed_jobs: 0,
            replication_mode: None,
            retention_summary: None,
        }),
        backup_storages: vec![StorageView {
            id: "backup-drive".to_string(),
            provider: ProviderChoice::GoogleDrive,
            name: "Archive backup".to_string(),
            location: "My Drive / rec_bag backup".to_string(),
            health: HealthTone::Warning,
            role: StorageRoleView::Backup,
            last_inventory: Some("Yesterday, 23:15".to_string()),
            last_verified: Some("Yesterday, 23:29".to_string()),
            pending_jobs: 4,
            failed_jobs: 1,
            replication_mode: Some("Copy verified originals".to_string()),
            retention_summary: Some("30 days, minimum 3 versions".to_string()),
        }],
        unassigned_storages: vec![StorageView {
            id: "primary-candidate".to_string(),
            provider: ProviderChoice::LocalFolder,
            name: "Migration candidate".to_string(),
            location: "/Volumes/NewArchive/rec_bag".to_string(),
            health: HealthTone::Healthy,
            role: StorageRoleView::Unassigned,
            last_inventory: Some("Today, 08:55".to_string()),
            last_verified: None,
            pending_jobs: 0,
            failed_jobs: 0,
            replication_mode: None,
            retention_summary: None,
        }],
        completeness: CompletenessView {
            canonical_tracks: 1_024,
            catalog_covered: 1_024,
            verified_assets: 948,
            missing_obtainable: 41,
            streaming_only: 20,
            restricted: 7,
            possible_matches: 5,
            ambiguous_matches: 2,
            unknown_primary_files: 3,
            backup_gaps: 4,
            provider_errors: 1,
            playlists: vec![
                PlaylistCompletenessView {
                    id: "late-night".to_string(),
                    name: "Late night".to_string(),
                    archived_tracks: 56,
                    total_tracks: 61,
                    needs_review: 1,
                },
                PlaylistCompletenessView {
                    id: "warmup".to_string(),
                    name: "Warmup".to_string(),
                    archived_tracks: 39,
                    total_tracks: 39,
                    needs_review: 0,
                },
                PlaylistCompletenessView {
                    id: "club".to_string(),
                    name: "Club tools".to_string(),
                    archived_tracks: 82,
                    total_tracks: 94,
                    needs_review: 3,
                },
            ],
        },
        tracks: vec![
            TrackStorageView {
                id: "track-night-transit".to_string(),
                title: "Night Transit".to_string(),
                artist: "Nia Archives".to_string(),
                status: TrackStorageStatusView::MissingObtainable,
                badges: vec!["SoundCloud".to_string(), "Missing primary".to_string()],
                pinned_offline: false,
            },
            TrackStorageView {
                id: "track-float".to_string(),
                title: "Float".to_string(),
                artist: "Skee Mask".to_string(),
                status: TrackStorageStatusView::StoredAndVerified,
                badges: vec!["Verified primary".to_string(), "Backup gap".to_string()],
                pinned_offline: true,
            },
            TrackStorageView {
                id: "track-echo".to_string(),
                title: "Echo Park".to_string(),
                artist: "DJ Seinfeld".to_string(),
                status: TrackStorageStatusView::PossibleMatch,
                badges: vec!["Possible match".to_string()],
                pinned_offline: false,
            },
        ],
        possible_matches: vec![MatchCandidateView {
            candidate_id: "match-echo".to_string(),
            track_id: "track-echo".to_string(),
            track_name: "Echo Park — DJ Seinfeld".to_string(),
            candidate_name: "DJ Seinfeld - Echo Park (320).mp3".to_string(),
            evidence: "Artist, title, and duration match; checksum is not available.".to_string(),
        }],
        unknown_primary_files: vec![UnknownPrimaryFileView {
            id: "unknown-001".to_string(),
            filename: "unlabeled-set-rip.flac".to_string(),
            location: "/Volumes/Archive/rec_bag/Imports".to_string(),
            discovered_at: "Today, 09:10".to_string(),
        }],
        external_changes: vec![ExternalChangeView {
            result_id: "result-external-001".to_string(),
            name: "Float — Skee Mask".to_string(),
            detail: "The primary file changed outside rec_bag.".to_string(),
        }],
        failed_reconciliations: vec![FailedReconciliationView {
            result_id: "result-failed-001".to_string(),
            name: "Unknown backup object".to_string(),
            error: "Storage was unavailable during checksum verification.".to_string(),
        }],
        cleanup_candidates: vec![CleanupItemView {
            id: "cleanup-001".to_string(),
            name: "Night Transit (replaced version)".to_string(),
            age: "42 days old".to_string(),
            reason: "Retention period elapsed".to_string(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_is_bounded_and_empty_sets_are_complete() {
        assert_eq!(percentage(25, 100), 25);
        assert_eq!(percentage(200, 100), 100);
        assert_eq!(percentage(0, 0), 100);
    }

    #[test]
    fn mock_view_has_reviewable_workspace_data() {
        let view = mock_music_management_view();
        assert!(view.primary_storage.is_some());
        assert!(!view.sources.is_empty());
        assert!(!view.backup_storages.is_empty());
        assert_eq!(
            view.unassigned_storages[0].role,
            StorageRoleView::Unassigned
        );
        assert!(!view.possible_matches.is_empty());
        assert!(!view.unknown_primary_files.is_empty());
        assert!(!view.external_changes.is_empty());
        assert!(!view.failed_reconciliations.is_empty());
        assert!(!view.cleanup_candidates.is_empty());

        let soundcloud = view
            .sources
            .iter()
            .find(|source| source.provider == ProviderChoice::SoundCloud)
            .expect("mock includes a SoundCloud account");
        assert!(soundcloud.progress.is_some());
        for expected in [
            "Uploads",
            "Created playlists",
            "Liked tracks",
            "Liked playlists",
        ] {
            assert!(soundcloud
                .collections
                .iter()
                .any(|collection| collection.name == expected));
        }
    }

    #[test]
    fn track_filters_match_their_visible_storage_state() {
        let tracks = mock_music_management_view().tracks;
        assert_eq!(
            tracks
                .iter()
                .filter(|track| track_matches_filter(track, TrackFilter::Missing))
                .count(),
            1
        );
        assert_eq!(
            tracks
                .iter()
                .filter(|track| track_matches_filter(track, TrackFilter::NeedsReview))
                .count(),
            1
        );
        assert_eq!(
            tracks
                .iter()
                .filter(|track| track_matches_filter(track, TrackFilter::PinnedOffline))
                .count(),
            1
        );
    }

    #[test]
    fn cleanup_selection_can_be_removed_and_restored() {
        let mut selected = vec!["old-version".to_string()];
        toggle_selection(&mut selected, "old-version");
        assert!(selected.is_empty());
        toggle_selection(&mut selected, "old-version");
        assert_eq!(selected, ["old-version"]);
    }

    #[test]
    fn provider_descriptions_explain_distinct_roles() {
        assert!(ProviderChoice::LocalFolder.description().contains("device"));
        assert!(ProviderChoice::SoundCloud
            .description()
            .contains("playlists"));
        assert!(ProviderChoice::GoogleDrive
            .description()
            .contains("storage"));
    }

    #[test]
    fn manual_association_event_preserves_candidate_track_and_path_ids() {
        let event = MusicManagementEvent::AssociateLocalFile {
            candidate_id: "candidate-1".to_string(),
            track_id: "track-1".to_string(),
            file_path: "/Music/track.flac".to_string(),
        };
        assert_eq!(
            event,
            MusicManagementEvent::AssociateLocalFile {
                candidate_id: "candidate-1".to_string(),
                track_id: "track-1".to_string(),
                file_path: "/Music/track.flac".to_string(),
            }
        );
    }

    #[test]
    fn unknown_file_association_uses_a_stable_track_id() {
        assert_eq!(
            MusicManagementEvent::AssociateUnknownFile {
                file_id: "file-1".to_string(),
                track_id: "track-1".to_string(),
            },
            MusicManagementEvent::AssociateUnknownFile {
                file_id: "file-1".to_string(),
                track_id: "track-1".to_string(),
            }
        );
    }

    #[test]
    fn progress_supports_determinate_and_indeterminate_jobs() {
        let determinate = JobProgressView {
            label: "Sync".to_string(),
            processed: 25,
            total: Some(100),
            current_item: None,
        };
        let indeterminate = JobProgressView {
            total: None,
            ..determinate.clone()
        };
        assert_eq!(determinate.percent(), Some(25));
        assert_eq!(indeterminate.percent(), None);
    }

    #[test]
    fn error_guidance_never_echoes_secrets_or_provider_bodies() {
        let secret = "access_token=super-secret";
        let body = format!("401 OAuth failure: {secret}; raw body={{\"private\":true}}");
        let guidance = safe_error_guidance(&body);
        assert_eq!(guidance, "Reconnect the provider, then try again.");
        assert!(!guidance.contains(secret));
        assert!(!guidance.contains("private"));
    }

    #[test]
    fn error_guidance_maps_actionable_categories() {
        assert!(safe_error_guidance("HTTP 429 rate limit").contains("Wait"));
        assert!(safe_error_guidance("network timeout").contains("connection"));
        assert!(safe_error_guidance("permission denied").contains("permissions"));
        assert!(safe_error_guidance("provider not configured").contains("configuration"));
    }

    #[test]
    fn disconnect_events_are_non_destructive_and_distinct() {
        let source_disconnect = LifecycleAction::DisconnectSource {
            source_id: "source-1".to_string(),
        };
        let storage_disconnect = LifecycleAction::DisconnectStorage {
            storage_id: "storage-1".to_string(),
        };

        assert!(!source_disconnect.requires_confirmation());
        assert!(!storage_disconnect.requires_confirmation());
        assert_eq!(
            source_disconnect.into_event(),
            MusicManagementEvent::DisconnectSource {
                source_id: "source-1".to_string(),
            }
        );
        assert_eq!(
            storage_disconnect.into_event(),
            MusicManagementEvent::DisconnectStorage {
                storage_id: "storage-1".to_string(),
            }
        );
    }

    #[test]
    fn soundcloud_disconnect_requires_confirmation_and_emits_a_source_disconnect() {
        let action = LifecycleAction::DisconnectSoundCloud {
            source_id: "source-1".to_string(),
            source_name: "SoundCloud".to_string(),
        };
        assert!(action.requires_confirmation());
        assert!(!action.can_confirm(false));
        assert_eq!(
            action.into_event(),
            MusicManagementEvent::DisconnectSource {
                source_id: "source-1".to_string(),
            }
        );
    }

    #[test]
    fn timestamps_render_in_local_time_and_fall_back_to_raw_text() {
        assert_eq!(display_timestamp("not a date"), "not a date");
        assert_eq!(display_timestamp("2026-10-24T08:30:00+00:00").len(), 16);
    }

    #[test]
    fn destructive_provider_actions_require_explicit_confirmation() {
        let actions = [
            LifecycleAction::RevokeProviderGrant {
                provider: ProviderChoice::SoundCloud,
                connection_id: "source-1".to_string(),
                subject_name: "Provider".to_string(),
            },
            LifecycleAction::ClearLocalCache {
                source_id: "source-1".to_string(),
                source_name: "Provider".to_string(),
            },
            LifecycleAction::ForgetProviderCatalogData {
                source_id: "source-1".to_string(),
                source_name: "Provider".to_string(),
            },
        ];

        for action in actions {
            assert!(action.requires_confirmation());
            assert!(!action.can_confirm(false));
            assert!(action.can_confirm(true));
        }
    }

    #[test]
    fn destructive_provider_events_remain_semantically_separate() {
        let revoke = LifecycleAction::RevokeProviderGrant {
            provider: ProviderChoice::GoogleDrive,
            connection_id: "storage-1".to_string(),
            subject_name: "Provider".to_string(),
        }
        .into_event();
        let clear = LifecycleAction::ClearLocalCache {
            source_id: "source-1".to_string(),
            source_name: "Provider".to_string(),
        }
        .into_event();
        let forget = LifecycleAction::ForgetProviderCatalogData {
            source_id: "source-1".to_string(),
            source_name: "Provider".to_string(),
        }
        .into_event();

        assert!(matches!(
            revoke,
            MusicManagementEvent::RevokeProviderGrant { .. }
        ));
        assert!(matches!(
            clear,
            MusicManagementEvent::ClearLocalCache { .. }
        ));
        assert!(matches!(
            forget,
            MusicManagementEvent::ForgetProviderCatalogData { .. }
        ));
        assert_ne!(revoke, clear);
        assert_ne!(clear, forget);
    }

    #[test]
    fn primary_events_do_not_conflate_creation_metadata_and_migration() {
        let configuration = PrimaryConfigurationView {
            provider: ProviderChoice::LocalFolder,
            name: "Archive".to_string(),
            location: "/Music/Archive".to_string(),
            organize_by_artist: true,
            verify_after_copy: true,
        };
        let metadata = PrimaryMetadataUpdateView {
            storage_id: "storage-1".to_string(),
            name: "Renamed archive".to_string(),
            organize_by_artist: false,
            verify_after_copy: true,
        };

        assert!(matches!(
            MusicManagementEvent::CreatePrimary(configuration.clone()),
            MusicManagementEvent::CreatePrimary(_)
        ));
        assert!(matches!(
            MusicManagementEvent::UpdatePrimaryMetadata(metadata),
            MusicManagementEvent::UpdatePrimaryMetadata(_)
        ));
        assert!(matches!(
            MusicManagementEvent::PreparePrimaryMigration(PrimaryMigrationPlanView {
                destination: configuration,
                verify_checksums: true,
                keep_old_as_backup: true,
            }),
            MusicManagementEvent::PreparePrimaryMigration(_)
        ));
    }
}
