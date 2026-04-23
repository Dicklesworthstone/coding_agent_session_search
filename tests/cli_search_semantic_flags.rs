//! CLI parsing tests for semantic search flags (bead bd-3bbv)
//!
//! Tests for the --model, --rerank, --reranker, --daemon, and --no-daemon flags
//! added to the search command.

use clap::Parser;
use coding_agent_search::search::query::SearchMode;
use coding_agent_search::{Cli, Commands};

fn run_on_large_stack<T, F>(f: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new()
        .name("cli-search-semantic-flags".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(f)
        .expect("spawn large-stack parser thread")
        .join()
        .expect("large-stack parser thread should not panic")
}

fn parse_cli<const N: usize>(args: [&'static str; N]) -> Cli {
    run_on_large_stack(move || Cli::try_parse_from(args).expect("parse search flags"))
}

#[test]
fn search_parses_model_flag() {
    let cli = parse_cli(["cass", "search", "query", "--model", "minilm"]);

    match cli.command {
        Some(Commands::Search { model, .. }) => {
            assert_eq!(model, Some("minilm".to_string()));
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_parses_rerank_flag() {
    let cli = parse_cli(["cass", "search", "query", "--rerank"]);

    match cli.command {
        Some(Commands::Search { rerank, .. }) => {
            assert!(rerank, "rerank flag should be true");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_parses_reranker_flag() {
    let cli = parse_cli(["cass", "search", "query", "--rerank", "--reranker", "bge"]);

    match cli.command {
        Some(Commands::Search {
            rerank, reranker, ..
        }) => {
            assert!(rerank, "rerank flag should be true");
            assert_eq!(reranker, Some("bge".to_string()));
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_parses_daemon_flag() {
    let cli = parse_cli(["cass", "search", "query", "--daemon"]);

    match cli.command {
        Some(Commands::Search { daemon, .. }) => {
            assert!(daemon, "daemon flag should be true");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_parses_no_daemon_flag() {
    let cli = parse_cli(["cass", "search", "query", "--no-daemon"]);

    match cli.command {
        Some(Commands::Search { no_daemon, .. }) => {
            assert!(no_daemon, "no_daemon flag should be true");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_default_flags_are_false() {
    let cli = parse_cli(["cass", "search", "query"]);

    match cli.command {
        Some(Commands::Search {
            model,
            rerank,
            reranker,
            daemon,
            no_daemon,
            ..
        }) => {
            assert_eq!(model, None, "model should be None by default");
            assert!(!rerank, "rerank should be false by default");
            assert_eq!(reranker, None, "reranker should be None by default");
            assert!(!daemon, "daemon should be false by default");
            assert!(!no_daemon, "no_daemon should be false by default");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_without_mode_keeps_hybrid_preferred_default_intent() {
    let cli = parse_cli(["cass", "search", "query"]);

    assert!(
        matches!(cli.command, Some(Commands::Search { .. })),
        "expected search command"
    );
    let Some(Commands::Search { mode, .. }) = cli.command else {
        return;
    };

    assert_eq!(mode, None, "absent --mode should stay distinguishable");
    assert_eq!(SearchMode::default(), SearchMode::Hybrid);
}

#[test]
fn search_explicit_lexical_and_semantic_modes_are_preserved() {
    for (mode_arg, expected) in [
        ("lexical", SearchMode::Lexical),
        ("semantic", SearchMode::Semantic),
    ] {
        let cli = parse_cli(["cass", "search", "query", "--mode", mode_arg]);

        assert!(
            matches!(cli.command, Some(Commands::Search { .. })),
            "expected search command for --mode {mode_arg}"
        );
        let Some(Commands::Search { mode, .. }) = cli.command else {
            return;
        };

        assert_eq!(
            mode,
            Some(expected),
            "explicit --mode {mode_arg} should be preserved"
        );
    }
}

#[test]
fn search_combines_mode_and_model_flags() {
    let cli = parse_cli([
        "cass", "search", "query", "--mode", "semantic", "--model", "minilm",
    ]);

    match cli.command {
        Some(Commands::Search { mode, model, .. }) => {
            // Pin the exact parsed mode — a regression that silently
            // defaults --mode semantic to Lexical or Hybrid would
            // otherwise slip past `.is_some()`.
            assert_eq!(
                mode,
                Some(SearchMode::Semantic),
                "--mode semantic must parse to SearchMode::Semantic exactly; \
                 got {mode:?}"
            );
            assert_eq!(model, Some("minilm".to_string()));
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_combines_rerank_and_daemon_flags() {
    let cli = parse_cli([
        "cass",
        "search",
        "query",
        "--rerank",
        "--reranker",
        "bge",
        "--daemon",
    ]);

    match cli.command {
        Some(Commands::Search {
            rerank,
            reranker,
            daemon,
            ..
        }) => {
            assert!(rerank);
            assert_eq!(reranker, Some("bge".to_string()));
            assert!(daemon);
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

// Note: The mutual exclusivity of --daemon and --no-daemon is enforced at runtime,
// not at parse time, so we test that separately via integration tests.

#[test]
fn search_parses_approximate_flag() {
    let cli = parse_cli(["cass", "search", "query", "--approximate"]);

    match cli.command {
        Some(Commands::Search { approximate, .. }) => {
            assert!(approximate, "approximate flag should be true");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_approximate_default_is_false() {
    let cli = parse_cli(["cass", "search", "query"]);

    match cli.command {
        Some(Commands::Search { approximate, .. }) => {
            assert!(!approximate, "approximate should be false by default");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_combines_mode_semantic_and_approximate() {
    let cli = parse_cli([
        "cass",
        "search",
        "query",
        "--mode",
        "semantic",
        "--approximate",
    ]);

    match cli.command {
        Some(Commands::Search {
            mode, approximate, ..
        }) => {
            assert_eq!(
                mode,
                Some(SearchMode::Semantic),
                "--mode semantic + --approximate must preserve \
                 SearchMode::Semantic as the parsed mode; got {mode:?}"
            );
            assert!(approximate, "approximate should be true");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}

#[test]
fn search_combines_mode_hybrid_and_approximate() {
    let cli = parse_cli([
        "cass",
        "search",
        "query",
        "--mode",
        "hybrid",
        "--approximate",
    ]);

    match cli.command {
        Some(Commands::Search {
            mode, approximate, ..
        }) => {
            assert_eq!(
                mode,
                Some(SearchMode::Hybrid),
                "--mode hybrid + --approximate must preserve \
                 SearchMode::Hybrid as the parsed mode; got {mode:?}"
            );
            assert!(approximate, "approximate should be true");
        }
        other => panic!("expected search command, got {other:?}"),
    }
}
