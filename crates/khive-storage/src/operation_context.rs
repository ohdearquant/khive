//! Request-runner attribution captured before event persistence crosses tasks.

use std::future::Future;

use khive_types::OperationAttribution;

tokio::task_local! {
    static OPERATION_ATTRIBUTION: OperationAttribution;
}

/// Read provenance established by the composed-request runner.
///
/// Direct non-request work and detached background tasks have no attribution.
/// Tokio task locals are deliberately not inherited by spawned tasks. Producers
/// construct their event within the operation scope before queueing persistence.
pub fn current_operation_attribution() -> Option<OperationAttribution> {
    OPERATION_ATTRIBUTION
        .try_with(|attribution| *attribution)
        .ok()
}

/// Run one resolved operation with its original parser position.
///
/// The scope ends on completion, cancellation or panic, so concurrent operations
/// cannot overwrite one another's provenance. This context contains no arguments
/// and grants no authority.
pub async fn scope_operation_attribution<F>(attribution: OperationAttribution, work: F) -> F::Output
where
    F: Future,
{
    OPERATION_ATTRIBUTION.scope(attribution, work).await
}

#[cfg(test)]
#[path = "operation_context/tests.rs"]
mod tests;
