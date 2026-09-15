//! Measure the planner for the note listing query using the production SQL builder.
//!
//! `query_notes_count_free` backs the `list` verb. It selects every note column,
//! filters on namespace and an optional kind, orders by `created_at DESC, id ASC`,
//! and only then applies `LIMIT`/`OFFSET`. If no index produces that ordering under
//! that filter, SQLite has to establish it with a sort, and the sorter carries the
//! selected columns — `content` and `properties` included — for every matching row
//! rather than for the page.

use super::{build_note_where, NOTE_COLUMNS};
use rusqlite::{Connection, StatementStatus};

fn fixture(notes: usize, kind_every: usize, shipped: bool) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../../sql/notes-ddl.sql"))
        .unwrap();
    if !shipped {
        conn.execute_batch("DROP INDEX idx_notes_namespace_created")
            .unwrap();
    }
    conn.execute_batch("BEGIN").unwrap();
    for i in 0..notes {
        // Two kinds, so a kind filter selects a real subset rather than the whole
        // table, and content wide enough that a sorter carrying it is visible.
        let kind = if i % kind_every == 0 {
            "task"
        } else {
            "message"
        };
        conn.execute(
            "INSERT INTO notes(id, namespace, kind, content, properties, created_at, updated_at)
             VALUES (?1, 'default', ?2, ?3, '{}', ?4, ?4)",
            rusqlite::params![
                format!("id-{i:08}"),
                kind,
                "x".repeat(512),
                format!("2026-01-01T00:00:{:02}.{:06}Z", i % 60, i),
            ],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    // Without stats the planner guesses; the production store has been open long
    // enough to have them, so measuring without ANALYZE measures a different
    // database than the one the report is about.
    conn.execute_batch("ANALYZE").unwrap();
    conn
}

/// Run the production statement and return its plan rows, the ids it produced and
/// how many VM steps it took.
fn measure(conn: &Connection, kind: Option<&str>, limit: i64) -> (Vec<String>, Vec<String>, i32) {
    let (where_sql, mut params) = build_note_where("default", kind);
    params.push(Box::new(limit));
    params.push(Box::new(0i64));
    let limit_idx = params.len() - 1;
    let offset_idx = params.len();
    let sql = format!(
        "SELECT {NOTE_COLUMNS} FROM notes{where_sql} ORDER BY created_at DESC, id ASC \
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
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
    let steps = statement.get_status(StatementStatus::VmStep);
    (plan, ids, steps)
}

fn sorts(plan: &[String]) -> bool {
    plan.iter()
        .any(|row| row.contains("TEMP B-TREE") || row.contains("USE TEMP B-TREE"))
}

#[test]
fn shipped_index_serves_common_kind_listing_without_sorting() {
    assert_shipped_listing(Some("task"));
}

#[test]
fn shipped_index_serves_unfiltered_listing_without_sorting() {
    assert_shipped_listing(None);
}

fn assert_shipped_listing(kind: Option<&str>) {
    let baseline = fixture(20_000, 2, false);
    let (base_plan, base_ids, base_steps) = measure(&baseline, kind, 10);
    assert!(
        sorts(&base_plan),
        "pre-V34 baseline must sort: {base_plan:?}"
    );

    let shipped = fixture(20_000, 2, true);
    let (plan, ids, steps) = measure(&shipped, kind, 10);
    assert!(!sorts(&plan), "shipped listing must not sort: {plan:?}");
    assert!(
        plan.iter()
            .any(|row| row.contains("idx_notes_namespace_created")),
        "shipped listing must name the order index: {plan:?}"
    );
    assert_eq!(base_ids, ids, "the shipped index changed the returned page");
    assert_eq!(ids.len(), 10);
    println!("note list {kind:?}: baseline {base_steps} {base_plan:?}; shipped {steps} {plan:?}");
}

#[test]
fn rare_kind_keeps_its_existing_plan_with_the_shipped_index() {
    let baseline = fixture(20_000, 1_000, false);
    let (base_plan, base_ids, base_steps) = measure(&baseline, Some("task"), 10);
    assert!(
        sorts(&base_plan),
        "rare-kind baseline keeps its small sort: {base_plan:?}"
    );

    let shipped = fixture(20_000, 1_000, true);
    let (plan, ids, steps) = measure(&shipped, Some("task"), 10);
    assert_eq!(
        base_ids, ids,
        "the shipped index changed the rare-kind page"
    );
    assert_eq!(ids.len(), 10);
    assert_eq!(base_plan, plan, "the rare-kind plan must remain unchanged");
    assert_eq!(
        base_steps, steps,
        "the rare-kind work must remain unchanged"
    );
}
