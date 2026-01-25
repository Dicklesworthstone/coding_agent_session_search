//! Smart filename generation for HTML exports.
//!
//! Generates cross-platform safe filenames from session metadata,
//! ensuring compatibility with Windows, macOS, and Linux filesystems.
//!
//! # Features
//!
//! - **Cross-platform safety**: Handles Windows reserved names, invalid characters
//! - **Smart downloads detection**: Finds platform-specific downloads folder
//! - **Collision handling**: Automatic numeric suffixes for duplicate names
//! - **Agent normalization**: Canonical slugs for all supported agents
//! - **Topic support**: Robot mode can specify intelligent topic names

use std::path::{Path, PathBuf};

/// Options for filename generation.
#[derive(Debug, Clone, Default)]
pub struct FilenameOptions {
    /// Include date in filename
    pub include_date: bool,

    /// Include agent name in filename
    pub include_agent: bool,

    /// Include project name in filename
    pub include_project: bool,

    /// Include topic in filename (if provided)
    pub include_topic: bool,

    /// Maximum filename length (excluding extension)
    pub max_length: Option<usize>,

    /// Custom prefix
    pub prefix: Option<String>,

    /// Custom suffix (before extension)
    pub suffix: Option<String>,
}

/// Metadata for filename generation.
#[derive(Debug, Clone, Default)]
pub struct FilenameMetadata {
    /// Session title or ID
    pub title: Option<String>,

    /// ISO date (YYYY-MM-DD)
    pub date: Option<String>,

    /// Agent name (claude, codex, etc.)
    pub agent: Option<String>,

    /// Project name
    pub project: Option<String>,

    /// Topic provided by calling agent (robot mode).
    /// Will be normalized to lowercase with underscores.
    pub topic: Option<String>,
}

/// Normalize a topic string to lowercase with underscores.
///
/// This is the canonical way to convert a user-provided topic
/// into the format expected by CASS filenames:
/// - Converts to lowercase
/// - Replaces spaces with underscores
/// - Removes invalid characters
/// - Collapses multiple underscores
///
/// # Examples
/// ```
/// use cass::html_export::filename::normalize_topic;
/// assert_eq!(normalize_topic("My Cool Topic"), "my_cool_topic");
/// assert_eq!(normalize_topic("HTML Export Feature"), "html_export_feature");
/// ```
pub fn normalize_topic(topic: &str) -> String {
    sanitize(topic)
}

/// Generate a safe, descriptive filename.
///
/// Returns a filename without extension (add .html manually).
pub fn generate_filename(metadata: &FilenameMetadata, options: &FilenameOptions) -> String {
    let mut parts = Vec::new();

    // Add prefix
    if let Some(prefix) = &options.prefix {
        push_part(&mut parts, prefix);
    }

    // Add date
    if options.include_date
        && let Some(date) = &metadata.date
    {
        push_part(&mut parts, date);
    }

    // Add agent
    if options.include_agent
        && let Some(agent) = &metadata.agent
    {
        push_part(&mut parts, agent);
    }

    // Add project
    if options.include_project
        && let Some(project) = &metadata.project
    {
        push_part(&mut parts, project);
    }

    // Add topic (robot mode can supply this for intelligent naming)
    if options.include_topic
        && let Some(topic) = &metadata.topic
    {
        let normalized = normalize_topic(topic);
        if !normalized.is_empty() {
            parts.push(normalized);
        }
    }

    // Add title (always included if present)
    if let Some(title) = &metadata.title {
        push_part(&mut parts, title);
    }

    // Add suffix
    if let Some(suffix) = &options.suffix {
        push_part(&mut parts, suffix);
    }

    // Combine parts
    let filename = if parts.is_empty() {
        "session".to_string()
    } else {
        parts.join("_")
    };

    finalize_filename(filename, options.max_length)
}

/// Generate a filename with path.
pub fn generate_filepath(
    base_dir: &std::path::Path,
    metadata: &FilenameMetadata,
    options: &FilenameOptions,
) -> PathBuf {
    let ext = ".html";
    let base_max = MAX_FILENAME_LEN.saturating_sub(ext.len());
    let mut adjusted = options.clone();
    adjusted.max_length = Some(match options.max_length {
        Some(user_max) => user_max.min(base_max).max(1),
        None => base_max,
    });
    let filename = generate_filename(metadata, &adjusted);
    base_dir.join(format!("{filename}{ext}"))
}

/// Sanitize a string for use in filenames.
///
/// - Replaces invalid characters with underscores
/// - Removes leading/trailing whitespace
/// - Collapses multiple underscores
/// - Limits to ASCII alphanumeric plus safe punctuation
fn sanitize(s: &str) -> String {
    let mut result = String::new();
    let mut last_was_underscore = false;

    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' {
            result.push(c.to_ascii_lowercase());
            last_was_underscore = false;
        } else if c == ' ' || c == '_' || c == '.' || c == '/' || c == '\\' {
            // Replace separators with underscore, avoiding duplicates
            if !last_was_underscore && !result.is_empty() {
                result.push('_');
                last_was_underscore = true;
            }
        }
        // Skip other characters
    }

    // Trim leading/trailing underscores
    result.trim_matches('_').to_string()
}

/// Push a sanitized part if it is non-empty.
fn push_part(parts: &mut Vec<String>, raw: &str) {
    let sanitized = sanitize(raw);
    if !sanitized.is_empty() {
        parts.push(sanitized);
    }
}

const MAX_FILENAME_LEN: usize = 255;

/// Finalize a filename by enforcing length limits and avoiding reserved names.
fn finalize_filename(mut name: String, max_len: Option<usize>) -> String {
    if name.is_empty() {
        name = "session".to_string();
    }

    name = trim_separators(&name);
    if name.is_empty() {
        name = "session".to_string();
    }

    name = enforce_max_len(name, max_len);
    name = avoid_reserved_name(name);
    name = enforce_max_len(name, max_len);

    name = trim_separators(&name);
    if name.is_empty() {
        "session".to_string()
    } else {
        name
    }
}

fn enforce_max_len(mut name: String, max_len: Option<usize>) -> String {
    let limit = max_len
        .unwrap_or(MAX_FILENAME_LEN)
        .clamp(1, MAX_FILENAME_LEN);
    if name.len() > limit {
        // Safe truncation at char boundary to avoid panic on multi-byte UTF-8
        let safe_limit = truncate_to_char_boundary(&name, limit);
        name.truncate(safe_limit);
        name = trim_separators(&name);
    }
    name
}

/// Find the largest byte index <= `max_bytes` that is on a UTF-8 char boundary.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    // Walk backwards from max_bytes to find a char boundary
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn trim_separators(name: &str) -> String {
    name.trim_matches(|c| c == '_' || c == '-').to_string()
}

fn avoid_reserved_name(name: String) -> String {
    if is_reserved_basename(&name) {
        format!("session_{}", name)
    } else {
        name
    }
}

fn is_reserved_basename(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let base_name = upper.split('.').next().unwrap_or(&upper);
    RESERVED_NAMES.contains(&base_name)
}

/// Characters that are invalid in filenames across platforms.
const INVALID_CHARS: &[char] = &[
    '<', '>', ':', '"', '/', '\\', '|', '?', '*', '\0', '\n', '\r', '\t',
];

/// Reserved filenames on Windows.
const RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Check if a filename is valid across platforms.
pub fn is_valid_filename(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }

    // Check for invalid characters
    if name.chars().any(|c| INVALID_CHARS.contains(&c)) {
        return false;
    }

    // Check for reserved names (Windows)
    let upper = name.to_ascii_uppercase();
    let base_name = upper.split('.').next().unwrap_or(&upper);
    if RESERVED_NAMES.contains(&base_name) {
        return false;
    }

    // Check for leading/trailing spaces or dots
    if name.starts_with(' ') || name.starts_with('.') || name.ends_with(' ') || name.ends_with('.')
    {
        return false;
    }

    // Check length (Windows MAX_PATH is 260, but NTFS supports 255 per component)
    if name.len() > 255 {
        return false;
    }

    true
}

// ============================================================================
// Platform-specific downloads folder detection
// ============================================================================

/// Get the platform-specific downloads directory.
///
/// Falls back through multiple strategies:
/// 1. Platform downloads dir (XDG on Linux, known folder on Windows)
/// 2. Home directory + "Downloads"
/// 3. Current working directory
pub fn get_downloads_dir() -> PathBuf {
    // Primary: Platform-specific downloads
    if let Some(downloads) = dirs::download_dir() {
        return downloads;
    }

    // Fallback 1: Home + Downloads
    if let Some(home) = dirs::home_dir() {
        let fallback = home.join("Downloads");
        if fallback.exists() {
            return fallback;
        }
    }

    // Fallback 2: Current directory
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Generate a unique filename that doesn't collide with existing files.
///
/// If the base filename exists, appends numeric suffixes: `file_1.html`, `file_2.html`, etc.
/// As an ultimate fallback, appends a timestamp.
pub fn unique_filename(dir: &Path, base_filename: &str) -> PathBuf {
    let path = dir.join(base_filename);
    if !path.exists() {
        return path;
    }

    // Extract stem and extension
    let (stem, ext) = if let Some(dot_pos) = base_filename.rfind('.') {
        (&base_filename[..dot_pos], &base_filename[dot_pos..])
    } else {
        (base_filename, "")
    };

    // Try numeric suffixes
    for i in 1..1000 {
        let new_name = format!("{}_{}{}", stem, i, ext);
        let new_path = dir.join(&new_name);
        if !new_path.exists() {
            return new_path;
        }
    }

    // Ultimate fallback: timestamp
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("{}_{}{}", stem, ts, ext))
}

// ============================================================================
// Agent slug normalization
// ============================================================================

/// Normalize agent name to canonical slug.
///
/// Maps various agent name formats to a consistent short form.
pub fn agent_slug(agent: &str) -> String {
    match agent.to_lowercase().replace(['-', '_'], "").as_str() {
        "claudecode" | "claude" => "claude".to_string(),
        "cursor" | "cursorai" => "cursor".to_string(),
        "chatgpt" | "gpt" | "openai" => "chatgpt".to_string(),
        "gemini" | "geminicli" | "google" => "gemini".to_string(),
        "codex" | "codexcli" => "codex".to_string(),
        "aider" => "aider".to_string(),
        "piagent" | "pi" => "piagent".to_string(),
        "factory" | "droid" => "factory".to_string(),
        "opencode" => "opencode".to_string(),
        "cline" => "cline".to_string(),
        "amp" => "amp".to_string(),
        "copilot" | "githubcopilot" => "copilot".to_string(),
        "cody" | "sourcegraph" => "cody".to_string(),
        "windsurf" => "windsurf".to_string(),
        "grok" => "grok".to_string(),
        other => {
            // Slugify unknown agents
            let slug = sanitize(other);
            if slug.len() > 15 {
                // Safe truncation at char boundary to avoid panic
                let safe_end = truncate_to_char_boundary(&slug, 15);
                slug[..safe_end].trim_end_matches('_').to_string()
            } else {
                slug
            }
        }
    }
}

/// Extract workspace/project name from a path.
///
/// Returns the last path component as a slug, or "standalone" if no workspace.
pub fn workspace_slug(workspace: Option<&Path>) -> String {
    match workspace {
        Some(path) => {
            // Get last component (project name)
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let slug = sanitize(name);
            if slug.len() > 20 {
                // Safe truncation at char boundary to avoid panic
                let safe_end = truncate_to_char_boundary(&slug, 20);
                slug[..safe_end].trim_end_matches('_').to_string()
            } else if slug.is_empty() {
                "project".to_string()
            } else {
                slug
            }
        }
        None => "standalone".to_string(),
    }
}

/// Format a Unix timestamp as a filename-safe datetime string.
///
/// Output format: `YYYY_MM_DD_HHMM` (e.g., `2026_01_25_1430`)
pub fn datetime_slug(timestamp_ms: Option<i64>) -> String {
    use chrono::{TimeZone, Utc};

    let dt = timestamp_ms
        .and_then(|ts| Utc.timestamp_millis_opt(ts).single())
        .unwrap_or_else(Utc::now);

    dt.format("%Y_%m_%d_%H%M").to_string()
}

/// Extract a topic from conversation content.
///
/// Priority order:
/// 1. Explicit title (if provided)
/// 2. First user message (truncated, cleaned)
/// 3. Fallback to "session"
pub fn extract_topic(title: Option<&str>, first_user_message: Option<&str>) -> String {
    // Priority 1: Explicit title
    if let Some(t) = title {
        let topic = sanitize(t);
        if !topic.is_empty() {
            return truncate_topic(&topic, 30);
        }
    }

    // Priority 2: First user message
    if let Some(msg) = first_user_message {
        // Extract meaningful words, skip code/urls
        let words: Vec<&str> = msg
            .split_whitespace()
            .filter(|w| !w.starts_with("http"))
            .filter(|w| !w.contains('/'))
            .filter(|w| !w.starts_with('`'))
            .filter(|w| w.len() < 20)
            .take(5)
            .collect();

        if !words.is_empty() {
            let topic = sanitize(&words.join(" "));
            if !topic.is_empty() {
                return truncate_topic(&topic, 30);
            }
        }
    }

    // Fallback
    "session".to_string()
}

/// Truncate a topic to max length at word boundaries.
fn truncate_topic(topic: &str, max_len: usize) -> String {
    if topic.len() <= max_len {
        return topic.to_string();
    }

    // Safe truncation at char boundary to avoid panic on multi-byte UTF-8
    let safe_end = truncate_to_char_boundary(topic, max_len);
    let truncated = &topic[..safe_end];

    // Try to truncate at underscore boundary for cleaner result
    if let Some(last_underscore) = truncated.rfind('_')
        && last_underscore > safe_end / 2
    {
        return truncated[..last_underscore].to_string();
    }

    truncated.trim_end_matches('_').to_string()
}

/// Generate a complete filename with all components.
///
/// Format: `{agent}_{workspace}_{datetime}_{topic}.html`
pub fn generate_full_filename(
    agent: &str,
    workspace: Option<&Path>,
    timestamp_ms: Option<i64>,
    title: Option<&str>,
    first_user_message: Option<&str>,
) -> String {
    let agent_part = agent_slug(agent);
    let workspace_part = workspace_slug(workspace);
    let datetime_part = datetime_slug(timestamp_ms);
    let topic_part = extract_topic(title, first_user_message);

    let ext = ".html";
    let base_max = MAX_FILENAME_LEN.saturating_sub(ext.len());
    let base = format!(
        "{}_{}_{}_{}",
        agent_part, workspace_part, datetime_part, topic_part
    );
    let base = finalize_filename(base, Some(base_max));
    format!("{base}{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_basic() {
        assert_eq!(sanitize("Hello World"), "hello_world");
        assert_eq!(sanitize("test.file"), "test_file");
        assert_eq!(sanitize("path/to/file"), "path_to_file");
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize("file<>:name"), "filename");
        assert_eq!(sanitize("test?*file"), "testfile");
    }

    #[test]
    fn test_sanitize_multiple_separators() {
        assert_eq!(sanitize("hello   world"), "hello_world");
        assert_eq!(sanitize("test___file"), "test_file");
    }

    #[test]
    fn test_generate_filename_basic() {
        let meta = FilenameMetadata {
            title: Some("My Session".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions::default();

        assert_eq!(generate_filename(&meta, &opts), "my_session");
    }

    #[test]
    fn test_generate_filename_with_date() {
        let meta = FilenameMetadata {
            title: Some("Session".to_string()),
            date: Some("2026-01-25".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions {
            include_date: true,
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        assert!(result.starts_with("2026-01-25"));
        assert!(result.contains("session"));
    }

    #[test]
    fn test_generate_filename_max_length() {
        let meta = FilenameMetadata {
            title: Some("A very long session title that exceeds limits".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions {
            max_length: Some(20),
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        assert!(result.len() <= 20);
    }

    #[test]
    fn test_generate_filename_zero_max_length() {
        let meta = FilenameMetadata {
            title: Some("Any Title".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions {
            max_length: Some(0),
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        assert!(!result.is_empty());
        assert!(result.len() <= 1);
    }

    #[test]
    fn test_generate_filename_caps_at_platform_limit() {
        let meta = FilenameMetadata {
            title: Some("a".repeat(400)),
            ..Default::default()
        };
        let opts = FilenameOptions {
            max_length: Some(400),
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        assert!(result.len() <= MAX_FILENAME_LEN);
    }

    #[test]
    fn test_generate_filename_empty() {
        let meta = FilenameMetadata::default();
        let opts = FilenameOptions::default();

        assert_eq!(generate_filename(&meta, &opts), "session");
    }

    #[test]
    fn test_generate_filename_skips_empty_parts() {
        let meta = FilenameMetadata {
            title: Some("Valid Session".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions {
            prefix: Some("###".to_string()),
            ..Default::default()
        };

        assert_eq!(generate_filename(&meta, &opts), "valid_session");
    }

    #[test]
    fn test_generate_filename_all_invalid() {
        let meta = FilenameMetadata {
            title: Some("###".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions::default();

        assert_eq!(generate_filename(&meta, &opts), "session");
    }

    #[test]
    fn test_generate_filename_reserved_name() {
        let meta = FilenameMetadata {
            title: Some("CON".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions::default();

        assert_eq!(generate_filename(&meta, &opts), "session_con");
    }

    #[test]
    fn test_is_valid_filename() {
        assert!(is_valid_filename("valid_file.txt"));
        assert!(is_valid_filename("test-123"));

        assert!(!is_valid_filename(""));
        assert!(!is_valid_filename("file<name"));
        assert!(!is_valid_filename("CON")); // Reserved on Windows
        assert!(!is_valid_filename(".hidden")); // Leading dot
    }

    #[test]
    fn test_generate_filepath() {
        let meta = FilenameMetadata {
            title: Some("test".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions::default();
        let path = generate_filepath(std::path::Path::new("/tmp"), &meta, &opts);

        assert_eq!(path, PathBuf::from("/tmp/test.html"));
    }

    #[test]
    fn test_generate_filepath_respects_extension_limit() {
        let meta = FilenameMetadata {
            title: Some("a".repeat(300)),
            ..Default::default()
        };
        let opts = FilenameOptions::default();
        let path = generate_filepath(std::path::Path::new("/tmp"), &meta, &opts);
        let filename = path.file_name().unwrap().to_string_lossy();
        assert!(filename.len() <= MAX_FILENAME_LEN);
        assert!(filename.ends_with(".html"));
    }

    #[test]
    fn test_normalize_topic_basic() {
        assert_eq!(normalize_topic("My Cool Topic"), "my_cool_topic");
        assert_eq!(
            normalize_topic("HTML Export Feature"),
            "html_export_feature"
        );
        assert_eq!(
            normalize_topic("debugging auth flow"),
            "debugging_auth_flow"
        );
    }

    #[test]
    fn test_normalize_topic_special_chars() {
        // Special characters should be removed
        assert_eq!(normalize_topic("API Design (v2)"), "api_design_v2");
        assert_eq!(normalize_topic("fix: login bug"), "fix_login_bug");
        assert_eq!(normalize_topic("add feature #123"), "add_feature_123");
    }

    #[test]
    fn test_normalize_topic_already_normalized() {
        // Already normalized topics should pass through
        assert_eq!(normalize_topic("already_normalized"), "already_normalized");
        assert_eq!(normalize_topic("lowercase_topic"), "lowercase_topic");
    }

    #[test]
    fn test_normalize_topic_multiple_spaces() {
        // Multiple spaces should collapse to single underscore
        assert_eq!(normalize_topic("too   many    spaces"), "too_many_spaces");
    }

    #[test]
    fn test_generate_filename_with_topic() {
        let meta = FilenameMetadata {
            date: Some("2026-01-25".to_string()),
            agent: Some("claude".to_string()),
            topic: Some("Debugging Auth Flow".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions {
            include_date: true,
            include_agent: true,
            include_topic: true,
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        assert!(result.contains("2026-01-25"));
        assert!(result.contains("claude"));
        assert!(result.contains("debugging_auth_flow"));
    }

    #[test]
    fn test_generate_filename_topic_without_flag() {
        // Topic should not appear if include_topic is false
        let meta = FilenameMetadata {
            topic: Some("My Topic".to_string()),
            title: Some("Session".to_string()),
            ..Default::default()
        };
        let opts = FilenameOptions {
            include_topic: false,
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        assert!(!result.contains("my_topic"));
        assert_eq!(result, "session");
    }

    #[test]
    fn test_generate_filename_full_robot_mode() {
        // Typical robot mode export with all metadata
        let meta = FilenameMetadata {
            date: Some("2026-01-25".to_string()),
            agent: Some("claude_code".to_string()),
            project: Some("my-project".to_string()),
            topic: Some("Fix Authentication Bug".to_string()),
            title: None, // Robot mode might not use title
        };
        let opts = FilenameOptions {
            include_date: true,
            include_agent: true,
            include_project: true,
            include_topic: true,
            ..Default::default()
        };

        let result = generate_filename(&meta, &opts);
        // Should produce something like: 2026-01-25_claude_code_my-project_fix_authentication_bug
        assert!(result.starts_with("2026-01-25"));
        assert!(result.contains("claude_code"));
        assert!(result.contains("my-project"));
        assert!(result.contains("fix_authentication_bug"));
    }

    // ========================================================================
    // Smart filename generation tests
    // ========================================================================

    #[test]
    fn test_agent_slug_canonical() {
        assert_eq!(agent_slug("claude_code"), "claude");
        assert_eq!(agent_slug("Claude-Code"), "claude");
        assert_eq!(agent_slug("cursor"), "cursor");
        assert_eq!(agent_slug("ChatGPT"), "chatgpt");
        assert_eq!(agent_slug("gemini-cli"), "gemini");
        assert_eq!(agent_slug("github_copilot"), "copilot");
    }

    #[test]
    fn test_agent_slug_unknown() {
        // Unknown agents get slugified
        assert_eq!(agent_slug("MyCustomAgent"), "mycustomagent");
        // Long names get truncated
        let long = agent_slug("VeryLongAgentNameThatExceedsLimit");
        assert!(long.len() <= 15);
    }

    #[test]
    fn test_workspace_slug_with_path() {
        let path = PathBuf::from("/home/user/projects/my-awesome-project");
        assert_eq!(workspace_slug(Some(&path)), "my-awesome-project");
    }

    #[test]
    fn test_workspace_slug_without_path() {
        assert_eq!(workspace_slug(None), "standalone");
    }

    #[test]
    fn test_workspace_slug_long_name() {
        let path = PathBuf::from("/path/to/very-long-project-name-that-exceeds-limit");
        let slug = workspace_slug(Some(&path));
        assert!(slug.len() <= 20);
    }

    #[test]
    fn test_datetime_slug_format() {
        // Test with a known timestamp (2026-01-25 14:30:00 UTC in milliseconds)
        let ts = 1769436600000i64;
        let slug = datetime_slug(Some(ts));
        // Should produce format like YYYY_MM_DD_HHMM
        assert!(slug.contains('_'));
        assert_eq!(slug.len(), 15); // YYYY_MM_DD_HHMM
    }

    #[test]
    fn test_datetime_slug_none() {
        // Should use current time when None
        let slug = datetime_slug(None);
        assert!(slug.starts_with("202")); // Reasonable year check
        assert_eq!(slug.len(), 15);
    }

    #[test]
    fn test_extract_topic_from_title() {
        let topic = extract_topic(Some("Fix Auth Bug"), None);
        assert_eq!(topic, "fix_auth_bug");
    }

    #[test]
    fn test_extract_topic_from_message() {
        let topic = extract_topic(None, Some("Help me debug this authentication issue"));
        // Topic gets truncated to 30 chars at word boundary
        assert_eq!(topic, "help_me_debug_this");
    }

    #[test]
    fn test_extract_topic_skips_urls() {
        let topic = extract_topic(None, Some("Check https://example.com for the issue"));
        assert!(!topic.contains("http"));
        assert!(topic.contains("check"));
    }

    #[test]
    fn test_extract_topic_fallback() {
        let topic = extract_topic(None, None);
        assert_eq!(topic, "session");
    }

    #[test]
    fn test_generate_full_filename() {
        let filename = generate_full_filename(
            "claude_code",
            Some(Path::new("/projects/myapp")),
            Some(1769436600000),
            Some("Fix Auth"),
            None,
        );

        assert!(filename.starts_with("claude_"));
        assert!(filename.contains("myapp"));
        assert!(filename.ends_with(".html"));
    }

    #[test]
    fn test_get_downloads_dir_returns_path() {
        let downloads = get_downloads_dir();
        // Should return some valid path
        assert!(!downloads.as_os_str().is_empty());
    }

    #[test]
    fn test_unique_filename_no_collision() {
        let dir = std::env::temp_dir();
        let unique_base = format!("test_unique_{}.html", std::process::id());
        let path = unique_filename(&dir, &unique_base);
        // Should return the original name if no collision
        assert!(
            path.to_string_lossy()
                .contains(&unique_base.replace(".html", ""))
        );
    }

    #[test]
    fn test_truncate_topic() {
        // Short topics unchanged
        assert_eq!(truncate_topic("short", 30), "short");

        // Long topics truncated at word boundary
        let long = "this_is_a_very_long_topic_name_that_needs_truncation";
        let truncated = truncate_topic(long, 30);
        assert!(truncated.len() <= 30);
        assert!(!truncated.ends_with('_'));
    }

    // ========================================================================
    // UTF-8 boundary safety tests
    // ========================================================================

    #[test]
    fn test_truncate_to_char_boundary() {
        // ASCII string
        assert_eq!(truncate_to_char_boundary("hello", 3), 3);
        assert_eq!(truncate_to_char_boundary("hello", 10), 5);

        // UTF-8 multi-byte characters
        // "日本語" = 3 chars, 9 bytes (each char is 3 bytes)
        let japanese = "日本語";
        assert_eq!(japanese.len(), 9);
        // Truncating at byte 4 should back up to byte 3 (end of first char)
        assert_eq!(truncate_to_char_boundary(japanese, 4), 3);
        // Truncating at byte 6 should stay at 6 (end of second char)
        assert_eq!(truncate_to_char_boundary(japanese, 6), 6);

        // "café" = 4 chars, 5 bytes (é is 2 bytes)
        let cafe = "café";
        assert_eq!(cafe.len(), 5);
        // Truncating at byte 4 should back up to byte 3 (before the é)
        assert_eq!(truncate_to_char_boundary(cafe, 4), 3);
    }

    #[test]
    fn test_enforce_max_len_utf8_safe() {
        // This test would panic before the fix if max_len cuts into a multi-byte char
        let long_with_emoji = "this_is_a_test_with_emoji_🎉_at_end";
        let result = enforce_max_len(long_with_emoji.to_string(), Some(30));
        // Should not panic, and result should be valid UTF-8
        assert!(result.len() <= 30);
        // The result should be valid UTF-8 (this wouldn't compile if not)
        let _ = result.chars().count();
    }

    #[test]
    fn test_agent_slug_utf8_safe() {
        // Long agent name with non-ASCII should not panic
        let result = agent_slug("müllerâgentnamëthätexceedslimit");
        // Should not panic, and result should be valid UTF-8
        assert!(result.len() <= 15);
        let _ = result.chars().count();
    }

    #[test]
    fn test_workspace_slug_utf8_safe() {
        // Project path with non-ASCII chars
        let path = PathBuf::from("/home/user/projéctswithöddnämesthätexceedlimits");
        let result = workspace_slug(Some(&path));
        // Should not panic, and result should be valid UTF-8
        assert!(result.len() <= 20);
        let _ = result.chars().count();
    }

    #[test]
    fn test_truncate_topic_utf8_safe() {
        // Topic with multi-byte characters that would panic if sliced incorrectly
        let topic = "日本語_programming_topic_that_is_very_long";
        let result = truncate_topic(topic, 20);
        // Should not panic, and result should be valid UTF-8
        assert!(result.len() <= 20);
        let _ = result.chars().count();
    }
}
