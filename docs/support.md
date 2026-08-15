# rec_bag Support

> **TEMPLATE — NOT DEPLOYED OR OPERATIONALLY APPROVED.** Replace every `[[REQUIRED: ...]]` item, staff and test each channel, define service levels, complete legal/privacy review, and publish on the official HTTPS domain before use.

## Contact

- Support portal: **[[REQUIRED: PUBLIC HTTPS SUPPORT URL]]**
- Support email: **[[REQUIRED: SUPPORT EMAIL]]**
- Privacy/data requests: **[[REQUIRED: PRIVACY EMAIL OR FORM]]**
- Security reports: **[[REQUIRED: SECURITY EMAIL OR DISCLOSURE URL]]**
- Postal contact: **[[REQUIRED: LEGAL ENTITY AND ADDRESS]]**
- Supported languages/hours: **[[REQUIRED: LANGUAGES, TIME ZONE, HOURS, HOLIDAYS]]**
- Target initial response: **[[REQUIRED: SLA BY SEVERITY; DO NOT PROMISE BEFORE STAFFING]]**

Privacy: [Privacy Policy](privacy-policy.md)

Terms: [Terms of Service](terms-of-service.md)

Deletion/revocation/export: [Data Deletion](data-deletion.md)

## Before contacting support

Record:

- rec_bag version/build commit and operating system;
- whether the issue affects SoundCloud, Google Drive, local storage, scheduler, ownership, recovery, or playback;
- safe error text, time/time zone, and steps to reproduce;
- whether the profile/storage is a copy and whether another manager may be running;
- the last successful sync/backup/snapshot time;
- non-secret storage/source identifiers only if needed.

Never send:

- passwords, client secrets, access/refresh tokens, OAuth authorization codes, PKCE verifiers, callback URLs with query data, or keyring exports;
- `.env` files, full `library.db`, recovery snapshots, provider response bodies, or audio files through ordinary email;
- unredacted local paths, folder IDs, account IDs, playlist/library metadata, or checksums unless support provides an approved secure upload and explains why they are needed.

## Self-service checks

Run offline checks from the repository when applicable:

```sh
cargo check -p desktop --features desktop
cargo test -p desktop
cargo test -p api --features server
cargo test -p soundcloud
cargo test -p music_storage_google_drive
cargo test -p music_storage_local
```

See [Troubleshooting, data, and privacy](troubleshooting-and-privacy.md) for startup, keyring, OAuth, scheduler, storage, and cache guidance.

## Provider access issues

### SoundCloud

Confirm the registered loopback redirect, desktop client ID, broker allow-list/client credentials, trusted-proxy `X-Real-IP`, and provider application status. Reauthorization may be required after failed single-use refresh-token rotation. Do not send tokens to support.

### Google Drive

Confirm Drive API is enabled, the installed-app OAuth client and loopback redirect are correct, consent grants only `drive.file`, and the account can create the dedicated `rec_bag Music` folder in My Drive. Shared Drives are unsupported.

To stop access or remove data, follow [Data Deletion](data-deletion.md); disconnect, revoke, cache clear, catalog forget, local deletion, and remote Drive deletion are separate actions.

## Ownership and recovery

Only one active manager is supported per profile. A second live manager is blocked. Confirm takeover only after proving the prior process/device will not continue writing. The takeover confirmation is bound to the observed owner and expires after five minutes.

Recovery should begin with snapshot health review and dry-run validation. A real restore exactly replaces all snapshot-owned catalog tables in one transaction; rows absent from the snapshot are deleted. Rollback exactly replaces the catalog from the pre-restore snapshot. Keep that snapshot until acceptance. Portable authority on another device requires an explicit confirmed handoff; copying files alone is insufficient. If startup reports interrupted recovery, preserve the profile and error before retrying.

## Security reports

Send suspected credential exposure, authorization bypass, data leakage, or destructive behavior to **[[REQUIRED: SECURITY CHANNEL]]**. Include a minimal reproduction and safe metadata. Do not test against other users or production beyond authorization.

[[REQUIRED: SECURITY REVIEW — coordinated disclosure policy, encryption key, acknowledgement/remediation targets, safe harbor, CVE process, and emergency contact.]]

## Privacy, deletion, and export requests

Use **[[REQUIRED: PRIVACY/DELETION FORM]]**. State whether you request access, correction, export, deletion, restriction, objection, consent withdrawal, or appeal. Support will use **[[REQUIRED: IDENTITY VERIFICATION METHOD]]** and respond within **[[REQUIRED: LEGAL/SERVICE PERIOD]]**.

The operator cannot remotely erase local profile directories or storage it does not control. The requester must complete local and provider-side steps described in [Data Deletion](data-deletion.md).

## Support data handling

Support records may contain contact information, diagnostics, correspondence, and redacted identifiers. They are stored by **[[REQUIRED: SUPPORT PROCESSOR/REGION]]**, accessible to **[[REQUIRED: ROLES]]**, and retained for **[[REQUIRED: PERIOD]]**. Secure uploads, if offered, use **[[REQUIRED: SYSTEM, ENCRYPTION, ACCESS, AND DELETION PERIOD]]**.

No support channel, service level, or secure-upload mechanism is represented as active until this template is completed, tested, approved, and published.
