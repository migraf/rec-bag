//! Shared application state using Dioxus Context API.

use dioxus::prelude::*;
use rec_core::{
    analyze_track, AudioEngine, Diagnosis, Doctor, Library, MusicSource, Playlist, SearchEngine,
    SqliteStorage, Track,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex as StdMutex};
use uuid::Uuid;

/// Scan a folder recursively for audio files.
/// This is a standalone function to allow running in a blocking thread.
fn scan_folder_for_tracks(path: &str) -> Vec<Track> {
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
                        if matches!(ext.as_str(), "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac") {
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

/// Frequency waveform data for colored visualization.
#[derive(Debug, Clone, Default)]
pub struct FreqWaveformData {
    pub low: Vec<f32>,
    pub mid: Vec<f32>,
    pub high: Vec<f32>,
}

/// Beat grid data for visualization.
#[derive(Debug, Clone, Default)]
pub struct BeatGridInfo {
    pub first_beat_ms: f64,
    pub beat_interval_ms: f64,
    pub beats: Vec<f64>,
}

/// Result from track analysis (waveform, BPM, key, beat grid)
#[derive(Debug, Clone)]
pub struct TrackAnalysisResult {
    pub track_id: Uuid,
    pub peaks: Vec<f32>,
    pub freq_waveform: Option<FreqWaveformData>,
    pub beat_grid: Option<BeatGridInfo>,
    pub bpm: Option<f32>,
    pub key: Option<String>,
}

/// Channel sender for analysis results (thread-safe, clonable)
pub type AnalysisSender = mpsc::Sender<TrackAnalysisResult>;
/// Channel receiver for analysis results
pub type AnalysisReceiver = mpsc::Receiver<TrackAnalysisResult>;

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
    /// Frequency waveform cache
    pub freq_waveform_cache: Signal<HashMap<Uuid, FreqWaveformData>>,
    /// Beat grid cache
    pub beat_grid_cache: Signal<HashMap<Uuid, BeatGridInfo>>,
    /// Tracks currently being analyzed
    pub analyzing_tracks: Signal<HashSet<Uuid>>,
    /// Channel sender for analysis results
    pub analysis_tx: Signal<AnalysisSender>,
    /// Channel receiver for analysis results (wrapped in Arc<Mutex> for sharing)
    analysis_rx: Signal<Arc<StdMutex<AnalysisReceiver>>>,
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
            freq_waveform_cache: Signal::new(HashMap::new()),
            beat_grid_cache: Signal::new(HashMap::new()),
            analyzing_tracks: Signal::new(HashSet::new()),
            analysis_tx: Signal::new(tx),
            analysis_rx: Signal::new(Arc::new(StdMutex::new(rx))),
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
                        // Pre-populate caches from loaded tracks
                        let mut wf_cache = HashMap::new();
                        let mut freq_cache = HashMap::new();
                        let mut beat_cache = HashMap::new();

                        let populate_caches =
                            |track: &Track,
                             wf: &mut HashMap<Uuid, Vec<f32>>,
                             freq: &mut HashMap<Uuid, FreqWaveformData>,
                             beat: &mut HashMap<Uuid, BeatGridInfo>| {
                                if let Some(w) = &track.waveform {
                                    wf.insert(track.id, w.peaks.clone());
                                }
                                if let Some(fw) = &track.freq_waveform {
                                    freq.insert(
                                        track.id,
                                        FreqWaveformData {
                                            low: fw.low.clone(),
                                            mid: fw.mid.clone(),
                                            high: fw.high.clone(),
                                        },
                                    );
                                }
                                if let Some(bg) = &track.beat_grid {
                                    beat.insert(
                                        track.id,
                                        BeatGridInfo {
                                            first_beat_ms: bg.first_beat_ms,
                                            beat_interval_ms: bg.beat_interval_ms,
                                            beats: bg.beats.clone(),
                                        },
                                    );
                                }
                            };

                        for track in &library.loose_tracks {
                            populate_caches(track, &mut wf_cache, &mut freq_cache, &mut beat_cache);
                        }
                        for playlist in &library.playlists {
                            for track in &playlist.tracks {
                                populate_caches(
                                    track,
                                    &mut wf_cache,
                                    &mut freq_cache,
                                    &mut beat_cache,
                                );
                            }
                        }
                        self.waveform_cache.set(wf_cache);
                        self.freq_waveform_cache.set(freq_cache);
                        self.beat_grid_cache.set(beat_cache);
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

    /// Get frequency waveform for a track from cache.
    pub fn get_freq_waveform(&self, track_id: Uuid) -> Option<FreqWaveformData> {
        self.freq_waveform_cache.read().get(&track_id).cloned()
    }

    /// Get beat grid for a track from cache.
    pub fn get_beat_grid(&self, track_id: Uuid) -> Option<BeatGridInfo> {
        self.beat_grid_cache.read().get(&track_id).cloned()
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

    /// Apply analysis result to the state (waveform, BPM, key, beat grid).
    pub fn apply_analysis_result(mut self, result: TrackAnalysisResult) {
        let track_id = result.track_id;
        let peaks = result.peaks;
        let freq_waveform = result.freq_waveform;
        let beat_grid = result.beat_grid;
        let bpm = result.bpm;
        let key = result.key;

        // Update caches for reactive updates
        self.waveform_cache.write().insert(track_id, peaks.clone());

        if let Some(ref fw) = freq_waveform {
            self.freq_waveform_cache
                .write()
                .insert(track_id, fw.clone());
        }

        if let Some(ref bg) = beat_grid {
            self.beat_grid_cache.write().insert(track_id, bg.clone());
        }

        // Remove from analyzing set
        self.analyzing_tracks.write().remove(&track_id);

        // Update deck tracks if they have this track
        self.update_deck_analysis(
            track_id,
            peaks.clone(),
            freq_waveform.clone(),
            beat_grid.clone(),
            bpm,
            key.clone(),
        );

        // Helper to apply all analysis data to a track
        fn apply_to_track(
            track: &mut Track,
            peaks: Vec<f32>,
            freq_waveform: Option<FreqWaveformData>,
            beat_grid: Option<BeatGridInfo>,
            bpm: Option<f32>,
            key: Option<String>,
        ) {
            track.set_waveform(peaks);
            if let Some(fw) = freq_waveform {
                track.set_freq_waveform(fw.low, fw.mid, fw.high);
            }
            if let Some(bg) = beat_grid {
                track.set_beat_grid(bg.first_beat_ms, bg.beat_interval_ms, bg.beats);
            }
            if let Some(bpm_val) = bpm {
                track.set_bpm(bpm_val);
            }
            if let Some(key_val) = key {
                track.set_key(key_val);
            }
        }

        // Update library tracks
        let mut library_tracks = self.library_tracks.write();
        if let Some(track) = library_tracks.iter_mut().find(|t| t.id == track_id) {
            apply_to_track(
                track,
                peaks.clone(),
                freq_waveform.clone(),
                beat_grid.clone(),
                bpm,
                key.clone(),
            );
            drop(library_tracks);
            self.mark_save_pending();
            return;
        }
        drop(library_tracks);

        // Update playlist tracks
        let mut playlists = self.playlists.write();
        for playlist in playlists.iter_mut() {
            if let Some(track) = playlist.tracks.iter_mut().find(|t| t.id == track_id) {
                apply_to_track(track, peaks, freq_waveform, beat_grid, bpm, key);
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

        // Scan the folder for tracks in a separate thread to avoid blocking UI
        let tracks = self.scan_source_folder_async(path.clone()).await;
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
        let source = self
            .sources
            .read()
            .iter()
            .find(|s| s.id == source_id)
            .cloned();

        if let Some(mut source) = source {
            if !source.enabled || !source.is_local() {
                return;
            }

            let path = source.uri.clone();
            // Scan the folder in a separate thread to avoid blocking UI
            let tracks = self.scan_source_folder_async(path).await;
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
        let source = self
            .sources
            .read()
            .iter()
            .find(|s| s.id == source_id)
            .cloned()?;

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
    /// This runs synchronously - use scan_source_folder_async for non-blocking scanning.
    fn scan_source_folder(&self, path: &str) -> Vec<Track> {
        scan_folder_for_tracks(path)
    }

    /// Scan a folder for audio files asynchronously in a separate thread.
    async fn scan_source_folder_async(&self, path: String) -> Vec<Track> {
        tokio::task::spawn_blocking(move || scan_folder_for_tracks(&path))
            .await
            .unwrap_or_default()
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

        // Check if waveform is in cache
        if !track.has_waveform() {
            if let Some(peaks) = self.get_waveform(track_id) {
                track.set_waveform(peaks);
            }
        }

        // Check if freq waveform is in cache
        if !track.has_freq_waveform() {
            if let Some(fw) = self.get_freq_waveform(track_id) {
                track.set_freq_waveform(fw.low, fw.mid, fw.high);
            }
        }

        // Check if beat grid is in cache
        if !track.has_beat_grid() {
            if let Some(bg) = self.get_beat_grid(track_id) {
                track.set_beat_grid(bg.first_beat_ms, bg.beat_interval_ms, bg.beats);
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

            // Spawn background analysis if track needs any analysis
            if track.needs_analysis() {
                spawn_track_analysis(self, track_id, track_path);
            }
        }

        loaded
    }

    /// Update deck track with analysis results (waveform, freq waveform, beat grid, BPM, key).
    pub fn update_deck_analysis(
        mut self,
        track_id: Uuid,
        peaks: Vec<f32>,
        freq_waveform: Option<FreqWaveformData>,
        beat_grid: Option<BeatGridInfo>,
        bpm: Option<f32>,
        key: Option<String>,
    ) {
        // Helper to apply analysis to a track
        fn apply_analysis(
            track: &mut Track,
            peaks: &[f32],
            freq_waveform: &Option<FreqWaveformData>,
            beat_grid: &Option<BeatGridInfo>,
            bpm: Option<f32>,
            key: &Option<String>,
        ) {
            if !track.has_waveform() && !peaks.is_empty() {
                track.set_waveform(peaks.to_vec());
            }
            if !track.has_freq_waveform() {
                if let Some(fw) = freq_waveform {
                    track.set_freq_waveform(fw.low.clone(), fw.mid.clone(), fw.high.clone());
                }
            }
            if !track.has_beat_grid() {
                if let Some(bg) = beat_grid {
                    track.set_beat_grid(bg.first_beat_ms, bg.beat_interval_ms, bg.beats.clone());
                }
            }
            if !track.has_bpm() {
                if let Some(bpm_val) = bpm {
                    track.set_bpm(bpm_val);
                }
            }
            if !track.has_key() {
                if let Some(key_val) = key {
                    track.set_key(key_val.clone());
                }
            }
        }

        // Update deck 1 if it has this track
        let deck1_update = {
            let track_opt = self.deck1_track.read().clone();
            if let Some(mut track) = track_opt {
                if track.id == track_id && track.needs_analysis() {
                    apply_analysis(&mut track, &peaks, &freq_waveform, &beat_grid, bpm, &key);
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
                if track.id == track_id && track.needs_analysis() {
                    apply_analysis(&mut track, &peaks, &freq_waveform, &beat_grid, bpm, &key);
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

    // ==================== Analysis Operations ====================

    /// Analyze a track (clears cached data and spawns fresh analysis).
    pub fn analyze_track(mut self, track_id: Uuid) {
        // Get track path
        let track_path = self
            .library_tracks
            .read()
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.path.clone());

        let Some(path) = track_path else {
            return;
        };

        // Clear cached analysis data
        self.waveform_cache.write().remove(&track_id);
        self.freq_waveform_cache.write().remove(&track_id);
        self.beat_grid_cache.write().remove(&track_id);

        // Clear analysis data from track in library
        let mut library_tracks = self.library_tracks.write();
        if let Some(track) = library_tracks.iter_mut().find(|t| t.id == track_id) {
            track.waveform = None;
            track.freq_waveform = None;
            track.beat_grid = None;
            track.bpm = None;
            track.key = None;
        }
        drop(library_tracks);

        // Clear from playlists too
        let mut playlists = self.playlists.write();
        for playlist in playlists.iter_mut() {
            if let Some(track) = playlist.tracks.iter_mut().find(|t| t.id == track_id) {
                track.waveform = None;
                track.freq_waveform = None;
                track.beat_grid = None;
                track.bpm = None;
                track.key = None;
            }
        }
        drop(playlists);

        // Clear from decks if loaded
        let deck1_clear = self
            .deck1_track
            .read()
            .as_ref()
            .map(|t| t.id == track_id)
            .unwrap_or(false);
        if deck1_clear {
            let mut track = self.deck1_track.write();
            if let Some(ref mut t) = *track {
                t.waveform = None;
                t.freq_waveform = None;
                t.beat_grid = None;
            }
        }

        let deck2_clear = self
            .deck2_track
            .read()
            .as_ref()
            .map(|t| t.id == track_id)
            .unwrap_or(false);
        if deck2_clear {
            let mut track = self.deck2_track.write();
            if let Some(ref mut t) = *track {
                t.waveform = None;
                t.freq_waveform = None;
                t.beat_grid = None;
            }
        }

        // Remove from analyzing set (in case it was stuck)
        self.analyzing_tracks.write().remove(&track_id);

        // Spawn new analysis
        spawn_track_analysis(self, track_id, path);
        self.mark_save_pending();
    }

    /// Analyze multiple tracks.
    pub fn analyze_tracks(self, track_ids: Vec<Uuid>) {
        for track_id in track_ids {
            self.analyze_track(track_id);
        }
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
            let results = app_state.poll_analysis_results();
            for result in results {
                app_state.apply_analysis_result(result);
            }
        }
    });
}

/// Spawn background track analysis (waveform, BPM, key, beat grid detection).
/// Uses std::thread::spawn for CPU-intensive work as per Dioxus docs.
/// Results are sent via channel and processed by use_storage_init's polling loop.
pub fn spawn_track_analysis(app_state: AppState, track_id: Uuid, path: PathBuf) {
    // Check if already analyzing or has waveform
    if app_state.is_analyzing(track_id) || app_state.get_waveform(track_id).is_some() {
        return;
    }

    // Mark as analyzing
    app_state.mark_analyzing(track_id);

    // Clone the sender before spawning (Sender is Send)
    let tx = (*app_state.analysis_tx.read()).clone();

    // Spawn on a separate thread to avoid blocking UI
    // As per Dioxus docs: "If you have an expensive task you need to run,
    // you should spawn it on a separate thread using std::thread::spawn"
    std::thread::spawn(move || {
        eprintln!("Starting track analysis for {:?}", path);

        match analyze_track(&path) {
            Ok(result) => {
                eprintln!(
                    "Analysis complete for track {}: {} peaks, BPM: {:?}, Key: {:?}, Beat grid: {}",
                    track_id,
                    result.waveform_peaks.len(),
                    result.bpm.as_ref().map(|b| b.bpm),
                    result.key.as_ref().map(|k| k.key.to_notation()),
                    result
                        .beat_grid
                        .as_ref()
                        .map(|b| b.beats.len())
                        .unwrap_or(0)
                );

                // Convert frequency waveform
                let freq_waveform = result.freq_waveform.map(|fw| FreqWaveformData {
                    low: fw.low,
                    mid: fw.mid,
                    high: fw.high,
                });

                // Convert beat grid
                let beat_grid = result.beat_grid.map(|bg| BeatGridInfo {
                    first_beat_ms: bg.first_beat_ms,
                    beat_interval_ms: bg.beat_interval_ms,
                    beats: bg.beats,
                });

                // Send result back via channel
                let _ = tx.send(TrackAnalysisResult {
                    track_id,
                    peaks: result.waveform_peaks,
                    freq_waveform,
                    beat_grid,
                    bpm: result.bpm.map(|b| b.bpm),
                    key: result.key.map(|k| k.key.to_string()),
                });
            }
            Err(e) => {
                eprintln!("Track analysis failed for {}: {}", track_id, e);
            }
        }
    });
}
