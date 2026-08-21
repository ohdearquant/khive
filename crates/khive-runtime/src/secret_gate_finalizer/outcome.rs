//! The five typed outcomes of finalization (ADR-115 Amendment 1 §4;
//! ADR-115 Amendment 1 §4).
//!
//! `FailureDiagnostic` and `FinalizerAuditGap` (in `super::log_sink`) are
//! deliberately narrow: neither carries submitted content, properties, tags,
//! digest, manifest entry, detector excerpt, or raw storage error, per the
//! contract's second-order sink requirement.

use uuid::Uuid;

/// Which substrate the finalizing candidate belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Substrate {
    Entity,
    Note,
}

/// Which finalization step failed, for the `FailureDiagnostic` it produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureClass {
    RecordWrite,
    Stamp,
    SuccessAudit,
}

/// A successfully committed exemption: record, runtime stamp, synchronous
/// index/edge state, and one success event landed atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExemptionCommit {
    pub(crate) record_id: Uuid,
    pub(crate) substrate: Substrate,
    pub(crate) entry_point: &'static str,
    pub(crate) digest_sha256: String,
    pub(crate) field_scope: &'static str,
    pub(crate) manifest_id: String,
}

/// The four §5 freshness/parse faults plus the other fail-closed parser
/// faults, kept distinguishable as causes under `ManifestInvalid`. A normal
/// manifest *miss* is never represented here — it uses the unchanged
/// scanner and never reaches this finalizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestFault {
    UnknownSchemaVersion,
    MissingExpectedCorpusIdentity,
    CorpusIdentityMismatch,
    RefreshFailure,
    AbsentOrUnreadable,
    Malformed,
    DuplicateConflict,
    UnsupportedAlgorithm,
    TruncatedDigest,
    MultipleMatches,
}

/// Redacted, structured diagnostic for a failed finalization step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FailureDiagnostic {
    pub(crate) diagnostic_id: Uuid,
    pub(crate) record_id: Uuid,
    pub(crate) substrate: Substrate,
    pub(crate) namespace: String,
    pub(crate) entry_point: &'static str,
    pub(crate) failure_class: FailureClass,
}

impl FailureDiagnostic {
    pub(crate) fn new(
        record_id: Uuid,
        substrate: Substrate,
        namespace: &str,
        entry_point: &'static str,
        failure_class: FailureClass,
    ) -> Self {
        Self {
            diagnostic_id: Uuid::new_v4(),
            record_id,
            substrate,
            namespace: namespace.to_string(),
            entry_point,
            failure_class,
        }
    }
}

/// The five internal outcomes. Never re-exported past the crate boundary
/// (ADR-115 Amendment 1 §4): callers
/// crossing the crate boundary see only the existing `RuntimeResult<T>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FinalizerOutcome {
    Exempted(ExemptionCommit),
    ManifestInvalid(ManifestFault),
    AuditFailed(FailureDiagnostic),
    StampFailed(FailureDiagnostic),
    RecordWriteFailed(FailureDiagnostic),
}
