//! Provider-neutral durable job planning and worker orchestration.

use chrono::{DateTime, Duration, Utc};
use rec_core::{
    BoxFuture, SyncCheckpoint, SyncError, SyncErrorKind, SyncIssue, SyncIssueSeverity, SyncJob,
    SyncJobPayload, SyncProgress, SyncRun, SyncRunKind, SyncStatus,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Persistence boundary required by [`JobEngine`].
///
/// Implementations must make `create_run_with_jobs`, `claim_due`,
/// `save_job_and_event`, and `request_cancellation` transactional. `claim_due`
/// must atomically select and lease one due job, including an expired lease.
/// Jobs are ordered deterministically by `(next_attempt_at, created_at, id)`.
pub trait JobStore: Send + Sync {
    fn create_run_with_jobs(
        &self,
        run: SyncRun,
        jobs: Vec<SyncJob>,
    ) -> BoxFuture<'_, Result<(), JobStoreError>>;

    fn claim_due(
        &self,
        worker_id: String,
        claimed_at: DateTime<Utc>,
        lease_expires_at: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Option<SyncJob>, JobStoreError>>;

    fn load_run(&self, run_id: Uuid) -> BoxFuture<'_, Result<Option<SyncRun>, JobStoreError>>;

    fn load_job(&self, job_id: Uuid) -> BoxFuture<'_, Result<Option<SyncJob>, JobStoreError>>;

    fn list_run_jobs(&self, run_id: Uuid) -> BoxFuture<'_, Result<Vec<SyncJob>, JobStoreError>>;

    fn save_run(&self, run: SyncRun) -> BoxFuture<'_, Result<(), JobStoreError>>;

    /// Atomically persist the job state and its corresponding event.
    fn save_job_and_event(
        &self,
        job: SyncJob,
        event: JobEvent,
    ) -> BoxFuture<'_, Result<(), JobStoreError>>;

    fn append_event(&self, event: JobEvent) -> BoxFuture<'_, Result<(), JobStoreError>>;

    fn list_events(&self, run_id: Uuid) -> BoxFuture<'_, Result<Vec<JobEvent>, JobStoreError>>;

    /// Atomically record cancellation and transition a non-terminal run to
    /// `Cancelling`. Repeated requests are idempotent.
    fn request_cancellation(
        &self,
        run_id: Uuid,
        requested_at: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<SyncRun, JobStoreError>>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JobStoreError {
    #[error("job store is temporarily unavailable: {0}")]
    TemporarilyUnavailable(String),
    #[error("job store rejected job state: {0}")]
    InvalidState(String),
    #[error("job store record was not found: {0}")]
    NotFound(String),
    #[error("job store failed: {0}")]
    Permanent(String),
}

/// A terminal or retry outcome reported by an executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobCompletion {
    Succeeded {
        bytes_transferred: u64,
    },
    Skipped {
        reason: String,
    },
    Retry {
        error: SyncError,
        retry_at: DateTime<Utc>,
    },
    Failed {
        error: SyncError,
    },
    Cancelled,
}

/// One durable stage boundary returned by a provider-neutral executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStep {
    Checkpoint {
        checkpoint: SyncCheckpoint,
        progress: SyncProgress,
        message: Option<String>,
    },
    Complete(JobCompletion),
}

/// Executes one bounded, idempotent stage. After a checkpoint the engine persists
/// state, re-checks cancellation, and calls the executor again with the updated job.
pub trait JobExecutor: Send + Sync {
    fn execute_step(&self, job: SyncJob) -> BoxFuture<'_, Result<JobStep, SyncError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEventKind {
    Planned,
    Claimed,
    Checkpointed,
    Succeeded,
    Skipped,
    Retrying,
    Failed,
    CancellationRequested,
    Cancelled,
    RunSucceeded,
    RunFailed,
    RunCancelled,
}

/// Structured durable event suitable for UI progress and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEvent {
    pub id: Uuid,
    pub job_id: Option<Uuid>,
    pub run_id: Uuid,
    pub kind: JobEventKind,
    pub at: DateTime<Utc>,
    pub message: Option<String>,
    pub progress: Option<SyncProgress>,
    pub issue: Option<SyncIssue>,
}

impl JobEvent {
    fn for_job(job: &SyncJob, kind: JobEventKind, at: DateTime<Utc>) -> Self {
        Self {
            id: Uuid::new_v4(),
            job_id: Some(job.id),
            run_id: job.run_id,
            kind,
            at,
            message: None,
            progress: Some(job.progress),
            issue: None,
        }
    }

    fn for_run(run_id: Uuid, kind: JobEventKind, at: DateTime<Utc>) -> Self {
        Self {
            id: Uuid::new_v4(),
            job_id: None,
            run_id,
            kind,
            at,
            message: None,
            progress: None,
            issue: None,
        }
    }
}

/// A fully materialized run and its independently retryable jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableJobPlan {
    pub run: SyncRun,
    pub jobs: Vec<SyncJob>,
}

pub struct JobPlanner;

impl JobPlanner {
    pub fn plan(
        kind: SyncRunKind,
        source_id: Option<Uuid>,
        storage_id: Option<Uuid>,
        payloads: impl IntoIterator<Item = SyncJobPayload>,
        created_at: DateTime<Utc>,
    ) -> DurableJobPlan {
        let run = SyncRun::at(kind, source_id, storage_id, created_at);
        let mut jobs: Vec<_> = payloads
            .into_iter()
            .map(|payload| {
                let key = deterministic_payload_key(&payload);
                SyncJob::with_retry_policy_at(run.id, payload, 3, Some(key), created_at)
            })
            .collect();
        jobs.sort_by(|left, right| left.idempotency_key.cmp(&right.idempotency_key));
        DurableJobPlan { run, jobs }
    }

    /// Build a new run from unfinished jobs while preserving checkpoints,
    /// progress, payloads, and provider idempotency keys.
    pub fn restart(
        previous: &SyncRun,
        previous_jobs: &[SyncJob],
        created_at: DateTime<Utc>,
    ) -> Result<DurableJobPlan, JobStoreError> {
        let run = SyncRun::resumed_from_at(previous, created_at)
            .map_err(|error| JobStoreError::InvalidState(error.to_string()))?;
        let mut jobs = Vec::new();
        for previous_job in previous_jobs {
            if previous_job.run_id != previous.id {
                return Err(JobStoreError::InvalidState(
                    "restart contains a job from another run".into(),
                ));
            }
            if previous_job.status == SyncStatus::Succeeded {
                continue;
            }
            let mut job = SyncJob::with_retry_policy_at(
                run.id,
                previous_job.payload.clone(),
                previous_job.max_attempts,
                Some(previous_job.idempotency_key.clone()),
                created_at.max(previous_job.updated_at),
            );
            job.checkpoint = previous_job.checkpoint.clone();
            job.progress = previous_job.progress;
            jobs.push(job);
        }
        jobs.sort_by(|left, right| left.idempotency_key.cmp(&right.idempotency_key));
        Ok(DurableJobPlan { run, jobs })
    }
}

/// Orchestration layer over core's persistable run and job models.
pub struct JobEngine<S> {
    store: S,
    lease_duration: Duration,
    max_checkpoint_steps: usize,
}

impl<S> JobEngine<S>
where
    S: JobStore,
{
    pub fn new(store: S) -> Self {
        Self {
            store,
            lease_duration: Duration::minutes(5),
            max_checkpoint_steps: 10_000,
        }
    }

    pub fn with_lease_duration(mut self, lease_duration: Duration) -> Self {
        self.lease_duration = lease_duration.max(Duration::seconds(1));
        self
    }

    pub fn with_max_checkpoint_steps(mut self, max_checkpoint_steps: usize) -> Self {
        self.max_checkpoint_steps = max_checkpoint_steps.max(1);
        self
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub async fn persist_plan(
        &self,
        run: SyncRun,
        jobs: Vec<SyncJob>,
    ) -> Result<(), JobStoreError> {
        if run.status != SyncStatus::Pending
            || jobs.iter().any(|job| {
                job.run_id != run.id
                    || job.status != SyncStatus::Pending
                    || job.validate_kind().is_err()
                    || !job.payload.has_supported_version()
            })
        {
            return Err(JobStoreError::InvalidState(
                "job plan contains invalid run, job, kind, or payload state".into(),
            ));
        }
        self.store.create_run_with_jobs(run, jobs).await
    }

    pub async fn persist_durable_plan(&self, plan: DurableJobPlan) -> Result<(), JobStoreError> {
        self.persist_plan(plan.run, plan.jobs).await
    }

    /// Atomically claims at most one due job. Deferred jobs use
    /// `next_attempt_at`; interrupted jobs become due after lease expiration.
    pub async fn claim_next(
        &self,
        worker_id: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<Option<SyncJob>, JobStoreError> {
        let job = self
            .store
            .claim_due(worker_id.into(), now, now + self.lease_duration)
            .await?;
        if let Some(job) = &job {
            validate_job(job)?;
            let mut run = self
                .store
                .load_run(job.run_id)
                .await?
                .ok_or_else(|| JobStoreError::NotFound(format!("run {}", job.run_id)))?;
            if run.status.can_start() {
                run.start_at(now)
                    .map_err(|error| JobStoreError::InvalidState(error.to_string()))?;
                self.store.save_run(run).await?;
            }
        }
        Ok(job)
    }

    pub async fn request_cancellation(
        &self,
        run_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<SyncRun, JobStoreError> {
        let run = self.store.request_cancellation(run_id, now).await?;
        let event = JobEvent::for_run(run_id, JobEventKind::CancellationRequested, now);
        self.store.append_event(event).await?;
        Ok(run)
    }

    /// Persist a monotonic checkpoint while retaining the active lease.
    pub async fn checkpoint(
        &self,
        mut job: SyncJob,
        checkpoint: SyncCheckpoint,
        progress: SyncProgress,
        message: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<SyncJob, JobStoreError> {
        if job.status != SyncStatus::Running {
            return Err(JobStoreError::InvalidState(format!(
                "job {} is not running",
                job.id
            )));
        }
        if checkpoint.processed_items < job.checkpoint.as_ref().map_or(0, |old| old.processed_items)
            || progress.processed_items < job.progress.processed_items
            || !progress.is_consistent()
        {
            return Err(JobStoreError::InvalidState(
                "checkpoint and progress must be monotonic and consistent".into(),
            ));
        }
        job.checkpoint = Some(checkpoint.clone());
        job.progress = progress;
        job.updated_at = now.max(job.updated_at).max(checkpoint.created_at);
        let mut event = JobEvent::for_job(&job, JobEventKind::Checkpointed, job.updated_at);
        event.message = message;
        self.store.save_job_and_event(job.clone(), event).await?;
        Ok(job)
    }

    /// Execute one claimed job through bounded checkpoint stages.
    pub async fn run_once<E: JobExecutor>(
        &self,
        worker_id: impl Into<String>,
        now: DateTime<Utc>,
        executor: &E,
    ) -> Result<Option<JobEvent>, JobStoreError> {
        let Some(mut job) = self.claim_next(worker_id, now).await? else {
            return Ok(None);
        };
        self.store
            .append_event(JobEvent::for_job(&job, JobEventKind::Claimed, now))
            .await?;

        for _ in 0..self.max_checkpoint_steps {
            if self.cancellation_requested(job.run_id).await? {
                let event = self.complete(job, JobCompletion::Cancelled, now).await?;
                self.finalize_run(event.run_id, now).await?;
                return Ok(Some(event));
            }

            match executor.execute_step(job.clone()).await {
                Ok(JobStep::Checkpoint {
                    checkpoint,
                    progress,
                    message,
                }) => {
                    job = self
                        .checkpoint(job, checkpoint, progress, message, now)
                        .await?;
                }
                Ok(JobStep::Complete(completion)) => {
                    let event = self.complete(job, completion, now).await?;
                    self.finalize_run(event.run_id, now).await?;
                    return Ok(Some(event));
                }
                Err(error) => {
                    let completion = if error.retryable {
                        JobCompletion::Retry {
                            retry_at: retry_time(&error, now),
                            error,
                        }
                    } else {
                        JobCompletion::Failed { error }
                    };
                    let event = self.complete(job, completion, now).await?;
                    self.finalize_run(event.run_id, now).await?;
                    return Ok(Some(event));
                }
            }
        }

        let error = SyncError::at(
            SyncErrorKind::Internal,
            "checkpoint_limit",
            "executor exceeded the checkpoint stage limit",
            false,
            now,
        );
        let event = self
            .complete(job, JobCompletion::Failed { error }, now)
            .await?;
        self.finalize_run(event.run_id, now).await?;
        Ok(Some(event))
    }

    /// Applies one outcome atomically with its structured event. The stable
    /// idempotency key is never altered across retries.
    pub async fn complete(
        &self,
        mut job: SyncJob,
        outcome: JobCompletion,
        now: DateTime<Utc>,
    ) -> Result<JobEvent, JobStoreError> {
        if job.status != SyncStatus::Running && !matches!(outcome, JobCompletion::Cancelled) {
            return Err(JobStoreError::InvalidState(format!(
                "job {} is not running",
                job.id
            )));
        }

        let (kind, message, issue) = match outcome {
            JobCompletion::Succeeded { bytes_transferred } => {
                job.progress.record_success();
                job.progress.add_bytes(bytes_transferred);
                finish_job(&mut job, SyncStatus::Succeeded, None, now);
                (JobEventKind::Succeeded, None, None)
            }
            JobCompletion::Skipped { reason } => {
                job.progress.record_skip();
                finish_job(&mut job, SyncStatus::Succeeded, None, now);
                (JobEventKind::Skipped, Some(reason), None)
            }
            JobCompletion::Retry { error, retry_at } => {
                let message = error.message.clone();
                let issue = issue_for_error(&error);
                job.issues.push(issue.clone());
                if error.retryable && job.schedule_retry_at(retry_at.max(now), now) {
                    (JobEventKind::Retrying, Some(message), Some(issue))
                } else {
                    job.progress.record_failure();
                    finish_job(&mut job, SyncStatus::Failed, Some(error), now);
                    (JobEventKind::Failed, Some(message), Some(issue))
                }
            }
            JobCompletion::Failed { error } => {
                let message = error.message.clone();
                let issue = issue_for_error(&error);
                job.progress.record_failure();
                job.issues.push(issue.clone());
                finish_job(&mut job, SyncStatus::Failed, Some(error), now);
                (JobEventKind::Failed, Some(message), Some(issue))
            }
            JobCompletion::Cancelled => {
                finish_job(&mut job, SyncStatus::Cancelled, None, now);
                (JobEventKind::Cancelled, None, None)
            }
        };
        let mut event = JobEvent::for_job(&job, kind, job.updated_at);
        event.message = message;
        event.issue = issue;
        self.store.save_job_and_event(job, event.clone()).await?;
        Ok(event)
    }

    async fn cancellation_requested(&self, run_id: Uuid) -> Result<bool, JobStoreError> {
        let run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| JobStoreError::NotFound(format!("run {run_id}")))?;
        Ok(run.status == SyncStatus::Cancelling || run.cancellation_requested_at.is_some())
    }

    async fn finalize_run(&self, run_id: Uuid, now: DateTime<Utc>) -> Result<(), JobStoreError> {
        let jobs = self.store.list_run_jobs(run_id).await?;
        if jobs.is_empty() || jobs.iter().any(|job| !job.status.is_terminal()) {
            return Ok(());
        }
        let mut run = self
            .store
            .load_run(run_id)
            .await?
            .ok_or_else(|| JobStoreError::NotFound(format!("run {run_id}")))?;
        if run.status.is_terminal() {
            return Ok(());
        }

        run.progress = aggregate_progress(&jobs);
        let (status, event_kind) = if jobs.iter().any(|job| job.status == SyncStatus::Failed) {
            run.error = jobs.iter().find_map(|job| job.error.clone());
            (SyncStatus::Failed, JobEventKind::RunFailed)
        } else if run.status == SyncStatus::Cancelling
            || jobs.iter().any(|job| job.status == SyncStatus::Cancelled)
        {
            (SyncStatus::Cancelled, JobEventKind::RunCancelled)
        } else {
            (SyncStatus::Succeeded, JobEventKind::RunSucceeded)
        };
        run.status = status;
        run.updated_at = now.max(run.updated_at);
        run.finished_at = Some(run.updated_at);
        self.store.save_run(run).await?;
        self.store
            .append_event(JobEvent::for_run(run_id, event_kind, now))
            .await
    }
}

fn validate_job(job: &SyncJob) -> Result<(), JobStoreError> {
    if job.validate_kind().is_err() || !job.payload.has_supported_version() {
        Err(JobStoreError::InvalidState(format!(
            "claimed job {} has an unsupported payload",
            job.id
        )))
    } else if job.status != SyncStatus::Running || job.lease_owner.is_none() {
        Err(JobStoreError::InvalidState(format!(
            "claimed job {} has no active running lease",
            job.id
        )))
    } else {
        Ok(())
    }
}

fn deterministic_payload_key(payload: &SyncJobPayload) -> String {
    // Debug output for these field-only core enums is stable within payload schema v1.
    format!("sync:{:?}:{payload:?}", payload.kind())
}

fn retry_time(error: &SyncError, now: DateTime<Utc>) -> DateTime<Utc> {
    let delay = match error.kind {
        SyncErrorKind::RateLimited => Duration::minutes(5),
        SyncErrorKind::TemporarilyUnavailable | SyncErrorKind::Io => Duration::minutes(1),
        _ => Duration::seconds(30),
    };
    now + delay
}

fn aggregate_progress(jobs: &[SyncJob]) -> SyncProgress {
    let mut progress = SyncProgress::default();
    for job in jobs {
        progress.discovered_items = progress
            .discovered_items
            .saturating_add(job.progress.discovered_items);
        progress.processed_items = progress
            .processed_items
            .saturating_add(job.progress.processed_items);
        progress.succeeded_items = progress
            .succeeded_items
            .saturating_add(job.progress.succeeded_items);
        progress.skipped_items = progress
            .skipped_items
            .saturating_add(job.progress.skipped_items);
        progress.failed_items = progress
            .failed_items
            .saturating_add(job.progress.failed_items);
        progress.bytes_transferred = progress
            .bytes_transferred
            .saturating_add(job.progress.bytes_transferred);
    }
    progress.total_items = Some(progress.discovered_items.max(progress.processed_items));
    progress
}

fn finish_job(job: &mut SyncJob, status: SyncStatus, error: Option<SyncError>, at: DateTime<Utc>) {
    debug_assert!(status.is_terminal());
    let at = at.max(job.updated_at);
    job.status = status;
    job.error = error;
    job.lease_owner = None;
    job.lease_expires_at = None;
    job.next_attempt_at = None;
    job.updated_at = at;
    job.finished_at = Some(at);
}

fn issue_for_error(error: &SyncError) -> SyncIssue {
    SyncIssue::at(
        SyncIssueSeverity::Error,
        error.kind,
        error.code.clone(),
        error.message.clone(),
        error.retryable,
        error.occurred_at,
    )
}

/// Maps a storage failure into the job error vocabulary without exposing provider
/// credentials or raw request details.
pub fn storage_error_kind(retryable: bool) -> SyncErrorKind {
    if retryable {
        SyncErrorKind::TemporarilyUnavailable
    } else {
        SyncErrorKind::Provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rec_core::{SyncJobPayload, SYNC_JOB_PAYLOAD_VERSION};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        runs: Mutex<BTreeMap<Uuid, SyncRun>>,
        jobs: Mutex<BTreeMap<Uuid, SyncJob>>,
        events: Mutex<Vec<JobEvent>>,
    }

    impl JobStore for FakeStore {
        fn create_run_with_jobs(
            &self,
            run: SyncRun,
            jobs: Vec<SyncJob>,
        ) -> BoxFuture<'_, Result<(), JobStoreError>> {
            self.runs.lock().unwrap().insert(run.id, run);
            self.jobs
                .lock()
                .unwrap()
                .extend(jobs.into_iter().map(|job| (job.id, job)));
            Box::pin(async { Ok(()) })
        }

        fn claim_due(
            &self,
            worker: String,
            at: DateTime<Utc>,
            expires: DateTime<Utc>,
        ) -> BoxFuture<'_, Result<Option<SyncJob>, JobStoreError>> {
            let mut jobs = self.jobs.lock().unwrap();
            let id = jobs
                .values()
                .filter(|job| job.is_claimable_at(at))
                .min_by_key(|job| (job.next_attempt_at, job.created_at, job.id))
                .map(|job| job.id);
            let claimed = id.and_then(|id| {
                let job = jobs.get_mut(&id).unwrap();
                job.claim_at(worker, at, expires).then(|| job.clone())
            });
            Box::pin(async move { Ok(claimed) })
        }

        fn load_run(&self, id: Uuid) -> BoxFuture<'_, Result<Option<SyncRun>, JobStoreError>> {
            let value = self.runs.lock().unwrap().get(&id).cloned();
            Box::pin(async move { Ok(value) })
        }

        fn load_job(&self, id: Uuid) -> BoxFuture<'_, Result<Option<SyncJob>, JobStoreError>> {
            let value = self.jobs.lock().unwrap().get(&id).cloned();
            Box::pin(async move { Ok(value) })
        }

        fn list_run_jobs(&self, id: Uuid) -> BoxFuture<'_, Result<Vec<SyncJob>, JobStoreError>> {
            let value = self
                .jobs
                .lock()
                .unwrap()
                .values()
                .filter(|job| job.run_id == id)
                .cloned()
                .collect();
            Box::pin(async move { Ok(value) })
        }

        fn save_run(&self, run: SyncRun) -> BoxFuture<'_, Result<(), JobStoreError>> {
            self.runs.lock().unwrap().insert(run.id, run);
            Box::pin(async { Ok(()) })
        }

        fn save_job_and_event(
            &self,
            job: SyncJob,
            event: JobEvent,
        ) -> BoxFuture<'_, Result<(), JobStoreError>> {
            self.jobs.lock().unwrap().insert(job.id, job);
            self.events.lock().unwrap().push(event);
            Box::pin(async { Ok(()) })
        }

        fn append_event(&self, event: JobEvent) -> BoxFuture<'_, Result<(), JobStoreError>> {
            self.events.lock().unwrap().push(event);
            Box::pin(async { Ok(()) })
        }

        fn list_events(&self, run_id: Uuid) -> BoxFuture<'_, Result<Vec<JobEvent>, JobStoreError>> {
            let events = self
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.run_id == run_id)
                .cloned()
                .collect();
            Box::pin(async move { Ok(events) })
        }

        fn request_cancellation(
            &self,
            id: Uuid,
            at: DateTime<Utc>,
        ) -> BoxFuture<'_, Result<SyncRun, JobStoreError>> {
            let result = self
                .runs
                .lock()
                .unwrap()
                .get_mut(&id)
                .ok_or_else(|| JobStoreError::NotFound(format!("run {id}")))
                .and_then(|run| {
                    if !run.status.is_terminal() && run.status != SyncStatus::Cancelling {
                        run.request_cancellation_at(at)
                            .map_err(|error| JobStoreError::InvalidState(error.to_string()))?;
                    }
                    Ok(run.clone())
                });
            Box::pin(async move { result })
        }
    }

    fn payload() -> SyncJobPayload {
        SyncJobPayload::ReplicateAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id: Uuid::new_v4(),
            source_storage_id: Uuid::new_v4(),
            source_object_id: "source".into(),
            source_object_key: None,
            destination_storage_id: Uuid::new_v4(),
            destination_object_key: "target".into(),
        }
    }

    struct CheckpointThenSuccess(Mutex<bool>);
    impl JobExecutor for CheckpointThenSuccess {
        fn execute_step(&self, job: SyncJob) -> BoxFuture<'_, Result<JobStep, SyncError>> {
            let first = !*self.0.lock().unwrap();
            *self.0.lock().unwrap() = true;
            Box::pin(async move {
                if first {
                    let mut progress = job.progress;
                    progress.discover(1);
                    Ok(JobStep::Checkpoint {
                        checkpoint: SyncCheckpoint::new(0),
                        progress,
                        message: Some("downloaded".into()),
                    })
                } else {
                    Ok(JobStep::Complete(JobCompletion::Succeeded {
                        bytes_transferred: 10,
                    }))
                }
            })
        }
    }

    #[tokio::test]
    async fn executor_persists_checkpoint_progress_and_terminal_events() {
        let store = FakeStore::default();
        let engine = JobEngine::new(store);
        let now = Utc::now();
        let plan = JobPlanner::plan(SyncRunKind::BackupReplication, None, None, [payload()], now);
        let run_id = plan.run.id;
        engine.persist_durable_plan(plan).await.unwrap();
        let event = engine
            .run_once("worker", now, &CheckpointThenSuccess(Mutex::new(false)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, JobEventKind::Succeeded);
        let events = engine.store().list_events(run_id).await.unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == JobEventKind::Checkpointed));
        assert!(events
            .iter()
            .any(|event| event.kind == JobEventKind::RunSucceeded));
        assert_eq!(
            engine
                .store()
                .load_run(run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SyncStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn retry_is_durable_and_preserves_idempotency_key() {
        let store = FakeStore::default();
        let engine = JobEngine::new(store);
        let now = Utc::now();
        let plan = JobPlanner::plan(SyncRunKind::BackupReplication, None, None, [payload()], now);
        let job_id = plan.jobs[0].id;
        let key = plan.jobs[0].idempotency_key.clone();
        engine.persist_durable_plan(plan).await.unwrap();
        let job = engine.claim_next("worker", now).await.unwrap().unwrap();
        engine
            .complete(
                job,
                JobCompletion::Retry {
                    error: SyncError::at(
                        SyncErrorKind::TemporarilyUnavailable,
                        "offline",
                        "backup offline",
                        true,
                        now,
                    ),
                    retry_at: now + Duration::minutes(10),
                },
                now,
            )
            .await
            .unwrap();
        let saved = engine.store().load_job(job_id).await.unwrap().unwrap();
        assert_eq!(saved.idempotency_key, key);
        assert_eq!(saved.status, SyncStatus::Pending);
        assert_eq!(saved.next_attempt_at, Some(now + Duration::minutes(10)));
        assert!(engine
            .claim_next("other", now + Duration::minutes(9))
            .await
            .unwrap()
            .is_none());
        assert!(engine
            .claim_next("other", now + Duration::minutes(10))
            .await
            .unwrap()
            .is_some());
    }

    struct PerBackupExecutor {
        failing_storage_id: Uuid,
        calls: Mutex<Vec<Uuid>>,
    }

    impl JobExecutor for PerBackupExecutor {
        fn execute_step(&self, job: SyncJob) -> BoxFuture<'_, Result<JobStep, SyncError>> {
            let destination = match job.payload {
                SyncJobPayload::ReplicateAsset {
                    destination_storage_id,
                    ..
                } => destination_storage_id,
                _ => Uuid::nil(),
            };
            self.calls.lock().unwrap().push(destination);
            let failing = destination == self.failing_storage_id;
            Box::pin(async move {
                if failing {
                    Ok(JobStep::Complete(JobCompletion::Failed {
                        error: SyncError::new(
                            SyncErrorKind::Provider,
                            "backup_failed",
                            "one backup failed",
                            false,
                        ),
                    }))
                } else {
                    Ok(JobStep::Complete(JobCompletion::Succeeded {
                        bytes_transferred: 10,
                    }))
                }
            })
        }
    }

    #[tokio::test]
    async fn cancellation_is_observed_before_executor_side_effects() {
        let now = Utc::now();
        let plan = JobPlanner::plan(SyncRunKind::BackupReplication, None, None, [payload()], now);
        let run_id = plan.run.id;
        let engine = JobEngine::new(FakeStore::default());
        engine.persist_durable_plan(plan).await.unwrap();
        engine.request_cancellation(run_id, now).await.unwrap();
        let executor = PerBackupExecutor {
            failing_storage_id: Uuid::nil(),
            calls: Mutex::new(Vec::new()),
        };
        let event = engine
            .run_once("worker", now, &executor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, JobEventKind::Cancelled);
        assert!(executor.calls.lock().unwrap().is_empty());
        assert_eq!(
            engine
                .store()
                .load_run(run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SyncStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn one_backup_failure_does_not_prevent_other_backup_execution() {
        let now = Utc::now();
        let failed_storage = Uuid::new_v4();
        let healthy_storage = Uuid::new_v4();
        let make_payload = |destination_storage_id| SyncJobPayload::ReplicateAsset {
            version: SYNC_JOB_PAYLOAD_VERSION,
            asset_id: Uuid::new_v4(),
            source_storage_id: Uuid::new_v4(),
            source_object_id: "source".into(),
            source_object_key: None,
            destination_storage_id,
            destination_object_key: "target".into(),
        };
        let plan = JobPlanner::plan(
            SyncRunKind::BackupReplication,
            None,
            None,
            [make_payload(failed_storage), make_payload(healthy_storage)],
            now,
        );
        let run_id = plan.run.id;
        let engine = JobEngine::new(FakeStore::default());
        engine.persist_durable_plan(plan).await.unwrap();
        let executor = PerBackupExecutor {
            failing_storage_id: failed_storage,
            calls: Mutex::new(Vec::new()),
        };
        engine.run_once("worker", now, &executor).await.unwrap();
        engine.run_once("worker", now, &executor).await.unwrap();
        let calls = executor.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&failed_storage));
        assert!(calls.contains(&healthy_storage));
        assert_eq!(
            engine
                .store()
                .load_run(run_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            SyncStatus::Failed
        );
    }

    #[test]
    fn restart_keeps_checkpoint_and_key_but_skips_successes() {
        let now = Utc::now();
        let mut run = SyncRun::at(SyncRunKind::BackupReplication, None, None, now);
        run.status = SyncStatus::Failed;
        let mut unfinished =
            SyncJob::with_retry_policy_at(run.id, payload(), 5, Some("stable"), now);
        unfinished.status = SyncStatus::Failed;
        unfinished.checkpoint = Some(SyncCheckpoint::at(2, now));
        let mut succeeded = SyncJob::new(run.id, payload());
        succeeded.status = SyncStatus::Succeeded;
        let restarted = JobPlanner::restart(&run, &[unfinished, succeeded], now).unwrap();
        assert_eq!(restarted.jobs.len(), 1);
        assert_eq!(restarted.jobs[0].idempotency_key, "stable");
        assert_eq!(
            restarted.jobs[0]
                .checkpoint
                .as_ref()
                .unwrap()
                .processed_items,
            2
        );
    }
}
