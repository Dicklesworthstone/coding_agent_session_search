//! Connector for OpenClaw session logs.
//!
//! OpenClaw stores JSONL sessions at:
//! - ~/.openclaw/agents/<agent>/sessions/*.jsonl
//!
//! Each line has a `type` discriminator: "session", "message", "model_change",
//! "thinking_level_change", "custom". Messages are wrapped:
//! {"type":"message","id":"...","message":{"role":"user","content":[...],...}}

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
    file_modified_since, flatten_content, parse_timestamp,
};

pub struct OpenClawConnector;

impl Default for OpenClawConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenClawConnector {
    pub fn new() -> Self {
        Self
    }

    fn openclaw_home() -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".openclaw"))
    }

    fn agents_root() -> Option<PathBuf> {
        Self::openclaw_home().map(|home| home.join("agents"))
    }

    fn find_agent_session_dirs() -> Vec<PathBuf> {
        match Self::agents_root() {
            Some(agents_root) => Self::find_agent_session_dirs_at(&agents_root),
            None => Vec::new(),
        }
    }

    fn find_agent_session_dirs_at(agents_root: &Path) -> Vec<PathBuf> {
        tracing::debug!(
            agents_root = %agents_root.display(),
            "openclaw: scanning agents root for sessions directories"
        );

        if !agents_root.exists() || !agents_root.is_dir() {
            return Vec::new();
        }

        let mut session_dirs: Vec<PathBuf> = Vec::new();
        let walker = WalkDir::new(agents_root)
            .follow_links(false)
            .min_depth(1)
            .max_depth(2);

        for entry_res in walker {
            let entry = match entry_res {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::debug!(
                        agents_root = %agents_root.display(),
                        error = %err,
                        "openclaw: cannot read directory entry, continuing"
                    );
                    continue;
                }
            };

            if !entry.file_type().is_dir() || entry.depth() != 1 {
                continue;
            }

            let agent_name = entry.file_name().to_string_lossy().to_string();
            let sessions_dir = entry.path().join("sessions");
            let has_sessions = sessions_dir.is_dir();
            tracing::debug!(
                agent = %agent_name,
                has_sessions,
                "openclaw: found agent directory"
            );

            if has_sessions {
                session_dirs.push(sessions_dir);
            } else {
                tracing::debug!(
                    agent = %agent_name,
                    "openclaw: skipping agent directory without sessions/ subdirectory"
                );
            }
        }

        session_dirs.sort();
        session_dirs.dedup();

        let mut agent_names: Vec<String> = session_dirs
            .iter()
            .filter_map(|dir| {
                dir.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
            .collect();
        agent_names.sort();

        tracing::debug!(
            count = session_dirs.len(),
            agents = ?agent_names,
            "openclaw: discovered agent session directories"
        );

        session_dirs
    }

    fn detect_from_agents_root(agents_root: &Path) -> DetectionResult {
        let roots = Self::find_agent_session_dirs_at(agents_root);
        let mut evidence = vec![
            format!("found {}", agents_root.display()),
            format!("discovered {} agent session dirs", roots.len()),
        ];

        if !roots.is_empty() {
            let mut names: Vec<String> = roots
                .iter()
                .filter_map(|path| {
                    path.parent()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(String::from)
                })
                .collect();
            names.sort();
            evidence.push(format!("agents: {}", names.join(", ")));
        }

        DetectionResult {
            detected: true,
            evidence,
            root_paths: roots,
        }
    }

    fn looks_like_openclaw_storage(path: &Path) -> bool {
        let path_str = path.to_string_lossy().to_lowercase();
        path_str.contains("openclaw") && path_str.contains("sessions")
    }

    fn session_root_from_candidate(path: &Path) -> Option<PathBuf> {
        let dir = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };

        if dir.file_name().and_then(|n| n.to_str()) == Some("sessions") && dir.is_dir() {
            return Some(dir.to_path_buf());
        }

        let sessions = dir.join("sessions");
        if sessions.is_dir() {
            Some(sessions)
        } else {
            None
        }
    }

    fn roots_from_scan_path(path: &Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        if let Some(explicit) = Self::session_root_from_candidate(path)
            && Self::looks_like_openclaw_storage(&explicit)
        {
            roots.push(explicit);
        }

        let embedded_agents = path.join(".openclaw").join("agents");
        if embedded_agents.exists() {
            roots.extend(Self::find_agent_session_dirs_at(&embedded_agents));
        }

        if path.file_name().and_then(|n| n.to_str()) == Some(".openclaw") {
            roots.extend(Self::find_agent_session_dirs_at(&path.join("agents")));
        }

        if path.file_name().and_then(|n| n.to_str()) == Some("agents") {
            roots.extend(Self::find_agent_session_dirs_at(path));
        }

        roots.sort();
        roots.dedup();
        roots
    }

    fn agent_directory_from_sessions_root(path: &Path) -> String {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("openclaw")
            .to_string()
    }

    fn agent_slug_for_directory(agent_dir: &str) -> String {
        if agent_dir == "openclaw" {
            "openclaw".to_string()
        } else {
            format!("openclaw/{agent_dir}")
        }
    }

    fn session_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if !root.exists() {
            return out;
        }

        for entry in WalkDir::new(root).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(entry.path().to_path_buf());
            }
        }

        // Keep scan order deterministic across filesystems and runs.
        out.sort();
        out
    }

    /// Flatten OpenClaw content blocks into a single string.
    /// Content is an array of blocks: text, toolCall, thinking.
    fn flatten_openclaw_content(content: &Value) -> String {
        match content {
            Value::String(s) => s.clone(),
            Value::Array(arr) => {
                let parts: Vec<String> = arr
                    .iter()
                    .filter_map(|block| {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match block_type {
                            "text" => block.get("text").and_then(|t| t.as_str()).map(String::from),
                            "toolCall" => {
                                let name = block
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("tool_call");
                                Some(format!("[tool: {name}]"))
                            }
                            "thinking" => {
                                block.get("text").and_then(|t| t.as_str()).map(String::from)
                            }
                            _ => block.get("text").and_then(|t| t.as_str()).map(String::from),
                        }
                    })
                    .collect();
                parts.join("\n")
            }
            _ => flatten_content(content),
        }
    }
}

impl Connector for OpenClawConnector {
    fn detect(&self) -> DetectionResult {
        if let Some(agents_root) = Self::agents_root()
            && agents_root.exists()
            && agents_root.is_dir()
        {
            return Self::detect_from_agents_root(&agents_root);
        }

        if let Some(parent) = Self::openclaw_home()
            && parent.exists()
        {
            return DetectionResult {
                detected: true,
                evidence: vec![
                    format!("found {}", parent.display()),
                    "discovered 0 agent session dirs".to_string(),
                ],
                root_paths: Vec::new(),
            };
        }

        DetectionResult::not_found()
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let mut roots: Vec<PathBuf> = Vec::new();

        if ctx.use_default_detection() {
            if let Some(explicit) = Self::session_root_from_candidate(&ctx.data_dir)
                && Self::looks_like_openclaw_storage(&explicit)
                && explicit.exists()
            {
                roots.push(explicit);
            } else {
                roots.extend(Self::find_agent_session_dirs());
            }
        } else {
            for root in &ctx.scan_roots {
                roots.extend(Self::roots_from_scan_path(&root.path));
            }
        }

        roots.sort();
        roots.dedup();

        if roots.is_empty() {
            return Ok(Vec::new());
        }

        let mut convs = Vec::new();
        let mut scanned_agents = 0usize;

        for mut root in roots {
            if root.is_file() {
                root = root.parent().unwrap_or(&root).to_path_buf();
            }

            let agent_directory = Self::agent_directory_from_sessions_root(&root);
            let agent_slug = Self::agent_slug_for_directory(&agent_directory);
            let files = Self::session_files(&root);
            let mut agent_file_count = 0usize;
            let mut agent_session_count = 0usize;
            let mut agent_error_count = 0usize;
            tracing::debug!(
                agent = %agent_directory,
                file_count = files.len(),
                "openclaw: scanning agent directory"
            );
            for file in files {
                agent_file_count += 1;
                if !file_modified_since(&file, ctx.since_ts) {
                    continue;
                }

                let source_path = file.clone();
                let external_id = source_path
                    .strip_prefix(&root)
                    .ok()
                    .and_then(|rel| {
                        rel.with_extension("")
                            .to_str()
                            .map(std::string::ToString::to_string)
                    })
                    .or_else(|| {
                        source_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .map(std::string::ToString::to_string)
                    });

                let external_id = if agent_directory == "openclaw" {
                    external_id
                } else {
                    external_id.map(|id| format!("{agent_directory}/{id}"))
                };

                let file_handle = match fs::File::open(&file) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::debug!(path = %file.display(), error = %e, "openclaw: skipping unreadable session");
                        agent_error_count += 1;
                        continue;
                    }
                };
                let reader = std::io::BufReader::new(file_handle);

                let mut messages = Vec::new();
                let mut started_at: Option<i64> = None;
                let mut ended_at: Option<i64> = None;
                let mut session_cwd: Option<String> = None;

                for line_res in reader.lines() {
                    let line = match line_res {
                        Ok(l) => l,
                        Err(_) => continue,
                    };
                    if line.trim().is_empty() {
                        continue;
                    }

                    let val: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let line_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    match line_type {
                        "session" => {
                            // Extract session metadata
                            session_cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
                            if let Some(ts) = val.get("timestamp").and_then(parse_timestamp) {
                                started_at = Some(ts);
                            }
                        }
                        "message" => {
                            // Messages are wrapped: {type:"message", message:{role, content, ...}}
                            let msg = match val.get("message") {
                                Some(m) => m,
                                None => continue,
                            };

                            let role = msg
                                .get("role")
                                .and_then(|v| v.as_str())
                                .unwrap_or("assistant");

                            let content = msg
                                .get("content")
                                .map(Self::flatten_openclaw_content)
                                .unwrap_or_default();

                            if content.trim().is_empty() {
                                continue;
                            }

                            // Timestamps can be on the wrapper or inner message
                            let created = val
                                .get("timestamp")
                                .and_then(parse_timestamp)
                                .or_else(|| msg.get("timestamp").and_then(parse_timestamp));

                            started_at = match (started_at, created) {
                                (Some(curr), Some(ts)) => Some(curr.min(ts)),
                                (None, Some(ts)) => Some(ts),
                                (other, None) => other,
                            };
                            ended_at = match (ended_at, created) {
                                (Some(curr), Some(ts)) => Some(curr.max(ts)),
                                (None, Some(ts)) => Some(ts),
                                (other, None) => other,
                            };

                            let idx = i64::try_from(messages.len()).unwrap_or(i64::MAX);
                            messages.push(NormalizedMessage {
                                idx,
                                role: role.to_string(),
                                author: msg.get("model").and_then(|v| v.as_str()).map(String::from),
                                created_at: created,
                                content,
                                extra: val,
                                snippets: Vec::new(),
                            });
                        }
                        // Skip model_change, thinking_level_change, custom, etc.
                        _ => continue,
                    }
                }

                if messages.is_empty() {
                    continue;
                }

                let title = messages
                    .iter()
                    .find(|m| m.role == "user")
                    .map(|m| {
                        m.content
                            .lines()
                            .next()
                            .unwrap_or(&m.content)
                            .chars()
                            .take(100)
                            .collect::<String>()
                    })
                    .or_else(|| {
                        messages
                            .first()
                            .and_then(|m| m.content.lines().next())
                            .map(|s| s.chars().take(100).collect())
                    });

                let workspace = session_cwd.as_ref().map(PathBuf::from);

                let metadata = serde_json::json!({
                    "source": "openclaw",
                    "cwd": session_cwd,
                    "agent_directory": agent_directory.clone(),
                });

                convs.push(NormalizedConversation {
                    agent_slug: agent_slug.clone(),
                    external_id,
                    title,
                    workspace,
                    source_path,
                    started_at,
                    ended_at,
                    metadata,
                    messages,
                });
                agent_session_count += 1;
            }

            scanned_agents += 1;
            tracing::debug!(
                agent = %agent_directory,
                files = agent_file_count,
                sessions = agent_session_count,
                errors = agent_error_count,
                "openclaw: completed agent scan"
            );
        }

        tracing::debug!(
            agents = scanned_agents,
            sessions = convs.len(),
            "openclaw: completed multi-agent scan"
        );

        Ok(convs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_session(root: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = root.join(name);
        let content = lines.join("\n");
        fs::write(&path, content).unwrap();
        path
    }

    fn write_minimal_openclaw_session(
        sessions_root: &Path,
        file_name: &str,
        cwd: &str,
        user_text: &str,
    ) -> PathBuf {
        write_session(
            sessions_root,
            file_name,
            &[
                &format!(
                    r#"{{"type":"session","id":"s1","timestamp":"2026-02-01T16:00:00.000Z","cwd":"{cwd}"}}"#
                ),
                &format!(
                    r#"{{"type":"message","id":"m1","timestamp":"2026-02-01T16:00:01.000Z","message":{{"role":"user","content":[{{"type":"text","text":"{user_text}"}}]}}}}"#
                ),
            ],
        )
    }

    fn ctx_with_root(root: &Path) -> ScanContext {
        ScanContext::with_roots(
            root.to_path_buf(),
            vec![crate::connectors::ScanRoot::local(root.to_path_buf())],
            None,
        )
    }

    #[test]
    fn scan_parses_openclaw_wrapped_messages() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(
            &sessions,
            "session.jsonl",
            &[
                r#"{"type":"session","id":"abc","timestamp":"2026-02-01T16:00:00.000Z","cwd":"/home/user/project","version":"0.1.0"}"#,
                r#"{"type":"message","id":"m1","parentId":"abc","timestamp":"2026-02-01T16:00:00.828Z","message":{"role":"user","content":[{"type":"text","text":"Hello OpenClaw"}],"timestamp":1769961600827}}"#,
                r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-02-01T16:00:06.672Z","message":{"role":"assistant","content":[{"type":"text","text":"Hi there!"},{"type":"toolCall","id":"tc1","name":"exec","arguments":{}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-opus-4-5"}}"#,
            ],
        );

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "openclaw");
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].title, Some("Hello OpenClaw".to_string()));
        assert_eq!(convs[0].messages[0].role, "user");
        assert_eq!(convs[0].messages[1].role, "assistant");
        assert!(convs[0].messages[1].content.contains("Hi there!"));
        assert!(convs[0].messages[1].content.contains("[tool: exec]"));
        assert_eq!(
            convs[0].messages[1].author,
            Some("claude-opus-4-5".to_string())
        );
        assert!(convs[0].workspace.is_some());
        assert!(convs[0].started_at.is_some());
    }

    #[test]
    fn scan_skips_non_message_types() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(
            &sessions,
            "session2.jsonl",
            &[
                r#"{"type":"session","id":"s1","timestamp":"2026-02-01T16:00:00.000Z","cwd":"/"}"#,
                r#"{"type":"model_change","model":"gpt-5"}"#,
                r#"{"type":"thinking_level_change","level":"high"}"#,
                r#"{"type":"message","id":"m1","timestamp":"2026-02-01T16:00:01.000Z","message":{"role":"user","content":"Only message"}}"#,
                r#"{"type":"custom","data":"something"}"#,
            ],
        );

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Only message");
    }

    #[test]
    fn scan_handles_empty_and_invalid_lines() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(
            &sessions,
            "bad.jsonl",
            &[
                "",
                "not-json",
                r#"{"type":"message","id":"m1","timestamp":"2026-02-01T16:00:00.000Z","message":{"role":"user","content":"Valid"}}"#,
                r#"{"type":"message","id":"m2","message":{"role":"assistant","content":""}}"#,
            ],
        );

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        // Only the valid non-empty message should appear
        assert_eq!(convs[0].messages.len(), 1);
        assert_eq!(convs[0].messages[0].content, "Valid");
    }

    #[test]
    fn agents_root_path_construction() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                OpenClawConnector::agents_root().unwrap(),
                home.join(".openclaw").join("agents")
            );
        }
    }

    #[test]
    fn find_dirs_empty_root() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        fs::create_dir_all(&agents_root).unwrap();
        tracing::debug!("Scanning agents root: {}", agents_root.display());
        let dirs = OpenClawConnector::find_agent_session_dirs_at(&agents_root);
        assert!(dirs.is_empty());
    }

    #[test]
    fn find_dirs_no_sessions_subdir() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        fs::create_dir_all(agents_root.join("alice")).unwrap();
        let dirs = OpenClawConnector::find_agent_session_dirs_at(&agents_root);
        assert!(dirs.is_empty());
    }

    #[test]
    fn find_dirs_one_agent() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        let alice = agents_root.join("alice").join("sessions");
        fs::create_dir_all(&alice).unwrap();

        let dirs = OpenClawConnector::find_agent_session_dirs_at(&agents_root);
        assert_eq!(dirs, vec![alice]);
    }

    #[test]
    fn find_dirs_multiple_agents_sorted() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        fs::create_dir_all(agents_root.join("charlie").join("sessions")).unwrap();
        fs::create_dir_all(agents_root.join("alice").join("sessions")).unwrap();
        fs::create_dir_all(agents_root.join("bob").join("sessions")).unwrap();

        let dirs = OpenClawConnector::find_agent_session_dirs_at(&agents_root);
        let discovered: Vec<String> = dirs
            .iter()
            .filter_map(|p| {
                p.parent()
                    .and_then(|pp| pp.file_name())
                    .and_then(|n| n.to_str())
                    .map(String::from)
            })
            .collect();
        assert_eq!(
            discovered,
            vec![
                "alice".to_string(),
                "bob".to_string(),
                "charlie".to_string()
            ]
        );
    }

    #[test]
    fn find_dirs_max_depth_ignores_deep_nesting() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        fs::create_dir_all(agents_root.join("alice").join("sessions")).unwrap();
        fs::create_dir_all(
            agents_root
                .join("nested")
                .join("too")
                .join("deep")
                .join("sessions"),
        )
        .unwrap();

        let dirs = OpenClawConnector::find_agent_session_dirs_at(&agents_root);
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].to_string_lossy().contains(&format!(
            "{}alice{}",
            std::path::MAIN_SEPARATOR,
            std::path::MAIN_SEPARATOR
        )));
    }

    #[test]
    fn session_files_are_sorted_for_deterministic_scan_order() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_session(
            &sessions,
            "z-last.jsonl",
            &[r#"{"type":"message","message":{"role":"user","content":"z"}}"#],
        );
        write_session(
            &sessions,
            "a-first.jsonl",
            &[r#"{"type":"message","message":{"role":"user","content":"a"}}"#],
        );

        let files = OpenClawConnector::session_files(&sessions);
        let file_names: Vec<String> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();

        assert_eq!(
            file_names,
            vec!["a-first.jsonl".to_string(), "z-last.jsonl".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_dirs_symlink_skipped() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        let real_agent = tmp.path().join("real_alice");
        fs::create_dir_all(real_agent.join("sessions")).unwrap();
        fs::create_dir_all(&agents_root).unwrap();
        symlink(&real_agent, agents_root.join("alice_link")).unwrap();

        let dirs = OpenClawConnector::find_agent_session_dirs_at(&agents_root);
        assert!(dirs.is_empty());
    }

    #[test]
    fn detect_reports_agent_names() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        fs::create_dir_all(agents_root.join("alice").join("sessions")).unwrap();
        fs::create_dir_all(agents_root.join("bob").join("sessions")).unwrap();

        let detection = OpenClawConnector::detect_from_agents_root(&agents_root);
        assert!(detection.detected);
        assert_eq!(detection.root_paths.len(), 2);
        let joined = detection.evidence.join(" | ");
        assert!(joined.contains("discovered 2 agent session dirs"));
        assert!(joined.contains("alice"));
        assert!(joined.contains("bob"));
    }

    #[test]
    fn detect_zero_agents() {
        let tmp = TempDir::new().unwrap();
        let agents_root = tmp.path().join("agents");
        fs::create_dir_all(&agents_root).unwrap();

        let detection = OpenClawConnector::detect_from_agents_root(&agents_root);
        assert!(detection.detected);
        assert!(detection.root_paths.is_empty());
        assert!(
            detection
                .evidence
                .iter()
                .any(|line| line.contains("discovered 0 agent session dirs"))
        );
    }

    #[test]
    fn scan_multiple_agents() {
        let tmp = TempDir::new().unwrap();
        let alice_sessions = tmp.path().join(".openclaw/agents/alice/sessions");
        let bob_sessions = tmp.path().join(".openclaw/agents/bob/sessions");
        fs::create_dir_all(&alice_sessions).unwrap();
        fs::create_dir_all(&bob_sessions).unwrap();
        write_minimal_openclaw_session(&alice_sessions, "alice.jsonl", "/tmp/alice", "hello alice");
        write_minimal_openclaw_session(&bob_sessions, "bob.jsonl", "/tmp/bob", "hello bob");

        let connector = OpenClawConnector::new();
        let ctx = ctx_with_root(tmp.path());
        let mut convs = connector.scan(&ctx).unwrap();
        convs.sort_by(|a, b| a.agent_slug.cmp(&b.agent_slug));

        assert_eq!(convs.len(), 2);
        assert_eq!(convs[0].agent_slug, "openclaw/alice");
        assert_eq!(convs[1].agent_slug, "openclaw/bob");
    }

    #[test]
    fn scan_agent_identity_preserved() {
        let tmp = TempDir::new().unwrap();
        let alice_sessions = tmp.path().join(".openclaw/agents/alice/sessions");
        fs::create_dir_all(&alice_sessions).unwrap();
        write_minimal_openclaw_session(&alice_sessions, "s1.jsonl", "/tmp/alice", "from alice");

        let connector = OpenClawConnector::new();
        let ctx = ctx_with_root(tmp.path());
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "openclaw/alice");
        assert_eq!(convs[0].external_id.as_deref(), Some("alice/s1"));
    }

    #[test]
    fn scan_agent_metadata_present() {
        let tmp = TempDir::new().unwrap();
        let alice_sessions = tmp.path().join(".openclaw/agents/alice/sessions");
        fs::create_dir_all(&alice_sessions).unwrap();
        write_minimal_openclaw_session(
            &alice_sessions,
            "meta.jsonl",
            "/tmp/alice",
            "metadata check",
        );

        let connector = OpenClawConnector::new();
        let ctx = ctx_with_root(tmp.path());
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0]
                .metadata
                .get("agent_directory")
                .and_then(|v| v.as_str()),
            Some("alice")
        );
    }

    #[test]
    fn scan_mixed_valid_invalid_across_agents() {
        let tmp = TempDir::new().unwrap();
        let alice_sessions = tmp.path().join(".openclaw/agents/alice/sessions");
        let bob_sessions = tmp.path().join(".openclaw/agents/bob/sessions");
        fs::create_dir_all(&alice_sessions).unwrap();
        fs::create_dir_all(&bob_sessions).unwrap();
        write_session(
            &alice_sessions,
            "bad.jsonl",
            &["not-json", "still-not-json"],
        );
        write_minimal_openclaw_session(&bob_sessions, "good.jsonl", "/tmp/bob", "valid from bob");

        let connector = OpenClawConnector::new();
        let ctx = ctx_with_root(tmp.path());
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "openclaw/bob");
    }

    #[test]
    fn scan_single_agent_unchanged_slug() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join(".openclaw/agents/openclaw/sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_minimal_openclaw_session(&sessions, "single.jsonl", "/tmp/openclaw", "legacy mode");

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::local_default(sessions.clone(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "openclaw");
        assert_eq!(convs[0].external_id.as_deref(), Some("single"));
        assert_eq!(
            convs[0]
                .metadata
                .get("agent_directory")
                .and_then(|v| v.as_str()),
            Some("openclaw")
        );
    }

    #[test]
    fn scan_with_explicit_agent_root_path() {
        let tmp = TempDir::new().unwrap();
        let agent_root = tmp.path().join(".openclaw/agents/alice");
        let sessions = agent_root.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        write_minimal_openclaw_session(&sessions, "root.jsonl", "/tmp/alice", "explicit root");

        let connector = OpenClawConnector::new();
        let ctx = ScanContext::with_roots(
            tmp.path().to_path_buf(),
            vec![crate::connectors::ScanRoot::local(agent_root)],
            None,
        );
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "openclaw/alice");
    }
}
