//! Exact canonical message addressing for search follow-ups (GH #493).
//!
//! Search's `line_number` is `messages.idx + 1`, not a physical file line.
//! Keep this lane independent of raw-file rendering, connector filtering, and
//! vector positions. One read transaction observes identity and messages
//! together; it never reparses or mutates the source file or archive.

use crate::franken_sync::compat::{ConnectionExt, RowExt};
use crate::robot_budget_envelope::RobotBudget;
use crate::storage::sqlite::FrankenStorage;
use crate::{CliError, CliErrorKind, CliResult, RobotFormat, ViewWindow};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct Request {
    path: PathBuf,
    db: PathBuf,
    source: Option<String>,
    conversation_id: Option<i64>,
    message_index: usize,
    context: usize,
}

mod stream;
mod window;

const DEADLINE_EXCEEDED: &str = "followup-deadline-exceeded";

fn check_deadline(budget: &RobotBudget) -> CliResult<()> {
    if budget.is_exhausted() {
        return Err(deadline_error());
    }
    Ok(())
}

fn deadline_error() -> CliError {
    CliError {
        code: 9,
        kind: DEADLINE_EXCEEDED,
        message: "Follow-up request deadline exhausted".into(),
        hint: None,
        retryable: true,
    }
}

fn resource_limit(reason: &str) -> CliError {
    error(
        "followup-resource-limit",
        reason,
        "Reduce -C/--context or inspect the source with a streaming tool. No partial target was emitted. Follow-ups admit at most 8 MiB per record, 32 MiB of window text and 4096 records.",
    )
}

fn error(kind: &'static str, message: impl Into<String>, hint: &str) -> CliError {
    CliError {
        code: 2,
        kind,
        message: message.into(),
        hint: Some(hint.to_string()),
        retryable: false,
    }
}

fn lookup_error(err: impl std::fmt::Display) -> CliError {
    error(
        CliErrorKind::IndexedSessionRequired.kind_str(),
        format!("Canonical message lookup failed: {err}"),
        "Check the archive used by search; no raw-file fallback was attempted.",
    )
}

fn resolve(request: &Request, expand: bool, budget: &RobotBudget) -> CliResult<Value> {
    check_deadline(budget)?;
    if request.message_index == 0 {
        return Err(error(
            CliErrorKind::InvalidLine.kind_str(),
            "Message indices start at 1, not 0",
            "Pass search's line_number unchanged to --message-index.",
        ));
    }
    if !request.db.is_file() {
        return Err(error(
            CliErrorKind::IndexedSessionRequired.kind_str(),
            "Canonical message lookup requires an existing archive",
            "Use the same --db as search. --message-index never falls back to file lines.",
        ));
    }
    let storage = FrankenStorage::open_strict_readonly(&request.db).map_err(lookup_error)?;
    check_deadline(budget)?;
    // Pin identity selection, index validation and content hydration to one
    // read transaction. Never choose against one snapshot and render another.
    storage
        .raw()
        .execute("BEGIN DEFERRED")
        .map_err(lookup_error)?;
    let result = resolve_snapshot(request, expand, &storage, budget);
    let released = storage.raw().execute("ROLLBACK").map_err(lookup_error);
    match result {
        Err(err) => Err(err),
        Ok(payload) => released.map(|_| payload),
    }
}

fn resolve_snapshot(
    request: &Request,
    expand: bool,
    storage: &FrankenStorage,
    budget: &RobotBudget,
) -> CliResult<Value> {
    check_deadline(budget)?;
    let source_sql = crate::normalized_source_identity_sql_expr("c.source_id", "c.origin_host");
    // Resolve ambiguity before reading ANY message content. Empty conversations
    // still participate, including multiple sessions stored in one provider DB.
    let sql = format!(
        "SELECT c.id, {source_sql} FROM conversations c
         WHERE c.source_path = ?1
           AND (?2 IS NULL OR {source_sql} = ?2)
           AND (?3 IS NULL OR c.id = ?3)
         ORDER BY c.id LIMIT 2"
    );
    let path = request.path.to_string_lossy().into_owned();
    let conversations = storage
        .raw()
        .query_map_collect(
            &sql,
            crate::franken_sync::params![
                path.as_str(),
                request.source.as_deref(),
                request.conversation_id
            ],
            |row| Ok((row.get_typed::<i64>(0)?, row.get_typed::<String>(1)?)),
        )
        .map_err(lookup_error)?;
    check_deadline(budget)?;
    let (conversation_id, source_id) = conversations.first().ok_or_else(|| {
        error(
            CliErrorKind::IndexedSessionRequired.kind_str(),
            "No archived conversation matches the requested path, source, and conversation id",
            "Copy source_path, source_id, and conversation_id from the same search hit.",
        )
    })?;
    if conversations.len() != 1 {
        return Err(error(
            CliErrorKind::AmbiguousSource.kind_str(),
            "Multiple archived conversations match this path",
            "Pass both --source and --conversation-id from the search hit.",
        ));
    }
    let conversation_id = *conversation_id;
    let mut selection = window::Selection::new(
        request.message_index,
        request.context,
        stream::MAX_WINDOW_RECORDS,
    );
    let mut invalid_index = None;
    let mut scan_expired = false;
    // The (conversation_id, idx) index can stream this ordered metadata pass.
    // Context is measured in actual messages, NOT arithmetic on sparse idxs.
    // Keep only O(context) anchors, but validate/count the whole conversation.
    let scanned = storage.raw().query_with_params_for_each(
        "SELECT id, idx FROM messages WHERE conversation_id = ?1 ORDER BY idx",
        &[crate::franken_sync::SqliteValue::Integer(conversation_id)],
        |row| {
            if budget.is_exhausted() {
                scan_expired = true;
                return Err(crate::franken_sync::FrankenError::Internal(
                    DEADLINE_EXCEEDED.into(),
                ));
            }
            let id = row.get_typed::<i64>(0)?;
            let idx = row.get_typed::<i64>(1)?;
            selection.observe(id, idx).map_err(|reason| {
                invalid_index = Some(reason);
                crate::franken_sync::FrankenError::Internal(reason.to_string())
            })
        },
    );
    if scan_expired {
        check_deadline(budget)?;
    }
    if let Some(reason) = invalid_index {
        return Err(match reason {
            window::SelectionError::ResourceLimit => {
                resource_limit("canonical window exceeds 4096 records")
            }
            window::SelectionError::InvalidIndex(reason) => error(
                CliErrorKind::InvalidLine.kind_str(),
                reason,
                "Inspect the canonical archive; no target has been selected.",
            ),
        });
    }
    scanned.map_err(lookup_error)?;
    check_deadline(budget)?;
    if !selection.found {
        return Err(error(
            CliErrorKind::LineNotFound.kind_str(),
            format!(
                "No archived message at message index {}",
                request.message_index
            ),
            "Re-run search for a current anchor. Neighbouring messages are never substituted.",
        ));
    }
    let first = selection
        .anchors
        .front()
        .ok_or_else(|| lookup_error("empty message window"))?;
    let last = selection
        .anchors
        .back()
        .ok_or_else(|| lookup_error("empty message window"))?;
    // Stream only the selected range, with a SQL byte guard before transferring
    // each body across the engine boundary. Byte lengths use BLOB casts: TEXT
    // length counts characters and can stop at an embedded NUL. A refusal is
    // never shortened into successful evidence. The engine's own page/value
    // allocations and time inside one engine call are not bounded by this guard.
    let mut lines = Vec::new();
    let mut retained = stream::WindowSize::default();
    let mut hydration_error = None;
    let hydrated = storage.raw().query_with_params_for_each(
        "SELECT id, idx, typeof(role), typeof(content),
         COALESCE(length(CAST(role AS BLOB)), 0),
         COALESCE(length(CAST(content AS BLOB)), 0),
         CASE WHEN COALESCE(length(CAST(role AS BLOB)), 0) + COALESCE(length(CAST(content AS BLOB)), 0) <= ?4
                   AND typeof(role) IN ('text', 'null') THEN role ELSE NULL END,
         CASE WHEN COALESCE(length(CAST(role AS BLOB)), 0) + COALESCE(length(CAST(content AS BLOB)), 0) <= ?4
                   AND typeof(content) IN ('text', 'null') THEN content ELSE NULL END
         FROM messages WHERE conversation_id = ?1 AND idx >= ?2 AND idx <= ?3 ORDER BY idx",
        &[
            crate::franken_sync::SqliteValue::Integer(conversation_id),
            crate::franken_sync::SqliteValue::Integer(first.idx),
            crate::franken_sync::SqliteValue::Integer(last.idx),
            crate::franken_sync::SqliteValue::Integer(stream::MAX_RECORD_BYTES as i64),
        ],
        |row| {
            let result = (|| -> CliResult<Value> {
                check_deadline(budget)?;
                let anchor = selection.anchors.get(lines.len())
                    .ok_or_else(|| lookup_error("unexpected message in snapshot hydration"))?;
                let id = row.get_typed::<i64>(0).map_err(lookup_error)?;
                let idx = row.get_typed::<i64>(1).map_err(lookup_error)?;
                if id != anchor.id || idx != anchor.idx {
                    return Err(lookup_error("message identity changed during snapshot hydration"));
                }
                let role_type = row.get_typed::<String>(2).map_err(lookup_error)?;
                let content_type = row.get_typed::<String>(3).map_err(lookup_error)?;
                if !matches!(role_type.as_str(), "text" | "null")
                    || !matches!(content_type.as_str(), "text" | "null") {
                    return Err(lookup_error("canonical role/content must be text or null"));
                }
                let role_bytes = usize::try_from(row.get_typed::<i64>(4).map_err(lookup_error)?)
                    .map_err(lookup_error)?;
                let content_bytes = usize::try_from(row.get_typed::<i64>(5).map_err(lookup_error)?)
                    .map_err(lookup_error)?;
                if role_bytes.saturating_add(content_bytes) > stream::MAX_RECORD_BYTES {
                    return Err(resource_limit("canonical role and content exceed 8 MiB"));
                }
                let role = row.get_typed::<Option<String>>(6).map_err(lookup_error)?;
                let content = row.get_typed::<Option<String>>(7).map_err(lookup_error)?;
                if (role_type == "text" && role.is_none())
                    || (content_type == "text" && content.is_none())
                    || role.as_ref().map_or(0, String::len) != role_bytes
                    || content.as_ref().map_or(0, String::len) != content_bytes {
                    return Err(lookup_error("bounded hydration did not return the complete canonical text"));
                }
                let role = role.unwrap_or_else(|| "unknown".to_string());
                let role = match role.to_ascii_lowercase().as_str() {
                    "agent" | "assistant" => "assistant".to_string(),
                    "user" => "user".to_string(),
                    "tool" => "tool".to_string(),
                    "system" => "system".to_string(),
                    _ => role,
                };
                let content = content.unwrap_or_default();
                retained.admit(role.len().saturating_add(content.len())).map_err(resource_limit)?;
                Ok(json!({
                    "line": anchor.number,
                    "message_index": anchor.number,
                    "coordinate_space": "message_index",
                    "content_source": "archive",
                    "message_id": id,
                    "conversation_id": conversation_id,
                    "source_id": source_id,
                    "role": role,
                    "content": content,
                    "is_target": anchor.number == request.message_index,
                    "highlighted": anchor.number == request.message_index,
                }))
            })();
            match result {
                Ok(line) => {
                    lines.push(line);
                    Ok(())
                }
                Err(error) => {
                    hydration_error = Some(error);
                    Err(crate::franken_sync::FrankenError::Internal("canonical hydration stopped".into()))
                }
            }
        },
    );
    if let Some(error) = hydration_error {
        return Err(error);
    }
    hydrated.map_err(lookup_error)?;
    check_deadline(budget)?;
    if lines.len() != selection.anchors.len() {
        return Err(lookup_error(
            "message window changed during snapshot hydration",
        ));
    }
    if expand {
        return Ok(Value::Array(lines));
    }
    let source_exists = request.path.exists();
    Ok(json!({
        "path": path,
        "source_id": source_id,
        "conversation_id": conversation_id,
        "coordinate_space": "message_index",
        "content_source": "archive",
        "target_line": request.message_index,
        "target_message_index": request.message_index,
        "context": request.context,
        "lines": lines,
        "total_lines": selection.total,
        "total_messages": selection.total,
        "source_exists": source_exists,
        "archive_only": !source_exists,
    }))
}

fn run(
    request: Request,
    expand: bool,
    output_format: Option<RobotFormat>,
    timeout_ms: Option<u64>,
) -> CliResult<()> {
    if let Some(source) = request.source.as_deref() {
        crate::validate_followup_source_id(source, "canonical message lookup")?;
    }
    let format = output_format
        .or_else(crate::robot_format_from_env)
        .map(|format| {
            if matches!(format, RobotFormat::Sessions) {
                RobotFormat::Compact
            } else {
                format
            }
        });
    let budget_ms = timeout_ms.unwrap_or_else(|| {
        dotenvy::var("CASS_VIEW_BUDGET_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(10_000)
    });
    let budget = RobotBudget::new(budget_ms);
    // Lookup AND output projection run inside the existing read-only deadline.
    // A timeout is an error, not a successful payload with an invented target.
    let encoded = crate::run_read_only_search_worker(budget.remaining_ms(), move || {
        let payload = resolve(&request, expand, &budget)?;
        check_deadline(&budget)?;
        if let Some(format) = format {
            let encoded = crate::encode_structured_value(payload, format)?;
            check_deadline(&budget)?;
            return Ok(encoded);
        }
        let lines = if expand { &payload } else { &payload["lines"] };
        let mut output = format!("Archived messages in {}\n", request.path.display());
        for message in lines.as_array().expect("message projection is an array") {
            check_deadline(&budget)?;
            output.push_str(&format!(
                "{} M{} {}\n{}\n\n",
                if message["is_target"] == true {
                    ">>>"
                } else {
                    "   "
                },
                message["message_index"],
                message["role"].as_str().unwrap_or("unknown"),
                message["content"].as_str().unwrap_or_default(),
            ));
        }
        check_deadline(&budget)?;
        Ok(output)
    });
    let encoded = match encoded {
        Err(error) if error.kind == DEADLINE_EXCEEDED => None,
        result => result?,
    }
    .ok_or_else(|| CliError {
        code: 9,
        kind: "message-lookup-timeout",
        message: format!("Canonical message lookup exceeded its {budget_ms}ms budget"),
        hint: Some("Retry the same --message-index, --source, and --conversation-id with a larger view --timeout or CASS_VIEW_BUDGET_MS.".to_string()),
        retryable: true,
    })?;
    println!("{encoded}");
    Ok(())
}

// Physical coordinates must never pass through the archive serializer: it
// emits one line per normalized message, not one line per source-file record.
// This reader has no database handle or path and opens the source exactly once.
fn resolve_physical(
    path: &Path,
    line: usize,
    context: usize,
    expand: bool,
    budget: &RobotBudget,
) -> CliResult<Value> {
    use std::collections::VecDeque;
    use std::io::BufReader;

    check_deadline(budget)?;

    if line == 0 {
        return Err(error(
            CliErrorKind::InvalidLine.kind_str(),
            "Line numbers start at 1, not 0",
            "Use --line 1 for a physical file line, or --message-index for a search hit.",
        ));
    }
    let open_error = |err: std::io::Error| {
        CliError {
        code: if err.kind() == std::io::ErrorKind::NotFound {
            3
        } else {
            9
        },
        kind: if err.kind() == std::io::ErrorKind::NotFound {
            CliErrorKind::FileNotFound.kind_str()
        } else {
            CliErrorKind::FileOpen.kind_str()
        },
        message: format!(
            "Cannot open physical source file {}: {err}. Use --message-index to read archived messages.",
            path.display()
        ),
        hint: Some("Use the search hit's source and conversation identity. --line never substitutes archived messages.".into()),
        retryable: false,
    }
    };
    let file_error = |err: std::io::Error| {
        CliError {
        code: 9,
        kind: CliErrorKind::FileRead.kind_str(),
        message: format!("Cannot read physical source file {}: {err}", path.display()),
        hint: Some("No archived content was substituted. Use --message-index to inspect an indexed message.".into()),
        retryable: false,
    }
    };
    // Refuse special files before opening. On Unix, nonblocking open also
    // closes the metadata/open race with a path replaced by a FIFO. A regular
    // file (including a symlink to one) keeps its ordinary read semantics.
    if !std::fs::metadata(path).map_err(open_error)?.is_file() {
        return Err(file_error(std::io::Error::other(
            "source is not a regular file",
        )));
    }
    check_deadline(budget)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(open_error)?;
    if !file.metadata().map_err(file_error)?.is_file() {
        return Err(file_error(std::io::Error::other(
            "source is not a regular file",
        )));
    }
    if expand
        && !path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        return Err(CliError {
            code: 9,
            kind: CliErrorKind::IndexedSessionRequired.kind_str(),
            message: "Physical expand requires a JSONL source; use --message-index for an indexed conversation".into(),
            hint: Some("For literal text lines in other formats, use view --line. An indexed conversation does not preserve physical line coordinates.".into()),
            retryable: false,
        });
    }

    let mut preceding = VecDeque::new();
    let mut selected = Vec::new();
    let mut total_lines = 0_usize;
    let mut found = false;
    let mut following = 0_usize;
    let mut retained = stream::WindowSize::default();
    let mut reader = BufReader::new(file);
    loop {
        let next_line = total_lines
            .checked_add(1)
            .ok_or_else(|| file_error(std::io::Error::other("physical line count overflow")))?;
        // Literal view context is known from physical coordinates before any
        // allocation. Expand must parse prior records when counting context,
        // but -C 0 can skip them without decoding or retaining their bodies.
        let retain = if expand {
            next_line >= line || context != 0
        } else {
            next_line >= line.saturating_sub(context) && next_line <= line.saturating_add(context)
        };
        let raw = stream::read_line(&mut reader, retain, stream::MAX_RECORD_BYTES, || {
            if budget.is_exhausted() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    DEADLINE_EXCEEDED,
                ))
            } else {
                Ok(())
            }
        })
        .map_err(|error| match error {
            stream::ReadError::RecordTooLarge => resource_limit("physical record exceeds 8 MiB"),
            stream::ReadError::Io(error)
                if error.kind() == std::io::ErrorKind::TimedOut && budget.is_exhausted() =>
            {
                deadline_error()
            }
            stream::ReadError::Io(error) => file_error(error),
        })?;
        let Some(raw) = raw else {
            break;
        };
        total_lines = total_lines
            .checked_add(1)
            .ok_or_else(|| file_error(std::io::Error::other("physical line count overflow")))?;
        let Some(raw) = raw else {
            continue;
        };
        check_deadline(budget)?;
        let mut entry = if expand {
            let Ok(record) = serde_json::from_str::<Value>(&raw) else {
                if total_lines == line {
                    break;
                }
                continue;
            };
            json!({
                "role": crate::extract_role(&record),
                "content": crate::extract_text_content(&record),
            })
        } else {
            json!({"content": raw})
        };
        entry["line"] = json!(total_lines);
        entry["file_line"] = json!(total_lines);
        entry["coordinate_space"] = json!("file_line");
        entry["content_source"] = json!("file");
        entry["is_target"] = json!(total_lines == line);
        entry["highlighted"] = json!(total_lines == line);
        let entry_bytes = physical_entry_bytes(&entry);
        if total_lines < line {
            if context > 0 {
                if preceding.len() == context
                    && let Some(old) = preceding.pop_front()
                {
                    retained.release(physical_entry_bytes(&old));
                }
                retained.admit(entry_bytes).map_err(resource_limit)?;
                preceding.push_back(entry);
            }
        } else if total_lines == line {
            retained.admit(entry_bytes).map_err(resource_limit)?;
            found = true;
            selected.extend(preceding.drain(..));
            selected.push(entry);
        } else if found && following < context {
            retained.admit(entry_bytes).map_err(resource_limit)?;
            selected.push(entry);
            following += 1;
        }
        // Expand exposes only a context array, not the total file length. Once
        // that window is complete no tail scan or tail payload decoding is needed.
        if expand && found && following == context {
            break;
        }
    }
    check_deadline(budget)?;
    if !expand && total_lines == 0 {
        return Err(CliError {
            code: 9,
            kind: CliErrorKind::EmptyFile.kind_str(),
            message: format!("File is empty: {}", path.display()),
            hint: None,
            retryable: false,
        });
    }
    if !found {
        return Err(error(
            if expand {
                CliErrorKind::LineNotFound.kind_str()
            } else {
                CliErrorKind::LineOutOfRange.kind_str()
            },
            format!(
                "No physical {} at line {line} in {}",
                if expand { "JSONL record" } else { "file line" },
                path.display()
            ),
            "No neighbouring or archived message was selected. Use --message-index for a search hit.",
        ));
    }
    if expand {
        return Ok(Value::Array(selected));
    }
    Ok(json!({
        "path": path.to_string_lossy(),
        "coordinate_space": "file_line",
        "content_source": "file",
        "target_line": line,
        "context": context,
        "lines": selected,
        "total_lines": total_lines,
        "source_exists": true,
        "archive_only": false,
    }))
}

fn physical_entry_bytes(entry: &Value) -> usize {
    entry["content"]
        .as_str()
        .map_or(0, str::len)
        .saturating_add(entry["role"].as_str().map_or(0, str::len))
}

#[allow(clippy::too_many_arguments)]
fn run_physical(
    path: &Path,
    db_override: Option<PathBuf>,
    source_id: Option<&str>,
    conversation_id: Option<i64>,
    window: ViewWindow,
    expand: bool,
    output_format: Option<RobotFormat>,
    timeout_ms: Option<u64>,
) -> CliResult<()> {
    if let Some(source) = source_id {
        crate::validate_followup_source_id(source, "physical file lookup")?;
    }
    let source = crate::canonical_followup_source_id(source_id);
    if conversation_id.is_some()
        || (source.is_some() && !crate::followup_source_is_local(source.as_deref()))
    {
        return Err(error(
            CliErrorKind::InvalidLine.kind_str(),
            "--line addresses a local physical file, not a remote source or an archive conversation",
            "Use --message-index with --source and --conversation-id for an archived search hit. Omit --line to browse an archived conversation without a target.",
        ));
    }
    let line = window.line.ok_or_else(|| {
        error(
            CliErrorKind::InvalidLine.kind_str(),
            "A physical file line is required",
            "Use --line or --message-index explicitly.",
        )
    })?;
    let format = output_format
        .or_else(crate::robot_format_from_env)
        .map(|format| {
            if matches!(format, RobotFormat::Sessions) {
                RobotFormat::Compact
            } else {
                format
            }
        });
    let budget_ms = timeout_ms.unwrap_or_else(|| {
        dotenvy::var("CASS_VIEW_BUDGET_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .unwrap_or(10_000)
    });
    let budget = crate::robot_budget_envelope::RobotBudget::new(budget_ms);
    let request_path = path.to_path_buf();
    let context = window.context;
    let read_finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_read_finished = std::sync::Arc::clone(&read_finished);
    let encoded = crate::run_read_only_search_worker(budget.remaining_ms(), move || {
        crate::maybe_test_view_delay();
        check_deadline(&budget)?;
        let mut payload = resolve_physical(&request_path, line, context, expand, &budget)?;
        worker_read_finished.store(true, std::sync::atomic::Ordering::Release);
        crate::maybe_test_search_worker_delay("CASS_TEST_VIEW_PROJECTION_SLOW_MS");
        check_deadline(&budget)?;
        if !expand {
            payload["budget"] = serde_json::to_value(
                crate::robot_budget_envelope::BudgetBlock::from_budget(&budget, Vec::new(), None),
            )
            .map_err(|err| {
                error(
                    CliErrorKind::SerializeMessage.kind_str(),
                    err.to_string(),
                    "Retry this physical-file lookup.",
                )
            })?;
        }
        if let Some(format) = format {
            let encoded = crate::encode_structured_value(payload, format)?;
            check_deadline(&budget)?;
            return Ok(encoded);
        }
        let lines = if expand { &payload } else { &payload["lines"] };
        let mut output = format!("Physical lines in {}\n", request_path.display());
        for entry in lines.as_array().expect("physical projection is an array") {
            check_deadline(&budget)?;
            let content = entry["content"].as_str().unwrap_or_default();
            let display = if expand {
                content.chars().take(300).collect::<String>()
            } else {
                content.to_string()
            };
            output.push_str(&format!(
                "{} L{} {}\n{}\n\n",
                if entry["is_target"] == true {
                    ">>>"
                } else {
                    "   "
                },
                entry["line"],
                entry["role"].as_str().unwrap_or_default(),
                display,
            ));
        }
        check_deadline(&budget)?;
        Ok(output)
    });
    let encoded = match encoded {
        Err(error) if error.kind == DEADLINE_EXCEEDED => None,
        result => result?,
    };
    if let Some(encoded) = encoded {
        println!("{encoded}");
        return Ok(());
    }
    if !expand && let Some(format) = format {
        let mut skipped = vec!["view_content".into(), "source_provenance".into()];
        if read_finished.load(std::sync::atomic::Ordering::Acquire) {
            skipped.push("output_projection".into());
        }
        return crate::output_bounded_view_partial(
            path,
            &db_override.unwrap_or_else(crate::default_db_path),
            source_id,
            None,
            Some(line),
            context,
            budget_ms,
            &budget,
            format,
            skipped,
        );
    }
    Err(CliError {
        code: 9,
        kind: "file-lookup-timeout",
        message: format!("Physical file lookup exceeded its {budget_ms}ms budget"),
        hint: Some(
            "Retry with a larger view --timeout or CASS_VIEW_BUDGET_MS. No target was emitted."
                .into(),
        ),
        retryable: true,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_view(
    path: &Path,
    db_override: Option<PathBuf>,
    source_id: Option<&str>,
    conversation_id: Option<i64>,
    window: ViewWindow,
    output_format: Option<RobotFormat>,
    timeout_ms: Option<u64>,
    message_index: Option<usize>,
) -> CliResult<()> {
    let Some(message_index) = message_index else {
        if window.line.is_some() {
            return run_physical(
                path,
                db_override,
                source_id,
                conversation_id,
                window,
                false,
                output_format,
                timeout_ms,
            );
        }
        // Unanchored browsing can render the archive; it asserts no target.
        return crate::run_view(
            path,
            db_override,
            source_id,
            conversation_id,
            window,
            output_format,
            timeout_ms,
        );
    };
    let request = Request {
        path: path.to_path_buf(),
        db: db_override.unwrap_or_else(crate::default_db_path),
        source: crate::canonical_followup_source_id(source_id),
        conversation_id,
        message_index,
        context: window.context,
    };
    // Validate before normalization so an explicitly empty source cannot become
    // an unconstrained lookup.
    if let Some(source) = source_id {
        crate::validate_followup_source_id(source, "cass view")?;
    }
    run(request, false, output_format, timeout_ms)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_expand(
    path: &Path,
    db_override: Option<PathBuf>,
    source_id: Option<&str>,
    line: Option<usize>,
    context: usize,
    output_format: Option<RobotFormat>,
    message_index: Option<usize>,
    conversation_id: Option<i64>,
) -> CliResult<()> {
    let Some(message_index) = message_index else {
        let line = line.ok_or_else(|| {
            error(
                CliErrorKind::InvalidLine.kind_str(),
                "Choose --line for physical lines or --message-index for a search hit",
                "Search's line_number belongs to --message-index, not --line.",
            )
        })?;
        if line == 0 {
            // The existing zero-line validator returns before any file/archive
            // lookup. Positive physical coordinates use the file-only reader.
            return crate::run_expand(path, db_override, source_id, line, context, output_format);
        }
        return run_physical(
            path,
            db_override,
            source_id,
            conversation_id,
            ViewWindow {
                line: Some(line),
                context,
            },
            true,
            output_format,
            None,
        );
    };
    if let Some(source) = source_id {
        crate::validate_followup_source_id(source, "cass expand")?;
    }
    run(
        Request {
            path: path.to_path_buf(),
            db: db_override.unwrap_or_else(crate::default_db_path),
            source: crate::canonical_followup_source_id(source_id),
            conversation_id,
            message_index,
            context,
        },
        true,
        output_format,
        None,
    )
}
