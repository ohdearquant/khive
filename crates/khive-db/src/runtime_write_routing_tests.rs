use super::*;
use crate::timeout_sink::{capture_direct_routes, Site};

#[test]
fn runtime_routing_refuses_strict_fallbacks_and_attributes_only_enabled_queue_degrades() {
    assert!(tokio::runtime::Handle::try_current().is_err());
    let operations = [
        (
            RuntimeWriteOperation::MergeEntity,
            Site::DirectRouteRuntimeMergeEntity,
        ),
        (
            RuntimeWriteOperation::MergeNote,
            Site::DirectRouteRuntimeMergeNote,
        ),
        (
            RuntimeWriteOperation::UpdateSymmetricEdge,
            Site::DirectRouteRuntimeUpdateSymmetricEdge,
        ),
    ];
    for (strict, enabled) in [(true, true), (true, false), (false, true), (false, false)] {
        let dir = tempfile::tempdir().unwrap();
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("runtime-routing.db")),
            write_queue_enabled: Some(enabled),
            write_routing_strict: strict,
            ..PoolConfig::for_test()
        })
        .unwrap();
        let (_, events) = capture_direct_routes(|| {
            for (operation, _) in operations {
                let result = pool.writer_task_for_runtime_write(operation);
                if strict && enabled {
                    assert!(matches!(result,
                        Err(StorageError::WriterOwnerUnavailable { ref source, .. })
                        if matches!(source.as_ref(), StorageError::WriterTaskNoRuntime)
                    ));
                } else if strict {
                    assert!(matches!(
                        result,
                        Err(StorageError::WriterOwnerUnavailable { operation: name, ref source })
                            if name == operation.operation() && matches!(source.as_ref(), StorageError::Pool { .. })
                    ));
                } else {
                    assert!(result.unwrap().is_none());
                }
            }
        });
        if !strict && enabled {
            assert_eq!(
                events.iter().map(|(_, site)| *site).collect::<Vec<_>>(),
                operations.iter().map(|(_, site)| *site).collect::<Vec<_>>()
            );
            assert!(events.iter().all(|(database, _)| !database.is_empty()));
        } else {
            assert!(
                events.is_empty(),
                "refusal or explicit opt-out is not a bypass"
            );
        }
    }
}
