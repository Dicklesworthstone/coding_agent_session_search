//! Opt-in additive migration for verified logical archives from older CASS schemas.
//!
//! Exact-schema restoration remains the default. This module accepts only an older
//! archive whose declared tables/columns are an unambiguous subset of today's
//! canonical schema with identical primary-key names/order. The current initializer
//! remains the sole executable schema authority; no SQL or path from the archive is
//! executed. Missing current columns are left to SQLite defaults/NULL constraints.
//!
//! Schema bookkeeping is intentionally transformed rather than replayed:
//! `_schema_migrations` stays at the current initializer's complete history and the
//! legacy `meta.schema_version` row stays current. Tables introduced after the
//! archived schema keep initializer-owned seed state. All other archived rows are
//! compared back from the persisted publication image before it can become visible.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use coding_agent_search::franken_sync::compat::RowExt;
use coding_agent_search::franken_sync::{Connection, FrankenError, SqliteValue};
use coding_agent_search::storage::sqlite::{CURRENT_SCHEMA_VERSION, SqliteStorage};

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};
use super::export::{self, DestinationLock};

const MAX_BATCH_RECORDS: usize = 128;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_TRIGGER_BYTES: usize = 1024 * 1024;
const MIGRATIONS_TABLE: &str = "_schema_migrations";
const META_TABLE: &str = "meta";
const SCHEMA_VERSION_KEY: &str = "schema_version";

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

fn inspect(file: &mut File, expected_archive_id: &str) -> Result<Inspected> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let Some(Record::Header { header }) = codec::read_record(&mut reader, 1)? else {
        bail!("logical archive must begin with a header");
    };
    header.validate()?;
    ensure!(
        header.archive_id == expected_archive_id,
        "logical archive identity does not match --archive-id"
    );
    let mut validator = Validator::new(header.clone())?;
    let mut tables = Vec::new();
    let mut line = 2_u64;
    while let Some(record) = codec::read_record(&mut reader, line)? {
        if let Record::Table { table } = &record {
            tables.push(table.clone());
        }
        validator
            .push(&record)
            .map_err(|error| anyhow!("record {line}: {error}"))?;
        line = line
            .checked_add(1)
            .ok_or_else(|| anyhow!("logical record position overflow"))?;
    }
    let (verified_header, completion) = validator.finish()?;
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

fn primary_key_names(table: &Table) -> Vec<&str> {
    table
        .primary_key
        .iter()
        .map(|&offset| table.columns[offset].as_str())
        .collect()
}

fn compatible_tables(connection: &Connection, archived: &[Table]) -> Result<Vec<Table>> {
    let current = export::tables(connection)?;
    for source in archived {
        let target = current
            .iter()
            .find(|table| table.name == source.name)
            .ok_or_else(|| {
                anyhow!(
                    "older archive table {} no longer exists in the current canonical schema",
                    source.name
                )
            })?;
        ensure!(
            primary_key_names(source) == primary_key_names(target),
            "older archive table {} changed primary-key identity; automatic migration is unsafe",
            source.name
        );
        for column in &source.columns {
            ensure!(
                target.columns.iter().any(|candidate| candidate == column),
                "older archive column {}.{} no longer exists; automatic migration is unsafe",
                source.name,
                column
            );
        }
    }
    Ok(current)
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
                    u64::from_str_radix(&bits, 16)
                        .map_err(|_| anyhow!("invalid REAL encoding"))?,
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
    let rows = connection.query(
        "SELECT name, substr(sql, 1, 65537) FROM sqlite_master WHERE type = 'trigger' ORDER BY name LIMIT 257",
    )?;
    ensure!(
        rows.len() <= 256,
        "canonical schema exceeds restore trigger limit"
    );
    let mut statements = Vec::new();
    let mut bytes = 0_usize;
    for row in rows {
        let name = row.get_typed::<String>(0)?;
        let sql = row.get_typed::<String>(1)?;
        bytes = bytes.saturating_add(sql.len());
        ensure!(
            sql.len() <= 65_536 && bytes <= MAX_TRIGGER_BYTES,
            "canonical trigger definitions exceed restore budget"
        );
        connection.execute(&format!("DROP TRIGGER {}", export::quoted(&name)?))?;
        statements.push(sql);
    }
    Ok(statements)
}

fn verify_current_schema_authority(connection: &Connection) -> Result<()> {
    let expected = i64::from(target_version()?);
    let version = connection
        .query_row("SELECT MAX(version) FROM _schema_migrations")?
        .get_typed::<i64>(0)?;
    ensure!(
        version == expected,
        "migrated candidate does not retain the current schema-migration authority"
    );
    let legacy = export::schema_version(connection)?;
    ensure!(
        legacy == expected.to_string(),
        "migrated candidate legacy schema marker is not current"
    );
    Ok(())
}

fn clear_archived_data(connection: &Connection, archived: &[Table]) -> Result<()> {
    for table in archived {
        if table.name == MIGRATIONS_TABLE {
            continue;
        }
        if table.name == META_TABLE {
            connection.execute("DELETE FROM \"meta\" WHERE \"key\" <> 'schema_version'")?;
        } else {
            connection.execute(&format!("DELETE FROM {}", export::quoted(&table.name)?))?;
        }
    }
    Ok(())
}

fn restore_older<R: BufRead>(
    connection: &Connection,
    input: &mut Input<R>,
    inspected: &Inspected,
) -> Result<()> {
    let source_version = parse_source_version(&inspected.header)?;
    let target = target_version()?;
    ensure!(
        source_version < target,
        "--allow-compatible-schema accepts only archives older than this binary; current-schema archives use exact restoration"
    );
    compatible_tables(connection, &inspected.tables)?;
    verify_current_schema_authority(connection)?;

    let Some(Record::Header { header }) = input.record(1)? else {
        bail!("logical archive must begin with a header");
    };
    ensure!(
        header == inspected.header,
        "logical archive changed between validation and replay"
    );
    let mut validator = Validator::new(header)?;
    connection.execute("PRAGMA foreign_keys = OFF")?;
    connection.execute("BEGIN IMMEDIATE")?;
    let triggers = suspend_triggers(connection)?;
    // Clear only tables represented by the older archive. Tables introduced by
    // the current schema retain initializer-owned seed state.
    clear_archived_data(connection, &inspected.tables)?;

    let mut statement = None;
    let mut current_table: Option<Table> = None;
    let mut table_count = 0_usize;
    let mut saw_schema_marker = false;
    let mut batch_records = 0_usize;
    let mut batch_bytes = 0_usize;
    let mut line = 2_u64;

    while let Some(record) = input.record(line)? {
        if batch_records == MAX_BATCH_RECORDS
            || input.record_bytes > MAX_BATCH_BYTES.saturating_sub(batch_bytes)
        {
            connection.execute("COMMIT")?;
            connection.execute("BEGIN IMMEDIATE")?;
            batch_records = 0;
            batch_bytes = 0;
        }
        validator
            .push(&record)
            .map_err(|error| anyhow!("record {line}: {error}"))?;
        match record {
            Record::Table { table } => {
                ensure!(
                    inspected.tables.get(table_count) == Some(&table),
                    "record {line}: logical table descriptor changed after validation"
                );
                statement = if table.name == MIGRATIONS_TABLE {
                    None
                } else {
                    Some(connection.prepare(&insert_sql(&table)?)?)
                };
                current_table = Some(table);
                table_count += 1;
            }
            Record::Row { values: cells } => {
                let table = current_table
                    .as_ref()
                    .ok_or_else(|| anyhow!("record {line}: row precedes its table"))?;
                if table.name == MIGRATIONS_TABLE {
                    // The current initializer's migration ledger is authoritative.
                } else if meta_schema_version_row(table, &cells) {
                    saw_schema_marker = true;
                } else {
                    statement
                        .as_ref()
                        .ok_or_else(|| anyhow!("record {line}: no compatible insert target"))?
                        .execute_with_params(&values(cells)?)
                        .map_err(|_| {
                            anyhow!(
                                "record {line}: row cannot satisfy the current additive schema; no destination was published"
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

    let (header, completion) = validator.finish()?;
    ensure!(
        (header, completion) == (inspected.header.clone(), inspected.completion.clone()),
        "logical archive changed during compatible replay"
    );
    ensure!(
        table_count == inspected.tables.len(),
        "logical archive table set changed during compatible replay"
    );
    ensure!(
        saw_schema_marker,
        "older logical archive lacks the schema_version marker required for additive migration"
    );

    drop(statement);
    for trigger in triggers {
        connection.execute_batch(&trigger)?;
    }
    verify_current_schema_authority(connection)?;
    super::import::verify_database(connection)?;
    connection.execute("COMMIT")?;
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
    connection
        .execute_with_params(
            "VACUUM INTO ?1",
            &[SqliteValue::Text(export::path_text(candidate)?.into())],
        )
        .context("cannot materialize a self-contained migrated publication image")?;
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

struct ProjectionCursor<R> {
    reader: R,
    line: u64,
    pending: Option<Record>,
}

impl<R: BufRead> ProjectionCursor<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: 1,
            pending: None,
        }
    }

    fn next(&mut self) -> Result<Option<Record>> {
        if self.pending.is_some() {
            return Ok(self.pending.take());
        }
        let line = self.line;
        let record = codec::read_record(&mut self.reader, line)?;
        if record.is_some() {
            self.line = self
                .line
                .checked_add(1)
                .ok_or_else(|| anyhow!("logical record position overflow"))?;
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
    let columns = table
        .columns
        .iter()
        .map(|name| export::quoted(name))
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
    let sql = format!(
        "SELECT {columns} FROM {} ORDER BY {order}",
        export::quoted(&table.name)?
    );

    let mut failure = None;
    let streamed = connection.query_with_params_for_each(&sql, &[], |row| {
        let result = (|| -> Result<()> {
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
            Ok(())
        })();
        if let Err(error) = result {
            failure = Some(error);
            return Err(FrankenError::Internal(
                "compatible archive projection comparison aborted".to_owned(),
            ));
        }
        Ok(())
    });
    if let Some(error) = failure {
        return Err(error);
    }
    streamed.map_err(|_| anyhow!("cannot compare persisted migrated table {}", table.name))?;

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

fn verify_persisted_projection(
    file: &mut File,
    candidate: &Path,
    inspected: &Inspected,
) -> Result<()> {
    let reader = export::open_source(candidate)?;
    super::import::verify_database(&reader)?;
    verify_current_schema_authority(&reader)?;
    compatible_tables(&reader, &inspected.tables)?;

    file.seek(SeekFrom::Start(0))?;
    let verified = codec::verify(&mut BufReader::new(&mut *file))?;
    ensure!(
        verified == (inspected.header.clone(), inspected.completion.clone()),
        "logical archive changed after compatible replay"
    );

    file.seek(SeekFrom::Start(0))?;
    let mut cursor = ProjectionCursor::new(BufReader::new(&mut *file));
    let Some(Record::Header { header }) = cursor.next()? else {
        bail!("logical archive must begin with a header");
    };
    ensure!(header == inspected.header, "logical archive header changed");

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
            compare_table_rows(&reader, &table, &mut cursor)?;
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

    reader.execute("ROLLBACK")?;
    reader.close_without_checkpoint()?;
    require_candidate_without_sidecars(candidate)?;
    Ok(())
}

pub fn import_compatible(
    input_path: &Path,
    destination: &Path,
    expected_archive_id: &str,
) -> Result<MigrationOutcome> {
    let mut file = super::import::open_input(input_path)?;
    let inspected = inspect(&mut file, expected_archive_id)?;
    let source_version = parse_source_version(&inspected.header)?;
    let target = target_version()?;

    if source_version == target {
        let (header, completion, created) = super::import::import_file_with_policy(
            input_path,
            destination,
            expected_archive_id,
            false,
        )?;
        return Ok(MigrationOutcome {
            header,
            completion,
            created,
            migration: None,
        });
    }
    ensure!(
        source_version < target,
        "logical archive schema is newer than this binary; downgrade migration is not supported"
    );

    let _lock = DestinationLock::acquire(destination)?;
    require_new_destination(destination)?;
    let staging = tempfile::Builder::new()
        .prefix(".cass-migrate-")
        .tempdir_in(export::parent(destination)?)?;
    let replay_path = staging.path().join("agent_search.db");
    let candidate = staging.path().join("publication.db");

    let storage = SqliteStorage::open(&replay_path)
        .context("cannot initialize compatible restore candidate")?;
    drop(storage);
    let connection = Connection::open(export::path_text(&replay_path)?)?;
    connection.execute("PRAGMA busy_timeout = 5000")?;
    let mode = connection
        .query_row("PRAGMA journal_mode = WAL")?
        .get_typed::<String>(0)?;
    ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "cannot enable WAL for private compatible replay"
    );
    connection.execute("PRAGMA synchronous = FULL")?;

    file.seek(SeekFrom::Start(0))?;
    let mut bounded = Input::new(BufReader::new(&mut file));
    restore_older(&connection, &mut bounded, &inspected)?;
    materialize_candidate(&connection, &candidate)?;
    connection.close()?;

    verify_persisted_projection(&mut file, &candidate, &inspected)?;
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
        migration: Some(SchemaMigrationReceipt {
            mode: "compatible_additive",
            from_storage_schema_version: inspected.header.storage_schema_version,
            to_storage_schema_version: target.to_string(),
            schema_authority: "current_binary_initializer",
            source_rows_verified: true,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_agent_search::model::types::{Agent, AgentKind};
    use coding_agent_search::storage::sqlite::SqliteStorage;
    use std::io::Write;

    fn older_archive(
        root: &Path,
        remove_required_agent_column: bool,
    ) -> Result<(PathBuf, String)> {
        let source = root.join("source.db");
        let storage = SqliteStorage::open(&source)?;
        storage.ensure_agent(&Agent {
            id: None,
            slug: "migration-fixture".into(),
            name: "Migration Fixture".into(),
            version: Some("old-version".into()),
            kind: AgentKind::Cli,
        })?;
        drop(storage);

        let connection = export::open_source(&source)?;
        let target = target_version()?;
        ensure!(target > 1, "migration fixture requires a version above one");
        let from = target - 1;
        let header = Header {
            format: codec::FORMAT.to_owned(),
            schema_version: codec::VERSION,
            archive_id: "older-compatible".to_owned(),
            exported_at_ms: 1,
            storage_schema_version: from.to_string(),
            record_types: ["table", "row", "completion"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            contains_private_data: true,
            omissions: vec!["derived_search_assets".to_owned()],
        };
        let mut validator = Validator::new(header.clone())?;
        let path = root.join("older.jsonl");
        let mut output = File::create(&path)?;
        output.write_all(&codec::encode(&Record::Header {
            header: header.clone(),
        })?)?;

        for mut table in export::tables(&connection)? {
            if table.name == MIGRATIONS_TABLE {
                continue;
            }
            if table.name == "agents" {
                let removed = if remove_required_agent_column {
                    "slug"
                } else {
                    "version"
                };
                let removed_offset = table
                    .columns
                    .iter()
                    .position(|column| column == removed)
                    .context("fixture agent column missing")?;
                ensure!(
                    !table.primary_key.contains(&removed_offset),
                    "fixture must not remove an agent primary key"
                );
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
            let sql = format!(
                "SELECT {columns} FROM {} ORDER BY {order}",
                export::quoted(&table.name)?
            );
            let mut failure = None;
            connection.query_with_params_for_each(&sql, &[], |row| {
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
                        ) {
                            if let Some(value_offset) =
                                table.columns.iter().position(|column| column == "value")
                            {
                                cells[value_offset] = Cell::Text(from.to_string());
                            }
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
            })?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        let completion = validator.completion();
        output.write_all(&validator.push(&Record::Completion { completion })?)?;
        output.flush()?;
        validator.finish()?;
        connection.execute("ROLLBACK")?;
        connection.close_without_checkpoint()?;
        Ok((path, from.to_string()))
    }

    #[test]
    fn additive_migration_restores_older_subset_into_current_schema() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (input, from) = older_archive(root.path(), false)?;
        let destination = root.path().join("restored.db");
        let outcome = import_compatible(&input, &destination, "older-compatible")?;
        let receipt = outcome.migration.context("migration receipt missing")?;
        assert_eq!(receipt.from_storage_schema_version, from);
        assert_eq!(
            receipt.to_storage_schema_version,
            target_version()?.to_string()
        );
        assert!(receipt.source_rows_verified);
        assert!(destination.is_file());

        let storage = SqliteStorage::open_readonly(&destination)?;
        assert_eq!(u32::try_from(storage.schema_version()?)?, target_version()?);
        let row = storage
            .raw()
            .query_row("SELECT slug, version FROM agents WHERE slug = 'migration-fixture'")?;
        assert_eq!(row.get_typed::<String>(0)?, "migration-fixture");
        assert_eq!(row.get_typed::<Option<String>>(1)?, None);
        Ok(())
    }

    #[test]
    fn missing_current_required_column_fails_without_publication() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (input, _) = older_archive(root.path(), true)?;
        let destination = root.path().join("restored.db");
        assert!(import_compatible(&input, &destination, "older-compatible").is_err());
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn future_schema_is_refused_without_creating_a_destination() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (input, _) = older_archive(root.path(), false)?;
        let current = format!(
            "\"storage_schema_version\":\"{}\"",
            target_version()? - 1
        );
        let future = format!(
            "\"storage_schema_version\":\"{}\"",
            target_version()? + 1
        );
        let text = fs::read_to_string(&input)?;
        fs::write(&input, text.replacen(&current, &future, 1))?;
        let destination = root.path().join("restored.db");
        assert!(import_compatible(&input, &destination, "older-compatible").is_err());
        assert!(!destination.exists());
        Ok(())
    }
}
