# The Definitive Guide to High-Performance CLI and Automation Tools with Rust (mid-2025 Edition)

This guide synthesizes modern best practices for building blazingly fast, user-friendly, and production-ready CLI tools and automation systems with the **latest Rust nightly**, clap, anyhow, and tokio. It moves beyond basic argument parsing to provide battle-tested patterns for real-world command-line applications.

## Prerequisites & Toolchain Configuration

Ensure your environment uses the **current Rust nightly** (pinned via `rust-toolchain.toml`) plus the latest crate releases (wildcard constraints) for **clap**, **anyhow**, and **tokio**. The 2024 edition provides better async ergonomics and should be your default.

```toml
# Cargo.toml - Base configuration for CLI tools (nightly + latest crates)
[package]
name = "myctl"
version = "0.1.0"
edition = "2024"
authors = ["Your Name <you@example.com>"]
description = "A blazingly fast CLI tool"
license = "MIT OR Apache-2.0"
repository = "https://github.com/yourusername/myctl"
keywords = ["cli", "automation", "tool"]
categories = ["command-line-utilities"]

[[bin]]
name = "myctl"
path = "src/main.rs"

[dependencies]
# Core CLI framework
clap = { version = "*", features = ["derive", "cargo", "env", "unicode", "wrap_help"] }
clap_complete = "*"
clap_mangen = "*"

# Error handling
anyhow = "*"
thiserror = "*"

# Async runtime
tokio = { version = "*", features = ["rt-multi-thread", "macros", "fs", "process", "io-util", "time", "signal"] }

# Serialization
serde = { version = "*", features = ["derive"] }
serde_json = "*"
toml = "*"

# User interaction
dialoguer = "*"
indicatif = "*"
console = "*"
colored = "*"

# System interaction
directories = "*"
which = "*"
shell-words = "*"

# HTTP client for API interactions
reqwest = { version = "*", features = ["json", "rustls-tls"], default-features = false }

# Logging
tracing = "*"
tracing-subscriber = { version = "*", features = ["env-filter", "json"] }

[dev-dependencies]
assert_cmd = "*"
predicates = "*"
tempfile = "*"
insta = { version = "*", features = ["yaml", "json"] }

[profile.release]
lto = true
codegen-units = 1
strip = true
panic = "abort"
opt-level = "z"  # Optimize for binary size
```

### Essential Development Tools

```bash
# Install development tools
cargo install cargo-binstall    # Install binaries faster
cargo install cargo-dist        # Cross-platform binary distribution
cargo install cargo-insta       # Snapshot testing
cargo install hyperfine         # CLI benchmarking
cargo install cargo-bloat       # Analyze binary size

# Platform-specific tools
cargo binstall cargo-zigbuild   # Better cross-compilation
cargo binstall cross            # Docker-based cross-compilation
```

---

## 1. Project Structure & Architecture

CLI tools require a different structure than libraries or web services. Prioritize modularity and testability.

### ✅ DO: Use a Scalable Project Layout

```
myctl/
├── Cargo.toml
├── build.rs                  # Build script for completions
├── src/
│   ├── main.rs              # Entry point - minimal logic
│   ├── cli.rs               # CLI structure and parsing
│   ├── commands/            # Command implementations
│   │   ├── mod.rs
│   │   ├── init.rs
│   │   ├── deploy.rs
│   │   └── status.rs
│   ├── config/              # Configuration management
│   │   ├── mod.rs
│   │   └── schema.rs
│   ├── client/              # API/service clients
│   │   └── mod.rs
│   └── utils/               # Shared utilities
│       ├── mod.rs
│       ├── progress.rs
│       └── terminal.rs
├── tests/                   # Integration tests
│   └── integration/
└── completions/             # Generated shell completions
```

### ✅ DO: Keep `main.rs` Minimal

```rust
// src/main.rs
use anyhow::Result;
use myctl::cli::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing early
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Run the actual CLI
    myctl::run().await
}

// src/lib.rs
use anyhow::Result;
use clap::Parser;

pub mod cli;
pub mod commands;
pub mod config;
pub mod utils;

pub async fn run() -> Result<()> {
    let cli = cli::Cli::parse();
    commands::execute(cli).await
}
```

---

## 2. Clap v4 Patterns: Beyond Basic Parsing

Clap 4.5 introduces improved derive macros and better async support. Master both the derive and builder APIs for maximum flexibility.

### ✅ DO: Use Derive API with Advanced Features

```rust
// src/cli.rs
use clap::{Parser, Subcommand, Args, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "myctl",
    about = "A powerful automation tool",
    version,
    author,
    long_about = None,
    // Enable colored help automatically
    color = clap::ColorChoice::Auto,
    // Custom help template
    help_template = "{before-help}{name} {version}\n{author}\n{about}\n\n{usage-heading} {usage}\n\n{all-args}{after-help}",
)]
pub struct Cli {
    /// Global configuration file
    #[arg(short, long, global = true, env = "MYCTL_CONFIG")]
    pub config: Option<PathBuf>,

    /// Output format
    #[arg(
        short, 
        long, 
        global = true, 
        value_enum,
        default_value = "auto",
        env = "MYCTL_OUTPUT"
    )]
    pub output: OutputFormat,

    /// Increase logging verbosity
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress all output
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum OutputFormat {
    /// Human-readable output with colors
    Auto,
    /// Plain text without formatting
    Plain,
    /// JSON output for scripting
    Json,
    /// YAML output
    Yaml,
    /// Table format
    Table,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new project
    Init(InitArgs),
    
    /// Deploy resources
    Deploy {
        #[command(flatten)]
        common: DeployCommonArgs,
        
        #[command(subcommand)]
        target: DeployTarget,
    },
    
    /// Show status of resources
    Status {
        /// Filter by resource name pattern
        #[arg(short, long)]
        filter: Option<String>,
        
        /// Watch for changes
        #[arg(short, long)]
        watch: bool,
    },
    
    /// Manage configurations
    Config(ConfigArgs),
}

#[derive(Args)]
pub struct InitArgs {
    /// Project name
    #[arg(value_name = "NAME")]
    pub name: String,
    
    /// Project template
    #[arg(short, long, default_value = "default")]
    pub template: String,
    
    /// Skip interactive prompts
    #[arg(long)]
    pub non_interactive: bool,
}

#[derive(Args)]
pub struct DeployCommonArgs {
    /// Dry run - show what would be deployed
    #[arg(long)]
    pub dry_run: bool,
    
    /// Force deployment without confirmation
    #[arg(short, long)]
    pub force: bool,
    
    /// Parallel deployment count
    #[arg(short, long, default_value = "4", value_parser = clap::value_parser!(u8).range(1..=32))]
    pub parallel: u8,
}

#[derive(Subcommand)]
pub enum DeployTarget {
    /// Deploy to production
    Production {
        /// Production environment name
        env: String,
    },
    /// Deploy to staging
    Staging,
    /// Deploy to local development
    Local {
        /// Local port to use
        #[arg(short, long, default_value = "8080")]
        port: u16,
    },
}

// Advanced: Custom type with validation
#[derive(Clone, Debug)]
pub struct ResourcePattern(String);

impl std::str::FromStr for ResourcePattern {
    type Err = String;
    
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("Resource pattern cannot be empty".to_string());
        }
        
        // Validate pattern syntax
        if s.contains("**") && s.contains("?") {
            return Err("Cannot mix ** and ? in patterns".to_string());
        }
        
        Ok(ResourcePattern(s.to_string()))
    }
}
```

### ✅ DO: Implement Shell Completions

```rust
// build.rs
use clap::CommandFactory;
use clap_complete::{generate_to, shells::*};
use std::env;
use std::io::Error;

include!("src/cli.rs");

fn main() -> Result<(), Error> {
    let outdir = match env::var_os("OUT_DIR") {
        None => return Ok(()),
        Some(outdir) => outdir,
    };

    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();

    // Generate completions for all shells
    generate_to(Bash, &mut cmd, &name, &outdir)?;
    generate_to(Zsh, &mut cmd, &name, &outdir)?;
    generate_to(Fish, &mut cmd, &name, &outdir)?;
    generate_to(PowerShell, &mut cmd, &name, &outdir)?;
    generate_to(Elvish, &mut cmd, &name, &outdir)?;

    println!("cargo:rerun-if-changed=src/cli.rs");
    Ok(())
}
```

### ✅ DO: Implement Dynamic Completions

```rust
use clap::{ArgMatches, Command};
use clap_complete::dynamic::CompletionCandidate;

// Provide dynamic completions for resource names
fn complete_resource_name(current: &str) -> Vec<CompletionCandidate> {
    // In real app, this would query your data source
    let resources = vec!["web-server", "database", "cache", "queue"];
    
    resources
        .into_iter()
        .filter(|r| r.starts_with(current))
        .map(|r| CompletionCandidate::new(r))
        .collect()
}

// Register dynamic completion
pub fn augment_args(cmd: Command) -> Command {
    cmd.arg(
        clap::Arg::new("resource")
            .value_parser(clap::builder::NonEmptyStringValueParser::new())
            .add(clap_complete::dynamic::ValueHint::Unknown)
            .value_hint(clap::ValueHint::Other)
    )
}
```

---

## 3. Error Handling with Anyhow

CLI tools need excellent error messages. Anyhow provides the perfect balance of ergonomics and informativeness.

### ✅ DO: Use Context for Better Error Messages

```rust
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;

pub async fn load_config(path: &Path) -> Result<Config> {
    // Add context to filesystem operations
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file at {}", path.display()))?;
    
    // Add context to parsing operations
    let config: Config = toml::from_str(&contents)
        .with_context(|| format!("Invalid TOML in config file {}", path.display()))?;
    
    // Validate with custom errors
    validate_config(&config)
        .with_context(|| "Configuration validation failed")?;
    
    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    if config.timeout_seconds == 0 {
        bail!("Timeout must be greater than 0");
    }
    
    if config.endpoints.is_empty() {
        return Err(anyhow!("At least one endpoint must be configured"));
    }
    
    for (name, endpoint) in &config.endpoints {
        if endpoint.url.scheme() != "https" && !config.allow_insecure {
            bail!(
                "Endpoint '{}' uses insecure protocol '{}'. \
                 Use HTTPS or set 'allow_insecure = true'",
                name,
                endpoint.url.scheme()
            );
        }
    }
    
    Ok(())
}
```

### ✅ DO: Create Helpful Error Displays

```rust
use console::style;
use std::fmt::Write;

pub fn display_error(err: &anyhow::Error) -> String {
    let mut output = String::new();
    
    // Primary error
    writeln!(
        &mut output, 
        "{} {}", 
        style("Error:").red().bold(),
        err
    ).unwrap();
    
    // Chain of causes
    let mut source = err.source();
    while let Some(cause) = source {
        writeln!(
            &mut output,
            "  {} {}",
            style("Caused by:").yellow(),
            cause
        ).unwrap();
        source = cause.source();
    }
    
    // Add helpful suggestions based on error type
    if let Some(suggestion) = suggest_fix(err) {
        writeln!(
            &mut output,
            "\n{} {}",
            style("Suggestion:").green(),
            suggestion
        ).unwrap();
    }
    
    output
}

fn suggest_fix(err: &anyhow::Error) -> Option<&'static str> {
    let msg = err.to_string();
    
    if msg.contains("EACCES") || msg.contains("Permission denied") {
        Some("Try running with elevated permissions (sudo on Unix)")
    } else if msg.contains("ENOENT") || msg.contains("No such file") {
        Some("Check if the file path is correct and the file exists")
    } else if msg.contains("EADDRINUSE") || msg.contains("Address already in use") {
        Some("Another process is using this port. Try a different port or stop the other process")
    } else if msg.contains("certificate") || msg.contains("SSL") {
        Some("This might be a certificate issue. Try --insecure to bypass (not recommended for production)")
    } else {
        None
    }
}
```

### ✅ DO: Use Custom Error Types When Needed

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Configuration file not found at {path}")]
    NotFound { path: PathBuf },
    
    #[error("Invalid configuration: {message}")]
    Invalid { message: String },
    
    #[error("Missing required field: {field}")]
    MissingField { field: &'static str },
    
    #[error("Environment variable {var} not set")]
    MissingEnv { var: String },
}

// Convert to anyhow::Error when needed
impl From<ConfigError> for anyhow::Error {
    fn from(err: ConfigError) -> Self {
        anyhow::Error::new(err)
    }
}
```

---

## 4. Async CLI Patterns with Tokio

Modern CLI tools often need concurrent operations. Tokio provides the foundation for high-performance async CLIs.

### ✅ DO: Structure Async Commands Properly

```rust
// src/commands/mod.rs
use anyhow::Result;
use tokio::task::JoinSet;
use std::time::Duration;

pub async fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Deploy { common, target } => {
            deploy::execute(common, target, &cli).await
        }
        Commands::Status { filter, watch } => {
            if watch {
                status::watch(filter, &cli).await
            } else {
                status::show(filter, &cli).await
            }
        }
        // ... other commands
    }
}

// src/commands/deploy.rs
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::Semaphore;
use std::sync::Arc;

pub async fn execute(
    args: DeployCommonArgs,
    target: DeployTarget,
    cli: &Cli,
) -> Result<()> {
    let resources = discover_resources(&target).await?;
    
    if args.dry_run {
        return show_deployment_plan(&resources, cli);
    }
    
    if !args.force && !confirm_deployment(&resources).await? {
        bail!("Deployment cancelled by user");
    }
    
    // Deploy with parallelism control
    let semaphore = Arc::new(Semaphore::new(args.parallel as usize));
    let multi_progress = MultiProgress::new();
    let mut tasks = JoinSet::new();
    
    for resource in resources {
        let sem = semaphore.clone();
        let pb = create_progress_bar(&multi_progress, &resource);
        
        tasks.spawn(async move {
            let _permit = sem.acquire().await?;
            deploy_resource(resource, pb).await
        });
    }
    
    // Collect results
    let mut failed = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(())) => {},
            Ok(Err(e)) => failed.push(e),
            Err(e) => failed.push(anyhow!("Task panicked: {}", e)),
        }
    }
    
    if failed.is_empty() {
        success!("All resources deployed successfully");
        Ok(())
    } else {
        error!("{} resources failed to deploy", failed.len());
        for (i, err) in failed.iter().enumerate() {
            eprintln!("  {}. {}", i + 1, err);
        }
        bail!("Deployment failed")
    }
}

async fn deploy_resource(
    resource: Resource,
    progress: ProgressBar,
) -> Result<()> {
    progress.set_message("Validating...");
    validate_resource(&resource).await?;
    
    progress.set_message("Uploading...");
    progress.set_position(25);
    upload_resource(&resource).await?;
    
    progress.set_message("Configuring...");
    progress.set_position(50);
    configure_resource(&resource).await?;
    
    progress.set_message("Starting...");
    progress.set_position(75);
    start_resource(&resource).await?;
    
    progress.set_message("Verifying...");
    progress.set_position(90);
    verify_resource(&resource).await?;
    
    progress.finish_with_message("✓ Deployed");
    Ok(())
}
```

### ✅ DO: Handle Signals Gracefully

```rust
use tokio::signal;
use tokio::sync::broadcast;

pub struct SignalHandler {
    shutdown_tx: broadcast::Sender<()>,
}

impl SignalHandler {
    pub fn new() -> (Self, broadcast::Receiver<()>) {
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        
        let handler = Self { shutdown_tx };
        
        // Spawn signal handling task
        tokio::spawn(async move {
            handler.handle_signals().await;
        });
        
        (handler, shutdown_rx)
    }
    
    async fn handle_signals(self) {
        let ctrl_c = async {
            signal::ctrl_c()
                .await
                .expect("Failed to install Ctrl+C handler");
        };

        #[cfg(unix)]
        let terminate = async {
            signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("Failed to install signal handler")
                .recv()
                .await;
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                info!("Received Ctrl+C, initiating graceful shutdown...");
            },
            _ = terminate => {
                info!("Received terminate signal, initiating graceful shutdown...");
            },
        }
        
        let _ = self.shutdown_tx.send(());
    }
}

// Usage in long-running command
pub async fn watch_resources(filter: Option<String>) -> Result<()> {
    let (_handler, mut shutdown_rx) = SignalHandler::new();
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("Stopping watch...");
                break;
            }
            _ = interval.tick() => {
                clear_screen();
                display_resources(&filter).await?;
            }
        }
    }
    
    Ok(())
}
```

### ✅ DO: Implement Timeouts and Retries

```rust
use anyhow::Result;
use tokio::time::{timeout, sleep};
use std::time::Duration;

pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub exponential_base: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
            exponential_base: 2.0,
        }
    }
}

pub async fn with_retry<F, Fut, T>(
    operation: F,
    config: RetryConfig,
) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut delay = config.initial_delay;
    
    for attempt in 1..=config.max_attempts {
        match timeout(Duration::from_secs(30), operation()).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(e)) if attempt == config.max_attempts => {
                return Err(e).context(format!(
                    "Operation failed after {} attempts",
                    config.max_attempts
                ));
            }
            Ok(Err(e)) => {
                warn!("Attempt {} failed: {}. Retrying in {:?}...", 
                    attempt, e, delay);
                sleep(delay).await;
                
                // Exponential backoff with jitter
                delay = std::cmp::min(
                    config.max_delay,
                    Duration::from_secs_f64(
                        delay.as_secs_f64() * config.exponential_base 
                        * (0.5 + rand::random::<f64>() * 0.5)
                    ),
                );
            }
            Err(_) => {
                if attempt == config.max_attempts {
                    bail!("Operation timed out after {} attempts", config.max_attempts);
                }
                warn!("Attempt {} timed out. Retrying...", attempt);
            }
        }
    }
    
    unreachable!()
}

// Usage
pub async fn fetch_with_retry(url: &str) -> Result<String> {
    with_retry(
        || async {
            let response = reqwest::get(url).await?;
            response.error_for_status()?.text().await
                .context("Failed to read response body")
        },
        RetryConfig::default(),
    ).await
}
```

---

## 5. Configuration Management

CLI tools need flexible configuration systems that support files, environment variables, and command-line overrides.

### ✅ DO: Implement Layered Configuration

```rust
// src/config/mod.rs
use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub api: ApiConfig,
    
    #[serde(default)]
    pub ui: UiConfig,
    
    #[serde(default)]
    pub defaults: DefaultsConfig,
    
    // Allow custom extensions
    #[serde(flatten)]
    pub extra: toml::Table,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ApiConfig {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    
    pub api_key: Option<String>,
    
    #[serde(default)]
    pub verify_tls: bool,
}

fn default_endpoint() -> String {
    "https://api.example.com".to_string()
}

fn default_timeout() -> u64 {
    30
}

impl Config {
    /// Load configuration from multiple sources with proper precedence
    pub async fn load(cli_path: Option<&Path>) -> Result<Self> {
        let mut config = Config::default();
        
        // 1. Load from default locations
        for path in Self::default_paths() {
            if path.exists() {
                config.merge_file(&path)?;
            }
        }
        
        // 2. Load from CLI-specified path
        if let Some(path) = cli_path {
            config.merge_file(path)
                .with_context(|| format!("Failed to load config from {}", path.display()))?;
        }
        
        // 3. Apply environment variables
        config.merge_env()?;
        
        // 4. Validate final configuration
        config.validate()?;
        
        Ok(config)
    }
    
    fn default_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        
        // System-wide config
        paths.push(PathBuf::from("/etc/myctl/config.toml"));
        
        // User config
        if let Some(proj_dirs) = ProjectDirs::from("com", "example", "myctl") {
            paths.push(proj_dirs.config_dir().join("config.toml"));
        }
        
        // Project-local config
        paths.push(PathBuf::from(".myctl.toml"));
        
        paths
    }
    
    fn merge_file(&mut self, path: &Path) -> Result<()> {
        let contents = std::fs::read_to_string(path)?;
        let file_config: Config = toml::from_str(&contents)
            .with_context(|| format!("Invalid TOML in {}", path.display()))?;
        
        // Merge with existing config
        self.merge(file_config);
        Ok(())
    }
    
    fn merge_env(&mut self) -> Result<()> {
        // Override with environment variables
        if let Ok(endpoint) = std::env::var("MYCTL_API_ENDPOINT") {
            self.api.endpoint = endpoint;
        }
        
        if let Ok(key) = std::env::var("MYCTL_API_KEY") {
            self.api.api_key = Some(key);
        }
        
        if let Ok(timeout) = std::env::var("MYCTL_API_TIMEOUT") {
            self.api.timeout_seconds = timeout.parse()
                .context("MYCTL_API_TIMEOUT must be a number")?;
        }
        
        Ok(())
    }
    
    fn validate(&self) -> Result<()> {
        if self.api.timeout_seconds == 0 {
            bail!("API timeout must be greater than 0");
        }
        
        if let Some(key) = &self.api.api_key {
            if key.is_empty() {
                bail!("API key cannot be empty");
            }
        }
        
        Ok(())
    }
}

// Create a config subcommand
pub fn config_command() -> Command {
    Command::new("config")
        .about("Manage configuration")
        .subcommand(
            Command::new("show")
                .about("Show current configuration")
        )
        .subcommand(
            Command::new("edit")
                .about("Edit configuration in your editor")
        )
        .subcommand(
            Command::new("validate")
                .about("Validate configuration files")
        )
        .subcommand(
            Command::new("path")
                .about("Show configuration file paths")
        )
}
```

### ✅ DO: Support Multiple Configuration Formats

```rust
use serde::de::DeserializeOwned;

pub enum ConfigFormat {
    Toml,
    Json,
    Yaml,
}

impl ConfigFormat {
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "toml" => Some(Self::Toml),
            "json" => Some(Self::Json),
            "yaml" | "yml" => Some(Self::Yaml),
            _ => None,
        }
    }
    
    pub fn parse<T: DeserializeOwned>(&self, contents: &str) -> Result<T> {
        match self {
            Self::Toml => toml::from_str(contents)
                .context("Invalid TOML"),
            Self::Json => serde_json::from_str(contents)
                .context("Invalid JSON"),
            Self::Yaml => serde_yaml::from_str(contents)
                .context("Invalid YAML"),
        }
    }
}
```

---

## 6. Interactive CLI Features

Modern CLI tools should provide rich interactive experiences when appropriate.

### ✅ DO: Use Dialoguer for User Interaction

```rust
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select, MultiSelect, Password};
use console::style;

pub async fn interactive_init() -> Result<ProjectConfig> {
    println!("{}", style("Welcome to MyCtl Setup!").bold().cyan());
    println!("This wizard will help you create a new project.\n");
    
    // Text input with validation
    let name: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("Project name")
        .validate_with(|input: &String| -> Result<(), &str> {
            if input.is_empty() {
                Err("Project name cannot be empty")
            } else if !is_valid_project_name(input) {
                Err("Project name can only contain letters, numbers, and hyphens")
            } else {
                Ok(())
            }
        })
        .interact_text()?;
    
    // Selection from list
    let template = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Select a project template")
        .items(&["Web API", "CLI Tool", "Library", "Custom"])
        .default(0)
        .interact()?;
    
    // Multi-select for features
    let features = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select features to enable")
        .items(&[
            "Authentication",
            "Database",
            "Caching",
            "Monitoring",
            "CI/CD Pipeline",
        ])
        .defaults(&[false, true, false, true, true])
        .interact()?;
    
    // Password input
    let api_key = if Confirm::new()
        .with_prompt("Do you want to configure API access now?")
        .default(true)
        .interact()?
    {
        Some(Password::new()
            .with_prompt("API Key")
            .with_confirmation("Confirm API Key", "Keys do not match")
            .interact()?)
    } else {
        None
    };
    
    // Confirmation
    println!("\n{}", style("Summary:").bold());
    println!("  Project: {}", style(&name).green());
    println!("  Template: {}", style(&template).green());
    println!("  Features: {} selected", style(features.len()).green());
    
    if !Confirm::new()
        .with_prompt("Create project with these settings?")
        .default(true)
        .interact()?
    {
        bail!("Project creation cancelled");
    }
    
    Ok(ProjectConfig {
        name,
        template,
        features,
        api_key,
    })
}
```

### ✅ DO: Implement Progress Indicators

```rust
use indicatif::{ProgressBar, ProgressStyle, MultiProgress, ProgressIterator};
use std::time::Duration;

pub struct ProgressReporter {
    multi: MultiProgress,
    main_bar: ProgressBar,
}

impl ProgressReporter {
    pub fn new(total_steps: u64) -> Self {
        let multi = MultiProgress::new();
        
        let main_bar = multi.add(ProgressBar::new(total_steps));
        main_bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} {msg:<40} [{bar:40.cyan/blue}] {pos}/{len}")
                .unwrap()
                .progress_chars("#>-")
        );
        
        Self { multi, main_bar }
    }
    
    pub fn add_subtask(&self, name: &str, total: u64) -> ProgressBar {
        let bar = self.multi.add(ProgressBar::new(total));
        bar.set_style(
            ProgressStyle::default_bar()
                .template("  {msg:<38} [{bar:40.cyan/blue}] {pos}/{len}")
                .unwrap()
                .progress_chars("=>-")
        );
        bar.set_message(name.to_string());
        bar
    }
    
    pub fn finish_main(&self, message: &str) {
        self.main_bar.finish_with_message(format!("✓ {}", message));
    }
}

// Usage example
pub async fn process_files(files: Vec<PathBuf>) -> Result<()> {
    let progress = ProgressReporter::new(files.len() as u64);
    
    for (i, file) in files.iter().enumerate() {
        progress.main_bar.set_message(format!("Processing {}", file.display()));
        
        // Create subtask progress
        let file_size = file.metadata()?.len();
        let subtask = progress.add_subtask("Reading file", file_size);
        
        // Process with progress updates
        process_file_with_progress(file, &subtask).await?;
        
        subtask.finish_with_message("✓ Complete");
        progress.main_bar.inc(1);
    }
    
    progress.finish_main("All files processed");
    Ok(())
}

// Spinner for indeterminate progress
pub async fn long_operation<F, Fut, T>(message: &str, operation: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    spinner.set_message(message.to_string());
    spinner.enable_steady_tick(Duration::from_millis(80));
    
    let result = operation().await;
    
    match &result {
        Ok(_) => spinner.finish_with_message(format!("✓ {}", message)),
        Err(_) => spinner.finish_with_message(format!("✗ {}", message)),
    }
    
    result
}
```

### ✅ DO: Support Both Interactive and Non-Interactive Modes

```rust
pub struct InteractionMode {
    interactive: bool,
    assume_yes: bool,
    output_format: OutputFormat,
}

impl InteractionMode {
    pub fn from_cli(cli: &Cli) -> Self {
        Self {
            interactive: atty::is(atty::Stream::Stdin) && !cli.quiet,
            assume_yes: cli.assume_yes,
            output_format: cli.output,
        }
    }
    
    pub async fn confirm(&self, message: &str) -> Result<bool> {
        if self.assume_yes {
            return Ok(true);
        }
        
        if !self.interactive {
            bail!("Cannot prompt for confirmation in non-interactive mode. Use --yes to proceed.");
        }
        
        Ok(Confirm::new()
            .with_prompt(message)
            .default(false)
            .interact()?)
    }
    
    pub async fn select_one<T: ToString>(
        &self,
        prompt: &str,
        options: &[T],
        default: Option<usize>,
    ) -> Result<usize> {
        if !self.interactive {
            if let Some(idx) = default {
                return Ok(idx);
            }
            bail!("Cannot prompt for selection in non-interactive mode");
        }
        
        let mut select = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt);
        
        for option in options {
            select = select.item(option.to_string());
        }
        
        if let Some(idx) = default {
            select = select.default(idx);
        }
        
        Ok(select.interact()?)
    }
}
```

---

## 7. Output Formatting & Display

CLI tools need to present information clearly across different output formats.

### ✅ DO: Implement Structured Output

```rust
use serde::Serialize;
use colored::Colorize;
use comfy_table::{Table, presets::UTF8_FULL};

pub trait Displayable: Serialize {
    fn display_human(&self) -> String;
    fn display_json(&self) -> Result<String>;
    fn display_yaml(&self) -> Result<String>;
    fn display_table(&self) -> String;
}

#[derive(Serialize)]
pub struct Resource {
    pub id: String,
    pub name: String,
    pub status: Status,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Serialize, Clone, Copy)]
pub enum Status {
    Running,
    Stopped,
    Failed,
    Unknown,
}

impl Status {
    fn colored(&self) -> String {
        match self {
            Status::Running => "Running".green().to_string(),
            Status::Stopped => "Stopped".yellow().to_string(),
            Status::Failed => "Failed".red().to_string(),
            Status::Unknown => "Unknown".dimmed().to_string(),
        }
    }
}

impl Displayable for Vec<Resource> {
    fn display_human(&self) -> String {
        if self.is_empty() {
            return "No resources found".dimmed().to_string();
        }
        
        let mut output = String::new();
        for resource in self {
            output.push_str(&format!(
                "{} {} ({})\n",
                resource.id.bright_blue(),
                resource.name,
                resource.status.colored()
            ));
        }
        output
    }
    
    fn display_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
    
    fn display_yaml(&self) -> Result<String> {
        Ok(serde_yaml::to_string(self)?)
    }
    
    fn display_table(&self) -> String {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL);
        table.set_header(vec!["ID", "Name", "Status", "Created"]);
        
        for resource in self {
            table.add_row(vec![
                &resource.id,
                &resource.name,
                &resource.status.colored(),
                &resource.created_at.format("%Y-%m-%d %H:%M").to_string(),
            ]);
        }
        
        table.to_string()
    }
}

// Generic output function
pub fn output<T: Displayable>(data: T, format: OutputFormat) -> Result<()> {
    let output = match format {
        OutputFormat::Auto | OutputFormat::Plain => data.display_human(),
        OutputFormat::Json => data.display_json()?,
        OutputFormat::Yaml => data.display_yaml()?,
        OutputFormat::Table => data.display_table(),
    };
    
    println!("{}", output);
    Ok(())
}
```

### ✅ DO: Use Colors and Formatting Wisely

```rust
use colored::*;
use console::{style, Emoji};

// Define consistent color scheme
pub struct Theme;

impl Theme {
    pub fn success<S: ToString>(msg: S) -> String {
        format!("{} {}", style("✓").green(), msg.to_string())
    }
    
    pub fn error<S: ToString>(msg: S) -> String {
        format!("{} {}", style("✗").red(), msg.to_string())
    }
    
    pub fn warning<S: ToString>(msg: S) -> String {
        format!("{} {}", style("⚠").yellow(), msg.to_string())
    }
    
    pub fn info<S: ToString>(msg: S) -> String {
        format!("{} {}", style("ℹ").blue(), msg.to_string())
    }
    
    pub fn highlight<S: ToString>(text: S) -> String {
        style(text.to_string()).bold().to_string()
    }
}

// Respect NO_COLOR environment variable
pub fn should_use_color() -> bool {
    std::env::var("NO_COLOR").is_err() 
        && atty::is(atty::Stream::Stdout)
        && !cfg!(windows) // Or check Windows terminal capabilities
}

// Helper macros
#[macro_export]
macro_rules! success {
    ($($arg:tt)*) => {
        println!("{}", $crate::utils::Theme::success(format!($($arg)*)));
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        eprintln!("{}", $crate::utils::Theme::error(format!($($arg)*)));
    };
}

#[macro_export]
macro_rules! warning {
    ($($arg:tt)*) => {
        eprintln!("{}", $crate::utils::Theme::warning(format!($($arg)*)));
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        println!("{}", $crate::utils::Theme::info(format!($($arg)*)));
    };
}
```

---

## 8. Testing CLI Applications

Testing CLI tools requires special patterns to capture output and simulate user input.

### ✅ DO: Use Integration Tests with assert_cmd

```rust
// tests/integration/basic.rs
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

#[test]
fn test_init_command() {
    let temp = TempDir::new().unwrap();
    
    Command::cargo_bin("myctl")
        .unwrap()
        .arg("init")
        .arg("test-project")
        .arg("--non-interactive")
        .current_dir(&temp)
        .assert()
        .success()
        .stdout(predicate::str::contains("Project created successfully"));
    
    // Verify files were created
    assert!(temp.path().join("test-project").exists());
    assert!(temp.path().join("test-project/config.toml").exists());
}

#[test]
fn test_invalid_config() {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("invalid.toml");
    std::fs::write(&config_path, "invalid = [toml").unwrap();
    
    Command::cargo_bin("myctl")
        .unwrap()
        .arg("--config")
        .arg(&config_path)
        .arg("status")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Invalid TOML"));
}

#[test]
fn test_json_output() {
    Command::cargo_bin("myctl")
        .unwrap()
        .args(&["status", "--output", "json"])
        .assert()
        .success()
        .stdout(predicate::str::is_json());
}

// Test with timeout
#[tokio::test]
async fn test_long_running_command() {
    use tokio::time::{timeout, Duration};
    
    let mut cmd = Command::cargo_bin("myctl")
        .unwrap()
        .args(&["deploy", "local", "--port", "9999"])
        .spawn()
        .unwrap();
    
    // Should respond within 5 seconds
    let result = timeout(Duration::from_secs(5), cmd.wait()).await;
    
    assert!(result.is_ok(), "Command timed out");
    assert!(result.unwrap().unwrap().success());
}
```

### ✅ DO: Use Snapshot Testing with Insta

```rust
// tests/snapshots.rs
use insta::assert_snapshot;
use assert_cmd::Command;

#[test]
fn test_help_output() {
    let output = Command::cargo_bin("myctl")
        .unwrap()
        .arg("--help")
        .output()
        .unwrap();
    
    assert_snapshot!(String::from_utf8_lossy(&output.stdout));
}

#[test]
fn test_error_messages() {
    let output = Command::cargo_bin("myctl")
        .unwrap()
        .arg("deploy")
        .arg("nonexistent")
        .output()
        .unwrap();
    
    assert!(!output.status.success());
    assert_snapshot!(
        "deploy_error", 
        String::from_utf8_lossy(&output.stderr)
    );
}

// Test with settings
#[test]
fn test_formatted_output() {
    let output = get_status_output();
    
    insta::with_settings!({
        filters => vec![
            // Replace timestamps with placeholder
            (r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}", "[TIMESTAMP]"),
            // Replace UUIDs
            (r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "[UUID]"),
        ]
    }, {
        assert_snapshot!(output);
    });
}
```

### ✅ DO: Mock External Dependencies

```rust
// tests/mocks.rs
use mockito::{mock, Mock};
use std::env;

pub struct ApiMock {
    server_url: String,
    mocks: Vec<Mock>,
}

impl ApiMock {
    pub fn new() -> Self {
        Self {
            server_url: mockito::server_url(),
            mocks: Vec::new(),
        }
    }
    
    pub fn mock_success(mut self) -> Self {
        let m = mock("GET", "/api/status")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"status": "healthy", "version": "1.0.0"}"#)
            .create();
        
        self.mocks.push(m);
        self
    }
    
    pub fn mock_auth_failure(mut self) -> Self {
        let m = mock("GET", mockito::Matcher::Any)
            .with_status(401)
            .with_body(r#"{"error": "Unauthorized"}"#)
            .create();
        
        self.mocks.push(m);
        self
    }
    
    pub fn run_test<F>(self, test: F) 
    where 
        F: FnOnce()
    {
        // Override API endpoint
        env::set_var("MYCTL_API_ENDPOINT", &self.server_url);
        
        test();
        
        // Verify all mocks were called
        for mock in self.mocks {
            mock.assert();
        }
    }
}

#[test]
fn test_with_mock_api() {
    ApiMock::new()
        .mock_success()
        .run_test(|| {
            Command::cargo_bin("myctl")
                .unwrap()
                .arg("status")
                .assert()
                .success()
                .stdout(predicate::str::contains("healthy"));
        });
}
```

### ✅ DO: Benchmark CLI Performance

```rust
// benches/performance.rs
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::process::Command;
use tempfile::TempDir;

fn benchmark_startup(c: &mut Criterion) {
    c.bench_function("cli startup", |b| {
        b.iter(|| {
            Command::new("target/release/myctl")
                .arg("--version")
                .output()
                .unwrap();
        });
    });
}

fn benchmark_config_parsing(c: &mut Criterion) {
    let temp = TempDir::new().unwrap();
    let config_path = temp.path().join("config.toml");
    std::fs::write(&config_path, include_str!("../fixtures/large_config.toml")).unwrap();
    
    c.bench_function("parse large config", |b| {
        b.iter(|| {
            Command::new("target/release/myctl")
                .arg("--config")
                .arg(&config_path)
                .arg("config")
                .arg("validate")
                .output()
                .unwrap();
        });
    });
}

// Benchmark with hyperfine in CI
#[test]
fn hyperfine_benchmarks() {
    if std::env::var("CI").is_ok() {
        let output = Command::new("hyperfine")
            .args(&[
                "--warmup", "3",
                "--min-runs", "10",
                "--export-json", "bench-results.json",
                "'target/release/myctl --version'",
                "'target/release/myctl status --output json'",
            ])
            .output()
            .expect("Failed to run hyperfine");
        
        assert!(output.status.success());
    }
}

criterion_group!(benches, benchmark_startup, benchmark_config_parsing);
criterion_main!(benches);
```

---

## 9. Distribution & Installation

Getting your CLI tool into users' hands requires careful consideration of packaging and distribution.

### ✅ DO: Use cargo-dist for Cross-Platform Distribution

```toml
# Cargo.toml
[package.metadata.dist]
# Automatically create GitHub releases with binaries
targets = ["x86_64-pc-windows-msvc", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]
ci = ["github"]
installers = ["shell", "powershell", "homebrew", "msi"]
tap = "myorg/homebrew-tap"
```

```yaml
# .github/workflows/release.yml
name: Release

on:
  push:
    tags:
      - 'v*'

jobs:
  dist:
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
          - os: ubuntu-latest
            target: aarch64-unknown-linux-gnu
          - os: windows-latest
            target: x86_64-pc-windows-msvc
          - os: macos-latest
            target: x86_64-apple-darwin
          - os: macos-latest
            target: aarch64-apple-darwin
    
    steps:
      - uses: actions/checkout@v4
      
      - name: Install Rust (nightly)
        uses: dtolnay/rust-toolchain@nightly
        with:
          targets: ${{ matrix.target }}
      
      - name: Build
        run: cargo build --release --target ${{ matrix.target }}
      
      - name: Create archive
        shell: bash
        run: |
          if [ "${{ matrix.os }}" = "windows-latest" ]; then
            7z a myctl-${{ matrix.target }}.zip ./target/${{ matrix.target }}/release/myctl.exe
          else
            tar czf myctl-${{ matrix.target }}.tar.gz -C target/${{ matrix.target }}/release myctl
          fi
      
      - name: Upload artifacts
        uses: actions/upload-artifact@v3
        with:
          name: binaries
          path: myctl-*
```

### ✅ DO: Create Install Scripts

```bash
#!/bin/sh
# install.sh - Universal installer script

set -e

REPO="myorg/myctl"
BINARY="myctl"

# Detect OS and architecture
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
    linux*)
        case "$ARCH" in
            x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
            aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
            *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    darwin*)
        case "$ARCH" in
            x86_64) TARGET="x86_64-apple-darwin" ;;
            arm64) TARGET="aarch64-apple-darwin" ;;
            *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    *) echo "Unsupported OS: $OS"; exit 1 ;;
esac

# Get latest release
LATEST=$(curl -s https://api.github.com/repos/$REPO/releases/latest | grep tag_name | cut -d '"' -f 4)
URL="https://github.com/$REPO/releases/download/$LATEST/$BINARY-$TARGET.tar.gz"

# Download and install
echo "Downloading $BINARY $LATEST for $TARGET..."
curl -sL "$URL" | tar xz

# Install to user's bin directory
INSTALL_DIR="${HOME}/.local/bin"
mkdir -p "$INSTALL_DIR"
mv "$BINARY" "$INSTALL_DIR/"

echo "Installed $BINARY to $INSTALL_DIR"
echo "Make sure $INSTALL_DIR is in your PATH"
```

### ✅ DO: Support Package Managers

```ruby
# Homebrew formula (homebrew-tap/Formula/myctl.rb)
class Myctl < Formula
  desc "Powerful automation tool"
  homepage "https://github.com/myorg/myctl"
  version "0.1.0"
  
  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/myorg/myctl/releases/download/v#{version}/myctl-aarch64-apple-darwin.tar.gz"
      sha256 "..."
    else
      url "https://github.com/myorg/myctl/releases/download/v#{version}/myctl-x86_64-apple-darwin.tar.gz"
      sha256 "..."
    end
  end
  
  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/myorg/myctl/releases/download/v#{version}/myctl-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "..."
    else
      url "https://github.com/myorg/myctl/releases/download/v#{version}/myctl-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "..."
    end
  end
  
  def install
    bin.install "myctl"
    
    # Generate completions
    generate_completions_from_executable(bin/"myctl", "completions")
  end
  
  test do
    assert_match "myctl #{version}", shell_output("#{bin}/myctl --version")
  end
end
```

### ✅ DO: Minimize Binary Size

```toml
# Cargo.toml - Size optimizations
[profile.release-min]
inherits = "release"
opt-level = "z"        # Optimize for size
lto = true            # Link-time optimization
codegen-units = 1     # Single codegen unit
strip = true          # Strip symbols
panic = "abort"       # No unwinding

# Use alternative allocator
[dependencies]
mimalloc = { version = "0.1", default-features = false }

# Reduce regex size
regex = { version = "1.10", default-features = false, features = ["std", "perf"] }
```

```rust
// src/main.rs - Use mimalloc
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

---

## 10. Real-World Patterns

### ✅ DO: Implement Plugins/Extensions

```rust
// Plugin system using dynamic loading
use libloading::{Library, Symbol};
use std::path::Path;

pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn execute(&self, args: &[String]) -> Result<()>;
}

pub struct PluginManager {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginManager {
    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let mut plugins = Vec::new();
        
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            
            if path.extension() == Some(std::ffi::OsStr::new("so")) 
                || path.extension() == Some(std::ffi::OsStr::new("dll"))
                || path.extension() == Some(std::ffi::OsStr::new("dylib")) 
            {
                match Self::load_plugin(&path) {
                    Ok(plugin) => {
                        info!("Loaded plugin: {}", plugin.name());
                        plugins.push(plugin);
                    }
                    Err(e) => {
                        warning!("Failed to load plugin {}: {}", path.display(), e);
                    }
                }
            }
        }
        
        Ok(Self { plugins })
    }
    
    unsafe fn load_plugin(path: &Path) -> Result<Box<dyn Plugin>> {
        type PluginCreate = unsafe fn() -> *mut dyn Plugin;
        
        let lib = Library::new(path)?;
        let constructor: Symbol<PluginCreate> = lib.get(b"_plugin_create")?;
        let plugin = Box::from_raw(constructor());
        
        std::mem::forget(lib); // Keep library loaded
        Ok(plugin)
    }
}

// In external plugin crate
#[no_mangle]
pub extern "C" fn _plugin_create() -> *mut dyn Plugin {
    Box::into_raw(Box::new(MyPlugin::new()))
}
```

### ✅ DO: Support Shell Integration

```rust
// Generate shell functions for enhanced integration
pub fn generate_shell_integration(shell: Shell) -> String {
    match shell {
        Shell::Bash => r#"
# myctl bash integration
_myctl_cd() {
    local dir=$(myctl workspace path "$1" 2>/dev/null)
    if [ -n "$dir" ]; then
        cd "$dir"
    else
        echo "Workspace not found: $1" >&2
        return 1
    fi
}

alias mcd='_myctl_cd'

# Auto-activate environment
_myctl_auto_env() {
    if [ -f ".myctl.toml" ]; then
        eval $(myctl env shell)
    fi
}

PROMPT_COMMAND="_myctl_auto_env;$PROMPT_COMMAND"
"#.to_string(),
        
        Shell::Zsh => r#"
# myctl zsh integration
myctl_cd() {
    local dir=$(myctl workspace path "$1" 2>/dev/null)
    if [ -n "$dir" ]; then
        cd "$dir"
    else
        echo "Workspace not found: $1" >&2
        return 1
    fi
}

alias mcd='myctl_cd'

# Hook for auto-env
add-zsh-hook chpwd myctl_auto_env
myctl_auto_env() {
    if [ -f ".myctl.toml" ]; then
        eval $(myctl env shell)
    fi
}
"#.to_string(),
        
        _ => String::new(),
    }
}
```

### ✅ DO: Implement Update Checking

```rust
use semver::Version;

pub struct UpdateChecker {
    current_version: Version,
    check_url: String,
}

impl UpdateChecker {
    pub async fn check_for_updates(&self) -> Result<Option<Release>> {
        // Check only once per day
        if !self.should_check()? {
            return Ok(None);
        }
        
        let response = reqwest::Client::new()
            .get(&self.check_url)
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        
        let latest: Release = response.json().await?;
        let latest_version = Version::parse(&latest.version)?;
        
        if latest_version > self.current_version {
            self.record_check()?;
            Ok(Some(latest))
        } else {
            Ok(None)
        }
    }
    
    fn should_check(&self) -> Result<bool> {
        let config_dir = directories::ProjectDirs::from("com", "example", "myctl")
            .context("Failed to get config directory")?;
        
        let check_file = config_dir.data_dir().join("last-update-check");
        
        if !check_file.exists() {
            return Ok(true);
        }
        
        let metadata = std::fs::metadata(&check_file)?;
        let modified = metadata.modified()?;
        let elapsed = modified.elapsed().unwrap_or(Duration::MAX);
        
        Ok(elapsed > Duration::from_secs(86400)) // 24 hours
    }
    
    fn record_check(&self) -> Result<()> {
        let config_dir = directories::ProjectDirs::from("com", "example", "myctl")
            .context("Failed to get config directory")?;
        
        std::fs::create_dir_all(config_dir.data_dir())?;
        let check_file = config_dir.data_dir().join("last-update-check");
        std::fs::write(check_file, "")?;
        
        Ok(())
    }
}

// Check on startup (non-blocking)
pub fn spawn_update_check() {
    tokio::spawn(async {
        let checker = UpdateChecker::new();
        
        match checker.check_for_updates().await {
            Ok(Some(release)) => {
                eprintln!(
                    "\n{} {} → {} available",
                    style("Update:").green().bold(),
                    env!("CARGO_PKG_VERSION"),
                    style(&release.version).green()
                );
                eprintln!(
                    "Install with: {}\n",
                    style("myctl self-update").cyan()
                );
            }
            Ok(None) => {
                // No update available
            }
            Err(e) => {
                debug!("Update check failed: {}", e);
            }
        }
    });
}
```

### ✅ DO: Handle Long-Running Operations

```rust
use tokio::process::Command as TokioCommand;
use tokio::io::{AsyncBufReadExt, BufReader};

pub async fn run_subprocess_with_output(
    cmd: &str,
    args: &[&str],
    on_line: impl Fn(&str),
) -> Result<()> {
    let mut child = TokioCommand::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn subprocess")?;
    
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    
    let stdout_reader = BufReader::new(stdout);
    let stderr_reader = BufReader::new(stderr);
    
    let mut stdout_lines = stdout_reader.lines();
    let mut stderr_lines = stderr_reader.lines();
    
    loop {
        tokio::select! {
            line = stdout_lines.next_line() => {
                match line? {
                    Some(line) => on_line(&line),
                    None => break,
                }
            }
            line = stderr_lines.next_line() => {
                match line? {
                    Some(line) => on_line(&line),
                    None => break,
                }
            }
        }
    }
    
    let status = child.wait().await?;
    
    if !status.success() {
        bail!("Command failed with status: {}", status);
    }
    
    Ok(())
}

// Usage
pub async fn build_project(path: &Path) -> Result<()> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_message("Building project...");
    
    run_subprocess_with_output(
        "cargo",
        &["build", "--release"],
        |line| {
            // Update spinner with build progress
            if line.contains("Compiling") {
                spinner.set_message(line);
            }
        }
    ).await?;
    
    spinner.finish_with_message("✓ Build complete");
    Ok(())
}
```

---

## 11. Advanced Automation Patterns

### ✅ DO: Implement Task Automation DSL

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Workflow {
    pub name: String,
    pub description: Option<String>,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Task {
    pub name: String,
    pub run: RunConfig,
    #[serde(default)]
    pub when: Condition,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RunConfig {
    Command(String),
    Script { script: String, shell: Option<String> },
    Function { function: String, args: toml::Table },
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    #[default]
    Always,
    OnSuccess,
    OnFailure,
    Expression(String),
}

pub struct WorkflowEngine {
    functions: HashMap<String, Box<dyn TaskFunction>>,
}

#[async_trait]
pub trait TaskFunction: Send + Sync {
    async fn execute(&self, args: &toml::Table) -> Result<serde_json::Value>;
}

impl WorkflowEngine {
    pub async fn execute_workflow(&self, workflow: Workflow) -> Result<()> {
        let mut completed = HashSet::new();
        let mut results = HashMap::new();
        
        while completed.len() < workflow.tasks.len() {
            let mut progress = false;
            
            for task in &workflow.tasks {
                if completed.contains(&task.name) {
                    continue;
                }
                
                // Check dependencies
                if task.depends_on.iter().all(|dep| completed.contains(dep)) {
                    info!("Executing task: {}", task.name);
                    
                    let result = self.execute_task(task, &results).await;
                    
                    match result {
                        Ok(value) => {
                            results.insert(task.name.clone(), value);
                            completed.insert(task.name.clone());
                            progress = true;
                        }
                        Err(e) => {
                            error!("Task {} failed: {}", task.name, e);
                            return Err(e);
                        }
                    }
                }
            }
            
            if !progress {
                bail!("Circular dependency detected in workflow");
            }
        }
        
        Ok(())
    }
    
    async fn execute_task(
        &self,
        task: &Task,
        context: &HashMap<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        match &task.run {
            RunConfig::Command(cmd) => {
                let output = shell_words::split(cmd)?;
                let result = TokioCommand::new(&output[0])
                    .args(&output[1..])
                    .output()
                    .await?;
                
                if !result.status.success() {
                    bail!("Command failed: {}", cmd);
                }
                
                Ok(json!({
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                }))
            }
            
            RunConfig::Script { script, shell } => {
                let shell = shell.as_deref().unwrap_or("sh");
                let result = TokioCommand::new(shell)
                    .arg("-c")
                    .arg(script)
                    .output()
                    .await?;
                
                Ok(json!({
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                }))
            }
            
            RunConfig::Function { function, args } => {
                let func = self.functions.get(function)
                    .ok_or_else(|| anyhow!("Unknown function: {}", function))?;
                
                func.execute(args).await
            }
        }
    }
}
```

### ✅ DO: Create Smart File Watchers

```rust
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub struct FileWatcher {
    watcher: RecommendedWatcher,
    rx: mpsc::Receiver<WatchEvent>,
}

#[derive(Debug, Clone)]
pub enum WatchEvent {
    Changed(PathBuf),
    Created(PathBuf),
    Removed(PathBuf),
}

impl FileWatcher {
    pub fn new(paths: Vec<PathBuf>, ignore_patterns: Vec<String>) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);
        let ignore = GlobSet::from_patterns(&ignore_patterns)?;
        
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    let path = &event.paths[0];
                    
                    // Apply ignore patterns
                    if ignore.is_match(path) {
                        return;
                    }
                    
                    let watch_event = match event.kind {
                        notify::EventKind::Create(_) => WatchEvent::Created(path.clone()),
                        notify::EventKind::Modify(_) => WatchEvent::Changed(path.clone()),
                        notify::EventKind::Remove(_) => WatchEvent::Removed(path.clone()),
                        _ => return,
                    };
                    
                    let _ = tx.blocking_send(watch_event);
                }
            },
            Config::default(),
        )?;
        
        // Watch all paths
        for path in paths {
            watcher.watch(&path, RecursiveMode::Recursive)?;
        }
        
        Ok(Self { watcher, rx })
    }
    
    pub async fn watch<F, Fut>(
        mut self,
        mut on_change: F,
    ) -> Result<()>
    where
        F: FnMut(WatchEvent) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let mut debounce = HashMap::new();
        let debounce_duration = Duration::from_millis(100);
        
        while let Some(event) = self.rx.recv().await {
            let path = match &event {
                WatchEvent::Changed(p) | WatchEvent::Created(p) | WatchEvent::Removed(p) => p,
            };
            
            // Debounce rapid changes
            let now = Instant::now();
            if let Some(last) = debounce.get(path) {
                if now.duration_since(*last) < debounce_duration {
                    continue;
                }
            }
            debounce.insert(path.clone(), now);
            
            if let Err(e) = on_change(event).await {
                error!("Handler error: {}", e);
            }
        }
        
        Ok(())
    }
}

// Usage
pub async fn watch_and_rebuild(project_dir: PathBuf) -> Result<()> {
    let watcher = FileWatcher::new(
        vec![project_dir.join("src")],
        vec!["*.tmp".to_string(), "target/*".to_string()],
    )?;
    
    info!("Watching for changes...");
    
    watcher.watch(|event| async move {
        match event {
            WatchEvent::Changed(path) | WatchEvent::Created(path) => {
                info!("Detected change in {}", path.display());
                
                // Rebuild project
                long_operation("Rebuilding", || async {
                    run_build().await
                }).await?;
                
                success!("Build complete");
            }
            WatchEvent::Removed(_) => {
                // Ignore removals
            }
        }
        
        Ok(())
    }).await
}
```

---

## 12. Performance Optimization

### ✅ DO: Optimize Startup Time

```rust
// Use lazy initialization for expensive operations
use once_cell::sync::Lazy;

static CONFIG: Lazy<Config> = Lazy::new(|| {
    Config::load_from_default_location()
        .expect("Failed to load config")
});

// Defer imports until needed
pub async fn handle_rare_command() -> Result<()> {
    // Only load heavy dependency when this command runs
    use heavy_dependency::ComplexProcessor;
    
    let processor = ComplexProcessor::new();
    processor.run().await
}

// Use compile-time includes for static data
static HELP_TEXT: &str = include_str!("../help.txt");
static DEFAULT_CONFIG: &[u8] = include_bytes!("../default-config.toml");

// Profile startup time
#[cfg(feature = "profiling")]
fn main() {
    let start = std::time::Instant::now();
    
    let result = actual_main();
    
    eprintln!("Startup time: {:?}", start.elapsed());
    
    std::process::exit(match result {
        Ok(()) => 0,
        Err(_) => 1,
    });
}
```

### ✅ DO: Use Zero-Copy Parsing

```rust
use nom::{
    IResult,
    bytes::complete::{tag, take_until},
    character::complete::{line_ending, not_line_ending},
    multi::many0,
    sequence::{delimited, pair},
};

// Parse without allocations
pub fn parse_config_line(input: &str) -> IResult<&str, (&str, &str)> {
    pair(
        take_until("="),
        delimited(tag("="), not_line_ending, line_ending),
    )(input)
}

// Use memory-mapped files for large inputs
use memmap2::Mmap;

pub fn process_large_file(path: &Path) -> Result<()> {
    let file = std::fs::File::open(path)?;
    let mmap = unsafe { Mmap::map(&file)? };
    
    // Process directly from memory-mapped data
    let text = std::str::from_utf8(&mmap)?;
    
    for line in text.lines() {
        // Zero-copy line processing
        process_line(line)?;
    }
    
    Ok(())
}
```

---

## 13. Security Best Practices

### ✅ DO: Validate All External Input

```rust
use validator::{Validate, ValidationError};

#[derive(Debug, Validate)]
pub struct DeploymentConfig {
    #[validate(length(min = 1, max = 64), regex = "IDENTIFIER_REGEX")]
    pub name: String,
    
    #[validate(url)]
    pub endpoint: String,
    
    #[validate(range(min = 1, max = 65535))]
    pub port: u16,
    
    #[validate(custom = "validate_path")]
    pub working_dir: PathBuf,
}

static IDENTIFIER_REGEX: Lazy<regex::Regex> = Lazy::new(|| {
    regex::Regex::new(r"^[a-zA-Z][a-zA-Z0-9_-]*$").unwrap()
});

fn validate_path(path: &PathBuf) -> Result<(), ValidationError> {
    // Prevent directory traversal
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(ValidationError::new("invalid_path"));
    }
    
    // Must be within working directory
    if !path.starts_with("/home/user/projects") {
        return Err(ValidationError::new("outside_working_directory"));
    }
    
    Ok(())
}

// Sanitize shell commands
pub fn run_user_command(cmd: &str) -> Result<()> {
    // Never pass user input directly to shell
    let parts = shell_words::split(cmd)?;
    
    if parts.is_empty() {
        bail!("Empty command");
    }
    
    // Whitelist allowed commands
    let allowed_commands = ["ls", "cat", "grep", "find"];
    if !allowed_commands.contains(&parts[0].as_str()) {
        bail!("Command not allowed: {}", parts[0]);
    }
    
    let output = std::process::Command::new(&parts[0])
        .args(&parts[1..])
        .output()?;
    
    if !output.status.success() {
        bail!("Command failed");
    }
    
    Ok(())
}
```

### ✅ DO: Store Secrets Securely

```rust
use keyring::Entry;
use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce, Key,
};

pub struct SecretStore {
    app_name: String,
}

impl SecretStore {
    pub fn new(app_name: &str) -> Self {
        Self {
            app_name: app_name.to_string(),
        }
    }
    
    // Store in OS keychain
    pub fn store_token(&self, name: &str, token: &str) -> Result<()> {
        let entry = Entry::new(&self.app_name, name)?;
        entry.set_password(token)?;
        Ok(())
    }
    
    pub fn get_token(&self, name: &str) -> Result<Option<String>> {
        let entry = Entry::new(&self.app_name, name)?;
        match entry.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    
    // Encrypt sensitive files
    pub fn encrypt_file(&self, path: &Path, key: &[u8; 32]) -> Result<()> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        
        let plaintext = std::fs::read(path)?;
        let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref())
            .map_err(|e| anyhow!("Encryption failed: {}", e))?;
        
        // Write nonce + ciphertext
        let mut output = nonce.to_vec();
        output.extend_from_slice(&ciphertext);
        
        let encrypted_path = path.with_extension("enc");
        std::fs::write(encrypted_path, output)?;
        
        // Securely delete original
        std::fs::remove_file(path)?;
        
        Ok(())
    }
}
```

---

## 14. Debugging and Diagnostics

### ✅ DO: Implement Comprehensive Debug Mode

```rust
pub struct DebugMode {
    enabled: bool,
    trace_file: Option<File>,
}

impl DebugMode {
    pub fn from_env() -> Self {
        let enabled = std::env::var("MYCTL_DEBUG").is_ok();
        
        let trace_file = if enabled {
            std::env::var("MYCTL_TRACE_FILE")
                .ok()
                .and_then(|path| File::create(path).ok())
        } else {
            None
        };
        
        Self { enabled, trace_file }
    }
    
    pub fn trace<F>(&mut self, f: F)
    where
        F: FnOnce() -> String,
    {
        if self.enabled {
            let msg = f();
            eprintln!("{} {}", style("[TRACE]").dim(), msg);
            
            if let Some(file) = &mut self.trace_file {
                writeln!(file, "[{}] {}", chrono::Local::now(), msg).ok();
            }
        }
    }
}

// Debug command implementation
pub async fn debug_info() -> Result<()> {
    println!("{}", style("System Information").bold().underline());
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Commit: {}", env!("VERGEN_GIT_SHA"));
    println!("Built: {}", env!("VERGEN_BUILD_TIMESTAMP"));
    println!("Rust: {}", env!("VERGEN_RUSTC_SEMVER"));
    
    println!("\n{}", style("Environment").bold().underline());
    for (key, value) in std::env::vars() {
        if key.starts_with("MYCTL_") {
            println!("{}: {}", key, value);
        }
    }
    
    println!("\n{}", style("Configuration").bold().underline());
    let config = Config::load(None).await?;
    println!("{:#?}", config);
    
    println!("\n{}", style("Paths").bold().underline());
    if let Some(dirs) = directories::ProjectDirs::from("com", "example", "myctl") {
        println!("Config: {}", dirs.config_dir().display());
        println!("Data: {}", dirs.data_dir().display());
        println!("Cache: {}", dirs.cache_dir().display());
    }
    
    Ok(())
}

// Performance tracing
#[instrument(level = "debug", skip(client))]
pub async fn api_call(client: &Client, endpoint: &str) -> Result<Response> {
    let start = Instant::now();
    
    let response = client.get(endpoint).send().await?;
    
    debug!(
        elapsed = ?start.elapsed(),
        status = response.status().as_u16(),
        "API call completed"
    );
    
    Ok(response)
}
```

---

## Conclusion

This guide provides a comprehensive foundation for building professional CLI tools with Rust. The key principles to remember:

1. **User Experience First** - Fast startup, helpful errors, beautiful output
2. **Robustness** - Handle errors gracefully, validate inputs, test thoroughly  
3. **Performance** - Profile before optimizing, use async wisely, minimize allocations
4. **Flexibility** - Support multiple platforms, output formats, and use cases
5. **Maintainability** - Structure code well, document thoroughly, automate releases

The Rust ecosystem for CLI tools continues to evolve rapidly. Stay updated with the latest crate versions and patterns, but always prioritize user experience and reliability over using the newest features.

For more examples and the latest updates to this guide, visit the companion repository at [github.com/rust-cli/definitive-guide](https://github.com/rust-cli/definitive-guide).
