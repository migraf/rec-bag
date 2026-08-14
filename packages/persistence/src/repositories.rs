use crate::{CatalogTransaction, TursoCatalog};
use chrono::{DateTime, Utc};
use rec_core::*;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use turso::Connection;
use uuid::Uuid;

pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;
pub const SNAPSHOT_CONTENT_TYPE: &str = "application/vnd.rec-bag.catalog-snapshot+json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshTokenMetadata {
    pub source_id: Uuid,
    /// Opaque key into a separate credential store. This is not a token.
    pub credential_reference: String,
    pub refresh_generation: u64,
    pub last_refreshed_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotDocument {
    format_version: u32,
    exported_at: DateTime<Utc>,
    tracks: Vec<CanonicalTrack>,
    playlists: Vec<CanonicalPlaylist>,
    sources: Vec<CatalogSource>,
    source_accounts: Vec<SourceAccount>,
    source_relationships: Vec<SourceCollectionRelationship>,
    source_occurrences: Vec<SourceTrackOccurrence>,
    external_playlists: Vec<ExternalPlaylist>,
    music_storages: Vec<MusicStorage>,
    original_assets: Vec<OriginalAsset>,
    derived_assets: Vec<DerivedAsset>,
    storage_objects: Vec<StorageObject>,
    inventory_generations: Vec<InventoryGeneration>,
    storage_object_tombstones: Vec<StorageObjectTombstone>,
    asset_locations: Vec<AssetLocation>,
    match_candidates: Vec<MatchCandidate>,
    reconciliation_results: Vec<ReconciliationResult>,
    sync_runs: Vec<SyncRun>,
    sync_jobs: Vec<SyncJob>,
    job_events: Vec<rec_sync::JobEvent>,
    settings: Vec<Setting>,
    refresh_token_metadata: Vec<RefreshTokenMetadata>,
}

pub(crate) trait Backend: Send + Sync {
    fn db(&self) -> &Connection;
    fn is_transaction(&self) -> bool;
}

fn invalid_data(message: impl Into<String>) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::InvalidData, message)
}
fn conflict(message: impl Into<String>) -> RepositoryError {
    RepositoryError::new(RepositoryErrorKind::Conflict, message)
}
fn database_error(error: turso::Error) -> RepositoryError {
    let message = error.to_string();
    let kind = if message.contains("constraint") || message.contains("FOREIGN KEY") {
        RepositoryErrorKind::ConstraintViolation
    } else if message.contains("busy") || message.contains("locked") {
        RepositoryErrorKind::TemporarilyUnavailable
    } else {
        RepositoryErrorKind::Permanent
    };
    RepositoryError::new(
        kind,
        format!("catalog database operation failed: {message}"),
    )
}

pub(crate) fn transaction_database_error(error: turso::Error) -> RepositoryError {
    database_error(error)
}

fn encode<T: Serialize>(value: &T) -> RepositoryResult<String> {
    serde_json::to_string(value)
        .map_err(|error| invalid_data(format!("failed to serialize catalog record: {error}")))
}
fn decode<T: DeserializeOwned>(payload: String) -> RepositoryResult<T> {
    serde_json::from_str(&payload)
        .map_err(|error| invalid_data(format!("stored catalog record is invalid: {error}")))
}
fn page_limit(page: &RepositoryPageRequest) -> usize {
    page.limit.unwrap_or(100).clamp(1, 500) as usize
}
fn enum_key<T: Serialize>(value: &T) -> RepositoryResult<String> {
    let value = encode(value)?;
    Ok(value.trim_matches('"').to_owned())
}

async fn begin_atomic<B: Backend + ?Sized>(backend: &B) -> RepositoryResult<bool> {
    if backend.is_transaction() {
        Ok(false)
    } else {
        backend
            .db()
            .execute_batch("BEGIN IMMEDIATE;")
            .await
            .map_err(database_error)?;
        Ok(true)
    }
}
async fn finish_atomic<B: Backend + ?Sized, T>(
    backend: &B,
    owner: bool,
    result: RepositoryResult<T>,
) -> RepositoryResult<T> {
    if !owner {
        return result;
    }
    match result {
        Ok(value) => {
            backend
                .db()
                .execute_batch("COMMIT;")
                .await
                .map_err(database_error)?;
            Ok(value)
        }
        Err(error) => {
            let _ = backend.db().execute_batch("ROLLBACK;").await;
            Err(error)
        }
    }
}

async fn get_payload<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
    params: impl turso::IntoParams,
) -> RepositoryResult<Option<T>> {
    let mut rows = connection
        .query(sql, params)
        .await
        .map_err(database_error)?;
    match rows.next().await.map_err(database_error)? {
        Some(row) => decode(row.get::<String>(0).map_err(database_error)?).map(Some),
        None => Ok(None),
    }
}

async fn list_payloads<T: DeserializeOwned>(
    connection: &Connection,
    sql: &str,
) -> RepositoryResult<Vec<T>> {
    let mut rows = connection.query(sql, ()).await.map_err(database_error)?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().await.map_err(database_error)? {
        values.push(decode(row.get::<String>(0).map_err(database_error)?)?);
    }
    Ok(values)
}

async fn page_payloads<T: DeserializeOwned>(
    connection: &Connection,
    table: &str,
    page: &RepositoryPageRequest,
) -> RepositoryResult<RepositoryPage<T>> {
    let cursor = page
        .cursor
        .as_ref()
        .map(|value| value.0.as_str())
        .unwrap_or("");
    let sql = format!("SELECT id,payload FROM {table} WHERE id>?1 ORDER BY id");
    let mut rows = connection
        .query(&sql, [cursor])
        .await
        .map_err(database_error)?;
    let limit = page_limit(page);
    let mut items = Vec::new();
    let mut last_id = None;
    let mut has_more = false;
    while let Some(row) = rows.next().await.map_err(database_error)? {
        if items.len() == limit {
            has_more = true;
            break;
        }
        last_id = Some(row.get::<String>(0).map_err(database_error)?);
        items.push(decode(row.get::<String>(1).map_err(database_error)?)?);
    }
    Ok(RepositoryPage {
        items,
        next_cursor: has_more.then(|| RepositoryCursor(last_id.expect("non-empty page"))),
    })
}

async fn replace_external_entries<B: Backend + ?Sized>(
    backend: &B,
    source_id: Uuid,
    external_id: &str,
    entries: &[ExternalPlaylistEntry],
) -> RepositoryResult<()> {
    backend.db().execute("DELETE FROM external_playlist_memberships WHERE source_id=?1 AND playlist_external_id=?2", (source_id.to_string(), external_id)).await.map_err(database_error)?;
    for entry in entries {
        if entry.track.source_id != source_id {
            return Err(invalid_data(
                "external playlist entry belongs to another source",
            ));
        }
        backend.db().execute("INSERT INTO external_playlist_memberships(source_id,playlist_external_id,position,payload) VALUES(?1,?2,?3,?4)", (source_id.to_string(), external_id, entry.position as i64, encode(entry)?)).await.map_err(database_error)?;
    }
    Ok(())
}

macro_rules! impl_repositories {
    ($ty:ty) => {
        impl TrackRepository for $ty {
            fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<CanonicalTrack>>> {
                Box::pin(async move { get_payload(self.db(), "SELECT payload FROM tracks WHERE id=?1", [id.to_string()]).await })
            }
            fn list(&self, page: RepositoryPageRequest) -> BoxFuture<'_, RepositoryResult<RepositoryPage<CanonicalTrack>>> {
                Box::pin(async move { page_payloads(self.db(), "tracks", &page).await })
            }
            fn upsert(&self, track: CanonicalTrack) -> BoxFuture<'_, RepositoryResult<()>> {
                Box::pin(async move {
                    let owner = begin_atomic(self).await?;
                    let result = async {
                        self.db().execute("INSERT INTO tracks(id,payload,created_at,updated_at,legacy_path) VALUES(?1,?2,?3,?4,NULL) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at", (track.id.to_string(), encode(&track)?, track.created_at.to_rfc3339(), track.updated_at.to_rfc3339())).await.map_err(database_error)?;
                        self.db().execute("DELETE FROM track_metadata_values WHERE track_id=?1", [track.id.to_string()]).await.map_err(database_error)?;
                        for (ordinal, value) in track.metadata.values.iter().enumerate() {
                            self.db().execute("INSERT INTO track_metadata_values(track_id,ordinal,payload) VALUES(?1,?2,?3)", (track.id.to_string(), ordinal as i64, encode(value)?)).await.map_err(database_error)?;
                        }
                        Ok(())
                    }.await;
                    finish_atomic(self, owner, result).await
                })
            }
        }

        impl PlaylistRepository for $ty {
            fn get(&self, id: Uuid) -> BoxFuture<'_, RepositoryResult<Option<CanonicalPlaylist>>> { Box::pin(async move { get_payload(self.db(), "SELECT payload FROM canonical_playlists WHERE id=?1", [id.to_string()]).await }) }
            fn list(&self, page: RepositoryPageRequest) -> BoxFuture<'_, RepositoryResult<RepositoryPage<CanonicalPlaylist>>> { Box::pin(async move { page_payloads(self.db(), "canonical_playlists", &page).await }) }
            fn upsert(&self, playlist: CanonicalPlaylist) -> BoxFuture<'_, RepositoryResult<()>> {
                Box::pin(async move {
                    let owner=begin_atomic(self).await?;
                    let result=async {
                        self.db().execute("INSERT INTO canonical_playlists(id,payload,created_at,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at", (playlist.id.to_string(),encode(&playlist)?,playlist.created_at.to_rfc3339(),playlist.updated_at.to_rfc3339())).await.map_err(database_error)?;
                        self.db().execute("DELETE FROM canonical_playlist_memberships WHERE playlist_id=?1",[playlist.id.to_string()]).await.map_err(database_error)?;
                        for entry in &playlist.entries { self.db().execute("INSERT INTO canonical_playlist_memberships(playlist_id,position,track_id) VALUES(?1,?2,?3)",(playlist.id.to_string(),entry.position as i64,entry.track_id.to_string())).await.map_err(database_error)?; }
                        Ok(())
                    }.await;
                    finish_atomic(self,owner,result).await
                })
            }
            fn get_external(&self, source_id: Uuid, external_id: String) -> BoxFuture<'_, RepositoryResult<Option<ExternalPlaylist>>> { Box::pin(async move { get_payload(self.db(),"SELECT payload FROM external_playlists WHERE source_id=?1 AND external_id=?2",(source_id.to_string(),external_id)).await }) }
            fn upsert_external(&self, playlist: ExternalPlaylist) -> BoxFuture<'_, RepositoryResult<()>> {
                Box::pin(async move {
                    let owner=begin_atomic(self).await?;
                    let result=async {
                        self.db().execute("INSERT INTO external_playlists(source_id,external_id,payload,snapshot) VALUES(?1,?2,?3,?4) ON CONFLICT(source_id,external_id) DO UPDATE SET payload=excluded.payload,snapshot=excluded.snapshot",(playlist.source_id.to_string(),playlist.external_id.clone(),encode(&playlist)?,playlist.snapshot.clone())).await.map_err(database_error)?;
                        replace_external_entries(self,playlist.source_id,&playlist.external_id,&playlist.entries).await
                    }.await;
                    finish_atomic(self,owner,result).await
                })
            }
            fn replace_external_entries(&self, source_id: Uuid, external_id: String, entries: Vec<ExternalPlaylistEntry>) -> BoxFuture<'_, RepositoryResult<()>> {
                Box::pin(async move { let owner=begin_atomic(self).await?; let result=replace_external_entries(self,source_id,&external_id,&entries).await; finish_atomic(self,owner,result).await })
            }
            fn begin_external_entry_staging(&self, scope: ExternalPlaylistEntryStagingScope) -> BoxFuture<'_, RepositoryResult<()>> { Box::pin(async move { self.db().execute("DELETE FROM external_playlist_entry_staging WHERE run_id=?1 AND source_id=?2 AND playlist_external_id=?3 AND snapshot_key=?4",(scope.run_id.to_string(),scope.source_id.to_string(),scope.playlist_external_id,scope.snapshot.unwrap_or_default())).await.map_err(database_error)?; Ok(()) }) }
            fn stage_external_entry_page(&self, scope: ExternalPlaylistEntryStagingScope, entries: Vec<ExternalPlaylistEntry>) -> BoxFuture<'_, RepositoryResult<()>> {
                Box::pin(async move { let owner=begin_atomic(self).await?; let result=async { let snapshot=scope.snapshot.unwrap_or_default(); for entry in entries { if entry.track.source_id!=scope.source_id{return Err(invalid_data("staged playlist entry belongs to another source"));} self.db().execute("INSERT INTO external_playlist_entry_staging(run_id,source_id,playlist_external_id,snapshot_key,position,payload) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(run_id,source_id,playlist_external_id,snapshot_key,position) DO UPDATE SET payload=excluded.payload",(scope.run_id.to_string(),scope.source_id.to_string(),scope.playlist_external_id.clone(),snapshot.clone(),entry.position as i64,encode(&entry)?)).await.map_err(database_error)?; } Ok(()) }.await; finish_atomic(self,owner,result).await })
            }
            fn finalize_external_entry_staging(&self, scope: ExternalPlaylistEntryStagingScope) -> BoxFuture<'_, RepositoryResult<()>> {
                Box::pin(async move { let owner=begin_atomic(self).await?; let result=async { let snapshot=scope.snapshot.clone().unwrap_or_default(); let mut rows=self.db().query("SELECT payload FROM external_playlist_entry_staging WHERE run_id=?1 AND source_id=?2 AND playlist_external_id=?3 AND snapshot_key=?4 ORDER BY position",(scope.run_id.to_string(),scope.source_id.to_string(),scope.playlist_external_id.clone(),snapshot.clone())).await.map_err(database_error)?; let mut entries=Vec::new(); while let Some(row)=rows.next().await.map_err(database_error)? { entries.push(decode(row.get::<String>(0).map_err(database_error)?)?); } replace_external_entries(self,scope.source_id,&scope.playlist_external_id,&entries).await?; if let Some(mut playlist)=get_payload::<ExternalPlaylist>(self.db(),"SELECT payload FROM external_playlists WHERE source_id=?1 AND external_id=?2",(scope.source_id.to_string(),scope.playlist_external_id.clone())).await? { playlist.entries=entries; playlist.snapshot=scope.snapshot.clone(); self.db().execute("UPDATE external_playlists SET payload=?3,snapshot=?4 WHERE source_id=?1 AND external_id=?2",(scope.source_id.to_string(),scope.playlist_external_id.clone(),encode(&playlist)?,scope.snapshot.clone())).await.map_err(database_error)?; } else { return Err(RepositoryError::new(RepositoryErrorKind::NotFound,"external playlist does not exist")); } self.db().execute("DELETE FROM external_playlist_entry_staging WHERE run_id=?1 AND source_id=?2 AND playlist_external_id=?3 AND snapshot_key=?4",(scope.run_id.to_string(),scope.source_id.to_string(),scope.playlist_external_id,snapshot)).await.map_err(database_error)?; Ok(()) }.await; finish_atomic(self,owner,result).await })
            }
            fn discard_external_entry_staging(&self, scope: ExternalPlaylistEntryStagingScope) -> BoxFuture<'_, RepositoryResult<()>> { Box::pin(async move { self.db().execute("DELETE FROM external_playlist_entry_staging WHERE run_id=?1 AND source_id=?2 AND playlist_external_id=?3 AND snapshot_key=?4",(scope.run_id.to_string(),scope.source_id.to_string(),scope.playlist_external_id,scope.snapshot.unwrap_or_default())).await.map_err(database_error)?; Ok(()) }) }
        }

        impl SourceRepository for $ty {
            fn get(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<CatalogSource>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM catalog_sources WHERE id=?1",[id.to_string()]).await})}
            fn list(&self,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<CatalogSource>>>{Box::pin(async move{page_payloads(self.db(),"catalog_sources",&page).await})}
            fn upsert(&self,source:CatalogSource)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO catalog_sources(id,payload,created_at,updated_at,legacy_uri) VALUES(?1,?2,?3,?4,NULL) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",(source.id.to_string(),encode(&source)?,source.created_at.to_rfc3339(),source.last_synced_at.unwrap_or(source.created_at).to_rfc3339())).await.map_err(database_error)?;Ok(())})}
            fn get_account(&self,source_id:Uuid,external_id:String)->BoxFuture<'_,RepositoryResult<Option<SourceAccount>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM source_accounts WHERE source_id=?1 AND external_id=?2",(source_id.to_string(),external_id)).await})}
            fn list_accounts(&self,source_id:Uuid,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<SourceAccount>>>{Box::pin(async move{let cursor=page.cursor.as_ref().map(|v|v.0.as_str()).unwrap_or("");let mut rows=self.db().query("SELECT external_id,payload FROM source_accounts WHERE source_id=?1 AND external_id>?2 ORDER BY external_id",(source_id.to_string(),cursor)).await.map_err(database_error)?;let mut items=Vec::new();let mut last=None;let mut more=false;while let Some(row)=rows.next().await.map_err(database_error)?{if items.len()==page_limit(&page){more=true;break;}last=Some(row.get::<String>(0).map_err(database_error)?);items.push(decode(row.get::<String>(1).map_err(database_error)?)?);}Ok(RepositoryPage{items,next_cursor:more.then(||RepositoryCursor(last.unwrap()))})})}
            fn upsert_account(&self,account:SourceAccount)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO source_accounts(source_id,external_id,payload) VALUES(?1,?2,?3) ON CONFLICT(source_id,external_id) DO UPDATE SET payload=excluded.payload",(account.source_id.to_string(),account.external_id.clone(),encode(&account)?)).await.map_err(database_error)?;Ok(())})}
            fn list_collection_relationships(&self,filter:SourceCollectionRelationshipFilter,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<SourceCollectionRelationship>>>{Box::pin(async move{let all=list_payloads::<SourceCollectionRelationship>(self.db(),"SELECT payload FROM source_collection_relationships ORDER BY identity_key").await?;let cursor=page.cursor.as_ref().map(|v|v.0.as_str()).unwrap_or("");let mut matched=Vec::new();for value in all{let key=encode(&value.identity)?;if key<=cursor.to_string()||value.identity.source_id!=filter.source_id||filter.account_external_id.as_ref().is_some_and(|v|v!=&value.identity.account_external_id)||filter.collection.is_some_and(|v|v!=value.identity.collection)||filter.entity_kind.is_some_and(|v|v!=value.identity.entity_kind){continue;}matched.push((key,value));}let limit=page_limit(&page);let more=matched.len()>limit;matched.truncate(limit);let next_cursor=more.then(||RepositoryCursor(matched.last().unwrap().0.clone()));Ok(RepositoryPage{items:matched.into_iter().map(|(_,v)|v).collect(),next_cursor})})}
            fn upsert_collection_relationship(&self,value:SourceCollectionRelationship)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{let i=&value.identity;let key=encode(i)?;self.db().execute("INSERT INTO source_collection_relationships(identity_key,source_id,account_external_id,collection,entity_kind,entity_external_id,parent_external_id,position,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(identity_key) DO UPDATE SET payload=excluded.payload",(key,i.source_id.to_string(),i.account_external_id.clone(),enum_key(&i.collection)?,enum_key(&i.entity_kind)?,i.entity_external_id.clone(),i.parent_external_id.clone().unwrap_or_default(),i.position.map(i64::from).unwrap_or(-1),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
            fn remove_collection_relationship(&self,identity:SourceCollectionRelationshipIdentity)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("DELETE FROM source_collection_relationships WHERE identity_key=?1",[encode(&identity)?]).await.map_err(database_error)?;Ok(())})}
            fn get_occurrence(&self,source_id:Uuid,external_id:String)->BoxFuture<'_,RepositoryResult<Option<SourceTrackOccurrence>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM source_track_occurrences WHERE source_id=?1 AND external_id=?2",(source_id.to_string(),external_id)).await})}
            fn upsert_occurrences(&self,values:Vec<SourceTrackOccurrence>)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{let owner=begin_atomic(self).await?;let result=async{for value in values{self.db().execute("INSERT INTO source_track_occurrences(source_id,external_id,canonical_track_id,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(source_id,external_id) DO UPDATE SET canonical_track_id=excluded.canonical_track_id,payload=excluded.payload",(value.identity.source_id.to_string(),value.identity.external_id.clone(),value.canonical_track_id.map(|v|v.to_string()),encode(&value)?)).await.map_err(database_error)?;}Ok(())}.await;finish_atomic(self,owner,result).await})}
        }

        impl AssetRepository for $ty {
            fn get_original(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<OriginalAsset>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM original_assets WHERE id=?1",[id.to_string()]).await})}
            fn list_originals_for_track(&self,track_id:Uuid)->BoxFuture<'_,RepositoryResult<Vec<OriginalAsset>>>{Box::pin(async move{let sql=format!("SELECT o.payload FROM original_assets o JOIN assets a ON a.id=o.id WHERE a.track_id='{}' ORDER BY a.created_at,a.id",track_id);list_payloads(self.db(),&sql).await})}
            fn upsert_original(&self,asset:OriginalAsset)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{if asset.record.asset_type!=AssetType::Original{return Err(invalid_data("original asset has a non-original parent type"));}let owner=begin_atomic(self).await?;let result=async{self.db().execute("INSERT INTO assets(id,track_id,asset_type,payload,created_at) VALUES(?1,?2,'original',?3,?4) ON CONFLICT(id) DO UPDATE SET track_id=excluded.track_id,payload=excluded.payload",(asset.id().to_string(),asset.track_id().to_string(),encode(&asset.record)?,asset.record.created_at.to_rfc3339())).await.map_err(database_error)?;self.db().execute("INSERT INTO original_assets(id,payload) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",(asset.id().to_string(),encode(&asset)?)).await.map_err(database_error)?;Ok(())}.await;finish_atomic(self,owner,result).await})}
            fn upsert_derived(&self,asset:DerivedAsset)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{if asset.record.asset_type!=AssetType::Derived{return Err(invalid_data("derived asset has a non-derived parent type"));}let owner=begin_atomic(self).await?;let result=async{self.db().execute("INSERT INTO assets(id,track_id,asset_type,payload,created_at) VALUES(?1,?2,'derived',?3,?4) ON CONFLICT(id) DO UPDATE SET track_id=excluded.track_id,payload=excluded.payload",(asset.id().to_string(),asset.track_id().to_string(),encode(&asset.record)?,asset.record.created_at.to_rfc3339())).await.map_err(database_error)?;self.db().execute("INSERT INTO derived_assets(id,payload,source_asset_id) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,source_asset_id=excluded.source_asset_id",(asset.id().to_string(),encode(&asset)?,asset.source_asset_id.to_string())).await.map_err(database_error)?;Ok(())}.await;finish_atomic(self,owner,result).await})}
            fn get_storage_object(&self,storage_id:Uuid,object_id:String)->BoxFuture<'_,RepositoryResult<Option<StorageObject>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM storage_objects WHERE storage_id=?1 AND object_id=?2 AND is_current=1",(storage_id.to_string(),object_id)).await})}
            fn list_storage_objects(&self,storage_id:Uuid,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<StorageObject>>>{Box::pin(async move{let cursor=page.cursor.as_ref().map(|v|v.0.as_str()).unwrap_or("");let mut rows=self.db().query("SELECT object_id,payload FROM storage_objects WHERE storage_id=?1 AND is_current=1 AND object_id>?2 ORDER BY object_id",(storage_id.to_string(),cursor)).await.map_err(database_error)?;let mut items=Vec::new();let mut last=None;let mut more=false;while let Some(row)=rows.next().await.map_err(database_error)?{if items.len()==page_limit(&page){more=true;break;}last=Some(row.get::<String>(0).map_err(database_error)?);items.push(decode(row.get::<String>(1).map_err(database_error)?)?);}Ok(RepositoryPage{items,next_cursor:more.then(||RepositoryCursor(last.unwrap()))})})}
            fn upsert_storage_object(&self,value:StorageObject)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO storage_objects(storage_id,object_id,generation_id,payload,is_current) VALUES(?1,?2,NULL,?3,1) ON CONFLICT(storage_id,object_id) DO UPDATE SET payload=excluded.payload,is_current=1",(value.storage_id.to_string(),value.object_id.clone(),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
            fn get_location(&self,storage_id:Uuid,object_id:String)->BoxFuture<'_,RepositoryResult<Option<AssetLocation>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM asset_locations WHERE storage_id=?1 AND object_id=?2",(storage_id.to_string(),object_id)).await})}
            fn list_locations_for_asset(&self,asset_id:Uuid)->BoxFuture<'_,RepositoryResult<Vec<AssetLocation>>>{Box::pin(async move{let sql=format!("SELECT payload FROM asset_locations WHERE asset_id='{}' ORDER BY id",asset_id);list_payloads(self.db(),&sql).await})}
            fn upsert_location(&self,value:AssetLocation)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO asset_locations(id,asset_id,storage_id,object_id,payload) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET asset_id=excluded.asset_id,storage_id=excluded.storage_id,object_id=excluded.object_id,payload=excluded.payload",(value.id.to_string(),value.asset_id.to_string(),value.storage_id.to_string(),value.object_id.clone(),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
            fn get_inventory_generation(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<InventoryGeneration>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM inventory_generations WHERE id=?1",[id.to_string()]).await})}
            fn latest_finalized_inventory_generation(&self,storage_id:Uuid)->BoxFuture<'_,RepositoryResult<Option<InventoryGeneration>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM inventory_generations WHERE storage_id=?1 AND status='finalized' ORDER BY finished_at DESC,id DESC LIMIT 1",[storage_id.to_string()]).await})}
            fn begin_inventory_generation(&self,start:InventoryGenerationStart)->BoxFuture<'_,RepositoryResult<InventoryGeneration>>{Box::pin(async move{if let Some(existing)=get_payload::<InventoryGeneration>(self.db(),"SELECT payload FROM inventory_generations WHERE id=?1",[start.id.to_string()]).await?{if existing.storage_id==start.storage_id&&existing.run_id==start.run_id&&existing.kind==start.kind{return Ok(existing);}return Err(conflict("inventory generation ID is already used by different inputs"));}let value=InventoryGeneration{id:start.id,storage_id:start.storage_id,run_id:start.run_id,kind:start.kind,status:InventoryGenerationStatus::Staging,started_at:start.started_at,finished_at:None,message:None};self.db().execute("INSERT INTO inventory_generations(id,storage_id,run_id,kind,status,started_at,finished_at,payload) VALUES(?1,?2,?3,?4,'staging',?5,NULL,?6)",(value.id.to_string(),value.storage_id.to_string(),value.run_id.to_string(),enum_key(&value.kind)?,value.started_at.to_rfc3339(),encode(&value)?)).await.map_err(database_error)?;Ok(value)})}
            fn stage_inventory_page(&self,generation_id:Uuid,page:InventoryStagingPage)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{let owner=begin_atomic(self).await?;let result=async{let generation=get_payload::<InventoryGeneration>(self.db(),"SELECT payload FROM inventory_generations WHERE id=?1",[generation_id.to_string()]).await?.ok_or_else(||RepositoryError::new(RepositoryErrorKind::NotFound,"inventory generation does not exist"))?;if generation.status!=InventoryGenerationStatus::Staging{return Err(conflict("inventory generation is not staging"));}for value in page.objects{if value.storage_id!=generation.storage_id{return Err(invalid_data("staged object belongs to another storage"));}self.db().execute("INSERT INTO inventory_staged_objects(generation_id,storage_id,object_id,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(generation_id,storage_id,object_id) DO UPDATE SET payload=excluded.payload",(generation_id.to_string(),value.storage_id.to_string(),value.object_id.clone(),encode(&value)?)).await.map_err(database_error)?;}for value in page.tombstones{if value.storage_id!=generation.storage_id||value.generation_id!=generation_id{return Err(invalid_data("staged tombstone has mismatched scope"));}self.db().execute("INSERT INTO inventory_staged_tombstones(generation_id,storage_id,object_id,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(generation_id,storage_id,object_id) DO UPDATE SET payload=excluded.payload",(generation_id.to_string(),value.storage_id.to_string(),value.object_id.clone(),encode(&value)?)).await.map_err(database_error)?;}Ok(())}.await;finish_atomic(self,owner,result).await})}
            fn finalize_inventory_generation(&self,generation_id:Uuid,finalized_at:DateTime<Utc>)->BoxFuture<'_,RepositoryResult<InventoryGeneration>>{Box::pin(async move{let owner=begin_atomic(self).await?;let result=async{let mut generation=get_payload::<InventoryGeneration>(self.db(),"SELECT payload FROM inventory_generations WHERE id=?1",[generation_id.to_string()]).await?.ok_or_else(||RepositoryError::new(RepositoryErrorKind::NotFound,"inventory generation does not exist"))?;if generation.status==InventoryGenerationStatus::Finalized{return Ok(generation);}if generation.status!=InventoryGenerationStatus::Staging{return Err(conflict("inventory generation cannot be finalized"));}if generation.kind==InventoryGenerationKind::Full{let mut rows=self.db().query("SELECT object_id,payload FROM storage_objects WHERE storage_id=?1 AND is_current=1 AND object_id NOT IN (SELECT object_id FROM inventory_staged_objects WHERE generation_id=?2)",(generation.storage_id.to_string(),generation_id.to_string())).await.map_err(database_error)?;while let Some(row)=rows.next().await.map_err(database_error)?{let object_id:String=row.get(0).map_err(database_error)?;let object:StorageObject=decode(row.get::<String>(1).map_err(database_error)?)?;let tombstone=StorageObjectTombstone{storage_id:generation.storage_id,object_id:object_id.clone(),generation_id,removed_at:finalized_at,provider_version:object.provider_version};self.db().execute("INSERT INTO storage_object_tombstones(storage_id,object_id,generation_id,payload) VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",(generation.storage_id.to_string(),object_id,generation_id.to_string(),encode(&tombstone)?)).await.map_err(database_error)?;}self.db().execute("UPDATE storage_objects SET is_current=0 WHERE storage_id=?1 AND object_id NOT IN (SELECT object_id FROM inventory_staged_objects WHERE generation_id=?2)",(generation.storage_id.to_string(),generation_id.to_string())).await.map_err(database_error)?;}self.db().execute("INSERT INTO storage_object_tombstones(storage_id,object_id,generation_id,payload) SELECT storage_id,object_id,generation_id,payload FROM inventory_staged_tombstones WHERE generation_id=?1 ON CONFLICT DO NOTHING",[generation_id.to_string()]).await.map_err(database_error)?;self.db().execute("UPDATE storage_objects SET is_current=0 WHERE storage_id=?1 AND object_id IN (SELECT object_id FROM inventory_staged_tombstones WHERE generation_id=?2)",(generation.storage_id.to_string(),generation_id.to_string())).await.map_err(database_error)?;self.db().execute("INSERT INTO storage_objects(storage_id,object_id,generation_id,payload,is_current) SELECT storage_id,object_id,generation_id,payload,1 FROM inventory_staged_objects WHERE generation_id=?1 ON CONFLICT(storage_id,object_id) DO UPDATE SET generation_id=excluded.generation_id,payload=excluded.payload,is_current=1",[generation_id.to_string()]).await.map_err(database_error)?;generation.status=InventoryGenerationStatus::Finalized;generation.finished_at=Some(finalized_at);self.db().execute("UPDATE inventory_generations SET status='finalized',finished_at=?2,payload=?3 WHERE id=?1",(generation_id.to_string(),finalized_at.to_rfc3339(),encode(&generation)?)).await.map_err(database_error)?;if let Some(mut storage)=MusicStorageRepository::get(self,generation.storage_id).await?{storage.last_inventory_at=Some(finalized_at);storage.updated_at=storage.updated_at.max(finalized_at);MusicStorageRepository::upsert(self,storage).await?;}Ok(generation)}.await;finish_atomic(self,owner,result).await})}
            fn abort_inventory_generation(&self,generation_id:Uuid,abort:InventoryGenerationAbort)->BoxFuture<'_,RepositoryResult<InventoryGeneration>>{Box::pin(async move{let owner=begin_atomic(self).await?;let result=async{let mut value=get_payload::<InventoryGeneration>(self.db(),"SELECT payload FROM inventory_generations WHERE id=?1",[generation_id.to_string()]).await?.ok_or_else(||RepositoryError::new(RepositoryErrorKind::NotFound,"inventory generation does not exist"))?;if value.status!=InventoryGenerationStatus::Staging{return Err(conflict("only a staging generation can be aborted"));}value.status=match abort.kind{InventoryGenerationAbortKind::Unavailable=>InventoryGenerationStatus::Unavailable,InventoryGenerationAbortKind::Failed=>InventoryGenerationStatus::Failed};value.finished_at=Some(abort.finished_at);value.message=abort.message;self.db().execute("UPDATE inventory_generations SET status=?2,finished_at=?3,payload=?4 WHERE id=?1",(generation_id.to_string(),enum_key(&value.status)?,abort.finished_at.to_rfc3339(),encode(&value)?)).await.map_err(database_error)?;self.db().execute("DELETE FROM inventory_staged_objects WHERE generation_id=?1",[generation_id.to_string()]).await.map_err(database_error)?;self.db().execute("DELETE FROM inventory_staged_tombstones WHERE generation_id=?1",[generation_id.to_string()]).await.map_err(database_error)?;Ok(value)}.await;finish_atomic(self,owner,result).await})}
            fn list_storage_object_tombstones(&self,storage_id:Uuid,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<StorageObjectTombstone>>>{Box::pin(async move{let cursor=page.cursor.as_ref().map(|v|v.0.as_str()).unwrap_or("");let mut rows=self.db().query("SELECT object_id||':'||generation_id,payload FROM storage_object_tombstones WHERE storage_id=?1 AND object_id||':'||generation_id>?2 ORDER BY object_id,generation_id",(storage_id.to_string(),cursor)).await.map_err(database_error)?;let mut items=Vec::new();let mut last=None;let mut more=false;while let Some(row)=rows.next().await.map_err(database_error)?{if items.len()==page_limit(&page){more=true;break;}last=Some(row.get::<String>(0).map_err(database_error)?);items.push(decode(row.get::<String>(1).map_err(database_error)?)?);}Ok(RepositoryPage{items,next_cursor:more.then(||RepositoryCursor(last.unwrap()))})})}
        }

        impl ReconciliationRepository for $ty {
            fn get_candidate(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<MatchCandidate>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM match_candidates WHERE id=?1",[id.to_string()]).await})}
            fn list_candidates(&self,filter:MatchCandidateFilter,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<MatchCandidate>>>{Box::pin(async move{let all=list_payloads::<MatchCandidate>(self.db(),"SELECT payload FROM match_candidates ORDER BY id").await?;filter_page(all,page,|v|filter.track_id.is_none_or(|x|x==v.track_id)&&filter.asset_id.is_none_or(|x|Some(x)==v.asset_id)&&filter.storage_id.is_none_or(|x|x==v.storage_id)&&filter.confidence.is_none_or(|x|x==v.confidence())&&filter.review_required.is_none_or(|x|x==v.requires_confirmation()),|v|v.id.to_string())})}
            fn upsert_candidate(&self,value:MatchCandidate)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO match_candidates(id,track_id,asset_id,storage_id,confidence,review_required,payload) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET track_id=excluded.track_id,asset_id=excluded.asset_id,storage_id=excluded.storage_id,confidence=excluded.confidence,review_required=excluded.review_required,payload=excluded.payload",(value.id.to_string(),value.track_id.to_string(),value.asset_id.map(|v|v.to_string()),value.storage_id.to_string(),enum_key(&value.confidence())?,i64::from(value.requires_confirmation()),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
            fn get_result(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<ReconciliationResult>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM reconciliation_results WHERE id=?1",[id.to_string()]).await})}
            fn list_results(&self,filter:ReconciliationResultFilter,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<ReconciliationResult>>>{Box::pin(async move{let all=list_payloads::<ReconciliationResult>(self.db(),"SELECT payload FROM reconciliation_results ORDER BY id").await?;filter_page(all,page,|v|filter.track_id.is_none_or(|x|Some(x)==v.track_id)&&filter.asset_id.is_none_or(|x|Some(x)==v.asset_id)&&filter.storage_id.is_none_or(|x|Some(x)==v.storage_id)&&filter.storage_object_id.as_ref().is_none_or(|x|Some(x)==v.storage_object_id.as_ref())&&(filter.outcomes.is_empty()||filter.outcomes.contains(&v.outcome))&&filter.review_required.is_none_or(|x|x==result_requires_review(v)),|v|v.id.to_string())})}
            fn upsert_result(&self,value:ReconciliationResult)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO reconciliation_results(id,track_id,asset_id,storage_id,storage_object_id,outcome,review_required,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET track_id=excluded.track_id,asset_id=excluded.asset_id,storage_id=excluded.storage_id,storage_object_id=excluded.storage_object_id,outcome=excluded.outcome,review_required=excluded.review_required,payload=excluded.payload",(value.id.to_string(),value.track_id.map(|v|v.to_string()),value.asset_id.map(|v|v.to_string()),value.storage_id.map(|v|v.to_string()),value.storage_object_id.clone(),enum_key(&value.outcome)?,i64::from(result_requires_review(&value)),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
        }

        impl MusicStorageRepository for $ty {
            fn get(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<MusicStorage>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM music_storages WHERE id=?1",[id.to_string()]).await})}
            fn list(&self)->BoxFuture<'_,RepositoryResult<Vec<MusicStorage>>>{Box::pin(async move{list_payloads(self.db(),"SELECT payload FROM music_storages ORDER BY id").await})}
            fn upsert(&self,value:MusicStorage)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO music_storages(id,payload,created_at,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",(value.id.to_string(),encode(&value)?,value.created_at.to_rfc3339(),value.updated_at.to_rfc3339())).await.map_err(database_error)?;Ok(())})}
            fn get_active_primary(&self)->BoxFuture<'_,RepositoryResult<Option<MusicStorage>>>{Box::pin(async move{let mut rows=self.db().query("SELECT active_primary_storage_id FROM profile_storage_settings WHERE singleton=1",()).await.map_err(database_error)?;let Some(row)=rows.next().await.map_err(database_error)? else{return Ok(None)};let id:Option<String>=row.get(0).map_err(database_error)?;match id{Some(id)=>get_payload(self.db(),"SELECT payload FROM music_storages WHERE id=?1",[id]).await,None=>Ok(None)}})}
            fn switch_active_primary(&self,expected:Option<Uuid>,new:Option<Uuid>)->BoxFuture<'_,RepositoryResult<Option<MusicStorage>>>{Box::pin(async move{let owner=begin_atomic(self).await?;let result=async{let mut rows=self.db().query("SELECT active_primary_storage_id FROM profile_storage_settings WHERE singleton=1",()).await.map_err(database_error)?;let current:Option<String>=rows.next().await.map_err(database_error)?.and_then(|row|row.get::<Option<String>>(0).ok()).flatten();if current!=expected.map(|v|v.to_string()){return Err(conflict("active primary changed"));}if let Some(id)=current{if Some(id.clone())!=new.map(|v|v.to_string()){if let Some(mut storage)=get_payload::<MusicStorage>(self.db(),"SELECT payload FROM music_storages WHERE id=?1",[id.clone()]).await?{storage.role=StorageRole::Unassigned;storage.updated_at=Utc::now();self.db().execute("UPDATE music_storages SET payload=?2,updated_at=?3 WHERE id=?1",(id,encode(&storage)?,storage.updated_at.to_rfc3339())).await.map_err(database_error)?;}}}let selected=if let Some(id)=new{let mut storage=get_payload::<MusicStorage>(self.db(),"SELECT payload FROM music_storages WHERE id=?1",[id.to_string()]).await?.ok_or_else(||RepositoryError::new(RepositoryErrorKind::NotFound,"new primary storage does not exist"))?;storage.role=StorageRole::Primary;storage.updated_at=Utc::now();self.db().execute("UPDATE music_storages SET payload=?2,updated_at=?3 WHERE id=?1",(id.to_string(),encode(&storage)?,storage.updated_at.to_rfc3339())).await.map_err(database_error)?;Some(storage)}else{None};self.db().execute("UPDATE profile_storage_settings SET active_primary_storage_id=?1 WHERE singleton=1",[new.map(|v|v.to_string())]).await.map_err(database_error)?;Ok(selected)}.await;finish_atomic(self,owner,result).await})}
        }

        impl SyncRunRepository for $ty {
            fn get(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<SyncRun>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM sync_runs WHERE id=?1",[id.to_string()]).await})}
            fn list(&self,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<SyncRun>>>{Box::pin(async move{page_payloads(self.db(),"sync_runs",&page).await})}
            fn upsert(&self,value:SyncRun)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO sync_runs(id,payload,created_at,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",(value.id.to_string(),encode(&value)?,value.created_at.to_rfc3339(),value.updated_at.to_rfc3339())).await.map_err(database_error)?;Ok(())})}
            fn get_job(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<SyncJob>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM sync_jobs WHERE id=?1",[id.to_string()]).await})}
            fn list_jobs(&self,run_id:Uuid)->BoxFuture<'_,RepositoryResult<Vec<SyncJob>>>{Box::pin(async move{let sql=format!("SELECT payload FROM sync_jobs WHERE run_id='{}' ORDER BY id",run_id);list_payloads(self.db(),&sql).await})}
            fn upsert_job(&self,value:SyncJob)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{if value.validate_kind().is_err()||!value.payload.has_supported_version(){return Err(invalid_data("sync job kind or payload version is invalid"));}self.db().execute("INSERT INTO sync_jobs(id,run_id,status,next_attempt_at,lease_expires_at,created_at,payload) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET run_id=excluded.run_id,status=excluded.status,next_attempt_at=excluded.next_attempt_at,lease_expires_at=excluded.lease_expires_at,created_at=excluded.created_at,payload=excluded.payload",(value.id.to_string(),value.run_id.to_string(),enum_key(&value.status)?,value.next_attempt_at.map(|v|v.to_rfc3339()),value.lease_expires_at.map(|v|v.to_rfc3339()),value.created_at.to_rfc3339(),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
        }

        impl SettingsRepository for $ty {
            fn get(&self,key:String)->BoxFuture<'_,RepositoryResult<Option<Setting>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM settings WHERE key=?1",[key]).await})}
            fn list(&self)->BoxFuture<'_,RepositoryResult<Vec<Setting>>>{Box::pin(async move{list_payloads(self.db(),"SELECT payload FROM settings ORDER BY key").await})}
            fn set(&self,value:Setting)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{self.db().execute("INSERT INTO settings(key,payload,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at",(value.key.clone(),encode(&value)?,value.updated_at.to_rfc3339())).await.map_err(database_error)?;Ok(())})}
        }

        impl CatalogSnapshotRepository for $ty {
            fn get(&self,id:Uuid)->BoxFuture<'_,RepositoryResult<Option<CatalogSnapshot>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM catalog_snapshots WHERE id=?1",[id.to_string()]).await})}
            fn latest(&self)->BoxFuture<'_,RepositoryResult<Option<CatalogSnapshot>>>{Box::pin(async move{get_payload(self.db(),"SELECT payload FROM catalog_snapshots ORDER BY created_at DESC,id DESC LIMIT 1",()).await})}
            fn list(&self,page:RepositoryPageRequest)->BoxFuture<'_,RepositoryResult<RepositoryPage<CatalogSnapshot>>>{Box::pin(async move{let all=list_payloads::<CatalogSnapshot>(self.db(),"SELECT payload FROM catalog_snapshots ORDER BY created_at DESC,id DESC").await?;let start=page.cursor.as_ref().and_then(|v|v.0.parse::<usize>().ok()).unwrap_or(0);let limit=page_limit(&page);let items=all.into_iter().skip(start).take(limit).collect::<Vec<_>>();let next_cursor=(items.len()==limit).then(||RepositoryCursor((start+items.len()).to_string()));Ok(RepositoryPage{items,next_cursor})})}
            fn put(&self,value:CatalogSnapshot)->BoxFuture<'_,RepositoryResult<()>>{Box::pin(async move{if !value.verify_integrity(){return Err(invalid_data("catalog snapshot integrity check failed"));}if let Some(existing)=get_payload::<CatalogSnapshot>(self.db(),"SELECT payload FROM catalog_snapshots WHERE id=?1",[value.id.to_string()]).await?{return if existing==value{Ok(())}else{Err(conflict("snapshot ID already contains different content"))};}if let Some(existing)=get_payload::<CatalogSnapshot>(self.db(),"SELECT payload FROM catalog_snapshots WHERE content_hash=?1",[value.content_hash().value()]).await?{return if existing.payload()==value.payload(){Ok(())}else{Err(conflict("snapshot hash collision"))};}self.db().execute("INSERT INTO catalog_snapshots(id,content_hash,created_at,payload) VALUES(?1,?2,?3,?4)",(value.id.to_string(),value.content_hash().value(),value.created_at.to_rfc3339(),encode(&value)?)).await.map_err(database_error)?;Ok(())})}
        }
    };
}

fn result_requires_review(value: &ReconciliationResult) -> bool {
    value
        .candidates
        .iter()
        .any(MatchCandidate::requires_confirmation)
        || value.actions.iter().any(|action| {
            matches!(
                action,
                ReconciliationAction::Confirm { .. } | ReconciliationAction::Reject { .. }
            )
        })
}

fn filter_page<T>(
    values: Vec<T>,
    page: RepositoryPageRequest,
    include: impl Fn(&T) -> bool,
    id: impl Fn(&T) -> String,
) -> RepositoryResult<RepositoryPage<T>> {
    let cursor = page.cursor.as_ref().map(|v| v.0.as_str()).unwrap_or("");
    let mut values = values
        .into_iter()
        .filter(|value| id(value).as_str() > cursor && include(value))
        .collect::<Vec<_>>();
    values.sort_by_key(|value| id(value));
    let limit = page_limit(&page);
    let more = values.len() > limit;
    values.truncate(limit);
    let next_cursor = more.then(|| RepositoryCursor(id(values.last().expect("non-empty page"))));
    Ok(RepositoryPage {
        items: values,
        next_cursor,
    })
}

impl Backend for TursoCatalog {
    fn db(&self) -> &Connection {
        self.connection()
    }
    fn is_transaction(&self) -> bool {
        false
    }
}
impl_repositories!(TursoCatalog);

impl Backend for CatalogTransaction {
    fn db(&self) -> &Connection {
        &self.connection
    }
    fn is_transaction(&self) -> bool {
        true
    }
}
impl_repositories!(CatalogTransaction);

impl TursoCatalog {
    pub async fn get_refresh_token_metadata(
        &self,
        source_id: Uuid,
    ) -> RepositoryResult<Option<RefreshTokenMetadata>> {
        let mut rows = self.connection().query("SELECT credential_reference,refresh_generation,last_refreshed_at,expires_at,updated_at FROM source_auth_metadata WHERE source_id=?1",[source_id.to_string()]).await.map_err(database_error)?;
        let Some(row) = rows.next().await.map_err(database_error)? else {
            return Ok(None);
        };
        let parse = |value: Option<String>| -> RepositoryResult<Option<DateTime<Utc>>> {
            value
                .map(|value| {
                    DateTime::parse_from_rfc3339(&value)
                        .map(|v| v.with_timezone(&Utc))
                        .map_err(|e| invalid_data(format!("invalid auth metadata timestamp: {e}")))
                })
                .transpose()
        };
        Ok(Some(RefreshTokenMetadata {
            source_id,
            credential_reference: row.get(0).map_err(database_error)?,
            refresh_generation: u64::try_from(row.get::<i64>(1).map_err(database_error)?)
                .map_err(|_| invalid_data("negative refresh generation"))?,
            last_refreshed_at: parse(row.get(2).map_err(database_error)?)?,
            expires_at: parse(row.get(3).map_err(database_error)?)?,
            updated_at: parse(Some(row.get(4).map_err(database_error)?))?.unwrap(),
        }))
    }

    pub async fn set_refresh_token_metadata(
        &self,
        value: RefreshTokenMetadata,
    ) -> RepositoryResult<()> {
        validate_credential_reference(&value.credential_reference)?;
        self.connection().execute("INSERT INTO source_auth_metadata(source_id,credential_reference,refresh_generation,last_refreshed_at,expires_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(source_id) DO UPDATE SET credential_reference=excluded.credential_reference,refresh_generation=excluded.refresh_generation,last_refreshed_at=excluded.last_refreshed_at,expires_at=excluded.expires_at,updated_at=excluded.updated_at",(value.source_id.to_string(),value.credential_reference,value.refresh_generation as i64,value.last_refreshed_at.map(|v|v.to_rfc3339()),value.expires_at.map(|v|v.to_rfc3339()),value.updated_at.to_rfc3339())).await.map_err(database_error)?;
        Ok(())
    }

    /// Atomically applies one source page, advances its run checkpoint, and optionally
    /// records non-secret credential-store metadata. Token values are not accepted.
    pub async fn apply_source_sync_page(
        &self,
        occurrences: Vec<SourceTrackOccurrence>,
        run: SyncRun,
        auth: Option<RefreshTokenMetadata>,
    ) -> RepositoryResult<()> {
        let transaction = self.begin_catalog_transaction().await?;
        transaction
            .sources()
            .upsert_occurrences(occurrences)
            .await?;
        transaction.sync_runs().upsert(run).await?;
        if let Some(value) = auth {
            validate_credential_reference(&value.credential_reference)?;
            set_auth_metadata(transaction.as_ref(), value).await?;
        }
        transaction.commit().await
    }

    pub async fn export_snapshot(&self) -> RepositoryResult<CatalogSnapshot> {
        let document=SnapshotDocument{
            format_version:SNAPSHOT_FORMAT_VERSION,exported_at:Utc::now(),
            tracks:list_payloads(self.connection(),"SELECT payload FROM tracks ORDER BY id").await?,
            playlists:list_payloads(self.connection(),"SELECT payload FROM canonical_playlists ORDER BY id").await?,
            sources:list_payloads(self.connection(),"SELECT payload FROM catalog_sources ORDER BY id").await?,
            source_accounts:list_payloads(self.connection(),"SELECT payload FROM source_accounts ORDER BY source_id,external_id").await?,
            source_relationships:list_payloads(self.connection(),"SELECT payload FROM source_collection_relationships ORDER BY identity_key").await?,
            source_occurrences:list_payloads(self.connection(),"SELECT payload FROM source_track_occurrences ORDER BY source_id,external_id").await?,
            external_playlists:list_payloads(self.connection(),"SELECT payload FROM external_playlists ORDER BY source_id,external_id").await?,
            music_storages:list_payloads(self.connection(),"SELECT payload FROM music_storages ORDER BY id").await?,
            original_assets:list_payloads(self.connection(),"SELECT payload FROM original_assets ORDER BY id").await?,
            derived_assets:list_payloads(self.connection(),"SELECT payload FROM derived_assets ORDER BY id").await?,
            storage_objects:list_payloads(self.connection(),"SELECT payload FROM storage_objects WHERE is_current=1 ORDER BY storage_id,object_id").await?,
            inventory_generations:list_payloads(self.connection(),"SELECT payload FROM inventory_generations WHERE status!='staging' ORDER BY id").await?,
            storage_object_tombstones:list_payloads(self.connection(),"SELECT payload FROM storage_object_tombstones ORDER BY storage_id,object_id,generation_id").await?,
            asset_locations:list_payloads(self.connection(),"SELECT payload FROM asset_locations ORDER BY id").await?,
            match_candidates:list_payloads(self.connection(),"SELECT payload FROM match_candidates ORDER BY id").await?,
            reconciliation_results:list_payloads(self.connection(),"SELECT payload FROM reconciliation_results ORDER BY id").await?,
            sync_runs:list_payloads(self.connection(),"SELECT payload FROM sync_runs ORDER BY id").await?,
            sync_jobs:list_payloads(self.connection(),"SELECT payload FROM sync_jobs ORDER BY id").await?,
            job_events:list_payloads(self.connection(),"SELECT payload FROM sync_job_events ORDER BY at,id").await?,
            settings:list_payloads(self.connection(),"SELECT payload FROM settings ORDER BY key").await?,
            refresh_token_metadata:list_auth_metadata(self.connection()).await?,
        };
        let snapshot = CatalogSnapshot::new(
            SNAPSHOT_FORMAT_VERSION,
            SNAPSHOT_CONTENT_TYPE,
            encode(&document)?.into_bytes(),
        );
        self.put(snapshot.clone()).await?;
        Ok(snapshot)
    }

    pub async fn import_snapshot(&self, snapshot: &CatalogSnapshot) -> RepositoryResult<()> {
        if !snapshot.verify_integrity() {
            return Err(invalid_data("catalog snapshot integrity check failed"));
        }
        if snapshot.format_version != SNAPSHOT_FORMAT_VERSION
            || snapshot.content_type != SNAPSHOT_CONTENT_TYPE
        {
            return Err(invalid_data("unsupported catalog snapshot format"));
        }
        let document: SnapshotDocument = serde_json::from_slice(snapshot.payload())
            .map_err(|e| invalid_data(format!("invalid snapshot document: {e}")))?;
        if document.format_version != SNAPSHOT_FORMAT_VERSION {
            return Err(invalid_data("snapshot document version mismatch"));
        }
        let transaction = self.begin_catalog_transaction().await?;
        let result = async {
            for value in document.tracks {
                transaction.tracks().upsert(value).await?;
            }
            for value in document.playlists {
                transaction.playlists().upsert(value).await?;
            }
            for value in document.sources {
                transaction.sources().upsert(value).await?;
            }
            for value in document.source_accounts {
                transaction.sources().upsert_account(value).await?;
            }
            for value in document.source_relationships {
                transaction
                    .sources()
                    .upsert_collection_relationship(value)
                    .await?;
            }
            transaction
                .sources()
                .upsert_occurrences(document.source_occurrences)
                .await?;
            for value in document.external_playlists {
                transaction.playlists().upsert_external(value).await?;
            }
            for value in document.music_storages {
                transaction.music_storages().upsert(value).await?;
            }
            for value in document.original_assets {
                transaction.assets().upsert_original(value).await?;
            }
            for value in document.derived_assets {
                transaction.assets().upsert_derived(value).await?;
            }
            for value in document.storage_objects {
                transaction.assets().upsert_storage_object(value).await?;
            }
            for value in document.asset_locations {
                transaction.assets().upsert_location(value).await?;
            }
            for value in document.match_candidates {
                transaction.reconciliation().upsert_candidate(value).await?;
            }
            for value in document.reconciliation_results {
                transaction.reconciliation().upsert_result(value).await?;
            }
            for value in document.sync_runs {
                transaction.sync_runs().upsert(value).await?;
            }
            for value in document.sync_jobs {
                transaction.sync_runs().upsert_job(value).await?;
            }
            for value in document.inventory_generations {
                transaction.db().execute("INSERT INTO inventory_generations(id,storage_id,run_id,kind,status,started_at,finished_at,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET storage_id=excluded.storage_id,run_id=excluded.run_id,kind=excluded.kind,status=excluded.status,started_at=excluded.started_at,finished_at=excluded.finished_at,payload=excluded.payload",(value.id.to_string(),value.storage_id.to_string(),value.run_id.to_string(),enum_key(&value.kind)?,enum_key(&value.status)?,value.started_at.to_rfc3339(),value.finished_at.map(|v|v.to_rfc3339()),encode(&value)?)).await.map_err(database_error)?;
            }
            for value in document.storage_object_tombstones {
                transaction.db().execute("INSERT INTO storage_object_tombstones(storage_id,object_id,generation_id,payload) VALUES(?1,?2,?3,?4) ON CONFLICT(storage_id,object_id,generation_id) DO UPDATE SET payload=excluded.payload",(value.storage_id.to_string(),value.object_id.clone(),value.generation_id.to_string(),encode(&value)?)).await.map_err(database_error)?;
            }
            for value in document.job_events {
                append_job_event(transaction.as_ref(), value).await?;
            }
            for value in document.settings {
                transaction.settings().set(value).await?;
            }
            for value in document.refresh_token_metadata {
                set_auth_metadata(transaction.as_ref(), value).await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => transaction.commit().await,
            Err(error) => {
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    }
}

fn validate_credential_reference(value: &str) -> RepositoryResult<()> {
    let lower = value.to_ascii_lowercase();
    if value.trim().is_empty()
        || lower.starts_with("bearer ")
        || lower.contains("refresh_token=")
        || lower.contains("access_token=")
    {
        return Err(invalid_data(
            "credential reference appears to contain a secret",
        ));
    }
    Ok(())
}
async fn set_auth_metadata(
    backend: &dyn Backend,
    value: RefreshTokenMetadata,
) -> RepositoryResult<()> {
    validate_credential_reference(&value.credential_reference)?;
    backend.db().execute("INSERT INTO source_auth_metadata(source_id,credential_reference,refresh_generation,last_refreshed_at,expires_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(source_id) DO UPDATE SET credential_reference=excluded.credential_reference,refresh_generation=excluded.refresh_generation,last_refreshed_at=excluded.last_refreshed_at,expires_at=excluded.expires_at,updated_at=excluded.updated_at",(value.source_id.to_string(),value.credential_reference,value.refresh_generation as i64,value.last_refreshed_at.map(|v|v.to_rfc3339()),value.expires_at.map(|v|v.to_rfc3339()),value.updated_at.to_rfc3339())).await.map_err(database_error)?;
    Ok(())
}
fn job_store_error(error: RepositoryError) -> rec_sync::JobStoreError {
    match error.kind {
        RepositoryErrorKind::TemporarilyUnavailable | RepositoryErrorKind::Io => {
            rec_sync::JobStoreError::TemporarilyUnavailable(error.message)
        }
        RepositoryErrorKind::NotFound => rec_sync::JobStoreError::NotFound(error.message),
        RepositoryErrorKind::InvalidData
        | RepositoryErrorKind::Conflict
        | RepositoryErrorKind::ConstraintViolation => {
            rec_sync::JobStoreError::InvalidState(error.message)
        }
        _ => rec_sync::JobStoreError::Permanent(error.message),
    }
}

impl rec_sync::JobStore for TursoCatalog {
    fn create_run_with_jobs(
        &self,
        run: SyncRun,
        jobs: Vec<SyncJob>,
    ) -> BoxFuture<'_, Result<(), rec_sync::JobStoreError>> {
        Box::pin(async move {
            if jobs.iter().any(|job| job.run_id != run.id) {
                return Err(rec_sync::JobStoreError::InvalidState(
                    "job plan contains a mismatched run ID".into(),
                ));
            }
            let transaction = self
                .begin_catalog_transaction()
                .await
                .map_err(job_store_error)?;
            let result = async {
                SyncRunRepository::upsert(transaction.as_ref(), run).await?;
                for job in jobs {
                    transaction.upsert_job(job).await?;
                }
                Ok::<(), RepositoryError>(())
            }
            .await;
            match result {
                Ok(()) => transaction.commit().await.map_err(job_store_error),
                Err(error) => {
                    let _ = transaction.rollback().await;
                    Err(job_store_error(error))
                }
            }
        })
    }

    fn claim_due(
        &self,
        worker_id: String,
        claimed_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Option<SyncJob>, rec_sync::JobStoreError>> {
        Box::pin(async move {
            let transaction = self
                .begin_catalog_transaction()
                .await
                .map_err(job_store_error)?;
            let result = async {
                let mut rows = transaction.db().query(
                    "SELECT payload FROM sync_jobs WHERE status IN ('pending','paused','running') ORDER BY COALESCE(next_attempt_at,created_at),id",
                    (),
                ).await.map_err(database_error)?;
                let mut selected = None;
                while let Some(row) = rows.next().await.map_err(database_error)? {
                    let mut job: SyncJob = decode(row.get::<String>(0).map_err(database_error)?)?;
                    if job.claim_at(worker_id.clone(), claimed_at, lease_expires_at) {
                        selected = Some(job);
                        break;
                    }
                }
                drop(rows);
                if let Some(job) = &selected {
                    transaction.upsert_job(job.clone()).await?;
                }
                Ok::<_, RepositoryError>(selected)
            }.await;
            match result {
                Ok(job) => {
                    transaction.commit().await.map_err(job_store_error)?;
                    Ok(job)
                }
                Err(error) => {
                    let _ = transaction.rollback().await;
                    Err(job_store_error(error))
                }
            }
        })
    }

    fn load_run(
        &self,
        run_id: Uuid,
    ) -> BoxFuture<'_, Result<Option<SyncRun>, rec_sync::JobStoreError>> {
        Box::pin(async move {
            SyncRunRepository::get(self, run_id)
                .await
                .map_err(job_store_error)
        })
    }

    fn load_job(
        &self,
        job_id: Uuid,
    ) -> BoxFuture<'_, Result<Option<SyncJob>, rec_sync::JobStoreError>> {
        Box::pin(async move { self.get_job(job_id).await.map_err(job_store_error) })
    }

    fn list_run_jobs(
        &self,
        run_id: Uuid,
    ) -> BoxFuture<'_, Result<Vec<SyncJob>, rec_sync::JobStoreError>> {
        Box::pin(async move { self.list_jobs(run_id).await.map_err(job_store_error) })
    }

    fn save_run(&self, run: SyncRun) -> BoxFuture<'_, Result<(), rec_sync::JobStoreError>> {
        Box::pin(async move {
            SyncRunRepository::upsert(self, run)
                .await
                .map_err(job_store_error)
        })
    }

    fn save_job_and_event(
        &self,
        job: SyncJob,
        event: rec_sync::JobEvent,
    ) -> BoxFuture<'_, Result<(), rec_sync::JobStoreError>> {
        Box::pin(async move {
            if event.run_id != job.run_id || event.job_id.is_some_and(|id| id != job.id) {
                return Err(rec_sync::JobStoreError::InvalidState(
                    "job event scope does not match job".into(),
                ));
            }
            let transaction = self
                .begin_catalog_transaction()
                .await
                .map_err(job_store_error)?;
            let result = async {
                transaction.upsert_job(job).await?;
                append_job_event(transaction.as_ref(), event).await
            }
            .await;
            match result {
                Ok(()) => transaction.commit().await.map_err(job_store_error),
                Err(error) => {
                    let _ = transaction.rollback().await;
                    Err(job_store_error(error))
                }
            }
        })
    }

    fn append_event(
        &self,
        event: rec_sync::JobEvent,
    ) -> BoxFuture<'_, Result<(), rec_sync::JobStoreError>> {
        Box::pin(async move { append_job_event(self, event).await.map_err(job_store_error) })
    }

    fn list_events(
        &self,
        run_id: Uuid,
    ) -> BoxFuture<'_, Result<Vec<rec_sync::JobEvent>, rec_sync::JobStoreError>> {
        Box::pin(async move {
            let sql = format!(
                "SELECT payload FROM sync_job_events WHERE run_id='{}' ORDER BY at,id",
                run_id
            );
            list_payloads(self.connection(), &sql)
                .await
                .map_err(job_store_error)
        })
    }

    fn request_cancellation(
        &self,
        run_id: Uuid,
        requested_at: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<SyncRun, rec_sync::JobStoreError>> {
        Box::pin(async move {
            let transaction = self
                .begin_catalog_transaction()
                .await
                .map_err(job_store_error)?;
            let result = async {
                let mut run = SyncRunRepository::get(transaction.as_ref(), run_id)
                    .await?
                    .ok_or_else(|| {
                        RepositoryError::new(
                            RepositoryErrorKind::NotFound,
                            "sync run does not exist",
                        )
                    })?;
                if run.status != SyncStatus::Cancelling && !run.status.is_terminal() {
                    run.request_cancellation_at(requested_at)
                        .map_err(|error| invalid_data(error.to_string()))?;
                    SyncRunRepository::upsert(transaction.as_ref(), run.clone()).await?;
                    append_job_event(
                        transaction.as_ref(),
                        rec_sync::JobEvent {
                            id: Uuid::new_v4(),
                            job_id: None,
                            run_id,
                            kind: rec_sync::JobEventKind::CancellationRequested,
                            at: requested_at,
                            message: None,
                            progress: Some(run.progress),
                            issue: None,
                        },
                    )
                    .await?;
                }
                Ok::<_, RepositoryError>(run)
            }
            .await;
            match result {
                Ok(run) => {
                    transaction.commit().await.map_err(job_store_error)?;
                    Ok(run)
                }
                Err(error) => {
                    let _ = transaction.rollback().await;
                    Err(job_store_error(error))
                }
            }
        })
    }
}

async fn append_job_event(
    backend: &dyn Backend,
    event: rec_sync::JobEvent,
) -> RepositoryResult<()> {
    backend.db().execute("INSERT INTO sync_job_events(id,run_id,job_id,at,payload) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO NOTHING",(event.id.to_string(),event.run_id.to_string(),event.job_id.map(|id|id.to_string()),event.at.to_rfc3339(),encode(&event)?)).await.map_err(database_error)?;
    Ok(())
}

async fn list_auth_metadata(
    connection: &Connection,
) -> RepositoryResult<Vec<RefreshTokenMetadata>> {
    let mut rows=connection.query("SELECT source_id,credential_reference,refresh_generation,last_refreshed_at,expires_at,updated_at FROM source_auth_metadata ORDER BY source_id",()).await.map_err(database_error)?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().await.map_err(database_error)? {
        let parse = |value: Option<String>| -> RepositoryResult<Option<DateTime<Utc>>> {
            value
                .map(|v| {
                    DateTime::parse_from_rfc3339(&v)
                        .map(|v| v.with_timezone(&Utc))
                        .map_err(|e| invalid_data(e.to_string()))
                })
                .transpose()
        };
        values.push(RefreshTokenMetadata {
            source_id: Uuid::parse_str(&row.get::<String>(0).map_err(database_error)?)
                .map_err(|e| invalid_data(e.to_string()))?,
            credential_reference: row.get(1).map_err(database_error)?,
            refresh_generation: row.get::<i64>(2).map_err(database_error)? as u64,
            last_refreshed_at: parse(row.get(3).map_err(database_error)?)?,
            expires_at: parse(row.get(4).map_err(database_error)?)?,
            updated_at: parse(Some(row.get(5).map_err(database_error)?))?.unwrap(),
        });
    }
    Ok(values)
}
