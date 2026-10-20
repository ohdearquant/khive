use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::time::Duration;

use khive_storage::{
    scope_request_read_cancellation, StorageCapability, StorageError, StorageResult,
    VectorMetadataFilter, VectorSearchHit, VectorSearchRequest, VectorStore,
};
use khive_types::SubstrateKind;
use tokio::sync::Notify;
use uuid::Uuid;

use super::SqliteVecStore;
use crate::pool::{ConnectionPool, PoolConfig};

const MODEL: &str = "batch-model";
const WAIT: Duration = Duration::from_secs(10);

thread_local! {
    static AFTER_SLOT: RefCell<Option<Arc<AfterSlotGate>>> = const { RefCell::new(None) };
}

/// Only owned synchronization and a transaction-state observation cross the
/// worker boundary. The installed read progress handler is left untouched.
pub(super) struct AfterSlotGate {
    entered: AtomicBool,
    in_transaction: AtomicBool,
    reached: Notify,
    released: Mutex<bool>,
    resume: Condvar,
}

impl AfterSlotGate {
    pub(super) fn after_slot(&self, conn: &rusqlite::Connection) {
        if self.entered.swap(true, Ordering::SeqCst) {
            return;
        }
        self.in_transaction
            .store(!conn.is_autocommit(), Ordering::SeqCst);
        self.reached.notify_one();
        let (released, _) = self
            .resume
            .wait_timeout_while(self.released.lock().unwrap(), WAIT, |released| !*released)
            .unwrap();
        assert!(*released, "batch after-slot gate was not released");
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.resume.notify_all();
    }
}

pub(super) fn current_after_slot_gate() -> Option<Arc<AfterSlotGate>> {
    AFTER_SLOT.with(|gate| gate.borrow().clone())
}

struct GateRegistration(Arc<AfterSlotGate>);

impl GateRegistration {
    fn new() -> Self {
        let gate = Arc::new(AfterSlotGate {
            entered: AtomicBool::new(false),
            in_transaction: AtomicBool::new(false),
            reached: Notify::new(),
            released: Mutex::new(false),
            resume: Condvar::new(),
        });
        AFTER_SLOT.with(|current| {
            assert!(current.borrow().is_none());
            *current.borrow_mut() = Some(Arc::clone(&gate));
        });
        Self(gate)
    }
}

impl Drop for GateRegistration {
    fn drop(&mut self) {
        self.0.release();
        AFTER_SLOT.with(|current| *current.borrow_mut() = None);
    }
}

struct Fixture {
    pool: Arc<ConnectionPool>,
    store: Arc<SqliteVecStore>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(max_readers: usize) -> Self {
        crate::extension::ensure_extensions_loaded();
        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(dir.path().join("batch.db")),
                max_readers,
                write_queue_enabled: Some(false),
                write_routing_strict: false,
                checkout_timeout: Duration::from_millis(100),
                busy_timeout: Duration::from_secs(5),
                disk_guard_config: Some(crate::disk_guard_config::EffectiveDiskGuardConfig {
                    reserve_bytes: 0,
                    ..Default::default()
                }),
                volume_lock_dir: Some(dir.path().join("locks")),
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        pool.writer()
            .unwrap()
            .autocommit(|conn| {
                let journal: String =
                    conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
                assert_eq!(journal.to_ascii_lowercase(), "wal");
                conn.execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)?;
                conn.execute_batch(
                    "CREATE VIRTUAL TABLE vec_batch USING vec0(\
                     subject_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL, \
                     field TEXT NOT NULL, embedding_model TEXT NOT NULL, \
                     embedding float[3] distance_metric=cosine)",
                )?;
                Ok(())
            })
            .unwrap();
        let store = Arc::new(store(&pool, MODEL));
        Self {
            pool,
            store,
            _dir: dir,
        }
    }
}

fn store(pool: &Arc<ConnectionPool>, model: &str) -> SqliteVecStore {
    SqliteVecStore::new(
        Arc::clone(pool),
        true,
        "batch".into(),
        model.into(),
        3,
        "local".into(),
    )
    .unwrap()
}

fn request(top_k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        query_vectors: vec![vec![1.0, 0.0, 0.0]],
        top_k,
        namespace: None,
        kind: Some(SubstrateKind::Note),
        embedding_model: None,
        filter: None,
        backend_hints: None,
    }
}

async fn insert(
    store: &SqliteVecStore,
    id: u128,
    namespace: &str,
    kind: SubstrateKind,
    vector: [f32; 3],
) -> StorageResult<()> {
    store
        .insert(
            Uuid::from_u128(id),
            kind,
            namespace,
            "note.content",
            vec![vector.to_vec()],
        )
        .await
}

async fn seed(fixture: &Fixture) {
    for (id, vector) in [
        (20, [1.0, 0.0, 0.0]),
        (10, [1.0, 0.0, 0.0]),
        (30, [0.0, 1.0, 0.0]),
        (40, [-1.0, 0.0, 0.0]),
    ] {
        insert(&fixture.store, id, "local", SubstrateKind::Note, vector)
            .await
            .unwrap();
    }
    insert(
        &fixture.store,
        1,
        "hidden",
        SubstrateKind::Note,
        [1.0, 0.0, 0.0],
    )
    .await
    .unwrap();
    insert(
        &fixture.store,
        2,
        "local",
        SubstrateKind::Entity,
        [1.0, 0.0, 0.0],
    )
    .await
    .unwrap();
    insert(
        &store(&fixture.pool, "other-model"),
        3,
        "local",
        SubstrateKind::Entity,
        [1.0, 0.0, 0.0],
    )
    .await
    .unwrap();
}

fn rows(hits: &[VectorSearchHit]) -> Vec<(Uuid, f64, u32)> {
    hits.iter()
        .map(|hit| (hit.subject_id, hit.score.to_f64(), hit.rank))
        .collect()
}

#[tokio::test]
async fn batch_three_queries_match_individual_metadata_ties_scores_and_ranks() {
    let fixture = Fixture::new(2);
    seed(&fixture).await;
    let mut hidden = request(1);
    hidden.namespace = Some("hidden".into());
    let mut other_model = request(4097);
    other_model.kind = Some(SubstrateKind::Entity);
    other_model.embedding_model = Some("other-model".into());
    let requests = [request(4), hidden, other_model];
    let mut individual = Vec::new();
    for query in &requests {
        individual.push(rows(&fixture.store.search(query.clone()).await.unwrap()));
    }
    assert_eq!(
        individual,
        vec![
            vec![
                (Uuid::from_u128(10), 1.0, 1),
                (Uuid::from_u128(20), 1.0, 2),
                (Uuid::from_u128(30), 0.0, 3),
                (Uuid::from_u128(40), -1.0, 4),
            ],
            vec![(Uuid::from_u128(1), 1.0, 1)],
            vec![(Uuid::from_u128(3), 1.0, 1)],
        ]
    );
    let batch = fixture.store.search_batch(&requests).await.unwrap();
    assert_eq!(batch.len(), 3);
    for (result, expected) in batch.into_iter().zip(individual) {
        assert_eq!(rows(&result.unwrap()), expected);
    }
}

fn assert_slot_error(error: StorageError, expected: &str) {
    match (expected, error) {
        (
            "filter",
            StorageError::Unsupported {
                capability,
                operation,
                message,
            },
        ) => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(operation, "vec_search");
            assert_eq!(message, "use search_with_filter for filtered queries");
        }
        (
            "arity",
            StorageError::Unsupported {
                capability,
                operation,
                message,
            },
        ) => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(operation, "vec_search");
            assert_eq!(
                message,
                "sqlite-vec supports exactly one query vector per search"
            );
        }
        (
            "finite",
            StorageError::InvalidInput {
                capability,
                operation,
                message,
            },
        ) => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(operation, "vec_search");
            assert!(message.starts_with("non-finite value at index 1:"));
        }
        (
            "dimension" | "native",
            StorageError::Driver {
                capability,
                operation,
                source,
            },
        ) => {
            assert_eq!(capability, StorageCapability::Vectors);
            assert_eq!(operation, "vec_search");
            let native = source.downcast_ref::<rusqlite::Error>().unwrap();
            if expected == "dimension" {
                assert!(matches!(
                    native,
                    rusqlite::Error::InvalidParameterCount(2, 3)
                ));
            }
        }
        (expected, actual) => panic!("expected {expected} slot error, got {actual:?}"),
    }
}

#[tokio::test]
async fn batch_invalid_middle_is_isolated_and_keeps_search_error_precedence() {
    let fixture = Fixture::new(2);
    seed(&fixture).await;
    let mut filter = request(1);
    filter.query_vectors.clear();
    filter.filter = Some(VectorMetadataFilter {
        namespaces: vec!["local".into()],
        ..Default::default()
    });
    let mut arity = request(1);
    arity.query_vectors = vec![vec![f32::NAN; 3], vec![1.0; 3]];
    let mut finite = request(1);
    finite.query_vectors[0][1] = f32::NAN;
    let mut dimension = request(1);
    dimension.query_vectors = vec![vec![f32::NAN, 0.0]];
    let mut native = request(1);
    native.query_vectors = vec![vec![0.0; 3]];
    let mut last = request(1);
    last.query_vectors = vec![vec![-1.0, 0.0, 0.0]];
    for (invalid, expected) in [
        (filter, "filter"),
        (arity, "arity"),
        (finite, "finite"),
        (dimension, "dimension"),
        (native, "native"),
    ] {
        assert_slot_error(
            fixture.store.search(invalid.clone()).await.unwrap_err(),
            expected,
        );
        let batch = fixture
            .store
            .search_batch(&[request(2), invalid, last.clone()])
            .await
            .expect("a slot error is not an outer transport failure");
        assert_eq!(batch.len(), 3);
        let mut slots = batch.into_iter();
        assert_eq!(
            rows(&slots.next().unwrap().unwrap()),
            [(Uuid::from_u128(10), 1.0, 1), (Uuid::from_u128(20), 1.0, 2),]
        );
        assert_slot_error(slots.next().unwrap().unwrap_err(), expected);
        assert_eq!(
            rows(&slots.next().unwrap().unwrap()),
            [(Uuid::from_u128(40), 1.0, 1)]
        );
    }
}

#[tokio::test]
async fn batch_uses_one_checkout_returns_permit_and_keeps_admission_failure_outer() {
    let fixture = Fixture::new(2);
    seed(&fixture).await;
    let before = fixture.pool.reader_acquisition_snapshot();
    let batch = fixture
        .store
        .search_batch(&[request(1), request(2), request(3)])
        .await
        .unwrap();
    assert_eq!(batch.len(), 3);
    assert!(batch.iter().all(Result::is_ok));
    let after = fixture.pool.reader_acquisition_snapshot();
    assert_eq!(
        after.completed_pooled_checkouts - before.completed_pooled_checkouts,
        1
    );
    assert_eq!(after.active_pooled_checkouts, 0);
    assert_eq!(after.available_reader_admission_slots, 2);

    let first = fixture
        .pool
        .acquire_reader_admission(StorageCapability::Vectors, "batch_test_hold")
        .await
        .unwrap();
    let second = fixture
        .pool
        .acquire_reader_admission(StorageCapability::Vectors, "batch_test_hold")
        .await
        .unwrap();
    assert!(fixture.store.search_batch(&[]).await.unwrap().is_empty());
    let error = fixture.store.search_batch(&[request(1)]).await.unwrap_err();
    assert!(
        matches!(error, StorageError::AdmissionTimeout { operation, .. } if operation == "vec_search_batch")
    );
    assert_eq!(
        fixture
            .pool
            .reader_acquisition_snapshot()
            .completed_pooled_checkouts,
        after.completed_pooled_checkouts
    );
    drop((first, second));
    assert_eq!(
        fixture
            .store
            .search_batch(&[request(1)])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fixture
            .pool
            .reader_acquisition_snapshot()
            .available_reader_admission_slots,
        2
    );
}

#[tokio::test(flavor = "current_thread")]
async fn batch_holds_one_wal_snapshot_and_next_search_sees_committed_write() {
    let fixture = Fixture::new(2);
    insert(
        &fixture.store,
        20,
        "local",
        SubstrateKind::Note,
        [0.0, 1.0, 0.0],
    )
    .await
    .unwrap();
    let gate = GateRegistration::new();
    let store = Arc::clone(&fixture.store);
    let mut task = tokio::spawn(async move { store.search_batch(&[request(1), request(1)]).await });
    let reached = tokio::time::timeout(WAIT, gate.0.reached.notified()).await;
    let in_transaction = gate.0.in_transaction.load(Ordering::SeqCst);
    let write = if reached.is_ok() {
        Some(
            tokio::time::timeout(
                WAIT,
                insert(
                    &fixture.store,
                    10,
                    "local",
                    SubstrateKind::Note,
                    [1.0, 0.0, 0.0],
                ),
            )
            .await,
        )
    } else {
        None
    };
    gate.0.release();
    let joined = tokio::time::timeout(WAIT, &mut task).await;
    if joined.is_err() {
        task.abort();
    }
    reached.expect("batch reached its first completed query");
    write
        .unwrap()
        .expect("writer completed while snapshot was held")
        .unwrap();
    let batch = joined
        .expect("batch settled after gate release")
        .unwrap()
        .unwrap();
    assert_eq!(batch.len(), 2);
    for slot in batch {
        assert_eq!(rows(&slot.unwrap()), [(Uuid::from_u128(20), 0.0, 1)]);
    }
    assert!(
        in_transaction,
        "actual worker connection has an open read transaction"
    );
    assert_eq!(
        rows(&fixture.store.search(request(1)).await.unwrap()),
        [(Uuid::from_u128(10), 1.0, 1)]
    );
    assert_eq!(
        fixture
            .pool
            .reader_acquisition_snapshot()
            .active_pooled_checkouts,
        0
    );
}

#[tokio::test(flavor = "current_thread")]
async fn batch_cancellation_is_outer_and_reader_recovers_after_release() {
    let fixture = Fixture::new(2);
    seed(&fixture).await;
    let gate = GateRegistration::new();
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let store = Arc::clone(&fixture.store);
    let requests = [request(1), request(2)];
    let mut batch = Box::pin(scope_request_read_cancellation(
        receiver,
        store.search_batch(&requests),
    ));
    let reached = tokio::time::timeout(WAIT, async {
        tokio::select! {
            result = &mut batch => panic!("batch finished before the gate: {result:?}"),
            () = gate.0.reached.notified() => {},
        }
    })
    .await;
    sender.send(true).unwrap();
    // Poll the actual request while its worker cannot complete. The ready
    // cancellation branch latches ReadControl::cancel before waiting for that
    // worker; this does not depend on scheduling a separate watcher task.
    let polled = std::future::poll_fn(|cx| Poll::Ready(batch.as_mut().poll(cx))).await;
    gate.0.release();
    reached.expect("batch reached its first query before cancellation");
    assert!(polled.is_pending(), "the gated worker has not settled yet");
    let error = tokio::time::timeout(WAIT, batch)
        .await
        .expect("cancelled batch settled after gate release")
        .unwrap_err();
    assert!(
        matches!(error, StorageError::Timeout { operation } if operation == "vec_search_batch")
    );
    assert!(!fixture.store.search(request(1)).await.unwrap().is_empty());
    assert_eq!(
        fixture
            .pool
            .reader_acquisition_snapshot()
            .active_pooled_checkouts,
        0
    );
    assert_eq!(
        fixture
            .pool
            .reader_acquisition_snapshot()
            .available_reader_admission_slots,
        2
    );
}

#[tokio::test]
async fn batch_shared_reader_settles_before_writer_reuse() {
    let fixture = Fixture::new(0);
    assert_eq!(fixture.pool.max_readers(), 0);
    seed(&fixture).await;
    let mut invalid = request(1);
    invalid.query_vectors = vec![vec![1.0, 0.0]];
    let batch = fixture
        .store
        .search_batch(&[request(1), invalid, request(2)])
        .await
        .unwrap();
    assert_eq!(batch.len(), 3);
    assert!(batch[0].is_ok());
    assert!(batch[1].is_err());
    assert!(batch[2].is_ok());
    fixture
        .pool
        .writer()
        .unwrap()
        .autocommit(|conn| {
            assert!(conn.is_autocommit(), "shared read transaction settled");
            conn.execute_batch("CREATE TABLE after_batch (value INTEGER)")?;
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn stateful_shared_reader_with_unsettled_transaction_is_quarantined() {
    let fixture = Fixture::new(0);
    assert_eq!(fixture.pool.max_readers(), 0);
    seed(&fixture).await;
    super::super::run_stateful_pooled_store_read(
        Arc::clone(&fixture.pool),
        StorageCapability::Vectors,
        "batch_unsettled_fixture",
        |conn| {
            conn.execute_batch("BEGIN DEFERRED")
                .map_err(|error| super::map_err(error, "batch_unsettled_fixture"))?;
            conn.query_row("SELECT COUNT(*) FROM vec_batch", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|error| super::map_err(error, "batch_unsettled_fixture"))?;
            assert!(!conn.is_autocommit());
            // Deliberately leave the real native transaction open. The reader
            // owner must refuse reuse even though this callback returned Ok.
            Ok(())
        },
    )
    .await
    .unwrap();
    let error = fixture
        .pool
        .writer()
        .err()
        .expect("unsettled shared reader retires the writer");
    assert!(
        matches!(
            &error,
            crate::error::SqliteError::InvalidData(message)
                if message == "pooled writer connection retired after a terminal transaction fault"
        ),
        "expected pooled-writer retirement refusal, got {error:?}"
    );
}
