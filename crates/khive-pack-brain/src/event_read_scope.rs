use khive_runtime::{
    split_stamped_label, KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult,
};

pub(crate) struct EventActorReadScope {
    pub(crate) actors: Vec<String>,
    pub(crate) default_caller: Option<String>,
}

pub(crate) fn event_actor_read_scope(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    actor: Option<&str>,
    all_actors: bool,
) -> RuntimeResult<EventActorReadScope> {
    if all_actors && actor.is_some() {
        return Err(RuntimeError::InvalidInput(
            "all_actors=true cannot be combined with actor".into(),
        ));
    }
    if all_actors
        && !runtime
            .config()
            .brain
            .fleet_readers
            .contains(&token.actor().id)
    {
        return Err(RuntimeError::InvalidInput(format!(
            "actor {:?} is not a configured fleet reader",
            token.actor().id
        )));
    }
    let caller = token.actor().label();
    if let Some(actor) = actor {
        let (identity, is_self) = match split_stamped_label(actor) {
            Some((kind, id)) => (
                if kind == "actor" { id } else { actor },
                token.actor().kind == kind && token.actor().id == id,
            ),
            None => (actor, actor == caller),
        };
        if !is_self && !token.visible_namespace_strs().contains(&identity) {
            return Err(RuntimeError::InvalidInput(format!(
                "actor {identity:?} is not visible to this caller"
            )));
        }
    }
    let default_scope = !all_actors && actor.is_none();

    // A prefixed id has no bare alias: that spelling belongs to another
    // principal's canonical events. Only default scope coalesces actor keys.
    let actors = match actor {
        Some(a) if split_stamped_label(a).is_some() => vec![a.to_string()],
        Some(a) => vec![a.to_string(), format!("actor:{a}")],
        None if all_actors => Vec::new(),
        None if token.actor().kind == "actor" && split_stamped_label(&caller).is_some() => {
            vec![format!("actor:{caller}")]
        }
        None if token.actor().kind == "actor" => {
            vec![caller.clone(), format!("actor:{caller}")]
        }
        None => vec![caller.clone()],
    };
    Ok(EventActorReadScope {
        actors,
        default_caller: default_scope.then_some(caller),
    })
}
