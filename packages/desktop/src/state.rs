//! Shared application state using Dioxus Context API.

use dioxus::prelude::*;
use rec_core::{extract_waveform, AudioEngine, Library, Playlist, SqliteStorage, Track};
use std::collections::HashMap;
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
    /// Scanned library tracks (not in any playlist yet)
    pub library_tracks: Signal<Vec<Track>>,
    /// Currently selected playlist ID
    pub selected_playlist_id: Signal<Option<Uuid>>,
    /// Flag to trigger save
    save_pending: Signal<bool>,
    /// Waveform results cache (for reactive updates)
    pub waveform_cache: Signal<HashMap<Uuid, Vec<f32>>>,
    /// Tracks currently being analyzed
    pub analyzing_tracks: Signal<std::collections::HashSet<Uuid>>,
    /// Channel sender for waveform results
    pub waveform_tx: Signal<WaveformSender>,
    /// Channel receiver for waveform results (wrapped in Arc<Mutex> for sharing)
    waveform_rx: Signal<Arc<StdMutex<WaveformReceiver>>>,
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
            selected_playlist_id: Signal::new(None),
            save_pending: Signal::new(false),
            waveform_cache: Signal::new(HashMap::new()),
            analyzing_tracks: Signal::new(std::collections::HashSet::new()),
            waveform_tx: Signal::new(tx),
            waveform_rx: Signal::new(Arc::new(StdMutex::new(rx))),
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
                
                self.storage.set(Some(storage));
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

    /// Scan a folder for audio files.
    pub fn scan_folder(mut self, path: &str) {
        if path.is_empty() {
            return;
        }

        if let Ok(entries) = std::fs::read_dir(path) {
            let mut found_tracks = Vec::new();
            for entry in entries.flatten() {
                let file_path = entry.path();
                if file_path.is_file() {
                    if let Some(ext) = file_path.extension() {
                        let ext = ext.to_string_lossy().to_lowercase();
                        if matches!(ext.as_str(), "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac") {
                            found_tracks.push(Track::new(file_path));
                        }
                    }
                }
            }
            self.library_tracks.set(found_tracks);
            self.mark_save_pending();
        }
    }
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
