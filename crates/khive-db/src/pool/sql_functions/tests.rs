use super::*;
use crate::pool::{ConnectionPool, PoolConfig, StandaloneReaderPurpose};
use rusqlite::{params, types::Value};

const QUERY: &str = "SELECT khive_tag_contains(?1, ?2)";

fn matches(conn: &Connection, tags: &str) -> bool {
    conn.query_row(QUERY, params![tags, "type:domain"], |row| row.get(0))
        .unwrap()
}

#[test]
fn sql_tag_matching_preserves_decoding_case_and_legacy_fallback() {
    let conn = Connection::open_in_memory().unwrap();
    assert!(
        conn.prepare(QUERY).is_err(),
        "control: registration is required"
    );
    register_read_functions(&conn).unwrap();
    for (tags, expected) in [
        (r#"["type:domain"]"#, true),
        (r#"["type\u003adomain"]"#, true),
        (r#"["ordinary","type:domain"]"#, true),
        (r#"["type:domain-extra"]"#, false),
        (r#"["prefix:type:domain"]"#, false),
        (r#"["TYPE:DOMAIN"]"#, false),
        (r#"[" type:domain "]"#, false),
        (r#"["type:domain-extra",7]"#, true),
        (r#"["type\u003adomain",7]"#, false),
        (r#"{"tag":"type:domain"}"#, true),
        (r#""type:domain""#, true),
        ("broken type:domain", true),
        ("broken TYPE:DOMAIN", false),
        ("null", false),
        ("[]", false),
        ("", false),
    ] {
        assert_eq!(matches(&conn, tags), expected, "{tags}");
        assert_eq!(
            khive_types::tag_contains(tags, "type:domain"),
            expected,
            "{tags}"
        );
    }
    for tags in [
        Value::Null,
        Value::Integer(7),
        Value::Real(3.5),
        Value::Blob(b"type:domain".to_vec()),
    ] {
        let matched: bool = conn
            .query_row(QUERY, params![tags, "type:domain"], |row| row.get(0))
            .unwrap();
        assert!(
            !matched,
            "non-text tags use the row decoder's empty fallback"
        );
    }
    let invalid_utf8: bool = conn
        .query_row(
            "SELECT khive_tag_contains(CAST(x'ff' AS TEXT), 'type:domain')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!invalid_utf8);
    for (tail, expected) in [("type:domain", true), ("type:domain-extra", false)] {
        let mut tags = b"[\"".to_vec();
        tags.push(0xff);
        tags.extend_from_slice(format!("\",\"{tail}\"]").as_bytes());
        let matched: bool = conn
            .query_row(
                "SELECT khive_tag_contains(CAST(?1 AS TEXT), 'type:domain')",
                [tags.clone()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            matched, expected,
            "invalid UTF-8 uses the SQL row decoder's lossy text"
        );
        assert_eq!(
            matched,
            khive_types::tag_contains(&String::from_utf8_lossy(&tags), "type:domain")
        );
    }
    for marker in [Value::Null, Value::Integer(7)] {
        let matched: bool = conn
            .query_row(QUERY, params!["type:domain", marker], |row| row.get(0))
            .unwrap();
        assert!(!matched);
    }
}

#[test]
fn pooled_standalone_readonly_and_shared_readers_register_tag_matching() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tag-functions.db");
    {
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap();
        assert!(matches(
            pool.writer().unwrap().conn(),
            r#"["type\u003adomain"]"#
        ));
        for conn in [
            pool.open_standalone_writer().unwrap(),
            pool.open_standalone_writer_untracked().unwrap(),
        ] {
            assert!(matches(&conn, r#"["type\u003adomain"]"#));
            assert!(!matches(&conn, r#"["type:domain-extra"]"#));
        }
        let reader = pool.reader().unwrap();
        let matched: bool = reader
            .query_row(
                QUERY,
                params![r#"["type:domain-extra"]"#, "type:domain"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!matched, "public read-shape policy admits the function");
        drop(reader);
        for purpose in [
            StandaloneReaderPurpose::ExplicitSqlReadTransaction,
            StandaloneReaderPurpose::DiagnosticsIndependentSnapshot,
            StandaloneReaderPurpose::BootSchemaProbe,
        ] {
            assert!(matches(
                &pool.open_standalone_reader(purpose).unwrap(),
                "broken type:domain"
            ));
        }
    }
    // A read-only pool refuses a WAL database whose writable -shm sidecar is still on disk, so
    // the fixture becomes a rollback-journal snapshot first. The mode change needs exclusive
    // access, so it also proves the writable pool above released every connection.
    let mode: String = Connection::open(&path)
        .unwrap()
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "delete");
    let readonly = ConnectionPool::new(PoolConfig {
        path: Some(path),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    assert!(matches(&readonly.writer.lock(), r#"["type\u003adomain"]"#));
    let reader = readonly.reader().unwrap();
    let matched: bool = reader
        .query_row(QUERY, params![r#"["TYPE:DOMAIN"]"#, "type:domain"], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(!matched);
    drop(reader);
    let memory = ConnectionPool::new(PoolConfig {
        path: None,
        ..PoolConfig::for_test()
    })
    .unwrap();
    assert_eq!(
        memory.max_readers(),
        0,
        "in-memory read uses the shared writer slot"
    );
    let matched: bool = memory
        .reader()
        .unwrap()
        .query_row(QUERY, params!["broken type:domain", "type:domain"], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(matched);
}

#[tokio::test]
async fn writer_task_registers_tag_matching_on_its_owned_connection() {
    let dir = tempfile::tempdir().unwrap();
    let pool = std::sync::Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("tag-writer-task.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    let handle = crate::writer_task::spawn(&pool, 8).unwrap();
    let matched = handle
        .send(|conn| {
            conn.query_row(
                QUERY,
                params![r#"["type\u003adomain"]"#, "type:domain"],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| khive_storage::StorageError::Pool {
                operation: "test_tag_function".into(),
                message: error.to_string(),
            })
        })
        .await
        .unwrap();
    assert!(matched);
    drop(handle);
    if let Some(join) = pool.take_writer_task_join() {
        join.await.unwrap();
    }
}
