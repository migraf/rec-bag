use super::*;
use chrono::Utc;
use rec_core::*;
use rec_sync::JobStore;
use serde_json::Value;
use std::fs;
use turso::Builder;
use uuid::Uuid;

#[tokio::test]
async fn empty_and_current_schema_migrations_are_idempotent() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let first = migrate(catalog.connection(), None).await.unwrap();
    let second = migrate(catalog.connection(), None).await.unwrap();
    assert_eq!(first.from_version, CURRENT_CATALOG_SCHEMA_VERSION.get());
    assert_eq!(second.from_version, CURRENT_CATALOG_SCHEMA_VERSION.get());

    let mut rows = catalog
        .connection()
        .query("PRAGMA foreign_keys", ())
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        1
    );
}

#[tokio::test]
async fn interrupted_legacy_migration_rolls_back_and_can_be_retried() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    let database = Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection.execute_batch(
        "CREATE TABLE tracks(id TEXT PRIMARY KEY,path TEXT NOT NULL,title TEXT NOT NULL,added_at TEXT NOT NULL,waveform_json TEXT);\
         INSERT INTO tracks VALUES('not-a-uuid','/music/a.mp3','A','2026-01-01T00:00:00Z',NULL);",
    ).await.unwrap();
    drop(connection);
    drop(database);

    assert!(TursoCatalog::open(&path).await.is_err());
    let database = Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    let mut rows = connection
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='tracks'",
            (),
        )
        .await
        .unwrap();
    assert!(
        rows.next().await.unwrap().is_some(),
        "rename must roll back"
    );
    let valid_id = Uuid::new_v4();
    connection
        .execute(
            "UPDATE tracks SET id=?1 WHERE id='not-a-uuid'",
            [valid_id.to_string()],
        )
        .await
        .unwrap();
    drop(connection);
    drop(database);

    let catalog = TursoCatalog::open(&path).await.unwrap();
    assert_eq!(
        TrackRepository::get(&catalog, valid_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        valid_id
    );
}

#[tokio::test]
async fn foreign_keys_and_multistatement_upserts_are_atomic() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let missing = Uuid::new_v4();
    let result = catalog.connection().execute(
        "INSERT INTO canonical_playlist_memberships(playlist_id,position,track_id) VALUES(?1,0,?2)",
        (Uuid::new_v4().to_string(), missing.to_string()),
    ).await;
    assert!(result.is_err());

    let track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, track.clone())
        .await
        .unwrap();
    let mut playlist = CanonicalPlaylist::new("original");
    playlist.replace_tracks([track.id]);
    PlaylistRepository::upsert(&catalog, playlist.clone())
        .await
        .unwrap();

    let mut invalid = playlist.clone();
    invalid.name = "must roll back".into();
    invalid.entries.push(CanonicalPlaylistEntry {
        position: 0,
        track_id: track.id,
    });
    assert!(PlaylistRepository::upsert(&catalog, invalid).await.is_err());
    assert_eq!(
        PlaylistRepository::get(&catalog, playlist.id)
            .await
            .unwrap()
            .unwrap(),
        playlist
    );
}

#[tokio::test]
async fn duplicate_source_occurrence_is_prevented_and_updated() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let source = CatalogSource::local_folder("folder", "/music");
    SourceRepository::upsert(&catalog, source.clone())
        .await
        .unwrap();
    let mut occurrence = SourceTrackOccurrence::new(source.id, "provider-track");
    occurrence.external_url = Some("old".into());
    catalog
        .upsert_occurrences(vec![occurrence.clone()])
        .await
        .unwrap();
    occurrence.external_url = Some("new".into());
    catalog
        .upsert_occurrences(vec![occurrence.clone(), occurrence.clone()])
        .await
        .unwrap();

    let mut rows = catalog
        .connection()
        .query(
            "SELECT COUNT(*) FROM source_track_occurrences WHERE source_id=?1 AND external_id=?2",
            (
                source.id.to_string(),
                occurrence.identity.external_id.clone(),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        1
    );
    assert_eq!(
        catalog
            .get_occurrence(source.id, occurrence.identity.external_id.clone())
            .await
            .unwrap()
            .unwrap()
            .external_url
            .as_deref(),
        Some("new")
    );
}

#[tokio::test]
async fn source_sync_and_refresh_metadata_commit_or_rollback_together() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let source = CatalogSource::local_folder("folder", "/music");
    SourceRepository::upsert(&catalog, source.clone())
        .await
        .unwrap();
    let occurrence = SourceTrackOccurrence::new(source.id, "one");
    let run = SyncRun::new(SyncRunKind::CatalogSync, Some(source.id), None);
    let bad = RefreshTokenMetadata {
        source_id: source.id,
        credential_reference: "Bearer actual-secret".into(),
        refresh_generation: 1,
        last_refreshed_at: Some(Utc::now()),
        expires_at: None,
        updated_at: Utc::now(),
    };
    assert!(catalog
        .apply_source_sync_page(vec![occurrence.clone()], run.clone(), Some(bad))
        .await
        .is_err());
    assert!(catalog
        .get_occurrence(source.id, "one".into())
        .await
        .unwrap()
        .is_none());
    assert!(SyncRunRepository::get(&catalog, run.id)
        .await
        .unwrap()
        .is_none());

    let metadata = RefreshTokenMetadata {
        source_id: source.id,
        credential_reference: "keyring:soundcloud/account-1".into(),
        refresh_generation: 2,
        last_refreshed_at: Some(Utc::now()),
        expires_at: Some(Utc::now()),
        updated_at: Utc::now(),
    };
    catalog
        .apply_source_sync_page(vec![occurrence], run.clone(), Some(metadata.clone()))
        .await
        .unwrap();
    assert!(catalog
        .get_occurrence(source.id, "one".into())
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        catalog.get_refresh_token_metadata(source.id).await.unwrap(),
        Some(metadata)
    );
}

#[tokio::test]
async fn snapshot_round_trip_and_integrity_rejection() {
    let source_catalog = TursoCatalog::in_memory().await.unwrap();
    let track = CanonicalTrack::new();
    TrackRepository::upsert(&source_catalog, track.clone())
        .await
        .unwrap();
    let mut playlist = CanonicalPlaylist::new("set");
    playlist.replace_tracks([track.id, track.id]);
    PlaylistRepository::upsert(&source_catalog, playlist.clone())
        .await
        .unwrap();
    let source = CatalogSource::local_folder("crate", "/crate");
    SourceRepository::upsert(&source_catalog, source.clone())
        .await
        .unwrap();
    let mut occurrence = SourceTrackOccurrence::new(source.id, "external");
    occurrence.canonical_track_id = Some(track.id);
    source_catalog
        .upsert_occurrences(vec![occurrence.clone()])
        .await
        .unwrap();
    let snapshot = source_catalog.export_snapshot().await.unwrap();
    source_catalog.put(snapshot.clone()).await.unwrap();

    let restored = TursoCatalog::in_memory().await.unwrap();
    restored.import_snapshot(&snapshot).await.unwrap();
    assert_eq!(
        TrackRepository::get(&restored, track.id).await.unwrap(),
        Some(track)
    );
    assert_eq!(
        PlaylistRepository::get(&restored, playlist.id)
            .await
            .unwrap(),
        Some(playlist)
    );
    assert_eq!(
        restored
            .get_occurrence(source.id, "external".into())
            .await
            .unwrap(),
        Some(occurrence)
    );

    let mut serialized = serde_json::to_value(&snapshot).unwrap();
    serialized["payload"] = Value::Array(vec![Value::from(0)]);
    let tampered: CatalogSnapshot = serde_json::from_value(serialized).unwrap();
    assert!(!tampered.verify_integrity());
    assert!(restored.import_snapshot(&tampered).await.is_err());
    assert!(restored.put(tampered).await.is_err());
}

#[tokio::test]
async fn legacy_playlist_analysis_sources_paths_and_ids_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    let media_path = directory.path().join("track.mp3");
    fs::write(&media_path, b"unchanged audio").unwrap();
    let track_id = Uuid::new_v4();
    let playlist_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let database = Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection.execute_batch(
        "CREATE TABLE tracks(id TEXT PRIMARY KEY,path TEXT NOT NULL,title TEXT NOT NULL,added_at TEXT NOT NULL,waveform_json TEXT);\
         CREATE TABLE playlists(id TEXT PRIMARY KEY,name TEXT NOT NULL,description TEXT,created_at TEXT NOT NULL,modified_at TEXT NOT NULL);\
         CREATE TABLE playlist_tracks(playlist_id TEXT NOT NULL,track_id TEXT NOT NULL,position INTEGER NOT NULL,PRIMARY KEY(playlist_id,track_id));\
         CREATE TABLE sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,source_type TEXT NOT NULL,uri TEXT NOT NULL,config_json TEXT,track_count INTEGER,enabled INTEGER,created_at TEXT NOT NULL,last_scanned TEXT);\
         CREATE TABLE source_tracks(source_id TEXT NOT NULL,track_id TEXT NOT NULL,PRIMARY KEY(source_id,track_id));\
         CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
    ).await.unwrap();
    connection
        .execute(
            "INSERT INTO tracks VALUES(?1,?2,'Legacy title','2026-01-01T00:00:00Z','[0.1,0.2]')",
            (
                track_id.to_string(),
                media_path.to_string_lossy().to_string(),
            ),
        )
        .await
        .unwrap();
    connection.execute("INSERT INTO playlists VALUES(?1,'Legacy playlist','kept','2026-01-01T00:00:00Z','2026-01-02T00:00:00Z')",[playlist_id.to_string()]).await.unwrap();
    connection
        .execute(
            "INSERT INTO playlist_tracks VALUES(?1,?2,0)",
            (playlist_id.to_string(), track_id.to_string()),
        )
        .await
        .unwrap();
    connection.execute("INSERT INTO sources VALUES(?1,'Legacy folder','local',?2,'{\"recursive\":true,\"refresh_token\":\"must-not-copy\"}',1,1,'2026-01-01T00:00:00Z','2026-01-03T00:00:00Z')",(source_id.to_string(),directory.path().to_string_lossy().to_string())).await.unwrap();
    connection
        .execute(
            "INSERT INTO source_tracks VALUES(?1,?2)",
            (source_id.to_string(), track_id.to_string()),
        )
        .await
        .unwrap();
    drop(connection);
    drop(database);

    let catalog = TursoCatalog::open(&path).await.unwrap();
    assert!(path.with_extension("db.pre-provider-v1.bak").exists());
    assert_eq!(fs::read(&media_path).unwrap(), b"unchanged audio");
    let track = TrackRepository::get(&catalog, track_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(track.id, track_id);
    let playlist = PlaylistRepository::get(&catalog, playlist_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        playlist.entries,
        vec![CanonicalPlaylistEntry {
            position: 0,
            track_id
        }]
    );
    let source = SourceRepository::get(&catalog, source_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.id, source_id);
    assert!(!source.config.keys().any(|key| key.contains("token")));
    assert_eq!(
        catalog
            .get_occurrence(source_id, track_id.to_string())
            .await
            .unwrap()
            .unwrap()
            .canonical_track_id,
        Some(track_id)
    );
    let assets = catalog.list_originals_for_track(track_id).await.unwrap();
    assert_eq!(assets.len(), 1);
    assert!(!assets[0].is_preferred);
    let locations = catalog
        .list_locations_for_asset(assets[0].id())
        .await
        .unwrap();
    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].object_id, media_path.to_string_lossy());
    assert!(catalog.get_active_primary().await.unwrap().is_none());
    assert!(MusicStorageRepository::list(&catalog)
        .await
        .unwrap()
        .iter()
        .all(|storage| storage.role == StorageRole::Unassigned));
    let mut rows = catalog
        .connection()
        .query(
            "SELECT waveform_json FROM legacy_track_analysis WHERE track_id=?1",
            [track_id.to_string()],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.next()
            .await
            .unwrap()
            .unwrap()
            .get::<String>(0)
            .unwrap(),
        "[0.1,0.2]"
    );
}

#[tokio::test]
async fn job_store_plan_and_claim_are_durable() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, None, None);
    let job = SyncJob::new(
        run.id,
        SyncJobPayload::FetchCatalogPage {
            version: SYNC_JOB_PAYLOAD_VERSION,
            source_id: Uuid::new_v4(),
            collection_key: "likes".into(),
            sync_cursor: None,
            page_cursor: None,
        },
    );
    catalog
        .create_run_with_jobs(run.clone(), vec![job.clone()])
        .await
        .unwrap();
    let claimed = catalog
        .claim_due(
            "worker".into(),
            Utc::now(),
            Utc::now() + chrono::Duration::minutes(1),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, SyncStatus::Running);
    assert!(catalog.load_run(run.id).await.unwrap().is_some());
}
