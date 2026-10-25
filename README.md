# rec_bag

rec_bag is a Dioxus 0.7 desktop music-catalog and storage manager. It combines local folders and a SoundCloud catalog with one authoritative local or Google Drive primary and optional local/Drive backups.

## Current production status

Stage 4 runtime assembly is present:

- startup acquires a one-active-manager lease before opening or migrating the catalog;
- a 10-second ownership heartbeat and the background scheduler start with the desktop runtime;
- durable jobs, SoundCloud sync, backup planning, verification, inventories, and recovery snapshots run on configured cadences;
- recovery supports snapshot discovery, integrity/secret checks, dry-run validation, exact transactional replacement, exact pre-restore rollback, and interrupted-restore reconciliation;
- a live owner blocks a second manager unless the user explicitly confirms takeover, and portable catalog authority requires an explicit confirmed handoff.

SoundCloud and Google live credential checks have **not** been run or claimed by this documentation. Keep an independent copy of the catalog and music assets; concurrent multi-device writers remain unsupported.

## Build and test

Requirements: stable Rust, Dioxus CLI 0.7.10-compatible tooling, desktop WebView dependencies, audio development libraries, and an available OS credential service.

```sh
cargo check --workspace
cargo test --workspace
dx build --release --desktop -p desktop --features desktop --locked
```

Build the fullstack server that hosts the SoundCloud broker endpoints:

```sh
dx build --release --server -p web --features server --locked
```

The `api` crate is a library of Dioxus server functions, not a standalone binary. Deploy it through a fullstack host such as the `web` package. The repository does not contain a complete production reverse-proxy or service manifest.

## Configuration overview

Desktop:

```dotenv
REC_BAG_DATA_DIR=/absolute/writable/profile-directory
REC_BAG_GOOGLE_CLIENT_ID=desktop-app-client-id
REC_BAG_GOOGLE_CLIENT_SECRET=desktop-app-client-secret
REC_BAG_GOOGLE_REDIRECT_URI=http://127.0.0.1:17842/oauth/google
```

Create the Google OAuth client as type **Desktop app**; no redirect URI registration is needed. Its client secret is not confidential in an installed app, but Google's token endpoint requires it. Debug desktop builds load `.env` automatically (`REC_BAG_LOAD_DOTENV` overrides); process environment wins.

The desktop app gets the public SoundCloud client ID and loopback redirect from the broker (`/api/soundcloud/oauth/config`), so connecting needs no manual setup. **Music Management → Configure SoundCloud** optionally overrides them in the local profile; the broker client secret is never entered in the desktop app.

SoundCloud broker:

```dotenv
SOUNDCLOUD_CLIENT_ID=server-side-client-id
SOUNDCLOUD_KEY=server-side-client-secret
SOUNDCLOUD_ALLOWED_REDIRECT_URIS=http://127.0.0.1:17841/oauth/soundcloud
```

Use a secret manager for server credentials. Provider user tokens belong in the desktop OS keyring, never in `.env`, SQLite, logs, support bundles, or snapshots. Google setup creates a dedicated My Drive folder named `rec_bag Music` under the `drive.file` grant.

## Documentation

- [Production setup](docs/production-setup.md) — desktop/server builds, broker environment, redirects, proxy/WAF, Google OAuth, keyring, and release checks.
- [Storage and operations](docs/storage-and-operations.md) — local/Drive primary and backups, migration, schedules, snapshots, recovery, and ownership takeover.
- [Troubleshooting, data, and privacy](docs/troubleshooting-and-privacy.md) — failure diagnosis, data locations, network flows, retention, and privacy implications.
- [Provider compliance](docs/provider-compliance.md) — SoundCloud and Google policy checklists, offline validation, publication gates, and clearly marked live tests.
- Public-page templates: [Privacy Policy](docs/privacy-policy.md), [Terms of Service](docs/terms-of-service.md), [Data Deletion](docs/data-deletion.md), and [Support](docs/support.md). These contain required placeholders and are not claimed as deployed or legally approved.

## Safety invariants

- SoundCloud is a catalog source; rec_bag does not archive SoundCloud playback transcodes.
- Exactly zero or one storage is active primary. Backups never become authoritative automatically, and playback never reads a backup: a file only a backup holds is restored to the primary.
- The active primary cannot be disconnected or forgotten; migrate to another primary first.
- A primary change must inventory, copy, verify, snapshot, confirm, and atomically switch roles. The old primary remains a backup by default.
- External primary changes wait for review before replication. Primary deletion does not delete backup copies.
- Google Drive access uses `drive.file`, My Drive only, and a dedicated app-created `rec_bag Music` folder subtree.
- SHA-256 is the canonical content checksum. Filename/title similarity alone never verifies a match.
- Disconnecting or revoking SoundCloud also deletes its provider-derived catalog data, as the SoundCloud API Terms require; canonical owned assets are kept.
- Forgetting provider data deletes current provider-derived source/storage rows and current snapshot records while preserving canonical owned assets and never deleting remote provider files.
- Cleanup requires explicit approval; defaults retain 30 days and at least three recent versions.
