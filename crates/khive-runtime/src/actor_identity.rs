//! Shared actor-identity resolution (issue #567).
//!
//! Single source of truth for "who is the caller", consumed by the gate
//! check, storage-token minting, comm attribution, and MCP strict-mode
//! enforcement — those four sites must agree, so drift here is a silent
//! trust-boundary bug.

use khive_gate::ActorRef;

/// Resolve an optional configured/request actor id string into the
/// [`ActorRef`] used for gate checks, `NamespaceToken` minting, and comm
/// attribution.
///
/// Preserves the pre-existing behavior at every call site: a non-empty
/// string (after trimming) becomes `ActorRef::new("actor", id)` using the
/// original (untrimmed) string; `None` or an all-whitespace string becomes
/// `ActorRef::anonymous()`.
pub fn resolve_actor(actor_id: Option<&str>) -> ActorRef {
    match actor_id {
        Some(id) if !id.trim().is_empty() => ActorRef::new("actor", id),
        _ => ActorRef::anonymous(),
    }
}

/// True when `actor` is the backward-compatible anonymous/local fallback and
/// therefore unsafe for multi-actor comm attribution or strict-mode serving.
pub fn actor_is_unattributed(actor: &ActorRef) -> bool {
    actor.id == "local"
}

/// Startup warning / strict-mode predicate for serving paths that load the
/// `comm` pack: true when the resolved actor is unattributed AND `"comm"` is
/// among the loaded packs.
pub fn should_warn_unattributed_actor(actor_id: Option<&str>, loaded_packs: &[String]) -> bool {
    let actor = resolve_actor(actor_id);
    actor_is_unattributed(&actor) && loaded_packs.iter().any(|p| p == "comm")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_actor_none_is_anonymous() {
        let actor = resolve_actor(None);
        assert_eq!(actor.id, "local");
        assert!(actor_is_unattributed(&actor));
    }

    #[test]
    fn resolve_actor_blank_is_anonymous() {
        let actor = resolve_actor(Some("   "));
        assert_eq!(actor.id, "local");
        assert!(actor_is_unattributed(&actor));
    }

    #[test]
    fn resolve_actor_configured_id_is_attributed() {
        let actor = resolve_actor(Some("lambda:khive"));
        assert_eq!(actor.kind, "actor");
        assert_eq!(actor.id, "lambda:khive");
        assert!(!actor_is_unattributed(&actor));
    }

    #[test]
    fn resolve_actor_preserves_untrimmed_id() {
        // Trim only decides emptiness; the id itself is stored verbatim.
        let actor = resolve_actor(Some(" lambda:khive "));
        assert_eq!(actor.id, " lambda:khive ");
    }

    #[test]
    fn should_warn_unattributed_actor_fires_for_local_plus_comm() {
        let packs = vec!["kg".to_string(), "comm".to_string()];
        assert!(should_warn_unattributed_actor(None, &packs));
        assert!(should_warn_unattributed_actor(Some("local"), &packs));
        assert!(should_warn_unattributed_actor(Some("  "), &packs));
    }

    #[test]
    fn should_warn_unattributed_actor_silent_without_comm_pack() {
        let packs = vec!["kg".to_string(), "memory".to_string()];
        assert!(!should_warn_unattributed_actor(None, &packs));
    }

    #[test]
    fn should_warn_unattributed_actor_silent_when_attributed() {
        let packs = vec!["kg".to_string(), "comm".to_string()];
        assert!(!should_warn_unattributed_actor(
            Some("lambda:khive"),
            &packs
        ));
    }
}
