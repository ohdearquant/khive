use khive_runtime::{EventActorReadScope, KhiveRuntime, NamespaceToken, RuntimeResult};

pub(crate) fn event_actor_read_scope(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    actor: Option<&str>,
    all_actors: bool,
) -> RuntimeResult<EventActorReadScope> {
    khive_runtime::resolve_event_actor_read_scope(
        token,
        &runtime.config().brain.fleet_readers,
        actor,
        all_actors,
    )
}
