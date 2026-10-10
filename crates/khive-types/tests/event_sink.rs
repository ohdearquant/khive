//! Public event declaration contract; no event-store routing is exercised here.
use khive_types::{EventKind, EventSink};

const EXISTING_KIND_NAMES: [&str; 42] = [
    "audit",
    "recall_executed",
    "rerank_executed",
    "search_executed",
    "tool_check_decided",
    "link_created",
    "entity_created",
    "entity_updated",
    "entity_deleted",
    "entity_merged",
    "note_merged",
    "note_created",
    "note_updated",
    "note_deleted",
    "edge_updated",
    "edge_deleted",
    "task_transitioned",
    "feedback_explicit",
    "feedback_unjudged",
    "profile_resolution_recommended",
    "profile_merged",
    "embedding_model_changed",
    "embedding_migration_completed",
    "embedding_migration_failed",
    "embedding_drift_detected",
    "embedder_initialized",
    "proposal_created",
    "proposal_reviewed",
    "proposal_applied",
    "proposal_withdrawn",
    "channel_poll_started",
    "channel_poll_succeeded",
    "channel_poll_failed",
    "channel_backoff_armed",
    "channel_backoff_reset",
    "channel_heartbeat_persist_failed",
    "config_locked",
    "checkpoint_outcome_recorded",
    "phase_started",
    "phase_completed",
    "phase_cancelled",
    "refusal",
];

#[test]
fn current_kind_set_has_explicit_caller_store_declarations() {
    let actual = EventKind::ALL.map(EventKind::name);
    assert_eq!(actual, EXISTING_KIND_NAMES);
    for (kind, expected_name) in EventKind::ALL.into_iter().zip(EXISTING_KIND_NAMES) {
        assert_eq!(kind.sink(), EventSink::CallerEventStore, "{expected_name}");
        assert_eq!(expected_name.parse::<EventKind>().unwrap(), kind);
    }
}

#[test]
fn closed_sink_names_and_declarations_are_const_usable() {
    const CALLER: &str = EventSink::CallerEventStore.name();
    const OPERATOR: &str = EventSink::OperatorAudit.name();
    const AUDIT: EventSink = EventKind::Audit.sink();
    assert_eq!(CALLER, "caller_event_store");
    assert_eq!(OPERATOR, "operator_audit");
    assert_eq!(AUDIT, EventSink::CallerEventStore);
    assert_ne!(EventSink::CallerEventStore, EventSink::OperatorAudit);
}

#[cfg(feature = "serde")]
#[test]
fn sink_serde_accepts_only_the_two_canonical_names() {
    for sink in [EventSink::CallerEventStore, EventSink::OperatorAudit] {
        let encoded = serde_json::to_value(sink).unwrap();
        assert_eq!(encoded, serde_json::json!(sink.name()));
        assert_eq!(serde_json::from_value::<EventSink>(encoded).unwrap(), sink);
    }
    for value in [
        serde_json::json!("unknown"),
        serde_json::json!("caller"),
        serde_json::json!("CallerEventStore"),
        serde_json::json!("operator-audit"),
        serde_json::json!(" operator_audit"),
        serde_json::json!(""),
        serde_json::json!(null),
        serde_json::json!(0),
        serde_json::json!({"sink": "operator_audit"}),
    ] {
        assert!(
            serde_json::from_value::<EventSink>(value.clone()).is_err(),
            "{value}"
        );
    }
}

#[cfg(feature = "serde")]
#[test]
fn existing_kind_wire_names_remain_unchanged() {
    for (kind, expected_name) in EventKind::ALL.into_iter().zip(EXISTING_KIND_NAMES) {
        let value = serde_json::to_value(kind).unwrap();
        assert_eq!(value, serde_json::json!(expected_name));
        assert_eq!(serde_json::from_value::<EventKind>(value).unwrap(), kind);
    }
}
