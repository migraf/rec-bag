use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use rec_core::{
    AssetLocation, AssetRepository, AssetType, CanonicalMetadata, CanonicalTrack, CatalogSource,
    HealthStatus, InventoryGenerationKind, InventoryGenerationStatus, InventoryMode,
    MatchCandidateFilter, MatchConfidence, MetadataField, MetadataFieldValue, MetadataProvenance,
    MetadataValue, MusicStorage, MusicStorageError, MusicStorageErrorKind, MusicStorageKind,
    MusicStorageRepository, OriginalAsset, OriginalAssetRole, ReconciliationRepository,
    RepositoryPageRequest, SettingsRepository, SourceAvailability, SourceCopyPolicy,
    SourceRepository, SourceTrackOccurrence, StorageCapabilities, StorageObject, StorageRole,
    StorageVerificationStatus, SyncErrorKind, SyncRunFilter, SyncRunKind, SyncRunRepository,
    SyncStatus, TrackRepository,
};
use rec_persistence::TursoCatalog;
use uuid::Uuid;

use crate::catalog_io;
use crate::copy::transfer_key;
use crate::offline_cache::{CacheKey, OfflineCache};
use crate::testing::{
    sha256, temp_catalog, writable_capabilities, FakeDriver, HealthScript, MemoryStore,
};
use crate::{
    CopyAuthorization, CopyOrigin, HandoffReport, LiveStores, MigrationOptions, MusicStorages,
    MusicStoragesConfig, NoHandoff, PlayableOrigin, Release, ReplicationHandoff, RootLocator,
    RootPurpose, StorageDriver, StorageDrivers, StorageError, StorageSettings, StorageSetup,
    Target,
};

fn config() -> MusicStoragesConfig {
    MusicStoragesConfig {
        cache_root: std::env::temp_dir().join("rec-bag-music-storage-cache"),
        staging_root: std::env::temp_dir().join("rec-bag-music-storage-staging"),
        default_cache_max_bytes: 1024,
    }
}

fn storages(catalog: &Arc<TursoCatalog>, drivers: Vec<Arc<dyn StorageDriver>>) -> MusicStorages {
    MusicStorages::new(
        catalog.clone(),
        StorageDrivers::new(drivers),
        config(),
        Arc::new(LiveStores::default()),
        Arc::new(NoHandoff),
    )
}

fn local_driver() -> Arc<FakeDriver> {
    Arc::new(FakeDriver::host(MusicStorageKind::LocalDirectory))
}

fn at(path: &Path) -> StorageSetup {
    StorageSetup {
        name: Some("Storage".into()),
        location: Some(path.to_string_lossy().into_owned()),
        ..StorageSetup::default()
    }
}

async fn add_source(catalog: &TursoCatalog, path: &Path) {
    SourceRepository::upsert(
        catalog,
        CatalogSource::local_folder("Source", path.to_string_lossy()),
    )
    .await
    .unwrap();
}

async fn persisted(catalog: &TursoCatalog) -> Vec<MusicStorage> {
    MusicStorageRepository::list(catalog).await.unwrap()
}

#[tokio::test]
async fn connect_persists_only_after_healthy_probe_with_probed_capabilities() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let root = tempfile::tempdir().unwrap();

    let storage = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &StorageSetup {
                organize_by_artist: true,
                ..at(root.path())
            },
        )
        .await
        .unwrap();

    let store = driver
        .store(storage.id)
        .expect("store opened for the new id");
    assert_eq!(store.health_calls(), 1);
    assert_eq!(persisted(&catalog).await, vec![storage.clone()]);
    assert_eq!(storage.role, StorageRole::Backup);
    assert_eq!(storage.capabilities, writable_capabilities());
    assert_eq!(
        storage.config.get("organize_by_artist").map(String::as_str),
        Some("true")
    );
    assert_eq!(driver.calls().opened, vec![storage.id]);
    assert!(storages.live_stores().contains(storage.id));
}

#[tokio::test]
async fn unhealthy_connect_persists_nothing_and_releases_unshared_grant() {
    let catalog = temp_catalog().await;
    let driver = Arc::new(
        FakeDriver::remote(MusicStorageKind::GoogleDrive, "account").with_grant("grant-1"),
    );
    driver.set_health(HealthScript {
        status: HealthStatus::Unavailable,
        message: Some("Drive storage root is not a folder".into()),
        capabilities: StorageCapabilities::default(),
        error: None,
    });
    let storages = storages(&catalog, vec![driver.clone()]);

    let error = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Backup,
            &StorageSetup::default(),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        StorageError::Unhealthy { status: HealthStatus::Unavailable, message }
            if message == "Drive storage root is not a folder"
    ));
    assert!(persisted(&catalog).await.is_empty());
    assert_eq!(driver.calls().released_grants, vec!["grant-1".to_owned()]);

    // A grant another storage still relies on is kept.
    let mut sharing = MusicStorage::new(
        "Existing",
        MusicStorageKind::GoogleDrive,
        StorageRole::Backup,
        writable_capabilities(),
    )
    .with_root_identifier("existing-folder");
    sharing.config.insert("grant".into(), "grant-1".into());
    MusicStorageRepository::upsert(catalog.as_ref(), sharing)
        .await
        .unwrap();
    storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Backup,
            &StorageSetup::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(driver.calls().released_grants.len(), 1);
    assert_eq!(persisted(&catalog).await.len(), 1);
}

#[tokio::test]
async fn duplicate_root_is_already_configured() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let root = tempfile::tempdir().unwrap();
    let first = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            &at(root.path()),
        )
        .await
        .unwrap();

    let error = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(root.path()),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, StorageError::AlreadyConfigured { id } if id == first.id));
    assert_eq!(persisted(&catalog).await.len(), 1);
}

#[tokio::test]
async fn primary_exists_is_rejected_before_provisioning() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let root = tempfile::tempdir().unwrap();
    let primary = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(root.path()),
        )
        .await
        .unwrap();
    let other = tempfile::tempdir().unwrap();

    let error = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(other.path()),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, StorageError::PrimaryExists { id } if id == primary.id));
    assert!(error
        .to_string()
        .contains("an active primary already exists; use primary migration"));
    assert_eq!(driver.calls().provisioned.len(), 1);
}

#[tokio::test]
async fn read_only_store_lacks_capability() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    driver.set_health(HealthScript {
        capabilities: StorageCapabilities {
            can_upload: false,
            ..writable_capabilities()
        },
        ..HealthScript::default()
    });
    let storages = storages(&catalog, vec![driver]);
    let root = tempfile::tempdir().unwrap();

    let error = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(root.path()),
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        StorageError::MissingCapability {
            role: StorageRole::Backup,
            missing: "store new objects"
        }
    ));
    assert!(persisted(&catalog).await.is_empty());
    assert!(!storages.live_stores().contains(Uuid::nil()));
}

#[tokio::test]
async fn nested_storages_rejected() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            &at(&nested),
        )
        .await
        .unwrap();

    let error = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(root.path()),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, StorageError::RootsOverlap { .. }));
    assert!(error.to_string().contains("distinct and non-nested"));
    assert_eq!(persisted(&catalog).await.len(), 1);
}

#[tokio::test]
async fn backup_overlapping_source_rejected() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let source = tempfile::tempdir().unwrap();
    add_source(&catalog, source.path()).await;

    let equal = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(source.path()),
        )
        .await
        .unwrap_err();
    let inside = source.path().join("backup");
    std::fs::create_dir(&inside).unwrap();
    let nested = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(&inside),
        )
        .await
        .unwrap_err();

    assert!(matches!(equal, StorageError::RootsOverlap { .. }));
    assert!(matches!(nested, StorageError::RootsOverlap { .. }));

    // The same rule applies when the source is the newcomer.
    let backup = tempfile::tempdir().unwrap();
    storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(backup.path()),
        )
        .await
        .unwrap();
    let error = storages
        .check_root_separation(
            &RootLocator::host_path(backup.path()),
            RootPurpose::CatalogSource,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("distinct and non-nested"));
}

#[tokio::test]
async fn primary_equal_to_source_allowed() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let library = tempfile::tempdir().unwrap();
    add_source(&catalog, library.path()).await;

    let primary = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(library.path()),
        )
        .await
        .unwrap();

    assert_eq!(primary.role, StorageRole::Primary);
    storages
        .check_root_separation(
            &RootLocator::host_path(library.path()),
            RootPurpose::CatalogSource,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn primary_nested_in_source_rejected() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let library = tempfile::tempdir().unwrap();
    add_source(&catalog, library.path()).await;
    let inside = library.path().join("primary");
    std::fs::create_dir(&inside).unwrap();

    let error = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(&inside),
        )
        .await
        .unwrap_err();

    assert!(matches!(error, StorageError::RootsOverlap { .. }));
    // A source around an existing primary candidate is rejected too.
    let outside = tempfile::tempdir().unwrap();
    let candidate = outside.path().join("candidate");
    std::fs::create_dir(&candidate).unwrap();
    storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            &at(&candidate),
        )
        .await
        .unwrap();
    assert!(storages
        .check_root_separation(
            &RootLocator::host_path(outside.path()),
            RootPurpose::CatalogSource
        )
        .await
        .is_err());
}

#[tokio::test]
async fn remote_namespaces_compare_equal_only() {
    let catalog = temp_catalog().await;
    let driver = Arc::new(FakeDriver::remote(
        MusicStorageKind::GoogleDrive,
        "google_drive:alice",
    ));
    let storages = storages(&catalog, vec![driver]);
    storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Backup,
            &StorageSetup {
                location: Some("folder-1".into()),
                ..StorageSetup::default()
            },
        )
        .await
        .unwrap();
    let remote = |namespace: &str, folder: &str| RootLocator::Remote {
        namespace: namespace.into(),
        path: vec![folder.into()],
    };
    let purpose = RootPurpose::Storage(StorageRole::Backup);

    assert!(storages
        .check_root_separation(&remote("google_drive:alice", "folder-1"), purpose)
        .await
        .is_err());
    for candidate in [
        remote("google_drive:bob", "folder-1"),
        remote("google_drive:alice", "folder-2"),
        RootLocator::HostPath("/folder-1".into()),
    ] {
        storages
            .check_root_separation(&candidate, purpose)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn open_all_reprobes_capabilities_and_records_faults() {
    let catalog = temp_catalog().await;
    let local = local_driver();
    let remote = Arc::new(FakeDriver::remote(MusicStorageKind::GoogleDrive, "account"));
    remote.set_health(HealthScript {
        status: HealthStatus::AuthenticationRequired,
        message: Some("reconnect Google".into()),
        capabilities: StorageCapabilities::default(),
        error: None,
    });
    let storages = storages(&catalog, vec![local.clone(), remote.clone()]);
    let root = tempfile::tempdir().unwrap();
    let persist = |storage: MusicStorage| {
        let catalog = catalog.clone();
        async move {
            MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
                .await
                .unwrap();
            storage
        }
    };
    let healthy = persist(
        MusicStorage::new(
            "Local",
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            StorageCapabilities::default(),
        )
        .with_root_identifier(root.path().to_string_lossy()),
    )
    .await;
    let stale_caps = StorageCapabilities {
        can_inventory: true,
        ..StorageCapabilities::default()
    };
    let unhealthy = persist(
        MusicStorage::new(
            "Drive",
            MusicStorageKind::GoogleDrive,
            StorageRole::Backup,
            stale_caps.clone(),
        )
        .with_root_identifier("folder"),
    )
    .await;
    let driverless = persist(
        MusicStorage::new(
            "Elsewhere",
            MusicStorageKind::Other("s3".into()),
            StorageRole::Backup,
            StorageCapabilities::default(),
        )
        .with_root_identifier("bucket"),
    )
    .await;
    let mut disabled = MusicStorage::new(
        "Off",
        MusicStorageKind::LocalDirectory,
        StorageRole::Backup,
        StorageCapabilities::default(),
    )
    .with_root_identifier("/nowhere");
    disabled.enabled = false;
    let disabled = persist(disabled).await;
    let synthetic = persist(MusicStorage::new(
        "Migrated local files",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    ))
    .await;

    let faults = storages.open_all().await.unwrap();

    let get = |id| {
        let catalog = catalog.clone();
        async move {
            MusicStorageRepository::get(catalog.as_ref(), id)
                .await
                .unwrap()
                .unwrap()
        }
    };
    assert_eq!(get(healthy.id).await.capabilities, writable_capabilities());
    assert_eq!(get(unhealthy.id).await.capabilities, stale_caps);
    assert!(storages.live_stores().contains(healthy.id));
    assert!(storages.live_stores().contains(unhealthy.id));
    assert!(!storages.live_stores().contains(driverless.id));
    assert!(!storages.live_stores().contains(disabled.id));
    assert!(!storages.live_stores().contains(synthetic.id));
    assert_eq!(local.calls().opened, vec![healthy.id]);

    let mut fault_ids: Vec<_> = faults.iter().map(|fault| fault.storage_id).collect();
    fault_ids.sort();
    let mut expected = vec![unhealthy.id, driverless.id];
    expected.sort();
    assert_eq!(fault_ids, expected);
    assert!(faults.iter().any(|fault| matches!(
        &fault.error,
        StorageError::Unhealthy { status: HealthStatus::AuthenticationRequired, message }
            if message == "reconnect Google"
    )));
    assert!(faults
        .iter()
        .any(|fault| matches!(&fault.error, StorageError::NoDriver(MusicStorageKind::Other(kind)) if kind == "s3")));
    assert_eq!(storages.faults().len(), 2);
}

/// Persists a local backup at `root` and opens it the way startup does.
async fn opened_backup(
    catalog: &Arc<TursoCatalog>,
    storages: &MusicStorages,
    driver: &FakeDriver,
    root: &Path,
) -> (MusicStorage, Arc<MemoryStore>) {
    let storage = MusicStorage::new(
        "Backup",
        MusicStorageKind::LocalDirectory,
        StorageRole::Backup,
        writable_capabilities(),
    )
    .with_root_identifier(root.to_string_lossy());
    MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
        .await
        .unwrap();
    storages.open_all().await.unwrap();
    let store = driver
        .store(storage.id)
        .expect("open_all opened the backup");
    (storage, store)
}

fn object_ids(objects: &[StorageObject]) -> Vec<&str> {
    objects
        .iter()
        .map(|object| object.object_id.as_str())
        .collect()
}

#[tokio::test]
async fn full_refresh_finalizes_objects_and_cursor() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let root = tempfile::tempdir().unwrap();
    let (storage, store) = opened_backup(&catalog, &storages, &driver, root.path()).await;
    store.insert("a.flac", b"one");

    let refreshed = storages.refresh(Target::Storage(storage.id)).await.unwrap();

    assert_eq!(refreshed.storage_id, storage.id);
    assert_eq!(refreshed.object_count, 1);
    assert!(!refreshed.incremental);
    assert!(matches!(
        store.inventory_requests()[0].mode,
        InventoryMode::Full
    ));
    let generation =
        AssetRepository::get_inventory_generation(catalog.as_ref(), refreshed.generation_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(generation.status, InventoryGenerationStatus::Finalized);
    assert_eq!(generation.kind, InventoryGenerationKind::Full);
    let objects = catalog_io::storage_objects(&catalog, storage.id)
        .await
        .unwrap();
    assert_eq!(object_ids(&objects), ["a.flac"]);
    assert!(objects[0].canonical_hash().is_some());
    let run = SyncRunRepository::get(catalog.as_ref(), refreshed.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.status, SyncStatus::Succeeded);
    assert_eq!(run.sync_cursor.unwrap().value, "1");
    assert!(MusicStorageRepository::get(catalog.as_ref(), storage.id)
        .await
        .unwrap()
        .unwrap()
        .last_inventory_at
        .is_some());
}

#[tokio::test]
async fn second_refresh_is_incremental_from_recorded_cursor() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let root = tempfile::tempdir().unwrap();
    let (storage, store) = opened_backup(&catalog, &storages, &driver, root.path()).await;
    store.insert("a.flac", b"one");
    store.insert("b.flac", b"two");
    storages.refresh(Target::Storage(storage.id)).await.unwrap();
    store.remove("a.flac");
    store.insert("c.flac", b"three");

    let second = storages.refresh(Target::Storage(storage.id)).await.unwrap();

    assert!(second.incremental);
    assert_eq!((second.object_count, second.removal_count), (1, 1));
    assert!(matches!(
        &store.inventory_requests()[1].mode,
        InventoryMode::Incremental { change_cursor } if change_cursor.value == "2"
    ));
    let generation =
        AssetRepository::get_inventory_generation(catalog.as_ref(), second.generation_id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(generation.kind, InventoryGenerationKind::Incremental);
    let objects = catalog_io::storage_objects(&catalog, storage.id)
        .await
        .unwrap();
    assert_eq!(object_ids(&objects), ["b.flac", "c.flac"]);

    // A store that stops tracking changes is listed in full again.
    store.set_health(HealthScript {
        capabilities: StorageCapabilities {
            supports_change_tracking: false,
            ..writable_capabilities()
        },
        ..HealthScript::default()
    });
    let third = storages.refresh(Target::Storage(storage.id)).await.unwrap();
    assert!(!third.incremental);
    let objects = catalog_io::storage_objects(&catalog, storage.id)
        .await
        .unwrap();
    assert_eq!(object_ids(&objects), ["b.flac", "c.flac"]);
}

#[tokio::test]
async fn unavailable_store_aborts_generation_and_fails_run() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let root = tempfile::tempdir().unwrap();
    let (storage, store) = opened_backup(&catalog, &storages, &driver, root.path()).await;
    store.insert("a.flac", b"one");
    let first = storages.refresh(Target::Storage(storage.id)).await.unwrap();
    store.insert("b.flac", b"two");
    store.fail_next_inventory(MusicStorageError::new(
        MusicStorageErrorKind::TemporarilyUnavailable,
        "storage is offline",
    ));

    let error = storages
        .refresh(Target::Storage(storage.id))
        .await
        .unwrap_err();

    assert!(matches!(
        &error,
        StorageError::Provider(error) if error.kind == MusicStorageErrorKind::TemporarilyUnavailable
    ));
    assert!(error.retryable());
    let latest =
        AssetRepository::latest_finalized_inventory_generation(catalog.as_ref(), storage.id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(latest.id, first.generation_id);
    let objects = catalog_io::storage_objects(&catalog, storage.id)
        .await
        .unwrap();
    assert_eq!(object_ids(&objects), ["a.flac"]);
    let failed = SyncRunRepository::latest(
        catalog.as_ref(),
        SyncRunFilter {
            kind: Some(SyncRunKind::StorageInventory),
            status: Some(SyncStatus::Failed),
            storage_id: Some(storage.id),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .expect("the failed inventory run is recorded");
    assert_eq!(
        failed.error.unwrap().kind,
        SyncErrorKind::TemporarilyUnavailable
    );

    // The next refresh resumes from the last successful cursor.
    let resumed = storages.refresh(Target::Storage(storage.id)).await.unwrap();
    assert!(resumed.incremental);
    let objects = catalog_io::storage_objects(&catalog, storage.id)
        .await
        .unwrap();
    assert_eq!(object_ids(&objects), ["a.flac", "b.flac"]);
}

#[tokio::test]
async fn refresh_storage_opens_on_demand_and_rejects_reauthorisation() {
    let catalog = temp_catalog().await;
    let driver = Arc::new(FakeDriver::remote(MusicStorageKind::GoogleDrive, "account"));
    let storages = storages(&catalog, vec![driver.clone()]);
    let storage = MusicStorage::new(
        "Drive",
        MusicStorageKind::GoogleDrive,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    )
    .with_root_identifier("folder");
    MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
        .await
        .unwrap();

    let refreshed = storages.refresh(Target::Storage(storage.id)).await.unwrap();

    assert!(storages.live_stores().contains(storage.id));
    assert_eq!(
        AssetRepository::latest_finalized_inventory_generation(catalog.as_ref(), storage.id)
            .await
            .unwrap()
            .map(|generation| generation.id),
        Some(refreshed.generation_id)
    );

    let store = driver.store(storage.id).unwrap();
    store.set_health(HealthScript {
        status: HealthStatus::AuthenticationRequired,
        message: Some("reconnect Google".into()),
        capabilities: StorageCapabilities::default(),
        error: None,
    });
    let error = storages
        .refresh(Target::Storage(storage.id))
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        StorageError::Unhealthy { status: HealthStatus::AuthenticationRequired, message }
            if message == "reconnect Google"
    ));
    assert_eq!(store.inventory_requests().len(), 1);

    let missing = Uuid::new_v4();
    assert!(matches!(
        storages.refresh(Target::Storage(missing)).await,
        Err(StorageError::StorageNotFound(id)) if id == missing
    ));
    assert!(matches!(
        storages.refresh(Target::Primary).await,
        Err(StorageError::NoPrimary)
    ));
}

fn titled_track(source_id: Uuid, title: &str) -> CanonicalTrack {
    let mut track = CanonicalTrack::new();
    track.metadata = CanonicalMetadata::new();
    track.metadata.add(MetadataFieldValue::new(
        MetadataField::Title,
        MetadataValue::Text(title.to_owned()),
        MetadataProvenance::provider(source_id, "source-track", Utc::now()),
    ));
    track
}

/// Records that `source` can obtain `track`, which puts the track in scope
/// for reconciliation.
async fn observe(catalog: &TursoCatalog, source_id: Uuid, track_id: Uuid) {
    let mut occurrence = SourceTrackOccurrence::new(source_id, track_id.to_string());
    occurrence.canonical_track_id = Some(track_id);
    occurrence.availability = SourceAvailability::Obtainable;
    SourceRepository::upsert_occurrences(catalog, vec![occurrence])
        .await
        .unwrap();
}

/// A source-observed track whose original is `bytes`, hashed.
async fn track_with_original(
    catalog: &TursoCatalog,
    source_id: Uuid,
    bytes: &[u8],
) -> OriginalAsset {
    let track = CanonicalTrack::new();
    TrackRepository::upsert(catalog, track.clone())
        .await
        .unwrap();
    observe(catalog, source_id, track.id).await;
    let original = OriginalAsset::new(
        track.id,
        OriginalAssetRole::Master,
        "kept.flac",
        bytes.len() as u64,
        sha256(bytes).value(),
    )
    .unwrap();
    AssetRepository::upsert_original(catalog, original.clone())
        .await
        .unwrap();
    original
}

/// A local catalog source in its own directory.
async fn library_source(catalog: &TursoCatalog, library: &Path) -> CatalogSource {
    let source = CatalogSource::local_folder("Library", library.to_string_lossy());
    SourceRepository::upsert(catalog, source.clone())
        .await
        .unwrap();
    source
}

#[tokio::test]
async fn refresh_persists_verified_matches_and_weak_candidates() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let kept = track_with_original(&catalog, source.id, b"abc").await;
    let song = titled_track(source.id, "Song");
    TrackRepository::upsert(catalog.as_ref(), song.clone())
        .await
        .unwrap();
    AssetRepository::upsert_original(
        catalog.as_ref(),
        OriginalAsset::new_unverified(song.id, OriginalAssetRole::Master, "unrelated.flac"),
    )
    .await
    .unwrap();
    observe(&catalog, source.id, song.id).await;
    let root = tempfile::tempdir().unwrap();
    let primary = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(root.path()),
        )
        .await
        .unwrap();
    let store = driver.store(primary.id).unwrap();
    store.insert("kept.flac", b"abc");
    store.insert_unhashed("Song.flac", b"song");

    let refreshed = storages.refresh(Target::Primary).await.unwrap();

    assert_eq!(refreshed.storage_id, primary.id);
    assert!(refreshed
        .report
        .verified_matches
        .iter()
        .any(|matched| matched.asset_id == kept.id()));
    let location = AssetRepository::get_location(catalog.as_ref(), primary.id, "kept.flac".into())
        .await
        .unwrap()
        .expect("the hash match is recorded as a location");
    assert_eq!(location.asset_id, kept.id());
    assert_eq!(
        location.verification_status,
        StorageVerificationStatus::Verified
    );
    let candidates = ReconciliationRepository::list_candidates(
        catalog.as_ref(),
        MatchCandidateFilter {
            confidence: Some(MatchConfidence::ReviewRequired),
            ..Default::default()
        },
        RepositoryPageRequest::default(),
    )
    .await
    .unwrap();
    assert!(!candidates.items.is_empty());
}

#[tokio::test]
async fn first_primary_connect_switches_after_refresh() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let root = tempfile::tempdir().unwrap();

    let primary = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(root.path()),
        )
        .await
        .unwrap();

    assert_eq!(primary.role, StorageRole::Primary);
    assert_eq!(
        MusicStorageRepository::get_active_primary(catalog.as_ref())
            .await
            .unwrap()
            .map(|storage| storage.id),
        Some(primary.id)
    );
    let generation =
        AssetRepository::latest_finalized_inventory_generation(catalog.as_ref(), primary.id)
            .await
            .unwrap()
            .expect("connect inventories the new storage");
    assert_eq!(generation.kind, InventoryGenerationKind::Full);
    let reconciliation = SyncRunRepository::latest(
        catalog.as_ref(),
        SyncRunFilter {
            kind: Some(SyncRunKind::Reconciliation),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(reconciliation.storage_id, Some(primary.id));
}

#[tokio::test]
async fn project_reports_health_and_report() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let kept = track_with_original(&catalog, source.id, b"abc").await;
    let root = tempfile::tempdir().unwrap();
    let primary = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(root.path()),
        )
        .await
        .unwrap();
    let store = driver.store(primary.id).unwrap();
    store.insert("kept.flac", b"abc");
    storages.refresh(Target::Primary).await.unwrap();
    store.set_health(HealthScript {
        status: HealthStatus::Degraded,
        ..HealthScript::default()
    });
    let mut closed = MusicStorage::new(
        "Closed",
        MusicStorageKind::LocalDirectory,
        StorageRole::Backup,
        writable_capabilities(),
    )
    .with_root_identifier("/closed");
    closed.enabled = false;
    MusicStorageRepository::upsert(catalog.as_ref(), closed.clone())
        .await
        .unwrap();
    let faulty = MusicStorage::new(
        "Elsewhere",
        MusicStorageKind::Other("s3".into()),
        StorageRole::Backup,
        StorageCapabilities::default(),
    )
    .with_root_identifier("bucket");
    MusicStorageRepository::upsert(catalog.as_ref(), faulty.clone())
        .await
        .unwrap();
    storages.open_all().await.unwrap();
    let probes = store.health_calls();

    let projection = storages.project().await.unwrap();

    let view = |id: Uuid| {
        projection
            .storages
            .iter()
            .find(|view| view.storage.id == id)
            .unwrap()
    };
    let primary_view = view(primary.id);
    assert!(primary_view.is_active_primary && primary_view.open);
    assert_eq!(
        primary_view.health.as_ref().map(|health| health.status),
        Some(HealthStatus::Degraded)
    );
    assert!(
        primary_view.fault.is_some(),
        "open_all recorded the degraded probe"
    );
    assert!(!view(closed.id).open && view(closed.id).health.is_none());
    assert!(!view(closed.id).is_active_primary);
    assert!(matches!(
        view(faulty.id).fault.as_ref().map(|fault| &fault.error),
        Some(StorageError::NoDriver(_))
    ));
    // The primary is probed once and that probe also decides its availability.
    assert_eq!(store.health_calls(), probes + 1);
    assert!(projection
        .report
        .verified_matches
        .iter()
        .any(|matched| matched.asset_id == kept.id() && matched.storage_id == primary.id));
}

/// Module storages whose offline cache lives under `cache_root`.
fn storages_caching_in(
    catalog: &Arc<TursoCatalog>,
    drivers: Vec<Arc<dyn StorageDriver>>,
    cache_root: &Path,
) -> MusicStorages {
    MusicStorages::new(
        catalog.clone(),
        StorageDrivers::new(drivers),
        MusicStoragesConfig {
            cache_root: cache_root.to_path_buf(),
            ..config()
        },
        Arc::new(LiveStores::default()),
        Arc::new(NoHandoff),
    )
}

fn remote_driver() -> Arc<FakeDriver> {
    Arc::new(FakeDriver::remote(
        MusicStorageKind::GoogleDrive,
        "google_drive:account",
    ))
}

/// A remote primary holding `bytes` as `kept.flac`, verified for a track by
/// the refresh that connecting runs.
async fn remote_primary_with_track(
    catalog: &Arc<TursoCatalog>,
    storages: &MusicStorages,
    driver: &FakeDriver,
    bytes: &[u8],
) -> (MusicStorage, Arc<MemoryStore>, OriginalAsset) {
    let library = tempfile::tempdir().unwrap();
    let source = library_source(catalog, library.path()).await;
    let original = track_with_original(catalog, source.id, bytes).await;
    let primary = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Primary,
            &StorageSetup {
                location: Some("primary-folder".into()),
                ..StorageSetup::default()
            },
        )
        .await
        .unwrap();
    let store = driver.store(primary.id).unwrap();
    store.insert("kept.flac", bytes);
    storages.refresh(Target::Primary).await.unwrap();
    (primary, store, original)
}

async fn location(catalog: &TursoCatalog, storage_id: Uuid, object_id: &str) -> AssetLocation {
    AssetRepository::get_location(catalog, storage_id, object_id.into())
        .await
        .unwrap()
        .expect("location recorded")
}

/// Records `object` as a verified copy of `original`.
async fn verify_on(catalog: &TursoCatalog, object: &StorageObject, original: &OriginalAsset) {
    AssetRepository::upsert_storage_object(catalog, object.clone())
        .await
        .unwrap();
    AssetRepository::upsert_location(
        catalog,
        AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.id(),
            asset_type: AssetType::Original,
            storage_id: object.storage_id,
            object_id: object.object_id.clone(),
            verification_status: StorageVerificationStatus::Verified,
            expected_hash: original.content_hash.clone(),
            observed_hash: object.canonical_hash().cloned(),
            provider_version: object.provider_version.clone(),
            last_checked_at: Some(Utc::now()),
            verified_at: Some(Utc::now()),
            error_message: None,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn playable_downloads_once_and_pins_verified_cache() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let cache = tempfile::tempdir().unwrap();
    let storages = storages_caching_in(&catalog, vec![driver.clone()], cache.path());
    let (primary, store, original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let track_id = original.track_id();

    let filled = storages.playable(track_id).await.unwrap();
    let hit = storages.playable(track_id).await.unwrap();

    assert_eq!(filled.origin, PlayableOrigin::OfflineCache { hit: false });
    assert_eq!(
        (
            filled.asset_id,
            filled.storage_id,
            filled.object_id.as_str()
        ),
        (original.id(), primary.id, "kept.flac")
    );
    assert!(filled
        .path
        .starts_with(cache.path().join(primary.id.to_string())));
    assert_eq!(std::fs::read(&filled.path).unwrap(), b"abc");
    assert_eq!(hit.origin, PlayableOrigin::OfflineCache { hit: true });
    assert_eq!(hit.path, filled.path);
    assert_eq!(store.downloads().len(), 1);
    assert_eq!(
        store.downloads()[0].expected_checksum,
        original.content_hash
    );

    storages.set_offline_pinned(track_id, true).await.unwrap();
    let key = CacheKey::for_location(&location(&catalog, primary.id, "kept.flac").await);
    assert!(storages.0.cache.is_pinned(&primary, &key));
    storages.set_offline_pinned(track_id, false).await.unwrap();
    assert!(!storages.0.cache.is_pinned(&primary, &key));

    // A cached file that no longer verifies is discarded and downloaded again.
    std::fs::write(&filled.path, b"xyz").unwrap();
    let refilled = storages.playable(track_id).await.unwrap();
    assert_eq!(refilled.origin, PlayableOrigin::OfflineCache { hit: false });
    assert_eq!(std::fs::read(&refilled.path).unwrap(), b"abc");
    assert_eq!(store.downloads().len(), 2);
}

#[tokio::test]
async fn cache_key_changes_with_revision() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let cache = tempfile::tempdir().unwrap();
    let storages = storages_caching_in(&catalog, vec![driver.clone()], cache.path());
    let (primary, store, original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let first = storages.playable(original.track_id()).await.unwrap();

    // The provider reports a new revision of the same object.
    store.insert("kept.flac", b"abc");
    storages.refresh(Target::Primary).await.unwrap();
    let second = storages.playable(original.track_id()).await.unwrap();

    assert_eq!(second.origin, PlayableOrigin::OfflineCache { hit: false });
    assert_ne!(second.path, first.path);
    assert_eq!(store.downloads().len(), 2);

    // Without a provider version the expected content hash is the revision.
    let mut located = location(&catalog, primary.id, "kept.flac").await;
    let versioned = CacheKey::for_location(&located);
    located.provider_version = None;
    let by_hash = CacheKey::for_location(&located);
    located.expected_hash = Some(sha256(b"other"));
    assert_ne!(versioned, by_hash);
    assert_ne!(by_hash, CacheKey::for_location(&located));
    // Object IDs are hashed, never used as paths.
    located.object_id = "a/b".into();
    let slash = CacheKey::for_location(&located);
    located.object_id = "a?b".into();
    assert_ne!(slash, CacheKey::for_location(&located));
}

#[tokio::test]
async fn playable_prefers_direct_path_without_cache() {
    let catalog = temp_catalog().await;
    let root = tempfile::tempdir().unwrap();
    let driver =
        Arc::new(FakeDriver::host(MusicStorageKind::LocalDirectory).with_direct_root(root.path()));
    let cache = tempfile::tempdir().unwrap();
    let storages = storages_caching_in(&catalog, vec![driver.clone()], cache.path());
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let original = track_with_original(&catalog, source.id, b"abc").await;
    let primary = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(root.path()),
        )
        .await
        .unwrap();
    let store = driver.store(primary.id).unwrap();
    store.insert("kept.flac", b"abc");
    std::fs::write(root.path().join("kept.flac"), b"abc").unwrap();
    storages.refresh(Target::Primary).await.unwrap();

    let playable = storages.playable(original.track_id()).await.unwrap();
    storages
        .set_offline_pinned(original.track_id(), true)
        .await
        .unwrap();

    assert_eq!(playable.origin, PlayableOrigin::Direct);
    assert_eq!(playable.path, root.path().join("kept.flac"));
    assert!(store.downloads().is_empty());
    assert!(!cache.path().join(primary.id.to_string()).exists());
}

#[tokio::test]
async fn playable_never_falls_back_to_backup() {
    let catalog = temp_catalog().await;
    let remote = remote_driver();
    let local = local_driver();
    let cache = tempfile::tempdir().unwrap();
    let storages = storages_caching_in(&catalog, vec![remote.clone(), local.clone()], cache.path());
    let (primary, _store, on_primary) =
        remote_primary_with_track(&catalog, &storages, &remote, b"abc").await;
    let backup_root = tempfile::tempdir().unwrap();
    let backup = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(backup_root.path()),
        )
        .await
        .unwrap();
    let backup_store = local.store(backup.id).unwrap();
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let backed_up = track_with_original(&catalog, source.id, b"backed up").await;
    verify_on(
        &catalog,
        &backup_store.insert("backed-up.flac", b"backed up"),
        &backed_up,
    )
    .await;
    let nowhere = track_with_original(&catalog, source.id, b"nowhere").await;

    let backup_only = storages.playable(backed_up.track_id()).await.unwrap_err();
    let missing = storages.playable(nowhere.track_id()).await.unwrap_err();

    assert!(matches!(
        backup_only,
        StorageError::PrimaryUnavailable {
            backup_has_copy: true
        }
    ));
    assert!(backup_only.to_string().contains("restore it from backup"));
    assert!(matches!(
        missing,
        StorageError::PrimaryUnavailable {
            backup_has_copy: false
        }
    ));
    assert!(backup_store.downloads().is_empty());

    // A primary copy that is neither cached nor reachable is not replaced by
    // the backup's either.
    verify_on(
        &catalog,
        &backup_store.insert("kept.flac", b"abc"),
        &on_primary,
    )
    .await;
    storages.live_stores().remove(primary.id);
    assert!(matches!(
        storages.playable(on_primary.track_id()).await,
        Err(StorageError::PrimaryUnavailable {
            backup_has_copy: true
        })
    ));
    assert!(backup_store.downloads().is_empty());
}

#[tokio::test]
async fn playable_without_primary_is_no_primary() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let original = track_with_original(&catalog, source.id, b"abc").await;

    assert!(matches!(
        storages.playable(original.track_id()).await,
        Err(StorageError::NoPrimary)
    ));
    assert!(matches!(
        storages.set_offline_pinned(original.track_id(), true).await,
        Err(StorageError::NoPrimary)
    ));
}

/// A location for `object` expecting `bytes`, as reconciliation records it.
fn cached_location(object: &StorageObject, bytes: &[u8]) -> AssetLocation {
    AssetLocation {
        id: Uuid::new_v4(),
        asset_id: Uuid::new_v4(),
        asset_type: AssetType::Original,
        storage_id: object.storage_id,
        object_id: object.object_id.clone(),
        verification_status: StorageVerificationStatus::Verified,
        expected_hash: Some(sha256(bytes)),
        observed_hash: object.canonical_hash().cloned(),
        provider_version: object.provider_version.clone(),
        last_checked_at: None,
        verified_at: None,
        error_message: None,
    }
}

#[tokio::test]
async fn offline_cache_evicts_unpinned_lru_and_recovers_from_corruption() {
    let root = tempfile::tempdir().unwrap();
    let cache = OfflineCache::new(root.path().to_path_buf(), 1024);
    let mut storage = MusicStorage::new(
        "Drive",
        MusicStorageKind::GoogleDrive,
        StorageRole::Primary,
        writable_capabilities(),
    );
    storage.config.insert("cache_max_bytes".into(), "6".into());
    let store = MemoryStore::new(storage.id);
    let fill = |object_id: &'static str, bytes: &'static [u8]| {
        let location = cached_location(&store.insert(object_id, bytes), bytes);
        let key = CacheKey::for_location(&location);
        (location, key)
    };
    let (first, first_key) = fill("a/b", b"aaa");
    let first_path = cache
        .fill(&storage, &store, &first_key, &first)
        .await
        .unwrap();
    cache.set_pinned(&storage, &first_key, true).unwrap();
    let (second, second_key) = fill("second", b"bbb");
    cache
        .fill(&storage, &store, &second_key, &second)
        .await
        .unwrap();
    let (third, third_key) = fill("third", b"ccc");
    cache
        .fill(&storage, &store, &third_key, &third)
        .await
        .unwrap();

    // Over budget: the oldest unpinned entry went, the pinned and newest stay.
    let expected = |location: &AssetLocation| location.expected_hash.clone();
    assert!(cache
        .lookup(&storage, &first_key, expected(&first).as_ref())
        .unwrap()
        .is_some());
    assert!(cache
        .lookup(&storage, &second_key, expected(&second).as_ref())
        .unwrap()
        .is_none());
    // A file that no longer hashes to its entry is discarded.
    std::fs::write(&first_path, b"bad").unwrap();
    assert!(cache
        .lookup(&storage, &first_key, expected(&first).as_ref())
        .unwrap()
        .is_none());
    assert!(!first_path.exists());
    // A corrupt index is set aside and the cache starts over.
    let directory = root.path().join(storage.id.to_string());
    std::fs::write(directory.join("cache-index.json"), b"not-json").unwrap();
    assert!(cache
        .lookup(&storage, &third_key, expected(&third).as_ref())
        .unwrap()
        .is_none());
    assert!(std::fs::read_dir(&directory).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("cache-index.corrupt-")));

    // Bytes that do not match the expected hash are never recorded.
    let (mut wrong, wrong_key) = fill("wrong", b"www");
    wrong.expected_hash = Some(sha256(b"something else"));
    assert!(cache
        .fill(&storage, &store, &wrong_key, &wrong)
        .await
        .is_err());
    assert!(cache.lookup(&storage, &wrong_key, None).unwrap().is_none());
    assert!(!std::fs::read_dir(&directory).unwrap().any(|entry| entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .ends_with(".tmp")));
}

#[tokio::test]
async fn clear_cache_removes_only_that_storage_cache() {
    let catalog = temp_catalog().await;
    let cache = tempfile::tempdir().unwrap();
    let storages = storages_caching_in(&catalog, vec![local_driver()], cache.path());
    let mut persisted_storages = Vec::new();
    for name in ["Cleared", "Kept"] {
        let storage = MusicStorage::new(
            name,
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        let directory = cache.path().join(storage.id.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("cached"), b"bytes").unwrap();
        persisted_storages.push(storage);
    }
    let (cleared, kept) = (&persisted_storages[0], &persisted_storages[1]);

    storages
        .release(cleared.id, Release::ClearCache)
        .await
        .unwrap();
    storages
        .release(cleared.id, Release::ClearCache)
        .await
        .unwrap();

    assert!(!cache.path().join(cleared.id.to_string()).exists());
    assert!(cache
        .path()
        .join(kept.id.to_string())
        .join("cached")
        .exists());
    assert_eq!(persisted(&catalog).await.len(), 2);
    assert_eq!(
        MusicStorageRepository::get(catalog.as_ref(), cleared.id)
            .await
            .unwrap()
            .as_ref(),
        Some(cleared)
    );
    let missing = Uuid::new_v4();
    assert!(matches!(
        storages.release(missing, Release::ClearCache).await,
        Err(StorageError::StorageNotFound(id)) if id == missing
    ));
}

/// Module storages that stage under `staging_root` and hand off to `handoff`.
fn storages_with(
    catalog: &Arc<TursoCatalog>,
    drivers: Vec<Arc<dyn StorageDriver>>,
    staging_root: &Path,
    handoff: Arc<dyn ReplicationHandoff>,
) -> MusicStorages {
    MusicStorages::new(
        catalog.clone(),
        StorageDrivers::new(drivers),
        MusicStoragesConfig {
            staging_root: staging_root.to_path_buf(),
            ..config()
        },
        Arc::new(LiveStores::default()),
        handoff,
    )
}

/// A local catalog source at `library` with `policy`.
async fn policy_source(
    catalog: &TursoCatalog,
    library: &Path,
    policy: SourceCopyPolicy,
) -> CatalogSource {
    let mut source = CatalogSource::local_folder("Library", library.to_string_lossy());
    source.copy_policy = policy;
    SourceRepository::upsert(catalog, source.clone())
        .await
        .unwrap();
    source
}

/// Writes `bytes` to `name` in `library` and catalogues the file as an
/// obtainable occurrence, with a hashed original, of a new track of `source`.
async fn source_file(
    catalog: &TursoCatalog,
    source: &CatalogSource,
    library: &Path,
    name: &str,
    bytes: &[u8],
) -> OriginalAsset {
    std::fs::write(library.join(name), bytes).unwrap();
    let track = CanonicalTrack::new();
    TrackRepository::upsert(catalog, track.clone())
        .await
        .unwrap();
    let mut occurrence = SourceTrackOccurrence::new(source.id, name);
    occurrence.canonical_track_id = Some(track.id);
    occurrence.availability = SourceAvailability::Obtainable;
    occurrence
        .provider_metadata
        .insert("relative_path".into(), MetadataValue::Text(name.into()));
    SourceRepository::upsert_occurrences(catalog, vec![occurrence])
        .await
        .unwrap();
    let original = OriginalAsset::new(
        track.id,
        OriginalAssetRole::Master,
        name,
        bytes.len() as u64,
        sha256(bytes).value(),
    )
    .unwrap();
    AssetRepository::upsert_original(catalog, original.clone())
        .await
        .unwrap();
    original
}

async fn local_primary(storages: &MusicStorages, root: &Path) -> MusicStorage {
    storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Primary,
            &at(root),
        )
        .await
        .unwrap()
}

async fn transfer_settings(catalog: &TursoCatalog) -> Vec<String> {
    SettingsRepository::list(catalog)
        .await
        .unwrap()
        .into_iter()
        .map(|setting| setting.key)
        .filter(|key| key.starts_with("storage.") || key.starts_with("google_drive.upload."))
        .collect()
}

#[tokio::test]
async fn copy_respects_check_only_and_ask_policies() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    let mut source = policy_source(&catalog, library.path(), SourceCopyPolicy::CheckOnly).await;
    let original = source_file(&catalog, &source, library.path(), "song.flac", b"song").await;
    let root = tempfile::tempdir().unwrap();
    let primary = local_primary(&storages, root.path()).await;
    let store = driver.store(primary.id).unwrap();
    let track_id = original.track_id();

    assert!(matches!(
        storages
            .copy_to_primary(track_id, CopyAuthorization::UserConfirmed)
            .await,
        Err(StorageError::CopyForbidden)
    ));
    source.copy_policy = SourceCopyPolicy::AskBeforeCopying;
    SourceRepository::upsert(catalog.as_ref(), source.clone())
        .await
        .unwrap();
    assert!(matches!(
        storages
            .copy_to_primary(track_id, CopyAuthorization::Automatic)
            .await,
        Err(StorageError::CopyConfirmationRequired)
    ));
    assert!(store.uploads().is_empty());
    assert!(
        AssetRepository::list_locations_for_asset(catalog.as_ref(), original.id())
            .await
            .unwrap()
            .is_empty()
    );

    let copied = storages
        .copy_to_primary(track_id, CopyAuthorization::UserConfirmed)
        .await
        .unwrap();

    assert_eq!(copied.origin, CopyOrigin::CatalogSource(source.id));
    assert_eq!(
        (copied.asset_id, copied.storage_id),
        (original.id(), primary.id)
    );
    let short_id = &track_id.to_string()[..8];
    assert_eq!(
        copied.object_id,
        format!("Unknown Artist/song [{short_id}].flac")
    );
    assert_eq!(
        store.bytes(&copied.object_id).as_deref(),
        Some(&b"song"[..])
    );
    let location = location(&catalog, primary.id, &copied.object_id).await;
    assert_eq!(location.asset_id, original.id());
    assert_eq!(
        location.verification_status,
        StorageVerificationStatus::Verified
    );
    assert_eq!(location.observed_hash, Some(sha256(b"song")));
}

#[tokio::test]
async fn copy_restores_from_backup_by_download() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let staging = tempfile::tempdir().unwrap();
    let storages = storages_with(
        &catalog,
        vec![driver.clone()],
        staging.path(),
        Arc::new(NoHandoff),
    );
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let original = track_with_original(&catalog, source.id, b"kept").await;
    let folder = |name: &str| StorageSetup {
        location: Some(name.into()),
        ..StorageSetup::default()
    };
    let primary = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Primary,
            &folder("primary-folder"),
        )
        .await
        .unwrap();
    let backup = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Backup,
            &folder("backup-folder"),
        )
        .await
        .unwrap();
    let backup_store = driver.store(backup.id).unwrap();
    verify_on(
        &catalog,
        &backup_store.insert("kept.flac", b"kept"),
        &original,
    )
    .await;

    let copied = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap();

    assert_eq!(copied.origin, CopyOrigin::Storage(backup.id));
    assert_eq!(backup_store.downloads().len(), 1);
    let primary_store = driver.store(primary.id).unwrap();
    assert_eq!(primary_store.uploads().len(), 1);
    assert_eq!(
        primary_store.bytes(&copied.object_id).as_deref(),
        Some(&b"kept"[..])
    );
    assert_eq!(
        location(&catalog, primary.id, &copied.object_id)
            .await
            .verification_status,
        StorageVerificationStatus::Verified
    );
    assert_eq!(
        std::fs::read_dir(staging.path()).unwrap().count(),
        0,
        "the staged download is removed"
    );
}

#[tokio::test]
async fn copy_in_place_stats_without_upload() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    let source = policy_source(
        &catalog,
        library.path(),
        SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
    )
    .await;
    let original = source_file(&catalog, &source, library.path(), "track.flac", b"audio").await;
    let primary = local_primary(&storages, library.path()).await;
    let store = driver.store(primary.id).unwrap();
    store.insert("track.flac", b"audio");

    let copied = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap();

    assert_eq!(copied.origin, CopyOrigin::InPlace);
    assert_eq!(copied.object_id, "track.flac");
    assert!(store.uploads().is_empty());
    assert_eq!(
        location(&catalog, primary.id, "track.flac")
            .await
            .verification_status,
        StorageVerificationStatus::Verified
    );
    assert_eq!(std::fs::read_dir(library.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn checksum_mismatch_persists_nothing() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    let source = policy_source(
        &catalog,
        library.path(),
        SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
    )
    .await;
    let original = source_file(&catalog, &source, library.path(), "song.flac", b"song").await;
    let root = tempfile::tempdir().unwrap();
    let mut primary = local_primary(&storages, root.path()).await;
    let store = driver.store(primary.id).unwrap();
    store.corrupt_uploads(true);
    let assert_nothing_persisted = || async {
        assert!(
            AssetRepository::list_locations_for_asset(catalog.as_ref(), original.id())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(catalog_io::storage_objects(catalog.as_ref(), primary.id)
            .await
            .unwrap()
            .is_empty());
        assert!(SyncRunRepository::latest(
            catalog.as_ref(),
            SyncRunFilter {
                kind: Some(SyncRunKind::AssetTransfer),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .is_none());
        assert_eq!(
            AssetRepository::get_original(catalog.as_ref(), original.id())
                .await
                .unwrap()
                .as_ref(),
            Some(&original)
        );
        assert!(transfer_settings(&catalog).await.is_empty());
    };

    // The stat hash differs, so the bytes are downloaded back and rejected.
    let error = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Verification(_)), "{error}");
    assert_eq!(store.downloads().len(), 1);
    assert_nothing_persisted().await;

    // Without verify_after_copy a contradicting hash is rejected outright.
    primary
        .config
        .insert("verify_after_copy".into(), "false".into());
    MusicStorageRepository::upsert(catalog.as_ref(), primary.clone())
        .await
        .unwrap();
    let error = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Verification(_)), "{error}");
    assert_eq!(store.downloads().len(), 1, "no download check was made");
    assert_nothing_persisted().await;
}

#[tokio::test]
async fn checkpoint_uses_neutral_prefix_and_is_removed() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    let source = policy_source(
        &catalog,
        library.path(),
        SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
    )
    .await;
    let original = source_file(&catalog, &source, library.path(), "song.flac", b"song").await;
    let root = tempfile::tempdir().unwrap();
    let primary = local_primary(&storages, root.path()).await;
    let store = driver.store(primary.id).unwrap();
    store.fail_next_upload(MusicStorageError::new(
        MusicStorageErrorKind::TemporarilyUnavailable,
        "storage is offline",
    ));

    let error = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap_err();

    assert!(error.retryable());
    let idempotency_key = format!(
        "copy-to-primary:{}:{}:{}",
        primary.id,
        original.id(),
        sha256(b"song").value()
    );
    let key = transfer_key(primary.id, original.id(), &idempotency_key);
    assert!(key.starts_with(&format!(
        "storage.{}.transfer.{}.",
        primary.id,
        original.id()
    )));
    assert_eq!(transfer_settings(&catalog).await, vec![key.clone()]);
    let checkpoint = SettingsRepository::get(catalog.as_ref(), key.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        checkpoint.value["idempotency_key"].as_str(),
        Some(idempotency_key.as_str())
    );

    storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap();

    assert!(transfer_settings(&catalog).await.is_empty());
    let uploads = store.uploads();
    assert_eq!(uploads.len(), 2);
    assert!(uploads
        .iter()
        .all(|upload| upload.idempotency_key.as_deref() == Some(idempotency_key.as_str())));
}

/// Records every handoff and reports one queued job per asset.
#[derive(Default)]
struct RecordingHandoff {
    calls: std::sync::Mutex<Vec<Vec<Uuid>>>,
}

impl ReplicationHandoff for RecordingHandoff {
    fn verified_primary_write<'a>(
        &'a self,
        asset_ids: &'a [Uuid],
    ) -> rec_core::BoxFuture<'a, HandoffReport> {
        self.calls.lock().unwrap().push(asset_ids.to_vec());
        Box::pin(std::future::ready(HandoffReport {
            planned_assets: asset_ids.len(),
            queued_jobs: asset_ids.len(),
            ..HandoffReport::default()
        }))
    }
}

#[tokio::test]
async fn handoff_receives_copied_asset() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let handoff = Arc::new(RecordingHandoff::default());
    let staging = tempfile::tempdir().unwrap();
    let storages = storages_with(
        &catalog,
        vec![driver.clone()],
        staging.path(),
        handoff.clone(),
    );
    let library = tempfile::tempdir().unwrap();
    let source = policy_source(
        &catalog,
        library.path(),
        SourceCopyPolicy::AutomaticallyCopyEligibleFiles,
    )
    .await;
    let original = source_file(&catalog, &source, library.path(), "song.flac", b"song").await;
    let root = tempfile::tempdir().unwrap();
    local_primary(&storages, root.path()).await;

    let copied = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap();

    assert_eq!(*handoff.calls.lock().unwrap(), vec![vec![original.id()]]);
    assert_eq!(copied.handoff.planned_assets, 1);
    assert_eq!(copied.handoff.queued_jobs, 1);
    assert!(copied.handoff.message().contains("1 durable job(s) queued"));

    // Copying again finds the verified copy and hands it off again.
    let again = storages
        .copy_to_primary(original.track_id(), CopyAuthorization::Automatic)
        .await
        .unwrap();
    assert_eq!(again.origin, CopyOrigin::AlreadyPresent);
    assert_eq!(again.object_id, copied.object_id);
    assert_eq!(handoff.calls.lock().unwrap().len(), 2);
}

/// A remote storage in `folder`, connected for `role`.
async fn remote_storage(storages: &MusicStorages, role: StorageRole, folder: &str) -> MusicStorage {
    storages
        .connect(
            MusicStorageKind::GoogleDrive,
            role,
            &StorageSetup {
                location: Some(folder.into()),
                ..StorageSetup::default()
            },
        )
        .await
        .unwrap()
}

async fn active_primary_id(catalog: &TursoCatalog) -> Option<Uuid> {
    MusicStorageRepository::get_active_primary(catalog)
        .await
        .unwrap()
        .map(|storage| storage.id)
}

async fn role_of(catalog: &TursoCatalog, storage_id: Uuid) -> StorageRole {
    MusicStorageRepository::get(catalog, storage_id)
        .await
        .unwrap()
        .unwrap()
        .role
}

#[tokio::test]
async fn migration_switches_only_after_destination_verification() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let staging = tempfile::tempdir().unwrap();
    let storages = storages_with(
        &catalog,
        vec![driver.clone()],
        staging.path(),
        Arc::new(NoHandoff),
    );
    let (primary, _store, original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let destination = remote_storage(&storages, StorageRole::Unassigned, "new-folder").await;
    let destination_store = driver.store(destination.id).unwrap();
    destination_store.corrupt_uploads(true);

    let error = storages
        .migrate_primary(destination.id, MigrationOptions::default())
        .await
        .unwrap_err();

    assert!(
        matches!(error, StorageError::MigrationIncomplete(1)),
        "{error}"
    );
    assert_eq!(active_primary_id(&catalog).await, Some(primary.id));
    assert_eq!(
        role_of(&catalog, destination.id).await,
        StorageRole::Unassigned
    );
    assert!(
        AssetRepository::list_locations_for_asset(catalog.as_ref(), original.id())
            .await
            .unwrap()
            .iter()
            .all(|location| location.storage_id != destination.id),
        "unverified bytes are never recorded"
    );

    destination_store.corrupt_uploads(false);
    let outcome = storages
        .migrate_primary(destination.id, MigrationOptions::default())
        .await
        .unwrap();

    assert_eq!(
        (outcome.previous_primary, outcome.active_primary),
        (primary.id, destination.id)
    );
    assert_eq!((outcome.copied_assets, outcome.already_present), (1, 0));
    assert_eq!(active_primary_id(&catalog).await, Some(destination.id));
    assert_eq!(role_of(&catalog, primary.id).await, StorageRole::Unassigned);
    let key = format!(
        "primary-migration:{}:{}:{}",
        primary.id,
        destination.id,
        original.id()
    );
    let uploads = destination_store.uploads();
    assert_eq!(uploads.len(), 2);
    assert!(uploads
        .iter()
        .all(|upload| upload.idempotency_key.as_deref() == Some(key.as_str())));
    let verified = AssetRepository::list_locations_for_asset(catalog.as_ref(), original.id())
        .await
        .unwrap()
        .into_iter()
        .find(|location| location.storage_id == destination.id)
        .expect("the copy is recorded on the destination");
    assert_eq!(
        verified.verification_status,
        StorageVerificationStatus::Verified
    );
    assert_eq!(
        destination_store.bytes(&verified.object_id).as_deref(),
        Some(&b"abc"[..])
    );
    assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0);
    assert!(transfer_settings(&catalog).await.is_empty());
}

#[tokio::test]
async fn migrate_without_primary() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![remote_driver()]);
    let destination = remote_storage(&storages, StorageRole::Unassigned, "new-folder").await;

    assert!(matches!(
        storages
            .migrate_primary(destination.id, MigrationOptions::default())
            .await,
        Err(StorageError::NoPrimary)
    ));
    assert_eq!(active_primary_id(&catalog).await, None);
}

#[tokio::test]
async fn migrate_to_assigned_storage_rejected() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let (primary, _store, _original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let backup = remote_storage(&storages, StorageRole::Backup, "backup-folder").await;
    let missing = Uuid::new_v4();

    for target in [backup.id, primary.id] {
        assert!(matches!(
            storages
                .migrate_primary(target, MigrationOptions::default())
                .await,
            Err(StorageError::InvalidTarget(_))
        ));
    }
    assert!(matches!(
        storages
            .migrate_primary(missing, MigrationOptions::default())
            .await,
        Err(StorageError::StorageNotFound(id)) if id == missing
    ));
    assert_eq!(active_primary_id(&catalog).await, Some(primary.id));
    assert_eq!(role_of(&catalog, backup.id).await, StorageRole::Backup);
    assert!(driver.store(backup.id).unwrap().uploads().is_empty());
}

#[tokio::test]
async fn incomplete_migration_does_not_switch() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let (primary, store, _original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let lost = track_with_original(&catalog, source.id, b"lost").await;
    verify_on(&catalog, &store.insert("lost.flac", b"lost"), &lost).await;
    // The primary still records the original as verified, but its bytes are
    // gone, so the destination cannot be given a copy.
    store.remove("lost.flac");
    let destination = remote_storage(&storages, StorageRole::Unassigned, "new-folder").await;

    let error = storages
        .migrate_primary(destination.id, MigrationOptions::default())
        .await
        .unwrap_err();

    assert!(
        matches!(error, StorageError::MigrationIncomplete(1)),
        "{error}"
    );
    assert!(error.to_string().contains("1 asset(s) remain unverified"));
    assert_eq!(active_primary_id(&catalog).await, Some(primary.id));
    assert_eq!(role_of(&catalog, primary.id).await, StorageRole::Primary);
    assert_eq!(
        role_of(&catalog, destination.id).await,
        StorageRole::Unassigned
    );
    // The original that could be copied was, so a retry only needs the rest.
    assert_eq!(driver.store(destination.id).unwrap().uploads().len(), 1);
}

#[tokio::test]
async fn old_primary_kept_as_backup() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let handoff = Arc::new(RecordingHandoff::default());
    let staging = tempfile::tempdir().unwrap();
    let storages = storages_with(
        &catalog,
        vec![driver.clone()],
        staging.path(),
        handoff.clone(),
    );
    let (primary, store, original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let destination = remote_storage(&storages, StorageRole::Unassigned, "new-folder").await;
    let destination_store = driver.store(destination.id).unwrap();
    // The destination already holds the bytes under another name; the
    // migration's own inventory proves it.
    destination_store.insert("Elsewhere/abc.flac", b"abc");

    let outcome = storages
        .migrate_primary(
            destination.id,
            MigrationOptions {
                keep_old_as_backup: true,
                verify_checksums: true,
            },
        )
        .await
        .unwrap();

    assert_eq!((outcome.copied_assets, outcome.already_present), (0, 1));
    assert!(destination_store.uploads().is_empty());
    assert!(store.downloads().is_empty());
    assert_eq!(active_primary_id(&catalog).await, Some(destination.id));
    assert_eq!(role_of(&catalog, primary.id).await, StorageRole::Backup);
    assert_eq!(
        location(&catalog, destination.id, "Elsewhere/abc.flac")
            .await
            .asset_id,
        original.id()
    );
    // Every verified primary asset is handed to replication, which now backs
    // up to the old primary.
    assert_eq!(*handoff.calls.lock().unwrap(), vec![Vec::<Uuid>::new()]);
    let playable = storages.playable(original.track_id()).await.unwrap();
    assert_eq!(playable.storage_id, destination.id);
}

/// A remote driver whose storages all share the grant `grant`.
fn granting_driver(grant: &str) -> Arc<FakeDriver> {
    Arc::new(
        FakeDriver::remote(MusicStorageKind::GoogleDrive, "google_drive:account").with_grant(grant),
    )
}

async fn stored(catalog: &TursoCatalog, storage_id: Uuid) -> Option<MusicStorage> {
    MusicStorageRepository::get(catalog, storage_id)
        .await
        .unwrap()
}

/// Writes a cached file for `storage_id` under `cache_root`.
fn cache_file(cache_root: &Path, storage_id: Uuid) -> std::path::PathBuf {
    let directory = cache_root.join(storage_id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("cached");
    std::fs::write(&file, b"bytes").unwrap();
    file
}

#[tokio::test]
async fn disconnect_is_idempotent_and_keeps_cache_and_catalog() {
    let catalog = temp_catalog().await;
    let cache = tempfile::tempdir().unwrap();
    let driver = granting_driver("grant");
    let storages = storages_caching_in(&catalog, vec![driver.clone()], cache.path());
    let primary = remote_storage(&storages, StorageRole::Primary, "primary-folder").await;
    let backup = remote_storage(&storages, StorageRole::Backup, "backup-folder").await;
    let cached = cache_file(cache.path(), backup.id);

    storages
        .release(backup.id, Release::Disconnect)
        .await
        .unwrap();
    storages
        .release(backup.id, Release::Disconnect)
        .await
        .unwrap();

    let saved = stored(&catalog, backup.id).await.unwrap();
    assert!(!saved.enabled);
    assert_eq!(saved.role, StorageRole::Backup);
    assert_eq!(saved.root_identifier.as_deref(), Some("backup-folder"));
    assert!(cached.exists());
    assert!(!storages.live_stores().contains(backup.id));
    assert!(driver.calls().released_grants.is_empty());

    // The primary can be replaced but never removed.
    let before = stored(&catalog, primary.id).await;
    let error = storages
        .release(primary.id, Release::Disconnect)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::PrimaryInUse { id } if id == primary.id));
    assert_eq!(stored(&catalog, primary.id).await, before);
    assert!(storages.live_stores().contains(primary.id));
    assert_eq!(active_primary_id(&catalog).await, Some(primary.id));
    let missing = Uuid::new_v4();
    assert!(matches!(
        storages.release(missing, Release::Disconnect).await,
        Err(StorageError::StorageNotFound(id)) if id == missing
    ));
}

#[tokio::test]
async fn revoke_disables_all_storages_sharing_grant() {
    let catalog = temp_catalog().await;
    let driver = granting_driver("shared");
    let local = local_driver();
    let storages = storages(&catalog, vec![driver.clone(), local.clone()]);
    let first = remote_storage(&storages, StorageRole::Backup, "first-folder").await;
    let second = remote_storage(&storages, StorageRole::Unassigned, "second-folder").await;
    // Another grant of the same kind, and the same grant name on another kind.
    let mut other_grant = MusicStorage::new(
        "Other account",
        MusicStorageKind::GoogleDrive,
        StorageRole::Backup,
        writable_capabilities(),
    )
    .with_root_identifier("other-folder");
    other_grant.config.insert("grant".into(), "other".into());
    let mut other_kind = MusicStorage::new(
        "Other kind",
        MusicStorageKind::LocalDirectory,
        StorageRole::Backup,
        writable_capabilities(),
    );
    other_kind.config.insert("grant".into(), "shared".into());
    for storage in [&other_grant, &other_kind] {
        MusicStorageRepository::upsert(catalog.as_ref(), storage.clone())
            .await
            .unwrap();
        storages
            .live_stores()
            .register(storage.id, Arc::new(MemoryStore::new(storage.id)));
    }

    // When the grant cannot be released (say, offline), nothing is
    // disconnected and the revoke can be retried.
    driver.fail_release_grant(Some(StorageError::Driver("provider is unreachable".into())));
    let error = storages
        .release(first.id, Release::RevokeGrant)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Driver(_)), "{error}");
    for storage in [&first, &second] {
        assert!(stored(&catalog, storage.id).await.unwrap().enabled);
        assert!(storages.live_stores().contains(storage.id));
    }

    driver.fail_release_grant(None);
    storages
        .release(first.id, Release::RevokeGrant)
        .await
        .unwrap();
    storages
        .release(first.id, Release::RevokeGrant)
        .await
        .unwrap();

    assert_eq!(
        driver.calls().released_grants,
        vec!["shared".to_owned(), "shared".to_owned()]
    );
    for storage in [&first, &second] {
        assert!(!stored(&catalog, storage.id).await.unwrap().enabled);
        assert!(!storages.live_stores().contains(storage.id));
    }
    for storage in [&other_grant, &other_kind] {
        assert!(stored(&catalog, storage.id).await.unwrap().enabled);
        assert!(storages.live_stores().contains(storage.id));
    }
    assert!(local.calls().released_grants.is_empty());

    // A storage without a grant has nothing to revoke.
    let root = tempfile::tempdir().unwrap();
    let ungranted = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Backup,
            &at(root.path()),
        )
        .await
        .unwrap();
    assert!(matches!(
        storages.release(ungranted.id, Release::RevokeGrant).await,
        Err(StorageError::InvalidTarget(_))
    ));
}

#[tokio::test]
async fn revoking_a_grant_the_primary_uses_is_primary_in_use() {
    let catalog = temp_catalog().await;
    let driver = granting_driver("shared");
    let storages = storages(&catalog, vec![driver.clone()]);
    let primary = remote_storage(&storages, StorageRole::Primary, "primary-folder").await;
    let backup = remote_storage(&storages, StorageRole::Backup, "backup-folder").await;
    let before = persisted(&catalog).await;

    for target in [backup.id, primary.id] {
        let error = storages
            .release(target, Release::RevokeGrant)
            .await
            .unwrap_err();
        assert!(matches!(error, StorageError::PrimaryInUse { id } if id == primary.id));
    }

    assert!(driver.calls().released_grants.is_empty());
    assert_eq!(persisted(&catalog).await, before);
    assert!(storages.live_stores().contains(primary.id));
    assert!(storages.live_stores().contains(backup.id));
}

#[tokio::test]
async fn forget_is_idempotent_and_keeps_owned_assets_and_stored_files() {
    let catalog = temp_catalog().await;
    let cache = tempfile::tempdir().unwrap();
    let driver = granting_driver("grant");
    let storages = storages_caching_in(&catalog, vec![driver.clone()], cache.path());
    let library = tempfile::tempdir().unwrap();
    let source = library_source(&catalog, library.path()).await;
    let original = track_with_original(&catalog, source.id, b"abc").await;
    let track = TrackRepository::get(catalog.as_ref(), original.track_id())
        .await
        .unwrap();
    let primary = remote_storage(&storages, StorageRole::Primary, "primary-folder").await;
    let backup = remote_storage(&storages, StorageRole::Backup, "backup-folder").await;
    let store = driver.store(backup.id).unwrap();
    verify_on(&catalog, &store.insert("one.flac", b"abc"), &original).await;
    let cached = cache_file(cache.path(), backup.id);

    storages.release(backup.id, Release::Forget).await.unwrap();
    storages.release(backup.id, Release::Forget).await.unwrap();

    assert_eq!(stored(&catalog, backup.id).await, None);
    assert!(
        AssetRepository::get_location(catalog.as_ref(), backup.id, "one.flac".into())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        AssetRepository::get_storage_object(catalog.as_ref(), backup.id, "one.flac".into())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        TrackRepository::get(catalog.as_ref(), original.track_id())
            .await
            .unwrap(),
        track
    );
    assert_eq!(
        AssetRepository::get_original(catalog.as_ref(), original.id())
            .await
            .unwrap(),
        Some(original)
    );
    assert!(!cached.exists());
    assert!(!storages.live_stores().contains(backup.id));
    assert_eq!(store.bytes("one.flac").as_deref(), Some(&b"abc"[..]));
    assert!(driver.calls().released_grants.is_empty());

    // The primary can be replaced but never removed.
    let primary_cache = cache_file(cache.path(), primary.id);
    let error = storages
        .release(primary.id, Release::Forget)
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::PrimaryInUse { id } if id == primary.id));
    assert!(stored(&catalog, primary.id).await.unwrap().enabled);
    assert!(primary_cache.exists());
    assert_eq!(active_primary_id(&catalog).await, Some(primary.id));

    // Forgetting a storage the catalog no longer has still removes a cache
    // left behind.
    let stray = Uuid::new_v4();
    let stray_cache = cache_file(cache.path(), stray);
    storages.release(stray, Release::Forget).await.unwrap();
    assert!(!stray_cache.exists());
}

#[tokio::test]
async fn update_settings_replaces_name_and_module_settings() {
    let catalog = temp_catalog().await;
    let storages = storages(&catalog, vec![local_driver()]);
    let root = tempfile::tempdir().unwrap();
    let storage = local_primary(&storages, root.path()).await;

    let updated = storages
        .update_settings(
            storage.id,
            StorageSettings {
                name: "  Renamed  ".into(),
                organize_by_artist: true,
                verify_after_copy: false,
            },
        )
        .await
        .unwrap();

    let saved = stored(&catalog, storage.id).await.unwrap();
    assert_eq!(saved, updated);
    assert_eq!(saved.name, "Renamed");
    assert_eq!(
        saved.config.get("organize_by_artist").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        saved.config.get("verify_after_copy").map(String::as_str),
        Some("false")
    );
    assert_eq!(saved.role, StorageRole::Primary);
    assert_eq!(saved.root_identifier, storage.root_identifier);
    assert!(matches!(
        storages
            .update_settings(
                storage.id,
                StorageSettings {
                    name: " ".into(),
                    organize_by_artist: false,
                    verify_after_copy: true,
                },
            )
            .await,
        Err(StorageError::InvalidTarget(_))
    ));
    assert_eq!(stored(&catalog, storage.id).await.unwrap().name, "Renamed");
}

#[tokio::test]
async fn keeping_an_in_place_primary_as_backup_is_refused_before_copying() {
    let catalog = temp_catalog().await;
    let driver = local_driver();
    let storages = storages(&catalog, vec![driver.clone()]);
    let library = tempfile::tempdir().unwrap();
    add_source(&catalog, library.path()).await;
    // Adopted in place: the primary sits exactly at the source root.
    let primary = local_primary(&storages, library.path()).await;
    let destination_root = tempfile::tempdir().unwrap();
    let destination = storages
        .connect(
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            &at(destination_root.path()),
        )
        .await
        .unwrap();
    let destination_store = driver.store(destination.id).unwrap();
    let inventories = destination_store.inventory_requests().len();

    let error = storages
        .migrate_primary(
            destination.id,
            MigrationOptions {
                keep_old_as_backup: true,
                verify_checksums: true,
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, StorageError::RootsOverlap { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("distinct and non-nested"));
    assert_eq!(active_primary_id(&catalog).await, Some(primary.id));
    assert_eq!(role_of(&catalog, primary.id).await, StorageRole::Primary);
    assert_eq!(
        role_of(&catalog, destination.id).await,
        StorageRole::Unassigned
    );
    assert_eq!(destination_store.inventory_requests().len(), inventories);
    assert!(destination_store.uploads().is_empty());

    // Without keeping the old primary as a backup, the migration goes ahead.
    storages
        .migrate_primary(destination.id, MigrationOptions::default())
        .await
        .unwrap();
    assert_eq!(active_primary_id(&catalog).await, Some(destination.id));
    assert_eq!(role_of(&catalog, primary.id).await, StorageRole::Unassigned);
}

#[tokio::test]
async fn retrying_a_failed_migration_reconnects_the_same_candidate() {
    let catalog = temp_catalog().await;
    let driver = remote_driver();
    let staging = tempfile::tempdir().unwrap();
    let storages = storages_with(
        &catalog,
        vec![driver.clone()],
        staging.path(),
        Arc::new(NoHandoff),
    );
    let (primary, _store, _original) =
        remote_primary_with_track(&catalog, &storages, &driver, b"abc").await;
    let candidate = remote_storage(&storages, StorageRole::Unassigned, "new-folder").await;
    let candidate_store = driver.store(candidate.id).unwrap();
    candidate_store.corrupt_uploads(true);
    assert!(matches!(
        storages
            .migrate_primary(candidate.id, MigrationOptions::default())
            .await,
        Err(StorageError::MigrationIncomplete(1))
    ));
    candidate_store.corrupt_uploads(false);

    // The user retries with the same location (and a new name).
    let again = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            &StorageSetup {
                name: Some("Renamed".into()),
                location: Some("new-folder".into()),
                ..StorageSetup::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(again.id, candidate.id);
    assert_eq!(again.name, "Renamed");
    assert_eq!(again.role, StorageRole::Unassigned);
    assert_eq!(persisted(&catalog).await.len(), 2);
    storages
        .migrate_primary(again.id, MigrationOptions::default())
        .await
        .unwrap();
    assert_eq!(active_primary_id(&catalog).await, Some(candidate.id));

    // Only enabled candidates are picked up again.
    let assigned = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            &StorageSetup {
                location: Some("new-folder".into()),
                ..StorageSetup::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(assigned, StorageError::AlreadyConfigured { id } if id == candidate.id));
    storages
        .release(primary.id, Release::Disconnect)
        .await
        .unwrap();
    let disconnected = storages
        .connect(
            MusicStorageKind::GoogleDrive,
            StorageRole::Unassigned,
            &StorageSetup {
                location: Some("primary-folder".into()),
                ..StorageSetup::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(disconnected, StorageError::AlreadyConfigured { id } if id == primary.id));
    assert_eq!(persisted(&catalog).await.len(), 2);
}
