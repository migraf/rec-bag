//! Library management view for browsing and organizing music files.

use crate::state::browser::{use_library_browser, BrowseScope};
use crate::state::integration::use_integration_state;
use crate::state::metadata::MetadataEdit;
use crate::state::use_app_state;
use crate::views::{load_selection_to_deck, use_context_menu};
use dioxus::prelude::*;
use std::path::PathBuf;
use ui::{
    ContextMenu, ContextMenuAction, ContextMenuEvent, MetadataEditor, MetadataSaveEvent,
    MusicManagementEvent, PlaylistInfo, SearchBar, SelectionEvent, SourcesPanel, TrackList,
};
use uuid::Uuid;

/// Library view for managing music files.
#[component]
pub fn Library() -> Element {
    let app_state = use_app_state();
    // The browser owns where this view is looking, what it is searching for, and what
    // is selected. Everything below is a gesture on it.
    let browser = use_library_browser();
    let diagnostics = app_state.diagnostics();

    let context_menu = use_context_menu();
    let mut sources_collapsed = use_signal(|| false);

    // Metadata editor state
    let mut metadata_editor_visible = use_signal(|| false);
    let mut metadata_editor_track_ids: Signal<Vec<Uuid>> = use_signal(Vec::new);

    // Searching is not free, so it re-runs when the scope, the query, or the projection
    // changes rather than on every render.
    let visible = use_memo(move || browser.visible_tracks());
    let sources = browser.sources();

    // Source handlers. Sources belong to the catalog, so configuring and scanning one
    // is integration work dispatched to the slice that owns its provider — the same
    // path the Music management view uses.
    let integration = use_integration_state();
    let mut add_source_integration = integration.clone();
    let mut remove_source_integration = integration.clone();
    let mut rescan_source_integration = integration.clone();
    let mut rescan_all_integration = integration.clone();

    let on_rescan_all = move |_| {
        for source in browser.sources().iter().filter(|s| s.enabled) {
            rescan_all_integration
                .dispatch(MusicManagementEvent::SyncSource(source.id.to_string()));
        }
    };

    let on_context_action = move |action: ContextMenuAction| {
        let selected = browser.selected_ids();
        match action {
            ContextMenuAction::AddToPlaylist(playlist_id) => {
                browser.add_selection_to_playlist(playlist_id);
            }
            ContextMenuAction::AddToNewPlaylist => {
                browser.add_selection_to_new_playlist("New Playlist");
            }
            ContextMenuAction::LoadToDeck1 => load_selection_to_deck(app_state, browser, 1),
            ContextMenuAction::LoadToDeck2 => load_selection_to_deck(app_state, browser, 2),
            ContextMenuAction::FixIssues => {
                for track_id in selected {
                    diagnostics.fix_all_for_track(track_id);
                }
            }
            ContextMenuAction::EditMetadata => {
                metadata_editor_track_ids.set(selected.into_iter().collect());
                metadata_editor_visible.set(true);
            }
            ContextMenuAction::Analyze => {
                app_state.analyze_tracks(selected.into_iter().collect());
            }
            _ => {}
        }
    };

    let playlist_infos: Vec<PlaylistInfo> = browser
        .playlists()
        .iter()
        // A mirrored playlist belongs to its source, so nothing can be added to it here.
        .filter(|p| p.is_editable())
        .map(|p| PlaylistInfo {
            id: p.id,
            name: p.name.clone(),
        })
        .collect();

    let track_count = visible().len();
    let query = browser.query();
    let has_search = !query.is_empty();
    let browsed_folder = browser.browsed_folder();
    let has_folder = browsed_folder.is_some();

    // Doctor badges for the track list.
    let track_issues = diagnostics.issues_by_track();
    let selected_have_fixable_issues = browser.selected_ids().iter().any(|id| {
        track_issues
            .get(id)
            .map(|info| info.fixable_count > 0)
            .unwrap_or(false)
    });

    let metadata_editor_tracks = browser.tracks_by_ids(&metadata_editor_track_ids());

    let on_metadata_save = move |event: MetadataSaveEvent| {
        let edit = MetadataEdit::from_editor(event.fields, event.write_to_file);
        app_state.update_metadata(&event.track_ids, &edit);

        metadata_editor_visible.set(false);
        metadata_editor_track_ids.set(Vec::new());
    };

    // Get current folder name for display
    let folder_display = browsed_folder
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    let total_track_count = browser.total_track_count();

    rsx! {
        div { class: "library-view",
            // Sources panel (collapsible) with folder browser
            SourcesPanel {
                sources: sources,
                collapsed: sources_collapsed(),
                on_add: move |path: String| {
                    add_source_integration.dispatch(MusicManagementEvent::AddLocalFolderSource { path });
                },
                on_remove: move |source_id: Uuid| {
                    remove_source_integration.dispatch(MusicManagementEvent::DisconnectSource {
                        source_id: source_id.to_string(),
                    });
                },
                on_rescan: move |source_id: Uuid| {
                    rescan_source_integration.dispatch(MusicManagementEvent::SyncSource(source_id.to_string()));
                },
                on_rescan_all: on_rescan_all,
                on_create_playlist: move |source_id: Uuid| {
                    spawn(async move {
                        app_state.create_playlist_from_source(source_id).await;
                    });
                },
                on_toggle_collapse: move |collapsed| sources_collapsed.set(collapsed),
                on_folder_select: move |path: PathBuf| browser.browse(BrowseScope::Folder(path)),
                on_show_all: move |_| browser.browse(BrowseScope::Everything),
                selected_folder: browsed_folder.clone(),
                total_track_count: total_track_count
            }

            // Track panel header with search
            div { class: "track-panel-header",
                div { class: "track-panel-title",
                    if let Some(ref folder_name) = folder_display {
                        span { class: "folder-breadcrumb",
                            button {
                                class: "breadcrumb-btn",
                                onclick: move |_| browser.browse(BrowseScope::Everything),
                                "Library"
                            }
                            span { class: "breadcrumb-sep", "/" }
                            span { class: "breadcrumb-current", "{folder_name}" }
                        }
                    } else {
                        "Library"
                    }
                    span { class: "track-panel-count", "({track_count})" }
                }
                SearchBar {
                    value: query,
                    placeholder: "Search tracks...".to_string(),
                    on_change: move |q| browser.search(q),
                    result_count: if has_search { Some(track_count) } else { None },
                    compact: true
                }
            }

            // Track list with multi-selection
            div { class: "library-tracks",
                TrackList {
                    tracks: visible(),
                    selected_ids: browser.selected_ids(),
                    on_select: move |event: SelectionEvent| browser.select(event),
                    on_load: move |(track_id, deck): (Uuid, usize)| { app_state.load_track_to_deck(track_id, deck); },
                    // The library keeps its tracks; only a playlist's membership is
                    // something a row can be removed from.
                    on_remove: move |_track_ids: Vec<Uuid>| {},
                    on_context_menu: move |event: ContextMenuEvent| context_menu.open(browser, event),
                    deck_count: 2,
                    show_playlist_badges: true,
                    playlists_by_track: browser.playlists_by_track(),
                    track_issues: track_issues.clone(),
                    on_fix_issues: move |track_id: Uuid| { diagnostics.fix_all_for_track(track_id); }
                }
            }

            if track_count == 0 && !has_search && !has_folder {
                div { class: "empty-state",
                    p { "No tracks in library." }
                    p { "Add a music source folder above to scan for music files." }
                    p { "Supported formats: MP3, WAV, FLAC, OGG, M4A, AAC" }
                }
            }

            if track_count == 0 && !has_search && has_folder {
                div { class: "empty-state",
                    p { "No audio files in this folder." }
                    p { "Select a different folder or browse subfolders." }
                }
            }

            if track_count == 0 && has_search {
                div { class: "empty-state",
                    p { "No tracks match your search." }
                }
            }

            // Context menu
            ContextMenu {
                visible: context_menu.visible(),
                x: context_menu.x(),
                y: context_menu.y(),
                playlists: playlist_infos,
                selected_count: browser.selected_count(),
                on_action: on_context_action,
                on_close: move |_| context_menu.close(),
                has_fixable_issues: selected_have_fixable_issues
            }

            // Metadata editor modal
            MetadataEditor {
                visible: metadata_editor_visible(),
                tracks: metadata_editor_tracks,
                on_close: move |_| {
                    metadata_editor_visible.set(false);
                    metadata_editor_track_ids.set(Vec::new());
                },
                on_save: on_metadata_save
            }
        }
    }
}
