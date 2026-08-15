# Storage and operations

## Storage model

A profile has zero or one active primary and any number of backups. Local directories and Google Drive folders can fill either role.

- **Primary:** authoritative original audio assets.
- **Backup:** verified replicas plus catalog snapshots; never promoted automatically.
- **Drive cache:** disposable, size-limited local playback/analysis cache; not a backup.
- **Catalog:** local `library.db`; contains metadata, mappings, jobs, checksums, and non-secret provider configuration.

Existing folders are inventoried in place. rec_bag does not move files merely because a folder is selected. Source copy policy defaults to `AskBeforeCopying`; alternatives are `CheckOnly` and `AutomaticallyCopyEligibleFiles`.

A verified primary write queues automatic backups. Scheduled and manual replication modes are also modeled. Unavailable storage retains its last known inventory and pending work; absence is not interpreted as deletion.

## Local primary and backup

1. Select a dedicated directory with enough free space and stable mount identity.
2. Ensure only the desktop user can write it. Avoid sync tools that rewrite metadata or files concurrently.
3. Inventory before accepting it as primary.
4. Review unknown files and possible matches. Only SHA-256, persisted association, stable rec_bag ID, or a rec_bag copy/upload operation is strong verification.
5. Test a write, checksum verification, restart, and read/playback.

On removable/network storage, mount at a stable path before launching rec_bag. If unavailable, leave it configured and disabled/deferred; do not point the storage record at an empty replacement directory.

## Google Drive primary and backup

After Google authorization, rec_bag creates a dedicated My Drive folder named `rec_bag Music`. It limits traversal to that app-created folder subtree, uses private `appProperties`, and disables Shared Drive behavior. Drive primary playback downloads to a verified local cache (default configured maximum: 10 GiB per storage).

Before production use:

1. Confirm `drive.file` consent and successful creation of the dedicated `rec_bag Music` folder.
2. Upload and download a non-sensitive test file.
3. Verify restart/refresh-token behavior through the OS keyring.
4. Confirm the file remains private and under the app-created folder.
5. Exercise cache eviction and offline failure behavior.

Do not treat Google revision history or the Drive cache as an independent backup. Configure a second storage/provider and test restore.

## Primary migration

Use only the application migration flow:

1. health-check and inventory the candidate;
2. reconcile already matching assets;
3. copy missing assets to temporary destinations;
4. verify required SHA-256 checksums;
5. create a catalog snapshot;
6. review final status and confirm;
7. atomically switch roles;
8. retain the old primary as a backup by default;
9. run backup-gap reconciliation.

Do not manually edit storage roles in SQLite. Do not delete or repurpose the old primary until catalog, asset, and playback checks pass and an independent backup is verified.

## Catalog schema migration

At desktop startup, `library.db` is opened with foreign keys and WAL enabled and migrated transactionally. A legacy v1 database is checkpointed and copied once to:

```text
library.db.pre-provider-v1.bak
```

A failed migration rolls back. A database newer than the application is rejected. The migration preserves track/playlist identities and paths and does not choose a primary or move audio.

For a legacy local library, startup attempts in-place adoption only when all migrated absolute files exist under one enabled configured local source root. Otherwise Music Management asks the user to select the existing folder; this remains non-moving.

Pre-upgrade procedure:

1. stop every rec_bag process;
2. copy the entire profile directory, including `library.db`, `-wal`, and `-shm` if present;
3. record the application version and profile path;
4. upgrade and launch once;
5. verify catalog counts, playlists, active primary, source roots, and several checksums/playbacks;
6. retain the pre-upgrade copy until backup verification completes.

Never downgrade against a migrated catalog. Restore the complete pre-upgrade profile copy instead.

## Scheduler and cadences

The scheduler implementation defines these defaults:

| Work | Default |
| --- | --- |
| Durable-job reclaim/poll | startup; then 1 minute idle polling or authoritative next-due time |
| SoundCloud sync | startup and every 6 hours |
| Automatic backup planning | startup and every 15 minutes |
| Scheduled backup planning | startup and every 24 hours |
| Rotating verification | startup and daily UTC boundary |
| Catalog snapshot | startup and daily UTC boundary |
| Complete backup inventory | next month UTC boundary, then monthly |
| Temporary-unavailability retry | exponential 5 seconds to 15 minutes |
| Shutdown checkpoint timeout | 30 seconds |

The runtime starts the scheduler after ownership acquisition, immediately reclaims durable work, and starts SoundCloud sync, backup planning, daily verification/snapshots, and monthly inventories. The scheduler runs only while the desktop app is running and still owns the profile. Immediate verification also occurs after writes/replication. On graceful shutdown it checkpoints due snapshot work with a 30-second scheduler bound; the desktop allows up to 35 seconds for scheduler shutdown.

## Snapshots, retention, and cleanup

Catalog snapshots include tracks, playlists, source relationships, storage mappings, checksums, jobs/settings, and non-secret refresh metadata. They exclude token values and keyring material. Snapshot payloads are SHA-256 integrity checked and deduplicated logically. Backup copies use `.rec_bag/catalog-snapshots/v1`.

Snapshot triggers include successful sync/replication, reconciliation changes, debounced metadata/playlist changes, before primary migration, before retention cleanup, and daily use. Current snapshot debounce is five minutes.

Backup retention defaults:

- retain replaced/deleted versions for 30 days;
- retain at least three recent versions regardless of age;
- require explicit cleanup confirmation;
- never cascade primary deletion into backups.

Review exact storage/object/version candidates before approval. Take a snapshot immediately before cleanup. Permanent deletion is not a space-pressure automation.

## Forget provider data

The confirmed **Forget provider catalog data** action is destructive to current local provider-derived state:

- SoundCloud deletes the source row and its account, occurrence, collection, external-playlist, sync, auth-metadata, and provider-provenance rows.
- Google Drive disconnects the storage, purges its local cache/runtime, and deletes the storage row, inventory, locations, reconciliation findings, transfer/job history, and upload checkpoints.
- Both paths clear current catalog snapshot rows/triggers so stale snapshots are not presented as the current post-forget state.
- Canonical tracks/playlists, owned original assets, and credentials are preserved unless separately revoked/deleted.
- No SoundCloud or Google Drive remote file/account content is deleted. Independent exported/off-device snapshot copies require separate retention/deletion handling.

Create any required export before forgetting provider data. A new post-forget snapshot can be generated after the transaction commits.

## Recovery

Recovery snapshots are stored under `recovery/catalog-snapshots/`. The runtime lists valid and invalid envelopes, verifies SHA-256 integrity, format/content type, JSON shape, and absence of prohibited secret fields, then dry-runs import against a migrated in-memory catalog.

For a real restore, rec_bag requires current profile ownership, creates an immutable pre-restore snapshot, writes a durable restore marker, and exactly replaces every snapshot-owned catalog table with the selected snapshot in one database transaction. Rows absent from the selected snapshot are deleted; schema and migration bookkeeping remain. Any failure rolls back the whole replacement. Startup detects interrupted markers and exactly replaces the catalog from the verified pre-restore snapshot when necessary. User-requested rollback performs the same exact transactional replacement from that pre-restore snapshot.

Safe procedure:

1. stop other rec_bag instances and preserve the current profile;
2. inspect snapshot health and select a known snapshot;
3. run dry-run restore first;
4. review ownership/handoff details, then perform restore;
5. reconnect SoundCloud and Google if credentials are absent, because snapshots never contain tokens;
6. inventory the primary and compare SHA-256/provider versions;
7. review unknown, missing, changed, and ambiguous assets;
8. verify backups and playback;
9. retain the reported pre-restore snapshot until acceptance;
10. use rollback if validation fails.

Recovery is exact replacement, not merge/upsert. The restored snapshot becomes the complete catalog authority for all snapshot-owned tables. Preserve the original profile and pre-restore snapshot until the replacement is accepted.

## Ownership takeover

The desktop acquires a 30-second profile lease before catalog open/migration and renews it every 10 seconds. Mutating operations and scheduled tasks verify ownership; loss of ownership stops mutation and begins shutdown. Lease writes are atomic, release is session-safe, stale takeover is supported, and live takeover uses a five-minute confirmation bound to the observed owner. Lease files are:

```text
profile-manager.lease.json
profile-manager.lease.guard
```

Safe transfer procedure:

1. finish/checkpoint jobs and close the old device;
2. copy or restore the catalog to the new private profile directory;
3. reconnect provider credentials on the new device;
4. inventory primary and backups without writes;
5. confirm the old process cannot restart or write;
6. only then enable management on the new device.

An unowned or expired local lease is acquired normally. For a live lease, the app blocks startup, displays the owner device/session, and requires explicit takeover confirmation within five minutes. Copying or restoring a catalog does not transfer portable catalog authority by itself: another-device use requires an explicit confirmed handoff identifying the snapshot’s previous manager and a current lease held by the receiving device. Never delete a corrupt lease or force takeover without preserving it and proving no live writer exists. Concurrent multi-device writers are unsupported.
