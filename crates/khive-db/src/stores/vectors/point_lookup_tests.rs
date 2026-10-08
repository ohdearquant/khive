use std::collections::HashMap;

use super::*;
use crate::pool::PoolConfig;

const MODEL: &str = "point_lookup";
const NAMESPACE: &str = "ns:point_lookup";
const TABLE: &str = "vec_point_lookup";

struct StoredVector {
    id: Uuid,
    namespace: &'static str,
    model: &'static str,
    kind: &'static str,
    field: &'static str,
    embedding: [f32; 2],
}

struct Fixture {
    pool: Arc<ConnectionPool>,
    store: SqliteVecStore,
    requested: Vec<Uuid>,
    stored: Vec<StoredVector>,
}

impl Fixture {
    fn new() -> Self {
        crate::extension::ensure_extensions_loaded();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                write_queue_enabled: Some(false),
                ..PoolConfig::for_test()
            })
            .expect("in-memory pool"),
        );
        let store = SqliteVecStore::new(
            Arc::clone(&pool),
            false,
            MODEL.into(),
            MODEL.into(),
            2,
            NAMESPACE.into(),
        )
        .expect("vector store");
        let mut requested: Vec<Uuid> = (1..=805).map(Uuid::from_u128).collect();
        // Exercise duplicates both within and across the original 399-ID groups.
        requested[7] = requested[2];
        requested[401] = requested[2];
        let stored: Vec<StoredVector> = (0..806)
            .filter(|index| index % 4 != 0)
            .map(|index| StoredVector {
                id: Uuid::from_u128(index + 1),
                namespace: if index % 5 == 0 {
                    "ns:other"
                } else {
                    NAMESPACE
                },
                model: if index % 7 == 0 { "other_model" } else { MODEL },
                kind: if index % 2 == 0 { "entity" } else { "note" },
                field: if index % 2 == 0 { "body" } else { "title" },
                embedding: match index % 3 {
                    0 => [1.0, 0.0],
                    1 => [0.0, 1.0],
                    _ => [-1.0, 0.0],
                },
            })
            .collect();
        {
            let writer = pool.try_writer().expect("pool writer");
            let conn = writer.conn();
            conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_point_lookup USING vec0(\
                     subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
                     kind TEXT NOT NULL, field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
                     embedding float[2] distance_metric=cosine)",
            )
            .expect("create vector table");
            conn.execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)
                .expect("create write log");
            conn.execute_batch(crate::migrations::VECTOR_PROVENANCE_DDL)
                .expect("create provenance sidecar");
            let mut insert = conn
                .prepare(
                    "INSERT INTO vec_point_lookup \
                         (subject_id, namespace, embedding_model, kind, field, embedding) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .expect("prepare seed insert");
            let mut provenance = conn
                .prepare(
                    "INSERT INTO vector_provenance \
                         (model_key, subject_id, namespace, embedding_digest) \
                         VALUES (?1, ?2, ?3, ?4)",
                )
                .expect("prepare seed provenance");
            for vector in &stored {
                insert
                    .execute(rusqlite::params![
                        vector.id.to_string(),
                        vector.namespace,
                        vector.model,
                        vector.kind,
                        vector.field,
                        encode_f32_native(&vector.embedding),
                    ])
                    .expect("seed vector");
                provenance
                    .execute(rusqlite::params![
                        MODEL,
                        vector.id.to_string(),
                        vector.namespace,
                        blake3::hash(&encode_f32_native(&vector.embedding))
                            .to_hex()
                            .to_string(),
                    ])
                    .expect("seed provenance");
            }
        }
        Self {
            pool,
            store,
            requested,
            stored,
        }
    }

    fn expected_exists(&self) -> HashSet<Uuid> {
        let requested: HashSet<Uuid> = self.requested.iter().copied().collect();
        self.stored
            .iter()
            .filter(|vector| {
                requested.contains(&vector.id)
                    && vector.namespace == NAMESPACE
                    && vector.model == MODEL
            })
            .map(|vector| vector.id)
            .collect()
    }

    fn expected_scores(&self) -> Vec<(Uuid, DeterministicScore, u32)> {
        let stored: HashMap<_, _> = self.stored.iter().map(|v| (v.id, v)).collect();
        let mut expected = Vec::new();
        for chunk in self.requested.chunks(399) {
            let mut seen = HashSet::new();
            for id in chunk.iter().filter(|id| seen.insert(*id)) {
                if let Some(vector) = stored.get(id) {
                    if vector.namespace == NAMESPACE && vector.model == MODEL {
                        expected.push((*id, DeterministicScore::from_f32(vector.embedding[0]), 0));
                    }
                }
            }
        }
        expected.sort_by(|a, b| cmp_desc_then_id(a.1, &a.0, b.1, &b.0));
        for (index, hit) in expected.iter_mut().enumerate() {
            hit.2 = (index + 1) as u32;
        }
        expected
    }
}

// Capture statements executed by the public operations, so planner checks
// cannot accidentally exercise a separate copy of their SQL. This rides the
// pool's shared statement observer: SQLite keeps one trace callback per
// connection, so a second hook here would silence the observer.
struct StatementCapture {
    observation: crate::StatementStartObservation,
}

impl StatementCapture {
    fn new(pool: Arc<ConnectionPool>) -> Self {
        // The bound turns an unexpectedly long run into an error rather
        // than a silently short list.
        let observation = pool
            .observe_test_statement_starts(100_000)
            .expect("observe statement starts");
        Self { observation }
    }

    fn finish(self) -> Vec<String> {
        self.observation
            .started_statements()
            .expect("captured statements")
            .into_iter()
            .map(|statement| statement.sql)
            .collect()
    }
}

fn assert_point_plans(pool: &ConnectionPool, statements: &[String], prefix: &str) {
    let statements: HashSet<_> = statements
        .iter()
        .filter(|sql| sql.starts_with(prefix) && sql.contains(TABLE))
        .collect();
    assert!(
        !statements.is_empty(),
        "production statement was not captured: {prefix}"
    );
    let writer = pool.try_writer().expect("pool writer");
    for sql in statements {
        let mut explain = writer
            .conn()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("explain production statement");
        let mut rows = explain.raw_query();
        let mut virtual_plans = Vec::new();
        while let Some(row) = rows.next().expect("query plan row") {
            let detail: String = row.get(3).expect("query plan detail");
            if detail.contains("VIRTUAL TABLE INDEX ") {
                virtual_plans.push(detail);
            }
        }
        assert_eq!(virtual_plans.len(), 1, "expected one vec0 plan for {sql}");
        // sqlite-vec 0.1.9 uses idxStr '2!___' for its POINT plan and '1'
        // for FULLSCAN. idxNum depends on the projected columns.
        assert!(
            virtual_plans[0]
                .rsplit_once(':')
                .is_some_and(|(_, index)| index.starts_with("2!")),
            "expected vec0 POINT plan for {sql}, got {virtual_plans:?}"
        );
    }
}

#[tokio::test]
async fn batch_exists_uses_point_plan_for_mixed_ids_across_chunks() {
    let fixture = Fixture::new();
    let capture = StatementCapture::new(Arc::clone(&fixture.pool));
    let found = fixture
        .store
        .batch_exists(&fixture.requested, NAMESPACE)
        .await
        .expect("batch existence check");
    let statements = capture.finish();
    assert_eq!(found, fixture.expected_exists());
    assert_point_plans(&fixture.pool, &statements, "SELECT subject_id FROM ");
}

#[tokio::test]
async fn score_candidates_uses_point_plan_for_mixed_ids_across_chunks() {
    let fixture = Fixture::new();
    let capture = StatementCapture::new(Arc::clone(&fixture.pool));
    let hits = fixture
        .store
        .score_candidates(&[1.0, 0.0], &fixture.requested)
        .await
        .expect("score candidates");
    let statements = capture.finish();
    let actual: Vec<_> = hits
        .iter()
        .map(|hit| (hit.subject_id, hit.score, hit.rank))
        .collect();
    assert_eq!(actual, fixture.expected_scores());
    assert_point_plans(&fixture.pool, &statements, "SELECT e.subject_id, ");
}

#[tokio::test]
async fn delete_subjects_uses_point_plans_for_mixed_ids_across_chunks() {
    let fixture = Fixture::new();
    let requested: HashSet<_> = fixture.requested.iter().copied().collect();
    let mut expected_log: Vec<_> = fixture
        .stored
        .iter()
        .filter(|vector| requested.contains(&vector.id))
        .map(|vector| {
            (
                vector.namespace.to_string(),
                vector.model.to_string(),
                vector.kind.to_string(),
                vector.field.to_string(),
                vector.id.to_string(),
                "delete".to_string(),
            )
        })
        .collect();
    expected_log.sort();
    let expected_remaining: HashSet<_> = fixture
        .stored
        .iter()
        .filter(|vector| !requested.contains(&vector.id))
        .map(|vector| vector.id.to_string())
        .collect();
    assert!(
        !expected_remaining.is_empty(),
        "fixture must retain unrequested vectors"
    );
    let capture = StatementCapture::new(Arc::clone(&fixture.pool));
    let deleted = fixture
        .store
        .delete_subjects(&fixture.requested)
        .await
        .expect("delete subjects");
    let statements = capture.finish();
    assert_eq!(deleted as usize, expected_log.len());
    {
        let writer = fixture.pool.try_writer().expect("pool writer");
        let conn = writer.conn();
        let mut log = conn
            .prepare(
                "SELECT namespace, embedding_model, kind, field, subject_id, op FROM ann_write_log",
            )
            .expect("read delete log");
        let mut actual_log: Vec<(String, String, String, String, String, String)> = log
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .expect("query delete log")
            .collect::<Result<_, _>>()
            .expect("delete log rows");
        actual_log.sort();
        assert_eq!(actual_log, expected_log);
        for sql in [
            "SELECT subject_id FROM vec_point_lookup",
            "SELECT subject_id FROM vector_provenance WHERE model_key = 'point_lookup'",
        ] {
            let actual: HashSet<String> = conn
                .prepare(sql)
                .expect("read surviving identities")
                .query_map([], |row| row.get(0))
                .expect("query surviving identities")
                .collect::<Result<_, _>>()
                .expect("surviving identities");
            assert_eq!(actual, expected_remaining);
        }
        assert!(conn.is_autocommit(), "delete must finish its transaction");
    }
    assert_point_plans(&fixture.pool, &statements, "INSERT INTO ann_write_log ");
    assert_point_plans(&fixture.pool, &statements, "DELETE FROM vec_point_lookup ");
}

#[tokio::test]
async fn orphan_sweep_uses_point_plans_across_batches() {
    let fixture = Fixture::new();
    // Every ninth stored vector keeps a live entity. The rest are orphans,
    // enough for one full 400-victim batch and a partial one.
    let live: HashSet<String> = fixture
        .stored
        .iter()
        .enumerate()
        .filter(|(index, _)| index % 9 == 0)
        .map(|(_, vector)| vector.id.to_string())
        .collect();
    {
        let writer = fixture.pool.try_writer().expect("pool writer");
        let conn = writer.conn();
        conn.execute_batch(
            "CREATE TABLE entities (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE notes (id TEXT PRIMARY KEY, deleted_at INTEGER); \
                 CREATE TABLE knowledge_atoms (id TEXT PRIMARY KEY, deleted_at INTEGER)",
        )
        .expect("create live-subject tables");
        let mut insert = conn
            .prepare("INSERT INTO entities (id, deleted_at) VALUES (?1, NULL)")
            .expect("prepare live entity insert");
        for id in &live {
            insert.execute([id.as_str()]).expect("seed live entity");
        }
    }
    let mut expected_log: Vec<_> = fixture
        .stored
        .iter()
        .filter(|vector| !live.contains(&vector.id.to_string()))
        .map(|vector| {
            (
                vector.namespace.to_string(),
                vector.model.to_string(),
                vector.kind.to_string(),
                vector.field.to_string(),
                vector.id.to_string(),
                "delete".to_string(),
            )
        })
        .collect();
    expected_log.sort();
    assert!(
        expected_log.len() > 400,
        "fixture must need more than one delete batch"
    );
    let capture = StatementCapture::new(Arc::clone(&fixture.pool));
    let result = fixture
        .store
        .orphan_sweep(&OrphanSweepConfig {
            subject_id_allowlist: None,
            namespaces: vec![],
            substrate_kinds: vec![],
            max_delete: 1000,
            dry_run: false,
        })
        .await
        .expect("orphan sweep");
    let statements = capture.finish();
    assert_eq!(result.scanned as usize, fixture.stored.len());
    assert_eq!(result.would_delete as usize, expected_log.len());
    assert_eq!(result.deleted as usize, expected_log.len());
    assert!(!result.max_delete_hit);
    {
        let writer = fixture.pool.try_writer().expect("pool writer");
        let conn = writer.conn();
        let mut log = conn
            .prepare(
                "SELECT namespace, embedding_model, kind, field, subject_id, op \
                     FROM ann_write_log",
            )
            .expect("read delete log");
        let mut actual_log: Vec<(String, String, String, String, String, String)> = log
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .expect("query delete log")
            .collect::<Result<_, _>>()
            .expect("delete log rows");
        actual_log.sort();
        assert_eq!(actual_log, expected_log);
        for sql in [
            "SELECT subject_id FROM vec_point_lookup",
            "SELECT subject_id FROM vector_provenance WHERE model_key = 'point_lookup'",
        ] {
            let actual: HashSet<String> = conn
                .prepare(sql)
                .expect("read surviving identities")
                .query_map([], |row| row.get(0))
                .expect("query surviving identities")
                .collect::<Result<_, _>>()
                .expect("surviving identities");
            assert_eq!(actual, live);
        }
        assert!(conn.is_autocommit(), "sweep must finish its transaction");
    }
    assert_point_plans(&fixture.pool, &statements, "INSERT INTO ann_write_log ");
    assert_point_plans(&fixture.pool, &statements, "DELETE FROM vec_point_lookup ");
}

#[tokio::test]
async fn observer_still_records_after_a_statement_capture_is_dropped() {
    let fixture = Fixture::new();
    drop(StatementCapture::new(Arc::clone(&fixture.pool)));
    let observation = fixture
        .pool
        .observe_test_statement_starts(8)
        .expect("observe after capture");
    let sql = "SELECT 17 AS after_capture";
    {
        let writer = fixture.pool.try_writer().expect("pool writer");
        let value: i64 = writer
            .conn()
            .query_row(sql, [], |row| row.get(0))
            .expect("run statement");
        assert_eq!(value, 17);
    }
    assert_eq!(
        observation.started_statements().expect("observed starts"),
        vec![crate::StartedStatement {
            sql: sql.to_owned(),
            readonly: true,
        }],
        "a dropped capture must leave the shared observer recording"
    );
}
