use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        expected_git: "https://github.com/Dicklesworthstone/frankensqlite",
        expected_rev: "c0d2c375",
        expected_version: "0.1.2",
        expected_features: &["fts5"],
        expected_default_features: None,
        repo_rel: "../frankensqlite",
        manifest_rel: "crates/fsqlite/Cargo.toml",
        patch_url: Some("https://github.com/Dicklesworthstone/frankensqlite"),
        patch_key: Some("fsqlite"),
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "frankensqlite shared types",
        dep_table: "dev-dependencies",
        dep_key: "fsqlite-types",
        crate_package_name: "fsqlite-types",
        manifest_package_field: Some("fsqlite-types"),
        expected_git: "https://github.com/Dicklesworthstone/frankensqlite",
        expected_rev: "c0d2c375",
        expected_version: "0.1.2",
        expected_features: &[],
        expected_default_features: None,
        repo_rel: "../frankensqlite",
        manifest_rel: "crates/fsqlite-types/Cargo.toml",
        patch_url: Some("https://github.com/Dicklesworthstone/frankensqlite"),
        patch_key: Some("fsqlite-types"),
        mode: ValidationMode::StrictOptIn,
    },
    DependencyContract {
        label: "franken_agent_detection",
        dep_table: "dependencies",
        dep_key: "franken-agent-detection",
        crate_package_name: "franken-agent-detection",
        manifest_package_field: None,
        expected_git: "https://github.com/Dicklesworthstone/franken_agent_detection",
        expected_rev: "ba9c598",
        expected_version: "0.1.3",
        expected_features: &["chatgpt", "connectors", "crush", "cursor", "opencode"],
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
        expected_git: "https://github.com/Dicklesworthstone/asupersync",
        expected_rev: "08dd31df",
        expected_version: "0.2.9",
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
        expected_git: "https://github.com/Dicklesworthstone/frankensearch",
        expected_rev: "fa186910",
        expected_version: "0.1.0",
        expected_features: &["ann", "fastembed-reranker", "hash", "lexical"],
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
        expected_git: "https://github.com/Dicklesworthstone/frankentui",
        expected_rev: "7a91089",
        expected_version: "0.2.1",
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
        expected_git: "https://github.com/Dicklesworthstone/frankentui",
        expected_rev: "7a91089",
        expected_version: "0.2.1",
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
        expected_git: "https://github.com/Dicklesworthstone/frankentui",
        expected_rev: "7a91089",
        expected_version: "0.2.1",
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
        expected_git: "https://github.com/Dicklesworthstone/frankentui",
        expected_rev: "7a91089",
        expected_version: "0.2.1",
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
        expected_git: "https://github.com/Dicklesworthstone/toon_rust",
        expected_rev: "bc3f9da",
        expected_version: "0.2.1",
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

    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR should be set by Cargo"),
    );
    let manifest_path = manifest_dir.join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", manifest_path.display()));
    let manifest: Value = toml::from_str(&manifest_text)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", manifest_path.display()));

    validate_path_dependency_contracts(&manifest_dir, &manifest);
    emit_vergen_metadata();
}

fn validate_path_dependency_contracts(manifest_dir: &Path, manifest: &Value) {
    let strict_enabled = strict_path_dep_validation_enabled();

    for contract in CONTRACTS {
        validate_manifest_dependency_spec(manifest, contract);

        if contract.mode == ValidationMode::ActivePathOverride {
            validate_patch_path(manifest, contract);
        }

        if contract.mode == ValidationMode::ActivePathOverride || strict_enabled {
            validate_local_contract(manifest_dir, contract, strict_enabled);
        }
    }
}

fn validate_manifest_dependency_spec(manifest: &Value, contract: &DependencyContract) {
    let spec = inline_table(
        table(manifest, contract.dep_table, "manifest root"),
        contract.dep_key,
        contract.dep_table,
    );

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

fn validate_patch_path(manifest: &Value, contract: &DependencyContract) {
    let patch_url = contract
        .patch_url
        .expect("active path override contracts must provide patch_url");
    let patch_key = contract
        .patch_key
        .expect("active path override contracts must provide patch_key");

    let patch_tables = table(manifest, "patch", "manifest root");
    let patch_source = table_value(Some(patch_tables), patch_url, "patch source");
    let patch_entry = inline_table(
        patch_source
            .as_table()
            .unwrap_or_else(|| panic!("[patch] source `{patch_url}` must be a TOML table")),
        patch_key,
        "[patch] source",
    );
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
            // Optional sibling repo not checked out — skip validation.
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
    for feature in contract.expected_features {
        let has_feature = features.is_some_and(|table| table.contains_key(*feature));
        if !has_feature {
            contract_error(
                contract,
                format!(
                    "sibling manifest `{}` must provide feature `{}` because cass enables it",
                    manifest_path.display(),
                    feature
                ),
            );
        }
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
    use vergen::{BuildBuilder, CargoBuilder, Emitter};

    let mut emitter = Emitter::default();

    if let Ok(build) = BuildBuilder::all_build() {
        let _ = emitter.add_instructions(&build);
    }
    if let Ok(cargo) = CargoBuilder::all_cargo() {
        let _ = emitter.add_instructions(&cargo);
    }

    if let Err(err) = emitter.emit() {
        eprintln!("vergen emit skipped: {err}");
    }
}

fn table<'a>(value: &'a Value, key: &str, context: &str) -> &'a toml::map::Map<String, Value> {
    table_value(value.as_table(), key, context)
        .as_table()
        .unwrap_or_else(|| panic!("{context} key `{key}` must be a TOML table"))
}

fn inline_table<'a>(
    table: &'a toml::map::Map<String, Value>,
    key: &str,
    context: &str,
) -> &'a toml::map::Map<String, Value> {
    table_value(Some(table), key, context)
        .as_table()
        .unwrap_or_else(|| panic!("{context} key `{key}` must be an inline table"))
}

fn table_value<'a>(
    table: Option<&'a toml::map::Map<String, Value>>,
    key: &str,
    context: &str,
) -> &'a Value {
    table
        .and_then(|table| table.get(key))
        .unwrap_or_else(|| panic!("{context} is missing key `{key}`"))
}

fn string_value<'a>(table: &'a toml::map::Map<String, Value>, key: &str, context: &str) -> &'a str {
    table
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{context} is missing string key `{key}`"))
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
    panic!(
        "path dependency contract violation for {}: {}\nupdate Cargo.toml, build.rs, and the README sibling dependency contract together",
        contract.label, message
    );
}
