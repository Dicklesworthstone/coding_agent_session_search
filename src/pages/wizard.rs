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
use crate::pages::confirmation::{
    ConfirmationConfig, ConfirmationFlow, ConfirmationStep, PasswordStrengthAction, StepValidation,
    validate_unencrypted_ack, unencrypted_warning_lines, UNENCRYPTED_ACK_PHRASE,
};
use crate::pages::password::{validate_password, format_strength_inline, PasswordStrength};
use crate::pages::docs::{DocConfig, DocumentationGenerator};
use crate::pages::encrypt::EncryptionEngine;
use crate::pages::export::{ExportEngine, ExportFilter, PathMode};
use crate::pages::secret_scan::{
    SecretScanConfig, SecretScanFilters, print_human_report, wizard_secret_scan,
};
use crate::pages::size::{BundleVerifier, SizeEstimate, SizeLimitResult};
use crate::pages::summary::{
    ExclusionSet, PrePublishSummary, SummaryFilters, SummaryGenerator, format_size,
};
use crate::storage::sqlite::SqliteStorage;
use rusqlite::Connection;

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

    // Pre-publish summary and exclusions
    pub exclusions: ExclusionSet,
    pub last_summary: Option<PrePublishSummary>,

    // Secret scan results
    pub secret_scan_has_findings: bool,
    pub secret_scan_has_critical: bool,
    pub secret_scan_count: usize,

    // Password entropy
    pub password_entropy_bits: f64,

    // Unencrypted export mode (DANGEROUS)
    pub no_encryption: bool,
    pub unencrypted_confirmed: bool,

    // Attachment support
    pub include_attachments: bool,
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
            exclusions: ExclusionSet::new(),
            last_summary: None,
            secret_scan_has_findings: false,
            secret_scan_has_critical: false,
            secret_scan_count: 0,
            password_entropy_bits: 0.0,
            no_encryption: false,
            unencrypted_confirmed: false,
            include_attachments: false,
        }
    }
}

pub struct PagesWizard {
    state: WizardState,
    no_encryption_mode: bool,
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
            no_encryption_mode: false,
        }
    }

    /// Set whether to skip encryption (DANGEROUS - requires explicit confirmation).
    pub fn set_no_encryption(&mut self, no_encryption: bool) {
        self.no_encryption_mode = no_encryption;
        self.state.no_encryption = no_encryption;
    }

    /// Set whether to include attachments in the export.
    pub fn set_include_attachments(&mut self, include: bool) {
        self.state.include_attachments = include;
    }

    pub fn run(&mut self) -> Result<()> {
        let mut term = Term::stdout();
        let theme = ColorfulTheme::default();

        term.clear_screen()?;
        self.print_header(&mut term)?;

        // If no-encryption mode, show explicit warning at the start
        if self.no_encryption_mode && !self.step_unencrypted_warning(&mut term, &theme)? {
            writeln!(term, "{}", style("Export cancelled.").yellow())?;
            return Ok(());
        }

        // Step 1: Content Selection
        self.step_content_selection(&mut term, &theme)?;

        // Step 2: Secret Scan
        self.step_secret_scan(&mut term, &theme)?;

        // Step 3: Security Configuration (skip if no encryption)
        if !self.no_encryption_mode {
            self.step_security_config(&mut term, &theme)?;
        }

        // Step 4: Site Configuration
        self.step_site_config(&mut term, &theme)?;

        // Step 5: Deployment Target
        self.step_deployment_target(&mut term, &theme)?;

        // Step 6: Pre-Publish Summary
        if !self.step_summary(&mut term, &theme)? {
            writeln!(term, "{}", style("Export cancelled.").yellow())?;
            return Ok(());
        }

        // Step 7: Safety Confirmation
        if !self.step_confirmation(&mut term, &theme)? {
            writeln!(term, "{}", style("Export cancelled.").yellow())?;
            return Ok(());
        }

        // Step 8: Export Progress
        self.step_export(&mut term)?;

        // Step 9: Deploy (if not local)
        self.step_deploy(&mut term)?;

        Ok(())
    }

    /// Step for unencrypted export warning and confirmation.
    fn step_unencrypted_warning(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<bool> {
        writeln!(term)?;
        writeln!(term, "{}", style("⚠️  SECURITY WARNING").red().bold())?;
        writeln!(term, "{}", style("━".repeat(60)).red())?;
        writeln!(term)?;

        for line in unencrypted_warning_lines() {
            if line.is_empty() {
                writeln!(term)?;
            } else {
                writeln!(term, "  {}", line)?;
            }
        }

        writeln!(term)?;
        writeln!(term, "{}", style("━".repeat(60)).red())?;
        writeln!(term)?;
        writeln!(term, "To proceed with unencrypted export, type exactly:")?;
        writeln!(term)?;
        writeln!(term, "  {}", style(UNENCRYPTED_ACK_PHRASE).cyan().bold())?;
        writeln!(term)?;

        loop {
            let input: String = Input::with_theme(theme)
                .with_prompt("Your input (or \"cancel\" to abort)")
                .interact_text()?;

            if input.trim().to_lowercase() == "cancel" {
                return Ok(false);
            }

            match validate_unencrypted_ack(&input) {
                StepValidation::Passed => {
                    // Additional y/N confirmation
                    writeln!(term)?;
                    let confirmed = Confirm::with_theme(theme)
                        .with_prompt("Are you ABSOLUTELY SURE you want to export WITHOUT encryption?")
                        .default(false)
                        .interact()?;

                    if !confirmed {
                        writeln!(term)?;
                        writeln!(term, "  {}", style("Good choice. Export cancelled.").green())?;
                        writeln!(term, "  Remove --no-encryption to export with encryption (recommended).")?;
                        return Ok(false);
                    }

                    self.state.unencrypted_confirmed = true;
                    writeln!(term)?;
                    writeln!(term, "  {} Unencrypted export acknowledged", style("⚠").yellow())?;
                    writeln!(term, "  {}", style("Proceeding without encryption...").dim())?;
                    return Ok(true);
                }
                StepValidation::Failed(msg) => {
                    writeln!(term, "  {} {}", style("✗").red(), msg)?;
                }
            }
        }
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
        writeln!(term, "\n{}", style("Step 1 of 9: Content Selection").bold())?;
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
        writeln!(term, "\n{}", style("Step 2 of 9: Secret Scan").bold())?;
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

        // Save secret scan results to state for confirmation flow
        self.state.secret_scan_has_findings = report.summary.total > 0;
        self.state.secret_scan_has_critical = report.summary.has_critical;
        self.state.secret_scan_count = report.summary.total;

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
            style("Step 3 of 9: Security Configuration").bold()
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

        self.state.password = Some(password.clone());
        writeln!(term, "  {} Password set", style("✓").green())?;

        // Validate password using new password strength module
        let validation = validate_password(&password);

        // Calculate and save password entropy for confirmation flow
        self.state.password_entropy_bits = validation.entropy_bits;

        // Show password strength indicator with visual bar
        writeln!(
            term,
            "    Password strength: {}",
            format_strength_inline(&validation)
        )?;
        writeln!(
            term,
            "    Entropy: {:.0} bits",
            validation.entropy_bits
        )?;

        // Show improvement suggestions if not strong
        if validation.strength != PasswordStrength::Strong && !validation.suggestions.is_empty() {
            writeln!(term, "    {}", style("Suggestions:").dim())?;
            for suggestion in &validation.suggestions {
                writeln!(term, "      {} {}", style("•").dim(), style(suggestion).dim())?;
            }
        }

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
            style("Step 4 of 9: Site Configuration").bold()
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
        writeln!(term, "\n{}", style("Step 5 of 9: Deployment Target").bold())?;
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

    fn step_summary(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<bool> {
        writeln!(
            term,
            "\n{}",
            style("Step 6 of 9: Pre-Publish Summary").bold()
        )?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        // Generate comprehensive summary from database
        writeln!(term, "\n  Generating summary...")?;
        let summary = self.generate_prepublish_summary()?;
        self.state.last_summary = Some(summary.clone());

        // Display content overview
        writeln!(term, "\n{}", style("📊 CONTENT OVERVIEW").bold().cyan())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;
        writeln!(
            term,
            "  Conversations: {}",
            style(summary.total_conversations).green()
        )?;
        writeln!(
            term,
            "  Messages:      {}",
            style(summary.total_messages).green()
        )?;
        writeln!(
            term,
            "  Characters:    {} (~{})",
            summary.total_characters,
            format_size(summary.total_characters)
        )?;
        writeln!(
            term,
            "  Archive Size:  ~{} (estimated, compressed + encrypted)",
            style(format_size(summary.estimated_size_bytes)).yellow()
        )?;

        // Display date range
        writeln!(term, "\n{}", style("📅 DATE RANGE").bold().cyan())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;
        if let (Some(earliest), Some(latest)) =
            (&summary.earliest_timestamp, &summary.latest_timestamp)
        {
            let days = (*latest - *earliest).num_days();
            writeln!(
                term,
                "  From: {}  To: {}  ({} days)",
                style(earliest.format("%Y-%m-%d")).white(),
                style(latest.format("%Y-%m-%d")).white(),
                days
            )?;

            // Show activity histogram (simplified sparkline)
            if !summary.date_histogram.is_empty() {
                let bars = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

                // Group by month for display
                let mut months: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for entry in &summary.date_histogram {
                    if entry.date.len() >= 7 {
                        let month = &entry.date[0..7];
                        *months.entry(month.to_string()).or_insert(0) += entry.message_count;
                    }
                }

                if !months.is_empty() {
                    let month_max = months.values().max().copied().unwrap_or(1);
                    let sparkline: String = months
                        .values()
                        .map(|&count| {
                            let idx = (count * 7 / month_max).min(7);
                            bars[idx]
                        })
                        .collect();
                    writeln!(term, "  Activity: {}", style(sparkline).cyan())?;
                }
            }
        } else {
            writeln!(term, "  No date information available")?;
        }

        // Display workspaces
        writeln!(
            term,
            "\n{} ({})",
            style("📁 WORKSPACES").bold().cyan(),
            summary.workspaces.len()
        )?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;
        for (_idx, ws) in summary.workspaces.iter().enumerate().take(5) {
            let included_marker =
                if ws.included && !self.state.exclusions.is_workspace_excluded(&ws.path) {
                    style("✓").green()
                } else {
                    style("✗").red()
                };
            writeln!(
                term,
                "  {} {} ({} conversations)",
                included_marker,
                style(&ws.display_name).white(),
                ws.conversation_count
            )?;
            if !ws.sample_titles.is_empty() {
                let titles: Vec<_> = ws
                    .sample_titles
                    .iter()
                    .take(2)
                    .map(|t| {
                        if t.len() > 30 {
                            format!("{}...", &t[..27])
                        } else {
                            t.clone()
                        }
                    })
                    .collect();
                writeln!(
                    term,
                    "      {}",
                    style(format!("\"{}\"", titles.join("\", \""))).dim()
                )?;
            }
        }
        if summary.workspaces.len() > 5 {
            writeln!(
                term,
                "  {} and {} more...",
                style("...").dim(),
                summary.workspaces.len() - 5
            )?;
        }

        // Display agents
        writeln!(term, "\n{}", style("🤖 AGENTS").bold().cyan())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;
        for agent in &summary.agents {
            writeln!(
                term,
                "  • {}: {} conversations ({:.0}%)",
                style(&agent.name).white(),
                agent.conversation_count,
                agent.percentage
            )?;
        }

        // Display security status
        writeln!(term, "\n{}", style("🔒 SECURITY").bold().cyan())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;
        if let Some(enc) = &summary.encryption_config {
            writeln!(term, "  Encryption: {}", enc.algorithm)?;
            writeln!(term, "  Key Derivation: {}", enc.key_derivation)?;
            writeln!(term, "  Key Slots: {}", enc.key_slot_count)?;
        } else {
            writeln!(term, "  Encryption: AES-256-GCM")?;
            writeln!(term, "  Key Derivation: Argon2id")?;
        }

        // Secret scan status
        let secret_status = if summary.secret_scan.total_findings == 0 {
            style("✓ No secrets detected".to_string()).green()
        } else if summary.secret_scan.has_critical {
            style(format!(
                "⚠️  {} issues (CRITICAL)",
                summary.secret_scan.total_findings
            ))
            .red()
        } else {
            style(format!(
                "⚠️  {} issues found",
                summary.secret_scan.total_findings
            ))
            .yellow()
        };
        writeln!(term, "  Secret Scan: {}", secret_status)?;

        // Configuration summary
        writeln!(term, "\n{}", style("⚙️  CONFIGURATION").bold().cyan())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;
        writeln!(term, "  Title: {}", self.state.title)?;
        writeln!(term, "  Target: {}", self.state.target)?;
        writeln!(term, "  Output: {}", self.state.output_dir.display())?;
        writeln!(
            term,
            "  Recovery Key: {}",
            if self.state.generate_recovery {
                "Yes"
            } else {
                "No"
            }
        )?;
        writeln!(
            term,
            "  QR Code: {}",
            if self.state.generate_qr { "Yes" } else { "No" }
        )?;

        // Exclusion summary
        let (ws_excluded, conv_excluded, pattern_excluded) =
            self.state.exclusions.exclusion_counts();
        if ws_excluded > 0 || conv_excluded > 0 || pattern_excluded > 0 {
            writeln!(term, "\n{}", style("🚫 EXCLUSIONS").bold().yellow())?;
            writeln!(term, "{}", style("─".repeat(40)).dim())?;
            if ws_excluded > 0 {
                writeln!(term, "  {} workspace(s) excluded", ws_excluded)?;
            }
            if conv_excluded > 0 {
                writeln!(term, "  {} conversation(s) excluded", conv_excluded)?;
            }
            if pattern_excluded > 0 {
                writeln!(term, "  {} pattern(s) active", pattern_excluded)?;
            }
        }

        writeln!(term)?;

        // Options menu
        loop {
            let options = vec![
                "✓ Proceed with export",
                "📁 View/Edit workspace exclusions",
                "✗ Cancel export",
            ];

            let selection = Select::with_theme(theme)
                .with_prompt("What would you like to do?")
                .items(&options)
                .default(0)
                .interact()?;

            match selection {
                0 => return Ok(true), // Proceed
                1 => {
                    // Edit workspace exclusions
                    self.edit_workspace_exclusions(term, theme, &summary)?;
                }
                2 => return Ok(false), // Cancel
                _ => unreachable!(),
            }
        }
    }

    /// Generate the pre-publish summary from the database.
    fn generate_prepublish_summary(&self) -> Result<PrePublishSummary> {
        let conn = Connection::open(&self.state.db_path)
            .context("Failed to open database for summary generation")?;

        let since_ts = self
            .state
            .time_range
            .as_deref()
            .and_then(crate::ui::time_parser::parse_time_input);

        let filters = SummaryFilters {
            agents: if self.state.agents.is_empty() {
                None
            } else {
                Some(self.state.agents.clone())
            },
            workspaces: self
                .state
                .workspaces
                .as_ref()
                .map(|ws| ws.iter().map(|p| p.to_string_lossy().to_string()).collect()),
            since_ts,
            until_ts: None,
        };

        let generator = SummaryGenerator::new(&conn);
        let summary = generator.generate_with_exclusions(Some(&filters), &self.state.exclusions)?;

        Ok(summary)
    }

    /// Interactive workspace exclusion editing.
    fn edit_workspace_exclusions(
        &mut self,
        term: &mut Term,
        theme: &ColorfulTheme,
        summary: &PrePublishSummary,
    ) -> Result<()> {
        writeln!(term, "\n{}", style("Workspace Exclusions").bold())?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        if summary.workspaces.is_empty() {
            writeln!(term, "  No workspaces to configure.")?;
            return Ok(());
        }

        // Build list of workspaces with current inclusion status
        let items: Vec<String> = summary
            .workspaces
            .iter()
            .map(|ws| {
                format!(
                    "{} ({} conversations)",
                    ws.display_name, ws.conversation_count
                )
            })
            .collect();

        // Determine which are currently selected (included)
        let defaults: Vec<bool> = summary
            .workspaces
            .iter()
            .map(|ws| !self.state.exclusions.is_workspace_excluded(&ws.path))
            .collect();

        let selections = MultiSelect::with_theme(theme)
            .with_prompt("Select workspaces to INCLUDE (unselected will be excluded)")
            .items(&items)
            .defaults(&defaults)
            .interact()?;

        // Update exclusions based on selections
        for (idx, ws) in summary.workspaces.iter().enumerate() {
            if selections.contains(&idx) {
                // Include this workspace (remove from exclusions)
                self.state.exclusions.include_workspace(&ws.path);
            } else {
                // Exclude this workspace
                self.state.exclusions.exclude_workspace(&ws.path);
            }
        }

        let (ws_excluded, _, _) = self.state.exclusions.exclusion_counts();
        writeln!(
            term,
            "  {} {} workspace(s) now excluded",
            style("✓").green(),
            ws_excluded
        )?;

        Ok(())
    }

    /// Multi-step safety confirmation flow.
    fn step_confirmation(&mut self, term: &mut Term, theme: &ColorfulTheme) -> Result<bool> {
        writeln!(
            term,
            "\n{}",
            style("Step 7 of 9: Safety Confirmation").bold()
        )?;
        writeln!(term, "{}", style("─".repeat(40)).dim())?;

        // Build confirmation configuration
        let target_domain = if self.state.target != DeployTarget::Local {
            self.state
                .repo_name
                .as_ref()
                .map(|name| match self.state.target {
                    DeployTarget::GitHubPages => format!("{}.github.io", name),
                    DeployTarget::CloudflarePages => format!("{}.pages.dev", name),
                    DeployTarget::Local => String::new(),
                })
        } else {
            None
        };

        let config = ConfirmationConfig {
            has_secrets: self.state.secret_scan_has_findings,
            has_critical_secrets: self.state.secret_scan_has_critical,
            secret_count: self.state.secret_scan_count,
            target_domain,
            is_remote_publish: self.state.target != DeployTarget::Local,
            password_entropy_bits: self.state.password_entropy_bits,
            has_recovery_key: self.state.generate_recovery,
            recovery_key_phrase: None, // Will be set after generation
            summary: self
                .state
                .last_summary
                .clone()
                .expect("Summary should be generated before confirmation"),
        };

        let mut flow = ConfirmationFlow::new(config);

        // Process each confirmation step
        loop {
            match flow.current_step() {
                ConfirmationStep::SecretScanAcknowledgment => {
                    if !self.confirm_secret_ack(term, theme, &flow)? {
                        return Ok(false);
                    }
                    flow.complete_current_step();
                }
                ConfirmationStep::ContentReview => {
                    if !self.confirm_content_review(term, theme, &flow)? {
                        return Ok(false);
                    }
                    flow.complete_current_step();
                }
                ConfirmationStep::PublicPublishingWarning => {
                    if !self.confirm_public_warning(term, theme, &flow)? {
                        return Ok(false);
                    }
                    flow.complete_current_step();
                }
                ConfirmationStep::PasswordStrengthWarning => {
                    match self.confirm_password_strength(term, theme, &mut flow)? {
                        PasswordStrengthAction::SetStronger => {
                            // User wants to set a stronger password - go back
                            writeln!(
                                term,
                                "\n  {} Returning to security configuration...",
                                style("←").cyan()
                            )?;
                            return Ok(false);
                        }
                        PasswordStrengthAction::ProceedAnyway => {
                            flow.complete_current_step();
                        }
                        PasswordStrengthAction::Abort => {
                            return Ok(false);
                        }
                    }
                }
                ConfirmationStep::RecoveryKeyBackup => {
                    if !self.confirm_recovery_key(term, theme, &flow)? {
                        return Ok(false);
                    }
                    flow.complete_current_step();
                }
                ConfirmationStep::FinalConfirmation => {
                    if !self.confirm_final(term, theme, &mut flow)? {
                        return Ok(false);
                    }
                    flow.complete_current_step();
                    break;
                }
            }
        }

        writeln!(
            term,
            "\n  {} All safety checks completed",
            style("✓").green()
        )?;
        Ok(true)
    }

    /// Confirm acknowledgment of detected secrets.
    fn confirm_secret_ack(
        &self,
        term: &mut Term,
        theme: &ColorfulTheme,
        flow: &ConfirmationFlow,
    ) -> Result<bool> {
        writeln!(
            term,
            "\n  {}",
            style("⚠️  SECRETS DETECTED").yellow().bold()
        )?;
        writeln!(term)?;
        writeln!(
            term,
            "  The secret scan found {} potential sensitive data item(s).",
            flow.config().secret_count
        )?;
        writeln!(term)?;
        writeln!(
            term,
            "  Even though the export will be encrypted, publishing content"
        )?;
        writeln!(term, "  containing secrets carries additional risk:")?;
        writeln!(term)?;
        writeln!(
            term,
            "  {} If your password is weak, secrets could be exposed",
            style("⚠").yellow()
        )?;
        writeln!(
            term,
            "  {} Secrets may remain valid if encryption is ever compromised",
            style("⚠").yellow()
        )?;
        writeln!(term)?;

        loop {
            let input: String = Input::with_theme(theme)
                .with_prompt("Type \"I understand the risks\" to proceed (or \"abort\" to cancel)")
                .interact_text()?;

            if input.trim().to_lowercase() == "abort" {
                return Ok(false);
            }

            match flow.validate_secret_ack(&input) {
                StepValidation::Passed => {
                    writeln!(term, "  {} Secrets acknowledged", style("✓").green())?;
                    return Ok(true);
                }
                StepValidation::Failed(msg) => {
                    writeln!(term, "  {} {}", style("✗").red(), msg)?;
                }
            }
        }
    }

    /// Confirm review of content summary.
    fn confirm_content_review(
        &self,
        term: &mut Term,
        theme: &ColorfulTheme,
        flow: &ConfirmationFlow,
    ) -> Result<bool> {
        writeln!(term, "\n  {}", style("📋 CONTENT REVIEW").cyan().bold())?;
        writeln!(term)?;
        writeln!(term, "  You are about to export:")?;
        writeln!(term)?;
        writeln!(
            term,
            "  • {} conversations from {} workspaces",
            flow.config().summary.total_conversations,
            flow.config().summary.workspaces.len()
        )?;
        writeln!(
            term,
            "  • {} messages",
            flow.config().summary.total_messages
        )?;
        writeln!(
            term,
            "  • Content from: {}",
            flow.config()
                .summary
                .agents
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        writeln!(term)?;

        let confirmed = Confirm::with_theme(theme)
            .with_prompt("Have you reviewed the content summary?")
            .default(false)
            .interact()?;

        if confirmed {
            writeln!(term, "  {} Content reviewed", style("✓").green())?;
        }
        Ok(confirmed)
    }

    /// Confirm public publishing warning.
    fn confirm_public_warning(
        &self,
        term: &mut Term,
        theme: &ColorfulTheme,
        flow: &ConfirmationFlow,
    ) -> Result<bool> {
        let domain = flow
            .config()
            .target_domain
            .as_deref()
            .unwrap_or("your-site");

        writeln!(
            term,
            "\n  {}",
            style("🌐 PUBLIC PUBLISHING WARNING").yellow().bold()
        )?;
        writeln!(term)?;
        writeln!(term, "  You are about to publish to:")?;
        writeln!(term, "    {}", style(format!("https://{}/", domain)).cyan())?;
        writeln!(term)?;
        writeln!(
            term,
            "  {} This URL will be publicly accessible on the internet",
            style("⚠").yellow()
        )?;
        writeln!(
            term,
            "  {} Anyone with the URL can download the encrypted archive",
            style("⚠").yellow()
        )?;
        writeln!(
            term,
            "  {} The security depends entirely on your password strength",
            style("⚠").yellow()
        )?;
        writeln!(term)?;

        loop {
            let input: String = Input::with_theme(theme)
                .with_prompt(format!(
                    "Type \"publish to {}\" to confirm (or \"abort\" to cancel)",
                    domain
                ))
                .interact_text()?;

            if input.trim().to_lowercase() == "abort" {
                return Ok(false);
            }

            match flow.validate_public_warning(&input) {
                StepValidation::Passed => {
                    writeln!(term, "  {} Public URL confirmed", style("✓").green())?;
                    return Ok(true);
                }
                StepValidation::Failed(msg) => {
                    writeln!(term, "  {} {}", style("✗").red(), msg)?;
                }
            }
        }
    }

    /// Handle password strength warning.
    fn confirm_password_strength(
        &self,
        term: &mut Term,
        theme: &ColorfulTheme,
        flow: &mut ConfirmationFlow,
    ) -> Result<PasswordStrengthAction> {
        writeln!(
            term,
            "\n  {}",
            style("🔐 PASSWORD STRENGTH WARNING").yellow().bold()
        )?;
        writeln!(term)?;
        writeln!(
            term,
            "  Your password has estimated entropy of {:.0} bits.",
            self.state.password_entropy_bits
        )?;
        writeln!(term)?;
        writeln!(term, "  Recommended minimum: 60 bits")?;
        writeln!(term)?;
        writeln!(
            term,
            "  A password with low entropy could potentially be cracked"
        )?;
        writeln!(
            term,
            "  by a determined attacker with sufficient resources."
        )?;
        writeln!(term)?;
        writeln!(term, "  For long-term security, consider:")?;
        writeln!(term, "  • Using a longer password (16+ characters)")?;
        writeln!(term, "  • Including numbers, symbols, and mixed case")?;
        writeln!(term, "  • Using a passphrase of 5+ random words")?;
        writeln!(term)?;

        let options = vec![
            "[S] Set a stronger password",
            "[P] Proceed with current password (not recommended)",
            "[A] Abort export",
        ];

        let selection = Select::with_theme(theme)
            .with_prompt("What would you like to do?")
            .items(&options)
            .default(0)
            .interact()?;

        let action = match selection {
            0 => PasswordStrengthAction::SetStronger,
            1 => {
                writeln!(
                    term,
                    "  {} Password warning acknowledged",
                    style("⚠").yellow()
                )?;
                PasswordStrengthAction::ProceedAnyway
            }
            _ => PasswordStrengthAction::Abort,
        };

        flow.set_password_action(action);
        Ok(action)
    }

    /// Confirm recovery key backup.
    fn confirm_recovery_key(
        &self,
        term: &mut Term,
        theme: &ColorfulTheme,
        _flow: &ConfirmationFlow,
    ) -> Result<bool> {
        writeln!(
            term,
            "\n  {}",
            style("💾 RECOVERY KEY BACKUP").cyan().bold()
        )?;
        writeln!(term)?;
        writeln!(
            term,
            "  A recovery key will be generated. This is the ONLY way"
        )?;
        writeln!(term, "  to recover your data if you forget your password.")?;
        writeln!(term)?;
        writeln!(
            term,
            "  {} If you lose both your password AND the recovery key,",
            style("⚠").yellow()
        )?;
        writeln!(term, "     your data will be permanently inaccessible.")?;
        writeln!(term)?;

        let confirmed = Confirm::with_theme(theme)
            .with_prompt("I understand that I must save the recovery key securely")
            .default(false)
            .interact()?;

        if confirmed {
            writeln!(
                term,
                "  {} Recovery key backup confirmed",
                style("✓").green()
            )?;
        }
        Ok(confirmed)
    }

    /// Final double-enter confirmation.
    fn confirm_final(
        &self,
        term: &mut Term,
        theme: &ColorfulTheme,
        flow: &mut ConfirmationFlow,
    ) -> Result<bool> {
        writeln!(term, "\n  {}", style("✓ FINAL CONFIRMATION").green().bold())?;
        writeln!(term)?;
        writeln!(term, "  Ready to publish:")?;
        writeln!(term)?;

        // Show completed steps
        for (_, label) in flow.completed_steps_summary() {
            writeln!(term, "  {} {}", style("✓").green(), label)?;
        }
        writeln!(term)?;

        // Show target info
        if self.state.target != DeployTarget::Local {
            if let Some(domain) = &flow.config().target_domain {
                writeln!(term, "  Target: https://{}/", domain)?;
            }
        } else {
            writeln!(
                term,
                "  Target: {} (local)",
                self.state.output_dir.display()
            )?;
        }

        if let Some(summary) = &self.state.last_summary {
            writeln!(
                term,
                "  Size: ~{}",
                format_size(summary.estimated_size_bytes)
            )?;
        }
        writeln!(term)?;

        writeln!(
            term,
            "  {}",
            style("Press Enter TWICE to confirm and begin export").dim()
        )?;
        writeln!(term)?;

        // First Enter
        let _: String = Input::with_theme(theme)
            .with_prompt("[First confirmation - press Enter]")
            .allow_empty(true)
            .interact_text()?;

        flow.process_final_enter();
        writeln!(term, "  {} First confirmation received", style("•").cyan())?;

        // Second Enter
        let _: String = Input::with_theme(theme)
            .with_prompt("[Second confirmation - press Enter to proceed]")
            .allow_empty(true)
            .interact_text()?;

        flow.process_final_enter();
        writeln!(
            term,
            "  {} Second confirmation received",
            style("✓").green()
        )?;

        Ok(true)
    }

    fn step_export(&mut self, term: &mut Term) -> Result<()> {
        writeln!(term, "\n{}", style("Step 8 of 9: Export Progress").bold())?;
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

        // Phase 2: Encryption (skip if no_encryption mode)
        if self.no_encryption_mode {
            writeln!(term)?;
            writeln!(
                term,
                "  {} Skipping encryption (unencrypted mode)",
                style("⚠").yellow()
            )?;
            writeln!(
                term,
                "  {}",
                style("WARNING: All content will be publicly readable!").red()
            )?;

            // For unencrypted mode, just copy the export.db to payload directory
            let payload_dir = self.state.output_dir.join("payload");
            std::fs::create_dir_all(&payload_dir)?;
            let dest_db = payload_dir.join("data.db");
            std::fs::copy(&export_db_path, &dest_db)?;

            // Write minimal config.json for unencrypted bundle
            let config = serde_json::json!({
                "encrypted": false,
                "version": "1.0.0",
                "warning": "UNENCRYPTED - All content is publicly readable"
            });
            let config_path = self.state.output_dir.join("config.json");
            std::fs::write(&config_path, serde_json::to_string_pretty(&config)?)?;
        } else {
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
        }

        // Phase 3: Build static site bundle
        let pb3 = ProgressBar::new_spinner();
        pb3.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap(),
        );
        pb3.enable_steady_tick(Duration::from_millis(100));
        pb3.set_message("Building static site bundle...");

        // Generate documentation
        let generated_docs = if let Some(ref summary) = self.state.last_summary {
            // Determine target URL based on deployment target
            let target_url = match self.state.target {
                DeployTarget::GitHubPages => self
                    .state
                    .repo_name
                    .as_ref()
                    .map(|name| format!("https://{}.github.io/{}", "YOUR_USERNAME", name)),
                DeployTarget::CloudflarePages => self
                    .state
                    .repo_name
                    .as_ref()
                    .map(|name| format!("https://{}.pages.dev", name)),
                DeployTarget::Local => None,
            };

            let doc_config = if let Some(url) = target_url {
                DocConfig::new().with_url(url)
            } else {
                DocConfig::new()
            };

            let doc_generator = DocumentationGenerator::new(doc_config, summary.clone());
            doc_generator.generate_all()
        } else {
            Vec::new()
        };

        // Create bundle configuration
        let bundle_config = BundleConfig {
            title: self.state.title.clone(),
            description: self.state.description.clone(),
            hide_metadata: self.state.hide_metadata,
            recovery_secret: self.state.recovery_secret.clone(),
            generate_qr: self.state.generate_qr,
            generated_docs,
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
        writeln!(term, "\n{}", style("Step 9 of 9: Deployment").bold())?;
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
}
