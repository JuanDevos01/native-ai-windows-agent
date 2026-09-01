//! Cron service — custom async scheduler with job persistence.
//!
//! Port of nanobot's `cron/service.py`.
//!
//! Architecture:
//! - Jobs stored in `~/.metis/cron/jobs.json`
//! - Timer sleeps until the nearest `next_run_at_ms`, then fires due jobs
//! - Job execution invokes a callback (typically `agent.process_direct()`)
//! - Results optionally delivered to a channel via the message bus
//!
//! No APScheduler. Fully custom async timer using `tokio::time::sleep`.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use metis_core::bus::queue::MessageBus;
use metis_core::bus::types::OutboundMessage;

use crate::types::{
    compute_next_run_from, CronJob, CronSchedule, CronStore, JobStatus, ScheduleKind,
};

// ─────────────────────────────────────────────
// Job callback type
// ─────────────────────────────────────────────

/// Callback invoked when a job fires.
///
/// Receives the job reference and returns the agent's response text.
/// In the gateway, this typically wraps `agent.process_direct()`.
pub type OnJobFn =
    Arc<dyn Fn(CronJob) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>> + Send + Sync>;

// ─────────────────────────────────────────────
// CronService
// ─────────────────────────────────────────────

/// Cron scheduler — manages jobs, persistence, and timed execution.
pub struct CronService {
    /// Path to the jobs JSON file.
    store_path: PathBuf,
    /// In-memory job store (protected by mutex for async safety).
    store: Arc<Mutex<CronStore>>,
    /// Message bus for outbound delivery.
    bus: Arc<MessageBus>,
    /// Callback for job execution (agent.process_direct).
    on_job: Arc<Mutex<Option<OnJobFn>>>,
    /// Shutdown signal.
    shutdown: Arc<Notify>,
    /// Re-arm signal (when jobs are added/modified).
    rearm: Arc<Notify>,
    /// Modification time of the store as of our last read or write.
    ///
    /// The store is shared with every `metis cron ...` CLI invocation, which
    /// is a separate process. Without this, a long-running gateway kept the
    /// copy it loaded at startup and every save silently overwrote whatever
    /// the CLI had added in the meantime.
    last_seen_mtime: Arc<Mutex<Option<std::time::SystemTime>>>,
}

/// How long the scheduler may sleep before re-checking the store on disk.
///
/// The re-arm signal is in-process only, so a job added by the CLI cannot
/// wake this loop. Capping the sleep bounds how long such a job stays
/// invisible.
const STORE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

impl CronService {
    /// Create a new cron service.
    ///
    /// If `store_path` is `None`, defaults to `~/.metis/cron/jobs.json`.
    pub fn new(bus: Arc<MessageBus>, store_path: Option<PathBuf>) -> Self {
        let path = store_path.unwrap_or_else(|| {
            let data_dir = metis_core::utils::get_data_path();
            data_dir.join("cron").join("jobs.json")
        });

        Self {
            store_path: path,
            store: Arc::new(Mutex::new(CronStore::new())),
            bus,
            on_job: Arc::new(Mutex::new(None)),
            shutdown: Arc::new(Notify::new()),
            rearm: Arc::new(Notify::new()),
            last_seen_mtime: Arc::new(Mutex::new(None)),
        }
    }

    /// Modification time of the store file, if it exists.
    async fn store_mtime(&self) -> Option<std::time::SystemTime> {
        tokio::fs::metadata(&self.store_path)
            .await
            .ok()
            .and_then(|m| m.modified().ok())
    }

    /// True when the file changed since we last read or wrote it.
    async fn changed_on_disk(&self) -> bool {
        let current = self.store_mtime().await;
        let seen = *self.last_seen_mtime.lock().await;
        match (current, seen) {
            (Some(c), Some(s)) => c != s,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Re-read the store when another process has changed it.
    ///
    /// Job definitions AND run state both live in the file, so adopting the
    /// on-disk copy wholesale is correct: whoever wrote it last had loaded
    /// our state first.
    pub async fn reload_if_changed(&self) -> bool {
        if !self.changed_on_disk().await {
            return false;
        }
        match self.load().await {
            Ok(()) => {
                debug!("cron store changed on disk, reloaded");
                true
            }
            Err(e) => {
                warn!(error = %e, "cron store changed on disk but could not be re-read");
                false
            }
        }
    }

    /// Set the on-job callback.
    pub async fn set_on_job(&self, callback: OnJobFn) {
        let mut on_job = self.on_job.lock().await;
        *on_job = Some(callback);
    }

    // ─────────────────────────────────────────
    // Persistence
    // ─────────────────────────────────────────

    /// Load the store from disk.
    pub async fn load(&self) -> anyhow::Result<()> {
        if !self.store_path.exists() {
            debug!(path = %self.store_path.display(), "no cron store file, starting empty");
            return Ok(());
        }

        // Read the mtime BEFORE the contents: if a writer lands between the
        // two, we record an older stamp and re-read next tick, rather than
        // recording a newer one and never noticing the change.
        let mtime = self.store_mtime().await;
        let data = tokio::fs::read_to_string(&self.store_path).await?;
        let loaded: CronStore = serde_json::from_str(&data)
            .map_err(|e| anyhow::anyhow!("failed to parse cron store: {}", e))?;

        let mut store = self.store.lock().await;
        *store = loaded;
        *self.last_seen_mtime.lock().await = mtime;
        info!(
            path = %self.store_path.display(),
            jobs = store.jobs.len(),
            "loaded cron store"
        );
        Ok(())
    }

    /// Save the store to disk.
    pub async fn save(&self) -> anyhow::Result<()> {
        // Ensure parent directory exists
        if let Some(parent) = self.store_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut store = self.store.lock().await;

        // Another process may have added a job since we last read the file.
        // Writing our copy verbatim would delete it — which is exactly what
        // happened to jobs added with `metis cron add` while the gateway was
        // running: the add succeeded, then the next scheduled run wrote the
        // gateway's stale copy back over it.
        if self.changed_on_disk().await {
            if let Ok(data) = tokio::fs::read_to_string(&self.store_path).await {
                if let Ok(disk) = serde_json::from_str::<CronStore>(&data) {
                    for job in disk.jobs {
                        if !store.jobs.iter().any(|j| j.id == job.id) {
                            info!(id = %job.id, name = %job.name, "adopting job added by another process");
                            store.jobs.push(job);
                        }
                    }
                }
            }
        }

        let json = serde_json::to_string_pretty(&*store)?;
        // Write to a temp file and rename, so a concurrent reader never sees
        // a half-written store.
        let tmp = self.store_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &self.store_path).await?;
        *self.last_seen_mtime.lock().await = self.store_mtime().await;
        debug!(path = %self.store_path.display(), "saved cron store");
        Ok(())
    }

    // ─────────────────────────────────────────
    // Job management
    // ─────────────────────────────────────────

    /// Add a job. Computes next run time and saves.
    pub async fn add_job(&self, mut job: CronJob) -> anyhow::Result<String> {
        // Compute initial next_run
        let now_ms = Utc::now().timestamp_millis();
        job.state.next_run_at_ms = compute_next_run_from(&job.schedule, now_ms);

        let id = job.id.clone();
        {
            let mut store = self.store.lock().await;
            store.add(job);
        }
        self.save().await?;
        self.rearm.notify_one();
        info!(id = %id, "added cron job");
        Ok(id)
    }

    /// Remove a job by ID.
    pub async fn remove_job(&self, id: &str) -> anyhow::Result<bool> {
        let removed = {
            let mut store = self.store.lock().await;
            store.remove(id)
        };
        if removed {
            self.save().await?;
            self.rearm.notify_one();
            info!(id = %id, "removed cron job");
        }
        Ok(removed)
    }

    /// Enable or disable a job.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> anyhow::Result<bool> {
        let found = {
            let mut store = self.store.lock().await;
            if let Some(job) = store.find_mut(id) {
                job.enabled = enabled;
                job.updated_at_ms = Utc::now().timestamp_millis();
                if enabled {
                    let now = Utc::now().timestamp_millis();
                    job.state.next_run_at_ms = compute_next_run_from(&job.schedule, now);
                }
                true
            } else {
                false
            }
        };
        if found {
            self.save().await?;
            self.rearm.notify_one();
        }
        Ok(found)
    }

    /// List all jobs (snapshot).
    pub async fn list_jobs(&self) -> Vec<CronJob> {
        let store = self.store.lock().await;
        store.jobs.clone()
    }

    /// Get a single job by ID.
    pub async fn get_job(&self, id: &str) -> Option<CronJob> {
        let store = self.store.lock().await;
        store.find(id).cloned()
    }

    // ─────────────────────────────────────────
    // Timer loop
    // ─────────────────────────────────────────

    /// Start the scheduler loop.
    ///
    /// Loads the store, then enters a loop:
    /// 1. Find nearest `next_run_at_ms`
    /// 2. Sleep until that time (or shutdown/rearm signal)
    /// 3. Execute all due jobs
    /// 4. Recompute and repeat
    pub async fn start(&self) -> anyhow::Result<()> {
        // Load persisted jobs
        if let Err(e) = self.load().await {
            warn!(error = %e, "failed to load cron store, starting empty");
        }

        info!("cron service started");

        loop {
            // Adopt anything the CLI changed while we were asleep.
            self.reload_if_changed().await;

            // Find how long to sleep
            let sleep_ms = {
                let store = self.store.lock().await;
                Self::next_wake_ms(&store)
            };

            let sleep_duration = if let Some(ms) = sleep_ms {
                let delay = (ms - Utc::now().timestamp_millis()).max(0) as u64;
                std::time::Duration::from_millis(delay)
            } else {
                // No scheduled jobs — wait for a rearm or an external change.
                std::time::Duration::from_secs(3600)
            }
            // Never sleep past the poll interval: a job the CLI adds cannot
            // signal this process, so the only way to notice it is to look.
            .min(STORE_POLL_INTERVAL);

            debug!(sleep_ms = sleep_duration.as_millis() as u64, "cron timer armed");

            tokio::select! {
                _ = tokio::time::sleep(sleep_duration) => {
                    // Timer fired — execute due jobs
                    self.execute_due_jobs().await;
                }
                _ = self.rearm.notified() => {
                    debug!("cron timer re-armed (job added/modified)");
                    // Loop back to recalculate sleep
                }
                _ = self.shutdown.notified() => {
                    info!("cron service shutting down");
                    return Ok(());
                }
            }
        }
    }

    /// Stop the scheduler.
    pub async fn stop(&self) {
        info!("stopping cron service");
        self.shutdown.notify_waiters();
    }

    /// Find the nearest next_run_at_ms across all enabled jobs.
    fn next_wake_ms(store: &CronStore) -> Option<i64> {
        store
            .jobs
            .iter()
            .filter(|j| j.enabled)
            .filter_map(|j| j.state.next_run_at_ms)
            .min()
    }

    /// Next run time after a completed execution.
    ///
    /// For interval (`every`) schedules this anchors to the time the run was
    /// DUE rather than the time it finished. Anchoring to completion made
    /// every run drift later by however long the previous one took — an
    /// hourly job that takes 3 minutes slips 3 minutes further each hour.
    /// If the job overran one or more whole intervals, skip ahead so it does
    /// not immediately re-fire for each missed slot.
    fn next_run_after_execution(
        schedule: &CronSchedule,
        scheduled_ms: Option<i64>,
        now_ms: i64,
    ) -> Option<i64> {
        if schedule.kind != ScheduleKind::Every {
            // Cron/At schedules are absolute — the next occurrence after now
            // is already correct and cannot drift.
            return compute_next_run_from(schedule, now_ms);
        }
        let interval = schedule.every_ms.unwrap_or(60_000).max(1);
        let anchor = scheduled_ms.unwrap_or(now_ms);
        let mut next = anchor.saturating_add(interval);
        if next <= now_ms {
            let missed = (now_ms - next) / interval + 1;
            next = next.saturating_add(missed.saturating_mul(interval));
        }
        Some(next)
    }

    /// Execute all due jobs.
    ///
    /// Runs them concurrently: they are independent, and executing them in a
    /// blocking sequence meant a long job (browser + LLM) delayed every other
    /// job that was due at the same moment.
    async fn execute_due_jobs(&self) {
        // Collect due job IDs (avoid holding lock during execution)
        let due_ids: Vec<String> = {
            let store = self.store.lock().await;
            store
                .due_jobs()
                .iter()
                .map(|j| j.id.clone())
                .collect()
        };

        if due_ids.is_empty() {
            return;
        }

        debug!(count = due_ids.len(), "executing due cron jobs");

        futures::future::join_all(due_ids.iter().map(|id| self.execute_job(id, None))).await;
    }

    /// Execute a single job by ID.
    ///
    /// If `manual_result` is `Some`, the gateway/agent callback is skipped and that outcome is
    /// persisted instead (used by `Metis cron run`).
    pub async fn execute_job(&self, id: &str, manual_result: Option<anyhow::Result<String>>) {
        // Get a snapshot of the job
        let job = {
            let store = self.store.lock().await;
            store.find(id).cloned()
        };

        let job = match job {
            Some(j) => j,
            None => {
                warn!(id = %id, "cron job not found for execution");
                return;
            }
        };

        // The time this run was DUE, captured before anything can overwrite
        // it — used to schedule the next run without drift.
        let scheduled_ms = job.state.next_run_at_ms;
        let started_ms = Utc::now().timestamp_millis();

        info!(id = %job.id, name = %job.name, "executing cron job");

        // Invoke callback unless this is a manual CLI run with a precomputed result.
        let result = match manual_result {
            Some(r) => Some(r),
            None => {
                // Clone the callback and RELEASE the lock before awaiting it.
                // Holding this mutex across the await serialized every cron
                // job in the process: a slow job (browser + LLM) blocked
                // unrelated jobs that were already due, so a task scheduled
                // for 06:00 could be delivered an hour late.
                let callback = { self.on_job.lock().await.clone() };
                if let Some(callback) = callback {
                    Some(callback(job.clone()).await)
                } else {
                    warn!(id = %id, "no on_job callback set, skipping execution");
                    None
                }
            }
        };

        // Update job state
        let now_ms = Utc::now().timestamp_millis();
        let duration_ms = now_ms - started_ms;
        // Lateness and duration were previously invisible: `last_run_at_ms`
        // recorded COMPLETION, so a job that started on time but ran for an
        // hour looked identical to one that started an hour late.
        info!(
            id = %job.id,
            name = %job.name,
            duration_ms,
            late_by_ms = scheduled_ms.map(|s| started_ms - s).unwrap_or(0),
            "cron job finished"
        );
        let mut should_delete = false;

        {
            let mut store = self.store.lock().await;
            if let Some(j) = store.find_mut(id) {
                j.state.last_run_at_ms = Some(now_ms);

                match &result {
                    Some(Ok(response)) => {
                        j.state.last_status = Some(JobStatus::Ok);
                        j.state.last_error = None;

                        // Deliver response to channel if configured (single place — gateway callback does not publish).
                        if j.payload.deliver {
                            if let Some(to) = j.payload.to.clone() {
                                let channel_name = j
                                    .payload
                                    .channel
                                    .clone()
                                    .unwrap_or_else(|| "telegram".to_string());
                                let outbound = OutboundMessage {
                                    channel: channel_name,
                                    chat_id: to,
                                    content: response.clone(),
                                    reply_to: None,
                                    media: Vec::new(),
                                    metadata: std::collections::HashMap::new(),
                                };
                                if let Err(e) = self.bus.publish_outbound(outbound).await {
                                    error!(error = %e, "failed to deliver cron response");
                                }
                            } else {
                                warn!(
                                    id = %id,
                                    name = %j.name,
                                    "cron job has deliver=true but no recipient (--to); skipping delivery"
                                );
                            }
                        }
                    }
                    Some(Err(e)) => {
                        j.state.last_status = Some(JobStatus::Error);
                        j.state.last_error = Some(e.to_string());
                        error!(
                            id = %id,
                            name = %j.name,
                            error = %e,
                            "cron job failed"
                        );
                    }
                    None => {
                        j.state.last_status = Some(JobStatus::Skipped);
                    }
                }

                // Compute next run
                if j.schedule.kind == ScheduleKind::At && j.delete_after_run {
                    should_delete = true;
                } else if j.schedule.kind == ScheduleKind::At {
                    j.enabled = false;
                    j.state.next_run_at_ms = None;
                } else {
                    j.state.next_run_at_ms =
                        Self::next_run_after_execution(&j.schedule, scheduled_ms, now_ms);
                }

                j.updated_at_ms = now_ms;
            }

            // Delete one-shot jobs
            if should_delete {
                store.remove(id);
            }
        }

        // Save
        if let Err(e) = self.save().await {
            error!(error = %e, "failed to save cron store after job execution");
        }
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CronPayload, CronSchedule};
    use tempfile::TempDir;

    fn make_bus() -> Arc<MessageBus> {
        Arc::new(MessageBus::new(10))
    }

    fn make_service(dir: &TempDir) -> CronService {
        let path = dir.path().join("jobs.json");
        CronService::new(make_bus(), Some(path))
    }

    #[tokio::test]
    async fn test_add_and_list() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let job = CronJob::new("test", CronSchedule::every(10_000), CronPayload::default());
        let id = svc.add_job(job).await.unwrap();

        let jobs = svc.list_jobs().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].name, "test");
    }

    #[tokio::test]
    async fn test_remove_job() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let job = CronJob::new("test", CronSchedule::every(10_000), CronPayload::default());
        let id = svc.add_job(job).await.unwrap();

        assert!(svc.remove_job(&id).await.unwrap());
        assert!(svc.list_jobs().await.is_empty());
    }

    #[tokio::test]
    async fn test_remove_nonexistent() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);
        assert!(!svc.remove_job("xyz").await.unwrap());
    }

    #[tokio::test]
    async fn test_set_enabled() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let job = CronJob::new("test", CronSchedule::every(10_000), CronPayload::default());
        let id = svc.add_job(job).await.unwrap();

        svc.set_enabled(&id, false).await.unwrap();
        let jobs = svc.list_jobs().await;
        assert!(!jobs[0].enabled);

        svc.set_enabled(&id, true).await.unwrap();
        let jobs = svc.list_jobs().await;
        assert!(jobs[0].enabled);
    }

    #[tokio::test]
    async fn test_set_enabled_nonexistent() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);
        assert!(!svc.set_enabled("xyz", true).await.unwrap());
    }

    #[tokio::test]
    async fn test_get_job() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let job = CronJob::new("test", CronSchedule::every(10_000), CronPayload::default());
        let id = svc.add_job(job).await.unwrap();

        assert!(svc.get_job(&id).await.is_some());
        assert!(svc.get_job("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_persistence_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("jobs.json");

        // Create and save
        {
            let svc = CronService::new(make_bus(), Some(path.clone()));
            let job = CronJob::new(
                "persistent",
                CronSchedule::every(5000),
                CronPayload {
                    message: "hello".into(),
                    deliver: true,
                    channel: Some("telegram".into()),
                    to: Some("12345".into()),
                },
            );
            svc.add_job(job).await.unwrap();
        }

        // Reload
        {
            let svc = CronService::new(make_bus(), Some(path));
            svc.load().await.unwrap();
            let jobs = svc.list_jobs().await;
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].name, "persistent");
            assert_eq!(jobs[0].payload.message, "hello");
        }
    }

    #[tokio::test]
    async fn test_load_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);
        // Should not error, just start empty
        svc.load().await.unwrap();
        assert!(svc.list_jobs().await.is_empty());
    }

    #[tokio::test]
    async fn test_execute_job_no_callback() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let mut job = CronJob::new("test", CronSchedule::every(10_000), CronPayload::default());
        job.state.next_run_at_ms = Some(0);
        let id = svc.add_job(job).await.unwrap();

        // Execute without callback — should mark as skipped
        svc.execute_job(&id, None).await;

        let j = svc.get_job(&id).await.unwrap();
        assert_eq!(j.state.last_status, Some(JobStatus::Skipped));
        assert!(j.state.last_run_at_ms.is_some());
    }

    #[tokio::test]
    async fn test_execute_job_with_callback() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let callback: OnJobFn = Arc::new(|_job| {
            Box::pin(async move { Ok("done".to_string()) })
        });
        svc.set_on_job(callback).await;

        let mut job = CronJob::new(
            "test",
            CronSchedule::every(10_000),
            CronPayload {
                message: "hello".into(),
                ..Default::default()
            },
        );
        job.state.next_run_at_ms = Some(0);
        let id = svc.add_job(job).await.unwrap();

        svc.execute_job(&id, None).await;

        let j = svc.get_job(&id).await.unwrap();
        assert_eq!(j.state.last_status, Some(JobStatus::Ok));
        assert!(j.state.next_run_at_ms.is_some());
    }

    #[tokio::test]
    async fn test_execute_job_error() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let callback: OnJobFn = Arc::new(|_job| {
            Box::pin(async move { Err(anyhow::anyhow!("boom")) })
        });
        svc.set_on_job(callback).await;

        let mut job = CronJob::new("failing", CronSchedule::every(10_000), CronPayload::default());
        job.state.next_run_at_ms = Some(0);
        let id = svc.add_job(job).await.unwrap();

        svc.execute_job(&id, None).await;

        let j = svc.get_job(&id).await.unwrap();
        assert_eq!(j.state.last_status, Some(JobStatus::Error));
        assert_eq!(j.state.last_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn test_execute_oneshot_deleted() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let callback: OnJobFn = Arc::new(|_| Box::pin(async { Ok("ok".into()) }));
        svc.set_on_job(callback).await;

        let mut job = CronJob::new("oneshot", CronSchedule::at(0), CronPayload::default());
        job.delete_after_run = true;
        job.state.next_run_at_ms = Some(0);
        let id = svc.add_job(job).await.unwrap();

        svc.execute_job(&id, None).await;

        // Job should be deleted
        assert!(svc.get_job(&id).await.is_none());
    }

    #[tokio::test]
    async fn test_execute_oneshot_disabled() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);

        let callback: OnJobFn = Arc::new(|_| Box::pin(async { Ok("ok".into()) }));
        svc.set_on_job(callback).await;

        let mut job = CronJob::new("oneshot", CronSchedule::at(0), CronPayload::default());
        job.delete_after_run = false;
        job.state.next_run_at_ms = Some(0);
        let id = svc.add_job(job).await.unwrap();

        svc.execute_job(&id, None).await;

        // Job should be disabled, not deleted
        let j = svc.get_job(&id).await.unwrap();
        assert!(!j.enabled);
        assert!(j.state.next_run_at_ms.is_none());
    }

    #[tokio::test]
    async fn test_execute_delivers_to_channel() {
        use tokio::time::{timeout, Duration};

        let dir = TempDir::new().unwrap();
        let bus = make_bus();
        let path = dir.path().join("jobs.json");
        let svc = CronService::new(bus.clone(), Some(path));

        let callback: OnJobFn = Arc::new(|_| Box::pin(async { Ok("response text".into()) }));
        svc.set_on_job(callback).await;

        let job = CronJob::new(
            "deliver",
            CronSchedule::every(10_000),
            CronPayload {
                message: "prompt".into(),
                deliver: true,
                channel: Some("telegram".into()),
                to: Some("user123".into()),
            },
        );
        let id = svc.add_job(job).await.unwrap();

        // Force job to be due NOW (add_job computes next_run in the future)
        {
            let mut store = svc.store.lock().await;
            if let Some(j) = store.find_mut(&id) {
                j.state.next_run_at_ms = Some(0);
            }
        }

        svc.execute_due_jobs().await;

        // Check outbound message was published (with timeout to avoid hanging)
        let outbound = timeout(Duration::from_secs(5), bus.consume_outbound())
            .await
            .expect("timed out waiting for outbound message");
        assert!(outbound.is_some());
        let msg = outbound.unwrap();
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.chat_id, "user123");
        assert_eq!(msg.content, "response text");
    }

    #[tokio::test]
    async fn test_next_wake_ms() {
        let mut store = CronStore::new();

        // Empty store → None
        assert!(CronService::next_wake_ms(&store).is_none());

        // One job
        let mut j1 = CronJob::new("j1", CronSchedule::every(10_000), CronPayload::default());
        j1.state.next_run_at_ms = Some(5000);
        store.add(j1);
        assert_eq!(CronService::next_wake_ms(&store), Some(5000));

        // Two jobs — picks earliest
        let mut j2 = CronJob::new("j2", CronSchedule::every(10_000), CronPayload::default());
        j2.state.next_run_at_ms = Some(3000);
        store.add(j2);
        assert_eq!(CronService::next_wake_ms(&store), Some(3000));
    }

    #[tokio::test]
    async fn test_next_wake_ms_ignores_disabled() {
        let mut store = CronStore::new();
        let mut j1 = CronJob::new("j1", CronSchedule::every(10_000), CronPayload::default());
        j1.enabled = false;
        j1.state.next_run_at_ms = Some(1000);
        store.add(j1);
        assert!(CronService::next_wake_ms(&store).is_none());
    }

    #[tokio::test]
    async fn test_stop() {
        let dir = TempDir::new().unwrap();
        let svc = make_service(&dir);
        // stop should not error even without start
        svc.stop().await;
    }

    #[test]
    fn test_every_schedule_does_not_drift_with_slow_jobs() {
        // Job due at t=1000, interval 1h, but the run took 5 minutes.
        let hour = 3_600_000i64;
        let schedule = CronSchedule::every(hour);
        let scheduled = 1_000_000i64;
        let finished = scheduled + 300_000; // +5 min

        let next = CronService::next_run_after_execution(&schedule, Some(scheduled), finished)
            .expect("interval schedule always has a next run");
        // Anchored to when it was DUE, not when it finished: exactly one
        // interval after the scheduled slot, so the job keeps its slot
        // instead of sliding 5 minutes later every hour.
        assert_eq!(next, scheduled + hour);
    }

    #[test]
    fn test_every_schedule_skips_missed_slots() {
        // A run that overran more than two whole intervals must not queue up
        // a burst of immediate catch-up executions.
        let hour = 3_600_000i64;
        let schedule = CronSchedule::every(hour);
        let scheduled = 1_000_000i64;
        let finished = scheduled + (hour * 2) + 60_000;

        let next = CronService::next_run_after_execution(&schedule, Some(scheduled), finished)
            .expect("interval schedule always has a next run");
        assert!(next > finished, "next run must be in the future");
        assert_eq!((next - scheduled) % hour, 0, "stays on the original cadence");
    }

    #[test]
    fn test_cron_schedule_next_is_absolute() {
        // Cron expressions are absolute wall-clock times and must not be
        // affected by how long the previous run took.
        let schedule = CronSchedule::cron("0 0 11 * * *");
        let now = chrono::Utc::now().timestamp_millis();
        let a = CronService::next_run_after_execution(&schedule, Some(now - 60_000), now);
        let b = compute_next_run_from(&schedule, now);
        assert_eq!(a, b);
    }

    // ── Regression: a job added by another process must survive ──────────
    //
    // A user asked the agent to schedule a news job. `metis cron add` ran in
    // its own process and succeeded. The gateway had loaded the store at
    // startup the previous evening and never re-read it, so the next
    // scheduled run wrote its stale copy back and the new job was gone. The
    // add really had worked, which is why the agent reported success — the
    // job was destroyed afterwards, silently.

    /// Build a job with a stable id derived from its name.
    fn make_job(name: &str) -> CronJob {
        let mut j = CronJob::new(name, CronSchedule::every(600_000), CronPayload::default());
        j.name = name.to_string();
        j
    }

    /// Write a store file directly, as a separate process would.
    fn write_store_file(path: &std::path::Path, jobs: Vec<CronJob>) {
        let store = CronStore { version: 1, jobs };
        std::fs::write(path, serde_json::to_string_pretty(&store).unwrap()).unwrap();
    }

    fn read_job_names(path: &std::path::Path) -> Vec<String> {
        let data = std::fs::read_to_string(path).unwrap();
        let store: CronStore = serde_json::from_str(&data).unwrap();
        store.jobs.into_iter().map(|j| j.name).collect()
    }

    #[tokio::test]
    async fn saving_does_not_delete_a_job_added_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let bus = Arc::new(MessageBus::new(10));

        // The gateway starts and loads one job.
        write_store_file(&path, vec![make_job("existing")]);
        let svc = CronService::new(bus, Some(path.clone()));
        svc.load().await.unwrap();

        // The CLI, a separate process, adds another job.
        let mut jobs = vec![make_job("existing"), make_job("added-by-cli")];
        jobs[1].id = "cli00001".to_string();
        write_store_file(&path, jobs);

        // The gateway finishes a run and saves.
        svc.save().await.unwrap();

        let names = read_job_names(&path);
        assert!(
            names.iter().any(|n| n == "added-by-cli"),
            "the CLI's job was destroyed by the gateway's save: {names:?}"
        );
        assert!(names.iter().any(|n| n == "existing"), "{names:?}");
    }

    #[tokio::test]
    async fn an_externally_added_job_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let bus = Arc::new(MessageBus::new(10));

        write_store_file(&path, vec![make_job("existing")]);
        let svc = CronService::new(bus, Some(path.clone()));
        svc.load().await.unwrap();
        assert_eq!(svc.list_jobs().await.len(), 1);

        // Someone runs `metis cron add`.
        let mut jobs = vec![make_job("existing"), make_job("added-by-cli")];
        jobs[1].id = "cli00001".to_string();
        // Filesystem mtime resolution is coarse; make the change unambiguous.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_store_file(&path, jobs);

        assert!(svc.reload_if_changed().await, "the change should be noticed");
        assert_eq!(
            svc.list_jobs().await.len(),
            2,
            "the scheduler must see the new job, or it will never run it"
        );
    }

    #[tokio::test]
    async fn an_unchanged_store_is_not_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let bus = Arc::new(MessageBus::new(10));

        write_store_file(&path, vec![make_job("existing")]);
        let svc = CronService::new(bus, Some(path.clone()));
        svc.load().await.unwrap();
        assert!(!svc.reload_if_changed().await, "nothing changed");

        // Our own save must not look like an external change either.
        svc.save().await.unwrap();
        assert!(!svc.reload_if_changed().await, "our own write is not external");
    }

    #[tokio::test]
    async fn the_store_is_never_left_half_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        let bus = Arc::new(MessageBus::new(10));

        let svc = CronService::new(bus, Some(path.clone()));
        svc.add_job(make_job("a")).await.unwrap();
        svc.add_job(make_job("b")).await.unwrap();

        // A reader at any point must get valid JSON, and no temp file is left.
        let data = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str::<CronStore>(&data).expect("store must always parse");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file should have been renamed away"
        );
    }
}
