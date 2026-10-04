//! Attachment role integrity, migration preservation, and quarantine ownership.

use std::time::Duration;

use khive_db::migrations::{latest_schema_version, read_schema_version, run_migrations};
use khive_db::stores::blob::FsBlobStore;
use khive_db::StorageBackend;
use khive_storage::attachment::validate_attachment_role;
use khive_storage::{
    BlobStore, SqlReader, SqlStatement, SqlValue, StorageCapability, StorageError,
};
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

const LEGACY_ATTACHMENTS: &str = include_str!("../sql/021-attachments-a-stage.sql");
const ATTACHMENT_FENCES: &str = include_str!("../sql/021-attachments-b-claim-fences.sql");
const COLUMNS: &str =
    "record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at";

fn current_schema() -> Connection {
    let mut conn = Connection::open_in_memory().expect("database");
    run_migrations(&mut conn).expect("current migrations");
    conn
}

// Build the genuine V46 attachment table using the unchanged V21 DDL. The
// remaining schema is initialized by the public runner, including migrations
// whose bodies contain Rust work. No V47 production SQL is copied into this
// legacy setup. The same setup also runs with the V46 parent implementation.
fn restore_legacy_attachment_schema(conn: &Connection) {
    conn.execute_batch(
        "DROP TABLE IF EXISTS attachment_quarantine; \
         DROP TABLE attachments; \
         DELETE FROM _schema_migrations WHERE version > 46;",
    )
    .expect("restore legacy attachment epoch");
    conn.execute_batch(LEGACY_ATTACHMENTS)
        .expect("legacy attachment table");
    conn.execute_batch(ATTACHMENT_FENCES)
        .expect("legacy claim fences");
    assert_eq!(read_schema_version(conn).expect("legacy version"), 46);
}

fn row(id: &str, role: &str, content_ref: &str) -> Vec<Value> {
    vec![
        Value::Text(id.into()),
        Value::Text("note".into()),
        Value::Text(role.into()),
        Value::Text(content_ref.into()),
        Value::Text("application/x-test\0metadata-tail".into()),
        Value::Integer(42),
        Value::Integer(-17),
    ]
}

fn insert_row(conn: &Connection, values: &[Value]) -> rusqlite::Result<usize> {
    conn.execute(
        &format!("INSERT INTO attachments ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
        params_from_iter(values.iter()),
    )
}

fn rows(conn: &Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).expect("snapshot statement");
    let count = stmt.column_count();
    let mapped = stmt
        .query_map([], |row| {
            (0..count).map(|index| row.get::<_, Value>(index)).collect()
        })
        .expect("snapshot rows");
    mapped
        .collect::<rusqlite::Result<_>>()
        .expect("snapshot values")
}

fn attachment_rows(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    rows(
        conn,
        &format!("SELECT {COLUMNS} FROM {table} ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY"),
    )
}

async fn reader_rows(reader: &mut dyn SqlReader, sql: String) -> Vec<Vec<Value>> {
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params: vec![],
            label: Some("attachment-quarantine-snapshot".into()),
        })
        .await
        .expect("public reader snapshot");
    rows.into_iter()
        .map(|row| {
            row.columns
                .into_iter()
                .map(|column| match column.value {
                    SqlValue::Null => Value::Null,
                    SqlValue::Integer(value) => Value::Integer(value),
                    SqlValue::Float(value) => Value::Real(value),
                    SqlValue::Text(value) => Value::Text(value),
                    SqlValue::Blob(value) => Value::Blob(value),
                    other => panic!("snapshot must retain a native SQLite value, got {other:?}"),
                })
                .collect()
        })
        .collect()
}

async fn reader_attachment_rows(reader: &mut dyn SqlReader, table: &str) -> Vec<Vec<Value>> {
    reader_rows(
        reader,
        format!("SELECT {COLUMNS} FROM {table} ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY"),
    )
    .await
}

fn assert_check_refusal(conn: &Connection, role: &str, label: &str) {
    assert!(
        validate_attachment_role(role).is_err(),
        "the Rust role boundary must refuse {label}"
    );
    let error = insert_row(conn, &row(label, role, &"a".repeat(64)))
        .expect_err("the table CHECK must refuse a role rejected by the reader");
    match error {
        rusqlite::Error::SqliteFailure(error, _) => {
            assert_eq!(error.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_CHECK);
        }
        other => panic!("expected a role CHECK refusal for {label}, got {other:?}"),
    }
}

fn assert_unicode_roles_admitted(conn: &Connection) {
    for (index, role) in ["content", " ", "~", "\u{a0}", "résumé/附件", "消息🙂"]
        .iter()
        .enumerate()
    {
        validate_attachment_role(role).expect("readable Unicode role");
        let values = row(&format!("unicode-{index}"), role, &"b".repeat(64));
        insert_row(conn, &values).expect("valid role must remain admitted");
        let actual = rows(
            conn,
            &format!("SELECT {COLUMNS} FROM attachments WHERE record_uuid = 'unicode-{index}'"),
        );
        assert_eq!(actual, vec![values]);
    }
}

#[test]
fn attachment_role_check_preserves_accented_and_cjk_roles() {
    assert_unicode_roles_admitted(&current_schema());
}

#[test]
fn attachment_role_check_rejects_c0_controls() {
    let conn = current_schema();
    assert_unicode_roles_admitted(&conn);
    assert_check_refusal(&conn, "", "empty");
    for codepoint in 0..=0x1f {
        let role = format!("prefix{}tail", char::from_u32(codepoint).unwrap());
        assert_check_refusal(&conn, &role, &format!("c0-{codepoint:02x}"));
    }
}

#[test]
fn attachment_role_check_rejects_del() {
    let conn = current_schema();
    assert_unicode_roles_admitted(&conn);
    assert_check_refusal(&conn, "prefix\u{7f}tail", "del");
}

#[test]
fn attachment_role_check_rejects_c1_controls() {
    let conn = current_schema();
    assert_unicode_roles_admitted(&conn);
    for codepoint in 0x80..=0x9f {
        let role = format!("prefix{}tail", char::from_u32(codepoint).unwrap());
        assert_check_refusal(&conn, &role, &format!("c1-{codepoint:02x}"));
    }
}

fn assert_other_attachment_constraints(conn: &Connection) {
    let valid = row("constraint-positive", "content", &"c".repeat(64));
    insert_row(conn, &valid).expect("constraint positive guard");
    let mut variants = Vec::new();
    for column in [0, 1, 2, 3, 6] {
        let mut invalid = valid.clone();
        invalid[0] = Value::Text(format!("null-{column}"));
        invalid[column] = Value::Null;
        variants.push((format!("not-null-{column}"), invalid));
    }
    let mut invalid = valid.clone();
    invalid[0] = Value::Text("bad-substrate".into());
    invalid[1] = Value::Text("edge".into());
    variants.push(("substrate".into(), invalid));
    for (label, content_ref) in [
        ("short", "a".repeat(63)),
        ("uppercase", "A".repeat(64)),
        ("nonhex", "g".repeat(64)),
        ("nul-tail", format!("{}\0tail", "a".repeat(64))),
    ] {
        let mut invalid = valid.clone();
        invalid[0] = Value::Text(format!("bad-ref-{label}"));
        invalid[3] = Value::Text(content_ref);
        variants.push((format!("content-ref-{label}"), invalid));
    }
    let mut invalid = valid.clone();
    invalid[0] = Value::Text("negative-size".into());
    invalid[5] = Value::Integer(-1);
    variants.push(("nonnegative-size".into(), invalid));
    let mut invalid = valid.clone();
    invalid[0] = Value::Text("strict-size".into());
    invalid[5] = Value::Text("not-an-integer".into());
    variants.push(("strict-type".into(), invalid));
    for (label, values) in variants {
        assert!(
            insert_row(conn, &values).is_err(),
            "preserved constraint must refuse: {label}"
        );
    }
    insert_row(conn, &valid).expect_err("record_uuid/role primary key remains unique");
    let mut nullable = row("nullable-metadata", "content", &"d".repeat(64));
    nullable[4] = Value::Null;
    nullable[5] = Value::Null;
    insert_row(conn, &nullable).expect("nullable media type and size remain accepted");
}

#[test]
fn attachment_role_migration_quarantines_exact_rows_and_preserves_live_schema() {
    let mut conn = current_schema();
    restore_legacy_attachment_schema(&conn);
    let valid = row("valid", "résumé/附件", &"a".repeat(64));
    let c0 = row("c0", "message-attachment:0\0tail", &"b".repeat(64));
    let mut c1 = row("c1", "message-attachment:1\u{85}", &"c".repeat(64));
    c1[1] = Value::Text("entity".into());
    c1[4] = Value::Null;
    c1[5] = Value::Integer(i64::MAX);
    c1[6] = Value::Integer(i64::MAX);
    for values in [&valid, &c0, &c1] {
        insert_row(&conn, values).expect("legacy role admitted");
    }
    let before = attachment_rows(&conn, "attachments");
    let indexes_before = rows(
        &conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE type IN ('index', 'trigger') ORDER BY type, name",
    );
    assert_eq!(before.len(), 3);
    assert_eq!(
        run_migrations(&mut conn).expect("V47 upgrade"),
        latest_schema_version()
    );
    assert_eq!(attachment_rows(&conn, "attachments"), vec![valid]);
    let quarantined = attachment_rows(&conn, "attachment_quarantine");
    assert_eq!(quarantined, vec![c0, c1]);
    assert_eq!(
        rows(
            &conn,
            "SELECT reason FROM attachment_quarantine ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY",
        ),
        vec![vec![Value::Text("invalid_role".into())]; 2]
    );
    assert_eq!(
        attachment_rows(&conn, "attachments").len() + quarantined.len(),
        before.len(),
        "every legacy row remains either readable or quarantined"
    );
    let indexes_after = rows(
        &conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE type IN ('index', 'trigger') AND tbl_name <> 'attachment_quarantine' \
         ORDER BY type, name",
    );
    assert_eq!(
        indexes_after, indexes_before,
        "old indexes and fences must remain exact"
    );
    assert_other_attachment_constraints(&conn);
    assert_check_refusal(&conn, "prefix\u{85}tail", "post-upgrade-c1");
    let live_after = attachment_rows(&conn, "attachments");
    let quarantine_after = attachment_rows(&conn, "attachment_quarantine");
    assert_eq!(
        run_migrations(&mut conn).expect("idempotent rerun"),
        latest_schema_version()
    );
    assert_eq!(attachment_rows(&conn, "attachments"), live_after);
    assert_eq!(
        attachment_rows(&conn, "attachment_quarantine"),
        quarantine_after
    );
}

#[tokio::test]
async fn migrated_v47_gc_preserves_exact_v21_admission_refusal() {
    let dir = tempfile::tempdir().expect("fixture directory");
    let backend =
        StorageBackend::sqlite_for_test(dir.path().join("khive.db")).expect("file-backed database");
    backend.prepare_core_schema().expect("initial schema");
    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .expect("blob store")
        .with_orphan_sweep_grace(Duration::ZERO);
    let quarantine_bytes = b"quarantined attachment".to_vec();
    let live_bytes = b"readable attachment".to_vec();
    let orphan_bytes = b"independent orphan".to_vec();
    let quarantined = store
        .put(quarantine_bytes.clone())
        .await
        .expect("quarantined blob");
    let live = store.put(live_bytes.clone()).await.expect("live blob");
    let orphan = store.put(orphan_bytes.clone()).await.expect("orphan blob");
    {
        let writer = backend.pool().writer().expect("fixture writer");
        restore_legacy_attachment_schema(writer.conn());
        insert_row(
            writer.conn(),
            &row(
                "quarantined-owner",
                "message-attachment:0\u{85}",
                quarantined.as_str(),
            ),
        )
        .expect("legacy quarantined owner");
        insert_row(
            writer.conn(),
            &row("live-owner", "message-attachment:0", live.as_str()),
        )
        .expect("live attachment owner");
    }
    assert_eq!(
        backend.prepare_core_schema().expect("migrate quarantine"),
        latest_schema_version()
    );
    let (live_before, quarantine_before) = {
        let mut reader = backend.sql().reader().await.expect("snapshot reader");
        let active = reader_attachment_rows(reader.as_mut(), "attachments").await;
        let quarantine = reader_rows(reader.as_mut(), format!(
            "SELECT {COLUMNS}, reason FROM attachment_quarantine ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY"
        )).await;
        assert_eq!(active.len(), 1);
        assert_eq!(quarantine.len(), 1);
        (active, quarantine)
    };
    for dry_run in [true, false] {
        let error = store
            .transactional_orphan_sweep(backend.sql().as_ref(), dry_run)
            .await
            .expect_err("V47 must retain exact-V21 GC admission refusal");
        match error {
            StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation,
                ..
            } => {
                assert_eq!(operation, "transactional_orphan_sweep");
            }
            other => {
                panic!("expected typed GC admission refusal for dry_run={dry_run}, got {other:?}")
            }
        }
        for (reference, bytes) in [
            (&quarantined, &quarantine_bytes),
            (&live, &live_bytes),
            (&orphan, &orphan_bytes),
        ] {
            assert!(store
                .exists(reference)
                .await
                .expect("unchanged object stat"));
            assert_eq!(
                store
                    .get_bounded_verified(reference, 64)
                    .await
                    .expect("unchanged object bytes"),
                *bytes
            );
        }
        let mut reader = backend.sql().reader().await.expect("refusal state reader");
        let claims = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM blob_gc_claims".into(),
                params: vec![],
                label: Some("attachment-quarantine-claims".into()),
            })
            .await
            .expect("claims remain empty");
        let claims = match claims {
            Some(SqlValue::Integer(claims)) => claims,
            other => panic!("claim count must remain an integer: {other:?}"),
        };
        assert_eq!(
            claims, 0,
            "admission refusal cannot create or delete a claim"
        );
        assert_eq!(
            reader_attachment_rows(reader.as_mut(), "attachments").await,
            live_before
        );
        assert_eq!(reader_rows(reader.as_mut(), format!(
            "SELECT {COLUMNS}, reason FROM attachment_quarantine ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY"
        )).await, quarantine_before);
    }
}

fn assert_attachment_and_quarantine_claim_fences(conn: &Connection) {
    let claimed_ref = "e".repeat(64);
    conn.execute(
        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) VALUES ('fixture', ?1, 1)",
        [&claimed_ref],
    )
    .expect("active blob claim");
    for table in ["attachments", "attachment_quarantine"] {
        let values = row(&format!("claimed-{table}"), "content", &claimed_ref);
        let insert = if table == "attachments" {
            format!("INSERT INTO attachments ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")
        } else {
            format!("INSERT INTO attachment_quarantine ({COLUMNS}, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'invalid_role')")
        };
        let insert_error = conn
            .execute(&insert, params_from_iter(values.iter()))
            .expect_err("recreated insert fence must refuse a claimed content reference");
        let (record_uuid, role): (String, String) = conn
            .query_row(
                &format!(
                    "SELECT record_uuid, role FROM {table} ORDER BY record_uuid, role LIMIT 1"
                ),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("preexisting attachment owner");
        let update_error = conn
            .execute(
                &format!(
                    "UPDATE {table} SET content_ref = ?1 WHERE record_uuid = ?2 AND role = ?3"
                ),
                rusqlite::params![claimed_ref, record_uuid, role],
            )
            .expect_err("recreated update fence must refuse a claimed content reference");
        for error in [insert_error, update_error] {
            match error {
                rusqlite::Error::SqliteFailure(code, message) => {
                    assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER);
                    assert_eq!(
                        message.as_deref(),
                        Some("content_ref is reserved by an active blob sweep")
                    );
                }
                other => panic!("expected canonical claim fence refusal, got {other:?}"),
            }
        }
    }
}

#[test]
fn attachment_role_migration_installs_absent_claim_fences() {
    let mut conn = current_schema();
    restore_legacy_attachment_schema(&conn);
    conn.execute_batch(
        "DROP TRIGGER attachments_reject_claimed_blob_insert; \
         DROP TRIGGER attachments_reject_claimed_blob_update;",
    )
    .expect("isolated historical fixture without finalized claim fences");
    let missing_fences: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' \
             AND name IN ('attachments_reject_claimed_blob_insert', 'attachments_reject_claimed_blob_update')",
            [],
            |row| row.get(0),
        )
        .expect("missing-fence premise");
    assert_eq!(missing_fences, 0);
    let valid = row("valid", "résumé/附件", &"a".repeat(64));
    let rejected = row("rejected", "content\0tail", &"b".repeat(64));
    insert_row(&conn, &valid).expect("legacy readable row");
    insert_row(&conn, &rejected).expect("legacy rejected-role row");
    let migration = run_migrations(&mut conn);
    assert!(
        migration.is_ok(),
        "the migration operation must accept absent historical claim fences: {migration:?}"
    );
    assert_check_refusal(
        &conn,
        "prefix\u{85}tail",
        "strict-c1-after-missing-fence-upgrade",
    );
    assert_eq!(
        migration.expect("successful migration result"),
        latest_schema_version()
    );
    assert_eq!(attachment_rows(&conn, "attachments"), vec![valid]);
    assert_eq!(
        attachment_rows(&conn, "attachment_quarantine"),
        vec![rejected]
    );
    assert_attachment_and_quarantine_claim_fences(&conn);
}

#[test]
fn attachment_role_migration_replays_tail_without_losing_quarantine() {
    let mut conn = current_schema();
    restore_legacy_attachment_schema(&conn);
    let valid = row("valid", "résumé/附件", &"a".repeat(64));
    let rejected = row("rejected", "content\0tail", &"b".repeat(64));
    insert_row(&conn, &valid).expect("legacy readable row");
    insert_row(&conn, &rejected).expect("legacy rejected-role row");
    assert_eq!(
        run_migrations(&mut conn).expect("first V47 upgrade"),
        latest_schema_version()
    );
    let live_before = attachment_rows(&conn, "attachments");
    let quarantine_before = rows(
        &conn,
        &format!("SELECT {COLUMNS}, reason FROM attachment_quarantine ORDER BY record_uuid, role"),
    );
    assert_eq!(live_before, vec![valid]);
    let mut expected_quarantine = rejected;
    expected_quarantine.push(Value::Text("invalid_role".into()));
    assert_eq!(quarantine_before, vec![expected_quarantine]);
    let schema_before = rows(
        &conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE type IN ('index', 'trigger') AND tbl_name IN ('attachments', 'attachment_quarantine') \
         ORDER BY type, name",
    );
    conn.execute("DELETE FROM _schema_migrations WHERE version >= 45", [])
        .expect("replay the same contiguous tail as the recipient migration fixture");
    assert_eq!(read_schema_version(&conn).expect("tail-replay premise"), 44);
    let migration = run_migrations(&mut conn);
    assert!(
        migration.is_ok(),
        "the migration operation must accept its existing quarantine on tail replay: {migration:?}"
    );
    assert_check_refusal(&conn, "prefix\u{85}tail", "strict-c1-after-tail-replay");
    assert_eq!(
        migration.expect("successful migration result"),
        latest_schema_version()
    );
    assert_eq!(attachment_rows(&conn, "attachments"), live_before);
    assert_eq!(
        rows(
            &conn,
            &format!(
                "SELECT {COLUMNS}, reason FROM attachment_quarantine ORDER BY record_uuid, role"
            ),
        ),
        quarantine_before,
        "tail replay must retain every quarantine column and reason"
    );
    assert_eq!(rows(
        &conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_master \
         WHERE type IN ('index', 'trigger') AND tbl_name IN ('attachments', 'attachment_quarantine') \
         ORDER BY type, name",
    ), schema_before, "tail replay must retain canonical indexes and fences");
    assert_attachment_and_quarantine_claim_fences(&conn);
}
