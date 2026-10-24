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
| Owned playlists | `GET /me/playlists` | `Playlists`; `limit`, `linked_partitioning`, `show_tracks=false` |
| Liked tracks | `GET /me/likes/tracks` | `Tracks`; `limit`, `linked_partitioning`; access defaults include playable, preview, and blocked |
| Liked playlists | `GET /me/likes/playlists` | `Playlists`; `limit`, `linked_partitioning`, `show_tracks=false` |
| Playlist tracks | `GET /playlists/{playlist_urn}/tracks` | `Tracks`; `limit=200`, `linked_partitioning`, `access=playable,preview,blocked` |
| Revoke grant (desktop) | `POST /disconnect` | 204; replaces the unsupported `secure.soundcloud.com/sign-out` |
| Deck playback | `GET /tracks/{track_urn}/streams` | `Streams`: `hls_mp3_128_url`, `hls_aac_160_url`, `preview_mp3_128_url` (authenticated) |

Playlist pages are fetched without embedded tracks so a page of large playlists cannot exceed the response body limit; each playlist's tracks are then paged in order. Embedded tracks are used only when present and complete (`tracks.len() == track_count`).

Re-checked on **2026-10-24** against the API guide, the OpenAPI document at `developers.soundcloud.com/docs/api/explorer/api.json`, the rate-limits page, the API Terms of Use (effective 2024-03-30), and the `soundcloud/api` releases. Relevant changes since the first validation: `/disconnect` added (2026-09-16), `http_mp3_128_url` removed (2026-08-12), numeric `id` removed in favour of `urn` (2025-06-30), and access tokens are variable-length JWTs.

The Terms forbid downloading or storing User Content even when the uploader enabled downloads, so a playable track maps to `StreamingOnly` and never to `Obtainable`; `downloadable`/`download_url` are kept only as metadata.

Deck playback asks `/streams` only when the user loads a track that has no local file. It prefers `hls_mp3_128_url`, then `hls_aac_160_url`, then the preview. The HLS playlist and its segments are fetched into memory and assembled there. The desktop keeps the result for the rest of the session (Terms section 5 session caching) in an anonymous temporary file that the operating system deletes when the app exits, crashes, or is killed. It is never a named file a user could copy or play offline. Analysis runs on that same in-memory copy, and its results are kept for the session only. The access token is sent only to the API origin, never to the signed CDN URLs. Encrypted playlists are refused. While a streamed track is loaded, the deck credits SoundCloud with a link back to the track's permalink, and the uploader appears as the artist (Terms section 8). SoundCloud playlists are mirrored read-only in the DJ view with a SoundCloud badge that links to the playlist.

The client requests linked pagination and follows `next_href`. Continuations are accepted only when scheme, host, and effective port match the configured API origin. The OpenAPI `limit` range is 1–200; the connector enforces it before making a request.

The current specification uses URNs (`soundcloud:tracks:*`, `soundcloud:playlists:*`, and `soundcloud:users:*`). Typed DTOs prefer `urn` but also accept legacy numeric/string `id` fields so previously captured provider responses remain importable; a legacy `id` is converted to the equivalent `soundcloud:{kind}:{id}` URN so a resource never gets two identities. The specification permits legacy unwrapped arrays for track and playlist responses; the client accepts those while requesting the preferred paginated object.

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
