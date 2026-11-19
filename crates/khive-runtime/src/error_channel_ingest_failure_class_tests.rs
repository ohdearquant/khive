use super::{ChannelIngestFailureClass, ResolutionFacts, RuntimeError};
use crate::secret_gate::SecretMatch;
use std::time::Duration;
use uuid::Uuid;

#[test]
fn secret_detected_is_permanent_by_typed_variant_not_display_text() {
    let first = RuntimeError::SecretDetected(SecretMatch {
        detector: "fixture",
        trigger: None,
        masked: "first-rendering".to_string(),
        location: None,
    });
    let second = RuntimeError::SecretDetected(SecretMatch {
        detector: "fixture",
        trigger: Some("token"),
        masked: "completely-different-rendering".to_string(),
        location: None,
    });

    assert_ne!(first.to_string(), second.to_string());
    assert_eq!(
        first.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Permanent {
            reason: "SecretDetected"
        }
    );
    assert_eq!(
        second.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Permanent {
            reason: "SecretDetected"
        },
        "rendered error details must not participate in ingest classification"
    );
}

#[test]
fn admission_failures_are_retryable_and_unclassified_errors_are_unknown() {
    let retryable =
        RuntimeError::Storage(khive_storage::StorageError::WriteQueueFull { timeout_ms: 25 });
    assert_eq!(
        retryable.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Retryable { reason: "Storage" }
    );

    let begin_busy =
        RuntimeError::Storage(khive_storage::StorageError::WriterTaskBusy { timeout_ms: 175 });
    let context = begin_busy
        .retryable_failure_context()
        .expect("contended BEGIN must remain typed and retryable");
    assert_eq!(context.stage, "writer_task_begin_busy");
    assert_eq!(context.timeout, Duration::from_millis(175));
    assert_eq!(context.operation.as_deref(), Some("writer_task_begin"));
    assert_eq!(context.scope, None);
    assert_eq!(context.retry_after_ms, None);
    assert_eq!(
        begin_busy.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Retryable { reason: "Storage" }
    );

    let unknown = RuntimeError::InvalidInput(
        "write blocked: SecretDetected text must not affect classification".to_string(),
    );
    assert_eq!(
            unknown.channel_ingest_failure_class(),
            ChannelIngestFailureClass::Unknown {
                reason: "InvalidInput"
            },
            "a rendered message resembling SecretDetected must remain Unknown unless its typed variant is SecretDetected"
        );
}

#[test]
fn resolution_wrapper_keeps_remote_fetch_retry_classification() {
    let wrapped = RuntimeError::RemoteFetchError {
        remote: "https://example.com/repo".into(),
        message: "cache repair failed".into(),
    }
    .with_resolution(ResolutionFacts {
        project_id: Uuid::from_u128(1),
        duplicate_anchor_ids: vec![],
        slug_backfilled: false,
        project_created: false,
        orphaned_project_id: None,
        orphaned_note_count: 0,
    });
    let RuntimeError::WithResolution { context } = &wrapped else {
        panic!("expected resolution context");
    };
    assert!(matches!(
        std::error::Error::source(context).and_then(|source| source.downcast_ref::<RuntimeError>()),
        Some(RuntimeError::RemoteFetchError { .. })
    ));
    assert!(matches!(
        wrapped.refusal_source(),
        RuntimeError::RemoteFetchError { .. }
    ));
    assert!(wrapped.retryable_failure_context().is_none());
    assert_eq!(
        wrapped.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Unknown {
            reason: "RemoteFetchError"
        }
    );
}

#[test]
fn writer_task_failure_context_separates_request_finality_from_task_liveness() {
    use khive_storage::{StorageError, WriterTaskRequestState};

    let rolled_back = RuntimeError::Storage(StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source: Box::new(StorageError::Pool {
            operation: "writer_task_commit".into(),
            message: "commit refused".into(),
        }),
    });
    let rolled_back_context = rolled_back
        .writer_task_failure_context()
        .expect("proven rollback must remain typed through RuntimeError");
    assert_eq!(
        rolled_back_context.request_state,
        WriterTaskRequestState::TransactionRolledBack
    );
    assert_eq!(
        rolled_back_context.stage,
        super::WRITER_TASK_REQUEST_FAILED_STAGE
    );
    assert!(!rolled_back_context.task_terminated);
    assert!(rolled_back_context.retryable);

    let unknown = RuntimeError::Storage(StorageError::writer_task_terminated(
        WriterTaskRequestState::SideEffectsUnknown,
    ));
    let unknown_context = unknown
        .writer_task_failure_context()
        .expect("ambiguous finality must remain typed through RuntimeError");
    assert_eq!(
        unknown_context.request_state,
        WriterTaskRequestState::SideEffectsUnknown
    );
    assert_eq!(unknown_context.stage, super::WRITER_TASK_TERMINATED_STAGE);
    assert!(unknown_context.task_terminated);
    assert!(!unknown_context.retryable);
}

#[test]
fn storage_admission_timeout_is_a_retryable_admission_failure() {
    let admission = RuntimeError::Storage(khive_storage::StorageError::AdmissionTimeout {
        operation: "sql_bridge.writer_handle".into(),
        timeout_ms: 30_000,
        pool_identity: None,
    });
    let context = admission
        .admission_failure_context()
        .expect("a storage admission timeout happens before the operation starts");
    assert_eq!(context.stage, super::STORAGE_ADMISSION_TIMEOUT_STAGE);
    assert_eq!(context.timeout, Duration::from_millis(30_000));
    assert_eq!(
        context.operation.as_deref(),
        Some("sql_bridge.writer_handle")
    );
    assert_eq!(context.scope, None);
    assert_eq!(context.retry_after_ms, None);
    assert_eq!(
        admission.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Retryable { reason: "Storage" }
    );

    // The general mid-flight timeout stays unclassified: it proves
    // nothing about whether work was in flight when the deadline expired.
    let mid_flight = RuntimeError::Storage(khive_storage::StorageError::Timeout {
        operation: "sql_bridge.reader_open".into(),
    });
    assert!(mid_flight.admission_failure_context().is_none());
    assert!(mid_flight.retryable_failure_context().is_none());
}

/// Binds both age-eviction outcomes a cached-reader rollback can produce
/// (clean rollback vs. failed/denied cleanup) to the same retryable wire
/// stage: a regression that reverted either branch back to generic
/// `StorageError::Transaction` would drop this match arm entirely and
/// fail here, since `Transaction` does have a `retryable_failure_context`
/// mapping distinct from `super::READ_TX_AGE_EVICTED_STAGE`.
#[test]
fn both_read_tx_age_eviction_outcomes_map_to_the_same_retryable_stage() {
    let clean = RuntimeError::Storage(khive_storage::StorageError::ReadTransactionAgeEvicted {
        operation: "query_all".into(),
        max_age_secs: 120,
    });
    let clean_context = clean
        .retryable_failure_context()
        .expect("a clean age eviction must be typed-retryable");
    assert_eq!(clean_context.stage, super::READ_TX_AGE_EVICTED_STAGE);
    assert_eq!(clean_context.timeout, Duration::from_secs(120));
    assert_eq!(clean_context.operation.as_deref(), Some("query_all"));
    assert_eq!(
        clean_context.capability,
        Some(khive_storage::StorageCapability::Sql)
    );

    let cleanup_failed = RuntimeError::Storage(
        khive_storage::StorageError::ReadTransactionAgeEvictionCleanupFailed {
            operation: "query_all".into(),
            max_age_secs: 120,
            message: "rollback failed: disk I/O error".into(),
        },
    );
    let cleanup_failed_context = cleanup_failed
        .retryable_failure_context()
        .expect("a failed cleanup rollback must remain typed-retryable");
    assert_eq!(
        cleanup_failed_context.stage,
        super::READ_TX_AGE_EVICTED_STAGE
    );
    assert_eq!(cleanup_failed_context.timeout, Duration::from_secs(120));
    assert_eq!(
        cleanup_failed_context.operation.as_deref(),
        Some("query_all")
    );
    assert_eq!(
        cleanup_failed_context.capability,
        Some(khive_storage::StorageCapability::Sql)
    );

    assert_ne!(
        clean.to_string(),
        cleanup_failed.to_string(),
        "the rendered message must distinguish a clean eviction from a failed cleanup even \
             though both map to the same wire stage"
    );
    assert!(cleanup_failed.to_string().contains("rollback failed"));
}
