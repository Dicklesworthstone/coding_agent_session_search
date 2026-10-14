//! Native row-cursor tests, including a later row that cannot be decoded.
use super::*;
use crate::franken_sync::compat::{ConnectionExt, ParamValue};
use std::io;

fn database(path: &std::path::Path, indexed: bool) -> Result<Connection> {
    let connection = Connection::open(path.to_string_lossy().into_owned())?;
    let unique = if indexed {
        ", UNIQUE(conversation_id, idx)"
    } else {
        ""
    };
    connection.execute(&format!(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER,
         idx INTEGER, role TEXT, author TEXT, created_at INTEGER, content TEXT{unique})"
    ))?;
    Ok(connection)
}

#[derive(Debug, thiserror::Error)]
#[error("consumer stopped before the next message")]
struct Stop;

#[test]
fn native_cursor_stops_before_decoding_later_rows_of_the_same_conversation() -> Result<()> {
    for indexed in [true, false] {
        let temp = tempfile::tempdir()?;
        let connection = database(&temp.path().join("stream.db"), indexed)?;
        connection.execute("INSERT INTO messages VALUES (1, 7, 0, 'user', NULL, 1, 'first')")?;
        // The permissive fixture accepts a BLOB role, but the canonical String
        // decoder must reject it. A collecting loader would fail before it could
        // call the consumer for the first row.
        connection.execute("INSERT INTO messages VALUES (2, 7, 1, X'00', NULL, 2, 'later')")?;
        let mut delivered = 0;
        let error = messages(&connection, 7, &mut |_| {
            delivered += 1;
            Err(Stop.into())
        })
        .expect_err("the stop is not successful partial replay");
        assert!(
            error.is::<Stop>(),
            "later decoding replaced consumer error: {error:#}"
        );
        assert_eq!(delivered, 1);
        let error = messages(&connection, 7, &mut |_| Ok(()))
            .expect_err("without the stop, the malformed later row must be observed");
        assert!(error.downcast_ref::<FrankenError>().is_some());
        // Both aborts finish cursor cleanup before we reuse or close the bridge.
        let count: i64 = connection.query_row_map(
            "SELECT COUNT(*) FROM messages",
            &[] as &[ParamValue],
            |row| row.get_typed(0),
        )?;
        assert_eq!(count, 2);
        connection.close_without_checkpoint()?;
    }
    Ok(())
}

#[test]
fn source_roles_envelopes_order_and_full_unicode_text_are_preserved() -> Result<()> {
    for indexed in [true, false] {
        let temp = tempfile::tempdir()?;
        let connection = database(&temp.path().join("projection.db"), indexed)?;
        let roles = ["user", "assistant", "agent", "tool", "system", "custom"];
        let long = "Unicode cafe\u{301} 東京 ownership\n".repeat(8192);
        for ordinal in (0..roles.len()).rev() {
            connection.execute_compat(
                "INSERT INTO messages VALUES (?1, 7, ?2, ?3, ?4, ?5, ?6)",
                &[
                    ParamValue::from(ordinal as i64 + 1),
                    ParamValue::from(ordinal as i64 - 3),
                    ParamValue::from(roles[ordinal]),
                    ParamValue::from("writer"),
                    ParamValue::from(100 + ordinal as i64),
                    ParamValue::from(if ordinal == 0 { long.as_str() } else { "" }),
                ],
            )?;
        }
        connection.execute("UPDATE messages SET author = NULL, created_at = NULL WHERE id = 2")?;
        connection
            .execute("INSERT INTO messages VALUES (99, 8, 0, 'user', NULL, NULL, 'foreign')")?;
        connection.execute("PRAGMA query_only = ON")?;
        let mut rows = Vec::new();
        messages(&connection, 7, &mut |message| {
            rows.push(message);
            Ok(())
        })?;
        assert_eq!(rows.len(), roles.len());
        assert_eq!(
            rows[0].content, long,
            "no lexical prefix/content cap may enter the proof"
        );
        assert_eq!(rows[0].author.as_deref(), Some("writer"));
        assert_eq!(rows[1].author, None);
        assert_eq!(rows[1].created_at, None);
        assert_eq!(rows[1].role, MessageRole::Agent);
        assert_eq!(rows[2].role, MessageRole::Agent);
        assert_eq!(rows[5].role, MessageRole::Other("custom".into()));
        for (ordinal, row) in rows.iter().enumerate() {
            assert_eq!(row.id, Some(ordinal as i64 + 1));
            assert_eq!(row.idx, ordinal as i64 - 3);
            assert!(row.extra_json.is_null() && row.snippets.is_empty());
        }
        let mut empty = 0;
        messages(&connection, 999, &mut |_| {
            empty += 1;
            Ok(())
        })?;
        assert_eq!(empty, 0);
        connection.close_without_checkpoint()?;
    }
    Ok(())
}

#[test]
fn callback_errors_never_trigger_missing_index_fallback_or_lose_their_type() -> Result<()> {
    for indexed in [true, false] {
        let temp = tempfile::tempdir()?;
        let connection = database(&temp.path().join("errors.db"), indexed)?;
        connection.execute("INSERT INTO messages VALUES (1, 7, 0, 'user', NULL, NULL, 'body')")?;
        let mut delivered = 0;
        let error = messages(&connection, 7, &mut |_| {
            delivered += 1;
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "no such index: sqlite_autoindex_messages_1",
            )
            .into())
        })
        .unwrap_err();
        assert_eq!(
            delivered, 1,
            "consumer failures must never replay already delivered rows"
        );
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
        connection.execute("ALTER TABLE messages RENAME TO retained_messages")?;
        let error = messages(&connection, 7, &mut |_| {
            delivered += 1;
            Ok(())
        })
        .expect_err("missing canonical data is not successful empty replay");
        assert!(error.downcast_ref::<FrankenError>().is_some());
        assert_eq!(delivered, 1);
        connection.close_without_checkpoint()?;
    }
    Ok(())
}
