# Provider compliance checklists

These are engineering release gates, not legal advice. Provider terms, verification requirements, branding rules, and product behavior can change. Re-check the current official policies at every release.

**Validation labels used below:**

- **Offline** — no provider credentials or provider network call required.
- **Deployed** — exercises your edge/origin but should use synthetic invalid inputs, not real provider credentials.
- **LIVE CREDENTIAL TEST** — contacts a provider using a real account/application. Run only in an approved test environment. No live credential test is claimed as completed by this documentation.

## SoundCloud checklist

### Registration and product behavior

- [ ] Confirm the application is eligible for SoundCloud API registration and the account/app tier meets current prerequisites.
- [ ] Record approved use case, application owner, support contact, privacy policy, terms URL, and data-deletion procedure.
- [ ] Register every exact desktop redirect URI; remove obsolete URIs.
- [ ] Keep the confidential client secret exclusively in the broker secret manager.
- [ ] Use authorization-code flow with PKCE and one-time state.
- [ ] Request/use only data needed for uploads, owned playlists, likes, and playlist membership.
- [ ] Treat imported SoundCloud playlists as read-only.
- [ ] Do not archive playback transcodes. Only associate/copy audio the user owns and is permitted to store.
- [ ] Streamed SoundCloud tracks show "Streaming from SoundCloud", linking to the track's permalink, and mirrored playlists show a SoundCloud badge linking to the playlist.
- [ ] Streamed audio is cached for the session only: in memory while a deck holds it, and otherwise in anonymous temporary files (no filesystem name, deleted by the OS when the app exits, crashes, or is killed) under `cache/streams` next to the catalog, which stays empty on Linux. The cache is capped at 2 GiB / 256 tracks and is dropped when SoundCloud is disconnected, revoked, forgotten, or its local cache is cleared. Analysis of a stream (BPM, key, waveform, beat grid) runs on that in-memory copy and is shown for the session only; it is never saved to the catalog, and a stream is never fetched just to be analyzed.
- [ ] Review SoundCloud API Terms section 3 (no modification; no playback experience that combines User Content with content from other services) against DJ mixing before any public release.
- [ ] Handle revocation/removal without deleting user-owned canonical data or backups.
- [ ] Document export/deletion and revoke tokens on disconnect where supported.

### Broker security

- [ ] Exact redirect allow-list is identical to provider registration and desktop setting.
- [ ] TLS is enforced; origin is private or authenticated.
- [ ] Edge strips and replaces `X-Real-IP`; direct origin access is blocked.
- [ ] Edge distributed rate limiting supplements per-instance defaults.
- [ ] OAuth route body/query capture is disabled in proxy, WAF, APM, tracing, and error reporting.
- [ ] Client secret rotation and user-grant incident procedures are rehearsed.
- [ ] Broker process restart/roll is part of every environment change because config is cached.

### Offline validation

```sh
cargo test -p api --features server
cargo test -p soundcloud
cargo test -p desktop soundcloud_catalog
cargo check -p web --features server
```

Review endpoint/schema provenance:

```sh
sed -n '1,220p' packages/soundcloud/ENDPOINT_VALIDATION.md
```

Secret/reference scan (review every match; this is not a full secret scanner):

```sh
git grep -n -E 'SOUNDCLOUD_KEY|SOUNDCLOUD_ACCESS_TOKEN|client_secret|refresh_token' -- ':!Cargo.lock' ':!docs/provider-compliance.md' ':!packages/soundcloud/ENDPOINT_VALIDATION.md'
```

Expected source references and test fixtures are allowed; actual values are not.

### Deployed edge validation — no live provider credential required

Replace the URL and use documentation-only fake values. Confirm safe `4xx/503` responses and no body/query logging:

```sh
curl -i -X POST https://broker.example/api/soundcloud/oauth/exchange \
  -H 'Content-Type: application/json' \
  -H 'X-Real-IP: 198.51.100.7' \
  --data '{"request":{"code":"invalid","code_verifier":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","redirect_uri":"http://127.0.0.1:17841/oauth/soundcloud","state":"invalid-state-value"}}'
```

Spoof/malformed identity tests:

```sh
curl -i -X POST https://broker.example/api/soundcloud/oauth/refresh \
  -H 'Content-Type: application/json' \
  -H 'X-Real-IP: 198.51.100.7, 203.0.113.4' \
  --data '{"request":{"refresh_token":"invalid"}}'
```

Exact Dioxus server-function wire envelopes can vary by generated client/version. If the first command is rejected before application validation, capture only safe status/headers and use the generated desktop client or integration harness; never enable body logging to diagnose it.

### **LIVE CREDENTIAL TEST — SoundCloud account smoke**

This ignored test makes one live `GET /me` request. Use a fresh short-lived access token from an approved test account:

```sh
SOUNDCLOUD_ACCESS_TOKEN='REDACTED' cargo test -p soundcloud --test connector live_account_smoke -- --ignored --exact
```

It validates only authentication and `/me` schema. It does not validate refresh rotation, private/liked data, removals, inaccessible resources, application approval, redirect registration, broker routing, WAF behavior, or policy compliance. Do not put the token in shell history on shared systems; prefer an ephemeral CI secret or interactive secret injection supported by your environment.

### **LIVE CREDENTIAL TEST — end-to-end broker authorization**

Manual, because it requires browser consent and real redirects:

- [ ] Connect an approved SoundCloud test account through the packaged desktop and deployed broker.
- [ ] Verify exact loopback callback, state/PKCE success, and keyring entry creation.
- [ ] Sync `/me`, uploads, owned playlists, liked tracks, and liked playlists appropriate to the account.
- [ ] Force/await token refresh and verify the old single-use refresh token is replaced atomically.
- [ ] Disconnect/revoke and confirm SoundCloud reports the grant revoked, the keyring entry is gone, and SoundCloud-derived catalog rows are deleted while canonical owned assets remain.
- [ ] Inspect restricted edge/origin/APM logs for absence of codes, verifiers, state, tokens, and provider bodies.

Record date, app ID (not secret), test account classification, environment, reviewer, and redacted evidence. This repository documentation records no such execution.

## Google API checklist

### Cloud project and consent

- [ ] Drive API is enabled in the intended production project.
- [ ] OAuth client type is Desktop/Installed application.
- [ ] Consent-screen app name, support email, homepage, privacy policy, terms, and authorized domains are current.
- [ ] Production/test user status and verification requirements are understood.
- [ ] Requested scope is exactly `https://www.googleapis.com/auth/drive.file`.
- [ ] Scope purpose and in-product disclosure explain app-created-folder music storage, local cache, and backups.
- [ ] No Google client secret is embedded or relied upon by the desktop app.
- [ ] Exact loopback redirect is configured and documented.
- [ ] User can disconnect and revoke access; local/remote deletion consequences are separately explained.
- [ ] Data use, retention, sharing, breach response, and deletion practices satisfy current Google API Services User Data Policy, including Limited Use requirements where applicable.

### Folder and Drive behavior

- [ ] rec_bag creates the dedicated `rec_bag Music` folder through Drive `files.create` after consent and validates its stable ID/type.
- [ ] Inventory, changes, uploads, and downloads are constrained to the app-created folder and descendants.
- [ ] Shared Drives remain disabled (`supportsAllDrives=false`, `includeItemsFromAllDrives=false`).
- [ ] New files are private by default and use private `appProperties` only for stable rec_bag identifiers.
- [ ] The app does not silently escalate from `drive.file` to broader Drive scopes.
- [ ] Public instructions do not request arbitrary Drive folder IDs or links; any future Picker flow uses identities granted within the same OAuth session.
- [ ] Audio upload/download and local cache behavior are disclosed before use.
- [ ] Quota, retry/backoff, resumable upload, checksum, and user-visible error behavior are tested.

### Offline validation

```sh
cargo test -p music_storage_google_drive
cargo test -p desktop google_drive
cargo check -p desktop --features desktop
```

Scope and Shared Drive guard scan:

```sh
git grep -n 'www.googleapis.com/auth/drive' -- packages/desktop packages/music_storage_google_drive
git grep -n -E 'supportsAllDrives|includeItemsFromAllDrives|corpora=user|spaces=drive' -- packages/music_storage_google_drive
```

Credential-reference scan (review every match; environment injection is test/headless behavior, not desktop production guidance):

```sh
git grep -n -E 'REC_BAG_GOOGLE_(ACCESS|REFRESH)_TOKEN|client_secret|refresh_token' -- ':!Cargo.lock' ':!docs/provider-compliance.md'
```

### **LIVE CREDENTIAL TEST — Google OAuth and app-created folder**

No ignored automated Google live test exists in this repository. Perform this manually with an approved test project/account and non-sensitive test audio:

- [ ] Start packaged desktop with `REC_BAG_GOOGLE_CLIENT_ID` and exact registered loopback redirect.
- [ ] Confirm consent requests only `drive.file`, offline access, and expected app identity.
- [ ] Verify rec_bag creates `rec_bag Music` in My Drive and validates the returned folder metadata.
- [ ] Upload, inventory, download, checksum, cache, restart, and refresh-token behavior.
- [ ] Confirm operations do not enumerate unrelated Drive content and Shared Drives remain absent.
- [ ] Confirm created files are private, under the app-created folder, and carry only expected private `appProperties`.
- [ ] Disconnect/revoke and verify keyring removal/reauthorization behavior without remote deletion.
- [ ] Inspect restricted logs and crash reports for absence of access tokens, refresh tokens, authorization codes, and audio payloads.

Record date, Cloud project/client ID (not secret), scope shown, test account class, folder ownership, reviewer, and redacted evidence. This documentation makes no claim that these checks have run.

## Public-page publication and console verification gate

The repository contains public-ready templates, not deployed or legally approved pages:

- [Privacy Policy](privacy-policy.md)
- [Terms of Service](terms-of-service.md)
- [Data Deletion](data-deletion.md)
- [Support](support.md)

Before production provider review or release:

- [ ] Replace every `[[REQUIRED: ...]]` placeholder and remove template banners only after legal/privacy review.
- [ ] Publish all four pages on an organization-controlled HTTPS domain.
- [ ] Verify each final URL returns `200`, has a valid certificate, is publicly reachable without authentication, and is readable on mobile/desktop.
- [ ] Put the exact Privacy Policy, Terms, support/homepage, and deletion URLs into the Google OAuth consent screen/Cloud Console where requested.
- [ ] Put the exact public policy/support/deletion URLs into the SoundCloud application registration/review fields where requested.
- [ ] Verify authorized domains and domain ownership in Google Search Console/Cloud Console as applicable.
- [ ] Verify app name, logo, contacts, scopes, redirects, and published-page identity are consistent in both provider consoles.
- [ ] Submit required Google verification/provider review and record status; do not represent submission as approval.
- [ ] Test the published deletion and support instructions end to end, including response ownership and retention timers.
- [ ] Verify recovery dry-run proves replacement semantics by removing a destination-only sentinel; verify restore exactly matches the selected snapshot and rollback exactly matches the pre-restore snapshot.
- [ ] Verify failed restore leaves the pre-restore catalog unchanged and interrupted restore exactly replaces from the verified pre-restore snapshot.
- [ ] Verify SoundCloud forget deletes current provider-derived source/account/relationship/playlist/sync/auth/provenance rows and current snapshot rows while preserving canonical owned tracks/assets, credentials, and all remote SoundCloud content.
- [ ] Verify Google forget disconnects/purges cache and deletes current storage/inventory/location/reconciliation/job/checkpoint and snapshot rows while preserving canonical owned tracks/assets, credentials, and all remote Drive files.
- [ ] Verify a copied/restored catalog cannot become portable authority on another device without an explicit confirmed handoff tied to the previous manager and current receiving lease.
- [ ] Verify a new post-forget snapshot contains none of the forgotten provider-derived rows.
- [ ] Archive screenshots, timestamps, final URLs, page hashes/version, reviewer approvals, and console configuration evidence.

Example publication checks after replacing placeholders:

```sh
curl -fsS https://[[REQUIRED-PUBLIC-DOMAIN]]/privacy-policy
curl -fsS https://[[REQUIRED-PUBLIC-DOMAIN]]/terms-of-service
curl -fsS https://[[REQUIRED-PUBLIC-DOMAIN]]/data-deletion
curl -fsS https://[[REQUIRED-PUBLIC-DOMAIN]]/support
```

These commands are templates and were not run against a deployed rec_bag domain.

## Release sign-off record

For each provider, retain outside the repository:

- policy/document versions and review date;
- application/project identifier and owner;
- registered redirect list;
- scope list and justification;
- offline command results and build commit;
- exact replacement, exact rollback, provider-forget, snapshot-clearing, and portable-authority handoff test evidence;
- deployed proxy/WAF test evidence;
- separately identified live-test evidence;
- known exceptions, risk owner, and expiry date;
- final engineering, security/privacy, and product/legal approvals as required by your organization.
