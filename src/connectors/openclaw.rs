//! Native and legacy OpenClaw history, parsed by franken-agent-detection.
//!
//! FAD 0.3.0's SQLite mtime prefilter ignores WAL-only commits. Recover only
//! those skipped stores here; do not fork its schema admission or event parser.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use franken_agent_detection::connectors::file_modified_since;
use franken_agent_detection::{
    Connector, DetectionResult, DiscoveredSourceFile, DiscoveredSourceRole,
    NormalizedConversation, ScanContext, ScanRoot,
};

#[derive(Default)]
pub struct OpenClawConnector;

impl OpenClawConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    fn wal_only_sources(ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        if ctx.since_ts.is_none() {
            return Ok(Vec::new());
        }
        let mut full = ctx.clone();
        full.since_ts = None;
        Ok(franken_agent_detection::OpenClawConnector::new()
            .discover_source_files(&full)?
            .into_iter()
            .filter(|source| {
                let wal = wal_path(&source.source_path);
                source.role == DiscoveredSourceRole::SqliteDatabase
                    && !file_modified_since(&source.source_path, ctx.since_ts)
                    && wal.is_file()
                    && file_modified_since(&wal, ctx.since_ts)
            })
            .collect())
    }
}

fn wal_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push("-wal");
    PathBuf::from(path)
}

impl Connector for OpenClawConnector {
    fn detect(&self) -> DetectionResult {
        franken_agent_detection::OpenClawConnector::new().detect()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let inner = franken_agent_detection::OpenClawConnector::new();
        let mut sources = inner.discover_source_files(ctx)?;
        let mut paths: HashSet<_> = sources.iter().map(|s| s.source_path.clone()).collect();
        for source in Self::wal_only_sources(ctx)? {
            if paths.insert(source.source_path.clone()) {
                sources.push(source);
            }
        }
        // The committed WAL is part of the source, not merely a cache. Declare
        // it for pre-mirroring even on full scans; never treat SHM as history.
        let mut sidecars = Vec::new();
        for source in &sources {
            if source.role == DiscoveredSourceRole::SqliteDatabase {
                let wal = wal_path(&source.source_path);
                if wal.is_file() && paths.insert(wal.clone()) {
                    sidecars.push(DiscoveredSourceFile {
                        source_path: wal,
                        role: DiscoveredSourceRole::MetadataSidecar,
                        required_for_reconstruction: true,
                        ..source.clone()
                    }.with_fs_metadata());
                }
            }
        }
        sources.extend(sidecars);
        Ok(sources)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        let inner = franken_agent_detection::OpenClawConnector::new();
        let pending = Self::wal_only_sources(ctx)?;
        let mut conversations = inner.scan(ctx)?;
        for source in pending {
            let mut scoped = ctx.clone();
            scoped.since_ts = None;
            // Only the selected database is reopened, never adjacent stores.
            let root = ctx.scan_roots.iter()
                .find(|root| source.source_path.starts_with(&root.path))
                .cloned()
                .unwrap_or_else(|| ScanRoot::local(source.source_path.clone()));
            scoped.scan_roots = vec![root.with_path(source.source_path.clone())];
            let native = inner.scan(&scoped)?;
            if native.is_empty() {
                // An unreadable/unrecognized store must not suppress legacy
                // fallback or any results already returned by the dependency.
                continue;
            }
            let native_ids: HashSet<_> = native.iter()
                .filter_map(|c| c.external_id.as_deref()).collect();
            let legacy_root = source.source_path.parent()
                .filter(|parent| parent.file_name().is_some_and(|name| name == "agent"))
                .and_then(Path::parent).map(|agent| agent.join("sessions"));
            // Prefer authoritative native history over migration leftovers,
            // including stale sessions that should not be re-emitted. Scope
            // suppression to this physical agent directory, not another mirror.
            conversations.retain(|conversation| {
                let same_store = conversation.source_path == source.source_path;
                let legacy_copy = legacy_root.as_ref().is_some_and(|root| {
                    conversation.source_path.starts_with(root)
                        && conversation.external_id.as_deref().is_some_and(|id| native_ids.contains(id))
                });
                !same_store && !legacy_copy
            });
            conversations.extend(native.into_iter().filter(|conversation| {
                // Match FAD's session-level cutoff and clock-skew allowance.
                // Keep every event of a selected session, not only new messages.
                match (ctx.since_ts, conversation.metadata.get("last_event_at").and_then(|v| v.as_i64())) {
                    (Some(since), Some(last)) => last >= since.saturating_sub(1_000),
                    _ => true,
                }
            }));
        }
        Ok(conversations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::franken_sync::Connection;
    use crate::franken_sync::compat::{ConnectionExt, ParamValue};
    use serde_json::json;
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    const OLD: i64 = 1_780_000_000_000;
    const NEW: i64 = OLD + 60_000;

    fn insert(conn: &Connection, id: &str, seq: i64, at: i64, text: &str) -> Result<()> {
        let body = json!({"type":"message", "message":{"role":"user", "content":text}}).to_string();
        conn.execute_compat("INSERT INTO transcript_events VALUES (?1, ?2, ?3, ?4)", &[
            ParamValue::from(id), ParamValue::from(seq), ParamValue::from(body.as_str()), ParamValue::from(at),
        ])?;
        Ok(())
    }

    #[test]
    fn incremental_wal_updates_preserve_prefixes_and_suppress_stale_migration_copies() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let agent = temp.path().join(".openclaw/agents/main");
        fs::create_dir_all(agent.join("agent"))?;
        fs::create_dir_all(agent.join("sessions"))?;
        let database = agent.join("agent/openclaw-agent.sqlite");
        let conn = Connection::open(database.to_string_lossy().into_owned())?;
        conn.execute("PRAGMA journal_mode = DELETE")?;
        conn.execute("CREATE TABLE transcript_events (session_id TEXT, seq INTEGER, event_json TEXT, created_at INTEGER)")?;
        insert(&conn, "fresh", 0, OLD, "original prefix")?;
        insert(&conn, "stale", 0, OLD, "native stale history")?;
        conn.close()?;
        let writer = Connection::open(database.to_string_lossy().into_owned())?;
        writer.execute("PRAGMA journal_mode = WAL")?;
        insert(&writer, "fresh", 1, NEW, "WAL append")?;
        writer.close_without_checkpoint()?;
        fs::File::options().write(true).open(&database)?
            .set_modified(UNIX_EPOCH + Duration::from_millis(OLD as u64))?;
        let wal = wal_path(&database);
        assert!(fs::metadata(&wal)?.len() > 32);
        for name in ["fresh", "stale", "legacy-only"] {
            let body = json!({"type":"message", "message":{"role":"user", "content":format!("legacy {name}")}});
            fs::write(agent.join("sessions").join(format!("{name}.jsonl")), format!("{body}\n"))?;
        }
        let before = (fs::read(&database)?, fs::read(&wal)?, fs::metadata(&database)?.modified()?, fs::metadata(&wal)?.modified()?);
        let ctx = ScanContext::with_roots(temp.path().join("cass"), vec![ScanRoot::local(temp.path().to_path_buf())], Some(NEW - 1_000));
        assert!(!file_modified_since(&database, ctx.since_ts), "fixture must exercise the stale database gate");
        let connector = OpenClawConnector::new();
        let sources = connector.discover_source_files(&ctx)?;
        assert!(sources.iter().any(|s| s.source_path == database && s.role == DiscoveredSourceRole::SqliteDatabase));
        assert!(sources.iter().any(|s| s.source_path == wal && s.role == DiscoveredSourceRole::MetadataSidecar && s.required_for_reconstruction));
        let conversations = connector.scan(&ctx)?;
        assert_eq!(conversations.len(), 2);
        let fresh = conversations.iter().find(|c| c.external_id.as_deref() == Some("main/fresh")).unwrap();
        assert_eq!(fresh.source_path, database);
        assert_eq!(fresh.messages.len(), 2);
        assert_eq!(fresh.messages[0].content, "original prefix");
        assert_eq!(fresh.messages[1].content, "WAL append");
        assert!(conversations.iter().any(|c| c.external_id.as_deref() == Some("main/legacy-only")));
        assert_eq!((fs::read(&database)?, fs::read(&wal)?, fs::metadata(&database)?.modified()?, fs::metadata(&wal)?.modified()?), before);
        Ok(())
    }

    #[test]
    fn wal_sidecar_suffix_preserves_non_extension_path_identity() {
        assert_eq!(wal_path(Path::new("agent/openclaw-agent.sqlite")), PathBuf::from("agent/openclaw-agent.sqlite-wal"));
        assert_eq!(wal_path(Path::new("agent/history")), PathBuf::from("agent/history-wal"));
    }
}
