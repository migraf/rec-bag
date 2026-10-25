//! Catalog settings the module keeps for a storage.
//!
//! Every key lives under one provider-neutral prefix per storage, so forgetting
//! a storage can delete all of them with one prefix match.

use uuid::Uuid;

/// The settings key `suffix` for `storage_id`: `storage.{id}.{suffix}`.
pub(crate) fn storage_key(storage_id: Uuid, suffix: &str) -> String {
    format!("storage.{storage_id}.{suffix}")
}
