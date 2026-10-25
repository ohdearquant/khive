use super::runtime_error_value;
use crate::{
    AuditObligationFailure, DenialAuditOutcome, DenialReceipt, DomainDisposition, RuntimeError,
};
use khive_storage::{CapacityUnavailablePhase, StorageCapability, StorageError};
use khive_types::{Details, ErrorCode, ErrorDomain, KhiveError};
use serde_json::json;

#[test]
fn capacity_phase_projection_preserves_writer_request_settlement() {
    for (phase, retryable) in [
        (CapacityUnavailablePhase::Identity, false),
        (CapacityUnavailablePhase::Lock, false),
        (CapacityUnavailablePhase::LockContended, true),
        (CapacityUnavailablePhase::Probe, false),
    ] {
        let direct = runtime_error_value(
            RuntimeError::Storage(StorageError::CapacityUnavailable {
                capability: StorageCapability::Sql,
                phase,
                message: "unavailable".into(),
            }),
            DomainDisposition::Unknown,
        );
        assert_eq!(direct["phase"], phase.as_str());
        assert_eq!(direct["retryable"], retryable);
        let value = runtime_error_value(
            RuntimeError::Storage(StorageError::WriterTaskRequestFailed {
                request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                source: Box::new(StorageError::CapacityUnavailable {
                    capability: StorageCapability::Sql,
                    phase,
                    message: "unavailable".into(),
                }),
            }),
            DomainDisposition::Unknown,
        );
        assert_eq!(value["code"], "sqlite_capacity_unavailable");
        assert_eq!(value["phase"], phase.as_str());
        assert_eq!(value["request_state"], "transaction_rolled_back");
        assert_eq!(value["task_terminated"], false);
        assert_eq!(value["retryable"], retryable);
    }
}

#[test]
fn sqlite_capacity_stages_preserve_typed_evidence_and_do_not_retry() {
    let refused = runtime_error_value(
        RuntimeError::Storage(StorageError::CapacityFloor {
            capability: StorageCapability::Sql,
            volume: "/volume".to_string(),
            available_bytes: 99,
            floor_bytes: 100,
            required_headroom_bytes: 12,
        }),
        DomainDisposition::Unknown,
    );
    assert_eq!(refused["stage"], "sqlite_capacity_refused");
    assert_eq!(refused["available_bytes"], 99);
    assert_eq!(refused["reserve_bytes"], 100);
    assert_eq!(refused["required_headroom_bytes"], 12);
    assert_eq!(refused["retryable"], false);

    let unavailable = runtime_error_value(
        RuntimeError::Storage(StorageError::CapacityUnavailable {
            capability: StorageCapability::Sql,
            phase: CapacityUnavailablePhase::Lock,
            message: "lock directory missing".to_string(),
        }),
        DomainDisposition::Unknown,
    );
    assert_eq!(unavailable["stage"], "sqlite_capacity_unavailable");
    assert_eq!(unavailable["phase"], "lock");
    assert_eq!(unavailable["retryable"], false);

    let full = runtime_error_value(
        RuntimeError::Storage(StorageError::driver(
            StorageCapability::Sql,
            "write",
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                None,
            ),
        )),
        DomainDisposition::Unknown,
    );
    assert_eq!(full["stage"], "sqlite_disk_full");
    assert_eq!(full["sqlite_primary_code"], rusqlite::ffi::SQLITE_FULL);
    assert_eq!(full["retryable"], false);
}

#[test]
fn unsettled_sqlite_writer_converts_to_side_effects_unknown() {
    for error in [
        khive_db::SqliteError::InheritedWriterTransaction,
        khive_db::SqliteError::WriterSettlementUnknown,
    ] {
        let value = runtime_error_value(RuntimeError::from(error), DomainDisposition::Unknown);
        assert_eq!(value["request_state"], "side_effects_unknown");
        assert_eq!(value["task_terminated"], true);
        assert_eq!(value["retryable"], false);
    }
}

#[test]
fn poisoned_writer_refusal_converts_to_not_started() {
    let value = runtime_error_value(
        RuntimeError::from(khive_db::SqliteError::WriterPoisoned),
        DomainDisposition::Unknown,
    );
    assert_eq!(value["request_state"], "not_started");
    assert_eq!(value["task_terminated"], true);
    assert_eq!(value["retryable"], false);
}

#[test]
fn missing_subject_projection_is_typed_without_matching_error_text() {
    for disposition in [DomainDisposition::Unknown, DomainDisposition::NotCommitted] {
        let error = RuntimeError::NotFound("subject missing".into());
        let message = error.to_string();
        let value = runtime_error_value(error, disposition);
        assert_eq!(value["kind"], "not_found");
        assert_eq!(value.get("code"), Some(&serde_json::Value::Null));
        assert_eq!(value.get("details"), Some(&serde_json::Value::Null));
        assert_eq!(value["message"], message);
        assert_eq!(value["domain_disposition"], disposition.as_str());
    }
    for error in [
        RuntimeError::Internal("not found: damaged store index".into()),
        RuntimeError::InvalidInput("not found: malformed reference".into()),
    ] {
        let value = runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(value["kind"], "runtime_error");
    }
}

#[test]
fn projection_preserves_structured_source_fields_and_serialized_bytes() {
    let source = KhiveError::unavailable("append reply missing")
        .with_code(ErrorCode::new(ErrorDomain::Db, 71))
        .with_details(Details::new([
            ("operation", "append"),
            ("driver_phase", "awaiting_reply"),
            ("retry_hint", "do_not_repeat"),
            ("extra", "preserve me\nincluding escapes"),
        ]));
    let mut expected = serde_json::to_value(&source).unwrap();
    expected["domain_disposition"] = json!("unknown");
    let expected_bytes = serde_json::to_vec(&expected).unwrap();
    let actual = runtime_error_value(source.into(), DomainDisposition::Unknown);
    assert_eq!(actual, expected);
    assert_eq!(serde_json::to_vec(&actual).unwrap(), expected_bytes);
}

#[test]
fn embedding_truncation_after_write_projects_committed_disposition() {
    let error = KhiveError::internal("embedding input truncated").with_details(Details::new([
        ("reason", "embedding_input_truncated"),
        ("record_id", "00000000-0000-0000-0000-000000000001"),
        ("committed", "true"),
        ("retryable", "false"),
    ]));
    let value = runtime_error_value(error.into(), DomainDisposition::Unknown);
    assert_eq!(value["domain_disposition"], "committed");
    assert_eq!(
        value["details"]["record_id"],
        "00000000-0000-0000-0000-000000000001"
    );
}

#[test]
fn resolution_wrapper_adds_details_without_changing_source_classification() {
    let source = RuntimeError::Khive(KhiveError::conflict("source conflict").with_details(
        Details::new([("reason", "seq_conflict"), ("extra", "retained")]),
    ));
    let selected = uuid::Uuid::from_u128(1);
    let duplicate = uuid::Uuid::from_u128(2);
    let error = source.with_resolution(crate::error::ResolutionFacts {
        project_id: selected,
        duplicate_anchor_ids: vec![duplicate],
        slug_backfilled: true,
        project_created: false,
        orphaned_project_id: None,
        orphaned_note_count: 0,
    });
    assert!(matches!(error.refusal_source(), RuntimeError::Khive(_)));
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["domain_disposition"], "not_committed");
    assert_eq!(value["details"]["reason"], "seq_conflict");
    assert_eq!(value["details"]["extra"], "retained");
    assert_eq!(
        value["details"]["resolution"]["project_id"],
        selected.to_string()
    );
    assert_eq!(
        value["details"]["resolution"]["duplicate_anchor_ids"],
        json!([duplicate])
    );
}

#[test]
fn resolution_wrapper_projects_remote_fetch_type_and_anchor_facts() {
    let selected = uuid::Uuid::from_u128(3);
    let error = RuntimeError::RemoteFetchError {
        remote: "https://example.com/repo".into(),
        message: "cache repair failed".into(),
    }
    .with_resolution(crate::error::ResolutionFacts {
        project_id: selected,
        duplicate_anchor_ids: vec![],
        slug_backfilled: false,
        project_created: false,
        orphaned_project_id: None,
        orphaned_note_count: 0,
    });
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["kind"], "remote_fetch_error");
    assert_eq!(value["remote"], "https://example.com/repo");
    assert_eq!(value["message"], "cache repair failed");
    assert_eq!(value["domain_disposition"], "unknown");
    assert_eq!(
        value["details"]["resolution"]["project_id"],
        selected.to_string()
    );
    assert_eq!(value["details"]["resolution"]["project_created"], false);
}

#[test]
fn named_refusals_keep_their_override_without_classifying_arbitrary_conflicts() {
    for (reason, expected) in [
        ("seq_conflict", "not_committed"),
        ("fence_conflict", "not_committed"),
        ("arbitrary_conflict", "unknown"),
    ] {
        let source = KhiveError::conflict("same rendered message")
            .with_details(Details::new([("reason", reason), ("extra", "retained")]));
        let value = runtime_error_value(source.into(), DomainDisposition::Unknown);
        assert_eq!(value["domain_disposition"], expected);
        assert_eq!(value["details"]["extra"], "retained");
    }
}

#[test]
fn idempotency_key_conflict_projects_not_committed() {
    let source = KhiveError::conflict("different content under an existing key").with_details(
        Details::new([
            ("reason", "idempotency_key_conflict"),
            ("key", "operation-1"),
            ("existing_id", "holder-1"),
        ]),
    );
    let value = runtime_error_value(source.into(), DomainDisposition::Unknown);
    assert_eq!(value["kind"], "conflict");
    assert_eq!(value["details"]["reason"], "idempotency_key_conflict");
    assert_eq!(
        value["domain_disposition"], "not_committed",
        "keyed refusal must project not_committed"
    );
}

#[test]
fn shared_projection_retains_obligation_result_and_denial_receipt() {
    let domain_result = json!({"rows": [{"id": "recorded", "extra": [null, true, 17]}]});
    let failure = Box::new(AuditObligationFailure::new(
        "stream.append",
        crate::audit_batch::AuditTerminalReason::StoreFailure,
    ));
    let expected = json!({
        "kind": "obligation", "code": failure.wire_code(),
        "message": failure.to_string(), "domain_result": domain_result,
        "domain_disposition": "unknown",
    });
    let projected = runtime_error_value(
        RuntimeError::AuditObligation {
            failure,
            domain_result,
        },
        DomainDisposition::Unknown,
    );
    assert_eq!(projected, expected);
    assert_eq!(
        serde_json::to_vec(&projected).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );

    let event_id = uuid::Uuid::from_u128(17);
    let denied = RuntimeError::PermissionDenied {
        verb: "stream.append".into(),
        reason: "policy".into(),
        receipt: Box::new(DenialReceipt {
            audit_event_id: Some(event_id),
            audit_outcome: DenialAuditOutcome::Committed,
        }),
    };
    let expected = json!({
        "kind": "runtime_error", "code": "permission_denied", "message": denied.to_string(),
        "verb": "stream.append", "reason": "policy", "audit_event_id": event_id.to_string(),
        "audit_outcome": "committed", "domain_disposition": "not_committed",
    });
    assert_eq!(
        runtime_error_value(denied, DomainDisposition::NotCommitted),
        expected
    );
}

/// A refusal that wrote a durable receipt must name it as its own field.
/// The consumer contract is `error.receipt_id`, not a substring of the
/// sentence: the sentence is free to be reworded and a regular expression
/// over it breaks without failing anything.
#[test]
fn a_refusal_receipt_id_is_a_field_and_not_only_a_substring_of_the_message() {
    let error = RuntimeError::RefusedWithReceipt(Box::new(crate::error::ReceiptRefusal {
        code: "exec_refused",
        message: "exec.run refused: tool not registered (receipt_id=r-1)".into(),
        receipt_id: "r-1".into(),
        reason: "tool not registered".into(),
        detail: json!({ "effective_max_output_bytes": 65536, "receipt_id": "shadow" }),
    }));

    // Disposition is deliberately the wrong one on the way in: a receipt-bearing
    // refusal establishes its own no-write, so the boundary's guess is overridden.
    let value = runtime_error_value(error, DomainDisposition::Unknown);

    assert_eq!(
        value["receipt_id"], "r-1",
        "a detail member cannot shadow a contract field"
    );
    assert_eq!(value["code"], "exec_refused");
    assert_eq!(value["kind"], "runtime_error");
    assert_eq!(value["domain_disposition"], "not_committed");
    assert_eq!(value["reason"], "tool not registered");
    assert_eq!(value["effective_max_output_bytes"], 65536);
    assert_eq!(
        value["message"], "exec.run refused: tool not registered (receipt_id=r-1)",
        "the existing wording is kept so a reader that parses it today keeps working"
    );
}

/// The control for the arm above: a refusal carrying no receipt is a
/// different error entirely and must not grow a `receipt_id`. Without this,
/// an implementation that stamped the field unconditionally would pass.
#[test]
fn an_ordinary_invalid_input_has_no_receipt_id() {
    let value = runtime_error_value(
        RuntimeError::InvalidInput("exec.run refused: tool not registered".into()),
        DomainDisposition::Unknown,
    );

    assert!(
        value.get("receipt_id").is_none(),
        "an error with no durable receipt must not name one: {value}"
    );
    assert_eq!(value["domain_disposition"], "unknown");
}

#[test]
fn terminal_full_projection_requires_consistent_native_codes() {
    for codes in [
        None,
        Some((5, 5)),
        Some((13, 5)),
        Some((5, 13)),
        Some((13, -243)),
    ] {
        let error = StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
            sqlite_full_codes: codes,
        };
        let value = runtime_error_value(
            crate::RuntimeError::Storage(error),
            crate::DomainDisposition::Unknown,
        );
        assert_eq!(value["stage"], "writer_task_terminated");
        assert!(value.get("sqlite_primary_code").is_none());
        assert_eq!(value["retryable"], false);
    }
    let extended = rusqlite::ffi::SQLITE_FULL | (3 << 8);
    let error = StorageError::WriterTaskTerminated {
        request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        sqlite_full_codes: Some((rusqlite::ffi::SQLITE_FULL, extended)),
    };
    let value = runtime_error_value(
        crate::RuntimeError::Storage(error),
        crate::DomainDisposition::Unknown,
    );
    assert_eq!(value["stage"], "sqlite_disk_full");
    assert_eq!(value["code"], "sqlite_disk_full");
    assert_eq!(value["sqlite_primary_code"], rusqlite::ffi::SQLITE_FULL);
    assert_eq!(value["sqlite_extended_code"], extended);
    assert_eq!(value["request_state"], "side_effects_unknown");
    assert_eq!(value["task_terminated"], true);
    assert_eq!(value["retryable"], false);
    assert_eq!(value["domain_disposition"], "unknown");
}

#[test]
fn projection_keeps_typed_writer_state_and_capability_spelling() {
    let error = RuntimeError::Storage(khive_storage::StorageError::writer_task_terminated(
        khive_storage::WriterTaskRequestState::SideEffectsUnknown,
    ));
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["request_state"], "side_effects_unknown");
    assert_eq!(value["task_terminated"], true);
    assert_eq!(value["retryable"], false);
    assert_eq!(value["domain_disposition"], "unknown");

    let error = RuntimeError::Storage(khive_storage::StorageError::driver(
        StorageCapability::Sql,
        "append checkout",
        khive_db::SqliteError::WriterPoolCheckoutTimeout {
            timeout: std::time::Duration::from_millis(17),
        },
    ));
    let value = runtime_error_value(error, DomainDisposition::Unknown);
    assert_eq!(value["capability"], "sql");
    assert_eq!(value["timeout_ms"], 17);
    assert_eq!(value["operation"], "append checkout");
    assert_eq!(value["domain_disposition"], "unknown");
}

#[test]
fn refusal_events_project_mixed_recording_without_changing_the_original_secret_error() {
    use crate::{RefusalEventRecording, RefusalRecordingErrorClass};
    let source = || {
        RuntimeError::SecretDetected(crate::secret_gate::SecretMatch {
            detector: "fixture",
            trigger: None,
            masked: "PRIVATE-MASKED-EXCERPT".into(),
            location: Some("atoms[1].properties[0].value".into()),
        })
    };
    let subject_a = uuid::Uuid::from_u128(11);
    let subject_b = uuid::Uuid::from_u128(12);
    let event = uuid::Uuid::from_u128(13);
    let expected = runtime_error_value(source(), DomainDisposition::Unknown);
    let mut actual = runtime_error_value(
        source().with_refusal_events(vec![
            RefusalEventRecording::Recorded {
                item_index: 1,
                subject: subject_a,
                event_id: event,
            },
            RefusalEventRecording::Failed {
                item_index: 4,
                subject: subject_b,
                error_class: RefusalRecordingErrorClass::EventAppendFailed,
            },
        ]),
        DomainDisposition::Unknown,
    );
    assert_eq!(actual["refusal_recorded"], false);
    assert_eq!(
        actual["refusal_events"],
        json!([
            {"item_index":1, "subject":subject_a, "event_id":event},
            {"item_index":4, "subject":subject_b, "error_class":"event_append_failed"},
        ])
    );
    assert!(!actual.to_string().contains("PRIVATE-MASKED-EXCERPT"));
    assert!(actual.get("receipt_id").is_none());
    let object = actual.as_object_mut().unwrap();
    object.remove("refusal_recorded");
    object.remove("refusal_events");
    assert_eq!(actual, expected);
}

#[test]
fn refusal_events_keep_typed_details_and_named_disposition_with_all_recorded() {
    use crate::RefusalEventRecording;
    let source = || {
        RuntimeError::Khive(
            KhiveError::conflict("fixed reason")
                .with_code(ErrorCode::new(ErrorDomain::Db, 71))
                .with_details(Details::new([
                    ("reason", "seq_conflict"),
                    ("extra", "unchanged"),
                ])),
        )
    };
    let mut expected = runtime_error_value(source(), DomainDisposition::Unknown);
    expected["refusal_recorded"] = json!(true);
    expected["refusal_events"] = json!([
        {"item_index":0, "subject":uuid::Uuid::from_u128(1), "event_id":uuid::Uuid::from_u128(2)},
        {"item_index":3, "subject":uuid::Uuid::from_u128(3), "event_id":uuid::Uuid::from_u128(4)},
    ]);
    let actual = runtime_error_value(
        source().with_refusal_events(vec![
            RefusalEventRecording::Recorded {
                item_index: 0,
                subject: uuid::Uuid::from_u128(1),
                event_id: uuid::Uuid::from_u128(2),
            },
            RefusalEventRecording::Recorded {
                item_index: 3,
                subject: uuid::Uuid::from_u128(3),
                event_id: uuid::Uuid::from_u128(4),
            },
        ]),
        DomainDisposition::Unknown,
    );
    assert_eq!(actual, expected);
    assert_eq!(actual["domain_disposition"], "not_committed");
}

#[test]
fn refusal_events_do_not_invent_receipts_for_failed_or_absent_recordings() {
    use crate::{RefusalEventRecording, RefusalRecordingErrorClass};
    for class in [
        RefusalRecordingErrorClass::EventStoreUnavailable,
        RefusalRecordingErrorClass::EventAppendFailed,
    ] {
        let error =
            RuntimeError::InvalidInput("validation refused".into()).with_refusal_events(vec![
                RefusalEventRecording::Failed {
                    item_index: 0,
                    subject: uuid::Uuid::from_u128(1),
                    error_class: class,
                },
            ]);
        let actual = runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(actual["refusal_recorded"], false);
        assert_eq!(actual["refusal_events"][0]["error_class"], class.as_str());
        assert!(actual["refusal_events"][0].get("event_id").is_none());
        assert!(actual.get("receipt_id").is_none());
        assert_eq!(actual["message"], "invalid input: validation refused");
    }
    let actual = runtime_error_value(
        RuntimeError::InvalidInput("validation refused".into()).with_refusal_events(vec![]),
        DomainDisposition::Unknown,
    );
    assert!(actual.get("refusal_recorded").is_none());
    assert!(actual.get("refusal_events").is_none());
}

use super::{failure_entry_parts, FailureEntrySource};

fn assert_failure_entry_disposition(entry: &serde_json::Value, expected: &str) {
    assert_eq!(entry["ok"], false);
    assert_eq!(entry["domain_disposition"], expected);
    assert_eq!(entry["error"]["domain_disposition"], expected);
}

fn failure_entry_obligation(result: serde_json::Value) -> RuntimeError {
    RuntimeError::AuditObligation {
        failure: Box::new(AuditObligationFailure::new(
            "stream.append",
            crate::audit_batch::AuditTerminalReason::StoreFailure,
        )),
        domain_result: result,
    }
}

fn failure_entry_receipt(result: serde_json::Value) -> RuntimeError {
    RuntimeError::RefusedWithReceipt(Box::new(crate::error::ReceiptRefusal {
        code: "exec_refused",
        message: "operation refused".into(),
        receipt_id: "receipt-1".into(),
        reason: "policy".into(),
        detail: json!({"domain_result": result}),
    }))
}

#[test]
fn failure_entry_string_error_keeps_exact_envelope_and_error_parts() {
    let error = json!("operation unavailable");
    let expected = json!({
        "ok": false,
        "tool": "probe",
        "domain_disposition": "not_committed",
        "error": {
            "kind": "runtime_error",
            "message": "operation unavailable",
            "domain_disposition": "not_committed",
        },
    });
    let entry = failure_entry_parts(FailureEntrySource::Value {
        error: error.clone(),
        disposition: DomainDisposition::NotCommitted,
    })
    .into_entry("probe");
    let error_only = failure_entry_parts(FailureEntrySource::Value {
        error,
        disposition: DomainDisposition::NotCommitted,
    })
    .into_error();
    assert_eq!(entry, expected);
    assert_eq!(error_only, expected["error"]);
    assert_eq!(
        serde_json::to_vec(&entry).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
}

#[test]
fn failure_entry_non_object_error_uses_the_existing_generic_message() {
    for error in [json!(null), json!(true), json!(17), json!(["discarded"])] {
        let entry = failure_entry_parts(FailureEntrySource::Value {
            error,
            disposition: DomainDisposition::Unknown,
        })
        .into_entry("probe");
        assert_eq!(
            entry,
            json!({
                "ok": false,
                "tool": "probe",
                "domain_disposition": "unknown",
                "error": {
                    "kind": "runtime_error",
                    "message": "operation failed",
                    "domain_disposition": "unknown",
                },
            })
        );
    }
}

#[test]
fn failure_entry_unknown_drops_a_nested_domain_result() {
    let entry = failure_entry_parts(FailureEntrySource::Value {
        error: json!({"kind":"obligation", "message":"unsettled", "domain_result":{"id":"nested"}}),
        disposition: DomainDisposition::Unknown,
    })
    .into_entry("probe");
    assert_failure_entry_disposition(&entry, "unknown");
    assert_eq!(entry["error"]["kind"], "obligation");
    assert!(
        entry["error"].get("domain_result").is_none(),
        "unknown outcome retained a nested domain result"
    );
}

#[test]
fn failure_entry_committed_boundary_and_projection_keep_the_complete_result() {
    let expected = json!({"id":"recorded", "rows":[null,true,{"nested":[1,2]}]});
    let entry = failure_entry_parts(FailureEntrySource::Runtime {
        error: failure_entry_obligation(expected.clone()),
        boundary: DomainDisposition::Committed,
    })
    .into_entry("probe");
    assert_failure_entry_disposition(&entry, "committed");
    assert_eq!(entry["error"]["kind"], "obligation");
    assert_eq!(entry["error"]["code"], "store_failure");
    assert_eq!(entry["error"]["domain_result"], expected);
}

#[test]
fn failure_entry_committed_boundary_does_not_keep_a_refused_projection_result() {
    let entry = failure_entry_parts(FailureEntrySource::Runtime {
        error: failure_entry_receipt(json!({"id":"nested", "extra":[1,2]})),
        boundary: DomainDisposition::Committed,
    })
    .into_entry("probe");
    assert_failure_entry_disposition(&entry, "not_committed");
    assert_eq!(entry["error"]["code"], "exec_refused");
    assert_eq!(entry["error"]["receipt_id"], "receipt-1");
    assert!(
        entry["error"].get("domain_result").is_none(),
        "a refused projection retained the nested domain result"
    );
}

#[test]
fn failure_entry_uncommitted_boundary_drops_a_committed_batch_projection_result() {
    let error = RuntimeError::BatchWrite {
        context: Box::new(crate::error::BatchWriteContext {
            source: Box::new(failure_entry_obligation(json!({"id":"nested"}))),
            disposition: DomainDisposition::Committed,
            admission: None,
        }),
    };
    assert_eq!(
        error.batch_write_disposition(),
        Some(DomainDisposition::Committed)
    );
    let entry = failure_entry_parts(FailureEntrySource::Runtime {
        error,
        boundary: DomainDisposition::NotCommitted,
    })
    .into_entry("probe");
    assert_failure_entry_disposition(&entry, "committed");
    assert_eq!(entry["error"]["kind"], "obligation");
    assert!(
        entry["error"].get("domain_result").is_none(),
        "the original boundary did not permit retaining a domain result"
    );
}

#[test]
fn failure_entry_too_deep_replacement_precedes_projection_result_removal() {
    for refused_projection in [false, true] {
        let mut result = json!(true);
        for _ in 0..crate::daemon::ERROR_DETAIL_NESTING_DEPTH_LIMIT + 1 {
            result = serde_json::Value::Array(vec![result]);
        }
        assert!(!crate::daemon::error_detail_value_within_limit(&result));
        let error = if refused_projection {
            failure_entry_receipt(result)
        } else {
            failure_entry_obligation(result)
        };
        let entry = failure_entry_parts(FailureEntrySource::Runtime {
            error,
            boundary: DomainDisposition::Committed,
        })
        .into_entry("probe");
        assert_failure_entry_disposition(
            &entry,
            if refused_projection {
                "not_committed"
            } else {
                "committed"
            },
        );
        assert_eq!(
            entry["error"]["code"], "result_too_deep",
            "too-deep committed result did not keep the existing replacement code"
        );
        assert_eq!(
            entry["error"]["message"],
            "committed domain result omitted because it exceeds the nesting depth limit"
        );
        assert!(entry["error"].get("domain_result").is_none());
    }
}

#[test]
fn failure_entry_named_refusal_uses_the_projected_disposition_on_both_fields() {
    let error = RuntimeError::Khive(KhiveError::conflict("same rendered message").with_details(
        Details::new([("reason", "seq_conflict"), ("extra", "retained")]),
    ));
    assert_eq!(error.batch_write_disposition(), None);
    let dispatch = crate::DispatchError::after_handler(error, false);
    assert_eq!(dispatch.disposition(), DomainDisposition::Unknown);
    let (error, boundary) = dispatch.into_parts();
    let entry =
        failure_entry_parts(FailureEntrySource::Runtime { error, boundary }).into_entry("probe");
    assert_failure_entry_disposition(&entry, "not_committed");
    assert_eq!(entry["error"]["kind"], "conflict");
    assert_eq!(entry["error"]["details"]["reason"], "seq_conflict");
    assert_eq!(entry["error"]["details"]["extra"], "retained");
}

#[test]
fn failure_entry_raw_value_cannot_override_the_supplied_disposition() {
    for disposition in [DomainDisposition::NotCommitted, DomainDisposition::Unknown] {
        let entry = failure_entry_parts(FailureEntrySource::Value {
            error: json!({"kind":"runtime_error", "message":"input claim", "domain_disposition":"committed", "extra":17}),
            disposition,
        }).into_entry("probe");
        assert_failure_entry_disposition(&entry, disposition.as_str());
        assert_eq!(entry["error"]["message"], "input claim");
        assert_eq!(entry["error"]["extra"], 17);
    }
}

#[test]
fn failure_entry_batch_write_proof_and_committed_boundary_keep_their_precedence() {
    for (boundary, expected) in [
        (DomainDisposition::Unknown, "not_committed"),
        (DomainDisposition::Committed, "committed"),
    ] {
        let error = RuntimeError::InvalidInput("prewrite refusal".into()).before_requested_write();
        assert_eq!(
            error.batch_write_disposition(),
            Some(DomainDisposition::NotCommitted)
        );
        let entry = failure_entry_parts(FailureEntrySource::Runtime { error, boundary })
            .into_entry("probe");
        assert_failure_entry_disposition(&entry, expected);
        assert_eq!(entry["error"]["message"], "invalid input: prewrite refusal");
        assert!(entry["error"].get("domain_result").is_none());
    }
}
