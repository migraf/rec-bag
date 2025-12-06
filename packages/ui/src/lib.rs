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
pub use deck::{DeckControl, DeckControlProps, DeckDisplayState};

mod playlist_browser;
pub use playlist_browser::{PlaylistBrowser, PlaylistBrowserProps};

mod track_list;
pub use track_list::{
    ContextMenuEvent, PlaylistRef, SelectionEvent, TrackIssueInfo, TrackList, TrackListLegacy,
    TrackListLegacyProps, TrackListProps,
};

mod trackbar;
pub use trackbar::{Trackbar, TrackbarProps};

mod sources_panel;
pub use sources_panel::{SourcesPanel, SourcesPanelProps};

mod search_bar;
pub use search_bar::{SearchBar, SearchBarProps};

mod context_menu;
pub use context_menu::{ContextMenu, ContextMenuAction, ContextMenuProps, MenuItem, PlaylistInfo};

mod folder_browser;
pub use folder_browser::{FolderBrowser, FolderBrowserProps};

mod metadata_editor;
pub use metadata_editor::{MetadataEditor, MetadataEditorProps, MetadataFields, MetadataSaveEvent};
