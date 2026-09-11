use super::{StreamAppendDisposition, StreamAppendFailure, StreamAppendSpec};
use crate::{runtime_error_value, DomainDisposition, KhiveRuntime, Namespace, RuntimeError};
use khive_storage::{
    AtomicUnitOp, SqlAccess, SqlReader, SqlRow, SqlStatement, SqlValue, SqlWriter,
    StorageCapability, StorageError, StorageResult, WriterTaskRequestState,
};
use khive_types::{Details, KhiveError};
use serde_json::json;
use std::{
    any::Any,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[test]
fn append_proof_preserves_original_error_value_and_bytes() {
    let source =
        KhiveError::unavailable("same error, different append phase").with_details(Details::new([
            ("reason", "custom_backend_failure"),
            ("phase", "readiness"),
            ("extra", "retained\nverbatim"),
        ]));
    let expected = runtime_error_value(source.clone().into(), DomainDisposition::Unknown);
    let bytes = serde_json::to_vec(&expected).unwrap();
    for failure in [
        StreamAppendFailure::not_committed(source.clone().into()),
        StreamAppendFailure::unknown(source.into()),
    ] {
        // Append-local proof never changes the enclosing error projection.
        let (source, _) = failure.into_parts();
        let value = runtime_error_value(source, DomainDisposition::Unknown);
        assert_eq!(value, expected);
        assert_eq!(serde_json::to_vec(&value).unwrap(), bytes);
    }
}

#[test]
fn append_proof_uses_typed_finality_and_never_retryability_or_error_text() {
    use StreamAppendDisposition::{NotCommitted, Unknown};
    for (source, expected) in [
        (
            StorageError::WriteQueueFull { timeout_ms: 9 }.into(),
            NotCommitted,
        ),
        (
            StorageError::WriterTaskBusy { timeout_ms: 9 }.into(),
            NotCommitted,
        ),
        (
            StorageError::AdmissionTimeout {
                operation: "append".into(),
                timeout_ms: 9,
            }
            .into(),
            NotCommitted,
        ),
        (
            RuntimeError::Sqlite(khive_db::SqliteError::WriterPoolCheckoutTimeout {
                timeout: std::time::Duration::from_millis(9),
            }),
            NotCommitted,
        ),
        (
            StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::NotStarted,
            }
            .into(),
            NotCommitted,
        ),
        (
            StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::TransactionRolledBack,
            }
            .into(),
            NotCommitted,
        ),
        (
            StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
            }
            .into(),
            Unknown,
        ),
        (
            StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source: Box::new(StorageError::Internal("same driver error".into())),
            }
            .into(),
            NotCommitted,
        ),
        (
            StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
                source: Box::new(StorageError::WriteQueueFull { timeout_ms: 9 }),
            }
            .into(),
            Unknown,
        ),
        (
            StorageError::ReadTransactionAgeEvicted {
                operation: "append".into(),
                max_age_secs: 9,
            }
            .into(),
            Unknown,
        ),
        (
            RuntimeError::InvalidInput("write queue full; transaction_rolled_back".into()),
            Unknown,
        ),
        (
            RuntimeError::DeadlineExceeded {
                operation: "append".into(),
                budget_ms: 9,
                elapsed_ms: 10,
            },
            Unknown,
        ),
        (KhiveError::conflict("arbitrary conflict").into(), Unknown),
    ] {
        assert_eq!(
            StreamAppendFailure::after_submission(source, false).disposition(),
            expected
        );
    }
    let preparation =
        StreamAppendFailure::not_committed(RuntimeError::InvalidInput("same input".into()));
    let submitted = StreamAppendFailure::after_submission(
        RuntimeError::InvalidInput("same input".into()),
        false,
    );
    assert_eq!(preparation.disposition(), NotCommitted);
    assert_eq!(submitted.disposition(), Unknown);
    let unknown = StreamAppendFailure::after_submission(
        StorageError::WriterTaskTerminated {
            request_state: WriterTaskRequestState::SideEffectsUnknown,
        }
        .into(),
        true,
    );
    assert_eq!(
        unknown.disposition(),
        Unknown,
        "unknown finality is not weakened by a phase hint"
    );
}

#[derive(Clone, Copy)]
enum Fault {
    Busy,
    BeforeClosure,
    BeforeWrite,
    LostReply,
    WrongOutcome,
}

struct FaultAccess {
    inner: Arc<dyn SqlAccess>,
    fault: Fault,
    calls: AtomicUsize,
}

fn same_conflict() -> StorageError {
    StorageError::Conflict {
        capability: StorageCapability::Sql,
        operation: "append fixture".into(),
        message: "same failure independent of execution phase".into(),
    }
}

#[async_trait::async_trait]
impl SqlAccess for FaultAccess {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        self.inner.reader().await
    }

    async fn writer(&self) -> StorageResult<Box<dyn SqlWriter>> {
        self.inner.writer().await
    }

    async fn atomic_unit(&self, op: AtomicUnitOp) -> StorageResult<Box<dyn Any + Send>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.fault {
            Fault::Busy => Err(StorageError::WriterTaskBusy { timeout_ms: 9 }),
            Fault::BeforeClosure => Err(same_conflict()),
            Fault::BeforeWrite => op(&mut RefusingWriter).await,
            Fault::LostReply => {
                self.inner.atomic_unit(op).await?;
                Err(same_conflict())
            }
            Fault::WrongOutcome => {
                self.inner.atomic_unit(op).await?;
                Ok(Box::new(()))
            }
        }
    }
}

// The closure receives a writer but its first read refuses. Any append write
// would panic, so the pre-write proof is independently observable here.
struct RefusingWriter;

#[async_trait::async_trait]
impl SqlReader for RefusingWriter {
    async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
        unreachable!()
    }
    async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        unreachable!()
    }
    async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
        Err(same_conflict())
    }
    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        unreachable!()
    }
}

#[async_trait::async_trait]
impl SqlWriter for RefusingWriter {
    async fn execute(&mut self, _: SqlStatement) -> StorageResult<u64> {
        panic!("pre-write refusal must not execute a write")
    }
    async fn execute_batch(&mut self, _: Vec<SqlStatement>) -> StorageResult<u64> {
        unreachable!()
    }
    async fn execute_script(&mut self, _: String) -> StorageResult<()> {
        unreachable!()
    }
}

#[tokio::test]
async fn submission_evidence_separates_refusal_lost_reply_and_downcast_without_retry() {
    use StreamAppendDisposition::{NotCommitted, Unknown};
    for (fault, expected, stored) in [
        (Fault::Busy, NotCommitted, 0),
        (Fault::BeforeClosure, Unknown, 0),
        (Fault::BeforeWrite, NotCommitted, 0),
        (Fault::LostReply, Unknown, 1),
        (Fault::WrongOutcome, Unknown, 1),
    ] {
        let rt = KhiveRuntime::memory().unwrap();
        let token = rt.authorize(Namespace::local()).unwrap();
        let spec = StreamAppendSpec {
            stream: "evidence".into(),
            record: json!({"kind": "event"}),
            expected_seq: None,
            embed: None,
            embedding_model: None,
            note_kind: "observation".into(),
            tags: None,
            fence: None,
        };
        let prepared = rt.prepare_stream_appends(&token, &[&spec]).await.unwrap();
        let access = FaultAccess {
            inner: rt.sql(),
            fault,
            calls: AtomicUsize::new(0),
        };
        let failure = KhiveRuntime::run_stream_appends(&access, &token, &prepared)
            .await
            .err()
            .expect("injected failure must be returned");
        assert_eq!(
            access.calls.load(Ordering::SeqCst),
            1,
            "append must not retry"
        );
        assert_eq!(failure.disposition(), expected);
        let value = runtime_error_value(failure.into_source(), DomainDisposition::Unknown);
        assert_eq!(
            value["domain_disposition"], "unknown",
            "local refusal proof must not rewrite the original error"
        );
        let entries = rt.stream_read(&token, "evidence", 0, 10).await.unwrap();
        assert_eq!(entries["entries"].as_array().unwrap().len(), stored);
    }
}

#[tokio::test]
async fn append_preparation_and_sequence_refusal_preserve_compatibility_errors() {
    let rt = KhiveRuntime::memory().unwrap();
    let token = rt.authorize(Namespace::local()).unwrap();
    for (stream, expected_seq) in [("invalid\0name", None), ("sequence", Some(2))] {
        let source = rt
            .stream_append(
                &token,
                stream,
                &json!({}),
                expected_seq,
                "observation",
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        let failure = rt
            .stream_append_with_outcome(
                &token,
                stream,
                &json!({}),
                expected_seq,
                "observation",
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.disposition(), StreamAppendDisposition::NotCommitted);
        let expected = runtime_error_value(source, DomainDisposition::Unknown);
        let actual = runtime_error_value(failure.into_source(), DomainDisposition::Unknown);
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
    let result = rt
        .stream_append_with_outcome(
            &token,
            "sequence",
            &json!({"control": true}),
            Some(1),
            "observation",
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(result["seq"], 1);
    let page = rt.stream_read(&token, "sequence", 0, 10).await.unwrap();
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
}
