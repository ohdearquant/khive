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

// ── pages over several visible namespaces ────────────────────────────────────

use super::{
    build_note_filter_read_clause, note_filter_page_order_clause, note_filter_page_sql, NoteFilter,
    NAMESPACE_ARMS_MAX,
};

/// `per_namespace` live notes in each of `ns-a` and `ns-b`, interleaved in time,
/// plus rows the page must never return: tombstoned notes in both, and notes in
/// `ns-hidden`, which no filter here makes visible. Every third creation instant
/// is shared by two notes, so the `id` tiebreak is exercised across namespaces.
///
/// Most of `ns-hidden` is twice the visible population and newer than all of it,
/// as on a store shared with busier actors. That matters for the control: when
/// the visible namespaces are nearly the whole table and `ANALYZE` has run,
/// SQLite can find a plan for the `IN` form that stops near the page, so the
/// `IN` form grows with the store only when other namespaces' rows stand in
/// front of the visible ones.
fn visible_fixture(per_namespace: usize, analyze: bool) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../../sql/notes-ddl.sql"))
        .unwrap();
    conn.execute_batch("BEGIN").unwrap();
    let insert = |id: String, namespace: &str, kind: &str, at: i64, deleted: Option<i64>| {
        conn.execute(
            "INSERT INTO notes(id, namespace, kind, content, properties, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, '{}', ?5, ?5, ?6)",
            rusqlite::params![id, namespace, kind, "x".repeat(256), at, deleted],
        )
        .unwrap();
    };
    for i in 0..per_namespace {
        let at = (i as i64 / 3) * 2;
        let kind = if i % 4 == 0 { "task" } else { "observation" };
        insert(format!("a-{i:08}"), "ns-a", kind, at, None);
        insert(
            format!("b-{i:08}"),
            "ns-b",
            kind,
            at + (i as i64 % 3 == 0) as i64,
            None,
        );
        if i % 50 == 0 {
            insert(format!("a-dead-{i:08}"), "ns-a", kind, at, Some(at + 1));
            insert(format!("b-dead-{i:08}"), "ns-b", kind, at, Some(at + 1));
            insert(format!("h-{i:08}"), "ns-hidden", kind, at, None);
        }
    }
    let newest = per_namespace as i64;
    for i in 0..per_namespace * 2 {
        insert(
            format!("h-new-{i:08}"),
            "ns-hidden",
            "observation",
            newest + i as i64,
            None,
        );
    }
    conn.execute_batch("COMMIT").unwrap();
    if analyze {
        conn.execute_batch("ANALYZE").unwrap();
    }
    conn
}

fn run_page(
    conn: &Connection,
    sql: &str,
    params: &[Box<dyn rusqlite::types::ToSql>],
) -> (Vec<String>, i32) {
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut statement = conn.prepare(sql).unwrap();
    let ids: Vec<String> = statement
        .query_map(refs.as_slice(), |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    (ids, statement.get_status(StatementStatus::VmStep))
}

/// The page as the production builder issues it.
fn page(
    conn: &Connection,
    filter: &NoteFilter,
    limit: i64,
    offset: i64,
) -> (Vec<String>, i32, String) {
    let (sql, params) = note_filter_page_sql("ns-a", filter, limit, offset).unwrap();
    let (ids, steps) = run_page(conn, &sql, &params);
    (ids, steps, sql)
}

/// The same page as the single `namespace IN (...)` statement, the form every
/// page read used before per-namespace arms.
fn in_form_page(
    conn: &Connection,
    filter: &NoteFilter,
    limit: i64,
    offset: i64,
) -> (Vec<String>, i32, Vec<String>) {
    let (where_sql, mut params) = build_note_filter_read_clause("ns-a", filter).unwrap();
    params.push(Box::new(limit));
    params.push(Box::new(offset));
    let limit_idx = params.len() - 1;
    let offset_idx = params.len();
    let order_clause = note_filter_page_order_clause(filter);
    let sql = format!(
        "SELECT {NOTE_COLUMNS} FROM notes{where_sql}{order_clause} \
         LIMIT ?{limit_idx} OFFSET ?{offset_idx}"
    );
    let (ids, steps) = run_page(conn, &sql, &params);
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let plan: Vec<String> = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(refs.as_slice(), |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    (ids, steps, plan)
}

fn visible(namespaces: &[&str]) -> NoteFilter {
    NoteFilter {
        namespaces: namespaces.iter().map(|ns| ns.to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn visible_namespace_page_work_is_bounded_by_the_page() {
    for analyze in [false, true] {
        let small = visible_fixture(100, analyze);
        let large = visible_fixture(10_000, analyze);
        let filter = visible(&["ns-a", "ns-b"]);
        for offset in [0, 40] {
            let (small_ids, small_steps, sql) = page(&small, &filter, 20, offset);
            let (large_ids, large_steps, _) = page(&large, &filter, 20, offset);
            assert!(
                sql.contains(" UNION ALL "),
                "two namespaces must split into arms: {sql}"
            );
            assert_eq!(large_ids.len(), 20);
            assert!(
                large_steps <= small_steps + 256,
                "analyze={analyze} offset={offset}: the page read grew with the namespaces' size: \
                 {large_steps} VM steps over 20,000 rows against {small_steps} over 200"
            );
            // Control, same run: the single IN statement reads and sorts every
            // visible row, so its work grows with the table. Both forms return
            // the same page.
            let (in_small_ids, in_small_steps, in_small_plan) =
                in_form_page(&small, &filter, 20, offset);
            let (in_large_ids, in_large_steps, in_large_plan) =
                in_form_page(&large, &filter, 20, offset);
            assert!(
                in_large_steps > in_small_steps * 10,
                "analyze={analyze} offset={offset}: control lost: the IN form took \
                 {in_large_steps} steps over 20,000 rows against {in_small_steps} over 200"
            );
            assert_eq!(small_ids, in_small_ids);
            assert_eq!(large_ids, in_large_ids);
            println!(
                "analyze={analyze} offset={offset}: arms {small_steps} -> {large_steps} steps, \
                 IN form {in_small_steps} -> {in_large_steps}; IN plans {in_small_plan:?} / {in_large_plan:?}"
            );
        }
    }
}

#[test]
fn visible_namespace_pages_equal_the_in_form() {
    let conn = visible_fixture(300, false);
    let mut task = visible(&["ns-a", "ns-b"]);
    task.kind = Some("task".to_string());
    let mut with_deleted = visible(&["ns-a", "ns-b"]);
    with_deleted.include_deleted = true;
    let cases = [
        ("two namespaces", visible(&["ns-a", "ns-b"]), true),
        ("kind filter", task, true),
        (
            "repeated namespace",
            visible(&["ns-b", "ns-a", "ns-b"]),
            true,
        ),
        (
            "an empty visible namespace",
            visible(&["ns-a", "ns-empty", "ns-b"]),
            true,
        ),
        ("one namespace", visible(&["ns-b"]), false),
        ("deleted rows included", with_deleted, false),
    ];
    for (label, filter, split) in cases {
        for limit in [1, 7, 20] {
            for offset in [0, 3, 50, 2_000] {
                let (ids, _, sql) = page(&conn, &filter, limit, offset);
                assert_eq!(sql.contains(" UNION ALL "), split, "{label}: {sql}");
                let (expected, _, _) = in_form_page(&conn, &filter, limit, offset);
                assert_eq!(ids, expected, "{label} limit={limit} offset={offset}");
            }
        }
        let (first, _, _) = page(&conn, &filter, 20, 0);
        assert!(!first.is_empty(), "{label}: fixture returned nothing");
        assert!(
            first.iter().all(|id| !id.starts_with("h-")),
            "{label}: a hidden namespace leaked into the page"
        );
    }
}

#[test]
fn visible_set_wider_than_the_arm_bound_keeps_one_statement() {
    let conn = visible_fixture(50, false);
    let mut namespaces: Vec<String> = (0..NAMESPACE_ARMS_MAX - 1)
        .map(|i| format!("ns-wide-{i}"))
        .collect();
    namespaces.push("ns-a".to_string());
    namespaces.push("ns-b".to_string());
    assert_eq!(namespaces.len(), NAMESPACE_ARMS_MAX + 1);
    let filter = NoteFilter {
        namespaces,
        ..Default::default()
    };
    let (ids, _, sql) = page(&conn, &filter, 20, 0);
    assert!(!sql.contains(" UNION ALL "), "{sql}");
    let (expected, _, _) = in_form_page(&conn, &filter, 20, 0);
    assert_eq!(ids, expected);
    assert_eq!(ids.len(), 20);
}
