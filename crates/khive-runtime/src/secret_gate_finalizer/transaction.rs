//! Atomic finalization with rollback on every failure path (ADR-115
//! Amendment 1 §4/§5; executable contract §5).
//!
//! `finalize` is deliberately storage-agnostic: it drives a
//! [`FinalizationEffects`] implementation through manifest resolution,
//! record write, stamp write, and success-audit write, rolling back and
//! attempting a failure-audit write on any failure, falling back to the
//! store-independent [`LogSink`] when that failure-audit write itself fails
//! (the second-order case). The real SQLite-backed `FinalizationEffects`
//! wiring against the declared entry points is the runtime-ingress lane's
//! job (executable contract §7); this module is exercised directly by its
//! own tests until that lands.

use uuid::Uuid;

use super::faults;
use super::log_sink::{FinalizerAuditGap, LogSink};
use super::outcome::{
    ExemptionCommit, FailureClass, FailureDiagnostic, FinalizerOutcome, ManifestFault, Substrate,
};

/// The runtime-recomputed manifest match a successful lookup produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExemptionMatch {
    pub(crate) digest_sha256: String,
    pub(crate) field_scope: &'static str,
    pub(crate) manifest_id: String,
}

/// Storage effects the finalizer drives through one atomic finalization
/// attempt. Implementors decide how manifest resolution, the three
/// transactional writes, and the post-rollback failure-audit write map onto
/// a real backend; `finalize` only decides *when* each is called and what
/// happens when one fails.
pub(crate) trait FinalizationEffects {
    /// Resolve the manifest match for the already-scanned candidate. Called
    /// before any transactional state is touched; a failure here never
    /// requires rollback.
    fn resolve_manifest(&mut self) -> Result<ExemptionMatch, ManifestFault>;

    /// Persist the candidate record. First transactional write.
    fn write_record(&mut self) -> Result<(), ()>;

    /// Persist the runtime exemption stamp plus synchronous index/edge
    /// state. Second transactional write; only called if `write_record`
    /// succeeded.
    fn write_stamp(&mut self) -> Result<(), ()>;

    /// Persist the single success audit event. Third transactional write;
    /// only called if `write_stamp` succeeded.
    fn write_success_audit(&mut self) -> Result<(), ()>;

    /// Roll back every transactional write attempted so far. Called on any
    /// of the three write failures above, before the failure-audit attempt.
    fn rollback(&mut self);

    /// Attempt to persist a diagnostic for the failure that triggered
    /// `rollback`. May itself fail (the second-order case), in which case
    /// `finalize` reports through the [`LogSink`] instead.
    fn write_failure_audit(&mut self, diagnostic: &FailureDiagnostic) -> Result<(), ()>;
}

/// The already-scanned candidate this finalization attempt is for.
#[derive(Debug, Clone)]
pub(crate) struct FinalizationInput {
    pub(crate) record_id: Uuid,
    pub(crate) substrate: Substrate,
    pub(crate) namespace: String,
    pub(crate) entry_point: &'static str,
}

/// Drive one finalization attempt to exactly one of the five outcomes.
///
/// Every step first checks this namespace's one-shot fault seam (see
/// [`super::faults`]) before invoking the real effect, so tests get
/// deterministic control over which step fails without needing a live
/// backend that can actually fail on demand.
pub(crate) fn finalize<E: FinalizationEffects>(
    effects: &mut E,
    sink: &dyn LogSink,
    input: &FinalizationInput,
) -> FinalizerOutcome {
    let ns = input.namespace.as_str();

    let manifest_match = match faults::consume_manifest_invalid(ns) {
        Some(fault) => return FinalizerOutcome::ManifestInvalid(fault),
        None => match effects.resolve_manifest() {
            Ok(matched) => matched,
            Err(fault) => return FinalizerOutcome::ManifestInvalid(fault),
        },
    };

    let record_write_failed =
        faults::consume_record_write_fail(ns) || effects.write_record().is_err();
    if record_write_failed {
        return fail(effects, sink, input, FailureClass::RecordWrite);
    }

    let stamp_write_failed = faults::consume_stamp_fail(ns) || effects.write_stamp().is_err();
    if stamp_write_failed {
        return fail(effects, sink, input, FailureClass::Stamp);
    }

    let success_audit_failed =
        faults::consume_success_audit_fail(ns) || effects.write_success_audit().is_err();
    if success_audit_failed {
        return fail(effects, sink, input, FailureClass::SuccessAudit);
    }

    FinalizerOutcome::Exempted(ExemptionCommit {
        record_id: input.record_id,
        substrate: input.substrate,
        entry_point: input.entry_point,
        digest_sha256: manifest_match.digest_sha256,
        field_scope: manifest_match.field_scope,
        manifest_id: manifest_match.manifest_id,
    })
}

/// Roll back, build the diagnostic, attempt the failure-audit write, and
/// fall back to the log sink on second-order failure. Returns the
/// `FinalizerOutcome` variant matching `class`.
fn fail<E: FinalizationEffects>(
    effects: &mut E,
    sink: &dyn LogSink,
    input: &FinalizationInput,
    class: FailureClass,
) -> FinalizerOutcome {
    effects.rollback();

    let diagnostic = FailureDiagnostic::new(
        input.record_id,
        input.substrate,
        &input.namespace,
        input.entry_point,
        class,
    );

    let failure_audit_failed = faults::consume_failure_audit_fail(&input.namespace)
        || effects.write_failure_audit(&diagnostic).is_err();
    if failure_audit_failed {
        sink.record_audit_gap(&FinalizerAuditGap {
            failure_class: diagnostic.failure_class,
            diagnostic_id: diagnostic.diagnostic_id,
            record_id: diagnostic.record_id,
            substrate: diagnostic.substrate,
            namespace: diagnostic.namespace.clone(),
            entry_point: diagnostic.entry_point,
        });
    }

    match class {
        FailureClass::RecordWrite => FinalizerOutcome::RecordWriteFailed(diagnostic),
        FailureClass::Stamp => FinalizerOutcome::StampFailed(diagnostic),
        FailureClass::SuccessAudit => FinalizerOutcome::AuditFailed(diagnostic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret_gate_finalizer::log_sink::CapturingLogSink;

    /// In-memory fake effects with observable pre/post state, so rollback
    /// correctness is a direct state assertion rather than an inference from
    /// the returned outcome alone.
    #[derive(Default)]
    struct FakeEffects {
        record_written: bool,
        stamp_written: bool,
        audit_written: bool,
        failure_audits: Vec<FailureDiagnostic>,
        resolve_err: Option<ManifestFault>,
    }

    impl FakeEffects {
        fn state(&self) -> (bool, bool, bool) {
            (self.record_written, self.stamp_written, self.audit_written)
        }
    }

    impl FinalizationEffects for FakeEffects {
        fn resolve_manifest(&mut self) -> Result<ExemptionMatch, ManifestFault> {
            match self.resolve_err {
                Some(fault) => Err(fault),
                None => Ok(ExemptionMatch {
                    digest_sha256: "deadbeef".to_string(),
                    field_scope: "record-content",
                    manifest_id: "khive-secret-gate-empty-v1".to_string(),
                }),
            }
        }

        fn write_record(&mut self) -> Result<(), ()> {
            self.record_written = true;
            Ok(())
        }

        fn write_stamp(&mut self) -> Result<(), ()> {
            self.stamp_written = true;
            Ok(())
        }

        fn write_success_audit(&mut self) -> Result<(), ()> {
            self.audit_written = true;
            Ok(())
        }

        fn rollback(&mut self) {
            self.record_written = false;
            self.stamp_written = false;
            self.audit_written = false;
        }

        fn write_failure_audit(&mut self, diagnostic: &FailureDiagnostic) -> Result<(), ()> {
            self.failure_audits.push(diagnostic.clone());
            Ok(())
        }
    }

    fn input(ns: &str) -> FinalizationInput {
        FinalizationInput {
            record_id: Uuid::new_v4(),
            substrate: Substrate::Entity,
            namespace: ns.to_string(),
            entry_point: "entity.create",
        }
    }

    #[test]
    fn happy_path_exempts_and_writes_all_three_states() {
        let ns = "txn-test-happy-path";
        let mut effects = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));
        assert!(matches!(outcome, FinalizerOutcome::Exempted(_)));
        assert_eq!(effects.state(), (true, true, true));
        assert!(sink.snapshot().is_empty());
    }

    #[test]
    fn manifest_invalid_touches_no_transactional_state() {
        let ns = "txn-test-manifest-invalid";
        let mut effects = FakeEffects {
            resolve_err: Some(ManifestFault::UnknownSchemaVersion),
            ..Default::default()
        };
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));
        assert_eq!(
            outcome,
            FinalizerOutcome::ManifestInvalid(ManifestFault::UnknownSchemaVersion)
        );
        assert_eq!(effects.state(), (false, false, false));
    }

    #[test]
    fn manifest_invalid_seam_overrides_effects_and_is_one_shot() {
        let ns = "txn-test-manifest-invalid-seam";
        let _arm = faults::arm_manifest_invalid(ns, ManifestFault::RefreshFailure);
        let mut effects = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));
        assert_eq!(
            outcome,
            FinalizerOutcome::ManifestInvalid(ManifestFault::RefreshFailure)
        );
        assert_eq!(effects.state(), (false, false, false));

        // One-shot: a second call on the same namespace hits real effects.
        let outcome2 = finalize(&mut effects, &sink, &input(ns));
        assert!(matches!(outcome2, FinalizerOutcome::Exempted(_)));
    }

    #[test]
    fn record_write_failure_rolls_back_to_pre_transaction_state() {
        let ns = "txn-test-record-write-fail";
        let pre_state = FakeEffects::default().state();
        let _arm = faults::arm_record_write_fail(ns);
        let mut effects = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));
        match outcome {
            FinalizerOutcome::RecordWriteFailed(diagnostic) => {
                assert_eq!(diagnostic.failure_class, FailureClass::RecordWrite);
            }
            other => panic!("expected RecordWriteFailed, got {other:?}"),
        }
        assert_eq!(
            effects.state(),
            pre_state,
            "post-failure state must equal pre-transaction state"
        );
        assert_eq!(effects.failure_audits.len(), 1);
        assert!(
            sink.snapshot().is_empty(),
            "primary failure-audit succeeded, sink must stay empty"
        );
    }

    #[test]
    fn stamp_failure_rolls_back_record_too() {
        let ns = "txn-test-stamp-fail";
        let pre_state = FakeEffects::default().state();
        let _arm = faults::arm_stamp_fail(ns);
        let mut effects = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));
        match outcome {
            FinalizerOutcome::StampFailed(diagnostic) => {
                assert_eq!(diagnostic.failure_class, FailureClass::Stamp);
            }
            other => panic!("expected StampFailed, got {other:?}"),
        }
        assert_eq!(effects.state(), pre_state);
        assert!(sink.snapshot().is_empty());
    }

    #[test]
    fn success_audit_failure_rolls_back_record_and_stamp() {
        let ns = "txn-test-success-audit-fail";
        let pre_state = FakeEffects::default().state();
        let _arm = faults::arm_success_audit_fail(ns);
        let mut effects = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));
        match outcome {
            FinalizerOutcome::AuditFailed(diagnostic) => {
                assert_eq!(diagnostic.failure_class, FailureClass::SuccessAudit);
            }
            other => panic!("expected AuditFailed, got {other:?}"),
        }
        assert_eq!(effects.state(), pre_state);
        assert!(sink.snapshot().is_empty());
    }

    #[test]
    fn second_order_failure_audit_failure_reaches_the_log_sink_exactly_once() {
        let ns = "txn-test-second-order";
        let pre_state = FakeEffects::default().state();
        let _record_arm = faults::arm_record_write_fail(ns);
        let _failure_audit_arm = faults::arm_failure_audit_fail(ns);
        let mut effects = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome = finalize(&mut effects, &sink, &input(ns));

        let diagnostic = match outcome {
            FinalizerOutcome::RecordWriteFailed(diagnostic) => diagnostic,
            other => panic!("expected RecordWriteFailed, got {other:?}"),
        };
        assert_eq!(
            effects.state(),
            pre_state,
            "rollback must still hold under second-order failure"
        );
        assert!(
            effects.failure_audits.is_empty(),
            "failure-audit write was injected to fail; it must not have recorded anything"
        );

        let gaps = sink.snapshot();
        assert_eq!(
            gaps.len(),
            1,
            "exactly one audit-gap record for the second-order case"
        );
        let gap = &gaps[0];
        assert_eq!(gap.diagnostic_id, diagnostic.diagnostic_id);
        assert_eq!(gap.failure_class, FailureClass::RecordWrite);
        assert_eq!(gap.record_id, diagnostic.record_id);
        assert_eq!(gap.substrate, diagnostic.substrate);
        assert_eq!(gap.namespace, diagnostic.namespace);
        assert_eq!(gap.entry_point, diagnostic.entry_point);
    }

    #[test]
    fn distinct_namespaces_do_not_cross_contaminate_seams() {
        let ns_a = "txn-test-ns-a";
        let ns_b = "txn-test-ns-b";
        let _arm = faults::arm_record_write_fail(ns_a);
        let mut effects_b = FakeEffects::default();
        let sink = CapturingLogSink::new();
        let outcome_b = finalize(&mut effects_b, &sink, &input(ns_b));
        assert!(matches!(outcome_b, FinalizerOutcome::Exempted(_)));
    }
}
