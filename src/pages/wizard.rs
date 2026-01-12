use anyhow::{Context, Result, bail};
use console::{Term, style};
use dialoguer::{Confirm, Input, MultiSelect, Password, Select, theme::ColorfulTheme};
use indicatif::{ProgressBar, ProgressStyle};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::pages::bundle::{BundleBuilder, BundleConfig};
use crate::pages::encrypt::EncryptionEngine;
use crate::pages::export::{ExportEngine, ExportFilter, PathMode};
use crate::pages::secret_scan::{
    SecretScanConfig, SecretScanFilters, print_human_report, wizard_secret_scan,
};
use crate::pages::size::{BundleVerifier, SizeEstimate, SizeLimitResult};
use crate::storage::sqlite::SqliteStorage;

/// Deployment target for the export
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployTarget {
    Local,
    GitHubPages,
    CloudflarePages,
}

impl std::fmt::Display for DeployTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeployTarget::Local => write!(f, "Local export only"),
            DeployTarget::GitHubPages => write!(f, "GitHub Pages"),
            DeployTarget::CloudflarePages => write!(f, "Cloudflare Pages"),
        }
    }
}

/// Wizard state tracking all configuration
#[derive(Debug, Clone)]
pub struct WizardState {
    // Content selection
    pub agents: Vec<String>,
    pub time_range: Option<String>,
    pub workspaces: Option<Vec<PathBuf>>,

    // Security configuration
    pub password: Option<String>,
    pub recovery_secret: Option<Vec<u8>>,
    pub generate_recovery: bool,
    pub generate_qr: bool,

    // Site configuration
    pub title: String,
    pub description: String,
    pub hide_metadata: bool,

    // Deployment
    pub target: DeployTarget,
    pub output_dir: PathBuf,
    pub repo_name: Option<String>,

    // Database path
    pub db_path: PathBuf,
}

impl Default for WizardState {
    fn default() -> Self {
        let db_path =
            directories::ProjectDirs::from("com", "dicklesworthstone", "coding-agent-search")
                .map(|dirs| dirs.data_dir().join("agent_search.db"))
                .expect("Could not determine data directory");

        Self {
            agents: Vec::new(),
            time_range: None,
            workspaces: None,
            password: None,
            recovery_secret: None,
            generate_recovery: true,
            generate_qr: false,
            title: "cass Archive".to_string(),
            description: "Encrypted archive of AI coding agent conversations".to_string(),
            hide_metadata: false,
            target: DeployTarget::Local,
            output_dir: PathBuf::from("cass-export"),
            repo_name: None,
            db_path,
        }
    }
}

pub struct PagesWizard {
    state: WizardState,
}

impl Default for PagesWizard {
    fn default() -> Self {
        Self::new()
    }
}

impl PagesWizard {
    pub fn new() -> Self {
        Self {
            state: WizardState::default(),
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let mut term = Term::stdout();
        let theme = ColorfulTheme::default();

        term.clear_screen()?;
        self.print_header(&mut term)?;

        // Step 1: Content Selection
        self.step_content_selection(&mut term, &theme)?;

        // Step 2: Secret Scan
        self.step_secret_scan(&mut term, &theme)?;

        // Step 3: Security Configuration
        self.step_security_config(&mut term, &theme)?;

        // Step 4: Site Configuration
        self.step_site_config(&mut term, &theme)?;

        // Step 5: Deployment Target
        self.step_deployment_target(&mut term, &theme)?;

        // Step 6: Pre-Publish Summary
        if !self.step_summary(&mut term, &theme)? {
            writeln!(term, "{}", style("Export cancelled.").yellow())?;
            return Ok(());
        }

        // Step 7: Export Progress
        self.step_export(&mut term)?;

        // Step 8: Deploy (if not local)
        self.step_deploy(&mut term)?;

        Ok(())
    }

    fn print_header(&self, term: &mut Term) -> Result<()> {
        writeln!(
            term,
            "{}",
            style("🔐 cass Pages Export Wizard").bold().cyan()
        )?;
        writeln!(
            term,
            "Create an encrypted, searchable web archive of your AI coding agent conversations."
        )?;
        writeln!(term)?;
        Ok(())
    }

    fn step_content_selection(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<()> {
        writeln!(term, "\n{}", style("Step 1 of 8: Content Selection").bold())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        // Load agents dynamically from database
        let storage = SqliteStorage::open_readonly(&self.state.db_path)
            .context("Failed to open database. Run 'cass index' first.")?;
        let db_agents = storage.list_agents()?;
        let db_workspaces = storage.list_workspaces()?;
        drop(storage);

        if db_agents.is_empty() {
            writeln!(
                term,
                "{}",
                style("⚠ No agents found in database. Run 'cass index' first.").red()
            )?;
            bail!("No agents found in database");
        }

        // Build agent display list with conversation counts
        let agent_items: Vec<String> = db_agents
            .iter()
            .map(|a| format!("{} ({})", a.name, a.slug))
            .collect();

        let selected_agents = MultiSelect::with_theme(theme)
            .with_prompt("Which agents would you like to include?")
            .items(&agent_items)
            .defaults(&vec![true; agent_items.len()])
            .interact()?;

        self.state.agents = selected_agents
            .iter()
            .map(|&i| db_agents[i].slug.clone())
            .collect();

        if self.state.agents.is_empty() {
            bail!("No agents selected. Export cancelled.");
        }

        writeln!(
            term,
            "  {} {} agents selected",
            style("✓").green(),
            self.state.agents.len()
        )?;

        // Workspace selection (optional)
        if !db_workspaces.is_empty() {
            let include_all = Confirm::with_theme(theme)
                .with_prompt("Include all workspaces?")
                .default(true)
                .interact()?;

            if !include_all {
                let workspace_items: Vec<String> = db_workspaces
                    .iter()
                    .map(|w| {
                        w.display_name
                            .clone()
                            .unwrap_or_else(|| w.path.to_string_lossy().to_string())
                    })
                    .collect();

                let selected_ws = MultiSelect::with_theme(theme)
                    .with_prompt("Select workspaces to include:")
                    .items(&workspace_items)
                    .interact()?;

                if !selected_ws.is_empty() {
                    self.state.workspaces = Some(
                        selected_ws
                            .iter()
                            .map(|&i| db_workspaces[i].path.clone())
                            .collect(),
                    );
                    writeln!(
                        term,
                        "  {} {} workspaces selected",
                        style("✓").green(),
                        selected_ws.len()
                    )?;
                }
            }
        }

        // Time Range
        let time_options = vec![
            "All time",
            "Last 7 days",
            "Last 30 days",
            "Last 90 days",
            "Last year",
        ];
        let time_selection = Select::with_theme(theme)
            .with_prompt("Time range")
            .default(0)
            .items(&time_options)
            .interact()?;

        self.state.time_range = match time_selection {
            1 => Some("-7d".to_string()),
            2 => Some("-30d".to_string()),
            3 => Some("-90d".to_string()),
            4 => Some("-365d".to_string()),
            _ => None,
        };

        writeln!(
            term,
            "  {} Time range: {}",
            style("✓").green(),
            time_options[time_selection]
        )?;

        Ok(())
    }

    fn step_secret_scan(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<()> {
        writeln!(term, "\n{}", style("Step 2 of 8: Secret Scan").bold())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        let since_ts = self
            .state
            .time_range
            .as_deref()
            .and_then(crate::ui::time_parser::parse_time_input);

        let filters = SecretScanFilters {
            agents: if self.state.agents.is_empty() {
                None
            } else {
                Some(self.state.agents.clone())
            },
            workspaces: self.state.workspaces.clone(),
            since_ts,
            until_ts: None,
        };

        let config = SecretScanConfig::from_inputs(&[], &[])?;
        if !config.allowlist_raw.is_empty() || !config.denylist_raw.is_empty() {
            writeln!(
                term,
                "  {} Allowlist patterns: {} | Denylist patterns: {}",
                style("ℹ").blue(),
                config.allowlist_raw.len(),
                config.denylist_raw.len()
            )?;
        }

        let report = wizard_secret_scan(&self.state.db_path, &filters, &config)?;
        print_human_report(term, &report, 3)?;

        if report.summary.has_critical {
            writeln!(
                term,
                "  {} Critical secrets detected. Export is blocked without acknowledgement.",
                style("✗").red()
            )?;
            let ack: String = Input::with_theme(theme)
                .with_prompt("Type \"I UNDERSTAND\" to proceed")
                .interact_text()?;
            if ack.trim() != "I UNDERSTAND" {
                bail!("Export cancelled due to critical secrets");
            }
            writeln!(term, "  {} Acknowledged", style("✓").green())?;
        }

        Ok(())
    }

    fn step_security_config(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<()> {
        writeln!(
            term,
            "\n{}",
            style("Step 3 of 8: Security Configuration").bold()
        )?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        // Password
        let password = Password::with_theme(theme)
            .with_prompt("Archive password (min 8 characters)")
            .with_confirmation("Confirm password", "Passwords don't match")
            .validate_with(|input: &String| -> Result<(), &str> {
                if input.len() >= 8 {
                    Ok(())
                } else {
                    Err("Password must be at least 8 characters")
                }
            })
            .interact()?;

        self.state.password = Some(password);
        writeln!(term, "  {} Password set", style("✓").green())?;

        // Show password strength indicator
        let strength = self.estimate_password_strength(self.state.password.as_ref().unwrap());
        let strength_color = match strength {
            s if s >= 4 => style("Strong").green(),
            s if s >= 3 => style("Good").yellow(),
            s if s >= 2 => style("Fair").yellow(),
            _ => style("Weak").red(),
        };
        writeln!(term, "    Password strength: {}", strength_color)?;

        // Recovery secret
        self.state.generate_recovery = Confirm::with_theme(theme)
            .with_prompt("Generate recovery secret? (recommended)")
            .default(true)
            .interact()?;

        if self.state.generate_recovery {
            writeln!(
                term,
                "  {} Recovery secret will be generated",
                style("✓").green()
            )?;
        }

        // QR code
        self.state.generate_qr = Confirm::with_theme(theme)
            .with_prompt("Generate QR code for recovery? (for mobile access)")
            .default(false)
            .interact()?;

        if self.state.generate_qr {
            writeln!(term, "  {} QR code will be generated", style("✓").green())?;
        }

        Ok(())
    }

    fn step_site_config(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<()> {
        writeln!(
            term,
            "\n{}",
            style("Step 4 of 8: Site Configuration").bold()
        )?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        // Title
        self.state.title = Input::with_theme(theme)
            .with_prompt("Archive title")
            .default(self.state.title.clone())
            .interact_text()?;

        writeln!(term, "  {} Title: {}", style("✓").green(), self.state.title)?;

        // Description
        self.state.description = Input::with_theme(theme)
            .with_prompt("Description (shown on unlock page)")
            .default(self.state.description.clone())
            .interact_text()?;

        writeln!(term, "  {} Description set", style("✓").green())?;

        // Metadata privacy
        self.state.hide_metadata = Confirm::with_theme(theme)
            .with_prompt("Hide workspace paths and file names? (for privacy)")
            .default(false)
            .interact()?;

        if self.state.hide_metadata {
            writeln!(term, "  {} Metadata will be obfuscated", style("✓").green())?;
        }

        Ok(())
    }

    fn step_deployment_target(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<()> {
        writeln!(term, "\n{}", style("Step 5 of 8: Deployment Target").bold())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        let targets = vec![
            "Local export only (generate files)",
            "GitHub Pages (requires gh CLI)",
            "Cloudflare Pages (requires wrangler CLI)",
        ];

        let target_selection = Select::with_theme(theme)
            .with_prompt("Where would you like to deploy?")
            .default(0)
            .items(&targets)
            .interact()?;

        self.state.target = match target_selection {
            1 => DeployTarget::GitHubPages,
            2 => DeployTarget::CloudflarePages,
            _ => DeployTarget::Local,
        };

        writeln!(
            term,
            "  {} Target: {}",
            style("✓").green(),
            self.state.target
        )?;

        // Output directory
        self.state.output_dir = PathBuf::from(
            Input::<String>::with_theme(theme)
                .with_prompt("Output directory")
                .default("cass-export".to_string())
                .interact_text()?,
        );

        writeln!(
            term,
            "  {} Output: {}",
            style("✓").green(),
            self.state.output_dir.display()
        )?;

        // Repository name for remote deployment
        if self.state.target != DeployTarget::Local {
            let default_repo = format!("cass-archive-{}", chrono::Utc::now().format("%Y%m%d"));
            self.state.repo_name = Some(
                Input::<String>::with_theme(theme)
                    .with_prompt("Repository/project name")
                    .default(default_repo)
                    .interact_text()?,
            );

            writeln!(
                term,
                "  {} Repo: {}",
                style("✓").green(),
                self.state.repo_name.as_ref().unwrap()
            )?;
        }

        Ok(())
    }

    fn step_summary(&self, term: &mut Term, theme: &ColorfulTheme) -> Result<bool> {
        writeln!(
            term,
            "\n{}",
            style("Step 6 of 8: Pre-Publish Summary").bold()
        )?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        writeln!(term, "\n{}", style("Configuration Summary:").underlined())?;
        writeln!(term, "  Agents: {}", self.state.agents.join(", "))?;
        writeln!(
            term,
            "  Time range: {}",
            self.state.time_range.as_deref().unwrap_or("All time")
        )?;
        writeln!(term, "  Title: {}", self.state.title)?;
        writeln!(term, "  Target: {}", self.state.target)?;
        writeln!(term, "  Output: {}", self.state.output_dir.display())?;
        writeln!(
            term,
            "  Recovery secret: {}",
            if self.state.generate_recovery {
                "Yes"
            } else {
                "No"
            }
        )?;
        writeln!(
            term,
            "  QR code: {}",
            if self.state.generate_qr { "Yes" } else { "No" }
        )?;
        writeln!(
            term,
            "  Hide metadata: {}",
            if self.state.hide_metadata {
                "Yes"
            } else {
                "No"
            }
        )?;

        writeln!(term)?;

        Ok(Confirm::with_theme(theme)
            .with_prompt("Proceed with export?")
            .default(true)
            .interact()?)
    }

    fn step_export(&mut self, term: &mut Term) -> Result<()> {
        writeln!(term, "\n{}", style("Step 7 of 8: Export Progress").bold())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        // Phase 0: Size estimation and limit checking
        writeln!(term, "\n  Estimating export size...")?;

        let since_ts = self
            .state
            .time_range
            .as_deref()
            .and_then(crate::ui::time_parser::parse_time_input);

        let agents: Vec<String> = self.state.agents.to_vec();
        let estimate = SizeEstimate::from_database(
            &self.state.db_path,
            if agents.is_empty() {
                None
            } else {
                Some(&agents)
            },
            since_ts,
            None,
        )?;

        // Display estimate
        writeln!(term)?;
        for line in estimate.format_display().lines() {
            writeln!(term, "  {}", line)?;
        }
        writeln!(term)?;

        // Check limits
        match estimate.check_limits() {
            SizeLimitResult::Ok => {
                writeln!(term, "  {} Size within limits", style("✓").green())?;
            }
            SizeLimitResult::Warning(warning) => {
                writeln!(term, "  {} {}", style("⚠").yellow(), warning)?;
                writeln!(term)?;

                let theme = ColorfulTheme::default();
                if !Confirm::with_theme(&theme)
                    .with_prompt("Continue with export?")
                    .default(true)
                    .interact()?
                {
                    bail!("Export cancelled due to size warning");
                }
            }
            SizeLimitResult::ExceedsLimit(error) => {
                writeln!(term)?;
                writeln!(term, "  {} {}", style("✗").red(), error)?;
                writeln!(term)?;
                bail!("Export blocked: {}", error);
            }
        }

        writeln!(term)?;

        // Create output directory
        if !self.state.output_dir.exists() {
            std::fs::create_dir_all(&self.state.output_dir)?;
        }

        let export_db_path = self.state.output_dir.join("export.db");

        // Phase 1: Database Export with progress
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_message("Filtering and exporting conversations...");

        // Build export filter with workspaces
        let workspaces = self.state.workspaces.clone();
        let since_dt = self.state.time_range.as_deref().and_then(|s| {
            crate::ui::time_parser::parse_time_input(s)
                .and_then(chrono::DateTime::from_timestamp_millis)
        });

        let filter = ExportFilter {
            agents: Some(self.state.agents.clone()),
            workspaces,
            since: since_dt,
            until: None,
            path_mode: if self.state.hide_metadata {
                PathMode::Hash
            } else {
                PathMode::Relative
            },
        };

        let engine = ExportEngine::new(&self.state.db_path, &export_db_path, filter);
        let running = Arc::new(AtomicBool::new(true));

        let stats = engine.execute(
            |current, total| {
                if total > 0 {
                    pb.set_message(format!("Exporting... {}/{} conversations", current, total));
                }
            },
            Some(running),
        )?;

        pb.finish_with_message(format!(
            "✓ Exported {} conversations, {} messages",
            stats.conversations_processed, stats.messages_processed
        ));

        // Phase 2: Encryption
        let pb2 = ProgressBar::new_spinner();
        pb2.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        pb2.enable_steady_tick(Duration::from_millis(100));
        pb2.set_message("Encrypting archive...");

        // Initialize encryption engine
        let mut enc_engine = EncryptionEngine::default();

        // Add password slot
        if let Some(password) = &self.state.password {
            enc_engine.add_password_slot(password)?;
        }

        // Generate and add recovery secret if requested
        if self.state.generate_recovery {
            let mut recovery_bytes = [0u8; 32];
            use rand::RngCore;
            rand::rngs::OsRng.fill_bytes(&mut recovery_bytes);
            enc_engine.add_recovery_slot(&recovery_bytes)?;
            self.state.recovery_secret = Some(recovery_bytes.to_vec());
        }

        // Encrypt the database
        let config = enc_engine.encrypt_file(&export_db_path, &self.state.output_dir, |_, _| {})?;

        // Write config.json
        let config_path = self.state.output_dir.join("config.json");
        std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;

        pb2.finish_with_message("✓ Encryption complete");

        // Phase 3: Build static site bundle
        let pb3 = ProgressBar::new_spinner();
        pb3.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        pb3.enable_steady_tick(Duration::from_millis(100));
        pb3.set_message("Building static site bundle...");

        // Create bundle configuration
        let bundle_config = BundleConfig {
            title: self.state.title.clone(),
            description: self.state.description.clone(),
            hide_metadata: self.state.hide_metadata,
            recovery_secret: self.state.recovery_secret.clone(),
            generate_qr: self.state.generate_qr,
        };

        // Build the bundle - creates site/ and private/ directories
        let bundle_output_dir = self
            .state
            .output_dir
            .parent()
            .map(|p| {
                p.join(format!(
                    "{}-bundle",
                    self.state
                        .output_dir
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                ))
            })
            .unwrap_or_else(|| self.state.output_dir.join("bundle"));

        let builder = BundleBuilder::with_config(bundle_config);
        let bundle_result =
            builder.build(&self.state.output_dir, &bundle_output_dir, |phase, msg| {
                pb3.set_message(format!("{}: {}", phase, msg));
            })?;

        pb3.finish_with_message(format!(
            "✓ Bundle complete: {} files, fingerprint {}",
            bundle_result.total_files,
            &bundle_result.fingerprint[..8]
        ));

        // Phase 4: Post-export verification
        let warnings = BundleVerifier::verify(&bundle_result.site_dir)?;
        if !warnings.is_empty() {
            writeln!(term)?;
            writeln!(term, "  {} Size warnings:", style("⚠").yellow())?;
            for warning in &warnings {
                writeln!(term, "    {}", warning)?;
            }
        }

        // Clean up temporary export.db (encrypted version is in payload/)
        std::fs::remove_file(&export_db_path).ok();

        writeln!(term)?;
        writeln!(
            term,
            "  {} Site directory (deploy this): {}",
            style("✓").green(),
            style(bundle_result.site_dir.display()).cyan()
        )?;
        writeln!(
            term,
            "  {} Private directory (keep secure): {}",
            style("✓").green(),
            style(bundle_result.private_dir.display()).cyan()
        )?;
        writeln!(
            term,
            "  {} Integrity fingerprint: {}",
            style("✓").green(),
            style(&bundle_result.fingerprint).cyan()
        )?;

        // Display recovery secret location if generated
        if self.state.recovery_secret.is_some() {
            writeln!(term)?;
            writeln!(
                term,
                "  {} Recovery secret saved to: {}",
                style("⚠").yellow().bold(),
                style(
                    bundle_result
                        .private_dir
                        .join("recovery-secret.txt")
                        .display()
                )
                .cyan()
            )?;
            writeln!(
                term,
                "  {}",
                style("Store this file securely - it can unlock your archive if you forget the password.").dim()
            )?;
        }

        if self.state.generate_qr {
            writeln!(
                term,
                "  {} QR codes saved to private directory",
                style("✓").green()
            )?;
        }

        Ok(())
    }

    fn step_deploy(&self, term: &mut Term) -> Result<()> {
        writeln!(term, "\n{}", style("Step 8 of 8: Deployment").bold())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        match self.state.target {
            DeployTarget::Local => {
                writeln!(term)?;
                writeln!(term, "{}", style("✓ Export complete!").green().bold())?;
                writeln!(term)?;
                writeln!(
                    term,
                    "Your archive has been exported to: {}",
                    style(self.state.output_dir.display()).cyan()
                )?;
                writeln!(term)?;
                writeln!(term, "To preview locally, run:")?;
                writeln!(
                    term,
                    "  {}",
                    style(format!(
                        "cd {} && python -m http.server 8080",
                        self.state.output_dir.display()
                    ))
                    .dim()
                )?;
                writeln!(term)?;
                writeln!(
                    term,
                    "Then open {} in your browser.",
                    style("http://localhost:8080").cyan()
                )?;
            }
            DeployTarget::GitHubPages => {
                writeln!(term, "  {} GitHub Pages deployment...", style("→").cyan())?;

                // TODO: Actually deploy using pages::deploy_github
                writeln!(
                    term,
                    "  {} GitHub Pages deployment not yet implemented",
                    style("⚠").yellow()
                )?;
                writeln!(term)?;
                writeln!(
                    term,
                    "To deploy manually, push the {} directory to a gh-pages branch.",
                    self.state.output_dir.display()
                )?;
            }
            DeployTarget::CloudflarePages => {
                writeln!(
                    term,
                    "  {} Cloudflare Pages deployment...",
                    style("→").cyan()
                )?;

                // TODO: Actually deploy using pages::deploy_cloudflare
                writeln!(
                    term,
                    "  {} Cloudflare Pages deployment not yet implemented",
                    style("⚠").yellow()
                )?;
                writeln!(term)?;
                writeln!(
                    term,
                    "To deploy manually, use wrangler to deploy the {} directory.",
                    self.state.output_dir.display()
                )?;
            }
        }

        writeln!(term)?;
        Ok(())
    }

    /// Estimate password strength (simple heuristic)
    fn estimate_password_strength(&self, password: &str) -> u8 {
        let mut score = 0u8;

        // Length
        if password.len() >= 12 {
            score += 2;
        } else if password.len() >= 8 {
            score += 1;
        }

        // Character variety
        let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
        let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
        let has_digit = password.chars().any(|c| c.is_ascii_digit());
        let has_special = password.chars().any(|c| !c.is_alphanumeric());

        if has_lower {
            score += 1;
        }
        if has_upper {
            score += 1;
        }
        if has_digit {
            score += 1;
        }
        if has_special {
            score += 1;
        }

        score.min(5)
    }
}
