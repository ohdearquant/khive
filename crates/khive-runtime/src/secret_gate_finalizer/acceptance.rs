//! Crate-private acceptance harness for ADR-115 Amendment 1's first
//! acceptance rung (executable contract §6; RUN_SPEC item 5; test-plan
//! evidence recorded with the change).
//!
//! `#[cfg(test)]`-only: this module exposes nothing past the crate
//! boundary and touches no public/wire surface.
//!
//! ## Scope note (read before extending)
//!
//! The declared entry points are not yet wired to a real storage backend
//! in this run — `secret_gate_finalizer.rs` states plainly: "Nothing in
//! this module is wired to a caller yet". The executable contract's §6
//! description of the harness (direct FTS/vector/edge/event row queries
//! against a live database) describes the harness *after* the
//! runtime-ingress lane lands the real `FinalizationEffects` backend. That
//! backend does not exist in this worktree yet, so this harness cannot
//! honestly claim to query live storage state for the transactional path.
//! Instead it drives the real, already-implemented `transaction::finalize`
//! state machine end-to-end through an in-memory `FinalizationEffects` fake
//! whose own booleans stand in for record/stamp/audit row existence —
//! every row of [`generated_acceptance_matrix`] that is observable through
//! `finalize` is asserted this way, keyed off the declaration itself so no
//! entry point can be silently skipped. This is recorded as a gap, not
//! substituted silently: see `acceptance_harness.md` "Known gaps".
//!
//! Case kinds that belong to the manifest scanner (legacy-scanner
//! behavior, one-byte miss, wrong-scope miss, one-snapshot refresh race)
//! or the reservation boundary (reserved-key mutation) are not
//! re-implemented here against a second, parallel model of that logic —
//! `manifest.rs` and `secret_gate.rs` already exercise them directly
//! against their real production functions. Duplicating them here would
//! either import `super::manifest`'s helpers under a different name (a
//! shadow harness that could silently drift from the real digest/parse
//! logic) or hand-roll the SHA-256 domain separation a second time, which
//! is exactly the parallel-model risk the contract's "generated, not
//! hand-maintained" principle warns against. Instead this module
//! independently cross-checks the fixed regression vectors (§4 of the
//! contract) against the real `manifest.rs` functions, and documents the
//! per-case ownership mapping in `acceptance_harness.md`.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use uuid::Uuid;

    use crate::secret_gate::reject_reserved_secret_gate_property;
    use crate::secret_gate_finalizer::declaration::{
        Substrate as DeclSubstrate, FINALIZER_ENTRY_POINTS,
    };
    use crate::secret_gate_finalizer::faults;
    use crate::secret_gate_finalizer::log_sink::CapturingLogSink;
    use crate::secret_gate_finalizer::manifest::{
        canonical_empty_document_sha256_hex, digest_to_hex, scoped_digest, RuntimeFieldScope,
    };
    use crate::secret_gate_finalizer::matrix::{generated_acceptance_matrix, MatrixCaseKind};
    use crate::secret_gate_finalizer::outcome::{
        FailureClass, FailureDiagnostic, FinalizerOutcome, ManifestFault, Substrate,
    };
    use crate::secret_gate_finalizer::transaction::{
        finalize, ExemptionMatch, FinalizationEffects, FinalizationInput,
    };

    /// In-memory `FinalizationEffects` used only by this harness. Mirrors
    /// `transaction.rs`'s own `FakeEffects` test double, but is driven from
    /// declaration-derived rows rather than one hand-picked entry point, so
    /// coverage is provably tied to every row of the generated matrix.
    #[derive(Default)]
    struct AcceptanceEffects {
        record_written: bool,
        stamp_written: bool,
        audit_written: bool,
        failure_audits: Vec<FailureDiagnostic>,
    }

    impl AcceptanceEffects {
        fn state(&self) -> (bool, bool, bool) {
            (self.record_written, self.stamp_written, self.audit_written)
        }
    }

    impl FinalizationEffects for AcceptanceEffects {
        fn resolve_manifest(&mut self) -> Result<ExemptionMatch, ManifestFault> {
            Ok(ExemptionMatch {
                digest_sha256: digest_to_hex(&scoped_digest(
                    RuntimeFieldScope::RecordContent,
                    "acceptance-harness-fixture-value",
                )),
                field_scope: "record-content",
                manifest_id: "khive-secret-gate-empty-v1".to_string(),
            })
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

    fn substrate_for(decl: DeclSubstrate) -> Substrate {
        match decl {
            DeclSubstrate::Entity => Substrate::Entity,
            DeclSubstrate::Note => Substrate::Note,
        }
    }

    /// The generated-matrix case kinds that are observable through
    /// `transaction::finalize` (the rest belong to the manifest scanner or
    /// reservation boundary; see the module doc comment).
    const FINALIZE_OBSERVABLE: &[MatrixCaseKind] = &[
        MatrixCaseKind::FixtureMatch,
        MatrixCaseKind::SupportedEcho,
        MatrixCaseKind::RecordWriteFailure,
        MatrixCaseKind::StampFailure,
        MatrixCaseKind::SuccessAuditFailure,
        MatrixCaseKind::SecondOrderFailureAuditFailure,
    ];

    /// Deliverables 2 + 3: drive every generated-matrix row whose case kind
    /// is finalize-observable, for every declared entry point, using the
    /// entry point's own id/substrate straight off the declaration row --
    /// never a hand-picked entry point or a second, independently
    /// maintained list. Asserts the exact outcome variant, that a failure
    /// path leaves state byte-for-byte equal to pre-transaction state (no
    /// survivor), and the second-order audit-gap sink behavior.
    #[test]
    fn generated_matrix_drives_finalize_for_every_declared_entry_point() {
        let rows = generated_acceptance_matrix();
        assert_eq!(
            rows.len(),
            FINALIZER_ENTRY_POINTS.len() * MatrixCaseKind::ALL.len(),
            "harness must consume the full generated cross product, not a subset"
        );

        let mut covered: BTreeSet<(&str, MatrixCaseKind)> = BTreeSet::new();
        let pre_state = AcceptanceEffects::default().state();

        for row in &rows {
            if !FINALIZE_OBSERVABLE.contains(&row.case) {
                continue;
            }
            let ns = format!("acceptance-{}-{:?}", row.entry_point.id, row.case);
            let input = FinalizationInput {
                record_id: Uuid::new_v4(),
                substrate: substrate_for(row.entry_point.substrate),
                namespace: ns.clone(),
                entry_point: row.entry_point.id,
            };

            match row.case {
                MatrixCaseKind::FixtureMatch | MatrixCaseKind::SupportedEcho => {
                    let mut effects = AcceptanceEffects::default();
                    let sink = CapturingLogSink::new();
                    let outcome = finalize(&mut effects, &sink, &input);
                    match &outcome {
                        FinalizerOutcome::Exempted(commit) => {
                            assert_eq!(commit.entry_point, row.entry_point.id);
                            assert_eq!(commit.record_id, input.record_id);
                        }
                        other => panic!(
                            "{}/{:?}: expected Exempted, got {other:?}",
                            row.entry_point.id, row.case
                        ),
                    }
                    assert_eq!(effects.state(), (true, true, true));
                    assert!(sink.snapshot().is_empty());
                }
                MatrixCaseKind::RecordWriteFailure => {
                    let _arm = faults::arm_record_write_fail(&ns);
                    let mut effects = AcceptanceEffects::default();
                    let sink = CapturingLogSink::new();
                    let outcome = finalize(&mut effects, &sink, &input);
                    match outcome {
                        FinalizerOutcome::RecordWriteFailed(d) => {
                            assert_eq!(d.failure_class, FailureClass::RecordWrite);
                            assert_eq!(d.entry_point, row.entry_point.id);
                        }
                        other => panic!(
                            "{}/{:?}: expected RecordWriteFailed, got {other:?}",
                            row.entry_point.id, row.case
                        ),
                    }
                    assert_eq!(
                        effects.state(),
                        pre_state,
                        "{}/{:?}: no partial mutation may survive rollback",
                        row.entry_point.id,
                        row.case
                    );
                    assert_eq!(effects.failure_audits.len(), 1);
                    assert!(sink.snapshot().is_empty());
                }
                MatrixCaseKind::StampFailure => {
                    let _arm = faults::arm_stamp_fail(&ns);
                    let mut effects = AcceptanceEffects::default();
                    let sink = CapturingLogSink::new();
                    let outcome = finalize(&mut effects, &sink, &input);
                    match outcome {
                        FinalizerOutcome::StampFailed(d) => {
                            assert_eq!(d.failure_class, FailureClass::Stamp)
                        }
                        other => panic!(
                            "{}/{:?}: expected StampFailed, got {other:?}",
                            row.entry_point.id, row.case
                        ),
                    }
                    assert_eq!(effects.state(), pre_state);
                }
                MatrixCaseKind::SuccessAuditFailure => {
                    let _arm = faults::arm_success_audit_fail(&ns);
                    let mut effects = AcceptanceEffects::default();
                    let sink = CapturingLogSink::new();
                    let outcome = finalize(&mut effects, &sink, &input);
                    match outcome {
                        FinalizerOutcome::AuditFailed(d) => {
                            assert_eq!(d.failure_class, FailureClass::SuccessAudit)
                        }
                        other => panic!(
                            "{}/{:?}: expected AuditFailed, got {other:?}",
                            row.entry_point.id, row.case
                        ),
                    }
                    assert_eq!(effects.state(), pre_state);
                }
                MatrixCaseKind::SecondOrderFailureAuditFailure => {
                    let _record_arm = faults::arm_record_write_fail(&ns);
                    let _failure_audit_arm = faults::arm_failure_audit_fail(&ns);
                    let mut effects = AcceptanceEffects::default();
                    let sink = CapturingLogSink::new();
                    let outcome = finalize(&mut effects, &sink, &input);
                    let diagnostic = match outcome {
                        FinalizerOutcome::RecordWriteFailed(d) => d,
                        other => panic!(
                            "{}/{:?}: expected RecordWriteFailed, got {other:?}",
                            row.entry_point.id, row.case
                        ),
                    };
                    assert_eq!(
                        effects.state(),
                        pre_state,
                        "second-order case must still leave no survivor"
                    );
                    assert!(
                        effects.failure_audits.is_empty(),
                        "failure-audit write was injected to fail; nothing should have recorded"
                    );
                    let gaps = sink.snapshot();
                    assert_eq!(
                        gaps.len(),
                        1,
                        "exactly one independent audit-gap record for the second-order case"
                    );
                    assert_eq!(gaps[0].diagnostic_id, diagnostic.diagnostic_id);
                    assert_eq!(gaps[0].entry_point, row.entry_point.id);
                    assert_eq!(gaps[0].failure_class, FailureClass::RecordWrite);
                }
                _ => unreachable!("filtered by FINALIZE_OBSERVABLE above"),
            }

            covered.insert((row.entry_point.id, row.case));
        }

        for ep in FINALIZER_ENTRY_POINTS {
            for case in FINALIZE_OBSERVABLE {
                assert!(
                    covered.contains(&(ep.id, *case)),
                    "missing acceptance coverage for {}/{:?}",
                    ep.id,
                    case
                );
            }
        }
    }

    /// Deliverable 1: universal reservation, exercised through the crate's
    /// one reservation surface (`reject_reserved_secret_gate_property`)
    /// across every operation-shape value that a create/patch/replace/
    /// merge/remove caller could present at the top level of `properties`.
    #[test]
    fn reservation_rejects_top_level_key_regardless_of_value_shape() {
        for value in [
            serde_json::json!("exempted:content-sha256-manifest-v1"),
            serde_json::json!("something-else-entirely"),
            serde_json::json!(null),
            serde_json::json!(42),
            serde_json::json!({"nested": true}),
            serde_json::json!([1, 2, 3]),
        ] {
            let props = serde_json::json!({ "khive:secret_gate": value, "other": "ok" });
            assert!(
                reject_reserved_secret_gate_property(Some(&props)).is_err(),
                "reservation must reject top-level key regardless of value shape: {props}"
            );
        }
    }

    /// Reservation must not over-reject: absent properties and a
    /// same-named key nested below the top level are ordinary content.
    #[test]
    fn reservation_allows_absent_properties_and_nested_occurrences() {
        assert!(reject_reserved_secret_gate_property(None).is_ok());
        let nested = serde_json::json!({"notes": {"khive:secret_gate": "not-a-stamp"}});
        assert!(reject_reserved_secret_gate_property(Some(&nested)).is_ok());
    }

    /// Deliverable 4: the closed v1 empty-value digest vectors, called
    /// through the real `manifest.rs` production functions (not a second,
    /// independently maintained SHA-256 implementation) and cross-checked
    /// byte-exact against the executable contract's fixed table
    /// (ADR-115 Amendment 1 §4).
    #[test]
    fn manifest_empty_value_vectors_match_the_executable_contract_table() {
        let vectors: &[(RuntimeFieldScope, &str)] = &[
            (
                RuntimeFieldScope::RecordContent,
                "8babc16495ddbc04d2fd382d3b423d452bedb7567e64cfbe5bb85a5bcb4ff04a",
            ),
            (
                RuntimeFieldScope::NameDescription,
                "db6c4a6305fabfdc246bfc3f37cba55907ee65e5035827f06598a37b478e97d1",
            ),
            (
                RuntimeFieldScope::JsonProperties,
                "2d1a970af8016a1721b1457c7675b08355b6da0b3c859934da1fabf7e660ee28",
            ),
            (
                RuntimeFieldScope::Tags,
                "a1037f3591a751f9d34748fd090a4551d87cbdbb819e7b612e470b6a3a58f833",
            ),
            (
                RuntimeFieldScope::CodeSource,
                "1a53a3a30c62d6c805aabd11ace2124699821b5f046df4c08cf96ac9fa18e892",
            ),
        ];
        for (scope, expected) in vectors {
            let actual = digest_to_hex(&scoped_digest(*scope, ""));
            assert_eq!(
                &actual, expected,
                "empty-value vector mismatch for {scope:?}"
            );
            assert_eq!(actual.len(), 64, "SHA-256 hex must be exactly 64 chars");
        }
    }

    /// Deliverable 4: the canonical EMPTY manifest document's exact bytes
    /// and its regression hash, called through the real production
    /// function and cross-checked against the contract's fixed value.
    #[test]
    fn canonical_empty_document_hash_matches_the_executable_contract_value() {
        let expected = "ee4e2ab801099252459bcf930583bed9e8107aad2cc7af2db361f85ee65a31b9";
        let actual = canonical_empty_document_sha256_hex();
        assert_eq!(actual.len(), 64);
        assert_eq!(actual, expected);
    }
}
