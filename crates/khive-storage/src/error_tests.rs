use super::*;
use std::fmt;

#[derive(Debug)]
struct FakeSource(String);

impl fmt::Display for FakeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl StdError for FakeSource {}

fn driver_err(operation: &'static str, message: &str) -> StorageError {
    StorageError::driver(
        StorageCapability::Text,
        operation,
        FakeSource(message.into()),
    )
}

#[test]
fn writer_task_request_state_display_is_stable() {
    assert_eq!(
        WriterTaskRequestState::NotStarted.to_string(),
        "not_started"
    );
    assert_eq!(
        WriterTaskRequestState::TransactionRolledBack.to_string(),
        "transaction_rolled_back"
    );
    assert_eq!(
        WriterTaskRequestState::SideEffectsUnknown.to_string(),
        "side_effects_unknown"
    );
}

#[test]
fn issued_commit_unknown_keeps_reply_source_and_never_implies_retry() {
    let error = StorageError::WriteOutcomeUnknown {
        source: Box::new(FakeSource("reply was lost".into())),
    };
    assert!(StdError::source(&error).unwrap().is::<FakeSource>());
    assert_eq!(
        error.writer_request_state(),
        Some(WriterTaskRequestState::SideEffectsUnknown)
    );
    assert!(error.is_write_outcome_unknown());
    assert!(!error.is_retryable());
    assert_eq!(error.capability(), None);

    let error = error.with_sqlite_write_failure(SqliteWriteFailure {
        stage: SqliteWriteStage::Commit,
        primary_code: 10,
        extended_code: 10,
        settlement_unknown: true,
    });
    assert!(error.is_write_outcome_unknown());
    assert_eq!(
        error.writer_request_state(),
        Some(WriterTaskRequestState::SideEffectsUnknown)
    );
    assert!(!error.is_retryable());
    let rolled_back = StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source: Box::new(error),
    };
    assert_eq!(
        rolled_back.writer_request_state(),
        Some(WriterTaskRequestState::TransactionRolledBack)
    );
    assert!(
        !rolled_back.is_write_outcome_unknown(),
        "outer rollback proof owns finality"
    );
}

#[test]
fn strict_routing_refusals_preserve_owner_source_and_retry_policy() {
    for (source, retryable) in [
        (
            StorageError::Pool {
                operation: "open".into(),
                message: "unavailable".into(),
            },
            true,
        ),
        (StorageError::WriterTaskNoRuntime, false),
    ] {
        let error = StorageError::WriterOwnerUnavailable {
            operation: "create".into(),
            source: Box::new(source),
        };
        let cause_box = StdError::source(&error)
            .expect("owner refusal must retain its public source")
            .downcast_ref::<Box<StorageError>>()
            .expect("owner refusal source must retain its typed storage box");
        let cause = cause_box.as_ref();
        let StorageError::WriterOwnerUnavailable { operation, source } = &error else {
            unreachable!("fixture constructs an owner-unavailable refusal")
        };
        assert_eq!(operation.as_ref(), "create");
        assert!(
            std::ptr::eq(cause_box, source),
            "public source must be the exact stored owner-cause box"
        );
        match cause {
            StorageError::Pool { operation, message } => {
                assert_eq!(operation.as_ref(), "open");
                assert_eq!(message, "unavailable");
            }
            StorageError::WriterTaskNoRuntime => assert!(
                matches!(source.as_ref(), StorageError::WriterTaskNoRuntime),
                "public NoRuntime cause must retain the stored NoRuntime identity"
            ),
            other => panic!("unexpected owner cause: {other:?}"),
        }
        assert_eq!(cause.is_retryable(), retryable);
        assert_eq!(error.is_retryable(), retryable);
        assert_eq!(
            error.writer_request_state(),
            Some(WriterTaskRequestState::NotStarted)
        );
        assert!(!error.is_write_outcome_unknown());
        assert_eq!(
            matches!(cause, StorageError::WriterTaskNoRuntime),
            !retryable
        );
    }
    let error = StorageError::WriterRouteViolation {
        operation: "transaction".into(),
    };
    assert_eq!(
        error.writer_request_state(),
        Some(WriterTaskRequestState::NotStarted)
    );
    assert!(!error.is_retryable());
    assert!(StdError::source(&error).is_none());
}

#[test]
fn writer_task_busy_is_retryable_without_claiming_queue_rejection() {
    let error = StorageError::WriterTaskBusy { timeout_ms: 175 };
    assert!(error.is_retryable());
    assert_eq!(
            error.to_string(),
            "writer task could not begin within 175ms because SQLite remained busy; request was not executed"
        );
    assert_eq!(error.capability(), None);
}

#[test]
fn capacity_retryability_distinguishes_contention_from_refusal() {
    for (phase, label, retryable) in [
        (None, "floor", false),
        (Some(CapacityUnavailablePhase::Identity), "identity", false),
        (Some(CapacityUnavailablePhase::Lock), "lock", false),
        (
            Some(CapacityUnavailablePhase::LockContended),
            "lock_contended",
            true,
        ),
        (Some(CapacityUnavailablePhase::Probe), "probe", false),
    ] {
        let error = match phase {
            Some(phase) => {
                assert_eq!(phase.as_str(), label);
                assert_eq!(phase.to_string(), label);
                StorageError::CapacityUnavailable {
                    capability: StorageCapability::Sql,
                    phase,
                    message: "capacity admission unavailable".into(),
                }
            }
            None => StorageError::CapacityFloor {
                capability: StorageCapability::Sql,
                volume: "volume".into(),
                available_bytes: 99,
                floor_bytes: 100,
                required_headroom_bytes: 0,
            },
        };
        assert_eq!(error.is_retryable(), retryable, "{label}");
        let wrapped = StorageError::WriterTaskRequestFailed {
            request_state: WriterTaskRequestState::TransactionRolledBack,
            source: Box::new(error),
        };
        assert_eq!(wrapped.is_retryable(), retryable, "wrapped {label}");
    }
}

#[test]
fn writer_task_request_failure_preserves_source_policy_and_rollback_state() {
    let error = StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source: Box::new(StorageError::Pool {
            operation: "writer_task_commit".into(),
            message: "commit refused".into(),
        }),
    };

    assert_eq!(error.capability(), None);
    assert!(
        error.is_retryable(),
        "rollback finality must not discard the source error's retry policy"
    );
    assert_eq!(
            error.to_string(),
            "writer task request failed (request_state=transaction_rolled_back): pool failure during writer_task_commit: commit refused"
        );
    assert_eq!(
        StdError::source(&error).map(ToString::to_string),
        Some("pool failure during writer_task_commit: commit refused".to_string()),
        "the original typed storage error must remain the public source"
    );
}

#[test]
fn writer_task_request_failure_does_not_invent_retryability() {
    let error = StorageError::WriterTaskRequestFailed {
        request_state: WriterTaskRequestState::TransactionRolledBack,
        source: Box::new(StorageError::InvalidInput {
            capability: StorageCapability::Notes,
            operation: "append_note".into(),
            message: "deterministic refusal".into(),
        }),
    };

    assert!(!error.is_retryable());
    assert_eq!(error.capability(), Some(StorageCapability::Notes));
}

#[test]
fn writer_task_terminated_is_uncapability_scoped_and_not_retryable() {
    for request_state in [
        WriterTaskRequestState::NotStarted,
        WriterTaskRequestState::TransactionRolledBack,
        WriterTaskRequestState::SideEffectsUnknown,
    ] {
        let error = StorageError::writer_task_terminated(request_state);
        assert_eq!(error.capability(), None);
        assert!(!error.is_retryable());
        assert_eq!(
            error.to_string(),
            format!("writer task terminated (request_state={request_state})")
        );
    }
}

#[test]
fn blob_integrity_errors_are_blob_scoped_and_not_retryable() {
    let requested = crate::blob::ContentRef::from_hex("a".repeat(64)).unwrap();
    let actual = crate::blob::ContentRef::from_hex("b".repeat(64)).unwrap();
    let errors = [
        StorageError::BlobTooLarge {
            content_ref: requested.clone(),
            max_bytes: 8,
            observed_at_least: 9,
        },
        StorageError::BlobSizeMismatch {
            content_ref: requested.clone(),
            metadata_bytes: 7,
            actual_bytes: 8,
        },
        StorageError::BlobDigestMismatch {
            expected: requested,
            actual,
        },
    ];

    for error in errors {
        assert_eq!(error.capability(), Some(StorageCapability::Blob));
        assert!(!error.is_retryable());
    }
}

include!("error_tests/fts_classification_tests.rs");

fn driver_err_sql(operation: &'static str, message: &str) -> StorageError {
    StorageError::driver(
        StorageCapability::Sql,
        operation,
        FakeSource(message.into()),
    )
}

include!("error_tests/unique_constraint_classification_tests.rs");

#[test]
fn secret_key_error_is_typed_notes_metadata_without_retry_or_raw_input() {
    let key = format!("ghp_{}", "A".repeat(40));
    let error = crate::note::validate_note_key_for_store(&key, "assign_note_key_if_unset")
        .expect_err("a credential key must be refused");
    assert_eq!(error.capability(), Some(StorageCapability::Notes));
    assert!(!error.is_retryable(), "secret refusal is not transient");
    let StorageError::SecretDetected {
        capability,
        operation,
        matched,
    } = &error
    else {
        panic!("expected typed secret refusal, got {error:?}");
    };
    assert_eq!(*capability, StorageCapability::Notes);
    assert_eq!(operation.as_ref(), "assign_note_key_if_unset");
    assert_eq!(matched.detector, "github-token");
    assert_eq!(matched.trigger, None);
    assert_eq!(matched.location.as_deref(), Some("note.key"));
    assert_eq!(matched.masked, "ghp_AA...44chars");
    assert_eq!(error.to_string(), format!("write blocked: {matched}"));
    assert!(!error.to_string().contains(&key));
    assert!(!format!("{error:?}").contains(&key));
    assert!(StdError::source(&error).is_none());
}
