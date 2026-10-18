//! A full stdout socket must not pin a reader or a worker beyond its budget.
//! UnixStream gives a deterministic, pre-filled blocking output sink without
//! assuming the platform's anonymous-pipe capacity or adding test hooks.

use super::*;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

fn blocked_output() -> Result<(UnixStream, Stdio)> {
    let (reader, mut writer) = UnixStream::pair()?;
    writer.set_nonblocking(true)?;
    let mut written = 0;
    loop {
        match writer.write(&[b'x'; 4096]) {
            Ok(0) => anyhow::bail!("output fixture closed while filling"),
            Ok(count) => {
                written += count;
                ensure!(written <= 2 * 1024 * 1024, "output fixture exceeded its memory bound");
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(written > 0, "output fixture was never filled");
    writer.set_nonblocking(false)?;
    Ok((reader, Stdio::from(OwnedFd::from(writer))))
}

fn deadline_releases_pool_after_blocked_output(one_shot: bool) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let (index, _, _) = fixture(temp.path())?;
    // Establish a real successful query with the same index before blocking
    // output. A parser/index failure is not an acceptable test substitute.
    let (status, out, err) = execute(temp.path(), &index,
        &["--query", "git fixture", "--limit", "1"], 10_000_000)?;
    ensure!(status.success(), "control failed: {}", String::from_utf8_lossy(&err));
    assert_eq!(serde_json::from_slice::<Value>(&out)?["result"]["count"], 1);

    let pool = temp.path().join("pool");
    let (_undrained_reader, output) = blocked_output()?;
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
    command.args(["serve", "--index"]).arg(&index)
        .args(["--request-timeout-ms", "2000", "--admission-dir"]).arg(&pool)
        .args(["--admission-slots", "1"])
        .current_dir(temp.path())
        .env("CASS_QUILL_QUERY_FUEL_BUDGET", "10000000")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("RUST_LOG", "error")
        .stdin(Stdio::piped())
        .stdout(output)
        .stderr(Stdio::null());
    if one_shot {
        command.args(["--query", "git fixture", "--limit", "1"]);
    } else {
        command.arg("--stdio");
    }
    let mut child = Child(command.spawn()?);
    let mut input = child.0.stdin.take().context("stdin")?;
    if !one_shot {
        writeln!(input, "{}", json!({"id":1,"op":"search","query":"git fixture","limit":1}))?;
    }
    // Do not drain stdout or close stdin to rescue the worker. Its own
    // deadline must terminate even a blocked flush, and the kernel must free
    // any pool lease when the process dies.
    let status = child.0.wait_timeout(Duration::from_secs(12))?
        .context("blocked output escaped request supervision")?;
    assert_eq!(status.code(), Some(124));
    drop(input);
    let policy = std::fs::read(pool.join("policy-v1"))?;
    assert_eq!(policy, b"CASS-READER-POOL-1\nslots=1\n");
    let slot = std::fs::OpenOptions::new().read(true).write(true)
        .open(pool.join("reader-00.lock"))?;
    assert!(slot.try_lock().is_ok(), "terminated worker leaked its reader slot");
    drop(slot);
    assert_eq!(std::fs::read(pool.join("policy-v1"))?, policy);
    Ok(())
}

#[test]
fn one_shot_deadline_covers_blocked_output_and_releases_admission() -> Result<()> {
    deadline_releases_pool_after_blocked_output(true)
}

#[test]
fn stdio_deadline_covers_blocked_output_and_releases_admission() -> Result<()> {
    deadline_releases_pool_after_blocked_output(false)
}
