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
mod tests {
    use super::*;
    use khive_types::RefResolution;

    #[tokio::test]
    async fn operation_scope_restores_parent_and_does_not_leak_to_spawned_tasks() {
        let outer = OperationAttribution {
            op_index: 1,
            ref_resolution: RefResolution::Resolved,
        };
        let inner = OperationAttribution {
            op_index: 0,
            ref_resolution: RefResolution::Literal,
        };
        assert_eq!(current_operation_attribution(), None);
        scope_operation_attribution(outer, async {
            assert_eq!(current_operation_attribution(), Some(outer));
            scope_operation_attribution(inner, async {
                assert_eq!(current_operation_attribution(), Some(inner));
            })
            .await;
            assert_eq!(current_operation_attribution(), Some(outer));
            let background = tokio::spawn(async { current_operation_attribution() });
            assert_eq!(background.await.unwrap(), None);
        })
        .await;
        assert_eq!(current_operation_attribution(), None);
    }
}
