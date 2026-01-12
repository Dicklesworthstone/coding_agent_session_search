use coding_agent_search::connectors::{
    NormalizedConversation, NormalizedMessage, NormalizedSnippet,
};
use coding_agent_search::model::types::{Conversation, Message, MessageRole, Snippet};
use coding_agent_search::search::query::{MatchType, SearchHit};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Captures tracing output for tests.
#[allow(dead_code)]
pub struct TestTracing {
    buffer: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

#[allow(dead_code)]
impl TestTracing {
    pub fn new() -> Self {
        Self {
            buffer: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.buffer.clone();
        let make_writer = move || TestWriter(writer.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(make_writer)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    pub fn output(&self) -> String {
        let buf = self.buffer.lock().unwrap();
        String::from_utf8_lossy(&buf).to_string()
    }

    /// Assert that the captured log output contains the provided substring.
    pub fn assert_contains(&self, needle: &str) {
        let out = self.output();
        assert!(
            out.contains(needle),
            "expected logs to contain `{needle}`, got:\n{out}"
        );
    }

    /// Return captured log lines (trimmed of trailing newline) for fine-grained checks.
    pub fn lines(&self) -> Vec<String> {
        self.output()
            .lines()
            .map(std::string::ToString::to_string)
            .collect()
    }
}

#[allow(dead_code)]
pub struct EnvGuard {
    key: String,
    prev: Option<String>,
}

#[allow(dead_code)]
impl EnvGuard {
    pub fn set(key: &str, val: impl AsRef<str>) -> Self {
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, val.as_ref()) };
        Self {
            key: key.to_string(),
            prev,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(&self.key, v) },
            None => unsafe { std::env::remove_var(&self.key) },
        }
    }
}

/// RAII guard for changing the current working directory.
/// Automatically restores the previous directory on drop, even if a test panics.
#[allow(dead_code)]
pub struct CwdGuard {
    prev: PathBuf,
}

#[allow(dead_code)]
impl CwdGuard {
    /// Change to the given directory and return a guard that restores the previous directory on drop.
    pub fn change_to(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let prev = std::env::current_dir()?;
        std::env::set_current_dir(path.as_ref())?;
        Ok(Self { prev })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Best effort restore - ignore errors during drop
        let _ = std::env::set_current_dir(&self.prev);
    }
}

struct TestWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for TestWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().unwrap();
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[allow(dead_code)]
pub struct TempFixtureDir {
    pub dir: TempDir,
}

#[allow(dead_code)]
impl TempFixtureDir {
    pub fn new() -> Self {
        Self {
            dir: TempDir::new().expect("tempdir"),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
}

use std::collections::HashMap;

/// Deterministic conversation/message generator for tests.
#[derive(Debug, Clone)]
pub struct ConversationFixtureBuilder {
    agent_slug: String,
    external_id: Option<String>,
    workspace: Option<PathBuf>,
    source_path: PathBuf,
    base_ts: i64,
    content_prefix: String,
    message_count: usize,
    snippets: Vec<SnippetSpec>,
    custom_content: HashMap<usize, String>,
    title: Option<String>,
}

#[allow(dead_code)]
impl ConversationFixtureBuilder {
    pub fn new(agent_slug: impl Into<String>) -> Self {
        let agent_slug = agent_slug.into();
        let source_path = PathBuf::from(format!("/tmp/{agent_slug}/session-0.jsonl"));
        Self {
            agent_slug,
            external_id: None,
            workspace: None,
            source_path,
            base_ts: 1_700_000_000_000, // stable timestamp for deterministic tests
            content_prefix: "msg".into(),
            message_count: 2,
            snippets: Vec::new(),
            custom_content: HashMap::new(),
            title: None,
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn external_id(mut self, id: impl Into<String>) -> Self {
        self.external_id = Some(id.into());
        self
    }

    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    pub fn source_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.source_path = path.into();
        self
    }

    pub fn base_ts(mut self, ts: i64) -> Self {
        self.base_ts = ts;
        self
    }

    pub fn content_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.content_prefix = prefix.into();
        self
    }

    pub fn messages(mut self, count: usize) -> Self {
        self.message_count = count.max(1);
        self
    }

    pub fn with_content(mut self, idx: usize, content: impl Into<String>) -> Self {
        self.custom_content.insert(idx, content.into());
        // Ensure message count covers this index
        if idx >= self.message_count {
            self.message_count = idx + 1;
        }
        self
    }

    /// Attach a snippet to a specific message index (0-based).
    pub fn with_snippet(mut self, spec: SnippetSpec) -> Self {
        self.snippets.push(spec);
        self
    }

    /// Convenience: attach a snippet with text/language to the first message.
    pub fn with_snippet_text(self, text: impl Into<String>, language: impl Into<String>) -> Self {
        self.with_snippet(
            SnippetSpec::new(0)
                .text(text)
                .language(language)
                .lines(1, 1),
        )
    }

    /// Build a `NormalizedConversation` (connector-facing).
    pub fn build_normalized(self) -> NormalizedConversation {
        let messages: Vec<NormalizedMessage> = (0..self.message_count)
            .map(|i| {
                let is_user = i % 2 == 0;
                let snippets: Vec<NormalizedSnippet> = self
                    .snippets
                    .iter()
                    .filter(|s| s.msg_idx == i)
                    .map(|s| NormalizedSnippet {
                        file_path: s.file_path.clone(),
                        start_line: s.start_line,
                        end_line: s.end_line,
                        language: s.language.clone(),
                        snippet_text: s.text.clone(),
                    })
                    .collect();

                let content = self
                    .custom_content
                    .get(&i)
                    .cloned()
                    .unwrap_or_else(|| format!("{}-{}", self.content_prefix, i));

                NormalizedMessage {
                    idx: i as i64,
                    role: if is_user { "user" } else { "assistant" }.into(),
                    author: if is_user {
                        Some("user".into())
                    } else {
                        Some("agent".into())
                    },
                    created_at: Some(self.base_ts + i as i64),
                    content,
                    extra: json!({"seed": i}),
                    snippets,
                }
            })
            .collect();

        NormalizedConversation {
            agent_slug: self.agent_slug.clone(),
            external_id: self.external_id.clone(),
            title: self
                .title
                .or_else(|| Some(format!("{} conversation", self.agent_slug))),
            workspace: self.workspace.clone(),
            source_path: self.source_path.clone(),
            started_at: messages.first().and_then(|m| m.created_at),
            ended_at: messages.last().and_then(|m| m.created_at),
            metadata: json!({"fixture": true}),
            messages,
        }
    }

    /// Build a Conversation (storage-facing).
    pub fn build_conversation(self) -> Conversation {
        let messages: Vec<Message> = (0..self.message_count)
            .map(|i| {
                let role = if i % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Agent
                };
                let snippets: Vec<Snippet> = self
                    .snippets
                    .iter()
                    .filter(|s| s.msg_idx == i)
                    .map(|s| Snippet {
                        id: None,
                        file_path: s.file_path.clone(),
                        start_line: s.start_line,
                        end_line: s.end_line,
                        language: s.language.clone(),
                        snippet_text: s.text.clone(),
                    })
                    .collect();

                let content = self
                    .custom_content
                    .get(&i)
                    .cloned()
                    .unwrap_or_else(|| format!("{}-{}", self.content_prefix, i));

                Message {
                    id: None,
                    idx: i as i64,
                    role,
                    author: if i % 2 == 0 {
                        Some("user".into())
                    } else {
                        Some("agent".into())
                    },
                    created_at: Some(self.base_ts + i as i64),
                    content,
                    extra_json: json!({"seed": i}),
                    snippets,
                }
            })
            .collect();

        Conversation {
            id: None,
            agent_slug: self.agent_slug.clone(),
            workspace: self.workspace.clone(),
            external_id: self.external_id.clone(),
            title: self
                .title
                .or_else(|| Some(format!("{} conversation", self.agent_slug))),
            source_path: self.source_path.clone(),
            started_at: messages.first().and_then(|m| m.created_at),
            ended_at: messages.last().and_then(|m| m.created_at),
            approx_tokens: Some((self.message_count * 12) as i64),
            metadata_json: json!({"fixture": true}),
            messages,
            source_id: "local".to_string(),
            origin_host: None,
        }
    }
}

/// Helper to fluently assert `SearchHit` fields in tests.
pub struct SearchHitAssert<'a> {
    hit: &'a SearchHit,
}

#[allow(dead_code)]
pub fn assert_hit(hit: &SearchHit) -> SearchHitAssert<'_> {
    SearchHitAssert { hit }
}

#[allow(dead_code)]
impl SearchHitAssert<'_> {
    pub fn title(self, expected: impl AsRef<str>) -> Self {
        assert_eq!(
            self.hit.title,
            expected.as_ref(),
            "title mismatch for hit {:?}",
            self.hit.source_path
        );
        self
    }

    pub fn agent(self, expected: impl AsRef<str>) -> Self {
        assert_eq!(
            self.hit.agent,
            expected.as_ref(),
            "agent mismatch for hit {:?}",
            self.hit.source_path
        );
        self
    }

    pub fn workspace(self, expected: impl AsRef<str>) -> Self {
        assert_eq!(
            self.hit.workspace,
            expected.as_ref(),
            "workspace mismatch for hit {:?}",
            self.hit.source_path
        );
        self
    }

    pub fn snippet_contains(self, needle: impl AsRef<str>) -> Self {
        let needle = needle.as_ref();
        assert!(
            self.hit.snippet.contains(needle),
            "snippet missing `{}` in hit {:?}",
            needle,
            self.hit.source_path
        );
        self
    }

    pub fn content_contains(self, needle: impl AsRef<str>) -> Self {
        let needle = needle.as_ref();
        assert!(
            self.hit.content.contains(needle),
            "content missing `{}` in hit {:?}",
            needle,
            self.hit.source_path
        );
        self
    }

    pub fn line(self, expected: usize) -> Self {
        assert_eq!(
            self.hit.line_number,
            Some(expected),
            "line number mismatch for hit {:?}",
            self.hit.source_path
        );
        self
    }

    pub fn match_type(self, expected: MatchType) -> Self {
        assert_eq!(
            self.hit.match_type, expected,
            "match type mismatch for hit {:?}",
            self.hit.source_path
        );
        self
    }
}

// -------- Macros & connector presets --------

#[macro_export]
macro_rules! assert_logs_contain {
    ($tracing:expr, $needle:expr) => {{
        let out = $tracing.output();
        assert!(
            out.contains($needle),
            "expected logs to contain `{}` but were:\n{}",
            $needle,
            out
        );
    }};
}

#[macro_export]
macro_rules! assert_logs_not_contain {
    ($tracing:expr, $needle:expr) => {{
        let out = $tracing.output();
        assert!(
            !out.contains($needle),
            "expected logs NOT to contain `{}` but were:\n{}",
            $needle,
            out
        );
    }};
}

/// Typical fixture shapes for each connector. Paths mirror real connectors but live in /tmp.
#[allow(dead_code)]
pub fn fixture_codex() -> ConversationFixtureBuilder {
    ConversationFixtureBuilder::new("codex")
        .workspace("/tmp/workspaces/codex")
        .source_path("/tmp/.codex/sessions/rollout-1.jsonl")
        .external_id("rollout-1")
}

#[allow(dead_code)]
pub fn fixture_cline() -> ConversationFixtureBuilder {
    ConversationFixtureBuilder::new("cline")
        .workspace("/tmp/workspaces/cline")
        .source_path(
            "/tmp/.config/Code/User/globalStorage/saoudrizwan.claude-dev/task/ui_messages.json",
        )
        .external_id("cline-task-1")
}

#[allow(dead_code)]
pub fn fixture_claude_code() -> ConversationFixtureBuilder {
    ConversationFixtureBuilder::new("claude_code")
        .workspace("/tmp/.claude/projects/demo")
        .source_path("/tmp/.claude/projects/demo/session.jsonl")
        .external_id("claude-session-1")
}

#[allow(dead_code)]
pub fn fixture_gemini() -> ConversationFixtureBuilder {
    ConversationFixtureBuilder::new("gemini")
        .workspace("/tmp/.gemini/tmp/project-hash")
        .source_path("/tmp/.gemini/tmp/project-hash/chats/session-1.json")
        .external_id("session-1")
}

#[allow(dead_code)]
pub fn fixture_opencode() -> ConversationFixtureBuilder {
    ConversationFixtureBuilder::new("opencode")
        .workspace("/tmp/opencode/workspace")
        .source_path("/tmp/opencode/database.db")
        .external_id("db-session-1")
}

#[allow(dead_code)]
pub fn fixture_amp() -> ConversationFixtureBuilder {
    ConversationFixtureBuilder::new("amp")
        .workspace("/tmp/sourcegraph.amp/ws")
        .source_path("/tmp/sourcegraph.amp/cache/session.json")
        .external_id("amp-1")
}

// =============================================================================
// Multi-Source Fixture Helpers (P7.6)
// =============================================================================

/// Create a conversation fixture with explicit provenance fields.
#[allow(dead_code)]
pub struct MultiSourceConversationBuilder {
    inner: ConversationFixtureBuilder,
    source_id: String,
    origin_host: Option<String>,
}

#[allow(dead_code)]
impl MultiSourceConversationBuilder {
    pub fn local(agent_slug: impl Into<String>) -> Self {
        Self {
            inner: ConversationFixtureBuilder::new(agent_slug),
            source_id: "local".to_string(),
            origin_host: None,
        }
    }

    pub fn remote(
        agent_slug: impl Into<String>,
        source_id: impl Into<String>,
        host: impl Into<String>,
    ) -> Self {
        let sid = source_id.into();
        Self {
            inner: ConversationFixtureBuilder::new(agent_slug),
            source_id: sid.clone(),
            origin_host: Some(host.into()),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.inner = self.inner.title(title);
        self
    }

    pub fn external_id(mut self, id: impl Into<String>) -> Self {
        self.inner = self.inner.external_id(id);
        self
    }

    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.workspace(path);
        self
    }

    pub fn source_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.inner = self.inner.source_path(path);
        self
    }

    pub fn base_ts(mut self, ts: i64) -> Self {
        self.inner = self.inner.base_ts(ts);
        self
    }

    pub fn messages(mut self, count: usize) -> Self {
        self.inner = self.inner.messages(count);
        self
    }

    pub fn with_content(mut self, idx: usize, content: impl Into<String>) -> Self {
        self.inner = self.inner.with_content(idx, content);
        self
    }

    /// Build a Conversation with the specified provenance.
    pub fn build(self) -> Conversation {
        let mut conv = self.inner.build_conversation();
        conv.source_id = self.source_id;
        conv.origin_host = self.origin_host;
        conv
    }
}

/// Pre-built fixture scenarios for multi-source testing.
#[allow(dead_code)]
pub mod multi_source_fixtures {
    use super::*;

    /// Local Claude Code session on myapp project.
    pub fn local_myapp_session1() -> MultiSourceConversationBuilder {
        MultiSourceConversationBuilder::local("claude_code")
            .title("Fix login authentication bug")
            .external_id("local-cc-001")
            .workspace("/Users/dev/projects/myapp")
            .source_path("/Users/dev/.claude/projects/myapp/session-local-001.jsonl")
            .base_ts(1_702_195_200_000) // 2025-12-10T09:00:00Z
            .messages(4)
            .with_content(0, "Fix the login authentication bug that causes the session to expire too early")
            .with_content(1, "I'll investigate the authentication module. Let me look at the session management code.")
    }

    /// Local Claude Code session on myapp project (rate limiting).
    pub fn local_myapp_session2() -> MultiSourceConversationBuilder {
        MultiSourceConversationBuilder::local("claude_code")
            .title("Add API rate limiting")
            .external_id("local-cc-002")
            .workspace("/Users/dev/projects/myapp")
            .source_path("/Users/dev/.claude/projects/myapp/session-local-002.jsonl")
            .base_ts(1_702_299_600_000) // 2025-12-11T14:00:00Z
            .messages(3)
            .with_content(0, "Add rate limiting to the API endpoints")
            .with_content(
                1,
                "I'll implement rate limiting using a token bucket algorithm.",
            )
    }

    /// Remote laptop session on myapp project (same workspace, different path).
    pub fn laptop_myapp_session() -> MultiSourceConversationBuilder {
        MultiSourceConversationBuilder::remote("claude_code", "laptop", "laptop.local")
            .title("Add logout button to header")
            .external_id("laptop-cc-001")
            .workspace("/home/user/projects/myapp") // Different path, same logical project
            .source_path("/home/user/.claude/projects/myapp/session-laptop-001.jsonl")
            .base_ts(1_702_112_400_000) // 2025-12-09T10:00:00Z
            .messages(3)
            .with_content(0, "Add logout button to the header component")
            .with_content(1, "I'll add a logout button to the header. Let me check the current header component structure.")
    }

    /// Remote workstation session on backend project.
    pub fn workstation_backend_session() -> MultiSourceConversationBuilder {
        MultiSourceConversationBuilder::remote("claude_code", "workstation", "work.example.com")
            .title("Implement user registration with email verification")
            .external_id("work-cc-001")
            .workspace("/home/dev/backend")
            .source_path("/home/dev/.claude/projects/backend/session-work-001.jsonl")
            .base_ts(1_702_396_800_000) // 2025-12-12T16:00:00Z
            .messages(5)
            .with_content(0, "Implement the user registration endpoint with email verification")
            .with_content(1, "I'll create the registration endpoint with proper validation and email verification flow.")
    }

    /// Generate a complete multi-source test set (4 sessions from 3 sources).
    pub fn all_sessions() -> Vec<Conversation> {
        vec![
            local_myapp_session1().build(),
            local_myapp_session2().build(),
            laptop_myapp_session().build(),
            workstation_backend_session().build(),
        ]
    }

    /// Get sessions filtered by source.
    pub fn sessions_by_source(source_id: &str) -> Vec<Conversation> {
        all_sessions()
            .into_iter()
            .filter(|c| c.source_id == source_id)
            .collect()
    }

    /// Get local sessions only.
    pub fn local_sessions() -> Vec<Conversation> {
        sessions_by_source("local")
    }

    /// Get remote sessions only.
    pub fn remote_sessions() -> Vec<Conversation> {
        all_sessions()
            .into_iter()
            .filter(|c| c.source_id != "local")
            .collect()
    }
}

/// Snippet specification for attaching code fragments to generated messages.
#[derive(Debug, Clone)]
pub struct SnippetSpec {
    pub msg_idx: usize,
    pub file_path: Option<PathBuf>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
    pub language: Option<String>,
    pub text: Option<String>,
}

impl SnippetSpec {
    pub fn new(msg_idx: usize) -> Self {
        Self {
            msg_idx,
            file_path: None,
            start_line: None,
            end_line: None,
            language: None,
            text: None,
        }
    }

    #[allow(dead_code)]
    pub fn file(mut self, path: impl Into<PathBuf>) -> Self {
        self.file_path = Some(path.into());
        self
    }

    pub fn lines(mut self, start: i64, end: i64) -> Self {
        self.start_line = Some(start);
        self.end_line = Some(end);
        self
    }

    pub fn language(mut self, lang: impl Into<String>) -> Self {
        self.language = Some(lang.into());
        self
    }

    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

// =============================================================================
// Deterministic RNG Utilities
// =============================================================================

/// Deterministic random number generator for reproducible tests.
///
/// Uses ChaCha8Rng seeded from a u64 for fast, reproducible random generation.
/// This ensures tests produce identical results across runs.
#[allow(dead_code)]
pub struct SeededRng {
    rng: ChaCha8Rng,
    seed: u64,
}

#[allow(dead_code)]
impl SeededRng {
    /// Create a new SeededRng with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed),
            seed,
        }
    }

    /// Get the seed used to initialize this RNG.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Generate a random f32 in the range [0, 1).
    pub fn f32(&mut self) -> f32 {
        self.rng.r#gen::<f32>()
    }

    /// Generate a random f32 in the given range [min, max).
    /// If min > max, they are swapped.
    pub fn f32_range(&mut self, min: f32, max: f32) -> f32 {
        let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
        lo + self.rng.r#gen::<f32>() * (hi - lo)
    }

    /// Generate a random i64 in the given range [min, max).
    /// If min >= max, returns min.
    pub fn i64_range(&mut self, min: i64, max: i64) -> i64 {
        if min >= max {
            return min;
        }
        self.rng.r#gen_range(min..max)
    }

    /// Generate a random usize in the given range [min, max).
    /// If min >= max, returns min.
    pub fn usize_range(&mut self, min: usize, max: usize) -> usize {
        if min >= max {
            return min;
        }
        self.rng.r#gen_range(min..max)
    }

    /// Generate a random alphanumeric string of the given length.
    pub fn alphanumeric(&mut self, len: usize) -> String {
        const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        (0..len)
            .map(|_| {
                let idx = self.rng.r#gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    }

    /// Generate a normalized f32 vector of the given dimension.
    /// Each component is in [-1, 1] and the vector is L2-normalized.
    pub fn normalized_vector(&mut self, dimension: usize) -> Vec<f32> {
        let mut vec: Vec<f32> = (0..dimension)
            .map(|_| self.f32_range(-1.0, 1.0))
            .collect();
        let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-10 {
            for v in &mut vec {
                *v /= norm;
            }
        }
        vec
    }

    /// Generate a vector of random f32 values.
    pub fn f32_vector(&mut self, dimension: usize) -> Vec<f32> {
        (0..dimension).map(|_| self.f32()).collect()
    }
}

// =============================================================================
// Performance Measurement Utilities
// =============================================================================

/// Performance measurement results with statistical analysis.
#[derive(Debug, Clone)]
pub struct PerfMeasurement {
    pub samples: Vec<Duration>,
    pub warmup_iterations: usize,
    pub measured_iterations: usize,
}

#[allow(dead_code)]
impl PerfMeasurement {
    /// Run a function with warmup and measurement iterations.
    ///
    /// # Arguments
    /// * `warmup` - Number of warmup iterations (not measured)
    /// * `iterations` - Number of measured iterations
    /// * `f` - The function to measure
    pub fn measure<F>(warmup: usize, iterations: usize, mut f: F) -> Self
    where
        F: FnMut(),
    {
        // Warmup phase
        for _ in 0..warmup {
            f();
        }

        // Measurement phase
        let mut samples = Vec::with_capacity(iterations);
        for _ in 0..iterations {
            let start = Instant::now();
            f();
            samples.push(start.elapsed());
        }

        Self {
            samples,
            warmup_iterations: warmup,
            measured_iterations: iterations,
        }
    }

    /// Get the mean duration.
    pub fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let total: Duration = self.samples.iter().sum();
        total / self.samples.len() as u32
    }

    /// Get the mean as milliseconds (f64).
    pub fn mean_ms(&self) -> f64 {
        self.mean().as_secs_f64() * 1000.0
    }

    /// Get the median duration.
    pub fn median(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted: Vec<_> = self.samples.clone();
        sorted.sort();
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2
        } else {
            sorted[mid]
        }
    }

    /// Get the median as milliseconds (f64).
    pub fn median_ms(&self) -> f64 {
        self.median().as_secs_f64() * 1000.0
    }

    /// Get the standard deviation.
    pub fn std_dev(&self) -> Duration {
        if self.samples.len() < 2 {
            return Duration::ZERO;
        }
        let mean_nanos = self.mean().as_nanos() as f64;
        let variance: f64 = self
            .samples
            .iter()
            .map(|d| {
                let diff = d.as_nanos() as f64 - mean_nanos;
                diff * diff
            })
            .sum::<f64>()
            / (self.samples.len() - 1) as f64;
        Duration::from_nanos(variance.sqrt() as u64)
    }

    /// Get the standard deviation as milliseconds (f64).
    pub fn std_dev_ms(&self) -> f64 {
        self.std_dev().as_secs_f64() * 1000.0
    }

    /// Get the minimum duration.
    pub fn min(&self) -> Duration {
        self.samples.iter().min().copied().unwrap_or(Duration::ZERO)
    }

    /// Get the maximum duration.
    pub fn max(&self) -> Duration {
        self.samples.iter().max().copied().unwrap_or(Duration::ZERO)
    }

    /// Get a percentile (0-100).
    /// Values outside [0, 100] are clamped.
    pub fn percentile(&self, p: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted: Vec<_> = self.samples.clone();
        sorted.sort();
        // Clamp p to [0, 100] to avoid negative values or overflow
        let p_clamped = p.clamp(0.0, 100.0);
        let idx = ((p_clamped / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    /// Print a summary of the measurement.
    pub fn print_summary(&self, label: &str) {
        println!(
            "{}: mean={:.3}ms median={:.3}ms std_dev={:.3}ms min={:.3}ms max={:.3}ms p95={:.3}ms",
            label,
            self.mean_ms(),
            self.median_ms(),
            self.std_dev_ms(),
            self.min().as_secs_f64() * 1000.0,
            self.max().as_secs_f64() * 1000.0,
            self.percentile(95.0).as_secs_f64() * 1000.0,
        );
    }
}

/// Compare two implementations and return whether the new one is faster.
///
/// Returns (speedup_ratio, baseline_measurement, new_measurement).
/// A speedup_ratio > 1.0 means the new implementation is faster.
#[allow(dead_code)]
pub fn compare_implementations<F1, F2>(
    warmup: usize,
    iterations: usize,
    mut baseline: F1,
    mut new_impl: F2,
) -> (f64, PerfMeasurement, PerfMeasurement)
where
    F1: FnMut(),
    F2: FnMut(),
{
    let baseline_perf = PerfMeasurement::measure(warmup, iterations, &mut baseline);
    let new_perf = PerfMeasurement::measure(warmup, iterations, &mut new_impl);

    let baseline_mean = baseline_perf.mean_ms();
    let new_mean = new_perf.mean_ms();

    let speedup = if new_mean > 0.0 {
        baseline_mean / new_mean
    } else {
        f64::INFINITY
    };

    (speedup, baseline_perf, new_perf)
}

// =============================================================================
// Float Comparison Assertions
// =============================================================================

/// Assert that two f32 values are approximately equal within epsilon.
#[allow(dead_code)]
pub fn assert_float_eq(a: f32, b: f32, epsilon: f32) {
    let diff = (a - b).abs();
    assert!(
        diff <= epsilon,
        "float mismatch: {} vs {} (diff={}, epsilon={})",
        a,
        b,
        diff,
        epsilon
    );
}

/// Assert that two f64 values are approximately equal within epsilon.
#[allow(dead_code)]
pub fn assert_float64_eq(a: f64, b: f64, epsilon: f64) {
    let diff = (a - b).abs();
    assert!(
        diff <= epsilon,
        "float64 mismatch: {} vs {} (diff={}, epsilon={})",
        a,
        b,
        diff,
        epsilon
    );
}

/// Assert that two f32 vectors are approximately equal (element-wise).
#[allow(dead_code)]
pub fn assert_vec_float_eq(a: &[f32], b: &[f32], epsilon: f32) {
    assert_eq!(
        a.len(),
        b.len(),
        "vector length mismatch: {} vs {}",
        a.len(),
        b.len()
    );
    for (i, (va, vb)) in a.iter().zip(b.iter()).enumerate() {
        let diff = (va - vb).abs();
        assert!(
            diff <= epsilon,
            "vector element mismatch at index {}: {} vs {} (diff={}, epsilon={})",
            i,
            va,
            vb,
            diff,
            epsilon
        );
    }
}

/// Assert that two slices contain the same elements (order-independent).
#[allow(dead_code)]
pub fn assert_same_elements<T: Ord + Clone + std::fmt::Debug>(a: &[T], b: &[T]) {
    let mut a_sorted: Vec<_> = a.to_vec();
    let mut b_sorted: Vec<_> = b.to_vec();
    a_sorted.sort();
    b_sorted.sort();
    assert_eq!(
        a_sorted, b_sorted,
        "slices contain different elements:\n  a={:?}\n  b={:?}",
        a, b
    );
}

/// Macro to assert two values are "isomorphic" (structurally equivalent).
/// Useful for comparing search results where order may vary but content should match.
#[macro_export]
macro_rules! assert_isomorphic {
    ($a:expr, $b:expr, $key_fn:expr) => {{
        let mut a_keys: Vec<_> = $a.iter().map($key_fn).collect();
        let mut b_keys: Vec<_> = $b.iter().map($key_fn).collect();
        a_keys.sort();
        b_keys.sort();
        assert_eq!(
            a_keys, b_keys,
            "collections are not isomorphic:\n  a keys={:?}\n  b keys={:?}",
            a_keys, b_keys
        );
    }};
}

// =============================================================================
// Test Data Generation Utilities
// =============================================================================

/// Generate test metadata (agent, workspace, source) using a seeded RNG.
#[allow(dead_code)]
pub struct TestDataGenerator {
    rng: SeededRng,
}

#[allow(dead_code)]
impl TestDataGenerator {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: SeededRng::new(seed),
        }
    }

    /// Generate a random agent slug.
    pub fn agent(&mut self) -> String {
        const AGENTS: &[&str] = &[
            "claude_code",
            "codex",
            "cline",
            "gemini",
            "opencode",
            "amp",
            "chatgpt",
        ];
        let idx = self.rng.usize_range(0, AGENTS.len());
        AGENTS[idx].to_string()
    }

    /// Generate a random workspace path.
    pub fn workspace(&mut self) -> PathBuf {
        let project = self.rng.alphanumeric(8);
        PathBuf::from(format!("/home/user/projects/{}", project))
    }

    /// Generate random message content with word count in [min_words, max_words].
    /// If min_words > max_words, they are swapped.
    pub fn content(&mut self, min_words: usize, max_words: usize) -> String {
        const WORDS: &[&str] = &[
            "rust",
            "code",
            "function",
            "test",
            "error",
            "fix",
            "implement",
            "refactor",
            "debug",
            "optimize",
            "performance",
            "memory",
            "async",
            "await",
            "struct",
            "enum",
            "trait",
            "impl",
            "pub",
            "mod",
            "use",
            "let",
            "mut",
            "const",
            "static",
            "fn",
            "return",
            "if",
            "else",
            "match",
            "loop",
            "while",
            "for",
            "in",
            "vec",
            "string",
            "option",
            "result",
            "ok",
            "err",
        ];
        let (lo, hi) = if min_words <= max_words {
            (min_words, max_words)
        } else {
            (max_words, min_words)
        };
        let word_count = self.rng.usize_range(lo, hi + 1);
        (0..word_count)
            .map(|_| {
                let idx = self.rng.usize_range(0, WORDS.len());
                WORDS[idx]
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Generate a timestamp in milliseconds.
    pub fn timestamp(&mut self) -> i64 {
        // Range: 2024-01-01 to 2025-12-31
        self.rng.i64_range(1704067200000, 1767225600000)
    }

    /// Generate a vector of random documents for embedding tests.
    pub fn documents(&mut self, count: usize) -> Vec<String> {
        (0..count)
            .map(|_| self.content(10, 50))
            .collect()
    }

    /// Generate embedding vectors for testing.
    pub fn embeddings(&mut self, count: usize, dimension: usize) -> Vec<Vec<f32>> {
        (0..count)
            .map(|_| self.rng.normalized_vector(dimension))
            .collect()
    }
}
