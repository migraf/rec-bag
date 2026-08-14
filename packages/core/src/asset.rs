//! Provider-neutral contracts for music assets and music-file storage.
//!
//! These types describe owned original audio, reproducible derived files, and
//! their locations without requiring a local filesystem path. Provider crates
//! are responsible for translating their native identifiers into these models.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use uuid::Uuid;

/// The canonical number of hexadecimal characters in a SHA-256 digest.
pub const SHA256_HEX_LENGTH: usize = 64;

/// Number of days replaced or deleted backup versions are retained by default.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// Minimum number of recent versions retained by default.
pub const DEFAULT_MINIMUM_VERSIONS: u32 = 3;

/// Kind of provider that stores actual music files.
///
/// `Other` allows provider implementations to preserve a stable provider key
/// without requiring a core release for every new storage backend.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicStorageKind {
    /// A directory on a local or mounted filesystem.
    LocalDirectory,
    /// A folder in a user's Google Drive.
    GoogleDrive,
    /// A provider not yet represented by a first-class variant.
    Other(String),
}

/// The responsibility assigned to a configured music storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageRole {
    /// No storage responsibility has been assigned yet.
    ///
    /// This supports migrated paths and primary candidates without declaring
    /// them to be the active primary.
    Unassigned,
    /// The storage is assigned authority for owned original assets.
    ///
    /// This role alone does not say that the configuration is enabled or that
    /// an application-level active-primary selection points to it.
    Primary,
    /// A retained replica of verified primary assets and catalog snapshots.
    Backup,
}

impl StorageRole {
    /// Returns whether this role assigns authority to the storage.
    ///
    /// This only describes the role. It does not inspect whether a storage is
    /// enabled or selected as the application's active primary.
    pub const fn is_authoritative(self) -> bool {
        matches!(self, Self::Primary)
    }

    /// Returns whether this role receives replication from primary storage.
    pub const fn receives_replication(self) -> bool {
        matches!(self, Self::Backup)
    }
}

/// Operations and consistency features exposed by a music storage provider.
///
/// Capabilities describe what an adapter can actually perform for the selected
/// root, account, and permissions. They must not be inferred from
/// [`MusicStorageKind`] alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StorageCapabilities {
    /// The provider can enumerate objects below the configured root.
    pub can_inventory: bool,
    /// The provider can retrieve current metadata for one object.
    pub can_stat: bool,
    /// The provider can upload or copy a new object into storage.
    pub can_upload: bool,
    /// The provider can download or open object content.
    pub can_download: bool,
    /// The provider can create child containers or directories.
    pub can_create_containers: bool,
    /// The provider can replace an object atomically from rec_bag's view.
    pub supports_atomic_replace: bool,
    /// The provider exposes a cursor, watcher, or equivalent change feed.
    pub supports_change_tracking: bool,
    /// The provider can expose or retain previous object versions.
    pub supports_version_history: bool,
    /// Hash algorithms reported directly by the provider.
    pub provider_hash_algorithms: Vec<HashAlgorithm>,
}

impl StorageCapabilities {
    /// Returns whether the provider supports the minimum operations needed to
    /// store and verify music assets.
    pub const fn can_store_and_verify(&self) -> bool {
        self.can_inventory && self.can_stat && self.can_upload && self.can_download
    }

    /// Returns whether these capabilities are sufficient for a storage role.
    ///
    /// Every configured storage, including an unassigned migration candidate,
    /// must support inventory, stat, upload, and download before it can store
    /// and verify assets. Role-specific scheduling and authority are modeled
    /// separately from provider capabilities.
    pub const fn supports_role(&self, _role: StorageRole) -> bool {
        self.can_store_and_verify()
    }

    /// Returns whether the provider reports hashes using `algorithm`.
    pub fn reports_hash(&self, algorithm: &HashAlgorithm) -> bool {
        self.provider_hash_algorithms.contains(algorithm)
    }
}

/// A configured location that stores actual music files.
///
/// Credentials are intentionally excluded and must be held by a secure
/// provider-specific credential service. `root_identifier` may be a local
/// directory, a cloud folder ID, or another provider-native locator; it is not
/// required to be a local path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MusicStorage {
    /// Stable rec_bag identifier for this storage configuration.
    pub id: Uuid,
    /// User-facing display name.
    pub name: String,
    /// Provider kind.
    pub kind: MusicStorageKind,
    /// Assigned primary or backup responsibility, or [`StorageRole::Unassigned`].
    pub role: StorageRole,
    /// Optional provider-native root locator, never a credential.
    pub root_identifier: Option<String>,
    /// Non-secret provider configuration. Credentials are stored separately.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Capabilities available with the current account and permissions.
    pub capabilities: StorageCapabilities,
    /// Whether new primary versions replicate to this storage.
    ///
    /// This setting is meaningful when `role` is [`StorageRole::Backup`].
    pub replication_mode: ReplicationMode,
    /// Policy for retaining replaced and deleted versions.
    ///
    /// This setting is meaningful when `role` is [`StorageRole::Backup`].
    pub retention_policy: RetentionPolicy,
    /// Whether rec_bag may schedule work for this storage.
    pub enabled: bool,
    /// Time at which this configuration was created.
    pub created_at: DateTime<Utc>,
    /// Time at which this configuration last changed.
    pub updated_at: DateTime<Utc>,
    /// Time at which a complete or incremental inventory last committed.
    pub last_inventory_at: Option<DateTime<Utc>>,
}

impl MusicStorage {
    /// Creates an enabled storage configuration with no required local path.
    pub fn new(
        name: impl Into<String>,
        kind: MusicStorageKind,
        role: StorageRole,
        capabilities: StorageCapabilities,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            kind,
            role,
            root_identifier: None,
            config: BTreeMap::new(),
            capabilities,
            replication_mode: ReplicationMode::default(),
            retention_policy: RetentionPolicy::default(),
            enabled: true,
            created_at: now,
            updated_at: now,
            last_inventory_at: None,
        }
    }

    /// Sets the provider-native root locator.
    pub fn with_root_identifier(mut self, root_identifier: impl Into<String>) -> Self {
        self.root_identifier = Some(root_identifier.into());
        self
    }

    /// Returns whether the configured provider can fulfill its assigned role.
    pub fn supports_assigned_role(&self) -> bool {
        self.capabilities.supports_role(self.role)
    }
}

/// A digest algorithm used as content identity or provider evidence.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    /// SHA-256, the canonical rec_bag content checksum.
    Sha256,
    /// SHA-1 reported by some storage providers.
    Sha1,
    /// MD5 reported by some storage providers.
    Md5,
    /// A provider or algorithm name not represented by a first-class variant.
    Other(String),
}

/// A content digest paired with its algorithm.
///
/// Fields are private so first-class SHA-256 values cannot bypass validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ContentHash {
    /// Algorithm used to calculate the digest.
    algorithm: HashAlgorithm,
    /// Provider-preserved or canonical hexadecimal digest value.
    value: String,
}

impl ContentHash {
    /// Creates a hash, validating values for algorithms with canonical rules.
    ///
    /// First-class SHA-256 values must be 64 hexadecimal characters and are
    /// normalized to lowercase. SHA-1, MD5, and provider-specific values are
    /// accepted verbatim because providers may return non-canonical evidence.
    pub fn new(
        algorithm: HashAlgorithm,
        value: impl Into<String>,
    ) -> Result<Self, ContentHashError> {
        let value = value.into();
        if algorithm == HashAlgorithm::Sha256 {
            return Self::sha256(value);
        }

        Ok(Self { algorithm, value })
    }

    /// Creates a canonical SHA-256 hash from a 64-character hexadecimal digest.
    ///
    /// Uppercase hexadecimal input is accepted and normalized to lowercase.
    /// This helper validates an already-computed digest; hashing byte streams is
    /// the responsibility of storage adapters.
    pub fn sha256(value: impl AsRef<str>) -> Result<Self, ContentHashError> {
        let value = value.as_ref();
        if value.len() != SHA256_HEX_LENGTH {
            return Err(ContentHashError::InvalidSha256Length {
                actual: value.len(),
            });
        }

        if let Some((index, character)) = value
            .char_indices()
            .find(|(_, character)| !character.is_ascii_hexdigit())
        {
            return Err(ContentHashError::InvalidHexCharacter { index, character });
        }

        Ok(Self {
            algorithm: HashAlgorithm::Sha256,
            value: value.to_ascii_lowercase(),
        })
    }

    /// Alias emphasizing that SHA-256 is rec_bag's canonical content hash.
    pub fn canonical_sha256(value: impl AsRef<str>) -> Result<Self, ContentHashError> {
        Self::sha256(value)
    }

    /// Returns the digest algorithm.
    pub const fn algorithm(&self) -> &HashAlgorithm {
        &self.algorithm
    }

    /// Returns the canonical or provider-preserved digest value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Returns whether this is a validated-form canonical SHA-256 digest.
    pub fn is_canonical_sha256(&self) -> bool {
        self.algorithm == HashAlgorithm::Sha256
            && self.value.len() == SHA256_HEX_LENGTH
            && self
                .value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SerializedContentHash {
            algorithm: HashAlgorithm,
            value: String,
        }

        let hash = SerializedContentHash::deserialize(deserializer)?;
        Self::new(hash.algorithm, hash.value).map_err(serde::de::Error::custom)
    }
}

/// Validation failure while constructing a canonical content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentHashError {
    /// A SHA-256 digest did not contain exactly 64 hexadecimal characters.
    InvalidSha256Length {
        /// Number of bytes supplied by the caller.
        actual: usize,
    },
    /// A non-hexadecimal character was found.
    InvalidHexCharacter {
        /// Byte offset of the invalid character.
        index: usize,
        /// Invalid character.
        character: char,
    },
}

impl fmt::Display for ContentHashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSha256Length { actual } => write!(
                formatter,
                "SHA-256 digest must contain {SHA256_HEX_LENGTH} hexadecimal characters, got {actual}"
            ),
            Self::InvalidHexCharacter { index, character } => write!(
                formatter,
                "invalid hexadecimal character {character:?} at byte {index}"
            ),
        }
    }
}

impl Error for ContentHashError {}

/// High-level classification used when associating a location with an asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetType {
    /// Owned audio that contributes to archival completeness.
    Original,
    /// Reproducible data that never substitutes for an original.
    Derived,
}

/// Common identity and type record for every catalog asset.
///
/// This is the provider-neutral contract for an `assets` SQL parent table.
/// [`OriginalAsset`] and [`DerivedAsset`] embed it as their common parent data,
/// and [`AssetLocation::asset_id`] references `id` regardless of subtype.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetRecord {
    /// Stable rec_bag identity shared by the parent and one subtype record.
    pub id: Uuid,
    /// Canonical track represented by this asset.
    pub track_id: Uuid,
    /// Subtype stored for this asset identity.
    pub asset_type: AssetType,
    /// Time at which this asset version was registered in the catalog.
    pub created_at: DateTime<Utc>,
}

impl AssetRecord {
    /// Creates a common parent record for an asset subtype.
    pub fn new(track_id: Uuid, asset_type: AssetType) -> Self {
        Self {
            id: Uuid::new_v4(),
            track_id,
            asset_type,
            created_at: Utc::now(),
        }
    }
}

/// Organizational role of an owned original audio file.
///
/// Both roles count as originals for archival completeness. Preference is a
/// separate flag because users may change it independently of provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalAssetRole {
    /// The principal archival representation supplied or selected by the user.
    Master,
    /// Another owned representation, such as a purchased MP3 beside a WAV.
    Alternate,
}

/// Purpose of a reproducible derived asset.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DerivedAssetRole {
    /// Audio converted for compatible or efficient playback.
    PlaybackTranscode,
    /// A verified local copy used for offline playback of remote storage.
    OfflineCache,
    /// Reproducible analysis input or output stored as a file.
    AnalysisArtifact,
    /// A waveform representation stored separately from catalog metadata.
    Waveform,
    /// A future derived purpose identified by a stable key.
    Other(String),
}

/// An owned audio file representing a canonical track.
///
/// An original contributes to archival completeness regardless of format. Its
/// provider locations are modeled separately by [`AssetLocation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalAsset {
    /// Common parent identity and original subtype discriminator.
    #[serde(flatten)]
    pub record: AssetRecord,
    /// Stable identity shared by all metadata versions of this format.
    ///
    /// A separately acquired or encoded format starts a separate lineage.
    pub lineage_id: Uuid,
    /// Monotonically increasing metadata version within `lineage_id`.
    pub version: u64,
    /// Immediately preceding asset version, if this is not the first version.
    pub supersedes_asset_id: Option<Uuid>,
    /// Organizational role among the track's originals.
    pub role: OriginalAssetRole,
    /// Original or managed file name, without requiring a local path.
    pub file_name: String,
    /// MIME type when known.
    pub media_type: Option<String>,
    /// File size in bytes, when known before verification.
    pub size_bytes: Option<u64>,
    /// Canonical SHA-256 content identity once the original has been hashed.
    ///
    /// Existing libraries can migrate path-backed assets before an inventory job has
    /// computed their checksum, so this is intentionally optional.
    pub content_hash: Option<ContentHash>,
    /// Whether this is the user's preferred original for the track.
    pub is_preferred: bool,
}

impl OriginalAsset {
    /// Creates an original asset with a validated canonical SHA-256 digest.
    pub fn new(
        track_id: Uuid,
        role: OriginalAssetRole,
        file_name: impl Into<String>,
        size_bytes: u64,
        sha256: impl AsRef<str>,
    ) -> Result<Self, ContentHashError> {
        let mut asset = Self::new_unverified(track_id, role, file_name);
        asset.size_bytes = Some(size_bytes);
        asset.content_hash = Some(ContentHash::sha256(sha256)?);
        Ok(asset)
    }

    /// Create an original asset before its content hash and size are known.
    pub fn new_unverified(
        track_id: Uuid,
        role: OriginalAssetRole,
        file_name: impl Into<String>,
    ) -> Self {
        Self {
            record: AssetRecord::new(track_id, AssetType::Original),
            lineage_id: Uuid::new_v4(),
            version: 1,
            supersedes_asset_id: None,
            role,
            file_name: file_name.into(),
            media_type: None,
            size_bytes: None,
            content_hash: None,
            is_preferred: false,
        }
    }

    /// Returns the stable common parent identity for this asset version.
    pub const fn id(&self) -> Uuid {
        self.record.id
    }

    /// Returns the canonical track represented by this original.
    pub const fn track_id(&self) -> Uuid {
        self.record.track_id
    }

    /// Creates the next metadata version in this format's lineage.
    ///
    /// The new record receives a distinct common asset identity, points to this
    /// version through `supersedes_asset_id`, and retains content identity and
    /// format metadata for the caller to edit before persistence. Register a
    /// separately acquired or encoded format with [`Self::new`] or
    /// [`Self::new_unverified`] instead so it receives a separate lineage.
    pub fn new_metadata_version(&self) -> Self {
        let mut next = self.clone();
        next.record = AssetRecord::new(self.track_id(), AssetType::Original);
        next.version = self
            .version
            .checked_add(1)
            .expect("original asset version exhausted u64");
        next.supersedes_asset_id = Some(self.id());
        next
    }

    /// Whether this original has the canonical hash required for exact matching.
    pub fn has_verified_identity(&self) -> bool {
        self.content_hash
            .as_ref()
            .is_some_and(ContentHash::is_canonical_sha256)
    }
}

/// A reproducible file generated from an original asset.
///
/// Derived files improve playback or analysis but never satisfy archival
/// completeness and may be safely regenerated from `source_asset_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedAsset {
    /// Common parent identity and derived subtype discriminator.
    #[serde(flatten)]
    pub record: AssetRecord,
    /// Original asset from which this file can be reproduced.
    pub source_asset_id: Uuid,
    /// Purpose of the derived file.
    pub role: DerivedAssetRole,
    /// File name, without requiring a local path.
    pub file_name: String,
    /// MIME type when known.
    pub media_type: Option<String>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Content identity of this derived file.
    pub content_hash: ContentHash,
    /// Whether resolvers should prefer this derivative for playback.
    pub is_preferred_for_playback: bool,
}

impl DerivedAsset {
    /// Creates a derived subtype with a common parent asset record.
    pub fn new(
        track_id: Uuid,
        source_asset_id: Uuid,
        role: DerivedAssetRole,
        file_name: impl Into<String>,
        size_bytes: u64,
        content_hash: ContentHash,
    ) -> Self {
        Self {
            record: AssetRecord::new(track_id, AssetType::Derived),
            source_asset_id,
            role,
            file_name: file_name.into(),
            media_type: None,
            size_bytes,
            content_hash,
            is_preferred_for_playback: false,
        }
    }

    /// Returns the stable common parent identity for this asset.
    pub const fn id(&self) -> Uuid {
        self.record.id
    }

    /// Returns the canonical track represented by this derivative.
    pub const fn track_id(&self) -> Uuid {
        self.record.track_id
    }
}

/// Provider-neutral metadata for an object returned by inventory or stat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageObject {
    /// Storage from which this object was observed.
    pub storage_id: Uuid,
    /// Stable provider-native object identity, such as a Drive file ID.
    pub object_id: String,
    /// Provider-reported object name.
    pub name: String,
    /// Optional user-facing path or hierarchy label.
    ///
    /// This is display metadata and is not required to be a local path or a
    /// stable identity.
    pub display_path: Option<String>,
    /// Object size in bytes.
    pub size_bytes: u64,
    /// MIME type when reported by the provider.
    pub media_type: Option<String>,
    /// Last modification time reported by the provider.
    pub modified_at: Option<DateTime<Utc>>,
    /// Opaque provider generation, revision, ETag, or version identifier.
    pub provider_version: Option<String>,
    /// Canonical and provider-reported hashes available for the object.
    pub hashes: Vec<ContentHash>,
    /// Asset ID embedded by rec_bag in provider metadata, when present.
    pub embedded_asset_id: Option<Uuid>,
    /// Whether the provider reports the object as trashed or deleted.
    pub is_trashed: bool,
}

impl StorageObject {
    /// Returns the object's canonical SHA-256 hash, when inventory supplied one.
    pub fn canonical_hash(&self) -> Option<&ContentHash> {
        self.hashes.iter().find(|hash| hash.is_canonical_sha256())
    }
}

/// Reconciliation state for an asset's expected storage location.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageVerificationStatus {
    /// The expected asset and provider object are confirmed to match.
    Verified,
    /// The expected provider object is absent.
    Missing,
    /// One candidate exists but only weak evidence supports the association.
    PossibleMatch,
    /// Multiple candidates prevent an automatic association.
    AmbiguousMatch,
    /// A previously known object changed and must be approved before replication.
    ChangedPendingReview,
    /// The storage cannot currently be reached; its inventory remains known.
    Unavailable,
    /// Verification failed for a reason other than temporary unavailability.
    Error,
}

impl StorageVerificationStatus {
    /// Returns whether this state is strong enough to use as a replication source.
    pub const fn permits_replication(self) -> bool {
        matches!(self, Self::Verified)
    }

    /// Returns whether this state requires a user matching or change decision.
    pub const fn requires_review(self) -> bool {
        matches!(
            self,
            Self::PossibleMatch | Self::AmbiguousMatch | Self::ChangedPendingReview
        )
    }
}

/// Persisted association between a catalog asset and a provider object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetLocation {
    /// Stable identifier for this association.
    pub id: Uuid,
    /// Common [`AssetRecord::id`] for the original or derived asset stored here.
    pub asset_id: Uuid,
    /// Asset classification used without loading the asset record.
    pub asset_type: AssetType,
    /// Configured music storage containing the object.
    pub storage_id: Uuid,
    /// Stable provider-native object identity.
    pub object_id: String,
    /// Current reconciliation and verification state.
    pub verification_status: StorageVerificationStatus,
    /// Hash expected from the associated asset, once it has been computed.
    pub expected_hash: Option<ContentHash>,
    /// Hash most recently observed for the provider object.
    pub observed_hash: Option<ContentHash>,
    /// Opaque provider version most recently observed.
    pub provider_version: Option<String>,
    /// Most recent verification attempt.
    pub last_checked_at: Option<DateTime<Utc>>,
    /// Most recent successful verification.
    pub verified_at: Option<DateTime<Utc>>,
    /// Non-secret diagnostic detail when verification has failed.
    pub error_message: Option<String>,
}

/// Policy applied when a catalog source file is missing from primary storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceCopyPolicy {
    /// Detect and report missing assets without copying them.
    CheckOnly,
    /// Ask the user before copying each eligible source asset.
    #[default]
    AskBeforeCopying,
    /// Automatically copy eligible source assets and verify the destination.
    AutomaticallyCopyEligibleFiles,
}

/// Scheduling mode for replication from primary to one backup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationMode {
    /// Queue replication after a primary write has been verified.
    #[default]
    Automatic,
    /// Replicate according to an external scheduler configuration.
    Scheduled,
    /// Replicate only when explicitly requested by the user.
    Manual,
}

/// Retention guarantees for replaced and deleted backup versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Age window during which replaced or deleted versions are retained.
    pub retention_days: u32,
    /// Minimum number of most recent versions retained regardless of age.
    pub minimum_versions: u32,
    /// Whether permanent cleanup requires explicit user confirmation.
    pub require_cleanup_confirmation: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_RETENTION_DAYS,
            minimum_versions: DEFAULT_MINIMUM_VERSIONS,
            require_cleanup_confirmation: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fully_capable_storage() -> StorageCapabilities {
        StorageCapabilities {
            can_inventory: true,
            can_stat: true,
            can_upload: true,
            can_download: true,
            can_create_containers: true,
            supports_atomic_replace: true,
            supports_change_tracking: true,
            supports_version_history: true,
            provider_hash_algorithms: vec![HashAlgorithm::Sha256],
        }
    }

    #[test]
    fn policy_defaults_are_safe_and_match_stage_one_contracts() {
        assert_eq!(
            SourceCopyPolicy::default(),
            SourceCopyPolicy::AskBeforeCopying
        );
        assert_eq!(ReplicationMode::default(), ReplicationMode::Automatic);

        let retention = RetentionPolicy::default();
        assert_eq!(retention.retention_days, 30);
        assert_eq!(retention.minimum_versions, 3);
        assert!(retention.require_cleanup_confirmation);
    }

    #[test]
    fn canonical_sha256_validates_and_normalizes_hex() {
        let uppercase = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
        let hash = ContentHash::canonical_sha256(uppercase).expect("valid SHA-256");

        assert_eq!(hash.algorithm(), &HashAlgorithm::Sha256);
        assert_eq!(
            hash.value(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        assert!(hash.is_canonical_sha256());
        assert!(matches!(
            ContentHash::sha256("abc"),
            Err(ContentHashError::InvalidSha256Length { actual: 3 })
        ));

        let invalid = format!("{}g", "0".repeat(SHA256_HEX_LENGTH - 1));
        assert!(matches!(
            ContentHash::sha256(invalid),
            Err(ContentHashError::InvalidHexCharacter {
                index: 63,
                character: 'g'
            })
        ));

        let provider_md5 = ContentHash::new(HashAlgorithm::Md5, "provider-value")
            .expect("provider MD5 evidence is accepted verbatim");
        assert_eq!(provider_md5.value(), "provider-value");
    }

    #[test]
    fn sha256_deserialization_validates_and_normalizes() {
        let invalid = r#"{"algorithm":"sha256","value":"abc"}"#;
        let error = serde_json::from_str::<ContentHash>(invalid)
            .expect_err("invalid SHA-256 must not deserialize");
        assert!(error.to_string().contains("must contain 64"));

        let uppercase = r#"{"algorithm":"sha256","value":"ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789"}"#;
        let hash = serde_json::from_str::<ContentHash>(uppercase)
            .expect("valid uppercase SHA-256 should deserialize");
        assert_eq!(
            hash.value(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn unverified_originals_can_be_registered_before_hashing() {
        let asset =
            OriginalAsset::new_unverified(Uuid::new_v4(), OriginalAssetRole::Master, "track.wav");

        assert!(asset.size_bytes.is_none());
        assert!(asset.content_hash.is_none());
        assert!(!asset.has_verified_identity());
    }

    #[test]
    fn original_formats_have_separate_lineages_and_metadata_edits_create_versions() {
        let track_id = Uuid::new_v4();
        let wav = OriginalAsset::new_unverified(track_id, OriginalAssetRole::Master, "track.wav");
        let flac =
            OriginalAsset::new_unverified(track_id, OriginalAssetRole::Alternate, "track.flac");

        assert_ne!(wav.lineage_id, flac.lineage_id);
        assert_eq!(wav.version, 1);
        assert_eq!(wav.supersedes_asset_id, None);

        let mut edited = wav.new_metadata_version();
        edited.file_name = "renamed-track.wav".to_owned();

        assert_eq!(edited.lineage_id, wav.lineage_id);
        assert_eq!(edited.version, 2);
        assert_eq!(edited.supersedes_asset_id, Some(wav.id()));
        assert_ne!(edited.id(), wav.id());
        assert_eq!(edited.track_id(), wav.track_id());
    }

    #[test]
    fn common_asset_record_parents_originals_derivatives_and_locations() {
        let track_id = Uuid::new_v4();
        let original =
            OriginalAsset::new_unverified(track_id, OriginalAssetRole::Master, "track.wav");
        let hash =
            ContentHash::sha256("0".repeat(SHA256_HEX_LENGTH)).expect("test hash should be valid");
        let derived = DerivedAsset::new(
            track_id,
            original.id(),
            DerivedAssetRole::PlaybackTranscode,
            "track.mp3",
            1_024,
            hash,
        );
        let location = AssetLocation {
            id: Uuid::new_v4(),
            asset_id: original.record.id,
            asset_type: original.record.asset_type,
            storage_id: Uuid::new_v4(),
            object_id: "provider-object".to_owned(),
            verification_status: StorageVerificationStatus::PossibleMatch,
            expected_hash: None,
            observed_hash: None,
            provider_version: None,
            last_checked_at: None,
            verified_at: None,
            error_message: None,
        };

        assert_eq!(original.record.asset_type, AssetType::Original);
        assert_eq!(derived.record.asset_type, AssetType::Derived);
        assert_eq!(original.record.track_id, derived.record.track_id);
        assert_eq!(location.asset_id, original.record.id);
    }

    #[test]
    fn storage_roles_and_capabilities_have_distinct_semantics() {
        assert!(StorageRole::Primary.is_authoritative());
        assert!(!StorageRole::Primary.receives_replication());
        assert!(!StorageRole::Backup.is_authoritative());
        assert!(StorageRole::Backup.receives_replication());
        assert!(!StorageRole::Unassigned.is_authoritative());
        assert!(!StorageRole::Unassigned.receives_replication());

        let no_capabilities = StorageCapabilities::default();
        assert!(!no_capabilities.supports_role(StorageRole::Unassigned));
        assert!(!no_capabilities.supports_role(StorageRole::Primary));
        assert!(!no_capabilities.supports_role(StorageRole::Backup));

        let capabilities = fully_capable_storage();
        assert!(capabilities.can_store_and_verify());
        assert!(capabilities.supports_role(StorageRole::Unassigned));
        assert!(capabilities.supports_role(StorageRole::Primary));
        assert!(capabilities.supports_role(StorageRole::Backup));
        assert!(capabilities.reports_hash(&HashAlgorithm::Sha256));

        let candidate = MusicStorage::new(
            "Candidate primary",
            MusicStorageKind::LocalDirectory,
            StorageRole::Unassigned,
            capabilities.clone(),
        );
        assert!(candidate.enabled);
        assert!(!candidate.role.is_authoritative());
        assert!(candidate.supports_assigned_role());

        let storage = MusicStorage::new(
            "Remote primary",
            MusicStorageKind::GoogleDrive,
            StorageRole::Primary,
            capabilities,
        );
        assert!(storage.root_identifier.is_none());
        assert!(storage.config.is_empty());
        assert!(storage.last_inventory_at.is_none());
        assert!(storage.supports_assigned_role());
    }
}
