//! Local catalog source integration slice: local folders rec_bag discovers
//! music in. Local storages are connected through the music storage slice.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use music_storage::{catalog_io, MusicStorages, RootLocator, RootPurpose, StorageError};
use music_storage_local::{is_recognized_audio_path, LocalStorageError};
use rec_core::{
    CanonicalTrack, CatalogSource, CatalogSourceKind, ContentHash, EmbeddedTags, HashAlgorithm,
    MetadataField, MetadataProvenance, MetadataValue, MusicStorageError, OriginalAsset,
    OriginalAssetRole, RepositoryError, SourceAvailability, SourceCopyPolicy, SourceRepository,
    SourceTrackOccurrence, SyncRun, SyncRunKind, SyncRunRepository, TransactionManager,
};
use rec_persistence::TursoCatalog;
use sha2::{Digest, Sha256};
use ui::{MusicManagementEvent, ProviderChoice};
use uuid::Uuid;

use super::catalog_projection::project_dashboard;
use super::integration::{IntegrationFuture, IntegrationSliceHandler};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

const CONFIG_PATH: &str = "path";
const META_RELATIVE_PATH: &str = "relative_path";
const META_SHA256: &str = "sha256";
const META_SIZE: &str = "size_bytes";

pub type SelectionFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<LocalSourceSelection>, LocalLibraryError>> + Send + 'a>,
>;

/// Native folder selection is injected because desktop dialogs are platform composition work.
pub trait LocalSelectionProvider: Send + Sync {
    fn select_source<'a>(&'a self) -> SelectionFuture<'a>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalSourceSelection {
    pub name: String,
    pub root: PathBuf,
    pub copy_policy: SourceCopyPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceScanResult {
    pub source_id: Uuid,
    pub run_id: Uuid,
    pub discovered: u64,
    pub created_tracks: u64,
    pub reused_tracks: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LocalLibraryError {
    #[error("native local-folder selection was cancelled")]
    SelectionCancelled,
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    #[error("local source was not found: {0}")]
    SourceNotFound(Uuid),
    #[error("configuration is not a local folder")]
    NotLocal,
    #[error("local root is unavailable: {0}")]
    InvalidRoot(String),
    #[error("catalog repository error: {0}")]
    Repository(#[from] RepositoryError),
    #[error("local storage configuration error: {0}")]
    LocalStorage(#[from] LocalStorageError),
    #[error("music storage operation failed: {0}")]
    MusicStorage(#[from] MusicStorageError),
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("task failed: {0}")]
    Join(String),
    #[error("invalid integration request: {0}")]
    UnsupportedEvent(String),
    #[error("{0}")]
    Storage(StorageError),
}

/// What `music_storage` reports while a source is registered (root
/// separation) or the dashboard is projected.
impl From<StorageError> for LocalLibraryError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Repository(error) => Self::Repository(error),
            StorageError::Provider(error) => Self::MusicStorage(error),
            other => Self::Storage(other),
        }
    }
}

impl LocalLibraryError {
    fn retryable(&self) -> bool {
        match self {
            Self::Repository(error) => error.is_retryable(),
            Self::MusicStorage(error) => error.is_retryable(),
            Self::Storage(error) => error.retryable(),
            Self::Io(_) | Self::Join(_) => true,
            _ => false,
        }
    }
}

pub struct LocalLibraryService {
    catalog: Arc<TursoCatalog>,
    selection: Arc<dyn LocalSelectionProvider>,
    storages: MusicStorages,
}

impl LocalLibraryService {
    pub fn new(
        catalog: Arc<TursoCatalog>,
        selection: Arc<dyn LocalSelectionProvider>,
        storages: MusicStorages,
    ) -> Self {
        Self {
            catalog,
            selection,
            storages,
        }
    }

    pub fn catalog(&self) -> &Arc<TursoCatalog> {
        &self.catalog
    }

    pub async fn add_selected_source(&self) -> Result<SourceScanResult, LocalLibraryError> {
        tracing::info!("requesting local source folder selection");
        let selected = self
            .selection
            .select_source()
            .await?
            .ok_or(LocalLibraryError::SelectionCancelled)?;
        tracing::info!(source_name = %selected.name, root = %selected.root.display(), "local source folder selected");
        self.register_local_source(selected).await
    }

    pub async fn register_local_source(
        &self,
        selection: LocalSourceSelection,
    ) -> Result<SourceScanResult, LocalLibraryError> {
        let root = canonical_directory(&selection.root)?;
        tracing::info!(source_name = %selection.name, root = %root.display(), "registering local source");
        self.storages
            .check_root_separation(&RootLocator::host_path(&root), RootPurpose::CatalogSource)
            .await?;
        let existing = catalog_io::sources(&self.catalog)
            .await?
            .into_iter()
            .find(|source| {
                source.kind == CatalogSourceKind::LocalFolder
                    && source.config.get(CONFIG_PATH).is_some_and(|path| {
                        canonical_directory(Path::new(path)).ok().as_ref() == Some(&root)
                    })
            });
        let mut source = existing.unwrap_or_else(|| {
            CatalogSource::local_folder(selection.name.clone(), root.to_string_lossy())
        });
        source.name = selection.name;
        source.copy_policy = selection.copy_policy;
        source
            .config
            .insert(CONFIG_PATH.into(), root.to_string_lossy().into_owned());
        SourceRepository::upsert(self.catalog.as_ref(), source.clone()).await?;
        tracing::debug!(source_id = %source.id, "local source registered; starting scan");
        self.scan_local_source(source.id).await
    }

    pub async fn scan_local_source(
        &self,
        source_id: Uuid,
    ) -> Result<SourceScanResult, LocalLibraryError> {
        let mut source = SourceRepository::get(self.catalog.as_ref(), source_id)
            .await?
            .ok_or(LocalLibraryError::SourceNotFound(source_id))?;
        if source.kind != CatalogSourceKind::LocalFolder {
            return Err(LocalLibraryError::NotLocal);
        }
        let root = source_root(&source)?;
        tracing::info!(source_id = %source_id, root = %root.display(), "local source scan started");
        let scanned = tokio::task::spawn_blocking(move || scan_files(&root))
            .await
            .map_err(|error| LocalLibraryError::Join(error.to_string()))??;
        tracing::info!(source_id = %source_id, discovered = scanned.len(), "local source filesystem scan completed");

        let existing_occurrences =
            catalog_io::occurrences(&self.catalog, Some(source_id), None).await?;
        let existing_by_external: HashMap<_, _> = existing_occurrences
            .iter()
            .map(|value| (value.identity.external_id.clone(), value.clone()))
            .collect();
        let mut existing_by_hash = HashMap::new();
        for occurrence in &existing_occurrences {
            if let Some(hash) = metadata_text(&occurrence.provider_metadata, META_SHA256) {
                existing_by_hash
                    .entry(hash.to_owned())
                    .or_insert_with(|| occurrence.clone());
            }
        }
        let legacy_by_path = self.legacy_track_paths().await?;

        let mut run = SyncRun::new(SyncRunKind::CatalogSync, Some(source_id), None);
        run.start().map_err(transition_error)?;
        SyncRunRepository::upsert(self.catalog.as_ref(), run.clone()).await?;

        let transaction = self.catalog.begin().await?;
        let mut occurrences = Vec::with_capacity(scanned.len());
        let mut created = 0_u64;
        let mut reused = 0_u64;
        for file in scanned {
            let prior = existing_by_external
                .get(&file.external_id)
                .or_else(|| existing_by_hash.get(file.hash.value()));
            let legacy_track_id = legacy_by_path.get(&file.absolute_path).copied();
            let track_id = prior
                .and_then(|value| value.canonical_track_id)
                .or(legacy_track_id);
            let mut track = match track_id {
                Some(id) => transaction
                    .tracks()
                    .get(id)
                    .await?
                    .unwrap_or_else(CanonicalTrack::new),
                None => CanonicalTrack::new(),
            };
            if track_id.is_some() {
                reused += 1;
            } else {
                created += 1;
            }

            let mut occurrence = SourceTrackOccurrence::new(source_id, file.external_id.clone());
            occurrence.canonical_track_id = Some(track.id);
            occurrence.availability = SourceAvailability::Obtainable;
            occurrence.last_seen_at = Utc::now();
            occurrence.provider_metadata.insert(
                META_RELATIVE_PATH.into(),
                MetadataValue::Text(file.relative_path.clone()),
            );
            occurrence.provider_metadata.insert(
                META_SHA256.into(),
                MetadataValue::Text(file.hash.value().to_owned()),
            );
            occurrence.provider_metadata.insert(
                META_SIZE.into(),
                MetadataValue::Integer(file.size_bytes as i64),
            );
            track.link_occurrence(occurrence.identity.clone());

            // The asset is settled first: tags are observations *of a file*, so they
            // can only be recorded once it is known which asset was read.
            let mut originals = transaction
                .assets()
                .list_originals_for_track(track.id)
                .await?;
            let matching_original = originals
                .iter()
                .position(|asset| asset.content_hash.as_ref() == Some(&file.hash))
                .map(|index| originals.swap_remove(index));
            let mut original = matching_original
                .or_else(|| originals.into_iter().next())
                .unwrap_or_else(|| {
                    OriginalAsset::new_unverified(
                        track.id,
                        OriginalAssetRole::Master,
                        file.absolute_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("audio"),
                    )
                });
            original.file_name = file
                .absolute_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("audio")
                .to_owned();
            original.size_bytes = Some(file.size_bytes);
            original.content_hash = Some(file.hash.clone());

            // A file that moved is the same file under a new name; the title its old
            // name gave it must go with the old name, not outrank the real one.
            let superseded = prior
                .map(|value| value.identity.external_id.as_str())
                .filter(|external_id| *external_id != file.external_id);
            record_observed_metadata(
                &mut track,
                &file,
                source_id,
                original.id(),
                superseded,
                Utc::now(),
            );
            transaction.tracks().upsert(track.clone()).await?;
            transaction.assets().upsert_original(original).await?;
            occurrences.push(occurrence);
        }
        transaction
            .sources()
            .upsert_occurrences(occurrences)
            .await?;
        source.last_synced_at = Some(Utc::now());
        transaction.sources().upsert(source).await?;
        run.progress.discover(scanned_len_u64(created, reused));
        for _ in 0..scanned_len_u64(created, reused) {
            run.progress.record_success();
        }
        run.succeed().map_err(transition_error)?;
        transaction.sync_runs().upsert(run.clone()).await?;
        transaction.commit().await?;

        let result = SourceScanResult {
            source_id,
            run_id: run.id,
            discovered: created + reused,
            created_tracks: created,
            reused_tracks: reused,
        };
        tracing::info!(source_id = %result.source_id, run_id = %result.run_id, discovered = result.discovered, created_tracks = result.created_tracks, reused_tracks = result.reused_tracks, "local source scan persisted");
        Ok(result)
    }

    async fn legacy_track_paths(&self) -> Result<HashMap<PathBuf, Uuid>, LocalLibraryError> {
        let originals = catalog_io::originals(&self.catalog).await?;
        let track_by_asset: HashMap<_, _> = originals
            .iter()
            .map(|asset| (asset.id(), asset.track_id()))
            .collect();
        let mut paths = HashMap::new();
        for location in catalog_io::locations(&self.catalog).await? {
            let path = PathBuf::from(&location.object_id);
            if path.is_absolute() {
                if let Some(track_id) = track_by_asset.get(&location.asset_id) {
                    paths.insert(path, *track_id);
                }
            }
        }
        Ok(paths)
    }
}

impl IntegrationSliceHandler for LocalLibraryService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::LocalLibrary
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            if request.slice != IntegrationSlice::LocalLibrary {
                return Err(IntegrationError::for_request(
                    &request,
                    "request was routed to the wrong integration slice",
                    false,
                ));
            }
            let operation: Result<String, LocalLibraryError> = async {
                match request.event.clone() {
                    MusicManagementEvent::AddSource(ProviderChoice::LocalFolder) => self
                        .add_selected_source()
                        .await
                        .map(|result| format!("Scanned {} local audio files", result.discovered)),
                    MusicManagementEvent::AddLocalFolderSource { path } => self
                        .register_local_source(local_source_at(&path))
                        .await
                        .map(|result| format!("Scanned {} local audio files", result.discovered)),
                    MusicManagementEvent::SyncSource(id) => {
                        let id = parse_uuid(&id)?;
                        self.scan_local_source(id).await.map(|result| {
                            format!("Scanned {} local audio files", result.discovered)
                        })
                    }
                    other => Err(LocalLibraryError::UnsupportedEvent(format!("{other:?}"))),
                }
            }
            .await;
            match operation {
                Ok(message) => project_dashboard(&self.catalog, &self.storages)
                    .await
                    .map_err(LocalLibraryError::from)
                    .map(|view| IntegrationOutcome {
                        view: Some(view),
                        message: Some(message),
                    })
                    .map_err(|error| {
                        IntegrationError::for_request(
                            &request,
                            error.to_string(),
                            error.retryable(),
                        )
                    }),
                Err(error) => Err(IntegrationError::for_request(
                    &request,
                    error.to_string(),
                    error.retryable(),
                )),
            }
        })
    }
}

#[derive(Debug)]
struct ScannedFile {
    absolute_path: PathBuf,
    relative_path: String,
    external_id: String,
    size_bytes: u64,
    hash: ContentHash,
    /// What the file's own tags say about it, read while walking the folder.
    tags: EmbeddedTags,
}

fn scan_files(root: &Path) -> Result<Vec<ScannedFile>, LocalLibraryError> {
    let mut files = Vec::new();
    scan_directory(root, root, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    files: &mut Vec<ScannedFile>,
) -> Result<(), LocalLibraryError> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if entry.file_name() != ".rec_bag_history" {
                scan_directory(root, &path, files)?;
            }
            continue;
        }
        if !metadata.is_file() || !is_recognized_audio_path(&path) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| LocalLibraryError::InvalidRoot(path.display().to_string()))?;
        let relative_path = path_to_object_id(relative)?;
        let hash = hash_file(&path)?;
        // A file that will not give up its tags is still a file worth cataloguing.
        let tags = rec_core::read_embedded_tags(&path).unwrap_or_else(|error| {
            tracing::debug!(path = %path.display(), %error, "could not read embedded tags");
            EmbeddedTags::default()
        });
        files.push(ScannedFile {
            absolute_path: path,
            external_id: filesystem_identity(&metadata)
                .unwrap_or_else(|| format!("local-v1:path:{relative_path}")),
            relative_path,
            size_bytes: metadata.len(),
            hash,
            tags,
        });
    }
    Ok(())
}

#[cfg(unix)]
fn filesystem_identity(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!(
        "local-v1:file:{}:{}",
        metadata.dev(),
        metadata.ino()
    ))
}

#[cfg(not(unix))]
fn filesystem_identity(_metadata: &fs::Metadata) -> Option<String> {
    None
}

fn hash_file(path: &Path) -> Result<ContentHash, LocalLibraryError> {
    let bytes = fs::read(path)?;
    let digest = Sha256::digest(bytes);
    ContentHash::new(HashAlgorithm::Sha256, format!("{digest:x}"))
        .map_err(|error| LocalLibraryError::InvalidRoot(error.to_string()))
}

/// A source configuration for a folder the user named directly. The folder's own name
/// is the source's name, which is what the native picker would also produce.
fn local_source_at(path: &str) -> LocalSourceSelection {
    let root = PathBuf::from(path);
    LocalSourceSelection {
        name: root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_owned()),
        root,
        copy_policy: SourceCopyPolicy::default(),
    }
}

pub(crate) fn canonical_directory(path: &Path) -> Result<PathBuf, LocalLibraryError> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(LocalLibraryError::InvalidRoot(path.display().to_string()));
    }
    Ok(path)
}

fn source_root(source: &CatalogSource) -> Result<PathBuf, LocalLibraryError> {
    if source.kind != CatalogSourceKind::LocalFolder {
        return Err(LocalLibraryError::NotLocal);
    }
    source
        .config
        .get(CONFIG_PATH)
        .map(PathBuf::from)
        .ok_or_else(|| LocalLibraryError::InvalidRoot("local source has no path".into()))
}

fn path_to_object_id(path: &Path) -> Result<String, LocalLibraryError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => components.push(value.to_string_lossy()),
            _ => return Err(LocalLibraryError::InvalidRoot(path.display().to_string())),
        }
    }
    if components.is_empty() {
        return Err(LocalLibraryError::InvalidRoot(path.display().to_string()));
    }
    Ok(components.join("/"))
}

fn metadata_text<'a>(metadata: &'a BTreeMap<String, MetadataValue>, key: &str) -> Option<&'a str> {
    match metadata.get(key) {
        Some(MetadataValue::Text(value)) => Some(value),
        _ => None,
    }
}

/// Record what one scanned file says about its canonical track.
///
/// Every field the file carries is recorded, because a track whose length the catalog
/// does not know cannot be seeked and a track whose artist it does not know cannot be
/// found. Tags describe one asset, so they are recorded against that asset and restated
/// on each scan: rescanning a folder updates what the file says instead of accumulating
/// another copy of it, and a tag that has since been removed is withdrawn.
///
/// The filename is only a fallback title. It is recorded as the source's own
/// observation when the file carries no title of its own, and withdrawn as soon as it
/// does, so a real title is never shadowed by a path — including the name the file went
/// by before it moved.
fn record_observed_metadata(
    track: &mut CanonicalTrack,
    file: &ScannedFile,
    source_id: Uuid,
    asset_id: Uuid,
    superseded_external_id: Option<&str>,
    observed_at: DateTime<Utc>,
) {
    let tags = &file.tags;
    let embedded = MetadataProvenance::embedded_tag(asset_id, None, observed_at);
    let text = |value: &Option<String>| value.clone().map(MetadataValue::Text);
    let count = |value: Option<u32>| value.map(|value| MetadataValue::Integer(value as i64));

    for (field, value) in [
        (MetadataField::Title, text(&tags.title)),
        (MetadataField::Artist, text(&tags.artist)),
        (MetadataField::Album, text(&tags.album)),
        (MetadataField::AlbumArtist, text(&tags.album_artist)),
        (MetadataField::Genre, text(&tags.genre)),
        (MetadataField::MusicalKey, text(&tags.musical_key)),
        (MetadataField::Isrc, text(&tags.isrc)),
        (MetadataField::ReleaseYear, count(tags.year)),
        (MetadataField::TrackNumber, count(tags.track_number)),
        (MetadataField::DiscNumber, count(tags.disc_number)),
        (
            MetadataField::DurationMs,
            tags.duration.and_then(|duration| {
                i64::try_from(duration.as_millis())
                    .ok()
                    .map(MetadataValue::Integer)
            }),
        ),
        (
            MetadataField::Bpm,
            tags.bpm.map(|bpm| MetadataValue::Decimal(bpm.to_string())),
        ),
    ] {
        track
            .metadata
            .set_observation(field, value, embedded.clone());
    }

    let filename_title = tags.title.is_none().then(|| {
        MetadataValue::Text(
            file.absolute_path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Unknown track")
                .to_owned(),
        )
    });
    track.metadata.set_observation(
        MetadataField::Title,
        filename_title,
        MetadataProvenance::provider(source_id, &file.external_id, observed_at),
    );
    if let Some(external_id) = superseded_external_id {
        track.metadata.set_observation(
            MetadataField::Title,
            None,
            MetadataProvenance::provider(source_id, external_id, observed_at),
        );
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, LocalLibraryError> {
    Uuid::parse_str(value).map_err(|_| LocalLibraryError::InvalidIdentifier(value.into()))
}

fn transition_error(error: impl std::fmt::Display) -> LocalLibraryError {
    LocalLibraryError::InvalidRoot(error.to_string())
}

fn scanned_len_u64(created: u64, reused: u64) -> u64 {
    created + reused
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    struct TestSelector {
        selection: Mutex<Option<LocalSourceSelection>>,
    }

    impl LocalSelectionProvider for TestSelector {
        fn select_source<'a>(&'a self) -> SelectionFuture<'a> {
            Box::pin(async move { Ok(self.selection.lock().unwrap().take()) })
        }
    }

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("rec-bag-local-library-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn service(selection: Option<LocalSourceSelection>) -> LocalLibraryService {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        service_on(catalog, selection)
    }

    fn service_on(
        catalog: Arc<TursoCatalog>,
        selection: Option<LocalSourceSelection>,
    ) -> LocalLibraryService {
        let selection: Arc<dyn LocalSelectionProvider> = Arc::new(TestSelector {
            selection: Mutex::new(selection),
        });
        let storages = crate::services::storage_drivers::local::local_only_storages(
            &catalog,
            selection.clone(),
        );
        LocalLibraryService::new(catalog, selection, storages)
    }

    #[test]
    fn scan_skips_symlinks_history_and_non_audio() {
        let root = TempDirectory::new("scan");
        fs::write(root.path().join("one.mp3"), b"one").unwrap();
        fs::write(root.path().join("notes.txt"), b"text").unwrap();
        fs::create_dir(root.path().join(".rec_bag_history")).unwrap();
        fs::write(root.path().join(".rec_bag_history/old.mp3"), b"old").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("one.mp3"), root.path().join("link.mp3"))
            .unwrap();
        let files = scan_files(root.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, "one.mp3");
        assert!(files[0].hash.is_canonical_sha256());
    }

    /// Write a real, tagged audio file of a known length.
    ///
    /// A placeholder byte string would test nothing here: the point is what a scan reads
    /// out of a file that genuinely carries tags and audio properties.
    fn write_tagged_audio(path: &Path, length: Duration, tag: impl FnOnce(&mut rec_core::Track)) {
        const SAMPLE_RATE: u32 = 44_100;
        let samples = (length.as_secs_f64() * SAMPLE_RATE as f64).round() as u32;
        let data_len = samples * 2;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        fs::write(path, wav).unwrap();

        let mut track = rec_core::Track::new_without_metadata(path.to_path_buf());
        tag(&mut track);
        track.save_metadata().unwrap();
    }

    fn preferred_value(track: &CanonicalTrack, field: MetadataField) -> Option<MetadataValue> {
        track
            .metadata
            .preferred(&field)
            .map(|candidate| candidate.value.clone())
    }

    /// A track whose length the catalog does not know cannot be seeked on a deck, so
    /// what a file says about itself has to survive the scan.
    #[tokio::test]
    async fn a_scan_records_what_the_files_own_tags_say() {
        let root = TempDirectory::new("tags");
        write_tagged_audio(
            &root.path().join("filename.wav"),
            Duration::from_secs(3),
            |track| {
                track.title = "Tagged title".into();
                track.artist = Some("Tagged artist".into());
                track.album = Some("Tagged album".into());
                track.genre = Some("Tagged genre".into());
                track.year = Some(1999);
            },
        );
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();

        let track = catalog_io::tracks(&service.catalog)
            .await
            .unwrap()
            .remove(0);

        assert_eq!(
            preferred_value(&track, MetadataField::DurationMs),
            Some(MetadataValue::Integer(3_000)),
            "the deck's trackbar spans this value; without it seeking is impossible"
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Title),
            Some(MetadataValue::Text("Tagged title".into())),
            "the file's own title outranks its filename"
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Artist),
            Some(MetadataValue::Text("Tagged artist".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Album),
            Some(MetadataValue::Text("Tagged album".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::Genre),
            Some(MetadataValue::Text("Tagged genre".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::ReleaseYear),
            Some(MetadataValue::Integer(1999))
        );
    }

    /// A file with nothing to say still gets a name, and still gets a length.
    #[tokio::test]
    async fn an_untagged_file_falls_back_to_its_filename_but_keeps_its_length() {
        let root = TempDirectory::new("untagged");
        write_tagged_audio(
            &root.path().join("Some Artist - Some Title.wav"),
            Duration::from_secs(1),
            |_| {},
        );
        let service = service(None).await;
        service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();

        let track = catalog_io::tracks(&service.catalog)
            .await
            .unwrap()
            .remove(0);

        assert_eq!(
            preferred_value(&track, MetadataField::Title),
            Some(MetadataValue::Text("Some Artist - Some Title".into()))
        );
        assert_eq!(
            preferred_value(&track, MetadataField::DurationMs),
            Some(MetadataValue::Integer(1_000))
        );
    }

    /// Rescanning restates what a file says; it does not stack another copy of it.
    #[tokio::test]
    async fn rescanning_restates_a_files_tags_rather_than_accumulating_them() {
        let root = TempDirectory::new("restate");
        let file = root.path().join("track.wav");
        write_tagged_audio(&file, Duration::from_secs(2), |track| {
            track.title = "First".into();
        });
        let service = service(None).await;
        let first = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();

        write_tagged_audio(&file, Duration::from_secs(2), |track| {
            track.title = "Second".into();
        });
        service.scan_local_source(first.source_id).await.unwrap();

        let track = catalog_io::tracks(&service.catalog)
            .await
            .unwrap()
            .remove(0);
        let titles: Vec<_> = track
            .metadata
            .values
            .iter()
            .filter(|candidate| candidate.field == MetadataField::Title)
            .collect();
        assert_eq!(titles.len(), 1, "one file says one thing about its title");
        assert_eq!(
            preferred_value(&track, MetadataField::Title),
            Some(MetadataValue::Text("Second".into()))
        );
    }

    #[tokio::test]
    async fn stable_scan_reuses_track_and_occurrence() {
        let root = TempDirectory::new("stable");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let first = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();
        let second = service.scan_local_source(first.source_id).await.unwrap();
        assert_eq!(first.created_tracks, 1);
        assert_eq!(second.created_tracks, 0);
        assert_eq!(second.reused_tracks, 1);
        assert_eq!(catalog_io::tracks(&service.catalog).await.unwrap().len(), 1);
        assert_eq!(
            catalog_io::occurrences(&service.catalog, Some(first.source_id), None)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn inode_identity_survives_a_move() {
        let root = TempDirectory::new("move");
        fs::write(root.path().join("before.mp3"), b"audio").unwrap();
        let service = service(None).await;
        let first = service
            .register_local_source(LocalSourceSelection {
                name: "Source".into(),
                root: root.path().into(),
                copy_policy: SourceCopyPolicy::AskBeforeCopying,
            })
            .await
            .unwrap();
        fs::rename(
            root.path().join("before.mp3"),
            root.path().join("after.mp3"),
        )
        .unwrap();
        let second = service.scan_local_source(first.source_id).await.unwrap();
        assert_eq!(second.created_tracks, 0);
        assert_eq!(catalog_io::tracks(&service.catalog).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn handler_uses_injected_selection_and_returns_replacement_view() {
        let root = TempDirectory::new("handler");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let service = service(Some(LocalSourceSelection {
            name: "Selected".into(),
            root: root.path().into(),
            copy_policy: SourceCopyPolicy::AskBeforeCopying,
        }))
        .await;
        let outcome = service
            .handle(IntegrationRequest {
                id: 7,
                slice: IntegrationSlice::LocalLibrary,
                event: MusicManagementEvent::AddSource(ProviderChoice::LocalFolder),
            })
            .await
            .unwrap();
        assert!(outcome.message.unwrap().contains("Scanned 1"));
        assert_eq!(outcome.view.unwrap().sources.len(), 1);
    }
}
