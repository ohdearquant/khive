//! Store-independent audit-gap sink for the second-order failure: the
//! failure-audit write itself failing after a primary finalization failure
//! already rolled back (ADR-115 Amendment 1 §4/§5). This sink must not
//! depend on the primary storage backend that just failed, so the
//! second-order case still leaves exactly one durable trace.

use uuid::Uuid;

use super::outcome::{FailureClass, Substrate};

/// One independent audit-gap record. Deliberately narrow: no submitted
/// content, properties, tags, digest, manifest entry, detector excerpt, or
/// raw storage error travels through this sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalizerAuditGap {
    pub(crate) failure_class: FailureClass,
    pub(crate) diagnostic_id: Uuid,
    pub(crate) record_id: Uuid,
    pub(crate) substrate: Substrate,
    pub(crate) namespace: String,
    pub(crate) entry_point: &'static str,
}

/// A destination for `FinalizerAuditGap` records that does not depend on
/// the store that just failed to persist the failure diagnostic.
pub(crate) trait LogSink: Send + Sync {
    fn record_audit_gap(&self, gap: &FinalizerAuditGap);
}

/// Production sink: structured `tracing::error!`, which never depends on
/// the SQLite backend the finalizer just failed to write to.
pub(crate) struct TracingLogSink;

impl LogSink for TracingLogSink {
    fn record_audit_gap(&self, gap: &FinalizerAuditGap) {
        tracing::error!(
            failure_class = ?gap.failure_class,
            diagnostic_id = %gap.diagnostic_id,
            record_id = %gap.record_id,
            substrate = ?gap.substrate,
            namespace = %gap.namespace,
            entry_point = gap.entry_point,
            "secret_gate_finalizer: audit-gap (failure-audit write itself failed)"
        );
    }
}

/// Test/harness sink that captures gaps in memory for assertion instead of
/// emitting through `tracing`.
#[cfg(test)]
pub(crate) struct CapturingLogSink {
    pub(crate) gaps: std::sync::Mutex<Vec<FinalizerAuditGap>>,
}

#[cfg(test)]
impl CapturingLogSink {
    pub(crate) fn new() -> Self {
        Self {
            gaps: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<FinalizerAuditGap> {
        self.gaps.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl LogSink for CapturingLogSink {
    fn record_audit_gap(&self, gap: &FinalizerAuditGap) {
        self.gaps.lock().unwrap().push(gap.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capturing_sink_survives_and_records_exactly_one_gap() {
        let sink = CapturingLogSink::new();
        let gap = FinalizerAuditGap {
            failure_class: FailureClass::RecordWrite,
            diagnostic_id: Uuid::new_v4(),
            record_id: Uuid::new_v4(),
            substrate: Substrate::Entity,
            namespace: "local".to_string(),
            entry_point: "entity.create",
        };
        sink.record_audit_gap(&gap);
        let captured = sink.snapshot();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0], gap);
    }

    #[test]
    fn tracing_sink_does_not_panic_on_record() {
        let sink = TracingLogSink;
        sink.record_audit_gap(&FinalizerAuditGap {
            failure_class: FailureClass::Stamp,
            diagnostic_id: Uuid::new_v4(),
            record_id: Uuid::new_v4(),
            substrate: Substrate::Note,
            namespace: "local".to_string(),
            entry_point: "note.create",
        });
    }
}
