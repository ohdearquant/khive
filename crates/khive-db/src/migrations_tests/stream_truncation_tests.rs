//! V60 schema acceptance; public truncation composition is exercised by the runtime.
use super::*;
use rusqlite::types::Value;

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut statement = conn.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_map([], |row| {
            (0..columns)
                .map(|column| row.get(column))
                .collect::<rusqlite::Result<Vec<Value>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn stream_schema(conn: &Connection) -> Vec<Vec<Value>> {
    rows(
        conn,
        "SELECT type,name,tbl_name,sql FROM sqlite_schema \
         WHERE name LIKE 'note_stream%' OR name LIKE 'refuse_stream_%' ORDER BY type,name",
    )
}

fn seed_note(conn: &Connection, id: &str, namespace: &str) {
    conn.execute(
        "INSERT INTO notes(id,namespace,kind,content,created_at,updated_at) \
         VALUES (?1,?2,'observation','null',1,1)",
        [id, namespace],
    )
    .unwrap();
}

fn count_and_head(conn: &Connection, namespace: &str, stream: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT COUNT(*), MAX(COALESCE(MAX(seq),0), \
         COALESCE((SELECT floor_seq FROM note_stream_floors \
                   WHERE namespace=?1 AND stream=?2),1)-1) \
         FROM note_streams WHERE namespace=?1 AND stream=?2",
        [namespace, stream],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

#[test]
fn stream_truncation_migration_preserves_rows_and_replays_without_schema_changes() {
    let mut fresh = open_memory();
    run_migrations(&mut fresh).unwrap();
    for name in ["note_stream_floors", "note_stream_truncations"] {
        assert!(
            table_exists(&fresh, name),
            "the registered V60 migration must install {name}"
        );
    }
    let expected_schema = stream_schema(&fresh);
    assert_eq!(
        fresh.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='trigger' AND name LIKE 'refuse_stream_%'",
            [], |row| row.get::<_, i64>(0)
        ).unwrap(),
        6,
        "V60 replaces two guards and retains the other four"
    );
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.version == 60)
        .unwrap();
    assert_eq!(migration.name, "note_stream_truncation");
    for populated in [false, true] {
        let mut conn = open_memory();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate_through(&mut conn, 59);
        if populated {
            seed_note(&conn, "first", "local");
            seed_note(&conn, "second", "local");
            conn.execute_batch(
                "INSERT INTO note_streams VALUES ('local','s',1,'first'); \
                 INSERT INTO note_streams VALUES ('local','s',2,'second');",
            )
            .unwrap();
        }
        let notes = rows(&conn, "SELECT * FROM notes ORDER BY id");
        let ledger = rows(
            &conn,
            "SELECT * FROM note_streams ORDER BY namespace,stream,seq",
        );
        let history = rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version");
        run_migrations(&mut conn).unwrap();
        assert_eq!(stream_schema(&conn), expected_schema);
        assert_eq!(rows(&conn, "SELECT * FROM notes ORDER BY id"), notes);
        assert_eq!(
            rows(
                &conn,
                "SELECT * FROM note_streams ORDER BY namespace,stream,seq"
            ),
            ledger
        );
        assert_eq!(
            rows(
                &conn,
                "SELECT * FROM _schema_migrations WHERE version<60 ORDER BY version"
            ),
            history
        );
        assert!(rows(&conn, "SELECT * FROM note_stream_floors").is_empty());
        assert!(rows(&conn, "SELECT * FROM note_stream_truncations").is_empty());
        conn.execute(
            "INSERT INTO note_stream_floors VALUES ('local','retained',7)",
            [],
        )
        .unwrap();
        let schema_version: i64 = conn
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        for replay in [false, true] {
            if replay {
                // Replaying V60 requires rewinding every later ledger entry as well.
                assert_eq!(
                    conn.execute("DELETE FROM _schema_migrations WHERE version>=60", [])
                        .unwrap(),
                    MIGRATIONS
                        .iter()
                        .filter(|migration| migration.version >= 60)
                        .count()
                );
            }
            run_migrations(&mut conn).unwrap();
            assert_eq!(stream_schema(&conn), expected_schema);
            assert_eq!(
                conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                schema_version
            );
            assert_eq!(rows(&conn, "SELECT * FROM notes ORDER BY id"), notes);
            assert_eq!(
                rows(
                    &conn,
                    "SELECT * FROM note_streams ORDER BY namespace,stream,seq"
                ),
                ledger
            );
            assert_eq!(
                rows(&conn, "SELECT floor_seq FROM note_stream_floors"),
                vec![vec![Value::Integer(7)]]
            );
            assert!(rows(&conn, "SELECT * FROM note_stream_truncations").is_empty());
            assert!(conn.is_autocommit());
        }
    }
}

#[test]
fn stream_truncation_permit_is_scoped_and_empty_stream_keeps_its_floor() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for (id, namespace) in [
        ("s1", "local"),
        ("s2", "local"),
        ("s3", "local"),
        ("s4", "local"),
        ("next", "local"),
        ("other", "local"),
        ("foreign", "other"),
    ] {
        seed_note(&conn, id, namespace);
    }
    conn.execute_batch(
        "INSERT INTO note_streams VALUES ('local','s',1,'s1'); \
         INSERT INTO note_streams VALUES ('local','s',2,'s2'); \
         INSERT INTO note_streams VALUES ('local','s',3,'s3'); \
         INSERT INTO note_streams VALUES ('local','other',1,'other'); \
         INSERT INTO note_streams VALUES ('other','s',1,'foreign');",
    )
    .unwrap();
    assert!(conn
        .execute(
            "INSERT INTO note_stream_floors VALUES ('local','bad',0)",
            []
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO note_stream_truncations VALUES ('local','bad',0)",
            []
        )
        .is_err());
    assert!(conn
        .execute(
            "DELETE FROM note_streams WHERE namespace='local' AND stream='s'",
            []
        )
        .is_err());
    conn.execute_batch(
        "BEGIN IMMEDIATE; INSERT INTO note_stream_truncations VALUES ('local','s',3);",
    )
    .unwrap();
    for sql in [
        "DELETE FROM note_streams WHERE namespace='other' AND stream='s'",
        "DELETE FROM note_streams WHERE namespace='local' AND stream='other'",
        "DELETE FROM note_streams WHERE namespace='local' AND stream='s' AND seq=3",
        "DELETE FROM notes WHERE id='s1'",
    ] {
        assert!(conn.execute(sql, []).is_err(), "permit overreached: {sql}");
    }
    assert_eq!(
        conn.execute(
            "DELETE FROM note_streams WHERE namespace='local' AND stream='s' AND seq<3",
            []
        )
        .unwrap(),
        2
    );
    assert_eq!(
        conn.execute("DELETE FROM notes WHERE id IN ('s1','s2')", [])
            .unwrap(),
        2
    );
    conn.execute_batch(
        "INSERT INTO note_stream_floors VALUES ('local','s',3); \
         DELETE FROM note_stream_truncations; COMMIT;",
    )
    .unwrap();
    assert_eq!(count_and_head(&conn, "local", "s"), (1, 3));
    conn.execute("INSERT INTO note_streams VALUES ('local','s',4,'s4')", [])
        .unwrap();
    conn.execute_batch(
        "BEGIN IMMEDIATE; INSERT INTO note_stream_truncations VALUES ('local','s',5); \
         DELETE FROM note_streams WHERE namespace='local' AND stream='s' AND seq<5; \
         DELETE FROM notes WHERE id IN ('s3','s4'); \
         UPDATE note_stream_floors SET floor_seq=5 WHERE namespace='local' AND stream='s'; \
         DELETE FROM note_stream_truncations; COMMIT;",
    )
    .unwrap();
    assert_eq!(count_and_head(&conn, "local", "s"), (0, 4));
    assert_eq!(count_and_head(&conn, "local", "other"), (1, 1));
    assert_eq!(count_and_head(&conn, "other", "s"), (1, 1));
    assert!(conn
        .execute("INSERT INTO note_streams VALUES ('local','s',1,'next')", [])
        .is_err());
    conn.execute("INSERT INTO note_streams VALUES ('local','s',5,'next')", [])
        .unwrap();
    assert_eq!(count_and_head(&conn, "local", "s"), (1, 5));
    for sql in [
        "DELETE FROM note_streams WHERE namespace='local' AND stream='s'",
        "DELETE FROM notes WHERE id='next'",
        "UPDATE notes SET content='changed' WHERE id='next'",
        "UPDATE note_streams SET seq=6 WHERE namespace='local' AND stream='s'",
    ] {
        assert!(
            conn.execute(sql, []).is_err(),
            "guard lost after truncation: {sql}"
        );
    }
    assert!(rows(&conn, "SELECT * FROM note_stream_truncations").is_empty());
}

#[test]
fn stream_truncation_schema_refuses_the_last_i64_sequence() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    seed_note(&conn, "last", "local");
    seed_note(&conn, "exhausted", "local");
    conn.execute(
        "INSERT INTO note_stream_floors VALUES ('local','top',?1)",
        [i64::MAX - 1],
    )
    .unwrap();
    assert_eq!(count_and_head(&conn, "local", "top"), (0, i64::MAX - 2));
    conn.execute(
        "INSERT INTO note_streams VALUES ('local','top',?1,'last')",
        [i64::MAX - 1],
    )
    .unwrap();
    let error = conn
        .execute(
            "INSERT INTO note_streams VALUES ('local','top',?1,'exhausted')",
            [i64::MAX],
        )
        .unwrap_err();
    assert!(error.to_string().contains("stream_gap"), "{error}");
    assert_eq!(count_and_head(&conn, "local", "top"), (1, i64::MAX - 1));
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    conn.execute(
        "INSERT INTO note_stream_truncations VALUES ('local','top',?1)",
        [i64::MAX],
    )
    .unwrap();
    assert_eq!(
        conn.execute(
            "DELETE FROM note_streams WHERE namespace='local' AND stream='top'",
            []
        )
        .unwrap(),
        1
    );
    conn.execute("DELETE FROM notes WHERE id='last'", [])
        .unwrap();
    conn.execute(
        "UPDATE note_stream_floors SET floor_seq=?1 WHERE namespace='local' AND stream='top'",
        [i64::MAX],
    )
    .unwrap();
    conn.execute_batch("DELETE FROM note_stream_truncations; COMMIT;")
        .unwrap();
    assert_eq!(count_and_head(&conn, "local", "top"), (0, i64::MAX - 1));
    assert_eq!(
        rows(&conn, "SELECT floor_seq FROM note_stream_floors"),
        vec![vec![Value::Integer(i64::MAX)]]
    );
    assert!(rows(&conn, "SELECT * FROM note_stream_truncations").is_empty());
    assert!(conn
        .execute(
            "INSERT INTO note_streams VALUES ('local','top',?1,'exhausted')",
            [i64::MAX]
        )
        .is_err());
}
