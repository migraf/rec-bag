//! SQLite-compatible Turso persistence for the provider-independent catalog.
//!
//! Provider credentials are deliberately out of scope. Only opaque credential-store
//! references and refresh timestamps may be persisted here.

mod migrations;
mod repositories;

pub use migrations::{migrate, MigrationError, MigrationOutcome};
pub use repositories::{RefreshTokenMetadata, SNAPSHOT_CONTENT_TYPE, SNAPSHOT_FORMAT_VERSION};

use rec_core::{
    AssetRepository, CatalogSnapshotRepository, MusicStorageRepository, PlaylistRepository,
    ReconciliationRepository, RepositoryResult, RepositoryTransaction, SettingsRepository,
    SourceRepository, SyncRunRepository, TrackRepository, TransactionManager,
};
use std::path::{Path, PathBuf};
use thiserror::Error;
use turso::{Builder, Connection, Database};

pub struct TursoCatalog {
    database: Database,
    connection: Connection,
    path: Option<PathBuf>,
}

pub(crate) struct CatalogTransaction {
    connection: Connection,
}

#[derive(Debug, Error)]
pub enum OpenError {
    #[error("failed to create catalog directory: {0}")]
    CreateDirectory(#[from] std::io::Error),
    #[error("failed to open Turso catalog: {0}")]
    Database(#[from] turso::Error),
    #[error("failed to migrate catalog: {0}")]
    Migration(#[from] MigrationError),
}

impl TursoCatalog {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let database = Builder::new_local(&path.to_string_lossy()).build().await?;
        let connection = database.connect()?;
        migrate(&connection, Some(&path)).await?;
        Ok(Self {
            database,
            connection,
            path: Some(path),
        })
    }

    pub async fn in_memory() -> Result<Self, OpenError> {
        let database = Builder::new_local(":memory:").build().await?;
        let connection = database.connect()?;
        migrate(&connection, None).await?;
        Ok(Self {
            database,
            connection,
            path: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub(crate) async fn begin_catalog_transaction(
        &self,
    ) -> RepositoryResult<Box<CatalogTransaction>> {
        let connection = self
            .database
            .connect()
            .map_err(repositories::transaction_database_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")
            .await
            .map_err(repositories::transaction_database_error)?;
        Ok(Box::new(CatalogTransaction { connection }))
    }
}

impl RepositoryTransaction for CatalogTransaction {
    fn tracks(&self) -> &dyn TrackRepository {
        self
    }
    fn playlists(&self) -> &dyn PlaylistRepository {
        self
    }
    fn sources(&self) -> &dyn SourceRepository {
        self
    }
    fn assets(&self) -> &dyn AssetRepository {
        self
    }
    fn reconciliation(&self) -> &dyn ReconciliationRepository {
        self
    }
    fn music_storages(&self) -> &dyn MusicStorageRepository {
        self
    }
    fn sync_runs(&self) -> &dyn SyncRunRepository {
        self
    }
    fn settings(&self) -> &dyn SettingsRepository {
        self
    }
    fn catalog_snapshots(&self) -> &dyn CatalogSnapshotRepository {
        self
    }

    fn commit<'a>(self: Box<Self>) -> rec_core::BoxFuture<'a, RepositoryResult<()>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            self.connection
                .execute_batch("COMMIT;")
                .await
                .map_err(repositories::transaction_database_error)?;
            Ok(())
        })
    }
    fn rollback<'a>(self: Box<Self>) -> rec_core::BoxFuture<'a, RepositoryResult<()>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            self.connection
                .execute_batch("ROLLBACK;")
                .await
                .map_err(repositories::transaction_database_error)?;
            Ok(())
        })
    }
}

impl TransactionManager for TursoCatalog {
    fn begin<'a>(
        &'a self,
    ) -> rec_core::BoxFuture<'a, RepositoryResult<Box<dyn RepositoryTransaction + 'a>>> {
        Box::pin(async move {
            Ok(self.begin_catalog_transaction().await? as Box<dyn RepositoryTransaction>)
        })
    }
}

#[cfg(test)]
mod tests;
