//! Library management view for browsing and organizing music files.

use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::Track;
use std::collections::HashSet;
use ui::{
    ContextMenu, ContextMenuAction, ContextMenuEvent, PlaylistInfo, SearchBar, SelectionEvent,
    SourcesPanel, TrackList,
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

    // Context menu state
    let mut context_menu_visible = use_signal(|| false);
    let mut context_menu_x = use_signal(|| 0i32);
    let mut context_menu_y = use_signal(|| 0i32);
    let mut context_menu_selected: Signal<HashSet<Uuid>> = use_signal(|| HashSet::new());

    // Get filtered tracks based on search
    let library_tracks = app_state.library_tracks.read().clone();
    let filtered_tracks = use_memo(move || {
        let query = search_query();
        if query.is_empty() {
            library_tracks.clone()
        } else {
            let mut engine = rec_core::SearchEngine::new();
            engine
                .search(&query, &library_tracks)
                .into_iter()
                .map(|r| r.track)
                .collect()
        }
    });

    let sources = app_state.sources.read().clone();
    let playlists = app_state.playlists.read().clone();

    // Source handlers
    let on_add_source = move |path: String| {
        let app = app_state;
        spawn(async move {
            app.add_source(path).await;
        });
    };

    let on_remove_source = move |source_id: Uuid| {
        let app = app_state;
        spawn(async move {
            app.remove_source(source_id).await;
        });
    };

    let on_rescan_source = move |source_id: Uuid| {
        let app = app_state;
        spawn(async move {
            app.rescan_source(source_id).await;
        });
    };

    let on_rescan_all = move |_| {
        let app = app_state;
        spawn(async move {
            app.rescan_all_sources().await;
        });
    };

    let on_create_playlist_from_source = move |source_id: Uuid| {
        app_state.create_playlist_from_source(source_id);
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
        // Find the track
        let track = filtered_tracks()
            .iter()
            .find(|t| t.id == track_id)
            .cloned();

        if let Some(track) = track {
            app_state.load_track_to_deck(track, deck);
        }
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
            _ => {}
        }
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

    rsx! {
        div { class: "library-view",
            // Sources panel (collapsible)
            SourcesPanel {
                sources: sources,
                collapsed: sources_collapsed(),
                on_add: on_add_source,
                on_remove: on_remove_source,
                on_rescan: on_rescan_source,
                on_rescan_all: on_rescan_all,
                on_create_playlist: on_create_playlist_from_source,
                on_toggle_collapse: move |collapsed| sources_collapsed.set(collapsed)
            }

            // Track panel header with search
            div { class: "track-panel-header",
                div { class: "track-panel-title",
                    "Library"
                    span { class: "track-panel-count", "({track_count} tracks)" }
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
                    show_playlist_badges: true
                }
            }

            if track_count == 0 && !has_search {
                div { class: "empty-state",
                    p { "No tracks in library." }
                    p { "Add a music source folder above to scan for music files." }
                    p { "Supported formats: MP3, WAV, FLAC, OGG, M4A, AAC" }
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
                on_close: move |_| context_menu_visible.set(false)
            }
        }
    }
}
