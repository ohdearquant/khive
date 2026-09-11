//! Schema acceptance and isolated mutation controls for ordered streams.
use rusqlite::{params, Connection};

const DDL: &str = include_str!("../sql/029-note-streams.sql");

fn fixture(ddl: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA recursive_triggers=OFF; CREATE TABLE notes (id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, content TEXT NOT NULL, properties TEXT, deleted_at INTEGER, salience REAL, name TEXT, decay_factor REAL);").unwrap();
    conn.execute_batch(ddl).unwrap();
    for (id, namespace, deleted) in [
        ("member", "local", None),
        ("next", "local", None),
        ("foreign", "other", None),
        ("deleted", "local", Some(1)),
    ] {
        conn.execute("INSERT INTO notes(id,namespace,kind,content,deleted_at) VALUES (?1,?2,'observation','null',?3)", params![id, namespace, deleted]).unwrap();
    }
    conn.execute(
        "INSERT INTO note_streams VALUES ('local','s',1,'member')",
        [],
    )
    .unwrap();
    conn
}

fn density(conn: &Connection) -> (i64, i64) {
    conn.query_row("SELECT COUNT(*), COALESCE(MAX(seq),0) FROM note_streams WHERE namespace='local' AND stream='s'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
}

#[test]
fn stream_schema_rejects_every_forbidden_direct_write() {
    let conn = fixture(DDL);
    for sql in [
        "UPDATE notes SET content='changed' WHERE id='member'",
        "UPDATE notes SET properties='{}' WHERE id='member'",
        "UPDATE notes SET deleted_at=1 WHERE id='member'",
        "UPDATE notes SET namespace='other' WHERE id='member'",
        "UPDATE notes SET kind='insight' WHERE id='member'",
        "UPDATE notes SET id='replacement' WHERE id='member'",
        "DELETE FROM notes WHERE id='member'",
        "DELETE FROM note_streams",
        "UPDATE note_streams SET seq=8",
        "UPDATE note_streams SET note_id='next'",
        "INSERT INTO note_streams VALUES ('local','s',0,'next')",
        "INSERT INTO note_streams VALUES ('local','s',1,'next')",
        "INSERT INTO note_streams VALUES ('local','s',3,'next')",
        "INSERT INTO note_streams VALUES ('local','s',2,'missing')",
        "INSERT INTO note_streams VALUES ('local','s',2,'foreign')",
        "INSERT INTO note_streams VALUES ('local','s',2,'deleted')",
        "INSERT OR REPLACE INTO note_streams VALUES ('local','s',2,'member')",
    ] {
        assert!(
            conn.execute(sql, []).is_err(),
            "forbidden statement succeeded: {sql}"
        );
        assert_eq!(density(&conn), (1, 1), "after {sql}");
    }
    conn.execute(
        "UPDATE notes SET salience=0.8,name='display',decay_factor=0.2 WHERE id='member'",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO note_streams VALUES ('local','s',2,'next')", [])
        .unwrap();
    assert_eq!(density(&conn), (2, 2));
}

#[test]
fn stream_schema_mutations_isolate_six_triggers_check_and_foreign_key() {
    // Each mutant removes exactly the named guard; the matching forbidden
    // operation must become possible. The restored control is the test above.
    for (trigger, sql) in [
        ("refuse_stream_foreign_note", "INSERT INTO note_streams VALUES ('local','s',2,'foreign')"),
        ("refuse_stream_gap", "INSERT INTO note_streams VALUES ('local','s',3,'next')"),
        ("refuse_stream_ledger_delete", "DELETE FROM note_streams"),
        ("refuse_stream_ledger_update", "UPDATE note_streams SET seq=8"),
        ("refuse_stream_entry_rewrite", "UPDATE notes SET content='changed' WHERE id='member'"),
        // Dropping DELETE still leaves FK: delete+reinsert the same id inside
        // one deferred-FK transaction isolates the entry-delete trigger.
        ("refuse_stream_entry_delete", "PRAGMA defer_foreign_keys=ON; BEGIN; DELETE FROM notes WHERE id='member'; INSERT INTO notes(id,namespace,kind,content) VALUES ('member','local','observation','changed'); COMMIT;"),
    ] {
        let conn = fixture(DDL);
        conn.execute_batch(&format!("DROP TRIGGER {trigger};")).unwrap();
        conn.execute_batch(sql).unwrap_or_else(|e| panic!("mutation {trigger} not isolated: {e}"));
    }
    let conn = fixture(DDL);
    conn.execute_batch("DROP TRIGGER refuse_stream_gap;")
        .unwrap();
    assert!(conn
        .execute("INSERT INTO note_streams VALUES ('local','s',0,'next')", [])
        .is_err());
    let no_check = DDL.replace("CHECK (seq > 0)", "");
    assert_ne!(no_check, DDL, "mutation must change the exact schema");
    let conn = fixture(&no_check);
    conn.execute_batch("DROP TRIGGER refuse_stream_gap;")
        .unwrap();
    conn.execute("INSERT INTO note_streams VALUES ('local','s',0,'next')", [])
        .unwrap();
    let conn = fixture(DDL);
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute("UPDATE notes SET id='replacement' WHERE id='member'", [])
        .unwrap();
}

#[test]
fn stream_schema_replacement_mutation_exposes_count_head_divergence() {
    let clause = "OR EXISTS (SELECT 1 FROM note_streams WHERE note_id = NEW.note_id)";
    let mutant = DDL.replace(clause, "");
    assert_ne!(mutant, DDL);
    let conn = fixture(&mutant);
    conn.execute(
        "INSERT OR REPLACE INTO note_streams VALUES ('local','s',2,'member')",
        [],
    )
    .unwrap();
    assert_eq!(density(&conn), (1, 2));
}
