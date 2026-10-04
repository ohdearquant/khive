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

const EXEC_RUNS_DDL: &str = "CREATE TABLE exec_runs (\
    id TEXT PRIMARY KEY, namespace TEXT NOT NULL, actor TEXT NOT NULL, \
    tool TEXT NOT NULL, session_id TEXT, seq INTEGER, receipt TEXT NOT NULL, \
    created_at INTEGER NOT NULL)";

const EXEC_RUNS_SESSION_SEQ_DDL: &str = "CREATE UNIQUE INDEX idx_exec_runs_session_seq \
    ON exec_runs(namespace, session_id, seq) WHERE session_id IS NOT NULL";

const EXEC_EVENTS_DDL: &str = "CREATE TABLE exec_events (\
    id INTEGER PRIMARY KEY AUTOINCREMENT, namespace TEXT NOT NULL, \
    run_id TEXT NOT NULL, kind TEXT NOT NULL, at INTEGER NOT NULL, detail TEXT)";

const GIT_RECEIPTS_DDL: &str = "CREATE TABLE git_receipts (\
    id TEXT PRIMARY KEY NOT NULL, namespace TEXT NOT NULL, actor TEXT NOT NULL, \
    session_id TEXT, verb TEXT NOT NULL, repo TEXT NOT NULL, \
    inputs TEXT NOT NULL CHECK (json_valid(inputs)), \
    gate TEXT NOT NULL CHECK (json_valid(gate)), policy TEXT, fork_policy TEXT, \
    credential TEXT, started_at INTEGER NOT NULL, finished_at INTEGER, \
    disposition TEXT NOT NULL, result TEXT NOT NULL CHECK (json_valid(result)), \
    reason TEXT)";

const TOOL_POLICY_DDL: &str = "CREATE TABLE tool_policy (\
    id TEXT PRIMARY KEY, namespace TEXT NOT NULL, actor TEXT NOT NULL, \
    tool TEXT NOT NULL, decision TEXT NOT NULL, note TEXT, created_at INTEGER NOT NULL, \
    created_by TEXT, updated_at INTEGER, updated_by TEXT, history TEXT, \
    deleted_at INTEGER, deleted_by TEXT)";

const TOOL_GRANTS_DDL: &str = "CREATE TABLE tool_grants (\
    id TEXT PRIMARY KEY, namespace TEXT NOT NULL, actor TEXT NOT NULL, \
    tool TEXT NOT NULL, scope TEXT, reason TEXT, status TEXT NOT NULL, \
    requested_at INTEGER NOT NULL, decided_at INTEGER, decided_by TEXT, \
    expires_at INTEGER, decision_note TEXT, registry_id TEXT, \
    definition_digest TEXT, invalidated_by_registry_id TEXT, invalidated_at INTEGER)";

const PACK_TABLES_DDL: [&str; 6] = [
    EXEC_RUNS_DDL,
    EXEC_RUNS_SESSION_SEQ_DDL,
    EXEC_EVENTS_DDL,
    GIT_RECEIPTS_DDL,
    TOOL_POLICY_DDL,
    TOOL_GRANTS_DDL,
];

/// The instant the tests below ask the move to judge grant expiry at.
const NOW: i64 = 1_000;

fn exec_run(conn: &Connection, id: &str, namespace: &str, seq: i64) {
    conn.execute(
        "INSERT INTO exec_runs \
         (id, namespace, actor, tool, session_id, seq, receipt, created_at) \
         VALUES (?1, ?2, 'actor', 'tool', 'session-1', ?3, '{}', 1)",
        rusqlite::params![id, namespace, seq],
    )
    .expect("seed exec run");
}

fn exec_event(conn: &Connection, run_id: &str, namespace: &str) {
    conn.execute(
        "INSERT INTO exec_events (namespace, run_id, kind, at) VALUES (?1, ?2, 'started', 1)",
        rusqlite::params![namespace, run_id],
    )
    .expect("seed exec event");
}

fn git_receipt(conn: &Connection, id: &str, namespace: &str) {
    conn.execute(
        "INSERT INTO git_receipts \
         (id, namespace, actor, verb, repo, inputs, gate, started_at, disposition, result) \
         VALUES (?1, ?2, 'actor', 'git.push', 'repo', '{}', '{}', 1, 'unknown', '{}')",
        rusqlite::params![id, namespace],
    )
    .expect("seed git receipt");
}

fn policy(conn: &Connection, id: &str, namespace: &str, deleted_at: Option<i64>) {
    conn.execute(
        "INSERT INTO tool_policy \
         (id, namespace, actor, tool, decision, note, created_at, deleted_at) \
         VALUES (?1, ?2, 'actor', ?1, 'allow', 'note-text-for-the-log-check', 1, ?3)",
        rusqlite::params![id, namespace, deleted_at],
    )
    .expect("seed policy");
}

fn grant(conn: &Connection, id: &str, namespace: &str, expires_at: Option<i64>) {
    conn.execute(
        "INSERT INTO tool_grants \
         (id, namespace, actor, tool, status, requested_at, expires_at) \
         VALUES (?1, ?2, 'actor', ?1, 'granted', 1, ?3)",
        rusqlite::params![id, namespace, expires_at],
    )
    .expect("seed grant");
}

/// A grant in any state, with no expiry and no invalidation.
fn grant_with_status(conn: &Connection, id: &str, namespace: &str, status: &str) {
    conn.execute(
        "INSERT INTO tool_grants \
         (id, namespace, actor, tool, status, requested_at) \
         VALUES (?1, ?2, 'actor', ?1, ?3, 1)",
        rusqlite::params![id, namespace, status],
    )
    .expect("seed grant");
}

fn ns(conn: &Connection, table: &str, id: &str) -> String {
    conn.query_row(
        &format!("SELECT namespace FROM {table} WHERE id = ?1"),
        [id],
        |row| row.get::<_, String>(0),
    )
    .expect("row")
}

/// Every column of one row, so a test can prove a move rewrote none of them.
fn image(conn: &Connection, table: &str, id: &str) -> Vec<rusqlite::types::Value> {
    let sql = format!("SELECT * FROM {table} WHERE id = ?1");
    let mut stmt = conn.prepare(&sql).expect("prepare");
    let columns = stmt.column_count();
    stmt.query_row([id], |row| {
        (0..columns)
            .map(|index| row.get::<_, rusqlite::types::Value>(index))
            .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
    })
    .expect("row image")
}

fn events_matching_their_run(conn: &Connection, namespace: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM exec_events AS event \
         JOIN exec_runs AS run ON run.id = event.run_id AND run.namespace = event.namespace \
         WHERE event.namespace = ?1",
        [namespace],
        |row| row.get(0),
    )
    .expect("count events")
}

/// One source holding rows in all five tables, beside a neighbour namespace
/// whose rows must never move. The policies include a soft-deleted one and the
/// grants include two that are expired at `NOW`, one of them at exactly `NOW`.
fn seeded_source() -> Connection {
    let conn = store(&PACK_TABLES_DDL);
    seed_note(&conn, "n1", "source", "observation");
    seed_note(&conn, "n2", "source", "decision");
    exec_run(&conn, "run-1", "source", 1);
    exec_run(&conn, "run-2", "source", 2);
    exec_run(&conn, "run-other", "elsewhere", 1);
    exec_event(&conn, "run-1", "source");
    exec_event(&conn, "run-1", "source");
    exec_event(&conn, "run-2", "source");
    exec_event(&conn, "run-other", "elsewhere");
    git_receipt(&conn, "receipt-1", "source");
    git_receipt(&conn, "receipt-other", "elsewhere");
    policy(&conn, "policy-live", "source", None);
    policy(&conn, "policy-live-2", "source", None);
    policy(&conn, "policy-gone", "source", Some(5));
    policy(&conn, "policy-elsewhere", "elsewhere", None);
    grant(&conn, "grant-open", "source", None);
    grant(&conn, "grant-future", "source", Some(NOW + 1));
    grant(&conn, "grant-past", "source", Some(NOW - 1));
    grant(&conn, "grant-edge", "source", Some(NOW));
    grant(&conn, "grant-elsewhere", "elsewhere", None);
    conn
}

fn total_request() -> MoveRequest {
    MoveRequest::new(
        "source",
        vec![
            route("note:observation", "target"),
            route("note:decision", "target"),
        ],
    )
    .at(NOW)
}

fn observation_request() -> MoveRequest {
    MoveRequest::new("source", vec![route("note:observation", "target")])
}

fn partitioning_request() -> MoveRequest {
    MoveRequest::new(
        "source",
        vec![
            route("note:observation", "one"),
            route("note:decision", "another"),
        ],
    )
    .at(NOW)
}

type CapturedEvents = std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>;

struct Captured(CapturedEvents);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        let _ = write!(self.0, "{}={:?} ", field.name(), value);
    }
}

impl tracing::Subscriber for Captured {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let level = *event.metadata().level();
        self.0.lock().unwrap().push((level, line));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Run `run` and return its result with every WARN event it logged, one string
/// of `field=value` pairs per event.
fn warnings_of<T>(run: impl FnOnce() -> T) -> (T, Vec<String>) {
    let events = CapturedEvents::default();
    let result = tracing::subscriber::with_default(Captured(events.clone()), run);
    let seen = std::mem::take(&mut *events.lock().unwrap());
    let warnings = seen
        .into_iter()
        .filter(|(level, _)| *level == tracing::Level::WARN)
        .map(|(_, line)| line)
        .collect();
    (result, warnings)
}

#[test]
fn pack_created_tables_are_classified_by_the_move() {
    let conn = store(&[
        GTD_AUDIT_DDL,
        EVAL_RUNS_DDL,
        SNAPSHOTS_DDL,
        EXEC_RUNS_DDL,
        EXEC_EVENTS_DDL,
        GIT_RECEIPTS_DDL,
        TOOL_POLICY_DDL,
        TOOL_GRANTS_DDL,
    ]);
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
        ("exec_runs", TableDisposition::NamespaceScopedAggregate),
        ("exec_events", TableDisposition::NamespaceScopedAggregate),
        ("git_receipts", TableDisposition::NamespaceScopedAggregate),
        ("tool_policy", TableDisposition::LeaveBehind),
        ("tool_grants", TableDisposition::LeaveBehind),
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

#[test]
fn a_total_move_carries_receipts_and_leaves_authorization_rows_where_they_are() {
    let conn = seeded_source();
    let policy_before = image(&conn, "tool_policy", "policy-live");
    let grant_before = image(&conn, "tool_grants", "grant-open");

    let request = total_request();
    let counts = move_namespace(&conn, &request).expect("a total single-target move");

    assert_eq!(counts.rows.get("exec_runs"), Some(&2));
    assert_eq!(counts.rows.get("exec_events"), Some(&3));
    assert_eq!(counts.rows.get("git_receipts"), Some(&1));
    assert_eq!(ns(&conn, "exec_runs", "run-1"), "target");
    assert_eq!(ns(&conn, "exec_runs", "run-2"), "target");
    assert_eq!(ns(&conn, "exec_runs", "run-other"), "elsewhere");
    assert_eq!(ns(&conn, "git_receipts", "receipt-1"), "target");
    assert_eq!(ns(&conn, "git_receipts", "receipt-other"), "elsewhere");
    assert_eq!(events_matching_their_run(&conn, "target"), 3);
    assert_eq!(events_matching_their_run(&conn, "source"), 0);

    assert!(!counts.rows.contains_key("tool_policy"));
    assert!(!counts.rows.contains_key("tool_grants"));
    assert_eq!(counts.left_behind.get("tool_policy"), Some(&3));
    assert_eq!(counts.left_behind.get("tool_grants"), Some(&4));
    assert_eq!(counts.live_policies_left_behind, 2);
    assert_eq!(counts.grants_in_force_left_behind, 2);
    assert_eq!(ns(&conn, "tool_policy", "policy-live"), "source");
    assert_eq!(ns(&conn, "tool_grants", "grant-open"), "source");
    assert_eq!(image(&conn, "tool_policy", "policy-live"), policy_before);
    assert_eq!(image(&conn, "tool_grants", "grant-open"), grant_before);
}

#[test]
fn a_partitioning_move_leaves_all_five_tables_and_reports_each_count() {
    let conn = seeded_source();
    // Two unexpired grants that were never in force: they stay and are counted
    // in `left_behind`, and the count of grants in force does not move.
    grant_with_status(&conn, "grant-undecided", "source", "requested");
    grant_with_status(&conn, "grant-revoked", "source", "revoked");
    let policy_before = image(&conn, "tool_policy", "policy-live");
    let grant_before = image(&conn, "tool_grants", "grant-open");

    let request = partitioning_request();
    let counts = move_namespace(&conn, &request).expect("a partitioning move");

    for (table, left) in [
        ("exec_runs", 2),
        ("exec_events", 3),
        ("git_receipts", 1),
        ("tool_policy", 3),
        ("tool_grants", 6),
    ] {
        assert_eq!(counts.left_behind.get(table), Some(&left), "{table}");
        assert!(!counts.rows.contains_key(table), "{table}");
    }
    assert_eq!(ns(&conn, "exec_runs", "run-1"), "source");
    assert_eq!(ns(&conn, "git_receipts", "receipt-1"), "source");
    assert_eq!(events_matching_their_run(&conn, "source"), 3);
    assert_eq!(counts.live_policies_left_behind, 2);
    assert_eq!(counts.grants_in_force_left_behind, 2);
    assert_eq!(image(&conn, "tool_policy", "policy-live"), policy_before);
    assert_eq!(image(&conn, "tool_grants", "grant-open"), grant_before);
}

#[test]
fn a_store_without_the_receipt_and_authorization_tables_moves_as_before() {
    let conn = store(&[]);
    seed_note(&conn, "n1", "source", "observation");

    let request = observation_request().at(NOW);
    let counts = move_namespace(&conn, &request).expect("absent pack tables are a no-op");

    assert_eq!(counts.subjects.get("note:observation"), Some(&1));
    assert!(counts.left_behind.is_empty(), "{:?}", counts.left_behind);
    assert_eq!(counts.live_policies_left_behind, 0);
    assert_eq!(counts.grants_in_force_left_behind, 0);
}

#[test]
fn leaving_live_authorization_rows_logs_one_warning_with_the_counts() {
    let conn = seeded_source();

    let request = total_request();
    let (outcome, warnings) = warnings_of(|| move_namespace(&conn, &request));
    outcome.expect("a total single-target move");

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    for expected in [
        "namespace=\"source\"",
        "live_policies=2",
        "grants_in_force=2",
    ] {
        assert!(
            warnings[0].contains(expected),
            "{expected} in {}",
            warnings[0]
        );
    }
    assert!(
        !warnings[0].contains("note-text-for-the-log-check"),
        "the line names no row contents: {}",
        warnings[0]
    );
}

#[test]
fn authorization_rows_that_are_all_dead_stay_without_a_warning() {
    let conn = store(&[TOOL_POLICY_DDL, TOOL_GRANTS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    policy(&conn, "policy-gone", "source", Some(5));
    grant(&conn, "grant-past", "source", Some(NOW - 1));

    let request = observation_request().at(NOW);
    let (outcome, warnings) = warnings_of(|| move_namespace(&conn, &request));
    let counts = outcome.expect("the move succeeds");

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(counts.left_behind.get("tool_policy"), Some(&1));
    assert_eq!(counts.left_behind.get("tool_grants"), Some(&1));
    assert_eq!(counts.live_policies_left_behind, 0);
    assert_eq!(counts.grants_in_force_left_behind, 0);
}

#[test]
fn grants_that_are_not_in_force_stay_without_a_warning() {
    let conn = store(&[TOOL_POLICY_DDL, TOOL_GRANTS_DDL]);
    seed_note(&conn, "n1", "source", "observation");
    policy(&conn, "policy-gone", "source", Some(5));
    grant_with_status(&conn, "grant-undecided", "source", "requested");
    grant_with_status(&conn, "grant-denied", "source", "denied");
    grant_with_status(&conn, "grant-revoked", "source", "revoked");
    grant(&conn, "grant-invalidated", "source", None);
    conn.execute(
        "UPDATE tool_grants SET invalidated_by_registry_id = 'registry-1', \
         invalidated_at = 7 WHERE id = 'grant-invalidated'",
        [],
    )
    .expect("invalidate grant");

    let request = observation_request().at(NOW);
    let (outcome, warnings) = warnings_of(|| move_namespace(&conn, &request));
    let counts = outcome.expect("the move succeeds");

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(counts.left_behind.get("tool_policy"), Some(&1));
    assert_eq!(counts.left_behind.get("tool_grants"), Some(&4));
    assert_eq!(counts.live_policies_left_behind, 0);
    assert_eq!(counts.grants_in_force_left_behind, 0);
}

#[test]
fn grant_expiry_is_judged_at_the_instant_the_request_carries() {
    for (now, want) in [(99, 1), (100, 0), (101, 0)] {
        let conn = store(&[TOOL_GRANTS_DDL]);
        grant(&conn, "grant-1", "source", Some(100));

        let request = observation_request().at(now);
        let counts = move_namespace(&conn, &request).expect("nothing is routed in this source");

        assert_eq!(counts.grants_in_force_left_behind, want, "now = {now}");
    }
}

#[test]
fn without_an_instant_grant_expiry_is_judged_at_the_wall_clock() {
    let conn = store(&[TOOL_GRANTS_DDL]);
    grant(&conn, "grant-old", "source", Some(1));
    grant(&conn, "grant-open", "source", None);

    let request = observation_request();
    let counts = move_namespace(&conn, &request).expect("nothing is routed in this source");

    assert_eq!(counts.left_behind.get("tool_grants"), Some(&2));
    assert_eq!(counts.grants_in_force_left_behind, 1);
}

/// `idx_exec_runs_session_seq` is a partial unique index, and the collision
/// pre-flight enumerates no partial index, so a clash on it is not named. The
/// move refuses as the failing statement itself, which the caller's rollback
/// then undoes. Same shape as the note-key arm in `namespace_move_fixture_tests`.
#[test]
fn an_exec_run_whose_session_seq_is_taken_in_the_target_fails_the_statement() {
    let conn = store(&PACK_TABLES_DDL);
    seed_note(&conn, "n1", "source", "observation");
    exec_run(&conn, "run-source", "source", 1);
    exec_run(&conn, "run-target", "target", 1);
    exec_event(&conn, "run-source", "source");
    let notes_before = image(&conn, "notes", "n1");
    let source_before = image(&conn, "exec_runs", "run-source");
    let target_before = image(&conn, "exec_runs", "run-target");

    let request = observation_request().at(NOW);
    conn.execute_batch("SAVEPOINT move").expect("savepoint");
    let outcome = move_namespace(&conn, &request);
    conn.execute_batch("ROLLBACK TO move").expect("rollback");
    conn.execute_batch("RELEASE move").expect("release");

    match outcome {
        Err(MoveError::Sqlite(error)) => {
            let message = error.to_string();
            assert!(
                message.contains("exec_runs.namespace, exec_runs.session_id, exec_runs.seq"),
                "the constraint raised it: {message}"
            );
        }
        Err(MoveError::Collisions { collisions }) => panic!(
            "the pre-flight now names partial-index clashes: upgrade this arm. {collisions:?}"
        ),
        other => panic!("expected the failing statement, got {other:?}"),
    }

    assert_eq!(ns(&conn, "notes", "n1"), "source");
    assert_eq!(image(&conn, "notes", "n1"), notes_before);
    assert_eq!(image(&conn, "exec_runs", "run-source"), source_before);
    assert_eq!(image(&conn, "exec_runs", "run-target"), target_before);
    assert_eq!(events_matching_their_run(&conn, "source"), 1);
    assert_eq!(events_matching_their_run(&conn, "target"), 0);
}
