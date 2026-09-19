//! Single-flight model loading without holding the published-state lock.
//!
//! Health/status inspect a short-lived snapshot even while a loader is blocked
//! in native code. A model and its display name publish together, only after the
//! loader succeeds. Unload serializes with loading so a late load cannot undo
//! it. Already borrowed model owners may finish inference after an unload.

use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

pub(super) struct ModelSnapshot<T: ?Sized> {
    pub model: Option<Arc<T>>,
    pub name: String,
}

pub(super) struct ModelSlot<T: ?Sized> {
    loading: Mutex<()>,
    published: RwLock<ModelSnapshot<T>>,
}

impl<T: ?Sized> ModelSlot<T> {
    pub(super) fn new() -> Self {
        Self {
            loading: Mutex::new(()),
            published: RwLock::new(ModelSnapshot {
                model: None,
                name: "not-loaded".to_owned(),
            }),
        }
    }

    pub(super) fn get(&self) -> Option<Arc<T>> {
        self.published.read().model.clone()
    }

    pub(super) fn snapshot(&self) -> ModelSnapshot<T> {
        let state = self.published.read();
        ModelSnapshot {
            model: state.model.clone(),
            name: state.name.clone(),
        }
    }

    pub(super) fn load_with<E>(
        &self,
        loader: impl FnOnce() -> Result<(Arc<T>, String), E>,
    ) -> Result<(), E> {
        if self.get().is_some() {
            return Ok(());
        }
        let _loading = self.loading.lock();
        if self.get().is_some() {
            return Ok(());
        }
        self.published.write().name = "loading".to_owned();
        // This guard also records a panicking loader as failed, without
        // swallowing the panic or storing private backend error messages.
        let _attempt = LoadAttempt(self);
        let (model, name) = loader()?;
        *self.published.write() = ModelSnapshot {
            model: Some(model),
            name,
        };
        Ok(())
    }

    pub(super) fn unload(&self) {
        let previous = {
            let _loading = self.loading.lock();
            std::mem::replace(
                &mut *self.published.write(),
                ModelSnapshot {
                    model: None,
                    name: "not-loaded".to_owned(),
                },
            )
        };
        // Native model destructors may be expensive. Never run one under the
        // publication lock needed by health/status or by another loader.
        drop(previous);
    }
}

struct LoadAttempt<'a, T: ?Sized>(&'a ModelSlot<T>);

impl<T: ?Sized> Drop for LoadAttempt<'_, T> {
    fn drop(&mut self) {
        let mut state = self.0.published.write();
        if state.model.is_none() {
            state.name = "load-failed".to_owned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn metadata_remains_readable_while_loader_is_blocked() {
        let slot = Arc::new(ModelSlot::<usize>::new());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let loading = Arc::clone(&slot);
        let worker = std::thread::spawn(move || {
            loading.load_with(|| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok::<_, ()>((Arc::new(42), "loaded-model".to_owned()))
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let inspecting = Arc::clone(&slot);
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let observer = std::thread::spawn(move || {
            snapshot_tx.send(inspecting.snapshot()).unwrap();
        });
        let snapshot = snapshot_rx.recv_timeout(Duration::from_secs(1));
        // Release and join even when the observation failed; a regression must
        // fail an assertion rather than leave a test process deadlocked.
        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        observer.join().unwrap();
        let snapshot = snapshot.expect("metadata waited for native model loading");
        assert!(snapshot.model.is_none());
        assert_eq!(snapshot.name, "loading");
        let snapshot = slot.snapshot();
        assert_eq!(*snapshot.model.unwrap(), 42);
        assert_eq!(snapshot.name, "loaded-model");
    }

    #[test]
    fn concurrent_loads_publish_one_owner_and_execute_one_loader() {
        let slot = Arc::new(ModelSlot::<usize>::new());
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let slot = Arc::clone(&slot);
                let calls = &calls;
                scope.spawn(move || {
                    slot.load_with(|| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, ()>((Arc::new(7), "seven".to_owned()))
                    })
                    .unwrap();
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&slot.get().unwrap(), &slot.get().unwrap()));
    }

    #[test]
    fn failed_load_is_not_published_and_a_later_request_can_retry() {
        let slot = ModelSlot::<usize>::new();
        assert_eq!(
            slot.load_with(|| Err("private backend detail")),
            Err("private backend detail")
        );
        assert!(slot.get().is_none());
        assert_eq!(slot.snapshot().name, "load-failed");
        slot.load_with(|| Ok::<_, ()>((Arc::new(1), "recovered".to_owned())))
            .unwrap();
        assert_eq!(slot.snapshot().name, "recovered");
    }

    #[test]
    fn panicking_load_releases_admission_and_does_not_stay_loading() {
        let slot = ModelSlot::<usize>::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slot.load_with::<()>(|| panic!("injected loader panic"))
        }));
        assert!(result.is_err());
        assert!(slot.get().is_none());
        assert_eq!(slot.snapshot().name, "load-failed");
        slot.load_with(|| Ok::<_, ()>((Arc::new(1), "recovered".to_owned())))
            .unwrap();
    }

    #[test]
    fn unload_waits_for_loading_and_prevents_late_republication() {
        let slot = Arc::new(ModelSlot::<usize>::new());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let loading = Arc::clone(&slot);
        let loader = std::thread::spawn(move || {
            loading.load_with(|| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok::<_, ()>((Arc::new(3), "three".to_owned()))
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let unloading = Arc::clone(&slot);
        let (unloaded_tx, unloaded_rx) = mpsc::channel();
        let unloader = std::thread::spawn(move || {
            unloading.unload();
            unloaded_tx.send(()).unwrap();
        });
        let early = unloaded_rx.recv_timeout(Duration::from_millis(50));
        release_tx.send(()).unwrap();
        loader.join().unwrap().unwrap();
        unloader.join().unwrap();
        assert!(early.is_err(), "unload returned before the pending loader");
        assert!(slot.get().is_none());
        assert_eq!(slot.snapshot().name, "not-loaded");
    }

    #[test]
    fn unload_preserves_already_borrowed_owners_not_future_snapshots() {
        let slot = ModelSlot::<usize>::new();
        slot.load_with(|| Ok::<_, ()>((Arc::new(9), "nine".to_owned())))
            .unwrap();
        let borrowed = slot.get().unwrap();
        slot.unload();
        assert_eq!(*borrowed, 9);
        assert!(slot.get().is_none());
        assert_eq!(slot.snapshot().name, "not-loaded");
    }

    #[test]
    fn metadata_does_not_wait_for_model_destruction() {
        struct SlowDrop(mpsc::Sender<()>, mpsc::Receiver<()>);
        impl Drop for SlowDrop {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
                self.1.recv().unwrap();
            }
        }
        // Mutex makes the synthetic destructor gate Sync, like native models.
        let slot = Arc::new(ModelSlot::<Mutex<SlowDrop>>::new());
        let (dropping_tx, dropping_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        slot.load_with(|| {
            Ok::<_, ()>((
                Arc::new(Mutex::new(SlowDrop(dropping_tx, release_rx))),
                "slow".to_owned(),
            ))
        })
        .unwrap();
        let unloading = Arc::clone(&slot);
        let unloader = std::thread::spawn(move || unloading.unload());
        dropping_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let inspecting = Arc::clone(&slot);
        let (tx, rx) = mpsc::channel();
        let observer = std::thread::spawn(move || tx.send(inspecting.snapshot().name).unwrap());
        let name = rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        unloader.join().unwrap();
        observer.join().unwrap();
        assert_eq!(name.unwrap(), "not-loaded");
    }
}
