//! `KindHook` default bodies: a hook that implements only `prepare_create` compiles, and the
//! post-write hook it inherits does nothing.

use async_trait::async_trait;
use khive_runtime::{KhiveRuntime, KindHook, RuntimeError};
use serde_json::{json, Value};
use uuid::Uuid;

/// Implements only the one required method. If `after_create` ever loses its default body,
/// this impl stops compiling.
#[derive(Debug)]
struct PrepareOnlyHook;

#[async_trait]
impl KindHook for PrepareOnlyHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn hook_implementing_only_prepare_create_inherits_a_no_op_after_create() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let hook = PrepareOnlyHook;
    let id = Uuid::new_v4();
    let args = json!({"name": "probe"});

    let result = hook.after_create(&runtime, id, &args).await;

    result.expect("the default after_create accepts every create");
}
