//! This crate contains all shared UI for the workspace.

use dioxus::prelude::*;

/// Base CSS with theme variables. Import this in your app's root component.
pub const BASE_CSS: Asset = asset!("/assets/base.css");

mod hero;
pub use hero::Hero;

mod navbar;
pub use navbar::Navbar;

mod echo;
pub use echo::Echo;

mod deck;
pub use deck::{DeckControl, DeckControlProps, DeckDisplayState, StreamCredit};

mod knob;
pub use knob::{Knob, KnobProps};

mod mixer;
pub use mixer::{Mixer, MixerProps};

mod playlist_browser;
pub use playlist_browser::{PlaylistBrowser, PlaylistBrowserProps};

mod track_list;
pub use track_list::{
    ContextMenuEvent, PlaylistRef, SelectionEvent, TrackIssueInfo, TrackList, TrackListLegacy,
    TrackListLegacyProps, TrackListProps,
};

mod trackbar;
pub use trackbar::{BeatGrid, FrequencyWaveform, Trackbar, TrackbarProps};

mod sources_panel;
pub use sources_panel::{SourcesPanel, SourcesPanelProps};

mod music_management;
pub use music_management::{
    mock_music_management_view, BackupPolicyView, CleanupItemView, CompletenessView,
    ExternalChangeView, FailedReconciliationView, HealthTone, JobProgressView, MatchCandidateView,
    MusicManagementDashboard, MusicManagementDashboardProps, MusicManagementEvent,
    MusicManagementMock, MusicManagementView, PlaylistCompletenessView, PrimaryConfigurationView,
    PrimaryMetadataUpdateView, PrimaryMigrationPlanView, ProviderChoice,
    SoundCloudConfigurationView, SourceCollectionView, SourceView, StorageRoleView, StorageView,
    TrackFilter, TrackStorageStatusView, TrackStorageView, UnknownPrimaryFileView,
};

mod search_bar;
pub use search_bar::{SearchBar, SearchBarProps};

mod context_menu;
pub use context_menu::{ContextMenu, ContextMenuAction, ContextMenuProps, MenuItem, PlaylistInfo};

mod folder_browser;
pub use folder_browser::{FolderBrowser, FolderBrowserProps};

mod metadata_editor;
pub use metadata_editor::{MetadataEditor, MetadataEditorProps, MetadataFields, MetadataSaveEvent};

mod playback_bar;
pub use playback_bar::{DeckInfo, PlaybackBar, PlaybackBarProps};
