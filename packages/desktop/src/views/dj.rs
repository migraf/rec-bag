//! Main DJ view with deck controls.

use crate::state::{spawn_waveform_analysis, use_app_state};
use dioxus::prelude::*;
use rec_core::Track;
use std::time::Duration;
use ui::{DeckControl, DeckDisplayState, PlaylistBrowser, TrackList};
use uuid::Uuid;

/// Main DJ interface view.
#[component]
pub fn DjView() -> Element {
    let mut app_state = use_app_state();

    // Deck states (local to this view)
    let mut deck1_state = use_signal(|| DeckDisplayState::Empty);
    let mut deck2_state = use_signal(|| DeckDisplayState::Empty);
    let mut deck1_track: Signal<Option<Track>> = use_signal(|| None);
    let mut deck2_track: Signal<Option<Track>> = use_signal(|| None);
    let mut deck1_volume = use_signal(|| 1.0f32);
    let mut deck2_volume = use_signal(|| 1.0f32);
    let mut deck1_position = use_signal(|| Duration::ZERO);
    let mut deck2_position = use_signal(|| Duration::ZERO);

    // Track selection (local to this view)
    let mut selected_track_id: Signal<Option<Uuid>> = use_signal(|| None);

    // Get tracks for currently selected playlist
    let current_tracks = use_memo(move || app_state.selected_playlist_tracks());

    // Watch waveform cache and update deck tracks when waveforms complete
    let waveform_cache = app_state.waveform_cache;
    use_effect(move || {
        let cache = waveform_cache.read();
        
        // Update deck 1 track if waveform became available
        if let Some(track) = deck1_track() {
            if !track.has_waveform() {
                if let Some(peaks) = cache.get(&track.id) {
                    let mut updated_track = track.clone();
                    updated_track.set_waveform(peaks.clone());
                    deck1_track.set(Some(updated_track));
                }
            }
        }
        
        // Update deck 2 track if waveform became available
        if let Some(track) = deck2_track() {
            if !track.has_waveform() {
                if let Some(peaks) = cache.get(&track.id) {
                    let mut updated_track = track.clone();
                    updated_track.set_waveform(peaks.clone());
                    deck2_track.set(Some(updated_track));
                }
            }
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
        selected_track_id.set(None);
    };

    let on_delete_playlist = move |id: Uuid| {
        app_state.delete_playlist(id);
    };

    // Track handlers
    let on_select_track = move |id: Uuid| {
        selected_track_id.set(Some(id));
    };

    let on_load_track = move |(track_id, deck): (Uuid, usize)| {
        // Find the track
        let track = current_tracks()
            .iter()
            .find(|t| t.id == track_id)
            .cloned();

        if let Some(mut track) = track {
            let track_path = track.path.clone();
            let needs_waveform = !track.has_waveform();

            // Check if waveform is in cache (from previous analysis)
            if needs_waveform {
                if let Some(peaks) = app_state.get_waveform(track_id) {
                    track.set_waveform(peaks);
                }
            }

            // Load track immediately (don't wait for waveform)
            if let Ok(mut eng) = (app_state.engine)().lock() {
                let deck_idx = deck - 1; // Convert to 0-indexed
                if eng.load_track(deck_idx, track.clone()).is_ok() {
                    if deck == 1 {
                        deck1_track.set(Some(track.clone()));
                        deck1_state.set(DeckDisplayState::Stopped);
                    } else {
                        deck2_track.set(Some(track.clone()));
                        deck2_state.set(DeckDisplayState::Stopped);
                    }
                }
            }

            // Spawn background waveform analysis if needed
            if needs_waveform && app_state.get_waveform(track_id).is_none() {
                spawn_waveform_analysis(app_state, track_id, track_path);
            }
        }
    };

    let on_remove_track = move |track_id: Uuid| {
        if let Some(playlist_id) = *app_state.selected_playlist_id.read() {
            let mut playlists = app_state.playlists.write();
            if let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) {
                playlist.remove_track_by_id(track_id);
            }
        }
    };

    rsx! {
        div { class: "dj-view",
            div { class: "decks-container",
                DeckControl {
                    deck_number: 1,
                    state: deck1_state(),
                    track: deck1_track(),
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
                    track: deck2_track(),
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
                    playlists: (app_state.playlists)(),
                    selected_id: (app_state.selected_playlist_id)(),
                    on_select: on_select_playlist,
                    on_create: on_create_playlist,
                    on_delete: on_delete_playlist
                }

                div { class: "track-panel",
                    if (app_state.selected_playlist_id)().is_some() {
                        TrackList {
                            tracks: current_tracks(),
                            selected_id: selected_track_id(),
                            on_select: on_select_track,
                            on_load: on_load_track,
                            on_remove: on_remove_track,
                            deck_count: 2
                        }
                    } else {
                        div { class: "empty-state",
                            "Select a playlist to view tracks"
                        }
                    }
                }
            }
        }
    }
}
