//! Build inputs that can invalidate durable source completions (ayhvu).
//!
//! This deliberately excludes only audited presentation modules. Shared code,
//! unknown source paths, the complete manifest/lockfile, and all Cargo features
//! remain inputs. In particular, release-version bumps and help edits in lib.rs
//! still invalidate completions. Audit producer dependencies before extending
//! the exclusions: the semantic replay cache also uses this contract.
//!
//! Standalone tests need no dependencies: `rustc --edition=2024 --test
//! build_support/source_ingest_contract.rs -o /tmp/cass-ingest-contract-tests`.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

    update(b"cass-source-ingest-contract-v2\0");
    update(&(inputs.len() as u64).to_le_bytes());
    for input in inputs {
        watch(&input);
        let name = input.to_string_lossy().replace('\\', "/");
        let bytes =
            fs::read(manifest_dir.join(&input)).map_err(|error| input_error(&input, error))?;
        emit_field(&mut update, name.as_bytes());
        emit_field(&mut update, &bytes);
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
                "Cargo.toml",
                "Cargo.lock",
                "build.rs",
                "build_support/source_ingest_contract.rs",
                "src/lib.rs",
            ] {
                fixture.write(file, b"original");
            }
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
    }

    #[test]
    fn complete_build_inputs_and_future_helpers_remain_covered() {
        let fixture = Fixture::new();
        for path in [
            "Cargo.toml",
            "Cargo.lock",
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
