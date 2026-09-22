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
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::codec::{self, Cell, Completion, Header, Record, Table, Validator};

const MAX_HITS: usize = 100;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_IDENTITY_BYTES: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const SNIPPET_CHARS: usize = 256;
const MAX_CONTEXT: usize = 20;
const MAX_VIEW_CONTENT_BYTES: usize = 64 * 1024;
const MAX_CURSOR_BYTES: usize = 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchCursor {
    version: u32,
    content_sha256: String,
    criteria_sha256: String,
    after_message_id: i64,
}

impl SearchCursor {
    fn criteria(contains: &str, conversation_id: Option<i64>) -> Result<String> {
        let bytes = serde_json::to_vec(&("cass.logical_query.v1", contains, conversation_id))?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    fn decode(encoded: &str, contains: &str, conversation_id: Option<i64>) -> Result<Self> {
        ensure!(
            !encoded.is_empty() && encoded.len() <= MAX_CURSOR_BYTES,
            "invalid logical search cursor size"
        );
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| anyhow!("invalid logical search cursor encoding"))?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| anyhow!("invalid logical search cursor"))?;
        ensure!(
            cursor.version == 1 && cursor.after_message_id > 0,
            "unsupported logical search cursor"
        );
        validate_digest(&cursor.content_sha256)?;
        ensure!(
            cursor.criteria_sha256 == Self::criteria(contains, conversation_id)?,
            "logical search cursor belongs to a different query or conversation filter"
        );
        Ok(cursor)
    }

    fn encode(self) -> Result<String> {
        let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&self)?);
        ensure!(
            encoded.len() <= MAX_CURSOR_BYTES,
            "logical search cursor exceeds its budget"
        );
        Ok(encoded)
    }
}

fn column(table: &Table, name: &str) -> Result<usize> {
    table
        .columns
        .iter()
        .position(|item| item == name)
        .ok_or_else(|| {
            anyhow!(
                "logical backup is missing a required {}.{name} column",
                table.name
            )
        })
}

fn id_key(table: &Table) -> Result<usize> {
    let id = column(table, "id")?;
    ensure!(
        table.primary_key == [id],
        "logical backup requires {}.id as its sole primary key",
        table.name
    );
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
        return Err(super::integrity("logical archive must begin with a header"));
    };
    let mut validator = Validator::new(header).map_err(super::integrity_unless_io)?;
    let mut current = Rows::Other;
    let mut messages = false;
    let mut conversations = false;
    let mut line = 2_u64;
    while let Some(record) = codec::read_record(input, line)? {
        validator
            .push(&record)
            .with_context(|| format!("logical evidence record {line}"))
            .map_err(super::integrity_unless_io)?;
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
        line = line
            .checked_add(1)
            .context("logical record position overflow")?;
    }
    let verified = validator.finish().map_err(super::integrity_unless_io)?;
    ensure!(
        messages && conversations,
        "logical backup lacks the messages/conversations evidence schema"
    );
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

fn conversation_identity(values: &[Cell], columns: &ConversationColumns) -> Result<Conversation> {
    let source_id = text(values, columns.source_id)?;
    let source_path = text(values, columns.source_path)?;
    ensure!(
        !source_id.is_empty()
            && !source_path.is_empty()
            && source_id.len() <= MAX_IDENTITY_BYTES
            && source_path.len() <= MAX_IDENTITY_BYTES,
        "selected conversation identity must contain 1..4096 bytes; identities are never truncated"
    );
    Ok(Conversation {
        source_id: source_id.to_owned(),
        source_path: source_path.to_owned(),
    })
}

fn resolve_conversations(
    input: &mut (impl BufRead + Seek),
    selected: &BTreeSet<i64>,
    expected: &(Header, Completion),
) -> Result<BTreeMap<i64, Conversation>> {
    input
        .rewind()
        .context("cannot rewind the admitted logical backup")?;
    let mut found = BTreeMap::new();
    let verified = scan(input, |rows, values| {
        if let Rows::Conversations(columns) = rows {
            let id = integer(values, columns.id)?;
            if selected.contains(&id) {
                found.insert(id, conversation_identity(values, columns)?);
            }
        }
        Ok(())
    })?;
    ensure!(
        &verified == expected,
        "logical backup changed between evidence selection and identity resolution"
    );
    ensure!(
        found.len() == selected.len(),
        "selected message refers to a missing canonical conversation"
    );
    Ok(found)
}

fn excerpt(content: &str, at: usize) -> (String, usize, usize) {
    let start = content[..at]
        .char_indices()
        .rev()
        .take(80)
        .last()
        .map_or(at, |(i, _)| i);
    let end = content[start..]
        .char_indices()
        .nth(SNIPPET_CHARS)
        .map_or(content.len(), |(i, _)| start + i);
    (content[start..end].to_owned(), start, end)
}

fn validate_search(contains: &str, limit: usize, conversation_id: Option<i64>) -> Result<()> {
    ensure!(
        !contains.is_empty() && contains.len() <= MAX_QUERY_BYTES,
        "--contains requires 1..1024 UTF-8 bytes"
    );
    ensure!(
        (1..=MAX_HITS).contains(&limit),
        "--limit must be between 1 and 100"
    );
    ensure!(
        conversation_id.is_none_or(|id| id > 0),
        "--conversation-id must be positive"
    );
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
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
    cursor: Option<&str>,
) -> Result<Value> {
    validate_search(contains, limit, conversation_id)?;
    // Decode before opening the input, including the bounded cursor payload.
    let cursor = cursor
        .map(|value| SearchCursor::decode(value, contains, conversation_id))
        .transpose()?;
    let mut input = BufReader::new(super::import::open_input(path)?);
    search_page(&mut input, contains, limit, conversation_id, cursor)
}

#[cfg(test)]
fn search_stream(
    input: &mut (impl BufRead + Seek),
    contains: &str,
    limit: usize,
    conversation_id: Option<i64>,
) -> Result<Value> {
    search_page(input, contains, limit, conversation_id, None)
}

fn search_page(
    input: &mut (impl BufRead + Seek),
    contains: &str,
    limit: usize,
    conversation_id: Option<i64>,
    cursor: Option<SearchCursor>,
) -> Result<Value> {
    validate_search(contains, limit, conversation_id)?;
    let mut total = 0_u64;
    let mut remaining = 0_u64;
    let mut retained = Vec::new();
    let verified = scan(input, |rows, values| {
        if let Rows::Messages(columns) = rows {
            let id = integer(values, columns.id)?;
            let conversation = integer(values, columns.conversation)?;
            let idx = integer(values, columns.idx)?;
            ensure!(
                id > 0 && conversation > 0 && idx >= 0,
                "logical message identities or coordinates are invalid"
            );
            let content = text(values, columns.content)?;
            if conversation_id.is_some_and(|expected| expected != conversation) {
                return Ok(());
            }
            let Some(at) = content.find(contains) else {
                return Ok(());
            };
            total = total
                .checked_add(1)
                .context("logical match count overflow")?;
            if cursor
                .as_ref()
                .is_some_and(|cursor| id <= cursor.after_message_id)
            {
                return Ok(());
            }
            remaining = remaining
                .checked_add(1)
                .context("logical match count overflow")?;
            if retained.len() == limit {
                return Ok(());
            }
            let role = text(values, columns.role)?;
            ensure!(role.len() <= 128, "selected message role exceeds 128 bytes");
            let (snippet, start, end) = excerpt(content, at);
            retained.push(MessageMatch {
                message_id: id,
                conversation_id: conversation,
                message_index: idx as u64 + 1,
                role: role.to_owned(),
                snippet,
                snippet_start_byte: start,
                snippet_end_byte: end,
                content_bytes: content.len(),
                match_start_byte: at,
                match_end_byte: at + contains.len(),
            });
        }
        Ok(())
    })?;
    if let Some(cursor) = cursor {
        ensure!(
            cursor.content_sha256 == verified.1.content_sha256,
            "logical backup changed since the cursor was issued; restart the search"
        );
    }
    let selected = retained.iter().map(|hit| hit.conversation_id).collect();
    let conversations = resolve_conversations(input, &selected, &verified)?;
    let has_more = remaining > retained.len() as u64;
    let next_cursor = if has_more {
        Some(
            SearchCursor {
                version: 1,
                content_sha256: verified.1.content_sha256.clone(),
                criteria_sha256: SearchCursor::criteria(contains, conversation_id)?,
                after_message_id: retained
                    .last()
                    .context("missing logical continuation anchor")?
                    .message_id,
            }
            .encode()?,
        )
    } else {
        None
    };
    let mut hits = Vec::with_capacity(retained.len());
    for hit in retained {
        let conversation = conversations
            .get(&hit.conversation_id)
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
        "matches": total, "matches_after_cursor": remaining,
        "limit": limit, "has_more": has_more, "next_cursor": next_cursor, "hits": hits,
        "coordinate_space": "message_index", "content_source": "logical_archive",
        "preview_only": true, "contains_private_data": true,
        "database_opened": false, "provider_files_opened": false,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Anchor {
    id: i64,
    conversation: i64,
    idx: i64,
}

impl Anchor {
    fn read(values: &[Cell], columns: &MessageColumns) -> Result<Self> {
        let anchor = Self {
            id: integer(values, columns.id)?,
            conversation: integer(values, columns.conversation)?,
            idx: integer(values, columns.idx)?,
        };
        ensure!(
            anchor.id > 0 && anchor.conversation > 0 && anchor.idx >= 0,
            "logical message identities or coordinates are invalid"
        );
        Ok(anchor)
    }
}

fn validate_view(message_id: i64, context: usize, digest: &str) -> Result<()> {
    ensure!(message_id > 0, "--message-id must be positive");
    ensure!(context <= MAX_CONTEXT, "--context must be between 0 and 20");
    validate_digest(digest)
}

fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "--content-sha256 requires the 64 lowercase hex digits from archive search or verify"
    );
    Ok(())
}

// Primary-key order is not message-index order. Keep the nearest actual
// neighbours while streaming, not idx +/- context and not adjacent wire rows.
// Bodies are deliberately not retained until this bounded selection is final.
fn retain_neighbour(
    selected: &mut BTreeMap<i64, Anchor>,
    anchor: Anchor,
    context: usize,
    preceding: bool,
) -> Result<()> {
    ensure!(
        !selected.contains_key(&anchor.idx),
        "selected conversation contains duplicate message coordinates"
    );
    selected.insert(anchor.idx, anchor);
    if selected.len() > context {
        if preceding {
            selected.pop_first();
        } else {
            selected.pop_last();
        }
    }
    Ok(())
}

/// Resolve an exact search hit against its content digest, then return complete
/// bounded bodies. Different snapshots cannot reuse a numeric message identity.
pub fn view(path: &Path, message_id: i64, context: usize, digest: &str) -> Result<Value> {
    validate_view(message_id, context, digest)?;
    let mut input = BufReader::new(super::import::open_input(path)?);
    view_stream(&mut input, message_id, context, digest)
}

fn view_stream(
    input: &mut (impl BufRead + Seek),
    message_id: i64,
    context: usize,
    digest: &str,
) -> Result<Value> {
    validate_view(message_id, context, digest)?;
    let mut target = None;
    let verified = scan(input, |rows, values| {
        if let Rows::Messages(columns) = rows
            && integer(values, columns.id)? == message_id
        {
            target = Some(Anchor::read(values, columns)?);
        }
        Ok(())
    })?;
    ensure!(
        verified.1.content_sha256 == digest,
        "logical backup does not match --content-sha256; rerun search instead of reusing a different snapshot's message ID"
    );
    let target = target.context(
        "message ID not found in the verified logical backup; no neighbour was substituted",
    )?;

    input
        .rewind()
        .context("cannot rewind the admitted logical backup")?;
    let mut before = BTreeMap::new();
    let mut after = BTreeMap::new();
    let mut before_count = 0_u64;
    let mut after_count = 0_u64;
    let mut conversation = None;
    let second = scan(input, |rows, values| {
        match rows {
            Rows::Conversations(columns) if integer(values, columns.id)? == target.conversation => {
                conversation = Some(conversation_identity(values, columns)?);
            }
            Rows::Messages(columns)
                if integer(values, columns.conversation)? == target.conversation =>
            {
                let anchor = Anchor::read(values, columns)?;
                match anchor.idx.cmp(&target.idx) {
                    std::cmp::Ordering::Equal => ensure!(
                        anchor == target,
                        "selected conversation contains duplicate target coordinates"
                    ),
                    std::cmp::Ordering::Less => {
                        before_count = before_count
                            .checked_add(1)
                            .context("neighbour count overflow")?;
                        retain_neighbour(&mut before, anchor, context, true)?;
                    }
                    std::cmp::Ordering::Greater => {
                        after_count = after_count
                            .checked_add(1)
                            .context("neighbour count overflow")?;
                        retain_neighbour(&mut after, anchor, context, false)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    ensure!(
        second == verified,
        "logical backup changed during context selection"
    );
    let conversation =
        conversation.context("selected message refers to a missing canonical conversation")?;
    let more_before = before_count > before.len() as u64;
    let more_after = after_count > after.len() as u64;
    let anchors: BTreeMap<i64, Anchor> = before
        .into_values()
        .chain(std::iter::once(target))
        .chain(after.into_values())
        .map(|anchor| (anchor.id, anchor))
        .collect();
    ensure!(
        anchors.len() <= 2 * context + 1,
        "logical context exceeded its selection budget"
    );

    input
        .rewind()
        .context("cannot rewind the admitted logical backup")?;
    let mut messages = BTreeMap::new();
    let mut content_bytes = 0_usize;
    let third = scan(input, |rows, values| {
        if let Rows::Messages(columns) = rows {
            let id = integer(values, columns.id)?;
            if let Some(expected) = anchors.get(&id) {
                let actual = Anchor::read(values, columns)?;
                ensure!(
                    &actual == expected,
                    "logical message coordinates changed during body hydration"
                );
                let content = text(values, columns.content)?;
                ensure!(
                    content.len() <= MAX_VIEW_CONTENT_BYTES - content_bytes,
                    "complete logical message window exceeds 64 KiB; reduce context or restore the backup for larger bodies"
                );
                let role = text(values, columns.role)?;
                ensure!(role.len() <= 128, "selected message role exceeds 128 bytes");
                content_bytes += content.len();
                messages.insert(
                    actual.idx,
                    json!({
                        "message_id": id, "message_index": actual.idx as u64 + 1,
                        "content": content, "role": role, "is_target": id == target.id,
                    }),
                );
            }
        }
        Ok(())
    })?;
    ensure!(
        third == verified,
        "logical backup changed during body hydration"
    );
    ensure!(
        messages.len() == anchors.len(),
        "selected logical messages are missing or ambiguous"
    );
    bounded_response(json!({
        "operation": "view", "format": codec::FORMAT, "schema_version": codec::VERSION,
        "archive_id": verified.0.archive_id, "content_sha256": verified.1.content_sha256,
        "integrity_verified": true, "database_integrity_checked": false,
        "source_id": conversation.source_id, "source_path": conversation.source_path,
        "conversation_id": target.conversation, "message_id": target.id,
        "message_index": target.idx as u64 + 1, "context": context,
        "messages": messages.into_values().collect::<Vec<_>>(), "content_bytes": content_bytes,
        "more_before": more_before, "more_after": more_after,
        "coordinate_space": "message_index", "content_source": "logical_archive",
        "preview_only": false, "contains_private_data": true,
        "database_opened": false, "provider_files_opened": false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn table(name: &str, names: &[&str]) -> Record {
        Record::Table {
            table: Table {
                name: name.into(),
                columns: names.iter().map(|s| (*s).into()).collect(),
                primary_key: vec![0],
            },
        }
    }

    fn conversation(id: i64, source: &str, path: &str) -> Record {
        Record::Row {
            values: vec![
                Cell::Integer(id),
                Cell::Text(source.into()),
                Cell::Text(path.into()),
            ],
        }
    }

    fn message(id: i64, conversation: i64, idx: i64, body: &str) -> Record {
        Record::Row {
            values: vec![
                Cell::Integer(id),
                Cell::Integer(conversation),
                Cell::Integer(idx),
                Cell::Text(body.into()),
                Cell::Text("user".into()),
            ],
        }
    }

    fn records() -> Vec<Record> {
        vec![
            table("conversations", &["id", "source_id", "source_path"]),
            conversation(1, "remote-a", "/absent/same.jsonl"),
            conversation(2, "remote-b", "/absent/same.jsonl"),
            table(
                "messages",
                &["id", "conversation_id", "idx", "content", "role"],
            ),
            message(1, 1, 0, "needle alpha"),
            message(2, 2, 7, "δ\0 needle beta"),
            message(3, 1, 99, "Needle differs"),
            message(4, 2, 1000, "needle gamma"),
        ]
    }

    fn wire(records: Vec<Record>) -> Vec<u8> {
        let header = Header {
            format: codec::FORMAT.into(),
            schema_version: codec::VERSION,
            archive_id: "query-fixture".into(),
            exported_at_ms: 1,
            storage_schema_version: "17".into(),
            record_types: vec!["table".into(), "row".into(), "completion".into()],
            contains_private_data: true,
            omissions: vec!["derived_search_assets".into()],
        };
        let mut bytes = codec::encode(&Record::Header {
            header: header.clone(),
        })
        .unwrap();
        let mut validator = Validator::new(header).unwrap();
        for record in records {
            bytes.extend(validator.push(&record).unwrap());
        }
        bytes.extend(
            codec::encode(&Record::Completion {
                completion: validator.completion(),
            })
            .unwrap(),
        );
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
        let last = valid[..valid.len() - 1]
            .iter()
            .rposition(|b| *b == b'\n')
            .unwrap()
            + 1;
        for cut in [last, valid.len() - 1] {
            assert!(search_stream(&mut Cursor::new(&valid[..cut]), "needle", 1, None).is_err());
        }
        let tampered = String::from_utf8(valid.clone())
            .unwrap()
            .replace("needle gamma", "needle delta");
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
        if let Record::Table { table } = &mut rows[3] {
            table.columns[3] = "unknown".into();
        }
        assert!(search_stream(&mut Cursor::new(wire(rows)), "needle", 1, None).is_err());
        let data = wire(vec![table("unrelated", &["id"])]);
        assert!(search_stream(&mut Cursor::new(data), "needle", 1, None).is_err());
    }

    #[test]
    fn descriptors_control_column_positions_not_a_fixed_schema_ordinal() {
        let mut rows = records();
        if let Record::Table { table } = &mut rows[3] {
            table.columns.swap(0, 3);
            table.primary_key = vec![3];
        }
        for row in &mut rows[4..] {
            if let Record::Row { values } = row {
                values.swap(0, 3);
            }
        }
        let result = search_stream(&mut Cursor::new(wire(rows)), "needle", 2, None).unwrap();
        assert_eq!(result["hits"][1]["message_id"], 2);
    }

    #[test]
    fn malformed_limits_fail_before_opening_any_input() {
        let missing = Path::new("/nonexistent/query-fixture.jsonl");
        for (needle, limit, conversation) in [
            ("", 1, None),
            ("x", 0, None),
            ("x", 101, None),
            ("x", 1, Some(0)),
        ] {
            assert!(search(missing, needle, limit, conversation, None).is_err());
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

    fn view_fixture(bytes: &[u8], id: i64, context: usize) -> Result<Value> {
        let (_, completion) = scan(&mut Cursor::new(bytes), |_, _| Ok(()))?;
        view_stream(
            &mut Cursor::new(bytes),
            id,
            context,
            &completion.content_sha256,
        )
    }

    #[test]
    fn full_view_uses_sparse_message_order_not_wire_order_or_source_path() {
        let mut rows = records();
        rows.push(message(5, 2, 2, "earliest"));
        rows.push(message(6, 2, 90, "closest after"));
        let result = view_fixture(&wire(rows), 2, 1).unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m["message_id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [5, 2, 6]
        );
        assert_eq!(
            messages
                .iter()
                .map(|m| m["message_index"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [3, 8, 91]
        );
        assert_eq!(messages[1]["content"], "δ\0 needle beta");
        assert_eq!(
            messages.iter().filter(|m| m["is_target"] == true).count(),
            1
        );
        assert_eq!(result["source_id"], "remote-b");
        assert_eq!(result["source_path"], "/absent/same.jsonl");
        assert_eq!(result["more_before"], false);
        assert_eq!(result["more_after"], true);
        assert_eq!(result["preview_only"], false);
        assert_eq!(result["database_opened"], false);
    }

    #[test]
    fn view_requires_the_selected_snapshot_and_an_exact_existing_message() {
        let bytes = wire(records());
        let error = view_fixture(&bytes, 999, 0).unwrap_err().to_string();
        assert!(error.contains("not found"));
        let (_, original) = scan(&mut Cursor::new(&bytes), |_, _| Ok(())).unwrap();
        let mut rows = records();
        rows[5] = message(2, 2, 7, "different body at the same message ID");
        assert!(view_stream(&mut Cursor::new(wire(rows)), 2, 0, &original.content_sha256).is_err());
        assert!(
            view_stream(
                &mut Cursor::new(&bytes[..bytes.len() - 1]),
                2,
                0,
                &original.content_sha256
            )
            .is_err()
        );
        for (id, context, digest) in [
            (0, 0, original.content_sha256.as_str()),
            (2, 21, original.content_sha256.as_str()),
            (2, 0, "wrong"),
        ] {
            assert!(view(Path::new("/missing-backup"), id, context, digest).is_err());
        }
    }

    #[test]
    fn view_counts_complete_utf8_and_nul_bytes_and_never_truncates_a_body() {
        let exact = "\0é".repeat(MAX_VIEW_CONTENT_BYTES / 3) + "x";
        assert_eq!(exact.len(), MAX_VIEW_CONTENT_BYTES);
        let mut rows = records();
        rows[5] = message(2, 2, 7, &exact);
        let bytes = wire(rows.clone());
        let result = view_fixture(&bytes, 2, 0).unwrap();
        assert_eq!(result["messages"][0]["content"], exact);
        assert_eq!(result["content_bytes"], MAX_VIEW_CONTENT_BYTES);
        assert!(view_fixture(&bytes, 2, 1).is_err());
        rows[5] = message(2, 2, 7, &(exact + "x"));
        assert!(view_fixture(&wire(rows), 2, 0).is_err());
    }

    #[test]
    fn unselected_large_bodies_do_not_consume_the_final_view_window() {
        let mut rows = records();
        // This is discovered before the closer neighbour but must not be
        // retained as content, charged to the final budget, or cause refusal.
        rows[7] = message(4, 2, 1000, &"x".repeat(MAX_VIEW_CONTENT_BYTES + 1));
        rows.push(message(5, 2, 8, "the actually closest neighbour"));
        let result = view_fixture(&wire(rows), 2, 1).unwrap();
        assert_eq!(result["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            result["messages"][1]["content"],
            "the actually closest neighbour"
        );
        assert_eq!(result["more_after"], true);
    }

    #[test]
    fn full_view_rejects_duplicate_target_coordinates_and_selected_orphans() {
        let mut rows = records();
        rows.push(message(5, 2, 7, "same coordinate different ID"));
        assert!(view_fixture(&wire(rows), 2, 0).is_err());
        let mut rows = records();
        rows.push(message(5, 2, 1000, "duplicate closest neighbour"));
        assert!(view_fixture(&wire(rows), 2, 1).is_err());
        let mut rows = records();
        rows.push(message(5, 999, 0, "orphan"));
        assert!(view_fixture(&wire(rows), 5, 0).is_err());
    }

    #[test]
    fn context_metadata_stays_bounded_independent_of_conversation_size() {
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        for idx in (0..10_000).rev() {
            let anchor = Anchor {
                id: idx + 1,
                conversation: 1,
                idx,
            };
            retain_neighbour(&mut before, anchor, MAX_CONTEXT, true).unwrap();
            retain_neighbour(&mut after, anchor, MAX_CONTEXT, false).unwrap();
            assert!(before.len() <= MAX_CONTEXT && after.len() <= MAX_CONTEXT);
        }
        assert_eq!(before.first_key_value().unwrap().0, &9980);
        assert_eq!(after.last_key_value().unwrap().0, &19);
    }

    #[test]
    fn backup_cursors_exhaust_matches_without_duplicates_or_false_more_pages() {
        let bytes = wire(records());
        let first = search_page(&mut Cursor::new(&bytes), "needle", 1, None, None).unwrap();
        let cursor =
            SearchCursor::decode(first["next_cursor"].as_str().unwrap(), "needle", None).unwrap();
        let second =
            search_page(&mut Cursor::new(&bytes), "needle", 2, None, Some(cursor)).unwrap();
        assert_eq!(first["hits"][0]["message_id"], 1);
        assert_eq!(second["matches"], 3);
        assert_eq!(second["matches_after_cursor"], 2);
        assert_eq!(second["hits"][0]["message_id"], 2);
        assert_eq!(second["hits"][1]["message_id"], 4);
        assert_eq!(second["has_more"], false);
        assert!(second["next_cursor"].is_null());
        assert_eq!(first["content_sha256"], second["content_sha256"]);
        let end = SearchCursor {
            version: 1,
            content_sha256: first["content_sha256"].as_str().unwrap().into(),
            criteria_sha256: SearchCursor::criteria("needle", None).unwrap(),
            after_message_id: i64::MAX,
        };
        let exhausted =
            search_page(&mut Cursor::new(&bytes), "needle", 1, None, Some(end)).unwrap();
        assert_eq!(exhausted["matches_after_cursor"], 0);
        assert_eq!(exhausted["has_more"], false);
    }

    #[test]
    fn cursors_reject_changed_criteria_snapshots_and_oversized_payloads() {
        let bytes = wire(records());
        let first = search_stream(&mut Cursor::new(&bytes), "needle", 1, None).unwrap();
        let token = first["next_cursor"].as_str().unwrap();
        assert!(SearchCursor::decode(token, "Needle", None).is_err());
        assert!(SearchCursor::decode(token, "needle", Some(1)).is_err());
        let mut rows = records();
        rows[7] = message(4, 2, 1000, "changed snapshot needle");
        let cursor = SearchCursor::decode(token, "needle", None).unwrap();
        assert!(
            search_page(
                &mut Cursor::new(wire(rows)),
                "needle",
                1,
                None,
                Some(cursor)
            )
            .is_err()
        );
        let oversize = "x".repeat(MAX_CURSOR_BYTES + 1);
        for invalid in ["", "{}", oversize.as_str()] {
            assert!(
                search(
                    Path::new("/nonexistent/backup"),
                    "needle",
                    1,
                    None,
                    Some(invalid)
                )
                .is_err()
            );
        }
    }
}
