use super::*;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

const DEFAULT_ROWS: usize = 4_096;
const MAX_ROWS: usize = 32_768;
const ROWS_ENV: &str = "KHIVE_ANN_MEASUREMENT_ROWS";
const TICK_PERIOD: Duration = Duration::from_millis(10);
const BASELINE_TICKS: usize = 20;

fn parse_rows(raw: Option<&str>) -> Result<usize, String> {
    let rows = match raw {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| format!("{ROWS_ENV} must be an integer in 1..={MAX_ROWS}"))?,
        None => DEFAULT_ROWS,
    };
    if !(1..=MAX_ROWS).contains(&rows) {
        return Err(format!("{ROWS_ENV} must be in 1..={MAX_ROWS}"));
    }
    Ok(rows)
}

fn corpus_vector(row: usize) -> [f32; WARM_DIMS] {
    let mut state = (row as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mut vector = [0.0; WARM_DIMS];
    for value in &mut vector {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *value = (state as u16 as f32) / 32_768.0 - 1.0;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    for value in &mut vector {
        *value /= norm;
    }
    vector
}

async fn seed_measurement_corpus(rt: &KhiveRuntime, token: &NamespaceToken, rows: usize) {
    let _store = rt
        .vectors_for_model(token, WARM_TEST_MODEL)
        .expect("vec store");
    let table = format!("vec_{}", sanitize_model_key(WARM_TEST_MODEL));
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    for row in 0..rows {
        let id = Uuid::from_u128(row as u128 + 1).to_string();
        writer
            .execute(SqlStatement {
                sql: format!(
                    "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, 'concept', 'knowledge.atom', ?3, ?4)"
                ),
                params: vec![
                    SqlValue::Text(id.clone()),
                    SqlValue::Text(token.namespace().as_str().to_owned()),
                    SqlValue::Text(WARM_TEST_MODEL.to_owned()),
                    SqlValue::Blob(khive_storage::encode_f32_native(&corpus_vector(row))),
                ],
                label: None,
            })
            .await
            .expect("insert measurement vector");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, 'concept', 'knowledge.atom', ?3, 'upsert')"
                    .into(),
                params: vec![
                    SqlValue::Text(token.namespace().as_str().to_owned()),
                    SqlValue::Text(WARM_TEST_MODEL.to_owned()),
                    SqlValue::Text(id),
                ],
                label: None,
            })
            .await
            .expect("append measurement write log");
    }
}

struct TickGaps {
    previous: Instant,
    max: Duration,
    samples: usize,
}

impl TickGaps {
    fn new(start: Instant) -> Self {
        Self {
            previous: start,
            max: Duration::ZERO,
            samples: 0,
        }
    }

    fn record(&mut self, now: Instant) {
        self.max = self.max.max(now.duration_since(self.previous));
        self.previous = now;
        self.samples += 1;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in timing measurement; see docs/benchmarks.md"]
async fn measure_inline_ann_build_tick_gap() {
    let raw_rows = match std::env::var(ROWS_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => panic!("invalid {ROWS_ENV}: {error}"),
    };
    let rows = parse_rows(raw_rows.as_deref()).expect("bounded measurement row count");
    let dir = TempDir::new().expect("measurement directory");
    let rt = file_rt_with_embedder(dir.path().join("measurement.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_measurement_corpus(&rt, &token, rows).await;

    let finished = Arc::new(AtomicBool::new(false));
    let ticker_finished = Arc::clone(&finished);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let ticker = tokio::spawn(async move {
        let mut interval = tokio::time::interval(TICK_PERIOD);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        let mut baseline = TickGaps::new(Instant::now());
        for _ in 0..BASELINE_TICKS {
            interval.tick().await;
            baseline.record(Instant::now());
        }
        let mut during = TickGaps::new(baseline.previous);
        ready_tx.send(()).expect("measurement waiting for ticker");
        loop {
            interval.tick().await;
            during.record(Instant::now());
            // Check after recording so completion cannot discard a delayed final tick.
            if ticker_finished.load(Ordering::Acquire) {
                break;
            }
        }
        (baseline, during)
    });
    ready_rx.await.expect("ticker baseline complete");

    // The tokio::test body runs on block_on's thread. Spawn to exercise a runtime worker.
    let build_rt = rt.clone();
    let build = tokio::spawn(async move {
        let start = Instant::now();
        let result = load_and_build_from_vector_store(&build_rt, &token, WARM_TEST_MODEL).await;
        let elapsed = start.elapsed();
        (result, elapsed)
    })
    .await;
    finished.store(true, Ordering::Release);
    let (baseline, during) = ticker.await.expect("ticker task");
    let (result, elapsed) = build.expect("load/build task");
    let bridge = result.expect("load/build result").expect("nonempty corpus");
    assert_eq!(
        bridge.id_map.len(),
        rows,
        "all seeded vectors must be built"
    );
    assert_eq!(bridge.index.live_count(), rows);

    println!(
        "ANN_RUNTIME_MEASUREMENT {}",
        json!({
            "rows": rows,
            "dimensions": WARM_DIMS,
            "corpus": "xorshift64-normalized-v1",
            "worker_threads": 2,
            "tick_period_ms": TICK_PERIOD.as_millis(),
            "baseline_tick_count": baseline.samples,
            "baseline_max_tick_gap_ms": baseline.max.as_secs_f64() * 1000.0,
            "load_and_build_ms": elapsed.as_secs_f64() * 1000.0,
            "tick_count": during.samples,
            "max_tick_gap_ms": during.max.as_secs_f64() * 1000.0,
            "exceeds_one_second": elapsed > Duration::from_secs(1),
            "debug_assertions": cfg!(debug_assertions),
            "target_arch": std::env::consts::ARCH,
            "target_os": std::env::consts::OS,
        })
    );
}

#[test]
fn row_limit_rejects_invalid_or_unbounded_corpora() {
    assert_eq!(parse_rows(None).expect("default"), DEFAULT_ROWS);
    assert_eq!(parse_rows(Some("32768")).expect("limit"), MAX_ROWS);
    for invalid in ["0", "32769", "-1", "", "garbage", "18446744073709551616"] {
        assert!(parse_rows(Some(invalid)).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn deterministic_corpus_has_distinct_normalized_vectors() {
    let first = corpus_vector(0);
    assert_eq!(first, corpus_vector(0));
    assert_ne!(first, corpus_vector(1));
    for row in [0, 1, DEFAULT_ROWS - 1, MAX_ROWS - 1] {
        let vector = corpus_vector(row);
        assert!(vector.iter().all(|value| value.is_finite()));
        let squared_norm: f32 = vector.iter().map(|value| value * value).sum();
        assert!((squared_norm - 1.0).abs() < 1e-5);
    }
}

#[test]
fn delayed_final_tick_is_retained_as_the_maximum_gap() {
    let start = Instant::now();
    let mut gaps = TickGaps::new(start);
    gaps.record(start + TICK_PERIOD);
    gaps.record(start + TICK_PERIOD * 2);
    gaps.record(start + TICK_PERIOD * 2 + Duration::from_secs(2));
    assert_eq!(gaps.samples, 3);
    assert_eq!(gaps.max, Duration::from_secs(2));
}
