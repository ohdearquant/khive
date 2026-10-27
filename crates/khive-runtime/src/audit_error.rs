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
            | Self::ReservedEnvelopeParam { .. } => C::Configuration,
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
        StorageError::CapacityFloor { .. } => C::PolicyRefusal,
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
}
