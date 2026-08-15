# Large-library persistence performance harness

This harness generates deterministic catalogs at runtime; no large fixtures are checked in. It covers:

- legacy v1 migration/import into the current schema;
- cursor-paginated track repository reads;
- reconciliation-input reads (source occurrences, original assets, and storage inventory);
- snapshot serialization, SHA-256 fingerprinting, logical fingerprinting, and export;
- snapshot import into a fresh catalog;
- atomic durable job claims.

## Running

A bounded CI/developer smoke run:

```sh
REC_BAG_BENCH_SMOKE=1 cargo bench -p rec_persistence --bench large_library
```

The default full profile generates 10,000 tracks/assets/occurrences/storage objects and 1,000 jobs:

```sh
cargo bench -p rec_persistence --bench large_library
```

Configuration:

| Variable | Meaning | Smoke default | Full default |
| --- | --- | ---: | ---: |
| `REC_BAG_BENCH_SMOKE` | Use bounded smoke profile (`1`, `true`, `yes`, or `on`) | — | false |
| `REC_BAG_BENCH_TRACKS` | Generated track/catalog record count | 200 | 10,000 |
| `REC_BAG_BENCH_JOBS` | Jobs created and atomically claimed | 40 | 1,000 |
| `REC_BAG_BENCH_PAGE_SIZE` | Repository page size | 250 | 250 |
| `REC_BAG_BENCH_THRESHOLD_MULTIPLIER` | Multiply all wall-clock thresholds for slower runners | 1.0 | 1.0 |
| `REC_BAG_BENCH_NO_THRESHOLDS` | Report without failing on thresholds | false | false |

Each result is emitted as a stable, grep-friendly line:

```text
PERF metric=snapshot_import total_ms=123.456 units=200 rate_per_sec=1620.3 threshold_ms=30000 status=pass
```

`units` is the number of records/items processed (snapshot bytes are reported separately by the normal smoke test). Thresholds intentionally detect catastrophic regressions rather than compare machines precisely:

| Metric | Smoke threshold | Full threshold |
| --- | ---: | ---: |
| Legacy migration | 20 s | 240 s |
| Paginated repository reads | 5 s | 60 s |
| Reconciliation-input reads | 10 s | 120 s |
| Snapshot fingerprint/export | 20 s | 240 s |
| Snapshot import | 30 s | 360 s |
| Job claim throughput | 20 s | 240 s |

The normal test target uses 12 tracks and 4 jobs, asserts all workload paths execute and validates record counts. Run it with:

```sh
cargo test -p rec_persistence --test persistence_benchmark_smoke
```

Wall-clock results vary with hardware and filesystem load. Preserve the generated counts when comparing changes, use release-mode `cargo bench`, and archive the `PERF` lines in CI artifacts. Use the multiplier rather than weakening individual thresholds for consistently slower CI machines.
