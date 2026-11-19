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
