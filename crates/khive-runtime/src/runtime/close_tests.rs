use super::*;
use crate::{RuntimeConfig, VerbRegistryBuilder};

fn owner(runtime: KhiveRuntime, host_tasks: RuntimeCloseHostTasks) -> RuntimeCloseOwner {
    RuntimeCloseOwner::new(
        VerbRegistryBuilder::new().build().unwrap(),
        vec![runtime],
        host_tasks,
    )
}

#[tokio::test]
async fn explicit_host_loops_refuse_before_consuming_the_slot() {
    for (tasks, expected) in [
        (
            RuntimeCloseHostTasks {
                daemon_checkpoint: true,
                mcp_session_sweep: false,
            },
            "daemon checkpoint loop",
        ),
        (
            RuntimeCloseHostTasks {
                daemon_checkpoint: false,
                mcp_session_sweep: true,
            },
            "MCP session sweep",
        ),
    ] {
        let runtime = KhiveRuntime::memory().unwrap();
        let mut close = owner(runtime, tasks);
        assert_eq!(
            close.close().await,
            Err(RuntimeCloseError::Refused(expected))
        );
        assert_eq!(close.runtimes.len(), 1);
        assert!(close.registry.is_some());
        assert!(!close.is_closed());
        // Refusal has neither taken a writer join nor closed this slot's audit.
        assert!(close.writers.is_none());
    }
}

#[tokio::test]
async fn claimed_checkpoint_is_detected_even_without_a_host_flag() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime
        .backend()
        .pool()
        .claim_checkpoint_ownership()
        .unwrap();
    let mut close = owner(runtime, RuntimeCloseHostTasks::default());
    assert_eq!(
        close.close().await,
        Err(RuntimeCloseError::Refused(
            "claimed daemon checkpoint owner"
        ))
    );
    assert!(close.writers.is_none());
}

#[tokio::test]
async fn split_events_and_ann_hooks_are_named_refusals() {
    let mut runtime = KhiveRuntime::memory().unwrap();
    runtime.config.events_split = Some(crate::events_split::EventsSplitConfig {
        db_path: "unused-events.sqlite".into(),
        socket_path: None,
    });
    let mut close = owner(runtime, RuntimeCloseHostTasks::default());
    assert_eq!(
        close.close().await,
        Err(RuntimeCloseError::Refused("split/socket events registry"))
    );
    let runtime = KhiveRuntime::memory().unwrap();
    *runtime.note_mutation_hook.write().unwrap() = Some(Arc::new(|_, _| Box::pin(async {})));
    let mut close = owner(runtime, RuntimeCloseHostTasks::default());
    assert_eq!(
        close.close().await,
        Err(RuntimeCloseError::Refused(
            "pack ANN tasks and mapped indexes"
        ))
    );
}

#[tokio::test]
async fn configured_embeddings_refuse_even_under_request_model_exclusions() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        events_split: None,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let models = runtime.registered_embedding_model_names();
    assert!(!models.is_empty());
    let mut close = owner(runtime, RuntimeCloseHostTasks::default());
    let result = super::super::scope_request_embedder_exclusions(models, close.close()).await;
    assert_eq!(
        result,
        Err(RuntimeCloseError::Refused(
            "embedder initialization and worker thread"
        ))
    );
    assert!(close.writers.is_none());
}
