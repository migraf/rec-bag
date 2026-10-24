# Data Deletion and Provider Revocation

> **TEMPLATE — NOT DEPLOYED OR LEGALLY APPROVED.** Replace every `[[REQUIRED: ...]]` item, test each procedure against the release build and production support workflow, obtain legal/privacy review, and publish on the official HTTPS domain.

**Last verified against release:** [[REQUIRED: VERSION/COMMIT AND DATE]]

**Deletion contact/form:** [[REQUIRED: PUBLIC EMAIL OR HTTPS FORM]]

## Choose the result you want

| Action | Local credentials | Local catalog | Local cache | Local primary/backups | Google Drive files | Provider account data |
| --- | --- | --- | --- | --- | --- | --- |
| Disconnect SoundCloud | Removed (remote revocation attempted; best effort when offline) | SoundCloud source and derived rows deleted; canonical owned tracks/assets preserved | Sync checkpoints deleted | Retained | Not applicable | Not deleted |
| Disconnect Google storage | Retained | Retained; storage disabled | Retained | Other local storage retained | Retained | Not deleted |
| Revoke SoundCloud access | Removed after SoundCloud confirms revocation | SoundCloud source and derived rows deleted; canonical owned tracks/assets preserved | Sync checkpoints deleted | Retained | Not applicable | SoundCloud tracks/playlists/likes not deleted |
| Revoke Google access | Removed; matching Drive storages disabled | Retained | Retained until cleared/profile deletion | Local storage retained | Retained | Google account content not deleted |
| Clear local cache | Retained | Retained; SoundCloud checkpoints or Drive cached bytes cleared as applicable | Cleared for selected connection | Originals/backups retained | Retained | Not deleted |
| Forget provider catalog data | Retained unless separately revoked | Current provider source/storage and derived rows plus current catalog snapshots deleted; canonical owned tracks/assets preserved | Provider cache for the forgotten Drive storage deleted | Owned originals/backups retained | Retained | Not deleted |
| Delete local profile | OS keyring entries require separate removal/revocation | Deleted if profile directory is deleted | Deleted with profile | Files inside profile deleted; external primaries/backups remain | Retained | Not deleted |
| Delete `rec_bag Music` in Drive | Credentials unchanged | Stale mappings remain until inventory/forget | Cached copies remain until cleared | Local copies remain | Folder/files deleted per Google behavior/trash/retention | Other Google data unchanged |

Always make and verify any desired export before deletion. Deletion can be irreversible.

## Disconnect without deleting data

In **Music Management**:

1. choose **Disconnect Google storage** for Drive;
2. confirm the connection is disabled;
3. understand that catalog data, caches, local files, backups, and provider files remain.

SoundCloud is different: **Disconnect SoundCloud** asks for confirmation, revokes access at SoundCloud (`POST /disconnect`) when reachable, removes the local credential, and deletes the SoundCloud-derived catalog data, because the SoundCloud API Terms require deleting a user's data after they disconnect. Canonical owned tracks and assets are kept; reconnecting imports the SoundCloud catalog again. Google storage disconnect intentionally retains the account credential so other matching Drive storages or reconnection can continue; use revocation to remove it.

## Revoke provider access

### Google

1. In Music Management choose **Revoke Google access** and confirm.
2. rec_bag sends the refresh token (or access token when no refresh token exists) to Google’s revocation endpoint.
3. On terminal success, rec_bag removes the OS-keyring credential, invalidates runtimes, disables every Drive storage using that account, and clears active-primary assignment if affected.
4. Verify rec_bag appears revoked in your Google Account’s third-party access settings: [[REQUIRED: CURRENT GOOGLE HELP/SETTINGS URL]].

Revocation does not delete the `rec_bag Music` folder, its audio, local catalog records, snapshots, or caches.

### SoundCloud

1. In Music Management choose **Revoke provider access** and confirm.
2. rec_bag calls SoundCloud's `POST https://api.soundcloud.com/disconnect`, refreshing an expired access token once first.
3. If remote revocation is unavailable or fails, rec_bag retains local credentials and data and reports the failure rather than claiming completion.
4. After success, the local keyring credential and the SoundCloud-derived catalog data are removed.
5. Verify/revoke rec_bag in SoundCloud connected-application settings: [[REQUIRED: CURRENT SOUNDCLOUD HELP/SETTINGS URL]].

Revocation does not delete SoundCloud uploads, playlists, likes, canonical owned tracks, local assets, or backups.

## Clear local cached data

Use **Clear local cache** for the connection:

- Google: removes local Drive playback/cache bytes and invalidates in-memory runtimes; remote Drive files and catalog records remain.
- SoundCloud: clears local synchronization checkpoints/pending OAuth state; provider-derived catalog records remain unless explicitly forgotten.

Close playback and allow the operation to finish. Verify disk usage and restart behavior.

## Forget provider-derived catalog data

Use **Forget provider catalog data** only after reviewing the confirmation:

- SoundCloud: atomically deletes the source configuration and current provider-derived account, collection, occurrence, external-playlist, sync/auth-metadata, and provider-provenance rows.
- Google Drive: disconnects the storage, purges its local cache/runtime, and atomically deletes the storage configuration and current inventory, object/tombstone, asset-location, reconciliation, transfer/job, and upload-checkpoint rows.
- Both: delete current catalog snapshot rows/triggers so pre-forget snapshots are not retained as current local recovery state.
- Both: preserve canonical tracks/playlists, owned original assets, and provider credentials unless you separately revoke/delete them.
- Neither action deletes SoundCloud account content or Google Drive remote files.

Forget is distinct from revocation. It is destructive to current local provider-derived rows and snapshots. Export anything required first, and separately delete independent/off-device snapshot copies under their own controls.

## Delete Google Drive files

rec_bag does not delete remote Drive files when you disconnect, revoke, clear cache, or forget catalog data.

To remove them:

1. export or copy anything you want to retain;
2. open Google Drive while signed into the connected account;
3. locate the app-created **`rec_bag Music`** folder;
4. delete the intended files/folder and empty trash if immediate permanent deletion is desired and permitted;
5. review Google retention, backup, Vault/organization, and account policies;
6. clear local cache and forget/reinventory catalog mappings in rec_bag as desired.

[[REQUIRED: LEGAL/PRODUCT REVIEW — link current Google deletion/trash documentation and describe managed Workspace exceptions.]]

## Delete local rec_bag data

1. Stop rec_bag and ensure no second manager is active.
2. Revoke provider grants first if you want future API access terminated.
3. Preserve any requested export.
4. Delete the rec_bag profile directory (`REC_BAG_DATA_DIR` or platform default), including `library.db`, recovery snapshots, cache, staging, and lease files.
5. Delete any external local primary/backup directories separately if desired.
6. Remove residual `rec_bag` provider entries from the OS credential store if revocation did not complete.
7. Delete independent copies, system backups, and support bundles according to their own controls.

Deleting the profile does not delete external music folders, Google Drive files, SoundCloud data, provider-account records, or copies held in independent backups.

## Export your data

Before deletion:

- preserve the complete local profile directory while rec_bag is stopped;
- preserve recovery/catalog snapshot envelopes and configured backup snapshots;
- export/copy original audio from local or Google Drive storage;
- use provider-native exports for Google/SoundCloud account data when broader provider records are required.

Recovery snapshots contain catalog metadata and mappings but no provider tokens. Restore exactly replaces snapshot-owned catalog tables; it does not merge with destination rows. They are designed for rec_bag recovery and may not be a complete human-readable or legally sufficient portability export. Moving a snapshot/catalog to another device requires an explicit confirmed authority handoff before it can become the portable managing authority.

Request an assisted export at **[[REQUIRED: EXPORT REQUEST EMAIL/FORM]]**. Include **[[REQUIRED: NON-SECRET IDENTIFIERS]]** and never send access/refresh tokens, OAuth callbacks, keyring dumps, or raw credentials. We verify identity using **[[REQUIRED: METHOD]]** and target a response within **[[REQUIRED: SLA/LEGAL PERIOD]]**.

## Request deletion from the operator

Send a request through **[[REQUIRED: PUBLIC DELETION FORM/EMAIL]]** with:

- the account/contact identifier needed for verification;
- which hosted support/security records you want deleted;
- whether you also request an export;
- no passwords, provider tokens, or music files unless explicitly requested through a secure channel.

We will:

1. acknowledge within [[REQUIRED: PERIOD]];
2. verify identity using [[REQUIRED: METHOD]];
3. identify operator-controlled systems and processors;
4. complete deletion or explain a lawful retention exception within [[REQUIRED: PERIOD]];
5. notify processors where required;
6. provide completion/appeal information.

The operator cannot remotely erase a local desktop profile or external storage that it does not control. You must complete those device/provider steps above.

## Retention exceptions

[[REQUIRED: LEGAL REVIEW — fraud/security records, transaction/tax records, litigation holds, backups, processor lag, anonymization, statutory retention, and appeal rights. Include exact periods.]]

## Help

See [Support](support.md) or contact **[[REQUIRED: SUPPORT/PRIVACY EMAIL]]**. This page is a template and no response channel or deletion SLA is represented as active until the placeholders are completed and the page is published.
