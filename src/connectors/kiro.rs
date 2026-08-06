//! Connector for Kiro coding-agent session logs.
//!
//! Unlike most connectors, Kiro has **no** implementation in the external
//! `franken_agent_detection` (FAD) crate, so this connector is defined
//! CASS-side and implements the re-exported [`Connector`] trait directly —
//! mirroring the local-connector pattern proven by [`super::codex`]. It builds
//! [`NormalizedConversation`]/[`NormalizedMessage`] values from Kiro's own log
//! files using the FAD helpers re-exported by [`super`].
//!
//! # Storage layout (Kiro CLI — the primary, richest surface)
//!
//! Kiro CLI (`kiro-cli chat`) writes a flat session store at:
//!
//! - `~/.kiro/sessions/cli/<session_uuid>.jsonl` — append-only event log
//!   (the primary parse target). Each line is a record:
//!   `{"version": "v1", "kind": <RecordKind>, "data": { … }}`.
//! - `~/.kiro/sessions/cli/<session_uuid>.json` — session-state snapshot read
//!   as a metadata sidecar (`session_id`, `cwd`, `title`, `created_at`,
//!   `updated_at`, `session_state.model_info.model_id`).
//! - `~/.kiro/sessions/cli/<session_uuid>.lock` / `sess_<uuid>.history` —
//!   ignored (not conversation logs).
//!
//! Record `kind` → normalized role:
//! - `Prompt` → `user` (carries `data.meta.timestamp` as **epoch seconds**).
//! - `AssistantMessage` → `assistant` (no timestamp).
//! - `ToolResults` → `tool` (no timestamp).
//!
//! Content blocks (`data.content[]`, each `{kind, data}`):
//! - `text` → indexed text (`data` is the string, or `{ … , "data": "…"}`).
//! - `thinking` → **excluded** from indexed content (assistant reasoning).
//! - `toolUse` → `[Tool: <name>]\n<input-json>` plus a [`NormalizedInvocation`].
//! - `toolResult` → `[Tool output: <id>]\n<text>`.
//!
//! Unknown record kinds and malformed lines are tolerated (skip-and-continue)
//! rather than failing the whole session. See `kiro-assumptions.md` for the
//! full observed contract and the assumptions this parser encodes.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use franken_agent_detection::NormalizedInvocation;
use serde_json::Value;

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    NormalizedMessage, ScanContext, ScanRoot, file_modified_since, parse_timestamp,
};

/// CASS-local connector for Kiro CLI session logs.
pub struct KiroConnector;

impl Default for KiroConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroConnector {
    /// Create a new Kiro connector.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The default Kiro CLI session store: `~/.kiro/sessions/cli`.
    fn default_sessions_root() -> Option<PathBuf> {
        home_dir().map(|home| home.join(".kiro").join("sessions").join("cli"))
    }

    /// Heuristic: does this path look like a Kiro session store? Kiro's default
    /// store lives under `~/.kiro/...` and test fixtures live under a `kiro`
    /// directory, so a `kiro` path segment is the stable signal.
    fn looks_like_kiro_storage(path: &Path) -> bool {
        path.to_string_lossy().to_lowercase().contains("kiro")
    }

    /// Resolve the concrete scan roots for this context, honoring explicit
    /// `scan_roots` when present and falling back to default detection.
    fn source_roots(ctx: &ScanContext) -> Vec<ScanRoot> {
        let mut roots: Vec<ScanRoot> = Vec::new();
        if ctx.use_default_detection() {
            if Self::looks_like_kiro_storage(&ctx.data_dir) && ctx.data_dir.exists() {
                roots.push(ScanRoot::local(ctx.data_dir.clone()));
            } else if let Some(default_root) = Self::default_sessions_root() {
                if default_root.exists() {
                    roots.push(ScanRoot::local(default_root));
                }
            }
        } else {
            for root in &ctx.scan_roots {
                let candidate = root.path.join(".kiro").join("sessions").join("cli");
                if candidate.exists() {
                    roots.push(root.with_path(candidate));
                }
                if Self::looks_like_kiro_storage(&root.path) && root.path.exists() {
                    roots.push(root.with_path(root.path.clone()));
                }
            }
        }
        roots.sort_by(|a, b| a.path.cmp(&b.path));
        roots.dedup_by(|a, b| a.path == b.path);
        roots
    }

    /// Enumerate the non-empty `*.jsonl` session logs under `root`
    /// (recursively), in deterministic order. Empty (0-byte) logs are skipped:
    /// they yield no conversations and would violate the "≥1 message" contract.
    pub(crate) fn session_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if !root.exists() {
            return out;
        }

        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let Ok(read_dir) = fs::read_dir(&dir) else {
                continue;
            };
            let mut entries: Vec<_> = read_dir.flatten().collect();
            entries.sort_by_key(std::fs::DirEntry::path);
            for entry in entries {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    pending.push(path);
                    continue;
                }
                if file_type.is_file()
                    && path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                    && fs::metadata(&path).map(|meta| meta.len() > 0).unwrap_or(false)
                {
                    out.push(path);
                }
            }
        }

        // Keep connector traversal deterministic across filesystems/runs.
        out.sort();
        out.dedup();
        out
    }

    /// Normalize a scan root so that a file root is treated as its parent dir.
    fn as_dir_root(mut root: ScanRoot) -> ScanRoot {
        if root.path.is_file() {
            let parent = root
                .path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| root.path.clone());
            root = root.with_path(parent);
        }
        root
    }

    /// Parse a single Kiro CLI `<uuid>.jsonl` log (paired with its optional
    /// `<uuid>.json` metadata sidecar) into a normalized conversation.
    ///
    /// Returns `None` for unreadable files or logs that yield no messages.
    #[allow(clippy::too_many_lines)]
    fn parse_session_file(root: &ScanRoot, file: &Path) -> Option<NormalizedConversation> {
        let source_path = file.to_path_buf();

        // Project-relative external id (fall back to the bare file stem/uuid).
        let external_id = source_path
            .strip_prefix(&root.path)
            .ok()
            .and_then(|rel| {
                rel.with_extension("")
                    .to_str()
                    .map(std::string::ToString::to_string)
            })
            .or_else(|| {
                source_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(std::string::ToString::to_string)
            });

        // Metadata sidecar: `<stem>.json` alongside the `.jsonl` log.
        let sidecar = file.with_extension("json");
        let meta = fs::read_to_string(&sidecar)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());

        let created_val = meta.as_ref().and_then(|m| m.get("created_at"));
        let conv_created_ms = created_val.and_then(parse_timestamp);
        let created_at_str = created_val
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string);
        let updated_at_str = meta
            .as_ref()
            .and_then(|m| m.get("updated_at"))
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string);
        let session_id = meta
            .as_ref()
            .and_then(|m| m.get("session_id"))
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string)
            .or_else(|| {
                source_path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(std::string::ToString::to_string)
            });
        let cwd = meta
            .as_ref()
            .and_then(|m| m.get("cwd"))
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string);
        let json_title = meta
            .as_ref()
            .and_then(|m| m.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(std::string::ToString::to_string);
        let model_id = meta
            .as_ref()
            .and_then(|m| m.pointer("/session_state/model_info/model_id"))
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string);
        let workspace = cwd.as_ref().map(PathBuf::from);

        // Base timestamp so every message can carry a present, nondecreasing
        // value: prefer the session's created_at, else the file mtime, else 0.
        let base_ts = conv_created_ms.or_else(|| file_mtime_ms(file)).unwrap_or(0);

        let file_handle = match fs::File::open(file) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::debug!(path = %file.display(), error = %err, "kiro: skipping unreadable session log");
                return None;
            }
        };

        let mut messages: Vec<NormalizedMessage> = Vec::new();
        let mut last_ts = base_ts;
        for line in BufReader::new(file_handle).lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            let role = match record.get("kind").and_then(Value::as_str) {
                Some("Prompt") => "user",
                Some("AssistantMessage") => "assistant",
                Some("ToolResults") => "tool",
                // Tolerate unknown/other record kinds (system, summary, …).
                _ => continue,
            };

            let data = record.get("data");
            let (content, invocations) = data
                .and_then(|d| d.get("content"))
                .map_or_else(|| (String::new(), Vec::new()), flatten_kiro_content);
            if content.trim().is_empty() {
                continue;
            }

            // Only Prompt records carry a timestamp (epoch seconds). Assistant
            // and tool records carry the previous value forward so the sequence
            // stays monotonically nondecreasing.
            let raw_ts = if role == "user" {
                data.and_then(|d| d.pointer("/meta/timestamp"))
                    .and_then(kiro_epoch_ms)
            } else {
                None
            };
            let effective_ts = raw_ts.map_or(last_ts, |ts| ts.max(last_ts));
            last_ts = effective_ts;

            messages.push(NormalizedMessage {
                idx: i64::try_from(messages.len()).unwrap_or(i64::MAX),
                role: role.to_string(),
                author: None,
                created_at: Some(effective_ts),
                content,
                extra: record,
                invocations,
                snippets: Vec::new(),
            });
        }

        if messages.is_empty() {
            return None;
        }

        let title = json_title
            .or_else(|| {
                messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| first_line_capped(&m.content))
            })
            .or_else(|| messages.first().map(|m| first_line_capped(&m.content)));

        // Messages are in append order with monotonic timestamps, so the first
        // and last bound the conversation.
        let started_at = messages.first().and_then(|m| m.created_at);
        let ended_at = messages.last().and_then(|m| m.created_at);

        let metadata = serde_json::json!({
            "source": "kiro",
            "format": "cli",
            "session_id": session_id,
            "cwd": cwd,
            "model_id": model_id,
            "created_at": created_at_str,
            "updated_at": updated_at_str,
        });

        Some(NormalizedConversation {
            agent_slug: "kiro".to_string(),
            external_id,
            title,
            workspace,
            source_path,
            started_at,
            ended_at,
            metadata,
            messages,
        })
    }
}

impl Connector for KiroConnector {
    fn detect(&self) -> DetectionResult {
        let mut evidence = Vec::new();
        let mut root_paths = Vec::new();
        if let Some(root) = Self::default_sessions_root() {
            if root.exists() {
                let count = Self::session_files(&root).len();
                evidence.push(format!(
                    "Kiro CLI session store present at {} ({count} session log(s))",
                    root.display()
                ));
                root_paths.push(root);
            }
        }
        DetectionResult {
            detected: !root_paths.is_empty(),
            evidence,
            root_paths,
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut conversations = Vec::new();
        self.scan_with_callback(ctx, &mut |conversation| {
            conversations.push(conversation);
            Ok(())
        })?;
        Ok(conversations)
    }

    fn supports_streaming_scan(&self) -> bool {
        true
    }

    fn scan_with_callback(
        &self,
        ctx: &ScanContext,
        on_conversation: &mut dyn FnMut(NormalizedConversation) -> Result<()>,
    ) -> Result<()> {
        for root in Self::source_roots(ctx) {
            let root = Self::as_dir_root(root);
            for file in Self::session_files(&root.path) {
                if !file_modified_since(&file, ctx.since_ts) {
                    continue;
                }
                if let Some(conversation) = Self::parse_session_file(&root, &file) {
                    on_conversation(conversation)?;
                }
            }
        }
        Ok(())
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let mut out = Vec::new();
        for root in Self::source_roots(ctx) {
            let root = Self::as_dir_root(root);
            for file in Self::session_files(&root.path) {
                if !file_modified_since(&file, ctx.since_ts) {
                    continue;
                }
                let sidecar = file.with_extension("json");
                out.push(
                    DiscoveredSourceFile::new(
                        "kiro",
                        &root,
                        file,
                        DiscoveredSourceRole::PrimarySessionLog,
                        true,
                    )
                    .with_fs_metadata(),
                );
                if sidecar.exists() {
                    out.push(
                        DiscoveredSourceFile::new(
                            "kiro",
                            &root,
                            sidecar,
                            DiscoveredSourceRole::MetadataSidecar,
                            false,
                        )
                        .with_fs_metadata(),
                    );
                }
            }
        }
        Ok(out)
    }
}

/// Resolve the user home directory without pulling in extra dependencies.
/// Prefers `HOME` (macOS/Linux); falls back to `USERPROFILE` (Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
}

/// Interpret a Kiro timestamp as epoch milliseconds.
///
/// Kiro CLI `Prompt` records store `meta.timestamp` as epoch **seconds**
/// (numeric); CASS normalizes to epoch milliseconds. Values that already look
/// like milliseconds (>= ~1e11) are passed through unscaled. ISO-8601 strings
/// (used in the metadata sidecar) fall back to [`parse_timestamp`].
fn kiro_epoch_ms(value: &Value) -> Option<i64> {
    const MS_THRESHOLD: i64 = 100_000_000_000;
    if let Some(secs) = value.as_i64() {
        return Some(if secs.abs() < MS_THRESHOLD {
            secs.saturating_mul(1000)
        } else {
            secs
        });
    }
    if let Some(secs) = value.as_f64() {
        let secs = secs as i64;
        return Some(if secs.abs() < MS_THRESHOLD {
            secs.saturating_mul(1000)
        } else {
            secs
        });
    }
    parse_timestamp(value)
}

/// Best-effort file mtime in epoch milliseconds.
fn file_mtime_ms(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|dur| i64::try_from(dur.as_millis()).ok())
}

/// First non-empty line of `content`, capped at 100 characters (title helper).
fn first_line_capped(content: &str) -> String {
    content
        .lines()
        .next()
        .unwrap_or(content)
        .chars()
        .take(100)
        .collect()
}

/// Push a trimmed, non-empty string onto `out`.
fn push_text(out: &mut Vec<String>, text: &str) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
}

/// Flatten a Kiro `content` array into indexed text plus any tool invocations.
///
/// `thinking` blocks are deliberately excluded from the indexed text.
fn flatten_kiro_content(content: &Value) -> (String, Vec<NormalizedInvocation>) {
    let mut texts: Vec<String> = Vec::new();
    let mut invocations: Vec<NormalizedInvocation> = Vec::new();

    if let Some(text) = content.as_str() {
        push_text(&mut texts, text);
        return (texts.join("\n"), invocations);
    }

    let Some(blocks) = content.as_array() else {
        return (String::new(), invocations);
    };

    for block in blocks {
        let kind = block.get("kind").and_then(Value::as_str).unwrap_or("");
        let data = block.get("data");
        match kind {
            "text" => {
                if let Some(data) = data {
                    if let Some(text) = data.as_str() {
                        push_text(&mut texts, text);
                    } else if let Some(text) = data.get("data").and_then(Value::as_str) {
                        push_text(&mut texts, text);
                    }
                }
            }
            // Assistant reasoning — excluded from indexed content.
            "thinking" => {}
            "toolUse" => {
                let name = data
                    .and_then(|d| d.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let call_id = data
                    .and_then(|d| d.get("toolUseId"))
                    .and_then(Value::as_str)
                    .map(std::string::ToString::to_string);
                let input = data.and_then(|d| d.get("input")).cloned();
                let mut rendered = format!("[Tool: {name}]");
                if let Some(input) = &input {
                    if !input.is_null() {
                        if let Ok(json) = serde_json::to_string(input) {
                            if !json.is_empty() {
                                rendered.push('\n');
                                rendered.push_str(&json);
                            }
                        }
                    }
                }
                texts.push(rendered);
                invocations.push(NormalizedInvocation {
                    kind: "tool".to_string(),
                    name: name.to_string(),
                    raw_name: None,
                    call_id,
                    arguments: input,
                });
            }
            "toolResult" => {
                let call_id = data.and_then(|d| d.get("toolUseId")).and_then(Value::as_str);
                let mut parts: Vec<String> = Vec::new();
                if let Some(inner) = data.and_then(|d| d.get("content")).and_then(Value::as_array) {
                    for item in inner {
                        if let Some(text) = item.get("data").and_then(Value::as_str) {
                            push_text(&mut parts, text);
                        } else if let Some(text) = item.as_str() {
                            push_text(&mut parts, text);
                        }
                    }
                }
                let label = call_id.map_or_else(
                    || "[Tool output]".to_string(),
                    |id| format!("[Tool output: {id}]"),
                );
                let body = parts.join("\n");
                texts.push(if body.is_empty() {
                    label
                } else {
                    format!("{label}\n{body}")
                });
            }
            // Tolerate unknown block kinds: index any bare-string payload.
            _ => {
                if let Some(text) = data.and_then(Value::as_str) {
                    push_text(&mut texts, text);
                }
            }
        }
    }

    (texts.join("\n"), invocations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a `~/.kiro/sessions/cli` store inside a temp dir and return it.
    fn kiro_cli_dir(tmp: &TempDir) -> PathBuf {
        let dir = tmp.path().join(".kiro").join("sessions").join("cli");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn scan_parses_cli_session_with_metadata() {
        let tmp = TempDir::new().unwrap();
        let cli = kiro_cli_dir(&tmp);
        let uuid = "916d2b72-aaaa-bbbb-cccc-000000000001";

        write(
            &cli.join(format!("{uuid}.jsonl")),
            &[
                r#"{"version":"v1","kind":"Prompt","data":{"message_id":"m1","content":[{"kind":"text","data":"Add Kiro support to the connector"}],"meta":{"timestamp":1785939877}}}"#,
                r#"{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m2","content":[{"kind":"thinking","data":{"text":"internal reasoning that must not be indexed","signature":"sig"}},{"kind":"text","data":"On it — here is the plan."}]}}"#,
            ]
            .join("\n"),
        );
        write(
            &cli.join(format!("{uuid}.json")),
            r#"{"session_id":"916d2b72-aaaa-bbbb-cccc-000000000001","cwd":"/work/repo","title":"Kiro connector work","created_at":"2026-08-06T08:44:02.921363Z","updated_at":"2026-08-06T08:59:00.000000Z","session_state":{"version":"v1","model_info":{"model_name":"claude-opus-4.8","model_id":"claude-opus-4.8"}}}"#,
        );

        let connector = KiroConnector::new();
        let ctx = ScanContext::local_default(cli, None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.agent_slug, "kiro");
        assert_eq!(conv.title.as_deref(), Some("Kiro connector work"));
        assert_eq!(conv.workspace.as_deref(), Some(Path::new("/work/repo")));
        assert_eq!(conv.messages.len(), 2);

        // Roles + monotonic timestamps.
        assert_eq!(conv.messages[0].role, "user");
        assert_eq!(conv.messages[0].created_at, Some(1_785_939_877_000));
        assert_eq!(conv.messages[1].role, "assistant");
        assert!(conv.messages[1].created_at.unwrap() >= conv.messages[0].created_at.unwrap());
        assert_eq!(conv.started_at, conv.messages[0].created_at);
        assert_eq!(conv.ended_at, conv.messages[1].created_at);

        // Indices contiguous from 0.
        assert_eq!(conv.messages[0].idx, 0);
        assert_eq!(conv.messages[1].idx, 1);

        // `thinking` excluded; visible text preserved.
        assert!(conv.messages[1].content.contains("here is the plan"));
        assert!(!conv.messages[1].content.contains("internal reasoning"));

        assert_eq!(conv.metadata.get("model_id").and_then(Value::as_str), Some("claude-opus-4.8"));
    }

    #[test]
    fn scan_extracts_tool_use_and_tool_result() {
        let tmp = TempDir::new().unwrap();
        let cli = kiro_cli_dir(&tmp);
        let uuid = "916d2b72-aaaa-bbbb-cccc-000000000002";

        write(
            &cli.join(format!("{uuid}.jsonl")),
            &[
                r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"run the tests"}],"meta":{"timestamp":1785939900}}}"#,
                r#"{"version":"v1","kind":"AssistantMessage","data":{"content":[{"kind":"toolUse","data":{"toolUseId":"tool-1","name":"execute_bash","input":{"command":"cargo test"}}}]}}"#,
                r#"{"version":"v1","kind":"ToolResults","data":{"content":[{"kind":"toolResult","data":{"toolUseId":"tool-1","content":[{"kind":"text","data":"test result: ok. 39 passed"}]}}]}}"#,
            ]
            .join("\n"),
        );

        let connector = KiroConnector::new();
        let ctx = ScanContext::local_default(cli, None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        let conv = &convs[0];
        assert_eq!(conv.messages.len(), 3);

        let tool_use = &conv.messages[1];
        assert_eq!(tool_use.role, "assistant");
        assert!(tool_use.content.contains("[Tool: execute_bash]"));
        assert!(tool_use.content.contains("cargo test"));
        assert_eq!(tool_use.invocations.len(), 1);
        assert_eq!(tool_use.invocations[0].name, "execute_bash");
        assert_eq!(tool_use.invocations[0].call_id.as_deref(), Some("tool-1"));

        let tool_result = &conv.messages[2];
        assert_eq!(tool_result.role, "tool");
        assert!(tool_result.content.contains("[Tool output: tool-1]"));
        assert!(tool_result.content.contains("39 passed"));
    }

    #[test]
    fn scan_skips_empty_logs_and_tolerates_unknown_kinds() {
        let tmp = TempDir::new().unwrap();
        let cli = kiro_cli_dir(&tmp);

        // 0-byte log: must be skipped entirely.
        write(&cli.join("empty.jsonl"), "");

        // Log with an unknown record kind + a malformed line; only the valid
        // Prompt should survive.
        write(
            &cli.join("mixed.jsonl"),
            &[
                "",
                "not-json",
                r#"{"version":"v1","kind":"SystemPrompt","data":{"content":[{"kind":"text","data":"system"}]}}"#,
                r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"real user message"}],"meta":{"timestamp":1785939999}}}"#,
            ]
            .join("\n"),
        );

        let connector = KiroConnector::new();
        let ctx = ScanContext::local_default(cli, None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1, "empty log must not yield a conversation");
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].role, "user");
        assert!(convs[0].messages[0].content.contains("real user message"));
    }

    #[test]
    fn discover_source_files_lists_log_and_sidecar() {
        let tmp = TempDir::new().unwrap();
        let cli = kiro_cli_dir(&tmp);
        let uuid = "916d2b72-aaaa-bbbb-cccc-000000000003";
        write(
            &cli.join(format!("{uuid}.jsonl")),
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"hi"}],"meta":{"timestamp":1785939877}}}"#,
        );
        write(&cli.join(format!("{uuid}.json")), r#"{"session_id":"s"}"#);

        let connector = KiroConnector::new();
        let ctx = ScanContext::local_default(cli, None);
        let discovered = connector.discover_source_files(&ctx).unwrap();

        assert_eq!(discovered.len(), 2);
        assert!(discovered.iter().all(|d| d.provider_slug == "kiro"));
        assert!(
            discovered
                .iter()
                .any(|d| d.role == DiscoveredSourceRole::PrimarySessionLog
                    && d.required_for_reconstruction)
        );
        assert!(
            discovered
                .iter()
                .any(|d| d.role == DiscoveredSourceRole::MetadataSidecar
                    && !d.required_for_reconstruction)
        );

        // Discovery must cover every scanned source path.
        let scanned: Vec<PathBuf> = connector
            .scan(&ctx)
            .unwrap()
            .into_iter()
            .map(|c| c.source_path)
            .collect();
        for path in scanned {
            assert!(discovered.iter().any(|d| d.source_path == path));
        }
    }

    #[test]
    fn scan_respects_file_level_since_ts() {
        let tmp = TempDir::new().unwrap();
        let cli = kiro_cli_dir(&tmp);
        let log = cli.join("session.jsonl");
        write(
            &log,
            r#"{"version":"v1","kind":"Prompt","data":{"content":[{"kind":"text","data":"old"}],"meta":{"timestamp":1000}}}"#,
        );

        let mtime_ms = file_mtime_ms(&log).unwrap();
        let connector = KiroConnector::new();
        let ctx = ScanContext::local_default(cli, Some(mtime_ms.saturating_add(60_000)));

        let convs = connector.scan(&ctx).unwrap();
        assert!(
            convs.is_empty(),
            "a file older than since_ts must be skipped"
        );
    }
}
