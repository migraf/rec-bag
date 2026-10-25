//! The one error type every music storage workflow returns.

use rec_core::{HealthStatus, MusicStorageError, MusicStorageKind, RepositoryError, StorageRole};
use thiserror::Error;
use uuid::Uuid;

use crate::roots::RootLocator;

/// Why a music storage workflow did not complete.
///
/// Drivers report their own failures as [`StorageError::Driver`] with text that
/// the desktop's safe failure messages still recognise (for example "local root
/// is unavailable" or "folder chooser"), so the kind of storage never has to be
/// inspected to explain an error.
#[derive(Debug, Clone, Error)]
pub enum StorageError {
    #[error("no active primary is configured")]
    NoPrimary,
    #[error("an active primary already exists; use primary migration (active primary {id})")]
    PrimaryExists { id: Uuid },
    #[error("storage {id} is the active primary; migrate to another primary first")]
    PrimaryInUse { id: Uuid },
    #[error("storage {id} is already configured for this root")]
    AlreadyConfigured { id: Uuid },
    #[error("storage was not found: {0}")]
    StorageNotFound(Uuid),
    #[error("track was not found: {0}")]
    TrackNotFound(Uuid),
    #[error("no storage driver is registered for {0:?}")]
    NoDriver(MusicStorageKind),
    /// The adapter's health probe did not report `Healthy`; `message` is the
    /// adapter's own text, such as "local storage is read-only".
    #[error("{message}")]
    Unhealthy {
        status: HealthStatus,
        message: String,
    },
    #[error("storage cannot act as {role:?}: it cannot {missing}")]
    MissingCapability {
        role: StorageRole,
        missing: &'static str,
    },
    #[error(
        "storage and source roots must be distinct and non-nested: {candidate} and {existing}"
    )]
    RootsOverlap {
        candidate: RootLocator,
        existing: RootLocator,
    },
    #[error("{}", primary_unavailable_message(*backup_has_copy))]
    PrimaryUnavailable { backup_has_copy: bool },
    #[error("no catalog source or storage can supply track {0}")]
    NoEligibleSource(Uuid),
    #[error("copy requires explicit confirmation")]
    CopyConfirmationRequired,
    #[error("source policy forbids copying")]
    CopyForbidden,
    #[error("primary migration cannot switch while {0} asset(s) remain unverified")]
    MigrationIncomplete(u64),
    #[error("invalid storage target: {0}")]
    InvalidTarget(String),
    #[error("asset verification failed: {0}")]
    Verification(String),
    #[error("storage setup was cancelled")]
    Cancelled,
    #[error("{0}")]
    Driver(String),
    #[error("music storage operation failed: {0}")]
    Provider(#[from] MusicStorageError),
    #[error("catalog repository error: {0}")]
    Repository(#[from] RepositoryError),
    #[error("filesystem operation failed: {0}")]
    Io(String),
}

impl StorageError {
    /// Whether repeating the same request later may succeed without user action.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Provider(error) => error.is_retryable(),
            Self::Repository(error) => error.is_retryable(),
            Self::Unhealthy { status, .. } => *status == HealthStatus::Unavailable,
            Self::Io(_) => true,
            _ => false,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

fn primary_unavailable_message(backup_has_copy: bool) -> &'static str {
    if backup_has_copy {
        "the primary storage cannot serve this track; restore it from backup"
    } else {
        "the primary storage cannot serve this track"
    }
}
