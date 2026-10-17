//! Contended callers must not wait for another caller's entire HTTP batch.
use super::*;
use std::sync::mpsc;

#[test]
fn external_waiter_cancels_while_another_request_is_still_in_flight() {
    let (active_tx, active_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = Server::new(move |index, input| {
        if index == 2 {
            active_tx.send(()).unwrap();
            // Bounded even if a broken client makes the test fail.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        }
        Reply::ok(success(input))
    });
    let cancelled = Arc::new(AtomicBool::new(false));
    let observe_waiter = Arc::new(AtomicBool::new(false));
    let (poll_tx, poll_rx) = mpsc::channel();
    let cancel = Arc::clone(&cancelled);
    let observe = Arc::clone(&observe_waiter);
    let embedder = Arc::new(
        ExternalEmbedder::connect(
            server.config(&[]),
            Arc::new(move || {
                if observe.load(Ordering::SeqCst) {
                    let _ = poll_tx.send(());
                }
                cancel.load(Ordering::SeqCst)
            }),
        )
        .unwrap(),
    );
    let owner = Arc::clone(&embedder);
    let owner = thread::spawn(move || owner.embed_sync("active request"));
    active_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(server.count(), 3, "two probes and one active request");
    observe_waiter.store(true, Ordering::SeqCst);
    let waiter = Arc::clone(&embedder);
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = done_tx.send(waiter.embed_sync("queued private text"));
    });
    // The owner is blocked in HTTP. Three polls establish that the waiter
    // entered and continued polling admission while the request gate is held.
    let polling = (0..3).all(|_| poll_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    cancelled.store(true, Ordering::SeqCst);
    let waiting_result = done_rx.recv_timeout(Duration::from_secs(1));
    // Always unblock/join both callers before asserting the regression, so an
    // old blocking lock fails the test rather than stranding a test thread.
    let _ = release_tx.send(());
    let owner_result = owner.join().unwrap();
    waiter.join().unwrap();
    assert!(polling, "the queued caller stopped polling cancellation");
    let error = waiting_result
        .expect("waiter must finish before the active HTTP request is released")
        .unwrap_err();
    assert!(error.to_string().contains("external_cancelled"));
    assert!(
        owner_result.is_err(),
        "the cancelled active reply is discarded"
    );
    assert_eq!(server.count(), 3, "queued text must never be transmitted");
    // Cancellation must not poison the gate or leave a detached sender.
    observe_waiter.store(false, Ordering::SeqCst);
    cancelled.store(false, Ordering::SeqCst);
    assert_eq!(
        embedder.embed_sync("resumed request").unwrap(),
        vector("resumed request")
    );
    assert_eq!(server.count(), 4);
}

#[test]
fn external_admission_deadline_expires_without_sending_or_poisoning_the_gate() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let mut provider = server.connect();
    // Isolate admission timing from preflight/HTTP timing on loaded CI hosts.
    provider.config.timeout = Duration::from_millis(50);
    let provider = Arc::new(provider);
    let gate = provider.request_gate.lock().unwrap();
    let queued = Arc::clone(&provider);
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = done_tx.send(queued.embed_sync("deadline private text"));
    });
    let result = done_rx.recv_timeout(Duration::from_secs(2));
    // Do not let a regression leave the thread permanently blocked.
    drop(gate);
    waiter.join().unwrap();
    let error = result
        .expect("admission deadline must bound queueing")
        .unwrap_err();
    assert!(error.to_string().contains("external_queue_timeout"));
    assert!(!error.to_string().contains("deadline private text"));
    assert_eq!(server.count(), 2, "only fixed preflight probes were sent");
    assert_eq!(provider.embed_sync("retry").unwrap(), vector("retry"));
    assert_eq!(server.count(), 3);
}

#[test]
fn external_waiter_proceeds_when_the_gate_is_released_before_its_deadline() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let observe = Arc::new(AtomicBool::new(false));
    let watching = Arc::clone(&observe);
    let (poll_tx, poll_rx) = mpsc::channel();
    let provider = Arc::new(
        ExternalEmbedder::connect(
            server.config(&[]),
            Arc::new(move || {
                if watching.load(Ordering::SeqCst) {
                    let _ = poll_tx.send(());
                }
                false
            }),
        )
        .unwrap(),
    );
    let gate = provider.request_gate.lock().unwrap();
    observe.store(true, Ordering::SeqCst);
    let queued = Arc::clone(&provider);
    let waiter = thread::spawn(move || queued.embed_batch_sync(&["first", "second"]));
    let polling = (0..3).all(|_| poll_rx.recv_timeout(Duration::from_secs(1)).is_ok());
    let before_release = server.count();
    drop(gate);
    let result = waiter.join().unwrap().unwrap();
    assert!(polling);
    assert_eq!(before_release, 2, "the held gate must exclude requests");
    assert_eq!(result, [vector("first"), vector("second")]);
    assert_eq!(server.count(), 3);
}

#[test]
fn external_poisoned_gate_is_an_explicit_failure_not_a_network_fallback() {
    let server = Server::new(|_, input| Reply::ok(success(input)));
    let provider = server.connect();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _gate = provider.request_gate.lock().unwrap();
        panic!("test-only poisoned admission gate");
    }));
    assert!(poisoned.is_err());
    let error = provider.embed_sync("never sent").unwrap_err();
    assert!(error.to_string().contains("request gate poisoned"));
    assert_eq!(server.count(), 2);
}
