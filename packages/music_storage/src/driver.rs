//! What a storage provider contributes: provisioning, opening and describing
//! its storages. Everything else is provider-neutral and lives in this crate.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rec_core::{BoxFuture, MusicAssetStore, MusicStorage, MusicStorageKind};

use crate::error::StorageError;
use crate::roots::RootLocator;

/// What the user asked for when connecting a storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageSetup {
    /// Display name; drivers may suggest one when this is `None`.
    pub name: Option<String>,
    /// Provider-native location the user typed or the caller fixed (a host
    /// directory, a Drive folder link). `None` lets the driver ask.
    pub location: Option<String>,
    pub organize_by_artist: bool,
    pub verify_after_copy: bool,
}

impl Default for StorageSetup {
    fn default() -> Self {
        Self {
            name: None,
            location: None,
            organize_by_artist: false,
            verify_after_copy: true,
        }
    }
}

/// A root a driver prepared (and authorised) for a new storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provisioned {
    /// Provider-native root locator, never a credential.
    pub root_identifier: String,
    /// A name to use when [`StorageSetup::name`] is empty.
    pub suggested_name: Option<String>,
    /// Driver-owned, non-secret configuration persisted with the storage.
    pub config: BTreeMap<String, String>,
}

/// One provider's storage driver.
///
/// The storage kind is used only to find a driver; behaviour and capabilities
/// come from the opened store's health probe.
pub trait StorageDriver: Send + Sync {
    /// The kind of storage this driver provisions and opens.
    fn kind(&self) -> MusicStorageKind;

    /// Prepares a root for a new storage, including any authorisation or
    /// folder selection the provider needs. Nothing is persisted yet.
    fn provision<'a>(
        &'a self,
        setup: &'a StorageSetup,
    ) -> BoxFuture<'a, Result<Provisioned, StorageError>>;

    /// Opens the adapter for a (possibly not yet persisted) storage.
    fn open<'a>(
        &'a self,
        storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<Arc<dyn MusicAssetStore>, StorageError>>;

    /// Where the storage is rooted, for root separation.
    fn root(&self, storage: &MusicStorage) -> Option<RootLocator>;

    /// A host path that plays `object_id` without copying, when the storage
    /// keeps its objects on the host.
    fn direct_path(&self, _storage: &MusicStorage, _object_id: &str) -> Option<PathBuf> {
        None
    }

    /// The authorisation grant the storage uses, when it has one. Storages
    /// with the same grant share its lifetime.
    fn grant_id(&self, _storage: &MusicStorage) -> Option<String> {
        None
    }

    /// Gives up the storage's grant (revokes and forgets its credential).
    fn release_grant<'a>(
        &'a self,
        _storage: &'a MusicStorage,
    ) -> BoxFuture<'a, Result<(), StorageError>> {
        ready_ok()
    }
}

/// An already-completed successful future, for drivers with nothing to do.
pub fn ready_ok<'a>() -> BoxFuture<'a, Result<(), StorageError>> {
    Box::pin(std::future::ready(Ok(())))
}

/// The drivers available to one composition, looked up by storage kind.
#[derive(Clone, Default)]
pub struct StorageDrivers(Vec<Arc<dyn StorageDriver>>);

impl StorageDrivers {
    pub fn new(drivers: Vec<Arc<dyn StorageDriver>>) -> Self {
        Self(drivers)
    }

    /// The driver for `kind`, if one is registered.
    pub fn get(&self, kind: &MusicStorageKind) -> Option<&Arc<dyn StorageDriver>> {
        self.0.iter().find(|driver| &driver.kind() == kind)
    }
}
