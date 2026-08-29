//! Views for the desktop application.

use crate::state::browser::LibraryBrowser;
use crate::state::AppState;
use dioxus::prelude::*;
use ui::ContextMenuEvent;

mod dj;
pub use dj::DjView;

mod doctor;
pub use doctor::DoctorView;

mod library;
pub use library::Library;

mod music_management;
pub use music_management::MusicManagement;

/// Where a track list's context menu is, and whether it is open.
///
/// Two views show the same menu over the same track list, so where it sits is state
/// they share the shape of rather than each keep three signals for.
#[derive(Clone, Copy)]
pub(crate) struct ContextMenuState {
    visible: Signal<bool>,
    x: Signal<i32>,
    y: Signal<i32>,
}

/// A closed context menu, owned by the view that shows one.
pub(crate) fn use_context_menu() -> ContextMenuState {
    ContextMenuState {
        visible: use_signal(|| false),
        x: use_signal(|| 0),
        y: use_signal(|| 0),
    }
}

impl ContextMenuState {
    /// Open the menu where a row was right-clicked, bringing that row into the
    /// selection the menu will act on.
    pub(crate) fn open(mut self, browser: LibraryBrowser, event: ContextMenuEvent) {
        browser.select_for_context_menu(event.track_id);
        self.x.set(event.x);
        self.y.set(event.y);
        self.visible.set(true);
    }

    pub(crate) fn close(mut self) {
        self.visible.set(false);
    }

    pub(crate) fn visible(&self) -> bool {
        *self.visible.read()
    }

    pub(crate) fn x(&self) -> i32 {
        *self.x.read()
    }

    pub(crate) fn y(&self) -> i32 {
        *self.y.read()
    }
}

/// Load the first selected track onto a deck.
///
/// A deck holds one track, so a selection of several loads the first one the browser is
/// showing — the row the user can point at — rather than an arbitrary member of the set.
pub(crate) fn load_selection_to_deck(app_state: AppState, browser: LibraryBrowser, deck: usize) {
    if let Some(track) = browser.selected_tracks().first() {
        app_state.load_track_to_deck(track.id, deck);
    }
}
