//! Desktop integration services.

pub mod integration;

// Disjoint ownership boundaries for Stage 3 vertical slices.
pub mod backup;
pub mod catalog;
pub mod catalog_projection;
pub mod google_drive;
pub mod local_library;
pub mod playback_bridge;
pub mod profile_manager;
pub mod reconciliation_actions;
pub mod recovery;
pub mod scheduler;
pub mod soundcloud_catalog;
