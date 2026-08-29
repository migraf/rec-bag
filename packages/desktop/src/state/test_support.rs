//! Shared fixtures for the tests of state built out of signals.

use crate::state::projection::PlaylistWriter;
use dioxus::prelude::*;
use rec_core::{Playlist, Track};
use std::path::PathBuf;
use std::sync::Mutex;
use uuid::Uuid;

/// Run `test` inside a Dioxus runtime, so it can create, read, and write signals.
///
/// A signal is owned by a scope, and a scope only exists inside a running runtime, so
/// state that holds signals cannot be constructed — let alone exercised — without one.
pub fn with_runtime<T>(test: impl FnOnce() -> T) -> T {
    fn app() -> Element {
        rsx! { "" }
    }
    let mut dom = VirtualDom::new(app);
    dom.rebuild_in_place();
    dom.in_runtime(test)
}

/// A playlist writer that records what it was asked to persist, so a gesture that edits
/// the projection without saving it fails a test rather than a user's playlist.
#[derive(Default)]
pub struct RecordingPlaylists {
    saved: Mutex<Vec<Playlist>>,
    deleted: Mutex<Vec<Uuid>>,
}

impl RecordingPlaylists {
    /// The membership last persisted for `playlist_id`, by track title.
    pub fn last_saved(&self, playlist_id: Uuid) -> Option<Vec<String>> {
        self.saved
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|playlist| playlist.id == playlist_id)
            .map(|playlist| titles(&playlist.tracks))
    }

    pub fn deleted(&self) -> Vec<Uuid> {
        self.deleted.lock().unwrap().clone()
    }
}

impl PlaylistWriter for RecordingPlaylists {
    fn save(&self, playlist: Playlist) {
        self.saved.lock().unwrap().push(playlist);
    }

    fn delete(&self, playlist_id: Uuid) {
        self.deleted.lock().unwrap().push(playlist_id);
    }
}

/// A track kept at `path`, titled `title`.
pub fn a_track(path: &str, title: &str) -> Track {
    let mut track = Track::new_without_metadata(PathBuf::from(path));
    track.title = title.to_string();
    track
}

/// The titles of `tracks`, in order — what most assertions here are actually about.
pub fn titles(tracks: &[Track]) -> Vec<String> {
    tracks.iter().map(|track| track.title.clone()).collect()
}
