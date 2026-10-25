# One music storage module with per-provider drivers

Each music storage provider used to bring its own desktop orchestration (`GoogleDriveService`, the storage half of `LocalLibraryService`), and the desktop picked between them by matching on the storage kind in many places. Adding iCloud Drive, Dropbox or S3 would have meant another ~4k-line service and another round of kind switches. We decided that all storage workflows — inventory, reconciliation, copying to the primary, primary migration, playback resolution, the offline cache and the storage lifecycle — live once, in a desktop-free `music_storage` crate. A provider contributes only an adapter crate implementing `MusicAssetStore` and a small `StorageDriver` that provisions, opens and describes its storages; the storage kind is used only to find the driver, never to decide behaviour or capabilities.

## Considered Options

- **A command enum (`apply(StorageCommand)`)** gave the smallest-looking interface, but it moved the interface into enum variants whose preconditions are checked only at runtime.
- **Optional per-store facets** (change feed, server-side copy, host paths, retention, …) were the most future-proof, but most facets had one adapter or none, so they would have been hypothetical seams that double the test paths.
- **A provisioning interaction protocol rendered by the UI** would allow in-app remote folder picking, but each driver keeps owning its own provisioning UI for now; revisit when a provider needs remote folder selection.

## Consequences

- The caller-facing interface treats the primary as the implicit target and bundles follow-up work: copying to the primary also verifies, reconciles and hands off to replication.
- A storage is persisted only after its adapter's health probe succeeds, with the probed storage capabilities. Adapters must therefore really probe; a constant capability set is a bug.
- Playback never falls back to a backup. When only a backup holds a file, the caller is told so and offers a restore.
- The offline cache is a derived asset owned by the module and filled through any store's `download`, so it is no longer private to Google Drive.
- Per-storage settings use a provider-neutral key prefix, so the catalog never needs to know a provider's private keys.
