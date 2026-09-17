//! Bounded embedding mailbox with cancellation attached to accepted jobs.
//!
//! Control messages run before pending submissions. A cancel marks only jobs
//! already admitted under the queue lock; a later retry gets fresh tokens.
//! Dequeue registers the active job under that same lock, closing the gap
//! between removing a submission and starting its first pass.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};

use super::{EmbeddingJobConfig, FastEmbedder, WorkerMessage};

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
    models: Vec<(String, AtomicBool)>,
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
                .map(|model| (model_key(&model), AtomicBool::new(false)))
                .collect(),
        }))
    }

    pub(super) fn is_cancelled(&self, model: &str) -> bool {
        let key = model_key(model);
        self.0.models.iter().any(|(candidate, cancelled)| {
            candidate == &key && cancelled.load(Ordering::SeqCst)
        })
    }

    pub(super) fn all_cancelled(&self) -> bool {
        self.0
            .models
            .iter()
            .all(|(_, cancelled)| cancelled.load(Ordering::SeqCst))
    }

    fn any_cancelled(&self) -> bool {
        self.0
            .models
            .iter()
            .any(|(_, cancelled)| cancelled.load(Ordering::SeqCst))
    }

    fn cancel(&self, scope: &CancelScope) {
        if self.0.db_path != scope.db_path {
            return;
        }
        let selected = scope.model.as_deref().map(model_key);
        for (model, cancelled) in &self.0.models {
            if selected.as_ref().is_none_or(|selected| selected == model) {
                cancelled.store(true, Ordering::SeqCst);
            }
        }
    }

    fn cancel_all(&self) {
        for (_, cancelled) in &self.0.models {
            cancelled.store(true, Ordering::SeqCst);
        }
    }
}

struct PendingJob {
    config: EmbeddingJobConfig,
    control: JobControl,
}

struct CancelScope {
    db_path: String,
    model: Option<String>,
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
                        return Err("embedding job queue is full; retry after pending work completes".to_string());
                    }
                    let control = JobControl::new(&config);
                    state.pending.push_back(PendingJob { config, control });
                }
            }
            WorkerMessage::Cancel { db_path, model_id } => {
                validate_request(&[&db_path, model_id.as_deref().unwrap_or_default()])?;
                if db_path.trim().is_empty() {
                    return Err("embedding cancellation requires a database path".to_string());
                }
                let scope = CancelScope { db_path, model: model_id };
                let duplicate = state.cancellations.iter().any(|pending| pending.covers(&scope));
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
                    active.cancel(&scope);
                }
                for pending in &state.pending {
                    pending.control.cancel(&scope);
                }
                state.pending.retain(|pending| !pending.control.all_cancelled());
                if !duplicate {
                    state.cancellations.retain(|pending| !scope.covers(pending));
                    state.cancellations.push_back(scope);
                }
            }
            WorkerMessage::Shutdown => {
                state.stopped = true;
                if let Some(active) = &state.active {
                    active.cancel_all();
                }
                state.pending.clear();
                state.cancellations.clear();
            }
        }
        drop(state);
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => Err("embedding worker channel closed".to_string()),
        }
    }
}

impl Receiver {
    pub(super) fn recv(&self) -> Result<(WorkerMessage, Option<JobPermit>), String> {
        loop {
            {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "embedding queue state lock poisoned".to_string())?;
                if state.stopped {
                    return Ok((WorkerMessage::Shutdown, None));
                }
                if let Some(scope) = state.cancellations.pop_front() {
                    return Ok((WorkerMessage::Cancel {
                        db_path: scope.db_path,
                        model_id: scope.model,
                    }, None));
                }
                if state.active.is_some() {
                    return Err("previous embedding job permit is still active".to_string());
                }
                if let Some(pending) = state.pending.pop_front() {
                    state.active = Some(pending.control.clone());
                    let permit = JobPermit {
                        state: Arc::downgrade(&self.state),
                        control: pending.control,
                    };
                    return Ok((WorkerMessage::Submit(pending.config), Some(permit)));
                }
            }
            self.wake
                .recv()
                .map_err(|_| "embedding worker channel closed".to_string())?;
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.receiver_alive = false;
            if let Some(active) = &state.active {
                active.cancel_all();
            }
            state.pending.clear();
            state.cancellations.clear();
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
        sender.send(WorkerMessage::Cancel {
            db_path: job(id).db_path,
            model_id: model.map(str::to_string),
        }).unwrap();
    }

    #[test]
    fn admission_is_bounded_and_pending_duplicates_coalesce_even_when_full() {
        let (sender, receiver) = channel();
        for id in 0..MAX_PENDING_JOBS {
            sender.send(WorkerMessage::Submit(job(id))).unwrap();
        }
        sender.send(WorkerMessage::Submit(job(0))).unwrap();
        assert!(sender.send(WorkerMessage::Submit(job(MAX_PENDING_JOBS))).is_err());
        let (first, permit) = receiver.recv().unwrap();
        assert!(matches!(first, WorkerMessage::Submit(config) if config.db_path == job(0).db_path));
        // A rerun accepted while the old job is active must not disappear.
        sender.send(WorkerMessage::Submit(job(0))).unwrap();
        assert_eq!(sender.state.lock().unwrap().pending.len(), MAX_PENDING_JOBS);
        drop(permit);
        for id in 1..MAX_PENDING_JOBS {
            let (message, permit) = receiver.recv().unwrap();
            assert!(matches!(message, WorkerMessage::Submit(config) if config.db_path == job(id).db_path));
            drop(permit);
        }
        let (last, permit) = receiver.recv().unwrap();
        assert!(matches!(last, WorkerMessage::Submit(config) if config.db_path == job(0).db_path));
        drop(permit);
    }

    #[test]
    fn cancel_removes_queued_work_before_it_can_open_an_archive() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        sender.send(WorkerMessage::Submit(job(2))).unwrap();
        cancel(&sender, 1, None);
        assert!(matches!(receiver.recv().unwrap().0, WorkerMessage::Cancel { .. }));
        let (message, permit) = receiver.recv().unwrap();
        assert!(matches!(message, WorkerMessage::Submit(config) if config.db_path == job(2).db_path));
        drop(permit);
        assert!(sender.state.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn cancellation_is_scoped_to_a_job_generation_and_model_including_dequeue_gap() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        let (_, old_permit) = receiver.recv().unwrap();
        let old = old_permit.as_ref().unwrap().control();
        cancel(&sender, 2, None);
        assert!(!old.any_cancelled());
        cancel(&sender, 1, Some("all-minilm-l6-v2"));
        assert!(old.is_cancelled("minilm-384"));
        assert!(!old.is_cancelled("hash"));
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        drop(old_permit);
        assert!(matches!(receiver.recv().unwrap().0, WorkerMessage::Cancel { .. }));
        assert!(matches!(receiver.recv().unwrap().0, WorkerMessage::Cancel { .. }));
        let (_, new_permit) = receiver.recv().unwrap();
        let new = new_permit.as_ref().unwrap().control();
        assert!(!new.any_cancelled(), "a retry admitted after cancellation is a new job");
        assert!(old.is_cancelled("minilm"));
        drop(new_permit);
    }

    #[test]
    fn cancelling_one_queued_tier_preserves_the_other_and_does_not_absorb_a_retry() {
        let (sender, receiver) = channel();
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        cancel(&sender, 1, Some("fnv1a-384"));
        sender.send(WorkerMessage::Submit(job(1))).unwrap();
        assert!(matches!(receiver.recv().unwrap().0, WorkerMessage::Cancel { .. }));
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
        assert!(matches!(receiver.recv().unwrap().0, WorkerMessage::Shutdown));
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
        assert_eq!(sender.state.lock().unwrap().cancellations.len(), MAX_PENDING_CANCELLATIONS);
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
        let threads = (0..MAX_PENDING_JOBS * 2).map(|id| {
            let sender = sender.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                sender.send(WorkerMessage::Submit(job(id))).is_ok()
            })
        }).collect::<Vec<_>>();
        let admitted = threads.into_iter().map(|thread| usize::from(thread.join().unwrap())).sum::<usize>();
        assert_eq!(admitted, MAX_PENDING_JOBS);
        assert_eq!(sender.state.lock().unwrap().pending.len(), MAX_PENDING_JOBS);
    }
}
