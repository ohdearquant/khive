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

// Wire-level tri-state nullable f64 for `update`: absent→None (preserve), null→Some(None)
// (clear), number→Some(Some(v)) (set). Regression: `Option<Value>` used to collapse
// absent and null into the same None, so JSON null couldn't distinguish clear/preserve.
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
            "id": "00000000-0000-0000-0000-000000000001",
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
        "id": "00000000-0000-0000-0000-000000000001",
        "decision": "approve",
    }))
    .expect("ReviewParams must deserialize without actor");
    assert_eq!(p.id, "00000000-0000-0000-0000-000000000001");
    assert_eq!(p.decision, "approve");
}

// CRIT-2 regression: WithdrawParams must not accept an `actor` field.
#[test]
fn withdraw_params_no_actor_field() {
    use super::WithdrawParams;
    let p: WithdrawParams = serde_json::from_value(json!({
        "id": "00000000-0000-0000-0000-000000000002",
    }))
    .expect("WithdrawParams must deserialize without actor");
    assert_eq!(p.id, "00000000-0000-0000-0000-000000000002");
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

// KG pack must expose exactly 18 handlers including propose/review/withdraw/verbs/stats/context/resolve
#[test]
fn kg_pack_exposes_18_handlers() {
    use crate::KgPack;
    use khive_types::Pack;
    let handlers = KgPack::HANDLERS;
    assert_eq!(
        handlers.len(),
        18,
        "kg pack must expose 18 handlers (was 17, +1 for resolve — unified-verb draft ADR Slice 1)"
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
    assert!(
        names.contains(&"context"),
        "context must be in KG_HANDLERS (ADR-089)"
    );
    assert!(
        names.contains(&"resolve"),
        "resolve must be in KG_HANDLERS (unified-verb draft ADR Slice 1)"
    );
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

// ---- Recursive walk_timestamps unit tests ----

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
    use khive_runtime::KhiveRuntime;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let rels = valid_relations_for_entity_pair(&rt, "concept", None, "concept", None);
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
    use khive_runtime::KhiveRuntime;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let rels = valid_relations_for_entity_pair(&rt, "person", None, "dataset", None);
    assert!(
        rels.is_empty(),
        "#486: person->dataset has no base-contract relations; got: {rels:?}"
    );
}

// Issue #543/#621: generative hint/validator divergence sweep. See docs/api/entity-kind-validation.md#test-coverage-hintvalidator-divergence-sweep-issue-543.
#[tokio::test]
async fn valid_relations_hint_matches_real_validator_acceptance_across_all_entity_kind_pairs() {
    use crate::vocab::EntityKind as KgEntityKind;
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use khive_storage::EdgeRelation;

    async fn accepted_relations_for(
        rt: &KhiveRuntime,
        src_kind: &str,
        tgt_kind: &str,
    ) -> Vec<&'static str> {
        let token = rt.authorize(Namespace::local()).unwrap();
        let mut accepted = Vec::new();
        for relation in EdgeRelation::ALL {
            // annotates is entity-invalid; see docs/api/entity-kind-validation.md.
            if relation == EdgeRelation::Annotates {
                continue;
            }
            let src = rt
                .create_entity(&token, src_kind, None, "src", None, None, vec![])
                .await
                .expect("create source entity");
            let tgt = rt
                .create_entity(&token, tgt_kind, None, "tgt", None, None, vec![])
                .await
                .expect("create target entity");
            let result = rt.link(&token, src.id, tgt.id, relation, 1.0, None).await;
            if result.is_ok() {
                accepted.push(relation.as_str());
            }
        }
        accepted.sort_unstable();
        accepted.dedup();
        accepted
    }

    // kg pack alone: base allowlist + KG_EDGE_RULES (person->org, person->project,
    // org->org additions from issue #60).
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());

    for src in KgEntityKind::ALL {
        for tgt in KgEntityKind::ALL {
            let (src_kind, tgt_kind) = (src.name(), tgt.name());
            let expected = accepted_relations_for(&rt, src_kind, tgt_kind).await;
            let hinted =
                super::valid_relations_for_entity_pair(&rt, src_kind, None, tgt_kind, None);
            assert_eq!(
                hinted, expected,
                "#543: hint set for {src_kind}->{tgt_kind} must equal the real \
                 validator's acceptance set; hinted={hinted:?} expected={expected:?}"
            );
        }
    }
}

// #621: `valid_relations_for_entity_pair` only
// matched `EndpointKind::EntityOfKind` pack rules, silently omitting
// `EndpointKind::EntityOfType` rules such as khive-pack-formal's typed
// `concept/theorem -> concept/definition` `depends_on` rule
// (`crates/khive-pack-formal/src/vocab.rs:29-38`) — exactly the #543
// divergence class this PR fixes. This test proves the fix: with `kg,formal`
// installed, the hint for the typed pair includes `depends_on`, and a
// generative cross-check against the real validator confirms the hint set is
// exactly the validator's acceptance set for that typed pair.
#[tokio::test]
async fn valid_relations_hint_covers_formal_pack_entity_of_type_rules() {
    use crate::KgPack;
    use khive_pack_formal::FormalPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
    use khive_storage::EdgeRelation;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(FormalPack::new(rt.clone()));
    let registry = builder.build().expect("kg+formal registry builds");
    rt.install_edge_rules(registry.all_edge_rules());

    let hinted = super::valid_relations_for_entity_pair(
        &rt,
        "concept",
        Some("theorem"),
        "concept",
        Some("definition"),
    );
    assert!(
        hinted.contains(&"depends_on"),
        "#621: hint for concept/theorem->concept/definition must \
         include depends_on from khive-pack-formal's EntityOfType rule; \
         got: {hinted:?}"
    );

    // Generative cross-check on the same typed pair: create real typed
    // entities, call the real validator across all relations, and assert the
    // hint equals the actual acceptance set — not just a presence check.
    let token = rt.authorize(Namespace::local()).unwrap();
    let mut expected = Vec::new();
    for relation in EdgeRelation::ALL {
        if relation == EdgeRelation::Annotates {
            continue;
        }
        let src = rt
            .create_entity(
                &token,
                "concept",
                Some("theorem"),
                "src",
                None,
                None,
                vec![],
            )
            .await
            .expect("create typed source entity");
        let tgt = rt
            .create_entity(
                &token,
                "concept",
                Some("definition"),
                "tgt",
                None,
                None,
                vec![],
            )
            .await
            .expect("create typed target entity");
        if rt
            .link(&token, src.id, tgt.id, relation, 1.0, None)
            .await
            .is_ok()
        {
            expected.push(relation.as_str());
        }
    }
    expected.sort_unstable();
    expected.dedup();

    assert_eq!(
        hinted, expected,
        "#621: hint set for typed concept/theorem->concept/definition \
         must equal the real validator's acceptance set; hinted={hinted:?} \
         expected={expected:?}"
    );
}

// GTD's task->task depends_on rule is NoteOfKind-scoped, not an entity-pair
// rule, so it structurally cannot appear in (and is correctly absent from)
// this entity-pair hint path — a task/task mismatch produces a different
// validation error ("must be an entity for relation ...") that never reaches
// `enrich_allowlist_error` in the first place. This pure-function check
// complements `gtd_task_mismatch_bypasses_enriched_hint_on_real_link_path`
// below, which proves the same boundary on the real `handle_link` path.
#[tokio::test]
async fn valid_relations_hint_does_not_cover_gtd_note_scoped_rules() {
    use crate::KgPack;
    use khive_pack_gtd::GtdPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    let registry = builder.build().expect("kg+gtd registry builds");
    rt.install_edge_rules(registry.all_edge_rules());

    // No entity kind is named "task" (it's a note kind) so no (src_kind,
    // tgt_kind) entity pair can ever surface GTD's depends_on rule here.
    let hinted = super::valid_relations_for_entity_pair(&rt, "project", None, "project", None);
    assert!(
        !hinted.is_empty(),
        "sanity: project->project must still have base-rule hints"
    );
    // The composed pack_edge_rules() DOES include GTD's rule at runtime, but
    // it is NoteOfKind("task") on both ends, so it can never satisfy the
    // entity-substrate match this hint function requires — confirmed by
    // checking no entity-kind pair yields "depends_on" from a NoteOfKind
    // source.
    assert!(
        rt.pack_edge_rules().iter().any(|r| matches!(
            (r.relation, r.source, r.target),
            (
                khive_storage::EdgeRelation::DependsOn,
                khive_types::EndpointKind::NoteOfKind("task"),
                khive_types::EndpointKind::NoteOfKind("task"),
            )
        )),
        "GTD's task->task depends_on rule must be present in the composed \
         runtime rule set (proves it's reachable, not just absent by omission)"
    );
}

// #621: proves the GTD boundary on the REAL
// `KgPack::handle_link` path with real task notes, not just rule presence in
// `pack_edge_rules()`. A task->task relation outside GTD's declared
// `depends_on` must fail with the substrate-mismatch error ("must be an
// entity for relation ...") and must NOT carry the enriched
// "Valid relations:" hint -- proving `enrich_allowlist_error` is never
// reached for a note/note mismatch. The GTD-declared relation
// (`depends_on`) between the same two notes must still succeed.
#[tokio::test]
async fn gtd_task_mismatch_bypasses_enriched_hint_on_real_link_path() {
    use crate::KgPack;
    use khive_pack_gtd::GtdPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    let registry = builder.build().expect("kg+gtd registry builds");
    rt.install_edge_rules(registry.all_edge_rules());

    let token = rt.authorize(Namespace::local()).unwrap();
    let src = rt
        .create_note(&token, "task", None, "task A", None, None, vec![])
        .await
        .expect("create task note A");
    let tgt = rt
        .create_note(&token, "task", None, "task B", None, None, vec![])
        .await
        .expect("create task note B");

    let pack = KgPack::new(rt.clone());

    // "extends" is a valid relation string but not GTD's task->task rule
    // (only depends_on is declared NoteOfKind for task->task).
    let params = serde_json::json!({
        "source_id": src.id.to_string(),
        "target_id": tgt.id.to_string(),
        "relation": "extends",
    });
    let result = pack.handle_link(&token, params).await;
    assert!(result.is_err(), "task->task extends must be rejected");
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("must be an entity"),
        "expected the substrate-mismatch error (only annotates crosses \
         substrates), got: {err_msg}"
    );
    assert!(
        !err_msg.contains("Valid relations:"),
        "a note/note mismatch must NOT be enriched with the entity-pair hint \
         -- enrich_allowlist_error is only reachable for entity/entity \
         mismatches; got: {err_msg}"
    );

    // Sanity: the actual GTD-declared relation succeeds on the same two notes
    // via the real link path.
    let params_ok = serde_json::json!({
        "source_id": src.id.to_string(),
        "target_id": tgt.id.to_string(),
        "relation": "depends_on",
    });
    assert!(
        pack.handle_link(&token, params_ok).await.is_ok(),
        "task->task depends_on must be accepted by the real link path"
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

// Regression: list(kind=note, thread_id=<filter>) must not panic when a stored
// thread_id contains a multi-byte UTF-8 character whose second byte falls at
// byte index 8. The old code used `stored[..8]` (a byte-index slice) which
// panics when byte 8 is not a char boundary. The fix uses `str::get(..8)`
// which returns None for invalid boundaries, converting the panic into a safe
// no-match result while leaving ASCII (UUID) thread_ids unaffected.
#[tokio::test]
async fn list_note_thread_filter_non_ascii_stored_no_panic() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).unwrap();

    // "1234567α": 7 ASCII bytes then α (U+03B1, 2 bytes) = 9 total bytes.
    // Byte index 8 is the second byte of α, not a char boundary.
    // The old code reached stored[..8] here and panicked.
    let non_ascii_thread = "1234567\u{03B1}";

    rt.create_note(
        &token,
        "observation",
        None,
        "note with non-ASCII thread_id",
        None,
        Some(serde_json::json!({"thread_id": non_ascii_thread})),
        vec![],
    )
    .await
    .expect("create note with non-ASCII thread_id");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    // Filter with a different 8-byte ASCII string so exact equality fails and
    // the prefix branch is reached.  On the old code, stored[..8] on
    // "1234567α" panics; with the fix it safely returns no match.
    let result = pack
        .handle_list(
            &token,
            serde_json::json!({"kind": "note", "thread_id": "12345678"}),
            &registry,
        )
        .await;

    assert!(
        result.is_ok(),
        "list with non-ASCII stored thread_id must not panic; got: {:?}",
        result.err()
    );
    let arr = result.unwrap();
    assert!(
        arr.as_array()
            .expect("list result must be a JSON array")
            .is_empty(),
        "no note should match a non-overlapping thread_id prefix"
    );
}

// Complement: exact-match on a non-ASCII thread_id must still return the note
// (the exact-equality branch fires before any byte slicing).
#[tokio::test]
async fn list_note_thread_filter_non_ascii_exact_match() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).unwrap();

    let non_ascii_thread = "1234567\u{03B1}";

    rt.create_note(
        &token,
        "observation",
        None,
        "note with non-ASCII thread_id exact",
        None,
        Some(serde_json::json!({"thread_id": non_ascii_thread})),
        vec![],
    )
    .await
    .expect("create note");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    let result = pack
        .handle_list(
            &token,
            serde_json::json!({"kind": "note", "thread_id": non_ascii_thread}),
            &registry,
        )
        .await
        .expect("exact-match list must succeed");

    let arr = result.as_array().expect("list result must be a JSON array");
    assert_eq!(
        arr.len(),
        1,
        "exact-match on non-ASCII thread_id must return the note"
    );
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

// Regression: update(id=<note>, description=...) must return an explicit error,
// not silently drop the field and return ok:true with stale content.
// The silent-drop was the original bug: description is an entity-only field;
// passing it on a note caused updated_at to bump while content stayed unchanged.
#[tokio::test]
async fn update_note_with_entity_field_description_returns_error() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    // Create a note with known content.
    let note = rt
        .create_note(
            &token,
            "observation",
            None,
            "original content",
            None,
            None,
            vec![],
        )
        .await
        .expect("create note");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    // Attempt to update the note using the entity-only field `description`.
    let result = pack
        .handle_update(
            &token,
            json!({ "id": note.id.to_string(), "description": "should be rejected" }),
            &registry,
        )
        .await;

    assert!(
        result.is_err(),
        "update note with entity-only field 'description' must return an error, got ok"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("description"),
        "error must name the invalid field 'description'; got: {err_msg}"
    );
    assert!(
        err_msg.contains("content"),
        "error must list 'content' as a valid note field; got: {err_msg}"
    );

    // Confirm note content is unchanged after the rejected update.
    let unchanged = rt
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .expect("note must still exist");
    assert_eq!(
        unchanged.content, "original content",
        "note content must be unchanged after rejected update"
    );
}

// Regression (symmetric): update(id=<entity>, content=...) must return an
// explicit error — `content` is a note-only field.
#[tokio::test]
async fn update_entity_with_note_field_content_returns_error() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    let entity = rt
        .create_entity(&token, "concept", None, "MyEntity", None, None, vec![])
        .await
        .expect("create entity");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    let result = pack
        .handle_update(
            &token,
            json!({ "id": entity.id.to_string(), "content": "should be rejected" }),
            &registry,
        )
        .await;

    assert!(
        result.is_err(),
        "update entity with note-only field 'content' must return an error, got ok"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("content"),
        "error must name the invalid field 'content'; got: {err_msg}"
    );
    assert!(
        err_msg.contains("description"),
        "error must list 'description' as a valid entity field; got: {err_msg}"
    );

    // Confirm entity name is unchanged after the rejected update.
    let unchanged = rt
        .get_entity(&token, entity.id)
        .await
        .expect("get_entity must not fail");
    assert_eq!(
        unchanged.name, "MyEntity",
        "entity name must be unchanged after rejected update"
    );
}

// Regression (edge branch): update(id=<edge>, description=...) must return an
// explicit error — edges only accept relation, weight, properties.
#[tokio::test]
async fn update_edge_with_non_edge_field_returns_error() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
    use khive_types::EdgeRelation;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    let src = rt
        .create_entity(&token, "concept", None, "SrcConcept", None, None, vec![])
        .await
        .expect("create source entity");
    let tgt = rt
        .create_entity(&token, "concept", None, "TgtConcept", None, None, vec![])
        .await
        .expect("create target entity");
    let edge = rt
        .link(&token, src.id, tgt.id, EdgeRelation::Extends, 0.8, None)
        .await
        .expect("create edge");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    // Attempt to update the edge using the non-edge field `description`.
    let result = pack
        .handle_update(
            &token,
            json!({ "id": edge.id.to_string(), "description": "should be rejected" }),
            &registry,
        )
        .await;

    assert!(
        result.is_err(),
        "update edge with non-edge field 'description' must return an error, got ok"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("description"),
        "error must name the invalid field 'description'; got: {err_msg}"
    );
    assert!(
        err_msg.contains("relation"),
        "error must list 'relation' as a valid edge field; got: {err_msg}"
    );

    // Confirm the edge is unchanged after the rejected update.
    let unchanged = rt
        .get_edge(&token, edge.id.into())
        .await
        .expect("get_edge must not fail")
        .expect("edge must still exist");
    assert_eq!(
        unchanged.weight, 0.8,
        "edge weight must be unchanged after rejected update"
    );
}

// HIGH regression: update(note_id, tags=[...]) must return an explicit error.
// Notes have no top-level tags column; tags live in properties["tags"].
// Before the fix, tags was silently dropped on the note path (the error string
// even advertised it as valid — making the bug worse for callers following docs).
#[tokio::test]
async fn update_note_with_entity_field_tags_returns_error() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    let note = rt
        .create_note(
            &token,
            "observation",
            None,
            "note content unchanged",
            None,
            None,
            vec![],
        )
        .await
        .expect("create note");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    let result = pack
        .handle_update(
            &token,
            json!({ "id": note.id.to_string(), "tags": ["rust", "ml"] }),
            &registry,
        )
        .await;

    assert!(
        result.is_err(),
        "update note with entity-only field 'tags' must return an error, got ok"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("tags"),
        "error must name the invalid field 'tags'; got: {err_msg}"
    );
    assert!(
        err_msg.contains("content"),
        "error must list 'content' as a valid note field; got: {err_msg}"
    );

    // Confirm note content is unchanged.
    let unchanged = rt
        .notes(&token)
        .unwrap()
        .get_note(note.id)
        .await
        .unwrap()
        .expect("note must still exist");
    assert_eq!(
        unchanged.content, "note content unchanged",
        "note content must be unchanged after rejected update"
    );
}

// MEDIUM regression: update(entity_id, salience=...) must return an explicit
// error — salience is a note-only field and was silently dropped on entities.
#[tokio::test]
async fn update_entity_with_note_field_salience_returns_error() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();

    let entity = rt
        .create_entity(
            &token,
            "concept",
            None,
            "SalienceEntity",
            None,
            None,
            vec![],
        )
        .await
        .expect("create entity");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");

    let result = pack
        .handle_update(
            &token,
            json!({ "id": entity.id.to_string(), "salience": 0.9 }),
            &registry,
        )
        .await;

    assert!(
        result.is_err(),
        "update entity with note-only field 'salience' must return an error, got ok"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("salience"),
        "error must name the invalid field 'salience'; got: {err_msg}"
    );
    assert!(
        err_msg.contains("description"),
        "error must list 'description' as a valid entity field; got: {err_msg}"
    );

    // Confirm entity name is unchanged.
    let unchanged = rt
        .get_entity(&token, entity.id)
        .await
        .expect("get_entity must not fail");
    assert_eq!(
        unchanged.name, "SalienceEntity",
        "entity name must be unchanged after rejected update"
    );
}

// ── #764: create's embedding_content wiring ─────────────────────────────────

/// A note create with a proper-prefix `embedding_content` must succeed and
/// store the full `content`; the override is a runtime-layer concern
/// (covered by `khive-runtime`'s own unit tests) — this proves the handler
/// forwards the field rather than dropping or misrouting it.
#[tokio::test]
async fn create_note_forwards_embedding_content_and_stores_full_content() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");
    let _token = rt.authorize(Namespace::local()).expect("authorize local");

    let full = "head sentinel plus a long tail sentinel beyond any cap";
    let head = &full[.."head sentinel".len()];

    let resp = registry
        .dispatch(
            "create",
            json!({
                "kind": "observation",
                "content": full,
                "embedding_content": head,
            }),
        )
        .await
        .expect("proper-prefix embedding_content must be accepted");
    assert_eq!(
        resp["content"], full,
        "stored content must be the full text"
    );
}

/// `embedding_content` is only meaningful for a singleton `kind=note` create;
/// supplying it alongside an entity kind must be rejected before any write.
#[tokio::test]
async fn create_entity_rejects_embedding_content() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");
    let _token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "project",
                "name": "should-not-be-created",
                "embedding_content": "irrelevant",
            }),
        )
        .await
        .expect_err("embedding_content on an entity create must be rejected");
    assert!(
        format!("{err}").contains("embedding_content"),
        "error must name the offending field: {err}"
    );

    let list = registry
        .dispatch("list", json!({"kind": "project", "limit": 10}))
        .await
        .expect("list ok");
    assert_eq!(
        list.as_array().expect("array").len(),
        0,
        "rejected create must leave no entity behind"
    );
}

/// `embedding_content` is not supported for bulk `items` create — supplying
/// it at the top level alongside `items` must be rejected before any write.
#[tokio::test]
async fn create_bulk_items_rejects_top_level_embedding_content() {
    use crate::KgPack;
    use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry build");
    let _token = rt.authorize(Namespace::local()).expect("authorize local");

    let err = registry
        .dispatch(
            "create",
            json!({
                "items": [{"kind": "concept", "name": "should-not-be-created"}],
                "embedding_content": "irrelevant",
            }),
        )
        .await
        .expect_err("embedding_content alongside bulk items must be rejected");
    assert!(
        format!("{err}").contains("embedding_content"),
        "error must name the offending field: {err}"
    );
}

// Shared by the `decision precedes decision` tests below: builds the KG
// registry and installs its composed edge rules (the same rule set the
// production MCP server runs under), so negative cases prove the
// `decision`->`decision` exception stays kind-specific rather than merely
// exercising the base entity-only rule against an unconfigured runtime.
async fn configured_kg_pack() -> (
    khive_runtime::KhiveRuntime,
    khive_runtime::NamespaceToken,
    crate::KgPack,
) {
    use crate::KgPack;
    use khive_runtime::VerbRegistryBuilder;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let pack = KgPack::new(rt.clone());
    (rt, token, pack)
}

// ADR-087 Amendment 1 §A9: review-round chains are `decision precedes
// decision` note-to-note edges (round N-1 precedes round N). KG_EDGE_RULES
// additively extends the base entity-only `precedes` contract to
// `decision`->`decision` note pairs, mirroring the GTD pack's `task`->`task`
// `depends_on` rule.
#[tokio::test]
async fn link_accepts_decision_precedes_decision() {
    let (rt, token, pack) = configured_kg_pack().await;

    let round1 = rt
        .create_note(
            &token,
            "decision",
            None,
            "round 1 decision",
            None,
            None,
            vec![],
        )
        .await
        .expect("create round-1 decision note");
    let round2 = rt
        .create_note(
            &token,
            "decision",
            None,
            "round 2 decision",
            None,
            None,
            vec![],
        )
        .await
        .expect("create round-2 decision note");

    let params = serde_json::json!({
        "source_id": round1.id.to_string(),
        "target_id": round2.id.to_string(),
        "relation": "precedes",
    });
    assert!(
        pack.handle_link(&token, params).await.is_ok(),
        "decision->decision precedes must be accepted"
    );
}

#[tokio::test]
async fn link_rejects_decision_precedes_observation() {
    let (rt, token, pack) = configured_kg_pack().await;

    let decision = rt
        .create_note(&token, "decision", None, "a decision", None, None, vec![])
        .await
        .expect("create decision note");
    let observation = rt
        .create_note(
            &token,
            "observation",
            None,
            "an observation",
            None,
            None,
            vec![],
        )
        .await
        .expect("create observation note");

    let params = serde_json::json!({
        "source_id": decision.id.to_string(),
        "target_id": observation.id.to_string(),
        "relation": "precedes",
    });
    assert!(
        pack.handle_link(&token, params).await.is_err(),
        "decision->observation precedes must be rejected"
    );
}

#[tokio::test]
async fn link_rejects_observation_precedes_decision() {
    let (rt, token, pack) = configured_kg_pack().await;

    let observation = rt
        .create_note(
            &token,
            "observation",
            None,
            "an observation",
            None,
            None,
            vec![],
        )
        .await
        .expect("create observation note");
    let decision = rt
        .create_note(&token, "decision", None, "a decision", None, None, vec![])
        .await
        .expect("create decision note");

    let params = serde_json::json!({
        "source_id": observation.id.to_string(),
        "target_id": decision.id.to_string(),
        "relation": "precedes",
    });
    assert!(
        pack.handle_link(&token, params).await.is_err(),
        "observation->decision precedes must be rejected"
    );
}

#[tokio::test]
async fn link_entity_precedes_entity_unaffected_by_decision_note_rule() {
    let (rt, token, pack) = configured_kg_pack().await;

    let src = rt
        .create_entity(&token, "project", None, "step 1", None, None, vec![])
        .await
        .expect("create source entity");
    let tgt = rt
        .create_entity(&token, "project", None, "step 2", None, None, vec![])
        .await
        .expect("create target entity");

    let params = serde_json::json!({
        "source_id": src.id.to_string(),
        "target_id": tgt.id.to_string(),
        "relation": "precedes",
    });
    assert!(
        pack.handle_link(&token, params).await.is_ok(),
        "entity->entity precedes must remain accepted under the composed rule set (base ADR-002 contract)"
    );
}
