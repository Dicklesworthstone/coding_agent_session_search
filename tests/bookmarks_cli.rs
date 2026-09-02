//! Integration tests for the `cass bookmarks` command surface.
//!
//! These drive the library entry point (`run_bookmarks_command_to`) directly
//! with an isolated `--data-dir`; the binary dispatcher is not exercised here.

use clap::Parser;
use coding_agent_search::bookmarks::{
    BOOKMARKS_EXIT_IO, BOOKMARKS_EXIT_NOT_FOUND, BOOKMARKS_EXIT_OK, BOOKMARKS_EXIT_USAGE,
    BookmarksArgs, BookmarksCommand, bookmarks_path_in, run_bookmarks_command_to,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Run one subcommand, returning `(exit_code, captured_stdout)`.
fn run(command: BookmarksCommand) -> (i32, String) {
    let mut out = Vec::new();
    let code = run_bookmarks_command_to(BookmarksArgs { command }, &mut out)
        .expect("bookmark commands report failures via exit codes, never Err");
    (code, String::from_utf8(out).expect("stdout must be utf-8"))
}

/// Run one subcommand in `--json` mode and parse its single stdout document.
fn run_json(command: BookmarksCommand) -> (i32, Value) {
    let (code, stdout) = run(command);
    assert_eq!(
        stdout.trim().lines().count(),
        1,
        "exactly one JSON line expected on stdout, got {stdout:?}"
    );
    let doc: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be one JSON document, got {stdout:?}: {e}"));
    (code, doc)
}

struct AddSpec<'a> {
    source: &'a str,
    line: Option<u64>,
    title: Option<&'a str>,
    note: Option<&'a str>,
    tags: Option<&'a str>,
}

fn add_cmd(dir: &Path, spec: &AddSpec<'_>, json: bool) -> BookmarksCommand {
    BookmarksCommand::Add {
        source_path: PathBuf::from(spec.source),
        line: spec.line,
        title: spec.title.map(str::to_string),
        note: spec.note.map(str::to_string),
        tags: spec.tags.map(str::to_string),
        agent: None,
        workspace: None,
        snippet: None,
        json,
        data_dir: Some(dir.to_path_buf()),
    }
}

fn list_cmd(dir: &Path, limit: Option<usize>, tag: Option<&str>, json: bool) -> BookmarksCommand {
    BookmarksCommand::List {
        limit,
        tag: tag.map(str::to_string),
        json,
        data_dir: Some(dir.to_path_buf()),
    }
}

fn search_cmd(dir: &Path, query: &str, limit: Option<usize>, json: bool) -> BookmarksCommand {
    BookmarksCommand::Search {
        query: query.to_string(),
        limit,
        json,
        data_dir: Some(dir.to_path_buf()),
    }
}

fn remove_cmd(dir: &Path, id: i64, json: bool) -> BookmarksCommand {
    BookmarksCommand::Remove {
        id,
        json,
        data_dir: Some(dir.to_path_buf()),
    }
}

fn export_cmd(dir: &Path, output: Option<PathBuf>, json: bool) -> BookmarksCommand {
    BookmarksCommand::Export {
        output,
        json,
        data_dir: Some(dir.to_path_buf()),
    }
}

fn import_cmd(dir: &Path, input: PathBuf, json: bool) -> BookmarksCommand {
    BookmarksCommand::Import {
        input,
        json,
        data_dir: Some(dir.to_path_buf()),
    }
}

fn bookmarks_of(doc: &Value) -> &Vec<Value> {
    doc["bookmarks"]
        .as_array()
        .unwrap_or_else(|| panic!("document must carry a `bookmarks` array: {doc}"))
}

fn add_ok(dir: &Path, spec: &AddSpec<'_>) -> i64 {
    let (code, doc) = run_json(add_cmd(dir, spec, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK, "add failed: {doc}");
    assert_eq!(doc["success"], true);
    let id = doc["bookmark"]["id"]
        .as_i64()
        .unwrap_or_else(|| panic!("add must echo the new bookmark id: {doc}"));
    assert!(id > 0, "ids are positive rowids, got {id}");
    id
}

#[test]
fn add_then_list_round_trips_title_tags_and_line() {
    let dir = TempDir::new().expect("tempdir");
    let id = add_ok(
        dir.path(),
        &AddSpec {
            source: "/sessions/auth.jsonl",
            line: Some(42),
            title: Some("Auth bug"),
            note: Some("found it"),
            tags: Some("rust, important"),
        },
    );
    assert!(
        bookmarks_path_in(dir.path()).is_file(),
        "bookmarks.db must be created inside --data-dir"
    );

    let (code, doc) = run_json(list_cmd(dir.path(), None, None, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["success"], true);
    assert_eq!(doc["count"], 1);
    let items = bookmarks_of(&doc);
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["id"].as_i64(), Some(id));
    assert_eq!(item["title"], "Auth bug");
    assert_eq!(item["tags"], "rust, important");
    assert_eq!(item["line_number"], 42);
    assert_eq!(item["source_path"], "/sessions/auth.jsonl");
    assert_eq!(item["note"], "found it");

    let (code, stdout) = run(list_cmd(dir.path(), None, None, false));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert!(
        stdout.contains(&format!("#{id}\t/sessions/auth.jsonl:42\tAuth bug")),
        "human list line missing: {stdout:?}"
    );
    assert!(stdout.contains("tags: rust, important"), "{stdout:?}");
    assert!(stdout.contains("note: found it"), "{stdout:?}");
}

#[test]
fn add_without_title_defaults_to_file_name_in_human_mode() {
    let dir = TempDir::new().expect("tempdir");
    let (code, stdout) = run(add_cmd(
        dir.path(),
        &AddSpec {
            source: "/sessions/project/transcript.jsonl",
            line: None,
            title: None,
            note: None,
            tags: None,
        },
        false,
    ));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert!(
        stdout.starts_with("Saved bookmark #")
            && stdout.ends_with(": transcript.jsonl (/sessions/project/transcript.jsonl)\n"),
        "{stdout:?}"
    );

    let (_, doc) = run_json(list_cmd(dir.path(), None, None, true));
    let items = bookmarks_of(&doc);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "transcript.jsonl");
    assert_eq!(items[0]["line_number"], Value::Null);
}

#[test]
fn search_matches_note_text_and_rejects_empty_query() {
    let dir = TempDir::new().expect("tempdir");
    add_ok(
        dir.path(),
        &AddSpec {
            source: "/s/a.jsonl",
            line: None,
            title: Some("First"),
            note: Some("authentication flow explained"),
            tags: None,
        },
    );
    add_ok(
        dir.path(),
        &AddSpec {
            source: "/s/b.jsonl",
            line: None,
            title: Some("Second"),
            note: Some("unrelated musings"),
            tags: None,
        },
    );

    let (code, doc) = run_json(search_cmd(dir.path(), "authentication", None, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    let items = bookmarks_of(&doc);
    assert_eq!(items.len(), 1, "only the note match should hit: {doc}");
    assert_eq!(items[0]["source_path"], "/s/a.jsonl");

    let (code, doc) = run_json(search_cmd(dir.path(), "zzz-no-such-text", None, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["count"], 0);
    assert!(bookmarks_of(&doc).is_empty());

    let (code, stdout) = run(search_cmd(dir.path(), "   ", None, true));
    assert_eq!(code, BOOKMARKS_EXIT_USAGE);
    assert!(
        stdout.is_empty(),
        "usage errors must not write stdout: {stdout:?}"
    );
}

#[test]
fn remove_returns_zero_then_not_found_for_the_same_id() {
    let dir = TempDir::new().expect("tempdir");
    let id = add_ok(
        dir.path(),
        &AddSpec {
            source: "/s/gone.jsonl",
            line: Some(3),
            title: Some("Doomed"),
            note: None,
            tags: None,
        },
    );

    let (code, doc) = run_json(remove_cmd(dir.path(), id, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["success"], true);
    assert_eq!(doc["removed"], id);

    // Planted negative: the same id a second time is a not-found (exit 13, the crate's
    // mapping/not-found class), with nothing on stdout.
    let (code, stdout) = run(remove_cmd(dir.path(), id, true));
    assert_eq!(code, BOOKMARKS_EXIT_NOT_FOUND);
    assert!(stdout.is_empty(), "{stdout:?}");

    let (code, stdout) = run(remove_cmd(dir.path(), id, false));
    assert_eq!(code, BOOKMARKS_EXIT_NOT_FOUND);
    assert!(stdout.is_empty(), "{stdout:?}");

    let (code, doc) = run_json(list_cmd(dir.path(), None, None, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert!(bookmarks_of(&doc).is_empty());
}

#[test]
fn export_to_file_then_import_into_fresh_data_dir() {
    let dir_a = TempDir::new().expect("tempdir a");
    add_ok(
        dir_a.path(),
        &AddSpec {
            source: "/s/first.jsonl",
            line: Some(1),
            title: Some("First"),
            note: None,
            tags: Some("tag1"),
        },
    );
    add_ok(
        dir_a.path(),
        &AddSpec {
            source: "/s/second.jsonl",
            line: None,
            title: Some("Second"),
            note: Some("second note"),
            tags: Some("tag2"),
        },
    );

    // Nested destination: export must create the parent directory.
    let file = dir_a.path().join("exports").join("bookmarks.json");
    let (code, doc) = run_json(export_cmd(dir_a.path(), Some(file.clone()), true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["success"], true);
    assert_eq!(doc["count"], 2);
    assert_eq!(doc["exported"], file.display().to_string());
    let raw = std::fs::read_to_string(&file).expect("export file exists");
    let exported: Value = serde_json::from_str(&raw).expect("export file is JSON");
    assert_eq!(exported.as_array().map(Vec::len), Some(2));

    let dir_b = TempDir::new().expect("tempdir b");
    let (code, doc) = run_json(import_cmd(dir_b.path(), file.clone(), true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["success"], true);
    assert_eq!(doc["imported"], 2);
    assert_eq!(doc["skipped"], 0);

    let (_, doc) = run_json(list_cmd(dir_b.path(), None, None, true));
    let items = bookmarks_of(&doc);
    assert_eq!(items.len(), 2);
    let mut titles: Vec<&str> = items
        .iter()
        .map(|b| b["title"].as_str().expect("title"))
        .collect();
    titles.sort_unstable();
    assert_eq!(titles, ["First", "Second"]);
    let second = items
        .iter()
        .find(|b| b["title"] == "Second")
        .expect("second bookmark");
    assert_eq!(second["note"], "second note");
    assert_eq!(second["tags"], "tag2");

    // Importing the same file again is a no-op: duplicates are skipped.
    let (code, stdout) = run(import_cmd(dir_b.path(), file, false));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert!(
        stdout.starts_with("Imported 0 bookmark(s) from ")
            && stdout.contains("(2 duplicate(s) skipped)"),
        "{stdout:?}"
    );
    let (_, doc) = run_json(list_cmd(dir_b.path(), None, None, true));
    assert_eq!(doc["count"], 2);
}

#[test]
fn export_without_output_is_one_document_and_round_trips() {
    let dir = TempDir::new().expect("tempdir");
    add_ok(
        dir.path(),
        &AddSpec {
            source: "/s/one.jsonl",
            line: None,
            title: Some("One"),
            note: None,
            tags: None,
        },
    );
    add_ok(
        dir.path(),
        &AddSpec {
            source: "/s/two.jsonl",
            line: Some(2),
            title: Some("Two"),
            note: None,
            tags: None,
        },
    );

    // --json: a single envelope that embeds the payload.
    let (code, doc) = run_json(export_cmd(dir.path(), None, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["exported"], "stdout");
    assert_eq!(doc["count"], 2);
    assert_eq!(bookmarks_of(&doc).len(), 2);

    // Human mode: the raw export array on stdout.
    let (code, stdout) = run(export_cmd(dir.path(), None, false));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    let raw: Value = serde_json::from_str(&stdout).expect("stdout export is JSON");
    assert_eq!(raw.as_array().map(Vec::len), Some(2));

    // The --json envelope form is accepted by import as well.
    let envelope_path = dir.path().join("envelope.json");
    std::fs::write(&envelope_path, doc.to_string()).expect("write envelope");
    let dir_c = TempDir::new().expect("tempdir c");
    let (code, doc) = run_json(import_cmd(dir_c.path(), envelope_path, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["imported"], 2);
}

#[test]
fn import_rejects_bad_input_with_usage_code_and_missing_file_with_io_code() {
    let dir = TempDir::new().expect("tempdir");
    let write = |name: &str, body: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, body).expect("write fixture");
        path
    };

    let (code, stdout) = run(import_cmd(
        dir.path(),
        write("garbage.json", "not json"),
        true,
    ));
    assert_eq!(code, BOOKMARKS_EXIT_USAGE);
    assert!(stdout.is_empty(), "{stdout:?}");

    let (code, _) = run(import_cmd(
        dir.path(),
        write("shape.json", r#"{"nope": []}"#),
        true,
    ));
    assert_eq!(code, BOOKMARKS_EXIT_USAGE);

    let (code, _) = run(import_cmd(
        dir.path(),
        write("partial.json", r#"[{"id": 1}]"#),
        true,
    ));
    assert_eq!(code, BOOKMARKS_EXIT_USAGE);

    let (code, doc) = run_json(import_cmd(dir.path(), write("empty.json", "[]"), true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(doc["imported"], 0);

    let (code, stdout) = run(import_cmd(
        dir.path(),
        dir.path().join("missing.json"),
        true,
    ));
    assert_eq!(code, BOOKMARKS_EXIT_IO);
    assert!(stdout.is_empty(), "{stdout:?}");

    // Nothing above may have written rows.
    let (_, doc) = run_json(list_cmd(dir.path(), None, None, true));
    assert_eq!(doc["count"], 0);
}

#[test]
fn list_supports_limit_and_case_insensitive_tag_filter() {
    let dir = TempDir::new().expect("tempdir");
    for (source, tags) in [
        ("/s/a.jsonl", "rust"),
        ("/s/b.jsonl", "python"),
        ("/s/c.jsonl", "rust, important"),
    ] {
        add_ok(
            dir.path(),
            &AddSpec {
                source,
                line: None,
                title: None,
                note: None,
                tags: Some(tags),
            },
        );
    }

    let (code, doc) = run_json(list_cmd(dir.path(), Some(2), None, true));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert_eq!(bookmarks_of(&doc).len(), 2);

    let (_, doc) = run_json(list_cmd(dir.path(), None, Some("RUST"), true));
    let items = bookmarks_of(&doc);
    assert_eq!(items.len(), 2, "{doc}");
    assert!(
        items
            .iter()
            .all(|b| b["tags"].as_str().is_some_and(|t| t.contains("rust")))
    );

    let (code, stdout) = run(list_cmd(dir.path(), None, Some("go"), false));
    assert_eq!(code, BOOKMARKS_EXIT_OK);
    assert!(
        stdout.is_empty(),
        "empty listings report on stderr only: {stdout:?}"
    );
}

#[test]
fn clap_parses_every_bookmarks_subcommand() {
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: BookmarksCommand,
    }

    let cli = Cli::try_parse_from([
        "cass-bookmarks",
        "add",
        "/tmp/x.jsonl",
        "-n",
        "7",
        "--tags",
        "a,b",
        "--robot",
        "--data-dir",
        "/d",
    ])
    .expect("add parses");
    match cli.command {
        BookmarksCommand::Add {
            source_path,
            line,
            tags,
            json,
            data_dir,
            ..
        } => {
            assert_eq!(source_path, PathBuf::from("/tmp/x.jsonl"));
            assert_eq!(line, Some(7));
            assert_eq!(tags.as_deref(), Some("a,b"));
            assert!(json, "--robot must alias --json");
            assert_eq!(data_dir, Some(PathBuf::from("/d")));
        }
        other => panic!("expected Add, got {other:?}"),
    }

    let cli = Cli::try_parse_from(["cass-bookmarks", "list", "--limit", "5", "--json"])
        .expect("list parses");
    match cli.command {
        BookmarksCommand::List {
            limit,
            tag,
            json,
            data_dir,
        } => {
            assert_eq!(limit, Some(5));
            assert_eq!(tag, None);
            assert!(json);
            assert_eq!(data_dir, None);
        }
        other => panic!("expected List, got {other:?}"),
    }

    let cli = Cli::try_parse_from(["cass-bookmarks", "search", "auth", "--limit", "1"])
        .expect("search parses");
    match cli.command {
        BookmarksCommand::Search {
            query, limit, json, ..
        } => {
            assert_eq!(query, "auth");
            assert_eq!(limit, Some(1));
            assert!(!json);
        }
        other => panic!("expected Search, got {other:?}"),
    }

    let cli = Cli::try_parse_from(["cass-bookmarks", "remove", "7"]).expect("remove parses");
    match cli.command {
        BookmarksCommand::Remove { id, json, .. } => {
            assert_eq!(id, 7);
            assert!(!json);
        }
        other => panic!("expected Remove, got {other:?}"),
    }

    let cli =
        Cli::try_parse_from(["cass-bookmarks", "export", "-o", "out.json"]).expect("export parses");
    match cli.command {
        BookmarksCommand::Export { output, .. } => {
            assert_eq!(output, Some(PathBuf::from("out.json")));
        }
        other => panic!("expected Export, got {other:?}"),
    }

    let cli = Cli::try_parse_from(["cass-bookmarks", "import", "in.json", "--robot"])
        .expect("import parses");
    match cli.command {
        BookmarksCommand::Import { input, json, .. } => {
            assert_eq!(input, PathBuf::from("in.json"));
            assert!(json);
        }
        other => panic!("expected Import, got {other:?}"),
    }

    assert!(
        Cli::try_parse_from(["cass-bookmarks", "remove", "abc"]).is_err(),
        "non-numeric id must be a parse error"
    );
    assert!(
        Cli::try_parse_from(["cass-bookmarks"]).is_err(),
        "a subcommand is required"
    );
}
