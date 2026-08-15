use rec_core::{BoxFuture, MusicStorageError, MusicStorageErrorKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;

/// Durable state needed to continue a Google Drive resumable upload.
///
/// The session URI is a capability URL and must be protected like a credential. Its
/// value is deliberately redacted from `Debug` output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveUploadCheckpoint {
    pub operation_key: String,
    pub object_path: String,
    pub source_size: u64,
    pub source_sha256: String,
    pub acknowledged_bytes: u64,
    session_uri: String,
}

impl DriveUploadCheckpoint {
    pub(crate) fn new(
        operation_key: String,
        object_path: String,
        source_size: u64,
        source_sha256: String,
        session_uri: String,
    ) -> Self {
        Self {
            operation_key,
            object_path,
            source_size,
            source_sha256,
            acknowledged_bytes: 0,
            session_uri,
        }
    }

    pub(crate) fn session_uri(&self) -> &str {
        &self.session_uri
    }
}

impl fmt::Debug for DriveUploadCheckpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DriveUploadCheckpoint")
            .field("operation_key", &self.operation_key)
            .field("object_path", &self.object_path)
            .field("source_size", &self.source_size)
            .field("source_sha256", &self.source_sha256)
            .field("acknowledged_bytes", &self.acknowledged_bytes)
            .field("session_uri", &"[REDACTED]")
            .finish()
    }
}

/// Persistence seam for resumable-upload state.
///
/// Implementations must store session URIs securely and durably before `save` returns.
/// A desktop registry can provide its own encrypted implementation; the file-backed
/// implementation below is suitable when its directory has application-private access.
pub trait DriveUploadCheckpointStore: Send + Sync {
    fn load<'a>(
        &'a self,
        operation_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<DriveUploadCheckpoint>, MusicStorageError>>;

    fn save<'a>(
        &'a self,
        checkpoint: &'a DriveUploadCheckpoint,
    ) -> BoxFuture<'a, Result<(), MusicStorageError>>;

    fn remove<'a>(&'a self, operation_key: &'a str)
        -> BoxFuture<'a, Result<(), MusicStorageError>>;
}

/// Atomic JSON checkpoint persistence rooted in an application-private directory.
#[derive(Debug, Clone)]
pub struct FileDriveUploadCheckpointStore {
    directory: PathBuf,
}

impl FileDriveUploadCheckpointStore {
    pub fn new(directory: impl Into<PathBuf>) -> Result<Self, MusicStorageError> {
        let directory = directory.into();
        fs::create_dir_all(&directory).map_err(io_error)?;
        let metadata = fs::symlink_metadata(&directory).map_err(io_error)?;
        if !metadata.file_type().is_dir() {
            return Err(MusicStorageError::new(
                MusicStorageErrorKind::InvalidData,
                "Drive upload checkpoint directory is not a real directory",
            ));
        }
        secure_directory(&directory)?;
        for entry in fs::read_dir(&directory).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("upload-") && name.ends_with(".json"))
            {
                secure_file(&path)?;
            }
        }
        Ok(Self { directory })
    }

    fn path(&self, operation_key: &str) -> PathBuf {
        let digest = Sha256::digest(operation_key.as_bytes());
        self.directory.join(format!("upload-{digest:x}.json"))
    }
}

impl DriveUploadCheckpointStore for FileDriveUploadCheckpointStore {
    fn load<'a>(
        &'a self,
        operation_key: &'a str,
    ) -> BoxFuture<'a, Result<Option<DriveUploadCheckpoint>, MusicStorageError>> {
        Box::pin(async move {
            let path = self.path(operation_key);
            let Some(bytes) = read_bounded_checkpoint(&path)? else {
                return Ok(None);
            };
            serde_json::from_slice(&bytes).map(Some).map_err(|_| {
                MusicStorageError::new(
                    MusicStorageErrorKind::InvalidData,
                    "Drive upload checkpoint is corrupt",
                )
            })
        })
    }

    fn save<'a>(
        &'a self,
        checkpoint: &'a DriveUploadCheckpoint,
    ) -> BoxFuture<'a, Result<(), MusicStorageError>> {
        Box::pin(async move {
            let path = self.path(&checkpoint.operation_key);
            let temporary = self
                .directory
                .join(format!(".upload-{}.tmp", Uuid::new_v4()));
            let result = (|| {
                let bytes = serde_json::to_vec(checkpoint).map_err(|error| {
                    MusicStorageError::new(MusicStorageErrorKind::InvalidData, error.to_string())
                })?;
                if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
                    return Err(MusicStorageError::new(
                        MusicStorageErrorKind::InvalidData,
                        "Drive upload checkpoint exceeds the size limit",
                    ));
                }
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&temporary).map_err(io_error)?;
                file.write_all(&bytes).map_err(io_error)?;
                file.sync_all().map_err(io_error)?;
                fs::rename(&temporary, path).map_err(io_error)?;
                sync_directory(&self.directory)
            })();
            if result.is_err() {
                let _ = fs::remove_file(temporary);
            }
            result
        })
    }

    fn remove<'a>(
        &'a self,
        operation_key: &'a str,
    ) -> BoxFuture<'a, Result<(), MusicStorageError>> {
        Box::pin(async move {
            match fs::remove_file(self.path(operation_key)) {
                Ok(()) => sync_directory(&self.directory),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error(error)),
            }
        })
    }
}

fn read_bounded_checkpoint(path: &Path) -> Result<Option<Vec<u8>>, MusicStorageError> {
    secure_file(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_CHECKPOINT_BYTES {
        return Err(MusicStorageError::new(
            MusicStorageErrorKind::InvalidData,
            "Drive upload checkpoint exceeds the size limit",
        ));
    }
    Ok(Some(bytes))
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), MusicStorageError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error)
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), MusicStorageError> {
    Ok(())
}

fn checkpoint_file_exists_safely(path: &Path) -> Result<bool, MusicStorageError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(MusicStorageError::new(
            MusicStorageErrorKind::InvalidData,
            "Drive upload checkpoint path is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), MusicStorageError> {
    use std::os::unix::fs::PermissionsExt;
    if checkpoint_file_exists_safely(path)? {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_file(path: &Path) -> Result<(), MusicStorageError> {
    checkpoint_file_exists_safely(path).map(|_| ())
}

fn sync_directory(path: &Path) -> Result<(), MusicStorageError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)?;
    Ok(())
}

fn io_error(error: std::io::Error) -> MusicStorageError {
    MusicStorageError::new(MusicStorageErrorKind::Io, error.to_string())
}
