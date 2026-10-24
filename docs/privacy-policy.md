# Privacy Policy

> **TEMPLATE — NOT DEPLOYED OR LEGALLY APPROVED.** Replace every `[[REQUIRED: ...]]` item, verify the statements against the production deployment and vendors, obtain legal/privacy review, and publish on the official HTTPS domain before use.

**Effective date:** [[REQUIRED: YYYY-MM-DD]]

**Last updated:** [[REQUIRED: YYYY-MM-DD]]

## 1. Who we are

rec_bag is operated by **[[REQUIRED: LEGAL ENTITY/CONTROLLER NAME]]**, located at **[[REQUIRED: POSTAL ADDRESS AND COUNTRY]]** (“we,” “us,” or “our”).

Privacy contact: **[[REQUIRED: PRIVACY EMAIL]]**

Support: [Support](support.md) / **[[REQUIRED: PUBLIC SUPPORT URL]]**

Data deletion: [Data Deletion](data-deletion.md) / **[[REQUIRED: PUBLIC DELETION URL]]**

[[REQUIRED: LEGAL REVIEW — identify controller/processor roles, representative, DPO, and applicable privacy laws.]]

## 2. Scope

This policy covers the rec_bag desktop application, the hosted SoundCloud OAuth broker, official support channels, and the public website at **[[REQUIRED: PUBLIC DOMAIN]]**. SoundCloud and Google process data under their own policies when you connect those services.

## 3. Data we access and use

### Local catalog and music storage

rec_bag processes:

- track, artist, album, playlist, tag, duration, codec, and analysis metadata;
- local file paths, storage configuration, provider object IDs, sync/job history, errors, and settings;
- SHA-256 and provider checksums used to identify and verify files;
- original audio files in a local primary or backup selected by you;
- temporary/staged files and a size-limited Google Drive playback/analysis cache;
- recovery snapshots containing catalog records, relationships, mappings, checksums, non-secret configuration, and job state.

Purposes: organize your library, reconcile catalog entries with owned audio, play/analyze audio, migrate primary storage, replicate backups, detect changes, restore the catalog, and prevent concurrent profile writers.

### SoundCloud data

When you connect SoundCloud, rec_bag accesses and stores as needed:

- OAuth access and refresh tokens, token expiry/type/scope, and non-secret refresh-generation metadata;
- connected account identity and provider IDs;
- tracks uploaded by the account;
- playlists created by the account and ordered playlist entries;
- liked tracks and liked playlists with ordered entries;
- track/playlist metadata, URLs, availability/access state, pagination/sync cursors, and sync errors.

Purposes: display a read-only SoundCloud catalog, maintain stable relationships, identify missing owned originals, and report catalog/archival coverage. rec_bag does **not** use SoundCloud playback transcodes as archival primary files. It does not delete SoundCloud tracks, playlists, or likes through catalog-forget actions.

### Google data

When you connect Google Drive, rec_bag requests only `https://www.googleapis.com/auth/drive.file` and processes:

- OAuth access and refresh tokens, token expiry/scope/type, and an account credential reference;
- the stable ID and name of the app-created My Drive folder `rec_bag Music`;
- files and folders created by or made available to rec_bag under that folder, including IDs, names, parent relationships, MIME type, size, timestamps, versions, checksums, download capability, change records, and private rec_bag `appProperties`;
- audio bytes that you upload to Drive primary/backup storage or download into the verified local cache.

Purposes: create and manage your dedicated music-storage folder, inventory and verify assets, upload/download originals and backups, detect changes, play/analyze cached files, and restore data. rec_bag does not request broad full-Drive scope and does not support Shared Drives.

Use of information received from Google APIs must comply with the [Google API Services User Data Policy](https://developers.google.com/terms/api-services-user-data-policy), including Limited Use requirements. [[REQUIRED: LEGAL/COMPLIANCE REVIEW — confirm this statement and all actual data uses.]]

### Hosted broker and website data

The SoundCloud broker temporarily receives authorization codes, PKCE verifiers, state values, or refresh tokens to exchange them with SoundCloud. It is designed not to persist user tokens or catalog data. Edge systems process client IP address, timestamps, route, status, and security/rate-limit metadata.

[[REQUIRED: LIST production hosting, CDN/WAF, logging, crash reporting, analytics, support, email, and other processors; specify fields collected by each.]]

## 4. Where data is stored

- Provider tokens: the operating system credential store on the desktop device.
- Catalog: local `library.db` in the rec_bag profile directory.
- Recovery/catalog snapshots: local profile and configured backup storages, without token values; recovery rejects prohibited secret fields.
- Audio: local primary/backups and/or the app-created `rec_bag Music` Drive folder, according to your configuration.
- Drive cache/staging: local profile directory.
- SoundCloud broker: secrets in the server secret manager; request credential material only in memory for the exchange.
- Service/security logs: [[REQUIRED: LOCATIONS, PROVIDERS, AND REGIONS]].

Local catalog, snapshots, cache, and audio files are not client-side encrypted by rec_bag. Protect them with OS accounts, filesystem permissions, disk encryption, and secure backup practices.

## 5. Sharing and disclosure

We disclose data only as needed to operate the requested feature:

- **SoundCloud:** OAuth/token requests and authenticated catalog API requests.
- **Google:** OAuth, token revocation, and Drive API requests for the app-created folder and its files.
- **Infrastructure/service providers:** [[REQUIRED: PROCESSOR LIST, PURPOSE, DATA, REGION, AND POLICY LINKS]].
- **Legal/safety disclosures:** [[REQUIRED: LEGAL REVIEW AND APPLICABLE STANDARD]].
- **Business transfer:** [[REQUIRED: LEGAL REVIEW]].

[[REQUIRED: CONFIRM AFTER BUSINESS/LEGAL REVIEW — whether data is sold, shared for cross-context behavioral advertising, or used for advertising/training. Do not publish an unsupported “we never sell” claim.]]

We do not provide Google user data to third parties except as necessary to provide or improve user-facing features, comply with law, protect security, or as otherwise permitted by Google policy. [[REQUIRED: LEGAL/COMPLIANCE REVIEW.]]

## 6. Retention

- OAuth tokens remain in the OS credential store until you revoke access, perform an action that removes credentials, delete the OS credential entry/profile, or the provider invalidates them. Disconnecting a Google storage alone retains its account credential; use **Revoke Google access** to revoke and remove it.
- Provider-derived catalog/storage rows remain until you use the explicit catalog-forget action or delete the local profile. Forget atomically deletes the current provider source/storage configuration and derived relationships, inventory, locations, reconciliation/operational rows, provider provenance, and current catalog snapshot rows/triggers. It preserves canonical owned tracks/assets and credentials unless separately removed.
- Local cache remains until eviction, **Clear local cache**, or profile deletion.
- Local audio remains until you delete it. Primary deletion does not automatically delete backup copies.
- Files in `rec_bag Music` remain in Google Drive after disconnect, revocation, cache clearing, or local catalog-forget; delete them in Drive if desired.
- Backup version defaults retain replaced/deleted versions for 30 days and at least the three most recent versions, with explicit cleanup confirmation.
- Provider forget deletes current catalog snapshot rows/triggers. Independently stored/exported recovery snapshot files and profile backups remain until separately deleted under your/operator retention process.
- Broker/security/support logs: [[REQUIRED: EXACT RETENTION PERIODS AND DELETION PROCESS]].
- Closed-account/request records: [[REQUIRED: LEGAL RETENTION PERIOD AND BASIS]].

## 7. Your controls, deletion, revocation, and export

In Music Management you can:

- disconnect a Google storage without deleting catalog data or provider files; disconnecting SoundCloud also deletes its SoundCloud-derived catalog data on this device;
- revoke provider access, removing local credentials and disabling affected connections after provider revocation succeeds;
- clear local cached bytes without deleting catalog or provider content;
- explicitly forget and delete current local provider-derived catalog/storage rows and current snapshot records while preserving canonical owned assets and without deleting provider files;
- review and approve backup-version cleanup separately.

You can also revoke rec_bag from your Google Account or SoundCloud account settings. Provider-side revocation stops future access but does not delete local catalog/snapshots or remote Drive files.

For complete procedures, consequences, local-profile deletion, remote-file deletion, and export, see [Data Deletion](data-deletion.md). Requests may be sent to **[[REQUIRED: PRIVACY/DELETION EMAIL OR FORM]]**. We will verify requests using **[[REQUIRED: VERIFICATION METHOD]]** and respond within **[[REQUIRED: PERIOD CONSISTENT WITH APPLICABLE LAW]]**.

Available data may be exported by preserving the local profile/catalog and recovery snapshots before deletion/forget. Recovery restore is exact replacement, not merge, and a copied snapshot does not transfer portable authority without an explicit confirmed handoff. [[REQUIRED: PRODUCT/LEGAL REVIEW — specify the supported user-facing export format, scope, delivery method, authentication, and response SLA; recovery snapshots may not satisfy all portability obligations.]]

## 8. Security

Controls include PKCE/state OAuth flows, exact redirect validation, OS keyring storage, secret-field rejection in recovery snapshots, checksum verification, trusted-proxy enforcement, rate limiting, secret-manager storage, and log redaction requirements. No system is completely secure.

Security contact: **[[REQUIRED: SECURITY EMAIL/REPORTING URL]]**.

## 9. International transfers

[[REQUIRED: IDENTIFY DATA REGIONS, TRANSFER MECHANISMS, PROCESSORS, AND SAFEGUARDS AFTER LEGAL REVIEW.]]

## 10. Children

[[REQUIRED: AGE REQUIREMENT, CHILDREN’S PRIVACY POSITION, AND JURISDICTION-SPECIFIC REVIEW.]]

## 11. Legal bases and rights

[[REQUIRED: LEGAL REVIEW — list legal bases and jurisdiction-specific rights, including access, correction, deletion, restriction, objection, portability, withdrawal of consent, appeal, and regulator complaint details.]]

## 12. Changes

We will post updates at **[[REQUIRED: FINAL PRIVACY POLICY URL]]** and change the “Last updated” date. [[REQUIRED: DEFINE material-change notice method.]]

## 13. Contact

**[[REQUIRED: LEGAL ENTITY]]**

**[[REQUIRED: POSTAL ADDRESS]]**

**[[REQUIRED: PRIVACY EMAIL]]**

**[[REQUIRED: REGULATOR/DPO/REPRESENTATIVE DETAILS IF APPLICABLE]]**
