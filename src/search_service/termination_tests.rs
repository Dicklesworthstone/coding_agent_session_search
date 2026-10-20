//! Exercise the production primitive from disposable Rust test processes.
//! No engine, archive, model or parent environment mutation is involved.

use super::*;
use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[cfg(any(unix, windows))]
fn supervisor_exit_codes_preserve_status_and_release_kernel_locks() -> io::Result<()> {
    // Keep the supervisor gate std-only. As in the existing memory tests,
    // retain the tiny lease fixture rather than adding cleanup I/O to exit.
    let root = std::env::temp_dir().join(format!(
        "cass-termination-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root)?;
    let module = module_path!().split_once("::").unwrap().1;
    for code in [124, 125, 126] {
        let path = root.join(format!("worker-{code}"));
        std::fs::create_dir(&path)?;
        let mut child = OwnedChild(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    &format!("{module}::terminated_owner_fixture"),
                    "--ignored",
                    "--nocapture",
                ])
                .env("CASS_TERMINATION_TEST_DIR", &path)
                .env("CASS_TERMINATION_TEST_CODE", code.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let expires = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            assert!(
                Instant::now() < expires,
                "termination entered blocking cleanup"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(code));
        assert_eq!(
            std::fs::read(path.join("ready"))?,
            b"lease and stdio locks held"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.join("reader.lock"))?;
        assert!(
            lock.try_lock().is_ok(),
            "kernel did not release the terminated reader's lock"
        );
    }
    Ok(())
}

#[test]
#[cfg(any(unix, windows))]
#[ignore = "subprocess fixture: holds locks and terminates without destructors"]
fn terminated_owner_fixture() {
    let path = PathBuf::from(std::env::var_os("CASS_TERMINATION_TEST_DIR").unwrap());
    let code = std::env::var_os("CASS_TERMINATION_TEST_CODE")
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<i32>()
        .unwrap();
    assert!([124, 125, 126].contains(&code));
    let lock = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path.join("reader.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    #[cfg(unix)]
    install_blocked_exit_handler();
    let stdout = io::stdout();
    let stderr = io::stderr();
    let _out = stdout.lock();
    let _err = stderr.lock();
    std::fs::write(path.join("ready"), b"lease and stdio locks held").unwrap();
    // The primitive must end the process even from a different thread than
    // the one holding native/stdio owners. No owner is unlocked manually.
    let killer: std::thread::JoinHandle<()> = std::thread::spawn(move || terminate(code));
    killer.join().unwrap();
    panic!("the production termination primitive returned");
}
