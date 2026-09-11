//! Canonical structured projection of typed runtime failures.
//!
//! Transport nesting limits and removal of a nested operation's domain result
//! belong to the transport boundary, not to this lossless shared projection.

use khive_storage::StorageCapability;
use serde_json::{json, Value};

use crate::{DomainDisposition, RuntimeError};

/// Project the original typed error, preserving every structured source field.
///
/// `disposition` describes the enclosing operation. Named runtime refusals keep
/// their existing wire override. This function moves an obligation's result
/// without cloning or recursively serializing it; callers apply their own depth
/// limits before any recursive operation on the returned value.
pub fn runtime_error_value(error: RuntimeError, disposition: DomainDisposition) -> Value {
    // These named outcomes carry their own domain proof. Do not infer general
    // write disposition from a conflict or unavailable variant.
    let named_disposition = match &error {
        RuntimeError::Khive(k) => match (k.kind(), k.details().and_then(|d| d.get("reason"))) {
            (khive_types::ErrorKind::Conflict, Some("key_conflict" | "fence_conflict")) => {
                Some("not_committed")
            }
            (khive_types::ErrorKind::Unavailable, Some("key_holder_unresolved")) => Some("unknown"),
            // ADR-174 A1.1: a stream member refusal carries
            // `domain_disposition: not_committed` wherever it surfaces. In
            // per-member mode it is the member's own value and the runtime
            // writes the field itself; in atomic mode the refusal is raised
            // as the call's error, where without these rows the boundary's
            // `unknown` would stand and the caller could not tell a batch
            // that wrote nothing from one whose outcome is unestablished.
            (khive_types::ErrorKind::Conflict, Some("seq_conflict")) => Some("not_committed"),
            (khive_types::ErrorKind::Conflict, Some("unknown_op")) => Some("not_committed"),
            (khive_types::ErrorKind::Conflict, Some("version_conflict" | "identity_conflict")) => {
                Some("not_committed")
            }
            (khive_types::ErrorKind::Conflict, Some("expired" | "live_until_unreadable")) => {
                Some("not_committed")
            }
            (khive_types::ErrorKind::NotFound, Some("stream_write_not_found")) => {
                Some("not_committed")
            }
            (khive_types::ErrorKind::InvalidInput, Some("member_unavailable")) => {
                Some("not_committed")
            }
            _ => None,
        },
        _ => None,
    };
    // The refusal text is the same Display string every consumer already
    // matches on; the receipt fields ride beside it.
    let denial_message =
        matches!(error, RuntimeError::PermissionDenied { .. }).then(|| error.to_string());
    let payload = match error {
        RuntimeError::PermissionDenied {
            verb,
            reason,
            receipt,
        } => json!({
            "kind": "runtime_error",
            "code": "permission_denied",
            "message": denial_message.unwrap_or_default(),
            "verb": verb,
            "reason": reason,
            "audit_event_id": receipt.audit_event_id.map(|id| id.to_string()),
            "audit_outcome": receipt.audit_outcome.wire_code(),
        }),
        RuntimeError::AuditObligation {
            failure,
            domain_result,
        } => {
            let mut error = serde_json::Map::from_iter([
                ("kind".into(), json!("obligation")),
                ("code".into(), json!(failure.wire_code())),
                ("message".into(), json!(failure.to_string())),
            ]);
            error.insert("domain_result".into(), domain_result);
            Value::Object(error)
        }
        RuntimeError::Khive(k) => serde_json::to_value(&k)
            .unwrap_or_else(|_| json!({"kind": "internal", "message": k.to_string()})),
        RuntimeError::RemoteFetchError { remote, message } => json!({
            "kind": "remote_fetch_error",
            "remote": remote,
            "message": message,
        }),
        other @ (RuntimeError::Storage(_)
        | RuntimeError::Sqlite(_)
        | RuntimeError::Query(_)
        | RuntimeError::NotFound(_)
        | RuntimeError::InvalidInput(_)
        | RuntimeError::UnknownVerb(_)
        | RuntimeError::Unconfigured(_)
        | RuntimeError::UnknownModel(_)
        | RuntimeError::Embedding(_)
        | RuntimeError::Ambiguous(_)
        | RuntimeError::Fusion(_)
        | RuntimeError::UnknownFusionStrategy(_)
        | RuntimeError::Internal(_)
        | RuntimeError::IncompatibleEventStore(_)
        | RuntimeError::GuardedWriteFailed(_)
        | RuntimeError::MissingPackDependency(_)
        | RuntimeError::MissingPackDependencies(_)
        | RuntimeError::CircularPackDependency(_)
        | RuntimeError::PackRedeclared { .. }
        | RuntimeError::VerbCollision { .. }
        | RuntimeError::ReservedEnvelopeParam { .. }
        | RuntimeError::GateUnavailable { .. }
        | RuntimeError::NamespaceMismatch { .. }
        | RuntimeError::AmbiguousPrefix { .. }
        | RuntimeError::CrossBackendMergeUnsupported { .. }
        | RuntimeError::UnknownRemote { .. }
        | RuntimeError::RemoteCacheMissing { .. }
        | RuntimeError::AmbiguousId { .. }
        | RuntimeError::CrossNamespaceWrite { .. }
        | RuntimeError::WriteBudgetExceeded { .. }
        | RuntimeError::SecretDetected(_)
        | RuntimeError::DeadlineExceeded { .. }) => {
            if let Some(context) = other.writer_task_failure_context() {
                json!({"kind":"storage", "code":context.stage, "stage":context.stage,
                    "message":other.to_string(), "retryable":context.retryable,
                    "request_state":context.request_state.to_string(), "task_terminated":context.task_terminated})
            } else if let Some(context) = other.retryable_failure_context() {
                let timeout_ms = u64::try_from(context.timeout.as_millis()).unwrap_or(u64::MAX);
                json!({"kind":"unavailable", "code":context.stage, "stage":context.stage,
                    "message":other.to_string(), "retryable":true, "timeout_ms":timeout_ms,
                    "capability":context.capability.map(storage_capability_wire_name),
                    "operation":context.operation, "scope":context.scope, "retry_after_ms":context.retry_after_ms})
            } else {
                json!({"kind":"runtime_error", "message":other.to_string()})
            }
        }
    };
    let mut value = payload;
    value["domain_disposition"] = json!(disposition.as_str());
    if let Some(named) = named_disposition {
        value["domain_disposition"] = json!(named);
    }
    value
}

fn storage_capability_wire_name(capability: StorageCapability) -> &'static str {
    match capability {
        StorageCapability::Sql => "sql",
        StorageCapability::Notes => "notes",
        StorageCapability::Entities => "entities",
        StorageCapability::Graph => "graph",
        StorageCapability::Events => "events",
        StorageCapability::Vectors => "vectors",
        StorageCapability::Sparse => "sparse",
        StorageCapability::Text => "text",
        StorageCapability::Blob => "blob",
        StorageCapability::Attachments => "attachments",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuditObligationFailure, DenialAuditOutcome, DenialReceipt};
    use khive_types::{Details, ErrorCode, ErrorDomain, KhiveError};

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

    #[test]
    fn projection_keeps_typed_writer_state_and_capability_spelling() {
        let error = RuntimeError::Storage(khive_storage::StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        });
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
}
