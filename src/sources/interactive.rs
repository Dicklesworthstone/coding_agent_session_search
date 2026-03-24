//! Interactive terminal prompts for the remote sources setup wizard.
//!
//! This module provides rich interactive components using dialoguer, including:
//! - Multi-select host picker with multi-line item display
//! - Confirmation prompts for destructive operations
//!
//! # Design Decision: dialoguer vs inquire
//!
//! We chose dialoguer because:
//! 1. It integrates well with indicatif (already used for progress bars)
//! 2. It's actively maintained and widely used
//! 3. It supports ANSI styling in items via the console crate
//!
//! # Multi-line Item Display
//!
//! Standard dialoguer MultiSelect shows single-line items. We achieve multi-line
//! display by embedding ANSI escape sequences and newlines directly in item strings:
//!
//! ```text
//! [x] css
//!     209.145.54.164 • ubuntu
//!     ✓ cass v0.1.50 installed • 1,234 sessions
//!     Claude ✓  Codex ✓  Cursor ✓
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use coding_agent_search::sources::interactive::{
//!     HostSelector, HostDisplayInfo, HostState, CassStatusDisplay
//! };
//!
//! let hosts = vec![
//!     HostDisplayInfo {
//!         name: "css".into(),
//!         hostname: "209.145.54.164".into(),
//!         username: "ubuntu".into(),
//!         cass_status: CassStatusDisplay::Installed { version: "0.1.50".into(), sessions: 1234 },
//!         detected_agents: vec!["claude".into(), "codex".into()],
//!         reachable: true,
//!         error: None,
//!         state: HostState::ReadyToSync,
//!         system_info: Some("ubuntu 22.04 • 45GB free".into()),
//!     },
//!     // ... more hosts
//! ];
//!
//! let selector = HostSelector::new(hosts);
//! let selected = selector.prompt()?;
//! ```

use std::collections::HashSet;
use std::fmt;
use std::io::IsTerminal;

use colored::Colorize;
use dialoguer::{Confirm, MultiSelect, theme::ColorfulTheme};

use super::probe::{CassStatus, HostProbeResult};

// =============================================================================
// Types
// =============================================================================

/// State of a host for selection purposes.
///
/// Determines how the host appears in the UI and whether it's selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostState {
    /// cass installed and indexed - ready to sync immediately.
    ReadyToSync,
    /// cass installed but needs indexing.
    NeedsIndexing,
    /// cass not found - needs installation.
    NeedsInstall,
    /// SSH connection failed.
    Unreachable,
    /// Already configured in sources.toml.
    AlreadyConfigured,
}

impl HostState {
    /// Get the status badge for display (right-aligned).
    pub fn status_badge(&self) -> String {
        match self {
            HostState::ReadyToSync => format!("{} Ready to sync", "✓".green()),
            HostState::NeedsIndexing => format!("{} Needs indexing", "⚡".yellow()),
            HostState::NeedsInstall => format!("{} Needs install", "⚠".yellow()),
            HostState::Unreachable => format!("{} Unreachable", "✗".red()),
            HostState::AlreadyConfigured => format!("{} Already setup", "═".cyan()),
        }
    }

    /// Check if this host state is selectable.
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            HostState::ReadyToSync | HostState::NeedsIndexing | HostState::NeedsInstall
        )
    }

    /// Check if this host should be pre-selected.
    pub fn should_preselect(&self) -> bool {
        // Pre-select ready and needs-indexing hosts; don't pre-select needs-install
        matches!(self, HostState::ReadyToSync | HostState::NeedsIndexing)
    }
}

/// Display information for a remote host in the selection UI.
#[derive(Debug, Clone)]
pub struct HostDisplayInfo {
    /// SSH config name (e.g., "css", "laptop")
    pub name: String,
    /// IP address or hostname
    pub hostname: String,
    /// SSH username
    pub username: String,
    /// cass installation status on this host
    pub cass_status: CassStatusDisplay,
    /// Detected coding agents on this host
    pub detected_agents: Vec<String>,
    /// Whether this host is reachable
    pub reachable: bool,
    /// Optional error message if unreachable
    pub error: Option<String>,
    /// Host state for selection purposes
    pub state: HostState,
    /// OS and free disk space info
    pub system_info: Option<String>,
}

/// cass installation status for display purposes.
#[derive(Debug, Clone)]
pub enum CassStatusDisplay {
    /// cass is installed with known version and session count
    Installed { version: String, sessions: u64 },
    /// cass is installed but not indexed
    InstalledNotIndexed { version: String },
    /// cass is not installed but agent data was detected
    NotInstalled,
    /// Could not determine status (e.g., probe failed)
    Unknown,
}

/// Result of host selection.
#[derive(Debug, Clone)]
pub struct HostSelectionResult {
    /// Indices of selected hosts in the original hosts list.
    pub selected_indices: Vec<usize>,
    /// Hosts that need cass installation.
    pub needs_install: Vec<usize>,
    /// Hosts that have cass but need indexing.
    pub needs_indexing: Vec<usize>,
    /// Hosts ready for sync (cass installed and indexed).
    pub ready_for_sync: Vec<usize>,
}

// =============================================================================
// Host Selector
// =============================================================================

/// Interactive multi-select host picker with rich display.
pub struct HostSelector {
    hosts: Vec<HostDisplayInfo>,
    theme: ColorfulTheme,
}

impl HostSelector {
    /// Create a new host selector with the given hosts.
    pub fn new(hosts: Vec<HostDisplayInfo>) -> Self {
        Self {
            hosts,
            theme: ColorfulTheme::default(),
        }
    }

    /// Format a single host for multi-line display.
    ///
    /// Returns a string with ANSI formatting suitable for terminal display.
    /// Format matches the mockup in the bead spec:
    /// ```text
    /// [x] css                                                    ✓ Ready to sync
    ///     209.145.54.164 • ubuntu 22.04 • 45GB free
    ///     ✓ cass v0.1.50 • 1,234 sessions indexed
    ///     Claude ✓  Codex ✓  Cursor ✓  Gemini ✓
    /// ```
    fn format_host(&self, host: &HostDisplayInfo) -> String {
        let mut lines = Vec::new();

        // Line 1: Host name with right-aligned status badge
        // Note: ANSI codes make length calculation tricky, so we use a fixed width
        let status_badge = host.state.status_badge();
        let name_line = format!("{}  {}", host.name.bold(), status_badge);
        lines.push(name_line);

        // Line 2: Hostname, OS, disk space (dimmed)
        let system_info = host.system_info.as_deref().unwrap_or("");
        let host_info = if system_info.is_empty() {
            format!(
                "    {} • {}",
                host.hostname.dimmed(),
                host.username.dimmed()
            )
        } else {
            format!(
                "    {} • {} • {}",
                host.hostname.dimmed(),
                host.username.dimmed(),
                system_info.dimmed()
            )
        };
        lines.push(host_info);

        // Line 3: cass status
        let status_line = match &host.cass_status {
            CassStatusDisplay::Installed { version, sessions } => {
                format!(
                    "    {} cass v{} • {} sessions indexed",
                    "✓".green(),
                    version,
                    sessions
                )
            }
            CassStatusDisplay::InstalledNotIndexed { version } => {
                format!(
                    "    {} cass v{} • {} (will index)",
                    "⚡".yellow(),
                    version,
                    "not indexed".yellow()
                )
            }
            CassStatusDisplay::NotInstalled => {
                format!(
                    "    {} cass not installed (will install via cargo)",
                    "✗".yellow()
                )
            }
            CassStatusDisplay::Unknown => {
                format!("    {} status unknown", "?".dimmed())
            }
        };
        lines.push(status_line);

        // Line 4: Detected agents (if any)
        if !host.detected_agents.is_empty() {
            let agents: Vec<String> = host
                .detected_agents
                .iter()
                .map(|a| {
                    // Capitalize first letter for display
                    let display_name = if a.is_empty() {
                        a.clone()
                    } else {
                        let mut chars = a.chars();
                        match chars.next() {
                            Some(first) => first.to_uppercase().chain(chars).collect(),
                            None => a.clone(),
                        }
                    };
                    format!("{} {}", display_name.cyan(), "✓".green())
                })
                .collect();
            let agents_line = format!("    {}", agents.join("  "));
            lines.push(agents_line);
        }

        // Line 5: Error if unreachable
        if !host.reachable {
            let error_msg = host.error.as_deref().unwrap_or("unreachable");
            let error_line = format!("    {} {}", "⚠".red(), error_msg.red());
            lines.push(error_line);
        }

        // Line 6: Already configured message
        if host.state == HostState::AlreadyConfigured {
            lines.push(format!(
                "    {}",
                "Use 'cass sources edit' to modify".dimmed()
            ));
        }

        lines.join("\n")
    }

    /// Show the interactive multi-select prompt.
    ///
    /// Returns the selection result or an error if the prompt was cancelled.
    pub fn prompt(&self) -> Result<HostSelectionResult, InteractiveError> {
        if self.hosts.is_empty() {
            return Err(InteractiveError::NoHosts);
        }

        // Only show selectable hosts (filter out unreachable and already-configured)
        let selectable_hosts: Vec<(usize, &HostDisplayInfo)> = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| h.state.is_selectable())
            .collect();

        if selectable_hosts.is_empty() {
            return Err(InteractiveError::NoSelectableHosts);
        }

        // Format selectable hosts for display
        let items: Vec<String> = selectable_hosts
            .iter()
            .map(|(_, h)| self.format_host(h))
            .collect();

        // Pre-select based on HostState
        let defaults: Vec<bool> = selectable_hosts
            .iter()
            .map(|(_, h)| h.state.should_preselect())
            .collect();

        // Show the prompt
        println!();
        println!(
            "{}",
            "Select hosts to configure as sources:".bold().underline()
        );
        println!(
            "{}",
            "[space] toggle  [a] all  [enter] confirm  [q] quit".dimmed()
        );
        println!();

        let selected_in_filtered = MultiSelect::with_theme(&self.theme)
            .items(&items)
            .defaults(&defaults)
            .interact_opt()
            .map_err(|e| InteractiveError::IoError(e.to_string()))?
            .ok_or(InteractiveError::Cancelled)?;

        // Map filtered indices back to original indices
        let selected: Vec<usize> = selected_in_filtered
            .iter()
            .filter_map(|&i| selectable_hosts.get(i).map(|(orig_idx, _)| *orig_idx))
            .collect();

        // Categorize selections by state
        let mut needs_install = Vec::new();
        let mut needs_indexing = Vec::new();
        let mut ready_for_sync = Vec::new();

        for &idx in &selected {
            if let Some(host) = self.hosts.get(idx) {
                match host.state {
                    HostState::ReadyToSync => ready_for_sync.push(idx),
                    HostState::NeedsIndexing => needs_indexing.push(idx),
                    HostState::NeedsInstall => needs_install.push(idx),
                    _ => {} // Unreachable and AlreadyConfigured are not selectable
                }
            }
        }

        Ok(HostSelectionResult {
            selected_indices: selected,
            needs_install,
            needs_indexing,
            ready_for_sync,
        })
    }

    /// Get host info by index.
    pub fn get_host(&self, index: usize) -> Option<&HostDisplayInfo> {
        self.hosts.get(index)
    }
}

// =============================================================================
// Confirmation Prompts
// =============================================================================

/// Ask for confirmation before a destructive operation.
pub fn confirm_action(message: &str, default: bool) -> Result<bool, InteractiveError> {
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(message)
        .default(default)
        .interact()
        .map_err(|e| InteractiveError::IoError(e.to_string()))
}

/// Ask for confirmation with a detailed explanation.
pub fn confirm_with_details(
    action: &str,
    details: &[&str],
    default: bool,
) -> Result<bool, InteractiveError> {
    println!();
    println!("{}", action.bold());
    for detail in details {
        println!("  • {}", detail);
    }
    println!();

    confirm_action("Proceed?", default)
}

// =============================================================================
// Probe Result Conversion
// =============================================================================

/// Convert a probe result to display info for the selection UI.
///
/// # Arguments
/// * `probe` - The probe result from SSH probing
/// * `already_configured` - Set of normalized source-name keys already configured
pub fn probe_to_display_info(
    probe: &HostProbeResult,
    already_configured: &HashSet<String>,
) -> HostDisplayInfo {
    let generated_name = super::config::normalize_generated_remote_source_name(&probe.host_name);

    // Determine host state
    let state = if already_configured.contains(&super::config::source_name_key(&generated_name)) {
        HostState::AlreadyConfigured
    } else if !probe.reachable {
        HostState::Unreachable
    } else {
        match &probe.cass_status {
            CassStatus::Indexed { session_count, .. } if *session_count > 0 => {
                HostState::ReadyToSync
            }
            CassStatus::Indexed { .. } => HostState::NeedsIndexing, // 0 sessions
            CassStatus::InstalledNotIndexed { .. } => HostState::NeedsIndexing,
            CassStatus::NotFound | CassStatus::Unknown => HostState::NeedsInstall,
        }
    };

    // Convert cass status
    let cass_status = match &probe.cass_status {
        CassStatus::Indexed {
            version,
            session_count,
            ..
        } => CassStatusDisplay::Installed {
            version: version.clone(),
            sessions: *session_count,
        },
        CassStatus::InstalledNotIndexed { version } => CassStatusDisplay::InstalledNotIndexed {
            version: version.clone(),
        },
        CassStatus::NotFound => CassStatusDisplay::NotInstalled,
        CassStatus::Unknown => CassStatusDisplay::Unknown,
    };

    // Format system info string
    let system_info = probe.system_info.as_ref().map(|si| {
        // Use distro if present and non-empty, otherwise fall back to OS
        let os_info = si
            .distro
            .as_deref()
            .filter(|d| !d.is_empty())
            .unwrap_or(&si.os);
        if let Some(res) = &probe.resources {
            let disk_gb = res.disk_available_mb / 1024;
            format!("{} • {}GB free", os_info, disk_gb)
        } else {
            os_info.to_string()
        }
    });

    // Extract detected agent names
    let detected_agents: Vec<String> = probe
        .detected_agents
        .iter()
        .map(|a| a.agent_type.clone())
        .collect();

    // Use probe host name as the display hostname
    let hostname = probe.host_name.clone();

    let username = probe
        .system_info
        .as_ref()
        .and_then(|si| {
            // Extract username from remote_home path like "/home/ubuntu"
            // Filter out empty results from paths like "/" or ""
            si.remote_home
                .rsplit('/')
                .find(|s| !s.is_empty())
                .map(String::from)
        })
        .unwrap_or_else(|| "user".to_string());

    HostDisplayInfo {
        name: probe.host_name.clone(),
        hostname,
        username,
        cass_status,
        detected_agents,
        reachable: probe.reachable,
        error: probe.error.clone(),
        state,
        system_info,
    }
}

/// Run the interactive host selection flow.
///
/// This is the main entry point for host selection. It:
/// 1. Converts probe results to display info
/// 2. Shows the interactive multi-select prompt
/// 3. Returns the selection result
///
/// # Arguments
/// * `probed_hosts` - Results from probing SSH hosts
/// * `already_configured` - Set of normalized source-name keys already configured
///
/// # Returns
/// The selected hosts and their categorization, or an error if cancelled.
pub fn run_host_selection(
    probed_hosts: &[HostProbeResult],
    already_configured: &HashSet<String>,
) -> Result<(HostSelectionResult, Vec<HostDisplayInfo>), InteractiveError> {
    // Check for TTY
    if !std::io::stdin().is_terminal() {
        return Err(InteractiveError::NotATty);
    }

    // Convert probe results to display info
    let hosts: Vec<HostDisplayInfo> = probed_hosts
        .iter()
        .map(|p| probe_to_display_info(p, already_configured))
        .collect();

    // Show non-selectable hosts info
    let unreachable_count = hosts
        .iter()
        .filter(|h| h.state == HostState::Unreachable)
        .count();
    let configured_count = hosts
        .iter()
        .filter(|h| h.state == HostState::AlreadyConfigured)
        .count();

    if unreachable_count > 0 || configured_count > 0 {
        println!();
        if unreachable_count > 0 {
            println!(
                "{}",
                format!(
                    "  {} {} unreachable (check SSH config)",
                    "⚠".yellow(),
                    unreachable_count
                )
                .dimmed()
            );
        }
        if configured_count > 0 {
            println!(
                "{}",
                format!("  {} {} already configured", "═".cyan(), configured_count).dimmed()
            );
        }
    }

    // Run selection
    let selector = HostSelector::new(hosts.clone());
    let result = selector.prompt()?;

    // Show summary
    let install_count = result.needs_install.len();
    let index_count = result.needs_indexing.len();
    let sync_count = result.ready_for_sync.len();
    let total = result.selected_indices.len();

    if total > 0 {
        println!();
        let mut parts = Vec::new();
        if sync_count > 0 {
            parts.push(format!("{} ready to sync", sync_count));
        }
        if index_count > 0 {
            parts.push(format!("{} needs indexing", index_count));
        }
        if install_count > 0 {
            // Estimate install time: ~3 min per host for cargo install
            let est_mins = install_count * 3;
            parts.push(format!(
                "{} needs install (~{} min)",
                install_count, est_mins
            ));
        }
        println!(
            "  {} selected: {}",
            total.to_string().bold(),
            parts.join(", ")
        );
    }

    Ok((result, hosts))
}

// =============================================================================
// Errors
// =============================================================================

/// Errors from interactive prompts.
#[derive(Debug)]
pub enum InteractiveError {
    /// User cancelled the prompt.
    Cancelled,
    /// No hosts available to select.
    NoHosts,
    /// Hosts exist but none are selectable (all unreachable or already configured).
    NoSelectableHosts,
    /// Not running in a TTY (interactive mode required).
    NotATty,
    /// IO error during prompt.
    IoError(String),
}

impl fmt::Display for InteractiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InteractiveError::Cancelled => write!(f, "Operation cancelled by user"),
            InteractiveError::NoHosts => write!(f, "No hosts available for selection"),
            InteractiveError::NoSelectableHosts => {
                write!(
                    f,
                    "No selectable hosts (all unreachable or already configured)"
                )
            }
            InteractiveError::NotATty => {
                write!(
                    f,
                    "Interactive selection requires a terminal.\n\n\
                     For non-interactive use:\n  \
                     cass sources setup --hosts css,csd,yto\n  \
                     cass sources setup --non-interactive  # select all reachable"
                )
            }
            InteractiveError::IoError(msg) => write!(f, "IO error: {}", msg),
        }
    }
}

impl std::error::Error for InteractiveError {}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_display_info_creation() {
        let host = HostDisplayInfo {
            name: "laptop".into(),
            hostname: "192.168.1.100".into(),
            username: "user".into(),
            cass_status: CassStatusDisplay::Installed {
                version: "0.1.50".into(),
                sessions: 123,
            },
            detected_agents: vec!["claude".into(), "codex".into()],
            reachable: true,
            error: None,
            state: HostState::ReadyToSync,
            system_info: Some("ubuntu 22.04 • 45GB free".into()),
        };

        assert_eq!(host.name, "laptop");
        assert!(host.reachable);
        assert!(matches!(
            host.cass_status,
            CassStatusDisplay::Installed { .. }
        ));
        assert_eq!(host.state, HostState::ReadyToSync);
    }

    #[test]
    fn test_host_selector_format() {
        let hosts = vec![HostDisplayInfo {
            name: "test-host".into(),
            hostname: "10.0.0.1".into(),
            username: "testuser".into(),
            cass_status: CassStatusDisplay::NotInstalled,
            detected_agents: vec!["claude".into()],
            reachable: true,
            error: None,
            state: HostState::NeedsInstall,
            system_info: None,
        }];

        let selector = HostSelector::new(hosts);
        let formatted = selector.format_host(&selector.hosts[0]);

        // Check that formatting includes expected content
        assert!(formatted.contains("test-host"));
        assert!(formatted.contains("10.0.0.1"));
        assert!(formatted.contains("testuser"));
        assert!(formatted.contains("cass not installed"));
        // Agent names are capitalized for display (claude -> Claude)
        assert!(formatted.contains("Claude"));
        // Should contain status badge
        assert!(formatted.contains("Needs install"));
    }

    #[test]
    fn test_host_selector_empty() {
        let selector = HostSelector::new(vec![]);
        // Can't actually call prompt() in tests, but we can verify error handling
        assert!(selector.hosts.is_empty());
    }

    #[test]
    fn test_cass_status_display_variants() {
        let installed = CassStatusDisplay::Installed {
            version: "0.1.50".into(),
            sessions: 100,
        };
        let not_installed = CassStatusDisplay::NotInstalled;
        let unknown = CassStatusDisplay::Unknown;

        assert!(matches!(installed, CassStatusDisplay::Installed { .. }));
        assert!(matches!(not_installed, CassStatusDisplay::NotInstalled));
        assert!(matches!(unknown, CassStatusDisplay::Unknown));
    }

    #[test]
    fn test_host_selection_result() {
        let result = HostSelectionResult {
            selected_indices: vec![0, 2, 3],
            needs_install: vec![2],
            needs_indexing: vec![],
            ready_for_sync: vec![0, 3],
        };

        assert_eq!(result.selected_indices.len(), 3);
        assert_eq!(result.needs_install.len(), 1);
        assert_eq!(result.needs_indexing.len(), 0);
        assert_eq!(result.ready_for_sync.len(), 2);
    }

    #[test]
    fn test_interactive_error_display() {
        let cancelled = InteractiveError::Cancelled;
        let no_hosts = InteractiveError::NoHosts;
        let io_error = InteractiveError::IoError("test error".into());

        assert!(cancelled.to_string().contains("cancelled"));
        assert!(no_hosts.to_string().contains("No hosts"));
        assert!(io_error.to_string().contains("test error"));
    }

    #[test]
    fn test_unreachable_host_format() {
        let hosts = vec![HostDisplayInfo {
            name: "unreachable-host".into(),
            hostname: "10.0.0.99".into(),
            username: "user".into(),
            cass_status: CassStatusDisplay::Unknown,
            detected_agents: vec![],
            reachable: false,
            error: Some("Connection timed out".into()),
            state: HostState::Unreachable,
            system_info: None,
        }];

        let selector = HostSelector::new(hosts);
        let formatted = selector.format_host(&selector.hosts[0]);

        assert!(formatted.contains("unreachable-host"));
        assert!(formatted.contains("Connection timed out"));
        assert!(formatted.contains("Unreachable"));
    }

    #[test]
    fn test_host_state_properties() {
        // Test is_selectable
        assert!(HostState::ReadyToSync.is_selectable());
        assert!(HostState::NeedsIndexing.is_selectable());
        assert!(HostState::NeedsInstall.is_selectable());
        assert!(!HostState::Unreachable.is_selectable());
        assert!(!HostState::AlreadyConfigured.is_selectable());

        // Test should_preselect
        assert!(HostState::ReadyToSync.should_preselect());
        assert!(HostState::NeedsIndexing.should_preselect());
        assert!(!HostState::NeedsInstall.should_preselect());
        assert!(!HostState::Unreachable.should_preselect());
        assert!(!HostState::AlreadyConfigured.should_preselect());
    }

    #[test]
    fn test_host_state_status_badges() {
        let badge = HostState::ReadyToSync.status_badge();
        assert!(badge.contains("Ready to sync"));

        let badge = HostState::NeedsIndexing.status_badge();
        assert!(badge.contains("Needs indexing"));

        let badge = HostState::NeedsInstall.status_badge();
        assert!(badge.contains("Needs install"));

        let badge = HostState::Unreachable.status_badge();
        assert!(badge.contains("Unreachable"));

        let badge = HostState::AlreadyConfigured.status_badge();
        assert!(badge.contains("Already setup"));
    }

    #[test]
    fn test_probe_to_display_info() {
        let probe = HostProbeResult {
            host_name: "test-server".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 100,
                last_indexed: None,
            },
            detected_agents: vec![],
            system_info: None,
            resources: None,
            error: None,
        };

        let already_configured = HashSet::new();
        let display = probe_to_display_info(&probe, &already_configured);

        assert_eq!(display.name, "test-server");
        assert_eq!(display.state, HostState::ReadyToSync);
        assert!(matches!(
            display.cass_status,
            CassStatusDisplay::Installed { sessions: 100, .. }
        ));
    }

    #[test]
    fn test_probe_to_display_info_already_configured() {
        let probe = HostProbeResult {
            host_name: "configured-host".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 100,
                last_indexed: None,
            },
            detected_agents: vec![],
            system_info: None,
            resources: None,
            error: None,
        };

        let mut already_configured = HashSet::new();
        already_configured.insert("configured-host".into());
        let display = probe_to_display_info(&probe, &already_configured);

        assert_eq!(display.state, HostState::AlreadyConfigured);
    }

    #[test]
    fn test_probe_to_display_info_already_configured_case_insensitive() {
        let probe = HostProbeResult {
            host_name: "Configured-Host".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 100,
                last_indexed: None,
            },
            detected_agents: vec![],
            system_info: None,
            resources: None,
            error: None,
        };

        let mut already_configured = HashSet::new();
        already_configured.insert(super::super::config::source_name_key("configured-host"));
        let display = probe_to_display_info(&probe, &already_configured);

        assert_eq!(display.state, HostState::AlreadyConfigured);
    }

    #[test]
    fn test_probe_to_display_info_reserved_local_ssh_alias_already_configured() {
        let probe = HostProbeResult {
            host_name: "local".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::Indexed {
                version: "0.1.50".into(),
                session_count: 100,
                last_indexed: None,
            },
            detected_agents: vec![],
            system_info: None,
            resources: None,
            error: None,
        };

        let mut already_configured = HashSet::new();
        already_configured.insert(super::super::config::source_name_key("local-ssh"));
        let display = probe_to_display_info(&probe, &already_configured);

        assert_eq!(display.state, HostState::AlreadyConfigured);
    }

    #[test]
    fn test_installed_not_indexed_status() {
        let status = CassStatusDisplay::InstalledNotIndexed {
            version: "0.1.50".into(),
        };
        assert!(matches!(
            status,
            CassStatusDisplay::InstalledNotIndexed { .. }
        ));
    }

    #[test]
    fn test_probe_to_display_info_username_extraction() {
        use super::super::probe::SystemInfo;

        // Normal case: /home/ubuntu -> ubuntu
        let probe = HostProbeResult {
            host_name: "test".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::NotFound,
            detected_agents: vec![],
            system_info: Some(SystemInfo {
                os: "Linux".into(),
                arch: "x86_64".into(),
                distro: None,
                has_cargo: false,
                has_cargo_binstall: false,
                has_curl: false,
                has_wget: false,
                remote_home: "/home/ubuntu".into(),
                machine_id: None,
            }),
            resources: None,
            error: None,
        };
        let display = probe_to_display_info(&probe, &HashSet::new());
        assert_eq!(display.username, "ubuntu");

        // Edge case: root path "/" -> should fall back to "user"
        let probe_root = HostProbeResult {
            host_name: "test".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::NotFound,
            detected_agents: vec![],
            system_info: Some(SystemInfo {
                os: "Linux".into(),
                arch: "x86_64".into(),
                distro: None,
                has_cargo: false,
                has_cargo_binstall: false,
                has_curl: false,
                has_wget: false,
                remote_home: "/".into(),
                machine_id: None,
            }),
            resources: None,
            error: None,
        };
        let display_root = probe_to_display_info(&probe_root, &HashSet::new());
        assert_eq!(display_root.username, "user");

        // Edge case: empty path -> should fall back to "user"
        let probe_empty = HostProbeResult {
            host_name: "test".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::NotFound,
            detected_agents: vec![],
            system_info: Some(SystemInfo {
                os: "Linux".into(),
                arch: "x86_64".into(),
                distro: None,
                has_cargo: false,
                has_cargo_binstall: false,
                has_curl: false,
                has_wget: false,
                remote_home: "".into(),
                machine_id: None,
            }),
            resources: None,
            error: None,
        };
        let display_empty = probe_to_display_info(&probe_empty, &HashSet::new());
        assert_eq!(display_empty.username, "user");
    }

    #[test]
    fn test_probe_to_display_info_empty_distro_fallback() {
        use super::super::probe::SystemInfo;

        // When distro is Some(""), should fall back to OS name
        let probe = HostProbeResult {
            host_name: "test".into(),
            reachable: true,
            connection_time_ms: 50,
            cass_status: CassStatus::NotFound,
            detected_agents: vec![],
            system_info: Some(SystemInfo {
                os: "Linux".into(),
                arch: "x86_64".into(),
                distro: Some("".into()), // Empty string distro
                has_cargo: false,
                has_cargo_binstall: false,
                has_curl: false,
                has_wget: false,
                remote_home: "/home/user".into(),
                machine_id: None,
            }),
            resources: None,
            error: None,
        };
        let display = probe_to_display_info(&probe, &HashSet::new());
        // system_info should show "Linux" not empty string
        assert!(display.system_info.as_ref().unwrap().contains("Linux"));
    }
}
