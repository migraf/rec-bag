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
async fn existing_v3_jobs_are_backfilled_without_losing_duplicate_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("v3.db");
    let database = Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection.execute_batch(
        "CREATE TABLE catalog_schema_version(singleton INTEGER PRIMARY KEY,version INTEGER NOT NULL,updated_at TEXT NOT NULL);\
         INSERT INTO catalog_schema_version VALUES(1,3,'2026-01-01T00:00:00Z');\
         CREATE TABLE sync_runs(id TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);\
         CREATE TABLE sync_jobs(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,status TEXT NOT NULL,next_attempt_at TEXT,lease_expires_at TEXT,created_at TEXT NOT NULL,payload TEXT NOT NULL);",
    ).await.unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, None, None);
    connection
        .execute(
            "INSERT INTO sync_runs VALUES(?1,?2,?3,?4)",
            (
                run.id.to_string(),
                serde_json::to_string(&run).unwrap(),
                run.created_at.to_rfc3339(),
                run.updated_at.to_rfc3339(),
            ),
        )
        .await
        .unwrap();
    for _ in 0..2 {
        let job = SyncJob::with_retry_policy(
            run.id,
            SyncJobPayload::FetchCatalogPage {
                version: SYNC_JOB_PAYLOAD_VERSION,
                source_id: Uuid::new_v4(),
                collection_key: "likes".into(),
                sync_cursor: None,
                page_cursor: None,
            },
            3,
            Some("legacy-duplicate"),
        );
        connection
            .execute(
                "INSERT INTO sync_jobs VALUES(?1,?2,'pending',NULL,NULL,?3,?4)",
                (
                    job.id.to_string(),
                    run.id.to_string(),
                    job.created_at.to_rfc3339(),
                    serde_json::to_string(&job).unwrap(),
                ),
            )
            .await
            .unwrap();
    }
    migrate(&connection, Some(&path)).await.unwrap();
    let mut rows = connection
        .query(
            "SELECT COUNT(*),COUNT(DISTINCT idempotency_key) FROM sync_jobs",
            (),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 2);
    assert_eq!(row.get::<i64>(1).unwrap(), 2);
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
        "CREATE TABLE tracks(id TEXT PRIMARY KEY,path TEXT NOT NULL,title TEXT NOT NULL,artist TEXT,album TEXT,duration_ms INTEGER,bpm REAL,key TEXT,genre TEXT,rating INTEGER,comment TEXT,added_at TEXT NOT NULL,last_played TEXT,play_count INTEGER DEFAULT 0,sample_rate INTEGER,bitrate INTEGER,year INTEGER,track_number INTEGER,waveform_json TEXT);\
         INSERT INTO tracks(id,path,title,added_at) VALUES('not-a-uuid','/music/a.mp3','A','2026-01-01T00:00:00Z');",
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
async fn replacement_import_removes_new_rows_and_restores_authority_and_owned_tables() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, track.clone())
        .await
        .unwrap();
    let primary = MusicStorage::new(
        "primary",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, primary.clone())
        .await
        .unwrap();
    catalog
        .switch_active_primary(None, Some(primary.id))
        .await
        .unwrap();
    let candidate = MatchCandidate::new(track.id, None, primary.id, "object", Vec::new());
    let result = ReconciliationResult::new(ReconciliationOutcome::UntrackedStorageObject)
        .with_track(track.id)
        .with_storage_object(primary.id, "object")
        .with_candidate(candidate.clone());
    catalog.upsert_result(result.clone()).await.unwrap();
    let resolution =
        ReconciliationResolution::new(result.id, ReconciliationDecision::RejectCandidate)
            .for_candidate(candidate.id);
    let suppression = ReconciliationSuppression::from_resolution(&resolution).unwrap();
    catalog
        .record_resolution(resolution.clone(), Some(suppression.clone()))
        .await
        .unwrap();
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
    catalog
        .append_event(rec_sync::JobEvent {
            id: Uuid::new_v4(),
            job_id: Some(job.id),
            run_id: run.id,
            kind: rec_sync::JobEventKind::Planned,
            at: Utc::now(),
            message: None,
            progress: None,
            issue: None,
        })
        .await
        .unwrap();
    let trigger = SnapshotTrigger {
        id: Uuid::new_v4(),
        source_run_id: Some(run.id),
        reason: "recovery".into(),
        requested_at: Utc::now(),
        completed_snapshot_id: None,
    };
    catalog.put_snapshot_trigger(trigger.clone()).await.unwrap();
    let approval = CleanupApproval {
        id: Uuid::new_v4(),
        storage_id: primary.id,
        object_id: "old-version".into(),
        reason: Some("approved".into()),
        approved_at: Utc::now(),
        expires_at: None,
        consumed_at: None,
    };
    catalog
        .put_cleanup_approval(approval.clone())
        .await
        .unwrap();
    let mut library_entry = LibraryEntry::new(track.id);
    library_entry.rating = Some(5);
    library_entry.record_play(Utc::now());
    LibraryEntryRepository::upsert(&catalog, library_entry.clone())
        .await
        .unwrap();
    let analysis = TrackAnalysis {
        waveform: Some(WaveformPeaks {
            peaks: vec![0.25, 0.5],
        }),
        bpm: Some(128.0),
        ..TrackAnalysis::new(track.id)
    };
    TrackAnalysisRepository::upsert(&catalog, analysis.clone())
        .await
        .unwrap();
    SettingsRepository::set(
        &catalog,
        Setting {
            key: "snapshot-owned".into(),
            value: Value::String("restored".into()),
            updated_at: Utc::now(),
        },
    )
    .await
    .unwrap();

    let historical = catalog.export_snapshot().await.unwrap();
    let recovery = catalog.export_snapshot().await.unwrap();
    let post_snapshot_track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, post_snapshot_track.clone())
        .await
        .unwrap();
    let replacement_primary = MusicStorage::new(
        "later-primary",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, replacement_primary.clone())
        .await
        .unwrap();
    catalog
        .switch_active_primary(Some(primary.id), Some(replacement_primary.id))
        .await
        .unwrap();
    SettingsRepository::set(
        &catalog,
        Setting {
            key: "post-snapshot".into(),
            value: Value::Bool(true),
            updated_at: Utc::now(),
        },
    )
    .await
    .unwrap();

    catalog.replace_from_snapshot(&recovery).await.unwrap();
    assert!(TrackRepository::get(&catalog, post_snapshot_track.id)
        .await
        .unwrap()
        .is_none());
    assert!(SettingsRepository::get(&catalog, "post-snapshot".into())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        catalog.get_active_primary().await.unwrap().unwrap().id,
        primary.id
    );
    assert_eq!(catalog.load_job(job.id).await.unwrap(), Some(job));
    assert_eq!(catalog.list_events(run.id).await.unwrap().len(), 1);
    assert_eq!(
        catalog.list_snapshot_triggers().await.unwrap(),
        vec![trigger]
    );
    assert_eq!(
        catalog
            .get_cleanup_approval(primary.id, "old-version")
            .await
            .unwrap(),
        Some(approval)
    );
    assert_eq!(
        LibraryEntryRepository::get(&catalog, track.id)
            .await
            .unwrap(),
        Some(library_entry)
    );
    assert_eq!(
        TrackAnalysisRepository::get(&catalog, track.id)
            .await
            .unwrap(),
        Some(analysis)
    );
    assert_eq!(
        catalog.get_resolution(resolution.id).await.unwrap(),
        Some(resolution)
    );
    assert_eq!(
        catalog
            .list_suppressions(
                ReconciliationResolutionFilter::default(),
                RepositoryPageRequest::default(),
            )
            .await
            .unwrap()
            .items,
        vec![suppression]
    );
    let snapshots = CatalogSnapshotRepository::list(&catalog, RepositoryPageRequest::default())
        .await
        .unwrap();
    assert_eq!(snapshots.items, vec![historical]);
}

#[tokio::test]
async fn replacement_import_late_failure_rolls_back_the_exact_pre_restore_state() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let snapshot_track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, snapshot_track.clone())
        .await
        .unwrap();
    let recovery = catalog.export_snapshot().await.unwrap();

    let existing_track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, existing_track.clone())
        .await
        .unwrap();
    let storage = MusicStorage::new(
        "current-primary",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, storage.clone())
        .await
        .unwrap();
    catalog
        .switch_active_primary(None, Some(storage.id))
        .await
        .unwrap();
    SettingsRepository::set(
        &catalog,
        Setting {
            key: "pre-restore".into(),
            value: Value::String("must-survive".into()),
            updated_at: Utc::now(),
        },
    )
    .await
    .unwrap();
    let snapshot_count_before =
        CatalogSnapshotRepository::list(&catalog, RepositoryPageRequest::default())
            .await
            .unwrap()
            .items
            .len();

    let mut payload: Value = serde_json::from_slice(recovery.payload()).unwrap();
    payload["active_primary_storage_id"] = Value::String(Uuid::new_v4().to_string());
    let invalid = CatalogSnapshot::new(
        SNAPSHOT_FORMAT_VERSION,
        SNAPSHOT_CONTENT_TYPE,
        serde_json::to_vec(&payload).unwrap(),
    );
    assert!(catalog.replace_from_snapshot(&invalid).await.is_err());
    assert_eq!(
        TrackRepository::get(&catalog, existing_track.id)
            .await
            .unwrap(),
        Some(existing_track)
    );
    assert_eq!(
        catalog.get_active_primary().await.unwrap().unwrap().id,
        storage.id
    );
    assert_eq!(
        SettingsRepository::get(&catalog, "pre-restore".into())
            .await
            .unwrap()
            .unwrap()
            .value,
        Value::String("must-survive".into())
    );
    assert_eq!(
        CatalogSnapshotRepository::list(&catalog, RepositoryPageRequest::default())
            .await
            .unwrap()
            .items
            .len(),
        snapshot_count_before
    );
}

#[tokio::test]
async fn interrupted_replace_transaction_rolls_back_and_legacy_documents_still_import() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, track.clone())
        .await
        .unwrap();
    let transaction = catalog.begin_catalog_transaction().await.unwrap();
    crate::repositories::clear_snapshot_owned_tables(transaction.as_ref())
        .await
        .unwrap();
    transaction.rollback().await.unwrap();
    assert_eq!(
        TrackRepository::get(&catalog, track.id).await.unwrap(),
        Some(track.clone())
    );

    let current = catalog.export_snapshot().await.unwrap();
    let mut legacy: Value = serde_json::from_slice(current.payload()).unwrap();
    for field in [
        "active_primary_storage_id",
        "catalog_snapshots",
        "snapshot_triggers",
        "cleanup_approvals",
        "library_entries",
        "track_analysis",
    ] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    let compatible = CatalogSnapshot::new(
        SNAPSHOT_FORMAT_VERSION,
        SNAPSHOT_CONTENT_TYPE,
        serde_json::to_vec(&legacy).unwrap(),
    );
    let restored = TursoCatalog::in_memory().await.unwrap();
    restored.import_snapshot(&compatible).await.unwrap();
    assert_eq!(
        TrackRepository::get(&restored, track.id).await.unwrap(),
        Some(track)
    );
    assert!(restored.get_active_primary().await.unwrap().is_none());
}

#[tokio::test]
async fn snapshot_export_policy_excludes_secrets_capability_urls_and_transient_state() {
    const TOKEN: &str = "token-value-MUST-NOT-EXPORT";
    const CODE: &str = "authorization-code-MUST-NOT-EXPORT";
    const VERIFIER: &str = "pkce-verifier-MUST-NOT-EXPORT";
    const AUTHORIZATION: &str = "Authorization: Bearer header-MUST-NOT-EXPORT";
    const SESSION_URI: &str = "https://upload.example/upload/session/capability-MUST-NOT-EXPORT";
    const LOCAL_PATH: &str = "/tmp/private-staging-MUST-NOT-EXPORT.flac";
    const MESSAGE: &str = "provider said Bearer message-MUST-NOT-EXPORT";

    let catalog = TursoCatalog::in_memory().await.unwrap();
    let mut source = CatalogSource::local_folder("source", "/music");
    source.config.insert("access_token".into(), TOKEN.into());
    source
        .config
        .insert("diagnostic_header".into(), AUTHORIZATION.into());
    SourceRepository::upsert(&catalog, source.clone())
        .await
        .unwrap();

    let mut storage = MusicStorage::new(
        "storage",
        MusicStorageKind::Other("test".into()),
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    storage
        .config
        .insert("session_uri".into(), SESSION_URI.into());
    storage
        .config
        .insert("safe_region".into(), "eu-central".into());
    MusicStorageRepository::upsert(&catalog, storage.clone())
        .await
        .unwrap();

    for (key, value) in [
        ("oauth.token", Value::String(TOKEN.into())),
        ("oauth.code", Value::String(CODE.into())),
        ("pkce.verifier", Value::String(VERIFIER.into())),
        ("http.Authorization", Value::String(AUTHORIZATION.into())),
        (
            "google_drive.upload.storage.asset.operation",
            serde_json::json!({ "session_uri": SESSION_URI }),
        ),
        (
            "backup.retention_review.v1",
            serde_json::json!({ "status": "safe-retention" }),
        ),
    ] {
        catalog
            .set(Setting {
                key: key.into(),
                value,
                updated_at: Utc::now(),
            })
            .await
            .unwrap();
    }

    let mut run = SyncRun::new(SyncRunKind::CatalogSync, Some(source.id), None);
    run.checkpoint = Some(SyncCheckpoint::new(0).with_last_item(LOCAL_PATH));
    run.error = Some(SyncError::new(
        SyncErrorKind::Provider,
        "provider_error",
        MESSAGE,
        true,
    ));
    run.issues.push(SyncIssue::new(
        SyncIssueSeverity::Warning,
        SyncErrorKind::Provider,
        "provider_warning",
        AUTHORIZATION,
        true,
    ));
    let local_job = SyncJob::new(
        run.id,
        SyncJobPayload::UploadAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id: Uuid::new_v4(),
            source_path: LOCAL_PATH.into(),
            destination_storage_id: storage.id,
            destination_object_key: "music/file.flac".into(),
        },
    );
    let mut safe_job = SyncJob::new(
        run.id,
        SyncJobPayload::FetchCatalogPage {
            version: SYNC_JOB_PAYLOAD_VERSION,
            source_id: source.id,
            collection_key: "likes".into(),
            sync_cursor: None,
            page_cursor: None,
        },
    );
    safe_job.checkpoint = Some(SyncCheckpoint::new(0).with_last_item(LOCAL_PATH));
    safe_job.error = Some(SyncError::new(
        SyncErrorKind::Authentication,
        "auth_failed",
        MESSAGE,
        false,
    ));
    safe_job.issues.push(SyncIssue::new(
        SyncIssueSeverity::Error,
        SyncErrorKind::Authentication,
        "auth_issue",
        AUTHORIZATION,
        false,
    ));
    catalog
        .create_run_with_jobs(run.clone(), vec![local_job.clone(), safe_job.clone()])
        .await
        .unwrap();
    catalog
        .append_event(rec_sync::JobEvent {
            id: Uuid::new_v4(),
            job_id: Some(safe_job.id),
            run_id: run.id,
            kind: rec_sync::JobEventKind::Failed,
            at: Utc::now(),
            message: Some(MESSAGE.into()),
            progress: None,
            issue: Some(SyncIssue::new(
                SyncIssueSeverity::Error,
                SyncErrorKind::Authorization,
                "authorization",
                AUTHORIZATION,
                false,
            )),
        })
        .await
        .unwrap();

    let snapshot = catalog.export_snapshot().await.unwrap();
    let payload = String::from_utf8(snapshot.payload().to_vec()).unwrap();
    for forbidden in [
        TOKEN,
        CODE,
        VERIFIER,
        AUTHORIZATION,
        SESSION_URI,
        LOCAL_PATH,
        MESSAGE,
    ] {
        assert!(!payload.contains(forbidden), "snapshot leaked {forbidden}");
    }
    assert!(payload.contains("safe-retention"));
    assert!(payload.contains("eu-central"));

    let restored = TursoCatalog::in_memory().await.unwrap();
    restored.import_snapshot(&snapshot).await.unwrap();
    assert!(SettingsRepository::get(&restored, "oauth.token".into())
        .await
        .unwrap()
        .is_none());
    assert!(SettingsRepository::get(
        &restored,
        "google_drive.upload.storage.asset.operation".into(),
    )
    .await
    .unwrap()
    .is_none());
    assert!(
        SettingsRepository::get(&restored, "backup.retention_review.v1".into())
            .await
            .unwrap()
            .is_some()
    );
    assert!(restored.load_job(local_job.id).await.unwrap().is_none());
    let restored_job = restored.load_job(safe_job.id).await.unwrap().unwrap();
    assert!(restored_job.checkpoint.is_none());
    assert_eq!(
        restored_job.error.unwrap().message,
        crate::repositories::SNAPSHOT_REDACTED_MESSAGE
    );
    assert_eq!(restored.list_events(run.id).await.unwrap().len(), 1);
    let restored_source = SourceRepository::get(&restored, source.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!restored_source.config.contains_key("access_token"));
    let restored_storage = MusicStorageRepository::get(&restored, storage.id)
        .await
        .unwrap()
        .unwrap();
    assert!(!restored_storage.config.contains_key("session_uri"));
}

#[tokio::test]
async fn setting_delete_supports_transient_lifecycle_cleanup() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    catalog
        .set(Setting {
            key: "google_drive.upload.completed".into(),
            value: serde_json::json!({ "state": "done" }),
            updated_at: Utc::now(),
        })
        .await
        .unwrap();
    assert!(catalog
        .delete_setting("google_drive.upload.completed")
        .await
        .unwrap());
    assert!(!catalog
        .delete_setting("google_drive.upload.completed")
        .await
        .unwrap());
    assert!(
        SettingsRepository::get(&catalog, "google_drive.upload.completed".into())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn source_provider_deletion_preserves_canonical_tracks_and_owned_assets() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let source = CatalogSource::local_folder("source", "/music");
    SourceRepository::upsert(&catalog, source.clone())
        .await
        .unwrap();
    let retained_source = CatalogSource::local_folder("other source", "/other");
    SourceRepository::upsert(&catalog, retained_source.clone())
        .await
        .unwrap();
    let mut track = CanonicalTrack::new();
    track.metadata.add(MetadataFieldValue::new(
        MetadataField::Title,
        MetadataValue::Text("provider title".into()),
        MetadataProvenance::provider(source.id, "external", Utc::now()),
    ));
    track.metadata.add(MetadataFieldValue::new(
        MetadataField::Artist,
        MetadataValue::Text("user artist".into()),
        MetadataProvenance::new(MetadataOrigin::User, Utc::now()),
    ));
    track
        .occurrences
        .push(SourceTrackIdentity::new(source.id, "external"));
    TrackRepository::upsert(&catalog, track.clone())
        .await
        .unwrap();
    let mut occurrence = SourceTrackOccurrence::new(source.id, "external");
    occurrence.canonical_track_id = Some(track.id);
    catalog.upsert_occurrences(vec![occurrence]).await.unwrap();
    let asset = OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "owned.flac");
    catalog.upsert_original(asset.clone()).await.unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, Some(source.id), None);
    SyncRunRepository::upsert(&catalog, run.clone())
        .await
        .unwrap();
    catalog.export_snapshot().await.unwrap();

    catalog
        .delete_source_provider_data(source.id)
        .await
        .unwrap();
    assert!(SourceRepository::get(&catalog, source.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        SourceRepository::get(&catalog, retained_source.id)
            .await
            .unwrap(),
        Some(retained_source)
    );
    assert!(catalog
        .get_occurrence(source.id, "external".into())
        .await
        .unwrap()
        .is_none());
    assert!(SyncRunRepository::get(&catalog, run.id)
        .await
        .unwrap()
        .is_none());
    let retained = TrackRepository::get(&catalog, track.id)
        .await
        .unwrap()
        .unwrap();
    assert!(retained.occurrences.is_empty());
    assert_eq!(retained.metadata.values.len(), 1);
    assert_eq!(
        retained.metadata.values[0].provenance.origin,
        MetadataOrigin::User
    );
    assert_eq!(catalog.get_original(asset.id()).await.unwrap(), Some(asset));
    assert!(
        CatalogSnapshotRepository::list(&catalog, RepositoryPageRequest::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn storage_provider_deletion_preserves_owned_assets_and_canonical_tracks() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let storage = MusicStorage::new(
        "storage",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, storage.clone())
        .await
        .unwrap();
    let retained_storage = MusicStorage::new(
        "other storage",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, retained_storage.clone())
        .await
        .unwrap();
    catalog
        .switch_active_primary(None, Some(storage.id))
        .await
        .unwrap();
    let mut track = CanonicalTrack::new();
    let asset = OriginalAsset::new_unverified(track.id, OriginalAssetRole::Master, "owned.flac");
    track.metadata.add(MetadataFieldValue::new(
        MetadataField::Title,
        MetadataValue::Text("embedded".into()),
        MetadataProvenance::embedded_tag(asset.id(), Some(storage.id), Utc::now()),
    ));
    TrackRepository::upsert(&catalog, track.clone())
        .await
        .unwrap();
    catalog.upsert_original(asset.clone()).await.unwrap();
    let object = StorageObject {
        storage_id: storage.id,
        object_id: "provider-object".into(),
        name: "owned.flac".into(),
        display_path: Some("/provider/owned.flac".into()),
        size_bytes: 10,
        media_type: Some("audio/flac".into()),
        modified_at: None,
        provider_version: Some("v1".into()),
        hashes: Vec::new(),
        embedded_asset_id: Some(asset.id()),
        is_trashed: false,
    };
    catalog.upsert_storage_object(object.clone()).await.unwrap();
    let location = AssetLocation {
        id: Uuid::new_v4(),
        asset_id: asset.id(),
        asset_type: AssetType::Original,
        storage_id: storage.id,
        object_id: object.object_id.clone(),
        verification_status: StorageVerificationStatus::Verified,
        expected_hash: None,
        observed_hash: None,
        provider_version: object.provider_version.clone(),
        last_checked_at: Some(Utc::now()),
        verified_at: Some(Utc::now()),
        error_message: None,
    };
    catalog.upsert_location(location).await.unwrap();
    let run = SyncRun::new(SyncRunKind::StorageInventory, None, Some(storage.id));
    SyncRunRepository::upsert(&catalog, run.clone())
        .await
        .unwrap();
    SettingsRepository::set(
        &catalog,
        Setting {
            key: format!("google_drive.upload.{}.checkpoint", storage.id),
            value: Value::Bool(true),
            updated_at: Utc::now(),
        },
    )
    .await
    .unwrap();
    catalog.export_snapshot().await.unwrap();

    catalog
        .delete_storage_provider_data(storage.id)
        .await
        .unwrap();
    assert!(MusicStorageRepository::get(&catalog, storage.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        MusicStorageRepository::get(&catalog, retained_storage.id)
            .await
            .unwrap(),
        Some(retained_storage)
    );
    assert!(catalog.get_active_primary().await.unwrap().is_none());
    assert!(catalog
        .get_storage_object(storage.id, object.object_id)
        .await
        .unwrap()
        .is_none());
    assert!(catalog
        .list_locations_for_asset(asset.id())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(catalog.get_original(asset.id()).await.unwrap(), Some(asset));
    let retained = TrackRepository::get(&catalog, track.id)
        .await
        .unwrap()
        .unwrap();
    assert!(retained.metadata.values.is_empty());
    assert!(SyncRunRepository::get(&catalog, run.id)
        .await
        .unwrap()
        .is_none());
    assert!(
        CatalogSnapshotRepository::list(&catalog, RepositoryPageRequest::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn legacy_playlist_analysis_sources_paths_and_ids_are_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("library.db");
    let media_path = directory.path().join("track.mp3");
    fs::write(&media_path, b"unchanged audio").unwrap();
    let track_id = Uuid::new_v4();
    let duplicate_path_track_id = Uuid::new_v4();
    let playlist_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let database = Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
    let connection = database.connect().unwrap();
    connection.execute_batch(
        "CREATE TABLE tracks(id TEXT PRIMARY KEY,path TEXT NOT NULL,title TEXT NOT NULL,artist TEXT,album TEXT,duration_ms INTEGER,bpm REAL,key TEXT,genre TEXT,rating INTEGER,comment TEXT,added_at TEXT NOT NULL,last_played TEXT,play_count INTEGER DEFAULT 0,sample_rate INTEGER,bitrate INTEGER,year INTEGER,track_number INTEGER,waveform_json TEXT);\
         CREATE TABLE playlists(id TEXT PRIMARY KEY,name TEXT NOT NULL,description TEXT,created_at TEXT NOT NULL,modified_at TEXT NOT NULL);\
         CREATE TABLE playlist_tracks(playlist_id TEXT NOT NULL,track_id TEXT NOT NULL,position INTEGER NOT NULL,PRIMARY KEY(playlist_id,track_id));\
         CREATE TABLE sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,source_type TEXT NOT NULL,uri TEXT NOT NULL,config_json TEXT,track_count INTEGER,enabled INTEGER,created_at TEXT NOT NULL,last_scanned TEXT);\
         CREATE TABLE source_tracks(source_id TEXT NOT NULL,track_id TEXT NOT NULL,PRIMARY KEY(source_id,track_id));\
         CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
    ).await.unwrap();
    connection
        .execute(
            "INSERT INTO tracks VALUES(?1,?2,'Legacy title','Legacy artist','Legacy album',213000,128.5,'Am','House',4,'Legacy note','2026-01-01T00:00:00Z','2026-02-01T00:00:00Z',7,44100,320,2019,3,'{\"peaks\":[0.1,0.2]}')",
            (
                track_id.to_string(),
                media_path.to_string_lossy().to_string(),
            ),
        )
        .await
        .unwrap();
    connection
        .execute(
            "INSERT INTO tracks VALUES(?1,?2,'Duplicate legacy path',NULL,NULL,NULL,NULL,NULL,NULL,NULL,NULL,'2026-01-01T00:00:00Z',NULL,0,NULL,NULL,NULL,NULL,NULL)",
            (
                duplicate_path_track_id.to_string(),
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
    let duplicate_assets = catalog
        .list_originals_for_track(duplicate_path_track_id)
        .await
        .unwrap();
    assert_eq!(duplicate_assets.len(), 1);
    let duplicate_locations = catalog
        .list_locations_for_asset(duplicate_assets[0].id())
        .await
        .unwrap();
    assert_eq!(locations.len() + duplicate_locations.len(), 1);
    let location = locations
        .first()
        .or_else(|| duplicate_locations.first())
        .expect("one duplicate-path asset must retain the physical-file location");
    assert_eq!(location.object_id, media_path.to_string_lossy());
    assert!(catalog.get_active_primary().await.unwrap().is_none());
    assert!(MusicStorageRepository::list(&catalog)
        .await
        .unwrap()
        .iter()
        .all(|storage| storage.role == StorageRole::Unassigned));
    assert_eq!(
        preferred_text(&track, MetadataField::Artist).as_deref(),
        Some("Legacy artist")
    );
    assert_eq!(
        preferred_text(&track, MetadataField::Album).as_deref(),
        Some("Legacy album")
    );
    assert_eq!(
        preferred_text(&track, MetadataField::Genre).as_deref(),
        Some("House")
    );
    assert_eq!(
        preferred_text(&track, MetadataField::MusicalKey).as_deref(),
        Some("Am")
    );

    let entry = LibraryEntryRepository::get(&catalog, track_id)
        .await
        .unwrap()
        .expect("a rated, commented, played legacy track keeps its library entry");
    assert_eq!(entry.rating, Some(4));
    assert_eq!(entry.comment.as_deref(), Some("Legacy note"));
    assert_eq!(entry.play_count, 7);
    assert!(entry.last_played_at.is_some());
    // A track the user never rated, commented on, or played gains no entry row.
    assert!(
        LibraryEntryRepository::get(&catalog, duplicate_path_track_id)
            .await
            .unwrap()
            .is_none()
    );

    let analysis = TrackAnalysisRepository::get(&catalog, track_id)
        .await
        .unwrap()
        .expect("a legacy waveform survives as track analysis");
    assert_eq!(
        analysis.waveform.map(|waveform| waveform.peaks),
        Some(vec![0.1, 0.2])
    );
    assert!(!table_exists(&catalog, "legacy_track_analysis").await);
}

/// The preferred value of one metadata field, for assertions about migrated tags.
fn preferred_text(track: &CanonicalTrack, field: MetadataField) -> Option<String> {
    match &track.metadata.preferred(&field)?.value {
        MetadataValue::Text(value) => Some(value.clone()),
        _ => None,
    }
}

async fn table_exists(catalog: &TursoCatalog, name: &str) -> bool {
    catalog
        .connection()
        .query(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .is_some()
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

#[tokio::test]
async fn evolved_list_filters_page_without_loading_callerside_catalogs() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let source = CatalogSource::local_folder("source", "/music");
    SourceRepository::upsert(&catalog, source.clone())
        .await
        .unwrap();
    let mut account = SourceAccount::new(source.id, "owner");
    account.display_name = Some("Owner".into());
    catalog.upsert_account(account).await.unwrap();
    let other_account = SourceAccount::new(source.id, "other");
    catalog.upsert_account(other_account).await.unwrap();

    let mut occurrence = SourceTrackOccurrence::new(source.id, "track");
    occurrence.account_external_id = Some("owner".into());
    catalog
        .upsert_occurrences(vec![occurrence.clone()])
        .await
        .unwrap();
    let mut playlist = ExternalPlaylist::new(source.id, "playlist", "Set");
    playlist.account_external_id = Some("owner".into());
    catalog.upsert_external(playlist.clone()).await.unwrap();

    let occurrences = catalog
        .list_occurrences(
            SourceOccurrenceFilter {
                source_id: Some(source.id),
                account_external_id: Some("owner".into()),
                ..Default::default()
            },
            RepositoryPageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(occurrences.items, vec![occurrence]);
    let playlists = catalog
        .list_external(
            ExternalPlaylistFilter {
                source_id: Some(source.id),
                owner_external_id: Some("owner".into()),
            },
            RepositoryPageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(playlists.items, vec![playlist]);

    let run = SyncRun::new(SyncRunKind::CatalogSync, Some(source.id), None);
    SyncRunRepository::upsert(&catalog, run.clone())
        .await
        .unwrap();
    assert_eq!(
        SyncRunRepository::latest(
            &catalog,
            SyncRunFilter {
                kind: Some(SyncRunKind::CatalogSync),
                source_id: Some(source.id),
                ..Default::default()
            },
        )
        .await
        .unwrap(),
        Some(run)
    );
}

#[tokio::test]
async fn candidate_lookup_resolutions_and_suppressions_are_durable() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let track = CanonicalTrack::new();
    TrackRepository::upsert(&catalog, track.clone())
        .await
        .unwrap();
    let storage = MusicStorage::new(
        "primary",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, storage.clone())
        .await
        .unwrap();
    let candidate = MatchCandidate::new(track.id, None, storage.id, "object", Vec::new());
    let result = ReconciliationResult::new(ReconciliationOutcome::UntrackedStorageObject)
        .with_track(track.id)
        .with_storage_object(storage.id, "object")
        .with_candidate(candidate.clone());
    catalog.upsert_result(result.clone()).await.unwrap();
    assert_eq!(
        catalog.get_result_by_candidate(candidate.id).await.unwrap(),
        Some(result.clone())
    );

    let resolution =
        ReconciliationResolution::new(result.id, ReconciliationDecision::RejectCandidate)
            .for_candidate(candidate.id);
    let suppression = ReconciliationSuppression::from_resolution(&resolution).unwrap();
    catalog
        .record_resolution(resolution.clone(), Some(suppression.clone()))
        .await
        .unwrap();
    assert_eq!(
        catalog.get_resolution(resolution.id).await.unwrap(),
        Some(resolution.clone())
    );
    assert_eq!(
        catalog
            .list_suppressions(
                ReconciliationResolutionFilter {
                    candidate_id: Some(candidate.id),
                    ..Default::default()
                },
                RepositoryPageRequest::default(),
            )
            .await
            .unwrap()
            .items,
        vec![suppression.clone()]
    );
    catalog.remove_suppression(suppression.id).await.unwrap();
    assert!(catalog
        .list_suppressions(
            ReconciliationResolutionFilter::default(),
            RepositoryPageRequest::default(),
        )
        .await
        .unwrap()
        .items
        .is_empty());
}

#[tokio::test]
async fn full_source_page_is_atomic_and_advances_cursors() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let source = CatalogSource::local_folder("source", "/music");
    SourceRepository::upsert(&catalog, source.clone())
        .await
        .unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, Some(source.id), None);
    let page_cursor = SyncCursor::new("page-2");
    let sync_cursor = SyncCursor::new("sync-1");
    let page = SourceSyncPage {
        items: vec![
            SourceSyncItem::Account(SourceAccount::new(source.id, "owner")),
            SourceSyncItem::Track(SourceTrackOccurrence::new(source.id, "track")),
            SourceSyncItem::Playlist(ExternalPlaylist::new(source.id, "playlist", "Set")),
        ],
        removals: Vec::new(),
        next_page_cursor: Some(page_cursor.clone()),
        next_sync_cursor: Some(sync_cursor.clone()),
        progress: Some(SyncProgress::default()),
    };
    let bad_auth = RefreshTokenMetadata {
        source_id: source.id,
        credential_reference: "Bearer secret".into(),
        refresh_generation: 1,
        last_refreshed_at: None,
        expires_at: None,
        updated_at: Utc::now(),
    };
    assert!(catalog
        .apply_full_source_sync_page(source.id, page.clone(), run.clone(), Some(bad_auth))
        .await
        .is_err());
    assert!(catalog
        .get_account(source.id, "owner".into())
        .await
        .unwrap()
        .is_none());

    catalog
        .apply_full_source_sync_page(source.id, page, run.clone(), None)
        .await
        .unwrap();
    let persisted = SyncRunRepository::get(&catalog, run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.page_cursor, Some(page_cursor));
    assert_eq!(persisted.sync_cursor, Some(sync_cursor));
    assert!(catalog
        .get_occurrence(source.id, "track".into())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn active_idempotency_is_unique_and_terminal_jobs_release_the_key() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, None, None);
    let payload = SyncJobPayload::FetchCatalogPage {
        version: SYNC_JOB_PAYLOAD_VERSION,
        source_id: Uuid::new_v4(),
        collection_key: "likes".into(),
        sync_cursor: None,
        page_cursor: None,
    };
    let mut first = SyncJob::with_retry_policy(run.id, payload.clone(), 3, Some("same-key"));
    catalog
        .create_run_with_jobs(run.clone(), vec![first.clone()])
        .await
        .unwrap();
    let second = SyncJob::with_retry_policy(run.id, payload, 3, Some("same-key"));
    assert!(catalog.upsert_job(second.clone()).await.is_err());
    assert_eq!(
        catalog
            .find_active_job_by_idempotency_key("same-key")
            .await
            .unwrap()
            .unwrap()
            .id,
        first.id
    );
    first.status = SyncStatus::Succeeded;
    first.finished_at = Some(Utc::now());
    catalog.upsert_job(first).await.unwrap();
    catalog.upsert_job(second.clone()).await.unwrap();
    assert_eq!(
        catalog
            .find_active_job_by_idempotency_key("same-key")
            .await
            .unwrap()
            .unwrap()
            .id,
        second.id
    );
}

#[tokio::test]
async fn transfer_result_terminal_job_and_event_commit_together() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let run = SyncRun::new(SyncRunKind::AssetTransfer, None, None);
    let mut job = SyncJob::new(
        run.id,
        SyncJobPayload::UploadAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id: Uuid::new_v4(),
            source_path: "source.flac".into(),
            destination_storage_id: Uuid::new_v4(),
            destination_object_key: "dest.flac".into(),
        },
    );
    catalog
        .create_run_with_jobs(run.clone(), vec![job.clone()])
        .await
        .unwrap();
    job.status = SyncStatus::Succeeded;
    job.finished_at = Some(Utc::now());
    let transfer = DurableTransferResult {
        idempotency_key: job.idempotency_key.clone(),
        job_id: job.id,
        asset_id: match &job.payload {
            SyncJobPayload::UploadAsset { asset_id, .. } => *asset_id,
            _ => unreachable!(),
        },
        storage_id: None,
        object_id: Some("dest.flac".into()),
        bytes_transferred: 42,
        completed_at: Utc::now(),
        metadata: Value::Null,
    };
    let event = rec_sync::JobEvent {
        id: Uuid::new_v4(),
        job_id: Some(job.id),
        run_id: run.id,
        kind: rec_sync::JobEventKind::Succeeded,
        at: Utc::now(),
        message: None,
        progress: Some(job.progress),
        issue: None,
    };
    catalog
        .save_transfer_result_and_terminal_job(transfer.clone(), job.clone(), event)
        .await
        .unwrap();
    assert_eq!(
        catalog
            .get_transfer_result(&job.idempotency_key)
            .await
            .unwrap(),
        Some(transfer)
    );
    assert_eq!(catalog.load_job(job.id).await.unwrap(), Some(job));
    assert_eq!(catalog.list_events(run.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn cancellation_requests_and_terminal_events_are_semantically_idempotent() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, None, None);
    catalog
        .create_run_with_jobs(run.clone(), Vec::new())
        .await
        .unwrap();
    let first = catalog
        .request_cancellation(run.id, Utc::now())
        .await
        .unwrap();
    let second = catalog
        .request_cancellation(run.id, Utc::now())
        .await
        .unwrap();
    assert!(first.newly_requested);
    assert!(!second.newly_requested);
    for _ in 0..2 {
        catalog
            .append_event(rec_sync::JobEvent {
                id: Uuid::new_v4(),
                job_id: None,
                run_id: run.id,
                kind: rec_sync::JobEventKind::CancellationRequested,
                at: Utc::now(),
                message: None,
                progress: None,
                issue: None,
            })
            .await
            .unwrap();
    }
    assert_eq!(catalog.list_events(run.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn logical_snapshot_fingerprint_excludes_timestamps_and_job_churn() {
    let first = TursoCatalog::in_memory().await.unwrap();
    let second = TursoCatalog::in_memory().await.unwrap();
    let mut track_a = CanonicalTrack::new();
    let mut track_b = track_a.clone();
    track_a.updated_at = track_a.updated_at + chrono::Duration::hours(1);
    track_b.updated_at = track_b.updated_at + chrono::Duration::hours(2);
    TrackRepository::upsert(&first, track_a).await.unwrap();
    TrackRepository::upsert(&second, track_b).await.unwrap();
    let run = SyncRun::new(SyncRunKind::CatalogSync, None, None);
    first.create_run_with_jobs(run, Vec::new()).await.unwrap();
    let snapshot_a = first.export_snapshot().await.unwrap();
    let snapshot_b = second.export_snapshot().await.unwrap();
    assert_eq!(
        first
            .logical_snapshot_fingerprint(snapshot_a.id)
            .await
            .unwrap(),
        second
            .logical_snapshot_fingerprint(snapshot_b.id)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn snapshot_triggers_and_cleanup_approvals_are_persisted() {
    let catalog = TursoCatalog::in_memory().await.unwrap();
    let storage = MusicStorage::new(
        "storage",
        MusicStorageKind::LocalDirectory,
        StorageRole::Unassigned,
        StorageCapabilities::default(),
    );
    MusicStorageRepository::upsert(&catalog, storage.clone())
        .await
        .unwrap();
    let trigger = SnapshotTrigger {
        id: Uuid::new_v4(),
        source_run_id: None,
        reason: "before cleanup".into(),
        requested_at: Utc::now(),
        completed_snapshot_id: None,
    };
    catalog.put_snapshot_trigger(trigger.clone()).await.unwrap();
    assert_eq!(
        catalog.list_snapshot_triggers().await.unwrap(),
        vec![trigger]
    );
    let approval = CleanupApproval {
        id: Uuid::new_v4(),
        storage_id: storage.id,
        object_id: "old-version".into(),
        reason: Some("retention review".into()),
        approved_at: Utc::now(),
        expires_at: None,
        consumed_at: None,
    };
    catalog
        .put_cleanup_approval(approval.clone())
        .await
        .unwrap();
    assert_eq!(
        catalog
            .get_cleanup_approval(storage.id, "old-version")
            .await
            .unwrap(),
        Some(approval)
    );
}
