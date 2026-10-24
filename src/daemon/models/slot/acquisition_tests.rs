use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn concurrent_acquisitions_return_the_single_published_owner() {
    let slot = ModelSlot::<usize>::new();
    let calls = AtomicUsize::new(0);
    let owners = std::thread::scope(|scope| {
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                scope.spawn(|| {
                    slot.acquire_with(|| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok::<_, ()>((Arc::new(17), "seventeen".to_owned()))
                    })
                    .unwrap()
                })
            })
            .collect();
        tasks
            .into_iter()
            .map(|task| task.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let published = slot.get().unwrap();
    assert!(owners.iter().all(|owner| Arc::ptr_eq(owner, &published)));
}

#[test]
fn waiting_unload_cannot_remove_the_owner_returned_by_loading() {
    let slot = Arc::new(ModelSlot::<usize>::new());
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let loading = Arc::clone(&slot);
    let loader = std::thread::spawn(move || {
        loading.acquire_with(|| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok::<_, ()>((Arc::new(23), "twenty-three".to_owned()))
        })
    });
    let started = started_rx.recv_timeout(Duration::from_secs(5));
    let unloading = Arc::clone(&slot);
    let unloader = std::thread::spawn(move || unloading.unload());
    // Release the loader and join both workers before asserting observations.
    release_tx.send(()).unwrap();
    let acquired = loader.join().unwrap().unwrap();
    unloader.join().unwrap();
    started.expect("loader did not enter its publication gate");
    assert!(slot.get().is_none());
    assert_eq!(slot.snapshot().name, "not-loaded");
    assert_eq!(*acquired, 23);
    assert_eq!(Arc::strong_count(&acquired), 1);
}

#[test]
fn warm_acquisition_keeps_its_owner_across_unload_and_replacement() {
    let slot = ModelSlot::<usize>::new();
    let first = slot
        .acquire_with(|| Ok::<_, ()>((Arc::new(1), "first".to_owned())))
        .unwrap();
    let borrowed = slot
        .acquire_with::<()>(|| panic!("warm acquisition ran a loader"))
        .unwrap();
    assert!(Arc::ptr_eq(&first, &borrowed));
    slot.unload();
    let replacement = slot
        .acquire_with(|| Ok::<_, ()>((Arc::new(2), "second".to_owned())))
        .unwrap();
    assert!(!Arc::ptr_eq(&first, &replacement));
    assert_eq!(*borrowed, 1);
    assert_eq!(*slot.get().unwrap(), 2);
    let weak = Arc::downgrade(&first);
    drop(first);
    assert!(weak.upgrade().is_some());
    drop(borrowed);
    assert!(weak.upgrade().is_none(), "the slot leaked its prior owner");
}

#[test]
fn acquisition_failures_publish_no_owner_and_remain_retryable() {
    let slot = ModelSlot::<usize>::new();
    assert_eq!(
        slot.acquire_with(|| Err("injected load error")),
        Err("injected load error")
    );
    assert!(slot.get().is_none());
    assert_eq!(slot.snapshot().name, "load-failed");
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        slot.acquire_with::<()>(|| panic!("injected load panic"))
    }));
    assert!(panic.is_err());
    assert!(slot.get().is_none());
    assert_eq!(slot.snapshot().name, "load-failed");
    let recovered = slot
        .acquire_with(|| Ok::<_, ()>((Arc::new(31), "recovered".to_owned())))
        .unwrap();
    assert_eq!(*recovered, 31);
    assert!(Arc::ptr_eq(&recovered, &slot.get().unwrap()));
}
