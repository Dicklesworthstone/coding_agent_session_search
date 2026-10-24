//! Connector for OpenCode session logs.
//!
//! Implementation lives in `franken_agent_detection::connectors::opencode`.

pub use franken_agent_detection::OpenCodeConnector;

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
