//! Decode the memory ANN corpus without retaining all raw SQL rows.

use std::{any::Any, collections::HashSet};

use khive_storage::{
    types::{SqlRow, SqlStatement, SqlValue},
    SqlReader, SqlRowConsumer, StorageError, StorageResult,
};

use super::Uuid;

pub(super) struct CorpusInput {
    pub(super) dimensions: usize,
    pub(super) ids: Vec<Uuid>,
    pub(super) flat: Vec<f32>,
    pub(super) namespaces: HashSet<String>,
    pub(super) watermark: u64,
}

struct CorpusConsumer {
    input: CorpusInput,
    saw_raw_row: bool,
}

impl CorpusConsumer {
    fn new(dimensions: usize) -> Self {
        Self {
            input: CorpusInput {
                dimensions,
                ids: Vec::new(),
                flat: Vec::new(),
                namespaces: HashSet::new(),
                watermark: 0,
            },
            saw_raw_row: false,
        }
    }
}

impl SqlRowConsumer for CorpusConsumer {
    fn push_row(&mut self, row: SqlRow) -> StorageResult<()> {
        if !self.saw_raw_row {
            self.saw_raw_row = true;
            self.input.watermark = match row.get("log_s") {
                Some(SqlValue::Integer(n)) => u64::try_from(*n).unwrap_or(0),
                _ => 0,
            };
        }

        let id_str = match row.get("subject_id") {
            Some(SqlValue::Text(s)) => s.as_str(),
            _ => return Ok(()),
        };
        let uuid = match Uuid::parse_str(id_str) {
            Ok(id) => id,
            Err(_) => return Ok(()),
        };
        let bytes = match row.get("embedding") {
            Some(SqlValue::Blob(b)) => b.as_slice(),
            _ => return Ok(()),
        };
        if bytes.len() != self.input.dimensions * 4 {
            return Ok(());
        }
        let Ok(vector) = khive_storage::decode_f32_native(bytes) else {
            return Ok(());
        };
        if let Some(SqlValue::Text(namespace)) = row.get("namespace") {
            self.input.namespaces.insert(namespace.clone());
        }
        self.input.ids.push(uuid);
        self.input.flat.extend_from_slice(&vector);
        Ok(())
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
        self
    }
}

pub(super) async fn scan(
    reader: &mut dyn SqlReader,
    statement: SqlStatement,
    dimensions: usize,
) -> StorageResult<CorpusInput> {
    let consumer = reader
        .query_consume(statement, Box::new(CorpusConsumer::new(dimensions)))
        .await?;
    let consumer = consumer
        .into_any()
        .downcast::<CorpusConsumer>()
        .map_err(|_| StorageError::Internal("memory ANN corpus consumer type mismatch".into()))?;
    Ok((*consumer).input)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use khive_storage::types::SqlColumn;

    use super::*;

    const A: &str = "00000000-0000-0000-0000-00000000000a";
    const B: &str = "00000000-0000-0000-0000-00000000000b";
    const C: &str = "00000000-0000-0000-0000-00000000000c";
    const D: &str = "00000000-0000-0000-0000-00000000000d";
    const FIXTURE_SQL: &str =
        "SELECT subject_id, embedding, namespace, log_s FROM fixture ORDER BY ordinal";

    #[derive(Default)]
    struct Calls {
        statements: Vec<SqlStatement>,
        query_all: usize,
        query_consume: usize,
        delivered: usize,
        raw_row_high_water: usize,
    }

    #[derive(Clone, Copy)]
    enum FirstWatermark {
        Missing,
        Text,
        Negative,
        Float,
        Zero,
    }

    #[derive(Clone, Copy)]
    enum Fixture {
        Mixed,
        Empty,
        Invalid,
        Watermark(FirstWatermark),
    }

    struct LazyReader {
        fixture: Fixture,
        calls: Arc<Mutex<Calls>>,
        fail_after: Option<usize>,
        wrong_consumer: bool,
    }

    impl LazyReader {
        fn new(fixture: Fixture) -> (Self, Arc<Mutex<Calls>>) {
            let calls = Arc::new(Mutex::new(Calls::default()));
            (
                Self {
                    fixture,
                    calls: calls.clone(),
                    fail_after: None,
                    wrong_consumer: false,
                },
                calls,
            )
        }

        fn len(&self) -> usize {
            match self.fixture {
                Fixture::Mixed => 32,
                Fixture::Empty => 0,
                Fixture::Invalid => 4,
                Fixture::Watermark(_) => 2,
            }
        }

        fn row(&self, index: usize) -> SqlRow {
            match self.fixture {
                Fixture::Mixed => mixed_row(index),
                Fixture::Empty => panic!("an empty fixture has no rows"),
                Fixture::Invalid => raw_row(
                    Some(text("invalid-uuid")),
                    Some(embedding([1.0, 2.0])),
                    Some(text("rejected")),
                    Some(SqlValue::Integer(if index == 0 { 70 } else { 71 })),
                ),
                Fixture::Watermark(first) if index == 0 => raw_row(
                    Some(text("invalid-uuid")),
                    Some(embedding([1.0, 2.0])),
                    Some(text("rejected")),
                    match first {
                        FirstWatermark::Missing => None,
                        FirstWatermark::Text => Some(text("41")),
                        FirstWatermark::Negative => Some(SqlValue::Integer(-1)),
                        FirstWatermark::Float => Some(SqlValue::Float(41.0)),
                        FirstWatermark::Zero => Some(SqlValue::Integer(0)),
                    },
                ),
                Fixture::Watermark(_) => raw_row(
                    Some(text(A)),
                    Some(embedding([1.25, -2.5])),
                    Some(text("alpha")),
                    Some(SqlValue::Integer(99)),
                ),
            }
        }

        fn note_row(&self, live_batch_len: usize) {
            let mut calls = self.calls.lock().expect("calls lock");
            calls.delivered += 1;
            calls.raw_row_high_water = calls.raw_row_high_water.max(live_batch_len);
        }
    }

    #[async_trait::async_trait]
    impl SqlReader for LazyReader {
        async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
            panic!("unexpected query_row")
        }

        async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            {
                let mut calls = self.calls.lock().expect("calls lock");
                calls.query_all += 1;
                calls.statements.push(statement);
            }
            let mut rows = Vec::new();
            for index in 0..self.len() {
                if self.fail_after == Some(index) {
                    return Err(late_error());
                }
                rows.push(self.row(index));
                self.note_row(rows.len());
            }
            Ok(rows)
        }

        async fn query_consume(
            &mut self,
            statement: SqlStatement,
            mut consumer: Box<dyn SqlRowConsumer>,
        ) -> StorageResult<Box<dyn SqlRowConsumer>> {
            {
                let mut calls = self.calls.lock().expect("calls lock");
                calls.query_consume += 1;
                calls.statements.push(statement);
            }
            for index in 0..self.len() {
                if self.fail_after == Some(index) {
                    return Err(late_error());
                }
                let row = self.row(index);
                self.note_row(1);
                consumer.push_row(row)?;
            }
            if self.wrong_consumer {
                return Ok(Box::new(WrongConsumer));
            }
            Ok(consumer)
        }

        async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
            panic!("unexpected query_scalar")
        }

        async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            panic!("unexpected explain")
        }
    }

    struct DefaultConsumeReader(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl SqlReader for DefaultConsumeReader {
        async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
            panic!("unexpected query_row")
        }

        async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
            panic!("unexpected query_scalar")
        }

        async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            panic!("unexpected explain")
        }
    }

    struct WrongConsumer;

    impl SqlRowConsumer for WrongConsumer {
        fn push_row(&mut self, _: SqlRow) -> StorageResult<()> {
            Ok(())
        }

        fn into_any(self: Box<Self>) -> Box<dyn Any + Send> {
            self
        }
    }

    fn text(value: &str) -> SqlValue {
        SqlValue::Text(value.to_owned())
    }

    fn embedding(values: [f32; 2]) -> SqlValue {
        SqlValue::Blob(values.into_iter().flat_map(f32::to_ne_bytes).collect())
    }

    fn raw_row(
        id: Option<SqlValue>,
        embedding: Option<SqlValue>,
        namespace: Option<SqlValue>,
        watermark: Option<SqlValue>,
    ) -> SqlRow {
        let mut columns = Vec::new();
        for (name, value) in [
            ("subject_id", id),
            ("embedding", embedding),
            ("namespace", namespace),
            ("log_s", watermark),
        ] {
            if let Some(value) = value {
                columns.push(SqlColumn {
                    name: name.to_owned(),
                    value,
                });
            }
        }
        SqlRow { columns }
    }

    fn late_error() -> StorageError {
        StorageError::Timeout {
            operation: "fixture_late_scan".into(),
        }
    }

    fn mixed_row(index: usize) -> SqlRow {
        let watermark = Some(SqlValue::Integer(if index == 0 { 91 } else { 99 }));
        let (id, vector, namespace) = match index {
            0 => (
                Some(text("invalid-first-uuid")),
                Some(embedding([0.0, 0.0])),
                Some(text("rejected-first")),
            ),
            1 => (Some(text(A)), Some(embedding([1.25, -2.5])), Some(text("alpha"))),
            2 => (None, Some(embedding([0.0, 0.0])), Some(text("rejected-id-missing"))),
            3 => (
                Some(SqlValue::Integer(10)),
                Some(embedding([0.0, 0.0])),
                Some(text("rejected-id-type")),
            ),
            4 => (
                Some(text("also-invalid")),
                Some(embedding([0.0, 0.0])),
                Some(text("rejected-id-invalid")),
            ),
            5 => (Some(text(B)), Some(embedding([3.0, 4.0])), Some(text("beta"))),
            6 => (Some(text(C)), None, Some(text("rejected-embedding-missing"))),
            7 => (Some(text(C)), Some(text("not-a-blob")), Some(text("rejected-embedding-type"))),
            8 => (
                Some(text(C)),
                Some(SqlValue::Blob(vec![0; 4])),
                Some(text("rejected-embedding-length")),
            ),
            9 => (Some(text(A)), Some(embedding([-5.0, 6.5])), Some(text("alpha"))),
            10 => (Some(text(C)), Some(embedding([7.0, 8.0])), None),
            11 => (
                Some(text(D)),
                Some(embedding([9.0, 10.0])),
                Some(SqlValue::Integer(7)),
            ),
            _ => (
                Some(text("invalid-tail")),
                Some(embedding([0.0, 0.0])),
                Some(text("rejected-tail")),
            ),
        };
        raw_row(id, vector, namespace, watermark)
    }

    fn statement() -> SqlStatement {
        SqlStatement {
            sql: FIXTURE_SQL.to_owned(),
            params: vec![text("fixture-model")],
            label: Some("memory_ann_corpus_fixture".into()),
        }
    }

    fn assert_one_statement(calls: &Calls) {
        assert_eq!(calls.statements.len(), 1);
        let statement = &calls.statements[0];
        assert_eq!(statement.sql, FIXTURE_SQL);
        match statement.params.as_slice() {
            [SqlValue::Text(model)] => assert_eq!(model, "fixture-model"),
            _ => panic!("statement parameters changed"),
        }
        assert_eq!(statement.label.as_deref(), Some("memory_ann_corpus_fixture"));
    }

    fn assert_mixed_input(input: &CorpusInput) {
        assert_eq!(input.dimensions, 2);
        assert_eq!(
            input.ids,
            vec![
                Uuid::from_u128(10),
                Uuid::from_u128(11),
                Uuid::from_u128(10),
                Uuid::from_u128(12),
                Uuid::from_u128(13),
            ]
        );
        assert_eq!(input.flat, vec![1.25, -2.5, 3.0, 4.0, -5.0, 6.5, 7.0, 8.0, 9.0, 10.0]);
        assert_eq!(input.namespaces, HashSet::from(["alpha".to_owned(), "beta".to_owned()]));
        assert_eq!(input.watermark, 91);
    }

    #[tokio::test]
    async fn memory_ann_corpus_stream_preserves_input_before_raw_row_high_water() {
        let (mut reader, calls) = LazyReader::new(Fixture::Mixed);
        let input = scan(&mut reader, statement(), 2).await.expect("complete scan");
        assert_mixed_input(&input);
        let calls = calls.lock().expect("calls lock");
        assert_one_statement(&calls);
        assert_eq!(calls.delivered, 32);
        eprintln!("positive decoded-input parity passed");
        assert_eq!(calls.raw_row_high_water, 1, "RAW_ROW_BATCH_HIGH_WATER");
        assert_eq!(calls.query_consume, 1);
        assert_eq!(calls.query_all, 0);
    }

    #[tokio::test]
    async fn memory_ann_corpus_first_raw_watermark_fallback_is_not_replaced() {
        for first in [
            FirstWatermark::Missing,
            FirstWatermark::Text,
            FirstWatermark::Negative,
            FirstWatermark::Float,
            FirstWatermark::Zero,
        ] {
            let (mut reader, calls) = LazyReader::new(Fixture::Watermark(first));
            let input = scan(&mut reader, statement(), 2).await.expect("complete scan");
            assert_eq!(input.dimensions, 2);
            assert_eq!(input.ids, vec![Uuid::from_u128(10)]);
            assert_eq!(input.flat, vec![1.25, -2.5]);
            assert_eq!(input.namespaces, HashSet::from(["alpha".to_owned()]));
            assert_eq!(input.watermark, 0);
            assert_one_statement(&calls.lock().expect("calls lock"));
        }
    }

    #[tokio::test]
    async fn memory_ann_corpus_empty_and_all_invalid_keep_empty_input() {
        for (fixture, watermark) in [(Fixture::Empty, 0), (Fixture::Invalid, 70)] {
            let (mut reader, calls) = LazyReader::new(fixture);
            let input = scan(&mut reader, statement(), 2).await.expect("complete scan");
            assert_eq!(input.dimensions, 2);
            assert!(input.ids.is_empty());
            assert!(input.flat.is_empty());
            assert!(input.namespaces.is_empty());
            assert_eq!(input.watermark, watermark);
            assert_one_statement(&calls.lock().expect("calls lock"));
        }
    }

    #[tokio::test]
    async fn memory_ann_corpus_unsupported_reader_has_no_query_all_fallback() {
        let query_all_calls = Arc::new(AtomicUsize::new(0));
        let mut reader = DefaultConsumeReader(query_all_calls.clone());
        let error = scan(&mut reader, statement(), 2)
            .await
            .err()
            .expect("unsupported scan");
        match error {
            StorageError::Unsupported { operation, .. } => assert_eq!(operation, "query_consume"),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(query_all_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn memory_ann_corpus_late_scan_error_returns_no_partial_input() {
        let (mut reader, calls) = LazyReader::new(Fixture::Mixed);
        reader.fail_after = Some(3);
        let error = scan(&mut reader, statement(), 2)
            .await
            .err()
            .expect("late scan failure");
        match error {
            StorageError::Timeout { operation } => {
                assert_eq!(operation, "fixture_late_scan");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let calls = calls.lock().expect("calls lock");
        assert_one_statement(&calls);
        assert_eq!(calls.delivered, 3);
        assert_eq!(calls.query_consume, 1);
        assert_eq!(calls.query_all, 0);
    }

    #[tokio::test]
    async fn memory_ann_corpus_wrong_consumer_is_a_typed_storage_error() {
        let (mut reader, _) = LazyReader::new(Fixture::Mixed);
        reader.wrong_consumer = true;
        let error = scan(&mut reader, statement(), 2)
            .await
            .err()
            .expect("consumer type mismatch");
        match error {
            StorageError::Internal(message) => {
                assert_eq!(message, "memory ANN corpus consumer type mismatch");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
