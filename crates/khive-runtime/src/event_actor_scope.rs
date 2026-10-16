use crate::{split_stamped_label, NamespaceToken, RuntimeError, RuntimeResult};

/// Actor labels admitted by a caller-scoped event read.
#[derive(Debug)]
pub struct EventActorReadScope {
    /// Exact stored labels to match; an empty list means an authorized aggregate read.
    pub actors: Vec<String>,
    /// Caller label for default-view aggregation; explicit and aggregate views do not coalesce.
    pub default_caller: Option<String>,
}

/// Resolve the existing ADR-103 Amendment 5 actor scope without reading storage.
///
/// `fleet_readers` must come from the serving runtime's resolved configuration.
/// Packs retain any additional input-label validation and their response presentation.
pub fn resolve_event_actor_read_scope(
    token: &NamespaceToken,
    fleet_readers: &[String],
    actor: Option<&str>,
    all_actors: bool,
) -> RuntimeResult<EventActorReadScope> {
    if all_actors && actor.is_some() {
        return Err(RuntimeError::InvalidInput(
            "all_actors=true cannot be combined with actor".into(),
        ));
    }
    if all_actors && !fleet_readers.contains(&token.actor().id) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActorRef, Namespace, RUNTIME_STAMPED_ACTOR_KINDS};

    fn token(kind: &str, id: &str, visible: &[&str]) -> NamespaceToken {
        NamespaceToken::mint_with_visibility(
            Namespace::local(),
            visible
                .iter()
                .map(|name| Namespace::parse(name).unwrap())
                .collect(),
            ActorRef::new(kind, id),
        )
    }

    fn invalid(result: RuntimeResult<EventActorReadScope>, expected: &str) {
        match result {
            Err(RuntimeError::InvalidInput(message)) => assert_eq!(message, expected),
            other => panic!("expected InvalidInput {expected:?}, got {other:?}"),
        }
    }

    #[test]
    fn default_scope_keeps_caller_labels_and_only_permitted_aliases() {
        for (kind, id, labels, caller) in [
            ("actor", "worker", vec!["worker", "actor:worker"], "worker"),
            (
                "actor",
                "svc:build",
                vec!["svc:build", "actor:svc:build"],
                "svc:build",
            ),
            ("agent", "worker", vec!["agent:worker"], "agent:worker"),
            (
                "anonymous",
                "local",
                vec!["anonymous:local"],
                "anonymous:local",
            ),
            (
                "service",
                "worker",
                vec!["service:worker"],
                "service:worker",
            ),
        ] {
            let scope =
                resolve_event_actor_read_scope(&token(kind, id, &[]), &[], None, false).unwrap();
            assert_eq!(scope.actors, labels, "{kind}:{id}");
            assert_eq!(scope.default_caller.as_deref(), Some(caller));
        }
        for kind in RUNTIME_STAMPED_ACTOR_KINDS {
            let id = format!("{kind}:worker");
            let scope = resolve_event_actor_read_scope(&token("actor", &id, &[]), &[], None, false)
                .unwrap();
            assert_eq!(scope.actors, vec![format!("actor:{id}")]);
            assert_eq!(scope.default_caller, Some(id));
        }
    }

    #[test]
    fn explicit_self_filters_keep_exact_stamps_and_do_not_coalesce() {
        for (kind, id, requested, labels) in [
            ("actor", "worker", "worker", vec!["worker", "actor:worker"]),
            ("actor", "worker", "actor:worker", vec!["actor:worker"]),
            ("agent", "worker", "agent:worker", vec!["agent:worker"]),
            (
                "anonymous",
                "local",
                "anonymous:local",
                vec!["anonymous:local"],
            ),
            (
                "service",
                "worker",
                "service:worker",
                vec!["service:worker", "actor:service:worker"],
            ),
        ] {
            let scope =
                resolve_event_actor_read_scope(&token(kind, id, &[]), &[], Some(requested), false)
                    .unwrap();
            assert_eq!(scope.actors, labels);
            assert!(scope.default_caller.is_none());
        }
    }

    #[test]
    fn foreign_visibility_uses_the_documented_exact_identity() {
        for (requested, identity, labels) in [
            ("peer", "peer", vec!["peer", "actor:peer"]),
            ("actor:peer", "peer", vec!["actor:peer"]),
            ("agent:peer", "agent:peer", vec!["agent:peer"]),
            (
                "anonymous:local",
                "anonymous:local",
                vec!["anonymous:local"],
            ),
            (
                "service:peer",
                "service:peer",
                vec!["service:peer", "actor:service:peer"],
            ),
        ] {
            let scope = resolve_event_actor_read_scope(
                &token("actor", "worker", &[identity]),
                &[],
                Some(requested),
                false,
            )
            .unwrap();
            assert_eq!(scope.actors, labels);
            assert!(scope.default_caller.is_none());
            let descendant = format!("{identity}:child");
            invalid(
                resolve_event_actor_read_scope(
                    &token("actor", "worker", &[&descendant]),
                    &[],
                    Some(requested),
                    false,
                ),
                &format!("actor {identity:?} is not visible to this caller"),
            );
        }
    }

    #[test]
    fn reserved_raw_ids_do_not_gain_collapsed_self_access() {
        for kind in RUNTIME_STAMPED_ACTOR_KINDS {
            let id = format!("{kind}:worker");
            let caller = token("actor", &id, &[]);
            let canonical = format!("actor:{id}");
            let own =
                resolve_event_actor_read_scope(&caller, &[], Some(&canonical), false).unwrap();
            assert_eq!(own.actors, vec![canonical]);
            let identity = if *kind == "actor" { "worker" } else { &id };
            invalid(
                resolve_event_actor_read_scope(&caller, &[], Some(&id), false),
                &format!("actor {identity:?} is not visible to this caller"),
            );
        }
    }

    #[test]
    fn aggregate_requires_exact_fleet_id_and_conflict_wins_first() {
        let caller = token("actor", "worker", &["peer"]);
        for readers in [vec![], vec!["actor:worker".into()]] {
            invalid(
                resolve_event_actor_read_scope(&caller, &readers, None, true),
                "actor \"worker\" is not a configured fleet reader",
            );
        }
        let readers = vec!["worker".into()];
        let all = resolve_event_actor_read_scope(&caller, &readers, None, true).unwrap();
        assert!(all.actors.is_empty());
        assert!(all.default_caller.is_none());
        let default = resolve_event_actor_read_scope(&caller, &readers, None, false).unwrap();
        assert_eq!(default.actors, vec!["worker", "actor:worker"]);
        assert_eq!(default.default_caller.as_deref(), Some("worker"));
        for readers in [vec![], readers] {
            invalid(
                resolve_event_actor_read_scope(&caller, &readers, Some(""), true),
                "all_actors=true cannot be combined with actor",
            );
        }
    }

    #[test]
    fn shared_scope_does_not_add_telemetry_label_validation_to_brain() {
        let caller = token("actor", " ", &[]);
        let scope = resolve_event_actor_read_scope(&caller, &[], Some(" "), false).unwrap();
        assert_eq!(scope.actors, vec![" ", "actor: "]);
        assert!(scope.default_caller.is_none());
    }
}
