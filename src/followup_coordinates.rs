//! Exact canonical message addressing for search follow-ups (GH #493).
//!
//! Search's `line_number` is `messages.idx + 1`, not a physical file line.
//! Keep this lane independent of raw-file rendering, connector filtering, and
//! vector positions. One read transaction observes identity and messages
//! together; it never reparses or mutates the source file or archive.

use crate::franken_sync::compat::{ConnectionExt, RowExt};
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

mod window;

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

fn resolve(request: &Request, expand: bool) -> CliResult<Value> {
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
    // Pin identity selection, index validation and content hydration to one
    // read transaction. Never choose against one snapshot and render another.
    storage.raw().execute("BEGIN DEFERRED").map_err(lookup_error)?;
    let result = resolve_snapshot(request, expand, &storage);
    let released = storage.raw().execute("ROLLBACK").map_err(lookup_error);
    match result {
        Err(err) => Err(err),
        Ok(payload) => released.map(|_| payload),
    }
}

fn resolve_snapshot(request: &Request, expand: bool, storage: &FrankenStorage) -> CliResult<Value> {
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
    let mut selection = window::Selection::new(request.message_index, request.context);
    let mut invalid_index = None;
    // The (conversation_id, idx) index can stream this ordered metadata pass.
    // Context is measured in actual messages, NOT arithmetic on sparse idxs.
    // Keep only O(context) anchors, but validate/count the whole conversation.
    let scanned = storage.raw().query_with_params_for_each(
        "SELECT id, idx FROM messages WHERE conversation_id = ?1 ORDER BY idx",
        &[crate::franken_sync::SqliteValue::Integer(conversation_id)],
        |row| {
            let id = row.get_typed::<i64>(0)?;
            let idx = row.get_typed::<i64>(1)?;
            selection.observe(id, idx).map_err(|reason| {
                invalid_index = Some(reason);
                crate::franken_sync::FrankenError::Internal(reason.to_string())
            })
        },
    );
    if let Some(reason) = invalid_index {
        return Err(error(
            CliErrorKind::InvalidLine.kind_str(),
            reason,
            "Inspect the canonical archive; no target has been selected.",
        ));
    }
    scanned.map_err(lookup_error)?;
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
    // Hydrate only the exact selected range. Even -C 0 previously decoded and
    // retained every tool result in the transcript before discarding it.
    let rows = storage
        .raw()
        .query_map_collect(
            "SELECT id, idx, role, content FROM messages
             WHERE conversation_id = ?1 AND idx >= ?2 AND idx <= ?3 ORDER BY idx",
            crate::franken_sync::params![conversation_id, first.idx, last.idx],
            |row| {
                Ok((
                    row.get_typed::<i64>(0)?,
                    row.get_typed::<i64>(1)?,
                    row.get_typed::<Option<String>>(2)?,
                    row.get_typed::<Option<String>>(3)?,
                ))
            },
        )
        .map_err(lookup_error)?;
    if rows.len() != selection.anchors.len() {
        return Err(lookup_error(
            "message window changed during snapshot hydration",
        ));
    }
    let mut lines = Vec::new();
    for ((id, idx, role, content), anchor) in rows.into_iter().zip(&selection.anchors) {
        if id != anchor.id || idx != anchor.idx {
            return Err(lookup_error(
                "message identity changed during snapshot hydration",
            ));
        }
        let role = role.unwrap_or_else(|| "unknown".to_string());
        let role = match role.to_ascii_lowercase().as_str() {
            "agent" | "assistant" => "assistant".to_string(),
            "user" => "user".to_string(),
            "tool" => "tool".to_string(),
            "system" => "system".to_string(),
            _ => role,
        };
        lines.push(json!({
            "line": anchor.number,
            "message_index": anchor.number,
            "coordinate_space": "message_index",
            "content_source": "archive",
            "message_id": id,
            "conversation_id": conversation_id,
            "source_id": source_id,
            "role": role,
            "content": content.unwrap_or_default(),
            "is_target": anchor.number == request.message_index,
            "highlighted": anchor.number == request.message_index,
        }));
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
    let budget = timeout_ms.unwrap_or_else(|| {
        dotenvy::var("CASS_VIEW_BUDGET_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(10_000)
    });
    // Lookup AND output projection run inside the existing read-only deadline.
    // A timeout is an error, not a successful payload with an invented target.
    let encoded = crate::run_read_only_search_worker(budget, move || {
        let payload = resolve(&request, expand)?;
        if let Some(format) = format {
            return crate::encode_structured_value(payload, format);
        }
        let lines = if expand { &payload } else { &payload["lines"] };
        let mut output = format!("Archived messages in {}\n", request.path.display());
        for message in lines.as_array().expect("message projection is an array") {
            output.push_str(&format!(
                "{} M{} {}\n{}\n\n",
                if message["is_target"] == true { ">>>" } else { "   " },
                message["message_index"],
                message["role"].as_str().unwrap_or("unknown"),
                message["content"].as_str().unwrap_or_default(),
            ));
        }
        Ok(output)
    })?
    .ok_or_else(|| CliError {
        code: 9,
        kind: "message-lookup-timeout",
        message: format!("Canonical message lookup exceeded its {budget}ms budget"),
        hint: Some("Retry the same --message-index, --source, and --conversation-id with a larger view --timeout or CASS_VIEW_BUDGET_MS.".to_string()),
        retryable: true,
    })?;
    println!("{encoded}");
    Ok(())
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
        return crate::run_expand(path, db_override, source_id, line, context, output_format);
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
