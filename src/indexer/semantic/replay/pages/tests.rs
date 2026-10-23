use super::*;
use crate::franken_sync::compat::{ConnectionExt, ParamValue};
use std::io;
use std::path::Path;

fn database(path: &Path, indexed: bool) -> Result<Connection> {
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

fn insert(connection: &Connection, id: i64, idx: i64, text: &str) -> Result<()> {
    connection.execute_compat(
        "INSERT INTO messages VALUES (?1, 7, ?2, 'user', NULL, 1, ?3)",
        &[
            ParamValue::from(id),
            ParamValue::from(idx),
            ParamValue::from(text),
        ],
    )?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("consumer stopped bounded replay")]
struct Stop;

#[test]
fn body_admission_uses_utf8_bytes_and_keeps_one_oversized_message_whole() -> Result<()> {
    for indexed in [true, false] {
        let temp = tempfile::tempdir()?;
        let connection = database(&temp.path().join("bytes.db"), indexed)?;
        let texts = ["éé", "東京", "oversized payload", "", "x"];
        for (i, text) in texts.iter().enumerate() {
            insert(&connection, i as i64 + 1, i as i64, text)?;
        }
        let limits = Limits {
            rows: 4,
            content_bytes: 8,
        };
        let mut cursor = None;
        let mut use_index = unique_conversation_index(&connection);
        assert_eq!(use_index, indexed);
        let mut admitted = Vec::new();
        let mut all = Vec::new();
        loop {
            let page = read_page(&connection, 7, cursor, &mut use_index, limits)?;
            assert!(page.messages.len() <= limits.rows);
            let bytes: usize = page
                .messages
                .iter()
                .map(|message| message.content.len())
                .sum();
            assert!(bytes <= limits.content_bytes || page.messages.len() == 1);
            admitted.push(
                page.messages
                    .iter()
                    .map(|message| message.id.unwrap())
                    .collect::<Vec<_>>(),
            );
            all.extend(page.messages.into_iter().map(|message| message.content));
            if page.exhausted {
                break;
            }
            assert!(page.last > cursor);
            cursor = page.last;
        }
        assert_eq!(admitted, [vec![1], vec![2], vec![3], vec![4, 5]]);
        assert_eq!(all, texts);
        connection.close_without_checkpoint()?;
    }
    Ok(())
}

#[test]
fn cursor_preserves_signed_gaps_and_legacy_duplicate_order_keys() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = database(&temp.path().join("tuple.db"), false)?;
    let rows = [
        (i64::MAX, i64::MAX),
        (3, 0),
        (-1, 0),
        (2, -7),
        (i64::MIN, i64::MIN),
        (1, 0),
        (7, i64::MAX),
    ];
    for (id, idx) in rows {
        insert(&connection, id, idx, "same text")?;
    }
    for rows_per_page in [1, 2, 3, MAX_ROWS] {
        let mut actual = Vec::new();
        messages_with_limits(
            &connection,
            7,
            Limits {
                rows: rows_per_page,
                content_bytes: MAX_CONTENT_BYTES,
            },
            &mut |message| {
                actual.push((message.id.unwrap(), message.idx));
                Ok(())
            },
        )?;
        let mut expected = rows.to_vec();
        expected.sort_by_key(|(id, idx)| (*idx, *id));
        assert_eq!(actual, expected);
    }
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn page_callbacks_can_query_the_same_connection_without_ending_its_transaction() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = database(&temp.path().join("reentrant.db"), true)?;
    for id in 1..=7 {
        insert(&connection, id, id, "row")?;
    }
    connection.execute("BEGIN")?;
    let mut ids = Vec::new();
    messages_with_limits(
        &connection,
        7,
        Limits {
            rows: 2,
            content_bytes: 4,
        },
        &mut |message| {
            let count: i64 = connection.query_row_map(
                "SELECT COUNT(*) FROM messages",
                &[] as &[ParamValue],
                |row| row.get_typed(0),
            )?;
            assert_eq!(count, 7);
            assert!(connection.as_async().in_transaction());
            ids.push(message.id.unwrap());
            Ok(())
        },
    )?;
    assert_eq!(ids, (1..=7).collect::<Vec<_>>());
    assert!(connection.as_async().in_transaction());
    connection.execute("ROLLBACK")?;
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn malformed_unadmitted_body_is_not_read_and_later_sql_errors_remain_typed() -> Result<()> {
    for indexed in [true, false] {
        let temp = tempfile::tempdir()?;
        let connection = database(&temp.path().join("failure.db"), indexed)?;
        insert(&connection, 1, 0, "four")?;
        insert(&connection, 2, 1, "eightxxx")?;
        connection.execute("UPDATE messages SET role = X'00' WHERE id = 2")?;
        let mut use_index = unique_conversation_index(&connection);
        assert_eq!(use_index, indexed);
        let first = read_page(
            &connection,
            7,
            None,
            &mut use_index,
            Limits {
                rows: MAX_ROWS,
                content_bytes: 4,
            },
        )?;
        assert_eq!(
            first.messages.len(),
            1,
            "reject the next body before its String decode"
        );
        assert!(!first.exhausted);
        let error = match read_page(
            &connection,
            7,
            first.last,
            &mut use_index,
            Limits {
                rows: MAX_ROWS,
                content_bytes: 4,
            },
        ) {
            Ok(_) => panic!("resuming must report the corrupt body"),
            Err(error) => error,
        };
        assert!(error.downcast_ref::<FrankenError>().is_some());
        let mut delivered = 0;
        let error = messages_with_limits(
            &connection,
            7,
            Limits {
                rows: MAX_ROWS,
                content_bytes: 4,
            },
            &mut |_| {
                delivered += 1;
                Err(Stop.into())
            },
        )
        .unwrap_err();
        assert!(error.is::<Stop>());
        assert_eq!(delivered, 1);
        connection.close_without_checkpoint()?;
    }
    Ok(())
}

#[test]
fn row_limit_stops_before_decoding_a_corrupt_tail_of_a_large_history() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = database(&temp.path().join("large.db"), true)?;
    for id in 1..=MAX_ROWS as i64 + 1 {
        insert(&connection, id, id, "x")?;
    }
    connection.execute(&format!(
        "UPDATE messages SET role = X'00' WHERE id = {}",
        MAX_ROWS + 1
    ))?;
    let mut delivered = 0;
    let error = messages(&connection, 7, &mut |_| {
        delivered += 1;
        Err(Stop.into())
    })
    .unwrap_err();
    assert!(error.is::<Stop>());
    assert_eq!(delivered, 1);
    let mut delivered = 0;
    let error = messages(&connection, 7, &mut |_| {
        delivered += 1;
        Ok(())
    })
    .unwrap_err();
    assert!(error.downcast_ref::<FrankenError>().is_some());
    assert_eq!(delivered, MAX_ROWS);
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn role_metadata_and_full_long_text_match_the_canonical_projection() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = database(&temp.path().join("projection.db"), true)?;
    let roles = ["user", "assistant", "agent", "tool", "system", "custom"];
    let long = "Unicode cafe\u{301} 東京\n".repeat(100_000);
    assert!(long.len() > MAX_CONTENT_BYTES);
    for (i, role) in roles.iter().enumerate() {
        connection.execute_compat(
            "INSERT INTO messages VALUES (?1, 7, ?2, ?3, 'writer', NULL, ?4)",
            &[
                ParamValue::from(i as i64 + 1),
                ParamValue::from(i as i64 - 3),
                ParamValue::from(*role),
                ParamValue::from(if i == 0 { long.as_str() } else { "" }),
            ],
        )?;
    }
    connection.execute("PRAGMA query_only = ON")?;
    let mut actual = Vec::new();
    messages(&connection, 7, &mut |message| {
        actual.push(message);
        Ok(())
    })?;
    assert_eq!(actual.len(), roles.len());
    assert_eq!(actual[0].content, long);
    assert_eq!(actual[1].role, MessageRole::Agent);
    assert_eq!(actual[2].role, MessageRole::Agent);
    assert_eq!(actual[5].role, MessageRole::Other("custom".into()));
    for (i, message) in actual.iter().enumerate() {
        assert_eq!(message.id, Some(i as i64 + 1));
        assert_eq!(message.idx, i as i64 - 3);
        assert_eq!(message.author.as_deref(), Some("writer"));
        assert_eq!(message.created_at, None);
        assert!(message.extra_json.is_null() && message.snippets.is_empty());
    }
    connection.close_without_checkpoint()?;
    Ok(())
}

#[test]
fn empty_data_limits_and_errors_never_turn_into_successful_partial_replay() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let connection = database(&temp.path().join("errors.db"), true)?;
    messages(&connection, 7, &mut |_| panic!("empty parent has no body"))?;
    for limits in [
        Limits {
            rows: 0,
            content_bytes: 1,
        },
        Limits {
            rows: MAX_ROWS + 1,
            content_bytes: 1,
        },
        Limits {
            rows: 1,
            content_bytes: 0,
        },
        Limits {
            rows: 1,
            content_bytes: MAX_CONTENT_BYTES + 1,
        },
    ] {
        assert!(read_page(&connection, 7, None, &mut true, limits).is_err());
    }
    insert(&connection, 1, 0, "valid")?;
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
    assert_eq!(delivered, 1);
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
    connection.execute("ALTER TABLE messages RENAME TO retained_messages")?;
    let error = messages(&connection, 7, &mut |_| Ok(())).unwrap_err();
    assert!(error.downcast_ref::<FrankenError>().is_some());
    connection.close_without_checkpoint()?;
    Ok(())
}

/// GH #534: on the cass schema every replay page query is a seek on the
/// (conversation_id, idx) key. FrankenSQLite 0.4.12 planned the previous
/// hinted `ORDER BY idx, id` shape as `SCAN messages` plus a temp B-tree and
/// read the whole table for every page (10k rows: 80 ms per page against
/// 1.3 ms for the seek; about 2 GB per conversation on the reporter's
/// archive). Any page shape that scans or sorts fails here. Paging over one
/// conversation of a populated table still returns exactly its messages in
/// idx order across cursor pages. The indexed shape is chosen from the schema:
/// only an autoindex on exactly (conversation_id, idx) qualifies, because the
/// engine does not reject the hint for a missing index in this shape and its
/// `idx >` cursor is wrong without the uniqueness.
#[test]
fn gh534_replay_page_queries_seek_the_conversation_key() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let unindexed = database(&temp.path().join("unindexed.db"), false)?;
    assert!(!unique_conversation_index(&unindexed));
    unindexed.close_without_checkpoint()?;
    let reversed = Connection::open(
        temp.path()
            .join("reversed.db")
            .to_string_lossy()
            .into_owned(),
    )?;
    reversed.execute(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, conversation_id INTEGER,
         idx INTEGER, role TEXT, author TEXT, created_at INTEGER, content TEXT,
         UNIQUE(idx, conversation_id))",
    )?;
    assert!(!unique_conversation_index(&reversed));
    reversed.close_without_checkpoint()?;

    let connection = database(&temp.path().join("gh534.db"), true)?;
    assert!(unique_conversation_index(&connection));
    connection.execute("BEGIN")?;
    let mut id = 1i64;
    for conversation in 1..=40i64 {
        for idx in 0..8i64 {
            connection.execute_compat(
                "INSERT INTO messages VALUES (?1, ?2, ?3, 'user', NULL, 1, 'body')",
                &[
                    ParamValue::from(id),
                    ParamValue::from(conversation),
                    ParamValue::from(idx),
                ],
            )?;
            id += 1;
        }
    }
    connection.execute("COMMIT")?;
    // Conversation 21 holds ids 161..=168 at idx 0..=7.
    let cursor = Some(Cursor { idx: 3, id: 164 });
    for (label, cursor, projection) in [
        ("identity, first page", None, IDENTITY_COLUMNS),
        ("identity, cursor page", cursor, IDENTITY_COLUMNS),
        ("body, first page", None, BODY_COLUMNS),
        ("body, cursor page", cursor, BODY_COLUMNS),
    ] {
        let mut plan: Vec<String> = Vec::new();
        connection.query_with_params_for_each(
            &format!("EXPLAIN QUERY PLAN {}", query(cursor, true, projection)),
            &bound_values(21, cursor, true, MAX_ROWS),
            |row| {
                plan.push(row.get_typed(3)?);
                Ok(())
            },
        )?;
        assert!(
            plan.iter().any(|detail| detail.starts_with(
                "SEARCH messages USING INDEX sqlite_autoindex_messages_1 (conversation_id=?"
            )),
            "{label}: {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|detail| detail.starts_with("SCAN") || detail.contains("TEMP B-TREE")),
            "{label}: {plan:?}"
        );
    }
    let mut seen = Vec::new();
    messages_with_limits(
        &connection,
        21,
        Limits {
            rows: 3,
            content_bytes: MAX_CONTENT_BYTES,
        },
        &mut |message| {
            seen.push((message.id, message.idx));
            Ok(())
        },
    )?;
    assert_eq!(
        seen,
        (0..8i64)
            .map(|idx| (Some(161 + idx), idx))
            .collect::<Vec<_>>()
    );
    connection.close_without_checkpoint()?;
    Ok(())
}
