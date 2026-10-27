//! Build inputs that can invalidate durable source completions (ayhvu).
//!
//! This deliberately excludes only audited presentation modules. Shared code,
//! unknown source paths, Cargo configuration/dependency identities, and all
//! Cargo features remain inputs. Only the root package's release version is
//! omitted from a verified manifest/lockfile pair. Shared lib.rs edits still
//! invalidate completions. Audit producer dependencies before extending the
//! exclusions: the semantic replay cache also uses this contract.
//!
//! Uses the existing `toml` dependency in both build.rs and the integration
//! target: `cargo test --locked --test source_ingest_contract`.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml::Value;

const PRESENTATION_DIRECTORIES: &[&str] = &["src/ui", "src/html_export", "src/pages_assets"];
const PRESENTATION_FILES: &[&str] = &[
    "src/doctor_robot_docs.rs",
    "src/recipes_robot_docs.rs",
    "src/tui_asciicast.rs",
    "src/ftui_harness.rs",
];

fn presentation_only(path: &Path) -> bool {
    PRESENTATION_DIRECTORIES
        .iter()
        .any(|directory| path.starts_with(directory))
        || PRESENTATION_FILES
            .iter()
            .any(|file| path == Path::new(file))
}

fn input_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot read ingest input {}: {error}", path.display()),
    )
}

/// Stream the exact fingerprint input, with deterministic ordering and framing.
/// The caller supplies the cryptographic hasher and Cargo's change notifications.
pub fn emit_contract(
    manifest_dir: &Path,
    environment: impl IntoIterator<Item = (String, String)>,
    mut update: impl FnMut(&[u8]),
    mut watch: impl FnMut(&Path),
) -> io::Result<()> {
    // Resolve both files together. Unrecognized release metadata retains the
    // original bytes instead of weakening invalidation or breaking a build.
    let (manifest, lockfile) = cargo_contract_inputs(manifest_dir)?;
    let mut inputs = BTreeSet::from([
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
        PathBuf::from("build.rs"),
    ]);
    let mut directories = vec![PathBuf::from("src"), PathBuf::from("build_support")];
    while let Some(directory) = directories.pop() {
        // Watching directories also notices newly added or removed producer
        // files. A presentation edit may rerun build.rs but keeps the digest.
        watch(&directory);
        let entries = fs::read_dir(manifest_dir.join(&directory))
            .map_err(|error| input_error(&directory, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| input_error(&directory, error))?;
            let path = directory.join(entry.file_name());
            if presentation_only(&path) {
                continue;
            }
            let kind = entry
                .file_type()
                .map_err(|error| input_error(&path, error))?;
            if kind.is_dir() {
                directories.push(path);
            } else {
                inputs.insert(path);
            }
        }
    }

    update(b"cass-source-ingest-contract-v3\0");
    update(&(inputs.len() as u64).to_le_bytes());
    for input in inputs {
        watch(&input);
        let name = input.to_string_lossy().replace('\\', "/");
        emit_field(&mut update, name.as_bytes());
        match name.as_str() {
            "Cargo.toml" => emit_field(&mut update, &manifest),
            "Cargo.lock" => emit_field(&mut update, &lockfile),
            _ => {
                let bytes = fs::read(manifest_dir.join(&input))
                    .map_err(|error| input_error(&input, error))?;
                emit_field(&mut update, &bytes);
            }
        }
    }
    let features: BTreeSet<_> = environment
        .into_iter()
        .filter(|(name, _)| name.starts_with("CARGO_FEATURE_"))
        .collect();
    update(&(features.len() as u64).to_le_bytes());
    for (name, value) in features {
        emit_field(&mut update, name.as_bytes());
        emit_field(&mut update, value.as_bytes());
    }
    Ok(())
}

fn emit_field(update: &mut impl FnMut(&[u8]), bytes: &[u8]) {
    update(&(bytes.len() as u64).to_le_bytes());
    update(bytes);
}

fn invalid_cargo_input(path: &str, detail: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid ingest input {path}: {detail}"),
    )
}

fn cargo_contract_inputs(manifest_dir: &Path) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let read = |path: &str| {
        fs::read(manifest_dir.join(path)).map_err(|error| input_error(Path::new(path), error))
    };
    let manifest = read("Cargo.toml")?;
    let lockfile = read("Cargo.lock")?;
    let normalized = std::str::from_utf8(&manifest)
        .ok()
        .zip(std::str::from_utf8(&lockfile).ok())
        .and_then(|(manifest, lockfile)| normalized_cargo_inputs(manifest, lockfile).ok());
    if let Some((manifest, lockfile)) = normalized {
        return Ok((manifest, lockfile));
    }
    // Preserve prior behavior for workspace inheritance, Cargo install source
    // identities, malformed or mismatched pairs, and future Cargo layouts.
    // Version changes still invalidate in this mode. A distinct tag prevents
    // raw bytes from colliding with the structured normalized representation.
    let raw = |bytes: Vec<u8>| {
        let mut tagged = b"raw-cargo-input\0".to_vec();
        tagged.extend_from_slice(&bytes);
        tagged
    };
    Ok((raw(manifest), raw(lockfile)))
}

/// Ignore release numbering, not dependency changes. In particular, a package
/// with the same name from a registry or git source is NOT the root package.
/// Missing, inherited or ambiguous root identities refuse normalization; the
/// caller then hashes ALL original bytes. This does not relax the independent
/// local-path dependency guard in dependency_blocks_reuse.
fn normalized_cargo_inputs(manifest: &str, lockfile: &str) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut manifest: Value =
        toml::from_str(manifest).map_err(|error| invalid_cargo_input("Cargo.toml", error))?;
    let mut lockfile: Value =
        toml::from_str(lockfile).map_err(|error| invalid_cargo_input("Cargo.lock", error))?;
    let package = manifest
        .get_mut("package")
        .and_then(Value::as_table_mut)
        .ok_or_else(|| invalid_cargo_input("Cargo.toml", "missing package table"))?;
    let root_string = |key: &str| {
        package
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                invalid_cargo_input("Cargo.toml", format!("package.{key} must be a string"))
            })
    };
    let name = root_string("name")?;
    let version = root_string("version")?;
    let packages = lockfile
        .get_mut("package")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid_cargo_input("Cargo.lock", "missing package array"))?;
    let mut roots = packages.iter_mut().filter(|entry| {
        entry.get("name").and_then(Value::as_str) == Some(name.as_str())
            && entry.get("version").and_then(Value::as_str) == Some(version.as_str())
            && entry.get("source").is_none()
    });
    let root = roots.next().ok_or_else(|| {
        invalid_cargo_input("Cargo.lock", "no exact source-less root package match")
    })?;
    if roots.next().is_some() {
        return Err(invalid_cargo_input(
            "Cargo.lock",
            "ambiguous source-less root package",
        ));
    }
    root.as_table_mut()
        .ok_or_else(|| invalid_cargo_input("Cargo.lock", "root package must be a table"))?
        .remove("version");
    package.remove("version");

    let encode = |value: &Value| {
        let mut bytes = b"normalized-cargo-input\0".to_vec();
        emit_toml(&mut |part| bytes.extend_from_slice(part), value);
        bytes
    };
    Ok((encode(&manifest), encode(&lockfile)))
}

/// Type- and length-framed TOML semantics. Sort tables explicitly so a future
/// dependency enabling toml's preserve_order feature cannot change the digest.
/// Keep array order and every unknown field; no dependency/version allowlist.
fn emit_toml(update: &mut impl FnMut(&[u8]), value: &Value) {
    match value {
        Value::String(value) => {
            update(b"s");
            emit_field(update, value.as_bytes());
        }
        Value::Integer(value) => {
            update(b"i");
            update(&value.to_le_bytes());
        }
        Value::Float(value) => {
            update(b"f");
            update(&value.to_bits().to_le_bytes());
        }
        Value::Boolean(value) => update(&[b'b', u8::from(*value)]),
        Value::Datetime(value) => {
            update(b"d");
            emit_field(update, value.to_string().as_bytes());
        }
        Value::Array(values) => {
            update(b"a");
            update(&(values.len() as u64).to_le_bytes());
            for value in values {
                emit_toml(update, value);
            }
        }
        Value::Table(values) => {
            update(b"t");
            update(&(values.len() as u64).to_le_bytes());
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
            for (name, value) in entries {
                emit_field(update, name.as_bytes());
                emit_toml(update, value);
            }
        }
    }
}

/// GH#426: the exact root package is the only source-less lock entry allowed.
/// Keep this predicate independent of TOML so its fail-closed behavior can be
/// tested without compiling the application or resolving another dependency graph.
pub fn dependency_blocks_reuse(
    package_name: Option<&str>,
    package_version: Option<&str>,
    package_source: Option<&str>,
    crate_name: &str,
    crate_version: &str,
) -> bool {
    package_source.is_none()
        && !(package_name == Some(crate_name) && package_version == Some(crate_version))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const MANIFEST: &str = r#"[package]
name = "coding-agent-search"
version = "0.10.0"
edition = "2024"

[dependencies]
parser = "1.0.0"
"#;
    const LOCKFILE: &str = r#"version = 4

[[package]]
name = "coding-agent-search"
version = "0.10.0"
dependencies = ["parser"]

[[package]]
name = "parser"
version = "1.0.0"
source = "registry+https://example.invalid/index"
checksum = "original-checksum"
"#;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = loop {
                let path = std::env::temp_dir().join(format!(
                    "cass-ingest-contract-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed),
                ));
                match fs::create_dir(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create contract fixture: {error}"),
                }
            };
            let fixture = Self(path);
            for file in [
                "build.rs",
                "build_support/source_ingest_contract.rs",
                "src/lib.rs",
            ] {
                fixture.write(file, b"original");
            }
            fixture.write("Cargo.toml", MANIFEST.as_bytes());
            fixture.write("Cargo.lock", LOCKFILE.as_bytes());
            fixture
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn stream(&self, environment: &[(&str, &str)]) -> Vec<u8> {
            let mut bytes = Vec::new();
            emit_contract(
                &self.0,
                environment
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string())),
                |part| bytes.extend_from_slice(part),
                |_| {},
            )
            .unwrap();
            bytes
        }
    }

    #[test]
    fn presentation_edits_and_additions_preserve_the_contract() {
        let fixture = Fixture::new();
        let original = fixture.stream(&[]);
        for path in [
            "src/ui/theme.rs",
            "src/html_export/render.rs",
            "src/pages_assets/search.js",
            "src/doctor_robot_docs.rs",
            "src/recipes_robot_docs.rs",
            "src/tui_asciicast.rs",
            "src/ftui_harness.rs",
        ] {
            fixture.write(path, b"original presentation");
            assert_eq!(fixture.stream(&[]), original, "added {path}");
            fixture.write(path, b"changed presentation");
            assert_eq!(fixture.stream(&[]), original, "edited {path}");
        }
    }

    #[test]
    fn producer_and_shared_code_edits_invalidate_the_contract() {
        let fixture = Fixture::new();
        for path in [
            "src/connectors/codex.rs",
            "src/indexer/redact_secrets.rs",
            "src/model/conversation_packet.rs",
            "src/storage/sqlite.rs",
            "src/sources/provenance.rs",
            "src/raw_mirror/store.rs",
            "src/franken_sync.rs",
            "src/search/canonicalize.rs",
            "src/lib.rs",
            "src/connector_ingest_diagnostics.rs",
            "src/future_provider/dictionary.bin",
        ] {
            fixture.write(path, b"original");
            let original = fixture.stream(&[]);
            fixture.write(path, b"changed");
            assert_ne!(fixture.stream(&[]), original, "edited {path}");
        }
    }

    #[test]
    fn producer_addition_and_removal_are_observed() {
        let fixture = Fixture::new();
        let original = fixture.stream(&[]);
        fixture.write("src/new_parser/reader.rs", b"producer");
        assert_ne!(fixture.stream(&[]), original);
        // Move the fixture out of the input tree to model a removed producer.
        fs::rename(
            fixture.0.join("src/new_parser/reader.rs"),
            fixture.0.join("removed-reader.rs"),
        )
        .unwrap();
        assert_eq!(fixture.stream(&[]), original);
        let mut watched = BTreeSet::new();
        emit_contract(
            &fixture.0,
            [],
            |_| {},
            |path| {
                watched.insert(path.to_path_buf());
            },
        )
        .unwrap();
        assert!(watched.contains(Path::new("src")));
        assert!(watched.contains(Path::new("src/new_parser")));
        assert!(watched.contains(Path::new("build_support")));
        assert!(watched.contains(Path::new("Cargo.toml")));
        assert!(watched.contains(Path::new("Cargo.lock")));
    }

    #[test]
    fn build_scripts_and_future_helpers_remain_covered() {
        let fixture = Fixture::new();
        for path in [
            "build.rs",
            "build_support/source_ingest_contract.rs",
            "build_support/future_helper.rs",
        ] {
            fixture.write(path, b"original");
            let original = fixture.stream(&[]);
            fixture.write(path, b"changed");
            assert_ne!(fixture.stream(&[]), original, "edited {path}");
        }
    }

    #[test]
    fn release_version_only_changes_preserve_source_completions() {
        let fixture = Fixture::new();
        let original = fixture.stream(&[]);
        for version in ["0.11.0", "0.11.1", "1.0.0-rc.1"] {
            fixture.write("Cargo.toml", MANIFEST.replace("0.10.0", version).as_bytes());
            fixture.write("Cargo.lock", LOCKFILE.replace("0.10.0", version).as_bytes());
            assert_eq!(fixture.stream(&[]), original, "release {version}");
        }
    }

    #[test]
    fn cargo_dependencies_and_unknown_fields_still_invalidate() {
        for (path, original, before, after) in [
            ("Cargo.toml", MANIFEST, "2024", "2021"),
            ("Cargo.toml", MANIFEST, "1.0.0", "2.0.0"),
            ("Cargo.lock", LOCKFILE, "1.0.0", "2.0.0"),
            ("Cargo.lock", LOCKFILE, "original-checksum", "changed-checksum"),
            (
                "Cargo.lock",
                LOCKFILE,
                "example.invalid/index",
                "other.invalid/index",
            ),
            ("Cargo.lock", LOCKFILE, "registry+", "git+"),
            ("Cargo.lock", LOCKFILE, "[\"parser\"]", "[\"parser\", \"other\"]"),
            ("Cargo.lock", LOCKFILE, "version = 4", "version = 3"),
        ] {
            let fixture = Fixture::new();
            let expected = fixture.stream(&[]);
            fixture.write(path, original.replace(before, after).as_bytes());
            assert_ne!(fixture.stream(&[]), expected, "{path}: {before}");
        }
        for (path, original, extra) in [
            ("Cargo.toml", MANIFEST, "\n[features]\nproducer = []\n"),
            (
                "Cargo.toml",
                MANIFEST,
                "\n[package.metadata]\nfuture_contract = 1\n",
            ),
            ("Cargo.lock", LOCKFILE, "future_identity = 'new'\n"),
        ] {
            let fixture = Fixture::new();
            let expected = fixture.stream(&[]);
            fixture.write(path, format!("{original}{extra}").as_bytes());
            assert_ne!(fixture.stream(&[]), expected, "{path}: {extra}");
        }
        let fixture = Fixture::new();
        let expected = fixture.stream(&[]);
        fixture.write(
            "Cargo.toml",
            MANIFEST.replace("coding-agent-search", "other").as_bytes(),
        );
        fixture.write(
            "Cargo.lock",
            LOCKFILE.replace("coding-agent-search", "other").as_bytes(),
        );
        assert_ne!(fixture.stream(&[]), expected, "root package name");
    }

    #[test]
    fn a_same_name_registry_dependency_keeps_its_version() {
        let fixture = Fixture::new();
        let dependency = r#"
[[package]]
name = "coding-agent-search"
version = "0.10.0"
source = "registry+https://example.invalid/index"
checksum = "same-name-dependency"
"#;
        fixture.write("Cargo.lock", format!("{LOCKFILE}{dependency}").as_bytes());
        let original = fixture.stream(&[]);
        let new_manifest = MANIFEST.replace("0.10.0", "0.11.0");
        let new_lock = LOCKFILE.replace("0.10.0", "0.11.0");
        fixture.write("Cargo.toml", new_manifest.as_bytes());
        fixture.write("Cargo.lock", format!("{new_lock}{dependency}").as_bytes());
        assert_eq!(fixture.stream(&[]), original, "only the root changed");
        let changed_dependency = dependency.replace("0.10.0", "0.11.0");
        fixture.write(
            "Cargo.lock",
            format!("{new_lock}{changed_dependency}").as_bytes(),
        );
        assert_ne!(fixture.stream(&[]), original, "dependency version changed");
    }

    #[test]
    fn unrecognized_cargo_pairs_retain_every_byte_instead_of_blocking_builds() {
        let duplicate =
            format!("{LOCKFILE}\n[[package]]\nname = 'coding-agent-search'\nversion = '0.10.0'\n");
        for (manifest, lockfile) in [
            (MANIFEST.to_string(), LOCKFILE.replace("0.10.0", "0.11.0")),
            (
                MANIFEST.replace("version = \"0.10.0\"", "version.workspace = true"),
                LOCKFILE.to_string(),
            ),
            (
                MANIFEST.replace("name = \"coding-agent-search\"", "name = ''"),
                LOCKFILE.to_string(),
            ),
            (MANIFEST.to_string(), duplicate),
            (MANIFEST.to_string(), "package = []".to_string()),
            ("not valid TOML".to_string(), LOCKFILE.to_string()),
            (MANIFEST.to_string(), "not valid TOML".to_string()),
            (
                MANIFEST.to_string(),
                LOCKFILE.replace(
                    "dependencies =",
                    "source = 'git+https://example.invalid/cass'\ndependencies =",
                ),
            ),
        ] {
            assert!(normalized_cargo_inputs(&manifest, &lockfile).is_err());
            let fixture = Fixture::new();
            fixture.write("Cargo.toml", manifest.as_bytes());
            fixture.write("Cargo.lock", lockfile.as_bytes());
            let (raw_manifest, raw_lockfile) = cargo_contract_inputs(&fixture.0).unwrap();
            assert_eq!(
                raw_manifest,
                [b"raw-cargo-input\0".as_slice(), manifest.as_bytes()].concat()
            );
            assert_eq!(
                raw_lockfile,
                [b"raw-cargo-input\0".as_slice(), lockfile.as_bytes()].concat()
            );
            let original = fixture.stream(&[]);
            fixture.write(
                "Cargo.toml",
                format!("{manifest}\n# retained in raw mode\n").as_bytes(),
            );
            assert_ne!(fixture.stream(&[]), original);
        }
    }

    #[test]
    fn cargo_formatting_and_table_key_order_do_not_change_semantics() {
        let fixture = Fixture::new();
        let original = fixture.stream(&[]);
        fixture.write(
            "Cargo.toml",
            b"# release metadata\n[dependencies]\nparser='1.0.0'\n\
              [package]\nedition='2024'\nversion='0.10.0'\nname='coding-agent-search'\n",
        );
        fixture.write(
            "Cargo.lock",
            format!("# generated lockfile\n{LOCKFILE}").as_bytes(),
        );
        assert_eq!(fixture.stream(&[]), original);
    }

    #[test]
    fn cargo_values_have_unambiguous_types_lengths_and_array_order() {
        let mut streams = BTreeSet::new();
        for value in [
            "'1'",
            "1",
            "1.0",
            "true",
            "1979-05-27T07:32:00Z",
            "['a', 'bc']",
            "['ab', 'c']",
            "['bc', 'a']",
            "{ a = 'bc' }",
            "{ ab = 'c' }",
            "[]",
            "{}",
        ] {
            let fixture = Fixture::new();
            fixture.write(
                "Cargo.toml",
                format!("{MANIFEST}\n[package.metadata]\nvalue = {value}\n").as_bytes(),
            );
            assert!(streams.insert(fixture.stream(&[])), "collision for {value}");
        }
    }

    #[test]
    fn only_cargo_features_affect_the_environment_contract() {
        let fixture = Fixture::new();
        let features = [("CARGO_FEATURE_QR", "1"), ("CARGO_FEATURE_ENCRYPTION", "1")];
        let original = fixture.stream(&features);
        assert_eq!(
            fixture.stream(&[features[1], ("UNRELATED", "value"), features[0]]),
            original
        );
        assert_ne!(fixture.stream(&[features[0]]), original);
        assert_ne!(
            fixture.stream(&[features[0], ("CARGO_FEATURE_ENCRYPTION", "0")]),
            original
        );
        assert_ne!(fixture.stream(&[]), original);
    }

    #[test]
    fn file_order_is_deterministic_and_names_and_contents_are_framed() {
        let left = Fixture::new();
        let right = Fixture::new();
        left.write("src/z", b"z");
        left.write("src/a", b"a");
        right.write("src/a", b"a");
        right.write("src/z", b"z");
        assert_eq!(left.stream(&[]), right.stream(&[]));

        let left = Fixture::new();
        let right = Fixture::new();
        left.write("src/a", b"bc");
        right.write("src/ab", b"c");
        assert_ne!(left.stream(&[]), right.stream(&[]));
    }

    #[test]
    fn presentation_prefix_lookalikes_are_producer_inputs() {
        let fixture = Fixture::new();
        for path in [
            "src/ui_extra/parser.rs",
            "src/html_exporter/parser.rs",
            "src/pages_assets_backup/payload.bin",
            "src/doctor_robot_docs.rs.extra",
            "src/nested/ui/parser.rs",
        ] {
            let original = fixture.stream(&[]);
            fixture.write(path, b"producer");
            assert_ne!(fixture.stream(&[]), original, "added {path}");
        }
    }

    #[test]
    fn local_path_dependencies_still_block_reuse_except_the_exact_root() {
        let blocks = |name, version, source| {
            dependency_blocks_reuse(name, version, source, "coding-agent-search", "0.10.0")
        };
        assert!(!blocks(Some("coding-agent-search"), Some("0.10.0"), None));
        assert!(blocks(Some("coding-agent-search"), Some("0.9.0"), None));
        assert!(blocks(
            Some("franken-agent-detection"),
            Some("0.10.0"),
            None
        ));
        assert!(blocks(None, Some("0.10.0"), None));
        assert!(blocks(Some("coding-agent-search"), None, None));
        assert!(!blocks(
            Some("parser"),
            Some("1.0.0"),
            Some("registry+https://example.invalid")
        ));
        assert!(!blocks(
            Some("parser"),
            Some("1.0.0"),
            Some("git+https://example.invalid#revision")
        ));
    }

    #[test]
    fn missing_mandatory_input_cannot_certify_a_contract() {
        let fixture = Fixture::new();
        fs::rename(fixture.0.join("Cargo.lock"), fixture.0.join("saved.lock")).unwrap();
        let error = emit_contract(&fixture.0, [], |_| {}, |_| {}).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("Cargo.lock"));
    }
}
