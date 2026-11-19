//! Production-page planner controls for legacy, including-deleted quarantines.

use super::note_filter_page_sql;
use khive_storage::note::{
    FilterOp, NoteExpiryFallback, NoteFilter, NoteTimeOrder, PropertyFilter,
};
use khive_storage::SqlValue;
use rusqlite::{Connection, StatementStatus};
use serde_json::{json, Value};
use uuid::Uuid;

const INDEX: &str = "idx_comm_quarantine_legacy_expiry";
const MIGRATION: &str = include_str!("../../sql/059-comm-legacy-quarantine-index.sql");

fn legacy_filter() -> NoteFilter {
    NoteFilter {
        kind: Some("message".into()),
        include_deleted: true,
        property_filters: vec![
            PropertyFilter {
                json_path: "$.channel_kind".into(),
                op: FilterOp::Eq,
                value: SqlValue::Text("email".into()),
            },
            PropertyFilter {
                json_path: "$.quarantined".into(),
                op: FilterOp::TrueOrTextTrue,
                value: SqlValue::Null,
            },
            PropertyFilter {
                json_path: "$.channel_slug".into(),
                op: FilterOp::MissingNullOrSpaceEmptyText,
                value: SqlValue::Null,
            },
        ],
        expiry_fallback: Some(NoteExpiryFallback {
            expires_at_or_before: 1_000,
            created_at_or_before: 900,
        }),
        time_order: Some(NoteTimeOrder::ExpiresAtOrCreatedAt),
        ..Default::default()
    }
}

fn insert(
    conn: &Connection,
    id: u128,
    namespace: &str,
    kind: &str,
    properties: Value,
    times: (i64, Option<i64>, Option<i64>),
) {
    conn.execute(
        "INSERT INTO notes(id, namespace, kind, properties, created_at, updated_at, expires_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
        rusqlite::params![
            Uuid::from_u128(id).to_string(),
            namespace,
            kind,
            properties.to_string(),
            times.0,
            times.1,
            times.2,
        ],
    )
    .unwrap();
}

fn fixture(unrelated: usize, populated: bool) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::run_migrations(&mut conn).unwrap();
    assert!(conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='idx_comm_quarantine_expiry')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap());
    // The real V52 index remains present. Only the candidate is removed for
    // the baseline; a removed migration must still reach behavioral assertions.
    conn.execute_batch("DROP INDEX IF EXISTS idx_comm_quarantine_legacy_expiry")
        .unwrap();
    let tx = conn.transaction().unwrap();
    if populated {
        for id in (1..=132).rev() {
            let mut properties = json!({"channel_kind":"email", "quarantined":true});
            match id % 4 {
                0 => {}
                1 => properties["channel_slug"] = Value::Null,
                2 => properties["channel_slug"] = json!(""),
                _ => properties["channel_slug"] = json!("   "),
            }
            if id % 3 == 0 {
                properties["quarantined"] = json!("true");
            }
            let (created, expiry) = if id % 2 == 0 {
                (900, None) // Inclusive creation fallback, ordered first.
            } else {
                (999, Some(1_000)) // Recent creation, inclusive explicit expiry.
            };
            insert(
                &tx,
                id,
                "local",
                "message",
                properties,
                (created, expiry, (id % 5 == 0).then_some(500)),
            );
        }
    }
    for (id, created, expiry) in [(1_000, 901, None), (1_001, 1, Some(1_001))] {
        insert(
            &tx,
            id,
            "local",
            "message",
            json!({"channel_kind":"email", "quarantined":true}),
            (created, expiry, None),
        );
    }
    // All exclusions sort ahead of the due page if any filter is lost.
    // The extra growth is unrelated by prefix or partial-index predicate,
    // never a claim about future rows inside the matching bucket.
    for n in 0..unrelated + 10 {
        let (mut namespace, mut kind) = ("local", "message");
        let mut properties = json!({"channel_kind":"email", "quarantined":true});
        match n % 10 {
            0 => namespace = "foreign",
            1 => kind = "observation",
            2 => properties["channel_kind"] = json!("telegram"),
            3 => properties["quarantined"] = json!(false),
            4 => properties["channel_slug"] = json!("inbox"),
            5 => properties["quarantined"] = json!(1),
            6 => properties["channel_slug"] = json!("\t"),
            7 => properties["channel_slug"] = json!("\u{2003}"),
            8 => properties["channel_slug"] = json!(0),
            _ => properties["quarantined"] = json!("True"),
        }
        insert(
            &tx,
            10_000 + n as u128,
            namespace,
            kind,
            properties,
            (1, Some(1), (n % 2 == 0).then_some(2)),
        );
    }
    tx.commit().unwrap();
    conn
}

#[derive(Debug)]
struct Measurement {
    ids: Vec<String>,
    plan: Vec<String>,
    steps: i32,
    sorts: i32,
}

fn measure(conn: &Connection) -> Measurement {
    let (sql, params) = note_filter_page_sql("local", &legacy_filter(), 128, 0).unwrap();
    let params: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params.as_slice(), |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    let mut statement = conn.prepare(&sql).unwrap();
    let ids = statement
        .query_map(params.as_slice(), |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    Measurement {
        ids,
        plan,
        steps: statement.get_status(StatementStatus::VmStep),
        sorts: statement.get_status(StatementStatus::Sort),
    }
}

fn expected(populated: bool) -> Vec<String> {
    if !populated {
        return Vec::new();
    }
    // COALESCE=900 fallback rows precede COALESCE=1000 explicit-expiry rows;
    // IDs break each tie, independent of reverse insertion and tombstones.
    (1..=132)
        .filter(|id| id % 2 == 0)
        .chain((1..=132).filter(|id| id % 2 != 0))
        .take(128)
        .map(|id| Uuid::from_u128(id).to_string())
        .collect()
}

fn assert_index_plan(result: &Measurement) {
    assert!(
        result.plan.iter().any(
            |line| line.contains(INDEX) && line.contains("namespace=? AND kind=? AND <expr>=?")
        ),
        "legacy cleanup must seek its namespace/kind/channel bucket: {result:?}"
    );
    assert!(
        result.plan.iter().all(|line| !line.contains("TEMP B-TREE")),
        "legacy cleanup must not sort its candidate bucket: {result:?}"
    );
    assert_eq!(result.sorts, 0, "native sort counter: {result:?}");
}

#[test]
fn legacy_quarantine_index_preserves_the_bounded_page_without_sorting() {
    for analyzed in [false, true] {
        for populated in [false, true] {
            let conn = fixture(100, populated);
            if analyzed {
                conn.execute_batch("ANALYZE").unwrap();
            }
            let baseline = measure(&conn);
            assert_eq!(baseline.ids, expected(populated), "{baseline:?}");
            conn.execute_batch(MIGRATION).unwrap();
            if analyzed {
                conn.execute_batch("ANALYZE").unwrap();
            }
            let indexed = measure(&conn);
            eprintln!(
                "legacy_quarantine_page analyzed={analyzed} populated={populated} baseline_vm_steps={} baseline_sorts={} baseline_plan={:?} indexed_vm_steps={} indexed_sorts={} indexed_plan={:?}",
                baseline.steps, baseline.sorts, baseline.plan, indexed.steps, indexed.sorts, indexed.plan
            );
            assert_eq!(indexed.ids, baseline.ids, "{indexed:?}");
            assert_index_plan(&indexed);
        }
    }
}

#[test]
fn legacy_quarantine_index_bounds_unrelated_growth_for_full_and_empty_pages() {
    for analyzed in [false, true] {
        for populated in [false, true] {
            let results: Vec<_> = [0, 2_000]
                .into_iter()
                .map(|unrelated| {
                    let conn = fixture(unrelated, populated);
                    conn.execute_batch(MIGRATION).unwrap();
                    if analyzed {
                        conn.execute_batch("ANALYZE").unwrap();
                    }
                    let result = measure(&conn);
                    assert_eq!(result.ids, expected(populated), "{result:?}");
                    assert_index_plan(&result);
                    result
                })
                .collect();
            eprintln!(
                "legacy_quarantine_growth analyzed={analyzed} populated={populated} unrelated=[0,2000] vm_steps=[{},{}] sorts=[{},{}]",
                results[0].steps, results[1].steps, results[0].sorts, results[1].sorts
            );
            assert!(
                results[1].steps <= results[0].steps + 128,
                "unrelated rows must not add proportional work (analyzed={analyzed}, populated={populated}): {results:?}"
            );
        }
    }
}
