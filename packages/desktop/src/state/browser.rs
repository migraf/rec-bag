//! One view's browse over the Catalog projection.
//!
//! A [`LibraryBrowser`] owns the place a view is looking, what it is searching for, and
//! what the user has selected, and offers one method per user gesture. Views never
//! write a signal held here, so selecting, scoping, searching, and save-marking behave
//! the same wherever they are driven from.
//!
//! A browser is per view rather than global: the DJ view browses playlists and the
//! Library view browses folders, and neither should inherit the other's place. What
//! they share is this interface — and therefore one implementation of each gesture.

use crate::state::projection::CatalogProjection;
use dioxus::prelude::*;
use rec_core::{MusicSource, Playlist, SearchEngine, Track};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use ui::{PlaylistRef, SelectionEvent};
use uuid::Uuid;

/// Where a browser is looking within the projection.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum BrowseScope {
    /// Every track the Catalog holds.
    #[default]
    Everything,
    /// One canonical playlist's membership.
    Playlist(Uuid),
    /// The tracks whose local file sits under a folder.
    Folder(PathBuf),
}

/// One view's browse over the Catalog projection.
#[derive(Clone, Copy)]
pub struct LibraryBrowser {
    projection: CatalogProjection,
    scope: Signal<BrowseScope>,
    query: Signal<String>,
    selected: Signal<HashSet<Uuid>>,
    /// The row a range extends from: the last row a plain or ctrl click landed on.
    anchor: Signal<Option<usize>>,
}

impl LibraryBrowser {
    // ==================== Browsing ====================

    /// Where the browser is looking.
    pub fn scope(&self) -> BrowseScope {
        self.scope.read().clone()
    }

    /// The playlist being browsed, if the scope is a playlist.
    pub fn browsed_playlist(&self) -> Option<Uuid> {
        match self.scope() {
            BrowseScope::Playlist(id) => Some(id),
            _ => None,
        }
    }

    /// The folder being browsed, if the scope is a folder.
    pub fn browsed_folder(&self) -> Option<PathBuf> {
        match self.scope() {
            BrowseScope::Folder(path) => Some(path),
            _ => None,
        }
    }

    /// Look somewhere else.
    ///
    /// A new place starts clean: a query and a selection describe rows that are no
    /// longer on screen.
    pub fn browse(mut self, scope: BrowseScope) {
        self.scope.set(scope);
        self.query.write().clear();
        self.selected.write().clear();
        self.anchor.set(None);
    }

    /// What the browser is showing: its scope, narrowed by its query.
    ///
    /// Searching is not free, so views hold this behind a memo rather than calling it
    /// once per render.
    pub fn visible_tracks(&self) -> Vec<Track> {
        matching(&self.query(), self.scoped_tracks())
    }

    /// Every track in scope, before the query narrows it.
    fn scoped_tracks(&self) -> Vec<Track> {
        match self.scope() {
            BrowseScope::Everything => self.projection.tracks(),
            BrowseScope::Playlist(id) => self.projection.playlist_tracks(id),
            BrowseScope::Folder(path) => under(&path, self.projection.tracks()),
        }
    }

    // ==================== Searching ====================

    /// What the browser is searching for.
    pub fn query(&self) -> String {
        self.query.read().clone()
    }

    /// Search within the current scope.
    ///
    /// The anchor is forgotten because it is a row number: narrowing the list would
    /// otherwise let a later shift-click extend a range over rows that moved.
    pub fn search(mut self, query: String) {
        self.query.set(query);
        self.anchor.set(None);
    }

    // ==================== Selecting ====================

    /// The selected tracks, by identity.
    pub fn selected_ids(&self) -> HashSet<Uuid> {
        self.selected.read().clone()
    }

    /// How many tracks are selected.
    pub fn selected_count(&self) -> usize {
        self.selected.read().len()
    }

    /// The selected tracks the browser is currently showing, in the order shown.
    pub fn selected_tracks(&self) -> Vec<Track> {
        let selected = self.selected.read();
        self.visible_tracks()
            .into_iter()
            .filter(|track| selected.contains(&track.id))
            .collect()
    }

    /// Apply one click on a row, with its modifier keys.
    pub fn select(mut self, event: SelectionEvent) {
        let visible = self.visible_tracks();
        let selected = clicked(&self.selected.read(), *self.anchor.read(), &event, &visible);
        self.selected.set(selected);
        // Shift extends the range the anchor already fixed; every other click moves it.
        if !event.shift_key || self.anchor.read().is_none() {
            self.anchor.set(Some(event.track_index));
        }
    }

    /// Bring a right-clicked track into the selection.
    ///
    /// Acting on a track the user can see is not selected surprises them, so a context
    /// menu opened outside the selection replaces it.
    pub fn select_for_context_menu(mut self, track_id: Uuid) {
        if self.selected.read().contains(&track_id) {
            return;
        }
        self.selected.set(HashSet::from([track_id]));
    }

    // ==================== The projection ====================

    /// Every canonical playlist.
    pub fn playlists(&self) -> Vec<Playlist> {
        self.projection.playlists()
    }

    /// Every configured catalog source.
    pub fn sources(&self) -> Vec<MusicSource> {
        self.projection.sources()
    }

    /// How many tracks the whole projection holds, whatever the scope is showing.
    pub fn total_track_count(&self) -> usize {
        self.projection.track_count()
    }

    /// The projected tracks for `track_ids`, in the order asked for.
    pub fn tracks_by_ids(&self, track_ids: &[Uuid]) -> Vec<Track> {
        self.projection.tracks_by_ids(track_ids)
    }

    /// Which playlists hold each track, for the track list's badges.
    pub fn playlists_by_track(&self) -> HashMap<Uuid, Vec<PlaylistRef>> {
        self.projection.playlists_by_track()
    }

    // ==================== Playlists ====================

    /// Create an empty playlist and browse it.
    pub fn create_playlist(self, name: impl Into<String>) -> Uuid {
        let id = self.projection.create_playlist(name);
        self.browse(BrowseScope::Playlist(id));
        id
    }

    /// Delete a playlist, leaving it if it is the one being browsed.
    pub fn delete_playlist(self, playlist_id: Uuid) {
        self.projection.delete_playlist(playlist_id);
        if self.browsed_playlist() == Some(playlist_id) {
            self.browse(BrowseScope::Everything);
        }
    }

    /// Add the selected tracks to a playlist.
    pub fn add_selection_to_playlist(&self, playlist_id: Uuid) {
        self.projection
            .add_tracks_to_playlist(self.selected_tracks(), playlist_id);
    }

    /// Add the selected tracks to a new playlist, without leaving this one.
    pub fn add_selection_to_new_playlist(&self, name: impl Into<String>) {
        let playlist_id = self.projection.create_playlist(name);
        self.add_selection_to_playlist(playlist_id);
    }

    /// Remove tracks from the playlist being browsed.
    ///
    /// Removal persists and deselects in one gesture: a removed track that stays
    /// selected is a track later actions still act on.
    pub fn remove_from_playlist(mut self, track_ids: &[Uuid]) {
        let Some(playlist_id) = self
            .browsed_playlist()
            .filter(|id| self.projection.is_editable(*id))
        else {
            return;
        };
        self.projection
            .remove_tracks_from_playlist(track_ids, playlist_id);
        let mut selected = self.selected.write();
        for track_id in track_ids {
            selected.remove(track_id);
        }
    }
}

/// A browser over the Catalog projection, owned by the view that asks for one.
pub fn use_library_browser() -> LibraryBrowser {
    let projection = crate::state::use_app_state().projection();
    let scope = use_signal(BrowseScope::default);
    let query = use_signal(String::new);
    let selected = use_signal(HashSet::new);
    let anchor = use_signal(|| None);
    LibraryBrowser {
        projection,
        scope,
        query,
        selected,
        anchor,
    }
}

/// Which tracks one click leaves selected.
///
/// Shift extends from the anchor over what the browser is showing, ctrl toggles a
/// single row, and a plain click replaces the selection. Ranges are computed against
/// the visible rows, so a search cannot make a range reach a row the user cannot see.
fn clicked(
    selected: &HashSet<Uuid>,
    anchor: Option<usize>,
    event: &SelectionEvent,
    visible: &[Track],
) -> HashSet<Uuid> {
    if event.shift_key {
        let Some(anchor) = anchor.filter(|anchor| *anchor < visible.len()) else {
            return HashSet::from([event.track_id]);
        };
        let start = anchor.min(event.track_index);
        let end = anchor.max(event.track_index).min(visible.len() - 1);
        return visible[start..=end].iter().map(|track| track.id).collect();
    }
    if event.ctrl_key {
        let mut selected = selected.clone();
        if !selected.remove(&event.track_id) {
            selected.insert(event.track_id);
        }
        return selected;
    }
    HashSet::from([event.track_id])
}

/// The tracks whose local file sits under `folder`.
fn under(folder: &Path, tracks: Vec<Track>) -> Vec<Track> {
    tracks
        .into_iter()
        .filter(|track| track.path.starts_with(folder))
        .collect()
}

/// The tracks matching `query`, or all of them when nothing is being searched for.
///
/// This is the only place the application searches, so every browser ranks the same.
fn matching(query: &str, tracks: Vec<Track>) -> Vec<Track> {
    if query.trim().is_empty() {
        return tracks;
    }
    let mut engine = SearchEngine::new();
    engine
        .search(query, &tracks)
        .into_iter()
        .map(|result| result.track)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{a_track, titles, with_runtime, RecordingPlaylists};
    use std::sync::Arc;

    /// A browser over three tracks, two of them kept under `/music/house`.
    fn browsing<T>(
        test: impl FnOnce(LibraryBrowser, Arc<RecordingPlaylists>, Vec<Track>) -> T,
    ) -> T {
        with_runtime(|| {
            let tracks = vec![
                a_track("/music/house/one.mp3", "Blue Monday"),
                a_track("/music/house/two.mp3", "Zebra"),
                a_track("/music/techno/three.mp3", "Windowlicker"),
            ];
            let writer = Arc::new(RecordingPlaylists::default());
            let projection = CatalogProjection::new_in_scope(tracks.clone());
            projection.persist_to(writer.clone());
            let browser = LibraryBrowser {
                projection,
                scope: Signal::new_in_scope(BrowseScope::Everything, ScopeId::ROOT),
                query: Signal::new_in_scope(String::new(), ScopeId::ROOT),
                selected: Signal::new_in_scope(HashSet::new(), ScopeId::ROOT),
                anchor: Signal::new_in_scope(None, ScopeId::ROOT),
            };
            test(browser, writer, tracks)
        })
    }

    fn a_click(track: &Track, index: usize) -> SelectionEvent {
        SelectionEvent {
            track_id: track.id,
            track_index: index,
            shift_key: false,
            ctrl_key: false,
        }
    }

    fn a_shift_click(track: &Track, index: usize) -> SelectionEvent {
        SelectionEvent {
            shift_key: true,
            ..a_click(track, index)
        }
    }

    fn a_ctrl_click(track: &Track, index: usize) -> SelectionEvent {
        SelectionEvent {
            ctrl_key: true,
            ..a_click(track, index)
        }
    }

    // ==================== Browsing ====================

    #[test]
    fn an_unscoped_browser_shows_the_whole_projection() {
        browsing(|browser, _, _| {
            assert_eq!(
                titles(&browser.visible_tracks()),
                ["Blue Monday", "Zebra", "Windowlicker"]
            );
        })
    }

    #[test]
    fn a_playlist_scope_shows_that_playlist_rather_than_everything() {
        browsing(|browser, _, tracks| {
            let playlist_id = browser.create_playlist("Set");
            browser
                .projection
                .add_tracks_to_playlist(vec![tracks[2].clone()], playlist_id);

            browser.browse(BrowseScope::Playlist(playlist_id));

            assert_eq!(titles(&browser.visible_tracks()), ["Windowlicker"]);
        })
    }

    #[test]
    fn a_folder_scope_shows_only_the_tracks_kept_under_it() {
        browsing(|browser, _, _| {
            browser.browse(BrowseScope::Folder(PathBuf::from("/music/house")));

            assert_eq!(titles(&browser.visible_tracks()), ["Blue Monday", "Zebra"]);
        })
    }

    /// The query narrows what the scope already chose, never the whole projection.
    #[test]
    fn a_query_searches_within_the_scope() {
        browsing(|browser, _, _| {
            browser.browse(BrowseScope::Folder(PathBuf::from("/music/house")));

            browser.search("windowlicker".to_string());

            assert!(browser.visible_tracks().is_empty());
        })
    }

    #[test]
    fn a_query_keeps_only_matching_tracks() {
        browsing(|browser, _, _| {
            browser.search("monday".to_string());

            assert_eq!(titles(&browser.visible_tracks()), ["Blue Monday"]);
        })
    }

    #[test]
    fn a_blank_query_shows_everything_in_scope() {
        browsing(|browser, _, _| {
            browser.search("   ".to_string());

            assert_eq!(browser.visible_tracks().len(), 3);
        })
    }

    #[test]
    fn browsing_elsewhere_starts_from_a_clean_query_and_selection() {
        browsing(|browser, _, tracks| {
            browser.search("monday".to_string());
            browser.select(a_click(&tracks[0], 0));

            browser.browse(BrowseScope::Folder(PathBuf::from("/music/techno")));

            assert_eq!(browser.query(), "");
            assert!(browser.selected_ids().is_empty());
        })
    }

    // ==================== Selecting ====================

    #[test]
    fn a_plain_click_replaces_the_selection() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[0], 0));

            browser.select(a_click(&tracks[2], 2));

            assert_eq!(browser.selected_ids(), HashSet::from([tracks[2].id]));
        })
    }

    #[test]
    fn a_ctrl_click_toggles_one_track_without_disturbing_the_rest() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[0], 0));

            browser.select(a_ctrl_click(&tracks[2], 2));
            assert_eq!(
                browser.selected_ids(),
                HashSet::from([tracks[0].id, tracks[2].id])
            );

            browser.select(a_ctrl_click(&tracks[2], 2));
            assert_eq!(browser.selected_ids(), HashSet::from([tracks[0].id]));
        })
    }

    #[test]
    fn a_shift_click_selects_the_range_from_the_anchor() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[2], 2));

            browser.select(a_shift_click(&tracks[0], 0));

            assert_eq!(
                browser.selected_ids(),
                HashSet::from([tracks[0].id, tracks[1].id, tracks[2].id])
            );
        })
    }

    /// The anchor stays put across shift-clicks, so widening and narrowing a range both
    /// extend from the row the user actually started on.
    #[test]
    fn a_second_shift_click_still_extends_from_the_first_anchor() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[0], 0));
            browser.select(a_shift_click(&tracks[2], 2));

            browser.select(a_shift_click(&tracks[1], 1));

            assert_eq!(
                browser.selected_ids(),
                HashSet::from([tracks[0].id, tracks[1].id])
            );
        })
    }

    #[test]
    fn a_shift_click_with_nothing_to_extend_from_selects_one_track() {
        browsing(|browser, _, tracks| {
            browser.select(a_shift_click(&tracks[1], 1));

            assert_eq!(browser.selected_ids(), HashSet::from([tracks[1].id]));
        })
    }

    /// A row number means nothing once the visible rows change, so searching drops the
    /// anchor rather than letting a range reach rows the user cannot see.
    #[test]
    fn searching_forgets_the_row_a_range_would_extend_from() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[2], 2));

            browser.search("monday".to_string());
            browser.select(a_shift_click(&tracks[0], 0));

            assert_eq!(browser.selected_ids(), HashSet::from([tracks[0].id]));
        })
    }

    /// The selection is what the next gesture acts on, so it is reported in the order
    /// the user sees rather than in whatever order a set yields.
    #[test]
    fn the_selected_tracks_are_the_visible_ones_in_the_order_shown() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[2], 2));
            browser.select(a_ctrl_click(&tracks[0], 0));

            assert_eq!(
                titles(&browser.selected_tracks()),
                ["Blue Monday", "Windowlicker"]
            );

            browser.search("monday".to_string());
            assert_eq!(titles(&browser.selected_tracks()), ["Blue Monday"]);
        })
    }

    #[test]
    fn a_context_menu_outside_the_selection_takes_over_the_selection() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[0], 0));

            browser.select_for_context_menu(tracks[2].id);

            assert_eq!(browser.selected_ids(), HashSet::from([tracks[2].id]));
        })
    }

    #[test]
    fn a_context_menu_inside_the_selection_leaves_it_alone() {
        browsing(|browser, _, tracks| {
            browser.select(a_click(&tracks[0], 0));
            browser.select(a_ctrl_click(&tracks[1], 1));

            browser.select_for_context_menu(tracks[1].id);

            assert_eq!(
                browser.selected_ids(),
                HashSet::from([tracks[0].id, tracks[1].id])
            );
        })
    }

    // ==================== Playlists ====================

    #[test]
    fn a_new_playlist_is_saved_and_becomes_the_place_being_browsed() {
        browsing(|browser, writer, _| {
            let playlist_id = browser.create_playlist("Set");

            assert_eq!(browser.browsed_playlist(), Some(playlist_id));
            assert_eq!(writer.last_saved(playlist_id), Some(Vec::new()));
        })
    }

    #[test]
    fn adding_the_selection_to_a_playlist_saves_the_new_membership() {
        browsing(|browser, writer, tracks| {
            let playlist_id = browser.create_playlist("Set");
            browser.browse(BrowseScope::Everything);
            browser.select(a_click(&tracks[0], 0));
            browser.select(a_ctrl_click(&tracks[1], 1));

            browser.add_selection_to_playlist(playlist_id);

            assert_eq!(
                writer.last_saved(playlist_id),
                Some(vec!["Blue Monday".to_string(), "Zebra".to_string()])
            );
        })
    }

    /// Removing from a playlist has to persist and deselect together: a removed track
    /// left selected is a track the next gesture still acts on.
    #[test]
    fn removing_from_the_browsed_playlist_saves_it_and_drops_the_selection() {
        browsing(|browser, writer, tracks| {
            let playlist_id = browser.create_playlist("Set");
            browser
                .projection
                .add_tracks_to_playlist(tracks.clone(), playlist_id);
            browser.select(a_click(&tracks[0], 0));
            browser.select(a_ctrl_click(&tracks[1], 1));

            browser.remove_from_playlist(&[tracks[0].id]);

            assert_eq!(
                writer.last_saved(playlist_id),
                Some(vec!["Zebra".to_string(), "Windowlicker".to_string()])
            );
            assert_eq!(titles(&browser.visible_tracks()), ["Zebra", "Windowlicker"]);
            assert_eq!(browser.selected_ids(), HashSet::from([tracks[1].id]));
        })
    }

    #[test]
    fn removing_outside_a_playlist_scope_changes_nothing() {
        browsing(|browser, _, tracks| {
            let playlist_id = browser.create_playlist("Set");
            browser
                .projection
                .add_tracks_to_playlist(tracks.clone(), playlist_id);
            browser.browse(BrowseScope::Everything);

            browser.remove_from_playlist(&[tracks[0].id]);

            assert_eq!(browser.playlists()[0].tracks.len(), 3);
        })
    }

    #[test]
    fn deleting_the_playlist_being_browsed_returns_to_everything() {
        browsing(|browser, writer, _| {
            let playlist_id = browser.create_playlist("Set");

            browser.delete_playlist(playlist_id);

            assert_eq!(browser.scope(), BrowseScope::Everything);
            assert_eq!(writer.deleted(), vec![playlist_id]);
            assert!(browser.playlists().is_empty());
        })
    }

    #[test]
    fn deleting_another_playlist_leaves_the_browser_where_it_is() {
        browsing(|browser, _, _| {
            let other = browser.create_playlist("Other");
            let browsed = browser.create_playlist("Set");

            browser.delete_playlist(other);

            assert_eq!(browser.browsed_playlist(), Some(browsed));
        })
    }
}
