//! ID-filtered event reads seek the primary key on a store without
//! sqlite_stat1, on both the events-database schema and the main-database
//! schema, and their work does not follow the size of the namespace.

use super::*;
use crate::migrations::run_migrations;
use rusqlite::{Connection, StatementStatus};

const NAMESPACE: &str = "local";

/// Builds a database of one event schema holding the given number of events.
type OpenEvents = fn(usize) -> Connection;

fn insert_events(conn: &Connection, rows: usize) {
    let mut stmt = conn
        .prepare(
            "INSERT INTO events (id, namespace, verb, substrate, actor, kind, outcome, created_at) \
             VALUES (?1, ?2, 'remember', 'note', 'actor:test', 'audit', 'success', ?3)",
        )
        .unwrap();
    for i in 1..=rows {
        stmt.execute(rusqlite::params![
            Uuid::from_u128(i as u128).to_string(),
            if i % 50 == 0 { "other" } else { NAMESPACE },
            i as i64,
        ])
        .unwrap();
    }
}

fn events_database(rows: usize) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    ensure_events_schema(&conn).unwrap();
    insert_events(&conn, rows);
    conn
}

fn main_database(rows: usize) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    run_migrations(&mut conn).unwrap();
    ensure_operation_attribution_columns(&conn).unwrap();
    insert_events(&conn, rows);
    conn
}

fn plan_rows_work(
    conn: &Connection,
    sql: &str,
    params: &[Box<dyn rusqlite::ToSql>],
) -> (Vec<String>, Vec<String>, i32) {
    let refs = params.iter().map(|p| p.as_ref()).collect::<Vec<_>>();
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(refs.as_slice(), |row| row.get(3))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let ids = stmt
        .query_map(refs.as_slice(), |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (plan, ids, stmt.get_status(StatementStatus::VmStep))
}

/// The split store probes the legacy store with the IDs of every new audit
/// batch (an ID-only filter, `LIMIT` = batch size). Those IDs are normally
/// absent, so a plan that walks the namespace in `created_at` order never
/// fills the limit and reads every row of the namespace.
#[test]
fn id_filtered_reads_seek_the_primary_key_without_statistics() {
    let schemas: [(&str, OpenEvents); 2] = [("events", events_database), ("main", main_database)];
    for (schema, open) in schemas {
        for analyzed in [false, true] {
            let mut work = Vec::new();
            for size in [1_000, 10_000] {
                let conn = open(size);
                if analyzed {
                    conn.execute_batch("ANALYZE").unwrap();
                }
                let present = Uuid::from_u128(7);
                let mut ids: Vec<Uuid> = (0..7).map(|i| Uuid::from_u128(u128::MAX - i)).collect();
                ids.push(present);
                let filter = EventFilter {
                    ids: ids.clone(),
                    ..EventFilter::default()
                };
                let (clause, mut params) =
                    build_event_filter_sql(&conn, NAMESPACE, &filter).unwrap();
                params.push(Box::new(i64::try_from(ids.len()).unwrap()));
                params.push(Box::new(0_i64));
                let sql = query_events_sql(&clause, params.len() - 1, params.len());
                let (plan, found, steps) = plan_rows_work(&conn, &sql, &params);
                eprintln!(
                    "event id probe schema={schema} size={size} analyzed={analyzed} vm_steps={steps} plan={plan:?}"
                );
                assert!(
                    plan.iter()
                        .any(|line| line.contains("sqlite_autoindex_events_1")),
                    "an ID-filtered read must seek the primary key ({schema}, analyzed={analyzed}): {plan:?}"
                );
                assert!(
                    !plan.iter().any(|line| line.contains("idx_events_ns_created")),
                    "an ID-filtered read must not walk the namespace in created_at order ({schema}, analyzed={analyzed}): {plan:?}"
                );
                assert_eq!(found, vec![present.to_string()]);
                work.push(steps);
            }
            assert!(
                work[1] < work[0] * 2,
                "ID-filtered read work must not track namespace size ({schema}, analyzed={analyzed}): {work:?}"
            );
        }
    }
}

/// Not a CI test: a manual scale measurement, run with:
/// `KHIVE_EVENT_PLAN_ROWS=500000 cargo test -p khive-db --lib event_plan_scale_measurement -- --ignored --nocapture`
///
/// Builds a file-backed main-database schema (all migrations) under the temp
/// dir with `KHIVE_EVENT_PLAN_ROWS` events (default 500,000) in one namespace,
/// no sqlite_stat1, then times two statements in the form shipped before this
/// guard (OLD) and with it (NEW), alternating OLD and NEW each round so drift
/// lands on both:
/// - the split store's legacy probe for a new audit batch: an ID-only filter
///   with 8 absent IDs, built by `build_event_filter_sql`/`query_events_sql`;
///   OLD is the same text without the unary `+`;
/// - the schedule creator-provenance lookup (text mirrors
///   `creator_provenance_statement` in khive-mcp's pending_events dispatch)
///   with exactly one matching row.
#[test]
#[ignore = "manual scale measurement, not a CI assertion; see doc comment"]
fn event_plan_scale_measurement() {
    const ROUNDS: usize = 15;
    const PROVENANCE_WHERE: &str = "WHERE namespace = ?1 AND verb = ?2 AND target_id = ?3 \
         AND outcome = 'success' AND json_extract(payload, '$.provenance') = ?4 \
         AND json_extract(payload, '$.event_type') = ?5";
    let rows: usize = std::env::var("KHIVE_EVENT_PLAN_ROWS")
        .ok()
        .map(|v| v.parse().expect("KHIVE_EVENT_PLAN_ROWS is a row count"))
        .unwrap_or(500_000);
    let path =
        std::env::temp_dir().join(format!("khive-event-plan-scale-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut conn = Connection::open(&path).expect("open scratch db under the temp dir");
    run_migrations(&mut conn).expect("migrate scratch db");
    ensure_operation_attribution_columns(&conn).expect("attribution columns");

    let target = Uuid::from_u128(0xfeed);
    let seed_start = std::time::Instant::now();
    conn.execute_batch("BEGIN").expect("begin seed");
    {
        let mut insert = conn
            .prepare(
                "INSERT INTO events (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                 target_id, created_at) VALUES (?1, ?2, ?3, 'note', 'actor:scale', 'audit', 'success', ?4, ?5, ?6)",
            )
            .expect("prepare seed insert");
        for i in 1..=rows {
            let provenance = i == rows / 2;
            let verb = if provenance {
                "schedule.creator_provenance".to_string()
            } else {
                format!("verb.{}", i % 40)
            };
            let payload = if provenance {
                serde_json::json!({"provenance": "schedule_creator_v1", "event_type": "remind"})
                    .to_string()
            } else {
                format!(
                    "{{\"request\":\"{}\",\"detail\":\"{}\"}}",
                    i,
                    "x".repeat(200)
                )
            };
            let target_id = if provenance {
                target
            } else {
                Uuid::from_u128((1_u128 << 64) | i as u128)
            };
            insert
                .execute(rusqlite::params![
                    Uuid::from_u128(i as u128).to_string(),
                    NAMESPACE,
                    verb,
                    payload,
                    target_id.to_string(),
                    i as i64,
                ])
                .expect("insert seed row");
        }
    }
    conn.execute_batch("COMMIT").expect("commit seed");
    eprintln!("seeded {rows} rows in {:?}", seed_start.elapsed());

    let filter = EventFilter {
        ids: (0..8).map(|i| Uuid::from_u128(u128::MAX - i)).collect(),
        ..EventFilter::default()
    };
    let (clause, mut probe_params) = build_event_filter_sql(&conn, NAMESPACE, &filter).unwrap();
    probe_params.push(Box::new(8_i64));
    probe_params.push(Box::new(0_i64));
    let probe_new = query_events_sql(&clause, probe_params.len() - 1, probe_params.len());
    assert!(probe_new.contains("+namespace"), "{probe_new}");
    let probe_old = probe_new.replace("+namespace", "namespace");
    let provenance_old = format!(
        "SELECT actor FROM events {PROVENANCE_WHERE} ORDER BY created_at ASC, id ASC LIMIT 2"
    );
    let provenance_new = format!(
        "SELECT actor FROM events {PROVENANCE_WHERE} ORDER BY +created_at ASC, +id ASC LIMIT 2"
    );
    let provenance_params: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(NAMESPACE),
        Box::new("schedule.creator_provenance"),
        Box::new(target.to_string()),
        Box::new("schedule_creator_v1"),
        Box::new("remind"),
    ];

    let time = |sql: &str, params: &[Box<dyn rusqlite::ToSql>]| {
        let started = std::time::Instant::now();
        let (_, found, steps) = plan_rows_work(&conn, sql, params);
        (started.elapsed().as_micros(), found.len(), steps)
    };
    for (name, old, new, params, expected) in [
        (
            "legacy id probe",
            &probe_old,
            &probe_new,
            &probe_params,
            0_usize,
        ),
        (
            "creator provenance",
            &provenance_old,
            &provenance_new,
            &provenance_params,
            1,
        ),
    ] {
        let mut old_us = Vec::new();
        let mut new_us = Vec::new();
        let (mut old_steps, mut new_steps) = (0, 0);
        for round in 0..ROUNDS + 2 {
            let (us, found, steps) = time(old.as_str(), params.as_slice());
            assert_eq!(found, expected, "{name} OLD rows");
            let (nus, nfound, nsteps) = time(new.as_str(), params.as_slice());
            assert_eq!(nfound, expected, "{name} NEW rows");
            if round >= 2 {
                old_us.push(us);
                new_us.push(nus);
            }
            (old_steps, new_steps) = (steps, nsteps);
        }
        old_us.sort_unstable();
        new_us.sort_unstable();
        let plan = |sql: &str| plan_rows_work(&conn, sql, params).0;
        eprintln!(
            "{name}: rows={rows} OLD median={}us max={}us vm_steps={old_steps} plan={:?}",
            old_us[ROUNDS / 2],
            old_us[ROUNDS - 1],
            plan(old.as_str())
        );
        eprintln!(
            "{name}: rows={rows} NEW median={}us max={}us vm_steps={new_steps} plan={:?}",
            new_us[ROUNDS / 2],
            new_us[ROUNDS - 1],
            plan(new.as_str())
        );
    }
    drop(conn);
    let _ = std::fs::remove_file(&path);
}
