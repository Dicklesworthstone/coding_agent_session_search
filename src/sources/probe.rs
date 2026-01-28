//! SSH host probing for remote source setup.
//!
//! This module provides functionality to probe SSH hosts and gather comprehensive
//! information needed for remote source configuration decisions:
//! - Whether cass is installed (and what version)
//! - Index status (session count)
//! - Detected agent session data directories
//! - System information (OS, architecture)
//! - Resource availability (disk space, memory)
//!
//! # Design
//!
//! Probing uses a single SSH session per host to minimize latency. A bash probe
//! script is piped to `bash -s` on the remote, gathering all information in one
//! round-trip.
//!
//! # Example
//!
//! ```rust,ignore
//! use coding_agent_search::sources::probe::{probe_host, probe_hosts_parallel};
//! use coding_agent_search::sources::config::DiscoveredHost;
//!
//! // Single host probe (returns HostProbeResult directly, not Result)
//! let host = DiscoveredHost { name: "laptop".into(), .. };
//! let result = probe_host(&host, 10);
//! if result.reachable {
//!     println!("Connected in {}ms", result.connection_time_ms);
//! }
//!
//! // Parallel probing with progress (synchronous, uses rayon internally)
//! let results = probe_hosts_parallel(&hosts, 10, |done, total, name| {
//!     println!("Probing {}/{}: {}", done, total, name);
//! });
//! ```

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::config::DiscoveredHost;

/// Default connection timeout in seconds.
pub const DEFAULT_PROBE_TIMEOUT: u64 = 10;

/// Result of probing an SSH host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProbeResult {
    /// SSH config host alias.
    pub host_name: String,
    /// Whether the host was reachable via SSH.
    pub reachable: bool,
    /// Connection time in milliseconds.
    pub connection_time_ms: u64,
    /// Status of cass installation on the remote.
    pub cass_status: CassStatus,
    /// Detected agent session directories.
    pub detected_agents: Vec<DetectedAgent>,
    /// System information.
    pub system_info: Option<SystemInfo>,
    /// Resource information (disk/memory).
    pub resources: Option<ResourceInfo>,
    /// Error message if probe failed.
    pub error: Option<String>,
}

impl HostProbeResult {
    /// Create a result for an unreachable host.
    pub fn unreachable(host_name: &str, error: impl Into<String>) -> Self {
        Self {
            host_name: host_name.to_string(),
            reachable: false,
            connection_time_ms: 0,
            cass_status: CassStatus::Unknown,
            detected_agents: Vec::new(),
            system_info: None,
            resources: None,
            error: Some(error.into()),
        }
    }

    /// Check if cass is installed on this host.
    pub fn has_cass(&self) -> bool {
        self.cass_status.is_installed()
    }

    /// Check if this host has any agent session data.
    pub fn has_agent_data(&self) -> bool {
        !self.detected_agents.is_empty()
    }

    /// Get total estimated sessions across all detected agents.
    pub fn total_sessions(&self) -> u64 {
        self.detected_agents
            .iter()
            .filter_map(|a| a.estimated_sessions)
            .sum()
    }
}

/// Status of cass installation on a remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CassStatus {
    /// cass is installed and has an indexed database.
    Indexed {
        version: String,
        session_count: u64,
        last_indexed: Option<String>,
    },
    /// cass is installed but no index exists or is empty.
    InstalledNotIndexed { version: String },
    /// cass is not found on PATH.
    NotFound,
    /// Couldn't determine cass status.
    Unknown,
}

impl CassStatus {
    /// Check if cass is installed (any version).
    pub fn is_installed(&self) -> bool {
        matches!(
            self,
            CassStatus::Indexed { .. } | CassStatus::InstalledNotIndexed { .. }
        )
    }

    /// Get the installed version if available.
    pub fn version(&self) -> Option<&str> {
        match self {
            CassStatus::Indexed { version, .. } | CassStatus::InstalledNotIndexed { version } => {
                Some(version)
            }
            _ => None,
        }
    }
}

/// Detected agent session data on a remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectedAgent {
    /// Type of agent (claude_code, codex, cursor, etc.).
    pub agent_type: String,
    /// Path to the agent's session directory.
    pub path: String,
    /// Estimated number of sessions (from file count).
    pub estimated_sessions: Option<u64>,
    /// Estimated size in megabytes.
    pub estimated_size_mb: Option<u64>,
}

/// System information gathered from remote host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    /// Operating system (linux, darwin).
    pub os: String,
    /// CPU architecture (x86_64, aarch64).
    pub arch: String,
    /// Linux distro name if available.
    pub distro: Option<String>,
    /// Whether cargo is available.
    pub has_cargo: bool,
    /// Whether cargo-binstall is available.
    pub has_cargo_binstall: bool,
    /// Whether curl is available.
    pub has_curl: bool,
    /// Whether wget is available.
    pub has_wget: bool,
    /// Remote home directory.
    pub remote_home: String,
    /// Unique machine identifier (for deduplication of SSH aliases).
    /// On Linux: /etc/machine-id, on macOS: IOPlatformUUID.
    #[serde(default)]
    pub machine_id: Option<String>,
}

/// Resource information for installation feasibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    /// Available disk space in MB (in home directory).
    pub disk_available_mb: u64,
    /// Total memory in MB.
    pub memory_total_mb: u64,
    /// Available memory in MB.
    pub memory_available_mb: u64,
    /// Heuristic: enough resources to compile Rust.
    pub can_compile: bool,
}

impl ResourceInfo {
    /// Minimum disk space (MB) recommended for cass installation.
    pub const MIN_DISK_MB: u64 = 1024; // 1 GB

    /// Minimum memory (MB) recommended for compilation.
    pub const MIN_MEMORY_MB: u64 = 2048; // 2 GB
}

/// Bash probe script that gathers all information in one SSH call.
///
/// Output format is key=value pairs, with special markers for sections.
const PROBE_SCRIPT: &str = r#"#!/bin/bash
echo "===PROBE_START==="

# System info
echo "OS=$(uname -s | tr '[:upper:]' '[:lower:]')"
echo "ARCH=$(uname -m)"
echo "HOME=$HOME"

# Distro detection (Linux only)
if [ -f /etc/os-release ]; then
    . /etc/os-release
    echo "DISTRO=$PRETTY_NAME"
fi

# Machine ID for deduplication of SSH aliases pointing to same host
# Linux: /etc/machine-id, macOS: IOPlatformUUID
if [ -f /etc/machine-id ]; then
    MACHINE_ID=$(cat /etc/machine-id 2>/dev/null | tr -d '\n')
    echo "MACHINE_ID=$MACHINE_ID"
elif command -v ioreg &> /dev/null; then
    MACHINE_ID=$(ioreg -rd1 -c IOPlatformExpertDevice 2>/dev/null | awk -F'"' '/IOPlatformUUID/{print $4}')
    echo "MACHINE_ID=$MACHINE_ID"
fi

# Cass status - check PATH and common install locations
# Non-interactive SSH doesn't source .bashrc, so user bin dirs may not be in PATH
CASS_BIN=""
if command -v cass &> /dev/null; then
    CASS_BIN="cass"
elif [ -x "$HOME/.cargo/bin/cass" ]; then
    CASS_BIN="$HOME/.cargo/bin/cass"
elif [ -x "$HOME/.local/bin/cass" ]; then
    CASS_BIN="$HOME/.local/bin/cass"
elif [ -x "/usr/local/bin/cass" ]; then
    CASS_BIN="/usr/local/bin/cass"
fi

if [ -n "$CASS_BIN" ]; then
    CASS_VER=$("$CASS_BIN" --version 2>/dev/null | head -1 | awk '{print $2}')
    if [ -z "$CASS_VER" ]; then
        # Binary exists but version command failed - treat as not found
        echo "CASS_VERSION=NOT_FOUND"
    else
        echo "CASS_VERSION=$CASS_VER"

        # Get health status (JSON output) - only if version was detected
        if "$CASS_BIN" health --json &>/dev/null; then
            echo "CASS_HEALTH=OK"
            # Try to get session count from stats
            STATS=$("$CASS_BIN" stats --json 2>/dev/null)
            if [ $? -eq 0 ] && [ -n "$STATS" ]; then
                # Extract total conversations from JSON (allow whitespace/newlines)
                SESSIONS=$(echo "$STATS" | tr -d '\n' | sed -n 's/.*"conversations"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p')
                echo "CASS_SESSIONS=${SESSIONS:-0}"
            else
                echo "CASS_SESSIONS=0"
            fi
        else
            echo "CASS_HEALTH=NOT_INDEXED"
        fi
    fi
else
    echo "CASS_VERSION=NOT_FOUND"
fi

# Tool availability - also check ~/.cargo/bin for non-interactive SSH sessions
if command -v cargo &> /dev/null || [ -x "$HOME/.cargo/bin/cargo" ]; then
    echo "HAS_CARGO=1"
else
    echo "HAS_CARGO=0"
fi
if command -v cargo-binstall &> /dev/null || [ -x "$HOME/.cargo/bin/cargo-binstall" ]; then
    echo "HAS_BINSTALL=1"
else
    echo "HAS_BINSTALL=0"
fi
command -v curl &> /dev/null && echo "HAS_CURL=1" || echo "HAS_CURL=0"
command -v wget &> /dev/null && echo "HAS_WGET=1" || echo "HAS_WGET=0"

# Resource info - disk (in KB, converted later)
DISK_KB=$(df -k ~ 2>/dev/null | awk 'NR==2 {print $4}')
echo "DISK_AVAIL_KB=${DISK_KB:-0}"

# Memory info (Linux)
if [ -f /proc/meminfo ]; then
    MEM_TOTAL=$(grep MemTotal /proc/meminfo 2>/dev/null | awk '{print $2}')
    MEM_AVAIL=$(grep MemAvailable /proc/meminfo 2>/dev/null | awk '{print $2}')
    echo "MEM_TOTAL_KB=${MEM_TOTAL:-0}"
    echo "MEM_AVAIL_KB=${MEM_AVAIL:-0}"
else
    # macOS - use sysctl
    if command -v sysctl &> /dev/null; then
        MEM_BYTES=$(sysctl -n hw.memsize 2>/dev/null)
        MEM_KB=$((MEM_BYTES / 1024))
        echo "MEM_TOTAL_KB=${MEM_KB:-0}"
        echo "MEM_AVAIL_KB=${MEM_KB:-0}"  # macOS doesn't have easy available mem
    fi
fi

# Agent data detection (with sizes and file counts)
for dir in ~/.claude/projects ~/.codex/sessions ~/.cursor \
           ~/.config/Code/User/globalStorage/saoudrizwan.claude-dev \
           ~/.config/Cursor/User/globalStorage/saoudrizwan.claude-dev \
           ~/Library/Application\ Support/Code/User/globalStorage/saoudrizwan.claude-dev \
           ~/Library/Application\ Support/Cursor/User/globalStorage/saoudrizwan.claude-dev \
           ~/.gemini/tmp ~/.pi/agent/sessions ~/.aider.chat.history.md \
           ~/.local/share/opencode ~/.goose/sessions ~/.continue/sessions; do
    # Expand the path
    expanded_dir=$(eval echo "$dir" 2>/dev/null)
    if [ -e "$expanded_dir" ]; then
        SIZE=$(du -sm "$expanded_dir" 2>/dev/null | cut -f1)
        # Count JSONL files for session estimate
        if [ -d "$expanded_dir" ]; then
            COUNT=$(find "$expanded_dir" \( -name "*.jsonl" -o -name "*.json" \) 2>/dev/null | wc -l | tr -d ' ')
        else
            COUNT=1  # Single file
        fi
        echo "AGENT_DATA=$dir|${SIZE:-0}|${COUNT:-0}"
    fi
done

echo "===PROBE_END==="
"#;

/// Probe a single SSH host.
///
/// Runs a comprehensive probe script via SSH to gather system info, cass status,
/// and detected agent data. Uses a single SSH session for efficiency.
///
/// # Arguments
/// * `host` - The discovered SSH host to probe
/// * `timeout_secs` - Connection timeout in seconds
///
/// # Returns
/// A `HostProbeResult` with all gathered information, or error details if probe failed.
pub fn probe_host(host: &DiscoveredHost, timeout_secs: u64) -> HostProbeResult {
    let start = Instant::now();

    // Build SSH command with appropriate options
    // Use UserKnownHostsFile=/dev/null to avoid polluting known_hosts during mass probing
    let ssh_opts = format!(
        "-o BatchMode=yes -o ConnectTimeout={} -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null",
        timeout_secs
    );

    // Use the host alias directly (SSH config handles Port, User, IdentityFile, ProxyJump, etc.)
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_opts.split_whitespace())
        .arg("--")
        .arg(&host.name)
        .arg("bash -s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Spawn the process and write probe script to stdin
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HostProbeResult::unreachable(
                &host.name,
                format!("Failed to execute ssh: {}", e),
            );
        }
    };

    // Write probe script to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        if let Err(e) = stdin.write_all(PROBE_SCRIPT.as_bytes()) {
            return HostProbeResult::unreachable(
                &host.name,
                format!("Failed to write probe script: {}", e),
            );
        }
    }

    // Wait for completion
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return HostProbeResult::unreachable(&host.name, format!("SSH command failed: {}", e));
        }
    };

    let connection_time_ms = start.elapsed().as_millis() as u64;

    // Check for SSH failures
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_msg = if stderr.contains("Connection refused") {
            "Connection refused".to_string()
        } else if stderr.contains("Connection timed out") || stderr.contains("timed out") {
            "Connection timed out".to_string()
        } else if stderr.contains("Permission denied") {
            "Permission denied (key not loaded in ssh-agent?)".to_string()
        } else if stderr.contains("Host key verification failed") {
            "Host key verification failed".to_string()
        } else if stderr.contains("No route to host") {
            "No route to host".to_string()
        } else {
            format!("SSH failed: {}", stderr.trim())
        };

        return HostProbeResult::unreachable(&host.name, error_msg);
    }

    // Parse successful output
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_probe_output(&host.name, &stdout, connection_time_ms)
}

/// Parse the probe script output into a HostProbeResult.
fn parse_probe_output(host_name: &str, output: &str, connection_time_ms: u64) -> HostProbeResult {
    let mut values: HashMap<String, String> = HashMap::new();
    let mut agent_data: Vec<(String, u64, u64)> = Vec::new(); // (path, size_mb, count)

    // Check for probe markers
    if !output.contains("===PROBE_START===") || !output.contains("===PROBE_END===") {
        return HostProbeResult::unreachable(host_name, "Probe script output malformed");
    }

    // Parse key=value pairs
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("===") {
            continue;
        }

        if line.starts_with("AGENT_DATA=") {
            // Special handling for agent data: AGENT_DATA=path|size|count
            if let Some(data) = line.strip_prefix("AGENT_DATA=") {
                // Use rsplitn to handle paths containing pipes (parse from right)
                // Yields: count, size, path
                let parts: Vec<&str> = data.rsplitn(3, '|').collect();
                if parts.len() == 3 {
                    let count = parts[0].parse().unwrap_or(0);
                    let size = parts[1].parse().unwrap_or(0);
                    let path = parts[2].to_string();
                    agent_data.push((path, size, count));
                }
            }
        } else if let Some((key, value)) = line.split_once('=') {
            values.insert(key.to_string(), value.to_string());
        }
    }

    // Build CassStatus
    let cass_status = if let Some(version) = values.get("CASS_VERSION") {
        if version == "NOT_FOUND" {
            CassStatus::NotFound
        } else {
            let health = values.get("CASS_HEALTH").map(|s| s.as_str());
            if health == Some("OK") {
                let sessions = values
                    .get("CASS_SESSIONS")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                CassStatus::Indexed {
                    version: version.clone(),
                    session_count: sessions,
                    last_indexed: None,
                }
            } else {
                CassStatus::InstalledNotIndexed {
                    version: version.clone(),
                }
            }
        }
    } else {
        CassStatus::Unknown
    };

    // Build SystemInfo
    let system_info = values.get("OS").map(|os| SystemInfo {
        os: os.clone(),
        arch: values.get("ARCH").cloned().unwrap_or_default(),
        distro: values.get("DISTRO").cloned(),
        has_cargo: values.get("HAS_CARGO").map(|v| v == "1").unwrap_or(false),
        has_cargo_binstall: values
            .get("HAS_BINSTALL")
            .map(|v| v == "1")
            .unwrap_or(false),
        has_curl: values.get("HAS_CURL").map(|v| v == "1").unwrap_or(false),
        has_wget: values.get("HAS_WGET").map(|v| v == "1").unwrap_or(false),
        remote_home: values.get("HOME").cloned().unwrap_or_default(),
        machine_id: values.get("MACHINE_ID").cloned().filter(|s| !s.is_empty()),
    });

    // Build ResourceInfo
    let resources = {
        let disk_kb = values
            .get("DISK_AVAIL_KB")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mem_total_kb = values
            .get("MEM_TOTAL_KB")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mem_avail_kb = values
            .get("MEM_AVAIL_KB")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        if disk_kb > 0 || mem_total_kb > 0 {
            let disk_mb = disk_kb / 1024;
            let mem_total_mb = mem_total_kb / 1024;
            let mem_avail_mb = mem_avail_kb / 1024;

            Some(ResourceInfo {
                disk_available_mb: disk_mb,
                memory_total_mb: mem_total_mb,
                memory_available_mb: mem_avail_mb,
                can_compile: disk_mb >= ResourceInfo::MIN_DISK_MB
                    && mem_total_mb >= ResourceInfo::MIN_MEMORY_MB,
            })
        } else {
            None
        }
    };

    // Build DetectedAgents
    let detected_agents: Vec<DetectedAgent> = agent_data
        .into_iter()
        .map(|(path, size_mb, count)| {
            let agent_type = infer_agent_type(&path);
            DetectedAgent {
                agent_type,
                path,
                estimated_sessions: Some(count),
                estimated_size_mb: Some(size_mb),
            }
        })
        .collect();

    HostProbeResult {
        host_name: host_name.to_string(),
        reachable: true,
        connection_time_ms,
        cass_status,
        detected_agents,
        system_info,
        resources,
        error: None,
    }
}

/// Infer agent type from path.
///
/// Note: More specific patterns must be checked first (e.g., `saoudrizwan.claude-dev`
/// contains `claude` so Cline must be checked before Claude Code).
fn infer_agent_type(path: &str) -> String {
    // Check Cline first - it contains "claude-dev" which could match ".claude"
    if path.contains("saoudrizwan.claude-dev") || path.contains("rooveterinaryinc.roo-cline") {
        "cline".to_string()
    } else if path.contains(".claude") {
        "claude_code".to_string()
    } else if path.contains(".codex") {
        "codex".to_string()
    } else if path.contains(".cursor") || path.contains("Cursor") {
        "cursor".to_string()
    } else if path.contains(".gemini") {
        "gemini".to_string()
    } else if path.contains(".pi") {
        "pi_agent".to_string()
    } else if path.contains(".aider") {
        "aider".to_string()
    } else if path.contains("opencode") {
        "opencode".to_string()
    } else if path.contains(".goose") {
        "goose".to_string()
    } else if path.contains(".continue") {
        "continue".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Probe multiple hosts in parallel.
///
/// Uses rayon's parallel iterator to probe hosts concurrently, calling the
/// progress callback as each probe completes.
///
/// # Arguments
/// * `hosts` - Slice of discovered hosts to probe
/// * `timeout_secs` - Connection timeout per host
/// * `on_progress` - Callback called after each host completes: (completed, total, host_name)
///
/// # Returns
/// Vector of probe results for all hosts.
pub fn probe_hosts_parallel<F>(
    hosts: &[DiscoveredHost],
    timeout_secs: u64,
    on_progress: F,
) -> Vec<HostProbeResult>
where
    F: Fn(usize, usize, &str) + Send + Sync,
{
    use rayon::prelude::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let total = hosts.len();
    let completed = Arc::new(AtomicUsize::new(0));
    let on_progress = Arc::new(on_progress);

    // Use rayon for true parallel execution
    hosts
        .par_iter()
        .map(|host| {
            let result = probe_host(host, timeout_secs);

            let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
            on_progress(done, total, &host.name);

            result
        })
        .collect()
}

/// Cache for probe results to avoid repeated probing.
///
/// Note: Use `ProbeCache::new(ttl_secs)` to create a cache. The `Default`
/// implementation uses a 5-minute TTL.
#[derive(Debug)]
pub struct ProbeCache {
    results: HashMap<String, (HostProbeResult, std::time::Instant)>,
    ttl_secs: u64,
}

impl Default for ProbeCache {
    fn default() -> Self {
        Self::new(300) // 5-minute default TTL
    }
}

impl ProbeCache {
    /// Create a new cache with the specified TTL in seconds.
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            results: HashMap::new(),
            ttl_secs,
        }
    }

    /// Get a cached result if still valid.
    pub fn get(&self, host_name: &str) -> Option<&HostProbeResult> {
        self.results.get(host_name).and_then(|(result, ts)| {
            if ts.elapsed().as_secs() < self.ttl_secs {
                Some(result)
            } else {
                None
            }
        })
    }

    /// Insert a result into the cache.
    pub fn insert(&mut self, result: HostProbeResult) {
        self.results.insert(
            result.host_name.clone(),
            (result, std::time::Instant::now()),
        );
    }

    /// Clear expired entries.
    pub fn clear_expired(&mut self) {
        self.results
            .retain(|_, (_, ts)| ts.elapsed().as_secs() < self.ttl_secs);
    }
}

/// Deduplicate probe results that point to the same physical machine.
///
/// Multiple SSH aliases may point to the same machine. This function identifies
/// duplicates using the machine_id from the probe and keeps only one entry per
/// physical machine.
///
/// # Selection criteria (when duplicates found)
/// 1. Prefer hosts with cass already installed
/// 2. Prefer hosts with more sessions indexed
/// 3. Otherwise, keep the first one alphabetically
///
/// # Returns
/// A tuple of (deduplicated results, merged aliases map).
/// The merged map contains: kept_host_name -> vec![merged_alias_names]
pub fn deduplicate_probe_results(
    results: Vec<HostProbeResult>,
) -> (Vec<HostProbeResult>, HashMap<String, Vec<String>>) {
    // Group by machine_id (skip hosts without machine_id - can't dedupe them)
    let mut by_machine_id: HashMap<String, Vec<HostProbeResult>> = HashMap::new();
    let mut no_machine_id: Vec<HostProbeResult> = Vec::new();

    for result in results {
        if let Some(ref machine_id) = result
            .system_info
            .as_ref()
            .and_then(|s| s.machine_id.clone())
        {
            by_machine_id
                .entry(machine_id.clone())
                .or_default()
                .push(result);
        } else {
            no_machine_id.push(result);
        }
    }

    let mut deduplicated: Vec<HostProbeResult> = Vec::new();
    let mut merged_aliases: HashMap<String, Vec<String>> = HashMap::new();

    // Process groups with machine_id
    for (_machine_id, mut group) in by_machine_id {
        if group.len() == 1 {
            deduplicated.push(group.remove(0));
        } else {
            // Multiple aliases for same machine - pick the best one
            group.sort_by(|a, b| {
                // 1. Prefer installed cass
                let a_installed = a.cass_status.is_installed();
                let b_installed = b.cass_status.is_installed();
                if a_installed != b_installed {
                    return b_installed.cmp(&a_installed);
                }

                // 2. Prefer more sessions
                let a_sessions = match &a.cass_status {
                    CassStatus::Indexed { session_count, .. } => *session_count,
                    _ => 0,
                };
                let b_sessions = match &b.cass_status {
                    CassStatus::Indexed { session_count, .. } => *session_count,
                    _ => 0,
                };
                if a_sessions != b_sessions {
                    return b_sessions.cmp(&a_sessions);
                }

                // 3. Alphabetically by name
                a.host_name.cmp(&b.host_name)
            });

            // Keep the first (best) one, record others as merged
            let kept = group.remove(0);
            let merged: Vec<String> = group.into_iter().map(|h| h.host_name).collect();

            if !merged.is_empty() {
                merged_aliases.insert(kept.host_name.clone(), merged);
            }
            deduplicated.push(kept);
        }
    }

    // Add back hosts without machine_id
    deduplicated.extend(no_machine_id);

    // Sort final list by name for consistent ordering
    deduplicated.sort_by(|a, b| a.host_name.cmp(&b.host_name));

    (deduplicated, merged_aliases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cass_status_is_installed() {
        assert!(
            CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 100,
                last_indexed: None
            }
            .is_installed()
        );

        assert!(
            CassStatus::InstalledNotIndexed {
                version: "0.1.50".into()
            }
            .is_installed()
        );

        assert!(!CassStatus::NotFound.is_installed());
        assert!(!CassStatus::Unknown.is_installed());
    }

    #[test]
    fn test_cass_status_version() {
        assert_eq!(
            CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 0,
                last_indexed: None
            }
            .version(),
            Some("0.1.50")
        );

        assert_eq!(
            CassStatus::InstalledNotIndexed {
                version: "0.1.49".into()
            }
            .version(),
            Some("0.1.49")
        );

        assert_eq!(CassStatus::NotFound.version(), None);
    }

    #[test]
    fn test_infer_agent_type() {
        assert_eq!(infer_agent_type("~/.claude/projects"), "claude_code");
        assert_eq!(infer_agent_type("~/.codex/sessions"), "codex");
        assert_eq!(infer_agent_type("~/.cursor"), "cursor");
        assert_eq!(infer_agent_type("~/.gemini/tmp"), "gemini");
        assert_eq!(
            infer_agent_type("~/.config/Code/User/globalStorage/saoudrizwan.claude-dev"),
            "cline"
        );
        assert_eq!(infer_agent_type("/some/random/path"), "unknown");
    }

    #[test]
    fn test_parse_probe_output_success() {
        let output = r#"
===PROBE_START===
OS=linux
ARCH=x86_64
HOME=/home/user
DISTRO=Ubuntu 22.04
CASS_VERSION=0.1.50
CASS_HEALTH=OK
CASS_SESSIONS=1234
HAS_CARGO=1
HAS_BINSTALL=0
HAS_CURL=1
HAS_WGET=1
DISK_AVAIL_KB=52428800
MEM_TOTAL_KB=16777216
MEM_AVAIL_KB=8388608
AGENT_DATA=~/.claude/projects|150|42
AGENT_DATA=~/.codex/sessions|50|10
===PROBE_END===
"#;

        let result = parse_probe_output("test-host", output, 100);

        assert!(result.reachable);
        assert_eq!(result.host_name, "test-host");
        assert_eq!(result.connection_time_ms, 100);

        // Check cass status
        match &result.cass_status {
            CassStatus::Indexed {
                version,
                session_count,
                ..
            } => {
                assert_eq!(version, "0.1.50");
                assert_eq!(*session_count, 1234);
            }
            _ => panic!("Expected Indexed status"),
        }

        // Check system info
        let sys = result.system_info.as_ref().unwrap();
        assert_eq!(sys.os, "linux");
        assert_eq!(sys.arch, "x86_64");
        assert_eq!(sys.distro, Some("Ubuntu 22.04".into()));
        assert!(sys.has_cargo);
        assert!(!sys.has_cargo_binstall);
        assert!(sys.has_curl);

        // Check resources
        let res = result.resources.as_ref().unwrap();
        assert_eq!(res.disk_available_mb, 51200); // 52428800 / 1024
        assert_eq!(res.memory_total_mb, 16384); // 16777216 / 1024
        assert!(res.can_compile);

        // Check detected agents
        assert_eq!(result.detected_agents.len(), 2);
        assert_eq!(result.detected_agents[0].agent_type, "claude_code");
        assert_eq!(result.detected_agents[0].estimated_sessions, Some(42));
        assert_eq!(result.detected_agents[1].agent_type, "codex");
    }

    #[test]
    fn test_parse_probe_output_cass_not_found() {
        let output = r#"
===PROBE_START===
OS=darwin
ARCH=arm64
HOME=/Users/user
CASS_VERSION=NOT_FOUND
HAS_CARGO=0
HAS_BINSTALL=0
HAS_CURL=1
HAS_WGET=0
DISK_AVAIL_KB=10240000
MEM_TOTAL_KB=8388608
MEM_AVAIL_KB=4194304
===PROBE_END===
"#;

        let result = parse_probe_output("mac-host", output, 50);

        assert!(result.reachable);
        assert!(matches!(result.cass_status, CassStatus::NotFound));

        let sys = result.system_info.as_ref().unwrap();
        assert_eq!(sys.os, "darwin");
        assert_eq!(sys.arch, "arm64");
        assert!(!sys.has_cargo);
    }

    #[test]
    fn test_parse_probe_output_malformed() {
        let output = "random garbage";
        let result = parse_probe_output("bad-host", output, 0);

        assert!(!result.reachable);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_host_probe_result_unreachable() {
        let result = HostProbeResult::unreachable("test", "Connection refused");

        assert!(!result.reachable);
        assert_eq!(result.error, Some("Connection refused".into()));
        assert!(!result.has_cass());
        assert!(!result.has_agent_data());
    }

    #[test]
    fn test_probe_cache() {
        let mut cache = ProbeCache::new(300); // 5 minute TTL

        let result = HostProbeResult {
            host_name: "test".into(),
            reachable: true,
            connection_time_ms: 100,
            cass_status: CassStatus::NotFound,
            detected_agents: vec![],
            system_info: None,
            resources: None,
            error: None,
        };

        cache.insert(result);

        assert!(cache.get("test").is_some());
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn test_resource_info_can_compile() {
        let good = ResourceInfo {
            disk_available_mb: 2000,
            memory_total_mb: 4000,
            memory_available_mb: 2000,
            can_compile: true,
        };
        assert!(good.can_compile);

        let low_disk = ResourceInfo {
            disk_available_mb: 500,
            memory_total_mb: 4000,
            memory_available_mb: 2000,
            can_compile: false,
        };
        assert!(!low_disk.can_compile);
    }

    // =========================================================================
    // Real system probe tests — run PROBE_SCRIPT locally without SSH
    // =========================================================================

    /// Execute PROBE_SCRIPT on the local system via bash, returning stdout.
    fn run_probe_script_locally() -> String {
        use std::io::Write;
        let mut cmd = Command::new("bash");
        cmd.arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Ensure HOME is set for the probe script (may not be set in some test environments)
        if std::env::var("HOME").is_err()
            && let Some(dirs) = directories::BaseDirs::new()
        {
            cmd.env("HOME", dirs.home_dir());
        }
        let mut child = cmd.spawn().expect("bash should be available");
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(PROBE_SCRIPT.as_bytes())
                .expect("write probe script");
        }
        let output = child
            .wait_with_output()
            .expect("probe script should finish");
        assert!(
            output.status.success(),
            "probe script failed: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    #[test]
    fn real_probe_script_produces_valid_markers() {
        let output = run_probe_script_locally();
        assert!(
            output.contains("===PROBE_START==="),
            "missing PROBE_START marker"
        );
        assert!(
            output.contains("===PROBE_END==="),
            "missing PROBE_END marker"
        );
    }

    #[test]
    fn real_probe_script_parses_into_reachable_result() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        assert!(
            result.reachable,
            "local probe should be reachable: {:?}",
            result.error
        );
        assert!(result.system_info.is_some(), "should have system info");
        assert!(result.resources.is_some(), "should have resource info");
    }

    #[test]
    fn real_probe_system_info_has_valid_os() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        assert!(
            sys.os == "linux" || sys.os == "darwin",
            "OS should be linux or darwin, got: {}",
            sys.os
        );
    }

    #[test]
    fn real_probe_system_info_has_valid_arch() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        let valid_archs = [
            "x86_64", "aarch64", "arm64", "armv7l", "i686", "s390x", "ppc64le",
        ];
        assert!(
            valid_archs.contains(&sys.arch.as_str()),
            "arch should be a known value, got: {}",
            sys.arch
        );
    }

    #[test]
    fn real_probe_system_info_has_nonempty_home() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        assert!(!sys.remote_home.is_empty(), "home should not be empty");
        assert!(
            sys.remote_home.starts_with('/'),
            "home should be absolute: {}",
            sys.remote_home
        );
    }

    #[test]
    fn real_probe_resources_have_nonzero_disk() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        assert!(res.disk_available_mb > 0, "disk_available_mb should be > 0");
    }

    #[test]
    fn real_probe_resources_have_nonzero_memory() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        assert!(res.memory_total_mb > 0, "memory_total_mb should be > 0");
        assert!(
            res.memory_available_mb > 0,
            "memory_available_mb should be > 0"
        );
    }

    #[test]
    fn real_probe_resources_memory_invariant() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        assert!(
            res.memory_available_mb <= res.memory_total_mb,
            "available memory ({}) should not exceed total ({})",
            res.memory_available_mb,
            res.memory_total_mb
        );
    }

    #[test]
    fn real_probe_resources_can_compile_reflects_thresholds() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let res = result.resources.as_ref().expect("resources");
        let expected = res.disk_available_mb >= ResourceInfo::MIN_DISK_MB
            && res.memory_total_mb >= ResourceInfo::MIN_MEMORY_MB;
        assert_eq!(
            res.can_compile, expected,
            "can_compile should match threshold check: disk={}MB mem={}MB",
            res.disk_available_mb, res.memory_total_mb
        );
    }

    #[test]
    fn real_probe_tool_detection_is_consistent() {
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");
        // If cargo-binstall is available, cargo must also be available
        if sys.has_cargo_binstall {
            assert!(sys.has_cargo, "binstall requires cargo");
        }
        // At least one download tool should exist on any modern system
        assert!(
            sys.has_curl || sys.has_wget,
            "system should have at least curl or wget"
        );
    }

    // =========================================================================
    // Deduplication tests
    // =========================================================================

    fn make_probe_result(
        name: &str,
        machine_id: Option<&str>,
        sessions: Option<u64>,
    ) -> HostProbeResult {
        HostProbeResult {
            host_name: name.to_string(),
            reachable: true,
            connection_time_ms: 100,
            cass_status: if let Some(s) = sessions {
                CassStatus::Indexed {
                    version: "0.1.50".into(),
                    session_count: s,
                    last_indexed: None,
                }
            } else {
                CassStatus::NotFound
            },
            detected_agents: vec![],
            system_info: Some(SystemInfo {
                os: "linux".into(),
                arch: "x86_64".into(),
                distro: Some("Ubuntu 25.10".into()),
                has_cargo: true,
                has_cargo_binstall: false,
                has_curl: true,
                has_wget: true,
                remote_home: "/home/ubuntu".into(),
                machine_id: machine_id.map(String::from),
            }),
            resources: Some(ResourceInfo {
                disk_available_mb: 800_000,
                memory_total_mb: 16_000,
                memory_available_mb: 8_000,
                can_compile: true,
            }),
            error: None,
        }
    }

    #[test]
    fn test_deduplicate_no_duplicates() {
        let results = vec![
            make_probe_result("host1", Some("machine-1"), Some(100)),
            make_probe_result("host2", Some("machine-2"), Some(200)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 2);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_deduplicate_same_machine() {
        // Two SSH aliases for the same machine
        let results = vec![
            make_probe_result("jain", Some("abc123"), None),
            make_probe_result("jain_ovh_box", Some("abc123"), None),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // Should keep "jain" (alphabetically first since neither has cass)
        assert_eq!(deduped[0].host_name, "jain");
        assert_eq!(
            merged.get("jain").unwrap(),
            &vec!["jain_ovh_box".to_string()]
        );
    }

    #[test]
    fn test_deduplicate_prefers_installed_cass() {
        // Two aliases, one with cass installed
        let results = vec![
            make_probe_result("alias_a", Some("machine-x"), None), // no cass
            make_probe_result("alias_b", Some("machine-x"), Some(500)), // has cass
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // Should keep alias_b because it has cass installed
        assert_eq!(deduped[0].host_name, "alias_b");
        assert!(merged.contains_key("alias_b"));
    }

    #[test]
    fn test_deduplicate_prefers_more_sessions() {
        // Both have cass, but different session counts
        let results = vec![
            make_probe_result("host_low", Some("machine-y"), Some(50)),
            make_probe_result("host_high", Some("machine-y"), Some(500)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // Should keep host_high because it has more sessions
        assert_eq!(deduped[0].host_name, "host_high");
        // Verify the merge recorded the merged alias
        assert!(merged.contains_key("host_high"));
    }

    #[test]
    fn test_deduplicate_no_machine_id_not_merged() {
        // Hosts without machine_id should not be merged
        let results = vec![
            make_probe_result("host1", None, Some(100)),
            make_probe_result("host2", None, Some(200)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 2);
        assert!(merged.is_empty());
    }

    #[test]
    fn test_deduplicate_mixed_with_and_without_machine_id() {
        let results = vec![
            make_probe_result("aliasA", Some("same-machine"), Some(100)),
            make_probe_result("aliasB", Some("same-machine"), Some(50)),
            make_probe_result("standalone", None, Some(75)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        // 2 hosts: one from deduplication, one standalone
        assert_eq!(deduped.len(), 2);
        // aliasA should be kept (more sessions)
        assert!(deduped.iter().any(|h| h.host_name == "aliasA"));
        assert!(deduped.iter().any(|h| h.host_name == "standalone"));
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn test_deduplicate_three_aliases_same_machine() {
        let results = vec![
            make_probe_result("alias1", Some("same"), Some(100)),
            make_probe_result("alias2", Some("same"), Some(200)),
            make_probe_result("alias3", Some("same"), Some(150)),
        ];

        let (deduped, merged) = deduplicate_probe_results(results);

        assert_eq!(deduped.len(), 1);
        // alias2 has the most sessions
        assert_eq!(deduped[0].host_name, "alias2");
        // The merged list should contain the other two aliases
        let merged_list = merged.get("alias2").unwrap();
        assert_eq!(merged_list.len(), 2);
        assert!(merged_list.contains(&"alias1".to_string()));
        assert!(merged_list.contains(&"alias3".to_string()));
    }

    #[test]
    fn real_probe_machine_id_present() {
        // Test that the local probe script actually collects machine_id
        let output = run_probe_script_locally();
        let result = parse_probe_output("localhost", &output, 0);
        let sys = result.system_info.as_ref().expect("system_info");

        // On Linux or macOS, we should get a machine_id
        // (this test may be skipped on unusual systems)
        if sys.os == "linux" || sys.os == "darwin" {
            assert!(
                sys.machine_id.is_some(),
                "machine_id should be present on {}",
                sys.os
            );
            let mid = sys.machine_id.as_ref().unwrap();
            assert!(!mid.is_empty(), "machine_id should not be empty");
            // Machine IDs are typically 32+ hex chars or UUID format
            assert!(
                mid.len() >= 32,
                "machine_id should be at least 32 chars, got: {}",
                mid
            );
        }
    }
}
