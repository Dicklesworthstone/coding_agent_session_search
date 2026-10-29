//! Connector for GitHub Copilot Chat session logs.
//!
//! Parsing lives in `franken_agent_detection::connectors::copilot`. CASS
//! handles detection and preservation of shared VS Code stores here.

use super::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole, NormalizedConversation,
    ScanContext,
};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Omit only VS Code state conclusively unrelated to legacy chat. Query
/// failures and existing chat keys keep the normal capture-before-parse path.
pub(crate) fn source_is_proven_chat_free(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    if path.file_name().is_some_and(|name| name == "state.vscdb") {
        return legacy_store_has_no_chat_key(path);
    }
    if path.file_name().is_none_or(|name| name != "workspace.json") {
        return false;
    }
    let Some(workspace) = path.parent() else {
        return false;
    };
    if workspace
        .parent()
        .and_then(Path::file_name)
        .is_none_or(|name| name != "workspaceStorage")
        || !legacy_store_has_no_chat_key(&workspace.join("state.vscdb"))
    {
        return false;
    }
    // This sidecar also resolves workspaces for native JSON/JSONL history.
    // Keep it whenever a transcript might exist or the directory is unsafe.
    let sessions = workspace.join("chatSessions");
    match std::fs::symlink_metadata(&sessions) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        _ => return false,
    }
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        if entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("jsonl")
            })
        {
            return false;
        }
    }
    true
}

fn legacy_store_has_no_chat_key(path: &Path) -> bool {
    use crate::franken_sync::compat::{OpenFlags, RowExt, open_with_flags};

    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        _ => return false,
    }
    let Some(path) = path.to_str() else {
        return false;
    };
    let Ok(connection) = open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    // Ask only whether the legacy key exists. Even a NULL, malformed, empty
    // or foreign-provider payload remains evidence; this is not a parser.
    let query = "SELECT COUNT(*) FROM ItemTable WHERE key = 'interactive.sessions'";
    let Ok(row) = connection.query_row(query) else {
        return false;
    };
    matches!(row.get_typed::<i64>(0), Ok(0))
}

/// GitHub Copilot Chat (VS Code, VS Code Insiders, VSCodium).
#[derive(Default)]
pub struct CopilotConnector {
    inner: franken_agent_detection::CopilotConnector,
}

impl CopilotConnector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Native VS Code chat stores the upstream connector would scan in `ctx`:
    /// `workspaceStorage/*/chatSessions`, the empty-window and transferred
    /// session stores, and legacy `state.vscdb` sessions. Not detected when
    /// discovery finds no session source.
    fn native_store_detection(&self, ctx: &ScanContext) -> DetectionResult {
        let Ok(files) = self.inner.discover_source_files(ctx) else {
            return DetectionResult::not_found();
        };
        let mut evidence = Vec::new();
        let mut root_paths: Vec<PathBuf> = Vec::new();
        for file in files.iter().filter(|file| {
            file.required_for_reconstruction && file.role != DiscoveredSourceRole::MetadataSidecar
        }) {
            if !root_paths.contains(&file.scan_root) {
                root_paths.push(file.scan_root.clone());
            }
            if evidence.len() < 8 {
                evidence.push(format!(
                    "VS Code native chat store: {}",
                    file.source_path.display()
                ));
            }
        }
        if root_paths.is_empty() {
            return DetectionResult::not_found();
        }
        DetectionResult {
            detected: true,
            evidence,
            root_paths,
        }
    }
}

impl Connector for CopilotConnector {
    /// Upstream detection (franken-agent-detection 0.3.0) probes only the
    /// Copilot Chat *extension* store (`globalStorage/github.copilot-chat`),
    /// while its scanner also reads VS Code's native chat stores, which are
    /// where current VS Code keeps chat history. A user with only native
    /// history was never detected, so the indexer skipped the connector and
    /// that history was never indexed. Detect those stores too; when neither
    /// is present the upstream (not-found) result and its evidence stand.
    fn detect(&self) -> DetectionResult {
        let upstream = self.inner.detect();
        if upstream.detected {
            return upstream;
        }
        // An empty data dir never exists, so the upstream connector resolves
        // its default per-user VS Code roots, exactly as a default scan does.
        let native = self.native_store_detection(&ScanContext::local_default(PathBuf::new(), None));
        if native.detected { native } else { upstream }
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        self.inner.scan(ctx)
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        self.inner.discover_source_files(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::ScanRoot;

    fn scan_home(home: &std::path::Path) -> ScanContext {
        ScanContext::with_roots(
            home.join("cass-data"),
            vec![ScanRoot::local(home.to_path_buf())],
            None,
        )
    }

    /// A home whose only Copilot history is VS Code's native chat store is
    /// detected, naming the session file and the root it was found under.
    #[test]
    fn native_chat_store_alone_is_detected() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home
            .path()
            .join(".config/Code/User/workspaceStorage/ws-1/chatSessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let session = sessions.join("session-1.json");
        std::fs::write(&session, br#"{"version":3,"requests":[]}"#).unwrap();

        let detection = CopilotConnector::new().native_store_detection(&scan_home(home.path()));
        assert!(detection.detected, "{detection:?}");
        assert_eq!(detection.root_paths, vec![home.path().to_path_buf()]);
        assert!(
            detection
                .evidence
                .iter()
                .any(|line| line.contains("session-1.json")),
            "{detection:?}"
        );
    }

    /// Negative controls: no store, or only a workspace.json sidecar, is not
    /// Copilot history.
    #[test]
    fn home_without_a_native_session_is_not_detected() {
        let home = tempfile::tempdir().unwrap();
        let connector = CopilotConnector::new();
        assert!(
            !connector
                .native_store_detection(&scan_home(home.path()))
                .detected
        );

        let workspace = home.path().join(".config/Code/User/workspaceStorage/ws-1");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("workspace.json"),
            br#"{"folder":"file:///w"}"#,
        )
        .unwrap();
        assert!(
            !connector
                .native_store_detection(&scan_home(home.path()))
                .detected
        );
    }

    fn empty_legacy_workspace(home: &Path) -> Result<PathBuf> {
        let workspace = home.join(".config/Code/User/workspaceStorage/empty-workspace");
        std::fs::create_dir_all(&workspace)?;
        std::fs::write(
            workspace.join("workspace.json"),
            br#"{"folder":"file:///w"}"#,
        )?;
        let database = workspace.join("state.vscdb");
        let connection = crate::franken_sync::Connection::open(database.to_str().unwrap())?;
        connection.execute("CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value BLOB)")?;
        connection
            .execute("INSERT INTO ItemTable VALUES ('unrelated.extension', 'workspace state')")?;
        connection.close()?;
        Ok(workspace)
    }

    #[test]
    fn chat_free_probe_is_read_only_and_keeps_existing_chat_keys() -> Result<()> {
        let home = tempfile::tempdir()?;
        let workspace = empty_legacy_workspace(home.path())?;
        let database = workspace.join("state.vscdb");
        let before = std::fs::read(&database)?;
        let modified = std::fs::metadata(&database)?.modified()?;
        assert!(source_is_proven_chat_free(&database));
        assert!(source_is_proven_chat_free(
            &workspace.join("workspace.json")
        ));
        assert_eq!(std::fs::read(&database)?, before);
        assert_eq!(std::fs::metadata(&database)?.modified()?, modified);

        let connection = crate::franken_sync::Connection::open(database.to_str().unwrap())?;
        connection.execute("INSERT INTO ItemTable VALUES ('interactive.sessions', NULL)")?;
        connection.close()?;
        assert!(!source_is_proven_chat_free(&database));
        assert!(!source_is_proven_chat_free(
            &workspace.join("workspace.json")
        ));

        let connection = crate::franken_sync::Connection::open(database.to_str().unwrap())?;
        connection.execute(
            "UPDATE ItemTable SET value = 'malformed chat payload' WHERE key = 'interactive.sessions'",
        )?;
        connection.close()?;
        assert!(!source_is_proven_chat_free(&database));
        Ok(())
    }

    #[test]
    fn native_transcript_keeps_its_workspace_sidecar_even_without_legacy_chat() -> Result<()> {
        let home = tempfile::tempdir()?;
        let workspace = empty_legacy_workspace(home.path())?;
        let sessions = workspace.join("chatSessions");
        std::fs::create_dir_all(&sessions)?;
        let transcript = sessions.join("unparsed.jsonl");
        std::fs::write(
            &transcript,
            b"{malformed transcript preserved before parsing\n",
        )?;
        assert!(source_is_proven_chat_free(&workspace.join("state.vscdb")));
        assert!(!source_is_proven_chat_free(
            &workspace.join("workspace.json")
        ));
        assert!(!source_is_proven_chat_free(&transcript));
        Ok(())
    }

    #[test]
    fn missing_and_corrupt_stores_never_prove_an_empty_legacy_database() -> Result<()> {
        let home = tempfile::tempdir()?;
        let path = home.path().join("state.vscdb");
        assert!(!source_is_proven_chat_free(&path));
        let bytes = b"unreadable legacy chat evidence";
        std::fs::write(&path, bytes)?;
        assert!(!source_is_proven_chat_free(&path));
        assert_eq!(std::fs::read(&path)?, bytes);
        Ok(())
    }

    #[test]
    fn committed_chat_key_in_a_live_wal_is_never_treated_as_chat_free() -> Result<()> {
        let home = tempfile::tempdir()?;
        let workspace = empty_legacy_workspace(home.path())?;
        let database = workspace.join("state.vscdb");
        let writer = crate::franken_sync::Connection::open(database.to_str().unwrap())?;
        writer.execute("PRAGMA journal_mode = WAL")?;
        writer.execute("PRAGMA wal_autocheckpoint = 0")?;
        let primary_before = std::fs::read(&database)?;
        writer.execute("INSERT INTO ItemTable VALUES ('interactive.sessions', 'unparsed chat')")?;
        let wal = workspace.join("state.vscdb-wal");
        let wal_before = std::fs::read(&wal)?;
        assert!(
            wal_before.len() > 32,
            "the provider must have a committed WAL"
        );
        assert_eq!(std::fs::read(&database)?, primary_before);
        assert!(!source_is_proven_chat_free(&database));
        assert_eq!(std::fs::read(&database)?, primary_before);
        assert_eq!(
            std::fs::read(wal)?,
            wal_before,
            "the probe must not rewrite the WAL"
        );
        writer.close()?;
        Ok(())
    }
}
