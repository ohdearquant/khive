use super::{parse_relation, UpdateParams};
use serde_json::json;

// F009 (CRIT): error text must be derived from EdgeRelation::ALL, not a hardcoded list.
// Error text must include derived_from and precedes (all 15 relations must appear).
#[test]
fn parse_relation_error_lists_all_relations() {
    let err = parse_relation("not_a_relation").unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("derived_from"),
        "F009: parse_relation error must list derived_from; got: {msg}"
    );
    assert!(
        msg.contains("precedes"),
        "F009: parse_relation error must list precedes; got: {msg}"
    );
}

// Wire-level tri-state nullable f64 for `update`:
//   absent  → outer None (preserve existing value)
//   null    → Some(None) (clear the value)
//   number  → Some(Some(v)) (set to v)
//
// Regression for round-3 finding: the previous `Option<Value>` representation
// collapsed absent and null into the same `None`, so JSON null could not
// distinguish "clear" from "preserve" through the MCP wire surface.
#[test]
fn update_params_tri_state_salience() {
    let absent: UpdateParams = serde_json::from_value(json!({"id": "x", "kind": "note"})).unwrap();
    assert_eq!(
        absent.salience, None,
        "absent salience key must deserialize to outer None (preserve)"
    );

    let cleared: UpdateParams =
        serde_json::from_value(json!({"id": "x", "kind": "note", "salience": null})).unwrap();
    assert_eq!(
        cleared.salience,
        Some(None),
        "salience=null must deserialize to Some(None) (clear)"
    );

    let set: UpdateParams =
        serde_json::from_value(json!({"id": "x", "kind": "note", "salience": 0.5})).unwrap();
    assert_eq!(
        set.salience,
        Some(Some(0.5)),
        "salience=0.5 must deserialize to Some(Some(0.5)) (set)"
    );
}

#[test]
fn update_params_tri_state_decay_factor() {
    let absent: UpdateParams = serde_json::from_value(json!({"id": "x", "kind": "note"})).unwrap();
    assert_eq!(
        absent.decay_factor, None,
        "absent decay_factor key must deserialize to outer None (preserve)"
    );

    let cleared: UpdateParams =
        serde_json::from_value(json!({"id": "x", "kind": "note", "decay_factor": null})).unwrap();
    assert_eq!(
        cleared.decay_factor,
        Some(None),
        "decay_factor=null must deserialize to Some(None) (clear)"
    );

    let set: UpdateParams =
        serde_json::from_value(json!({"id": "x", "kind": "note", "decay_factor": 0.6})).unwrap();
    assert_eq!(
        set.decay_factor,
        Some(Some(0.6)),
        "decay_factor=0.6 must deserialize to Some(Some(0.6)) (set)"
    );
}

// resolve_kind_spec must recognise "proposal" as KindSpec::Proposal
#[test]
fn resolve_kind_spec_proposal() {
    use super::{resolve_kind_spec, KindSpec};
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    let spec = resolve_kind_spec("proposal", &registry).expect("should resolve proposal");
    assert_eq!(
        spec,
        KindSpec::Proposal,
        "kind=proposal must resolve to KindSpec::Proposal"
    );

    let spec_upper = resolve_kind_spec("Proposal", &registry).expect("should be case-insensitive");
    assert_eq!(
        spec_upper,
        KindSpec::Proposal,
        "kind=Proposal (mixed case) must resolve"
    );
}

// propose param deserialization
#[test]
fn propose_params_deserialization() {
    use super::ProposeParams;
    let p: ProposeParams = serde_json::from_value(json!({
        "title": "Add RoPE",
        "description": "Add RoPE entity to the graph",
        "changeset": {
            "kind": "add_entity",
            "entity": {"kind": "concept", "name": "RoPE"}
        },
        "reviewers": ["alice"],
    }))
    .expect("ProposeParams must deserialize");
    assert_eq!(p.title, "Add RoPE");
    assert_eq!(p.reviewers, vec!["alice"]);
    assert!(p.parent_id.is_none());
    assert!(p.expiry.is_none());
}

// review param deserialization with all valid decisions
#[test]
fn review_params_decisions() {
    use super::ReviewParams;
    for decision in ["approve", "reject", "comment", "request_changes"] {
        let p: ReviewParams = serde_json::from_value(json!({
            "proposal_id": "00000000-0000-0000-0000-000000000001",
            "decision": decision,
        }))
        .expect("ReviewParams must deserialize");
        assert_eq!(p.decision, decision);
    }
}

// CRIT-2 regression: ReviewParams must not accept an `actor` field.
// The actor is always derived from the NamespaceToken at dispatch time.
// If a client passes actor=<other_id>, the field is ignored (unknown fields
// are allowed by serde default, so the struct simply lacks the field).
#[test]
fn review_params_no_actor_field() {
    use super::ReviewParams;
    // Baseline: ReviewParams works without actor.
    let p: ReviewParams = serde_json::from_value(json!({
        "proposal_id": "00000000-0000-0000-0000-000000000001",
        "decision": "approve",
    }))
    .expect("ReviewParams must deserialize without actor");
    assert_eq!(p.proposal_id, "00000000-0000-0000-0000-000000000001");
    assert_eq!(p.decision, "approve");
}

// CRIT-2 regression: WithdrawParams must not accept an `actor` field.
#[test]
fn withdraw_params_no_actor_field() {
    use super::WithdrawParams;
    let p: WithdrawParams = serde_json::from_value(json!({
        "proposal_id": "00000000-0000-0000-0000-000000000002",
    }))
    .expect("WithdrawParams must deserialize without actor");
    assert_eq!(p.proposal_id, "00000000-0000-0000-0000-000000000002");
    assert!(p.rationale.is_none());
}

// CRIT-2 regression: ProposeParams must not accept an `actor` field.
#[test]
fn propose_params_no_actor_field() {
    use super::ProposeParams;
    let p: ProposeParams = serde_json::from_value(json!({
        "title": "Fix RoPE",
        "description": "Fix RoPE entity",
        "changeset": {"kind": "add_entity", "entity": {"kind": "concept", "name": "RoPE"}},
    }))
    .expect("ProposeParams must deserialize without actor");
    assert_eq!(p.title, "Fix RoPE");
}

// KG pack must expose exactly 16 handlers including propose/review/withdraw/verbs/stats
#[test]
fn kg_pack_exposes_16_handlers() {
    use crate::KgPack;
    use khive_types::Pack;
    let handlers = KgPack::HANDLERS;
    assert_eq!(
        handlers.len(),
        16,
        "kg pack must expose 16 handlers (was 15, +1 for stats — #280)"
    );
    let names: Vec<&str> = handlers.iter().map(|h| h.name).collect();
    assert!(names.contains(&"propose"), "propose must be in KG_HANDLERS");
    assert!(names.contains(&"review"), "review must be in KG_HANDLERS");
    assert!(
        names.contains(&"withdraw"),
        "withdraw must be in KG_HANDLERS"
    );
    assert!(names.contains(&"verbs"), "verbs must be in KG_HANDLERS");
    assert!(names.contains(&"stats"), "stats must be in KG_HANDLERS");
}

// ---- Wave 4 regression tests ----

// CC-2 regression: ListParams must accept a `tags` field.
#[test]
fn list_params_accepts_tags() {
    use super::ListParams;
    let p: ListParams = serde_json::from_value(json!({
        "kind": "entity",
        "tags": ["rust", "systems"],
    }))
    .expect("ListParams must accept tags");
    assert_eq!(
        p.tags,
        Some(vec!["rust".to_string(), "systems".to_string()])
    );
}

// CC-2 regression: ListParams with no tags field produces None (not empty vec).
#[test]
fn list_params_no_tags_is_none() {
    use super::ListParams;
    let p: ListParams = serde_json::from_value(json!({"kind": "entity"})).unwrap();
    assert!(
        p.tags.is_none(),
        "absent tags must be None so the entity filter is not applied"
    );
}

// ue-kg-deep C3 regression: UpdateParams must capture entity_kind so the
// handler can return an explicit error instead of silently discarding it.
#[test]
fn update_params_captures_entity_kind() {
    use super::UpdateParams;
    let p: UpdateParams = serde_json::from_value(json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "entity_kind": "dataset",
    }))
    .expect("UpdateParams must deserialize with entity_kind present");
    assert!(
        p.entity_kind.is_some(),
        "entity_kind field must be captured (not silently discarded)"
    );
}

// ue-kg-deep C3 regression: absent entity_kind → None (preserves normal update flow).
#[test]
fn update_params_entity_kind_absent_is_none() {
    use super::UpdateParams;
    let p: UpdateParams = serde_json::from_value(json!({
        "id": "00000000-0000-0000-0000-000000000001",
        "name": "NewName",
    }))
    .unwrap();
    assert!(
        p.entity_kind.is_none(),
        "absent entity_kind must be None so normal updates are not rejected"
    );
}

// ue-kg-deep C4 regression: SearchParams must accept a `min_score` field.
#[test]
fn search_params_accepts_min_score() {
    use super::SearchParams;
    let p: SearchParams = serde_json::from_value(json!({
        "kind": "entity",
        "query": "transformer",
        "min_score": 0.1,
    }))
    .expect("SearchParams must accept min_score");
    assert_eq!(p.min_score, Some(0.1));
}

// #518: SearchParams must accept a `tags` field.
#[test]
fn search_tags_params_accepts_tags() {
    use super::SearchParams;
    let p: SearchParams = serde_json::from_value(json!({
        "kind": "entity",
        "query": "language models",
        "tags": ["rust", "ml"],
    }))
    .expect("SearchParams must accept tags");
    assert_eq!(
        p.tags.as_deref(),
        Some(&["rust".to_string(), "ml".to_string()][..])
    );
}

// #518: absent tags → None (no filter applied).
#[test]
fn search_params_tags_absent_is_none() {
    use super::SearchParams;
    let p: SearchParams = serde_json::from_value(json!({
        "kind": "entity",
        "query": "language models",
    }))
    .unwrap();
    assert!(
        p.tags.is_none(),
        "absent tags must be None; no filter applied by default"
    );
}

// #518: tags_match_any — OR semantics, case-insensitive.
#[test]
fn tags_match_any_or_semantics() {
    use super::tags_match_any;
    let entity = vec!["Rust".to_string(), "systems".to_string()];
    assert!(tags_match_any(&entity, &["rust".to_string()]));
    assert!(tags_match_any(
        &entity,
        &["systems".to_string(), "ml".to_string()]
    ));
    assert!(!tags_match_any(&entity, &["python".to_string()]));
    assert!(tags_match_any(&entity, &[]));
}

// ue-kg-deep C4 regression: absent min_score → None (no floor applied, returns all hits).
#[test]
fn search_params_min_score_absent_is_none() {
    use super::SearchParams;
    let p: SearchParams = serde_json::from_value(json!({
        "kind": "entity",
        "query": "transformer",
    }))
    .unwrap();
    assert!(
        p.min_score.is_none(),
        "absent min_score must be None; no floor applied by default"
    );
}

// ---- Round-6: recursive walk_timestamps unit tests ----

#[test]
fn walk_timestamps_converts_top_level_created_at() {
    use super::walk_timestamps;
    let micros = 1779757074693195i64;
    let mut v = json!({ "created_at": micros, "name": "test" });
    walk_timestamps(&mut v);
    let s = v["created_at"].as_str().expect("must be string");
    assert!(s.len() >= 20 && s.contains('T'), "must be ISO-8601: {s}");
    assert_eq!(v["name"], json!("test"), "name must be unchanged");
}

#[test]
fn walk_timestamps_converts_nested_object_timestamp() {
    use super::walk_timestamps;
    let micros = 1_779_757_074_693_195u64;
    let mut v = json!({
        "payload": {
            "result": { "applied_at": micros }
        }
    });
    walk_timestamps(&mut v);
    let s = v["payload"]["result"]["applied_at"]
        .as_str()
        .expect("payload.result.applied_at must be string");
    assert!(s.len() >= 20 && s.contains('T'), "must be ISO-8601: {s}");
}

#[test]
fn walk_timestamps_converts_array_element_timestamps() {
    use super::walk_timestamps;
    let micros1 = 1_779_757_074_000_000u64;
    let micros2 = 1_779_757_075_000_000u64;
    let mut v = json!({
        "payload": {
            "steps": [
                { "updated_at": micros1 },
                { "updated_at": micros2 }
            ]
        }
    });
    walk_timestamps(&mut v);
    let steps = v["payload"]["steps"].as_array().unwrap();
    for step in steps {
        let s = step["updated_at"]
            .as_str()
            .expect("array element updated_at must be string");
        assert!(s.len() >= 20 && s.contains('T'), "must be ISO-8601: {s}");
    }
}

#[test]
fn walk_timestamps_handles_i64_branch() {
    use super::walk_timestamps;
    // i64 — covers legacy fields and the as_i64() branch of the conversion.
    let micros: i64 = 1_234_567_890_000_000;
    let mut v = json!({ "applied_at": micros });
    walk_timestamps(&mut v);
    let s = v["applied_at"].as_str().expect("must be string");
    assert!(s.contains('T'), "must be ISO-8601: {s}");
}

#[test]
fn walk_timestamps_leaves_strings_unchanged() {
    use super::walk_timestamps;
    let iso = "2026-05-26T00:00:00+00:00";
    let mut v = json!({ "created_at": iso });
    walk_timestamps(&mut v);
    assert_eq!(v["created_at"].as_str().unwrap(), iso);
}

#[test]
fn walk_timestamps_leaves_null_unchanged() {
    use super::walk_timestamps;
    let mut v = json!({ "deleted_at": null, "created_at": 1779757074693195i64 });
    walk_timestamps(&mut v);
    assert_eq!(v["deleted_at"], json!(null));
    assert!(
        v["created_at"].as_str().is_some(),
        "created_at must be converted"
    );
}

#[test]
fn walk_timestamps_non_timestamp_number_untouched() {
    use super::walk_timestamps;
    // A key that is NOT in TIMESTAMP_KEYS — must not be touched.
    let mut v = json!({ "count": 42, "created_at": 1779757074693195i64 });
    walk_timestamps(&mut v);
    assert_eq!(
        v["count"],
        json!(42),
        "non-timestamp number must be unchanged"
    );
    assert!(v["created_at"].as_str().is_some());
}

#[test]
fn walk_timestamps_u64_max_left_unchanged() {
    use super::walk_timestamps;
    // u64::MAX overflows i64 — the value must be left as a number (not
    // converted), because checked try_from rejects it gracefully.
    let mut v = json!({ "created_at": u64::MAX });
    walk_timestamps(&mut v);
    // The value must remain a number — not turned into a string.
    assert!(
        v["created_at"].is_number(),
        "out-of-range u64 timestamp must be left unchanged, got: {:?}",
        v["created_at"]
    );
}

#[test]
fn normalize_entity_timestamps_converts_i64_to_iso() {
    use super::normalize_entity_timestamps;
    // 2026-05-26T00:57:54.693195Z → micros since epoch
    let micros = 1779757074693195i64;
    let v = json!({ "created_at": micros, "updated_at": micros, "name": "test" });
    let out = normalize_entity_timestamps(v);
    let created = out["created_at"]
        .as_str()
        .expect("created_at must be a string");
    let updated = out["updated_at"]
        .as_str()
        .expect("updated_at must be a string");
    // Both must look like ISO-8601 (start with 4-digit year, contain 'T').
    assert!(
        created.len() >= 20 && created.contains('T'),
        "created_at must be ISO-8601, got: {created:?}"
    );
    assert!(
        updated.len() >= 20 && updated.contains('T'),
        "updated_at must be ISO-8601, got: {updated:?}"
    );
    // name must be unchanged.
    assert_eq!(out["name"], json!("test"));
}

#[test]
fn normalize_entity_timestamps_leaves_string_unchanged() {
    use super::normalize_entity_timestamps;
    let iso = "2026-05-26T00:57:54.693195+00:00";
    let v = json!({ "created_at": iso, "updated_at": iso });
    let out = normalize_entity_timestamps(v);
    // Already a string — must not be double-converted.
    assert_eq!(out["created_at"].as_str().unwrap(), iso);
}

#[test]
fn normalize_entity_timestamps_leaves_null_unchanged() {
    use super::normalize_entity_timestamps;
    let v = json!({ "created_at": 1779757074693195i64, "deleted_at": null });
    let out = normalize_entity_timestamps(v);
    assert!(
        out["created_at"].as_str().is_some(),
        "created_at must be converted"
    );
    assert_eq!(out["deleted_at"], json!(null), "null must remain null");
}

#[test]
fn normalize_entity_timestamps_array_converts_each_element() {
    use super::normalize_entity_timestamps_array;
    let micros = 1779757074693195i64;
    let v = json!([
        { "created_at": micros, "name": "a" },
        { "created_at": micros, "name": "b" },
    ]);
    let out = normalize_entity_timestamps_array(v);
    let arr = out.as_array().unwrap();
    for item in arr {
        assert!(
            item["created_at"].as_str().is_some(),
            "each element's created_at must be ISO string"
        );
    }
}

// ---- Issue #486: link endpoint validation should suggest valid relations ----

// Unit test: valid_relations_for_entity_pair returns expected relations for known pairs.
#[test]
fn valid_relations_concept_to_concept_includes_extends() {
    use super::valid_relations_for_entity_pair;
    let rels = valid_relations_for_entity_pair("concept", "concept");
    assert!(
        rels.contains(&"extends"),
        "#486: concept->concept must include extends; got: {rels:?}"
    );
    assert!(
        rels.contains(&"competes_with"),
        "#486: concept->concept must include competes_with; got: {rels:?}"
    );
    assert!(
        rels.contains(&"composed_with"),
        "#486: concept->concept must include composed_with; got: {rels:?}"
    );
    assert!(
        rels.contains(&"instance_of"),
        "#486: concept->concept must include instance_of (wildcard src); got: {rels:?}"
    );
}

// Unit test: unsupported endpoint pair returns empty vec (not a panic).
#[test]
fn valid_relations_unsupported_pair_returns_empty() {
    use super::valid_relations_for_entity_pair;
    let rels = valid_relations_for_entity_pair("person", "dataset");
    assert!(
        rels.is_empty(),
        "#486: person->dataset has no base-contract relations; got: {rels:?}"
    );
}

// Integration test: link with invalid relation returns error containing valid relations.
#[tokio::test]
async fn link_invalid_relation_error_suggests_valid_relations() {
    use crate::KgPack;
    use khive_runtime::KhiveRuntime;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    let src_val = rt
        .create_entity(&token, "concept", None, "ConceptA", None, None, vec![])
        .await
        .expect("create source entity");
    let tgt_val = rt
        .create_entity(&token, "concept", None, "ConceptB", None, None, vec![])
        .await
        .expect("create target entity");

    let pack = KgPack::new(rt.clone());

    // "depends_on" is a valid relation string but NOT in the concept->concept allowlist.
    let params = json!({
        "source_id": src_val.id.to_string(),
        "target_id": tgt_val.id.to_string(),
        "relation": "depends_on",
    });
    let result = pack.handle_link(&token, params).await;
    assert!(
        result.is_err(),
        "#486: depends_on on concept->concept should fail"
    );
    let err_msg = format!("{}", result.unwrap_err());
    // The enriched error must mention valid relations.
    assert!(
        err_msg.contains("Valid relations:"),
        "#486: error must contain 'Valid relations:'; got: {err_msg}"
    );
    // concept->concept includes extends; verify it appears in the suggestion.
    assert!(
        err_msg.contains("extends"),
        "#486: valid relations for concept->concept must include 'extends'; got: {err_msg}"
    );
    // The error should name the endpoint kinds.
    assert!(
        err_msg.contains("concept"),
        "#486: error must mention endpoint kinds; got: {err_msg}"
    );
}

// ── #567 regression: ensure_note_kind must not disclose foreign note metadata ──

#[tokio::test]
async fn ensure_note_kind_rejects_foreign_note_before_kind_check() {
    use super::ensure_note_kind;
    use khive_runtime::{KhiveRuntime, RuntimeError};
    use khive_types::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ns_a = rt.authorize(Namespace::parse("ns-a").unwrap()).unwrap();
    let ns_b = rt.authorize(Namespace::parse("ns-b").unwrap()).unwrap();
    let foreign = rt
        .create_note(&ns_b, "observation", None, "foreign", None, None, vec![])
        .await
        .unwrap();

    // All expected_kind values must yield opaque NotFound — no kind leakage.
    for expected in [Some("observation"), Some("task"), None] {
        let err = ensure_note_kind(&rt, &ns_a, foreign.id, expected)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::NotFound(_)),
            "foreign note preflight must be NotFound, got {err:?}"
        );
        assert!(
            err.to_string().contains("not found in this namespace"),
            "error message must be opaque, got {err}"
        );
    }
}

// ---- Weight validation tests (KG-AUD-002) ----

#[test]
fn validate_weight_rejects_negative() {
    use super::validate_weight;
    let err = validate_weight(Some(-0.1)).unwrap_err();
    assert!(
        err.to_string().contains("[0.0, 1.0]"),
        "error must mention valid range; got: {err}"
    );
}

#[test]
fn validate_weight_rejects_above_one() {
    use super::validate_weight;
    let err = validate_weight(Some(1.1)).unwrap_err();
    assert!(
        err.to_string().contains("[0.0, 1.0]"),
        "error must mention valid range; got: {err}"
    );
}

#[test]
fn validate_weight_rejects_nan() {
    use super::validate_weight;
    let err = validate_weight(Some(f64::NAN)).unwrap_err();
    assert!(
        err.to_string().contains("finite"),
        "error must mention finite; got: {err}"
    );
}

#[test]
fn validate_weight_rejects_infinity() {
    use super::validate_weight;
    let err = validate_weight(Some(f64::INFINITY)).unwrap_err();
    assert!(
        err.to_string().contains("finite"),
        "error must mention finite; got: {err}"
    );
}

#[test]
fn validate_weight_accepts_valid_range() {
    use super::validate_weight;
    assert_eq!(validate_weight(None).unwrap(), 1.0);
    assert_eq!(validate_weight(Some(0.0)).unwrap(), 0.0);
    assert_eq!(validate_weight(Some(0.5)).unwrap(), 0.5);
    assert_eq!(validate_weight(Some(1.0)).unwrap(), 1.0);
}
