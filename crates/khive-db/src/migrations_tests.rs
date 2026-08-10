use super::query_embedding_models_conn;
use super::*;

fn open_memory() -> Connection {
    Connection::open_in_memory().expect("in-memory connection")
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
fn rejects_pre_consolidation_ledger() {
    let mut conn = open_memory();
    // Simulate a database carrying the old, pre-consolidation V1..V22 ledger.
    conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
    conn.execute(
        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (22, 'legacy', 0)",
        [],
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
    run_migrations(&mut conn).expect("migrations should succeed");
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
    run_migrations(&mut conn).expect("migrations should succeed");

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

    assert_eq!(run_migrations(&mut conn).unwrap(), 19);
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

// khive#1212: two processes booting the same database file must both complete
// migrations — the IMMEDIATE transaction serializes them and the under-lock
// re-check makes the loser converge instead of failing on already-applied DDL.
#[test]
#[serial_test::serial(migration_contention)]
fn concurrent_boots_converge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("concurrent-boot.db");
    let _guard = BarrierGuard;

    // Deterministic interleaving via the in-crate stale-read barrier: both
    // threads must observe the empty ledger (version 0, no lock held) before
    // either is released to compete for the IMMEDIATE write lock. The loser
    // is thereby guaranteed to reach the under-lock re-check with a stale
    // view, which the fast-forward counter asserts below.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    *test_sync::STALE_READ_BARRIER.lock().unwrap() = Some(barrier);
    test_sync::LOCKED_FAST_FORWARDS.store(0, std::sync::atomic::Ordering::Relaxed);
    test_sync::BUSY_OBSERVED.store(false, std::sync::atomic::Ordering::SeqCst);
    test_sync::WINNER_COMMITTED.store(false, std::sync::atomic::Ordering::SeqCst);
    test_sync::LOSER_SAW_WINNER_COMMIT.store(false, std::sync::atomic::Ordering::SeqCst);

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                test_sync::PARTICIPATE.with(|p| p.set(true));
                let mut conn = Connection::open(&path).expect("open");
                run_migrations(&mut conn)
            })
        })
        .collect();

    let latest = MIGRATIONS.last().expect("at least one migration").version;
    for handle in handles {
        let version = handle
            .join()
            .expect("thread join")
            .expect("both concurrent boots must succeed");
        assert_eq!(version, latest);
    }
    *test_sync::STALE_READ_BARRIER.lock().unwrap() = None;

    // Both threads observed version 0 before either took the write lock, so
    // the loser necessarily re-checked under the lock and fast-forwarded past
    // the winner's applied migrations. This fails if either the IMMEDIATE
    // behavior or the under-lock MAX(version) re-check regresses.
    assert!(
        test_sync::LOCKED_FAST_FORWARDS.load(std::sync::atomic::Ordering::Relaxed) >= 1,
        "loser thread must observe the sibling's ledger under the write lock"
    );

    // SQLite itself reported a busy acquisition to the loser while the winner
    // held the write lock: the winner does not commit until the loser's busy
    // handler has fired, so this is observed contention, not an intended
    // attempt. If IMMEDIATE regressed to deferred behavior, no busy signal
    // occurs on BEGIN and this fails (that interleaving also fails outright
    // on duplicate DDL).
    assert!(
        test_sync::BUSY_OBSERVED.load(std::sync::atomic::Ordering::SeqCst),
        "SQLite must observe the loser's blocked BEGIN IMMEDIATE while the winner holds the lock"
    );
    assert!(
        test_sync::LOSER_SAW_WINNER_COMMIT.load(std::sync::atomic::Ordering::SeqCst),
        "loser's BEGIN IMMEDIATE must return only after the winner committed"
    );

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
    run_migrations(&mut conn).expect("fresh migrate to latest (includes V19)");

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
    conn.execute("DELETE FROM _schema_migrations WHERE version = 19", [])
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
