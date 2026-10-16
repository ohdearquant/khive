use std::{future::Future, pin::Pin, task::Poll};

use khive_storage::{await_request_read_phase, ensure_request_read_active};
use tokio::sync::Semaphore;

use super::types::*;

type Job<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;
type ArmError<A> = ExecutorError<<A as ExecutorArm>::Error, <A as ExecutorArm>::Evidence>;
type Slot<A> = (
    usize,
    Vec<Scored<<A as ExecutorArm>::Id, <A as ExecutorArm>::Label>>,
    ArmReport<<A as ExecutorArm>::Evidence, <A as ExecutorArm>::Error>,
);
type Group<A> = (Vec<Slot<A>>, Option<<A as ExecutorArm>::Evidence>);

#[derive(Clone, Copy)]
pub(crate) enum ArmErrorPolicy {
    CollectThenSelect,
    FailFast,
}

async fn join_jobs<T: Send, E: Send, D: Send>(
    operation: &'static str,
    jobs: Vec<Job<'_, T, ExecutorError<E, D>>>,
    policy: ArmErrorPolicy,
) -> Result<Vec<T>, ExecutorError<E, D>> {
    let mut jobs: Vec<_> = jobs.into_iter().map(Some).collect();
    let mut results: Vec<Option<Result<T, ExecutorError<E, D>>>> =
        (0..jobs.len()).map(|_| None).collect();
    let mut remaining = jobs.len();
    std::future::poll_fn(move |cx| {
        for (index, job) in jobs.iter_mut().enumerate() {
            if let Err(error) = ensure_request_read_active(operation) {
                return Poll::Ready(Err(ExecutorError::RequestStopped(error)));
            }
            let Some(future) = job.as_mut() else {
                continue;
            };
            let polled = future.as_mut().poll(cx);
            if let Err(error) = ensure_request_read_active(operation) {
                return Poll::Ready(Err(ExecutorError::RequestStopped(error)));
            }
            if let Poll::Ready(result) = polled {
                *job = None;
                remaining -= 1;
                if matches!(policy, ArmErrorPolicy::FailFast) {
                    match result {
                        Ok(value) => results[index] = Some(Ok(value)),
                        Err(error) => return Poll::Ready(Err(error)),
                    }
                } else {
                    results[index] = Some(result);
                }
            }
        }
        if remaining == 0 {
            Poll::Ready(
                results
                    .iter_mut()
                    .map(|result| result.take().expect("completed job has a result"))
                    .collect(),
            )
        } else {
            Poll::Pending
        }
    })
    .await
}

fn skipped<A: ExecutorArm + ?Sized>(
    slot: usize,
    spec: &ArmSpec<'_, A>,
    preparation: Option<A::Evidence>,
) -> Slot<A> {
    (
        slot,
        Vec::new(),
        ArmReport {
            key: spec.key.to_owned(),
            requested: spec.candidate_limit,
            returned: 0,
            preparation,
            state: ArmState::Skipped,
        },
    )
}

async fn search_slot<A: ExecutorArm + ?Sized>(
    operation: &'static str,
    slot: usize,
    spec: &ArmSpec<'_, A>,
    input: Option<&ArmInput<'_>>,
    preparation: Option<A::Evidence>,
    permits: &Semaphore,
) -> Result<Slot<A>, ArmError<A>> {
    let Some(input) = input.filter(|_| !spec.scope.namespaces.is_empty()) else {
        return Ok(skipped(slot, spec, preparation));
    };
    let permit = await_request_read_phase(operation, permits.acquire())
        .await
        .map_err(ExecutorError::RequestStopped)?
        .expect("executor-owned semaphore is never closed");
    let result = await_request_read_phase(
        operation,
        spec.adapter.search(ArmRequest {
            scope: SearchScope {
                namespaces: spec.scope.namespaces,
                filter: spec.scope.filter,
            },
            input,
            candidate_limit: spec.candidate_limit,
        }),
    )
    .await
    .map_err(ExecutorError::RequestStopped)?;
    drop(permit);
    let (hits, state) = match result {
        Ok(completion) => {
            if completion.hits.len() > spec.candidate_limit as usize {
                return Err(ExecutorError::Protocol {
                    slot: Some(slot),
                    message: "arm exceeded its candidate limit",
                });
            }
            (
                completion.hits,
                ArmState::Completed {
                    route: completion.route,
                    evidence: completion.evidence,
                },
            )
        }
        Err(ArmFailure::Ordinary(error)) => match spec.failure {
            FailurePolicy::Required => return Err(ExecutorError::Arm { slot, error }),
            FailurePolicy::TolerateOrdinary => (Vec::new(), ArmState::Failed(error)),
        },
        Err(ArmFailure::Fatal(error)) => return Err(ExecutorError::Arm { slot, error }),
        Err(ArmFailure::Unavailable(evidence)) => match spec.failure {
            FailurePolicy::Required => return Err(ExecutorError::Unavailable { slot, evidence }),
            FailurePolicy::TolerateOrdinary => (Vec::new(), ArmState::Unavailable(evidence)),
        },
    };
    let report = ArmReport {
        key: spec.key.to_owned(),
        requested: spec.candidate_limit,
        returned: hits.len(),
        preparation,
        state,
    };
    Ok((slot, hits, report))
}

async fn vector_group<A: ExecutorArm + ?Sized>(
    request: &ExecutorRequest<'_, A>,
    permits: &Semaphore,
) -> Result<Group<A>, ArmError<A>> {
    let operation = request.operation;
    let mut jobs: Vec<Job<'_, Slot<A>, ArmError<A>>> = Vec::new();
    let mut group_evidence = None;
    if let Some(preparer) = request.vector_preparer {
        let selected: Vec<_> = request
            .vector_slots
            .iter()
            .enumerate()
            .filter_map(|(position, &slot)| {
                let spec = &request.arms[slot];
                spec.input.as_ref().and_then(|input| {
                    (!spec.scope.namespaces.is_empty()).then_some((
                        position,
                        ArmRequest {
                            scope: SearchScope {
                                namespaces: spec.scope.namespaces,
                                filter: spec.scope.filter,
                            },
                            input,
                            candidate_limit: spec.candidate_limit,
                        },
                    ))
                })
            })
            .collect();
        if selected.is_empty() {
            return Ok((
                request
                    .vector_slots
                    .iter()
                    .map(|&slot| skipped(slot, &request.arms[slot], None))
                    .collect(),
                None,
            ));
        }
        let expected = selected.len();
        let prepared = await_request_read_phase(
            operation,
            preparer.prepare(selected, request.vector_slots.len()),
        )
        .await
        .map_err(ExecutorError::RequestStopped)?
        .map_err(ExecutorError::VectorPreparation)?;
        if prepared.arms.len() != expected {
            return Err(ExecutorError::Protocol {
                slot: None,
                message: "vector preparation changed selected slot count",
            });
        }
        group_evidence = Some(prepared.evidence);
        let mut prepared = prepared.arms.into_iter();
        for &slot in request.vector_slots {
            let spec = &request.arms[slot];
            let Some(input) = spec
                .input
                .as_ref()
                .filter(|_| !spec.scope.namespaces.is_empty())
            else {
                jobs.push(Box::pin(async move { Ok(skipped(slot, spec, None)) }));
                continue;
            };
            let prepared = prepared.next().expect("preparation count was validated");
            jobs.push(Box::pin(async move {
                let PreparedVector {
                    embedding,
                    evidence,
                } = prepared;
                let Some(embedding) = embedding else {
                    return Ok(skipped(slot, spec, Some(evidence)));
                };
                let ArmInput::Vector {
                    engine,
                    model_id,
                    space_id,
                    ..
                } = input
                else {
                    return Err(ExecutorError::Invalid("vector slot contains a text arm"));
                };
                let input = ArmInput::Vector {
                    engine,
                    model_id,
                    space_id: *space_id,
                    input: VectorInput::Embedding(&embedding),
                };
                search_slot(operation, slot, spec, Some(&input), Some(evidence), permits).await
            }));
        }
    } else {
        for &slot in request.vector_slots {
            let spec = &request.arms[slot];
            jobs.push(Box::pin(search_slot(
                operation,
                slot,
                spec,
                spec.input.as_ref(),
                None,
                permits,
            )));
        }
    }
    // Collect every model before selecting the first error in original slot order.
    let slots = join_jobs(operation, jobs, ArmErrorPolicy::CollectThenSelect).await?;
    Ok((slots, group_evidence))
}

pub(super) async fn collect<A: ExecutorArm + ?Sized>(
    request: &ExecutorRequest<'_, A>,
    policy: ArmErrorPolicy,
) -> Result<CollectedArms<A::Id, A::Label, A::Evidence, A::Error>, ArmError<A>> {
    let operation = request.operation;
    let permits = Semaphore::new(request.max_in_flight.get());
    let mut groups: Vec<Job<'_, Group<A>, ArmError<A>>> = Vec::new();
    for (slot, spec) in request.arms.iter().enumerate() {
        if !request.vector_slots.contains(&slot) {
            let permits = &permits;
            groups.push(Box::pin(async move {
                search_slot(operation, slot, spec, spec.input.as_ref(), None, permits)
                    .await
                    .map(|slot| (vec![slot], None))
            }));
        }
    }
    if !request.vector_slots.is_empty() {
        groups.push(Box::pin(vector_group(request, &permits)));
    }
    let completed = join_jobs(operation, groups, policy).await?;
    let mut slots: Vec<Option<Slot<A>>> = (0..request.arms.len()).map(|_| None).collect();
    let mut vector_preparation = None;
    for (group_slots, evidence) in completed {
        if evidence.is_some() {
            vector_preparation = evidence;
        }
        for value in group_slots {
            let slot = value.0;
            slots[slot] = Some(value);
        }
    }
    let mut arms = Vec::with_capacity(slots.len());
    let mut reports = Vec::with_capacity(slots.len());
    for slot in slots {
        let (_, hits, report) = slot.expect("each validated plan slot was scheduled once");
        arms.push(hits);
        reports.push(report);
    }
    Ok(CollectedArms {
        arms,
        reports,
        vector_preparation,
    })
}
