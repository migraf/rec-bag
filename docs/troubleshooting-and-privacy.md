# Troubleshooting, data, and privacy

## Fast diagnosis

Run offline checks first:

```sh
cargo check -p desktop --features desktop
cargo test -p desktop
cargo test -p api --features server
cargo test -p soundcloud
cargo test -p music_storage_google_drive
cargo test -p music_storage_local
```

Do not paste `.env`, OAuth callbacks, provider responses, keyring exports, `library.db`, or catalog snapshots into public issues. Redact account IDs, folder IDs, file names, paths, and source metadata when they identify a person or music library.

## Startup and catalog

### Cannot determine application data directory

Set an absolute writable `REC_BAG_DATA_DIR`. Check parent permissions, disk space, and ownership. Avoid world-readable or shared directories.

### Catalog migration failed

Stop all processes. Preserve the complete profile and `library.db.pre-provider-v1.bak` if present. Do not retry with multiple app versions or delete WAL files while a process is running. A future-schema error means the catalog was opened by a newer rec_bag; use that version or restore a pre-upgrade copy.

### Migrated library needs folder selection

Select the existing common source root. rec_bag requires migrated paths to be absolute, existing files under one enabled source root. Adoption does not move files.

## Keyring

### Credential store unavailable / authorization repeatedly required

- Confirm the native app was built with `--features desktop`.
- Linux: verify a Secret Service provider and unlocked login session are available to the packaged process.
- macOS: inspect Keychain prompts and code-signing/bundle identity changes.
- Windows: run under the same user profile that authorized the provider.
- Do not run GUI rec_bag under `sudo` or another service account.

A copied catalog does not include tokens. Reauthorize on the destination device. Do not insert tokens into SQLite.

## SoundCloud

### “SoundCloud is not configured”

Set `REC_BAG_SOUNDCLOUD_CLIENT_ID`, confirm the desktop redirect, restart, and ensure the broker uses the matching application client ID/secret and exact allow-list entry.

### Callback port cannot bind or authorization times out

The default listener is `127.0.0.1:17841` for 180 seconds. Close the conflicting process, permit loopback in endpoint security, and verify the registered path is `/oauth/soundcloud`. Do not change only one of desktop, provider registration, and broker allow-list.

### Broker returns 503 before contacting SoundCloud

Verify the trusted proxy replaces `X-Real-IP` with exactly one IP and the origin is not being called directly. Missing, malformed, or comma-separated values fail closed.

### Rate limited or temporary failure

Honor retry/backoff and inspect edge plus per-instance limits. Do not retry authorization in a tight loop. Broker restarts reset the in-process limiter, so edge limits remain mandatory.

### Refresh fails after partial credential changes

SoundCloud refresh tokens are single-use. The desktop serializes refresh under a lock and writes the complete new token generation as one keyring value. If storage failed after provider rotation, require reauthorization; never retry an already-consumed token from logs or SQLite.

## Google Drive

### OAuth configuration missing

Set `REC_BAG_GOOGLE_CLIENT_ID` to an installed-app client ID. Register the exact loopback redirect (default `http://127.0.0.1:17842/oauth/google`) and restart.

### Callback port or state failure

The loopback URL must be plain HTTP on localhost/loopback with an explicit port. The listener binds `127.0.0.1`, times out after 180 seconds, validates the callback path, and consumes state once. Restart the authorization flow; never reuse a callback URL.

### Dedicated folder creation returns 403/404

Confirm Drive API is enabled, consent granted `drive.file`, the account can create files in My Drive, and organization policy permits the app. rec_bag creates and validates `rec_bag Music`; do not broaden to full Drive scope as a troubleshooting shortcut.

### Shared Drive or out-of-folder files are missing

This is expected. Shared Drives are disabled, and inventory is constrained to the app-created `rec_bag Music` folder subtree.

### Playback cache is corrupt/full

Cache entries are disposable and checksum verified. Stop playback, remove only the affected storage cache under `cache/google-drive/<storage-id>/`, then allow a fresh download. Never delete primary/backup Drive files to fix cache pressure.

## Local/backup behavior

### Storage appears empty while disconnected

Do not rescan an empty mount point as authoritative. Restore the mount/network connection at the same path. Known inventory and queued work should be retained during temporary unavailability.

### External change is not replicated

Expected: changed primary files become pending review. Approve the new verified version or restore the prior version before backup propagation.

### Backup remains pending

Check storage health, credentials, free space, durable job state, scheduler error state, and profile ownership. The scheduler operates only while the desktop app is running and owns the profile; temporary failures back off from 5 seconds to 15 minutes. Use the relevant manual action if immediate retry is safe. Never mark a backup complete solely because the destination file name exists; verify checksum/provider version.

### Recovery contains rows that are not in the selected snapshot

This is not expected. Recovery uses exact transactional replacement, so snapshot-owned destination rows absent from the selected snapshot must be deleted. Stop the app, preserve the profile and pre-restore snapshot, and do not accept the restore. Rollback must exactly replace the catalog from the pre-restore snapshot. On another device, also verify an explicit confirmed authority handoff occurred.

### Cleanup did not free space

Retention keeps 30 days and at least three versions and requires exact approval. Drive/provider version deletion support can vary. Re-run review; do not bypass catalog approvals with direct bulk deletion.

## Data and privacy implications

### Stored locally

- `library.db`: track/playlist metadata, source relationships, paths/app-created Drive folder IDs, checksums, provider object IDs, jobs, errors, settings, and non-secret credential references.
- catalog and recovery snapshots: most of the same metadata and operational history, with secret-field rejection and without token values.
- Drive cache/staging: temporary full audio content.
- local primary/backups: normal unencrypted audio files.
- OS keyring: SoundCloud and Google user token material.

Local files and snapshots are not client-side encrypted. Use encrypted disks, private OS accounts, restricted filesystem permissions, encrypted backup transport, and appropriate physical security. File names, listening/library structure, source URLs, folder IDs, and checksums may be personal data even without tokens.

### Network destinations

Depending on configured features, desktop/server communicate with:

- the deployed Dioxus fullstack server for SoundCloud broker calls;
- `secure.soundcloud.com` for authorization/token exchange;
- SoundCloud API endpoints for account/catalog metadata;
- `accounts.google.com` and `oauth2.googleapis.com` for Google OAuth;
- `www.googleapis.com` / Drive upload endpoints for folder metadata, inventory, uploads, downloads, and changes.

Audio bytes leave the device when Google Drive is primary/backup. SoundCloud catalog synchronization sends OAuth bearer credentials to SoundCloud and receives account/library metadata. Broker exchanges temporarily process authorization codes or refresh tokens in memory. Proxy/APM vendors can observe metadata unless body/query capture is disabled.

### Retention and deletion

Disconnect is non-destructive to catalog/assets/provider files: SoundCloud disconnect removes its local credential after sign-out handling, while Google storage disconnect retains the account credential for other/reconnected Drive storage. Revocation removes the relevant local credential and disables affected connections but does not delete remote content. Confirmed provider forget deletes current provider-derived catalog/storage rows and current catalog snapshot rows while preserving canonical owned assets and remote files. Independently exported/off-device snapshots and backups require separate deletion. Primary deletion does not cascade to backups. Define operator retention for profile backups, logs, and support artifacts, and honor user deletion/export requests across each location.

### Logs and support

Use structured IDs and safe classifications, not secrets or provider bodies. Exclude request/response bodies for OAuth routes. Treat catalog databases, snapshots, paths, metadata, and file hashes as sensitive. If exposure is suspected, rotate the server client secret, revoke affected user grants, roll all broker instances, restrict and purge logs under policy, and correct capture/redaction before reopening traffic.

Public disclosure templates are [Privacy Policy](privacy-policy.md), [Terms of Service](terms-of-service.md), [Data Deletion](data-deletion.md), and [Support](support.md). They require completed placeholders, deployment verification, and legal/privacy approval before publication.
