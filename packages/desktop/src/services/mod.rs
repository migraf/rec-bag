//! Desktop integration services.

pub mod integration;

// Disjoint ownership boundaries for Stage 3 vertical slices.
pub mod backup;
pub mod catalog;
pub mod catalog_projection;
#[cfg(feature = "desktop")]
pub mod credentials;
pub mod local_library;
pub mod music_storage_slice;
pub mod playback_bridge;
pub mod profile_manager;
pub mod reconciliation_actions;
pub mod scheduler;
pub mod soundcloud_catalog;
pub mod storage_drivers;
pub mod stream_cache;
