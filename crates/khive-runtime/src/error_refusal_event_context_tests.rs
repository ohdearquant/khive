use super::*;

fn failed() -> Vec<RefusalEventRecording> {
    vec![RefusalEventRecording::Failed {
        item_index: 2,
        subject: Uuid::from_u128(11),
        error_class: RefusalRecordingErrorClass::EventAppendFailed,
    }]
}

#[test]
fn refusal_events_preserve_the_source_type_display_and_policy_class() {
    let original = RuntimeError::SecretDetected(crate::secret_gate::SecretMatch {
        detector: "fixture",
        trigger: None,
        masked: "never-in-display".into(),
        location: Some("atoms[2].content".into()),
    });
    let message = original.to_string();
    let wrapped = original.with_refusal_events(failed());
    assert_eq!(wrapped.to_string(), message);
    assert!(matches!(
        wrapped.refusal_source(),
        RuntimeError::SecretDetected(_)
    ));
    assert!(std::error::Error::source(&wrapped)
        .unwrap()
        .downcast_ref::<RuntimeError>()
        .is_some_and(|error| matches!(error, RuntimeError::SecretDetected(_))));
    assert_eq!(
        wrapped.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Permanent {
            reason: "SecretDetected"
        }
    );
    assert!(wrapped.retryable_failure_context().is_none());

    let lookalike = RuntimeError::InvalidInput(message).with_refusal_events(failed());
    assert_eq!(
        lookalike.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Unknown {
            reason: "InvalidInput"
        }
    );
    let policy = RuntimeError::Khive(
        khive_types::KhiveError::conflict("immutable")
            .with_details(khive_types::Details::new([("reason", "stream_member")])),
    )
    .with_refusal_events(failed());
    assert!(policy.is_stream_policy_refusal());
}

#[test]
fn refusal_events_preserve_retry_and_writer_finality_contexts() {
    let checkout = RuntimeError::from(khive_db::SqliteError::WriterPoolCheckoutTimeout {
        timeout: Duration::from_millis(17),
    })
    .with_refusal_events(failed());
    assert_eq!(
        checkout
            .writer_pool_checkout_timeout_context()
            .unwrap()
            .timeout,
        Duration::from_millis(17)
    );
    assert_eq!(
        checkout.admission_failure_context().unwrap().stage,
        WRITER_POOL_CHECKOUT_TIMEOUT_STAGE
    );
    assert_eq!(
        checkout.retryable_failure_context().unwrap().stage,
        WRITER_POOL_CHECKOUT_TIMEOUT_STAGE
    );

    let queued =
        RuntimeError::Storage(khive_storage::StorageError::WriteQueueFull { timeout_ms: 23 })
            .with_refusal_events(failed());
    assert_eq!(
        queued.retryable_failure_context().unwrap().stage,
        WRITER_QUEUE_SATURATED_STAGE
    );
    assert_eq!(
        queued.channel_ingest_failure_class(),
        ChannelIngestFailureClass::Retryable { reason: "Storage" }
    );

    let busy =
        RuntimeError::Storage(khive_storage::StorageError::WriterTaskBusy { timeout_ms: 31 })
            .with_refusal_events(failed());
    assert_eq!(
        busy.retryable_failure_context().unwrap().stage,
        WRITER_TASK_BEGIN_BUSY_STAGE
    );

    let stopped = RuntimeError::Storage(khive_storage::StorageError::writer_task_terminated(
        khive_storage::WriterTaskRequestState::SideEffectsUnknown,
    ))
    .with_refusal_events(failed());
    let context = stopped.writer_task_failure_context().unwrap();
    assert_eq!(
        context.request_state,
        khive_storage::WriterTaskRequestState::SideEffectsUnknown
    );
    assert!(context.task_terminated);
    assert!(!context.retryable);
    assert!(stopped.retryable_failure_context().is_none());
}

#[test]
fn refusal_events_preserve_the_original_error_and_its_underlying_source_chain() {
    let original = RuntimeError::Storage(khive_storage::StorageError::driver(
        khive_storage::StorageCapability::Notes,
        "refusal-context-fixture",
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "fixture refusal"),
    ));
    let message = original.to_string();
    let wrapped = original.with_refusal_events(failed());
    assert_eq!(wrapped.to_string(), message);

    let runtime = std::error::Error::source(&wrapped)
        .unwrap()
        .downcast_ref::<RuntimeError>()
        .expect("the first source must retain the runtime classification");
    assert!(matches!(runtime, RuntimeError::Storage(_)));
    let storage = std::error::Error::source(runtime)
        .unwrap()
        .downcast_ref::<khive_storage::StorageError>()
        .expect("the original runtime error must retain its storage source");
    let driver = std::error::Error::source(storage)
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .expect("the storage source chain must remain traversable");
    assert_eq!(driver.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn refusal_events_with_no_eligible_targets_do_not_wrap_or_grow_metadata() {
    let error = RuntimeError::InvalidInput("unchanged".into()).with_refusal_events(vec![]);
    assert!(matches!(error, RuntimeError::InvalidInput(ref message) if message == "unchanged"));
    let error = error
        .with_refusal_events(failed())
        .with_refusal_events(vec![RefusalEventRecording::Recorded {
            item_index: 3,
            subject: Uuid::from_u128(12),
            event_id: Uuid::from_u128(13),
        }]);
    let source = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<RuntimeError>()
        .expect("appending recordings must not add another error wrapper");
    assert!(matches!(source, RuntimeError::InvalidInput(_)));
    assert!(std::error::Error::source(source).is_none());
    let RuntimeError::RefusedWithEvents { context } = error else {
        panic!("recordings must accompany the original typed source");
    };
    let RefusalEventContext { source, recordings } = context;
    assert!(matches!(*source, RuntimeError::InvalidInput(_)));
    assert_eq!(recordings.len(), 2);
    assert_eq!(recordings[0], failed()[0]);
}

#[test]
fn refusal_recording_classes_serialize_only_the_closed_safe_spellings() {
    for (class, name) in [
        (
            RefusalRecordingErrorClass::EventStoreUnavailable,
            "event_store_unavailable",
        ),
        (
            RefusalRecordingErrorClass::EventAppendFailed,
            "event_append_failed",
        ),
    ] {
        assert_eq!(class.as_str(), name);
        assert_eq!(
            serde_json::to_value(class).unwrap(),
            serde_json::json!(name)
        );
    }
}
