//! Real-binary proof for the reviewed logical-archive v20, v21 and v22 -> v23
//! bridges.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use coding_agent_search::franken_sync::Connection;
use coding_agent_search::franken_sync::compat::RowExt;
use coding_agent_search::model::types::{Agent, AgentKind};
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, SqliteStorage};
use serde_json::Value;

const REVIEWED_TARGET_VERSION: i64 = 23;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cass"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("CASS_DATA_DIR", home.join("unused-default"))
        .env("CASS_IGNORE_SOURCES_CONFIG", "1")
        .env("CASS_AUTO_REFRESH", "0")
        .env("CODING_AGENT_SEARCH_NO_UPDATE_PROMPT", "1")
        .env_remove("CASS_OUTPUT_FORMAT")
        .env_remove("TOON_DEFAULT_FORMAT")
        .timeout(Duration::from_secs(90));
    command
}

fn json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "archive command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON receipt")
}

fn database_files(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    ["", "-wal", "-shm", "-journal"]
        .into_iter()
        .filter_map(|suffix| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            let path = PathBuf::from(name);
            match fs::read(&path) {
                Ok(bytes) => Some((path, bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("cannot read test database image: {error}"),
            }
        })
        .collect()
}

/// A canonical database at storage schema `version`, made by undoing on a
/// current database exactly what each later reviewed step added: v23's session
/// tombstones, v22's source tombstones, and v21's context index. The v22 fixture
/// retains an existing source tombstone, which migration must preserve.
fn make_source(root: &Path, version: i64) -> PathBuf {
    assert_eq!(
        CURRENT_SCHEMA_VERSION, REVIEWED_TARGET_VERSION,
        "reviewed migration test must be revisited when the canonical target schema advances"
    );
    let source = root.join(format!("source-v{version}.db"));
    let storage = SqliteStorage::open(&source).expect("create current canonical fixture");
    storage
        .ensure_agent(&Agent {
            id: None,
            slug: format!("reviewed-v{version}-agent"),
            name: format!("Reviewed v{version} agent"),
            version: Some("preserve-me".into()),
            kind: AgentKind::Cli,
        })
        .expect("insert canonical source row");
    if version >= 22 {
        storage
            .raw()
            .execute(
                "INSERT INTO forgotten_sources(source_path, size_bytes, mtime_ms, forgotten_at_ms)
             VALUES('/fixture/retained-forget.jsonl', 12, 34, 56)",
            )
            .expect("seed existing source tombstone");
    }
    drop(storage);

    let downgrade = match version {
        20 => {
            "DROP TABLE forgotten_conversations;
             DROP TABLE forgotten_sources;
             DROP INDEX idx_conversations_context;
             DELETE FROM _schema_migrations WHERE version >= 21;
             UPDATE meta SET value = '20' WHERE key = 'schema_version';"
        }
        21 => {
            "DROP TABLE forgotten_conversations;
             DROP TABLE forgotten_sources;
             DELETE FROM _schema_migrations WHERE version >= 22;
             UPDATE meta SET value = '21' WHERE key = 'schema_version';"
        }
        22 => {
            "DROP TABLE forgotten_conversations;
             DELETE FROM _schema_migrations WHERE version >= 23;
             UPDATE meta SET value = '22' WHERE key = 'schema_version';"
        }
        other => panic!("no reviewed downgrade for schema {other}"),
    };
    let connection =
        Connection::open(source.to_str().expect("UTF-8 fixture path")).expect("open fixture");
    connection
        .execute_batch(downgrade)
        .expect("downgrade only what the reviewed steps added");
    connection
        .close()
        .expect("durably close downgraded fixture");
    source
}

fn export_source(home: &Path, source: &Path, backup: &Path, archive_id: &str) -> Value {
    json(
        command(home)
            .args(["archive", "export", "--db"])
            .arg(source)
            .args(["--archive-id", archive_id, "--include-private", "--output"])
            .arg(backup)
            .output()
            .expect("run archive export"),
    )
}

#[test]
fn reviewed_v20_backup_migrates_to_v23_and_retries_read_only() {
    assert_reviewed_backup_migrates(20, "reviewed_v20_to_v23");
}

/// A v0.9.0 export (schema v21) restores into the current v23 build.
#[test]
fn reviewed_v21_backup_migrates_to_v23_and_retries_read_only() {
    assert_reviewed_backup_migrates(21, "reviewed_v21_to_v23");
}

#[test]
fn reviewed_v22_backup_preserves_tombstones_and_retries_read_only() {
    assert_reviewed_backup_migrates(22, "reviewed_v22_to_v23");
}

fn assert_reviewed_backup_migrates(version: i64, mode: &str) {
    let home = tempfile::tempdir().expect("temp home");
    let source = make_source(home.path(), version);
    let archive_id = format!("reviewed-v{version}");
    let backup = home.path().join(format!("v{version}.jsonl"));
    let export_receipt = export_source(home.path(), &source, &backup, &archive_id);
    assert_eq!(export_receipt["archive_id"], archive_id, "{export_receipt}");

    let exact_destination = home.path().join("exact-refused.db");
    let exact = command(home.path())
        .args(["archive", "import"])
        .arg(&backup)
        .args(["--archive-id", &archive_id, "--include-private", "--output"])
        .arg(&exact_destination)
        .output()
        .expect("run exact import");
    assert!(
        !exact.status.success(),
        "exact schema import unexpectedly accepted v{version}"
    );
    assert!(
        !exact_destination.exists(),
        "failed exact import must publish nothing"
    );

    let destination = home.path().join("migrated.db");
    let created = json(
        command(home.path())
            .args(["archive", "import"])
            .arg(&backup)
            .args([
                "--archive-id",
                &archive_id,
                "--include-private",
                "--allow-compatible-schema",
                "--output",
            ])
            .arg(&destination)
            .output()
            .expect("run reviewed migration"),
    );
    assert_eq!(created["destination_status"], "created", "{created}");
    assert_eq!(created["schema_migration"]["mode"], mode, "{created}");
    assert_eq!(
        created["schema_migration"]["from_storage_schema_version"],
        version.to_string(),
        "{created}"
    );
    assert_eq!(
        created["schema_migration"]["to_storage_schema_version"],
        REVIEWED_TARGET_VERSION.to_string(),
        "{created}"
    );
    assert_eq!(
        created["schema_migration"]["source_rows_verified"], true,
        "{created}"
    );

    let storage = SqliteStorage::open_readonly(&destination).expect("open migrated archive");
    assert_eq!(
        storage.schema_version().expect("read current schema"),
        REVIEWED_TARGET_VERSION
    );
    let agent = storage
        .raw()
        .query_row(&format!(
            "SELECT slug, version FROM agents WHERE slug = 'reviewed-v{version}-agent'"
        ))
        .expect("read migrated agent");
    assert_eq!(
        agent.get_typed::<String>(0).expect("agent slug"),
        format!("reviewed-v{version}-agent")
    );
    assert_eq!(
        agent.get_typed::<Option<String>>(1).expect("agent version"),
        Some("preserve-me".into())
    );
    // Newly introduced tables are empty; existing v22 tombstones survive.
    let forgotten = storage
        .raw()
        .query_row("SELECT COUNT(*) FROM forgotten_sources")
        .expect("forgotten_sources exists after migration");
    assert_eq!(
        forgotten.get_typed::<i64>(0).expect("count"),
        if version >= 22 { 1 } else { 0 }
    );
    if version >= 22 {
        let stamp = storage
            .raw()
            .query_row(
                "SELECT source_path, size_bytes, mtime_ms, forgotten_at_ms FROM forgotten_sources",
            )
            .expect("preserved source tombstone");
        assert_eq!(
            stamp.get_typed::<String>(0).expect("source path"),
            "/fixture/retained-forget.jsonl"
        );
        assert_eq!(stamp.get_typed::<i64>(1).expect("size"), 12);
        assert_eq!(stamp.get_typed::<i64>(2).expect("mtime"), 34);
        assert_eq!(stamp.get_typed::<i64>(3).expect("forgotten time"), 56);
    }
    assert_eq!(
        storage
            .raw()
            .query_row("SELECT COUNT(*) FROM forgotten_conversations")
            .expect("session tombstones table exists")
            .get_typed::<i64>(0)
            .expect("count"),
        0
    );
    drop(storage);

    let before = database_files(&destination);
    let repeated = json(
        command(home.path())
            .args(["archive", "import"])
            .arg(&backup)
            .args([
                "--archive-id",
                &archive_id,
                "--include-private",
                "--allow-compatible-schema",
                "--if-identical",
                "--output",
            ])
            .arg(&destination)
            .output()
            .expect("run reviewed identical retry"),
    );
    assert_eq!(repeated["destination_status"], "unchanged", "{repeated}");
    assert_eq!(repeated["schema_migration"]["mode"], mode, "{repeated}");
    assert_eq!(
        before,
        database_files(&destination),
        "identical reviewed retry must not mutate the migrated database image"
    );
}
