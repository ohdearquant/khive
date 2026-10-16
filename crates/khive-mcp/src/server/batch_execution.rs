//! Bounded batch and unit execution with response-budget accounting.

use std::future::Future;

use futures::{stream::FuturesUnordered, StreamExt};
use khive_runtime::DomainDisposition;
use khive_types::RefusalReason;
use serde_json::{json, Value};

#[cfg(doc)]
use super::batch_types::UnitBudget;
use super::batch_types::{failure_entry, BatchTask, UnitOutcome, UnitTask};

/// Response-envelope `status` for a batch of `failed`/`aborted` counts
/// (#1220): `"partial"` when either is non-zero, `"success"` otherwise. A
/// caller that only checks for the absence of a top-level RPC error has
/// nothing else to branch on for a batch where some ops failed or were
/// skipped after a chain abort.
pub(super) fn batch_status(failed: usize, aborted: usize) -> &'static str {
    if failed == 0 && aborted == 0 {
        "success"
    } else {
        "partial"
    }
}

/// Attach the CLI aggregate `strict-op-failure` reason before any save sink
/// serializes and hashes the canonical result rows.
///
/// This function deliberately does not emit stderr: the operator boundary in
/// `kkernel` owns emission. A dispatch-owned specific reason always wins, and
/// an unfamiliar future sibling reason is preserved rather than overwritten.
pub(super) fn attach_strict_refusal_reasons(result: &mut Value) {
    let failed = result["summary"]["failed"].as_u64().unwrap_or(0);
    let aborted = result["summary"]["aborted"].as_u64().unwrap_or(0);
    if failed == 0 && aborted == 0 {
        return;
    }

    let Some(entries) = result["results"].as_array_mut() else {
        return;
    };
    for entry in entries {
        if entry["ok"].as_bool() == Some(true) || entry.get("reason").is_some() {
            continue;
        }
        if let Some(object) = entry.as_object_mut() {
            object.insert(
                "reason".to_string(),
                json!(RefusalReason::StrictOpFailure.as_str()),
            );
        }
    }
}

pub(super) fn batch_budget_error(tool: &str, response_budget: usize) -> Value {
    failure_entry(
        tool,
        json!({"kind":"response_budget_exceeded", "code":"response_budget_exceeded",
        "message":format!("batch response budget of {response_budget} serialized bytes exceeded")}),
        DomainDisposition::NotCommitted,
    )
}

pub(super) async fn execute_bounded_batch<I, F>(
    tasks: I,
    response_budget: usize,
    max_concurrency: usize,
) -> Vec<Value>
where
    I: IntoIterator<Item = BatchTask<F>>,
    F: Future<Output = Value>,
{
    assert!(max_concurrency > 0, "batch concurrency must be nonzero");
    let mut queued: std::collections::VecDeque<_> = tasks.into_iter().collect();
    let total = queued.len();
    let mut in_flight = FuturesUnordered::new();
    let start = |task: BatchTask<F>| async move {
        let entry = task.future.await;
        (task.index, task.tool, entry)
    };
    for _ in 0..max_concurrency {
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }

    let mut results: Vec<Option<Value>> = (0..total).map(|_| None).collect();
    let mut accumulated_bytes = 0usize;
    let mut budget_breached = false;

    while let Some((index, _tool, entry)) = in_flight.next().await {
        if budget_breached {
            results[index] = Some(entry);
            continue;
        }
        let serialized_bytes = serde_json::to_vec(&entry)
            .expect("serde_json::Value is always serializable")
            .len();
        if serialized_bytes > response_budget.saturating_sub(accumulated_bytes) {
            results[index] = Some(entry);
            budget_breached = true;
            continue;
        }

        accumulated_bytes += serialized_bytes;
        results[index] = Some(entry);
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }

    for task in queued {
        results[task.index] = Some(batch_budget_error(&task.tool, response_budget));
    }

    results
        .into_iter()
        .map(|entry| entry.expect("every started or queued batch task has a result"))
        .collect()
}

/// Bounded-concurrency driver for one bracketed batch of chains (ADR-016
/// Amendment 2): at most `max_concurrency` units are admitted and polled at
/// once, refilled only as units complete. Every admitted unit dispatches its
/// own leaves sequentially (never more than one leaf in flight per unit), so
/// this bounds total in-flight leaves at `max_concurrency` too. Unlike
/// [`execute_bounded_batch`], response-budget accounting lives inside each
/// unit's own leaf loop (via the shared [`UnitBudget`]) rather than in this
/// driver: a unit admitted after the budget is already exhausted finds that
/// out at the top of its own first leaf, before it dispatches anything.
pub(super) async fn execute_bounded_units<I, F>(
    tasks: I,
    max_concurrency: usize,
) -> Vec<UnitOutcome>
where
    I: IntoIterator<Item = UnitTask<F>>,
    F: Future<Output = UnitOutcome>,
{
    assert!(max_concurrency > 0, "unit concurrency must be nonzero");
    let mut queued: std::collections::VecDeque<_> = tasks.into_iter().collect();
    let mut in_flight = FuturesUnordered::new();
    let start = |task: UnitTask<F>| task.future;
    for _ in 0..max_concurrency {
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }

    let mut outcomes = Vec::new();
    while let Some(outcome) = in_flight.next().await {
        outcomes.push(outcome);
        if let Some(task) = queued.pop_front() {
            in_flight.push(start(task));
        }
    }
    outcomes
}

pub(super) fn parallel_batch_envelope(results: Vec<Value>) -> Value {
    let total = results.len();
    let succeeded = results
        .iter()
        .filter(|result| result.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let failed = total - succeeded;
    json!({
        "results": results,
        "summary": { "total": total, "succeeded": succeeded, "failed": failed, "aborted": 0 },
        "status": batch_status(failed, 0),
    })
}
