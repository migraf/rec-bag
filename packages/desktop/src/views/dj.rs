//! Main DJ view with deck controls.

use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::Track;
use std::collections::HashSet;

use ui::{
    ContextMenu, ContextMenuAction, ContextMenuEvent, DeckControl, PlaylistBrowser, PlaylistInfo,
    SearchBar, SelectionEvent, TrackList,
};
use uuid::Uuid;

/// Main DJ interface view.
#[component]
pub fn DjView() -> Element {
    let mut app_state = use_app_state();

    // Track selection (multi-selection)
    let mut selected_track_ids: Signal<HashSet<Uuid>> = use_signal(|| HashSet::new());
    let mut last_clicked_index: Signal<Option<usize>> = use_signal(|| None);

    // Search state
    let mut search_query = use_signal(|| String::new());

    // Context menu state
    let mut context_menu_visible = use_signal(|| false);
    let mut context_menu_x = use_signal(|| 0i32);
    let mut context_menu_y = use_signal(|| 0i32);
    let mut context_menu_selected: Signal<HashSet<Uuid>> = use_signal(|| HashSet::new());

    // Browse the selected playlist, or the live library/catalog projection when no
    // playlist is selected.
    let browser_tracks = use_memo(move || app_state.dj_tracks());
    let filtered_tracks = use_memo(move || {
        let query = search_query();
        let tracks = browser_tracks();
        if query.is_empty() {
            tracks
        } else {
            let mut engine = rec_core::SearchEngine::new();
            engine
                .search(&query, &tracks)
                .into_iter()
                .map(|r| r.track)
                .collect()
        }
    });

    // Playlist handlers
    let on_create_playlist = move |name: String| {
        let id = app_state.create_playlist(name);
        app_state.selected_playlist_id.set(Some(id));
    };

    let on_select_playlist = move |id: Uuid| {
        app_state.selected_playlist_id.set(Some(id));
        selected_track_ids.set(HashSet::new());
        search_query.set(String::new());
    };

    let on_delete_playlist = move |id: Uuid| {
        app_state.delete_playlist(id);
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

    // Load track to deck
    // Load track to deck using shared state
    let load_track_to_deck = move |track_id: Uuid, deck: usize| {
        app_state.load_track_to_deck(track_id, deck);
    };

    let on_load_track = move |(track_id, deck): (Uuid, usize)| {
        load_track_to_deck(track_id, deck);
    };

    let mut on_remove_track = move |track_ids: Vec<Uuid>| {
        if let Some(playlist_id) = *app_state.selected_playlist_id.read() {
            app_state.remove_tracks_from_playlist(&track_ids, playlist_id);
        }
        // Clear selection for removed tracks
        let mut ids = selected_track_ids();
        for id in &track_ids {
            ids.remove(id);
        }
        selected_track_ids.set(ids);
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

        match action {
            ContextMenuAction::AddToPlaylist(playlist_id) => {
                let tracks: Vec<Track> = filtered_tracks()
                    .iter()
                    .filter(|t| selected.contains(&t.id))
                    .cloned()
                    .collect();
                app_state.add_tracks_to_playlist(tracks, playlist_id);
            }
            ContextMenuAction::AddToNewPlaylist => {
                let tracks: Vec<Track> = filtered_tracks()
                    .iter()
                    .filter(|t| selected.contains(&t.id))
                    .cloned()
                    .collect();
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
            ContextMenuAction::RemoveFromPlaylist => {
                on_remove_track(selected.iter().copied().collect());
            }
            ContextMenuAction::Analyze => {
                // Trigger analysis for selected tracks
                app_state.analyze_tracks(selected.iter().copied().collect());
            }
            _ => {}
        }
    };

    // Build playlist info for context menu
    let playlists = app_state.playlists.read().clone();
    let playlist_infos: Vec<PlaylistInfo> = playlists
        .iter()
        .map(|p| PlaylistInfo {
            id: p.id,
            name: p.name.clone(),
        })
        .collect();

    let track_count = filtered_tracks().len();
    let has_playlist_selected = app_state.selected_playlist_id.read().is_some();
    let browser_title = if has_playlist_selected {
        "Tracks"
    } else {
        "Library"
    };

    rsx! {
        div { class: "dj-view",
            div { class: "decks-container",
                for deck in app_state.decks() {
                    DeckControl {
                        deck_number: deck.number(),
                        state: deck.state(),
                        track: deck.track(),
                        position: deck.position(),
                        volume: deck.volume(),
                        on_play: move |_| { deck.play() },
                        on_pause: move |_| { deck.pause() },
                        on_stop: move |_| { deck.stop() },
                        on_volume_change: move |volume| { deck.set_volume(volume) },
                        on_seek: move |position| { deck.seek(position) },
                        on_file_drop: |_| {}
                    }
                }
            }

            div { class: "browser-container",
                PlaylistBrowser {
                    playlists: playlists.clone(),
                    selected_id: *app_state.selected_playlist_id.read(),
                    on_select: on_select_playlist,
                    on_create: on_create_playlist,
                    on_delete: on_delete_playlist
                }

                div { class: "track-panel",
                    // Track panel header with search
                    div { class: "track-panel-header",
                        div { class: "track-panel-title",
                            "{browser_title}"
                            span { class: "track-panel-count", "({track_count})" }
                        }
                        SearchBar {
                            value: search_query(),
                            placeholder: if has_playlist_selected { "Search playlist...".to_string() } else { "Search library...".to_string() },
                            on_change: move |q| search_query.set(q),
                            result_count: if search_query().is_empty() { None } else { Some(track_count) },
                            compact: true
                        }
                    }

                    TrackList {
                        tracks: filtered_tracks(),
                        selected_ids: selected_track_ids(),
                        on_select: on_select,
                        on_load: on_load_track,
                        on_remove: on_remove_track,
                        on_context_menu: on_context_menu,
                        deck_count: 2,
                        show_playlist_badges: false
                    }
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
