# Production setup

This guide describes the configuration supported by the current source. It does not imply that a live SoundCloud or Google application has been approved or tested.

## 1. Release topology

Use two separately trusted components:

1. **Desktop app** — owns the local catalog, local/Drive storage access, browser OAuth initiation, and OS-keyring user credentials.
2. **Fullstack server** — exposes the stateless SoundCloud token broker at:
   - `POST /api/soundcloud/oauth/exchange`
   - `POST /api/soundcloud/oauth/refresh`

The SoundCloud client secret must exist only on the server. The broker does not persist user tokens or catalog data. Google OAuth is performed directly by the installed desktop application and uses an installed-app client with PKCE.

The `api` package is not a standalone executable. Build it through a Dioxus fullstack package:

```sh
dx build --release --server -p web --features server --locked
```

Build the native app with its desktop-only keyring, HTTP, and browser dependencies:

```sh
dx build --release --desktop -p desktop --features desktop --locked
```

Before release, confirm how the generated desktop client resolves Dioxus server functions to the deployed server. No production API-origin configuration or deployment manifest is checked into this repository; a successful compile alone does not prove desktop-to-broker routing.

## 2. Desktop profile and environment

Set `REC_BAG_DATA_DIR` to an absolute, private, writable directory. If omitted, rec_bag uses:

- Linux: `$XDG_DATA_HOME/rec_bag`, otherwise `$HOME/.local/share/rec_bag`
- macOS: `$HOME/Library/Application Support/rec_bag`
- Windows: `%APPDATA%/rec_bag`

The directory contains:

```text
library.db
cache/offline/<storage-id>/
staging/storage/
staging/backup/
```

Recommended desktop configuration:

```dotenv
REC_BAG_DATA_DIR=/absolute/private/path
REC_BAG_GOOGLE_CLIENT_ID=google-installed-app-client-id
REC_BAG_GOOGLE_REDIRECT_URI=http://127.0.0.1:17842/oauth/google
```

The desktop fetches the public SoundCloud client ID and loopback redirect from the broker on first connect. **Music Management → Configure SoundCloud** optionally overrides them in the local profile; no restart is needed. The SoundCloud broker client secret is server-only and is never collected by the desktop UI.

Optional local-folder override:

```dotenv
REC_BAG_LOCAL_SOURCE=/absolute/music/folder
```

`REC_BAG_LOCAL_SOURCE` bypasses the native local-folder chooser. On Linux, a native local-folder chooser requires `zenity` or `kdialog`.

Do not use the headless `REC_BAG_GOOGLE_ACCESS_TOKEN` / `REC_BAG_GOOGLE_REFRESH_TOKEN` injection path for production desktop distribution. It is intended for non-desktop test/server injection and places credentials in process environment.

## 3. SoundCloud application and broker

### Desktop redirect

The built-in callback listener requires an exact HTTP loopback URL with a fixed port. Register the same value in SoundCloud, desktop configuration, and broker allow-list. The default is:

```text
http://127.0.0.1:17841/oauth/soundcloud
```

The app binds `127.0.0.1`, waits up to 180 seconds, checks the callback path, and validates/consumes OAuth state before code exchange. Ensure endpoint security software permits the local listener. Do not expose this port externally.

### Broker environment

Required:

```dotenv
SOUNDCLOUD_CLIENT_ID=client-id
SOUNDCLOUD_KEY=client-secret
SOUNDCLOUD_ALLOWED_REDIRECT_URIS=http://127.0.0.1:17841/oauth/soundcloud
```

Optional bounded settings and defaults:

```dotenv
SOUNDCLOUD_OAUTH_RATE_LIMIT=20
SOUNDCLOUD_OAUTH_RATE_WINDOW_SECONDS=60
SOUNDCLOUD_OAUTH_MAX_TRACKED_CLIENTS=10000
SOUNDCLOUD_OAUTH_HTTP_TIMEOUT_SECONDS=15
SOUNDCLOUD_LOAD_DOTENV=false   # default: true in debug builds, false in release builds
```

`SOUNDCLOUD_ALLOWED_REDIRECT_URIS` is a comma-separated exact allow-list. Wildcards, fragments, URL credentials, malformed URLs, and public plain HTTP fail closed. Loopback HTTP is allowed for installed-app callbacks. Broker configuration is cached on first use; restart or roll all instances after any change.

Use a secret manager and restrict secret read access to the broker workload. Never place `SOUNDCLOUD_KEY` in the desktop binary, image layer, repository, command line, crash report, or client-visible configuration.

### Reverse proxy and WAF

The broker requires exactly one canonical `X-Real-IP` value. Deploy only behind a trusted proxy/load balancer that:

- removes any inbound client-supplied `X-Real-IP`;
- sets one IPv4 or IPv6 address from an authenticated connection/client-IP policy;
- blocks direct public access to the origin;
- terminates TLS and uses authenticated/private or TLS origin transport;
- applies a distributed per-IP limit to `/api/soundcloud/oauth/*` in addition to the per-instance application limit;
- caps request size and permits only `POST` on broker routes;
- disables request/response body capture and query-string logging on broker routes;
- does not log authorization codes, PKCE verifiers, state, refresh tokens, or provider responses.

Minimal nginx location (adapt trusted-hop handling to the deployment):

```nginx
location /api/soundcloud/oauth/ {
    limit_except POST { deny all; }
    client_max_body_size 32k;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_pass http://rec_bag;
}
```

Never use `$http_x_real_ip`. Test spoofed, missing, malformed, and comma-separated values through both the edge and a private origin path. The application intentionally returns a safe `503` when trusted identity is absent.

## 4. Google OAuth and app-created storage folder

Create a Google Cloud project, enable Google Drive API, configure the OAuth consent screen, and create an OAuth client of type **Desktop app**. Desktop-type clients have no redirect URI registration: Google permits loopback IP redirects on any port. rec_bag listens on this default, overridable with `REC_BAG_GOOGLE_REDIRECT_URI`:

```text
http://127.0.0.1:17842/oauth/google
```

Configure the desktop with both values from the client:

```dotenv
REC_BAG_GOOGLE_CLIENT_ID=your-client-id.apps.googleusercontent.com
REC_BAG_GOOGLE_CLIENT_SECRET=your-desktop-client-secret
```

Google issues a client secret for Desktop-type clients. It is not confidential in an installed app (it ships with every copy), but Google's token endpoint still requires it for code exchange and refresh, even with PKCE. Debug builds load these from `.env` (searched upward from the working directory); set `REC_BAG_LOAD_DOTENV=true` or `false` to override. Process environment variables take precedence over `.env`.

The desktop requests only:

```text
https://www.googleapis.com/auth/drive.file
```

It also requests offline access and consent so a refresh token can be issued. The flow uses PKCE S256 and a one-time state value, and sends the Desktop client secret to the token endpoint.

After authorization, rec_bag creates a dedicated My Drive folder named `rec_bag Music` with Drive `files.create`. This app-created folder is directly accessible under `drive.file`. rec_bag validates the returned object as a non-trashed My Drive folder and persists its stable ID and display name.

Inventory, changes, uploads, and downloads are rooted at that folder and descendants. Shared Drives are unsupported. Files created by rec_bag carry private `appProperties` identifiers. Do not silently broaden the OAuth scope.

## 5. OS keyring behavior

Desktop builds use the platform credential store under service `rec_bag` with provider/account-specific entries. SoundCloud stores one serialized access/refresh-token generation atomically; Google stores its access token, optional refresh token, expiry, and account reference. SQLite and snapshots contain only opaque references and non-secret refresh metadata.

Operational requirements:

- Linux needs a functioning Secret Service/keyring session (for example GNOME Keyring or KWallet integration), including in packaged launches.
- macOS users may receive Keychain prompts after signing identity or bundle changes.
- Windows credentials are tied to the user profile.
- copying `library.db` does **not** copy credentials; reconnect providers on a new OS account/device;
- logout/disconnect should remove the relevant keyring entry; a missing entry produces authorization-required, not token fallback;
- never replace the OS keyring with plaintext files for support or migration.

## 6. Assembled desktop runtime

Before opening or migrating `library.db`, the desktop acquires a 30-second one-active-manager lease. It renews ownership every 10 seconds and stops mutations if ownership is lost. A live lease blocks a second manager unless the user explicitly confirms a takeover bound to the observed owner; concurrent writers remain unsupported.

After startup, the runtime starts the scheduler for durable jobs, SoundCloud sync, backup planning, daily verification/recovery snapshots, and monthly inventories. Shutdown cancels scheduling, checkpoints due snapshot work, waits for active operations, stops the heartbeat, and releases only its own lease.

Recovery snapshots live under `recovery/catalog-snapshots/`. Restore performs secret/integrity checks, an in-memory dry run that proves absent sentinel rows are removed, a durable pre-restore snapshot, and exact transactional replacement of all snapshot-owned tables. Rollback and interrupted-restore repair use the same exact replacement path. A catalog moved to another device is not authoritative merely because its files were copied; portable authority requires an explicit confirmed handoff to the receiving manager.

## 7. Deployment validation

Offline/static checks:

```sh
cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo check -p desktop --features desktop
cargo check -p web --features server
cargo test -p api --features server
```

Build artifacts:

```sh
dx build --release --desktop -p desktop --features desktop --locked
dx build --release --server -p web --features server --locked
```

These commands validate source/build behavior only. They do not validate provider registration, consent-screen approval, redirect ownership, WAF behavior, DNS/TLS, deployed server-function routing, public policy-page publication, or live credentials. See [Provider compliance](provider-compliance.md) for publication/console gates and separately labeled live tests.
