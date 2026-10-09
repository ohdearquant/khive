//! ADR-071 A1: concrete backend errors cross the public runtime boundary as storage errors.

use std::time::Duration;

use khive_db::SqliteError;
use khive_runtime::{runtime_error_value, DomainDisposition, RuntimeError};
use khive_storage::{
    CapacityUnavailablePhase, StorageCapability, StorageError, WriterTaskRequestState,
};
use serde_json::json;

fn project(error: SqliteError) -> serde_json::Value {
    let error = RuntimeError::from(error);
    assert!(matches!(error, RuntimeError::Storage(_)), "{error:?}");
    runtime_error_value(error, DomainDisposition::Unknown)
}

#[test]
fn backend_configuration_errors_retain_typed_sources_in_storage() {
    let error = RuntimeError::from(SqliteError::InvalidConfig("fixture".into()));
    let RuntimeError::Storage(StorageError::Driver { source, .. }) = &error else {
        panic!("expected storage driver failure: {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<SqliteError>(),
        Some(SqliteError::InvalidConfig(message)) if message == "fixture"
    ));
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["kind"], "runtime_error");
    assert!(value.get("code").is_none());
    assert_eq!(value["domain_disposition"], "unknown");
}

#[test]
fn backend_capacity_and_native_full_keep_their_wire_codes() {
    let refused = project(SqliteError::CapacityFloor {
        volume: "/volume".into(),
        available_bytes: 99,
        floor_bytes: 100,
        required_headroom_bytes: 12,
    });
    assert_eq!(refused["code"], "sqlite_capacity_refused");
    assert_eq!(refused["stage"], "sqlite_capacity_refused");
    assert_eq!(refused["volume"], "/volume");
    assert_eq!(refused["available_bytes"], 99);
    assert_eq!(refused["reserve_bytes"], 100);
    assert_eq!(refused["required_headroom_bytes"], 12);
    assert_eq!(refused["retryable"], false);
    assert_eq!(refused["domain_disposition"], "unknown");

    for phase in [
        CapacityUnavailablePhase::Identity,
        CapacityUnavailablePhase::Lock,
        CapacityUnavailablePhase::Probe,
    ] {
        let unavailable = project(SqliteError::CapacityUnavailable {
            phase,
            message: "fixture".into(),
        });
        assert_eq!(unavailable["code"], "sqlite_capacity_unavailable");
        assert_eq!(unavailable["phase"], phase.as_str());
        assert_eq!(unavailable["retryable"], false);
        assert_eq!(unavailable["capability"], "sql");
    }

    let full = project(SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
        None,
    )));
    assert_eq!(full["code"], "sqlite_disk_full");
    assert_eq!(full["sqlite_primary_code"], rusqlite::ffi::SQLITE_FULL);
    assert_eq!(full["sqlite_extended_code"], rusqlite::ffi::SQLITE_FULL);
    assert_eq!(full["retryable"], false);
}

#[test]
fn backend_checkout_keeps_safe_retry_and_typed_timeout() {
    let error = RuntimeError::from(SqliteError::WriterPoolCheckoutTimeout {
        timeout: Duration::from_millis(17),
    });
    assert!(matches!(error, RuntimeError::Storage(_)));
    let context = error.writer_pool_checkout_timeout_context().unwrap();
    assert_eq!(context.timeout, Duration::from_millis(17));
    assert_eq!(context.capability, Some(StorageCapability::Sql));
    assert_eq!(context.operation.as_deref(), Some("runtime"));
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["code"], "writer_pool_checkout_timeout");
    assert_eq!(value["retryable"], true);
    assert_eq!(value["timeout_ms"], 17);
}

#[test]
fn backend_settlement_and_read_failures_keep_structural_classification() {
    for (error, expected) in [
        (
            SqliteError::InheritedWriterTransaction,
            WriterTaskRequestState::SideEffectsUnknown,
        ),
        (
            SqliteError::WriterSettlementUnknown,
            WriterTaskRequestState::SideEffectsUnknown,
        ),
        (
            SqliteError::WriterPoisoned,
            WriterTaskRequestState::NotStarted,
        ),
    ] {
        let runtime = RuntimeError::from(error);
        let context = runtime.writer_task_failure_context().unwrap();
        assert_eq!(context.request_state, expected);
        assert!(context.task_terminated);
        let value = runtime_error_value(runtime, DomainDisposition::Unknown);
        assert_eq!(value["request_state"], json!(expected.to_string()));
        assert_eq!(value["task_terminated"], true);
    }

    let runtime = RuntimeError::from(SqliteError::RequestReadStopped(
        StorageError::AdmissionTimeout {
            operation: "read".into(),
            timeout_ms: 23,
            pool_identity: Some("fixture-pool".into()),
        },
    ));
    assert!(matches!(
        runtime,
        RuntimeError::Storage(StorageError::AdmissionTimeout { .. })
    ));
    let context = runtime.retryable_failure_context().unwrap();
    assert_eq!(context.timeout, Duration::from_millis(23));
    assert_eq!(context.pool_identity.as_deref(), Some("fixture-pool"));
}
