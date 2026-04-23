//! Shared search-asset lifecycle test harness (bead ibuuh.15).
//!
//! Provides reusable infrastructure for validating search-asset lifecycle
//! behaviour: self-healing, fallback, upgrade, rollout, and corruption
//! recovery.  Downstream beads plug into this harness instead of building
//! ad hoc fixtures.
//!
//! # Provided
//!
//! - **[`TestCorpus`]**: Deterministic synthetic corpus with configurable
//!   conversations, messages, and agents.
//! - **[`CorruptionInjector`]**: Intentionally corrupt or remove lexical/
//!   semantic assets (manifests, metadata, indices, checkpoints).
//! - **[`GoldenQuery`]**: Expected-result corpus for lexical and hybrid search.
//! - **[`HarnessLog`]**: Structured, timestamped log for artifact snapshots
//!   and phase markers, enabling CI failure diagnosis.
//! - **Self-tests**: Proves the harness produces deterministic diagnostics.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

use coding_agent_search::search::policy::{
    CHUNKING_STRATEGY_VERSION, SEMANTIC_SCHEMA_VERSION, SemanticPolicy,
};
use coding_agent_search::search::semantic_manifest::{
    ArtifactRecord, BuildCheckpoint, MANIFEST_FORMAT_VERSION, SemanticManifest, TierKind,
    TierReadiness,
};

// ─── Structured test logging ───────────────────────────────────────────────

/// Structured log entry for test harness diagnostics.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp_ms: i64,
    pub phase: String,
    pub message: String,
    pub artifacts: HashMap<String, String>,
}

/// Accumulates structured, timestamped log entries for a test scenario.
///
/// After the test, the log can be dumped as JSON lines for CI artifact
/// retention and post-mortem diagnosis.
#[derive(Debug, Default)]
pub struct HarnessLog {
    entries: Vec<LogEntry>,
}

impl HarnessLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a phase marker with optional artifact snapshots.
    pub fn phase(&mut self, phase: &str, message: &str) {
        self.entries.push(LogEntry {
            timestamp_ms: now_ms(),
            phase: phase.to_owned(),
            message: message.to_owned(),
            artifacts: HashMap::new(),
        });
    }

    /// Record a phase marker with artifact key-value snapshots.
    pub fn phase_with_artifacts(
        &mut self,
        phase: &str,
        message: &str,
        artifacts: HashMap<String, String>,
    ) {
        self.entries.push(LogEntry {
            timestamp_ms: now_ms(),
            phase: phase.to_owned(),
            message: message.to_owned(),
            artifacts,
        });
    }

    /// Snapshot the current state of a directory tree (file names + sizes).
    pub fn snapshot_dir(&mut self, phase: &str, dir: &Path) {
        let mut artifacts = HashMap::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                artifacts.insert(name, format!("{size} bytes"));
            }
        }
        self.phase_with_artifacts(phase, &format!("snapshot of {}", dir.display()), artifacts);
    }

    /// Dump as JSON lines (one line per entry).
    pub fn to_jsonl(&self) -> String {
        self.entries
            .iter()
            .map(|e| {
                // Use serde_json for proper escaping of all special characters
                // (newlines, backslashes, control chars) — not just quotes.
                let phase = serde_json::to_string(&e.phase).unwrap_or_else(|_| "\"\"".to_owned());
                let msg = serde_json::to_string(&e.message).unwrap_or_else(|_| "\"\"".to_owned());
                let artifacts =
                    serde_json::to_string(&e.artifacts).unwrap_or_else(|_| "{}".to_owned());
                format!(
                    r#"{{"ts":{},"phase":{},"msg":{},"artifacts":{}}}"#,
                    e.timestamp_ms, phase, msg, artifacts,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn entries(&self) -> &[LogEntry] {
        &self.entries
    }
}

// ─── Deterministic synthetic corpus ────────────────────────────────────────

/// Configuration for a synthetic test corpus.
#[derive(Debug, Clone)]
pub struct CorpusConfig {
    /// Number of conversations to generate.
    pub num_conversations: usize,
    /// Messages per conversation.
    pub messages_per_conversation: usize,
    /// Agent names to cycle through.
    pub agents: Vec<String>,
    /// Workspace path (deterministic).
    pub workspace: String,
    /// Source ID.
    pub source_id: String,
}

impl Default for CorpusConfig {
    fn default() -> Self {
        Self {
            num_conversations: 10,
            messages_per_conversation: 4,
            agents: vec![
                "claude_code".to_owned(),
                "codex".to_owned(),
                "gemini".to_owned(),
            ],
            workspace: "/projects/test-workspace".to_owned(),
            source_id: "local".to_owned(),
        }
    }
}

/// A synthetic test conversation.
#[derive(Debug, Clone)]
pub struct TestConversation {
    pub id: usize,
    pub agent: String,
    pub title: String,
    pub messages: Vec<TestMessage>,
    pub started_at_ms: i64,
}

/// A synthetic test message.
#[derive(Debug, Clone)]
pub struct TestMessage {
    pub idx: usize,
    pub role: String,
    pub content: String,
}

/// Deterministic test corpus with known content for golden-query assertions.
pub struct TestCorpus {
    pub config: CorpusConfig,
    pub conversations: Vec<TestConversation>,
}

impl TestCorpus {
    /// Generate a deterministic corpus from config.
    pub fn generate(config: CorpusConfig) -> Self {
        let roles = ["user", "assistant"];
        let topics = [
            "authentication middleware",
            "database migration",
            "async runtime setup",
            "rate limiting implementation",
            "error handling patterns",
            "caching strategy",
            "deployment pipeline",
            "test coverage",
            "performance profiling",
            "API versioning",
        ];

        let conversations = (0..config.num_conversations)
            .map(|i| {
                let agent = &config.agents[i % config.agents.len()];
                let topic = topics[i % topics.len()];
                let messages = (0..config.messages_per_conversation)
                    .map(|j| {
                        let role = roles[j % 2];
                        let content = format!(
                            "Conv {i} msg {j}: Discussion about {topic} in {agent} session. \
                             This is deterministic content for golden-query validation."
                        );
                        TestMessage {
                            idx: j,
                            role: role.to_owned(),
                            content,
                        }
                    })
                    .collect();

                TestConversation {
                    id: i + 1,
                    agent: agent.clone(),
                    title: format!("Test session: {topic}"),
                    messages,
                    started_at_ms: 1_700_000_000_000 + (i as i64 * 3_600_000),
                }
            })
            .collect();

        Self {
            config,
            conversations,
        }
    }

    /// Total message count across all conversations.
    pub fn total_messages(&self) -> usize {
        self.conversations.iter().map(|c| c.messages.len()).sum()
    }

    /// Total conversation count.
    pub fn total_conversations(&self) -> usize {
        self.conversations.len()
    }
}

// ─── Corruption / fault injection ──────────────────────────────────────────

/// Intentionally corrupt or remove search assets for testing self-healing and
/// fallback behaviour.
pub struct CorruptionInjector {
    data_dir: PathBuf,
}

impl CorruptionInjector {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Remove the semantic manifest file.
    pub fn remove_semantic_manifest(&self) -> bool {
        let path = SemanticManifest::path(&self.data_dir);
        fs::remove_file(&path).is_ok()
    }

    /// Write a corrupt (non-JSON) semantic manifest.
    pub fn corrupt_semantic_manifest(&self) {
        let path = SemanticManifest::path(&self.data_dir);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&path, b"THIS IS NOT VALID JSON").expect("write corrupt manifest");
    }

    /// Write a manifest with a future version number.
    pub fn write_future_version_manifest(&self) {
        let path = SemanticManifest::path(&self.data_dir);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let manifest = SemanticManifest {
            manifest_version: MANIFEST_FORMAT_VERSION + 99,
            ..Default::default()
        };
        let json = serde_json::to_string(&manifest).unwrap();
        fs::write(&path, json).expect("write future manifest");
    }

    /// Write a manifest with schema version mismatch (triggers rebuild).
    pub fn write_stale_schema_manifest(&self) {
        let path = SemanticManifest::path(&self.data_dir);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let manifest = SemanticManifest {
            quality_tier: Some(ArtifactRecord {
                tier: TierKind::Quality,
                embedder_id: "minilm-384".to_owned(),
                model_revision: "abc123".to_owned(),
                schema_version: 0, // mismatch!
                chunking_version: CHUNKING_STRATEGY_VERSION,
                dimension: 384,
                doc_count: 100,
                conversation_count: 25,
                db_fingerprint: "fp-test".to_owned(),
                index_path: "vector_index/index-minilm-384.fsvi".to_owned(),
                size_bytes: 50_000,
                started_at_ms: 1_700_000_000_000,
                completed_at_ms: 1_700_000_060_000,
                ready: true,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        fs::write(&path, json).expect("write stale schema manifest");
    }

    /// Remove the vector index directory entirely.
    pub fn remove_vector_index_dir(&self) -> bool {
        let dir = self.data_dir.join("vector_index");
        fs::remove_dir_all(&dir).is_ok()
    }

    /// Remove a specific vector index file.
    pub fn remove_vector_index_file(&self, embedder_id: &str) -> bool {
        let path = self
            .data_dir
            .join("vector_index")
            .join(format!("index-{embedder_id}.fsvi"));
        fs::remove_file(&path).is_ok()
    }

    /// Write a zero-byte vector index file (truncated / corrupt).
    pub fn truncate_vector_index(&self, embedder_id: &str) {
        let dir = self.data_dir.join("vector_index");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("index-{embedder_id}.fsvi"));
        fs::write(&path, b"").expect("truncate vector index");
    }

    /// Remove the lexical index state checkpoint.
    pub fn remove_lexical_checkpoint(&self) {
        let state_path = self.data_dir.join("index").join("v4").join("state.json");
        let _ = fs::remove_file(&state_path);
    }

    /// Write a pre-manifest (legacy) vector index without any manifest file.
    pub fn write_legacy_vector_index(&self, embedder_id: &str, content: &[u8]) {
        let dir = self.data_dir.join("vector_index");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join(format!("index-{embedder_id}.fsvi"));
        fs::write(&path, content).expect("write legacy vector index");
        // Ensure NO manifest exists (legacy state).
        let _ = fs::remove_file(SemanticManifest::path(&self.data_dir));
    }

    /// Create a half-published directory (build started but not completed).
    pub fn write_partial_build(&self, embedder_id: &str) {
        let dir = self.data_dir.join("vector_index");
        let _ = fs::create_dir_all(&dir);
        // Write partial index
        let path = dir.join(format!("index-{embedder_id}.fsvi"));
        fs::write(&path, b"partial-data").expect("write partial index");

        // Write manifest with checkpoint but artifact not ready
        let mut manifest = SemanticManifest {
            checkpoint: Some(BuildCheckpoint {
                tier: TierKind::Quality,
                embedder_id: embedder_id.to_owned(),
                last_offset: 50,
                docs_embedded: 200,
                conversations_processed: 50,
                total_conversations: 100,
                db_fingerprint: "fp-partial".to_owned(),
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                saved_at_ms: now_ms(),
            }),
            ..Default::default()
        };
        manifest
            .save(&self.data_dir)
            .expect("save partial manifest");
    }
}

// ─── Golden query corpus ───────────────────────────────────────────────────

/// A golden query with expected search behaviour.
#[derive(Debug, Clone)]
pub struct GoldenQuery {
    /// The search query string.
    pub query: String,
    /// Minimum number of hits expected from the default 10-conversation corpus.
    pub min_hits: usize,
    /// Agent that should appear in results (if any).
    pub expected_agent: Option<String>,
    /// Whether this query should work in lexical-only mode.
    pub works_lexical: bool,
    /// Whether this query benefits from semantic (concept matching).
    pub benefits_from_semantic: bool,
}

/// Standard golden-query corpus for the default 10-conversation test corpus.
pub fn golden_queries() -> Vec<GoldenQuery> {
    vec![
        GoldenQuery {
            query: "authentication".to_owned(),
            min_hits: 1,
            expected_agent: None,
            works_lexical: true,
            benefits_from_semantic: true,
        },
        GoldenQuery {
            query: "database migration".to_owned(),
            min_hits: 1,
            expected_agent: None,
            works_lexical: true,
            benefits_from_semantic: true,
        },
        GoldenQuery {
            query: "async runtime".to_owned(),
            min_hits: 1,
            expected_agent: None,
            works_lexical: true,
            benefits_from_semantic: false,
        },
        GoldenQuery {
            query: "claude_code session".to_owned(),
            min_hits: 1,
            expected_agent: Some("claude_code".to_owned()),
            works_lexical: true,
            benefits_from_semantic: false,
        },
        GoldenQuery {
            query: "deterministic content golden".to_owned(),
            min_hits: 1, // all messages contain this phrase
            expected_agent: None,
            works_lexical: true,
            benefits_from_semantic: false,
        },
        GoldenQuery {
            query: "nonexistent_xyzzy_query".to_owned(),
            min_hits: 0, // should return nothing
            expected_agent: None,
            works_lexical: true,
            benefits_from_semantic: false,
        },
    ]
}

// ─── Test environment builder ──────────────────────────────────────────────

/// Fully isolated test environment with data dir, corpus, and harness log.
pub struct TestEnvironment {
    pub dir: TempDir,
    pub data_dir: PathBuf,
    pub corpus: TestCorpus,
    pub log: HarnessLog,
    pub injector: CorruptionInjector,
}

impl TestEnvironment {
    /// Create a new test environment with the default corpus.
    pub fn new() -> Self {
        Self::with_config(CorpusConfig::default())
    }

    /// Create a new test environment with a custom corpus config.
    pub fn with_config(config: CorpusConfig) -> Self {
        let dir = TempDir::new().expect("create tempdir");
        let data_dir = dir.path().to_path_buf();
        let corpus = TestCorpus::generate(config);
        let injector = CorruptionInjector::new(&data_dir);
        let mut log = HarnessLog::new();
        log.phase("setup", "test environment created");

        Self {
            dir,
            data_dir,
            corpus,
            log,
            injector,
        }
    }

    /// Ensure the vector_index directory exists.
    pub fn ensure_vector_dir(&self) {
        let dir = self.data_dir.join("vector_index");
        fs::create_dir_all(&dir).expect("create vector_index dir");
    }

    /// Write a valid semantic manifest with the given artifact state.
    pub fn write_manifest(&self, manifest: &mut SemanticManifest) {
        self.ensure_vector_dir();
        manifest.save(&self.data_dir).expect("save manifest");
    }

    /// Load the current manifest (if any).
    pub fn load_manifest(&self) -> Option<SemanticManifest> {
        SemanticManifest::load(&self.data_dir).ok().flatten()
    }
}

impl Default for TestEnvironment {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ─── Self-tests for the harness ────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Corpus generation ──────────────────────────────────────────────

    #[test]
    fn corpus_generation_is_deterministic() {
        let a = TestCorpus::generate(CorpusConfig::default());
        let b = TestCorpus::generate(CorpusConfig::default());

        assert_eq!(a.total_conversations(), b.total_conversations());
        assert_eq!(a.total_messages(), b.total_messages());

        // Content is identical across generations.
        for (ca, cb) in a.conversations.iter().zip(b.conversations.iter()) {
            assert_eq!(ca.title, cb.title);
            assert_eq!(ca.agent, cb.agent);
            assert_eq!(ca.started_at_ms, cb.started_at_ms);
            for (ma, mb) in ca.messages.iter().zip(cb.messages.iter()) {
                assert_eq!(ma.content, mb.content);
                assert_eq!(ma.role, mb.role);
            }
        }
    }

    #[test]
    fn corpus_counts_are_correct() {
        let config = CorpusConfig {
            num_conversations: 5,
            messages_per_conversation: 3,
            ..Default::default()
        };
        let corpus = TestCorpus::generate(config);
        assert_eq!(corpus.total_conversations(), 5);
        assert_eq!(corpus.total_messages(), 15);
    }

    #[test]
    fn corpus_agents_cycle_correctly() {
        let config = CorpusConfig {
            num_conversations: 6,
            agents: vec!["a".to_owned(), "b".to_owned()],
            ..Default::default()
        };
        let corpus = TestCorpus::generate(config);
        let agents: Vec<_> = corpus
            .conversations
            .iter()
            .map(|c| c.agent.as_str())
            .collect();
        assert_eq!(agents, vec!["a", "b", "a", "b", "a", "b"]);
    }

    // ── Corruption injector ────────────────────────────────────────────

    #[test]
    fn injector_corrupt_manifest_produces_parse_error() {
        let env = TestEnvironment::new();
        env.ensure_vector_dir();
        env.injector.corrupt_semantic_manifest();

        let result = SemanticManifest::load(&env.data_dir);
        assert!(result.is_err());
    }

    #[test]
    fn injector_future_version_produces_version_error() {
        let env = TestEnvironment::new();
        env.ensure_vector_dir();
        env.injector.write_future_version_manifest();

        let result = SemanticManifest::load(&env.data_dir);
        assert!(result.is_err());
    }

    #[test]
    fn injector_stale_schema_manifest_detected_as_incompatible() {
        let env = TestEnvironment::new();
        env.injector.write_stale_schema_manifest();

        let manifest = env.load_manifest().expect("manifest should load");
        let policy = SemanticPolicy::compiled_defaults();
        let readiness = manifest.quality_tier_readiness(&policy, "fp-test", "abc123");
        assert!(
            matches!(readiness, TierReadiness::Incompatible { .. }),
            "expected Incompatible, got {readiness:?}"
        );
    }

    #[test]
    fn injector_remove_manifest_returns_none_on_load() {
        let env = TestEnvironment::new();
        env.ensure_vector_dir();

        let mut manifest = SemanticManifest::default();
        env.write_manifest(&mut manifest);
        assert!(env.load_manifest().is_some());

        env.injector.remove_semantic_manifest();
        assert!(env.load_manifest().is_none());
    }

    #[test]
    fn injector_partial_build_has_checkpoint_not_ready() {
        let env = TestEnvironment::new();
        env.injector.write_partial_build("minilm-384");

        let manifest = env.load_manifest().expect("manifest should load");
        // Bead 7k7pl: pin the EXACT checkpoint shape that
        // write_partial_build seeds — embedder_id, docs_embedded,
        // conversations_processed / total_conversations ratio, and
        // db_fingerprint. A regression that emitted a default-filled
        // BuildCheckpoint (e.g., all zeros or a different embedder)
        // would slip past `.is_some()` while breaking the
        // partial-build readiness semantics this test probes.
        let checkpoint = manifest
            .checkpoint
            .as_ref()
            .expect("write_partial_build must seed a checkpoint");
        assert_eq!(checkpoint.embedder_id, "minilm-384");
        assert_eq!(checkpoint.docs_embedded, 200);
        assert_eq!(checkpoint.conversations_processed, 50);
        assert_eq!(checkpoint.total_conversations, 100);
        assert_eq!(checkpoint.db_fingerprint, "fp-partial");
        assert!(manifest.quality_tier.is_none()); // no completed artifact

        let policy = SemanticPolicy::compiled_defaults();
        let readiness = manifest.quality_tier_readiness(&policy, "fp-partial", "rev");
        assert!(
            matches!(readiness, TierReadiness::Building { progress_pct: 50 }),
            "expected Building(50), got {readiness:?}"
        );
    }

    #[test]
    fn injector_legacy_layout_no_manifest() {
        let env = TestEnvironment::new();
        env.injector
            .write_legacy_vector_index("fnv1a-384", b"legacy-index-data");

        // Manifest should be absent (legacy state).
        assert!(env.load_manifest().is_none());

        // But the index file should exist.
        let path = env.data_dir.join("vector_index/index-fnv1a-384.fsvi");
        assert!(path.exists());
    }

    // ── HarnessLog ─────────────────────────────────────────────────────

    #[test]
    fn harness_log_records_phases() {
        let mut log = HarnessLog::new();
        log.phase("init", "starting test");
        log.phase("inject", "corrupting manifest");
        log.phase("verify", "checking recovery");

        assert_eq!(log.entries().len(), 3);
        assert_eq!(log.entries()[0].phase, "init");
        assert_eq!(log.entries()[1].phase, "inject");
        assert_eq!(log.entries()[2].phase, "verify");
    }

    #[test]
    fn harness_log_jsonl_is_parseable() {
        let mut log = HarnessLog::new();
        log.phase("test", "hello world");

        let jsonl = log.to_jsonl();
        assert!(!jsonl.is_empty());
        // Bead 7k7pl: pin the SHAPE of the emitted JSONL — phase name
        // and message must both appear as distinct fields, not just
        // "valid JSON arrived". A serializer regression that emitted
        // a valid-but-empty `{}` or swapped field names would slip
        // past `.is_empty()` + `from_str(..)` probes.
        let parsed: serde_json::Value =
            serde_json::from_str(&jsonl).expect("harness log line must be valid JSON");
        assert_eq!(
            parsed.get("phase").and_then(|v| v.as_str()),
            Some("test"),
            "harness JSONL must carry the phase name verbatim; got {parsed}"
        );
        assert_eq!(
            parsed.get("msg").and_then(|v| v.as_str()),
            Some("hello world"),
            "harness JSONL must carry the phase message verbatim; got {parsed}"
        );
    }

    #[test]
    fn harness_log_snapshots_directory() {
        let env = TestEnvironment::new();
        env.ensure_vector_dir();

        // Write a test file.
        fs::write(env.data_dir.join("vector_index/test.fsvi"), b"test content").unwrap();

        let mut log = HarnessLog::new();
        log.snapshot_dir("snapshot", &env.data_dir.join("vector_index"));

        assert_eq!(log.entries().len(), 1);
        assert!(log.entries()[0].artifacts.contains_key("test.fsvi"));
    }

    // ── Golden queries ─────────────────────────────────────────────────

    #[test]
    fn golden_queries_cover_expected_scenarios() {
        let queries = golden_queries();

        // At least one query that expects hits.
        assert!(queries.iter().any(|q| q.min_hits > 0));
        // At least one query that expects zero hits.
        assert!(queries.iter().any(|q| q.min_hits == 0));
        // At least one query that targets a specific agent.
        assert!(queries.iter().any(|q| q.expected_agent.is_some()));
        // All queries work in lexical mode.
        assert!(queries.iter().all(|q| q.works_lexical));
    }

    // ── TestEnvironment ────────────────────────────────────────────────

    #[test]
    fn test_environment_setup_is_clean() {
        let env = TestEnvironment::new();
        assert!(env.data_dir.exists());
        assert_eq!(env.corpus.total_conversations(), 10);
        assert_eq!(env.corpus.total_messages(), 40);
        assert!(!env.log.entries().is_empty()); // setup phase logged
    }

    #[test]
    fn test_environment_manifest_write_and_load() {
        let env = TestEnvironment::new();
        let mut manifest = SemanticManifest::default();
        manifest.backlog.total_conversations = 42;
        env.write_manifest(&mut manifest);

        let loaded = env.load_manifest().expect("manifest should exist");
        assert_eq!(loaded.backlog.total_conversations, 42);
    }

    // ── Sample robot E2E scenario ──────────────────────────────────────

    #[test]
    fn sample_e2e_scenario_corrupt_manifest_recovery() {
        let env = TestEnvironment::new();
        let mut log = HarnessLog::new();
        let policy = SemanticPolicy::compiled_defaults();

        // Phase 1: Write a valid manifest.
        log.phase("setup", "writing initial valid manifest");
        let mut manifest = SemanticManifest {
            fast_tier: Some(ArtifactRecord {
                tier: TierKind::Fast,
                embedder_id: "fnv1a-384".to_owned(),
                model_revision: "hash".to_owned(),
                schema_version: SEMANTIC_SCHEMA_VERSION,
                chunking_version: CHUNKING_STRATEGY_VERSION,
                dimension: 384,
                doc_count: 40,
                conversation_count: 10,
                db_fingerprint: "fp-initial".to_owned(),
                index_path: "vector_index/index-fnv1a-384.fsvi".to_owned(),
                size_bytes: 1000,
                started_at_ms: 1_700_000_000_000,
                completed_at_ms: 1_700_000_001_000,
                ready: true,
            }),
            ..Default::default()
        };
        env.write_manifest(&mut manifest);
        log.snapshot_dir("after-write", &env.data_dir.join("vector_index"));

        // Phase 2: Verify initial state is ready.
        log.phase("verify-initial", "checking fast tier readiness");
        let loaded = env.load_manifest().unwrap();
        let readiness = loaded.fast_tier_readiness(&policy, "fp-initial", "hash");
        assert_eq!(readiness, TierReadiness::Ready);

        // Phase 3: Corrupt the manifest.
        log.phase("inject", "corrupting manifest file");
        env.injector.corrupt_semantic_manifest();

        // Phase 4: Verify corruption is detected.
        log.phase("verify-corrupt", "attempting to load corrupt manifest");
        let result = SemanticManifest::load(&env.data_dir);
        assert!(result.is_err(), "corrupt manifest should fail to load");

        // Phase 5: Recovery — load_or_default falls back gracefully.
        log.phase("recover", "falling back to default manifest");
        let recovered = SemanticManifest::load_or_default(&env.data_dir).unwrap();
        assert!(
            recovered.fast_tier.is_none(),
            "recovered manifest has no artifacts"
        );

        // Phase 6: Verify log has all phases.
        log.phase("done", "scenario complete");
        let phases: Vec<_> = log.entries().iter().map(|e| e.phase.as_str()).collect();
        assert_eq!(
            phases,
            vec![
                "setup",
                "after-write",
                "verify-initial",
                "inject",
                "verify-corrupt",
                "recover",
                "done"
            ]
        );

        // Dump log for CI artifact retention.
        let _jsonl = log.to_jsonl();
    }
}
