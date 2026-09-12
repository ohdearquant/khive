use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, RUNTIME_STAMPED_ACTOR_KINDS};
use serde_json::Value;

pub(super) fn caller_actor(token: &NamespaceToken) -> String {
    format!("{}:{}", token.actor().kind, token.actor().id)
}

fn split_stamped_actor(label: &str) -> Option<(&str, &str)> {
    label
        .split_once(':')
        .filter(|(kind, _)| RUNTIME_STAMPED_ACTOR_KINDS.contains(kind))
}

pub(super) fn is_caller(token: &NamespaceToken, actor: &str) -> bool {
    actor == caller_actor(token) || (token.actor().kind == "actor" && actor == token.actor().id)
}

pub(super) struct ActorScope {
    actors: Option<Vec<String>>,
}

impl ActorScope {
    pub(super) fn resolve(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        actor: Option<&str>,
        all_actors: bool,
    ) -> Result<Self, RuntimeError> {
        if all_actors && actor.is_some() {
            return Err(super::invalid(
                "all_actors=true cannot be combined with actor",
            ));
        }
        if all_actors {
            if !runtime
                .config()
                .brain
                .fleet_readers
                .contains(&token.actor().id)
            {
                return Err(super::invalid(format!(
                    "actor {:?} is not a configured fleet reader",
                    token.actor().id
                )));
            }
            return Ok(Self { actors: None });
        }
        if let Some(actor) = actor {
            super::label("actor", actor)?;
            let (identity, is_self) = match split_stamped_actor(actor) {
                Some((kind, id)) => (
                    if kind == "actor" { id } else { actor },
                    token.actor().kind == kind && token.actor().id == id,
                ),
                None => (actor, is_caller(token, actor)),
            };
            if !is_self && !token.visible_namespace_strs().contains(&identity) {
                return Err(super::invalid(format!(
                    "actor {identity:?} is not visible to this caller"
                )));
            }
        }
        let actors = match actor {
            Some(actor) if split_stamped_actor(actor).is_some() => vec![actor.to_string()],
            Some(actor) => vec![actor.to_string(), format!("actor:{actor}")],
            None if token.actor().kind == "actor"
                && split_stamped_actor(&token.actor().id).is_none() =>
            {
                vec![token.actor().id.clone(), caller_actor(token)]
            }
            None => vec![caller_actor(token)],
        };
        Ok(Self {
            actors: Some(actors),
        })
    }

    pub(super) fn matches(&self, record: &Value) -> bool {
        self.actors.as_ref().is_none_or(|actors| {
            record
                .get("actor")
                .and_then(Value::as_str)
                .is_some_and(|actor| actors.iter().any(|allowed| allowed == actor))
        })
    }

    pub(super) fn description(&self) -> Value {
        match &self.actors {
            Some(actors) => serde_json::json!({"all_actors": false, "actors": actors}),
            None => serde_json::json!({"all_actors": true}),
        }
    }

    #[cfg(test)]
    pub(super) fn all() -> Self {
        Self { actors: None }
    }
}
