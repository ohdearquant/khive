//! What a move does with the tables a pack creates outside the migration chain.
//!
//! `khive-db` cannot depend on a pack, so each test hands its store the DDL the
//! pack applies at boot, copied from that pack's schema plan.

use super::tests::{migrated, route, seed_note};
use super::*;

const GTD_AUDIT_DDL: &str = "CREATE TABLE gtd_lifecycle_audit (\
    note_id TEXT NOT NULL, from_state TEXT NOT NULL, to_state TEXT NOT NULL, \
    note TEXT, at INTEGER NOT NULL, namespace TEXT)";

const EVAL_RUNS_DDL: &str = "CREATE TABLE knowledge_eval_runs (\
    id TEXT PRIMARY KEY, namespace TEXT NOT NULL, run_at INTEGER NOT NULL, \
    query_set TEXT NOT NULL, total_queries INTEGER NOT NULL, \
    precision_at_5 REAL NOT NULL, recall_at_5 REAL NOT NULL, mrr REAL NOT NULL, \
    notes TEXT)";

const SNAPSHOTS_DDL: &str = "CREATE TABLE retrieval_snapshots (\
    namespace TEXT NOT NULL, index_type TEXT NOT NULL, snapshot BLOB NOT NULL, \
    created_at INTEGER NOT NULL, PRIMARY KEY (namespace, index_type))";

fn store(ddl: &[&str]) -> Connection {
    let conn = migrated();
    for statement in ddl {
        conn.execute_batch(statement).expect("pack table");
    }
    conn
}

fn audit(conn: &Connection, note_id: &str, at: i64, namespace: Option<&str>) {
    conn.execute(
        "INSERT INTO gtd_lifecycle_audit (note_id, from_state, to_state, note, at, namespace) \
         VALUES (?1, 'open', 'done', NULL, ?2, ?3)",
        rusqlite::params![note_id, at, namespace],
    )
    .expect("seed audit row");
}

fn audit_namespace(conn: &Connection, note_id: &str, at: i64) -> Option<String> {
    conn.query_row(
        "SELECT namespace FROM gtd_lifecycle_audit WHERE note_id = ?1 AND at = ?2",
        rusqlite::params![note_id, at],
        |row| row.get::<_, Option<String>>(0),
    )
    .expect("audit row")
}

fn eval_run(conn: &Connection, id: &str, namespace: &str) {
    conn.execute(
        "INSERT INTO knowledge_eval_runs \
         (id, namespace, run_at, query_set, total_queries, precision_at_5, recall_at_5, mrr) \
         VALUES (?1, ?2, 1, 'set', 1, 0.5, 0.5, 0.5)",
        rusqlite::params![id, namespace],
    )
    .expect("seed eval run");
}

fn eval_namespace(conn: &Connection, id: &str) -> String {
    conn.query_row(
        "SELECT namespace FROM knowledge_eval_runs WHERE id = ?1",
        [id],
        |row| row.get::<_, String>(0),
    )
    .expect("eval run")
}

fn snapshot(conn: &Connection, namespace: &str, index_type: &str) {
    conn.execute(
        "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) \
         VALUES (?1, ?2, x'00', 1)",
        rusqlite::params![namespace, index_type],
    )
    .expect("seed snapshot");
}

fn snapshot_namespaces(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT namespace FROM retrieval_snapshots ORDER BY namespace")
        .expect("prepare");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("rows")
}

#[test]
fn pack_created_tables_are_classified_by_the_move() {
    let conn = store(&[GTD_AUDIT_DDL, EVAL_RUNS_DDL, SNAPSHOTS_DDL]);
    let census = namespace_census::census(&conn).expect("census");
    for (name, expected) in [
        (
            "gtd_lifecycle_audit",
            TableDisposition::SubjectKeyed {
                subject_column: "note_id",
            },
        ),
        (
            "knowledge_eval_runs",
            TableDisposition::NamespaceScopedAggregate,
        ),
        (
            "retrieval_snapshots",
            TableDisposition::Derived {
                trigger_maintained: false,
            },
        ),
    ] {
        let table = census
            .tables
            .iter()
            .find(|table| table.name == name)
            .unwrap_or_else(|| panic!("{name} is in the census"));
        assert_eq!(disposition(table), Some(expected), "{name}");
    }
}

#[test]
fn a_task_audit_row_follows_its_note_and_a_null_namespace_row_is_untouched() {
    let conn = store(&[GTD_AUDIT_DDL]);
    seed_note(&conn, "task-1", "source", "task");
    audit(&conn, "task-1", 1, Some("source"));
    audit(&conn, "task-1", 2, Some("source"));
    audit(&conn, "task-1", 3, None);

    let request = MoveRequest::new("source", vec![route("note:task", "target")]);
    let counts = move_namespace(&conn, &request).expect("a task and its audit rows move");

    assert_eq!(counts.subjects.get("note:task"), Some(&1));
    assert_eq!(counts.rows.get("gtd_lifecycle_audit"), Some(&2));
    assert!(!counts.left_behind.contains_key("gtd_lifecycle_audit"));
    assert_eq!(
        audit_namespace(&conn, "task-1", 1).as_deref(),
        Some("target")
    );
    assert_eq!(
        audit_namespace(&conn, "task-1", 2).as_deref(),
        Some("target")
    );
    assert_eq!(
        audit_namespace(&conn, "task-1", 3),
        None,
        "a NULL-namespace row is outside the source and is not touched"
    );
}

#[test]
fn an_audit_row_without_a_routed_source_note_stays_and_is_reported() {
    let conn = store(&[GTD_AUDIT_DDL]);
    seed_note(&conn, "task-1", "source", "task");
    seed_note(&conn, "resident", "target", "task");
    audit(&conn, "task-1", 1, Some("source"));
    audit(&conn, "gone", 2, Some("source"));
    audit(&conn, "resident", 3, Some("source"));

    let request = MoveRequest::new("source", vec![route("note:task", "target")]);
    let counts = move_namespace(&conn, &request).expect("the routed task moves");

    assert_eq!(counts.rows.get("gtd_lifecycle_audit"), Some(&1));
    assert_eq!(counts.left_behind.get("gtd_lifecycle_audit"), Some(&2));
    assert_eq!(
        audit_namespace(&conn, "task-1", 1).as_deref(),
        Some("target")
    );
    assert_eq!(
        audit_namespace(&conn, "gone", 2).as_deref(),
        Some("source"),
        "no note at all"
    );
    assert_eq!(
        audit_namespace(&conn, "resident", 3).as_deref(),
        Some("source"),
        "its note was never in the source, so it is not a routed source note"
    );
}

#[test]
fn audit_rows_follow_their_note_to_its_own_target_in_a_partitioning_move() {
    let conn = store(&[GTD_AUDIT_DDL]);
    seed_note(&conn, "task-1", "source", "task");
    seed_note(&conn, "obs-1", "source", "observation");
    audit(&conn, "task-1", 1, Some("source"));
    audit(&conn, "obs-1", 2, Some("source"));

    let request = MoveRequest::new(
        "source",
        vec![
            route("note:task", "tasks"),
            route("note:observation", "observations"),
        ],
    );
    let counts = move_namespace(&conn, &request).expect("a partitioning move");

    assert_eq!(counts.rows.get("gtd_lifecycle_audit"), Some(&2));
    assert!(!counts.left_behind.contains_key("gtd_lifecycle_audit"));
    assert_eq!(
        audit_namespace(&conn, "task-1", 1).as_deref(),
        Some("tasks")
    );
    assert_eq!(
        audit_namespace(&conn, "obs-1", 2).as_deref(),
        Some("observations")
    );
}

#[test]
fn eval_runs_move_with_a_total_single_target_request() {
    let conn = store(&[EVAL_RUNS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    eval_run(&conn, "run-1", "source");
    eval_run(&conn, "run-2", "elsewhere");

    let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
    let counts = move_namespace(&conn, &request).expect("a total single-target move");

    assert_eq!(counts.rows.get("knowledge_eval_runs"), Some(&1));
    assert!(!counts.left_behind.contains_key("knowledge_eval_runs"));
    assert_eq!(eval_namespace(&conn, "run-1"), "target");
    assert_eq!(eval_namespace(&conn, "run-2"), "elsewhere");
}

#[test]
fn eval_runs_of_a_partitioning_move_stay_and_are_reported() {
    let conn = store(&[EVAL_RUNS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    seed_note(&conn, "n2", "source", "decision");
    eval_run(&conn, "run-1", "source");

    let request = MoveRequest::new(
        "source",
        vec![
            route("note:observation", "one"),
            route("note:decision", "another"),
        ],
    );
    let counts = move_namespace(&conn, &request).expect("a partitioning move");

    assert_eq!(counts.left_behind.get("knowledge_eval_runs"), Some(&1));
    assert!(!counts.rows.contains_key("knowledge_eval_runs"));
    assert_eq!(eval_namespace(&conn, "run-1"), "source");
}

#[test]
fn source_snapshots_are_removed_and_neighbouring_namespaces_keep_theirs() {
    let conn = store(&[SNAPSHOTS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    for namespace in [
        "source",
        "source::vamana::model-a",
        "source:vamana:model-a",
        "sourcery",
        "sourcery::vamana::model-a",
        "target::vamana::model-a",
    ] {
        snapshot(&conn, namespace, "vamana");
    }

    let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
    move_namespace(&conn, &request).expect("the note moves and its namespace's snapshots go");

    assert_eq!(
        snapshot_namespaces(&conn),
        [
            "source:vamana:model-a",
            "sourcery",
            "sourcery::vamana::model-a",
            "target::vamana::model-a",
        ]
    );
}

#[test]
fn a_source_name_with_like_metacharacters_deletes_only_its_own_snapshots() {
    for (source, own, neighbour) in [
        ("a_b", "a_b::vamana::m", "axb::vamana::m"),
        ("a%", "a%::vamana::m", "abc::vamana::m"),
        ("a\\b", "a\\b::vamana::m", "ab::vamana::m"),
    ] {
        let conn = store(&[SNAPSHOTS_DDL]);
        snapshot(&conn, own, "vamana");
        snapshot(&conn, neighbour, "vamana");

        let request = MoveRequest::new(source, vec![route("note:observation", "target")]);
        move_namespace(&conn, &request).expect("nothing is routed in this source");

        assert_eq!(snapshot_namespaces(&conn), [neighbour], "{source}");
    }
}

#[test]
fn a_namespace_differing_only_in_case_keeps_its_snapshots() {
    let conn = store(&[SNAPSHOTS_DDL]);
    snapshot(&conn, "a::vamana::m", "vamana");
    snapshot(&conn, "A::vamana::m", "vamana");

    let request = MoveRequest::new("a", vec![route("note:observation", "target")]);
    move_namespace(&conn, &request).expect("nothing is routed in this source");

    assert_eq!(snapshot_namespaces(&conn), ["A::vamana::m"]);
}

#[test]
fn snapshots_sharing_an_index_type_across_source_and_target_do_not_refuse_the_move() {
    let conn = store(&[SNAPSHOTS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    snapshot(&conn, "source", "vamana");
    snapshot(&conn, "target", "vamana");

    let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
    move_namespace(&conn, &request).expect("a deleted snapshot cannot collide with a kept one");

    assert_eq!(snapshot_namespaces(&conn), ["target"]);
}

#[test]
fn a_store_without_the_snapshot_table_moves_cleanly() {
    let conn = store(&[EVAL_RUNS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    eval_run(&conn, "run-1", "source");

    let request = MoveRequest::new("source", vec![route("note:observation", "target")]);
    let counts = move_namespace(&conn, &request).expect("an absent snapshot table is a no-op");

    assert_eq!(counts.subjects.get("note:observation"), Some(&1));
    assert_eq!(eval_namespace(&conn, "run-1"), "target");
}
