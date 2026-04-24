use clap::{Parser, error::ErrorKind};
use coding_agent_search::{Cli, Commands, ModelsCommand};

fn parse(args: &[&str]) -> Result<Cli, String> {
    Cli::try_parse_from(args).map_err(|err| format!("parse cass CLI for {args:?}: {err}"))
}

fn run_on_large_stack<F>(f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("cli-model-lifecycle-contract".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .map_err(|err| format!("spawn large-stack CLI parser test: {err}"))?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("large-stack CLI parser test panicked".to_string()),
    }
}

#[test]
fn models_install_from_file_keeps_acquisition_data_dir_scoped() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "install",
            "--model",
            "all-minilm-l6-v2",
            "--from-file",
            "/seeded/models/all-minilm-l6-v2",
            "--data-dir",
            "/cass/models",
            "--yes",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Install {
                model,
                mirror: None,
                from_file: Some(from_file),
                yes: true,
                data_dir: Some(data_dir),
            })) if model == "all-minilm-l6-v2"
                && from_file.display().to_string() == "/seeded/models/all-minilm-l6-v2"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected local model acquisition controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn models_install_from_mirror_keeps_acquisition_data_dir_scoped() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "install",
            "--model",
            "all-minilm-l6-v2",
            "--mirror",
            "https://mirror.example/models",
            "--data-dir",
            "/cass/models",
            "--yes",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Install {
                model,
                mirror: Some(mirror),
                from_file: None,
                yes: true,
                data_dir: Some(data_dir),
            })) if model == "all-minilm-l6-v2"
                && mirror == "https://mirror.example/models"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected mirror model acquisition controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn models_install_rejects_ambiguous_mirror_and_from_file_sources() -> Result<(), String> {
    run_on_large_stack(|| {
        let result = Cli::try_parse_from([
            "cass",
            "models",
            "install",
            "--mirror",
            "https://mirror.example/models",
            "--from-file",
            "/seeded/models/all-minilm-l6-v2",
            "--data-dir",
            "/cass/models",
            "--yes",
        ]);

        match result {
            Err(err) if err.kind() == ErrorKind::ArgumentConflict => Ok(()),
            Err(err) => Err(format!(
                "expected mirror/from-file acquisition conflict, got {:?}: {err}",
                err.kind()
            )),
            Ok(cli) => Err(format!(
                "expected mirror/from-file acquisition conflict, parsed: {cli:?}"
            )),
        }
    })
}

#[test]
fn models_install_defaults_to_standard_model_with_confirmation() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&["cass", "models", "install", "--data-dir", "/cass/models"])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Install {
                model,
                mirror: None,
                from_file: None,
                yes: false,
                data_dir: Some(data_dir),
            })) if model == "all-minilm-l6-v2"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected default model acquisition controls to stay confirmation-gated: {other:?}"
            )),
        }
    })
}

#[test]
fn models_verify_repair_controls_remain_data_dir_scoped() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "verify",
            "--repair",
            "--data-dir",
            "/cass/models",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Verify {
                repair: true,
                data_dir: Some(data_dir),
                json: true,
            })) if data_dir.display().to_string() == "/cass/models" => Ok(()),
            other => Err(format!(
                "expected data-dir scoped model verify repair controls: {other:?}"
            )),
        }
    })
}

#[test]
fn models_verify_defaults_to_inspection_without_repair() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "verify",
            "--data-dir",
            "/cass/models",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Verify {
                repair: false,
                data_dir: Some(data_dir),
                json: true,
            })) if data_dir.display().to_string() == "/cass/models" => Ok(()),
            other => Err(format!(
                "expected model verification to default to inspect-only mode: {other:?}"
            )),
        }
    })
}

#[test]
fn models_verify_json_missing_cache_stays_fail_open_and_read_only() {
    use assert_cmd::cargo::cargo_bin;
    use serde_json::Value;
    use std::path::Path;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("cass-data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    let output = std::process::Command::new(cargo_bin("cass"))
        .args(["models", "verify", "--json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .output()
        .expect("run cass models verify --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "models verify --json should succeed with a fail-open lifecycle payload; stdout: {stdout}\nstderr: {stderr}"
    );

    let payload: Value = serde_json::from_slice(&output.stdout).expect("models verify emits JSON");
    assert_eq!(payload["status"].as_str(), Some("not_acquired"));
    assert_eq!(payload["lexical_fail_open"].as_bool(), Some(true));
    assert_eq!(payload["all_valid"].as_bool(), Some(false));
    assert_eq!(
        payload["error"].as_str(),
        Some("model directory does not exist")
    );

    let model_dir = payload["model_dir"]
        .as_str()
        .expect("models verify must expose model_dir");
    assert!(
        Path::new(model_dir).starts_with(&data_dir),
        "model_dir must stay under the requested data_dir; got {model_dir}"
    );
    assert!(
        !Path::new(model_dir).exists(),
        "verify --json must inspect an absent cache without creating model_dir; got {model_dir}"
    );

    let lifecycle = &payload["cache_lifecycle"];
    assert_eq!(lifecycle["model_dir"].as_str(), Some(model_dir));
    assert_eq!(lifecycle["state"]["state"].as_str(), Some("not_acquired"));
    assert_eq!(lifecycle["installed_size_bytes"].as_u64(), Some(0));
}

#[test]
fn models_remove_requires_explicit_model_and_yes_controls() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "remove",
            "--model",
            "all-minilm-l6-v2",
            "--data-dir",
            "/cass/models",
            "--yes",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Remove {
                model,
                yes: true,
                data_dir: Some(data_dir),
            })) if model == "all-minilm-l6-v2"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected explicit model removal controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn models_remove_defaults_to_interactive_reclamation() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&["cass", "models", "remove", "--data-dir", "/cass/models"])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Remove {
                model,
                yes: false,
                data_dir: Some(data_dir),
            })) if model == "all-minilm-l6-v2"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected model removal to default to interactive reclamation: {other:?}"
            )),
        }
    })
}

#[test]
fn models_check_update_reports_against_scoped_data_dir() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "check-update",
            "--data-dir",
            "/cass/models",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::CheckUpdate {
                data_dir: Some(data_dir),
                json: true,
            })) if data_dir.display().to_string() == "/cass/models" => Ok(()),
            other => Err(format!(
                "expected scoped model update check controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn models_backfill_keeps_semantic_work_data_dir_and_db_scoped() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "backfill",
            "--tier",
            "quality",
            "--embedder",
            "fastembed",
            "--batch-conversations",
            "17",
            "--scheduled",
            "--data-dir",
            "/cass/data",
            "--db",
            "/cass/data/agent_search.db",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Backfill {
                tier,
                embedder: Some(embedder),
                batch_conversations: 17,
                scheduled: true,
                data_dir: Some(data_dir),
                db: Some(db),
                json: true,
            })) if tier == "quality"
                && embedder == "fastembed"
                && data_dir.display().to_string() == "/cass/data"
                && db.display().to_string() == "/cass/data/agent_search.db" =>
            {
                Ok(())
            }
            other => Err(format!(
                "expected scoped model backfill controls to parse: {other:?}"
            )),
        }
    })
}

// ========================================================================
// Bead coding_agent_session_search-7hot1 (child of ibuuh.10, scenario G:
// semantic model acquisition — air-gap entry point, error-envelope E2E).
//
// AGENTS.md's Search Asset Contract promises: "Semantic model acquisition
// is opt-in... Air-gapped installs use `--from-file <dir>`." The existing
// arg-parse contract tests above pin the CLI shape, but no test pins the
// RUNTIME validation error surface emitted by src/lib.rs::run_models_install
// at line ~26760. Three distinct validation errors can fire there —
// not-a-directory, missing required file, and mirror/from-file conflict —
// and all three are silently untested at the user-visible stderr boundary.
//
// A regression that changed err.kind from "model" to something else,
// dropped the hint pointing operators at the expected file set, or
// silently accepted a partial file set would not be caught by the
// arg-parse tests above. This gap matters especially for agents
// running air-gapped installs: they consume the JSON error envelope
// to decide what to do next.
//
// Contract pinned here, for the two cases that don't need the real
// ~90MB MiniLM model (which this test fixture deliberately avoids):
//   1. --from-file <nonexistent-path>
//      - exit 21, err.kind == "model", err.code == 21,
//        err.retryable == false, hint names "directory" + expected
//        model file examples.
//   2. --from-file <empty-dir>
//      - exit 21, err.kind == "model", err.code == 21,
//        err.retryable == false, hint enumerates the required file set
//        (model.onnx, tokenizer.json, config.json, ...).
// ========================================================================

#[test]
fn models_install_from_file_nonexistent_path_emits_model_error_envelope() {
    use assert_cmd::cargo::cargo_bin;
    use serde_json::Value;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("cass-data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let nonexistent = tmp.path().join("definitely-not-there");
    assert!(
        !nonexistent.exists(),
        "precondition: target path must not exist"
    );

    let output = std::process::Command::new(cargo_bin("cass"))
        .args(["models", "install", "--from-file"])
        .arg(&nonexistent)
        .args(["--yes", "--robot-format", "json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .output()
        .expect("run cass models install");

    let exit = output.status.code().expect("exit code present");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        exit, 21,
        "--from-file pointing at a nonexistent path must exit 21 (model kind); \
         stdout: {stdout}\nstderr: {stderr}"
    );

    // Find the JSON error envelope — may be on stderr or stdout.
    let envelope_line = stderr
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .or_else(|| {
            stdout
                .lines()
                .rev()
                .find(|l| l.trim_start().starts_with('{'))
        })
        .unwrap_or_else(|| {
            panic!(
                "expected JSON error envelope on stderr or stdout; \
                 stdout: {stdout}\nstderr: {stderr}"
            )
        });
    let envelope: Value = serde_json::from_str(envelope_line.trim()).unwrap_or_else(|err| {
        panic!("JSON parse of error envelope failed: {err}; line: {envelope_line}")
    });
    let err_obj = envelope
        .get("error")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("error envelope must have `error` object; got: {envelope}"));

    assert_eq!(
        err_obj.get("kind").and_then(Value::as_str),
        Some("model"),
        "err.kind must be 'model' for --from-file validation failures; got: {err_obj:?}"
    );
    assert_eq!(
        err_obj.get("code").and_then(Value::as_i64),
        Some(21),
        "err.code must be 21 (model acquisition) and match exit code; got: {err_obj:?}"
    );
    assert_eq!(
        err_obj.get("retryable").and_then(Value::as_bool),
        Some(false),
        "err.retryable must be false for invalid-path validation; got: {err_obj:?}"
    );

    let message = err_obj
        .get("message")
        .and_then(Value::as_str)
        .expect("message must be a string");
    assert!(
        message.contains("is not a directory"),
        "message must name the not-a-directory condition so operators can diagnose; \
         got: {message:?}"
    );

    let hint = err_obj
        .get("hint")
        .and_then(Value::as_str)
        .expect("hint must be a string");
    assert!(
        hint.contains("directory"),
        "hint must guide the operator toward providing a directory; got: {hint:?}"
    );
}

#[test]
fn models_install_from_file_empty_dir_emits_required_file_error() {
    use assert_cmd::cargo::cargo_bin;
    use serde_json::Value;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("cass-data");
    let model_src = tmp.path().join("empty-model-source");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    std::fs::create_dir_all(&model_src).expect("create empty model source dir");

    let output = std::process::Command::new(cargo_bin("cass"))
        .args(["models", "install", "--from-file"])
        .arg(&model_src)
        .args(["--yes", "--robot-format", "json", "--data-dir"])
        .arg(&data_dir)
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("HOME", tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .output()
        .expect("run cass models install");

    let exit = output.status.code().expect("exit code present");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        exit, 21,
        "--from-file pointing at an empty dir must exit 21 (model kind); \
         stdout: {stdout}\nstderr: {stderr}"
    );

    let envelope_line = stderr
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .or_else(|| {
            stdout
                .lines()
                .rev()
                .find(|l| l.trim_start().starts_with('{'))
        })
        .unwrap_or_else(|| {
            panic!(
                "expected JSON error envelope on stderr or stdout; \
                 stdout: {stdout}\nstderr: {stderr}"
            )
        });
    let envelope: Value = serde_json::from_str(envelope_line.trim())
        .unwrap_or_else(|err| panic!("JSON parse failed: {err}; line: {envelope_line}"));
    let err_obj = envelope
        .get("error")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("error envelope must have `error` object; got: {envelope}"));

    assert_eq!(
        err_obj.get("kind").and_then(Value::as_str),
        Some("model"),
        "err.kind must be 'model'; got: {err_obj:?}"
    );

    let message = err_obj
        .get("message")
        .and_then(Value::as_str)
        .expect("message must be a string");
    assert!(
        message.contains("Required file") && message.contains("not found"),
        "message must name the 'Required file ... not found' condition; got: {message:?}"
    );

    let hint = err_obj
        .get("hint")
        .and_then(Value::as_str)
        .expect("hint must be a string");
    // Hint must enumerate the expected file set so operators can
    // assemble the air-gap bundle correctly. At minimum it names
    // model.onnx and tokenizer.json.
    assert!(
        hint.contains("model.onnx"),
        "hint must name the required model.onnx file; got: {hint:?}"
    );
    assert!(
        hint.contains("tokenizer.json"),
        "hint must name the required tokenizer.json file; got: {hint:?}"
    );
}
