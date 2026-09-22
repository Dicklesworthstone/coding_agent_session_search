//! Real-binary proof for the reviewed logical-archive v20 -> v21 bridge.

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

const REVIEWED_SOURCE_VERSION: i64 = 20;
const REVIEWED_TARGET_VERSION: i64 = 21;

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

fn make_v20_source(root: &Path) -> PathBuf {
    assert_eq!(
        CURRENT_SCHEMA_VERSION, REVIEWED_TARGET_VERSION,
        "reviewed migration test must be revisited when the canonical target schema advances"
    );
    let source = root.join("source-v20.db");
    let storage = SqliteStorage::open(&source).expect("create current canonical fixture");
    storage
        .ensure_agent(&Agent {
            id: None,
            slug: "reviewed-v20-agent".into(),
            name: "Reviewed v20 agent".into(),
            version: Some("preserve-me".into()),
            kind: AgentKind::Cli,
        })
        .expect("insert canonical source row");
    drop(storage);

    let connection =
        Connection::open(source.to_str().expect("UTF-8 fixture path")).expect("open fixture");
    connection
        .execute_batch(
            "DROP INDEX idx_conversations_context;
             DELETE FROM _schema_migrations WHERE version >= 21;
             UPDATE meta SET value = '20' WHERE key = 'schema_version';",
        )
        .expect("downgrade only the reviewed v21 index/schema authority");
    connection
        .close()
        .expect("durably close downgraded fixture");
    source
}

fn export_v20(home: &Path, source: &Path, backup: &Path) -> Value {
    json(
        command(home)
            .args(["archive", "export", "--db"])
            .arg(source)
            .args([
                "--archive-id",
                "reviewed-v20",
                "--include-private",
                "--output",
            ])
            .arg(backup)
            .output()
            .expect("run archive export"),
    )
}

#[test]
fn reviewed_v20_backup_migrates_to_v21_and_retries_read_only() {
    let home = tempfile::tempdir().expect("temp home");
    let source = make_v20_source(home.path());
    let backup = home.path().join("v20.jsonl");
    let export_receipt = export_v20(home.path(), &source, &backup);
    assert_eq!(
        export_receipt["archive_id"], "reviewed-v20",
        "{export_receipt}"
    );

    let exact_destination = home.path().join("exact-refused.db");
    let exact = command(home.path())
        .args(["archive", "import"])
        .arg(&backup)
        .args([
            "--archive-id",
            "reviewed-v20",
            "--include-private",
            "--output",
        ])
        .arg(&exact_destination)
        .output()
        .expect("run exact import");
    assert!(
        !exact.status.success(),
        "exact schema import unexpectedly accepted v20"
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
                "reviewed-v20",
                "--include-private",
                "--allow-compatible-schema",
                "--output",
            ])
            .arg(&destination)
            .output()
            .expect("run reviewed migration"),
    );
    assert_eq!(created["destination_status"], "created", "{created}");
    assert_eq!(
        created["schema_migration"]["mode"], "reviewed_v20_to_v21",
        "{created}"
    );
    assert_eq!(
        created["schema_migration"]["from_storage_schema_version"],
        REVIEWED_SOURCE_VERSION.to_string(),
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
        .query_row("SELECT slug, version FROM agents WHERE slug = 'reviewed-v20-agent'")
        .expect("read migrated agent");
    assert_eq!(
        agent.get_typed::<String>(0).expect("agent slug"),
        "reviewed-v20-agent"
    );
    assert_eq!(
        agent.get_typed::<Option<String>>(1).expect("agent version"),
        Some("preserve-me".into())
    );
    drop(storage);

    let before = database_files(&destination);
    let repeated = json(
        command(home.path())
            .args(["archive", "import"])
            .arg(&backup)
            .args([
                "--archive-id",
                "reviewed-v20",
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
    assert_eq!(
        repeated["schema_migration"]["mode"], "reviewed_v20_to_v21",
        "{repeated}"
    );
    assert_eq!(
        before,
        database_files(&destination),
        "identical reviewed retry must not mutate the migrated database image"
    );
}
