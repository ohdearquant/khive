//! Bounded retrieval orchestration alongside the unchanged published search traits.
//!
//! Adapters retain authorization, physical binding, local timeout and fallback policy.
//! The executor preserves the inherited absolute request context and raw arm evidence.

mod plan;
mod protocol;
mod schedule;
mod types;

pub use plan::{
    execute_collect_leaf, execute_leaf, finish_collect, CollectedPlanOutcome, CollectionPlan,
    CollectionSlot, ExecutedLeaf, LeafExecutionContext, LeafKind, LeafSlot, PlanOperators,
    PlanProgram, PlanTemplate, PreparedLeaf,
};

pub use types::{
    ArmCompletion, ArmFailure, ArmInput, ArmReport, ArmRequest, ArmRoute, ArmSpec, ArmState,
    CandidateHydrator, CollectedArms, ExecutorArm, ExecutorError, ExecutorOutcome, ExecutorRequest,
    FailurePolicy, HydratedHit, HydratorArms, PreparedArms, PreparedVector, Scored, SearchScope,
    VectorGroupPreparer, VectorInput, VectorPreparation,
};

use khive_storage::{
    await_request_read_phase, ensure_request_read_active, inherit_request_read_context,
};
use schedule::ArmErrorPolicy;

#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod scheduling_tests;

/// Execute a runtime-style plan: collect all arms, then select text errors before vector errors.
///
/// Operators resolve in template preorder before validation/providers, even at zero limits.
/// This 0.21 API deliberately replaces the former opaque fusion closure with [`PlanProgram`].
/// Raw `Collect` templates are refused here; use [`execute_collect_leaf`] and [`finish_collect`].
/// Vector models always collect before selecting an error in original plan order.
/// Request cancellation and the inherited absolute deadline refuse every phase.
pub async fn execute_hybrid<A, H, O>(
    request: ExecutorRequest<'_, A>,
    hydrator: &H,
    program: PlanProgram<'_, O>,
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
    O: PlanOperators<Id = A::Id, Label = A::Label, Error = A::Error> + ?Sized,
{
    execute(
        request,
        hydrator,
        program,
        ArmErrorPolicy::CollectThenSelect,
    )
    .await
}

/// Execute a memory-style plan: the text/vector outer groups short-circuit on failure.
///
/// The vector group still collects every selected model before choosing its first error
/// in original plan order. Dropping an outer group drops its unfinished borrowed work.
/// All bounds, request context, preparation and hydration rules match [`execute_hybrid`].
pub async fn execute_memory_hybrid<A, H, O>(
    request: ExecutorRequest<'_, A>,
    hydrator: &H,
    program: PlanProgram<'_, O>,
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
    O: PlanOperators<Id = A::Id, Label = A::Label, Error = A::Error> + ?Sized,
{
    execute(request, hydrator, program, ArmErrorPolicy::FailFast).await
}

async fn execute<A, H, O>(
    request: ExecutorRequest<'_, A>,
    hydrator: &H,
    program: PlanProgram<'_, O>,
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
    O: PlanOperators<Id = A::Id, Label = A::Label, Error = A::Error> + ?Sized,
{
    let operation = request.operation;
    await_request_read_phase(
        operation,
        inherit_request_read_context(async move {
            let resolved = plan::resolve(program.template, program.operators);
            ensure_request_read_active(operation).map_err(ExecutorError::RequestStopped)?;
            let resolved = resolved?;
            protocol::validate(&request).map_err(ExecutorError::Invalid)?;
            let active = plan::validate(&resolved, &request).map_err(ExecutorError::Invalid)?;
            if request.vector_preparer.is_none() && !request.scope.namespaces.is_empty() {
                for (slot, spec) in request.arms.iter().enumerate() {
                    if active[slot]
                        && !spec.scope.namespaces.is_empty()
                        && matches!(
                            &spec.input,
                            Some(ArmInput::Vector {
                                input: VectorInput::Text(_),
                                ..
                            })
                        )
                    {
                        return Err(ExecutorError::MissingVectorPreparer { slot });
                    }
                }
            }
            if request.scope.namespaces.is_empty() {
                return Ok(ExecutorOutcome {
                    plan: crate::QueryNode::Empty,
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
                await_request_read_phase(operation, schedule::collect(&request, policy, &active))
                    .await
                    .map_err(ExecutorError::RequestStopped)??;
            let CollectedPlanOutcome {
                plan: consumed,
                collected,
            } = collected;
            let crate::QueryNode::Collect { children } = consumed else {
                unreachable!("scheduler returns a collection")
            };
            let mut nodes: Vec<_> = children.into_iter().map(Some).collect();
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
            let mut streams: Vec<_> = prepared.arms.into_iter().map(Some).collect();
            let (fused, plan) = plan::evaluate(
                operation,
                resolved,
                program.operators,
                &mut streams,
                &mut nodes,
                request.max_candidates,
            )
            .await?;
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
                plan,
                hits,
                arms: reports,
                vector_preparation,
            })
        }),
    )
    .await
    .map_err(ExecutorError::RequestStopped)?
}
