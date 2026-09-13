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

/// The candidate: the filter columns in order, then the ordering, partial on the
/// deleted predicate the builder always emits.
const CANDIDATE: &str = "CREATE INDEX idx_candidate_note_list_order
 ON notes(namespace, kind, created_at DESC, id ASC)
 WHERE deleted_at IS NULL";

fn fixture(notes: usize, seed_ddl: Option<&str>) -> Connection {
    fixture_with_kind_every(notes, 2, seed_ddl)
}

/// `task` on every `kind_every`-th row, `message` on the rest. `kind_every = 2`
/// is the even split; a large value makes `task` the rare kind, which is the
/// shape a namespace+time index has to walk past to fill a page.
fn fixture_with_kind_every(notes: usize, kind_every: usize, seed_ddl: Option<&str>) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../../sql/notes-ddl.sql"))
        .unwrap();
    // A later schema may ship the candidate. The baseline arm has to keep
    // describing the unfixed planner after that happens, or it stops being a
    // baseline the moment it would matter.
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_candidate_note_list_order;\n         DROP INDEX IF EXISTS idx_candidate_note_list_order_all",
    )
        .unwrap();
    if let Some(ddl) = seed_ddl {
        conn.execute_batch(ddl).unwrap();
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

/// The finding: with the indexes the schema ships, listing one page of notes
/// establishes the order with a temporary b-tree over the whole matching set.
/// The candidate index removes it, and must return the same page while doing so.
#[test]
fn listing_notes_sorts_the_whole_namespace_until_an_index_serves_the_order() {
    let baseline = fixture(20_000, None);
    let (base_plan, base_ids, base_steps) = measure(&baseline, Some("task"), 10);
    assert!(
        sorts(&base_plan),
        "expected the shipped schema to sort for this order; plan was {base_plan:?}"
    );

    let fixed = fixture(20_000, Some(CANDIDATE));
    let (fixed_plan, fixed_ids, fixed_steps) = measure(&fixed, Some("task"), 10);
    assert!(
        !sorts(&fixed_plan),
        "the candidate index must remove the sort, not just make it faster; plan was {fixed_plan:?}"
    );
    assert!(
        fixed_plan.iter().any(|row| row.contains(CANDIDATE_NAME)),
        "the plan must name the candidate index: {fixed_plan:?}"
    );

    // The correctness arm, and the one that matters most: an index that changes
    // which rows come back is a defect wearing a speedup's clothing.
    assert_eq!(
        base_ids, fixed_ids,
        "the candidate index changed the page that is returned"
    );
    assert_eq!(
        base_ids.len(),
        10,
        "the page itself must be the requested size"
    );

    // Reported rather than asserted as a threshold: a step count is a property of
    // this fixture's size, and a threshold on it would only measure the fixture.
    println!(
        "note list plan: baseline steps {base_steps} plan {base_plan:?}; \
         with candidate steps {fixed_steps} plan {fixed_plan:?}"
    );
    assert!(
        fixed_steps < base_steps,
        "the candidate must do strictly less work: {fixed_steps} vs {base_steps}"
    );
}

const CANDIDATE_NAME: &str = "idx_candidate_note_list_order";

/// The second candidate, for the listing that names no kind. The first one cannot
/// serve it: with no equality on `kind`, its second column stands between the
/// namespace and the ordering.
const CANDIDATE_NO_KIND: &str = "CREATE INDEX idx_candidate_note_list_order_all
 ON notes(namespace, created_at DESC, id ASC)
 WHERE deleted_at IS NULL";
const CANDIDATE_NO_KIND_NAME: &str = "idx_candidate_note_list_order_all";

/// The same statement with no kind filter, which is what an unfiltered `list`
/// issues. Without this arm a candidate that only serves the two-column filter
/// would read as a complete fix.
#[test]
fn listing_notes_without_a_kind_filter_sorts_for_the_same_reason() {
    let baseline = fixture(20_000, None);
    let (base_plan, base_ids, _) = measure(&baseline, None, 10);
    assert!(
        sorts(&base_plan),
        "unfiltered listing was expected to sort too; plan was {base_plan:?}"
    );

    let fixed = fixture(20_000, Some(CANDIDATE));
    let (fixed_plan, fixed_ids, _) = measure(&fixed, None, 10);
    assert_eq!(
        base_ids, fixed_ids,
        "the candidate index changed the unfiltered page"
    );
    assert!(
        sorts(&fixed_plan),
        "the kind-bearing candidate was expected NOT to serve this ordering, which is why the \
         second one exists; plan was {fixed_plan:?}"
    );

    let both = fixture(20_000, Some(&format!("{CANDIDATE};{CANDIDATE_NO_KIND}")));
    let (both_plan, both_ids, _) = measure(&both, None, 10);
    assert!(
        !sorts(&both_plan),
        "the no-kind candidate must remove the sort for an unfiltered listing; plan was {both_plan:?}"
    );
    assert!(
        both_plan
            .iter()
            .any(|row| row.contains(CANDIDATE_NO_KIND_NAME)),
        "the plan must name the no-kind candidate: {both_plan:?}"
    );
    assert_eq!(
        base_ids, both_ids,
        "the no-kind candidate changed the unfiltered page"
    );

    // And the kind-filtered listing must not regress once both exist: a second
    // index changes what the planner can choose, so the first arm's conclusion
    // has to be re-derived in the presence of the second.
    let (kinded_plan, _, _) = measure(&both, Some("task"), 10);
    assert!(
        !sorts(&kinded_plan),
        "adding the second index reintroduced a sort for the kind-filtered listing: {kinded_plan:?}"
    );

    println!(
        "note list plan, no kind filter: baseline {base_plan:?}; kind-bearing candidate only \
         {fixed_plan:?}; both candidates {both_plan:?}; kind-filtered with both {kinded_plan:?}"
    );
}

/// Which of the two candidates is actually needed. The kind-bearing one is the
/// faster answer for a kind-filtered page, but every index on `notes` is paid for
/// on every note insert, and `notes` is the table the daemon writes most. So the
/// question a schema change has to answer is whether ONE index over
/// `(namespace, created_at DESC, id ASC)` serves both listings, filtering the kind
/// as an ordinary term while walking rows already in order.
#[test]
fn the_namespace_and_time_index_alone_serves_a_kind_filtered_page_without_sorting() {
    let baseline = fixture(20_000, None);
    let (base_plan, base_ids, base_steps) = measure(&baseline, Some("task"), 10);
    assert!(sorts(&base_plan), "baseline must sort: {base_plan:?}");

    let one = fixture(20_000, Some(CANDIDATE_NO_KIND));
    let (one_plan, one_ids, one_steps) = measure(&one, Some("task"), 10);

    let two = fixture(20_000, Some(&format!("{CANDIDATE};{CANDIDATE_NO_KIND}")));
    let (two_plan, two_ids, two_steps) = measure(&two, Some("task"), 10);

    assert_eq!(base_ids, one_ids, "the single index changed the page");
    assert_eq!(base_ids, two_ids, "the pair changed the page");

    println!(
        "note list plan, one index vs two, kind-filtered: baseline steps {base_steps} \
         {base_plan:?}; namespace+time only steps {one_steps} {one_plan:?}; both steps \
         {two_steps} {two_plan:?}"
    );

    assert!(
        !sorts(&one_plan),
        "the namespace+time index alone was expected to serve the order for a kind-filtered \
         page as well, walking past the other kinds; plan was {one_plan:?}"
    );
    assert!(
        one_steps < base_steps,
        "one index must still beat the sort: {one_steps} vs {base_steps}"
    );
}

/// The shape that decides between one index and two, and the arm that answered
/// the opposite of what it was written to show. A rare kind was expected to be
/// where a namespace+time index costs the most, because it has to walk past
/// everything else to fill a page. What happens instead is that the planner does
/// not take it: with statistics, sorting the few rows of a rare kind is cheaper
/// than walking the namespace in time order, so the plan stays exactly as it is
/// today. The sort is only expensive when the kind is a large share of the
/// namespace, which is the case the daemon's own outbox scan hits.
///
/// So this arm asserts the non-regression property rather than a speedup: adding
/// the namespace+time index must leave the rare-kind plan alone.
#[test]
fn a_rare_kind_keeps_its_existing_plan_when_the_namespace_and_time_index_exists() {
    const RARE_EVERY: usize = 1_000;

    let baseline = fixture_with_kind_every(20_000, RARE_EVERY, None);
    let (base_plan, base_ids, base_steps) = measure(&baseline, Some("task"), 10);
    assert!(
        sorts(&base_plan),
        "the rare-kind baseline sorts too, it is just a small sort: {base_plan:?}"
    );

    let one = fixture_with_kind_every(20_000, RARE_EVERY, Some(CANDIDATE_NO_KIND));
    let (one_plan, one_ids, one_steps) = measure(&one, Some("task"), 10);

    let two = fixture_with_kind_every(
        20_000,
        RARE_EVERY,
        Some(&format!("{CANDIDATE};{CANDIDATE_NO_KIND}")),
    );
    let (two_plan, two_ids, two_steps) = measure(&two, Some("task"), 10);

    assert_eq!(base_ids, one_ids, "the single index changed the page");
    assert_eq!(base_ids, two_ids, "the pair changed the page");
    assert_eq!(
        base_ids.len(),
        10,
        "the rare kind must still fill a whole page, or the arm measures a short page"
    );

    println!(
        "note list plan, rare kind (1 in {RARE_EVERY}): baseline steps {base_steps} \
         {base_plan:?}; namespace+time only steps {one_steps} {one_plan:?}; both steps \
         {two_steps} {two_plan:?}"
    );

    assert_eq!(
        base_plan, one_plan,
        "the namespace+time index must not change the rare-kind plan, in either direction: the \
         planner is expected to keep the cheap small sort"
    );
    assert_eq!(
        base_steps, one_steps,
        "same plan, so the same work: {one_steps} vs {base_steps}"
    );
    assert!(
        !sorts(&two_plan) && two_steps < base_steps,
        "only the kind-bearing index removes the rare-kind sort, and this is the whole of what a \
         second index buys: {two_steps} vs {base_steps}, plan {two_plan:?}"
    );
}
