use super::*;
use khive_storage::{SqlReader, SqlRow, StorageError, StorageResult};

/// A store whose reader is handed out by `reader`, and which nothing in these
/// tests writes to.
struct ReaderOnly<F: Fn() -> StorageResult<Box<dyn SqlReader>> + Send + Sync>(F);

#[async_trait::async_trait]
impl<F: Fn() -> StorageResult<Box<dyn SqlReader>> + Send + Sync> SqlAccess for ReaderOnly<F> {
    async fn reader(&self) -> StorageResult<Box<dyn SqlReader>> {
        (self.0)()
    }

    async fn writer(&self) -> StorageResult<Box<dyn khive_storage::SqlWriter>> {
        panic!("a snapshot version read must not acquire a writer")
    }

    async fn atomic_unit(
        &self,
        _op: khive_storage::AtomicUnitOp,
    ) -> StorageResult<Box<dyn std::any::Any + Send>> {
        panic!("a snapshot version read must not open a unit")
    }
}

/// A reader whose snapshot-version query returns `version`.
struct VersionReader(Option<SqlValue>);

#[async_trait::async_trait]
impl SqlReader for VersionReader {
    async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
        panic!("only the scalar version query is expected")
    }

    async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        panic!("only the scalar version query is expected")
    }

    async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
        Ok(self.0.clone())
    }

    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        panic!("only the scalar version query is expected")
    }
}

#[tokio::test]
async fn admission_refusal_on_the_state_load_keeps_its_type_and_fields() {
    let store = ReaderOnly(|| {
        Err(StorageError::AdmissionTimeout {
            operation: "fixture reader".into(),
            timeout_ms: 17,
            pool_identity: Some("fixture-pool".into()),
        })
    });
    match load_snapshot_version(&store, "local").await {
        Err(RuntimeError::Storage(StorageError::AdmissionTimeout {
            operation,
            timeout_ms,
            pool_identity,
        })) => {
            assert_eq!(operation, "fixture reader");
            assert_eq!(timeout_ms, 17);
            assert_eq!(pool_identity.as_deref(), Some("fixture-pool"));
        }
        other => panic!("expected the typed admission refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_value_persistence_cannot_decode_still_names_brain_persistence() {
    let store = ReaderOnly(|| {
        Ok(
            Box::new(VersionReader(Some(SqlValue::Text("not a version".into()))))
                as Box<dyn SqlReader>,
        )
    });
    match load_snapshot_version(&store, "local").await {
        Err(RuntimeError::Internal(message)) => {
            assert!(
                message.starts_with("brain persistence load snapshot version: expected integer"),
                "{message}"
            );
        }
        other => panic!("expected an internal decode error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_expired_request_reaches_a_brain_verb_caller_as_a_typed_timeout() {
    // Through the registry the pack loads its state before the handler runs, so the
    // load meets the expired deadline first.
    let runtime = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::BrainPack::new(runtime));
    let registry = builder.build().unwrap();
    let expired = khive_storage::RequestReadDeadline::after(std::time::Duration::ZERO);

    let result = khive_storage::scope_request_read_deadline_at(
        expired,
        registry.dispatch(
            "brain.event_counts",
            serde_json::json!({"since": "2026-10-01T00:00:00Z"}),
        ),
    )
    .await;
    assert!(
        matches!(
            result,
            Err(RuntimeError::Storage(StorageError::Timeout { .. }))
        ),
        "dispatched result: {result:?}"
    );
}

#[test]
fn a_storage_error_inside_the_feedback_unit_keeps_its_type() {
    let timeout = feedback::unit_err(
        "fixture step",
        RuntimeError::Storage(StorageError::Timeout {
            operation: "fixture read".into(),
        }),
    );
    assert!(
        matches!(&timeout, StorageError::Timeout { operation } if operation == "fixture read"),
        "{timeout:?}"
    );
    let other = feedback::unit_err("fixture step", RuntimeError::Internal("boom".into()));
    assert!(
        matches!(other, StorageError::Driver { .. }),
        "a non-storage failure is still reported as a driver error: {other:?}"
    );
}
