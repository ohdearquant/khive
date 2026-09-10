use std::{future::Future, time::Duration};

use khive_storage::{capture_request_read_context, StorageError};
use serde::Serialize;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LexicalPass {
    Full,
    #[serde(rename = "subquery_1")]
    Subquery1,
    #[serde(rename = "subquery_2")]
    Subquery2,
}

impl LexicalPass {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Subquery1 => "subquery_1",
            Self::Subquery2 => "subquery_2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LexicalPhase {
    ReaderOpen,
    TermFrequency,
    PhaseARowids,
    PhaseBHydration,
    EligibilityFallback,
    NamespaceMembership,
    RecentFallback,
}

impl LexicalPhase {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ReaderOpen => "reader_open",
            Self::TermFrequency => "term_frequency",
            Self::PhaseARowids => "phase_a_rowids",
            Self::PhaseBHydration => "phase_b_hydration",
            Self::EligibilityFallback => "eligibility_fallback",
            Self::NamespaceMembership => "namespace_membership",
            Self::RecentFallback => "recent_fallback",
        }
    }

    pub(super) fn public(self) -> bool {
        // Later phases depend on global-index matches before namespace filtering.
        // Even their presence, without row counts, can reveal foreign matches.
        matches!(self, Self::ReaderOpen | Self::TermFrequency)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) struct LexicalTimeout {
    pub(super) pass: LexicalPass,
    pub(super) phase: LexicalPhase,
    pub(super) stage_elapsed_ms: u64,
    pub(super) operation_elapsed_ms: u64,
    pub(super) configured_budget_ms: u64,
    pub(super) effective_budget_ms: u64,
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

pub(super) struct LexicalStage {
    pass: LexicalPass,
    started: Instant,
    configured_budget_ms: u64,
    effective_budget_ms: u64,
    pub(super) timeout: Option<LexicalTimeout>,
}

impl LexicalStage {
    pub(super) fn new(pass: LexicalPass, started: Instant, configured: Duration) -> Self {
        let effective = capture_request_read_context()
            .deadline()
            .map(|deadline| {
                deadline
                    .async_at()
                    .saturating_duration_since(Instant::now())
            })
            .unwrap_or(configured)
            .min(configured);
        Self {
            pass,
            started,
            configured_budget_ms: millis(configured),
            effective_budget_ms: millis(effective),
            timeout: None,
        }
    }

    pub(super) async fn read<T>(
        &mut self,
        phase: LexicalPhase,
        read: impl Future<Output = Result<T, StorageError>>,
    ) -> Result<T, StorageError> {
        let operation_started = Instant::now();
        #[cfg(test)]
        if let Some(error) = tests::inject_timeout(self.pass, phase).await {
            self.capture_timeout(phase, operation_started);
            return Err(error);
        }
        let result = read.await;
        if matches!(&result, Err(StorageError::Timeout { .. })) {
            self.capture_timeout(phase, operation_started);
        }
        result
    }

    fn capture_timeout(&mut self, phase: LexicalPhase, operation_started: Instant) {
        let now = Instant::now();
        let detail = LexicalTimeout {
            pass: self.pass,
            phase,
            stage_elapsed_ms: millis(now.saturating_duration_since(self.started)),
            operation_elapsed_ms: millis(now.saturating_duration_since(operation_started)),
            configured_budget_ms: self.configured_budget_ms,
            effective_budget_ms: self.effective_budget_ms,
        };
        tracing::warn!(
            pass = detail.pass.label(),
            phase = detail.phase.label(),
            stage_elapsed_ms = detail.stage_elapsed_ms,
            operation_elapsed_ms = detail.operation_elapsed_ms,
            configured_budget_ms = detail.configured_budget_ms,
            effective_budget_ms = detail.effective_budget_ms,
            "lexical read timed out"
        );
        self.timeout = Some(detail);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    tokio::task_local! {
        static INJECT: (Vec<(LexicalPass, LexicalPhase)>, Duration);
    }

    pub(in crate::knowledge) async fn with_timeout<F: Future>(
        phases: Vec<LexicalPhase>,
        elapsed: Duration,
        future: F,
    ) -> F::Output {
        let targets = phases
            .into_iter()
            .flat_map(|phase| {
                [
                    LexicalPass::Full,
                    LexicalPass::Subquery1,
                    LexicalPass::Subquery2,
                ]
                .map(|pass| (pass, phase))
            })
            .collect();
        with_pass_timeouts(targets, elapsed, future).await
    }

    pub(in crate::knowledge) async fn with_pass_timeouts<F: Future>(
        targets: Vec<(LexicalPass, LexicalPhase)>,
        elapsed: Duration,
        future: F,
    ) -> F::Output {
        INJECT.scope((targets, elapsed), future).await
    }

    pub(super) async fn inject_timeout(
        pass: LexicalPass,
        phase: LexicalPhase,
    ) -> Option<StorageError> {
        let elapsed = INJECT
            .try_with(|(targets, elapsed)| targets.contains(&(pass, phase)).then_some(*elapsed))
            .ok()
            .flatten()?;
        tokio::time::advance(elapsed).await;
        Some(StorageError::Timeout {
            operation: "test.lexical_read".into(),
        })
    }

    #[tokio::test(start_paused = true)]
    async fn earlier_deadline_and_cancellation_keep_the_entry_allowance() {
        for cancel in [false, true] {
            let (tx, rx) = tokio::sync::watch::channel(false);
            khive_storage::scope_request_read_cancellation(
                rx,
                khive_storage::scope_request_read_deadline(Duration::from_millis(100), async {
                    tokio::time::advance(Duration::from_millis(30)).await;
                    let started = Instant::now();
                    khive_storage::scope_request_read_deadline(
                        Duration::from_millis(2000),
                        async {
                            let mut stage = LexicalStage::new(
                                LexicalPass::Full,
                                started,
                                Duration::from_millis(2000),
                            );
                            let elapsed = if cancel { 11 } else { 70 };
                            let result = stage
                                .read(LexicalPhase::TermFrequency, async {
                                    tokio::time::advance(Duration::from_millis(elapsed)).await;
                                    if cancel {
                                        tx.send(true).expect("cancel receiver alive");
                                    }
                                    khive_storage::ensure_request_read_active("test.read")
                                })
                                .await;
                            assert!(matches!(result, Err(StorageError::Timeout { .. })));
                            let detail = stage.timeout.expect("missing timeout detail");
                            assert_eq!(detail.effective_budget_ms, 70);
                            assert_eq!(detail.configured_budget_ms, 2000);
                            assert_eq!(detail.operation_elapsed_ms, elapsed);
                            assert_eq!(detail.stage_elapsed_ms, elapsed);
                            assert!(serde_json::to_value(detail).unwrap().get("cause").is_none());
                        },
                    )
                    .await;
                }),
            )
            .await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timings_are_per_operation_and_frozen_before_downstream_work() {
        let started = Instant::now();
        khive_storage::scope_request_read_deadline(Duration::from_millis(2000), async {
            let mut stage =
                LexicalStage::new(LexicalPass::Full, started, Duration::from_millis(2000));
            stage
                .read(LexicalPhase::TermFrequency, async {
                    tokio::time::advance(Duration::from_millis(25)).await;
                    Ok(())
                })
                .await
                .unwrap();
            let result: Result<(), StorageError> = stage
                .read(LexicalPhase::TermFrequency, async {
                    tokio::time::advance(Duration::from_millis(7)).await;
                    Err(StorageError::Timeout {
                        operation: "test.read".into(),
                    })
                })
                .await;
            assert!(result.is_err());
            tokio::time::advance(Duration::from_millis(4000)).await;
            let detail = stage.timeout.expect("missing timeout detail");
            assert_eq!(detail.stage_elapsed_ms, 32);
            assert_eq!(detail.operation_elapsed_ms, 7);
            assert_eq!(detail.effective_budget_ms, 2000);
        })
        .await;
    }

    #[tokio::test]
    async fn non_timeout_errors_do_not_create_diagnostics() {
        let mut stage = LexicalStage::new(
            LexicalPass::Full,
            Instant::now(),
            Duration::from_millis(2000),
        );
        let result: Result<(), StorageError> = stage
            .read(LexicalPhase::ReaderOpen, async {
                Err(StorageError::AdmissionTimeout {
                    operation: "test.admission".into(),
                    timeout_ms: 5,
                })
            })
            .await;
        assert!(matches!(
            result,
            Err(StorageError::AdmissionTimeout { timeout_ms: 5, .. })
        ));
        assert!(stage.timeout.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn expired_parent_has_zero_allowance() {
        khive_storage::scope_request_read_deadline(Duration::from_millis(5), async {
            tokio::time::advance(Duration::from_millis(8)).await;
            let started = Instant::now();
            khive_storage::scope_request_read_deadline(Duration::from_millis(2000), async {
                let mut stage =
                    LexicalStage::new(LexicalPass::Full, started, Duration::from_millis(2000));
                assert!(stage
                    .read(LexicalPhase::ReaderOpen, async {
                        khive_storage::ensure_request_read_active("test.expired")
                    })
                    .await
                    .is_err());
                let detail = stage.timeout.expect("missing expired timeout detail");
                assert_eq!(detail.effective_budget_ms, 0);
                assert_eq!(detail.configured_budget_ms, 2000);
                assert_eq!(detail.stage_elapsed_ms, 0);
                assert_eq!(detail.operation_elapsed_ms, 0);
            })
            .await;
        })
        .await;
    }
}
