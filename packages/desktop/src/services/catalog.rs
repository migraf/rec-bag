//! The one Catalog the application reads and writes.
//!
//! Everything the interface shows about music — tracks, playlists, sources, the user's
//! own library entries, cached analysis, and preferences — is projected from the
//! canonical repositories here. There is no second track model and no second schema.
//!
//! A canonical track deliberately has no filesystem path. Playback therefore never
//! reads a path off a track: it asks [`Catalog::load_track`] for a [`LoadedTrack`], and
//! that one function is the only place a track's audio is resolved — to a local file
//! when one exists, otherwise to a provider stream held in memory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use rec_core::{
    AudioInput, CanonicalPlaylist, CanonicalPlaylistEntry, CanonicalTrack, CatalogSource,
    CatalogSourceKind, ExternalPlaylistFilter, LibraryEntry, LibraryEntryRepository, MetadataField,
    MetadataValue, MusicSource, Playlist, PlaylistOrigin, PlaylistRepository, RepositoryPage,
    RepositoryPageRequest, Setting, SettingsRepository, SourceCollectionRelationshipFilter,
    SourceEntityKind, SourceOccurrenceFilter, SourceRepository, SourceType, Track, TrackAnalysis,
    TrackAnalysisRepository, TrackRepository,
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

/// A provider stream of a track's audio, fetched into memory for one playback session.
#[derive(Clone)]
pub struct StreamedAudio {
    pub bytes: Arc<[u8]>,
    pub origin: StreamOrigin,
}

/// Who is streaming a track. Whatever plays it must credit them and link back
/// (SoundCloud API Terms, section 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamOrigin {
    pub source_type: SourceType,
    /// The track's public page at the provider.
    pub permalink: Option<String>,
    /// Whether this is only a short preview rather than the full track.
    pub preview: bool,
}

pub type StreamFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<StreamedAudio>, CatalogError>> + Send + 'a>>;

/// Where a track with no local file can be streamed from.
///
/// `Ok(None)` means no connected provider offers the track; an error means one does but the
/// stream could not be fetched, which is worth telling the user.
pub trait TrackStreams: Send + Sync {
    fn stream<'a>(&'a self, track_id: Uuid) -> StreamFuture<'a>;
}

/// A canonical track prepared for playback: identity and reconciled metadata, the
/// analysis derived from its audio, and the audio to decode.
///
/// A loaded track is a moment in the life of a track, not a record of one. Nothing here
/// is written back to the catalog, so a cache copy of a remote asset never becomes the
/// track's identity, and a stream never outlives the deck holding it.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedTrack {
    /// The canonical track as the interface shows it, with its analysis attached.
    pub track: Track,
    /// The audio the deck decodes: a local file, or a stream held in memory.
    pub audio: AudioInput,
    /// Whether the audio is a disposable cache entry.
    pub cached: bool,
    /// The provider streaming the audio, when it is not a local file.
    pub streamed_from: Option<StreamOrigin>,
}

impl LoadedTrack {
    /// The canonical track's identity.
    pub fn id(&self) -> Uuid {
        self.track.id
    }

    /// The local file being played, which analysis can read. `None` for a stream.
    pub fn path(&self) -> Option<&Path> {
        self.audio.path()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogError {
    message: String,
    /// Whether the same call is worth making again. A resolver or repository that was
    /// momentarily unavailable is retryable; a track that is not in the catalog is not.
    pub retryable: bool,
}

impl CatalogError {
    /// An error the caller should not simply repeat.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    /// The same error, marked as worth retrying.
    pub fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
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
    move |error| repository_error(format!("could not read {what} from the catalog"), error)
}

fn write_error(what: &str) -> impl FnOnce(rec_core::RepositoryError) -> CatalogError + '_ {
    move |error| repository_error(format!("could not save {what} to the catalog"), error)
}

fn repository_error(context: String, error: rec_core::RepositoryError) -> CatalogError {
    CatalogError {
        message: format!("{context}: {error}"),
        retryable: error.is_retryable(),
    }
}

/// Read every page of a repository listing.
///
/// Repository listings are cursor-paged so no single query can grow unbounded. Callers
/// projecting a whole catalog want the whole thing, and should not each re-implement the
/// walk.
async fn all_pages<T, F>(what: &str, mut page_of: F) -> CatalogResult<Vec<T>>
where
    F: AsyncFnMut(RepositoryPageRequest) -> Result<RepositoryPage<T>, rec_core::RepositoryError>,
{
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let page = page_of(RepositoryPageRequest {
            cursor,
            limit: Some(PAGE_SIZE),
        })
        .await
        .map_err(read_error(what))?;
        items.extend(page.items);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(items),
        }
    }
}

/// How many records to read per repository page.
const PAGE_SIZE: u32 = 500;

/// One Catalog over the canonical repositories.
pub struct Catalog {
    store: Arc<TursoCatalog>,
    paths: Arc<dyn PlaybackPaths>,
    streams: Option<Arc<dyn TrackStreams>>,
}

impl Catalog {
    pub fn new(store: Arc<TursoCatalog>, paths: Arc<dyn PlaybackPaths>) -> Self {
        Self {
            store,
            paths,
            streams: None,
        }
    }

    /// Stream tracks that have no local file from `streams`.
    pub fn with_streams(mut self, streams: Arc<dyn TrackStreams>) -> Self {
        self.streams = Some(streams);
        self
    }

    /// Every canonical track, projected with the user's library entry, its cached
    /// analysis, and a local path when one is known.
    pub async fn tracks(&self) -> CatalogResult<Vec<Track>> {
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
                catalog_track(
                    track,
                    entries.get(&id),
                    analysis.get(&id),
                    paths.get(&id).and_then(|paths| paths.first()).cloned(),
                )
            })
            .collect())
    }

    /// The canonical tracks a source has observed, by identity rather than by path.
    pub async fn track_ids_for_source(&self, source_id: Uuid) -> CatalogResult<HashSet<Uuid>> {
        let occurrences = all_pages("source occurrences", async |page| {
            SourceRepository::list_occurrences(
                self.store.as_ref(),
                SourceOccurrenceFilter {
                    source_id: Some(source_id),
                    ..Default::default()
                },
                page,
            )
            .await
        })
        .await?;
        Ok(occurrences
            .into_iter()
            .filter_map(|occurrence| occurrence.canonical_track_id)
            .collect())
    }

    /// Every canonical playlist, then every playlist mirrored from a catalog source, with
    /// membership resolved against `library`.
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
                    origin: None,
                });
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        playlists.extend(self.mirrored_playlists(&by_id).await?);
        Ok(playlists)
    }

    /// The playlists each provider source currently reports, read-only and in provider
    /// order.
    ///
    /// A playlist row outlives its deletion or unliking at the source; only its collection
    /// relationship goes. So a playlist is mirrored only while a top-level relationship
    /// still names it, which is what the source reported on its last full sync.
    async fn mirrored_playlists(
        &self,
        library: &HashMap<Uuid, &Track>,
    ) -> CatalogResult<Vec<Playlist>> {
        let sources = all_pages("sources", async |page| {
            SourceRepository::list(self.store.as_ref(), page).await
        })
        .await?;
        let mut mirrored = Vec::new();
        for source in sources {
            if source.kind == CatalogSourceKind::LocalFolder {
                continue;
            }
            let source_id = source.id;
            let current: HashSet<String> = all_pages("source collections", async |page| {
                SourceRepository::list_collection_relationships(
                    self.store.as_ref(),
                    SourceCollectionRelationshipFilter {
                        source_id,
                        account_external_id: None,
                        collection: None,
                        entity_kind: Some(SourceEntityKind::Playlist),
                    },
                    page,
                )
                .await
            })
            .await?
            .into_iter()
            .filter(|relationship| relationship.identity.parent_external_id.is_none())
            .map(|relationship| relationship.identity.entity_external_id)
            .collect();
            if current.is_empty() {
                continue;
            }
            let track_by_external_id: HashMap<String, Uuid> =
                all_pages("source occurrences", async |page| {
                    SourceRepository::list_occurrences(
                        self.store.as_ref(),
                        SourceOccurrenceFilter {
                            source_id: Some(source_id),
                            ..Default::default()
                        },
                        page,
                    )
                    .await
                })
                .await?
                .into_iter()
                .filter_map(|occurrence| {
                    occurrence
                        .canonical_track_id
                        .map(|track_id| (occurrence.identity.external_id, track_id))
                })
                .collect();
            let external = all_pages("source playlists", async |page| {
                PlaylistRepository::list_external(
                    self.store.as_ref(),
                    ExternalPlaylistFilter {
                        source_id: Some(source_id),
                        owner_external_id: None,
                    },
                    page,
                )
                .await
            })
            .await?;
            for playlist in external
                .into_iter()
                .filter(|playlist| current.contains(&playlist.external_id))
            {
                let mut entries = playlist.entries;
                entries.sort_by_key(|entry| entry.position);
                mirrored.push(Playlist {
                    // Stable across refreshes, so selecting a mirrored playlist survives a sync.
                    id: Uuid::new_v5(&source_id, playlist.external_id.as_bytes()),
                    name: playlist.name,
                    description: playlist.description,
                    tracks: entries
                        .iter()
                        .filter_map(|entry| track_by_external_id.get(&entry.track.external_id))
                        .filter_map(|track_id| library.get(track_id))
                        .map(|track| (*track).clone())
                        .collect(),
                    created_at: playlist.last_seen_at,
                    modified_at: playlist.last_seen_at,
                    origin: Some(PlaylistOrigin {
                        source_id,
                        source_type: source_type(&source.kind),
                        external_url: playlist.external_url,
                    }),
                });
            }
        }
        mirrored.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(mirrored)
    }

    /// Every configured catalog source, as the sources panel shows them.
    pub async fn sources(&self) -> CatalogResult<Vec<MusicSource>> {
        let counts = self
            .store
            .occurrence_counts_by_source()
            .await
            .map_err(read_error("source track counts"))?;
        Ok(all_pages("sources", async |page| {
            SourceRepository::list(self.store.as_ref(), page).await
        })
        .await?
        .into_iter()
        .map(|source| {
            let count = counts.get(&source.id).copied().unwrap_or(0);
            music_source(source, count)
        })
        .collect())
    }

    /// Prepare a canonical track for a deck.
    ///
    /// This is the only seam between a track's identity and its audio. It resolves the
    /// track through the storage resolver it was given, and refuses a path that is not an
    /// available local file rather than handing the audio engine something it cannot
    /// decode. A track with no local file is streamed instead, when a provider offers it;
    /// an owned file always wins over a stream.
    pub async fn load_track(&self, track_id: Uuid) -> CatalogResult<LoadedTrack> {
        self.load(track_id, true).await
    }

    /// Prepare a canonical track's local file, never a stream: for reading the audio
    /// rather than playing it, such as analysis.
    pub async fn load_local_track(&self, track_id: Uuid) -> CatalogResult<LoadedTrack> {
        self.load(track_id, false).await
    }

    async fn load(&self, track_id: Uuid, allow_stream: bool) -> CatalogResult<LoadedTrack> {
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
        let local = self.paths.resolve(track_id).await.and_then(|resolved| {
            if resolved.path.is_file() {
                Ok(resolved)
            } else {
                Err(CatalogError::retryable(format!(
                    "resolved playback path is not an available local file: {}",
                    resolved.path.display()
                )))
            }
        });
        let resolved = match local {
            Ok(resolved) => resolved,
            Err(no_file) if !allow_stream => return Err(no_file),
            Err(no_file) => {
                let Some(stream) = self.stream(track_id).await? else {
                    return Err(no_file);
                };
                let track = catalog_track(canonical, entry.as_ref(), analysis.as_ref(), None);
                return Ok(LoadedTrack {
                    track,
                    audio: AudioInput::Memory(stream.bytes),
                    cached: false,
                    streamed_from: Some(stream.origin),
                });
            }
        };

        let mut track = catalog_track(
            canonical,
            entry.as_ref(),
            analysis.as_ref(),
            Some(resolved.path.clone()),
        );
        // A deck must know how long its file runs: the trackbar spans that length, so a
        // track the catalog has no duration for cannot be seeked at all. The file itself
        // is the answer, and it is right here. It stays on the loaded track rather than
        // going back to the catalog, which only records what a scan observed.
        if track.duration.is_none() {
            track.duration = file_duration(&resolved.path).await;
        }

        Ok(LoadedTrack {
            track,
            audio: AudioInput::File(resolved.path),
            cached: resolved.cached,
            streamed_from: None,
        })
    }

    async fn stream(&self, track_id: Uuid) -> CatalogResult<Option<StreamedAudio>> {
        match &self.streams {
            Some(streams) => streams.stream(track_id).await,
            None => Ok(None),
        }
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
            canonical.metadata.set_user_value(field, value, now);
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
        all_pages("tracks", async |page| {
            TrackRepository::list(self.store.as_ref(), page).await
        })
        .await
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

/// How long the audio at `path` runs, according to the file itself.
///
/// Reading a file's properties is blocking work, so it runs off the async executor.
/// A file that will not answer yields `None` rather than failing the load.
async fn file_duration(path: &Path) -> Option<Duration> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || rec_core::read_embedded_tags(&path).ok()?.duration)
        .await
        .ok()
        .flatten()
}

/// Project one canonical track into the record the interface renders.
///
/// `path` is informational: it is where the track's audio is known to sit on disk, and
/// it is empty for a track that only exists at a provider. Playback resolves its own
/// path through [`Catalog::load_track`] and never reads this one.
fn catalog_track(
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
        analysis.apply_to(&mut track);
    }
    track
}

/// Project one catalog source into the record the sources panel renders.
fn music_source(source: CatalogSource, track_count: u32) -> MusicSource {
    MusicSource {
        id: source.id,
        name: source.name,
        // The panel browses a local source's filesystem tree and summarizes any other
        // kind, so calling a provider source local would mount a folder browser on
        // something that has no folder.
        source_type: source_type(&source.kind),
        uri: source.config.get("path").cloned().unwrap_or_default(),
        config: None,
        track_count,
        enabled: source.enabled,
        created_at: source.created_at,
        last_scanned: source.last_synced_at,
    }
}

fn source_type(kind: &CatalogSourceKind) -> SourceType {
    match kind {
        CatalogSourceKind::LocalFolder => SourceType::Local,
        CatalogSourceKind::SoundCloud => SourceType::SoundCloud,
        CatalogSourceKind::Custom(_) => SourceType::WebDav,
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

    /// A real, decodable audio file of a known length.
    ///
    /// The fixture has to be genuine audio: what a deck can seek through depends on how
    /// long its file actually runs, so a placeholder would test nothing.
    fn temp_audio(label: &str) -> PathBuf {
        temp_audio_of(label, Duration::from_secs(2))
    }

    fn temp_audio_of(label: &str, length: Duration) -> PathBuf {
        const SAMPLE_RATE: u32 = 44_100;
        let path =
            std::env::temp_dir().join(format!("rec-bag-catalog-{label}-{}.wav", Uuid::new_v4()));
        let samples = (length.as_secs_f64() * SAMPLE_RATE as f64).round() as u32;
        let data_len = samples * 2;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // PCM header length
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        std::fs::write(&path, wav).unwrap();
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

        let library = catalog.tracks().await.unwrap();

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

    /// Analysis is inferred; a tempo the user or a provider stated outranks it. The one
    /// merge rule must hold wherever analysis meets a track — projection and deck alike.
    #[tokio::test]
    async fn analysis_fills_gaps_but_never_replaces_reconciled_metadata() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let mut track = canonical_track("Stated", "Artist");
        track.metadata.add(MetadataFieldValue::new(
            MetadataField::Bpm,
            MetadataValue::Decimal("124".into()),
            MetadataProvenance::new(MetadataOrigin::User, Utc::now()),
        ));
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();
        TrackAnalysisRepository::upsert(
            store.as_ref(),
            TrackAnalysis {
                bpm: Some(128.0),
                key: Some("Am".into()),
                ..TrackAnalysis::new(track.id)
            },
        )
        .await
        .unwrap();

        let shown = catalog.tracks().await.unwrap().remove(0);

        assert_eq!(shown.bpm, Some(124.0), "the stated tempo wins");
        assert_eq!(shown.key.as_deref(), Some("Am"), "the empty key is filled");
    }

    /// A canonical track has no path of its own. Nothing may invent one for it.
    #[tokio::test]
    async fn a_track_with_no_local_file_is_shown_without_a_path() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("Streaming only", "Remote artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        let library = catalog.tracks().await.unwrap();

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
        assert_eq!(loaded.path(), Some(cache.as_path()));
        assert!(loaded.cached);
        // The catalog record is untouched: nothing wrote the cache path back.
        assert_eq!(catalog.tracks().await.unwrap()[0].path, PathBuf::new());
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

        let library = catalog.tracks().await.unwrap();
        let playlists = catalog.playlists(&library).await.unwrap();

        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].tracks.len(), 1);
        assert_eq!(playlists[0].tracks[0].id, kept.id);
        assert!(playlists[0].tracks.iter().all(|track| track.id != departed));
    }

    /// A SoundCloud source holding two linked tracks, a playlist it still reports (in the
    /// reverse of canonical order), and one it no longer does.
    async fn a_soundcloud_source(
        store: &TursoCatalog,
    ) -> (CatalogSource, CanonicalTrack, CanonicalTrack) {
        let source = CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud);
        SourceRepository::upsert(store, source.clone())
            .await
            .unwrap();
        SourceRepository::upsert_account(
            store,
            rec_core::SourceAccount::new(source.id, "soundcloud:users:1"),
        )
        .await
        .unwrap();
        let first = canonical_track("First", "Uploader");
        let second = canonical_track("Second", "Uploader");
        let mut occurrences = Vec::new();
        for (external_id, track) in [
            ("soundcloud:tracks:1", &first),
            ("soundcloud:tracks:2", &second),
        ] {
            TrackRepository::upsert(store, track.clone()).await.unwrap();
            let mut occurrence = rec_core::SourceTrackOccurrence::new(source.id, external_id);
            occurrence.canonical_track_id = Some(track.id);
            occurrence.availability = rec_core::SourceAvailability::StreamingOnly;
            occurrence.external_url = Some(format!("https://soundcloud.com/u/{external_id}"));
            occurrences.push(occurrence);
        }
        SourceRepository::upsert_occurrences(store, occurrences)
            .await
            .unwrap();
        for (external_id, current) in [
            ("soundcloud:playlists:9", true),
            ("soundcloud:playlists:8", false),
        ] {
            let mut playlist = rec_core::ExternalPlaylist::new(
                source.id,
                external_id,
                format!("Set {external_id}"),
            );
            playlist.external_url = Some("https://soundcloud.com/u/sets/set".into());
            playlist.entries = vec![
                rec_core::ExternalPlaylistEntry::new(
                    1,
                    rec_core::SourceTrackIdentity::new(source.id, "soundcloud:tracks:1"),
                ),
                rec_core::ExternalPlaylistEntry::new(
                    0,
                    rec_core::SourceTrackIdentity::new(source.id, "soundcloud:tracks:2"),
                ),
            ];
            PlaylistRepository::upsert_external(store, playlist)
                .await
                .unwrap();
            if current {
                SourceRepository::upsert_collection_relationship(
                    store,
                    rec_core::SourceCollectionRelationship::new(
                        rec_core::SourceCollectionRelationshipIdentity::new(
                            source.id,
                            "soundcloud:users:1",
                            rec_core::SourceCollection::OwnedPlaylists,
                            SourceEntityKind::Playlist,
                            external_id,
                        ),
                        Utc::now(),
                    ),
                )
                .await
                .unwrap();
            }
        }
        (source, first, second)
    }

    #[tokio::test]
    async fn a_soundcloud_playlist_is_mirrored_read_only_in_provider_order() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let (source, first, second) = a_soundcloud_source(&store).await;

        let library = catalog.tracks().await.unwrap();
        let playlists = catalog.playlists(&library).await.unwrap();

        assert_eq!(
            playlists.len(),
            1,
            "only the playlist SoundCloud still reports"
        );
        let mirrored = &playlists[0];
        assert!(!mirrored.is_editable());
        assert_eq!(
            mirrored.origin,
            Some(PlaylistOrigin {
                source_id: source.id,
                source_type: SourceType::SoundCloud,
                external_url: Some("https://soundcloud.com/u/sets/set".into()),
            })
        );
        assert_eq!(
            mirrored
                .tracks
                .iter()
                .map(|track| track.id)
                .collect::<Vec<_>>(),
            [second.id, first.id]
        );
        // The same identity on every refresh, so a browsed playlist stays selected.
        let again = catalog.playlists(&library).await.unwrap();
        assert_eq!(again[0].id, mirrored.id);
    }

    struct StubStreams(Mutex<u32>);

    impl TrackStreams for StubStreams {
        fn stream<'a>(&'a self, _track_id: Uuid) -> StreamFuture<'a> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async {
                Ok(Some(StreamedAudio {
                    bytes: Arc::from(&b"mp3"[..]),
                    origin: StreamOrigin {
                        source_type: SourceType::SoundCloud,
                        permalink: Some("https://soundcloud.com/u/t".into()),
                        preview: false,
                    },
                }))
            })
        }
    }

    #[tokio::test]
    async fn a_track_with_no_local_file_is_streamed_into_memory() {
        let track = canonical_track("Streamed", "Uploader");
        let local = canonical_track("Owned", "Me");
        let file = temp_audio("owned");
        let paths = StubPaths::new(HashMap::from([(
            local.id,
            Ok(ResolvedPlaybackPath {
                path: file.clone(),
                cached: false,
            }),
        )]));
        let store = Arc::new(TursoCatalog::in_memory().await.unwrap());
        let streams = Arc::new(StubStreams(Mutex::new(0)));
        let catalog = Catalog::new(store.clone(), paths).with_streams(streams.clone());
        for track in [&track, &local] {
            TrackRepository::upsert(store.as_ref(), track.clone())
                .await
                .unwrap();
        }

        let streamed = catalog.load_track(track.id).await.unwrap();
        assert_eq!(streamed.audio, AudioInput::Memory(Arc::from(&b"mp3"[..])));
        assert_eq!(streamed.path(), None);
        assert_eq!(
            streamed.streamed_from.map(|origin| origin.source_type),
            Some(SourceType::SoundCloud)
        );

        // An owned file wins over a stream, and analysis never streams.
        let owned = catalog.load_track(local.id).await.unwrap();
        assert_eq!(owned.path(), Some(file.as_path()));
        assert!(owned.streamed_from.is_none());
        assert!(catalog.load_local_track(track.id).await.is_err());
        assert_eq!(*streams.0.lock().unwrap(), 1);
        std::fs::remove_file(file).unwrap();
    }

    #[tokio::test]
    async fn a_saved_playlist_round_trips_through_the_canonical_stack() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        let track = canonical_track("Kept", "Artist");
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();
        let library = catalog.tracks().await.unwrap();
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

        let library = catalog.tracks().await.unwrap();
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
        assert_eq!(catalog.tracks().await.unwrap()[0].title, "Third");
    }

    /// The panel mounts a filesystem browser on a local source and summarizes anything
    /// else, so a provider source must not be projected as a local folder.
    #[tokio::test]
    async fn a_provider_source_is_not_projected_as_a_local_folder() {
        let (catalog, store) = catalog(StubPaths::new(HashMap::new())).await;
        SourceRepository::upsert(
            store.as_ref(),
            CatalogSource::new("SoundCloud", CatalogSourceKind::SoundCloud),
        )
        .await
        .unwrap();

        let sources = catalog.sources().await.unwrap();

        assert_eq!(sources[0].source_type, SourceType::SoundCloud);
        assert!(!sources[0].is_local());
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

    // ---- DJ-view seek probe ------------------------------------------------
    //
    // The DJ view hands `LoadedTrack::track` straight to `DeckControl`, whose Trackbar
    // renders the seek slider as `min=0 max={duration_ms}`. A zero duration makes that
    // slider a single point: it is the whole seek gesture, so it is asserted end to end.

    mod probe {
        use dioxus::prelude::*;
        use rec_core::Track;

        #[component]
        pub fn SeekProbe(track: Option<Track>) -> Element {
            rsx! {
                ui::DeckControl {
                    deck_number: 1,
                    state: ui::DeckDisplayState::Stopped,
                    track,
                    position: std::time::Duration::ZERO,
                    volume: 1.0,
                    on_play: |_| {},
                    on_pause: |_| {},
                    on_stop: |_| {},
                    on_volume_change: |_| {},
                    on_seek: |_| {},
                    on_file_drop: |_| {},
                }
            }
        }
    }
    use probe::{SeekProbe, SeekProbeProps};

    /// The largest position the rendered seek slider can be dragged to, in milliseconds.
    fn seekable_range_ms(track: Track) -> u64 {
        let mut dom = dioxus::prelude::VirtualDom::new_with_props(
            SeekProbe,
            SeekProbeProps { track: Some(track) },
        );
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        let slider = html
            .split("<input")
            .find(|tag| tag.contains("trackbar-slider"))
            .and_then(|tag| tag.split('>').next())
            .unwrap_or_else(|| panic!("no seek slider was rendered:\n{html}"));
        assert!(
            !slider.contains("disabled"),
            "the seek slider rendered disabled:\n{slider}"
        );
        let max = slider
            .split("max=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or_else(|| panic!("the seek slider has no max:\n{slider}"));
        max.parse().unwrap()
    }

    /// A length the catalog knows is the one the trackbar spans; the file is not consulted.
    #[tokio::test]
    async fn a_duration_the_catalog_knows_is_what_the_trackbar_spans() {
        let mut track = canonical_track("Scanned track", "Scanned artist");
        track.metadata.add(MetadataFieldValue::new(
            MetadataField::DurationMs,
            MetadataValue::Integer(240_000),
            MetadataProvenance::new(MetadataOrigin::User, Utc::now()),
        ));
        // Two seconds of audio, four minutes claimed: what the catalog knows wins.
        let file = temp_audio_of("seek-known", Duration::from_secs(2));
        let paths = StubPaths::new(HashMap::from([(
            track.id,
            Ok(ResolvedPlaybackPath {
                path: file,
                cached: false,
            }),
        )]));
        let (catalog, store) = catalog(paths).await;
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        let loaded = catalog.load_track(track.id).await.unwrap();

        assert_eq!(seekable_range_ms(loaded.track), 240_000);
    }

    /// The user's symptom: the seek slider on a loaded deck does nothing at all.
    #[tokio::test]
    async fn a_loaded_track_can_be_seeked_through_its_whole_length() {
        // Exactly what a local folder scan writes: one title candidate, nothing else.
        let track = canonical_track("Scanned track", "Scanned artist");
        let file = temp_audio("seek");
        let paths = StubPaths::new(HashMap::from([(
            track.id,
            Ok(ResolvedPlaybackPath {
                path: file.clone(),
                cached: false,
            }),
        )]));
        let (catalog, store) = catalog(paths).await;
        TrackRepository::upsert(store.as_ref(), track.clone())
            .await
            .unwrap();

        let loaded = catalog.load_track(track.id).await.unwrap();

        let span = seekable_range_ms(loaded.track);
        assert!(
            span > 0,
            "the seek slider spans no time, so no drag can reach any position"
        );
        assert!(
            (1_900..=2_100).contains(&span),
            "the slider should span the file's own two seconds, but spans {span}ms"
        );
    }
}
