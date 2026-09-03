use super::query_embedding_models_conn;
use super::*;

fn open_memory() -> Connection {
    Connection::open_in_memory().expect("in-memory connection")
}

/// SQLite fixes the text encoding when the database is first initialized;
/// creating (and dropping) a table under the pragma leaves an initialized
/// UTF-16LE database that every later statement on this connection inherits.
fn open_memory_utf16le() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory connection");
    conn.execute_batch(
        "PRAGMA encoding = 'UTF-16le'; \
         CREATE TABLE __encoding_pin (x INTEGER); \
         DROP TABLE __encoding_pin;",
    )
    .expect("pin utf-16le encoding");
    conn
}

/// `length()` and `GLOB` both stop scanning at an embedded NUL, so 64 hex
/// characters followed by a NUL and trailing bytes passes the character-only
/// arms while still being 66+ bytes on the wire.
fn nul_embedded_canonical_ref() -> String {
    let mut polluted = "a".repeat(64);
    polluted.push('\0');
    polluted.push_str("zz");
    polluted
}

fn migrate_through(conn: &mut Connection, through_version: u32) {
    conn.execute_batch(MIGRATION_TRACKING_TABLE)
        .expect("create migration ledger");
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= through_version)
    {
        let tx = conn.transaction().expect("begin historical migration");
        tx.execute_batch(migration.up)
            .expect("apply historical migration body");
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![migration.version, migration.name],
        )
        .expect("record historical migration");
        tx.commit().expect("commit historical migration");
    }
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn insert_dependency_test_note(
    conn: &Connection,
    id: &str,
    kind: &str,
    properties: &str,
    deleted_at: Option<i64>,
) {
    conn.execute(
        "INSERT INTO notes \
         (id, namespace, kind, status, name, content, properties, created_at, updated_at, deleted_at) \
         VALUES (?1, 'local', ?2, 'active', ?1, '', ?3, 1, 1, ?4)",
        rusqlite::params![id, kind, properties, deleted_at],
    )
    .expect("insert dependency-test note");
}

fn insert_dependency_test_edge(
    conn: &Connection,
    id: &str,
    source_id: &str,
    target_id: &str,
    relation: &str,
    deleted_at: Option<i64>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, weight, created_at, updated_at, deleted_at) \
         VALUES ('local', ?1, ?2, ?3, ?4, 1.0, 1, 1, ?5)",
        rusqlite::params![id, source_id, target_id, relation, deleted_at],
    )
}

#[test]
fn read_only_schema_validation_requires_exact_current_version_without_writes() {
    let mut current = open_memory();
    let latest = run_migrations(&mut current).expect("migrate current schema");
    let changes_before = current.total_changes();
    assert_eq!(
        validate_schema_is_current(&current).expect("current schema validates"),
        latest
    );
    assert_eq!(
        current.total_changes(),
        changes_before,
        "compatibility validation must perform no writes"
    );

    let mut wrong_name = open_memory();
    let wrong_name_latest = run_migrations(&mut wrong_name).expect("migrate name-check schema");
    wrong_name
        .execute(
            "UPDATE _schema_migrations SET name = 'foreign_current_schema' WHERE version = ?1",
            [wrong_name_latest],
        )
        .expect("inject a foreign current-version ledger name");
    let wrong_name_error = validate_schema_is_current(&wrong_name)
        .expect_err("numeric equality must not hide a foreign migration history");
    assert!(
        wrong_name_error
            .to_string()
            .contains("migration history does not match"),
        "name-divergence diagnostic must match writable boot: {wrong_name_error}"
    );

    let mut missing_middle = open_memory();
    run_migrations(&mut missing_middle).expect("migrate missing-row schema");
    missing_middle
        .execute("DELETE FROM _schema_migrations WHERE version = 7", [])
        .expect("remove one canonical ledger row below the maximum");
    let missing_error = validate_schema_is_current(&missing_middle)
        .expect_err("MAX(version) equality must not hide a missing migration row");
    assert!(
        missing_error.to_string().contains("missing version 7"),
        "missing-row diagnostic must name the exact gap: {missing_error}"
    );

    let mut unknown_version = open_memory();
    run_migrations(&mut unknown_version).expect("migrate unknown-row schema");
    unknown_version
        .execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) \
             VALUES (0, 'foreign_baseline', 0)",
            [],
        )
        .expect("inject a non-canonical version below the maximum");
    let unknown_error = validate_schema_is_current(&unknown_version)
        .expect_err("MAX(version) equality must not hide an unknown ledger version");
    assert!(
        unknown_error.to_string().contains("unknown version 0"),
        "unknown-version diagnostic must name the foreign row: {unknown_error}"
    );

    let behind = open_memory();
    let behind_error = validate_schema_is_current(&behind)
        .expect_err("an un-migrated read-only snapshot must be rejected");
    assert!(
        behind_error.to_string().contains("migrate a writable copy"),
        "behind-version diagnostic must be actionable: {behind_error}"
    );

    current
        .execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, 'future', 0)",
            [latest + 1],
        )
        .expect("inject future schema ledger row");
    let ahead_error = validate_schema_is_current(&current)
        .expect_err("a snapshot newer than this build must be rejected");
    assert!(
        ahead_error.to_string().contains("compatible newer build"),
        "ahead-version diagnostic must be actionable: {ahead_error}"
    );
}

#[test]
fn writable_upgrade_rejects_noncanonical_ledger_before_applying_next_migration() {
    let mut missing_middle = open_memory();
    run_migrations(&mut missing_middle).expect("migrate missing-row upgrade source");
    missing_middle
        .execute(
            "DELETE FROM _schema_migrations WHERE version IN (7, 19)",
            [],
        )
        .expect("simulate a pre-V19 ledger with a hidden middle gap");
    let missing_error = run_migrations(&mut missing_middle)
        .expect_err("writable migration must reject a missing row before applying V19");
    assert!(
        missing_error.to_string().contains("missing version 7"),
        "writable missing-row diagnostic must name the exact gap: {missing_error}"
    );
    assert_eq!(
        missing_middle
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 19",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "ledger validation must fail before V19 is recorded"
    );

    let mut unknown_version = open_memory();
    run_migrations(&mut unknown_version).expect("migrate foreign-row upgrade source");
    unknown_version
        .execute("DELETE FROM _schema_migrations WHERE version = 19", [])
        .expect("simulate a pre-V19 ledger");
    unknown_version
        .execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) \
             VALUES (0, 'foreign_baseline', 0)",
            [],
        )
        .expect("inject a non-canonical version below the maximum");
    let unknown_error = run_migrations(&mut unknown_version)
        .expect_err("writable migration must reject a foreign row before applying V19");
    assert!(
        unknown_error.to_string().contains("unknown version 0"),
        "writable foreign-row diagnostic must name the exact version: {unknown_error}"
    );
    assert_eq!(
        unknown_version
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 19",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "ledger validation must fail before V19 is recorded"
    );
}

#[test]
fn apply_schema_plan_rolls_back_migration_when_ledger_insert_fails() {
    static MIGRATIONS: &[Migration] = &[Migration {
        id: "001_atomic",
        up_sql: "CREATE TABLE migration_effect (id INTEGER PRIMARY KEY);",
        down_sql: None,
        is_already_applied: None,
    }];
    let plan = ServiceSchemaPlan {
        service: "atomicity_test",
        sqlite: MIGRATIONS,
        postgres: &[],
    };
    let conn = open_memory();
    conn.execute_batch(SCHEMA_VERSION_TABLE).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_schema_version
         BEFORE INSERT ON _schema_versions
         BEGIN
             SELECT RAISE(ABORT, 'injected ledger failure');
         END;",
    )
    .unwrap();

    apply_schema_plan(&conn, &plan).expect_err("ledger failure must abort the migration");

    assert!(
        !table_exists(&conn, "migration_effect"),
        "migration body must roll back when its ledger insert fails"
    );
    let ledger_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM _schema_versions WHERE service = 'atomicity_test'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_rows, 0);
}

#[test]
fn fresh_db_migrates_to_latest() {
    let mut conn = open_memory();
    let version = run_migrations(&mut conn).expect("migrations should succeed");
    let latest = MIGRATIONS.last().expect("at least one migration").version;
    assert_eq!(
        version, latest,
        "run_migrations must reach the latest version"
    );

    let recorded: i64 = conn
        .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        recorded,
        MIGRATIONS.len() as i64,
        "ledger row count must equal the number of migrations"
    );
}

#[test]
fn v16_fresh_start_installs_narrow_gtd_dependency_cycle_guards() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");

    for trigger in [
        "gtd_task_dependency_cycle_notes_bi",
        "gtd_task_dependency_cycle_notes_bu",
        "gtd_task_dependency_cycle_note_activation_bi",
        "gtd_task_dependency_cycle_note_activation_bu",
        "gtd_task_dependency_cycle_edges_bi",
        "gtd_task_dependency_cycle_edges_bu",
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
                rusqlite::params![trigger],
                |row| row.get(0),
            )
            .expect("query trigger catalog");
        assert!(exists, "V16 must install {trigger}");
    }

    insert_dependency_test_note(&conn, "task-a", "task", r#"{"status":"next"}"#, None);
    insert_dependency_test_note(&conn, "task-b", "task", r#"{"status":"next"}"#, None);
    insert_dependency_test_note(&conn, "task-c", "task", r#"{"status":"next"}"#, None);
    let insert_error = conn
        .execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, content, properties, created_at, updated_at) \
             VALUES ('task-self', 'local', 'task', 'active', '', \
                     '{\"depends_on\":[\"task-self\"]}', 1, 1)",
            [],
        )
        .expect_err("the insert trigger must reject a direct property cycle");
    assert!(
        insert_error.to_string().contains("dependency cycle"),
        "unexpected insert trigger error: {insert_error}"
    );

    conn.execute(
        r#"UPDATE notes SET properties = '{"status":"next","depends_on":["task-b"]}' WHERE id = 'task-a'"#,
        [],
    )
    .expect("first property dependency");
    conn.execute(
        r#"UPDATE notes SET properties = '{"status":"next","depends_on":["task-c"]}' WHERE id = 'task-b'"#,
        [],
    )
    .expect("second property dependency");
    let property_error = conn
        .execute(
            r#"UPDATE notes SET properties = '{"status":"next","depends_on":["task-a"]}' WHERE id = 'task-c'"#,
            [],
        )
        .expect_err("closing a property cycle must fail at write time");
    assert!(
        property_error.to_string().contains("dependency cycle"),
        "unexpected property trigger error: {property_error}"
    );

    insert_dependency_test_edge(&conn, "edge-a-b", "task-a", "task-b", "depends_on", None)
        .expect("first edge dependency");
    insert_dependency_test_edge(&conn, "edge-b-c", "task-b", "task-c", "depends_on", None)
        .expect("second edge dependency");
    let edge_error =
        insert_dependency_test_edge(&conn, "edge-c-a", "task-c", "task-a", "depends_on", None)
            .expect_err("closing an edge cycle must fail at write time");
    assert!(
        edge_error.to_string().contains("dependency cycle"),
        "unexpected edge trigger error: {edge_error}"
    );

    // Soft-deleted rows are not live reachability. Removing B -> C from the
    // live edge graph makes C -> A acyclic even though the tombstone remains.
    conn.execute(
        "UPDATE graph_edges SET deleted_at = 2 WHERE id = 'edge-b-c'",
        [],
    )
    .expect("soft-delete dependency edge");
    insert_dependency_test_edge(&conn, "edge-c-a", "task-c", "task-a", "depends_on", None)
        .expect("soft-deleted edges must not participate in reachability");
    let reactivation_error = conn
        .execute(
            "UPDATE graph_edges SET deleted_at = NULL WHERE id = 'edge-b-c'",
            [],
        )
        .expect_err("the update trigger must reject reactivating a cycle");
    assert!(
        reactivation_error.to_string().contains("dependency cycle"),
        "unexpected edge-update trigger error: {reactivation_error}"
    );

    // A soft-deleted task's old properties likewise do not contribute a live
    // path. Direct storage can still create a broken reference; GTD read-time
    // diagnostics classify it and typed public hooks reject it earlier.
    insert_dependency_test_note(
        &conn,
        "task-deleted",
        "task",
        r#"{"depends_on":["task-a"]}"#,
        Some(2),
    );
    conn.execute(
        r#"UPDATE notes SET properties = '{"depends_on":["task-deleted"]}' WHERE id = 'task-a'"#,
        [],
    )
    .expect("soft-deleted notes must not participate in reachability");

    // The migration governs only the GTD task dependency graph. Other note
    // kinds and other edge relations retain their existing write behavior.
    insert_dependency_test_note(
        &conn,
        "observation-self",
        "observation",
        r#"{"depends_on":["observation-self"]}"#,
        None,
    );
    insert_dependency_test_edge(&conn, "related-a-b", "task-a", "task-b", "related_to", None)
        .expect("unrelated edge relation must remain writable");
    insert_dependency_test_edge(&conn, "related-b-a", "task-b", "task-a", "related_to", None)
        .expect("unrelated edge cycle must remain writable");

    // Edge reachability is scoped to the edge row's namespace, even though
    // UUID endpoint resolution itself is namespace-agnostic (ADR-007). An
    // opposite direction in another graph namespace is independent, while a
    // cycle completed inside that namespace is still rejected.
    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
         VALUES ('other', 'other-b-a', 'task-b', 'task-a', 'depends_on', 1.0, 1, 1)",
        [],
    )
    .expect("opposite direction in another edge namespace remains independent");
    let other_namespace_error = conn
        .execute(
            "INSERT INTO graph_edges \
             (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
             VALUES ('other', 'other-a-b', 'task-a', 'task-b', 'depends_on', 1.0, 1, 1)",
            [],
        )
        .expect_err("a cycle inside the other edge namespace must still fail");
    assert!(
        other_namespace_error
            .to_string()
            .contains("dependency cycle"),
        "unexpected cross-namespace guard error: {other_namespace_error}"
    );
}

#[test]
fn v13_property_guard_ignores_non_array_legacy_dependencies() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    let task_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let task_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    insert_dependency_test_note(&conn, task_a, "task", r#"{"status":"next"}"#, None);
    let scalar_properties = format!(r#"{{"depends_on":"{task_a}"}}"#);
    insert_dependency_test_note(&conn, task_b, "task", &scalar_properties, None);

    let array_properties = format!(r#"{{"depends_on":["{task_b}"]}}"#);
    conn.execute(
        "UPDATE notes SET properties = ?1 WHERE id = ?2",
        rusqlite::params![array_properties, task_a],
    )
    .expect("a legacy scalar depends_on value is not a traversable dependency edge");

    let persisted: String = conn
        .query_row(
            "SELECT properties FROM notes WHERE id = ?1",
            rusqlite::params![task_a],
            |row| row.get(0),
        )
        .expect("load accepted dependency properties");
    assert!(persisted.contains(task_b));
}

#[test]
fn v13_serializes_alternate_uuid_spelling_updates_without_committing_a_cycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("gtd-cycle-race.db");
    let mut setup = Connection::open(&path).expect("open setup connection");
    run_migrations(&mut setup).expect("migrations should succeed");
    let task_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let task_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    insert_dependency_test_note(&setup, task_a, "task", r#"{"status":"next"}"#, None);
    insert_dependency_test_note(&setup, task_b, "task", r#"{"status":"next"}"#, None);
    drop(setup);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for (source, target) in [(task_a, task_b), (task_b, task_a)] {
        let path = path.clone();
        let barrier = barrier.clone();
        let target = target.to_ascii_uppercase();
        handles.push(std::thread::spawn(move || {
            let mut conn = Connection::open(path).expect("open racing connection");
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .expect("set busy timeout");
            barrier.wait();
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .expect("begin immediate");
            let properties = format!(r#"{{"status":"next","depends_on":["{target}"]}}"#);
            let result = tx.execute(
                "UPDATE notes SET properties = ?1 WHERE id = ?2",
                rusqlite::params![properties, source],
            );
            match result {
                Ok(_) => tx.commit().map(|_| ()),
                Err(error) => {
                    tx.rollback().expect("rollback rejected update");
                    Err(error)
                }
            }
        }));
    }

    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("race thread"))
        .collect();
    assert_eq!(
        outcomes.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one direction may commit: {outcomes:?}"
    );
    let rejected = outcomes
        .iter()
        .find_map(|result| result.as_ref().err())
        .expect("one direction must be rejected");
    assert!(
        rejected.to_string().contains("dependency cycle"),
        "unexpected race rejection: {rejected}"
    );

    let verify = Connection::open(path).expect("open verification connection");
    let dependency_rows: i64 = verify
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id IN (?1, ?2) \
             AND json_array_length(properties, '$.depends_on') = 1",
            rusqlite::params![task_a, task_b],
            |row| row.get(0),
        )
        .expect("count committed dependency directions");
    assert_eq!(dependency_rows, 1, "the committed graph must stay acyclic");
}

#[test]
fn v13_edge_guard_ignores_paths_through_soft_deleted_tasks() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    insert_dependency_test_note(&conn, "live-a", "task", r#"{"status":"next"}"#, None);
    insert_dependency_test_note(&conn, "deleted-b", "task", r#"{"status":"next"}"#, None);
    insert_dependency_test_note(&conn, "live-c", "task", r#"{"status":"next"}"#, None);
    insert_dependency_test_edge(
        &conn,
        "live-a-deleted-b",
        "live-a",
        "deleted-b",
        "depends_on",
        None,
    )
    .expect("first live edge");
    insert_dependency_test_edge(
        &conn,
        "deleted-b-live-c",
        "deleted-b",
        "live-c",
        "depends_on",
        None,
    )
    .expect("second live edge");
    conn.execute(
        "UPDATE notes SET status = 'deleted', deleted_at = 2 WHERE id = 'deleted-b'",
        [],
    )
    .expect("soft-delete the intermediate task without deleting its edges");

    insert_dependency_test_edge(
        &conn,
        "live-c-live-a",
        "live-c",
        "live-a",
        "depends_on",
        None,
    )
    .expect("tombstoned task edges must not form a live dependency path");
}

#[test]
fn v13_note_activation_rejects_dormant_edge_cycles() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");

    insert_dependency_test_note(&conn, "live-a", "task", "{}", None);
    insert_dependency_test_note(&conn, "deleted-b", "task", "{}", Some(2));
    insert_dependency_test_edge(
        &conn,
        "live-a-deleted-b",
        "live-a",
        "deleted-b",
        "depends_on",
        None,
    )
    .expect("edge to tombstoned task is dormant");
    insert_dependency_test_edge(
        &conn,
        "deleted-b-live-a",
        "deleted-b",
        "live-a",
        "depends_on",
        None,
    )
    .expect("edge from tombstoned task is dormant");

    let reactivation_error = conn
        .execute(
            "UPDATE notes SET deleted_at = NULL WHERE id = 'deleted-b'",
            [],
        )
        .expect_err("reactivating a task endpoint must not expose an edge cycle");
    assert!(
        reactivation_error.to_string().contains("dependency cycle"),
        "unexpected task-reactivation error: {reactivation_error}"
    );
    let remains_deleted: bool = conn
        .query_row(
            "SELECT deleted_at IS NOT NULL FROM notes WHERE id = 'deleted-b'",
            [],
            |row| row.get(0),
        )
        .expect("load rejected task reactivation");
    assert!(remains_deleted, "the rejected reactivation must roll back");

    insert_dependency_test_note(&conn, "live-c", "task", "{}", None);
    insert_dependency_test_note(&conn, "observation-d", "observation", "{}", None);
    insert_dependency_test_edge(
        &conn,
        "live-c-observation-d",
        "live-c",
        "observation-d",
        "depends_on",
        None,
    )
    .expect("edge to non-task note is dormant");
    insert_dependency_test_edge(
        &conn,
        "observation-d-live-c",
        "observation-d",
        "live-c",
        "depends_on",
        None,
    )
    .expect("edge from non-task note is dormant");

    let conversion_error = conn
        .execute(
            "UPDATE notes SET kind = 'task' WHERE id = 'observation-d'",
            [],
        )
        .expect_err("converting a note to a task must not expose an edge cycle");
    assert!(
        conversion_error.to_string().contains("dependency cycle"),
        "unexpected task-conversion error: {conversion_error}"
    );
    let persisted_kind: String = conn
        .query_row(
            "SELECT kind FROM notes WHERE id = 'observation-d'",
            [],
            |row| row.get(0),
        )
        .expect("load rejected task conversion");
    assert_eq!(persisted_kind, "observation");
}

#[test]
fn v13_note_insert_rejects_dormant_edge_cycles() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");

    insert_dependency_test_note(&conn, "live-a", "task", "{}", None);
    insert_dependency_test_edge(
        &conn,
        "live-a-missing-b",
        "live-a",
        "missing-b",
        "depends_on",
        None,
    )
    .expect("edge to a missing task is dormant");
    insert_dependency_test_edge(
        &conn,
        "missing-b-live-a",
        "missing-b",
        "live-a",
        "depends_on",
        None,
    )
    .expect("edge from a missing task is dormant");

    let insert_error = conn
        .execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, name, content, properties, created_at, updated_at) \
             VALUES ('missing-b', 'local', 'task', 'active', 'missing-b', '', '{}', 1, 1)",
            [],
        )
        .expect_err("inserting a task endpoint must not expose an edge cycle");
    assert!(
        insert_error.to_string().contains("dependency cycle"),
        "unexpected task-insert error: {insert_error}"
    );
    let rejected_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id = 'missing-b'",
            [],
            |row| row.get(0),
        )
        .expect("count rejected task insert");
    assert_eq!(rejected_rows, 0, "the rejected insert must roll back");

    insert_dependency_test_edge(
        &conn,
        "missing-c-live-a",
        "missing-c",
        "live-a",
        "depends_on",
        None,
    )
    .expect("acyclic edge from a missing task is dormant");
    insert_dependency_test_note(&conn, "missing-c", "task", "{}", None);
}

#[test]
fn v13_property_id_replacement_uses_the_post_update_graph() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");

    let old_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let middle_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let new_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let old_properties = format!(r#"{{"depends_on":["{new_id}"]}}"#);
    let middle_properties = format!(r#"{{"depends_on":["{old_id}"]}}"#);
    insert_dependency_test_note(&conn, old_id, "task", &old_properties, None);
    insert_dependency_test_note(&conn, middle_id, "task", &middle_properties, None);

    let replacement_properties = format!(r#"{{"depends_on":["{middle_id}"]}}"#);
    conn.execute(
        "UPDATE notes SET id = ?1, properties = ?2 WHERE id = ?3",
        rusqlite::params![new_id, replacement_properties, old_id],
    )
    .expect("the disappearing old endpoint must not create a phantom property cycle");
    let replacement_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1",
            rusqlite::params![new_id],
            |row| row.get(0),
        )
        .expect("count replacement task");
    assert_eq!(replacement_rows, 1);
    let old_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1",
            rusqlite::params![old_id],
            |row| row.get(0),
        )
        .expect("count disappearing old task");
    assert_eq!(old_rows, 0);

    let cycle_old_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let cycle_middle_id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let cycle_new_id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    insert_dependency_test_note(&conn, cycle_old_id, "task", "{}", None);
    let cycle_middle_properties = format!(r#"{{"depends_on":["{cycle_new_id}"]}}"#);
    insert_dependency_test_note(
        &conn,
        cycle_middle_id,
        "task",
        &cycle_middle_properties,
        None,
    );

    let cycle_replacement_properties = format!(r#"{{"depends_on":["{cycle_middle_id}"]}}"#);
    let cycle_error = conn
        .execute(
            "UPDATE notes SET id = ?1, properties = ?2 WHERE id = ?3",
            rusqlite::params![cycle_new_id, cycle_replacement_properties, cycle_old_id],
        )
        .expect_err("a real property cycle through the replacement id must still fail");
    assert!(
        cycle_error.to_string().contains("dependency cycle"),
        "unexpected replacement-cycle error: {cycle_error}"
    );
    let preserved_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1",
            rusqlite::params![cycle_old_id],
            |row| row.get(0),
        )
        .expect("count preserved old task");
    assert_eq!(preserved_rows, 1, "the rejected replacement must roll back");
    let rejected_new_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE id = ?1",
            rusqlite::params![cycle_new_id],
            |row| row.get(0),
        )
        .expect("count rejected replacement id");
    assert_eq!(rejected_new_rows, 0);
}

#[test]
fn v13_note_activation_preserves_acyclic_and_unrelated_edges() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");

    insert_dependency_test_note(&conn, "live-a", "task", "{}", None);
    insert_dependency_test_note(&conn, "deleted-b", "task", "{}", Some(2));
    insert_dependency_test_edge(
        &conn,
        "deleted-b-live-a",
        "deleted-b",
        "live-a",
        "depends_on",
        None,
    )
    .expect("acyclic edge from tombstoned task is dormant");
    conn.execute(
        "UPDATE notes SET deleted_at = NULL WHERE id = 'deleted-b'",
        [],
    )
    .expect("acyclic task reactivation must remain allowed");

    insert_dependency_test_note(&conn, "observation-c", "observation", "{}", None);
    insert_dependency_test_edge(
        &conn,
        "live-a-observation-c",
        "live-a",
        "observation-c",
        "depends_on",
        None,
    )
    .expect("acyclic edge to non-task note is dormant");
    conn.execute(
        "UPDATE notes SET kind = 'task' WHERE id = 'observation-c'",
        [],
    )
    .expect("acyclic task conversion must remain allowed");

    insert_dependency_test_note(&conn, "deleted-d", "task", "{}", Some(2));
    insert_dependency_test_edge(
        &conn,
        "live-a-deleted-d-related",
        "live-a",
        "deleted-d",
        "related_to",
        None,
    )
    .expect("first unrelated edge");
    insert_dependency_test_edge(
        &conn,
        "deleted-d-live-a-related",
        "deleted-d",
        "live-a",
        "related_to",
        None,
    )
    .expect("second unrelated edge");
    conn.execute(
        "UPDATE notes SET deleted_at = NULL WHERE id = 'deleted-d'",
        [],
    )
    .expect("unrelated edge cycles must not block task reactivation");
}

#[test]
fn v4_creates_consolidated_fts_tables() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    assert!(
        table_exists(&conn, "fts_entities"),
        "V4 must create fts_entities"
    );
    assert!(table_exists(&conn, "fts_notes"), "V4 must create fts_notes");
}

#[test]
fn rejects_ledger_ahead_of_binary_latest() {
    let mut conn = open_memory();
    // Simulate a database whose recorded version is ahead of everything this
    // build knows — a newer build's ledger (historically, the
    // pre-consolidation shape). Computed from the live chain so the fixture
    // stays ahead when a real migration lands on the number a literal would
    // have pinned.
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    conn.execute(
        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, 'legacy', 0)",
        rusqlite::params![latest_schema_version() + 1],
    )
    .unwrap();

    let err = run_migrations(&mut conn).expect_err("must reject a version ahead of latest");
    match err {
        SqliteError::InvalidData(msg) => assert!(
            msg.contains("ahead of the latest known migration"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected InvalidData, got {other:?}"),
    }
}

#[test]
fn core_tables_exist() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    for t in [
        "entities",
        "entities_seq",
        "graph_edges",
        "graph_edges_seq",
        "notes",
        "notes_seq",
        "events",
        "event_observations",
        "_embedding_models",
        "proposals_open",
        "brain_profile_snapshots",
        "brain_event_log",
        "knowledge_atoms",
        "knowledge_domains",
        "knowledge_sections",
        "ann_consumer_pending",
    ] {
        assert!(table_exists(&conn, t), "missing table: {t}");
    }
}

#[test]
fn knowledge_atoms_has_content_not_description() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    assert!(
        column_exists(&conn, "knowledge_atoms", "content"),
        "knowledge_atoms must have a content column"
    );
    assert!(
        !column_exists(&conn, "knowledge_atoms", "description"),
        "knowledge_atoms must NOT have a description column"
    );
}

#[test]
fn knowledge_sections_has_content_hash() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    assert!(column_exists(&conn, "knowledge_sections", "content_hash"));
}

#[test]
fn knowledge_sections_unique_on_atom_and_content_hash() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    let now = chrono::Utc::now().timestamp_micros();
    conn.execute(
        "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, created_at, updated_at) \
         VALUES ('a1', 'default', 'slug-1', 'Atom', 'body text here', ?1, ?1)",
        rusqlite::params![now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO knowledge_sections (id, atom_id, namespace, section_type, content, content_hash, created_at, updated_at) \
         VALUES ('s1', 'a1', 'default', 'other', 'X', 'hash-abc', ?1, ?1)",
        rusqlite::params![now],
    )
    .unwrap();
    // Same (atom_id, content_hash) must be rejected.
    let dup = conn.execute(
        "INSERT INTO knowledge_sections (id, atom_id, namespace, section_type, content, content_hash, created_at, updated_at) \
         VALUES ('s2', 'a1', 'default', 'overview', 'Y', 'hash-abc', ?1, ?1)",
        rusqlite::params![now],
    );
    assert!(dup.is_err(), "duplicate (atom_id, content_hash) must fail");
}

#[test]
fn run_migrations_twice_is_idempotent() {
    let mut conn = open_memory();
    let v1 = run_migrations(&mut conn).expect("first run");
    let v2 = run_migrations(&mut conn).expect("second run");
    assert_eq!(v1, v2);
    let recorded: i64 = conn
        .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        recorded,
        MIGRATIONS.len() as i64,
        "no duplicate migration rows on re-run"
    );
}

#[test]
fn v22_upgrades_pre_index_database_for_read_only_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-unread-probe-index.db");

    {
        let mut conn = Connection::open(&path).expect("create pre-index database");
        migrate_through(&mut conn, 20);
        stage_attachment_cutover(&mut conn).expect("stage empty attachment cutover");
        finalize_attachment_cutover(&mut conn).expect("finalize empty attachment cutover");
        assert_eq!(read_schema_version(&conn).expect("read V21 ledger"), 21);
        conn.execute("DROP INDEX idx_notes_unread_probe_recipient", [])
            .expect("simulate pre-index V21 database");
        assert!(!index_exists(&conn, "idx_notes_unread_probe_recipient"));
    }

    {
        let mut conn = Connection::open(&path).expect("reopen writable database");
        assert_eq!(
            run_migrations(&mut conn).expect("apply V22 unread probe migration"),
            latest_schema_version()
        );
        assert!(index_exists(&conn, "idx_notes_unread_probe_recipient"));
    }

    let read_only = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open migrated database read-only");
    assert!(index_exists(&read_only, "idx_notes_unread_probe_recipient"));
    validate_schema_is_current(&read_only).expect("migrated read-only schema validates");
}

#[test]
fn v23_backfills_indexed_record_kinds_and_preserves_unmatched_fts_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pre-record-kind.db");

    {
        let mut conn = Connection::open(&path).expect("create pre-V23 database");
        migrate_through(&mut conn, 20);
        stage_attachment_cutover(&mut conn).expect("stage empty attachment cutover");
        finalize_attachment_cutover(&mut conn).expect("finalize empty attachment cutover");

        let v22 = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 22)
            .expect("V22 migration");
        let tx = conn.transaction().expect("begin V22 migration");
        tx.execute_batch(v22.up).expect("apply V22 migration");
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![v22.version, v22.name],
        )
        .expect("record V22 migration");
        tx.commit().expect("commit V22 migration");
        assert_eq!(read_schema_version(&conn).expect("read V22 ledger"), 22);
        assert!(!column_exists(&conn, "fts_notes", "record_kind"));

        conn.execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, content, created_at, updated_at) \
             VALUES ('memory-id', 'local', 'memory', 'active', 'common recall token', 1, 1)",
            [],
        )
        .expect("insert memory note");
        conn.execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, content, created_at, updated_at) \
             VALUES ('message-id', 'local', 'message', 'active', 'common recall token', 1, 1)",
            [],
        )
        .expect("insert message note");
        conn.execute(
            "INSERT INTO notes \
             (id, namespace, kind, status, content, created_at, updated_at, deleted_at) \
             VALUES ('deleted-memory-id', 'local', 'memory', 'deleted', \
                     'common recall token', 1, 2, 2)",
            [],
        )
        .expect("insert soft-deleted memory note");
        conn.execute(
            "INSERT INTO entities \
             (id, namespace, kind, name, tags, created_at, updated_at) \
             VALUES ('concept-id', 'local', 'concept', 'Common concept', '[]', 1, 1)",
            [],
        )
        .expect("insert concept entity");

        let insert_legacy_fts = |table: &str, id: &str, body: &str| {
            conn.execute(
                &format!(
                    "INSERT INTO {table} \
                     (subject_id, kind, title, body, tags, namespace, metadata, updated_at) \
                     VALUES (?1, ?2, '', ?3, '[]', 'local', NULL, 1)"
                ),
                rusqlite::params![
                    id,
                    if table == "fts_notes" {
                        "note"
                    } else {
                        "entity"
                    },
                    body
                ],
            )
            .expect("insert legacy FTS row");
        };
        insert_legacy_fts("fts_notes", "memory-id", "common recall token");
        insert_legacy_fts("fts_notes", "message-id", "common recall token");
        insert_legacy_fts("fts_notes", "deleted-memory-id", "common recall token");
        insert_legacy_fts("fts_notes", "stale-id", "common recall token");
        insert_legacy_fts("fts_entities", "concept-id", "common concept token");

        // This test is specifically about V23's own preservation behavior
        // (every prior FTS row survives, classified or not), so it stops at
        // V23 explicitly rather than running the full chain: V24 introduces
        // its own orphan sweep that would remove `stale-id` (see
        // `v24_rowid_map_backfills_dedups_and_sweeps_orphans` below), which
        // would make this test's "preserves every row" assertion false for a
        // reason that has nothing to do with V23.
        let v23 = MIGRATIONS
            .iter()
            .find(|migration| migration.version == 23)
            .expect("V23 migration");
        let tx = conn.transaction().expect("begin V23 migration");
        tx.execute_batch(v23.up).expect("apply V23 migration");
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![v23.version, v23.name],
        )
        .expect("record V23 migration");
        tx.commit().expect("commit V23 migration");
        assert_eq!(read_schema_version(&conn).expect("read V23 ledger"), 23);
        assert!(column_exists(&conn, "fts_notes", "record_kind"));
        assert!(column_exists(&conn, "fts_entities", "record_kind"));

        let table_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'fts_notes'",
                [],
                |row| row.get(0),
            )
            .expect("read rebuilt FTS DDL");
        assert!(table_sql.contains("record_kind"), "{table_sql}");
        assert!(
            !table_sql.contains("record_kind UNINDEXED"),
            "record_kind must participate in the FTS index: {table_sql}"
        );

        let rows = conn
            .prepare(
                "SELECT subject_id, record_kind FROM fts_notes \
                 ORDER BY subject_id",
            )
            .expect("prepare backfill read")
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query backfilled rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect backfilled rows");
        assert_eq!(
            rows,
            vec![
                ("deleted-memory-id".to_string(), String::new()),
                ("memory-id".to_string(), "memory".to_string()),
                ("message-id".to_string(), "message".to_string()),
                ("stale-id".to_string(), String::new()),
            ],
            "V23 must preserve every prior FTS row while classifying live records"
        );
        let memory_matches: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_notes \
                 WHERE fts_notes MATCH 'record_kind : \"memory\" AND common'",
                [],
                |row| row.get(0),
            )
            .expect("query indexed memory classifier");
        assert_eq!(memory_matches, 1);
        let entity_kind: String = conn
            .query_row(
                "SELECT record_kind FROM fts_entities WHERE subject_id = 'concept-id'",
                [],
                |row| row.get(0),
            )
            .expect("read entity classifier");
        assert_eq!(entity_kind, "concept");
    }

    // Deliberately not `validate_schema_is_current`: this fixture stops at
    // V23 on purpose (see the comment above the V23 application), so V24 is
    // still pending.
    let read_only = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open V23 database read-only");
    assert_eq!(
        read_schema_version(&read_only).expect("read V23 ledger read-only"),
        23
    );
}

/// V24: the sidecar rowid map must end up covering every surviving FTS row
/// exactly once, legacy duplicates must collapse to the highest (most
/// recent) rowid, orphaned rows (no backing `notes`/`entities` record at all)
/// must be gone, and a soft-deleted-but-still-present note's FTS row must
/// survive untouched (soft delete is a view-layer filter, not a data-layer
/// deletion — see `crates/khive-db/sql/024-fts-rowid-map.sql`'s header).
#[test]
fn v24_rowid_map_backfills_dedups_and_sweeps_orphans() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 23);

    conn.execute(
        "INSERT INTO notes \
         (id, namespace, kind, status, content, created_at, updated_at) \
         VALUES ('memory-id', 'local', 'memory', 'active', 'common recall token', 1, 1)",
        [],
    )
    .expect("insert live memory note");
    conn.execute(
        "INSERT INTO notes \
         (id, namespace, kind, status, content, created_at, updated_at, deleted_at) \
         VALUES ('deleted-memory-id', 'local', 'memory', 'deleted', \
                 'common recall token', 1, 2, 2)",
        [],
    )
    .expect("insert soft-deleted memory note (still present in `notes`)");
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at) \
         VALUES ('concept-id', 'local', 'concept', 'Common concept', '[]', 1, 1)",
        [],
    )
    .expect("insert live concept entity");

    // `memory-id` has TWO legacy FTS rows at different rowids — a pre-atomic
    // upsert race that left both a stale and a fresh copy behind. Rowid 200
    // is the later (correct) one and must be the survivor.
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (100, 'memory-id', 'note', '', 'stale duplicate body', '[]', 'local', NULL, 1, 'memory')",
        [],
    )
    .expect("insert stale duplicate fts_notes row");
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (200, 'memory-id', 'note', '', 'fresh body', '[]', 'local', NULL, 2, 'memory')",
        [],
    )
    .expect("insert fresh fts_notes row for the same subject");
    // A soft-deleted note's FTS row: still backed by a real `notes` row, so
    // the sweep must NOT remove it.
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (300, 'deleted-memory-id', 'note', '', 'soft deleted body', '[]', 'local', NULL, 2, '')",
        [],
    )
    .expect("insert fts_notes row for soft-deleted note");
    // A pure orphan: no `notes` row named `stale-id` exists at all.
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (400, 'stale-id', 'note', '', 'orphan body', '[]', 'local', NULL, 2, '')",
        [],
    )
    .expect("insert orphaned fts_notes row");
    conn.execute(
        "INSERT INTO fts_entities \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (500, 'concept-id', 'entity', 'Common concept', 'Common concept', '[]', 'local', NULL, 1, 'concept')",
        [],
    )
    .expect("insert live fts_entities row");
    // A pure orphan on the entity side too.
    conn.execute(
        "INSERT INTO fts_entities \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (600, 'stale-entity-id', 'entity', 'Gone', 'Gone', '[]', 'local', NULL, 1, 'concept')",
        [],
    )
    .expect("insert orphaned fts_entities row");

    assert_eq!(
        run_migrations(&mut conn).expect("apply V24 rowid-map migration"),
        24
    );

    // -- fts_notes: duplicates collapsed, orphan gone, live rows kept. --
    let mut note_rows: Vec<(i64, String)> = conn
        .prepare("SELECT rowid, subject_id FROM fts_notes ORDER BY rowid")
        .expect("prepare fts_notes read")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query fts_notes rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect fts_notes rows");
    note_rows.sort();
    assert_eq!(
        note_rows,
        vec![
            (200, "memory-id".to_string()),
            (300, "deleted-memory-id".to_string()),
        ],
        "rowid 100 (stale duplicate) and stale-id (pure orphan) must both be swept; \
         the soft-deleted-but-still-present note's row must survive"
    );

    // -- fts_entities: orphan gone, live row kept. --
    let entity_rows: Vec<String> = conn
        .prepare("SELECT subject_id FROM fts_entities ORDER BY subject_id")
        .expect("prepare fts_entities read")
        .query_map([], |row| row.get(0))
        .expect("query fts_entities rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect fts_entities rows");
    assert_eq!(entity_rows, vec!["concept-id".to_string()]);

    // -- The map covers every surviving row exactly once, at its own rowid. --
    for (table, map) in [
        ("fts_notes", "fts_notes_rowids"),
        ("fts_entities", "fts_entities_rowids"),
    ] {
        let fts_count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count fts rows");
        let map_count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
            .expect("count map rows");
        assert_eq!(fts_count, map_count, "{table}/{map} row-count parity");

        let mismatched: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table} AS t \
                     LEFT JOIN {map} AS m ON m.rowid = t.rowid \
                     WHERE m.rowid IS NULL"
                ),
                [],
                |row| row.get(0),
            )
            .expect("count fts rows missing a map entry");
        assert_eq!(mismatched, 0, "every {table} row must have a {map} entry");
    }

    // The map's own row for `memory-id` must point at the surviving rowid.
    let mapped_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM fts_notes_rowids WHERE namespace = 'local' AND subject_id = 'memory-id'",
            [],
            |row| row.get(0),
        )
        .expect("read memory-id map entry");
    assert_eq!(mapped_rowid, 200);
}

/// A legacy FTS row with NULL `namespace`/
/// `subject_id` (permitted — both are UNINDEXED FTS5 columns) cannot be
/// attributed to a (namespace, subject_id) key at all. It must survive V24
/// completely unmapped: neither backfilled into the map (impossible — the
/// map's columns are NOT NULL) nor swept by either sweep as if it were an
/// unmapped duplicate or an orphan.
#[test]
fn v24_null_key_fts_row_survives_unmapped() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 23);

    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (900, NULL, 'note', '', 'legacy null-key body', '[]', NULL, NULL, 1, '')",
        [],
    )
    .expect("insert legacy NULL-key fts_notes row");

    assert_eq!(
        run_migrations(&mut conn).expect("apply V24 rowid-map migration"),
        24
    );

    let still_present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM fts_notes WHERE rowid = 900",
            [],
            |row| row.get(0),
        )
        .expect("count NULL-key fts_notes row");
    assert_eq!(
        still_present, 1,
        "a NULL-key FTS row must survive V24 unmapped, not be swept by either sweep"
    );

    let mapped: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM fts_notes_rowids WHERE rowid = 900",
            [],
            |row| row.get(0),
        )
        .expect("count map rows at rowid 900");
    assert_eq!(
        mapped, 0,
        "a NULL-key row can never be mapped (its key cannot be attributed)"
    );
}

/// The orphan sweep's `NOT EXISTS` predicate must
/// stay NULL-safe (a NULL `entities.id`/`notes.id` row must not silence the
/// sweep of a genuine orphan the way the old `subject_id NOT IN (...)` form
/// did) and namespace-scoped (a subject that exists only in a DIFFERENT
/// namespace is still an orphan of this namespace's row).
#[test]
fn v24_orphan_sweep_is_null_safe_and_namespace_scoped() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 23);

    // A NULL `id` in the backing table must not suppress the sweep of a
    // real orphan. `entities.id` carries no explicit NOT NULL (only the
    // list-cursor trigger's own `entities_seq.entity_id` requires one), so
    // drop that trigger for this one insert — it is irrelevant to what V24
    // checks and would otherwise make this legitimate (if unusual) row
    // impossible to construct.
    conn.execute("DROP TRIGGER IF EXISTS assign_entity_list_seq", [])
        .expect("drop list-cursor trigger for the NULL-id fixture insert");
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at) \
         VALUES (NULL, 'local', 'concept', 'null-id row', '[]', 1, 1)",
        [],
    )
    .expect("insert entities row with NULL id");
    conn.execute(
        "INSERT INTO fts_entities \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (700, 'pure-orphan-id', 'entity', 'Gone', 'Gone', '[]', 'local', NULL, 1, 'concept')",
        [],
    )
    .expect("insert pure orphan fts_entities row");

    // A subject that exists, but only in a DIFFERENT namespace, must still
    // count as an orphan of this namespace's row.
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at) \
         VALUES ('cross-ns-id', 'other_ns', 'concept', 'other namespace concept', '[]', 1, 1)",
        [],
    )
    .expect("insert entities row in a different namespace");
    conn.execute(
        "INSERT INTO fts_entities \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (800, 'cross-ns-id', 'entity', 'Wrong namespace', 'Wrong namespace', '[]', 'local', NULL, 1, 'concept')",
        [],
    )
    .expect("insert fts_entities row whose subject exists only in another namespace");

    assert_eq!(
        run_migrations(&mut conn).expect("apply V24 rowid-map migration"),
        24
    );

    let orphan_survives: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM fts_entities WHERE rowid = 700",
            [],
            |row| row.get(0),
        )
        .expect("count pure orphan row");
    assert_eq!(
        orphan_survives, 0,
        "a real orphan must be swept even though the backing table has a NULL id row"
    );

    let cross_ns_survives: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM fts_entities WHERE rowid = 800",
            [],
            |row| row.get(0),
        )
        .expect("count cross-namespace row");
    assert_eq!(
        cross_ns_survives, 0,
        "a subject existing only in a different namespace must still be swept as an orphan \
         of this namespace's row"
    );
}

/// FTS5 rowid is not a write timestamp — explicit
/// rowids are legal and V23's replacement-table repopulation carries no
/// ORDER BY. The backfill must choose the survivor by `updated_at`, with
/// rowid only as a tie-break, not by rowid alone.
#[test]
fn v24_backfill_survivor_is_chosen_by_updated_at_not_rowid() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 23);

    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, created_at, updated_at) \
         VALUES ('note-id', 'local', 'memory', 'active', 'content', 1, 1)",
        [],
    )
    .expect("insert live note");

    // Lower rowid, but NEWER updated_at -- the correct, more recent write.
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (100, 'note-id', 'note', '', 'newer body', '[]', 'local', NULL, 500, 'memory')",
        [],
    )
    .expect("insert newer-but-lower-rowid fts_notes row");
    // Higher rowid, but OLDER updated_at -- a stale write that landed at a
    // later rowid.
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (200, 'note-id', 'note', '', 'older body', '[]', 'local', NULL, 100, 'memory')",
        [],
    )
    .expect("insert older-but-higher-rowid fts_notes row");

    assert_eq!(
        run_migrations(&mut conn).expect("apply V24 rowid-map migration"),
        24
    );

    let surviving_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM fts_notes WHERE subject_id = 'note-id'",
            [],
            |row| row.get(0),
        )
        .expect("read surviving fts_notes row for note-id");
    assert_eq!(
        surviving_rowid, 100,
        "the newer document (by updated_at) must survive even though its rowid is lower"
    );

    let mapped_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM fts_notes_rowids WHERE namespace = 'local' AND subject_id = 'note-id'",
            [],
            |row| row.get(0),
        )
        .expect("read note-id map entry");
    assert_eq!(mapped_rowid, 100);
}

/// `ORDER BY updated_at ASC, rowid ASC` feeding `INSERT OR REPLACE` means an
/// exact `updated_at` tie is broken by rowid: the higher rowid is processed
/// last and wins the map entry. Reversing or dropping the rowid tie-breaker
/// would leave `v24_backfill_survivor_is_chosen_by_updated_at_not_rowid`
/// (unequal timestamps) green while changing this specified behavior.
#[test]
fn v24_backfill_survivor_on_equal_updated_at_is_the_higher_rowid() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 23);

    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, created_at, updated_at) \
         VALUES ('note-id', 'local', 'memory', 'active', 'content', 1, 1)",
        [],
    )
    .expect("insert live note");

    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (100, 'note-id', 'note', '', 'lower rowid body', '[]', 'local', NULL, 500, 'memory')",
        [],
    )
    .expect("insert lower-rowid fts_notes row");
    conn.execute(
        "INSERT INTO fts_notes \
         (rowid, subject_id, kind, title, body, tags, namespace, metadata, updated_at, record_kind) \
         VALUES (200, 'note-id', 'note', '', 'higher rowid body', '[]', 'local', NULL, 500, 'memory')",
        [],
    )
    .expect("insert higher-rowid fts_notes row with the SAME updated_at");

    assert_eq!(
        run_migrations(&mut conn).expect("apply V24 rowid-map migration"),
        24
    );

    let surviving_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM fts_notes WHERE subject_id = 'note-id'",
            [],
            |row| row.get(0),
        )
        .expect("read surviving fts_notes row for note-id");
    assert_eq!(
        surviving_rowid, 200,
        "an exact updated_at tie must be broken toward the higher rowid"
    );

    let mapped_rowid: i64 = conn
        .query_row(
            "SELECT rowid FROM fts_notes_rowids WHERE namespace = 'local' AND subject_id = 'note-id'",
            [],
            |row| row.get(0),
        )
        .expect("read note-id map entry");
    assert_eq!(mapped_rowid, 200);
}

/// Migration 024 writes a durable completion marker for each map's own
/// state sidecar in the same transaction as its backfill, so a migrated
/// database's runtime `ensure_fts_rowid_map_backfilled` short-circuit never
/// re-scans `fts_entities`/`fts_notes`.
#[test]
fn v24_leaves_both_backfill_markers_present() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 23);

    assert_eq!(
        run_migrations(&mut conn).expect("apply V24 rowid-map migration"),
        24
    );

    for state in ["fts_entities_rowids_state", "fts_notes_rowids_state"] {
        let marked: bool = conn
            .query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM {state} WHERE key = 'backfill' AND value = 'complete')"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|e| panic!("read {state} marker: {e}"));
        assert!(
            marked,
            "{state} must carry a complete backfill marker after V24"
        );
    }
}

// ── V5: external_id unique index tests ──────────────────────────────────────

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
        rusqlite::params![name],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

fn index_is_unique(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT \"unique\" FROM pragma_index_list('notes') WHERE name=?1",
        rusqlite::params![name],
        |row| {
            let v: i64 = row.get(0)?;
            Ok(v != 0)
        },
    )
    .unwrap_or(false)
}

#[test]
fn v5_creates_unique_external_id_index() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    assert!(
        index_exists(&conn, "idx_comm_message_external_id"),
        "V5 must create idx_comm_message_external_id"
    );
    assert!(
        index_is_unique(&conn, "idx_comm_message_external_id"),
        "idx_comm_message_external_id must be UNIQUE"
    );
}

#[test]
fn v5_duplicate_external_id_insert_rejected() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    let now = chrono::Utc::now().timestamp_micros();
    // Insert a note with external_id
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('id-ext-1', 'local', 'message', 'active', 'body', \
                 json_object('external_id', 'imap:host:1:1'), ?1, ?1)",
        rusqlite::params![now],
    )
    .expect("first insert");
    // A second note with the same external_id must be rejected by the unique index.
    let dup = conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('id-ext-2', 'local', 'message', 'active', 'body2', \
                 json_object('external_id', 'imap:host:1:1'), ?1, ?1)",
        rusqlite::params![now],
    );
    assert!(dup.is_err(), "duplicate external_id must be rejected");
}

#[test]
fn v5_upgrade_from_duplicate_rows_succeeds() {
    // Simulate a V4-state database that already contains duplicate external_id rows.
    // Apply only migrations up to V4, insert duplicates, then run V5 and verify:
    //   - V5 migration completes without error
    //   - The canonical (earliest) row keeps its external_id
    //   - Later duplicate rows survive with external_id cleared to NULL
    let mut conn = open_memory();

    // Apply V1..V4 only.
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    let now = chrono::Utc::now().timestamp_micros();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 4) {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(migration.up).unwrap();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, now],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Insert two notes sharing the same external_id (canonical + duplicate).
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('canonical-row', 'local', 'message', 'active', 'first', \
                 json_object('external_id', 'imap:h:9:9'), ?1, ?1)",
        rusqlite::params![now],
    )
    .expect("canonical row");
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, status, content, properties, created_at, updated_at) \
         VALUES ('dup-row', 'local', 'message', 'active', 'second', \
                 json_object('external_id', 'imap:h:9:9'), ?1, ?1)",
        rusqlite::params![now],
    )
    .expect("duplicate row (allowed before V5 unique index)");

    // Now run V5.
    let tx = conn.transaction().unwrap();
    let v5 = MIGRATIONS.iter().find(|m| m.version == 5).unwrap();
    tx.execute_batch(v5.up)
        .expect("V5 migration must succeed on a DB with duplicate external_ids");
    tx.execute(
        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
        rusqlite::params![v5.version, v5.name, now],
    )
    .unwrap();
    tx.commit().unwrap();

    // V5 must have created the unique index.
    assert!(
        index_exists(&conn, "idx_comm_message_external_id"),
        "V5 must create idx_comm_message_external_id"
    );
    assert!(
        index_is_unique(&conn, "idx_comm_message_external_id"),
        "idx_comm_message_external_id must be UNIQUE after V5 upgrade"
    );

    // Canonical row keeps its external_id.
    let canonical_ext: Option<String> = conn
        .query_row(
            "SELECT json_extract(properties, '$.external_id') FROM notes WHERE id='canonical-row'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        canonical_ext.as_deref(),
        Some("imap:h:9:9"),
        "canonical row must retain its external_id"
    );

    // Duplicate row survives but with external_id cleared.
    let dup_content: String = conn
        .query_row("SELECT content FROM notes WHERE id='dup-row'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        dup_content, "second",
        "duplicate row must survive (not deleted)"
    );

    let dup_ext: Option<String> = conn
        .query_row(
            "SELECT json_extract(properties, '$.external_id') FROM notes WHERE id='dup-row'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        dup_ext.is_none(),
        "duplicate row must have external_id cleared (got {:?})",
        dup_ext
    );
}

// ── _embedding_models.dim u32 range tests ───────────────────────────────────

/// Helper: open a migrated in-memory DB and insert a row into `_embedding_models`
/// with the given raw `dim` value (stored as i64 to exercise negative/overflow cases).
fn insert_model_with_dim(conn: &Connection, dim: i64) {
    // id and canonical_key are BLOBs; use distinct values per dim to avoid UNIQUE conflicts.
    let id = dim.to_be_bytes();
    let canonical_key = [(dim % 127) as u8; 8];
    let now = 0i64;
    conn.execute(
        "INSERT INTO _embedding_models \
         (id, engine_name, model_id, key_version, dim, status, canonical_key, created_at) \
         VALUES (?1, 'engine', 'model', 'engine/model', ?2, 'active', ?3, ?4)",
        rusqlite::params![id.as_slice(), dim, canonical_key.as_slice(), now],
    )
    .expect("insert model");
}

/// dim = -1 must be rejected: would silently become u32::MAX via `as u32`.
#[test]
fn embedding_model_dim_negative_is_rejected() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    insert_model_with_dim(&conn, -1);
    let result = query_embedding_models_conn(&conn, None);
    assert!(
        result.is_err(),
        "dim = -1 must be rejected; got: {:?}",
        result
    );
}

/// dim = u32::MAX + 1 must be rejected: would silently truncate to 0 via `as u32`.
#[test]
fn embedding_model_dim_u32_max_plus_one_is_rejected() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    insert_model_with_dim(&conn, i64::from(u32::MAX) + 1);
    let result = query_embedding_models_conn(&conn, None);
    assert!(
        result.is_err(),
        "dim = u32::MAX + 1 must be rejected; got: {:?}",
        result
    );
}

/// dim = u32::MAX (4 294 967 295) is a legal u32 value and must be accepted.
#[test]
fn embedding_model_dim_u32_max_is_accepted() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    insert_model_with_dim(&conn, i64::from(u32::MAX));
    let result = query_embedding_models_conn(&conn, None);
    assert!(
        result.is_ok(),
        "dim = u32::MAX must be accepted; got: {:?}",
        result
    );
    let records = result.unwrap();
    assert_eq!(records[0].dimensions, u32::MAX);
}

// ── V6: ADR-081 recall retune driver (brain_implicit_mass + brain_serve_ledger) ──

#[test]
fn v6_creates_brain_retune_tables() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations should succeed");
    assert!(
        table_exists(&conn, "brain_implicit_mass"),
        "V6 must create brain_implicit_mass"
    );
    assert!(
        column_exists(&conn, "brain_implicit_mass", "last_effective_weight"),
        "V6 must add last_effective_weight to brain_implicit_mass"
    );
    assert!(
        table_exists(&conn, "brain_serve_ledger"),
        "V6 must create brain_serve_ledger"
    );
    // Note: `pragma_table_info` does not surface `GENERATED ALWAYS AS ... VIRTUAL`
    // columns on this SQLite version (verified empirically) — the column's
    // presence and COALESCE behavior are instead exercised directly by the
    // v6_accounting_profile_id_* tests below via SELECT.
    assert!(index_exists(&conn, "idx_brain_serve_ledger_unique"));
    // `index_is_unique` (shared helper) hardcodes `pragma_index_list('notes')`, so
    // it cannot check an index on brain_serve_ledger — query the correct table
    // directly instead.
    let is_unique: bool = conn
        .query_row(
            "SELECT \"unique\" FROM pragma_index_list('brain_serve_ledger') WHERE name = ?1",
            rusqlite::params!["idx_brain_serve_ledger_unique"],
            |row| {
                let v: i64 = row.get(0)?;
                Ok(v != 0)
            },
        )
        .unwrap_or(false);
    assert!(is_unique, "idx_brain_serve_ledger_unique must be UNIQUE");
    assert!(index_exists(&conn, "idx_brain_serve_ledger_suppression"));
    assert!(index_exists(&conn, "idx_brain_serve_ledger_accounting"));
    assert!(
        table_exists(&conn, "brain_scorer_dedup"),
        "V6 must create brain_scorer_dedup (ADR-081 §2/§6 dedup claim table)"
    );
}

#[test]
fn v6_scorer_dedup_primary_key_rejects_duplicate() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    conn.execute(
        "INSERT INTO brain_scorer_dedup (scorer_run_id, serve_ledger_id, claimed_at) \
         VALUES ('run-1', 'row-1', 1000)",
        [],
    )
    .expect("first claim");
    let dup = conn.execute(
        "INSERT INTO brain_scorer_dedup (scorer_run_id, serve_ledger_id, claimed_at) \
         VALUES ('run-1', 'row-1', 2000)",
        [],
    );
    assert!(
        dup.is_err(),
        "duplicate (scorer_run_id, serve_ledger_id) must be rejected by the primary key"
    );
    // A different scorer_run_id grading the same row, or the same run grading
    // a different row, must both be legal (ADR-081 §2: one run may legitimately
    // grade multiple serve rows for the same target).
    conn.execute(
        "INSERT INTO brain_scorer_dedup (scorer_run_id, serve_ledger_id, claimed_at) \
         VALUES ('run-2', 'row-1', 3000)",
        [],
    )
    .expect("different scorer_run_id, same row must be legal");
    conn.execute(
        "INSERT INTO brain_scorer_dedup (scorer_run_id, serve_ledger_id, claimed_at) \
         VALUES ('run-1', 'row-2', 4000)",
        [],
    )
    .expect("same scorer_run_id, different row must be legal");
}

#[test]
fn v6_accounting_profile_id_prefers_served_by() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    conn.execute(
        "INSERT INTO brain_serve_ledger \
         (id, namespace, consumer_kind, served_by_profile_id, resolved_profile_id, \
          target_id, query_class, query_raw, served_at) \
         VALUES ('row-1', 'local', 'recall', 'served-profile', 'resolved-profile', \
                 'target-1', 'class-1', 'raw query', 1000)",
        [],
    )
    .expect("insert");
    let accounting: String = conn
        .query_row(
            "SELECT accounting_profile_id FROM brain_serve_ledger WHERE id = 'row-1'",
            [],
            |row| row.get(0),
        )
        .expect("read accounting_profile_id");
    assert_eq!(
        accounting, "served-profile",
        "served_by_profile_id must win when both are set"
    );
}

#[test]
fn v6_accounting_profile_id_falls_back_to_resolved() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    conn.execute(
        "INSERT INTO brain_serve_ledger \
         (id, namespace, consumer_kind, resolved_profile_id, \
          target_id, query_class, query_raw, served_at) \
         VALUES ('row-2', 'local', 'recall', 'resolved-profile', \
                 'target-1', 'class-1', 'raw query', 1000)",
        [],
    )
    .expect("insert");
    let accounting: Option<String> = conn
        .query_row(
            "SELECT accounting_profile_id FROM brain_serve_ledger WHERE id = 'row-2'",
            [],
            |row| row.get(0),
        )
        .expect("read accounting_profile_id");
    assert_eq!(accounting.as_deref(), Some("resolved-profile"));
}

#[test]
fn v6_accounting_profile_id_null_when_both_unset() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    conn.execute(
        "INSERT INTO brain_serve_ledger \
         (id, namespace, consumer_kind, target_id, query_class, query_raw, served_at) \
         VALUES ('row-3', 'local', 'recall', 'target-1', 'class-1', 'raw query', 1000)",
        [],
    )
    .expect("insert");
    let accounting: Option<String> = conn
        .query_row(
            "SELECT accounting_profile_id FROM brain_serve_ledger WHERE id = 'row-3'",
            [],
            |row| row.get(0),
        )
        .expect("read accounting_profile_id");
    assert!(
        accounting.is_none(),
        "accounting_profile_id must be NULL (fail-safe path) when neither source is set"
    );
}

#[test]
fn v6_serve_ledger_uniqueness_rejects_duplicate() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    conn.execute(
        "INSERT INTO brain_serve_ledger \
         (id, namespace, consumer_kind, target_id, query_class, query_raw, served_at) \
         VALUES ('row-a', 'local', 'recall', 'target-1', 'class-1', 'q', 1000)",
        [],
    )
    .expect("first insert");
    let dup = conn.execute(
        "INSERT INTO brain_serve_ledger \
         (id, namespace, consumer_kind, target_id, query_class, query_raw, served_at) \
         VALUES ('row-b', 'local', 'recall', 'target-1', 'class-1', 'q', 1000)",
        [],
    );
    assert!(
        dup.is_err(),
        "duplicate (namespace, target_id, query_class, served_at) must be rejected"
    );
}

#[test]
fn v6_implicit_mass_upsert_on_conflict() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("migrations");
    conn.execute(
        "INSERT INTO brain_implicit_mass (profile_id, namespace, target_id, mass, last_event_at, last_effective_weight) \
         VALUES ('p1', 'local', 't1', 0.1, 1000, 0.1) \
         ON CONFLICT(profile_id, namespace, target_id) \
         DO UPDATE SET mass = excluded.mass, last_event_at = excluded.last_event_at, \
                       last_effective_weight = excluded.last_effective_weight",
        [],
    )
    .expect("first insert");
    conn.execute(
        "INSERT INTO brain_implicit_mass (profile_id, namespace, target_id, mass, last_event_at, last_effective_weight) \
         VALUES ('p1', 'local', 't1', 0.2, 2000, 0.0) \
         ON CONFLICT(profile_id, namespace, target_id) \
         DO UPDATE SET mass = excluded.mass, last_event_at = excluded.last_event_at, \
                       last_effective_weight = excluded.last_effective_weight",
        [],
    )
    .expect("conflicting upsert");
    let (mass, last_event_at, last_effective_weight): (f64, i64, f64) = conn
        .query_row(
            "SELECT mass, last_event_at, last_effective_weight FROM brain_implicit_mass \
             WHERE profile_id='p1' AND namespace='local' AND target_id='t1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read row");
    assert_eq!(mass, 0.2);
    assert_eq!(last_event_at, 2000);
    assert_eq!(
        last_effective_weight, 0.0,
        "last_effective_weight must reflect the second (conflicting) upsert's value"
    );
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM brain_implicit_mass WHERE profile_id='p1' AND namespace='local' AND target_id='t1'",
            [],
            |row| row.get(0),
        )
        .expect("count rows");
    assert_eq!(count, 1, "upsert must not create a second row");
}

// ── V10: entities.content_ref (khive#292) ───────────────────────────────────

#[test]
fn v10_adds_content_ref_column_and_partial_index() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 10);
    assert!(
        column_exists(&conn, "entities", "content_ref"),
        "V10 must add entities.content_ref"
    );
    assert!(
        index_exists(&conn, "idx_entities_content_ref"),
        "V10 must create idx_entities_content_ref"
    );
}

#[test]
fn v10_content_ref_defaults_null_and_accepts_a_value() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 10);

    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at) \
         VALUES ('e1', 'local', 'concept', 'NullRef', '[]', 0, 0)",
        [],
    )
    .expect("insert without content_ref");
    let null_ref: Option<String> = conn
        .query_row(
            "SELECT content_ref FROM entities WHERE id = 'e1'",
            [],
            |row| row.get(0),
        )
        .expect("read content_ref");
    assert_eq!(null_ref, None, "content_ref must default to NULL");

    let digest = "a".repeat(64);
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('e2', 'local', 'concept', 'WithRef', '[]', 0, 0, ?1)",
        rusqlite::params![digest],
    )
    .expect("insert with content_ref");
    let stored_ref: Option<String> = conn
        .query_row(
            "SELECT content_ref FROM entities WHERE id = 'e2'",
            [],
            |row| row.get(0),
        )
        .expect("read content_ref");
    assert_eq!(stored_ref, Some(digest));
}

// ── V13: stable list-cursor insertion sequences (#1424, #1462) ───────────────

#[test]
fn v13_upgrade_backfills_all_substrate_ledgers_in_legacy_order() {
    let mut conn = open_memory();
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 12)
    {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(migration.up).unwrap();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![migration.version, migration.name],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Deliberately insert out of order. Entity/note V13 backfills use
    // `(created_at, id)` ascending; edge backfill must instead preserve the
    // public pre-V13 cursor's `id` ordering so an outstanding edge cursor can
    // resume across the migration.
    for (id, created_at) in [("entity-z", 20), ("entity-b", 10), ("entity-a", 10)] {
        conn.execute(
            "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
             VALUES (?1, 'local', 'concept', ?1, ?2, ?2)",
            rusqlite::params![id, created_at],
        )
        .unwrap();
    }
    for (id, created_at) in [("note-z", 20), ("note-b", 10), ("note-a", 10)] {
        conn.execute(
            "INSERT INTO notes (id, namespace, kind, content, created_at, updated_at) \
             VALUES (?1, 'local', 'observation', ?1, ?2, ?2)",
            rusqlite::params![id, created_at],
        )
        .unwrap();
    }
    // Timestamp order is z, b, a while the compatibility order is a, b, z.
    for (id, created_at) in [("edge-a", 30), ("edge-z", 10), ("edge-b", 20)] {
        conn.execute(
            "INSERT INTO graph_edges \
             (namespace, id, source_id, target_id, relation, created_at, updated_at) \
             VALUES ('local', ?1, ?1 || '-source', ?1 || '-target', 'extends', ?2, ?2)",
            rusqlite::params![id, created_at],
        )
        .unwrap();
    }

    let latest = MIGRATIONS.last().expect("at least one migration").version;
    assert_eq!(run_migrations(&mut conn).unwrap(), latest);

    fn ordered_ids(conn: &Connection, table: &str, id_column: &str) -> Vec<String> {
        let sql = format!("SELECT {id_column} FROM {table} ORDER BY seq ASC");
        let mut stmt = conn.prepare(&sql).unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    assert_eq!(
        ordered_ids(&conn, "entities_seq", "entity_id"),
        vec![
            "entity-a".to_string(),
            "entity-b".to_string(),
            "entity-z".to_string()
        ]
    );
    assert_eq!(
        ordered_ids(&conn, "notes_seq", "note_id"),
        vec![
            "note-a".to_string(),
            "note-b".to_string(),
            "note-z".to_string()
        ]
    );
    assert_eq!(
        ordered_ids(&conn, "graph_edges_seq", "edge_id"),
        vec![
            "edge-a".to_string(),
            "edge-b".to_string(),
            "edge-z".to_string()
        ]
    );
}

#[test]
fn v13_insert_triggers_assign_monotonic_sequences_for_all_substrates() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();

    let sequence = |conn: &Connection, table: &str, id_column: &str, id: &str| -> i64 {
        conn.query_row(
            &format!("SELECT seq FROM {table} WHERE {id_column} = ?1"),
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap()
    };

    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES ('f-entity', 'local', 'concept', 'first', 100, 100)",
        [],
    )
    .unwrap();
    let entity_first = sequence(&conn, "entities_seq", "entity_id", "f-entity");
    conn.execute(
        "INSERT OR REPLACE INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES ('f-entity', 'local', 'concept', 'updated', 999, 999)",
        [],
    )
    .unwrap();
    assert_eq!(
        sequence(&conn, "entities_seq", "entity_id", "f-entity"),
        entity_first,
        "entity replacement must retain its first-insert sequence"
    );
    conn.execute("DELETE FROM entities WHERE id = 'f-entity'", [])
        .unwrap();
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES ('f-entity', 'local', 'concept', 'resurrected', 1000, 1000)",
        [],
    )
    .unwrap();
    assert_eq!(
        sequence(&conn, "entities_seq", "entity_id", "f-entity"),
        entity_first,
        "hard-delete plus same-id entity resurrection must retain its first sequence"
    );
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
         VALUES ('0-entity', 'local', 'concept', 'later', 100, 100)",
        [],
    )
    .unwrap();
    assert!(sequence(&conn, "entities_seq", "entity_id", "0-entity") > entity_first);

    conn.execute(
        "INSERT INTO notes (id, namespace, kind, content, created_at, updated_at) \
         VALUES ('f-note', 'local', 'observation', 'first', 100, 100)",
        [],
    )
    .unwrap();
    let note_first = sequence(&conn, "notes_seq", "note_id", "f-note");
    conn.execute(
        "INSERT OR REPLACE INTO notes \
         (id, namespace, kind, content, created_at, updated_at) \
         VALUES ('f-note', 'local', 'observation', 'updated', 999, 999)",
        [],
    )
    .unwrap();
    assert_eq!(
        sequence(&conn, "notes_seq", "note_id", "f-note"),
        note_first,
        "note upsert must retain its first-insert sequence"
    );
    conn.execute("DELETE FROM notes WHERE id = 'f-note'", [])
        .unwrap();
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, content, created_at, updated_at) \
         VALUES ('f-note', 'local', 'observation', 'resurrected', 1000, 1000)",
        [],
    )
    .unwrap();
    assert_eq!(
        sequence(&conn, "notes_seq", "note_id", "f-note"),
        note_first,
        "hard-delete plus same-id note resurrection must retain its first sequence"
    );
    conn.execute(
        "INSERT INTO notes (id, namespace, kind, content, created_at, updated_at) \
         VALUES ('0-note', 'local', 'observation', 'later', 100, 100)",
        [],
    )
    .unwrap();
    assert!(sequence(&conn, "notes_seq", "note_id", "0-note") > note_first);

    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, created_at, updated_at) \
         VALUES ('local', 'f-edge', 's1', 't1', 'extends', 100, 100)",
        [],
    )
    .unwrap();
    let edge_first = sequence(&conn, "graph_edges_seq", "edge_id", "f-edge");
    conn.execute(
        "INSERT OR REPLACE INTO graph_edges \
         (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
         VALUES ('local', 'f-edge', 's1', 't1', 'extends', 0.5, 999, 999)",
        [],
    )
    .unwrap();
    assert_eq!(
        sequence(&conn, "graph_edges_seq", "edge_id", "f-edge"),
        edge_first,
        "edge upsert must retain its first-insert sequence"
    );
    conn.execute(
        "DELETE FROM graph_edges WHERE namespace = 'local' AND id = 'f-edge'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, created_at, updated_at) \
         VALUES ('local', 'f-edge', 's1', 't1', 'extends', 1000, 1000)",
        [],
    )
    .unwrap();
    assert_eq!(
        sequence(&conn, "graph_edges_seq", "edge_id", "f-edge"),
        edge_first,
        "hard-delete plus same-id edge resurrection must retain its first sequence"
    );
    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, created_at, updated_at) \
         VALUES ('local', '0-edge', 's2', 't2', 'extends', 100, 100)",
        [],
    )
    .unwrap();
    assert!(sequence(&conn, "graph_edges_seq", "edge_id", "0-edge") > edge_first);
}

#[test]
fn v13_sequence_trigger_failure_rolls_back_each_substrate_insert() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER reject_entity_list_seq BEFORE INSERT ON entities_seq
         WHEN NEW.entity_id = 'reject-entity'
         BEGIN SELECT RAISE(ABORT, 'reject entity sequence'); END;
         CREATE TRIGGER reject_note_list_seq BEFORE INSERT ON notes_seq
         WHEN NEW.note_id = 'reject-note'
         BEGIN SELECT RAISE(ABORT, 'reject note sequence'); END;
         CREATE TRIGGER reject_edge_list_seq BEFORE INSERT ON graph_edges_seq
         WHEN NEW.edge_id = 'reject-edge'
         BEGIN SELECT RAISE(ABORT, 'reject edge sequence'); END;",
    )
    .unwrap();

    assert!(conn
        .execute(
            "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at) \
             VALUES ('reject-entity', 'local', 'concept', 'rejected', 100, 100)",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO notes (id, namespace, kind, content, created_at, updated_at) \
             VALUES ('reject-note', 'local', 'observation', 'rejected', 100, 100)",
            [],
        )
        .is_err());
    assert!(conn
        .execute(
            "INSERT INTO graph_edges \
             (namespace, id, source_id, target_id, relation, created_at, updated_at) \
             VALUES ('local', 'reject-edge', 's', 't', 'extends', 100, 100)",
            [],
        )
        .is_err());

    for (table, id_column, id) in [
        ("entities", "id", "reject-entity"),
        ("notes", "id", "reject-note"),
        ("graph_edges", "id", "reject-edge"),
    ] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {id_column} = ?1"),
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "failed sequence assignment stranded {table} row");
    }
}

// ── V13/V14: graph_edges.id must be globally unique (#1424, #1462 follow-up) ─

#[test]
fn v13_rejects_legacy_duplicate_edge_id_on_upgrade_from_v12() {
    let mut conn = open_memory();
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 12)
    {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(migration.up).unwrap();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![migration.version, migration.name],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // A V12 (pre-list-cursor) database permits the same edge id in two
    // namespaces because the base PRIMARY KEY is (namespace, id) -- exactly
    // the legacy state the list-cursor ledger's UUID-only backfill would
    // otherwise silently collapse onto one shared sequence row.
    for ns in ["ns-a", "ns-b"] {
        conn.execute(
            "INSERT INTO graph_edges \
             (namespace, id, source_id, target_id, relation, created_at, updated_at) \
             VALUES (?1, 'dup-edge', 's', 't', 'extends', 100, 100)",
            rusqlite::params![ns],
        )
        .unwrap();
    }

    // This drives the real upgrade path (unlike a fixture that force-applies
    // V13 before inserting the duplicate): run_migrations must hit V13's own
    // uniqueness guard on this legacy pair, not silently backfill a ledger
    // for V14 to fail on one version later.
    let result = run_migrations(&mut conn);
    assert!(
        result.is_err(),
        "V13 must fail loudly on a legacy cross-namespace duplicate edge id instead of \
         backfilling a ledger that collapses the two rows onto one sequence entry"
    );
    assert_eq!(
        read_schema_version(&conn).unwrap(),
        12,
        "a failed V13 migration must leave the database at its last good version"
    );
    assert!(
        !table_exists(&conn, "graph_edges_seq"),
        "V13 must roll back in full on a legacy duplicate -- no ledger table, ambiguous or not"
    );
}

#[test]
fn v14_index_rejects_new_cross_namespace_duplicate_edge_id() {
    let mut conn = open_memory();
    run_migrations(&mut conn).unwrap();

    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, created_at, updated_at) \
         VALUES ('ns-a', 'dup-edge', 's', 't', 'extends', 100, 100)",
        [],
    )
    .unwrap();

    let err = conn
        .execute(
            "INSERT INTO graph_edges \
             (namespace, id, source_id, target_id, relation, created_at, updated_at) \
             VALUES ('ns-b', 'dup-edge', 's2', 't2', 'extends', 200, 200)",
            [],
        )
        .expect_err(
            "a second namespace inserting an already-used edge id must hit the unique \
             index, not silently succeed and share the first namespace's ledger row",
        );
    assert!(
        err.to_string().to_lowercase().contains("unique"),
        "expected a UNIQUE constraint failure, got: {err}"
    );
}

// ── V18: distinguish never-activated ANN consumers from active S=0 ──────────

#[test]
fn v18_moves_legacy_zero_watermark_into_timestamped_pending_state() {
    let mut conn = open_memory();
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 16)
    {
        let tx = conn.transaction().unwrap();
        tx.execute_batch(migration.up).unwrap();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
            rusqlite::params![migration.version, migration.name],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    conn.execute(
        "INSERT INTO ann_consumer_watermark \
         (consumer, namespace, embedding_model, watermark) \
         VALUES ('legacy-zero', 'local', 'model', 0), \
                ('active', 'local', 'model', 17), \
                ('recovering', 'local', 'model', -1)",
        [],
    )
    .unwrap();

    let latest = MIGRATIONS.last().expect("at least one migration").version;
    assert_eq!(run_migrations(&mut conn).unwrap(), latest);
    assert!(table_exists(&conn, "ann_consumer_pending"));
    let legacy: (i64, i64) = conn
        .query_row(
            "SELECT watermark, pending.registered_at_us \
             FROM ann_consumer_watermark watermark \
             JOIN ann_consumer_pending pending USING \
               (consumer, namespace, embedding_model) \
             WHERE watermark.consumer = 'legacy-zero'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(legacy.0, -2);
    assert!(legacy.1 > 0);
    let protected: Vec<i64> = conn
        .prepare(
            "SELECT watermark FROM ann_consumer_watermark \
             WHERE consumer IN ('active', 'recovering') ORDER BY watermark",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(protected, vec![-1, 17]);
}

#[test]
fn read_schema_version_missing_ledger_is_zero() {
    let conn = open_memory();
    assert_eq!(
        read_schema_version(&conn).expect("absent ledger is not an error"),
        0
    );
}

/// Clears the shared `test_sync` barrier on drop so a panicking test cannot
/// strand it and hang every later test that opts into the contention hook.
struct BarrierGuard;

impl Drop for BarrierGuard {
    fn drop(&mut self) {
        *test_sync::STALE_READ_BARRIER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

// khive#1212 + ADR-160: two boots of one file must both converge, with the
// canonical database-GC owner acquired before either pool writer. The
// pre-held owner makes both waiters deterministic and proves no V21 marker or
// ledger mutation appears before ownership transfers.
#[test]
#[serial_test::serial(migration_contention)]
fn concurrent_boots_converge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("concurrent-boot.db");
    let backends = [
        crate::StorageBackend::sqlite(&path).expect("open first backend"),
        crate::StorageBackend::sqlite(&path).expect("open second backend"),
    ];
    let canonical = backends[0]
        .pool()
        .canonical_path()
        .expect("file-backed canonical path")
        .to_path_buf();
    let owner =
        crate::stores::blob::acquire_database_gc_owner_for_path_blocking(Some(canonical.clone()))
            .expect("pre-hold database GC owner");

    let handles: Vec<_> = backends
        .into_iter()
        .map(|backend| std::thread::spawn(move || backend.prepare_core_schema()))
        .collect();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::stores::blob::database_gc_waiter_count(Some(&canonical)) != 2
        && std::time::Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert_eq!(
        crate::stores::blob::database_gc_waiter_count(Some(&canonical)),
        2,
        "both boot paths must wait behind the canonical owner before taking a writer"
    );
    let before = Connection::open(&path).expect("inspect pre-owner schema");
    assert_eq!(
        read_schema_version(&before).expect("read pre-owner ledger"),
        0,
        "schema must remain untouched while database-GC ownership is unavailable"
    );
    drop(before);
    drop(owner);

    let latest = MIGRATIONS.last().expect("at least one migration").version;
    for handle in handles {
        let version = handle
            .join()
            .expect("thread join")
            .expect("both concurrent boots must succeed");
        assert_eq!(version, latest);
    }

    let conn = Connection::open(&path).expect("reopen");
    let rows: u32 = conn
        .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
            row.get(0)
        })
        .expect("count ledger rows");
    assert_eq!(
        rows as usize,
        MIGRATIONS.len(),
        "exactly one ledger row per migration"
    );
}

#[test]
#[serial_test::serial(migration_contention)]
fn raw_file_migration_refuses_when_database_gc_owner_is_held() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("raw-owner-refusal.db");
    drop(Connection::open(&path).expect("create raw migration database"));
    let canonical = std::fs::canonicalize(&path).expect("canonical database path");
    let owner = crate::stores::blob::acquire_database_gc_owner_for_path_blocking(Some(canonical))
        .expect("hold database GC owner");
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let path_for_runner = path.clone();
    let runner = std::thread::spawn(move || {
        let mut conn = Connection::open(path_for_runner).expect("open raw migration connection");
        let result = run_migrations(&mut conn)
            .map(|_| "unexpected success".to_string())
            .unwrap_or_else(|error| error.to_string());
        let version = read_schema_version(&conn).expect("read post-refusal schema");
        result_tx.send((result, version)).expect("report refusal");
    });
    let (error, version) = match result_rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(result) => result,
        Err(timeout) => {
            drop(owner);
            let _ = runner.join();
            panic!("raw migration waited for database-GC ownership instead of refusing: {timeout}");
        }
    };
    assert!(
        error.contains("database GC owner"),
        "unexpected refusal: {error}"
    );
    assert_eq!(
        version, 0,
        "raw refusal must occur before creating the migration ledger"
    );
    drop(owner);
    runner.join().expect("raw migration runner joins");
}

// khive#1217 review blocking finding: the pre-lock ahead-of-latest guard runs
// on a stale read. If a NEWER build commits a schema version above this
// binary's latest while this process waits for the migration write lock, the
// under-lock re-read must reject that version — not clamp it into a false Ok.
#[test]
#[serial_test::serial(migration_contention)]
fn mixed_version_boot_rejects_newer_schema_under_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("mixed-version-boot.db");
    let _guard = BarrierGuard;

    // The "newer build": create the ledger, then hold an uncommitted
    // IMMEDIATE transaction carrying a version above latest. Uncommitted, it
    // is invisible to the booting thread's stale read regardless of thread
    // scheduling; committed only after the barrier, it is ordered before the
    // booting thread's under-lock re-read by the write lock itself. That
    // makes the under-lock guard — not the pre-lock guard — the one that
    // must fire.
    let newer = Connection::open(&path).expect("open newer-build connection");
    newer
        .execute_batch(MIGRATION_TRACKING_TABLE)
        .expect("create ledger");
    let latest = MIGRATIONS.last().expect("at least one migration").version;
    newer
        .execute_batch(&format!(
            "BEGIN IMMEDIATE; INSERT INTO _schema_migrations (version, name, applied_at) \
             VALUES ({}, 'future-build', 0);",
            latest + 1
        ))
        .expect("stage future version uncommitted");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    *test_sync::STALE_READ_BARRIER.lock().unwrap() = Some(barrier.clone());
    test_sync::BUSY_OBSERVED.store(false, std::sync::atomic::Ordering::SeqCst);
    test_sync::WINNER_COMMITTED.store(false, std::sync::atomic::Ordering::SeqCst);
    test_sync::LOSER_SAW_WINNER_COMMIT.store(false, std::sync::atomic::Ordering::SeqCst);

    let boot = {
        let path = path.clone();
        std::thread::spawn(move || {
            test_sync::PARTICIPATE.with(|p| p.set(true));
            let mut conn = Connection::open(&path).expect("open booting connection");
            run_migrations(&mut conn)
        })
    };

    // Rendezvous: the booting thread has read the (stale, version-0) ledger
    // and is released toward its BEGIN IMMEDIATE, which blocks on the lock
    // still held here. Committing now publishes the future version strictly
    // before the boot's under-lock re-read.
    barrier.wait();
    newer
        .execute_batch("COMMIT")
        .expect("commit future version");

    let err = boot
        .join()
        .expect("thread join")
        .expect_err("a schema version above latest must be rejected, not clamped");
    let msg = err.to_string();
    assert!(
        msg.contains("ahead of the latest known migration"),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("migration write lock"),
        "the under-lock guard, not the pre-lock guard, must fire: {msg}"
    );
}

// ── V19: divergent V13/V14 ledger repair (#1649) ────────────────────────────

fn insert_v19_test_entity(conn: &Connection, id: &str, created_at: i64) {
    conn.execute(
        "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at) \
         VALUES (?1, 'local', 'concept', ?1, '[]', ?2, ?2)",
        rusqlite::params![id, created_at],
    )
    .expect("insert v19 test entity");
}

fn insert_v19_test_note(conn: &Connection, id: &str, created_at: i64) {
    conn.execute(
        "INSERT INTO notes \
         (id, namespace, kind, status, name, content, created_at, updated_at) \
         VALUES (?1, 'local', 'note', 'active', ?1, '', ?2, ?2)",
        rusqlite::params![id, created_at],
    )
    .expect("insert v19 test note");
}

fn insert_v19_test_edge(conn: &Connection, id: &str, source_id: &str, target_id: &str) {
    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
         VALUES ('local', ?1, ?2, ?3, 'relates_to', 1.0, 1, 1)",
        rusqlite::params![id, source_id, target_id],
    )
    .expect("insert v19 test edge");
}

#[test]
fn v19_repairs_divergent_cursor_ledgers() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 18);

    // Populate rows that predate the divergence being simulated below.
    insert_v19_test_entity(&conn, "e1", 10);
    insert_v19_test_entity(&conn, "e2", 20);
    insert_v19_test_note(&conn, "n1", 10);
    insert_v19_test_edge(&conn, "edge1", "e1", "e2");

    // Simulate a database that committed V13/V14 under non-canonical names,
    // never recorded V19, and lost part of its ledger — the exact divergence
    // V19 exists to repair.
    conn.execute(
        "UPDATE _schema_migrations SET name = 'legacy_v13_rename' WHERE version = 13",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE _schema_migrations SET name = 'legacy_v14_rename' WHERE version = 14",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM _schema_migrations WHERE version >= 19", [])
        .unwrap();
    conn.execute("DELETE FROM entities_seq WHERE entity_id = 'e1'", [])
        .unwrap();
    conn.execute("DELETE FROM notes_seq WHERE note_id = 'n1'", [])
        .unwrap();
    conn.execute("DELETE FROM graph_edges_seq WHERE edge_id = 'edge1'", [])
        .unwrap();

    run_migrations(&mut conn).expect("rerun must repair the divergence");

    let entity_cursor_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entities_seq \
             CROSS JOIN entities ON entities.id = entities_seq.entity_id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        entity_cursor_rows, 2,
        "both entities must be cursor-joinable"
    );

    let note_cursor_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes_seq \
             CROSS JOIN notes ON notes.id = notes_seq.note_id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(note_cursor_rows, 1, "the note must be cursor-joinable");

    let edge_cursor_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM graph_edges_seq \
             CROSS JOIN graph_edges ON graph_edges.id = graph_edges_seq.edge_id",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(edge_cursor_rows, 1, "the edge must be cursor-joinable");

    assert!(
        index_exists(&conn, "idx_graph_edges_id_unique"),
        "V19 must reassert the global edge-id unique index"
    );

    let (v13_name, v14_name): (String, String) = (
        conn.query_row(
            "SELECT name FROM _schema_migrations WHERE version = 13",
            [],
            |row| row.get(0),
        )
        .unwrap(),
        conn.query_row(
            "SELECT name FROM _schema_migrations WHERE version = 14",
            [],
            |row| row.get(0),
        )
        .unwrap(),
    );
    assert_eq!(v13_name, "list_cursor_sequences");
    assert_eq!(v14_name, "graph_edges_id_unique");

    let before: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities_seq", [], |row| row.get(0))
        .unwrap();
    run_migrations(&mut conn).expect("second rerun must be a no-op");
    let after: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities_seq", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        before, after,
        "a second rerun must not duplicate ledger rows"
    );
}

#[test]
fn post_v19_name_mismatch_fails_loud() {
    let mut conn = open_memory();
    run_migrations(&mut conn).expect("fresh migrate to latest (includes V19)");

    // An unrelated already-applied migration recorded under the wrong name —
    // not the known V13/V14 divergence V19 repairs — must fail loudly rather
    // than be silently accepted or trigger arbitrary re-execution.
    conn.execute(
        "UPDATE _schema_migrations SET name = 'wrong_name' WHERE version = 7",
        [],
    )
    .unwrap();

    let err = run_migrations(&mut conn).expect_err("a name mismatch must fail startup");
    let msg = err.to_string();
    assert!(
        msg.contains('7'),
        "error must name the affected version: {msg}"
    );
    assert!(
        msg.contains("wrong_name"),
        "error must name the actual recorded name: {msg}"
    );
    assert!(
        msg.contains("notes_seq"),
        "error must name the expected canonical name: {msg}"
    );
}

// ── V20: durable blob-GC claims (#1850) ──────────────────────────────────

#[test]
fn v20_blob_gc_claims_block_new_live_references_until_cleanup() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);

    assert!(
        table_exists(&conn, "blob_gc_claims"),
        "V20 must install the durable claim table"
    );

    let claimed = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    conn.execute(
        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
         VALUES ('root-a', ?1, 1)",
        [claimed],
    )
    .expect("install active claim");

    let insert_err = conn
        .execute(
            "INSERT INTO entities \
             (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
             VALUES ('blocked', 'local', 'document', 'blocked', '[]', 1, 1, ?1)",
            [claimed],
        )
        .expect_err("a live entity must not acquire a claimed content_ref");
    assert!(
        insert_err.to_string().contains("active blob sweep"),
        "unexpected trigger error: {insert_err}"
    );

    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('unrelated', 'local', 'document', 'unrelated', '[]', 1, 1, \
                 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb')",
        [],
    )
    .expect("an unrelated content_ref remains writable");

    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, deleted_at, content_ref) \
         VALUES ('tombstone', 'local', 'document', 'tombstone', '[]', 1, 1, 1, ?1)",
        [claimed],
    )
    .expect("a tombstoned row is not a live blob reference");

    let update_err = conn
        .execute(
            "UPDATE entities SET content_ref = ?1 WHERE id = 'unrelated'",
            [claimed],
        )
        .expect_err("an update must not acquire a claimed content_ref");
    assert!(
        update_err.to_string().contains("active blob sweep"),
        "unexpected update trigger error: {update_err}"
    );

    conn.execute(
        "DELETE FROM blob_gc_claims WHERE root_key = 'root-a' AND content_ref = ?1",
        [claimed],
    )
    .expect("release active claim");
    conn.execute(
        "UPDATE entities SET content_ref = ?1 WHERE id = 'unrelated'",
        [claimed],
    )
    .expect("the reference becomes writable after claim cleanup");
}

// ── V21: coordinated attachments-first cutover (ADR-160 D4) ────────────────────

#[test]
fn v21_legacy_refs_remain_pending_until_explicit_stage_and_finalize() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    let content = khive_storage::blob::ContentRef::from_hex("a".repeat(64)).unwrap();
    let model = khive_storage::blob::ContentRef::from_hex("b".repeat(64)).unwrap();
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, entity_type, name, tags, created_at, updated_at, deleted_at, content_ref) \
         VALUES ('doc', 'local', 'document', NULL, 'doc', '[]', 1, 1, 2, ?1), \
                ('model', 'local', 'artifact', 'moodboard_model', 'model', '[]', 1, 1, NULL, ?2)",
        rusqlite::params![content.as_str(), model.as_str()],
    )
    .unwrap();

    assert_eq!(run_migrations(&mut conn).unwrap(), 20);
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Pending
    );
    assert!(!table_exists(&conn, "attachments"));

    conn.execute(
        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) VALUES ('abandoned', ?1, 1)",
        [content.as_str()],
    )
    .unwrap();
    stage_attachment_cutover(&mut conn).unwrap();
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Incomplete
    );
    assert!(column_exists(&conn, "entities", "content_ref"));
    assert_eq!(read_schema_version(&conn).unwrap(), 20);
    let backfilled: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM attachments WHERE role = 'content' AND record_uuid IN ('doc', 'model')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backfilled, 2, "stage must include soft-deleted entity rows");
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "exclusive stage owner clears abandoned claims"
    );

    assert_eq!(
        run_migrations(&mut conn).unwrap(),
        20,
        "restart must remain resumable"
    );
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Incomplete
    );

    apply_generic_verified_attachment(
        &conn,
        "model",
        "entity",
        "fann-network",
        &model,
        Some("application/vnd.khive.fann+json"),
        Some(123),
        3,
    )
    .unwrap();
    finalize_attachment_cutover(&mut conn).unwrap();
    assert_eq!(read_schema_version(&conn).unwrap(), 21);
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Complete
    );
    assert!(!column_exists(&conn, "entities", "content_ref"));
    assert!(!index_exists(&conn, "idx_entities_content_ref"));
    for trigger in [
        "entities_reject_claimed_blob_insert",
        "entities_reject_claimed_blob_update",
    ] {
        assert!(
            !schema_object_exists(&conn, "trigger", trigger).unwrap(),
            "finalization must remove legacy trigger {trigger}"
        );
    }
    for trigger in [
        "attachments_reject_claimed_blob_insert",
        "attachments_reject_claimed_blob_update",
    ] {
        assert!(
            schema_object_exists(&conn, "trigger", trigger).unwrap(),
            "finalization must install attachment trigger {trigger}"
        );
    }
}

#[test]
fn v21_empty_database_fast_path_is_atomic_and_complete() {
    let mut conn = open_memory();
    assert_eq!(run_migrations(&mut conn).unwrap(), latest_schema_version());
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Complete
    );
    assert!(table_exists(&conn, "attachments"));
    assert!(!column_exists(&conn, "entities", "content_ref"));
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM attachment_cutover_state WHERE state = 'incomplete'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0,
        "the zero-ref fast path must expose no durable incomplete window"
    );
}

#[test]
fn v21_stage_rejects_invalid_refs_without_partial_schema_or_marker() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('bad', 'local', 'document', 'bad', '[]', 1, 1, 'NOT-CANONICAL')",
        [],
    )
    .unwrap();

    let error = stage_attachment_cutover(&mut conn)
        .expect_err("invalid legacy refs must fail the coordinated stage");
    assert!(
        error.to_string().contains("canonical"),
        "unexpected error: {error}"
    );
    assert_eq!(read_schema_version(&conn).unwrap(), 20);
    assert!(
        !table_exists(&conn, "attachments"),
        "failed stage must roll back DDL"
    );
    assert!(!table_exists(&conn, "attachment_cutover_state"));
    assert!(column_exists(&conn, "entities", "content_ref"));
}

/// Reddens the `021-attachments-a-stage.sql` table CHECK if the byte-length
/// arm is reverted: a 64-hex + NUL + tail value must still violate the CHECK
/// on a direct INSERT into `attachments`.
#[test]
fn v21_attachments_check_rejects_a_nul_embedded_content_ref() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    stage_attachment_cutover(&mut conn).unwrap();

    let error = conn
        .execute(
            "INSERT INTO attachments \
             (record_uuid, substrate, role, content_ref, created_at) \
             VALUES ('doc', 'entity', 'nul-probe', ?1, 1)",
            rusqlite::params![nul_embedded_canonical_ref()],
        )
        .expect_err("a NUL-embedded content_ref must violate the attachments CHECK constraint");
    assert!(
        error.to_string().to_lowercase().contains("check"),
        "expected a CHECK constraint violation, got {error}"
    );
}

/// Reddens `validate_canonical_legacy_refs` if its byte-length arm is
/// reverted: entities.content_ref carries no CHECK of its own, so a
/// NUL-tailed value is only caught by the migration validator.
#[test]
fn v21_stage_rejects_a_nul_embedded_legacy_content_ref() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('bad', 'local', 'document', 'bad', '[]', 1, 1, ?1)",
        rusqlite::params![nul_embedded_canonical_ref()],
    )
    .unwrap();

    let error = stage_attachment_cutover(&mut conn)
        .expect_err("a NUL-embedded legacy content_ref must fail the coordinated stage");
    assert!(
        error.to_string().contains("canonical"),
        "unexpected error: {error}"
    );
    assert_eq!(read_schema_version(&conn).unwrap(), 20);
    assert!(
        !table_exists(&conn, "attachments"),
        "failed stage must roll back DDL"
    );
}

/// Reddens `validate_canonical_attachment_and_claim_refs` if its byte-length
/// arm is reverted: `blob_gc_claims` carries no CHECK of its own, so a
/// NUL-tailed value is only caught by the migration validator.
#[test]
fn v21_stage_rejects_a_nul_embedded_blob_gc_claim_ref() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    conn.execute(
        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) VALUES ('abandoned', ?1, 1)",
        rusqlite::params![nul_embedded_canonical_ref()],
    )
    .unwrap();

    let error = stage_attachment_cutover(&mut conn)
        .expect_err("a NUL-embedded blob_gc_claims ref must fail the coordinated stage");
    let message = error.to_string();
    assert!(
        message.contains("blob_gc_claims") && message.contains("canonical"),
        "unexpected error: {message}"
    );
    assert_eq!(read_schema_version(&conn).unwrap(), 20);
}

/// A fixed bytes=64 arm would reject every valid 64-char ref in a UTF-16LE
/// database (128 bytes), so this proves the width-derived arm is
/// encoding-neutral rather than a hardcoded 64.
#[test]
fn v21_stage_accepts_a_valid_ref_in_a_utf16le_database() {
    let mut conn = open_memory_utf16le();
    let encoding: String = conn
        .query_row("PRAGMA encoding", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        encoding, "UTF-16le",
        "the fixture database must actually be UTF-16le"
    );

    migrate_through(&mut conn, 20);
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('doc', 'local', 'document', 'doc', '[]', 1, 1, ?1)",
        rusqlite::params!["a".repeat(64)],
    )
    .unwrap();

    stage_attachment_cutover(&mut conn)
        .expect("a valid canonical ref must pass the width-derived CHECK in a UTF-16LE database");
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Incomplete
    );
}

/// The NUL arm must stay red in UTF-16 as well: 64 chars + NUL + tail is
/// 130+ bytes against the expected 128.
#[test]
fn v21_stage_rejects_a_nul_embedded_ref_in_a_utf16le_database() {
    let mut conn = open_memory_utf16le();
    let encoding: String = conn
        .query_row("PRAGMA encoding", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        encoding, "UTF-16le",
        "the fixture database must actually be UTF-16le"
    );

    migrate_through(&mut conn, 20);
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('bad', 'local', 'document', 'bad', '[]', 1, 1, ?1)",
        rusqlite::params![nul_embedded_canonical_ref()],
    )
    .unwrap();

    let error = stage_attachment_cutover(&mut conn)
        .expect_err("a NUL-embedded ref must stay rejected in a UTF-16LE database");
    assert!(
        error.to_string().contains("canonical"),
        "unexpected error: {error}"
    );
}

#[test]
fn v21_verified_attachment_conflict_preserves_the_staged_identity() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    let original = khive_storage::blob::ContentRef::from_hex("d".repeat(64)).unwrap();
    let conflicting = khive_storage::blob::ContentRef::from_hex("e".repeat(64)).unwrap();
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, name, tags, created_at, updated_at, content_ref) \
         VALUES ('doc', 'local', 'document', 'doc', '[]', 1, 1, ?1)",
        [original.as_str()],
    )
    .unwrap();
    stage_attachment_cutover(&mut conn).unwrap();

    let error = apply_generic_verified_attachment(
        &conn,
        "doc",
        "entity",
        "content",
        &conflicting,
        None,
        None,
        2,
    )
    .expect_err("one role cannot be rebound to a different verified digest");
    assert!(
        error.to_string().contains("conflicts"),
        "unexpected error: {error}"
    );
    let retained: String = conn
        .query_row(
            "SELECT content_ref FROM attachments WHERE record_uuid = 'doc' AND role = 'content'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, original.as_str());
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Incomplete
    );
}

#[test]
fn v21_finalize_revalidates_model_coverage_and_attachment_claim_fences() {
    let mut conn = open_memory();
    migrate_through(&mut conn, 20);
    let model = khive_storage::blob::ContentRef::from_hex("c".repeat(64)).unwrap();
    conn.execute(
        "INSERT INTO entities \
         (id, namespace, kind, entity_type, name, tags, created_at, updated_at, content_ref) \
         VALUES ('model', 'local', 'artifact', 'moodboard_model', 'model', '[]', 1, 1, ?1)",
        [model.as_str()],
    )
    .unwrap();
    stage_attachment_cutover(&mut conn).unwrap();

    let error = finalize_attachment_cutover(&mut conn)
        .expect_err("every extant model with content requires fann-network");
    assert!(
        error.to_string().contains("fann-network"),
        "unexpected error: {error}"
    );
    assert_eq!(
        attachment_cutover_status(&conn).unwrap(),
        AttachmentCutoverStatus::Incomplete
    );
    assert!(column_exists(&conn, "entities", "content_ref"));

    apply_generic_verified_attachment(
        &conn,
        "model",
        "entity",
        "fann-network",
        &model,
        None,
        None,
        2,
    )
    .unwrap();
    finalize_attachment_cutover(&mut conn).unwrap();
    conn.execute(
        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) VALUES ('active', ?1, 3)",
        [model.as_str()],
    )
    .unwrap();
    let insert_error = conn
        .execute(
            "INSERT INTO attachments \
             (record_uuid, substrate, role, content_ref, created_at) \
             VALUES ('other', 'entity', 'content', ?1, 4)",
            [model.as_str()],
        )
        .expect_err("an attachment cannot acquire a claimed digest");
    assert!(insert_error.to_string().contains("active blob sweep"));
}
