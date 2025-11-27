use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct AmpConnector;
impl Default for AmpConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl AmpConnector {
    pub fn new() -> Self {
        Self
    }

    fn cache_root() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("amp")
    }

    fn vscode_global_storage() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".config/Code/User/globalStorage/sourcegraph.amp"));
            roots.push(
                home.join("Library/Application Support/Code/User/globalStorage/sourcegraph.amp"),
            );
            roots.push(home.join("AppData/Roaming/Code/User/globalStorage/sourcegraph.amp"));
        }
        roots
    }

    pub fn candidate_roots() -> Vec<PathBuf> {
        let mut roots = vec![Self::cache_root()];
        roots.extend(Self::vscode_global_storage());
        roots
    }
}

impl Connector for AmpConnector {
    fn detect(&self) -> DetectionResult {
        let evidence: Vec<String> = Self::candidate_roots()
            .into_iter()
            .filter(|r| r.exists())
            .map(|r| format!("found {}", r.display()))
            .collect();

        if evidence.is_empty() {
            DetectionResult::not_found()
        } else {
            DetectionResult {
                detected: true,
                evidence,
            }
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut convs = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        // allow tests to override via ctx.data_root
        let roots = if ctx.data_root.exists() {
            vec![ctx.data_root.clone()]
        } else {
            Self::candidate_roots()
        };

        for root in roots {
            if !root.exists() {
                continue;
            }

            for entry in WalkDir::new(&root).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if !is_amp_log_file(path) {
                    continue;
                }
                let text = match std::fs::read_to_string(path) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let val: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(messages) = extract_messages(&val, ctx.since_ts) {
                    if messages.is_empty() {
                        continue;
                    }
                    let title = val
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            messages
                                .first()
                                .and_then(|m| m.content.lines().next())
                                .map(|s| s.to_string())
                        });

                    let workspace = infer_workspace(&val).or_else(|| {
                        messages.iter().find_map(|m| {
                            m.extra
                                .get("workspace")
                                .and_then(|w| w.as_str())
                                .map(PathBuf::from)
                        })
                    });

                    let external_id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            val.get("id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        });

                    let key = external_id
                        .clone()
                        .map(|id| format!("amp:{id}"))
                        .unwrap_or_else(|| format!("amp:{}", path.display()));
                    if seen_ids.insert(key) {
                        convs.push(NormalizedConversation {
                            agent_slug: "amp".into(),
                            external_id,
                            title,
                            workspace,
                            source_path: path.to_path_buf(),
                            started_at: messages.first().and_then(|m| m.created_at),
                            ended_at: messages.last().and_then(|m| m.created_at),
                            metadata: val.clone(),
                            messages,
                        });
                        tracing::info!(
                            target: "connector::amp",
                            source = %path.display(),
                            messages = convs.last().map(|c| c.messages.len()).unwrap_or(0),
                            since_ts = ctx.since_ts,
                            "amp_scan"
                        );
                    }
                }
            }
        }

        Ok(convs)
    }
}

fn extract_messages(val: &Value, since_ts: Option<i64>) -> Option<Vec<NormalizedMessage>> {
    let msgs = val
        .get("messages")
        .and_then(|m| m.as_array().cloned())
        .or_else(|| {
            val.get("thread")
                .and_then(|t| t.get("messages"))
                .and_then(|m| m.as_array().cloned())
        })?;

    let mut out = Vec::new();
    for m in msgs.into_iter() {
        let role = m
            .get("role")
            .or_else(|| m.get("speaker"))
            .or_else(|| m.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("agent")
            .to_string();
        let content = m
            .get("content")
            .or_else(|| m.get("text"))
            .or_else(|| m.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // Use parse_timestamp to handle both i64 milliseconds and ISO-8601 strings
        let created_at = m
            .get("created_at")
            .or_else(|| m.get("createdAt"))
            .or_else(|| m.get("timestamp"))
            .or_else(|| m.get("ts"))
            .and_then(crate::connectors::parse_timestamp);
        let author = m
            .get("author")
            .or_else(|| m.get("sender"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(since) = since_ts
            && let Some(ts) = created_at
            && ts <= since
        {
            continue;
        }

        out.push(NormalizedMessage {
            idx: 0, // Will be re-assigned after filtering
            role,
            author,
            created_at,
            content,
            extra: m.clone(),
            snippets: Vec::new(),
        });
    }

    // Re-assign indices after filtering to maintain sequential order
    for (i, msg) in out.iter_mut().enumerate() {
        msg.idx = i as i64;
    }

    if out.is_empty() { None } else { Some(out) }
}

fn infer_workspace(val: &Value) -> Option<PathBuf> {
    let keys = ["workspace", "cwd", "path", "project_path", "repo", "root"];
    for k in keys {
        if let Some(p) = val.get(k).and_then(|v| v.as_str()) {
            return Some(PathBuf::from(p));
        }
    }
    None
}

fn is_amp_log_file(path: &std::path::Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return false;
    }
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        let stem_lower = stem.to_ascii_lowercase();
        return stem_lower.contains("thread")
            || stem_lower.contains("conversation")
            || stem_lower.contains("chat");
    }
    false
}
