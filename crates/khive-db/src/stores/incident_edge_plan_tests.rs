//! Hard-delete incident-edge statements: the namespace walk finds the same
//! edges as a scan by endpoint, and its work does not follow the size of the
//! edge table.

use super::{purge_incident_edges_statement, GRAPH_DDL};
use crate::sql_bridge::bind_params;
use crate::stores::event::hard_delete_lineage_warning_statements;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::SubstrateKind;
use rusqlite::{Connection, StatementStatus};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use uuid::Uuid;

const EVENTS_DDL: &str = include_str!("../../sql/events-ddl.sql");

/// The endpoint scans the statements used before the namespace walk, kept
/// here as the reference result.
const SCAN_PURGE: &str = "DELETE FROM graph_edges WHERE source_id = ?1 OR target_id = ?1";
const SCAN_INCIDENT: &str = "SELECT id, namespace, source_id, target_id, deleted_at \
     FROM graph_edges WHERE (source_id = ?1 OR target_id = ?1) AND relation = ?2 \
     ORDER BY namespace, id";

const NAMESPACES: [&str; 3] = ["ns-a", "ns-b", "ns-c"];
const RELATIONS: [&str; 6] = [
    "derived_from",
    "supersedes",
    "precedes",
    "supports",
    "refutes",
    "enables",
];

fn deleted_node() -> Uuid {
    Uuid::from_u128(0x0dd0_0000_0000_4000_8000_0000_0000_0001)
}

fn insert_edge(
    conn: &Connection,
    namespace: &str,
    id: &str,
    source: &str,
    target: &str,
    relation: &str,
    deleted_at: Option<i64>,
) {
    conn.execute(
        "INSERT INTO graph_edges \
         (namespace, id, source_id, target_id, relation, weight, created_at, updated_at, deleted_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, 1.0, 1, 1, ?6)",
        rusqlite::params![namespace, id, source, target, relation, deleted_at],
    )
    .unwrap();
}

/// Edges incident to [`deleted_node`] in two of three namespaces (as source,
/// as target, a self-loop, a tombstoned edge and a relation without a
/// warning), plus `unrelated_per_namespace` edges in every namespace that do
/// not touch it.
fn fixture(unrelated_per_namespace: usize, analyze: bool) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(GRAPH_DDL).unwrap();
    conn.execute_batch(EVENTS_DDL).unwrap();
    let x = deleted_node().to_string();
    for (namespace, id, source, target, relation, deleted_at) in [
        ("ns-a", "in-1", x.as_str(), "a-1", "derived_from", None),
        ("ns-a", "in-2", "a-2", x.as_str(), "supports", None),
        ("ns-a", "in-3", x.as_str(), x.as_str(), "precedes", None),
        ("ns-a", "in-4", "a-3", x.as_str(), "derived_from", None),
        ("ns-b", "in-5", x.as_str(), "b-1", "refutes", Some(7)),
        ("ns-b", "in-6", "b-2", x.as_str(), "supersedes", None),
        ("ns-b", "in-7", x.as_str(), "b-3", "enables", None),
    ] {
        insert_edge(&conn, namespace, id, source, target, relation, deleted_at);
    }
    for namespace in NAMESPACES {
        for i in 0..unrelated_per_namespace {
            insert_edge(
                &conn,
                namespace,
                &format!("u-{namespace}-{i}"),
                &format!("n-{i}"),
                &format!("n-{}", i + 1),
                RELATIONS[i % RELATIONS.len()],
                (i % 50 == 0).then_some(9),
            );
        }
    }
    if analyze {
        conn.execute_batch("ANALYZE").unwrap();
    }
    conn
}

/// Execute one statement and return (rows changed, SQLite VM steps).
fn execute(conn: &Connection, sql: &str, params: &[SqlValue]) -> (usize, i32) {
    let mut stmt = conn.prepare(sql).unwrap();
    bind_params(&mut stmt, params).unwrap();
    let changed = stmt.raw_execute().unwrap();
    (changed, stmt.get_status(StatementStatus::VmStep))
}

fn run_all(conn: &Connection, statements: &[SqlStatement]) -> i32 {
    statements
        .iter()
        .map(|statement| execute(conn, &statement.sql, &statement.params).1)
        .sum()
}

fn remaining_edges(conn: &Connection) -> BTreeSet<(String, String)> {
    let mut stmt = conn
        .prepare("SELECT namespace, id FROM graph_edges")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn scan_incident(conn: &Connection, node: &str, relation: &str) -> Vec<Value> {
    let mut stmt = conn.prepare(SCAN_INCIDENT).unwrap();
    stmt.query_map(rusqlite::params![node, relation], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "namespace": row.get::<_, String>(1)?,
            "source_id": row.get::<_, String>(2)?,
            "target_id": row.get::<_, String>(3)?,
            "deleted_at": row.get::<_, Option<i64>>(4)?,
        }))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// (relation, edge_count, edges) of every warning event the statements wrote.
fn warnings_written(conn: &Connection) -> Vec<(String, i64, Value)> {
    let mut stmt = conn
        .prepare("SELECT payload FROM events ORDER BY json_extract(payload, '$.relation')")
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|payload| {
            let payload: Value = serde_json::from_str(&payload.unwrap()).unwrap();
            (
                payload["relation"].as_str().unwrap().to_string(),
                payload["edge_count"].as_i64().unwrap(),
                payload["edges"].clone(),
            )
        })
        .collect()
}

#[test]
fn incident_statements_find_the_edges_the_endpoint_scan_finds() {
    let x = deleted_node();
    let node = x.to_string();
    for analyze in [false, true] {
        let walked = fixture(200, analyze);
        let warnings =
            hard_delete_lineage_warning_statements("ns-a", "agent:test", x, SubstrateKind::Entity);
        run_all(&walked, &warnings);

        // Every warning relation with an incident edge writes one event whose
        // edges are exactly the scan's rows, in the scan's order; relations
        // without one write nothing.
        let mut expected = Vec::new();
        for statement in &warnings {
            let SqlValue::Text(relation) = &statement.params[8] else {
                panic!("relation parameter");
            };
            let rows = scan_incident(&walked, &node, relation);
            if !rows.is_empty() {
                expected.push((relation.clone(), rows.len() as i64, Value::Array(rows)));
            }
        }
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(warnings_written(&walked), expected, "analyze={analyze}");
        assert_eq!(expected.len(), 5, "fixture covers every warning relation");

        // The purge removes the same edges as the endpoint scan, the self-loop once.
        let scanned = fixture(200, analyze);
        let (scan_removed, _) = execute(&scanned, SCAN_PURGE, &[SqlValue::Text(node.clone())]);
        let purge = purge_incident_edges_statement(x);
        let (walk_removed, _) = execute(&walked, &purge.sql, &purge.params);
        assert_eq!(walk_removed, scan_removed, "analyze={analyze}");
        assert_eq!(walk_removed, 7);
        assert_eq!(remaining_edges(&walked), remaining_edges(&scanned));
    }

    // A node with no incident edge, and an empty table.
    let walked = fixture(50, false);
    let other = Uuid::from_u128(42);
    let warnings =
        hard_delete_lineage_warning_statements("ns-a", "agent:test", other, SubstrateKind::Note);
    run_all(&walked, &warnings);
    assert!(warnings_written(&walked).is_empty());
    let purge = purge_incident_edges_statement(other);
    assert_eq!(execute(&walked, &purge.sql, &purge.params).0, 0);
    let empty = fixture(0, false);
    empty.execute_batch("DELETE FROM graph_edges").unwrap();
    let purge = purge_incident_edges_statement(x);
    assert_eq!(execute(&empty, &purge.sql, &purge.params).0, 0);
}

#[test]
fn incident_statement_work_does_not_follow_the_edge_table() {
    let x = deleted_node();
    for analyze in [false, true] {
        let mut walk_steps = Vec::new();
        let mut scan_steps = Vec::new();
        for unrelated in [100, 10_000] {
            let conn = fixture(unrelated, analyze);
            let mut steps = run_all(
                &conn,
                &hard_delete_lineage_warning_statements(
                    "ns-a",
                    "agent:test",
                    x,
                    SubstrateKind::Entity,
                ),
            );
            let purge = purge_incident_edges_statement(x);
            steps += execute(&conn, &purge.sql, &purge.params).1;
            walk_steps.push(steps);

            let control = fixture(unrelated, analyze);
            let node = SqlValue::Text(x.to_string());
            let mut steps = 0;
            for relation in &RELATIONS[..5] {
                let mut stmt = control.prepare(SCAN_INCIDENT).unwrap();
                bind_params(
                    &mut stmt,
                    &[node.clone(), SqlValue::Text((*relation).to_string())],
                )
                .unwrap();
                let mut rows = stmt.raw_query();
                while rows.next().unwrap().is_some() {}
                drop(rows);
                steps += stmt.get_status(StatementStatus::VmStep);
            }
            steps += execute(&control, SCAN_PURGE, &[node]).1;
            scan_steps.push(steps);
        }
        println!("analyze={analyze} walk steps {walk_steps:?} scan steps {scan_steps:?}");
        // Control: without statistics, which is how the stores run (nothing
        // runs ANALYZE), the endpoint scan reads every edge, so 100x more
        // unrelated edges cost it far more work. With statistics SQLite can
        // skip-scan the namespace-leading indexes instead, so the control
        // only applies without them.
        if !analyze {
            assert!(
                scan_steps[1] > scan_steps[0] * 10,
                "analyze={analyze}: {scan_steps:?}"
            );
        }
        // The namespace walk seeks: three namespaces either way, the same
        // incident edges, so its work stays within a few index levels.
        assert!(
            walk_steps[1] <= walk_steps[0] + 512,
            "analyze={analyze}: {walk_steps:?}"
        );
    }
}
