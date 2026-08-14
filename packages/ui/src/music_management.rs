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
    pub total: u32,
    pub current_item: Option<String>,
}

impl JobProgressView {
    fn percent(&self) -> u32 {
        if self.total == 0 {
            0
        } else {
            (self.processed.saturating_mul(100) / self.total).min(100)
        }
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
    Primary,
    Backup,
}

impl StorageRoleView {
    fn label(self) -> &'static str {
        match self {
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

/// Provider-neutral per-track storage state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackStorageView {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub badges: Vec<String>,
    pub is_missing: bool,
    pub needs_review: bool,
    pub pinned_offline: bool,
}

/// A weak storage association that requires user confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchCandidateView {
    pub id: String,
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

/// Provider-neutral values submitted by the primary storage editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrimaryConfigurationView {
    pub provider: ProviderChoice,
    pub name: String,
    pub location: String,
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
    pub completeness: CompletenessView,
    pub tracks: Vec<TrackStorageView>,
    pub possible_matches: Vec<MatchCandidateView>,
    pub unknown_primary_files: Vec<UnknownPrimaryFileView>,
    pub cleanup_candidates: Vec<CleanupItemView>,
}

/// Events emitted by [`MusicManagementDashboard`].
///
/// These events are deliberately provider-independent. Runtime code maps them
/// onto provider-specific flows, jobs, and repositories in a later stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MusicManagementEvent {
    AddSource(ProviderChoice),
    SyncSource(String),
    DisconnectSource(String),
    SavePrimaryConfiguration(PrimaryConfigurationView),
    AddBackup(ProviderChoice),
    SaveBackupPolicy(BackupPolicyView),
    StartPrimaryMigration(PrimaryMigrationPlanView),
    RetryStorage(String),
    SetTrackFilter(TrackFilter),
    ToggleOfflinePin(String, bool),
    CopyToPrimary(String),
    ReviewMatch(String),
    ConfirmMatch(String),
    RejectMatch(String),
    AssociateLocalFile {
        track_id: String,
        file_path: String,
    },
    AssociateUnknownFile {
        file_id: String,
        track_query: String,
    },
    ImportUnknownFile(String),
    IgnoreUnknownFile(String),
    StartCleanupReview,
    ConfirmCleanup(Vec<String>),
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
        div { class: "music-management",
            section { class: "mm-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Sources" }
                        p { "Catalog connections are independent from where music files are stored." }
                    }
                    ProviderChooser {
                        providers: vec![ProviderChoice::LocalFolder, ProviderChoice::SoundCloud],
                        action_label: "Add source".to_string(),
                        on_choose: move |provider| on_event.call(MusicManagementEvent::AddSource(provider)),
                    }
                }
                div { class: "mm-source-grid",
                    for source in props.view.sources.iter() {
                        SourceCard { key: "{source.id}", source: source.clone(), on_event }
                    }
                }
            }

            section { class: "mm-section",
                div { class: "mm-section-heading",
                    div {
                        h2 { "Music storage" }
                        p { "Primary storage is authoritative; backups retain verified copies." }
                    }
                    button {
                        class: "mm-button secondary",
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
                                key: "{candidate.id}",
                                class: "mm-queue-item",
                                onclick: {
                                    let id = candidate.id.clone();
                                    move |_| open_match.set(Some(id.clone()))
                                },
                                strong { "{candidate.track_name}" }
                                span { "Candidate: {candidate.candidate_name}" }
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

            if let Some(id) = open_match() {
                if let Some(candidate) = props.view.possible_matches.iter().find(|candidate| candidate.id == id).cloned() {
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
    let disconnect_id = props.source.id.clone();
    let last_sync = props
        .source
        .last_sync
        .clone()
        .unwrap_or_else(|| "Never".to_string());
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
                if let Some(next_sync) = props.source.next_sync.clone() {
                    span { "Next: {next_sync}" }
                }
            }
            if let Some(progress) = props.source.progress.clone() {
                JobProgress { progress }
            }
            if let Some(warning) = props.source.warning.clone() {
                p { class: "mm-message warning", "{warning}" }
            }
            if let Some(error) = props.source.error.clone() {
                p { class: "mm-message error", "{error}" }
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
                button {
                    class: "mm-button secondary",
                    onclick: move |_| props.on_event.call(MusicManagementEvent::SyncSource(source_id.clone())),
                    "Sync now"
                }
                if props.source.connected {
                    button {
                        class: "mm-link-button danger",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::DisconnectSource(disconnect_id.clone())),
                        "Disconnect"
                    }
                }
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
    let policy_id = props.storage.id.clone();
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
                        onclick: move |_| props.on_event.call(MusicManagementEvent::SavePrimaryConfiguration(PrimaryConfigurationView {
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
                span { "{props.progress.processed}/{props.progress.total}" }
            }
            div { class: "mm-progress-track",
                div { class: "mm-progress-fill", style: "width: {percent}%;" }
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
                if props.track.is_missing {
                    button {
                        class: "mm-link-button",
                        onclick: move |_| props.on_event.call(MusicManagementEvent::CopyToPrimary(copy_id.clone())),
                        "Copy to primary"
                    }
                }
                if props.track.needs_review {
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
struct PossibleMatchDialogProps {
    candidate: MatchCandidateView,
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn PossibleMatchDialog(props: PossibleMatchDialogProps) -> Element {
    let id = props.candidate.id.clone();
    let reject_id = props.candidate.id.clone();
    let association_id = props.candidate.id.clone();
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
                        track_id: association_id.clone(),
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
    on_close: EventHandler<()>,
    on_event: EventHandler<MusicManagementEvent>,
}

#[component]
fn UnknownPrimaryFileDialog(props: UnknownPrimaryFileDialogProps) -> Element {
    let associate_id = props.file.id.clone();
    let import_id = props.file.id.clone();
    let ignore_id = props.file.id.clone();
    let mut track_query = use_signal(String::new);
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
                input {
                    r#type: "search",
                    placeholder: "Search by title or track ID",
                    value: "{track_query}",
                    oninput: move |event| track_query.set(event.value()),
                }
            }
            div { class: "mm-dialog-actions mm-wrap-actions",
                button {
                    class: "mm-button secondary",
                    disabled: track_query().trim().is_empty(),
                    onclick: move |_| props.on_event.call(MusicManagementEvent::AssociateUnknownFile {
                        file_id: associate_id.clone(),
                        track_query: track_query().trim().to_string(),
                    }),
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

    rsx! {
        Modal { title: "Configure primary storage".to_string(), on_close: props.on_close,
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
                    placeholder: if provider == ProviderChoice::LocalFolder { "/Music/rec_bag" } else { "My Drive / rec_bag" },
                    oninput: move |event| location.set(event.value()),
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
            p { class: "mm-message warning", "Changing primary configuration never deletes files from the previous location." }
            div { class: "mm-dialog-actions",
                button {
                    class: "mm-button primary",
                    disabled: name().trim().is_empty() || location().trim().is_empty(),
                    onclick: move |_| props.on_event.call(MusicManagementEvent::SavePrimaryConfiguration(PrimaryConfigurationView {
                        provider,
                        name: name().trim().to_string(),
                        location: location().trim().to_string(),
                        organize_by_artist: organize_by_artist(),
                        verify_after_copy: verify_after_copy(),
                    })),
                    "Save primary"
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
                        onclick: move |_| props.on_event.call(MusicManagementEvent::StartPrimaryMigration(PrimaryMigrationPlanView {
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
        TrackFilter::Missing => track.is_missing,
        TrackFilter::NeedsReview => track.needs_review,
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
                    total: 241,
                    current_item: Some("Night Transit — Nia Archives".to_string()),
                }),
                warning: Some("3 tracks are streaming-only and cannot be archived.".to_string()),
                error: None,
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
                badges: vec!["SoundCloud".to_string(), "Missing primary".to_string()],
                is_missing: true,
                needs_review: false,
                pinned_offline: false,
            },
            TrackStorageView {
                id: "track-float".to_string(),
                title: "Float".to_string(),
                artist: "Skee Mask".to_string(),
                badges: vec!["Verified primary".to_string(), "Backup gap".to_string()],
                is_missing: false,
                needs_review: false,
                pinned_offline: true,
            },
            TrackStorageView {
                id: "track-echo".to_string(),
                title: "Echo Park".to_string(),
                artist: "DJ Seinfeld".to_string(),
                badges: vec!["Possible match".to_string()],
                is_missing: false,
                needs_review: true,
                pinned_offline: false,
            },
        ],
        possible_matches: vec![MatchCandidateView {
            id: "match-echo".to_string(),
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
        assert!(!view.possible_matches.is_empty());
        assert!(!view.unknown_primary_files.is_empty());
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
    fn manual_association_event_preserves_the_selected_path() {
        let event = MusicManagementEvent::AssociateLocalFile {
            track_id: "track-1".to_string(),
            file_path: "/Music/track.flac".to_string(),
        };
        assert_eq!(
            event,
            MusicManagementEvent::AssociateLocalFile {
                track_id: "track-1".to_string(),
                file_path: "/Music/track.flac".to_string(),
            }
        );
    }
}
