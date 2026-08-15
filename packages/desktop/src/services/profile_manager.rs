//! One-active-manager profile ownership.
//!
//! Ownership is represented by a small lease file in the profile directory. A separate,
//! short-lived guard file serializes every read/modify/write operation so stale takeover cannot
//! race a heartbeat. Lease replacement uses write-and-rename and release always compares the
//! session identifier before removing anything.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::task::JoinHandle;
use uuid::Uuid;

const LEASE_FILE: &str = "profile-manager.lease.json";
const OPERATION_GUARD_FILE: &str = "profile-manager.lease.guard";
const LEASE_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerIdentity {
    pub profile_id: String,
    pub device_id: String,
}

impl ManagerIdentity {
    pub fn new(profile_id: impl Into<String>, device_id: impl Into<String>) -> Self {
        Self {
            profile_id: profile_id.into(),
            device_id: device_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    pub format_version: u32,
    pub profile_id: String,
    pub device_id: String,
    pub session_id: Uuid,
    pub acquired_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl LeaseRecord {
    pub fn is_stale_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at <= now
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverReason {
    NoOwner,
    ExpiredLease,
    ConfirmedHandoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipLease {
    pub record: LeaseRecord,
    pub takeover_reason: TakeoverReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipStatus {
    Available,
    Owned(LeaseRecord),
    Stale(LeaseRecord),
    Corrupt(String),
}

/// A short-lived, owner-specific confirmation. It is bound to the observed live manager
/// (device and session), preventing confirmation from evicting a replacement owner while
/// allowing the current manager to renew its heartbeat.
#[derive(Debug, Clone)]
pub struct AssumeOwnershipConfirmation {
    observed: LeaseRecord,
    requesting_device_id: String,
    nonce: Uuid,
    valid_until: DateTime<Utc>,
}

impl AssumeOwnershipConfirmation {
    pub fn current_owner(&self) -> &LeaseRecord {
        &self.observed
    }

    pub fn requesting_device_id(&self) -> &str {
        &self.requesting_device_id
    }

    pub fn valid_until(&self) -> DateTime<Utc> {
        self.valid_until
    }
}

#[derive(Debug, Error)]
pub enum ProfileManagerError {
    #[error("profile and device identities must not be empty")]
    InvalidIdentity,
    #[error("profile is actively managed by device {owner_device} (session {owner_session})")]
    AlreadyOwned {
        owner_device: String,
        owner_session: Uuid,
    },
    #[error("the ownership operation is already in progress")]
    Busy,
    #[error("ownership was lost to another manager")]
    OwnershipLost,
    #[error("explicit assume-ownership confirmation was declined")]
    ConfirmationRequired,
    #[error("assume-ownership confirmation is expired or no longer matches the active owner")]
    InvalidConfirmation,
    #[error("invalid lease file: {0}")]
    InvalidLease(String),
    #[error("profile ownership I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone)]
pub struct ProfileManager {
    inner: Arc<ProfileManagerInner>,
}

#[derive(Debug)]
struct ProfileManagerInner {
    root: PathBuf,
    identity: ManagerIdentity,
    session_id: Uuid,
    lease_duration: Duration,
}

impl ProfileManager {
    pub fn new(
        profile_root: impl Into<PathBuf>,
        identity: ManagerIdentity,
        lease_duration: Duration,
    ) -> Result<Self, ProfileManagerError> {
        if identity.profile_id.trim().is_empty()
            || identity.device_id.trim().is_empty()
            || lease_duration.is_zero()
        {
            return Err(ProfileManagerError::InvalidIdentity);
        }
        Ok(Self {
            inner: Arc::new(ProfileManagerInner {
                root: profile_root.into(),
                identity,
                session_id: Uuid::new_v4(),
                lease_duration,
            }),
        })
    }

    pub fn identity(&self) -> &ManagerIdentity {
        &self.inner.identity
    }

    pub fn session_id(&self) -> Uuid {
        self.inner.session_id
    }

    pub fn lease_path(&self) -> PathBuf {
        self.inner.root.join(LEASE_FILE)
    }

    /// Inspect the lease without mutating it. A malformed lease is never treated as stale.
    pub fn status(&self, now: DateTime<Utc>) -> Result<OwnershipStatus, ProfileManagerError> {
        let path = self.lease_path();
        match fs::read(&path) {
            Ok(bytes) => match parse_lease(&bytes, &self.inner.identity.profile_id) {
                Ok(lease) if lease.is_stale_at(now) => Ok(OwnershipStatus::Stale(lease)),
                Ok(lease) => Ok(OwnershipStatus::Owned(lease)),
                Err(error) => Ok(OwnershipStatus::Corrupt(error.to_string())),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(OwnershipStatus::Available),
            Err(error) => Err(error.into()),
        }
    }

    /// Acquire an unowned profile, renew this session, or automatically recover an expired lease.
    pub fn acquire(&self, now: DateTime<Utc>) -> Result<OwnershipLease, ProfileManagerError> {
        fs::create_dir_all(&self.inner.root)?;
        let _guard = OperationGuard::acquire(&self.inner.root, self.inner.lease_duration)?;
        let current = read_optional_lease(&self.lease_path(), &self.inner.identity.profile_id)?;
        let reason = match current {
            None => TakeoverReason::NoOwner,
            Some(ref lease) if lease.session_id == self.inner.session_id => TakeoverReason::NoOwner,
            Some(ref lease) if lease.is_stale_at(now) => TakeoverReason::ExpiredLease,
            Some(lease) => {
                return Err(ProfileManagerError::AlreadyOwned {
                    owner_device: lease.device_id,
                    owner_session: lease.session_id,
                })
            }
        };
        let record = self.make_record(now, current.as_ref().map(|value| value.acquired_at));
        atomic_write_json(&self.lease_path(), &record)?;
        Ok(OwnershipLease {
            record,
            takeover_reason: reason,
        })
    }

    /// Prepare the warning/confirmation step needed to evict a live manager on another device.
    pub fn prepare_assume_ownership(
        &self,
        now: DateTime<Utc>,
    ) -> Result<AssumeOwnershipConfirmation, ProfileManagerError> {
        let lease = read_optional_lease(&self.lease_path(), &self.inner.identity.profile_id)?
            .ok_or(ProfileManagerError::InvalidConfirmation)?;
        if lease.session_id == self.inner.session_id || lease.is_stale_at(now) {
            return Err(ProfileManagerError::InvalidConfirmation);
        }
        Ok(AssumeOwnershipConfirmation {
            observed: lease,
            requesting_device_id: self.inner.identity.device_id.clone(),
            nonce: Uuid::new_v4(),
            valid_until: now + ChronoDuration::minutes(5),
        })
    }

    /// Explicitly evict a live owner. `confirmed` must come from the final user confirmation UI.
    pub fn assume_ownership(
        &self,
        confirmation: &AssumeOwnershipConfirmation,
        confirmed: bool,
        now: DateTime<Utc>,
    ) -> Result<OwnershipLease, ProfileManagerError> {
        if !confirmed {
            return Err(ProfileManagerError::ConfirmationRequired);
        }
        let _ = confirmation.nonce;
        if confirmation.requesting_device_id != self.inner.identity.device_id
            || confirmation.valid_until < now
        {
            return Err(ProfileManagerError::InvalidConfirmation);
        }
        fs::create_dir_all(&self.inner.root)?;
        let _guard = OperationGuard::acquire(&self.inner.root, self.inner.lease_duration)?;
        let current = read_optional_lease(&self.lease_path(), &self.inner.identity.profile_id)?
            .ok_or(ProfileManagerError::InvalidConfirmation)?;
        // Heartbeats update timestamps and expiry while the manager session remains the same.
        // A confirmation must remain usable across those renewals, but never across a different
        // manager session (including a new session on the same device).
        if current.device_id != confirmation.observed.device_id
            || current.session_id != confirmation.observed.session_id
            || current.is_stale_at(now)
        {
            return Err(ProfileManagerError::InvalidConfirmation);
        }
        let record = self.make_record(now, None);
        atomic_write_json(&self.lease_path(), &record)?;
        Ok(OwnershipLease {
            record,
            takeover_reason: TakeoverReason::ConfirmedHandoff,
        })
    }

    pub fn heartbeat(&self, now: DateTime<Utc>) -> Result<LeaseRecord, ProfileManagerError> {
        let _guard = OperationGuard::acquire(&self.inner.root, self.inner.lease_duration)?;
        let current = read_optional_lease(&self.lease_path(), &self.inner.identity.profile_id)?
            .ok_or(ProfileManagerError::OwnershipLost)?;
        if current.session_id != self.inner.session_id
            || current.device_id != self.inner.identity.device_id
        {
            return Err(ProfileManagerError::OwnershipLost);
        }
        let record = self.make_record(now, Some(current.acquired_at));
        atomic_write_json(&self.lease_path(), &record)?;
        Ok(record)
    }

    /// Run heartbeats until the handle is stopped or ownership is lost.
    pub fn start_heartbeat(&self, interval: Duration) -> HeartbeatHandle {
        let manager = self.clone();
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if manager.heartbeat(Utc::now()).is_err() {
                    break;
                }
            }
        });
        HeartbeatHandle { task }
    }

    /// Release only this exact session. A replacement owner is never removed.
    pub fn release(&self) -> Result<bool, ProfileManagerError> {
        let _guard = OperationGuard::acquire(&self.inner.root, self.inner.lease_duration)?;
        let Some(current) =
            read_optional_lease(&self.lease_path(), &self.inner.identity.profile_id)?
        else {
            return Ok(false);
        };
        if current.session_id != self.inner.session_id {
            return Ok(false);
        }
        match fs::remove_file(self.lease_path()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn owns_profile(&self, now: DateTime<Utc>) -> Result<bool, ProfileManagerError> {
        Ok(matches!(
            self.status(now)?,
            OwnershipStatus::Owned(lease) if lease.session_id == self.inner.session_id
        ))
    }

    fn make_record(&self, now: DateTime<Utc>, acquired_at: Option<DateTime<Utc>>) -> LeaseRecord {
        let duration = ChronoDuration::from_std(self.inner.lease_duration)
            .unwrap_or_else(|_| ChronoDuration::days(365));
        LeaseRecord {
            format_version: LEASE_FORMAT_VERSION,
            profile_id: self.inner.identity.profile_id.clone(),
            device_id: self.inner.identity.device_id.clone(),
            session_id: self.inner.session_id,
            acquired_at: acquired_at.unwrap_or(now),
            heartbeat_at: now,
            expires_at: now + duration,
        }
    }
}

#[derive(Debug)]
pub struct HeartbeatHandle {
    task: JoinHandle<()>,
}

impl HeartbeatHandle {
    pub fn stop(self) {
        self.task.abort();
    }
}

impl Drop for HeartbeatHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct OperationGuard {
    path: PathBuf,
}

impl OperationGuard {
    fn acquire(root: &Path, stale_after: Duration) -> Result<Self, ProfileManagerError> {
        let path = root.join(OPERATION_GUARD_FILE);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", Uuid::new_v4())?;
                file.sync_all()?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .and_then(|modified| modified.elapsed().map_err(io::Error::other))
                    .is_ok_and(|age| age > stale_after.max(Duration::from_secs(30)));
                if stale {
                    match fs::remove_file(&path) {
                        Ok(()) => Self::acquire(root, stale_after),
                        Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {
                            Self::acquire(root, stale_after)
                        }
                        Err(remove_error) => Err(remove_error.into()),
                    }
                } else {
                    Err(ProfileManagerError::Busy)
                }
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn read_optional_lease(
    path: &Path,
    expected_profile: &str,
) -> Result<Option<LeaseRecord>, ProfileManagerError> {
    match fs::read(path) {
        Ok(bytes) => parse_lease(&bytes, expected_profile).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn parse_lease(bytes: &[u8], expected_profile: &str) -> Result<LeaseRecord, ProfileManagerError> {
    let lease: LeaseRecord = serde_json::from_slice(bytes)
        .map_err(|error| ProfileManagerError::InvalidLease(error.to_string()))?;
    if lease.format_version != LEASE_FORMAT_VERSION || lease.profile_id != expected_profile {
        return Err(ProfileManagerError::InvalidLease(
            "unsupported version or mismatched profile identity".into(),
        ));
    }
    Ok(lease)
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), ProfileManagerError> {
    let parent = path.parent().ok_or_else(|| {
        ProfileManagerError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing parent",
        ))
    })?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{LEASE_FILE}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<(), ProfileManagerError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, value)
            .map_err(|error| ProfileManagerError::InvalidLease(error.to_string()))?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "rec-bag-profile-manager-{label}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manager(root: &Path, device: &str, lease: Duration) -> ProfileManager {
        ProfileManager::new(root, ManagerIdentity::new("profile-1", device), lease).unwrap()
    }

    #[test]
    fn only_one_live_manager_and_release_is_session_safe() {
        let root = TempDirectory::new("exclusive");
        let now = Utc::now();
        let first = manager(&root.0, "device-a", Duration::from_secs(60));
        let second = manager(&root.0, "device-b", Duration::from_secs(60));
        first.acquire(now).unwrap();
        assert!(matches!(
            second.acquire(now),
            Err(ProfileManagerError::AlreadyOwned { .. })
        ));
        assert!(!second.release().unwrap());
        assert!(first.owns_profile(now).unwrap());
        assert!(first.release().unwrap());
        assert!(second.acquire(now).is_ok());
    }

    #[test]
    fn expired_lease_is_recovered_after_a_crash() {
        let root = TempDirectory::new("stale");
        let first = manager(&root.0, "device-a", Duration::from_millis(10));
        let second = manager(&root.0, "device-b", Duration::from_secs(60));
        let now = Utc::now();
        first.acquire(now).unwrap();
        let takeover = second.acquire(now + ChronoDuration::seconds(1)).unwrap();
        assert_eq!(takeover.takeover_reason, TakeoverReason::ExpiredLease);
        assert!(matches!(
            first.heartbeat(now + ChronoDuration::seconds(1)),
            Err(ProfileManagerError::OwnershipLost)
        ));
    }

    #[test]
    fn heartbeat_does_not_invalidate_active_takeover_confirmation() {
        let root = TempDirectory::new("confirmation");
        let first = manager(&root.0, "device-a", Duration::from_secs(60));
        let second = manager(&root.0, "device-b", Duration::from_secs(60));
        let now = Utc::now();
        first.acquire(now).unwrap();
        let confirmation = second.prepare_assume_ownership(now).unwrap();
        assert!(matches!(
            second.assume_ownership(&confirmation, false, now),
            Err(ProfileManagerError::ConfirmationRequired)
        ));
        first.heartbeat(now + ChronoDuration::seconds(1)).unwrap();
        let lease = second
            .assume_ownership(&confirmation, true, now + ChronoDuration::seconds(1))
            .unwrap();
        assert_eq!(lease.takeover_reason, TakeoverReason::ConfirmedHandoff);
        assert!(!first.release().unwrap());
    }

    #[test]
    fn corrupt_or_wrong_profile_lease_is_not_silently_taken_over() {
        let root = TempDirectory::new("corrupt");
        let manager = manager(&root.0, "device-a", Duration::from_secs(60));
        fs::write(manager.lease_path(), b"not-json").unwrap();
        assert!(matches!(
            manager.status(Utc::now()).unwrap(),
            OwnershipStatus::Corrupt(_)
        ));
        assert!(matches!(
            manager.acquire(Utc::now()),
            Err(ProfileManagerError::InvalidLease(_))
        ));
    }
}
