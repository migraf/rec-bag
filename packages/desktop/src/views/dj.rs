//! Main DJ view with deck controls.

use crate::state::browser::{use_library_browser, BrowseScope};
use crate::state::use_app_state;
use crate::views::{load_selection_to_deck, use_context_menu};
use dioxus::prelude::*;
use ui::{
    ContextMenu, ContextMenuAction, ContextMenuEvent, DeckControl, PlaylistBrowser, PlaylistInfo,
    SearchBar, SelectionEvent, TrackList,
};
use uuid::Uuid;

/// Main DJ interface view.
#[component]
pub fn DjView() -> Element {
    let app_state = use_app_state();
    // The browser owns where this view is looking, what it is searching for, and what
    // is selected. Everything below is a gesture on it.
    let browser = use_library_browser();
    let context_menu = use_context_menu();

    // The deck refresh loop re-renders this view many times a second, so searching is
    // memoized: it re-runs when the scope, the query, or the projection changes, not
    // when a playhead moves.
    let visible = use_memo(move || browser.visible_tracks());

    let on_context_action = move |action: ContextMenuAction| match action {
        ContextMenuAction::AddToPlaylist(playlist_id) => {
            browser.add_selection_to_playlist(playlist_id);
        }
        ContextMenuAction::AddToNewPlaylist => {
            browser.add_selection_to_new_playlist("New Playlist");
        }
        ContextMenuAction::LoadToDeck1 => load_selection_to_deck(app_state, browser, 1),
        ContextMenuAction::LoadToDeck2 => load_selection_to_deck(app_state, browser, 2),
        ContextMenuAction::RemoveFromPlaylist => {
            browser.remove_from_playlist(&Vec::from_iter(browser.selected_ids()));
        }
        ContextMenuAction::Analyze => {
            app_state.analyze_tracks(browser.selected_ids().into_iter().collect());
        }
        _ => {}
    };

    let playlists = browser.playlists();
    let playlist_infos: Vec<PlaylistInfo> = playlists
        .iter()
        // A mirrored playlist belongs to its source, so nothing can be added to it here.
        .filter(|p| p.is_editable())
        .map(|p| PlaylistInfo {
            id: p.id,
            name: p.name.clone(),
        })
        .collect();

    let browsed_playlist = browser.browsed_playlist();
    let track_count = visible().len();
    let query = browser.query();
    let browser_title = if browsed_playlist.is_some() {
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
                        stream: deck.stream(),
                        loading: deck.loading(),
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
                    selected_id: browsed_playlist,
                    on_select: move |id: Uuid| browser.browse(BrowseScope::Playlist(id)),
                    on_create: move |name: String| { browser.create_playlist(name); },
                    on_delete: move |id: Uuid| browser.delete_playlist(id)
                }

                div { class: "track-panel",
                    // Track panel header with search
                    div { class: "track-panel-header",
                        div { class: "track-panel-title",
                            "{browser_title}"
                            span { class: "track-panel-count", "({track_count})" }
                        }
                        SearchBar {
                            value: query.clone(),
                            placeholder: if browsed_playlist.is_some() { "Search playlist...".to_string() } else { "Search library...".to_string() },
                            on_change: move |q| browser.search(q),
                            result_count: if query.is_empty() { None } else { Some(track_count) },
                            compact: true
                        }
                    }

                    TrackList {
                        tracks: visible(),
                        selected_ids: browser.selected_ids(),
                        on_select: move |event: SelectionEvent| browser.select(event),
                        on_load: move |(track_id, deck): (Uuid, usize)| { app_state.load_track_to_deck(track_id, deck); },
                        on_remove: move |track_ids: Vec<Uuid>| browser.remove_from_playlist(&track_ids),
                        on_context_menu: move |event: ContextMenuEvent| context_menu.open(browser, event),
                        deck_count: 2,
                        show_playlist_badges: false
                    }
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
                on_close: move |_| context_menu.close()
            }
        }
    }
}
