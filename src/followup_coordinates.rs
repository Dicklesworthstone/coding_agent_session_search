//! Exact canonical message addressing for search follow-ups (GH #493).
//!
//! Search's `line_number` is `messages.idx + 1`, not a physical file line.
//! Keep this lane independent of raw-file rendering, connector filtering, and
//! vector positions. One SELECT observes conversation identity and messages
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

struct Row {
    conversation_id: i64,
    source_id: String,
    message_id: Option<i64>,
    idx: Option<i64>,
    role: Option<String>,
    content: Option<String>,
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
    let storage = FrankenStorage::open_readonly(&request.db).map_err(|err| {
        error(
            CliErrorKind::IndexedSessionRequired.kind_str(),
            format!("Cannot read canonical archive: {err}"),
            "Check the --db used by search; no raw-file fallback was attempted.",
        )
    })?;
    let source_sql = crate::normalized_source_identity_sql_expr("c.source_id", "c.origin_host");
    // LIMIT 2 is deliberate: an ambiguous path must never select an arbitrary
    // conversation (including when several sessions share a provider DB).
    // The LEFT JOIN preserves empty conversations so they cannot hide ambiguity.
    let sql = format!(
        "SELECT c.id, {source_sql}, m.id, m.idx, m.role, m.content
         FROM (SELECT c.id, c.source_id, c.origin_host FROM conversations c
               WHERE c.source_path = ?1
                 AND (?2 IS NULL OR {source_sql} = ?2)
                 AND (?3 IS NULL OR c.id = ?3)
               ORDER BY c.id LIMIT 2) c
         LEFT JOIN messages m ON m.conversation_id = c.id
         ORDER BY c.id, m.idx"
    );
    let path = request.path.to_string_lossy().into_owned();
    let rows = storage
        .raw()
        .query_map_collect(
            &sql,
            crate::franken_sync::params![path, request.source.clone(), request.conversation_id],
            |row| {
                Ok(Row {
                    conversation_id: row.get_typed(0)?,
                    source_id: row.get_typed(1)?,
                    message_id: row.get_typed(2)?,
                    idx: row.get_typed(3)?,
                    role: row.get_typed(4)?,
                    content: row.get_typed(5)?,
                })
            },
        )
        .map_err(|err| {
            error(
                CliErrorKind::IndexedSessionRequired.kind_str(),
                format!("Canonical message lookup failed: {err}"),
                "Check the archive used by search; no raw-file fallback was attempted.",
            )
        })?;
    let first = rows.first().ok_or_else(|| {
        error(
            CliErrorKind::IndexedSessionRequired.kind_str(),
            "No archived conversation matches the requested path, source, and conversation id",
            "Copy source_path, source_id, and conversation_id from the same search hit.",
        )
    })?;
    if rows
        .iter()
        .any(|row| row.conversation_id != first.conversation_id)
    {
        return Err(error(
            CliErrorKind::AmbiguousSource.kind_str(),
            "Multiple archived conversations match this path",
            "Pass both --source and --conversation-id from the search hit.",
        ));
    }
    let conversation_id = first.conversation_id;
    let source_id = first.source_id.clone();
    let mut messages = Vec::new();
    let mut previous = None;
    for row in rows {
        let Some(message_id) = row.message_id else {
            continue;
        };
        let number = row
            .idx
            .and_then(|idx| usize::try_from(idx).ok())
            .and_then(|idx| idx.checked_add(1))
            .ok_or_else(|| {
                error(
                    CliErrorKind::InvalidLine.kind_str(),
                    "Archive contains an invalid message index",
                    "Inspect the canonical archive; no target has been selected.",
                )
            })?;
        if previous.is_some_and(|previous| number <= previous) {
            return Err(error(
                CliErrorKind::InvalidLine.kind_str(),
                "Archive contains duplicate or unordered message indices",
                "Inspect the canonical archive; no target has been selected.",
            ));
        }
        previous = Some(number);
        let role = row.role.unwrap_or_else(|| "unknown".to_string());
        let role = match role.to_ascii_lowercase().as_str() {
            "agent" | "assistant" => "assistant".to_string(),
            "user" => "user".to_string(),
            "tool" => "tool".to_string(),
            "system" => "system".to_string(),
            _ => role,
        };
        messages.push(json!({
            "line": number,
            "message_index": number,
            "coordinate_space": "message_index",
            "message_id": message_id,
            "conversation_id": conversation_id,
            "source_id": source_id,
            "role": role,
            "content": row.content.unwrap_or_default(),
            "is_target": number == request.message_index,
            "highlighted": number == request.message_index,
        }));
    }
    let target = messages
        .iter()
        .position(|message| message["is_target"] == true)
        .ok_or_else(|| {
            error(
                CliErrorKind::LineNotFound.kind_str(),
                format!(
                    "No archived message at message index {}",
                    request.message_index
                ),
                "Re-run search for a current anchor. Neighbouring messages are never substituted.",
            )
        })?;
    let start = target.saturating_sub(request.context);
    let end = target
        .saturating_add(request.context)
        .saturating_add(1)
        .min(messages.len());
    let total = messages.len();
    let lines: Vec<_> = messages.drain(start..end).collect();
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
        "total_lines": total,
        "total_messages": total,
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
