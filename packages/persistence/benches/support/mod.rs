#![allow(dead_code)]

use chrono::{DateTime, Duration, Utc};
use rec_core::*;
use rec_persistence::TursoCatalog;
use rec_sync::JobStore;
use std::error::Error;
use std::time::{Duration as StdDuration, Instant};
use tempfile::TempDir;
use turso::Builder;
use uuid::Uuid;

pub type BenchResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const NAMESPACE: Uuid = Uuid::from_u128(0x6e2bf443_7aa9_4ff1_a4d2_8dc0a5834a10);
const FIXED_TIME: &str = "2026-01-01T00:00:00Z";

#[derive(Clone, Copy, Debug)]
pub struct BenchConfig {
    pub tracks: usize,
    pub jobs: usize,
    pub page_size: u32,
    pub smoke: bool,
    pub enforce_thresholds: bool,
}

impl BenchConfig {
    pub fn from_env() -> BenchResult<Self> {
        let smoke = env_bool("REC_BAG_BENCH_SMOKE", false);
        Ok(Self {
            tracks: env_usize("REC_BAG_BENCH_TRACKS", if smoke { 200 } else { 10_000 })?,
            jobs: env_usize("REC_BAG_BENCH_JOBS", if smoke { 40 } else { 1_000 })?,
            page_size: env_usize("REC_BAG_BENCH_PAGE_SIZE", 250)? as u32,
            smoke,
            enforce_thresholds: !env_bool("REC_BAG_BENCH_NO_THRESHOLDS", false),
        })
    }

    pub const fn test() -> Self {
        Self {
            tracks: 12,
            jobs: 4,
            page_size: 5,
            smoke: true,
            enforce_thresholds: false,
        }
    }
}

#[derive(Debug)]
pub struct Measurement {
    pub name: &'static str,
    pub elapsed: StdDuration,
    pub units: usize,
    pub threshold: StdDuration,
}

impl Measurement {
    fn print_and_check(&self, enforce: bool) -> BenchResult<()> {
        let seconds = self.elapsed.as_secs_f64();
        let rate = if seconds > 0.0 {
            self.units as f64 / seconds
        } else {
            f64::INFINITY
        };
        let passed = self.elapsed <= self.threshold;
        println!(
            "PERF metric={} total_ms={:.3} units={} rate_per_sec={:.1} threshold_ms={} status={}",
            self.name,
            seconds * 1_000.0,
            self.units,
            rate,
            self.threshold.as_millis(),
            if passed { "pass" } else { "FAIL" }
        );
        if enforce && !passed {
            return Err(format!(
                "{} took {:?}, exceeding {:?}",
                self.name, self.elapsed, self.threshold
            )
            .into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct SuiteResult {
    pub measurements: Vec<Measurement>,
    pub paginated_tracks: usize,
    pub reconciliation_inputs: usize,
    pub claimed_jobs: usize,
}

pub async fn run_suite(config: BenchConfig) -> BenchResult<SuiteResult> {
    if config.tracks == 0 || config.jobs == 0 || config.page_size == 0 {
        return Err("tracks, jobs, and page size must all be greater than zero".into());
    }
    println!(
        "PERF profile={} tracks={} jobs={} page_size={} thresholds={}",
        if config.smoke { "smoke" } else { "full" },
        config.tracks,
        config.jobs,
        config.page_size,
        if config.enforce_thresholds {
            "on"
        } else {
            "off"
        }
    );

    let legacy = create_legacy_catalog(config.tracks).await?;
    let legacy_path = legacy.path().join("legacy.db");
    let started = Instant::now();
    let migrated = TursoCatalog::open(&legacy_path).await?;
    let migration = measure("legacy_migration", started, config.tracks, config, 20, 240);
    drop(migrated);

    let source = TursoCatalog::in_memory().await?;
    populate_catalog(&source, config.tracks).await?;

    let started = Instant::now();
    let paginated_tracks = read_all_tracks(&source, config.page_size).await?;
    let paginated = measure(
        "paginated_repository_reads",
        started,
        paginated_tracks,
        config,
        5,
        60,
    );

    let started = Instant::now();
    let reconciliation_inputs = read_reconciliation_inputs(&source, config.page_size).await?;
    let reconciliation = measure(
        "reconciliation_input_reads",
        started,
        reconciliation_inputs,
        config,
        10,
        120,
    );

    let job_catalog = TursoCatalog::in_memory().await?;
    seed_jobs(&job_catalog, config.jobs).await?;
    let started = Instant::now();
    let claimed_jobs = claim_jobs(&job_catalog, config.jobs).await?;
    let claims = measure(
        "job_claim_throughput",
        started,
        claimed_jobs,
        config,
        20,
        240,
    );

    let measurements = vec![migration, paginated, reconciliation, claims];
    for value in &measurements {
        value.print_and_check(config.enforce_thresholds)?;
    }

    Ok(SuiteResult {
        measurements,
        paginated_tracks,
        reconciliation_inputs,
        claimed_jobs,
    })
}

fn measure(
    name: &'static str,
    started: Instant,
    units: usize,
    config: BenchConfig,
    smoke_seconds: u64,
    full_seconds: u64,
) -> Measurement {
    let base = if config.smoke {
        smoke_seconds
    } else {
        full_seconds
    };
    let multiplier = std::env::var("REC_BAG_BENCH_THRESHOLD_MULTIPLIER")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(1.0);
    Measurement {
        name,
        elapsed: started.elapsed(),
        units,
        threshold: StdDuration::from_secs_f64(base as f64 * multiplier),
    }
}

async fn populate_catalog(catalog: &TursoCatalog, count: usize) -> BenchResult<()> {
    let at = fixed_time();
    let source_id = stable_id("source");
    let storage_id = stable_id("storage");
    let mut source = CatalogSource::local_folder("Generated library", "/music/generated");
    source.id = source_id;
    source.created_at = at;
    source.last_synced_at = Some(at);
    let mut storage = MusicStorage::new(
        "Generated primary",
        MusicStorageKind::LocalDirectory,
        StorageRole::Primary,
        StorageCapabilities::default(),
    );
    storage.id = storage_id;
    storage.created_at = at;
    storage.updated_at = at;

    let transaction = catalog.begin().await?;
    transaction.sources().upsert(source).await?;
    transaction.music_storages().upsert(storage).await?;
    let mut occurrences = Vec::with_capacity(count);
    let mut playlist = CanonicalPlaylist::new("Generated favorites");
    playlist.id = stable_id("playlist");
    playlist.created_at = at;
    playlist.updated_at = at;

    for index in 0..count {
        let track_id = stable_id(&format!("track:{index}"));
        let asset_id = stable_id(&format!("asset:{index}"));
        let mut metadata = CanonicalMetadata::new();
        metadata.add(MetadataFieldValue::new(
            MetadataField::Title,
            MetadataValue::Text(format!("Track {index:06}")),
            MetadataProvenance::provider(source_id, format!("provider-{index:06}"), at),
        ));
        metadata.add(MetadataFieldValue::new(
            MetadataField::Artist,
            MetadataValue::Text(format!("Artist {:04}", index % 500)),
            MetadataProvenance::provider(source_id, format!("provider-{index:06}"), at),
        ));
        let track = CanonicalTrack {
            id: track_id,
            metadata,
            occurrences: vec![SourceTrackIdentity::new(
                source_id,
                format!("provider-{index:06}"),
            )],
            created_at: at,
            updated_at: at,
        };
        transaction.tracks().upsert(track).await?;

        let mut original = OriginalAsset::new_unverified(
            track_id,
            if index % 8 == 0 {
                OriginalAssetRole::Alternate
            } else {
                OriginalAssetRole::Master
            },
            format!("Artist {:04} - Track {index:06}.flac", index % 500),
        );
        original.record.id = asset_id;
        original.record.created_at = at;
        original.lineage_id = stable_id(&format!("lineage:{index}"));
        original.size_bytes = Some(25_000_000 + (index % 1000) as u64 * 1024);
        original.media_type = Some("audio/flac".into());
        original.is_preferred = index % 8 != 0;
        transaction.assets().upsert_original(original).await?;

        transaction
            .assets()
            .upsert_storage_object(StorageObject {
                storage_id,
                object_id: format!("object-{index:06}"),
                name: format!("Track {index:06}.flac"),
                display_path: Some(format!(
                    "Artist {:04}/Album {:04}",
                    index % 500,
                    index % 2000
                )),
                size_bytes: 25_000_000 + (index % 1000) as u64 * 1024,
                media_type: Some("audio/flac".into()),
                modified_at: Some(at),
                provider_version: Some(format!("etag-{index:06}")),
                hashes: Vec::new(),
                embedded_asset_id: Some(asset_id),
                is_trashed: false,
            })
            .await?;

        let mut occurrence = SourceTrackOccurrence::new(source_id, format!("provider-{index:06}"));
        occurrence.canonical_track_id = Some(track_id);
        occurrence.account_external_id = Some(format!("account-{}", index % 4));
        occurrence.external_url = Some(format!("https://example.invalid/tracks/{index}"));
        occurrence.availability = SourceAvailability::Obtainable;
        occurrence.last_seen_at = at;
        occurrences.push(occurrence);
        if index < 500 {
            playlist.entries.push(CanonicalPlaylistEntry {
                track_id,
                position: index as u32,
            });
        }
    }
    transaction
        .sources()
        .upsert_occurrences(occurrences)
        .await?;
    transaction.playlists().upsert(playlist).await?;
    transaction.commit().await?;
    Ok(())
}

async fn read_all_tracks(catalog: &TursoCatalog, page_size: u32) -> BenchResult<usize> {
    let mut request = RepositoryPageRequest {
        cursor: None,
        limit: Some(page_size),
    };
    let mut total = 0;
    loop {
        let page = TrackRepository::list(catalog, request.clone()).await?;
        total += page.items.len();
        match page.next_cursor {
            Some(cursor) => request.cursor = Some(cursor),
            None => return Ok(total),
        }
    }
}

async fn read_reconciliation_inputs(catalog: &TursoCatalog, page_size: u32) -> BenchResult<usize> {
    let source_id = stable_id("source");
    let storage_id = stable_id("storage");
    let mut total = 0;
    let mut cursor = None;
    loop {
        let page = catalog
            .list_occurrences(
                SourceOccurrenceFilter {
                    source_id: Some(source_id),
                    ..Default::default()
                },
                RepositoryPageRequest {
                    cursor,
                    limit: Some(page_size),
                },
            )
            .await?;
        total += page.items.len();
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut cursor = None;
    loop {
        let page = catalog
            .list_originals(
                OriginalAssetFilter::default(),
                RepositoryPageRequest {
                    cursor,
                    limit: Some(page_size),
                },
            )
            .await?;
        total += page.items.len();
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    let mut cursor = None;
    loop {
        let page = catalog
            .list_storage_objects(
                storage_id,
                RepositoryPageRequest {
                    cursor,
                    limit: Some(page_size),
                },
            )
            .await?;
        total += page.items.len();
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    Ok(total)
}

async fn seed_jobs(catalog: &TursoCatalog, count: usize) -> BenchResult<()> {
    let at = fixed_time();
    let source_id = stable_id("source");
    let mut run = SyncRun::new(SyncRunKind::CatalogSync, Some(source_id), None);
    run.id = stable_id("job-run");
    run.created_at = at;
    run.updated_at = at;
    let jobs = (0..count)
        .map(|index| {
            let mut job = SyncJob::with_retry_policy_at(
                run.id,
                SyncJobPayload::FetchCatalogPage {
                    version: SYNC_JOB_PAYLOAD_VERSION,
                    source_id,
                    collection_key: format!("collection-{}", index % 10),
                    sync_cursor: None,
                    page_cursor: None,
                },
                3,
                Some(format!("generated-job-{index:06}")),
                at,
            );
            job.id = stable_id(&format!("job:{index}"));
            job
        })
        .collect();
    catalog.create_run_with_jobs(run, jobs).await?;
    Ok(())
}

async fn claim_jobs(catalog: &TursoCatalog, expected: usize) -> BenchResult<usize> {
    let at = fixed_time() + Duration::hours(1);
    let mut claimed = 0;
    while claimed < expected {
        let job = catalog
            .claim_due(
                format!("worker-{}", claimed % 8),
                at,
                at + Duration::minutes(5),
            )
            .await?;
        if job.is_none() {
            break;
        }
        claimed += 1;
    }
    Ok(claimed)
}

async fn create_legacy_catalog(count: usize) -> BenchResult<TempDir> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("legacy.db");
    let database = Builder::new_local(path.to_str().ok_or("non-UTF-8 temporary path")?)
        .build()
        .await?;
    let connection = database.connect()?;
    connection
        .execute_batch(
            "CREATE TABLE tracks(id TEXT PRIMARY KEY,path TEXT NOT NULL,title TEXT NOT NULL,artist TEXT,album TEXT,duration_ms INTEGER,bpm REAL,key TEXT,genre TEXT,rating INTEGER,comment TEXT,added_at TEXT NOT NULL,last_played TEXT,play_count INTEGER DEFAULT 0,sample_rate INTEGER,bitrate INTEGER,year INTEGER,track_number INTEGER,waveform_json TEXT);\
             CREATE TABLE playlists(id TEXT PRIMARY KEY,name TEXT NOT NULL,description TEXT,created_at TEXT NOT NULL,modified_at TEXT NOT NULL);\
             CREATE TABLE playlist_tracks(playlist_id TEXT NOT NULL,track_id TEXT NOT NULL,position INTEGER NOT NULL,PRIMARY KEY(playlist_id,track_id));\
             CREATE TABLE sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,source_type TEXT NOT NULL,uri TEXT NOT NULL,config_json TEXT,track_count INTEGER,enabled INTEGER,created_at TEXT NOT NULL,last_scanned TEXT);\
             CREATE TABLE source_tracks(source_id TEXT NOT NULL,track_id TEXT NOT NULL,PRIMARY KEY(source_id,track_id));\
             CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);\
             BEGIN IMMEDIATE;",
        )
        .await?;
    let source_id = stable_id("legacy-source");
    connection
        .execute(
            "INSERT INTO sources VALUES(?1,'Legacy folder','local','/music/legacy','{}',?2,1,?3,?3)",
            (source_id.to_string(), count as i64, FIXED_TIME),
        )
        .await?;
    for index in 0..count {
        let track_id = stable_id(&format!("legacy-track:{index}"));
        connection
            .execute(
                "INSERT INTO tracks(id,path,title,artist,rating,play_count,added_at) VALUES(?1,?2,?3,?4,3,?5,?6)",
                (
                    track_id.to_string(),
                    format!(
                        "/music/legacy/Artist {:04}/track-{index:06}.flac",
                        index % 500
                    ),
                    format!("Legacy Track {index:06}"),
                    format!("Artist {:04}", index % 500),
                    index as i64 % 7,
                    FIXED_TIME,
                ),
            )
            .await?;
        connection
            .execute(
                "INSERT INTO source_tracks VALUES(?1,?2)",
                (source_id.to_string(), track_id.to_string()),
            )
            .await?;
    }
    connection.execute_batch("COMMIT;").await?;
    drop(connection);
    drop(database);
    Ok(directory)
}

pub fn stable_id(key: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, key.as_bytes())
}

fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(FIXED_TIME)
        .expect("fixed benchmark timestamp is valid")
        .with_timezone(&Utc)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> BenchResult<usize> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|error| format!("invalid {name}={value:?}: {error}").into()),
        Err(_) => Ok(default),
    }
}
