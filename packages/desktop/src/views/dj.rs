//! Main DJ view with deck controls.

use crate::state::use_app_state;
use dioxus::prelude::*;
use rec_core::Track;
use std::collections::HashSet;
use std::time::Duration;
use ui::{
    ContextMenu, ContextMenuAction, ContextMenuEvent, DeckControl, DeckDisplayState,
    PlaylistBrowser, PlaylistInfo, SearchBar, SelectionEvent, TrackList,
};
use uuid::Uuid;

/// Main DJ interface view.
#[component]
pub fn DjView() -> Element {
    let mut app_state = use_app_state();

    // Deck display states (local to this view - playback state)
    let mut deck1_state = use_signal(|| DeckDisplayState::Empty);
    let mut deck2_state = use_signal(|| DeckDisplayState::Empty);
    let mut deck1_volume = use_signal(|| 1.0f32);
    let mut deck2_volume = use_signal(|| 1.0f32);
    let mut deck1_position = use_signal(|| Duration::ZERO);
    let mut deck2_position = use_signal(|| Duration::ZERO);

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

    // Get tracks for currently selected playlist with search filtering
    let playlist_tracks = use_memo(move || app_state.selected_playlist_tracks());
    let filtered_tracks = use_memo(move || {
        let query = search_query();
        let tracks = playlist_tracks();
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

    // Update deck state when tracks are loaded (from any view)
    let deck1_track = app_state.deck1_track;
    let deck2_track = app_state.deck2_track;

    use_effect(move || {
        // Update deck 1 state based on whether a track is loaded
        if deck1_track.read().is_some() {
            if deck1_state() == DeckDisplayState::Empty {
                deck1_state.set(DeckDisplayState::Stopped);
            }
        } else {
            deck1_state.set(DeckDisplayState::Empty);
        }

        // Update deck 2 state based on whether a track is loaded
        if deck2_track.read().is_some() {
            if deck2_state() == DeckDisplayState::Empty {
                deck2_state.set(DeckDisplayState::Stopped);
            }
        } else {
            deck2_state.set(DeckDisplayState::Empty);
        }
    });

    // Periodically update deck positions while playing
    use_future(move || async move {
        loop {
            // Update every 200ms for smooth display
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Use try_lock to avoid blocking UI interactions
            if let Ok(eng) = (app_state.engine)().try_lock() {
                // Update deck 1 position
                if let Ok(deck) = eng.deck(0) {
                    deck1_position.set(deck.position());
                    // Auto-update state if track finished
                    if deck1_state() == DeckDisplayState::Playing && deck.is_finished() {
                        deck1_state.set(DeckDisplayState::Stopped);
                    }
                }

                // Update deck 2 position
                if let Ok(deck) = eng.deck(1) {
                    deck2_position.set(deck.position());
                    // Auto-update state if track finished
                    if deck2_state() == DeckDisplayState::Playing && deck.is_finished() {
                        deck2_state.set(DeckDisplayState::Stopped);
                    }
                }
            }
        }
    });

    // Deck 1 controls
    let on_deck1_play = move |_| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.play(0);
            deck1_state.set(DeckDisplayState::Playing);
        }
    };

    let on_deck1_pause = move |_| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.pause(0);
            deck1_state.set(DeckDisplayState::Paused);
        }
    };

    let on_deck1_stop = move |_| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.stop(0);
            deck1_state.set(DeckDisplayState::Stopped);
        }
    };

    let on_deck1_volume = move |vol: f32| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.set_volume(0, vol);
            deck1_volume.set(vol);
        }
    };

    let on_deck1_seek = move |pos: Duration| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            if let Ok(deck) = eng.deck_mut(0) {
                let _ = deck.seek(pos);
                deck1_position.set(pos);
            }
        }
    };

    // Deck 2 controls
    let on_deck2_play = move |_| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.play(1);
            deck2_state.set(DeckDisplayState::Playing);
        }
    };

    let on_deck2_pause = move |_| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.pause(1);
            deck2_state.set(DeckDisplayState::Paused);
        }
    };

    let on_deck2_stop = move |_| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.stop(1);
            deck2_state.set(DeckDisplayState::Stopped);
        }
    };

    let on_deck2_volume = move |vol: f32| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            let _ = eng.set_volume(1, vol);
            deck2_volume.set(vol);
        }
    };

    let on_deck2_seek = move |pos: Duration| {
        if let Ok(mut eng) = (app_state.engine)().lock() {
            if let Ok(deck) = eng.deck_mut(1) {
                let _ = deck.seek(pos);
                deck2_position.set(pos);
            }
        }
    };

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
        // Find the track in filtered tracks or playlist tracks
        let track = filtered_tracks()
            .iter()
            .find(|t| t.id == track_id)
            .cloned();

        if let Some(track) = track {
            app_state.load_track_to_deck(track, deck);
        }
    };

    let on_load_track = move |(track_id, deck): (Uuid, usize)| {
        load_track_to_deck(track_id, deck);
    };

    let mut on_remove_track = move |track_ids: Vec<Uuid>| {
        if let Some(playlist_id) = *app_state.selected_playlist_id.read() {
            let mut playlists = app_state.playlists.write();
            if let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) {
                for track_id in &track_ids {
                    playlist.remove_track_by_id(*track_id);
                }
            }
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

    // Read deck tracks from shared state
    let deck1_track_value = app_state.deck1_track.read().clone();
    let deck2_track_value = app_state.deck2_track.read().clone();

    rsx! {
        div { class: "dj-view",
            div { class: "decks-container",
                DeckControl {
                    deck_number: 1,
                    state: deck1_state(),
                    track: deck1_track_value.clone(),
                    position: deck1_position(),
                    volume: deck1_volume(),
                    on_play: on_deck1_play,
                    on_pause: on_deck1_pause,
                    on_stop: on_deck1_stop,
                    on_volume_change: on_deck1_volume,
                    on_seek: on_deck1_seek,
                    on_file_drop: |_| {}
                }
                DeckControl {
                    deck_number: 2,
                    state: deck2_state(),
                    track: deck2_track_value.clone(),
                    position: deck2_position(),
                    volume: deck2_volume(),
                    on_play: on_deck2_play,
                    on_pause: on_deck2_pause,
                    on_stop: on_deck2_stop,
                    on_volume_change: on_deck2_volume,
                    on_seek: on_deck2_seek,
                    on_file_drop: |_| {}
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
                    if has_playlist_selected {
                        div { class: "track-panel-header",
                            div { class: "track-panel-title",
                                "Tracks"
                                span { class: "track-panel-count", "({track_count})" }
                            }
                            SearchBar {
                                value: search_query(),
                                placeholder: "Search playlist...".to_string(),
                                on_change: move |q| search_query.set(q),
                                result_count: if search_query().is_empty() { None } else { Some(track_count) },
                                compact: true
                            }
                        }
                    }

                    if has_playlist_selected {
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
                    } else {
                        div { class: "empty-state",
                            "Select a playlist to view tracks"
                        }
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
