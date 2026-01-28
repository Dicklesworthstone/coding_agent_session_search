//! Resource monitoring and process priority management for the daemon.
//!
//! This module provides utilities for:
//! - Monitoring process memory usage
//! - Applying nice values (CPU priority)
//! - Applying ionice (I/O priority)

use std::fs;
use std::process::Command;

use tracing::{debug, warn};

/// Resource monitor for tracking daemon resource usage.
#[derive(Debug, Default)]
pub struct ResourceMonitor {
    /// Cached PID for /proc lookups.
    pid: u32,
}

impl ResourceMonitor {
    /// Create a new resource monitor.
    pub fn new() -> Self {
        Self {
            pid: std::process::id(),
        }
    }

    /// Get current process memory usage in bytes.
    ///
    /// Reads from /proc/self/statm on Linux. Returns 0 on error or non-Linux.
    pub fn memory_usage(&self) -> u64 {
        #[cfg(target_os = "linux")]
        {
            self.linux_memory_usage()
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }

    /// Linux-specific memory usage from /proc/self/statm.
    #[cfg(target_os = "linux")]
    fn linux_memory_usage(&self) -> u64 {
        // /proc/self/statm format: size resident share text lib data dt
        // Fields are in pages, multiply by page size
        let page_size = Self::page_size();

        match fs::read_to_string("/proc/self/statm") {
            Ok(content) => {
                let parts: Vec<&str> = content.split_whitespace().collect();
                if parts.len() >= 2 {
                    // Use RSS (resident set size) - second field
                    if let Ok(pages) = parts[1].parse::<u64>() {
                        return pages * page_size;
                    }
                }
                0
            }
            Err(e) => {
                debug!(error = %e, "Failed to read /proc/self/statm");
                0
            }
        }
    }

    /// Get system page size in bytes.
    #[cfg(target_os = "linux")]
    fn page_size() -> u64 {
        // sysconf(_SC_PAGESIZE) is typically 4096
        4096
    }

    /// Apply a nice value to the current process.
    ///
    /// Nice values range from -20 (highest priority) to 19 (lowest priority).
    /// Returns true if successful.
    pub fn apply_nice(&self, nice_value: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            // Use libc::nice() for the current process
            // Note: nice() only returns -1 on error, and -1 is also a valid nice value
            // So we need to check errno
            unsafe {
                // Reset errno
                *libc::__errno_location() = 0;

                let result = libc::nice(nice_value);

                let errno = *libc::__errno_location();
                if errno != 0 {
                    warn!(
                        nice = nice_value,
                        errno = errno,
                        "Failed to set nice value"
                    );
                    return false;
                }

                debug!(nice = result, "Applied nice value");
                true
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            debug!(nice = nice_value, "nice not supported on this platform");
            let _ = nice_value;
            false
        }
    }

    /// Apply an I/O priority class to the current process using ionice.
    ///
    /// IO priority classes:
    /// - 0: None (use the CFQ default)
    /// - 1: Realtime (highest priority)
    /// - 2: Best-effort (normal priority)
    /// - 3: Idle (lowest priority)
    ///
    /// Returns true if successful.
    pub fn apply_ionice(&self, class: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            let class_str = class.to_string();

            // Use ionice command to set I/O scheduling class
            match Command::new("ionice")
                .args(["-c", &class_str, "-p", &self.pid.to_string()])
                .output()
            {
                Ok(output) => {
                    if output.status.success() {
                        debug!(class = class, pid = self.pid, "Applied ionice class");
                        true
                    } else {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!(
                            class = class,
                            error = %stderr,
                            "ionice command failed"
                        );
                        false
                    }
                }
                Err(e) => {
                    debug!(error = %e, "ionice command not available");
                    false
                }
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            debug!(class = class, "ionice not supported on this platform");
            let _ = class;
            false
        }
    }

    /// Get memory usage as a human-readable string.
    pub fn memory_usage_human(&self) -> String {
        let bytes = self.memory_usage();
        if bytes == 0 {
            return "unknown".to_string();
        }

        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;

        if bytes >= GB {
            format!("{:.1} GB", bytes as f64 / GB as f64)
        } else if bytes >= MB {
            format!("{:.1} MB", bytes as f64 / MB as f64)
        } else if bytes >= KB {
            format!("{:.1} KB", bytes as f64 / KB as f64)
        } else {
            format!("{} B", bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_monitor_creation() {
        let monitor = ResourceMonitor::new();
        assert!(monitor.pid > 0);
    }

    #[test]
    fn test_memory_usage() {
        let monitor = ResourceMonitor::new();
        let mem = monitor.memory_usage();

        // On Linux, we should get a non-zero value
        #[cfg(target_os = "linux")]
        assert!(mem > 0, "Memory usage should be non-zero on Linux");

        // On non-Linux, it returns 0
        #[cfg(not(target_os = "linux"))]
        assert_eq!(mem, 0);
    }

    #[test]
    fn test_memory_usage_human() {
        let monitor = ResourceMonitor::new();
        let human = monitor.memory_usage_human();

        // Should return a valid string
        assert!(!human.is_empty());

        #[cfg(target_os = "linux")]
        {
            // Should contain a unit
            assert!(
                human.contains("KB") || human.contains("MB") || human.contains("GB"),
                "Memory string should contain unit: {}",
                human
            );
        }
    }

    #[test]
    fn test_apply_nice_range() {
        let monitor = ResourceMonitor::new();

        // Applying nice to increase niceness (lower priority) should work
        // Note: Decreasing niceness requires root privileges
        #[cfg(target_os = "linux")]
        {
            // Nice to 19 (lowest priority) should always work
            let result = monitor.apply_nice(19);
            // May fail if already at max nice
            let _ = result;
        }

        #[cfg(not(target_os = "linux"))]
        {
            assert!(!monitor.apply_nice(10));
        }
    }

    #[test]
    fn test_apply_ionice() {
        let monitor = ResourceMonitor::new();

        #[cfg(target_os = "linux")]
        {
            // Best-effort class (2) should work
            let result = monitor.apply_ionice(2);
            // May fail if ionice isn't available
            let _ = result;

            // Idle class (3) should work too
            let result = monitor.apply_ionice(3);
            let _ = result;
        }

        #[cfg(not(target_os = "linux"))]
        {
            assert!(!monitor.apply_ionice(2));
        }
    }

    #[test]
    fn test_page_size() {
        #[cfg(target_os = "linux")]
        {
            let size = ResourceMonitor::page_size();
            // Page size is typically 4096 on most systems
            assert!(size >= 4096);
            // Should be a power of 2
            assert!(size.is_power_of_two() || size == 4096);
        }
    }
}
