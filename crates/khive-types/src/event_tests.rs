extern crate alloc;

use super::*;
use crate::{Namespace, Timestamp};
#[cfg(feature = "serde")]
use alloc::string::ToString;

fn header() -> Header {
    Header::new(
        Id128::from_u128(1),
        Namespace::local(),
        Timestamp::from_secs(1700000000),
    )
}

#[test]
fn event_kind_parse_roundtrip() {
    for kind in EventKind::ALL {
        let parsed: EventKind = kind
            .name()
            .parse()
            .expect("EventKind::name must parse back");
        assert_eq!(parsed, kind);
    }
}

#[test]
fn grant_change_kind_has_canonical_name_and_caller_sink() {
    let kind = EventKind::GrantChange;
    assert!(EventKind::ALL.contains(&kind));
    assert_eq!(kind.name(), "grant_change");
    assert_eq!("grant_change".parse::<EventKind>().unwrap(), kind);
    assert_eq!(kind.sink(), EventSink::CallerEventStore);
    assert!("unknown"
        .parse::<EventKind>()
        .unwrap_err()
        .valid
        .contains(&"grant_change"));
}

#[cfg(feature = "serde")]
#[test]
fn grant_change_kind_has_canonical_display_and_serde_roundtrip() {
    let kind = EventKind::GrantChange;
    assert_eq!(kind.to_string(), "grant_change");
    assert_eq!(serde_json::to_string(&kind).unwrap(), "\"grant_change\"");
    assert_eq!(
        serde_json::from_str::<EventKind>("\"grant_change\"").unwrap(),
        kind
    );
}

#[cfg(feature = "serde")]
#[test]
fn feedback_unjudged_kind_has_canonical_string_and_serde_roundtrip() {
    let kind = EventKind::FeedbackUnjudged;
    assert!(EventKind::ALL.contains(&kind));
    assert_eq!(kind.name(), "feedback_unjudged");
    assert_eq!(kind.to_string(), "feedback_unjudged");
    assert_eq!("feedback_unjudged".parse::<EventKind>().unwrap(), kind);
    assert!("unknown"
        .parse::<EventKind>()
        .unwrap_err()
        .valid
        .contains(&"feedback_unjudged"));
    assert_eq!(
        serde_json::to_string(&kind).unwrap(),
        "\"feedback_unjudged\""
    );
    assert_eq!(
        serde_json::from_str::<EventKind>("\"feedback_unjudged\"").unwrap(),
        kind
    );
}

#[cfg(feature = "serde")]
#[test]
fn refusal_kind_has_canonical_string_and_serde_roundtrip() {
    let kind = EventKind::Refusal;
    assert!(EventKind::ALL.contains(&kind));
    assert_eq!(kind.name(), "refusal");
    assert_eq!(kind.to_string(), "refusal");
    assert_eq!("refusal".parse::<EventKind>().unwrap(), kind);
    assert_eq!(serde_json::to_string(&kind).unwrap(), "\"refusal\"");
    assert_eq!(
        serde_json::from_str::<EventKind>("\"refusal\"").unwrap(),
        kind
    );
}

#[test]
fn rerank_payload_records_served_profile() {
    let payload = EventPayload::RerankExecuted(RerankExecutedPayload {
        served_by_profile_id: Some("profile-a".into()),
        reranker: RerankerKind::Native,
        model_id: Some(Id128::from_u128(1)),
        query_id: None,
        tiers: Vec::new(),
        ignored_weights: Vec::new(),
        unidentified_candidates: 0,
        candidates: Vec::new(),
        reranked: Vec::new(),
        final_scores: Vec::new(),
        latency_us: 100,
        hook_applied: false,
        hook_target_match: false,
    });
    let event = EventBuilder::new("rerank", SubstrateKind::Note, "agent:test")
        .kind(EventKind::RerankExecuted)
        .payload(payload)
        .build(header());

    if let EventPayload::RerankExecuted(ref p) = event.payload {
        assert_eq!(p.served_by_profile_id.as_deref(), Some("profile-a"));
    } else {
        panic!("unexpected payload variant");
    }
}

#[test]
fn proposal_payloads_are_typed() {
    let payload = EventPayload::ProposalReviewed(ProposalReviewedPayload {
        proposal_id: Id128::from_u128(42),
        reviewer: "operator".into(),
        decision: ProposalDecision::Approve,
        comment: None,
    });
    let event = EventBuilder::new("review", SubstrateKind::Entity, "operator")
        .kind(EventKind::ProposalReviewed)
        .payload(payload)
        .build(header());
    assert_eq!(event.kind.name(), "proposal_reviewed");
}

/// C1 regression: all ProposalChangeset variants that carry Id128 fields must
/// round-trip through serde_json::Value.  Previously `Id128::deserialize` used
/// `<&str>::deserialize` which fails when the deserializer holds owned data
/// (the Value-backed path used by the MCP DSL parser).
#[cfg(feature = "serde")]
#[test]
fn proposal_changeset_id_variants_deserialize_from_value() {
    let uuid = "7426afd6-0234-4701-9045-83dfd39166e6";
    let uuid2 = "abcdef01-2345-6789-abcd-ef0123456789";

    // UpdateEntity — patch is now a structured ProposalEntityPatch object
    let v = serde_json::json!({"kind": "update_entity", "id": uuid, "patch": {"name": "NewName"}});
    let cs: ProposalChangeset =
        serde_json::from_value(v).expect("UpdateEntity must deserialize from Value");
    assert!(
        matches!(cs, ProposalChangeset::UpdateEntity { .. }),
        "expected UpdateEntity"
    );

    // Tri-state entity_type (ADR-014): absent vs explicit null vs set
    // must all survive the proposal wire boundary distinctly.
    for (json, expected) in [
        (serde_json::json!({}), None),
        (serde_json::json!({"entity_type": null}), Some(None)),
        (
            serde_json::json!({"entity_type": "algorithm"}),
            Some(Some("algorithm".to_string())),
        ),
    ] {
        let v = serde_json::json!({"kind": "update_entity", "id": uuid, "patch": json});
        let cs: ProposalChangeset =
            serde_json::from_value(v).expect("UpdateEntity must deserialize");
        let ProposalChangeset::UpdateEntity { patch, .. } = cs else {
            panic!("expected UpdateEntity");
        };
        assert_eq!(patch.entity_type, expected, "patch: {json}");
    }

    // The clear must serialize back as an explicit null.
    let patch = ProposalEntityPatch {
        name: None,
        description: None,
        properties: None,
        tags: None,
        entity_type: Some(None),
    };
    let v = serde_json::to_value(&patch).expect("serialize");
    assert_eq!(v.get("entity_type"), Some(&serde_json::Value::Null));
    assert!(
        v.get("name").is_none(),
        "absent fields must not be serialized"
    );

    // AddEdge
    let v = serde_json::json!({
        "kind": "add_edge",
        "source": uuid, "target": uuid2,
        "relation": "extends", "weight": 1.0
    });
    let cs: ProposalChangeset =
        serde_json::from_value(v).expect("AddEdge must deserialize from Value");
    assert!(
        matches!(cs, ProposalChangeset::AddEdge { .. }),
        "expected AddEdge"
    );

    // MergeEntities
    let v = serde_json::json!({"kind": "merge_entities", "into": uuid, "from": uuid2});
    let cs: ProposalChangeset =
        serde_json::from_value(v).expect("MergeEntities must deserialize from Value");
    assert!(
        matches!(cs, ProposalChangeset::MergeEntities { .. }),
        "expected MergeEntities"
    );

    // SupersedeEntity
    let v = serde_json::json!({"kind": "supersede_entity", "old": uuid, "new": uuid2});
    let cs: ProposalChangeset =
        serde_json::from_value(v).expect("SupersedeEntity must deserialize from Value");
    assert!(
        matches!(cs, ProposalChangeset::SupersedeEntity { .. }),
        "expected SupersedeEntity"
    );
}

#[cfg(feature = "serde")]
#[test]
fn proposal_changeset_rejects_invalid_edge_weight() {
    let uuid = "7426afd6-0234-4701-9045-83dfd39166e6";
    let uuid2 = "abcdef01-2345-6789-abcd-ef0123456789";

    let v = serde_json::json!({
        "kind": "add_edge",
        "source": uuid, "target": uuid2,
        "relation": "extends", "weight": 2.0
    });
    let result: Result<ProposalChangeset, _> = serde_json::from_value(v);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("[0.0, 1.0]"),
        "error should mention range: {err}"
    );
}

#[cfg(feature = "serde")]
#[test]
fn proposal_changeset_edge_metadata_preserves_wire_intent_and_legacy_shape() {
    let legacy = serde_json::json!({
        "kind": "add_edge",
        "source": "7426afd6-0234-4701-9045-83dfd39166e6",
        "target": "abcdef01-2345-6789-abcd-ef0123456789",
        "relation": "extends",
        "weight": 0.25
    });
    let decoded: ProposalChangeset = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(
        &decoded,
        ProposalChangeset::AddEdge { metadata: None, .. }
    ));
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);

    let mut explicit_null = legacy.clone();
    explicit_null["metadata"] = serde_json::Value::Null;
    let decoded: ProposalChangeset = serde_json::from_value(explicit_null).unwrap();
    assert!(matches!(
        &decoded,
        ProposalChangeset::AddEdge { metadata: None, .. }
    ));
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);

    // Shape and governed-key validation belongs to the link write boundary;
    // decoding must retain even a draft that will be refused when applied.
    for metadata in [
        serde_json::json!({"optional": false, "context": {"steps": [1, true, null], "label": "evidence"}}),
        serde_json::json!({}),
        serde_json::json!({"dependency_kind": "not-a-kind"}),
        serde_json::json!(["not an object"]),
        serde_json::json!(42),
        serde_json::json!(false),
        serde_json::json!("not an object"),
    ] {
        let mut raw = legacy.clone();
        raw["metadata"] = metadata.clone();
        let decoded: ProposalChangeset = serde_json::from_value(raw.clone()).unwrap();
        let ProposalChangeset::AddEdge {
            metadata: stored, ..
        } = &decoded
        else {
            panic!("expected AddEdge");
        };
        assert_eq!(stored.as_ref(), Some(&metadata));
        assert_eq!(serde_json::to_value(decoded).unwrap(), raw);
    }
}

#[cfg(feature = "serde")]
#[test]
fn proposal_changeset_accepts_null_edge_weight() {
    let uuid = "7426afd6-0234-4701-9045-83dfd39166e6";
    let uuid2 = "abcdef01-2345-6789-abcd-ef0123456789";

    let v = serde_json::json!({
        "kind": "add_edge",
        "source": uuid, "target": uuid2,
        "relation": "extends", "weight": null
    });
    let cs: ProposalChangeset = serde_json::from_value(v).expect("null weight should be accepted");
    assert!(matches!(
        cs,
        ProposalChangeset::AddEdge { weight: None, .. }
    ));
}

#[cfg(feature = "serde")]
#[test]
fn proposal_changeset_retire_and_supersede_edge_round_trip_and_validate() {
    let a = "7426afd6-0234-4701-9045-83dfd39166e6";
    let b = "abcdef01-2345-6789-abcd-ef0123456789";
    let c = "0123abcd-4567-89ab-cdef-0123456789ab";

    let retire = serde_json::json!({
        "kind": "retire_edge", "source": a, "target": b, "relation": "extends"
    });
    let cs: ProposalChangeset = serde_json::from_value(retire.clone()).expect("retire_edge");
    assert!(matches!(cs, ProposalChangeset::RetireEdge { .. }));
    assert_eq!(serde_json::to_value(&cs).unwrap(), retire);

    let supersede = serde_json::json!({
        "kind": "supersede_edge",
        "old": {"source": a, "target": b, "relation": "extends"},
        "new": {"source": a, "target": c, "relation": "extends", "weight": 0.5}
    });
    let cs: ProposalChangeset = serde_json::from_value(supersede.clone()).expect("supersede_edge");
    assert!(matches!(cs, ProposalChangeset::SupersedeEdge { .. }));
    assert_eq!(serde_json::to_value(&cs).unwrap(), supersede);

    let decode = |old: serde_json::Value, new: serde_json::Value| {
        serde_json::from_value::<ProposalChangeset>(
            serde_json::json!({"kind": "supersede_edge", "old": old, "new": new}),
        )
    };
    // `new` naming the edge `old` names is refused, including a symmetric
    // relation written with the endpoints swapped.
    for (old, new) in [
        (
            serde_json::json!({"source": a, "target": b, "relation": "extends"}),
            serde_json::json!({"source": a, "target": b, "relation": "extends"}),
        ),
        (
            serde_json::json!({"source": a, "target": b, "relation": "competes_with"}),
            serde_json::json!({"source": b, "target": a, "relation": "competes_with"}),
        ),
    ] {
        let err = decode(old, new).unwrap_err().to_string();
        assert!(err.contains("different edge"), "{err}");
    }
    // A directed relation reversed is a different edge.
    decode(
        serde_json::json!({"source": a, "target": b, "relation": "extends"}),
        serde_json::json!({"source": b, "target": a, "relation": "extends"}),
    )
    .expect("a reversed directed edge is a new edge");
    let err = decode(
        serde_json::json!({"source": a, "target": b, "relation": "extends"}),
        serde_json::json!({"source": a, "target": c, "relation": "extends", "weight": 2.0}),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[0.0, 1.0]"), "{err}");
}

#[cfg(feature = "serde")]
#[test]
fn rerank_payload_serde_rejects_non_finite_score() {
    let json = serde_json::json!({
        "served_by_profile_id": null,
        "model_id": "00000000-0000-0000-0000-000000000001",
        "candidates": [],
        "reranked": [],
        "final_scores": [["00000000-0000-0000-0000-000000000001", "Infinity"]],
        "latency_us": 100,
        "hook_applied": false,
        "hook_target_match": false
    });
    let result: Result<RerankExecutedPayload, _> = serde_json::from_value(json);
    assert!(result.is_err());
}

#[test]
fn rerank_payload_is_valid_checks_finite() {
    let p = RerankExecutedPayload {
        served_by_profile_id: None,
        reranker: RerankerKind::Native,
        model_id: Some(Id128::from_u128(1)),
        query_id: None,
        tiers: Vec::new(),
        ignored_weights: Vec::new(),
        unidentified_candidates: 0,
        candidates: Vec::new(),
        reranked: Vec::new(),
        final_scores: alloc::vec![(Id128::from_u128(1), 0.5)],
        latency_us: 100,
        hook_applied: false,
        hook_target_match: false,
    };
    assert!(p.is_valid());

    let p_inf = RerankExecutedPayload {
        served_by_profile_id: None,
        reranker: RerankerKind::Native,
        model_id: Some(Id128::from_u128(1)),
        query_id: None,
        tiers: Vec::new(),
        ignored_weights: Vec::new(),
        unidentified_candidates: 0,
        candidates: Vec::new(),
        reranked: Vec::new(),
        final_scores: alloc::vec![(Id128::from_u128(1), f32::INFINITY)],
        latency_us: 100,
        hook_applied: false,
        hook_target_match: false,
    };
    assert!(!p_inf.is_valid());
}

#[cfg(feature = "serde")]
#[test]
fn rerank_payload_reads_pre_amendment_events_as_native() {
    let old = serde_json::json!({
        "served_by_profile_id": "profile-a",
        "model_id": "00000000-0000-0000-0000-000000000001",
        "candidates": ["00000000-0000-0000-0000-000000000002"],
        "reranked": [],
        "final_scores": [["00000000-0000-0000-0000-000000000002", 0.25]],
        "latency_us": 100, "hook_applied": true, "hook_target_match": true
    });
    let payload: RerankExecutedPayload = serde_json::from_value(old).unwrap();
    assert_eq!(payload.reranker, RerankerKind::Native);
    assert_eq!(payload.model_id, Some(Id128::from_u128(1)));
    assert_eq!(payload.query_id, None);
    assert!(payload.tiers.is_empty());
    assert!(payload.ignored_weights.is_empty());
    assert_eq!(payload.unidentified_candidates, 0);
    assert!(payload.hook_applied && payload.hook_target_match);
    assert_eq!(
        payload.final_scores,
        alloc::vec![(Id128::from_u128(2), 0.25)]
    );
}

#[cfg(feature = "serde")]
#[test]
fn weighted_rerank_payload_has_no_fabricated_model_or_query() {
    let raw = serde_json::json!({
        "reranker": "weighted", "served_by_profile_id": null,
        "tiers": ["relevance", "temporal"], "ignored_weights": ["typo"],
        "unidentified_candidates": 2, "candidates": [], "reranked": [],
        "final_scores": [], "latency_us": 0,
        "hook_applied": false, "hook_target_match": false
    });
    let payload: RerankExecutedPayload = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(payload.reranker, RerankerKind::Weighted);
    assert_eq!(payload.model_id, None);
    assert_eq!(payload.query_id, None);
    assert_eq!(serde_json::to_value(&payload).unwrap(), raw);
    let mut invalid = raw.clone();
    invalid["reranker"] = serde_json::json!("unknown");
    assert!(serde_json::from_value::<RerankExecutedPayload>(invalid).is_err());
    let mut invalid = raw.clone();
    invalid["model_id"] = serde_json::json!("not-a-uuid");
    assert!(serde_json::from_value::<RerankExecutedPayload>(invalid).is_err());
    let mut invalid = raw.clone();
    invalid["query_id"] = serde_json::json!(42);
    assert!(serde_json::from_value::<RerankExecutedPayload>(invalid).is_err());
    let mut correlated = raw.clone();
    correlated["query_id"] = serde_json::json!(" query/session-7 ");
    let payload: RerankExecutedPayload = serde_json::from_value(correlated.clone()).unwrap();
    assert_eq!(payload.query_id.as_deref(), Some(" query/session-7 "));
    assert_eq!(serde_json::to_value(payload).unwrap(), correlated);
    // A finite f64 can still overflow the existing finite-f32 event contract.
    for field in ["final_scores", "reranked"] {
        let mut invalid = raw.clone();
        invalid[field] = if field == "final_scores" {
            serde_json::json!([["00000000-0000-0000-0000-000000000001", f64::MAX]])
        } else {
            serde_json::json!([[
                "00000000-0000-0000-0000-000000000001",
                [["relevance", f64::MAX]]
            ]])
        };
        assert!(serde_json::from_value::<RerankExecutedPayload>(invalid).is_err());
    }
}
