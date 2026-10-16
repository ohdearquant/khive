//! Bounded retrieval orchestration alongside the unchanged published search traits.
//!
//! Adapters retain authorization, physical binding, local timeout and fallback policy.
//! The executor preserves the inherited absolute request context and raw arm evidence.

mod protocol;
mod schedule;
mod types;

pub use types::{
    ArmCompletion, ArmFailure, ArmInput, ArmReport, ArmRequest, ArmRoute, ArmSpec, ArmState,
    CandidateHydrator, CollectedArms, ExecutorArm, ExecutorError, ExecutorOutcome, ExecutorRequest,
    FailurePolicy, HydratedHit, HydratorArms, PreparedArms, PreparedVector, Scored, SearchScope,
    VectorGroupPreparer, VectorInput, VectorPreparation,
};

use std::future::Future;

use khive_storage::{
    await_request_read_phase, ensure_request_read_active, inherit_request_read_context,
};
use schedule::ArmErrorPolicy;

#[cfg(test)]
mod scheduling_tests;

/// Execute a runtime-style plan: collect all arms, then select text errors before vector errors.
///
/// The fusion resolver runs once before providers, including at a zero final limit.
/// Vector models always collect before selecting an error in original plan order.
/// Request cancellation and the inherited absolute deadline refuse every phase.
pub async fn execute_hybrid<A, H, Resolve, Fuse, FuseFuture>(
    request: ExecutorRequest<'_, A>,
    hydrator: &H,
    resolve_fusion: Resolve,
) -> Result<
    ExecutorOutcome<A::Id, A::Label, A::Evidence, H::Item, A::Error>,
    ExecutorError<A::Error, A::Evidence>,
>
where
    A: ExecutorArm + ?Sized,
    H: CandidateHydrator<
            Id = A::Id,
            Label = A::Label,
            Evidence = A::Evidence,
            Filter = A::Filter,
            Error = A::Error,
        > + ?Sized,
    Resolve: FnOnce() -> Result<Fuse, A::Error> + Send,
    Fuse: FnOnce(Vec<Vec<Scored<A::Id, A::Label>>>) -> FuseFuture + Send,
    FuseFuture: Future<Output = Result<Vec<Scored<A::Id, A::Label>>, A::Error>> + Send,
{
    execute(
        request,
        hydrator,
        resolve_fusion,
        ArmErrorPolicy::CollectThenSelect,
    )
    .await
}

/// Execute a memory-style plan: the text/vector outer groups short-circuit on failure.
///
/// The vector group still collects every selected model before choosing its first error
/// in original plan order. Dropping an outer group drops its unfinished borrowed work.
/// All bounds, request context, preparation and hydration rules match [`execute_hybrid`].
pub async fn execute_memory_hybrid<A, H, Resolve, Fuse, FuseFuture>(
    request: ExecutorRequest<'_, A>,
    hydrator: &H,
    resolve_fusion: Resolve,
) -> Result<
    ExecutorOutcome<A::Id, A::Label, A::Evidence, H::Item, A::Error>,
    ExecutorError<A::Error, A::Evidence>,
>
where
    A: ExecutorArm + ?Sized,
    H: CandidateHydrator<
            Id = A::Id,
            Label = A::Label,
            Evidence = A::Evidence,
            Filter = A::Filter,
            Error = A::Error,
        > + ?Sized,
    Resolve: FnOnce() -> Result<Fuse, A::Error> + Send,
    Fuse: FnOnce(Vec<Vec<Scored<A::Id, A::Label>>>) -> FuseFuture + Send,
    FuseFuture: Future<Output = Result<Vec<Scored<A::Id, A::Label>>, A::Error>> + Send,
{
    execute(request, hydrator, resolve_fusion, ArmErrorPolicy::FailFast).await
}

async fn execute<A, H, Resolve, Fuse, FuseFuture>(
    request: ExecutorRequest<'_, A>,
    hydrator: &H,
    resolve_fusion: Resolve,
    policy: ArmErrorPolicy,
) -> Result<
    ExecutorOutcome<A::Id, A::Label, A::Evidence, H::Item, A::Error>,
    ExecutorError<A::Error, A::Evidence>,
>
where
    A: ExecutorArm + ?Sized,
    H: CandidateHydrator<
            Id = A::Id,
            Label = A::Label,
            Evidence = A::Evidence,
            Filter = A::Filter,
            Error = A::Error,
        > + ?Sized,
    Resolve: FnOnce() -> Result<Fuse, A::Error> + Send,
    Fuse: FnOnce(Vec<Vec<Scored<A::Id, A::Label>>>) -> FuseFuture + Send,
    FuseFuture: Future<Output = Result<Vec<Scored<A::Id, A::Label>>, A::Error>> + Send,
{
    let operation = request.operation;
    await_request_read_phase(
        operation,
        inherit_request_read_context(async move {
            let resolved = resolve_fusion();
            ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
            let fuse = resolved.map_err(ExecutorError::FusionResolution)?;
            protocol::validate(&request).map_err(ExecutorError::Invalid)?;
            if request.scope.namespaces.is_empty() {
                return Ok(ExecutorOutcome {
                    hits: Vec::new(),
                    arms: request
                        .arms
                        .iter()
                        .map(|spec| ArmReport {
                            key: spec.key.to_owned(),
                            requested: spec.candidate_limit,
                            returned: 0,
                            preparation: None,
                            state: ArmState::Skipped,
                        })
                        .collect(),
                    vector_preparation: None,
                });
            }
            let collected =
                await_request_read_phase(operation, schedule::collect(&request, policy))
                    .await
                    .map_err(ExecutorError::RequestStopped)??;
            let original = protocol::identities(&collected.arms);
            let scores: Vec<Vec<_>> = collected
                .arms
                .iter()
                .map(|arm| arm.iter().map(|hit| hit.1).collect())
                .collect();
            let accounting = protocol::accounting(&collected.reports);
            let had_vector_preparation = collected.vector_preparation.is_some();
            let validated = hydrator.validate_outcomes(collected);
            ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
            let collected = validated.map_err(ExecutorError::OutcomePolicy)?;
            if !protocol::unchanged(&original, &collected.arms)
                || collected
                    .arms
                    .iter()
                    .zip(&scores)
                    .any(|(arm, scores)| arm.iter().zip(scores).any(|(hit, score)| hit.1 != *score))
                || accounting != protocol::accounting(&collected.reports)
                || had_vector_preparation != collected.vector_preparation.is_some()
            {
                return Err(ExecutorError::Protocol {
                    slot: None,
                    message: "outcome validation changed streams or raw accounting",
                });
            }
            let CollectedArms {
                arms,
                reports,
                vector_preparation,
            } = collected;
            let prepared = await_request_read_phase(
                operation,
                hydrator.prepare_arms(
                    SearchScope {
                        namespaces: request.scope.namespaces,
                        filter: request.scope.filter,
                    },
                    arms,
                ),
            )
            .await
            .map_err(ExecutorError::RequestStopped)?
            .map_err(ExecutorError::Preparation)?;
            if !protocol::subsequences(&original, &prepared.arms) {
                return Err(ExecutorError::Protocol {
                    slot: None,
                    message:
                        "preparation changed slots or added, duplicated, or reordered an identity",
                });
            }
            let mut available = protocol::counts(
                prepared
                    .arms
                    .iter()
                    .flat_map(|arm| arm.iter().map(|hit| hit.0.clone())),
            );
            let fused = await_request_read_phase(operation, fuse(prepared.arms))
                .await
                .map_err(ExecutorError::RequestStopped)?
                .map_err(ExecutorError::Fusion)?;
            if fused.len() > request.max_candidates
                || fused
                    .iter()
                    .any(|hit| !protocol::consume(&mut available, &hit.0))
            {
                return Err(ExecutorError::Protocol {
                    slot: None,
                    message: "fusion exceeded candidate bounds or identity multiplicity",
                });
            }
            let mut available = protocol::counts(fused.iter().map(|hit| hit.0.clone()));
            let mut hits = await_request_read_phase(
                operation,
                hydrator.hydrate_filter_rank(
                    SearchScope {
                        namespaces: request.scope.namespaces,
                        filter: request.scope.filter,
                    },
                    prepared.state,
                    fused,
                ),
            )
            .await
            .map_err(ExecutorError::RequestStopped)?
            .map_err(ExecutorError::Hydration)?;
            if hits.len() > request.max_candidates
                || hits
                    .iter()
                    .any(|hit| !protocol::consume(&mut available, &hit.id))
            {
                return Err(ExecutorError::Protocol {
                    slot: None,
                    message: "hydration exceeded candidate bounds or identity multiplicity",
                });
            }
            hits.truncate(request.final_limit);
            Ok(ExecutorOutcome {
                hits,
                arms: reports,
                vector_preparation,
            })
        }),
    )
    .await
    .map_err(ExecutorError::RequestStopped)?
}
