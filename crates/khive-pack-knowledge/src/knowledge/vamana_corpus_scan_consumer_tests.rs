use super::*;
use khive_storage::types::StorageResult;
use khive_storage::{SqlReader, SqlRow, SqlRowConsumer, SqlStatement, StorageError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

type Corpus = Option<(Vec<f32>, Vec<Uuid>, u64)>;

#[derive(Clone, Copy)]
enum FirstWatermark {
    Value,
    Missing,
    Negative,
    Text,
}

#[derive(Clone, Copy)]
enum Fixture {
    Mixed(FirstWatermark),
    Empty,
    Invalid,
    Ordered(usize),
}

impl Fixture {
    fn len(self) -> usize {
        match self {
            Self::Mixed(_) => 7,
            Self::Empty => 0,
            Self::Invalid => 5,
            Self::Ordered(rows) => rows,
        }
    }

    fn row(self, index: usize) -> SqlRow {
        let mut columns = vec![("log_s", SqlValue::Integer(79))];
        if let Self::Ordered(_) = self {
            columns.push((
                "subject_id",
                SqlValue::Text(Uuid::from_u128(index as u128 + 1).to_string()),
            ));
            columns.push((
                "embedding",
                SqlValue::Blob(khive_storage::encode_f32_native(&[
                    index as f32,
                    -(index as f32),
                ])),
            ));
            return sql_row(columns);
        }
        if index == 0 {
            columns.clear();
            match self {
                Self::Mixed(FirstWatermark::Missing) => {}
                Self::Mixed(FirstWatermark::Negative) => {
                    columns.push(("log_s", SqlValue::Integer(-1)))
                }
                Self::Mixed(FirstWatermark::Text) => {
                    columns.push(("log_s", SqlValue::Text("71".into())))
                }
                _ => columns.push(("log_s", SqlValue::Integer(71))),
            }
        }
        match index {
            0 => columns.push(("subject_id", SqlValue::Integer(9))),
            1 => columns.push(("subject_id", SqlValue::Text("!invalid-first".into()))),
            2 => columns.push(("subject_id", SqlValue::Text(Uuid::from_u128(2).to_string()))),
            3 => {
                columns.push(("subject_id", SqlValue::Text(Uuid::from_u128(3).to_string())));
                columns.push(("embedding", SqlValue::Text("not bytes".into())));
            }
            4 => {
                columns.push(("subject_id", SqlValue::Text(Uuid::from_u128(4).to_string())));
                columns.push(("embedding", SqlValue::Blob(vec![1, 2, 3])));
            }
            5 | 6 => {
                columns.push((
                    "subject_id",
                    SqlValue::Text(Uuid::from_u128(index as u128).to_string()),
                ));
                let values = if index == 5 {
                    [f32::from_bits(0x8000_0000), f32::from_bits(0x3f80_0001)]
                } else {
                    [f32::MIN_POSITIVE, -1.25]
                };
                columns.push((
                    "embedding",
                    SqlValue::Blob(khive_storage::encode_f32_native(&values)),
                ));
            }
            _ => unreachable!(),
        }
        sql_row(columns)
    }
}

fn sql_row(columns: Vec<(&str, SqlValue)>) -> SqlRow {
    SqlRow {
        columns: columns
            .into_iter()
            .map(|(name, value)| khive_storage::types::SqlColumn {
                name: name.to_owned(),
                value,
            })
            .collect(),
    }
}

#[derive(Default)]
struct Witness {
    live_raw_rows: AtomicUsize,
    raw_rows_high_water: AtomicUsize,
    query_all_calls: AtomicUsize,
    query_consume_calls: AtomicUsize,
    statements: Mutex<Vec<SqlStatement>>,
}

// Witness for fixture-owned raw rows, not engine buffering or decoded output.
// Drop also balances the counter when push_row returns an error.
struct RawRowVisit<'a> {
    witness: &'a Witness,
}

impl<'a> RawRowVisit<'a> {
    fn new(witness: &'a Witness) -> Self {
        let live = witness.live_raw_rows.fetch_add(1, Ordering::SeqCst) + 1;
        witness
            .raw_rows_high_water
            .fetch_max(live, Ordering::SeqCst);
        Self { witness }
    }
}

impl Drop for RawRowVisit<'_> {
    fn drop(&mut self) {
        self.witness.live_raw_rows.fetch_sub(1, Ordering::SeqCst);
    }
}

struct LazyReader {
    fixture: Fixture,
    witness: Arc<Witness>,
    fail_after: Option<usize>,
    wrong_consumer: bool,
}

impl LazyReader {
    fn new(fixture: Fixture) -> Self {
        Self {
            fixture,
            witness: Arc::new(Witness::default()),
            fail_after: None,
            wrong_consumer: false,
        }
    }
}

#[async_trait::async_trait]
impl SqlReader for LazyReader {
    async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
        Err(StorageError::Internal("unexpected query_row".into()))
    }

    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.witness.query_all_calls.fetch_add(1, Ordering::SeqCst);
        self.witness.statements.lock().unwrap().push(statement);
        let mut rows = Vec::new();
        for index in 0..self.fixture.len() {
            rows.push(self.fixture.row(index));
            self.witness
                .raw_rows_high_water
                .fetch_max(rows.len(), Ordering::SeqCst);
        }
        Ok(rows)
    }

    async fn query_consume(
        &mut self,
        statement: SqlStatement,
        mut consumer: Box<dyn SqlRowConsumer>,
    ) -> StorageResult<Box<dyn SqlRowConsumer>> {
        self.witness
            .query_consume_calls
            .fetch_add(1, Ordering::SeqCst);
        self.witness.statements.lock().unwrap().push(statement);
        // Construct one raw row on demand and finish its visit before the next.
        // No fixture stores a Vec<SqlRow> ahead of this loop.
        for index in 0..self.fixture.len() {
            if self.fail_after == Some(index) {
                return Err(StorageError::Internal(
                    "deliberate corpus read failure".into(),
                ));
            }
            let _visit = RawRowVisit::new(self.witness.as_ref());
            consumer.push_row(self.fixture.row(index))?;
        }
        if self.wrong_consumer {
            Ok(Box::new(UnexpectedConsumer))
        } else {
            Ok(consumer)
        }
    }

    async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
        Err(StorageError::Internal("unexpected query_scalar".into()))
    }

    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        Err(StorageError::Internal("unexpected explain".into()))
    }
}

struct UnexpectedConsumer;

impl SqlRowConsumer for UnexpectedConsumer {
    fn push_row(&mut self, _: SqlRow) -> StorageResult<()> {
        Ok(())
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self
    }
}

fn statement() -> SqlStatement {
    knowledge_corpus("local").corpus_scan(
        "vec_consumer_fixture",
        "fixture-model",
        "vamana_corpus_scan",
    )
}

// Test-only reference: the old query_all path and its filtering order.
async fn old_scan(
    reader: &mut dyn SqlReader,
    statement: SqlStatement,
    dims: usize,
) -> StorageResult<Corpus> {
    let rows = reader.query_all(statement).await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut id_map = Vec::with_capacity(rows.len());
    let mut flat = Vec::with_capacity(rows.len() * dims);
    let scan_watermark = rows
        .first()
        .and_then(|row| row.i64_or_none("log_s"))
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0);
    for row in &rows {
        let id_str = match row.get("subject_id") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => continue,
        };
        let uuid = match Uuid::parse_str(id_str) {
            Ok(id) => id,
            Err(_) => continue,
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(b)) => b.as_slice(),
            _ => continue,
        };
        if bytes.len() != dims * 4 {
            continue;
        }
        let Ok(vec) = khive_storage::decode_f32_native(bytes) else {
            continue;
        };
        id_map.push(uuid);
        flat.extend_from_slice(&vec);
    }
    if id_map.is_empty() {
        Ok(None)
    } else {
        Ok(Some((flat, id_map, scan_watermark)))
    }
}

fn assert_corpus_equal(actual: Corpus, expected: Corpus) {
    match (actual, expected) {
        (None, None) => {}
        (Some((flat, ids, watermark)), Some((old_flat, old_ids, old_watermark))) => {
            assert_eq!(
                khive_storage::encode_f32_native(&flat),
                khive_storage::encode_f32_native(&old_flat)
            );
            assert_eq!(ids, old_ids);
            assert_eq!(watermark, old_watermark);
        }
        _ => panic!("corpus presence differs"),
    }
}

#[tokio::test]
async fn corpus_scan_consumer_matches_old_ordered_malformed_rows_byte_for_byte() {
    let fixture = Fixture::Mixed(FirstWatermark::Value);
    let mut old_reader = LazyReader::new(fixture);
    let mut reader = LazyReader::new(fixture);
    let expected = old_scan(&mut old_reader, statement(), 2).await.unwrap();
    let actual = scan_corpus_rows(&mut reader, statement(), 2).await.unwrap();
    let (flat, ids, watermark) = actual.as_ref().expect("two valid rows");
    assert_eq!(ids.as_slice(), &[Uuid::from_u128(5), Uuid::from_u128(6)]);
    assert_eq!(
        *watermark, 71,
        "first raw row is malformed but owns the watermark"
    );
    assert_eq!(flat[0].to_bits(), 0x8000_0000);
    assert_eq!(flat[1].to_bits(), 0x3f80_0001);
    assert_corpus_equal(actual, expected);
    let seen = reader.witness.statements.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let expected_statement = statement();
    assert_eq!(seen[0].sql.as_bytes(), expected_statement.sql.as_bytes());
    assert_eq!(
        serde_json::to_value(&seen[0].params).unwrap(),
        serde_json::to_value(&expected_statement.params).unwrap()
    );
    assert_eq!(seen[0].label, expected_statement.label);
}

#[tokio::test]
async fn corpus_scan_consumer_uses_only_the_first_raw_watermark() {
    for watermark in [
        FirstWatermark::Missing,
        FirstWatermark::Negative,
        FirstWatermark::Text,
    ] {
        let fixture = Fixture::Mixed(watermark);
        let mut old_reader = LazyReader::new(fixture);
        let mut reader = LazyReader::new(fixture);
        let actual = scan_corpus_rows(&mut reader, statement(), 2).await.unwrap();
        assert_eq!(actual.as_ref().unwrap().2, 0);
        assert_corpus_equal(
            actual,
            old_scan(&mut old_reader, statement(), 2).await.unwrap(),
        );
    }
}

#[tokio::test]
async fn corpus_scan_consumer_empty_and_all_invalid_are_none() {
    for fixture in [Fixture::Empty, Fixture::Invalid] {
        let mut old_reader = LazyReader::new(fixture);
        let mut reader = LazyReader::new(fixture);
        assert!(scan_corpus_rows(&mut reader, statement(), 2)
            .await
            .unwrap()
            .is_none());
        assert!(old_scan(&mut old_reader, statement(), 2)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn corpus_scan_consumer_errors_do_not_become_empty_corpora() {
    let mut reader = LazyReader::new(Fixture::Ordered(3));
    reader.fail_after = Some(1);
    let failed = scan_corpus_rows(&mut reader, statement(), 2).await;
    assert!(
        matches!(failed, Err(StorageError::Internal(ref message)) if message == "deliberate corpus read failure")
    );
    assert_eq!(reader.witness.live_raw_rows.load(Ordering::SeqCst), 0);
    reader.fail_after = None;
    reader.wrong_consumer = true;
    let wrong = scan_corpus_rows(&mut reader, statement(), 2).await;
    assert!(
        matches!(wrong, Err(StorageError::Internal(ref message)) if message == "unexpected corpus SQL consumer")
    );
}

#[tokio::test]
async fn corpus_scan_consumer_holds_at_most_one_raw_row() {
    let mut reader = LazyReader::new(Fixture::Ordered(32));
    let result = scan_corpus_rows(&mut reader, statement(), 2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.1.len(), 32);
    assert_eq!(reader.witness.live_raw_rows.load(Ordering::SeqCst), 0);
    let observed = reader.witness.raw_rows_high_water.load(Ordering::SeqCst);
    assert_eq!(observed, 1, "observed raw rows held at once: {observed}");
    assert_eq!(reader.witness.query_all_calls.load(Ordering::SeqCst), 0);
    assert_eq!(reader.witness.query_consume_calls.load(Ordering::SeqCst), 1);
}

struct UnsupportedReader {
    query_all_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SqlReader for UnsupportedReader {
    async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
        Err(StorageError::Internal("unexpected fallback".into()))
    }

    async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.query_all_calls.fetch_add(1, Ordering::SeqCst);
        Err(StorageError::Internal("unexpected fallback".into()))
    }

    async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
        Err(StorageError::Internal("unexpected fallback".into()))
    }

    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        Err(StorageError::Internal("unexpected fallback".into()))
    }
}

#[tokio::test]
async fn corpus_scan_consumer_unsupported_surfaces_internal_without_query_all_fallback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut reader = UnsupportedReader {
        query_all_calls: Arc::clone(&calls),
    };
    let error = scan_corpus_rows(&mut reader, statement(), 2)
        .await
        .expect_err("unsupported consumer scan must refuse");
    assert!(matches!(&error, StorageError::Unsupported {
        capability: khive_storage::StorageCapability::Sql,
        operation,
        ..
    } if operation.as_ref() == "query_consume"));
    let expected_message = error.to_string();
    match map_corpus_scan_error(error) {
        RuntimeError::Internal(message) => {
            assert_eq!(message, expected_message);
            assert!(message.contains("this reader does not support row consumption"));
        }
        _ => panic!("consumer refusal must keep the legacy Internal runtime mapping"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// Deadlock guards only; these durations are not performance expectations.
const NATIVE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
const CONSUMER_FAILSAFE: std::time::Duration = std::time::Duration::from_secs(60);

// Native fixtures exercise the production helper and its unchanged corpus
// statement through a real file-backed runtime. An ordinary private table has
// the same columns as the corpus query; no embedding service is needed.
async fn native_fixture() -> (tempfile::TempDir, KhiveRuntime) {
    let dir = tempfile::tempdir().expect("native corpus fixture directory");
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("corpus-consumer.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed corpus fixture runtime");
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute(SqlStatement {
            sql: "CREATE TABLE vec_consumer_fixture (\
                  subject_id TEXT NOT NULL, namespace TEXT NOT NULL, \
                  kind TEXT NOT NULL, field TEXT NOT NULL, \
                  embedding_model TEXT NOT NULL, embedding BLOB NOT NULL)"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("create private corpus fixture table");
    drop(writer);
    append_native_row(&rt, 1).await;
    append_native_row(&rt, 2).await;
    (dir, rt)
}

async fn append_native_row(rt: &KhiveRuntime, index: usize) {
    let id = Uuid::from_u128(index as u128).to_string();
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("native fixture writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO vec_consumer_fixture \
                  (subject_id, namespace, kind, field, embedding_model, embedding) \
                  VALUES (?1, 'local', 'concept', 'knowledge.atom', ?2, ?3)"
                .into(),
            params: vec![
                SqlValue::Text(id.clone()),
                SqlValue::Text("fixture-model".into()),
                SqlValue::Blob(khive_storage::encode_f32_native(&[
                    index as f32,
                    -(index as f32),
                ])),
            ],
            label: None,
        })
        .await
        .expect("append native fixture vector");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                  (namespace, embedding_model, kind, field, subject_id, op) \
                  VALUES ('local', ?1, 'concept', 'knowledge.atom', ?2, 'upsert')"
                .into(),
            params: vec![SqlValue::Text("fixture-model".into()), SqlValue::Text(id)],
            label: None,
        })
        .await
        .expect("append native fixture log record");
}

struct FirstRowGate {
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

// Release even if a test assertion fails while the worker is paused.
struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

impl ReleaseOnDrop {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

struct FirstRowConsumer {
    inner: Box<dyn SqlRowConsumer>,
    gate: Option<FirstRowGate>,
    visits: Arc<AtomicUsize>,
}

impl SqlRowConsumer for FirstRowConsumer {
    fn push_row(&mut self, row: SqlRow) -> StorageResult<()> {
        self.inner.push_row(row)?;
        self.visits.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = self.gate.take() {
            gate.entered.send(()).map_err(|()| {
                StorageError::Internal("first-row entry receiver was dropped".into())
            })?;
            gate.release
                .recv_timeout(CONSUMER_FAILSAFE)
                .map_err(|error| {
                    StorageError::Internal(format!("first-row release failed: {error}"))
                })?;
        }
        Ok(())
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send> {
        self.inner.into_any()
    }
}

struct FirstRowReader {
    inner: Box<dyn SqlReader>,
    gate: Option<FirstRowGate>,
    visits: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SqlReader for FirstRowReader {
    async fn query_row(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlRow>> {
        self.inner.query_row(statement).await
    }

    async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.inner.query_all(statement).await
    }

    async fn query_consume(
        &mut self,
        statement: SqlStatement,
        consumer: Box<dyn SqlRowConsumer>,
    ) -> StorageResult<Box<dyn SqlRowConsumer>> {
        self.inner
            .query_consume(
                statement,
                Box::new(FirstRowConsumer {
                    inner: consumer,
                    gate: self.gate.take(),
                    visits: Arc::clone(&self.visits),
                }),
            )
            .await
    }

    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        self.inner.query_scalar(statement).await
    }

    async fn explain(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        self.inner.explain(statement).await
    }
}

async fn gated_reader(
    rt: &KhiveRuntime,
) -> (
    FirstRowReader,
    tokio::sync::oneshot::Receiver<()>,
    ReleaseOnDrop,
    Arc<AtomicUsize>,
) {
    let sql = rt.sql();
    let reader = sql.reader().await.expect("native fixture reader");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let visits = Arc::new(AtomicUsize::new(0));
    (
        FirstRowReader {
            inner: reader,
            gate: Some(FirstRowGate {
                entered: entered_tx,
                release: release_rx,
            }),
            visits: Arc::clone(&visits),
        },
        entered_rx,
        ReleaseOnDrop(Some(release_tx)),
        visits,
    )
}

async fn native_baseline(rt: &KhiveRuntime) -> Corpus {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("positive baseline reader");
    let corpus = scan_corpus_rows(reader.as_mut(), statement(), 2)
        .await
        .expect("positive baseline scan");
    let (flat, ids, watermark) = corpus.as_ref().expect("nonempty positive baseline");
    assert_eq!(ids.as_slice(), &[Uuid::from_u128(1), Uuid::from_u128(2)]);
    assert_eq!(flat.as_slice(), &[1.0, -1.0, 2.0, -2.0]);
    assert!(
        *watermark > 0,
        "baseline log must have a positive high-water"
    );
    corpus
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corpus_scan_consumer_native_snapshot_excludes_concurrent_writer() {
    let (_dir, rt) = native_fixture().await;
    let baseline = native_baseline(&rt).await;
    let (mut reader, entered, mut release, visits) = gated_reader(&rt).await;
    let scan = tokio::spawn(async move {
        let result = scan_corpus_rows(&mut reader, statement(), 2).await;
        (reader, result)
    });
    tokio::time::timeout(NATIVE_WAIT, entered)
        .await
        .expect("native scan must enter first consumer visit")
        .expect("native scan must publish first-row entry");
    assert_eq!(visits.load(Ordering::SeqCst), 1);
    // The query is paused inside its first owned-row visit. Commit both writes
    // before it resumes, so a second watermark query would observe the change.
    let written = tokio::time::timeout(NATIVE_WAIT, append_native_row(&rt, 3)).await;
    release.release();
    written.expect("concurrent writer must commit while the snapshot is held");
    let (mut reader, result) = tokio::time::timeout(NATIVE_WAIT, scan)
        .await
        .expect("native snapshot scan must finish")
        .expect("native snapshot task");
    assert_corpus_equal(result.expect("native snapshot result"), baseline.clone());
    let after = scan_corpus_rows(&mut reader, statement(), 2)
        .await
        .expect("read after concurrent commit")
        .expect("nonempty corpus after commit");
    assert_eq!(
        after.1.as_slice(),
        &[Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
    );
    assert!(after.2 > baseline.as_ref().unwrap().2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corpus_scan_consumer_native_cancellation_releases_reader() {
    let (_dir, rt) = native_fixture().await;
    let baseline = native_baseline(&rt).await;
    let (mut reader, entered, mut release, visits) = gated_reader(&rt).await;
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let scan = tokio::spawn(khive_storage::scope_request_read_cancellation(
        cancel_rx,
        async move {
            let result = scan_corpus_rows(&mut reader, statement(), 2).await;
            (reader, result)
        },
    ));
    tokio::time::timeout(NATIVE_WAIT, entered)
        .await
        .expect("cancellable native scan must enter first consumer visit")
        .expect("cancellable native scan must publish first-row entry");
    assert_eq!(visits.load(Ordering::SeqCst), 1);
    cancel_tx.send(true).expect("cancel active native scan");
    // A synchronous consumer cannot be preempted. Release its handshake so the
    // worker can observe cancellation and give back its reader admission.
    release.release();
    let (mut reader, result) = tokio::time::timeout(NATIVE_WAIT, scan)
        .await
        .expect("cancelled native scan must finish")
        .expect("cancelled native scan task");
    assert!(matches!(result, Err(StorageError::Timeout { .. })));
    let recovered =
        tokio::time::timeout(NATIVE_WAIT, scan_corpus_rows(&mut reader, statement(), 2))
            .await
            .expect("reader admission must recover after cancellation")
            .expect("following native scan succeeds");
    assert_corpus_equal(recovered, baseline);
}
