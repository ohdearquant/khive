//! Measure mailbox query index candidates using the production SQL builder.

use super::{build_note_filter_where, note_filter_page_order_clause, NOTE_COLUMNS};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::SqlValue;
use rusqlite::{Connection, StatementStatus};
use serde_json::{json, Value};

// Comm pack indexes at the baseline of these planner measurements. Keeping the
// literal fixture local avoids a cross-crate source dependency in packaged tests.
const COMM_INDEXES: &str = "
CREATE INDEX idx_comm_message_direction
 ON notes(namespace, kind, json_extract(properties, '$.direction'),
 json_extract(properties, '$.read'), created_at DESC) WHERE deleted_at IS NULL;
CREATE INDEX idx_comm_message_thread
 ON notes(namespace, kind, json_extract(properties, '$.thread_id'), created_at DESC)
 WHERE deleted_at IS NULL;
CREATE INDEX idx_comm_message_to_actor
 ON notes(namespace, kind, json_extract(properties, '$.to_actor'),
 json_extract(properties, '$.direction'), json_extract(properties, '$.read'), created_at DESC)
 WHERE deleted_at IS NULL;
CREATE INDEX idx_comm_message_outbound_ref
 ON notes(namespace, kind, json_extract(properties, '$.direction'),
 json_extract(properties, '$.from_actor'), json_extract(properties, '$.outbound_ref'))
 WHERE deleted_at IS NULL;";

const RECIPIENT_ONLY: &str = "CREATE INDEX idx_candidate_recipient_only
 ON notes(namespace, kind, ifnull(json_extract(properties, '$.to_actor'), ''))
 WHERE deleted_at IS NULL";
const FULL_RECIPIENT: &str = "CREATE INDEX IF NOT EXISTS idx_notes_message_recipient_direction
 ON notes(namespace, kind, ifnull(json_extract(properties, '$.to_actor'), ''),
 json_extract(properties, '$.direction'), created_at DESC, id ASC)
 WHERE deleted_at IS NULL";

fn register_comm_indexes(conn: &Connection) {
    // Pack registration follows migrations at startup. Recreate these fixture
    // indexes to exercise that order instead of retaining an earlier catalog.
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_comm_message_direction;
         DROP INDEX IF EXISTS idx_comm_message_thread;
         DROP INDEX IF EXISTS idx_comm_message_to_actor;
         DROP INDEX IF EXISTS idx_comm_message_outbound_ref;",
    )
    .unwrap();
    conn.execute_batch(COMM_INDEXES).unwrap();
}

fn fixture(foreign_count: usize, read_backlog: usize) -> Connection {
    fixture_with_connection(
        Connection::open_in_memory().unwrap(),
        foreign_count,
        read_backlog,
        None,
    )
}

fn fixture_with_connection(
    conn: Connection,
    foreign_count: usize,
    read_backlog: usize,
    before_seed: Option<&str>,
) -> Connection {
    conn.execute_batch(include_str!("../../sql/notes-ddl.sql"))
        .unwrap();
    // A fresh schema may already contain the candidate; baseline measurements
    // must continue to describe the unfixed planner after that schema changes.
    conn.execute_batch("DROP INDEX IF EXISTS idx_notes_message_recipient_direction")
        .unwrap();
    if let Some(ddl) = before_seed {
        conn.execute_batch(ddl).unwrap();
    }
    register_comm_indexes(&conn);
    conn.execute_batch("BEGIN").unwrap();
    let mut next_id = 0usize;
    let mut insert = |properties: Value, created_at: usize| {
        next_id += 1;
        conn.execute(
            "INSERT INTO notes(id, namespace, kind, properties, created_at, updated_at)
             VALUES (?1, 'default', 'message', ?2, ?3, ?3)",
            rusqlite::params![
                format!("{next_id:036}"),
                properties.to_string(),
                created_at as i64
            ],
        )
        .unwrap();
    };
    for i in 0..80 {
        insert(
            json!({"direction":"inbound", "to_actor":"test:target",
                   "from_actor":"test:sender", "read":i % 2 == 0}),
            i,
        );
        insert(
            json!({"direction":"outbound", "to_actor":"test:target",
                   "from_actor":"test:sender", "read":false}),
            i,
        );
    }
    for i in 0..read_backlog {
        insert(
            json!({"direction":"inbound", "to_actor":"test:target",
                   "from_actor":"test:sender", "read":true}),
            i + 1000,
        );
    }
    // Caller-owned identities precede foreign rows so growth cannot change IDs.
    for i in 0..foreign_count {
        insert(
            json!({"direction":"inbound", "to_actor":"test:other",
                   "from_actor":"test:other", "read":i % 2 == 0}),
            i + 20_000,
        );
        insert(
            json!({"direction":"outbound", "to_actor":"test:other",
                   "from_actor":"test:other", "read":false}),
            i + 20_000,
        );
    }
    conn.execute_batch("COMMIT").unwrap();
    conn
}

fn inbox_filter(status: &str, raw_recipient: bool) -> NoteFilter {
    let mut filters = vec![PropertyFilter {
        json_path: "$.direction".into(),
        op: FilterOp::Eq,
        value: SqlValue::Text("inbound".into()),
    }];
    match status {
        "unread" | "read" => filters.push(PropertyFilter {
            json_path: "$.read".into(),
            op: if status == "unread" {
                FilterOp::JsonTypeNeMissing
            } else {
                FilterOp::JsonTypeEq
            },
            value: SqlValue::Text("true".into()),
        }),
        "all" => {}
        _ => panic!("unsupported fixture status"),
    }
    filters.push(PropertyFilter {
        json_path: "$.to_actor".into(),
        op: if raw_recipient {
            FilterOp::EqOrMissing
        } else {
            FilterOp::EqOrLegacyIndexed
        },
        value: SqlValue::Text("test:target".into()),
    });
    NoteFilter {
        kind: Some("message".into()),
        property_filters: filters,
        ..Default::default()
    }
}

fn sent_filter() -> NoteFilter {
    NoteFilter {
        kind: Some("message".into()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.direction".into(),
                op: FilterOp::Eq,
                value: SqlValue::Text("outbound".into()),
            },
            PropertyFilter {
                json_path: "$.from_actor".into(),
                op: FilterOp::Eq,
                value: SqlValue::Text("test:sender".into()),
            },
        ],
        ..Default::default()
    }
}

fn measure(conn: &Connection, filter: &NoteFilter) -> Value {
    let (where_sql, mut params) = build_note_filter_where("default", filter).unwrap();
    params.push(Box::new(21_i64));
    params.push(Box::new(0_i64));
    let sql = format!(
        "SELECT {NOTE_COLUMNS} FROM notes{where_sql}{} LIMIT ?{} OFFSET ?{}",
        note_filter_page_order_clause(filter),
        params.len() - 1,
        params.len(),
    );
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let plan: Vec<String> = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(refs.as_slice(), |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut statement = conn.prepare(&sql).unwrap();
    let ids: Vec<String> = statement
        .query_map(refs.as_slice(), |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    json!({"sql":sql, "plan":plan, "ids":ids,
           "vm_steps":statement.get_status(StatementStatus::VmStep)})
}

#[test]
fn comm_filter_query_plan_candidates_preserve_rows_and_record_unread_risk() {
    let mut all = Vec::new();
    let conn = fixture(6000, 10_000);
    let baseline: Vec<_> = ["unread", "read", "all"]
        .iter()
        .map(|status| measure(&conn, &inbox_filter(status, false)))
        .collect();
    for (candidate, ddl) in [
        ("base", None),
        ("recipient_only", Some(RECIPIENT_ONLY)),
        ("full", Some(FULL_RECIPIENT)),
    ] {
        let candidate_conn = fixture(6000, 10_000);
        if let Some(ddl) = ddl {
            candidate_conn.execute_batch(ddl).unwrap();
        }
        for (index, status) in ["unread", "read", "all"].iter().enumerate() {
            for raw in [false, true] {
                let result = measure(&candidate_conn, &inbox_filter(status, raw));
                assert_eq!(result["ids"], baseline[index]["ids"]);
                all.push(json!({"candidate":candidate, "status":status,
                                "raw_recipient":raw, "result":result}));
            }
        }
        all.push(json!({"candidate":candidate, "box":"sent",
                        "result":measure(&candidate_conn, &sent_filter())}));
    }
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(), "measurements":all})
    );
}

#[test]
fn comm_filter_query_plan_candidates_record_foreign_mailbox_growth() {
    let mut all = Vec::new();
    for (candidate, ddl) in [
        ("base", None),
        ("recipient_only", Some(RECIPIENT_ONLY)),
        ("full", Some(FULL_RECIPIENT)),
    ] {
        let mut previous_ids = Vec::new();
        for foreign_count in [0, 6000] {
            let conn = fixture(foreign_count, 0);
            if let Some(ddl) = ddl {
                conn.execute_batch(ddl).unwrap();
            }
            for (index, status) in ["unread", "read", "all", "sent"].iter().enumerate() {
                let filter = if *status == "sent" {
                    sent_filter()
                } else {
                    inbox_filter(status, false)
                };
                let result = measure(&conn, &filter);
                if foreign_count == 0 {
                    previous_ids.push(result["ids"].clone());
                } else {
                    assert_eq!(result["ids"], previous_ids[index]);
                }
                all.push(json!({"candidate":candidate, "foreign_count":foreign_count,
                                "status":status, "result":result}));
            }
        }
    }
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(), "growth":all})
    );
}

fn full_then_unread_ddl() -> String {
    let schema = include_str!("../../sql/notes-ddl.sql");
    let start = schema
        .find("CREATE INDEX IF NOT EXISTS idx_notes_unread_probe_recipient_direction")
        .unwrap();
    let end = schema[start..].find(';').unwrap() + start + 1;
    format!(
        "{FULL_RECIPIENT}; DROP INDEX idx_notes_unread_probe_recipient_direction; {}",
        &schema[start..end]
    )
}

fn assert_actor_seek(result: &Value, status: &str) {
    let plan = result["plan"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let expected_index = match status {
        "unread" => "idx_notes_unread_probe_recipient_direction",
        "sent" => "idx_comm_message_outbound_ref",
        _ => "idx_notes_message_recipient_direction",
    };
    assert!(plan.contains(expected_index), "{status}: {plan}");
    assert!(
        plan.contains("namespace=? AND kind=? AND <expr>=? AND <expr>=?"),
        "actor and direction must both constrain the {status} search: {plan}"
    );
}

#[test]
fn comm_filter_recreated_unread_index_preserves_fresh_reopen_and_analyzed_plans() {
    let migration = full_then_unread_ddl();
    let baseline_conn = fixture(6000, 10_000);
    let baseline: Vec<_> = ["unread", "read", "all", "sent"]
        .iter()
        .map(|status| {
            measure(
                &baseline_conn,
                &if *status == "sent" {
                    sent_filter()
                } else {
                    inbox_filter(status, false)
                },
            )
        })
        .collect();
    let unread_baseline_steps = baseline[0]["vm_steps"].as_u64().unwrap();
    let mut measurements = Vec::new();
    for fresh_schema in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mailbox.sqlite");
        let mut conn = fixture_with_connection(
            Connection::open(&path).unwrap(),
            6000,
            10_000,
            fresh_schema.then_some(migration.as_str()),
        );
        if !fresh_schema {
            conn.execute_batch(&migration).unwrap();
            register_comm_indexes(&conn);
        }
        for stage in [
            "fresh_connection",
            "reopened",
            "analyzed",
            "analyzed_reopened",
        ] {
            if stage.ends_with("reopened") {
                drop(conn);
                conn = Connection::open(&path).unwrap();
            }
            if stage == "analyzed" {
                conn.execute_batch("ANALYZE").unwrap();
            }
            for (index, status) in ["unread", "read", "all", "sent"].iter().enumerate() {
                let filter = if *status == "sent" {
                    sent_filter()
                } else {
                    inbox_filter(status, false)
                };
                let result = measure(&conn, &filter);
                assert_actor_seek(&result, status);
                assert_eq!(result["ids"], baseline[index]["ids"]);
                if *status == "unread" {
                    let steps = result["vm_steps"].as_u64().unwrap();
                    assert!(
                        steps <= unread_baseline_steps + 128,
                        "{fresh_schema}/{stage}: read-heavy unread work regressed ({unread_baseline_steps} -> {steps})"
                    );
                }
                measurements.push(json!({"fresh_schema":fresh_schema,"stage":stage,"status":status,"result":result}));
            }
        }
    }
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(),"migration":migration,"baseline":baseline,"lifecycle":measurements})
    );
}

#[test]
#[ignore = "planner proof for the recreated unread-probe index; the production index and its migration are a follow-up"]
fn comm_filter_recreated_unread_index_bounds_work_as_other_mailboxes_grow() {
    let migration = full_then_unread_ddl();
    let mut measurements = Vec::new();
    for fresh_schema in [true, false] {
        let mut small_results = Vec::new();
        for foreign_count in [0, 6000] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("mailbox.sqlite");
            let mut conn = fixture_with_connection(
                Connection::open(&path).unwrap(),
                foreign_count,
                0,
                fresh_schema.then_some(migration.as_str()),
            );
            if !fresh_schema {
                conn.execute_batch(&migration).unwrap();
                register_comm_indexes(&conn);
            }
            for (stage_index, stage) in [
                "fresh_connection",
                "reopened",
                "analyzed",
                "analyzed_reopened",
            ]
            .iter()
            .enumerate()
            {
                if stage.ends_with("reopened") {
                    drop(conn);
                    conn = Connection::open(&path).unwrap();
                }
                if *stage == "analyzed" {
                    conn.execute_batch("ANALYZE").unwrap();
                }
                for (index, status) in ["unread", "read", "all", "sent"].iter().enumerate() {
                    let filter = if *status == "sent" {
                        sent_filter()
                    } else {
                        inbox_filter(status, false)
                    };
                    let result = measure(&conn, &filter);
                    assert_actor_seek(&result, status);
                    if foreign_count == 0 {
                        small_results.push(result.clone());
                    } else {
                        let baseline: &Value = &small_results[stage_index * 4 + index];
                        assert_eq!(result["ids"], baseline["ids"]);
                        let steps = result["vm_steps"].as_u64().unwrap();
                        let baseline_steps = baseline["vm_steps"].as_u64().unwrap();
                        assert!(steps <= baseline_steps + 128,
                            "{fresh_schema}/{stage}/{status}: foreign mailbox growth must not add row-proportional work ({baseline_steps} -> {steps})");
                    }
                    measurements.push(json!({"fresh_schema":fresh_schema,"stage":stage,"foreign_count":foreign_count,"status":status,"result":result}));
                }
            }
        }
    }
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(),"bounded_growth":measurements})
    );
}
