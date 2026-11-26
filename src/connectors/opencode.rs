//! Connector for OpenCode session logs.
//!
//! Implementation lives in `franken_agent_detection::connectors::opencode`.

use std::borrow::Cow;

use anyhow::{Context, Result};
use franken_agent_detection::{
    Connector, DetectionResult, DiscoveredSourceFile, LOCAL_SOURCE_ID, NormalizedConversation,
    OpenCodeConnector as UpstreamOpenCode, ScanContext, SourceKind,
};

#[derive(Default)]
pub struct OpenCodeConnector;

impl OpenCodeConnector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

// FAD's OpenCode reader intentionally adds the machine's default databases
// for local roots (#174). A named local source owns only its configured paths:
// the indexer attributes the entire scan to that source (#578). The pinned
// reader exposes its explicit-roots-only mode through non-local origin kind.
// Adapt that input privately, and restore the declared origin on discovery.
// The reader only accesses local paths in both modes.
fn parser_context(ctx: &ScanContext) -> Cow<'_, ScanContext> {
    // A mixed request may legitimately include the default local database.
    // FAD already disables default discovery when any real SSH root is present.
    if ctx.scan_roots.is_empty()
        || ctx.scan_roots.iter().any(|root| {
            root.origin.kind != SourceKind::Local || root.origin.source_id == LOCAL_SOURCE_ID
        })
    {
        return Cow::Borrowed(ctx);
    }
    let mut scoped = ctx.clone();
    // Upstream probes data_dir ahead of the configured roots. Passing it
    // separately would rediscover an explicit DB with default provenance.
    scoped.data_dir.clear();
    for root in &mut scoped.scan_roots {
        root.origin.kind = SourceKind::Ssh;
    }
    Cow::Owned(scoped)
}

impl Connector for OpenCodeConnector {
    fn detect(&self) -> DetectionResult {
        UpstreamOpenCode::new().detect()
    }

    fn discover_source_files(&self, ctx: &ScanContext) -> Result<Vec<DiscoveredSourceFile>> {
        let selected = parser_context(ctx);
        let mut sources = UpstreamOpenCode::new().discover_source_files(&selected)?;
        if matches!(selected, Cow::Owned(_)) {
            for source in &mut sources {
                let root = ctx
                    .scan_roots
                    .iter()
                    .find(|root| {
                        root.origin.source_id == source.origin.source_id
                            && source.source_path.starts_with(&root.path)
                    })
                    .context("OpenCode discovered a source outside its configured roots")?;
                source.origin = root.origin.clone();
            }
        }
        Ok(sources)
    }

    fn scan(&self, ctx: &ScanContext) -> Result<Vec<NormalizedConversation>> {
        UpstreamOpenCode::new().scan(&parser_context(ctx))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::Context;
    use asupersync::runtime::{Runtime, RuntimeHandle};
    use franken_agent_detection::connectors::sqlite_sync::{Connection, ConnectionExt};

    use crate::franken_sync::compat::RowExt;

    fn connection_with_observed_runtime() -> anyhow::Result<(Connection, RuntimeHandle)> {
        let connection = Connection::open(":memory:")?;
        connection.execute_batch(
            "CREATE TABLE messages (body TEXT); INSERT INTO messages VALUES ('complete');",
        )?;
        // The row mapper runs inside FAD's actual bridge, so this is its
        // runtime, not a separate runtime constructed by the test.
        let (body, runtime) =
            connection.query_row_map("SELECT body FROM messages", &[], |row| {
                Ok((row.get_typed::<String>(0)?, Runtime::current_handle()))
            })?;
        assert_eq!(body, "complete");
        let runtime = runtime.context("FAD query must install its bridge runtime")?;
        Ok((connection, runtime))
    }

    fn weak_worker_runtime_handle(runtime: RuntimeHandle) -> anyhow::Result<RuntimeHandle> {
        let (sender, receiver) = mpsc::channel();
        // A handle captured inside block_on is strong. An Asupersync native
        // worker's ambient handle is weak, so it can witness runtime death
        // without itself keeping the runtime alive. Spawn after the DB call
        // has returned, when this task can only run on the background worker.
        let task = runtime.try_spawn(async move {
            let _sent = sender.send((std::thread::current().id(), Runtime::current_handle()));
        })?;
        drop(runtime);
        let (worker_id, runtime) = receiver.recv_timeout(Duration::from_secs(10))?;
        drop(task);
        assert_ne!(
            worker_id,
            std::thread::current().id(),
            "the lifecycle witness must come from the real scheduler worker"
        );
        runtime.context("FAD scheduler worker must install its runtime handle")
    }

    fn assert_fad_runtime_drained(runtime: &RuntimeHandle) {
        assert!(
            runtime.trace_snapshot().is_some(),
            "the FAD runtime must be alive before CASS cleanup"
        );
        assert!(crate::shutdown_thread_local_bridge_runtimes());
        assert!(
            runtime.trace_snapshot().is_none(),
            "CASS cleanup must destroy FAD's cached runtime before thread exit"
        );
        assert!(
            crate::shutdown_thread_local_bridge_runtimes(),
            "cleanup must be idempotent when the caches are empty"
        );
    }

    #[test]
    fn sqlite_export_cleanup_destroys_fad_runtime_and_allows_recreation() -> anyhow::Result<()> {
        let (connection, runtime) = connection_with_observed_runtime()?;
        drop(connection);
        let first_runtime = weak_worker_runtime_handle(runtime)?;
        assert_fad_runtime_drained(&first_runtime);

        // This real query must work after cleanup, with a fresh live runtime;
        // the previously retired runtime must remain unavailable.
        let (connection, runtime) = connection_with_observed_runtime()?;
        drop(connection);
        let second_runtime = weak_worker_runtime_handle(runtime)?;
        assert!(first_runtime.trace_snapshot().is_none());
        assert_fad_runtime_drained(&second_runtime);
        Ok(())
    }

    #[test]
    fn sqlite_export_cleanup_destroys_fad_runtime_after_failed_read() -> anyhow::Result<()> {
        let (connection, runtime) = connection_with_observed_runtime()?;
        let read = connection.query_row_map("SELECT body FROM missing_messages", &[], |row| {
            row.get_typed::<String>(0)
        });
        assert!(read.is_err(), "the database read must actually fail");
        drop(connection);
        let runtime = weak_worker_runtime_handle(runtime)?;
        assert_fad_runtime_drained(&runtime);
        Ok(())
    }
}
