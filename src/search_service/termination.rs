//! Last-resort termination for a read-only worker whose supervisor has fired.
//!
//! `std::process::exit` runs runtime cleanup and C `atexit` handlers. Those
//! handlers can wait on the very native thread or output lock that timed out,
//! defeating the watchdog. Never enter application cleanup on this path.
//! Ordinary successful requests still drop/join their owners normally.
//!
//! This avoids user-space cleanup, not OS scheduling or pending-I/O latency.
//! In particular Windows may wait for the kernel to cancel pending I/O. No
//! process-local watchdog can promise progress against a stalled kernel.

/// Terminate this process and its threads without running cleanup handlers.
/// The caller has already decided that the worker cannot safely continue.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn terminate(code: i32) -> ! {
    unsafe extern "C" {
        fn _exit(status: std::ffi::c_int) -> !;
    }
    // SAFETY: POSIX _exit accepts every int, retains no pointers, and terminates
    // the calling process. Unlike exit, it does not invoke atexit handlers or
    // flush stdio. Kernel teardown, not Rust destructors, releases reader locks.
    unsafe { _exit(code) }
}

/// Windows must bypass DLL process-detach callbacks as well as CRT cleanup.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn terminate(code: i32) -> ! {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetCurrentProcess"]
        fn current_process() -> *mut std::ffi::c_void;
        #[link_name = "TerminateProcess"]
        fn terminate_process(process: *mut std::ffi::c_void, exit_code: u32) -> i32;
    }
    // SAFETY: the pseudo-handle denotes only this process and needs no close.
    // TerminateProcess terminates all its threads without DLL detach callbacks;
    // success on the current process never returns. Preserve all exit-code bits.
    unsafe {
        terminate_process(current_process(), u32::from_ne_bytes(code.to_ne_bytes()));
    }
    // If the OS unexpectedly rejects self-termination, do not fall back to
    // normal exit (which may deadlock). Abort rather than resume unsafe work.
    std::process::abort()
}

// CASS's supported native targets are Unix and Windows. Preserve prior
// behavior on other targets without asserting the native no-cleanup contract.
#[cfg(not(any(unix, windows)))]
pub(crate) fn terminate(code: i32) -> ! {
    std::process::exit(code)
}

/// Install only in a disposable test child: normal exit must now hang. This
/// makes replacing immediate termination with std::process::exit fail the
/// subprocess regressions instead of passing on an otherwise quiet runtime.
#[cfg(all(test, unix))]
#[allow(unsafe_code)]
pub(crate) fn install_blocked_exit_handler() {
    extern "C" fn blocked_cleanup() {
        loop {
            std::thread::park();
        }
    }
    unsafe extern "C" {
        fn atexit(callback: extern "C" fn()) -> std::ffi::c_int;
    }
    // SAFETY: the callback has static code lifetime, takes no arguments, never
    // unwinds, and is registered only inside a subprocess intentionally killed
    // by its supervisor. The parent always has its own external kill/reap bound.
    assert_eq!(unsafe { atexit(blocked_cleanup) }, 0);
}

#[cfg(all(test, any(unix, windows)))]
#[path = "termination_tests.rs"]
mod tests;
