use clap::Parser;
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
