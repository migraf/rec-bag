//! The music storage slice: every storage event from the dashboard, for every
//! provider, carried out through [`MusicStorages`].
//!
//! The provider a user picks only chooses the storage kind to connect (see
//! [`storage_kind`]); everything after that is provider-neutral.

use std::sync::Arc;

use music_storage::{
    CopyAuthorization, MigrationOptions, MusicStorages, Release, RootLocator, StorageError,
    StorageSettings, StorageSetup, Target,
};
use rec_core::{MusicStorage, MusicStorageKind, MusicStorageRepository, StorageRole};
use rec_persistence::TursoCatalog;
use ui::{MusicManagementEvent, PrimaryConfigurationView, ProviderChoice};
use uuid::Uuid;

use super::catalog_projection::project_dashboard;
use super::integration::{IntegrationFuture, IntegrationSliceHandler};
use crate::state::integration::{
    IntegrationError, IntegrationOutcome, IntegrationRequest, IntegrationSlice,
};

#[derive(Debug, thiserror::Error)]
pub enum MusicStorageServiceError {
    /// The event cannot be carried out as asked; the text says why.
    #[error("{0}")]
    InvalidRequest(String),
    #[error("{0}")]
    Storage(#[from] StorageError),
}

impl MusicStorageServiceError {
    fn retryable(&self) -> bool {
        match self {
            Self::InvalidRequest(_) => false,
            Self::Storage(error) => error.retryable(),
        }
    }
}

impl From<rec_core::RepositoryError> for MusicStorageServiceError {
    fn from(error: rec_core::RepositoryError) -> Self {
        Self::Storage(error.into())
    }
}

/// The storage kind a provider choice from the dashboard connects.
fn storage_kind(provider: ProviderChoice) -> Result<MusicStorageKind, MusicStorageServiceError> {
    match provider {
        ProviderChoice::LocalFolder => Ok(MusicStorageKind::LocalDirectory),
        ProviderChoice::GoogleDrive => Ok(MusicStorageKind::GoogleDrive),
        ProviderChoice::SoundCloud => Err(MusicStorageServiceError::InvalidRequest(
            "SoundCloud cannot be used as music storage".into(),
        )),
    }
}

/// The name a new backup gets. A local backup is named after the folder
/// picked for it; a Drive backup's name also names the folder created for it.
fn backup_name(provider: ProviderChoice) -> Option<String> {
    match provider {
        ProviderChoice::GoogleDrive => Some("Google Drive backup".into()),
        ProviderChoice::LocalFolder | ProviderChoice::SoundCloud => None,
    }
}

/// Who issued a provider grant, as the user knows them.
fn grant_issuer(provider: ProviderChoice) -> &'static str {
    match provider {
        ProviderChoice::GoogleDrive => "Google",
        other => other.label(),
    }
}

/// What a primary configuration asks a driver for. An empty location lets the
/// driver ask (a folder picker, a Drive folder of its own).
fn setup(configuration: &PrimaryConfigurationView) -> StorageSetup {
    StorageSetup {
        name: Some(configuration.name.trim().to_owned()),
        location: (!configuration.location.trim().is_empty())
            .then(|| configuration.location.clone()),
        organize_by_artist: configuration.organize_by_artist,
        verify_after_copy: configuration.verify_after_copy,
    }
}

fn parse_id(what: &str, value: &str) -> Result<Uuid, MusicStorageServiceError> {
    Uuid::parse_str(value).map_err(|error| {
        MusicStorageServiceError::InvalidRequest(format!("invalid {what} ID: {error}"))
    })
}

pub struct MusicStorageService {
    catalog: Arc<TursoCatalog>,
    storages: MusicStorages,
}

impl MusicStorageService {
    pub fn new(catalog: Arc<TursoCatalog>, storages: MusicStorages) -> Self {
        Self { catalog, storages }
    }

    #[cfg(test)]
    pub(crate) fn storages(&self) -> &MusicStorages {
        &self.storages
    }

    /// Connects the first primary; it becomes the active primary once
    /// connecting has inventoried it.
    pub async fn create_primary(
        &self,
        configuration: &PrimaryConfigurationView,
    ) -> Result<MusicStorage, MusicStorageServiceError> {
        self.connect(configuration, StorageRole::Primary).await
    }

    /// Connects a primary candidate for migration. It stays `Unassigned`
    /// until [`MusicStorages::migrate_primary`] makes it the active primary;
    /// connecting the same location again reconnects it.
    pub async fn connect_candidate(
        &self,
        configuration: &PrimaryConfigurationView,
    ) -> Result<MusicStorage, MusicStorageServiceError> {
        self.connect(configuration, StorageRole::Unassigned).await
    }

    /// Connects a backup the driver lets the user choose; connecting
    /// inventories it.
    pub async fn add_backup(
        &self,
        provider: ProviderChoice,
    ) -> Result<MusicStorage, MusicStorageServiceError> {
        let setup = StorageSetup {
            name: backup_name(provider),
            ..StorageSetup::default()
        };
        Ok(self
            .storages
            .connect(storage_kind(provider)?, StorageRole::Backup, &setup)
            .await?)
    }

    async fn connect(
        &self,
        configuration: &PrimaryConfigurationView,
        role: StorageRole,
    ) -> Result<MusicStorage, MusicStorageServiceError> {
        let kind = storage_kind(configuration.provider)?;
        tracing::info!(storage_name = %configuration.name, location = %configuration.location, ?kind, ?role, "connecting music storage");
        let storage = self
            .storages
            .connect(kind, role, &setup(configuration))
            .await?;
        tracing::info!(storage_id = %storage.id, root = ?storage.root_identifier, role = ?storage.role, "music storage connected");
        Ok(storage)
    }

    /// Carries out one storage event and says what happened.
    async fn apply(
        &self,
        event: &MusicManagementEvent,
    ) -> Result<String, MusicStorageServiceError> {
        Ok(match event {
            MusicManagementEvent::CreatePrimary(configuration) => {
                let kind = storage_kind(configuration.provider)?;
                match MusicStorageRepository::get_active_primary(self.catalog.as_ref()).await? {
                    // Creating the primary that already exists is a no-op.
                    Some(active)
                        if active.kind == kind
                            && self.storages.root_of(&active)
                                == Some(RootLocator::host_path(&configuration.location)) =>
                    {
                        format!("Primary {} is already configured", active.id)
                    }
                    Some(active) => {
                        return Err(StorageError::PrimaryExists { id: active.id }.into())
                    }
                    None => {
                        let primary = self.create_primary(configuration).await?;
                        format!("Primary {} configured and inventoried", primary.name)
                    }
                }
            }
            MusicManagementEvent::AddBackup(provider) => {
                let backup = self.add_backup(*provider).await?;
                format!("Backup {} configured and inventoried", backup.name)
            }
            MusicManagementEvent::UpdatePrimaryMetadata(update) => {
                self.storages
                    .update_settings(
                        parse_id("storage", &update.storage_id)?,
                        StorageSettings {
                            name: update.name.clone(),
                            organize_by_artist: update.organize_by_artist,
                            verify_after_copy: update.verify_after_copy,
                        },
                    )
                    .await?;
                "Updated primary metadata".into()
            }
            MusicManagementEvent::CopyToPrimary(track_id) => {
                let copied = self
                    .storages
                    .copy_to_primary(
                        parse_id("track", track_id)?,
                        CopyAuthorization::UserConfirmed,
                    )
                    .await?;
                format!(
                    "Copied {} to the active primary. {}",
                    copied.object_id,
                    copied.handoff.message()
                )
            }
            MusicManagementEvent::ToggleOfflinePin(track_id, pinned) => {
                self.storages
                    .set_offline_pinned(parse_id("track", track_id)?, *pinned)
                    .await?;
                if *pinned {
                    "Track pinned for offline playback".into()
                } else {
                    "Offline pin removed".into()
                }
            }
            MusicManagementEvent::RetryStorage(storage_id) => {
                let refreshed = self
                    .storages
                    .refresh(Target::Storage(parse_id("storage", storage_id)?))
                    .await?;
                format!("Inventoried {} storage objects", refreshed.object_count)
            }
            // The migration plan dialog is the final confirmation boundary;
            // the destination is connected only once a primary exists to
            // migrate from, so no authorisation prompt opens for nothing.
            MusicManagementEvent::PreparePrimaryMigration(plan) => {
                storage_kind(plan.destination.provider)?;
                if MusicStorageRepository::get_active_primary(self.catalog.as_ref())
                    .await?
                    .is_none()
                {
                    return Err(StorageError::NoPrimary.into());
                }
                let destination = self.connect_candidate(&plan.destination).await?;
                let migrated = self
                    .storages
                    .migrate_primary(
                        destination.id,
                        MigrationOptions {
                            keep_old_as_backup: plan.keep_old_as_backup,
                            verify_checksums: plan.verify_checksums,
                        },
                    )
                    .await?;
                format!(
                    "Migrated primary to {}. {}",
                    destination.name,
                    migrated.handoff.message()
                )
            }
            MusicManagementEvent::DisconnectStorage { storage_id } => {
                self.storages
                    .release(parse_id("storage", storage_id)?, Release::Disconnect)
                    .await?;
                "Music storage disconnected; catalog data was retained".into()
            }
            MusicManagementEvent::RevokeProviderGrant {
                provider,
                connection_id,
            } => {
                self.storages
                    .release(parse_id("storage", connection_id)?, Release::RevokeGrant)
                    .await?;
                format!(
                    "{} access revoked and matching storages disabled",
                    grant_issuer(*provider)
                )
            }
            MusicManagementEvent::ClearLocalCache { source_id } => {
                self.storages
                    .release(parse_id("storage", source_id)?, Release::ClearCache)
                    .await?;
                "Offline cache cleared; catalog data was retained".into()
            }
            MusicManagementEvent::ForgetProviderCatalogData { source_id } => {
                self.storages
                    .release(parse_id("storage", source_id)?, Release::Forget)
                    .await?;
                "Storage catalog data and offline cache deleted; stored files and authorization were retained".into()
            }
            MusicManagementEvent::AddSource(ProviderChoice::GoogleDrive) => {
                return Err(MusicStorageServiceError::InvalidRequest(
                    "Google Drive is music storage, not a catalog source".into(),
                ))
            }
            _ => {
                return Err(MusicStorageServiceError::InvalidRequest(
                    "event is not owned by the music storage slice".into(),
                ))
            }
        })
    }
}

impl IntegrationSliceHandler for MusicStorageService {
    fn slice(&self) -> IntegrationSlice {
        IntegrationSlice::MusicStorage
    }

    fn handle<'a>(&'a self, request: IntegrationRequest) -> IntegrationFuture<'a> {
        Box::pin(async move {
            let result: Result<IntegrationOutcome, MusicStorageServiceError> = async {
                let message = self.apply(&request.event).await?;
                Ok(IntegrationOutcome {
                    view: Some(project_dashboard(&self.catalog, &self.storages).await?),
                    message: Some(message),
                })
            }
            .await;
            result.map_err(|error| {
                IntegrationError::for_request(&request, error.to_string(), error.retryable())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use rec_core::{AssetRepository, SourceCopyPolicy, StorageVerificationStatus};

    use crate::services::local_library::{
        LocalLibraryService, LocalSelectionProvider, LocalSourceSelection, SelectionFuture,
    };
    use crate::services::storage_drivers::local::local_only_storages;

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
                .join(format!("rec-bag-music-storage-{label}-{}", Uuid::new_v4()));
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

    /// The local library and the storage slice over one catalog and one set
    /// of storages; the folder picker answers once with `picked`.
    struct Fixture {
        catalog: Arc<TursoCatalog>,
        local: LocalLibraryService,
        storage: MusicStorageService,
    }

    async fn fixture(picked: Option<&Path>) -> Fixture {
        let catalog = Arc::new(TursoCatalog::in_memory().await.unwrap());
        fixture_on(catalog, picked)
    }

    fn fixture_on(catalog: Arc<TursoCatalog>, picked: Option<&Path>) -> Fixture {
        let selection: Arc<dyn LocalSelectionProvider> = Arc::new(TestSelector {
            selection: Mutex::new(picked.map(|root| LocalSourceSelection {
                name: "Picked folder".into(),
                root: root.into(),
                copy_policy: SourceCopyPolicy::CheckOnly,
            })),
        });
        let storages = local_only_storages(&catalog, selection.clone());
        Fixture {
            local: LocalLibraryService::new(catalog.clone(), selection, storages.clone()),
            storage: MusicStorageService::new(catalog.clone(), storages),
            catalog,
        }
    }

    impl Fixture {
        async fn add_source(&self, root: &Path, copy_policy: SourceCopyPolicy) -> Uuid {
            self.local
                .register_local_source(LocalSourceSelection {
                    name: "Source".into(),
                    root: root.into(),
                    copy_policy,
                })
                .await
                .unwrap()
                .source_id
        }

        async fn handle(
            &self,
            id: u64,
            event: MusicManagementEvent,
        ) -> Result<IntegrationOutcome, IntegrationError> {
            self.storage
                .handle(IntegrationRequest {
                    id,
                    slice: IntegrationSlice::MusicStorage,
                    event,
                })
                .await
        }

        async fn active_primary(&self) -> Option<MusicStorage> {
            MusicStorageRepository::get_active_primary(self.catalog.as_ref())
                .await
                .unwrap()
        }

        async fn first_track(&self) -> Uuid {
            music_storage::catalog_io::tracks(&self.catalog)
                .await
                .unwrap()[0]
                .id
        }
    }

    fn primary_view(path: &Path) -> PrimaryConfigurationView {
        PrimaryConfigurationView {
            provider: ProviderChoice::LocalFolder,
            name: "Primary".into(),
            location: path.to_string_lossy().into_owned(),
            organize_by_artist: true,
            verify_after_copy: true,
        }
    }

    #[tokio::test]
    async fn handler_claims_the_music_storage_slice_and_rejects_non_storage_requests() {
        let fixture = fixture(None).await;
        assert_eq!(fixture.storage.slice(), IntegrationSlice::MusicStorage);

        let error = fixture
            .handle(
                1,
                MusicManagementEvent::AddSource(ProviderChoice::GoogleDrive),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "Google Drive is music storage, not a catalog source"
        );
        assert_eq!(error.slice, Some(IntegrationSlice::MusicStorage));
        assert_eq!(error.request_id, Some(1));

        let mut soundcloud = primary_view(Path::new("/unused"));
        soundcloud.provider = ProviderChoice::SoundCloud;
        let error = fixture
            .handle(2, MusicManagementEvent::CreatePrimary(soundcloud))
            .await
            .unwrap_err();
        assert_eq!(error.message, "SoundCloud cannot be used as music storage");
        assert!(!error.retryable);
    }

    #[tokio::test]
    async fn create_primary_is_a_no_op_for_the_active_root_and_refused_elsewhere() {
        let primary_root = TempDirectory::new("create-primary");
        let other_root = TempDirectory::new("create-other");
        let fixture = fixture(None).await;

        let outcome = fixture
            .handle(
                1,
                MusicManagementEvent::CreatePrimary(primary_view(primary_root.path())),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.message.as_deref(),
            Some("Primary Primary configured and inventoried")
        );
        let primary = fixture.active_primary().await.unwrap();
        assert_eq!(
            outcome.view.unwrap().primary_storage.unwrap().id,
            primary.id.to_string()
        );

        let again = fixture
            .handle(
                2,
                MusicManagementEvent::CreatePrimary(primary_view(primary_root.path())),
            )
            .await
            .unwrap();
        assert_eq!(
            again.message,
            Some(format!("Primary {} is already configured", primary.id))
        );
        let error = fixture
            .handle(
                3,
                MusicManagementEvent::CreatePrimary(primary_view(other_root.path())),
            )
            .await
            .unwrap_err();
        assert!(
            error.message.contains("use primary migration"),
            "{}",
            error.message
        );
        assert_eq!(fs::read_dir(other_root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connecting_a_read_only_local_root_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let root = TempDirectory::new("read-only-primary");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let fixture = fixture(None).await;

        let error = fixture
            .storage
            .connect_candidate(&primary_view(root.path()))
            .await
            .unwrap_err();

        assert!(
            matches!(&error, MusicStorageServiceError::Storage(StorageError::Unhealthy { message, .. }) if message == "local storage is read-only"),
            "{error}"
        );
        assert!(MusicStorageRepository::list(fixture.catalog.as_ref())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn in_place_adoption_verifies_without_copying() {
        let root = TempDirectory::new("in-place");
        fs::write(root.path().join("track.mp3"), b"audio").unwrap();
        let fixture = fixture(None).await;
        let source_id = fixture
            .add_source(
                root.path(),
                SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
            )
            .await;
        // What startup adoption does: rescan, then connect the primary at
        // the source's own root.
        fixture.local.scan_local_source(source_id).await.unwrap();
        let primary = fixture
            .storage
            .create_primary(&primary_view(root.path()))
            .await
            .unwrap();
        assert_eq!(fixture.active_primary().await.unwrap().id, primary.id);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        let playable = fixture
            .storage
            .storages()
            .playable(fixture.first_track().await)
            .await
            .unwrap();
        assert_eq!(playable.origin, music_storage::PlayableOrigin::Direct);
        assert_eq!(playable.path, root.path().join("track.mp3"));
    }

    #[tokio::test]
    async fn local_to_local_copy_uses_managed_path_and_verified_location() {
        let source_root = TempDirectory::new("copy-source");
        let primary_root = TempDirectory::new("copy-primary");
        fs::write(source_root.path().join("track.mp3"), b"audio").unwrap();
        let fixture = fixture(None).await;
        let source_id = fixture
            .add_source(
                source_root.path(),
                SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
            )
            .await;
        let primary = fixture
            .storage
            .create_primary(&primary_view(primary_root.path()))
            .await
            .unwrap();

        let copied = fixture
            .storage
            .storages()
            .copy_to_primary(fixture.first_track().await, CopyAuthorization::Automatic)
            .await
            .unwrap();

        assert_eq!(
            copied.origin,
            music_storage::CopyOrigin::CatalogSource(source_id)
        );
        assert!(copied.object_id.starts_with("Unknown Artist/"));
        assert!(primary_root.path().join(&copied.object_id).is_file());
        assert!(source_root.path().join("track.mp3").is_file());
        let location = fixture
            .catalog
            .get_location(primary.id, copied.object_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            location.verification_status,
            StorageVerificationStatus::Verified
        );
    }

    #[tokio::test]
    async fn copy_to_primary_event_uses_user_confirmation_and_refreshes_view() {
        let source_root = TempDirectory::new("event-copy-source");
        let primary_root = TempDirectory::new("event-copy-primary");
        fs::write(source_root.path().join("track.mp3"), b"audio").unwrap();
        let fixture = fixture(None).await;
        fixture
            .add_source(source_root.path(), SourceCopyPolicy::AskBeforeCopying)
            .await;
        let primary = fixture
            .storage
            .create_primary(&primary_view(primary_root.path()))
            .await
            .unwrap();
        let track_id = fixture.first_track().await;

        let outcome = fixture
            .handle(
                20,
                MusicManagementEvent::CopyToPrimary(track_id.to_string()),
            )
            .await
            .unwrap();

        assert!(outcome.message.unwrap().starts_with("Copied "));
        let view = outcome.view.unwrap();
        assert_eq!(view.primary_storage.unwrap().id, primary.id.to_string());
        assert_eq!(view.completeness.verified_assets, 1);
        assert_eq!(
            music_storage::catalog_io::storage_objects(&fixture.catalog, primary.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn add_local_backup_event_configures_enabled_backup_and_rejects_overlap() {
        let primary_root = TempDirectory::new("event-backup-primary");
        let backup_root = TempDirectory::new("event-backup-target");
        let fixture = fixture(Some(backup_root.path())).await;
        fixture
            .storage
            .create_primary(&primary_view(primary_root.path()))
            .await
            .unwrap();

        let outcome = fixture
            .handle(
                22,
                MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder),
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.message.as_deref(),
            Some("Backup Picked folder configured and inventoried")
        );
        let view = outcome.view.unwrap();
        assert_eq!(view.backup_storages.len(), 1);
        let backup_id = Uuid::parse_str(&view.backup_storages[0].id).unwrap();
        let backup = MusicStorageRepository::get(fixture.catalog.as_ref(), backup_id)
            .await
            .unwrap()
            .unwrap();
        assert!(backup.enabled);
        assert_eq!(backup.role, StorageRole::Backup);

        let add_backup_at = |root: PathBuf, id: u64| {
            let unsafe_fixture = fixture_on(fixture.catalog.clone(), Some(&root));
            async move {
                unsafe_fixture
                    .handle(
                        id,
                        MusicManagementEvent::AddBackup(ProviderChoice::LocalFolder),
                    )
                    .await
                    .unwrap_err()
            }
        };
        // The primary's own folder is already a configured storage root.
        let error = add_backup_at(primary_root.path().into(), 23).await;
        assert!(error.message.contains("already configured"));
        let error = add_backup_at(primary_root.path().join("backup"), 24).await;
        assert!(error.message.contains("distinct and non-nested"));
        assert_eq!(
            MusicStorageRepository::list(fixture.catalog.as_ref())
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn local_source_must_stay_separate_from_backups_but_may_equal_primary() {
        let primary_root = TempDirectory::new("source-at-primary");
        let backup_root = TempDirectory::new("source-in-backup");
        let fixture = fixture(Some(backup_root.path())).await;
        fixture
            .storage
            .connect_candidate(&primary_view(primary_root.path()))
            .await
            .unwrap();
        fixture
            .storage
            .add_backup(ProviderChoice::LocalFolder)
            .await
            .unwrap();
        let inside_backup = backup_root.path().join("music");
        fs::create_dir(&inside_backup).unwrap();

        let error = fixture
            .local
            .register_local_source(LocalSourceSelection {
                name: "Inside backup".into(),
                root: inside_backup,
                copy_policy: SourceCopyPolicy::default(),
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("distinct and non-nested"));
        fixture
            .add_source(primary_root.path(), SourceCopyPolicy::default())
            .await;
    }

    #[tokio::test]
    async fn prepare_migration_refuses_overlapping_backup_and_retries_same_location() {
        let library = TempDirectory::new("migrate-library");
        let destination_root = TempDirectory::new("migrate-destination");
        fs::write(library.path().join("track.mp3"), b"audio").unwrap();
        let fixture = fixture(None).await;
        let source_id = fixture
            .add_source(library.path(), SourceCopyPolicy::AskBeforeCopying)
            .await;
        fixture.local.scan_local_source(source_id).await.unwrap();
        // Adopted in place, so it cannot become a backup of its own source.
        let old = fixture
            .storage
            .create_primary(&primary_view(library.path()))
            .await
            .unwrap();
        let migrate = |keep_old_as_backup| {
            MusicManagementEvent::PreparePrimaryMigration(ui::PrimaryMigrationPlanView {
                destination: primary_view(destination_root.path()),
                verify_checksums: true,
                keep_old_as_backup,
            })
        };

        let error = fixture.handle(30, migrate(true)).await.unwrap_err();

        assert!(
            error.message.contains("distinct and non-nested"),
            "{}",
            error.message
        );
        assert_eq!(fixture.active_primary().await.unwrap().id, old.id);
        assert_eq!(fs::read_dir(destination_root.path()).unwrap().count(), 0);

        // Retrying with the same location reuses the candidate connected by
        // the refused attempt instead of failing as already configured.
        let outcome = fixture.handle(31, migrate(false)).await.unwrap();

        assert!(outcome.message.unwrap().starts_with("Migrated primary"));
        let storages = MusicStorageRepository::list(fixture.catalog.as_ref())
            .await
            .unwrap();
        assert_eq!(storages.len(), 2);
        assert_ne!(fixture.active_primary().await.unwrap().id, old.id);
        assert!(storages
            .iter()
            .any(|storage| storage.id == old.id && storage.role == StorageRole::Unassigned));
        assert!(library.path().join("track.mp3").is_file());
    }

    #[tokio::test]
    async fn prepare_migration_without_a_primary_connects_nothing() {
        let destination = TempDirectory::new("migrate-nothing");
        let fixture = fixture(None).await;

        let error = fixture
            .handle(
                32,
                MusicManagementEvent::PreparePrimaryMigration(ui::PrimaryMigrationPlanView {
                    destination: primary_view(destination.path()),
                    verify_checksums: true,
                    keep_old_as_backup: true,
                }),
            )
            .await
            .unwrap_err();

        assert_eq!(error.message, StorageError::NoPrimary.to_string());
        assert!(MusicStorageRepository::list(fixture.catalog.as_ref())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn lifecycle_events_work_for_local_storages_but_never_release_the_primary() {
        let primary_root = TempDirectory::new("lifecycle-primary");
        let backup_root = TempDirectory::new("lifecycle-backup");
        let fixture = fixture(Some(backup_root.path())).await;
        let primary = fixture
            .storage
            .create_primary(&primary_view(primary_root.path()))
            .await
            .unwrap();
        let backup = fixture
            .storage
            .add_backup(ProviderChoice::LocalFolder)
            .await
            .unwrap();

        let error = fixture
            .handle(
                40,
                MusicManagementEvent::DisconnectStorage {
                    storage_id: primary.id.to_string(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("migrate to another primary first"));
        assert!(fixture
            .handle(
                41,
                MusicManagementEvent::ClearLocalCache {
                    source_id: primary.id.to_string(),
                },
            )
            .await
            .is_ok());

        let outcome = fixture
            .handle(
                42,
                MusicManagementEvent::DisconnectStorage {
                    storage_id: backup.id.to_string(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            outcome.message.as_deref(),
            Some("Music storage disconnected; catalog data was retained")
        );
        assert!(
            !MusicStorageRepository::get(fixture.catalog.as_ref(), backup.id)
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        let error = fixture
            .handle(
                43,
                MusicManagementEvent::RevokeProviderGrant {
                    provider: ProviderChoice::LocalFolder,
                    connection_id: backup.id.to_string(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            error.message.contains("no authorisation grant"),
            "{}",
            error.message
        );
        fixture
            .handle(
                44,
                MusicManagementEvent::ForgetProviderCatalogData {
                    source_id: backup.id.to_string(),
                },
            )
            .await
            .unwrap();
        assert!(
            MusicStorageRepository::get(fixture.catalog.as_ref(), backup.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(backup_root.path().is_dir(), "stored files are kept");
    }

    #[tokio::test]
    async fn primary_switch_uses_expected_active_storage() {
        let first_root = TempDirectory::new("cas-first");
        let second_root = TempDirectory::new("cas-second");
        let fixture = fixture(None).await;
        fixture
            .storage
            .create_primary(&primary_view(first_root.path()))
            .await
            .unwrap();
        let second = fixture
            .storage
            .connect_candidate(&primary_view(second_root.path()))
            .await
            .unwrap();
        let error = fixture
            .catalog
            .switch_active_primary(None, Some(second.id))
            .await
            .unwrap_err();
        assert_eq!(error.kind, rec_core::RepositoryErrorKind::Conflict);
    }
}
