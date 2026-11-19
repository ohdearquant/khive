use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use serde_json::Value;

pub(super) fn caller_actor(token: &NamespaceToken) -> String {
    format!("{}:{}", token.actor().kind, token.actor().id)
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
        // Keep telemetry's label validation before visibility checks, but let the
        // shared resolver report an all_actors/actor conflict before label errors.
        if !all_actors {
            if let Some(actor) = actor {
                super::label("actor", actor)?;
            }
        }
        let scope = khive_runtime::resolve_event_actor_read_scope(
            token,
            &runtime.config().brain.fleet_readers,
            actor,
            all_actors,
        )?;
        Ok(Self {
            actors: (!all_actors).then_some(scope.actors),
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
