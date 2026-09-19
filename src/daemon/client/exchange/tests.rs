//! Real Unix sockets and client methods, with synthetic protocol peers. These
//! tests establish transport contracts, not model accuracy or server runtime.

use super::*;
use crate::daemon::client::DaemonClientConfig;
use crate::daemon::protocol::{EmbedResponse, ErrorResponse, HealthStatus, encode_message};
use crate::search::daemon_client::DaemonClient as _;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;

fn client(timeout: Duration) -> UdsDaemonClient {
    UdsDaemonClient::new(DaemonClientConfig {
        auto_spawn: false,
        request_timeout: timeout,
        connect_timeout: timeout,
        ..Default::default()
    })
}

fn pair(timeout: Duration) -> io::Result<(UdsDaemonClient, UnixStream)> {
    let (socket, peer) = UnixStream::pair()?;
    peer.set_read_timeout(Some(Duration::from_secs(3)))?;
    peer.set_write_timeout(Some(Duration::from_secs(3)))?;
    let client = client(timeout);
    *client.connection.lock() = Some(socket);
    client.available.store(true, Ordering::SeqCst);
    *client.last_health_check.lock() = Some(Instant::now());
    Ok((client, peer))
}

fn receive(peer: &mut UnixStream) -> io::Result<FramedMessage<Request>> {
    let mut prefix = [0; 4];
    peer.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::other("unexpected test request size"));
    }
    let mut bytes = vec![0; length];
    peer.read_exact(&mut bytes)?;
    decode_message(&bytes).map_err(io::Error::other)
}

fn health(request_id: String) -> FramedMessage<Response> {
    FramedMessage::new(
        request_id,
        Response::Health(HealthStatus {
            uptime_secs: 1,
            version: PROTOCOL_VERSION,
            ready: true,
            memory_bytes: 0,
        }),
    )
}

fn reply(peer: &mut UnixStream, response: FramedMessage<Response>) -> io::Result<()> {
    peer.write_all(&encode_message(&response).map_err(io::Error::other)?)
}

fn invalidated(client: &UdsDaemonClient) {
    assert!(client.connection.lock().is_none());
    assert!(!client.available.load(Ordering::SeqCst));
    assert!(client.last_health_check.lock().is_none());
}

#[test]
fn trailing_response_bytes_invalidate_the_stream_without_replaying_the_request() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        let mut encoded = encode_message(&health(request.request_id)).map_err(io::Error::other)?;
        // The trailing object is inside the declared frame, not a second
        // valid frame in a persistent stream. A prefix-only decoder misses it.
        encoded.push(0xc0);
        let length = u32::try_from(encoded.len() - 4).map_err(io::Error::other)?;
        encoded[..4].copy_from_slice(&length.to_be_bytes());
        peer.write_all(&encoded)?;
        let count = peer.read(&mut [0])?;
        assert_eq!(
            count, 0,
            "client must close, not reuse or replay this exchange"
        );
        Ok(())
    });
    let result = client.health();
    server.join().unwrap()?;
    assert!(matches!(result, Err(DaemonError::Failed(_))));
    invalidated(&client);
    assert_eq!(client.request_counter.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn waiting_caller_times_out_without_clearing_the_owned_connection() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_millis(100))?;
    let client = Arc::new(client);
    let guard = client.connection.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = Arc::clone(&client);
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(waiting.health());
    });
    // Release the held lock even on a regression, so the test never deadlocks.
    let result = rx.recv_timeout(Duration::from_secs(2));
    assert!(client.available.load(Ordering::SeqCst));
    assert!(client.last_health_check.lock().is_some());
    drop(guard);
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        reply(&mut peer, health(request.request_id))
    });
    // A regressed waiter can now finish and consume the one server response;
    // validate its deadline result before issuing the next exchange.
    waiter.join().unwrap();
    assert!(matches!(result, Ok(Err(DaemonError::Timeout(_)))));
    assert!(client.health().is_ok());
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn spawn_lock_wait_is_bounded_and_does_not_launch_or_change_the_lock() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let socket = temp.path().join("absent.sock");
    let path = daemon_spawn_guard_lock_path(&socket);
    std::fs::write(&path, b"preserve spawn metadata")?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    lock.try_lock_exclusive()?;
    let client = UdsDaemonClient::new(DaemonClientConfig {
        socket_path: socket.clone(),
        auto_spawn: true,
        daemon_binary: Some(temp.path().join("must-not-execute")),
        connect_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(client.connect());
    });
    let result = rx.recv_timeout(Duration::from_secs(2));
    drop(lock);
    waiter.join().unwrap();
    assert!(matches!(result, Ok(Err(DaemonError::Timeout(_)))));
    assert_eq!(std::fs::read(&path)?, b"preserve spawn metadata");
    assert!(!socket.exists());
    assert_eq!(std::fs::read_dir(temp.path())?.count(), 1);
    Ok(())
}

#[test]
fn partial_response_is_dropped_and_the_next_call_reconnects_without_replay() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("peer.sock");
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let server = std::thread::spawn(move || -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        let end = Instant::now() + Duration::from_secs(3);
        for attempt in 0..2 {
            let mut peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break peer,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < end =>
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error),
                }
            };
            peer.set_read_timeout(Some(Duration::from_secs(2)))?;
            let request = receive(&mut peer)?;
            ids.push(request.request_id.clone());
            if attempt == 0 {
                let bytes =
                    encode_message(&health(request.request_id)).map_err(io::Error::other)?;
                peer.write_all(&bytes[..2])?;
                let mut unexpected_request = [0];
                assert_eq!(
                    peer.read(&mut unexpected_request)?,
                    0,
                    "a timed-out exchange must close, not replay"
                );
            } else {
                reply(&mut peer, health(request.request_id))?;
            }
        }
        Ok(ids)
    });
    let client = UdsDaemonClient::new(DaemonClientConfig {
        socket_path: path,
        auto_spawn: false,
        request_timeout: Duration::from_millis(150),
        connect_timeout: Duration::from_millis(150),
        ..Default::default()
    });
    assert!(matches!(client.health(), Err(DaemonError::Timeout(_))));
    invalidated(&client);
    assert!(client.health().is_ok());
    assert_eq!(server.join().unwrap()?, ["cass-0", "cass-1"]);
    Ok(())
}

#[test]
fn oversized_local_request_sends_nothing_and_preserves_the_live_connection() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let result = client.send_request(Request::EmbeddingJobStatus {
        db_path: "x".repeat(MAX_FRAME_BYTES),
    });
    assert!(matches!(result, Err(DaemonError::InvalidInput(_))));
    assert!(client.connection.lock().is_some());
    assert!(client.available.load(Ordering::SeqCst));
    peer.set_nonblocking(true)?;
    assert_eq!(
        peer.read(&mut [0]).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    peer.set_nonblocking(false)?;
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        reply(&mut peer, health(request.request_id))
    });
    assert!(client.health().is_ok());
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn peer_overload_is_bounded_and_does_not_poison_the_next_exchange() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let server = std::thread::spawn(move || -> io::Result<()> {
        let first = receive(&mut peer)?;
        reply(
            &mut peer,
            FramedMessage::new(
                first.request_id,
                Response::Error(ErrorResponse {
                    code: ErrorCode::Overloaded,
                    message: "é".repeat(1100),
                    retryable: true,
                    retry_after_ms: Some(17),
                }),
            ),
        )?;
        let next = receive(&mut peer)?;
        reply(&mut peer, health(next.request_id))
    });
    match client.health() {
        Err(DaemonError::Overloaded {
            retry_after,
            message,
        }) => {
            assert_eq!(retry_after, Some(Duration::from_millis(17)));
            assert_eq!(message, "é".repeat(1024) + " [truncated]");
        }
        other => panic!("wrong overload response: {other:?}"),
    }
    assert!(client.health().is_ok());
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn wrong_response_type_closes_without_echoing_the_foreign_payload() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        reply(
            &mut peer,
            FramedMessage::new(
                request.request_id,
                Response::Embed(EmbedResponse {
                    embeddings: vec![vec![0.5]],
                    model: "private-foreign-payload".into(),
                    elapsed_ms: 0,
                }),
            ),
        )
    });
    let error = client.health().unwrap_err();
    assert!(matches!(error, DaemonError::Failed(_)));
    assert!(!error.to_string().contains("private-foreign-payload"));
    invalidated(&client);
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn health_payload_version_mismatch_cannot_be_cached() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        let mut framed = health(request.request_id);
        if let Response::Health(health) = &mut framed.payload {
            health.version += 1;
        }
        reply(&mut peer, framed)
    });
    assert!(matches!(client.health(), Err(DaemonError::Failed(_))));
    invalidated(&client);
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn repeated_connect_reuses_the_same_stream_instead_of_replacing_it() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("reuse.sock");
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let client = UdsDaemonClient::new(DaemonClientConfig {
        socket_path: path,
        auto_spawn: false,
        ..Default::default()
    });
    client.connect().map_err(io::Error::other)?;
    let (mut peer, _) = listener.accept()?;
    peer.set_read_timeout(Some(Duration::from_secs(2)))?;
    client.connect().map_err(io::Error::other)?;
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        reply(&mut peer, health(request.request_id))
    });
    assert!(client.health().is_ok());
    server.join().unwrap()?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn full_unix_backlog_times_out_without_removing_or_respawning_a_live_endpoint() -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("busy.sock");
    let listener = UnixListener::bind(&path)?;
    // SAFETY: only changes the backlog of this test-owned listening socket.
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = transport::connect(&path, &Deadline::new(Duration::from_secs(1)))?;
    let before = std::fs::symlink_metadata(&path)?.ino();
    let client = UdsDaemonClient::new(DaemonClientConfig {
        socket_path: path.clone(),
        auto_spawn: true,
        daemon_binary: Some(temp.path().join("must-not-execute")),
        connect_timeout: Duration::from_millis(100),
        ..Default::default()
    });
    assert!(matches!(client.connect(), Err(DaemonError::Timeout(_))));
    assert_eq!(std::fs::symlink_metadata(&path)?.ino(), before);
    assert!(!daemon_spawn_guard_lock_path(&path).exists());
    Ok(())
}

#[test]
fn expired_startup_does_not_report_success_and_early_exit_is_observed() -> io::Result<()> {
    let temp = tempfile::tempdir()?;
    let client = UdsDaemonClient::new(DaemonClientConfig {
        socket_path: temp.path().join("absent.sock"),
        auto_spawn: false,
        ..Default::default()
    });
    let mut child = Command::new("sh").args(["-c", "exec sleep 2"]).spawn()?;
    let result = client.wait_until(&mut child, &Deadline::new(Duration::from_millis(30)));
    let _ = child.kill();
    let _ = child.wait();
    assert!(matches!(result, Err(DaemonError::Timeout(_))));
    let mut child = Command::new("sh").args(["-c", "exit 7"]).spawn()?;
    let result = client.wait_until(&mut child, &Deadline::new(Duration::from_secs(2)));
    let _ = child.wait();
    assert!(matches!(result, Err(DaemonError::Unavailable(_))));
    assert!(!client.config.socket_path.exists());
    Ok(())
}

#[test]
fn raw_embedding_count_dimension_and_finiteness_fail_before_returning_any_vector() -> io::Result<()>
{
    let mut nonfinite = vec![0.5; 384];
    nonfinite[123] = f32::NAN;
    for vectors in [
        vec![],
        vec![vec![0.5; 384]; 2],
        vec![vec![0.5; 383]],
        vec![vec![]],
        vec![nonfinite],
    ] {
        let (client, mut peer) = pair(Duration::from_secs(2))?;
        let server = std::thread::spawn(move || -> io::Result<()> {
            let request = receive(&mut peer)?;
            reply(
                &mut peer,
                FramedMessage::new(
                    request.request_id,
                    Response::Embed(EmbedResponse {
                        embeddings: vectors,
                        model: "minilm-384".into(),
                        elapsed_ms: 0,
                    }),
                ),
            )
        });
        assert!(matches!(
            client.embed("private-input", "request"),
            Err(DaemonError::Failed(_))
        ));
        invalidated(&client);
        server.join().unwrap()?;
    }
    Ok(())
}

#[test]
fn valid_embedding_order_and_exact_float_bits_survive_transport() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let expected = vec![vec![-0.0_f32; 384], vec![-0.375; 384]];
    let vectors = expected.clone();
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        reply(
            &mut peer,
            FramedMessage::new(
                request.request_id,
                Response::Embed(EmbedResponse {
                    embeddings: vectors,
                    model: "minilm-384".into(),
                    elapsed_ms: 0,
                }),
            ),
        )
    });
    let actual = client
        .embed_batch(&["first", "second"], "request")
        .map_err(io::Error::other)?;
    let bits = |rows: Vec<Vec<f32>>| {
        rows.into_iter()
            .map(|row| row.into_iter().map(f32::to_bits).collect::<Vec<_>>())
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(actual), bits(expected));
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn nonfinite_rerank_scores_cannot_enter_search_results() -> io::Result<()> {
    let (client, mut peer) = pair(Duration::from_secs(2))?;
    let server = std::thread::spawn(move || -> io::Result<()> {
        let request = receive(&mut peer)?;
        reply(
            &mut peer,
            FramedMessage::new(
                request.request_id,
                Response::Rerank(crate::daemon::protocol::RerankResponse {
                    scores: vec![f32::INFINITY],
                    model: "ms-marco-minilm-l6-v2".into(),
                    elapsed_ms: 0,
                }),
            ),
        )
    });
    assert!(matches!(
        client.rerank("query", &["document"], "request"),
        Err(DaemonError::Failed(_))
    ));
    invalidated(&client);
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn uncached_availability_probe_timeout_does_not_disable_another_callers_connection()
-> io::Result<()> {
    let (client, peer) = pair(Duration::from_millis(100))?;
    let client = Arc::new(client);
    *client.last_health_check.lock() = None;
    let guard = client.connection.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiting = Arc::clone(&client);
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(waiting.is_available());
    });
    let result = rx.recv_timeout(Duration::from_secs(2));
    // Capture the ownership invariant before releasing the simulated active
    // request. Release both resources even if a regression missed the deadline.
    let remained_available = client.available.load(Ordering::SeqCst);
    let cache_remained_empty = client.last_health_check.lock().is_none();
    drop(guard);
    drop(peer);
    waiter.join().unwrap();
    assert!(
        matches!(result, Ok(false)),
        "the waiting probe must report unavailable for this call only"
    );
    assert!(
        remained_available,
        "a probe that never owned the exchange must not disable the shared stream"
    );
    assert!(
        cache_remained_empty,
        "the timed-out probe must not synthesize a health observation"
    );
    assert!(
        client.connection.lock().is_some(),
        "the waiting probe must not clear another caller's stream"
    );
    Ok(())
}
