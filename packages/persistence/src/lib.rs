//! SQLite-compatible Turso persistence for the provider-independent catalog.
//!
//! Provider credentials are deliberately out of scope. Only opaque credential-store
//! references and refresh timestamps may be persisted here.

mod migrations;
mod repositories;

pub use migrations::{migrate, MigrationError, MigrationOutcome};
pub use repositories::{CleanupApproval, DurableTransferResult, RefreshTokenMetadata};

use rec_core::{
    AssetRepository, LibraryEntryRepository, MusicStorageRepository, PlaylistRepository,
    ReconciliationRepository, RepositoryResult, RepositoryTransaction, SettingsRepository,
    SourceRepository, SyncRunRepository, TrackAnalysisRepository, TrackRepository,
    TransactionManager,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use turso::{Builder, Connection, Database};

/// Exclusive use of the catalog for the duration of one operation.
///
/// Turso rejects two overlapping uses of the same connection outright — the SDK reports
/// `concurrent use forbidden` — and a `BEGIN IMMEDIATE` block spanning several statements
/// is only atomic while nothing else is talking to that connection. The catalog is a
/// single-writer local store, so rather than racing, every operation takes this gate and
/// the rest queue behind it.
pub(crate) type CatalogGate = OwnedMutexGuard<()>;

pub struct TursoCatalog {
    database: Database,
    connection: Connection,
    /// Held for the length of one catalog operation, or one transaction. See [`CatalogGate`].
    gate: Arc<Mutex<()>>,
    path: Option<PathBuf>,
}

pub(crate) struct CatalogTransaction {
    connection: Connection,
    /// Released when the transaction commits, rolls back, or is dropped uncommitted.
    _gate: CatalogGate,
}

fn copy_error(error: std::io::Error) -> rec_core::RepositoryError {
    rec_core::RepositoryError::new(
        rec_core::RepositoryErrorKind::TemporarilyUnavailable,
        format!("could not copy the catalog database: {error}"),
    )
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
            gate: Arc::new(Mutex::new(())),
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
            gate: Arc::new(Mutex::new(())),
            path: None,
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Waits for exclusive use of the catalog. Every entry point that reaches
    /// [`Self::connection`] holds one of these for its whole body, so no two operations
    /// ever share the connection.
    ///
    /// The gate is not reentrant, and an operation that waits for a gate it already holds
    /// wedges the catalog for the life of the process. An operation holding the gate
    /// therefore reaches the database through the free helpers in `repositories`, never by
    /// calling a second repository method; `gated_operations_never_call_one_another` holds
    /// that line.
    pub(crate) async fn acquire(&self) -> CatalogGate {
        self.gate.clone().lock_owned().await
    }

    /// Writes a consistent copy of the catalog database to `destination`, returning its
    /// size in bytes.
    ///
    /// This is what a backup copies. Holding the gate keeps every writer out for the
    /// length of the copy, and truncating the WAL first makes the main file a complete
    /// database image rather than one half of a pair.
    pub async fn copy_database_to(&self, destination: &Path) -> RepositoryResult<u64> {
        let Some(path) = self.path.as_deref() else {
            return Err(rec_core::RepositoryError::new(
                rec_core::RepositoryErrorKind::InvalidData,
                "an in-memory catalog has no database file to copy",
            ));
        };
        let _gate = self.acquire().await;
        let mut checkpoint = self
            .connection
            .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
            .await
            .map_err(repositories::transaction_database_error)?;
        while checkpoint
            .next()
            .await
            .map_err(repositories::transaction_database_error)?
            .is_some()
        {}
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(copy_error)?;
        }
        std::fs::copy(path, destination).map_err(copy_error)
    }

    pub(crate) async fn begin_catalog_transaction(
        &self,
    ) -> RepositoryResult<Box<CatalogTransaction>> {
        let gate = self.acquire().await;
        let connection = self
            .database
            .connect()
            .map_err(repositories::transaction_database_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON; BEGIN IMMEDIATE;")
            .await
            .map_err(repositories::transaction_database_error)?;
        Ok(Box::new(CatalogTransaction {
            connection,
            _gate: gate,
        }))
    }
}

impl RepositoryTransaction for CatalogTransaction {
    fn tracks(&self) -> &dyn TrackRepository {
        self
    }
    fn library_entries(&self) -> &dyn LibraryEntryRepository {
        self
    }
    fn track_analysis(&self) -> &dyn TrackAnalysisRepository {
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
