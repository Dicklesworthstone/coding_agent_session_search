//! Synchronous facade over the async FrankenSQLite 0.2 engine API.
//!
//! fsqlite 0.2 made every engine entry point `async` with `!Send` futures
//! (the engine is `Rc<RefCell<..>>` internally; it was already `!Send` at
//! 0.1.x — only the call shape changed). CASS's storage layer is fully
//! synchronous, so this module preserves the pre-0.2 blocking call shape by
//! driving each engine future to completion on the calling thread with a
//! private current-thread `asupersync` runtime (the proven
//! sqlmodel-frankensqlite `block_on` bridge pattern, sqlmodel_rust d9a3355).
//!
//! Every future is created, polled, and dropped entirely within one bridge
//! call, so the engine's `Rc<RefCell<..>>` state never crosses a thread
//! boundary between poll steps. `Runtime::block_on` has no `Send` bound and
//! saves/restores the ambient runtime handle, so nesting inside a consumer's
//! own `block_on` is safe (sqlmodel's `nested_block_on_*` probes pin this).
//!
//! The runtime lives in a thread-local slot and is *taken out* while a
//! future is being driven: a reentrant bridge call (e.g. SQL issued from
//! inside a row-mapping closure) finds the slot empty and builds a fresh
//! runtime instead of re-entering `block_on` on the same runtime instance.
//!
//! Everything outside this module refers to the engine through
//! `crate::franken_sync::` (or `coding_agent_search::franken_sync::` from
//! integration tests); only this module names the `frankensqlite` (fsqlite)
//! dependency directly for connection/transaction/statement driving.

use std::cell::RefCell;
use std::future::Future;

use asupersync::runtime::{Runtime, RuntimeBuilder};

pub use frankensqlite::{FileIdentity, FrankenError, Row, SqliteValue, fsqlite_vfs, params};

// ---------------------------------------------------------------------------
// Bridge driver
// ---------------------------------------------------------------------------

thread_local! {
    static DRIVER: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Drive a `!Send` fsqlite future to completion on the calling thread.
fn drive<T>(future: impl Future<Output = T>) -> T {
    let runtime = DRIVER
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(|| {
            RuntimeBuilder::current_thread()
                .build()
                .expect("failed to build FrankenSQLite sync-bridge runtime")
        });
    let output = runtime.block_on(future);
    DRIVER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(runtime);
        }
    });
    output
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Synchronous wrapper over [`frankensqlite::Connection`] with the pre-0.2
/// blocking method signatures.
pub struct Connection {
    inner: frankensqlite::Connection,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("path", &self.inner.path())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Open (or create) a database at `path`.
    pub fn open(path: impl Into<String>) -> Result<Self, FrankenError> {
        Ok(Self {
            inner: drive(frankensqlite::Connection::open(path))?,
        })
    }

    /// Open an existing database only (never creates), loading the schema.
    pub fn open_existing_schema_only(path: impl Into<String>) -> Result<Self, FrankenError> {
        Ok(Self {
            inner: drive(frankensqlite::Connection::open_existing_schema_only(path))?,
        })
    }

    /// Open an existing database only, deferring FTS5 shadow-table
    /// validation (corrupt-shadow repair path, cass#368 defect 3).
    pub fn open_existing_schema_only_deferred_fts5(
        path: impl Into<String>,
    ) -> Result<Self, FrankenError> {
        Ok(Self {
            inner: drive(frankensqlite::Connection::open_existing_schema_only_deferred_fts5(path))?,
        })
    }

    /// Access the wrapped async connection (escape hatch for callers that
    /// drive engine APIs this facade does not wrap).
    pub fn as_async(&self) -> &frankensqlite::Connection {
        &self.inner
    }

    /// Execute a single SQL statement, returning the affected row count.
    pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
        drive(self.inner.execute(sql))
    }

    /// Execute a single SQL statement with positional parameters.
    pub fn execute_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        drive(self.inner.execute_with_params(sql, params))
    }

    /// Execute a string of semicolon-separated SQL statements.
    pub fn execute_batch(&self, sql: &str) -> Result<(), FrankenError> {
        drive(self.inner.execute_batch(sql))
    }

    /// Query, returning all rows.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
        drive(self.inner.query(sql))
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        drive(self.inner.query_with_params(sql, params))
    }

    /// Query with positional parameters, streaming rows into `f`.
    pub fn query_with_params_for_each<F>(
        &self,
        sql: &str,
        params: &[SqliteValue],
        f: F,
    ) -> Result<(), FrankenError>
    where
        F: FnMut(&Row) -> Result<(), FrankenError>,
    {
        drive(self.inner.query_with_params_for_each(sql, params, f))
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
        drive(self.inner.query_row(sql))
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Row, FrankenError> {
        drive(self.inner.query_row_with_params(sql, params))
    }

    /// Prepare a statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement<'_>, FrankenError> {
        Ok(PreparedStatement {
            inner: drive(self.inner.prepare(sql))?,
        })
    }

    /// Last-inserted rowid on this connection.
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    /// Close the connection (rolls back any active transaction, then runs the
    /// final passive WAL checkpoint).
    pub fn close(mut self) -> Result<(), FrankenError> {
        drive(self.inner.close_in_place())
    }

    /// Close without the final WAL checkpoint (committed frames stay durable
    /// in the WAL sidecar and are recovered by the next open).
    pub fn close_without_checkpoint(mut self) -> Result<(), FrankenError> {
        drive(self.inner.close_without_checkpoint_in_place())
    }

    /// Close in place, retaining the handle on error so callers can retry.
    pub fn close_in_place(&mut self) -> Result<(), FrankenError> {
        drive(self.inner.close_in_place())
    }

    /// Close in place without the final WAL checkpoint.
    pub fn close_without_checkpoint_in_place(&mut self) -> Result<(), FrankenError> {
        drive(self.inner.close_without_checkpoint_in_place())
    }

    /// Best-effort in-place close (never fails; marks the handle closed).
    pub fn close_best_effort_in_place(&mut self) {
        drive(self.inner.close_best_effort_in_place());
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // fsqlite 0.1.x closed on drop (best-effort, no checkpoint:
        // `close_internal(true, false)`); 0.2's `Drop` cannot await and so
        // skips that teardown, and 0.2's read-only opens are mutation-free
        // (GH#294) — they no longer recover an unpublished WAL sidecar.
        // Driving the same best-effort close here restores the 0.1.x
        // observable contract that writes made through a dropped connection
        // are visible to any later open. `close_internal` is a no-op if the
        // connection was already explicitly closed.
        drive(self.inner.close_best_effort_in_place());
    }
}

// ---------------------------------------------------------------------------
// Prepared statements
// ---------------------------------------------------------------------------

/// Synchronous wrapper over [`frankensqlite::PreparedStatement`].
pub struct PreparedStatement<'conn> {
    inner: frankensqlite::PreparedStatement<'conn>,
}

impl PreparedStatement<'_> {
    /// Query, returning all rows.
    pub fn query(&self) -> Result<Vec<Row>, FrankenError> {
        drive(self.inner.query())
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(&self, params: &[SqliteValue]) -> Result<Vec<Row>, FrankenError> {
        drive(self.inner.query_with_params(params))
    }

    /// Query with positional parameters, streaming rows into `f`.
    pub fn query_with_params_for_each<F>(
        &self,
        params: &[SqliteValue],
        f: F,
    ) -> Result<(), FrankenError>
    where
        F: FnMut(&Row) -> Result<(), FrankenError>,
    {
        drive(self.inner.query_with_params_for_each(params, f))
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self) -> Result<Row, FrankenError> {
        drive(self.inner.query_row())
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(&self, params: &[SqliteValue]) -> Result<Row, FrankenError> {
        drive(self.inner.query_row_with_params(params))
    }

    /// Execute, returning the affected row count.
    pub fn execute(&self) -> Result<usize, FrankenError> {
        drive(self.inner.execute())
    }

    /// Execute with positional parameters, returning the affected row count.
    pub fn execute_with_params(&self, params: &[SqliteValue]) -> Result<usize, FrankenError> {
        drive(self.inner.execute_with_params(params))
    }
}

// ---------------------------------------------------------------------------
// compat: rusqlite-style ergonomics, synchronous form
// ---------------------------------------------------------------------------

pub mod compat {
    use super::{Connection, FrankenError, Row, SqliteValue, drive};
    use frankensqlite::compat::TransactionExt as AsyncTransactionExt;

    pub use frankensqlite::compat::{
        FromSqliteValue, OpenFlags, OptionalExtension, ParamValue, RowExt, param_slice_to_values,
        params_from_iter,
    };

    /// Open a database with rusqlite-style open flags (synchronous form of
    /// [`frankensqlite::compat::open_with_flags`]).
    pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection, FrankenError> {
        Ok(Connection {
            inner: drive(frankensqlite::compat::open_with_flags(path, flags))?,
        })
    }

    /// Synchronous form of [`frankensqlite::compat::ConnectionExt`].
    pub trait ConnectionExt {
        /// Execute a query that returns exactly one row, mapping it with `f`.
        fn query_row_map<T, F>(
            &self,
            sql: &str,
            params: &[ParamValue],
            f: F,
        ) -> Result<T, FrankenError>
        where
            F: FnOnce(&Row) -> Result<T, FrankenError>;

        /// Execute a query and collect all rows into a `Vec<T>` via `f`.
        fn query_map_collect<T, F>(
            &self,
            sql: &str,
            params: &[ParamValue],
            f: F,
        ) -> Result<Vec<T>, FrankenError>
        where
            F: FnMut(&Row) -> Result<T, FrankenError>;

        /// Execute a SQL statement with `ParamValue` parameters.
        fn execute_compat(&self, sql: &str, params: &[ParamValue]) -> Result<usize, FrankenError>;
    }

    // Mirrors upstream compat semantics: `ParamValue` unwrap plus
    // rusqlite-style row mapping.
    impl ConnectionExt for Connection {
        fn query_row_map<T, F>(
            &self,
            sql: &str,
            params: &[ParamValue],
            f: F,
        ) -> Result<T, FrankenError>
        where
            F: FnOnce(&Row) -> Result<T, FrankenError>,
        {
            let values = param_slice_to_values(params);
            let row = self.query_row_with_params(sql, &values)?;
            f(&row)
        }

        fn query_map_collect<T, F>(
            &self,
            sql: &str,
            params: &[ParamValue],
            mut f: F,
        ) -> Result<Vec<T>, FrankenError>
        where
            F: FnMut(&Row) -> Result<T, FrankenError>,
        {
            let values = param_slice_to_values(params);
            let rows = self.query_with_params(sql, &values)?;
            let mut mapped = Vec::with_capacity(rows.len());
            for row in &rows {
                mapped.push(f(row)?);
            }
            Ok(mapped)
        }

        fn execute_compat(&self, sql: &str, params: &[ParamValue]) -> Result<usize, FrankenError> {
            let values = param_slice_to_values(params);
            self.execute_with_params(sql, &values)
        }
    }

    /// Synchronous wrapper over [`frankensqlite::compat::Transaction`].
    ///
    /// The wrapped transaction's `Drop` obligation semantics are preserved:
    /// dropping without `commit()`/`rollback()` records a mandatory rollback
    /// obligation on the connection, discharged synchronously before the next
    /// statement runs (`mark_transaction_cleanup_required` is sync).
    pub struct Transaction<'conn> {
        inner: frankensqlite::compat::Transaction<'conn>,
    }

    impl Transaction<'_> {
        /// Commit the transaction.
        pub fn commit(&mut self) -> Result<(), FrankenError> {
            drive(self.inner.commit())
        }

        /// Roll back the transaction explicitly.
        pub fn rollback(&mut self) -> Result<(), FrankenError> {
            drive(self.inner.rollback())
        }

        /// Execute a SQL statement within this transaction.
        pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
            drive(self.inner.execute(sql))
        }

        /// Execute a SQL statement with positional parameters.
        pub fn execute_with_params(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<usize, FrankenError> {
            drive(self.inner.execute_with_params(sql, params))
        }

        /// Execute with positional parameters, skipping the internal statement
        /// savepoint (the transaction is the rollback boundary).
        pub fn execute_with_params_skip_statement_savepoint(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<usize, FrankenError> {
            drive(
                self.inner
                    .execute_with_params_skip_statement_savepoint(sql, params),
            )
        }

        /// Execute a SQL statement with `ParamValue` parameters.
        pub fn execute_compat(
            &self,
            sql: &str,
            params: &[ParamValue],
        ) -> Result<usize, FrankenError> {
            drive(self.inner.execute_compat(sql, params))
        }

        /// Query within this transaction.
        pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
            drive(self.inner.query(sql))
        }

        /// Query with positional parameters within this transaction.
        pub fn query_with_params(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<Vec<Row>, FrankenError> {
            drive(self.inner.query_with_params(sql, params))
        }

        /// Query with `ParamValue` parameters within this transaction.
        pub fn query_params(
            &self,
            sql: &str,
            params: &[ParamValue],
        ) -> Result<Vec<Row>, FrankenError> {
            drive(self.inner.query_params(sql, params))
        }

        /// Query returning exactly one row within this transaction.
        pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
            drive(self.inner.query_row(sql))
        }

        /// Query returning exactly one row with positional parameters.
        pub fn query_row_with_params(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<Row, FrankenError> {
            drive(self.inner.query_row_with_params(sql, params))
        }

        /// Query returning exactly one row, mapping it with `f`.
        pub fn query_row_map<T, F>(
            &self,
            sql: &str,
            params: &[ParamValue],
            f: F,
        ) -> Result<T, FrankenError>
        where
            F: FnOnce(&Row) -> Result<T, FrankenError>,
        {
            drive(self.inner.query_row_map(sql, params, f))
        }

        /// Query and collect all rows into a `Vec<T>` via `f`.
        pub fn query_map_collect<T, F>(
            &self,
            sql: &str,
            params: &[ParamValue],
            f: F,
        ) -> Result<Vec<T>, FrankenError>
        where
            F: FnMut(&Row) -> Result<T, FrankenError>,
        {
            drive(self.inner.query_map_collect(sql, params, f))
        }

        /// Execute a string of semicolon-separated SQL statements.
        pub fn execute_batch(&self, sql: &str) -> Result<(), FrankenError> {
            drive(self.inner.execute_batch(sql))
        }

        /// Last-inserted rowid within this transaction.
        pub fn last_insert_rowid(&self) -> Result<i64, FrankenError> {
            self.inner.last_insert_rowid()
        }
    }

    /// Synchronous form of [`frankensqlite::compat::TransactionExt`].
    pub trait TransactionExt {
        /// Begin a new transaction.
        fn transaction(&self) -> Result<Transaction<'_>, FrankenError>;
    }

    impl TransactionExt for Connection {
        fn transaction(&self) -> Result<Transaction<'_>, FrankenError> {
            Ok(Transaction {
                inner: drive(AsyncTransactionExt::transaction(self.as_async()))?,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// migrate: schema migration runner, synchronous form
// ---------------------------------------------------------------------------

pub mod migrate {
    use super::{Connection, FrankenError, drive};

    pub use frankensqlite::migrate::{Migration, MigrationResult};

    /// Synchronous wrapper over [`frankensqlite::migrate::MigrationRunner`].
    #[derive(Default)]
    pub struct MigrationRunner {
        inner: frankensqlite::migrate::MigrationRunner,
    }

    impl MigrationRunner {
        /// Create an empty runner.
        #[must_use]
        pub fn new() -> Self {
            Self {
                inner: frankensqlite::migrate::MigrationRunner::new(),
            }
        }

        /// Register a migration step.
        #[must_use]
        pub fn add(mut self, version: i64, name: &'static str, sql: &'static str) -> Self {
            self.inner = self.inner.add(version, name, sql);
            self
        }

        /// Run all pending migrations against `conn`.
        pub fn run(&self, conn: &Connection) -> Result<MigrationResult, FrankenError> {
            drive(self.inner.run(conn.as_async()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compat::RowExt;
    use super::*;

    #[test]
    fn multi_statement_execute_error_does_not_replay_prior_side_effects()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = Connection::open(":memory:")?;
        conn.execute("CREATE TABLE replay_guard (value INTEGER NOT NULL);")?;

        let result = conn.execute(
            "INSERT INTO replay_guard (value) VALUES (1); \
             INSERT INTO missing_replay_target (value) VALUES (2);",
        );
        assert!(result.is_err(), "missing target unexpectedly accepted");

        let count = conn
            .query_row("SELECT COUNT(*) FROM replay_guard;")?
            .get_typed::<i64>(0)?;
        assert_eq!(count, 1, "the successful prefix statement was replayed");
        Ok(())
    }

    #[test]
    fn row_callback_busy_recovery_is_propagated_exactly_once()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = Connection::open(":memory:")?;
        let mut invocations = 0_u32;

        let result = conn.query_with_params_for_each("SELECT 1;", &[], |_row| {
            invocations += 1;
            Err(FrankenError::BusyRecovery)
        });

        assert!(matches!(result, Err(FrankenError::BusyRecovery)));
        assert_eq!(invocations, 1, "row callback was replayed after its error");
        Ok(())
    }

    #[test]
    fn engine_transaction_state_covers_every_execution_surface()
    -> Result<(), Box<dyn std::error::Error>> {
        fn require(condition: bool, message: &'static str) -> Result<(), std::io::Error> {
            condition
                .then_some(())
                .ok_or_else(|| std::io::Error::other(message))
        }

        let conn = Connection::open(":memory:")?;
        require(
            !conn.as_async().in_transaction(),
            "new connection marked in transaction",
        )?;

        let invalid = conn.execute_batch("BEGIN INVALID;");
        require(invalid.is_err(), "invalid BEGIN unexpectedly succeeded")?;
        require(
            !conn.as_async().in_transaction(),
            "failed control statement changed bridge state",
        )?;

        conn.execute_batch("BEGIN IMMEDIATE TRANSACTION; CREATE TABLE batch_opened (id INTEGER);")?;
        require(
            conn.as_async().in_transaction(),
            "multi-statement batch BEGIN was not observed",
        )?;
        conn.execute("SAVEPOINT bridge_state_test")?;
        conn.execute("ROLLBACK TO bridge_state_test")?;
        require(
            conn.as_async().in_transaction(),
            "ROLLBACK TO incorrectly closed the outer transaction",
        )?;
        conn.execute("RELEASE bridge_state_test")?;
        conn.execute_batch("COMMIT;")?;
        require(
            !conn.as_async().in_transaction(),
            "COMMIT left transaction marked open",
        )?;

        conn.execute_with_params("BEGIN;", &[])?;
        require(
            conn.as_async().in_transaction(),
            "parameterized BEGIN was not observed",
        )?;
        conn.execute_with_params("ROLLBACK;", &[])?;
        require(
            !conn.as_async().in_transaction(),
            "ROLLBACK left transaction marked open",
        )?;
        Ok(())
    }
}
