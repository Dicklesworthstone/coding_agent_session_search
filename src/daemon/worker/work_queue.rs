//! Bounded embedding mailbox with cancellation attached to accepted jobs.
//!
//! Control messages run before pending submissions. A cancel marks only jobs
//! already admitted under the queue lock; a later retry gets fresh tokens.
//! Dequeue registers the active job under that same lock, closing the gap
//! between removing a submission and starting its first pass.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::{EmbeddingJobConfig, EmbeddingWorkerHandle, FastEmbedder, WorkerMessage};
use crate::franken_sync::CancellationHandle;

const MAX_PENDING_JOBS: usize = 32;
const MAX_PENDING_CANCELLATIONS: usize = 32;
const MAX_REQUEST_BYTES: usize = 16 * 1024;

fn model_key(model: &str) -> String {
    let model = model.trim();
    if model.eq_ignore_ascii_case("hash") || model.eq_ignore_ascii_case("fnv1a-384") {
        "hash".to_string()
    } else {
        FastEmbedder::canonical_name(model)
            .map(str::to_string)
            .unwrap_or_else(|| model.to_ascii_lowercase())
    }
}

struct Cancellation {
    db_path: String,
    models: Vec<(String, AtomicBool, CancellationHandle)>,
    preparation: CancellationHandle,
}

#[derive(Clone)]
pub(super) struct JobControl(Arc<Cancellation>);

impl JobControl {
    fn new(config: &EmbeddingJobConfig) -> Self {
        let models = if config.two_tier {
            vec![config.fast_pass_model(), config.quality_pass_model()]
        } else {
            vec![config.single_pass_model()]
        };
        Self(Arc::new(Cancellation {
            db_path: config.db_path.clone(),
            models: models
                .into_iter()
                .map(|model| {
                    (
                        model_key(&model),
                        AtomicBool::new(false),
                        CancellationHandle::default(),
                    )
                })
                .collect(),
            preparation: CancellationHandle::default(),
        }))
    }

    pub(super) fn is_cancelled(&self, model: &str) -> bool {
        let key = model_key(model);
        self.0
            .models
            .iter()
            .any(|(candidate, cancelled, _)| candidate == &key && cancelled.load(Ordering::SeqCst))
    }

    pub(super) fn all_cancelled(&self) -> bool {
        self.0
            .models
            .iter()
            .all(|(_, cancelled, _)| cancelled.load(Ordering::SeqCst))
    }

    fn any_cancelled(&self) -> bool {
        self.0
            .models
            .iter()
            .any(|(_, cancelled, _)| cancelled.load(Ordering::SeqCst))
    }

    fn cancel(&self, scope: &CancelScope) -> usize {
        if self.0.db_path != scope.db_path {
            return 0;
        }
        let selected = scope.model.as_deref().map(model_key);
        let mut changed = 0;
        for (model, cancelled, _) in &self.0.models {
            if selected.as_ref().is_none_or(|selected| selected == model)
                && !cancelled.swap(true, Ordering::SeqCst)
            {
                changed += 1;
            }
        }
        changed
    }

    fn cancel_all(&self) {
        for (_, cancelled, _) in &self.0.models {
            cancelled.store(true, Ordering::SeqCst);
        }
    }

    pub(super) fn preparation_cancellation(&self) -> CancellationHandle {
        self.0.preparation.clone()
    }

    pub(super) fn model_cancellation(&self, model: &str) -> Option<CancellationHandle> {
        let key = model_key(model);
        self.0
            .models
            .iter()
            .find(|(candidate, _, _)| candidate == &key)
            .map(|(_, _, cancellation)| cancellation.clone())
    }

    /// Engine relays invoke wakers, so callers must release the mailbox and
    /// worker ownership mutexes before propagating these already-latched flags.
    fn propagate_sql_cancellation(&self) {
        for (_, cancelled, cancellation) in &self.0.models {
            if cancelled.load(Ordering::SeqCst) {
                cancellation.cancel();
            }
        }
        // Initial archive preparation serves every tier. Cancelling only the
        // fast tier must not poison the snapshot the quality tier still needs.
        if self.all_cancelled() {
            self.0.preparation.cancel();
        }
    }
}

struct PendingJob {
    config: EmbeddingJobConfig,
    control: JobControl,
    retry_at: Option<Instant>,
}

struct CancelScope {
    db_path: String,
    model: Option<String>,
    retry_at: Option<Instant>,
}

impl CancelScope {
    fn covers(&self, other: &Self) -> bool {
        self.db_path == other.db_path
            && match (&self.model, &other.model) {
                (None, _) => true,
                (Some(left), Some(right)) => model_key(left) == model_key(right),
                (Some(_), None) => false,
            }
    }
}

struct State {
    pending: VecDeque<PendingJob>,
    cancellations: VecDeque<CancelScope>,
    active: Option<JobControl>,
    stopped: bool,
    receiver_alive: bool,
}

/// The wake channel carries no requests. Its one buffered notification is
/// sufficient because the bounded queues, inspected before waiting, own work.
#[derive(Clone)]
pub(super) struct Sender {
    state: Arc<Mutex<State>>,
    wake: SyncSender<()>,
}

pub(super) struct Receiver {
    state: Arc<Mutex<State>>,
    wake: mpsc::Receiver<()>,
}

/// Keeps the dequeued job visible to cancel until processing leaves its scope.
/// Dropping it never cancels or clears a different job's registration.
pub(super) struct JobPermit {
    state: Weak<Mutex<State>>,
    control: JobControl,
}

impl JobPermit {
    pub(super) fn control(&self) -> JobControl {
        self.control.clone()
    }
}

impl Drop for JobPermit {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade()
            && let Ok(mut state) = state.lock()
            && state
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.0, &self.control.0))
        {
            state.active = None;
        }
    }
}

pub(super) fn channel() -> (Sender, Receiver) {
    let state = Arc::new(Mutex::new(State {
        pending: VecDeque::new(),
        cancellations: VecDeque::new(),
        active: None,
        stopped: false,
        receiver_alive: true,
    }));
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);
    (
        Sender {
            state: Arc::clone(&state),
            wake: wake_tx,
        },
        Receiver {
            state,
            wake: wake_rx,
        },
    )
}

fn same_config(left: &EmbeddingJobConfig, right: &EmbeddingJobConfig) -> bool {
    left.db_path == right.db_path
        && left.index_path == right.index_path
        && left.two_tier == right.two_tier
        && left.fast_model == right.fast_model
        && left.quality_model == right.quality_model
}

fn validate_request(parts: &[&str]) -> Result<(), String> {
    let bytes = parts
        .iter()
        .fold(0usize, |total, part| total.saturating_add(part.len()));
    if bytes > MAX_REQUEST_BYTES || parts.iter().any(|part| part.contains('\0')) {
        return Err("embedding request exceeds its byte limit or contains a NUL byte".to_string());
    }
    Ok(())
}

impl Sender {
    /// Admission never waits for queue space. Exact duplicate pending jobs
    /// coalesce, but an active job may have one pending rerun for freshness.
    pub(super) fn send(&self, message: WorkerMessage) -> Result<(), String> {
        self.send_counted(message).map(|_| ())
    }

    /// Count newly signalled passes inside the admission lock. Repeated
    /// cancellation returns zero; the count is not a durable completion claim.
    fn send_counted(&self, message: WorkerMessage) -> Result<usize, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "embedding queue state lock poisoned".to_string())?;
        if !state.receiver_alive {
            return Err("embedding worker channel closed".to_string());
        }
        if state.stopped && !matches!(&message, WorkerMessage::Shutdown) {
            return Err("embedding worker is shutting down".to_string());
        }
        let mut cancelled_passes = 0;
        let mut sql_cancellations = Vec::new();
        match message {
            WorkerMessage::Submit(config) => {
                validate_request(&[
                    &config.db_path,
                    &config.index_path,
                    config.fast_model.as_deref().unwrap_or_default(),
                    config.quality_model.as_deref().unwrap_or_default(),
                ])?;
                if config.db_path.trim().is_empty() || config.index_path.trim().is_empty() {
                    return Err("embedding job requires database and index paths".to_string());
                }
                let duplicate = state.pending.iter().any(|pending| {
                    same_config(&pending.config, &config) && !pending.control.any_cancelled()
                });
                if !duplicate {
                    if state.pending.len() >= MAX_PENDING_JOBS {
                        return Err(
                            "embedding job queue is full; retry after pending work completes"
                                .to_string(),
                        );
                    }
                    let control = JobControl::new(&config);
                    state.pending.push_back(PendingJob {
                        config,
                        control,
                        retry_at: None,
                    });
                }
            }
            WorkerMessage::Cancel { db_path, model_id } => {
                validate_request(&[&db_path, model_id.as_deref().unwrap_or_default()])?;
                if db_path.trim().is_empty() {
                    return Err("embedding cancellation requires a database path".to_string());
                }
                let scope = CancelScope {
                    db_path,
                    model: model_id,
                    retry_at: None,
                };
                let duplicate = state
                    .cancellations
                    .iter()
                    .any(|pending| pending.covers(&scope));
                let retained = state
                    .cancellations
                    .iter()
                    .filter(|pending| !scope.covers(pending))
                    .count();
                // Check capacity before changing cancellation tokens: a refused
                // request must not partially cancel jobs behind the caller's back.
                if !duplicate && retained >= MAX_PENDING_CANCELLATIONS {
                    return Err("embedding cancellation queue is full; retry after pending controls complete".to_string());
                }
                if let Some(active) = &state.active {
                    let count = active.cancel(&scope);
                    cancelled_passes += count;
                    if count > 0 {
                        sql_cancellations.push(active.clone());
                    }
                }
                for pending in &state.pending {
                    let count = pending.control.cancel(&scope);
                    cancelled_passes += count;
                    if count > 0 {
                        sql_cancellations.push(pending.control.clone());
                    }
                }
                state
                    .pending
                    .retain(|pending| !pending.control.all_cancelled());
                if !duplicate {
                    state.cancellations.retain(|pending| !scope.covers(pending));
                    state.cancellations.push_back(scope);
                }
            }
            WorkerMessage::Shutdown => {
                state.stopped = true;
                if let Some(active) = &state.active {
                    active.cancel_all();
                    sql_cancellations.push(active.clone());
                }
                state.pending.clear();
                state.cancellations.clear();
            }
        }
        drop(state);
        for control in sql_cancellations {
            control.propagate_sql_cancellation();
        }
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(cancelled_passes),
            Err(TrySendError::Disconnected(())) => {
                Err("embedding worker channel closed".to_string())
            }
        }
    }
}

impl EmbeddingWorkerHandle {
    /// Return the exact number of queued/running passes newly signalled by
    /// this request. Durable job-row cleanup is queued before later jobs;
    /// callers must not report this receipt as completed database cleanup.
    pub fn cancel_with_count(
        &self,
        db_path: String,
        model_id: Option<String>,
    ) -> Result<usize, String> {
        self.sender
            .send_counted(WorkerMessage::Cancel { db_path, model_id })
    }
}

impl Receiver {
    /// Return accepted work to the tail without creating a new cancellation
    /// generation. The active slot already accounts for this job, so a full
    /// pending queue must not make a lock-contention retry lose accepted work.
    pub(super) fn defer_job(
        &self,
        config: EmbeddingJobConfig,
        permit: JobPermit,
        delay: Duration,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "embedding queue state lock poisoned".to_string())?;
        if !state
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.0, &permit.control.0))
        {
            return Err("deferred embedding job no longer owns its active slot".to_string());
        }
        state.active = None;
        if !state.stopped && !permit.control.all_cancelled() {
            state.pending.push_back(PendingJob {
                config,
                control: permit.control.clone(),
                retry_at: Some(Instant::now() + delay),
            });
        }
        // JobPermit::drop also takes state; never drop it under this lock.
        drop(state);
        drop(permit);
        Ok(())
    }

    /// A deferred cleanup remains ahead of newer submissions for its archive.
    /// Other archives can proceed while this one waits for a maintenance lock.
    pub(super) fn defer_cancel(
        &self,
        db_path: String,
        model: Option<String>,
        delay: Duration,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "embedding queue state lock poisoned".to_string())?;
        if state.stopped {
            return Ok(());
        }
        let scope = CancelScope {
            db_path,
            model,
            retry_at: Some(Instant::now() + delay),
        };
        if !state
            .cancellations
            .iter()
            .any(|pending| pending.covers(&scope))
        {
            state.cancellations.retain(|pending| !scope.covers(pending));
            state.cancellations.push_back(scope);
        }
        Ok(())
    }

    pub(super) fn recv(&self) -> Result<(WorkerMessage, Option<JobPermit>), String> {
        loop {
            let retry_delay =
                {
                    let mut state = self
                        .state
                        .lock()
                        .map_err(|_| "embedding queue state lock poisoned".to_string())?;
                    if state.stopped {
                        return Ok((WorkerMessage::Shutdown, None));
                    }
                    let now = Instant::now();
                    if let Some(position) = state
                        .cancellations
                        .iter()
                        .position(|scope| scope.retry_at.is_none_or(|retry_at| retry_at <= now))
                    {
                        let scope = state.cancellations.remove(position).ok_or_else(|| {
                            "selected embedding cancellation disappeared".to_string()
                        })?;
                        return Ok((
                            WorkerMessage::Cancel {
                                db_path: scope.db_path,
                                model_id: scope.model,
                            },
                            None,
                        ));
                    }
                    if state.active.is_some() {
                        return Err("previous embedding job permit is still active".to_string());
                    }
                    if let Some(position) = state.pending.iter().position(|pending| {
                        pending.retry_at.is_none_or(|retry_at| retry_at <= now)
                            && !state
                                .cancellations
                                .iter()
                                .any(|scope| scope.db_path == pending.config.db_path)
                    }) {
                        let pending = state.pending.remove(position).ok_or_else(|| {
                            "selected embedding submission disappeared".to_string()
                        })?;
                        state.active = Some(pending.control.clone());
                        let permit = JobPermit {
                            state: Arc::downgrade(&self.state),
                            control: pending.control,
                        };
                        return Ok((WorkerMessage::Submit(pending.config), Some(permit)));
                    }
                    state
                        .cancellations
                        .iter()
                        .filter_map(|scope| scope.retry_at)
                        .chain(
                            state
                                .pending
                                .iter()
                                .filter(|pending| {
                                    !state
                                        .cancellations
                                        .iter()
                                        .any(|scope| scope.db_path == pending.config.db_path)
                                })
                                .filter_map(|pending| pending.retry_at),
                        )
                        .min()
                        .map(|retry_at| retry_at.saturating_duration_since(now))
                };
            match retry_delay {
                Some(delay) => match self.wake.recv_timeout(delay) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    // Sender loss still drains admitted work. No sender can
                    // request shutdown now, so this bounded wait is sufficient.
                    Err(RecvTimeoutError::Disconnected) => std::thread::sleep(delay),
                },
                None => self
                    .wake
                    .recv()
                    .map_err(|_| "embedding worker channel closed".to_string())?,
            }
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let active = if let Ok(mut state) = self.state.lock() {
            state.receiver_alive = false;
            if let Some(active) = &state.active {
                active.cancel_all();
            }
            state.pending.clear();
            state.cancellations.clear();
            state.active.clone()
        } else {
            None
        };
        if let Some(active) = active {
            active.propagate_sql_cancellation();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: usize) -> EmbeddingJobConfig {
        EmbeddingJobConfig {
            db_path: format!("/archive/{id}.db"),
            index_path: format!("/index/{id}"),
            two_tier: true,
            fast_model: Some("hash".into()),
            quality_model: Some("minilm".into()),
        }
    }

    fn cancel(sender: &Sender, id: usize, model: Option<&str>) {
        sender
            .send(WorkerMessage::Cancel {
                db_path: job(id).db_path,
                model_id: model.map(str::to_string),
            })
            .unwrap();
    }

    #[test]
    fn admission_is_bounded_and_pending_duplicates_coalesce_even_when_full() {
        let (sender, receiver) = channel();
        for id in 0..MAX_PENDING_JOBS {
            sender.send(WorkerMessage::Submit(job(id))).unwrap();
        }
        sender.send(WorkerMessage::Submit(job(0))).unwrap();
        assert!(
            sender
                .send(WorkerMessage::Submit(job(MAX_PENDING_JOBS)))
                .is_err()
        );
        let (first, permit) = receiver.recv().unwrap();
        assert!(matches!(first, WorkerMessage::Submit(config) if config.db_path == job(0).db_path));
        // A rerun accepted while the old job is active must not disappear.
        sender.send(WorkerMessage::Submit(job(0))).unwrap();
        assert_eq!(sender.state.lock().unwrap().pending.len(), MAX_PENDING_JOBS);
        drop(permit);
        for id in 1..MAX_PENDING_JOBS {
            let (message, permit) = receiver.recv().unwrap();
            assert!(
                matches!(message, WorkerMessage::Submit(config) if config.db_path == job(id).db_path)
            );
            drop(permit);
        }
        let (last, permit) = receiver.recv().unwrap();
        assert!(matches!(last, WorkerMessage::Submit(config) if config.db_path == job(0).db_path));
        drop(permit);
    }

    #[test]
    fn deferred_jobs_keep_their_cancellation_generation_and_yield_to_other_archives() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        sender.send(WorkerMessage::Submit(job(2))).unwrap();
        let (_, first) = receiver.recv().unwrap();
        let first = first.unwrap();
        let control = first.control();
        receiver.defer_job(job(1), first, Duration::ZERO).unwrap();

        let (ready, permit) = receiver.recv().unwrap();
        assert!(matches!(ready, WorkerMessage::Submit(config) if config.db_path == job(2).db_path));
        drop(permit);
        cancel(&sender, 1, Some("minilm"));
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        assert!(control.is_cancelled("minilm"));
        assert!(!control.is_cancelled("hash"));
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Cancel { .. }
        ));
        let (_, resumed) = receiver.recv().unwrap();
        assert!(resumed.as_ref().unwrap().control().is_cancelled("minilm"));
        drop(resumed);
        let (_, fresh) = receiver.recv().unwrap();
        assert!(!fresh.as_ref().unwrap().control().any_cancelled());
        drop(fresh);
    }

    #[test]
    fn deferring_an_active_job_cannot_lose_it_when_pending_admission_is_full() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(100))).unwrap();
        let (_, permit) = receiver.recv().unwrap();
        for id in 0..MAX_PENDING_JOBS {
            sender.send(WorkerMessage::Submit(job(id))).unwrap();
        }
        receiver
            .defer_job(job(100), permit.unwrap(), Duration::ZERO)
            .unwrap();
        assert!(sender.send(WorkerMessage::Submit(job(200))).is_err());
        drop(sender);
        let mut delivered = Vec::new();
        while let Ok((message, permit)) = receiver.recv() {
            if let WorkerMessage::Submit(config) = message {
                delivered.push(config.db_path);
            }
            drop(permit);
        }
        assert_eq!(delivered.len(), MAX_PENDING_JOBS + 1);
        assert_eq!(delivered.last(), Some(&job(100).db_path));
        assert_eq!(
            delivered
                .iter()
                .filter(|path| **path == job(100).db_path)
                .count(),
            1
        );
    }

    #[test]
    fn deferred_cleanup_blocks_only_its_archive_and_shutdown_wakes_the_deadline() {
        let (sender, receiver) = channel();
        receiver
            .defer_cancel(job(1).db_path, None, Duration::from_secs(60))
            .unwrap();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        sender.send(WorkerMessage::Submit(job(2))).unwrap();
        let (ready, permit) = receiver.recv().unwrap();
        assert!(matches!(ready, WorkerMessage::Submit(config) if config.db_path == job(2).db_path));
        drop(permit);
        let (finished_tx, finished_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let shutdown = matches!(receiver.recv().unwrap().0, WorkerMessage::Shutdown);
            finished_tx.send(shutdown).unwrap();
        });
        sender.send(WorkerMessage::Shutdown).unwrap();
        assert!(finished_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        waiter.join().unwrap();
    }

    #[test]
    fn ready_cleanup_retried_before_a_later_submission_for_its_archive() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        receiver
            .defer_cancel(job(1).db_path, None, Duration::ZERO)
            .unwrap();
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Cancel { .. }
        ));
        let (ready, permit) = receiver.recv().unwrap();
        assert!(matches!(ready, WorkerMessage::Submit(config) if config.db_path == job(1).db_path));
        drop(permit);
    }

    #[test]
    fn cancel_removes_queued_work_before_it_can_open_an_archive() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        sender.send(WorkerMessage::Submit(job(2))).unwrap();
        cancel(&sender, 1, None);
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Cancel { .. }
        ));
        let (message, permit) = receiver.recv().unwrap();
        assert!(
            matches!(message, WorkerMessage::Submit(config) if config.db_path == job(2).db_path)
        );
        drop(permit);
        assert!(sender.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn cancellation_is_scoped_to_a_job_generation_and_model_including_dequeue_gap() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        let (_, old_permit) = receiver.recv().unwrap();
        let old = old_permit.as_ref().unwrap().control();
        let preparation = old.preparation_cancellation();
        let quality_sql = old.model_cancellation("minilm").unwrap();
        let fast_sql = old.model_cancellation("hash").unwrap();
        cancel(&sender, 2, None);
        assert!(!old.any_cancelled());
        assert!(!preparation.is_cancelled());
        assert!(!quality_sql.is_cancelled());
        cancel(&sender, 1, Some("all-minilm-l6-v2"));
        assert!(old.is_cancelled("minilm-384"));
        assert!(!old.is_cancelled("hash"));
        assert!(quality_sql.is_cancelled());
        assert!(!fast_sql.is_cancelled());
        assert!(
            !preparation.is_cancelled(),
            "the uncancelled tier still needs initial archive preparation"
        );
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        drop(old_permit);
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Cancel { .. }
        ));
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Cancel { .. }
        ));
        let (_, new_permit) = receiver.recv().unwrap();
        let new = new_permit.as_ref().unwrap().control();
        assert!(
            !new.any_cancelled(),
            "a retry admitted after cancellation is a new job"
        );
        assert!(!new.preparation_cancellation().is_cancelled());
        assert!(!new.model_cancellation("minilm").unwrap().is_cancelled());
        assert!(old.is_cancelled("minilm"));
        drop(new_permit);
    }

    #[test]
    fn cancelling_one_queued_tier_preserves_the_other_and_does_not_absorb_a_retry() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        cancel(&sender, 1, Some("fnv1a-384"));
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Cancel { .. }
        ));
        let (_, first) = receiver.recv().unwrap();
        let first_control = first.as_ref().unwrap().control();
        assert!(first_control.is_cancelled("hash"));
        assert!(!first_control.is_cancelled("minilm"));
        drop(first);
        let (_, retry) = receiver.recv().unwrap();
        assert!(!retry.as_ref().unwrap().control().any_cancelled());
        drop(retry);
    }

    #[test]
    fn shutdown_preempts_full_queues_and_permanently_refuses_submissions() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(100))).unwrap();
        let (_, permit) = receiver.recv().unwrap();
        let active = permit.as_ref().unwrap().control();
        for id in 0..MAX_PENDING_JOBS {
            sender.send(WorkerMessage::Submit(job(id))).unwrap();
        }
        for id in 200..200 + MAX_PENDING_CANCELLATIONS {
            cancel(&sender, id, None);
        }
        sender.send(WorkerMessage::Shutdown).unwrap();
        assert!(active.all_cancelled());
        assert!(active.preparation_cancellation().is_cancelled());
        assert!(active.model_cancellation("hash").unwrap().is_cancelled());
        assert!(active.model_cancellation("minilm").unwrap().is_cancelled());
        assert!(matches!(
            receiver.recv().unwrap().0,
            WorkerMessage::Shutdown
        ));
        assert!(sender.send(WorkerMessage::Submit(job(999))).is_err());
        assert!(sender.state.lock().unwrap().pending.is_empty());
        assert!(sender.state.lock().unwrap().cancellations.is_empty());
        drop(permit);
    }

    #[test]
    fn refused_control_has_no_partial_cancellation_and_broad_controls_coalesce() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(100))).unwrap();
        let (_, permit) = receiver.recv().unwrap();
        let active = permit.as_ref().unwrap().control();
        for id in 0..MAX_PENDING_CANCELLATIONS {
            cancel(&sender, id, Some("minilm"));
        }
        let refused = sender.send(WorkerMessage::Cancel {
            db_path: job(100).db_path,
            model_id: None,
        });
        assert!(refused.is_err());
        assert!(!active.any_cancelled());
        cancel(&sender, 0, None);
        cancel(&sender, 0, Some("hash"));
        assert_eq!(
            sender.state.lock().unwrap().cancellations.len(),
            MAX_PENDING_CANCELLATIONS
        );
        drop(permit);
    }

    #[test]
    fn request_byte_limit_and_nul_rejection_do_not_consume_slots() {
        let (sender, _receiver) = channel();
        let mut oversized = job(1);
        oversized.index_path = "x".repeat(MAX_REQUEST_BYTES + 1);
        assert!(sender.send(WorkerMessage::Submit(oversized)).is_err());
        let mut malformed = job(2);
        malformed.db_path.push('\0');
        assert!(sender.send(WorkerMessage::Submit(malformed)).is_err());
        assert!(sender.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn receiver_loss_cancels_active_work_and_sender_loss_drains_accepted_work() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        let (_, permit) = receiver.recv().unwrap();
        let active = permit.as_ref().unwrap().control();
        drop(receiver);
        assert!(active.all_cancelled());
        assert!(active.preparation_cancellation().is_cancelled());
        assert!(active.model_cancellation("hash").unwrap().is_cancelled());
        assert!(sender.send(WorkerMessage::Submit(job(2))).is_err());
        drop(permit);

        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(3))).unwrap();
        drop(sender);
        let (message, permit) = receiver.recv().unwrap();
        assert!(matches!(message, WorkerMessage::Submit(_)));
        drop(permit);
        assert!(receiver.recv().is_err());
    }

    #[test]
    fn concurrent_senders_cannot_over_admit() {
        let (sender, _receiver) = channel();
        let barrier = Arc::new(std::sync::Barrier::new(MAX_PENDING_JOBS * 2));
        let threads = (0..MAX_PENDING_JOBS * 2)
            .map(|id| {
                let sender = sender.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    sender.send(WorkerMessage::Submit(job(id))).is_ok()
                })
            })
            .collect::<Vec<_>>();
        let admitted = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum::<usize>();
        assert_eq!(admitted, MAX_PENDING_JOBS);
        assert_eq!(sender.state.lock().unwrap().pending.len(), MAX_PENDING_JOBS);
    }

    #[test]
    fn cancellation_receipts_count_only_newly_signalled_passes() {
        let (worker, handle) = super::super::EmbeddingWorker::new();
        handle.submit(job(1)).unwrap();
        assert_eq!(
            handle
                .cancel_with_count(job(1).db_path, Some("minilm-384".into()))
                .unwrap(),
            1
        );
        assert_eq!(
            handle
                .cancel_with_count(job(1).db_path, Some("minilm".into()))
                .unwrap(),
            0
        );
        assert_eq!(handle.cancel_with_count(job(1).db_path, None).unwrap(), 1);
        assert_eq!(handle.cancel_with_count(job(1).db_path, None).unwrap(), 0);
        // A fresh job is not swallowed by an older coalesced control message.
        handle.submit(job(1)).unwrap();
        assert_eq!(handle.cancel_with_count(job(1).db_path, None).unwrap(), 2);
        assert!(worker.receiver.state.lock().unwrap().pending.is_empty());
    }
}
