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

/// True when `err` can mean the connection's schema image predates another
/// connection's DDL commit.
///
/// fsqlite 0.2.1 upstream regression (verified by standalone probe on both
/// macOS and Linux, absent at the 0.1.19 git pin): a connection opened before
/// another connection CREATEs a table does not see that table through the
/// plain `query`/`execute` paths — but `prepare()` refreshes the shared
/// schema publication before resolving, after which the same SQL succeeds.
/// The facade therefore treats these errors as possibly-stale-schema, drives
/// a `prepare()` of the same SQL to force the refresh, and retries once.
/// Plan-time resolution failures have no side effects, so the retry is safe.
fn schema_stale(err: &FrankenError) -> bool {
    matches!(
        err,
        FrankenError::NoSuchTable { .. }
            | FrankenError::NoSuchColumn { .. }
            | FrankenError::NoSuchIndex { .. }
    )
}

/// Bounded retry for `FrankenError::BusyRecovery`.
///
/// fsqlite 0.2's ns-lifecycle opens can put a database into a short
/// "recovery in progress" window; statements admitted during that window
/// fail with `BusyRecovery` immediately instead of waiting out the
/// connection's busy timeout. C SQLite's busy handler covers
/// `SQLITE_BUSY_RECOVERY`, and the 0.1.x line had no recovery windows at
/// all, so a bounded caller-side retry restores the pre-0.2 observable
/// behavior. Plain `Busy` is deliberately NOT retried here: cass classifies
/// ordinary lock contention itself and the engine owns that timeout.
fn retry_busy_recovery<T>(
    mut attempt: impl FnMut() -> Result<T, FrankenError>,
) -> Result<T, FrankenError> {
    const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    const BACKOFF_CAP: std::time::Duration = std::time::Duration::from_millis(250);
    let start = std::time::Instant::now();
    let mut backoff = std::time::Duration::from_millis(5);
    loop {
        match attempt() {
            Err(FrankenError::BusyRecovery) if start.elapsed() < RETRY_BUDGET => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
            other => return other,
        }
    }
}

macro_rules! with_engine_retries {
    ($conn:expr, $sql:expr, $attempt:expr) => {{
        let first = retry_busy_recovery(|| $attempt);
        match first {
            Err(ref err) if schema_stale(err) => {
                // `prepare` refreshes the schema image from the shared
                // publication plane even when it ultimately fails to resolve.
                let _ = drive($conn.prepare($sql));
                retry_busy_recovery(|| $attempt)
            }
            other => other,
        }
    }};
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
        with_engine_retries!(self.inner, sql, drive(self.inner.execute(sql)))
    }

    /// Execute a single SQL statement with positional parameters.
    pub fn execute_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.execute_with_params(sql, params))
        )
    }

    /// Execute a string of semicolon-separated SQL statements.
    pub fn execute_batch(&self, sql: &str) -> Result<(), FrankenError> {
        drive(self.inner.execute_batch(sql))
    }

    /// Query, returning all rows.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
        with_engine_retries!(self.inner, sql, drive(self.inner.query(sql)))
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.query_with_params(sql, params))
        )
    }

    /// Query with positional parameters, streaming rows into `f`.
    pub fn query_with_params_for_each<F>(
        &self,
        sql: &str,
        params: &[SqliteValue],
        mut f: F,
    ) -> Result<(), FrankenError>
    where
        F: FnMut(&Row) -> Result<(), FrankenError>,
    {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.query_with_params_for_each(sql, params, &mut f))
        )
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
        with_engine_retries!(self.inner, sql, drive(self.inner.query_row(sql)))
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Row, FrankenError> {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.query_row_with_params(sql, params))
        )
    }

    /// Prepare a statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement<'_>, FrankenError> {
        Ok(PreparedStatement {
            inner: retry_busy_recovery(|| drive(self.inner.prepare(sql)))?,
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

    // Implemented over the facade's own retrying primitives (not the async
    // `compat::ConnectionExt`) so these paths inherit the stale-schema
    // prepare-refresh retry. Mirrors upstream compat semantics: `ParamValue`
    // unwrap + rusqlite-style row mapping.
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
            super::retry_busy_recovery(|| drive(self.inner.commit()))
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
                inner: super::retry_busy_recovery(|| {
                    drive(AsyncTransactionExt::transaction(self.as_async()))
                })?,
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
