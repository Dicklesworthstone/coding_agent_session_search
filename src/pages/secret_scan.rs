use anyhow::{Context, Result, bail};
use console::{Term, style};
use indicatif::{ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const DEFAULT_ENTROPY_THRESHOLD: f64 = 4.0;
const DEFAULT_ENTROPY_MIN_LEN: usize = 20;
const DEFAULT_CONTEXT_BYTES: usize = 120;
const DEFAULT_MAX_FINDINGS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSeverity {
    Critical,
    High,
    Medium,
    Low,
}

impl SecretSeverity {
    fn rank(self) -> u8 {
        match self {
            SecretSeverity::Critical => 0,
            SecretSeverity::High => 1,
            SecretSeverity::Medium => 2,
            SecretSeverity::Low => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SecretSeverity::Critical => "critical",
            SecretSeverity::High => "high",
            SecretSeverity::Medium => "medium",
            SecretSeverity::Low => "low",
        }
    }

    fn styled(self, text: &str) -> String {
        match self {
            SecretSeverity::Critical => style(text).red().bold().to_string(),
            SecretSeverity::High => style(text).red().to_string(),
            SecretSeverity::Medium => style(text).yellow().to_string(),
            SecretSeverity::Low => style(text).blue().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretLocation {
    ConversationTitle,
    ConversationMetadata,
    MessageContent,
    MessageMetadata,
}

impl SecretLocation {
    fn label(&self) -> &'static str {
        match self {
            SecretLocation::ConversationTitle => "conversation.title",
            SecretLocation::ConversationMetadata => "conversation.metadata",
            SecretLocation::MessageContent => "message.content",
            SecretLocation::MessageMetadata => "message.metadata",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretFinding {
    pub severity: SecretSeverity,
    pub kind: String,
    pub pattern: String,
    pub match_redacted: String,
    pub context: String,
    pub location: SecretLocation,
    pub agent: Option<String>,
    pub workspace: Option<String>,
    pub source_path: Option<String>,
    pub conversation_id: Option<i64>,
    pub message_id: Option<i64>,
    pub message_idx: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretScanSummary {
    pub total: usize,
    pub by_severity: HashMap<SecretSeverity, usize>,
    pub has_critical: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretScanReport {
    pub summary: SecretScanSummary,
    pub findings: Vec<SecretFinding>,
}

#[derive(Debug, Clone)]
pub struct SecretScanFilters {
    pub agents: Option<Vec<String>>,
    pub workspaces: Option<Vec<PathBuf>>,
    pub since_ts: Option<i64>,
    pub until_ts: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SecretScanConfig {
    pub allowlist: Vec<Regex>,
    pub denylist: Vec<Regex>,
    pub allowlist_raw: Vec<String>,
    pub denylist_raw: Vec<String>,
    pub entropy_threshold: f64,
    pub entropy_min_len: usize,
    pub context_bytes: usize,
    pub max_findings: usize,
}

impl SecretScanConfig {
    pub fn from_inputs(allowlist: &[String], denylist: &[String]) -> Result<Self> {
        Self::from_inputs_with_env(allowlist, denylist, true)
    }

    pub fn from_inputs_with_env(
        allowlist: &[String],
        denylist: &[String],
        use_env: bool,
    ) -> Result<Self> {
        let allowlist_raw = if allowlist.is_empty() && use_env {
            parse_env_regex_list("CASS_SECRETS_ALLOWLIST")?
        } else {
            allowlist.to_vec()
        };
        let denylist_raw = if denylist.is_empty() && use_env {
            parse_env_regex_list("CASS_SECRETS_DENYLIST")?
        } else {
            denylist.to_vec()
        };

        Ok(Self {
            allowlist: compile_regexes(&allowlist_raw, "allowlist")?,
            denylist: compile_regexes(&denylist_raw, "denylist")?,
            allowlist_raw,
            denylist_raw,
            entropy_threshold: DEFAULT_ENTROPY_THRESHOLD,
            entropy_min_len: DEFAULT_ENTROPY_MIN_LEN,
            context_bytes: DEFAULT_CONTEXT_BYTES,
            max_findings: DEFAULT_MAX_FINDINGS,
        })
    }
}

struct SecretPattern {
    id: &'static str,
    severity: SecretSeverity,
    regex: Regex,
}

static BUILTIN_PATTERNS: Lazy<Vec<SecretPattern>> = Lazy::new(|| {
    vec![
        SecretPattern {
            id: "aws_access_key_id",
            severity: SecretSeverity::High,
            regex: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("aws access key regex"),
        },
        SecretPattern {
            id: "aws_secret_key",
            severity: SecretSeverity::Critical,
            regex: Regex::new(
                r#"(?i)aws(.{0,20})?(secret|access)?[_-]?key\s*[:=]\s*['"]?[A-Za-z0-9/+=]{40}['"]?"#,
            )
                .expect("aws secret regex"),
        },
        SecretPattern {
            id: "github_pat",
            severity: SecretSeverity::High,
            regex: Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{36}\b").expect("github pat regex"),
        },
        SecretPattern {
            id: "openai_key",
            severity: SecretSeverity::High,
            regex: Regex::new(r"\bsk-[A-Za-z0-9]{20,}\b").expect("openai key regex"),
        },
        SecretPattern {
            id: "anthropic_key",
            severity: SecretSeverity::High,
            regex: Regex::new(r"\bsk-ant-[A-Za-z0-9]{20,}\b").expect("anthropic key regex"),
        },
        SecretPattern {
            id: "jwt",
            severity: SecretSeverity::Medium,
            regex: Regex::new(r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\b")
                .expect("jwt regex"),
        },
        SecretPattern {
            id: "private_key",
            severity: SecretSeverity::Critical,
            regex: Regex::new(r"-----BEGIN (?:RSA|EC|DSA|OPENSSH|PGP) PRIVATE KEY-----")
                .expect("private key regex"),
        },
        SecretPattern {
            id: "database_url",
            severity: SecretSeverity::Medium,
            regex: Regex::new(r"(?i)\b(postgres|postgresql|mysql|mongodb|redis)://[^\s]+")
                .expect("db url regex"),
        },
        SecretPattern {
            id: "generic_api_key",
            severity: SecretSeverity::Low,
            regex: Regex::new(
                r#"(?i)(api[_-]?key|token|secret|password|passwd)\s*[:=]\s*['"]?[A-Za-z0-9_\-]{8,}['"]?"#,
            )
            .expect("generic api key regex"),
        },
    ]
});

static ENTROPY_BASE64_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{20,}").expect("entropy base64 regex"));
static ENTROPY_HEX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b[A-Fa-f0-9]{32,}\b").expect("entropy hex regex"));

#[derive(Debug, Clone)]
struct ScanContext {
    agent: Option<String>,
    workspace: Option<String>,
    source_path: Option<String>,
    conversation_id: Option<i64>,
    message_id: Option<i64>,
    message_idx: Option<i64>,
}

struct FindingCandidate<'a> {
    severity: SecretSeverity,
    kind: &'a str,
    pattern: &'a str,
    text: &'a str,
    start: usize,
    end: usize,
    location: SecretLocation,
    ctx: &'a ScanContext,
}

pub fn scan_database<P: AsRef<Path>>(
    db_path: P,
    filters: &SecretScanFilters,
    config: &SecretScanConfig,
    running: Option<Arc<AtomicBool>>,
    progress: Option<&ProgressBar>,
) -> Result<SecretScanReport> {
    let conn = Connection::open_with_flags(
        db_path.as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("Failed to open database for secret scan")?;

    let mut findings: Vec<SecretFinding> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut truncated = false;

    let (conv_where, conv_params) = build_where_clause(filters)?;
    let conv_sql = format!(
        "SELECT c.id, c.title, c.metadata_json, c.source_path, a.slug, w.path\n         FROM conversations c\n         JOIN agents a ON c.agent_id = a.id\n         LEFT JOIN workspaces w ON c.workspace_id = w.id{}",
        conv_where
    );
    let mut conv_stmt = conn.prepare(&conv_sql)?;
    let mut conv_rows = conv_stmt.query(rusqlite::params_from_iter(
        conv_params.iter().map(|p| p.as_ref()),
    ))?;

    while let Some(row) = conv_rows.next()? {
        if running
            .as_ref()
            .is_some_and(|flag| !flag.load(Ordering::Relaxed))
        {
            break;
        }
        let conv_id: i64 = row.get(0)?;
        let title: Option<String> = row.get(1)?;
        let metadata_json: Option<String> = row.get(2)?;
        let source_path: String = row.get(3)?;
        let agent_slug: String = row.get(4)?;
        let workspace_path: Option<String> = row.get(5)?;

        let ctx = ScanContext {
            agent: Some(agent_slug),
            workspace: workspace_path,
            source_path: Some(source_path),
            conversation_id: Some(conv_id),
            message_id: None,
            message_idx: None,
        };

        if let Some(title_text) = title {
            scan_text(
                &title_text,
                SecretLocation::ConversationTitle,
                &ctx,
                config,
                &mut findings,
                &mut seen,
                &mut truncated,
            );
        }
        if let Some(meta) = metadata_json {
            scan_text(
                &meta,
                SecretLocation::ConversationMetadata,
                &ctx,
                config,
                &mut findings,
                &mut seen,
                &mut truncated,
            );
        }

        if truncated {
            break;
        }

        if let Some(pb) = progress {
            pb.inc(1);
        }
    }

    if !truncated {
        let (msg_where, msg_params) = build_where_clause(filters)?;
        let msg_sql = format!(
            "SELECT m.id, m.idx, m.content, m.extra_json, c.id, c.source_path, a.slug, w.path\n             FROM messages m\n             JOIN conversations c ON m.conversation_id = c.id\n             JOIN agents a ON c.agent_id = a.id\n             LEFT JOIN workspaces w ON c.workspace_id = w.id{}",
            msg_where
        );
        let mut msg_stmt = conn.prepare(&msg_sql)?;
        let mut msg_rows = msg_stmt.query(rusqlite::params_from_iter(
            msg_params.iter().map(|p| p.as_ref()),
        ))?;

        while let Some(row) = msg_rows.next()? {
            if running
                .as_ref()
                .is_some_and(|flag| !flag.load(Ordering::Relaxed))
            {
                break;
            }
            let msg_id: i64 = row.get(0)?;
            let msg_idx: i64 = row.get(1)?;
            let content: String = row.get(2)?;
            let extra_json: Option<String> = row.get(3)?;
            let conv_id: i64 = row.get(4)?;
            let source_path: String = row.get(5)?;
            let agent_slug: String = row.get(6)?;
            let workspace_path: Option<String> = row.get(7)?;

            let ctx = ScanContext {
                agent: Some(agent_slug),
                workspace: workspace_path,
                source_path: Some(source_path),
                conversation_id: Some(conv_id),
                message_id: Some(msg_id),
                message_idx: Some(msg_idx),
            };

            scan_text(
                &content,
                SecretLocation::MessageContent,
                &ctx,
                config,
                &mut findings,
                &mut seen,
                &mut truncated,
            );
            if let Some(extra) = extra_json {
                scan_text(
                    &extra,
                    SecretLocation::MessageMetadata,
                    &ctx,
                    config,
                    &mut findings,
                    &mut seen,
                    &mut truncated,
                );
            }

            if truncated {
                break;
            }

            if let Some(pb) = progress {
                pb.inc(1);
            }
        }
    }

    findings.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then_with(|| a.kind.cmp(&b.kind))
    });

    let mut by_severity: HashMap<SecretSeverity, usize> = HashMap::new();
    for finding in &findings {
        *by_severity.entry(finding.severity).or_insert(0) += 1;
    }

    let has_critical = by_severity
        .get(&SecretSeverity::Critical)
        .copied()
        .unwrap_or(0)
        > 0;

    Ok(SecretScanReport {
        summary: SecretScanSummary {
            total: findings.len(),
            by_severity,
            has_critical,
            truncated,
        },
        findings,
    })
}

pub fn print_human_report(
    term: &mut Term,
    report: &SecretScanReport,
    max_examples: usize,
) -> Result<()> {
    let total = report.summary.total;
    if total == 0 {
        writeln!(term, "  {} No secrets detected", style("✓").green())?;
        return Ok(());
    }

    writeln!(
        term,
        "  {} {} potential secret(s) detected",
        style("⚠").yellow(),
        total
    )?;

    let mut severities = vec![
        SecretSeverity::Critical,
        SecretSeverity::High,
        SecretSeverity::Medium,
        SecretSeverity::Low,
    ];

    severities.sort_by_key(|s| s.rank());

    for severity in severities {
        let count = report
            .summary
            .by_severity
            .get(&severity)
            .copied()
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        let label = severity.styled(severity.label());
        writeln!(term, "  {}: {}", label, count)?;

        for finding in report
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .take(max_examples)
        {
            writeln!(
                term,
                "    - {} in {} ({})",
                finding.kind,
                finding.location.label(),
                finding.match_redacted
            )?;
            if !finding.context.is_empty() {
                writeln!(term, "      {}", style(&finding.context).dim())?;
            }
        }
        if count > max_examples {
            writeln!(term, "      {}", style("…additional findings hidden").dim())?;
        }
    }

    if report.summary.truncated {
        writeln!(
            term,
            "  {} Results truncated (max findings reached)",
            style("⚠").yellow()
        )?;
    }

    Ok(())
}

pub fn print_cli_report(report: &SecretScanReport, json: bool) -> Result<()> {
    if json {
        let payload = serde_json::to_string_pretty(report)?;
        println!("{payload}");
        return Ok(());
    }

    let mut term = Term::stdout();
    print_human_report(&mut term, report, 3)
}

pub fn run_secret_scan_cli<P: AsRef<Path>>(
    db_path: P,
    filters: &SecretScanFilters,
    config: &SecretScanConfig,
    json: bool,
    fail_on_secrets: bool,
) -> Result<()> {
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    progress.set_message("Scanning for secrets...");
    progress.enable_steady_tick(Duration::from_millis(120));

    let report = scan_database(db_path, filters, config, None, Some(&progress))?;
    progress.finish_and_clear();

    print_cli_report(&report, json)?;

    if fail_on_secrets && report.summary.total > 0 {
        bail!("Secrets detected ({} finding(s))", report.summary.total);
    }

    Ok(())
}

pub fn wizard_secret_scan<P: AsRef<Path>>(
    db_path: P,
    filters: &SecretScanFilters,
    config: &SecretScanConfig,
) -> Result<SecretScanReport> {
    let progress = ProgressBar::new_spinner();
    progress.set_style(
        ProgressStyle::with_template("{spinner} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    progress.set_message("Scanning for secrets...");
    progress.enable_steady_tick(Duration::from_millis(120));

    let report = scan_database(db_path, filters, config, None, Some(&progress))?;
    progress.finish_and_clear();
    Ok(report)
}

fn scan_text(
    text: &str,
    location: SecretLocation,
    ctx: &ScanContext,
    config: &SecretScanConfig,
    findings: &mut Vec<SecretFinding>,
    seen: &mut HashSet<String>,
    truncated: &mut bool,
) {
    if *truncated || text.is_empty() {
        return;
    }

    // Denylist first (always critical)
    for deny in &config.denylist {
        for mat in deny.find_iter(text) {
            if findings.len() >= config.max_findings {
                *truncated = true;
                return;
            }
            push_finding(
                findings,
                seen,
                FindingCandidate {
                    severity: SecretSeverity::Critical,
                    kind: "denylist",
                    pattern: deny.as_str(),
                    text,
                    start: mat.start(),
                    end: mat.end(),
                    location: location.clone(),
                    ctx,
                },
                config,
            );
        }
    }

    // Built-in patterns
    for pattern in BUILTIN_PATTERNS.iter() {
        for mat in pattern.regex.find_iter(text) {
            if findings.len() >= config.max_findings {
                *truncated = true;
                return;
            }
            let matched = &text[mat.start()..mat.end()];
            if is_allowlisted(matched, config) {
                continue;
            }
            push_finding(
                findings,
                seen,
                FindingCandidate {
                    severity: pattern.severity,
                    kind: pattern.id,
                    pattern: pattern.regex.as_str(),
                    text,
                    start: mat.start(),
                    end: mat.end(),
                    location: location.clone(),
                    ctx,
                },
                config,
            );
        }
    }

    // Entropy-based detection
    for mat in ENTROPY_BASE64_RE.find_iter(text) {
        if findings.len() >= config.max_findings {
            *truncated = true;
            return;
        }
        let candidate = &text[mat.start()..mat.end()];
        if candidate.len() < config.entropy_min_len {
            continue;
        }
        if is_allowlisted(candidate, config) {
            continue;
        }
        let entropy = shannon_entropy(candidate);
        if entropy >= config.entropy_threshold {
            push_finding(
                findings,
                seen,
                FindingCandidate {
                    severity: SecretSeverity::Medium,
                    kind: "high_entropy_base64",
                    pattern: "entropy",
                    text,
                    start: mat.start(),
                    end: mat.end(),
                    location: location.clone(),
                    ctx,
                },
                config,
            );
        }
    }

    for mat in ENTROPY_HEX_RE.find_iter(text) {
        if findings.len() >= config.max_findings {
            *truncated = true;
            return;
        }
        let candidate = &text[mat.start()..mat.end()];
        if candidate.len() < 32 {
            continue;
        }
        if is_allowlisted(candidate, config) {
            continue;
        }
        let entropy = shannon_entropy(candidate);
        if entropy >= 3.0 {
            push_finding(
                findings,
                seen,
                FindingCandidate {
                    severity: SecretSeverity::Low,
                    kind: "high_entropy_hex",
                    pattern: "entropy",
                    text,
                    start: mat.start(),
                    end: mat.end(),
                    location: location.clone(),
                    ctx,
                },
                config,
            );
        }
    }
}

fn push_finding(
    findings: &mut Vec<SecretFinding>,
    seen: &mut HashSet<String>,
    candidate: FindingCandidate<'_>,
    config: &SecretScanConfig,
) {
    let match_text = &candidate.text[candidate.start..candidate.end];
    let match_redacted = redact_token(match_text);
    let context = redact_context(
        candidate.text,
        candidate.start,
        candidate.end,
        config.context_bytes,
        &match_redacted,
    );

    let key = format!(
        "{}:{}:{}:{}:{}",
        candidate.ctx.conversation_id.unwrap_or_default(),
        candidate.ctx.message_id.unwrap_or_default(),
        candidate.location.label(),
        candidate.kind,
        match_redacted
    );

    if !seen.insert(key) {
        return;
    }

    findings.push(SecretFinding {
        severity: candidate.severity,
        kind: candidate.kind.to_string(),
        pattern: candidate.pattern.to_string(),
        match_redacted,
        context,
        location: candidate.location,
        agent: candidate.ctx.agent.clone(),
        workspace: candidate.ctx.workspace.clone(),
        source_path: candidate.ctx.source_path.clone(),
        conversation_id: candidate.ctx.conversation_id,
        message_id: candidate.ctx.message_id,
        message_idx: candidate.ctx.message_idx,
    });
}

fn redact_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let len = chars.len();
    if len <= 8 {
        return "[redacted]".to_string();
    }
    let prefix: String = chars.iter().take(4).collect();
    let suffix: String = chars
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{}…{} (len {})", prefix, suffix, len)
}

fn redact_context(
    text: &str,
    start: usize,
    end: usize,
    window: usize,
    replacement: &str,
) -> String {
    if text.is_empty() || start >= end || start >= text.len() {
        return String::new();
    }

    let ctx_start = start.saturating_sub(window / 2);
    let ctx_end = (end + window / 2).min(text.len());
    let ctx_start = adjust_to_char_boundary(text, ctx_start, false);
    let ctx_end = adjust_to_char_boundary(text, ctx_end, true);

    if ctx_start >= ctx_end {
        return String::new();
    }

    let safe_start = start.min(text.len());
    let safe_end = end.min(text.len());

    let prefix = &text[ctx_start..safe_start];
    let suffix = &text[safe_end..ctx_end];

    let mut snippet = String::new();
    snippet.push_str(prefix);
    snippet.push_str(replacement);
    snippet.push_str(suffix);
    snippet
}

fn adjust_to_char_boundary(text: &str, idx: usize, forward: bool) -> usize {
    if idx >= text.len() {
        return text.len();
    }
    if text.is_char_boundary(idx) {
        return idx;
    }
    if forward {
        for i in idx..text.len() {
            if text.is_char_boundary(i) {
                return i;
            }
        }
        text.len()
    } else {
        for i in (0..=idx).rev() {
            if text.is_char_boundary(i) {
                return i;
            }
        }
        0
    }
}

fn shannon_entropy(token: &str) -> f64 {
    let bytes = token.as_bytes();
    let len = bytes.len() as f64;
    if len == 0.0 {
        return 0.0;
    }
    let mut freq = [0usize; 256];
    for b in bytes {
        freq[*b as usize] += 1;
    }
    let mut entropy = 0.0;
    for count in freq.iter().copied() {
        if count == 0 {
            continue;
        }
        let p = count as f64 / len;
        entropy -= p * p.log2();
    }
    entropy
}

fn is_allowlisted(matched: &str, config: &SecretScanConfig) -> bool {
    for allow in &config.allowlist {
        if allow.is_match(matched) {
            return true;
        }
    }
    false
}

fn build_where_clause(
    filters: &SecretScanFilters,
) -> Result<(String, Vec<Box<dyn rusqlite::ToSql>>)> {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(agents) = filters.agents.as_ref().filter(|a| !a.is_empty()) {
        let placeholders: Vec<&str> = agents.iter().map(|_| "?").collect();
        conditions.push(format!("a.slug IN ({})", placeholders.join(", ")));
        for agent in agents {
            params.push(Box::new(agent.clone()));
        }
    }

    if let Some(workspaces) = filters.workspaces.as_ref().filter(|w| !w.is_empty()) {
        let placeholders: Vec<&str> = workspaces.iter().map(|_| "?").collect();
        conditions.push(format!("w.path IN ({})", placeholders.join(", ")));
        for ws in workspaces {
            params.push(Box::new(ws.to_string_lossy().to_string()));
        }
    }

    if let Some(since) = filters.since_ts {
        conditions.push("c.started_at >= ?".to_string());
        params.push(Box::new(since));
    }

    if let Some(until) = filters.until_ts {
        conditions.push("c.started_at <= ?".to_string());
        params.push(Box::new(until));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };

    Ok((where_clause, params))
}

fn parse_env_regex_list(var: &str) -> Result<Vec<String>> {
    let value = match dotenvy::var(var) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let items = value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    Ok(items)
}

fn compile_regexes(patterns: &[String], label: &str) -> Result<Vec<Regex>> {
    let mut compiled = Vec::new();
    for pat in patterns {
        let regex = Regex::new(pat).with_context(|| format!("Invalid {} regex: {}", label, pat))?;
        compiled.push(regex);
    }
    Ok(compiled)
}
