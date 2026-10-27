//! Warm ANN bridge: wraps `VamanaIndex` per model to cache memory-note vector search.
//! One index per model covers all namespaces; namespace filtering is applied at recall time.
//! See `crates/khive-pack-memory/docs/api/ann-lifecycle.md` for lifecycle and race handling,
//! and `crates/khive-pack-memory/docs/ann.md` for the restart classifier and ADR-118 design.

use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_retrieval::ann::corpus::{CorpusScope, LiveRowJoin, WatermarkCapture};
use khive_retrieval::ann::registry::{self as ann_registry, WatermarkAuthority, PENDING_WATERMARK};
use khive_runtime::config::ann_rebuild_threshold_from_env as ann_rebuild_threshold;
use khive_runtime::resource_admission::{
    AdmittedJob, BackgroundJoinError, BodyOutcome, FinalizationOutcome, JobOutcome, PriorityError,
    ResourceError, TrackedJob, WorkClass,
};
use khive_runtime::{
    is_benign_shutdown_cancellation, KhiveRuntime, Namespace, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::StorageError;
use khive_vamana::bridge::AnnBridgeCore;
use khive_vamana::distance::l2_normalize;
use khive_vamana::{read_commit_info, segment_commit_digest, CorpusFingerprint};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

// Reached by the test modules through `use super::*`.
#[cfg(test)]
use khive_vamana::write_external_ids_sidecar;

#[cfg(test)]
tokio::task_local! {
    static SESSION_EXACT_STATEMENT_COUNT: std::cell::Cell<usize>;
    static SESSION_FENCE_PROBE_COUNT: std::cell::Cell<usize>;
}

#[cfg(test)]
pub(crate) async fn count_session_statements<F: std::future::Future>(
    future: F,
) -> (F::Output, usize, usize) {
    SESSION_EXACT_STATEMENT_COUNT
        .scope(std::cell::Cell::new(0), async {
            SESSION_FENCE_PROBE_COUNT
                .scope(std::cell::Cell::new(0), async {
                    let output = future.await;
                    let probes = SESSION_FENCE_PROBE_COUNT.with(std::cell::Cell::get);
                    let exact = SESSION_EXACT_STATEMENT_COUNT.with(std::cell::Cell::get);
                    (output, probes, exact)
                })
                .await
        })
        .await
}

#[path = "ann/incremental.rs"]
mod incremental;
use incremental::*;
#[path = "ann/checkpoint_timer.rs"]
mod checkpoint_timer;
#[path = "ann/delta.rs"]
mod delta;
#[path = "ann/final_tail.rs"]
mod final_tail;
use final_tail::fetch_final_tail_on;
#[path = "ann/epoch.rs"]
mod epoch;
pub(crate) use epoch::{
    bump_durable_epoch, durable_epoch, ensure_epoch_schema, maybe_check_durable_epoch,
    MEMORY_SCHEMA_PLAN_STMTS,
};
#[path = "ann/scoring.rs"]
mod scoring;
pub(crate) use scoring::{AnnScoreRoute, FreshTailSearch};
#[path = "ann/session_exact.rs"]
mod session_exact;
pub(crate) use session_exact::session_exact_candidates;
#[cfg(test)]
#[path = "ann/rotation_watch_test_support.rs"]
mod rotation_watch_test_support;
#[cfg(test)]
pub(crate) use rotation_watch_test_support::take_rotation_watch_handle_for_test;

mod bridge_impl;
mod checkpoint;
mod fresh_tail;
mod loaded_index;
mod state;
mod warm;
mod watermark_log;

use checkpoint::*;
pub(crate) use fresh_tail::*;
pub(crate) use loaded_index::*;
pub(crate) use state::*;
pub(crate) use warm::*;
pub(crate) use watermark_log::*;

// ── helpers ───────────────────────────────────────────────────────────────────

pub(crate) use khive_runtime::config::sanitize_key as sanitize_model_key;

// Runtime owns the shared typed shutdown-cancellation classifier.

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "ann_owned_build_tests.rs"]
mod owned_build_tests;

#[cfg(test)]
#[path = "ann_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ann/bridge_incremental_tests.rs"]
mod bridge_incremental_tests;

#[cfg(test)]
#[path = "ann/corpus_statement_tests.rs"]
mod corpus_statement_tests;

#[cfg(test)]
#[path = "ann/corpus_capture_tests.rs"]
mod corpus_capture_tests;

// Two explicit execution contexts share the corpus implementation. Interactive
// keeps RuntimeError; admitted cancellation/refusal never becomes an empty graph.
trait AnnBuildContext: Copy + Send + Sync {
    const IS_INTERACTIVE: bool;
    type Error: From<RuntimeError> + From<StorageError>;
    fn build(
        self,
        flat: Vec<f32>,
        dims: usize,
        ids: Vec<Uuid>,
        namespaces: HashSet<String>,
    ) -> impl std::future::Future<Output = Result<AnnBridge, Self::Error>> + Send;
}

#[derive(Clone, Copy)]
struct InteractiveAnn;
impl AnnBuildContext for InteractiveAnn {
    const IS_INTERACTIVE: bool = true;
    type Error = RuntimeError;
    async fn build(
        self,
        flat: Vec<f32>,
        dims: usize,
        ids: Vec<Uuid>,
        namespaces: HashSet<String>,
    ) -> Result<AnnBridge, RuntimeError> {
        tokio::task::spawn_blocking(move || AnnBridge::build(flat, dims, ids, namespaces))
            .await
            .map_err(|error| {
                RuntimeError::Storage(StorageError::driver(
                    khive_storage::StorageCapability::Vectors,
                    "memory_ann_build",
                    error,
                ))
            })?
            .map_err(Into::into)
    }
}

#[derive(Clone, Copy)]
struct BackgroundAnn<'a> {
    job: &'a AdmittedJob,
    #[cfg(test)]
    before_build: Option<&'a Arc<dyn Fn() + Send + Sync>>,
}
impl<'a> BackgroundAnn<'a> {
    fn new(job: &'a AdmittedJob) -> Self {
        Self {
            job,
            #[cfg(test)]
            before_build: None,
        }
    }
}
enum AnnBackgroundError {
    Runtime(RuntimeError),
    Cancelled,
    Refused(PriorityError),
    FinalizationTimedOut,
}
impl From<RuntimeError> for AnnBackgroundError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}
impl From<StorageError> for AnnBackgroundError {
    fn from(error: StorageError) -> Self {
        Self::Runtime(error.into())
    }
}
fn ann_submission_error(error: ResourceError) -> AnnBackgroundError {
    match error {
        ResourceError::Closed => AnnBackgroundError::Cancelled,
        ResourceError::Priority(error) => AnnBackgroundError::Refused(error),
        error => AnnBackgroundError::Runtime(RuntimeError::Internal(error.to_string())),
    }
}

impl AnnBuildContext for BackgroundAnn<'_> {
    const IS_INTERACTIVE: bool = false;
    type Error = AnnBackgroundError;
    async fn build(
        self,
        flat: Vec<f32>,
        dims: usize,
        ids: Vec<Uuid>,
        namespaces: HashSet<String>,
    ) -> Result<AnnBridge, AnnBackgroundError> {
        #[cfg(test)]
        let before_build = self.before_build.cloned();
        let child = self
            .job
            .spawn_blocking(move || {
                #[cfg(test)]
                if let Some(hook) = before_build {
                    hook();
                }
                AnnBridge::build(flat, dims, ids, namespaces)
            })
            .map_err(ann_submission_error)?;
        match child.join().await {
            Ok(JobOutcome::Completed(result)) => {
                result.map_err(|error| AnnBackgroundError::Runtime(error.into()))
            }
            Ok(JobOutcome::Cancelled) | Err(BackgroundJoinError::Cancelled) => {
                Err(AnnBackgroundError::Cancelled)
            }
            Ok(JobOutcome::FinalizationTimedOut) => Err(AnnBackgroundError::FinalizationTimedOut),
            Ok(JobOutcome::Refused(error)) => Err(AnnBackgroundError::Refused(error)),
            Err(error) => Err(AnnBackgroundError::Runtime(RuntimeError::Internal(
                format!("memory ANN CPU child join failed: {error:?}",),
            ))),
        }
    }
}

/// The supervisor owns the bracket. Cancellation selects operations, never the
/// terminal finalizer; an actual CPU child retains the same job through exit.
async fn ensure_ann_for_model_admitted(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    job: &AdmittedJob,
) -> JobOutcome<Result<AnnEnsureStatus, RuntimeError>> {
    ensure_ann_for_model_admitted_inner(
        rt,
        token,
        ann,
        model,
        job,
        #[cfg(test)]
        None,
    )
    .await
}

async fn ensure_ann_for_model_admitted_inner(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
    job: &AdmittedJob,
    #[cfg(test)] after_start: Option<&AnnAfterStart>,
) -> JobOutcome<Result<AnnEnsureStatus, RuntimeError>> {
    let prepared = match job.run_body(prepare_ann_for_model(rt, ann, model)).await {
        BodyOutcome::Finished(Ok(prepared)) => prepared,
        BodyOutcome::Finished(Err(error)) => return JobOutcome::Completed(Err(error)),
        BodyOutcome::Cancelled => return JobOutcome::Cancelled,
    };
    let (key, target_generation, force_full_rebuild, _guard) = match prepared {
        AnnPreparation::Attempt {
            key,
            target_generation,
            force_full_rebuild,
            _guard,
        } => (key, target_generation, force_full_rebuild, _guard),
        AnnPreparation::Finished(status) => return JobOutcome::Completed(Ok(status)),
    };
    if job.is_cancelled() {
        return JobOutcome::Cancelled;
    }
    let phase_start = std::time::Instant::now();
    let cpu_start = khive_runtime::process_resource_usage();
    let _phase_guard = khive_runtime::register_active_phase("ann_warm");
    #[cfg(test)]
    if after_start.is_some_and(|gate| gate.cancel_before_start.load(Ordering::SeqCst)) {
        job.request_cancel();
    }
    let mut start_attempted = false;
    let started = job
        .run_body(async {
            // Mark the attempt only when the emitter future is actually polled.
            // Cancellation before that poll has no phase pair to finalize.
            start_attempted = true;
            emit_ann_warm_phase_event(
                rt,
                token,
                model,
                khive_types::EventKind::PhaseStarted,
                khive_storage::PhaseStartedPayload {
                    work_class: "warm".into(),
                    phase: "ann_warm".into(),
                    corpus_size: None,
                },
            )
            .await;
        })
        .await;
    if !start_attempted {
        return JobOutcome::Cancelled;
    }
    let mut details = AnnWarmDetails::default();
    let outcome = match started {
        BodyOutcome::Cancelled => {
            tracing::warn!(
                phase = "ann_warm",
                start_attempted,
                start_finished = false,
                start_append_persistence = "unknown",
                "warm start append was interrupted before returning",
            );
            JobOutcome::Cancelled
        }
        BodyOutcome::Finished(()) => {
            #[cfg(test)]
            if let Some(gate) = after_start {
                gate.entered.notify_one();
                if matches!(
                    job.run_body(gate.release.notified()).await,
                    BodyOutcome::Cancelled
                ) {
                    return finalize_ann_attempt(
                        rt,
                        token,
                        model,
                        job,
                        JobOutcome::Cancelled,
                        phase_start.elapsed().as_micros() as i64,
                        khive_runtime::cpu_delta_us(
                            cpu_start,
                            khive_runtime::process_resource_usage(),
                        ),
                        details,
                    )
                    .await;
                }
            }
            match job
                .run_body(warm::ensure_ann_for_model_inner_with_context(
                    rt,
                    token,
                    ann,
                    model,
                    target_generation,
                    force_full_rebuild,
                    &mut details,
                    BackgroundAnn::new(job),
                ))
                .await
            {
                BodyOutcome::Cancelled
                | BodyOutcome::Finished(Err(AnnBackgroundError::Cancelled)) => {
                    JobOutcome::Cancelled
                }
                BodyOutcome::Finished(Err(AnnBackgroundError::Refused(error))) => {
                    JobOutcome::Refused(error)
                }
                BodyOutcome::Finished(Err(AnnBackgroundError::FinalizationTimedOut)) => {
                    JobOutcome::FinalizationTimedOut
                }
                BodyOutcome::Finished(Err(AnnBackgroundError::Runtime(error))) => {
                    JobOutcome::Completed(Err(error))
                }
                BodyOutcome::Finished(Ok(status)) => JobOutcome::Completed(Ok(status)),
            }
        }
    };
    let outcome = finalize_ann_attempt(
        rt,
        token,
        model,
        job,
        outcome,
        phase_start.elapsed().as_micros() as i64,
        khive_runtime::cpu_delta_us(cpu_start, khive_runtime::process_resource_usage()),
        details,
    )
    .await;
    if matches!(&outcome, JobOutcome::Completed(Ok(_))) && !job.is_cancelled() {
        checkpoint_timer::schedule_checkpoint(rt, ann, &key).await;
    }
    outcome
}

#[cfg(test)]
#[derive(Default)]
struct AnnAfterStart {
    cancel_before_start: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

async fn finalize_ann_attempt(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    model: &str,
    job: &AdmittedJob,
    outcome: JobOutcome<Result<AnnEnsureStatus, RuntimeError>>,
    wall_us: i64,
    cpu_us: Option<i64>,
    details: AnnWarmDetails,
) -> JobOutcome<Result<AnnEnsureStatus, RuntimeError>> {
    let terminal = async {
        match &outcome {
            JobOutcome::Completed(result) => {
                emit_ann_warm_terminal_phase(rt, token, model, result, wall_us, cpu_us, details)
                    .await
            }
            _ => {
                emit_ann_warm_phase_event(
                    rt,
                    token,
                    model,
                    khive_types::EventKind::PhaseCancelled,
                    khive_storage::PhaseCancelledPayload {
                        work_class: "warm".into(),
                        phase: "ann_warm".into(),
                        wall_us,
                        cpu_us,
                    },
                )
                .await
            }
        }
    };
    let finalization = job.finalize(terminal).await;
    if matches!(
        finalization,
        FinalizationOutcome::AttemptTimedOut
            | FinalizationOutcome::NoAttemptBeforeDeadline
            | FinalizationOutcome::DeadlineOverflow
    ) {
        return JobOutcome::FinalizationTimedOut;
    }
    outcome
}

async fn submit_ann_attempt(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
    token: NamespaceToken,
    tracking: &TrackedJob,
) -> JobOutcome<Result<AnnEnsureStatus, RuntimeError>> {
    let domain = rt.background_domain();
    let admitted = match domain.admit(WorkClass::Warm, tracking).await {
        Ok(job) => job,
        Err(error) => {
            return match ann_submission_error(error) {
                AnnBackgroundError::Cancelled => JobOutcome::Cancelled,
                AnnBackgroundError::Refused(error) => JobOutcome::Refused(error),
                AnnBackgroundError::FinalizationTimedOut => JobOutcome::FinalizationTimedOut,
                AnnBackgroundError::Runtime(error) => JobOutcome::Completed(Err(error)),
            }
        }
    };
    let rt = rt.background_view();
    let ann = ann.clone();
    let model = model.to_owned();
    match domain.spawn_supervised(admitted, move |job| async move {
        ensure_ann_for_model_admitted(&rt, &token, &ann, &model, &job).await
    }) {
        Ok(job) => match job.join().await {
            Ok(outcome) => outcome,
            Err(BackgroundJoinError::Cancelled) => JobOutcome::Cancelled,
            Err(error) => JobOutcome::Completed(Err(RuntimeError::Internal(format!(
                "memory ANN supervisor join failed: {error:?}",
            )))),
        },
        Err(error) => match ann_submission_error(error) {
            AnnBackgroundError::Cancelled => JobOutcome::Cancelled,
            AnnBackgroundError::Refused(error) => JobOutcome::Refused(error),
            AnnBackgroundError::FinalizationTimedOut => JobOutcome::FinalizationTimedOut,
            AnnBackgroundError::Runtime(error) => JobOutcome::Completed(Err(error)),
        },
    }
}

#[cfg(test)]
#[path = "ann/resource_admission_tests.rs"]
mod resource_admission_tests;

enum AnnPreparation {
    Finished(AnnEnsureStatus),
    Attempt {
        key: AnnKey,
        target_generation: u64,
        force_full_rebuild: bool,
        _guard: tokio::sync::OwnedMutexGuard<()>,
    },
}

async fn prepare_ann_for_model(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    model: &str,
) -> Result<AnnPreparation, RuntimeError> {
    if model.is_empty() {
        return Ok(AnnPreparation::Finished(AnnEnsureStatus::EmptyCorpus));
    }
    let key = AnnKey::from_token(model);

    // Cross-process registry state is checked before the in-memory fast
    // path: a peer compactor can retire a pending row, after which an
    // already-loaded bridge is no longer protected and must not be served.
    let mut force_full_rebuild = match read_own_watermark(rt, model).await {
        Ok(Some(watermark)) if watermark >= 0 => false,
        Ok(Some(PENDING_WATERMARK)) => {
            evict_unprotected_index(ann, &key).await;
            true
        }
        Ok(Some(watermark)) => {
            evict_unprotected_index(ann, &key).await;
            tracing::warn!(
                model,
                watermark,
                "memory ANN registry is in a closed recovery state; refusing stale serve"
            );
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry for {model} is in unsupported recovery state {watermark}"
            )));
        }
        Ok(None) => {
            evict_unprotected_index(ann, &key).await;
            register_consumer(rt, model)
                .await
                .map_err(RuntimeError::Internal)?;
            true
        }
        Err(error) => {
            evict_unprotected_index(ann, &key).await;
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry read failed before serve: {error}"
            )));
        }
    };
    // A graph published before Slice 3 has no note_search protection. Its
    // old tail may already have been compacted under the memory row, so only
    // a new full scan may activate this second consumer. Keep the memory
    // bridge available while that work runs.
    force_full_rebuild |= note_search_requires_full_rebuild(rt, ann, model).await;

    // Read generation BEFORE any fast path or corpus snapshot to close the write race.
    let target_generation = current_generation(ann, &key).await;

    // Fast path: no lock needed if already warm AND fresh enough.
    if !force_full_rebuild
        && installed_is_fresh(ann, &key, target_generation).await
        && !checkpoint_due(ann, &key).await
    {
        return Ok(AnnPreparation::Finished(AnnEnsureStatus::AlreadyLoaded));
    }

    let lock = model_warm_lock(ann, &key).await;
    let guard = lock.lock_owned().await;

    // A concurrent caller may have activated the pending registration and
    // satisfied our generation while we waited. Re-read durable state before
    // accepting its bridge; local presence alone cannot prove protection.
    match read_own_watermark(rt, model).await {
        Ok(Some(watermark)) if watermark >= 0 => {
            force_full_rebuild = note_search_requires_full_rebuild(rt, ann, model).await;
            if !force_full_rebuild
                && installed_is_fresh(ann, &key, target_generation).await
                && !checkpoint_due(ann, &key).await
            {
                return Ok(AnnPreparation::Finished(AnnEnsureStatus::AlreadyLoaded));
            }
        }
        Ok(Some(PENDING_WATERMARK)) => force_full_rebuild = true,
        Ok(None) => {
            register_consumer(rt, model)
                .await
                .map_err(RuntimeError::Internal)?;
            force_full_rebuild = true;
        }
        Ok(Some(watermark)) => {
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry for {model} is in unsupported recovery state {watermark}"
            )));
        }
        Err(error) => {
            return Err(RuntimeError::Internal(format!(
                "memory ANN registry revalidation failed: {error}"
            )));
        }
    }
    force_full_rebuild |= note_search_requires_full_rebuild(rt, ann, model).await;

    Ok(AnnPreparation::Attempt {
        key,
        target_generation,
        force_full_rebuild,
        _guard: guard,
    })
}
