use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use toml::Value;

#[derive(Clone, Copy, Eq, PartialEq)]
enum ValidationMode {
    ActivePathOverride,
    StrictOptIn,
}

#[derive(Clone, Copy)]
struct DependencyContract {
    label: &'static str,
    dep_table: &'static str,
    dep_key: &'static str,
    crate_package_name: &'static str,
    manifest_package_field: Option<&'static str>,
    expected_git: &'static str,
    expected_rev: &'static str,
    expected_version: &'static str,
    expected_features: &'static [&'static str],
    expected_default_features: Option<bool>,
    repo_rel: &'static str,
    manifest_rel: &'static str,
    patch_url: Option<&'static str>,
    patch_key: Option<&'static str>,
    mode: ValidationMode,
}

struct GitState {
    head: String,
    dirty: bool,
}

const STRICT_PATH_DEP_FEATURE: &str = "strict-path-dep-validation";
const STRICT_PATH_DEP_ENV: &str = "CASS_STRICT_PATH_DEP_VALIDATION";

const CONTRACTS: &[DependencyContract] = &[
    DependencyContract {
        label: "frankensqlite facade",
        dep_table: "dependencies",
        dep_key: "frankensqlite",
        crate_package_name: "fsqlite",
        manifest_package_field: Some("fsqlite"),
        // Exact upstream source pin (established with the fsqlite 0.2.1
        // migration, bead bo000; now at 0.3.16. 0.3.15 was evaluated on
        // 2026-09-02 (bead gh382-fsqlite-pin) and NOT adopted: cass's own
        // writable open still looped on a large archive with a large WAL
        // (reclaim sweep x per-page WAL rescan, cass GH #382 / bead g3zyo).
        // 0.3.16 carries that fix — frankensqlite 8d012706a, index the
        // appended WAL tail once per stable tail instead of rescanning it
        // per page — plus the GH#405 FTS5 undo log (savepoints no longer
        // clone the whole table), the GH#406 content-backed INSERT as an
        // incremental segment append, lazy contentless FTS5 on the ordinary
        // open path, prefix-BM25 scoring parity, and the 0.3.15 line (FTS5
        // 'optimize', origin-poison self-heal, macOS clippy gate). Adopted
        // 2026-09-04 by owner instruction. The 0.3.13 semantics CASS relies
        // on (asupersync 0.4.3 runtime migration, GH#333/GH#334, the
        // cass#393 namespace-sidecar repair, the GH#438 Windows sidecar-less
        // read-only close, integrity-check through read-only guards, and
        // the cass#434 autoindex-vanish fixes) all carry forward.
        // fsqlite resolves from crates.io at the exact version below.
        expected_git: "",
        expected_rev: "",
        expected_version: "0.3.16",
        // `async-api` exposes frankensqlite::AsyncConnection, which
        // src/search/query.rs uses (as SearchSqliteConnection) for the
        // no-hit alternate-agent suggestions without a full storage open.
        expected_features: &["fts5", "async-api"],
        expected_default_features: None,
        repo_rel: "../frankensqlite",
        manifest_rel: "crates/fsqlite/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "frankensqlite shared types (production)",
        dep_table: "dependencies",
        dep_key: "fsqlite-types",
        crate_package_name: "fsqlite-types",
        manifest_package_field: Some("fsqlite-types"),
        // Keep shared types on the identical registry version as the facade.
        expected_git: "",
        expected_rev: "",
        expected_version: "0.3.16",
        expected_features: &[],
        expected_default_features: None,
        repo_rel: "../frankensqlite",
        manifest_rel: "crates/fsqlite-types/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "frankensqlite shared types (test)",
        dep_table: "dev-dependencies",
        dep_key: "fsqlite-types",
        crate_package_name: "fsqlite-types",
        manifest_package_field: Some("fsqlite-types"),
        // Keep shared types on the identical registry version as the facade.
        expected_git: "",
        expected_rev: "",
        expected_version: "0.3.16",
        expected_features: &[],
        expected_default_features: None,
        repo_rel: "../frankensqlite",
        manifest_rel: "crates/fsqlite-types/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "franken_agent_detection",
        dep_table: "dependencies",
        dep_key: "franken-agent-detection",
        crate_package_name: "franken-agent-detection",
        manifest_package_field: None,
        // GH#416: registry pin. crates.io 0.2.2 is the upstream release tag
        // f19e7e0 (2026-09: cursor/antigravity/grok scan-root scoping plus the
        // aider/copilot-cli/amp/opencode/clawdbot/muse session-loss fixes).
        // The Shelley connector, FAD#22 source-boundary seam, and the
        // chatgpt/omp injection seams live past this tag and wait on the next
        // publish. crates.io refuses git dependencies, hence version-only.
        expected_git: "",
        expected_rev: "",
        expected_version: "0.2.2",
        expected_features: &[
            "chatgpt",
            "connectors",
            "crush",
            "cursor",
            "goose",
            "hermes",
            "opencode",
        ],
        expected_default_features: None,
        repo_rel: "../franken_agent_detection",
        manifest_rel: "Cargo.toml",
        patch_url: Some("https://github.com/Dicklesworthstone/franken_agent_detection"),
        patch_key: Some("franken-agent-detection"),
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "asupersync",
        dep_table: "dependencies",
        dep_key: "asupersync",
        crate_package_name: "asupersync",
        manifest_package_field: None,
        // crates.io-only exact pin: every source (direct dep, frankensqlite
        // transitive, frankensearch transitive) resolves to a single published
        // release. The 0.4.x line (>=0.4.3,<0.5) is required by fsqlite 0.3.x,
        // whose public API names asupersync 0.4.x types. The current 0.4.9 pin
        // preserves the 0.4.x typed-result cancellation contract.
        // Empty `expected_git` signals `validate_manifest_dependency_spec`
        // to skip git/rev checks.
        expected_git: "",
        expected_rev: "",
        expected_version: "0.4.9",
        expected_features: &["test-internals", "tls-native-roots"],
        expected_default_features: None,
        repo_rel: "../asupersync",
        manifest_rel: "Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "frankensearch",
        dep_table: "dependencies",
        dep_key: "frankensearch",
        crate_package_name: "frankensearch",
        manifest_package_field: None,
        // Registry pin (gh#429, gh#410). 0.4.2 extends the native Windows Quill
        // publication line with the explicit multilingual MiniLM embedding
        // profile while preserving the first crates.io line carrying
        // the pure-Rust `native` feature and the explicit `cass-compat` ->
        // `lexical-tantivy` foreign-index surface (which keeps CASS schema-v8
        // access independent from FrankenSearch's swappable generic lexical
        // backend — cass #308, bd-8nqz.5). Registry 0.3.2 was a stale
        // same-version twin of an older tree (no quill/cass-compat/native);
        // the exact `=0.4.2` pin exists so resolution can never reach it.
        // Empty `expected_git` signals `validate_manifest_dependency_spec`
        // to skip git/rev checks.
        expected_git: "",
        expected_rev: "",
        expected_version: "0.4.2",
        // cass #308: the ort/ONNX `fastembed` stack was removed; semantic
        // embedding + reranking are now pure-Rust via frankensearch's `native`
        // feature, kept always-on here (no AVX/ONNX static-init hazard, so no
        // separate `-baseline` build is needed). Bead tg5o9 retired the vacuous
        // cass `semantic` feature; semantic readiness is now determined solely
        // by runtime model/vector assets.
        // `quill` added for the CASS->Quill lexical flip: it links the native
        // Quill engine alongside the Tantivy incumbent so the two can be
        // compared before Tantivy is dropped. `cass-compat` leaves with
        // Tantivy once the flip completes.
        expected_features: &["ann", "cass-compat", "hash", "native", "quill"],
        expected_default_features: Some(false),
        repo_rel: "../frankensearch",
        manifest_rel: "frankensearch/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "ftui facade",
        dep_table: "dependencies",
        dep_key: "ftui",
        crate_package_name: "ftui",
        manifest_package_field: None,
        expected_git: "",
        expected_rev: "",
        expected_version: "0.5.0",
        expected_features: &[],
        expected_default_features: None,
        repo_rel: "../frankentui",
        manifest_rel: "crates/ftui/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "ftui-runtime",
        dep_table: "dependencies",
        dep_key: "ftui-runtime",
        crate_package_name: "ftui-runtime",
        manifest_package_field: None,
        expected_git: "",
        expected_rev: "",
        expected_version: "0.5.0",
        expected_features: &["crossterm-compat", "native-backend"],
        expected_default_features: None,
        repo_rel: "../frankentui",
        manifest_rel: "crates/ftui-runtime/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "ftui-tty",
        dep_table: "dependencies",
        dep_key: "ftui-tty",
        crate_package_name: "ftui-tty",
        manifest_package_field: None,
        expected_git: "",
        expected_rev: "",
        expected_version: "0.5.0",
        expected_features: &[],
        expected_default_features: None,
        repo_rel: "../frankentui",
        manifest_rel: "crates/ftui-tty/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "ftui-extras",
        dep_table: "dependencies",
        dep_key: "ftui-extras",
        crate_package_name: "ftui-extras",
        manifest_package_field: None,
        expected_git: "",
        expected_rev: "",
        expected_version: "0.5.0",
        expected_features: &[
            "canvas",
            "charts",
            "clipboard",
            "clipboard-fallback",
            "export",
            "forms",
            "help",
            "markdown",
            "syntax",
            "theme",
            "validation",
            "visual-fx",
        ],
        expected_default_features: Some(false),
        repo_rel: "../frankentui",
        manifest_rel: "crates/ftui-extras/Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "toon",
        dep_table: "dependencies",
        dep_key: "toon",
        crate_package_name: "tru",
        manifest_package_field: Some("tru"),
        // GH#416: registry pin. crates.io 0.2.4 differs from the previously
        // pinned git rev d7185c78 by exactly one TEST assertion line
        // (src/decode/event_builder.rs); production sources are
        // byte-identical (verified by tree diff, not the version field).
        expected_git: "",
        expected_rev: "",
        expected_version: "0.2.4",
        expected_features: &[],
        expected_default_features: None,
        repo_rel: "../toon_rust",
        manifest_rel: "Cargo.toml",
        patch_url: None,
        patch_key: None,
        mode: ValidationMode::StrictOptIn,
    },
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed={STRICT_PATH_DEP_ENV}");

    // MSVC reserves only 1 MiB for an executable's main thread by default.
    // CASS's clap command graph and startup state exceed that in debug builds,
    // causing even `cass --version` to abort before argument dispatch. Reserve
    // virtual address space here for the actual binary; thread stacks remain
    // independently bounded by their structured spawn sites.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-arg-bin=cass=/STACK:8388608");
    }

    let manifest_dir = match env::var("CARGO_MANIFEST_DIR") {
        Ok(value) => PathBuf::from(value),
        Err(err) => fatal(format!(
            "CARGO_MANIFEST_DIR should be set by Cargo before running build.rs: {err}"
        )),
    };
    let manifest_path = manifest_dir.join("Cargo.toml");
    let manifest_text = match fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(err) => fatal(format!("failed to read {}: {err}", manifest_path.display())),
    };
    let manifest: Value = match toml::from_str(&manifest_text) {
        Ok(value) => value,
        Err(err) => fatal(format!(
            "failed to parse {}: {err}",
            manifest_path.display()
        )),
    };

    let packaged_manifest = manifest_dir.join("Cargo.toml.orig").is_file();
    validate_path_dependency_contracts(&manifest_dir, &manifest, packaged_manifest);
    emit_vergen_metadata();
    emit_build_commit_metadata(&manifest_dir);
}

/// Embed the build commit at compile time (GH #399).
///
/// A binary built from `main` and the binary from the nearest tag are
/// otherwise indistinguishable at runtime: bare `version` in clap prints only
/// `CARGO_PKG_VERSION`. Resolve the commit HERE, against the crate's own
/// checkout (`CARGO_MANIFEST_DIR`), never at runtime against the process CWD
/// (ubs#79 is the failure mode to avoid: a runtime `git rev-parse` reports
/// the *scanned* repository's commit as the build SHA).
///
/// Emits:
/// - `VERGEN_GIT_SHA` — full commit hash (name kept because doctor run
///   journaling already reads `option_env!("VERGEN_GIT_SHA")`); only when
///   resolvable.
/// - `CASS_BUILD_COMMIT` — short (12-char) hash with a `-dirty` suffix when
///   the worktree has uncommitted tracked changes; only when resolvable.
/// - `CASS_BUILD_COMMIT_DATE` — commit date (`YYYY-MM-DD`); only when
///   resolvable.
/// - `CASS_VERSION_FULL` — ALWAYS emitted: `<semver> (<short-sha> <date>)`
///   when git metadata is available, plain `<semver>` otherwise (crates.io /
///   tarball builds have no `.git`).
fn emit_build_commit_metadata(manifest_dir: &Path) {
    let crate_version = env::var("CARGO_PKG_VERSION").unwrap_or_default();

    // Rebuild when HEAD moves or the checked-out branch's ref advances, so a
    // cached build script cannot pin a stale SHA into a fresh binary.
    let git_dir = manifest_dir.join(".git");
    if git_dir.exists() {
        let head = git_dir.join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
        if let Ok(head_contents) = fs::read_to_string(&head)
            && let Some(reference) = head_contents.trim().strip_prefix("ref: ")
        {
            let ref_path = git_dir.join(reference);
            if ref_path.exists() {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }

    let state = git_state(manifest_dir).ok();
    let commit_date = git_output(manifest_dir, &["show", "-s", "--format=%cs", "HEAD"])
        .map(|s| s.trim().to_string())
        .ok();

    let full_version = match &state {
        Some(state) if state.head.len() >= 12 => {
            let mut short = state.head[..12].to_string();
            if state.dirty {
                short.push_str("-dirty");
            }
            println!("cargo:rustc-env=VERGEN_GIT_SHA={}", state.head);
            println!("cargo:rustc-env=CASS_BUILD_COMMIT={short}");
            if let Some(date) = &commit_date {
                println!("cargo:rustc-env=CASS_BUILD_COMMIT_DATE={date}");
                format!("{crate_version} ({short} {date})")
            } else {
                format!("{crate_version} ({short})")
            }
        }
        _ => crate_version.clone(),
    };
    println!("cargo:rustc-env=CASS_VERSION_FULL={full_version}");
}

fn validate_path_dependency_contracts(
    manifest_dir: &Path,
    manifest: &Value,
    packaged_manifest: bool,
) {
    let strict_enabled = strict_path_dep_validation_enabled();
    validate_fsqlite_source_pin(manifest_dir, manifest, packaged_manifest);

    for contract in CONTRACTS {
        validate_manifest_dependency_spec(manifest, contract, packaged_manifest);

        if contract.mode == ValidationMode::ActivePathOverride {
            validate_patch_path(manifest, contract);
        }

        if contract.mode == ValidationMode::ActivePathOverride || strict_enabled {
            validate_local_contract(manifest_dir, contract, strict_enabled);
        }
    }
}

fn validate_fsqlite_source_pin(manifest_dir: &Path, manifest: &Value, packaged_manifest: bool) {
    // The fsqlite engine family must resolve exclusively from crates.io at
    // one exact version. The single-source identity is load-bearing for the
    // read-only FTS5 integrity preflight used by CASS on Windows.
    const EXPECTED_VERSION: &str = "0.3.16";
    const EXPECTED_REGISTRY_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

    // 1. With the family on crates.io (e926644f), a `[patch]` table is no
    //    longer required — but if one exists, it must not silently redirect
    //    any fsqlite family member back to a shadow source.
    if !packaged_manifest
        && let Some(patch_tables) = manifest.get("patch")
        && let Some(crates_io) = patch_tables.get("crates-io").and_then(Value::as_table)
    {
        for dependency in crates_io.keys() {
            if dependency == "fsqlite" || dependency.starts_with("fsqlite-") {
                fatal(format!(
                    "dependency source contract violation for {dependency}: the fsqlite \
                     family resolves from crates.io at ={EXPECTED_VERSION}; a \
                     [patch.crates-io] redirect would reintroduce an unreviewed shadow \
                     source"
                ));
            }
        }
    }

    // 2. Lockfile convergence: every resolved fsqlite-family package must be
    //    the pinned source revision, with exactly one version per crate.
    //    Cargo resolves the lockfile before running build scripts, so the
    //    lockfile is authoritative here. Packaged manifests (`cargo package`
    //    verification builds) re-resolve into a fresh lockfile that inherits
    //    these same requirements from the manifest pin.
    let lock_path = manifest_dir.join("Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    let lock_text = match fs::read_to_string(&lock_path) {
        Ok(text) => text,
        Err(err) => {
            if packaged_manifest {
                return;
            }
            fatal(format!(
                "dependency source contract: failed to read {}: {err}",
                lock_path.display()
            ))
        }
    };
    let lock: Value = match toml::from_str(&lock_text) {
        Ok(value) => value,
        Err(err) => fatal(format!(
            "dependency source contract: failed to parse {}: {err}",
            lock_path.display()
        )),
    };
    let packages = lock
        .get("package")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            fatal("dependency source contract: Cargo.lock has no [[package]] entries")
        });
    // Collect EVERY off-pin family member before failing, and print the
    // exact remediation for each (GH#417): reporting only the first
    // mismatch made a multi-member drift a whack-a-mole loop, and only 2 of
    // the ~20 family members are direct dependencies, so a bare
    // `cargo update` can float any transitive member off-pin.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut violations: Vec<String> = Vec::new();
    let mut remediations: Vec<String> = Vec::new();
    for package in packages {
        let name = package.get("name").and_then(Value::as_str).unwrap_or("");
        if !(name == "fsqlite" || name.starts_with("fsqlite-")) {
            continue;
        }
        let version = package.get("version").and_then(Value::as_str).unwrap_or("");
        if !seen.insert(name) {
            violations.push(format!(
                "`{name}` resolves more than once; the fsqlite family must converge on \
                 a single source revision"
            ));
        }
        if version != EXPECTED_VERSION {
            violations.push(format!(
                "`{name}` resolves at `{version}`, expected `{EXPECTED_VERSION}`"
            ));
            remediations.push(format!(
                "cargo update -p {name}@{version} --precise {EXPECTED_VERSION}"
            ));
        }
        let source = package.get("source").and_then(Value::as_str).unwrap_or("");
        if source != EXPECTED_REGISTRY_SOURCE {
            violations.push(format!(
                "`{name}` resolves from `{source}`, expected the crates.io registry \
                 (`{EXPECTED_REGISTRY_SOURCE}`)"
            ));
        }
    }
    if !seen.contains("fsqlite") {
        violations.push(
            "Cargo.lock does not resolve `fsqlite`; the engine dependency is missing".to_owned(),
        );
    }
    if !violations.is_empty() {
        let mut message = format!(
            "dependency source contract violation: {} fsqlite-family problem(s) in Cargo.lock:\n",
            violations.len()
        );
        for violation in &violations {
            message.push_str("  - ");
            message.push_str(violation);
            message.push('\n');
        }
        if !remediations.is_empty() {
            message.push_str("remediate every off-pin member, then rebuild:\n");
            for remediation in &remediations {
                message.push_str("  ");
                message.push_str(remediation);
                message.push('\n');
            }
        }
        fatal(message);
    }
}

fn validate_manifest_dependency_spec(
    manifest: &Value,
    contract: &DependencyContract,
    packaged_manifest: bool,
) {
    let spec = inline_table(
        table(manifest, contract.dep_table, "manifest root"),
        contract.dep_key,
        contract.dep_table,
    );

    validate_manifest_dependency_version(spec, contract, packaged_manifest);

    if contract.expected_git.is_empty() {
        // Pure crates.io dependency: lock in the registry version, which is the
        // only source identity crates.io gives us.
        if spec.contains_key("git") || spec.contains_key("rev") {
            contract_error(
                contract,
                format!(
                    "dependency `{}` in [{}] is a crates.io dep in this contract; remove `git`/`rev`",
                    contract.dep_key, contract.dep_table
                ),
            );
        }
    } else if packaged_manifest && !spec.contains_key("git") && !spec.contains_key("rev") {
        // Cargo rewrites git dependencies to registry dependencies in the
        // generated package manifest used by `cargo publish` verification.
        // Validate that rewritten shape against the version we expect instead
        // of requiring `git`/`rev` keys that no longer exist there.
    } else {
        let actual_git = string_value(spec, "git", contract.dep_key);
        if actual_git != contract.expected_git {
            contract_error(
                contract,
                format!(
                    "dependency `{}` in [{}] must pin git = `{}`, found `{}`",
                    contract.dep_key, contract.dep_table, contract.expected_git, actual_git
                ),
            );
        }

        let actual_rev = string_value(spec, "rev", contract.dep_key);
        if actual_rev != contract.expected_rev {
            contract_error(
                contract,
                format!(
                    "dependency `{}` in [{}] must pin rev = `{}`, found `{}`",
                    contract.dep_key, contract.dep_table, contract.expected_rev, actual_rev
                ),
            );
        }
    }

    let actual_package = spec.get("package").and_then(Value::as_str);
    if actual_package != contract.manifest_package_field {
        let expected = contract.manifest_package_field.unwrap_or("<omitted>");
        let actual = actual_package.unwrap_or("<omitted>");
        contract_error(
            contract,
            format!(
                "dependency `{}` in [{}] must use package = `{}`, found `{}`",
                contract.dep_key, contract.dep_table, expected, actual
            ),
        );
    }

    let actual_features = feature_set(spec.get("features"));
    let expected_features: BTreeSet<String> = contract
        .expected_features
        .iter()
        .map(|feature| (*feature).to_string())
        .collect();
    if actual_features != expected_features {
        contract_error(
            contract,
            format!(
                "dependency `{}` in [{}] must enable features {:?}, found {:?}",
                contract.dep_key, contract.dep_table, expected_features, actual_features
            ),
        );
    }

    if let Some(expected_default_features) = contract.expected_default_features {
        let actual_default_features = spec
            .get("default-features")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if actual_default_features != expected_default_features {
            contract_error(
                contract,
                format!(
                    "dependency `{}` in [{}] must use default-features = `{}`, found `{}`",
                    contract.dep_key,
                    contract.dep_table,
                    expected_default_features,
                    actual_default_features
                ),
            );
        }
    }
}

fn validate_manifest_dependency_version(
    spec: &toml::map::Map<String, Value>,
    contract: &DependencyContract,
    packaged_manifest: bool,
) {
    let actual_version = string_value(spec, "version", contract.dep_key);
    let expected_manifest_version = expected_manifest_version_requirement(contract);
    let package_manifest_may_strip_exact_operator =
        packaged_manifest || !contract.expected_git.is_empty();
    let version_matches = actual_version == expected_manifest_version
        || (package_manifest_may_strip_exact_operator
            && actual_version == contract.expected_version);
    if !version_matches {
        contract_error(
            contract,
            format!(
                "dependency `{}` in [{}] must pin version = `{}`, found `{}`",
                contract.dep_key, contract.dep_table, expected_manifest_version, actual_version
            ),
        );
    }
}

fn expected_manifest_version_requirement(contract: &DependencyContract) -> String {
    format!("={}", contract.expected_version)
}

fn validate_patch_path(manifest: &Value, contract: &DependencyContract) {
    let Some(patch_url) = contract.patch_url else {
        contract_error(
            contract,
            "active path override contracts must provide patch_url".to_string(),
        );
    };
    let Some(patch_key) = contract.patch_key else {
        contract_error(
            contract,
            "active path override contracts must provide patch_key".to_string(),
        );
    };

    let patch_tables = table(manifest, "patch", "manifest root");
    let patch_source = table_value(Some(patch_tables), patch_url, "patch source");
    let Some(patch_source_table) = patch_source.as_table() else {
        contract_error(
            contract,
            format!("[patch] source `{patch_url}` must be a TOML table"),
        );
    };
    let patch_entry = inline_table(patch_source_table, patch_key, "[patch] source");
    let actual_path = string_value(patch_entry, "path", patch_key);
    let expected_path = expected_patch_path(contract);

    if actual_path != expected_path {
        contract_error(
            contract,
            format!(
                "[patch.\"{patch_url}\"].{patch_key}.path must be `{expected_path}`, found `{actual_path}`"
            ),
        );
    }
}

fn validate_local_contract(
    manifest_dir: &Path,
    contract: &DependencyContract,
    strict_enabled: bool,
) {
    let repo_root = manifest_dir.join(contract.repo_rel);
    let manifest_path = repo_root.join(contract.manifest_rel);
    println!("cargo:rerun-if-changed={}", manifest_path.display());

    let local_manifest_text = match fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(err) if contract.mode == ValidationMode::StrictOptIn => {
            // Optional sibling repo not checked out; skip validation.
            // Only ActivePathOverride repos are required on disk.
            println!(
                "cargo:warning=skipping {} contract validation: sibling manifest `{}` not found: {err}",
                contract.label,
                manifest_path.display()
            );
            return;
        }
        Err(err) => contract_error(
            contract,
            format!(
                "expected sibling manifest at `{}` but could not read it: {err}",
                manifest_path.display()
            ),
        ),
    };
    let local_manifest: Value = toml::from_str(&local_manifest_text).unwrap_or_else(|err| {
        contract_error(
            contract,
            format!(
                "failed to parse sibling manifest `{}`: {err}",
                manifest_path.display()
            ),
        )
    });

    let package_table = table(
        &local_manifest,
        "package",
        &manifest_path.display().to_string(),
    );
    let package_name = table_value(Some(package_table), "name", "package")
        .as_str()
        .unwrap_or_else(|| {
            contract_error(
                contract,
                format!(
                    "sibling manifest `{}` is missing a string package.name",
                    manifest_path.display()
                ),
            )
        });
    if package_name != contract.crate_package_name {
        contract_error(
            contract,
            format!(
                "sibling manifest `{}` must expose package `{}`, found `{}`",
                manifest_path.display(),
                contract.crate_package_name,
                package_name
            ),
        );
    }

    let version = table_value(Some(package_table), "version", "package")
        .as_str()
        .unwrap_or_else(|| {
            contract_error(
                contract,
                format!(
                    "sibling manifest `{}` is missing a string package.version",
                    manifest_path.display()
                ),
            )
        });
    if version != contract.expected_version {
        contract_error(
            contract,
            format!(
                "sibling manifest `{}` must expose version `{}`, found `{}`",
                manifest_path.display(),
                contract.expected_version,
                version
            ),
        );
    }

    let features = local_manifest.get("features").and_then(Value::as_table);
    let missing_feature = contract
        .expected_features
        .iter()
        .copied()
        .find(|feature| !features.is_some_and(|table| table.contains_key(*feature)));
    if let Some(feature) = missing_feature {
        contract_error(
            contract,
            format!(
                "sibling manifest `{}` must provide feature `{}` because cass enables it",
                manifest_path.display(),
                feature
            ),
        );
    }

    match (strict_enabled, contract.mode, git_state(&repo_root)) {
        (true, _, Ok(state)) => validate_strict_git_state(contract, &repo_root, &state),
        (true, _, Err(err)) => contract_error(
            contract,
            format!(
                "strict validation could not inspect git state for `{}`: {err}",
                repo_root.display()
            ),
        ),
        (false, ValidationMode::ActivePathOverride, Ok(state)) => {
            warn_on_path_drift(contract, &repo_root, &state)
        }
        _ => {}
    }
}

fn validate_strict_git_state(contract: &DependencyContract, repo_root: &Path, state: &GitState) {
    // Crates.io-only contracts (empty `expected_rev`) intentionally
    // have nothing to enforce at the sibling repo level; the actual
    // pin lives in the crates.io version. A local sibling checkout
    // may be on any branch and may be dirty; that's fine because
    // we're not building against it. Skip both sub-checks.
    if contract.expected_rev.is_empty() {
        return;
    }
    if !state.head.starts_with(contract.expected_rev) {
        contract_error(
            contract,
            format!(
                "strict path dependency validation expected `{}` HEAD to start with `{}`, found `{}`",
                repo_root.display(),
                contract.expected_rev,
                state.head
            ),
        );
    }

    if state.dirty {
        contract_error(
            contract,
            format!(
                "strict path dependency validation requires `{}` to have a clean worktree",
                repo_root.display()
            ),
        );
    }
}

fn warn_on_path_drift(contract: &DependencyContract, repo_root: &Path, state: &GitState) {
    if state.head.starts_with(contract.expected_rev) && !state.dirty {
        return;
    }

    let mut details = Vec::new();
    if !state.head.starts_with(contract.expected_rev) {
        details.push(format!(
            "HEAD {} does not match pinned rev {}",
            state.head, contract.expected_rev
        ));
    }
    if state.dirty {
        details.push("worktree is dirty".to_string());
    }

    println!(
        "cargo:warning=path dependency drift for {} at {}: {}. Enable `--features {}` or set {}=1 to make this a hard error.",
        contract.label,
        repo_root.display(),
        details.join("; "),
        STRICT_PATH_DEP_FEATURE,
        STRICT_PATH_DEP_ENV
    );
}

fn strict_path_dep_validation_enabled() -> bool {
    env::var_os("CARGO_FEATURE_STRICT_PATH_DEP_VALIDATION").is_some()
        || matches!(
            env::var(STRICT_PATH_DEP_ENV)
                .ok()
                .as_deref()
                .map(|value| value.trim().to_ascii_lowercase()),
            Some(value) if matches!(value.as_str(), "1" | "true" | "yes" | "on")
        )
}

fn expected_patch_path(contract: &DependencyContract) -> String {
    if contract.manifest_rel == "Cargo.toml" {
        contract.repo_rel.to_string()
    } else {
        format!(
            "{}/{}",
            contract.repo_rel,
            contract
                .manifest_rel
                .trim_end_matches("Cargo.toml")
                .trim_end_matches('/')
        )
    }
}

fn git_state(repo_root: &Path) -> Result<GitState, String> {
    let head = git_output(repo_root, &["rev-parse", "HEAD"])?;
    let dirty = !git_output(repo_root, &["status", "--short", "--untracked-files=no"])?
        .trim()
        .is_empty();
    Ok(GitState {
        head: head.trim().to_string(),
        dirty,
    })
}

fn git_output(repo_root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|err| format!("failed to execute git {:?}: {err}", args))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn emit_vergen_metadata() {
    use vergen::{Build, Cargo, Emitter};

    // vergen 10 replaced the `XxxBuilder::all_*()` constructors (which returned a
    // `Result`) with bon-based config structs whose `all_*()` associated fns
    // return the value directly. `Build::all_build()` / `Cargo::all_cargo()`
    // preserve the previous "emit every build/cargo instruction" behavior.
    let mut emitter = Emitter::default();
    let _ = emitter.add_instructions(&Build::all_build());
    let _ = emitter.add_instructions(&Cargo::all_cargo());

    if let Err(err) = emitter.emit() {
        eprintln!("vergen emit skipped: {err}");
    }
}

fn table<'a>(value: &'a Value, key: &str, context: &str) -> &'a toml::map::Map<String, Value> {
    let value = table_value(value.as_table(), key, context);
    match value.as_table() {
        Some(table) => table,
        None => fatal(format!("{context} key `{key}` must be a TOML table")),
    }
}

fn inline_table<'a>(
    table: &'a toml::map::Map<String, Value>,
    key: &str,
    context: &str,
) -> &'a toml::map::Map<String, Value> {
    let value = table_value(Some(table), key, context);
    match value.as_table() {
        Some(table) => table,
        None => fatal(format!("{context} key `{key}` must be an inline table")),
    }
}

fn table_value<'a>(
    table: Option<&'a toml::map::Map<String, Value>>,
    key: &str,
    context: &str,
) -> &'a Value {
    match table.and_then(|table| table.get(key)) {
        Some(value) => value,
        None => fatal(format!("{context} is missing key `{key}`")),
    }
}

fn string_value<'a>(table: &'a toml::map::Map<String, Value>, key: &str, context: &str) -> &'a str {
    match table.get(key).and_then(Value::as_str) {
        Some(value) => value,
        None => fatal(format!("{context} is missing string key `{key}`")),
    }
}

fn feature_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn contract_error(contract: &DependencyContract, message: String) -> ! {
    fatal(format!(
        "dependency source contract violation for {}: {}\nupdate Cargo.toml, build.rs, and the README dependency source contract together",
        contract.label, message
    ))
}

fn fatal(message: impl fmt::Display) -> ! {
    eprintln!("{message}");
    println!("cargo:error={message}");
    process::exit(1);
}
