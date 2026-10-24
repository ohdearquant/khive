//! Canonical structured projection of typed runtime failures.
//!
//! Transport nesting limits and removal of a nested operation's domain result
//! belong to the transport boundary, not to this lossless shared projection.

use khive_storage::{StorageCapability, StorageError};
use serde_json::{json, Value};

use crate::{DomainDisposition, RuntimeError};

/// Project the original typed error, preserving every structured source field.
///
/// `disposition` describes the enclosing operation. Named runtime refusals keep
/// their existing wire override. This function moves an obligation's result
/// without cloning or recursively serializing it; callers apply their own depth
/// limits before any recursive operation on the returned value.
pub fn runtime_error_value(error: RuntimeError, disposition: DomainDisposition) -> Value {
    let disposition = if disposition == DomainDisposition::Committed {
        disposition
    } else {
        error.batch_write_disposition().unwrap_or(disposition)
    };
    runtime_error_value_inner(error, disposition)
}

// Choose operation-local proof once. Recursive projection must not promote an
// inner batch proof past an enclosing context with earlier or unknown effects.
fn runtime_error_value_inner(error: RuntimeError, disposition: DomainDisposition) -> Value {
    // These named outcomes carry their own domain proof. Do not infer general
    // write disposition from a conflict or unavailable variant.
    let sqlite_write_failure = match error.refusal_source() {
        RuntimeError::Storage(error) => error.sqlite_write_failure(),
        _ => None,
    };
    let outcome_has_precedence = error.writer_task_failure_context().is_some_and(|context| {
        context.task_terminated
            || context.request_state == khive_storage::WriterTaskRequestState::SideEffectsUnknown
    });
    let receipt_projection = crate::visibility_receipts::receipt_error_projection(&error);
    let named_disposition = match &error {
        _ if receipt_projection.is_some_and(|(_, replay)| replay) => Some("not_committed"),
        // An immutable-stream policy refusal is a definite no-write. It is matched by the
        // shared predicate rather than by a reason string so the two consumers of that
        // predicate and this projection cannot drift into disagreeing about it.
        error if error.is_stream_policy_refusal() => Some("not_committed"),
        // A refusal that wrote a receipt is a definite no-write of the thing it
        // refused: the receipt exists precisely to record that nothing ran.
        RuntimeError::RefusedWithReceipt(_) => Some("not_committed"),
        RuntimeError::Khive(k) => match (k.kind(), k.details().and_then(|d| d.get("reason"))) {
            (
                khive_types::ErrorKind::Internal,
                Some("post_commit_degraded" | "embedding_input_truncated"),
            ) => Some("committed"),
            // These keyed conflicts certify that this request wrote no domain record.
            (
                khive_types::ErrorKind::Conflict,
                Some("key_conflict" | "fence_conflict" | "idempotency_key_conflict"),
            ) => Some("not_committed"),
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
            // An exact keyed replay whose holder was hard-deleted before its
            // provenance read: the attempt's unit rolled back on the key claim.
            (khive_types::ErrorKind::NotFound, Some("keyed_replay_holder_missing")) => {
                Some("not_committed")
            }
            (
                khive_types::ErrorKind::InvalidInput,
                Some("member_unavailable" | "sequence_exhausted"),
            ) => Some("not_committed"),
            _ => None,
        },
        _ => None,
    };
    // The refusal text is the same Display string every consumer already
    // matches on; the receipt fields ride beside it.
    let denial_message =
        matches!(error, RuntimeError::PermissionDenied { .. }).then(|| error.to_string());
    let payload = match error {
        RuntimeError::CanonicalUnsupportedOperation {
            op_index,
            operation,
        } => {
            return json!({
                "kind": "runtime_error",
                "code": "invalid_input",
                "reason": "KnownUnimplemented",
                "operation": operation,
                "op_index": op_index,
                "message": format!("canonical operation {op_index}: {operation} is KnownUnimplemented for canonical apply"),
                "domain_disposition": "not_committed",
                "retryable": false,
            });
        }
        RuntimeError::CanonicalAdmission { op_index, source } => {
            // Preparation precedes all domain DML. Keep the original typed
            // refusal (including secret detector/permission receipt) and add
            // only the constituent's index; never stringify captured input.
            let mut value = runtime_error_value_inner(*source, DomainDisposition::NotCommitted);
            value["op_index"] = json!(op_index);
            value["domain_disposition"] = json!("not_committed");
            value["retryable"] = json!(false);
            return value;
        }
        RuntimeError::LintFix { source, detail } => {
            let mut value = runtime_error_value_inner(*source, DomainDisposition::Unknown);
            let object = value
                .as_object_mut()
                .expect("runtime error projection is an object");
            let details = object.entry("details").or_insert_with(|| json!({}));
            if !details.is_object() {
                *details = json!({});
            }
            details["lint_fix"] = json!(detail);
            // Earlier actions may already have committed. A named refusal of
            // the last action cannot certify the whole call as not committed.
            value["domain_disposition"] = json!("unknown");
            value["retryable"] = json!(false);
            return value;
        }

        RuntimeError::WithResolution { context } => {
            let crate::error::ResolutionFailureContext { source, resolution } = context;
            let mut value = runtime_error_value_inner(*source, disposition);
            let details = value
                .as_object_mut()
                .expect("runtime error projection is an object")
                .entry("details")
                .or_insert_with(|| json!({}));
            if !details.is_object() {
                *details = json!({});
            }
            details["resolution"] = json!(resolution);
            return value;
        }
        RuntimeError::BatchWrite { context } => {
            let crate::error::BatchWriteContext {
                source, admission, ..
            } = *context;
            let mut value = runtime_error_value_inner(*source, disposition);
            if let Some(report) = admission {
                if report.validation_complete {
                    if let Some(first) = report.refused.first() {
                        if !value["details"].is_object() {
                            value["details"] = json!({});
                        }
                        value["details"]["index"] = json!(first.index.to_string());
                        value["details"]["field"] = json!(first.field);
                        value["details"]["reason"] = json!(first.reason);
                    }
                } else {
                    value["unchecked_indices"] = json!(report.unchecked_indices);
                }
                value["refused"] = json!(report.refused);
                value["validation_complete"] = json!(report.validation_complete);
            }
            value["domain_disposition"] = json!(disposition.as_str());
            return value;
        }
        RuntimeError::RefusedWithEvents { context } => {
            let crate::error::RefusalEventContext { source, recordings } = context;
            // Project the source first so named dispositions, code/details,
            // detector location and refusal text retain their original meaning.
            let mut value = runtime_error_value_inner(*source, disposition);
            if !recordings.is_empty() {
                value["refusal_recorded"] = json!(recordings.iter().all(|recording| {
                    matches!(recording, crate::RefusalEventRecording::Recorded { .. })
                }));
                value["refusal_events"] = json!(recordings);
            }
            return value;
        }
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
        RuntimeError::SecretDetected(matched) => {
            let message = RuntimeError::SecretDetected(matched.clone()).to_string();
            json!({
                "kind": "runtime_error",
                "code": "secret_detected",
                "detector": matched.detector,
                "location": matched.location,
                "message": message,
            })
        }
        RuntimeError::RefusedWithReceipt(refusal) => {
            let crate::error::ReceiptRefusal {
                code,
                message,
                receipt_id,
                reason,
                detail,
            } = *refusal;
            // Surface evidence first, contract fields last: a detail member that
            // happens to share a name with a contract field cannot shadow it.
            let mut error = match detail {
                Value::Object(members) => members,
                _ => serde_json::Map::new(),
            };
            error.insert("kind".into(), json!("runtime_error"));
            error.insert("code".into(), json!(code));
            error.insert("message".into(), json!(message));
            error.insert("receipt_id".into(), json!(receipt_id));
            error.insert("reason".into(), json!(reason));
            Value::Object(error)
        }
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
        RuntimeError::Khive(k) => {
            let mut value = serde_json::to_value(&k)
                .unwrap_or_else(|_| json!({"kind": "internal", "message": k.to_string()}));
            if k.kind() == khive_types::ErrorKind::InvalidInput
                && k.details().and_then(|d| d.get("reason")) == Some("external_id_unverifiable")
            {
                value["code"] = json!("external_id_unverifiable");
            }
            value
        }
        RuntimeError::RemoteFetchError { remote, message } => json!({
            "kind": "remote_fetch_error",
            "remote": remote,
            "message": message,
        }),
        missing @ RuntimeError::NotFound(_) => json!({
            "kind": "not_found",
            "code": null,
            "details": null,
            "message": missing.to_string(),
        }),
        other @ (RuntimeError::Storage(_)
        | RuntimeError::Query(_)
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
        | RuntimeError::DeadlineExceeded { .. }) => {
            if let Some(capacity) = sqlite_capacity_failure(&other) {
                let mut value = capacity.into_value(other.to_string());
                if let Some(context) = other.writer_task_failure_context() {
                    value["request_state"] = json!(context.request_state.to_string());
                    value["task_terminated"] = json!(context.task_terminated);
                }
                value
            } else if let Some(context) = other.writer_task_failure_context() {
                json!({"kind":"storage", "code":context.stage, "stage":context.stage,
                    "message":other.to_string(), "retryable":context.retryable,
                    "request_state":context.request_state.to_string(), "task_terminated":context.task_terminated})
            } else if let Some(context) = other.retryable_failure_context() {
                let timeout_ms = u64::try_from(context.timeout.as_millis()).unwrap_or(u64::MAX);
                let mut value = json!({"kind":"unavailable", "code":context.stage, "stage":context.stage,
                    "message":other.to_string(), "retryable":true, "timeout_ms":timeout_ms,
                    "capability":context.capability.map(storage_capability_wire_name),
                    "operation":context.operation, "scope":context.scope, "retry_after_ms":context.retry_after_ms});
                if let Some(pool_identity) = context.pool_identity {
                    value["pool_identity"] = json!(pool_identity);
                }
                value
            } else {
                json!({"kind":"runtime_error", "message":other.to_string()})
            }
        }
    };
    let mut value = payload;
    if let Some(failure) = sqlite_write_failure {
        // Capacity refusal retains its existing caller-visible precedence.
        if !outcome_has_precedence
            && !failure.settlement_unknown
            && failure.primary_code != rusqlite::ffi::SQLITE_FULL
        {
            value["code"] = json!(failure.stage.as_str());
            value["stage"] = json!(failure.stage.as_str());
            if value["kind"] == "runtime_error" {
                value["kind"] = json!("storage");
            }
        }
        value["sqlite_write_stage"] = json!(failure.stage.as_str());
        value["sqlite_primary_code"] = json!(failure.primary_code);
        value["sqlite_extended_code"] = json!(failure.extended_code);
    }
    if let Some((retryable, _)) = receipt_projection {
        value["retryable"] = json!(retryable);
    }
    value["domain_disposition"] = json!(disposition.as_str());
    if let Some(named) = named_disposition {
        value["domain_disposition"] = json!(named);
    }
    value
}

enum SqliteCapacityFailure {
    Refused {
        volume: String,
        available_bytes: u64,
        reserve_bytes: u64,
        required_headroom_bytes: u64,
    },
    Unavailable {
        phase: khive_storage::CapacityUnavailablePhase,
    },
    NativeFull {
        primary_code: i32,
        extended_code: i32,
    },
}

impl SqliteCapacityFailure {
    fn into_value(self, message: String) -> Value {
        match self {
            Self::Refused {
                volume,
                available_bytes,
                reserve_bytes,
                required_headroom_bytes,
            } => json!({
                "kind": "storage",
                "code": "sqlite_capacity_refused",
                "stage": "sqlite_capacity_refused",
                "message": message,
                "retryable": false,
                "capability": "sql",
                "volume": volume,
                "available_bytes": available_bytes,
                "reserve_bytes": reserve_bytes,
                "required_headroom_bytes": required_headroom_bytes,
            }),
            Self::Unavailable { phase } => json!({
                "kind": "storage",
                "code": "sqlite_capacity_unavailable",
                "stage": "sqlite_capacity_unavailable",
                "message": message,
                "retryable": phase == khive_storage::CapacityUnavailablePhase::LockContended,
                "capability": "sql",
                "phase": phase.as_str(),
            }),
            Self::NativeFull {
                primary_code,
                extended_code,
            } => json!({
                "kind": "storage",
                "code": "sqlite_disk_full",
                "stage": "sqlite_disk_full",
                "message": message,
                "retryable": false,
                "capability": "sql",
                "sqlite_primary_code": primary_code,
                "sqlite_extended_code": extended_code,
            }),
        }
    }
}

fn sqlite_capacity_failure(error: &RuntimeError) -> Option<SqliteCapacityFailure> {
    match error {
        RuntimeError::Storage(storage) => sqlite_capacity_failure_from_storage(storage),
        _ => None,
    }
}

fn sqlite_capacity_failure_from_storage(error: &StorageError) -> Option<SqliteCapacityFailure> {
    match error {
        StorageError::CapacityFloor {
            capability: StorageCapability::Sql,
            volume,
            available_bytes,
            floor_bytes,
            required_headroom_bytes,
        } => Some(SqliteCapacityFailure::Refused {
            volume: volume.clone(),
            available_bytes: *available_bytes,
            reserve_bytes: *floor_bytes,
            required_headroom_bytes: *required_headroom_bytes,
        }),
        StorageError::CapacityUnavailable {
            capability: StorageCapability::Sql,
            phase,
            ..
        } => Some(SqliteCapacityFailure::Unavailable { phase: *phase }),
        StorageError::WriterTaskTerminated {
            sqlite_full_codes: Some((primary_code, extended_code)),
            ..
        } if *primary_code == rusqlite::ffi::SQLITE_FULL
            && *extended_code >= 0
            && (*extended_code & 0xff) == *primary_code =>
        {
            Some(SqliteCapacityFailure::NativeFull {
                primary_code: *primary_code,
                extended_code: *extended_code,
            })
        }
        StorageError::SqliteWrite { failure, .. }
            if failure.primary_code == rusqlite::ffi::SQLITE_FULL =>
        {
            Some(SqliteCapacityFailure::NativeFull {
                primary_code: failure.primary_code,
                extended_code: failure.extended_code,
            })
        }
        StorageError::WriterTaskRequestFailed { source, .. }
        | StorageError::SqliteWrite { source, .. } => sqlite_capacity_failure_from_storage(source),
        StorageError::Driver { source, .. } => {
            if let Some(sqlite) = source.downcast_ref::<khive_db::SqliteError>() {
                sqlite_capacity_failure_from_sqlite(sqlite)
            } else {
                native_sqlite_full(source.as_ref())
            }
        }
        _ => None,
    }
}

fn sqlite_capacity_failure_from_sqlite(
    error: &khive_db::SqliteError,
) -> Option<SqliteCapacityFailure> {
    match error {
        khive_db::SqliteError::CapacityFloor {
            volume,
            available_bytes,
            floor_bytes,
            required_headroom_bytes,
        } => Some(SqliteCapacityFailure::Refused {
            volume: volume.clone(),
            available_bytes: *available_bytes,
            reserve_bytes: *floor_bytes,
            required_headroom_bytes: *required_headroom_bytes,
        }),
        khive_db::SqliteError::CapacityUnavailable { phase, .. } => {
            Some(SqliteCapacityFailure::Unavailable { phase: *phase })
        }
        khive_db::SqliteError::Rusqlite(sqlite)
        | khive_db::SqliteError::Write { source: sqlite, .. } => native_sqlite_full(sqlite),
        _ => None,
    }
}

fn native_sqlite_full(error: &(dyn std::error::Error + 'static)) -> Option<SqliteCapacityFailure> {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            current.downcast_ref::<rusqlite::Error>()
        {
            if code.code == rusqlite::ErrorCode::DiskFull {
                return Some(SqliteCapacityFailure::NativeFull {
                    primary_code: code.extended_code & 0xff,
                    extended_code: code.extended_code,
                });
            }
        }
        source = current.source();
    }
    None
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
#[path = "error_projection_tests.rs"]
mod tests;
