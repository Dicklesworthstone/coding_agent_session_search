use super::*;

fn args(include_private: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "cass", "archive", "extract-conversation", "missing-backup.jsonl",
        "--conversation-id", "7", "--content-sha256",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "--output", "must-not-create.jsonl", "--json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if include_private {
        args.push("--include-private".into());
    }
    args
}

#[test]
fn conversation_extraction_is_explicit_and_discoverable_in_the_cli_tree() {
    use clap::CommandFactory;
    Cli::command().debug_assert();
    let cli = Cli::try_parse_from(args(true)).unwrap();
    assert!(cli.db.is_none());
    assert!(cli.json);
    let Root::Archive {
        command: Operation::ExtractConversation {
            input, conversation_id, content_sha256, output, include_private,
        },
    } = cli.command else {
        panic!("the exact archive command must not be rewritten to search or import");
    };
    assert_eq!(input, PathBuf::from("missing-backup.jsonl"));
    assert_eq!(conversation_id, 7);
    assert_eq!(content_sha256, "0".repeat(64));
    assert_eq!(output, PathBuf::from("must-not-create.jsonl"));
    assert!(include_private);
    assert!(clap_commands()[0].find_subcommand("extract-conversation").is_some());
}

#[test]
fn conversation_extraction_requires_private_acknowledgement_before_any_file_open() {
    let error = run(args(false)).unwrap_err();
    assert_eq!(classify_failure(&error), (2, "logical-archive-usage", false));
    assert!(error.to_string().contains("--include-private"));
    // The input does not exist. An attempted input open would instead be I/O.
}

#[test]
fn conversation_extraction_requires_a_digest_and_a_new_output_argument() {
    for flag in ["--content-sha256", "--output"] {
        let mut args = args(true);
        let at = args.iter().position(|argument| argument == flag).unwrap();
        args.drain(at..at + 2);
        assert!(Cli::try_parse_from(args).is_err());
    }
}
