use clap::Parser;
use coding_agent_search::{Cli, Commands};

fn parse(args: &[&str]) -> Result<Cli, String> {
    Cli::try_parse_from(args).map_err(|err| format!("parse cass CLI for {args:?}: {err}"))
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
