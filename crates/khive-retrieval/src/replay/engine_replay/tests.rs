use super::*;

fn make_conn() -> Arc<Mutex<Connection>> {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    crate::weights::init_weight_schema(&conn).expect("init replay test schema");
    Arc::new(Mutex::new(conn))
}

fn insert_weight_event(
    conn: &Arc<Mutex<Connection>>,
    namespace: &str,
    atom_id: &str,
    weight_after: f32,
    ts_us: i64,
) {
    let c = conn.lock();
    c.execute(
        "INSERT INTO weight_events (namespace, atom_id, delta, weight_after, channel, eta, ts)
             VALUES (?1, ?2, 0.1, ?3, 'explicit', 0.1, ?4)",
        params![namespace, atom_id, weight_after as f64, ts_us],
    )
    .expect("insert weight_event");
}

#[tokio::test]
async fn test_weights_as_of_returns_snapshot_at_time() {
    let conn = make_conn();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();
    let atom_str = atom.to_string();

    // t0: weight 1.5
    let t0_us: i64 = 1_000_000_000;
    insert_weight_event(&conn, lambda, &atom_str, 1.5, t0_us);

    // t1: weight 2.5 (later)
    let t1_us: i64 = 2_000_000_000;
    insert_weight_event(&conn, lambda, &atom_str, 2.5, t1_us);

    // Query at t0 + 1: should see 1.5.
    let at_t0 = DateTime::from_timestamp_micros(t0_us + 1).unwrap();
    let snapshot = weights_as_of(&conn, lambda, at_t0)
        .await
        .expect("weights_as_of");
    let w = *snapshot.get(&atom).expect("atom must be in snapshot");
    assert!((w - 1.5).abs() < 0.01, "expected 1.5 at t0, got {w}");

    // Query at t1 + 1: should see 2.5.
    let at_t1 = DateTime::from_timestamp_micros(t1_us + 1).unwrap();
    let snapshot2 = weights_as_of(&conn, lambda, at_t1)
        .await
        .expect("weights_as_of at t1");
    let w2 = *snapshot2
        .get(&atom)
        .expect("atom must be in snapshot at t1");
    assert!((w2 - 2.5).abs() < 0.01, "expected 2.5 at t1, got {w2}");
}

#[tokio::test]
async fn test_weights_as_of_before_any_event_is_empty() {
    let conn = make_conn();
    let lambda = "lambda:test";
    let atom = Uuid::new_v4();
    let atom_str = atom.to_string();

    let t1_us: i64 = 2_000_000_000;
    insert_weight_event(&conn, lambda, &atom_str, 2.0, t1_us);

    // Query before t1: no rows.
    let before = DateTime::from_timestamp_micros(t1_us - 1).unwrap();
    let snapshot = weights_as_of(&conn, lambda, before)
        .await
        .expect("weights_as_of");
    assert!(
        snapshot.is_empty(),
        "snapshot before any event should be empty"
    );
}

#[tokio::test]
async fn test_rank_history_returns_ordered_events() {
    let conn = make_conn();
    let lambda = "lambda:rank_hist";
    let atom = Uuid::new_v4();
    let atom_str = atom.to_string();

    insert_weight_event(&conn, lambda, &atom_str, 1.2, 1_000);
    insert_weight_event(&conn, lambda, &atom_str, 1.4, 2_000);
    insert_weight_event(&conn, lambda, &atom_str, 1.1, 3_000);

    let history = rank_history(&conn, lambda, atom)
        .await
        .expect("rank_history");

    assert_eq!(history.len(), 3, "expected 3 history points");
    // Verify ascending timestamp order.
    assert!(history[0].ts <= history[1].ts);
    assert!(history[1].ts <= history[2].ts);
    // Verify weights.
    assert!((history[0].weight_after - 1.2).abs() < 0.01);
    assert!((history[1].weight_after - 1.4).abs() < 0.01);
    assert!((history[2].weight_after - 1.1).abs() < 0.01);
}

#[test]
fn test_compute_diff_report_jaccard() {
    let t1 = vec![
        Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
    ];
    let t2 = vec![
        Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap(),
        Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap(),
    ];

    let report = compute_diff_report(t1, t2);

    // |intersection| = 2 ({002, 003}), |union| = 4
    assert!(
        (report.jaccard - 0.5).abs() < 0.01,
        "jaccard={}",
        report.jaccard
    );
    assert_eq!(report.added.len(), 1, "one atom added");
    assert_eq!(report.dropped.len(), 1, "one atom dropped");
}

#[test]
fn test_compute_diff_report_identical() {
    let ids: Vec<Uuid> = (1..=3)
        .map(|i| Uuid::parse_str(&format!("00000000-0000-0000-0000-{:012}", i)).unwrap())
        .collect();

    let report = compute_diff_report(ids.clone(), ids);
    assert!((report.jaccard - 1.0).abs() < 0.001);
    assert!(report.added.is_empty());
    assert!(report.dropped.is_empty());
}

#[test]
fn test_compute_diff_report_disjoint() {
    let t1 = vec![Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()];
    let t2 = vec![Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap()];

    let report = compute_diff_report(t1, t2);
    assert!((report.jaccard - 0.0).abs() < 0.001);
    assert_eq!(report.added.len(), 1);
    assert_eq!(report.dropped.len(), 1);
}

#[tokio::test]
async fn test_adjustment_rate_per_day() {
    let conn = make_conn();
    let lambda = "lambda:rate_test";
    let atom = Uuid::new_v4();
    let atom_str = atom.to_string();

    // Insert 3 events: 2 "today" and 1 "yesterday".
    let now_us = Utc::now().timestamp_micros();
    let yesterday_us = now_us - 86_400_000_001_i64; // slightly over 24h ago

    insert_weight_event(&conn, lambda, &atom_str, 1.1, now_us - 100);
    insert_weight_event(&conn, lambda, &atom_str, 1.2, now_us - 50);
    insert_weight_event(&conn, lambda, &atom_str, 1.3, yesterday_us);

    let rates = metrics::adjustment_rate_per_day(&conn, lambda, 7)
        .await
        .expect("adjustment_rate_per_day");

    // At least 2 buckets (today and yesterday within 7 days).
    assert!(
        !rates.is_empty(),
        "expected at least 1 day bucket, got {:?}",
        rates
    );
    // Sum of all counts should be 3.
    let total: u64 = rates.iter().map(|(_, c)| c).sum();
    assert_eq!(total, 3, "expected 3 total events");
}
