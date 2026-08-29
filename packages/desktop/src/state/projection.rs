//! The live Catalog projection every view browses.
//!
//! Everything persistent lives in the Catalog; this is the reactive reading of it that
//! the running application renders. One projected track is shown in several places at
//! once — the library, every playlist holding it, and any deck playing it — so it is
//! edited through this handle rather than by writing the signals it holds.
//!
//! Views do not browse the projection directly. They go through a
//! [`LibraryBrowser`](super::browser::LibraryBrowser), which scopes it.

use dioxus::prelude::*;
use rec_core::{MusicSource, Playlist, Track};
use std::collections::HashMap;
use std::sync::Arc;
use ui::PlaylistRef;
use uuid::Uuid;

/// Where playlist edits are persisted.
///
/// The running application writes through to the Catalog. Tests substitute a recorder,
/// so playlist choreography can be exercised without a database: a playlist edited in
/// the projection but never handed to a writer is exactly the bug this seam exists to
/// catch.
pub trait PlaylistWriter: Send + Sync {
    /// Persist a playlist's current name and membership.
    fn save(&self, playlist: Playlist);

    /// Forget a playlist entirely.
    fn delete(&self, playlist_id: Uuid);
}

/// The live Catalog projection.
#[derive(Clone, Copy)]
pub struct CatalogProjection {
    tracks: Signal<Vec<Track>>,
    playlists: Signal<Vec<Playlist>>,
    sources: Signal<Vec<MusicSource>>,
    playlist_writer: Signal<Option<Arc<dyn PlaylistWriter>>>,
}

impl CatalogProjection {
    /// An empty projection, before the integration runtime has opened the Catalog.
    pub fn new() -> Self {
        Self {
            tracks: Signal::new(Vec::new()),
            playlists: Signal::new(Vec::new()),
            sources: Signal::new(Vec::new()),
            playlist_writer: Signal::new(None),
        }
    }

    /// Install the writer that playlist edits are persisted through.
    pub fn persist_to(mut self, writer: Arc<dyn PlaylistWriter>) {
        self.playlist_writer.set(Some(writer));
    }

    /// Replace the whole projection with what the Catalog now holds.
    pub fn replace(
        mut self,
        tracks: Vec<Track>,
        playlists: Vec<Playlist>,
        sources: Vec<MusicSource>,
    ) {
        self.playlists.set(playlists);
        self.sources.set(sources);
        self.tracks.set(tracks);
    }

    /// Every projected track.
    pub fn tracks(&self) -> Vec<Track> {
        self.tracks.read().clone()
    }

    /// How many tracks the projection holds.
    pub fn track_count(&self) -> usize {
        self.tracks.read().len()
    }

    /// One projected track, by canonical identity.
    pub fn track(&self, track_id: Uuid) -> Option<Track> {
        self.tracks
            .read()
            .iter()
            .find(|t| t.id == track_id)
            .cloned()
    }

    /// The projected tracks for `track_ids`, in the order asked for.
    pub fn tracks_by_ids(&self, track_ids: &[Uuid]) -> Vec<Track> {
        let tracks = self.tracks.read();
        track_ids
            .iter()
            .filter_map(|id| tracks.iter().find(|t| t.id == *id).cloned())
            .collect()
    }

    /// Every canonical playlist.
    pub fn playlists(&self) -> Vec<Playlist> {
        self.playlists.read().clone()
    }

    /// One playlist's membership.
    pub fn playlist_tracks(&self, playlist_id: Uuid) -> Vec<Track> {
        self.playlists
            .read()
            .iter()
            .find(|p| p.id == playlist_id)
            .map(|p| p.tracks.clone())
            .unwrap_or_default()
    }

    /// Every configured catalog source.
    pub fn sources(&self) -> Vec<MusicSource> {
        self.sources.read().clone()
    }

    /// Which playlists hold each track, for the track list's badges.
    ///
    /// Built in one pass rather than asked per row: a lookup that costs one walk over
    /// the playlists is a lookup a list of five hundred rows can afford.
    pub fn playlists_by_track(&self) -> HashMap<Uuid, Vec<PlaylistRef>> {
        let mut by_track: HashMap<Uuid, Vec<PlaylistRef>> = HashMap::new();
        for playlist in self.playlists.read().iter() {
            let reference = PlaylistRef {
                id: playlist.id,
                name: playlist.name.clone(),
            };
            for track in &playlist.tracks {
                by_track
                    .entry(track.id)
                    .or_default()
                    .push(reference.clone());
            }
        }
        by_track
    }

    /// Apply `edit` to every copy of `track_id` the projection holds — the library and
    /// each playlist containing it. Returns whether the library held the track.
    ///
    /// A change reaching only one copy is a bug rather than an optimization. The
    /// playlists are only written when one of them actually holds the track, because
    /// writing a signal re-renders everything reading it.
    pub fn edit_track(mut self, track_id: Uuid, edit: &impl Fn(&mut Track)) -> bool {
        let mut found = false;
        let mut tracks = self.tracks.write();
        if let Some(track) = tracks.iter_mut().find(|t| t.id == track_id) {
            edit(track);
            found = true;
        }
        drop(tracks);

        if !self.any_playlist_holds(track_id) {
            return found;
        }
        let mut playlists = self.playlists.write();
        for playlist in playlists.iter_mut() {
            if let Some(track) = playlist.tracks.iter_mut().find(|t| t.id == track_id) {
                edit(track);
            }
        }
        found
    }

    /// Create an empty playlist and persist it.
    pub fn create_playlist(mut self, name: impl Into<String>) -> Uuid {
        let playlist = Playlist::new(name);
        let id = playlist.id;
        self.playlists.write().push(playlist);
        self.save_playlist(id);
        id
    }

    /// Adopt an already-built playlist and persist it.
    pub fn add_playlist(mut self, playlist: Playlist) -> Uuid {
        let id = playlist.id;
        self.playlists.write().push(playlist);
        self.save_playlist(id);
        id
    }

    /// Delete a playlist and forget it in the store.
    pub fn delete_playlist(mut self, playlist_id: Uuid) {
        self.playlists.write().retain(|p| p.id != playlist_id);
        if let Some(writer) = self.playlist_writer.read().clone() {
            writer.delete(playlist_id);
        }
    }

    /// Add tracks to a playlist and persist the new membership.
    pub fn add_tracks_to_playlist(mut self, tracks: Vec<Track>, playlist_id: Uuid) {
        let mut playlists = self.playlists.write();
        let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) else {
            return;
        };
        for track in tracks {
            playlist.add_track(track);
        }
        drop(playlists);
        self.save_playlist(playlist_id);
    }

    /// Remove tracks from a playlist and persist the new membership.
    pub fn remove_tracks_from_playlist(mut self, track_ids: &[Uuid], playlist_id: Uuid) {
        let mut playlists = self.playlists.write();
        let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) else {
            return;
        };
        for track_id in track_ids {
            playlist.remove_track_by_id(*track_id);
        }
        drop(playlists);
        self.save_playlist(playlist_id);
    }

    fn any_playlist_holds(&self, track_id: Uuid) -> bool {
        self.playlists
            .read()
            .iter()
            .any(|playlist| playlist.tracks.iter().any(|track| track.id == track_id))
    }

    /// Hand one playlist's current state to the writer.
    ///
    /// Every membership edit ends here, so no gesture can change a playlist without
    /// also saving it.
    fn save_playlist(&self, playlist_id: Uuid) {
        let Some(writer) = self.playlist_writer.read().clone() else {
            return;
        };
        let Some(playlist) = self
            .playlists
            .read()
            .iter()
            .find(|p| p.id == playlist_id)
            .cloned()
        else {
            return;
        };
        writer.save(playlist);
    }
}

impl Default for CatalogProjection {
    fn default() -> Self {
        Self::new()
    }
}

/// A projection built outside a running component, for the tests of the modules that
/// browse and diagnose it.
#[cfg(test)]
impl CatalogProjection {
    pub(crate) fn new_in_scope(tracks: Vec<Track>) -> Self {
        Self {
            tracks: Signal::new_in_scope(tracks, ScopeId::ROOT),
            playlists: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            sources: Signal::new_in_scope(Vec::new(), ScopeId::ROOT),
            playlist_writer: Signal::new_in_scope(None, ScopeId::ROOT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_support::{a_track, titles, with_runtime, RecordingPlaylists};

    fn a_projection(tracks: Vec<Track>) -> (CatalogProjection, Arc<RecordingPlaylists>) {
        let writer = Arc::new(RecordingPlaylists::default());
        let projection = CatalogProjection::new_in_scope(tracks);
        projection.persist_to(writer.clone());
        (projection, writer)
    }

    /// A track is shown in the library and in every playlist holding it at once, so an
    /// edit that reaches only one of them leaves a stale row on screen.
    #[test]
    fn editing_a_track_reaches_the_library_and_every_playlist_holding_it() {
        with_runtime(|| {
            let (projection, _) = a_projection(vec![a_track("/music/one.mp3", "Blue Monday")]);
            let track = projection.tracks()[0].clone();
            let playlist_id = projection.create_playlist("Set");
            projection.add_tracks_to_playlist(vec![track.clone()], playlist_id);

            let found = projection.edit_track(track.id, &|track| track.bpm = Some(128.0));

            assert!(found);
            assert_eq!(projection.track(track.id).unwrap().bpm, Some(128.0));
            assert_eq!(projection.playlist_tracks(playlist_id)[0].bpm, Some(128.0));
        })
    }

    #[test]
    fn editing_a_track_the_projection_does_not_hold_reports_it() {
        with_runtime(|| {
            let (projection, _) = a_projection(Vec::new());

            assert!(!projection.edit_track(Uuid::new_v4(), &|track| track.bpm = Some(128.0)));
        })
    }

    #[test]
    fn every_membership_edit_is_persisted() {
        with_runtime(|| {
            let (projection, writer) = a_projection(vec![a_track("/music/one.mp3", "Blue Monday")]);
            let track = projection.tracks()[0].clone();
            let playlist_id = projection.create_playlist("Set");

            projection.add_tracks_to_playlist(vec![track.clone()], playlist_id);
            assert_eq!(
                writer.last_saved(playlist_id),
                Some(vec!["Blue Monday".to_string()])
            );

            projection.remove_tracks_from_playlist(&[track.id], playlist_id);
            assert_eq!(writer.last_saved(playlist_id), Some(Vec::new()));

            projection.delete_playlist(playlist_id);
            assert_eq!(writer.deleted(), vec![playlist_id]);
        })
    }

    #[test]
    fn a_tracks_badges_name_every_playlist_holding_it() {
        with_runtime(|| {
            let (projection, _) = a_projection(vec![
                a_track("/music/one.mp3", "Blue Monday"),
                a_track("/music/two.mp3", "Zebra"),
            ]);
            let tracks = projection.tracks();
            let set = projection.create_playlist("Set");
            let warmup = projection.create_playlist("Warmup");
            projection.add_tracks_to_playlist(vec![tracks[0].clone()], set);
            projection.add_tracks_to_playlist(tracks.clone(), warmup);

            let badges = projection.playlists_by_track();

            let mut names: Vec<String> = badges[&tracks[0].id]
                .iter()
                .map(|reference| reference.name.clone())
                .collect();
            names.sort();
            assert_eq!(names, ["Set", "Warmup"]);
            assert_eq!(badges[&tracks[1].id].len(), 1);
        })
    }

    #[test]
    fn tracks_are_looked_up_in_the_order_they_were_asked_for() {
        with_runtime(|| {
            let (projection, _) = a_projection(vec![
                a_track("/music/one.mp3", "Blue Monday"),
                a_track("/music/two.mp3", "Zebra"),
            ]);
            let tracks = projection.tracks();

            let found = projection.tracks_by_ids(&[tracks[1].id, Uuid::new_v4(), tracks[0].id]);

            assert_eq!(titles(&found), ["Zebra", "Blue Monday"]);
        })
    }
}
