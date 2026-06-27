//! Setup wizard for configuring remote sources.
//!
//! This module provides an interactive wizard that orchestrates the complete
//! remote sources setup workflow:
//!
//! 1. Discovery - Find SSH hosts from ~/.ssh/config
//! 2. Probing - Check host connectivity and cass status
//! 3. Selection - Interactive host selection UI
//! 4. Installation - Install cass on remotes that need it
//! 5. Indexing - Run cass index on remotes
//! 6. Configuration - Generate sources.toml entries
//! 7. Sync - Initial sync of session data
//!
//! The wizard supports resume capability via state persistence, allowing
//! interrupted setups to continue where they left off.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::Utc;
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

use super::config::{SourceConfigGenerator, SourcesConfig};
use super::discover_ssh_hosts;
use super::index::{IndexProgress, RemoteIndexer};
use super::install::{InstallProgress, RemoteInstaller};
use super::interactive::{confirm_action, run_host_selection};
use super::probe::{CassStatus, HostProbeResult, deduplicate_probe_results, probe_hosts_parallel};

/// Options for the setup wizard.
#[derive(Debug, Clone)]
pub struct SetupOptions {
    /// Preview what would happen without making changes.
    pub dry_run: bool,
    /// Skip interactive prompts (use defaults).
    pub non_interactive: bool,
    /// Specific hosts to configure (skips discovery/selection).
    pub hosts: Option<Vec<String>>,
    /// Skip cass installation on remotes.
    pub skip_install: bool,
    /// Skip indexing on remotes.
    pub skip_index: bool,
    /// Skip syncing after setup.
    pub skip_sync: bool,
    /// SSH connection timeout in seconds.
    pub timeout: u64,
    /// Continue from previous interrupted setup.
    pub resume: bool,
    /// Show detailed progress output.
    pub verbose: bool,
    /// Output as JSON.
    pub json: bool,
}

impl Default for SetupOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            non_interactive: false,
            hosts: None,
            skip_install: false,
            skip_index: false,
            skip_sync: false,
            timeout: 10,
            resume: false,
            verbose: false,
            json: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedHostNameConflict {
    kept_host_name: String,
    skipped_host_name: String,
    kept_source_name: String,
}

fn dedupe_selected_hosts_by_generated_name(
    selected_hosts: Vec<&HostProbeResult>,
) -> (Vec<&HostProbeResult>, Vec<SelectedHostNameConflict>) {
    let mut selected = Vec::new();
    let mut conflicts = Vec::new();
    let mut seen_name_keys: HashMap<String, (String, String)> = HashMap::new();

    for host in selected_hosts {
        let generated_name = super::config::normalize_generated_remote_source_name(&host.host_name);
        let generated_name_key = super::config::source_name_key(&generated_name);
        if let Some((kept_host_name, kept_source_name)) = seen_name_keys.get(&generated_name_key) {
            conflicts.push(SelectedHostNameConflict {
                kept_host_name: kept_host_name.clone(),
                skipped_host_name: host.host_name.clone(),
                kept_source_name: kept_source_name.clone(),
            });
            continue;
        }

        seen_name_keys.insert(generated_name_key, (host.host_name.clone(), generated_name));
        selected.push(host);
    }

    (selected, conflicts)
}

fn setup_should_index_host(
    host: &HostProbeResult,
    completed_installs: &HashSet<&str>,
    planned_installs: &HashSet<&str>,
) -> bool {
    if matches!(
        host.cass_status,
        CassStatus::Indexed { session_count, .. } if session_count > 0
    ) {
        return false;
    }

    let host_name = host.host_name.as_str();
    // Probe status is captured before installation. Same-run installs and
    // dry-run installation plans can make a NotFound probe indexable, but a
    // skip-install or failed-install NotFound host must not be indexed.
    completed_installs.contains(host_name)
        || planned_installs.contains(host_name)
        || RemoteIndexer::needs_indexing(host)
}

/// Persistent state for resumable setup.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SetupState {
    /// Whether discovery phase is complete.
    pub discovery_complete: bool,
    /// Number of discovered hosts.
    pub discovered_hosts: usize,
    /// Names of discovered hosts.
    pub discovered_host_names: Vec<String>,
    /// Whether probing phase is complete.
    pub probing_complete: bool,
    /// Probe results for each host.
    #[serde(default)]
    pub probed_hosts: Vec<HostProbeResult>,
    /// Whether selection phase is complete.
    pub selection_complete: bool,
    /// Names of selected hosts.
    pub selected_host_names: Vec<String>,
    /// Whether installation phase is complete.
    pub installation_complete: bool,
    /// Hosts where installation completed.
    pub completed_installs: Vec<String>,
    /// Whether indexing phase is complete.
    pub indexing_complete: bool,
    /// Hosts where indexing completed.
    pub completed_indexes: Vec<String>,
    /// Whether configuration phase is complete.
    pub configuration_complete: bool,
    /// Whether sync phase is complete.
    pub sync_complete: bool,
    /// Current operation description (for display).
    pub current_operation: Option<String>,
    /// When setup started (ISO 8601 timestamp).
    pub started_at: Option<String>,
}

impl SetupState {
    /// Get the state file path.
    fn path() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("cass")
            .join("setup_state.json")
    }

    /// Load state from disk if it exists.
    pub fn load() -> Result<Option<Self>, SetupError> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let state = serde_json::from_str(&content).map_err(SetupError::Json)?;
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SetupError::Io(e)),
        }
    }

    /// Save state to disk.
    pub fn save(&self) -> Result<(), SetupError> {
        let path = Self::path();
        self.save_to_path(&path)
    }

    fn save_to_path(&self, path: &Path) -> Result<(), SetupError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(SetupError::Io)?;
        }
        let content = serde_json::to_vec_pretty(self).map_err(SetupError::Json)?;
        let temp_path = write_setup_state_temp_file(path, &content).map_err(SetupError::Io)?;
        replace_setup_state_from_temp(&temp_path, path).map_err(SetupError::Io)?;
        Ok(())
    }

    /// Clear state from disk.
    pub fn clear() -> Result<(), SetupError> {
        let path = Self::path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SetupError::Io(e)),
        }
    }

    /// Check if there's any progress to resume.
    pub fn has_progress(&self) -> bool {
        self.discovery_complete
            || self.probing_complete
            || self.selection_complete
            || self.installation_complete
            || self.indexing_complete
            || self.configuration_complete
    }
}

fn write_setup_state_temp_file(path: &Path, contents: &[u8]) -> Result<PathBuf, std::io::Error> {
    for _ in 0..100 {
        let temp_path = unique_setup_state_temp_path(path)?;
        match write_setup_state_temp_file_at(&temp_path, contents) {
            Ok(()) => return Ok(temp_path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "failed to allocate unique setup state temp path for {}",
            path.display()
        ),
    ))
}

fn write_setup_state_temp_file_at(path: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

fn unique_setup_state_temp_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = setup_state_temp_path_nonce()?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("setup_state.json");

    Ok(path.with_file_name(format!(".{file_name}.tmp.{timestamp}.{nonce:016x}")))
}

fn setup_state_temp_path_nonce() -> Result<u64, std::io::Error> {
    let rng = ring::rand::SystemRandom::new();
    let mut bytes = [0u8; 8];
    ring::rand::SecureRandom::fill(&rng, &mut bytes)
        .map_err(|_| std::io::Error::other("secure random generation failed"))?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(not(windows))]
fn replace_setup_state_from_temp(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), std::io::Error> {
    std::fs::rename(temp_path, final_path)?;
    sync_setup_state_parent_directory(final_path)
}

#[cfg(windows)]
fn replace_setup_state_from_temp(
    temp_path: &Path,
    final_path: &Path,
) -> Result<(), std::io::Error> {
    match std::fs::rename(temp_path, final_path) {
        Ok(()) => sync_setup_state_parent_directory(final_path),
        Err(first_err)
            if setup_state_path_entry_exists(final_path)
                && matches!(
                    first_err.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) =>
        {
            let backup_path = unique_setup_state_replace_backup_path(final_path)?;
            std::fs::rename(final_path, &backup_path).map_err(|backup_err| {
                std::io::Error::other(format!(
                    "failed preparing backup {} before replacing {}: first error: {}; backup error: {}",
                    backup_path.display(),
                    final_path.display(),
                    first_err,
                    backup_err
                ))
            })?;
            match std::fs::rename(temp_path, final_path) {
                Ok(()) => sync_setup_state_parent_directory(final_path),
                Err(second_err) => {
                    let restore_result = std::fs::rename(&backup_path, final_path);
                    match restore_result {
                        Ok(()) => {
                            sync_setup_state_parent_directory(final_path).map_err(|sync_err| {
                                std::io::Error::other(format!(
                                    "failed replacing {} with {}: first error: {}; second error: {}; restored original file but failed syncing parent directory: {}",
                                    final_path.display(),
                                    temp_path.display(),
                                    first_err,
                                    second_err,
                                    sync_err
                                ))
                            })?;
                            Err(std::io::Error::new(
                                second_err.kind(),
                                format!(
                                    "failed replacing {} with {}: first error: {}; second error: {}; restored original file",
                                    final_path.display(),
                                    temp_path.display(),
                                    first_err,
                                    second_err
                                ),
                            ))
                        }
                        Err(restore_err) => Err(std::io::Error::other(format!(
                            "failed replacing {} with {}: first error: {}; second error: {}; restore error: {}; temp file retained at {}",
                            final_path.display(),
                            temp_path.display(),
                            first_err,
                            second_err,
                            restore_err,
                            temp_path.display()
                        ))),
                    }
                }
            }
        }
        Err(rename_err) => Err(rename_err),
    }
}

#[cfg(any(windows, test))]
fn setup_state_path_entry_exists(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

#[cfg(windows)]
fn unique_setup_state_replace_backup_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let nonce = setup_state_temp_path_nonce()?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("setup_state.json");

    Ok(path.with_file_name(format!(".{file_name}.bak.{timestamp}.{nonce:016x}")))
}

#[cfg(not(windows))]
fn sync_setup_state_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_setup_state_parent_directory(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

/// Errors that can occur during setup.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// IO error.
    #[error("IO error: {0}")]
    Io(std::io::Error),
    /// JSON serialization error.
    #[error("JSON error: {0}")]
    Json(serde_json::Error),
    /// Configuration error.
    #[error("Config error: {0}")]
    Config(super::config::ConfigError),
    /// Installation error.
    #[error("Install error: {0}")]
    Install(super::install::InstallError),
    /// Index error.
    #[error("Index error: {0}")]
    Index(super::index::IndexError),
    /// Interactive UI error.
    #[error("Interactive error: {0}")]
    Interactive(super::interactive::InteractiveError),
    /// User cancelled.
    #[error("Setup cancelled by user")]
    Cancelled,
    /// No hosts found.
    #[error("No SSH hosts found or selected")]
    NoHosts,
    /// Setup interrupted.
    #[error("Setup interrupted")]
    Interrupted,
}

/// Result of the setup wizard.
#[derive(Debug)]
pub struct SetupResult {
    /// Number of sources added.
    pub sources_added: usize,
    /// Number of hosts where cass was installed.
    pub hosts_installed: usize,
    /// Number of hosts that were indexed.
    pub hosts_indexed: usize,
    /// Total sessions now searchable.
    pub total_sessions: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Print a phase header.
fn print_phase_header(phase: &str) {
    println!();
    println!(
        "{}",
        format!("┌─ {} ", phase).bold().on_bright_black().white()
    );
}

/// Print phase completion.
fn print_phase_done(message: &str) {
    println!("│ {} {}", "✓".green(), message);
    println!("└{}", "─".repeat(70).dimmed());
}

/// Run the setup wizard.
pub fn run_setup(opts: &SetupOptions) -> Result<SetupResult, SetupError> {
    // Set up interruption flag (Ctrl+C handled at CLI level)
    let interrupted = Arc::new(AtomicBool::new(false));

    // Load or create state
    let mut state = if opts.resume {
        SetupState::load()?.unwrap_or_default()
    } else {
        SetupState::default()
    };

    if state.started_at.is_none() {
        state.started_at = Some(Utc::now().to_rfc3339());
    }

    // Check for interruption helper
    let check_interrupted = || {
        if interrupted.load(Ordering::SeqCst) {
            Err(SetupError::Interrupted)
        } else {
            Ok(())
        }
    };

    // Print header
    if !opts.json {
        println!();
        println!(
            "{}",
            "╭─────────────────────────────────────────────────────────────────────────────╮"
                .bright_blue()
        );
        println!(
            "{}",
            "│  cass sources setup                                                         │"
                .bright_blue()
        );
        println!(
            "{}",
            "╰─────────────────────────────────────────────────────────────────────────────╯"
                .bright_blue()
        );

        if opts.dry_run {
            println!();
            println!("{}", "  [DRY RUN - no changes will be made]".yellow());
        }

        if opts.resume && state.has_progress() {
            println!();
            println!("{}", "  Resuming from previous session...".cyan());
        }
    }

    // =========================================================================
    // Phase 1: Discovery
    // =========================================================================
    let discovered_hosts = if !state.discovery_complete {
        check_interrupted()?;

        if !opts.json {
            print_phase_header("Phase 1: Discovery");
        }

        let hosts = if let Some(ref specific_hosts) = opts.hosts {
            // User specified specific hosts
            specific_hosts
                .iter()
                .map(|h| super::config::DiscoveredHost {
                    name: h.clone(),
                    hostname: None,
                    user: None,
                    port: None,
                    identity_file: None,
                })
                .collect()
        } else {
            // Auto-discover from SSH config
            discover_ssh_hosts()
        };

        state.discovered_hosts = hosts.len();
        state.discovered_host_names = hosts.iter().map(|h| h.name.clone()).collect();
        state.discovery_complete = true;
        state.save()?;

        if !opts.json {
            if opts.hosts.is_some() {
                print_phase_done(&format!("Using {} specified host(s)", hosts.len()));
            } else {
                print_phase_done(&format!("Found {} SSH hosts in ~/.ssh/config", hosts.len()));
            }
        }

        hosts
    } else {
        // Reconstruct from saved state
        state
            .discovered_host_names
            .iter()
            .map(|name| super::config::DiscoveredHost {
                name: name.clone(),
                hostname: None,
                user: None,
                port: None,
                identity_file: None,
            })
            .collect()
    };

    if discovered_hosts.is_empty() {
        if !opts.json {
            println!();
            println!(
                "{}",
                "  No SSH hosts found. Add hosts to ~/.ssh/config or use --hosts.".yellow()
            );
        }
        SetupState::clear()?;
        return Err(SetupError::NoHosts);
    }

    // =========================================================================
    // Phase 2: Probing
    // =========================================================================
    let probed_hosts = if !state.probing_complete {
        check_interrupted()?;

        if !opts.json {
            print_phase_header("Phase 2: Probing hosts");
        }

        let progress = if !opts.json {
            let pb = ProgressBar::new(discovered_hosts.len() as u64);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("│ {bar:50.cyan/blue} {pos}/{len} {msg}")
                    .expect("valid progress bar template")
                    .progress_chars("██░"),
            );
            Some(pb)
        } else {
            None
        };

        let progress_clone = progress.clone();
        let results = probe_hosts_parallel(
            &discovered_hosts,
            opts.timeout,
            move |completed, total, name| {
                if let Some(ref pb) = progress_clone {
                    pb.set_position(completed as u64);
                    pb.set_message(format!("{}/{} - {}", completed, total, name));
                }
            },
        );

        if let Some(pb) = &progress {
            pb.finish_and_clear();
        }

        // Deduplicate hosts that resolve to the same machine (multiple SSH aliases)
        let (results, merged_aliases) = deduplicate_probe_results(results);

        let reachable = results.iter().filter(|p| p.reachable).count();
        let with_cass = results
            .iter()
            .filter(|p| p.cass_status.is_installed())
            .count();

        state.probed_hosts = results.clone();
        state.probing_complete = true;
        state.save()?;

        if !opts.json {
            print_phase_done(&format!(
                "{} reachable, {} with cass installed",
                reachable, with_cass
            ));

            // Show merged aliases if any
            if !merged_aliases.is_empty() {
                let total_merged: usize = merged_aliases.values().map(|v| v.len()).sum();
                println!(
                    "│ {} {} duplicate alias(es) merged (same machine):",
                    "ℹ".blue(),
                    total_merged
                );
                // Sort for deterministic output
                let mut sorted_merges: Vec<_> = merged_aliases.iter().collect();
                sorted_merges.sort_by_key(|(k, _)| *k);
                for (kept, aliases) in sorted_merges {
                    let mut sorted_aliases = aliases.clone();
                    sorted_aliases.sort();
                    println!(
                        "│   {} ← {}",
                        kept.bold(),
                        sorted_aliases.join(", ").dimmed()
                    );
                }
            }
        }

        results
    } else {
        state.probed_hosts.clone()
    };

    let reachable_hosts: Vec<_> = probed_hosts.iter().filter(|p| p.reachable).collect();

    if reachable_hosts.is_empty() {
        if !opts.json {
            println!();
            println!(
                "{}",
                "  No reachable hosts found. Check SSH connectivity.".yellow()
            );
        }
        SetupState::clear()?;
        return Err(SetupError::NoHosts);
    }

    // =========================================================================
    // Phase 3: Selection
    // =========================================================================
    let selection_performed = !state.selection_complete;
    let mut selected_hosts: Vec<&HostProbeResult> = if !state.selection_complete {
        check_interrupted()?;

        if !opts.json {
            print_phase_header("Phase 3: Host Selection");
        }

        let existing_config = SourcesConfig::load().unwrap_or_default();
        let existing_name_keys: HashSet<_> = existing_config.configured_name_keys();

        if opts.non_interactive {
            // Auto-select all reachable hosts not already configured
            let mut selected_name_keys = existing_name_keys.clone();
            let auto_selected: Vec<_> = reachable_hosts
                .iter()
                .filter(|h| {
                    let generated_name =
                        super::config::normalize_generated_remote_source_name(&h.host_name);
                    selected_name_keys.insert(super::config::source_name_key(&generated_name))
                })
                .copied()
                .collect();

            auto_selected
        } else {
            // Interactive selection
            // Convert Vec<&HostProbeResult> to Vec<HostProbeResult> for the API
            let probes_for_selection: Vec<HostProbeResult> =
                reachable_hosts.iter().map(|p| (*p).clone()).collect();

            match run_host_selection(&probes_for_selection, &existing_name_keys) {
                Ok((result, display_infos)) => {
                    // Convert selected indices to host names
                    let selected: Vec<_> = result
                        .selected_indices
                        .iter()
                        .filter_map(|&idx| {
                            display_infos.get(idx).and_then(|info| {
                                reachable_hosts
                                    .iter()
                                    .find(|h| h.host_name == info.hostname)
                                    .copied()
                            })
                        })
                        .collect();
                    selected
                }
                Err(e) => {
                    state.save()?;
                    return Err(SetupError::Interactive(e));
                }
            }
        }
    } else {
        // Reconstruct from saved state
        state
            .selected_host_names
            .iter()
            .filter_map(|name| probed_hosts.iter().find(|h| h.host_name == *name))
            .collect()
    };

    let (deduped_selected_hosts, selection_conflicts) =
        dedupe_selected_hosts_by_generated_name(selected_hosts);
    selected_hosts = deduped_selected_hosts;

    if selection_performed && !opts.json {
        let selection_message = if opts.non_interactive {
            format!(
                "Auto-selected {} hosts (non-interactive)",
                selected_hosts.len()
            )
        } else {
            format!("Selected {} hosts", selected_hosts.len())
        };
        print_phase_done(&selection_message);
    }

    if !selection_conflicts.is_empty() && !opts.json {
        println!(
            "│ {} skipped {} host(s) because their generated source names conflict:",
            "Warning:".yellow().bold(),
            selection_conflicts.len()
        );
        for conflict in &selection_conflicts {
            println!(
                "│   - {} skipped; conflicts with {} as source '{}'",
                conflict.skipped_host_name, conflict.kept_host_name, conflict.kept_source_name
            );
        }
        println!(
            "│   Edit host aliases or use 'cass sources add --name ...' later if you need distinct source IDs."
        );
    }

    let selected_host_names: Vec<String> =
        selected_hosts.iter().map(|h| h.host_name.clone()).collect();
    if !state.selection_complete || state.selected_host_names != selected_host_names {
        state.selected_host_names = selected_host_names;
        state.selection_complete = true;
        state.save()?;
    }

    if selected_hosts.is_empty() {
        if !opts.json {
            println!();
            println!("{}", "  No hosts selected. Setup cancelled.".yellow());
        }
        SetupState::clear()?;
        return Ok(SetupResult {
            sources_added: 0,
            hosts_installed: 0,
            hosts_indexed: 0,
            total_sessions: 0,
            dry_run: opts.dry_run,
        });
    }

    // =========================================================================
    // Phase 4: Installation
    // =========================================================================
    let mut hosts_installed = 0;
    let mut dry_run_planned_install_host_names: HashSet<String> = HashSet::new();

    if !opts.skip_install && !state.installation_complete {
        check_interrupted()?;

        let needs_install: Vec<_> = selected_hosts
            .iter()
            .filter(|h| !h.cass_status.is_installed())
            .filter(|h| !state.completed_installs.contains(&h.host_name))
            .collect();

        if !needs_install.is_empty() {
            if !opts.json {
                print_phase_header("Phase 4: Installing cass");
            }

            if opts.dry_run {
                if !opts.json {
                    println!("│ Would install cass on {} hosts:", needs_install.len());
                    for host in &needs_install {
                        println!("│   - {}", host.host_name);
                    }
                    println!("└{}", "─".repeat(70).dimmed());
                }
                dry_run_planned_install_host_names
                    .extend(needs_install.iter().map(|host| host.host_name.clone()));
                hosts_installed = needs_install.len();
            } else {
                // Confirm installation
                let proceed = if opts.non_interactive {
                    true
                } else {
                    confirm_action(
                        &format!("Install cass on {} hosts?", needs_install.len()),
                        true,
                    )
                    .unwrap_or(false)
                };

                if proceed {
                    for host in needs_install {
                        check_interrupted()?;

                        state.current_operation = Some(format!("Installing on {}", host.host_name));
                        state.save()?;

                        // Create installer for this specific host
                        // Skip hosts without system info (they likely failed probing)
                        let Some(system_info) = host.system_info.clone() else {
                            if !opts.json {
                                println!(
                                    "│ {} {} skipped (no system info)",
                                    "⚠".yellow(),
                                    host.host_name
                                );
                            }
                            continue;
                        };
                        let Some(resources) = host.resources.clone() else {
                            if !opts.json {
                                println!(
                                    "│ {} {} skipped (no resource info)",
                                    "⚠".yellow(),
                                    host.host_name
                                );
                            }
                            continue;
                        };
                        let installer =
                            RemoteInstaller::new(host.host_name.clone(), system_info, resources);

                        if !opts.json {
                            println!("│ Installing on {}...", host.host_name);
                        }

                        let host_name_for_progress = host.host_name.clone();
                        let verbose = opts.verbose;
                        let json = opts.json;
                        let progress_callback = move |progress: InstallProgress| {
                            if verbose && !json {
                                println!(
                                    "│   {}: {} ({}%)",
                                    host_name_for_progress,
                                    progress.stage, // Uses Display impl
                                    progress.percent.unwrap_or(0)
                                );
                            }
                        };

                        match installer.install(progress_callback) {
                            Ok(_) => {
                                if !opts.json {
                                    println!("│ {} {} installed", "✓".green(), host.host_name);
                                }
                                state.completed_installs.push(host.host_name.clone());
                                state.save()?;
                                hosts_installed += 1;
                            }
                            Err(e) => {
                                if !opts.json {
                                    println!("│ {} {} failed: {}", "✗".red(), host.host_name, e);
                                }
                                if opts.verbose {
                                    eprintln!("  Install error: {e}");
                                }
                            }
                        }
                    }

                    if !opts.json {
                        print_phase_done(&format!("Installed cass on {} hosts", hosts_installed));
                    }
                } else if !opts.json {
                    println!("│ Skipping installation.");
                    println!("└{}", "─".repeat(70).dimmed());
                }
            }
        }

        if !opts.dry_run {
            let completed: HashSet<&str> = state
                .completed_installs
                .iter()
                .map(std::string::String::as_str)
                .collect();
            let remaining_installs = selected_hosts
                .iter()
                .filter(|h| !h.cass_status.is_installed())
                .filter(|h| !completed.contains(h.host_name.as_str()))
                .count();
            state.installation_complete = remaining_installs == 0;
            state.save()?;
        }
    }

    // =========================================================================
    // Phase 5: Indexing
    // =========================================================================
    let mut hosts_indexed = 0;

    if !opts.skip_index && !state.indexing_complete {
        check_interrupted()?;

        let completed_install_host_names: HashSet<&str> = state
            .completed_installs
            .iter()
            .map(std::string::String::as_str)
            .collect();
        let dry_run_planned_install_host_names: HashSet<&str> = dry_run_planned_install_host_names
            .iter()
            .map(std::string::String::as_str)
            .collect();
        let needs_index: Vec<_> = selected_hosts
            .iter()
            .filter(|h| {
                setup_should_index_host(
                    h,
                    &completed_install_host_names,
                    &dry_run_planned_install_host_names,
                )
            })
            .filter(|h| !state.completed_indexes.contains(&h.host_name))
            .collect();

        if !needs_index.is_empty() {
            if !opts.json {
                print_phase_header("Phase 5: Indexing sessions");
            }

            if opts.dry_run {
                if !opts.json {
                    println!("│ Would index sessions on {} hosts", needs_index.len());
                    println!("└{}", "─".repeat(70).dimmed());
                }
                hosts_indexed = needs_index.len();
            } else {
                for host in needs_index {
                    check_interrupted()?;

                    state.current_operation = Some(format!("Indexing on {}", host.host_name));
                    state.save()?;

                    if !opts.json {
                        println!("│ Indexing on {}...", host.host_name);
                    }

                    // Create indexer for this specific host
                    let indexer = RemoteIndexer::with_defaults(host.host_name.clone());

                    let host_name_for_progress = host.host_name.clone();
                    let verbose = opts.verbose;
                    let json = opts.json;
                    let progress_callback = move |progress: IndexProgress| {
                        if verbose && !json {
                            let pct = progress.percent.unwrap_or(0);
                            println!(
                                "│   {}: {} ({}%)",
                                host_name_for_progress,
                                progress.stage, // Uses Display impl
                                pct
                            );
                        }
                    };

                    match indexer.run_index(progress_callback) {
                        Ok(result) => {
                            if !opts.json {
                                println!("│ {} {} indexed", "✓".green(), host.host_name);
                                if opts.verbose
                                    && let Some(artifact) = &result.artifact_manifest
                                {
                                    if artifact.success {
                                        println!(
                                            "│   {} artifact proof {} ({} chunks)",
                                            "✓".green(),
                                            artifact
                                                .bundle_id
                                                .as_deref()
                                                .unwrap_or("bundle id unavailable"),
                                            artifact.chunk_count.unwrap_or(0)
                                        );
                                    } else {
                                        println!(
                                            "│   {} artifact proof unavailable: {}",
                                            "⚠".yellow(),
                                            artifact
                                                .error
                                                .as_deref()
                                                .unwrap_or("unknown artifact manifest error")
                                        );
                                    }
                                }
                            }
                            state.completed_indexes.push(host.host_name.clone());
                            state.save()?;
                            hosts_indexed += 1;
                        }
                        Err(e) => {
                            if !opts.json {
                                println!(
                                    "│ {} Index error on {}: {}",
                                    "✗".red(),
                                    host.host_name,
                                    e
                                );
                            }
                        }
                    }
                }

                if !opts.json {
                    print_phase_done(&format!("Indexed {} hosts", hosts_indexed));
                }
            }
        }

        if !opts.dry_run {
            let completed: HashSet<&str> = state
                .completed_indexes
                .iter()
                .map(std::string::String::as_str)
                .collect();
            let completed_install_host_names: HashSet<&str> = state
                .completed_installs
                .iter()
                .map(std::string::String::as_str)
                .collect();
            let pending_install_host_names: HashSet<&str> = if opts.skip_install {
                HashSet::new()
            } else {
                selected_hosts
                    .iter()
                    .filter(|h| !h.cass_status.is_installed())
                    .filter(|h| !completed_install_host_names.contains(h.host_name.as_str()))
                    .map(|h| h.host_name.as_str())
                    .collect()
            };
            let remaining_indexes = selected_hosts
                .iter()
                .filter(|h| {
                    setup_should_index_host(
                        h,
                        &completed_install_host_names,
                        &pending_install_host_names,
                    )
                })
                .filter(|h| !completed.contains(h.host_name.as_str()))
                .count();
            state.indexing_complete = remaining_indexes == 0;
            state.save()?;
        }
    }

    // =========================================================================
    // Phase 6: Configuration
    // =========================================================================
    let mut sources_added = 0;

    if !state.configuration_complete {
        check_interrupted()?;

        if !opts.json {
            print_phase_header("Phase 6: Configuring sources");
        }

        let mut config = SourcesConfig::load().unwrap_or_default();
        let generator = SourceConfigGenerator::new();

        // Generate preview
        let probes: Vec<(&str, &HostProbeResult)> = selected_hosts
            .iter()
            .map(|h| (h.host_name.as_str(), *h))
            .collect();

        let preview = generator.generate_preview(&probes, &config.configured_name_keys());

        if opts.dry_run {
            if !opts.json {
                preview.display();
                println!("└{}", "─".repeat(70).dimmed());
            }
            sources_added = preview.add_count();
        } else {
            // Merge and save
            let (added, _skipped) = config.merge_preview(&preview).map_err(SetupError::Config)?;
            sources_added = added;

            if added > 0 {
                config.write_with_backup().map_err(SetupError::Config)?;
            }

            if !opts.json {
                print_phase_done(&format!("Added {} sources to configuration", added));
            }
        }

        state.configuration_complete = true;
        state.save()?;
    }

    // =========================================================================
    // Phase 7: Sync
    // =========================================================================
    if !opts.skip_sync && !opts.dry_run && !state.sync_complete {
        check_interrupted()?;

        if !opts.json {
            print_phase_header("Phase 7: Syncing data");
            println!("│ Run 'cass sources sync' to sync session data from remotes.");
            println!("└{}", "─".repeat(70).dimmed());
        }

        // Note: We don't actually run sync here because it can be long-running
        // and the user might want to control when it happens. We just mark it
        // as skipped and let them run it manually.
        state.sync_complete = true;
        state.save()?;
    }

    // =========================================================================
    // Phase 8: Summary
    // =========================================================================
    if !opts.json {
        print_phase_header("Setup Complete");

        let total_sessions: u64 = selected_hosts
            .iter()
            .filter_map(|h| {
                if let CassStatus::Indexed { session_count, .. } = &h.cass_status {
                    Some(*session_count)
                } else {
                    None
                }
            })
            .sum();

        if opts.dry_run {
            println!("│");
            println!("│ {} Dry run complete. No changes were made.", "ℹ".blue());
            println!("│ Run without --dry-run to execute setup.");
        } else {
            println!("│");
            println!("│ {} {} sources configured", "✓".green(), sources_added);
            if hosts_installed > 0 {
                println!(
                    "│ {} cass installed on {} hosts",
                    "✓".green(),
                    hosts_installed
                );
            }
            if hosts_indexed > 0 {
                println!("│ {} {} hosts indexed", "✓".green(), hosts_indexed);
            }
            println!(
                "│ {} ~{} sessions now searchable",
                "✓".green(),
                total_sessions
            );
            println!("│");
            println!(
                "│ Run '{}' to search across all machines",
                "cass search <query>".cyan()
            );
        }

        println!("└{}", "─".repeat(70).dimmed());
    }

    // Clear state on success
    SetupState::clear()?;

    let total_sessions: u64 = selected_hosts
        .iter()
        .filter_map(|h| {
            if let CassStatus::Indexed { session_count, .. } = &h.cass_status {
                Some(*session_count)
            } else {
                None
            }
        })
        .sum();

    Ok(SetupResult {
        sources_added,
        hosts_installed,
        hosts_indexed,
        total_sessions,
        dry_run: opts.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    type SetupTestResult = Result<(), Box<dyn std::error::Error>>;

    fn setup_test_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
        std::io::Error::other(message.into()).into()
    }

    fn ensure_setup_test(condition: bool, message: impl Into<String>) -> SetupTestResult {
        if condition {
            Ok(())
        } else {
            Err(setup_test_error(message))
        }
    }

    #[test]
    fn test_setup_options_default() {
        let opts = SetupOptions::default();
        assert!(!opts.dry_run);
        assert!(!opts.non_interactive);
        assert!(opts.hosts.is_none());
        assert!(!opts.skip_install);
        assert!(!opts.skip_index);
        assert!(!opts.skip_sync);
        assert_eq!(opts.timeout, 10);
        assert!(!opts.resume);
        assert!(!opts.verbose);
        assert!(!opts.json);
    }

    #[test]
    fn test_setup_state_default() {
        let state = SetupState::default();
        assert!(!state.discovery_complete);
        assert_eq!(state.discovered_hosts, 0);
        assert!(state.discovered_host_names.is_empty());
        assert!(!state.probing_complete);
        assert!(state.probed_hosts.is_empty());
        assert!(!state.selection_complete);
        assert!(state.selected_host_names.is_empty());
        assert!(!state.installation_complete);
        assert!(state.completed_installs.is_empty());
        assert!(!state.indexing_complete);
        assert!(state.completed_indexes.is_empty());
        assert!(!state.configuration_complete);
        assert!(!state.sync_complete);
        assert!(state.current_operation.is_none());
        assert!(state.started_at.is_none());
    }

    #[test]
    fn test_setup_state_has_progress_empty() {
        let state = SetupState::default();
        assert!(!state.has_progress());
    }

    #[test]
    fn test_setup_state_has_progress_discovery() {
        let state = SetupState {
            discovery_complete: true,
            ..Default::default()
        };
        assert!(state.has_progress());
    }

    #[test]
    fn test_setup_state_has_progress_probing() {
        let state = SetupState {
            probing_complete: true,
            ..Default::default()
        };
        assert!(state.has_progress());
    }

    #[test]
    fn test_setup_state_has_progress_selection() {
        let state = SetupState {
            selection_complete: true,
            ..Default::default()
        };
        assert!(state.has_progress());
    }

    #[test]
    fn test_setup_state_has_progress_installation() {
        let state = SetupState {
            installation_complete: true,
            ..Default::default()
        };
        assert!(state.has_progress());
    }

    #[test]
    fn test_setup_state_has_progress_indexing() {
        let state = SetupState {
            indexing_complete: true,
            ..Default::default()
        };
        assert!(state.has_progress());
    }

    #[test]
    fn test_setup_state_has_progress_configuration() {
        let state = SetupState {
            configuration_complete: true,
            ..Default::default()
        };
        assert!(state.has_progress());
    }

    #[test]
    fn test_setup_state_serde_roundtrip() {
        let state = SetupState {
            discovery_complete: true,
            discovered_hosts: 5,
            discovered_host_names: vec!["host1".to_string(), "host2".to_string()],
            selected_host_names: vec!["host1".to_string()],
            started_at: Some("2025-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: SetupState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.discovery_complete, state.discovery_complete);
        assert_eq!(deserialized.discovered_hosts, state.discovered_hosts);
        assert_eq!(
            deserialized.discovered_host_names,
            state.discovered_host_names
        );
        assert_eq!(deserialized.selected_host_names, state.selected_host_names);
        assert_eq!(deserialized.started_at, state.started_at);
    }

    #[test]
    fn test_setup_state_save_to_path_round_trips() -> SetupTestResult {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("setup_state.json");
        let state = SetupState {
            discovery_complete: true,
            discovered_hosts: 2,
            discovered_host_names: vec!["alpha".to_string(), "beta".to_string()],
            current_operation: Some("probing".to_string()),
            started_at: Some("2026-05-28T00:00:00Z".to_string()),
            ..Default::default()
        };

        state.save_to_path(&path)?;

        let loaded: SetupState = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure_setup_test(
            loaded.discovery_complete == state.discovery_complete,
            "discovery_complete should round-trip",
        )?;
        ensure_setup_test(
            loaded.discovered_hosts == state.discovered_hosts,
            "discovered_hosts should round-trip",
        )?;
        ensure_setup_test(
            loaded.discovered_host_names == state.discovered_host_names,
            "discovered_host_names should round-trip",
        )?;
        ensure_setup_test(
            loaded.current_operation == state.current_operation,
            "current_operation should round-trip",
        )?;
        ensure_setup_test(
            loaded.started_at == state.started_at,
            "started_at should round-trip",
        )?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_setup_state_save_replaces_symlink_without_following() -> SetupTestResult {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let path = temp.path().join("setup_state.json");
        let protected = temp.path().join("protected.json");
        let state = SetupState {
            probing_complete: true,
            selected_host_names: vec!["remote-box".to_string()],
            current_operation: Some("configuring".to_string()),
            ..Default::default()
        };

        std::fs::write(&protected, b"protected")?;
        symlink(&protected, &path)?;

        state.save_to_path(&path)?;

        ensure_setup_test(
            std::fs::read(&protected)? == b"protected",
            "protected target should not be overwritten",
        )?;
        ensure_setup_test(
            !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "setup state save should replace the symlink path itself",
        )?;
        let loaded: SetupState = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure_setup_test(
            loaded.probing_complete == state.probing_complete,
            "probing_complete should round-trip after symlink replacement",
        )?;
        ensure_setup_test(
            loaded.selected_host_names == state.selected_host_names,
            "selected_host_names should round-trip after symlink replacement",
        )?;
        ensure_setup_test(
            loaded.current_operation == state.current_operation,
            "current_operation should round-trip after symlink replacement",
        )?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_setup_state_path_entry_exists_detects_dangling_symlink() -> SetupTestResult {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()?;
        let path = temp.path().join("setup_state.json");
        let missing_target = temp.path().join("missing.json");

        symlink(&missing_target, &path)?;

        ensure_setup_test(!path.exists(), "Path::exists follows the missing target")?;
        ensure_setup_test(
            setup_state_path_entry_exists(&path),
            "replacement fallback must detect the symlink path entry itself",
        )?;
        Ok(())
    }

    #[test]
    fn test_setup_error_display_cancelled() {
        let err = SetupError::Cancelled;
        assert_eq!(format!("{err}"), "Setup cancelled by user");
    }

    #[test]
    fn test_setup_error_display_no_hosts() {
        let err = SetupError::NoHosts;
        assert_eq!(format!("{err}"), "No SSH hosts found or selected");
    }

    #[test]
    fn test_setup_error_display_interrupted() {
        let err = SetupError::Interrupted;
        assert_eq!(format!("{err}"), "Setup interrupted");
    }

    #[test]
    fn test_setup_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err = SetupError::Io(io_err);
        assert!(format!("{err}").contains("IO error"));
    }

    #[test]
    fn test_setup_error_source_is_preserved_as_none() {
        let errors = [
            SetupError::Cancelled,
            SetupError::NoHosts,
            SetupError::Interrupted,
            SetupError::Io(std::io::Error::other("io")),
            SetupError::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
        ];

        for err in errors {
            assert!(std::error::Error::source(&err).is_none(), "{err}");
        }
    }

    #[test]
    fn test_setup_result_structure() {
        let result = SetupResult {
            sources_added: 3,
            hosts_installed: 1,
            hosts_indexed: 2,
            total_sessions: 150,
            dry_run: false,
        };
        assert_eq!(result.sources_added, 3);
        assert_eq!(result.hosts_installed, 1);
        assert_eq!(result.hosts_indexed, 2);
        assert_eq!(result.total_sessions, 150);
        assert!(!result.dry_run);
    }

    #[test]
    fn test_setup_result_dry_run() {
        let result = SetupResult {
            sources_added: 5,
            hosts_installed: 0,
            hosts_indexed: 0,
            total_sessions: 0,
            dry_run: true,
        };
        assert!(result.dry_run);
        assert_eq!(result.sources_added, 5);
    }

    fn make_selected_probe(host_name: &str) -> HostProbeResult {
        HostProbeResult {
            host_name: host_name.to_string(),
            reachable: true,
            connection_time_ms: 0,
            cass_status: CassStatus::NotFound,
            detected_agents: Vec::new(),
            system_info: None,
            resources: None,
            error: None,
        }
    }

    fn make_selected_probe_with_status(
        host_name: &str,
        cass_status: CassStatus,
    ) -> HostProbeResult {
        HostProbeResult {
            host_name: host_name.to_string(),
            reachable: true,
            connection_time_ms: 0,
            cass_status,
            detected_agents: Vec::new(),
            system_info: None,
            resources: None,
            error: None,
        }
    }

    #[test]
    fn test_setup_indexing_eligibility_skips_missing_cass_without_install() {
        let host = make_selected_probe("fresh-host");
        let completed_installs = HashSet::new();
        let planned_installs = HashSet::new();

        assert!(!setup_should_index_host(
            &host,
            &completed_installs,
            &planned_installs
        ));
    }

    #[test]
    fn test_setup_indexing_eligibility_indexes_host_installed_this_run() {
        let host = make_selected_probe("fresh-host");
        let completed_installs = HashSet::from(["fresh-host"]);
        let planned_installs = HashSet::new();

        assert!(setup_should_index_host(
            &host,
            &completed_installs,
            &planned_installs
        ));
    }

    #[test]
    fn test_setup_indexing_eligibility_indexes_host_planned_for_dry_run_install() {
        let host = make_selected_probe("fresh-host");
        let completed_installs = HashSet::new();
        let planned_installs = HashSet::from(["fresh-host"]);

        assert!(setup_should_index_host(
            &host,
            &completed_installs,
            &planned_installs
        ));
    }

    #[test]
    fn test_setup_indexing_eligibility_keeps_pending_install_as_remaining_work() {
        let host = make_selected_probe("fresh-host");
        let completed_installs = HashSet::new();
        let pending_installs = HashSet::from(["fresh-host"]);

        assert!(setup_should_index_host(
            &host,
            &completed_installs,
            &pending_installs
        ));
    }

    #[test]
    fn test_setup_indexing_eligibility_uses_probe_status_for_existing_cass() {
        let host = make_selected_probe_with_status(
            "existing-host",
            CassStatus::InstalledNotIndexed {
                version: "0.1.0".to_string(),
            },
        );
        let completed_installs = HashSet::new();
        let planned_installs = HashSet::new();

        assert!(setup_should_index_host(
            &host,
            &completed_installs,
            &planned_installs
        ));
    }

    #[test]
    fn test_setup_indexing_eligibility_skips_indexed_sessions_even_if_install_recorded() {
        let host = make_selected_probe_with_status(
            "indexed-host",
            CassStatus::Indexed {
                version: "0.1.0".to_string(),
                session_count: 42,
                last_indexed: Some("2026-05-06T00:00:00Z".to_string()),
            },
        );
        let completed_installs = HashSet::from(["indexed-host"]);
        let planned_installs = HashSet::from(["indexed-host"]);

        assert!(!setup_should_index_host(
            &host,
            &completed_installs,
            &planned_installs
        ));
    }

    #[test]
    fn test_dedupe_selected_hosts_by_generated_name_case_insensitive() {
        let laptop_upper = make_selected_probe("Laptop");
        let laptop_lower = make_selected_probe("laptop");

        let (selected, conflicts) =
            dedupe_selected_hosts_by_generated_name(vec![&laptop_upper, &laptop_lower]);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].host_name, "Laptop");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kept_host_name, "Laptop");
        assert_eq!(conflicts[0].skipped_host_name, "laptop");
        assert_eq!(conflicts[0].kept_source_name, "Laptop");
    }

    #[test]
    fn test_dedupe_selected_hosts_by_generated_name_reserved_local_alias() {
        let local_lower = make_selected_probe("local");
        let local_upper = make_selected_probe("LOCAL");

        let (selected, conflicts) =
            dedupe_selected_hosts_by_generated_name(vec![&local_lower, &local_upper]);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].host_name, "local");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kept_host_name, "local");
        assert_eq!(conflicts[0].skipped_host_name, "LOCAL");
        assert_eq!(conflicts[0].kept_source_name, "local-ssh");
    }

    #[test]
    fn test_setup_state_path() {
        let path = SetupState::path();
        assert!(path.ends_with("setup_state.json"));
        assert!(path.to_string_lossy().contains("cass"));
    }
}
