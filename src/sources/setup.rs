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
use std::path::PathBuf;
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

fn generated_source_name_for_host(host_name: &str) -> String {
    super::config::normalize_generated_remote_source_name(host_name)
}

fn generated_source_name_key_for_host(host_name: &str) -> String {
    super::config::source_name_key(&generated_source_name_for_host(host_name))
}

fn dedupe_selected_hosts_by_generated_name(
    selected_hosts: Vec<&HostProbeResult>,
) -> (Vec<&HostProbeResult>, Vec<SelectedHostNameConflict>) {
    let mut selected = Vec::new();
    let mut conflicts = Vec::new();
    let mut seen_name_keys: HashMap<String, (String, String)> = HashMap::new();

    for host in selected_hosts {
        let generated_name = generated_source_name_for_host(&host.host_name);
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(SetupError::Io)?;
        }
        let content = serde_json::to_string_pretty(self).map_err(SetupError::Json)?;
        std::fs::write(&path, content).map_err(SetupError::Io)?;
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

/// Errors that can occur during setup.
#[derive(Debug)]
pub enum SetupError {
    /// IO error.
    Io(std::io::Error),
    /// JSON serialization error.
    Json(serde_json::Error),
    /// Configuration error.
    Config(super::config::ConfigError),
    /// Installation error.
    Install(super::install::InstallError),
    /// Index error.
    Index(super::index::IndexError),
    /// Interactive UI error.
    Interactive(super::interactive::InteractiveError),
    /// User cancelled.
    Cancelled,
    /// No hosts found.
    NoHosts,
    /// Setup interrupted.
    Interrupted,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {e}"),
            Self::Json(e) => write!(f, "JSON error: {e}"),
            Self::Config(e) => write!(f, "Config error: {e}"),
            Self::Install(e) => write!(f, "Install error: {e}"),
            Self::Index(e) => write!(f, "Index error: {e}"),
            Self::Interactive(e) => write!(f, "Interactive error: {e}"),
            Self::Cancelled => write!(f, "Setup cancelled by user"),
            Self::NoHosts => write!(f, "No SSH hosts found or selected"),
            Self::Interrupted => write!(f, "Setup interrupted"),
        }
    }
}

impl std::error::Error for SetupError {}

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
                    selected_name_keys.insert(generated_source_name_key_for_host(&h.host_name))
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

        let needs_index: Vec<_> = selected_hosts
            .iter()
            .filter(|h| {
                // Need to index if not already indexed with sessions
                !matches!(
                    h.cass_status,
                    CassStatus::Indexed { session_count, .. } if session_count > 0
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
                        Ok(_) => {
                            if !opts.json {
                                println!("│ {} {} indexed", "✓".green(), host.host_name);
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
            let remaining_indexes = selected_hosts
                .iter()
                .filter(|h| {
                    !matches!(
                        h.cass_status,
                        CassStatus::Indexed { session_count, .. } if session_count > 0
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
