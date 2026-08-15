//! Background scheduling and durable worker lifecycle.
//!
//! The scheduler deliberately does not keep a second job queue. [`ScheduledTask::DurableJobs`]
//! is a wake-up for the durable `JobStore`/`BackupService` worker: claiming, lease reclaim and
//! retry state remain owned by the store. A task runner can return the store's earliest known due
//! time so deferred work is not polled early.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use rec_core::BoxFuture;
use rec_sync::{ReplicationRequest, VerificationRequest};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration as StdDuration,
};
use thiserror::Error;
use tokio::{
    sync::{watch, Mutex, Notify},
    task::JoinHandle,
};

/// A bounded unit of background work. Implementations normally translate these into source-sync
/// and [`super::backup::BackupSchedulerHook`] calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScheduledTask {
    /// Claim one due durable job. Calling this on startup reclaims expired leases.
    DurableJobs,
    SoundCloudSync,
    AutomaticBackup,
    ScheduledBackup,
    DailyVerification,
    MonthlyInventory,
    DailySnapshot,
}

/// Result of one bounded task invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskOutcome {
    /// `true` when another durable job may be claimed immediately.
    pub made_progress: bool,
    /// An authoritative earlier/later due time, typically `SyncJob::next_attempt_at`.
    pub next_due: Option<DateTime<Utc>>,
}

impl TaskOutcome {
    pub const IDLE: Self = Self {
        made_progress: false,
        next_due: None,
    };

    pub const fn progress() -> Self {
        Self {
            made_progress: true,
            next_due: None,
        }
    }

    pub const fn deferred(next_due: DateTime<Utc>) -> Self {
        Self {
            made_progress: false,
            next_due: Some(next_due),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchedulerTaskError {
    #[error("scheduler dependency is temporarily unavailable: {0}")]
    Unavailable(String),
    #[error("scheduler task failed: {0}")]
    Failed(String),
}

impl SchedulerTaskError {
    fn retryable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }
}

/// Injectable service boundary. Each call must be bounded and cancellation-safe: shutdown may
/// drop an in-flight future and then invoke `checkpoint`.
pub trait SchedulerTasks: Send + Sync + 'static {
    fn run(
        &self,
        task: ScheduledTask,
        now: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<TaskOutcome, SchedulerTaskError>>;

    /// Flush/checkpoint interrupted work. The scheduler waits up to `checkpoint_timeout`.
    fn checkpoint(&self, _now: DateTime<Utc>) -> BoxFuture<'_, Result<(), SchedulerTaskError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Application-specific discovery and SoundCloud trigger boundary used by
/// [`BackupServiceScheduler`]. Keeping discovery here avoids coupling the scheduler to UI state or
/// provider credential implementations.
pub trait SchedulerTriggers: Send + Sync + 'static {
    fn trigger_soundcloud_sync(
        &self,
        now: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<(), SchedulerTaskError>>;

    fn automatic_backups(
        &self,
        _now: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Vec<ReplicationRequest>, SchedulerTaskError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn scheduled_backups(
        &self,
        _now: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<Vec<ReplicationRequest>, SchedulerTaskError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn daily_verification(
        &self,
        _now: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<(Vec<VerificationRequest>, u64), SchedulerTaskError>> {
        Box::pin(async { Ok((Vec::new(), 1)) })
    }

    fn checkpoint(&self, _now: DateTime<Utc>) -> BoxFuture<'_, Result<(), SchedulerTaskError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Ready-to-compose task runner around the durable backup hooks. The trigger implementation owns
/// SoundCloud sync and catalog policy discovery; `BackupService` continues to own durable planning
/// and execution.
pub struct BackupServiceScheduler<T> {
    backup: Arc<super::backup::BackupService>,
    triggers: Arc<T>,
    worker_id: String,
}

impl<T> BackupServiceScheduler<T>
where
    T: SchedulerTriggers,
{
    pub fn new(
        backup: Arc<super::backup::BackupService>,
        triggers: Arc<T>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            backup,
            triggers,
            worker_id: worker_id.into(),
        }
    }

    async fn run_replications(
        &self,
        requests: Vec<ReplicationRequest>,
        scheduled: bool,
        now: DateTime<Utc>,
    ) -> Result<TaskOutcome, SchedulerTaskError> {
        let made_progress = !requests.is_empty();
        for request in requests {
            let hook = if scheduled {
                super::backup::BackupSchedulerHook::ScheduledReplication(request)
            } else {
                super::backup::BackupSchedulerHook::VerifiedPrimaryWrite(request)
            };
            self.backup
                .handle_scheduler_hook(hook, &self.worker_id, now)
                .await
                .map_err(map_backup_error)?;
        }
        Ok(TaskOutcome {
            made_progress,
            next_due: None,
        })
    }
}

impl<T> SchedulerTasks for BackupServiceScheduler<T>
where
    T: SchedulerTriggers,
{
    fn run(
        &self,
        task: ScheduledTask,
        now: DateTime<Utc>,
    ) -> BoxFuture<'_, Result<TaskOutcome, SchedulerTaskError>> {
        Box::pin(async move {
            use super::backup::{BackupSchedulerHook, BackupSchedulerOutcome};

            match task {
                ScheduledTask::DurableJobs => {
                    let outcome = self
                        .backup
                        .handle_scheduler_hook(
                            BackupSchedulerHook::WorkerWake,
                            &self.worker_id,
                            now,
                        )
                        .await
                        .map_err(map_backup_error)?;
                    Ok(if matches!(outcome, BackupSchedulerOutcome::Idle) {
                        TaskOutcome::IDLE
                    } else {
                        TaskOutcome::progress()
                    })
                }
                ScheduledTask::SoundCloudSync => {
                    self.triggers.trigger_soundcloud_sync(now).await?;
                    Ok(TaskOutcome::progress())
                }
                ScheduledTask::AutomaticBackup => {
                    let requests = self.triggers.automatic_backups(now).await?;
                    self.run_replications(requests, false, now).await
                }
                ScheduledTask::ScheduledBackup => {
                    let requests = self.triggers.scheduled_backups(now).await?;
                    self.run_replications(requests, true, now).await
                }
                ScheduledTask::DailyVerification => {
                    let (requests, sample_modulus) = self.triggers.daily_verification(now).await?;
                    let made_progress = !requests.is_empty();
                    self.backup
                        .handle_scheduler_hook(
                            BackupSchedulerHook::Verification {
                                requests,
                                sample_modulus,
                            },
                            &self.worker_id,
                            now,
                        )
                        .await
                        .map_err(map_backup_error)?;
                    Ok(TaskOutcome {
                        made_progress,
                        next_due: None,
                    })
                }
                ScheduledTask::MonthlyInventory => {
                    let outcome = self
                        .backup
                        .handle_scheduler_hook(
                            BackupSchedulerHook::MonthlyInventory,
                            &self.worker_id,
                            now,
                        )
                        .await
                        .map_err(map_backup_error)?;
                    Ok(TaskOutcome {
                        made_progress: matches!(
                            outcome,
                            BackupSchedulerOutcome::VerificationRun(Some(_))
                        ),
                        next_due: None,
                    })
                }
                ScheduledTask::DailySnapshot => {
                    let outcome = self
                        .backup
                        .handle_scheduler_hook(
                            BackupSchedulerHook::DailySnapshot,
                            &self.worker_id,
                            now,
                        )
                        .await
                        .map_err(map_backup_error)?;
                    Ok(TaskOutcome {
                        made_progress: matches!(
                            outcome,
                            BackupSchedulerOutcome::SnapshotRun(Some(_))
                        ),
                        next_due: None,
                    })
                }
            }
        })
    }

    fn checkpoint(&self, now: DateTime<Utc>) -> BoxFuture<'_, Result<(), SchedulerTaskError>> {
        self.triggers.checkpoint(now)
    }
}

fn map_backup_error(error: super::backup::BackupError) -> SchedulerTaskError {
    if error.retryable() {
        SchedulerTaskError::Unavailable(error.to_string())
    } else {
        SchedulerTaskError::Failed(error.to_string())
    }
}

pub trait SchedulerClock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl SchedulerClock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub trait SchedulerSleeper: Send + Sync + 'static {
    fn sleep_until(&self, deadline: DateTime<Utc>) -> BoxFuture<'_, ()>;
}

#[derive(Debug, Default)]
pub struct TokioSleeper;

impl SchedulerSleeper for TokioSleeper {
    fn sleep_until(&self, deadline: DateTime<Utc>) -> BoxFuture<'_, ()> {
        let delay = (deadline - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        Box::pin(tokio::time::sleep(delay))
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub soundcloud_interval: Option<Duration>,
    pub automatic_backup_interval: Option<Duration>,
    pub scheduled_backup_interval: Option<Duration>,
    pub durable_idle_poll: Duration,
    pub unavailable_retry_initial: Duration,
    pub unavailable_retry_max: Duration,
    pub checkpoint_timeout: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            soundcloud_interval: Some(Duration::hours(6)),
            automatic_backup_interval: Some(Duration::minutes(15)),
            scheduled_backup_interval: Some(Duration::hours(24)),
            durable_idle_poll: Duration::minutes(1),
            unavailable_retry_initial: Duration::seconds(5),
            unavailable_retry_max: Duration::minutes(15),
            checkpoint_timeout: Duration::seconds(30),
        }
    }
}

impl SchedulerConfig {
    fn normalized(mut self) -> Self {
        self.soundcloud_interval = positive(self.soundcloud_interval);
        self.automatic_backup_interval = positive(self.automatic_backup_interval);
        self.scheduled_backup_interval = positive(self.scheduled_backup_interval);
        self.durable_idle_poll = self.durable_idle_poll.max(Duration::seconds(1));
        self.unavailable_retry_initial = self
            .unavailable_retry_initial
            .max(Duration::milliseconds(1));
        self.unavailable_retry_max = self
            .unavailable_retry_max
            .max(self.unavailable_retry_initial);
        self.checkpoint_timeout = self.checkpoint_timeout.max(Duration::milliseconds(1));
        self
    }
}

fn positive(value: Option<Duration>) -> Option<Duration> {
    value.filter(|duration| *duration > Duration::zero())
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("scheduler checkpoint failed: {0}")]
    Checkpoint(#[source] SchedulerTaskError),
    #[error("scheduler checkpoint did not finish within {0:?}")]
    CheckpointTimeout(Duration),
    #[error("scheduler task panicked or was aborted: {0}")]
    Join(#[from] tokio::task::JoinError),
}

struct HandleInner {
    wake: Notify,
    stopped: AtomicBool,
    shutdown_tx: watch::Sender<bool>,
    join: Mutex<Option<JoinHandle<Result<(), SchedulerError>>>>,
    last_error: StdMutex<Option<SchedulerTaskError>>,
}

/// Cloneable control plane. Clones may wake work or request cancellation; exactly one caller takes
/// the join result from [`SchedulerHandle::shutdown`]. Repeated shutdown calls are safe.
#[derive(Clone)]
pub struct SchedulerHandle {
    inner: Arc<HandleInner>,
}

impl SchedulerHandle {
    /// Wake the durable worker immediately (for example after a new backup job is committed).
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    /// Request cancellation without waiting for checkpoint completion.
    pub fn cancel(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        let _ = self.inner.shutdown_tx.send(true);
        self.inner.wake.notify_waiters();
    }

    pub fn last_error(&self) -> Option<SchedulerTaskError> {
        self.inner
            .last_error
            .lock()
            .expect("scheduler error lock poisoned")
            .clone()
    }

    /// Cancel, wait for the worker to stop, and wait for its bounded checkpoint flush.
    pub async fn shutdown(&self) -> Result<(), SchedulerError> {
        self.cancel();
        let join = self.inner.join.lock().await.take();
        match join {
            Some(join) => join.await?,
            None => Ok(()),
        }
    }
}

/// Start a scheduler with production time primitives.
pub fn start_scheduler<T>(tasks: Arc<T>, config: SchedulerConfig) -> SchedulerHandle
where
    T: SchedulerTasks + ?Sized,
{
    start_scheduler_with(tasks, Arc::new(SystemClock), Arc::new(TokioSleeper), config)
}

/// Start a scheduler with injectable time primitives.
pub fn start_scheduler_with<T, C, S>(
    tasks: Arc<T>,
    clock: Arc<C>,
    sleeper: Arc<S>,
    config: SchedulerConfig,
) -> SchedulerHandle
where
    T: SchedulerTasks + ?Sized,
    C: SchedulerClock + ?Sized,
    S: SchedulerSleeper + ?Sized,
{
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let inner = Arc::new(HandleInner {
        wake: Notify::new(),
        stopped: AtomicBool::new(false),
        shutdown_tx,
        join: Mutex::new(None),
        last_error: StdMutex::new(None),
    });
    let worker_inner = inner.clone();
    let join = tokio::spawn(run_scheduler(
        tasks,
        clock,
        sleeper,
        config.normalized(),
        worker_inner,
        shutdown_rx,
    ));
    *inner
        .join
        .try_lock()
        .expect("new scheduler join lock is uncontended") = Some(join);
    SchedulerHandle { inner }
}

async fn run_scheduler<T, C, S>(
    tasks: Arc<T>,
    clock: Arc<C>,
    sleeper: Arc<S>,
    config: SchedulerConfig,
    inner: Arc<HandleInner>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), SchedulerError>
where
    T: SchedulerTasks + ?Sized,
    C: SchedulerClock + ?Sized,
    S: SchedulerSleeper + ?Sized,
{
    let mut schedule = Schedule::new(clock.now(), &config);
    loop {
        if inner.stopped.load(Ordering::Acquire) || *shutdown_rx.borrow() {
            break;
        }

        let now = clock.now();
        if let Some(task) = schedule.take_due(now) {
            let run = tasks.run(task, now);
            let result = tokio::select! {
                biased;
                changed = shutdown_rx.changed() => {
                    let _ = changed;
                    break;
                }
                result = run => result,
            };
            if let Err(error) = &result {
                *inner
                    .last_error
                    .lock()
                    .expect("scheduler error lock poisoned") = Some(error.clone());
            }
            schedule.complete(task, now, result, &config);
            continue;
        }

        let deadline = schedule.next_deadline();
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => {
                let _ = changed;
                break;
            }
            _ = inner.wake.notified() => schedule.wake_durable(clock.now()),
            _ = sleeper.sleep_until(deadline) => {}
        }
    }

    let now = clock.now();
    let timeout_at = now + config.checkpoint_timeout;
    tokio::select! {
        result = tasks.checkpoint(now) => result.map_err(SchedulerError::Checkpoint),
        _ = sleeper.sleep_until(timeout_at) => Err(SchedulerError::CheckpointTimeout(config.checkpoint_timeout)),
    }
}

#[derive(Debug, Clone)]
struct Schedule {
    due: BTreeMap<ScheduledTask, DateTime<Utc>>,
    unavailable_attempts: BTreeMap<ScheduledTask, u32>,
}

impl Schedule {
    fn new(now: DateTime<Utc>, config: &SchedulerConfig) -> Self {
        let mut due = BTreeMap::new();
        // Startup reclaim and startup source sync are intentionally immediate. Durable planning
        // and idempotency make repeated process starts safe.
        due.insert(ScheduledTask::DurableJobs, now);
        if config.soundcloud_interval.is_some() {
            due.insert(ScheduledTask::SoundCloudSync, now);
        }
        if config.automatic_backup_interval.is_some() {
            due.insert(ScheduledTask::AutomaticBackup, now);
        }
        if config.scheduled_backup_interval.is_some() {
            due.insert(ScheduledTask::ScheduledBackup, now);
        }
        due.insert(ScheduledTask::DailyVerification, now);
        due.insert(ScheduledTask::DailySnapshot, now);
        due.insert(
            ScheduledTask::MonthlyInventory,
            next_month_boundary(now, true),
        );
        Self {
            due,
            unavailable_attempts: BTreeMap::new(),
        }
    }

    fn take_due(&mut self, now: DateTime<Utc>) -> Option<ScheduledTask> {
        self.due
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .min_by_key(|(task, deadline)| (**deadline, **task))
            .map(|(task, _)| *task)
            .inspect(|task| {
                self.due.remove(task);
            })
    }

    fn next_deadline(&self) -> DateTime<Utc> {
        self.due
            .values()
            .copied()
            .min()
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    }

    fn wake_durable(&mut self, now: DateTime<Utc>) {
        self.due.insert(ScheduledTask::DurableJobs, now);
    }

    fn complete(
        &mut self,
        task: ScheduledTask,
        now: DateTime<Utc>,
        result: Result<TaskOutcome, SchedulerTaskError>,
        config: &SchedulerConfig,
    ) {
        match result {
            Ok(outcome) => {
                self.unavailable_attempts.remove(&task);
                let default_due = match task {
                    ScheduledTask::DurableJobs if outcome.made_progress => now,
                    ScheduledTask::DurableJobs => now + config.durable_idle_poll,
                    ScheduledTask::SoundCloudSync => {
                        now + config
                            .soundcloud_interval
                            .expect("disabled tasks are not scheduled")
                    }
                    ScheduledTask::AutomaticBackup => {
                        now + config
                            .automatic_backup_interval
                            .expect("disabled tasks are not scheduled")
                    }
                    ScheduledTask::ScheduledBackup => {
                        now + config
                            .scheduled_backup_interval
                            .expect("disabled tasks are not scheduled")
                    }
                    ScheduledTask::DailyVerification | ScheduledTask::DailySnapshot => {
                        next_day_boundary(now)
                    }
                    ScheduledTask::MonthlyInventory => next_month_boundary(now, false),
                };
                self.due
                    .insert(task, outcome.next_due.unwrap_or(default_due));
                // Newly planned backup/verification/inventory work should be claimable now.
                if task != ScheduledTask::DurableJobs && outcome.made_progress {
                    self.wake_durable(now);
                }
            }
            Err(error) => {
                let delay = if error.retryable() {
                    let attempt = self.unavailable_attempts.entry(task).or_default();
                    let multiplier = 1_i32.checked_shl((*attempt).min(30)).unwrap_or(i32::MAX);
                    *attempt = attempt.saturating_add(1);
                    (config.unavailable_retry_initial * multiplier)
                        .min(config.unavailable_retry_max)
                } else {
                    self.unavailable_attempts.remove(&task);
                    config.durable_idle_poll
                };
                self.due.insert(task, now + delay);
            }
        }
    }
}

fn next_day_boundary(now: DateTime<Utc>) -> DateTime<Utc> {
    let date = now.date_naive().succ_opt().expect("UTC date overflow");
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("valid midnight"))
}

fn next_month_boundary(now: DateTime<Utc>, include_current: bool) -> DateTime<Utc> {
    if include_current && now.day() == 1 {
        return now;
    }
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .expect("valid first day of month")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, day, hour, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn six_hour_cadence_and_calendar_cadences_are_deterministic() {
        let config = SchedulerConfig::default();
        let now = at(14, 10);
        let mut schedule = Schedule::new(now, &config);
        schedule.take_due(now); // durable jobs sorts first
        schedule.complete(
            ScheduledTask::SoundCloudSync,
            now,
            Ok(TaskOutcome::IDLE),
            &config,
        );
        schedule.complete(
            ScheduledTask::DailySnapshot,
            now,
            Ok(TaskOutcome::IDLE),
            &config,
        );
        assert_eq!(schedule.due[&ScheduledTask::SoundCloudSync], at(14, 16));
        assert_eq!(schedule.due[&ScheduledTask::DailySnapshot], at(15, 0));
        assert_eq!(
            schedule.due[&ScheduledTask::MonthlyInventory],
            Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).single().unwrap()
        );
    }

    #[test]
    fn restart_immediately_reclaims_and_triggers_source_sync() {
        let schedule = Schedule::new(at(14, 10), &SchedulerConfig::default());
        assert_eq!(schedule.due[&ScheduledTask::DurableJobs], at(14, 10));
        assert_eq!(schedule.due[&ScheduledTask::SoundCloudSync], at(14, 10));
    }

    #[test]
    fn wake_overrides_a_later_deferred_poll() {
        let config = SchedulerConfig::default();
        let now = at(14, 10);
        let mut schedule = Schedule::new(now, &config);
        schedule.complete(
            ScheduledTask::DurableJobs,
            now,
            Ok(TaskOutcome::deferred(at(14, 16))),
            &config,
        );
        schedule.wake_durable(at(14, 11));
        assert_eq!(schedule.due[&ScheduledTask::DurableJobs], at(14, 11));
    }

    #[test]
    fn unavailable_dependencies_retry_with_bounded_backoff() {
        let mut config = SchedulerConfig::default();
        config.unavailable_retry_initial = Duration::seconds(2);
        config.unavailable_retry_max = Duration::seconds(5);
        let now = at(14, 10);
        let mut schedule = Schedule::new(now, &config);
        for (offset, expected) in [(0, 2), (2, 4), (6, 5)] {
            let attempt_at = now + Duration::seconds(offset);
            schedule.complete(
                ScheduledTask::DurableJobs,
                attempt_at,
                Err(SchedulerTaskError::Unavailable("offline".into())),
                &config,
            );
            assert_eq!(
                schedule.due[&ScheduledTask::DurableJobs],
                attempt_at + Duration::seconds(expected)
            );
        }
    }

    struct PendingTasks {
        checkpointed: Arc<AtomicBool>,
    }

    impl SchedulerTasks for PendingTasks {
        fn run(
            &self,
            _task: ScheduledTask,
            _now: DateTime<Utc>,
        ) -> BoxFuture<'_, Result<TaskOutcome, SchedulerTaskError>> {
            Box::pin(std::future::pending())
        }

        fn checkpoint(&self, _now: DateTime<Utc>) -> BoxFuture<'_, Result<(), SchedulerTaskError>> {
            let checkpointed = self.checkpointed.clone();
            Box::pin(async move {
                checkpointed.store(true, Ordering::Release);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn cancellation_drops_inflight_work_and_waits_for_checkpoint() {
        let checkpointed = Arc::new(AtomicBool::new(false));
        let handle = start_scheduler(
            Arc::new(PendingTasks {
                checkpointed: checkpointed.clone(),
            }),
            SchedulerConfig::default(),
        );
        tokio::task::yield_now().await;
        tokio::time::timeout(StdDuration::from_secs(1), handle.shutdown())
            .await
            .expect("shutdown must be bounded")
            .unwrap();
        assert!(checkpointed.load(Ordering::Acquire));
    }
}
