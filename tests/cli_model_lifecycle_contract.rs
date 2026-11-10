use clap::{Parser, error::ErrorKind};
use coding_agent_search::search::policy::MAX_MODEL_SIZE_MB;
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
                dry_run: false,
                json: false,
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
                dry_run: false,
                json: false,
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
                dry_run: false,
                json: false,
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
fn models_install_dry_run_keeps_preview_structured_and_data_dir_scoped() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "install",
            "--model",
            "minilm",
            "--dry-run",
            "--json",
            "--data-dir",
            "/cass/preview",
        ])?;
        match cli.command {
            Some(Commands::Models(ModelsCommand::Install {
                model,
                mirror: None,
                from_file: None,
                yes: false,
                dry_run: true,
                json: true,
                data_dir: Some(data_dir),
            })) if model == "minilm" && data_dir.display().to_string() == "/cass/preview" => Ok(()),
            other => Err(format!("expected a scoped JSON preview, got {other:?}")),
        }
    })
}

#[test]
fn models_install_json_requires_dry_run() -> Result<(), String> {
    run_on_large_stack(
        || match Cli::try_parse_from(["cass", "models", "install", "--json"]) {
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument => Ok(()),
            other => Err(format!(
                "expected --json to require --dry-run, got {other:?}"
            )),
        },
    )
}

#[test]
fn models_verify_repair_controls_remain_data_dir_scoped() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "models",
            "verify",
            "--model",
            "multilingual-minilm",
            "--repair",
            "--data-dir",
            "/cass/models",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::Verify {
                model,
                repair: true,
                data_dir: Some(data_dir),
                json: true,
            })) if model == "multilingual-minilm"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
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
                model,
                repair: false,
                data_dir: Some(data_dir),
                json: true,
            })) if model == "all-minilm-l6-v2"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
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
        .env("CASS_DATA_DIR", &data_dir)
        .env("FRANKENSEARCH_MODEL_DIR", "")
        .env("CASS_SEMANTIC_MODE", "hybrid_preferred")
        .env("CASS_SEMANTIC_DOWNLOAD_POLICY", "opt_in")
        .env("CASS_SEMANTIC_EMBEDDER", "minilm")
        .env(
            "CASS_SEMANTIC_MAX_MODEL_SIZE_MB",
            MAX_MODEL_SIZE_MB.to_string(),
        )
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
fn explicit_model_repair_and_install_reach_the_shared_lease_in_robot_mode() {
    use assert_cmd::cargo::cargo_bin;
    use coding_agent_search::search::model_download::ModelManifest;
    use serde_json::Value;
    use std::time::Duration;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("cass-data");
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let run = |args: &[&str]| {
        let mut command = assert_cmd::Command::new(cargo_bin("cass"));
        command
            .args(args)
            .arg("--data-dir")
            .arg(&data_dir)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CASS_DATA_DIR", &data_dir)
            .env("FRANKENSEARCH_MODEL_DIR", "")
            .env("CASS_SEMANTIC_MODE", "hybrid_preferred")
            .env("CASS_SEMANTIC_DOWNLOAD_POLICY", "opt_in")
            .env("CASS_SEMANTIC_EMBEDDER", "minilm")
            .env(
                "CASS_SEMANTIC_MAX_MODEL_SIZE_MB",
                MAX_MODEL_SIZE_MB.to_string(),
            )
            .env("HOME", tmp.path())
            .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
            .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
            .timeout(Duration::from_secs(10));
        command.output().expect("run bounded model command")
    };

    let absent = run(&["models", "verify", "--model", "minilm", "--json"]);
    assert!(absent.status.success());
    let absent: Value = serde_json::from_slice(&absent.stdout).expect("absent-cache JSON");
    let model_dir =
        std::path::PathBuf::from(absent["model_dir"].as_str().expect("model directory"));
    assert!(model_dir.starts_with(&data_dir));
    assert!(
        !model_dir.exists(),
        "inspection must leave the cache absent"
    );
    std::fs::create_dir_all(model_dir.parent().unwrap()).expect("create install lease parent");
    let lock_name = format!(
        ".{}.install.lock",
        model_dir.file_name().unwrap().to_str().unwrap()
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(model_dir.with_file_name(lock_name))
        .expect("open real shared install lease");
    fs2::FileExt::lock_exclusive(&lock).expect("hold install lease");

    let assert_mutation_is_blocked = |args: &[&str]| {
        let output = run(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "unexpected no-op success: {stdout}; {stderr}"
        );
        assert!(
            stdout.contains("another model installation is in progress")
                || stderr.contains("another model installation is in progress"),
            "expected actual lease refusal: {stdout}; {stderr}"
        );
    };
    // Repair must reach installation even when the entire cache is absent.
    // The held sibling lease refuses admission before any network access.
    assert_mutation_is_blocked(&[
        "models", "verify", "--model", "minilm", "--repair", "--json",
    ]);
    assert!(
        !model_dir.exists(),
        "refusal must not create the live cache"
    );

    let manifest = ModelManifest::for_embedder("minilm").expect("registered native manifest");
    std::fs::create_dir_all(&model_dir).expect("create model directory");
    for file in &manifest.files {
        std::fs::write(model_dir.join(file.local_name()), b"corrupt payload")
            .expect("write actual invalid payload");
    }
    let marker = format!("revision={}\nsource=registry\n", manifest.revision);
    std::fs::write(model_dir.join(".verified"), &marker).expect("write marker");

    // Ordinary verification remains read-only and does not require the lease.
    let inspected = run(&["models", "verify", "--model", "minilm", "--json"]);
    assert!(inspected.status.success());
    let inspected: Value = serde_json::from_slice(&inspected.stdout).expect("inspection JSON");
    assert_eq!(inspected["all_valid"], false);
    assert_eq!(inspected["status"], "checksum_mismatch");

    // Both explicit mutations must reach the actual shared lease BEFORE any
    // network access. The old JSON-repair and presence-only-install paths
    // incorrectly returned success without attempting that admission.
    let attempts: &[&[&str]] = &[
        &[
            "models", "verify", "--model", "minilm", "--repair", "--json",
        ],
        &[
            "models",
            "install",
            "--model",
            "minilm",
            "--yes",
            "--robot-format",
            "json",
        ],
    ];
    for args in attempts {
        assert_mutation_is_blocked(args);
        assert_eq!(
            std::fs::read_to_string(model_dir.join(".verified")).unwrap(),
            marker
        );
        for file in &manifest.files {
            assert_eq!(
                std::fs::read(model_dir.join(file.local_name())).unwrap(),
                b"corrupt payload"
            );
        }
    }
}

#[test]
fn models_status_json_reports_explicit_multilingual_space_without_acquiring_it() {
    use assert_cmd::cargo::cargo_bin;
    use serde_json::Value;

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("cass-data");
    let output = std::process::Command::new(cargo_bin("cass"))
        .args(["models", "status", "--json"])
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_DATA_DIR", &data_dir)
        .env("FRANKENSEARCH_MODEL_DIR", "")
        .env("CASS_SEMANTIC_MODE", "hybrid_preferred")
        .env("CASS_SEMANTIC_DOWNLOAD_POLICY", "opt_in")
        .env("CASS_SEMANTIC_EMBEDDER", "multilingual-minilm")
        .env(
            "CASS_SEMANTIC_MAX_MODEL_SIZE_MB",
            MAX_MODEL_SIZE_MB.to_string(),
        )
        .env("HOME", tmp.path())
        .env("XDG_DATA_HOME", tmp.path().join(".local/share"))
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .output()
        .expect("run cass models status --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "multilingual status must fail open without downloading; stdout: {stdout}\nstderr: {stderr}"
    );

    let payload: Value = serde_json::from_slice(&output.stdout).expect("models status emits JSON");
    assert_eq!(
        payload["policy_quality_tier_embedder"].as_str(),
        Some("multilingual-minilm")
    );
    assert_eq!(
        payload["active_registry_name"].as_str(),
        Some("multilingual-minilm")
    );
    assert_eq!(
        payload["model_id"].as_str(),
        Some("paraphrase-multilingual-minilm-l12-v2")
    );
    assert_eq!(payload["installed"].as_bool(), Some(false));
    assert_eq!(payload["state"].as_str(), Some("not_acquired"));
    assert_eq!(payload["lexical_fail_open"].as_bool(), Some(true));

    let models = payload["models"].as_array().expect("models array");
    assert_eq!(models.len(), 2);
    let baseline = models
        .iter()
        .find(|model| model["registry_name"] == "minilm")
        .expect("baseline model status");
    let multilingual = models
        .iter()
        .find(|model| model["registry_name"] == "multilingual-minilm")
        .expect("multilingual model status");
    assert_eq!(baseline["active"].as_bool(), Some(false));
    assert_eq!(multilingual["active"].as_bool(), Some(true));
    assert_ne!(baseline["model_dir"], multilingual["model_dir"]);
    for (status, model_name) in [
        (&payload, "multilingual-minilm"),
        (multilingual, "multilingual-minilm"),
        (baseline, "minilm"),
    ] {
        let model_dir = status["model_dir"].as_str().expect("model directory");
        assert!(
            std::path::Path::new(model_dir).starts_with(&data_dir),
            "model cache must stay under the isolated data directory: {model_dir}"
        );
        assert!(
            !std::path::Path::new(model_dir).exists(),
            "status must leave the isolated model cache absent: {model_dir}"
        );
        let advice = status["next_step"].as_str().expect("model-specific advice");
        let command = advice.split('`').nth(1).expect("explicit install command");
        let cli = <Cli as clap::Parser>::try_parse_from(command.split_whitespace())
            .expect("accepted install syntax");
        assert!(matches!(
            cli.command,
            Some(Commands::Models(ModelsCommand::Install { model, .. })) if model == model_name
        ));
    }
}

#[test]
fn external_embedding_discovery_is_complete_private_and_passive() {
    use coding_agent_search::search::external_embedder::ExternalEmbeddingConfig;
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};
    use std::net::TcpListener;
    use std::time::Duration;

    let home = tempfile::tempdir().expect("isolated home");
    let data_dir = home.path().join("data");
    let db = home.path().join("archive.db");
    let endpoint = TcpListener::bind("127.0.0.1:0").expect("synthetic endpoint");
    endpoint
        .set_nonblocking(true)
        .expect("nonblocking endpoint");
    let url = format!("http://{}/v1/embeddings", endpoint.local_addr().unwrap());
    let model = "synthetic-private-discovery-model";
    let key = "synthetic-private-discovery-key-not-a-credential";
    let config = BTreeMap::from([
        ("CASS_EXTERNAL_EMBEDDINGS", "1"),
        ("CASS_EXTERNAL_EMBEDDING_URL", url.as_str()),
        ("CASS_EXTERNAL_EMBEDDING_MODEL", model),
        ("CASS_EXTERNAL_EMBEDDING_DIMENSION", "384"),
        ("CASS_EXTERNAL_EMBEDDING_API_KEY", key),
    ]);
    // Compare discovery with the actual provider parser's consumed settings,
    // rather than another independently maintained list of variable names.
    let mut consumed = BTreeSet::new();
    ExternalEmbeddingConfig::from_lookup(|name| {
        consumed.insert(name.to_owned());
        config.get(name).map(|value| (*value).to_owned())
    })
    .expect("valid external configuration")
    .expect("explicit consent");

    let run = |args: &[&str]| {
        let mut command = assert_cmd::Command::new(assert_cmd::cargo::cargo_bin!("cass"));
        command
            .env_clear()
            .envs(std::env::vars_os().filter(|(name, _)| {
                matches!(
                    name.to_str(),
                    Some(
                        "PATH"
                            | "SystemRoot"
                            | "WINDIR"
                            | "TMP"
                            | "TEMP"
                            | "TMPDIR"
                            | "LD_LIBRARY_PATH"
                            | "DYLD_LIBRARY_PATH"
                    )
                )
            }))
            .envs(config.iter())
            .env("HOME", home.path())
            .env("USERPROFILE", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_DATA_HOME", home.path().join("xdg-data"))
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .env("CASS_DATA_DIR", &data_dir)
            .env("CASS_DB_PATH", &db)
            .env("CASS_SEMANTIC_EMBEDDER", "external")
            .env("CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS", "100")
            .env("CASS_IGNORE_SOURCES_CONFIG", "1")
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .env("NO_COLOR", "1")
            .current_dir(home.path())
            .timeout(Duration::from_secs(20))
            .args(args);
        let output = command.assert().success().get_output().clone();
        let emitted = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for private in [url.as_str(), model, key] {
            assert!(
                !emitted.contains(private),
                "discovery exposed configuration"
            );
        }
        output.stdout
    };

    let capabilities: Value =
        serde_json::from_slice(&run(&["capabilities", "--json"])).expect("capabilities JSON");
    let entries = capabilities["env_vars"].as_array().expect("env registry");
    let registry: BTreeMap<_, _> = entries
        .iter()
        .map(|entry| (entry["name"].as_str().expect("setting name"), entry))
        .collect();
    assert_eq!(registry.len(), entries.len(), "duplicate env setting");
    let docs = String::from_utf8(run(&["robot-docs", "env"])).expect("robot docs");
    for name in consumed {
        assert!(registry.contains_key(name.as_str()), "missing {name}");
        assert!(docs.contains(&name), "robot docs omitted {name}");
    }
    for (name, default) in [
        ("CASS_EXTERNAL_EMBEDDINGS", "0"),
        ("CASS_EXTERNAL_EMBEDDING_REVISION", "1"),
        ("CASS_EXTERNAL_EMBEDDING_BATCH_SIZE", "64"),
        ("CASS_EXTERNAL_EMBEDDING_MAX_REQUEST_BYTES", "262144"),
        ("CASS_EXTERNAL_EMBEDDING_TIMEOUT_MS", "30000"),
        ("CASS_EXTERNAL_EMBEDDING_MAX_RETRIES", "0"),
    ] {
        assert_eq!(registry[name]["default"], default, "default for {name}");
    }
    for name in [
        "CASS_EXTERNAL_EMBEDDING_URL",
        "CASS_EXTERNAL_EMBEDDING_MODEL",
        "CASS_EXTERNAL_EMBEDDING_DIMENSION",
        "CASS_EXTERNAL_EMBEDDING_API_KEY",
    ] {
        assert!(
            registry[name].get("default").is_none(),
            "default for {name}"
        );
    }
    assert!(docs.contains("hash|minilm|multilingual-minilm|external"));
    let doctor: Value = serde_json::from_slice(&run(&["doctor", "--emit-capabilities", "--json"]))
        .expect("doctor capabilities JSON");
    let selection = doctor["env_vars"]
        .as_array()
        .expect("doctor env settings")
        .iter()
        .find(|entry| entry["name"] == "CASS_SEMANTIC_EMBEDDER")
        .expect("doctor embedder selection");
    assert!(selection["purpose"].as_str().unwrap().contains("external"));
    assert!(
        matches!(endpoint.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(!db.exists(), "discovery created an archive");
    assert!(
        !data_dir.join("models").exists(),
        "discovery acquired models"
    );
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
            "--model",
            "multilingual-minilm",
            "--data-dir",
            "/cass/models",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Models(ModelsCommand::CheckUpdate {
                model,
                data_dir: Some(data_dir),
                json: true,
            })) if model == "multilingual-minilm"
                && data_dir.display().to_string() == "/cass/models" =>
            {
                Ok(())
            }
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
            "--max-batches",
            "3",
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
                max_batches: 3,
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
//        (model.safetensors, tokenizer.json, config.json, ...).
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
    // model.safetensors and tokenizer.json.
    assert!(
        hint.contains("model.safetensors"),
        "hint must name the required model.safetensors file; got: {hint:?}"
    );
    assert!(
        hint.contains("tokenizer.json"),
        "hint must name the required tokenizer.json file; got: {hint:?}"
    );
}
