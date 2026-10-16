use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use super::types::*;

pub(super) fn validate<A: ExecutorArm + ?Sized>(
    request: &ExecutorRequest<'_, A>,
) -> Result<(), &'static str> {
    if request.operation.is_empty() {
        return Err("operation must not be empty");
    }
    if request.max_in_flight.get() > tokio::sync::Semaphore::MAX_PERMITS {
        return Err("max_in_flight exceeds the supported permit bound");
    }
    let mut vector_slots = HashSet::new();
    let mut previous = None;
    for &slot in request.vector_slots {
        if slot >= request.arms.len() || previous.is_some_and(|previous| slot <= previous) {
            return Err("vector slots must be in increasing original plan order and in range");
        }
        vector_slots.insert(slot);
        previous = Some(slot);
    }
    let mut keys = HashSet::new();
    let mut limit = 0usize;
    for (slot, spec) in request.arms.iter().enumerate() {
        if spec.key.is_empty() || !keys.insert(spec.key) {
            return Err("arm keys must be nonempty and unique");
        }
        if let Some(input) = &spec.input {
            match input {
                ArmInput::Vector {
                    engine,
                    model_id,
                    space_id,
                    ..
                } => {
                    if !vector_slots.contains(&slot) {
                        return Err("every vector arm must have a vector slot");
                    }
                    if engine.is_empty()
                        || model_id.is_empty()
                        || space_id.is_some_and(str::is_empty)
                    {
                        return Err("vector bindings must not contain empty identifiers");
                    }
                }
                ArmInput::Text { .. } if vector_slots.contains(&slot) => {
                    return Err("vector slot contains a text arm");
                }
                ArmInput::Text { .. } => {}
            }
            if !spec.scope.namespaces.is_empty() {
                limit = limit
                    .checked_add(spec.candidate_limit as usize)
                    .ok_or("candidate limit sum overflowed")?;
            }
        }
    }
    if limit > request.max_candidates {
        return Err("selected candidate limit sum exceeds max_candidates");
    }
    Ok(())
}

pub(super) fn identities<I: Clone, L>(arms: &[Vec<Scored<I, L>>]) -> Vec<Vec<I>> {
    arms.iter()
        .map(|arm| arm.iter().map(|hit| hit.0.clone()).collect())
        .collect()
}

pub(super) fn unchanged<I: Eq, L>(expected: &[Vec<I>], actual: &[Vec<Scored<I, L>>]) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| {
            expected.len() == actual.len()
                && expected.iter().zip(actual).all(|(id, hit)| id == &hit.0)
        })
}

pub(super) fn subsequences<I: Eq, L>(expected: &[Vec<I>], actual: &[Vec<Scored<I, L>>]) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(expected, actual)| {
            let mut remaining = expected.iter();
            actual.iter().all(|hit| remaining.any(|id| id == &hit.0))
        })
}

pub(super) fn counts<I: Eq + Hash>(ids: impl IntoIterator<Item = I>) -> HashMap<I, usize> {
    let mut counts = HashMap::new();
    for id in ids {
        *counts.entry(id).or_insert(0) += 1;
    }
    counts
}

pub(super) fn consume<I: Eq + Hash>(counts: &mut HashMap<I, usize>, id: &I) -> bool {
    match counts.get_mut(id) {
        Some(count) if *count != 0 => {
            *count -= 1;
            true
        }
        _ => false,
    }
}

#[derive(PartialEq)]
pub(super) struct Accounting {
    key: String,
    requested: u32,
    returned: usize,
    prepared: bool,
    state: u8,
}

pub(super) fn accounting<D, E>(reports: &[ArmReport<D, E>]) -> Vec<Accounting> {
    reports
        .iter()
        .map(|report| Accounting {
            key: report.key.clone(),
            requested: report.requested,
            returned: report.returned,
            prepared: report.preparation.is_some(),
            state: match &report.state {
                ArmState::Skipped => 0,
                ArmState::Completed {
                    route: ArmRoute::Primary,
                    ..
                } => 1,
                ArmState::Completed {
                    route: ArmRoute::ExactFallback,
                    ..
                } => 2,
                ArmState::Unavailable(_) => 3,
                ArmState::Failed(_) => 4,
            },
        })
        .collect()
}
