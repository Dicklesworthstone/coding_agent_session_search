//! Job request handlers shared by the daemon's live dispatch and regressions.
//!
//! Cancellation acknowledges queue admission, not completed database writes.
//! Only the worker performs durable cleanup, in order before newer jobs.
//! Status opens the archive read-only: asking about a missing database must
//! never create it or run schema migrations.

use std::path::Path;

use crate::daemon::protocol::{
    EmbeddingJobDetail, EmbeddingJobInfo, ErrorCode, ErrorResponse, Response,
};
use crate::daemon::worker::{EmbeddingJobConfig, EmbeddingWorkerHandle};
use crate::franken_sync::compat::{ConnectionExt, OpenFlags, ParamValue, RowExt, open_with_flags};

const MAX_STATUS_ROWS: usize = 256;
const MAX_MODEL_CHARS: usize = 256;
const MAX_STATE_CHARS: usize = 64;
const MAX_ERROR_CHARS: usize = 1024;
const MAX_PATH_BYTES: usize = 16 * 1024;

fn error(code: ErrorCode, message: impl Into<String>, retryable: bool) -> Response {
    Response::Error(ErrorResponse {
        code,
        message: message.into(),
        retryable,
        retry_after_ms: retryable.then_some(1000),
    })
}

fn worker_error(message: String) -> Response {
    if message.contains("queue is full") {
        error(ErrorCode::Overloaded, message, true)
    } else if message.contains("requires")
        || message.contains("byte limit")
        || message.contains("NUL byte")
    {
        error(ErrorCode::InvalidInput, message, false)
    } else {
        error(ErrorCode::Internal, message, true)
    }
}

pub(super) fn submit(
    config: EmbeddingJobConfig,
    request_id: &str,
    worker: Option<&EmbeddingWorkerHandle>,
) -> Response {
    let Some(worker) = worker else {
        return error(
            ErrorCode::Internal,
            "embedding worker not initialized",
            true,
        );
    };
    match worker.submit(config) {
        Ok(()) => Response::JobSubmitted {
            job_id: request_id.to_string(),
            message: "embedding job admitted (identical pending requests may coalesce)".to_string(),
        },
        Err(message) => worker_error(message),
    }
}

pub(super) fn cancel(
    db_path: String,
    model_id: Option<String>,
    worker: Option<&EmbeddingWorkerHandle>,
) -> Response {
    let Some(worker) = worker else {
        return error(
            ErrorCode::Internal,
            "embedding worker not initialized; cancellation was not admitted",
            true,
        );
    };
    match worker.cancel_with_count(db_path, model_id) {
        Ok(count) => Response::JobCancelled {
            cancelled: count,
            message: format!(
                "cancellation requested for {count} queued/running pass(es); durable job cleanup is pending"
            ),
        },
        Err(message) => worker_error(message),
    }
}

pub(super) fn status(db_path: &str) -> Response {
    if db_path.trim().is_empty() || db_path.len() > MAX_PATH_BYTES || db_path.contains('\0') {
        return error(
            ErrorCode::InvalidInput,
            "job status requires a valid bounded database path",
            false,
        );
    }
    match std::fs::metadata(Path::new(db_path)) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Response::JobStatus(EmbeddingJobInfo { jobs: Vec::new() });
        }
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return error(
                ErrorCode::InvalidInput,
                "job status target is not a regular file",
                false,
            );
        }
        Err(source) => {
            tracing::warn!(error = %source, "Cannot inspect embedding job status target");
            return error(
                ErrorCode::Internal,
                "could not inspect job status database",
                false,
            );
        }
    }
    match read_status(db_path) {
        Ok(jobs) => Response::JobStatus(EmbeddingJobInfo { jobs }),
        Err(source) => {
            tracing::warn!(error = %source, "Cannot read bounded embedding job status");
            error(
                ErrorCode::Internal,
                format!("could not read job status: {source}"),
                false,
            )
        }
    }
}

fn read_status(db_path: &str) -> anyhow::Result<Vec<EmbeddingJobDetail>> {
    let connection = open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // Bound both row count and variable-width fields before returning them to
    // Rust. Oversized identity/state fields are rejected, never relabelled.
    // Error diagnostics may be truncated, with an explicit display marker.
    // This bounds the response projection, not the engine's scan/IO costs.
    let mut jobs = connection.query_map_collect(
        "SELECT id, substr(model_id, 1, ?2), substr(status, 1, ?3), \
         total_docs, completed_docs, substr(error_message, 1, ?4) \
         FROM embedding_jobs WHERE db_path = ?1 ORDER BY id DESC LIMIT ?5",
        &[
            ParamValue::from(db_path),
            ParamValue::from((MAX_MODEL_CHARS + 1) as i64),
            ParamValue::from((MAX_STATE_CHARS + 1) as i64),
            ParamValue::from((MAX_ERROR_CHARS + 1) as i64),
            ParamValue::from((MAX_STATUS_ROWS + 1) as i64),
        ],
        |row| {
            Ok(EmbeddingJobDetail {
                job_id: row.get_typed(0)?,
                model_id: row.get_typed(1)?,
                status: row.get_typed(2)?,
                total_docs: row.get_typed(3)?,
                completed_docs: row.get_typed(4)?,
                error_message: row.get_typed(5)?,
            })
        },
    )?;
    if jobs.len() > MAX_STATUS_ROWS {
        anyhow::bail!("embedding job status exceeds the {MAX_STATUS_ROWS}-row response limit");
    }
    for job in &mut jobs {
        if job.model_id.chars().count() > MAX_MODEL_CHARS
            || job.status.chars().count() > MAX_STATE_CHARS
        {
            anyhow::bail!("embedding job identity or state exceeds its response limit");
        }
        if let Some(message) = &mut job.error_message
            && message.chars().count() > MAX_ERROR_CHARS
        {
            *message = message.chars().take(MAX_ERROR_CHARS).collect::<String>() + " [truncated]";
        }
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::worker::EmbeddingWorker;
    use crate::franken_sync::Connection;

    fn config(db_path: String, index_path: String) -> EmbeddingJobConfig {
        EmbeddingJobConfig {
            db_path,
            index_path,
            two_tier: true,
            fast_model: Some("hash".into()),
            quality_model: Some("minilm".into()),
        }
    }

    fn schema(connection: &Connection) -> anyhow::Result<()> {
        connection.execute_batch(
            "CREATE TABLE embedding_jobs (
                id INTEGER PRIMARY KEY, db_path TEXT NOT NULL, model_id TEXT NOT NULL,
                status TEXT NOT NULL, total_docs INTEGER NOT NULL,
                completed_docs INTEGER NOT NULL, error_message TEXT
             );",
        )?;
        Ok(())
    }

    #[test]
    fn status_and_unavailable_cancellation_do_not_create_an_archive() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("absent.db").to_string_lossy().into_owned();
        assert!(matches!(status(&path), Response::JobStatus(info) if info.jobs.is_empty()));
        assert!(
            matches!(cancel(path.clone(), None, None), Response::Error(error) if error.retryable)
        );
        assert!(!Path::new(&path).exists());
        assert_eq!(std::fs::read_dir(temp.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn accepted_cancellation_reports_signalled_passes_not_completed_cleanup() -> anyhow::Result<()>
    {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("absent.db").to_string_lossy().into_owned();
        let (_worker, handle) = EmbeddingWorker::new();
        let job = config(
            path.clone(),
            temp.path().join("index").to_string_lossy().into_owned(),
        );
        assert!(matches!(
            submit(job, "request-one", Some(&handle)),
            Response::JobSubmitted { .. }
        ));
        match cancel(path.clone(), None, Some(&handle)) {
            Response::JobCancelled { cancelled, message } => {
                assert_eq!(cancelled, 2);
                assert!(message.contains("requested"));
                assert!(message.contains("cleanup is pending"));
            }
            other => panic!("expected an admission receipt, got {other:?}"),
        }
        assert!(matches!(
            cancel(path.clone(), None, Some(&handle)),
            Response::JobCancelled { cancelled: 0, .. }
        ));
        assert!(!Path::new(&path).exists());
        Ok(())
    }

    #[test]
    fn full_queue_and_invalid_requests_have_distinct_retry_contracts() {
        let (_worker, handle) = EmbeddingWorker::new();
        for index in 0..32 {
            let job = config(format!("/archive/{index}.db"), format!("/index/{index}"));
            assert!(matches!(
                submit(job, "request", Some(&handle)),
                Response::JobSubmitted { .. }
            ));
        }
        let overflow = config("/archive/overflow.db".into(), "/index/overflow".into());
        assert!(
            matches!(submit(overflow, "overflow", Some(&handle)), Response::Error(error)
            if error.code == ErrorCode::Overloaded && error.retryable && error.retry_after_ms.is_some())
        );
        let invalid = config(String::new(), "/index/invalid".into());
        assert!(
            matches!(submit(invalid, "invalid", Some(&handle)), Response::Error(error)
            if error.code == ErrorCode::InvalidInput && !error.retryable)
        );
    }

    #[test]
    fn refused_cancellation_is_an_error_not_a_successful_zero_count() {
        let (_worker, handle) = EmbeddingWorker::new();
        for index in 0..32 {
            assert!(matches!(
                cancel(format!("/archive/{index}.db"), None, Some(&handle)),
                Response::JobCancelled { .. }
            ));
        }
        assert!(
            matches!(cancel("/archive/overflow.db".into(), None, Some(&handle)), Response::Error(error)
            if error.code == ErrorCode::Overloaded && error.retryable)
        );
    }

    #[test]
    fn status_preserves_database_bytes_and_reports_real_ordered_rows() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("jobs.db");
        let path_text = path.to_string_lossy().into_owned();
        let connection = Connection::open(path_text.clone())?;
        schema(&connection)?;
        for (id, model, state, done) in [
            (1_i64, "hash", "completed", 10_i64),
            (2, "minilm", "running", 4),
        ] {
            connection.execute_compat(
                "INSERT INTO embedding_jobs VALUES (?1, ?2, ?3, ?4, 10, ?5, NULL)",
                &[
                    ParamValue::from(id),
                    ParamValue::from(path_text.as_str()),
                    ParamValue::from(model),
                    ParamValue::from(state),
                    ParamValue::from(done),
                ],
            )?;
        }
        connection.close()?;
        let before = std::fs::read(&path)?;
        match status(&path_text) {
            Response::JobStatus(info) => {
                assert_eq!(info.jobs.len(), 2);
                assert_eq!(info.jobs[0].job_id, 2);
                assert_eq!(info.jobs[0].completed_docs, 4);
                assert_eq!(info.jobs[1].status, "completed");
            }
            other => panic!("expected real job rows, got {other:?}"),
        }
        assert_eq!(std::fs::read(&path)?, before);
        Ok(())
    }

    #[test]
    fn status_bounds_diagnostics_but_never_truncates_job_identity() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("jobs.db").to_string_lossy().into_owned();
        let connection = Connection::open(path.clone())?;
        schema(&connection)?;
        let diagnostic = "界".repeat(MAX_ERROR_CHARS + 100);
        connection.execute_compat(
            "INSERT INTO embedding_jobs VALUES (1, ?1, 'minilm', 'failed', 10, 0, ?2)",
            &[
                ParamValue::from(path.as_str()),
                ParamValue::from(diagnostic.as_str()),
            ],
        )?;
        connection.close()?;
        let rows = read_status(&path)?;
        assert_eq!(
            rows[0].error_message.as_deref(),
            Some(("界".repeat(MAX_ERROR_CHARS) + " [truncated]").as_str())
        );
        let connection = Connection::open(path.clone())?;
        let oversized = "m".repeat(MAX_MODEL_CHARS + 1);
        connection.execute_compat(
            "UPDATE embedding_jobs SET model_id = ?1",
            &[ParamValue::from(oversized.as_str())],
        )?;
        connection.close()?;
        assert!(read_status(&path).is_err());
        Ok(())
    }

    #[test]
    fn status_refuses_excess_rows_instead_of_claiming_an_incomplete_list_is_complete()
    -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("jobs.db").to_string_lossy().into_owned();
        let connection = Connection::open(path.clone())?;
        schema(&connection)?;
        connection.execute("BEGIN IMMEDIATE")?;
        for id in 1..=MAX_STATUS_ROWS + 1 {
            connection.execute_compat(
                "INSERT INTO embedding_jobs VALUES (?1, ?2, 'hash', 'completed', 0, 0, NULL)",
                &[ParamValue::from(id as i64), ParamValue::from(path.as_str())],
            )?;
        }
        connection.execute("COMMIT")?;
        connection.close()?;
        assert!(read_status(&path).is_err());
        Ok(())
    }
}
