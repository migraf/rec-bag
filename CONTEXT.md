# rec_bag

rec_bag is a desktop music-catalog and storage manager. It reconciles what you *have* (audio files spread across local folders and cloud storage) with what you *know about* (tracks and playlists observed at providers such as SoundCloud), and keeps one authoritative copy of every file you own.

The central distinction in this language is between **discovery** and **custody**. A Catalog Source is somewhere rec_bag *looks*; a Music Storage is somewhere rec_bag *keeps things*. Terms are grouped accordingly.

One folder may be both — a Google Drive folder can be scanned as a source and also hold owned files as a storage. When it is, that is two separate configurations that happen to share a root, not one configuration with two roles. Sources and storages have independent identities and neither refers to the other.

## Language

### The catalog

**Catalog**:
The local authoritative record of tracks, playlists, sources, assets, and storage locations. The catalog holds knowledge about music, never the audio itself.
_Avoid_: Database, index, library

**Catalog Source**:
A configured provider-neutral location where rec_bag discovers and observes music, such as a local folder or a SoundCloud account. Discovering a file at a source does not put rec_bag in custody of it.
_Avoid_: Music source, source (unqualified), provider, backend

**Copy Policy**:
What a source does when a file it can obtain is absent from primary storage. The copy policy is the seam by which discovered music becomes owned music.

**Source Account**:
An account exposed by a catalog source. Account identity is scoped to its source, because two providers may issue the same external identifier.

**Source Track Occurrence**:
One provider's instance of a track, identified by the pair of its source and its provider-assigned external ID. Everything else about it — title, URL, availability — is an observation, not identity.
_Avoid_: Remote track, provider track, external track

**Availability**:
What a source currently permits for an occurrence, ranging from obtainable through streaming-only to removed. Availability is observed and may change without the occurrence changing.

**Canonical Track**:
The provider-neutral identity of a piece of music, together with metadata reconciled from every occurrence linked to it. A canonical track deliberately has no filesystem path.
_Avoid_: Master track, merged track, deduplicated track

**Metadata Candidate**:
One observed value for one metadata field, carrying its provenance. Canonical values are selected from competing candidates rather than overwritten.

**Embedded Tags**:
What one audio file's own tags and audio properties say about it, read off the file itself. Embedded tags are observed from an asset rather than reported by a source, so a scan turns them into metadata candidates and reconciliation decides which of them become canonical.
_Avoid_: File metadata, ID3, tags (unqualified)

**Library Entry**:
The user's own record about a canonical track — rating, comment, play count, last played, and when it was added. A library entry is authored or accumulated locally; it is never observed from a provider and never reconciled against one.
_Avoid_: User metadata, personal metadata, play stats

**Canonical Playlist**:
An editable rec_bag playlist whose membership references canonical tracks by identity.
_Avoid_: Playlist (unqualified), library

**External Playlist**:
A playlist imported from a catalog source. External playlists are read-only observations and stay that way until explicitly copied into a canonical playlist.
_Avoid_: Imported playlist, remote playlist, synced playlist

**Catalog Projection**:
The live, in-memory reading of the catalog that the running application renders — its tracks, playlists, and sources. The projection is derived from the catalog and never authoritative over it.
_Avoid_: Cache, model, store

**Library Browser**:
One view's browse over the catalog projection: where it is looking, what it is searching for, and what the user has selected. Each view has its own browser, because looking at a playlist and looking at a folder are different places.
_Avoid_: Track list (that is the component that renders one), selection state

### Playback

**Loaded Track**:
A canonical track prepared for playback: a decoded audio file at a concrete path, with its analysis attached. A loaded track is a moment in the life of a track, not a record of one — it exists only while loaded.
_Avoid_: Deck track, playing track, track (unqualified)

**Track Analysis**:
The waveform, frequency bands, beat grid, tempo, and key derived from a track's audio. Analysis is reproducible from any original asset, so it is cached rather than owned, and it is attached to a loaded track.
_Avoid_: Waveform (unqualified), beat data

**Deck**:
One playback channel that a loaded track occupies. Decks are what the DJ view mixes between.

### Files and custody

**Music Storage**:
A configured location that holds actual music files, such as a local directory or a Google Drive folder. A storage is defined by its root identifier and its capabilities, never by credentials.
_Avoid_: Destination, drive, disk, repository

**Storage Capabilities**:
What a storage adapter can actually do for the selected root, account, and permissions. Capabilities are established per configuration and must never be inferred from the kind of storage alone.

**Storage Role**:
The responsibility assigned to a music storage: Primary, Backup, or Unassigned.

**Primary**:
The single storage that holds the authoritative copy of every owned file. At most one storage is primary; there is none only before the first is set up, and from then on a primary can be replaced but never removed.
_Avoid_: Main, master, source of truth

**Backup**:
A storage holding replicated copies of primary content, subject to a retention policy. A backup never satisfies a request the primary should serve: when the primary lacks a file that a backup holds, the file is restored to the primary rather than read from the backup.

**Asset**:
A file belonging to a canonical track. Every asset is either an original or a derived asset; the distinction determines whether losing it loses anything.
_Avoid_: File, media, item

**Original Asset**:
An owned audio file that contributes to archival completeness, regardless of its format. Originals are either the Master or an Alternate; both count as originals.

**Derived Asset**:
A file reproducible from an original — a playback transcode, offline cache, analysis artifact, or waveform. Derived assets never contribute to archival completeness and may be regenerated or discarded freely.
_Avoid_: Cache, temp file, artifact (unqualified)

**Archival Completeness**:
Whether every canonical track has at least one original asset present and verified. This is the property the storage side of rec_bag exists to protect.

**Content Hash**:
A digest paired with the algorithm that produced it, used as content identity or as provider-supplied evidence.
_Avoid_: Checksum, fingerprint, digest (unqualified)

**Storage Object**:
Provider-neutral metadata for one object as the storage itself reports it. A storage object is what the provider says exists, independent of what the catalog expects.

**Asset Location**:
The persisted association between a catalog asset and a storage object. The location is the claim; the storage object is the evidence.

**Inventory**:
A point-in-time enumeration of the objects a storage holds, captured as a generation so successive enumerations can be compared.
_Avoid_: Scan, listing, crawl

**Replication**:
Copying primary content to a backup according to its replication mode and retention policy.
_Avoid_: Sync (see below), backup as a verb, mirroring

### Reconciling the two

**Reconciliation**:
Determining how the catalog's expectations relate to what storages actually hold, and producing findings for the differences.
_Avoid_: Matching, syncing, diffing

**Match Candidate**:
A possible association between a canonical track or asset and a storage object, supported by evidence and carrying a confidence.

**Match Evidence**:
One fact supporting a candidate, together with the method that produced it. Evidence accumulates; confidence is derived from it.

**Resolution**:
A durable user or worker decision recorded against a reconciliation finding. Resolutions persist across reconciliation runs.

**Suppression**:
A resolution's lasting effect of keeping a rejected or ignored finding from resurfacing. A suppression without an expiry stands until explicitly removed.

**Sync Run**:
One top-level synchronization operation against sources or storages, composed of individually restartable sync jobs.
_Avoid_: Sync, job (unqualified), task

**Recovery Snapshot**:
A captured catalog state from which the catalog can be restored. Snapshots protect the catalog, not the music; assets are protected by replication.
