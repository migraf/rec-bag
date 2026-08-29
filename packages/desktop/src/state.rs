//! Shared application state using Dioxus Context API.
//!
//! Everything persistent lives in the one [`Catalog`]. `AppState` is the composition
//! root for what the running application holds on top of it: the reactive projection
//! views browse ([`CatalogProjection`]), what the Doctor last found ([`Diagnostics`]), the
//! playback decks, and the transient state that belongs to no record — which tracks are
//! being analyzed right now, and which theme is on.
//!
//! Each of those has its own module and its own interface. `AppState` owns only what
//! spans them: adopting the Catalog, and the edits that have to reach the projection,
//! the decks, and the Catalog together.

pub mod browser;
pub mod deck;
pub mod diagnostics;
pub mod integration;
pub mod metadata;
pub mod projection;
#[cfg(test)]
pub mod test_support;

use crate::services::catalog::{Catalog, CatalogError};
use deck::{Deck, SharedAudio, DECK_COUNT};
use diagnostics::Diagnostics;
use dioxus::prelude::*;
use metadata::MetadataEdit;
use projection::{CatalogProjection, PlaylistWriter};
use rec_core::{
    analyze_track, AudioEngine, BeatGridData, FrequencyWaveformData, LibraryEntry, Playlist, Track,
    TrackAnalysis, WaveformPeaks,
};
use std::collections::HashSet;
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
    /// The live Catalog projection every view browses.
    projection: CatalogProjection,
    /// What the Doctor last found in that projection.
    diagnostics: Diagnostics,
    /// Tracks currently being analyzed.
    analyzing_tracks: Signal<HashSet<Uuid>>,
    /// Channel sender for analysis results.
    analysis_tx: Signal<AnalysisSender>,
    /// Channel receiver for analysis results (wrapped in Arc<Mutex> for sharing).
    analysis_rx: Signal<Arc<StdMutex<AnalysisReceiver>>>,
    /// Current theme name (e.g., "dark", "light").
    theme: Signal<String>,
}

impl AppState {
    /// Create a new app state with initialized audio engine.
    pub fn new() -> Self {
        let engine = AudioEngine::two_deck().expect("Failed to initialize audio engine");
        let audio: SharedAudio = Arc::new(StdMutex::new(engine));
        let audio = Signal::new(audio);
        let (tx, rx) = mpsc::channel();
        let projection = CatalogProjection::new();

        Self {
            decks: std::array::from_fn(|index| Deck::new(audio, index)),
            catalog: Signal::new(None),
            projection,
            diagnostics: Diagnostics::new(projection),
            analyzing_tracks: Signal::new(HashSet::new()),
            analysis_tx: Signal::new(tx),
            analysis_rx: Signal::new(Arc::new(StdMutex::new(rx))),
            theme: Signal::new("dark".to_string()),
        }
    }

    /// The live Catalog projection every view browses.
    pub fn projection(&self) -> CatalogProjection {
        self.projection
    }

    /// What the Doctor last found in the projection.
    pub fn diagnostics(&self) -> Diagnostics {
        self.diagnostics
    }

    // ==================== Catalog ====================

    /// Install the Catalog and load everything it holds.
    ///
    /// The integration runtime owns catalog startup, so this is the composition root's
    /// hand-off rather than something the application state opens for itself.
    pub async fn adopt_catalog(mut self, catalog: Arc<Catalog>) -> Result<(), CatalogError> {
        self.catalog.set(Some(catalog.clone()));
        self.projection
            .persist_to(Arc::new(CatalogPlaylists(catalog.clone())));
        if let Some(theme) = catalog.setting("theme").await? {
            self.theme.set(theme);
        }
        self.refresh_catalog().await
    }

    /// The Catalog, if the integration runtime has opened it.
    pub fn catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.read().clone()
    }

    /// Re-read the tracks, playlists, and sources from the Catalog.
    ///
    /// A failure is returned rather than logged, because the caller is the integration
    /// runner that shows the user retryable errors.
    pub async fn refresh_catalog(self) -> Result<(), CatalogError> {
        let Some(catalog) = self.catalog() else {
            return Ok(());
        };
        let tracks = catalog.tracks().await?;
        let playlists = catalog.playlists(&tracks).await?;
        let sources = catalog.sources().await?;
        self.projection.replace(tracks, playlists, sources);
        Ok(())
    }

    /// Create a playlist holding every track a source has observed.
    ///
    /// Membership is selected by canonical identity through the source's occurrences,
    /// never by matching a filesystem prefix: a canonical track has no path of its own.
    pub async fn create_playlist_from_source(self, source_id: Uuid) -> Option<Uuid> {
        let catalog = self.catalog()?;
        let name = self
            .projection
            .sources()
            .into_iter()
            .find(|source| source.id == source_id)
            .map(|source| source.name)?;
        let track_ids = match catalog.track_ids_for_source(source_id).await {
            Ok(track_ids) => track_ids,
            Err(error) => {
                eprintln!("Could not read the tracks of source {source_id}: {error}");
                return None;
            }
        };
        let tracks: Vec<Track> = self
            .projection
            .tracks()
            .into_iter()
            .filter(|track| track_ids.contains(&track.id))
            .collect();
        if tracks.is_empty() {
            return None;
        }

        Some(
            self.projection
                .add_playlist(Playlist::with_tracks(name, tracks)),
        )
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

    // ==================== Metadata ====================

    /// Apply one metadata-editor submission to every track it names.
    ///
    /// Rating and comment are the user's own record and go to the track's library
    /// entry; everything else describes the audio, so the submission decides whether it
    /// is also written back into the file's tags. A track the projection no longer
    /// holds is skipped rather than reported: the editor was opened on rows that were
    /// on screen, so a gap means the Catalog moved on underneath it.
    pub fn update_metadata(self, track_ids: &[Uuid], edit: &MetadataEdit) {
        for track_id in track_ids {
            self.update_track_metadata(*track_id, edit);
        }
    }

    /// Apply a submission to one track.
    fn update_track_metadata(self, track_id: Uuid, edit: &MetadataEdit) {
        let found = self.edit_track_everywhere(track_id, |track| {
            edit.apply_to(track);
            if edit.write_to_file && track.path.is_file() {
                if let Err(error) = track.save_metadata() {
                    eprintln!("Failed to save metadata to file: {error}");
                }
            }
        });
        if !found {
            return;
        }

        self.save_library_entry(track_id, edit);
        self.save_metadata_edits(track_id, edit);
        // Metadata changed, so what the Doctor knows about this track is now stale.
        self.diagnostics.recheck(track_id);
    }

    /// Persist metadata the user authored, as candidates on the canonical track.
    fn save_metadata_edits(self, track_id: Uuid, edit: &MetadataEdit) {
        let candidates = edit.candidates();
        if candidates.is_empty() {
            return;
        }
        let Some(catalog) = self.catalog() else {
            return;
        };
        spawn(async move {
            if let Err(error) = catalog.record_metadata_edits(track_id, candidates).await {
                eprintln!("Could not save metadata for track {track_id}: {error}");
            }
        });
    }

    /// Persist the user's own record about a track.
    fn save_library_entry(self, track_id: Uuid, edit: &MetadataEdit) {
        if !edit.touches_library_entry() {
            return;
        }
        let Some(catalog) = self.catalog() else {
            return;
        };
        let edit = edit.clone();
        spawn(async move {
            let mut entry = match catalog.library_entry(track_id).await {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("Could not read the library entry for {track_id}: {error}");
                    LibraryEntry::new(track_id)
                }
            };
            edit.apply_to_entry(&mut entry, chrono::Utc::now());
            if let Err(error) = catalog.save_library_entry(entry).await {
                eprintln!("Could not save the library entry for {track_id}: {error}");
            }
        });
    }

    // ==================== Analysis Operations ====================

    /// Check if a track is currently being analyzed.
    pub fn is_analyzing(&self, track_id: Uuid) -> bool {
        self.analyzing_tracks.read().contains(&track_id)
    }

    /// Mark a track as being analyzed.
    pub fn mark_analyzing(mut self, track_id: Uuid) {
        self.analyzing_tracks.write().insert(track_id);
    }

    /// Poll for completed analysis results (non-blocking).
    /// Returns all available results.
    fn poll_analysis_results(self) -> Vec<TrackAnalysisResult> {
        let mut results = Vec::new();
        if let Ok(rx) = self.analysis_rx.read().try_lock() {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }
        results
    }

    /// Apply analysis to the projection, the decks, and the Catalog.
    async fn apply_analysis_result(mut self, result: TrackAnalysisResult) {
        let TrackAnalysisResult { track_id, analysis } = result;

        self.analyzing_tracks.write().remove(&track_id);
        let Some(analysis) = analysis else {
            return;
        };
        self.edit_track_everywhere(track_id, |track| analysis.apply_to(track));

        if let Some(catalog) = self.catalog() {
            if let Err(error) = catalog.save_analysis(analysis).await {
                eprintln!("Could not save analysis for track {track_id}: {error}");
            }
        }
    }

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

    /// Apply `edit` to every copy of `track_id`: the projection, and any deck holding
    /// it. Returns whether the projection held the track.
    ///
    /// One track is shown in several places at once, so a change reaching only one of
    /// them is a bug rather than an optimization.
    fn edit_track_everywhere(self, track_id: Uuid, edit: impl Fn(&mut Track)) -> bool {
        let found = self.projection.edit_track(track_id, &edit);
        for deck in self.decks {
            deck.edit_track(track_id, &edit);
        }
        found
    }

    // ==================== Theme Operations ====================

    /// The current theme name.
    pub fn theme(&self) -> String {
        self.theme.read().clone()
    }

    /// Set the current theme and save it to the Catalog.
    pub async fn set_theme(mut self, theme: String) {
        self.theme.set(theme.clone());
        if let Some(catalog) = self.catalog() {
            if let Err(error) = catalog.set_setting("theme", &theme).await {
                eprintln!("Could not save the theme preference: {error}");
            }
        }
    }

    /// Toggle between dark and light themes.
    pub async fn toggle_theme(self) {
        let new_theme = if self.theme() == "dark" {
            "light"
        } else {
            "dark"
        };
        self.set_theme(new_theme.to_string()).await;
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Playlist edits, written through to the Catalog.
///
/// Saving is fire-and-forget from the gesture's point of view: the projection already
/// shows the new membership, and a failure is reported rather than rolled back.
struct CatalogPlaylists(Arc<Catalog>);

impl PlaylistWriter for CatalogPlaylists {
    fn save(&self, playlist: Playlist) {
        let catalog = self.0.clone();
        spawn(async move {
            let playlist_id = playlist.id;
            if let Err(error) = catalog.save_playlist(&playlist).await {
                eprintln!("Could not save playlist {playlist_id}: {error}");
            }
        });
    }

    fn delete(&self, playlist_id: Uuid) {
        let catalog = self.0.clone();
        spawn(async move {
            if let Err(error) = catalog.delete_playlist(playlist_id).await {
                eprintln!("Could not delete playlist {playlist_id}: {error}");
            }
        });
    }
}

/// Provide the app state to the component tree.
pub fn use_app_state_provider() {
    let app_state = use_context_provider(AppState::new);
    // One refresh loop feeds every view, so a deck reads the same wherever it is shown.
    use_future(move || async move {
        // [DEBUG-4e77] Does the refresh loop actually keep ticking?
        static TICKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        deck::debug_4e77("refresh loop: STARTED");
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if deck::debug_4e77_every(&TICKS, 20) {
                deck::debug_4e77("refresh loop: tick");
            }
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
