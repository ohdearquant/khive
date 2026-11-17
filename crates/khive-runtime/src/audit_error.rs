//! Closed, content-free classes on failed dispatch audit rows.
//!
//! This classification does not change retryability or domain disposition.
//! Registry gate denials use `GateRefusal`; a handler's `PermissionDenied`
//! uses `PolicyRefusal`. Historical rows without a class remain unclassified.

use crate::error::{AuditObligationReason, RuntimeError};
use khive_storage::StorageError;

/// Stable additive audit vocabulary; unknown stored tokens are never internal errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuditErrorClass {
    PolicyRefusal,
    GateRefusal,
    InvalidParams,
    NotFoundOrNotAddressed,
    DeadlineExceeded,
    AdmissionExhausted,
    Conflict,
    Unsupported,
    Configuration,
    Storage,
    Unavailable,
    Internal,
}

impl AuditErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PolicyRefusal => "policy_refusal",
            Self::GateRefusal => "gate_refusal",
            Self::InvalidParams => "invalid_params",
            Self::NotFoundOrNotAddressed => "not_found_or_not_addressed",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::AdmissionExhausted => "admission_exhausted",
            Self::Conflict => "conflict",
            Self::Unsupported => "unsupported",
            Self::Configuration => "configuration",
            Self::Storage => "storage",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }

    /// Read one exact known token. Absence and future/invalid tokens have no class.
    pub fn from_token(token: &str) -> Option<Self> {
        Some(match token {
            "policy_refusal" => Self::PolicyRefusal,
            "gate_refusal" => Self::GateRefusal,
            "invalid_params" => Self::InvalidParams,
            "not_found_or_not_addressed" => Self::NotFoundOrNotAddressed,
            "deadline_exceeded" => Self::DeadlineExceeded,
            "admission_exhausted" => Self::AdmissionExhausted,
            "conflict" => Self::Conflict,
            "unsupported" => Self::Unsupported,
            "configuration" => Self::Configuration,
            "storage" => Self::Storage,
            "unavailable" => Self::Unavailable,
            "internal" => Self::Internal,
            _ => return None,
        })
    }
}

impl RuntimeError {
    /// Classify an error returned by a handler, without rendering it.
    /// Every runtime variant is named: adding a variant requires a classification.
    pub fn audit_error_class(&self) -> AuditErrorClass {
        use AuditErrorClass as C;
        match self {
            Self::CanonicalAdmission { source, .. } | Self::LintFix { source, .. } => {
                source.audit_error_class()
            }
            Self::RefusedWithEvents { context } => context.source.audit_error_class(),
            Self::BatchWrite { context } => context.source.audit_error_class(),
            Self::WithResolution { context } => context.source.audit_error_class(),
            Self::SearchRead { context } => context.source().audit_error_class(),
            Self::Storage(error) => storage_class(error),
            Self::Query(error) => match error {
                khive_query::QueryError::Parse { .. }
                | khive_query::QueryError::Compile(_)
                | khive_query::QueryError::Validation(_)
                | khive_query::QueryError::InvalidInput(_) => C::InvalidParams,
                khive_query::QueryError::Unsupported(_) => C::Unsupported,
            },
            Self::NotFound(_)
            | Self::NamespaceMismatch { .. }
            | Self::RemoteCacheMissing { .. } => C::NotFoundOrNotAddressed,
            Self::InvalidInput(_)
            | Self::UnknownVerb(_)
            | Self::UnknownModel(_)
            | Self::Ambiguous(_)
            | Self::UnknownFusionStrategy(_)
            | Self::AmbiguousPrefix { .. }
            | Self::AmbiguousId { .. }
            | Self::UnknownRemote { .. } => C::InvalidParams,
            Self::Unconfigured(_)
            | Self::IncompatibleEventStore(_)
            | Self::MissingPackDependency(_)
            | Self::MissingPackDependencies(_)
            | Self::CircularPackDependency(_)
            | Self::PackRedeclared { .. }
            | Self::VerbCollision { .. }
            | Self::ReservedEnvelopeParam { .. }
            | Self::AttachmentCutoverRequired { .. } => C::Configuration,
            Self::CanonicalUnsupportedOperation { .. }
            | Self::CrossBackendMergeUnsupported { .. } => C::Unsupported,
            Self::PermissionDenied { .. }
            | Self::SecretDetected(_)
            | Self::CrossNamespaceWrite { .. }
            | Self::WriteBudgetExceeded { .. }
            | Self::RefusedWithReceipt(_) => C::PolicyRefusal,
            Self::GuardedWriteFailed(_) => C::Conflict,
            Self::DeadlineExceeded { .. } => C::DeadlineExceeded,
            Self::GateUnavailable { .. } | Self::RemoteFetchError { .. } | Self::Embedding(_) => {
                C::Unavailable
            }
            Self::Fusion(_) | Self::Internal(_) => C::Internal,
            Self::Khive(error) => match error.kind() {
                khive_types::ErrorKind::NotFound => C::NotFoundOrNotAddressed,
                khive_types::ErrorKind::InvalidInput => C::InvalidParams,
                khive_types::ErrorKind::Unauthorized => C::PolicyRefusal,
                khive_types::ErrorKind::Conflict => {
                    if self.is_stream_policy_refusal() {
                        C::PolicyRefusal
                    } else {
                        C::Conflict
                    }
                }
                khive_types::ErrorKind::Unavailable => C::Unavailable,
                khive_types::ErrorKind::Internal => C::Internal,
            },
            Self::AuditObligation { failure, .. } => match failure.reason {
                AuditObligationReason::GitDigestReceiptFailure => C::Internal,
                AuditObligationReason::Terminal(reason) => {
                    use crate::audit_batch::AuditTerminalReason as R;
                    match reason {
                        R::PreflightRejected => C::InvalidParams,
                        R::AdmissionClosed | R::QueueAdmissionExhausted => C::AdmissionExhausted,
                        R::AdmissionDeadlineExpired | R::ResolutionDeadlineExpired => {
                            C::DeadlineExceeded
                        }
                        R::IdentityConflict => C::Conflict,
                        R::StoreFailure | R::RetryExhausted | R::StoreWedged => C::Storage,
                        R::IdempotencyUnsupported => C::Unsupported,
                        R::DriverPanicked
                        | R::DriverCancelled
                        | R::DriverJoinLost
                        | R::DriverExitedInconsistent
                        | R::DriverAppendAbandoned => C::Internal,
                    }
                }
            },
        }
    }
}

fn storage_class(error: &StorageError) -> AuditErrorClass {
    use AuditErrorClass as C;
    match error {
        StorageError::SqliteWrite { source, .. }
        | StorageError::WriterTaskRequestFailed { source, .. } => storage_class(source),
        StorageError::NotFound { .. } => C::NotFoundOrNotAddressed,
        StorageError::AlreadyExists { .. } | StorageError::Conflict { .. } => C::Conflict,
        StorageError::InvalidInput { .. } | StorageError::BlobTooLarge { .. } => C::InvalidParams,
        StorageError::Unsupported { .. } => C::Unsupported,
        StorageError::Timeout { .. } | StorageError::ReadTransactionAgeEvicted { .. } => {
            C::DeadlineExceeded
        }
        StorageError::AdmissionTimeout { .. }
        | StorageError::WriteQueueFull { .. }
        | StorageError::WriterTaskBusy { .. } => C::AdmissionExhausted,
        StorageError::CapacityFloor { .. } | StorageError::SecretDetected { .. } => {
            C::PolicyRefusal
        }
        StorageError::WriterTaskNoRuntime => C::Configuration,
        StorageError::Driver { source, .. } => {
            if matches!(
                source.downcast_ref::<khive_db::SqliteError>(),
                Some(khive_db::SqliteError::WriterPoolCheckoutTimeout { .. })
            ) {
                C::AdmissionExhausted
            } else {
                C::Storage
            }
        }
        StorageError::BlobSizeMismatch { .. }
        | StorageError::BlobDigestMismatch { .. }
        | StorageError::Pool { .. }
        | StorageError::Transaction { .. }
        | StorageError::ReadTransactionAgeEvictionCleanupFailed { .. }
        | StorageError::Serialization { .. }
        | StorageError::IndexMaintenance { .. }
        | StorageError::WriterTaskTerminated { .. }
        | StorageError::Internal(_)
        | StorageError::CapacityUnavailable { .. } => C::Storage,
    }
}

#[cfg(test)]
mod tests {
    use super::AuditErrorClass;
    use crate::error::{BatchAdmissionReport, BatchRefusal, DomainDisposition, RuntimeError};
    use khive_storage::{StorageError, WriterTaskRequestState};

    #[test]
    fn incoming_key_secret_is_a_policy_refusal_through_both_storage_routes() {
        let key = format!("ghp_{}", "A".repeat(40));
        for converted in [false, true] {
            let storage =
                khive_storage::note::validate_note_key_for_store(&key, "assign_note_key_if_unset")
                    .expect_err("the incoming key must be refused");
            let runtime = if converted {
                RuntimeError::from(storage)
            } else {
                RuntimeError::Storage(storage)
            };
            assert_eq!(
                runtime.audit_error_class(),
                AuditErrorClass::PolicyRefusal,
                "a typed credential refusal is policy, not storage failure or invalid shape"
            );
        }
    }

    #[test]
    fn batch_write_delegates_audit_class_without_changing_settlement() {
        for (source, expected) in [
            (
                RuntimeError::InvalidInput("fixture input".into()),
                AuditErrorClass::InvalidParams,
            ),
            (
                RuntimeError::NotFound("fixture target".into()),
                AuditErrorClass::NotFoundOrNotAddressed,
            ),
            (
                RuntimeError::Internal("fixture internal".into()),
                AuditErrorClass::Internal,
            ),
        ] {
            assert_eq!(
                source.audit_error_class(),
                expected,
                "unwrapped source control"
            );
            let refusal = source.before_requested_write();
            assert_eq!(
                refusal.audit_error_class(),
                expected,
                "BatchWrite must retain the source audit class"
            );
            assert_eq!(
                refusal.batch_write_disposition(),
                Some(DomainDisposition::NotCommitted)
            );
        }

        let admission = RuntimeError::InvalidInput("fixture admission".into())
            .with_batch_admission(BatchAdmissionReport {
                refused: vec![BatchRefusal {
                    index: 0,
                    field: "content",
                    reason: "fixture-refusal",
                }],
                validation_complete: true,
                unchecked_indices: vec![],
            });
        assert_eq!(
            admission.audit_error_class(),
            AuditErrorClass::InvalidParams
        );
        assert_eq!(
            admission.batch_write_disposition(),
            Some(DomainDisposition::NotCommitted)
        );

        for (state, expected) in [
            (
                WriterTaskRequestState::NotStarted,
                DomainDisposition::NotCommitted,
            ),
            (
                WriterTaskRequestState::TransactionRolledBack,
                DomainDisposition::NotCommitted,
            ),
            (
                WriterTaskRequestState::SideEffectsUnknown,
                DomainDisposition::Unknown,
            ),
        ] {
            let failure =
                RuntimeError::atomic_write_failure(StorageError::WriterTaskRequestFailed {
                    request_state: state,
                    source: Box::new(StorageError::Internal("fixture write cause".into())),
                });
            assert_eq!(failure.audit_error_class(), AuditErrorClass::Storage);
            assert_eq!(failure.batch_write_disposition(), Some(expected));
        }

        for (state, expected) in [
            (
                WriterTaskRequestState::NotStarted,
                DomainDisposition::NotCommitted,
            ),
            (
                WriterTaskRequestState::TransactionRolledBack,
                DomainDisposition::NotCommitted,
            ),
            (
                WriterTaskRequestState::SideEffectsUnknown,
                DomainDisposition::Unknown,
            ),
        ] {
            let failure =
                RuntimeError::atomic_write_failure(StorageError::writer_task_terminated(state));
            assert_eq!(failure.audit_error_class(), AuditErrorClass::Storage);
            assert_eq!(failure.batch_write_disposition(), Some(expected));
        }
    }
    fn search_context(source: RuntimeError) -> RuntimeError {
        source.with_search_read_context(
            crate::BackendId::parse("archive").unwrap(),
            crate::error::SearchReadSubstrate::Note,
        )
    }

    #[test]
    fn search_read_preserves_projected_kinds_and_typed_classification() {
        use khive_types::KhiveError;
        type Factory = fn() -> RuntimeError;
        let cases: &[(Factory, &str)] = &[
            (
                || RuntimeError::RemoteFetchError {
                    remote: "private remote".into(),
                    message: "private message".into(),
                },
                "remote_fetch_error",
            ),
            (
                || RuntimeError::AuditObligation {
                    failure: Box::new(crate::error::AuditObligationFailure::new(
                        "search",
                        crate::audit_batch::AuditTerminalReason::StoreFailure,
                    )),
                    domain_result: serde_json::json!({"private": "ordinary domain canary"}),
                },
                "obligation",
            ),
            (
                || {
                    StorageError::writer_task_terminated(WriterTaskRequestState::SideEffectsUnknown)
                        .into()
                },
                "storage",
            ),
            (
                || {
                    StorageError::AdmissionTimeout {
                        operation: "metadata".into(),
                        timeout_ms: 7,
                        pool_identity: Some("private pool".into()),
                    }
                    .into()
                },
                "unavailable",
            ),
            (
                || {
                    StorageError::Timeout {
                        operation: "metadata".into(),
                    }
                    .into()
                },
                "runtime_error",
            ),
            (
                || RuntimeError::permission_denied("search", "private policy"),
                "runtime_error",
            ),
            (
                || KhiveError::not_found("record", "private id").into(),
                "not_found",
            ),
            (
                || KhiveError::invalid_input("private input").into(),
                "invalid_input",
            ),
            (
                || KhiveError::unauthorized("private identity").into(),
                "unauthorized",
            ),
            (
                || KhiveError::conflict("private conflict").into(),
                "conflict",
            ),
            (
                || KhiveError::unavailable("private availability").into(),
                "unavailable",
            ),
            (
                || KhiveError::internal("private internal").into(),
                "internal",
            ),
            (
                || RuntimeError::Internal("private internal".into()),
                "runtime_error",
            ),
            (
                || RuntimeError::CanonicalAdmission {
                    op_index: 3,
                    source: Box::new(KhiveError::not_found("record", "private id").into()),
                },
                "not_found",
            ),
        ];
        for &(factory, expected_kind) in cases {
            let source = factory();
            let audit_class = source.audit_error_class();
            let retry = source.retryable_failure_context();
            let channel_class = source.channel_ingest_failure_class();
            let raw =
                crate::error_projection::runtime_error_value(source, DomainDisposition::Unknown);
            assert_eq!(raw["kind"], expected_kind, "raw kind discriminator");
            let wrapped = search_context(factory());
            assert_eq!(wrapped.audit_error_class(), audit_class);
            assert_eq!(wrapped.retryable_failure_context(), retry);
            assert_eq!(wrapped.channel_ingest_failure_class(), channel_class);
            let value =
                crate::error_projection::runtime_error_value(wrapped, DomainDisposition::Unknown);
            assert_eq!(value["kind"], raw["kind"]);
            assert_eq!(value["domain_disposition"], raw["domain_disposition"]);
            let mut expected = serde_json::json!({
                "kind": expected_kind,
                "code": "search_read_failed",
                "message": "search metadata read failed on backend archive (note)",
                "cause_class": audit_class.as_str(),
                "domain_disposition": raw["domain_disposition"],
                "search_context": {"backend": "archive", "substrate": "note"},
            });
            if retry.is_some() {
                expected["retryable"] = serde_json::json!(true);
            }
            assert_eq!(value, expected);
        }
        let wrapped = search_context(
            RuntimeError::InvalidInput("private input".into()).before_requested_write(),
        );
        assert_eq!(
            wrapped.batch_write_disposition(),
            Some(DomainDisposition::NotCommitted)
        );
        assert!(
            matches!(wrapped.refusal_source(), RuntimeError::InvalidInput(text) if text == "private input")
        );
    }

    #[test]
    fn search_read_hides_raw_content_and_untrusted_retry_fields() {
        fn receipt() -> RuntimeError {
            RuntimeError::RefusedWithReceipt(Box::new(crate::error::ReceiptRefusal {
                code: "ordinary-code-canary",
                message: "ordinary-message-canary".into(),
                receipt_id: "ordinary-receipt-canary".into(),
                reason: "ordinary-reason-canary".into(),
                detail: serde_json::json!({"detail": "ordinary-detail-canary", "kind": "spoofed-kind", "domain_disposition": "committed", "retryable": {"ordinary": "ordinary-retry-canary"}}),
            }))
        }
        let raw =
            crate::error_projection::runtime_error_value(receipt(), DomainDisposition::Unknown);
        assert_eq!(raw["kind"], "runtime_error");
        assert_eq!(raw["domain_disposition"], "not_committed");
        let raw_text = raw.to_string();
        let wrapped = search_context(receipt());
        let RuntimeError::SearchRead { context } = &wrapped else {
            panic!("typed context missing")
        };
        let RuntimeError::RefusedWithReceipt(source) = context.source() else {
            panic!("original source missing")
        };
        assert_eq!(source.code, "ordinary-code-canary");
        assert_eq!(source.message, "ordinary-message-canary");
        assert_eq!(source.receipt_id, "ordinary-receipt-canary");
        assert_eq!(source.reason, "ordinary-reason-canary");
        assert_eq!(source.detail["detail"], "ordinary-detail-canary");
        assert_eq!(
            source.detail["retryable"]["ordinary"],
            "ordinary-retry-canary"
        );
        let display = wrapped.to_string();
        let value =
            crate::error_projection::runtime_error_value(wrapped, DomainDisposition::Unknown);
        for canary in [
            "ordinary-code-canary",
            "ordinary-message-canary",
            "ordinary-receipt-canary",
            "ordinary-reason-canary",
            "ordinary-detail-canary",
            "ordinary-retry-canary",
        ] {
            assert!(
                raw_text.contains(canary),
                "raw projection must expose this witness"
            );
            assert!(!display.contains(canary));
            assert!(!value.to_string().contains(canary));
        }
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "runtime_error", "code": "search_read_failed",
                "message": "search metadata read failed on backend archive (note)",
                "cause_class": "policy_refusal", "domain_disposition": "not_committed",
                "search_context": {"backend": "archive", "substrate": "note"},
            })
        );
        fn detail() -> RuntimeError {
            khive_types::KhiveError::invalid_input("ordinary-khive-message")
                .with_details(khive_types::Details::new([(
                    "private",
                    "ordinary-khive-detail",
                )]))
                .into()
        }
        fn driver() -> RuntimeError {
            StorageError::driver(
                khive_storage::StorageCapability::Sql,
                "ordinary-driver-operation",
                std::io::Error::other("ordinary-driver-body"),
            )
            .into()
        }
        for (factory, canaries) in [
            (
                detail as fn() -> RuntimeError,
                ["ordinary-khive-message", "ordinary-khive-detail"],
            ),
            (
                driver as fn() -> RuntimeError,
                ["ordinary-driver-operation", "ordinary-driver-body"],
            ),
        ] {
            let raw =
                crate::error_projection::runtime_error_value(factory(), DomainDisposition::Unknown)
                    .to_string();
            let wrapped = search_context(factory());
            let display = wrapped.to_string();
            assert_eq!(wrapped.refusal_source().to_string(), factory().to_string());
            match wrapped.refusal_source() {
                RuntimeError::Khive(source) => assert_eq!(
                    source.details().unwrap().get("private"),
                    Some("ordinary-khive-detail")
                ),
                RuntimeError::Storage(StorageError::Driver {
                    operation, source, ..
                }) => {
                    assert_eq!(operation.as_ref(), "ordinary-driver-operation");
                    assert_eq!(
                        source.downcast_ref::<std::io::Error>().unwrap().to_string(),
                        "ordinary-driver-body"
                    );
                }
                _ => panic!("private typed source changed"),
            }
            let safe =
                crate::error_projection::runtime_error_value(wrapped, DomainDisposition::Unknown)
                    .to_string();
            for canary in canaries {
                assert!(raw.contains(canary));
                assert!(!display.contains(canary));
                assert!(!safe.contains(canary));
            }
        }
    }
    #[test]
    fn search_read_preserves_nested_lint_retry_refusal() {
        fn retryable() -> RuntimeError {
            StorageError::AdmissionTimeout {
                operation: "metadata".into(),
                timeout_ms: 7,
                pool_identity: None,
            }
            .into()
        }
        fn lint(source: RuntimeError) -> RuntimeError {
            use crate::graph_lint::{
                ActionIdentity, FixAction, LintFixFailureDetail, LintFixOutcome, LintReport,
            };
            RuntimeError::LintFix {
                source: Box::new(source),
                detail: Box::new(LintFixFailureDetail {
                    outcome: LintFixOutcome::Unknown,
                    failed_action: ActionIdentity {
                        violation_index: 0,
                        action: FixAction::SoftDeleteEdge,
                    },
                    confirmed_receipts: vec![],
                    report: LintReport {
                        violations: vec![],
                        summary: Default::default(),
                        stats: Default::default(),
                    },
                }),
            }
        }
        assert!(retryable().retryable_failure_context().is_some());
        for error in [
            search_context(lint(retryable())),
            lint(search_context(retryable())),
        ] {
            assert!(error.retryable_failure_context().is_none());
            assert!(matches!(
                error.refusal_source(),
                RuntimeError::Storage(StorageError::AdmissionTimeout { .. })
            ));
            assert_eq!(
                error.audit_error_class(),
                AuditErrorClass::AdmissionExhausted
            );
        }
        let value = crate::error_projection::runtime_error_value(
            search_context(lint(retryable().before_requested_write())),
            DomainDisposition::Unknown,
        );
        assert_eq!(value["kind"], "unavailable");
        assert_eq!(value["domain_disposition"], "unknown");
        assert!(value.get("retryable").is_none());
    }
}
