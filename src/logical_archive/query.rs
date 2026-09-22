//! Read evidence directly from a logical backup without opening SQLite or an index.
//!
//! Search scans every record before returning anything, then rewinds the SAME
//! admitted file to resolve only the retained conversations. Both complete
//! passes must have identical headers and digests. State is bounded by one wire
//! record and the requested page, not the number of messages or conversations.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, BufReader, Seek, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use serde_json::{Value, json};

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};

const MAX_HITS: usize = 100;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_IDENTITY_BYTES: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const SNIPPET_CHARS: usize = 256;

fn column(table: &Table, name: &str) -> Result<usize> {
    table.columns.iter().position(|item| item == name)
        .ok_or_else(|| anyhow!("logical backup is missing a required {}.{name} column", table.name))
}

fn id_key(table: &Table) -> Result<usize> {
    let id = column(table, "id")?;
    ensure!(table.primary_key == [id], "logical backup requires {}.id as its sole primary key", table.name);
    Ok(id)
}

fn integer(values: &[Cell], offset: usize) -> Result<i64> {
    match values.get(offset) {
        Some(Cell::Integer(value)) => Ok(*value),
        _ => bail!("logical evidence requires an integer identity or coordinate"),
    }
}

fn text(values: &[Cell], offset: usize) -> Result<&str> {
    match values.get(offset) {
        Some(Cell::Text(value)) => Ok(value),
        _ => bail!("logical evidence requires text in the selected column"),
    }
}

struct MessageColumns {
    id: usize,
    conversation: usize,
    idx: usize,
    content: usize,
    role: usize,
}

struct ConversationColumns {
    id: usize,
    source_id: usize,
    source_path: usize,
}

enum Rows {
    Messages(MessageColumns),
    Conversations(ConversationColumns),
    Other,
}

impl Rows {
    fn for_table(table: &Table) -> Result<Self> {
        match table.name.as_str() {
            "messages" => Ok(Self::Messages(MessageColumns {
                id: id_key(table)?,
                conversation: column(table, "conversation_id")?,
                idx: column(table, "idx")?,
                content: column(table, "content")?,
                role: column(table, "role")?,
            })),
            "conversations" => Ok(Self::Conversations(ConversationColumns {
                id: id_key(table)?,
                source_id: column(table, "source_id")?,
                source_path: column(table, "source_path")?,
            })),
            _ => Ok(Self::Other),
        }
    }
}

/// Wire-integrity verification is not a database foreign-key/integrity audit.
/// In addition to that verification, callers check the relationships they emit.
fn scan(
    input: &mut impl BufRead,
    mut visit: impl FnMut(&Rows, &[Cell]) -> Result<()>,
) -> Result<(Header, Completion)> {
    let Some(Record::Header { header }) = codec::read_record(input, 1)? else {
        bail!("logical archive must begin with a header");
    };
    let mut validator = Validator::new(header)?;
    let mut current = Rows::Other;
    let mut messages = false;
    let mut conversations = false;
    let mut line = 2_u64;
    while let Some(record) = codec::read_record(input, line)? {
        validator.push(&record).with_context(|| format!("logical evidence record {line}"))?;
        match &record {
            Record::Table { table } => {
                current = Rows::for_table(table)?;
                messages |= matches!(current, Rows::Messages(_));
                conversations |= matches!(current, Rows::Conversations(_));
            }
            Record::Row { values } => visit(&current, values)
                .with_context(|| format!("logical evidence record {line}"))?,
            Record::Header { .. } | Record::Completion { .. } => {}
        }
        line = line.checked_add(1).context("logical record position overflow")?;
    }
    let verified = validator.finish()?;
    ensure!(messages && conversations, "logical backup lacks the messages/conversations evidence schema");
    Ok(verified)
}

#[derive(Serialize)]
struct MessageMatch {
    message_id: i64,
    conversation_id: i64,
    message_index: u64,
    role: String,
    snippet: String,
    snippet_start_byte: usize,
    snippet_end_byte: usize,
    content_bytes: usize,
    match_start_byte: usize,
    match_end_byte: usize,
}

struct Conversation {
    source_id: String,
    source_path: String,
}

fn resolve_conversations(
    input: &mut (impl BufRead + Seek),
    selected: &BTreeSet<i64>,
    expected: &(Header, Completion),
) -> Result<BTreeMap<i64, Conversation>> {
    input.rewind().context("cannot rewind the admitted logical backup")?;
    let mut found = BTreeMap::new();
    let verified = scan(input, |rows, values| {
        if let Rows::Conversations(columns) = rows {
            let id = integer(values, columns.id)?;
            if selected.contains(&id) {
                let source_id = text(values, columns.source_id)?;
                let source_path = text(values, columns.source_path)?;
                ensure!(
                    !source_id.is_empty() && !source_path.is_empty()
                        && source_id.len() <= MAX_IDENTITY_BYTES
                        && source_path.len() <= MAX_IDENTITY_BYTES,
                    "selected conversation identity must contain 1..4096 bytes; identities are never truncated"
                );
                found.insert(id, Conversation {
                    source_id: source_id.to_owned(), source_path: source_path.to_owned(),
                });
            }
        }
        Ok(())
    })?;
    ensure!(&verified == expected, "logical backup changed between evidence selection and identity resolution");
    ensure!(found.len() == selected.len(), "selected message refers to a missing canonical conversation");
    Ok(found)
}

fn excerpt(content: &str, at: usize) -> (String, usize, usize) {
    let start = content[..at].char_indices().rev().take(80).last().map_or(at, |(i, _)| i);
    let end = content[start..].char_indices().nth(SNIPPET_CHARS)
        .map_or(content.len(), |(i, _)| start + i);
    (content[start..end].to_owned(), start, end)
}

fn validate_search(contains: &str, limit: usize, conversation_id: Option<i64>) -> Result<()> {
    ensure!(!contains.is_empty() && contains.len() <= MAX_QUERY_BYTES, "--contains requires 1..1024 UTF-8 bytes");
    ensure!((1..=MAX_HITS).contains(&limit), "--limit must be between 1 and 100");
    ensure!(conversation_id.is_none_or(|id| id > 0), "--conversation-id must be positive");
    Ok(())
}

// Count actual JSON escaping before stdout publication, without first allocating
// an oversized serialization. The retained evidence itself also has fixed caps.
struct ResponseBudget(usize);

impl Write for ResponseBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(io::Error::other("logical evidence response exceeds 2 MiB"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn bounded_response(value: Value) -> Result<Value> {
    serde_json::to_writer(ResponseBudget(MAX_RESPONSE_BYTES - 1), &value)
        .context("logical evidence response is too large; narrow the requested page")?;
    Ok(value)
}

/// Case-sensitive literal search over complete stored message bodies. There is
/// no tokenization, stemming, fuzzy matching, ranking or implicit provider I/O.
pub fn search(
    path: &Path,
    contains: &str,
    limit: usize,
    conversation_id: Option<i64>,
) -> Result<Value> {
    validate_search(contains, limit, conversation_id)?;
    let mut input = BufReader::new(super::import::open_input(path)?);
    search_stream(&mut input, contains, limit, conversation_id)
}

fn search_stream(
    input: &mut (impl BufRead + Seek),
    contains: &str,
    limit: usize,
    conversation_id: Option<i64>,
) -> Result<Value> {
    validate_search(contains, limit, conversation_id)?;
    let mut total = 0_u64;
    let mut retained = Vec::new();
    let verified = scan(input, |rows, values| {
        if let Rows::Messages(columns) = rows {
            let id = integer(values, columns.id)?;
            let conversation = integer(values, columns.conversation)?;
            let idx = integer(values, columns.idx)?;
            ensure!(id > 0 && conversation > 0 && idx >= 0, "logical message identities or coordinates are invalid");
            let content = text(values, columns.content)?;
            if conversation_id.is_some_and(|expected| expected != conversation) { return Ok(()); }
            let Some(at) = content.find(contains) else { return Ok(()); };
            total = total.checked_add(1).context("logical match count overflow")?;
            if retained.len() == limit { return Ok(()); }
            let role = text(values, columns.role)?;
            ensure!(role.len() <= 128, "selected message role exceeds 128 bytes");
            let (snippet, start, end) = excerpt(content, at);
            retained.push(MessageMatch {
                message_id: id, conversation_id: conversation, message_index: idx as u64 + 1,
                role: role.to_owned(), snippet, snippet_start_byte: start, snippet_end_byte: end,
                content_bytes: content.len(), match_start_byte: at, match_end_byte: at + contains.len(),
            });
        }
        Ok(())
    })?;
    let selected = retained.iter().map(|hit| hit.conversation_id).collect();
    let conversations = resolve_conversations(input, &selected, &verified)?;
    let mut hits = Vec::with_capacity(retained.len());
    for hit in retained {
        let conversation = conversations.get(&hit.conversation_id)
            .context("selected conversation disappeared from bounded evidence")?;
        let mut value = serde_json::to_value(hit)?;
        value["source_id"] = json!(conversation.source_id);
        value["source_path"] = json!(conversation.source_path);
        hits.push(value);
    }
    bounded_response(json!({
        "operation": "search", "format": codec::FORMAT, "schema_version": codec::VERSION,
        "archive_id": verified.0.archive_id, "content_sha256": verified.1.content_sha256,
        "integrity_verified": true, "database_integrity_checked": false,
        "match_mode": "literal_case_sensitive", "order": "message_id",
        "matches": total, "limit": limit, "has_more": total > hits.len() as u64, "hits": hits,
        "coordinate_space": "message_index", "content_source": "logical_archive",
        "preview_only": true, "database_opened": false, "provider_files_opened": false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn table(name: &str, names: &[&str]) -> Record {
        Record::Table { table: Table {
            name: name.into(), columns: names.iter().map(|s| (*s).into()).collect(), primary_key: vec![0],
        }}
    }

    fn conversation(id: i64, source: &str, path: &str) -> Record {
        Record::Row { values: vec![Cell::Integer(id), Cell::Text(source.into()), Cell::Text(path.into())] }
    }

    fn message(id: i64, conversation: i64, idx: i64, body: &str) -> Record {
        Record::Row { values: vec![Cell::Integer(id), Cell::Integer(conversation), Cell::Integer(idx),
            Cell::Text(body.into()), Cell::Text("user".into())] }
    }

    fn records() -> Vec<Record> {
        vec![
            table("conversations", &["id", "source_id", "source_path"]),
            conversation(1, "remote-a", "/absent/same.jsonl"),
            conversation(2, "remote-b", "/absent/same.jsonl"),
            table("messages", &["id", "conversation_id", "idx", "content", "role"]),
            message(1, 1, 0, "needle alpha"), message(2, 2, 7, "δ\0 needle beta"),
            message(3, 1, 99, "Needle differs"), message(4, 2, 1000, "needle gamma"),
        ]
    }

    fn wire(records: Vec<Record>) -> Vec<u8> {
        let header = Header { format: codec::FORMAT.into(), schema_version: codec::VERSION,
            archive_id: "query-fixture".into(), exported_at_ms: 1, storage_schema_version: "17".into(),
            record_types: vec!["table".into(), "row".into(), "completion".into()],
            contains_private_data: true, omissions: vec!["derived_search_assets".into()] };
        let mut bytes = codec::encode(&Record::Header { header: header.clone() }).unwrap();
        let mut validator = Validator::new(header).unwrap();
        for record in records { bytes.extend(validator.push(&record).unwrap()); }
        bytes.extend(codec::encode(&Record::Completion { completion: validator.completion() }).unwrap());
        bytes
    }

    #[test]
    fn literal_search_counts_all_matches_and_resolves_only_bounded_exact_identities() {
        let data = wire(records());
        let result = search_stream(&mut Cursor::new(&data), "needle", 2, None).unwrap();
        assert_eq!(result["matches"], 3);
        assert_eq!(result["has_more"], true);
        assert_eq!(result["hits"][0]["source_id"], "remote-a");
        assert_eq!(result["hits"][1]["source_id"], "remote-b");
        assert_eq!(result["hits"][1]["message_index"], 8);
        assert_eq!(result["hits"][1]["snippet"], "δ\0 needle beta");
        assert_eq!(result["hits"][1]["match_start_byte"], 4);
        assert_eq!(result["database_opened"], false);
        assert_eq!(result["database_integrity_checked"], false);
        let scoped = search_stream(&mut Cursor::new(&data), "needle", 2, Some(1)).unwrap();
        assert_eq!(scoped["matches"], 1);
        let empty = search_stream(&mut Cursor::new(&data), "AND OR *", 2, None).unwrap();
        assert_eq!(empty["matches"], 0);
    }

    #[test]
    fn no_page_can_escape_completion_tamper_or_trailing_record_validation() {
        let valid = wire(records());
        let last = valid[..valid.len() - 1].iter().rposition(|b| *b == b'\n').unwrap() + 1;
        for cut in [last, valid.len() - 1] {
            assert!(search_stream(&mut Cursor::new(&valid[..cut]), "needle", 1, None).is_err());
        }
        let tampered = String::from_utf8(valid.clone()).unwrap().replace("needle gamma", "needle delta");
        assert!(search_stream(&mut Cursor::new(tampered), "needle", 1, None).is_err());
        let mut trailing = valid;
        trailing.extend(b"{}\n");
        assert!(search_stream(&mut Cursor::new(trailing), "needle", 1, None).is_err());
    }

    #[test]
    fn selected_relationships_and_schema_are_not_guessed() {
        let mut rows = records();
        rows.push(message(5, 999, 0, "orphan"));
        assert!(search_stream(&mut Cursor::new(wire(rows)), "orphan", 1, None).is_err());
        let mut rows = records();
        if let Record::Table { table } = &mut rows[3] { table.columns[3] = "unknown".into(); }
        assert!(search_stream(&mut Cursor::new(wire(rows)), "needle", 1, None).is_err());
        let data = wire(vec![table("unrelated", &["id"])]);
        assert!(search_stream(&mut Cursor::new(data), "needle", 1, None).is_err());
    }

    #[test]
    fn descriptors_control_column_positions_not_a_fixed_schema_ordinal() {
        let mut rows = records();
        if let Record::Table { table } = &mut rows[3] { table.columns.swap(0, 3); table.primary_key = vec![3]; }
        for row in &mut rows[4..] { if let Record::Row { values } = row { values.swap(0, 3); } }
        let result = search_stream(&mut Cursor::new(wire(rows)), "needle", 2, None).unwrap();
        assert_eq!(result["hits"][1]["message_id"], 2);
    }

    #[test]
    fn malformed_limits_fail_before_opening_any_input() {
        let missing = Path::new("/nonexistent/query-fixture.jsonl");
        for (needle, limit, conversation) in [("", 1, None), ("x", 0, None), ("x", 101, None), ("x", 1, Some(0))] {
            assert!(search(missing, needle, limit, conversation).is_err());
        }
    }

    #[test]
    fn excerpt_offsets_are_utf8_boundaries_even_beyond_a_large_prefix() {
        let body = format!("{}needle{}", "δ😀".repeat(2000), "é".repeat(1000));
        let at = body.find("needle").unwrap();
        let (snippet, start, end) = excerpt(&body, at);
        assert_eq!(snippet, body[start..end]);
        assert!(snippet.contains("needle"));
        assert_eq!(snippet.chars().count(), SNIPPET_CHARS);
        assert!(start > 0 && end < body.len());
    }

    #[test]
    fn identity_resolution_rejects_a_different_valid_archive() {
        let bytes = wire(records());
        let expected = scan(&mut Cursor::new(bytes), |_, _| Ok(())).unwrap();
        let mut changed = records();
        changed[1] = conversation(1, "different-machine", "/absent/same.jsonl");
        let mut input = Cursor::new(wire(changed));
        assert!(resolve_conversations(&mut input, &BTreeSet::from([1]), &expected).is_err());
    }

    #[test]
    fn response_budget_counts_json_escaping_without_truncation() {
        assert!(bounded_response(json!({"text":"\0".repeat(MAX_RESPONSE_BYTES / 6)})).is_err());
        assert!(bounded_response(json!({"text":"δ\0😀"})).is_ok());
    }
}
