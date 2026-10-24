# API

This crate contains shared Dioxus 0.7 fullstack server functions. It is built for the server with the `server` feature and for clients without server-only dependencies.

# SoundCloud OAuth broker

The broker implements SoundCloud authorization-code/PKCE and refresh-token exchanges. It is stateless with respect to users: authorization codes and access/refresh tokens are held only for the lifetime of one request, are never persisted, and must never be included in logs, traces, panic context, metrics labels, or reverse-proxy access logs.

## Required configuration

Supply secrets with the deployment platform's secret manager, not an image, manifest, command line, or checked-in file.

```dotenv
SOUNDCLOUD_CLIENT_ID=your-soundcloud-client-id
SOUNDCLOUD_KEY=your-soundcloud-client-secret
SOUNDCLOUD_ALLOWED_REDIRECT_URIS=http://127.0.0.1:17841/oauth/soundcloud
```

`SOUNDCLOUD_ALLOWED_REDIRECT_URIS` is a required comma-separated **exact** allow-list. Wildcards, fragments, credentials in URLs, public plain-HTTP callbacks, and malformed entries cause startup-on-first-request configuration failure. Plain HTTP is accepted only for loopback development callbacks. Every production callback must also be registered with SoundCloud.

Optional bounded settings:

```dotenv
SOUNDCLOUD_OAUTH_RATE_LIMIT=20
SOUNDCLOUD_OAUTH_RATE_WINDOW_SECONDS=60
SOUNDCLOUD_OAUTH_MAX_TRACKED_CLIENTS=10000
SOUNDCLOUD_OAUTH_HTTP_TIMEOUT_SECONDS=15
SOUNDCLOUD_LOAD_DOTENV=false   # default: true in debug builds, false in release builds
```

`.env` loading is on by default in debug builds (local `dx serve`) and off in release builds; `SOUNDCLOUD_LOAD_DOTENV` overrides either default. Process environment values take precedence. Invalid, zero, or out-of-range numeric values fail closed rather than silently falling back.

Configuration is cached on first broker use. Any environment change therefore requires a process restart or rolling deployment.

## Public desktop configuration

`GET /api/soundcloud/oauth/config` returns the client ID and the first loopback `http` entry of `SOUNDCLOUD_ALLOWED_REDIRECT_URIS`. The desktop uses it when no local override is saved. Neither value is secret.

## OAuth responsibility split

Desktop must:

1. Generate a cryptographically random, URL-safe OAuth `state` with at least 128 bits of entropy.
2. Store the state and PKCE verifier locally for the pending authorization.
3. Compare callback state to stored state in constant time and consume the pending state before calling the broker.
4. Send the original PKCE verifier and exact callback URI to the exchange endpoint.
5. Store returned credentials only in the OS credential store. On refresh, atomically replace the old single-use refresh token with the returned refresh token before reporting success.

The broker validates state and PKCE syntax, exact redirect membership, and forwards the verifier. It cannot establish state/nonce authenticity because it intentionally has no browser/desktop session and stores no pending authorization state.

## Mandatory trusted-proxy boundary and rate limiting

Dioxus server functions expose request headers but do not provide a deployment-independent peer socket address. Both OAuth endpoints therefore require one canonical `X-Real-IP` header containing exactly one IPv4 or IPv6 address and apply an atomic per-IP sliding-window limit. Missing, malformed, or comma-separated values fail closed with a safe `503`. Debug builds only treat a *missing* header as the loopback client so local `dx serve` works without a proxy; release builds always require it. Limiter identity storage is bounded by `SOUNDCLOUD_OAUTH_MAX_TRACKED_CLIENTS`.

Deploy the app only behind a trusted reverse proxy/load balancer configured to:

- remove every inbound `X-Real-IP` supplied by the public client;
- set `X-Real-IP` from the proxy's authenticated connection peer/client-IP policy;
- send exactly one address (never an `X-Forwarded-For` chain);
- block direct public access to the application listener;
- apply an additional distributed per-IP OAuth limit at the edge, because in-process limits are per instance and reset on restart;
- redact request/response bodies and query strings for `/api/soundcloud/oauth/*` from access logs and tracing.

Example nginx policy (adapt the trusted-network and real-IP settings to the platform):

```nginx
location /api/soundcloud/oauth/ {
    proxy_set_header X-Real-IP $remote_addr;
    proxy_pass http://rec_bag;
}
```

Do not pass a client-provided `$http_x_real_ip`. If the platform cannot guarantee header replacement and prevent direct access, these endpoints are not safe to expose; use edge middleware that injects an authenticated client-IP extension/header first.

## Error and provider handling

Provider bodies are never passed through or logged. Upstream `400/401/403`, `429`, transport/5xx failures, and malformed success responses map to fixed client-safe messages. Responses are timeout-bound, redirect-following is disabled, and response bodies are capped at 64 KiB. DTOs containing tokens intentionally do not implement `Debug` or `Display`.

## Deployment checklist

1. Register every exact redirect URI with SoundCloud and set the same allow-list in deployment configuration.
2. Configure `SOUNDCLOUD_CLIENT_ID` and `SOUNDCLOUD_KEY` through a secret manager with least-privilege read access.
3. Configure and test the trusted-proxy rules above, including spoofed/missing `X-Real-IP` requests and blocked direct origin access.
4. Apply edge and application rate limits appropriate for instance count and expected login/refresh volume.
5. Disable body capture and verify sampled logs contain no authorization codes, PKCE verifiers, state values, tokens, or client secret.
6. Use TLS from client to edge and, where the network is not private/authenticated, edge to origin.
7. Restart all instances after configuration changes and verify exchange and refresh through the real proxy path.

## Client-secret rotation and incident response

Normal rotation:

1. Create/obtain the replacement SoundCloud credential without putting it in tickets or logs.
2. Update `SOUNDCLOUD_KEY` in the secret manager.
3. Roll all broker instances; cached configuration means live instances do not reload it.
4. Perform a synthetic authorization exchange and refresh through the trusted proxy.
5. Revoke the old credential after all instances use the replacement. Re-authorize users if SoundCloud invalidates existing grants.

Suspected disclosure:

1. Revoke/rotate the credential immediately and roll every instance.
2. Rotate any user grants/tokens SoundCloud identifies as affected; do not collect tokens in diagnostics.
3. Search deployment, proxy, APM, and CI logs for exposure using restricted access, then purge according to provider policy.
4. Correct the disclosure path and repeat spoofing/redaction tests before reopening traffic.
