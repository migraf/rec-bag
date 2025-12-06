//! Shared application state using Dioxus Context API.

use dioxus::prelude::*;
use rec_core::{
    extract_waveform, AudioEngine, Diagnosis, Doctor, Library, MusicSource, Playlist,
    SearchEngine, SqliteStorage, Track,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex as StdMutex};
use uuid::Uuid;

/// Result from waveform analysis
#[derive(Debug, Clone)]
pub struct WaveformResult {
    pub track_id: Uuid,
    pub peaks: Vec<f32>,
}

/// Channel sender for waveform results (thread-safe, clonable)
pub type WaveformSender = mpsc::Sender<WaveformResult>;
/// Channel receiver for waveform results
pub type WaveformReceiver = mpsc::Receiver<WaveformResult>;

/// Global application state shared across all views.
#[derive(Clone, Copy)]
pub struct AppState {
    /// Audio engine (wrapped in Arc<Mutex> for thread safety)
    pub engine: Signal<Arc<StdMutex<AudioEngine>>>,
    /// SQLite storage backend
    pub storage: Signal<Option<Arc<SqliteStorage>>>,
    /// All playlists
    pub playlists: Signal<Vec<Playlist>>,
    /// Scanned library tracks (from all sources)
    pub library_tracks: Signal<Vec<Track>>,
    /// Music sources (folders to watch for music files)
    pub sources: Signal<Vec<MusicSource>>,
    /// Currently selected playlist ID
    pub selected_playlist_id: Signal<Option<Uuid>>,
    /// Current search query for filtering
    pub search_query: Signal<String>,
    /// Flag to trigger save
    save_pending: Signal<bool>,
    /// Waveform results cache (for reactive updates)
    pub waveform_cache: Signal<HashMap<Uuid, Vec<f32>>>,
    /// Tracks currently being analyzed
    pub analyzing_tracks: Signal<HashSet<Uuid>>,
    /// Channel sender for waveform results
    pub waveform_tx: Signal<WaveformSender>,
    /// Channel receiver for waveform results (wrapped in Arc<Mutex> for sharing)
    waveform_rx: Signal<Arc<StdMutex<WaveformReceiver>>>,
    /// Track loaded on deck 1 (shared across views)
    pub deck1_track: Signal<Option<Track>>,
    /// Track loaded on deck 2 (shared across views)
    pub deck2_track: Signal<Option<Track>>,
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
        let (tx, rx) = mpsc::channel();

        Self {
            engine: Signal::new(Arc::new(StdMutex::new(engine))),
            storage: Signal::new(None),
            playlists: Signal::new(Vec::new()),
            library_tracks: Signal::new(Vec::new()),
            sources: Signal::new(Vec::new()),
            selected_playlist_id: Signal::new(None),
            search_query: Signal::new(String::new()),
            save_pending: Signal::new(false),
            waveform_cache: Signal::new(HashMap::new()),
            analyzing_tracks: Signal::new(HashSet::new()),
            waveform_tx: Signal::new(tx),
            waveform_rx: Signal::new(Arc::new(StdMutex::new(rx))),
            deck1_track: Signal::new(None),
            deck2_track: Signal::new(None),
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

    /// Initialize storage and load data asynchronously.
    pub async fn init_storage(mut self) {
        match SqliteStorage::default_location().await {
            Ok(storage) => {
                let storage = Arc::new(storage);

                // Load existing data
                match storage.load_library_async().await {
                    Ok(library) => {
                        // Pre-populate waveform cache from loaded tracks
                        let mut cache = HashMap::new();
                        for track in &library.loose_tracks {
                            if let Some(wf) = &track.waveform {
                                cache.insert(track.id, wf.peaks.clone());
                            }
                        }
                        for playlist in &library.playlists {
                            for track in &playlist.tracks {
                                if let Some(wf) = &track.waveform {
                                    cache.insert(track.id, wf.peaks.clone());
                                }
                            }
                        }
                        self.waveform_cache.set(cache);
                        self.playlists.set(library.playlists);
                        self.library_tracks.set(library.loose_tracks);
                    }
                    Err(e) => {
                        eprintln!("Failed to load library from storage: {}", e);
                    }
                }

                // Load music sources
                match storage.load_sources().await {
                    Ok(sources) => {
                        self.sources.set(sources);
                    }
                    Err(e) => {
                        eprintln!("Failed to load sources: {}", e);
                    }
                }

                // Load theme preference
                match storage.get_setting("theme").await {
                    Ok(Some(theme)) => {
                        self.current_theme.set(theme);
                    }
                    Ok(None) => {
                        // Keep default "dark" theme
                    }
                    Err(e) => {
                        eprintln!("Failed to load theme setting: {}", e);
                    }
                }

                self.storage.set(Some(storage.clone()));

                // Trigger auto-rescan of all enabled sources on startup
                self.rescan_all_sources().await;
            }
            Err(e) => {
                eprintln!("Failed to initialize storage: {}", e);
            }
        }
    }

    /// Save the current library state to storage.
    pub async fn save_to_storage(self) {
        if let Some(storage) = self.storage.read().as_ref() {
            let library = Library {
                playlists: self.playlists.read().clone(),
                loose_tracks: self.library_tracks.read().clone(),
            };
            
            if let Err(e) = storage.save_library_async(&library).await {
                eprintln!("Failed to save library: {}", e);
            }
        }
    }

    /// Mark that a save is pending.
    fn mark_save_pending(mut self) {
        self.save_pending.set(true);
    }

    /// Check and clear save pending flag.
    pub fn take_save_pending(mut self) -> bool {
        let pending = *self.save_pending.read();
        if pending {
            self.save_pending.set(false);
        }
        pending
    }

    /// Get waveform for a track from cache.
    pub fn get_waveform(&self, track_id: Uuid) -> Option<Vec<f32>> {
        self.waveform_cache.read().get(&track_id).cloned()
    }

    /// Check if a track is currently being analyzed.
    pub fn is_analyzing(&self, track_id: Uuid) -> bool {
        self.analyzing_tracks.read().contains(&track_id)
    }

    /// Poll for completed waveform results (non-blocking).
    /// Returns all available results.
    pub fn poll_waveform_results(self) -> Vec<WaveformResult> {
        let mut results = Vec::new();
        if let Ok(rx) = self.waveform_rx.read().try_lock() {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        results
    }

    /// Apply waveform result to the state.
    pub fn apply_waveform_result(mut self, result: WaveformResult) {
        let track_id = result.track_id;
        let peaks = result.peaks;

        // Update cache for reactive updates
        self.waveform_cache.write().insert(track_id, peaks.clone());

        // Remove from analyzing set
        self.analyzing_tracks.write().remove(&track_id);

        // Update deck tracks if they have this track
        self.update_deck_waveform(track_id, peaks.clone());

        // Update library tracks
        let mut library_tracks = self.library_tracks.write();
        if let Some(track) = library_tracks.iter_mut().find(|t| t.id == track_id) {
            track.set_waveform(peaks.clone());
            drop(library_tracks);
            self.mark_save_pending();
            return;
        }
        drop(library_tracks);

        // Update playlist tracks
        let mut playlists = self.playlists.write();
        for playlist in playlists.iter_mut() {
            if let Some(track) = playlist.tracks.iter_mut().find(|t| t.id == track_id) {
                track.set_waveform(peaks);
                break;
            }
        }
        drop(playlists);
        self.mark_save_pending();
    }

    /// Mark a track as being analyzed.
    pub fn mark_analyzing(mut self, track_id: Uuid) {
        self.analyzing_tracks.write().insert(track_id);
    }

    /// Add a track to a playlist.
    pub fn add_track_to_playlist(mut self, track: Track, playlist_id: Uuid) {
        let mut playlists = self.playlists.write();
        if let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) {
            playlist.add_track(track);
        }
        drop(playlists);
        self.mark_save_pending();
    }

    /// Create a new playlist.
    pub fn create_playlist(mut self, name: String) -> Uuid {
        let playlist = Playlist::new(name);
        let id = playlist.id;
        self.playlists.write().push(playlist);
        self.mark_save_pending();
        id
    }

    /// Delete a playlist.
    pub fn delete_playlist(mut self, id: Uuid) {
        self.playlists.write().retain(|p| p.id != id);
        if *self.selected_playlist_id.read() == Some(id) {
            self.selected_playlist_id.set(None);
        }
        self.mark_save_pending();
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

    // ==================== Source Management ====================

    /// Add a new music source folder.
    pub async fn add_source(mut self, path: String) -> Option<Uuid> {
        // Check if path exists and is a directory
        let path_obj = std::path::Path::new(&path);
        if !path_obj.exists() || !path_obj.is_dir() {
            return None;
        }

        // Check if source already exists
        if self.sources.read().iter().any(|s| s.uri == path) {
            return None;
        }

        let mut source = MusicSource::new_local(&path);
        let source_id = source.id;

        // Scan the folder for tracks
        let tracks = self.scan_source_folder(&path);
        source.set_track_count(tracks.len() as u32);

        // Save source to storage
        if let Some(storage) = self.storage.read().as_ref() {
            if let Err(e) = storage.upsert_source(&source).await {
                eprintln!("Failed to save source: {}", e);
                return None;
            }

            // Link tracks to source
            let track_ids: Vec<Uuid> = tracks.iter().map(|t| t.id).collect();
            if let Err(e) = storage.link_tracks_to_source(source_id, &track_ids).await {
                eprintln!("Failed to link tracks to source: {}", e);
            }
        }

        // Add tracks to library
        let mut library_tracks = self.library_tracks.write();
        for track in tracks {
            if !library_tracks.iter().any(|t| t.path == track.path) {
                library_tracks.push(track);
            }
        }
        drop(library_tracks);

        // Add source to state
        self.sources.write().push(source);
        self.mark_save_pending();

        Some(source_id)
    }

    /// Remove a music source.
    pub async fn remove_source(mut self, source_id: Uuid) {
        // Remove from storage
        if let Some(storage) = self.storage.read().as_ref() {
            if let Err(e) = storage.delete_source(source_id).await {
                eprintln!("Failed to delete source: {}", e);
            }
        }

        // Remove from state
        self.sources.write().retain(|s| s.id != source_id);
        self.mark_save_pending();
    }

    /// Rescan a specific source for changes.
    pub async fn rescan_source(mut self, source_id: Uuid) {
        let source = self.sources.read().iter().find(|s| s.id == source_id).cloned();

        if let Some(mut source) = source {
            if !source.enabled || !source.is_local() {
                return;
            }

            let path = source.uri.clone();
            let tracks = self.scan_source_folder(&path);
            source.set_track_count(tracks.len() as u32);

            // Update storage
            if let Some(storage) = self.storage.read().as_ref() {
                // Update source
                let _ = storage.upsert_source(&source).await;

                // Update track links
                let track_ids: Vec<Uuid> = tracks.iter().map(|t| t.id).collect();
                let _ = storage.link_tracks_to_source(source_id, &track_ids).await;
            }

            // Update library tracks (merge new tracks)
            let mut library_tracks = self.library_tracks.write();
            for track in tracks {
                if !library_tracks.iter().any(|t| t.path == track.path) {
                    library_tracks.push(track);
                }
            }
            drop(library_tracks);

            // Update source in state
            let mut sources = self.sources.write();
            if let Some(s) = sources.iter_mut().find(|s| s.id == source_id) {
                *s = source;
            }
            drop(sources);

            self.mark_save_pending();
        }
    }

    /// Rescan all enabled sources.
    pub async fn rescan_all_sources(self) {
        let source_ids: Vec<Uuid> = self
            .sources
            .read()
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.id)
            .collect();

        for source_id in source_ids {
            self.rescan_source(source_id).await;
        }
    }

    /// Create a playlist from a source folder.
    pub fn create_playlist_from_source(mut self, source_id: Uuid) -> Option<Uuid> {
        let source = self.sources.read().iter().find(|s| s.id == source_id).cloned()?;

        // Get tracks from this source
        let source_path = source.local_path()?;
        let source_tracks: Vec<Track> = self
            .library_tracks
            .read()
            .iter()
            .filter(|t| t.path.starts_with(source_path))
            .cloned()
            .collect();

        if source_tracks.is_empty() {
            return None;
        }

        // Create playlist with source name
        let mut playlist = Playlist::new(&source.name);
        for track in source_tracks {
            playlist.add_track(track);
        }

        let playlist_id = playlist.id;
        self.playlists.write().push(playlist);
        self.mark_save_pending();

        Some(playlist_id)
    }

    /// Scan a folder for audio files (internal helper).
    fn scan_source_folder(&self, path: &str) -> Vec<Track> {
        let mut found_tracks = Vec::new();

        fn scan_recursive(path: &std::path::Path, tracks: &mut Vec<Track>) {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let file_path = entry.path();
                    if file_path.is_dir() {
                        scan_recursive(&file_path, tracks);
                    } else if file_path.is_file() {
                        if let Some(ext) = file_path.extension() {
                            let ext = ext.to_string_lossy().to_lowercase();
                            if matches!(ext.as_str(), "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac")
                            {
                                tracks.push(Track::new(file_path));
                            }
                        }
                    }
                }
            }
        }

        scan_recursive(std::path::Path::new(path), &mut found_tracks);
        found_tracks
    }

    /// Scan a folder for audio files (legacy method, kept for compatibility).
    pub fn scan_folder(mut self, path: &str) {
        if path.is_empty() {
            return;
        }

        let found_tracks = self.scan_source_folder(path);
        self.library_tracks.set(found_tracks);
        self.mark_save_pending();
    }

    // ==================== Search ====================

    /// Set the search query.
    pub fn set_search_query(mut self, query: String) {
        self.search_query.set(query);
    }

    /// Get filtered library tracks based on search query.
    pub fn filtered_library_tracks(&self) -> Vec<Track> {
        let query = self.search_query.read().clone();
        let tracks = self.library_tracks.read().clone();

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

    /// Get filtered playlist tracks based on search query.
    pub fn filtered_playlist_tracks(&self) -> Vec<Track> {
        let query = self.search_query.read().clone();
        let tracks = self.selected_playlist_tracks();

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
        if let Some(playlist) = playlists.iter_mut().find(|p| p.id == playlist_id) {
            for track in tracks {
                playlist.add_track(track);
            }
        }
        drop(playlists);
        self.mark_save_pending();
    }

    // ==================== Deck Operations ====================

    /// Load a track to a deck (1 or 2). Updates both the audio engine and shared state.
    /// Returns true if successful.
    pub fn load_track_to_deck(mut self, mut track: Track, deck: usize) -> bool {
        let track_id = track.id;
        let track_path = track.path.clone();
        let needs_waveform = !track.has_waveform();

        // Check if waveform is in cache
        if needs_waveform {
            if let Some(peaks) = self.get_waveform(track_id) {
                track.set_waveform(peaks);
            }
        }

        // Load track to audio engine
        let loaded = if let Ok(mut eng) = (self.engine)().lock() {
            let deck_idx = deck - 1; // Convert to 0-indexed (deck 1 = index 0)
            eng.load_track(deck_idx, track.clone()).is_ok()
        } else {
            false
        };

        if loaded {
            // Update shared deck state
            if deck == 1 {
                self.deck1_track.set(Some(track.clone()));
            } else if deck == 2 {
                self.deck2_track.set(Some(track.clone()));
            }

            // Spawn background waveform analysis if needed
            if needs_waveform && self.get_waveform(track_id).is_none() {
                spawn_waveform_analysis(self, track_id, track_path);
            }
        }

        loaded
    }

    /// Update deck track with new waveform data.
    pub fn update_deck_waveform(mut self, track_id: Uuid, peaks: Vec<f32>) {
        // Update deck 1 if it has this track
        let deck1_update = {
            let track_opt = self.deck1_track.read().clone();
            if let Some(mut track) = track_opt {
                if track.id == track_id && !track.has_waveform() {
                    track.set_waveform(peaks.clone());
                    Some(track)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(track) = deck1_update {
            self.deck1_track.set(Some(track));
        }

        // Update deck 2 if it has this track
        let deck2_update = {
            let track_opt = self.deck2_track.read().clone();
            if let Some(mut track) = track_opt {
                if track.id == track_id && !track.has_waveform() {
                    track.set_waveform(peaks);
                    Some(track)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(track) = deck2_update {
            self.deck2_track.set(Some(track));
        }
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
        let new_diagnoses: Vec<Diagnosis> = doctor
            .check_track(&track)
            .into_iter()
            .collect();

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
    pub fn fix_all_by_check(mut self, check_name: &str) -> (usize, usize) {
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
    pub fn fix_all_for_track(mut self, track_id: Uuid) -> (usize, usize) {
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
    /// If `write_to_file` is true, also saves the changes to the audio file.
    pub fn update_track_metadata(
        mut self,
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
        // Update library tracks
        let mut library_tracks = self.library_tracks.write();
        if let Some(track) = library_tracks.iter_mut().find(|t| t.id == track_id) {
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

            if write_to_file {
                if let Err(e) = track.save_metadata() {
                    eprintln!("Failed to save metadata to file: {}", e);
                    // Continue anyway, memory state is updated
                }
            }
        }
        drop(library_tracks);

        // Also update in playlists
        let mut playlists = self.playlists.write();
        for playlist in playlists.iter_mut() {
            if let Some(track) = playlist.tracks.iter_mut().find(|t| t.id == track_id) {
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

                if write_to_file {
                    let _ = track.save_metadata();
                }
            }
        }
        drop(playlists);

        // Update deck tracks if loaded
        let deck1_update = {
            let deck1_track = self.deck1_track.read().clone();
            if let Some(mut track) = deck1_track {
                if track.id == track_id {
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
                    Some(track)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(track) = deck1_update {
            self.deck1_track.set(Some(track));
        }

        let deck2_update = {
            let deck2_track = self.deck2_track.read().clone();
            if let Some(mut track) = deck2_track {
                if track.id == track_id {
                    track.update_metadata(
                        title,
                        artist,
                        album,
                        genre,
                        bpm,
                        key,
                        year,
                        track_number,
                        comment,
                        rating,
                    );
                    Some(track)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(track) = deck2_update {
            self.deck2_track.set(Some(track));
        }

        self.mark_save_pending();

        // Re-run doctor for this track since metadata changed
        self.recheck_track_diagnoses(track_id);

        Ok(())
    }

    /// Update metadata for multiple tracks (batch operation).
    /// Only non-None fields will be updated.
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

    // ==================== Theme Operations ====================

    /// Get the current theme name.
    pub fn get_theme(&self) -> String {
        self.current_theme.read().clone()
    }

    /// Set the current theme and save to storage.
    pub async fn set_theme(mut self, theme: String) {
        self.current_theme.set(theme.clone());

        // Save to storage
        if let Some(storage) = self.storage.read().as_ref() {
            if let Err(e) = storage.set_setting("theme", &theme).await {
                eprintln!("Failed to save theme setting: {}", e);
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

/// Reference to a playlist (for display purposes).
#[derive(Debug, Clone, PartialEq)]
pub struct PlaylistRef {
    pub id: Uuid,
    pub name: String,
}

/// Provide the app state to the component tree.
pub fn use_app_state_provider() {
    use_context_provider(AppState::new);
}

/// Get the app state from context.
pub fn use_app_state() -> AppState {
    use_context::<AppState>()
}

/// Hook to initialize storage and set up auto-save.
pub fn use_storage_init() {
    let app_state = use_app_state();
    
    // Initialize storage on mount
    use_future(move || async move {
        app_state.init_storage().await;
    });
    
    // Auto-save when changes are pending (debounced)
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if app_state.take_save_pending() {
                app_state.save_to_storage().await;
            }
        }
    });

    // Poll for waveform results and apply them
    use_future(move || async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let results = app_state.poll_waveform_results();
            for result in results {
                app_state.apply_waveform_result(result);
            }
        }
    });
}

/// Spawn background waveform analysis for a track.
/// Uses std::thread::spawn for CPU-intensive work as per Dioxus docs.
/// Results are sent via channel and processed by use_storage_init's polling loop.
pub fn spawn_waveform_analysis(app_state: AppState, track_id: Uuid, path: PathBuf) {
    // Check if already analyzing or has waveform
    if app_state.is_analyzing(track_id) || app_state.get_waveform(track_id).is_some() {
        return;
    }

    // Mark as analyzing
    app_state.mark_analyzing(track_id);

    // Clone the sender before spawning (Sender is Send)
    let tx = (*app_state.waveform_tx.read()).clone();

    // Spawn on a separate thread to avoid blocking UI
    // As per Dioxus docs: "If you have an expensive task you need to run, 
    // you should spawn it on a separate thread using std::thread::spawn"
    std::thread::spawn(move || {
        eprintln!("Starting waveform extraction for {:?}", path);
        
        match extract_waveform(&path) {
            Ok(waveform) => {
                eprintln!("Extracted waveform: {} peaks for track {}", waveform.peaks.len(), track_id);
                // Send result back via channel
                let _ = tx.send(WaveformResult {
                    track_id,
                    peaks: waveform.peaks,
                });
            }
            Err(e) => {
                eprintln!("Waveform extraction failed for {}: {}", track_id, e);
            }
        }
    });
}
