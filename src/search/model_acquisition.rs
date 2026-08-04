// Dead-code tolerated module-wide: this is the hardened model-acquisition
// contract for bead cass-fleet-resilience-20260608-uojcg.5.5. The classifier,
// report, and on-disk probe land here; [`VerifiedMarker::render`] is wired into
// the `--from-file` install path (air-gapped installs record source provenance
// + a model fingerprint) and [`live_acquisition_block`] is wired into `cass
// models status` / `cass models verify` (bead 0qxwg). Remaining types still
// feed the dependent .5.4 truthful-fallback + the .11.5 integrated E2E gate, so
// the module-wide allow stays until those land.
#![allow(dead_code)]

//! Hardened semantic model acquisition contract (bead
//! cass-fleet-resilience-20260608-uojcg.5.5).
//!
//! The on-disk model lifecycle is already classified by
//! [`crate::search::model_download::ModelCacheState`] (`not_acquired`,
//! `acquiring`, `acquired`, `checksum_mismatch`, ...), and the semantic
//! readiness reason vocabulary lives in
//! [`crate::search::semantic_readiness`]. Both answer "are the model files on
//! disk and checksum-valid?" — neither can answer **"even if the files are
//! present and valid, can this model actually be loaded and run on *this*
//! host?"** A checksum-valid model still cannot run in the pre-AVX2
//! `-baseline` artifact (the `semantic` feature is compiled out, see
//! `src/main.rs`), on an x86_64 CPU without AVX2 (the ONNX runtime would
//! `SIGILL`), or when the ONNX graph fails to initialize. Those situations
//! must not masquerade as a generic "semantic unavailable".
//!
//! This module adds the missing **runtime-load dimension**
//! ([`RuntimeLoadability`]) and folds it together with the on-disk signals and
//! policy into a single precise [`ModelAcquisitionState`] plus a hardened
//! [`ModelAcquisitionReport`]. The report carries exactly the fields the .5.5
//! acceptance names: a stable [`ModelFingerprint`], the install
//! [`ModelSource`] provenance, the expected download [`DownloadCostClass`], a
//! typed [`SkippedNetworkReason`] that *proves* cass never auto-downloads, a
//! safe (never-destructive) next command, and rollback/cleanup guidance for
//! corrupt or air-gapped installs.
//!
//! The classifier operates on an explicit [`ModelAcquisitionSignals`] input so
//! it is fully testable without any model bits; [`probe_on_disk`] builds the
//! on-disk portion of those signals from real files (reusing
//! [`crate::search::model_download::compute_sha256`] and `model_file_path`)
//! and performs **filesystem reads only — never any network I/O**. A bridge
//! ([`ModelAcquisitionSignals::semantic_model_inputs`]) feeds the readiness
//! classifier in [`crate::search::semantic_readiness`], so the two contracts
//! compose instead of drifting. All enums serialize as snake_case.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::search::model_download::{ModelManifest, compute_sha256, model_file_path};
use crate::search::semantic_readiness::FallbackMode;

/// The on-disk name of the post-install verification marker. Kept identical to
/// the legacy marker written by the downloader so the existing
/// `classify_model_cache` reader keeps working unchanged.
pub(crate) const VERIFIED_MARKER_NAME: &str = ".verified";

/// Optional quarantine marker dropped beside the model files when a previously
/// installed cache is found corrupt. Detected by [`probe_on_disk`].
pub(crate) const QUARANTINE_MARKER_NAME: &str = ".quarantined";

// ----------------------------------------------------------------------------
// Runtime loadability — the dimension on-disk classification cannot see.
// ----------------------------------------------------------------------------

/// Whether an on-disk-present, checksum-valid model can actually be **loaded
/// and run** on this host.
///
/// On-disk classification cannot answer this. The cheap variants
/// ([`Self::BaselineBuildNoSemantic`], [`Self::IncompatibleCpu`]) are
/// detectable without touching the ONNX runtime — so a hot status probe can
/// report them without risking a `SIGILL`. [`Self::OnnxLoadFailed`] and
/// [`Self::Loadable`] are only knowable after an actual load attempt, so they
/// are reported by deeper surfaces (`cass models verify`, search-time load),
/// never by hot status. [`Self::NotProbed`] means the cheap checks passed and
/// no load was attempted — a status surface treats that as "would be loadable".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeLoadability {
    /// No load attempted; the cheap CPU/build checks passed.
    NotProbed,
    /// The embedder initialized successfully on this host.
    Loadable,
    /// This is the `-baseline` artifact: the `semantic` feature is compiled
    /// out, so no model can ever load regardless of the on-disk files.
    BaselineBuildNoSemantic,
    /// A semantic-enabled build running on an x86_64 CPU without AVX2; loading
    /// the ONNX runtime would crash with `SIGILL`.
    IncompatibleCpu,
    /// Files are present and checksum-valid but the ONNX runtime/embedder
    /// failed to initialize (corrupt graph, ABI mismatch, ...).
    OnnxLoadFailed,
}

impl RuntimeLoadability {
    /// Classify cheap host loadability from explicit inputs (pure; testable).
    pub(crate) fn probe_cheap(semantic_feature_built: bool, cpu_has_avx2: bool) -> Self {
        if !semantic_feature_built {
            Self::BaselineBuildNoSemantic
        } else if !cpu_has_avx2 {
            Self::IncompatibleCpu
        } else {
            Self::NotProbed
        }
    }

    /// Cheap host loadability for the running binary. (cass #308 / tg5o9)
    /// Every build carries the pure-Rust frankentorch embedder / reranker —
    /// the former `semantic` Cargo feature is retired — and there is no ONNX
    /// and no AVX hard-block: the int8 GEMM selects NEON / AVX2-when-present
    /// at runtime and falls back to SSE2/scalar otherwise, so both probe
    /// inputs are constant `true`.
    pub(crate) fn probe_cheap_host() -> Self {
        Self::probe_cheap(true, true)
    }

    /// Whether this host can never run the model regardless of the on-disk
    /// files (a build/CPU hard block, known without loading anything).
    fn is_hard_host_block(self) -> bool {
        matches!(self, Self::BaselineBuildNoSemantic | Self::IncompatibleCpu)
    }
}

// ----------------------------------------------------------------------------
// Install provenance.
// ----------------------------------------------------------------------------

/// Where an installed model came from, recorded in the `.verified` marker for
/// audit and rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelSource {
    /// Direct download from the model registry (HuggingFace).
    Registry,
    /// Download from a configured mirror base URL.
    Mirror,
    /// Air-gapped install from a local directory (`--from-file`).
    FromFile,
    /// Files preseeded locally and verified without a cass-written marker.
    Preseeded,
    /// Files are present but provenance was not recorded.
    Unknown,
}

impl ModelSource {
    /// The token written into / read from the `.verified` marker's `source=`
    /// line. Compatible with the legacy values written by the downloader
    /// (`registry`, `mirror:<url>`, `preseeded_local`).
    fn marker_token(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Mirror => "mirror",
            Self::FromFile => "from-file",
            Self::Preseeded => "preseeded_local",
            Self::Unknown => "unknown",
        }
    }

    /// Classify a marker `source=` value into provenance. Recognizes the
    /// legacy `mirror:<url>` and `preseeded_local` tokens plus the hardened
    /// `from-file` token.
    fn from_marker_value(value: &str) -> Self {
        if value == "registry" {
            Self::Registry
        } else if value.starts_with("mirror:") || value == "mirror" {
            Self::Mirror
        } else if value == "from-file" {
            Self::FromFile
        } else if value == "preseeded_local" {
            Self::Preseeded
        } else {
            Self::Unknown
        }
    }
}

// ----------------------------------------------------------------------------
// Model fingerprint.
// ----------------------------------------------------------------------------

/// A stable, deterministic identity for exactly which model bits are
/// installed: the pinned revision plus a content digest over the manifest's
/// per-file checksums. Two installs of the same model version produce the same
/// fingerprint regardless of where the files came from or where they live.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct ModelFingerprint {
    pub revision: String,
    /// Hex SHA-256 over `revision` + sorted `local_name=sha256` lines.
    pub content_digest: String,
}

impl ModelFingerprint {
    /// Derive the fingerprint from a manifest's pinned revision + expected
    /// per-file checksums. Deterministic and independent of file ordering.
    pub(crate) fn from_manifest(manifest: &ModelManifest) -> Self {
        let mut lines: Vec<String> = manifest
            .files
            .iter()
            .map(|f| format!("{}={}", f.local_name(), f.sha256))
            .collect();
        lines.sort();
        let mut hasher = Sha256::new();
        hasher.update(manifest.revision.as_bytes());
        hasher.update(b"\n");
        for line in &lines {
            hasher.update(line.as_bytes());
            hasher.update(b"\n");
        }
        Self {
            revision: manifest.revision.clone(),
            content_digest: hex::encode(hasher.finalize()),
        }
    }

    /// Compact single-token form for the `.verified` marker and logs:
    /// `<revision>:<first-16-of-digest>`.
    pub(crate) fn marker_token(&self) -> String {
        // `get(..16)` is panic-free even if the digest is unexpectedly short;
        // a full SHA-256 hex digest is 64 ASCII chars so byte 16 is always a
        // valid char boundary.
        let short = self
            .content_digest
            .get(..16)
            .unwrap_or(self.content_digest.as_str());
        format!("{}:{short}", self.revision)
    }
}

// ----------------------------------------------------------------------------
// Hardened `.verified` marker contract.
// ----------------------------------------------------------------------------

/// The hardened `.verified` marker. Byte-compatible with the legacy marker
/// (`revision=`/`verified_at=`/`source=` first, same `key=value` line format,
/// same `marker_field` reader semantics) and adds the `source_path=` and
/// `fingerprint=` lines that air-gapped/from-file installs were missing.
/// Unknown lines are ignored by the legacy reader, so this is forward- and
/// backward-compatible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedMarker {
    pub revision: String,
    pub verified_at: Option<String>,
    pub source: ModelSource,
    /// For `from-file`/mirror installs: the source the files came from
    /// (a local directory path or a mirror URL).
    pub source_path: Option<String>,
    pub fingerprint: Option<String>,
}

impl VerifiedMarker {
    /// Build the marker for a freshly verified `--from-file` install.
    pub(crate) fn for_from_file(
        manifest: &ModelManifest,
        source_path: &Path,
        verified_at: String,
    ) -> Self {
        Self {
            revision: manifest.revision.clone(),
            verified_at: Some(verified_at),
            source: ModelSource::FromFile,
            source_path: Some(source_path.display().to_string()),
            fingerprint: Some(ModelFingerprint::from_manifest(manifest).marker_token()),
        }
    }

    /// Render the marker file contents. `revision` is always the first line so
    /// the legacy `check_version_mismatch` (`starts_with("revision=")`) reader
    /// keeps working.
    pub(crate) fn render(&self) -> String {
        let mut out = format!("revision={}\n", self.revision);
        if let Some(ts) = &self.verified_at {
            out.push_str(&format!("verified_at={ts}\n"));
        }
        // Mirror provenance keeps its `mirror:<url>` token (url in source_path).
        let source_value = match (self.source, self.source_path.as_deref()) {
            (ModelSource::Mirror, Some(url)) => format!("mirror:{url}"),
            (source, _) => source.marker_token().to_string(),
        };
        out.push_str(&format!("source={source_value}\n"));
        // For mirror, the url is already folded into source=; avoid duplicating it.
        if let Some(path) = &self.source_path
            && self.source != ModelSource::Mirror
        {
            out.push_str(&format!("source_path={path}\n"));
        }
        if let Some(fp) = &self.fingerprint {
            out.push_str(&format!("fingerprint={fp}\n"));
        }
        out
    }

    /// Parse a marker file's contents. Uses the same `strip_prefix`/`trim`
    /// semantics as the legacy `marker_field` reader. Returns `None` when no
    /// `revision=` line is present (not a valid marker).
    pub(crate) fn parse(content: &str) -> Option<Self> {
        let revision = field(content, "revision")?;
        let raw_source = field(content, "source");
        let source = raw_source
            .as_deref()
            .map(ModelSource::from_marker_value)
            .unwrap_or(ModelSource::Unknown);
        // For a `mirror:<url>` token, recover the url as the source path.
        let mirror_url = raw_source
            .as_deref()
            .and_then(|v| v.strip_prefix("mirror:").map(str::to_string));
        let source_path = field(content, "source_path").or(mirror_url);
        Some(Self {
            revision,
            verified_at: field(content, "verified_at"),
            source,
            source_path,
            fingerprint: field(content, "fingerprint"),
        })
    }
}

/// Read one `key=value` field from marker contents — identical semantics to
/// the private `marker_field` in `model_download` (strip prefix, trim,
/// non-empty).
fn field(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    content
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

// ----------------------------------------------------------------------------
// Download cost class.
// ----------------------------------------------------------------------------

/// A coarse, human-facing class for the network/disk cost of acquiring a
/// model, derived from its total byte size. Lets a robot/human decide whether
/// an explicit install is worth it without parsing raw byte counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DownloadCostClass {
    /// Nothing to download.
    None,
    /// Up to ~100 MiB (e.g. all-minilm-l6-v2, ~90 MB).
    Small,
    /// ~100–250 MiB (e.g. snowflake-arctic-s, ~120 MB).
    Medium,
    /// ≥ ~250 MiB (e.g. nomic-embed, ~270 MB).
    Large,
}

impl DownloadCostClass {
    const SMALL_MAX: u64 = 100 * 1024 * 1024;
    const MEDIUM_MAX: u64 = 250 * 1024 * 1024;

    pub(crate) fn from_bytes(bytes: u64) -> Self {
        if bytes == 0 {
            Self::None
        } else if bytes <= Self::SMALL_MAX {
            Self::Small
        } else if bytes <= Self::MEDIUM_MAX {
            Self::Medium
        } else {
            Self::Large
        }
    }
}

// ----------------------------------------------------------------------------
// Skipped-network reason — the typed "cass never auto-downloads" proof.
// ----------------------------------------------------------------------------

/// Why a non-install surface (search/status/health) did **not** acquire the
/// model over the network. This is the typed contract behind the project rule
/// that "no command auto-downloads a model except explicit `cass models
/// install`". A surface that hits a missing/blocked model reports one of these
/// instead of silently downloading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkippedNetworkReason {
    /// Files are missing/incomplete; acquisition requires an explicit install.
    ExplicitInstallRequired,
    /// An offline policy forbids network acquisition.
    OfflinePolicy,
    /// Semantic acquisition is disabled by policy.
    DisabledByPolicy,
    /// The model exceeds the configured byte budget.
    BudgetExceeded,
    /// The model is already acquired; no network is needed.
    AlreadyAcquired,
}

// ----------------------------------------------------------------------------
// Unified acquisition state + next step.
// ----------------------------------------------------------------------------

/// The single precise model-acquisition state, folding on-disk classification,
/// policy, and runtime loadability. Ordered intentional-off → host hard-blocks
/// → on-disk problems → runtime-load problems → ready; the classifier reports
/// the first applicable state.
///
/// Legacy mapping: `disabled_by_policy`, `offline_blocked`, `budget_blocked`,
/// `checksum_mismatch`, and `quarantined_corrupt` use the same codes as
/// [`crate::search::model_download::ModelCacheState`]; `absent` ⇔ legacy
/// `not_acquired`, `partial_download` ⇔ legacy `acquiring`, `ready` ⇔ legacy
/// `acquired`/`preseeded_local`/`mirror_sourced`. `baseline_no_semantic`,
/// `incompatible_runtime`, and `runtime_load_failed` are new: the runtime
/// dimension legacy on-disk classification cannot represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelAcquisitionState {
    /// Semantic acquisition is disabled by policy; lexical-only by design.
    DisabledByPolicy,
    /// The `-baseline` artifact: `semantic` compiled out, no model can load.
    BaselineNoSemantic,
    /// A semantic build on a host whose CPU/runtime cannot load the model.
    IncompatibleRuntime,
    /// A previously installed cache was found corrupt and quarantined.
    QuarantinedCorrupt,
    /// A staged/partial acquisition is present but incomplete.
    PartialDownload,
    /// No model acquired (opt-in not taken); lexical search still works.
    Absent,
    /// Acquisition needed but the host is offline.
    OfflineBlocked,
    /// The model exceeds the configured byte budget.
    BudgetBlocked,
    /// Files are present but at least one failed checksum verification.
    ChecksumMismatch,
    /// Files are present and checksum-valid, but the ONNX runtime failed to
    /// load them (knowable only after a load attempt).
    RuntimeLoadFailed,
    /// Present, checksum-valid, and loadable (or load not yet attempted on a
    /// host that passes the cheap compatibility checks).
    Ready,
}

impl ModelAcquisitionState {
    /// Stable machine-readable code (same string as the snake_case
    /// serialization). `absent` is the hardened name for legacy
    /// `not_acquired`.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::DisabledByPolicy => "disabled_by_policy",
            Self::BaselineNoSemantic => "baseline_no_semantic",
            Self::IncompatibleRuntime => "incompatible_runtime",
            Self::QuarantinedCorrupt => "quarantined_corrupt",
            Self::PartialDownload => "partial_download",
            Self::Absent => "absent",
            Self::OfflineBlocked => "offline_blocked",
            Self::BudgetBlocked => "budget_blocked",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::RuntimeLoadFailed => "runtime_load_failed",
            Self::Ready => "ready",
        }
    }

    /// Whether the model can be used by semantic search right now.
    pub(crate) fn is_usable(self) -> bool {
        matches!(self, Self::Ready)
    }

    fn state_detail(self) -> &'static str {
        match self {
            Self::DisabledByPolicy => {
                "semantic model acquisition disabled by policy; lexical search only"
            }
            Self::BaselineNoSemantic => {
                "this is the pre-AVX2 baseline build (semantic compiled out); install the full \
                 artifact on an AVX2 host to enable semantic search"
            }
            Self::IncompatibleRuntime => {
                "this host's CPU lacks AVX2; the semantic ONNX runtime cannot load here — use \
                 lexical search or run on an AVX2 host"
            }
            Self::QuarantinedCorrupt => {
                "the model cache was found corrupt and quarantined; repair or reinstall it"
            }
            Self::PartialDownload => {
                "a partial model acquisition is present; resume it with an explicit install"
            }
            Self::Absent => {
                "no embedding model acquired; lexical search works, semantic is opt-in via \
                 explicit install"
            }
            Self::OfflineBlocked => {
                "model is not acquired and the host is offline; install from local files"
            }
            Self::BudgetBlocked => {
                "the model exceeds the configured byte budget; raise the budget or keep lexical"
            }
            Self::ChecksumMismatch => {
                "model files failed checksum verification; repair or reinstall them"
            }
            Self::RuntimeLoadFailed => {
                "model files are valid but the ONNX runtime failed to load them; reinstall or run \
                 on a compatible host"
            }
            Self::Ready => "embedding model acquired, checksum-valid, and loadable",
        }
    }

    fn next_step(self) -> ModelAcquisitionNextStep {
        use ModelAcquisitionNextStep as N;
        match self {
            Self::Ready => N::None,
            Self::DisabledByPolicy => N::EnableSemanticPolicy,
            Self::BaselineNoSemantic => N::UseFullBuildOnCompatibleHost,
            Self::IncompatibleRuntime | Self::RuntimeLoadFailed => N::ReinstallOnCompatibleHost,
            Self::QuarantinedCorrupt | Self::ChecksumMismatch => N::RepairOrReinstall,
            Self::PartialDownload => N::ResumeInstall,
            Self::Absent => N::InstallModel,
            Self::OfflineBlocked => N::InstallFromFileOffline,
            Self::BudgetBlocked => N::RaiseBudgetOrUseLexical,
        }
    }

    fn skipped_network_reason(self) -> Option<SkippedNetworkReason> {
        use SkippedNetworkReason as R;
        match self {
            Self::DisabledByPolicy => Some(R::DisabledByPolicy),
            Self::Absent
            | Self::PartialDownload
            | Self::ChecksumMismatch
            | Self::QuarantinedCorrupt => Some(R::ExplicitInstallRequired),
            Self::OfflineBlocked => Some(R::OfflinePolicy),
            Self::BudgetBlocked => Some(R::BudgetExceeded),
            Self::Ready => Some(R::AlreadyAcquired),
            // Host hard-blocks: a download would not help, so there is no
            // skipped-network story to tell.
            Self::BaselineNoSemantic | Self::IncompatibleRuntime | Self::RuntimeLoadFailed => None,
        }
    }

    fn rollback_guidance(self, model_name: &str) -> Option<String> {
        match self {
            Self::ChecksumMismatch | Self::QuarantinedCorrupt | Self::RuntimeLoadFailed => {
                Some(format!(
                    "Safe to clear: `cass models remove --model {model_name}` removes only the \
                     derived model cache (lexical search keeps working); then reinstall with \
                     `cass models install --model {model_name}`."
                ))
            }
            Self::PartialDownload => Some(format!(
                "The partial cache is resumable; re-run `cass models install --model {model_name}`, \
                 or clear it with `cass models remove --model {model_name}` and start fresh."
            )),
            _ => None,
        }
    }
}

/// The safe (never-destructive) next step to improve acquisition readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelAcquisitionNextStep {
    /// Nothing to do; the model is ready.
    None,
    /// Acquire the model (opt-in, explicit install).
    InstallModel,
    /// Resume an interrupted/partial install.
    ResumeInstall,
    /// Re-enable semantic acquisition in policy/config.
    EnableSemanticPolicy,
    /// Install the full (non-baseline) artifact on an AVX2 host.
    UseFullBuildOnCompatibleHost,
    /// Reinstall or run on a CPU/runtime-compatible host.
    ReinstallOnCompatibleHost,
    /// Install from a local directory while offline (`--from-file`).
    InstallFromFileOffline,
    /// Repair the verified cache or reinstall.
    RepairOrReinstall,
    /// Raise the model byte budget or keep using lexical search.
    RaiseBudgetOrUseLexical,
}

impl ModelAcquisitionNextStep {
    /// A robot-safe, never-destructive command template for this step, with
    /// the concrete `model_name` filled in. `None` for steps that are a config
    /// change or a host change rather than a single command.
    fn next_command(self, model_name: &str) -> Option<String> {
        match self {
            Self::None
            | Self::EnableSemanticPolicy
            | Self::UseFullBuildOnCompatibleHost
            | Self::ReinstallOnCompatibleHost
            | Self::RaiseBudgetOrUseLexical => None,
            Self::InstallModel | Self::ResumeInstall => {
                Some(format!("cass models install --model {model_name} --json"))
            }
            Self::InstallFromFileOffline => Some(format!(
                "cass models install --model {model_name} --from-file <dir> --json"
            )),
            Self::RepairOrReinstall => Some("cass models verify --repair --json".to_string()),
        }
    }
}

// ----------------------------------------------------------------------------
// Signals + classifier.
// ----------------------------------------------------------------------------

/// The explicit signals a surface supplies to classify model acquisition.
/// `Copy` so fixtures and call sites stay cheap. [`probe_on_disk`] fills the
/// filesystem-derived fields; the caller supplies policy + runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelAcquisitionSignals {
    /// Semantic acquisition is enabled by policy (mode != lexical-only).
    pub policy_enabled: bool,
    /// Host loadability (cheap or probed).
    pub runtime: RuntimeLoadability,
    /// Provenance of the on-disk model (from the verified marker).
    pub source: ModelSource,
    /// All required manifest files are present on disk.
    pub files_present: bool,
    /// Some — but not all — files (or a staging dir) are present.
    pub partial_present: bool,
    /// Every present file matches its expected SHA-256 (only meaningful when
    /// `files_present`).
    pub checksum_ok: bool,
    /// A corrupt-cache quarantine marker is present.
    pub quarantined: bool,
    /// An offline policy is in effect.
    pub offline: bool,
    /// The model exceeds the configured byte budget.
    pub budget_exceeded: bool,
    /// The manifest's total download size in bytes (for cost class).
    pub expected_total_bytes: u64,
    /// An air-gapped install path exists for this model.
    pub offline_install_available: bool,
}

impl ModelAcquisitionSignals {
    /// Classify the single precise acquisition state in priority order.
    pub(crate) fn state(&self) -> ModelAcquisitionState {
        use ModelAcquisitionState as S;
        if !self.policy_enabled {
            return S::DisabledByPolicy;
        }
        // Host hard-blocks dominate: on these hosts no on-disk state can make
        // the model usable, so the actionable truth is the host, not the files.
        match self.runtime {
            RuntimeLoadability::BaselineBuildNoSemantic => return S::BaselineNoSemantic,
            RuntimeLoadability::IncompatibleCpu => return S::IncompatibleRuntime,
            _ => {}
        }
        if self.quarantined {
            return S::QuarantinedCorrupt;
        }
        if !self.files_present {
            if self.partial_present {
                return S::PartialDownload;
            }
            if self.offline {
                return S::OfflineBlocked;
            }
            if self.budget_exceeded {
                return S::BudgetBlocked;
            }
            return S::Absent;
        }
        if !self.checksum_ok {
            return S::ChecksumMismatch;
        }
        if self.runtime == RuntimeLoadability::OnnxLoadFailed {
            return S::RuntimeLoadFailed;
        }
        S::Ready
    }

    /// Derive the full hardened acquisition report.
    pub(crate) fn report(&self, model_name: &str) -> ModelAcquisitionReport {
        let state = self.state();
        let next_step = state.next_step();
        ModelAcquisitionReport {
            state,
            usable: state.is_usable(),
            runtime: self.runtime,
            source: self.source,
            cost_class: DownloadCostClass::from_bytes(self.expected_total_bytes),
            expected_download_bytes: self.expected_total_bytes,
            offline_install_available: self.offline_install_available,
            skipped_network_reason: state.skipped_network_reason(),
            // Semantic always fails open to lexical until a model is fully
            // ready; this mirrors the search asset contract.
            fallback_mode: if state.is_usable() {
                FallbackMode::None
            } else {
                FallbackMode::Lexical
            },
            next_step,
            next_command: next_step.next_command(model_name),
            state_detail: state.state_detail().to_string(),
            rollback_guidance: state.rollback_guidance(model_name),
        }
    }

    /// Bridge the on-disk acquisition signals into the inputs the semantic
    /// readiness classifier ([`crate::search::semantic_readiness::SemanticSignals`])
    /// expects, so the two contracts compose instead of drifting.
    pub(crate) fn semantic_model_inputs(&self) -> SemanticModelInputs {
        SemanticModelInputs {
            // A model is "present" once any of its files have landed; a
            // partial install counts as present-but-incomplete.
            model_present: self.files_present || self.partial_present,
            model_files_complete: self.files_present,
            checksum_ok: self.files_present && self.checksum_ok,
        }
    }
}

/// The three model-layer booleans the semantic readiness classifier consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SemanticModelInputs {
    pub model_present: bool,
    pub model_files_complete: bool,
    pub checksum_ok: bool,
}

/// The derived hardened model-acquisition report every status/verify/install
/// surface projects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelAcquisitionReport {
    pub state: ModelAcquisitionState,
    pub usable: bool,
    pub runtime: RuntimeLoadability,
    pub source: ModelSource,
    pub cost_class: DownloadCostClass,
    pub expected_download_bytes: u64,
    pub offline_install_available: bool,
    pub skipped_network_reason: Option<SkippedNetworkReason>,
    pub fallback_mode: FallbackMode,
    pub next_step: ModelAcquisitionNextStep,
    pub next_command: Option<String>,
    pub state_detail: String,
    pub rollback_guidance: Option<String>,
}

// ----------------------------------------------------------------------------
// On-disk probe (filesystem reads only — never any network I/O).
// ----------------------------------------------------------------------------

/// Policy inputs the on-disk probe needs that are not derivable from the files.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OnDiskPolicyInputs {
    pub policy_enabled: bool,
    pub offline: bool,
    /// Maximum allowed model size; `None` means no budget cap.
    pub budget_max_bytes: Option<u64>,
}

/// Build the on-disk portion of [`ModelAcquisitionSignals`] from real files.
///
/// Performs **filesystem reads only** — it never opens a socket, so it cannot
/// auto-download. It reuses the same `compute_sha256` + `model_file_path`
/// helpers the live verifier uses, so its checksum verdict matches `cass
/// models verify`. The caller supplies the (cheap or probed) `runtime`.
pub(crate) fn probe_on_disk(
    model_dir: &Path,
    manifest: &ModelManifest,
    runtime: RuntimeLoadability,
    policy: OnDiskPolicyInputs,
) -> ModelAcquisitionSignals {
    let total = manifest.total_size();

    let mut present = 0usize;
    let mut checksum_ok = true;
    for file in &manifest.files {
        if let Some(path) = model_file_path(model_dir, file) {
            present += 1;
            match compute_sha256(&path) {
                Ok(actual) => {
                    if actual != file.sha256 {
                        checksum_ok = false;
                    }
                }
                // Unreadable present file: treat as a checksum failure
                // rather than silently passing.
                Err(_) => checksum_ok = false,
            }
        }
    }

    let files_present = present == manifest.files.len() && !manifest.files.is_empty();
    let partial_present = present > 0 && !files_present;

    let source = std::fs::read_to_string(model_dir.join(VERIFIED_MARKER_NAME))
        .ok()
        .and_then(|c| VerifiedMarker::parse(&c))
        .map(|m| m.source)
        // Files present but no parseable marker ⇒ preseeded (matches the live
        // classifier, which treats marker-less complete files as preseeded).
        .unwrap_or(if files_present {
            ModelSource::Preseeded
        } else {
            ModelSource::Unknown
        });

    let quarantined = model_dir.join(QUARANTINE_MARKER_NAME).is_file();
    let budget_exceeded = policy.budget_max_bytes.is_some_and(|max| total > max);

    ModelAcquisitionSignals {
        policy_enabled: policy.policy_enabled,
        runtime,
        source,
        files_present,
        partial_present,
        // checksum_ok is only consulted when files_present; report the real
        // verdict either way.
        checksum_ok: files_present && checksum_ok,
        quarantined,
        offline: policy.offline,
        budget_exceeded,
        expected_total_bytes: total,
        offline_install_available: !manifest.files.is_empty(),
    }
}

// ----------------------------------------------------------------------------
// Live surface projection (status/verify).
// ----------------------------------------------------------------------------

/// Project the hardened model-acquisition contract for a live `cass models
/// status` / `cass models verify` surface as a JSON object: the full
/// [`ModelAcquisitionReport`] (state, runtime, source, cost_class,
/// `skipped_network_reason`, `next_command`, `rollback_guidance`, …) plus a
/// stable [`ModelFingerprint`].
///
/// The work is an on-disk probe — **filesystem reads only, never a socket** —
/// so a surface that emits this block *proves* cass did not auto-acquire the
/// model: the typed `skipped_network_reason` is exactly the "no command
/// downloads except explicit `cass models install`" contract made machine
/// checkable. Both `models status` and `models verify` emit this single
/// projection so the two surfaces cannot drift. Bead
/// `coding_agent_session_search-0qxwg` (follow-on to
/// `cass-fleet-resilience-20260608-uojcg.5.5`).
pub(crate) fn live_acquisition_block(
    model_dir: &Path,
    manifest: &ModelManifest,
    runtime: RuntimeLoadability,
    policy: OnDiskPolicyInputs,
    model_name: &str,
) -> serde_json::Value {
    let report = probe_on_disk(model_dir, manifest, runtime, policy).report(model_name);
    let fingerprint = ModelFingerprint::from_manifest(manifest);
    let mut block = serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = block.as_object_mut() {
        obj.insert(
            "fingerprint".to_string(),
            serde_json::json!({
                "revision": fingerprint.revision,
                "content_digest": fingerprint.content_digest,
                "marker_token": fingerprint.marker_token(),
            }),
        );
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::model_download::ModelFile;
    use crate::search::semantic_readiness::{SemanticReadinessReason, SemanticSignals};

    // --- fixtures -----------------------------------------------------------

    /// A small synthetic manifest whose checksums are computed at test time so
    /// real on-disk verification works without a 90 MB model.
    fn synthetic_manifest(files: &[(&str, &str)]) -> ModelManifest {
        ModelManifest {
            id: "test-model".to_string(),
            repo: "test/model".to_string(),
            revision: "rev-abc123".to_string(),
            files: files
                .iter()
                .map(|(name, body)| ModelFile {
                    name: name.to_string(),
                    sha256: sha256_hex(body.as_bytes()),
                    size: body.len() as u64,
                })
                .collect(),
            license: "Apache-2.0".to_string(),
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    /// A fully-ready signal set; individual tests flip one field.
    fn ready() -> ModelAcquisitionSignals {
        ModelAcquisitionSignals {
            policy_enabled: true,
            runtime: RuntimeLoadability::Loadable,
            source: ModelSource::Registry,
            files_present: true,
            partial_present: false,
            checksum_ok: true,
            quarantined: false,
            offline: false,
            budget_exceeded: false,
            expected_total_bytes: 90 * 1024 * 1024,
            offline_install_available: true,
        }
    }

    fn policy(budget: Option<u64>) -> OnDiskPolicyInputs {
        OnDiskPolicyInputs {
            policy_enabled: true,
            offline: false,
            budget_max_bytes: budget,
        }
    }

    // --- pure classification ------------------------------------------------

    #[test]
    fn ready_state_is_usable_with_no_fallback() {
        let r = ready().report("minilm");
        assert_eq!(r.state, ModelAcquisitionState::Ready);
        assert!(r.usable);
        assert_eq!(r.fallback_mode, FallbackMode::None);
        assert_eq!(r.next_step, ModelAcquisitionNextStep::None);
        assert_eq!(
            r.skipped_network_reason,
            Some(SkippedNetworkReason::AlreadyAcquired)
        );
        assert!(r.rollback_guidance.is_none());
        assert!(r.next_command.is_none());
    }

    #[test]
    fn not_probed_runtime_is_still_ready_so_status_never_loads_onnx() {
        let mut s = ready();
        s.runtime = RuntimeLoadability::NotProbed;
        assert_eq!(s.state(), ModelAcquisitionState::Ready);
    }

    #[test]
    fn disabled_policy_dominates_everything() {
        let mut s = ready();
        s.policy_enabled = false;
        let r = s.report("minilm");
        assert_eq!(r.state, ModelAcquisitionState::DisabledByPolicy);
        assert_eq!(r.fallback_mode, FallbackMode::Lexical);
        assert_eq!(r.next_step, ModelAcquisitionNextStep::EnableSemanticPolicy);
        assert_eq!(
            r.skipped_network_reason,
            Some(SkippedNetworkReason::DisabledByPolicy)
        );
    }

    #[test]
    fn baseline_build_blocks_even_with_valid_files() {
        let mut s = ready();
        s.runtime = RuntimeLoadability::BaselineBuildNoSemantic;
        let r = s.report("minilm");
        assert_eq!(r.state, ModelAcquisitionState::BaselineNoSemantic);
        // A host hard-block has no "skipped download" story.
        assert!(r.skipped_network_reason.is_none());
        assert_eq!(
            r.next_step,
            ModelAcquisitionNextStep::UseFullBuildOnCompatibleHost
        );
    }

    #[test]
    fn incompatible_cpu_blocks_even_with_valid_files() {
        let mut s = ready();
        s.runtime = RuntimeLoadability::IncompatibleCpu;
        let r = s.report("minilm");
        assert_eq!(r.state, ModelAcquisitionState::IncompatibleRuntime);
        assert!(r.skipped_network_reason.is_none());
        assert_eq!(
            r.next_step,
            ModelAcquisitionNextStep::ReinstallOnCompatibleHost
        );
    }

    #[test]
    fn onnx_load_failure_only_when_files_valid() {
        let mut s = ready();
        s.runtime = RuntimeLoadability::OnnxLoadFailed;
        assert_eq!(s.state(), ModelAcquisitionState::RuntimeLoadFailed);
        let r = s.report("minilm");
        assert!(r.rollback_guidance.is_some());

        // If files are *not* valid, the checksum problem is reported first —
        // we never claim a runtime load failure on unverified bits.
        let mut s = ready();
        s.runtime = RuntimeLoadability::OnnxLoadFailed;
        s.checksum_ok = false;
        assert_eq!(s.state(), ModelAcquisitionState::ChecksumMismatch);
    }

    #[test]
    fn absent_reports_explicit_install_required_no_auto_download() {
        let mut s = ready();
        s.files_present = false;
        s.checksum_ok = false;
        let r = s.report("minilm");
        assert_eq!(r.state, ModelAcquisitionState::Absent);
        assert_eq!(
            r.skipped_network_reason,
            Some(SkippedNetworkReason::ExplicitInstallRequired)
        );
        assert_eq!(r.next_step, ModelAcquisitionNextStep::InstallModel);
        assert_eq!(
            r.next_command.as_deref(),
            Some("cass models install --model minilm --json")
        );
        assert_eq!(r.fallback_mode, FallbackMode::Lexical);
    }

    #[test]
    fn partial_offline_budget_are_distinct() {
        let mut s = ready();
        s.files_present = false;
        s.partial_present = true;
        assert_eq!(s.state(), ModelAcquisitionState::PartialDownload);

        let mut s = ready();
        s.files_present = false;
        s.offline = true;
        assert_eq!(s.state(), ModelAcquisitionState::OfflineBlocked);
        assert_eq!(
            s.report("minilm").next_step,
            ModelAcquisitionNextStep::InstallFromFileOffline
        );

        let mut s = ready();
        s.files_present = false;
        s.budget_exceeded = true;
        assert_eq!(s.state(), ModelAcquisitionState::BudgetBlocked);
    }

    #[test]
    fn checksum_mismatch_offers_safe_rollback() {
        let mut s = ready();
        s.checksum_ok = false;
        let r = s.report("snowflake-arctic-s");
        assert_eq!(r.state, ModelAcquisitionState::ChecksumMismatch);
        let guidance = r.rollback_guidance.expect("rollback guidance");
        assert!(guidance.contains("cass models remove --model snowflake-arctic-s"));
        assert!(guidance.contains("lexical search keeps working"));
        assert_eq!(
            r.next_command.as_deref(),
            Some("cass models verify --repair --json")
        );
    }

    #[test]
    fn every_state_is_reachable_from_some_signal_set() {
        use ModelAcquisitionState as S;
        let reached: std::collections::BTreeSet<S> = [
            {
                let mut s = ready();
                s.policy_enabled = false;
                s.state()
            },
            {
                let mut s = ready();
                s.runtime = RuntimeLoadability::BaselineBuildNoSemantic;
                s.state()
            },
            {
                let mut s = ready();
                s.runtime = RuntimeLoadability::IncompatibleCpu;
                s.state()
            },
            {
                let mut s = ready();
                s.quarantined = true;
                s.state()
            },
            {
                let mut s = ready();
                s.files_present = false;
                s.partial_present = true;
                s.state()
            },
            {
                let mut s = ready();
                s.files_present = false;
                s.offline = true;
                s.state()
            },
            {
                let mut s = ready();
                s.files_present = false;
                s.budget_exceeded = true;
                s.state()
            },
            {
                let mut s = ready();
                s.files_present = false;
                s.state()
            },
            {
                let mut s = ready();
                s.checksum_ok = false;
                s.state()
            },
            {
                let mut s = ready();
                s.runtime = RuntimeLoadability::OnnxLoadFailed;
                s.state()
            },
            ready().state(),
        ]
        .into_iter()
        .collect();
        assert_eq!(reached.len(), 11, "all eleven states must be reachable");
    }

    // --- serialization ------------------------------------------------------

    #[test]
    fn report_round_trips_through_json_with_snake_case_codes() {
        let r = ready().report("minilm");
        let json = serde_json::to_string(&r).unwrap();
        for needle in [
            "\"state\":\"ready\"",
            "\"usable\":true",
            "\"runtime\":\"loadable\"",
            "\"source\":\"registry\"",
            "\"cost_class\":\"small\"",
            "\"fallback_mode\":\"none\"",
            "\"skipped_network_reason\":\"already_acquired\"",
        ] {
            assert!(json.contains(needle), "missing {needle} in {json}");
        }
        let parsed: ModelAcquisitionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn state_code_matches_snake_case_serialization() {
        for state in [
            ModelAcquisitionState::Absent,
            ModelAcquisitionState::ChecksumMismatch,
            ModelAcquisitionState::Ready,
            ModelAcquisitionState::BaselineNoSemantic,
            ModelAcquisitionState::RuntimeLoadFailed,
        ] {
            let serialized = serde_json::to_string(&state).unwrap();
            assert_eq!(serialized, format!("\"{}\"", state.code()));
        }
    }

    // --- cost class ---------------------------------------------------------

    #[test]
    fn cost_class_boundaries() {
        assert_eq!(DownloadCostClass::from_bytes(0), DownloadCostClass::None);
        assert_eq!(
            DownloadCostClass::from_bytes(90 * 1024 * 1024),
            DownloadCostClass::Small
        );
        assert_eq!(
            DownloadCostClass::from_bytes(120 * 1024 * 1024),
            DownloadCostClass::Medium
        );
        assert_eq!(
            DownloadCostClass::from_bytes(270 * 1024 * 1024),
            DownloadCostClass::Large
        );
        // Exact boundary at the small cap is still small.
        assert_eq!(
            DownloadCostClass::from_bytes(100 * 1024 * 1024),
            DownloadCostClass::Small
        );
    }

    // --- runtime cheap probe ------------------------------------------------

    #[test]
    fn cheap_probe_classifies_build_and_cpu() {
        assert_eq!(
            RuntimeLoadability::probe_cheap(false, true),
            RuntimeLoadability::BaselineBuildNoSemantic
        );
        assert_eq!(
            RuntimeLoadability::probe_cheap(true, false),
            RuntimeLoadability::IncompatibleCpu
        );
        assert_eq!(
            RuntimeLoadability::probe_cheap(true, true),
            RuntimeLoadability::NotProbed
        );
        // The baseline build dominates: a missing-AVX2 baseline reports the
        // build problem (installing the full artifact is the real fix).
        assert_eq!(
            RuntimeLoadability::probe_cheap(false, false),
            RuntimeLoadability::BaselineBuildNoSemantic
        );
        // The host probe must never panic on any architecture.
        let _ = RuntimeLoadability::probe_cheap_host();
    }

    // --- fingerprint --------------------------------------------------------

    #[test]
    fn fingerprint_is_deterministic_and_order_independent() {
        let a = synthetic_manifest(&[("model.onnx", "weights"), ("tokenizer.json", "vocab")]);
        let b = synthetic_manifest(&[("tokenizer.json", "vocab"), ("model.onnx", "weights")]);
        let fa = ModelFingerprint::from_manifest(&a);
        let fb = ModelFingerprint::from_manifest(&b);
        assert_eq!(fa, fb, "fingerprint must not depend on file order");
        assert_eq!(fa.revision, "rev-abc123");
        assert!(fa.marker_token().starts_with("rev-abc123:"));

        // Different content ⇒ different fingerprint.
        let c = synthetic_manifest(&[("model.onnx", "OTHER"), ("tokenizer.json", "vocab")]);
        assert_ne!(
            fa.content_digest,
            ModelFingerprint::from_manifest(&c).content_digest
        );
    }

    // --- verified marker round trip ----------------------------------------

    #[test]
    fn from_file_marker_records_source_and_fingerprint_and_round_trips() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights")]);
        let src = Path::new("/home/dev/models/minilm");
        let marker = VerifiedMarker::for_from_file(&manifest, src, "2026-06-15T00:00:00Z".into());
        let rendered = marker.render();

        // First line is revision= so the legacy version reader keeps working.
        assert!(rendered.starts_with("revision=rev-abc123\n"));
        assert!(rendered.contains("source=from-file\n"));
        assert!(rendered.contains("source_path=/home/dev/models/minilm\n"));
        assert!(rendered.contains("fingerprint=rev-abc123:"));

        let parsed = VerifiedMarker::parse(&rendered).expect("parse");
        assert_eq!(parsed, marker);
        assert_eq!(parsed.source, ModelSource::FromFile);
    }

    #[test]
    fn parser_is_compatible_with_legacy_markers() {
        // Legacy downloader marker (registry).
        let legacy = "revision=rev-abc123\nverified_at=2026-01-01T00:00:00Z\nsource=registry\n";
        let m = VerifiedMarker::parse(legacy).unwrap();
        assert_eq!(m.source, ModelSource::Registry);
        assert!(m.fingerprint.is_none());

        // Legacy mirror marker folds the url into source=; we recover it.
        let mirror = "revision=r\nsource=mirror:https://m.example/cache\n";
        let m = VerifiedMarker::parse(mirror).unwrap();
        assert_eq!(m.source, ModelSource::Mirror);
        assert_eq!(m.source_path.as_deref(), Some("https://m.example/cache"));

        // Thin from-file marker (revision only) is still parseable.
        let thin = "revision=r\n";
        assert!(VerifiedMarker::parse(thin).is_some());

        // No revision ⇒ not a valid marker.
        assert!(VerifiedMarker::parse("source=registry\n").is_none());
    }

    #[test]
    fn mirror_marker_round_trips_without_duplicate_path_line() {
        let marker = VerifiedMarker {
            revision: "r".into(),
            verified_at: None,
            source: ModelSource::Mirror,
            source_path: Some("https://m.example/cache".into()),
            fingerprint: None,
        };
        let rendered = marker.render();
        assert!(rendered.contains("source=mirror:https://m.example/cache\n"));
        assert!(!rendered.contains("source_path="));
        assert_eq!(VerifiedMarker::parse(&rendered).unwrap(), marker);
    }

    // --- on-disk fixtures (real filesystem, no network) ---------------------

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    fn temp_model_dir(tag: &str) -> std::path::PathBuf {
        // Each test uses a distinct `tag`; the pid keeps parallel runs apart,
        // and we clear any stale dir before use (no Date/rand needed).
        let base =
            std::env::temp_dir().join(format!("cass-model-acq-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn probe_absent_model_dir() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights"), ("tokenizer.json", "v")]);
        let dir = temp_model_dir("absent");
        let signals = probe_on_disk(&dir, &manifest, RuntimeLoadability::NotProbed, policy(None));
        assert!(!signals.files_present);
        assert!(!signals.partial_present);
        assert_eq!(signals.source, ModelSource::Unknown);
        assert_eq!(signals.state(), ModelAcquisitionState::Absent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_partial_model_dir() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights"), ("tokenizer.json", "v")]);
        let dir = temp_model_dir("partial");
        write(&dir, "model.onnx", "weights"); // only one of two files
        let signals = probe_on_disk(&dir, &manifest, RuntimeLoadability::NotProbed, policy(None));
        assert!(!signals.files_present);
        assert!(signals.partial_present);
        assert_eq!(signals.state(), ModelAcquisitionState::PartialDownload);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_checksum_mismatch_model_dir() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights"), ("tokenizer.json", "v")]);
        let dir = temp_model_dir("mismatch");
        write(&dir, "model.onnx", "CORRUPT"); // wrong content
        write(&dir, "tokenizer.json", "v");
        let signals = probe_on_disk(&dir, &manifest, RuntimeLoadability::NotProbed, policy(None));
        assert!(signals.files_present);
        assert!(!signals.checksum_ok);
        assert_eq!(signals.state(), ModelAcquisitionState::ChecksumMismatch);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_from_file_install_is_ready_with_provenance() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights"), ("tokenizer.json", "v")]);
        let dir = temp_model_dir("fromfile");
        write(&dir, "model.onnx", "weights");
        write(&dir, "tokenizer.json", "v");
        let marker = VerifiedMarker::for_from_file(
            &manifest,
            Path::new("/srv/airgap"),
            "2026-06-15T00:00:00Z".into(),
        );
        write(&dir, VERIFIED_MARKER_NAME, &marker.render());

        let signals = probe_on_disk(&dir, &manifest, RuntimeLoadability::Loadable, policy(None));
        assert!(signals.files_present);
        assert!(signals.checksum_ok);
        assert_eq!(signals.source, ModelSource::FromFile);
        assert_eq!(signals.state(), ModelAcquisitionState::Ready);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_quarantined_model_dir() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights")]);
        let dir = temp_model_dir("quarantine");
        write(&dir, "model.onnx", "weights");
        write(&dir, QUARANTINE_MARKER_NAME, "corrupt graph at load");
        let signals = probe_on_disk(&dir, &manifest, RuntimeLoadability::NotProbed, policy(None));
        assert!(signals.quarantined);
        assert_eq!(signals.state(), ModelAcquisitionState::QuarantinedCorrupt);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_budget_blocked_when_over_cap() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights")]);
        let dir = temp_model_dir("budget");
        // empty dir + a 1-byte budget ⇒ absent files, but over budget.
        let signals = probe_on_disk(
            &dir,
            &manifest,
            RuntimeLoadability::NotProbed,
            policy(Some(1)),
        );
        assert!(signals.budget_exceeded);
        assert_eq!(signals.state(), ModelAcquisitionState::BudgetBlocked);
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- live surface projection (status/verify) ----------------------------

    #[test]
    fn live_acquisition_block_carries_report_and_fingerprint() {
        let manifest = synthetic_manifest(&[("model.onnx", "weights"), ("tokenizer.json", "v")]);
        let dir = temp_model_dir("liveblock");
        // Absent model dir on an AVX2/semantic host (NotProbed): the block must
        // prove no auto-download and stay lexical fail-open.
        let block = live_acquisition_block(
            &dir,
            &manifest,
            RuntimeLoadability::NotProbed,
            policy(None),
            "minilm",
        );
        assert_eq!(block["state"].as_str(), Some("absent"));
        assert_eq!(
            block["skipped_network_reason"].as_str(),
            Some("explicit_install_required")
        );
        assert_eq!(block["fallback_mode"].as_str(), Some("lexical"));
        assert_eq!(
            block["next_command"].as_str(),
            Some("cass models install --model minilm --json")
        );
        // The fingerprint sub-object is present and deterministic.
        assert_eq!(
            block["fingerprint"]["revision"].as_str(),
            Some("rev-abc123")
        );
        assert!(
            block["fingerprint"]["marker_token"]
                .as_str()
                .is_some_and(|t| t.starts_with("rev-abc123:")),
            "marker_token should be <revision>:<digest16>; got {:?}",
            block["fingerprint"]["marker_token"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // --- composition with the semantic readiness contract (bead 5.1) --------

    #[test]
    fn bridges_into_semantic_readiness_without_drift() {
        // A from-file ready install must drive the readiness classifier to a
        // model-acquired verdict (not ModelNotAcquired / ChecksumMismatch).
        let acq = ready();
        let inputs = acq.semantic_model_inputs();
        assert!(inputs.model_present && inputs.model_files_complete && inputs.checksum_ok);
        let sem = SemanticSignals {
            policy_enabled: true,
            baseline_only: false,
            model_present: inputs.model_present,
            model_files_complete: inputs.model_files_complete,
            checksum_ok: inputs.checksum_ok,
            vector_index_present: true,
            db_fingerprint_matches: Some(true),
            backfill_in_progress: false,
            fast_tier_ready: true,
            quality_tier_ready: true,
        };
        assert_eq!(sem.reason(), SemanticReadinessReason::QualityTierReady);

        // A checksum mismatch in acquisition surfaces as ChecksumMismatch in
        // readiness too — the two contracts agree.
        let mut acq = ready();
        acq.checksum_ok = false;
        let inputs = acq.semantic_model_inputs();
        let sem = SemanticSignals {
            policy_enabled: true,
            baseline_only: false,
            model_present: inputs.model_present,
            model_files_complete: inputs.model_files_complete,
            checksum_ok: inputs.checksum_ok,
            vector_index_present: false,
            db_fingerprint_matches: None,
            backfill_in_progress: false,
            fast_tier_ready: false,
            quality_tier_ready: false,
        };
        assert_eq!(sem.reason(), SemanticReadinessReason::ChecksumMismatch);

        // An absent model bridges to ModelNotAcquired.
        let mut acq = ready();
        acq.files_present = false;
        acq.partial_present = false;
        let inputs = acq.semantic_model_inputs();
        let sem = SemanticSignals {
            policy_enabled: true,
            baseline_only: false,
            model_present: inputs.model_present,
            model_files_complete: inputs.model_files_complete,
            checksum_ok: inputs.checksum_ok,
            vector_index_present: false,
            db_fingerprint_matches: None,
            backfill_in_progress: false,
            fast_tier_ready: false,
            quality_tier_ready: false,
        };
        assert_eq!(sem.reason(), SemanticReadinessReason::ModelNotAcquired);
    }
}
