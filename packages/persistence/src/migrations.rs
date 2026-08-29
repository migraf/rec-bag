use chrono::{DateTime, Utc};
use rec_core::{
    AssetLocation, AssetType, CanonicalMetadata, CanonicalPlaylist, CanonicalPlaylistEntry,
    CanonicalTrack, CatalogSource, CatalogSourceKind, LibraryEntry, MetadataField,
    MetadataFieldValue, MetadataOrigin, MetadataProvenance, MetadataValue, MusicStorage,
    MusicStorageKind, OriginalAsset, OriginalAssetRole, Setting, SourceAvailability,
    SourceCapabilities, SourceTrackOccurrence, StorageCapabilities, StorageObject, StorageRole,
    StorageVerificationStatus, TrackAnalysis, WaveformPeaks, CURRENT_CATALOG_SCHEMA_VERSION,
    LEGACY_CATALOG_SCHEMA_VERSION,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;
use turso::Connection;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub from_version: u32,
    pub to_version: u32,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("database I/O failed: {0}")]
    Database(#[from] turso::Error),
    #[error("failed to back up legacy database: {0}")]
    Backup(#[from] std::io::Error),
    #[error("catalog schema version {0} is newer than this application supports")]
    FutureSchema(u32),
    #[error("legacy catalog contains malformed data: {0}")]
    LegacyData(String),
}

pub async fn migrate(
    connection: &Connection,
    database_path: Option<&Path>,
) -> Result<MigrationOutcome, MigrationError> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .await?;
    let mut journal_rows = connection.query("PRAGMA journal_mode = WAL", ()).await?;
    while journal_rows.next().await?.is_some() {}
    let from_version = detected_version(connection).await?;
    let target = CURRENT_CATALOG_SCHEMA_VERSION.get();
    if from_version > target {
        return Err(MigrationError::FutureSchema(from_version));
    }

    if from_version == target {
        // Idempotently install indexes/tables added while Stage 2A was under development.
        install_current_schema(connection).await?;
        return Ok(MigrationOutcome {
            from_version,
            to_version: target,
            backup_path: None,
        });
    }

    let backup_path = if from_version == LEGACY_CATALOG_SCHEMA_VERSION.get() {
        match database_path {
            Some(path) => Some(create_wal_safe_backup(connection, path).await?),
            None => None,
        }
    } else {
        None
    };
    connection.execute_batch("BEGIN IMMEDIATE;").await?;
    let result = async {
        match from_version {
            0 | 2 => install_current_schema(connection).await?,
            1 => migrate_legacy_schema(connection).await?,
            _ => unreachable!(),
        }
        connection.execute_batch("COMMIT;").await?;
        Ok::<(), MigrationError>(())
    }
    .await;
    if let Err(error) = result {
        let _ = connection.execute_batch("ROLLBACK;").await;
        return Err(error);
    }
    Ok(MigrationOutcome {
        from_version,
        to_version: target,
        backup_path,
    })
}

async fn detected_version(connection: &Connection) -> Result<u32, MigrationError> {
    if table_exists(connection, "catalog_schema_version").await? {
        let mut rows = connection
            .query(
                "SELECT version FROM catalog_schema_version WHERE singleton = 1",
                (),
            )
            .await?;
        let row = rows.next().await?.ok_or_else(|| {
            MigrationError::LegacyData("schema-version table has no singleton row".into())
        })?;
        let version: i64 = row.get(0)?;
        return u32::try_from(version)
            .map_err(|_| MigrationError::LegacyData("invalid schema version".into()));
    }
    if table_exists(connection, "tracks").await? {
        Ok(LEGACY_CATALOG_SCHEMA_VERSION.get())
    } else {
        Ok(0)
    }
}

async fn create_wal_safe_backup(
    connection: &Connection,
    path: &Path,
) -> Result<PathBuf, MigrationError> {
    // Checkpointing first makes the main database a complete SQLite image even when the
    // application previously used WAL. Opening/migration owns this connection, so no writer
    // can race through this catalog object while the copy is made.
    let mut checkpoint_rows = connection
        .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await?;
    while checkpoint_rows.next().await?.is_some() {}
    let backup = path.with_extension("db.pre-provider-v1.bak");
    if !backup.exists() {
        fs::copy(path, &backup)?;
    }
    Ok(backup)
}

async fn table_exists(connection: &Connection, name: &str) -> Result<bool, MigrationError> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

fn parse_uuid(value: &str, kind: &str) -> Result<Uuid, MigrationError> {
    Uuid::parse_str(value).map_err(|error| {
        MigrationError::LegacyData(format!("invalid {kind} UUID {value:?}: {error}"))
    })
}

fn parse_time(value: &str, kind: &str) -> Result<DateTime<Utc>, MigrationError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| MigrationError::LegacyData(format!("invalid {kind} timestamp: {error}")))
}

fn json<T: Serialize>(value: &T) -> Result<String, MigrationError> {
    serde_json::to_string(value)
        .map_err(|error| MigrationError::LegacyData(format!("serialization failed: {error}")))
}

fn stable_uuid(namespace: Uuid, value: &str) -> Uuid {
    Uuid::new_v5(&namespace, value.as_bytes())
}

/// The legacy track columns this migration reads, and their positions in that order.
///
/// Turso rows are read by position, so the projection and its indexes are declared
/// together: reordering one without the other would silently mis-migrate every track.
const LEGACY_TRACK_COLUMNS: &str = "id, path, title, artist, album, genre, key, bpm, \
     duration_ms, year, track_number, rating, comment, play_count, last_played, added_at, \
     waveform_json";
const ID: usize = 0;
const PATH: usize = 1;
const TITLE: usize = 2;
const ARTIST: usize = 3;
const ALBUM: usize = 4;
const GENRE: usize = 5;
const KEY: usize = 6;
const BPM: usize = 7;
const DURATION_MS: usize = 8;
const YEAR: usize = 9;
const TRACK_NUMBER: usize = 10;
const RATING: usize = 11;
const COMMENT: usize = 12;
const PLAY_COUNT: usize = 13;
const LAST_PLAYED: usize = 14;
const ADDED_AT: usize = 15;
const WAVEFORM_JSON: usize = 16;

fn legacy_text(row: &turso::Row, index: usize) -> Result<Option<MetadataValue>, MigrationError> {
    Ok(row
        .get::<Option<String>>(index)?
        .filter(|value| !value.trim().is_empty())
        .map(MetadataValue::Text))
}

fn legacy_integer(row: &turso::Row, index: usize) -> Result<Option<MetadataValue>, MigrationError> {
    Ok(row.get::<Option<i64>>(index)?.map(MetadataValue::Integer))
}

async fn migrate_legacy_schema(connection: &Connection) -> Result<(), MigrationError> {
    for (old, new) in [
        ("tracks", "legacy_v1_tracks"),
        ("playlists", "legacy_v1_playlists"),
        ("playlist_tracks", "legacy_v1_playlist_tracks"),
        ("sources", "legacy_v1_sources"),
        ("source_tracks", "legacy_v1_source_tracks"),
        ("settings", "legacy_v1_settings"),
    ] {
        if table_exists(connection, old).await? {
            connection
                .execute_batch(format!("ALTER TABLE {old} RENAME TO {new};"))
                .await?;
        }
    }
    install_current_schema(connection).await?;

    let mut tracks = BTreeMap::<Uuid, (CanonicalTrack, String)>::new();
    if table_exists(connection, "legacy_v1_tracks").await? {
        let mut migrated = Vec::new();
        let mut rows = connection
            .query(
                &format!("SELECT {LEGACY_TRACK_COLUMNS} FROM legacy_v1_tracks ORDER BY id"),
                (),
            )
            .await?;
        while let Some(row) = rows.next().await? {
            let raw_id: String = row.get(ID)?;
            let id = parse_uuid(&raw_id, "track")?;
            let path: String = row.get(PATH)?;
            let added_at = parse_time(&row.get::<String>(ADDED_AT)?, "track")?;

            // Every legacy tag was authored or read locally, so it migrates as one User
            // candidate per field. Nothing here was reported by a provider.
            let mut metadata = CanonicalMetadata::new();
            let provenance = MetadataProvenance::new(MetadataOrigin::User, added_at);
            let mut record_user_value = |field: MetadataField, value: Option<MetadataValue>| {
                if let Some(value) = value {
                    metadata.add(MetadataFieldValue::new(field, value, provenance.clone()));
                }
            };
            record_user_value(
                MetadataField::Title,
                Some(MetadataValue::Text(row.get::<String>(TITLE)?)),
            );
            record_user_value(MetadataField::Artist, legacy_text(&row, ARTIST)?);
            record_user_value(MetadataField::Album, legacy_text(&row, ALBUM)?);
            record_user_value(MetadataField::Genre, legacy_text(&row, GENRE)?);
            record_user_value(MetadataField::MusicalKey, legacy_text(&row, KEY)?);
            record_user_value(
                MetadataField::Bpm,
                row.get::<Option<f64>>(BPM)?
                    .map(|value| MetadataValue::Decimal(value.to_string())),
            );
            record_user_value(
                MetadataField::DurationMs,
                legacy_integer(&row, DURATION_MS)?,
            );
            record_user_value(MetadataField::ReleaseYear, legacy_integer(&row, YEAR)?);
            record_user_value(
                MetadataField::TrackNumber,
                legacy_integer(&row, TRACK_NUMBER)?,
            );

            let track = CanonicalTrack {
                id,
                metadata,
                occurrences: Vec::new(),
                created_at: added_at,
                updated_at: added_at,
            };

            // The user's own record is the only thing the path-keyed track still owned.
            let mut entry = LibraryEntry::added(id, added_at);
            entry.rating = row
                .get::<Option<i64>>(RATING)?
                .and_then(|value| u8::try_from(value).ok());
            entry.comment = row.get::<Option<String>>(COMMENT)?;
            entry.play_count = row
                .get::<Option<i64>>(PLAY_COUNT)?
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(0);
            entry.last_played_at = row
                .get::<Option<String>>(LAST_PLAYED)?
                .map(|value| parse_time(&value, "track play"))
                .transpose()?;

            let analysis = row
                .get::<Option<String>>(WAVEFORM_JSON)?
                .map(|waveform| track_analysis(id, &waveform))
                .transpose()?;

            migrated.push((raw_id, path, track, entry, analysis));
        }
        drop(rows);

        for (raw_id, path, track, entry, analysis) in migrated {
            connection.execute(
                "INSERT INTO tracks(id,payload,created_at,updated_at,legacy_path) VALUES(?1,?2,?3,?4,?5)",
                (raw_id.clone(), json(&track)?, track.created_at.to_rfc3339(), track.updated_at.to_rfc3339(), path.clone()),
            ).await?;
            for (ordinal, value) in track.metadata.values.iter().enumerate() {
                connection.execute(
                    "INSERT INTO track_metadata_values(track_id,ordinal,payload) VALUES(?1,?2,?3)",
                    (raw_id.clone(), ordinal as i64, json(value)?),
                ).await?;
            }
            if !entry.is_empty() {
                connection
                    .execute(
                        "INSERT INTO library_entries(track_id,payload,updated_at) VALUES(?1,?2,?3)",
                        (raw_id.clone(), json(&entry)?, entry.updated_at.to_rfc3339()),
                    )
                    .await?;
            }
            if let Some(analysis) = analysis {
                connection
                    .execute(
                        "INSERT INTO track_analysis(track_id,payload) VALUES(?1,?2)",
                        (raw_id.clone(), json(&analysis)?),
                    )
                    .await?;
            }
            tracks.insert(track.id, (track, path));
        }
    }

    if table_exists(connection, "legacy_v1_playlists").await? {
        let mut rows = connection.query("SELECT id,name,description,created_at,modified_at FROM legacy_v1_playlists ORDER BY id", ()).await?;
        while let Some(row) = rows.next().await? {
            let raw_id: String = row.get(0)?;
            let id = parse_uuid(&raw_id, "playlist")?;
            let mut entry_rows = connection.query("SELECT track_id,position FROM legacy_v1_playlist_tracks WHERE playlist_id=?1 ORDER BY position", [raw_id.clone()]).await?;
            let mut entries = Vec::new();
            while let Some(entry) = entry_rows.next().await? {
                entries.push(CanonicalPlaylistEntry {
                    track_id: parse_uuid(&entry.get::<String>(0)?, "playlist track")?,
                    position: u32::try_from(entry.get::<i64>(1)?).map_err(|_| {
                        MigrationError::LegacyData("negative playlist position".into())
                    })?,
                });
            }
            let created_at = parse_time(&row.get::<String>(3)?, "playlist")?;
            let updated_at = parse_time(&row.get::<String>(4)?, "playlist")?;
            let playlist = CanonicalPlaylist {
                id,
                name: row.get(1)?,
                description: row.get(2)?,
                entries: entries.clone(),
                created_at,
                updated_at,
            };
            connection.execute("INSERT INTO canonical_playlists(id,payload,created_at,updated_at) VALUES(?1,?2,?3,?4)", (raw_id.clone(), json(&playlist)?, created_at.to_rfc3339(), updated_at.to_rfc3339())).await?;
            for entry in entries {
                connection.execute("INSERT INTO canonical_playlist_memberships(playlist_id,position,track_id) VALUES(?1,?2,?3)", (raw_id.clone(), entry.position as i64, entry.track_id.to_string())).await?;
            }
        }
    }
    let mut source_ids = Vec::new();
    if table_exists(connection, "legacy_v1_sources").await? {
        let mut rows = connection.query("SELECT id,name,source_type,uri,config_json,enabled,created_at,last_scanned FROM legacy_v1_sources ORDER BY id", ()).await?;
        while let Some(row) = rows.next().await? {
            let raw_id: String = row.get(0)?;
            let id = parse_uuid(&raw_id, "source")?;
            let source_type: String = row.get(2)?;
            let uri: String = row.get(3)?;
            let created_at = parse_time(&row.get::<String>(6)?, "source")?;
            let last_scanned = row
                .get::<Option<String>>(7)?
                .map(|value| parse_time(&value, "source scan"))
                .transpose()?;
            let mut config = BTreeMap::from([("path".to_string(), uri.clone())]);
            if let Some(raw_config) = row.get::<Option<String>>(4)? {
                if let Ok(Value::Object(values)) = serde_json::from_str::<Value>(&raw_config) {
                    for (key, value) in values {
                        let lower = key.to_ascii_lowercase();
                        if !["token", "secret", "password", "credential", "authorization"]
                            .iter()
                            .any(|word| lower.contains(word))
                        {
                            config.insert(
                                key,
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| value.to_string()),
                            );
                        }
                    }
                }
            }
            let source = CatalogSource {
                id,
                name: row.get(1)?,
                kind: if source_type == "local" {
                    CatalogSourceKind::LocalFolder
                } else {
                    CatalogSourceKind::Custom(source_type)
                },
                enabled: row.get::<i64>(5)? != 0,
                created_at,
                last_synced_at: last_scanned,
                config,
                capabilities: SourceCapabilities::local_folder(),
                copy_policy: Default::default(),
            };
            connection.execute("INSERT INTO catalog_sources(id,payload,created_at,updated_at,legacy_uri) VALUES(?1,?2,?3,?4,?5)", (raw_id, json(&source)?, created_at.to_rfc3339(), last_scanned.unwrap_or(created_at).to_rfc3339(), uri)).await?;
            source_ids.push(id);
        }
    }

    if table_exists(connection, "legacy_v1_source_tracks").await? {
        let mut rows = connection.query("SELECT source_id,track_id FROM legacy_v1_source_tracks ORDER BY source_id,track_id", ()).await?;
        while let Some(row) = rows.next().await? {
            let source_id = parse_uuid(&row.get::<String>(0)?, "source link")?;
            let track_id = parse_uuid(&row.get::<String>(1)?, "source track link")?;
            let observed_at = tracks
                .get(&track_id)
                .ok_or_else(|| {
                    MigrationError::LegacyData(format!(
                        "source link references missing track {track_id}"
                    ))
                })?
                .0
                .updated_at;
            let mut occurrence = SourceTrackOccurrence::new(source_id, track_id.to_string());
            occurrence.canonical_track_id = Some(track_id);
            occurrence.availability = SourceAvailability::Obtainable;
            occurrence.last_seen_at = observed_at;
            connection.execute("INSERT INTO source_track_occurrences(source_id,external_id,canonical_track_id,payload) VALUES(?1,?2,?3,?4)", (source_id.to_string(), track_id.to_string(), track_id.to_string(), json(&occurrence)?)).await?;
        }
    }

    // Every legacy path becomes an unverified original and an unassigned local location.
    // No path is opened and no storage is selected as primary.
    let storage_id = stable_uuid(Uuid::NAMESPACE_OID, "rec_bag:legacy-local-storage");

    if !tracks.is_empty() {
        let now = Utc::now();
        let storage = MusicStorage::new(
            "Migrated local files",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            StorageCapabilities::default(),
        );
        let storage = MusicStorage {
            id: storage_id,
            created_at: now,
            updated_at: now,
            ..storage
        };
        connection
            .execute(
                "INSERT INTO music_storages(id,payload,created_at,updated_at) VALUES(?1,?2,?3,?4)",
                (
                    storage_id.to_string(),
                    json(&storage)?,
                    now.to_rfc3339(),
                    now.to_rfc3339(),
                ),
            )
            .await?;
        let mut located_paths = BTreeSet::new();
        for (track_id, (track, path)) in &tracks {
            let file_name = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path)
                .to_owned();
            let mut asset = OriginalAsset::new_unverified(
                *track_id,
                OriginalAssetRole::Master,
                file_name.clone(),
            );
            asset.record.id = stable_uuid(*track_id, "legacy-original-asset");
            asset.lineage_id = stable_uuid(*track_id, "legacy-original-lineage");
            asset.record.created_at = track.created_at;
            asset.is_preferred = false;
            connection
                .execute(
                    "INSERT INTO assets(id,track_id,asset_type,payload,created_at) VALUES(?1,?2,'original',?3,?4)",
                    (asset.id().to_string(), track_id.to_string(), json(&asset.record)?, track.created_at.to_rfc3339()),
                )
                .await?;
            connection
                .execute(
                    "INSERT INTO original_assets(id,payload) VALUES(?1,?2)",
                    (asset.id().to_string(), json(&asset)?),
                )
                .await?;

            // A legacy catalog could contain multiple track records for one local path. The
            // current schema correctly permits only one storage object/location per path. Keep
            // every track and original-asset record, but associate the physical file with its
            // first migrated asset instead of inventing a distinct path for duplicates.
            if !located_paths.insert(path.clone()) {
                continue;
            }
            let object = StorageObject {
                storage_id,
                object_id: path.clone(),
                name: file_name,
                display_path: Some(path.clone()),
                size_bytes: 0,
                media_type: None,
                modified_at: None,
                provider_version: None,
                hashes: Vec::new(),
                embedded_asset_id: Some(asset.id()),
                is_trashed: false,
            };
            connection
                .execute(
                    "INSERT INTO storage_objects(storage_id,object_id,generation_id,payload,is_current) VALUES(?1,?2,NULL,?3,1)",
                    (storage_id.to_string(), path.clone(), json(&object)?),
                )
                .await?;
            let location = AssetLocation {
                id: stable_uuid(asset.id(), path),
                asset_id: asset.id(),
                asset_type: AssetType::Original,
                storage_id,
                object_id: path.clone(),
                verification_status: StorageVerificationStatus::PossibleMatch,
                expected_hash: None,
                observed_hash: None,
                provider_version: None,
                last_checked_at: None,
                verified_at: None,
                error_message: None,
            };
            connection
                .execute(
                    "INSERT INTO asset_locations(id,asset_id,storage_id,object_id,payload) VALUES(?1,?2,?3,?4,?5)",
                    (
                        location.id.to_string(),
                        location.asset_id.to_string(),
                        storage_id.to_string(),
                        path.clone(),
                        json(&location)?,
                    ),
                )
                .await?;
        }
    }

    if table_exists(connection, "legacy_v1_settings").await? {
        let mut rows = connection
            .query("SELECT key,value FROM legacy_v1_settings ORDER BY key", ())
            .await?;
        while let Some(row) = rows.next().await? {
            let key: String = row.get(0)?;
            let raw: String = row.get(1)?;
            let setting = Setting {
                key: key.clone(),
                value: serde_json::from_str(&raw).unwrap_or(Value::String(raw)),
                updated_at: Utc::now(),
            };
            connection
                .execute(
                    "INSERT INTO settings(key,payload,updated_at) VALUES(?1,?2,?3)",
                    (key, json(&setting)?, setting.updated_at.to_rfc3339()),
                )
                .await?;
        }
    }
    Ok(())
}

async fn install_current_schema(connection: &Connection) -> Result<(), MigrationError> {
    connection.execute_batch(r#"
CREATE TABLE IF NOT EXISTS catalog_schema_version(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL CHECK(version>0),updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS catalog_migration_reports(id TEXT PRIMARY KEY,report_json TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tracks(id TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,legacy_path TEXT);
CREATE TABLE IF NOT EXISTS track_metadata_values(track_id TEXT NOT NULL,ordinal INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(track_id,ordinal),FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS canonical_playlists(id TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS canonical_playlist_memberships(playlist_id TEXT NOT NULL,position INTEGER NOT NULL CHECK(position>=0),track_id TEXT NOT NULL,PRIMARY KEY(playlist_id,position),FOREIGN KEY(playlist_id) REFERENCES canonical_playlists(id) ON DELETE CASCADE,FOREIGN KEY(track_id) REFERENCES tracks(id));
CREATE TABLE IF NOT EXISTS catalog_sources(id TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,legacy_uri TEXT);
CREATE TABLE IF NOT EXISTS source_accounts(source_id TEXT NOT NULL,external_id TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(source_id,external_id),FOREIGN KEY(source_id) REFERENCES catalog_sources(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS source_auth_metadata(source_id TEXT PRIMARY KEY,credential_reference TEXT NOT NULL,refresh_generation INTEGER NOT NULL DEFAULT 0,last_refreshed_at TEXT,expires_at TEXT,updated_at TEXT NOT NULL,FOREIGN KEY(source_id) REFERENCES catalog_sources(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS source_collection_relationships(identity_key TEXT PRIMARY KEY,source_id TEXT NOT NULL,account_external_id TEXT NOT NULL,collection TEXT NOT NULL,entity_kind TEXT NOT NULL,entity_external_id TEXT NOT NULL,parent_external_id TEXT NOT NULL DEFAULT '',position INTEGER NOT NULL DEFAULT -1,payload TEXT NOT NULL,UNIQUE(source_id,account_external_id,collection,entity_kind,entity_external_id,parent_external_id,position),FOREIGN KEY(source_id,account_external_id) REFERENCES source_accounts(source_id,external_id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS source_track_occurrences(source_id TEXT NOT NULL,external_id TEXT NOT NULL,canonical_track_id TEXT,payload TEXT NOT NULL,PRIMARY KEY(source_id,external_id),FOREIGN KEY(source_id) REFERENCES catalog_sources(id) ON DELETE CASCADE,FOREIGN KEY(canonical_track_id) REFERENCES tracks(id));
CREATE TABLE IF NOT EXISTS external_playlists(source_id TEXT NOT NULL,external_id TEXT NOT NULL,payload TEXT NOT NULL,snapshot TEXT,PRIMARY KEY(source_id,external_id),FOREIGN KEY(source_id) REFERENCES catalog_sources(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS external_playlist_memberships(source_id TEXT NOT NULL,playlist_external_id TEXT NOT NULL,position INTEGER NOT NULL CHECK(position>=0),payload TEXT NOT NULL,PRIMARY KEY(source_id,playlist_external_id,position),FOREIGN KEY(source_id,playlist_external_id) REFERENCES external_playlists(source_id,external_id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS external_playlist_entry_staging(run_id TEXT NOT NULL,source_id TEXT NOT NULL,playlist_external_id TEXT NOT NULL,snapshot_key TEXT NOT NULL DEFAULT '',position INTEGER NOT NULL CHECK(position>=0),payload TEXT NOT NULL,PRIMARY KEY(run_id,source_id,playlist_external_id,snapshot_key,position));
CREATE TABLE IF NOT EXISTS music_storages(id TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS profile_storage_settings(singleton INTEGER PRIMARY KEY CHECK(singleton=1),active_primary_storage_id TEXT,FOREIGN KEY(active_primary_storage_id) REFERENCES music_storages(id));
CREATE TABLE IF NOT EXISTS assets(id TEXT PRIMARY KEY,track_id TEXT NOT NULL,asset_type TEXT NOT NULL CHECK(asset_type IN ('original','derived')),payload TEXT NOT NULL,created_at TEXT NOT NULL,FOREIGN KEY(track_id) REFERENCES tracks(id));
CREATE TABLE IF NOT EXISTS original_assets(id TEXT PRIMARY KEY,payload TEXT NOT NULL,FOREIGN KEY(id) REFERENCES assets(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS derived_assets(id TEXT PRIMARY KEY,payload TEXT NOT NULL,source_asset_id TEXT NOT NULL,FOREIGN KEY(id) REFERENCES assets(id) ON DELETE CASCADE,FOREIGN KEY(source_asset_id) REFERENCES original_assets(id));
CREATE TABLE IF NOT EXISTS sync_runs(id TEXT PRIMARY KEY,payload TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sync_jobs(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,kind TEXT NOT NULL DEFAULT '',status TEXT NOT NULL,next_attempt_at TEXT,lease_expires_at TEXT,created_at TEXT NOT NULL,idempotency_key TEXT NOT NULL,payload TEXT NOT NULL,FOREIGN KEY(run_id) REFERENCES sync_runs(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS sync_issues(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,job_id TEXT,payload TEXT NOT NULL,FOREIGN KEY(run_id) REFERENCES sync_runs(id) ON DELETE CASCADE,FOREIGN KEY(job_id) REFERENCES sync_jobs(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS sync_job_events(id TEXT PRIMARY KEY,run_id TEXT NOT NULL,job_id TEXT,kind TEXT NOT NULL DEFAULT '',semantic_key TEXT,payload TEXT NOT NULL,at TEXT NOT NULL,FOREIGN KEY(run_id) REFERENCES sync_runs(id) ON DELETE CASCADE,FOREIGN KEY(job_id) REFERENCES sync_jobs(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS inventory_generations(id TEXT PRIMARY KEY,storage_id TEXT NOT NULL,run_id TEXT NOT NULL,kind TEXT NOT NULL,status TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,payload TEXT NOT NULL,FOREIGN KEY(storage_id) REFERENCES music_storages(id),FOREIGN KEY(run_id) REFERENCES sync_runs(id));
CREATE TABLE IF NOT EXISTS storage_objects(storage_id TEXT NOT NULL,object_id TEXT NOT NULL,generation_id TEXT,payload TEXT NOT NULL,is_current INTEGER NOT NULL DEFAULT 1,PRIMARY KEY(storage_id,object_id),FOREIGN KEY(storage_id) REFERENCES music_storages(id),FOREIGN KEY(generation_id) REFERENCES inventory_generations(id));
CREATE TABLE IF NOT EXISTS inventory_staged_objects(generation_id TEXT NOT NULL,storage_id TEXT NOT NULL,object_id TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(generation_id,storage_id,object_id),FOREIGN KEY(generation_id) REFERENCES inventory_generations(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS storage_object_tombstones(storage_id TEXT NOT NULL,object_id TEXT NOT NULL,generation_id TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(storage_id,object_id,generation_id),FOREIGN KEY(storage_id) REFERENCES music_storages(id),FOREIGN KEY(generation_id) REFERENCES inventory_generations(id));
CREATE TABLE IF NOT EXISTS inventory_staged_tombstones(generation_id TEXT NOT NULL,storage_id TEXT NOT NULL,object_id TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(generation_id,storage_id,object_id),FOREIGN KEY(generation_id) REFERENCES inventory_generations(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS asset_locations(id TEXT PRIMARY KEY,asset_id TEXT NOT NULL,storage_id TEXT NOT NULL,object_id TEXT NOT NULL,payload TEXT NOT NULL,UNIQUE(storage_id,object_id),FOREIGN KEY(asset_id) REFERENCES assets(id),FOREIGN KEY(storage_id,object_id) REFERENCES storage_objects(storage_id,object_id));
CREATE TABLE IF NOT EXISTS match_candidates(id TEXT PRIMARY KEY,track_id TEXT NOT NULL,asset_id TEXT,storage_id TEXT NOT NULL,confidence TEXT NOT NULL,review_required INTEGER NOT NULL,payload TEXT NOT NULL,FOREIGN KEY(track_id) REFERENCES tracks(id),FOREIGN KEY(asset_id) REFERENCES assets(id),FOREIGN KEY(storage_id) REFERENCES music_storages(id));
CREATE TABLE IF NOT EXISTS reconciliation_results(id TEXT PRIMARY KEY,track_id TEXT,asset_id TEXT,storage_id TEXT,storage_object_id TEXT,outcome TEXT NOT NULL,review_required INTEGER NOT NULL,payload TEXT NOT NULL,FOREIGN KEY(track_id) REFERENCES tracks(id),FOREIGN KEY(asset_id) REFERENCES assets(id),FOREIGN KEY(storage_id) REFERENCES music_storages(id));
CREATE TABLE IF NOT EXISTS reconciliation_result_candidates(result_id TEXT NOT NULL,candidate_id TEXT NOT NULL,PRIMARY KEY(result_id,candidate_id),FOREIGN KEY(result_id) REFERENCES reconciliation_results(id) ON DELETE CASCADE,FOREIGN KEY(candidate_id) REFERENCES match_candidates(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS reconciliation_resolutions(id TEXT PRIMARY KEY,result_id TEXT NOT NULL,candidate_id TEXT,decision TEXT NOT NULL,decided_at TEXT NOT NULL,payload TEXT NOT NULL,FOREIGN KEY(result_id) REFERENCES reconciliation_results(id) ON DELETE CASCADE,FOREIGN KEY(candidate_id) REFERENCES match_candidates(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS reconciliation_suppressions(id TEXT PRIMARY KEY,resolution_id TEXT NOT NULL UNIQUE,result_id TEXT NOT NULL,candidate_id TEXT,decision TEXT NOT NULL,expires_at TEXT,payload TEXT NOT NULL,FOREIGN KEY(resolution_id) REFERENCES reconciliation_resolutions(id) ON DELETE CASCADE,FOREIGN KEY(result_id) REFERENCES reconciliation_results(id) ON DELETE CASCADE,FOREIGN KEY(candidate_id) REFERENCES match_candidates(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS transfer_results(idempotency_key TEXT PRIMARY KEY,job_id TEXT NOT NULL UNIQUE,asset_id TEXT NOT NULL,storage_id TEXT,object_id TEXT,completed_at TEXT NOT NULL,payload TEXT NOT NULL,FOREIGN KEY(job_id) REFERENCES sync_jobs(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS snapshot_triggers(id TEXT PRIMARY KEY,source_run_id TEXT,requested_at TEXT NOT NULL,completed_snapshot_id TEXT,payload TEXT NOT NULL,FOREIGN KEY(source_run_id) REFERENCES sync_runs(id),FOREIGN KEY(completed_snapshot_id) REFERENCES catalog_snapshots(id));
CREATE TABLE IF NOT EXISTS cleanup_approvals(id TEXT PRIMARY KEY,storage_id TEXT NOT NULL,object_id TEXT NOT NULL,approved_at TEXT NOT NULL,expires_at TEXT,consumed_at TEXT,payload TEXT NOT NULL,UNIQUE(storage_id,object_id),FOREIGN KEY(storage_id) REFERENCES music_storages(id));
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,payload TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS catalog_snapshots(id TEXT PRIMARY KEY,content_hash TEXT NOT NULL UNIQUE,logical_fingerprint TEXT,created_at TEXT NOT NULL,payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS library_entries(track_id TEXT PRIMARY KEY,payload TEXT NOT NULL,updated_at TEXT NOT NULL,FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS track_analysis(track_id TEXT PRIMARY KEY,payload TEXT NOT NULL,FOREIGN KEY(track_id) REFERENCES tracks(id) ON DELETE CASCADE);
CREATE INDEX IF NOT EXISTS idx_tracks_updated ON tracks(updated_at,id);
CREATE INDEX IF NOT EXISTS idx_playlist_memberships_track ON canonical_playlist_memberships(track_id);
CREATE INDEX IF NOT EXISTS idx_source_accounts_source ON source_accounts(source_id,external_id);
CREATE INDEX IF NOT EXISTS idx_source_relationship_filter ON source_collection_relationships(source_id,account_external_id,collection,entity_kind,identity_key);
CREATE INDEX IF NOT EXISTS idx_source_occurrences_track ON source_track_occurrences(canonical_track_id);
CREATE INDEX IF NOT EXISTS idx_external_playlist_entries ON external_playlist_memberships(source_id,playlist_external_id,position);
CREATE INDEX IF NOT EXISTS idx_assets_track ON assets(track_id,asset_type);

CREATE INDEX IF NOT EXISTS idx_asset_locations_asset ON asset_locations(asset_id);

CREATE INDEX IF NOT EXISTS idx_sync_jobs_run ON sync_jobs(run_id,id);

CREATE INDEX IF NOT EXISTS idx_sync_job_events_run ON sync_job_events(run_id,at,id);

CREATE INDEX IF NOT EXISTS idx_tombstones_storage ON storage_object_tombstones(storage_id,object_id);
CREATE INDEX IF NOT EXISTS idx_snapshots_latest ON catalog_snapshots(created_at DESC,id DESC);
INSERT INTO profile_storage_settings(singleton,active_primary_storage_id) VALUES(1,NULL) ON CONFLICT(singleton) DO NOTHING;
INSERT INTO catalog_schema_version(singleton,version,updated_at) VALUES(1,3,'1970-01-01T00:00:00Z') ON CONFLICT(singleton) DO UPDATE SET version=excluded.version,updated_at=excluded.updated_at;
"#).await?;
    // Version 3 existed briefly during Stage 2A development. Additive guards make opening
    // those catalogs safe without requiring a destructive schema rebuild.
    for (table, column, definition) in [
        ("external_playlists", "snapshot", "snapshot TEXT"),
        ("derived_assets", "source_asset_id", "source_asset_id TEXT"),
        ("sync_runs", "updated_at", "updated_at TEXT"),
        ("sync_jobs", "kind", "kind TEXT NOT NULL DEFAULT ''"),
        ("sync_jobs", "status", "status TEXT"),
        ("sync_jobs", "next_attempt_at", "next_attempt_at TEXT"),
        ("sync_jobs", "lease_expires_at", "lease_expires_at TEXT"),
        ("sync_jobs", "created_at", "created_at TEXT"),
        ("sync_jobs", "idempotency_key", "idempotency_key TEXT"),
        ("sync_job_events", "kind", "kind TEXT NOT NULL DEFAULT ''"),
        ("sync_job_events", "semantic_key", "semantic_key TEXT"),
        (
            "catalog_snapshots",
            "logical_fingerprint",
            "logical_fingerprint TEXT",
        ),
        ("inventory_generations", "run_id", "run_id TEXT"),
        ("inventory_generations", "kind", "kind TEXT"),
        ("inventory_generations", "started_at", "started_at TEXT"),
        ("inventory_generations", "finished_at", "finished_at TEXT"),
        (
            "storage_objects",
            "is_current",
            "is_current INTEGER NOT NULL DEFAULT 1",
        ),
        (
            "match_candidates",
            "review_required",
            "review_required INTEGER NOT NULL DEFAULT 1",
        ),
        ("reconciliation_results", "outcome", "outcome TEXT"),
        (
            "reconciliation_results",
            "review_required",
            "review_required INTEGER NOT NULL DEFAULT 1",
        ),
    ] {
        ensure_column(connection, table, column, definition).await?;
    }
    fold_legacy_track_analysis(connection).await?;
    if sync_job_index_backfill_required(connection).await? {
        backfill_sync_job_indexes(connection).await?;
    }
    connection.execute_batch(r#"
CREATE INDEX IF NOT EXISTS idx_storage_objects_current ON storage_objects(storage_id,is_current,object_id);
CREATE INDEX IF NOT EXISTS idx_candidates_filter ON match_candidates(track_id,asset_id,storage_id,confidence,review_required,id);
CREATE INDEX IF NOT EXISTS idx_results_filter ON reconciliation_results(track_id,asset_id,storage_id,outcome,review_required,id);
CREATE INDEX IF NOT EXISTS idx_sync_jobs_due ON sync_jobs(status,next_attempt_at,lease_expires_at,id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sync_jobs_active_idempotency ON sync_jobs(idempotency_key) WHERE status IN ('pending','running','paused','cancelling');
CREATE UNIQUE INDEX IF NOT EXISTS idx_sync_job_events_semantic ON sync_job_events(semantic_key) WHERE semantic_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_result_candidates_candidate ON reconciliation_result_candidates(candidate_id,result_id);
CREATE INDEX IF NOT EXISTS idx_reconciliation_resolutions_filter ON reconciliation_resolutions(result_id,candidate_id,decision,id);
CREATE INDEX IF NOT EXISTS idx_reconciliation_suppressions_filter ON reconciliation_suppressions(result_id,candidate_id,decision,id);
CREATE INDEX IF NOT EXISTS idx_snapshots_logical_fingerprint ON catalog_snapshots(logical_fingerprint,created_at DESC);
CREATE INDEX IF NOT EXISTS idx_inventory_latest ON inventory_generations(storage_id,status,finished_at DESC);
"#).await?;
    Ok(())
}

/// Legacy Stage 2A rows can lack the indexed values mirrored from their JSON payload. Once
/// populated, do not scan and rewrite every job again at application startup.
async fn sync_job_index_backfill_required(connection: &Connection) -> Result<bool, MigrationError> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM sync_jobs WHERE kind IS NULL OR status IS NULL OR idempotency_key IS NULL OR idempotency_key='' LIMIT 1",
            (),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn backfill_sync_job_indexes(connection: &Connection) -> Result<(), MigrationError> {
    let mut rows = connection
        .query("SELECT id,payload FROM sync_jobs", ())
        .await?;
    let mut updates = Vec::new();
    while let Some(row) = rows.next().await? {
        let id: String = row.get(0)?;
        let payload: String = row.get(1)?;
        match serde_json::from_str::<rec_core::SyncJob>(&payload) {
            Ok(job) => updates.push((
                id,
                json(&job.kind)?.trim_matches('"').to_owned(),
                json(&job.status)?.trim_matches('"').to_owned(),
                job.idempotency_key,
            )),
            // Preserve readable legacy rows even if a future payload cannot be decoded by
            // this binary. Its unique ID remains a safe conservative idempotency key.
            Err(_) => updates.push((id.clone(), String::new(), String::new(), id)),
        }
    }
    drop(rows);
    let mut active_keys = BTreeSet::new();
    for (id, _, status, idempotency_key) in &mut updates {
        if matches!(
            status.as_str(),
            "pending" | "running" | "paused" | "cancelling"
        ) && !active_keys.insert(idempotency_key.clone())
        {
            *idempotency_key = format!("{idempotency_key}:legacy-duplicate:{id}");
        }
    }
    for (id, kind, status, idempotency_key) in updates {
        if kind.is_empty() {
            connection.execute(
                "UPDATE sync_jobs SET idempotency_key=?2 WHERE id=?1 AND (idempotency_key IS NULL OR idempotency_key='')",
                (id, idempotency_key),
            ).await?;
        } else {
            connection
                .execute(
                    "UPDATE sync_jobs SET kind=?2,status=?3,idempotency_key=?4 WHERE id=?1",
                    (id, kind, status, idempotency_key),
                )
                .await?;
        }
    }
    Ok(())
}

/// Analysis was briefly kept as a bare waveform blob under its own legacy table. It is a
/// derived value, so folding it into `track_analysis` and dropping the old table costs at
/// worst one re-analysis rather than anything the user owns.
async fn fold_legacy_track_analysis(connection: &Connection) -> Result<(), MigrationError> {
    if !table_exists(connection, "legacy_track_analysis").await? {
        return Ok(());
    }
    let mut rows = connection
        .query(
            "SELECT track_id,waveform_json FROM legacy_track_analysis ORDER BY track_id",
            (),
        )
        .await?;
    let mut folded = Vec::new();
    while let Some(row) = rows.next().await? {
        let raw_id: String = row.get(0)?;
        let waveform_json: String = row.get(1)?;
        let track_id = parse_uuid(&raw_id, "track")?;
        folded.push((raw_id, track_analysis(track_id, &waveform_json)?));
    }
    drop(rows);
    for (raw_id, analysis) in folded {
        connection
            .execute(
                "INSERT INTO track_analysis(track_id,payload) VALUES(?1,?2) ON CONFLICT(track_id) DO NOTHING",
                (raw_id, json(&analysis)?),
            )
            .await?;
    }
    connection
        .execute_batch("DROP TABLE legacy_track_analysis;")
        .await?;
    Ok(())
}

/// Build an analysis record from a legacy waveform blob, discarding a blob this binary
/// cannot read rather than refusing to open the catalog.
fn track_analysis(track_id: Uuid, waveform_json: &str) -> Result<TrackAnalysis, MigrationError> {
    Ok(TrackAnalysis {
        track_id,
        waveform: serde_json::from_str::<WaveformPeaks>(waveform_json).ok(),
        ..TrackAnalysis::new(track_id)
    })
}

async fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), MigrationError> {
    let mut rows = connection
        .query(&format!("PRAGMA table_info({table})"), ())
        .await?;
    while let Some(row) = rows.next().await? {
        if row.get::<String>(1)? == column {
            return Ok(());
        }
    }
    connection
        .execute_batch(format!("ALTER TABLE {table} ADD COLUMN {definition};"))
        .await?;
    Ok(())
}
