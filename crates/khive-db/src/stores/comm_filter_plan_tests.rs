//! Measure mailbox query index candidates using the production SQL builder.

use super::{
    build_note_filter_read_clause, build_note_filter_where, comm_filter_index_clause,
    note_filter_page_order_clause, NOTE_COLUMNS,
};
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
 WHERE deleted_at IS NULL;
CREATE INDEX idx_comm_message_outbound_recipient
 ON notes(namespace, kind, json_extract(properties, '$.direction'),
 json_extract(properties, '$.to_actor'), created_at DESC, id ASC)
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
         DROP INDEX IF EXISTS idx_comm_message_outbound_ref;
         DROP INDEX IF EXISTS idx_comm_message_outbound_recipient;",
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

/// The channel delivery loops' scan: pending predicate plus the channel
/// prefix on the recipient, all in the statement, newest-first. The prefix
/// selects the caller-owned `test:target` rows and excludes `test:other`.
fn outbox_filter() -> NoteFilter {
    NoteFilter {
        kind: Some("message".into()),
        property_filters: vec![
            PropertyFilter {
                json_path: "$.direction".into(),
                op: FilterOp::Eq,
                value: SqlValue::Text("outbound".into()),
            },
            PropertyFilter {
                json_path: "$.delivered_at".into(),
                op: FilterOp::JsonTypeMissingOrNullIndexed,
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.delivery".into(),
                op: FilterOp::NotInOrMissing(vec![
                    SqlValue::Text("delivered".into()),
                    SqlValue::Text("failed".into()),
                ]),
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.to_actor".into(),
                op: FilterOp::TextStartsWithIndexed,
                value: SqlValue::Text("test:t".into()),
            },
        ],
        ..Default::default()
    }
}

fn measure(conn: &Connection, filter: &NoteFilter) -> Value {
    measure_with_pin(conn, filter, true)
}

fn measure_unpinned(conn: &Connection, filter: &NoteFilter) -> Value {
    measure_with_pin(conn, filter, false)
}

fn measure_with_pin(conn: &Connection, filter: &NoteFilter, pinned: bool) -> Value {
    let (where_sql, mut params) = if pinned {
        build_note_filter_read_clause("default", filter).unwrap()
    } else {
        build_note_filter_where("default", filter).unwrap()
    };
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

/// The scan seeks the outbound-recipient index on (direction, recipient
/// prefix range). Ordering by `created_at` after a range seek needs a sort,
/// but that sort covers only the channel's own pending rows, never the
/// actor-to-actor backlog the prefix excludes.
#[test]
fn outbox_filter_plan_seeks_the_outbound_recipient_index() {
    let result = measure(&fixture(0, 10_000), &outbox_filter());
    let plan = result["plan"]
        .as_array()
        .unwrap()
        .iter()
        .map(|detail| detail.as_str().unwrap())
        .collect::<Vec<_>>();

    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_comm_message_outbound_recipient")),
        "outbox filter must seek the outbound recipient index, got plan: {plan:?}"
    );
    assert_eq!(
        result["ids"].as_array().unwrap().len(),
        21,
        "the page holds only caller-owned outbound rows"
    );
}

/// Pending rows addressed to other recipients (every actor-to-actor message
/// stays pending forever) must not add work to a channel's scan: 10,000
/// foreign outbound rows leave the ids and the VM step count unchanged.
#[test]
fn outbox_filter_work_is_bounded_by_the_channels_own_rows() {
    let small = measure(&fixture(0, 0), &outbox_filter());
    let large = measure(&fixture(10_000, 0), &outbox_filter());

    assert_eq!(large["ids"], small["ids"]);
    let small_steps = small["vm_steps"].as_i64().unwrap();
    let large_steps = large["vm_steps"].as_i64().unwrap();
    assert!(
        large_steps <= small_steps + 128,
        "foreign outbound growth must not add row-proportional outbox work: {small_steps} -> {large_steps}"
    );
}

#[test]
fn comm_filter_query_plan_candidates_preserve_rows_and_record_unread_risk() {
    let mut all = Vec::new();
    let conn = fixture(6000, 10_000);
    let baseline: Vec<_> = ["unread", "read", "all"]
        .iter()
        .map(|status| measure_unpinned(&conn, &inbox_filter(status, false)))
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
                let result = measure_unpinned(&candidate_conn, &inbox_filter(status, raw));
                assert_eq!(result["ids"], baseline[index]["ids"]);
                all.push(json!({"candidate":candidate, "status":status,
                                "raw_recipient":raw, "result":result}));
            }
        }
        all.push(json!({"candidate":candidate, "box":"sent",
                        "result":measure_unpinned(&candidate_conn, &sent_filter())}));
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
                let result = measure_unpinned(&conn, &filter);
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
    include_str!("../../sql/033-notes-message-recipient-direction.sql").to_owned()
}

fn assert_inbox_actor_seek(result: &Value, status: &str) {
    let plan = result["plan"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let expected_index = match status {
        "unread" => "idx_notes_unread_probe_recipient_direction",
        "read" | "all" => "idx_notes_message_recipient_direction",
        _ => panic!("recipient seek assertion requires an inbox status: {status}"),
    };
    assert!(plan.contains(expected_index), "{status}: {plan}");
    assert!(
        plan.contains("namespace=? AND kind=? AND <expr>=? AND <expr>=?"),
        "actor and direction must both constrain the {status} search: {plan}"
    );
}

#[test]
fn comm_filter_fresh_bootstrap_preserves_recipient_plans_without_rebuilds() {
    let conn = Connection::open_in_memory().unwrap();
    let ddl = include_str!("../../sql/notes-ddl.sql");
    conn.execute_batch(ddl).unwrap();
    assert_listing_index_present(&conn);
    register_comm_indexes(&conn);
    let version: i64 = conn
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .unwrap();
    for _ in 0..2 {
        for status in ["unread", "read", "all"] {
            assert_inbox_actor_seek(&measure(&conn, &inbox_filter(status, false)), status);
        }
        conn.execute_batch(ddl).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            version
        );
    }
}

#[test]
fn comm_filter_recreated_unread_index_preserves_fresh_reopen_and_analyzed_plans() {
    let migration = full_then_unread_ddl();
    let baseline_conn = fixture(6000, 10_000);
    let baseline: Vec<_> = ["unread", "read", "all", "sent"]
        .iter()
        .map(|status| {
            measure_unpinned(
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
                if *status == "sent" {
                    // ADR-187 pins recipient seeks; sender-only plans remain cost-selected.
                    assert!(!result["sql"].as_str().unwrap().contains("INDEXED BY"));
                } else {
                    assert_inbox_actor_seek(&result, status);
                }
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
fn comm_filter_preanalyzed_upgrade_preserves_existing_comm_indexes() {
    fn comm_catalog(conn: &Connection) -> Vec<(String, i64, i64, String)> {
        conn.prepare(
            "SELECT name, rowid, rootpage, sql FROM sqlite_schema \
             WHERE type = 'index' AND name GLOB 'idx_comm_message_*' ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn comm_stats(conn: &Connection) -> Vec<(String, String)> {
        conn.prepare(
            "SELECT idx, stat FROM sqlite_stat1 WHERE idx GLOB 'idx_comm_message_*' ORDER BY idx",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("preanalyzed-mailbox.sqlite");
    let mut conn = fixture_with_connection(Connection::open(&path).unwrap(), 6000, 10_000, None);
    conn.execute_batch("DROP INDEX idx_notes_namespace_created")
        .unwrap();
    conn.execute_batch("ANALYZE").unwrap();
    let catalog_before = comm_catalog(&conn);
    let stats_before = comm_stats(&conn);
    assert_eq!(catalog_before.len(), 5);
    assert_eq!(stats_before.len(), 5);
    let filters: Vec<_> = ["unread", "read", "all", "sent"]
        .iter()
        .map(|status| {
            if *status == "sent" {
                sent_filter()
            } else {
                inbox_filter(status, false)
            }
        })
        .collect();
    let baseline: Vec<_> = filters
        .iter()
        .map(|filter| measure_unpinned(&conn, filter))
        .collect();

    // Existing pack indexes survive CREATE IF NOT EXISTS registration during
    // a real upgrade, including their catalog positions and ANALYZE statistics.
    conn.execute_batch(&full_then_unread_ddl()).unwrap();
    conn.execute_batch(include_str!("../../sql/034-notes-namespace-created.sql"))
        .unwrap();
    assert_listing_index_present(&conn);
    let mut measurements = Vec::new();
    for stage in ["upgraded", "reopened"] {
        if stage == "reopened" {
            drop(conn);
            conn = Connection::open(&path).unwrap();
        }
        assert_eq!(comm_catalog(&conn), catalog_before, "{stage}");
        assert_eq!(comm_stats(&conn), stats_before, "{stage}");
        for (index, status) in ["unread", "read", "all", "sent"].iter().enumerate() {
            let result = measure(&conn, &filters[index]);
            if *status == "sent" {
                assert!(!result["sql"].as_str().unwrap().contains("INDEXED BY"));
            } else {
                assert_inbox_actor_seek(&result, status);
            }
            assert_eq!(result["ids"], baseline[index]["ids"]);
            if *status == "unread" {
                let before = baseline[index]["vm_steps"].as_u64().unwrap();
                let after = result["vm_steps"].as_u64().unwrap();
                assert!(
                    after <= before + 128,
                    "{stage}: unread work regressed ({before} -> {after})"
                );
            }
            measurements.push(json!({"stage":stage,"status":status,"result":result}));
        }
    }
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(),"preanalyzed_upgrade":measurements})
    );
}

#[test]
fn comm_filter_recreated_unread_index_bounds_work_as_other_mailboxes_grow() {
    let migration = full_then_unread_ddl();
    let mut measurements = Vec::new();
    for fresh_schema in [true, false] {
        let mut small_results = Vec::new();
        for foreign_count in [0, 6000] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("mailbox.sqlite");
            // Sent already changes plans after ANALYZE as the population grows.
            // Compare this inbox migration to an identical no-full-index cell.
            let baseline_path = temp.path().join("baseline.sqlite");
            let mut baseline_conn = fixture_with_connection(
                Connection::open(&baseline_path).unwrap(),
                foreign_count,
                0,
                None,
            );
            let mut conn = fixture_with_connection(
                Connection::open(&path).unwrap(),
                foreign_count,
                0,
                fresh_schema.then_some(migration.as_str()),
            );
            if !fresh_schema {
                conn.execute_batch(&migration).unwrap();
                register_comm_indexes(&conn);
                register_comm_indexes(&baseline_conn);
            }
            let full_exists: bool = baseline_conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'idx_notes_message_recipient_direction')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!full_exists, "sent baseline must exclude the full index");
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
                    drop(baseline_conn);
                    baseline_conn = Connection::open(&baseline_path).unwrap();
                }
                if *stage == "analyzed" {
                    conn.execute_batch("ANALYZE").unwrap();
                    baseline_conn.execute_batch("ANALYZE").unwrap();
                }
                for (index, status) in ["unread", "read", "all", "sent"].iter().enumerate() {
                    let filter = if *status == "sent" {
                        sent_filter()
                    } else {
                        inbox_filter(status, false)
                    };
                    let result = measure(&conn, &filter);
                    let sent_baseline =
                        (*status == "sent").then(|| measure_unpinned(&baseline_conn, &filter));
                    if let Some(before) = &sent_baseline {
                        assert_eq!(result["sql"], before["sql"]);
                        assert_eq!(result["ids"], before["ids"]);
                        let before_steps = before["vm_steps"].as_u64().unwrap();
                        let after_steps = result["vm_steps"].as_u64().unwrap();
                        assert!(after_steps <= before_steps,
                            "{fresh_schema}/{foreign_count}/{stage}: sent work regressed ({before} -> {result})");
                    } else {
                        assert_inbox_actor_seek(&result, status);
                    }
                    if foreign_count == 0 {
                        small_results.push(result.clone());
                    } else {
                        let baseline: &Value = &small_results[stage_index * 4 + index];
                        assert_eq!(result["ids"], baseline["ids"]);
                        if *status != "sent" {
                            let steps = result["vm_steps"].as_u64().unwrap();
                            let baseline_steps = baseline["vm_steps"].as_u64().unwrap();
                            assert!(steps <= baseline_steps + 128,
                                "{fresh_schema}/{stage}/{status}: foreign mailbox growth must not add row-proportional work ({baseline_steps} -> {steps})");
                        }
                    }
                    measurements.push(json!({"fresh_schema":fresh_schema,"stage":stage,"foreign_count":foreign_count,"status":status,"result":result,"sent_baseline":sent_baseline}));
                }
            }
        }
    }
    println!(
        "{}",
        json!({"sqlite_version":rusqlite::version(),"bounded_growth":measurements})
    );
}

fn assert_listing_index_present(conn: &Connection) {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'idx_notes_namespace_created')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        exists,
        "comm plans must be checked with the shipped listing index present"
    );
}

#[test]
fn comm_seek_pins_match_emitted_predicates_and_preserve_pages() {
    let conn = fixture(300, 200);
    conn.execute_batch(&full_then_unread_ddl()).unwrap();
    assert_listing_index_present(&conn);
    for (i, properties) in [
        json!({"direction":"inbound", "read":false}),
        json!({"direction":"inbound", "to_actor":null, "read":true}),
        json!({"direction":"inbound", "to_actor":"", "read":false}),
        json!({"direction":"outbound", "read":false}),
        json!({"direction":"inbound", "to_actor":null, "read":false}),
    ]
    .iter()
    .enumerate()
    {
        conn.execute(
            "INSERT INTO notes(id, namespace, kind, properties, created_at, updated_at) VALUES (?1, 'default', 'message', ?2, 90000, 90000)",
            rusqlite::params![format!("legacy-{i}"), properties.to_string()],
        ).unwrap();
    }

    for status in ["unread", "read", "all"] {
        for op in [
            FilterOp::EqOrMissingIndexed,
            FilterOp::EqOrLegacyIndexed,
            FilterOp::JsonTypeMissingOrNullIndexed,
        ] {
            let mut filter = inbox_filter(status, false);
            filter.property_filters.last_mut().unwrap().op = op;
            for namespaces in [vec![], vec!["default".to_owned(), "other".to_owned()]] {
                filter.namespaces = namespaces;
                let (predicate, _) = build_note_filter_where("default", &filter).unwrap();
                let expected = if status == "unread" {
                    " INDEXED BY idx_notes_unread_probe_recipient_direction"
                } else {
                    " INDEXED BY idx_notes_message_recipient_direction"
                };
                assert_eq!(comm_filter_index_clause(&filter, &predicate), expected);
                let pinned = measure(&conn, &filter);
                let unpinned = measure_unpinned(&conn, &filter);
                assert_inbox_actor_seek(&pinned, status);
                assert_eq!(pinned["ids"], unpinned["ids"]);
            }
        }
    }
}

#[test]
fn unmatched_comm_filters_remain_unpinned() {
    let conn = fixture(30, 20);
    let mut filters = vec![
        inbox_filter("unread", true),
        sent_filter(),
        NoteFilter::default(),
    ];
    let mut no_direction = inbox_filter("unread", false);
    no_direction.property_filters.remove(0);
    filters.push(no_direction);
    let mut other_kind = inbox_filter("unread", false);
    other_kind.kind = Some("task".into());
    filters.push(other_kind);
    for filter in filters {
        let (predicate, _) = build_note_filter_where("default", &filter).unwrap();
        assert_eq!(comm_filter_index_clause(&filter, &predicate), "");
        let result = measure(&conn, &filter);
        assert!(!result["sql"].as_str().unwrap().contains("INDEXED BY"));
        assert_eq!(result["ids"], measure_unpinned(&conn, &filter)["ids"]);
    }
}

#[test]
fn missing_pinned_comm_indexes_are_errors_without_fallback() {
    for (status, index) in [
        ("all", "idx_notes_message_recipient_direction"),
        ("unread", "idx_notes_unread_probe_recipient_direction"),
    ] {
        let conn = fixture(30, 20);
        conn.execute_batch(&full_then_unread_ddl()).unwrap();
        let result = measure(&conn, &inbox_filter(status, false));
        conn.execute_batch(&format!("DROP INDEX {index}")).unwrap();
        let error = conn.prepare(result["sql"].as_str().unwrap()).err().unwrap();
        assert!(error.to_string().contains(index), "{error}");
    }
}
