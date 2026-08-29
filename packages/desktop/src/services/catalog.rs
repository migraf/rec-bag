//! The one Catalog the application reads and writes.
//!
//! Everything the interface shows about music — tracks, playlists, sources, the user's
//! own library entries, cached analysis, and preferences — is projected from the
//! canonical repositories here. There is no second track model and no second schema.
//!
//! A canonical track deliberately has no filesystem path. Playback therefore never
//! reads a path off a track: it asks [`Catalog::load_track`] for a [`LoadedTrack`], and
//! that one function is the only place a concrete audio file is resolved.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rec_core::{
    CanonicalPlaylist, CanonicalPlaylistEntry, CanonicalTrack, CatalogSource, CatalogSourceKind,
    LibraryEntry, LibraryEntryRepository, MetadataField, MetadataValue, MusicSource, Playlist,
    PlaylistRepository, RepositoryPageRequest, Setting, SettingsRepository, SourceRepository,
    SourceType, Track, TrackAnalysis, TrackAnalysisRepository, TrackRepository,
};
use rec_persistence::TursoCatalog;
use uuid::Uuid;

/// A local file prepared for immediate playback or analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPlaybackPath {
    pub path: PathBuf,
    /// True when the resolver materialized a disposable cache entry rather than
    /// handing back an owned original.
    pub cached: bool,
}

pub type PlaybackPathFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ResolvedPlaybackPath, CatalogError>> + Send + 'a>>;

/// Where a canonical track's audio can be read from right now.
///
/// The Catalog is given one of these when it is built, so the resolver is a value the
/// composition root passes in rather than a process-wide slot anything can reach.
pub trait PlaybackPaths: Send + Sync {
    fn resolve<'a>(&'a self, track_id: Uuid) -> PlaybackPathFuture<'a>;
}

/// A canonical track prepared for playback: identity and reconciled metadata, the
/// analysis derived from its audio, and a concrete file to decode.
///
/// A loaded track is a moment in the life of a track, not a record of one. Nothing here
/// is written back to the catalog, so a cache copy of a remote asset never becomes the
/// track's identity.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedTrack {
    /// The canonical track as the interface shows it, with its analysis attached.
    pub track: Track,
    /// The audio file the deck decodes.
    pub path: PathBuf,
    /// Whether `path` is a disposable cache entry.
    pub cached: bool,
}

impl LoadedTrack {
    /// The canonical track's identity.
    pub fn id(&self) -> Uuid {
        self.track.id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogError {
    message: String,
}

impl CatalogError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CatalogError {}

type CatalogResult<T> = Result<T, CatalogError>;

fn read_error(what: &str) -> impl FnOnce(rec_core::RepositoryError) -> CatalogError + '_ {
    move |error| CatalogError::new(format!("could not read {what} from the catalog: {error}"))
}

fn write_error(what: &str) -> impl FnOnce(rec_core::RepositoryError) -> CatalogError + '_ {
    move |error| CatalogError::new(format!("could not save {what} to the catalog: {error}"))
}

/// One Catalog over the canonical repositories.
pub struct Catalog {
    store: Arc<TursoCatalog>,
    paths: Arc<dyn PlaybackPaths>,
}

impl Catalog {
    pub fn new(store: Arc<TursoCatalog>, paths: Arc<dyn PlaybackPaths>) -> Self {
        Self { store, paths }
    }

    /// Every canonical track, projected with the user's library entry, its cached
    /// analysis, and a local path when one is known.
    pub async fn library(&self) -> CatalogResult<Vec<Track>> {
        let canonical = self.canonical_tracks().await?;
        let entries = self.library_entries().await?;
        let analysis = self.analysis_by_track().await?;
        let paths = self
            .store
            .local_source_paths_by_track()
            .await
            .map_err(read_error("local source paths"))?;

        Ok(canonical
            .into_iter()
            .map(|track| {
                let id = track.id;
                library_track(
                    track,
                    entries.get(&id),
                    analysis.get(&id),
                    paths.get(&id).and_then(|paths| paths.first()).cloned(),
                )
            })
            .collect())
    }

    /// Every canonical playlist, with its membership resolved against `library`.
    ///
    /// Membership references tracks by identity, so a playlist entry whose track has
    /// left the catalog is dropped rather than shown as a hole.
    pub async fn playlists(&self, library: &[Track]) -> CatalogResult<Vec<Playlist>> {
        let by_id: HashMap<Uuid, &Track> = library.iter().map(|track| (track.id, track)).collect();
        let mut playlists = Vec::new();
        let mut cursor = None;
        loop {
            let page = PlaylistRepository::list(
                self.store.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await
            .map_err(read_error("playlists"))?;
            for canonical in page.items {
                playlists.push(Playlist {
                    id: canonical.id,
                    name: canonical.name,
                    description: canonical.description,
                    tracks: canonical
                        .entries
                        .iter()
                        .filter_map(|entry| {
                            by_id.get(&entry.track_id).map(|track| (*track).clone())
                        })
                        .collect(),
                    created_at: canonical.created_at,
                    modified_at: canonical.updated_at,
                });
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(playlists)
    }

    /// Every configured catalog source, as the sources panel shows them.
    pub async fn sources(&self) -> CatalogResult<Vec<MusicSource>> {
        let counts = self
            .store
            .occurrence_counts_by_source()
            .await
            .map_err(read_error("source track counts"))?;
        let mut sources = Vec::new();
        let mut cursor = None;
        loop {
            let page = SourceRepository::list(
                self.store.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(200),
                },
            )
            .await
            .map_err(read_error("sources"))?;
            for source in page.items {
                let count = counts.get(&source.id).copied().unwrap_or(0);
                sources.push(music_source(source, count));
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(sources)
    }

    /// Prepare a canonical track for a deck.
    ///
    /// This is the only seam between a track's identity and a file on disk. It resolves
    /// the track's audio through the storage resolver it was given, and refuses a path
    /// that is not an available local file rather than handing the audio engine
    /// something it cannot decode.
    pub async fn load_track(&self, track_id: Uuid) -> CatalogResult<LoadedTrack> {
        let canonical = TrackRepository::get(self.store.as_ref(), track_id)
            .await
            .map_err(read_error("a track"))?
            .ok_or_else(|| CatalogError::new(format!("track {track_id} is not in the catalog")))?;
        let entry = LibraryEntryRepository::get(self.store.as_ref(), track_id)
            .await
            .map_err(read_error("a library entry"))?;
        let analysis = TrackAnalysisRepository::get(self.store.as_ref(), track_id)
            .await
            .map_err(read_error("track analysis"))?;
        let resolved = self.paths.resolve(track_id).await?;
        if !resolved.path.is_file() {
            return Err(CatalogError::new(format!(
                "resolved playback path is not an available local file: {}",
                resolved.path.display()
            )));
        }

        Ok(LoadedTrack {
            track: library_track(
                canonical,
                entry.as_ref(),
                analysis.as_ref(),
                Some(resolved.path.clone()),
            ),
            path: resolved.path,
            cached: resolved.cached,
        })
    }

    /// Record the user's own values for a canonical track's metadata fields.
    ///
    /// Each edit becomes a user-authored candidate rather than an overwrite, so a later
    /// provider observation cannot silently undo it and withdrawing an edit falls back
    /// to what was observed.
    pub async fn record_metadata_edits(
        &self,
        track_id: Uuid,
        edits: Vec<(MetadataField, Option<MetadataValue>)>,
    ) -> CatalogResult<()> {
        if edits.is_empty() {
            return Ok(());
        }
        let mut canonical = TrackRepository::get(self.store.as_ref(), track_id)
            .await
            .map_err(read_error("a track"))?
            .ok_or_else(|| CatalogError::new(format!("track {track_id} is not in the catalog")))?;
        let now = chrono::Utc::now();
        for (field, value) in edits {
            canonical.metadata.retain_user_edit(field, value, now);
        }
        canonical.updated_at = now;
        TrackRepository::upsert(self.store.as_ref(), canonical)
            .await
            .map_err(write_error("a track"))
    }

    /// The user's own record about a track, or an empty one when nothing is recorded.
    pub async fn library_entry(&self, track_id: Uuid) -> CatalogResult<LibraryEntry> {
        Ok(LibraryEntryRepository::get(self.store.as_ref(), track_id)
            .await
            .map_err(read_error("a library entry"))?
            .unwrap_or_else(|| LibraryEntry::new(track_id)))
    }

    pub async fn save_library_entry(&self, entry: LibraryEntry) -> CatalogResult<()> {
        LibraryEntryRepository::upsert(self.store.as_ref(), entry)
            .await
            .map_err(write_error("a library entry"))
    }

    pub async fn save_analysis(&self, analysis: TrackAnalysis) -> CatalogResult<()> {
        TrackAnalysisRepository::upsert(self.store.as_ref(), analysis)
            .await
            .map_err(write_error("track analysis"))
    }

    pub async fn forget_analysis(&self, track_id: Uuid) -> CatalogResult<()> {
        TrackAnalysisRepository::remove(self.store.as_ref(), track_id)
            .await
            .map_err(write_error("track analysis"))
    }

    pub async fn save_playlist(&self, playlist: &Playlist) -> CatalogResult<()> {
        PlaylistRepository::upsert(
            self.store.as_ref(),
            CanonicalPlaylist {
                id: playlist.id,
                name: playlist.name.clone(),
                description: playlist.description.clone(),
                entries: playlist
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(position, track)| CanonicalPlaylistEntry {
                        position: position as u32,
                        track_id: track.id,
                    })
                    .collect(),
                created_at: playlist.created_at,
                updated_at: playlist.modified_at,
            },
        )
        .await
        .map_err(write_error("a playlist"))
    }

    pub async fn delete_playlist(&self, id: Uuid) -> CatalogResult<()> {
        PlaylistRepository::remove(self.store.as_ref(), id)
            .await
            .map_err(write_error("a playlist"))
    }

    /// A stored preference, or `None` when it has never been set.
    pub async fn setting(&self, key: &str) -> CatalogResult<Option<String>> {
        let setting = SettingsRepository::get(self.store.as_ref(), key.to_owned())
            .await
            .map_err(read_error("a setting"))?;
        Ok(setting.map(|setting| match setting.value {
            serde_json::Value::String(value) => value,
            other => other.to_string(),
        }))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> CatalogResult<()> {
        SettingsRepository::set(
            self.store.as_ref(),
            Setting {
                key: key.to_owned(),
                value: serde_json::Value::String(value.to_owned()),
                updated_at: chrono::Utc::now(),
            },
        )
        .await
        .map_err(write_error("a setting"))
    }

    async fn canonical_tracks(&self) -> CatalogResult<Vec<CanonicalTrack>> {
        let mut tracks = Vec::new();
        let mut cursor = None;
        loop {
            let page = TrackRepository::list(
                self.store.as_ref(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(500),
                },
            )
            .await
            .map_err(read_error("tracks"))?;
            tracks.extend(page.items);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(tracks)
    }

    async fn library_entries(&self) -> CatalogResult<BTreeMap<Uuid, LibraryEntry>> {
        Ok(LibraryEntryRepository::list(self.store.as_ref())
            .await
            .map_err(read_error("library entries"))?
            .into_iter()
            .map(|entry| (entry.track_id, entry))
            .collect())
    }

    async fn analysis_by_track(&self) -> CatalogResult<BTreeMap<Uuid, TrackAnalysis>> {
        Ok(TrackAnalysisRepository::list(self.store.as_ref())
            .await
            .map_err(read_error("track analysis"))?
            .into_iter()
            .map(|analysis| (analysis.track_id, analysis))
            .collect())
    }
}

/// Project one canonical track into the record the interface renders.
///
/// `path` is informational: it is where the track's audio is known to sit on disk, and
/// it is empty for a track that only exists at a provider. Playback resolves its own
/// path through [`Catalog::load_track`] and never reads this one.
fn library_track(
    canonical: CanonicalTrack,
    entry: Option<&LibraryEntry>,
    analysis: Option<&TrackAnalysis>,
    path: Option<PathBuf>,
) -> Track {
    let mut track = Track::new_without_metadata(path.unwrap_or_default());
    track.id = canonical.id;
    track.added_at = entry.map_or(canonical.created_at, |entry| entry.added_at);
    track.title = text(&canonical, MetadataField::Title).unwrap_or_else(|| "Unknown track".into());
    track.artist = text(&canonical, MetadataField::Artist);
    track.album = text(&canonical, MetadataField::Album);
    track.genre = text(&canonical, MetadataField::Genre);
    track.key = text(&canonical, MetadataField::MusicalKey);
    track.bpm = number(&canonical, MetadataField::Bpm).map(|value| value as f32);
    track.duration = integer(&canonical, MetadataField::DurationMs)
        .and_then(|value| u64::try_from(value).ok())
        .map(Duration::from_millis);
    track.year =
        integer(&canonical, MetadataField::ReleaseYear).and_then(|value| u32::try_from(value).ok());
    track.track_number =
        integer(&canonical, MetadataField::TrackNumber).and_then(|value| u32::try_from(value).ok());

    if let Some(entry) = entry {
        track.rating = entry.rating;
        track.comment = entry.comment.clone();
        track.play_count = entry.play_count;
        track.last_played = entry.last_played_at;
    }
    if let Some(analysis) = analysis {
        track.waveform = analysis.waveform.clone();
        track.freq_waveform = analysis.freq_waveform.clone();
        track.beat_grid = analysis.beat_grid.clone();
        track.bpm = track.bpm.or(analysis.bpm);
        track.key = track.key.clone().or_else(|| analysis.key.clone());
    }
    track
}

/// Project one catalog source into the record the sources panel renders.
fn music_source(source: CatalogSource, track_count: u32) -> MusicSource {
    MusicSource {
        id: source.id,
        name: source.name,
        source_type: match source.kind {
            CatalogSourceKind::LocalFolder => SourceType::Local,
            // The panel's type only decides an icon; a source it has no icon for is
            // still shown rather than hidden.
            _ => SourceType::Local,
        },
        uri: source.config.get("path").cloned().unwrap_or_default(),
        config: None,
        track_count,
        enabled: source.enabled,
        created_at: source.created_at,
        last_scanned: source.last_synced_at,
    }
}

fn value(canonical: &CanonicalTrack, field: MetadataField) -> Option<&MetadataValue> {
    canonical
        .metadata
        .preferred(&field)
        .map(|candidate| &candidate.value)
}

fn text(canonical: &CanonicalTrack, field: MetadataField) -> Option<String> {
    match value(canonical, field)? {
        MetadataValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}

fn integer(canonical: &CanonicalTrack, field: MetadataField) -> Option<i64> {
    match value(canonical, field)? {
        MetadataValue::Integer(value) => Some(*value),
        _ => None,
    }
}

fn number(canonical: &CanonicalTrack, field: MetadataField) -> Option<f64> {
    match value(canonical, field)? {
        MetadataValue::Integer(value) => Some(*value as f64),
        MetadataValue::Decimal(value) => value.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rec_core::{
        CanonicalMetadata, MetadataFieldValue, MetadataOrigin, MetadataProvenance, WaveformPeaks,
    };
    use std::sync::Mutex;

    struct StubPaths {
        resolved: Mutex<HashMap<Uuid, Result<ResolvedPlaybackPath, CatalogError>>>,
    }

    impl StubPaths {
        fn new(resolved: HashMap<Uuid, Result<ResolvedPlaybackPath, CatalogError>>) -> Arc<Self> {
            Arc::new(Self {
                resolved: Mutex::new(resolved),
            })
        }
    }

    impl PlaybackPaths for StubPaths {
        fn resolve<'a>(&'a self, track_id: Uuid) -> PlaybackPathFuture<'a> {
            let result = self
                .resolved
                .lock()
                .expect("stub resolver lock poisoned")
                .get(&track_id)
                .cloned()
                .unwrap_or_else(|| Err(CatalogError::new("no stub path for this track")));
            Box::pin(async move { result })
        }
    }

    fn canonical_track(title: &str, artist: &str) -> CanonicalTrack {
        let mut track = CanonicalTrack::new();
        let mut metadata = CanonicalMetadata::new();
        for (field, value) in [
            (MetadataField::Title, title),
            (MetadataField::Artist, artist),
        ] {
            metadata.add(MetadataFieldValue::new(
                field,
                MetadataValue::Text(value.to_string()),
                MetadataProvenance::new(MetadataOrigin::User, Utc::now()),
            ));
        }
        track.metadata = metadata;
        track
    }

    fn temp_audio(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("rec-bag-catalog-{label}-{}", Uuid::new_v4()));
        std::fs::write(&path, b"audio").unwrap();
        path
    }

    async fn catalog(paths: Arc<dyn PlaybackPaths>) -> (Catalog, Arc<TursoCatalog>) {
        let store = Arc::new(TursoCatalog::in_memory().await.unwrap());
        (Catalog::new(store.clone(), paths), store)
    }

    #[tokio::test]
    async fn the_library_carries_canonical_metadata_the_users_entry_and_its_analysis() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("Canonical title", "Canonical artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();
        let mut entry = LibraryEntry::new(track.id);
        entry.rating = Some(5);
        entry.comment = Some("keeper".into());
        entry.record_play(Utc::now());
        LibraryEntryRepository::upsert(store.as_ref(), entry)
            .await
            .unwrap();
        TrackAnalysisRepository::upsert(
            store.as_ref(),
            TrackAnalysis {
                waveform: Some(WaveformPeaks {
                    peaks: vec![0.1, 0.9],
                }),
                bpm: Some(128.0),
                ..TrackAnalysis::new(track.id)
            },
        )
        .await
        .unwrap();

        let library = catalog.library().await.unwrap();

        assert_eq!(library.len(), 1);
        let shown = &library[0];
        assert_eq!(shown.id, track.id);
        assert_eq!(shown.title, "Canonical title");
        assert_eq!(shown.artist.as_deref(), Some("Canonical artist"));
        assert_eq!(shown.rating, Some(5));
        assert_eq!(shown.comment.as_deref(), Some("keeper"));
        assert_eq!(shown.play_count, 1);
        assert_eq!(shown.bpm, Some(128.0));
        assert_eq!(
            shown.waveform.as_ref().map(|waveform| waveform.peaks.len()),
            Some(2)
        );
    }

    /// A canonical track has no path of its own. Nothing may invent one for it.
    #[tokio::test]
    async fn a_track_with_no_local_file_is_shown_without_a_path() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("Streaming only", "Remote artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        let library = catalog.library().await.unwrap();

        assert_eq!(library[0].path, PathBuf::new());
    }

    #[tokio::test]
    async fn loading_a_track_resolves_a_concrete_file_without_changing_its_identity() {
        let track = canonical_track("Drive track", "Drive artist");
        let cache = temp_audio("drive-cache");
        let paths = StubPaths::new(HashMap::from([(
            track.id,
            Ok(ResolvedPlaybackPath {
                path: cache.clone(),
                cached: true,
            }),
        )]));
        let (catalog, store) = catalog(paths).await;
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        let loaded = catalog.load_track(track.id).await.unwrap();

        assert_eq!(loaded.id(), track.id);
        assert_eq!(loaded.track.title, "Drive track");
        assert_eq!(loaded.path, cache);
        assert!(loaded.cached);
        // The catalog record is untouched: nothing wrote the cache path back.
        assert_eq!(catalog.library().await.unwrap()[0].path, PathBuf::new());
        std::fs::remove_file(cache).unwrap();
    }

    #[tokio::test]
    async fn a_resolved_path_that_is_not_a_file_is_refused() {
        let track = canonical_track("Missing", "Nobody");
        let paths = StubPaths::new(HashMap::from([(
            track.id,
            Ok(ResolvedPlaybackPath {
                path: PathBuf::from("/does/not/exist.mp3"),
                cached: false,
            }),
        )]));
        let (catalog, store) = catalog(paths).await;
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        let error = catalog.load_track(track.id).await.unwrap_err();

        assert!(error.to_string().contains("not an available local file"));
    }

    #[tokio::test]
    async fn loading_a_track_the_catalog_does_not_hold_is_an_error() {
        let (catalog, _store) = catalog(StubPaths::new(HashMap::new())).await;

        let error = catalog.load_track(Uuid::new_v4()).await.unwrap_err();

        assert!(error.to_string().contains("is not in the catalog"));
    }

    #[tokio::test]
    async fn playlist_membership_resolves_against_the_library_by_identity() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let kept = canonical_track("Kept", "Artist");
        TrackRepository::upsert(store.as_ref(), kept.clone())
            .await
            .unwrap();
        let departed = Uuid::new_v4();
        let now = Utc::now();
        PlaylistRepository::upsert(
            store.as_ref(),
            CanonicalPlaylist {
                id: Uuid::new_v4(),
                name: "Set".into(),
                description: None,
                entries: vec![CanonicalPlaylistEntry {
                    position: 0,
                    track_id: kept.id,
                }],
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .unwrap();

        let library = catalog.library().await.unwrap();
        let playlists = catalog.playlists(&library).await.unwrap();

        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].tracks.len(), 1);
        assert_eq!(playlists[0].tracks[0].id, kept.id);
        assert!(playlists[0].tracks.iter().all(|track| track.id != departed));
    }

    #[tokio::test]
    async fn a_saved_playlist_round_trips_through_the_canonical_stack() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("Kept", "Artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();
        let library = catalog.library().await.unwrap();
        let mut playlist = Playlist::new("Warmup");
        playlist.add_track(library[0].clone());

        catalog.save_playlist(&playlist).await.unwrap();

        let reloaded = catalog.playlists(&library).await.unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].name, "Warmup");
        assert_eq!(reloaded[0].tracks[0].id, track.id);
    }

    #[tokio::test]
    async fn a_user_metadata_edit_becomes_a_canonical_candidate_and_survives_a_reload() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("Observed title", "Observed artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        catalog
            .record_metadata_edits(
                track.id,
                vec![
                    (
                        MetadataField::Title,
                        Some(MetadataValue::Text("Corrected title".into())),
                    ),
                    (
                        MetadataField::Genre,
                        Some(MetadataValue::Text("Dub".into())),
                    ),
                ],
            )
            .await
            .unwrap();

        let library = catalog.library().await.unwrap();
        assert_eq!(library[0].title, "Corrected title");
        assert_eq!(library[0].genre.as_deref(), Some("Dub"));
    }

    /// Editing the same field twice must not leave two competing user candidates.
    #[tokio::test]
    async fn re_editing_a_field_replaces_the_users_previous_value() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("First", "Artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        for title in ["Second", "Third"] {
            catalog
                .record_metadata_edits(
                    track.id,
                    vec![(
                        MetadataField::Title,
                        Some(MetadataValue::Text(title.into())),
                    )],
                )
                .await
                .unwrap();
        }

        let stored = TrackRepository::get(store.as_ref(), track.id)
            .await
            .unwrap()
            .unwrap();
        let user_titles = stored
            .metadata
            .values
            .iter()
            .filter(|value| {
                value.field == MetadataField::Title
                    && value.provenance.origin == MetadataOrigin::User
            })
            .count();
        assert_eq!(user_titles, 1);
        assert_eq!(catalog.library().await.unwrap()[0].title, "Third");
    }

    #[tokio::test]
    async fn a_setting_round_trips() {
        let (catalog, _store) = catalog(StubPaths::new(HashMap::new())).await;

        assert_eq!(catalog.setting("theme").await.unwrap(), None);
        catalog.set_setting("theme", "light").await.unwrap();
        assert_eq!(
            catalog.setting("theme").await.unwrap().as_deref(),
            Some("light")
        );
    }

    #[tokio::test]
    async fn a_local_folder_source_is_shown_with_its_path_and_track_count() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let source = CatalogSource::local_folder("Music", "/music");
        SourceRepository::upsert(store.as_ref(), source.clone())
            .await
            .unwrap();

        let sources = catalog.sources().await.unwrap();

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].id, source.id);
        assert_eq!(sources[0].uri, "/music");
        assert_eq!(sources[0].source_type, SourceType::Local);
        assert_eq!(sources[0].track_count, 0);
    }
}
