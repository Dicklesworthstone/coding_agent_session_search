use crate::franken_sync::compat::{ConnectionExt, ParamValue, RowExt, params_from_iter};
use crate::franken_sync::params;
use crate::indexer::redact_secrets::{
    ANTHROPIC_API_KEY_PATTERN, AWS_ACCESS_KEY_PATTERN, AWS_SECRET_KEY_PATTERN,
    AWS_SESSION_TOKEN_PATTERN, BEARER_TOKEN_PATTERN, DATABASE_URL_PATTERN,
    GENERIC_SECRET_ASSIGNMENT_PATTERN, GITHUB_TOKEN_PATTERN, JWT_PATTERN, OPENAI_API_KEY_PATTERN,
    PRIVATE_KEY_BLOCK_PATTERN, SLACK_TOKEN_PATTERN, STRIPE_KEY_PATTERN,
};
use anyhow::{Context, Result, bail};
use console::{Term, style};
use indicatif::{ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const DEFAULT_ENTROPY_THRESHOLD: f64 = 4.0;
const DEFAULT_ENTROPY_MIN_LEN: usize = 20;
const DEFAULT_CONTEXT_BYTES: usize = 120;
const DEFAULT_MAX_FINDINGS: usize = 500;
const SCAN_PAGE_ROWS: usize = 128;
const REDACTED_CONTEXT: &str = "[redacted]";
const REDACTED_METADATA_CONTEXT: &str = "structured metadata: [redacted]";
const CUSTOM_DENYLIST_PATTERN_ID: &str = "custom_denylist";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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
    MessageSnippet,
}

impl SecretLocation {
    fn label(&self) -> &'static str {
        match self {
            SecretLocation::ConversationTitle => "conversation.title",
            SecretLocation::ConversationMetadata => "conversation.metadata",
            SecretLocation::MessageContent => "message.content",
            SecretLocation::MessageMetadata => "message.metadata",
            SecretLocation::MessageSnippet => "message.snippet",
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
    pub by_severity: BTreeMap<SecretSeverity, usize>,
    pub has_critical: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SecretScanReport {
    pub summary: SecretScanSummary,
    pub findings: Vec<SecretFinding>,
}

/// Proof that a complete scan observed one immutable staged export artifact.
#[derive(Debug, Clone)]
pub struct StagedSecretScan {
    pub report: SecretScanReport,
    pub artifact_sha256: String,
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
            regex: Regex::new(AWS_ACCESS_KEY_PATTERN).expect("aws access key regex"),
        },
        SecretPattern {
            id: "aws_secret_key",
            severity: SecretSeverity::Critical,
            regex: Regex::new(AWS_SECRET_KEY_PATTERN).expect("aws secret regex"),
        },
        SecretPattern {
            id: "github_pat",
            severity: SecretSeverity::High,
            regex: Regex::new(GITHUB_TOKEN_PATTERN).expect("github token regex"),
        },
        SecretPattern {
            id: "openai_key",
            severity: SecretSeverity::High,
            regex: Regex::new(OPENAI_API_KEY_PATTERN).expect("openai key regex"),
        },
        SecretPattern {
            id: "anthropic_key",
            severity: SecretSeverity::High,
            regex: Regex::new(ANTHROPIC_API_KEY_PATTERN).expect("anthropic key regex"),
        },
        SecretPattern {
            id: "jwt",
            severity: SecretSeverity::Medium,
            regex: Regex::new(JWT_PATTERN).expect("jwt regex"),
        },
        SecretPattern {
            id: "private_key",
            severity: SecretSeverity::Critical,
            regex: Regex::new(PRIVATE_KEY_BLOCK_PATTERN).expect("private key regex"),
        },
        SecretPattern {
            id: "database_url",
            severity: SecretSeverity::Medium,
            regex: Regex::new(DATABASE_URL_PATTERN).expect("database URL regex"),
        },
        SecretPattern {
            id: "generic_api_key",
            severity: SecretSeverity::Low,
            regex: Regex::new(GENERIC_SECRET_ASSIGNMENT_PATTERN)
                .expect("generic secret assignment regex"),
        },
        SecretPattern {
            id: "aws_session_token",
            severity: SecretSeverity::Critical,
            regex: Regex::new(AWS_SESSION_TOKEN_PATTERN).expect("aws session token regex"),
        },
        SecretPattern {
            id: "bearer_token",
            severity: SecretSeverity::High,
            regex: Regex::new(BEARER_TOKEN_PATTERN).expect("bearer token regex"),
        },
        SecretPattern {
            id: "slack_token",
            severity: SecretSeverity::High,
            regex: Regex::new(SLACK_TOKEN_PATTERN).expect("slack token regex"),
        },
        SecretPattern {
            id: "stripe_key",
            severity: SecretSeverity::High,
            regex: Regex::new(STRIPE_KEY_PATTERN).expect("stripe key regex"),
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

struct StructuredMetadataScan {
    text: String,
    value: Option<serde_json::Value>,
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

#[derive(Debug, Clone, Copy)]
struct RedactionRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct FindingOccurrence {
    source_path: Option<String>,
    conversation_id: Option<i64>,
    message_id: Option<i64>,
    message_idx: Option<i64>,
    location: SecretLocation,
    start: usize,
    end: usize,
    finding_index: usize,
}

impl FindingOccurrence {
    fn overlaps(&self, candidate: &FindingCandidate<'_>) -> bool {
        self.source_path == candidate.ctx.source_path
            && self.conversation_id == candidate.ctx.conversation_id
            && self.message_id == candidate.ctx.message_id
            && self.message_idx == candidate.ctx.message_idx
            && self.location == candidate.location
            && self.start < candidate.end
            && candidate.start < self.end
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretScanCheckpoint {
    BeforeOpen,
    AfterOpen,
    BeforeConversationHighWatermark,
    BeforeConversationPage,
    ConversationRow,
    AfterConversations,
    BeforeMessageHighWatermark,
    BeforeMessagePage,
    MessageRow,
    AfterMessages,
    BeforeSnippetHighWatermark,
    BeforeSnippetPage,
    SnippetRow,
    AfterSnippets,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretScanSchema {
    Archive,
    PagesExport,
}

impl SecretScanSchema {
    fn detect(conn: &crate::franken_sync::Connection) -> Result<Self> {
        let has_export_agent = table_has_column(conn, "conversations", "agent")?;
        let has_export_workspace = table_has_column(conn, "conversations", "workspace")?;
        if has_export_agent && has_export_workspace {
            return Ok(Self::PagesExport);
        }

        let has_archive_agent = table_has_column(conn, "conversations", "agent_id")?;
        let has_archive_workspace =
            table_has_column(conn, "conversations", "workspace_id")?;
        if has_archive_agent && has_archive_workspace {
            return Ok(Self::Archive);
        }

        bail!(
            "Unsupported secret-scan database schema: conversations must contain either agent/workspace export columns or agent_id/workspace_id archive columns"
        )
    }

    fn agent_expression(self) -> &'static str {
        match self {
            Self::Archive => "COALESCE(a.slug, 'unknown')",
            Self::PagesExport => "COALESCE(c.agent, 'unknown')",
        }
    }

    fn workspace_expression(self) -> &'static str {
        match self {
            Self::Archive => "w.path",
            Self::PagesExport => "c.workspace",
        }
    }

    fn conversation_joins(self) -> &'static str {
        match self {
            Self::Archive => {
                "\n             LEFT JOIN agents a ON c.agent_id = a.id\n             LEFT JOIN workspaces w ON c.workspace_id = w.id"
            }
            Self::PagesExport => "",
        }
    }
}

fn ensure_secret_scan_running(
    cancellation_requested: &mut impl FnMut(SecretScanCheckpoint) -> bool,
    checkpoint: SecretScanCheckpoint,
) -> Result<()> {
    if cancellation_requested(checkpoint) {
        bail!("Secret scan cancelled before completion");
    }
    Ok(())
}

pub fn scan_database<P: AsRef<Path>>(
    db_path: P,
    filters: &SecretScanFilters,
    config: &SecretScanConfig,
    running: Option<Arc<AtomicBool>>,
    progress: Option<&ProgressBar>,
) -> Result<SecretScanReport> {
    scan_database_with_cancel_check(db_path, filters, config, progress, |_| {
        running
            .as_ref()
            .is_some_and(|flag| !flag.load(Ordering::Relaxed))
    })
}

/// Scan an immutable Pages export and bind the report to its exact bytes.
///
/// The digest is checked on both sides of the scan. Any concurrent mutation or
/// a finding-cap truncation is an error, never an approvable/clean result.
pub fn scan_staged_export_database<P: AsRef<Path>>(
    db_path: P,
    config: &SecretScanConfig,
) -> Result<StagedSecretScan> {
    let db_path = db_path.as_ref();
    let digest_before = sha256_file(db_path)?;
    let filters = SecretScanFilters {
        agents: None,
        workspaces: None,
        since_ts: None,
        until_ts: None,
    };
    let report = scan_database(db_path, &filters, config, None, None)?;
    let digest_after = sha256_file(db_path)?;

    if digest_before != digest_after {
        bail!(
            "Staged Pages export changed while its secret scan was running; refusing approval"
        );
    }
    if report.summary.truncated {
        bail!(
            "Staged Pages export secret scan reached its finding cap; refusing incomplete approval"
        );
    }

    Ok(StagedSecretScan {
        report,
        artifact_sha256: digest_after,
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open staged export {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to hash staged export {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn scan_database_with_cancel_check<P: AsRef<Path>>(
    db_path: P,
    filters: &SecretScanFilters,
    config: &SecretScanConfig,
    progress: Option<&ProgressBar>,
    mut cancellation_requested: impl FnMut(SecretScanCheckpoint) -> bool,
) -> Result<SecretScanReport> {
    // Cancellation must never be represented as a complete (and potentially
    // false-clean) report. Check before opening the database as well as at
    // every query/row boundary below, and return a distinct error on abort.
    ensure_secret_scan_running(
        &mut cancellation_requested,
        SecretScanCheckpoint::BeforeOpen,
    )?;
    let conn = super::open_existing_sqlite_db(db_path.as_ref())
        .context("Failed to open database for secret scan")?;
    ensure_secret_scan_running(&mut cancellation_requested, SecretScanCheckpoint::AfterOpen)?;
    let schema = SecretScanSchema::detect(&conn)?;

    let mut findings: Vec<SecretFinding> = Vec::new();
    let mut seen = Vec::new();
    let mut truncated = false;

    // LEFT JOIN + COALESCE on agents so secret scanning also covers legacy
    // conversations with NULL agent_id — dropping them would hide credential
    // leaks rather than exposing them.
    let has_metadata_bin = table_has_column(&conn, "conversations", "metadata_bin")?;
    let metadata_bin_projection = if has_metadata_bin {
        "c.metadata_bin"
    } else {
        "NULL"
    };
    let (conv_where, conv_params) = build_where_clause_for_columns(
        filters,
        schema.agent_expression(),
        schema.workspace_expression(),
    )?;
    ensure_secret_scan_running(
        &mut cancellation_requested,
        SecretScanCheckpoint::BeforeConversationHighWatermark,
    )?;
    let conv_high_watermark = table_max_id(&conn, "conversations")?;
    let conv_select = format!(
        "SELECT c.id, c.title, c.metadata_json, c.source_path, {}, {}, {metadata_bin_projection}\n         FROM conversations c{}",
        schema.agent_expression(),
        schema.workspace_expression(),
        schema.conversation_joins(),
    );
    let mut last_conv_id = None;
    while !truncated {
        ensure_secret_scan_running(
            &mut cancellation_requested,
            SecretScanCheckpoint::BeforeConversationPage,
        )?;
        let page_where = bounded_keyset_page_where(&conv_where, "c.id", last_conv_id);
        let conv_sql = format!(
            "{conv_select}{page_where} ORDER BY c.id LIMIT {SCAN_PAGE_ROWS}"
        );
        let mut page_params = conv_params.clone();
        page_params.push(ParamValue::from(conv_high_watermark));
        if let Some(last_id) = last_conv_id {
            page_params.push(ParamValue::from(last_id));
        }
        let page_param_values = params_from_iter(page_params);
        let conv_rows = conn.query_with_params(&conv_sql, &page_param_values)?;
        if conv_rows.is_empty() {
            break;
        }
        let page_len = conv_rows.len();

        for row in &conv_rows {
            ensure_secret_scan_running(
                &mut cancellation_requested,
                SecretScanCheckpoint::ConversationRow,
            )?;
            let conv_id: i64 = row.get_typed(0)?;
            let title: Option<String> = row.get_typed(1)?;
            let metadata_json: Option<String> = row.get_typed(2)?;
            let source_path: String = row.get_typed(3)?;
            let agent_slug: String = row.get_typed(4)?;
            let workspace_path: Option<String> = row.get_typed(5)?;
            let metadata_bin: Option<Vec<u8>> = row.get_typed(6)?;
            last_conv_id = Some(conv_id);

            let ctx = ScanContext {
                agent: Some(agent_slug),
                workspace: workspace_path,
                source_path: Some(source_path),
                conversation_id: Some(conv_id),
                message_id: None,
                message_idx: None,
            };

            // These provenance fields are part of the exported database too.
            // Merely redacting them in the diagnostic report would otherwise
            // allow a credential-bearing path/workspace to yield a false-clean
            // approval while the original value remains in the payload.
            for provenance in [
                ctx.agent.as_deref(),
                ctx.workspace.as_deref(),
                ctx.source_path.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                scan_text(
                    provenance,
                    SecretLocation::ConversationMetadata,
                    &ctx,
                    config,
                    &mut findings,
                    &mut seen,
                    &mut truncated,
                );
            }

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
            if let Some(meta) = structured_metadata_scan_text(
                metadata_bin.as_deref(),
                metadata_json.as_deref(),
                "conversations.metadata_bin",
                "conversations.metadata_json",
                conv_id,
            )? {
                let first_metadata_finding = findings.len();
                scan_text(
                    &meta.text,
                    SecretLocation::ConversationMetadata,
                    &ctx,
                    config,
                    &mut findings,
                    &mut seen,
                    &mut truncated,
                );
                redact_structured_metadata_contexts(&mut findings[first_metadata_finding..]);
                if let Some(value) = meta.value.as_ref() {
                    scan_sensitive_json_fields(
                        value,
                        SecretLocation::ConversationMetadata,
                        &ctx,
                        config,
                        &mut findings,
                        &mut truncated,
                    );
                }
            }

            if truncated {
                break;
            }

            if let Some(pb) = progress {
                pb.inc(1);
            }
        }

        if truncated || page_len < SCAN_PAGE_ROWS {
            break;
        }
    }
    ensure_secret_scan_running(
        &mut cancellation_requested,
        SecretScanCheckpoint::AfterConversations,
    )?;

    if !truncated {
        let has_extra_json = table_has_column(&conn, "messages", "extra_json")?;
        let extra_json_projection = if has_extra_json { "m.extra_json" } else { "NULL" };
        let has_extra_bin = table_has_column(&conn, "messages", "extra_bin")?;
        let extra_bin_projection = if has_extra_bin { "m.extra_bin" } else { "NULL" };
        let has_attachment_refs =
            table_has_column(&conn, "messages", "attachment_refs")?;
        let attachment_refs_projection =
            if schema == SecretScanSchema::PagesExport && has_attachment_refs {
                "m.attachment_refs"
            } else {
                "NULL"
            };
        let has_model = table_has_column(&conn, "messages", "model")?;
        let model_projection = if has_model { "m.model" } else { "NULL" };
        let (msg_where, msg_params) = build_where_clause_for_columns(
            filters,
            schema.agent_expression(),
            schema.workspace_expression(),
        )?;
        ensure_secret_scan_running(
            &mut cancellation_requested,
            SecretScanCheckpoint::BeforeMessageHighWatermark,
        )?;
        let msg_high_watermark = table_max_id(&conn, "messages")?;
        let msg_select = format!(
            "SELECT m.id, m.idx, m.content, {extra_json_projection}, c.id, c.source_path, {}, {}, {extra_bin_projection}, {attachment_refs_projection}, m.role, {model_projection}\n             FROM messages m\n             JOIN conversations c ON m.conversation_id = c.id{}",
            schema.agent_expression(),
            schema.workspace_expression(),
            schema.conversation_joins(),
        );
        let mut last_msg_id = None;
        while !truncated {
            ensure_secret_scan_running(
                &mut cancellation_requested,
                SecretScanCheckpoint::BeforeMessagePage,
            )?;
            let page_where = bounded_keyset_page_where(&msg_where, "m.id", last_msg_id);
            let msg_sql =
                format!("{msg_select}{page_where} ORDER BY m.id LIMIT {SCAN_PAGE_ROWS}");
            let mut page_params = msg_params.clone();
            page_params.push(ParamValue::from(msg_high_watermark));
            if let Some(last_id) = last_msg_id {
                page_params.push(ParamValue::from(last_id));
            }
            let page_param_values = params_from_iter(page_params);
            let msg_rows = conn.query_with_params(&msg_sql, &page_param_values)?;
            if msg_rows.is_empty() {
                break;
            }
            let page_len = msg_rows.len();

            for row in &msg_rows {
                ensure_secret_scan_running(
                    &mut cancellation_requested,
                    SecretScanCheckpoint::MessageRow,
                )?;
                let msg_id: i64 = row.get_typed(0)?;
                let msg_idx: i64 = row.get_typed(1)?;
                let content: String = row.get_typed(2)?;
                let extra_json: Option<String> = row.get_typed(3)?;
                let conv_id: i64 = row.get_typed(4)?;
                let source_path: String = row.get_typed(5)?;
                let agent_slug: String = row.get_typed(6)?;
                let workspace_path: Option<String> = row.get_typed(7)?;
                let extra_bin: Option<Vec<u8>> = row.get_typed(8)?;
                let attachment_refs: Option<String> = row.get_typed(9)?;
                let role: String = row.get_typed(10)?;
                let model: Option<String> = row.get_typed(11)?;
                last_msg_id = Some(msg_id);

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
                if let Some(attachment_refs) = attachment_refs.as_deref() {
                    scan_text(
                        attachment_refs,
                        SecretLocation::MessageMetadata,
                        &ctx,
                        config,
                        &mut findings,
                        &mut seen,
                        &mut truncated,
                    );
                }
                for metadata_text in [Some(role.as_str()), model.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    scan_text(
                        metadata_text,
                        SecretLocation::MessageMetadata,
                        &ctx,
                        config,
                        &mut findings,
                        &mut seen,
                        &mut truncated,
                    );
                }
                if let Some(extra) = structured_metadata_scan_text(
                    extra_bin.as_deref(),
                    extra_json.as_deref(),
                    "messages.extra_bin",
                    "messages.extra_json",
                    msg_id,
                )? {
                    let first_metadata_finding = findings.len();
                    scan_text(
                        &extra.text,
                        SecretLocation::MessageMetadata,
                        &ctx,
                        config,
                        &mut findings,
                        &mut seen,
                        &mut truncated,
                    );
                    redact_structured_metadata_contexts(&mut findings[first_metadata_finding..]);
                    if let Some(value) = extra.value.as_ref() {
                        scan_sensitive_json_fields(
                            value,
                            SecretLocation::MessageMetadata,
                            &ctx,
                            config,
                            &mut findings,
                            &mut truncated,
                        );
                    }
                }

                if truncated {
                    break;
                }

                if let Some(pb) = progress {
                    pb.inc(1);
                }
            }

            if truncated || page_len < SCAN_PAGE_ROWS {
                break;
            }
        }
    }
    ensure_secret_scan_running(
        &mut cancellation_requested,
        SecretScanCheckpoint::AfterMessages,
    )?;

    if !truncated && table_exists(&conn, "snippets")? {
        let (snip_where, snip_params) = build_where_clause_for_columns(
            filters,
            schema.agent_expression(),
            schema.workspace_expression(),
        )?;
        ensure_secret_scan_running(
            &mut cancellation_requested,
            SecretScanCheckpoint::BeforeSnippetHighWatermark,
        )?;
        let snip_high_watermark = table_max_id(&conn, "snippets")?;
        let snip_select = format!(
            "SELECT s.id, s.snippet_text, m.id, m.idx, c.id, c.source_path, {}, {}, s.file_path, s.language\n             FROM snippets s\n             JOIN messages m ON s.message_id = m.id\n             JOIN conversations c ON m.conversation_id = c.id{}",
            schema.agent_expression(),
            schema.workspace_expression(),
            schema.conversation_joins(),
        );
        let mut last_snippet_id = None;
        while !truncated {
            ensure_secret_scan_running(
                &mut cancellation_requested,
                SecretScanCheckpoint::BeforeSnippetPage,
            )?;
            let page_where = bounded_keyset_page_where(&snip_where, "s.id", last_snippet_id);
            let snip_sql =
                format!("{snip_select}{page_where} ORDER BY s.id LIMIT {SCAN_PAGE_ROWS}");
            let mut page_params = snip_params.clone();
            page_params.push(ParamValue::from(snip_high_watermark));
            if let Some(last_id) = last_snippet_id {
                page_params.push(ParamValue::from(last_id));
            }
            let page_param_values = params_from_iter(page_params);
            let snip_rows = conn.query_with_params(&snip_sql, &page_param_values)?;
            if snip_rows.is_empty() {
                break;
            }
            let page_len = snip_rows.len();

            for row in &snip_rows {
                ensure_secret_scan_running(
                    &mut cancellation_requested,
                    SecretScanCheckpoint::SnippetRow,
                )?;
                let snippet_id: i64 = row.get_typed(0)?;
                let snippet_text: String = row.get_typed(1)?;
                let msg_id: i64 = row.get_typed(2)?;
                let msg_idx: i64 = row.get_typed(3)?;
                let conv_id: i64 = row.get_typed(4)?;
                let source_path: String = row.get_typed(5)?;
                let agent_slug: String = row.get_typed(6)?;
                let workspace_path: Option<String> = row.get_typed(7)?;
                let snippet_file_path: Option<String> = row.get_typed(8)?;
                let snippet_language: Option<String> = row.get_typed(9)?;
                last_snippet_id = Some(snippet_id);

                let ctx = ScanContext {
                    agent: Some(agent_slug),
                    workspace: workspace_path,
                    source_path: Some(source_path),
                    conversation_id: Some(conv_id),
                    message_id: Some(msg_id),
                    message_idx: Some(msg_idx),
                };

                scan_text(
                    &snippet_text,
                    SecretLocation::MessageSnippet,
                    &ctx,
                    config,
                    &mut findings,
                    &mut seen,
                    &mut truncated,
                );
                if let Some(snippet_file_path) = snippet_file_path.as_deref() {
                    scan_text(
                        snippet_file_path,
                        SecretLocation::MessageMetadata,
                        &ctx,
                        config,
                        &mut findings,
                        &mut seen,
                        &mut truncated,
                    );
                }
                if let Some(snippet_language) = snippet_language.as_deref() {
                    scan_text(
                        snippet_language,
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

            if truncated || page_len < SCAN_PAGE_ROWS {
                break;
            }
        }
    }
    ensure_secret_scan_running(
        &mut cancellation_requested,
        SecretScanCheckpoint::AfterSnippets,
    )?;

    findings.sort_by(|a, b| {
        a.severity
            .rank()
            .cmp(&b.severity.rank())
            .then_with(|| a.kind.cmp(&b.kind))
    });

    let mut by_severity: BTreeMap<SecretSeverity, usize> = BTreeMap::new();
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

fn table_exists(conn: &crate::franken_sync::Connection, table_name: &str) -> Result<bool> {
    if !table_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!("Invalid SQLite identifier while inspecting secret-scan schema");
    }

    let pragma = format!("PRAGMA table_info({table_name})");
    conn.query_map_collect(&pragma, params![], |row| row.get_typed::<String>(1))
        .map(|columns| !columns.is_empty())
        .with_context(|| format!("Failed to inspect {table_name} schema for secret scan"))
}

fn table_max_id(
    conn: &crate::franken_sync::Connection,
    table_name: &str,
) -> Result<Option<i64>> {
    if !table_name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!("Invalid SQLite identifier while paging secret-scan rows");
    }

    let sql = format!("SELECT MAX(id) FROM {table_name}");
    conn.query_row_map(&sql, params![], |row| row.get_typed(0))
        .with_context(|| format!("Failed to bound {table_name} secret-scan rows"))
}

fn table_has_column(
    conn: &crate::franken_sync::Connection,
    table_name: &str,
    column_name: &str,
) -> Result<bool> {
    if !table_name
        .chars()
        .chain(column_name.chars())
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!("Invalid SQLite identifier while inspecting secret-scan schema");
    }

    let pragma = format!("PRAGMA table_info({table_name})");
    let columns = conn
        .query_map_collect(&pragma, params![], |row| row.get_typed::<String>(1))
        .with_context(|| format!("Failed to inspect {table_name} schema for secret scan"))?;
    Ok(columns.iter().any(|column| column == column_name))
}

/// Select the authoritative structured metadata representation for scanning.
///
/// New archive rows store MessagePack in `*_bin`; legacy rows store JSON text.
/// A non-empty binary value is authoritative and must never silently fall back
/// to a stale JSON shadow. In particular, malformed binary is an integrity
/// error: falling back could hide secrets that exist only in the canonical
/// binary payload.
fn structured_metadata_scan_text(
    binary: Option<&[u8]>,
    legacy_json: Option<&str>,
    binary_column: &str,
    legacy_column: &str,
    row_id: i64,
) -> Result<Option<StructuredMetadataScan>> {
    if let Some(bytes) = binary.filter(|bytes| !bytes.is_empty()) {
        let mut deserializer = rmp_serde::Deserializer::new(Cursor::new(bytes));
        let value = serde_json::Value::deserialize(&mut deserializer).with_context(|| {
            format!(
                "Failed to decode non-empty {binary_column} MessagePack for row {row_id}; refusing legacy JSON fallback"
            )
        })?;
        let consumed = usize::try_from(deserializer.get_ref().position()).with_context(|| {
            format!("Decoded {binary_column} position does not fit usize for row {row_id}")
        })?;
        if consumed != bytes.len() {
            bail!(
                "Non-empty {binary_column} for row {row_id} contains trailing bytes after its MessagePack value; refusing legacy JSON fallback"
            );
        }
        return serde_json::to_string(&value)
            .with_context(|| {
                format!("Failed to serialize decoded {binary_column} value for row {row_id}")
            })
            .map(|text| Some(StructuredMetadataScan {
                text,
                value: Some(value),
            }));
    }

    let Some(text) = legacy_json.filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    let value = serde_json::from_str(text).with_context(|| {
        format!(
            "Failed to decode non-empty {legacy_column} JSON for row {row_id}; refusing a text-only scan that could miss credential-bearing fields"
        )
    })?;
    Ok(Some(StructuredMetadataScan {
        text: text.to_owned(),
        value: Some(value),
    }))
}

fn redact_structured_metadata_contexts(findings: &mut [SecretFinding]) {
    for finding in findings {
        finding.context = REDACTED_METADATA_CONTEXT.to_string();
    }
}

/// Report credential-bearing structured fields that text patterns cannot see.
///
/// The ingestion redactor treats field names such as `pin`, `cookie`, and
/// `private_key` as authoritative even when their values are too short to
/// satisfy a token regex. The post-hoc scanner must apply the same structural
/// floor or it can return a false-clean report for historical rows. A field
/// already covered by an equal-or-higher-severity text detector is skipped so
/// the structural floor does not inflate an equivalent existing finding. A
/// lower-severity heuristic must not suppress this high-severity floor.
fn scan_sensitive_json_fields(
    value: &serde_json::Value,
    location: SecretLocation,
    ctx: &ScanContext,
    config: &SecretScanConfig,
    findings: &mut Vec<SecretFinding>,
    truncated: &mut bool,
) {
    if *truncated {
        return;
    }

    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                scan_sensitive_json_fields(
                    value,
                    location.clone(),
                    ctx,
                    config,
                    findings,
                    truncated,
                );
                if *truncated {
                    return;
                }
            }
        }
        serde_json::Value::Object(fields) => {
            for (field, field_value) in fields {
                if crate::indexer::redact_secrets::is_sensitive_json_field(field) {
                    if !structured_value_contains_material(field_value)
                        || structured_value_is_fully_allowlisted(field_value, config)
                        || structured_field_has_equal_or_higher_text_detector(
                            field,
                            field_value,
                            config,
                            SecretSeverity::High,
                        )
                    {
                        continue;
                    }
                    if findings.len() >= config.max_findings {
                        *truncated = true;
                        return;
                    }
                    findings.push(SecretFinding {
                        severity: SecretSeverity::High,
                        kind: "sensitive_metadata_field".to_string(),
                        pattern: "sensitive_json_field".to_string(),
                        match_redacted: REDACTED_CONTEXT.to_string(),
                        context: REDACTED_METADATA_CONTEXT.to_string(),
                        location: location.clone(),
                        agent: redact_report_provenance(&ctx.agent, config),
                        workspace: redact_report_provenance(&ctx.workspace, config),
                        source_path: redact_report_provenance(&ctx.source_path, config),
                        conversation_id: ctx.conversation_id,
                        message_id: ctx.message_id,
                        message_idx: ctx.message_idx,
                    });
                } else {
                    scan_sensitive_json_fields(
                        field_value,
                        location.clone(),
                        ctx,
                        config,
                        findings,
                        truncated,
                    );
                    if *truncated {
                        return;
                    }
                }
            }
        }
        _ => {}
    }
}

fn structured_value_contains_material(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(REDACTED_CONTEXT)
        }
        serde_json::Value::Array(values) => {
            values.iter().any(structured_value_contains_material)
        }
        serde_json::Value::Object(fields) => {
            fields.values().any(structured_value_contains_material)
        }
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
    }
}

fn structured_value_is_fully_allowlisted(
    value: &serde_json::Value,
    config: &SecretScanConfig,
) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(text) => {
            text.trim().is_empty()
                || text.trim().eq_ignore_ascii_case(REDACTED_CONTEXT)
                || is_fully_allowlisted(text.trim(), config)
        }
        serde_json::Value::Array(values) => values
            .iter()
            .filter(|value| structured_value_contains_material(value))
            .all(|value| structured_value_is_fully_allowlisted(value, config)),
        serde_json::Value::Object(fields) => fields
            .values()
            .filter(|value| structured_value_contains_material(value))
            .all(|value| structured_value_is_fully_allowlisted(value, config)),
        serde_json::Value::Bool(value) => is_fully_allowlisted(&value.to_string(), config),
        serde_json::Value::Number(value) => is_fully_allowlisted(&value.to_string(), config),
    }
}

fn structured_field_has_equal_or_higher_text_detector(
    field: &str,
    value: &serde_json::Value,
    config: &SecretScanConfig,
    structural_severity: SecretSeverity,
) -> bool {
    let Ok(field_text) = serde_json::to_string(field) else {
        return false;
    };
    let Ok(value_text) = serde_json::to_string(value) else {
        return false;
    };
    let mut text = String::with_capacity(
        field_text
            .len()
            .saturating_add(value_text.len())
            .saturating_add(3),
    );
    text.push('{');
    text.push_str(&field_text);
    text.push(':');
    text.push_str(&value_text);
    text.push('}');

    if SecretSeverity::Critical.rank() <= structural_severity.rank()
        && config
            .denylist
            .iter()
            .any(|pattern| pattern.is_match(&text))
    {
        return true;
    }
    BUILTIN_PATTERNS.iter().any(|pattern| {
        pattern.severity.rank() <= structural_severity.rank()
            && pattern
                .regex
                .find_iter(&text)
                .any(|matched| !is_allowlisted(matched.as_str(), config))
    })
}

pub fn print_human_report(
    term: &mut Term,
    report: &SecretScanReport,
    max_examples: usize,
) -> Result<()> {
    write_human_report(term, report, max_examples)
}

fn write_human_report(
    writer: &mut impl Write,
    report: &SecretScanReport,
    max_examples: usize,
) -> Result<()> {
    let total = report.summary.total;
    if total == 0 && !report.summary.truncated {
        writeln!(writer, "  {} No secrets detected", style("✓").green())?;
        return Ok(());
    }

    if total > 0 {
        writeln!(
            writer,
            "  {} {} potential secret(s) detected",
            style("⚠").yellow(),
            total
        )?;
    }

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
        writeln!(writer, "  {}: {}", label, count)?;

        for finding in report
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .take(max_examples)
        {
            writeln!(
                writer,
                "    - {} in {} ({})",
                finding.kind,
                finding.location.label(),
                finding.match_redacted
            )?;
            if !finding.context.is_empty() {
                writeln!(writer, "      {}", style(&finding.context).dim())?;
            }
        }
        if count > max_examples {
            writeln!(
                writer,
                "      {}",
                style("…additional findings hidden").dim()
            )?;
        }
    }

    if report.summary.truncated {
        writeln!(
            writer,
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
    seen: &mut Vec<FindingOccurrence>,
    truncated: &mut bool,
) {
    if *truncated || text.is_empty() {
        return;
    }

    // Denylist first (always critical)
    for deny in &config.denylist {
        for mat in deny.find_iter(text) {
            push_finding(
                findings,
                seen,
                FindingCandidate {
                    severity: SecretSeverity::Critical,
                    kind: "denylist",
                    pattern: CUSTOM_DENYLIST_PATTERN_ID,
                    text,
                    start: mat.start(),
                    end: mat.end(),
                    location: location.clone(),
                    ctx,
                },
                config,
                truncated,
            );
            if *truncated {
                return;
            }
        }
    }

    // Built-in patterns
    for pattern in BUILTIN_PATTERNS.iter() {
        for mat in pattern.regex.find_iter(text) {
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
                truncated,
            );
            if *truncated {
                return;
            }
        }
    }

    // Entropy-based detection
    for mat in ENTROPY_BASE64_RE.find_iter(text) {
        let candidate = &text[mat.start()..mat.end()];
        if is_base64_entropy_secret(candidate, config, true) {
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
                truncated,
            );
            if *truncated {
                return;
            }
        }
    }

    for mat in ENTROPY_HEX_RE.find_iter(text) {
        let candidate = &text[mat.start()..mat.end()];
        if is_hex_entropy_secret(candidate, config, true) {
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
                truncated,
            );
            if *truncated {
                return;
            }
        }
    }
}

fn push_finding(
    findings: &mut Vec<SecretFinding>,
    seen: &mut Vec<FindingOccurrence>,
    candidate: FindingCandidate<'_>,
    config: &SecretScanConfig,
    truncated: &mut bool,
) {
    if let Some(occurrence) = seen
        .iter_mut()
        .find(|occurrence| occurrence.overlaps(&candidate))
    {
        let Some(existing) = findings.get_mut(occurrence.finding_index) else {
            return;
        };
        if candidate.severity.rank() < existing.severity.rank() {
            existing.severity = candidate.severity;
            existing.kind = candidate.kind.to_string();
            existing.pattern = candidate.pattern.to_string();
            existing.match_redacted = redact_token(
                &candidate.text[candidate.start..candidate.end],
            );
            existing.context = redact_context(
                candidate.text,
                candidate.start,
                candidate.end,
                config.context_bytes,
                config,
            );
            occurrence.start = occurrence.start.min(candidate.start);
            occurrence.end = occurrence.end.max(candidate.end);
        }
        return;
    }

    if findings.len() >= config.max_findings {
        *truncated = true;
        return;
    }

    let match_text = &candidate.text[candidate.start..candidate.end];
    let match_redacted = redact_token(match_text);
    let context = redact_context(
        candidate.text,
        candidate.start,
        candidate.end,
        config.context_bytes,
        config,
    );
    let finding_index = findings.len();
    seen.push(FindingOccurrence {
        source_path: candidate.ctx.source_path.clone(),
        conversation_id: candidate.ctx.conversation_id,
        message_id: candidate.ctx.message_id,
        message_idx: candidate.ctx.message_idx,
        location: candidate.location.clone(),
        start: candidate.start,
        end: candidate.end,
        finding_index,
    });

    findings.push(SecretFinding {
        severity: candidate.severity,
        kind: candidate.kind.to_string(),
        pattern: candidate.pattern.to_string(),
        match_redacted,
        context,
        location: candidate.location,
        agent: redact_report_provenance(&candidate.ctx.agent, config),
        workspace: redact_report_provenance(&candidate.ctx.workspace, config),
        source_path: redact_report_provenance(&candidate.ctx.source_path, config),
        conversation_id: candidate.ctx.conversation_id,
        message_id: candidate.ctx.message_id,
        message_idx: candidate.ctx.message_idx,
    });
}

fn redact_token(_token: &str) -> String {
    REDACTED_CONTEXT.to_string()
}

fn redact_report_provenance(
    value: &Option<String>,
    config: &SecretScanConfig,
) -> Option<String> {
    value
        .as_deref()
        .map(|text| redact_report_slice(text, 0, text.len(), config, Vec::new()))
}

fn redact_context(
    text: &str,
    start: usize,
    end: usize,
    window: usize,
    config: &SecretScanConfig,
) -> String {
    if text.is_empty() || start >= end || start >= text.len() {
        return String::new();
    }

    let safe_start = adjust_to_char_boundary(text, start.min(text.len()), false);
    let safe_end = adjust_to_char_boundary(text, end.min(text.len()), true);
    if safe_start >= safe_end {
        return String::new();
    }

    let ctx_start = safe_start.saturating_sub(window / 2);
    let ctx_end = safe_end.saturating_add(window / 2).min(text.len());
    let ctx_start = adjust_to_char_boundary(text, ctx_start, false);
    let ctx_end = adjust_to_char_boundary(text, ctx_end, true);

    if ctx_start >= ctx_end {
        return String::new();
    }

    // Context is a report surface, so it has a stronger rule than finding
    // admission: every known secret span is masked, including allowlisted
    // spans and adjacent matches that are not the focal finding. Only ranges
    // intersecting this bounded window are retained in memory.
    let mut local_redactions = Vec::with_capacity(1);
    // The focal match must be masked even if a future finding source is not
    // represented in `collect_context_redactions` yet.
    local_redactions.push(RedactionRange {
        start: safe_start,
        end: safe_end,
    });
    redact_report_slice(text, ctx_start, ctx_end, config, local_redactions)
}

fn redact_report_slice(
    text: &str,
    slice_start: usize,
    slice_end: usize,
    config: &SecretScanConfig,
    mut redactions: Vec<RedactionRange>,
) -> String {
    redactions.extend(collect_context_redactions(
        text,
        config,
        slice_start,
        slice_end,
    ));
    let redactions = merge_redaction_ranges(redactions);

    let mut redacted = String::with_capacity(slice_end.saturating_sub(slice_start));
    let mut cursor = slice_start;
    for range in redactions {
        let range_start = range.start.max(slice_start);
        let range_end = range.end.min(slice_end);
        if range_start >= range_end {
            continue;
        }
        if cursor < range_start {
            redacted.push_str(&text[cursor..range_start]);
        }
        if cursor < range_end {
            redacted.push_str(REDACTED_CONTEXT);
            cursor = range_end;
        }
    }
    if cursor < slice_end {
        redacted.push_str(&text[cursor..slice_end]);
    }

    // Defense in depth: the shared ingestion redactor is the canonical
    // credential floor. The interval pass additionally covers custom
    // denylist and entropy matches, including secrets crossing slice edges.
    crate::indexer::redact_secrets::redact_text(&redacted).into_owned()
}

fn collect_context_redactions(
    text: &str,
    config: &SecretScanConfig,
    context_start: usize,
    context_end: usize,
) -> Vec<RedactionRange> {
    if context_start >= context_end || context_start >= text.len() {
        return Vec::new();
    }
    let mut ranges = Vec::new();

    for deny in &config.denylist {
        ranges.extend(
            deny.find_iter(text)
                .take_while(|mat| mat.start() < context_end)
                .filter(|mat| mat.start() < mat.end() && mat.end() > context_start)
                .map(|mat| RedactionRange {
                    start: mat.start(),
                    end: mat.end(),
                }),
        );
    }

    for pattern in BUILTIN_PATTERNS.iter() {
        ranges.extend(
            pattern
                .regex
                .find_iter(text)
                .take_while(|mat| mat.start() < context_end)
                .filter(|mat| mat.start() < mat.end() && mat.end() > context_start)
                .map(|mat| RedactionRange {
                    start: mat.start(),
                    end: mat.end(),
                }),
        );
    }

    ranges.extend(
        ENTROPY_BASE64_RE
            .find_iter(text)
            .take_while(|mat| mat.start() < context_end)
            .filter(|mat| mat.start() < mat.end() && mat.end() > context_start)
            .filter(|mat| is_base64_entropy_secret(mat.as_str(), config, false))
            .map(|mat| RedactionRange {
                start: mat.start(),
                end: mat.end(),
            }),
    );
    ranges.extend(
        ENTROPY_HEX_RE
            .find_iter(text)
            .take_while(|mat| mat.start() < context_end)
            .filter(|mat| mat.start() < mat.end() && mat.end() > context_start)
            .filter(|mat| is_hex_entropy_secret(mat.as_str(), config, false))
            .map(|mat| RedactionRange {
                start: mat.start(),
                end: mat.end(),
            }),
    );

    merge_redaction_ranges(ranges)
}

fn merge_redaction_ranges(mut ranges: Vec<RedactionRange>) -> Vec<RedactionRange> {
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<RedactionRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if range.start >= range.end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

fn is_base64_entropy_secret(
    candidate: &str,
    config: &SecretScanConfig,
    honor_allowlist: bool,
) -> bool {
    if candidate.len() < config.entropy_min_len
        || (honor_allowlist && is_allowlisted(candidate, config))
    {
        return false;
    }
    // Pure alphabetic strings are commonly code identifiers, not secrets.
    if candidate.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return false;
    }
    shannon_entropy(candidate) >= config.entropy_threshold
}

fn is_hex_entropy_secret(
    candidate: &str,
    config: &SecretScanConfig,
    honor_allowlist: bool,
) -> bool {
    candidate.len() >= 32
        && !(honor_allowlist && is_allowlisted(candidate, config))
        && shannon_entropy(candidate) >= 3.0
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

fn is_fully_allowlisted(value: &str, config: &SecretScanConfig) -> bool {
    config.allowlist.iter().any(|allow| {
        allow
            .find(value)
            .is_some_and(|matched| matched.start() == 0 && matched.end() == value.len())
    })
}

#[cfg(test)]
fn build_where_clause(filters: &SecretScanFilters) -> Result<(String, Vec<ParamValue>)> {
    build_where_clause_for_columns(filters, "COALESCE(a.slug, 'unknown')", "w.path")
}

fn build_where_clause_for_columns(
    filters: &SecretScanFilters,
    agent_expression: &str,
    workspace_expression: &str,
) -> Result<(String, Vec<ParamValue>)> {
    for expression in [agent_expression, workspace_expression] {
        if !expression.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '_' | '.' | '(' | ')' | ',' | '\'' | ' ')
        }) {
            bail!("Invalid SQLite expression while building secret-scan filters");
        }
    }

    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<ParamValue> = Vec::new();

    if let Some(agents) = filters.agents.as_ref() {
        if agents.is_empty() {
            conditions.push("1=0".to_string());
        } else {
            let placeholders: Vec<&str> = agents.iter().map(|_| "?").collect();
            conditions.push(format!(
                "{agent_expression} IN ({})",
                placeholders.join(", ")
            ));
            for agent in agents {
                params.push(ParamValue::from(agent.as_str()));
            }
        }
    }

    if let Some(workspaces) = filters.workspaces.as_ref() {
        if workspaces.is_empty() {
            conditions.push("1=0".to_string());
        } else {
            let placeholders: Vec<&str> = workspaces.iter().map(|_| "?").collect();
            conditions.push(format!(
                "{workspace_expression} IN ({})",
                placeholders.join(", ")
            ));
            for ws in workspaces {
                params.push(ParamValue::from(ws.to_string_lossy().to_string()));
            }
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

fn bounded_keyset_page_where(base_where: &str, id_column: &str, last_id: Option<i64>) -> String {
    let mut page_where = if base_where.is_empty() {
        format!(" WHERE {id_column} <= ?")
    } else {
        format!("{base_where} AND {id_column} <= ?")
    };
    if last_id.is_some() {
        page_where.push_str(&format!(" AND {id_column} > ?"));
    }
    page_where
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

    fn redact_context_for_test(text: &str, start: usize, end: usize, window: usize) -> String {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false)
            .expect("construct test secret-scan config");
        redact_context(text, start, end, window, &config)
    }

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
    fn redact_token_long_is_fully_opaque() {
        let result = redact_token("sk-abcdefghijklmnop");
        assert_eq!(result, REDACTED_CONTEXT);
        assert!(!result.contains("sk"));
        assert!(!result.contains("op"));
    }

    #[test]
    fn redact_token_nine_chars_is_fully_opaque() {
        let result = redact_token("123456789");
        assert_eq!(result, REDACTED_CONTEXT);
    }

    // =========================================================================
    // Redact context tests
    // =========================================================================

    #[test]
    fn redact_context_empty_text_returns_empty() {
        assert_eq!(redact_context_for_test("", 0, 0, 120), "");
    }

    #[test]
    fn redact_context_replaces_match_with_replacement() {
        let text = "The key is sk-ABCDEFGHIJ and more";
        let start = 11;
        let end = 25;
        let result = redact_context_for_test(text, start, end, 120);
        assert!(result.contains(REDACTED_CONTEXT), "result: {}", result);
        assert!(
            !result.contains("sk-ABCDEFGHIJ"),
            "secret should be removed: {}",
            result
        );
    }

    #[test]
    fn redact_context_match_at_start() {
        let text = "sk-SECRET rest of the text";
        let result = redact_context_for_test(text, 0, 9, 120);
        assert!(result.starts_with(REDACTED_CONTEXT), "result: {}", result);
    }

    #[test]
    fn redact_context_match_at_end() {
        let text = "prefix sk-SECRET";
        let result = redact_context_for_test(text, 7, 16, 120);
        assert!(result.ends_with(REDACTED_CONTEXT), "result: {}", result);
    }

    #[test]
    fn redact_context_start_beyond_text_returns_empty() {
        assert_eq!(redact_context_for_test("short", 10, 15, 120), "");
    }

    #[test]
    fn redact_context_masks_private_key_body_and_adjacent_secret_classes() {
        let focal = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ123456789";
        let private_body = "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAA";
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature_123456789";
        let entropy = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let text = format!(
            "before {focal} middle -----BEGIN OPENSSH PRIVATE KEY-----\n{private_body}\n-----END OPENSSH PRIVATE KEY----- after {jwt} tail {entropy}"
        );
        let start = text.find(focal).expect("focal fixture offset");
        let result = redact_context_for_test(&text, start, start + focal.len(), text.len() * 2);

        for raw_secret in [focal, private_body, jwt, entropy] {
            assert!(
                !result.contains(raw_secret),
                "context leaked adjacent secret {raw_secret:?}: {result}"
            );
        }
        assert!(
            result.matches(REDACTED_CONTEXT).count() >= 3,
            "distinct secret ranges should be visibly masked: {result}"
        );
    }

    #[test]
    fn redact_context_masks_allowlisted_and_custom_denylist_neighbors() {
        let allowlisted = "sk-ALLOWLISTabcdefghijklmnopqrstuvwxyz012345";
        let denied = "INTERNAL_SECRET_ABC123XYZ789";
        let focal = "AKIAIOSFODNN7EXAMPLE";
        let text = format!("{allowlisted} before {focal} after {denied}");
        let config = SecretScanConfig::from_inputs_with_env(
            &["sk-ALLOWLIST.*".to_string()],
            &["INTERNAL_SECRET_[A-Z0-9]+".to_string()],
            false,
        )
        .expect("construct test secret-scan config");
        let start = text.find(focal).expect("focal fixture offset");
        let result = redact_context(
            &text,
            start,
            start + focal.len(),
            text.len() * 2,
            &config,
        );

        assert!(!result.contains(allowlisted), "allowlisted neighbor leaked");
        assert!(!result.contains(denied), "denylisted neighbor leaked");
    }

    #[test]
    fn report_provenance_masks_custom_denylist_and_entropy_secrets() {
        let denied = "INTERNAL_SECRET_ABC123XYZ789";
        let entropy = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let value = Some(format!("/tmp/{denied}/safe.txt/{entropy}/session.jsonl"));
        let config = SecretScanConfig::from_inputs_with_env(
            &[],
            &["INTERNAL_SECRET_[A-Z0-9]+".to_string()],
            false,
        )
        .unwrap();

        let redacted = redact_report_provenance(&value, &config).unwrap();
        assert!(!redacted.contains(denied), "custom denylist value leaked");
        assert!(!redacted.contains(entropy), "entropy secret leaked");
        assert!(
            redacted.matches(REDACTED_CONTEXT).count() >= 2,
            "independent provenance secrets should be masked: {redacted}"
        );
    }

    #[test]
    fn truncated_zero_finding_report_is_not_presented_as_clean() {
        let report = SecretScanReport {
            summary: SecretScanSummary {
                total: 0,
                by_severity: BTreeMap::new(),
                has_critical: false,
                truncated: true,
            },
            findings: Vec::new(),
        };
        let mut output = Vec::new();

        write_human_report(&mut output, &report, 3).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("No secrets detected"), "{output}");
        assert!(output.contains("Results truncated"), "{output}");
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

    #[test]
    fn structured_allowlist_requires_a_full_scalar_match() {
        let config = SecretScanConfig::from_inputs_with_env(
            &["SAFE".to_string()],
            &[],
            false,
        )
        .unwrap();
        assert!(is_fully_allowlisted("SAFE", &config));
        assert!(!is_fully_allowlisted("SAFE-real-secret", &config));
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

    #[test]
    fn table_exists_distinguishes_absence_from_invalid_schema_probe() -> Result<()> {
        let conn = crate::franken_sync::Connection::open(":memory:")?;
        conn.execute("CREATE TABLE snippets (id INTEGER PRIMARY KEY);")?;

        assert!(table_exists(&conn, "snippets")?);
        assert!(!table_exists(&conn, "not_present")?);

        let error = table_exists(&conn, "snippets; DROP TABLE snippets")
            .expect_err("invalid schema identifiers must fail closed");
        assert!(
            error.to_string().contains("Invalid SQLite identifier"),
            "unexpected invalid-identifier diagnostic: {error:#}"
        );
        Ok(())
    }

    #[test]
    fn staged_export_scan_reads_flat_pages_schema_and_binds_digest() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("export.db");
        let conn = crate::franken_sync::Connection::open(db_path.to_string_lossy().as_ref())?;
        conn.execute_batch(
            r#"
            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent TEXT NOT NULL,
                workspace TEXT,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                ended_at INTEGER,
                message_count INTEGER,
                metadata_json TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                conversation_id INTEGER NOT NULL,
                idx INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER,
                updated_at INTEGER,
                model TEXT,
                attachment_refs TEXT
            );
            CREATE TABLE snippets (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                file_path TEXT,
                start_line INTEGER,
                end_line INTEGER,
                language TEXT,
                snippet_text TEXT
            );
            INSERT INTO conversations (
                id, agent, workspace, title, source_path, started_at,
                message_count, metadata_json
            ) VALUES (
                1, 'codex', '/tmp/project', 'safe title',
                'session.jsonl', 1700000000000, 2, '{}'
            );
            INSERT INTO messages (
                id, conversation_id, idx, role, content, created_at
            ) VALUES (
                7, 1, 0, 'user', 'credential AKIAIOSFODNN7EXAMPLE',
                1700000000000
            );
            INSERT INTO messages (
                id, conversation_id, idx, role, content, created_at, model,
                attachment_refs
            ) VALUES (
                8, 1, 1, 'assistant', 'safe content', 1700000000001,
                'safe safe safe safe safe safe safe safe safe safe AUX_MODEL_CREDENTIAL',
                'AUX_ATTACHMENT_CREDENTIAL'
            );
            INSERT INTO snippets (
                id, message_id, file_path, language, snippet_text
            ) VALUES (
                9, 8,
                'safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe AUX_PATH_CREDENTIAL',
                'safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe safe AUX_LANGUAGE_CREDENTIAL',
                'safe snippet'
            );
            "#,
        )?;
        drop(conn);

        let config = SecretScanConfig::from_inputs_with_env(
            &[],
            &["AUX_(?:MODEL|ATTACHMENT|PATH|LANGUAGE)_CREDENTIAL".to_string()],
            false,
        )?;
        let attestation = scan_staged_export_database(&db_path, &config)?;

        assert_eq!(attestation.artifact_sha256.len(), 64);
        assert!(!attestation.report.summary.truncated);
        let finding = attestation
            .report
            .findings
            .iter()
            .find(|finding| finding.kind == "aws_access_key_id")
            .expect("message content credential must be detected");
        assert_eq!(finding.agent.as_deref(), Some("codex"));
        assert_eq!(finding.workspace.as_deref(), Some("/tmp/project"));
        assert_eq!(finding.message_id, Some(7));
        let auxiliary_findings: Vec<_> = attestation
            .report
            .findings
            .iter()
            .filter(|finding| finding.kind == CUSTOM_DENYLIST_PATTERN_ID)
            .collect();
        assert_eq!(
            auxiliary_findings.len(),
            4,
            "model, attachment, snippet path, and snippet language must all be scanned"
        );
        assert!(auxiliary_findings.iter().all(|finding| {
            finding.message_id == Some(8) && finding.location == SecretLocation::MessageMetadata
        }));
        Ok(())
    }

    #[test]
    fn cancellation_at_conversation_page_boundary_stops_before_message_schema() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("scan.db");
        let conn = crate::franken_sync::Connection::open(db_path.to_string_lossy().as_ref())?;
        conn.execute_batch(
            r#"
            CREATE TABLE agents (
                id INTEGER PRIMARY KEY,
                slug TEXT NOT NULL
            );
            CREATE TABLE workspaces (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL
            );
            CREATE TABLE conversations (
                id INTEGER PRIMARY KEY,
                agent_id INTEGER,
                workspace_id INTEGER,
                title TEXT,
                source_path TEXT NOT NULL,
                started_at INTEGER,
                metadata_json TEXT
            );
            INSERT INTO agents (id, slug) VALUES (1, 'codex');
            INSERT INTO workspaces (id, path) VALUES (1, '/tmp/project');
            INSERT INTO conversations (
                id, agent_id, workspace_id, title, source_path, started_at, metadata_json
            ) VALUES (
                1, 1, 1, 'safe title', '/tmp/project/session.jsonl', 1700000000000, '{}'
            );
            "#,
        )?;
        drop(conn);

        let filters = SecretScanFilters {
            agents: None,
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false)?;
        let error = scan_database_with_cancel_check(
            &db_path,
            &filters,
            &config,
            None,
            |checkpoint| checkpoint == SecretScanCheckpoint::AfterConversations,
        )
        .expect_err("page-boundary cancellation must abort instead of probing messages");

        assert!(
            error.to_string().contains("Secret scan cancelled"),
            "the missing messages table must never be probed after cancellation: {error:#}"
        );
        Ok(())
    }

    // =========================================================================
    // Scan text tests (via scan_database with crafted DB)
    // =========================================================================

    #[test]
    fn builtin_patterns_aws_access_key_detected() -> Result<(), String> {
        let pattern = &BUILTIN_PATTERNS[0]; // aws_access_key_id
        let cases = [
            ("Found key AKIAIOSFODNN7EXAMPLE in config", true),
            ("Found temporary key ASIAIOSFODNN7EXAMPLE in config", true),
            ("ASIAIOSFODNN7EXAMPL", false),
            ("asiaiosfodnn7example", false),
        ];
        match cases
            .into_iter()
            .find(|(input, expected)| pattern.regex.is_match(input) != *expected)
        {
            Some((input, expected)) => Err(format!(
                "AWS access-key scanner returned {} for {input:?}, expected {expected}",
                pattern.regex.is_match(input)
            )),
            None => Ok(()),
        }
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
    fn builtin_patterns_private_key_detected() -> Result<(), String> {
        let pattern = &BUILTIN_PATTERNS[6]; // private_key
        for kind in [
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
            "DSA PRIVATE KEY",
            "OPENSSH PRIVATE KEY",
            "PRIVATE KEY",
            "ENCRYPTED PRIVATE KEY",
            "PGP PRIVATE KEY BLOCK",
        ] {
            let text = format!("-----BEGIN {kind}-----\nMIIE...");
            if !pattern.regex.is_match(&text) {
                return Err(format!("post-hoc scanner missed {kind}"));
            }
        }
        Ok(())
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

    #[test]
    fn severity_summary_serializes_in_stable_rank_order() {
        let mut by_severity = BTreeMap::new();
        by_severity.insert(SecretSeverity::Low, 1);
        by_severity.insert(SecretSeverity::Critical, 1);
        by_severity.insert(SecretSeverity::Medium, 1);
        by_severity.insert(SecretSeverity::High, 1);
        let encoded = serde_json::to_string(&SecretScanSummary {
            total: 4,
            by_severity,
            has_critical: true,
            truncated: false,
        })
        .unwrap();

        let positions = ["critical", "high", "medium", "low"]
            .map(|label| encoded.find(label).expect("serialized severity label"));
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{encoded}");
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
        assert!(
            clause.contains("COALESCE(a.slug, 'unknown') IN"),
            "clause: {}",
            clause
        );
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
    fn build_where_clause_empty_agent_list_matches_nothing() {
        let filters = SecretScanFilters {
            agents: Some(vec![]),
            workspaces: None,
            since_ts: None,
            until_ts: None,
        };
        let (clause, _) = build_where_clause(&filters).unwrap();
        assert!(
            clause.contains("1=0"),
            "empty agent list should match nothing: {}",
            clause
        );
    }

    #[test]
    fn build_where_clause_empty_workspace_list_matches_nothing() {
        let filters = SecretScanFilters {
            agents: None,
            workspaces: Some(vec![]),
            since_ts: None,
            until_ts: None,
        };
        let (clause, _) = build_where_clause(&filters).unwrap();
        assert!(
            clause.contains("1=0"),
            "empty workspace list should match nothing: {}",
            clause
        );
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
        let mut seen = Vec::new();
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
        let mut seen = Vec::new();
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
        let mut seen = Vec::new();
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
        let mut seen = Vec::new();
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
        let mut seen = Vec::new();
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
    fn scan_text_preserves_distinct_occurrences_with_identical_display_masks() {
        let config = SecretScanConfig::from_inputs_with_env(
            &[],
            &[r"AA[0-9]{8}ZZ".to_string()],
            false,
        )
        .unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: Some("fixture.jsonl".to_string()),
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = Vec::new();
        let mut truncated = false;

        scan_text(
            "AA11111111ZZ AA22222222ZZ",
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].match_redacted, findings[1].match_redacted);
    }

    #[test]
    fn scan_text_reports_overlapping_detectors_as_one_occurrence() {
        let config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: Some("fixture.jsonl".to_string()),
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = Vec::new();
        let mut truncated = false;

        scan_text(
            "token=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, "github_pat");
    }

    #[test]
    fn overlapping_later_detector_upgrades_to_highest_severity_at_capacity() {
        let mut config = SecretScanConfig::from_inputs_with_env(&[], &[], false).unwrap();
        config.max_findings = 1;
        let ctx = ScanContext {
            agent: None,
            workspace: None,
            source_path: Some("fixture.jsonl".to_string()),
            conversation_id: Some(1),
            message_id: Some(1),
            message_idx: Some(0),
        };
        let mut findings = Vec::new();
        let mut seen = Vec::new();
        let mut truncated = false;

        scan_text(
            r#"authorization="Bearer abcdefgh12345678""#,
            SecretLocation::MessageContent,
            &ctx,
            &config,
            &mut findings,
            &mut seen,
            &mut truncated,
        );

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, "bearer_token");
        assert_eq!(findings[0].severity, SecretSeverity::High);
        assert!(!truncated, "an overlapping upgrade must not consume capacity");
    }

    #[test]
    fn context_range_storage_excludes_matches_outside_the_window() {
        let config = SecretScanConfig::from_inputs_with_env(
            &[],
            &[r"SECRET[0-9]{4}".to_string()],
            false,
        )
        .unwrap();
        let text = (0..1_000)
            .map(|index| format!("SECRET{index:04} "))
            .collect::<String>();
        let focal = text.find("SECRET0500").unwrap();
        let ranges = collect_context_redactions(
            &text,
            &config,
            focal.saturating_sub(1),
            focal + "SECRET0500".len() + 1,
        );

        assert_eq!(ranges.len(), 1, "only the bounded context span is retained");
        assert_eq!(ranges[0].start, focal);
    }

    #[test]
    fn scan_text_max_findings_truncates() {
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
        let mut seen = Vec::new();
        let mut truncated = false;

        // Distinct source spans remain distinct findings even though report
        // redaction intentionally gives every match the same opaque form.
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
        let mut seen = Vec::new();
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
