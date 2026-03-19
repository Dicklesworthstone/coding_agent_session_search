use anyhow::{Context, Result, bail};
use console::{Term, style};
use frankensqlite::compat::{ParamValue, RowExt, params_from_iter};
use indicatif::{ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use regex::Regex;
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
            // Note: this also matches Anthropic keys (sk-ant-...) — the anthropic_key
            // pattern below is more specific and checked separately. Dedup by position
            // in the caller prevents double-reporting.
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
            regex: Regex::new(
                r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----",
            )
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
    let conn = super::open_existing_sqlite_db(db_path.as_ref())
        .context("Failed to open database for secret scan")?;

    let mut findings: Vec<SecretFinding> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut truncated = false;

    let (conv_where, conv_params) = build_where_clause(filters)?;
    let conv_sql = format!(
        "SELECT c.id, c.title, c.metadata_json, c.source_path, a.slug, w.path\n         FROM conversations c\n         JOIN agents a ON c.agent_id = a.id\n         LEFT JOIN workspaces w ON c.workspace_id = w.id{}",
        conv_where
    );
    let conv_param_values = params_from_iter(conv_params);
    let conv_rows = conn.query_with_params(&conv_sql, &conv_param_values)?;

    for row in &conv_rows {
        if running
            .as_ref()
            .is_some_and(|flag| !flag.load(Ordering::Relaxed))
        {
            break;
        }
        let conv_id: i64 = row.get_typed(0)?;
        let title: Option<String> = row.get_typed(1)?;
        let metadata_json: Option<String> = row.get_typed(2)?;
        let source_path: String = row.get_typed(3)?;
        let agent_slug: String = row.get_typed(4)?;
        let workspace_path: Option<String> = row.get_typed(5)?;

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
        let msg_param_values = params_from_iter(msg_params);
        let msg_rows = conn.query_with_params(&msg_sql, &msg_param_values)?;

        for row in &msg_rows {
            if running
                .as_ref()
                .is_some_and(|flag| !flag.load(Ordering::Relaxed))
            {
                break;
            }
            let msg_id: i64 = row.get_typed(0)?;
            let msg_idx: i64 = row.get_typed(1)?;
            let content: String = row.get_typed(2)?;
            let extra_json: Option<String> = row.get_typed(3)?;
            let conv_id: i64 = row.get_typed(4)?;
            let source_path: String = row.get_typed(5)?;
            let agent_slug: String = row.get_typed(6)?;
            let workspace_path: Option<String> = row.get_typed(7)?;

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
        // Heuristic: Pure alphabetic strings are likely code identifiers (CamelCase), not secrets.
        // Secrets usually have digits or symbols.
        if candidate.chars().all(|c| c.is_ascii_alphabetic()) {
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
    let prefix: String = chars.iter().take(2).collect();
    let suffix: String = chars
        .iter()
        .rev()
        .take(2)
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

fn build_where_clause(filters: &SecretScanFilters) -> Result<(String, Vec<ParamValue>)> {
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<ParamValue> = Vec::new();

    if let Some(agents) = filters.agents.as_ref().filter(|a| !a.is_empty()) {
        let placeholders: Vec<&str> = agents.iter().map(|_| "?").collect();
        conditions.push(format!("a.slug IN ({})", placeholders.join(", ")));
        for agent in agents {
            params.push(ParamValue::from(agent.as_str()));
        }
    }

    if let Some(workspaces) = filters.workspaces.as_ref().filter(|w| !w.is_empty()) {
        let placeholders: Vec<&str> = workspaces.iter().map(|_| "?").collect();
        conditions.push(format!("w.path IN ({})", placeholders.join(", ")));
        for ws in workspaces {
            params.push(ParamValue::from(ws.to_string_lossy().to_string()));
        }
    }

    if let Some(since) = filters.since_ts {
        conditions.push("c.started_at >= ?".to_string());
        params.push(ParamValue::from(since));
    }

    if let Some(until) = filters.until_ts {
        conditions.push("c.started_at <= ?".to_string());
        params.push(ParamValue::from(until));
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

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Shannon entropy tests
    // =========================================================================

    #[test]
    fn shannon_entropy_empty_string_returns_zero() {
        assert_eq!(shannon_entropy(""), 0.0);
    }

    #[test]
    fn shannon_entropy_single_repeated_char_returns_zero() {
        assert_eq!(shannon_entropy("aaaaaaaaaa"), 0.0);
    }

    #[test]
    fn shannon_entropy_two_equal_chars_returns_one() {
        let e = shannon_entropy("ab");
        assert!((e - 1.0).abs() < 0.001, "expected ~1.0, got {}", e);
    }

    #[test]
    fn shannon_entropy_high_entropy_base64() {
        // A string with many distinct chars should have high entropy
        let token = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let e = shannon_entropy(token);
        assert!(e > 4.0, "expected entropy > 4.0, got {}", e);
    }

    #[test]
    fn shannon_entropy_hex_string() {
        let hex = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
        let e = shannon_entropy(hex);
        assert!(e > 3.0, "expected entropy > 3.0 for hex, got {}", e);
    }

    // =========================================================================
    // Redact token tests
    // =========================================================================

    #[test]
    fn redact_token_short_returns_redacted() {
        assert_eq!(redact_token("abcd"), "[redacted]");
        assert_eq!(redact_token("12345678"), "[redacted]");
    }

    #[test]
    fn redact_token_long_shows_prefix_suffix_len() {
        let result = redact_token("sk-abcdefghijklmnop");
        assert!(
            result.starts_with("sk"),
            "should start with first 2 chars: {}",
            result
        );
        assert!(
            result.contains("op"),
            "should end with last 2 chars: {}",
            result
        );
        assert!(result.contains("len 19"), "should show length: {}", result);
    }

    #[test]
    fn redact_token_nine_chars_shows_format() {
        let result = redact_token("123456789");
        assert!(result.starts_with("12"), "{}", result);
        assert!(result.contains("89"), "{}", result);
        assert!(result.contains("len 9"), "{}", result);
    }

    // =========================================================================
    // Redact context tests
    // =========================================================================

    #[test]
    fn redact_context_empty_text_returns_empty() {
        assert_eq!(redact_context("", 0, 0, 120, "[REDACTED]"), "");
    }

    #[test]
    fn redact_context_replaces_match_with_replacement() {
        let text = "The key is sk-ABCDEFGHIJ and more";
        let start = 11;
        let end = 25;
        let result = redact_context(text, start, end, 120, "[REDACTED]");
        assert!(result.contains("[REDACTED]"), "result: {}", result);
        assert!(
            !result.contains("sk-ABCDEFGHIJ"),
            "secret should be removed: {}",
            result
        );
    }

    #[test]
    fn redact_context_match_at_start() {
        let text = "sk-SECRET rest of the text";
        let result = redact_context(text, 0, 9, 120, "[R]");
        assert!(result.starts_with("[R]"), "result: {}", result);
    }

    #[test]
    fn redact_context_match_at_end() {
        let text = "prefix sk-SECRET";
        let result = redact_context(text, 7, 16, 120, "[R]");
        assert!(result.ends_with("[R]"), "result: {}", result);
    }

    #[test]
    fn redact_context_start_beyond_text_returns_empty() {
        assert_eq!(redact_context("short", 10, 15, 120, "[R]"), "");
    }

    // =========================================================================
    // Allowlist tests
    // =========================================================================

    #[test]
    fn is_allowlisted_returns_true_for_matching_pattern() {
        let config =
            SecretScanConfig::from_inputs_with_env(&["sk-test.*".to_string()], &[], false).unwrap();
        assert!(is_allowlisted("sk-test1234567890abcdef", &config));
    }

    #[test]
    fn is_allowlisted_returns_false_when_no_match() {
        let config =
            SecretScanConfig::from_inputs_with_env(&["sk-test.*".to_string()], &[], false).unwrap();
        assert!(!is_allowlisted("sk-prod1234567890abcdef", &config));
    }

    #[test]
    fn is_allowlisted_empty_list_returns_false() {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        assert!(!is_allowlisted("anything", &config));
    }

    // =========================================================================
    // Adjust to char boundary tests
    // =========================================================================

    #[test]
    fn adjust_to_char_boundary_ascii() {
        let text = "hello";
        assert_eq!(adjust_to_char_boundary(text, 3, true), 3);
        assert_eq!(adjust_to_char_boundary(text, 3, false), 3);
    }

    #[test]
    fn adjust_to_char_boundary_multibyte_forward() {
        let text = "héllo"; // 'é' is 2 bytes (0xC3 0xA9)
        // Index 2 is in the middle of 'é', forward should skip to next boundary
        let idx = adjust_to_char_boundary(text, 2, true);
        assert!(
            text.is_char_boundary(idx),
            "idx {} not a char boundary",
            idx
        );
    }

    #[test]
    fn adjust_to_char_boundary_multibyte_backward() {
        let text = "héllo";
        let idx = adjust_to_char_boundary(text, 2, false);
        assert!(
            text.is_char_boundary(idx),
            "idx {} not a char boundary",
            idx
        );
    }

    #[test]
    fn adjust_to_char_boundary_beyond_len() {
        let text = "abc";
        assert_eq!(adjust_to_char_boundary(text, 100, true), 3);
    }

    // =========================================================================
    // Config construction tests
    // =========================================================================

    #[test]
    fn config_from_inputs_with_valid_patterns() {
        let config = SecretScanConfig::from_inputs_with_env(
            &["allowed_.*".to_string()],
            &["denied_.*".to_string()],
            false,
        )
        .unwrap();
        assert_eq!(config.allowlist.len(), 1);
        assert_eq!(config.denylist.len(), 1);
        assert_eq!(config.entropy_threshold, DEFAULT_ENTROPY_THRESHOLD);
    }

    #[test]
    fn config_from_inputs_with_invalid_regex_returns_error() {
        let result = SecretScanConfig::from_inputs_with_env(&["[invalid".to_string()], &[], false);
        assert!(result.is_err(), "invalid regex should return error");
    }

    #[test]
    fn config_from_inputs_empty_lists() {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        assert!(config.allowlist.is_empty());
        assert!(config.denylist.is_empty());
        assert_eq!(config.max_findings, DEFAULT_MAX_FINDINGS);
    }

    // =========================================================================
    // Scan text tests (via scan_database with crafted DB)
    // =========================================================================

    #[test]
    fn builtin_patterns_aws_access_key_detected() {
        let text = "Found key AKIAIOSFODNN7EXAMPLE in config";
        let pattern = &BUILTIN_PATTERNS[0]; // aws_access_key_id
        assert!(
            pattern.regex.is_match(text),
            "should detect AWS access key ID"
        );
    }

    #[test]
    fn builtin_patterns_github_pat_detected() {
        let text = "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let pattern = &BUILTIN_PATTERNS[2]; // github_pat
        assert!(pattern.regex.is_match(text), "should detect GitHub PAT");
    }

    #[test]
    fn builtin_patterns_anthropic_key_detected() {
        let text = "sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh";
        let pattern = &BUILTIN_PATTERNS[4]; // anthropic_key
        assert!(pattern.regex.is_match(text), "should detect Anthropic key");
    }

    #[test]
    fn builtin_patterns_jwt_detected() {
        let text = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abc123";
        let pattern = &BUILTIN_PATTERNS[5]; // jwt
        assert!(pattern.regex.is_match(text), "should detect JWT");
    }

    #[test]
    fn builtin_patterns_private_key_detected() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIE...";
        let pattern = &BUILTIN_PATTERNS[6]; // private_key
        assert!(pattern.regex.is_match(text), "should detect private key");
    }

    #[test]
    fn builtin_patterns_database_url_detected() {
        let text = "database_url=postgres://user:pass@host:5432/db";
        let pattern = &BUILTIN_PATTERNS[7]; // database_url
        assert!(pattern.regex.is_match(text), "should detect database URL");
    }

    #[test]
    fn builtin_patterns_generic_api_key_detected() {
        let text = "api_key=abcdefgh12345678";
        let pattern = &BUILTIN_PATTERNS[8]; // generic_api_key
        assert!(
            pattern.regex.is_match(text),
            "should detect generic API key"
        );
    }

    #[test]
    fn builtin_patterns_safe_text_not_detected() {
        let safe_text = "This is a normal message about Rust programming.";
        for pattern in BUILTIN_PATTERNS.iter() {
            assert!(
                !pattern.regex.is_match(safe_text),
                "pattern {} should not match safe text",
                pattern.id,
            );
        }
    }

    // =========================================================================
    // Severity ranking tests
    // =========================================================================

    #[test]
    fn severity_rank_ordering() {
        assert!(SecretSeverity::Critical.rank() < SecretSeverity::High.rank());
        assert!(SecretSeverity::High.rank() < SecretSeverity::Medium.rank());
        assert!(SecretSeverity::Medium.rank() < SecretSeverity::Low.rank());
    }

    #[test]
    fn severity_label_values() {
        assert_eq!(SecretSeverity::Critical.label(), "critical");
        assert_eq!(SecretSeverity::High.label(), "high");
        assert_eq!(SecretSeverity::Medium.label(), "medium");
        assert_eq!(SecretSeverity::Low.label(), "low");
    }

    // =========================================================================
    // SecretLocation label tests
    // =========================================================================

    #[test]
    fn location_labels() {
        assert_eq!(
            SecretLocation::ConversationTitle.label(),
            "conversation.title"
        );
        assert_eq!(
            SecretLocation::ConversationMetadata.label(),
            "conversation.metadata"
        );
        assert_eq!(SecretLocation::MessageContent.label(), "message.content");
        assert_eq!(SecretLocation::MessageMetadata.label(), "message.metadata");
    }

    // =========================================================================
    // Build where clause tests
    // =========================================================================

    #[test]
    fn build_where_clause_empty_filters() {
        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let (clause, params) = build_where_clause(&filters).unwrap();
        assert!(clause.is_empty(), "empty filters should give empty clause");
        assert!(params.is_empty());
    }

    #[test]
    fn build_where_clause_with_agent_filter() {
        let filters = SecretScanFilters {
            agents: Some(vec!["claude".to_string(), "codex".to_string()]),
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let (clause, params) = build_where_clause(&filters).unwrap();
        assert!(clause.contains("a.slug IN"), "clause: {}", clause);
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn build_where_clause_with_time_range() {
        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: Some(1000),
            until_ts: Some(2000),
        };
        let (clause, params) = build_where_clause(&filters).unwrap();
        assert!(clause.contains("c.started_at >="), "clause: {}", clause);
        assert!(clause.contains("c.started_at <="), "clause: {}", clause);
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn build_where_clause_with_workspace_filter() {
        let filters = SecretScanFilters {
            agents: None,
            workspaces: Some(vec![PathBuf::from("/home/user/project")]),
            since_ts: None,
            until_ts: None,
        };
        let (clause, params) = build_where_clause(&filters).unwrap();
        assert!(clause.contains("w.path IN"), "clause: {}", clause);
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn build_where_clause_empty_agent_list_ignored() {
        let filters = SecretScanFilters {
            agents: Some(vec![]),
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let (clause, _) = build_where_clause(&filters).unwrap();
        assert!(clause.is_empty(), "empty agent list should be ignored");
    }

    // =========================================================================
    // Entropy regex tests
    // =========================================================================

    #[test]
    fn entropy_base64_regex_matches_long_strings() {
        assert!(ENTROPY_BASE64_RE.is_match("ABCDEFGHIJKLMNOPQRSTuv"));
        assert!(!ENTROPY_BASE64_RE.is_match("short"));
    }

    #[test]
    fn entropy_hex_regex_matches_32_plus_chars() {
        assert!(ENTROPY_HEX_RE.is_match("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"));
        assert!(!ENTROPY_HEX_RE.is_match("a1b2c3d4"));
    }

    // =========================================================================
    // Edge case tests — malformed input robustness (br-ig84)
    // =========================================================================

    #[test]
    fn scan_text_empty_text_no_findings() {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: None,
            conversation_id: None,
            message_id: None,
            message_idx: None,
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;

        scan_text(
            "",
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );
        assert!(findings.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn scan_text_already_truncated_skips() {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: None,
            conversation_id: None,
            message_id: None,
            message_idx: None,
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = true; // pre-set

        scan_text(
            "sk-test1234567890abcdefghijklmnopqr",
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );
        assert!(findings.is_empty(), "should skip when already truncated");
    }

    #[test]
    fn scan_text_denylist_always_critical() {
        let config =
            SecretScanConfig::from_inputs_with_env(&[], &["FORBIDDEN_TOKEN_.*".to_string()], false)
                .unwrap();
        let ctx = ScanContext {
            agent: Some("test".to_string()),
            workspace: None,
            source_path: None,
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;

        scan_text(
            "prefix FORBIDDEN_TOKEN_abc suffix",
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecretSeverity::Critical);
        assert_eq!(findings[0].kind, "denylist");
    }

    #[test]
    fn scan_text_allowlist_suppresses_builtin_match() {
        let config =
            SecretScanConfig::from_inputs_with_env(&["sk-test.*".to_string()], &[], false).unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: None,
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;

        scan_text(
            "sk-testABCDEFGHIJKLMNOPQRSTUVWXYZ12345",
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        // The openai_key pattern should match but be suppressed by allowlist
        assert!(
            !findings.iter().any(|f| f.kind == "openai_key"),
            "allowlisted key should be suppressed"
        );
    }

    #[test]
    fn scan_text_deduplicates_findings() {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: None,
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;

        // Scan same text twice — same context, so duplicates should be skipped
        let text = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ123456789";
        scan_text(
            text,
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );
        let count_after_first = findings.len();

        scan_text(
            text,
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );
        assert_eq!(
            findings.len(),
            count_after_first,
            "duplicate findings should be skipped"
        );
    }

    #[test]
    fn scan_text_max_findings_truncates() {
        // Use longer tokens (>8 chars) so each gets a unique redacted form for dedup
        let mut config =
            SecretScanConfig::from_inputs_with_env(&[], &["LONG_SECRET_\\d+".to_string()], false)
                .unwrap();
        config.max_findings = 3;

        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: None,
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;

        // Each match is >8 chars so redact_token produces unique output per token
        let text =
            "LONG_SECRET_001 LONG_SECRET_002 LONG_SECRET_003 LONG_SECRET_004 LONG_SECRET_005";
        scan_text(
            text,
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        assert!(
            findings.len() <= 3,
            "should cap at max_findings: {}",
            findings.len()
        );
        assert!(truncated, "should set truncated flag");
    }

    #[test]
    fn scan_text_pure_alphabetic_base64_skipped() {
        // Pure alphabetic strings (CamelCase identifiers) should NOT trigger entropy detection
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: None,
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = HashSet::new();
        let mut truncated = false;

        // This is a pure alphabetic string — should be skipped by the heuristic
        let text = "SecretScanConfigFromInputsWithEnvTest";
        scan_text(
            text,
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        assert!(
            !findings.iter().any(|f| f.kind == "high_entropy_base64"),
            "pure alphabetic strings should not trigger entropy detection"
        );
    }
}
