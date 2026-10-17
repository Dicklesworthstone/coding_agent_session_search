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
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use asupersync::runtime::{Runtime, RuntimeBuilder};
use fsqlite_types::cx::{CancelReason, LocalCancelRelay};

pub use frankensqlite::{FileIdentity, FrankenError, Row, SqliteValue, fsqlite_vfs, params};

// ---------------------------------------------------------------------------
// Bridge driver
// ---------------------------------------------------------------------------

thread_local! {
    static DRIVER: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static CANCELLATION: RefCell<Option<CancellationHandle>> = const { RefCell::new(None) };
}

/// Cancellation authority for synchronous SQL issued inside an explicit scope.
///
/// The caller may request cancellation from its signal/owner thread while a
/// thread-affine connection is executing. Only engine-derived operation
/// contexts are cancelled: the connection root and its native runtime remain
/// available for transaction rollback and close. Every admitted engine future
/// is still driven to completion, including mandatory cancellation cleanup.
#[derive(Clone, Debug, Default)]
pub(crate) struct CancellationHandle {
    inner: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    active: Mutex<Vec<Arc<LocalCancelRelay>>>,
}

impl CancellationHandle {
    /// Latch cancellation and notify every currently admitted SQL operation.
    /// Registration rechecks the latch, so a request cannot be lost between
    /// observing the scope and publishing an operation's relay.
    pub(crate) fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        let active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Cancellation invokes arbitrary engine/runtime wakers. Never run
        // them under the registry lock: a woken operation may finish and
        // unregister immediately on its owning thread.
        for relay in active {
            let _ = relay.cancel_local(CancelReason::UserInterrupt);
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Activate cancellation for SQL on this thread until the guard drops.
    /// Nested scopes restore their caller's handle. The guard must stay on
    /// its originating thread, just like the raw connections it protects.
    pub(crate) fn enter(&self) -> CancellationScope {
        let previous = CANCELLATION.with(|slot| slot.replace(Some(self.clone())));
        CancellationScope {
            previous,
            _thread_affine: PhantomData,
        }
    }

    fn register(&self, relay: LocalCancelRelay) -> CancellationRegistration {
        let relay = Arc::new(relay);
        let cancelled = {
            let mut active = self
                .inner
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.push(Arc::clone(&relay));
            self.is_cancelled()
        };
        let registration = CancellationRegistration {
            handle: self.clone(),
            relay,
        };
        if cancelled {
            let _ = registration.relay.cancel_local(CancelReason::UserInterrupt);
        }
        registration
    }
}

pub(crate) struct CancellationScope {
    previous: Option<CancellationHandle>,
    _thread_affine: PhantomData<Rc<()>>,
}

impl Drop for CancellationScope {
    fn drop(&mut self) {
        CANCELLATION.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

struct CancellationRegistration {
    handle: CancellationHandle,
    relay: Arc<LocalCancelRelay>,
}

impl Drop for CancellationRegistration {
    fn drop(&mut self) {
        self.handle
            .inner
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|relay| !Arc::ptr_eq(relay, &self.relay));
    }
}

/// Refuse new connection admission after a scoped stop has been requested.
/// Existing constructor futures still run to completion: the engine does
/// not expose a cancellable bootstrap context on its default runtime.
fn check_connection_admission() -> Result<(), FrankenError> {
    if CANCELLATION.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(CancellationHandle::is_cancelled)
    }) {
        Err(FrankenError::Abort)
    } else {
        Ok(())
    }
}

pub(crate) fn shutdown_driver() -> bool {
    DRIVER
        .with(|slot| slot.borrow_mut().take())
        .is_none_or(|runtime| runtime.shutdown_timeout(std::time::Duration::from_secs(30)))
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
    fn drive_operation<T>(
        &self,
        future: impl Future<Output = Result<T, FrankenError>>,
    ) -> Result<T, FrankenError> {
        let cancellation = CANCELLATION.with(|slot| slot.borrow().clone());
        let Some(cancellation) = cancellation else {
            return drive(future);
        };
        if cancellation.is_cancelled() {
            return Err(FrankenError::Abort);
        }
        let (operation, relay) = self.inner.root_cx().create_child_with_local_cancel_relay();
        let _registration = cancellation.register(relay);
        let _binding = self.inner.bind_operation_cx(&operation);
        // Keep polling through cancellation. Dropping a pending engine
        // future here could abandon a transaction or an in-flight I/O lease.
        drive(future)
    }

    /// Open (or create) a database at `path`.
    pub fn open(path: impl Into<String>) -> Result<Self, FrankenError> {
        check_connection_admission()?;
        Ok(Self {
            inner: drive(frankensqlite::Connection::open(path))?,
        })
    }

    /// Open read-only while permitting derived WAL-index recovery under the
    /// engine's recovery locks. Database/WAL writes remain forbidden.
    pub fn open_schema_only_with_wal_index_recovery(
        path: impl Into<String>,
    ) -> Result<Self, FrankenError> {
        check_connection_admission()?;
        Ok(Self {
            inner: drive(
                frankensqlite::Connection::open_schema_only_with_wal_index_recovery(path),
            )?,
        })
    }

    /// Open an existing database only (never creates), loading the schema.
    pub fn open_existing_schema_only(path: impl Into<String>) -> Result<Self, FrankenError> {
        check_connection_admission()?;
        Ok(Self {
            inner: drive(frankensqlite::Connection::open_existing_schema_only(path))?,
        })
    }

    /// Open an existing database only, deferring FTS5 shadow-table
    /// validation (corrupt-shadow repair path, cass#368 defect 3).
    pub fn open_existing_schema_only_deferred_fts5(
        path: impl Into<String>,
    ) -> Result<Self, FrankenError> {
        check_connection_admission()?;
        Ok(Self {
            inner: drive(frankensqlite::Connection::open_existing_schema_only_deferred_fts5(path))?,
        })
    }

    /// Access the wrapped async connection (escape hatch for callers that
    /// drive engine APIs this facade does not wrap).
    pub fn as_async(&self) -> &frankensqlite::Connection {
        &self.inner
    }

    /// Descriptor-bound identity, distinct from a possibly replaced pathname.
    pub fn file_identity(&self) -> Result<Option<FileIdentity>, FrankenError> {
        self.drive_operation(self.inner.file_identity())
    }

    /// Execute a single SQL statement, returning the affected row count.
    pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
        self.drive_operation(self.inner.execute(sql))
    }

    /// Execute a single SQL statement with positional parameters.
    pub fn execute_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        self.drive_operation(self.inner.execute_with_params(sql, params))
    }

    /// Execute a string of semicolon-separated SQL statements.
    pub fn execute_batch(&self, sql: &str) -> Result<(), FrankenError> {
        self.drive_operation(self.inner.execute_batch(sql))
    }

    /// Query, returning all rows.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
        self.drive_operation(self.inner.query(sql))
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        self.drive_operation(self.inner.query_with_params(sql, params))
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
        self.drive_operation(self.inner.query_with_params_for_each(sql, params, f))
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
        self.drive_operation(self.inner.query_row(sql))
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Row, FrankenError> {
        self.drive_operation(self.inner.query_row_with_params(sql, params))
    }

    /// Prepare a statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement<'_>, FrankenError> {
        Ok(PreparedStatement {
            inner: self.drive_operation(self.inner.prepare(sql))?,
            connection: self,
        })
    }

    /// Last-inserted rowid on this connection.
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    /// Return every in-range page that neither a b-tree nor the durable
    /// freelist owns (integrity_check's "page N is never used" class) to the
    /// freelist through a normal write commit. Returns the pages freed. Run
    /// only at a quiescent point: no other writer may be active.
    pub fn repair_orphaned_pages(&self) -> Result<usize, FrankenError> {
        self.drive_operation(self.inner.repair_orphaned_pages())
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
    connection: &'conn Connection,
}

impl PreparedStatement<'_> {
    /// Query, returning all rows.
    pub fn query(&self) -> Result<Vec<Row>, FrankenError> {
        self.connection.drive_operation(self.inner.query())
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(&self, params: &[SqliteValue]) -> Result<Vec<Row>, FrankenError> {
        self.connection
            .drive_operation(self.inner.query_with_params(params))
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
        self.connection
            .drive_operation(self.inner.query_with_params_for_each(params, f))
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self) -> Result<Row, FrankenError> {
        self.connection.drive_operation(self.inner.query_row())
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(&self, params: &[SqliteValue]) -> Result<Row, FrankenError> {
        self.connection
            .drive_operation(self.inner.query_row_with_params(params))
    }

    /// Execute, returning the affected row count.
    pub fn execute(&self) -> Result<usize, FrankenError> {
        self.connection.drive_operation(self.inner.execute())
    }

    /// Execute with positional parameters, returning the affected row count.
    pub fn execute_with_params(&self, params: &[SqliteValue]) -> Result<usize, FrankenError> {
        self.connection
            .drive_operation(self.inner.execute_with_params(params))
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
        super::check_connection_admission()?;
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

        /// Stream rows through `f` and collect the mapped values.
        ///
        /// The facade does not retain a second `Vec<Row>` containing every
        /// original text/blob payload. The returned `Vec<T>` and any buffers
        /// required by the engine's query plan still consume memory.
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
            let mut mapped = Vec::new();
            self.query_with_params_for_each(sql, &values, |row| {
                mapped.push(f(row)?);
                Ok(())
            })?;
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
        connection: &'conn Connection,
    }

    impl Transaction<'_> {
        /// Commit the transaction.
        pub fn commit(&mut self) -> Result<(), FrankenError> {
            self.connection.drive_operation(self.inner.commit())
        }

        /// Roll back the transaction explicitly.
        pub fn rollback(&mut self) -> Result<(), FrankenError> {
            // Explicit rollback is mandatory cleanup, including after the
            // scope has latched cancellation. It uses the healthy root Cx.
            drive(self.inner.rollback())
        }

        /// Execute a SQL statement within this transaction.
        pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
            self.connection.drive_operation(self.inner.execute(sql))
        }

        /// Execute a SQL statement with positional parameters.
        pub fn execute_with_params(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<usize, FrankenError> {
            self.connection
                .drive_operation(self.inner.execute_with_params(sql, params))
        }

        /// Execute with positional parameters, skipping the internal statement
        /// savepoint (the transaction is the rollback boundary).
        pub fn execute_with_params_skip_statement_savepoint(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<usize, FrankenError> {
            self.connection.drive_operation(
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
            self.connection
                .drive_operation(self.inner.execute_compat(sql, params))
        }

        /// Query within this transaction.
        pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
            self.connection.drive_operation(self.inner.query(sql))
        }

        /// Query with positional parameters within this transaction.
        pub fn query_with_params(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<Vec<Row>, FrankenError> {
            self.connection
                .drive_operation(self.inner.query_with_params(sql, params))
        }

        /// Query with `ParamValue` parameters within this transaction.
        pub fn query_params(
            &self,
            sql: &str,
            params: &[ParamValue],
        ) -> Result<Vec<Row>, FrankenError> {
            self.connection
                .drive_operation(self.inner.query_params(sql, params))
        }

        /// Query returning exactly one row within this transaction.
        pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
            self.connection.drive_operation(self.inner.query_row(sql))
        }

        /// Query returning exactly one row with positional parameters.
        pub fn query_row_with_params(
            &self,
            sql: &str,
            params: &[SqliteValue],
        ) -> Result<Row, FrankenError> {
            self.connection
                .drive_operation(self.inner.query_row_with_params(sql, params))
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
            self.connection
                .drive_operation(self.inner.query_row_map(sql, params, f))
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
            self.connection
                .drive_operation(self.inner.query_map_collect(sql, params, f))
        }

        /// Execute a string of semicolon-separated SQL statements.
        pub fn execute_batch(&self, sql: &str) -> Result<(), FrankenError> {
            self.connection
                .drive_operation(self.inner.execute_batch(sql))
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
                inner: self.drive_operation(AsyncTransactionExt::transaction(self.as_async()))?,
                connection: self,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// migrate: schema migration runner, synchronous form
// ---------------------------------------------------------------------------

pub mod migrate {
    use super::{Connection, FrankenError};

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
            conn.drive_operation(self.inner.run(conn.as_async()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compat::{RowExt, TransactionExt};
    use super::*;
    use asupersync::{Cx, cx::CapMask};

    #[test]
    fn cancellation_before_open_preserves_files_and_runtime()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let existing = directory.path().join("existing.db");
        let missing = directory.path().join("not-admitted.db");
        let conn = Connection::open(existing.to_string_lossy())?;
        conn.execute("CREATE TABLE committed_values(value INTEGER)")?;
        conn.execute("INSERT INTO committed_values VALUES(7)")?;
        conn.close()?;

        let cancellation = CancellationHandle::default();
        cancellation.cancel();
        {
            let _scope = cancellation.enter();
            assert!(matches!(
                Connection::open(missing.to_string_lossy()),
                Err(FrankenError::Abort)
            ));
            assert!(matches!(
                compat::open_with_flags(
                    missing.to_string_lossy().as_ref(),
                    compat::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | compat::OpenFlags::SQLITE_OPEN_CREATE,
                ),
                Err(FrankenError::Abort)
            ));
            assert!(
                !missing.exists(),
                "a cancelled open must not create a database"
            );
            assert!(matches!(
                Connection::open_existing_schema_only(existing.to_string_lossy()),
                Err(FrankenError::Abort)
            ));
            assert!(matches!(
                Connection::open_existing_schema_only_deferred_fts5(existing.to_string_lossy()),
                Err(FrankenError::Abort)
            ));
            assert!(matches!(
                Connection::open_schema_only_with_wal_index_recovery(existing.to_string_lossy()),
                Err(FrankenError::Abort)
            ));
        }

        let reopened = Connection::open(existing.to_string_lossy())?;
        assert_eq!(
            reopened
                .query_row("SELECT value FROM committed_values")?
                .get_typed::<i64>(0)?,
            7,
            "local cancellation must leave the shared runtime and committed data healthy"
        );
        reopened.close()?;
        Ok(())
    }

    #[test]
    fn cancellation_scope_is_latched_and_rollback_preserves_connection()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = Connection::open(":memory:")?;
        conn.execute("CREATE TABLE cancellation_values(value INTEGER)")?;
        let prepared = conn.prepare("SELECT COUNT(*) FROM cancellation_values")?;
        let mut transaction = conn.transaction()?;
        transaction.execute("INSERT INTO cancellation_values VALUES(1)")?;
        let handle = CancellationHandle::default();
        handle.cancel();
        {
            let _scope = handle.enter();
            assert!(matches!(conn.query("SELECT 1"), Err(FrankenError::Abort)));
            assert!(matches!(prepared.query(), Err(FrankenError::Abort)));
            assert!(matches!(conn.prepare("SELECT 1"), Err(FrankenError::Abort)));
            assert!(matches!(
                transaction.execute("INSERT INTO cancellation_values VALUES(2)"),
                Err(FrankenError::Abort)
            ));
            transaction.rollback()?;
        }
        drop(transaction);
        assert_eq!(prepared.query_row()?.get_typed::<i64>(0)?, 0);
        conn.execute("INSERT INTO cancellation_values VALUES(3)")?;
        assert_eq!(prepared.query_row()?.get_typed::<i64>(0)?, 1);
        assert!(handle.inner.active.lock().unwrap().is_empty());
        drop(prepared);
        conn.close()?;
        Ok(())
    }

    #[test]
    fn cancellation_scopes_restore_parent_without_cancelling_native_runtime()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = Connection::open(":memory:")?;
        let outer = CancellationHandle::default();
        let inner = CancellationHandle::default();
        {
            let _outer_scope = outer.enter();
            outer.cancel();
            {
                let _inner_scope = inner.enter();
                assert_eq!(conn.query_row("SELECT 42")?.get_typed::<i64>(0)?, 42);
            }
            assert!(matches!(conn.query("SELECT 1"), Err(FrankenError::Abort)));
        }
        assert_eq!(conn.query_row("SELECT 43")?.get_typed::<i64>(0)?, 43);
        assert!(!inner.is_cancelled());
        conn.close()?;
        Ok(())
    }

    #[test]
    fn cancellation_stops_admitted_busy_writer_and_allows_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const CHILD_ENV: &str = "CASS_TEST_SQL_CANCELLATION_CHILD";
        const TEST_NAME: &str =
            "franken_sync::tests::cancellation_stops_admitted_busy_writer_and_allows_recovery";
        if !matches!(std::env::var(CHILD_ENV).as_deref(), Ok("1")) {
            struct Child(std::process::Child);
            impl Drop for Child {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }

            // A lost engine wake must fail the regression without stranding
            // its SQL owner thread. Only this supervisor enforces a deadline;
            // the child keeps driving admitted futures through cleanup.
            let directory = tempfile::tempdir()?;
            let log_path = directory.path().join("sql-cancellation.log");
            let log = std::fs::File::create(&log_path)?;
            let mut child = Child(
                std::process::Command::new(std::env::current_exe()?)
                    .args(["--exact", TEST_NAME, "--test-threads=1", "--nocapture"])
                    .env(CHILD_ENV, "1")
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .spawn()?,
            );
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(status) = child.0.try_wait()? {
                    let output = std::fs::read_to_string(&log_path)?;
                    assert!(
                        status.success() && output.contains("1 passed"),
                        "SQL cancellation child failed ({status}):\n{output}"
                    );
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "SQL cancellation child exceeded 60 seconds:\n{}",
                        std::fs::read_to_string(&log_path)?
                    )
                    .into());
                }
                std::thread::park_timeout(Duration::from_millis(10));
            }
        }

        // Scoped ownership joins the SQL worker even when an earlier setup or
        // channel operation fails. If engine cleanup itself cannot settle,
        // the owning supervisor above kills and reaps this entire process.
        // Keep the archive directory outside the scope closure: on an early
        // error its locals drop before the scope joins the SQL owner.
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("cancellation.db");
        std::thread::scope(|scope| -> Result<(), Box<dyn std::error::Error>> {
            let blocker = Connection::open(path.to_string_lossy().into_owned())?;
            blocker.execute("PRAGMA fsqlite.concurrent_mode = OFF")?;
            blocker.execute("CREATE TABLE writes(value INTEGER)")?;
            let handle = CancellationHandle::default();
            let worker_handle = handle.clone();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (begin_tx, begin_rx) = mpsc::channel();
            let (pending_tx, pending_rx) = mpsc::channel();
            let (result_tx, result_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let worker = scope.spawn(move || -> Result<(), String> {
                let execute = || -> Result<(), Box<dyn std::error::Error>> {
                    let conn = Connection::open(path.to_string_lossy().into_owned())?;
                    conn.execute("PRAGMA fsqlite.concurrent_mode = OFF")?;
                    conn.execute("PRAGMA busy_timeout = 10000")?;
                    ready_tx.send(())?;
                    begin_rx.recv_timeout(Duration::from_secs(10))?;
                    let result = {
                        let _scope = worker_handle.enter();
                        let mut future =
                            std::pin::pin!(conn.inner.execute("INSERT INTO writes VALUES(2)"));
                        let mut reported_pending = false;
                        conn.drive_operation(std::future::poll_fn(|context| {
                            let result = future.as_mut().poll(context);
                            if result.is_pending() && !reported_pending {
                                reported_pending = true;
                                let _ = pending_tx.send(());
                            }
                            result
                        }))
                    };
                    result_tx.send(result)?;
                    release_rx.recv_timeout(Duration::from_secs(10))?;
                    conn.execute("INSERT INTO writes VALUES(3)")?;
                    conn.close_without_checkpoint()?;
                    Ok(())
                };
                execute().map_err(|error| error.to_string())
            });

            ready_rx.recv_timeout(Duration::from_secs(10))?;
            blocker.execute("BEGIN IMMEDIATE")?;
            blocker.execute("INSERT INTO writes VALUES(1)")?;
            begin_tx.send(())?;
            // Prove the real engine future has been polled to Pending before
            // cancelling. A delayed worker must not satisfy this test through
            // drive_operation's prelatched rejection without exercising a wake.
            pending_rx.recv_timeout(Duration::from_secs(10))?;
            // This wait observes a real contended SQL operation while the writer
            // above owns its transaction; it does not inject a parked engine task.
            let premature = result_rx.recv_timeout(Duration::from_millis(100));
            handle.cancel();
            let cancelled = match premature {
                Ok(result) => (false, Ok(result)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    (true, result_rx.recv_timeout(Duration::from_secs(2)))
                }
                Err(error) => (false, Err(error)),
            };
            // Release the real writer and every handshake before asserting, so a
            // failing cancellation assertion still lets the worker drain/join.
            blocker.execute("COMMIT")?;
            release_tx.send(())?;
            worker.join().expect("SQL owner thread panicked")?;
            assert!(cancelled.0, "the second writer must wait for ownership");
            assert!(
                matches!(cancelled.1, Ok(Err(FrankenError::Abort))),
                "cancellation must settle before the 10-second busy timeout: {:?}",
                cancelled.1
            );
            let rows = blocker.query("SELECT value FROM writes ORDER BY value")?;
            let values = rows
                .iter()
                .map(|row| row.get_typed::<i64>(0))
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(values, vec![1, 3], "cancelled writes must never commit");
            assert!(handle.inner.active.lock().unwrap().is_empty());
            blocker.close()?;
            Ok(())
        })
    }

    #[test]
    fn nested_sql_bridge_restores_full_and_restricted_callers()
    -> Result<(), Box<dyn std::error::Error>> {
        let runtime = RuntimeBuilder::current_thread().build()?;
        runtime.block_on(async {
            let parent = Cx::current().expect("outer runtime context");
            for restricted in [false, true] {
                let restriction = restricted.then(|| Cx::push_restriction(CapMask::none()));
                let caller = Cx::current().expect("caller context");
                let assert_caller = || {
                    let current = Cx::current().expect("restored caller context");
                    assert_eq!(current.task_id(), caller.task_id());
                    assert_eq!(current.region_id(), caller.region_id());
                    assert_eq!(current.capabilities(), caller.capabilities());
                    if restricted {
                        assert!(!current.capabilities().spawn);
                        assert!(!current.capabilities().io);
                    }
                };

                let conn = Connection::open(":memory:")?;
                assert_caller();
                conn.execute_batch(
                    "CREATE TABLE bridge_values (value INTEGER); \
                     INSERT INTO bridge_values VALUES (40);",
                )?;
                assert_caller();
                let mut values = Vec::new();
                conn.query_with_params_for_each("SELECT value FROM bridge_values", &[], |row| {
                    // Streaming invokes this closure inside drive(). Exercise
                    // a second bridge while that runtime is still polling.
                    let mapping = Cx::current().expect("row callback context");
                    let nested = Connection::open(":memory:")?;
                    let extra = nested.query_row("SELECT 2")?.get_typed::<i64>(0)?;
                    nested.close()?;
                    let restored = Cx::current().expect("restored row callback context");
                    assert_eq!(restored.task_id(), mapping.task_id());
                    assert_eq!(restored.region_id(), mapping.region_id());
                    assert_eq!(restored.capabilities(), mapping.capabilities());
                    values.push(row.get_typed::<i64>(0)? + extra);
                    Ok(())
                })?;
                assert_eq!(values, vec![42]);
                assert_caller();
                conn.close()?;
                assert_caller();
                drop(restriction);
                let restored = Cx::current().expect("restored outer runtime context");
                assert_eq!(restored.task_id(), parent.task_id());
                assert_eq!(restored.region_id(), parent.region_id());
                assert_eq!(restored.capabilities(), parent.capabilities());
            }
            Ok::<(), FrankenError>(())
        })?;
        assert!(shutdown_driver(), "SQLite bridge runtime must drain");
        assert!(runtime.shutdown_timeout(std::time::Duration::from_secs(30)));
        Ok(())
    }

    #[test]
    fn multi_statement_execute_error_does_not_replay_prior_side_effects()
    -> Result<(), Box<dyn std::error::Error>> {
        let conn = Connection::open(":memory:")?;
        conn.execute("CREATE TABLE replay_guard (value INTEGER NOT NULL);")?;

        let result = conn.execute(
            "INSERT INTO replay_guard (value) VALUES (1); \
             SELECT * FROM missing_replay_target;",
        );
        assert!(
            matches!(&result, Err(FrankenError::NoSuchTable { .. })),
            "missing SELECT target did not surface NoSuchTable: {result:?}"
        );

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

    #[test]
    fn mapped_query_streams_through_the_bridge_and_preserves_order()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::compat::{ConnectionExt, ParamValue};

        let conn = Connection::open(":memory:")?;
        conn.execute_batch(
            "CREATE TABLE mapped_rows (id INTEGER PRIMARY KEY, body TEXT); \
             INSERT INTO mapped_rows VALUES (1, 'first'), (2, 'second'), (3, 'third');",
        )?;
        let values = conn.query_map_collect(
            "SELECT id, body FROM mapped_rows WHERE id >= ?1 ORDER BY id DESC",
            &[ParamValue::from(2_i64)],
            |row| {
                // The old collect-then-map path ran this outside drive().
                // This is also a regression for nested bridge ownership.
                let context = Cx::current().expect("streaming row callback context");
                let nested = Connection::open(":memory:")?;
                let extra = nested.query_row("SELECT 10")?.get_typed::<i64>(0)?;
                nested.close()?;
                let restored = Cx::current().expect("restored row callback context");
                assert_eq!(restored.task_id(), context.task_id());
                assert_eq!(restored.region_id(), context.region_id());
                Ok((
                    row.get_typed::<i64>(0)? + extra,
                    row.get_typed::<String>(1)?,
                ))
            },
        )?;
        assert_eq!(values, vec![(13, "third".into()), (12, "second".into())]);
        Ok(())
    }

    #[test]
    fn mapped_query_error_stops_callbacks_without_replay_or_partial_success()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::compat::ConnectionExt;

        let conn = Connection::open(":memory:")?;
        conn.execute_batch(
            "CREATE TABLE mapped_errors (id INTEGER PRIMARY KEY); \
             INSERT INTO mapped_errors VALUES (1), (2), (3);",
        )?;
        let mut seen = Vec::new();
        let result: Result<Vec<i64>, FrankenError> =
            conn.query_map_collect("SELECT id FROM mapped_errors ORDER BY id", &[], |row| {
                let id = row.get_typed::<i64>(0)?;
                seen.push(id);
                if id == 2 {
                    return Err(FrankenError::BusyRecovery);
                }
                Ok(id)
            });
        assert!(matches!(result, Err(FrankenError::BusyRecovery)));
        assert_eq!(seen, vec![1, 2]);
        let recovered: Vec<i64> =
            conn.query_map_collect("SELECT id FROM mapped_errors ORDER BY id", &[], |row| {
                row.get_typed(0)
            })?;
        assert_eq!(recovered, vec![1, 2, 3]);
        Ok(())
    }

    #[test]
    fn mapped_query_empty_and_engine_error_do_not_invoke_mapper()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::compat::ConnectionExt;

        let conn = Connection::open(":memory:")?;
        let mut calls = 0;
        let empty: Vec<i64> = conn.query_map_collect("SELECT 1 WHERE 0", &[], |row| {
            calls += 1;
            row.get_typed(0)
        })?;
        assert!(empty.is_empty());
        let missing: Result<Vec<i64>, FrankenError> =
            conn.query_map_collect("SELECT id FROM missing_mapped_table", &[], |row| {
                calls += 1;
                row.get_typed(0)
            });
        assert!(matches!(missing, Err(FrankenError::NoSuchTable { .. })));
        assert_eq!(calls, 0);
        Ok(())
    }
}
