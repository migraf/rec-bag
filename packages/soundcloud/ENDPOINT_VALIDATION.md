# SoundCloud endpoint and schema validation

Validated on **2026-08-14** against SoundCloud's official sources:

- [API Reference](https://developers.soundcloud.com/docs/api/reference), which identifies the OpenAPI document as the canonical reference.
- [`soundcloud/api/openapi/api.yaml`](https://github.com/soundcloud/api/blob/15cf0147cba3d6afbda7b0fb63b04ccc3c6f536b/openapi/api.yaml) at commit `15cf0147cba3d6afbda7b0fb63b04ccc3c6f536b` (published 2026-08-12).
- [API Guide](https://developers.soundcloud.com/docs/api/guide) for OAuth header, single-use refresh-token, linked-pagination, and access-level behavior.

## Read endpoints used

| Purpose | Endpoint | Validated response/parameters |
|---|---|---|
| Account identity | `GET /me` | `Me`; OAuth authorization-code token required |
| Uploaded tracks | `GET /me/tracks` | `Tracks`; `limit`, `linked_partitioning` |
| Owned playlists | `GET /me/playlists` | `Playlists`; `limit`, `linked_partitioning`, `show_tracks` |
| Liked tracks | `GET /me/likes/tracks` | `Tracks`; `limit`, `linked_partitioning`; access defaults include playable, preview, and blocked |
| Liked playlists | `GET /me/likes/playlists` | `Playlists`; `limit`, `linked_partitioning` |
| Playlist detail fallback | `GET /playlists/{playlist_urn}` | `Playlist`; `show_tracks=true` |

The client requests linked pagination and follows `next_href`. Continuations are accepted only when scheme, host, and effective port match the configured API origin. The OpenAPI `limit` range is 1–200; the connector enforces it before making a request.

The current specification uses URNs (`soundcloud:tracks:*`, `soundcloud:playlists:*`, and `soundcloud:users:*`). Typed DTOs prefer `urn` but also accept legacy numeric/string `id` fields so previously captured provider responses remain importable. The specification permits legacy unwrapped arrays for track and playlist responses; the client accepts those while requesting the preferred paginated object.

## Fixture provenance and drift strategy

Fixtures under `tests/fixtures/*_current.json` are minimal, synthetic examples shaped from the OpenAPI `Me`, `Track`, `Tracks`, `Playlist`, and `Playlists` schemas. They contain no live account data or credentials. They intentionally cover:

- current URN identities and paginated collection envelopes;
- uploaded and liked tracks;
- owned and liked playlists with ordered and repeated entries;
- public/private metadata;
- playable, preview, blocked, inaccessible, and removed observations;
- unknown future fields and recursive secret redaction.

`state: inaccessible|failed|removed|deleted` is treated as a defensive provider extension for degraded or tombstone payloads. These values are not currently enumerated by the OpenAPI `Track` schema; retaining unknown fields and fixture-testing this mapping prevents such payloads from becoming raw untyped traversal throughout the client.

Response DTOs type fields used for identity and normalization and use a flattened map only for forward-compatible, non-secret provider metadata. Mapping serializes the DTO back to metadata, preserving known and unknown fields while recursively dropping keys containing `token` or `secret` and the `authorization` key.

When SoundCloud changes the OpenAPI document:

1. Record the new upstream commit here.
2. Compare the six endpoints and referenced schemas above.
3. Update typed DTO fields used by normalization.
4. Add or update a synthetic fixture for every changed response shape.
5. Run `cargo test -p soundcloud` before accepting the schema change.

## Sync and removal semantics

SoundCloud's listed collection endpoints are snapshots, not a change feed. Therefore `SourceSyncMode::Incremental` performs an authoritative, checkpointed snapshot diff:

- the durable sync cursor stores selected collections and the prior relationship identities;
- each page cursor stores provider continuation, prior/current relationship sets, run observation time, and emitted entity identities;
- a resumed page requires the same mode/base cursor and collection selection;
- after the entire sequence, prior relationships absent from the new snapshot are emitted as `RelationshipRemoval` records;
- explicit removed track payloads also produce `SourceRemoval` and `SourceAvailability::Removed`;
- the new durable cursor is returned only on the final page.

This makes partial resume and retries stateless and idempotent by core identity. It deliberately does **not** infer entity deletion merely because one collection membership disappeared; a track can be unliked or removed from a playlist while remaining valid elsewhere. Cursor size grows with relationship count, which is the tradeoff required for authoritative removals without connector-owned persistence or a provider change feed.

## Credential-gated validation

All normal tests are offline. To make a single live `GET /me` request with a fresh authorization-code access token:

```sh
SOUNDCLOUD_ACCESS_TOKEN='...' cargo test -p soundcloud --test connector live_account_smoke -- --ignored --exact
```

The ignored smoke test does not persist or print the token and has no refresh credential. It validates only account authentication/schema. Exact private, liked, inaccessible, and removed-resource behavior still requires an authorized account containing those cases and cannot be established from public fixtures alone.
