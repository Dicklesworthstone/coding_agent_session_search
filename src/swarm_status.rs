//! Fixtureable source adapters for the planned `cass swarm status` surface.
//!
//! This module intentionally avoids live provider calls. It defines the adapter
//! trait and deterministic fixture-backed implementation that the future
//! aggregator can consume without knowing whether data came from fixtures or a
//! live source.

use crate::pages::redact::{redact_swarm_json_value, redact_swarm_text};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Providers named by the swarm status contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmProviderName {
    AgentMail,
    Beads,
    CassHealth,
    CassStatus,
    DependencyDrift,
    Evidence,
    Git,
    Process,
}

impl SwarmProviderName {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentMail => "agent_mail",
            Self::Beads => "beads",
            Self::CassHealth => "cass_health",
            Self::CassStatus => "cass_status",
            Self::DependencyDrift => "dependency_drift",
            Self::Evidence => "evidence",
            Self::Git => "git",
            Self::Process => "process",
        }
    }

    #[must_use]
    pub const fn fixture_key(self) -> &'static str {
        match self {
            Self::Process => "processes",
            _ => self.as_str(),
        }
    }
}

impl fmt::Display for SwarmProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Required source providers from the current fixture contract.
pub const REQUIRED_SWARM_SOURCE_PROVIDERS: &[SwarmProviderName] = &[
    SwarmProviderName::AgentMail,
    SwarmProviderName::Beads,
    SwarmProviderName::CassHealth,
    SwarmProviderName::CassStatus,
    SwarmProviderName::Evidence,
    SwarmProviderName::Git,
    SwarmProviderName::Process,
];

/// Optional source providers available to richer status/evidence projections.
pub const OPTIONAL_SWARM_SOURCE_PROVIDERS: &[SwarmProviderName] =
    &[SwarmProviderName::DependencyDrift];

/// Every fixtureable provider named by the swarm status contract.
pub const ALL_SWARM_SOURCE_PROVIDERS: &[SwarmProviderName] = &[
    SwarmProviderName::AgentMail,
    SwarmProviderName::Beads,
    SwarmProviderName::CassHealth,
    SwarmProviderName::CassStatus,
    SwarmProviderName::DependencyDrift,
    SwarmProviderName::Evidence,
    SwarmProviderName::Git,
    SwarmProviderName::Process,
];

/// Provider availability normalized for robot output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SwarmProviderStatus {
    Ok,
    Partial,
    Unavailable,
    Skipped,
}

/// Where a diagnostic belongs. Provider stderr is kept out of stdout payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmDiagnosticStream {
    Stderr,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmProviderDiagnostic {
    pub stream: SwarmDiagnosticStream,
    pub message: String,
}

/// One provider snapshot, including typed status and raw provider payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmSourceSnapshot {
    pub name: SwarmProviderName,
    pub source: String,
    pub status: SwarmProviderStatus,
    pub freshness_ms: Option<u64>,
    pub elapsed_ms: u64,
    pub error_kind: Option<String>,
    pub warning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<SwarmProviderDiagnostic>,
    pub payload: Value,
}

impl SwarmSourceSnapshot {
    #[must_use]
    pub fn ok(name: SwarmProviderName, source: impl Into<String>, payload: Value) -> Self {
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Ok,
            freshness_ms: Some(0),
            elapsed_ms: 0,
            error_kind: None,
            warning: None,
            diagnostics: Vec::new(),
            payload: redact_swarm_json_value(&payload),
        }
    }

    #[must_use]
    pub fn partial(
        name: SwarmProviderName,
        source: impl Into<String>,
        warning: impl Into<String>,
        payload: Value,
    ) -> Self {
        let warning = warning.into();
        let warning = redact_swarm_text(&warning);
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Partial,
            freshness_ms: Some(0),
            elapsed_ms: 0,
            error_kind: None,
            warning: Some(warning.clone()),
            diagnostics: vec![SwarmProviderDiagnostic {
                stream: SwarmDiagnosticStream::Internal,
                message: warning,
            }],
            payload: redact_swarm_json_value(&payload),
        }
    }

    #[must_use]
    pub fn unavailable(
        name: SwarmProviderName,
        source: impl Into<String>,
        error_kind: impl Into<String>,
        warning: impl Into<String>,
    ) -> Self {
        let warning = warning.into();
        let warning = redact_swarm_text(&warning);
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Unavailable,
            freshness_ms: None,
            elapsed_ms: 0,
            error_kind: Some(error_kind.into()),
            warning: Some(warning.clone()),
            diagnostics: vec![SwarmProviderDiagnostic {
                stream: SwarmDiagnosticStream::Stderr,
                message: warning,
            }],
            payload: Value::Null,
        }
    }

    #[must_use]
    pub fn skipped(
        name: SwarmProviderName,
        source: impl Into<String>,
        warning: impl Into<String>,
    ) -> Self {
        let warning = warning.into();
        let warning = redact_swarm_text(&warning);
        Self {
            name,
            source: source.into(),
            status: SwarmProviderStatus::Skipped,
            freshness_ms: None,
            elapsed_ms: 0,
            error_kind: None,
            warning: Some(warning.clone()),
            diagnostics: vec![SwarmProviderDiagnostic {
                stream: SwarmDiagnosticStream::Internal,
                message: warning,
            }],
            payload: Value::Null,
        }
    }
}

/// Common interface for live and fixture-backed swarm status providers.
pub trait SwarmSourceAdapter: Send + Sync {
    fn provider(&self) -> SwarmProviderName;
    fn collect(&self) -> SwarmSourceSnapshot;
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwarmSourceCollection {
    pub snapshots: Vec<SwarmSourceSnapshot>,
}

impl SwarmSourceCollection {
    #[must_use]
    pub fn partial(&self) -> bool {
        self.snapshots
            .iter()
            .any(|snapshot| snapshot.status != SwarmProviderStatus::Ok)
    }

    #[must_use]
    pub fn snapshot(&self, provider: SwarmProviderName) -> Option<&SwarmSourceSnapshot> {
        self.snapshots
            .iter()
            .find(|snapshot| snapshot.name == provider)
    }
}

#[must_use]
pub fn collect_swarm_sources<'a, I>(adapters: I) -> SwarmSourceCollection
where
    I: IntoIterator<Item = &'a dyn SwarmSourceAdapter>,
{
    SwarmSourceCollection {
        snapshots: adapters
            .into_iter()
            .map(SwarmSourceAdapter::collect)
            .collect(),
    }
}

#[derive(Debug, Clone)]
pub struct SwarmFixtureInput {
    path: PathBuf,
    fixture_id: String,
    description: Option<String>,
    sources: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RawSwarmFixtureInput {
    fixture_id: String,
    #[serde(default)]
    description: Option<String>,
    sources: BTreeMap<String, Value>,
}

impl SwarmFixtureInput {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, SwarmSourceError> {
        let path = path.as_ref();
        let body = fs::read_to_string(path).map_err(|source| SwarmSourceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let raw = serde_json::from_str::<RawSwarmFixtureInput>(&body).map_err(|source| {
            SwarmSourceError::Json {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Self::from_raw(path.to_path_buf(), raw)
    }

    pub fn from_value(path: impl Into<PathBuf>, value: Value) -> Result<Self, SwarmSourceError> {
        let path = path.into();
        let raw = serde_json::from_value::<RawSwarmFixtureInput>(value).map_err(|source| {
            SwarmSourceError::Json {
                path: path.clone(),
                source,
            }
        })?;
        Self::from_raw(path, raw)
    }

    fn from_raw(path: PathBuf, raw: RawSwarmFixtureInput) -> Result<Self, SwarmSourceError> {
        if raw.fixture_id.trim().is_empty() {
            return Err(SwarmSourceError::InvalidFixture {
                path,
                reason: "fixture_id cannot be empty",
            });
        }
        Ok(Self {
            path,
            fixture_id: raw.fixture_id,
            description: raw.description,
            sources: raw.sources,
        })
    }

    #[must_use]
    pub fn fixture_id(&self) -> &str {
        &self.fixture_id
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn source_value(&self, provider: SwarmProviderName) -> Option<&Value> {
        self.sources.get(provider.fixture_key())
    }
}

#[derive(Debug, Clone)]
pub struct FixtureSwarmSourceAdapter {
    input: Arc<SwarmFixtureInput>,
    provider: SwarmProviderName,
}

impl FixtureSwarmSourceAdapter {
    #[must_use]
    pub fn new(input: Arc<SwarmFixtureInput>, provider: SwarmProviderName) -> Self {
        Self { input, provider }
    }
}

impl SwarmSourceAdapter for FixtureSwarmSourceAdapter {
    fn provider(&self) -> SwarmProviderName {
        self.provider
    }

    fn collect(&self) -> SwarmSourceSnapshot {
        let source = format!("fixture:{}", self.provider.fixture_key());
        match self.input.source_value(self.provider) {
            Some(value) => SwarmSourceSnapshot::ok(self.provider, source, value.clone()),
            None => SwarmSourceSnapshot::unavailable(
                self.provider,
                source,
                "missing-fixture-provider",
                format!(
                    "fixture {} at {} is missing provider source {}",
                    self.input.fixture_id(),
                    self.input.path().display(),
                    self.provider.fixture_key()
                ),
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FixtureSwarmAdapterSet {
    input: Arc<SwarmFixtureInput>,
}

impl FixtureSwarmAdapterSet {
    pub fn from_fixture_path(path: impl AsRef<Path>) -> Result<Self, SwarmSourceError> {
        Ok(Self {
            input: Arc::new(SwarmFixtureInput::from_path(path)?),
        })
    }

    #[must_use]
    pub fn from_input(input: SwarmFixtureInput) -> Self {
        Self {
            input: Arc::new(input),
        }
    }

    #[must_use]
    pub fn input(&self) -> &SwarmFixtureInput {
        &self.input
    }

    #[must_use]
    pub fn required_adapters(&self) -> Vec<FixtureSwarmSourceAdapter> {
        REQUIRED_SWARM_SOURCE_PROVIDERS
            .iter()
            .copied()
            .map(|provider| FixtureSwarmSourceAdapter::new(Arc::clone(&self.input), provider))
            .collect()
    }

    #[must_use]
    pub fn all_adapters(&self) -> Vec<FixtureSwarmSourceAdapter> {
        ALL_SWARM_SOURCE_PROVIDERS
            .iter()
            .copied()
            .map(|provider| FixtureSwarmSourceAdapter::new(Arc::clone(&self.input), provider))
            .collect()
    }

    #[must_use]
    pub fn collect_required(&self) -> SwarmSourceCollection {
        let adapters = self.required_adapters();
        collect_swarm_sources(
            adapters
                .iter()
                .map(|adapter| adapter as &dyn SwarmSourceAdapter),
        )
    }

    #[must_use]
    pub fn collect_all(&self) -> SwarmSourceCollection {
        let adapters = self.all_adapters();
        collect_swarm_sources(
            adapters
                .iter()
                .map(|adapter| adapter as &dyn SwarmSourceAdapter),
        )
    }
}

#[derive(Debug)]
pub enum SwarmSourceError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidFixture {
        path: PathBuf,
        reason: &'static str,
    },
}

impl fmt::Display for SwarmSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    f,
                    "failed to read swarm fixture {}: {source}",
                    path.display()
                )
            }
            Self::Json { path, source } => {
                write!(
                    f,
                    "failed to parse swarm fixture {}: {source}",
                    path.display()
                )
            }
            Self::InvalidFixture { path, reason } => {
                write!(f, "invalid swarm fixture {}: {reason}", path.display())
            }
        }
    }
}

impl Error for SwarmSourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::InvalidFixture { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn fixture_adapter_collects_every_required_provider_from_healthy_fixture() {
        let adapters = FixtureSwarmAdapterSet::from_fixture_path(repo_path(
            "tests/fixtures/swarm_status/healthy.inputs.json",
        ))
        .expect("healthy fixture should parse");

        let collection = adapters.collect_required();

        assert!(!collection.partial());
        assert_eq!(
            collection
                .snapshots
                .iter()
                .map(|snapshot| snapshot.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "agent_mail",
                "beads",
                "cass_health",
                "cass_status",
                "evidence",
                "git",
                "process"
            ]
        );
        assert_eq!(
            collection
                .snapshot(SwarmProviderName::Beads)
                .and_then(|snapshot| snapshot.payload["ready"].as_array())
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn missing_fixture_provider_becomes_unavailable_snapshot() {
        let input = SwarmFixtureInput::from_value(
            "inline-missing.json",
            json!({
                "fixture_id": "missing-provider",
                "sources": {
                    "beads": {"ready": []}
                }
            }),
        )
        .expect("inline fixture should parse");
        let set = FixtureSwarmAdapterSet::from_input(input);

        let collection = set.collect_required();
        let missing = collection
            .snapshot(SwarmProviderName::AgentMail)
            .expect("agent_mail snapshot should exist");

        assert!(collection.partial());
        assert_eq!(missing.status, SwarmProviderStatus::Unavailable);
        assert_eq!(
            missing.error_kind.as_deref(),
            Some("missing-fixture-provider")
        );
        assert_eq!(missing.payload, Value::Null);
        assert_eq!(
            missing
                .diagnostics
                .first()
                .map(|diagnostic| diagnostic.stream),
            Some(SwarmDiagnosticStream::Stderr)
        );
    }

    #[test]
    fn process_provider_uses_contract_name_and_fixture_key() {
        let input = SwarmFixtureInput::from_value(
            "inline-process.json",
            json!({
                "fixture_id": "process-provider",
                "sources": {
                    "processes": {"active_rch_jobs": 2}
                }
            }),
        )
        .expect("inline fixture should parse");
        let adapter = FixtureSwarmSourceAdapter::new(Arc::new(input), SwarmProviderName::Process);
        let snapshot = adapter.collect();

        assert_eq!(SwarmProviderName::Process.as_str(), "process");
        assert_eq!(SwarmProviderName::Process.fixture_key(), "processes");
        assert_eq!(snapshot.name, SwarmProviderName::Process);
        assert_eq!(snapshot.source, "fixture:processes");
        assert_eq!(snapshot.status, SwarmProviderStatus::Ok);
        assert_eq!(snapshot.payload["active_rch_jobs"], 2);
    }

    #[test]
    fn status_variants_serialize_to_contract_values() {
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Ok).unwrap(),
            r#""ok""#
        );
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Partial).unwrap(),
            r#""partial""#
        );
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Unavailable).unwrap(),
            r#""unavailable""#
        );
        assert_eq!(
            serde_json::to_string(&SwarmProviderStatus::Skipped).unwrap(),
            r#""skipped""#
        );
    }

    #[test]
    fn partial_and_skipped_snapshots_are_degraded_and_redacted() {
        let partial = SwarmSourceSnapshot::partial(
            SwarmProviderName::Git,
            "fixture:git",
            "partial fixture read at /home/alice/private-client with TOKEN=SECRET_VALUE",
            json!({
                "path": "/home/alice/private-client/src/lib.rs",
                "dirty_by_path": {
                    "/home/alice/private-client/src/lib.rs": "modified"
                },
                "command": "env TOKEN=SECRET_VALUE cargo test",
                "evidence_ref": "pack:///data/projects/private-client/session.jsonl#L44"
            }),
        );
        let skipped = SwarmSourceSnapshot::skipped(
            SwarmProviderName::Evidence,
            "fixture:evidence",
            "skipped optional evidence probe for /home/alice/private-client",
        );
        let collection = SwarmSourceCollection {
            snapshots: vec![partial, skipped],
        };

        assert!(collection.partial());
        let git = collection
            .snapshot(SwarmProviderName::Git)
            .expect("git snapshot should exist");
        let evidence = collection
            .snapshot(SwarmProviderName::Evidence)
            .expect("evidence snapshot should exist");

        assert_eq!(git.status, SwarmProviderStatus::Partial);
        assert_eq!(evidence.status, SwarmProviderStatus::Skipped);
        assert_eq!(git.diagnostics[0].stream, SwarmDiagnosticStream::Internal);
        assert_eq!(
            evidence.diagnostics[0].stream,
            SwarmDiagnosticStream::Internal
        );
        assert_eq!(git.payload["evidence_ref"], "pack://[REDACTED_PATH]#L44");
        assert!(
            git.payload["dirty_by_path"]
                .as_object()
                .is_some_and(|paths| paths.contains_key("[REDACTED_PATH]"))
        );

        let serialized =
            serde_json::to_string(&collection.snapshots).expect("snapshots should serialize");
        assert!(!serialized.contains("/home/alice"));
        assert!(!serialized.contains("/data/projects/private-client"));
        assert!(!serialized.contains("SECRET_VALUE"));
        assert!(serialized.contains("[REDACTED_PATH]"));
        assert!(serialized.contains("[SECRET_ENV_REDACTED]"));
    }

    #[test]
    fn required_adapters_collects_evidence_provider() {
        let input = SwarmFixtureInput::from_value(
            "inline-evidence.json",
            json!({
                "fixture_id": "evidence-provider",
                "sources": {
                    "agent_mail": {"messages": []},
                    "beads": {"ready": []},
                    "cass_health": {"healthy": true},
                    "cass_status": {"search_ready": true},
                    "git": {"dirty": false},
                    "processes": {"active_rch_jobs": 0},
                    "evidence": {
                        "recent_proofs": [
                            {
                                "ref": "pack:///data/projects/private-client/session.jsonl#L44",
                                "status": "redacted"
                            }
                        ]
                    }
                }
            }),
        )
        .expect("inline fixture should parse");
        let set = FixtureSwarmAdapterSet::from_input(input);

        let collection = set.collect_required();
        let evidence = collection
            .snapshot(SwarmProviderName::Evidence)
            .expect("evidence snapshot should exist");

        assert_eq!(
            collection.snapshots.len(),
            REQUIRED_SWARM_SOURCE_PROVIDERS.len()
        );
        assert!(!collection.partial());
        assert_eq!(evidence.status, SwarmProviderStatus::Ok);
        assert_eq!(evidence.source, "fixture:evidence");
        assert_eq!(
            evidence.payload["recent_proofs"][0]["ref"],
            "pack://[REDACTED_PATH]#L44"
        );
    }

    #[test]
    fn fixture_payload_strings_pass_through_redaction_layer() {
        let input = SwarmFixtureInput::from_value(
            "inline-redaction.json",
            json!({
                "fixture_id": "redaction-provider",
                "sources": {
                    "git": {
                        "dirty_paths": [
                            {"path": "/home/alice/private-client/src/lib.rs"}
                        ],
                        "dirty_by_path": {
                            "/home/alice/private-client/src/lib.rs": "modified"
                        },
                        "last_author": "alice@example.com",
                        "command": "env TOKEN=SECRET_VALUE CARGO_TARGET_DIR=/home/alice/cass-target cargo test",
                        "evidence_ref": "pack:///data/projects/private-client/session.jsonl#L44"
                    }
                }
            }),
        )
        .expect("inline fixture should parse");
        let adapter = FixtureSwarmSourceAdapter::new(Arc::new(input), SwarmProviderName::Git);
        let snapshot = adapter.collect();
        let serialized = serde_json::to_string(&snapshot.payload).expect("payload serializes");

        assert!(!serialized.contains("/home/alice"));
        assert!(!serialized.contains("/data/projects/private-client"));
        assert!(!serialized.contains("alice@example.com"));
        assert!(!serialized.contains("SECRET_VALUE"));
        assert_eq!(
            snapshot.payload["evidence_ref"],
            "pack://[REDACTED_PATH]#L44"
        );
        assert!(
            snapshot.payload["dirty_by_path"]
                .as_object()
                .is_some_and(|paths| paths.contains_key("[REDACTED_PATH]"))
        );
        assert!(serialized.contains("[REDACTED_PATH]"));
        assert!(serialized.contains("[EMAIL_REDACTED]"));
        assert!(serialized.contains("[SECRET_ENV_REDACTED]"));
    }

    #[test]
    fn collector_consumes_only_the_adapter_trait() {
        let input = Arc::new(
            SwarmFixtureInput::from_value(
                "inline-trait.json",
                json!({
                    "fixture_id": "trait-collector",
                    "sources": {
                        "beads": {"ready": []},
                        "git": {"dirty": false}
                    }
                }),
            )
            .expect("inline fixture should parse"),
        );
        let adapters = [
            FixtureSwarmSourceAdapter::new(Arc::clone(&input), SwarmProviderName::Beads),
            FixtureSwarmSourceAdapter::new(Arc::clone(&input), SwarmProviderName::Git),
        ];
        let trait_refs = adapters
            .iter()
            .map(|adapter| adapter as &dyn SwarmSourceAdapter);

        let collection = collect_swarm_sources(trait_refs);

        assert_eq!(collection.snapshots.len(), 2);
        assert_eq!(
            collection.snapshot(SwarmProviderName::Git).unwrap().status,
            SwarmProviderStatus::Ok
        );
    }

    #[test]
    fn checked_in_swarm_fixtures_provide_all_required_sources() {
        for name in [
            "healthy",
            "busy",
            "stale_advisory",
            "reservation_conflict",
            "build_pressure",
            "no_ready_work",
            "privacy_guardrails",
        ] {
            let path = repo_path(&format!("tests/fixtures/swarm_status/{name}.inputs.json"));
            let adapters = FixtureSwarmAdapterSet::from_fixture_path(path)
                .unwrap_or_else(|err| panic!("{name} fixture should parse: {err}"));
            let collection = adapters.collect_required();

            assert!(
                !collection.partial(),
                "{name} fixture should provide every required provider: {collection:#?}"
            );
        }
    }
}
