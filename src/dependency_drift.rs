use chrono::Utc;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SCHEMA_VERSION: &str = "cass.swarm.dependency_drift.v1";
const STRICT_CHECK_COMMAND: &str = "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-strict-target cargo check --features strict-path-dep-validation";
const FULL_CHECK_COMMAND: &str =
    "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-check-target cargo check --all-targets";
const FSQLITE_REGRESSION_COMMAND: &str = "rch exec -- env CARGO_TARGET_DIR=/tmp/cass-fsqlite-target cargo test --lib cleanup_orphan_fk_rows -- --nocapture";

#[derive(Clone, Copy)]
struct DependencySpec {
    name: &'static str,
    package: &'static str,
    manifest_table: &'static str,
    manifest_key: &'static str,
    source_kind: &'static str,
    repo_rel: &'static str,
    required_tests: &'static [&'static str],
}

#[derive(Clone, Default)]
struct ManifestPin {
    status: String,
    git: Option<String>,
    rev: Option<String>,
    version: Option<String>,
    package: Option<String>,
}

#[derive(Clone)]
struct DependencyObservation {
    name: String,
    package: String,
    manifest_table: String,
    manifest_key: String,
    source_kind: String,
    git: Option<String>,
    version: Option<String>,
    pinned_rev: Option<String>,
    manifest_status: String,
    sibling_path: Option<String>,
    sibling_status: String,
    local_head: Option<String>,
    dirty: bool,
    upstream_status: String,
    required_tests: Vec<String>,
}

#[derive(Clone)]
struct DependencyRisk {
    level: &'static str,
    kind: &'static str,
    release_readiness: &'static str,
    summary: String,
    partial: bool,
}

const DEPENDENCY_SPECS: &[DependencySpec] = &[
    DependencySpec {
        name: "frankensqlite",
        package: "fsqlite",
        manifest_table: "dependencies",
        manifest_key: "frankensqlite",
        source_kind: "git",
        repo_rel: "../frankensqlite",
        required_tests: &[
            STRICT_CHECK_COMMAND,
            FULL_CHECK_COMMAND,
            FSQLITE_REGRESSION_COMMAND,
        ],
    },
    DependencySpec {
        name: "fsqlite-types",
        package: "fsqlite-types",
        manifest_table: "dev-dependencies",
        manifest_key: "fsqlite-types",
        source_kind: "git",
        repo_rel: "../frankensqlite",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "franken-agent-detection",
        package: "franken-agent-detection",
        manifest_table: "dependencies",
        manifest_key: "franken-agent-detection",
        source_kind: "git",
        repo_rel: "../franken_agent_detection",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "asupersync",
        package: "asupersync",
        manifest_table: "dependencies",
        manifest_key: "asupersync",
        source_kind: "registry",
        repo_rel: "../asupersync",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "frankensearch",
        package: "frankensearch",
        manifest_table: "dependencies",
        manifest_key: "frankensearch",
        source_kind: "git",
        repo_rel: "../frankensearch",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "ftui",
        package: "ftui",
        manifest_table: "dependencies",
        manifest_key: "ftui",
        source_kind: "git",
        repo_rel: "../frankentui",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "ftui-runtime",
        package: "ftui-runtime",
        manifest_table: "dependencies",
        manifest_key: "ftui-runtime",
        source_kind: "git",
        repo_rel: "../frankentui",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "ftui-tty",
        package: "ftui-tty",
        manifest_table: "dependencies",
        manifest_key: "ftui-tty",
        source_kind: "git",
        repo_rel: "../frankentui",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "ftui-extras",
        package: "ftui-extras",
        manifest_table: "dependencies",
        manifest_key: "ftui-extras",
        source_kind: "git",
        repo_rel: "../frankentui",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
    DependencySpec {
        name: "toon",
        package: "tru",
        manifest_table: "dependencies",
        manifest_key: "toon",
        source_kind: "git",
        repo_rel: "../toon_rust",
        required_tests: &[STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
    },
];

#[must_use]
pub fn render_dependency_drift_live() -> Value {
    let manifest_dir = runtime_manifest_dir();
    let manifest = read_manifest(&manifest_dir.join("Cargo.toml"));
    let dependencies = DEPENDENCY_SPECS
        .iter()
        .map(|spec| live_observation(spec, &manifest_dir, manifest.as_ref()))
        .collect::<Vec<_>>();

    render_payload("live", "live", dependencies, "not_checked", None)
}

#[must_use]
pub fn render_dependency_drift_fixture(fixture_id: &str, source: Option<&Value>) -> Value {
    let upstream_status = source
        .and_then(|value| value.get("network"))
        .and_then(|network| {
            network
                .get("upstream_status")
                .or_else(|| network.get("status"))
        })
        .and_then(Value::as_str)
        .unwrap_or("not_checked");

    let Some(source) = source else {
        return render_payload(
            fixture_id,
            "fixture",
            Vec::new(),
            upstream_status,
            Some("dependency_drift fixture source is missing"),
        );
    };

    let dependencies = source
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| fixture_observation(value, upstream_status))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let fixture_problem = if dependencies.is_empty() {
        Some("dependency_drift fixture did not include dependencies")
    } else {
        None
    };

    render_payload(
        fixture_id,
        "fixture",
        dependencies,
        upstream_status,
        fixture_problem,
    )
}

fn runtime_manifest_dir() -> PathBuf {
    match std::env::current_dir() {
        Ok(path) if path.join("Cargo.toml").is_file() => path,
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
    }
}

fn read_manifest(path: &Path) -> Option<toml::Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
}

fn live_observation(
    spec: &DependencySpec,
    manifest_dir: &Path,
    manifest: Option<&toml::Value>,
) -> DependencyObservation {
    let pin = manifest
        .map(|manifest| manifest_pin(manifest, spec))
        .unwrap_or_else(|| ManifestPin {
            status: "manifest-unavailable".to_string(),
            ..ManifestPin::default()
        });
    let repo_path = manifest_dir.join(spec.repo_rel);
    let sibling_path = Some(display_path(&repo_path));
    let sibling_state = sibling_state(&repo_path);

    DependencyObservation {
        name: spec.name.to_string(),
        package: pin
            .package
            .clone()
            .unwrap_or_else(|| spec.package.to_string()),
        manifest_table: spec.manifest_table.to_string(),
        manifest_key: spec.manifest_key.to_string(),
        source_kind: spec.source_kind.to_string(),
        git: pin.git,
        version: pin.version,
        pinned_rev: pin.rev,
        manifest_status: pin.status,
        sibling_path,
        sibling_status: sibling_state.0,
        local_head: sibling_state.1,
        dirty: sibling_state.2,
        upstream_status: "not_checked".to_string(),
        required_tests: spec
            .required_tests
            .iter()
            .map(|command| (*command).to_string())
            .collect(),
    }
}

fn manifest_pin(manifest: &toml::Value, spec: &DependencySpec) -> ManifestPin {
    let Some(table) = manifest
        .get(spec.manifest_table)
        .and_then(toml::Value::as_table)
    else {
        return ManifestPin {
            status: "missing-table".to_string(),
            ..ManifestPin::default()
        };
    };
    let Some(value) = table.get(spec.manifest_key) else {
        return ManifestPin {
            status: "missing-dependency".to_string(),
            ..ManifestPin::default()
        };
    };

    if let Some(version) = value.as_str() {
        return ManifestPin {
            status: "version-pinned".to_string(),
            version: Some(version.to_string()),
            ..ManifestPin::default()
        };
    }

    let Some(spec_table) = value.as_table() else {
        return ManifestPin {
            status: "invalid-spec".to_string(),
            ..ManifestPin::default()
        };
    };

    let git = spec_table
        .get("git")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let rev = spec_table
        .get("rev")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let version = spec_table
        .get("version")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let package = spec_table
        .get("package")
        .and_then(toml::Value::as_str)
        .map(str::to_string);
    let status = if spec.source_kind == "git" {
        match (&git, &rev) {
            (Some(_), Some(_)) => "pinned",
            (Some(_), None) => "missing-rev",
            (None, Some(_)) => "missing-git",
            (None, None) => "missing-git-rev",
        }
    } else if version.is_some() {
        "version-pinned"
    } else {
        "missing-version"
    };

    ManifestPin {
        status: status.to_string(),
        git,
        rev,
        version,
        package,
    }
}

fn sibling_state(repo_path: &Path) -> (String, Option<String>, bool) {
    if !repo_path.is_dir() {
        return ("missing".to_string(), None, false);
    }

    let head = git_output(repo_path, &["rev-parse", "HEAD"]);
    let status = git_output(
        repo_path,
        &["status", "--porcelain=v1", "--untracked-files=no"],
    );
    match (head, status) {
        (Some(head), Some(status)) => {
            let dirty = !status.trim().is_empty();
            let state = if dirty { "dirty" } else { "clean" };
            (state.to_string(), Some(head), dirty)
        }
        (Some(head), None) => ("unavailable".to_string(), Some(head), false),
        _ => ("unavailable".to_string(), None, false),
    }
}

fn git_output(repo_path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn fixture_observation(value: &Value, inherited_upstream_status: &str) -> DependencyObservation {
    let name = string_field(value, &["name", "dependency", "manifest_key"], "unknown");
    let manifest_key = string_field(value, &["manifest_key", "name", "dependency"], &name);
    let source_kind = string_field(value, &["source_kind"], "git");
    let pinned_rev = nested_string_field(value, &[&["pinned", "rev"], &["pinned", "revision"]])
        .or_else(|| string_field_optional(value, &["pinned_rev", "manifest_rev", "rev"]));
    let version = nested_string_field(value, &[&["pinned", "version"]])
        .or_else(|| string_field_optional(value, &["version", "manifest_version"]));
    let git = nested_string_field(value, &[&["pinned", "git"]])
        .or_else(|| string_field_optional(value, &["git", "manifest_git"]));
    let local_head = string_field_optional(value, &["local_head", "sibling_head"]);
    let dirty = bool_field(value, &["dirty", "sibling_dirty"], false);
    let sibling_status = string_field(
        value,
        &["sibling_status"],
        if dirty { "dirty" } else { "clean" },
    );
    let upstream_status = nested_string_field(value, &[&["upstream", "status"]])
        .or_else(|| string_field_optional(value, &["upstream_status"]))
        .unwrap_or_else(|| inherited_upstream_status.to_string());
    let manifest_status = string_field(
        value,
        &["manifest_status"],
        if source_kind == "git" && pinned_rev.is_none() {
            "missing-rev"
        } else if source_kind == "registry" && version.is_none() {
            "missing-version"
        } else if source_kind == "registry" {
            "version-pinned"
        } else {
            "pinned"
        },
    );
    let package = string_field(value, &["package"], &manifest_key);
    let manifest_table = string_field(value, &["manifest_table"], "dependencies");
    let sibling_path = string_field_optional(value, &["sibling_path", "path"]);
    let required_tests = value
        .get("required_downstream_tests")
        .or_else(|| value.get("required_tests"))
        .and_then(Value::as_array)
        .map(|tests| {
            tests
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|tests| !tests.is_empty())
        .unwrap_or_else(|| {
            vec![
                STRICT_CHECK_COMMAND.to_string(),
                FULL_CHECK_COMMAND.to_string(),
            ]
        });

    DependencyObservation {
        name,
        package,
        manifest_table,
        manifest_key,
        source_kind,
        git,
        version,
        pinned_rev,
        manifest_status,
        sibling_path,
        sibling_status,
        local_head,
        dirty,
        upstream_status,
        required_tests,
    }
}

fn render_payload(
    fixture_id: &str,
    source_kind: &str,
    dependencies: Vec<DependencyObservation>,
    upstream_status: &str,
    fixture_problem: Option<&str>,
) -> Value {
    let rendered_dependencies = dependencies
        .iter()
        .map(render_dependency)
        .collect::<Vec<_>>();
    let summary = summarize(&dependencies, upstream_status, fixture_problem);
    let status = summary
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("partial")
        .to_string();
    let recommendations = recommendations(&dependencies, fixture_problem);

    json!({
        "schema_version": SCHEMA_VERSION,
        "status": status,
        "_meta": {
            "generated_at": Utc::now().to_rfc3339(),
            "source": source_kind,
            "fixture_id": fixture_id,
            "contract": "read-only sibling dependency drift sentinel"
        },
        "summary": summary,
        "dependencies": rendered_dependencies,
        "recommendations": recommendations,
        "mutation_contract": {
            "read_only": true,
            "mutates_git": false,
            "mutates_files": false,
            "runs_builds": false,
            "touches_network": false,
            "network_policy": "remote upstreams are not queried by default"
        },
        "privacy": {
            "contains_session_content": false,
            "contains_secrets": false,
            "redaction_applied": false
        }
    })
}

fn render_dependency(observation: &DependencyObservation) -> Value {
    let risk = classify(observation);
    let revision_matches_pin = revision_matches_pin(observation);
    json!({
        "name": &observation.name,
        "package": &observation.package,
        "manifest": {
            "table": &observation.manifest_table,
            "key": &observation.manifest_key,
            "status": &observation.manifest_status
        },
        "source": {
            "kind": &observation.source_kind,
            "git": &observation.git,
            "version": &observation.version,
            "rev": &observation.pinned_rev
        },
        "sibling": {
            "path": &observation.sibling_path,
            "status": &observation.sibling_status,
            "local_head": &observation.local_head,
            "dirty": observation.dirty,
            "revision_matches_pin": revision_matches_pin
        },
        "upstream": {
            "status": &observation.upstream_status
        },
        "risk": {
            "level": risk.level,
            "kind": risk.kind,
            "release_readiness": risk.release_readiness,
            "summary": risk.summary
        },
        "required_downstream_tests": &observation.required_tests
    })
}

fn summarize(
    dependencies: &[DependencyObservation],
    upstream_status: &str,
    fixture_problem: Option<&str>,
) -> Value {
    let risks = dependencies.iter().map(classify).collect::<Vec<_>>();
    let warning_count = risks.iter().filter(|risk| risk.level == "warning").count();
    let blocked_count = risks
        .iter()
        .filter(|risk| risk.release_readiness == "blocked")
        .count();
    let partial_count = risks.iter().filter(|risk| risk.partial).count()
        + usize::from(upstream_status == "unavailable")
        + usize::from(fixture_problem.is_some());
    let dirty_count = dependencies.iter().filter(|dep| dep.dirty).count();
    let local_rev_mismatch_count = dependencies
        .iter()
        .filter(|dep| revision_matches_pin(dep) == Some(false))
        .count();
    let missing_sibling_count = dependencies
        .iter()
        .filter(|dep| dep.sibling_status == "missing")
        .count();
    let missing_manifest_count = dependencies
        .iter()
        .filter(|dep| dep.manifest_status.starts_with("missing"))
        .count();
    let clean_count = risks.iter().filter(|risk| risk.level == "clean").count();
    let status = if warning_count > 0 || blocked_count > 0 {
        "warning"
    } else if partial_count > 0 {
        "partial"
    } else {
        "ok"
    };
    let release_readiness = if blocked_count > 0 {
        "blocked"
    } else if warning_count > 0 {
        "review-required"
    } else {
        "ready"
    };
    let recommended_action = if blocked_count > 0 {
        "restore-manifest-pin"
    } else if warning_count > 0 {
        "review-sibling-drift"
    } else if partial_count > 0 {
        "optional-sibling-context-missing"
    } else {
        "dependencies-clean"
    };

    json!({
        "status": status,
        "dependency_count": dependencies.len(),
        "clean_count": clean_count,
        "warning_count": warning_count,
        "partial_count": partial_count,
        "dirty_count": dirty_count,
        "local_rev_mismatch_count": local_rev_mismatch_count,
        "missing_sibling_count": missing_sibling_count,
        "missing_manifest_count": missing_manifest_count,
        "network_status": upstream_status,
        "release_readiness": release_readiness,
        "recommended_action": recommended_action,
        "fixture_problem": fixture_problem
    })
}

fn classify(observation: &DependencyObservation) -> DependencyRisk {
    if observation.manifest_status.starts_with("missing")
        || observation.manifest_status == "invalid-spec"
        || observation.manifest_status == "manifest-unavailable"
    {
        return DependencyRisk {
            level: "warning",
            kind: "manifest-pin-missing",
            release_readiness: "blocked",
            summary: format!(
                "{} does not have a complete manifest pin in [{}].{}",
                observation.name, observation.manifest_table, observation.manifest_key
            ),
            partial: false,
        };
    }

    if observation.dirty || observation.sibling_status == "dirty" {
        return DependencyRisk {
            level: "warning",
            kind: "dirty-sibling",
            release_readiness: "review-required",
            summary: format!(
                "{} sibling checkout is dirty; verify the committed pin before depending on local behavior.",
                observation.name
            ),
            partial: false,
        };
    }

    if revision_matches_pin(observation) == Some(false) {
        return DependencyRisk {
            level: "warning",
            kind: "local-head-differs-from-pin",
            release_readiness: "review-required",
            summary: format!(
                "{} sibling checkout HEAD differs from the Cargo.toml pin.",
                observation.name
            ),
            partial: false,
        };
    }

    if observation.sibling_status == "missing" {
        return DependencyRisk {
            level: "info",
            kind: "missing-sibling-checkout",
            release_readiness: "ready",
            summary: format!(
                "{} sibling checkout is absent; Cargo.toml remains authoritative.",
                observation.name
            ),
            partial: true,
        };
    }

    if observation.sibling_status == "unavailable" {
        return DependencyRisk {
            level: "info",
            kind: "sibling-state-unavailable",
            release_readiness: "ready",
            summary: format!(
                "{} sibling checkout exists but git state could not be read.",
                observation.name
            ),
            partial: true,
        };
    }

    if observation.upstream_status == "unavailable" {
        return DependencyRisk {
            level: "info",
            kind: "upstream-unavailable",
            release_readiness: "ready",
            summary: format!(
                "{} upstream was not reachable in the fixture; no network check is run by cass.",
                observation.name
            ),
            partial: true,
        };
    }

    DependencyRisk {
        level: "clean",
        kind: "matches-pin",
        release_readiness: "ready",
        summary: format!(
            "{} manifest pin and local sibling state are aligned.",
            observation.name
        ),
        partial: false,
    }
}

fn revision_matches_pin(observation: &DependencyObservation) -> Option<bool> {
    if observation.source_kind != "git" {
        return None;
    }
    let local_head = observation.local_head.as_deref()?;
    let pinned_rev = observation.pinned_rev.as_deref()?;
    Some(
        local_head == pinned_rev
            || local_head.starts_with(pinned_rev)
            || pinned_rev.starts_with(local_head),
    )
}

fn recommendations(
    dependencies: &[DependencyObservation],
    fixture_problem: Option<&str>,
) -> Vec<Value> {
    let risks = dependencies.iter().map(classify).collect::<Vec<_>>();
    let warning_count = risks.iter().filter(|risk| risk.level == "warning").count();
    let has_fsqlite_warning = dependencies
        .iter()
        .zip(risks.iter())
        .any(|(dep, risk)| dep.package == "fsqlite" && risk.level == "warning");
    let mut output = vec![json!({
        "kind": "strict-path-dep-validation",
        "summary": "Run the strict dependency contract check before enabling local sibling overrides or updating pins.",
        "commands": [STRICT_CHECK_COMMAND],
        "requires_network": false,
        "requires_human_confirmation": false
    })];

    if fixture_problem.is_some() {
        output.push(json!({
            "kind": "fixture-repair",
            "summary": "Provide a sources.dependency_drift.dependencies array in the fixture before treating this projection as complete.",
            "commands": ["cass swarm dependency-drift --json --fixture <fixture>"],
            "requires_network": false,
            "requires_human_confirmation": false
        }));
    }

    if warning_count > 0 {
        output.push(json!({
            "kind": "review-drift-before-release",
            "summary": "Do not treat local sibling behavior as release proof until Cargo.toml pins, build.rs contracts, and downstream checks agree.",
            "commands": [STRICT_CHECK_COMMAND, FULL_CHECK_COMMAND],
            "requires_network": false,
            "requires_human_confirmation": false
        }));
    }

    if has_fsqlite_warning {
        output.push(json!({
            "kind": "frankensqlite-first",
            "summary": "If SQLite behavior is missing, fix /data/projects/frankensqlite and bump the fsqlite pin; do not add new rusqlite workarounds.",
            "commands": [FSQLITE_REGRESSION_COMMAND, STRICT_CHECK_COMMAND],
            "requires_network": false,
            "requires_human_confirmation": false
        }));
    }

    output
}

fn string_field(value: &Value, keys: &[&str], fallback: &str) -> String {
    string_field_optional(value, keys).unwrap_or_else(|| fallback.to_string())
}

fn string_field_optional(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::to_string)
}

fn nested_string_field(value: &Value, paths: &[&[&str]]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| {
            let mut current = value;
            for key in *path {
                current = current.get(*key)?;
            }
            current.as_str()
        })
        .map(str::to_string)
}

fn bool_field(value: &Value, keys: &[&str], fallback: bool) -> bool {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_bool))
        .unwrap_or(fallback)
}

fn display_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}
