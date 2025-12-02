//! Shared application state using Dioxus Context API.

use dioxus::prelude::*;
use rec_core::{
    extract_waveform, AudioEngine, Library, MusicSource, Playlist, SearchEngine, SqliteStorage,
    Track,
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
