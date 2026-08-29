//! Library management view for browsing and organizing music files.

use crate::state::integration::use_integration_state;
use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::Track;
use std::collections::HashSet;
use std::path::PathBuf;
use ui::{
    ContextMenu, ContextMenuAction, ContextMenuEvent, MetadataEditor, MetadataSaveEvent,
    MusicManagementEvent, PlaylistInfo, SearchBar, SelectionEvent, SourcesPanel, TrackIssueInfo,
    TrackList,
};
use uuid::Uuid;

/// Library view for managing music files.
#[component]
pub fn Library() -> Element {
    let app_state = use_app_state();

    // Local state
    let mut sources_collapsed = use_signal(|| false);
    let mut search_query = use_signal(|| String::new());
    let mut selected_track_ids: Signal<HashSet<Uuid>> = use_signal(|| HashSet::new());
    let mut last_clicked_index: Signal<Option<usize>> = use_signal(|| None);
    let mut selected_folder: Signal<Option<PathBuf>> = use_signal(|| None);

    // Context menu state
    let mut context_menu_visible = use_signal(|| false);
    let mut context_menu_x = use_signal(|| 0i32);
    let mut context_menu_y = use_signal(|| 0i32);
    let mut context_menu_selected: Signal<HashSet<Uuid>> = use_signal(|| HashSet::new());

    // Metadata editor state
    let mut metadata_editor_visible = use_signal(|| false);
    let mut metadata_editor_track_ids: Signal<Vec<Uuid>> = use_signal(|| Vec::new());

    // Get filtered tracks based on search and folder selection
    let library_tracks = app_state.library_tracks.read().clone();
    let filtered_tracks = use_memo(move || {
        let query = search_query();
        let folder = selected_folder();

        // First filter by folder if one is selected
        let folder_filtered: Vec<Track> = if let Some(folder_path) = folder {
            library_tracks
                .iter()
                .filter(|t| t.path.starts_with(&folder_path))
                .cloned()
                .collect()
        } else {
            library_tracks.clone()
        };

        // Then filter by search query
        if query.is_empty() {
            folder_filtered
        } else {
            let mut engine = rec_core::SearchEngine::new();
            engine
                .search(&query, &folder_filtered)
                .into_iter()
                .map(|r| r.track)
                .collect()
        }
    });

    let sources = app_state.sources.read().clone();
    let playlists = app_state.playlists.read().clone();

    // Source handlers. Sources belong to the catalog, so configuring and scanning one
    // is integration work dispatched to the slice that owns its provider — the same
    // path the Music management view uses.
    let integration = use_integration_state();
    let mut add_source_integration = integration.clone();
    let mut remove_source_integration = integration.clone();
    let mut rescan_source_integration = integration.clone();
    let mut rescan_all_integration = integration.clone();

    let on_add_source = move |path: String| {
        add_source_integration.dispatch(MusicManagementEvent::AddLocalFolderSource { path });
    };

    let on_remove_source = move |source_id: Uuid| {
        remove_source_integration.dispatch(MusicManagementEvent::DisconnectSource {
            source_id: source_id.to_string(),
        });
    };

    let on_rescan_source = move |source_id: Uuid| {
        rescan_source_integration.dispatch(MusicManagementEvent::SyncSource(source_id.to_string()));
    };

    let on_rescan_all = move |_| {
        for source in app_state.sources.read().iter().filter(|s| s.enabled) {
            rescan_all_integration
                .dispatch(MusicManagementEvent::SyncSource(source.id.to_string()));
        }
    };

    let on_create_playlist_from_source = move |source_id: Uuid| {
        let Some(root) = app_state
            .sources
            .read()
            .iter()
            .find(|source| source.id == source_id)
            .and_then(|source| source.local_path().map(PathBuf::from))
        else {
            return;
        };
        let name = app_state
            .sources
            .read()
            .iter()
            .find(|source| source.id == source_id)
            .map(|source| source.name.clone())
            .unwrap_or_else(|| "Source".to_string());
        let tracks: Vec<Track> = app_state
            .library_tracks
            .read()
            .iter()
            .filter(|track| track.path.starts_with(&root))
            .cloned()
            .collect();
        if tracks.is_empty() {
            return;
        }
        let playlist_id = app_state.create_playlist(name);
        app_state.add_tracks_to_playlist(tracks, playlist_id);
    };

    // Selection handler with shift/ctrl support
    let on_select = move |event: SelectionEvent| {
        let tracks = filtered_tracks();

        if event.shift_key {
            // Shift-click: select range
            if let Some(last_idx) = last_clicked_index() {
                let start = last_idx.min(event.track_index);
                let end = last_idx.max(event.track_index);
                let range_ids: HashSet<Uuid> = tracks[start..=end.min(tracks.len() - 1)]
                    .iter()
                    .map(|t| t.id)
                    .collect();
                selected_track_ids.set(range_ids);
            } else {
                let mut ids = HashSet::new();
                ids.insert(event.track_id);
                selected_track_ids.set(ids);
            }
        } else if event.ctrl_key {
            // Ctrl-click: toggle selection
            let mut ids = selected_track_ids();
            if ids.contains(&event.track_id) {
                ids.remove(&event.track_id);
            } else {
                ids.insert(event.track_id);
            }
            selected_track_ids.set(ids);
        } else {
            // Normal click: single selection
            let mut ids = HashSet::new();
            ids.insert(event.track_id);
            selected_track_ids.set(ids);
        }
        last_clicked_index.set(Some(event.track_index));
    };

    // Load track to deck using shared state
    let load_track_to_deck = move |track_id: Uuid, deck: usize| {
        app_state.load_track_to_deck(track_id, deck);
    };

    // Context menu handler
    let on_context_menu = move |event: ContextMenuEvent| {
        context_menu_x.set(event.x);
        context_menu_y.set(event.y);
        context_menu_selected.set(event.selected_ids);
        context_menu_visible.set(true);
    };

    let on_context_action = move |action: ContextMenuAction| {
        let selected = context_menu_selected();
        let tracks: Vec<Track> = filtered_tracks()
            .iter()
            .filter(|t| selected.contains(&t.id))
            .cloned()
            .collect();

        match action {
            ContextMenuAction::AddToPlaylist(playlist_id) => {
                app_state.add_tracks_to_playlist(tracks, playlist_id);
            }
            ContextMenuAction::AddToNewPlaylist => {
                let playlist_id = app_state.create_playlist("New Playlist".to_string());
                app_state.add_tracks_to_playlist(tracks, playlist_id);
            }
            ContextMenuAction::LoadToDeck1 => {
                if let Some(track_id) = selected.iter().next() {
                    load_track_to_deck(*track_id, 1);
                }
            }
            ContextMenuAction::LoadToDeck2 => {
                if let Some(track_id) = selected.iter().next() {
                    load_track_to_deck(*track_id, 2);
                }
            }
            ContextMenuAction::FixIssues => {
                // Fix issues for all selected tracks
                for track_id in selected {
                    app_state.fix_all_for_track(track_id);
                }
            }
            ContextMenuAction::EditMetadata => {
                // Open metadata editor for selected tracks
                metadata_editor_track_ids.set(selected.iter().copied().collect());
                metadata_editor_visible.set(true);
            }
            ContextMenuAction::Analyze => {
                // Trigger analysis for selected tracks
                app_state.analyze_tracks(selected.iter().copied().collect());
            }
            _ => {}
        }
    };

    // Handler for fixing issues from the track list badge click
    let on_fix_issues = move |track_id: Uuid| {
        app_state.fix_all_for_track(track_id);
    };

    let on_load = move |(track_id, deck): (Uuid, usize)| {
        load_track_to_deck(track_id, deck);
    };

    let on_remove = move |_track_ids: Vec<Uuid>| {
        // Library tracks are not removed via this - they persist
        // This is just for selection clearing
    };

    // Build playlist info for context menu
    let playlist_infos: Vec<PlaylistInfo> = playlists
        .iter()
        .map(|p| PlaylistInfo {
            id: p.id,
            name: p.name.clone(),
        })
        .collect();

    let track_count = filtered_tracks().len();
    let has_search = !search_query().is_empty();
    let has_folder = selected_folder().is_some();

    // Build track issues map for doctor integration
    let diagnoses_by_track = app_state.diagnoses_by_track.read().clone();
    let track_issues: std::collections::HashMap<Uuid, TrackIssueInfo> = diagnoses_by_track
        .iter()
        .map(|(track_id, diagnoses)| {
            let fixable_count = diagnoses.iter().filter(|d| d.fixable).count();
            let summary = diagnoses
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            (
                *track_id,
                TrackIssueInfo {
                    count: diagnoses.len(),
                    fixable_count,
                    summary,
                },
            )
        })
        .collect();

    // Check if any selected track has fixable issues (for context menu)
    let selected_have_fixable_issues = context_menu_selected().iter().any(|id| {
        track_issues
            .get(id)
            .map(|info| info.fixable_count > 0)
            .unwrap_or(false)
    });

    // Get tracks for metadata editor
    let metadata_editor_tracks = app_state.get_tracks_by_ids(&metadata_editor_track_ids());

    // Metadata save handler
    let on_metadata_save = move |event: MetadataSaveEvent| {
        let fields = event.fields;

        // Convert MetadataFields to the format expected by update_track_metadata
        let title = fields.title;
        let artist = fields.artist.map(Some);
        let album = fields.album.map(Some);
        let genre = fields.genre.map(Some);
        let bpm = fields.bpm.map(Some);
        let key = fields.key.map(Some);
        let year = fields.year.map(Some);
        let track_number = fields.track_number.map(Some);
        let comment = fields.comment.map(Some);
        let rating = fields.rating.map(Some);

        app_state.batch_update_metadata(
            event.track_ids,
            title,
            artist,
            album,
            genre,
            bpm,
            key,
            year,
            track_number,
            comment,
            rating,
            event.write_to_file,
        );

        metadata_editor_visible.set(false);
        metadata_editor_track_ids.set(Vec::new());
    };

    // Get current folder name for display
    let folder_display = selected_folder()
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    let on_folder_select = move |path: PathBuf| {
        selected_folder.set(Some(path));
    };

    let on_show_all = move |_| {
        selected_folder.set(None);
    };

    let total_track_count = app_state.library_tracks.read().len();

    rsx! {
        div { class: "library-view",
            // Sources panel (collapsible) with folder browser
            SourcesPanel {
                sources: sources,
                collapsed: sources_collapsed(),
                on_add: on_add_source,
                on_remove: on_remove_source,
                on_rescan: on_rescan_source,
                on_rescan_all: on_rescan_all,
                on_create_playlist: on_create_playlist_from_source,
                on_toggle_collapse: move |collapsed| sources_collapsed.set(collapsed),
                on_folder_select: on_folder_select,
                on_show_all: on_show_all,
                selected_folder: selected_folder(),
                total_track_count: total_track_count
            }

            // Track panel header with search
            div { class: "track-panel-header",
                div { class: "track-panel-title",
                    if let Some(ref folder_name) = folder_display {
                        span { class: "folder-breadcrumb",
                            button {
                                class: "breadcrumb-btn",
                                onclick: move |_| selected_folder.set(None),
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
                    value: search_query(),
                    placeholder: "Search tracks...".to_string(),
                    on_change: move |q| search_query.set(q),
                    result_count: if has_search { Some(track_count) } else { None },
                    compact: true
                }
            }

            // Track list with multi-selection
            div { class: "library-tracks",
                TrackList {
                    tracks: filtered_tracks(),
                    selected_ids: selected_track_ids(),
                    on_select: on_select,
                    on_load: on_load,
                    on_remove: on_remove,
                    on_context_menu: on_context_menu,
                    deck_count: 2,
                    show_playlist_badges: true,
                    track_issues: track_issues.clone(),
                    on_fix_issues: on_fix_issues
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
                visible: context_menu_visible(),
                x: context_menu_x(),
                y: context_menu_y(),
                playlists: playlist_infos,
                selected_count: context_menu_selected().len(),
                on_action: on_context_action,
                on_close: move |_| context_menu_visible.set(false),
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
