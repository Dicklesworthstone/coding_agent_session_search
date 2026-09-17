//! Real daemon/socket regressions requiring no model assets or user archive.
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use coding_agent_search::daemon::core::{DaemonConfig, ModelDaemon};
use coding_agent_search::daemon::models::ModelManager;
use coding_agent_search::daemon::protocol::{
    ErrorCode, FramedMessage, PROTOCOL_VERSION, Request, Response, decode_message, encode_message,
};

struct RunningDaemon {
    daemon: Arc<ModelDaemon>,
    thread: Option<JoinHandle<std::io::Result<()>>>,
}

impl RunningDaemon {
    fn start(root: &Path) -> anyhow::Result<(Self, UnixStream)> {
        let socket = root.join("s");
        let daemon = Arc::new(ModelDaemon::new(
            DaemonConfig {
                socket_path: socket.clone(),
                request_timeout: Duration::from_secs(2),
                nice_value: 0,
                index_interval: Duration::ZERO,
                // No authority directory: this fixture must not create keys.
                data_dir: None,
                ..Default::default()
            },
            ModelManager::new(root),
        ));
        let owner = Arc::clone(&daemon);
        let thread = std::thread::spawn(move || owner.run());
        let running = Self { daemon, thread: Some(thread) };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = UnixStream::connect(&socket) {
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                return Ok((running, stream));
            }
            if Instant::now() >= deadline || running.thread.as_ref().is_some_and(|thread| thread.is_finished()) {
                anyhow::bail!("fixture daemon did not accept connections");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.daemon.request_shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| anyhow::anyhow!("fixture daemon panicked"))??;
        }
        Ok(())
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.daemon.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn exchange(stream: &mut UnixStream, id: &str, request: Request) -> anyhow::Result<Response> {
    stream.write_all(&encode_message(&FramedMessage::new(id, request))?)?;
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    anyhow::ensure!(length <= 10 * 1024 * 1024, "oversized fixture response");
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload)?;
    let response = decode_message::<Response>(&payload)?;
    anyhow::ensure!(response.version == PROTOCOL_VERSION, "wrong response version");
    anyhow::ensure!(response.request_id == id, "wrong response request id");
    Ok(response.payload)
}

#[test]
fn bounded_inference_rejections_keep_a_framed_connection_usable() -> anyhow::Result<()> {
    const CHILD: &str = "CASS_INFERENCE_WIRE_TEST_CHILD";
    const TEST: &str = "bounded_inference_rejections_keep_a_framed_connection_usable";
    // Isolate model-selection and .env resolution without process-global env
    // mutation. An unsupported selection fails before consulting any assets.
    if dotenvy::var(CHILD).ok().as_deref() != Some(TEST) {
        let root = tempfile::tempdir()?;
        let result = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
            .env(CHILD, TEST)
            .env("CASS_SEMANTIC_EMBEDDER", "unsupported-wire-test-model")
            .env("FRANKENSEARCH_MODEL_DIR", root.path().join("no-assets"))
            .current_dir(root.path())
            .output()?;
        anyhow::ensure!(result.status.success(), "wire fixture failed: stdout={} stderr={}",
            String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        anyhow::ensure!(String::from_utf8_lossy(&result.stdout).contains("1 passed; 0 failed"), "exact child test did not execute");
        return Ok(());
    }

    let root = tempfile::tempdir()?;
    let (running, mut stream) = RunningDaemon::start(root.path())?;
    let requests = [
        (Request::Embed { texts: vec!["private-input".into()], model: "default".into(), dims: Some(768) }, ErrorCode::InvalidInput),
        (Request::Embed { texts: vec!["private-input".into()], model: "unknown-model".into(), dims: None }, ErrorCode::ModelNotFound),
        (Request::Embed { texts: vec!["private-input".into(); 1025], model: "default".into(), dims: None }, ErrorCode::InvalidInput),
        (Request::Embed { texts: vec!["x".repeat(64 * 1024); 65], model: "default".into(), dims: None }, ErrorCode::InvalidInput),
        (Request::Rerank { query: "private-input".into(), documents: vec!["x".repeat(64 * 1024 + 1)], model: "default".into() }, ErrorCode::InvalidInput),
        (Request::Rerank { query: "private-input".into(), documents: vec!["doc".into()], model: "unknown-model".into() }, ErrorCode::ModelNotFound),
    ];
    for (index, (request, expected)) in requests.into_iter().enumerate() {
        let response = exchange(&mut stream, &format!("reject-{index}"), request)?;
        let Response::Error(error) = response else { anyhow::bail!("invalid inference unexpectedly succeeded"); };
        assert_eq!(error.code, expected);
        assert!(!error.retryable);
        assert!(error.retry_after_ms.is_none());
        assert!(!error.message.contains("private-input"));
        // Every refusal must leave framing and the next control request usable.
        assert!(matches!(exchange(&mut stream, "health", Request::Health)?, Response::Health(health) if !health.ready));
    }
    assert!(matches!(exchange(&mut stream, "stop", Request::Shutdown)?, Response::Shutdown { .. }));
    drop(stream);
    running.finish()?;
    assert!(!root.path().join("agent_search.db").exists());
    assert!(!root.path().join("models").exists());
    assert!(!root.path().join("s").exists());
    Ok(())
}
