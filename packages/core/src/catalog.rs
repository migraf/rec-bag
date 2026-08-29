//! Provider-neutral catalog contracts.
//!
//! These models describe catalog sources, source-scoped track occurrences, external
//! playlists, canonical metadata, the user's own library entries, and cached analysis,
//! all without requiring a local audio-file path. [`crate::track::Track`] is what an
//! interface renders once these records are projected for display.

use crate::asset::SourceCopyPolicy;
use crate::track::{BeatGridData, FrequencyWaveformData, WaveformPeaks};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The provider or storage mechanism behind a catalog source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSourceKind {
    /// A folder on a locally accessible filesystem.
    #[default]
    LocalFolder,
    /// A SoundCloud account or public catalog connection.
    SoundCloud,
    /// A provider not represented by a built-in variant.
    Custom(String),
}

/// Operations supported by a catalog source.
///
/// All capabilities default to `false`; source adapters should advertise only the
/// operations they actually implement for the current account and configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SourceCapabilities {
    /// Whether the source can enumerate a library or collection.
    pub library_sync: bool,
    /// Whether the source can enumerate external playlists.
    pub playlist_sync: bool,
    /// Whether the source supports catalog search.
    pub search: bool,
    /// Whether full tracks can be streamed directly.
    pub streaming: bool,
    /// Whether audio can be obtained for local use.
    pub download: bool,
    /// Whether the source can supply track metadata.
    pub metadata: bool,
}

impl SourceCapabilities {
    /// Capabilities normally available from a readable local folder.
    pub fn local_folder() -> Self {
        Self {
            library_sync: true,
            playlist_sync: false,
            search: false,
            streaming: true,
            download: true,
            metadata: true,
        }
    }
}

/// A configured provider-neutral catalog source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogSource {
    /// Application-assigned stable identifier.
    pub id: Uuid,
    /// User-facing source name.
    pub name: String,
    /// Provider or storage mechanism used by the source.
    pub kind: CatalogSourceKind,
    /// Whether synchronization and source operations are enabled.
    pub enabled: bool,
    /// Time at which this source configuration was created.
    pub created_at: DateTime<Utc>,
    /// Time of the most recent completed synchronization, if any.
    pub last_synced_at: Option<DateTime<Utc>>,
    /// Non-secret adapter configuration.
    ///
    /// Values such as folder paths, region names, and scan options belong here.
    /// Passwords, API tokens, refresh tokens, and other credentials must be kept in
    /// a separate credential store and referenced indirectly when needed.
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    /// Operations currently supported by this source.
    #[serde(default)]
    pub capabilities: SourceCapabilities,
    /// Policy used when an obtainable source file is missing from primary storage.
    #[serde(default)]
    pub copy_policy: SourceCopyPolicy,
}

impl CatalogSource {
    /// Create an enabled source with empty public configuration and capabilities.
    pub fn new(name: impl Into<String>, kind: CatalogSourceKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            kind,
            enabled: true,
            created_at: Utc::now(),
            last_synced_at: None,
            config: BTreeMap::new(),
            capabilities: SourceCapabilities::default(),
            copy_policy: SourceCopyPolicy::default(),
        }
    }

    /// Create an enabled local-folder source.
    pub fn local_folder(name: impl Into<String>, path: impl Into<String>) -> Self {
        let mut source = Self::new(name, CatalogSourceKind::LocalFolder);
        source.config.insert("path".to_string(), path.into());
        source.capabilities = SourceCapabilities::local_folder();
        source
    }
}

/// An account exposed by a configured catalog source.
///
/// Account identity is scoped to a source because different providers may issue the
/// same external identifier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceAccount {
    /// Source through which the account is accessed.
    pub source_id: Uuid,
    /// Provider-assigned account identifier, stable within the source.
    pub external_id: String,
    /// Provider username or handle, when available.
    pub username: Option<String>,
    /// Human-readable account name, when available.
    pub display_name: Option<String>,
    /// Public provider URL for the account, when available.
    pub external_url: Option<String>,
    /// Non-secret provider-specific account metadata.
    #[serde(default)]
    pub provider_metadata: BTreeMap<String, MetadataValue>,
}

impl SourceAccount {
    /// Create an account with no optional profile data.
    pub fn new(source_id: Uuid, external_id: impl Into<String>) -> Self {
        Self {
            source_id,
            external_id: external_id.into(),
            username: None,
            display_name: None,
            external_url: None,
            provider_metadata: BTreeMap::new(),
        }
    }

    /// Return the source-scoped stable account identity.
    pub fn identity(&self) -> (Uuid, &str) {
        (self.source_id, self.external_id.as_str())
    }
}

/// Account collections whose membership is observed during source synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCollection {
    UploadedTracks,
    LikedTracks,
    OwnedPlaylists,
    LikedPlaylists,
}

/// A durable kind of source entity.
///
/// Playlist entries are relationships, not entities: removing an entry or a like must
/// not imply that the referenced track occurrence was removed from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceEntityKind {
    Account,
    Track,
    Playlist,
}

/// Stable identity of one account collection relationship.
///
/// Observation time and mutable provider metadata are deliberately excluded. Parent
/// and position distinguish repeated playlist membership without changing the source
/// entity's own identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceCollectionRelationshipIdentity {
    pub source_id: Uuid,
    pub account_external_id: String,
    pub collection: SourceCollection,
    pub entity_kind: SourceEntityKind,
    pub entity_external_id: String,
    pub parent_external_id: Option<String>,
    pub position: Option<u32>,
}

impl SourceCollectionRelationshipIdentity {
    /// Create a top-level account collection membership identity.
    pub fn new(
        source_id: Uuid,
        account_external_id: impl Into<String>,
        collection: SourceCollection,
        entity_kind: SourceEntityKind,
        entity_external_id: impl Into<String>,
    ) -> Self {
        Self {
            source_id,
            account_external_id: account_external_id.into(),
            collection,
            entity_kind,
            entity_external_id: entity_external_id.into(),
            parent_external_id: None,
            position: None,
        }
    }

    /// Scope the relationship to a parent entity and optional ordered position.
    pub fn with_parent(
        mut self,
        parent_external_id: impl Into<String>,
        position: Option<u32>,
    ) -> Self {
        self.parent_external_id = Some(parent_external_id.into());
        self.position = position;
        self
    }
}

/// A timestamped observation of a durable account collection relationship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceCollectionRelationship {
    pub identity: SourceCollectionRelationshipIdentity,
    pub last_seen_at: DateTime<Utc>,
}

impl SourceCollectionRelationship {
    pub fn new(
        identity: SourceCollectionRelationshipIdentity,
        last_seen_at: DateTime<Utc>,
    ) -> Self {
        Self {
            identity,
            last_seen_at,
        }
    }
}

/// Availability of a track occurrence at its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceAvailability {
    /// Full audio can be obtained for local use.
    Obtainable,
    /// Only metadata is available; no playable media is exposed.
    #[default]
    MetadataOnly,
    /// Full audio is available only through provider streaming.
    StreamingOnly,
    /// Only a limited preview is playable.
    PreviewOnly,
    /// Access is denied by policy, territory, account, or entitlement.
    Blocked,
    /// The provider reports that the track was removed.
    Removed,
    /// The item is known but cannot currently be inspected or accessed.
    Inaccessible,
}

/// Stable identity of a track occurrence within a source.
///
/// The pair `(source_id, external_id)` is the deduplication key. Provider metadata,
/// URLs, availability, and timestamps are observations and are not identity fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceTrackIdentity {
    /// Source that owns the external identifier namespace.
    pub source_id: Uuid,
    /// Provider-assigned stable track identifier within the source.
    pub external_id: String,
}

impl SourceTrackIdentity {
    /// Create a source-scoped track identity.
    pub fn new(source_id: Uuid, external_id: impl Into<String>) -> Self {
        Self {
            source_id,
            external_id: external_id.into(),
        }
    }

    /// Borrow the identity as its canonical pair.
    pub fn as_pair(&self) -> (Uuid, &str) {
        (self.source_id, self.external_id.as_str())
    }
}

/// A provider's occurrence of a track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceTrackOccurrence {
    /// Stable source-scoped identity used for deduplication.
    pub identity: SourceTrackIdentity,
    /// Canonical track linked to this occurrence, if reconciliation has completed.
    pub canonical_track_id: Option<Uuid>,
    /// Account that owns or exposed the occurrence, when applicable.
    pub account_external_id: Option<String>,
    /// Public provider URL for this occurrence, when available.
    pub external_url: Option<String>,
    /// Provider-specific metadata retained without affecting identity.
    #[serde(default)]
    pub provider_metadata: BTreeMap<String, MetadataValue>,
    /// Current media availability at the source.
    pub availability: SourceAvailability,
    /// Most recent time this occurrence was observed at the source.
    pub last_seen_at: DateTime<Utc>,
}

impl SourceTrackOccurrence {
    /// Create a metadata-only occurrence observed now.
    pub fn new(source_id: Uuid, external_id: impl Into<String>) -> Self {
        Self {
            identity: SourceTrackIdentity::new(source_id, external_id),
            canonical_track_id: None,
            account_external_id: None,
            external_url: None,
            provider_metadata: BTreeMap::new(),
            availability: SourceAvailability::default(),
            last_seen_at: Utc::now(),
        }
    }

    /// Return the stable deduplication key.
    pub fn dedup_key(&self) -> &SourceTrackIdentity {
        &self.identity
    }
}

/// A playlist imported from a catalog source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalPlaylist {
    /// Source that owns the playlist identifier namespace.
    pub source_id: Uuid,
    /// Provider-assigned stable playlist identifier within the source.
    pub external_id: String,
    /// Provider-visible playlist name.
    pub name: String,
    /// Provider-visible playlist description, when available.
    pub description: Option<String>,
    /// Account that owns or exposed the playlist, when applicable.
    pub account_external_id: Option<String>,
    /// Public provider URL for the playlist, when available.
    pub external_url: Option<String>,
    /// Entries in provider order. Each entry also carries its provider position so
    /// order can be reconstructed after storage or partial synchronization.
    #[serde(default)]
    pub entries: Vec<ExternalPlaylistEntry>,
    /// Opaque provider revision or snapshot token, when available.
    pub snapshot: Option<String>,
    /// Most recent time this playlist was observed at the source.
    pub last_seen_at: DateTime<Utc>,
    /// Provider-specific playlist metadata.
    #[serde(default)]
    pub provider_metadata: BTreeMap<String, MetadataValue>,
}

impl ExternalPlaylist {
    /// Create an empty external playlist observed now.
    pub fn new(source_id: Uuid, external_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            source_id,
            external_id: external_id.into(),
            name: name.into(),
            description: None,
            account_external_id: None,
            external_url: None,
            entries: Vec::new(),
            snapshot: None,
            last_seen_at: Utc::now(),
            provider_metadata: BTreeMap::new(),
        }
    }

    /// Return the stable source-scoped playlist identity.
    pub fn identity(&self) -> (Uuid, &str) {
        (self.source_id, self.external_id.as_str())
    }

    /// Sort entries by provider position while preserving relative order for ties.
    pub fn sort_entries(&mut self) {
        self.entries.sort_by_key(|entry| entry.position);
    }
}

/// One ordered entry in an external playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalPlaylistEntry {
    /// Zero-based provider position in the playlist.
    pub position: u32,
    /// Stable identity of the referenced source track occurrence.
    pub track: SourceTrackIdentity,
    /// Time at which the provider says the entry was added, when available.
    pub added_at: Option<DateTime<Utc>>,
    /// Provider-specific entry metadata.
    #[serde(default)]
    pub provider_metadata: BTreeMap<String, MetadataValue>,
}

impl ExternalPlaylistEntry {
    /// Create an entry at a provider-defined zero-based position.
    pub fn new(position: u32, track: SourceTrackIdentity) -> Self {
        Self {
            position,
            track,
            added_at: None,
            provider_metadata: BTreeMap::new(),
        }
    }
}

/// An editable rec_bag playlist whose membership references canonical tracks.
///
/// This normalized representation avoids embedding cloned track records in every
/// playlist. Imported provider playlists remain represented by [`ExternalPlaylist`]
/// and are read-only until explicitly copied into a canonical playlist.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalPlaylist {
    /// Stable rec_bag playlist identifier.
    pub id: Uuid,
    /// User-facing playlist name.
    pub name: String,
    /// Optional user-authored description.
    pub description: Option<String>,
    /// Ordered canonical track references.
    #[serde(default)]
    pub entries: Vec<CanonicalPlaylistEntry>,
    /// Time at which the playlist was created.
    pub created_at: DateTime<Utc>,
    /// Time at which the playlist was last changed.
    pub updated_at: DateTime<Utc>,
}

impl CanonicalPlaylist {
    /// Create an empty editable playlist.
    pub fn new(name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            description: None,
            entries: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Replace membership and normalize positions to a contiguous zero-based order.
    pub fn replace_tracks(&mut self, track_ids: impl IntoIterator<Item = Uuid>) {
        self.entries = track_ids
            .into_iter()
            .enumerate()
            .map(|(position, track_id)| CanonicalPlaylistEntry {
                position: position as u32,
                track_id,
            })
            .collect();
        self.updated_at = Utc::now();
    }
}

/// One ordered canonical track reference in an editable rec_bag playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CanonicalPlaylistEntry {
    /// Zero-based position. Repeated track IDs are allowed at different positions.
    pub position: u32,
    /// Canonical track referenced by this playlist entry.
    pub track_id: Uuid,
}

/// A canonical or provider-specific metadata field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataField {
    /// Track title.
    Title,
    /// Primary artist or credited artist text.
    Artist,
    /// Album or release title.
    Album,
    /// Album artist text.
    AlbumArtist,
    /// Genre text.
    Genre,
    /// Tempo in beats per minute.
    Bpm,
    /// Musical key.
    MusicalKey,
    /// Duration in milliseconds.
    DurationMs,
    /// Release year.
    ReleaseYear,
    /// Position on the release.
    TrackNumber,
    /// Disc number on a multi-disc release.
    DiscNumber,
    /// International Standard Recording Code.
    Isrc,
    /// Artwork URL.
    ArtworkUrl,
    /// A field not represented by a built-in variant.
    Custom(String),
}

/// A serializable metadata value independent of any provider schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataValue {
    /// UTF-8 text.
    Text(String),
    /// Signed integer value.
    Integer(i64),
    /// Exact base-10 decimal text, preserving provider precision without NaN or infinity.
    #[serde(alias = "float")]
    Decimal(String),
    /// Boolean value.
    Boolean(bool),
    /// Ordered list of values.
    List(Vec<MetadataValue>),
    /// String-keyed object value.
    Object(BTreeMap<String, MetadataValue>),
    /// Explicit provider null value.
    Null,
}

/// The origin category of a metadata observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataOrigin {
    /// Explicit user-authored metadata.
    User,
    /// Metadata read from tags embedded in media.
    EmbeddedTag,
    /// Metadata reported by a catalog provider.
    Provider,
    /// Metadata inferred by analysis or heuristics.
    Inferred,
    /// An application-defined origin.
    Custom(String),
}

impl MetadataOrigin {
    /// Default precedence for this origin category; larger values win.
    ///
    /// Embedded tags are unverified by default and therefore rank below provider
    /// observations. Tags verified against the primary asset use
    /// [`MetadataProvenance::verified_primary_embedded_tag`] to opt into a higher
    /// priority.
    pub fn default_priority(&self) -> i32 {
        match self {
            Self::User => 400,
            Self::Provider => 300,
            Self::EmbeddedTag => 200,
            Self::Inferred => 100,
            Self::Custom(_) => 0,
        }
    }
}

/// Provenance and precedence attached to one metadata value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetadataProvenance {
    /// Category from which the value originated.
    pub origin: MetadataOrigin,
    /// Source responsible for the observation, when source-backed.
    pub source_id: Option<Uuid>,
    /// Provider entity responsible for the observation, when available.
    pub external_id: Option<String>,
    /// Asset from which metadata was read, when asset-backed.
    #[serde(default)]
    pub asset_id: Option<Uuid>,
    /// Storage containing the observed asset, when storage-backed.
    #[serde(default)]
    pub storage_id: Option<Uuid>,
    /// Whether an embedded tag was verified as coming from the primary asset.
    #[serde(default)]
    pub verified_primary_embedded_tag: bool,
    /// Time at which the value was observed or authored.
    pub observed_at: DateTime<Utc>,
    /// Explicit precedence; larger values are preferred.
    ///
    /// Constructors initialize this from `origin`, but import and reconciliation
    /// policies may override it without changing the provenance category.
    pub priority: i32,
}

impl MetadataProvenance {
    /// Create provenance with the origin's default priority.
    pub fn new(origin: MetadataOrigin, observed_at: DateTime<Utc>) -> Self {
        let priority = origin.default_priority();
        Self {
            origin,
            source_id: None,
            external_id: None,
            asset_id: None,
            storage_id: None,
            verified_primary_embedded_tag: false,
            observed_at,
            priority,
        }
    }

    /// Create provenance for a provider observation.
    pub fn provider(
        source_id: Uuid,
        external_id: impl Into<String>,
        observed_at: DateTime<Utc>,
    ) -> Self {
        let mut provenance = Self::new(MetadataOrigin::Provider, observed_at);
        provenance.source_id = Some(source_id);
        provenance.external_id = Some(external_id.into());
        provenance
    }

    /// Create unverified embedded-tag provenance for a specific asset and storage.
    pub fn embedded_tag(
        asset_id: Uuid,
        storage_id: Option<Uuid>,
        observed_at: DateTime<Utc>,
    ) -> Self {
        let mut provenance = Self::new(MetadataOrigin::EmbeddedTag, observed_at);
        provenance.asset_id = Some(asset_id);
        provenance.storage_id = storage_id;
        provenance
    }

    /// Create embedded-tag provenance verified against the primary original asset.
    ///
    /// Verified primary tags outrank provider observations but remain below explicit
    /// user-authored metadata.
    pub fn verified_primary_embedded_tag(
        asset_id: Uuid,
        storage_id: Option<Uuid>,
        observed_at: DateTime<Utc>,
    ) -> Self {
        let mut provenance = Self::embedded_tag(asset_id, storage_id, observed_at);
        provenance.verified_primary_embedded_tag = true;
        provenance.priority = 350;
        provenance
    }

    /// Override the default precedence for a specific reconciliation policy.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// One candidate value for a metadata field, including its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetadataFieldValue {
    /// Field described by this value.
    pub field: MetadataField,
    /// Typed metadata value.
    pub value: MetadataValue,
    /// Origin, observation time, and precedence policy for this value.
    pub provenance: MetadataProvenance,
}

impl MetadataFieldValue {
    /// Create a metadata field candidate.
    pub fn new(field: MetadataField, value: MetadataValue, provenance: MetadataProvenance) -> Self {
        Self {
            field,
            value,
            provenance,
        }
    }
}

/// Candidate metadata and deterministic canonical-value selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CanonicalMetadata {
    /// All retained metadata candidates, including lower-precedence observations.
    #[serde(default)]
    pub values: Vec<MetadataFieldValue>,
}

impl CanonicalMetadata {
    /// Create an empty metadata set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain another candidate value.
    pub fn add(&mut self, value: MetadataFieldValue) {
        self.values.push(value);
    }

    /// Set the user's own value for a field, replacing any value they gave before.
    ///
    /// Passing `None` withdraws the user's override rather than blanking the field, so
    /// what a source observed becomes canonical again. Candidates from other origins
    /// are never touched: this is a reconciled model, not a set of overwritable cells.
    pub fn set_user_value(
        &mut self,
        field: MetadataField,
        value: Option<MetadataValue>,
        edited_at: DateTime<Utc>,
    ) {
        self.values.retain(|candidate| {
            candidate.field != field || candidate.provenance.origin != MetadataOrigin::User
        });
        if let Some(value) = value {
            self.add(MetadataFieldValue::new(
                field,
                value,
                MetadataProvenance::new(MetadataOrigin::User, edited_at),
            ));
        }
    }

    /// Select the canonical candidate for a field.
    ///
    /// Higher explicit priority wins. For equal priority, the most recently observed
    /// value wins. If both are equal, the later retained candidate wins.
    pub fn preferred(&self, field: &MetadataField) -> Option<&MetadataFieldValue> {
        self.values
            .iter()
            .filter(|candidate| &candidate.field == field)
            .max_by_key(|candidate| {
                (
                    candidate.provenance.priority,
                    candidate.provenance.observed_at,
                )
            })
    }
}

/// A provider-neutral canonical track.
///
/// A canonical track is an identity and reconciled metadata record. It deliberately
/// has no required filesystem path: local files and remote provider items are linked
/// through source occurrences instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalTrack {
    /// Application-assigned canonical identifier.
    pub id: Uuid,
    /// Metadata candidates from which canonical values are selected.
    #[serde(default)]
    pub metadata: CanonicalMetadata,
    /// Source occurrences reconciled to this track, deduplicated by source identity.
    #[serde(default)]
    pub occurrences: Vec<SourceTrackIdentity>,
    /// Time at which the canonical track was created.
    pub created_at: DateTime<Utc>,
    /// Time at which the canonical track was last changed.
    pub updated_at: DateTime<Utc>,
}

impl CanonicalTrack {
    /// Create a path-independent canonical track with no metadata or occurrences.
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            metadata: CanonicalMetadata::default(),
            occurrences: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Link an occurrence identity, returning `true` only when it was newly inserted.
    pub fn link_occurrence(&mut self, identity: SourceTrackIdentity) -> bool {
        if self.occurrences.contains(&identity) {
            return false;
        }

        self.occurrences.push(identity);
        self.updated_at = Utc::now();
        true
    }
}

impl Default for CanonicalTrack {
    fn default() -> Self {
        Self::new()
    }
}

/// The user's own record about a canonical track.
///
/// A library entry is authored or accumulated locally: it is never observed from a
/// provider and never reconciled against one, so it carries no metadata candidates and
/// no provenance. It is the only thing the retired path-keyed track model still owned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibraryEntry {
    /// Canonical track this entry describes.
    pub track_id: Uuid,
    /// User-defined rating, from 0 to 5.
    pub rating: Option<u8>,
    /// User comment or note.
    pub comment: Option<String>,
    /// How many times the track has been played through rec_bag.
    pub play_count: u32,
    /// When the track was last played.
    pub last_played_at: Option<DateTime<Utc>>,
    /// When the track entered the user's library.
    pub added_at: DateTime<Utc>,
    /// When any of the above last changed.
    pub updated_at: DateTime<Utc>,
}

impl LibraryEntry {
    /// Create an empty entry that was added now.
    pub fn new(track_id: Uuid) -> Self {
        let now = Utc::now();
        Self::added(track_id, now)
    }

    /// Create an empty entry added at a known time.
    pub fn added(track_id: Uuid, added_at: DateTime<Utc>) -> Self {
        Self {
            track_id,
            rating: None,
            comment: None,
            play_count: 0,
            last_played_at: None,
            added_at,
            updated_at: added_at,
        }
    }

    /// Record one play, at `played_at`.
    pub fn record_play(&mut self, played_at: DateTime<Utc>) {
        self.play_count = self.play_count.saturating_add(1);
        self.last_played_at = Some(played_at);
        self.updated_at = played_at;
    }

    /// Whether this entry holds anything the user authored or accumulated.
    ///
    /// An empty entry carries no information beyond `added_at`, so callers may skip
    /// persisting it rather than writing a row per track.
    pub fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.comment.is_none()
            && self.play_count == 0
            && self.last_played_at.is_none()
    }
}

/// What analysis derived from a canonical track's audio.
///
/// Analysis is reproducible from any original asset, so losing it loses nothing. It is
/// cached in the catalog because recomputing it costs a full decode, and it is attached
/// to a [`crate::track::Track`] by [`TrackAnalysis::apply_to`] when one is projected for
/// display or prepared for a deck.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackAnalysis {
    /// Canonical track this analysis describes.
    pub track_id: Uuid,
    /// Amplitude peaks for waveform display.
    #[serde(default)]
    pub waveform: Option<WaveformPeaks>,
    /// Per-band peaks for coloured waveform display.
    #[serde(default)]
    pub freq_waveform: Option<FrequencyWaveformData>,
    /// Detected beat positions.
    #[serde(default)]
    pub beat_grid: Option<BeatGridData>,
    /// Detected tempo, in beats per minute.
    #[serde(default)]
    pub bpm: Option<f32>,
    /// Detected musical key, in the notation the analyzer reports.
    #[serde(default)]
    pub key: Option<String>,
}

impl TrackAnalysis {
    /// An analysis record holding no results yet.
    pub fn new(track_id: Uuid) -> Self {
        Self {
            track_id,
            ..Default::default()
        }
    }

    /// Attach what has been derived to a track, leaving values it already carries alone.
    ///
    /// This is the one merge rule. Reconciled canonical metadata is applied first and
    /// therefore wins: a tempo the user or a provider stated is not replaced by a
    /// detected one.
    pub fn apply_to(&self, track: &mut crate::track::Track) {
        fn fill<T: Clone>(slot: &mut Option<T>, value: &Option<T>) {
            if slot.is_none() {
                slot.clone_from(value);
            }
        }
        fill(&mut track.waveform, &self.waveform);
        fill(&mut track.freq_waveform, &self.freq_waveform);
        fill(&mut track.beat_grid, &self.beat_grid);
        fill(&mut track.bpm, &self.bpm);
        fill(&mut track.key, &self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::HashSet;

    fn timestamp(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).single().unwrap()
    }

    #[test]
    fn occurrence_identity_is_stable_and_source_scoped() {
        let source_id = Uuid::from_u128(1);
        let mut first = SourceTrackOccurrence::new(source_id, "track-42");
        first.external_url = Some("https://provider.example/old".to_string());
        first.availability = SourceAvailability::StreamingOnly;
        first.last_seen_at = timestamp(10);

        let mut refreshed = SourceTrackOccurrence::new(source_id, "track-42");
        refreshed.external_url = Some("https://provider.example/new".to_string());
        refreshed.availability = SourceAvailability::Obtainable;
        refreshed.last_seen_at = timestamp(20);

        assert_eq!(first.dedup_key(), refreshed.dedup_key());

        let identities = HashSet::from([
            first.dedup_key().clone(),
            refreshed.dedup_key().clone(),
            SourceTrackIdentity::new(Uuid::from_u128(2), "track-42"),
        ]);
        assert_eq!(identities.len(), 2);
    }

    #[test]
    fn collection_relationship_identity_is_stable_and_membership_scoped() {
        let source_id = Uuid::from_u128(1);
        let identity = SourceCollectionRelationshipIdentity::new(
            source_id,
            "account-1",
            SourceCollection::LikedTracks,
            SourceEntityKind::Track,
            "track-42",
        );
        let first = SourceCollectionRelationship::new(identity.clone(), timestamp(10));
        let refreshed = SourceCollectionRelationship::new(identity.clone(), timestamp(20));

        assert_eq!(first.identity, refreshed.identity);

        let identities = HashSet::from([
            first.identity,
            refreshed.identity,
            SourceCollectionRelationshipIdentity::new(
                source_id,
                "account-1",
                SourceCollection::UploadedTracks,
                SourceEntityKind::Track,
                "track-42",
            ),
            identity.with_parent("playlist-1", Some(0)),
        ]);
        assert_eq!(identities.len(), 3);
    }

    #[test]
    fn external_playlist_entries_retain_and_sort_provider_order() {
        let source_id = Uuid::from_u128(1);
        let mut playlist = ExternalPlaylist::new(source_id, "playlist-1", "Set");
        playlist.entries = vec![
            ExternalPlaylistEntry::new(2, SourceTrackIdentity::new(source_id, "track-c")),
            ExternalPlaylistEntry::new(0, SourceTrackIdentity::new(source_id, "track-a")),
            ExternalPlaylistEntry::new(1, SourceTrackIdentity::new(source_id, "track-b")),
        ];

        playlist.sort_entries();

        let ordered_ids: Vec<_> = playlist
            .entries
            .iter()
            .map(|entry| (entry.position, entry.track.external_id.as_str()))
            .collect();
        assert_eq!(
            ordered_ids,
            vec![(0, "track-a"), (1, "track-b"), (2, "track-c")]
        );
    }

    #[test]
    fn decimal_metadata_round_trips_exact_text() {
        let value = MetadataValue::Decimal("1234567890.12345678901234567890".to_string());

        let encoded = serde_json::to_string(&value).unwrap();
        let decoded: MetadataValue = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, value);
        assert_eq!(encoded, r#"{"decimal":"1234567890.12345678901234567890"}"#);
    }

    #[test]
    fn metadata_precedence_prefers_priority_then_recency() {
        let source_id = Uuid::from_u128(1);
        let mut metadata = CanonicalMetadata::new();
        metadata.add(MetadataFieldValue::new(
            MetadataField::Title,
            MetadataValue::Text("Old provider title".to_string()),
            MetadataProvenance::provider(source_id, "track-1", timestamp(10)),
        ));
        metadata.add(MetadataFieldValue::new(
            MetadataField::Title,
            MetadataValue::Text("New provider title".to_string()),
            MetadataProvenance::provider(source_id, "track-1", timestamp(20)),
        ));

        assert_eq!(
            metadata
                .preferred(&MetadataField::Title)
                .map(|item| &item.value),
            Some(&MetadataValue::Text("New provider title".to_string()))
        );

        metadata.add(MetadataFieldValue::new(
            MetadataField::Title,
            MetadataValue::Text("User title".to_string()),
            MetadataProvenance::new(MetadataOrigin::User, timestamp(5)),
        ));

        assert_eq!(
            metadata
                .preferred(&MetadataField::Title)
                .map(|item| &item.value),
            Some(&MetadataValue::Text("User title".to_string()))
        );
    }

    #[test]
    fn metadata_precedence_distinguishes_verified_primary_embedded_tags() {
        let source_id = Uuid::from_u128(1);
        let asset_id = Uuid::from_u128(2);
        let storage_id = Uuid::from_u128(3);
        let mut metadata = CanonicalMetadata::new();
        metadata.add(MetadataFieldValue::new(
            MetadataField::Artist,
            MetadataValue::Text("Unverified tag".to_string()),
            MetadataProvenance::embedded_tag(asset_id, Some(storage_id), timestamp(30)),
        ));
        metadata.add(MetadataFieldValue::new(
            MetadataField::Artist,
            MetadataValue::Text("Provider artist".to_string()),
            MetadataProvenance::provider(source_id, "track-1", timestamp(20)),
        ));

        assert_eq!(
            metadata
                .preferred(&MetadataField::Artist)
                .map(|item| &item.value),
            Some(&MetadataValue::Text("Provider artist".to_string()))
        );

        let verified = MetadataProvenance::verified_primary_embedded_tag(
            asset_id,
            Some(storage_id),
            timestamp(10),
        );
        assert!(verified.verified_primary_embedded_tag);
        assert_eq!(verified.asset_id, Some(asset_id));
        assert_eq!(verified.storage_id, Some(storage_id));
        metadata.add(MetadataFieldValue::new(
            MetadataField::Artist,
            MetadataValue::Text("Verified primary tag".to_string()),
            verified,
        ));

        assert_eq!(
            metadata
                .preferred(&MetadataField::Artist)
                .map(|item| &item.value),
            Some(&MetadataValue::Text("Verified primary tag".to_string()))
        );
    }

    #[test]
    fn canonical_track_deduplicates_linked_occurrences() {
        let identity = SourceTrackIdentity::new(Uuid::from_u128(1), "track-42");
        let mut track = CanonicalTrack::new();

        assert!(track.link_occurrence(identity.clone()));
        assert!(!track.link_occurrence(identity));
        assert_eq!(track.occurrences.len(), 1);
    }

    #[test]
    fn canonical_playlist_uses_track_ids_and_preserves_duplicates() {
        let track_id = Uuid::new_v4();
        let other_track_id = Uuid::new_v4();
        let mut playlist = CanonicalPlaylist::new("Set");

        playlist.replace_tracks([track_id, other_track_id, track_id]);

        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(playlist.entries[0].position, 0);
        assert_eq!(playlist.entries[2].position, 2);
        assert_eq!(playlist.entries[0].track_id, playlist.entries[2].track_id);
    }
}
