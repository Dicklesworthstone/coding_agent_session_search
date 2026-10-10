//! Reviewed cross-version restoration for CASS logical archives.
//!
//! Exact-schema restoration remains the default. Cross-version restoration is
//! deliberately allowlisted rather than inferred from "compatible-looking" SQL
//! shapes: data backfills can be semantically required even when columns appear
//! additive. The reviewed bridges are storage schema v20 and v21 -> v22.
//! Repository migration fixtures establish what each step adds. v21 adds the
//! conversation-context index. v22 adds the `forgotten_sources` table, the
//! `cass forget` tombstones. An archive older than v22 cannot carry rows for
//! that table, so a migrated archive gets it empty, which is the state an
//! in-place v21 -> v22 upgrade leaves. Every other canonical table layout is
//! unchanged.
//!
//! The current initializer remains the sole executable schema authority. Archived
//! `_schema_migrations` rows and `meta.schema_version` are verified as input but
//! never replayed as current authority. Every other archived row is replayed into
//! private staging and streamed back from the persisted publication image before
//! no-clobber publication. Identical retries use the same read-only projection.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use coding_agent_search::franken_sync::compat::RowExt;
use coding_agent_search::franken_sync::{Connection, FileIdentity, FrankenError, SqliteValue};
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, SqliteStorage};

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};
use super::export::{self, DestinationLock};

const MAX_BATCH_RECORDS: usize = 128;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRIGGER_BYTES: usize = 1024 * 1024;
const MIGRATIONS_TABLE: &str = "_schema_migrations";
const META_TABLE: &str = "meta";
const SCHEMA_VERSION_KEY: &str = "schema_version";
/// Storage schema versions an archive may be migrated from, into
/// [`REVIEWED_TARGET_VERSION`] only.
const REVIEWED_SOURCE_VERSIONS: [u32; 2] = [20, 21];
const REVIEWED_TARGET_VERSION: u32 = 22;
/// Canonical tables the target added after every reviewed source. A migrated
/// archive has no rows for them, and they must stay empty.
const TABLES_ADDED_SINCE_REVIEWED_SOURCES: [&str; 1] = ["forgotten_sources"];

#[derive(Debug, Clone, serde::Serialize)]
pub struct SchemaMigrationReceipt {
    pub mode: &'static str,
    pub from_storage_schema_version: String,
    pub to_storage_schema_version: String,
    pub schema_authority: &'static str,
    pub source_rows_verified: bool,
}

pub struct MigrationOutcome {
    pub header: Header,
    pub completion: Completion,
    pub created: bool,
    pub migration: Option<SchemaMigrationReceipt>,
}

struct Inspected {
    header: Header,
    completion: Completion,
    tables: Vec<Table>,
}

/// The reviewed bridge replaces this marker with current initializer authority,
/// but the archived marker must first agree with the admitted source version.
/// Every accepting pass checks it, including rows not replayed into the target.
struct SourceSchemaMarker {
    version: String,
    columns: Option<(usize, usize)>,
    seen: bool,
}

impl SourceSchemaMarker {
    fn new(header: &Header) -> Self {
        Self {
            version: header.storage_schema_version.clone(),
            columns: None,
            seen: false,
        }
    }

    fn observe(&mut self, record: &Record) -> Result<()> {
        match record {
            Record::Table { table } => {
                self.columns = if table.name == META_TABLE {
                    Some((
                        table
                            .columns
                            .iter()
                            .position(|name| name == "key")
                            .context(
                                "logical archive metadata lacks its schema marker key column",
                            )?,
                        table
                            .columns
                            .iter()
                            .position(|name| name == "value")
                            .context(
                                "logical archive metadata lacks its schema marker value column",
                            )?,
                    ))
                } else {
                    None
                };
            }
            Record::Row { values } => {
                if let Some((key, value)) = self.columns
                    && matches!(values.get(key), Some(Cell::Text(name)) if name == SCHEMA_VERSION_KEY)
                {
                    ensure!(
                        matches!(values.get(value), Some(Cell::Text(version)) if version == &self.version),
                        "logical archive header and canonical schema metadata disagree"
                    );
                    self.seen = true;
                }
            }
            Record::Header { .. } | Record::Completion { .. } => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        ensure!(
            self.seen,
            "logical archive lacks the schema_version marker required for reviewed migration"
        );
        Ok(())
    }
}

struct Input<R> {
    inner: R,
    record_bytes: usize,
}

impl<R: BufRead> Input<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            record_bytes: 0,
        }
    }

    fn record(&mut self, line: u64) -> Result<Option<Record>> {
        self.record_bytes = 0;
        codec::read_record(self, line)
    }
}

impl<R: BufRead> Read for Input<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(bytes)?;
        self.record_bytes = self.record_bytes.saturating_add(count);
        Ok(count)
    }
}

impl<R: BufRead> BufRead for Input<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.inner.consume(count);
        self.record_bytes = self.record_bytes.saturating_add(count);
    }
}

fn target_version() -> Result<u32> {
    u32::try_from(CURRENT_SCHEMA_VERSION).context("current canonical schema version is invalid")
}

fn parse_source_version(header: &Header) -> Result<u32> {
    header
        .storage_schema_version
        .parse::<u32>()
        .context("logical archive storage schema version is invalid")
}

fn require_reviewed_transition(source: u32, target: u32) -> Result<()> {
    ensure!(
        REVIEWED_SOURCE_VERSIONS.contains(&source) && target == REVIEWED_TARGET_VERSION,
        "no reviewed logical-archive migration exists from storage schema {source} to {target}; exact restore or a version-specific migration is required"
    );
    Ok(())
}

fn inspect(file: &mut File, expected_archive_id: &str) -> Result<Inspected> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let Some(Record::Header { header }) = codec::read_record(&mut reader, 1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    header.validate().map_err(super::integrity_unless_io)?;
    ensure!(
        header.archive_id == expected_archive_id,
        "logical archive identity does not match --archive-id"
    );
    let mut validator = Validator::new(header.clone()).map_err(super::integrity_unless_io)?;
    let mut schema_marker = SourceSchemaMarker::new(&header);
    let mut tables = Vec::new();
    let mut line = 2_u64;
    while let Some(record) = codec::read_record(&mut reader, line)? {
        if let Record::Table { table } = &record {
            tables.push(table.clone());
        }
        validator
            .validate(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        schema_marker
            .observe(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let (verified_header, completion) = validator.finish().map_err(super::integrity_unless_io)?;
    schema_marker.finish().map_err(super::integrity_unless_io)?;
    ensure!(
        verified_header == header,
        "logical archive header changed during verification"
    );
    Ok(Inspected {
        header,
        completion,
        tables,
    })
}

fn require_reviewed_table_layout(
    connection: &Connection,
    archived: &[Table],
) -> Result<Vec<Table>> {
    let (added, carried): (Vec<Table>, Vec<Table>) = export::tables(connection)?
        .into_iter()
        .partition(|table| TABLES_ADDED_SINCE_REVIEWED_SOURCES.contains(&table.name.as_str()));
    ensure!(
        archived == carried,
        "reviewed migration to v{REVIEWED_TARGET_VERSION} requires canonical table/column/primary-key descriptors identical to the current ones, apart from tables added since v21; table drift is not authorized"
    );
    ensure!(
        added.len() == TABLES_ADDED_SINCE_REVIEWED_SOURCES.len(),
        "current schema lacks a table the reviewed migration adds"
    );
    for table in &added {
        let sql = format!("SELECT 1 FROM {} LIMIT 1", export::quoted(&table.name)?);
        ensure!(
            connection
                .query(&sql)
                .map_err(|error| {
                    super::database_failure(
                        format!("cannot check newly added logical table {}", table.name),
                        error,
                    )
                })?
                .is_empty(),
            "table {} added since the archived schema must stay empty in a migrated archive",
            table.name
        );
    }
    Ok(carried)
}

fn values(cells: Vec<Cell>) -> Result<Vec<SqliteValue>> {
    cells
        .into_iter()
        .map(|cell| {
            cell.validate()?;
            Ok(match cell {
                Cell::Null => SqliteValue::Null,
                Cell::Integer(value) => SqliteValue::Integer(value),
                Cell::Real(bits) => SqliteValue::Float(f64::from_bits(
                    u64::from_str_radix(&bits, 16).map_err(|_| anyhow!("invalid REAL encoding"))?,
                )),
                Cell::Text(value) => SqliteValue::Text(value.into()),
                Cell::Blob(value) => SqliteValue::Blob(
                    STANDARD
                        .decode(value)
                        .map_err(|_| anyhow!("invalid BLOB encoding"))?
                        .into(),
                ),
            })
        })
        .collect()
}

fn insert_sql(table: &Table) -> Result<String> {
    table.validate()?;
    let columns = table
        .columns
        .iter()
        .map(|name| export::quoted(name))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let placeholders = vec!["?"; table.columns.len()].join(", ");
    Ok(format!(
        "INSERT INTO {} ({columns}) VALUES ({placeholders})",
        export::quoted(&table.name)?
    ))
}

fn meta_schema_version_row(table: &Table, cells: &[Cell]) -> bool {
    if table.name != META_TABLE {
        return false;
    }
    table
        .columns
        .iter()
        .position(|column| column == "key")
        .and_then(|offset| cells.get(offset))
        .is_some_and(|cell| matches!(cell, Cell::Text(value) if value == SCHEMA_VERSION_KEY))
}

fn suspend_triggers(connection: &Connection) -> Result<Vec<String>> {
    let rows = connection
        .query(
            "SELECT name, substr(sql, 1, 65537) FROM sqlite_master WHERE type = 'trigger' ORDER BY name LIMIT 257",
        )
        .map_err(|error| super::database_failure("cannot read canonical migration triggers", error))?;
    ensure!(
        rows.len() <= 256,
        "canonical schema exceeds restore trigger limit"
    );
    let mut statements = Vec::new();
    let mut bytes = 0_usize;
    for row in rows {
        let name = row.get_typed::<String>(0).map_err(|error| {
            super::database_failure("cannot decode a canonical migration trigger name", error)
        })?;
        let sql = row.get_typed::<String>(1).map_err(|error| {
            super::database_failure(
                "cannot decode a canonical migration trigger definition",
                error,
            )
        })?;
        bytes = bytes.saturating_add(sql.len());
        ensure!(
            sql.len() <= 65_536 && bytes <= MAX_TRIGGER_BYTES,
            "canonical trigger definitions exceed restore budget"
        );
        connection
            .execute(&format!("DROP TRIGGER {}", export::quoted(&name)?))
            .map_err(|error| {
                super::database_failure("cannot suspend a canonical migration trigger", error)
            })?;
        statements.push(sql);
    }
    Ok(statements)
}

fn verify_current_schema_authority(connection: &Connection) -> Result<()> {
    super::import::verify_schema_authority(connection, &target_version()?.to_string())
}

fn clear_archived_data(connection: &Connection, archived: &[Table]) -> Result<()> {
    for table in archived {
        if table.name == MIGRATIONS_TABLE {
            continue;
        }
        if table.name == META_TABLE {
            connection
                .execute("DELETE FROM \"meta\" WHERE \"key\" <> 'schema_version'")
                .map_err(|error| {
                    super::database_failure(
                        "cannot clear initializer rows in logical table meta",
                        error,
                    )
                })?;
        } else {
            connection
                .execute(&format!("DELETE FROM {}", export::quoted(&table.name)?))
                .map_err(|error| {
                    super::database_failure(
                        format!(
                            "cannot clear initializer rows in logical table {}",
                            table.name
                        ),
                        error,
                    )
                })?;
        }
    }
    Ok(())
}

fn restore_reviewed<R: BufRead>(
    connection: &Connection,
    input: &mut Input<R>,
    inspected: &Inspected,
) -> Result<()> {
    let source_version = parse_source_version(&inspected.header)?;
    let target = target_version()?;
    require_reviewed_transition(source_version, target)?;
    require_reviewed_table_layout(connection, &inspected.tables)?;
    verify_current_schema_authority(connection)?;

    let Some(Record::Header { header }) = input.record(1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    ensure!(
        header == inspected.header,
        "logical archive changed between validation and replay"
    );
    let mut schema_marker = SourceSchemaMarker::new(&header);
    let mut validator = Validator::new(header).map_err(super::integrity_unless_io)?;
    connection
        .execute("PRAGMA foreign_keys = OFF")
        .map_err(|error| {
            super::database_failure("cannot suspend migration foreign-key enforcement", error)
        })?;
    connection.execute("BEGIN IMMEDIATE").map_err(|error| {
        super::database_failure("cannot begin the private migration transaction", error)
    })?;
    let triggers = suspend_triggers(connection)?;
    clear_archived_data(connection, &inspected.tables)?;

    let mut statement = None;
    let mut current_table: Option<Table> = None;
    let mut table_count = 0_usize;
    let mut table_row = 0_u64;
    let mut batch_records = 0_usize;
    let mut batch_bytes = 0_usize;
    let mut line = 2_u64;

    while let Some(record) = input.record(line)? {
        if batch_records == MAX_BATCH_RECORDS
            || input.record_bytes > MAX_BATCH_BYTES.saturating_sub(batch_bytes)
        {
            connection.execute("COMMIT").map_err(|error| {
                super::database_failure(
                    format!("cannot commit the private migration batch before record {line}"),
                    error,
                )
            })?;
            connection.execute("BEGIN IMMEDIATE").map_err(|error| {
                super::database_failure(
                    format!("cannot begin the private migration batch for record {line}"),
                    error,
                )
            })?;
            batch_records = 0;
            batch_bytes = 0;
        }
        validator
            .validate(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        schema_marker
            .observe(&record)
            .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
        match record {
            Record::Table { table } => {
                ensure!(
                    inspected.tables.get(table_count) == Some(&table),
                    "record {line}: logical table descriptor changed after validation"
                );
                statement = if table.name == MIGRATIONS_TABLE {
                    None
                } else {
                    Some(connection.prepare(&insert_sql(&table)?).map_err(|error| {
                        super::database_failure(
                            format!("record {line}: cannot prepare logical table {} migration insertion", table.name),
                            error,
                        )
                    })?)
                };
                current_table = Some(table);
                table_count += 1;
                table_row = 0;
            }
            Record::Row { values: cells } => {
                let table = current_table
                    .as_ref()
                    .ok_or_else(|| anyhow!("record {line}: row precedes its table"))?;
                table_row = table_row
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("logical table row position overflow"))?;
                if table.name == MIGRATIONS_TABLE {
                    // Archived migration history is input evidence, never current authority.
                } else if meta_schema_version_row(table, &cells) {
                    // The observed source marker agrees with the admitted header;
                    // the initializer supplies the current target marker instead.
                } else {
                    let row = values(cells)?;
                    statement
                        .as_ref()
                        .ok_or_else(|| anyhow!("record {line}: no compatible insert target"))?
                        .execute_with_params(&row)
                        .map_err(|error| {
                            super::database_failure(
                                format!(
                                    "record {line}: {}, reviewed v{REVIEWED_TARGET_VERSION} row insertion failed; no destination was published",
                                    export::row_location(table, table_row, &row)
                                ),
                                error,
                            )
                        })?;
                }
            }
            Record::Completion { .. } => {}
            Record::Header { .. } => bail!("record {line}: duplicate archive header"),
        }
        batch_records += 1;
        batch_bytes += input.record_bytes;
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }

    let (header, completion) = validator.finish().map_err(super::integrity_unless_io)?;
    ensure!(
        (header, completion) == (inspected.header.clone(), inspected.completion.clone()),
        "logical archive changed during reviewed migration replay"
    );
    ensure!(
        table_count == inspected.tables.len(),
        "logical archive table set changed during reviewed migration replay"
    );
    schema_marker.finish().map_err(super::integrity_unless_io)?;

    drop(statement);
    for trigger in triggers {
        connection.execute_batch(&trigger).map_err(|error| {
            super::database_failure("cannot reinstate canonical migration triggers", error)
        })?;
    }
    verify_current_schema_authority(connection)?;
    super::import::verify_database(connection)?;
    connection.execute("COMMIT").map_err(|error| {
        super::database_failure(
            "cannot commit the verified private migrated database",
            error,
        )
    })?;
    Ok(())
}

fn sidecars(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", "-journal"].map(|suffix| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    })
}

fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => bail!(
            "restore destination or SQLite sidecar already exists or cannot be inspected; nothing was replaced"
        ),
    }
}

fn require_new_destination(path: &Path) -> Result<()> {
    require_absent(path)?;
    for sidecar in sidecars(path) {
        require_absent(&sidecar)?;
    }
    Ok(())
}

fn require_candidate_without_sidecars(candidate: &Path) -> Result<()> {
    for sidecar in sidecars(candidate) {
        require_absent(&sidecar)
            .context("restore publication image still depends on SQLite sidecars")?;
    }
    Ok(())
}

fn materialize_candidate(connection: &Connection, candidate: &Path) -> Result<()> {
    require_new_destination(candidate)?;
    // Only the private, committed replay is checkpointed. The engine's page
    // backup avoids VACUUM INTO's whole-database hydration in the pinned engine.
    connection
        .backup_exact_to(candidate)
        .map_err(|error| {
            super::database_failure(
                "cannot materialize a self-contained migrated publication image",
                error,
            )
        })?;
    require_candidate_without_sidecars(candidate)
}

fn sync_candidate(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "migrated publication candidate must be a regular, non-symlink file"
    );
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
        .open(path)?
        .sync_all()
        .context("cannot sync the verified migrated publication candidate")
}

fn identity(path: &Path) -> Result<FileIdentity> {
    let file = super::import::open_input(path)?;
    FileIdentity::from_file(&file)?
        .ok_or_else(|| anyhow!("cannot prove the existing destination's file identity"))
}

fn require_same_file(connection: &Connection, path: &Path) -> Result<()> {
    let actual = identity(path)?;
    ensure!(
        connection.file_identity().map_err(|error| {
            super::database_failure(
                "cannot recheck the reviewed migration destination identity",
                error,
            )
        })? == Some(actual),
        "restore destination changed during reviewed migration comparison; retry without replacing it"
    );
    Ok(())
}

struct ProjectionCursor<R> {
    reader: R,
    line: u64,
    pending: Option<Record>,
    validator: Validator,
    schema_marker: SourceSchemaMarker,
    completed: bool,
    eof: bool,
}

impl<R: BufRead> ProjectionCursor<R> {
    fn new(reader: R, header: Header) -> Result<Self> {
        let schema_marker = SourceSchemaMarker::new(&header);
        Ok(Self {
            reader,
            line: 2,
            pending: None,
            validator: Validator::new(header).map_err(super::integrity_unless_io)?,
            schema_marker,
            completed: false,
            eof: false,
        })
    }

    fn next(&mut self) -> Result<Option<Record>> {
        if self.pending.is_some() {
            // A table boundary was already validated when first decoded.
            return Ok(self.pending.take());
        }
        if self.eof {
            return Ok(None);
        }
        let line = self.line;
        let record = codec::read_record(&mut self.reader, line)?;
        if let Some(record) = &record {
            self.validator
                .validate(record)
                .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
            self.schema_marker
                .observe(record)
                .map_err(|error| super::integrity(format!("record {line}: {error}")))?;
            self.completed = matches!(record, Record::Completion { .. });
            self.line = self
                .line
                .checked_add(1)
                .ok_or_else(|| anyhow!("logical record position overflow"))?;
        } else {
            self.eof = true;
            if !self.completed {
                return Err(super::integrity(format!(
                    "logical archive ended before completion at record {line}"
                )));
            }
        }
        Ok(record)
    }

    fn put_back(&mut self, record: Record) -> Result<()> {
        ensure!(
            self.pending.is_none(),
            "logical projection cursor already has a boundary"
        );
        self.pending = Some(record);
        Ok(())
    }

    fn finish(self) -> Result<(Header, Completion)> {
        ensure!(
            self.eof && self.pending.is_none(),
            "logical projection must reach EOF before claiming verified source rows"
        );
        let verified = self
            .validator
            .finish()
            .map_err(super::integrity_unless_io)?;
        self.schema_marker
            .finish()
            .map_err(super::integrity_unless_io)?;
        Ok(verified)
    }
}

fn migrated_row_equal(table: &Table, archived: &[Cell], actual: &[Cell]) -> Result<bool> {
    if !meta_schema_version_row(table, archived) {
        return Ok(archived == actual);
    }
    ensure!(
        archived.len() == actual.len(),
        "migrated schema marker row changed shape"
    );
    let value_offset = table.columns.iter().position(|column| column == "value");
    let Some(value_offset) = value_offset else {
        return Ok(archived == actual);
    };
    for offset in 0..archived.len() {
        if offset == value_offset {
            ensure!(
                actual[offset] == Cell::Text(target_version()?.to_string()),
                "persisted schema marker is not current"
            );
        } else if archived[offset] != actual[offset] {
            return Ok(false);
        }
    }
    Ok(true)
}

fn compare_table_rows<R: BufRead>(
    connection: &Connection,
    table: &Table,
    cursor: &mut ProjectionCursor<R>,
) -> Result<()> {
    let sql = export::ordered_scan_sql(connection, table)?;

    let mut failure = None;
    let mut rows_compared = 0_u64;
    let streamed = connection.query_with_params_for_each(&sql, &[], |row| {
        let result = (|| -> Result<()> {
            let next_row = rows_compared
                .checked_add(1)
                .ok_or_else(|| anyhow!("logical table row position overflow"))?;
            let Some(record) = cursor.next()? else {
                bail!(
                    "persisted migrated table {} contains an extra row",
                    table.name
                );
            };
            let Record::Row { values: archived } = record else {
                bail!(
                    "persisted migrated table {} contains more rows than the archive",
                    table.name
                );
            };
            let actual = export::cells(row.values())?;
            ensure!(
                migrated_row_equal(table, &archived, &actual)?,
                "persisted migrated row differs from verified archive table {}",
                table.name
            );
            rows_compared = next_row;
            Ok(())
        })();
        if let Err(error) = result {
            failure = Some(error);
            return Err(FrankenError::Internal(
                "reviewed archive projection comparison aborted".to_owned(),
            ));
        }
        Ok(())
    });
    if let Some(error) = failure {
        return Err(error);
    }
    streamed.map_err(|error| {
        super::database_failure(
            format!(
                "cannot compare persisted migrated logical table {} before row {}, after {rows_compared} complete rows",
                table.name,
                rows_compared.saturating_add(1)
            ),
            error,
        )
    })?;

    if let Some(record) = cursor.next()? {
        if matches!(record, Record::Row { .. }) {
            bail!(
                "verified archive table {} contains a row missing from the migrated database",
                table.name
            );
        }
        cursor.put_back(record)?;
    }
    Ok(())
}

fn skip_archived_rows<R: BufRead>(cursor: &mut ProjectionCursor<R>) -> Result<()> {
    while let Some(record) = cursor.next()? {
        if matches!(record, Record::Row { .. }) {
            continue;
        }
        cursor.put_back(record)?;
        return Ok(());
    }
    bail!("logical archive ended before completion")
}

/// Authenticate the same input pass that is compared with persisted rows.
/// Even skipped migration-history rows advance the validator exactly once.
fn verify_projection_rows(
    connection: &Connection,
    mut input: impl BufRead,
    inspected: &Inspected,
) -> Result<()> {
    let Some(Record::Header { header }) = codec::read_record(&mut input, 1)? else {
        return Err(super::integrity("logical archive must begin with a header"));
    };
    ensure!(header == inspected.header, "logical archive header changed");
    let mut cursor = ProjectionCursor::new(input, header)?;

    for expected in &inspected.tables {
        let Some(Record::Table { table }) = cursor.next()? else {
            bail!("logical archive table set changed during persisted verification");
        };
        ensure!(
            table == *expected,
            "logical archive table descriptor changed during persisted verification"
        );
        if table.name == MIGRATIONS_TABLE {
            skip_archived_rows(&mut cursor)?;
        } else {
            compare_table_rows(connection, &table, &mut cursor)?;
        }
    }

    let Some(Record::Completion { completion }) = cursor.next()? else {
        bail!("logical archive completion moved during persisted verification");
    };
    ensure!(
        completion == inspected.completion,
        "logical archive completion changed during persisted verification"
    );
    ensure!(
        cursor.next()?.is_none(),
        "records follow the archive completion during persisted verification"
    );
    ensure!(
        cursor.finish()? == (inspected.header.clone(), inspected.completion.clone()),
        "logical archive changed during persisted verification"
    );
    Ok(())
}

fn verify_persisted_projection(
    file: &mut File,
    candidate: &Path,
    inspected: &Inspected,
    expected_identity: Option<FileIdentity>,
    require_sidecar_free: bool,
) -> Result<()> {
    let reader = export::open_source(candidate)?;
    if let Some(expected_identity) = expected_identity {
        ensure!(
            reader.file_identity().map_err(|error| {
                super::database_failure(
                    "cannot read the reviewed migration destination identity",
                    error,
                )
            })? == Some(expected_identity),
            "restore destination changed before reviewed migration comparison; nothing was replaced"
        );
    }
    super::import::verify_database(&reader)?;
    verify_current_schema_authority(&reader)?;
    require_reviewed_table_layout(&reader, &inspected.tables)?;

    file.seek(SeekFrom::Start(0))?;
    verify_projection_rows(&reader, BufReader::new(&mut *file), inspected)?;

    if expected_identity.is_some() {
        require_same_file(&reader, candidate)?;
    }
    reader.execute("ROLLBACK").map_err(|error| {
        super::database_failure(
            "cannot release the reviewed migration comparison snapshot",
            error,
        )
    })?;
    reader.close_without_checkpoint().map_err(|error| {
        super::database_failure(
            "cannot close the reviewed migration comparison reader",
            error,
        )
    })?;
    if require_sidecar_free {
        require_candidate_without_sidecars(candidate)?;
    }
    Ok(())
}

fn migration_receipt(inspected: &Inspected) -> Result<SchemaMigrationReceipt> {
    let source = parse_source_version(&inspected.header)?;
    require_reviewed_transition(source, target_version()?)?;
    Ok(SchemaMigrationReceipt {
        mode: match source {
            20 => "reviewed_v20_to_v22",
            21 => "reviewed_v21_to_v22",
            _ => bail!("no reviewed migration mode for storage schema {source}"),
        },
        from_storage_schema_version: inspected.header.storage_schema_version.clone(),
        to_storage_schema_version: target_version()?.to_string(),
        schema_authority: "current_binary_initializer",
        source_rows_verified: true,
    })
}

pub fn import_compatible(
    input_path: &Path,
    destination: &Path,
    expected_archive_id: &str,
    if_identical: bool,
) -> Result<MigrationOutcome> {
    let mut file = super::import::open_input(input_path)?;
    let inspected = inspect(&mut file, expected_archive_id)?;
    let source_version = parse_source_version(&inspected.header)?;
    let target = target_version()?;

    if source_version == target {
        let (header, completion, created) = super::import::import_inspected_file(
            file,
            destination,
            &inspected.header,
            &inspected.completion,
            if_identical,
        )?;
        return Ok(MigrationOutcome {
            header,
            completion,
            created,
            migration: None,
        });
    }
    require_reviewed_transition(source_version, target)?;

    let _lock = DestinationLock::acquire(destination)?;
    if if_identical && fs::symlink_metadata(destination).is_ok() {
        let admitted = identity(destination)?;
        verify_persisted_projection(
            &mut file,
            destination,
            &inspected,
            Some(admitted),
            false,
        )
        .map_err(|error| {
            let message = format!(
                "cannot verify the existing destination as a reviewed migration; nothing was replaced: {error}"
            );
            error.context(message)
        })?;
        return Ok(MigrationOutcome {
            header: inspected.header.clone(),
            completion: inspected.completion.clone(),
            created: false,
            migration: Some(migration_receipt(&inspected)?),
        });
    }

    require_new_destination(destination)?;
    let staging = tempfile::Builder::new()
        .prefix(".cass-migrate-")
        .tempdir_in(export::parent(destination)?)?;
    let replay_path = staging.path().join("agent_search.db");
    let candidate = staging.path().join("publication.db");

    let storage = SqliteStorage::open(&replay_path)
        .context("cannot initialize reviewed migration candidate")?;
    drop(storage);
    let connection = Connection::open(export::path_text(&replay_path)?).map_err(|error| {
        super::database_failure("cannot open the private migration database", error)
    })?;
    connection
        .execute("PRAGMA busy_timeout = 5000")
        .map_err(|error| {
            super::database_failure("cannot set the private migration busy timeout", error)
        })?;
    let mode = connection
        .query_row("PRAGMA journal_mode = WAL")
        .map_err(|error| {
            super::database_failure(
                "cannot enable WAL for private reviewed migration replay",
                error,
            )
        })?
        .get_typed::<String>(0)
        .map_err(|error| {
            super::database_failure("cannot decode the private migration journal mode", error)
        })?;
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "cannot enable WAL for private reviewed migration replay"
    );
    connection
        .execute("PRAGMA synchronous = FULL")
        .map_err(|error| {
            super::database_failure("cannot require durable private migration writes", error)
        })?;

    file.seek(SeekFrom::Start(0))?;
    let mut bounded = Input::new(BufReader::new(&mut file));
    restore_reviewed(&connection, &mut bounded, &inspected)?;
    materialize_candidate(&connection, &candidate)?;
    connection.close().map_err(|error| {
        super::database_failure(
            "cannot close the materialized private migration database",
            error,
        )
    })?;

    verify_persisted_projection(&mut file, &candidate, &inspected, None, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600))?;
    }
    sync_candidate(&candidate)?;
    require_new_destination(destination)?;
    fs::hard_link(&candidate, destination).context(
        "cannot publish migrated archive without replacing existing data; destination must support hard links",
    )?;
    export::sync_parent(destination)?;

    Ok(MigrationOutcome {
        header: inspected.header.clone(),
        completion: inspected.completion.clone(),
        created: true,
        migration: Some(migration_receipt(&inspected)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_agent_search::model::types::{Agent, AgentKind};
    use coding_agent_search::storage::sqlite::SqliteStorage;
    use std::collections::BTreeMap;
    use std::io::{Cursor, Write};

    fn projection_header(format_version: u32, source_version: u32) -> Header {
        Header {
            format: codec::FORMAT.to_owned(),
            schema_version: format_version,
            archive_id: "projection-regression".into(),
            exported_at_ms: 1,
            storage_schema_version: source_version.to_string(),
            record_types: codec::record_types(format_version),
            contains_private_data: true,
            omissions: vec!["derived_search_assets".into()],
        }
    }

    fn projection_records(marker: Option<Cell>, content: &str) -> Vec<Record> {
        let mut records = vec![
            Record::Table {
                table: Table {
                    name: MIGRATIONS_TABLE.into(),
                    columns: vec!["version".into(), "description".into()],
                    primary_key: vec![0],
                },
            },
            Record::Row {
                values: vec![Cell::Integer(1), Cell::Text("PRIVATE-HISTORY".into())],
            },
            Record::Table {
                table: Table {
                    name: "messages".into(),
                    columns: vec!["id".into(), "content".into()],
                    primary_key: vec![0],
                },
            },
            Record::Row {
                values: vec![Cell::Integer(7), Cell::Text(content.into())],
            },
            Record::Table {
                table: Table {
                    name: META_TABLE.into(),
                    columns: vec!["key".into(), "value".into()],
                    primary_key: vec![0],
                },
            },
        ];
        if let Some(marker) = marker {
            records.push(Record::Row {
                values: vec![Cell::Text(SCHEMA_VERSION_KEY.into()), marker],
            });
        }
        records
    }

    fn archive_wire(header: Header, records: &[Record]) -> Result<(Vec<u8>, Inspected)> {
        let mut output = codec::encode(&Record::Header {
            header: header.clone(),
        })?;
        let mut validator = Validator::new(header.clone())?;
        let mut tables = Vec::new();
        for record in records {
            validator.prepare(record)?.write_to(&mut output)?;
            if let Record::Table { table } = record {
                tables.push(table.clone());
            }
        }
        let completion = validator.completion();
        validator
            .prepare(&Record::Completion {
                completion: completion.clone(),
            })?
            .write_to(&mut output)?;
        validator.finish()?;
        Ok((
            output,
            Inspected {
                header,
                completion,
                tables,
            },
        ))
    }

    /// Deliberately retain an earlier completion after changing compared or
    /// skipped records. A projection that trusts its earlier verification pass
    /// accepts some of these streams even though their digest is now false.
    fn unchecked_projection_wire(inspected: &Inspected, records: &[Record]) -> Result<Vec<u8>> {
        let mut output = codec::encode(&Record::Header {
            header: inspected.header.clone(),
        })?;
        for record in records {
            serde_json::to_writer(&mut output, record)?;
            output.push(b'\n');
        }
        output.extend(codec::encode(&Record::Completion {
            completion: inspected.completion.clone(),
        })?);
        Ok(output)
    }

    fn projection_database(content: &str) -> Result<Connection> {
        let connection = Connection::open(":memory:")?;
        connection.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT)")?;
        connection.execute("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT)")?;
        connection.execute_with_params(
            "INSERT INTO messages VALUES (7, ?1)",
            &[SqliteValue::Text(content.into())],
        )?;
        connection.execute_with_params(
            "INSERT INTO meta VALUES ('schema_version', ?1)",
            &[SqliteValue::Text(
                REVIEWED_TARGET_VERSION.to_string().into(),
            )],
        )?;
        Ok(connection)
    }

    #[test]
    fn projection_authenticates_the_rows_it_compares_after_prior_inspection() -> Result<()> {
        let connection = projection_database("PRIVATE-NEW-BODY")?;
        for version in [codec::VERSION, codec::CHUNKED_VERSION] {
            let original = projection_records(Some(Cell::Text("20".into())), "PRIVATE-OLD-BODY");
            let (wire, inspected) = archive_wire(projection_header(version, 20), &original)?;
            assert_eq!(
                codec::verify(&mut Cursor::new(wire))?.1,
                inspected.completion
            );

            let changed = projection_records(Some(Cell::Text("20".into())), "PRIVATE-NEW-BODY");
            // The second pass matches every persisted row, but still presents
            // the original completion. Old unvalidated projection accepted it.
            let forged = unchecked_projection_wire(&inspected, &changed)?;
            let error = verify_projection_rows(&connection, Cursor::new(forged), &inspected)
                .expect_err("a prior digest cannot authenticate newly compared rows");
            assert!(error.to_string().contains("digest mismatch"));
            assert!(!error.to_string().contains("PRIVATE-"));

            // Recomputing a valid digest for the replacement is not permission
            // to compare a different archive than the one already admitted.
            let (changed_wire, _) = archive_wire(inspected.header.clone(), &changed)?;
            assert!(
                verify_projection_rows(&connection, Cursor::new(changed_wire), &inspected).is_err()
            );
            assert_eq!(
                connection
                    .query_row("SELECT content FROM messages WHERE id = 7")?
                    .get_typed::<String>(0)?,
                "PRIVATE-NEW-BODY"
            );
        }
        connection.close()?;
        Ok(())
    }

    #[test]
    fn projection_checks_skipped_history_identity_shape_and_content() -> Result<()> {
        let connection = projection_database("PRIVATE-BODY")?;
        let original = projection_records(Some(Cell::Text("21".into())), "PRIVATE-BODY");
        let (_, inspected) =
            archive_wire(projection_header(codec::CHUNKED_VERSION, 21), &original)?;
        for replacement in [
            vec![Cell::Null, Cell::Text("PRIVATE-HISTORY".into())],
            vec![Cell::Integer(1)],
            vec![
                Cell::Integer(1),
                Cell::Text("PRIVATE-CHANGED-HISTORY".into()),
            ],
        ] {
            let mut changed = original.clone();
            changed[1] = Record::Row {
                values: replacement,
            };
            let wire = unchecked_projection_wire(&inspected, &changed)?;
            let error = verify_projection_rows(&connection, Cursor::new(wire), &inspected)
                .expect_err("skipped migration history must still be authenticated");
            assert!(!error.to_string().contains("PRIVATE-"));
        }
        let mut duplicate = original.clone();
        duplicate.insert(2, original[1].clone());
        let wire = unchecked_projection_wire(&inspected, &duplicate)?;
        let error = verify_projection_rows(&connection, Cursor::new(wire), &inspected)
            .expect_err("skipped history cannot repeat a primary key");
        assert!(error.to_string().contains("duplicate or unordered"));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn projection_verifies_v1_v2_boundaries_and_current_or_reviewed_markers() -> Result<()> {
        let connection = projection_database("PRIVATE-BODY")?;
        for version in [codec::VERSION, codec::CHUNKED_VERSION] {
            for source in [20, 21, REVIEWED_TARGET_VERSION] {
                let records =
                    projection_records(Some(Cell::Text(source.to_string())), "PRIVATE-BODY");
                let (wire, inspected) = archive_wire(projection_header(version, source), &records)?;
                // Both nonempty table transitions go through put_back. Hashing
                // either boundary twice would reject this valid completion.
                verify_projection_rows(&connection, Cursor::new(&wire), &inspected)?;
                // EOF at a complete-record boundary is still an incomplete
                // archive, not a database conflict or a generic engine error.
                let mut boundary = 0;
                for record in wire.split_inclusive(|byte| *byte == b'\n') {
                    let error = verify_projection_rows(
                        &connection,
                        Cursor::new(&wire[..boundary]),
                        &inspected,
                    )
                    .expect_err("a verified prefix must not replace completion");
                    assert_eq!(
                        super::super::classify_failure(&error),
                        (5, "logical-archive-integrity", false)
                    );
                    boundary += record.len();
                }
                assert!(
                    verify_projection_rows(
                        &connection,
                        Cursor::new(&wire[..wire.len() - 1]),
                        &inspected
                    )
                    .is_err()
                );
                let mut trailing = wire;
                trailing.extend(codec::encode(&records[1])?);
                assert!(
                    verify_projection_rows(&connection, Cursor::new(trailing), &inspected).is_err()
                );
            }
        }
        connection.close()?;
        Ok(())
    }

    #[test]
    fn projection_cursor_authenticates_boundaries_once_and_refuses_incomplete_prefixes()
    -> Result<()> {
        for version in [codec::VERSION, codec::CHUNKED_VERSION] {
            let records = projection_records(Some(Cell::Text("20".into())), "PRIVATE-BODY");
            let (wire, inspected) = archive_wire(projection_header(version, 20), &records)?;
            let mut input = Cursor::new(&wire);
            let Some(Record::Header { header }) = codec::read_record(&mut input, 1)? else {
                bail!("fixture header missing");
            };
            let mut cursor = ProjectionCursor::new(input, header)?;
            let mut boundaries = 0;
            while let Some(record) = cursor.next()? {
                if matches!(record, Record::Table { .. } | Record::Completion { .. }) {
                    let position = cursor.line;
                    cursor.put_back(record.clone())?;
                    assert_eq!(cursor.next()?, Some(record));
                    assert_eq!(cursor.line, position);
                    boundaries += 1;
                }
            }
            assert_eq!(boundaries, 4);
            assert_eq!(
                cursor.finish()?,
                (inspected.header.clone(), inspected.completion)
            );

            let header_end = wire.iter().position(|byte| *byte == b'\n').unwrap() + 1;
            let mut boundary = header_end;
            for record in wire[header_end..].split_inclusive(|byte| *byte == b'\n') {
                let mut input = Cursor::new(&wire[..boundary]);
                let Some(Record::Header { header }) = codec::read_record(&mut input, 1)? else {
                    bail!("fixture header missing");
                };
                let mut cursor = ProjectionCursor::new(input, header)?;
                let error = loop {
                    match cursor.next() {
                        Ok(Some(_)) => {}
                        Ok(None) => bail!("cursor accepted a prefix without completion"),
                        Err(error) => break error,
                    }
                };
                assert_eq!(
                    super::super::classify_failure(&error),
                    (5, "logical-archive-integrity", false)
                );
                assert!(!error.to_string().contains("PRIVATE-"));
                boundary += record.len();
            }
        }
        Ok(())
    }

    #[test]
    fn projection_cursor_refuses_changed_rows_and_false_markers_after_inspection() -> Result<()> {
        let consume = |wire: Vec<u8>| -> Result<(Header, Completion)> {
            let mut input = Cursor::new(wire);
            let Some(Record::Header { header }) = codec::read_record(&mut input, 1)? else {
                bail!("fixture header missing");
            };
            let mut cursor = ProjectionCursor::new(input, header)?;
            while cursor.next()?.is_some() {}
            cursor.finish()
        };

        for version in [codec::VERSION, codec::CHUNKED_VERSION] {
            let records = projection_records(Some(Cell::Text("20".into())), "PRIVATE-BODY");
            let (wire, inspected) = archive_wire(projection_header(version, 20), &records)?;
            codec::verify(&mut Cursor::new(wire))?;
            for row in [1, 3] {
                let mut changed = records.clone();
                let Record::Row { values } = &mut changed[row] else {
                    bail!("fixture row missing");
                };
                values[1] = Cell::Text("PRIVATE-CHANGED-AFTER-INSPECTION".into());
                let error = consume(unchecked_projection_wire(&inspected, &changed)?)
                    .expect_err("compared and skipped rows both require the current digest");
                assert_eq!(
                    super::super::classify_failure(&error),
                    (5, "logical-archive-integrity", false)
                );
                assert!(error.to_string().contains("digest mismatch"));
                assert!(!error.to_string().contains("PRIVATE-"));
            }
            for marker in [
                None,
                Some(Cell::Null),
                Some(Cell::Integer(20)),
                Some(Cell::Text("21".into())),
                Some(Cell::Text("PRIVATE-FALSE-VERSION".into())),
            ] {
                let records = projection_records(marker, "PRIVATE-BODY");
                let (wire, _) = archive_wire(projection_header(version, 20), &records)?;
                codec::verify(&mut Cursor::new(&wire))?;
                let error = consume(wire).expect_err(
                    "a valid digest does not establish a truthful source schema marker",
                );
                assert_eq!(
                    super::super::classify_failure(&error),
                    (5, "logical-archive-integrity", false)
                );
                assert!(!error.to_string().contains("PRIVATE-"));
            }
        }
        Ok(())
    }

    #[test]
    fn projection_preserves_a_reader_failure_after_the_valid_completion() -> Result<()> {
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PRIVATE-READ-DETAIL",
                ))
            }
        }
        impl BufRead for FailedRead {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PRIVATE-READ-DETAIL",
                ))
            }
            fn consume(&mut self, _: usize) {}
        }

        let records = projection_records(Some(Cell::Text("20".into())), "PRIVATE-BODY");
        let (wire, _) = archive_wire(projection_header(codec::CHUNKED_VERSION, 20), &records)?;
        let mut input = Cursor::new(wire).chain(FailedRead);
        let Some(Record::Header { header }) = codec::read_record(&mut input, 1)? else {
            bail!("fixture header missing");
        };
        let mut cursor = ProjectionCursor::new(input, header)?;
        let error = loop {
            match cursor.next() {
                Ok(Some(_)) => {}
                Ok(None) => bail!("completion hid a failed EOF read"),
                Err(error) => break error,
            }
        };
        assert_eq!(
            super::super::classify_failure(&error),
            (14, "logical-archive-io", true)
        );
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!error.to_string().contains("PRIVATE-"));
        Ok(())
    }

    #[test]
    fn projection_rejects_false_or_missing_source_markers_even_with_a_valid_digest() -> Result<()> {
        let connection = projection_database("PRIVATE-BODY")?;
        for marker in [
            None,
            Some(Cell::Null),
            Some(Cell::Integer(20)),
            Some(Cell::Text("21".into())),
            Some(Cell::Text("PRIVATE-FALSE-VERSION".into())),
        ] {
            let records = projection_records(marker, "PRIVATE-BODY");
            let (wire, inspected) =
                archive_wire(projection_header(codec::CHUNKED_VERSION, 20), &records)?;
            codec::verify(&mut Cursor::new(&wire))?;
            let error = verify_projection_rows(&connection, Cursor::new(wire), &inspected)
                .expect_err("discarding an archived marker must not excuse a false source version");
            assert!(!error.to_string().contains("PRIVATE-"));
        }
        connection.close()?;
        Ok(())
    }

    #[test]
    fn projection_database_read_failure_retains_its_safe_typed_cause() -> Result<()> {
        let connection = Connection::open(":memory:")?;
        let table = Table {
            name: "messages".into(),
            columns: vec!["id".into(), "content".into()],
            primary_key: vec![0],
        };
        let descriptor = Cursor::new(codec::encode(&Record::Table {
            table: table.clone(),
        })?);
        let mut cursor = ProjectionCursor::new(
            descriptor.chain(&b"PRIVATE-ARCHIVE-MUST-NOT-BE-READ"[..]),
            projection_header(codec::VERSION, 20),
        )?;
        assert!(matches!(cursor.next()?, Some(Record::Table { .. })));
        let error = compare_table_rows(&connection, &table, &mut cursor)
            .expect_err("a missing canonical table must fail database comparison");
        let message = error.to_string();
        assert!(message.contains("logical table messages before row 1, after 0 complete rows"));
        assert!(message.contains("missing table"));
        assert!(!message.contains("PRIVATE-ARCHIVE"));
        assert!(matches!(
            error.downcast_ref::<FrankenError>(),
            Some(FrankenError::NoSuchTable { .. })
        ));
        assert_eq!(
            cursor.line, 3,
            "database errors must not consume archive rows"
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn projection_cancellation_reports_the_completed_prefix_and_preserves_database() -> Result<()> {
        use fsqlite_types::cx::CancelReason;
        use std::cell::Cell as Counter;

        struct CancelOnFirstRecord<'a, F> {
            remaining: &'a [u8],
            records: &'a Counter<u64>,
            cancel: Option<F>,
        }

        impl<F: FnOnce()> Read for CancelOnFirstRecord<'_, F> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let count = output.len().min(self.remaining.len());
                output[..count].copy_from_slice(&self.remaining[..count]);
                self.consume(count);
                Ok(count)
            }
        }

        impl<F: FnOnce()> BufRead for CancelOnFirstRecord<'_, F> {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                Ok(self.remaining)
            }

            fn consume(&mut self, count: usize) {
                for byte in &self.remaining[..count] {
                    if *byte == b'\n' {
                        self.records.set(self.records.get() + 1);
                        if let Some(cancel) = self.cancel.take() {
                            cancel();
                        }
                    }
                }
                self.remaining = &self.remaining[count..];
            }
        }

        let connection = Connection::open(":memory:")?;
        connection.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT)")?;
        let table = Table {
            name: "messages".into(),
            columns: vec!["id".into(), "content".into()],
            primary_key: vec![0],
        };
        let mut encoded = Vec::new();
        connection.execute("BEGIN IMMEDIATE")?;
        let statement = connection.prepare("INSERT INTO messages VALUES (?, ?)")?;
        for id in 1..=2048 {
            statement.execute_with_params(&[
                SqliteValue::Integer(id),
                SqliteValue::Text("PRIVATE-PROJECTION-CONTENT".into()),
            ])?;
            encoded.extend(codec::encode(&Record::Row {
                values: vec![
                    Cell::Integer(id),
                    Cell::Text("PRIVATE-PROJECTION-CONTENT".into()),
                ],
            })?);
        }
        drop(statement);
        connection.execute("COMMIT")?;
        connection.execute("BEGIN")?;
        let (operation, relay) = connection
            .as_async()
            .root_cx()
            .create_child_with_local_cancel_relay();
        let records = Counter::new(0);
        let input = CancelOnFirstRecord {
            remaining: &encoded,
            records: &records,
            cancel: Some(|| assert!(relay.cancel_local(CancelReason::UserInterrupt))),
        };
        let descriptor = Cursor::new(codec::encode(&Record::Table {
            table: table.clone(),
        })?);
        let mut cursor = ProjectionCursor::new(
            descriptor.chain(input),
            projection_header(codec::VERSION, 20),
        )?;
        assert!(matches!(cursor.next()?, Some(Record::Table { .. })));
        let error = {
            let _binding = connection.as_async().bind_operation_cx(&operation);
            compare_table_rows(&connection, &table, &mut cursor)
                .expect_err("cancellation must stop a running database comparison")
        };
        assert!((1..2048).contains(&records.get()));
        let message = error.to_string();
        assert!(message.contains(&format!("after {} complete rows", records.get())));
        assert!(message.contains("FrankenSQLite Abort (code 4)"));
        assert!(!message.contains("PRIVATE-PROJECTION-CONTENT"));
        assert!(matches!(
            error.downcast_ref::<FrankenError>(),
            Some(FrankenError::Abort)
        ));
        connection.execute("ROLLBACK")?;
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM messages")?
                .get_typed::<i64>(0)?,
            2048
        );
        connection.close()?;
        Ok(())
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
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => panic!("cannot inspect migrated fixture: {error}"),
                }
            })
            .collect()
    }

    /// A logical archive labelled `source_version`, exported from a current
    /// database. Tables the target added after that version are left out,
    /// as a real archive of that version lacks them, unless
    /// `carry_added_tables` keeps them (a forged or drifted archive).
    fn versioned_archive(
        root: &Path,
        source_version: u32,
        descriptor_drift: bool,
        carry_added_tables: bool,
    ) -> Result<PathBuf> {
        ensure!(
            target_version()? == REVIEWED_TARGET_VERSION,
            "reviewed migration tests require schema v{REVIEWED_TARGET_VERSION}; update policy before accepting a newer target"
        );
        let source = root.join(format!("source-{source_version}.db"));
        let storage = SqliteStorage::open(&source)?;
        storage.ensure_agent(&Agent {
            id: None,
            slug: "migration-fixture".into(),
            name: "Migration Fixture".into(),
            version: Some("v20-data".into()),
            kind: AgentKind::Cli,
        })?;
        drop(storage);

        let connection = export::open_source(&source)?;
        let header = Header {
            format: codec::FORMAT.to_owned(),
            schema_version: codec::VERSION,
            archive_id: "reviewed-migration".to_owned(),
            exported_at_ms: 1,
            storage_schema_version: source_version.to_string(),
            record_types: ["table", "row", "completion"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            contains_private_data: true,
            omissions: vec!["derived_search_assets".to_owned()],
        };
        let mut validator = Validator::new(header.clone())?;
        let path = root.join(format!("schema-{source_version}.jsonl"));
        let mut output = File::create(&path)?;
        output.write_all(&codec::encode(&Record::Header {
            header: header.clone(),
        })?)?;

        for mut table in export::tables(&connection)? {
            if !carry_added_tables
                && source_version < REVIEWED_TARGET_VERSION
                && TABLES_ADDED_SINCE_REVIEWED_SOURCES.contains(&table.name.as_str())
            {
                continue;
            }
            if descriptor_drift && table.name == "agents" {
                let removed_offset = table
                    .columns
                    .iter()
                    .position(|column| column == "version")
                    .context("fixture agent version column missing")?;
                ensure!(!table.primary_key.contains(&removed_offset));
                table.columns.remove(removed_offset);
                for offset in &mut table.primary_key {
                    if *offset > removed_offset {
                        *offset -= 1;
                    }
                }
            }
            table.validate()?;
            output.write_all(&validator.push(&Record::Table {
                table: table.clone(),
            })?)?;
            let columns = table
                .columns
                .iter()
                .map(|column| export::quoted(column))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let order = table
                .primary_key
                .iter()
                .map(|&offset| {
                    Ok(format!(
                        "{} COLLATE BINARY ASC",
                        export::quoted(&table.columns[offset])?
                    ))
                })
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let predicate = if table.name == MIGRATIONS_TABLE {
                format!(" WHERE version <= {source_version}")
            } else {
                String::new()
            };
            let sql = format!(
                "SELECT {columns} FROM {}{predicate} ORDER BY {order}",
                export::quoted(&table.name)?
            );
            let mut failure = None;
            let streamed = connection.query_with_params_for_each(&sql, &[], |row| {
                let result = (|| -> Result<()> {
                    let mut cells = export::cells(row.values())?;
                    if table.name == META_TABLE {
                        let key = table
                            .columns
                            .iter()
                            .position(|column| column == "key")
                            .and_then(|offset| cells.get(offset));
                        if key.is_some_and(
                            |cell| matches!(cell, Cell::Text(value) if value == SCHEMA_VERSION_KEY),
                        ) && let Some(value_offset) =
                            table.columns.iter().position(|column| column == "value")
                        {
                            cells[value_offset] = Cell::Text(source_version.to_string());
                        }
                    }
                    output.write_all(&validator.push(&Record::Row { values: cells })?)?;
                    Ok(())
                })();
                if let Err(error) = result {
                    failure = Some(error);
                    return Err(FrankenError::Internal(
                        "fixture archive writer aborted".into(),
                    ));
                }
                Ok(())
            });
            if let Some(error) = failure {
                return Err(error);
            }
            streamed?;
        }
        let completion = validator.completion();
        output.write_all(&validator.push(&Record::Completion { completion })?)?;
        output.flush()?;
        validator.finish()?;
        connection.execute("ROLLBACK")?;
        connection.close_without_checkpoint()?;
        Ok(path)
    }

    #[test]
    fn inspected_source_marker_must_match_before_a_migration_candidate_is_created() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 20, false, false)?;
        let mut reader = BufReader::new(File::open(&input)?);
        let Some(Record::Header { header }) = codec::read_record(&mut reader, 1)? else {
            bail!("fixture header missing");
        };
        let mut records = Vec::new();
        let mut line = 2;
        let mut table = None;
        let mut marker_offset = None;
        while let Some(record) = codec::read_record(&mut reader, line)? {
            match &record {
                Record::Table { table: next } => table = Some(next.clone()),
                Record::Row { values }
                    if table
                        .as_ref()
                        .is_some_and(|table| meta_schema_version_row(table, values)) =>
                {
                    let value = table
                        .as_ref()
                        .and_then(|table| table.columns.iter().position(|name| name == "value"))
                        .context("fixture marker value column missing")?;
                    marker_offset = Some((records.len(), value));
                }
                Record::Completion { .. } => break,
                Record::Row { .. } | Record::Header { .. } => {}
            }
            records.push(record);
            line += 1;
        }
        let (marker_row, marker_column) = marker_offset.context("fixture marker missing")?;
        for (case, replacement) in [
            None,
            Some(Cell::Null),
            Some(Cell::Integer(20)),
            Some(Cell::Text("21".into())),
            Some(Cell::Text("PRIVATE-FALSE-VERSION".into())),
        ]
        .into_iter()
        .enumerate()
        {
            let mut changed = records.clone();
            if let Some(replacement) = replacement {
                let Record::Row { values } = &mut changed[marker_row] else {
                    bail!("fixture marker is not a row");
                };
                values[marker_column] = replacement;
            } else {
                changed.remove(marker_row);
            }
            let (wire, _) = archive_wire(header.clone(), &changed)?;
            codec::verify(&mut Cursor::new(&wire))?;
            let changed_input = root.path().join(format!("marker-case-{case}.jsonl"));
            fs::write(&changed_input, &wire)?;
            let destination = root.path().join(format!("refused-{case}.db"));
            let error =
                import_compatible(&changed_input, &destination, "reviewed-migration", false)
                    .err()
                    .context("a false source marker was accepted")?;
            assert!(
                error.to_string().contains("schema metadata disagree")
                    || error
                        .to_string()
                        .contains("lacks the schema_version marker")
            );
            assert!(!error.to_string().contains("PRIVATE-"));
            assert!(!destination.exists());
            assert!(
                !root
                    .path()
                    .join(format!(".refused-{case}.db.logical-archive.lock"))
                    .exists()
            );
            assert_eq!(fs::read(&changed_input)?, wire);
        }
        Ok(())
    }

    #[test]
    fn reviewed_migrations_restore_rows_under_current_authority() -> Result<()> {
        for (source, mode) in [(20, "reviewed_v20_to_v22"), (21, "reviewed_v21_to_v22")] {
            let root = tempfile::tempdir()?;
            let input = versioned_archive(root.path(), source, false, false)?;
            let destination = root.path().join("restored.db");
            let outcome = import_compatible(&input, &destination, "reviewed-migration", false)?;
            let receipt = outcome.migration.context("migration receipt missing")?;
            assert_eq!(receipt.mode, mode);
            assert_eq!(receipt.from_storage_schema_version, source.to_string());
            assert_eq!(
                receipt.to_storage_schema_version,
                REVIEWED_TARGET_VERSION.to_string()
            );
            assert!(receipt.source_rows_verified);

            let storage = SqliteStorage::open_readonly(&destination)?;
            assert_eq!(
                u32::try_from(storage.schema_version()?)?,
                REVIEWED_TARGET_VERSION
            );
            let row = storage
                .raw()
                .query_row("SELECT slug, version FROM agents WHERE slug = 'migration-fixture'")?;
            assert_eq!(row.get_typed::<String>(0)?, "migration-fixture");
            assert_eq!(row.get_typed::<Option<String>>(1)?, Some("v20-data".into()));
            // The table v22 added exists, empty, as an in-place upgrade leaves it.
            let forgotten = storage
                .raw()
                .query_row("SELECT COUNT(*) FROM forgotten_sources")?;
            assert_eq!(forgotten.get_typed::<i64>(0)?, 0);
        }
        Ok(())
    }

    #[test]
    fn repeated_reviewed_migration_is_read_only_and_reports_unchanged() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 21, false, false)?;
        let destination = root.path().join("restored.db");
        let created = import_compatible(&input, &destination, "reviewed-migration", false)?;
        assert!(created.created);
        let before = database_files(&destination);
        let repeated = import_compatible(&input, &destination, "reviewed-migration", true)?;
        assert!(!repeated.created);
        assert!(repeated.migration.is_some());
        assert_eq!(before, database_files(&destination));
        Ok(())
    }

    #[test]
    fn reviewed_retry_rejects_sparse_current_history_even_when_projection_matches() -> Result<()> {
        for source in [20, 21] {
            let root = tempfile::tempdir()?;
            let input = versioned_archive(root.path(), source, false, false)?;
            let destination = root.path().join("restored.db");
            import_compatible(&input, &destination, "reviewed-migration", false)?;
            let writer = Connection::open(export::path_text(&destination)?)?;
            writer.execute("DELETE FROM _schema_migrations WHERE version = 18")?;
            assert_eq!(
                writer
                    .query_row("SELECT MAX(version) FROM _schema_migrations")?
                    .get_typed::<i64>(0)?,
                i64::from(REVIEWED_TARGET_VERSION)
            );
            writer.close()?;
            let before = database_files(&destination);
            let input_before = fs::read(&input)?;

            // The authenticated projection intentionally ignores migration
            // history differences. It still matches this sparse target, so
            // the current-schema authority check must reject it separately.
            let mut file = super::super::import::open_input(&input)?;
            let inspected = inspect(&mut file, "reviewed-migration")?;
            file.seek(SeekFrom::Start(0))?;
            let reader = export::open_source(&destination)?;
            verify_projection_rows(&reader, BufReader::new(file), &inspected)?;
            reader.execute("ROLLBACK")?;
            reader.close_without_checkpoint()?;

            let error = import_compatible(&input, &destination, "reviewed-migration", true)
                .err()
                .context("a matching projection must not authorize lower-version migration replay")?;
            assert!(error.to_string().contains("missing required migration v18"));
            assert_eq!(before, database_files(&destination));
            assert_eq!(input_before, fs::read(&input)?);
        }
        Ok(())
    }

    #[test]
    fn changed_migrated_destination_is_a_conflict_not_an_overwrite() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 20, false, false)?;
        let destination = root.path().join("restored.db");
        import_compatible(&input, &destination, "reviewed-migration", false)?;
        let writer = Connection::open(export::path_text(&destination)?)?;
        writer.execute(
            "INSERT INTO meta (key, value) VALUES ('operator_note', 'keep migrated note')",
        )?;
        writer.close()?;
        let before = database_files(&destination);
        assert!(import_compatible(&input, &destination, "reviewed-migration", true).is_err());
        assert_eq!(before, database_files(&destination));
        Ok(())
    }

    #[test]
    fn a_row_in_an_added_table_makes_the_migrated_destination_a_conflict() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 21, false, false)?;
        let destination = root.path().join("restored.db");
        import_compatible(&input, &destination, "reviewed-migration", false)?;
        let writer = Connection::open(export::path_text(&destination)?)?;
        writer.execute(
            "INSERT INTO forgotten_sources (source_path, size_bytes, mtime_ms, forgotten_at_ms) \
             VALUES ('/not/from/the/archive.jsonl', 1, 1, 1)",
        )?;
        writer.close()?;
        let before = database_files(&destination);
        assert!(import_compatible(&input, &destination, "reviewed-migration", true).is_err());
        assert_eq!(before, database_files(&destination));
        Ok(())
    }

    #[test]
    fn reviewed_bridge_rejects_canonical_descriptor_drift() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 20, true, false)?;
        let destination = root.path().join("restored.db");
        assert!(import_compatible(&input, &destination, "reviewed-migration", false).is_err());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn older_archive_carrying_a_table_added_later_is_refused() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 21, false, true)?;
        let destination = root.path().join("restored.db");
        assert!(import_compatible(&input, &destination, "reviewed-migration", false).is_err());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn unreviewed_older_schema_is_refused_without_publication() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), 19, false, false)?;
        let destination = root.path().join("restored.db");
        assert!(import_compatible(&input, &destination, "reviewed-migration", false).is_err());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn newer_schema_is_refused_without_publication() -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = versioned_archive(root.path(), REVIEWED_TARGET_VERSION + 1, false, false)?;
        let destination = root.path().join("restored.db");
        assert!(import_compatible(&input, &destination, "reviewed-migration", false).is_err());
        assert!(!destination.exists());
        Ok(())
    }
}
