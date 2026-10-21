use super::*;

use chrono::{TimeZone, Utc};
use khive_storage::types::Edge;
use khive_types::EdgeRelation;
use rusqlite::types::Value;
use uuid::Uuid;

pub(super) const MIGRATION_SQL: &str = include_str!("../../sql/061-edge-governance.sql");

pub(super) const DECISION_INSERT: &str = "INSERT INTO edge_governance_decisions
    (decision_id,edge_id,edge_namespace,source_id,target_id,relation,target_backend,
     disposition,via,authorizer_kind,authorizer_id,applied_by,authority_action,
     authority_subject_id,provider_id,provider_revision,proposal_id,review_event_id,
     decided_at,reason_code)
    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)";

pub(super) fn fixture_edge() -> Edge {
    let now = Utc.timestamp_micros(100).single().unwrap();
    Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id: Uuid::new_v4(),
        target_id: Uuid::new_v4(),
        relation: EdgeRelation::Supersedes,
        weight: 0.5,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    }
}

fn decision_values(edge: &Edge, decision_id: &str) -> Vec<Value> {
    vec![
        Value::Text(decision_id.into()),
        Value::Text(Uuid::from(edge.id).to_string()),
        Value::Text(edge.namespace.clone()),
        Value::Text(edge.source_id.to_string()),
        Value::Text(edge.target_id.to_string()),
        Value::Text(edge.relation.to_string()),
        Value::Null,
        Value::Text("authorized".into()),
        Value::Text("migration_review".into()),
        Value::Text("agent".into()),
        Value::Text("fixture-actor".into()),
        Value::Null,
        Value::Text("memory.supersede".into()),
        Value::Text(edge.target_id.to_string()),
        Value::Text("fixture-policy".into()),
        Value::Text("1".into()),
        Value::Null,
        Value::Text("fixture-review-event".into()),
        Value::Integer(100),
        Value::Text("migration_authorized".into()),
    ]
}

pub(super) fn insert_decision(
    conn: &Connection,
    edge: &Edge,
    decision_id: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        DECISION_INSERT,
        rusqlite::params_from_iter(decision_values(edge, decision_id)),
    )
}

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

fn migrated() -> Connection {
    let mut conn = open_memory();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    run_migrations(&mut conn).unwrap();
    conn
}

#[test]
fn edge_governance_fresh_schema_is_registered_and_inactive() {
    assert_eq!(
        MIGRATIONS
            .iter()
            .find(|migration| migration.version == 61)
            .map(|migration| (migration.name, migration.up)),
        Some(("edge_governance", MIGRATION_SQL)),
        "V61 must be in the actual migration registry"
    );
    let conn = migrated();
    let columns: [(&str, &[&str]); 4] = [
        (
            "edge_governance_decisions",
            &[
                "decision_id",
                "edge_id",
                "edge_namespace",
                "source_id",
                "target_id",
                "relation",
                "target_backend",
                "disposition",
                "via",
                "authorizer_kind",
                "authorizer_id",
                "applied_by",
                "authority_action",
                "authority_subject_id",
                "provider_id",
                "provider_revision",
                "proposal_id",
                "review_event_id",
                "decided_at",
                "reason_code",
            ],
        ),
        (
            "edge_governance_invalidations",
            &[
                "invalidation_id",
                "decision_id",
                "edge_id",
                "cause",
                "invalidated_at",
            ],
        ),
        (
            "edge_governance_active",
            &[
                "edge_id",
                "decision_id",
                "edge_namespace",
                "source_id",
                "target_id",
                "relation",
                "target_backend",
                "stamped_at",
            ],
        ),
        (
            "edge_governance_state",
            &[
                "singleton",
                "schema_version",
                "status",
                "activation_epoch",
                "activated_at",
                "inventoried_edges",
                "authorized_edges",
                "rejected_edges",
                "late_arrival_edges",
                "provider_mode",
                "provider_version",
                "trigger_manifest",
            ],
        ),
    ];
    for (table, expected) in columns {
        assert!(table_exists(&conn, table), "missing {table}");
        let actual = conn
            .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
            .unwrap()
            .query_map([table], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(actual, expected, "{table} columns");
    }
    assert_eq!(
        rows(&conn, "SELECT name FROM pragma_index_info('idx_edge_governance_active_target') ORDER BY seqno"),
        vec![
            vec![Value::Text("target_id".into())],
            vec![Value::Text("source_id".into())],
            vec![Value::Text("edge_id".into())],
        ]
    );
    assert_eq!(
        rows(&conn, "SELECT * FROM edge_governance_state"),
        vec![vec![
            Value::Integer(1),
            Value::Integer(61),
            Value::Text("inactive".into()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
    assert!(conn
        .execute(
            "INSERT INTO edge_governance_state SELECT * FROM edge_governance_state",
            []
        )
        .is_err());
}

#[test]
fn edge_governance_upgrade_reopen_and_replay_preserve_existing_rows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("governance.db");
    let mut conn = Connection::open(&path).unwrap();
    let admission =
        WriteAdmission::for_migration_policy(Some(path.clone()), &migration_test_policy()).unwrap();
    {
        let mut writes = RawMigrationTransactions::new(&mut conn, &admission);
        writes
            .admitted(|conn| bootstrap_migration_ledger(conn, &admission))
            .unwrap();
        for migration in MIGRATIONS.iter().filter(|migration| migration.version < 61) {
            assert!(matches!(
                writes
                    .admitted(|conn| apply_versioned_migration(
                        conn,
                        migration,
                        &admission,
                        latest_schema_version()
                    ))
                    .unwrap(),
                MigrationStep::Applied
            ));
        }
    }
    assert_eq!(read_schema_version(&conn).unwrap(), 60);
    conn.execute_batch("INSERT INTO notes(id,namespace,kind,content,properties,created_at,updated_at)
        VALUES ('old-note','local','memory','unchanged', '{ \"keep\" : [1, 2] }',11,12);
        INSERT INTO graph_edges(namespace,id,source_id,target_id,relation,created_at,updated_at,metadata)
        VALUES ('other','old-edge','old-note','old-target','supersedes',21,22,'{ \"keep\" : true }');
        INSERT INTO events(id,namespace,verb,substrate,actor,outcome,kind,payload,created_at)
        VALUES ('old-event','local','link','entity','fixture-actor','success','edge_created','{ \"keep\" : 3 }',31);").unwrap();
    let notes = rows(&conn, "SELECT rowid,* FROM notes ORDER BY id");
    let edges = rows(&conn, "SELECT rowid,* FROM graph_edges ORDER BY id");
    let events = rows(&conn, "SELECT rowid,* FROM events ORDER BY id");
    let history = rows(&conn, "SELECT * FROM _schema_migrations ORDER BY version");
    run_migrations(&mut conn).unwrap();
    assert!(table_exists(&conn, "edge_governance_active"));
    assert_eq!(read_schema_version(&conn).unwrap(), 61);
    assert_eq!(rows(&conn, "SELECT rowid,* FROM notes ORDER BY id"), notes);
    assert_eq!(
        rows(&conn, "SELECT rowid,* FROM graph_edges ORDER BY id"),
        edges
    );
    assert_eq!(
        rows(&conn, "SELECT rowid,* FROM events ORDER BY id"),
        events
    );
    assert_eq!(
        rows(
            &conn,
            "SELECT * FROM _schema_migrations WHERE version < 61 ORDER BY version"
        ),
        history
    );
    // Fixture-only active receipt and spent history must survive a tail replay.
    // This exercises row preservation, not the deferred activation protocol.
    insert_decision(&conn, &fixture_edge(), "replay-decision").unwrap();
    conn.execute_batch(
        "INSERT INTO edge_governance_invalidations
            (decision_id,edge_id,cause,invalidated_at)
         SELECT decision_id,edge_id,'hard_delete',101 FROM edge_governance_decisions;
         UPDATE edge_governance_state SET status='active',activation_epoch=7,
            activated_at=100,inventoried_edges=1,authorized_edges=1,rejected_edges=0,
            late_arrival_edges=0,provider_mode='single-principal',
            provider_version='fixture-only',trigger_manifest='fixture-only'",
    )
    .unwrap();
    let decisions = rows(&conn, "SELECT rowid,* FROM edge_governance_decisions");
    let invalidations = rows(&conn, "SELECT rowid,* FROM edge_governance_invalidations");
    let state = rows(&conn, "SELECT * FROM edge_governance_state");
    let schema = rows(&conn, "SELECT type,name,sql FROM sqlite_schema WHERE name LIKE '%edge_governance%' ORDER BY type,name");
    conn.close().unwrap();
    let mut reopened = Connection::open(&path).unwrap();
    // Rewind only the migration ledger, as older-tail replay tests do; the
    // installed schema and governance rows already exist when V61 runs again.
    reopened
        .execute("DELETE FROM _schema_migrations WHERE version=61", [])
        .unwrap();
    let schema_version: i64 = reopened
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .unwrap();
    run_migrations(&mut reopened).unwrap();
    assert_eq!(
        reopened
            .pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        schema_version,
        "migration replay must not alter the installed schema"
    );
    assert_eq!(
        rows(&reopened, "SELECT rowid,* FROM edge_governance_decisions"),
        decisions
    );
    assert_eq!(
        rows(
            &reopened,
            "SELECT rowid,* FROM edge_governance_invalidations"
        ),
        invalidations
    );
    assert_eq!(
        rows(&reopened, "SELECT * FROM edge_governance_state"),
        state
    );
    assert_eq!(rows(&reopened, "SELECT type,name,sql FROM sqlite_schema WHERE name LIKE '%edge_governance%' ORDER BY type,name"), schema);
    assert_eq!(
        rows(&reopened, "SELECT rowid,* FROM notes ORDER BY id"),
        notes
    );
    assert_eq!(
        rows(&reopened, "SELECT rowid,* FROM graph_edges ORDER BY id"),
        edges
    );
    assert_eq!(
        rows(&reopened, "SELECT rowid,* FROM events ORDER BY id"),
        events
    );
    assert_eq!(
        rows(
            &reopened,
            "SELECT version,name FROM _schema_migrations WHERE version=61"
        ),
        vec![vec![
            Value::Integer(61),
            Value::Text("edge_governance".into())
        ]]
    );
}

#[test]
fn edge_governance_evidence_constraints_reject_unknown_or_incomplete_values() {
    let conn = migrated();
    let edge = fixture_edge();
    insert_decision(&conn, &edge, "valid").unwrap();
    let invalid = [
        (5, Value::Text("extends".into())),
        (6, Value::Text("remote".into())),
        (7, Value::Text("unknown".into())),
        (8, Value::Text("unknown".into())),
        (17, Value::Null),
        (9, Value::Text(String::new())),
        (10, Value::Text(String::new())),
        (12, Value::Text("other.action".into())),
        (13, Value::Text(String::new())),
        (14, Value::Text(String::new())),
        (15, Value::Text(String::new())),
        (19, Value::Text("unknown_reason".into())),
    ];
    for (index, value) in invalid {
        let mut values = decision_values(&edge, "invalid");
        values[index] = value;
        let error = conn
            .execute(DECISION_INSERT, rusqlite::params_from_iter(values))
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation),
            "column {index}"
        );
    }
    let mut reviewed = decision_values(&edge, "reviewed");
    reviewed[8] = Value::Text("reviewed_proposal".into());
    reviewed[17] = Value::Text("fixture-review-event".into());
    conn.execute(DECISION_INSERT, rusqlite::params_from_iter(reviewed))
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT COUNT(*) FROM edge_governance_decisions"),
        vec![vec![Value::Integer(2)]]
    );
    assert!(conn.execute("INSERT INTO edge_governance_invalidations(decision_id,edge_id,cause,invalidated_at) VALUES ('missing','edge','hard_delete',1)", []).is_err());
    assert!(conn.execute("INSERT INTO edge_governance_invalidations(decision_id,edge_id,cause,invalidated_at) VALUES ('valid','edge','unknown',1)", []).is_err());
    // Closed vocabulary, including the reserved/unemitted merge cause. No
    // UNIQUE(decision_id): the append-only object is an event ledger.
    for cause in [
        "endpoint_or_relation_or_backend_rewrite",
        "merge_rewire",
        "edge_liveness_transition",
        "endpoint_note_liveness_transition",
        "hard_delete",
        "revoked_by_decision",
    ] {
        conn.execute("INSERT INTO edge_governance_invalidations(decision_id,edge_id,cause,invalidated_at) VALUES ('valid','edge',?1,1)", [cause]).unwrap();
    }
    assert_eq!(
        rows(
            &conn,
            "SELECT COUNT(*) FROM edge_governance_invalidations WHERE decision_id='valid'"
        ),
        vec![vec![Value::Integer(6)]]
    );
}

#[test]
fn edge_governance_decision_and_invalidation_history_is_append_only() {
    let conn = migrated();
    insert_decision(&conn, &fixture_edge(), "history").unwrap();
    // No FK may mask the decisions_no_delete trigger omission.
    insert_decision(&conn, &fixture_edge(), "unreferenced-history").unwrap();
    conn.execute("INSERT INTO edge_governance_invalidations(decision_id,edge_id,cause,invalidated_at) VALUES ('history','edge','hard_delete',123)", []).unwrap();
    let decisions = rows(&conn, "SELECT * FROM edge_governance_decisions");
    let invalidations = rows(&conn, "SELECT * FROM edge_governance_invalidations");
    for sql in [
        "UPDATE edge_governance_decisions SET reason_code='revocation'",
        "DELETE FROM edge_governance_decisions WHERE decision_id='unreferenced-history'",
        "UPDATE edge_governance_invalidations SET invalidated_at=456",
        "DELETE FROM edge_governance_invalidations",
    ] {
        assert!(
            conn.execute(sql, []).is_err(),
            "history mutation accepted: {sql}"
        );
        assert_eq!(
            rows(&conn, "SELECT * FROM edge_governance_decisions"),
            decisions
        );
        assert_eq!(
            rows(&conn, "SELECT * FROM edge_governance_invalidations"),
            invalidations
        );
    }
}

#[test]
fn edge_governance_reason_vocabulary_matches_accepted_contract() {
    let conn = migrated();
    let edge = fixture_edge();
    // ADR046 Amendment 2:914–918 plus ADR159 §6:389–392.
    for (index, reason) in [
        "migration_authorized",
        "migration_rejected_unowned",
        "migration_rejected_disputed",
        "revocation",
        "late_arrival_unreviewed",
    ]
    .into_iter()
    .enumerate()
    {
        let mut values = decision_values(&edge, &format!("reason-{index}"));
        values[19] = Value::Text(reason.into());
        conn.execute(DECISION_INSERT, rusqlite::params_from_iter(values))
            .unwrap();
    }
    for reason in ["authorized", "rejected", "unknown_reason"] {
        let mut values = decision_values(&edge, "unsupported-reason");
        values[19] = Value::Text(reason.into());
        let error = conn
            .execute(DECISION_INSERT, rusqlite::params_from_iter(values))
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }
    assert_eq!(
        rows(&conn, "SELECT COUNT(*) FROM edge_governance_decisions"),
        vec![vec![Value::Integer(5)]]
    );
}

#[test]
fn edge_governance_state_requires_complete_receipt_and_inactive_nulls() {
    let conn = migrated();
    let fields = [
        "activation_epoch",
        "activated_at",
        "inventoried_edges",
        "authorized_edges",
        "rejected_edges",
        "late_arrival_edges",
        "provider_mode",
        "provider_version",
        "trigger_manifest",
    ];
    let inactive = rows(&conn, "SELECT * FROM edge_governance_state");
    assert!(conn
        .execute("UPDATE edge_governance_state SET status='active'", [])
        .is_err());
    for field in fields {
        let value = match field {
            "provider_mode" => Value::Text("single-principal".into()),
            "provider_version" | "trigger_manifest" => Value::Text("fixture-only".into()),
            _ => Value::Integer(0),
        };
        assert!(
            conn.execute(
                &format!("UPDATE edge_governance_state SET {field}=?1"),
                [value]
            )
            .is_err(),
            "inactive receipt must keep {field} NULL"
        );
        assert_eq!(rows(&conn, "SELECT * FROM edge_governance_state"), inactive);
    }
    // Fixture-only row-shape test: this does not implement activation, validate
    // authority, or establish that the deliberately synthetic receipt is valid.
    conn.execute_batch(
        "UPDATE edge_governance_state SET status='active',activation_epoch=1,
        activated_at=100,inventoried_edges=0,authorized_edges=0,rejected_edges=0,
        late_arrival_edges=0,provider_mode='single-principal',provider_version='fixture-only',
        trigger_manifest='fixture-only'",
    )
    .unwrap();
    let active = rows(&conn, "SELECT * FROM edge_governance_state");
    for field in fields {
        assert!(
            conn.execute(
                &format!("UPDATE edge_governance_state SET {field}=NULL"),
                []
            )
            .is_err(),
            "active receipt requires {field}"
        );
        assert_eq!(rows(&conn, "SELECT * FROM edge_governance_state"), active);
    }
}
