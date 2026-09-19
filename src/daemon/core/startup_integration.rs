//! Real serving-loop tests with an intentionally blocked startup callback.
//! No model assets are installed; this proves control/routing and ownership,
//! not native inference accuracy or cold-model loading performance.

use super::*;
use std::sync::mpsc;

fn control(peer: &mut UnixStream, request: Request) -> anyhow::Result<Response> {
    peer.write_all(&encode_message(&FramedMessage::new(
        "startup-control",
        request,
    ))?)?;
    let mut prefix = [0; 4];
    peer.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    anyhow::ensure!(length > 0 && length <= MAX_FRAME_BYTES);
    let mut bytes = vec![0; length];
    peer.read_exact(&mut bytes)?;
    let response = decode_message::<Response>(&bytes)?;
    anyhow::ensure!(response.request_id == "startup-control");
    anyhow::ensure!(response.version == PROTOCOL_VERSION);
    Ok(response.payload)
}

fn daemon_for(dir: &Path) -> ModelDaemon {
    ModelDaemon::new(
        DaemonConfig {
            socket_path: dir.join("control.sock"),
            request_timeout: Duration::from_secs(2),
            idle_timeout: Duration::ZERO,
            index_interval: Duration::ZERO,
            memory_limit: 0,
            nice_value: 0,
            ionice_class: 0,
            ..Default::default()
        },
        ModelManager::new(dir),
    )
}

#[test]
fn health_status_and_shutdown_work_before_warmup_finishes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let daemon = Arc::new(daemon_for(dir.path()));
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let owner = Arc::clone(&daemon);
    let server = std::thread::spawn(move || {
        owner.run_with_model_warmup(move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
        })
    });
    let outcome = (|| -> anyhow::Result<()> {
        started_rx.recv_timeout(Duration::from_secs(5))?;
        let mut peer = UnixStream::connect(&daemon.config.socket_path)?;
        peer.set_read_timeout(Some(Duration::from_secs(2)))?;
        peer.set_write_timeout(Some(Duration::from_secs(2)))?;
        anyhow::ensure!(
            matches!(control(&mut peer, Request::Health)?, Response::Health(health) if !health.ready)
        );
        let Response::Status(status) = control(&mut peer, Request::Status)? else {
            anyhow::bail!("startup status did not respond");
        };
        anyhow::ensure!(status.embedders.len() == 1 && !status.embedders[0].loaded);
        anyhow::ensure!(status.embedders[0].dimension.is_none());
        anyhow::ensure!(status.rerankers.len() == 1 && !status.rerankers[0].loaded);
        let response = control(
            &mut peer,
            Request::Embed {
                texts: vec!["do not queue behind startup".into()],
                model: "default".into(),
                dims: None,
            },
        )?;
        anyhow::ensure!(
            matches!(response, Response::Error(error) if error.code == ErrorCode::Overloaded && error.retryable)
        );
        anyhow::ensure!(matches!(
            control(&mut peer, Request::Shutdown)?,
            Response::Shutdown { .. }
        ));
        anyhow::ensure!(daemon.shutdown.load(Ordering::Acquire));
        anyhow::ensure!(
            !server.is_finished(),
            "shutdown detached the blocked startup owner"
        );
        anyhow::ensure!(!daemon.models.embedder_loaded());
        Ok(())
    })();
    // Release and join even on an assertion/transport error. A bad regression
    // must fail, rather than pinning the whole test runner behind our callback.
    daemon.request_shutdown();
    let _ = release_tx.send(());
    server
        .join()
        .map_err(|_| anyhow::anyhow!("daemon server panicked"))??;
    outcome?;
    anyhow::ensure!(daemon.active_connections.load(Ordering::Acquire) == 0);
    anyhow::ensure!(daemon.worker_thread.lock().is_none());
    anyhow::ensure!(daemon.inference_gate.try_enter().is_ok());
    anyhow::ensure!(!daemon.config.socket_path.exists());
    Ok(())
}

#[test]
fn startup_panic_stops_the_serving_loop_and_cleans_up_owned_work() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let daemon = daemon_for(dir.path());
    let result = daemon.run_with_model_warmup(|| panic!("injected startup failure"));
    anyhow::ensure!(result.is_err());
    anyhow::ensure!(daemon.shutdown.load(Ordering::Acquire));
    anyhow::ensure!(daemon.worker_thread.lock().is_none());
    anyhow::ensure!(daemon.worker_handle.lock().is_none());
    anyhow::ensure!(daemon.inference_gate.try_enter().is_ok());
    anyhow::ensure!(!daemon.config.socket_path.exists());
    Ok(())
}
