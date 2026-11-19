use async_trait::async_trait;
use serde_json::Value;

use crate::{KhiveRuntime, KindHook, RuntimeError};

#[derive(Debug)]
struct SharedCreateRefusal {
    kind: &'static str,
    own_verb: &'static str,
}

/// Refuse shared creation of a kind whose specialized writer owns its creation contract.
/// Delegate only `prepare_create` when the owning hook also guards updates or proposals.
pub fn refuse_shared_create(kind: &'static str, own_verb: &'static str) -> impl KindHook {
    SharedCreateRefusal { kind, own_verb }
}

#[async_trait]
impl KindHook for SharedCreateRefusal {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        _args: &mut Value,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "kind={} is not creatable through shared `create`, `stream.batch`, or standalone \
             `stream.append`; only `{}` applies this kind's creation contract; use `{}` instead",
            self.kind, self.own_verb, self.own_verb
        )))
    }
}
