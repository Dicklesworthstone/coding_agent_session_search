use clap::Parser;
use coding_agent_search::{
    Cli, Commands, RobotFormat,
    doctor::{DoctorBackupCommand, DoctorCommandRequest, DoctorCommandSurface},
    raw_mirror::{
        RawMirrorCaptureInput, RawMirrorDbLink, RawMirrorPruneOptions, capture_source_file,
        merge_manifest_db_links, prune, storage_summary,
    },
};

fn parse(args: &[&str]) -> Result<Cli, String> {
    Cli::try_parse_from(args).map_err(|err| format!("parse cass CLI for {args:?}: {err}"))
}

#[test]
fn gh461_mirror_prune_cli_composes_provider_and_path_selectors() {
    let temp = tempfile::tempdir().expect("fixture");
    let data_dir = temp.path().join("data");
    let capture = |provider, name, bytes: &[u8]| {
        let source_path = temp.path().join(name);
        std::fs::write(&source_path, bytes).expect("source");
        capture_source_file(RawMirrorCaptureInput {
            data_dir: &data_dir,
            provider,
            source_id: "local",
            origin_kind: "local",
            origin_host: None,
            source_path: &source_path,
            db_links: &[],
        })
        .expect("capture")
    };
    let selected = capture("opencode", "target.db", b"selected");
    let other_path = capture("opencode", "other.db", b"other path");
    let other_provider = capture("codex", "codex.jsonl", b"other provider");
    let run = |extra: &[&str]| {
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_cass"));
        command
            .args(["mirror", "prune", "--data-dir"])
            .arg(&data_dir)
            .args(["--older-than", "0s", "--safety-hold-down", "0s", "--json"])
            .args(extra)
            .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
            .timeout(std::time::Duration::from_secs(30));
        command.assert()
    };
    let result = run(&["--provider", "opencode", "--source-path", "*target.db"]).success();
    let report: serde_json::Value =
        serde_json::from_slice(&result.get_output().stdout).expect("prune JSON");
    assert_eq!(
        report["prune"]["providers"],
        serde_json::json!(["opencode"])
    );
    assert_eq!(report["prune"]["source_path"], "*target.db");
    assert_eq!(
        report["prune"]["scope_blob_bytes"],
        selected.source_size_bytes
    );
    let entries = report["prune"]["entries"].as_array().expect("entries");
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == selected.manifest_relative_path)
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["path"] == other_path.manifest_relative_path)
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["path"] == other_provider.manifest_relative_path)
    );

    let result = run(&["--provider", "opencode", "--provider", "codex"]).success();
    let report: serde_json::Value =
        serde_json::from_slice(&result.get_output().stdout).expect("union JSON");
    assert_eq!(report["prune"]["planned_manifest_count"], 3);
    let result = run(&["--provider", "absent"]).success();
    let report: serde_json::Value =
        serde_json::from_slice(&result.get_output().stdout).expect("no matches JSON");
    assert_eq!(report["prune"]["planned_manifest_count"], 0);
    run(&["--source-path", "[invalid"]).code(2);
    run(&["--source-path", " "]).code(2);
    run(&["--provider", " "]).code(2);
    for capture in [selected, other_path, other_provider] {
        assert!(
            data_dir
                .join("raw-mirror/v1")
                .join(capture.manifest_relative_path)
                .exists()
        );
    }
}

fn run_on_large_stack<F>(f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    let handle = std::thread::Builder::new()
        .name("cli-refresh-contract".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .map_err(|err| format!("spawn large-stack CLI parser test: {err}"))?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("large-stack CLI parser test panicked".to_string()),
    }
}

#[test]
fn search_refresh_and_catch_up_alias_enable_incremental_preflight() -> Result<(), String> {
    run_on_large_stack(|| {
        for args in [
            ["cass", "search", "needle", "--refresh"],
            ["cass", "search", "needle", "--catch-up"],
        ] {
            let cli = parse(&args)?;
            match cli.command {
                Some(Commands::Search { refresh: true, .. }) => {}
                Some(Commands::Search { .. }) => {
                    return Err(format!("search should enable refresh for args {args:?}"));
                }
                other => {
                    return Err(format!(
                        "expected search command for args {args:?}: {other:?}"
                    ));
                }
            }
        }
        Ok(())
    })
}

#[test]
fn tui_refresh_and_catch_up_alias_enable_incremental_preflight() -> Result<(), String> {
    run_on_large_stack(|| {
        for args in [
            ["cass", "tui", "--once", "--refresh"],
            ["cass", "tui", "--once", "--catch-up"],
        ] {
            let cli = parse(&args)?;
            match cli.command {
                Some(Commands::Tui { refresh: true, .. }) => {}
                Some(Commands::Tui { .. }) => {
                    return Err(format!("tui should enable refresh for args {args:?}"));
                }
                other => return Err(format!("expected tui command for args {args:?}: {other:?}")),
            }
        }
        Ok(())
    })
}

#[test]
fn refresh_preflight_stays_opt_in_for_search_and_tui() -> Result<(), String> {
    run_on_large_stack(|| {
        let search = parse(&["cass", "search", "needle"])?;
        match search.command {
            Some(Commands::Search { refresh: false, .. }) => {}
            Some(Commands::Search { .. }) => {
                return Err("search refresh must stay opt-in".to_string());
            }
            other => return Err(format!("expected search command: {other:?}")),
        }

        let tui = parse(&["cass", "tui", "--once"])?;
        match tui.command {
            Some(Commands::Tui { refresh: false, .. }) => {}
            Some(Commands::Tui { .. }) => return Err("tui refresh must stay opt-in".to_string()),
            other => return Err(format!("expected tui command: {other:?}")),
        }
        Ok(())
    })
}

#[test]
fn refresh_preflight_remains_scoped_to_requested_data_dir() -> Result<(), String> {
    run_on_large_stack(|| {
        let search = parse(&[
            "cass",
            "search",
            "needle",
            "--refresh",
            "--data-dir",
            "/tmp/cass-refresh-contract",
            "--json",
        ])?;
        match search.command {
            Some(Commands::Search {
                refresh: true,
                data_dir: Some(data_dir),
                json: true,
                ..
            }) if data_dir.display().to_string() == "/tmp/cass-refresh-contract" => {}
            other => {
                return Err(format!(
                    "search refresh preflight must stay data-dir scoped: {other:?}"
                ));
            }
        }

        let tui = parse(&[
            "cass",
            "tui",
            "--once",
            "--catch-up",
            "--data-dir",
            "/tmp/cass-refresh-contract",
        ])?;
        match tui.command {
            Some(Commands::Tui {
                once: true,
                refresh: true,
                data_dir: Some(data_dir),
                ..
            }) if data_dir.display().to_string() == "/tmp/cass-refresh-contract" => Ok(()),
            other => Err(format!(
                "tui catch-up preflight must stay data-dir scoped: {other:?}"
            )),
        }
    })
}

#[test]
fn index_refresh_operator_controls_remain_parseable() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "index",
            "--full",
            "--force-rebuild",
            "--json",
            "--idempotency-key",
            "stale-refresh-001",
            "--progress-interval-ms",
            "250",
            "--no-progress-events",
        ])?;

        match cli.command {
            Some(Commands::Index {
                full: true,
                force_rebuild: true,
                json: true,
                idempotency_key: Some(key),
                progress_interval_ms: 250,
                no_progress_events: true,
                ..
            }) if key == "stale-refresh-001" => Ok(()),
            other => Err(format!(
                "expected full refresh operator controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn index_refresh_robot_alias_keeps_global_format_contract() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "--robot-format",
            "jsonl",
            "index",
            "--full",
            "--robot",
            "--idempotency-key",
            "stale-refresh-jsonl-001",
            "--progress-interval-ms",
            "500",
        ])?;

        match cli {
            Cli {
                robot_format: Some(RobotFormat::Jsonl),
                command:
                    Some(Commands::Index {
                        full: true,
                        json: true,
                        idempotency_key: Some(key),
                        progress_interval_ms: 500,
                        ..
                    }),
                ..
            } if key == "stale-refresh-jsonl-001" => Ok(()),
            other => Err(format!(
                "index refresh robot alias must preserve global robot format: {other:?}"
            )),
        }
    })
}

#[test]
fn index_refresh_force_alias_stays_available_for_repair_scripts() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&["cass", "index", "--force"])?;

        match cli.command {
            Some(Commands::Index {
                force_rebuild: true,
                ..
            }) => Ok(()),
            other => Err(format!(
                "expected --force alias to map to force_rebuild: {other:?}"
            )),
        }
    })
}

#[test]
fn raw_mirror_and_doctor_modules_are_public_embedding_surfaces() -> Result<(), String> {
    let data_dir = tempfile::tempdir().map_err(|err| format!("temp data dir: {err}"))?;
    let source_path = data_dir.path().join("session.jsonl");
    std::fs::write(&source_path, b"{\"role\":\"user\",\"content\":\"hello\"}\n")
        .map_err(|err| format!("write source fixture: {err}"))?;

    let db_link = RawMirrorDbLink {
        conversation_id: Some(7),
        message_count: Some(1),
        source_path: Some(source_path.display().to_string()),
        started_at_ms: Some(123),
    };
    let captured = capture_source_file(RawMirrorCaptureInput {
        data_dir: data_dir.path(),
        provider: "contract-test",
        source_id: "session-1",
        origin_kind: "local",
        origin_host: None,
        source_path: &source_path,
        db_links: std::slice::from_ref(&db_link),
    })
    .map_err(|err| format!("capture raw mirror source: {err}"))?;
    merge_manifest_db_links(
        data_dir.path(),
        &captured.manifest_relative_path,
        &[db_link],
    )
    .map_err(|err| format!("merge raw mirror db links: {err}"))?;

    let summary = storage_summary(data_dir.path());
    assert_eq!(summary.manifest_count, 1);
    assert_eq!(summary.unique_blob_count, 1);

    let prune_report = prune(
        data_dir.path(),
        RawMirrorPruneOptions {
            older_than_ms: Some(i64::MAX),
            max_size_bytes: None,
            keep_tags: Vec::new(),
            safety_hold_down_ms: 0,
            apply: false,
            ..RawMirrorPruneOptions::default()
        },
    )
    .map_err(|err| format!("dry-run raw mirror prune: {err}"))?;
    assert_eq!(prune_report.mode, "dry-run");

    let doctor_request = DoctorCommandRequest::from_cli_flags_with_backups(
        Some(data_dir.path().to_path_buf()),
        None,
        Some(RobotFormat::Json),
        true,
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        None,
        false,
        false,
        false,
    )
    .map_err(|err| format!("build public doctor check request: {err}"))?;
    assert_eq!(doctor_request.surface, DoctorCommandSurface::Check);
    assert_eq!(DoctorBackupCommand::List.stable_name(), "list");
    let _execute: fn(DoctorCommandRequest) -> coding_agent_search::CliResult<()> =
        coding_agent_search::doctor::execute_doctor_command;

    Ok(())
}

#[test]
fn index_watch_refresh_entrypoints_remain_parseable() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&[
            "cass",
            "index",
            "--watch",
            "--watch-interval",
            "7",
            "--watch-once",
            "/sessions/a.jsonl,/sessions/b.jsonl",
            "--watch-once",
            "/sessions/c.jsonl",
            "--json",
        ])?;

        match cli.command {
            Some(Commands::Index {
                watch: true,
                watch_interval: 7,
                watch_once: Some(paths),
                json: true,
                ..
            }) => {
                let rendered: Vec<String> = paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                if rendered
                    == [
                        "/sessions/a.jsonl",
                        "/sessions/b.jsonl",
                        "/sessions/c.jsonl",
                    ]
                {
                    Ok(())
                } else {
                    Err(format!("watch-once paths parsed incorrectly: {rendered:?}"))
                }
            }
            other => Err(format!(
                "expected watch refresh entrypoint controls to parse: {other:?}"
            )),
        }
    })
}

#[test]
fn index_watch_refresh_defaults_stay_bounded() -> Result<(), String> {
    run_on_large_stack(|| {
        let cli = parse(&["cass", "index", "--watch"])?;

        match cli.command {
            Some(Commands::Index {
                watch: true,
                watch_interval: 30,
                watch_once: None,
                ..
            }) => Ok(()),
            other => Err(format!(
                "expected bounded watch refresh defaults to parse: {other:?}"
            )),
        }
    })
}
