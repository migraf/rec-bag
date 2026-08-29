//! Shared application state using Dioxus Context API.
//!
//! Everything persistent lives in the one [`Catalog`]. `AppState` holds the reactive
//! projection of it that views render, plus the transient playback state that belongs
//! to no record: which decks hold what, and which tracks are being analyzed right now.

pub mod deck;
pub mod integration;

use crate::services::catalog::Catalog;
use deck::{Deck, SharedAudio, DECK_COUNT};
use dioxus::prelude::*;
use rec_core::{
    analyze_track, AudioEngine, BeatGridData, Diagnosis, Doctor, FrequencyWaveformData,
    LibraryEntry, MetadataField, MetadataValue, MusicSource, Playlist, SearchEngine, Track,
    TrackAnalysis, WaveformPeaks,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex as StdMutex};
use uuid::Uuid;

/// What one background analysis produced.
///
/// A failure reports too, because a track that stays marked as analyzing can never be
/// analyzed again.
#[derive(Debug, Clone)]
pub struct TrackAnalysisResult {
    pub track_id: Uuid,
    pub analysis: Option<TrackAnalysis>,
}

/// Channel sender for analysis results (thread-safe, clonable)
pub type AnalysisSender = mpsc::Sender<TrackAnalysisResult>;
/// Channel receiver for analysis results
pub type AnalysisReceiver = mpsc::Receiver<TrackAnalysisResult>;

/// Global application state shared across all views.
#[derive(Clone, Copy)]
pub struct AppState {
    /// The playback decks, in deck order.
    decks: [Deck; DECK_COUNT],
    /// The one Catalog, once the integration runtime has opened it.
    catalog: Signal<Option<Arc<Catalog>>>,
    /// All playlists
    pub playlists: Signal<Vec<Playlist>>,
    /// The live catalog projection every view browses.
    pub library_tracks: Signal<Vec<Track>>,
    /// Music sources (folders and providers rec_bag observes)
    pub sources: Signal<Vec<MusicSource>>,
    /// Currently selected playlist ID
    pub selected_playlist_id: Signal<Option<Uuid>>,
    /// Current search query for filtering
    pub search_query: Signal<String>,
    /// Tracks currently being analyzed
    pub analyzing_tracks: Signal<HashSet<Uuid>>,
    /// Channel sender for analysis results
    pub analysis_tx: Signal<AnalysisSender>,
    /// Channel receiver for analysis results (wrapped in Arc<Mutex> for sharing)
    analysis_rx: Signal<Arc<StdMutex<AnalysisReceiver>>>,
    // ==================== Doctor State ====================
    /// Doctor instance for running diagnostics
    pub doctor: Signal<Arc<Doctor>>,
    /// Current diagnoses from the last scan
    pub diagnoses: Signal<Vec<Diagnosis>>,
    /// Whether a doctor scan is currently running
    pub doctor_scanning: Signal<bool>,
    /// Number of tracks scanned in current/last scan
    pub doctor_scanned_count: Signal<usize>,
    /// Diagnoses indexed by track ID for quick lookup
    pub diagnoses_by_track: Signal<HashMap<Uuid, Vec<Diagnosis>>>,
    // ==================== Theme State ====================
    /// Current theme name (e.g., "dark", "light")
    pub current_theme: Signal<String>,
}

impl AppState {
    /// Create a new app state with initialized audio engine.
    pub fn new() -> Self {
        let engine = AudioEngine::two_deck().expect("Failed to initialize audio engine");
        let audio: SharedAudio = Arc::new(StdMutex::new(engine));
        let audio = Signal::new(audio);
        let (tx, rx) = mpsc::channel();

        Self {
            decks: std::array::from_fn(|index| Deck::new(audio, index)),
            catalog: Signal::new(None),
            playlists: Signal::new(Vec::new()),
            library_tracks: Signal::new(Vec::new()),
            sources: Signal::new(Vec::new()),
            selected_playlist_id: Signal::new(None),
            search_query: Signal::new(String::new()),
            analyzing_tracks: Signal::new(HashSet::new()),
            analysis_tx: Signal::new(tx),
            analysis_rx: Signal::new(Arc::new(StdMutex::new(rx))),
            // Doctor state
            doctor: Signal::new(Arc::new(Doctor::new())),
            diagnoses: Signal::new(Vec::new()),
            doctor_scanning: Signal::new(false),
            doctor_scanned_count: Signal::new(0),
            diagnoses_by_track: Signal::new(HashMap::new()),
            // Theme state
            current_theme: Signal::new("dark".to_string()),
        }
    }

    /// Install the Catalog and load everything it holds.
    ///
    /// The integration runtime owns catalog startup, so this is the composition root's
    /// hand-off rather than something the application state opens for itself.
    pub async fn adopt_catalog(mut self, catalog: Arc<Catalog>) {
        self.catalog.set(Some(catalog.clone()));
        match catalog.setting("theme").await {
            Ok(Some(theme)) => self.current_theme.set(theme),
            Ok(None) => {}
            Err(error) => eprintln!("Could not load the theme preference: {error}"),
        }
        self.refresh_catalog().await;
    }

    /// The Catalog, if the integration runtime has opened it.
    pub fn catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.read().clone()
    }

    /// Re-read the library, playlists, and sources from the Catalog.
    pub async fn refresh_catalog(mut self) {
        let Some(catalog) = self.catalog() else {
            return;
        };
        let tracks = match catalog.library().await {
            Ok(tracks) => tracks,
            Err(error) => {
                eprintln!("Could not read the library from the catalog: {error}");
                return;
            }
        };
        match catalog.playlists(&tracks).await {
            Ok(playlists) => self.playlists.set(playlists),
            Err(error) => eprintln!("Could not read playlists from the catalog: {error}"),
        }
        match catalog.sources().await {
            Ok(sources) => self.sources.set(sources),
            Err(error) => eprintln!("Could not read sources from the catalog: {error}"),
        }
        self.library_tracks.set(tracks);
    }

    /// Check if a track is currently being analyzed.
    pub fn is_analyzing(&self, track_id: Uuid) -> bool {
        self.analyzing_tracks.read().contains(&track_id)
    }

    /// Poll for completed analysis results (non-blocking).
    /// Returns all available results.
    pub fn poll_analysis_results(self) -> Vec<TrackAnalysisResult> {
        let mut results = Vec::new();
        if let Ok(rx) = self.analysis_rx.read().try_lock() {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        results
    }

    /// Apply analysis to the library, the playlists, the decks, and the catalog.
    pub async fn apply_analysis_result(mut self, result: TrackAnalysisResult) {
        let TrackAnalysisResult { track_id, analysis } = result;

        self.analyzing_tracks.write().remove(&track_id);
        let Some(analysis) = analysis else {
            return;
        };
        self.edit_track_everywhere(track_id, |track| apply_analysis(track, &analysis));

        if let Some(catalog) = self.catalog() {
            if let Err(error) = catalog.save_analysis(analysis).await {
                eprintln!("Could not save analysis for track {track_id}: {error}");
            }
        }
    }

    /// Mark a track as being analyzed.
    pub fn mark_analyzing(mut self, track_id: Uuid) {
        self.analyzing_tracks.write().insert(track_id);
    }

    /// Add a track to a playlist.
    pub fn add_track_to_playlist(self, track: Track, playlist_id: Uuid) {
        self.add_tracks_to_playlist(vec![track], playlist_id);
    }

    /// Create a new playlist.
    pub fn create_playlist(mut self, name: String) -> Uuid {
        let playlist = Playlist::new(name);
        let id = playlist.id;
        self.playlists.write().push(playlist);
        self.save_playlist(id);
        id
    }

    /// Delete a playlist.
    pub fn delete_playlist(mut self, id: Uuid) {
        self.playlists.write().retain(|p| p.id != id);
        if *self.selected_playlist_id.read() == Some(id) {
            self.selected_playlist_id.set(None);
        }
        let Some(catalog) = self.catalog() else {
            return;
        };
        spawn(async move {
            if let Err(error) = catalog.delete_playlist(id).await {
                eprintln!("Could not delete playlist {id}: {error}");
            }
        });
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

    /// Get tracks for the selected playlist.
    pub fn selected_playlist_tracks(&self) -> Vec<Track> {
        if let Some(id) = *self.selected_playlist_id.read() {
            self.playlists
                .read()
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.tracks.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Tracks available to the DJ browser. With no playlist selected, browse the whole
    /// library projection.
    pub fn dj_tracks(&self) -> Vec<Track> {
        if self.selected_playlist_id.read().is_some() {
            self.selected_playlist_tracks()
        } else {
            self.library_tracks.read().clone()
        }
    }

    // ==================== Search ====================

    /// Set the search query.
    pub fn set_search_query(mut self, query: String) {
        self.search_query.set(query);
    }

    /// Get filtered library tracks based on search query.
    pub fn filtered_library_tracks(&self) -> Vec<Track> {
        filtered(
            self.search_query.read().clone(),
            self.library_tracks.read().clone(),
        )
    }

    /// Get filtered playlist tracks based on search query.
    pub fn filtered_playlist_tracks(&self) -> Vec<Track> {
        filtered(
            self.search_query.read().clone(),
            self.selected_playlist_tracks(),
        )
    }

    // ==================== Playlist References ====================

    /// Get all playlists that contain a specific track.
    pub fn get_playlists_for_track(&self, track_id: Uuid) -> Vec<PlaylistRef> {
        self.playlists
            .read()
            .iter()
            .filter(|p| p.tracks.iter().any(|t| t.id == track_id))
            .map(|p| PlaylistRef {
                id: p.id,
                name: p.name.clone(),
            })
            .collect()
    }

    // ==================== Batch Operations ====================

    /// Add multiple tracks to a playlist.
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

    /// Persist one playlist's current membership.
    fn save_playlist(self, playlist_id: Uuid) {
        let Some(catalog) = self.catalog() else {
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
        spawn(async move {
            if let Err(error) = catalog.save_playlist(&playlist).await {
                eprintln!("Could not save playlist {playlist_id}: {error}");
            }
        });
    }

    // ==================== Deck Operations ====================

    /// Every deck, in deck order.
    pub fn decks(&self) -> [Deck; DECK_COUNT] {
        self.decks
    }

    /// The deck with the given user-facing number, counting from one.
    pub fn deck(&self, number: usize) -> Option<Deck> {
        number
            .checked_sub(1)
            .and_then(|index| self.decks.get(index))
            .copied()
    }

    /// Re-read every deck's live state from the audio output.
    pub fn refresh_decks(&self) {
        for deck in self.decks {
            deck.refresh();
        }
    }

    /// Load the canonical track `track_id` onto deck 1 or 2.
    ///
    /// The Catalog resolves the concrete file; the deck keeps the track's own identity,
    /// so a cached copy of a remote asset never becomes the track.
    pub fn load_track_to_deck(self, track_id: Uuid, deck: usize) -> bool {
        let Some(deck) = self.deck(deck) else {
            return false;
        };
        let Some(catalog) = self.catalog() else {
            return false;
        };

        spawn(async move {
            let loaded = match catalog.load_track(track_id).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    eprintln!("Could not prepare track {track_id} for playback: {error}");
                    return;
                }
            };

            let needs_analysis = loaded.track.needs_analysis();
            let path = loaded.path.clone();
            if !deck.load(loaded) {
                return;
            }
            if needs_analysis {
                spawn_track_analysis(self, track_id, path);
            }
        });

        true
    }

    // ==================== Doctor Operations ====================

    /// Run doctor diagnostics on all library tracks.
    pub fn run_doctor_scan(mut self) {
        self.doctor_scanning.set(true);
        self.doctor_scanned_count.set(0);

        let tracks = self.library_tracks.read().clone();
        let doctor = self.doctor.read().clone();

        // Run diagnostics
        let all_diagnoses = doctor.check_tracks(&tracks);

        // Build index by track ID
        let mut by_track: HashMap<Uuid, Vec<Diagnosis>> = HashMap::new();
        for diagnosis in &all_diagnoses {
            by_track
                .entry(diagnosis.track_id)
                .or_default()
                .push(diagnosis.clone());
        }

        // Update state
        self.diagnoses.set(all_diagnoses);
        self.diagnoses_by_track.set(by_track);
        self.doctor_scanned_count.set(tracks.len());
        self.doctor_scanning.set(false);
    }

    /// Get diagnoses for a specific track.
    pub fn get_diagnoses_for_track(&self, track_id: Uuid) -> Vec<Diagnosis> {
        self.diagnoses_by_track
            .read()
            .get(&track_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Check if a track has any issues.
    pub fn track_has_issues(&self, track_id: Uuid) -> bool {
        self.diagnoses_by_track.read().contains_key(&track_id)
    }

    /// Check if a track has fixable issues.
    pub fn track_has_fixable_issues(&self, track_id: Uuid) -> bool {
        self.diagnoses_by_track
            .read()
            .get(&track_id)
            .map(|d| d.iter().any(|diag| diag.fixable))
            .unwrap_or(false)
    }

    /// Fix a single diagnosis for a track.
    pub fn fix_diagnosis(mut self, track_id: Uuid, check_name: &str) -> Result<(), String> {
        // Find the track
        let track = self
            .library_tracks
            .read()
            .iter()
            .find(|t| t.id == track_id)
            .cloned();

        let Some(track) = track else {
            return Err("Track not found".to_string());
        };

        // Find the diagnosis
        let diagnosis = self
            .diagnoses
            .read()
            .iter()
            .find(|d| d.track_id == track_id && d.check_name == check_name)
            .cloned();

        let Some(diagnosis) = diagnosis else {
            return Err("Diagnosis not found".to_string());
        };

        // Apply the fix
        let doctor = self.doctor.read().clone();
        doctor
            .fix_diagnosis(&diagnosis, &track)
            .map_err(|e| e.to_string())?;

        // Re-scan the track to update diagnoses
        let new_diagnoses: Vec<Diagnosis> = doctor.check_track(&track).into_iter().collect();

        // Update the diagnoses state
        let mut all_diagnoses = self.diagnoses.write();
        all_diagnoses.retain(|d| d.track_id != track_id);
        all_diagnoses.extend(new_diagnoses.clone());
        drop(all_diagnoses);

        // Update the by-track index
        let mut by_track = self.diagnoses_by_track.write();
        if new_diagnoses.is_empty() {
            by_track.remove(&track_id);
        } else {
            by_track.insert(track_id, new_diagnoses);
        }

        Ok(())
    }

    /// Fix all fixable issues for a specific check type.
    pub fn fix_all_by_check(self, check_name: &str) -> (usize, usize) {
        let diagnoses_to_fix: Vec<Diagnosis> = self
            .diagnoses
            .read()
            .iter()
            .filter(|d| d.check_name == check_name && d.fixable)
            .cloned()
            .collect();

        let mut fixed = 0;
        let mut failed = 0;

        for diagnosis in diagnoses_to_fix {
            match self.fix_diagnosis(diagnosis.track_id, &diagnosis.check_name) {
                Ok(_) => fixed += 1,
                Err(_) => failed += 1,
            }
        }

        (fixed, failed)
    }

    /// Fix all fixable issues for a specific track.
    pub fn fix_all_for_track(self, track_id: Uuid) -> (usize, usize) {
        let diagnoses_to_fix: Vec<Diagnosis> = self
            .get_diagnoses_for_track(track_id)
            .into_iter()
            .filter(|d| d.fixable)
            .collect();

        let mut fixed = 0;
        let mut failed = 0;

        for diagnosis in diagnoses_to_fix {
            match self.fix_diagnosis(track_id, &diagnosis.check_name) {
                Ok(_) => fixed += 1,
                Err(_) => failed += 1,
            }
        }

        (fixed, failed)
    }

    // ==================== Metadata Operations ====================

    /// Update metadata for a single track.
    ///
    /// Rating and comment are the user's own record and go to the track's library
    /// entry. Everything else describes the audio, so `write_to_file` decides whether
    /// it is also written back into the file's tags.
    #[allow(clippy::too_many_arguments)]
    pub fn update_track_metadata(
        self,
        track_id: Uuid,
        title: Option<String>,
        artist: Option<Option<String>>,
        album: Option<Option<String>>,
        genre: Option<Option<String>>,
        bpm: Option<Option<f32>>,
        key: Option<Option<String>>,
        year: Option<Option<u32>>,
        track_number: Option<Option<u32>>,
        comment: Option<Option<String>>,
        rating: Option<Option<u8>>,
        write_to_file: bool,
    ) -> Result<(), String> {
        let found = self.edit_track_everywhere(track_id, |track| {
            track.update_metadata(
                title.clone(),
                artist.clone(),
                album.clone(),
                genre.clone(),
                bpm,
                key.clone(),
                year,
                track_number,
                comment.clone(),
                rating,
            );
            if write_to_file && track.path.is_file() {
                if let Err(error) = track.save_metadata() {
                    eprintln!("Failed to save metadata to file: {error}");
                }
            }
        });

        if !found {
            return Err("Track not found".to_string());
        }

        if comment.is_some() || rating.is_some() {
            self.save_library_entry(track_id, comment.flatten(), rating.flatten());
        }
        self.save_metadata_edits(
            track_id,
            metadata_edits(title, artist, album, genre, bpm, key, year, track_number),
        );

        // Re-run doctor for this track since metadata changed
        self.recheck_track_diagnoses(track_id);

        Ok(())
    }

    /// Update metadata for multiple tracks (batch operation).
    /// Only non-None fields will be updated.
    #[allow(clippy::too_many_arguments)]
    pub fn batch_update_metadata(
        self,
        track_ids: Vec<Uuid>,
        title: Option<String>,
        artist: Option<Option<String>>,
        album: Option<Option<String>>,
        genre: Option<Option<String>>,
        bpm: Option<Option<f32>>,
        key: Option<Option<String>>,
        year: Option<Option<u32>>,
        track_number: Option<Option<u32>>,
        comment: Option<Option<String>>,
        rating: Option<Option<u8>>,
        write_to_file: bool,
    ) -> (usize, usize) {
        let mut success = 0;
        let mut failed = 0;

        for track_id in track_ids {
            match self.update_track_metadata(
                track_id,
                title.clone(),
                artist.clone(),
                album.clone(),
                genre.clone(),
                bpm,
                key.clone(),
                year,
                track_number,
                comment.clone(),
                rating,
                write_to_file,
            ) {
                Ok(_) => success += 1,
                Err(_) => failed += 1,
            }
        }

        (success, failed)
    }

    /// Persist metadata the user authored, as candidates on the canonical track.
    fn save_metadata_edits(
        self,
        track_id: Uuid,
        edits: Vec<(MetadataField, Option<MetadataValue>)>,
    ) {
        if edits.is_empty() {
            return;
        }
        let Some(catalog) = self.catalog() else {
            return;
        };
        spawn(async move {
            if let Err(error) = catalog.record_metadata_edits(track_id, edits).await {
                eprintln!("Could not save metadata for track {track_id}: {error}");
            }
        });
    }

    /// Persist the user's own record about a track.
    fn save_library_entry(self, track_id: Uuid, comment: Option<String>, rating: Option<u8>) {
        let Some(catalog) = self.catalog() else {
            return;
        };
        spawn(async move {
            let mut entry = match catalog.library_entry(track_id).await {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("Could not read the library entry for {track_id}: {error}");
                    LibraryEntry::new(track_id)
                }
            };
            entry.comment = comment;
            entry.rating = rating;
            entry.updated_at = chrono::Utc::now();
            if let Err(error) = catalog.save_library_entry(entry).await {
                eprintln!("Could not save the library entry for {track_id}: {error}");
            }
        });
    }

    /// Re-check diagnoses for a specific track after metadata changes.
    fn recheck_track_diagnoses(mut self, track_id: Uuid) {
        let track = self
            .library_tracks
            .read()
            .iter()
            .find(|t| t.id == track_id)
            .cloned();

        if let Some(track) = track {
            let doctor = self.doctor.read().clone();
            let new_diagnoses: Vec<Diagnosis> = doctor.check_track(&track);

            // Update the diagnoses state
            let mut all_diagnoses = self.diagnoses.write();
            all_diagnoses.retain(|d| d.track_id != track_id);
            all_diagnoses.extend(new_diagnoses.clone());
            drop(all_diagnoses);

            // Update the by-track index
            let mut by_track = self.diagnoses_by_track.write();
            if new_diagnoses.is_empty() {
                by_track.remove(&track_id);
            } else {
                by_track.insert(track_id, new_diagnoses);
            }
        }
    }

    /// Get tracks by their IDs.
    pub fn get_tracks_by_ids(&self, track_ids: &[Uuid]) -> Vec<Track> {
        let library_tracks = self.library_tracks.read();
        track_ids
            .iter()
            .filter_map(|id| library_tracks.iter().find(|t| t.id == *id).cloned())
            .collect()
    }

    // ==================== Analysis Operations ====================

    /// Discard a track's analysis and derive it again from its audio.
    pub fn analyze_track(self, track_id: Uuid) {
        let Some(catalog) = self.catalog() else {
            return;
        };

        spawn(async move {
            let loaded = match catalog.load_track(track_id).await {
                Ok(loaded) => loaded,
                Err(error) => {
                    eprintln!("Could not prepare track {track_id} for analysis: {error}");
                    return;
                }
            };

            let mut app_state = self;
            if let Err(error) = catalog.forget_analysis(track_id).await {
                eprintln!("Could not clear analysis for track {track_id}: {error}");
            }
            app_state.edit_track_everywhere(track_id, |track| {
                track.waveform = None;
                track.freq_waveform = None;
                track.beat_grid = None;
                track.bpm = None;
                track.key = None;
            });
            app_state.analyzing_tracks.write().remove(&track_id);
            spawn_track_analysis(app_state, track_id, loaded.path);
        });
    }

    /// Analyze multiple tracks.
    pub fn analyze_tracks(self, track_ids: Vec<Uuid>) {
        for track_id in track_ids {
            self.analyze_track(track_id);
        }
    }

    /// Apply `edit` to every copy of `track_id`: the library, the playlists, and any
    /// deck holding it. Returns whether the track was found in the library.
    ///
    /// One projection is shown in several places at once, so a change reaching only one
    /// of them is a bug rather than an optimization.
    fn edit_track_everywhere(mut self, track_id: Uuid, edit: impl Fn(&mut Track)) -> bool {
        let mut found = false;
        let mut library_tracks = self.library_tracks.write();
        if let Some(track) = library_tracks.iter_mut().find(|t| t.id == track_id) {
            edit(track);
            found = true;
        }
        drop(library_tracks);

        let mut playlists = self.playlists.write();
        for playlist in playlists.iter_mut() {
            if let Some(track) = playlist.tracks.iter_mut().find(|t| t.id == track_id) {
                edit(track);
            }
        }
        drop(playlists);

        for deck in self.decks {
            deck.edit_track(track_id, &edit);
        }
        found
    }

    // ==================== Theme Operations ====================

    /// Get the current theme name.
    pub fn get_theme(&self) -> String {
        self.current_theme.read().clone()
    }

    /// Set the current theme and save it to the Catalog.
    pub async fn set_theme(mut self, theme: String) {
        self.current_theme.set(theme.clone());
        if let Some(catalog) = self.catalog() {
            if let Err(error) = catalog.set_setting("theme", &theme).await {
                eprintln!("Could not save the theme preference: {error}");
            }
        }
    }

    /// Toggle between dark and light themes.
    pub async fn toggle_theme(self) {
        let current = self.get_theme();
        let new_theme = if current == "dark" { "light" } else { "dark" };
        self.set_theme(new_theme.to_string()).await;
    }
}

/// Turn one metadata-editor submission into canonical metadata candidates.
///
/// The outer `Option` is "the editor did not touch this field"; the inner one is "the
/// user cleared it". Only touched fields are recorded.
#[allow(clippy::too_many_arguments)]
fn metadata_edits(
    title: Option<String>,
    artist: Option<Option<String>>,
    album: Option<Option<String>>,
    genre: Option<Option<String>>,
    bpm: Option<Option<f32>>,
    key: Option<Option<String>>,
    year: Option<Option<u32>>,
    track_number: Option<Option<u32>>,
) -> Vec<(MetadataField, Option<MetadataValue>)> {
    fn text(
        field: MetadataField,
        value: Option<Option<String>>,
    ) -> Option<(MetadataField, Option<MetadataValue>)> {
        Some((
            field,
            value?
                .filter(|value| !value.trim().is_empty())
                .map(MetadataValue::Text),
        ))
    }
    fn count(
        field: MetadataField,
        value: Option<Option<u32>>,
    ) -> Option<(MetadataField, Option<MetadataValue>)> {
        Some((
            field,
            value?.map(|value| MetadataValue::Integer(i64::from(value))),
        ))
    }

    [
        text(MetadataField::Title, title.map(Some)),
        text(MetadataField::Artist, artist),
        text(MetadataField::Album, album),
        text(MetadataField::Genre, genre),
        text(MetadataField::MusicalKey, key),
        bpm.map(|bpm| {
            (
                MetadataField::Bpm,
                bpm.map(|bpm| MetadataValue::Decimal(bpm.to_string())),
            )
        }),
        count(MetadataField::ReleaseYear, year),
        count(MetadataField::TrackNumber, track_number),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Attach analysis results to a track, leaving values it already carries alone.
fn apply_analysis(track: &mut Track, analysis: &TrackAnalysis) {
    fn set<T>(slot: &mut Option<T>, value: &Option<T>)
    where
        T: Clone,
    {
        if slot.is_none() {
            slot.clone_from(value);
        }
    }
    set::<WaveformPeaks>(&mut track.waveform, &analysis.waveform);
    set::<FrequencyWaveformData>(&mut track.freq_waveform, &analysis.freq_waveform);
    set::<BeatGridData>(&mut track.beat_grid, &analysis.beat_grid);
    set::<f32>(&mut track.bpm, &analysis.bpm);
    set::<String>(&mut track.key, &analysis.key);
}

fn filtered(query: String, tracks: Vec<Track>) -> Vec<Track> {
    if query.is_empty() {
        return tracks;
    }
    let mut engine = SearchEngine::new();
    engine
        .search(&query, &tracks)
        .into_iter()
        .map(|r| r.track)
        .collect()
}

/// Reference to a playlist (for display purposes).
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistRef {
    pub id: Uuid,
    pub name: String,
}

/// Provide the app state to the component tree.
pub fn use_app_state_provider() {
    let app_state = use_context_provider(AppState::new);
    // One refresh loop feeds every view, so a deck reads the same wherever it is shown.
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            app_state.refresh_decks();
        }
    });
    // Analysis runs off the Dioxus executor and reports back through a channel. Without
    // this loop its results are computed and dropped, and a track stays "analyzing"
    // forever, so re-analysis silently becomes a no-op.
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            for result in app_state.poll_analysis_results() {
                app_state.apply_analysis_result(result).await;
            }
        }
    });
}

/// Get the app state from context.
pub fn use_app_state() -> AppState {
    use_context::<AppState>()
}

/// Spawn background track analysis (waveform, BPM, key, beat grid detection).
/// Uses std::thread::spawn for CPU-intensive work as per Dioxus docs.
/// Results are sent via channel and processed by `use_app_state_provider`'s poll loop.
pub fn spawn_track_analysis(app_state: AppState, track_id: Uuid, path: PathBuf) {
    if app_state.is_analyzing(track_id) {
        return;
    }
    app_state.mark_analyzing(track_id);

    // Clone the sender before spawning (Sender is Send)
    let tx = (*app_state.analysis_tx.read()).clone();

    // Spawn on a separate thread to avoid blocking UI
    // As per Dioxus docs: "If you have an expensive task you need to run,
    // you should spawn it on a separate thread using std::thread::spawn"
    std::thread::spawn(move || {
        tracing::debug!(%track_id, path = %path.display(), "starting track analysis");

        match analyze_track(&path) {
            Ok(result) => {
                let analysis = TrackAnalysis {
                    track_id,
                    waveform: Some(WaveformPeaks {
                        peaks: result.waveform_peaks,
                    }),
                    freq_waveform: result.freq_waveform.map(|fw| FrequencyWaveformData {
                        low: fw.low,
                        mid: fw.mid,
                        high: fw.high,
                    }),
                    beat_grid: result.beat_grid.map(|bg| BeatGridData {
                        first_beat_ms: bg.first_beat_ms,
                        beat_interval_ms: bg.beat_interval_ms,
                        beats: bg.beats,
                    }),
                    bpm: result.bpm.map(|b| b.bpm),
                    key: result.key.map(|k| k.key.to_string()),
                };
                tracing::debug!(
                    %track_id,
                    bpm = ?analysis.bpm,
                    key = ?analysis.key,
                    "track analysis complete"
                );
                let _ = tx.send(TrackAnalysisResult {
                    track_id,
                    analysis: Some(analysis),
                });
            }
            Err(error) => {
                tracing::warn!(%track_id, %error, "track analysis failed");
                let _ = tx.send(TrackAnalysisResult {
                    track_id,
                    analysis: None,
                });
            }
        }
    });
}
