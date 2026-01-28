//! Beats connector for btv insight files.
//!
//! Beats stores insights at `.beats/beats.jsonl` in various repositories.
//! Each line is a JSON object with fields:
//!   - id: unique identifier
//!   - created_at: ISO timestamp
//!   - content: the insight text
//!   - impetus: { label, id } - what triggered the beat
//!   - entities: array of topic strings
//!   - references: array of reference objects
//!   - session_id: optional session linkage

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::connectors::{
    Connector, DetectionResult, NormalizedConversation, NormalizedMessage, ScanContext,
};

pub struct BeatsConnector;

impl Default for BeatsConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl BeatsConnector {
    pub fn new() -> Self {
        Self
    }

    /// Find all .beats/beats.jsonl files under common locations.
    fn find_beats_files() -> Vec<PathBuf> {
        let mut files = Vec::new();

        // Check home directory and common workspace roots
        if let Some(home) = dirs::home_dir() {
            // Check ~/werk and subdirectories
            let werk_dir = home.join("werk");
            if werk_dir.exists() {
                files.extend(find_beats_in_dir(&werk_dir));
            }

            // Check home-level .beats
            let home_beats = home.join(".beats/beats.jsonl");
            if home_beats.exists() {
                files.push(home_beats);
            }
        }

        files
    }
}

/// Find all .beats/beats.jsonl files under a directory.
fn find_beats_in_dir(dir: &Path) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .max_depth(5)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            let path = e.path();
            path.file_name() == Some("beats.jsonl".as_ref())
                && path.parent().and_then(|p| p.file_name()) == Some(".beats".as_ref())
        })
        .map(|e| e.path().to_path_buf())
        .collect()
}

// ============================================================================
// JSON Structures for Beats Storage
// ============================================================================

#[derive(Debug, Deserialize)]
struct Beat {
    id: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    impetus: Option<BeatImpetus>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    references: Vec<serde_json::Value>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
struct BeatImpetus {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

impl Connector for BeatsConnector {
    fn detect(&self) -> DetectionResult {
        let files = Self::find_beats_files();
        if !files.is_empty() {
            DetectionResult {
                detected: true,
                evidence: files.iter().map(|p| format!("found {}", p.display())).collect(),
                root_paths: files,
            }
        } else {
            DetectionResult::not_found()
        }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        // Determine which files to scan
        let files = if ctx.use_default_detection() {
            // Check if data_dir itself is a beats file or contains one
            if ctx.data_dir.is_file() && looks_like_beats_file(&ctx.data_dir) {
                vec![ctx.data_dir.clone()]
            } else if ctx.data_dir.is_dir() {
                let mut found = find_beats_in_dir(&ctx.data_dir);
                if found.is_empty() {
                    found = Self::find_beats_files();
                }
                found
            } else {
                Self::find_beats_files()
            }
        } else if ctx.data_dir.is_file() && looks_like_beats_file(&ctx.data_dir) {
            vec![ctx.data_dir.clone()]
        } else if ctx.data_dir.is_dir() {
            find_beats_in_dir(&ctx.data_dir)
        } else {
            Vec::new()
        };

        let mut convs = Vec::new();

        for file in files {
            if !crate::connectors::file_modified_since(&file, ctx.since_ts) {
                continue;
            }

            // Derive workspace from the .beats directory's parent
            let workspace = file
                .parent() // .beats
                .and_then(|p| p.parent()) // actual workspace
                .map(PathBuf::from);

            // Parse the JSONL file into beats
            let beats = match load_beats(&file) {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!("beats: failed to parse {}: {e}", file.display());
                    continue;
                }
            };

            if beats.is_empty() {
                continue;
            }

            // Group beats by session_id if present, otherwise create one conversation per file
            let grouped = group_beats_by_session(beats);

            for (session_id, session_beats) in grouped {
                let messages = beats_to_messages(session_beats);
                if messages.is_empty() {
                    continue;
                }

                // Timestamps from first/last beat
                let started_at = messages.first().and_then(|m| m.created_at);
                let ended_at = messages.last().and_then(|m| m.created_at);

                // Title from first beat content
                let title = messages
                    .first()
                    .map(|m| m.content.lines().next().unwrap_or("").chars().take(100).collect());

                let external_id = session_id.clone().or_else(|| {
                    // Use file path hash as fallback ID
                    Some(format!("beats-{}", hash_path(&file)))
                });

                convs.push(NormalizedConversation {
                    agent_slug: "beats".into(),
                    external_id,
                    title,
                    workspace: workspace.clone(),
                    source_path: file.clone(),
                    started_at,
                    ended_at,
                    metadata: serde_json::json!({
                        "session_id": session_id,
                    }),
                    messages,
                });
            }
        }

        Ok(convs)
    }
}

/// Check if a path looks like a beats file.
fn looks_like_beats_file(path: &Path) -> bool {
    path.file_name() == Some("beats.jsonl".as_ref())
}

/// Simple hash for path-based IDs.
fn hash_path(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Load beats from a JSONL file.
fn load_beats(path: &Path) -> Result<Vec<Beat>> {
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut beats = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<Beat>(&line) {
            Ok(beat) => beats.push(beat),
            Err(_) => continue,
        }
    }

    Ok(beats)
}

/// Group beats by session_id. Beats without session_id go into None group.
fn group_beats_by_session(beats: Vec<Beat>) -> Vec<(Option<String>, Vec<Beat>)> {
    use std::collections::HashMap;

    let mut groups: HashMap<Option<String>, Vec<Beat>> = HashMap::new();

    for beat in beats {
        groups
            .entry(beat.session_id.clone())
            .or_default()
            .push(beat);
    }

    groups.into_iter().collect()
}

/// Convert beats to normalized messages.
fn beats_to_messages(beats: Vec<Beat>) -> Vec<NormalizedMessage> {
    let mut messages: Vec<NormalizedMessage> = beats
        .into_iter()
        .filter_map(|beat| {
            let content = beat.content.as_ref()?;
            if content.trim().is_empty() {
                return None;
            }

            let created_at = beat
                .created_at
                .as_ref()
                .and_then(|s| crate::connectors::parse_timestamp(&serde_json::json!(s)));

            // Build searchable content including entities as tags
            let mut full_content = content.clone();
            if !beat.entities.is_empty() {
                full_content.push_str("\n\nTags: ");
                full_content.push_str(&beat.entities.join(", "));
            }

            Some(NormalizedMessage {
                idx: 0, // Will be reindexed
                role: "user".into(), // Beats are user-authored insights
                author: beat.impetus.as_ref().and_then(|i| i.label.clone()),
                created_at,
                content: full_content,
                extra: serde_json::json!({
                    "beat_id": beat.id,
                    "impetus": beat.impetus,
                    "entities": beat.entities,
                    "references": beat.references,
                }),
                snippets: Vec::new(),
            })
        })
        .collect();

    // Sort by timestamp if available
    messages.sort_by_key(|m| m.created_at);

    // Reindex
    crate::connectors::reindex_messages(&mut messages);

    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn create_beats_storage(dir: &TempDir) -> PathBuf {
        let beats_dir = dir.path().join(".beats");
        fs::create_dir_all(&beats_dir).unwrap();
        beats_dir.join("beats.jsonl")
    }

    fn write_beats(path: &Path, beats: &[serde_json::Value]) {
        let content: String = beats
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(path, content).unwrap();
    }

    #[test]
    fn new_creates_connector() {
        let connector = BeatsConnector::new();
        let _ = connector;
    }

    #[test]
    fn scan_parses_simple_beats() {
        let dir = TempDir::new().unwrap();
        let beats_file = create_beats_storage(&dir);

        let beats = vec![
            json!({
                "id": "beat-1",
                "created_at": "2026-01-15T10:30:00Z",
                "content": "First insight",
                "entities": ["topic1", "topic2"],
                "references": []
            }),
            json!({
                "id": "beat-2",
                "created_at": "2026-01-15T11:00:00Z",
                "content": "Second insight",
                "entities": ["topic1"],
                "references": []
            }),
        ];
        write_beats(&beats_file, &beats);

        let connector = BeatsConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].agent_slug, "beats");
        assert_eq!(convs[0].messages.len(), 2);
        assert!(convs[0].messages[0].content.contains("First insight"));
        assert!(convs[0].messages[0].content.contains("Tags: topic1, topic2"));
    }

    #[test]
    fn scan_groups_by_session() {
        let dir = TempDir::new().unwrap();
        let beats_file = create_beats_storage(&dir);

        let beats = vec![
            json!({
                "id": "beat-1",
                "content": "Session A beat 1",
                "session_id": "session-a",
                "entities": []
            }),
            json!({
                "id": "beat-2",
                "content": "Session B beat",
                "session_id": "session-b",
                "entities": []
            }),
            json!({
                "id": "beat-3",
                "content": "Session A beat 2",
                "session_id": "session-a",
                "entities": []
            }),
        ];
        write_beats(&beats_file, &beats);

        let connector = BeatsConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 2);
        // Verify we have both sessions
        let session_ids: Vec<_> = convs.iter().filter_map(|c| c.metadata.get("session_id").and_then(|v| v.as_str())).collect();
        assert!(session_ids.contains(&"session-a"));
        assert!(session_ids.contains(&"session-b"));
    }

    #[test]
    fn scan_handles_impetus() {
        let dir = TempDir::new().unwrap();
        let beats_file = create_beats_storage(&dir);

        let beats = vec![json!({
            "id": "beat-1",
            "content": "Session insight",
            "impetus": {"label": "session", "id": "xyz"},
            "entities": []
        })];
        write_beats(&beats_file, &beats);

        let connector = BeatsConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs[0].messages[0].author, Some("session".to_string()));
    }

    #[test]
    fn scan_handles_empty_file() {
        let dir = TempDir::new().unwrap();
        let beats_file = create_beats_storage(&dir);
        fs::write(&beats_file, "").unwrap();

        let connector = BeatsConnector::new();
        let ctx = ScanContext::local_default(dir.path().to_path_buf(), None);
        let convs = connector.scan(&ctx).unwrap();

        assert_eq!(convs.len(), 0);
    }

    #[test]
    fn looks_like_beats_file_works() {
        assert!(looks_like_beats_file(Path::new("/path/.beats/beats.jsonl")));
        assert!(!looks_like_beats_file(Path::new("/path/other.jsonl")));
    }
}
