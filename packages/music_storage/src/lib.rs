//! Provider-neutral music storage workflows.
//!
//! Every storage workflow lives here once, for every provider. A provider
//! contributes a [`MusicAssetStore`](rec_core::MusicAssetStore) adapter and a
//! small [`StorageDriver`] that provisions, opens and describes its storages;
//! the storage kind only selects the driver.

pub mod catalog_io;
mod copy;
mod driver;
mod error;
mod handoff;
mod inventory;
mod lifecycle;
mod migration;
mod offline_cache;
mod playback;
mod projection;
mod reconcile;
mod roots;
mod settings;
mod storages;
mod stores;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use copy::{Copied, CopyAuthorization, CopyOrigin};
pub use driver::{ready_ok, Provisioned, StorageDriver, StorageDrivers, StorageSetup};
pub use error::StorageError;
pub use handoff::{HandoffReport, NoHandoff, ReplicationHandoff};
pub use inventory::{Refreshed, Target};
pub use lifecycle::{Release, StorageSettings};
pub use migration::{MigrationOptions, MigrationOutcome};
pub use playback::{Playable, PlayableOrigin};
pub use projection::{StorageProjection, StorageView};
pub use roots::{source_root, RootLocator, RootPurpose};
pub use storages::{
    is_migrated_synthetic_storage, MusicStorages, MusicStoragesConfig, StorageFault,
};
pub use stores::LiveStores;

#[cfg(test)]
mod tests;
