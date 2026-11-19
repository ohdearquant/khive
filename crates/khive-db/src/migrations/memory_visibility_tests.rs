use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value;
use rusqlite::{params, Connection};

use super::{migrate_through, table_exists};
use crate::migrations::memory_visibility::test_state::{self, Stop};
use crate::migrations::{
    finalize_attachment_cutover_for_test as finalize_attachment_cutover, migrate_outbound_due_key,
    read_schema_version, run_migrations_for_test as run_migrations,
    stage_attachment_cutover_for_test as stage_attachment_cutover,
    validate_memory_visibility_cutover, MIGRATIONS,
};

struct StopGuard;

impl StopGuard {
    fn at(point: Stop) -> Self {
        test_state::STOP.with(|stop| assert!(stop.replace(Some(point)).is_none()));
        Self
    }
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        test_state::STOP.with(|stop| stop.set(None));
    }
}

fn historical(version: u32) -> (tempfile::TempDir, PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("visibility.db");
    let mut conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    assert!(
        (21..=47).contains(&version),
        "fixture covers V21 through V47"
    );
    // A database recorded at V21 or later has completed the coordinated
    // attachment cutover, so the marker row and final schema must exist.
    migrate_through(&mut conn, 20);
    stage_attachment_cutover(&mut conn).unwrap();
    finalize_attachment_cutover(&mut conn).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| (22..=version).contains(&migration.version))
    {
        let tx = conn.transaction().unwrap();
        if migration.version == 44 {
            migrate_outbound_due_key(&tx).unwrap();
        } else {
            tx.execute_batch(migration.up).unwrap();
        }
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            params![migration.version, migration.name],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    (dir, path, conn)
}

fn memory(conn: &Connection, id: &str, namespace: &str, deleted: bool) {
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, key, name, \
         content, created_at, updated_at, deleted_at) \
         VALUES (?1, ?2, 'memory', ?1, 'name', 'original bytes', 1, 1, ?3)",
        params![id, namespace, deleted.then_some(2_i64)],
    )
    .unwrap();
}

fn receipt(conn: &Connection, id: &str, namespace: &str, expected: i64, models: &[(&str, i64)]) {
    conn.execute(
        concat!(
            "INSERT INTO memory_visibility_receipts (namespace, note_id, model_count) ",
            "VALUES (?1, ?2, ?3)"
        ),
        params![namespace, id, expected],
    )
    .unwrap();
    for (model, seq) in models {
        conn.execute(
            "INSERT INTO memory_visibility_fences (namespace, note_id, model, ann_write_log_seq) \
             VALUES (?1, ?2, ?3, ?4)",
            params![namespace, id, model, seq],
        )
        .unwrap();
    }
}

fn epochs(conn: &Connection) -> Vec<(String, String, String)> {
    conn.prepare("SELECT note_id, namespace, epoch FROM memory_visibility_epochs ORDER BY note_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn snapshot(conn: &Connection, table: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn pre_v46_inventory_and_ledger_roll_back_together() {
    let (_dir, path, mut conn) = historical(45);
    memory(&conn, "old", "alpha", false);
    let before = snapshot(&conn, "notes");
    {
        let _stop = StopGuard::at(Stop::AfterCapture);
        let error = run_migrations(&mut conn).unwrap_err();
        assert!(error.to_string().contains("AfterCapture"));
    }
    drop(conn);
    let mut conn = Connection::open(path).unwrap();
    assert_eq!(read_schema_version(&conn).unwrap(), 45);
    assert!(!table_exists(&conn, "memory_visibility_pre_v46"));
    assert!(!table_exists(&conn, "memory_visibility_receipts"));
    assert_eq!(snapshot(&conn, "notes"), before);
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        epochs(&conn),
        vec![("old".into(), "alpha".into(), "legacy".into())]
    );
}

#[test]
fn durable_pre_v46_capture_survives_restart_without_recapturing_new_notes() {
    let (_dir, path, mut conn) = historical(45);
    memory(&conn, "old-live", "alpha", false);
    memory(&conn, "old-deleted", "beta", true);
    {
        let _stop = StopGuard::at(Stop::AfterV46Commit);
        assert!(run_migrations(&mut conn)
            .unwrap_err()
            .to_string()
            .contains("AfterV46Commit"));
    }
    assert_eq!(read_schema_version(&conn).unwrap(), 46);
    drop(conn);
    let mut conn = Connection::open(&path).unwrap();
    assert_eq!(snapshot(&conn, "memory_visibility_pre_v46").len(), 2);
    memory(&conn, "later-missing", "beta", false);
    // Its age is deliberately identical to the captured population.
    let notes = snapshot(&conn, "notes");
    run_migrations(&mut conn).unwrap();
    assert_eq!(snapshot(&conn, "notes"), notes);
    let expected = vec![
        ("later-missing".into(), "beta".into(), "unknown".into()),
        ("old-deleted".into(), "beta".into(), "legacy".into()),
        ("old-live".into(), "alpha".into(), "legacy".into()),
    ];
    assert_eq!(epochs(&conn), expected);
    drop(conn);
    let mut conn = Connection::open(path).unwrap();
    run_migrations(&mut conn).unwrap();
    assert_eq!(epochs(&conn), expected);
}

#[test]
fn already_v46_receipts_classify_complete_zero_incomplete_and_missing_without_writes() {
    let (_dir, _path, mut conn) = historical(46);
    for id in [
        "complete",
        "zero",
        "partial",
        "removed",
        "surplus",
        "wrong-namespace",
    ] {
        memory(&conn, id, "alpha", false);
    }
    receipt(
        &conn,
        "complete",
        "alpha",
        2,
        &[("model-a", 17), ("model-b", 29)],
    );
    receipt(&conn, "zero", "alpha", 0, &[]);
    receipt(&conn, "partial", "alpha", 2, &[("model-a", 17)]);
    receipt(&conn, "removed", "alpha", 0, &[]);
    conn.execute(
        "DELETE FROM memory_visibility_receipts WHERE note_id = 'removed'",
        [],
    )
    .unwrap();
    receipt(&conn, "surplus", "alpha", 0, &[("model-a", 1)]);
    receipt(&conn, "wrong-namespace", "beta", 0, &[]);
    conn.execute(
        "INSERT INTO ann_write_log(namespace, embedding_model, kind, field, subject_id, op) \
         VALUES ('other', 'model-a', 'note', 'note.content', 'unrelated', 'upsert')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO vector_provenance(model_key, subject_id, namespace, embedding_digest) \
         VALUES ('model-a', 'unrelated', 'other', ?1)",
        ["a".repeat(64)],
    )
    .unwrap();
    let tables = [
        "notes",
        "memory_visibility_receipts",
        "memory_visibility_fences",
        "ann_write_log",
        "vector_provenance",
    ];
    let before: Vec<_> = tables.iter().map(|table| snapshot(&conn, table)).collect();
    run_migrations(&mut conn).unwrap();
    let after: Vec<_> = tables.iter().map(|table| snapshot(&conn, table)).collect();
    assert_eq!(after, before);
    assert_eq!(
        epochs(&conn),
        vec![
            ("complete".into(), "alpha".into(), "modern".into()),
            ("partial".into(), "alpha".into(), "unknown".into()),
            ("removed".into(), "alpha".into(), "unknown".into()),
            ("surplus".into(), "alpha".into(), "unknown".into()),
            ("wrong-namespace".into(), "alpha".into(), "unknown".into()),
            ("zero".into(), "alpha".into(), "modern".into()),
        ]
    );
    assert!(snapshot(&conn, "memory_visibility_pre_v46").is_empty());
}

#[test]
fn captured_identity_with_receipt_is_unknown_not_legacy() {
    let (_dir, _path, mut conn) = historical(45);
    memory(&conn, "contradiction", "alpha", false);
    {
        let _stop = StopGuard::at(Stop::AfterV46Commit);
        run_migrations(&mut conn).unwrap_err();
    }
    receipt(&conn, "contradiction", "alpha", 0, &[]);
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        epochs(&conn),
        vec![("contradiction".into(), "alpha".into(), "unknown".into())]
    );
}

#[test]
fn malformed_receipt_values_do_not_establish_modern() {
    let (_dir, _path, mut conn) = historical(46);
    for id in ["real-count", "real-seq", "empty-model", "zero-seq"] {
        memory(&conn, id, "alpha", false);
        receipt(&conn, id, "alpha", 1, &[("model", 1)]);
    }
    conn.execute(
        "UPDATE memory_visibility_receipts SET model_count = 1.5 WHERE note_id = 'real-count'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE memory_visibility_fences SET ann_write_log_seq = 1.5 WHERE note_id = 'real-seq'",
        [],
    )
    .unwrap();
    // Deliberate damaged-data fixtures; restore constraint checking before migration.
    conn.pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    conn.execute(
        "UPDATE memory_visibility_fences SET model = '' WHERE note_id = 'empty-model'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE memory_visibility_fences SET ann_write_log_seq = 0 WHERE note_id = 'zero-seq'",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "ignore_check_constraints", "OFF")
        .unwrap();
    run_migrations(&mut conn).unwrap();
    assert_eq!(epochs(&conn).len(), 4);
    assert!(epochs(&conn).iter().all(|row| row.2 == "unknown"));
}

#[test]
fn classifications_survive_later_receipt_loss_or_restoration_and_restart() {
    let (_dir, path, mut conn) = historical(46);
    memory(&conn, "known", "alpha", false);
    memory(&conn, "unknown", "alpha", false);
    receipt(&conn, "known", "alpha", 1, &[("model", 7)]);
    run_migrations(&mut conn).unwrap();
    let before = epochs(&conn);
    conn.execute(
        "DELETE FROM memory_visibility_fences WHERE note_id = 'known'",
        [],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM memory_visibility_receipts WHERE note_id = 'known'",
        [],
    )
    .unwrap();
    receipt(&conn, "unknown", "alpha", 0, &[]);
    drop(conn);
    let mut conn = Connection::open(path).unwrap();
    run_migrations(&mut conn).unwrap();
    assert_eq!(epochs(&conn), before);
    assert_eq!(
        before,
        vec![
            ("known".into(), "alpha".into(), "modern".into()),
            ("unknown".into(), "alpha".into(), "unknown".into()),
        ]
    );
}

#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl tracing::Subscriber for Reports {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().into(), format!("{value:?}"));
            }
        }
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        fields
            .0
            .insert("target".into(), event.metadata().target().into());
        fields
            .0
            .insert("level".into(), event.metadata().level().to_string());
        if fields.0.get("message").is_some_and(|message| {
            message.contains("memory visibility provenance cutover complete")
        }) {
            self.0.lock().unwrap().push(fields.0);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn cutover_reports_persisted_population_only_after_commit_and_once_on_resume() {
    let (_dir, path, mut conn) = historical(45);
    memory(&conn, "legacy", "alpha", false);
    {
        let _stop = StopGuard::at(Stop::AfterV46Commit);
        run_migrations(&mut conn).unwrap_err();
    }
    for (id, ns) in [
        ("modern", "alpha"),
        ("unknown-a", "alpha"),
        ("unknown-b1", "beta"),
        ("unknown-b2", "beta"),
    ] {
        memory(&conn, id, ns, false);
    }
    receipt(&conn, "modern", "alpha", 0, &[]);
    let reports = Reports::default();
    let capture = reports.clone();
    // With a single live dispatcher tracing caches a callsite's interest from
    // whichever thread registers it first, so concurrent tests without a
    // subscriber would silence this one. A second live dispatcher makes every
    // registration consult all dispatchers.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(capture, || {
        {
            let _stop = StopGuard::at(Stop::BeforeCutoverCommit);
            assert!(run_migrations(&mut conn)
                .unwrap_err()
                .to_string()
                .contains("BeforeCutoverCommit"));
        }
        assert!(
            reports.0.lock().unwrap().is_empty(),
            "no report for rolled-back classification"
        );
        assert!(!table_exists(&conn, "memory_visibility_epochs"));
        drop(conn);
        let mut conn = Connection::open(&path).unwrap();
        run_migrations(&mut conn).unwrap();
        assert_eq!(epochs(&conn).len(), 5);
        run_migrations(&mut conn).unwrap();
    });
    let reports = reports.0.lock().unwrap();
    assert_eq!(
        reports.len(),
        1,
        "a successful resumed cutover emits one operator report"
    );
    let fields = &reports[0];
    assert_eq!(fields.get("target").map(String::as_str), Some("khive.boot"));
    assert_eq!(fields.get("level").map(String::as_str), Some("INFO"));
    assert_eq!(fields.get("legacy").map(String::as_str), Some("1"));
    assert_eq!(fields.get("modern").map(String::as_str), Some("1"));
    assert_eq!(fields.get("unknown").map(String::as_str), Some("3"));
    assert_eq!(
        fields.get("unknown_by_namespace").map(String::as_str),
        Some("{\"alpha\": 1, \"beta\": 2}")
    );
    assert!(fields["database"].contains(path.file_name().unwrap().to_str().unwrap()));
}

#[test]
fn readiness_rejects_old_or_missing_schema_but_not_an_individually_missing_marker() {
    let (_dir, path, mut conn) = historical(46);
    memory(&conn, "unclassified", "alpha", false);
    assert!(validate_memory_visibility_cutover(&conn).is_err());
    let old_read_only =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(validate_memory_visibility_cutover(&old_read_only).is_err());
    drop(old_read_only);
    run_migrations(&mut conn).unwrap();
    validate_memory_visibility_cutover(&conn).unwrap();
    conn.execute(
        "DELETE FROM memory_visibility_epochs WHERE note_id = 'unclassified'",
        [],
    )
    .unwrap();
    validate_memory_visibility_cutover(&conn).unwrap();
    drop(conn);
    let ro =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    validate_memory_visibility_cutover(&ro).unwrap();
    drop(ro);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE memory_visibility_epochs")
        .unwrap();
    assert!(validate_memory_visibility_cutover(&conn).is_err());
}

#[cfg(feature = "vectors")]
#[test]
fn cutover_preserves_actual_vec0_rows_and_original_ann_log_values() {
    crate::extension::ensure_extensions_loaded();
    let (_dir, _path, mut conn) = historical(46);
    memory(&conn, "vector-note", "alpha", false);
    conn.execute_batch(
        "CREATE VIRTUAL TABLE vec_visibility_upgrade USING vec0(\
           subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,\
           field TEXT NOT NULL, embedding_model TEXT NOT NULL,\
           embedding float[2] distance_metric=cosine); \
         INSERT INTO vec_visibility_upgrade(subject_id, namespace, kind, field, \
           embedding_model, embedding) \
         VALUES ('vector-note', 'alpha', 'note', 'note.content', 'model', '[1.0,0.0]'); \
         INSERT INTO ann_write_log(namespace, embedding_model, kind, field, subject_id, op) \
         VALUES ('alpha', 'model', 'note', 'note.content', 'vector-note', 'upsert')",
    )
    .unwrap();
    let seq = conn.last_insert_rowid();
    receipt(&conn, "vector-note", "alpha", 1, &[("model", seq)]);
    let vector = |conn: &Connection| -> (String, String, String) {
        conn.query_row(
            "SELECT subject_id, namespace, vec_to_json(embedding) FROM vec_visibility_upgrade",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    };
    let before = (
        vector(&conn),
        snapshot(&conn, "ann_write_log"),
        snapshot(&conn, "memory_visibility_fences"),
    );
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        (
            vector(&conn),
            snapshot(&conn, "ann_write_log"),
            snapshot(&conn, "memory_visibility_fences")
        ),
        before
    );
    assert_eq!(
        epochs(&conn),
        vec![("vector-note".into(), "alpha".into(), "modern".into())]
    );
}

#[test]
fn existing_independent_modern_and_unknown_markers_are_not_reinferred_from_receipts() {
    let (_dir, _path, mut conn) = historical(46);
    memory(&conn, "modern-missing", "alpha", false);
    memory(&conn, "unknown-restored", "alpha", false);
    memory(&conn, "invalid-epoch", "alpha", false);
    // Stage real cutover schema without its ledger to model durable provenance
    // presented to an unapplied migration, then give only one identity modern proof.
    conn.execute_batch(crate::migrations::V54_UP).unwrap();
    conn.execute(
        "UPDATE memory_visibility_epochs SET epoch = 'modern' WHERE note_id = 'modern-missing'",
        [],
    )
    .unwrap();
    receipt(&conn, "unknown-restored", "alpha", 0, &[]);
    receipt(&conn, "invalid-epoch", "alpha", 0, &[]);
    conn.pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    conn.execute(
        "UPDATE memory_visibility_epochs SET epoch = 'invalid' WHERE note_id = 'invalid-epoch'",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "ignore_check_constraints", "OFF")
        .unwrap();
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        epochs(&conn),
        vec![
            ("invalid-epoch".into(), "alpha".into(), "unknown".into()),
            ("modern-missing".into(), "alpha".into(), "modern".into()),
            ("unknown-restored".into(), "alpha".into(), "unknown".into()),
        ]
    );
}

#[test]
fn duplicate_model_fences_cannot_satisfy_an_expected_count() {
    let (_dir, _path, mut conn) = historical(46);
    memory(&conn, "duplicate", "alpha", false);
    receipt(&conn, "duplicate", "alpha", 2, &[]);
    // Damage the uniqueness constraint to exercise value validation independently
    // of the normal primary key: two rows must not count as two different models.
    conn.execute_batch(
        "DROP TABLE memory_visibility_fences; \
         CREATE TABLE memory_visibility_fences(\
           namespace TEXT, note_id TEXT, model TEXT, ann_write_log_seq INTEGER); \
         INSERT INTO memory_visibility_fences VALUES \
           ('alpha', 'duplicate', 'same-model', 7), ('alpha', 'duplicate', 'same-model', 8)",
    )
    .unwrap();
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        epochs(&conn),
        vec![("duplicate".into(), "alpha".into(), "unknown".into())]
    );
}

#[test]
fn unreadable_provenance_is_a_driver_error_and_never_persists_unknown() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (_dir, _path, mut conn) = historical(46);
    memory(&conn, "known", "alpha", false);
    receipt(&conn, "known", "alpha", 0, &[]);
    run_migrations(&mut conn).unwrap();
    validate_memory_visibility_cutover(&conn).unwrap();
    let before = epochs(&conn);
    let denied = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&denied);
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(
            context.action,
            AuthAction::Read {
                table_name: "memory_visibility_epochs",
                ..
            }
        ) {
            observed.fetch_add(1, Ordering::SeqCst);
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))
    .unwrap();
    let error = validate_memory_visibility_cutover(&conn).unwrap_err();
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert!(matches!(error, crate::SqliteError::Rusqlite(_)));
    assert!(
        denied.load(Ordering::SeqCst) > 0,
        "the actual provenance read must be denied"
    );
    assert_eq!(epochs(&conn), before);
    assert_eq!(before[0].2, "modern");
    validate_memory_visibility_cutover(&conn).unwrap();
}

#[test]
fn orphan_and_foreign_fences_are_contradictory_even_without_a_matching_header() {
    let (_dir, _path, mut conn) = historical(45);
    memory(&conn, "captured-orphan", "alpha", false);
    {
        let _stop = StopGuard::at(Stop::AfterV46Commit);
        run_migrations(&mut conn).unwrap_err();
    }
    memory(&conn, "zero-foreign", "alpha", false);
    receipt(&conn, "zero-foreign", "alpha", 0, &[]);
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    assert_eq!(
        conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    conn.execute_batch(
        "INSERT INTO memory_visibility_fences(namespace, note_id, model, ann_write_log_seq) VALUES \
         ('alpha', 'captured-orphan', 'model', 7), \
         ('beta', 'zero-foreign', 'model', 8)",
    ).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let before = snapshot(&conn, "memory_visibility_fences");
    run_migrations(&mut conn).unwrap();
    assert_eq!(
        epochs(&conn),
        vec![
            ("captured-orphan".into(), "alpha".into(), "unknown".into()),
            ("zero-foreign".into(), "alpha".into(), "unknown".into()),
        ]
    );
    assert_eq!(snapshot(&conn, "memory_visibility_fences"), before);
}
