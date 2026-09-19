//! An owned startup worker: controls are served while native models load.
//!
//! Reserve foreground inference before scheduling the loader, rather than
//! racing the first client. Scope ownership joins native work on shutdown;
//! cancellation can skip later stages but cannot preempt a running model call.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{Scope, ScopedJoinHandle};

use super::inference::InferenceGate;

pub(super) struct StartupWarmup<'scope> {
    worker: ScopedJoinHandle<'scope, ()>,
}

impl<'scope> StartupWarmup<'scope> {
    pub(super) fn spawn<'env>(
        scope: &'scope Scope<'scope, 'env>,
        gate: &'scope InferenceGate,
        shutdown: &'scope AtomicBool,
        warmup: impl FnOnce() + Send + 'scope,
    ) -> io::Result<Self> {
        let permit = gate.try_enter().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "daemon startup inference is already owned",
            )
        })?;
        let worker = std::thread::Builder::new()
            .name("cass-model-warmup".to_owned())
            .spawn_scoped(scope, move || {
                let _permit = permit;
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(warmup))
                {
                    // Wake the serving loop so it cancels handlers and joins
                    // both this worker and the background embedding worker.
                    shutdown.store(true, Ordering::Release);
                    std::panic::resume_unwind(payload);
                }
            })?;
        Ok(Self { worker })
    }

    pub(super) fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }

    pub(super) fn join(self) -> io::Result<()> {
        self.worker
            .join()
            .map_err(|_| io::Error::other("daemon model warm-up panicked"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WarmupStage {
    Embedder,
    Attestation,
    Reranker,
}

pub(super) fn run_stages(shutdown: &AtomicBool, mut run: impl FnMut(WarmupStage)) {
    for stage in [
        WarmupStage::Embedder,
        WarmupStage::Attestation,
        WarmupStage::Reranker,
    ] {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        run(stage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn inference_is_reserved_before_the_startup_handle_is_returned() {
        let gate = InferenceGate::default();
        let shutdown = AtomicBool::new(false);
        let (release_tx, release_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let warmup = StartupWarmup::spawn(scope, &gate, &shutdown, move || {
                release_rx.recv().unwrap();
            })
            .unwrap();
            let refused = gate.try_enter().is_err();
            let unfinished = !warmup.is_finished();
            release_tx.send(()).unwrap();
            warmup.join().unwrap();
            assert!(refused);
            assert!(unfinished);
            assert!(gate.try_enter().is_ok());
        });
    }

    #[test]
    fn busy_startup_does_not_steal_the_current_inference_owner() {
        let gate = InferenceGate::default();
        let shutdown = AtomicBool::new(false);
        let owner = gate.try_enter().unwrap();
        std::thread::scope(|scope| {
            let error = StartupWarmup::spawn(scope, &gate, &shutdown, || panic!("must not run"))
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            assert!(gate.try_enter().is_err());
        });
        drop(owner);
        assert!(gate.try_enter().is_ok());
    }

    #[test]
    fn preexisting_shutdown_skips_loading_and_releases_the_permit() {
        let gate = InferenceGate::default();
        let shutdown = AtomicBool::new(true);
        std::thread::scope(|scope| {
            StartupWarmup::spawn(scope, &gate, &shutdown, || panic!("must not load"))
                .unwrap()
                .join()
                .unwrap();
        });
        assert!(gate.try_enter().is_ok());
    }

    #[test]
    fn a_panicking_loader_cancels_serving_and_is_joined_as_an_error() {
        let gate = InferenceGate::default();
        let shutdown = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let warmup = StartupWarmup::spawn(scope, &gate, &shutdown, || {
                panic!("injected model panic");
            })
            .unwrap();
            assert!(warmup.join().is_err());
        });
        assert!(shutdown.load(Ordering::Acquire));
        assert!(gate.try_enter().is_ok());
    }

    #[test]
    fn cancellation_skips_later_stages_without_detaching_the_current_call() {
        let gate = InferenceGate::default();
        let shutdown = AtomicBool::new(false);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let stages = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            let shutdown_ref = &shutdown;
            let stages_ref = &stages;
            let warmup = StartupWarmup::spawn(scope, &gate, &shutdown, move || {
                run_stages(shutdown_ref, |stage| {
                    stages_ref.lock().unwrap().push(stage);
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                });
            })
            .unwrap();
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            shutdown.store(true, Ordering::Release);
            let still_owned = gate.try_enter().is_err() && !warmup.is_finished();
            release_tx.send(()).unwrap();
            warmup.join().unwrap();
            assert!(
                still_owned,
                "cancellation must not detach a running native load"
            );
        });
        assert_eq!(*stages.lock().unwrap(), vec![WarmupStage::Embedder]);
        assert!(gate.try_enter().is_ok());
    }

    #[test]
    fn successful_startup_authenticates_embedding_before_optional_reranker() {
        let shutdown = AtomicBool::new(false);
        let mut stages = Vec::new();
        run_stages(&shutdown, |stage| stages.push(stage));
        assert_eq!(
            stages,
            vec![
                WarmupStage::Embedder,
                WarmupStage::Attestation,
                WarmupStage::Reranker
            ]
        );
    }

    #[test]
    fn cancelled_startup_never_enters_a_stage() {
        let shutdown = AtomicBool::new(true);
        run_stages(&shutdown, |_| panic!("cancelled startup entered a stage"));
    }
}
