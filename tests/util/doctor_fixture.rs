#![allow(dead_code)]

use assert_cmd::Command;
use coding_agent_search::model::types::{Agent, AgentKind, Conversation};
use coding_agent_search::storage::sqlite::SqliteStorage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use tempfile::TempDir;

use super::ConversationFixtureBuilder;

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const RAW_MIRROR_SCHEMA_VERSION: u32 = 1;
const RAW_MIRROR_MANIFEST_KIND: &str = "cass_raw_session_mirror_v1";
const RAW_MIRROR_HASH_ALGORITHM: &str = "blake3";
const FIXTURE_BASE_TS_MS: i64 = 1_733_000_000_000;
const PRIVACY_SENTINEL_ID: &str = "doctor-fixture-secret-token";
const PRIVACY_SENTINEL_VALUE: &str = "CASS_DOCTOR_PRIVACY_SENTINEL_DO_NOT_LEAK";

#[derive(Debug)]
pub struct DoctorFixtureFactory {
    root: DoctorFixtureRoot,
    fixture_id: String,
    home_dir: PathBuf,
    data_dir: PathBuf,
    manifest: DoctorFixtureScenarioManifest,
}

#[derive(Debug)]
enum DoctorFixtureRoot {
    Temp(TempDir),
    Persistent(PathBuf),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixtureScenarioManifest {
    pub schema_version: u32,
    pub fixture_id: String,
    pub risk_class: String,
    pub expected_mutation_class: String,
    pub allowed_commands: Vec<String>,
    pub forbidden_live_path_patterns: Vec<String>,
    pub expected_artifact_keys: Vec<String>,
    pub redaction_policy: DoctorFixtureRedactionExpectation,
    pub repair_eligibility: String,
    pub provider_set: Vec<String>,
    pub expected_source_inventory: DoctorFixtureSourceInventoryExpectation,
    pub expected_coverage_state: String,
    pub expected_anomalies: Vec<String>,
    pub expected_mutability: DoctorFixtureMutabilityExpectation,
    pub privacy_sentinels: Vec<DoctorFixturePrivacySentinel>,
    pub cleanup_expectations: Vec<DoctorFixtureCleanupExpectation>,
    pub artifacts: Vec<DoctorFixtureArtifact>,
    pub structured_log: Vec<DoctorFixtureLogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DoctorFixtureSourceInventoryExpectation {
    pub total_conversations: usize,
    pub missing_current_source_count: usize,
    pub mirrored_source_count: usize,
    pub provider_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixtureMutabilityExpectation {
    pub doctor_check_may_mutate: bool,
    pub doctor_fix_may_mutate: bool,
    pub protected_path_classes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixtureRedactionExpectation {
    pub raw_session_text_in_default_output: bool,
    pub full_source_paths_in_default_output: bool,
    pub privacy_sentinel_in_default_output: bool,
    pub sensitive_attachments_require_opt_in: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixturePrivacySentinel {
    pub sentinel_id: String,
    pub value_blake3: String,
    pub relative_path: String,
    pub must_be_absent_from_default_output: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixtureCleanupExpectation {
    pub path_class: String,
    pub may_be_reclaimed_by_fix: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixtureArtifact {
    pub artifact_kind: String,
    pub relative_path: String,
    pub size_bytes: u64,
    pub blake3: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DoctorFixtureLogEntry {
    pub step: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorFixtureScenario {
    Healthy,
    PartiallyIndexed,
    SourcePruned,
    SourceTruncated,
    MirrorMissing,
    DbCorrupt,
    DbCorruptWithStaleIndex,
    IndexCorrupt,
    StaleLock,
    ActiveLock,
    InterruptedRepair,
    BackupAvailable,
    LowDisk,
    BackupExclusion,
    SupportBundle,
    MultiSource,
    PathEdgeCases,
}

#[derive(Debug, Clone, Copy)]
pub struct DoctorProviderSpec {
    pub slug: &'static str,
    pub name: &'static str,
    pub relative_source_path: &'static str,
    pub sample_body: &'static str,
}

#[derive(Debug, Clone)]
pub struct DoctorFixtureSource {
    pub provider: DoctorProviderSpec,
    pub source_id: String,
    pub source_path: PathBuf,
    pub conversation_id: i64,
    pub message_count: usize,
    pub mirrored: bool,
    pub pruned: bool,
    pub manifest_id: Option<String>,
}

impl Default for DoctorFixtureMutabilityExpectation {
    fn default() -> Self {
        Self {
            doctor_check_may_mutate: false,
            doctor_fix_may_mutate: true,
            protected_path_classes: vec![
                "source_session_log".to_string(),
                "raw_mirror_blob".to_string(),
                "raw_mirror_manifest".to_string(),
                "archive_database".to_string(),
                "privacy_sentinel".to_string(),
            ],
        }
    }
}

impl Default for DoctorFixtureRedactionExpectation {
    fn default() -> Self {
        Self {
            raw_session_text_in_default_output: false,
            full_source_paths_in_default_output: false,
            privacy_sentinel_in_default_output: false,
            sensitive_attachments_require_opt_in: true,
        }
    }
}

impl DoctorFixtureRoot {
    fn path(&self) -> &Path {
        match self {
            Self::Temp(temp_dir) => temp_dir.path(),
            Self::Persistent(path) => path,
        }
    }
}

impl DoctorFixtureFactory {
    pub fn new(fixture_id: impl Into<String>) -> Self {
        let fixture_id = fixture_id.into();
        assert!(
            !fixture_id.trim().is_empty(),
            "doctor fixture id must not be empty"
        );
        let temp_dir = TempDir::new().expect("create doctor fixture tempdir");
        Self::from_root(DoctorFixtureRoot::Temp(temp_dir), fixture_id)
    }

    pub fn new_under(parent: impl AsRef<Path>, fixture_id: impl Into<String>) -> Self {
        let fixture_id = fixture_id.into();
        assert!(
            !fixture_id.trim().is_empty(),
            "doctor fixture id must not be empty"
        );
        let parent = parent.as_ref();
        assert!(
            parent.is_absolute(),
            "doctor fixture persistent parent must be absolute: {}",
            parent.display()
        );
        let dirname = safe_fixture_dirname(&fixture_id);
        fs::create_dir_all(parent).expect("create persistent doctor fixture parent");
        let root = parent.join(dirname);
        assert!(
            !root.exists(),
            "doctor fixture refuses to reuse persistent root: {}",
            root.display()
        );
        fs::create_dir(&root).expect("create persistent doctor fixture root");
        Self::from_root(DoctorFixtureRoot::Persistent(root), fixture_id)
    }

    fn from_root(root: DoctorFixtureRoot, fixture_id: String) -> Self {
        let root_path = root.path();
        let home_dir = root_path.join("home");
        let data_dir = root_path.join("cass-data");
        fs::create_dir_all(&home_dir).expect("create fixture home");
        fs::create_dir_all(&data_dir).expect("create fixture data dir");
        let manifest = DoctorFixtureScenarioManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            fixture_id,
            risk_class: "healthy".to_string(),
            expected_mutation_class: "read-only".to_string(),
            allowed_commands: default_allowed_commands(),
            forbidden_live_path_patterns: default_forbidden_live_path_patterns(),
            expected_artifact_keys: default_expected_artifact_keys(),
            redaction_policy: DoctorFixtureRedactionExpectation::default(),
            repair_eligibility: "no-op".to_string(),
            provider_set: Vec::new(),
            expected_source_inventory: DoctorFixtureSourceInventoryExpectation::default(),
            expected_coverage_state: "healthy".to_string(),
            expected_anomalies: Vec::new(),
            expected_mutability: DoctorFixtureMutabilityExpectation::default(),
            privacy_sentinels: Vec::new(),
            cleanup_expectations: Vec::new(),
            artifacts: Vec::new(),
            structured_log: Vec::new(),
        };

        Self {
            root,
            fixture_id: manifest.fixture_id.clone(),
            home_dir,
            data_dir,
            manifest,
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn manifest(&self) -> &DoctorFixtureScenarioManifest {
        &self.manifest
    }

    pub fn into_manifest(self) -> DoctorFixtureScenarioManifest {
        self.manifest
    }

    pub fn cass_cmd(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        cmd.env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("XDG_DATA_HOME", &self.home_dir)
            .env("XDG_CONFIG_HOME", &self.home_dir)
            .env("HOME", &self.home_dir);
        cmd
    }

    pub fn seed_empty_archive_db(&mut self) -> &mut Self {
        fs::create_dir_all(&self.data_dir).expect("create fixture data dir");
        let db_path = self.data_dir.join("agent_search.db");
        SqliteStorage::open(&db_path).expect("create fixture archive db");
        self.log(
            "seed_empty_archive_db",
            "created frankensqlite archive schema",
        );
        self
    }

    pub fn seed_empty_search_index(&mut self) -> &mut Self {
        let out = self
            .cass_cmd()
            .args([
                "index",
                "--force-rebuild",
                "--json",
                "--data-dir",
                self.data_dir.to_str().expect("utf8 fixture data dir"),
            ])
            .output()
            .expect("run fixture cass index --json");
        assert!(
            out.status.success(),
            "fixture cass index --json failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        self.log(
            "seed_empty_search_index",
            "created empty derived search index through cass CLI",
        );
        self
    }

    pub fn add_all_provider_source_trees(&mut self) -> &mut Self {
        for provider in DoctorProviderSpec::all() {
            let _ = self.add_provider_source(provider, "local", true, false, false);
        }
        self
    }

    pub fn add_provider_source(
        &mut self,
        provider: DoctorProviderSpec,
        source_id: &str,
        source_exists: bool,
        mirror_raw: bool,
        prune_after_mirror: bool,
    ) -> DoctorFixtureSource {
        self.seed_empty_archive_db();
        self.register_provider(provider.slug);
        let source_path = if source_exists && !prune_after_mirror {
            self.confined_home_path(provider.relative_source_path)
                .expect("provider source path must be confined")
        } else {
            PathBuf::from(format!(
                "/cass-doctor-fixture/{}/{}",
                self.fixture_id, provider.relative_source_path
            ))
        };
        let source_bytes = provider.sample_body.as_bytes();
        if source_exists && !prune_after_mirror {
            self.write_confined_file(&source_path, source_bytes, "provider_source_log");
            self.write_provider_sidecars(provider, &source_path);
        }

        let conversation_id = self.insert_conversation(provider, source_id, &source_path, 2);
        self.manifest.expected_source_inventory.total_conversations += 1;
        *self
            .manifest
            .expected_source_inventory
            .provider_counts
            .entry(provider.slug.to_string())
            .or_default() += 1;

        let manifest_id = if mirror_raw {
            self.manifest
                .expected_source_inventory
                .mirrored_source_count += 1;
            let manifest = self.write_raw_mirror(
                provider,
                source_id,
                &source_path,
                source_bytes,
                conversation_id,
                2,
            );
            manifest["manifest_id"].as_str().map(ToOwned::to_owned)
        } else {
            None
        };

        if prune_after_mirror || !source_exists {
            self.log(
                "provider_source_absent",
                &format!("left absent {}", self.display_fixture_path(&source_path)),
            );
            self.manifest
                .expected_anomalies
                .push_unique("upstream-source-pruned");
            self.manifest
                .expected_source_inventory
                .missing_current_source_count += 1;
            self.manifest.expected_coverage_state = if mirror_raw {
                "source-pruned-mirror-verified".to_string()
            } else {
                "source-pruned-mirror-missing".to_string()
            };
        }

        DoctorFixtureSource {
            provider,
            source_id: source_id.to_string(),
            source_path,
            conversation_id,
            message_count: 2,
            mirrored: mirror_raw,
            pruned: prune_after_mirror || !source_exists,
            manifest_id,
        }
    }

    pub fn apply_scenario(&mut self, scenario: DoctorFixtureScenario) -> &mut Self {
        match scenario {
            DoctorFixtureScenario::Healthy => {
                self.set_contract("healthy", "read-only", "no-op");
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    false,
                    false,
                );
                self.manifest.expected_coverage_state = "healthy".to_string();
            }
            DoctorFixtureScenario::PartiallyIndexed => {
                self.set_contract(
                    "derived-asset-risk",
                    "derived-only",
                    "safe-derived-repair-eligible",
                );
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    false,
                    false,
                );
                self.write_marker("diagnostics/partial-index.fixture", b"partial-index");
                self.manifest
                    .expected_anomalies
                    .push_unique("partially-indexed");
            }
            DoctorFixtureScenario::SourcePruned => {
                self.set_contract(
                    "archive-sole-copy-risk",
                    "read-only",
                    "reconstruct-plan-required",
                );
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    true,
                    true,
                );
            }
            DoctorFixtureScenario::SourceTruncated => {
                self.set_contract(
                    "archive-sole-copy-risk",
                    "read-only",
                    "reconstruct-plan-required",
                );
                let source = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    true,
                    false,
                );
                self.overwrite_confined_file_for_fixture_drift(
                    &source.source_path,
                    b"{\"type\":\"message\",\"role\":\"user\",\"content\":\"truncated after mirror\"}\n",
                    "provider_source_truncated",
                );
                self.manifest
                    .expected_anomalies
                    .push_unique("upstream-source-truncated");
                self.manifest.expected_coverage_state =
                    "source-truncated-mirror-verified".to_string();
            }
            DoctorFixtureScenario::MirrorMissing => {
                self.set_contract(
                    "archive-authority-risk",
                    "blocked",
                    "no-safe-repair-authority",
                );
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    false,
                    false,
                    false,
                );
                self.manifest
                    .expected_anomalies
                    .push_unique("raw-mirror-missing");
            }
            DoctorFixtureScenario::DbCorrupt => {
                self.set_contract(
                    "archive-corruption-risk",
                    "blocked",
                    "reconstruct-candidate-required",
                );
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    true,
                    true,
                );
                let db_path = self.data_dir.join("agent_search.db");
                self.overwrite_confined_file_for_corruption(
                    &db_path,
                    b"not a sqlite database",
                    "archive_database_corrupt",
                );
                for suffix in ["-wal", "-shm"] {
                    let sidecar_path = db_path.with_file_name(format!("agent_search.db{suffix}"));
                    if sidecar_path.exists() {
                        self.overwrite_confined_file_for_corruption(
                            &sidecar_path,
                            b"not a sqlite sidecar",
                            "archive_database_sidecar_corrupt",
                        );
                    }
                }
                self.manifest
                    .expected_anomalies
                    .push_unique("archive-db-corrupt");
            }
            DoctorFixtureScenario::DbCorruptWithStaleIndex => {
                self.apply_scenario(DoctorFixtureScenario::DbCorrupt);
                let index_path =
                    coding_agent_search::search::tantivy::expected_index_dir(&self.data_dir);
                self.write_confined_file(
                    &index_path.join("stale-derived-segment.fixture"),
                    b"stale derived lexical fixture",
                    "derived_lexical_stale_fixture",
                );
                self.manifest
                    .expected_anomalies
                    .push_unique("derived-lexical-stale");
            }
            DoctorFixtureScenario::IndexCorrupt => {
                self.set_contract(
                    "derived-asset-risk",
                    "derived-only",
                    "safe-derived-repair-eligible",
                );
                self.write_marker("index/corrupt-derived-segment.fixture", b"corrupt-index");
                self.manifest
                    .expected_anomalies
                    .push_unique("derived-lexical-stale");
            }
            DoctorFixtureScenario::StaleLock => {
                self.set_contract("concurrency-risk", "read-only", "stale-lock-diagnosis");
                self.write_marker("locks/doctor.stale.lock", b"pid=999999\nheartbeat=0\n");
                self.manifest
                    .expected_anomalies
                    .push_unique("lock-contention");
            }
            DoctorFixtureScenario::ActiveLock => {
                self.set_contract("concurrency-risk", "read-only", "wait-required");
                self.write_marker(
                    "locks/doctor.active.lock",
                    b"pid=1\nheartbeat=1733000000000\nstate=active\n",
                );
                self.manifest
                    .expected_anomalies
                    .push_unique("active-lock-contention");
            }
            DoctorFixtureScenario::InterruptedRepair => {
                self.set_contract("repair-state-risk", "blocked", "resume-or-inspect-required");
                self.write_marker(
                    "doctor/tmp/interrupted-repair/plan.json",
                    br#"{"state":"interrupted"}"#,
                );
                self.manifest
                    .expected_anomalies
                    .push_unique("interrupted-repair");
            }
            DoctorFixtureScenario::BackupAvailable => {
                self.set_contract(
                    "backup-inspection",
                    "read-only",
                    "restore-rehearsal-eligible",
                );
                self.write_marker("backups/agent_search.db.fixture.bak", b"backup");
                self.manifest
                    .cleanup_expectations
                    .push(DoctorFixtureCleanupExpectation {
                        path_class: "backup".to_string(),
                        may_be_reclaimed_by_fix: false,
                        reason: "backup evidence is retained for operator inspection".to_string(),
                    });
            }
            DoctorFixtureScenario::LowDisk => {
                self.set_contract(
                    "storage-pressure",
                    "derived-cleanup-only",
                    "cleanup-fingerprint-required",
                );
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    true,
                    true,
                );
                self.seed_empty_search_index();
                self.write_marker("diagnostics/low-disk.fixture", b"free_bytes=1024\n");
                self.write_failed_reclaimable_generation_fixture();
                self.write_marker("backups/low-disk-agent_search.db.bak", b"backup");
                self.write_marker("doctor/receipts/prior-cleanup-receipt.json", b"receipt");
                self.write_marker(
                    "doctor/support-bundles/prior-support-bundle.json",
                    b"support",
                );
                self.write_marker("sources.toml", b"# low disk source config\n");
                self.write_marker("bookmarks.json", b"[]");
                self.manifest.allowed_commands = vec![
                    "cass doctor cleanup --json".to_string(),
                    "cass doctor cleanup --yes --plan-fingerprint <fingerprint> --json".to_string(),
                ];
                self.manifest
                    .cleanup_expectations
                    .push(DoctorFixtureCleanupExpectation {
                        path_class: "failed_derived_lexical_generation".to_string(),
                        may_be_reclaimed_by_fix: true,
                        reason: "failed derived generation can be rebuilt from the canonical archive DB after explicit cleanup fingerprint approval".to_string(),
                    });
                for (path_class, reason) in [
                    (
                        "raw_mirror",
                        "raw mirrors may be the only remaining session archive copy",
                    ),
                    (
                        "backup",
                        "backup evidence requires explicit restore/export policy",
                    ),
                    ("receipt", "operation receipts are audit evidence"),
                    ("support_bundle", "support bundles are diagnostic evidence"),
                    ("config", "operator configuration is never cleanup material"),
                    ("bookmark", "bookmarks are user state, not derived cache"),
                ] {
                    self.manifest
                        .cleanup_expectations
                        .push(DoctorFixtureCleanupExpectation {
                            path_class: path_class.to_string(),
                            may_be_reclaimed_by_fix: false,
                            reason: reason.to_string(),
                        });
                }
                self.manifest
                    .expected_anomalies
                    .push_unique("storage-pressure");
            }
            DoctorFixtureScenario::BackupExclusion => {
                self.set_contract("archive-preservation-risk", "read-only", "warn-only");
                fs::create_dir_all(self.root().join(".git")).expect("create fixture repo marker");
                let repo_gitignore = self.root().join(".gitignore");
                self.write_confined_file(
                    &repo_gitignore,
                    b"cass-data/raw-mirror/**\n",
                    "repo_gitignore_exclusion",
                );
                self.write_marker(".rsync-filter", b"- backups/**\n- doctor/receipts/**\n");
                self.write_marker(
                    "config.toml",
                    b"backup_exclude = [\"doctor/support-bundles/**\"]\n",
                );
                self.write_marker(
                    "backup-policy/exclusion-risk.fixture",
                    b"fixture creates real .gitignore, .rsync-filter, and config.toml exclusion evidence\n",
                );
                self.manifest
                    .expected_anomalies
                    .push_unique("config-exclusion-risk");
            }
            DoctorFixtureScenario::SupportBundle => {
                self.set_contract("privacy-risk", "read-only", "support-bundle-eligible");
                self.add_privacy_sentinel();
            }
            DoctorFixtureScenario::MultiSource => {
                self.set_contract("source-sync-risk", "read-only", "sync-gap-analysis");
                let _ = self.add_provider_source(
                    DoctorProviderSpec::codex(),
                    "local",
                    true,
                    false,
                    false,
                );
                let _ = self.add_provider_source(
                    DoctorProviderSpec::cline(),
                    "work-laptop",
                    true,
                    false,
                    false,
                );
                self.manifest.expected_coverage_state = "multi-source".to_string();
            }
            DoctorFixtureScenario::PathEdgeCases => {
                self.set_contract("path-safety-risk", "read-only", "fixture-validation-only");
                self.write_marker("diagnostics/path-edge-case.fixture", b"path-edge-case\n");
                self.manifest
                    .expected_anomalies
                    .push_unique("path-edge-case");
            }
        }
        self
    }

    fn write_provider_sidecars(&mut self, provider: DoctorProviderSpec, source_path: &Path) {
        if provider.slug != "cline" {
            return;
        }
        let Some(task_dir) = source_path.parent() else {
            return;
        };
        let sidecar_path = task_dir.join("task_metadata.json");
        self.write_confined_file(
            &sidecar_path,
            br#"{"title":"Doctor fixture Cline task","rootPath":"/fixture/project"}"#,
            "provider_source_sidecar",
        );
    }

    fn set_contract(
        &mut self,
        risk_class: &str,
        expected_mutation_class: &str,
        repair_eligibility: &str,
    ) {
        self.manifest.risk_class = risk_class.to_string();
        self.manifest.expected_mutation_class = expected_mutation_class.to_string();
        self.manifest.repair_eligibility = repair_eligibility.to_string();
    }

    pub fn add_privacy_sentinel(&mut self) -> &mut Self {
        let sentinel_path = self
            .confined_data_path("support-bundle-input/private-session.txt")
            .expect("privacy sentinel path");
        self.write_confined_file(
            &sentinel_path,
            PRIVACY_SENTINEL_VALUE.as_bytes(),
            "privacy_sentinel",
        );
        self.manifest
            .privacy_sentinels
            .push(DoctorFixturePrivacySentinel {
                sentinel_id: PRIVACY_SENTINEL_ID.to_string(),
                value_blake3: blake3_hex(PRIVACY_SENTINEL_VALUE.as_bytes()),
                relative_path: self.relative_to_root(&sentinel_path),
                must_be_absent_from_default_output: true,
            });
        self.manifest
            .expected_anomalies
            .push_unique("privacy-redaction-required");
        self
    }

    pub fn confined_home_path(&self, relative: &str) -> Result<PathBuf, String> {
        self.confined_path(&self.home_dir, relative)
    }

    pub fn confined_data_path(&self, relative: &str) -> Result<PathBuf, String> {
        self.confined_path(&self.data_dir, relative)
    }

    pub fn validate_manifest(&self) -> Result<(), String> {
        self.manifest.validate_against_root(self.root())
    }

    pub fn assert_doctor_payload_matches_manifest(&self, payload: &Value) {
        let expected = &self.manifest.expected_source_inventory;
        let archive_db_corrupt = self
            .manifest
            .expected_anomalies
            .iter()
            .any(|anomaly| anomaly == "archive-db-corrupt");
        if archive_db_corrupt {
            assert!(
                payload["raw_mirror"]["status"].as_str() == Some("verified"),
                "corrupt archive fixtures rely on verified raw mirror evidence for reconstruction"
            );
        } else {
            assert_eq!(
                payload["source_inventory"]["total_indexed_conversations"].as_u64(),
                Some(expected.total_conversations as u64),
                "doctor source inventory total_indexed_conversations should match fixture manifest"
            );
            assert_eq!(
                payload["source_inventory"]["missing_current_source_count"].as_u64(),
                Some(expected.missing_current_source_count as u64),
                "doctor source inventory missing_current_source_count should match fixture manifest"
            );
            for (provider, count) in &expected.provider_counts {
                assert_eq!(
                    payload["source_inventory"]["provider_counts"][provider].as_u64(),
                    Some(*count as u64),
                    "doctor provider count for {provider} should match fixture manifest"
                );
            }
        }
        if expected.mirrored_source_count > 0 {
            assert_eq!(
                payload["raw_mirror"]["summary"]["manifest_count"].as_u64(),
                Some(expected.mirrored_source_count as u64),
                "doctor raw_mirror manifest_count should match fixture manifest"
            );
        }
        if self.manifest.expected_coverage_state == "source-pruned-mirror-verified" {
            assert_eq!(
                payload["raw_mirror"]["status"].as_str(),
                Some("verified"),
                "doctor raw_mirror status should prove pruned-source evidence is verified"
            );
        }
        if self
            .manifest
            .expected_anomalies
            .iter()
            .any(|anomaly| anomaly == "upstream-source-pruned")
            && !archive_db_corrupt
        {
            assert!(
                expected.missing_current_source_count > 0,
                "upstream-source-pruned fixtures should declare a missing current source"
            );
            assert!(
                payload["source_inventory"]["missing_current_source_count"]
                    .as_u64()
                    .is_some_and(|count| count > 0),
                "doctor source_inventory should report the pruned upstream source"
            );
        }
    }

    fn insert_conversation(
        &self,
        provider: DoctorProviderSpec,
        source_id: &str,
        source_path: &Path,
        message_count: usize,
    ) -> i64 {
        let storage = SqliteStorage::open(&self.data_dir.join("agent_search.db"))
            .expect("open fixture archive db");
        let agent_id = storage
            .ensure_agent(&Agent {
                id: None,
                slug: provider.slug.to_string(),
                name: provider.name.to_string(),
                version: Some("fixture".to_string()),
                kind: AgentKind::Cli,
            })
            .expect("ensure fixture agent");
        let workspace = self
            .confined_home_path("workspaces/fixture-project")
            .expect("workspace path");
        let workspace_id = storage
            .ensure_workspace(&workspace, Some("fixture-project"))
            .expect("ensure fixture workspace");
        let mut conv: Conversation = ConversationFixtureBuilder::new(provider.slug)
            .external_id(format!("{}-{source_id}-{}", provider.slug, self.fixture_id))
            .workspace(workspace)
            .source_path(source_path)
            .base_ts(FIXTURE_BASE_TS_MS)
            .messages(message_count)
            .with_content(
                0,
                format!("{} fixture source for {}", provider.slug, self.fixture_id),
            )
            .build_conversation();
        conv.source_id = source_id.to_string();
        let outcome = storage
            .insert_conversation_tree(agent_id, Some(workspace_id), &conv)
            .expect("insert fixture conversation");
        outcome.conversation_id
    }

    fn write_raw_mirror(
        &mut self,
        provider: DoctorProviderSpec,
        source_id: &str,
        original_path: &Path,
        bytes: &[u8],
        conversation_id: i64,
        message_count: usize,
    ) -> Value {
        let blob_blake3 = blake3_hex(bytes);
        let blob_relative_path = format!("blobs/blake3/{}/{}.raw", &blob_blake3[..2], blob_blake3);
        let original_path_str = original_path.to_string_lossy().into_owned();
        let original_path_blake3 = raw_original_path_blake3(&original_path_str);
        let origin_kind = fixture_origin_kind(source_id);
        let origin_host = (origin_kind == "ssh").then_some(source_id);
        let manifest_id = canonical_blake3(
            "doctor-raw-mirror-manifest-id-v1",
            json!({
                "provider": provider.slug,
                "source_id": source_id,
                "origin_kind": origin_kind,
                "origin_host": origin_host,
                "original_path_blake3": original_path_blake3,
                "blob_blake3": blob_blake3,
            }),
        );
        let mut manifest = json!({
            "schema_version": RAW_MIRROR_SCHEMA_VERSION,
            "manifest_kind": RAW_MIRROR_MANIFEST_KIND,
            "manifest_id": manifest_id,
            "blob_hash_algorithm": RAW_MIRROR_HASH_ALGORITHM,
            "blob_blake3": blob_blake3,
            "blob_relative_path": blob_relative_path,
            "blob_size_bytes": bytes.len() as u64,
            "provider": provider.slug,
            "source_id": source_id,
            "origin_kind": origin_kind,
            "origin_host": origin_host,
            "original_path": original_path_str,
            "redacted_original_path": format!("[{}]/{}", provider.slug, original_path.file_name().and_then(|name| name.to_str()).unwrap_or("session")),
            "original_path_blake3": original_path_blake3,
            "captured_at_ms": FIXTURE_BASE_TS_MS,
            "source_mtime_ms": FIXTURE_BASE_TS_MS,
            "source_size_bytes": bytes.len() as u64,
            "compression": {
                "state": "none",
                "algorithm": Value::Null,
                "uncompressed_size_bytes": bytes.len() as u64
            },
            "encryption": {
                "state": "none",
                "algorithm": Value::Null,
                "key_id": Value::Null,
                "envelope_version": Value::Null
            },
            "db_links": [{
                "conversation_id": conversation_id,
                "message_count": message_count,
                "source_path": original_path.to_string_lossy(),
                "started_at_ms": FIXTURE_BASE_TS_MS
            }],
            "verification": {
                "status": "captured",
                "verifier": "doctor_fixture_factory",
                "content_blake3": Value::Null,
                "verified_at_ms": Value::Null
            }
        });
        let manifest_blake3 = canonical_blake3("doctor-raw-mirror-manifest-v1", manifest.clone());
        manifest["manifest_blake3"] = json!(manifest_blake3);

        let root = self.data_dir.join("raw-mirror/v1");
        let blob_path = root.join(manifest["blob_relative_path"].as_str().expect("blob path"));
        self.write_confined_file(&blob_path, bytes, "raw_mirror_blob");
        let manifest_path = root.join("manifests").join(format!(
            "{}.json",
            manifest["manifest_id"].as_str().expect("manifest id")
        ));
        self.write_confined_file(
            &manifest_path,
            &serde_json::to_vec_pretty(&manifest).expect("raw mirror manifest json"),
            "raw_mirror_manifest",
        );
        manifest
    }

    fn write_marker(&mut self, relative_data_path: &str, bytes: &[u8]) {
        let path = self
            .confined_data_path(relative_data_path)
            .expect("marker path must be confined");
        self.write_confined_file(&path, bytes, "scenario_marker");
    }

    fn write_failed_reclaimable_generation_fixture(&mut self) {
        let generation_dir = self
            .confined_data_path("index/generation-failed-reclaimable")
            .expect("failed generation path must be confined");
        let manifest_path = generation_dir.join("lexical-generation-manifest.json");
        self.write_confined_file(
            &manifest_path,
            &serde_json::to_vec_pretty(&json!({
                "manifest_version": 3,
                "generation_id": "gen-failed-reclaimable",
                "attempt_id": "attempt-1",
                "created_at_ms": FIXTURE_BASE_TS_MS,
                "updated_at_ms": FIXTURE_BASE_TS_MS + 321,
                "source_db_fingerprint": "fixture-db-fingerprint",
                "conversation_count": 1,
                "message_count": 2,
                "indexed_doc_count": 0,
                "equivalence_manifest_fingerprint": null,
                "shard_plan": null,
                "build_budget": null,
                "shards": [{
                    "shard_id": "shard-failed",
                    "shard_ordinal": 0,
                    "state": "abandoned",
                    "updated_at_ms": FIXTURE_BASE_TS_MS + 222,
                    "indexed_doc_count": 0,
                    "message_count": 0,
                    "artifact_bytes": 192,
                    "stable_hash": null,
                    "reclaimable": true,
                    "pinned": false,
                    "recovery_reason": "failed generation can be rebuilt from canonical SQLite",
                    "quarantine_reason": null
                }],
                "merge_debt": {
                    "state": "none",
                    "updated_at_ms": null,
                    "pending_shard_count": 0,
                    "pending_artifact_bytes": 0,
                    "reason": null,
                    "controller_reason": null
                },
                "build_state": "failed",
                "publish_state": "staged",
                "failure_history": [{
                    "attempt_id": "attempt-1",
                    "at_ms": FIXTURE_BASE_TS_MS + 300,
                    "phase": "validate",
                    "message": "fixture open probe failed before publish"
                }]
            }))
            .expect("failed generation manifest JSON"),
            "failed_derived_generation_manifest",
        );
        self.write_confined_file(
            &generation_dir.join("segment-failed"),
            b"failed derived generation bytes",
            "failed_derived_generation_segment",
        );
    }

    fn write_confined_file(&mut self, path: &Path, bytes: &[u8], kind: &str) {
        assert!(
            path.starts_with(self.root()),
            "doctor fixture write escaped temp root: {}",
            path.display()
        );
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture parent");
        }
        if path.exists() {
            let existing = fs::read(path).expect("read existing doctor fixture file");
            assert_eq!(
                existing,
                bytes,
                "doctor fixture refuses to overwrite existing fixture file with different bytes: {}",
                path.display()
            );
            self.record_file(kind, path);
            self.log(
                "reuse_file",
                &format!("{kind}:{}", self.relative_to_root(path)),
            );
            return;
        }
        fs::write(path, bytes).expect("write doctor fixture file");
        self.record_file(kind, path);
        self.log(
            "write_file",
            &format!("{kind}:{}", self.relative_to_root(path)),
        );
    }

    fn overwrite_confined_file_for_corruption(&mut self, path: &Path, bytes: &[u8], kind: &str) {
        assert!(
            path.starts_with(self.root()),
            "doctor fixture overwrite escaped temp root: {}",
            path.display()
        );
        assert!(
            path.exists() && path.is_file(),
            "doctor fixture corruption target must already be a file: {}",
            path.display()
        );
        fs::write(path, bytes).expect("overwrite doctor fixture file for corruption scenario");
        self.record_file(kind, path);
        self.log(
            "overwrite_file_for_corruption",
            &format!("{kind}:{}", self.relative_to_root(path)),
        );
    }

    fn overwrite_confined_file_for_fixture_drift(&mut self, path: &Path, bytes: &[u8], kind: &str) {
        assert!(
            path.starts_with(self.root()),
            "doctor fixture drift write escaped temp root: {}",
            path.display()
        );
        assert!(
            path.exists() && path.is_file(),
            "doctor fixture drift target must already be a file: {}",
            path.display()
        );
        fs::write(path, bytes).expect("overwrite doctor fixture file for drift scenario");
        let relative_path = self.relative_to_root(path);
        let blake3 = blake3_hex(bytes);
        for artifact in &mut self.manifest.artifacts {
            if artifact.relative_path == relative_path {
                artifact.size_bytes = bytes.len() as u64;
                artifact.blake3 = blake3.clone();
            }
        }
        self.record_file(kind, path);
        self.log(
            "overwrite_file_for_fixture_drift",
            &format!("{kind}:{}", self.relative_to_root(path)),
        );
    }

    fn record_file(&mut self, kind: &str, path: &Path) {
        if !path.exists() || !path.is_file() {
            return;
        }
        let bytes = fs::read(path).expect("read fixture file for hash");
        let relative_path = self.relative_to_root(path);
        if self.manifest.artifacts.iter().any(|artifact| {
            artifact.relative_path == relative_path && artifact.artifact_kind == kind
        }) {
            return;
        }
        self.manifest.artifacts.push(DoctorFixtureArtifact {
            artifact_kind: kind.to_string(),
            relative_path,
            size_bytes: bytes.len() as u64,
            blake3: blake3_hex(&bytes),
        });
        self.manifest
            .artifacts
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    }

    fn register_provider(&mut self, provider: &str) {
        self.manifest.provider_set.push_unique(provider);
    }

    fn log(&mut self, step: &str, detail: &str) {
        self.manifest.structured_log.push(DoctorFixtureLogEntry {
            step: step.to_string(),
            detail: detail.to_string(),
        });
    }

    fn relative_to_root(&self, path: &Path) -> String {
        path.strip_prefix(self.root())
            .expect("fixture path should be under root")
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn display_fixture_path(&self, path: &Path) -> String {
        path.strip_prefix(self.root())
            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
    }

    fn confined_path(&self, base: &Path, relative: &str) -> Result<PathBuf, String> {
        if relative.trim().is_empty() {
            return Err("relative path is empty".to_string());
        }
        let path = Path::new(relative);
        if path.is_absolute() {
            return Err("fixture path must be relative".to_string());
        }
        let mut clean = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => clean.push(part),
                Component::CurDir => {}
                Component::ParentDir => return Err("fixture path must not contain ..".to_string()),
                Component::RootDir | Component::Prefix(_) => {
                    return Err("fixture path must stay under fixture root".to_string());
                }
            }
        }
        if clean.as_os_str().is_empty() {
            return Err("fixture path has no normal components".to_string());
        }
        let joined = base.join(clean);
        if !joined.starts_with(self.root()) {
            return Err("fixture path escaped temp root".to_string());
        }
        Ok(joined)
    }
}

impl DoctorFixtureScenarioManifest {
    pub fn validate_against_root(&self, root: &Path) -> Result<(), String> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {}",
                self.schema_version
            ));
        }
        if self.fixture_id.trim().is_empty() {
            return Err("fixture_id must not be empty".to_string());
        }
        validate_non_empty_field("risk_class", &self.risk_class)?;
        validate_non_empty_field("expected_mutation_class", &self.expected_mutation_class)?;
        validate_non_empty_field("repair_eligibility", &self.repair_eligibility)?;
        validate_non_empty_list("allowed_commands", &self.allowed_commands)?;
        validate_non_empty_list(
            "forbidden_live_path_patterns",
            &self.forbidden_live_path_patterns,
        )?;
        validate_non_empty_list("expected_artifact_keys", &self.expected_artifact_keys)?;
        for command in &self.allowed_commands {
            if command == "cass" || (!command.contains("--json") && !command.contains("--robot")) {
                return Err(format!(
                    "allowed command must be non-interactive and machine-readable: {command}"
                ));
            }
        }
        for pattern in &self.forbidden_live_path_patterns {
            validate_manifest_relative_path(pattern)?;
        }
        for required in default_expected_artifact_keys() {
            if !self
                .expected_artifact_keys
                .iter()
                .any(|key| key == &required)
            {
                return Err(format!(
                    "expected_artifact_keys is missing required key {required}"
                ));
            }
        }
        if self.redaction_policy.raw_session_text_in_default_output
            || self.redaction_policy.full_source_paths_in_default_output
            || self.redaction_policy.privacy_sentinel_in_default_output
        {
            return Err("default redaction policy allows sensitive output".to_string());
        }
        let mut seen = BTreeSet::new();
        for provider in &self.provider_set {
            if provider.trim().is_empty() {
                return Err("provider_set contains an empty provider".to_string());
            }
            if !seen.insert(provider) {
                return Err(format!(
                    "provider_set contains duplicate provider {provider}"
                ));
            }
        }
        for artifact in &self.artifacts {
            validate_manifest_relative_path(&artifact.relative_path)?;
            let absolute = root.join(&artifact.relative_path);
            if !absolute.starts_with(root) {
                return Err(format!(
                    "artifact {} escapes fixture root",
                    artifact.relative_path
                ));
            }
            if !absolute.exists() {
                return Err(format!(
                    "artifact {} is listed but missing on disk",
                    artifact.relative_path
                ));
            }
            let bytes = fs::read(&absolute).map_err(|err| {
                format!(
                    "artifact {} could not be read for validation: {err}",
                    artifact.relative_path
                )
            })?;
            if bytes.len() as u64 != artifact.size_bytes {
                return Err(format!(
                    "artifact {} size drifted: manifest={} actual={}",
                    artifact.relative_path,
                    artifact.size_bytes,
                    bytes.len()
                ));
            }
            let actual_hash = blake3_hex(&bytes);
            if actual_hash != artifact.blake3 {
                return Err(format!(
                    "artifact {} checksum drifted: manifest={} actual={actual_hash}",
                    artifact.relative_path, artifact.blake3
                ));
            }
        }
        for sentinel in &self.privacy_sentinels {
            validate_manifest_relative_path(&sentinel.relative_path)?;
            if sentinel.sentinel_id == PRIVACY_SENTINEL_VALUE
                || sentinel.value_blake3 == PRIVACY_SENTINEL_VALUE
            {
                return Err("privacy sentinel raw value leaked into manifest".to_string());
            }
        }
        Ok(())
    }
}

fn default_allowed_commands() -> Vec<String> {
    vec!["cass doctor --json".to_string()]
}

fn default_forbidden_live_path_patterns() -> Vec<String> {
    vec![
        "real-home/.codex".to_string(),
        "real-home/.claude".to_string(),
        "real-home/.config/cass".to_string(),
        "real-home/.local/share/cass".to_string(),
        "current-repo/.beads".to_string(),
    ]
}

pub fn default_expected_artifact_keys() -> Vec<String> {
    vec![
        "scenario_json".to_string(),
        "fixture_inventory".to_string(),
        "source_inventory_before".to_string(),
        "source_inventory_after".to_string(),
        "execution_flow".to_string(),
        "commands_jsonl".to_string(),
        "stdout_doctor_json".to_string(),
        "stderr_doctor_json".to_string(),
        "parsed_json_doctor_json".to_string(),
        "candidate_staging".to_string(),
        "post_repair_probes".to_string(),
        "file_tree_before".to_string(),
        "file_tree_after".to_string(),
        "checksums".to_string(),
        "timing".to_string(),
        "receipts".to_string(),
        "doctor_logs".to_string(),
        "redaction_report".to_string(),
    ]
}

fn validate_non_empty_field(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_non_empty_list(field: &str, values: &[String]) -> Result<(), String> {
    if values.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    for value in values {
        validate_non_empty_field(field, value)?;
    }
    Ok(())
}

impl DoctorProviderSpec {
    pub fn all() -> Vec<Self> {
        vec![
            Self::claude_code(),
            Self::codex(),
            Self::cursor(),
            Self::gemini(),
            Self::aider(),
            Self::amp(),
            Self::cline(),
            Self::opencode(),
            Self::pi_agent(),
            Self::copilot(),
            Self::openclaw(),
            Self::clawdbot(),
            Self::vibe(),
            Self::chatgpt(),
            Self::fad_backed(),
        ]
    }

    pub fn claude_code() -> Self {
        Self::new(
            "claude_code",
            "Claude Code",
            ".claude/projects/demo/session.jsonl",
        )
    }

    pub fn codex() -> Self {
        Self::new(
            "codex",
            "Codex",
            ".codex/sessions/2026/05/05/rollout-fixture.jsonl",
        )
    }

    pub fn cursor() -> Self {
        Self::new(
            "cursor",
            "Cursor",
            ".config/Cursor/User/globalStorage/state.vscdb",
        )
    }

    pub fn gemini() -> Self {
        Self::new("gemini", "Gemini", ".gemini/tmp/demo/chats/session.json")
    }

    pub fn aider() -> Self {
        Self::new("aider", "Aider", "project/.aider.chat.history.md")
    }

    pub fn amp() -> Self {
        Self::new(
            "amp",
            "Amp",
            ".config/sourcegraph/amp/sessions/session.json",
        )
    }

    pub fn cline() -> Self {
        Self::new(
            "cline",
            "Cline",
            ".config/Code/User/globalStorage/saoudrizwan.claude-dev/tasks/task/ui_messages.json",
        )
    }

    pub fn opencode() -> Self {
        Self::new("opencode", "OpenCode", ".local/share/opencode/opencode.db")
    }

    pub fn pi_agent() -> Self {
        Self::new("pi_agent", "Pi Agent", ".pi-agent/sessions/session.jsonl")
    }

    pub fn copilot() -> Self {
        Self::new("copilot", "Copilot", ".config/github-copilot/chat.json")
    }

    pub fn openclaw() -> Self {
        Self::new("openclaw", "OpenClaw", ".openclaw/sessions/session.jsonl")
    }

    pub fn clawdbot() -> Self {
        Self::new("clawdbot", "ClawdBot", ".clawdbot/sessions/session.jsonl")
    }

    pub fn vibe() -> Self {
        Self::new("vibe", "Vibe", ".vibe/sessions/session.jsonl")
    }

    pub fn chatgpt() -> Self {
        Self::new(
            "chatgpt",
            "ChatGPT",
            ".config/cass/chatgpt/conversations.json",
        )
    }

    pub fn fad_backed() -> Self {
        Self::new(
            "fad_generic",
            "FAD-backed Provider",
            ".local/share/franken-agent-detection/provider-session.jsonl",
        )
    }

    fn new(slug: &'static str, name: &'static str, relative_source_path: &'static str) -> Self {
        Self {
            slug,
            name,
            relative_source_path,
            sample_body: "{\"type\":\"fixture\",\"message\":\"doctor fixture source\"}\n",
        }
    }
}

trait PushUnique {
    fn push_unique(&mut self, value: &str);
}

impl PushUnique for Vec<String> {
    fn push_unique(&mut self, value: &str) {
        if !self.iter().any(|existing| existing == value) {
            self.push(value.to_string());
            self.sort();
        }
    }
}

fn validate_manifest_relative_path(relative: &str) -> Result<(), String> {
    let path = Path::new(relative);
    if relative.trim().is_empty() || path.is_absolute() {
        return Err(format!("invalid manifest relative path {relative:?}"));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "manifest relative path contains unsafe component: {relative}"
                ));
            }
        }
    }
    Ok(())
}

fn canonical_json_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut canonical = serde_json::Map::new();
            for (key, value) in entries {
                canonical.insert(key, canonical_json_value(value));
            }
            Value::Object(canonical)
        }
        other => other,
    }
}

fn canonical_blake3(prefix: &str, value: Value) -> String {
    let canonical = canonical_json_value(value);
    let encoded = serde_json::to_vec(&canonical).expect("canonical json");
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix.as_bytes());
    hasher.update(&[0]);
    hasher.update(&encoded);
    format!("{prefix}-{}", hasher.finalize().to_hex())
}

fn raw_original_path_blake3(path: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"doctor-raw-mirror-original-path-v1");
    hasher.update(&[0]);
    hasher.update(path.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn fixture_origin_kind(source_id: &str) -> &'static str {
    if source_id == "local" { "local" } else { "ssh" }
}

fn safe_fixture_dirname(fixture_id: &str) -> String {
    assert!(
        fixture_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
        "doctor fixture id must be path-safe for persistent roots: {fixture_id:?}"
    );
    assert!(
        fixture_id != "." && fixture_id != "..",
        "doctor fixture id must not be a path traversal component"
    );
    fixture_id.to_string()
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
